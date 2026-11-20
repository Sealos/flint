//! The bridge between a UI (or any other front end) and the core (ADR-0002).
//!
//! The core runs on its own multi-thread tokio runtime. Front ends send [`Command`]s over
//! `async_channel` channels and read [`Event`]s from a bounded queue ([`crate::events`]) that
//! keeps the latest state of each kind; both work with any executor, so a GPUI task can await
//! them directly. Nothing here blocks the caller.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{oneshot, watch};

use crate::cluster::{ClusterId, ServerVersion};
use crate::discovery::ClusterDiscovery;
use crate::exec::{ContextCheck, ContextWatch, ExecRequest, ExecSession};
use crate::kubeconfig::ContextInfo;
use crate::logs::{
    Freshness, LogLines, LogOptions, LogSearch, LogSnapshot, LogSubject, LogTarget, PodSet,
    WorkloadPod,
};
use crate::manager::ClusterManager;
use crate::protection::{SessionUnlocks, WriteRefusal};
use crate::read::{KindId, ObjectDetail};
use crate::recheck::{ContextPin, RECHECK_EVERY};
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
    /// Retry timings of the watch engine. Leave at the default; tests against a fake API
    /// server shorten them.
    pub watch_timing: WatchTiming,
}

/// How long the watch engine waits before retrying. The defaults suit real API servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchTiming {
    /// First wait after a broken watch or failed list; doubles up to `backoff_max`.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// First wait after a 403 or 404, which is retried a few times before it is believed.
    pub denied_backoff_min: Duration,
}

impl Default for WatchTiming {
    fn default() -> Self {
        WatchTiming {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            denied_backoff_min: Duration::from_millis(500),
        }
    }
}

/// Requests from the front end.
#[derive(Debug, Clone)]
pub enum Command {
    /// Make a cluster the active one, connecting it if it is not connected. Selecting a cluster
    /// whose connection failed retries it.
    Activate(ClusterId),
    /// Expose a cluster to AI agents at `server` (`Some`), limited to `scope` (ADR-0013), or hide
    /// it (`None`), in `settings.toml`. `server` is the URL the user confirmed; if the context
    /// no longer points there, nothing is written and an error is reported. Exposing an exposed
    /// cluster again rewrites its entry with the new scope in one write.
    SetAiExposed {
        cluster: ClusterId,
        server: Option<String>,
        scope: crate::settings::AiScope,
    },
    /// Change what agents may read of a cluster exposed at `server` to `scope` (Edit AI Scope),
    /// in `settings.toml`. Never exposes anything: if the context no longer points at
    /// `server`, or the file no longer exposes it there (hidden meanwhile), nothing is written
    /// and an error is reported.
    SetAiScope {
        cluster: ClusterId,
        server: String,
        /// The scope the user started editing: if it changed on disk since, nothing is
        /// written.
        from: crate::settings::AiScope,
        scope: crate::settings::AiScope,
    },
    /// Trust a cluster at `server` (`Some`), or stop trusting it (`None`), in `settings.toml`
    /// (ADR-0015). As with [`Command::SetAiExposed`], `server` is the URL the user confirmed; if
    /// the context no longer points there, nothing is written and an error is reported.
    ///
    /// `request` numbers the request among the context's Trust and Stop Trusting requests;
    /// [`CoreHandle::trust`] and [`CoreHandle::untrust`] take it from the session's state, so
    /// use those rather than sending this yourself.
    SetTrusted {
        cluster: ClusterId,
        server: Option<String>,
        request: u64,
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
    /// Set the MiB each log session's lines may take and write it to `settings.toml`. Sessions
    /// opened afterwards use it. The change arrives back as `Event::SettingsChanged`.
    SetLogBufferMib(u32),
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
        subject: LogSubject,
        options: LogOptions,
        snapshots: watch::Sender<Arc<LogSnapshot>>,
        paused: watch::Receiver<bool>,
        /// Registered when the session was opened, so whether it is shown counts from the
        /// start; the session keeps it while it runs.
        budget: crate::logs::Budgeted,
    },
    CloseLogs {
        id: u64,
    },
    /// The cluster's client, if it is connected. For sessions, which run on their own tasks.
    Client {
        cluster: ClusterId,
        reply: oneshot::Sender<Option<kube::Client>>,
    },
    /// The cluster's client for an action that changes the cluster or runs code in it, if the
    /// cluster is connected and not protected (ADR-0015). Every such action asks through this.
    /// Answered with the context's pin, read in the same step as the client, which a session
    /// started with it is pinned to.
    WriteClient {
        cluster: ClusterId,
        reply: oneshot::Sender<Result<(kube::Client, ContextPin), WriteRefusal>>,
    },
    /// Why a session opened on `cluster` at `pin` must end (the context was re-pointed or
    /// removed since), or `None`. Never contacts the server.
    CheckContext {
        cluster: ClusterId,
        pin: ContextPin,
        reply: oneshot::Sender<Option<String>>,
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
    /// Why it failed, as safe to show ([`crate::cluster::ClusterError::summary`]).
    Failed(crate::SafeErrorText),
}

/// A cluster as the sidebar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterSummary {
    pub info: ContextInfo,
    pub state: ConnectionState,
    /// The core's slot for the context: a new one (re-pointed or otherwise changed, back after
    /// being removed) has a higher generation. What came from an older one (its discovery, a
    /// session opened on it) belongs to a cluster that is gone, whatever lists a front end skipped
    /// (#255).
    pub generation: u64,
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
        /// The generation of the slot it was discovered for ([`ClusterSummary::generation`]):
        /// one for an older slot is stale.
        generation: u64,
        discovery: Option<Arc<ClusterDiscovery>>,
        error: Option<String>,
    },
    /// The agent activity log ([`crate::activity`]), at start and whenever `flint mcp` adds to
    /// it: every record kept, oldest first, for all clusters, with the keys its servers are keyed
    /// by (read with it, off the UI thread).
    AgentActivity(
        Arc<Vec<crate::activity::ActivityRecord>>,
        crate::activity::ServerKeys,
    ),
    /// A problem worth showing that is not tied to one cluster's connection.
    Error(String),
}

/// What [`CoreHandle::store_for_tests`] returns: keep it while the test reads the stores.
#[cfg(any(test, feature = "test-support"))]
pub struct StoredForTests(
    #[expect(dead_code, reason = "held, not read")] watch::Receiver<Arc<KindSnapshot>>,
);

/// How long [`CoreHandle::stored_snapshots`] waits for the watches nobody subscribes to, to
/// build their snapshots: a few of their 100 ms publish intervals.
pub const STORED_WAIT: Duration = Duration::from_millis(300);

/// Core work is mostly waiting on the network; blocking work (file reads, exec plugins) runs on
/// tokio's separate blocking pool.
const CORE_WORKER_THREADS: usize = 4;

/// The front end's handle to the core. Cheap to clone; the runtime stops when the last clone is
/// dropped.
#[derive(Clone)]
pub struct CoreHandle {
    commands: async_channel::Sender<Command>,
    requests: async_channel::Sender<Request>,
    events: crate::events::EventSource,
    hubs: HubRegistry,
    next_subscription: Arc<AtomicU64>,
    unlocks: SessionUnlocks,
    log_budget: Arc<crate::logs::LogBudget>,
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
        let (event_tx, event_rx) = crate::events::queue();
        let hubs = HubRegistry::default();
        let unlocks = SessionUnlocks::default();
        let manager = ClusterManager::new(config, event_tx, hubs.clone(), unlocks.clone());
        let log_budget = manager.log_budget();
        runtime.spawn(manager.run(cmd_rx, req_rx));
        Ok(Self {
            commands: cmd_tx,
            requests: req_tx,
            events: event_rx,
            hubs,
            next_subscription: Arc::new(AtomicU64::new(1)),
            unlocks,
            log_budget,
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

    /// The rows the watches of `cluster` hold in memory now, one snapshot per kind that has
    /// listed, for a search over every object (the resource switcher). Never reaches the API
    /// server and never starts a watch, in either watch mode: kinds not watched now are left
    /// out. Answers within about [`STORED_WAIT`]. Works on any executor; dropping the future
    /// stops the work.
    pub fn stored_snapshots(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Vec<Arc<KindSnapshot>>> + Send + 'static {
        let hubs = self.hubs.clone();
        let cluster = cluster.to_owned();
        let task = spawn_aborting(&self.runtime, async move {
            hubs.stored(&cluster, STORED_WAIT).await
        });
        async move { task.await.unwrap_or_default() }
    }

    /// Puts `snapshot` in its kind's store as a watch would, for front-end tests of what reads
    /// the stores without subscribing ([`CoreHandle::stored_snapshots`]). While the returned
    /// value is kept, the kind counts as subscribed, so reading it does not wait for a watch
    /// task to build a snapshot (there is none).
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_for_tests(&self, snapshot: KindSnapshot) -> StoredForTests {
        let hub = self.hubs.get_or_create(&snapshot.cluster, &snapshot.kind);
        hub.take_over(snapshot);
        StoredForTests(hub.subscribe())
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

    /// Opens a log session: streams the log of `subject` on `cluster` into a ring buffer of
    /// [`crate::Settings::log_buffer_limit`] lines and [`crate::Settings::log_buffer_bytes`]
    /// bytes, reconnecting when the stream drops. The
    /// session keeps running when another cluster becomes active, until the returned
    /// [`LogSession`] is dropped. Snapshots arrive at most about every 100 ms. Never blocks.
    ///
    /// `subject` is a pod's container ([`LogTarget`]) or a workload's pods
    /// ([`crate::WorkloadTarget`]): every container of every matching pod, merged into one
    /// buffer with each line tagged by pod and container, following pods as they come and go
    /// (see [`crate::PodSetStatus`] for the streams' states).
    pub fn open_logs(
        &self,
        cluster: &str,
        subject: impl Into<LogSubject>,
        options: LogOptions,
    ) -> LogSession {
        let subject = subject.into();
        let (snapshots, mut rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        rx.mark_changed();
        let (paused, paused_rx) = watch::channel(false);
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.try_send(Request::OpenLogs {
            id,
            cluster: cluster.to_owned(),
            subject: subject.clone(),
            options: options.clone(),
            snapshots,
            paused: paused_rx,
            budget: crate::logs::Budgeted::new(self.log_budget.clone(), id),
        });
        LogSession {
            id,
            cluster: cluster.to_owned(),
            subject,
            options,
            rx,
            paused,
            requests: self.requests.clone(),
            budget: self.log_budget.clone(),
        }
    }

    /// The memory budget all log sessions share (`log_total_mib`), for tests and diagnostics.
    pub fn log_budget(&self) -> &crate::logs::LogBudget {
        &self.log_budget
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

    /// Opens an exec session (a shell in a container, by default) on a connected cluster. The
    /// session runs on its own task: it keeps running when the active cluster changes, and ends
    /// when it is closed or dropped. When the cluster is not connected, it fails at once. Never
    /// blocks.
    pub fn exec(&self, request: ExecRequest) -> ExecSession {
        // Someone opened or restarted a shell: as a retry, it may run the cluster's resting exec
        // auth plugin again (the cached client may hold it back after a timed-out refresh).
        crate::auth_plugin::retry_now(&request.cluster);
        let (session, channels) = ExecSession::new(request.clone());
        let client = self.write_client(&request.cluster);
        let requests = self.requests.clone();
        let cluster = request.cluster.clone();
        self.runtime.spawn(async move {
            let client = client.await.map(|(client, pin)| {
                let check = context_check(requests, cluster, pin);
                (client, ContextWatch::new(check, RECHECK_EVERY))
            });
            crate::exec::start(client, request, channels).await;
        });
        session
    }

    /// The client for an action that changes `cluster` or runs code in it, with the context's
    /// pin: refused while the cluster is protected (ADR-0015) or not connected.
    /// Never blocks.
    fn write_client(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Result<(kube::Client, ContextPin), WriteRefusal>> + Send + 'static
    {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.try_send(Request::WriteClient {
            cluster: cluster.to_owned(),
            reply,
        });
        async move {
            if sent.is_err() {
                return Err(WriteRefusal::Stopped);
            }
            rx.await.unwrap_or(Err(WriteRefusal::Stopped))
        }
    }

    /// Unlocks `cluster` at `server` (the URL the user confirmed) until Flint quits or
    /// [`CoreHandle::lock`] (ADR-0015). It takes effect at once. If the context now points
    /// elsewhere, the unlock does not cover it: the pin is to `server`.
    pub fn unlock(&self, cluster: &str, server: &str) {
        self.unlocks.unlock(cluster, server);
    }

    /// Locks `cluster` again (Lock Again). Sessions already running keep running; nothing new
    /// that changes the cluster starts. Returns whether it was unlocked.
    pub fn lock(&self, cluster: &str) -> bool {
        self.unlocks.lock(cluster)
    }

    /// Trusts `cluster` at `server` (the URL the user confirmed), written to `settings.toml`
    /// through [`Command::SetTrusted`]. A revocation of its trust earlier in the session is
    /// lifted once the core has checked the server and saved the pin, not before.
    pub fn trust(&self, cluster: &str, server: &str) {
        let request = self.unlocks.request_trust(cluster);
        self.send(Command::SetTrusted {
            cluster: cluster.to_owned(),
            server: Some(server.to_owned()),
            request,
        });
    }

    /// Stops trusting `cluster` and locks it: from this moment nothing new that changes the
    /// cluster starts, before `settings.toml` is written (commands and the core's gate take
    /// separate queues), and no Trust written before then lifts it. Shells already open keep
    /// running.
    pub fn untrust(&self, cluster: &str) {
        let request = self.unlocks.revoke_trust(cluster);
        self.send(Command::SetTrusted {
            cluster: cluster.to_owned(),
            server: None,
            request,
        });
    }

    /// The session's unlocks, for showing each cluster's [`crate::Protection`].
    pub fn unlocks(&self) -> &SessionUnlocks {
        &self.unlocks
    }

    /// Lists the pods of a workload (or a label selector) in `namespace` on a connected
    /// cluster, once and sorted by name, so a front end can open a shell in one of them
    /// ([`WorkloadPod::first_ready`]). [`Freshness::Cached`] reads the API server's cache, for
    /// showing; [`Freshness::Current`] is a quorum read, for acting. Never blocks; the future
    /// works on any executor and fails if the cluster is not connected, the workload is missing,
    /// the selector is empty or the list is refused. Dropping the future stops the request.
    pub fn workload_pods(
        &self,
        cluster: &str,
        namespace: &str,
        pods: PodSet,
        freshness: Freshness,
    ) -> impl Future<Output = Result<Vec<WorkloadPod>, String>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.try_send(Request::Client {
            cluster: cluster.to_owned(),
            reply,
        });
        let (cluster, namespace) = (cluster.to_owned(), namespace.to_owned());
        let listed = spawn_aborting(&self.runtime, async move {
            match sent {
                Ok(()) => match rx.await {
                    Ok(Some(client)) => {
                        crate::logs::list_workload_pods(client, &namespace, &pods, freshness)
                            .await
                            .map_err(|e| e.message)
                    }
                    Ok(None) => Err(format!("\"{cluster}\" is not connected")),
                    Err(_) => Err("the core stopped before answering".to_owned()),
                },
                Err(_) => Err("the core is not running".to_owned()),
            }
        });
        async move {
            listed
                .await
                .unwrap_or_else(|| Err("the core stopped before answering".to_owned()))
        }
    }

    /// Sends a command. Never blocks. Commands sent after the core stopped are dropped.
    pub fn send(&self, command: Command) {
        if self.commands.try_send(command).is_err() {
            tracing::warn!("core is not running; command dropped");
        }
    }

    /// The event stream ([`crate::events`]): latest state per kind and cluster, and recent
    /// errors. Each event goes to one reader. The handle does not keep the stream open: once
    /// every reader is dropped, the core stops queueing events.
    pub fn events(&self) -> crate::events::Events {
        self.events.events()
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
    subject: LogSubject,
    options: LogOptions,
    rx: watch::Receiver<Arc<LogSnapshot>>,
    paused: watch::Sender<bool>,
    requests: async_channel::Sender<Request>,
    budget: Arc<crate::logs::LogBudget>,
}

impl LogSession {
    /// The cluster the session reads from, whichever cluster is active.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    /// What the session reads.
    pub fn subject(&self) -> &LogSubject {
        &self.subject
    }

    /// The pod container a pod session reads; `None` for a workload session.
    pub fn target(&self) -> Option<&LogTarget> {
        match &self.subject {
            LogSubject::Pod(target) => Some(target),
            LogSubject::Workload(_) => None,
        }
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

    /// A second reader of this session's snapshots, for a task that waits on them while the
    /// owner keeps the session (to pause it, and to stop it by dropping it). It starts with the
    /// newest snapshot unseen, and it does not keep the session running: once the session is
    /// dropped, [`LogUpdates::changed`] returns `false`.
    pub fn updates(&self) -> LogUpdates {
        let mut rx = self.rx.clone();
        rx.mark_changed();
        LogUpdates { rx }
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

    /// Whether this session's tab is the one shown. The shared log memory budget trims the
    /// sessions viewed least recently first and never the shown one (see
    /// [`crate::logs::LogBudget`]).
    pub fn set_shown(&self, shown: bool) {
        self.budget.set_shown(self.id, shown);
    }

    /// Whether [`LogSession::set_shown`] last said this tab is shown (once the core has started
    /// the session).
    pub fn is_shown(&self) -> bool {
        self.budget.is_shown(self.id)
    }
}

/// Reads a [`LogSession`]'s snapshots from elsewhere; see [`LogSession::updates`].
#[derive(Debug)]
pub struct LogUpdates {
    rx: watch::Receiver<Arc<LogSnapshot>>,
}

impl LogUpdates {
    /// Waits until a snapshot newer than the last one read with [`LogUpdates::latest`] is
    /// published. Returns `false` once the session is closed or the core has stopped. Works on
    /// any executor.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    /// The newest snapshot, marking it as seen.
    pub fn latest(&mut self) -> Arc<LogSnapshot> {
        self.rx.borrow_and_update().clone()
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
            .field("subject", &self.subject)
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

/// Asks the cluster manager whether `cluster` changed since `pin`, for a session pinned there.
/// While the core is busy or stopped, the session goes on: it has its own client.
fn context_check(
    requests: async_channel::Sender<Request>,
    cluster: ClusterId,
    pin: ContextPin,
) -> ContextCheck {
    Arc::new(move || {
        let (reply, rx) = oneshot::channel();
        let sent = requests.try_send(Request::CheckContext {
            cluster: cluster.clone(),
            pin,
            reply,
        });
        Box::pin(async move {
            if sent.is_err() {
                return None;
            }
            rx.await.ok().flatten()
        })
    })
}

/// Runs `task` on `runtime`. The returned future yields its output (`None` if the runtime
/// dropped it), and dropping that future aborts the task, so an answer nobody waits for any
/// more is not worked on.
fn spawn_aborting<T: Send + 'static>(
    runtime: &tokio::runtime::Handle,
    task: impl Future<Output = T> + Send + 'static,
) -> impl Future<Output = Option<T>> + Send + 'static {
    let mut task = crate::exec::AbortOnDrop(runtime.spawn(task));
    async move { (&mut task.0).await.ok() }
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
            watch_timing: Default::default(),
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

    // covers UP-31: the stores are read through the hubs alone (`HubRegistry::stored`), which
    // hold no channel to the manager, so no watch can start.
    #[tokio::test]
    async fn stored_snapshots_read_without_subscribing() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        assert!(core.stored_snapshots("nowhere").await.is_empty());
        let _kept = core.store_for_tests(KindSnapshot {
            cluster: "nowhere".into(),
            kind: KindId::pods(),
            state: SyncState::Synced,
            ..Default::default()
        });
        let stored = core.stored_snapshots("nowhere").await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].kind, KindId::pods());
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

    // covers UP-44: opening or restarting a shell is a retry: it ends the rest of the cluster's
    // exec auth plugin, as Retry does.
    #[tokio::test]
    async fn a_shell_ends_the_rest_of_its_clusters_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let before = crate::auth_plugin::retry_epoch("shell-rest");
        let _session = core.exec(crate::exec::ExecRequest::shell("shell-rest", "ns", "p"));
        assert_eq!(crate::auth_plugin::retry_epoch("shell-rest"), before + 1);
    }

    // covers UP-47: a pod list nobody waits for any more stops: dropping the future aborts
    // the task doing the request.
    #[tokio::test]
    async fn dropping_a_spawned_request_aborts_it() {
        struct Flag(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Flag {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let (dropped, aborted) = tokio::sync::oneshot::channel();
        let flag = Flag(Some(dropped));
        let waiting = spawn_aborting(&tokio::runtime::Handle::current(), async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        // Not even polled once.
        drop(waiting);
        tokio::time::timeout(Duration::from_secs(5), aborted)
            .await
            .expect("the task was aborted, dropping what it held")
            .unwrap();

        let answered = spawn_aborting(&tokio::runtime::Handle::current(), async { 7 });
        assert_eq!(answered.await, Some(7));
    }

    #[tokio::test]
    async fn workload_pods_on_an_unknown_cluster_answer_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let err = core
            .workload_pods(
                "nowhere",
                "ns",
                PodSet::Selector("app=web".into()),
                Freshness::Current,
            )
            .await
            .unwrap_err();
        assert!(err.contains("not connected"), "{err}");
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
        assert_eq!(session.target(), Some(&target));
        assert_eq!(session.subject(), &LogSubject::Pod(target.clone()));
        assert!(session.options().follow);
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(session.changed().await, "core running");
                let snapshot = session.latest();
                if let crate::logs::LogState::Ended { error, .. } = &snapshot.state {
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

    // covers UP-42: a view waits on a second reader while it keeps (and later drops) the session.
    #[tokio::test]
    async fn a_second_reader_follows_the_session_until_it_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let target = LogTarget {
            namespace: "ns".into(),
            pod: "p".into(),
            container: None,
        };
        let session = core.open_logs("nowhere", target, LogOptions::default());
        let mut updates = session.updates();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(updates.changed().await, "session open");
                if matches!(updates.latest().state, crate::logs::LogState::Ended { .. }) {
                    break;
                }
            }
        })
        .await
        .expect("the reader sees the session end");
        session.set_paused(true);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !updates.latest().paused {
                assert!(updates.changed().await);
            }
        })
        .await
        .expect("the reader sees the pause");
        drop(session);
        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            while updates.changed().await {}
        })
        .await;
        assert!(
            closed.is_ok(),
            "the reader ends once the session is dropped"
        );
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

    // covers UP-41
    #[tokio::test]
    async fn exec_on_a_cluster_that_is_not_connected_fails_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let mut session = core.exec(ExecRequest::shell("nowhere", "ns", "pod"));
        let state = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = session.latest().state.clone();
                if state.is_finished() {
                    return state;
                }
                assert!(session.changed().await);
            }
        })
        .await
        .expect("fails at once");
        assert_eq!(
            state,
            crate::exec::ExecState::Failed(crate::exec::ExecFailure::new(
                crate::exec::ExecFailureKind::Unreachable,
                "\"nowhere\" is not connected"
            ))
        );
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
