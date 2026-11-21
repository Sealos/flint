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

use crate::kubeconfig::LoadedKubeconfig;
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
    server: Option<String>,
}

impl ContextPin {
    /// The API server the session's client was built for.
    pub fn server(&self) -> Option<&str> {
        self.server.as_deref()
    }
}

/// A context's definition, as kubeconfig loads last saw it.
#[derive(Debug)]
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
///   ([`Change::Unconfirmed`]): it never moves to the confirmed server.
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
#[derive(Debug, Default)]
pub(crate) struct ContextHistory {
    records: HashMap<String, Record>,
    /// The context names each file defined when it last loaded, shadowed ones included.
    defines: HashMap<PathBuf, HashSet<String>>,
    /// The last generation handed out; generations are unique across contexts.
    last: u64,
}

impl ContextHistory {
    /// Records a load: its contexts (each from the file that won it), its files in order, and
    /// the files that failed.
    pub fn record(&mut self, loaded: &LoadedKubeconfig) {
        let rank = |path: &Path| loaded.paths.iter().position(|p| p == path);
        let first_failed = loaded.errors.iter().filter_map(|(p, _)| rank(p)).min();
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
            let provisional = masked(&info.source, false);
            match self.records.get_mut(name) {
                // A confirmed definition stays until a load confirms a change: a fallback never
                // replaces it, nor its file.
                Some(r) if r.present && r.confirmed && provisional => {}
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
                        },
                    );
                }
            }
        }
        for (name, r) in &mut self.records {
            if r.present
                && !seen.contains(name.as_str())
                && !masked(&r.source, true)
                && !known_in_failed(name)
            {
                self.last += 1;
                r.present = false;
                r.generation = self.last;
                r.endpoint = self.last;
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
            server: server.map(str::to_owned),
        }
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
            // Pinned stale at once: opened on what a load showed but did not confirm (a later
            // file's fallback, while the first one can't be read).
            Some(r) if r.present && pinned == 0 => Change::Unconfirmed,
            Some(r) if r.present && moved(r) => Change::Repointed,
            // Removed and added back, or re-pointed and put back, since the session opened.
            Some(r) if r.present => Change::Changed,
            _ => Change::Removed,
        };
        Some(reason(session, context, change))
    }
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
        }
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
