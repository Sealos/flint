//! Rechecking that a session's context still points at the API server the session opened on.
//!
//! Log sessions and exec sessions keep their client for as long as they run, so a context that
//! was re-pointed at another server (`aws eks update-kubeconfig --alias main` run for another
//! cluster, say) or removed would go unnoticed: the session would keep talking to the old
//! server while the context's name now means another cluster. Each such session checks its
//! context every [`RECHECK_EVERY`] with a [`Recheck`]: the cluster manager answers from its
//! [`ContextHistory`], never the server, and the session ends with the [`reason`] it gives.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::time::{Instant, Sleep};

use crate::cluster::context_fingerprint;
use crate::kubeconfig::{BlankFile, FileRead, KubeconfigRead, LoadedKubeconfig};
use crate::settings::server_identity;

/// How often a running session checks its context still points at the server it opened on.
pub(crate) const RECHECK_EVERY: Duration = Duration::from_secs(30);

/// A check that runs every `every`. It keeps its timer and the check in flight between polls,
/// so it can sit in a hand-written `poll` or in a `select!` loop that drops the future it
/// polled.
pub(crate) struct Recheck<T> {
    every: Duration,
    next: Pin<Box<Sleep>>,
    checking: Option<BoxFuture<'static, T>>,
}

impl<T> Recheck<T> {
    /// The first check is due `every` from now.
    pub fn new(every: Duration) -> Self {
        Recheck {
            every,
            next: Box::pin(tokio::time::sleep(every)),
            checking: None,
        }
    }

    /// Once a check is due, starts it with `start` and polls it. Ready with its result. The
    /// next check is due `every` after this one started.
    pub fn poll_check(
        &mut self,
        cx: &mut Context<'_>,
        start: impl FnOnce() -> BoxFuture<'static, T>,
    ) -> Poll<T> {
        if self.checking.is_none() {
            std::task::ready!(self.next.as_mut().poll(cx));
            self.next.as_mut().reset(Instant::now() + self.every);
            self.checking = Some(start());
        }
        let checking = self.checking.as_mut().expect("set above");
        let result = std::task::ready!(checking.poll_unpin(cx));
        self.checking = None;
        Poll::Ready(result)
    }
}

/// What a pinned session is, for what its reason to end tells to do instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pinned {
    Logs,
    Shell,
}

/// What a session is pinned to, read in the same step as its client: its context's
/// generation in [`ContextHistory`], and the API server the client was built for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextPin {
    /// `0` when the client was not built from the context's confirmed definition: the pin is
    /// stale from the start.
    generation: u64,
    /// The same for the endpoint, which ignores a trailing slash: what a log session is pinned
    /// to. A shell keeps the exact [`Self::generation`], since the write gate checks it.
    endpoint: u64,
    /// [`Record::gate`] when pinned: what the write gate holds a launch to.
    gate: u64,
    /// [`Record::definition`] when pinned. A write compares it with the one it was reviewed on.
    definition: Option<u64>,
    server: Option<String>,
}

impl ContextPin {
    /// The API server the session's client was built for.
    pub fn server(&self) -> Option<&str> {
        self.server.as_deref()
    }

    /// Whether `other` is this pin on another connection definition, and nothing else moved.
    pub fn only_definition_differs(&self, other: &ContextPin) -> bool {
        self.definition != other.definition
            && self.generation == other.generation
            && self.endpoint == other.endpoint
            && self.gate == other.gate
            && self.server == other.server
    }
}

/// A context's definition, as kubeconfig loads last saw it.
#[derive(Debug, Clone)]
struct Record {
    server: Option<String>,
    /// The file that defined it.
    source: PathBuf,
    present: bool,
    /// A load confirmed it. Unconfirmed: only seen while a file before its own failed to load
    /// (at startup, say), so it may be a fallback; a load that confirms it on the same server
    /// keeps its generation.
    confirmed: bool,
    /// Bumped whenever a confirmed change gives the context another server, or removes it, so
    /// a change that a later reload undoes is still seen by sessions pinned before it.
    generation: u64,
    /// Bumped likewise, but not by a change that only adds or drops a trailing slash.
    endpoint: u64,
    /// Bumped when another file's cluster entry shows another server while the entry that won
    /// can't be read. A running session survives that, but the write gate does not let
    /// anything new start on a pin from before it, even once the file is back.
    gate: u64,
    /// The hash of the context, cluster and user entries the last load showed
    /// ([`context_fingerprint`]), confirmed or not.
    definition: Option<u64>,
    /// A file that failed in the last load could define the context, its cluster or its user.
    provisional: bool,
}

/// Whether two servers are one endpoint: the same URL but for a trailing slash.
fn same_endpoint(a: Option<&str>, b: Option<&str>) -> bool {
    a.map(server_identity) == b.map(server_identity)
}

/// What the core knows of the server a context shows, for the front end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shown {
    /// The context's confirmed definition: it changes when a confirmed change moves the context,
    /// even one a later reload undoes, and not on an edit that keeps its server.
    pub definition: u64,
    /// [`Self::definition`] for the endpoint: an edit that only adds or drops a trailing slash
    /// keeps it, as it keeps a log session running.
    pub endpoint: u64,
    /// A load confirmed the server shown. Otherwise it may be a fallback, showing only while a
    /// kubeconfig file before it can't be read.
    pub confirmed: bool,
}

/// Every context's history across kubeconfig loads, for the sessions pinned to them (UP-06).
///
/// The loader reads the kubeconfig files in order, and the first file that defines a name
/// wins. A file it cannot read or parse (caught half-written, say) is skipped, and what it
/// defines is unknown for that load. So a load only confirms what no failed file could change:
///
/// - **Removal.** A context missing from a load is removed only if no failed file ranks at or
///   above the file that defined it. If its own file, or one before it, failed, the context
///   may still be there.
/// - **Another server.** A context that a load gives another server (or brings back) is a
///   confirmed change only if no failed file ranks above the file it now comes from. Otherwise
///   the entry may be a fallback, a later file's definition showing only while the first one
///   is unreadable: it is provisional, and the confirmed definition stays. A session opened on
///   a provisional definition is pinned stale, and its first check ends it
///   ([`Change::Unconfirmed`]): it never moves to the confirmed server. The server comes from
///   the context's cluster entry, which another file can define: the entry is a fallback too
///   if a failed file ranks above the file that won the cluster, or if no file defines the
///   cluster while any file failed.
///
/// A failed file ranked below a context's file cannot change which definition wins, so it
/// masks nothing by rank. It masks a removal only if it defined the context too (a shadowed
/// definition) when it last loaded: removing the winning one then leaves its own, unknown for
/// now. A failure in a file never known to define the context masks nothing.
///
/// What a load shows but cannot confirm never replaces confirmed history. A context first seen
/// that way (a file before it already unreadable at startup) is recorded as unconfirmed, with
/// its file, so its absence is masked the same way; sessions opened on it run, and a load that
/// confirms it on the same server leaves them running.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextHistory {
    records: HashMap<String, Record>,
    /// The context names each file defined when it last loaded, shadowed ones included.
    defines: HashMap<PathBuf, HashSet<String>>,
    /// The files that won a cluster or user entry when they last loaded: a file can give a
    /// context its server or credentials without defining any context.
    supplies: HashSet<PathBuf>,
    /// The last generation handed out; generations are unique across contexts.
    last: u64,
    /// The last load had a file it could not read, so a server shown may be a fallback.
    failing: bool,
}

impl ContextHistory {
    /// Records a load: its contexts (each from the file that won it), its files in order, and
    /// the files that failed.
    pub fn record(&mut self, loaded: &LoadedKubeconfig) {
        let rank = |path: &Path| loaded.paths.iter().position(|p| p == path);
        let first_failed = loaded.errors.iter().filter_map(|(p, _)| rank(p)).min();
        self.failing = !loaded.errors.is_empty();
        // A failed file ranked above `path` (or at it, with `or_at`) could define what `path`
        // does. A file no longer in the list ranks nowhere.
        let masked = |path: &Path, or_at: bool| match (first_failed, rank(path)) {
            (Some(failed), Some(at)) => failed < at || (or_at && failed == at),
            _ => false,
        };
        // What each file defines, as of its last load. A file that no longer exists defines
        // nothing; a failed one keeps what it defined last.
        let failed: Vec<&Path> = loaded.errors.iter().map(|(p, _)| p.as_path()).collect();
        self.defines
            .retain(|path, _| failed.contains(&path.as_path()));
        for (path, names) in &loaded.defines {
            self.defines
                .insert(path.clone(), names.iter().cloned().collect());
        }
        self.supplies
            .retain(|path| failed.contains(&path.as_path()));
        self.supplies.extend(
            loaded
                .cluster_sources
                .values()
                .chain(loaded.user_sources.values())
                .cloned(),
        );
        // A failed file known to define `name` (shadowed, when it last loaded) may define it
        // still. A failed file that never loaded is unknown, and masks only by rank.
        let defines = &self.defines;
        let known_in_failed = |name: &str| {
            failed
                .iter()
                .any(|f| defines.get(*f).is_some_and(|names| names.contains(name)))
        };
        let mut seen = HashSet::new();
        for info in &loaded.contexts {
            let (name, server) = (info.name.as_str(), info.server.as_deref());
            if !seen.insert(name) {
                continue;
            }
            // A failed file before its own could define it: the entry may be a fallback.
            let context_masked = masked(&info.source, false);
            // The same for the cluster entry that gives it its server.
            let cluster_masked = info.cluster.as_ref().is_some_and(|cluster| {
                match loaded.cluster_sources.get(cluster) {
                    Some(file) => masked(file, false),
                    None => !loaded.errors.is_empty(),
                }
            });
            let provisional = context_masked || cluster_masked;
            // The user entry says nothing about the server, so it stays out of the history above,
            // but a write must not go out on one that may be a fallback.
            let user_masked =
                info.user
                    .as_ref()
                    .is_some_and(|user| match loaded.user_sources.get(user) {
                        Some(file) => masked(file, false),
                        None => !loaded.errors.is_empty(),
                    });
            let definition = context_fingerprint(&loaded.merged, name);
            match self.records.get_mut(name) {
                // A confirmed definition stays until a load confirms a change: a fallback never
                // replaces it, nor its file.
                Some(r) if r.present && r.confirmed && provisional => {
                    if cluster_masked && r.server.as_deref() != server {
                        self.last += 1;
                        r.gate = self.last;
                    }
                    // The context entry itself is certain: its file is what masks its removal.
                    if !context_masked {
                        r.source = info.source.clone();
                    }
                }
                // Same server: the file it now comes from is the one whose rank masks its
                // removal, confirmed or not (an unconfirmed record follows its fallback).
                Some(r) if r.present && r.server.as_deref() == server => {
                    r.source = info.source.clone();
                    r.confirmed |= !provisional;
                }
                Some(r) => {
                    self.last += 1;
                    let endpoint = if r.present && same_endpoint(r.server.as_deref(), server) {
                        r.endpoint
                    } else {
                        self.last
                    };
                    *r = Record {
                        server: server.map(str::to_owned),
                        source: info.source.clone(),
                        present: true,
                        confirmed: !provisional,
                        generation: self.last,
                        endpoint,
                        gate: r.gate,
                        definition,
                        provisional: false,
                    };
                }
                None => {
                    self.last += 1;
                    self.records.insert(
                        name.to_owned(),
                        Record {
                            server: server.map(str::to_owned),
                            source: info.source.clone(),
                            present: true,
                            confirmed: !provisional,
                            generation: self.last,
                            endpoint: self.last,
                            gate: 0,
                            definition,
                            provisional: false,
                        },
                    );
                }
            }
            if let Some(r) = self.records.get_mut(name) {
                r.definition = definition;
                r.provisional = provisional || user_masked;
            }
        }
        for (name, r) in &mut self.records {
            if !r.present || seen.contains(name.as_str()) {
                continue;
            }
            r.definition = None;
            r.provisional = true;
            if !masked(&r.source, true) && !known_in_failed(name) {
                self.last += 1;
                r.present = false;
                r.generation = self.last;
                r.endpoint = self.last;
            }
        }
    }

    /// Whether `path` gave the merge a context, or a winning cluster or user entry, when it last
    /// loaded.
    pub fn file_mattered(&self, path: &Path) -> bool {
        self.supplies.contains(path)
            || self
                .defines
                .get(path)
                .is_some_and(|names| !names.is_empty())
    }

    /// Counts each file in `blank` that mattered when it last loaded as failed in `loaded`: a
    /// writer that truncates before it writes leaves such a file for a moment, and what it
    /// defines is unknown until it has content again. A blank file that gave nothing is an
    /// ordinary empty file.
    pub fn fail_emptied(&self, loaded: &mut LoadedKubeconfig, blank: &[BlankFile]) {
        for BlankFile { path, .. } in blank {
            if self.file_mattered(path) {
                loaded.defines.retain(|(defined, _)| defined != path);
                loaded
                    .errors
                    .push((path.clone(), "the file is empty".to_owned()));
            }
        }
    }

    /// The pin for a session on `context` whose client was built for `server`: stale unless
    /// that is the context's confirmed server.
    pub fn pin(&self, context: &str, server: Option<&str>) -> ContextPin {
        let record = self.records.get(context).filter(|r| r.present);
        let generation = record
            .filter(|r| r.server.as_deref() == server)
            .map_or(0, |r| r.generation);
        let endpoint = record
            .filter(|r| same_endpoint(r.server.as_deref(), server))
            .map_or(0, |r| r.endpoint);
        ContextPin {
            generation,
            endpoint,
            gate: record.map_or(0, |r| r.gate),
            definition: record.and_then(|r| r.definition),
            server: server.map(str::to_owned),
        }
    }

    /// Whether a cluster entry's fallback showed since `pin` was taken: nothing new may start
    /// on it, though a session already running goes on.
    pub fn gate_moved(&self, context: &str, pin: &ContextPin) -> bool {
        self.records
            .get(context)
            .is_some_and(|r| r.gate != pin.gate)
    }

    /// Whether a write may go out on `pin`: the context's definition is the pinned one, and no
    /// file that can't be read could change it.
    pub fn definition_holds(&self, context: &str, pin: &ContextPin) -> bool {
        self.records
            .get(context)
            .is_some_and(|r| r.present && !r.provisional && r.definition == pin.definition)
    }

    /// Whether `fresh`, a read of the files newer than the last recorded load, still gives
    /// `context` the definition recorded for it at exactly `server`, with no file that failed
    /// able to change it. Judged by the rules of [`Self::record`]; records nothing.
    pub fn holds_in(&self, fresh: &LoadedKubeconfig, context: &str, server: &str) -> bool {
        let pin = self.pin(context, Some(server));
        self.pin_in(fresh, context, &pin) == FreshDefinition::Holds
    }

    /// What `fresh`, a read of the files newer than the last recorded load, says of the exact
    /// definition `pin` was taken on. Judged by the rules of [`Self::record`]; records nothing.
    pub fn pin_in(
        &self,
        fresh: &LoadedKubeconfig,
        context: &str,
        pin: &ContextPin,
    ) -> FreshDefinition {
        let mut next = self.clone();
        next.record(fresh);
        if pin.definition.is_some()
            && next.check(Pinned::Shell, context, pin).is_none()
            && !next.gate_moved(context, pin)
            && next.definition_holds(context, pin)
        {
            FreshDefinition::Holds
        } else if !fresh.errors.is_empty() && next.provisional(context) {
            FreshDefinition::Unreadable
        } else {
            FreshDefinition::Changed
        }
    }

    /// Whether a file that failed in the last load could define `context`, its cluster or its
    /// user.
    pub fn provisional(&self, context: &str) -> bool {
        self.records.get(context).is_none_or(|r| r.provisional)
    }

    /// Whether `context`'s recorded server is the exact URL `pin` was taken for. Only says how to
    /// word a refusal: a pin can be stale on the same server.
    pub fn on_pinned_server(&self, context: &str, pin: &ContextPin) -> bool {
        self.records
            .get(context)
            .is_some_and(|r| r.present && r.server == pin.server)
    }

    /// Whether one of the files that failed in this load defined `context` when it last loaded.
    pub fn defined_in_failed(&self, context: &str, failed: &[(PathBuf, String)]) -> bool {
        failed.iter().any(|(path, _)| {
            self.defines
                .get(path)
                .is_some_and(|names| names.contains(context))
        })
    }

    /// Whether the last load confirmed that `context` is gone.
    pub fn removed(&self, context: &str) -> bool {
        self.records.get(context).is_some_and(|r| !r.present)
    }

    /// What is known of `context` showing `server`.
    pub fn shown(&self, context: &str, server: Option<&str>) -> Shown {
        match self.records.get(context).filter(|r| r.present) {
            Some(r) => Shown {
                definition: r.generation,
                endpoint: r.endpoint,
                confirmed: r.confirmed && r.server.as_deref() == server,
            },
            None => Shown {
                definition: 0,
                endpoint: 0,
                confirmed: false,
            },
        }
    }

    /// The pin for a session on `context` with its confirmed server, for a session that gets
    /// its client later (a log session). Stale if the context is unknown or removed.
    pub fn pin_confirmed(&self, context: &str) -> ContextPin {
        let server = self.records.get(context).and_then(|r| r.server.as_deref());
        self.pin(context, server)
    }

    /// Why a session on `context` pinned at `pin` must end, or `None` while the context's
    /// confirmed definition is unchanged since. A shell compares the exact URL: the write gate
    /// relies on this, so it refuses even a change that only adds a trailing slash. Log sessions
    /// only read, and compare the endpoint.
    pub fn check(&self, session: Pinned, context: &str, pin: &ContextPin) -> Option<String> {
        let (current, pinned): (fn(&Record) -> u64, u64) = match session {
            Pinned::Shell => (|r| r.generation, pin.generation),
            Pinned::Logs => (|r| r.endpoint, pin.endpoint),
        };
        let moved = |r: &Record| match session {
            Pinned::Shell => r.server != pin.server,
            Pinned::Logs => !same_endpoint(r.server.as_deref(), pin.server.as_deref()),
        };
        let change = match self.records.get(context) {
            Some(r) if r.present && current(r) == pinned => return None,
            // Pinned stale at once, with a file still unreadable: opened on what a load showed
            // but did not confirm (a later file's fallback). With every file read, the context
            // has left that server for certain.
            Some(r) if r.present && pinned == 0 && self.failing => Change::Unconfirmed,
            Some(r) if r.present && moved(r) => Change::Repointed,
            // Removed and added back, or re-pointed and put back, since the session opened.
            Some(r) if r.present => Change::Changed,
            _ => Change::Removed,
        };
        Some(reason(session, context, change))
    }
}

/// How long a kubeconfig file that defined something must stay emptied and unwritten before it
/// counts as really empty. A writer that truncates first is done well within this.
pub(crate) const EMPTY_SETTLE: Duration = Duration::from_secs(5);

/// A file's modification time and size.
type Stamp = Option<(std::time::SystemTime, u64)>;

/// The files found emptied and still waiting to count as really empty: since when, and the
/// stamp they had then.
#[derive(Debug, Default)]
pub(crate) struct EmptiedFiles(HashMap<PathBuf, (std::time::Instant, Stamp)>);

impl EmptiedFiles {
    /// Of the files in `blank` that mattered when they last loaded, those not yet emptied and
    /// unwritten for `settle`. Those still count as failed.
    pub fn waiting(
        &mut self,
        history: &ContextHistory,
        blank: &[BlankFile],
        now: std::time::Instant,
        settle: Duration,
    ) -> Vec<BlankFile> {
        let emptied: Vec<&BlankFile> = blank
            .iter()
            .filter(|file| history.file_mattered(&file.path))
            .collect();
        self.0
            .retain(|path, _| emptied.iter().any(|file| &file.path == path));
        let mut waiting = Vec::new();
        for file in emptied {
            let since = match self.0.get(&file.path) {
                Some((since, stamp)) if *stamp == file.stamp => *since,
                _ => {
                    self.0.insert(file.path.clone(), (now, file.stamp));
                    now
                }
            };
            if now.duration_since(since) < settle {
                waiting.push(file.clone());
            }
        }
        waiting
    }
}

/// Each context's definition when a load last showed it with no unreadable file that could
/// change it. `None` is a context a load confirmed gone: no definition matches it, so what shows
/// up under its name behind an unreadable file waits, even with its old definition.
#[derive(Debug, Default)]
pub(crate) struct SettledDefinitions(HashMap<String, Option<u64>>);

impl SettledDefinitions {
    /// Whether `name`, showing the definition `shown`, may be a later file's fallback and differs
    /// from its confirmed definition. One that matches it, or that no load ever confirmed, is
    /// not paused.
    pub fn paused(&self, history: &ContextHistory, name: &str, shown: Option<u64>) -> bool {
        history.provisional(name)
            && self
                .0
                .get(name)
                .is_some_and(|confirmed| confirmed.is_none() || *confirmed != shown)
    }

    /// Whether `shown` is exactly the definition a load last confirmed for `name`.
    pub fn confirms(&self, name: &str, shown: Option<u64>) -> bool {
        shown.is_some() && self.0.get(name) == Some(&shown)
    }

    /// Takes the definitions `loaded`, already recorded in `history`, confirms, and forgets those
    /// of contexts it confirmed gone. Returns the contexts whose confirmed definition this
    /// changed.
    pub fn update(&mut self, history: &ContextHistory, loaded: &LoadedKubeconfig) -> Vec<String> {
        for (name, definition) in &mut self.0 {
            if history.removed(name) {
                *definition = None;
            }
        }
        let mut changed = Vec::new();
        for info in &loaded.contexts {
            if history.provisional(&info.name) {
                continue;
            }
            let definition = context_fingerprint(&loaded.merged, &info.name);
            let before = self.0.insert(info.name.clone(), definition);
            if before.is_some_and(|b| b != definition) {
                changed.push(info.name.clone());
            }
        }
        changed
    }
}

/// Which contexts' definitions are confirmed across kubeconfig loads, for a process that reads
/// the files itself. It applies the desktop app's rule: while a file can't be read, a context
/// showing anything but its confirmed definition is paused, one unchanged from it goes on, and
/// one no load has confirmed yet has nothing to differ from, so it goes on too.
#[derive(Debug, Default)]
pub struct ConfirmedDefinitions {
    history: ContextHistory,
    settled: SettledDefinitions,
    emptied: EmptiedFiles,
    /// [`FileRead::order`](crate::kubeconfig::FileRead::order) of the last read recorded.
    last_read: u64,
}

/// What became of a read given to [`ConfirmedDefinitions::judge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Recorded, with every file read: it stands until a file's stamp changes.
    Clean,
    /// Recorded, but a file failed or is settling: read again on the next call, even if no
    /// stamp changed, since a repair need not change one.
    Recheck,
    /// The files changed during the read, so its merge may never have existed. Not recorded.
    Unstable,
    /// A read that started later is already recorded. Not recorded.
    Obsolete,
}

/// A read of the kubeconfig files, with what it pauses.
#[derive(Debug)]
pub struct JudgedRead {
    pub loaded: LoadedKubeconfig,
    /// The contexts nothing new may start on.
    pub paused: HashSet<String>,
    pub outcome: ReadOutcome,
}

impl ConfirmedDefinitions {
    /// Records `read` if it is stable and newer than the last one recorded, and says what it
    /// pauses. A file that gave something and is now empty counts as failed until it has stayed
    /// so for a moment. A read that is not recorded can't vouch for anything, so only a context
    /// showing exactly its confirmed definition stays unpaused in it.
    pub fn judge(&mut self, read: KubeconfigRead) -> JudgedRead {
        self.judge_at(read, std::time::Instant::now(), EMPTY_SETTLE)
    }

    fn judge_at(
        &mut self,
        read: KubeconfigRead,
        now: std::time::Instant,
        settle: Duration,
    ) -> JudgedRead {
        let FileRead {
            mut loaded,
            blank,
            consistent,
            order,
            ..
        } = read.0;
        let outcome = if order <= self.last_read {
            ReadOutcome::Obsolete
        } else if !consistent {
            ReadOutcome::Unstable
        } else {
            self.last_read = order;
            let waiting = self.emptied.waiting(&self.history, &blank, now, settle);
            self.history.fail_emptied(&mut loaded, &waiting);
            self.history.record(&loaded);
            self.settled.update(&self.history, &loaded);
            if loaded.errors.is_empty() {
                ReadOutcome::Clean
            } else {
                ReadOutcome::Recheck
            }
        };
        let recorded = matches!(outcome, ReadOutcome::Clean | ReadOutcome::Recheck);
        let paused = loaded
            .contexts
            .iter()
            .map(|info| info.name.as_str())
            .filter(|name| {
                let shown = context_fingerprint(&loaded.merged, name);
                if recorded {
                    self.settled.paused(&self.history, name, shown)
                } else {
                    !self.settled.confirms(name, shown)
                }
            })
            .map(str::to_owned)
            .collect();
        JudgedRead {
            loaded,
            paused,
            outcome,
        }
    }
}

/// What a fresh read of the kubeconfig files says of a pinned definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreshDefinition {
    /// The files give the context exactly that definition, and no failed file could change it.
    Holds,
    /// A file that could define the context failed.
    Unreadable,
    /// The context has another definition now, or is gone.
    Changed,
}

/// How a session's context changed since it opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// It points to another API server now.
    Repointed,
    /// It changed and came back to the same server (removed and added back, say).
    Changed,
    /// A load that no failed file could mask lacks it.
    Removed,
    /// The session opened on a definition no load confirmed: a later file's, showing while a
    /// file before it can't be read.
    Unconfirmed,
}

/// Why a session on `context` ends, as shown.
pub(crate) fn reason(session: Pinned, context: &str, change: Change) -> String {
    let again = match session {
        Pinned::Logs => "open the logs again to read from it",
        Pinned::Shell => "open the shell again to run one there",
    };
    match change {
        Change::Removed => format!("\"{context}\" is no longer in your kubeconfig files"),
        Change::Repointed => {
            format!("\"{context}\" now points to a different API server; {again}")
        }
        Change::Changed => format!(
            "\"{context}\" changed in your kubeconfig files since this session opened; {again}"
        ),
        Change::Unconfirmed => format!(
            "\"{context}\" may be defined differently while one of your kubeconfig files can't \
             be read; {again} once it loads"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn reasons_keep_their_wording() {
        assert_eq!(
            reason(Pinned::Shell, "dev", Change::Repointed),
            "\"dev\" now points to a different API server; open the shell again to run one there"
        );
        assert_eq!(
            reason(Pinned::Logs, "dev", Change::Repointed),
            "\"dev\" now points to a different API server; open the logs again to read from it"
        );
        assert_eq!(
            reason(Pinned::Shell, "dev", Change::Removed),
            "\"dev\" is no longer in your kubeconfig files"
        );
        assert_eq!(
            reason(Pinned::Logs, "dev", Change::Changed),
            "\"dev\" changed in your kubeconfig files since this session opened; open the logs \
             again to read from it"
        );
    }

    // covers UP-06: a change of only a trailing slash leaves a log session running, and the exact
    // pin a shell and the write gate use still refuses it.
    #[test]
    fn a_trailing_slash_is_one_endpoint_for_logs_only() {
        let (with, without) = (Some("https://staging/"), Some("https://staging"));
        let mut h = ContextHistory::default();
        h.record(&load(&[("dev", without, "a")], &[]));
        let (logs, shell) = (h.pin("dev", without), h.pin("dev", without));
        h.record(&load(&[("dev", with, "a")], &[]));
        assert_eq!(h.check(Pinned::Logs, "dev", &logs), None);
        assert!(h.check(Pinned::Shell, "dev", &shell).is_some());
        // A log tab labelled before the slash changed, pinned after it.
        let late = h.pin("dev", without);
        assert_eq!(h.check(Pinned::Logs, "dev", &late), None);
        assert!(h.check(Pinned::Shell, "dev", &late).is_some(), "not exact");
        assert_eq!(h.check(Pinned::Shell, "dev", &h.pin("dev", with)), None);
    }

    // covers UP-06: a log session still ends on a real move, and on one that was undone.
    #[test]
    fn a_log_session_ends_on_a_move_even_if_undone() {
        let mut h = ContextHistory::default();
        h.record(&load(&[("dev", STAGING, "a")], &[]));
        let pin = h.pin("dev", STAGING);
        h.record(&load(&[("dev", PROD, "a")], &[]));
        assert!(h.check(Pinned::Logs, "dev", &pin).is_some());
        h.record(&load(&[("dev", STAGING, "a")], &[]));
        assert!(
            h.check(Pinned::Logs, "dev", &pin).is_some(),
            "moved and back"
        );
        assert!(
            h.check(Pinned::Shell, "dev", &pin).is_some(),
            "moved and back"
        );
    }

    // covers UP-06: the definition the dock compares changes on a confirmed move, even undone,
    // and not on an edit that keeps the server; a fallback is not confirmed.
    #[test]
    fn shown_tells_a_confirmed_move_from_a_same_server_edit() {
        let mut h = ContextHistory::default();
        h.record(&load(&[("dev", STAGING, "a")], &[]));
        let first = h.shown("dev", STAGING);
        assert!(first.confirmed);
        h.record(&load(&[("dev", STAGING, "b")], &[]));
        assert_eq!(h.shown("dev", STAGING), first, "same server");
        h.record(&load(&[("dev", PROD, "a")], &[]));
        h.record(&load(&[("dev", STAGING, "a")], &[]));
        assert_ne!(h.shown("dev", STAGING).definition, first.definition);

        let settled = h.shown("dev", STAGING);
        // `a` can't be read, and `b`'s definition shows meanwhile.
        h.record(&load(&[("dev", PROD, "b")], &["a"]));
        let fallback = h.shown("dev", PROD);
        assert!(!fallback.confirmed);
        assert_eq!(fallback.definition, settled.definition);
        assert_eq!(h.shown("nope", STAGING).definition, 0);
        assert!(!h.shown("nope", STAGING).confirmed);
    }

    // covers UP-06: a file that gave only the winning user entry is not an ordinary empty file
    // when it is truncated, so the later file's user is not taken for the definition.
    #[test]
    fn a_truncated_file_with_only_a_user_entry_counts_as_failed() {
        let mut h = ContextHistory::default();
        let mut first = load(&[("dev", STAGING, "b")], &[]);
        first.contexts[0].user = Some("u".to_owned());
        first
            .user_sources
            .insert("u".to_owned(), PathBuf::from("a"));
        h.record(&first);
        let pin = h.pin("dev", STAGING);

        let mut next = load(&[("dev", STAGING, "b")], &[]);
        next.contexts[0].user = Some("u".to_owned());
        next.user_sources.insert("u".to_owned(), PathBuf::from("b"));
        let blank = BlankFile {
            path: PathBuf::from("a"),
            stamp: None,
        };
        h.fail_emptied(&mut next, &[blank]);
        assert_eq!(next.errors.len(), 1);
        h.record(&next);
        assert!(h.provisional("dev"));
        assert!(!h.definition_holds("dev", &pin));
    }

    /// Two files, in order: `first` and `second`, each loaded through `confirmed`.
    fn load_files(
        confirmed: &mut ConfirmedDefinitions,
        paths: &[PathBuf],
        at: std::time::Instant,
    ) -> JudgedRead {
        confirmed.judge_at(KubeconfigRead::settled(paths), at, Duration::from_secs(5))
    }

    fn config(token: &str) -> String {
        format!(
            "clusters:\n- name: c\n  cluster:\n    server: https://one\n\
             contexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n\
             users:\n- name: u\n  user:\n    token: {token}\n"
        )
    }

    // covers UP-54: once a load confirms a context gone, its old definition vouches for nothing:
    // the same entry showing up in a later file behind an unreadable one waits.
    #[test]
    fn a_confirmed_removal_forgets_the_definition() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        let paths = [first.clone(), second.clone()];
        let at = std::time::Instant::now();
        let mut confirmed = ConfirmedDefinitions::default();

        std::fs::write(&first, config("one")).unwrap();
        assert!(load_files(&mut confirmed, &paths, at).paused.is_empty());
        std::fs::write(&first, "kind: Config\n").unwrap();
        let removed = load_files(&mut confirmed, &paths, at);
        assert_eq!(removed.outcome, ReadOutcome::Clean);
        assert!(confirmed.history.removed("dev"));

        std::fs::write(&first, "clusters: [not valid").unwrap();
        std::fs::write(&second, config("one")).unwrap();
        let back = load_files(&mut confirmed, &paths, at);
        assert!(back.loaded.context("dev").is_some());
        assert!(back.paused.contains("dev"), "its old definition is gone");
        let shown = context_fingerprint(&back.loaded.merged, "dev");
        assert!(confirmed.settled.paused(&confirmed.history, "dev", shown));

        std::fs::write(&first, "kind: Config\n").unwrap();
        assert!(load_files(&mut confirmed, &paths, at).paused.is_empty());
    }

    // covers UP-54: with an earlier file unreadable, a context showing its confirmed definition
    // goes on, one showing another is paused, and repairing the file ends the pause.
    #[test]
    fn a_context_is_paused_only_on_a_definition_it_never_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        let paths = [first.clone(), second.clone()];
        let now = std::time::Instant::now();
        std::fs::write(&first, config("one")).unwrap();
        std::fs::write(&second, config("two")).unwrap();
        let mut confirmed = ConfirmedDefinitions::default();
        let judged = load_files(&mut confirmed, &paths, now);
        assert_eq!(judged.outcome, ReadOutcome::Clean);
        assert!(!judged.paused.contains("dev"));

        std::fs::write(&first, "not: [valid").unwrap();
        let judged = load_files(&mut confirmed, &paths, now);
        assert_eq!(judged.outcome, ReadOutcome::Recheck);
        assert!(judged.paused.contains("dev"), "the second file's entry");

        std::fs::write(&second, config("one")).unwrap();
        let judged = load_files(&mut confirmed, &paths, now);
        assert!(!judged.paused.contains("dev"), "same as confirmed");

        std::fs::write(&second, config("two")).unwrap();
        std::fs::write(&first, config("one")).unwrap();
        let judged = load_files(&mut confirmed, &paths, now);
        assert!(!judged.paused.contains("dev"), "repaired");
    }

    // covers UP-54: a read the files changed under confirms nothing: it can't vouch for a context
    // first seen in it, and a later fallback matching its merge is still paused.
    #[test]
    fn an_unstable_read_confirms_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        let paths = [first.clone(), second.clone()];
        let now = std::time::Instant::now();
        std::fs::write(&first, config("one")).unwrap();
        std::fs::write(&second, config("two")).unwrap();
        let unstable = |paths: &[PathBuf]| {
            let mut read = KubeconfigRead::settled(paths);
            read.0.consistent = false;
            read
        };

        let mut unseen = ConfirmedDefinitions::default();
        let judged = unseen.judge_at(unstable(&paths), now, EMPTY_SETTLE);
        assert_eq!(judged.outcome, ReadOutcome::Unstable);
        assert!(judged.paused.contains("dev"), "first seen in a mixed read");

        let mut confirmed = ConfirmedDefinitions::default();
        load_files(&mut confirmed, &paths, now);
        let judged = confirmed.judge_at(unstable(&paths), now, EMPTY_SETTLE);
        assert!(!judged.paused.contains("dev"), "unchanged keeps connecting");

        std::fs::write(&first, config("two")).unwrap();
        let judged = confirmed.judge_at(unstable(&paths), now, EMPTY_SETTLE);
        assert!(judged.paused.contains("dev"), "changed");
        std::fs::write(&first, "not: [valid").unwrap();
        let judged = load_files(&mut confirmed, &paths, now);
        assert!(judged.paused.contains("dev"), "the mixed read was not kept");
    }

    // covers UP-54: a read that finishes after a later one was recorded changes nothing.
    #[test]
    fn an_obsolete_read_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        let paths = [first.clone(), second.clone()];
        let now = std::time::Instant::now();
        std::fs::write(&first, config("one")).unwrap();
        std::fs::write(&second, config("two")).unwrap();
        let mut confirmed = ConfirmedDefinitions::default();
        let older = KubeconfigRead::settled(&paths);
        std::fs::write(&first, config("two")).unwrap();
        load_files(&mut confirmed, &paths, now);

        let judged = confirmed.judge_at(older, now, EMPTY_SETTLE);
        assert_eq!(judged.outcome, ReadOutcome::Obsolete);
        assert!(judged.paused.contains("dev"), "not what is confirmed now");

        std::fs::write(&first, "not: [valid").unwrap();
        let judged = load_files(&mut confirmed, &paths, now);
        assert!(
            !judged.paused.contains("dev"),
            "the later read still counts"
        );
    }

    // covers UP-54: a context first seen behind a broken file has no confirmed definition to
    // differ from, so it is not paused. Once a load confirms it, a different fallback is.
    #[test]
    fn a_context_first_seen_with_a_broken_file_is_not_paused() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        std::fs::write(&first, "not: [valid").unwrap();
        std::fs::write(&second, config("two")).unwrap();
        let mut confirmed = ConfirmedDefinitions::default();
        let paths = [first.clone(), second.clone()];
        let now = std::time::Instant::now();
        let judged = load_files(&mut confirmed, &paths, now);
        assert_eq!(judged.outcome, ReadOutcome::Recheck);
        assert!(!judged.paused.contains("dev"), "nothing to differ from");

        std::fs::write(&first, config("one")).unwrap();
        let judged = load_files(&mut confirmed, &paths, now);
        assert_eq!(judged.outcome, ReadOutcome::Clean);
        assert!(!judged.paused.contains("dev"), "confirmed");

        std::fs::write(&first, "not: [valid").unwrap();
        let judged = load_files(&mut confirmed, &paths, now);
        assert!(judged.paused.contains("dev"), "differs from confirmed");
    }

    // covers UP-54: an emptied file counts as failed while it settles, then as really empty, the
    // same as in the app; a file that gave nothing is an ordinary empty file throughout.
    #[test]
    fn an_emptied_file_pauses_until_it_settles() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        let paths = [first.clone(), second.clone()];
        let now = std::time::Instant::now();
        std::fs::write(&first, config("one")).unwrap();
        std::fs::write(&second, config("two")).unwrap();
        let mut confirmed = ConfirmedDefinitions::default();
        load_files(&mut confirmed, &paths, now);

        std::fs::write(&first, "").unwrap();
        let judged = load_files(&mut confirmed, &paths, now);
        assert_eq!(judged.outcome, ReadOutcome::Recheck);
        assert_eq!(judged.loaded.errors.len(), 1);
        assert!(judged.paused.contains("dev"));

        let later = now + Duration::from_secs(5);
        let judged = load_files(&mut confirmed, &paths, later);
        assert_eq!(judged.outcome, ReadOutcome::Clean);
        assert!(judged.loaded.errors.is_empty());
        assert!(!judged.paused.contains("dev"), "the change is confirmed");

        let mut fresh = ConfirmedDefinitions::default();
        let judged = load_files(&mut fresh, &paths, now);
        assert_eq!(judged.outcome, ReadOutcome::Clean);
    }

    const STAGING: Option<&str> = Some("https://staging");
    const PROD: Option<&str> = Some("https://prod");

    /// A load of the files `a`, `b` and `c`, in that order: each context with its server and
    /// the file that won it, and the files that failed to load.
    /// What each file that loaded defines is taken from `contexts` (no shadowed names); see
    /// [`load_shadowing`] for those.
    fn load(contexts: &[(&str, Option<&str>, &str)], failed: &[&str]) -> LoadedKubeconfig {
        let defines = ["a", "b", "c", "d"]
            .into_iter()
            .filter(|f| !failed.contains(f))
            .map(|f| {
                let names = contexts.iter().filter(|c| c.2 == f).map(|c| c.0.to_owned());
                (PathBuf::from(f), names.collect())
            })
            .collect();
        load_shadowing(contexts, failed, defines)
    }

    fn load_shadowing(
        contexts: &[(&str, Option<&str>, &str)],
        failed: &[&str],
        defines: Vec<(PathBuf, Vec<String>)>,
    ) -> LoadedKubeconfig {
        let contexts = contexts
            .iter()
            .map(|&(name, server, file)| crate::kubeconfig::ContextInfo {
                name: name.to_owned(),
                cluster: None,
                user: None,
                namespace: None,
                server: server.map(str::to_owned),
                proxy_url: None,
                source: PathBuf::from(file),
            })
            .collect();
        LoadedKubeconfig {
            merged: Default::default(),
            contexts,
            paths: ["a", "b", "c", "d"].map(PathBuf::from).to_vec(),
            errors: failed
                .iter()
                .map(|f| (PathBuf::from(f), "did not parse".to_owned()))
                .collect(),
            defines,
            cluster_sources: HashMap::new(),
            user_sources: HashMap::new(),
            blank: Vec::new(),
        }
    }

    /// [`load`] with `dev` on the cluster entry `shared`, which `file` won.
    fn on_cluster(mut loaded: LoadedKubeconfig, file: &str) -> LoadedKubeconfig {
        loaded.contexts[0].cluster = Some("shared".to_owned());
        loaded
            .cluster_sources
            .insert("shared".to_owned(), PathBuf::from(file));
        loaded
    }

    // covers UP-06: `a` defines the context, `b` its cluster, and `c` shadows that cluster. With
    // `b` unreadable, `c`'s URL is a fallback: a running session goes on, and nothing new starts
    // on it, or on a pin from before it, even once `b` is back.
    #[test]
    fn a_shadowed_cluster_entry_is_a_fallback() {
        let mut h = ContextHistory::default();
        h.record(&on_cluster(load(&[("dev", STAGING, "a")], &[]), "b"));
        let pin = h.pin("dev", STAGING);

        h.record(&on_cluster(load(&[("dev", PROD, "a")], &["b"]), "c"));
        assert_eq!(h.check(Pinned::Shell, "dev", &pin), None, "still running");
        assert_eq!(h.check(Pinned::Logs, "dev", &pin), None, "still running");
        assert!(h.gate_moved("dev", &pin), "nothing new starts on it");
        assert!(!h.shown("dev", PROD).confirmed);
        let fallback = h.pin("dev", PROD);
        assert_eq!(
            h.check(Pinned::Shell, "dev", &fallback),
            Some(reason(Pinned::Shell, "dev", Change::Unconfirmed))
        );

        h.record(&on_cluster(load(&[("dev", STAGING, "a")], &[]), "b"));
        assert_eq!(h.check(Pinned::Shell, "dev", &pin), None, "survived");
        assert!(h.gate_moved("dev", &pin), "the gate stays as strict");
        assert!(h.on_pinned_server("dev", &pin), "refused, but not moved");
        assert!(!h.on_pinned_server("dev", &fallback), "this one did move");
        let fresh = h.pin("dev", STAGING);
        assert!(!h.gate_moved("dev", &fresh));
    }

    // covers UP-06: `a` defines the cluster, `b` the context, `c` a fallback cluster. With `a`
    // unreadable both entries are masked, and the gate still refuses the old pin.
    #[test]
    fn the_gate_moves_when_context_and_cluster_are_both_masked() {
        let mut h = ContextHistory::default();
        h.record(&on_cluster(load(&[("dev", STAGING, "b")], &[]), "a"));
        let pin = h.pin("dev", STAGING);
        h.record(&on_cluster(load(&[("dev", PROD, "b")], &["a"]), "c"));
        assert_eq!(h.check(Pinned::Shell, "dev", &pin), None, "still running");
        assert!(h.gate_moved("dev", &pin));
    }

    // covers UP-06: the context moved to `a`, certain, while its cluster entry is masked. Once
    // `a` drops it, the failed `b`, ranked below `a` and never defining it, hides nothing.
    #[test]
    fn a_confirmed_context_file_decides_its_removal() {
        let mut h = ContextHistory::default();
        h.record(&on_cluster(load(&[("dev", STAGING, "c")], &[]), "c"));
        let pin = h.pin("dev", STAGING);
        h.record(&on_cluster(load(&[("dev", STAGING, "a")], &["b"]), "c"));
        assert_eq!(h.check(Pinned::Shell, "dev", &pin), None);
        h.record(&load(&[], &["b"]));
        assert_eq!(
            h.check(Pinned::Shell, "dev", &pin),
            Some(reason(Pinned::Shell, "dev", Change::Removed))
        );
    }

    // covers UP-54: a user entry that may be a fallback makes the pin provisional for the write
    // gate, and leaves a running session's pin as it was.
    #[test]
    fn a_masked_user_entry_is_provisional() {
        let on_user = |mut loaded: LoadedKubeconfig, file: &str| {
            loaded.contexts[0].user = Some("me".to_owned());
            loaded
                .user_sources
                .insert("me".to_owned(), PathBuf::from(file));
            loaded
        };
        let mut h = ContextHistory::default();
        h.record(&on_user(load(&[("dev", STAGING, "a")], &[]), "b"));
        let pin = h.pin("dev", STAGING);
        assert!(h.definition_holds("dev", &pin));

        h.record(&on_user(load(&[("dev", STAGING, "a")], &["b"]), "c"));
        assert!(!h.definition_holds("dev", &pin));
        assert_eq!(h.check(Pinned::Shell, "dev", &pin), None, "still running");

        let mut missing = load(&[("dev", STAGING, "a")], &["b"]);
        missing.contexts[0].user = Some("me".to_owned());
        h.record(&missing);
        assert!(!h.definition_holds("dev", &pin), "may be in `b`");

        h.record(&on_user(load(&[("dev", STAGING, "a")], &[]), "b"));
        assert!(h.definition_holds("dev", &pin));
        assert_eq!(h.pin("dev", STAGING), pin);
    }

    // covers UP-54: a pin holds only for the definition it was taken on, while a running session
    // on the same server goes on.
    #[test]
    fn a_changed_user_entry_no_longer_holds() {
        let with_token = |token: &str| {
            let mut loaded = load(&[("dev", STAGING, "a")], &[]);
            loaded.merged = kube::config::Kubeconfig::from_yaml(&format!(
                "contexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n\
                 users:\n- name: u\n  user:\n    token: {token}\n"
            ))
            .unwrap();
            loaded
        };
        let mut h = ContextHistory::default();
        h.record(&with_token("one"));
        let pin = h.pin("dev", STAGING);
        assert!(h.definition_holds("dev", &pin));
        h.record(&with_token("two"));
        assert!(!h.definition_holds("dev", &pin));
        assert_ne!(h.pin("dev", STAGING), pin);
        assert_eq!(h.check(Pinned::Shell, "dev", &pin), None);
    }

    // covers UP-06: a cluster entry no file defines, while a file failed, may be in that file.
    #[test]
    fn a_missing_cluster_entry_with_a_failed_file_is_unconfirmed() {
        let mut h = ContextHistory::default();
        h.record(&on_cluster(load(&[("dev", STAGING, "a")], &[]), "b"));
        let mut failed = load(&[("dev", None, "a")], &["b"]);
        failed.contexts[0].cluster = Some("shared".to_owned());
        h.record(&failed);
        assert!(!h.shown("dev", None).confirmed);
        assert_eq!(h.check(Pinned::Logs, "dev", &h.pin("dev", STAGING)), None);
    }

    // covers UP-06: a request pinned to a server the context left for certain says it moved; the
    // kubeconfig files all read, so no unreadable file is to blame.
    #[test]
    fn a_stale_pin_says_moved_when_every_file_loaded() {
        let mut h = history(&[("dev", STAGING, "a")]);
        h.record(&load(&[("dev", PROD, "a")], &[]));
        let late = h.pin("dev", STAGING);
        assert_eq!(
            h.check(Pinned::Logs, "dev", &late),
            Some(reason(Pinned::Logs, "dev", Change::Repointed))
        );
    }

    fn history(contexts: &[(&str, Option<&str>, &str)]) -> ContextHistory {
        let mut history = ContextHistory::default();
        history.record(&load(contexts, &[]));
        history
    }

    // covers UP-06, UP-41
    #[test]
    fn a_pinned_session_ends_once_its_context_moves_or_goes() {
        let mut h = history(&[("main", STAGING, "a"), ("other", PROD, "a")]);
        let pin = h.pin("main", STAGING);
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
        // Another context changing is none of its business; nor is the same load again.
        h.record(&load(
            &[("main", STAGING, "a"), ("other", STAGING, "a")],
            &[],
        ));
        h.record(&load(
            &[("main", STAGING, "a"), ("other", STAGING, "a")],
            &[],
        ));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);

        h.record(&load(&[("main", PROD, "a")], &[]));
        let moved = Some(reason(Pinned::Shell, "main", Change::Repointed));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), moved);
        // A session opened now is pinned to the new server.
        assert_eq!(h.check(Pinned::Shell, "main", &h.pin("main", PROD)), None);

        let pin = h.pin("main", PROD);
        h.record(&load(&[("other", PROD, "a")], &[]));
        let gone = Some(reason(Pinned::Shell, "main", Change::Removed));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), gone);
        // A context never seen is stale at once.
        assert_eq!(
            h.check(Pinned::Shell, "nowhere", &h.pin_confirmed("nowhere")),
            Some(reason(Pinned::Shell, "nowhere", Change::Removed))
        );
    }

    // covers UP-06, UP-41: changes undone between two checks still end the session, and the
    // reason does not claim another server when the context points where it did.
    #[test]
    fn a_change_undone_before_the_next_check_still_counts() {
        let mut h = history(&[("main", STAGING, "a")]);
        let pin = h.pin("main", STAGING);
        h.record(&load(&[("main", PROD, "a")], &[]));
        h.record(&load(&[("main", STAGING, "a")], &[]));
        let changed = Some(reason(Pinned::Shell, "main", Change::Changed));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), changed);
        // Removed and added back on the same server, too.
        let pin = h.pin("main", STAGING);
        h.record(&load(&[], &[]));
        h.record(&load(&[("main", STAGING, "a")], &[]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), changed);
        // Re-pointed, then re-pointed again elsewhere: another server.
        let pin = h.pin("main", STAGING);
        h.record(&load(&[("main", PROD, "a")], &[]));
        h.record(&load(&[("main", Some("https://dev"), "a")], &[]));
        assert_eq!(
            h.check(Pinned::Shell, "main", &pin),
            Some(reason(Pinned::Shell, "main", Change::Repointed))
        );
    }

    // covers UP-06: a file caught half-written is skipped by the loader. Its contexts are
    // missing from that load, which does not remove them.
    #[test]
    fn a_file_that_fails_to_load_does_not_remove_its_contexts() {
        let mut h = history(&[("main", STAGING, "b"), ("other", PROD, "c")]);
        let pin = h.pin("main", STAGING);
        h.record(&load(&[("other", PROD, "c")], &["b"]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
        // A failed file before it could define it as well.
        h.record(&load(&[("other", PROD, "c")], &["a"]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
        // The file comes back as it was.
        h.record(&load(&[("main", STAGING, "b"), ("other", PROD, "c")], &[]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
    }

    // covers UP-06, UP-41: a file that cannot be read masks nothing it could not define. A
    // readable file removing a context, or re-pointing it, with a file after it broken, is a
    // confirmed change.
    #[test]
    fn a_broken_file_after_a_context_masks_no_change_to_it() {
        let mut h = history(&[("main", STAGING, "b"), ("other", PROD, "b")]);
        let pin = h.pin("main", STAGING);
        h.record(&load(&[("other", PROD, "b")], &["c"]));
        assert_eq!(
            h.check(Pinned::Shell, "main", &pin),
            Some(reason(Pinned::Shell, "main", Change::Removed))
        );
        // A new log session on it ends at once rather than wait for the file.
        let logs = h.pin_confirmed("main");
        assert_eq!(
            h.check(Pinned::Logs, "main", &logs),
            Some(reason(Pinned::Logs, "main", Change::Removed))
        );

        let mut h = history(&[("main", STAGING, "a")]);
        let pin = h.pin("main", STAGING);
        h.record(&load(&[("main", PROD, "a")], &["b"]));
        assert_eq!(
            h.check(Pinned::Shell, "main", &pin),
            Some(reason(Pinned::Shell, "main", Change::Repointed))
        );
    }

    // covers UP-06: while the first file defining `main` is unreadable, a later file's `main`
    // shows instead. That fallback is provisional: sessions on the first definition go on, and
    // nothing changed once the first file is back as it was.
    #[test]
    fn a_fallback_while_the_first_file_is_unreadable_is_provisional() {
        let mut h = history(&[("main", STAGING, "a")]);
        let pin = h.pin("main", STAGING);
        h.record(&load(&[("main", PROD, "b")], &["a"]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
        // A shell opened on the fallback meanwhile is stale: its server is not confirmed.
        let on_fallback = h.pin("main", PROD);
        let unconfirmed = Some(reason(Pinned::Shell, "main", Change::Unconfirmed));
        assert_eq!(h.check(Pinned::Shell, "main", &on_fallback), unconfirmed);
        h.record(&load(&[("main", STAGING, "a")], &[]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);

        // Back, but re-pointed: confirmed.
        h.record(&load(&[("main", PROD, "b")], &["a"]));
        h.record(&load(&[("main", PROD, "a")], &[]));
        assert_eq!(
            h.check(Pinned::Shell, "main", &pin),
            Some(reason(Pinned::Shell, "main", Change::Repointed))
        );
    }

    // covers UP-06: a fallback on the same server never replaces the confirmed definition's
    // file, and a context first seen while a file before it is unreadable is not confirmed
    // history: its absence while that file still fails removes nothing.
    #[test]
    fn what_a_load_cannot_confirm_never_replaces_confirmed_history() {
        let mut h = history(&[("main", STAGING, "a")]);
        h.record(&load(&[("main", STAGING, "b")], &["a"]));
        let r = &h.records["main"];
        assert!(r.confirmed && r.source == Path::new("a"), "{r:?}");

        // At startup, the first file already unreadable.
        let mut h = ContextHistory::default();
        h.record(&load(&[("main", STAGING, "c")], &["a"]));
        let r = &h.records["main"];
        assert!(!r.confirmed, "{r:?}");
        let pin = h.pin("main", STAGING);
        assert_eq!(
            h.check(Pinned::Shell, "main", &pin),
            None,
            "sessions on it run"
        );
        // The fallback goes while the first file still fails: the first may define it.
        h.record(&load(&[], &["a"]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
        // The first file loads and confirms it on the same server: nothing changed.
        h.record(&load(&[("main", STAGING, "a")], &[]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
        assert!(h.records["main"].confirmed);
        // Then a fallback on the same server while it fails again, and a removal from that
        // fallback's file: the confirmed file is still the one that counts.
        h.record(&load(&[("main", STAGING, "c")], &["a"]));
        h.record(&load(&[], &["a"]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
    }

    // covers UP-06: an unconfirmed context that moves, on the same server, to a later file's
    // definition while a file between them fails is masked by that file: dropped from the later
    // file, it may still be in the failed one.
    #[test]
    fn an_unconfirmed_context_follows_its_file() {
        let mut h = ContextHistory::default();
        // `a` unreadable at startup: `main` from `b` is unconfirmed.
        h.record(&load(&[("main", STAGING, "b")], &["a"]));
        let pin = h.pin("main", STAGING);
        // Now from `d` on the same server, `c` unreadable.
        h.record(&load(&[("main", STAGING, "d")], &["c"]));
        assert_eq!(h.records["main"].source, Path::new("d"));
        assert!(!h.records["main"].confirmed);
        // `d` drops it while `c` still fails: `c` may define it.
        h.record(&load(&[], &["c"]));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);
    }

    // covers UP-06: `main` in `a` shadows a `main` on the same server in `c`. When `a` drops it
    // while `c` can't be read, `c` may still define it: not a removal. A file that failed but
    // never defined it masks nothing.
    #[test]
    fn a_shadowed_definition_in_a_failed_file_masks_a_removal() {
        let both = |main_in_a: bool| {
            let a = if main_in_a {
                vec!["main".to_owned()]
            } else {
                vec![]
            };
            vec![
                (PathBuf::from("a"), a),
                (PathBuf::from("b"), vec!["other".to_owned()]),
                (PathBuf::from("c"), vec!["main".to_owned()]),
            ]
        };
        let mut h = ContextHistory::default();
        h.record(&load_shadowing(
            &[("main", STAGING, "a"), ("other", PROD, "b")],
            &[],
            both(true),
        ));
        let pin = h.pin("main", STAGING);
        let mut without_c = both(false);
        without_c.pop();
        h.record(&load_shadowing(&[("other", PROD, "b")], &["c"], without_c));
        assert_eq!(h.check(Pinned::Shell, "main", &pin), None);

        // `b` failing instead, which never defined `main`, masks nothing.
        let mut h = ContextHistory::default();
        h.record(&load_shadowing(
            &[("main", STAGING, "a"), ("other", PROD, "b")],
            &[],
            both(true),
        ));
        let pin = h.pin("main", STAGING);
        // `a` and `c` both drop it.
        let dropped = vec![(PathBuf::from("a"), vec![]), (PathBuf::from("c"), vec![])];
        h.record(&load_shadowing(&[], &["b"], dropped));
        assert_eq!(
            h.check(Pinned::Shell, "main", &pin),
            Some(reason(Pinned::Shell, "main", Change::Removed))
        );
    }

    /// Polls `recheck` once; a check it starts takes the next of `answers`.
    async fn poll_once(
        recheck: &mut Recheck<u8>,
        answers: &mut VecDeque<oneshot::Receiver<u8>>,
        started: &mut usize,
    ) -> Poll<u8> {
        futures::future::poll_fn(|cx| {
            Poll::Ready(recheck.poll_check(cx, || {
                *started += 1;
                let rx = answers.pop_front().expect("a scripted answer");
                async move { rx.await.unwrap_or(0) }.boxed()
            }))
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn checks_run_every_interval_and_survive_dropped_polls() {
        let every = Duration::from_secs(30);
        let begin = Instant::now();
        let mut recheck = Recheck::new(every);
        let (first, rx) = oneshot::channel();
        let (second, rx2) = oneshot::channel();
        let mut answers = VecDeque::from([rx, rx2]);
        let mut started = 0;
        assert!(
            poll_once(&mut recheck, &mut answers, &mut started)
                .await
                .is_pending()
        );
        assert_eq!(started, 0, "not due yet");
        tokio::time::sleep(every).await;
        // Due: the check starts once, and stays in flight across polls that are dropped.
        assert!(
            poll_once(&mut recheck, &mut answers, &mut started)
                .await
                .is_pending()
        );
        assert!(
            poll_once(&mut recheck, &mut answers, &mut started)
                .await
                .is_pending()
        );
        assert_eq!(started, 1);
        first.send(7).unwrap();
        assert_eq!(
            poll_once(&mut recheck, &mut answers, &mut started).await,
            Poll::Ready(7)
        );
        // The next is due `every` after the first started, not before.
        tokio::time::sleep_until(begin + every * 2 - Duration::from_millis(1)).await;
        assert!(
            poll_once(&mut recheck, &mut answers, &mut started)
                .await
                .is_pending()
        );
        assert_eq!(started, 1);
        tokio::time::sleep_until(begin + every * 2).await;
        second.send(9).unwrap();
        assert_eq!(
            poll_once(&mut recheck, &mut answers, &mut started).await,
            Poll::Ready(9)
        );
        assert_eq!(started, 2);
    }
}
