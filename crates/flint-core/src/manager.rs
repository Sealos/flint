//! `ClusterManager`: the core's single owner of cluster state, running as one task on the core
//! runtime. It handles [`Command`]s, connection results, and kubeconfig and settings reloads, and
//! emits [`Event`]s.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kube::Client;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::activity;
use crate::cluster::{self, CONNECT_TIMEOUT, ClusterId, ServerVersion};
use crate::discovery::ClusterDiscovery;
use crate::handle::{
    ClusterSummary, ClustersSnapshot, Command, ConnectionState, CoreConfig, Event, Request,
};
use crate::kubeconfig::{self, ContextInfo, LoadedKubeconfig};
use crate::logs::{self, KubeLogSource, KubePodSource, LogError, SessionConfig};
use crate::protection::{ClusterClients, GatedClient, SessionUnlocks, WriteRefusal, protection};
use crate::read::{self, KindId, KindInfo};
use crate::recheck::{ContextHistory, ContextPin, Pinned};
use crate::settings::{self, AiScope, Settings};
use crate::store::{KindSnapshot, SyncState};
use crate::watch::{ClusterWatches, EngineConfig, HubRegistry, KubeSource};

/// Changes to watched files often come in bursts (editor save, `kind` rewriting the file).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
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
    /// Id of the latest connection attempt, from the manager-wide counter, so a late result of an
    /// older attempt (even one made before the context was removed and re-added) is ignored.
    attempt: u64,
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
        for task in [self.crd_task.take(), self.namespaces_task.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
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
            attempt: 0,
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
            crd_task: None,
            namespaces_task: None,
            namespace_filter: Vec::new(),
            listed_namespaces: Vec::new(),
        }
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

    fn summary(&self) -> ClusterSummary {
        ClusterSummary {
            info: self.info.clone(),
            state: self.state.clone(),
            generation: self.generation,
        }
    }
}

enum Internal {
    /// Reload kubeconfig, then say so ([`crate::CoreHandle::reload_kubeconfig`]).
    ReloadKubeconfig {
        reply: tokio::sync::oneshot::Sender<()>,
    },
    Connected {
        cluster: ClusterId,
        attempt: u64,
        result: Result<(ClusterClients, ServerVersion), cluster::SafeErrorText>,
    },
    FilesChanged(Changed),
    Discovered {
        cluster: ClusterId,
        attempt: u64,
        result: Result<read::DiscoveredKinds, String>,
    },
    CrdsChanged {
        cluster: ClusterId,
        fingerprint: u64,
        kinds: HashSet<KindId>,
    },
    RetryDiscovery {
        cluster: ClusterId,
    },
    NamespacesListed {
        cluster: ClusterId,
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
    settings: Arc<Settings>,
    settings_error: Option<String>,
    /// In kubeconfig order.
    clusters: Vec<Cluster>,
    active: Option<ClusterId>,
    /// Source of connection attempt ids; never reused.
    next_attempt: u64,
    watcher: Option<FileWatcher>,
    hubs: HubRegistry,
    engine_config: Arc<EngineConfig>,
    /// Open subscriptions by id.
    subscriptions: HashMap<u64, (ClusterId, KindId)>,
    /// Running log sessions by id. They belong to no cluster slot, so they outlive cluster
    /// switches and kubeconfig reloads (UP-42).
    log_sessions: HashMap<u64, JoinHandle<()>>,
    /// The memory budget all log sessions share (`log_total_mib`).
    log_budget: Arc<logs::LogBudget>,
    /// The clusters the user unlocked for this session, shared with the front end (ADR-0015).
    unlocks: SessionUnlocks,
}

struct FileWatcher {
    _watcher: RecommendedWatcher,
    /// Whether the config directory is watched. It may not exist until settings are first written.
    settings_dir_watched: bool,
}

impl ClusterManager {
    /// The log sessions' shared budget, for [`crate::LogSession::set_shown`].
    pub(crate) fn log_budget(&self) -> Arc<logs::LogBudget> {
        self.log_budget.clone()
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
            next_attempt: 1,
            watcher: None,
            hubs,
            engine_config,
            subscriptions: HashMap::new(),
            log_sessions: HashMap::new(),
            log_budget,
            unlocks,
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
        self.rebuild_watcher();
        self.reload_activity().await;
        self.reload_kubeconfig().await;
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
        }
    }

    fn emit(&self, event: Event) {
        self.events.send(event);
    }

    fn emit_settings(&self) {
        self.emit(Event::SettingsChanged {
            settings: self.settings.clone(),
            error: self.settings_error.clone(),
        });
    }

    async fn handle_command(&mut self, command: Command) {
        match command {
            Command::Activate(id) => self.activate(id),
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
            Command::ReloadKubeconfig => self.reload_kubeconfig().await,
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
            Command::SetAppearance(appearance) => {
                self.write_settings("the appearance", move |dir| {
                    settings::set_appearance(dir, appearance)
                })
                .await
            }
            Command::SetNamespaceFilter {
                cluster,
                namespaces,
            } => {
                if let Some(slot) = self.cluster_mut(&cluster) {
                    slot.namespace_filter = namespaces;
                    slot.update_scope();
                }
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
            // Requests are handled without waiting: the reload runs as an internal message.
            Request::ReloadKubeconfig { reply } => {
                let _ = self.internal_tx.send(Internal::ReloadKubeconfig { reply });
            }
            Request::Subscribe { id, cluster, kind } => {
                self.subscriptions.insert(id, (cluster.clone(), kind));
                self.reconcile(&cluster);
            }
            Request::Unsubscribe { id } => {
                if let Some((cluster, kind)) = self.subscriptions.remove(&id) {
                    self.reconcile(&cluster);
                    let running = self
                        .clusters
                        .iter()
                        .find(|c| c.info.name == cluster)
                        .is_some_and(|c| c.watches.running().contains_key(&kind));
                    if !running {
                        self.hubs.release(&cluster, &kind);
                    }
                }
            }
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
                let pin = match &server {
                    Some(server) => self.contexts.pin(&cluster, server.as_deref()),
                    None => self.session_pin(&cluster),
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
            Request::CheckContext {
                cluster,
                pin,
                reply,
            } => {
                let _ = reply.send(self.contexts.check(Pinned::Shell, &cluster, &pin));
            }
            Request::FetchObject {
                cluster,
                kind,
                namespace,
                name,
                reply,
            } => {
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
        }
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

    /// Brings a cluster's watches in line with whether it is active, its watch mode, and the
    /// open subscriptions, and follows its CRDs while it is active.
    fn reconcile(&mut self, id: &str) {
        let active = self.active.as_deref() == Some(id);
        let mode = self.settings.watch_mode_for(id);
        let subscribed: HashSet<KindId> = self
            .subscriptions
            .values()
            .filter(|(c, _)| c == id)
            .map(|(_, k)| k.clone())
            .collect();
        let hubs = self.hubs.clone();
        let internal = self.internal_tx.clone();
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        let follow = active && slot.discovery.is_some() && slot.client.is_some();
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
        slot.watches.reconcile(active, mode, &subscribed);
        if follow && slot.crd_task.is_none() {
            let follower =
                Follower::new(&hubs, id, KindId::custom_resource_definitions(), &internal);
            slot.crd_task = Some(tokio::spawn(follow_crds(follower, internal.clone())));
        }
        if follow && slot.namespaces_task.is_none() {
            let follower = Follower::new(&hubs, id, KindId::namespaces(), &internal);
            slot.namespaces_task = Some(tokio::spawn(follow_namespaces(follower, internal)));
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
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        let Some(client) = slot.client.as_ref().map(|c| c.read.clone()) else {
            return;
        };
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
        self.write_settings("the AI scope", move |dir| {
            settings::set_ai_scope(dir, &cluster, &server, &from, &scope)
        })
        .await;
    }

    /// Pins `cluster` at `server` in a list of pins (`Some`), or removes it (`None`), in
    /// `settings.toml`. Returns whether the change was saved.
    async fn set_pin(&mut self, pins: Pins, cluster: ClusterId, server: Option<String>) -> bool {
        if let Some(server) = &server {
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
                if self
                    .watcher
                    .as_ref()
                    .is_some_and(|w| !w.settings_dir_watched)
                {
                    // The write may have created the config directory.
                    self.rebuild_watcher();
                }
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
            self.kubeconfig_paths = paths;
            self.rebuild_watcher();
            self.reload_kubeconfig().await;
        }
    }

    async fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::ReloadKubeconfig { reply } => {
                self.reload_kubeconfig().await;
                let _ = reply.send(());
            }
            Internal::Connected {
                cluster,
                attempt,
                result,
            } => {
                let target = cluster::Target::of(&self.kubeconfig.merged, &cluster);
                let Some(slot) = self.cluster_mut(&cluster) else {
                    return;
                };
                if slot.attempt != attempt {
                    return;
                }
                let connected = result.is_ok();
                match result {
                    Ok((clients, version)) => {
                        slot.watches.set_source(Some(KubeSource::new(
                            clients.read.clone(),
                            Some(&version),
                            target,
                        )));
                        slot.client = Some(clients);
                        slot.state = ConnectionState::Connected(version);
                    }
                    Err(message) => {
                        slot.watches.set_source(None);
                        slot.client = None;
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
            }
            Internal::Discovered {
                cluster,
                attempt,
                result,
            } => {
                let Some(slot) = self.cluster_mut(&cluster) else {
                    return;
                };
                if slot.attempt != attempt {
                    return;
                }
                slot.discovering = false;
                let again = std::mem::take(&mut slot.rediscover);
                let mut retry = false;
                let mut partial = false;
                let error = match result {
                    Ok(found) => {
                        // Groups that failed this time keep the kinds (and watches) they had,
                        // and discovery is retried with backoff until they answer again.
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
                        let behind = slot.crds.iter().any(|k| !served.contains(k))
                            || slot.removed_crds.iter().any(|k| served.contains(k));
                        if behind && !again && slot.discovery_retries < DISCOVERY_RETRIES {
                            slot.discovery_retries += 1;
                            retry = true;
                        }
                        tracing::info!(cluster, kinds = kinds.len(), "discovery done");
                        slot.watches.set_kinds(&kinds);
                        slot.discovery = Some(Arc::new(ClusterDiscovery::new(kinds)));
                        None
                    }
                    Err(e) => {
                        tracing::warn!(cluster, error = %e, "discovery failed");
                        Some(e)
                    }
                };
                // A failed discovery is retried with backoff while the cluster is active;
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
                let active = self.active.as_deref() == Some(cluster.as_str());
                let delay = if retry {
                    Some(DISCOVERY_RETRY)
                } else if again {
                    self.start_discovery(&cluster);
                    None
                } else {
                    failed_retry.filter(|_| active)
                };
                if let Some(delay) = delay {
                    let internal = self.internal_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = internal.send(Internal::RetryDiscovery { cluster });
                    });
                }
            }
            // Only while the cluster is active: a timer may outlive a switch to another one.
            Internal::RetryDiscovery { cluster } => {
                if self.active.as_deref() == Some(cluster.as_str()) {
                    self.start_discovery(&cluster);
                }
            }
            Internal::NamespacesListed { cluster, names } => {
                if let Some(slot) = self.cluster_mut(&cluster) {
                    slot.listed_namespaces = names;
                    slot.update_scope();
                }
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
                fingerprint,
                kinds,
            } => {
                let Some(slot) = self.cluster_mut(&cluster) else {
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
                if changed.rearm {
                    // Before reloading, so a file written right after its directory is caught
                    // either by the reload or by the new watcher.
                    self.rebuild_watcher();
                }
                if changed.settings {
                    // First, since settings can change the kubeconfig file list.
                    self.reload_settings().await;
                }
                if changed.kubeconfig {
                    self.reload_kubeconfig().await;
                }
                if changed.activity {
                    self.reload_activity().await;
                }
            }
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
        let slot = self
            .cluster(cluster)
            .filter(|s| s.info.server.as_deref() == pin.server());
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
        self.write_client(cluster).map(|client| (client, pin))
    }

    /// The pin for a session opened on `cluster` now: to the server its slot shows (what a tab
    /// is labelled with), stale if that is not the confirmed one ([`ContextHistory::pin`]).
    fn session_pin(&self, cluster: &str) -> ContextPin {
        match self.cluster(cluster) {
            Some(slot) => self.contexts.pin(cluster, slot.info.server.as_deref()),
            None => self.contexts.pin_confirmed(cluster),
        }
    }

    fn client(&self, cluster: &str) -> Option<Client> {
        self.cluster(cluster)
            .and_then(|c| c.client.as_ref().map(|c| c.read.clone()))
    }

    /// The gate every action that changes a cluster, or runs code in it, goes through
    /// (ADR-0015): the cluster's client, unless the cluster is protected or not connected.
    fn write_client(&self, cluster: &str) -> Result<GatedClient, WriteRefusal> {
        let not_connected = || WriteRefusal::NotConnected {
            cluster: cluster.to_owned(),
        };
        // A context Flint does not know has no client either: say that, not "protected".
        let slot = self.cluster(cluster).ok_or_else(not_connected)?;
        let (server, proxy) = (slot.info.server.as_deref(), slot.info.proxy_url.as_deref());
        let protection = protection(&self.settings, &self.unlocks, cluster, server, proxy);
        if !protection.allows_changes() {
            return Err(WriteRefusal::Protected {
                cluster: cluster.to_owned(),
            });
        }
        slot.client
            .as_ref()
            .ok_or_else(not_connected)?
            .writes
            .grant(protection, cluster)
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
        if self.cluster_mut(&id).is_none() {
            self.emit(Event::Error(format!("cluster \"{id}\" no longer exists")));
            return;
        }
        if self.active.as_deref() != Some(&id) {
            let previous = self.active.replace(id.clone());
            self.emit(Event::ActiveChanged(Some(id.clone())));
            if let Some(previous) = previous {
                // Its watches stop; its client and discovery stay (ADR-0008).
                self.reconcile(&previous);
            }
        }
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

    /// Starts a connection unless one is up or in progress.
    fn ensure_connected(&mut self, id: &str) {
        let kubeconfig = self.kubeconfig.clone();
        let internal = self.internal_tx.clone();
        let attempt = self.next_attempt;
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if matches!(
            slot.state,
            ConnectionState::Connecting | ConnectionState::Connected(_)
        ) {
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
        slot.state = ConnectionState::Connecting;
        slot.client = None;
        let cluster = id.to_owned();
        self.emit(Event::ConnectionChanged {
            cluster: cluster.clone(),
            state: ConnectionState::Connecting,
        });
        tracing::info!(cluster, "connecting");
        tokio::spawn(async move {
            // Run the attempt as its own task so a panic inside kube or an auth plugin handler
            // becomes a failed connection instead of a cluster stuck in "connecting".
            let name = cluster.clone();
            let attempt_task = tokio::spawn(async move {
                cluster::connect_gated_and_check(&kubeconfig.merged, &name, CONNECT_TIMEOUT)
                    .await
                    .map_err(|e| e.summary())
            });
            // The app shows this message as it is: only safe text ([`ClusterError::summary`]).
            let result = attempt_task.await.unwrap_or_else(|e| {
                Err(cluster::SafeErrorText::reduced(&format!(
                    "connection task failed: {e}"
                )))
            });
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

    async fn reload_kubeconfig(&mut self) {
        let paths = self.kubeconfig_paths.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            let loaded = kubeconfig::load(&paths);
            let exist: Vec<(PathBuf, bool)> = loaded
                .paths
                .iter()
                .map(|p| (p.clone(), p.exists()))
                .collect();
            (loaded, exist)
        })
        .await;
        let (loaded, path_exist) = match loaded {
            Ok((loaded, exist)) => (Arc::new(loaded), exist),
            Err(e) => {
                self.emit(Event::Error(format!("kubeconfig reload failed: {e}")));
                return;
            }
        };
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

        let mut old: HashMap<String, Cluster> = self
            .clusters
            .drain(..)
            .map(|c| (c.info.name.clone(), c))
            .collect();
        let mut changed_active = false;
        for info in &loaded.contexts {
            let fingerprint = cluster::context_fingerprint(&loaded.merged, &info.name);
            let slot = match old.remove(&info.name) {
                Some(mut existing) if existing.fingerprint == fingerprint => {
                    existing.info = info.clone();
                    // The default namespace may have changed.
                    existing.update_scope();
                    existing
                }
                Some(old) => {
                    // The context's connection details changed: the client is stale. The fresh
                    // slot has attempt 0, which no in-flight attempt uses. The front end's
                    // namespace filter still applies.
                    if self.active.as_deref() == Some(&info.name) {
                        changed_active = true;
                    }
                    let mut fresh =
                        Cluster::new(info.clone(), fingerprint, &self.hubs, &self.engine_config);
                    fresh.namespace_filter = old.namespace_filter.clone();
                    fresh.update_scope();
                    fresh.generation = self.next_generation();
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
        // A file that failed to load masks the changes it could have made (UP-06).
        self.contexts.record(&loaded);

        let snapshot = ClustersSnapshot {
            clusters: self.clusters.iter().map(Cluster::summary).collect(),
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
            } else if changed_active {
                self.ensure_connected(&active);
            }
        }
    }

    /// (Re)creates the file watcher for the current kubeconfig files and `settings.toml`, if
    /// watching is enabled. Dropping the old watcher stops it.
    fn rebuild_watcher(&mut self) {
        if !self.config.watch_files {
            return;
        }
        self.watcher = None;
        self.watcher = self.watch_files();
    }

    /// Watches the directories holding the kubeconfig files and `settings.toml`, so files that
    /// are replaced by rename or created later are noticed. Each file is matched by its path and
    /// its resolved path (symlink targets, `/var` versus `/private/var`).
    fn watch_files(&self) -> Option<FileWatcher> {
        // Create the config directory, so a settings.toml written there later (by hand or by
        // another Flint) is noticed. Failing is fine: the app's own writes create it too.
        let _ = std::fs::create_dir_all(&self.config.config_dir);
        let kube_files: HashSet<PathBuf> = self
            .kubeconfig_paths
            .iter()
            .flat_map(|p| watch_names(p))
            .collect();
        let settings_path = self.config.config_dir.join(settings::SETTINGS_FILE);
        let settings_files: HashSet<PathBuf> = watch_names(&settings_path).into_iter().collect();
        // Beside settings.toml, so it needs no directory of its own.
        let activity_files: HashSet<PathBuf> = [activity::ACTIVITY_FILE, activity::PREVIOUS_FILE]
            .iter()
            .flat_map(|f| watch_names(&self.config.config_dir.join(f)))
            .collect();
        let (watch_dirs, pending) = watch_plan(kube_files.iter().chain(&settings_files));

        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Changed>();
        let (kube_watched, settings_watched) = (kube_files.clone(), settings_files.clone());
        let watched = Watched {
            pending,
            kube: kube_watched,
            settings: settings_watched,
            activity: activity_files,
        };
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                let changed = classify(&event.paths, &watched);
                if changed.kubeconfig || changed.settings || changed.activity {
                    let _ = raw_tx.send(changed);
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot watch kubeconfig and settings files");
                    return None;
                }
            };
        let mut watched_dirs = HashSet::new();
        for dir in &watch_dirs {
            match watcher.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    watched_dirs.insert(dir.clone());
                }
                Err(e) => tracing::debug!(dir = %dir.display(), error = %e, "not watching dir"),
            }
        }
        let settings_dir_watched = settings_files
            .iter()
            .filter_map(|f| f.parent())
            .any(|d| watched_dirs.contains(&resolved(d)));
        tokio::spawn(debounce(raw_rx, self.internal_tx.clone()));
        Some(FileWatcher {
            _watcher: watcher,
            settings_dir_watched,
        })
    }
}

/// `dir` resolved (symlinks, `..`), or as given if it cannot be.
fn resolved(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// The directories to watch so `files` are noticed, each once by its resolved name (inotify
/// merges a directory's aliases into one watch, and removing one alias's watch would remove
/// the other's), and the missing directories whose creation means the watcher is rebuilt. A
/// directory that does not exist yet cannot be watched: its nearest existing ancestor is.
fn watch_plan<'a>(
    files: impl Iterator<Item = &'a PathBuf>,
) -> (HashSet<PathBuf>, HashSet<PathBuf>) {
    let dirs: HashSet<&Path> = files.filter_map(|f| f.parent()).collect();
    let mut watch_dirs = HashSet::new();
    let mut pending = HashSet::new();
    for dir in dirs {
        match missing_dirs(dir) {
            Some((ancestor, missing)) => {
                watch_dirs.insert(resolved(&ancestor));
                pending.extend(missing);
            }
            None => {
                watch_dirs.insert(resolved(dir));
            }
        }
    }
    (watch_dirs, pending)
}

/// What an event on `paths` changed. A missing directory that appeared (`pending`) may
/// already hold kubeconfig files or `settings.toml` (a symlink's target directory restored
/// with the file in it), so both are read again, besides rebuilding the watcher.
/// The paths a file watcher reacts to.
struct Watched {
    /// Missing directories whose creation means the watcher is rebuilt.
    pending: HashSet<PathBuf>,
    kube: HashSet<PathBuf>,
    settings: HashSet<PathBuf>,
    activity: HashSet<PathBuf>,
}

fn classify(paths: &[PathBuf], watched: &Watched) -> Changed {
    let rearm = paths.iter().any(|p| watched.pending.contains(p));
    let any = |set: &HashSet<PathBuf>| rearm || paths.iter().any(|p| set.contains(p));
    Changed {
        kubeconfig: any(&watched.kube),
        settings: any(&watched.settings),
        activity: any(&watched.activity),
        rearm,
    }
}

/// For a directory that does not exist: its nearest existing ancestor, and every directory
/// from there down to it that is missing (as given and with the ancestor resolved, since
/// events may name either), whose creation means the watcher should be rebuilt. `None` when
/// the directory exists.
fn missing_dirs(dir: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    if dir.is_dir() {
        return None;
    }
    let ancestor = dir.ancestors().skip(1).find(|a| a.is_dir())?.to_path_buf();
    let resolved = std::fs::canonicalize(&ancestor).ok();
    let mut missing = Vec::new();
    for path in dir.ancestors().take_while(|a| *a != ancestor) {
        missing.push(path.to_path_buf());
        if let (Some(resolved), Ok(rest)) = (&resolved, path.strip_prefix(&ancestor)) {
            missing.push(resolved.join(rest));
        }
    }
    Some((ancestor, missing))
}

/// The paths a file may be reported under: as given, resolved (if it exists), and with its
/// directory resolved (if only the directory exists yet).
fn watch_names(path: &Path) -> Vec<PathBuf> {
    let mut names = vec![path.to_path_buf()];
    if let Ok(target) = std::fs::canonicalize(path) {
        names.push(target);
    }
    // A symlink whose target is missing cannot be canonicalized: name its target as the link
    // says it (relative to the link's directory), and with that directory resolved, so
    // restoring the target is noticed.
    if let Ok(link) = std::fs::read_link(path) {
        let target = match path.parent() {
            Some(dir) if link.is_relative() => dir.join(link),
            _ => link,
        };
        if let (Some(dir), Some(name)) = (target.parent(), target.file_name())
            && let Ok(dir) = std::fs::canonicalize(dir)
        {
            names.push(dir.join(name));
        }
        names.push(target);
    }
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
        && let Ok(dir) = std::fs::canonicalize(dir)
    {
        names.push(dir.join(name));
    }
    names
}

/// Ends when the watcher is dropped (its sender closes) or the manager stops.
async fn debounce(mut raw: mpsc::UnboundedReceiver<Changed>, out: mpsc::UnboundedSender<Internal>) {
    while let Some(first) = raw.recv().await {
        tokio::time::sleep(RELOAD_DEBOUNCE).await;
        let mut changed = first;
        while let Ok(more) = raw.try_recv() {
            changed.kubeconfig |= more.kubeconfig;
            changed.settings |= more.settings;
            changed.activity |= more.activity;
            changed.rearm |= more.rearm;
        }
        if out.send(Internal::FilesChanged(changed)).is_err() {
            break;
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
async fn follow_crds(mut follower: Follower, out: mpsc::UnboundedSender<Internal>) {
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
async fn follow_namespaces(mut follower: Follower, out: mpsc::UnboundedSender<Internal>) {
    let mut last: Option<Vec<String>> = None;
    while let Some(snapshot) = follower.next_synced(Duration::ZERO).await {
        let names: Vec<String> = snapshot.names().into_iter().map(str::to_owned).collect();
        if last.as_ref() == Some(&names) {
            continue;
        }
        last = Some(names.clone());
        let msg = Internal::NamespacesListed {
            cluster: follower.cluster.clone(),
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
        assert!(manager.cluster("dev").is_none(), "its contexts are skipped");
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
        let error = log_error_on_a_fallback(false).await;
        assert!(error.contains("can't be read"), "{error}");
    }

    // covers UP-06, UP-40: a log tab opened on the fallback, its request taken only once the
    // first file is back: pinned to the server the tab shows, not to where the context points
    // when the core takes it.
    #[tokio::test]
    async fn open_logs_taken_after_the_fallback_went_keeps_the_tabs_server() {
        let error = log_error_on_a_fallback(true).await;
        assert!(error.contains("may be defined differently"), "{error}");
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
        manager.kubeconfig = Arc::new(loaded);
        manager.clusters.push(slot);

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
        unlocks.lock("dev");
        assert_eq!(manager.write_client("dev").err(), Some(protected("dev")));

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
            pending: HashSet::from([PathBuf::from("/cfg/dotfiles")]),
            kube: HashSet::from([PathBuf::from("/k/config")]),
            settings: HashSet::from([PathBuf::from("/cfg/dotfiles/flint.toml")]),
            activity: HashSet::from([PathBuf::from("/cfg/agent-activity.jsonl")]),
        };
        let changed = classify(&[PathBuf::from("/cfg/dotfiles")], &watched);
        assert!(changed.rearm && changed.settings && changed.kubeconfig && changed.activity);
        let changed = classify(&[PathBuf::from("/k/config")], &watched);
        assert!(!changed.rearm && !changed.settings && changed.kubeconfig && !changed.activity);
        let changed = classify(&[PathBuf::from("/cfg/dotfiles/flint.toml")], &watched);
        assert!(!changed.rearm && changed.settings && !changed.kubeconfig && !changed.activity);
        // covers UP-16: an agent's call reloads only the activity log.
        let changed = classify(&[PathBuf::from("/cfg/agent-activity.jsonl")], &watched);
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

    // covers UP-24
    #[tokio::test]
    async fn a_stopped_follower_asks_for_its_hub_to_be_released() {
        let hubs = HubRegistry::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let crds = KindId::custom_resource_definitions();
        let follower = Follower::new(&hubs, "dev", crds.clone(), &tx);
        let task = tokio::spawn(follow_crds(follower, tx.clone()));
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
