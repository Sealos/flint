//! The app's view of the cluster list, updated from core events.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use flint_core::settings::AiExposureState;
use flint_core::{
    Appearance, ClusterDiscovery, ClusterId, ClusterSummary, Command, ConnectionState, CoreHandle,
    Event, Settings, WatchMode,
};
use gpui_kit::*;

pub struct ClustersModel {
    core: CoreHandle,
    pub clusters: Vec<ClusterSummary>,
    pub kubeconfig_errors: Vec<(PathBuf, String)>,
    /// Every kubeconfig path the core looked at, and whether it existed, for the empty state.
    pub kubeconfig_paths: Vec<(PathBuf, bool)>,
    /// `KUBECONFIG` as this process saw it, for the empty state.
    pub kubeconfig_env: Option<OsString>,
    /// False until the core has loaded kubeconfig once.
    pub loaded: bool,
    pub active: Option<ClusterId>,
    pub settings: Arc<Settings>,
    /// Why `settings.toml` could not be read. While set, nothing is exposed to AI agents.
    pub settings_error: Option<String>,
    /// The last error not tied to a cluster connection (for example a settings write). Cleared
    /// when the user issues the next command; if that one fails, its error replaces it.
    pub last_error: Option<String>,
    /// The latest discovery result per cluster, and why the last discovery failed, if it did.
    pub discovery: HashMap<ClusterId, Discovered>,
    /// The cluster to activate once kubeconfig is loaded (`FLINT_CLUSTER`). Taken on first load.
    startup_cluster: Option<String>,
    _events: Task<()>,
}

impl ClustersModel {
    /// `settings` are the ones loaded at startup, until the core reports its own.
    pub fn new(
        core: CoreHandle,
        settings: Settings,
        startup_cluster: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let events = core.events();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this.update(cx, |model, cx| model.apply(event, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            core,
            clusters: Vec::new(),
            kubeconfig_errors: Vec::new(),
            kubeconfig_paths: Vec::new(),
            kubeconfig_env: std::env::var_os("KUBECONFIG"),
            loaded: false,
            active: None,
            settings: Arc::new(settings),
            settings_error: None,
            last_error: None,
            discovery: HashMap::new(),
            startup_cluster,
            _events: task,
        }
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::ClustersChanged(snapshot) => {
                self.clusters = snapshot.clusters.clone();
                self.kubeconfig_errors = snapshot.errors.clone();
                self.kubeconfig_paths = snapshot.paths.clone();
                self.loaded = true;
                if let Some(requested) = self.startup_cluster.take() {
                    match startup_cluster(&requested, &self.clusters) {
                        Some(id) => {
                            tracing::info!(cluster = id, "FLINT_CLUSTER: activating");
                            self.activate(id);
                        }
                        None => tracing::warn!(
                            cluster = requested,
                            "FLINT_CLUSTER names a context that is not in the loaded kubeconfig; \
                             starting with nothing selected"
                        ),
                    }
                }
            }
            Event::ActiveChanged(active) => self.active = active,
            Event::ConnectionChanged { cluster, state } => {
                if let Some(c) = self.clusters.iter_mut().find(|c| c.info.name == cluster) {
                    c.state = state;
                }
            }
            Event::SettingsChanged { settings, error } => {
                self.settings = settings;
                self.settings_error = error;
            }
            Event::DiscoveryChanged {
                cluster,
                discovery,
                error,
            } => {
                self.discovery
                    .insert(cluster, Discovered { discovery, error });
            }
            Event::Error(message) => {
                tracing::warn!("{message}");
                self.last_error = Some(message);
            }
        }
        cx.notify();
    }

    pub fn activate(&mut self, cluster: ClusterId) {
        self.send(Command::Activate(cluster));
    }

    /// Sends a command and clears the previous command's error. The core reports a failure as an
    /// `Event::Error`, which arrives after this (both channels are FIFO).
    fn send(&mut self, command: Command) {
        self.last_error = None;
        self.core.send(command);
    }

    /// Exposes `cluster` at `server`, the URL the user confirmed.
    pub fn expose_to_ai(&mut self, cluster: ClusterId, server: String) {
        self.send(Command::SetAiExposed {
            cluster,
            server: Some(server),
        });
    }

    /// Sets the global watch mode (`cluster: None`), or a cluster's override (`mode: None`
    /// removes it). Written to `settings.toml`; the change comes back as a settings event.
    pub fn set_watch_mode(&mut self, cluster: Option<ClusterId>, mode: Option<WatchMode>) {
        self.send(Command::SetWatchMode { cluster, mode });
    }

    /// Sets the appearance. Written to `settings.toml`; the change comes back as a settings
    /// event, and the window follows it.
    pub fn set_appearance(&mut self, appearance: Appearance) {
        // Not `send`: changing the theme is not an answer to the last error (a removed cluster,
        // a failed write), so its notice stays.
        self.core.send(Command::SetAppearance(appearance));
    }

    /// Tells the core which namespaces the user filters `cluster` on, for kinds the user may not
    /// list in all namespaces.
    pub fn set_namespace_filter(&mut self, cluster: ClusterId, namespaces: Vec<String>) {
        self.core.send(Command::SetNamespaceFilter {
            cluster,
            namespaces,
        });
    }

    pub fn core(&self) -> &CoreHandle {
        &self.core
    }

    pub fn hide_from_ai(&mut self, cluster: ClusterId) {
        self.send(Command::SetAiExposed {
            cluster,
            server: None,
        });
    }

    pub fn active_cluster(&self) -> Option<&ClusterSummary> {
        let active = self.active.as_deref()?;
        self.clusters.iter().find(|c| c.info.name == active)
    }

    pub fn ai_exposure(&self, cluster: &ClusterSummary) -> AiExposureState {
        self.settings
            .ai_exposure(&cluster.info.name, cluster.info.server.as_deref())
    }
}

/// A cluster's discovery, as the last `DiscoveryChanged` event left it.
#[derive(Debug, Clone, Default)]
pub struct Discovered {
    pub discovery: Option<Arc<ClusterDiscovery>>,
    pub error: Option<String>,
}

/// The cluster `FLINT_CLUSTER` selects: the context with exactly that name, or none.
pub fn startup_cluster(requested: &str, clusters: &[ClusterSummary]) -> Option<ClusterId> {
    clusters
        .iter()
        .find(|c| c.info.name == requested)
        .map(|c| c.info.name.clone())
}

/// A short label for a connection state.
pub fn state_label(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Disconnected => "Not connected",
        ConnectionState::Connecting => "Connecting",
        ConnectionState::Connected(_) => "Connected",
        ConnectionState::Failed(_) => "Connection failed",
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings in `gpui_kit::test`, which shadows the std `#[test]`.
    use super::startup_cluster;
    use flint_core::kubeconfig::ContextInfo;
    use flint_core::{ClusterSummary, ConnectionState};
    use std::path::PathBuf;

    fn cluster(name: &str) -> ClusterSummary {
        ClusterSummary {
            info: ContextInfo {
                name: name.into(),
                cluster: None,
                user: None,
                namespace: None,
                server: None,
                source: PathBuf::from("/k"),
            },
            state: ConnectionState::Disconnected,
        }
    }

    // covers UP-03, UP-91
    #[test]
    fn flint_cluster_selects_only_the_exact_name() {
        let clusters = [cluster("kind-flint-dev"), cluster("prod")];
        assert_eq!(
            startup_cluster("kind-flint-dev", &clusters).as_deref(),
            Some("kind-flint-dev")
        );
        assert_eq!(startup_cluster("kind-flint", &clusters), None);
        assert_eq!(startup_cluster("KIND-FLINT-DEV", &clusters), None);
        assert_eq!(startup_cluster("", &clusters), None);
        assert_eq!(startup_cluster("prod", &[]), None);
    }
}
