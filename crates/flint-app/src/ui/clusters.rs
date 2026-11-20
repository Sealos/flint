//! The app's view of the cluster list, updated from core events.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use flint_core::settings::{AiExposureState, AiScope, TrustState};
use flint_core::{
    Appearance, ClusterDiscovery, ClusterId, ClusterSummary, Command, ConnectionState, CoreHandle,
    Event, Protection, Settings, WatchMode,
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
                replace_clusters(&mut self.discovery, &mut self.clusters, &snapshot.clusters);
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

    /// Exposes `cluster` at `server`, the URL the user confirmed, in full.
    pub fn expose_to_ai(&mut self, cluster: ClusterId, server: String) {
        self.send(Command::SetAiExposed {
            cluster,
            server: Some(server),
            scope: AiScope::default(),
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
            scope: AiScope::default(),
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

    /// Whether Flint may change `cluster` now (ADR-0015). The core keeps the same check for
    /// every action that changes a cluster; this one is for showing it.
    pub fn protection(&self, cluster: &ClusterSummary) -> Protection {
        flint_core::protection::protection(
            &self.settings,
            self.core.unlocks(),
            &cluster.info.name,
            cluster.info.server.as_deref(),
            cluster.info.proxy_url.as_deref(),
        )
    }

    /// [`ClustersModel::protection`] of the context named `cluster`; protected if there is none.
    pub fn protection_of(&self, cluster: &str) -> Protection {
        self.clusters
            .iter()
            .find(|c| c.info.name == cluster)
            .map_or(Protection::Protected, |c| self.protection(c))
    }

    pub fn trust(&self, cluster: &ClusterSummary) -> TrustState {
        self.settings
            .trust(&cluster.info.name, cluster.info.server.as_deref())
    }

    /// Unlocks `cluster` at `server`, the URL the user confirmed, until Flint quits or Lock
    /// Again. It takes effect at once, in the core too.
    pub fn unlock(&mut self, cluster: &str, server: &str, cx: &mut Context<Self>) {
        self.core.unlock(cluster, server);
        cx.notify();
    }

    /// Locks `cluster` again (Lock Again).
    pub fn lock(&mut self, cluster: &str, cx: &mut Context<Self>) {
        self.core.lock(cluster);
        cx.notify();
    }

    /// Trusts `cluster` at `server`, the URL the user confirmed. Written to `settings.toml`;
    /// the change comes back as a settings event.
    pub fn trust_cluster(&mut self, cluster: ClusterId, server: String) {
        self.last_error = None;
        self.core.trust(&cluster, &server);
    }

    /// Stops trusting `cluster` (or removes a stale trust), and locks it: the user wants it
    /// protected, even if it was also unlocked for this session. It is protected from this
    /// moment, in the core's gate too, before `settings.toml` is written.
    pub fn untrust_cluster(&mut self, cluster: ClusterId, cx: &mut Context<Self>) {
        self.last_error = None;
        self.core.untrust(&cluster);
        cx.notify();
    }
}

/// A cluster's discovery, as the last `DiscoveryChanged` event left it.
#[derive(Debug, Clone, Default)]
pub struct Discovered {
    pub discovery: Option<Arc<ClusterDiscovery>>,
    pub error: Option<String>,
}

/// Drops the discovery of contexts that left kubeconfig (`before` -> `now`), or that now point at
/// another API server under the same name: the core dropped their slots (a re-pointed context
/// starts over and is discovered again), and the model would otherwise keep every such context's
/// kinds for as long as the app runs, or show the old server's kinds for the new one. (Other
/// changes to a context, such as its user, start it over in the core too; its kinds come from the
/// same server, so they are kept until it is discovered again.)
fn forget_removed(
    discovery: &mut HashMap<ClusterId, Discovered>,
    before: &[ClusterSummary],
    now: &[ClusterSummary],
) {
    let server = |clusters: &[ClusterSummary], id: &str| {
        clusters
            .iter()
            .find(|c| c.info.name == id)
            .map(|c| c.info.server.clone())
    };
    discovery.retain(|id, _| match (server(before, id), server(now, id)) {
        (_, None) => false,
        (Some(was), Some(is)) => was == is,
        // Discovered before the cluster list arrived: keep it.
        (None, Some(_)) => true,
    });
}

/// Replaces the cluster list with `now`, first forgetting the discovery its changes make stale
/// ([`forget_removed`]). One step, so the old list is always the one compared against.
fn replace_clusters(
    discovery: &mut HashMap<ClusterId, Discovered>,
    clusters: &mut Vec<ClusterSummary>,
    now: &[ClusterSummary],
) {
    forget_removed(discovery, clusters, now);
    *clusters = now.to_vec();
}

/// The cluster `FLINT_CLUSTER` selects: the context with exactly that name, or none.
pub fn startup_cluster(requested: &str, clusters: &[ClusterSummary]) -> Option<ClusterId> {
    clusters
        .iter()
        .find(|c| c.info.name == requested)
        .map(|c| c.info.name.clone())
}

/// What a cluster's sidebar dot shows: its connection, and whether Flint is watching it. Only
/// the active cluster is watched (UP-24): another one keeps its client and discovery but not
/// its watches, so it is idle, not live, even though it is still connected. When warm clusters
/// keep watching in the background (ADR-0012), this should come from the core's watch state
/// instead of `active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    NotConnected,
    Connecting,
    /// Connected and watched.
    Live,
    /// Connected, but its watches are stopped (cold).
    Idle,
    Failed,
}

impl Presence {
    pub fn of(state: &ConnectionState, active: bool) -> Self {
        match state {
            ConnectionState::Disconnected => Presence::NotConnected,
            ConnectionState::Connecting => Presence::Connecting,
            ConnectionState::Connected(_) if active => Presence::Live,
            ConnectionState::Connected(_) => Presence::Idle,
            ConnectionState::Failed(_) => Presence::Failed,
        }
    }

    /// Its name in the automation state.
    #[cfg(any(test, feature = "automation"))]
    pub fn name(self) -> &'static str {
        match self {
            Presence::NotConnected => "not_connected",
            Presence::Connecting => "connecting",
            Presence::Live => "live",
            Presence::Idle => "idle",
            Presence::Failed => "failed",
        }
    }
}

/// A connection state's name in the automation state: `disconnected`, `connecting`,
/// `connected` or `failed`. The e2e tests read these words.
#[cfg(any(test, feature = "automation"))]
pub fn state_name(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected(_) => "connected",
        ConnectionState::Failed(_) => "failed",
    }
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
    use super::{
        Discovered, Presence, forget_removed, replace_clusters, startup_cluster, state_name,
    };
    use flint_core::kubeconfig::ContextInfo;
    use flint_core::{ClusterSummary, ConnectionState, ServerVersion};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn cluster(name: &str) -> ClusterSummary {
        ClusterSummary {
            info: ContextInfo {
                name: name.into(),
                cluster: None,
                user: None,
                namespace: None,
                server: None,
                proxy_url: None,
                source: PathBuf::from("/k"),
            },
            state: ConnectionState::Disconnected,
        }
    }

    #[test]
    fn removed_and_re_pointed_contexts_lose_their_discovery() {
        let mut discovery: HashMap<String, Discovered> = ["kept", "removed", "re-pointed"]
            .into_iter()
            .map(|name| (name.to_owned(), Discovered::default()))
            .collect();
        let at = |name: &str, server: &str| {
            let mut c = cluster(name);
            c.info.server = Some(server.into());
            c
        };
        let before = [
            at("kept", "https://a"),
            at("removed", "https://b"),
            at("re-pointed", "https://c"),
        ];
        let now = [
            at("kept", "https://a"),
            at("re-pointed", "https://elsewhere"),
            at("new", "https://d"),
        ];
        forget_removed(&mut discovery, &before, &now);
        let mut left: Vec<&str> = discovery.keys().map(String::as_str).collect();
        left.sort();
        assert_eq!(left, ["kept"]);
    }

    // Reloads one after another, as `ClustersChanged` applies them: each compares against the
    // list the previous one left.
    #[test]
    fn each_reload_forgets_against_the_list_before_it() {
        let at = |name: &str, server: &str| {
            let mut c = cluster(name);
            c.info.server = Some(server.into());
            c
        };
        let mut discovery: HashMap<String, Discovered> = HashMap::new();
        let mut clusters = Vec::new();
        replace_clusters(&mut discovery, &mut clusters, &[at("a", "https://one")]);
        discovery.insert("a".into(), Discovered::default());
        replace_clusters(&mut discovery, &mut clusters, &[at("a", "https://one")]);
        assert!(discovery.contains_key("a"), "an unchanged reload keeps it");
        replace_clusters(&mut discovery, &mut clusters, &[at("a", "https://two")]);
        assert!(discovery.is_empty(), "a re-pointed context is forgotten");
        assert_eq!(clusters[0].info.server.as_deref(), Some("https://two"));
        discovery.insert("a".into(), Discovered::default());
        replace_clusters(&mut discovery, &mut clusters, &[]);
        assert!(discovery.is_empty(), "a removed context is forgotten");
        assert!(clusters.is_empty());
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

    // covers UP-24: a cluster switched away from stops its watches, so its dot must not say live.
    #[test]
    fn only_the_active_connected_cluster_is_live() {
        let connected = ConnectionState::Connected(ServerVersion {
            git_version: "v1.31.0".into(),
            platform: "linux/arm64".into(),
        });
        assert_eq!(Presence::of(&connected, true), Presence::Live);
        assert_eq!(Presence::of(&connected, false), Presence::Idle);
        let failed = ConnectionState::Failed(flint_core::SafeErrorText::reduced("refused"));
        assert_eq!(Presence::of(&failed, false), Presence::Failed);
        assert_eq!(
            Presence::of(&ConnectionState::Connecting, false),
            Presence::Connecting
        );
        assert_eq!(
            Presence::of(&ConnectionState::Disconnected, true),
            Presence::NotConnected
        );
        assert_eq!(Presence::Idle.name(), "idle");
    }

    // The automation state's words for a connection; the e2e tests read them.
    #[test]
    fn connection_states_have_fixed_names() {
        let connected = ConnectionState::Connected(ServerVersion {
            git_version: "v1.31.0".into(),
            platform: "linux/arm64".into(),
        });
        let failed = ConnectionState::Failed(flint_core::SafeErrorText::reduced("refused"));
        assert_eq!(
            [
                ConnectionState::Disconnected,
                ConnectionState::Connecting,
                connected,
                failed,
            ]
            .each_ref()
            .map(state_name),
            ["disconnected", "connecting", "connected", "failed"]
        );
    }
}

#[cfg(test)]
#[path = "clusters_tests.rs"]
mod view_tests;
