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

use crate::cluster::{self, CONNECT_TIMEOUT, ClusterId, ServerVersion};
use crate::discovery::ClusterDiscovery;
use crate::handle::{
    ClusterSummary, ClustersSnapshot, Command, ConnectionState, CoreConfig, Event, Request,
};
use crate::kubeconfig::{self, ContextInfo, LoadedKubeconfig};
use crate::logs::{self, KubeLogSource, LogError, SessionConfig};
use crate::read::{self, KindId, KindInfo};
use crate::settings::{self, Settings};
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
    client: Option<Client>,
    /// Id of the latest connection attempt, from the manager-wide counter, so a late result of an
    /// older attempt (even one made before the context was removed and re-added) is ignored.
    attempt: u64,
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
        }
    }
}

enum Internal {
    Connected {
        cluster: ClusterId,
        attempt: u64,
        result: Result<(Client, ServerVersion), String>,
    },
    FilesChanged(Changed),
    Discovered {
        cluster: ClusterId,
        attempt: u64,
        result: Result<Vec<KindInfo>, String>,
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
        /// The API server the session was opened against.
        server: Option<String>,
        reply: oneshot::Sender<Result<Client, LogError>>,
    },
}

/// Which watched files changed.
#[derive(Debug, Clone, Copy, Default)]
struct Changed {
    kubeconfig: bool,
    settings: bool,
}

pub(crate) struct ClusterManager {
    config: CoreConfig,
    events: async_channel::Sender<Event>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: Option<mpsc::UnboundedReceiver<Internal>>,
    kubeconfig: Arc<LoadedKubeconfig>,
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
}

struct FileWatcher {
    _watcher: RecommendedWatcher,
    /// Whether the config directory is watched. It may not exist until settings are first written.
    settings_dir_watched: bool,
}

impl ClusterManager {
    pub(crate) fn new(
        config: CoreConfig,
        events: async_channel::Sender<Event>,
        hubs: HubRegistry,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let kubeconfig_paths = kubeconfig::paths_for_settings(
            &config.kubeconfig_base,
            &config.settings,
            config.home.as_deref(),
        );
        Self {
            settings: Arc::new(config.settings.clone()),
            settings_error: config.settings_error.clone(),
            config,
            events,
            internal_tx,
            internal_rx: Some(internal_rx),
            kubeconfig: Arc::default(),
            kubeconfig_paths,
            clusters: Vec::new(),
            active: None,
            next_attempt: 1,
            watcher: None,
            hubs,
            engine_config: Arc::new(EngineConfig::default()),
            subscriptions: HashMap::new(),
            log_sessions: HashMap::new(),
        }
    }

    pub(crate) async fn run(
        mut self,
        commands: async_channel::Receiver<Command>,
        requests: async_channel::Receiver<Request>,
    ) {
        let mut internal_rx = self.internal_rx.take().expect("run is called once");
        self.emit_settings();
        self.reload_kubeconfig().await;
        self.rebuild_watcher();
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
        // An unbounded channel only fails when the receiver is gone, i.e. the front end quit.
        let _ = self.events.try_send(event);
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
            Command::SetAiExposed { cluster, server } => self.set_ai_exposed(cluster, server).await,
            Command::ReloadKubeconfig => self.reload_kubeconfig().await,
            Command::SetWatchMode { cluster, mode } => {
                self.write_settings("the watch mode", move |dir| {
                    settings::set_watch_mode(dir, cluster.as_deref(), mode)
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
                target,
                options,
                snapshots,
                paused,
            } => {
                let internal = self.internal_tx.clone();
                // Pinned now: a reconnect must never read logs from another server that the
                // context was re-pointed to in the meantime (UP-06).
                let server = self
                    .clusters
                    .iter()
                    .find(|c| c.info.name == cluster)
                    .and_then(|c| c.info.server.clone());
                let lookup: logs::ClientLookup = Arc::new(move || {
                    let (reply, rx) = oneshot::channel();
                    let _ = internal.send(Internal::LogClient {
                        cluster: cluster.clone(),
                        server: server.clone(),
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
                let source = Arc::new(KubeLogSource::new(lookup));
                let task = tokio::spawn(logs::run(
                    source, target, options, config, snapshots, paused,
                ));
                self.log_sessions.retain(|_, t| !t.is_finished());
                self.log_sessions.insert(id, task);
            }
            Request::CloseLogs { id } => {
                if let Some(task) = self.log_sessions.remove(&id) {
                    task.abort();
                }
            }
            Request::Client { cluster, reply } => {
                let client = self
                    .clusters
                    .iter()
                    .find(|c| c.info.name == cluster)
                    .and_then(|c| c.client.clone());
                let _ = reply.send(client);
            }
            Request::FetchObject {
                cluster,
                kind,
                namespace,
                name,
                reply,
            } => {
                let found = self.clusters.iter().find(|c| c.info.name == cluster);
                let client = found.and_then(|c| c.client.clone());
                let info = found
                    .and_then(|c| c.discovery.as_ref())
                    .and_then(|d| d.kind(&kind).cloned());
                let (client, info) = match (client, info) {
                    (Some(c), Some(i)) => (c, i),
                    (None, _) => {
                        let _ = reply.send(Err(format!("\"{cluster}\" is not connected")));
                        return;
                    }
                    (_, None) => {
                        let _ = reply.send(Err(format!("\"{cluster}\" does not serve {kind}")));
                        return;
                    }
                };
                tokio::spawn(async move {
                    let result = read::object_detail(&client, &info, namespace.as_deref(), &name)
                        .await
                        .map(Arc::new)
                        .map_err(|e| e.to_string());
                    let _ = reply.send(result);
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
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        let Some(client) = slot.client.clone() else {
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
                .map_err(|e| e.to_string());
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

    async fn set_ai_exposed(&mut self, cluster: ClusterId, server: Option<String>) {
        if let Some(server) = &server {
            // The user confirmed this server. If the context moved since, do not expose the new
            // one on the strength of that confirmation.
            let current = self
                .clusters
                .iter()
                .find(|c| c.info.name == cluster)
                .and_then(|c| c.info.server.as_deref());
            if current != Some(server.as_str()) {
                self.emit(Event::Error(format!(
                    "\"{cluster}\" now points to a different server; it was not exposed. Review it and try again."
                )));
                return;
            }
        }
        if let Some(error) = &self.settings_error {
            // Rewriting a file that does not parse would fail anyway; say why.
            self.emit(Event::Error(format!(
                "settings.toml must be fixed before exposure can change: {}",
                crate::redact::safe_error_summary(error)
            )));
            return;
        }
        let dir = self.config.config_dir.clone();
        let result = tokio::task::spawn_blocking(move || {
            settings::set_ai_exposed(&dir, &cluster, server.as_deref())
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
            }
            // A settings write error can quote the file: one sanitized line.
            Ok(Err(e)) => self.emit(Event::Error(crate::redact::safe_error_summary(
                &e.to_string(),
            ))),
            Err(e) => self.emit(Event::Error(format!("settings write failed: {e}"))),
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
                // Expose nothing (as the MCP server does), but keep the other settings, so a
                // typo does not make clusters from extra kubeconfig files disappear.
                tracing::warn!(
                    "settings.toml unreadable; nothing is exposed to AI: {}",
                    crate::redact::safe_error_summary(&e)
                );
                let kept = Settings {
                    ai_exposed_clusters: Vec::new(),
                    ..(*self.settings).clone()
                };
                (kept, Some(e))
            }
        };
        if *self.settings == settings && self.settings_error == error {
            return;
        }
        self.settings = Arc::new(settings);
        self.settings_error = error;
        self.emit_settings();
        // The watch mode may have changed.
        self.reconcile_all();
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
            Internal::Connected {
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
                let connected = result.is_ok();
                match result {
                    Ok((client, version)) => {
                        slot.watches
                            .set_source(Some(KubeSource::new(client.clone(), Some(&version))));
                        slot.client = Some(client);
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
                let error = match result {
                    Ok(kinds) => {
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
                let failed_retry = if error.is_some() {
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
                self.emit(Event::DiscoveryChanged {
                    cluster: cluster.clone(),
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
                server,
                reply,
            } => {
                let _ = reply.send(self.log_client(&cluster, server.as_deref()));
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
                if changed.settings {
                    // First, since settings can change the kubeconfig file list.
                    self.reload_settings().await;
                }
                if changed.kubeconfig {
                    self.reload_kubeconfig().await;
                }
            }
        }
    }

    /// The client a log session should use for `cluster` now. A cluster that was never
    /// connected is connected (the session asked for it); one whose connection failed is not
    /// retried here, so a broken exec plugin does not run again at every session retry. A
    /// context that now points at another API server than `server` (the one the session was
    /// opened against) ends the session.
    fn log_client(&mut self, cluster: &str, server: Option<&str>) -> Result<Client, LogError> {
        let Some(slot) = self.clusters.iter().find(|c| c.info.name == cluster) else {
            return Err(LogError::fatal(format!(
                "\"{cluster}\" is no longer in your kubeconfig files"
            )));
        };
        if slot.info.server.as_deref() != server {
            return Err(LogError::fatal(format!(
                "\"{cluster}\" now points to a different API server; open the logs again to read \
                 from it"
            )));
        }
        if let Some(client) = &slot.client {
            return Ok(client.clone());
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

    fn cluster_mut(&mut self, id: &str) -> Option<&mut Cluster> {
        self.clusters.iter_mut().find(|c| c.info.name == id)
    }

    fn activate(&mut self, id: ClusterId) {
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
                cluster::connect_and_check(&kubeconfig.merged, &name, CONNECT_TIMEOUT)
                    .await
                    .map_err(|e| e.to_string())
            });
            let result = attempt_task
                .await
                .unwrap_or_else(|e| Err(format!("connection task failed: {e}")));
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
                    fresh
                }
                None => Cluster::new(info.clone(), fingerprint, &self.hubs, &self.engine_config),
            };
            self.clusters.push(slot);
        }
        self.kubeconfig = loaded.clone();

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
        let dirs: HashSet<PathBuf> = kube_files
            .iter()
            .chain(&settings_files)
            .filter_map(|f| f.parent().map(Path::to_path_buf))
            .collect();

        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Changed>();
        let (kube_watched, settings_watched) = (kube_files.clone(), settings_files.clone());
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                let changed = Changed {
                    kubeconfig: event.paths.iter().any(|p| kube_watched.contains(p)),
                    settings: event.paths.iter().any(|p| settings_watched.contains(p)),
                };
                if changed.kubeconfig || changed.settings {
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
        for dir in &dirs {
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
            .any(|d| watched_dirs.contains(d));
        tokio::spawn(debounce(raw_rx, self.internal_tx.clone()));
        Some(FileWatcher {
            _watcher: watcher,
            settings_dir_watched,
        })
    }
}

/// The paths a file may be reported under: as given, resolved (if it exists), and with its
/// directory resolved (if only the directory exists yet).
fn watch_names(path: &Path) -> Vec<PathBuf> {
    let mut names = vec![path.to_path_buf()];
    if let Ok(target) = std::fs::canonicalize(path) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn context(namespace: Option<&str>) -> ContextInfo {
        ContextInfo {
            name: "dev".into(),
            cluster: None,
            user: None,
            namespace: namespace.map(str::to_owned),
            server: Some("https://dev".into()),
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

    // covers UP-06, UP-40
    #[test]
    fn a_log_session_never_reconnects_to_a_re_pointed_context() {
        let (events, _rx) = async_channel::unbounded();
        let config = CoreConfig {
            config_dir: PathBuf::from("/nonexistent"),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: Vec::new(),
            home: None,
            watch_files: false,
        };
        let hubs = HubRegistry::default();
        let mut manager = ClusterManager::new(config, events, hubs.clone());
        let mut slot = Cluster::new(context(None), None, &hubs, &manager.engine_config);
        slot.state = ConnectionState::Failed("offline".into());
        manager.clusters.push(slot);

        let same = manager
            .log_client("dev", Some("https://dev"))
            .err()
            .expect("no client");
        assert!(!same.fatal, "the same server is retried: {}", same.message);
        let moved = manager
            .log_client("dev", Some("https://old"))
            .err()
            .expect("no client");
        assert!(moved.fatal);
        assert!(
            moved.message.contains("different API server"),
            "{}",
            moved.message
        );
        let gone = manager.log_client("other", None).err().expect("no client");
        assert!(gone.fatal);
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
}
