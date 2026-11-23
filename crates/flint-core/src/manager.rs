//! `ClusterManager`: the core's single owner of cluster state, running as one task on the core
//! runtime. It handles [`Command`]s, connection results, and kubeconfig and settings reloads, and
//! emits [`Event`]s.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kube::Client;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};

use crate::activity;
use crate::cluster::{self, CONNECT_TIMEOUT, ClusterId, ServerVersion};
use crate::connection::member::{Member, ServerIdentity};
use crate::credential::{CredentialSource, WriteCredential};
use crate::discovery::ClusterDiscovery;
use crate::handle::{
    ActivationRefused, ClusterSummary, ClustersSnapshot, Command, ConnectionState, CoreConfig,
    Event, ReadPin, Request,
};
use crate::kubeconfig::{
    self, BlankFile, ContextInfo, FileRead, KubeconfigReader, LoadedKubeconfig, ReadBusy,
};
use crate::logs::{self, KubeLogSource, KubePodSource, LogError, SessionConfig};
use crate::protection::{ClusterClients, GatedClient, SessionUnlocks, WriteRefusal, protection};
use crate::read::{self, KindId, KindInfo};
use crate::recheck::{
    ContextHistory, ContextPin, DefinitionHolds, EMPTY_SETTLE, EmptiedFiles, FreshDefinition,
    Pinned, SettledDefinitions,
};
use crate::settings::{self, AiScope, Settings, server_identity};
use crate::store::{KindSnapshot, SyncState};
use crate::watch::{ClusterWatches, EngineConfig, HubRegistry, KubeSource};
use crate::write::WriteError;

mod file_watch;
mod forwards;
mod keep_watching;
mod running_set;
mod selected_retry;
mod telemetry;

pub(crate) use keep_watching::KeptGrants;
#[cfg(test)]
pub(crate) use keep_watching::QUIT_WAIT;

use crate::connection::member::{AttemptOutcome, MemberEvent};
use crate::connection::policy::{CredentialAccess, CredentialLease, ForegroundOnly, Role};
use crate::connection::schedule::{AttemptId, Schedule};
use file_watch::{FileWatcher, WatchScope};
#[cfg(test)]
use file_watch::{Watched, classify, missing_dirs, watch_names, watch_plan};

/// CRD changes come in bursts too (create, then status updates as names are accepted).
const CRD_DEBOUNCE: Duration = Duration::from_millis(500);
/// The API server publishes a new CRD in discovery a moment after the CRD changes. When
/// discovery does not match the CRD list yet, it is retried this often, at most
/// `DISCOVERY_RETRIES` times per change.
const DISCOVERY_RETRY: Duration = Duration::from_secs(1);
const DISCOVERY_RETRIES: u32 = 10;
/// A failed discovery (API server restarting, network blip) is retried with a backoff that
/// doubles from `DISCOVERY_RETRY` up to this, while the cluster is active.
const DISCOVERY_RETRY_MAX: Duration = Duration::from_secs(60);

/// One kubeconfig context: its connection, discovery cache, and watches.
pub(crate) struct Cluster {
    info: ContextInfo,
    fingerprint: Option<u64>,
    state: ConnectionState,
    client: Option<ClusterClients>,
    /// The credential `client`'s write client presents: set and cleared with it.
    write_credential: Option<WriteCredential>,
    /// Id of the latest connection attempt, from the manager-wide counter, so a late result of an
    /// older attempt (even one made before the context was removed and re-added) is ignored.
    attempt: u64,
    /// Stops the latest attempt, so one that a newer attempt or a new slot replaced ends rather
    /// than run on. Its result, if already sent, is still discarded by `attempt`.
    connect_task: Option<AbortHandle>,
    /// The command permission of the latest connection or attempt. Each attempt gets its own,
    /// and the one it replaces is revoked, so a client outliving its slot can't regain it.
    access: CredentialAccess,
    /// The latest attempt's use of a credential that runs a command, if it began allowed to. Once
    /// ended, that attempt's result says nothing about the cluster, success or error.
    attempt_lease: Option<CredentialLease>,
    /// The running set's attempt that the connection in progress answers.
    member_attempt: Option<AttemptId>,
    /// The latest attempt whose credential was refused, so its failure means signing in.
    rejected_attempt: Option<u64>,
    /// The latest attempt that failed in a way time can fix, so it may be retried.
    transient_attempt: Option<u64>,
    /// An automatic retry was ended by a change: nothing reconnects it until it is selected.
    awaits_selection: bool,
    /// A pause or a changed definition ended this slot's connection: a member connects again at
    /// the first load that leaves it on its server and not paused.
    reconnect: bool,
    /// The slot's generation, from the manager-wide counter: a new slot (a new context, one
    /// re-pointed or otherwise changed, one back after being removed) has a new one. Front ends
    /// tell a cluster that started over by it, whatever lists they skipped (#255).
    generation: u64,
    /// Kept while the cluster is not active (ADR-0008).
    discovery: Option<Arc<ClusterDiscovery>>,
    discovering: bool,
    /// The CRDs changed while discovery ran: run it again when it finishes.
    rediscover: bool,
    /// The CRD list as last seen, so a changed list triggers a refresh.
    crd_fingerprint: Option<u64>,
    /// Kinds the CRD list defines, and kinds it defined before the last change.
    crds: HashSet<KindId>,
    removed_crds: HashSet<KindId>,
    discovery_retries: u32,
    /// Discoveries that failed in a row, for the retry backoff.
    discovery_failures: u32,
    watches: ClusterWatches,
    /// The telemetry feed on this slot's connection ([`ClusterManager::sync_telemetry`]).
    telemetry: Option<telemetry::SlotFeed>,
    /// Follows the CRD list while the cluster is active.
    crd_task: Option<JoinHandle<()>>,
    /// Follows the namespace list while the cluster is active, for namespace-limited lists.
    namespaces_task: Option<JoinHandle<()>>,
    /// The front end's namespace filter (empty: all namespaces).
    namespace_filter: Vec<String>,
    /// Namespaces the user can list, as last seen.
    listed_namespaces: Vec<String>,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.access.revoke();
        for task in [self.crd_task.take(), self.namespaces_task.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        if let Some(attempt) = self.connect_task.take() {
            attempt.abort();
        }
    }
}

impl Cluster {
    fn new(
        info: ContextInfo,
        fingerprint: Option<u64>,
        hubs: &HubRegistry,
        config: &Arc<EngineConfig>,
    ) -> Self {
        let watches = ClusterWatches::new(info.name.clone(), hubs.clone(), config.clone());
        Self {
            info,
            fingerprint,
            state: ConnectionState::Disconnected,
            client: None,
            write_credential: None,
            attempt: 0,
            connect_task: None,
            access: CredentialAccess::new(Arc::new(ForegroundOnly), Role::Foreground),
            attempt_lease: None,
            member_attempt: None,
            rejected_attempt: None,
            transient_attempt: None,
            awaits_selection: false,
            reconnect: false,
            generation: 0,
            discovery: None,
            discovering: false,
            rediscover: false,
            crd_fingerprint: None,
            crds: HashSet::new(),
            removed_crds: HashSet::new(),
            discovery_retries: 0,
            discovery_failures: 0,
            watches,
            telemetry: None,
            crd_task: None,
            namespaces_task: None,
            namespace_filter: Vec::new(),
            listed_namespaces: Vec::new(),
        }
    }

    /// Ends the slot's connection and its permission to run a credential command.
    fn clear_connection(&mut self) {
        self.telemetry = None;
        self.watches.set_source(None);
        self.client = None;
        self.write_credential = None;
        self.access.revoke();
    }

    /// Where namespace-limited lists look (see [`Command::SetNamespaceFilter`]).
    fn namespace_scope(&self) -> Vec<String> {
        if !self.namespace_filter.is_empty() {
            self.namespace_filter.clone()
        } else if let Some(ns) = self.info.namespace.as_ref().filter(|n| !n.is_empty()) {
            vec![ns.clone()]
        } else {
            self.listed_namespaces.clone()
        }
    }

    fn update_scope(&mut self) {
        let scope = self.namespace_scope();
        self.watches.set_scope(scope);
    }

    fn summary(&self, history: &ContextHistory, provisional: bool, paused: bool) -> ClusterSummary {
        let shown = history.shown(&self.info.name, self.info.server.as_deref());
        ClusterSummary {
            info: self.info.clone(),
            state: self.state.clone(),
            generation: self.generation,
            definition: shown.definition,
            endpoint: shown.endpoint,
            confirmed: shown.confirmed,
            provisional,
            paused,
        }
    }
}

/// The pause before reading again files that were still changing when a read gave up.
const READ_AGAIN_AFTER: Duration = Duration::from_millis(250);

/// How many such reads follow one another before waiting for another reason to read.
const READ_AGAIN_LIMIT: u8 = 8;

/// What the order of kubeconfig reads allows.
///
/// Every read takes the next number from [`ClusterManager::read_seq`] before it starts, so a read
/// numbered before a change may not have seen it. One rule follows: **a result counts only if its
/// read was numbered after the last known change and, while writes are held, not before the hold.**
/// A result that does not count grants no write and ends no hold. Results can also arrive out of
/// the order their reads started in ([`FileRead::order`]), and one that arrives after a later
/// read's result counts for nothing either: that later read may have seen a change.
#[derive(Debug, Default)]
struct ReadMarks {
    /// The newest read numbered before the last known change of the files.
    stale_through: u64,
    /// While writes are held: the newest read numbered when the hold was set.
    held_from: Option<u64>,
    /// A follow-up read of files that would not hold still is scheduled.
    retry_armed: bool,
    /// The follow-up reads since a read last applied or the files last changed.
    retries: u8,
    /// The latest-started read whose result has come in ([`FileRead::order`]).
    newest_seen: u64,
}

impl ReadMarks {
    /// The files changed, or may have, after every read up to `newest` was numbered.
    fn changed(&mut self, newest: u64) {
        self.stale_through = newest;
        self.retries = 0;
    }

    /// A reload could not start, so every read up to `newest` may predate what it was for. This
    /// is no new change: the follow-up reads keep counting.
    fn overtaken(&mut self, newest: u64) {
        self.stale_through = newest;
    }

    /// Records that the result of the read numbered `order` came in, and says whether no read
    /// that started later came in before it.
    fn saw(&mut self, order: u64) -> bool {
        let in_order = order >= self.newest_seen;
        self.newest_seen = self.newest_seen.max(order);
        in_order
    }

    /// Whether read `seq` started after the last known change, so it can answer a write.
    fn current(&self, seq: u64) -> bool {
        seq > self.stale_through
    }

    /// Whether the result of read `seq` may become the state, with `applied` the newest applied.
    fn may_apply(&self, seq: u64, applied: u64) -> bool {
        seq > applied && self.current(seq) && self.held_from.is_none_or(|from| seq >= from)
    }

    /// Whether another follow-up read may be scheduled, and counts it.
    fn arm_retry(&mut self) -> bool {
        if self.retry_armed || self.retries >= READ_AGAIN_LIMIT {
            return false;
        }
        self.retry_armed = true;
        self.retries += 1;
        true
    }

    /// A read was applied, so no hold is left and the follow-up reads start over.
    fn applied(&mut self) {
        self.held_from = None;
        self.retries = 0;
    }
}

/// Why a kubeconfig read runs, which decides what it means if another read holds the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadFor {
    /// The files changed or may have, so a read already running may predate that.
    Change,
    /// Only to catch up after an out-of-order result. No change is known, so reads running stay
    /// valid and this waits for the reader to be idle.
    Refresh,
}

enum Internal {
    /// Reload kubeconfig, then say so ([`crate::CoreHandle::reload_kubeconfig`]).
    ReloadKubeconfig {
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    Connected {
        cluster: ClusterId,
        attempt: u64,
        result: Result<(cluster::GatedConnection, ServerVersion), cluster::SafeErrorText>,
    },
    /// Sent just before the [`Internal::Connected`] of an attempt whose credential was refused.
    AttemptRejected {
        cluster: ClusterId,
        attempt: u64,
    },
    /// Sent just before the [`Internal::Connected`] of an attempt that failed in a way time can
    /// fix.
    AttemptTransient {
        cluster: ClusterId,
        attempt: u64,
    },
    /// The selected cluster's retry timer `timer` ran out.
    SelectedRetryDue {
        timer: u64,
    },
    /// Time to look at the running set again.
    MemberTick,
    /// A kept cluster's background connection finished. `Err` says whether it needs a sign-in.
    KeepWatchConnected {
        cluster: ClusterId,
        attempt: u64,
        result: Result<Client, bool>,
    },
    /// A kept cluster's credential failed or was refused.
    KeepWatchFailed {
        cluster: ClusterId,
        attempt: u64,
    },
    /// Time to connect kept clusters that failed for a reason worth retrying.
    KeepWatchRetry,
    FilesChanged(Changed),
    /// The file watches changed and now cover these directories. Anything may have been written
    /// before they started, so everything is read again.
    WatchesStarted(HashSet<PathBuf>),
    /// A kubeconfig read finished after its caller stopped waiting.
    LateRead {
        paths: Vec<PathBuf>,
        /// Which read it was ([`ClusterManager::read_seq`]).
        seq: u64,
        read: Box<FileRead>,
    },
    /// Time to look again at files that were found emptied.
    SettleEmptied,
    /// Time to read again files that were still changing when a read gave up.
    ReadAgain,
    /// The read of the kubeconfig files made for one write check, started after it was asked.
    WriteRead {
        cluster: ClusterId,
        pin: ContextPin,
        paths: Vec<PathBuf>,
        /// Which read it was ([`ClusterManager::read_seq`]).
        seq: u64,
        read: Option<Box<FileRead>>,
        /// When the check has to be answered by, a second read included.
        deadline: tokio::time::Instant,
        reply: oneshot::Sender<Result<(), WriteError>>,
        /// Counts this check as still to report ([`ClusterManager::write_checks`]).
        check: Arc<()>,
    },
    Discovered {
        cluster: ClusterId,
        attempt: u64,
        result: Result<read::DiscoveredKinds, String>,
    },
    CrdsChanged {
        cluster: ClusterId,
        /// The slot whose watch listed them ([`Cluster::generation`]).
        generation: u64,
        fingerprint: u64,
        kinds: HashSet<KindId>,
    },
    RetryDiscovery {
        cluster: ClusterId,
    },
    NamespacesListed {
        cluster: ClusterId,
        /// The slot whose watch listed them ([`Cluster::generation`]).
        generation: u64,
        names: Vec<String>,
    },
    /// A follower stopped: forget its hub unless a watch still feeds it.
    ReleaseHub {
        cluster: ClusterId,
        kind: KindId,
    },
    /// A log session (re)connects and needs the cluster's current client.
    LogClient {
        cluster: ClusterId,
        /// The session's context as it was when the session opened.
        pin: ContextPin,
        reply: oneshot::Sender<Result<Client, LogError>>,
    },
}

/// A list of pinned clusters in `settings.toml` ([`crate::settings::ClusterPin`]).
#[derive(Debug, Clone)]
enum Pins {
    /// `ai_exposed_clusters` (ADR-0009), with what agents may read (ADR-0013).
    AiExposure(AiScope),
    /// `trusted_clusters` (ADR-0015).
    Trust,
}

impl Pins {
    /// What the list is, in errors.
    fn what(&self) -> &'static str {
        match self {
            Pins::AiExposure(_) => "exposure",
            Pins::Trust => "trust",
        }
    }

    /// What pinning a cluster did, in errors.
    fn done(&self) -> &'static str {
        match self {
            Pins::AiExposure(_) => "exposed",
            Pins::Trust => "trusted",
        }
    }
}

/// Whether `to` could let agents read anything `from` does not. A scope that is not in its clean
/// form ([`AiScope::new`]) can't be compared, so it counts as widening.
///
/// Under a namespace scope with no kinds, cluster-scoped kinds are closed, and naming any kind
/// could open one: a kind id alone does not say whether it is cluster-scoped, so that counts too.
fn scope_widens(from: &AiScope, to: &AiScope) -> bool {
    let clean = |s: &AiScope| *s == AiScope::new(s.namespaces.clone(), s.kinds.clone());
    if !clean(from) || !clean(to) {
        return true;
    }
    let within = |to: &[String], from: &[String]| {
        from.is_empty() || (!to.is_empty() && to.iter().all(|name| from.contains(name)))
    };
    let opens_cluster_kinds =
        !from.namespaces.is_empty() && from.kinds.is_empty() && !to.kinds.is_empty();
    !within(&to.namespaces, &from.namespaces)
        || !within(&to.kinds, &from.kinds)
        || opens_cluster_kinds
}

/// Which watched files changed.
#[derive(Debug, Clone, Copy, Default)]
struct Changed {
    kubeconfig: bool,
    settings: bool,
    /// The agent activity log (UP-16).
    activity: bool,
    /// A missing directory of a watched file was created: watch it now (UP-02: `~/.kube`
    /// created after launch).
    rearm: bool,
}

pub(crate) struct ClusterManager {
    config: CoreConfig,
    events: crate::events::EventSender,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: Option<mpsc::UnboundedReceiver<Internal>>,
    kubeconfig: Arc<LoadedKubeconfig>,
    /// The last slot generation handed out ([`Cluster::generation`]).
    generation: u64,
    /// Every context's history across kubeconfig loads, which sessions are pinned to.
    contexts: ContextHistory,
    /// The kubeconfig files for the current settings: the base files plus `kubeconfig_paths`.
    kubeconfig_paths: Vec<PathBuf>,
    /// Reads those files without letting a stuck one hold up this loop.
    reader: KubeconfigReader,
    /// The files whose read is stuck, once that was applied and reported.
    stalled_on: Option<Vec<PathBuf>>,
    /// The number of reads started, which orders their results.
    read_seq: u64,
    /// The newest read whose result was applied.
    applied_read: u64,
    /// A reload was asked for while a read was running, so that read may predate the reason.
    reread: bool,
    /// What is queued in `reread` is only a [`ReadFor::Refresh`].
    refresh_only: bool,
    /// Cloned into each write check until its result is handled, so the count says how many are
    /// still to come.
    write_checks: Arc<()>,
    /// Which reads count ([`ReadMarks`]).
    marks: ReadMarks,
    /// The files found emptied and still waiting to count as really empty.
    emptied: EmptiedFiles,
    /// How long a file must stay emptied, unwritten, before it counts as really empty.
    empty_settle: Duration,
    /// A follow-up read of emptied files is scheduled.
    settle_armed: bool,
    settings: Arc<Settings>,
    settings_error: Option<String>,
    /// The newest Trust or Stop Trusting request whose write has finished.
    trust_settled: u64,
    /// In kubeconfig order.
    clusters: Vec<Cluster>,
    active: Option<ClusterId>,
    /// Clusters kept running beside the active one ([`Command::SetRunningSet`]).
    members: HashMap<ClusterId, Member>,
    /// When members' background connection attempts may start.
    schedule: Schedule<ClusterId>,
    /// Automatic retries of the selected cluster.
    retries: selected_retry::RetryDriver,
    /// Runs while the set has members.
    set_ticker: Option<JoinHandle<()>>,
    /// Source of connection attempt ids; never reused.
    next_attempt: u64,
    watcher: Option<FileWatcher>,
    hubs: HubRegistry,
    engine_config: Arc<EngineConfig>,
    /// Open subscriptions by id, each with the member it was admitted on if a set opened it.
    subscriptions: HashMap<u64, (ClusterId, KindId, Option<crate::handle::AdmittedMember>)>,
    /// Keeps each member subscription's channel open after it is cut off or refused, so it
    /// reads as empty rather than as a stopped core.
    member_feeds: HashMap<u64, crate::watch::MemberFeed>,
    /// Open overviews by id, with the slot generation each was opened on.
    overviews: HashMap<u64, telemetry::OpenOverview>,
    /// Running log sessions by id. They belong to no cluster slot, so they outlive cluster
    /// switches and kubeconfig reloads (UP-42).
    log_sessions: HashMap<u64, JoinHandle<()>>,
    /// The memory budget all log sessions share (`log_total_mib`).
    log_budget: Arc<logs::LogBudget>,
    /// The running telemetry feeds, as the front end reads them.
    telemetry: crate::telemetry::TelemetryBoard,
    telemetry_timing: crate::telemetry::TelemetryTiming,
    /// The clusters the user unlocked for this session, shared with the front end (ADR-0015).
    unlocks: SessionUnlocks,
    /// Contexts the last load did not show, listed only because a file that defined them can't
    /// be parsed. Their connections and watches go on, for reading only.
    held: HashSet<ClusterId>,
    /// Each context's definition when a load last showed it with no unreadable file that could
    /// change it.
    settled: SettledDefinitions,
    /// What `unsettled` and `paused` say now, shared with the front end. Only shown: the gate
    /// never reads it.
    holds: DefinitionHolds,
    /// Clusters watched in the background, on connections of their own.
    keep: keep_watching::KeepWatch,
    forwards: forwards::Forwards,
}

impl ClusterManager {
    /// The log sessions' shared budget, for [`crate::LogSession::set_shown`].
    pub(crate) fn log_budget(&self) -> Arc<logs::LogBudget> {
        self.log_budget.clone()
    }

    /// The running telemetry feeds, for [`crate::CoreHandle::telemetry`].
    pub(crate) fn telemetry_board(&self) -> crate::telemetry::TelemetryBoard {
        self.telemetry.clone()
    }

    /// The published holds, for [`crate::CoreHandle::definition_holds`].
    pub(crate) fn definition_holds(&self) -> DefinitionHolds {
        self.holds.clone()
    }

    pub(crate) fn new(
        config: CoreConfig,
        events: crate::events::EventSender,
        hubs: HubRegistry,
        unlocks: SessionUnlocks,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let kubeconfig_paths = kubeconfig::paths_for_settings(
            &config.kubeconfig_base,
            &config.settings,
            config.home.as_deref(),
        );
        let engine_config = Arc::new(EngineConfig::with_timing(&config.watch_timing));
        let log_budget = logs::LogBudget::new(config.settings.log_total_bytes());
        Self {
            settings: Arc::new(config.settings.clone()),
            settings_error: config.settings_error.clone(),
            trust_settled: 0,
            config,
            events,
            internal_tx,
            internal_rx: Some(internal_rx),
            kubeconfig: Arc::default(),
            generation: 0,
            contexts: ContextHistory::default(),
            kubeconfig_paths,
            clusters: Vec::new(),
            active: None,
            members: HashMap::new(),
            schedule: Schedule::default(),
            retries: Default::default(),
            set_ticker: None,
            next_attempt: 1,
            watcher: None,
            hubs,
            engine_config,
            subscriptions: HashMap::new(),
            member_feeds: HashMap::new(),
            overviews: HashMap::new(),
            log_sessions: HashMap::new(),
            log_budget,
            telemetry: Default::default(),
            telemetry_timing: Default::default(),
            unlocks,
            held: HashSet::new(),
            settled: SettledDefinitions::default(),
            holds: DefinitionHolds::default(),
            keep: Default::default(),
            forwards: Default::default(),
            reader: KubeconfigReader::default(),
            stalled_on: None,
            read_seq: 0,
            applied_read: 0,
            reread: false,
            refresh_only: false,
            write_checks: Arc::default(),
            marks: ReadMarks::default(),
            emptied: EmptiedFiles::default(),
            empty_settle: EMPTY_SETTLE,
            settle_armed: false,
        }
    }

    pub(crate) async fn run(
        mut self,
        commands: async_channel::Receiver<Command>,
        requests: async_channel::Receiver<Request>,
    ) {
        let mut internal_rx = self.internal_rx.take().expect("run is called once");
        self.emit_settings();
        // Watching first, so a change after the first reads (an agent's call landing meanwhile)
        // is noticed.
        self.arm_watcher();
        self.reload_activity().await;
        self.reload_kubeconfig().await;
        self.sync_keep_watching();
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Ok(command) => self.handle_command(command).await,
                    // Every CoreHandle is gone.
                    Err(_) => break,
                },
                Ok(request) = requests.recv() => self.handle_request(request),
                Some(msg) = internal_rx.recv() => self.handle_internal(msg).await,
            }
            self.sync_keep_watching();
            self.sync_forwards();
            self.sync_selected_retry();
            self.sync_telemetry();
        }
    }

    fn emit(&self, event: Event) {
        self.events.send(event);
    }

    fn emit_settings(&self) {
        self.emit(Event::SettingsChanged {
            settings: self.settings.clone(),
            error: self.settings_error.clone(),
            trust_settled: self.trust_settled,
        });
    }

    async fn handle_command(&mut self, command: Command) {
        match command {
            Command::Activate(id) => self.activate(id),
            Command::CancelConnection {
                cluster,
                generation,
            } => self.cancel_connection(&cluster, generation),
            Command::SetRunningSet(ids) => self.set_running_set(ids),
            Command::SetKeepWatching {
                cluster,
                definition,
            } => self.set_keep_watching(cluster, definition).await,
            Command::ApproveKeepWatching(approval) => self.approve_keep_watching(approval).await,
            Command::RevokeBackgroundSignIn(cluster) => self.revoke_background_sign_in(&cluster),
            Command::SetAiExposed {
                cluster,
                server,
                scope,
            } => {
                self.set_pin(Pins::AiExposure(scope), cluster, server).await;
            }
            Command::SetAiScope {
                cluster,
                server,
                from,
                scope,
            } => self.set_ai_scope(cluster, server, from, scope).await,
            Command::SetTrusted {
                cluster,
                server,
                request,
            } => self.set_trusted(cluster, server, request).await,
            Command::ReloadKubeconfig => {
                self.reload_kubeconfig().await;
            }
            Command::SetWatchMode { cluster, mode } => {
                self.write_settings("the watch mode", move |dir| {
                    settings::set_watch_mode(dir, cluster.as_deref(), mode)
                })
                .await
            }
            Command::SetClusterName { cluster, name } => {
                self.write_settings("the cluster name", move |dir| {
                    settings::set_cluster_name(dir, &cluster, name.as_deref())
                })
                .await
            }
            Command::SetClusterTag {
                cluster,
                server,
                tag,
            } => {
                self.write_settings("the cluster tag", move |dir| {
                    settings::set_cluster_tag(dir, &cluster, &server, tag)
                })
                .await
            }
            Command::SetAppearance(appearance) => {
                self.write_settings("the appearance", move |dir| {
                    settings::set_appearance(dir, appearance)
                })
                .await
            }
            Command::SetDesktopNotifications(enabled) => {
                self.write_settings("desktop notifications", move |dir| {
                    settings::set_desktop_notifications(dir, enabled)
                })
                .await
            }
            Command::SetClusterAlerts {
                cluster,
                server,
                enabled,
            } => {
                self.write_settings("the cluster's alerts", move |dir| {
                    settings::set_cluster_alerts(dir, &cluster, &server, enabled)
                })
                .await
            }
            Command::SetNamespaceFilter {
                cluster,
                namespaces,
            } => {
                if let Some(slot) = self.cluster_mut(&cluster) {
                    slot.namespace_filter = namespaces;
                }
                self.apply_scope(&cluster);
            }
            Command::SetLogBufferLines(lines) => {
                self.write_settings("the log buffer size", move |dir| {
                    settings::set_log_buffer_lines(dir, lines)
                })
                .await
            }
            Command::SetLogBufferMib(mib) => {
                self.write_settings("the log buffer memory", move |dir| {
                    settings::set_log_buffer_mib(dir, mib)
                })
                .await
            }
        }
    }

    fn handle_request(&mut self, request: Request) {
        match request {
            Request::RunningSet { reply } => {
                let _ = reply.send(self.member_reports());
            }
            Request::KeepWatching { reply } => {
                let _ = reply.send(self.keep_watching_views());
            }
            Request::OfferKeepWatching {
                cluster,
                shown,
                reply,
            } => {
                let _ = reply.send(self.offer_keep_watching(&cluster, shown));
            }
            // Requests are handled without waiting: the reload runs as an internal message.
            Request::ReloadKubeconfig { reply } => {
                let _ = self.internal_tx.send(Internal::ReloadKubeconfig { reply });
            }
            Request::ActivatePinned {
                cluster,
                pin,
                generation,
                reply,
            } => {
                let _ = reply.send(self.activate_pinned(cluster, &pin, generation));
            }
            Request::Subscribe { id, cluster, kind } => {
                self.subscriptions.insert(id, (cluster.clone(), kind, None));
                self.reconcile(&cluster);
            }
            Request::SubscribeMember {
                id,
                cluster,
                member,
                kind,
                feed,
            } => {
                self.subscribe_member(id, &cluster, member, kind, feed);
            }
            Request::Unsubscribe { id } => {
                self.member_feeds.remove(&id);
                if let Some((cluster, kind, member)) = self.subscriptions.remove(&id) {
                    if member.is_some() {
                        self.hubs.remove_member(&cluster, &kind, id);
                    }
                    self.reconcile(&cluster);
                    self.release_hub_if_unwatched(&cluster, &kind);
                }
            }
            Request::OpenOverview { id, cluster } => self.open_overview(id, cluster),
            Request::OpenOverviewOn {
                id,
                cluster,
                generation,
                live,
            } => self.open_overview_on(id, cluster, generation, live),
            Request::CloseOverview { id } => self.close_overview(id),
            Request::Discovery { cluster, reply } => {
                let discovery = self
                    .clusters
                    .iter()
                    .find(|c| c.info.name == cluster)
                    .and_then(|c| c.discovery.clone());
                let _ = reply.send(discovery);
            }
            Request::WatchedKinds { cluster, reply } => {
                let _ = reply.send(self.watched_kinds(&cluster));
            }
            Request::OpenLogs {
                id,
                cluster,
                server,
                member,
                subject,
                options,
                snapshots,
                paused,
                budget,
            } => {
                let internal = self.internal_tx.clone();
                // Pinned now, to the server shown (the tab's): a reconnect must never read logs
                // from another server (UP-06). Opened on a fallback, the pin is stale and the
                // first client lookup ends the session, rather than read from the server the
                // context's confirmed definition names once its file loads again.
                // Given the tab's server, pinned to exactly that: a request taken after the
                // context moved (its fallback gone, say) is not pinned to where it is now.
                let pin = match (&member, &server) {
                    (Some(member), _) => member.log_pin(&cluster),
                    (None, Some(server)) => self.contexts.pin(&cluster, server.as_deref()),
                    (None, None) => self.session_pin(&cluster),
                };
                let lookup: logs::ClientLookup = Arc::new(move || {
                    let (reply, rx) = oneshot::channel();
                    let _ = internal.send(Internal::LogClient {
                        cluster: cluster.clone(),
                        pin: pin.clone(),
                        reply,
                    });
                    Box::pin(async move {
                        rx.await
                            .unwrap_or_else(|_| Err(LogError::fatal("the core stopped")))
                    })
                });
                let config = SessionConfig::new(
                    self.settings.log_buffer_limit(),
                    self.settings.log_buffer_bytes(),
                );
                let feed = logs::feed(
                    subject,
                    options,
                    &config,
                    Arc::new(KubeLogSource::new(lookup.clone())),
                    Arc::new(KubePodSource::new(lookup)),
                );
                let task = tokio::spawn(logs::run(feed, config, snapshots, paused, Some(budget)));
                self.log_sessions.retain(|_, t| !t.is_finished());
                self.log_sessions.insert(id, task);
            }
            Request::CloseLogs { id } => {
                if let Some(task) = self.log_sessions.remove(&id) {
                    task.abort();
                }
            }
            Request::Client { cluster, reply } => {
                let _ = reply.send(self.client(&cluster));
            }
            Request::WriteClient {
                cluster,
                server,
                reply,
            } => {
                let _ = reply.send(self.shell_client(&cluster, server));
            }
            Request::BoundWriteClient {
                cluster,
                server,
                reply,
            } => {
                let _ = reply.send(self.bound_write_client(&cluster, server));
            }
            Request::CheckWriteGate {
                cluster,
                pin,
                reply,
            } => {
                let _ = reply.send(self.launch_gate(&cluster, &pin));
            }
            Request::CheckWritePin {
                cluster,
                pin,
                reply,
            } => {
                self.check_write_pin(cluster, pin, reply);
            }
            Request::AdmitForward {
                cluster,
                server,
                admitted,
                reply,
            } => {
                let _ = reply.send(self.admit_forward(cluster, server, admitted.as_ref()));
            }
            Request::CheckForward { id, reply } => {
                let _ = reply.send(self.check_forward(id));
            }
            Request::CheckContext {
                cluster,
                pin,
                reply,
            } => {
                let _ = reply.send(self.contexts.check(Pinned::Shell, &cluster, &pin));
            }
            Request::AdmitRunningSet { pins, reply } => {
                let _ = reply.send(self.admit_running_set(pins));
            }
            Request::CheckMember { member, reply } => {
                let _ = reply.send(self.member_changed(&member));
            }
            Request::MemberClient { member, reply } => {
                let _ = reply.send(self.member_client(&member));
            }
            Request::FetchObject {
                cluster,
                kind,
                namespace,
                name,
                member,
                reply,
            } => {
                // Before any client is taken: a row's read never reaches a replacement server.
                if let Some(why) = member.as_ref().and_then(|m| self.member_changed(m)) {
                    let _ = reply.send(Err(why));
                    return;
                }
                let Some(client) = self.client(&cluster) else {
                    let _ = reply.send(Err(format!("\"{cluster}\" is not connected")));
                    return;
                };
                let info = self
                    .cluster(&cluster)
                    .and_then(|c| c.discovery.as_ref())
                    .and_then(|d| d.kind(&kind).cloned());
                let Some(info) = info else {
                    let _ = reply.send(Err(format!("\"{cluster}\" does not serve {kind}")));
                    return;
                };
                let target = cluster::Target::of(&self.kubeconfig.merged, &cluster);
                tokio::spawn(async move {
                    let mut reply = reply;
                    let read = read::object_detail_at(
                        &client,
                        &info,
                        namespace.as_deref(),
                        &name,
                        &target,
                        read::DETAIL_TIMEOUT,
                    );
                    // A closed pane drops the receiver: stop the read (and its HTTP requests)
                    // rather than keep the object alive until the server answers. Checked
                    // first, so a pane already gone starts no request.
                    tokio::select! {
                        biased;
                        () = reply.closed() => {}
                        result = read => {
                            let result = result
                                .map(Arc::new)
                                .map_err(|e| e.for_target(&target).summary().to_string());
                            let _ = reply.send(result);
                        }
                    }
                });
            }
            Request::FetchPinnedObject {
                cluster,
                pin,
                kind,
                namespace,
                name,
                reply,
            } => {
                // In one step with taking the client, which a paused cluster doesn't give.
                if let Some(why) = self.read_pin_changed(&cluster, &pin) {
                    let _ = reply.send(Err(why));
                    return;
                }
                let Some(client) = self.client(&cluster) else {
                    let _ = reply.send(Err(format!("\"{cluster}\" is not connected")));
                    return;
                };
                let info = self
                    .cluster(&cluster)
                    .and_then(|c| c.discovery.as_ref())
                    .and_then(|d| d.kind(&kind).cloned());
                let Some(info) = info else {
                    let _ = reply.send(Err(format!("\"{cluster}\" does not serve {kind}")));
                    return;
                };
                let target = cluster::Target::of(&self.kubeconfig.merged, &cluster);
                tokio::spawn(async move {
                    let mut reply = reply;
                    let read = read::object_only(
                        &client,
                        &info,
                        namespace.as_deref(),
                        &name,
                        read::DETAIL_TIMEOUT,
                    );
                    // Whoever asked is gone: stop the request, and start none if it is already.
                    tokio::select! {
                        biased;
                        () = reply.closed() => {}
                        result = read => {
                            let result = result
                                .map(Arc::new)
                                .map_err(|e| e.for_target(&target).summary().to_string());
                            let _ = reply.send(result);
                        }
                    }
                });
            }
        }
    }

    /// Why `cluster` is no longer what `pin` was taken from, if so.
    fn read_pin_changed(&self, cluster: &str, pin: &crate::handle::ReadPin) -> Option<String> {
        let slot = self.cluster(cluster)?;
        let server = slot.info.server.as_deref();
        let same = server == pin.server.as_deref()
            && self.contexts.shown(cluster, server).definition == pin.definition;
        (!same).then(|| format!("\"{cluster}\" changed since this view opened."))
    }

    fn watched_kinds(&self, cluster: &str) -> Vec<(KindId, crate::store::Feed)> {
        let mut kinds: Vec<_> = self
            .clusters
            .iter()
            .find(|c| c.info.name == cluster)
            .map(|c| c.watches.running().into_iter().collect())
            .unwrap_or_default();
        kinds.sort();
        kinds
    }

    /// Whether `id` should run watches and follow its CRDs: the active cluster, or a member while
    /// its slot still shows the server it joined for.
    fn wants_watches(&self, id: &str) -> bool {
        self.active.as_deref() == Some(id) || self.member_on_its_server(id)
    }

    /// Whether `id` is a member and its slot shows the server the member is bound to.
    fn member_on_its_server(&self, id: &str) -> bool {
        self.members
            .get(id)
            .zip(self.cluster(id))
            .is_some_and(|(member, slot)| {
                slot.info
                    .server
                    .as_deref()
                    .is_some_and(|server| ServerIdentity::new(server) == *member.server())
            })
    }

    /// Brings a cluster's watches in line with whether it wants them, its watch mode, and the
    /// open subscriptions, and follows its CRDs while it wants them.
    fn reconcile(&mut self, id: &str) {
        // Before anything below can start a watch on the context's current server.
        self.cut_off_changed_members(Some(id));
        let wanted = self.wants_watches(id);
        let mode = self.settings.watch_mode_for(id);
        let subscribed: HashSet<KindId> = self
            .subscriptions
            .values()
            .filter(|(c, _, member)| {
                // A member's subscription asks for nothing once its context left that server.
                c == id
                    && member
                        .as_ref()
                        .is_none_or(|m| self.member_changed(m).is_none())
            })
            .map(|(_, k, _)| k.clone())
            .collect();
        let hubs = self.hubs.clone();
        let internal = self.internal_tx.clone();
        let paused = self.paused(id);
        let background = self.active.as_deref() != Some(id);
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        slot.watches.set_background(background);
        let follow = wanted && slot.discovery.is_some() && slot.client.is_some();
        if !follow {
            // Before the watches stop, so the followers' hubs can be released (each follower
            // asks for that when it is gone; see `Follower`).
            for task in [slot.crd_task.take(), slot.namespaces_task.take()]
                .into_iter()
                .flatten()
            {
                task.abort();
            }
        }
        // Paused, what runs goes on and nothing new starts; the load that ends it catches up.
        if paused {
            slot.watches.stop_unwanted(wanted, mode, &subscribed);
            return;
        }
        slot.watches.reconcile(wanted, mode, &subscribed);
        if follow && slot.crd_task.is_none() {
            let follower =
                Follower::new(&hubs, id, KindId::custom_resource_definitions(), &internal);
            let generation = slot.generation;
            slot.crd_task = Some(tokio::spawn(follow_crds(
                follower,
                generation,
                internal.clone(),
            )));
        }
        if follow && slot.namespaces_task.is_none() {
            let follower = Follower::new(&hubs, id, KindId::namespaces(), &internal);
            let generation = slot.generation;
            slot.namespaces_task = Some(tokio::spawn(follow_namespaces(
                follower, generation, internal,
            )));
        }
    }

    fn reconcile_all(&mut self) {
        let ids: Vec<String> = self.clusters.iter().map(|c| c.info.name.clone()).collect();
        for id in ids {
            self.reconcile(&id);
        }
    }

    /// Runs discovery for a connected cluster unless it is already running.
    fn start_discovery(&mut self, id: &str) {
        let internal = self.internal_tx.clone();
        let target = cluster::Target::of(&self.kubeconfig.merged, id);
        let paused = self.paused(id);
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        let Some(client) = slot.client.as_ref().map(|c| c.read.clone()) else {
            return;
        };
        if paused {
            slot.rediscover = true;
            return;
        }
        if slot.discovering {
            slot.rediscover = true;
            return;
        }
        slot.discovering = true;
        slot.rediscover = false;
        let attempt = slot.attempt;
        let cluster = id.to_owned();
        tokio::spawn(async move {
            let result = read::discover_kinds(&client)
                .await
                .map_err(|e| e.for_target(&target).summary().to_string());
            let _ = internal.send(Internal::Discovered {
                cluster,
                attempt,
                result,
            });
        });
    }

    /// Writes one change to `settings.toml` (`what` names it in errors) and applies the result.
    async fn write_settings(
        &mut self,
        what: &str,
        write: impl FnOnce(&Path) -> Result<Settings, settings::SettingsError> + Send + 'static,
    ) {
        if let Some(error) = &self.settings_error {
            self.emit(Event::Error(format!(
                "settings.toml must be fixed before {what} can change: {}",
                crate::redact::safe_error_summary(error)
            )));
            return;
        }
        let dir = self.config.config_dir.clone();
        let result = tokio::task::spawn_blocking(move || write(&dir)).await;
        match result {
            Ok(Ok(settings)) => self.apply_settings(Ok(settings)).await,
            // A settings write error can quote the file: one sanitized line.
            Ok(Err(e)) => self.emit(Event::Error(crate::redact::safe_error_summary(
                &e.to_string(),
            ))),
            Err(e) => self.emit(Event::Error(format!("settings write failed: {e}"))),
        }
    }

    /// Trusts `cluster` at `server` (`Some`) or stops trusting it (`None`): request `request`,
    /// numbered when it was made ([`crate::protection::SessionUnlocks`]). A Trust reports how
    /// its write went, which lifts an earlier Stop Trusting only if no request was made since.
    async fn set_trusted(&mut self, cluster: ClusterId, server: Option<String>, request: u64) {
        let trusting = server.is_some();
        let saved = self.set_pin(Pins::Trust, cluster.clone(), server).await;
        if trusting {
            self.unlocks.trust_written(&cluster, request, saved);
        } else if saved {
            self.unlocks.trust_removed(&cluster, request);
        }
        self.trust_settled = self.trust_settled.max(request);
        if saved {
            // The settings may be unchanged (the same pin again, or none to remove): say it
            // anyway, so the front end shows the protection that holds now.
            self.emit_settings();
        }
    }

    /// Changes what agents may read of `cluster`, exposed at `server`, to `scope`; never
    /// exposes it ([`settings::set_ai_scope`]).
    async fn set_ai_scope(
        &mut self,
        cluster: ClusterId,
        server: String,
        from: AiScope,
        scope: AiScope,
    ) {
        // Until a load settles the definition, a scope can only shrink.
        if self.unsettled(&cluster) && scope_widens(&from, &scope) {
            self.emit(Event::Error(format!(
                "The AI scope of \"{cluster}\" was not widened: a kubeconfig file that could \
                 define it can't be read. Fix the file and try again."
            )));
            return;
        }
        let current = self
            .clusters
            .iter()
            .find(|c| c.info.name == cluster)
            .and_then(|c| c.info.server.as_deref());
        if current != Some(server.as_str()) {
            self.emit(Event::Error(format!(
                "\"{cluster}\" now points to a different server; its AI scope was not changed. \
                 Review it and try again."
            )));
            return;
        }
        if scope_widens(&from, &scope) && !self.files_still_show(&cluster, &server).await {
            self.emit(Event::Error(format!(
                "The AI scope of \"{cluster}\" was not widened: its kubeconfig files can't be \
                 read or no longer match what is shown. Check them and try again."
            )));
            return;
        }
        self.write_settings("the AI scope", move |dir| {
            settings::set_ai_scope(dir, &cluster, &server, &from, &scope)
        })
        .await;
    }

    /// Pins `cluster` at `server` in a list of pins (`Some`), or removes it (`None`), in
    /// `settings.toml`. Returns whether the change was saved.
    async fn set_pin(&mut self, pins: Pins, cluster: ClusterId, server: Option<String>) -> bool {
        if let Some(server) = &server {
            // No load shows a settled definition, so there is nothing to pin yet.
            if self.unsettled(&cluster) {
                self.emit(Event::Error(format!(
                    "\"{cluster}\" was not {}: a kubeconfig file that could define it can't be \
                     read. Fix the file and try again.",
                    pins.done()
                )));
                return false;
            }
            // The user confirmed this server. If the context moved since, do not pin the new
            // one on the strength of that confirmation.
            let current = self
                .clusters
                .iter()
                .find(|c| c.info.name == cluster)
                .and_then(|c| c.info.server.as_deref());
            if current != Some(server.as_str()) {
                self.emit(Event::Error(format!(
                    "\"{cluster}\" now points to a different server; it was not {}. Review it and try again.",
                    pins.done()
                )));
                return false;
            }
        }
        if let Some(error) = &self.settings_error {
            // Rewriting a file that does not parse would fail anyway; say why.
            self.emit(Event::Error(format!(
                "settings.toml must be fixed before {} can change: {}",
                pins.what(),
                crate::redact::safe_error_summary(error)
            )));
            return false;
        }
        if let Some(at) = &server
            && !self.files_still_show(&cluster, at).await
        {
            self.emit(Event::Error(format!(
                "\"{cluster}\" was not {}: its kubeconfig files can't be read or no longer match \
                 what is shown. Check them and try again.",
                pins.done()
            )));
            return false;
        }
        let dir = self.config.config_dir.clone();
        let result = tokio::task::spawn_blocking(move || match pins {
            Pins::AiExposure(scope) => {
                settings::set_ai_exposed(&dir, &cluster, server.as_deref(), &scope)
            }
            Pins::Trust => settings::set_trusted(&dir, &cluster, server.as_deref()),
        })
        .await;
        match result {
            Ok(Ok(settings)) => {
                self.apply_settings(Ok(settings)).await;
                // The write may have created the config directory.
                self.arm_watcher();
                true
            }
            // A settings write error can quote the file: one sanitized line.
            Ok(Err(e)) => {
                self.emit(Event::Error(crate::redact::safe_error_summary(
                    &e.to_string(),
                )));
                false
            }
            Err(e) => {
                self.emit(Event::Error(format!("settings write failed: {e}")));
                false
            }
        }
    }

    async fn reload_settings(&mut self) {
        let dir = self.config.config_dir.clone();
        match tokio::task::spawn_blocking(move || settings::load_settings(&dir)).await {
            Ok(result) => self.apply_settings(result.map_err(|e| e.to_string())).await,
            Err(e) => self.emit(Event::Error(format!("settings reload failed: {e}"))),
        }
    }

    /// Takes new settings (or the error loading them, which means nothing is exposed), emits them
    /// if they changed, and reloads kubeconfig if the file list changed.
    async fn apply_settings(&mut self, result: Result<Settings, String>) {
        let (settings, error) = match result {
            Ok(s) => (s, None),
            Err(e) => {
                // Expose nothing (as the MCP server does), trust nothing and protect local
                // clusters too (as at startup), but keep the other settings, so a typo does
                // not make clusters from extra kubeconfig files disappear. An entry removed by
                // hand behind the typo must not stay in force.
                tracing::warn!(
                    "settings.toml unreadable; nothing is exposed to AI or trusted, and local \
                     clusters are protected: {}",
                    crate::redact::safe_error_summary(&e)
                );
                let kept = (*self.settings).clone().unreadable_fallback();
                (kept, Some(e))
            }
        };
        if *self.settings == settings && self.settings_error == error {
            return;
        }
        self.settings = Arc::new(settings);
        self.settings_error = error;
        self.log_budget.set_limit(self.settings.log_total_bytes());
        // The watch mode may have changed: start and stop the watches first, so a front end
        // that reads the stores when it hears of the change (the resource switcher) finds
        // them as the new settings have them.
        self.reconcile_all();
        self.emit_settings();
        let paths = kubeconfig::paths_for_settings(
            &self.config.kubeconfig_base,
            &self.settings,
            self.config.home.as_deref(),
        );
        if paths != self.kubeconfig_paths {
            self.retire_plugin_grants();
            self.kubeconfig_paths = paths;
            self.arm_watcher();
            self.reload_kubeconfig().await;
        }
    }

    async fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::ReloadKubeconfig { reply } => {
                self.marks.changed(self.read_seq);
                let applied = self.reload_kubeconfig().await;
                let _ = reply.send(applied);
            }
            Internal::LateRead { paths, seq, read } => {
                let in_order = self.saw_read(&paths, Some(&read));
                let current = in_order
                    && paths == self.kubeconfig_paths
                    && self.marks.may_apply(seq, self.applied_read);
                // A reload asked for since this read began knows of a newer file, and a read
                // that saw its files change is no state they were ever in.
                let superseded = self.change_queued();
                let torn = current && !read.consistent;
                if superseded {
                    self.reload_kubeconfig().await;
                } else if torn {
                    self.hold_stalled(paths);
                    self.read_again_later();
                } else if current {
                    self.take_read(seq, *read);
                } else if !in_order && self.stalled_on.as_ref() == Some(&paths) {
                    // Too old to end the hold, and no other result may be coming.
                    self.read_again_later();
                }
                self.refresh_if_idle().await;
                // A hold from a refused check ends here if this read was not applied.
                if !self.reader.busy(&self.kubeconfig_paths) {
                    self.publish_holds();
                }
            }
            Internal::SettleEmptied => {
                self.settle_armed = false;
                self.reload_kubeconfig().await;
            }
            Internal::ReadAgain => {
                self.marks.retry_armed = false;
                if self.stalled_on.is_some() {
                    self.reload_kubeconfig().await;
                }
            }
            Internal::WriteRead {
                cluster,
                pin,
                paths,
                seq,
                read,
                deadline,
                reply,
                check,
            } => {
                drop(check);
                // A late result already applied leaves nothing to hold for.
                let timed_out =
                    read.is_none() && paths == self.kubeconfig_paths && seq > self.applied_read;
                let in_order = self.saw_read(&paths, read.as_deref());
                // Starting a read beside a running one would count as a missed reload and make
                // every check in flight stale, so an out-of-order result is refused instead.
                let behind_a_read = !in_order && self.reader.busy(&paths);
                // A read can still start between that look and a read from here. An out-of-order
                // result is no news of a change, so finding the reader busy then moves no mark.
                let why = if in_order {
                    ReadFor::Change
                } else {
                    ReadFor::Refresh
                };
                let (answer, differs) = if behind_a_read {
                    (Err(WriteError::KubeconfigUnreadable), false)
                } else if read.is_some() && (!in_order || !self.marks.current(seq)) {
                    let answer = self
                        .revalidate_write(&cluster, &pin, &paths, deadline, why)
                        .await;
                    (answer, false)
                } else {
                    self.fresh_write_gate(&cluster, &pin, &paths, read)
                };
                // Not writable on screen once a check of the files refused: the load comes next.
                if answer.is_err() && (differs || timed_out || behind_a_read) {
                    self.holds.hold_writes(&cluster);
                }
                let _ = reply.send(answer);
                if behind_a_read {
                    // The running read's result comes through this loop and runs the refresh.
                    self.queue_refresh();
                } else if timed_out && self.reader.busy(&paths) {
                    // That read's end comes later, and it may be too old to apply.
                    self.reread = true;
                    self.refresh_only = false;
                    self.hold_stalled(paths);
                } else if timed_out
                    || differs
                    || self.change_queued()
                    || self.stalled_on.as_ref() == Some(&paths)
                {
                    self.read_and_apply(None, why, |_, _| {}).await;
                } else {
                    self.refresh_if_idle().await;
                }
                // A read still running keeps the hold: its load publishes.
                if !self.reader.busy(&self.kubeconfig_paths) {
                    self.publish_holds();
                }
            }
            Internal::Connected {
                cluster,
                attempt,
                result,
            } => {
                self.end_invalid_retry();
                let target = cluster::Target::of(&self.kubeconfig.merged, &cluster);
                let paused = self.paused(&cluster);
                let Some(slot) = self.cluster_mut(&cluster) else {
                    return;
                };
                if slot.attempt != attempt {
                    return;
                }
                slot.connect_task = None;
                let member_attempt = slot.member_attempt.take();
                let rejected = slot.rejected_attempt.take() == Some(attempt);
                let lease_ended = slot
                    .attempt_lease
                    .take()
                    .is_some_and(|lease| !lease.holds());
                // Dropped either way: the load that ends the pause connects again.
                if paused {
                    slot.access.revoke();
                    slot.state = ConnectionState::AwaitingConfirmation;
                    slot.reconnect = true;
                    self.emit(Event::ConnectionChanged {
                        cluster: cluster.clone(),
                        state: ConnectionState::AwaitingConfirmation,
                    });
                    // The pause already voided the member's attempt: this frees its slot.
                    let outcome = AttemptOutcome::Failed(cluster::SafeErrorText::reduced("paused"));
                    self.finish_member_attempt(&cluster, member_attempt, outcome, Instant::now());
                    return;
                }
                // Its lease ended while it was being made (switched away and back), so what it
                // returned, an error included, is no answer about the cluster. It is dropped, and
                // the current selection connects anew under a new grant.
                if lease_ended
                    || result
                        .as_ref()
                        .is_ok_and(|(connection, _)| connection.credential.spent())
                {
                    slot.clear_connection();
                    slot.attempt = 0;
                    slot.discovering = false;
                    slot.reconnect = true;
                    slot.state = ConnectionState::Disconnected;
                    self.emit(Event::ConnectionChanged {
                        cluster: cluster.clone(),
                        state: ConnectionState::Disconnected,
                    });
                    let now = Instant::now();
                    let outcome =
                        AttemptOutcome::Failed(cluster::SafeErrorText::reduced("superseded"));
                    self.finish_member_attempt(&cluster, member_attempt, outcome, now);
                    if self.active.as_deref() == Some(cluster.as_str()) {
                        self.reconnect_active(&cluster);
                    }
                    self.pump_members(now);
                    return;
                }
                let connected = result.is_ok();
                let outcome = match &result {
                    Ok(_) => AttemptOutcome::Connected,
                    Err(error) if rejected => AttemptOutcome::AuthRejected(error.clone()),
                    Err(error) => AttemptOutcome::Failed(error.clone()),
                };
                match result {
                    Ok((connection, version)) => {
                        slot.watches.set_source(Some(KubeSource::new(
                            connection.clients.read.clone(),
                            Some(&version),
                            target,
                        )));
                        slot.client = Some(connection.clients);
                        slot.write_credential = Some(connection.credential);
                        slot.state = ConnectionState::Connected(version);
                    }
                    Err(message) => {
                        slot.clear_connection();
                        slot.state = ConnectionState::Failed(message);
                    }
                }
                let state = slot.state.clone();
                self.emit(Event::ConnectionChanged {
                    cluster: cluster.clone(),
                    state,
                });
                if let Some(slot) = self.cluster_mut(&cluster) {
                    slot.update_scope();
                }
                if connected {
                    // Discovery runs at every connect; until it is back, a cached result (from an
                    // earlier connection) keeps the navigation and watches going.
                    self.start_discovery(&cluster);
                    self.reconcile(&cluster);
                }
                let now = Instant::now();
                self.finish_member_attempt(&cluster, member_attempt, outcome, now);
                self.pump_members(now);
            }
            Internal::AttemptRejected { cluster, attempt } => {
                if let Some(slot) = self.cluster_mut(&cluster)
                    && slot.attempt == attempt
                {
                    slot.rejected_attempt = Some(attempt);
                }
            }
            Internal::AttemptTransient { cluster, attempt } => {
                if let Some(slot) = self.cluster_mut(&cluster)
                    && slot.attempt == attempt
                {
                    slot.transient_attempt = Some(attempt);
                }
            }
            Internal::SelectedRetryDue { timer } => self.selected_retry_due(timer).await,
            Internal::MemberTick => self.tick_members(Instant::now()),
            Internal::KeepWatchConnected {
                cluster,
                attempt,
                result,
            } => self.kept_connected(&cluster, attempt, result),
            Internal::KeepWatchFailed { cluster, attempt } => self.kept_failed(&cluster, attempt),
            // The loop's sync starts what is due.
            Internal::KeepWatchRetry => {}
            Internal::Discovered {
                cluster,
                attempt,
                result,
            } => {
                let paused = self.paused(&cluster);
                let Some(slot) = self.cluster_mut(&cluster) else {
                    return;
                };
                if slot.attempt != attempt {
                    return;
                }
                slot.discovering = false;
                if paused {
                    slot.rediscover = true;
                    return;
                }
                let again = std::mem::take(&mut slot.rediscover);
                let mut retry = false;
                let mut partial = false;
                let error = match result {
                    Ok(found) => {
                        // Groups that failed this time keep the kinds (and watches) they had,
                        // and discovery is retried with backoff until they answer again.
                        let mut incomplete_groups = found.incomplete_groups();
                        let mut kinds = found.kinds;
                        if !found.skipped_groups.is_empty() {
                            partial = true;
                            tracing::warn!(
                                cluster,
                                groups = ?found.skipped_groups,
                                "discovery skipped API groups; keeping their previous kinds"
                            );
                            kinds = keep_skipped_groups(
                                kinds,
                                slot.discovery.as_deref(),
                                &found.skipped_groups,
                            );
                        }
                        let served: HashSet<KindId> = kinds.iter().map(KindInfo::id).collect();
                        // A CRD watch can be ahead of discovery, so these groups prove nothing yet.
                        let mismatched: Vec<&KindId> = slot
                            .crds
                            .iter()
                            .filter(|k| !served.contains(*k))
                            .chain(slot.removed_crds.iter().filter(|k| served.contains(*k)))
                            .collect();
                        let behind = !mismatched.is_empty();
                        incomplete_groups.extend(mismatched.iter().map(|k| k.group().to_owned()));
                        incomplete_groups.sort();
                        incomplete_groups.dedup();
                        if behind && !again && slot.discovery_retries < DISCOVERY_RETRIES {
                            slot.discovery_retries += 1;
                            retry = true;
                        }
                        tracing::info!(cluster, kinds = kinds.len(), "discovery done");
                        slot.watches.set_kinds(&kinds);
                        slot.discovery = Some(Arc::new(ClusterDiscovery::with_incomplete_groups(
                            kinds,
                            incomplete_groups,
                        )));
                        None
                    }
                    Err(e) => {
                        tracing::warn!(cluster, error = %e, "discovery failed");
                        Some(e)
                    }
                };
                // A failed discovery is retried with backoff while the cluster wants watches;
                // selecting the cluster again also retries it (see `activate`).
                let failed_retry = if error.is_some() || partial {
                    let delay = DISCOVERY_RETRY
                        .saturating_mul(2u32.saturating_pow(slot.discovery_failures.min(6)))
                        .min(DISCOVERY_RETRY_MAX);
                    slot.discovery_failures += 1;
                    Some(delay)
                } else {
                    slot.discovery_failures = 0;
                    None
                };
                let discovery = slot.discovery.clone();
                let generation = slot.generation;
                self.emit(Event::DiscoveryChanged {
                    cluster: cluster.clone(),
                    generation,
                    discovery,
                    error,
                });
                self.reconcile(&cluster);
                let wanted = self.wants_watches(&cluster);
                let delay = if retry {
                    Some(DISCOVERY_RETRY)
                } else if again {
                    self.start_discovery(&cluster);
                    None
                } else {
                    failed_retry.filter(|_| wanted)
                };
                if let Some(delay) = delay {
                    let internal = self.internal_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = internal.send(Internal::RetryDiscovery { cluster });
                    });
                }
            }
            // Only while the cluster wants watches: a timer may outlive a switch to another one.
            Internal::RetryDiscovery { cluster } => {
                if self.wants_watches(&cluster) {
                    self.start_discovery(&cluster);
                }
            }
            Internal::NamespacesListed {
                cluster,
                generation,
                names,
            } => {
                // Queued by a slot since replaced: those names are another server's.
                let Some(slot) = self
                    .cluster_mut(&cluster)
                    .filter(|slot| slot.generation == generation)
                else {
                    return;
                };
                slot.listed_namespaces = names;
                self.apply_scope(&cluster);
            }
            Internal::ReleaseHub { cluster, kind } => {
                let running = self
                    .clusters
                    .iter()
                    .find(|c| c.info.name == cluster)
                    .is_some_and(|c| c.watches.is_running(&kind));
                if !running {
                    self.hubs.release(&cluster, &kind);
                }
            }
            Internal::LogClient {
                cluster,
                pin,
                reply,
            } => {
                let _ = reply.send(self.log_client(&cluster, &pin));
            }
            Internal::CrdsChanged {
                cluster,
                generation,
                fingerprint,
                kinds,
            } => {
                // Queued by a slot since replaced: those CRDs are another server's.
                let Some(slot) = self
                    .cluster_mut(&cluster)
                    .filter(|slot| slot.generation == generation)
                else {
                    return;
                };
                slot.removed_crds = slot.crds.difference(&kinds).cloned().collect();
                slot.crds = kinds;
                slot.discovery_retries = 0;
                let previous = slot.crd_fingerprint.replace(fingerprint);
                let changed = previous.is_some_and(|p| p != fingerprint);
                // The first list after (re)activation may already differ from discovery.
                let behind = slot.discovery.as_ref().is_some_and(|d| {
                    let served: HashSet<KindId> = d.kinds.iter().map(KindInfo::id).collect();
                    slot.crds.iter().any(|k| !served.contains(k))
                        || slot.removed_crds.iter().any(|k| served.contains(k))
                });
                if changed || behind {
                    tracing::info!(cluster, "CRDs changed; refreshing discovery");
                    self.start_discovery(&cluster);
                }
            }
            Internal::FilesChanged(changed) => {
                if changed.rearm || changed.kubeconfig || changed.settings {
                    // A symlink may have moved. Anything written before the new watches start
                    // is caught by the read the watcher asks for once they have.
                    self.arm_watcher();
                }
                self.reload_changed(changed).await;
            }
            Internal::WatchesStarted(dirs) => {
                tracing::debug!(dirs = dirs.len(), "file watches changed");
                self.reload_changed(Changed {
                    kubeconfig: true,
                    settings: true,
                    activity: true,
                    rearm: false,
                })
                .await;
            }
        }
    }

    /// Reads again what `changed` names.
    async fn reload_changed(&mut self, changed: Changed) {
        if changed.settings {
            // First, since settings can change the kubeconfig file list.
            self.reload_settings().await;
        }
        if changed.kubeconfig {
            self.suspend_kept_plugins();
            self.marks.changed(self.read_seq);
            self.reload_kubeconfig().await;
        }
        if changed.activity {
            self.reload_activity().await;
        }
    }

    /// The client a log session should use for `cluster` now. A cluster that was never
    /// connected is connected (the session asked for it); one whose connection failed is not
    /// retried here, so a broken exec plugin does not run again at every session retry. A
    /// context re-pointed or removed since `pin` (when the session opened) ends the session
    /// ([`ContextHistory::check`]).
    fn log_client(&mut self, cluster: &str, pin: &ContextPin) -> Result<Client, LogError> {
        if let Some(reason) = self.contexts.check(Pinned::Logs, cluster, pin) {
            return Err(LogError::cluster(reason));
        }
        // A context unchanged since `pin` whose entry is missing or points elsewhere: a
        // kubeconfig file failed to load (caught half-written, say), and what shows is not
        // confirmed (`ContextHistory`). Wait for the file rather than read from it.
        let slot = self.cluster(cluster).filter(|s| {
            let shown = s.info.server.as_deref();
            // Streams already open go on; this is asked only for a new one.
            !self.paused(cluster)
                // The endpoint alone is enough only for a confirmed server: a fallback at the
                // same URL can come with other credentials.
                && (shown == pin.server()
                    || (shown.map(server_identity) == pin.server().map(server_identity)
                        && self.contexts.shown(cluster, shown).confirmed))
        });
        let Some(slot) = slot else {
            return Err(LogError::retry(format!(
                "\"{cluster}\" is uncertain while a kubeconfig file can't be read"
            )));
        };
        if let Some(clients) = &slot.client {
            return Ok(clients.read.clone());
        }
        match &slot.state {
            ConnectionState::Failed(error) => Err(LogError::retry(format!(
                "\"{cluster}\" is not connected: {error}"
            ))),
            ConnectionState::AwaitingConfirmation => Err(LogError::retry(format!(
                "\"{cluster}\" is waiting for its kubeconfig to be confirmed"
            ))),
            ConnectionState::Disconnected => {
                self.ensure_connected(cluster);
                Err(LogError::retry(format!("connecting to \"{cluster}\"")))
            }
            ConnectionState::Connecting | ConnectionState::Connected(_) => {
                Err(LogError::retry(format!("connecting to \"{cluster}\"")))
            }
        }
    }

    /// The client for a shell on `cluster`, with its pin, read in the same step: a shell is
    /// pinned to the context its client was built from (`CheckContext`). Given the server its
    /// tab shows, refused if the context shows another now (a fallback that came or went since
    /// the tab's view), so it never runs on a server its tab does not name (UP-06).
    fn shell_client(
        &self,
        cluster: &str,
        server: Option<Option<String>>,
    ) -> Result<(GatedClient, ContextPin), WriteRefusal> {
        // Asked for a server, on a context a load confirmed is gone: say so, not "not connected".
        if server.is_some() && self.cluster(cluster).is_none() && self.contexts.removed(cluster) {
            return Err(WriteRefusal::Removed {
                cluster: cluster.to_owned(),
            });
        }
        if let Some(expected) = &server
            && self
                .cluster(cluster)
                .is_some_and(|slot| slot.info.server != *expected)
        {
            return Err(WriteRefusal::Moved {
                cluster: cluster.to_owned(),
            });
        }
        let pin = self.session_pin(cluster);
        self.write_gate(cluster)?;
        let provisional = self.contexts.provisional(cluster);
        // Before the client: a paused context may have none, and this is why it is refused.
        if provisional && self.paused(cluster) {
            return Err(WriteRefusal::Unsettled {
                cluster: cluster.to_owned(),
            });
        }
        let client = self.write_client(cluster)?;
        // A same-URL fallback keeps its pin, but it may carry another file's credentials.
        if provisional {
            return Err(WriteRefusal::Unsettled {
                cluster: cluster.to_owned(),
            });
        }
        Ok((client, pin))
    }

    /// [`Self::shell_client`] for one request of a reviewed write, with the credential of the
    /// connection the client belongs to.
    fn bound_write_client(
        &self,
        cluster: &str,
        server: Option<Option<String>>,
    ) -> Result<crate::handle::BoundWriteClient, WriteError> {
        if self.held(cluster) {
            return Err(WriteError::KubeconfigUnreadable);
        }
        let (client, pin) =
            self.shell_client(cluster, server)
                .map_err(|refusal| match refusal {
                    WriteRefusal::Unsettled { .. } => WriteError::KubeconfigUnreadable,
                    refusal => refusal.into(),
                })?;
        let credential = self
            .cluster(cluster)
            .and_then(|slot| slot.write_credential.clone())
            .ok_or_else(|| WriteRefusal::NotConnected {
                cluster: cluster.to_owned(),
            })?;
        Ok((client, pin, credential))
    }

    /// The pin for a session opened on `cluster` now: to the server its slot shows (what a tab
    /// is labelled with), stale if that is not the confirmed one ([`ContextHistory::pin`]).
    fn session_pin(&self, cluster: &str) -> ContextPin {
        match self.cluster(cluster) {
            Some(slot) => self.contexts.pin(cluster, slot.info.server.as_deref()),
            None => self.contexts.pin_confirmed(cluster),
        }
    }

    /// The client for a new one-off read of `cluster`. None while it is paused.
    fn client(&self, cluster: &str) -> Option<Client> {
        self.cluster(cluster)
            .filter(|_| !self.paused(cluster))
            .and_then(|c| c.client.as_ref().map(|c| c.read.clone()))
    }

    /// The gate every action that changes a cluster, or runs code in it, goes through
    /// (ADR-0015): the cluster's client, unless the cluster is protected or not connected.
    fn write_client(&self, cluster: &str) -> Result<GatedClient, WriteRefusal> {
        let protection = self.write_gate(cluster)?;
        self.cluster(cluster)
            .and_then(|slot| slot.client.as_ref())
            .ok_or_else(|| WriteRefusal::NotConnected {
                cluster: cluster.to_owned(),
            })?
            .writes
            .grant(protection, cluster)
    }

    /// The cluster's protection now, if it allows changes.
    fn write_gate(&self, cluster: &str) -> Result<crate::Protection, WriteRefusal> {
        let server = self
            .cluster(cluster)
            .and_then(|slot| slot.info.server.as_deref());
        self.gate_for_server(cluster, server)
    }

    /// The gate for a session about to run a command with the client it was given at `pin`: for
    /// the server that client talks to, in the same step as checking the context still names it.
    fn launch_gate(&self, cluster: &str, pin: &ContextPin) -> Result<(), WriteRefusal> {
        self.pin_gate(cluster, pin)?;
        if self.contexts.provisional(cluster) {
            return Err(WriteRefusal::Unsettled {
                cluster: cluster.to_owned(),
            });
        }
        Ok(())
    }

    /// Whether `pin` is still good for starting something on `cluster`, and the gate is open for
    /// its server.
    fn pin_gate(&self, cluster: &str, pin: &ContextPin) -> Result<(), WriteRefusal> {
        if self.contexts.check(Pinned::Shell, cluster, pin).is_some()
            || self.contexts.gate_moved(cluster, pin)
        {
            // Refused either way: this only picks the words.
            let cluster = cluster.to_owned();
            return Err(if self.contexts.removed(&cluster) {
                WriteRefusal::Removed { cluster }
            } else if self.contexts.on_pinned_server(&cluster, pin) {
                WriteRefusal::Stale { cluster }
            } else {
                WriteRefusal::Moved { cluster }
            });
        }
        self.gate_for_server(cluster, pin.server()).map(drop)
    }

    /// [`Self::launch_gate`] for a write, in one step with checking the definition it is pinned
    /// to is still the current one and certain.
    fn write_pin_gate(&self, cluster: &str, pin: &ContextPin) -> Result<(), WriteError> {
        if self.held(cluster) {
            return Err(WriteError::KubeconfigUnreadable);
        }
        self.pin_gate(cluster, pin)?;
        if self.contexts.provisional(cluster) {
            Err(WriteError::KubeconfigUnreadable)
        } else if self.contexts.definition_holds(cluster, pin) {
            Ok(())
        } else {
            Err(WriteError::ConnectionChanged)
        }
    }

    /// Whether a port-forward on `pin` may carry on: as for a write, and not while the context
    /// is paused. From memory, so a running forward can ask often.
    fn forward_gate(&self, cluster: &str, pin: &ContextPin) -> Result<(), WriteError> {
        self.write_pin_gate(cluster, pin)?;
        if self.paused(cluster) {
            return Err(WriteRefusal::Unsettled {
                cluster: cluster.to_owned(),
            }
            .into());
        }
        Ok(())
    }

    /// Answers whether a write may go out on `pin`, from a read of the files that starts after
    /// this call. The read runs off this loop, which goes on meanwhile.
    fn check_write_pin(
        &mut self,
        cluster: ClusterId,
        pin: ContextPin,
        reply: oneshot::Sender<Result<(), WriteError>>,
    ) {
        if let Err(refused) = self.write_pin_gate(&cluster, &pin) {
            let _ = reply.send(Err(refused));
            return;
        }
        let paths = self.kubeconfig_paths.clone();
        let (seq, on_late) = self.next_read(&paths);
        let (reader, internal) = (self.reader.clone(), self.internal_tx.clone());
        let deadline = reader.deadline();
        let check = self.write_checks.clone();
        tokio::spawn(async move {
            let read = reader.read_fresh(&paths, on_late).await.map(Box::new);
            let _ = internal.send(Internal::WriteRead {
                cluster,
                pin,
                paths,
                seq,
                read,
                deadline,
                reply,
                check,
            });
        });
    }

    /// [`Self::write_pin_gate`] now, and the files as `read` shows them still giving `pin`'s exact
    /// definition. Also says whether the files differ from the loaded state, to load them.
    fn fresh_write_gate(
        &self,
        cluster: &str,
        pin: &ContextPin,
        paths: &[PathBuf],
        read: Option<Box<FileRead>>,
    ) -> (Result<(), WriteError>, bool) {
        if let Err(refused) = self.write_pin_gate(cluster, pin) {
            return (Err(refused), false);
        }
        if paths != self.kubeconfig_paths {
            return (Err(WriteError::ConnectionChanged), false);
        }
        let Some(fresh) = read else {
            return (Err(WriteError::KubeconfigUnreadable), false);
        };
        let answer = self.read_gives_pin(cluster, pin, &fresh);
        let differs = answer.is_err();
        (answer, differs)
    }

    /// Whether `read` is consistent and gives `pin`'s exact definition. Records nothing.
    fn read_gives_pin(
        &self,
        cluster: &str,
        pin: &ContextPin,
        read: &FileRead,
    ) -> Result<(), WriteError> {
        if !read.consistent {
            return Err(WriteError::KubeconfigUnreadable);
        }
        let mut loaded = read.loaded.clone();
        self.contexts.fail_emptied(&mut loaded, &read.blank);
        match self.contexts.pin_in(&loaded, cluster, pin) {
            FreshDefinition::Holds => Ok(()),
            FreshDefinition::Unreadable => Err(WriteError::KubeconfigUnreadable),
            FreshDefinition::Changed => Err(WriteError::ConnectionChanged),
        }
    }

    /// Answers a write check whose read predates a known change, from one new read that must
    /// finish by `deadline`. That read is also the reload the change asked for. If it can't
    /// start or finish in time the write is refused; it is never sent on the older read.
    async fn revalidate_write(
        &mut self,
        cluster: &str,
        pin: &ContextPin,
        paths: &[PathBuf],
        deadline: tokio::time::Instant,
        why: ReadFor,
    ) -> Result<(), WriteError> {
        self.write_pin_gate(cluster, pin)?;
        if paths != self.kubeconfig_paths {
            return Err(WriteError::ConnectionChanged);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(WriteError::KubeconfigUnreadable);
        }
        let mut answer = Err(WriteError::KubeconfigUnreadable);
        let applied = self
            .read_and_apply(Some(deadline), why, |manager, read| {
                answer = manager.read_gives_pin(cluster, pin, read);
            })
            .await;
        if !applied {
            return Err(WriteError::KubeconfigUnreadable);
        }
        // Applying can lock the cluster or move its gate. A refusal from the read itself stands.
        answer?;
        self.write_pin_gate(cluster, pin)
    }

    /// `cluster`'s protection for `server`, if it allows changes.
    fn gate_for_server(
        &self,
        cluster: &str,
        server: Option<&str>,
    ) -> Result<crate::Protection, WriteRefusal> {
        // A context Flint does not know has no client either: say that, not "protected". A held
        // slot counts as unknown here: no load shows its definition, so nothing new starts on it.
        let slot = self
            .cluster(cluster)
            .filter(|_| !self.held(cluster))
            .ok_or_else(|| WriteRefusal::NotConnected {
                cluster: cluster.to_owned(),
            })?;
        let proxy = slot.info.proxy_url.as_deref();
        let protection = protection(&self.settings, &self.unlocks, cluster, server, proxy);
        if protection.allows_changes() {
            Ok(protection)
        } else {
            Err(WriteRefusal::Protected {
                cluster: cluster.to_owned(),
            })
        }
    }

    /// Whether `id`'s slot is only kept because a file that defined it can't be parsed.
    fn held(&self, id: &str) -> bool {
        self.held.contains(id)
    }

    /// Whether `id` is listed but a file that can't be parsed could still change its definition:
    /// a held slot, or an entry that may be a later file's fallback.
    fn unsettled(&self, id: &str) -> bool {
        self.held(id) || (self.cluster(id).is_some() && self.contexts.provisional(id))
    }

    /// Every slot as front ends see it. `provisional` is [`Self::unsettled`], so it covers each
    /// reason the write gate refuses an uncertain definition, a held slot included.
    fn summaries(&self) -> Vec<ClusterSummary> {
        self.clusters
            .iter()
            .map(|c| {
                let name = &c.info.name;
                c.summary(&self.contexts, self.unsettled(name), self.paused(name))
            })
            .collect()
    }

    /// Whether nothing new may start for `id`: a held slot, or an entry that may be a later file's
    /// fallback and differs from the definition a load last confirmed. A provisional entry that
    /// matches it keeps connecting. One no load ever confirmed has nothing to differ from, so it
    /// connects too, with whatever credential it shows. Writes stay refused on all of them.
    fn paused(&self, id: &str) -> bool {
        self.held(id)
            || self
                .cluster(id)
                .is_some_and(|slot| self.settled.paused(&self.contexts, id, slot.fingerprint))
    }

    /// Makes `id`'s state say what [`Self::paused`] says: a paused slot with no connection waits,
    /// and one whose pause ended is disconnected, for the usual paths to connect it. An attempt
    /// still running keeps `Connecting`: its result lands here as a wait.
    fn show_pause_state(&mut self, id: &str) {
        let paused = self.paused(id);
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        match &slot.state {
            ConnectionState::AwaitingConfirmation if !paused => {
                slot.state = ConnectionState::Disconnected;
            }
            ConnectionState::Disconnected if paused && slot.client.is_none() => {
                slot.state = ConnectionState::AwaitingConfirmation;
            }
            ConnectionState::Failed(_)
                if paused && slot.client.is_none() && !slot.awaits_selection =>
            {
                slot.reconnect = true;
                slot.state = ConnectionState::AwaitingConfirmation;
            }
            ConnectionState::Connecting
                if paused && slot.client.is_none() && slot.connect_task.is_none() =>
            {
                slot.reconnect = true;
                slot.state = ConnectionState::AwaitingConfirmation;
            }
            _ => {}
        }
    }

    /// Publishes what the gate and the sessions go by now, for front ends to read directly.
    fn publish_holds(&self) {
        let of = |test: &dyn Fn(&str) -> bool| {
            self.clusters
                .iter()
                .map(|c| c.info.name.clone())
                .filter(|name| test(name))
                .collect()
        };
        self.holds
            .publish(of(&|id| self.unsettled(id)), of(&|id| self.paused(id)));
    }

    /// Reads the kubeconfig files now, ahead of the file watcher, and says whether they still
    /// give `id` the definition the last load showed at `server`. Anything else is a no, a read
    /// that fails included, and a read that stalls also holds writes as a reload's does. Another
    /// read of the files still running is a no too, but no stall: that read's own timeout says
    /// whether they stalled. What gives access checks this right before saving.
    async fn files_still_show(&mut self, id: &str, server: &str) -> bool {
        let paths = self.kubeconfig_paths.clone();
        let (_, on_late) = self.next_read(&paths);
        match self.reader.read_once(&paths, on_late).await {
            Ok(Some(mut fresh)) => {
                self.saw_read(&paths, Some(&fresh));
                self.contexts.fail_emptied(&mut fresh.loaded, &fresh.blank);
                fresh.consistent && self.contexts.holds_in(&fresh.loaded, id, server)
            }
            Ok(None) => {
                self.hold_stalled(paths);
                false
            }
            Err(ReadBusy) => false,
        }
    }

    /// Records a finished read of `paths`, and says whether it arrived in the order reads started.
    /// A read of another list says nothing about these files, and a read that gave no result has
    /// nothing to compare.
    fn saw_read(&mut self, paths: &[PathBuf], read: Option<&FileRead>) -> bool {
        match read {
            Some(read) if paths == self.kubeconfig_paths => self.marks.saw(read.order),
            _ => true,
        }
    }

    /// Numbers a new read of `paths`, and what to do with its result if it comes in late.
    fn next_read(
        &mut self,
        paths: &[PathBuf],
    ) -> (u64, impl FnOnce(FileRead) + Clone + Send + 'static) {
        self.read_seq += 1;
        let seq = self.read_seq;
        let internal = self.internal_tx.clone();
        let paths = paths.to_vec();
        (seq, move |read| {
            let _ = internal.send(Internal::LateRead {
                paths,
                seq,
                read: Box::new(read),
            });
        })
    }

    /// Applies a read of `paths` that did not finish as every file failing, which keeps the
    /// clusters and holds their writes. Files of the last state that are not in `paths` count as
    /// failed too, so what they defined stays until a finished read shows it gone. Only a read
    /// numbered from here on ends the hold ([`ReadMarks`]). Reported once per stall.
    fn hold_stalled(&mut self, paths: Vec<PathBuf>) {
        self.marks.held_from = Some(self.read_seq);
        if self.stalled_on.as_ref() == Some(&paths) {
            return;
        }
        self.emit(Event::Error(
            "Your kubeconfig files are taking too long to read. Clusters stay as they were, and \
             changes to them are refused until the files can be read."
                .to_owned(),
        ));
        let mut stalled = FileRead::stalled(&paths);
        let last = &self.kubeconfig;
        stalled.keep_undecided(last.paths.iter().chain(last.errors.iter().map(|(p, _)| p)));
        self.stalled_on = Some(paths);
        self.apply_files(stalled);
    }

    /// Of the files in `blank` that mattered when they last loaded, those not yet emptied and
    /// unwritten for [`Self::empty_settle`]. Those still count as failed, and a follow-up read
    /// is scheduled for them.
    fn still_settling(&mut self, blank: &[BlankFile]) -> Vec<BlankFile> {
        let waiting =
            self.emptied
                .waiting(&self.contexts, blank, Instant::now(), self.empty_settle);
        if !waiting.is_empty() && !self.settle_armed {
            self.settle_armed = true;
            let (internal, after) = (self.internal_tx.clone(), self.empty_settle);
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                let _ = internal.send(Internal::SettleEmptied);
            });
        }
        waiting
    }

    fn cluster(&self, id: &str) -> Option<&Cluster> {
        self.clusters.iter().find(|c| c.info.name == id)
    }

    fn cluster_mut(&mut self, id: &str) -> Option<&mut Cluster> {
        self.clusters.iter_mut().find(|c| c.info.name == id)
    }

    fn activate(&mut self, id: ClusterId) {
        // Someone selected or retried a cluster: a prompting plugin resting after a timed-out
        // refresh may run again now.
        crate::auth_plugin::retry_now(&id);
        self.keep_watching_acted_on(&id);
        let Some(slot) = self.cluster_mut(&id) else {
            self.emit(Event::Error(format!("cluster \"{id}\" no longer exists")));
            return;
        };
        slot.awaits_selection = false;
        let idle = !matches!(
            slot.state,
            ConnectionState::Connecting | ConnectionState::Connected(_)
        );
        self.retries.retry.selected(idle);
        if self.active.as_deref() != Some(&id) {
            // Judged while it is still in the background, so a connection already refused is
            // replaced below rather than reused.
            self.take_refusal(&id, Instant::now());
            let previous = self.select(&id);
            self.emit(Event::ActiveChanged(Some(id.clone())));
            if let Some(previous) = previous {
                let now = Instant::now();
                self.apply_member(&previous, MemberEvent::Deactivated, now);
                self.demote_member(&previous, now);
                // Its watches stop; its client and discovery stay (ADR-0008).
                self.reconcile(&previous);
            }
            // A provider's lease ended when it was switched away from: selecting it reconnects.
            self.drop_spent_connections();
        }
        self.apply_member(&id, MemberEvent::Activated, Instant::now());
        self.ensure_connected(&id);
        // A connected cluster whose discovery failed gets another try when selected.
        if let Some(slot) = self.cluster_mut(&id)
            && slot.client.is_some()
            && slot.discovery.is_none()
            && !slot.discovering
        {
            self.start_discovery(&id);
        }
        self.reconcile(&id);
    }

    /// [`Self::activate`], only if `id` is still the slot and identity the caller pinned.
    fn activate_pinned(
        &mut self,
        id: ClusterId,
        pin: &ReadPin,
        generation: u64,
    ) -> Result<(), ActivationRefused> {
        let Some(slot) = self.clusters.iter().find(|c| c.info.name == id) else {
            return Err(ActivationRefused::Removed);
        };
        let now = slot.summary(&self.contexts, self.unsettled(&id), self.paused(&id));
        if now.generation != generation || now.read_pin() != *pin {
            return Err(ActivationRefused::Changed);
        }
        if now.paused {
            return Err(ActivationRefused::Paused);
        }
        self.activate(id);
        Ok(())
    }

    /// Makes `id` the selection and returns the one before it, already out of the foreground: this
    /// comes before the new selection is published, so no request on the old connection can run
    /// its credential command once it is visible.
    fn select(&mut self, id: &str) -> Option<ClusterId> {
        let previous = self.active.replace(id.to_owned());
        if let Some(slot) = previous.as_deref().and_then(|name| self.cluster(name)) {
            slot.access.set_role(self.connection_role(&slot.info.name));
        }
        self.sync_roles();
        previous
    }

    /// Starts a connection unless one is up or in progress. A paused context waits, and the load
    /// that ends the pause starts the attempt. A member connects only through its own attempt,
    /// so a request without one asks for it instead.
    fn ensure_connected(&mut self, id: &str) {
        let kubeconfig = self.kubeconfig.clone();
        let internal = self.internal_tx.clone();
        let attempt = self.next_attempt;
        let paused = self.paused(id);
        self.show_pause_state(id);
        let role = self.connection_role(id);
        let runs_a_command = self.definition_runs_a_command(id);
        let unbound = self.members.contains_key(id)
            && self.cluster(id).is_some_and(|slot| {
                slot.member_attempt.is_none()
                    && !matches!(
                        slot.state,
                        ConnectionState::Connecting | ConnectionState::Connected(_)
                    )
            });
        if unbound && !paused {
            self.request_member_attempt(id);
            return;
        }
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if paused
            || matches!(
                slot.state,
                ConnectionState::Connecting | ConnectionState::Connected(_)
            )
        {
            let state = slot.state.clone();
            self.emit(Event::ConnectionChanged {
                cluster: id.to_owned(),
                state,
            });
            return;
        }
        self.next_attempt += 1;
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        slot.attempt = attempt;
        slot.reconnect = false;
        slot.state = ConnectionState::Connecting;
        slot.telemetry = None;
        slot.client = None;
        slot.write_credential = None;
        slot.access.revoke();
        slot.access = CredentialAccess::new(Arc::new(ForegroundOnly), role);
        // One denied from the start reports that denial, which is a real answer.
        slot.attempt_lease = runs_a_command
            .then(|| slot.access.grant(CredentialSource::Plugin).lease())
            .filter(CredentialLease::holds);
        let access = slot.access.clone();
        let cluster = id.to_owned();
        self.emit(Event::ConnectionChanged {
            cluster: cluster.clone(),
            state: ConnectionState::Connecting,
        });
        tracing::info!(cluster, "connecting");
        // Run the attempt as its own task so a panic inside kube or an auth plugin handler
        // becomes a failed connection instead of a cluster stuck in "connecting".
        let name = cluster.clone();
        let rejections = internal.clone();
        let attempt_task = tokio::spawn(async move {
            let merged = &kubeconfig.merged;
            let result = cluster::connect_gated_and_check_with_policy(
                merged,
                &name,
                CONNECT_TIMEOUT,
                &access,
            )
            .await;
            result.map_err(|e| {
                if running_set::needs_sign_in(&e) {
                    let _ = rejections.send(Internal::AttemptRejected {
                        cluster: name.clone(),
                        attempt,
                    });
                } else if crate::connection::retry::transient(&e) {
                    let _ = rejections.send(Internal::AttemptTransient {
                        cluster: name.clone(),
                        attempt,
                    });
                }
                e.summary()
            })
        });
        if let Some(slot) = self.cluster_mut(id)
            && let Some(superseded) = slot.connect_task.replace(attempt_task.abort_handle())
        {
            superseded.abort();
        }
        tokio::spawn(async move {
            // The app shows this message as it is: only safe text ([`ClusterError::summary`]).
            let result = match attempt_task.await {
                Ok(result) => result,
                // Aborted because something replaced it, so no one is waiting for this result.
                Err(e) if e.is_cancelled() => return,
                Err(e) => Err(cluster::SafeErrorText::reduced(&format!(
                    "connection task failed: {e}"
                ))),
            };
            if let Err(e) = &result {
                tracing::warn!(cluster, error = %e, "connection failed");
            }
            let _ = internal.send(Internal::Connected {
                cluster,
                attempt,
                result,
            });
        });
    }

    /// Gives `id`'s watches the namespaces it should list now. A new namespace starts a list, so
    /// a paused cluster keeps what it has, and the load that ends the pause applies this.
    fn apply_scope(&mut self, id: &str) {
        if self.paused(id) {
            return;
        }
        if let Some(slot) = self.cluster_mut(id) {
            slot.update_scope();
        }
    }

    /// Catches up a cluster whose pause just ended on the same definition: the discovery and
    /// watches that waited start now. A watch that kept running publishes its rows once more, so
    /// a view refused during the pause tries again even if the kind stays quiet. A slot replaced
    /// by a changed definition runs no watch yet, so it publishes nothing here.
    fn resume(&mut self, id: &str) {
        if let Some(slot) = self.cluster(id) {
            for kind in slot.watches.running().into_keys() {
                self.hubs.get_or_create(id, &kind).wake.notify_one();
            }
        }
        let wanted = self.wants_watches(id);
        if self.cluster(id).is_some_and(|slot| {
            slot.client.is_some()
                && !slot.discovering
                && (slot.rediscover || (wanted && slot.discovery.is_none()))
        }) {
            self.start_discovery(id);
        }
        self.reconcile(id);
    }

    /// Reads the agent activity log and sends it to the front end.
    async fn reload_activity(&mut self) {
        let dir = self.config.config_dir.clone();
        let read = tokio::task::spawn_blocking(move || {
            (activity::read(&dir), activity::ServerKeys::open(&dir))
        });
        if let Ok((records, keys)) = read.await {
            self.emit(Event::AgentActivity(Arc::new(records), keys));
        }
    }

    /// A new slot generation.
    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Loads the kubeconfig files and applies them. Whether that was done: a file that fails to
    /// parse still counts, and is reported in the cluster list. A read that does not finish in
    /// time is applied as every file failing, which keeps the clusters and holds their writes,
    /// and counts as not done. Its result is applied when it comes in, if nothing newer was. So
    /// are files that kept changing through every read in that time.
    ///
    /// With a read of these files already running this returns false at once, without waiting
    /// on it or holding anything: that read's own timeout decides whether the files stalled,
    /// and the reload runs when its result comes in. Files still changing when the time ran out
    /// are read again a little later, as no late result will come for them.
    async fn reload_kubeconfig(&mut self) -> bool {
        self.read_and_apply(None, ReadFor::Change, |_, _| {}).await
    }

    /// Whether a queued reload is for a change, which no read running may answer.
    fn change_queued(&self) -> bool {
        self.reread && !self.refresh_only
    }

    /// Queues a [`ReadFor::Refresh`]. A change already queued stays one.
    fn queue_refresh(&mut self) {
        self.refresh_only = !self.change_queued();
        self.reread = true;
    }

    /// Runs a queued refresh if no write check is still to report and no read is running. If
    /// either is, its result comes through this loop and calls this again.
    async fn refresh_if_idle(&mut self) {
        let waiting = Arc::strong_count(&self.write_checks) > 1;
        if self.reread && !waiting && !self.reader.busy(&self.kubeconfig_paths) {
            self.read_and_apply(None, ReadFor::Refresh, |_, _| {}).await;
        }
    }

    /// [`Self::reload_kubeconfig`], finishing by `by` if given. `inspect` sees a consistent read
    /// just before it is applied.
    async fn read_and_apply(
        &mut self,
        by: Option<tokio::time::Instant>,
        why: ReadFor,
        inspect: impl FnOnce(&Self, &FileRead),
    ) -> bool {
        let paths = self.kubeconfig_paths.clone();
        let (seq, on_late) = self.next_read(&paths);
        let Ok(read) = self.reader.read_settled(&paths, by, on_late).await else {
            match why {
                ReadFor::Change => {
                    self.reread = true;
                    self.refresh_only = false;
                    self.marks.overtaken(self.read_seq);
                }
                ReadFor::Refresh => self.queue_refresh(),
            }
            return false;
        };
        self.reread = false;
        self.refresh_only = false;
        if let Some(read) = &read {
            self.saw_read(&paths, Some(read));
        }
        match read {
            Some(read) if read.consistent => {
                inspect(self, &read);
                self.take_read(seq, read);
                true
            }
            Some(_) => {
                self.hold_stalled(paths);
                self.read_again_later();
                false
            }
            None => {
                self.hold_stalled(paths);
                false
            }
        }
    }

    /// Takes the finished read `seq` as the current state, which ends any hold.
    fn take_read(&mut self, seq: u64, read: FileRead) {
        self.applied_read = seq;
        self.stalled_on = None;
        self.marks.applied();
        self.apply_files(read);
    }

    /// Schedules one more read of files that would not hold still, up to [`READ_AGAIN_LIMIT`].
    fn read_again_later(&mut self) {
        if !self.marks.arm_retry() {
            return;
        }
        let internal = self.internal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(READ_AGAIN_AFTER).await;
            let _ = internal.send(Internal::ReadAgain);
        });
    }

    /// Takes a read of the kubeconfig files as the current state.
    fn apply_files(&mut self, read: FileRead) {
        let FileRead {
            mut loaded,
            exist: path_exist,
            blank,
            consistent: _,
            held_still: _,
            order: _,
        } = read;
        let settling = self.still_settling(&blank);
        self.contexts.fail_emptied(&mut loaded, &settling);
        let loaded = Arc::new(loaded);
        tracing::info!(
            contexts = loaded.contexts.len(),
            files = loaded.paths.len(),
            "kubeconfig loaded"
        );
        for (path, err) in &loaded.errors {
            // The full error can quote lines of the file, credentials included; log one line.
            tracing::warn!(
                path = %path.display(),
                error = %kubeconfig::error_summary(err),
                "kubeconfig file skipped"
            );
        }

        let order: Vec<String> = self.clusters.iter().map(|c| c.info.name.clone()).collect();
        let paused: HashSet<String> = order
            .iter()
            .filter(|name| self.paused(name))
            .cloned()
            .collect();
        let mut old: HashMap<String, Cluster> = self
            .clusters
            .drain(..)
            .map(|c| (c.info.name.clone(), c))
            .collect();
        let mut changed_active = false;
        // Dropping a slot aborts its attempt, which then never reports: its admission is freed here.
        let mut discarded: Vec<AttemptId> = Vec::new();
        let mut retry_replaced = None;
        for info in &loaded.contexts {
            let fingerprint = cluster::context_fingerprint(&loaded.merged, &info.name);
            let was_paused = paused.contains(&info.name);
            let slot = match old.remove(&info.name) {
                Some(mut existing) if existing.fingerprint == fingerprint => {
                    // Back unchanged. No connection starts while paused, so start it now.
                    if was_paused && self.active.as_deref() == Some(&info.name) {
                        changed_active = true;
                    }
                    existing.info = info.clone();
                    existing
                }
                Some(old) => {
                    // The context's connection details changed: the client is stale. The fresh
                    // slot has attempt 0, which no in-flight attempt uses. The front end's
                    // namespace filter still applies.
                    if self.active.as_deref() == Some(&info.name) {
                        changed_active = true;
                        if self.retries.retry.has_retried(&info.name, old.generation) {
                            retry_replaced = Some(info.name.clone());
                        }
                    }
                    discarded.extend(old.member_attempt);
                    let mut fresh =
                        Cluster::new(info.clone(), fingerprint, &self.hubs, &self.engine_config);
                    fresh.namespace_filter = old.namespace_filter.clone();
                    fresh.update_scope();
                    fresh.generation = self.next_generation();
                    // A slot that waits for a new selection goes on waiting through later loads.
                    if old.awaits_selection {
                        fresh.awaits_selection = true;
                        fresh.state = old.state.clone();
                    }
                    fresh.reconnect = !old.awaits_selection
                        && (old.reconnect
                            || !matches!(
                                old.state,
                                ConnectionState::Disconnected
                                    | ConnectionState::AwaitingConfirmation
                            ));
                    fresh
                }
                None => {
                    let mut fresh =
                        Cluster::new(info.clone(), fingerprint, &self.hubs, &self.engine_config);
                    fresh.generation = self.next_generation();
                    fresh
                }
            };
            self.clusters.push(slot);
        }
        self.kubeconfig = loaded.clone();
        let uncertain: HashSet<&str> = loaded
            .contexts
            .iter()
            .map(|info| info.name.as_str())
            .filter(|name| self.contexts.provisional(name) && !self.contexts.removed(name))
            .collect();
        // A file that failed to load masks the changes it could have made (UP-06).
        self.contexts.record(&loaded);
        // An unlock names only the server. A context that comes out of an uncertain spell on
        // another definition may hold another user or token there, so it is locked again.
        for name in self.settled.update(&self.contexts, &loaded) {
            if uncertain.contains(name.as_str()) {
                self.unlocks.lock(&name);
            }
        }
        // A context that a file which can't be parsed defined is not gone: keep its slot, in its
        // place. Writes on it are refused until a load shows it again ([`Self::held`]).
        self.held.clear();
        for (at, name) in order.iter().enumerate() {
            if self.contexts.defined_in_failed(name, &loaded.errors)
                && let Some(slot) = old.remove(name)
            {
                self.clusters.insert(at.min(self.clusters.len()), slot);
                self.held.insert(name.clone());
            }
        }
        // In one step with the state the gate reads, and before anything is told of this load.
        self.publish_holds();
        let names: Vec<ClusterId> = self.clusters.iter().map(|c| c.info.name.clone()).collect();
        for name in &names {
            self.show_pause_state(name);
        }
        discarded.extend(old.values().filter_map(|slot| slot.member_attempt));
        // Dropped only when this load ends, and their clients must stop before anything else.
        for slot in old.values_mut() {
            slot.telemetry = None;
            slot.access.revoke();
        }
        for attempt in discarded {
            self.schedule.finish(attempt);
        }
        // A member ends with its context, or on another server once a load confirms it. Until then
        // the server may be a fallback, so it keeps its place and starts nothing.
        let ended: Vec<ClusterId> = self
            .members
            .keys()
            .filter(|id| !self.member_on_its_server(id) && !self.unsettled(id))
            .cloned()
            .collect();
        for id in &ended {
            self.members.remove(id);
            self.schedule.dequeue(id);
        }
        // On every load: a context on a fallback now may be back on its server by the next one.
        let mut to_reconcile = self.cut_off_changed_members(None);
        for id in &ended {
            if !to_reconcile.contains(id) {
                to_reconcile.push(id.clone());
            }
        }
        if let Some(id) = &retry_replaced {
            self.retry_replaced(id);
        }
        // Before anything reports this load, so no context is shown paused with a feed running.
        self.stop_unwanted_telemetry();
        self.arm_set_ticker();
        self.sync_roles();
        self.drop_spent_connections();
        self.sync_member_pauses(Instant::now());
        // The default namespace may have changed, and a filter set during a pause waits for this.
        for name in loaded.contexts.iter().map(|info| &info.name) {
            self.apply_scope(name);
        }

        let snapshot = ClustersSnapshot {
            clusters: self.summaries(),
            errors: loaded.errors.clone(),
            paths: path_exist,
        };
        self.emit(Event::ClustersChanged(Arc::new(snapshot)));

        if let Some(active) = self.active.clone() {
            if old.contains_key(&active) {
                // The active cluster was removed from kubeconfig.
                self.active = None;
                self.emit(Event::ActiveChanged(None));
                self.emit(Event::Error(format!(
                    "\"{active}\" was removed from your kubeconfig files."
                )));
            } else if changed_active && !self.cluster(&active).is_some_and(|s| s.awaits_selection) {
                self.reconnect_active(&active);
            }
        }
        for name in paused
            .iter()
            .filter(|name| !self.paused(name))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.resume(&name);
        }
        for id in to_reconcile {
            self.reconcile(&id);
        }
        // A paused context starts nothing, and the flag then waits for a later load. The rest go
        // through the queue, so a change to many contexts does not reconnect them all at once.
        let due: Vec<ClusterId> = self
            .members
            .keys()
            .filter(|id| {
                self.member_on_its_server(id)
                    && !self.paused(id)
                    && self.cluster(id).is_some_and(|slot| slot.reconnect)
            })
            .cloned()
            .collect();
        let now = Instant::now();
        for id in due {
            // An attempt already running for this slot answers the flag.
            if self.cluster(&id).is_some_and(|slot| {
                !matches!(slot.state, ConnectionState::Connecting) || slot.member_attempt.is_none()
            }) {
                self.apply_member(&id, MemberEvent::ConnectionLost, now);
            }
        }
        self.pump_members(now);
    }

    /// Asks for the file watches to match the current kubeconfig files and `settings.toml`, if
    /// watching is enabled. The scan runs on the watcher's own thread, so a stuck mount cannot
    /// hold up this loop; once watches changed, that thread asks for everything to be read again.
    fn arm_watcher(&mut self) {
        if !self.config.watch_files {
            return;
        }
        if self.watcher.is_none() {
            self.watcher = FileWatcher::start(self.internal_tx.clone());
        }
        if let Some(watcher) = &self.watcher {
            watcher.request(WatchScope {
                kubeconfig: self.kubeconfig_paths.clone(),
                config_dir: self.config.config_dir.clone(),
            });
        }
    }
}

/// A subscription a follower task reads. When the task ends (aborted, or the core stops), the
/// receiver is dropped first and the manager is then asked to release the hub, so hubs of
/// followed kinds do not pile up as clusters are deactivated or removed.
struct Follower {
    rx: Option<watch::Receiver<Arc<KindSnapshot>>>,
    cluster: ClusterId,
    kind: KindId,
    out: mpsc::UnboundedSender<Internal>,
}

impl Follower {
    fn new(
        hubs: &HubRegistry,
        cluster: &str,
        kind: KindId,
        out: &mpsc::UnboundedSender<Internal>,
    ) -> Self {
        let (hub, rx) = hubs.subscribe(cluster, &kind);
        hub.wake.notify_one();
        Follower {
            rx: Some(rx),
            cluster: cluster.to_owned(),
            kind,
            out: out.clone(),
        }
    }

    /// The next synced snapshot, at most every `debounce`. `None` once the hub is gone.
    async fn next_synced(&mut self, debounce: Duration) -> Option<Arc<KindSnapshot>> {
        let rx = self.rx.as_mut()?;
        loop {
            rx.changed().await.ok()?;
            if !debounce.is_zero() {
                tokio::time::sleep(debounce).await;
            }
            let snapshot = rx.borrow_and_update().clone();
            if snapshot.state == SyncState::Synced {
                return Some(snapshot);
            }
        }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.rx = None;
        let _ = self.out.send(Internal::ReleaseHub {
            cluster: self.cluster.clone(),
            kind: self.kind.clone(),
        });
    }
}

/// Follows the CRD list of an active cluster and reports its fingerprint when it changes, so
/// discovery can be refreshed. Ends when aborted or the hub goes away.
async fn follow_crds(
    mut follower: Follower,
    generation: u64,
    out: mpsc::UnboundedSender<Internal>,
) {
    let mut last = None;
    while let Some(snapshot) = follower.next_synced(CRD_DEBOUNCE).await {
        let fingerprint = crd_fingerprint(&snapshot);
        if last == Some(fingerprint) {
            continue;
        }
        last = Some(fingerprint);
        tracing::debug!(
            cluster = follower.cluster,
            crds = snapshot.rows.len(),
            "CRD list changed"
        );
        // A CRD's name is `<plural>.<group>`, which is its kind's id.
        let kinds = snapshot
            .rows
            .iter()
            .map(|r| {
                let (plural, group) = r.name.split_once('.').unwrap_or((&r.name, ""));
                KindId::new(group, plural)
            })
            .collect();
        let msg = Internal::CrdsChanged {
            cluster: follower.cluster.clone(),
            generation,
            fingerprint,
            kinds,
        };
        if out.send(msg).is_err() {
            return;
        }
    }
}

/// Follows the namespaces the user can list on an active cluster, for namespace-limited lists.
/// Ends when aborted or the hub goes away.
async fn follow_namespaces(
    mut follower: Follower,
    generation: u64,
    out: mpsc::UnboundedSender<Internal>,
) {
    let mut last: Option<Vec<String>> = None;
    while let Some(snapshot) = follower.next_synced(Duration::ZERO).await {
        let names: Vec<String> = snapshot.names().into_iter().map(str::to_owned).collect();
        if last.as_ref() == Some(&names) {
            continue;
        }
        last = Some(names.clone());
        let msg = Internal::NamespacesListed {
            cluster: follower.cluster.clone(),
            generation,
            names,
        };
        if out.send(msg).is_err() {
            return;
        }
    }
}

/// Changes when a CRD is added, removed, or its spec changes (versions, names, printer
/// columns). Status-only updates (conditions) leave it alone.
fn crd_fingerprint(snapshot: &KindSnapshot) -> u64 {
    let mut hasher = DefaultHasher::new();
    for row in &snapshot.rows {
        row.uid.hash(&mut hasher);
        row.generation.hash(&mut hasher);
    }
    hasher.finish()
}

/// `kinds` plus, for each group in `skipped`, the kinds `previous` discovery had in it, so a
/// group that failed once (a flapping aggregated API) does not lose its navigation and watches.
fn keep_skipped_groups(
    mut kinds: Vec<KindInfo>,
    previous: Option<&ClusterDiscovery>,
    skipped: &[String],
) -> Vec<KindInfo> {
    if let Some(previous) = previous {
        kinds.extend(
            previous
                .kinds
                .iter()
                .filter(|k| skipped.iter().any(|g| g == k.group()))
                .cloned(),
        );
        // Same order as discovery: core first, then by group and kind.
        kinds.sort_by(|a, b| {
            (!a.group().is_empty(), a.group(), a.kind()).cmp(&(
                !b.group().is_empty(),
                b.group(),
                b.kind(),
            ))
        });
    }
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::member::MemberState;

    // covers UP-20, UP-22
    #[test]
    fn skipped_groups_keep_their_previous_kinds() {
        use crate::discovery::tests::kind;
        let lw = &["get", "list", "watch"];
        let previous = ClusterDiscovery::new(vec![
            kind("", "Pod", "pods", true, lw),
            kind(
                "metrics.k8s.io",
                "PodMetrics",
                "pods",
                true,
                &["get", "list"],
            ),
            kind("example.com", "Widget", "widgets", true, lw),
        ]);
        let now = vec![
            kind("", "Pod", "pods", true, lw),
            kind("example.com", "Gadget", "gadgets", true, lw),
        ];
        let ids = |kinds: Vec<KindInfo>| -> Vec<String> {
            kinds.iter().map(|k| k.id().to_string()).collect()
        };
        assert_eq!(
            ids(keep_skipped_groups(
                now.clone(),
                Some(&previous),
                &["metrics.k8s.io".to_owned()]
            )),
            ["pods", "gadgets.example.com", "pods.metrics.k8s.io"]
        );
        // A group that answered replaces its old kinds (widgets is gone), and without an
        // earlier discovery nothing is added.
        assert_eq!(
            ids(keep_skipped_groups(
                now,
                None,
                &["metrics.k8s.io".to_owned()]
            )),
            ["pods", "gadgets.example.com"]
        );
    }

    fn context(namespace: Option<&str>) -> ContextInfo {
        ContextInfo {
            name: "dev".into(),
            cluster: None,
            user: None,
            namespace: namespace.map(str::to_owned),
            server: Some("https://dev".into()),
            proxy_url: None,
            source: PathBuf::from("/k"),
        }
    }

    /// A kubeconfig file in `dir` defining [`context`]'s server, as the list of files to read.
    fn kubeconfig_file(dir: &Path) -> Vec<PathBuf> {
        let path = dir.join("kubeconfig");
        std::fs::write(&path, kubeconfig_with("https://dev")).unwrap();
        vec![path]
    }

    /// Records a clean load of the manager's files, for a manager whose slot a test added by
    /// hand.
    fn record_load(manager: &mut ClusterManager) {
        let loaded = kubeconfig::load(&manager.kubeconfig_paths);
        manager.contexts.record(&loaded);
    }

    // covers UP-21
    #[test]
    fn namespace_limited_lists_use_the_filter_then_the_default_then_what_is_listable() {
        let hubs = HubRegistry::default();
        let config = Arc::new(EngineConfig::default());
        let mut cluster = Cluster::new(context(Some("team-a")), None, &hubs, &config);
        cluster.listed_namespaces = vec!["x".into(), "y".into()];
        cluster.update_scope();
        assert_eq!(
            *cluster.watches.scope(),
            ["team-a"],
            "the context's default namespace"
        );
        cluster.namespace_filter = vec!["b".into(), "c".into()];
        cluster.update_scope();
        assert_eq!(*cluster.watches.scope(), ["b", "c"], "the filter wins");

        let mut cluster = Cluster::new(context(None), None, &hubs, &config);
        cluster.listed_namespaces = vec!["x".into(), "y".into()];
        cluster.update_scope();
        assert_eq!(
            *cluster.watches.scope(),
            ["x", "y"],
            "else what the user can list"
        );
    }

    /// A kubeconfig with `dev` at `server`, and `other`. Nothing in it is ever contacted.
    fn kubeconfig_with(server: &str) -> String {
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {server}\n\
             - name: o\n  cluster:\n    server: https://other\nusers:\n- name: u\n  user: {{}}\n\
             contexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n- name: other\n  \
             context:\n    cluster: o\n    user: u\n"
        )
    }

    // covers UP-06: a shell asked for on a context a load removed says so.
    #[tokio::test]
    async fn a_shell_on_a_removed_context_says_it_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://remote-a")).await;
        let without_dev = "apiVersion: v1\nkind: Config\nclusters:\n- name: o\n  cluster:\n    \
                           server: https://other\nusers:\n- name: u\n  user: {}\ncontexts:\n- \
                           name: other\n  context:\n    cluster: o\n    user: u\n";
        let pin = manager.contexts.pin_confirmed("dev");
        rewrite(&mut manager, dir.path(), without_dev).await;
        assert_eq!(
            manager
                .shell_client("dev", Some(Some("https://remote-a".into())))
                .err(),
            Some(WriteRefusal::Removed {
                cluster: "dev".into()
            })
        );
        assert_eq!(
            manager.launch_gate("dev", &pin),
            Err(WriteRefusal::Removed {
                cluster: "dev".into()
            }),
            "a launch check says it is gone, not moved"
        );
        assert_eq!(
            manager.shell_client("dev", None).err(),
            Some(WriteRefusal::NotConnected {
                cluster: "dev".into()
            }),
            "no tab named a server"
        );
    }

    /// A manager reading one kubeconfig file, `kubeconfig` in `dir`, loaded once.
    async fn manager_reading(dir: &Path, yaml: &str) -> ClusterManager {
        let path = dir.join("kubeconfig");
        std::fs::write(&path, yaml).unwrap();
        let (events, _rx) = crate::events::channel();
        let config = CoreConfig {
            config_dir: PathBuf::from("/nonexistent"),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: vec![path],
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        };
        let mut manager = ClusterManager::new(
            config,
            events,
            HubRegistry::default(),
            SessionUnlocks::default(),
        );
        manager.reload_kubeconfig().await;
        assert!(manager.cluster("dev").is_some(), "dev loaded");
        manager
    }

    /// Rewrites the manager's kubeconfig file and reloads it, as the file watcher does.
    async fn rewrite(manager: &mut ClusterManager, dir: &Path, yaml: &str) {
        std::fs::write(dir.join("kubeconfig"), yaml).unwrap();
        manager.reload_kubeconfig().await;
    }

    /// Never connects: a test must not reach for a server.
    fn offline(manager: &mut ClusterManager) {
        manager.cluster_mut("dev").unwrap().state =
            ConnectionState::Failed(cluster::SafeErrorText::fixed("offline"));
    }

    // covers UP-06, UP-40, UP-45
    #[tokio::test]
    async fn a_log_session_never_reconnects_to_a_re_pointed_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        offline(&mut manager);
        let pin = manager.contexts.pin_confirmed("dev");

        let same = manager.log_client("dev", &pin).err().expect("no client");
        assert!(
            !same.is_fatal() && !same.is_cluster(),
            "the same server is retried: {}",
            same.message
        );
        rewrite(
            &mut manager,
            dir.path(),
            &kubeconfig_with("https://elsewhere"),
        )
        .await;
        offline(&mut manager);
        let moved = manager.log_client("dev", &pin).err().expect("no client");
        assert!(moved.is_cluster(), "it ends a workload session too");
        assert!(
            moved.message.contains("different API server"),
            "{}",
            moved.message
        );
        let nowhere = manager.contexts.pin_confirmed("nowhere");
        let gone = manager
            .log_client("other", &nowhere)
            .err()
            .expect("no client");
        assert!(gone.is_cluster());
    }

    // covers UP-55: from Stop Trusting on, the gate refuses at every step: before the write, once
    // it is done, and after the file is read back.
    #[tokio::test]
    async fn the_gate_stays_closed_through_stop_trusting() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::events::channel();
        let config = CoreConfig {
            config_dir: dir.path().to_owned(),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: kubeconfig_file(dir.path()),
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        };
        let hubs = HubRegistry::default();
        let unlocks = SessionUnlocks::default();
        let mut manager = ClusterManager::new(config, events, hubs.clone(), unlocks.clone());
        manager.clusters.push(Cluster::new(
            context(None),
            None,
            &hubs,
            &manager.engine_config,
        ));
        record_load(&mut manager);
        let protected = Some(WriteRefusal::Protected {
            cluster: "dev".into(),
        });

        let trust = unlocks.request_trust("dev");
        manager
            .set_trusted("dev".into(), Some("https://dev".into()), trust)
            .await;
        assert!(manager.write_gate("dev").is_ok());

        let stop = unlocks.revoke_trust("dev");
        assert_eq!(manager.write_gate("dev").err(), protected);
        manager.set_trusted("dev".into(), None, stop).await;
        assert!(!unlocks.trust_revoked("dev"), "the revocation has ended");
        assert!(manager.settings.trusted_clusters.is_empty());
        assert_eq!(manager.write_gate("dev").err(), protected);
        manager.reload_settings().await;
        assert_eq!(manager.write_gate("dev").err(), protected);
    }

    // covers UP-54: a running port-forward stops being allowed once its context is held
    // (paused behind a kubeconfig file that can't be read) or re-pointed.
    #[tokio::test]
    async fn a_forward_is_refused_while_held_or_re_pointed() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        let pin = manager.contexts.pin_confirmed("dev");
        manager.held.insert("dev".into());
        assert!(matches!(
            manager.forward_gate("dev", &pin),
            Err(WriteError::KubeconfigUnreadable)
        ));
        manager.held.remove("dev");
        rewrite(&mut manager, dir.path(), &kubeconfig_with("https://old")).await;
        assert!(matches!(
            manager.forward_gate("dev", &pin),
            Err(WriteError::Refused(WriteRefusal::Moved { .. }))
        ));
    }

    // covers UP-06, UP-41: a shell's check (`Request::CheckContext`) compares the context with
    // how it was when the shell got its client, whatever the context says now.
    #[tokio::test]
    async fn a_shell_ends_once_its_context_points_elsewhere_or_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        let pin = manager.contexts.pin_confirmed("dev");
        assert_eq!(manager.contexts.check(Pinned::Shell, "dev", &pin), None);
        rewrite(&mut manager, dir.path(), &kubeconfig_with("https://old")).await;
        let moved = manager
            .contexts
            .check(Pinned::Shell, "dev", &pin)
            .expect("re-pointed");
        assert!(moved.contains("different API server"), "{moved}");
        assert!(moved.contains("open the shell again"), "{moved}");

        let pin = manager.contexts.pin_confirmed("dev");
        let without_dev = "apiVersion: v1\nkind: Config\nclusters: []\nusers: []\ncontexts: []\n";
        rewrite(&mut manager, dir.path(), without_dev).await;
        let gone = manager
            .contexts
            .check(Pinned::Shell, "dev", &pin)
            .expect("removed");
        assert!(gone.contains("no longer in your kubeconfig"), "{gone}");
    }

    // covers UP-06, UP-41: re-pointed and put back by two reloads between checks still ends
    // the shell and the log session.
    #[tokio::test]
    async fn a_re_pointing_undone_between_checks_still_ends_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        let pin = manager.contexts.pin_confirmed("dev");
        rewrite(&mut manager, dir.path(), &kubeconfig_with("https://prod")).await;
        rewrite(&mut manager, dir.path(), &kubeconfig_with("https://dev")).await;
        offline(&mut manager);
        let shell = manager.contexts.check(Pinned::Shell, "dev", &pin);
        // It points where it did, so the reason does not claim another server.
        assert!(
            shell
                .as_deref()
                .is_some_and(|r| r.contains("changed in your kubeconfig files")),
            "{shell:?}"
        );
        let logs = manager.log_client("dev", &pin).err().expect("no client");
        assert!(logs.is_cluster(), "{}", logs.message);
    }

    /// Reads every path whatever its kind, so a FIFO blocks as a stalled mount would.
    fn blocking_load(paths: &[PathBuf]) -> LoadedKubeconfig {
        for path in paths {
            let _ = std::fs::read(path);
        }
        kubeconfig::load(paths)
    }

    /// The next message the manager's tasks sent it.
    async fn next_internal(manager: &mut ClusterManager) -> Internal {
        let internal = manager.internal_rx.as_mut().unwrap();
        tokio::time::timeout(Duration::from_secs(5), internal.recv())
            .await
            .expect("a message in time")
            .expect("the channel is open")
    }

    /// A load that always finishes just after the short timeout of these tests.
    fn slow_load(paths: &[PathBuf]) -> LoadedKubeconfig {
        let loaded = kubeconfig::load(paths);
        std::thread::sleep(Duration::from_millis(120));
        loaded
    }

    // covers UP-06, UP-54: a Trust check that is the first read to stall holds writes, as a
    // reload's stall does.
    #[tokio::test]
    async fn a_stalled_access_check_holds_writes() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, &yaml).await;
        manager.config.config_dir = dir.path().join("config");
        let second = dir.path().join("second");
        std::fs::remove_file(&second).unwrap();
        make_fifo(&second);
        manager.reader = KubeconfigReader::new(blocking_load, Duration::from_millis(50));
        let pin = manager.contexts.pin_confirmed("dev");

        let at = Some("https://dev".to_owned());
        assert!(!manager.set_pin(Pins::Trust, "dev".into(), at).await);
        assert!(manager.held("dev") && manager.unsettled("dev"));
        assert_eq!(
            manager.write_pin_gate("dev", &pin),
            Err(WriteError::KubeconfigUnreadable)
        );
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&second)
                .unwrap(),
        );
    }

    // covers UP-06: reads that always end just after the deadline still reach the state, so the
    // clusters do not stay held. A late read of another list, or older than one applied, does not.
    #[tokio::test]
    async fn a_late_read_is_applied_only_while_current() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, &yaml).await;
        manager.reader = KubeconfigReader::new(slow_load, Duration::from_millis(40));

        assert!(!manager.reload_kubeconfig().await);
        assert!(manager.held("dev"), "held while the read is out");
        let late = next_internal(&mut manager).await;
        manager.handle_internal(late).await;
        assert!(!manager.held("dev"), "the late result was applied");

        // Held again, with this read overtaken by a newer applied one: it changes nothing.
        assert!(!manager.reload_kubeconfig().await);
        let late = next_internal(&mut manager).await;
        manager.reader = KubeconfigReader::default();
        manager.applied_read = manager.read_seq + 1;
        manager.handle_internal(late).await;
        assert!(manager.held("dev"), "an overtaken read is dropped");

        assert!(manager.reload_kubeconfig().await);
        manager.reader = KubeconfigReader::new(slow_load, Duration::from_millis(40));
        assert!(!manager.reload_kubeconfig().await);
        let late = next_internal(&mut manager).await;
        manager.kubeconfig_paths.pop();
        manager.handle_internal(late).await;
        assert!(manager.held("dev"), "a read of another list is dropped");
    }

    // covers UP-06, UP-54: an emptied file holds writes while it settles, and one that stays
    // empty and unwritten then removes its contexts through a follow-up read.
    #[tokio::test]
    async fn a_file_left_empty_removes_its_contexts_after_settling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.empty_settle = Duration::from_millis(150);
        let pin = manager.contexts.pin_confirmed("dev");

        std::fs::write(&path, "").unwrap();
        manager.reload_kubeconfig().await;
        assert!(manager.cluster("dev").is_some(), "kept while settling");
        assert_eq!(
            manager.write_pin_gate("dev", &pin),
            Err(WriteError::KubeconfigUnreadable)
        );

        let follow_up = next_internal(&mut manager).await;
        assert!(matches!(follow_up, Internal::SettleEmptied));
        manager.handle_internal(follow_up).await;
        assert!(manager.cluster("dev").is_none(), "gone once settled");
        assert!(manager.contexts.removed("dev"));
        assert!(manager.write_pin_gate("dev", &pin).is_err());
    }

    // covers UP-06: a file written again while it settles starts the wait over.
    #[tokio::test]
    async fn rewriting_an_emptied_file_restarts_its_settling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.empty_settle = Duration::from_millis(200);

        std::fs::write(&path, "").unwrap();
        manager.reload_kubeconfig().await;
        tokio::time::sleep(Duration::from_millis(220)).await;
        std::fs::write(&path, "\n").unwrap();
        manager.reload_kubeconfig().await;
        assert!(manager.cluster("dev").is_some(), "written again: it waits");
    }

    /// Asks for a write check, as a request does.
    fn ask_write(
        manager: &mut ClusterManager,
        pin: &ContextPin,
    ) -> oneshot::Receiver<Result<(), WriteError>> {
        let (reply, answer) = oneshot::channel();
        manager.check_write_pin("dev".into(), pin.clone(), reply);
        answer
    }

    /// How long a [`ReadHold`] keeps a load before letting it go by itself, so that a loop that
    /// wrongly waits for the read fails the test instead of hanging it.
    const READ_HOLD_LIMIT: Duration = Duration::from_secs(30);

    /// Keeps [`held_read`] of one path from finishing until the test drops this or the limit
    /// passes, as a stalled mount would.
    struct ReadHold {
        path: PathBuf,
        state: Arc<ReadHoldState>,
    }

    #[derive(Default)]
    struct ReadHoldState {
        progress: std::sync::Mutex<ReadHoldProgress>,
        changed: std::sync::Condvar,
    }

    #[derive(Default)]
    struct ReadHoldProgress {
        loads: usize,
        released: bool,
    }

    static READ_HOLDS: std::sync::Mutex<Vec<(PathBuf, Arc<ReadHoldState>)>> =
        std::sync::Mutex::new(Vec::new());

    impl ReadHold {
        fn on(path: &Path) -> Self {
            let state = Arc::new(ReadHoldState::default());
            READ_HOLDS
                .lock()
                .unwrap()
                .push((path.to_path_buf(), state.clone()));
            Self {
                path: path.to_path_buf(),
                state,
            }
        }

        /// Whether a load has started and is still being kept from finishing.
        fn holding(&self) -> bool {
            let progress = self.state.progress.lock().unwrap();
            progress.loads > 0 && !progress.released
        }

        /// Waits for a load to reach the hold.
        async fn until_holding(&self) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !self.holding() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the read reached the hold");
        }
    }

    impl Drop for ReadHold {
        fn drop(&mut self) {
            READ_HOLDS
                .lock()
                .unwrap()
                .retain(|(path, _)| *path != self.path);
            self.state.progress.lock().unwrap().released = true;
            self.state.changed.notify_all();
        }
    }

    /// Loads `paths`, first waiting on the [`ReadHold`] of any of them.
    fn held_read(paths: &[PathBuf]) -> LoadedKubeconfig {
        let hold = READ_HOLDS
            .lock()
            .unwrap()
            .iter()
            .find(|(path, _)| paths.contains(path))
            .map(|(_, state)| state.clone());
        if let Some(state) = hold {
            let until = std::time::Instant::now() + READ_HOLD_LIMIT;
            let mut progress = state.progress.lock().unwrap();
            progress.loads += 1;
            while !progress.released {
                let left = until.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    progress.released = true;
                    break;
                }
                progress = state.changed.wait_timeout(progress, left).unwrap().0;
            }
        }
        kubeconfig::load(paths)
    }

    // covers UP-54, UP-55: a write check on a hung read leaves the loop free, so Stop Trusting and
    // Hide from AI save meanwhile, and it is refused when the read's timeout passes.
    #[tokio::test]
    async fn a_hung_read_refuses_the_write_and_leaves_the_loop_free() {
        // Only the paused clock below reaches this, so no save can have waited it out.
        const TIMEOUT: Duration = Duration::from_secs(3600);
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, &yaml).await;
        manager.config.config_dir = dir.path().join("config");
        let at = Some("https://dev".to_owned());
        assert!(manager.set_pin(Pins::Trust, "dev".into(), at.clone()).await);
        let expose = Pins::AiExposure(AiScope::default());
        assert!(manager.set_pin(expose, "dev".into(), at).await);
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let hold = ReadHold::on(&dir.path().join("second"));
        manager.reader = KubeconfigReader::new(held_read, TIMEOUT);

        let mut answer = ask_write(&mut manager, &pin);
        hold.until_holding().await;
        let hide = Pins::AiExposure(AiScope::default());
        assert!(manager.set_pin(hide, "dev".into(), None).await);
        assert!(manager.set_pin(Pins::Trust, "dev".into(), None).await);
        assert!(hold.holding(), "the saves did not wait for the read");
        assert!(answer.try_recv().is_err(), "no answer before the read");
        let internal = manager.internal_rx.as_mut().unwrap();
        assert!(internal.try_recv().is_err(), "the read has not ended");
        let kept = saved(dir.path());
        assert!(kept.ai_exposed_clusters.is_empty() && kept.trusted_clusters.is_empty());

        tokio::time::pause();
        tokio::time::advance(TIMEOUT).await;
        let read = next_internal(&mut manager).await;
        assert!(matches!(read, Internal::WriteRead { read: None, .. }));
        assert!(hold.holding(), "refused on the timeout, not a late result");
        manager.handle_internal(read).await;
        assert_eq!(answer.await.unwrap(), Err(WriteError::KubeconfigUnreadable));
        assert!(manager.held("dev"), "later writes are refused at once");
    }

    /// Parses the files, then waits on a FIFO named `gate` beside them if there is one, so what
    /// it returns is older than the files by the time it returns.
    fn load_then_wait(paths: &[PathBuf]) -> LoadedKubeconfig {
        let loaded = kubeconfig::load(paths);
        let gate = paths[0].with_file_name("gate");
        if gate.exists() {
            std::fs::write(paths[0].with_file_name("parsed"), "").unwrap();
            let _ = std::fs::read(&gate);
            std::fs::remove_file(&gate).unwrap();
        }
        loaded
    }

    // covers UP-54: a read that started before the file changed, and would pass, is not what a
    // later write check is answered from.
    #[tokio::test]
    async fn a_read_in_flight_does_not_satisfy_a_later_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading(dir.path(), &yaml).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let gate = dir.path().join("gate");
        make_fifo(&gate);
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_secs(5));

        let (reader, paths) = (manager.reader.clone(), manager.kubeconfig_paths.clone());
        let in_flight = tokio::spawn(async move { reader.read(&paths, |_| {}).await });
        until_parsed(dir.path()).await;

        std::fs::write(&path, yaml.replace("user: {}", "user: {token: another}")).unwrap();
        let answer = ask_write(&mut manager, &pin);
        tokio::time::sleep(Duration::from_millis(30)).await;
        release_gate(&gate);

        let old = in_flight.await.unwrap().expect("the first read finished");
        assert_eq!(
            manager.contexts.pin_in(&old.loaded, "dev", &pin),
            FreshDefinition::Holds,
            "the read in flight would have passed"
        );
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(answer.await.unwrap(), Err(WriteError::ConnectionChanged));
    }

    /// Lets [`load_then_wait`] go on, opening the FIFO until the load has removed it: a writer
    /// that opens and closes while the reader is still inside `open` can leave it waiting.
    fn release_gate(gate: &Path) {
        use std::os::unix::fs::OpenOptionsExt;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while gate.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the load took the gate"
            );
            drop(
                std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(gate),
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Waits for [`load_then_wait`] to have parsed the files.
    async fn until_parsed(dir: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.join("parsed").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the read parsed the file");
    }

    /// Hands the manager a write check whose read did not finish, and returns its answer.
    async fn timed_out_write(
        manager: &mut ClusterManager,
        pin: &ContextPin,
        seq: u64,
    ) -> Result<(), WriteError> {
        let (reply, answer) = oneshot::channel();
        let paths = manager.kubeconfig_paths.clone();
        manager
            .handle_internal(Internal::WriteRead {
                cluster: "dev".into(),
                pin: pin.clone(),
                paths,
                seq,
                read: None,
                deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                reply,
                check: manager.write_checks.clone(),
            })
            .await;
        answer.await.unwrap()
    }

    // covers UP-06, UP-54: a reload asked for while a write check reads the files runs when that
    // read ends, though the write is refused, so the change shows and the hold ends.
    #[tokio::test]
    async fn a_reload_during_a_write_read_runs_when_the_read_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading(dir.path(), &yaml).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let gate = dir.path().join("gate");
        make_fifo(&gate);
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_secs(5));

        let answer = ask_write(&mut manager, &pin);
        until_parsed(dir.path()).await;
        std::fs::write(&path, yaml.replace("user: {}", "user: {token: another}")).unwrap();
        assert!(!manager.reload_kubeconfig().await, "the files are in use");
        assert!(!manager.held("dev"));
        release_gate(&gate);

        let read = next_internal(&mut manager).await;
        assert!(matches!(read, Internal::WriteRead { read: Some(_), .. }));
        manager.handle_internal(read).await;
        assert_eq!(answer.await.unwrap(), Err(WriteError::ConnectionChanged));
        assert!(!manager.held("dev"), "the pending reload ran");
        assert!(
            !manager.contexts.definition_holds("dev", &pin),
            "the new content was applied"
        );
    }

    // covers UP-06, UP-54: a reload asked for while a healthy write check reads unchanged files
    // is answered at once and holds nothing, the write goes through, and the reload then runs.
    #[tokio::test]
    async fn a_reload_during_a_healthy_write_read_lets_the_write_through() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let gate = dir.path().join("gate");
        make_fifo(&gate);
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_secs(5));

        let answer = ask_write(&mut manager, &pin);
        until_parsed(dir.path()).await;
        let applied_before = manager.applied_read;
        let (reply, reloaded) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(1),
            manager.handle_internal(Internal::ReloadKubeconfig { reply }),
        )
        .await
        .expect("the loop does not wait on the read");
        assert_eq!(reloaded.await, Ok(false), "answered, as not applied yet");
        assert!(manager.reread, "the reload is queued");
        assert!(!manager.held("dev") && manager.stalled_on.is_none());
        release_gate(&gate);

        let read = next_internal(&mut manager).await;
        assert!(matches!(read, Internal::WriteRead { read: Some(_), .. }));
        manager.handle_internal(read).await;
        assert_eq!(answer.await.unwrap(), Ok(()));
        assert!(!manager.reread, "the queued reload ran");
        assert!(manager.applied_read > applied_before, "and applied");
        assert!(!manager.held("dev"));
    }

    // covers UP-06, UP-54: a read whose late result came in before its write check's timeout
    // does not hold the files again, and that write stays refused.
    #[tokio::test]
    async fn a_timeout_after_its_late_result_does_not_hold_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let paths = manager.kubeconfig_paths.clone();
        let read = manager.reader.read(&paths, |_| {}).await.unwrap();
        let (seq, _) = manager.next_read(&paths);
        manager.hold_stalled(paths.clone());

        let late = Internal::LateRead {
            paths,
            seq,
            read: Box::new(read),
        };
        manager.handle_internal(late).await;
        assert!(!manager.held("dev"), "the late result applied");
        assert_eq!(
            timed_out_write(&mut manager, &pin, seq).await,
            Err(WriteError::KubeconfigUnreadable)
        );
        assert!(
            !manager.held("dev"),
            "no stall is left with nothing to end it"
        );
    }

    // covers UP-06: a write check that timed out waiting behind a read that has since ended has
    // no result coming, so the files are read again instead of staying held.
    #[tokio::test]
    async fn a_timeout_with_no_read_running_reads_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let paths = manager.kubeconfig_paths.clone();
        let (seq, _) = manager.next_read(&paths);

        assert_eq!(
            timed_out_write(&mut manager, &pin, seq).await,
            Err(WriteError::KubeconfigUnreadable)
        );
        assert!(!manager.held("dev"));
    }

    // covers UP-06, UP-54: a write check's stall behind a read whose result is overtaken still
    // ends when that read does, from a new read of the current content.
    #[tokio::test]
    async fn a_stall_behind_an_overtaken_read_ends_with_a_new_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading(dir.path(), &yaml).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let gate = dir.path().join("gate");
        make_fifo(&gate);
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_millis(40));
        let paths = manager.kubeconfig_paths.clone();

        let (_, on_late) = manager.next_read(&paths);
        assert!(manager.reader.read(&paths, on_late).await.is_none());
        let (seq, _) = manager.next_read(&paths);
        assert_eq!(
            timed_out_write(&mut manager, &pin, seq).await,
            Err(WriteError::KubeconfigUnreadable)
        );
        assert!(manager.held("dev"), "held while the read runs");
        manager.applied_read = seq;

        std::fs::write(&path, yaml.replace("user: {}", "user: {token: another}")).unwrap();
        release_gate(&gate);
        let late = next_internal(&mut manager).await;
        assert!(matches!(late, Internal::LateRead { .. }));
        manager.handle_internal(late).await;
        assert!(!manager.held("dev"), "read again");
        assert!(!manager.contexts.definition_holds("dev", &pin));
    }

    /// Parses the files, then moves a waiting `next` file over the first one, once.
    fn load_then_repoint(paths: &[PathBuf]) -> LoadedKubeconfig {
        let loaded = kubeconfig::load(paths);
        let _ = std::fs::rename(paths[0].with_file_name("next"), &paths[0]);
        loaded
    }

    /// Parses the files, then always changes the first one, so no read ever settles.
    fn load_then_touch(paths: &[PathBuf]) -> LoadedKubeconfig {
        use std::io::Write as _;
        let loaded = kubeconfig::load(paths);
        let mut file = std::fs::File::options()
            .append(true)
            .open(&paths[0])
            .unwrap();
        file.write_all(b"# changed\n").unwrap();
        loaded
    }

    // covers UP-54: the target re-pointed after its file was parsed is not authorized from the
    // parse that matches the old pin.
    #[tokio::test]
    async fn a_definition_changed_during_the_read_refuses_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        std::fs::write(dir.path().join("next"), kubeconfig_with("https://moved")).unwrap();
        manager.reader = KubeconfigReader::new(load_then_repoint, Duration::from_secs(5));

        let answer = ask_write(&mut manager, &pin);
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(answer.await.unwrap(), Err(WriteError::ConnectionChanged));
    }

    // covers UP-54: files that never hold still give no consistent read, so the write is refused.
    #[tokio::test]
    async fn files_that_never_settle_refuse_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        manager.reader = KubeconfigReader::new(load_then_touch, Duration::from_millis(150));

        let answer = ask_write(&mut manager, &pin);
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(answer.await.unwrap(), Err(WriteError::KubeconfigUnreadable));
    }

    // covers UP-06: a late read that a pending file-change reload supersedes is not published,
    // so a context the newer file still has stays selected.
    #[tokio::test]
    async fn a_pending_reload_supersedes_a_late_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        let yaml = kubeconfig_with("https://dev.invalid");
        let mut manager = manager_reading(dir.path(), &yaml).await;
        manager.active = Some("dev".into());
        let paths = manager.kubeconfig_paths.clone();
        let without_dev = "apiVersion: v1\nkind: Config\nclusters:\n- name: o\n  cluster:\n    \
                           server: https://other.invalid\nusers:\n- name: u\n  user: {}\n\
                           contexts:\n- name: other\n  context:\n    cluster: o\n    user: u\n";
        std::fs::write(&path, without_dev).unwrap();
        let old = manager.reader.read(&paths, |_| {}).await.unwrap();
        assert!(old.loaded.context("dev").is_none());
        std::fs::write(&path, &yaml).unwrap();
        let (seq, _) = manager.next_read(&paths);
        manager.reread = true;

        let late = Internal::LateRead {
            paths,
            seq,
            read: Box::new(old),
        };
        manager.handle_internal(late).await;
        assert_eq!(manager.active.as_deref(), Some("dev"), "still selected");
        assert!(manager.cluster("dev").is_some() && !manager.held("dev"));
        assert!(!manager.reread, "the pending reload ran");
    }

    /// Hands the manager a write check whose read finished with `read`, and returns its answer.
    async fn finished_write(
        manager: &mut ClusterManager,
        pin: &ContextPin,
        seq: u64,
        read: FileRead,
    ) -> Result<(), WriteError> {
        finished_write_on(manager, "dev", pin, seq, read).await
    }

    /// [`finished_write`] for a write to `cluster`.
    async fn finished_write_on(
        manager: &mut ClusterManager,
        cluster: &str,
        pin: &ContextPin,
        seq: u64,
        read: FileRead,
    ) -> Result<(), WriteError> {
        let (reply, answer) = oneshot::channel();
        let paths = manager.kubeconfig_paths.clone();
        manager
            .handle_internal(Internal::WriteRead {
                cluster: cluster.into(),
                pin: pin.clone(),
                paths,
                seq,
                read: Some(Box::new(read)),
                deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                reply,
                check: manager.write_checks.clone(),
            })
            .await;
        answer.await.unwrap()
    }

    /// A manager with a write check's consistent read of the old file in hand, the file since
    /// re-pointed, and the watcher's reload queued behind a read that is still running.
    async fn write_read_overtaken_by_a_change(
        dir: &Path,
    ) -> (
        ClusterManager,
        ContextPin,
        u64,
        FileRead,
        JoinHandle<Option<FileRead>>,
    ) {
        let mut manager = manager_reading(dir, &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let paths = manager.kubeconfig_paths.clone();
        let old = manager.reader.read(&paths, |_| {}).await.unwrap();
        assert!(old.consistent);
        let (seq, _) = manager.next_read(&paths);

        make_fifo(&dir.join("gate"));
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_secs(5));
        let reader = manager.reader.clone();
        let running = tokio::spawn(async move { reader.read(&paths, |_| {}).await });
        until_parsed(dir).await;
        std::fs::write(dir.join("kubeconfig"), kubeconfig_with("https://moved")).unwrap();
        let changed = Changed {
            kubeconfig: true,
            ..Changed::default()
        };
        manager
            .handle_internal(Internal::FilesChanged(changed))
            .await;
        assert!(manager.reread, "the reload is queued");
        (manager, pin, seq, old, running)
    }

    // covers UP-54: a file changed after the reader checked it leaves a read that says it is
    // consistent. With the reload still queued, that read grants nothing.
    #[tokio::test]
    async fn a_queued_change_refuses_a_write_while_files_cannot_be_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, pin, seq, old, running) =
            write_read_overtaken_by_a_change(dir.path()).await;

        assert_eq!(
            finished_write(&mut manager, &pin, seq, old).await,
            Err(WriteError::KubeconfigUnreadable)
        );
        let gate = dir.path().join("gate");
        release_gate(&gate);
        running.await.unwrap();
    }

    // covers UP-54: the same read, once the files can be read again, is checked against a new
    // read, which finds the server the file points to now.
    #[tokio::test]
    async fn a_queued_change_revalidates_the_write_from_a_new_read() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, pin, seq, old, running) =
            write_read_overtaken_by_a_change(dir.path()).await;
        let gate = dir.path().join("gate");
        release_gate(&gate);
        running.await.unwrap();

        assert_eq!(
            finished_write(&mut manager, &pin, seq, old).await,
            Err(WriteError::ConnectionChanged)
        );
        assert!(!manager.reread, "the queued reload ran");
        assert!(!manager.contexts.definition_holds("dev", &pin));
    }

    // covers UP-06: a result older than the read that holds the files does not end the hold; the
    // newer read's own result does.
    #[tokio::test]
    async fn an_older_late_read_does_not_clear_a_newer_hold() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, &yaml).await;
        manager.reader = KubeconfigReader::new(slow_load, Duration::from_millis(40));
        let paths = manager.kubeconfig_paths.clone();

        assert!(!manager.reload_kubeconfig().await);
        tokio::time::timeout(Duration::from_secs(2), async {
            while manager.reader.busy(&paths) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the first read ended");
        assert!(!manager.reload_kubeconfig().await);

        let older = next_internal(&mut manager).await;
        manager.handle_internal(older).await;
        assert!(manager.held("dev"), "the newer read is still out");
        let newer = next_internal(&mut manager).await;
        manager.handle_internal(newer).await;
        assert!(!manager.held("dev"), "its own result ends the hold");
    }

    // covers UP-06: when the only file defining the selected context is replaced and the new file
    // stalls, the context stays until a finished read shows it is gone.
    #[tokio::test]
    async fn a_stalled_replacement_file_keeps_the_previous_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.active = Some("dev".into());
        let pin = manager.contexts.pin_confirmed("dev");
        let replacement = dir.path().join("replacement");
        make_fifo(&replacement);
        manager.kubeconfig_paths = vec![replacement.clone()];
        manager.reader = KubeconfigReader::new(blocking_load, Duration::from_millis(40));

        assert!(!manager.reload_kubeconfig().await);
        assert_eq!(manager.active.as_deref(), Some("dev"), "still selected");
        assert!(manager.held("dev"));
        assert_eq!(
            manager.write_pin_gate("dev", &pin),
            Err(WriteError::KubeconfigUnreadable)
        );

        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap();
        let without_dev = dir.path().join("without-dev");
        std::fs::write(
            &without_dev,
            "apiVersion: v1\nkind: Config\nclusters: []\nusers: []\ncontexts: []\n",
        )
        .unwrap();
        std::fs::rename(&without_dev, &replacement).unwrap();
        drop(writer);
        let late = next_internal(&mut manager).await;
        manager.handle_internal(late).await;
        assert!(
            manager.cluster("dev").is_none(),
            "a finished read removed it"
        );
        assert_eq!(manager.active, None);
    }

    // covers UP-54, UP-55: an access check that meets a healthy write check's read answers no,
    // and holds no cluster. The write check is not disturbed.
    #[tokio::test]
    async fn an_access_check_behind_a_healthy_read_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.config.config_dir = dir.path().join("config");
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let gate = dir.path().join("gate");
        make_fifo(&gate);
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_secs(5));

        let answer = ask_write(&mut manager, &pin);
        until_parsed(dir.path()).await;
        let at = Some("https://dev".to_owned());
        assert!(
            !manager.set_pin(Pins::Trust, "dev".into(), at).await,
            "the files could not be confirmed"
        );
        assert!(saved(dir.path()).trusted_clusters.is_empty());
        assert!(!manager.held("dev") && manager.stalled_on.is_none());

        release_gate(&gate);
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(answer.await.unwrap(), Ok(()));
    }

    /// Parses the files, then changes the first one unless a `still` file sits beside it.
    fn load_then_touch_until_still(paths: &[PathBuf]) -> LoadedKubeconfig {
        if paths[0].with_file_name("still").exists() {
            kubeconfig::load(paths)
        } else {
            load_then_touch(paths)
        }
    }

    // covers UP-06: files that were still changing when a reload gave up are read again without
    // any watcher, so the hold ends once they are quiet.
    #[tokio::test]
    async fn files_changing_past_the_deadline_are_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.reader =
            KubeconfigReader::new(load_then_touch_until_still, Duration::from_millis(60));

        assert!(!manager.reload_kubeconfig().await);
        assert!(manager.held("dev"));
        std::fs::write(dir.path().join("still"), "").unwrap();
        let again = next_internal(&mut manager).await;
        manager.handle_internal(again).await;
        assert!(!manager.held("dev"), "a follow-up read ended the hold");
    }

    /// A manager that may write to both contexts, with a read taken before `dev` moved to
    /// another server and a read taken after, each with its number.
    async fn reads_around_a_move(
        dir: &Path,
    ) -> (
        ClusterManager,
        ContextPin,
        ContextPin,
        (u64, FileRead),
        (u64, FileRead),
    ) {
        let mut manager = manager_reading(dir, &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        manager.unlocks.unlock("other", "https://other");
        let dev = manager.contexts.pin_confirmed("dev");
        let other = manager.contexts.pin_confirmed("other");
        let paths = manager.kubeconfig_paths.clone();
        let older = manager.reader.read(&paths, |_| {}).await.unwrap();
        let (older_seq, _) = manager.next_read(&paths);
        std::fs::write(dir.join("kubeconfig"), kubeconfig_with("https://moved")).unwrap();
        let newer = manager.reader.read(&paths, |_| {}).await.unwrap();
        let (newer_seq, _) = manager.next_read(&paths);
        (manager, dev, other, (older_seq, older), (newer_seq, newer))
    }

    // covers UP-54: a newer read that passed for another cluster and saw this one move makes an
    // older result for this cluster count for nothing: it is checked against a new read.
    #[tokio::test]
    async fn an_older_result_after_a_newer_read_is_checked_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, dev, other, (older_seq, older), (newer_seq, newer)) =
            reads_around_a_move(dir.path()).await;

        assert_eq!(
            finished_write_on(&mut manager, "other", &other, newer_seq, newer).await,
            Ok(())
        );
        assert_eq!(
            finished_write(&mut manager, &dev, older_seq, older).await,
            Err(WriteError::ConnectionChanged)
        );
        assert!(!manager.contexts.definition_holds("dev", &dev));
    }

    // covers UP-54: with the files busy, an out-of-order result is refused without waiting, and
    // the read running ends in a reload.
    #[tokio::test]
    async fn an_out_of_order_result_behind_a_running_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, dev, other, (older_seq, older), (newer_seq, newer)) =
            reads_around_a_move(dir.path()).await;
        assert_eq!(
            finished_write_on(&mut manager, "other", &other, newer_seq, newer).await,
            Ok(())
        );
        let gate = dir.path().join("gate");
        make_fifo(&gate);
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_secs(5));
        let (reader, paths) = (manager.reader.clone(), manager.kubeconfig_paths.clone());
        let running = tokio::spawn(async move { reader.read(&paths, |_| {}).await });
        until_parsed(dir.path()).await;

        let answer = tokio::time::timeout(
            Duration::from_secs(1),
            finished_write(&mut manager, &dev, older_seq, older),
        )
        .await
        .expect("the loop does not wait on the read");
        assert_eq!(answer, Err(WriteError::KubeconfigUnreadable));
        assert!(manager.reread, "a reload follows the running read");
        release_gate(&gate);
        running.await.unwrap();
    }

    /// A manager on unchanged files with a write check's read running, an out-of-order result
    /// already refused behind it, and a later valid result in hand with its number.
    async fn refresh_queued_behind_a_check(
        dir: &Path,
    ) -> (
        ClusterManager,
        ContextPin,
        oneshot::Receiver<Result<(), WriteError>>,
        (u64, FileRead),
    ) {
        let mut manager = manager_reading(dir, &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let paths = manager.kubeconfig_paths.clone();
        let mut numbered = Vec::new();
        for _ in 0..3 {
            let read = manager.reader.read(&paths, |_| {}).await.unwrap();
            numbered.push((manager.next_read(&paths).0, read));
        }
        let (valid_seq, valid) = numbered.pop().unwrap();
        let (newer_seq, newer) = numbered.pop().unwrap();
        let (older_seq, older) = numbered.pop().unwrap();
        assert_eq!(
            finished_write(&mut manager, &pin, newer_seq, newer).await,
            Ok(())
        );
        make_fifo(&dir.join("gate"));
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_secs(5));
        let running = ask_write(&mut manager, &pin);
        until_parsed(dir).await;
        assert_eq!(
            finished_write(&mut manager, &pin, older_seq, older).await,
            Err(WriteError::KubeconfigUnreadable),
            "an out-of-order result grants nothing"
        );
        (manager, pin, running, (valid_seq, valid))
    }

    // covers UP-54: a refresh left by an out-of-order result waits for the running read, so it
    // does not make that read's valid snapshot stale, and it runs once the reader is idle.
    #[tokio::test]
    async fn a_refresh_waits_for_the_reader_without_making_checks_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, pin, running, (valid_seq, valid)) =
            refresh_queued_behind_a_check(dir.path()).await;
        let stale_through = manager.marks.stale_through;
        let applied_before = manager.applied_read;

        assert_eq!(
            finished_write(&mut manager, &pin, valid_seq, valid).await,
            Ok(())
        );
        assert_eq!(manager.marks.stale_through, stale_through, "no new change");
        assert!(manager.reread, "the refresh is still pending");

        let gate = dir.path().join("gate");
        release_gate(&gate);
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(running.await.unwrap(), Ok(()));
        assert_eq!(manager.marks.stale_through, stale_through);
        assert!(!manager.reread, "the refresh ran");
        assert!(manager.applied_read > applied_before, "and applied");
    }

    // covers UP-54: a file change while a refresh is pending still invalidates the running read.
    #[tokio::test]
    async fn a_file_change_during_a_pending_refresh_still_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, running, _) = refresh_queued_behind_a_check(dir.path()).await;
        let stale_through = manager.marks.stale_through;

        std::fs::write(
            dir.path().join("kubeconfig"),
            kubeconfig_with("https://moved"),
        )
        .unwrap();
        let changed = Changed {
            kubeconfig: true,
            ..Changed::default()
        };
        manager
            .handle_internal(Internal::FilesChanged(changed))
            .await;
        assert!(manager.marks.stale_through > stale_through);

        let gate = dir.path().join("gate");
        release_gate(&gate);
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(running.await.unwrap(), Err(WriteError::ConnectionChanged));
        assert!(!manager.reread, "the reload ran");
    }

    /// A manager with reads A and B numbered, B's result taken, the files since reported changed
    /// so both are stale, a write check started after that still running, and A's result in hand
    /// with its number.
    async fn stale_result_behind_a_check(
        dir: &Path,
    ) -> (
        ClusterManager,
        ContextPin,
        oneshot::Receiver<Result<(), WriteError>>,
        (u64, FileRead),
    ) {
        let mut manager = manager_reading(dir, &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let paths = manager.kubeconfig_paths.clone();
        let older = manager.reader.read(&paths, |_| {}).await.unwrap();
        let (older_seq, _) = manager.next_read(&paths);
        let newer = manager.reader.read(&paths, |_| {}).await.unwrap();
        let (newer_seq, _) = manager.next_read(&paths);
        assert_eq!(
            finished_write(&mut manager, &pin, newer_seq, newer).await,
            Ok(())
        );
        let changed = Changed {
            kubeconfig: true,
            ..Changed::default()
        };
        manager
            .handle_internal(Internal::FilesChanged(changed))
            .await;
        assert!(!manager.marks.current(older_seq) && !manager.marks.current(newer_seq));
        make_fifo(&dir.join("gate"));
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_secs(5));
        let running = ask_write(&mut manager, &pin);
        until_parsed(dir).await;
        (manager, pin, running, (older_seq, older))
    }

    // covers UP-54: a stale out-of-order result is refused without making a check that started
    // after the change stale, and the refresh it leaves runs once that check is done.
    #[tokio::test]
    async fn a_stale_out_of_order_result_leaves_a_running_check_valid() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, pin, running, (older_seq, older)) =
            stale_result_behind_a_check(dir.path()).await;
        let running_seq = manager.read_seq;
        let stale_through = manager.marks.stale_through;

        assert_eq!(
            finished_write(&mut manager, &pin, older_seq, older).await,
            Err(WriteError::KubeconfigUnreadable),
            "an out-of-order result grants nothing"
        );
        assert_eq!(manager.marks.stale_through, stale_through);
        assert!(manager.marks.current(running_seq));
        assert!(
            manager.reread && manager.refresh_only,
            "a refresh is queued"
        );

        release_gate(&dir.path().join("gate"));
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(running.await.unwrap(), Ok(()));
        assert!(!manager.reread, "the refresh ran");
    }

    // covers UP-54: a change queued behind the running read stays a change through a stale
    // out-of-order result, so the running read still can't answer.
    #[tokio::test]
    async fn a_stale_out_of_order_result_keeps_a_queued_change() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, pin, running, (older_seq, older)) =
            stale_result_behind_a_check(dir.path()).await;
        std::fs::write(
            dir.path().join("kubeconfig"),
            kubeconfig_with("https://moved"),
        )
        .unwrap();
        let changed = Changed {
            kubeconfig: true,
            ..Changed::default()
        };
        manager
            .handle_internal(Internal::FilesChanged(changed))
            .await;
        assert!(manager.change_queued());
        let stale_through = manager.marks.stale_through;

        assert_eq!(
            finished_write(&mut manager, &pin, older_seq, older).await,
            Err(WriteError::KubeconfigUnreadable)
        );
        assert!(manager.change_queued(), "still a change");
        assert_eq!(manager.marks.stale_through, stale_through);

        release_gate(&dir.path().join("gate"));
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(running.await.unwrap(), Err(WriteError::ConnectionChanged));
        assert!(!manager.reread, "the reload ran");
    }

    // covers UP-54: a stale out-of-order result starts a read that stalls, so the reload after it
    // finds the reader busy. That moves no mark, and the stalled read still ends the hold.
    #[tokio::test]
    async fn a_stale_out_of_order_result_moves_no_mark_when_the_reader_turns_busy() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://dev")).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let paths = manager.kubeconfig_paths.clone();
        let older = manager.reader.read(&paths, |_| {}).await.unwrap();
        let (older_seq, _) = manager.next_read(&paths);
        let newer = manager.reader.read(&paths, |_| {}).await.unwrap();
        let (newer_seq, _) = manager.next_read(&paths);
        assert_eq!(
            finished_write(&mut manager, &pin, newer_seq, newer).await,
            Ok(())
        );
        let changed = Changed {
            kubeconfig: true,
            ..Changed::default()
        };
        manager
            .handle_internal(Internal::FilesChanged(changed))
            .await;
        assert!(!manager.marks.current(older_seq));
        let gate = dir.path().join("gate");
        make_fifo(&gate);
        manager.reader = KubeconfigReader::new(load_then_wait, Duration::from_millis(40));
        let stale_through = manager.marks.stale_through;

        assert_eq!(
            finished_write(&mut manager, &pin, older_seq, older).await,
            Err(WriteError::KubeconfigUnreadable)
        );
        assert_eq!(manager.marks.stale_through, stale_through);
        assert!(
            manager.reread && manager.refresh_only,
            "a refresh is queued"
        );
        assert!(manager.held("dev"), "held while the read runs");

        release_gate(&gate);
        let late = next_internal(&mut manager).await;
        assert!(matches!(late, Internal::LateRead { .. }));
        manager.reader = KubeconfigReader::new(kubeconfig::load, Duration::from_secs(5));
        manager.handle_internal(late).await;
        assert!(!manager.held("dev"), "the stalled read ended the hold");
        assert!(!manager.reread, "the refresh ran");
    }

    // covers UP-54: a refresh that finds the reader busy leaves a queued change a change, moves no
    // mark and grants nothing.
    #[tokio::test]
    async fn a_refresh_that_finds_the_reader_busy_keeps_a_queued_change() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, pin, running, _) = stale_result_behind_a_check(dir.path()).await;
        std::fs::write(
            dir.path().join("kubeconfig"),
            kubeconfig_with("https://moved"),
        )
        .unwrap();
        let changed = Changed {
            kubeconfig: true,
            ..Changed::default()
        };
        manager
            .handle_internal(Internal::FilesChanged(changed))
            .await;
        assert!(manager.change_queued());
        let stale_through = manager.marks.stale_through;
        let paths = manager.kubeconfig_paths.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        assert_eq!(
            manager
                .revalidate_write("dev", &pin, &paths, deadline, ReadFor::Refresh)
                .await,
            Err(WriteError::KubeconfigUnreadable)
        );
        assert!(manager.change_queued(), "still a change");
        assert_eq!(manager.marks.stale_through, stale_through);

        release_gate(&dir.path().join("gate"));
        let read = next_internal(&mut manager).await;
        manager.handle_internal(read).await;
        assert_eq!(running.await.unwrap(), Err(WriteError::ConnectionChanged));
        assert!(!manager.reread, "the reload ran");
    }

    // covers UP-06, UP-54: a late result older than one already seen does not bring back what the
    // files no longer say.
    #[tokio::test]
    async fn an_older_late_read_does_not_replace_newer_state() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, dev, _, (_, older), _) = reads_around_a_move(dir.path()).await;
        assert!(manager.reload_kubeconfig().await);
        assert!(!manager.contexts.definition_holds("dev", &dev));
        let paths = manager.kubeconfig_paths.clone();
        let (seq, _) = manager.next_read(&paths);

        let late = Internal::LateRead {
            paths,
            seq,
            read: Box::new(older),
        };
        manager.handle_internal(late).await;
        assert!(!manager.contexts.definition_holds("dev", &dev));
    }

    /// A manager handed a late read that saw its files change.
    async fn manager_after_a_torn_late_read(dir: &Path, retries: u8) -> (ClusterManager, u64) {
        let mut manager = manager_reading(dir, &kubeconfig_with("https://dev")).await;
        manager.marks.retries = retries;
        let paths = manager.kubeconfig_paths.clone();
        let mut read = manager.reader.read(&paths, |_| {}).await.unwrap();
        read.consistent = false;
        let (seq, _) = manager.next_read(&paths);
        let applied = manager.applied_read;
        let late = Internal::LateRead {
            paths,
            seq,
            read: Box::new(read),
        };
        manager.handle_internal(late).await;
        (manager, applied)
    }

    // covers UP-06: a late read that saw its files change waits for the follow-up read, which
    // ends the hold from the files as they are then.
    #[tokio::test]
    async fn a_torn_late_read_waits_for_the_follow_up_read() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, applied) = manager_after_a_torn_late_read(dir.path(), 0).await;
        assert_eq!(manager.applied_read, applied, "no read started at once");
        assert!(manager.held("dev"), "writes wait for a finished read");
        assert!(manager.marks.retry_armed);
        assert_eq!(manager.marks.retries, 1);

        let again = next_internal(&mut manager).await;
        assert!(matches!(again, Internal::ReadAgain));
        manager.handle_internal(again).await;
        assert!(manager.applied_read > applied);
        assert!(!manager.held("dev"), "a follow-up read ended the hold");
    }

    // covers UP-06: past the limit of follow-up reads a torn late read schedules no other.
    #[tokio::test]
    async fn a_torn_late_read_past_the_limit_reads_no_more() {
        let dir = tempfile::tempdir().unwrap();
        let (manager, applied) = manager_after_a_torn_late_read(dir.path(), READ_AGAIN_LIMIT).await;
        assert_eq!(manager.applied_read, applied);
        assert!(manager.held("dev"));
        assert!(!manager.marks.retry_armed);
    }

    // covers UP-06: a reload that could not start is not a change, so it does not refill the
    // follow-up reads, though it still makes the reads before it stale.
    #[test]
    fn a_reload_that_could_not_start_keeps_the_retry_count() {
        let mut marks = ReadMarks::default();
        assert!(marks.arm_retry());
        marks.overtaken(5);
        assert_eq!(marks.retries, 1);
        assert!(!marks.current(5) && marks.current(6));
        marks.changed(6);
        assert_eq!(marks.retries, 0);
    }

    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    }

    // covers UP-06, UP-54: with a kubeconfig read hung, the context is held and nothing widens,
    // while Hide from AI and Stop Trusting still save. When the read ends, a reload is asked for
    // and the files apply again.
    #[tokio::test]
    async fn a_hung_read_holds_contexts_and_still_saves_revocations() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, &yaml).await;
        manager.config.config_dir = dir.path().join("config");
        let at = Some("https://dev".to_owned());
        assert!(manager.set_pin(Pins::Trust, "dev".into(), at.clone()).await);
        let expose = Pins::AiExposure(AiScope::default());
        assert!(manager.set_pin(expose, "dev".into(), at.clone()).await);

        let second = dir.path().join("second");
        std::fs::remove_file(&second).unwrap();
        make_fifo(&second);
        manager.reader = KubeconfigReader::new(blocking_load, Duration::from_millis(50));

        assert!(
            !manager.reload_kubeconfig().await,
            "the read did not finish"
        );
        assert!(manager.cluster("dev").is_some(), "the last state stays");
        assert!(manager.held("dev") && manager.unsettled("dev"));
        assert!(!manager.set_pin(Pins::Trust, "dev".into(), at.clone()).await);

        let started = std::time::Instant::now();
        assert!(!manager.reload_kubeconfig().await, "still stuck");
        assert!(started.elapsed() < Duration::from_millis(50), "no new wait");
        let hide = Pins::AiExposure(AiScope::default());
        assert!(manager.set_pin(hide, "dev".into(), None).await);
        assert!(manager.set_pin(Pins::Trust, "dev".into(), None).await);
        let kept = saved(dir.path());
        assert!(kept.ai_exposed_clusters.is_empty() && kept.trusted_clusters.is_empty());

        // A writer that opens and closes the FIFO ends the blocked read.
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&second)
                .unwrap(),
        );
        let late = next_internal(&mut manager).await;
        assert!(matches!(late, Internal::LateRead { .. }), "the late read");
        std::fs::remove_file(&second).unwrap();
        std::fs::write(&second, &yaml).unwrap();
        assert!(manager.reload_kubeconfig().await);
        assert!(!manager.unsettled("dev"), "settled again");
    }

    // covers UP-06, UP-54: an unlocked context on a same-URL fallback with another token starts
    // no shell, though the pin stays good and the unlock is kept. With the file back it does.
    #[tokio::test]
    async fn a_same_url_fallback_starts_no_shell() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let fallback = yaml.replace("user: {}", "user: {token: fallback}");
        let mut manager = manager_reading_two(dir.path(), &yaml, &fallback).await;
        offline(&mut manager);
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        assert_eq!(manager.launch_gate("dev", &pin), Ok(()));

        let first = dir.path().join("first");
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        offline(&mut manager);
        assert!(!manager.held("dev") && manager.contexts.provisional("dev"));
        let unsettled = WriteRefusal::Unsettled {
            cluster: "dev".into(),
        };
        assert_eq!(manager.launch_gate("dev", &pin), Err(unsettled.clone()));
        assert_eq!(
            manager.write_pin_gate("dev", &pin),
            Err(WriteError::KubeconfigUnreadable),
            "a write keeps its own refusal"
        );
        // A client that is never used: building one contacts nothing.
        let unused = dir.path().join("unused");
        std::fs::write(&unused, &yaml).unwrap();
        let loaded = crate::kubeconfig::load(&[unused]);
        let client = crate::cluster::connect(&loaded.merged, "dev")
            .await
            .expect("a client");
        let connect = |manager: &mut ClusterManager| {
            manager.cluster_mut("dev").unwrap().client = Some(ClusterClients {
                read: client.clone(),
                writes: crate::protection::WriteAccess::new(client.clone()),
            });
        };
        connect(&mut manager);
        assert_eq!(manager.shell_client("dev", None).err(), Some(unsettled));
        assert_eq!(
            manager.bound_write_client("dev", None).err(),
            Some(WriteError::KubeconfigUnreadable)
        );

        rewrite_file(&mut manager, &first, &yaml).await;
        offline(&mut manager);
        manager.unlocks.unlock("dev", "https://dev");
        assert_eq!(manager.launch_gate("dev", &pin), Ok(()));
        connect(&mut manager);
        assert!(manager.shell_client("dev", None).is_ok());
    }

    // covers UP-54: a session's launch check goes through the request channel and answers for
    // the server its client talks to, whatever the context names now.
    #[tokio::test]
    async fn a_launch_check_answers_for_the_pinned_server() {
        async fn ask(
            manager: &mut ClusterManager,
            inbox: &async_channel::Receiver<Request>,
            check: &crate::exec::LaunchCheck,
        ) -> Result<(), WriteRefusal> {
            let answer = tokio::spawn(check());
            let request = inbox.recv().await.unwrap();
            manager.handle_request(request);
            answer.await.unwrap()
        }

        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://remote-a")).await;
        offline(&mut manager);
        let unlocks = manager.unlocks.clone();
        unlocks.unlock("dev", "https://remote-a");
        let pin = manager.contexts.pin_confirmed("dev");
        let (requests, inbox) = async_channel::bounded(8);
        let check = crate::handle::launch_check(requests, "dev".into(), pin);
        let protected = Err(WriteRefusal::Protected {
            cluster: "dev".into(),
        });
        let moved = Err(WriteRefusal::Moved {
            cluster: "dev".into(),
        });

        assert_eq!(ask(&mut manager, &inbox, &check).await, Ok(()));
        unlocks.lock("dev");
        assert_eq!(ask(&mut manager, &inbox, &check).await, protected);
        unlocks.unlock("dev", "https://remote-a");
        assert_eq!(ask(&mut manager, &inbox, &check).await, Ok(()));

        // The context now names another server, unlocked in its own right: the client still
        // talks to the first one.
        rewrite(
            &mut manager,
            dir.path(),
            &kubeconfig_with("https://127.0.0.1:6443"),
        )
        .await;
        offline(&mut manager);
        unlocks.lock("dev");
        assert!(
            manager.write_gate("dev").is_err(),
            "locked at the new server"
        );
        unlocks.unlock("dev", "https://127.0.0.1:6443");
        assert!(manager.write_gate("dev").is_ok(), "open for the new server");
        assert_eq!(ask(&mut manager, &inbox, &check).await, moved);

        // Put back, it is still not the definition the session was pinned to.
        rewrite(
            &mut manager,
            dir.path(),
            &kubeconfig_with("https://remote-a"),
        )
        .await;
        offline(&mut manager);
        unlocks.unlock("dev", "https://remote-a");
        let stale = Err(WriteRefusal::Stale {
            cluster: "dev".into(),
        });
        assert_eq!(ask(&mut manager, &inbox, &check).await, stale);

        drop(inbox);
        assert_eq!(check().await, Err(WriteRefusal::Stopped));
    }

    // covers UP-06: a kubeconfig caught half-written fails to parse and is skipped. That is not
    // a removal: a shell and a log session on its contexts go on, and still do once the file is
    // back as it was.
    #[tokio::test]
    async fn a_half_written_kubeconfig_ends_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading(dir.path(), &yaml).await;
        let pin = manager.contexts.pin_confirmed("dev");
        rewrite(&mut manager, dir.path(), &yaml[..yaml.len() / 2]).await;
        assert!(
            !manager.kubeconfig.errors.is_empty(),
            "the file failed to load"
        );
        assert!(manager.held("dev"), "its contexts stay listed");
        assert_eq!(manager.contexts.check(Pinned::Shell, "dev", &pin), None);
        let logs = manager.log_client("dev", &pin).err().expect("no client");
        assert!(
            !logs.is_cluster() && !logs.is_fatal(),
            "the log session retries: {}",
            logs.message
        );
        rewrite(&mut manager, dir.path(), &yaml).await;
        assert_eq!(manager.contexts.check(Pinned::Shell, "dev", &pin), None);
    }

    /// A manager reading `first` then `second` in `dir`, which define a context `dev` (the
    /// first file wins), loaded once.
    async fn manager_reading_two(dir: &Path, first: &str, second: &str) -> ClusterManager {
        let paths = [dir.join("first"), dir.join("second")];
        std::fs::write(&paths[0], first).unwrap();
        std::fs::write(&paths[1], second).unwrap();
        let (events, _rx) = crate::events::channel();
        let config = CoreConfig {
            config_dir: PathBuf::from("/nonexistent"),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: paths.to_vec(),
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        };
        let mut manager = ClusterManager::new(
            config,
            events,
            HubRegistry::default(),
            SessionUnlocks::default(),
        );
        manager.reload_kubeconfig().await;
        manager
    }

    async fn rewrite_file(manager: &mut ClusterManager, path: &Path, yaml: &str) {
        std::fs::write(path, yaml).unwrap();
        manager.reload_kubeconfig().await;
    }

    const UNREADABLE: &str = "apiVersion: v1\nkind: Config\nclusters:\n- name: [";

    // covers UP-06: a log session waits rather than use a fallback that differs from its server
    // only by a trailing slash, since it can carry other credentials. Once a load confirms the
    // slash, the session goes on.
    #[tokio::test]
    async fn a_log_session_does_not_reconnect_on_a_slash_only_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let (exact, slash) = (
            kubeconfig_with("https://dev"),
            kubeconfig_with("https://dev/"),
        );
        let mut manager = manager_reading_two(dir.path(), &exact, &slash).await;
        let pin = manager.contexts.pin_confirmed("dev");

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        let waiting = manager.log_client("dev", &pin).err().expect("no client");
        assert!(waiting.message.contains("uncertain"), "{}", waiting.message);

        rewrite_file(&mut manager, &first, &slash).await;
        let connecting = manager.log_client("dev", &pin).err().expect("no client");
        assert!(
            connecting.message.contains("connecting"),
            "{}",
            connecting.message
        );
    }

    // covers UP-06, UP-41: a file that fails to load after the one defining a context cannot
    // change it, so it masks no removal: shells and log sessions on it end.
    #[tokio::test]
    async fn a_broken_later_file_does_not_mask_a_removal() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, UNREADABLE).await;
        assert_eq!(
            manager.kubeconfig.errors.len(),
            1,
            "the second file is broken"
        );
        let pin = manager.contexts.pin_confirmed("dev");
        let without_dev = "apiVersion: v1\nkind: Config\nclusters: []\nusers: []\ncontexts: []\n";
        rewrite_file(&mut manager, &dir.path().join("first"), without_dev).await;
        let gone = manager.contexts.check(Pinned::Shell, "dev", &pin);
        assert!(
            gone.as_deref()
                .is_some_and(|r| r.contains("no longer in your kubeconfig")),
            "{gone:?}"
        );
        // A log session opened now ends at once, instead of retrying for the file.
        let pin = manager.contexts.pin_confirmed("dev");
        let logs = manager.log_client("dev", &pin).err().expect("no client");
        assert!(logs.is_cluster(), "{}", logs.message);
        assert!(
            logs.message.contains("no longer in your kubeconfig"),
            "{}",
            logs.message
        );
    }

    // covers UP-06: both files define `dev` on the same server. The first drops it while the
    // second, whose `dev` it shadowed, is caught half-written: the second may still define it,
    // so no session ends.
    #[tokio::test]
    async fn a_shadowed_definition_in_a_half_written_file_ends_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, &yaml).await;
        let pin = manager.contexts.pin_confirmed("dev");
        let without_dev = "apiVersion: v1\nkind: Config\nclusters: []\nusers: []\ncontexts: []\n";
        std::fs::write(dir.path().join("second"), &yaml[..yaml.len() / 2]).unwrap();
        rewrite_file(&mut manager, &dir.path().join("first"), without_dev).await;
        assert_eq!(
            manager.kubeconfig.errors.len(),
            1,
            "the second file is broken"
        );
        assert_eq!(manager.contexts.check(Pinned::Shell, "dev", &pin), None);
        // Back as it was: `dev` from the second file, on the same server.
        rewrite_file(&mut manager, &dir.path().join("second"), &yaml).await;
        assert_eq!(manager.contexts.check(Pinned::Shell, "dev", &pin), None);
    }

    const NO_CONTEXTS: &str =
        "apiVersion: v1\nkind: Config\nclusters: []\nusers: []\ncontexts: []\n";

    /// Asserts that nothing that changes the cluster can start on `dev` now.
    fn assert_no_write_starts(manager: &ClusterManager, pin: &ContextPin) {
        let not_connected = WriteRefusal::NotConnected {
            cluster: "dev".to_owned(),
        };
        assert_eq!(
            manager.shell_client("dev", None).err(),
            Some(not_connected.clone())
        );
        assert_eq!(manager.launch_gate("dev", pin).err(), Some(not_connected));
        assert_eq!(
            manager.bound_write_client("dev", None).err(),
            Some(WriteError::KubeconfigUnreadable)
        );
        assert_eq!(
            manager.write_pin_gate("dev", pin).err(),
            Some(WriteError::KubeconfigUnreadable)
        );
    }

    // covers UP-06, UP-54: a context whose file is broken stays listed, in its place, and refuses
    // every write, even unlocked. The same file back restores the unlock and the review's pin.
    #[tokio::test]
    async fn a_broken_file_keeps_its_context_listed_but_not_writable() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        assert_eq!(manager.write_pin_gate("dev", &pin), Ok(()));

        let path = dir.path().join("first");
        rewrite_file(&mut manager, &path, UNREADABLE).await;
        let names: Vec<_> = manager.clusters.iter().map(|c| &*c.info.name).collect();
        assert_eq!(names, ["dev", "other"]);
        assert_no_write_starts(&manager, &pin);

        rewrite_file(&mut manager, &path, &yaml).await;
        assert!(manager.unlocks.is_unlocked("dev", "https://dev"));
        assert_eq!(manager.write_pin_gate("dev", &pin), Ok(()));
    }

    // covers UP-06, UP-54: a file that comes back with another server, user or token carries
    // over neither the unlock nor a review made before it broke.
    #[tokio::test]
    async fn a_broken_file_back_with_another_definition_keeps_no_unlock_or_review() {
        let yaml = kubeconfig_with("https://dev");
        let another_server = kubeconfig_with("https://elsewhere");
        let another_token = yaml.replace("user: {}", "user: {token: another}");
        let another_user = yaml
            .replace("name: u\n", "name: w\n")
            .replace("user: u\n", "user: w\n");
        for (changed, server) in [
            (&another_server, "https://elsewhere"),
            (&another_token, "https://dev"),
            (&another_user, "https://dev"),
        ] {
            assert_ne!(changed, &yaml);
            let dir = tempfile::tempdir().unwrap();
            let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
            manager.unlocks.unlock("dev", "https://dev");
            let pin = manager.contexts.pin_confirmed("dev");
            let path = dir.path().join("first");
            rewrite_file(&mut manager, &path, UNREADABLE).await;
            rewrite_file(&mut manager, &path, changed).await;

            assert!(
                !manager.unlocks.is_unlocked("dev", "https://dev"),
                "{changed}"
            );
            assert!(manager.write_pin_gate("dev", &pin).is_err(), "{changed}");
            // Unlocked again, the review made before still does not hold.
            manager.unlocks.unlock("dev", server);
            assert!(manager.write_pin_gate("dev", &pin).is_err(), "{changed}");
        }
    }

    // covers UP-06, UP-54: the same when a later file's `dev` shows while the first is broken,
    // so the slot is never held: the first file back with another token ends the unlock.
    #[tokio::test]
    async fn a_file_back_with_another_token_after_a_fallback_keeps_no_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let fallback = yaml.replace("user: {}", "user: {token: fallback}");
        let mut manager = manager_reading_two(dir.path(), &yaml, &fallback).await;
        manager.unlocks.unlock("dev", "https://dev");
        let path = dir.path().join("first");
        rewrite_file(&mut manager, &path, UNREADABLE).await;
        assert!(!manager.held("dev"), "the fallback shows");
        let another = yaml.replace("user: {}", "user: {token: another}");
        rewrite_file(&mut manager, &path, &another).await;
        assert!(!manager.unlocks.is_unlocked("dev", "https://dev"));
    }

    // covers UP-55: every way a scope can let agents read more, cluster-scoped kinds included.
    #[test]
    fn a_scope_change_is_widening_unless_it_only_shrinks() {
        let scope = |namespaces: &[&str], kinds: &[&str]| {
            let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect();
            AiScope::new(names(namespaces), names(kinds))
        };
        let shrinks = [
            (scope(&[], &[]), scope(&[], &[])),
            (scope(&[], &[]), scope(&["a"], &[])),
            (scope(&[], &[]), scope(&[], &["pods"])),
            (scope(&[], &[]), scope(&["a"], &["nodes"])),
            (scope(&["a", "b"], &[]), scope(&["a"], &[])),
            (scope(&[], &["nodes", "pods"]), scope(&["a"], &["nodes"])),
            (scope(&["a"], &["nodes", "pods"]), scope(&["a"], &["pods"])),
        ];
        for (from, to) in shrinks {
            assert!(!scope_widens(&from, &to), "{from:?} to {to:?}");
        }
        let widens = [
            (scope(&["a"], &[]), scope(&[], &[])),
            (scope(&["a"], &[]), scope(&["a", "b"], &[])),
            (scope(&["a"], &[]), scope(&["b"], &[])),
            (scope(&[], &["pods"]), scope(&[], &[])),
            (scope(&[], &["pods"]), scope(&["a"], &[])),
            (scope(&[], &["pods"]), scope(&[], &["pods", "nodes"])),
            (scope(&["a"], &["pods"]), scope(&["a"], &[])),
            // Closed to cluster-scoped kinds; a named kind may be one.
            (scope(&["a"], &[]), scope(&["a"], &["nodes"])),
            (scope(&["a"], &[]), scope(&["a"], &["pods"])),
        ];
        for (from, to) in widens {
            assert!(scope_widens(&from, &to), "{from:?} to {to:?}");
        }
        let raw = AiScope {
            namespaces: vec![" a".into()],
            kinds: vec![],
        };
        assert!(scope_widens(&scope(&["a"], &[]), &raw), "not comparable");
        assert!(scope_widens(&raw, &scope(&["a"], &[])), "not comparable");
    }

    fn saved(dir: &Path) -> Settings {
        settings::load_settings(&dir.join("config")).unwrap()
    }

    // covers UP-06, UP-55: a later file's fallback takes no new Trust or exposure, at its own
    // server or any other, and a pin it already has can go. With the file back, pins work again.
    #[tokio::test]
    async fn a_fallback_definition_takes_no_new_pin() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let fallback = kubeconfig_with("https://fallback");
        let mut manager = manager_reading_two(dir.path(), &yaml, &fallback).await;
        manager.config.config_dir = dir.path().join("config");
        let first = dir.path().join("first");
        let at = Some("https://dev".to_owned());
        assert!(manager.set_pin(Pins::Trust, "dev".into(), at).await);
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(!manager.held("dev") && manager.unsettled("dev"));

        let at = Some("https://fallback".to_owned());
        assert!(!manager.set_pin(Pins::Trust, "dev".into(), at.clone()).await);
        let expose = Pins::AiExposure(AiScope::default());
        assert!(!manager.set_pin(expose, "dev".into(), at).await);
        let kept = saved(dir.path());
        assert_eq!(kept.trusted_clusters.len(), 1, "only the earlier pin");
        assert_eq!(kept.trusted_clusters[0].server, "https://dev");
        assert!(kept.ai_exposed_clusters.is_empty());

        assert!(manager.set_pin(Pins::Trust, "dev".into(), None).await);
        assert!(saved(dir.path()).trusted_clusters.is_empty());

        rewrite_file(&mut manager, &first, &yaml).await;
        assert!(!manager.unsettled("dev"));
        let at = Some("https://dev".to_owned());
        assert!(manager.set_pin(Pins::Trust, "dev".into(), at.clone()).await);
        let expose = Pins::AiExposure(AiScope::default());
        assert!(manager.set_pin(expose, "dev".into(), at).await);
        let back = saved(dir.path());
        assert_eq!(back.trusted_clusters.len(), 1);
        assert_eq!(back.ai_exposed_clusters.len(), 1);
    }

    // covers UP-06, UP-55: on a same-server fallback a scope can shrink and not grow; with the
    // file back it can grow again.
    #[tokio::test]
    async fn a_fallback_definition_only_narrows_its_ai_scope() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let fallback = yaml.replace("user: {}", "user: {token: fallback}");
        let mut manager = manager_reading_two(dir.path(), &yaml, &fallback).await;
        manager.config.config_dir = dir.path().join("config");
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let wide = AiScope::new(names(&["a", "b"]), Vec::new());
        let narrow = AiScope::new(names(&["a"]), Vec::new());
        let server = "https://dev".to_owned();
        let expose = Pins::AiExposure(wide.clone());
        assert!(
            manager
                .set_pin(expose, "dev".into(), Some(server.clone()))
                .await
        );
        let first = dir.path().join("first");
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(!manager.held("dev") && manager.unsettled("dev"));

        let everything = AiScope::default();
        manager
            .set_ai_scope("dev".into(), server.clone(), wide.clone(), everything)
            .await;
        assert_eq!(saved(dir.path()).ai_exposed_clusters[0].scope(), wide);

        manager
            .set_ai_scope("dev".into(), server.clone(), wide.clone(), narrow.clone())
            .await;
        assert_eq!(saved(dir.path()).ai_exposed_clusters[0].scope(), narrow);

        rewrite_file(&mut manager, &first, &yaml).await;
        manager
            .set_ai_scope("dev".into(), server, narrow, wide.clone())
            .await;
        assert_eq!(saved(dir.path()).ai_exposed_clusters[0].scope(), wide);

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(
            manager
                .set_pin(Pins::AiExposure(wide), "dev".into(), None)
                .await
        );
        assert!(saved(dir.path()).ai_exposed_clusters.is_empty());
    }

    // covers UP-06, UP-55: what gives access reads the files as they are now, before any reload
    // has seen them. Narrowing and removing need no read. An unchanged file allows it all.
    #[tokio::test]
    async fn an_access_increase_reads_the_files_first() {
        let yaml = kubeconfig_with("https://dev");
        let changes = [
            UNREADABLE.to_owned(),
            kubeconfig_with("https://moved"),
            yaml.replace("user: {}", "user: {token: another}"),
            NO_CONTEXTS.to_owned(),
        ];
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let wide = AiScope::new(names(&["a", "b"]), Vec::new());
        let narrow = AiScope::new(names(&["a"]), Vec::new());
        let server = "https://dev".to_owned();
        for changed in changes {
            let dir = tempfile::tempdir().unwrap();
            let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
            manager.config.config_dir = dir.path().join("config");
            let other = Some("https://other".to_owned());
            assert!(manager.set_pin(Pins::Trust, "other".into(), other).await);
            let expose = Pins::AiExposure(wide.clone());
            let at = Some(server.clone());
            assert!(manager.set_pin(expose, "dev".into(), at.clone()).await);
            let before = saved(dir.path());

            let first = dir.path().join("first");
            std::fs::write(&first, &changed).unwrap();
            assert!(!manager.unsettled("dev"), "no load has seen the change");

            assert!(!manager.set_pin(Pins::Trust, "dev".into(), at.clone()).await);
            let everything = Pins::AiExposure(AiScope::default());
            assert!(!manager.set_pin(everything, "dev".into(), at.clone()).await);
            manager
                .set_ai_scope(
                    "dev".into(),
                    server.clone(),
                    wide.clone(),
                    AiScope::default(),
                )
                .await;
            assert_eq!(saved(dir.path()), before, "nothing saved");

            manager
                .set_ai_scope("dev".into(), server.clone(), wide.clone(), narrow.clone())
                .await;
            assert_eq!(saved(dir.path()).ai_exposed_clusters[0].scope(), narrow);
            let exposure = Pins::AiExposure(narrow.clone());
            assert!(manager.set_pin(exposure, "dev".into(), None).await);
            assert!(manager.set_pin(Pins::Trust, "other".into(), None).await);
            let cleared = saved(dir.path());
            assert!(cleared.ai_exposed_clusters.is_empty());
            assert!(cleared.trusted_clusters.is_empty());

            let new_pin = Pins::AiExposure(narrow.clone());
            assert!(!manager.set_pin(new_pin, "dev".into(), at.clone()).await);
            assert!(saved(dir.path()).ai_exposed_clusters.is_empty());

            std::fs::write(&first, &yaml).unwrap();
            assert!(manager.set_pin(Pins::Trust, "dev".into(), at.clone()).await);
            let new_pin = Pins::AiExposure(narrow.clone());
            assert!(manager.set_pin(new_pin, "dev".into(), at).await);
            manager
                .set_ai_scope("dev".into(), server.clone(), narrow.clone(), wide.clone())
                .await;
            let allowed = saved(dir.path());
            assert_eq!(allowed.trusted_clusters.len(), 1);
            assert_eq!(allowed.ai_exposed_clusters[0].scope(), wide);
        }
    }

    // covers UP-55: an unlock lasts through a confirmed removal and re-add on the same server.
    // A review made before does not.
    #[tokio::test]
    async fn a_context_removed_and_added_back_keeps_its_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        manager.unlocks.unlock("dev", "https://dev");
        let pin = manager.contexts.pin_confirmed("dev");
        let path = dir.path().join("first");
        rewrite_file(&mut manager, &path, NO_CONTEXTS).await;
        assert!(manager.contexts.removed("dev"));
        let another = yaml.replace("user: {}", "user: {token: another}");
        rewrite_file(&mut manager, &path, &another).await;

        assert!(manager.unlocks.is_unlocked("dev", "https://dev"));
        assert!(manager.write_pin_gate("dev", &pin).is_err());
    }

    // covers UP-06: selecting a held context starts no connection; the load that shows it again
    // does.
    #[tokio::test]
    async fn a_held_context_connects_once_its_file_is_back() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://127.0.0.1:1");
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        let path = dir.path().join("first");
        rewrite_file(&mut manager, &path, UNREADABLE).await;

        manager.activate("dev".to_owned());
        let slot = manager.cluster("dev").unwrap();
        assert_eq!(slot.state, ConnectionState::AwaitingConfirmation);
        let attempt = slot.attempt;

        rewrite_file(&mut manager, &path, &yaml).await;
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_ne!(slot.attempt, attempt);
    }

    /// What the second file holds for each way a context pauses when the first breaks: another
    /// definition (a fresh, provisional slot), or nothing (the slot stays, held).
    fn pauses() -> [String; 2] {
        [
            kubeconfig_with("https://127.0.0.1:3"),
            NO_CONTEXTS.to_owned(),
        ]
    }

    /// A manager on two files with `dev` at an address nothing listens on, its first file broken
    /// when `pause` is called.
    async fn pausable(dir: &Path, second: &str) -> (ClusterManager, String, PathBuf) {
        let yaml = kubeconfig_with("https://127.0.0.1:1");
        let manager = manager_reading_two(dir, &yaml, second).await;
        (manager, yaml, dir.join("first"))
    }

    /// Delivers a finished connection for `dev`'s current attempt, built without contacting
    /// anything, and leaves no discovery marked as running.
    async fn land_connection(manager: &mut ClusterManager) {
        let attempt = manager.next_attempt;
        manager.next_attempt += 1;
        let slot = manager.cluster_mut("dev").unwrap();
        slot.attempt = attempt;
        slot.state = ConnectionState::Connecting;
        let result = Ok((built_connection(manager).await, test_version()));
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result,
            })
            .await;
        manager.cluster_mut("dev").unwrap().discovering = false;
    }

    async fn built_connection(manager: &ClusterManager) -> cluster::GatedConnection {
        let access = crate::connection::policy::CredentialAccess::always();
        let built =
            cluster::connect_gated_with_policy(&manager.kubeconfig.merged, "dev", &access).await;
        let Ok(connection) = built else {
            panic!("building a client contacts nothing");
        };
        connection
    }

    fn test_version() -> ServerVersion {
        ServerVersion {
            git_version: "v1.30.0".into(),
            platform: "linux/arm64".into(),
        }
    }

    /// Brings the feeds in line, as the manager's loop does after each message, and says whether
    /// `dev` has one.
    fn dev_has_feed(manager: &mut ClusterManager) -> bool {
        manager.sync_telemetry();
        let feed = manager.cluster("dev").unwrap().telemetry.is_some();
        assert_eq!(manager.telemetry.get("dev").is_some(), feed);
        feed
    }

    #[test]
    fn only_a_user_with_a_token_file_counts_as_reading_one() {
        let config = kube::config::Kubeconfig::from_yaml(
            "
apiVersion: v1
kind: Config
contexts:
- name: file
  context: {cluster: c, user: with-file}
- name: inline
  context: {cluster: c, user: with-token}
users:
- name: with-file
  user: {tokenFile: /nonexistent/token}
- name: with-token
  user: {token: abc}
",
        )
        .unwrap();
        assert!(telemetry::reads_token_file(&config, "file"));
        assert!(!telemetry::reads_token_file(&config, "inline"));
        assert!(!telemetry::reads_token_file(&config, "missing"));
    }

    // covers UP-25: On view watches only open views, so the feed follows the cluster's mode.
    #[tokio::test]
    async fn a_feed_runs_only_in_watch_mode_all() {
        use crate::settings::WatchMode;
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        land_connection(&mut manager).await;
        manager.active = Some("dev".to_owned());
        assert!(dev_has_feed(&mut manager), "the default mode");

        manager.settings = Arc::new(Settings {
            watch_mode: WatchMode::OnView,
            ..Settings::default()
        });
        assert!(!dev_has_feed(&mut manager), "on view by default");

        manager.settings = Arc::new(Settings {
            watch_mode: WatchMode::OnView,
            cluster_watch_mode: [("dev".to_owned(), WatchMode::All)].into(),
            ..Settings::default()
        });
        assert!(dev_has_feed(&mut manager), "all for this cluster");

        manager.settings = Arc::new(Settings {
            cluster_watch_mode: [("dev".to_owned(), WatchMode::OnView)].into(),
            ..Settings::default()
        });
        assert!(!dev_has_feed(&mut manager), "on view for this cluster");
    }

    /// `dev` connected and active, in watch mode On view.
    async fn on_view_manager(dir: &std::path::Path) -> ClusterManager {
        let mut manager = manager_reading(dir, &kubeconfig_with("https://127.0.0.1:1")).await;
        land_connection(&mut manager).await;
        manager.active = Some("dev".to_owned());
        manager.settings = Arc::new(Settings {
            watch_mode: crate::settings::WatchMode::OnView,
            ..Settings::default()
        });
        manager
    }

    fn open_dev_overview(manager: &mut ClusterManager, id: u64) {
        manager.handle_request(Request::OpenOverview {
            id,
            cluster: "dev".to_owned(),
        });
    }

    // covers UP-25: the overview is an open view of the kinds its feed watches.
    #[tokio::test]
    async fn in_on_view_a_feed_runs_only_while_the_overview_is_showing() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = on_view_manager(dir.path()).await;
        assert!(!dev_has_feed(&mut manager), "no overview open");

        open_dev_overview(&mut manager, 1);
        assert!(dev_has_feed(&mut manager), "opened");
        open_dev_overview(&mut manager, 2);
        manager.handle_request(Request::CloseOverview { id: 1 });
        assert!(dev_has_feed(&mut manager), "another is still open");
        manager.handle_request(Request::CloseOverview { id: 2 });
        assert!(!dev_has_feed(&mut manager), "left");
        assert!(manager.overviews.is_empty());
    }

    #[tokio::test]
    async fn an_overview_feed_stops_with_the_active_cluster_and_generation() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = on_view_manager(dir.path()).await;
        open_dev_overview(&mut manager, 1);
        assert!(dev_has_feed(&mut manager));

        // Even a running-set member needs to be the cluster shown.
        join_dev(&mut manager);
        manager.active = Some("other".to_owned());
        assert!(!dev_has_feed(&mut manager), "switched away");
        manager.active = Some("dev".to_owned());
        assert!(dev_has_feed(&mut manager), "back on the same generation");

        manager.cluster_mut("dev").unwrap().generation += 1;
        assert!(!dev_has_feed(&mut manager), "the context started over");
    }

    #[tokio::test]
    async fn an_open_overview_starts_no_feed_on_a_paused_cluster() {
        for second in pauses() {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, _, first) = pausable(dir.path(), &second).await;
            manager.active = Some("dev".to_owned());
            land_connection(&mut manager).await;
            manager.settings = Arc::new(Settings {
                watch_mode: crate::settings::WatchMode::OnView,
                ..Settings::default()
            });
            open_dev_overview(&mut manager, 1);
            assert!(dev_has_feed(&mut manager));

            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(manager.paused("dev"));
            assert!(!dev_has_feed(&mut manager), "stopped by the pause");
            open_dev_overview(&mut manager, 2);
            assert!(!dev_has_feed(&mut manager), "opening starts nothing");
        }
    }

    #[tokio::test]
    async fn a_feed_runs_only_for_a_wanted_connected_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        assert!(!dev_has_feed(&mut manager), "not connected");

        land_connection(&mut manager).await;
        assert!(!dev_has_feed(&mut manager), "connected but not wanted");

        manager.active = Some("dev".to_owned());
        assert!(dev_has_feed(&mut manager), "selected");
        let first = manager.cluster("dev").unwrap().attempt;

        manager.active = Some("other".to_owned());
        assert!(!dev_has_feed(&mut manager), "switched away");

        join_dev(&mut manager);
        assert!(dev_has_feed(&mut manager), "a running-set member");

        land_connection(&mut manager).await;
        assert_ne!(manager.cluster("dev").unwrap().attempt, first);
        assert!(dev_has_feed(&mut manager), "follows the new connection");

        manager.cluster_mut("dev").unwrap().clear_connection();
        assert!(manager.cluster("dev").unwrap().telemetry.is_none());
        manager.cluster_mut("dev").unwrap().state = ConnectionState::Disconnected;
        assert!(!dev_has_feed(&mut manager), "disconnected");
    }

    #[tokio::test]
    async fn a_pause_stops_the_feed() {
        for second in pauses() {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, _, first) = pausable(dir.path(), &second).await;
            manager.active = Some("dev".to_owned());
            land_connection(&mut manager).await;
            assert!(dev_has_feed(&mut manager));

            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(manager.paused("dev"));
            // The load itself stopped it, ahead of the loop's next sync.
            assert!(manager.cluster("dev").unwrap().telemetry.is_none());
            assert!(manager.telemetry.get("dev").is_none());
            assert!(!dev_has_feed(&mut manager));
        }
    }

    #[tokio::test]
    async fn no_feed_is_left_after_a_context_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        manager.active = Some("dev".to_owned());
        land_connection(&mut manager).await;
        assert!(dev_has_feed(&mut manager));

        rewrite(&mut manager, dir.path(), NO_CONTEXTS).await;
        assert!(manager.cluster("dev").is_none());
        assert!(manager.telemetry.get("dev").is_none());
        assert!(manager.telemetry.all().is_empty());
    }

    #[tokio::test]
    async fn a_background_member_whose_sign_in_runs_a_program_has_no_feed() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        manager.active = Some("other".to_owned());
        join_dev(&mut manager);
        manager.sync_roles();
        land_connection(&mut manager).await;
        assert!(dev_has_feed(&mut manager), "a static credential may run");

        command_credential(&mut manager);
        assert!(!dev_has_feed(&mut manager));
    }

    #[tokio::test]
    async fn a_revoked_command_credential_has_no_feed_even_when_selected() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        manager.active = Some("dev".to_owned());
        manager.sync_roles();
        land_connection(&mut manager).await;
        command_credential(&mut manager);
        assert!(dev_has_feed(&mut manager));

        manager.cluster("dev").unwrap().access.revoke();
        assert!(!dev_has_feed(&mut manager));
    }

    /// Makes `dev` a member of the running set, joined for the server it has now.
    fn join_dev(manager: &mut ClusterManager) {
        let server = manager.cluster("dev").unwrap().info.server.clone().unwrap();
        let (member, _) = Member::join(ServerIdentity::new(server), None, None);
        manager.members.insert("dev".to_owned(), member);
    }

    /// Gives `dev` a connection and discovery with one kind, in on-view mode, with a view open on
    /// that kind.
    async fn watching_dev(manager: &mut ClusterManager) -> KindId {
        use crate::discovery::tests::kind;
        let kinds = vec![kind("", "Pod", "pods", true, &["get", "list", "watch"])];
        let pods = kinds[0].id();
        manager.settings = Arc::new(Settings {
            watch_mode: crate::settings::WatchMode::OnView,
            ..Settings::default()
        });
        land_connection(manager).await;
        let slot = manager.cluster_mut("dev").unwrap();
        slot.watches.set_kinds(&kinds);
        slot.discovery = Some(Arc::new(ClusterDiscovery::new(kinds)));
        manager.handle_request(Request::Subscribe {
            id: 1,
            cluster: "dev".into(),
            kind: pods.clone(),
        });
        pods
    }

    fn watches(manager: &ClusterManager, kind: &KindId) -> bool {
        manager.watched_kinds("dev").iter().any(|(k, _)| k == kind)
    }

    // With no members, only the active cluster runs watches.
    #[tokio::test]
    async fn without_members_only_the_active_cluster_keeps_watches() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        let pods = watching_dev(&mut manager).await;
        assert!(manager.wants_watches("dev"));
        assert!(watches(&manager, &pods));

        manager.active = None;
        manager.reconcile("dev");
        assert!(!manager.wants_watches("dev"));
        assert!(!watches(&manager, &pods), "stopped with the switch");
    }

    #[tokio::test]
    async fn a_member_keeps_watches_when_it_is_not_active() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        join_dev(&mut manager);
        let pods = watching_dev(&mut manager).await;

        manager.active = None;
        manager.reconcile("dev");
        assert!(manager.wants_watches("dev"));
        assert!(watches(&manager, &pods), "a member goes on");
    }

    #[tokio::test]
    async fn a_removed_members_watches_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        let pods = watching_dev(&mut manager).await;
        assert!(watches(&manager, &pods));

        manager.members.remove("dev");
        manager.reconcile("dev");
        assert!(!watches(&manager, &pods));
    }

    // A confirmed move to another server ends the membership, and nothing connects there.
    #[tokio::test]
    async fn a_repointed_member_is_removed_without_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, first) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        let pods = watching_dev(&mut manager).await;
        let next = manager.next_attempt;

        rewrite_file(
            &mut manager,
            &first,
            &kubeconfig_with("https://127.0.0.1:2"),
        )
        .await;
        assert!(!manager.members.contains_key("dev"));
        assert!(!watches(&manager, &pods));
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Disconnected));
        assert_eq!(manager.next_attempt, next, "no attempt started");
    }

    // A member whose context left the kubeconfig is no longer a member.
    #[tokio::test]
    async fn a_member_whose_context_is_gone_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, first) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        rewrite_file(&mut manager, &first, NO_CONTEXTS).await;
        assert!(manager.cluster("dev").is_none());
        assert!(!manager.members.contains_key("dev"));
    }

    // A held member keeps what runs and starts no connection, discovery or watch.
    #[tokio::test]
    async fn a_held_member_starts_nothing() {
        use crate::discovery::tests::kind;
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, first) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        let pods = watching_dev(&mut manager).await;
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.held("dev"));
        assert!(manager.members.contains_key("dev"), "a held slot stays");
        assert!(watches(&manager, &pods), "what runs goes on");

        let configmaps = kind(
            "",
            "ConfigMap",
            "configmaps",
            true,
            &["get", "list", "watch"],
        );
        let slot = manager.cluster_mut("dev").unwrap();
        let mut kinds = vec![configmaps.clone()];
        kinds.extend(slot.discovery.as_ref().unwrap().kinds.iter().cloned());
        slot.watches.set_kinds(&kinds);
        manager.handle_request(Request::Subscribe {
            id: 2,
            cluster: "dev".into(),
            kind: configmaps.id(),
        });
        assert!(!watches(&manager, &configmaps.id()), "no new watch");
        assert!(watches(&manager, &pods), "subscribing stops nothing");

        let next = manager.next_attempt;
        manager
            .handle_internal(Internal::RetryDiscovery {
                cluster: "dev".into(),
            })
            .await;
        // Disconnected, only the pause keeps this from starting an attempt.
        manager.cluster_mut("dev").unwrap().state = ConnectionState::Disconnected;
        manager.ensure_connected("dev");
        let slot = manager.cluster("dev").unwrap();
        assert!(!slot.discovering, "no discovery");
        assert!(matches!(slot.state, ConnectionState::Disconnected));
        assert_eq!(manager.next_attempt, next, "no attempt started");
    }

    // A member queued to connect when its context moves unconfirmed waits, however often the set
    // is looked at, and connects once the context shows its server again.
    #[tokio::test]
    async fn a_queued_member_never_connects_to_an_unconfirmed_server() {
        let dir = tempfile::tempdir().unwrap();
        let second = dir.path().join("second");
        let at_a = kubeconfig_with("https://127.0.0.1:1");
        let at_b = kubeconfig_with("https://127.0.0.1:2");
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &at_a).await;
        join_dev(&mut manager);
        manager.schedule.enqueue("dev".to_owned());
        let next = manager.next_attempt;

        rewrite_file(&mut manager, &second, &at_b).await;
        manager.tick_members(Instant::now() + Duration::from_secs(5));
        assert!(manager.members["dev"].paused().is_some());
        assert_eq!(manager.next_attempt, next, "no attempt on the fallback");

        rewrite_file(&mut manager, &second, &at_a).await;
        assert!(manager.members["dev"].paused().is_none());
        assert_eq!(manager.next_attempt, next + 1, "connects on its server");
    }

    // First seen behind a broken file, `dev` has no confirmed definition. A move to another server
    // may be a fallback: the member stays and starts nothing there, comes back with its server,
    // and ends only when a load confirms the move.
    #[tokio::test]
    async fn an_unconfirmed_move_keeps_a_member_until_a_load_confirms_it() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        let at_a = kubeconfig_with("https://127.0.0.1:1");
        let at_b = kubeconfig_with("https://127.0.0.1:2");
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &at_a).await;
        join_dev(&mut manager);
        let next = manager.next_attempt;

        rewrite_file(&mut manager, &second, &at_b).await;
        assert!(manager.contexts.provisional("dev") && !manager.paused("dev"));
        assert!(
            manager.members.contains_key("dev"),
            "the move is unconfirmed"
        );
        assert!(
            !manager.wants_watches("dev"),
            "nothing runs on the fallback"
        );
        assert!(manager.watched_kinds("dev").is_empty());
        assert_eq!(manager.next_attempt, next, "no attempt started");

        rewrite_file(&mut manager, &second, &at_a).await;
        assert!(manager.wants_watches("dev"), "back on its server");

        rewrite_file(&mut manager, &second, &at_b).await;
        rewrite_file(&mut manager, &first, NO_CONTEXTS).await;
        assert!(!manager.contexts.provisional("dev"));
        assert!(
            !manager.members.contains_key("dev"),
            "the move is confirmed"
        );
    }

    // Switching away during an outage drops the rows but not the error, so coming back says
    // reconnecting rather than loading (UP-27).
    #[tokio::test]
    async fn switching_away_keeps_the_outage_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        let pods = watching_dev(&mut manager).await;
        let state = SyncState::Reconnecting {
            error: Some("dropped".into()),
        };
        let hub = manager.hubs.get_or_create("dev", &pods);
        hub.take_over(KindSnapshot::empty(
            "dev".into(),
            pods.clone(),
            state.clone(),
        ));

        manager.active = Some("other".to_owned());
        manager.reconcile("dev");
        assert!(!watches(&manager, &pods));

        manager.active = Some("dev".to_owned());
        manager.reconcile("dev");
        let hub = manager.hubs.get_or_create("dev", &pods);
        assert_eq!(hub.latest().state, state);
    }

    // With no confirmed definition, the watches of a cluster that was active on the moved server
    // stop with the switch.
    #[tokio::test]
    async fn a_member_moved_while_never_confirmed_keeps_no_watches() {
        let dir = tempfile::tempdir().unwrap();
        let second = dir.path().join("second");
        let at_a = kubeconfig_with("https://127.0.0.1:1");
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &at_a).await;
        assert!(manager.contexts.provisional("dev") && !manager.paused("dev"));
        join_dev(&mut manager);

        rewrite_file(
            &mut manager,
            &second,
            &kubeconfig_with("https://127.0.0.1:2"),
        )
        .await;
        assert!(manager.contexts.provisional("dev") && !manager.paused("dev"));
        assert!(manager.members.contains_key("dev"));

        manager.active = Some("dev".to_owned());
        let pods = watching_dev(&mut manager).await;
        assert!(watches(&manager, &pods));
        manager.active = None;
        manager.reconcile("dev");
        assert!(!manager.wants_watches("dev"));
        assert!(!watches(&manager, &pods), "stopped with the switch");
    }

    // A member bound to another server wants nothing, and the load that removes it stops what
    // still runs on the slot.
    #[tokio::test]
    async fn removing_a_stale_member_stops_its_watches() {
        let dir = tempfile::tempdir().unwrap();
        let at_b = kubeconfig_with("https://127.0.0.1:2");
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &at_b).await;
        manager.active = Some("dev".to_owned());
        let pods = watching_dev(&mut manager).await;
        let (member, _) = Member::join(ServerIdentity::new("https://127.0.0.1:1"), None, None);
        manager.members.insert("dev".to_owned(), member);
        manager.active = None;
        assert!(!manager.wants_watches("dev"), "bound to another server");
        assert!(watches(&manager, &pods));

        rewrite_file(&mut manager, &dir.path().join("first"), NO_CONTEXTS).await;
        assert!(!manager.contexts.provisional("dev"));
        assert!(!manager.members.contains_key("dev"));
        assert!(!watches(&manager, &pods), "stopped with the member");
    }

    // A removed member that is the active cluster keeps its watches.
    #[tokio::test]
    async fn removing_a_stale_member_leaves_the_active_cluster_running() {
        let dir = tempfile::tempdir().unwrap();
        let at_b = kubeconfig_with("https://127.0.0.1:2");
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &at_b).await;
        manager.active = Some("dev".to_owned());
        let pods = watching_dev(&mut manager).await;
        let (member, _) = Member::join(ServerIdentity::new("https://127.0.0.1:1"), None, None);
        manager.members.insert("dev".to_owned(), member);

        rewrite_file(&mut manager, &dir.path().join("first"), NO_CONTEXTS).await;
        assert!(!manager.members.contains_key("dev"));
        assert!(watches(&manager, &pods));
    }

    // A fallback at another server is a real pause: the member keeps its place, wants nothing
    // and starts nothing, and counts again once the load shows its server.
    #[tokio::test]
    async fn a_paused_member_on_a_fallback_server_keeps_its_place() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = kubeconfig_with("https://127.0.0.1:2");
        let (mut manager, confirmed, first) = pausable(dir.path(), &fallback).await;
        join_dev(&mut manager);
        let next = manager.next_attempt;

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.contexts.provisional("dev") && manager.paused("dev"));
        assert!(manager.members.contains_key("dev"), "a pause keeps it");
        assert!(!manager.wants_watches("dev"));
        assert!(manager.watched_kinds("dev").is_empty());

        rewrite_file(&mut manager, &first, &confirmed).await;
        assert!(manager.members.contains_key("dev"));
        assert!(manager.wants_watches("dev"));
        assert_eq!(
            manager.next_attempt,
            next + 1,
            "connects once the pause ends"
        );
    }

    // A connection result handled during a hold is dropped, and the load that ends the hold
    // connects the member again even though it is not the active cluster.
    #[tokio::test]
    async fn a_member_whose_connection_a_hold_dropped_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        let attempt = manager.next_attempt;
        manager.next_attempt += 1;
        let slot = manager.cluster_mut("dev").unwrap();
        slot.attempt = attempt;
        slot.state = ConnectionState::Connecting;

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.held("dev"));
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Err(cluster::SafeErrorText::fixed("refused")),
            })
            .await;
        let slot = manager.cluster("dev").unwrap();
        assert_eq!(slot.state, ConnectionState::AwaitingConfirmation);

        let next = manager.next_attempt;
        rewrite_file(&mut manager, &first, &confirmed).await;
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_eq!(manager.next_attempt, next + 1, "one attempt started");
    }

    // A kubeconfig link moved to a file with a new credential for the same server reaches a
    // member that is not active: its slot is replaced and connects again.
    #[tokio::test]
    async fn a_moved_link_reconnects_a_background_member() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        let pods = watching_dev(&mut manager).await;
        let generation = manager.cluster("dev").unwrap().generation;

        let moved = dir.path().join("moved");
        std::fs::write(
            &moved,
            confirmed.replace("user: {}", "user: {token: rotated}"),
        )
        .unwrap();
        point_link(&first, &moved);
        let next = manager.next_attempt;
        manager.reload_kubeconfig().await;

        assert!(manager.members.contains_key("dev"));
        assert!(
            !watches(&manager, &pods),
            "the old connection's watch stopped"
        );
        let slot = manager.cluster("dev").unwrap();
        assert_ne!(slot.generation, generation);
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_eq!(manager.next_attempt, next + 1, "one attempt started");
    }

    /// Makes `dev` a live member with a connection, as if it had connected through its attempt.
    async fn live_dev(manager: &mut ClusterManager) -> KindId {
        join_dev(manager);
        let pods = watching_dev(manager).await;
        let attempt = manager.schedule.start_active(&"dev".to_owned());
        let member = manager.members.get_mut("dev").unwrap();
        assert!(member.begin_attempt(attempt));
        let finished = MemberEvent::AttemptFinished {
            attempt,
            outcome: AttemptOutcome::Connected,
        };
        manager.apply_member("dev", finished, Instant::now());
        pods
    }

    fn dev_state(manager: &ClusterManager) -> MemberState {
        manager.members["dev"].state().clone()
    }

    // A connection made before the set existed keeps its credential's rules when the kubeconfig
    // that defined it can no longer be read: moved to the background, it ends as signed out.
    #[tokio::test]
    async fn a_retained_command_credential_stops_while_its_kubeconfig_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, first) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        let pods = live_dev(&mut manager).await;
        let slot = manager.cluster_mut("dev").unwrap();
        let grant = slot.access.grant(CredentialSource::Plugin);
        slot.write_credential = Some(WriteCredential::with_grant(
            Vec::new(),
            CredentialSource::Plugin,
            None,
            None,
            grant.clone(),
            None,
        ));

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.held("dev"));
        assert!(manager.runs_a_command("dev"));
        manager.activate("other".to_owned());

        assert!(!grant.allowed(), "the retained credential may not run");
        assert!(matches!(
            dev_state(&manager),
            MemberState::SignInNeeded { .. }
        ));
        let slot = manager.cluster("dev").unwrap();
        assert!(slot.client.is_none() && slot.write_credential.is_none());
        assert!(!watches(&manager, &pods));
    }

    // With no connection to ask, what can't be known counts as running a command.
    #[tokio::test]
    async fn an_unknown_credential_counts_as_running_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        assert!(!manager.runs_a_command("dev"), "an empty user runs nothing");
        assert!(manager.runs_a_command("missing"));

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.runs_a_command("dev"), "its file can't be read");

        let no_user = confirmed.replace("- name: u\n  user: {}\n", "- name: x\n  user: {}\n");
        rewrite_file(&mut manager, &first, &no_user).await;
        assert!(manager.runs_a_command("dev"), "it names a user not defined");
    }

    // A legacy auth provider's command runs inside kube and can't be stopped once started, so a
    // context using one neither connects nor refreshes unless it is in the foreground.
    #[tokio::test]
    async fn a_provider_command_never_runs_for_a_background_member() {
        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("ran");
        let script = dir.path().join("provider.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\necho '{{\"token\":\"t\"}}'\n",
                ran.display()
            ),
        )
        .unwrap();
        let provider = format!(
            "user:\n    auth-provider:\n      name: gcp\n      config:\n        cmd-path: /bin/sh\n        \
             cmd-args: {}\n        token-key: '{{.token}}'\n",
            script.display()
        );
        let yaml = kubeconfig_with("https://127.0.0.1:1").replace("user: {}\n", &provider);
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        assert!(manager.runs_a_command("dev"));

        manager.set_running_set(vec!["dev".to_owned(), "other".to_owned()]);
        for id in ["dev", "other"] {
            assert!(matches!(
                manager.members[id].state(),
                MemberState::SignInNeeded { .. }
            ));
            assert!(manager.cluster(id).unwrap().connect_task.is_none());
        }

        // Nor does a connection asked for directly under the background role.
        let access = manager.cluster("dev").unwrap().access.clone();
        let refused =
            cluster::connect_gated_with_policy(&manager.kubeconfig.merged, "dev", &access).await;
        let Err(error) = refused else {
            panic!("a background connection must be refused");
        };
        assert!(running_set::needs_sign_in(&error));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!ran.exists(), "the command never started");
    }

    // A set is admitted whole or not at all, on the exact servers it was checked on.
    #[tokio::test]
    async fn a_stale_pin_admits_no_member_of_the_set() {
        use crate::settings::ClusterPin;
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading(dir.path(), &kubeconfig_with("https://remote-a")).await;
        let other = ClusterPin::new("other", "https://other");
        let refused = manager
            .admit_running_set(vec![
                other.clone(),
                ClusterPin::new("dev", "https://replacement"),
            ])
            .unwrap_err();
        assert!(refused.contains("another server"), "{refused}");
        assert!(manager.members.is_empty(), "nobody joined");

        let admitted = manager
            .admit_running_set(vec![ClusterPin::new("dev", "https://remote-a"), other])
            .unwrap();
        assert_eq!(manager.members.len(), 2);
        assert_eq!(admitted[0].server(), "https://remote-a");
        assert_eq!(manager.member_changed(&admitted[0]), None);
    }

    // Moved and put back, a member's reads stay refused: it is not the connection admitted.
    #[tokio::test]
    async fn a_moved_member_is_refused_before_a_client_even_once_moved_back() {
        use crate::settings::ClusterPin;
        let dir = tempfile::tempdir().unwrap();
        let original = kubeconfig_with("https://remote-a");
        let mut manager = manager_reading(dir.path(), &original).await;
        let admitted = manager
            .admit_running_set(vec![
                ClusterPin::new("dev", "https://remote-a"),
                ClusterPin::new("other", "https://other"),
            ])
            .unwrap();
        let dev = &admitted[0];
        assert!(manager.member_client(dev).is_err(), "not connected yet");

        rewrite(
            &mut manager,
            dir.path(),
            &kubeconfig_with("https://replacement"),
        )
        .await;
        let why = manager.member_changed(dev).expect("moved");
        assert!(why.contains("changed"), "{why}");
        assert_eq!(manager.member_client(dev).err(), Some(why));

        rewrite(&mut manager, dir.path(), &original).await;
        assert!(manager.member_changed(dev).is_some(), "moved back");
        assert_eq!(
            manager.member_changed(&admitted[1]),
            None,
            "other is intact"
        );
    }

    // A member that left its server, even if it came back, gets no watch from a new subscription.
    #[tokio::test]
    async fn a_changed_member_starts_no_subscription() {
        use crate::settings::ClusterPin;
        let dir = tempfile::tempdir().unwrap();
        let original = kubeconfig_with("https://remote-a");
        let mut manager = manager_reading(dir.path(), &original).await;
        let admitted = manager
            .admit_running_set(vec![
                ClusterPin::new("dev", "https://remote-a"),
                ClusterPin::new("other", "https://other"),
            ])
            .unwrap();
        let kind = KindId::pods();
        let feed = || tokio::sync::watch::Sender::new(Arc::default());
        manager.subscribe_member(1, "dev", admitted[0].clone(), kind.clone(), feed());
        assert!(manager.subscriptions.contains_key(&1), "still admitted");

        rewrite(
            &mut manager,
            dir.path(),
            &kubeconfig_with("https://replacement"),
        )
        .await;
        manager.subscribe_member(2, "dev", admitted[0].clone(), kind.clone(), feed());
        rewrite(&mut manager, dir.path(), &original).await;
        manager.subscribe_member(3, "dev", admitted[0].clone(), kind, feed());
        assert!(!manager.subscriptions.contains_key(&2), "moved");
        assert!(!manager.subscriptions.contains_key(&3), "moved back");
    }

    // Seen on a fallback by a load, a member's subscription has ended, even if the next load puts
    // the context back before anything else reconciles it.
    #[tokio::test]
    async fn a_member_subscription_cut_on_a_fallback_stays_cut_once_restored() {
        use crate::settings::ClusterPin;
        let dir = tempfile::tempdir().unwrap();
        let fallback = kubeconfig_with("https://127.0.0.1:2");
        let (mut manager, confirmed, first) = pausable(dir.path(), &fallback).await;
        let admitted = manager
            .admit_running_set(vec![
                ClusterPin::new("dev", "https://127.0.0.1:1"),
                ClusterPin::new("other", "https://other"),
            ])
            .unwrap();
        let pods = KindId::pods();
        let (feed, rows) = tokio::sync::watch::channel(Arc::default());
        manager.subscribe_member(1, "dev", admitted[0].clone(), pods.clone(), feed);

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(!manager.subscriptions.contains_key(&1), "cut by the load");
        rewrite_file(&mut manager, &first, &confirmed).await;
        assert!(!manager.subscriptions.contains_key(&1), "stays cut");

        let hub = manager.hubs.get_or_create("dev", &pods);
        let epoch = hub.take_over(KindSnapshot::empty(
            "dev".into(),
            pods.clone(),
            SyncState::Loading,
        ));
        assert!(hub.publish(
            epoch,
            KindSnapshot::empty("dev".into(), pods, SyncState::Synced)
        ));
        assert_eq!(rows.borrow().state, SyncState::Idle);
    }

    // Opening and closing member views leaves no channel in a hub a plain view keeps open, and
    // a cut-off view leaves no hub behind.
    #[tokio::test]
    async fn closed_and_cut_off_member_views_leave_nothing_in_the_hubs() {
        use crate::settings::ClusterPin;
        let dir = tempfile::tempdir().unwrap();
        let original = kubeconfig_with("https://remote-a");
        let mut manager = manager_reading(dir.path(), &original).await;
        let admitted = manager
            .admit_running_set(vec![
                ClusterPin::new("dev", "https://remote-a"),
                ClusterPin::new("other", "https://other"),
            ])
            .unwrap();
        let pods = KindId::pods();
        let (hub, _plain) = manager.hubs.subscribe("dev", &pods);
        for id in 1..=3 {
            let (feed, rows) = tokio::sync::watch::channel(Arc::default());
            manager.handle_request(Request::SubscribeMember {
                id,
                cluster: "dev".into(),
                member: admitted[0].clone(),
                kind: pods.clone(),
                feed,
            });
            drop(rows);
            manager.handle_request(Request::Unsubscribe { id });
        }
        assert!(hub.member_feeds_empty(), "closed channels stay behind");

        let services = KindId::new("", "services");
        let (feed, _rows) = tokio::sync::watch::channel(Arc::default());
        manager.subscribe_member(4, "dev", admitted[0].clone(), services.clone(), feed);
        assert!(
            manager
                .hubs
                .states("dev")
                .iter()
                .any(|(k, _)| *k == services)
        );
        rewrite(
            &mut manager,
            dir.path(),
            &kubeconfig_with("https://replacement"),
        )
        .await;
        assert!(
            !manager
                .hubs
                .states("dev")
                .iter()
                .any(|(k, _)| *k == services),
            "no watch and no listener left"
        );
    }

    // A reload that replaces the active member's credential on the same server binds the new
    // connection to the member, so its failure is not reported as the old live state.
    #[tokio::test]
    async fn an_active_members_replacement_connection_reports_to_the_member() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        live_dev(&mut manager).await;

        let changed = confirmed.replace("user: {}", "user: {token: rotated}");
        rewrite_file(&mut manager, &first, &changed).await;
        assert_eq!(dev_state(&manager), MemberState::Connecting);
        let slot = manager.cluster("dev").unwrap();
        assert!(slot.member_attempt.is_some(), "bound to the member");
        let attempt = slot.attempt;

        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Err(cluster::SafeErrorText::fixed("refused")),
            })
            .await;
        assert!(matches!(dev_state(&manager), MemberState::Failed { .. }));
    }

    // A result from before a pause is never taken for the connection made after it, even when
    // the file comes back unchanged before the result does.
    #[tokio::test]
    async fn a_pause_voids_a_member_attempt_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        manager.schedule.enqueue("dev".to_owned());
        manager.pump_members(Instant::now());
        let before = manager.cluster("dev").unwrap().attempt;
        assert!(manager.cluster("dev").unwrap().member_attempt.is_some());

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Disconnected));
        assert!(slot.member_attempt.is_none() && slot.connect_task.is_none());
        assert_eq!(manager.schedule.slots_in_use(), 0, "its admission is free");

        rewrite_file(&mut manager, &first, &confirmed).await;
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt: before,
                result: Ok((built_connection(&manager).await, test_version())),
            })
            .await;
        assert_eq!(dev_state(&manager), MemberState::Connecting);
        assert!(manager.cluster("dev").unwrap().client.is_none());
    }

    // A 401 on a background member's connection means signing in, at once. One from while it
    // was in the foreground is not held against it later.
    #[tokio::test]
    async fn a_refused_background_credential_needs_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        manager.sync_roles();
        let pods = live_dev(&mut manager).await;
        let slot = manager.cluster_mut("dev").unwrap();
        let grant = slot.access.grant(CredentialSource::Definition);
        slot.write_credential = Some(WriteCredential::with_grant(
            Vec::new(),
            CredentialSource::Definition,
            None,
            None,
            grant.clone(),
            None,
        ));

        grant.note_refused();
        manager.active = Some("other".to_owned());
        manager.sync_roles();
        manager.tick_members(Instant::now());
        assert_eq!(
            dev_state(&manager),
            MemberState::Live,
            "from the foreground"
        );

        grant.note_refused();
        manager.tick_members(Instant::now());
        assert!(matches!(
            dev_state(&manager),
            MemberState::SignInNeeded { .. }
        ));
        assert!(!watches(&manager, &pods));
    }

    /// Gives `dev`'s current connection a command credential under the slot's own access.
    fn command_credential(
        manager: &mut ClusterManager,
    ) -> crate::connection::policy::CredentialGrant {
        let slot = manager.cluster_mut("dev").unwrap();
        let grant = slot.access.grant(CredentialSource::Plugin);
        slot.write_credential = Some(WriteCredential::with_grant(
            Vec::new(),
            CredentialSource::Plugin,
            None,
            None,
            grant.clone(),
            None,
        ));
        grant
    }

    // A client kept by a task outlives its slot: replacing the slot ends its permission, and
    // the new slot's activation does not bring it back.
    #[tokio::test]
    async fn replacing_a_slot_revokes_the_old_connections_command_permission() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        manager.sync_roles();
        live_dev(&mut manager).await;
        let old = command_credential(&mut manager);
        let held_by_a_task = old.clone();
        assert!(old.allowed());

        let changed = confirmed.replace("user: {}", "user: {token: rotated}");
        rewrite_file(&mut manager, &first, &changed).await;
        assert!(!held_by_a_task.allowed(), "the old connection is revoked");

        manager.activate("other".to_owned());
        manager.activate("dev".to_owned());
        assert!(!held_by_a_task.allowed(), "and stays revoked");
    }

    // A hold stops new commands, leaves the running connection and watch alone, and ends with
    // the same connection able to refresh. A use begun before the hold does not come back.
    #[tokio::test]
    async fn a_hold_blocks_commands_but_keeps_running_reads() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        let pods = live_dev(&mut manager).await;
        let grant = command_credential(&mut manager);
        let before = grant.lease();

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.held("dev"));
        assert!(!grant.allowed(), "no command during a hold");
        assert!(!before.holds());
        assert!(manager.cluster("dev").unwrap().client.is_some());
        assert!(watches(&manager, &pods), "the running watch goes on");

        rewrite_file(&mut manager, &first, &confirmed).await;
        assert!(grant.allowed(), "the same connection may refresh again");
        assert!(!before.holds(), "an ended use stays ended");
    }

    // A provider's guard ends with the hold, so its connection is replaced when the hold ends.
    #[tokio::test]
    async fn a_connection_spent_by_a_hold_reconnects_through_the_member() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("provider.sh");
        std::fs::write(
            &script,
            "printf '{\"token\":\"t\",\"expiry\":\"2020-01-01T00:00:00Z\"}'\n",
        )
        .unwrap();
        let provider = format!(
            "user:\n    auth-provider:\n      name: gcp\n      config:\n        cmd-path: /bin/sh\n        \
             cmd-args: {}\n        token-key: '{{.token}}'\n        expiry-key: '{{.expiry}}'\n",
            script.display()
        );
        let yaml = kubeconfig_with("https://127.0.0.1:1").replace("user: {}\n", &provider);
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        let first = dir.path().join("first");
        manager.active = Some("dev".to_owned());
        manager.sync_roles();
        join_dev(&mut manager);
        let access = manager.cluster("dev").unwrap().access.clone();
        let connection =
            cluster::connect_gated_with_policy(&manager.kubeconfig.merged, "dev", &access)
                .await
                .unwrap_or_else(|_| panic!("building a client runs nothing"));
        let attempt = manager.next_attempt;
        manager.next_attempt += 1;
        let slot = manager.cluster_mut("dev").unwrap();
        slot.attempt = attempt;
        slot.state = ConnectionState::Connecting;
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Ok((connection, test_version())),
            })
            .await;
        let bound = manager.schedule.start_active(&"dev".to_owned());
        assert!(manager.members.get_mut("dev").unwrap().begin_attempt(bound));
        let finished = MemberEvent::AttemptFinished {
            attempt: bound,
            outcome: AttemptOutcome::Connected,
        };
        manager.apply_member("dev", finished, Instant::now());

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.cluster("dev").unwrap().client.is_some());
        rewrite_file(&mut manager, &first, &yaml).await;

        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert!(slot.member_attempt.is_some(), "bound to the member");
    }

    // Outside the set too, only the selected cluster may run a command. The one switched away
    // from keeps its connection, and selecting it allows the command again.
    #[tokio::test]
    async fn a_cluster_switched_away_from_may_not_run_its_command() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        land_connection(&mut manager).await;
        let grant = command_credential(&mut manager);
        manager.sync_roles();
        assert!(grant.allowed());

        manager.activate("other".to_owned());
        assert_eq!(manager.connection_role("dev"), Role::Background);
        assert!(!grant.allowed(), "no command once switched away");
        let slot = manager.cluster("dev").unwrap();
        assert!(slot.client.is_some() && slot.write_credential.is_some());
        assert!(matches!(slot.state, ConnectionState::Connected(_)));

        manager.activate("dev".to_owned());
        assert!(grant.allowed(), "selecting it grants again");
    }

    // A provider's lease can't come back, so selecting its cluster again makes a new connection,
    // while being switched away from leaves the old one to the sessions on it.
    #[tokio::test]
    async fn selecting_a_spent_connection_again_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("provider.sh");
        std::fs::write(
            &script,
            "printf '{\"token\":\"t\",\"expiry\":\"2020-01-01T00:00:00Z\"}'\n",
        )
        .unwrap();
        let provider = format!(
            "user:\n    auth-provider:\n      name: gcp\n      config:\n        cmd-path: /bin/sh\n        \
             cmd-args: {}\n        token-key: '{{.token}}'\n        expiry-key: '{{.expiry}}'\n",
            script.display()
        );
        let yaml = kubeconfig_with("https://127.0.0.1:1").replace("user: {}\n", &provider);
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        manager.sync_roles();
        let access = manager.cluster("dev").unwrap().access.clone();
        let connection =
            cluster::connect_gated_with_policy(&manager.kubeconfig.merged, "dev", &access)
                .await
                .unwrap_or_else(|_| panic!("building a client runs nothing"));
        let attempt = manager.next_attempt;
        manager.next_attempt += 1;
        let slot = manager.cluster_mut("dev").unwrap();
        slot.attempt = attempt;
        slot.state = ConnectionState::Connecting;
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Ok((connection, test_version())),
            })
            .await;

        manager.activate("other".to_owned());
        let slot = manager.cluster("dev").unwrap();
        assert!(slot.write_credential.as_ref().unwrap().spent());
        assert!(slot.client.is_some(), "kept while switched away");

        manager.activate("dev".to_owned());
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert!(slot.client.is_none() && slot.write_credential.is_none());
    }

    // First seen behind an unreadable file, a definition with a command connects only while it
    // is selected: its grant ends with the selection.
    #[tokio::test]
    async fn a_first_seen_command_definition_connects_only_while_selected() {
        let dir = tempfile::tempdir().unwrap();
        let exec = "user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      \
                    command: /usr/bin/false\n      interactiveMode: Never\n";
        let yaml = kubeconfig_with("https://127.0.0.1:1").replace("user: {}\n", exec);
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &yaml).await;
        assert!(manager.contexts.provisional("dev") && !manager.paused("dev"));
        assert_eq!(manager.connection_role("dev"), Role::Background);
        assert!(manager.runs_a_command("dev"));

        let next = manager.next_attempt;
        manager.activate("dev".to_owned());
        assert_eq!(manager.next_attempt, next + 1, "one attempt started");
        assert_eq!(manager.connection_role("dev"), Role::Foreground);
        let slot = manager.cluster("dev").unwrap();
        let lease = slot.attempt_lease.clone().expect("the selection holds it");
        assert!(lease.holds());

        manager.select("other");
        assert_eq!(manager.connection_role("dev"), Role::Background);
        assert!(!lease.holds(), "no command runs once it is not selected");
    }

    /// Ends `dev`'s attempt with a failure time can fix, as the attempt's task reports it.
    async fn fail_selected(manager: &mut ClusterManager) {
        let attempt = manager.cluster("dev").unwrap().attempt;
        manager
            .handle_internal(Internal::AttemptTransient {
                cluster: "dev".into(),
                attempt,
            })
            .await;
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Err(cluster::SafeErrorText::fixed("connection refused")),
            })
            .await;
        manager.sync_selected_retry();
    }

    fn retry_waiting(manager: &ClusterManager) -> bool {
        manager
            .retries
            .retry
            .progress()
            .is_some_and(|p| p.next_retry.is_some())
    }

    #[tokio::test]
    async fn the_selected_cluster_retries_a_transient_failure_until_canceled() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;
        assert!(retry_waiting(&manager));

        let next = manager.next_attempt;
        manager.selected_retry_due(manager.retries.timer_id).await;
        assert_eq!(manager.next_attempt, next + 1, "the retry started");
        manager.sync_selected_retry();
        let generation = manager.cluster("dev").unwrap().generation;

        manager.cancel_connection("dev", generation);
        manager.sync_selected_retry();
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Failed(_)));
        assert!(slot.connect_task.is_none());
        assert!(!retry_waiting(&manager));
        let next = manager.next_attempt;
        manager.selected_retry_due(manager.retries.timer_id).await;
        assert_eq!(manager.next_attempt, next, "canceled");
    }

    #[tokio::test]
    async fn an_automatic_retry_never_runs_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let exec = "user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      \
                    command: /usr/bin/false\n      interactiveMode: Never\n";
        let yaml = kubeconfig_with("https://127.0.0.1:1").replace("user: {}\n", exec);
        let mut manager = manager_reading(dir.path(), &yaml).await;
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;
        assert!(!retry_waiting(&manager));
        let next = manager.next_attempt;
        manager.selected_retry_due(manager.retries.timer_id).await;
        assert_eq!(manager.next_attempt, next);
    }

    #[tokio::test]
    async fn a_pause_ends_a_waiting_retry() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, first) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;
        assert!(retry_waiting(&manager));
        let timer = manager.retries.timer_id;

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.paused("dev"));
        let next = manager.next_attempt;
        manager.selected_retry_due(timer).await;
        assert_eq!(manager.next_attempt, next, "nothing is sent while paused");
    }

    #[tokio::test]
    async fn a_changed_definition_or_removal_ends_a_waiting_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;
        let timer = manager.retries.timer_id;

        rewrite(
            &mut manager,
            dir.path(),
            &kubeconfig_with("https://127.0.0.1:2"),
        )
        .await;
        let next = manager.next_attempt;
        manager.selected_retry_due(timer).await;
        assert_eq!(manager.next_attempt, next, "not sent to the new definition");
        manager.sync_selected_retry();
        assert!(!retry_waiting(&manager));

        let without_dev = "apiVersion: v1\nkind: Config\nclusters:\n- name: o\n  cluster:\n    \
                           server: https://other\nusers:\n- name: u\n  user: {}\ncontexts:\n- \
                           name: other\n  context:\n    cluster: o\n    user: u\n";
        rewrite(&mut manager, dir.path(), without_dev).await;
        manager.sync_selected_retry();
        assert!(manager.retries.retry.progress().is_none());
    }

    /// Takes `dev` from a transient failure to an automatic retry under way, and returns its
    /// attempt.
    async fn retry_under_way(manager: &mut ClusterManager) -> u64 {
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(manager).await;
        assert!(retry_waiting(manager));
        retry_fires(manager).await;
        manager.sync_selected_retry();
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        slot.attempt
    }

    async fn retry_fires(manager: &mut ClusterManager) {
        let timer = manager.retries.timer_id;
        manager.selected_retry_due(timer).await;
    }

    /// Delivers a finished connection for `attempt`, as its task would.
    async fn deliver_connection(manager: &mut ClusterManager, attempt: u64) {
        let result = Ok((built_connection(manager).await, test_version()));
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result,
            })
            .await;
    }

    #[tokio::test]
    async fn a_retry_under_way_ends_when_another_cluster_is_selected() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        let attempt = retry_under_way(&mut manager).await;

        // Already up, so selecting it dials nothing.
        manager.cluster_mut("other").unwrap().state = ConnectionState::Connected(test_version());
        manager.activate("other".to_owned());
        manager.sync_selected_retry();
        deliver_connection(&mut manager, attempt).await;

        let slot = manager.cluster("dev").unwrap();
        assert!(slot.client.is_none(), "its result is not installed");
        assert!(slot.connect_task.is_none(), "its task is stopped");
    }

    #[tokio::test]
    async fn a_queued_retry_result_is_refused_once_its_cluster_is_not_selected() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        let attempt = retry_under_way(&mut manager).await;

        manager.cluster_mut("other").unwrap().state = ConnectionState::Connected(test_version());
        manager.activate("other".to_owned());
        // The result is handled before the loop looks at the retry again.
        deliver_connection(&mut manager, attempt).await;

        assert!(manager.cluster("dev").unwrap().client.is_none());
    }

    #[tokio::test]
    async fn a_retry_under_way_ends_when_its_definition_turns_provisional() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://127.0.0.1:1");
        let mut manager = manager_reading_two(dir.path(), &yaml, &yaml).await;
        let attempt = retry_under_way(&mut manager).await;

        rewrite_file(&mut manager, &dir.path().join("first"), UNREADABLE).await;
        assert!(manager.unsettled("dev"));
        manager.sync_selected_retry();
        deliver_connection(&mut manager, attempt).await;

        assert!(manager.cluster("dev").unwrap().client.is_none());
    }

    #[tokio::test]
    async fn a_pause_keeps_the_retries_stopped_until_selected() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, yaml, first) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;
        assert!(retry_waiting(&manager));

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        manager.sync_selected_retry();
        rewrite_file(&mut manager, &first, &yaml).await;
        manager.sync_selected_retry();
        // The load that ended the pause connects, and that attempt fails.
        fail_selected(&mut manager).await;
        assert!(!retry_waiting(&manager), "no new series starts");

        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;
        assert!(retry_waiting(&manager), "selecting it starts over");
    }

    #[tokio::test]
    async fn a_retry_does_not_probe_a_server_the_file_no_longer_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;

        // Edited, with the watcher's reload still to come.
        let path = dir.path().join("kubeconfig");
        std::fs::write(path, kubeconfig_with("https://127.0.0.1:2")).unwrap();
        let next = manager.next_attempt;
        retry_fires(&mut manager).await;
        assert_eq!(manager.next_attempt, next, "nothing was dialed");
    }

    #[tokio::test]
    async fn a_reload_that_ends_a_waiting_retry_needs_a_new_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            manager_reading(dir.path(), &kubeconfig_with("https://127.0.0.1:1")).await;
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;
        let next = manager.next_attempt;

        let moved = kubeconfig_with("https://127.0.0.1:2");
        rewrite(&mut manager, dir.path(), &moved).await;
        manager.sync_selected_retry();
        assert_eq!(manager.next_attempt, next, "the new server is not dialed");
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Failed(_)));
        assert!(!retry_waiting(&manager));

        manager.activate("dev".to_owned());
        assert_eq!(manager.next_attempt, next + 1, "selecting it connects");
    }

    // The connection made after a retry was ended brings back neither the member's feed nor its
    // interest in the kind, even with the context back on the server it was admitted on.
    #[tokio::test]
    async fn a_reconnect_after_a_retry_leaves_a_cut_off_member_view_cut_off() {
        use crate::settings::ClusterPin;
        let dir = tempfile::tempdir().unwrap();
        let original = kubeconfig_with("https://127.0.0.1:1");
        let mut manager = manager_reading(dir.path(), &original).await;
        let admitted = manager
            .admit_running_set(vec![
                ClusterPin::new("dev", "https://127.0.0.1:1"),
                ClusterPin::new("other", "https://other"),
            ])
            .unwrap();
        let pods = KindId::pods();
        let (feed, rows) = tokio::sync::watch::channel(Arc::default());
        manager.subscribe_member(1, "dev", admitted[0].clone(), pods.clone(), feed);
        manager.activate("dev".to_owned());
        manager.sync_selected_retry();
        fail_selected(&mut manager).await;
        assert!(retry_waiting(&manager));
        let next = manager.next_attempt;

        rewrite(
            &mut manager,
            dir.path(),
            &kubeconfig_with("https://127.0.0.1:2"),
        )
        .await;
        manager.sync_selected_retry();
        assert!(!manager.subscriptions.contains_key(&1), "cut by the load");
        rewrite(&mut manager, dir.path(), &original).await;
        manager.sync_selected_retry();
        assert_eq!(manager.next_attempt, next, "waits for a new selection");
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Failed(_)));

        manager.activate("dev".to_owned());
        let attempt = manager.cluster("dev").unwrap().attempt;
        deliver_connection(&mut manager, attempt).await;
        assert!(manager.cluster("dev").unwrap().client.is_some());
        assert!(!manager.subscriptions.contains_key(&1), "stays cut");
        assert!(!manager.cluster("dev").unwrap().watches.is_running(&pods));

        let hub = manager.hubs.get_or_create("dev", &pods);
        let epoch = hub.take_over(KindSnapshot::empty(
            "dev".into(),
            pods.clone(),
            SyncState::Loading,
        ));
        assert!(hub.publish(
            epoch,
            KindSnapshot::empty("dev".into(), pods, SyncState::Synced)
        ));
        assert_eq!(rows.borrow().state, SyncState::Idle);
    }

    // A log session asks for a background member's connection; the queue decides when it starts.
    #[tokio::test]
    async fn a_log_request_waits_for_a_background_members_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("other".to_owned());
        join_dev(&mut manager);
        let server = manager.cluster("dev").unwrap().info.server.clone();
        let pin = manager.contexts.pin("dev", server.as_deref());
        // An attempt just admitted: the stagger lets nothing else start yet.
        manager.schedule.enqueue("x".to_owned());
        assert!(manager.schedule.admit(Instant::now()).is_some());

        assert!(manager.log_client("dev", &pin).is_err());
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Disconnected));
        assert!(
            slot.connect_task.is_none(),
            "no connection bypassed the queue"
        );
    }

    #[tokio::test]
    async fn a_pinned_activation_refuses_a_context_that_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = None;
        let now = manager.summaries().remove(0);
        let (pin, generation) = (now.read_pin(), now.generation);

        let mut moved = pin.clone();
        moved.server = Some("https://other.example:6443".to_owned());
        let refused = manager.activate_pinned("dev".into(), &moved, generation);
        assert_eq!(refused, Err(ActivationRefused::Changed));
        let refused = manager.activate_pinned("dev".into(), &pin, generation + 1);
        assert_eq!(refused, Err(ActivationRefused::Changed));
        let refused = manager.activate_pinned("gone".into(), &pin, generation);
        assert_eq!(refused, Err(ActivationRefused::Removed));
        assert_eq!(manager.active, None, "nothing was activated");

        assert_eq!(
            manager.activate_pinned("dev".into(), &pin, generation),
            Ok(())
        );
        assert_eq!(manager.active.as_deref(), Some("dev"));
    }

    // A first connection that failed, then a hold and the same file back: the retry reports to
    // the member, so its old failure does not stay on show.
    #[tokio::test]
    async fn a_retry_after_a_hold_is_bound_to_the_active_member() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        join_dev(&mut manager);
        manager.apply_member("dev", MemberEvent::Activated, Instant::now());
        let attempt = manager.cluster("dev").unwrap().attempt;
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Err(cluster::SafeErrorText::fixed("refused")),
            })
            .await;
        assert!(matches!(dev_state(&manager), MemberState::Failed { .. }));
        assert!(!manager.cluster("dev").unwrap().reconnect);

        rewrite_file(&mut manager, &first, UNREADABLE).await;
        rewrite_file(&mut manager, &first, &confirmed).await;

        let slot = manager.cluster("dev").unwrap();
        assert!(slot.member_attempt.is_some(), "bound to the member");
        let attempt = slot.attempt;
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Ok((built_connection(&manager).await, test_version())),
            })
            .await;
        assert_eq!(dev_state(&manager), MemberState::Live);
    }

    // A 401 that landed just before the selection is acted on: the refused connection is not
    // reused, and selecting the cluster makes a new one.
    #[tokio::test]
    async fn activating_a_member_handles_a_pending_refusal_first() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("other".to_owned());
        live_dev(&mut manager).await;
        manager.sync_roles();
        let slot = manager.cluster_mut("dev").unwrap();
        let grant = slot.access.grant(CredentialSource::Definition);
        slot.write_credential = Some(WriteCredential::with_grant(
            Vec::new(),
            CredentialSource::Definition,
            None,
            None,
            grant.clone(),
            None,
        ));
        grant.note_refused();

        manager.activate("dev".to_owned());

        assert_eq!(dev_state(&manager), MemberState::Connecting);
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert!(slot.client.is_none() && slot.member_attempt.is_some());
    }

    // A relist with no error is routine: it starts no outage timer, and a real error still does.
    #[test]
    fn an_errorless_relist_does_not_start_the_outage_timer() {
        let relisting = SyncState::Reconnecting { error: None };
        assert_eq!(running_set::health_of([&relisting]), (false, None));

        let lost = SyncState::Reconnecting {
            error: Some("connection lost".into()),
        };
        let (any_synced, error) = running_set::health_of([&relisting, &lost]);
        assert!(!any_synced && error.is_some(), "a real error still counts");
    }

    // A load that discards a slot frees the admission its aborted attempt held.
    #[tokio::test]
    async fn a_discarded_slot_frees_its_admission() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, first) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        manager.schedule.enqueue("dev".to_owned());
        manager.pump_members(Instant::now());
        assert_eq!(manager.schedule.slots_in_use(), 1);

        rewrite_file(&mut manager, &first, NO_CONTEXTS).await;
        assert!(!manager.members.contains_key("dev"));
        assert_eq!(manager.schedule.slots_in_use(), 0);
    }

    // CRDs listed by a slot since replaced are another connection's, and change nothing.
    #[tokio::test]
    async fn a_crd_list_from_a_replaced_slot_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, confirmed, first) = pausable(dir.path(), NO_CONTEXTS).await;
        join_dev(&mut manager);
        watching_dev(&mut manager).await;
        let stale = manager.cluster("dev").unwrap().generation;
        let changed = confirmed.replace("user: {}", "user: {token: rotated}");
        rewrite_file(&mut manager, &first, &changed).await;

        manager
            .handle_internal(Internal::CrdsChanged {
                cluster: "dev".into(),
                generation: stale,
                fingerprint: 1,
                kinds: HashSet::from([KindId::new("example.com", "widgets")]),
            })
            .await;
        let slot = manager.cluster("dev").unwrap();
        assert!(slot.crds.is_empty());
        assert_eq!(slot.crd_fingerprint, None);
    }

    // A view closed during an outage keeps its rows on the active cluster only.
    #[tokio::test]
    async fn only_the_active_cluster_keeps_a_closed_views_rows() {
        for (active, kept) in [("dev", 1), ("other", 0)] {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
            manager.active = Some("dev".to_owned());
            join_dev(&mut manager);
            let pods = watching_dev(&mut manager).await;
            manager.active = Some(active.to_owned());
            manager.reconcile("dev");
            assert!(watches(&manager, &pods));

            let row = Arc::new(crate::table::Row {
                uid: "u1".into(),
                name: "web".into(),
                namespace: Some("default".into()),
                resource_version: "1".into(),
                generation: None,
                created: None,
                labels: Box::new([]),
                cells: Vec::new().into(),
            });
            let state = SyncState::Reconnecting {
                error: Some("dropped".into()),
            };
            let hub = manager.hubs.get_or_create("dev", &pods);
            hub.take_over(KindSnapshot {
                rows: vec![row],
                ..KindSnapshot::empty("dev".into(), pods.clone(), state)
            });

            manager.handle_request(Request::Unsubscribe { id: 1 });
            assert!(!watches(&manager, &pods));
            let (_hub, rows) = manager.hubs.subscribe("dev", &pods);
            assert_eq!(rows.borrow().rows.len(), kept, "with {active} active");
        }
    }

    /// Waits for the client to close `socket`, reading whatever it sent first.
    async fn closed_by_client(socket: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut buffer = [0; 1024];
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(socket.read(&mut buffer).await, Ok(0) | Err(_)) {}
        })
        .await
        .expect("the attempt should have been aborted");
    }

    // A new attempt aborts the one it replaces, whose late result is ignored. Replacing the slot
    // aborts the attempt too.
    #[tokio::test]
    async fn a_superseded_attempt_is_aborted_and_its_late_result_ignored() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let accept = async || {
            let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept());
            accepted.await.unwrap().unwrap().0
        };
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with(&server);
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;

        manager.activate("dev".to_owned());
        let mut first = accept().await;
        let superseded = manager.cluster("dev").unwrap().attempt;

        manager.cluster_mut("dev").unwrap().state =
            ConnectionState::Failed(cluster::SafeErrorText::fixed("refused"));
        manager.ensure_connected("dev");
        closed_by_client(&mut first).await;
        let mut second = accept().await;
        let current = manager.cluster("dev").unwrap().attempt;
        assert_ne!(current, superseded);

        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt: superseded,
                result: Ok((built_connection(&manager).await, test_version())),
            })
            .await;
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert!(slot.client.is_none(), "the late result is ignored");
        assert_eq!(slot.attempt, current);

        let moved = kubeconfig_with("https://127.0.0.1:1");
        rewrite_file(&mut manager, &dir.path().join("first"), &moved).await;
        closed_by_client(&mut second).await;
    }

    // A paused context starts no connection, and the load that ends the pause starts it.
    #[tokio::test]
    async fn a_paused_active_context_connects_only_when_the_pause_ends() {
        for second in pauses() {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, yaml, first) = pausable(dir.path(), &second).await;
            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(manager.paused("dev"));
            let next = manager.next_attempt;

            manager.activate("dev".to_owned());
            manager.ensure_connected("dev");
            let slot = manager.cluster("dev").unwrap();
            assert_eq!(slot.state, ConnectionState::AwaitingConfirmation);
            assert_eq!(manager.next_attempt, next, "no attempt started");

            rewrite_file(&mut manager, &first, &yaml).await;
            let slot = manager.cluster("dev").unwrap();
            assert!(matches!(slot.state, ConnectionState::Connecting));
            assert_eq!(slot.attempt, next, "the load started it");
        }
    }

    // A connection that finishes during a pause is dropped, whichever way it went, and the
    // cluster connects again when the pause ends.
    #[tokio::test]
    async fn a_connection_landing_during_a_pause_is_discarded() {
        for second in pauses() {
            for succeeded in [true, false] {
                let dir = tempfile::tempdir().unwrap();
                let (mut manager, yaml, first) = pausable(dir.path(), &second).await;
                manager.active = Some("dev".to_owned());
                let attempt = manager.next_attempt;
                manager.next_attempt += 1;
                let slot = manager.cluster_mut("dev").unwrap();
                slot.attempt = attempt;
                slot.state = ConnectionState::Connecting;
                let result = if succeeded {
                    Ok((built_connection(&manager).await, test_version()))
                } else {
                    Err(cluster::SafeErrorText::fixed("refused"))
                };
                rewrite_file(&mut manager, &first, UNREADABLE).await;
                assert!(manager.paused("dev"));
                // A changed definition has a fresh slot: give it the attempt, so that only the
                // pause drops the result.
                let slot = manager.cluster_mut("dev").unwrap();
                slot.attempt = attempt;
                slot.state = ConnectionState::Connecting;

                manager
                    .handle_internal(Internal::Connected {
                        cluster: "dev".into(),
                        attempt,
                        result,
                    })
                    .await;
                let slot = manager.cluster("dev").unwrap();
                assert!(slot.client.is_none() && slot.write_credential.is_none());
                assert!(!slot.discovering, "no discovery on a discarded result");
                assert_eq!(slot.state, ConnectionState::AwaitingConfirmation);

                rewrite_file(&mut manager, &first, &yaml).await;
                let slot = manager.cluster("dev").unwrap();
                assert!(matches!(slot.state, ConnectionState::Connecting));
                assert_ne!(slot.attempt, attempt, "a new attempt");
            }
        }
    }

    // Selecting or subscribing to a paused cluster starts no discovery or watch, what already
    // runs goes on, and both catch up when the pause ends.
    #[tokio::test]
    async fn a_paused_cluster_starts_no_discovery_or_watch() {
        use crate::discovery::tests::kind;
        let lw = &["get", "list", "watch"];
        let kinds = vec![
            kind("", "Pod", "pods", true, lw),
            kind("", "ConfigMap", "configmaps", true, lw),
        ];
        let (pods, configmaps) = (kinds[0].id(), kinds[1].id());
        for second in [NO_CONTEXTS] {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, yaml, first) = pausable(dir.path(), second).await;
            manager.settings = Arc::new(Settings {
                watch_mode: crate::settings::WatchMode::OnView,
                ..Settings::default()
            });
            manager.active = Some("dev".to_owned());
            land_connection(&mut manager).await;
            let slot = manager.cluster_mut("dev").unwrap();
            slot.watches.set_kinds(&kinds);
            slot.discovery = Some(Arc::new(ClusterDiscovery::new(kinds.clone())));
            let subscribe = |id, kind: &KindId| Request::Subscribe {
                id,
                cluster: "dev".into(),
                kind: kind.clone(),
            };
            manager.handle_request(subscribe(1, &pods));
            let running = |m: &ClusterManager, k: &KindId| {
                m.watched_kinds("dev").iter().any(|(kind, _)| kind == k)
            };
            assert!(running(&manager, &pods));

            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(manager.paused("dev"));
            manager.handle_request(subscribe(2, &configmaps));
            assert!(!running(&manager, &configmaps), "no new watch");
            assert!(running(&manager, &pods), "the running watch goes on");

            manager.cluster_mut("dev").unwrap().discovery = None;
            manager.activate("dev".to_owned());
            assert!(!manager.cluster("dev").unwrap().discovering);
            assert!(!running(&manager, &configmaps), "no new watch");
            manager
                .handle_internal(Internal::RetryDiscovery {
                    cluster: "dev".into(),
                })
                .await;
            assert!(!manager.cluster("dev").unwrap().discovering);

            rewrite_file(&mut manager, &first, &yaml).await;
            let slot = manager.cluster("dev").unwrap();
            assert!(
                slot.client.is_some(),
                "the same definition keeps its client"
            );
            assert!(slot.discovering, "discovery caught up");
            assert!(running(&manager, &configmaps), "the watch caught up");
        }
    }

    // Selecting a paused cluster that has a client but no discovery does not run discovery.
    #[tokio::test]
    async fn selecting_a_paused_cluster_starts_no_discovery() {
        for second in [NO_CONTEXTS] {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, _, first) = pausable(dir.path(), second).await;
            land_connection(&mut manager).await;
            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(manager.paused("dev"));

            manager.activate("dev".to_owned());
            assert!(!manager.cluster("dev").unwrap().discovering);
        }
    }

    // A discovery retry timer that fires during a pause runs nothing.
    #[tokio::test]
    async fn a_discovery_retry_during_a_pause_starts_nothing() {
        for second in [NO_CONTEXTS] {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, _, first) = pausable(dir.path(), second).await;
            manager.active = Some("dev".to_owned());
            land_connection(&mut manager).await;
            rewrite_file(&mut manager, &first, UNREADABLE).await;

            manager
                .handle_internal(Internal::RetryDiscovery {
                    cluster: "dev".into(),
                })
                .await;
            assert!(!manager.cluster("dev").unwrap().discovering);
        }
    }

    // Discovery that finishes during a pause is dropped and runs again when the pause ends.
    #[tokio::test]
    async fn discovery_landing_during_a_pause_is_discarded() {
        use crate::discovery::tests::kind;
        for second in pauses() {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, yaml, first) = pausable(dir.path(), &second).await;
            manager.active = Some("dev".to_owned());
            land_connection(&mut manager).await;
            let attempt = manager.cluster("dev").unwrap().attempt;
            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(manager.paused("dev"));
            let held = manager.held("dev");
            let slot = manager.cluster_mut("dev").unwrap();
            slot.attempt = attempt;
            slot.discovering = true;

            let found = read::DiscoveredKinds {
                kinds: vec![kind("", "Pod", "pods", true, &["get", "list", "watch"])],
                skipped_groups: Vec::new(),
                stale_groups: Default::default(),
            };
            manager
                .handle_internal(Internal::Discovered {
                    cluster: "dev".into(),
                    attempt,
                    result: Ok(found),
                })
                .await;
            let slot = manager.cluster("dev").unwrap();
            assert!(slot.discovery.is_none() && !slot.discovering);

            rewrite_file(&mut manager, &first, &yaml).await;
            let slot = manager.cluster("dev").unwrap();
            if held {
                assert!(slot.discovering, "the same definition runs discovery again");
            } else {
                assert!(slot.client.is_none(), "a changed definition reuses nothing");
                assert!(matches!(slot.state, ConnectionState::Connecting));
            }
        }
    }

    // A pause that ends on another server drops the old connection, and a result of an attempt
    // made before it is not taken for the new definition.
    #[tokio::test]
    async fn a_pause_ending_on_another_server_reuses_nothing() {
        for second in pauses() {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, _, first) = pausable(dir.path(), &second).await;
            manager.active = Some("dev".to_owned());
            land_connection(&mut manager).await;
            let old = manager.cluster("dev").unwrap().attempt;
            let late = built_connection(&manager).await;
            rewrite_file(&mut manager, &first, UNREADABLE).await;

            let moved = kubeconfig_with("https://127.0.0.1:2");
            rewrite_file(&mut manager, &first, &moved).await;
            assert!(!manager.paused("dev"));
            let slot = manager.cluster("dev").unwrap();
            assert!(slot.client.is_none() && slot.write_credential.is_none());
            assert!(matches!(slot.state, ConnectionState::Connecting));
            let attempt = slot.attempt;
            assert_ne!(attempt, old);

            manager
                .handle_internal(Internal::Connected {
                    cluster: "dev".into(),
                    attempt: old,
                    result: Ok((late, test_version())),
                })
                .await;
            let slot = manager.cluster("dev").unwrap();
            assert!(slot.client.is_none(), "the old attempt's result is ignored");
            assert!(matches!(slot.state, ConnectionState::Connecting));
        }
    }

    // A paused context runs nothing: a later file's definition on the same server that differs
    // only by its user, an exec plugin, never starts it. The held case then connects, which shows
    // the plugin would have been seen.
    #[tokio::test]
    async fn a_paused_context_runs_no_plugin_and_uses_no_fallback_credential() {
        let plugin = crate::auth_plugin::tests::FakePlugin::hanging();
        let with_plugin = format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
             http://127.0.0.1:1\nusers:\n- name: u\n  user:\n{}contexts:\n- name: dev\n  \
             context:\n    cluster: c\n    user: u\n",
            plugin.user_yaml()
        );

        let dir = tempfile::tempdir().unwrap();
        let confirmed = kubeconfig_with("http://127.0.0.1:1");
        let mut manager = manager_reading_two(dir.path(), &confirmed, &with_plugin).await;
        let first = dir.path().join("first");
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(!manager.held("dev") && manager.contexts.provisional("dev"));
        assert!(manager.paused("dev"), "another user is another definition");
        manager.activate("dev".to_owned());
        manager.ensure_connected("dev");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(plugin.starts().is_empty(), "the fallback's plugin ran");
        rewrite_file(&mut manager, &first, &confirmed).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            plugin.starts().is_empty(),
            "it ran after the file came back"
        );

        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading_two(dir.path(), &with_plugin, NO_CONTEXTS).await;
        let first = dir.path().join("first");
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.held("dev"));
        manager.activate("dev".to_owned());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(plugin.starts().is_empty(), "a held context ran its plugin");
        rewrite_file(&mut manager, &first, &with_plugin).await;
        plugin.wait_started().await;
    }

    // A provisional context showing its confirmed definition is not paused: it connects and its
    // discovery runs, and writes stay refused.
    #[tokio::test]
    async fn an_unchanged_provisional_context_keeps_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://127.0.0.1:1");
        let (mut manager, _, first) = pausable(dir.path(), &yaml).await;
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.contexts.provisional("dev") && !manager.held("dev"));
        assert!(manager.unsettled("dev") && !manager.paused("dev"));

        let next = manager.next_attempt;
        manager.activate("dev".to_owned());
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_eq!(slot.attempt, next);

        land_connection(&mut manager).await;
        assert!(manager.cluster("dev").unwrap().client.is_some());
        manager.unlocks.unlock("dev", "https://127.0.0.1:1");
        assert_eq!(
            manager.shell_client("dev", None).err(),
            Some(WriteRefusal::Unsettled {
                cluster: "dev".to_owned(),
            })
        );
    }

    // A provisional context showing its confirmed definition with no connection is refused as
    // not connected: nothing about it changed. A paused one says it is uncertain.
    #[tokio::test]
    async fn an_unchanged_provisional_context_without_a_client_is_not_connected() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://127.0.0.1:1");
        let (mut manager, _, first) = pausable(dir.path(), &yaml).await;
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.contexts.provisional("dev") && !manager.paused("dev"));
        manager.unlocks.unlock("dev", "https://127.0.0.1:1");
        let not_connected = WriteRefusal::NotConnected {
            cluster: "dev".to_owned(),
        };
        assert_eq!(
            manager.shell_client("dev", None).err(),
            Some(not_connected.clone())
        );
        assert!(matches!(
            manager.bound_write_client("dev", None).err(),
            Some(WriteError::Refused(WriteRefusal::NotConnected { .. }))
        ));

        let dir = tempfile::tempdir().unwrap();
        let [other, _] = pauses();
        let (mut manager, _, first) = pausable(dir.path(), &other).await;
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.contexts.provisional("dev") && manager.paused("dev"));
        manager.unlocks.unlock("dev", "https://127.0.0.1:3");
        assert_eq!(
            manager.shell_client("dev", None).err(),
            Some(WriteRefusal::Unsettled {
                cluster: "dev".to_owned(),
            })
        );
        assert!(matches!(
            manager.bound_write_client("dev", None).err(),
            Some(WriteError::KubeconfigUnreadable)
        ));
    }

    // A namespace list from an earlier slot of the context says nothing about this one.
    #[tokio::test]
    async fn a_late_namespace_list_of_an_old_slot_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, first) = pausable(dir.path(), NO_CONTEXTS).await;
        let old = manager.cluster("dev").unwrap().generation;
        rewrite_file(
            &mut manager,
            &first,
            &kubeconfig_with("https://127.0.0.1:2"),
        )
        .await;
        let current = manager.cluster("dev").unwrap().generation;
        assert_ne!(old, current);
        let listed = |generation, name: &str| Internal::NamespacesListed {
            cluster: "dev".into(),
            generation,
            names: vec![name.to_owned()],
        };

        manager.handle_internal(listed(old, "from-a")).await;
        assert!(manager.cluster("dev").unwrap().listed_namespaces.is_empty());

        manager.handle_internal(listed(current, "from-b")).await;
        assert_eq!(
            manager.cluster("dev").unwrap().listed_namespaces,
            ["from-b"]
        );
    }

    // A context first seen while an earlier file is broken has no confirmed definition to differ
    // from: it connects, and writes stay refused.
    #[tokio::test]
    async fn a_context_first_seen_behind_a_broken_file_connects() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://127.0.0.1:1");
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &yaml).await;
        assert!(manager.contexts.provisional("dev") && !manager.paused("dev"));

        let next = manager.next_attempt;
        manager.activate("dev".to_owned());
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_eq!(slot.attempt, next);

        land_connection(&mut manager).await;
        assert!(manager.cluster("dev").unwrap().client.is_some());
        manager.unlocks.unlock("dev", "https://127.0.0.1:1");
        assert_eq!(
            manager.shell_client("dev", None).err(),
            Some(WriteRefusal::Unsettled {
                cluster: "dev".to_owned(),
            })
        );
    }

    // A first-seen definition with an inline token connects behind a broken file and takes no
    // writes. A context whose confirmed definition differs stays paused.
    #[tokio::test]
    async fn a_first_seen_token_connects_and_a_changed_definition_stays_paused() {
        let with_token = kubeconfig_with("https://127.0.0.1:1")
            .replace("user: {}", "user:\n    token: inline-secret");
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &with_token).await;
        assert!(manager.contexts.provisional("dev") && !manager.paused("dev"));

        let next = manager.next_attempt;
        manager.activate("dev".to_owned());
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_eq!(manager.next_attempt, next + 1, "one attempt started");

        land_connection(&mut manager).await;
        assert!(manager.cluster("dev").unwrap().client.is_some());
        manager.unlocks.unlock("dev", "https://127.0.0.1:1");
        assert_eq!(
            manager.shell_client("dev", None).err(),
            Some(WriteRefusal::Unsettled {
                cluster: "dev".to_owned(),
            })
        );
        assert!(matches!(
            manager.bound_write_client("dev", None).err(),
            Some(WriteError::KubeconfigUnreadable)
        ));

        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, first) = pausable(dir.path(), &with_token).await;
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.contexts.provisional("dev") && manager.paused("dev"));
        let next = manager.next_attempt;
        manager.activate("dev".to_owned());
        manager.ensure_connected("dev");
        assert_eq!(manager.next_attempt, next, "nothing started");
        let slot = manager.cluster("dev").unwrap();
        assert!(slot.client.is_none());
        assert_eq!(slot.state, ConnectionState::AwaitingConfirmation);
        manager.unlocks.unlock("dev", "https://127.0.0.1:1");
        assert_eq!(
            manager.shell_client("dev", None).err(),
            Some(WriteRefusal::Unsettled {
                cluster: "dev".to_owned(),
            })
        );
    }

    // A paused slot with no connection says it waits instead of offering a Retry the core would
    // refuse, whatever it showed before, and the load that ends the pause clears that.
    #[tokio::test]
    async fn a_paused_slot_without_a_connection_waits_until_the_pause_ends() {
        for before in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Failed(cluster::SafeErrorText::fixed("offline")),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let [second, _] = pauses();
            let (mut manager, yaml, first) = pausable(dir.path(), &second).await;
            manager.cluster_mut("dev").unwrap().state = before.clone();
            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(manager.paused("dev"));
            let slot = manager.cluster("dev").unwrap();
            assert_eq!(
                slot.state,
                ConnectionState::AwaitingConfirmation,
                "{before:?}"
            );

            rewrite_file(&mut manager, &first, &yaml).await;
            assert!(!manager.paused("dev"));
            let slot = manager.cluster("dev").unwrap();
            assert_ne!(
                slot.state,
                ConnectionState::AwaitingConfirmation,
                "{before:?}"
            );
        }
    }

    // A first-seen context behind a broken file never waits: it has nothing to differ from.
    #[tokio::test]
    async fn a_first_seen_context_never_shows_a_wait() {
        let with_token = kubeconfig_with("https://127.0.0.1:1")
            .replace("user: {}", "user:\n    token: inline-secret");
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_reading_two(dir.path(), UNREADABLE, &with_token).await;
        assert!(manager.contexts.provisional("dev") && !manager.paused("dev"));
        let slot = manager.cluster("dev").unwrap();
        assert_eq!(slot.state, ConnectionState::Disconnected);
        assert!(manager.holds.writes_paused("dev"));
        assert!(!manager.holds.sessions_paused("dev"));
        manager.show_pause_state("dev");
        let slot = manager.cluster("dev").unwrap();
        assert_eq!(slot.state, ConnectionState::Disconnected);
    }

    // What front ends read directly is what the gate goes by, from the same step of a load.
    #[tokio::test]
    async fn the_published_holds_follow_the_gate() {
        for second in pauses() {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, yaml, first) = pausable(dir.path(), &second).await;
            assert!(!manager.holds.writes_paused("dev"));

            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(manager.unsettled("dev") && manager.holds.writes_paused("dev"));
            assert_eq!(manager.holds.sessions_paused("dev"), manager.paused("dev"));

            rewrite_file(&mut manager, &first, &yaml).await;
            assert!(!manager.holds.writes_paused("dev"));
            assert!(!manager.holds.sessions_paused("dev"));
        }
    }

    // A fresh read that refuses a write shows as paused before the answer is out, and the next
    // publish goes back to what the loaded state says.
    #[test]
    fn a_refused_validation_holds_until_the_next_publish() {
        let holds = crate::recheck::DefinitionHolds::default();
        holds.publish(HashSet::new(), HashSet::new());
        holds.hold_writes("dev");
        assert!(holds.writes_paused("dev") && !holds.sessions_paused("dev"));
        holds.publish(HashSet::new(), HashSet::new());
        assert!(!holds.writes_paused("dev"));
    }

    // A CRD the watch reported before discovery lists it, or one it removed that discovery still
    // lists, makes its group incomplete until discovery catches up.
    #[tokio::test]
    async fn a_crd_mismatch_marks_its_group_incomplete() {
        use crate::discovery::tests::kind;
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        land_connection(&mut manager).await;
        let attempt = manager.cluster("dev").unwrap().attempt;
        let widgets = || kind("example.com", "Widget", "widgets", true, &["get", "list"]);
        let gadgets = || kind("gone.example", "Gadget", "gadgets", true, &["get", "list"]);
        let slot = manager.cluster_mut("dev").unwrap();
        slot.crds.insert(widgets().id());
        slot.removed_crds.insert(gadgets().id());

        for (kinds, expected) in [
            (vec![gadgets()], vec!["example.com", "gone.example"]),
            (vec![widgets()], Vec::new()),
        ] {
            manager
                .handle_internal(Internal::Discovered {
                    cluster: "dev".into(),
                    attempt,
                    result: Ok(read::DiscoveredKinds {
                        kinds,
                        ..Default::default()
                    }),
                })
                .await;
            let slot = manager.cluster("dev").unwrap();
            let discovery = slot.discovery.as_ref().unwrap();
            assert_eq!(discovery.incomplete_groups, expected);
        }
    }

    // What a summary says about writes is what the gate does, held slots and fallbacks alike.
    #[tokio::test]
    async fn a_summary_is_provisional_exactly_while_the_gate_refuses_for_it() {
        // With a pin taken now, so only the definition's state can refuse it.
        let refused = |manager: &ClusterManager| {
            let pin = manager.session_pin("dev");
            manager.write_pin_gate("dev", &pin).is_err()
        };
        let provisional = |manager: &ClusterManager| {
            let summaries = manager.summaries();
            summaries
                .iter()
                .find(|c| c.info.name == "dev")
                .unwrap()
                .provisional
        };
        for second in pauses() {
            let dir = tempfile::tempdir().unwrap();
            let (mut manager, yaml, first) = pausable(dir.path(), &second).await;
            // Unlocked, so protection is not what refuses it below.
            manager.unlocks.unlock("dev", "https://127.0.0.1:1");
            assert!(!provisional(&manager) && !refused(&manager));

            rewrite_file(&mut manager, &first, UNREADABLE).await;
            assert!(provisional(&manager) && refused(&manager));

            rewrite_file(&mut manager, &first, &yaml).await;
            assert!(!provisional(&manager) && !refused(&manager));
        }
    }

    // Skipped and stale groups both reach the discovery a front end gets, and a full discovery
    // afterwards clears them.
    #[tokio::test]
    async fn discovery_says_which_groups_are_incomplete() {
        use crate::discovery::tests::kind;
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        land_connection(&mut manager).await;
        let attempt = manager.cluster("dev").unwrap().attempt;
        let pods = || vec![kind("", "Pod", "pods", true, &["get", "list", "watch"])];

        let partial = read::DiscoveredKinds {
            kinds: pods(),
            skipped_groups: vec!["metrics.k8s.io".into()],
            stale_groups: vec!["example.com".into(), "metrics.k8s.io".into()],
        };
        for (found, expected) in [
            (partial, vec!["example.com", "metrics.k8s.io"]),
            (
                read::DiscoveredKinds {
                    kinds: pods(),
                    ..Default::default()
                },
                Vec::new(),
            ),
        ] {
            manager
                .handle_internal(Internal::Discovered {
                    cluster: "dev".into(),
                    attempt,
                    result: Ok(found),
                })
                .await;
            let slot = manager.cluster("dev").unwrap();
            let discovery = slot.discovery.as_ref().unwrap();
            assert_eq!(discovery.incomplete_groups, expected);
        }
    }

    // The old connection is out of the foreground before the new selection can be seen.
    #[tokio::test]
    async fn a_switch_demotes_the_old_connection_before_publishing() {
        use crate::connection::policy::{CredentialAccess, ForegroundOnly, Role};
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, _, _) = pausable(dir.path(), NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        let access = CredentialAccess::new(Arc::new(ForegroundOnly), Role::Foreground);
        let grant = access.grant(crate::credential::CredentialSource::Plugin);
        manager.cluster_mut("dev").unwrap().access = access;
        assert!(grant.allowed());

        // What `activate` runs before it emits `ActiveChanged`.
        let previous = manager.select("other");

        assert_eq!(previous.as_deref(), Some("dev"));
        assert!(!grant.allowed(), "no command runs once the switch is seen");
    }

    // A provider connection whose lease ended while it was being made is not installed: the
    // selection gets a new attempt under a new grant.
    #[tokio::test]
    async fn a_connection_with_an_ended_lease_is_replaced() {
        use crate::connection::policy::{CredentialAccess, ForegroundOnly, Role};
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("provider");
        std::fs::write(
            &script,
            "printf '{\"token\":\"t\",\"expiry\":\"2020-01-01T00:00:00Z\"}'\n",
        )
        .unwrap();
        let yaml = format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
             http://127.0.0.1:1\nusers:\n- name: u\n  user:\n    auth-provider:\n      name: gcp\n\
             \x20     config:\n        cmd-path: /bin/sh\n        cmd-args: {}\n        \
             token-key: '{{.token}}'\n        expiry-key: '{{.expiry}}'\ncontexts:\n- name: dev\n\
             \x20 context:\n    cluster: c\n    user: u\n",
            script.display()
        );
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        let access = CredentialAccess::new(Arc::new(ForegroundOnly), Role::Foreground);
        let built =
            cluster::connect_gated_with_policy(&manager.kubeconfig.merged, "dev", &access).await;
        let Ok(connection) = built else {
            panic!("building the client contacts nothing");
        };
        access.set_role(Role::Background);
        access.set_role(Role::Foreground);
        assert!(connection.credential.spent());

        let attempt = manager.next_attempt;
        manager.next_attempt += 1;
        let slot = manager.cluster_mut("dev").unwrap();
        slot.attempt = attempt;
        slot.state = ConnectionState::Connecting;
        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Ok((connection, test_version())),
            })
            .await;

        let slot = manager.cluster("dev").unwrap();
        assert!(slot.client.is_none() && slot.write_credential.is_none());
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_ne!(slot.attempt, attempt, "a new attempt");
        assert_eq!(manager.next_attempt, attempt + 2, "exactly one");
    }

    // A selected member waiting on a same-server fallback with another credential: the load that
    // confirms the fallback starts its attempt, and nothing resets it into a second one.
    #[tokio::test]
    async fn confirming_a_fallback_starts_one_attempt_for_a_selected_member() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = kubeconfig_with("https://127.0.0.1:1")
            .replace("user: {}", "user:\n    token: fallback");
        let (mut manager, _, first) = pausable(dir.path(), &fallback).await;
        join_dev(&mut manager);
        rewrite_file(&mut manager, &first, UNREADABLE).await;
        assert!(manager.paused("dev"));
        manager.activate("dev".to_owned());
        let next = manager.next_attempt;

        rewrite_file(&mut manager, &first, NO_CONTEXTS).await;

        assert!(!manager.paused("dev") && manager.members.contains_key("dev"));
        assert_eq!(manager.next_attempt, next + 1, "exactly one attempt");
        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_eq!(slot.attempt, next, "the first attempt still runs");
        assert!(slot.member_attempt.is_some());
    }

    /// Starts `dev`'s attempt, then switches away and back before it reports. Returns the attempt.
    async fn switched_away_and_back_while_connecting(manager: &mut ClusterManager) -> u64 {
        manager.active = Some("dev".to_owned());
        manager.sync_roles();
        manager.ensure_connected("dev");
        let attempt = manager.cluster("dev").unwrap().attempt;
        manager.select("other");
        manager.activate("dev".to_owned());
        let slot = manager.cluster("dev").unwrap();
        assert_eq!(slot.attempt, attempt, "reselecting starts nothing yet");
        assert!(matches!(slot.state, ConnectionState::Connecting));
        attempt
    }

    // An attempt whose lease ended fails because of that, which says nothing about the cluster:
    // the error is dropped and the selection connects by itself under a new grant.
    #[tokio::test]
    async fn an_error_from_a_revoked_attempt_reconnects_the_selection() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("provider.sh");
        std::fs::write(
            &script,
            "printf '{\"token\":\"t\",\"expiry\":\"2020-01-01T00:00:00Z\"}'\n",
        )
        .unwrap();
        let provider = format!(
            "user:\n    auth-provider:\n      name: gcp\n      config:\n        cmd-path: /bin/sh\n        \
             cmd-args: {}\n        token-key: '{{.token}}'\n        expiry-key: '{{.expiry}}'\n",
            script.display()
        );
        let yaml = kubeconfig_with("https://127.0.0.1:1").replace("user: {}\n", &provider);
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        let attempt = switched_away_and_back_while_connecting(&mut manager).await;
        let old_access = manager.cluster("dev").unwrap().access.clone();

        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Err(cluster::SafeErrorText::fixed("not allowed")),
            })
            .await;

        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Connecting));
        assert_ne!(slot.attempt, attempt, "a new attempt");
        assert_eq!(manager.next_attempt, attempt + 2, "exactly one");
        let fresh = slot.access.grant(CredentialSource::AuthProvider);
        assert!(fresh.allowed(), "the new attempt has a new grant");
        assert!(!old_access.grant(CredentialSource::AuthProvider).allowed());
    }

    // A credential that runs nothing has no lease to lose, so its error is the cluster's.
    #[tokio::test]
    async fn an_error_without_a_command_credential_stays_after_a_switch() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://127.0.0.1:1");
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        let attempt = switched_away_and_back_while_connecting(&mut manager).await;

        manager
            .handle_internal(Internal::Connected {
                cluster: "dev".into(),
                attempt,
                result: Err(cluster::SafeErrorText::fixed("refused")),
            })
            .await;

        let slot = manager.cluster("dev").unwrap();
        assert!(matches!(slot.state, ConnectionState::Failed(_)));
        assert_eq!(manager.next_attempt, attempt + 1, "no retry");
    }

    // covers UP-06: once the broken file is valid without the context, it is gone.
    #[tokio::test]
    async fn a_broken_file_back_without_the_context_deselects_it() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = kubeconfig_with("https://dev");
        let mut manager = manager_reading_two(dir.path(), &yaml, NO_CONTEXTS).await;
        manager.active = Some("dev".to_owned());
        let path = dir.path().join("first");
        rewrite_file(&mut manager, &path, UNREADABLE).await;
        assert_eq!(manager.active.as_deref(), Some("dev"));

        rewrite_file(&mut manager, &path, NO_CONTEXTS).await;
        assert_eq!(manager.active, None);
        assert!(manager.cluster("dev").is_none());
        assert!(manager.contexts.removed("dev"));
    }

    // covers UP-06: while the first file defining `dev` is caught half-written, the second
    // file's `dev`, on another server, shows instead. Sessions on the first go on and are
    // never handed the fallback's client, and nothing changed once the file is back.
    #[tokio::test]
    async fn a_fallback_from_a_later_file_ends_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let first = kubeconfig_with("https://dev");
        let second = kubeconfig_with("https://prod");
        let mut manager = manager_reading_two(dir.path(), &first, &second).await;
        let pin = manager.contexts.pin_confirmed("dev");
        assert_eq!(pin.server(), Some("https://dev"));

        let path = dir.path().join("first");
        rewrite_file(&mut manager, &path, &first[..first.len() / 2]).await;
        let shown = manager.cluster("dev").and_then(|c| c.info.server.clone());
        assert_eq!(shown.as_deref(), Some("https://prod"), "the fallback shows");
        assert_eq!(manager.contexts.check(Pinned::Shell, "dev", &pin), None);
        offline(&mut manager);
        let logs = manager.log_client("dev", &pin).err().expect("no client");
        assert!(
            !logs.is_cluster() && !logs.is_fatal(),
            "the log session waits for the file: {}",
            logs.message
        );
        assert!(
            logs.message.contains("can't be read"),
            "not the fallback's client: {}",
            logs.message
        );

        rewrite_file(&mut manager, &path, &first).await;
        assert_eq!(manager.contexts.check(Pinned::Shell, "dev", &pin), None);
    }

    // covers UP-06, UP-40: logs opened while `dev` shows the second file's fallback are labelled
    // with the fallback's server. They never read from another one: the session ends with why,
    // also after the first file is back, rather than read from the first file's server.
    #[tokio::test]
    async fn logs_opened_on_a_fallback_never_switch_servers() {
        let dir = tempfile::tempdir().unwrap();
        let first = kubeconfig_with("https://dev");
        let second = kubeconfig_with("https://prod");
        let mut manager = manager_reading_two(dir.path(), &first, &second).await;
        let path = dir.path().join("first");
        rewrite_file(&mut manager, &path, &first[..first.len() / 2]).await;
        let shown = manager.cluster("dev").and_then(|c| c.info.server.clone());
        assert_eq!(shown.as_deref(), Some("https://prod"), "the tab's server");
        // As `Request::OpenLogs` and `Request::WriteClient` pin.
        let pin = manager.session_pin("dev");
        assert_eq!(pin.server(), Some("https://prod"));
        offline(&mut manager);
        let ended = manager.log_client("dev", &pin).err().expect("no client");
        assert!(ended.is_cluster(), "{}", ended.message);
        assert!(ended.message.contains("can't be read"), "{}", ended.message);

        rewrite_file(&mut manager, &path, &first).await;
        offline(&mut manager);
        let shown = manager.cluster("dev").and_then(|c| c.info.server.clone());
        assert_eq!(shown.as_deref(), Some("https://dev"));
        let ended = manager
            .log_client("dev", &pin)
            .err()
            .expect("never dev's client");
        assert!(ended.is_cluster(), "{}", ended.message);
    }

    // covers UP-06, UP-40: the same through `Request::OpenLogs`, with the first file back
    // before the session asks for its client.
    #[tokio::test]
    async fn open_logs_on_a_fallback_ends_instead_of_switching_servers() {
        // The file is back by the time it ends: no unreadable file is left to blame.
        let error = log_error_on_a_fallback(false).await;
        assert!(error.contains("different API server"), "{error}");
    }

    // covers UP-06, UP-40: a log tab opened on the fallback, its request taken only once the
    // first file is back: pinned to the server the tab shows, not to where the context points
    // when the core takes it.
    #[tokio::test]
    async fn open_logs_taken_after_the_fallback_went_keeps_the_tabs_server() {
        let error = log_error_on_a_fallback(true).await;
        assert!(error.contains("different API server"), "{error}");
    }

    /// Opens logs on `dev` while its first file is half-written (a fallback to prod shows),
    /// labelled with prod; with `taken_late`, the core takes the request only after the file is
    /// back. Returns why the session ended.
    async fn log_error_on_a_fallback(taken_late: bool) -> String {
        use crate::logs::{LogOptions, LogSnapshot, LogState, LogSubject, LogTarget};
        let dir = tempfile::tempdir().unwrap();
        let first = kubeconfig_with("https://dev");
        let second = kubeconfig_with("https://prod");
        let mut manager = manager_reading_two(dir.path(), &first, &second).await;
        let mut internal = manager.internal_rx.take().unwrap();
        let path = dir.path().join("first");
        rewrite_file(&mut manager, &path, &first[..first.len() / 2]).await;

        let (snapshots, mut rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (_paused, paused) = watch::channel(false);
        let target = LogTarget {
            namespace: "ns".into(),
            pod: "picked-on-prod".into(),
            container: None,
        };
        let shown = manager.cluster("dev").and_then(|c| c.info.server.clone());
        assert_eq!(shown.as_deref(), Some("https://prod"));
        let request = Request::OpenLogs {
            id: 1,
            cluster: "dev".into(),
            server: taken_late.then_some(shown),
            member: None,
            subject: LogSubject::Pod(target),
            options: LogOptions::default(),
            snapshots,
            paused,
            budget: crate::logs::Budgeted::new(manager.log_budget(), 1),
        };
        if taken_late {
            rewrite_file(&mut manager, &path, &first).await;
            manager.handle_request(request);
        } else {
            manager.handle_request(request);
            rewrite_file(&mut manager, &path, &first).await;
        }
        offline(&mut manager);

        let error = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    Some(msg) = internal.recv() => manager.handle_internal(msg).await,
                    changed = rx.changed() => {
                        assert!(changed.is_ok(), "session running");
                        if let LogState::Ended { error, .. } = &rx.borrow().state {
                            return error.clone();
                        }
                    }
                }
            }
        })
        .await
        .expect("the session ends rather than read from dev");
        error.expect("with why")
    }

    // covers UP-04: selecting or reconnecting a cluster while its exec auth plugin still runs
    // waits for that attempt; it does not start a second plugin.
    #[tokio::test]
    async fn a_retry_while_connecting_starts_no_second_plugin() {
        let plugin = crate::auth_plugin::tests::FakePlugin::hanging();
        let path = plugin.dir.path().join("kubeconfig");
        let yaml = format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
             http://127.0.0.1:1\nusers:\n- name: u\n  user:\n{}contexts:\n- name: dev\n  \
             context:\n    cluster: c\n    user: u\n",
            plugin.user_yaml()
        );
        std::fs::write(&path, yaml).unwrap();
        let (events, _rx) = crate::events::channel();
        let config = CoreConfig {
            config_dir: PathBuf::from("/nonexistent"),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: Vec::new(),
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        };
        let hubs = HubRegistry::default();
        let mut manager =
            ClusterManager::new(config, events, hubs.clone(), SessionUnlocks::default());
        let loaded = kubeconfig::load(&[path]);
        let slot = Cluster::new(
            loaded.contexts[0].clone(),
            None,
            &hubs,
            &manager.engine_config,
        );
        manager.contexts.record(&loaded);
        manager.kubeconfig = Arc::new(loaded);
        manager.clusters.push(slot);
        manager.active = Some("dev".to_owned());

        manager.ensure_connected("dev");
        let (leader, _) = plugin.wait_started().await;
        for _ in 0..3 {
            manager.ensure_connected("dev");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(plugin.starts(), [leader]);
    }

    // covers UP-54, UP-55: the one gate for actions that change a cluster refuses while it is
    // protected, follows unlocks and trust, and is pinned to the server.
    #[tokio::test]
    async fn the_write_gate_refuses_protected_clusters() {
        let (events, _rx) = crate::events::channel();
        let config = CoreConfig {
            config_dir: PathBuf::from("/nonexistent"),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: Vec::new(),
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        };
        let hubs = HubRegistry::default();
        let unlocks = SessionUnlocks::default();
        let mut manager = ClusterManager::new(config, events, hubs.clone(), unlocks.clone());
        manager.clusters.push(Cluster::new(
            context(None),
            None,
            &hubs,
            &manager.engine_config,
        ));
        let protected = |c: &str| WriteRefusal::Protected { cluster: c.into() };
        let not_connected = WriteRefusal::NotConnected {
            cluster: "dev".into(),
        };
        assert_eq!(manager.write_client("dev").err(), Some(protected("dev")));
        // A shell for another server than the context shows (its tab's view is older) is
        // refused before it starts, whatever the gate says.
        let moved = WriteRefusal::Moved {
            cluster: "dev".into(),
        };
        let shown = manager.cluster("dev").unwrap().info.server.clone();
        assert_eq!(
            manager
                .shell_client("dev", Some(Some("https://other".into())))
                .err(),
            Some(moved)
        );
        assert_eq!(
            manager.shell_client("dev", Some(shown)).err(),
            Some(protected("dev"))
        );
        // A context Flint does not know is never handed a client.
        assert_eq!(
            manager.write_client("gone").err(),
            Some(WriteRefusal::NotConnected {
                cluster: "gone".into()
            })
        );

        unlocks.unlock("dev", "https://elsewhere");
        assert_eq!(manager.write_client("dev").err(), Some(protected("dev")));
        unlocks.unlock("dev", "https://dev");
        assert_eq!(
            manager.write_client("dev").err(),
            Some(not_connected.clone())
        );
        assert!(manager.write_gate("dev").is_ok());
        unlocks.lock("dev");
        assert_eq!(manager.write_client("dev").err(), Some(protected("dev")));
        assert_eq!(manager.write_gate("dev").err(), Some(protected("dev")));

        let mut trusted = Settings::default();
        trusted
            .trusted_clusters
            .push(settings::ClusterPin::new("dev", "https://dev/"));
        manager.settings = Arc::new(trusted);
        assert_eq!(
            manager.write_client("dev").err(),
            Some(not_connected.clone())
        );
        // Stop Trusting refuses at once, before the settings change arrives.
        unlocks.revoke_trust("dev");
        assert_eq!(manager.write_client("dev").err(), Some(protected("dev")));
        let trust = unlocks.request_trust("dev");
        unlocks.trust_written("dev", trust, true);
        assert_eq!(manager.write_client("dev").err(), Some(not_connected));

        // A settings.toml that stops parsing trusts nothing, as at startup: an entry removed
        // by hand behind a typo is not kept in force.
        manager.apply_settings(Err("expected `=`".into())).await;
        assert!(manager.settings.trusted_clusters.is_empty());
        assert!(manager.settings.protect_local_clusters);
        assert_eq!(manager.write_client("dev").err(), Some(protected("dev")));
    }

    /// A step of a trust interleaving ([`trust_and_stop_trusting_interleave_safely`]).
    #[derive(Debug, Clone, Copy)]
    enum Step {
        /// Trust This Cluster confirmed: the request is numbered and its write queued.
        Trust,
        /// Stop Trusting: the request is numbered, the gate refuses at once, the write is queued.
        Stop,
        /// The write of request `n` (0-based, in the order requested) runs and lands.
        Write(usize),
        /// The write of request `n` runs while settings.toml cannot be written: it fails.
        Fail(usize),
        /// settings.toml stops parsing and is reloaded.
        Break,
        /// settings.toml is put back as it was before [`Step::Break`] and reloaded.
        Fix,
        /// A reload catches settings.toml empty (an editor truncating it before writing),
        /// then reads it whole again.
        Blank,
        /// Unlock for this session, at the context's server.
        Unlock,
        /// Lock Again.
        Lock,
        /// What the gate does now: `true` lets a new shell through.
        Allows(bool),
    }

    // covers UP-55: whatever order Trust and Stop Trusting requests are made in, and their
    // writes land or fail in, a shell is let through only while the newest request is a Trust
    // whose pin is saved (or a trust no later request touched). Stop Trusting refuses at once,
    // a Trust written before a later Stop Trusting never lifts it, and a broken settings.toml
    // trusts nothing.
    #[tokio::test]
    async fn trust_and_stop_trusting_interleave_safely() {
        use Step::*;
        let cases: &[(&str, &[Step])] = &[
            ("trust", &[Trust, Allows(false), Write(0), Allows(true)]),
            (
                "trust then stop, trust written first",
                &[
                    Trust,
                    Stop,
                    Allows(false),
                    Write(0),
                    Allows(false),
                    Write(1),
                    Allows(false),
                ],
            ),
            (
                "trust then stop, stop written first",
                &[
                    Trust,
                    Stop,
                    Write(1),
                    Allows(false),
                    Write(0),
                    Allows(false),
                ],
            ),
            (
                "stop then trust",
                &[
                    Trust,
                    Write(0),
                    Allows(true),
                    Stop,
                    Allows(false),
                    Trust,
                    Allows(false),
                    Write(1),
                    Allows(false),
                    Write(2),
                    Allows(true),
                ],
            ),
            (
                "stop then trust, trust written first",
                &[
                    Trust,
                    Write(0),
                    Stop,
                    Trust,
                    Write(2),
                    Allows(true),
                    Write(1),
                    Allows(false),
                ],
            ),
            (
                "trust, stop, trust in order",
                &[
                    Trust,
                    Stop,
                    Trust,
                    Write(0),
                    Allows(false),
                    Write(1),
                    Allows(false),
                    Write(2),
                    Allows(true),
                ],
            ),
            (
                "trust, stop, trust, the last written first",
                &[
                    Trust,
                    Stop,
                    Trust,
                    Write(2),
                    Allows(true),
                    Write(0),
                    Allows(true),
                    Write(1),
                    Allows(false),
                ],
            ),
            ("a failed trust", &[Trust, Fail(0), Allows(false)]),
            (
                "a failed stop keeps it protected",
                &[
                    Trust,
                    Write(0),
                    Stop,
                    Fail(1),
                    Allows(false),
                    Fix,
                    Allows(false),
                ],
            ),
            (
                "a failed trust after a stop",
                &[
                    Trust,
                    Write(0),
                    Stop,
                    Write(1),
                    Trust,
                    Fail(2),
                    Allows(false),
                ],
            ),
            (
                "the file breaks after a stop",
                &[
                    Trust,
                    Write(0),
                    Stop,
                    Break,
                    Allows(false),
                    Fail(1),
                    Fix,
                    Allows(false),
                ],
            ),
            (
                "the file breaks while trusted",
                &[
                    Trust,
                    Write(0),
                    Allows(true),
                    Break,
                    Allows(false),
                    Fix,
                    Allows(true),
                ],
            ),
            (
                "the file breaks between a trust and its write",
                &[Trust, Break, Allows(false), Fail(0), Fix, Allows(false)],
            ),
            (
                "lock again while a trust is being written",
                &[
                    Unlock,
                    Allows(true),
                    Trust,
                    Lock,
                    Allows(false),
                    Write(0),
                    Allows(false),
                ],
            ),
            (
                "lock again then trust again",
                &[
                    Unlock,
                    Trust,
                    Lock,
                    Trust,
                    Write(0),
                    Allows(false),
                    Write(1),
                    Allows(true),
                ],
            ),
            (
                "a blank read while a stop is not written",
                &[
                    Trust,
                    Write(0),
                    Stop,
                    Blank,
                    Allows(false),
                    Fail(1),
                    Blank,
                    Allows(false),
                ],
            ),
            (
                "a blank read after the stop is written",
                &[Trust, Write(0), Stop, Write(1), Blank, Allows(false)],
            ),
        ];
        for (name, steps) in cases {
            let dir = tempfile::tempdir().unwrap();
            let (events, _rx) = crate::events::channel();
            let config = CoreConfig {
                config_dir: dir.path().to_owned(),
                settings: Settings::default(),
                settings_error: None,
                kubeconfig_base: kubeconfig_file(dir.path()),
                home: None,
                watch_files: false,
                watch_timing: Default::default(),
            };
            let hubs = HubRegistry::default();
            let unlocks = SessionUnlocks::default();
            let mut manager = ClusterManager::new(config, events, hubs.clone(), unlocks.clone());
            manager.clusters.push(Cluster::new(
                context(None),
                None,
                &hubs,
                &manager.engine_config,
            ));
            record_load(&mut manager);
            let file = dir.path().join(settings::SETTINGS_FILE);
            let mut requests: Vec<(bool, u64)> = Vec::new();
            let mut saved_text: Option<String> = None;
            for (i, step) in steps.iter().enumerate() {
                match *step {
                    Trust => requests.push((true, unlocks.request_trust("dev"))),
                    Stop => requests.push((false, unlocks.revoke_trust("dev"))),
                    Write(n) | Fail(n) => {
                        let (trust, request) = requests[n];
                        let failing = matches!(step, Fail(_));
                        let before = manager.settings_error.clone();
                        if failing && before.is_none() {
                            manager.settings_error = Some("cannot be written".into());
                        }
                        let server = trust.then(|| "https://dev".to_owned());
                        manager.set_trusted("dev".into(), server, request).await;
                        manager.settings_error = before;
                    }
                    Break => {
                        saved_text = Some(std::fs::read_to_string(&file).unwrap_or_default());
                        std::fs::write(&file, "watch_mode = ").unwrap();
                        manager.reload_settings().await;
                    }
                    Fix => {
                        if let Some(text) = saved_text.take() {
                            std::fs::write(&file, text).unwrap();
                        }
                        manager.reload_settings().await;
                    }
                    Blank => {
                        let text = std::fs::read_to_string(&file).unwrap_or_default();
                        std::fs::write(&file, "").unwrap();
                        manager.reload_settings().await;
                        std::fs::write(&file, text).unwrap();
                        manager.reload_settings().await;
                    }
                    Unlock => unlocks.unlock("dev", "https://dev"),
                    Lock => {
                        unlocks.lock("dev");
                    }
                    Allows(wanted) => {
                        let allows = !matches!(
                            manager.write_client("dev"),
                            Err(WriteRefusal::Protected { .. })
                        );
                        assert_eq!(allows, wanted, "{name}: step {i} ({step:?}) of {steps:?}");
                    }
                }
            }
        }
    }

    // covers UP-06: a directory named with `..` and by its resolved name is watched once, by
    // the resolved name, so on Linux one inotify watch serves both and none is lost.
    #[test]
    fn watched_directories_are_resolved_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(real.join("kube")).unwrap();
        std::fs::create_dir(real.join("other")).unwrap();
        let files = [
            real.join("kube/config"),
            real.join("other/../kube/extra"),
            dir.path().join("kube/third"),
            // Missing: watched at its nearest ancestor, resolved too.
            real.join("other/../gone/deeper/config"),
        ];
        let (watch, pending) = watch_plan(files.iter());
        assert_eq!(
            watch,
            HashSet::from([real.join("kube"), real.clone()]),
            "{watch:?}"
        );
        assert!(pending.contains(&real.join("other/../gone")), "{pending:?}");
    }

    // covers UP-12: a missing directory that appears (a symlink's target directory restored
    // with settings.toml in it) reloads the settings too, not only kubeconfig.
    #[test]
    fn a_directory_that_appears_reloads_settings_and_kubeconfig() {
        let watched = Watched {
            dirs: HashSet::new(),
            pending: HashSet::from([PathBuf::from("/cfg/dotfiles")]),
            kube: HashSet::from([PathBuf::from("/k/config")]),
            settings: HashSet::from([PathBuf::from("/cfg/dotfiles/flint.toml")]),
            activity: HashSet::from([PathBuf::from("/cfg/agent-activity.jsonl")]),
        };
        let changed = classify(&[PathBuf::from("/cfg/dotfiles")], false, &watched);
        assert!(changed.rearm && changed.settings && changed.kubeconfig && changed.activity);
        let changed = classify(&[PathBuf::from("/k/config")], false, &watched);
        assert!(!changed.rearm && !changed.settings && changed.kubeconfig && !changed.activity);
        let changed = classify(
            &[PathBuf::from("/cfg/dotfiles/flint.toml")],
            false,
            &watched,
        );
        assert!(!changed.rearm && changed.settings && !changed.kubeconfig && !changed.activity);
        // covers UP-16: an agent's call reloads only the activity log.
        let changed = classify(
            &[PathBuf::from("/cfg/agent-activity.jsonl")],
            false,
            &watched,
        );
        assert!(!changed.rearm && !changed.settings && !changed.kubeconfig && changed.activity);
    }

    // covers UP-12: settings.toml as a symlink to a file that is gone is watched at its
    // target too, so restoring the target reloads it.
    #[test]
    fn a_dangling_symlink_is_watched_at_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("dotfiles")).unwrap();
        let link = dir.path().join("settings.toml");
        std::os::unix::fs::symlink("dotfiles/flint.toml", &link).unwrap();
        let names = watch_names(&link);
        assert!(
            names.contains(&real.join("dotfiles/flint.toml")),
            "{names:?}"
        );
        assert!(names.contains(&dir.path().join("dotfiles/flint.toml")));
        // An absolute target, in a directory that does exist.
        let absolute = dir.path().join("abs.toml");
        std::os::unix::fs::symlink(real.join("dotfiles/other.toml"), &absolute).unwrap();
        assert!(watch_names(&absolute).contains(&real.join("dotfiles/other.toml")));
    }

    /// A manager watching `paths` for real, loaded once.
    async fn manager_watching(root: &Path, paths: Vec<PathBuf>) -> ClusterManager {
        let (events, _rx) = crate::events::channel();
        let config = CoreConfig {
            config_dir: root.join("flint"),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: paths,
            home: None,
            watch_files: true,
            watch_timing: Default::default(),
        };
        let mut manager = ClusterManager::new(
            config,
            events,
            HubRegistry::default(),
            SessionUnlocks::default(),
        );
        manager.arm_watcher();
        manager.reload_kubeconfig().await;
        manager
    }

    /// Runs the manager's internal messages until `context` shows `server` (`None`: is gone).
    /// Returns as soon as it does, and fails after a limit only a lost event can reach.
    async fn until_server(manager: &mut ClusterManager, context: &str, server: Option<&str>) {
        let mut internal = manager.internal_rx.take().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let shown = manager.cluster(context).map(|c| c.info.server.clone());
            if shown.as_ref().map(|s| s.as_deref()) == server.map(Some) {
                break;
            }
            match tokio::time::timeout_at(deadline, internal.recv()).await {
                Ok(Some(msg)) => manager.handle_internal(msg).await,
                _ => panic!("{context} still shows {shown:?}, wanted {server:?}"),
            }
        }
        manager.internal_rx = Some(internal);
    }

    /// Runs the manager's internal messages until the watches on `dir` have started and the
    /// read that follows is done, so a later edit can only be seen through a live event.
    async fn until_watching(manager: &mut ClusterManager, dir: &Path) {
        let mut internal = manager.internal_rx.take().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let msg = match tokio::time::timeout_at(deadline, internal.recv()).await {
                Ok(Some(msg)) => msg,
                _ => panic!("{} is still not watched", dir.display()),
            };
            let started = matches!(&msg, Internal::WatchesStarted(dirs) if dirs.contains(dir));
            manager.handle_internal(msg).await;
            if started {
                break;
            }
        }
        manager.internal_rx = Some(internal);
    }

    /// Points the symlink `link` at `target` in one step, as `ln -sfn` tools do.
    fn point_link(link: &Path, target: &Path) {
        let next = link.with_extension("next");
        std::os::unix::fs::symlink(target, &next).unwrap();
        std::fs::rename(&next, link).unwrap();
    }

    /// `root/name/config` defining `dev` at `server`.
    fn config_in(root: &Path, name: &str, server: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config");
        std::fs::write(&file, kubeconfig_with(server)).unwrap();
        file
    }

    #[tokio::test]
    async fn a_link_moved_to_another_folder_is_still_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let first = config_in(&root, "a", "https://a");
        let second = config_in(&root, "b", "https://b");
        let link = root.join("link");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let mut manager = manager_watching(&root, vec![link.clone()]).await;
        until_server(&mut manager, "dev", Some("https://a")).await;

        point_link(&link, &second);
        until_watching(&mut manager, &root.join("b")).await;
        until_server(&mut manager, "dev", Some("https://b")).await;
        std::fs::write(&second, kubeconfig_with("https://b-edited")).unwrap();
        until_server(&mut manager, "dev", Some("https://b-edited")).await;
    }

    // inotify keeps one watch on a directory whose parent is renamed, under both names.
    #[tokio::test]
    async fn a_target_whose_parent_was_renamed_is_still_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let first = config_in(&root, "old/team", "https://a");
        let link = root.join("link");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let mut manager = manager_watching(&root, vec![link.clone()]).await;
        until_watching(&mut manager, &root.join("old/team")).await;
        until_server(&mut manager, "dev", Some("https://a")).await;

        std::fs::rename(root.join("old"), root.join("new")).unwrap();
        let moved = root.join("new/team/config");
        point_link(&link, &moved);
        until_watching(&mut manager, &root.join("new/team")).await;
        until_server(&mut manager, "dev", Some("https://a")).await;
        std::fs::write(&moved, kubeconfig_with("https://edited")).unwrap();
        until_server(&mut manager, "dev", Some("https://edited")).await;
    }

    // Only a move of what is watched replaces the watcher; a plain edit keeps it.
    #[test]
    fn the_watch_plan_changes_only_when_a_link_moves() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let first = config_in(&root, "a", "https://a");
        let second = config_in(&root, "b", "https://b");
        let link = root.join("link");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let paths = [link.clone()];
        let before = Watched::for_files(&paths, &root);
        std::fs::write(&first, kubeconfig_with("https://a-edited")).unwrap();
        assert!(before == Watched::for_files(&paths, &root), "an edit");
        point_link(&link, &second);
        let after = Watched::for_files(&paths, &root);
        assert!(before != after, "a move");
        assert!(after.dirs.contains(&root.join("b")), "{:?}", after.dirs);
    }

    #[test]
    fn a_link_cycle_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        assert!(watch_names(&a).contains(&b));
    }

    #[tokio::test]
    async fn a_moved_link_in_a_chain_is_still_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let first = config_in(&root, "a", "https://a");
        let second = config_in(&root, "b", "https://b");
        std::fs::create_dir(root.join("hops")).unwrap();
        let (near, far) = (root.join("hops/near"), root.join("hops/far"));
        std::os::unix::fs::symlink(&first, &far).unwrap();
        std::os::unix::fs::symlink(&far, &near).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&near, &link).unwrap();
        let mut manager = manager_watching(&root, vec![link]).await;
        until_server(&mut manager, "dev", Some("https://a")).await;

        // The last link of the chain moves, not the one the file list names.
        point_link(&far, &second);
        until_watching(&mut manager, &root.join("b")).await;
        until_server(&mut manager, "dev", Some("https://b")).await;
        std::fs::write(&second, kubeconfig_with("https://b-edited")).unwrap();
        until_server(&mut manager, "dev", Some("https://b-edited")).await;
    }

    #[tokio::test]
    async fn a_link_target_that_disappears_and_returns_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let first = config_in(&root, "a", "https://a");
        let second = config_in(&root, "b", "https://b");
        let link = root.join("link");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let mut manager = manager_watching(&root, vec![link.clone()]).await;
        until_server(&mut manager, "dev", Some("https://a")).await;
        point_link(&link, &second);
        until_watching(&mut manager, &root.join("b")).await;
        until_server(&mut manager, "dev", Some("https://b")).await;

        std::fs::remove_dir_all(root.join("b")).unwrap();
        until_server(&mut manager, "dev", None).await;
        config_in(&root, "b", "https://b-back");
        until_watching(&mut manager, &root.join("b")).await;
        until_server(&mut manager, "dev", Some("https://b-back")).await;
        std::fs::write(&second, kubeconfig_with("https://b-edited")).unwrap();
        until_server(&mut manager, "dev", Some("https://b-edited")).await;
    }

    #[tokio::test]
    async fn each_of_several_kubeconfig_links_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let two = |server: &str| {
            kubeconfig_with(server)
                .replace("name: dev", "name: two")
                .replace("name: c\n", "name: c2\n")
                .replace("cluster: c\n", "cluster: c2\n")
        };
        let kept = config_in(&root, "kept", "https://kept");
        // In the folder of the link that stays, which must stay watched when this one moves.
        let first = root.join("kept/first");
        let second = config_in(&root, "b", "https://b");
        std::fs::write(&first, two("https://a")).unwrap();
        std::fs::write(&second, two("https://b")).unwrap();
        let (one, moved) = (root.join("one"), root.join("moved"));
        std::os::unix::fs::symlink(&kept, &one).unwrap();
        std::os::unix::fs::symlink(&first, &moved).unwrap();
        let mut manager = manager_watching(&root, vec![one, moved.clone()]).await;
        until_server(&mut manager, "two", Some("https://a")).await;

        point_link(&moved, &second);
        until_watching(&mut manager, &root.join("b")).await;
        until_server(&mut manager, "two", Some("https://b")).await;
        std::fs::write(&second, two("https://b-edited")).unwrap();
        until_server(&mut manager, "two", Some("https://b-edited")).await;
        std::fs::write(&kept, kubeconfig_with("https://kept-edited")).unwrap();
        until_server(&mut manager, "dev", Some("https://kept-edited")).await;
    }

    // covers UP-24
    #[tokio::test]
    async fn a_stopped_follower_asks_for_its_hub_to_be_released() {
        let hubs = HubRegistry::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let crds = KindId::custom_resource_definitions();
        let follower = Follower::new(&hubs, "dev", crds.clone(), &tx);
        let task = tokio::spawn(follow_crds(follower, 0, tx.clone()));
        // The watch manager stopped the CRD watch while the follower still listened: the hub
        // stays (someone reads it).
        hubs.release("dev", &crds);
        assert_eq!(hubs.states("dev").len(), 1);
        task.abort();
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let Internal::ReleaseHub { cluster, kind } = msg else {
            panic!("expected a release");
        };
        assert_eq!((cluster.as_str(), &kind), ("dev", &crds));
        // What the manager does with it when no watch runs for the kind.
        hubs.release(&cluster, &kind);
        assert!(hubs.states("dev").is_empty(), "no hub left behind");
    }

    // covers UP-02: with no ~/.kube yet, its parent is watched and creating it re-arms.
    #[test]
    fn a_missing_directory_is_watched_through_its_nearest_ancestor() {
        let home = tempfile::tempdir().unwrap();
        let kube = home.path().join(".kube");
        let (ancestor, missing) = missing_dirs(&kube).unwrap();
        assert_eq!(ancestor, home.path());
        assert!(missing.contains(&kube), "{missing:?}");
        let resolved = std::fs::canonicalize(home.path()).unwrap().join(".kube");
        assert!(missing.contains(&resolved), "{missing:?}");
        // Two levels missing: both count.
        let deep = home.path().join("a/b");
        let (ancestor, missing) = missing_dirs(&deep).unwrap();
        assert_eq!(ancestor, home.path());
        assert!(missing.contains(&home.path().join("a")) && missing.contains(&deep));
        // An existing directory is watched as it is.
        std::fs::create_dir(&kube).unwrap();
        assert!(missing_dirs(&kube).is_none());
    }
}
