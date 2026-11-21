//! The app's view of the cluster list, updated from core events.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use flint_core::activity::ActivityRecord;
use flint_core::settings::{AiExposureState, AiScope, TrustState};
use flint_core::{
    Appearance, ClusterDiscovery, ClusterId, ClusterSummary, Command, ConnectionState, CoreHandle,
    Event, KindId, Protection, Settings, SyncState, WatchMode,
};
use gpui_kit::component::Theme;
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
    /// Clusters this model asked to stop trusting, by request, until the core reports that
    /// request done: the trust [`ClustersModel::settings`] holds no longer counts for what is
    /// shown.
    stopping_trust: HashMap<ClusterId, u64>,
    /// The newest Trust or Stop Trusting request the core has finished.
    pub(in crate::ui) trust_settled: u64,
    /// Why `settings.toml` could not be read. While set, nothing is exposed to AI agents.
    pub settings_error: Option<String>,
    /// The last error not tied to a cluster connection (for example a settings write). Cleared
    /// when the user issues the next command; if that one fails, its error replaces it.
    pub last_error: Option<String>,
    /// The latest discovery result per cluster, and why the last discovery failed, if it did:
    /// only for the cluster's current slot ([`ClusterSummary::generation`]).
    pub discovery: HashMap<ClusterId, Discovered>,
    /// Discovery of a slot newer than the cluster list the model has (the core's queue can put
    /// the list behind it, #255): used once the list catches up.
    ahead: HashMap<ClusterId, Discovered>,
    /// The agent activity log, every cluster's, oldest first (UP-16).
    pub agent_activity: Arc<Vec<ActivityRecord>>,
    /// The keys the log's servers are keyed by: none until the log is first read.
    pub activity_keys: flint_core::activity::ServerKeys,
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
            stopping_trust: HashMap::new(),
            trust_settled: 0,
            settings_error: None,
            last_error: None,
            discovery: HashMap::new(),
            ahead: HashMap::new(),
            agent_activity: Arc::default(),
            activity_keys: Default::default(),
            startup_cluster,
            _events: task,
        }
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::ClustersChanged(snapshot) => {
                replace_clusters(
                    &mut self.discovery,
                    &mut self.ahead,
                    &mut self.clusters,
                    &snapshot.clusters,
                );
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
            Event::SettingsChanged {
                settings,
                error,
                trust_settled,
            } => {
                self.settings = settings;
                self.settings_error = error;
                self.trust_settled = trust_settled;
                self.stopping_trust
                    .retain(|_, request| *request > trust_settled);
            }
            Event::DiscoveryChanged {
                cluster,
                generation,
                discovery,
                error,
            } => {
                let found = Discovered {
                    discovery,
                    error,
                    generation,
                };
                let current = self
                    .clusters
                    .iter()
                    .find(|c| c.info.name == cluster)
                    .map(|c| c.generation);
                match current {
                    // An older slot's: stale.
                    Some(current) if generation < current => {}
                    // A newer slot's than the list the model has: kept for when it comes.
                    Some(current) if generation > current => {
                        self.ahead.insert(cluster, found);
                    }
                    _ => {
                        self.discovery.insert(cluster, found);
                    }
                }
            }
            Event::AgentActivity(records, keys) => {
                self.agent_activity = records;
                self.activity_keys = keys;
            }
            Event::Error(message) => {
                tracing::warn!("{message}");
                self.last_error = Some(message);
            }
        }
        cx.notify();
    }

    /// What was discovered of `cluster`'s current slot: nothing for an older one.
    pub fn discovered(&self, cluster: &str) -> Option<&Discovered> {
        let generation = self
            .clusters
            .iter()
            .find(|c| c.info.name == cluster)
            .map(|c| c.generation);
        self.discovery
            .get(cluster)
            .filter(|d| generation.is_none_or(|g| g == d.generation))
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

    /// Exposes `cluster` at `server`, the URL the user confirmed, limited to `scope`
    /// (ADR-0013). For a cluster exposed already, this rewrites its entry with the new scope.
    pub fn expose_to_ai(&mut self, cluster: ClusterId, server: String, scope: AiScope) {
        self.send(Command::SetAiExposed {
            cluster,
            server: Some(server),
            scope,
        });
    }

    /// Changes what AI agents may read of `cluster`, exposed at `server`, from `from` (the scope
    /// the dialog opened with) to `scope`. Never exposes it: if it was hidden or re-pointed
    /// meanwhile, or its scope changed on disk, nothing is written.
    pub fn set_ai_scope(
        &mut self,
        cluster: ClusterId,
        server: String,
        from: AiScope,
        scope: AiScope,
    ) {
        self.send(Command::SetAiScope {
            cluster,
            server,
            from,
            scope,
        });
    }

    /// What AI agents may read of `cluster`, if it is exposed at the server it points at now.
    pub fn ai_scope(&self, cluster: &ClusterSummary) -> Option<AiScope> {
        self.settings
            .ai_scope(&cluster.info.name, cluster.info.server.as_deref())
    }

    /// Sets the global watch mode (`cluster: None`), or a cluster's override (`mode: None`
    /// removes it). Written to `settings.toml`; the change comes back as a settings event.
    pub fn set_watch_mode(&mut self, cluster: Option<ClusterId>, mode: Option<WatchMode>) {
        self.send(Command::SetWatchMode { cluster, mode });
    }

    /// What each cluster is called on screen.
    pub fn names(&self) -> flint_core::display::ClusterNames {
        flint_core::display::ClusterNames::new(
            self.clusters.iter().map(|c| c.info.name.as_str()),
            &self.settings.cluster_names,
        )
    }

    /// Sets the name shown for `cluster` (`None` removes it). Written to `settings.toml`; the
    /// change comes back as a settings event.
    pub fn set_cluster_name(&mut self, cluster: ClusterId, name: Option<String>) {
        self.send(Command::SetClusterName { cluster, name });
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
        let without_its_trust = self
            .stopping_trust
            .contains_key(&cluster.info.name)
            .then(|| {
                let mut settings = (*self.settings).clone();
                settings
                    .trusted_clusters
                    .retain(|pin| pin.context != cluster.info.name);
                settings
            });
        flint_core::protection::protection(
            without_its_trust.as_ref().unwrap_or(&self.settings),
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
        self.stopping_trust.remove(&cluster);
        self.core.trust(&cluster, &server);
    }

    /// Stops trusting `cluster` (or removes a stale trust), and locks it: the user wants it
    /// protected, even if it was also unlocked for this session. It is protected from this
    /// moment, in the core's gate too, before `settings.toml` is written.
    pub fn untrust_cluster(&mut self, cluster: ClusterId, cx: &mut Context<Self>) {
        self.last_error = None;
        let request = self.core.untrust(&cluster);
        self.stopping_trust.insert(cluster, request);
        cx.notify();
    }
}

/// A cluster's discovery, as the last `DiscoveryChanged` event left it.
#[derive(Debug, Clone, Default)]
pub struct Discovered {
    pub discovery: Option<Arc<ClusterDiscovery>>,
    pub error: Option<String>,
    /// The generation of the slot it is for ([`ClusterSummary::generation`]).
    pub generation: u64,
}

/// Keeps the discovery of each cluster's current slot only: a context that left kubeconfig, or
/// started over in the core (a new generation: re-pointed, changed, back after being removed),
/// loses what was discovered of its old slot. Generations, not a comparison with the list
/// before, so it holds however many lists the model skipped (#255). Discovery that came ahead of
/// its list is used once the list has its slot.
fn forget_stale(
    discovery: &mut HashMap<ClusterId, Discovered>,
    ahead: &mut HashMap<ClusterId, Discovered>,
    now: &[ClusterSummary],
) {
    let generation = |id: &str| now.iter().find(|c| c.info.name == id).map(|c| c.generation);
    discovery.retain(|id, d| generation(id) == Some(d.generation));
    ahead.retain(|id, d| generation(id).is_some_and(|g| d.generation >= g));
    let caught_up: Vec<ClusterId> = ahead
        .iter()
        .filter(|(id, d)| generation(id) == Some(d.generation))
        .map(|(id, _)| id.clone())
        .collect();
    for id in caught_up {
        if let Some(d) = ahead.remove(&id) {
            discovery.insert(id, d);
        }
    }
}

/// Replaces the cluster list with `now`, keeping only the discovery of its slots
/// ([`forget_stale`]).
fn replace_clusters(
    discovery: &mut HashMap<ClusterId, Discovered>,
    ahead: &mut HashMap<ClusterId, Discovered>,
    clusters: &mut Vec<ClusterSummary>,
    now: &[ClusterSummary],
) {
    forget_stale(discovery, ahead, now);
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
pub enum Presence<'a> {
    NotConnected,
    Connecting,
    /// Connected and watched.
    Live,
    Reconnecting {
        error: Option<&'a str>,
    },
    /// Connected, but its watches are stopped (cold).
    Idle,
    Failed,
}

/// The shape of a cluster's presence marker. Shape carries the state without colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotShape {
    Filled,
    Ring,
    /// A short horizontal bar.
    Dash,
}

impl<'a> Presence<'a> {
    pub fn of(
        state: &ConnectionState,
        active: bool,
        states: &'a HashMap<KindId, SyncState>,
    ) -> Self {
        match state {
            ConnectionState::Disconnected => Presence::NotConnected,
            ConnectionState::Connecting => Presence::Connecting,
            ConnectionState::Connected(_) if active => reconnecting(states).unwrap_or(Self::Live),
            ConnectionState::Connected(_) => Presence::Idle,
            ConnectionState::Failed(_) => Presence::Failed,
        }
    }

    /// The marker's colour and shape. Both are set here, so they change in one place.
    pub fn dot(self, theme: &Theme) -> (Hsla, DotShape) {
        match self {
            // Solid, not dimmed: faded it fell under 3:1. A dash, not a disc: in grayscale the
            // grey disc matches the live, connecting and failed ones (UP-32).
            Presence::NotConnected => (theme.muted_foreground, DotShape::Dash),
            Presence::Connecting | Presence::Reconnecting { .. } => {
                (theme.warning, DotShape::Filled)
            }
            Presence::Live => (theme.success, DotShape::Filled),
            Presence::Idle => (theme.success, DotShape::Ring),
            Presence::Failed => (theme.danger, DotShape::Filled),
        }
    }

    /// The row's marker, in the footprint of a dot, so rows do not shift between states.
    pub fn marker(self, theme: &Theme) -> Div {
        let (color, shape) = self.dot(theme);
        match shape {
            DotShape::Filled => super::theme::status_dot(color, true).size_2(),
            DotShape::Ring => super::theme::status_dot(color, false).size_2(),
            DotShape::Dash => div()
                .size_2()
                .flex()
                .items_center()
                .justify_center()
                .child(div().w_2().h(px(2.)).rounded_full().bg(color)),
        }
    }

    /// Its name in the automation state.
    #[cfg(any(test, feature = "automation"))]
    pub fn name(self) -> &'static str {
        match self {
            Presence::NotConnected => "not_connected",
            Presence::Connecting => "connecting",
            Presence::Live => "live",
            Presence::Reconnecting { .. } => "reconnecting",
            Presence::Idle => "idle",
            Presence::Failed => "failed",
        }
    }
}

/// The row's status while reconnecting. A watch error can carry the server's own message, so
/// only its safe summary is shown.
pub fn reconnecting_text(error: Option<&str>) -> String {
    match error {
        Some(error) => format!(
            "Reconnecting: {}",
            flint_core::redact::safe_error_summary(error)
        ),
        None => "Reconnecting".to_owned(),
    }
}

/// The cluster is reconnecting when at least one watched kind is reconnecting and no kind is
/// synced. Other states alone do not indicate connection trouble. Use the first reconnecting
/// kind with an error by kind ID so the tooltip is stable.
fn reconnecting(states: &HashMap<KindId, SyncState>) -> Option<Presence<'_>> {
    if states.values().any(SyncState::is_current) {
        return None;
    }
    let (_, error) = states
        .iter()
        .filter_map(|(kind, state)| match state {
            SyncState::Reconnecting { error } => Some((kind, error.as_deref())),
            SyncState::RetryingRefusal { error } => Some((kind, Some(error.as_str()))),
            _ => None,
        })
        .min_by_key(|(kind, error)| (error.is_none(), *kind))?;
    Some(Presence::Reconnecting { error })
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
        Discovered, Presence, forget_stale, reconnecting, reconnecting_text, replace_clusters,
        startup_cluster, state_name,
    };
    use flint_core::kubeconfig::ContextInfo;
    use flint_core::{ClusterSummary, ConnectionState, KindId, ServerVersion, SyncState};
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
            generation: 1,
            definition: 1,
            endpoint: 1,
            confirmed: true,
        }
    }

    fn at(name: &str, generation: u64) -> ClusterSummary {
        ClusterSummary {
            generation,
            ..cluster(name)
        }
    }

    fn found(generation: u64) -> Discovered {
        Discovered {
            generation,
            ..Discovered::default()
        }
    }

    // covers UP-06: what a list keeps is decided by generations: a context gone, or one whose slot
    // is newer (re-pointed, changed, back after being removed, even at the same server), loses
    // what was discovered of its old slot.
    #[test]
    fn only_the_current_slots_discovery_is_kept() {
        let mut discovery: HashMap<String, Discovered> = [
            ("kept", found(1)),
            ("removed", found(1)),
            ("started-over", found(1)),
        ]
        .into_iter()
        .map(|(name, d)| (name.to_owned(), d))
        .collect();
        let mut ahead = HashMap::new();
        forget_stale(
            &mut discovery,
            &mut ahead,
            &[at("kept", 1), at("started-over", 2), at("new", 3)],
        );
        let mut left: Vec<&str> = discovery.keys().map(String::as_str).collect();
        left.sort();
        assert_eq!(left, ["kept"]);
    }

    // covers UP-06: discovery of a slot newer than the list the model has (the queue put the list
    // behind it) is held, then used once the list catches up; a list that skipped a slot in
    // between (A to B and back) still drops the oldest.
    #[test]
    fn discovery_ahead_of_its_list_waits_for_it() {
        let mut discovery: HashMap<String, Discovered> = HashMap::new();
        let mut ahead: HashMap<String, Discovered> = HashMap::new();
        let mut clusters = Vec::new();
        replace_clusters(&mut discovery, &mut ahead, &mut clusters, &[at("a", 1)]);
        discovery.insert("a".into(), found(1));
        ahead.insert("a".into(), found(3));
        replace_clusters(&mut discovery, &mut ahead, &mut clusters, &[at("a", 2)]);
        assert!(discovery.is_empty(), "the old slot's kinds go");
        assert!(ahead.contains_key("a"), "a newer slot's wait");
        replace_clusters(&mut discovery, &mut ahead, &mut clusters, &[at("a", 3)]);
        assert_eq!(discovery["a"].generation, 3);
        assert!(ahead.is_empty());
        replace_clusters(&mut discovery, &mut ahead, &mut clusters, &[]);
        assert!(discovery.is_empty(), "a removed context is forgotten");
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

    // covers UP-32: not connected reads without colour or a tooltip, by its shape.
    #[test]
    fn not_connected_has_its_own_shape() {
        let theme = gpui_kit::component::Theme::default();
        let shape = |p: Presence| p.dot(&theme).1;
        let not_connected = shape(Presence::NotConnected);
        assert_ne!(not_connected, shape(Presence::Live));
        assert_ne!(not_connected, shape(Presence::Connecting));
        assert_ne!(not_connected, shape(Presence::Failed));
        assert_ne!(not_connected, shape(Presence::Idle));
        assert_ne!(shape(Presence::Live), shape(Presence::Idle));
    }

    // covers UP-24: a cluster switched away from stops its watches, so its dot must not say live.
    #[test]
    fn only_the_active_connected_cluster_is_live() {
        let connected = ConnectionState::Connected(ServerVersion {
            git_version: "v1.31.0".into(),
            platform: "linux/arm64".into(),
        });
        assert_eq!(
            Presence::of(&connected, true, &HashMap::new()),
            Presence::Live
        );
        let states = HashMap::from([(KindId::pods(), SyncState::Reconnecting { error: None })]);
        assert_eq!(
            Presence::of(&connected, true, &states),
            Presence::Reconnecting { error: None }
        );
        assert_eq!(Presence::of(&connected, false, &states), Presence::Idle);
        let failed = ConnectionState::Failed(flint_core::SafeErrorText::reduced("refused"));
        assert_eq!(Presence::of(&failed, true, &states), Presence::Failed);
        assert_eq!(
            Presence::of(&ConnectionState::Connecting, true, &states),
            Presence::Connecting
        );
        assert_eq!(
            Presence::of(&ConnectionState::Disconnected, true, &states),
            Presence::NotConnected
        );
        assert_eq!(Presence::Idle.name(), "idle");
    }

    #[test]
    fn connection_trouble_requires_a_reconnecting_kind_and_no_synced_kind() {
        assert_eq!(reconnecting(&HashMap::new()), None);
        let pods = KindId::pods();
        let mut states = HashMap::from([
            (KindId::new("", "configmaps"), SyncState::Idle),
            (KindId::new("", "nodes"), SyncState::Loading),
            (KindId::new("", "namespaces"), SyncState::Unavailable),
            (
                KindId::new("", "secrets"),
                SyncState::Failed {
                    error: "forbidden".into(),
                },
            ),
        ]);
        assert_eq!(reconnecting(&states), None);
        states.insert(pods, SyncState::Reconnecting { error: None });
        assert_eq!(
            reconnecting(&states),
            Some(Presence::Reconnecting { error: None })
        );
        states.insert(KindId::new("", "nodes"), SyncState::Synced);
        assert_eq!(reconnecting(&states), None);
    }

    #[test]
    fn reconnecting_text_shows_only_a_safe_summary() {
        assert_eq!(reconnecting_text(None), "Reconnecting");
        let text = reconnecting_text(Some("denied for \"s3cret-value\"\nsecond line"));
        assert!(text.starts_with("Reconnecting: denied for"), "{text}");
        assert!(!text.contains("s3cret-value"), "{text}");
        assert!(!text.contains("second line"), "{text}");
    }

    #[test]
    fn reconnecting_uses_the_first_kind_with_an_error() {
        let states = HashMap::from([
            (
                KindId::new("", "configmaps"),
                SyncState::Reconnecting { error: None },
            ),
            (
                KindId::pods(),
                SyncState::Reconnecting {
                    error: Some("timed out".into()),
                },
            ),
            (
                KindId::new("", "nodes"),
                SyncState::Reconnecting {
                    error: Some("connection dropped".into()),
                },
            ),
        ]);
        assert_eq!(
            reconnecting(&states),
            Some(Presence::Reconnecting {
                error: Some("connection dropped")
            })
        );
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
