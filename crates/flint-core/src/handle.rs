//! The bridge between a UI (or any other front end) and the core (ADR-0002).
//!
//! The core runs on its own multi-thread tokio runtime. Front ends send [`Command`]s and receive
//! [`Event`]s over `async_channel` channels, which work with any executor, so a GPUI task can
//! await them directly. Nothing here blocks the caller.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crate::cluster::{ClusterId, ServerVersion};
use crate::kubeconfig::ContextInfo;
use crate::manager::ClusterManager;
use crate::settings::Settings;

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
    /// Kubeconfig files that could not be read or parsed.
    pub errors: Vec<(PathBuf, String)>,
    /// Every kubeconfig path considered.
    pub paths: Vec<PathBuf>,
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
    events: async_channel::Receiver<Event>,
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
        let (event_tx, event_rx) = async_channel::unbounded();
        runtime.spawn(ClusterManager::new(config, event_tx).run(cmd_rx));
        Ok(Self {
            commands: cmd_tx,
            events: event_rx,
            _runtime: Arc::new(RuntimeGuard(Some(runtime))),
        })
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
