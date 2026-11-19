//! The bridge between a UI (or any other front end) and the core (ADR-0002).
//!
//! The core runs on its own multi-thread tokio runtime. Front ends send [`Command`]s and receive
//! [`Event`]s over `async_channel` channels, which work with any executor, so a GPUI task can
//! await them directly. Nothing here blocks the caller.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{oneshot, watch};

use crate::cluster::{ClusterId, ServerVersion};
use crate::discovery::ClusterDiscovery;
use crate::kubeconfig::ContextInfo;
use crate::logs::{LogLines, LogOptions, LogSearch, LogSnapshot, LogTarget};
use crate::manager::ClusterManager;
use crate::read::{KindId, ObjectDetail};
use crate::settings::{Appearance, Settings, WatchMode};
use crate::store::{Feed, KindSnapshot, SyncState};
use crate::view::{self, ViewQuery};
use crate::watch::HubRegistry;

/// What the core needs to start. Usually built from [`crate::bootstrap::Bootstrap`].
#[derive(Debug, Clone)]
pub struct CoreConfig {
    /// Flint's config directory (`settings.toml`, `state.toml`).
    pub config_dir: PathBuf,
    /// Settings loaded at startup.
    pub settings: Settings,
    /// Why `settings.toml` could not be loaded at startup, if it could not.
    pub settings_error: Option<String>,
    /// Kubeconfig files from `KUBECONFIG` or `~/.kube/config`, in merge order. The settings'
    /// `kubeconfig_paths` are appended, and recomputed when settings change.
    pub kubeconfig_base: Vec<PathBuf>,
    /// The home directory, for `~` in the settings' `kubeconfig_paths`.
    pub home: Option<PathBuf>,
    /// Watch the kubeconfig files and `settings.toml`, and reload on change.
    pub watch_files: bool,
}

/// Requests from the front end.
#[derive(Debug, Clone)]
pub enum Command {
    /// Make a cluster the active one, connecting it if it is not connected. Selecting a cluster
    /// whose connection failed retries it.
    Activate(ClusterId),
    /// Expose a cluster to AI agents at `server` (`Some`), or hide it (`None`), in
    /// `settings.toml`. `server` is the URL the user confirmed; if the context no longer points
    /// there, nothing is written and an error is reported.
    SetAiExposed {
        cluster: ClusterId,
        server: Option<String>,
    },
    /// Reload the kubeconfig files now.
    ReloadKubeconfig,
    /// Set the watch mode and write it to `settings.toml` (ADR-0008). `cluster: None` sets the
    /// global mode; `Some` sets that cluster's override, and `mode: None` then removes the
    /// override. The change applies at once and arrives back as `Event::SettingsChanged`.
    SetWatchMode {
        cluster: Option<ClusterId>,
        mode: Option<WatchMode>,
    },
    /// Set the appearance (light, dark, or following macOS) and write it to `settings.toml`. The
    /// change arrives back as `Event::SettingsChanged`.
    SetAppearance(Appearance),
    /// The namespaces the user filters `cluster` on (empty: all). When the user may not list a
    /// kind in all namespaces, it is listed in these one by one; without a filter, in the
    /// context's default namespace, else in every namespace the user can list. The filter
    /// itself is applied by the front end and is not stored by the core.
    SetNamespaceFilter {
        cluster: ClusterId,
        namespaces: Vec<String>,
    },
    /// Set the lines each log session keeps and write it to `settings.toml`. Sessions opened
    /// afterwards use it. The change arrives back as `Event::SettingsChanged`.
    SetLogBufferLines(u32),
}

/// Requests that carry a reply channel or come from a [`Subscription`].
pub(crate) enum Request {
    Subscribe {
        id: u64,
        cluster: ClusterId,
        kind: KindId,
    },
    Unsubscribe {
        id: u64,
    },
    FetchObject {
        cluster: ClusterId,
        kind: KindId,
        namespace: Option<String>,
        name: String,
        reply: oneshot::Sender<Result<Arc<ObjectDetail>, String>>,
    },
    Discovery {
        cluster: ClusterId,
        reply: oneshot::Sender<Option<Arc<ClusterDiscovery>>>,
    },
    WatchedKinds {
        cluster: ClusterId,
        reply: oneshot::Sender<Vec<(KindId, Feed)>>,
    },
    OpenLogs {
        id: u64,
        cluster: ClusterId,
        target: LogTarget,
        options: LogOptions,
        snapshots: watch::Sender<Arc<LogSnapshot>>,
        paused: watch::Receiver<bool>,
    },
    CloseLogs {
        id: u64,
    },
}

/// Connection state of one cluster.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// Nothing has been tried yet. No network traffic happens until a cluster is activated.
    #[default]
    Disconnected,
    Connecting,
    Connected(ServerVersion),
    Failed(String),
}

/// A cluster as the sidebar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterSummary {
    pub info: ContextInfo,
    pub state: ConnectionState,
}

/// The full cluster list. Sent whole whenever kubeconfig is (re)loaded.
#[derive(Debug, Clone, Default)]
pub struct ClustersSnapshot {
    pub clusters: Vec<ClusterSummary>,
    /// Kubeconfig files that could not be read or parsed. The error text may quote the file;
    /// show it through [`crate::redact::safe_error_summary`].
    pub errors: Vec<(PathBuf, String)>,
    /// Every kubeconfig path considered, and whether it existed when it was loaded.
    pub paths: Vec<(PathBuf, bool)>,
}

/// Notifications from the core. Snapshots are shared and read-only.
#[derive(Debug, Clone)]
pub enum Event {
    ClustersChanged(Arc<ClustersSnapshot>),
    ActiveChanged(Option<ClusterId>),
    ConnectionChanged {
        cluster: ClusterId,
        state: ConnectionState,
    },
    /// Settings were loaded, reloaded, or written. When `settings.toml` cannot be read, `error`
    /// says why and `settings` are the defaults (nothing exposed), as the MCP server also treats
    /// an unreadable file as nothing exposed.
    SettingsChanged {
        settings: Arc<Settings>,
        error: Option<String>,
    },
    /// Discovery ran for a cluster: at connect, and again when its CRDs change. On failure,
    /// `error` is set and `discovery` holds the previous result, if any.
    DiscoveryChanged {
        cluster: ClusterId,
        discovery: Option<Arc<ClusterDiscovery>>,
        error: Option<String>,
    },
    /// A problem worth showing that is not tied to one cluster's connection.
    Error(String),
}

/// Core work is mostly waiting on the network; blocking work (file reads, exec plugins) runs on
/// tokio's separate blocking pool.
const CORE_WORKER_THREADS: usize = 4;

/// The front end's handle to the core. Cheap to clone; the runtime stops when the last clone is
/// dropped.
#[derive(Clone)]
pub struct CoreHandle {
    commands: async_channel::Sender<Command>,
    requests: async_channel::Sender<Request>,
    events: async_channel::Receiver<Event>,
    hubs: HubRegistry,
    next_subscription: Arc<AtomicU64>,
    runtime: tokio::runtime::Handle,
    _runtime: Arc<RuntimeGuard>,
}

impl CoreHandle {
    /// Starts the tokio runtime on background threads and the cluster manager on it.
    pub fn start(config: CoreConfig) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(CORE_WORKER_THREADS)
            .thread_name("flint-core")
            .enable_all()
            .build()?;
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (req_tx, req_rx) = async_channel::unbounded();
        let (event_tx, event_rx) = async_channel::unbounded();
        let hubs = HubRegistry::default();
        runtime.spawn(ClusterManager::new(config, event_tx, hubs.clone()).run(cmd_rx, req_rx));
        Ok(Self {
            commands: cmd_tx,
            requests: req_tx,
            events: event_rx,
            hubs,
            next_subscription: Arc::new(AtomicU64::new(1)),
            runtime: runtime.handle().clone(),
            _runtime: Arc::new(RuntimeGuard(Some(runtime))),
        })
    }

    /// Opens a live list of `kind` on `cluster`. While the subscription lives, the kind is
    /// watched whenever the cluster is active, in both watch modes. The first
    /// [`Subscription::changed`] resolves at once; later ones at most about every 100 ms, however
    /// fast the cluster changes. Dropping the subscription closes it. Never blocks.
    pub fn subscribe(&self, cluster: &str, kind: &KindId) -> Subscription {
        let (hub, mut rx) = self.hubs.subscribe(cluster, kind);
        rx.mark_changed();
        // A running watch publishes what it held back while nobody listened.
        hub.wake.notify_one();
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.try_send(Request::Subscribe {
            id,
            cluster: cluster.to_owned(),
            kind: kind.clone(),
        });
        Subscription {
            id,
            cluster: cluster.to_owned(),
            kind: kind.clone(),
            rx: Some(rx),
            requests: self.requests.clone(),
        }
    }

    /// Fetches one object (managedFields removed, rendered as YAML too) and the events about
    /// it, for a detail view. The future works on any executor and fails if the cluster is not
    /// connected or does not serve `kind`.
    pub fn fetch_object(
        &self,
        cluster: &str,
        kind: &KindId,
        namespace: Option<&str>,
        name: &str,
    ) -> impl Future<Output = Result<Arc<ObjectDetail>, String>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.try_send(Request::FetchObject {
            cluster: cluster.to_owned(),
            kind: kind.clone(),
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
            reply,
        });
        async move {
            if sent.is_err() {
                return Err("the core is not running".to_owned());
            }
            rx.await
                .unwrap_or_else(|_| Err("the core stopped before answering".to_owned()))
        }
    }

    /// The cluster's current discovery results (navigation and kinds), if discovery has run.
    /// The same value also arrives as [`Event::DiscoveryChanged`]; this serves views that open
    /// later. Works on any executor.
    pub fn discovery(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Option<Arc<ClusterDiscovery>>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.try_send(Request::Discovery {
            cluster: cluster.to_owned(),
            reply,
        });
        async move { rx.await.ok().flatten() }
    }

    /// The kinds of `cluster` being watched (or polled) right now, sorted. For status displays
    /// ("watching 52 kinds") and tests. Works on any executor.
    pub fn watched_kinds(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Vec<(KindId, Feed)>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.try_send(Request::WatchedKinds {
            cluster: cluster.to_owned(),
            reply,
        });
        async move { rx.await.unwrap_or_default() }
    }

    /// The sync state of each kind of `cluster` that is watched, polled or subscribed right now,
    /// sorted by kind, read without subscribing (so it never starts a watch in On view mode).
    /// For status displays such as the navigation's per-kind markers. Takes a short lock and
    /// never waits on core work, so the UI thread may call it.
    pub fn sync_states(&self, cluster: &str) -> Vec<(KindId, SyncState)> {
        self.hubs.states(cluster)
    }

    /// Runs [`view::compute`] (namespace filter, text filter, sort) on the core's threads, so a
    /// list of 10,000 rows never stalls the UI thread. Resolves to indexes into
    /// `snapshot.rows`. Works on any executor.
    pub fn compute_view(
        &self,
        snapshot: Arc<KindSnapshot>,
        query: ViewQuery,
    ) -> impl Future<Output = Arc<[u32]>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn_blocking(move || {
            let rows = view::compute(&snapshot, &query, crate::table::unix_now());
            let _ = tx.send(Arc::<[u32]>::from(rows));
        });
        async move { rx.await.unwrap_or_else(|_| Arc::from(Vec::new())) }
    }

    /// Opens a log session: streams `target`'s log on `cluster` into a ring buffer of
    /// [`crate::Settings::log_buffer_limit`] lines, reconnecting when the stream drops. The
    /// session keeps running when another cluster becomes active, until the returned
    /// [`LogSession`] is dropped. Snapshots arrive at most about every 100 ms. Never blocks.
    pub fn open_logs(&self, cluster: &str, target: LogTarget, options: LogOptions) -> LogSession {
        let (snapshots, mut rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        rx.mark_changed();
        let (paused, paused_rx) = watch::channel(false);
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.try_send(Request::OpenLogs {
            id,
            cluster: cluster.to_owned(),
            target: target.clone(),
            options: options.clone(),
            snapshots,
            paused: paused_rx,
        });
        LogSession {
            id,
            cluster: cluster.to_owned(),
            target,
            options,
            rx,
            paused,
            requests: self.requests.clone(),
        }
    }

    /// Brings `search` up to date with `lines` on the core's threads (see [`LogSearch`]). Use it
    /// for the first scan of a new query over a full buffer; later updates while following
    /// scan only new lines and are cheap enough to run anywhere. Works on any executor.
    pub fn search_logs(
        &self,
        mut search: LogSearch,
        lines: LogLines,
    ) -> impl Future<Output = LogSearch> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let query = search.query().clone();
        self.runtime.spawn_blocking(move || {
            search.update(&lines);
            let _ = tx.send(search);
        });
        async move { rx.await.unwrap_or_else(|_| LogSearch::new(query)) }
    }

    /// Sends a command. Never blocks. Commands sent after the core stopped are dropped.
    pub fn send(&self, command: Command) {
        if self.commands.try_send(command).is_err() {
            tracing::warn!("core is not running; command dropped");
        }
    }

    /// The event stream. There should be one consumer: each event goes to one receiver only.
    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }
}

/// A live list of one kind on one cluster, from [`CoreHandle::subscribe`].
///
/// The core publishes immutable snapshots; this only ever holds the newest, so a slow reader
/// skips intermediate states instead of queueing them. Dropping it unsubscribes.
pub struct Subscription {
    id: u64,
    cluster: ClusterId,
    kind: KindId,
    /// `None` only while dropping, so the receiver is gone before the manager hears about it.
    rx: Option<watch::Receiver<Arc<KindSnapshot>>>,
    requests: async_channel::Sender<Request>,
}

impl Subscription {
    fn rx(&self) -> &watch::Receiver<Arc<KindSnapshot>> {
        self.rx.as_ref().expect("set until drop")
    }

    fn rx_mut(&mut self) -> &mut watch::Receiver<Arc<KindSnapshot>> {
        self.rx.as_mut().expect("set until drop")
    }

    /// The cluster this subscription lists.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    /// The kind this subscription lists.
    pub fn kind(&self) -> &KindId {
        &self.kind
    }

    /// Waits until a snapshot newer than the last one read with [`Subscription::latest`] is
    /// published. Returns `false` once the core has stopped. Works on any executor.
    pub async fn changed(&mut self) -> bool {
        self.rx_mut().changed().await.is_ok()
    }

    /// The newest snapshot, marking it as seen.
    pub fn latest(&mut self) -> Arc<KindSnapshot> {
        self.rx_mut().borrow_and_update().clone()
    }

    /// The newest snapshot, without marking it as seen.
    pub fn peek(&self) -> Arc<KindSnapshot> {
        self.rx().borrow().clone()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Drop the receiver first: the manager releases the hub only when nobody listens.
        self.rx = None;
        let _ = self.requests.try_send(Request::Unsubscribe { id: self.id });
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("cluster", &self.cluster)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A running log session, from [`CoreHandle::open_logs`]. Holds only the newest snapshot, so a
/// slow reader skips intermediate ones. Dropping it stops the session.
pub struct LogSession {
    id: u64,
    cluster: ClusterId,
    target: LogTarget,
    options: LogOptions,
    rx: watch::Receiver<Arc<LogSnapshot>>,
    paused: watch::Sender<bool>,
    requests: async_channel::Sender<Request>,
}

impl LogSession {
    /// The cluster the session reads from, whichever cluster is active.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    pub fn target(&self) -> &LogTarget {
        &self.target
    }

    pub fn options(&self) -> &LogOptions {
        &self.options
    }

    /// Waits until a snapshot newer than the last one read with [`LogSession::latest`] is
    /// published. The first call resolves at once. Returns `false` once the core has stopped.
    /// Works on any executor.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    /// The newest snapshot, marking it as seen.
    pub fn latest(&mut self) -> Arc<LogSnapshot> {
        self.rx.borrow_and_update().clone()
    }

    /// The newest snapshot, without marking it as seen.
    pub fn peek(&self) -> Arc<LogSnapshot> {
        self.rx.borrow().clone()
    }

    /// Pauses (`true`) or resumes delivery. While paused, snapshots keep the lines shown when
    /// the session was paused and count the lines held back; the buffer keeps filling (and
    /// evicting), so nothing is lost unless it overflows. Resuming shows everything at once.
    pub fn set_paused(&self, paused: bool) {
        self.paused.send_replace(paused);
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }
}

impl Drop for LogSession {
    fn drop(&mut self) {
        let _ = self.requests.try_send(Request::CloseLogs { id: self.id });
    }
}

impl std::fmt::Debug for LogSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogSession")
            .field("cluster", &self.cluster)
            .field("target", &self.target)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

struct RuntimeGuard(Option<tokio::runtime::Runtime>);

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        // `shutdown_background` does not block, so dropping the handle is safe from any context,
        // including the UI thread and async tests.
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn start(dir: &std::path::Path) -> CoreHandle {
        CoreHandle::start(CoreConfig {
            config_dir: dir.to_owned(),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: Vec::new(),
            home: None,
            watch_files: false,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_subscription_resolves_at_once_and_releases_its_hub_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let mut sub = core.subscribe("nowhere", &KindId::pods());
        assert_eq!(sub.cluster(), "nowhere");
        tokio::time::timeout(Duration::from_secs(5), sub.changed())
            .await
            .expect("the first change resolves at once");
        assert_eq!(sub.latest().state, SyncState::Idle);
        assert_eq!(core.sync_states("nowhere").len(), 1);
        drop(sub);
        // The manager releases the hub once it hears about the drop.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !core.sync_states("nowhere").is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("hub released");
    }

    #[tokio::test]
    async fn requests_for_an_unknown_cluster_answer_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let err = core
            .fetch_object("nowhere", &KindId::pods(), Some("ns"), "p")
            .await
            .unwrap_err();
        assert!(err.contains("not connected"), "{err}");
        assert!(core.discovery("nowhere").await.is_none());
        assert!(core.watched_kinds("nowhere").await.is_empty());
    }

    // covers UP-40
    #[tokio::test]
    async fn a_log_session_on_an_unknown_cluster_ends_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let target = LogTarget {
            namespace: "ns".into(),
            pod: "p".into(),
            container: None,
        };
        let mut session = core.open_logs("nowhere", target.clone(), LogOptions::default());
        assert_eq!(session.cluster(), "nowhere");
        assert_eq!(session.target(), &target);
        assert!(session.options().follow);
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(session.changed().await, "core running");
                let snapshot = session.latest();
                if let crate::logs::LogState::Ended { error } = &snapshot.state {
                    return error.clone();
                }
            }
        })
        .await
        .expect("the session ended");
        assert!(ended.unwrap().contains("nowhere"));
        session.set_paused(true);
        assert!(session.is_paused());
        // An ended session still answers, until it is dropped.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !session.latest().paused {
                assert!(session.changed().await);
            }
        })
        .await
        .expect("pause shown");
    }

    #[tokio::test]
    async fn search_logs_runs_off_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let mut buffer = crate::logs::LogBuffer::new(10);
        buffer.push(crate::logs::LogLine::parse("needle", false));
        buffer.push(crate::logs::LogLine::parse("hay", false));
        let search = core
            .search_logs(
                LogSearch::new(crate::logs::SearchQuery::new("NEEDLE")),
                buffer.view(),
            )
            .await;
        assert_eq!(search.matches(), &[0]);
    }

    #[tokio::test]
    async fn compute_view_runs_off_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let rows = core
            .compute_view(Arc::new(KindSnapshot::default()), ViewQuery::default())
            .await;
        assert!(rows.is_empty());
    }
}
