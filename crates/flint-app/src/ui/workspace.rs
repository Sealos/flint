//! The main window: the cluster sidebar, the active cluster's navigation, and the main area
//! with a header (breadcrumb, namespace filter, watch mode), the resource list, and the detail
//! pane, above the bottom dock (log tabs, which outlive cluster switches).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use flint_core::redact::safe_error_summary;
use flint_core::settings::{AiExposureState, PaneWidths, State};
use flint_core::{
    ClusterId, ClusterSummary, ConnectionState, KindId, NavEntry, SyncState, WatchMode,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::resizable::{HandleEdge, resize_handle};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::clusters::{ClustersModel, Presence, state_label};
use super::detail::{DetailEvent, DetailTab, DetailView, ObjectRef, TakeFocus};
use super::dock::{self, Dock, DockEvent, DockTab, TabKey};
use super::dock_model::TabCluster;
use super::list::{ListEvent, ResourceList};
use super::log_view::{LogRequest, LogView};
use super::nav::NavPanel;
use super::notice::notice;
use super::registry::{
    self, AddNamespace, DefaultWatchAllKinds, DefaultWatchOpenViewsOnly, ExposeToAi, HideFromAi,
    RetryConnection, SelectCluster, ShowAllNamespaces, ToggleNamespace, UseDefaultWatchMode,
    WatchAllKinds, WatchOpenViewsOnly,
};
use super::row_menu;
use super::startup::StartupView;
use super::terminal_model::ShellRequest;
use super::terminal_view::TerminalView;
use super::theme;
use super::ui_state::{self, UiState};
use flint_core::display;

/// The height of the main area's header and the list's toolbar.
pub const HEADER_HEIGHT: f32 = 32.;

/// The panes the user can resize by dragging their edge, with default and limit widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Clusters,
    Navigation,
    Detail,
}

impl Pane {
    fn default_width(self) -> f32 {
        match self {
            Pane::Clusters => 200.,
            Pane::Navigation => 220.,
            Pane::Detail => 460.,
        }
    }

    fn limits(self) -> (f32, f32) {
        match self {
            Pane::Clusters => (140., 420.),
            Pane::Navigation => (150., 420.),
            Pane::Detail => (300., 1600.),
        }
    }

    fn get(self, panes: &PaneWidths) -> f32 {
        let saved = match self {
            Pane::Clusters => panes.clusters,
            Pane::Navigation => panes.navigation,
            Pane::Detail => panes.detail,
        };
        let (min, max) = self.limits();
        saved.map_or(self.default_width(), |w| (w as f32).clamp(min, max))
    }

    fn set(self, panes: &mut PaneWidths, width: f32) {
        let (min, max) = self.limits();
        let width = Some(width.clamp(min, max).round() as u32);
        match self {
            Pane::Clusters => panes.clusters = width,
            Pane::Navigation => panes.navigation = width,
            Pane::Detail => panes.detail = width,
        }
    }
}

/// What a pane-edge drag carries.
#[derive(Debug, Clone, Copy)]
struct PaneDrag(Pane);

impl Render for PaneDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// What a drag of the dock's top edge carries.
#[derive(Debug, Clone, Copy)]
struct DockDrag;

impl Render for DockDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The list keeps at least this much room when the detail pane is dragged wider.
/// 360 px fits Name, Namespace and a status column of a pods list.
const MIN_LIST_WIDTH: f32 = 360.;
/// The pane widths shown in a window `window_width` wide: as saved, but when the window is too
/// narrow for them and the list's [`MIN_LIST_WIDTH`], the detail pane gives way first (down to
/// its minimum), then the navigation, then the cluster sidebar. Saved widths are untouched, so a
/// wider window gets them back.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layout {
    clusters: f32,
    navigation: f32,
    detail: f32,
}

fn layout(panes: &PaneWidths, window_width: f32, nav_shown: bool, detail_open: bool) -> Layout {
    let mut widths = Layout {
        clusters: Pane::Clusters.get(panes),
        // Hidden (no active cluster): it takes no room.
        navigation: if nav_shown {
            Pane::Navigation.get(panes)
        } else {
            0.
        },
        detail: if detail_open {
            Pane::Detail.get(panes)
        } else {
            0.
        },
    };
    let mut over =
        widths.clusters + widths.navigation + widths.detail + MIN_LIST_WIDTH - window_width;
    let mut give = |width: &mut f32, min: f32| {
        let taken = over.clamp(0., (*width - min).max(0.));
        *width -= taken;
        over -= taken;
    };
    if detail_open {
        give(&mut widths.detail, Pane::Detail.limits().0);
    }
    if nav_shown {
        give(&mut widths.navigation, Pane::Navigation.limits().0);
    }
    give(&mut widths.clusters, Pane::Clusters.limits().0);
    widths
}

/// Pane widths are written to `state.toml` once a drag has paused this long.
const PANE_SAVE_DELAY: Duration = Duration::from_millis(400);

pub struct Workspace {
    model: Entity<ClustersModel>,
    /// `FLINT_APPEARANCE`, a development aid that wins over the setting.
    forced_appearance: Option<ThemeMode>,
    /// The appearance last forced on the native window chrome (`Some(None)`: following macOS),
    /// so it is set only when it changes.
    chrome: Option<Option<WindowAppearance>>,
    ui_state: UiState,
    nav: Entity<NavPanel>,
    /// The cluster the main area shows: the active one.
    cluster: Option<ClusterId>,
    list: Option<Entity<ResourceList>>,
    detail: Option<Entity<DetailView>>,
    /// The bottom dock. It does not change with the active cluster.
    dock: Entity<Dock>,
    /// Namespace names of the active cluster, for the namespace filter.
    namespaces: Vec<String>,
    /// Whether the user may list namespaces (`Failed` when not), for the namespace filter.
    namespaces_state: SyncState,
    /// Types a namespace into the filter (for users who may not list namespaces).
    namespace_input: Entity<InputState>,
    /// Pane widths, as dragged.
    panes: PaneWidths,
    /// `FLINT_VIEW`, until it has been opened.
    startup_view: Option<StartupView>,
    /// Open the next detail view on its YAML tab (`FLINT_VIEW=…#yaml`).
    startup_yaml: bool,
    /// Open the next detail view's pod logs (`FLINT_VIEW=…#logs`).
    startup_logs: bool,
    /// Why the last Logs or Shell did not open, shown over the dock until the next one does,
    /// the selection, list or cluster changes, or [`ACTION_ERROR_SHOWN`] has passed.
    action_error: Option<String>,
    _action_error: Task<()>,
    /// The last write of `state.toml`; the next one waits for it, so writes land in order.
    saving: Option<Task<()>>,
    /// Writes the pane widths once a drag pauses.
    _save_panes: Task<()>,
    /// Focused when nothing inside is, so keys and registered actions reach the workspace. It is
    /// tracked by an element nothing can click, so clicks elsewhere keep focus where it was.
    focus: FocusHandle,
    _namespaces: Task<()>,
    _tick: Task<()>,
    _list_subscriptions: Vec<Subscription>,
    _detail_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
    /// The confirmation last opened, which the automation state shows while it is open.
    dialog: Option<DialogText>,
}

impl Workspace {
    pub fn new(
        model: Entity<ClustersModel>,
        ui_state: UiState,
        startup_view: Option<StartupView>,
        forced_appearance: Option<ThemeMode>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Flint Light or Flint Dark, per the `appearance` setting, following macOS live when it
        // is `system`, and re-applied whenever the settings change.
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            this.show_appearance(window, cx);
        });
        let observe = cx.observe_in(&model, window, |this, _, window, cx| {
            this.show_appearance(window, cx);
            this.sync(window, cx);
            cx.notify();
        });
        let nav = cx.new(|_| NavPanel::new());
        // Quitting right after a drag (or another change) still saves state.toml.
        let on_quit = cx.on_app_quit(|this: &mut Self, _| {
            let save = this.ui_state.set_panes(this.panes);
            let pending = this.saving.take();
            async move {
                if let Some(pending) = pending {
                    pending.await;
                }
                if let Some(save) = save {
                    ui_state::save(save);
                }
            }
        });
        let namespace_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Add a namespace by name…"));
        let on_namespace_input = cx.subscribe_in(
            &namespace_input,
            window,
            |this, input, event, window, cx| {
                // Enter runs AddNamespace's handler (this listener holds the workspace, so it
                // cannot go through the registry's guards, which read it).
                if let InputEvent::PressEnter { .. } = event {
                    let namespace = input.read(cx).value().to_string();
                    this.add_namespace(&AddNamespace { namespace }, window, cx);
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                }
            },
        );
        // Once a second: refresh the navigation's per-kind watch states (a short lock in the
        // core, no network).
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    let Some(cluster) = this.cluster.clone() else {
                        return;
                    };
                    let states = this.model.read(cx).core().sync_states(&cluster);
                    this.nav.update(cx, |nav, cx| nav.set_states(states, cx));
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let dock = cx.new(Dock::new);
        let on_dock = cx.subscribe_in(&dock, window, |this, _, event, window, cx| match event {
            DockEvent::Blurred => this.reclaim_focus(window, cx),
            DockEvent::Restart(id) => this.restart_tab(*id, window, cx),
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        // Focus left with a closed view (a cluster switch drops the list): take it back, so keys
        // and registered actions still reach the workspace.
        let on_focus_lost =
            cx.on_focus_lost(window, |this, window, cx| this.reclaim_focus(window, cx));
        registry::install(focus.clone(), Self::guards(cx.entity().downgrade()), cx);
        let mut this = Self {
            model,
            focus,
            forced_appearance,
            chrome: None,
            panes: ui_state.panes(),
            ui_state,
            nav,
            cluster: None,
            list: None,
            detail: None,
            dock,
            namespaces: Vec::new(),
            namespaces_state: SyncState::Idle,
            namespace_input,
            startup_yaml: startup_view.as_ref().is_some_and(|v| v.yaml),
            startup_logs: startup_view.as_ref().is_some_and(|v| v.logs),
            action_error: None,
            _action_error: Task::ready(()),
            startup_view,
            saving: None,
            _save_panes: Task::ready(()),
            _namespaces: Task::ready(()),
            _tick: tick,
            _list_subscriptions: Vec::new(),
            _detail_subscriptions: Vec::new(),
            dialog: None,
            _subscriptions: vec![
                observe,
                appearance,
                on_namespace_input,
                on_quit,
                on_focus_lost,
                on_dock,
            ],
        };
        this.show_appearance(window, cx);
        this
    }

    /// Shows the theme the `appearance` setting, macOS and `FLINT_APPEARANCE` call for, and
    /// makes the native window chrome (titlebar, border) match it.
    fn show_appearance(&mut self, window: &Window, cx: &mut App) {
        let setting = self.model.read(cx).settings.appearance;
        // Clear a forced chrome appearance before reading the window's: while one is forced,
        // the window reports it instead of macOS's.
        let chrome = theme::chrome(setting, self.forced_appearance);
        if self.chrome != Some(chrome) {
            self.chrome = Some(chrome);
            cx.set_window_appearance(chrome);
        }
        let mode = theme::resolve(setting, self.forced_appearance, window.appearance());
        theme::show(mode, cx);
    }

    /// Writes `state.toml` off the UI thread, after any write still running.
    fn persist(&mut self, save: Option<(PathBuf, State)>, cx: &mut Context<Self>) {
        let Some(save) = save else {
            return;
        };
        let previous = self.saving.take();
        self.saving = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            ui_state::save(save);
        }));
    }

    /// Connects (and discovers) the active cluster again after a failure.
    fn retry(&mut self, cx: &mut Context<Self>) {
        if let Some(cluster) = self.cluster.clone() {
            self.model.update(cx, |m, _| m.activate(cluster));
        }
    }

    /// Follows a pane-edge drag, and saves the widths once it pauses.
    fn drag_pane(&mut self, pane: Pane, x: f32, window_width: f32, cx: &mut Context<Self>) {
        // Measured from the panes as shown, which a narrow window may have shrunk.
        let shown = self.layout(window_width, cx);
        let width = match pane {
            Pane::Clusters => x,
            Pane::Navigation => x - shown.clusters,
            Pane::Detail => window_width - x,
        };
        self.resize_pane(pane, width, window_width, cx);
    }

    /// Sets a pane's width within its limits, measured against the panes as shown (the detail
    /// pane leaves the list [`MIN_LIST_WIDTH`]), and saves the widths once changes pause.
    /// Dragging and `SetPaneWidth` both end here.
    fn resize_pane(&mut self, pane: Pane, width: f32, window_width: f32, cx: &mut Context<Self>) {
        let shown = self.layout(window_width, cx);
        let width = match pane {
            Pane::Detail => {
                width.min(window_width - shown.clusters - shown.navigation - MIN_LIST_WIDTH)
            }
            _ => width,
        };
        let before = self.panes;
        pane.set(&mut self.panes, width);
        // A drag that a narrow window keeps from moving the divider changes nothing on screen;
        // saving it would replace the preference the user gets back in a wider window.
        if self.panes == before || self.layout(window_width, cx) == shown {
            self.panes = before;
            return;
        }
        cx.notify();
        self.save_panes_later(cx);
    }

    /// Saves the pane sizes once changes pause for [`PANE_SAVE_DELAY`].
    fn save_panes_later(&mut self, cx: &mut Context<Self>) {
        let panes = self.panes;
        self._save_panes = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PANE_SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                let save = this.ui_state.set_panes(panes);
                this.persist(save, cx);
            });
        });
    }

    /// The detail pane, showing the selected row, if one is open.
    pub(in crate::ui) fn detail_view(&self) -> Option<Entity<DetailView>> {
        self.detail.clone()
    }

    /// The dock's height as shown in a window `window_height` tall.
    pub(in crate::ui) fn dock_height(&self, window_height: f32) -> f32 {
        dock::dock_height(self.panes.dock, window_height)
    }

    /// Sets the dock's height within its limits and saves it once changes pause. Dragging its
    /// edge and `SetDockHeight` both end here.
    fn resize_dock(&mut self, height: f32, window_height: f32, cx: &mut Context<Self>) {
        let shown = self.dock_height(window_height);
        let height = dock::dock_height(Some(height.max(0.).round() as u32), window_height);
        if height == shown {
            return;
        }
        self.panes.dock = Some(height as u32);
        cx.notify();
        self.save_panes_later(cx);
    }

    /// Opens (or shows) the log tab for `request` in the dock, focusing it if `focus`.
    /// Refused, saying why, while its cluster is not connected.
    pub(in crate::ui) fn open_logs(
        &mut self,
        request: LogRequest,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(reason) = self.sessions_blocked(&request.cluster, cx) {
            self.refuse(format!("Logs: {reason}"), cx);
            return;
        }
        let core = self.model.read(cx).core().clone();
        let cluster = self.tab_cluster(&request.cluster, cx);
        self.open_tab(
            TabKey::Logs(request.clone()),
            cluster,
            |window, cx| DockTab::Logs(cx.new(|cx| LogView::new(core, request, window, cx))),
            focus,
            window,
            cx,
        );
    }

    /// Opens (or shows) a dock tab. With `focus` it takes focus. Without, focus stays where it
    /// is; and if that is in the dock, the tab the user is in stays shown and the new one opens
    /// behind it, so focus is never left in a tab that is not drawn.
    fn open_tab(
        &mut self,
        key: TabKey,
        cluster: TabCluster,
        build: impl FnOnce(&mut Window, &mut Context<Dock>) -> DockTab,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_action_error(cx);
        let kept = (!focus).then(|| window.focused(cx)).flatten();
        let in_dock = kept.is_some() && self.dock.read(cx).has_focus(window, cx);
        self.dock.update(cx, |dock, cx| {
            if in_dock {
                dock.add(key, cluster, build, window, cx)
            } else {
                dock.open(key, cluster, build, window, cx)
            }
        });
        if let Some(kept) = kept.filter(|_| !in_dock) {
            window.focus(&kept, cx);
        }
        cx.notify();
    }

    /// Starts tab `id`'s ended session again (Reconnect Logs, Restart Shell) in a new tab beside
    /// it, keeping the ended one's output: on the server its context points at now, refused,
    /// saying why, while the cluster is not connected.
    fn restart_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.dock.read(cx).tab(id).map(|t| t.item(cx).tab_key()) else {
            return;
        };
        let what = match &key {
            TabKey::Logs(_) => "Logs",
            TabKey::Shell(_) => "Shell",
        };
        if let Some(reason) = self.sessions_blocked(key.context(), cx) {
            self.refuse(format!("{what}: {reason}"), cx);
            return;
        }
        self.clear_action_error(cx);
        let cluster = self.tab_cluster(key.context(), cx);
        let core = self.model.read(cx).core().clone();
        self.dock.update(cx, |dock, cx| {
            dock.open_beside(
                id,
                cluster,
                move |window, cx| match key {
                    TabKey::Logs(request) => {
                        DockTab::Logs(cx.new(|cx| LogView::new(core, request, window, cx)))
                    }
                    TabKey::Shell(request) => {
                        let session = core.exec(request.exec_request(Default::default()));
                        DockTab::Shell(cx.new(|cx| TerminalView::new(session, request, window, cx)))
                    }
                },
                window,
                cx,
            )
        });
    }

    /// Says why Logs or Shell did not open, over the dock, for [`ACTION_ERROR_SHOWN`].
    pub(in crate::ui) fn refuse(&mut self, why: String, cx: &mut Context<Self>) {
        tracing::warn!("{why}");
        self.action_error = Some(why);
        let timer = cx.background_executor().timer(ACTION_ERROR_SHOWN);
        self._action_error = cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |ws, cx| ws.clear_action_error(cx));
        });
        cx.notify();
    }

    /// Takes the reason a Logs or Shell did not open away: it no longer applies.
    fn clear_action_error(&mut self, cx: &mut Context<Self>) {
        if self.action_error.take().is_some() {
            self._action_error = Task::ready(());
            cx.notify();
        }
    }

    /// Why the last Logs or Shell did not open, while it is shown.
    #[cfg(any(test, feature = "automation"))]
    pub(in crate::ui) fn action_error(&self) -> Option<&str> {
        self.action_error.as_deref()
    }

    /// Opens (or shows) the shell tab for `request` in the dock, focusing it if `focus`. The
    /// session starts at the classic 80 by 24; the tab sends its real size once laid out.
    /// Refused, saying why, while its cluster is not connected.
    pub(in crate::ui) fn open_shell(
        &mut self,
        request: ShellRequest,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(reason) = self.sessions_blocked(&request.cluster, cx) {
            self.refuse(format!("Shell: {reason}"), cx);
            return;
        }
        let core = self.model.read(cx).core().clone();
        let cluster = self.tab_cluster(&request.cluster, cx);
        self.open_tab(
            TabKey::Shell(request.clone()),
            cluster,
            |window, cx| {
                let session = core.exec(request.exec_request(Default::default()));
                DockTab::Shell(cx.new(|cx| TerminalView::new(session, request, window, cx)))
            },
            focus,
            window,
            cx,
        );
    }

    /// The cluster a session opened on `context` now runs on: the context and its server.
    fn tab_cluster(&self, context: &str, cx: &App) -> TabCluster {
        let server = self
            .cluster_summary(context, cx)
            .and_then(|c| c.info.server.as_deref());
        TabCluster::new(context, server)
    }

    /// Tells the dock kubeconfig's contexts (its chips tell alike names apart), and ends the
    /// shells whose context was re-pointed at another server (see [`Dock::check_servers`]).
    fn check_dock_servers(&mut self, cx: &mut Context<Self>) {
        let contexts: Vec<String> = self
            .model
            .read(cx)
            .clusters
            .iter()
            .map(|c| c.info.name.clone())
            .collect();
        self.dock
            .update(cx, |dock, cx| dock.set_contexts(contexts, cx));
        if self.dock.read(cx).tab_count() == 0 {
            return;
        }
        let servers: HashMap<String, Option<String>> = self
            .model
            .read(cx)
            .clusters
            .iter()
            .map(|c| (c.info.name.clone(), c.info.server.clone()))
            .collect();
        self.dock.update(cx, |dock, cx| {
            dock.check_servers(|context| servers.get(context).cloned(), cx)
        });
    }

    /// Brings the navigation and main area in line with the model: the active cluster and its
    /// discovery.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.check_dock_servers(cx);
        let model = self.model.read(cx);
        let active = model.active.clone();
        let core = model.core().clone();
        let failed = model
            .active_cluster()
            .is_some_and(|c| matches!(c.state, ConnectionState::Failed(_)));
        let discovered = active
            .as_ref()
            .and_then(|c| model.discovery.get(c))
            .cloned()
            .unwrap_or_default();
        if active != self.cluster {
            // Switching clusters closes the old cluster's views; dropping the list drops its
            // subscription (UP-24).
            self.cluster = active.clone();
            self.close_list(cx);
            self.namespaces.clear();
            self.namespaces_state = SyncState::Idle;
            self._namespaces = match &active {
                Some(cluster) => {
                    // For kinds the user may not list in all namespaces (namespace-limited
                    // lists), the core needs the filter too.
                    let filter = self.ui_state.namespace_filter(cluster).to_vec();
                    let id = cluster.clone();
                    self.model
                        .update(cx, |m, _| m.set_namespace_filter(id, filter));
                    self.watch_namespaces(&core, cluster, cx)
                }
                None => Task::ready(()),
            };
        }
        self.nav.update(cx, |nav, cx| {
            nav.set_connection_failed(failed, cx);
            nav.set_cluster(
                active.clone(),
                discovered.discovery.clone(),
                discovered.error.clone(),
                cx,
            )
        });
        let (Some(cluster), Some(discovery)) = (active, discovered.discovery) else {
            return;
        };
        if self.list.is_some() {
            return;
        }
        let entries = || {
            discovery
                .navigation
                .sections
                .iter()
                .flat_map(|s| s.entries.iter())
        };
        if let Some(view) = self.startup_view.take() {
            // Names as kubectl takes them (`deploy`, `po`, `Deployment.apps`), resolved by the
            // same core resolver as the MCP server.
            let entry = match discovery.nav_entry(&view.kind) {
                Ok(entry) => Some(entry.clone()),
                Err(e @ flint_core::NavEntryError::Unresolved(_)) => {
                    tracing::warn!(kind = view.kind, "FLINT_VIEW names no kind: {e}");
                    None
                }
                Err(e @ flint_core::NavEntryError::NotInNavigation(_)) => {
                    tracing::warn!(kind = view.kind, "FLINT_VIEW cannot be opened: {e}");
                    None
                }
            };
            if let Some(entry) = entry {
                self.open_kind(&entry, window, cx);
                if let (Some(list), Some(name)) = (&self.list, view.name) {
                    list.update(cx, |list, cx| {
                        list.select_when_listed(view.namespace, name, cx)
                    });
                }
            }
        } else if let Some(entry) = self
            .ui_state
            .last_view(&cluster)
            .and_then(|kind| entries().find(|e| e.id.as_str() == kind))
            .cloned()
        {
            // The kind last open on this cluster, also after a restart.
            self.open_kind(&entry, window, cx);
        }
    }

    /// Keeps `self.namespaces` in step with the cluster's namespaces, for the filter.
    fn watch_namespaces(
        &self,
        core: &flint_core::CoreHandle,
        cluster: &str,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let mut subscription = core.subscribe(cluster, &KindId::namespaces());
        cx.spawn(async move |this, cx| {
            while subscription.changed().await {
                let snapshot = subscription.latest();
                let names: Vec<String> = snapshot.names().into_iter().map(str::to_owned).collect();
                let state = snapshot.state.clone();
                if this
                    .update(cx, |this, cx| {
                        if this.namespaces != names || this.namespaces_state != state {
                            this.namespaces = names;
                            this.namespaces_state = state;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    fn close_list(&mut self, cx: &mut Context<Self>) {
        // Another kind, list or cluster: a reason given for the rows left no longer applies.
        self.clear_action_error(cx);
        self.list = None;
        self.detail = None;
        self._list_subscriptions.clear();
        self._detail_subscriptions.clear();
    }

    fn open_kind(&mut self, entry: &NavEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cluster) = self.cluster.clone() else {
            return;
        };
        if self
            .list
            .as_ref()
            .is_some_and(|l| *l.read(cx).kind() == entry.id)
        {
            return;
        }
        self.close_list(cx);
        let core = self.model.read(cx).core().clone();
        let namespaces = self.ui_state.namespace_filter(&cluster).to_vec();
        let list =
            cx.new(|cx| ResourceList::new(core, cluster.clone(), entry, namespaces, window, cx));
        if row_menu::has_row_actions(&entry.id) {
            let menu = row_menu::row_menu(cx.weak_entity());
            list.update(cx, |list, _| list.set_row_menu(menu));
        }
        self._list_subscriptions = vec![cx.subscribe_in(&list, window, Self::on_list_event)];
        let focus = list.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        self.list = Some(list);
        let save = self.ui_state.set_last_view(&cluster, entry.id.as_str());
        self.persist(save, cx);
        let selected = Some(entry.id.clone());
        self.nav
            .update(cx, |nav, cx| nav.set_selected(selected, cx));
        cx.notify();
    }

    fn on_list_event(
        &mut self,
        list: &Entity<ResourceList>,
        event: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ListEvent::Selected(None) => {
                self.clear_action_error(cx);
                self.detail = None;
                self._detail_subscriptions.clear();
                let focus = list.read(cx).focus_handle().clone();
                window.focus(&focus, cx);
            }
            ListEvent::Selected(Some(selected)) => {
                let list = list.read(cx);
                let object = ObjectRef {
                    cluster: list.cluster().to_owned(),
                    kind: list.kind().clone(),
                    kind_label: list.label().to_owned(),
                    namespace: selected.namespace.clone(),
                    name: selected.name.clone(),
                    uid: selected.uid.clone(),
                };
                if self
                    .detail
                    .as_ref()
                    .is_some_and(|d| *d.read(cx).object() == object)
                {
                    return;
                }
                // Moving between rows keeps the tab the user was on.
                let tab = if std::mem::take(&mut self.startup_yaml) {
                    DetailTab::Yaml
                } else {
                    self.detail
                        .as_ref()
                        .map_or(DetailTab::Overview, |d| d.read(cx).tab())
                };
                // A reason given for the row left no longer applies.
                self.clear_action_error(cx);
                let core = self.model.read(cx).core().clone();
                let startup_logs = std::mem::take(&mut self.startup_logs);
                let detail = cx.new(|cx| {
                    let mut detail = DetailView::new(core, object, tab, window, cx);
                    if startup_logs {
                        detail.open_logs_when_loaded(cx);
                    }
                    detail
                });
                let list = self.list.clone();
                self._detail_subscriptions =
                    vec![
                        cx.subscribe_in(&detail, window, move |this, _, event, window, cx| {
                            match event {
                                DetailEvent::Close => {
                                    if let Some(list) = &list {
                                        list.update(cx, |list, cx| list.clear_selection(cx));
                                    }
                                }
                                DetailEvent::OpenLogs(request, focus) => {
                                    let focus = takes_focus(focus, window, cx);
                                    this.open_logs(request.clone(), focus, window, cx)
                                }
                                DetailEvent::OpenShell(request, focus) => {
                                    let focus = takes_focus(focus, window, cx);
                                    this.open_shell(request.clone(), focus, window, cx)
                                }
                                DetailEvent::Refused(why) => this.refuse(why.clone(), cx),
                            }
                        }),
                    ];
                self.detail = Some(detail);
            }
            ListEvent::SelectedChanged => {
                if let Some(detail) = &self.detail {
                    detail.update(cx, |detail, cx| detail.refresh(window, cx));
                }
            }
            ListEvent::SelectedGone => {
                if let Some(detail) = &self.detail {
                    detail.update(cx, |detail, cx| detail.mark_gone(cx));
                }
            }
        }
        cx.notify();
    }

    /// Sets the active cluster's namespace filter, remembers it in `state.toml`, and applies it
    /// to the open list.
    fn set_namespace_filter(&mut self, namespaces: Vec<String>, cx: &mut Context<Self>) {
        let Some(cluster) = self.cluster.clone() else {
            return;
        };
        let save = self.ui_state.set_namespace_filter(&cluster, namespaces);
        self.persist(save, cx);
        let filter = self.ui_state.namespace_filter(&cluster).to_vec();
        let id = cluster.clone();
        let core_filter = filter.clone();
        self.model
            .update(cx, |m, _| m.set_namespace_filter(id, core_filter));
        if let Some(list) = &self.list {
            list.update(cx, |list, cx| list.set_namespaces(filter, cx));
        }
        cx.notify();
    }

    fn render_sidebar(
        &self,
        widths: &Layout,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let model = self.model.read(cx);
        let active = model.active.clone();

        let mut list = v_flex().gap_px().px_1p5().py_1();
        if let Some(error) = &model.settings_error {
            list = list.child(sidebar_error(
                "settings-error",
                "settings.toml is invalid. Nothing is exposed to AI until it is fixed.",
                error,
                theme,
            ));
        }
        if !model.loaded {
            list = list.child(
                div()
                    .px_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading kubeconfig…"),
            );
        } else if model.clusters.is_empty() {
            list = list.child(empty_state(model, theme));
        }
        for cluster in &model.clusters {
            let is_active = active.as_deref() == Some(cluster.info.name.as_str());
            let exposure = model.ai_exposure(cluster);
            let settings_ok = model.settings_error.is_none();
            list = list.child(self.render_cluster_row(
                cluster,
                is_active,
                exposure,
                settings_ok,
                theme,
            ));
        }
        for (i, (path, error)) in model.kubeconfig_errors.iter().enumerate() {
            // Only a sanitized summary, here and in the tooltip: the full parse error can quote
            // lines of the file, credentials included (as the MCP server does).
            let summary = safe_error_summary(error);
            let detail = format!("{}\n{summary}", path.display());
            list = list.child({
                let text = format!("{}: {summary}", display::file_name(path));
                div()
                    .id(SharedString::from(format!("kubeconfig-error-{i}")))
                    .aria_label(text.clone())
                    .aria_description(detail.clone())
                    .px_2()
                    .pt_2()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(text)
                    .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
                    .test_support()
            });
        }
        if let Some(error) = &model.last_error {
            list = list.child(
                div()
                    .px_2()
                    .pt_2()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }

        v_flex()
            .w(px(widths.clusters))
            .h_full()
            .flex_none()
            .relative()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child("CLUSTERS"),
            )
            .child(
                div()
                    .id("cluster-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(pane_handle(
                "clusters-edge",
                Pane::Clusters,
                HandleEdge::Trailing,
            ))
    }

    fn render_cluster_row(
        &self,
        cluster: &ClusterSummary,
        is_active: bool,
        exposure: AiExposureState,
        settings_ok: bool,
        theme: &Theme,
    ) -> AnyElement {
        let name = cluster.info.name.clone();
        let server = cluster.info.server.clone();
        let title = display::short_name(&name);
        let subtitle = display::subtitle(&name, cluster.info.server_host());
        let presence = Presence::of(&cluster.state, is_active);
        let state_text = match &cluster.state {
            ConnectionState::Connected(v) if presence == Presence::Idle => format!(
                "Connected, Kubernetes {}. Idle: not watched while another cluster is active",
                v.git_version
            ),
            ConnectionState::Connected(v) => format!("Connected, Kubernetes {}", v.git_version),
            ConnectionState::Failed(message) => format!("Connection failed: {message}"),
            other => state_label(other).to_owned(),
        };
        let exposure_text = match &exposure {
            AiExposureState::Hidden => "Hidden from AI agents".to_owned(),
            AiExposureState::Exposed => "Exposed to AI agents (flint mcp, read-only)".to_owned(),
            AiExposureState::ServerChanged { exposed_server } => format!(
                "Cluster changed: exposed at {exposed_server}, now points elsewhere. Hidden from \
                 AI agents until you expose it again."
            ),
        };
        let hover_text = format!(
            "{}\n{}\n{}\n{}\nfrom {}",
            name,
            server.as_deref().unwrap_or("no server"),
            state_text,
            exposure_text,
            cluster.info.source.display()
        );
        let changed = matches!(exposure, AiExposureState::ServerChanged { .. });
        // Filled while live (or failing, or connecting); a ring while idle: connected, but not
        // watched.
        let dot = div()
            .id(SharedString::from(format!("cluster-dot-{name}")))
            .size_2()
            .flex_none()
            .rounded_full()
            .map(|el| match presence {
                Presence::NotConnected => el.bg(theme.muted_foreground.opacity(0.4)),
                Presence::Connecting => el.bg(theme.warning),
                Presence::Live => el.bg(theme.success),
                Presence::Idle => el.border_1().border_color(theme.success),
                Presence::Failed => el.bg(theme.danger),
            })
            .test_support();

        let tooltip = hover_text.clone();
        h_flex()
            .id(SharedString::from(format!("cluster-{name}")))
            // Named for assistive technology (and view tests): the short title, then the
            // tooltip's full context, server and state.
            .aria_label(title.clone())
            .aria_description(tooltip)
            .gap_2()
            .px_1p5()
            .py_1()
            .rounded(theme.radius)
            .cursor_pointer()
            // The active cluster is marked with the accent, as selections are.
            .relative()
            .when(is_active, |el| {
                el.bg(theme.list_active).child(super::theme::selection_edge(
                    theme.list_active_border,
                    px(4.),
                ))
            })
            .when(!is_active, |el| el.hover(|el| el.bg(theme.list_hover)))
            .tooltip(move |window, cx| Tooltip::new(hover_text.clone()).build(window, cx))
            .on_click(registry::on_click(SelectCluster {
                cluster: name.clone(),
            }))
            .child(dot)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().line_height(px(18.)).truncate().child(title))
                    .child(
                        div()
                            .id(SharedString::from(format!("cluster-server-{name}")))
                            .text_xs()
                            .line_height(px(14.))
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(subtitle)
                            .test_support(),
                    )
                    // On a line of its own, so it never takes the server's room.
                    .when(changed, |col| {
                        col.child(
                            div()
                                .id(SharedString::from(format!("cluster-changed-{name}")))
                                .pt_0p5()
                                .flex()
                                .child(Tag::warning().outline().xsmall().child("Cluster changed"))
                                .tooltip(|window, cx| {
                                    Tooltip::new(
                                        "Cluster changed: this context points at a different \
                                         server than the one exposed to AI agents.",
                                    )
                                    .build(window, cx)
                                })
                                .test_support(),
                        )
                    }),
            )
            // Neutral: the toggle next to it already shows the exposure in the accent.
            .when(exposure.is_exposed(), |row| {
                row.child(Tag::secondary().outline().xsmall().flex_none().child("AI"))
            })
            .child(self.render_exposure_toggle(&name, server, exposure, settings_ok))
            .test_support()
            .into_any_element()
    }

    /// The Expose to AI button. Exposing asks for confirmation naming the context and server;
    /// hiding does not. It is not a tab stop, so a stray Space or Enter cannot flip it.
    fn render_exposure_toggle(
        &self,
        name: &str,
        server: Option<String>,
        exposure: AiExposureState,
        settings_ok: bool,
    ) -> AnyElement {
        let cluster = name.to_owned();
        let button = Button::new(SharedString::from(format!("expose-{name}")))
            .icon(IconName::Bot)
            .xsmall()
            .tab_stop(false);
        if !settings_ok {
            return button
                .ghost()
                .disabled(true)
                .tooltip("settings.toml is invalid; fix it to change AI exposure.")
                .into_any_element();
        }
        if server.is_none() {
            if matches!(exposure, AiExposureState::ServerChanged { .. }) {
                // A stale entry for a context that lost its server: it can only be removed.
                return button
                    .ghost()
                    .tooltip("Cluster changed: this context has no API server now. Click to remove its old AI exposure.")
                    .on_click(registry::on_click_contained(HideFromAi { cluster }))
                    .into_any_element();
            }
            return button
                .ghost()
                .disabled(true)
                .tooltip("This context has no API server, so it cannot be exposed to AI.")
                .into_any_element();
        }
        match exposure {
            AiExposureState::Exposed => button
                .primary()
                .tooltip("Exposed to AI agents (Flint's MCP server, read-only). Click to hide.")
                .on_click(registry::on_click_contained(HideFromAi { cluster }))
                .into_any_element(),
            AiExposureState::ServerChanged { .. } => {
                // Either expose the new server (after confirming) or drop the stale entry. The
                // wrapper keeps the click from also selecting the cluster row.
                let menu = button
                    .ghost()
                    .tooltip(
                        "Cluster changed: this context now points at a different server. Click \
                         to expose it again or to remove the old exposure.",
                    )
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        menu.item(PopupMenuItem::new("Expose at the new server…").on_click(
                            registry::on_click(ExposeToAi {
                                cluster: cluster.clone(),
                            }),
                        ))
                        .item(
                            PopupMenuItem::new("Remove the old exposure").on_click(
                                registry::on_click(HideFromAi {
                                    cluster: cluster.clone(),
                                }),
                            ),
                        )
                    });
                div()
                    .flex_none()
                    // not-an-action: keeps the click on the menu button from selecting the row.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(menu)
                    .into_any_element()
            }
            AiExposureState::Hidden => button
                .ghost()
                .tooltip(
                    "Hidden from AI agents. Click to expose it to Flint's MCP server (read-only).",
                )
                .on_click(registry::on_click_contained(ExposeToAi { cluster }))
                .into_any_element(),
        }
    }

    fn render_nav_column(&self, width: f32, theme: &Theme) -> impl IntoElement {
        v_flex()
            .w(px(width))
            .h_full()
            .flex_none()
            .relative()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child("RESOURCES"),
            )
            .child(div().flex_1().min_h_0().child(self.nav.clone()))
            .child(pane_handle(
                "nav-edge",
                Pane::Navigation,
                HandleEdge::Trailing,
            ))
    }

    /// The main area's header: where you are (cluster, kind, namespaces, object), the namespace
    /// filter, and the watch mode.
    fn render_header(
        &self,
        cluster: &ClusterSummary,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let name = cluster.info.name.clone();
        let filter = self.ui_state.namespace_filter(&name).to_vec();
        let list = self.list.as_ref().map(|l| l.read(cx));
        let controls = header_controls(cluster, list);
        let ns_summary = namespace_summary(&filter);

        let mut crumbs: Vec<(SharedString, String)> = vec![(
            display::short_name(&name).into(),
            format!(
                "{name}\n{}",
                cluster.info.server.as_deref().unwrap_or("no server")
            ),
        )];
        // The namespaces are not a crumb: the namespace filter next to it already shows them.
        if let Some(list) = list {
            crumbs.push((list.label().to_owned().into(), list.kind().to_string()));
        }
        if let Some(detail) = &self.detail {
            let object = detail.read(cx).object();
            let full = match &object.namespace {
                Some(ns) => format!("{ns}/{}", object.name),
                None => object.name.clone(),
            };
            crumbs.push((object.name.clone().into(), full));
        }
        // The object's name is a resource name: monospace, like the list's names.
        let mono_last = self.detail.is_some();
        let crumb_count = crumbs.len();
        let mut breadcrumb = h_flex().flex_1().min_w_0().gap_1().text_sm();
        for (i, (text, tip)) in crumbs.into_iter().enumerate() {
            if i > 0 {
                breadcrumb = breadcrumb.child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                );
            }
            let last = i + 1 == crumb_count;
            breadcrumb = breadcrumb.child(
                div()
                    .id(("crumb", i))
                    .min_w_0()
                    .truncate()
                    .when(!last, |el| el.text_color(theme.muted_foreground))
                    .when(last, |el| el.font_semibold())
                    .when(last && mono_last, |el| {
                        el.font_family(theme.mono_font_family.clone())
                    })
                    .child(text)
                    .when(!tip.is_empty(), |el| {
                        el.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    }),
            );
        }

        h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(breadcrumb)
            .when(controls.namespaces, |el| {
                el.child(self.render_namespace_filter(&ns_summary, cx))
            })
            .when(controls.watch_mode, |el| {
                el.child(self.render_watch_mode(&name, cx))
            })
    }

    fn render_namespace_filter(&self, summary: &str, cx: &Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let input = self.namespace_input.clone();
        Popover::new("namespace-filter")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("namespace-filter-button")
                    .xsmall()
                    .outline()
                    .label(summary.to_owned())
                    .tooltip("Filter by namespace (remembered per cluster)"),
            )
            .content(move |_, _, cx| {
                let theme = cx.theme().clone();
                let Some(this) = this.upgrade() else {
                    return v_flex();
                };
                let ws = this.read(cx);
                let filter = ws
                    .cluster
                    .as_deref()
                    .map(|c| ws.ui_state.namespace_filter(c).to_vec())
                    .unwrap_or_default();
                // Show filtered namespaces even before (or after) they are listed.
                let mut names = ws.namespaces.clone();
                names.extend(filter.iter().cloned());
                let names = ui_state::normalize(names);
                let denied = matches!(ws.namespaces_state, SyncState::Failed { .. });
                let empty_text = if denied {
                    "You may not list namespaces on this cluster. Type the ones you use below."
                } else {
                    "Loading namespaces…"
                };
                v_flex()
                    .w(px(260.))
                    .gap_1()
                    .child(
                        Checkbox::new("ns-all")
                            .label("All namespaces")
                            .checked(filter.is_empty())
                            .on_click(registry::on_click(ShowAllNamespaces)),
                    )
                    .child(div().h_px().bg(theme.border))
                    .child(
                        v_flex()
                            .id("ns-list")
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .gap_1()
                            .when(names.is_empty(), |el| {
                                el.child(
                                    div()
                                        .id("ns-empty")
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(empty_text),
                                )
                            })
                            .children(names.into_iter().map(|ns| {
                                let checked = filter.contains(&ns);
                                Checkbox::new(SharedString::from(format!("ns-{ns}")))
                                    .label(ns.clone())
                                    .checked(checked)
                                    .on_click(registry::on_click(ToggleNamespace { namespace: ns }))
                            })),
                    )
                    .child(div().h_px().bg(theme.border))
                    .child(Input::new(&input).id("ns-add").small())
            })
    }

    /// The watch mode switch: this cluster's override, and the global default (ADR-0008).
    fn render_watch_mode(&self, cluster: &str, cx: &Context<Self>) -> impl IntoElement {
        let settings = self.model.read(cx).settings.clone();
        let effective = settings.watch_mode_for(cluster);
        let label = match effective {
            WatchMode::All => "Live: all kinds",
            WatchMode::OnView => "Live: open views only",
        };
        let model = self.model.clone();
        let cluster = cluster.to_owned();
        let settings_ok = self.model.read(cx).settings_error.is_none();
        Button::new("watch-mode")
            .xsmall()
            .ghost()
            .label(label)
            .disabled(!settings_ok)
            .tooltip("Which kinds Flint keeps live (watches) on this cluster")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, cx| {
                let settings = model.read(cx).settings.clone();
                let own = settings.cluster_watch_mode.get(&cluster).copied();
                let global = settings.watch_mode;
                menu.label("This cluster")
                    .item(
                        PopupMenuItem::new(format!("Use the default ({})", mode_name(global)))
                            .checked(own.is_none())
                            .on_click(registry::on_click(UseDefaultWatchMode)),
                    )
                    .item(
                        PopupMenuItem::new("All kinds")
                            .checked(own == Some(WatchMode::All))
                            .on_click(registry::on_click(WatchAllKinds)),
                    )
                    .item(
                        PopupMenuItem::new("Open views only")
                            .checked(own == Some(WatchMode::OnView))
                            .on_click(registry::on_click(WatchOpenViewsOnly)),
                    )
                    .separator()
                    .label("Default for all clusters")
                    .item(
                        PopupMenuItem::new("All kinds")
                            .checked(global == WatchMode::All)
                            .on_click(registry::on_click(DefaultWatchAllKinds)),
                    )
                    .item(
                        PopupMenuItem::new("Open views only")
                            .checked(global == WatchMode::OnView)
                            .on_click(registry::on_click(DefaultWatchOpenViewsOnly)),
                    )
            })
    }

    /// The detail pane's width: as dragged, but narrower when the window is, so the list keeps
    /// [`MIN_LIST_WIDTH`] (a remembered width from a wider window would squeeze it).
    fn detail_width(&self, window_width: f32) -> f32 {
        layout(&self.panes, window_width, true, true).detail
    }

    /// The pane widths as shown now, with the panes that are showing.
    fn layout(&self, window_width: f32, cx: &App) -> Layout {
        let nav_shown = self.model.read(cx).active_cluster().is_some();
        layout(&self.panes, window_width, nav_shown, self.detail.is_some())
    }

    fn render_main(&self, window_width: f32, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let model = self.model.read(cx);
        if model.loaded && model.clusters.is_empty() {
            let text = no_contexts_text(
                model.kubeconfig_env.as_deref(),
                !model.kubeconfig_errors.is_empty(),
            );
            return centered(
                v_flex()
                    .id("main-no-contexts")
                    .gap_2()
                    .max_w(px(520.))
                    .child(div().text_lg().child(text.title))
                    .children(text.lines.into_iter().map(|line| {
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(line)
                    }))
                    .test_support(),
            );
        }
        let Some(cluster) = model.active_cluster().cloned() else {
            return centered(
                v_flex()
                    .gap_1()
                    .items_center()
                    .child(div().text_lg().child("Select a cluster"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Flint connects only when you pick one in the sidebar."),
                    ),
            );
        };
        let content: AnyElement = match (&self.list, &cluster.state) {
            // A failed connection stops the watches; say why instead of an endless "Loading".
            (Some(list), state) if !matches!(state, ConnectionState::Failed(_)) => h_flex()
                .flex_1()
                .min_h_0()
                .child(div().flex_1().min_w_0().h_full().child(list.clone()))
                .when_some(self.detail.clone(), |el, detail| {
                    el.child(
                        div()
                            .w(px(self.detail_width(window_width)))
                            .h_full()
                            .flex_none()
                            .relative()
                            .border_l_1()
                            .border_color(theme.border)
                            .child(detail)
                            .child(pane_handle(
                                "detail-edge",
                                Pane::Detail,
                                HandleEdge::Leading,
                            )),
                    )
                })
                .into_any_element(),
            (None, ConnectionState::Connected(version)) => centered(
                v_flex()
                    .gap_1()
                    .items_center()
                    .child(div().text_sm().child(format!(
                        "Connected. Kubernetes {} ({})",
                        version.git_version, version.platform
                    )))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Pick a kind in the navigation."),
                    ),
            ),
            (_, ConnectionState::Failed(message)) => centered(
                v_flex()
                    .gap_1()
                    .max_w(px(640.))
                    .child(div().text_color(theme.danger).child("Connection failed"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(message.to_string()),
                    )
                    .child(
                        div().pt_2().child(
                            Button::new("connection-retry")
                                .small()
                                .outline()
                                .label("Retry")
                                .on_click(registry::on_click(RetryConnection)),
                        ),
                    ),
            ),
            _ => centered(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Connecting…"),
            ),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_header(&cluster, theme, cx))
            .child(content)
            .into_any_element()
    }
}

/// Asks before exposing a cluster to AI agents, naming the context and the server it points at.
fn confirm_expose(
    window: &mut Window,
    cx: &mut App,
    model: Entity<ClustersModel>,
    context: String,
    server: String,
    dialog: &DialogText,
) {
    let dialog = dialog.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (model, context, server) = (model.clone(), context.clone(), server.clone());
        // Test builds open it at once, so automation sees it without waiting for an animation.
        #[cfg(feature = "automation")]
        let alert = alert.entrance(gpui_kit::component::dialog::DialogEntrance::None);
        alert
            .confirm()
            .title(dialog.title)
            .description(
                div()
                    .id("expose-description")
                    .aria_label(dialog.text.clone())
                    .child(dialog.text.clone())
                    .test_support(),
            )
            .ok_text(dialog.ok)
            .cancel_text(dialog.cancel)
            .on_ok(move |_, _, cx| {
                model.update(cx, |m, _| m.expose_to_ai(context.clone(), server.clone()));
                true
            })
    });
}

/// What a confirmation dialog says; the workspace keeps the open one for the automation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DialogText {
    pub title: &'static str,
    pub text: String,
    pub ok: &'static str,
    pub cancel: &'static str,
}

/// The Expose to AI confirmation, naming the context and the server it points at.
fn expose_dialog(context: &str, server: &str, previous: Option<&str>) -> DialogText {
    let mut text = format!(
        "AI agents using Flint's MCP server will be able to read this cluster: resources, events \
         and pod logs. They cannot change anything. Only Secret values are hidden: everything else, \
         pod logs and event messages included, reaches the agent as the cluster returns it, so any \
         password or token in it does too.\n\n\
         Context: {context}\nServer: {server}"
    );
    if let Some(previous) = previous {
        text.push_str(&format!(
            "\n\nThis context was exposed at {previous}, which is a different server."
        ));
    }
    DialogText {
        title: "Expose to AI agents?",
        text,
        ok: "Expose",
        cancel: "Cancel",
    }
}

/// An error card at the top of the sidebar. Only a sanitized summary of `error` is shown, in the
/// card and its tooltip: parse errors can quote the file, which may hold credentials.
fn sidebar_error(
    id: &'static str,
    message: &'static str,
    error: &str,
    theme: &Theme,
) -> AnyElement {
    let summary = safe_error_summary(error);
    let tip = summary.clone();
    div()
        .id(id)
        .aria_label(message)
        .aria_description(summary.clone())
        .mx_1()
        .mb_2()
        .px_2()
        .py_1p5()
        .rounded_md()
        .border_1()
        .border_color(theme.danger)
        .bg(theme.danger.opacity(0.1))
        .text_xs()
        .text_color(theme.danger)
        .child(message)
        .child(
            div()
                .text_color(theme.muted_foreground)
                .line_clamp(2)
                .child(summary),
        )
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .test_support()
        .into_any_element()
}

/// The drag handle on one edge of a resizable pane.
fn pane_handle(id: &'static str, pane: Pane, edge: HandleEdge) -> impl IntoElement {
    resize_handle::<PaneDrag, PaneDrag>(id, Axis::Horizontal)
        .inside(edge)
        .on_drag(PaneDrag(pane), |drag, _, _, cx| {
            cx.stop_propagation();
            cx.new(|_| *drag)
        })
}

/// What to show when no contexts were found: the files searched and the `KUBECONFIG` value.
fn empty_state(model: &ClustersModel, theme: &Theme) -> AnyElement {
    let kubeconfig_env = match &model.kubeconfig_env {
        Some(v) if !v.is_empty() => v.to_string_lossy().into_owned(),
        Some(_) => "(set but empty)".to_owned(),
        None => "(not set)".to_owned(),
    };
    v_flex()
        .px_2()
        .gap_1()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(
            div()
                .id("kubeconfig-empty")
                .aria_label("No contexts found in your kubeconfig files.")
                .child("No contexts found in your kubeconfig files.")
                .test_support(),
        )
        .child({
            let line = format!("KUBECONFIG: {kubeconfig_env}");
            let tip = line.clone();
            div()
                .id("kubeconfig-env")
                .aria_label(line.clone())
                .text_xs()
                .pt_1()
                .truncate()
                .child(line)
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .test_support()
        })
        .child(div().text_xs().child("Searched:"))
        .children(
            model
                .kubeconfig_paths
                .iter()
                .enumerate()
                .map(|(i, (path, exists))| {
                    let line = format!(
                        "{}{}",
                        path.display(),
                        if *exists { "" } else { " (missing)" }
                    );
                    let tip = line.clone();
                    // One line per path, cut at the end with the whole path on hover, instead
                    // of wrapping mid-name.
                    div()
                        .id(("searched-path", i))
                        .aria_label(line.clone())
                        .text_xs()
                        .pl_2()
                        .truncate()
                        .child(line)
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                        .test_support()
                }),
        )
        .when(model.kubeconfig_paths.is_empty(), |el| {
            el.child(div().text_xs().pl_2().child("(no paths: HOME is not set)"))
        })
        .into_any_element()
}

/// What the main area says when no kubeconfig file had a context.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NoContextsText {
    title: &'static str,
    lines: Vec<String>,
}

/// Where Flint looked for contexts and what to do about it, given `KUBECONFIG` as Flint saw it.
/// Paths are named the way the user sets them (`~`, `KUBECONFIG`, `kubeconfig_paths`), never
/// as full local paths: the sidebar lists the files themselves. When a file could not be
/// read (`unreadable`), that is the likely cause, so it is said first.
fn no_contexts_text(kubeconfig_env: Option<&std::ffi::OsStr>, unreadable: bool) -> NoContextsText {
    let searched = match kubeconfig_env {
        Some(v) if !v.is_empty() => "the files KUBECONFIG names".to_owned(),
        // An empty KUBECONFIG falls back to the default file, as with kubectl.
        Some(_) => "~/.kube/config (KUBECONFIG is empty)".to_owned(),
        None => "~/.kube/config (KUBECONFIG is not set)".to_owned(),
    };
    let mut lines = Vec::new();
    if unreadable {
        lines.push(
            "A kubeconfig file could not be read: the sidebar names it and the error. Fix the \
             file, and Flint reloads it."
                .to_owned(),
        );
    }
    lines.extend([
        format!(
            "Flint looked in {searched}, and in any kubeconfig_paths in settings.toml. None \
                 of them has a context."
        ),
        "Create a context (for example with kind create cluster or aws eks \
             update-kubeconfig), or add your kubeconfig files to kubeconfig_paths. Flint reloads \
             these files when they change; a new KUBECONFIG value takes effect when Flint \
             restarts."
            .to_owned(),
        "The sidebar lists every file searched.".to_owned(),
    ]);
    NoContextsText {
        title: "No clusters found",
        lines,
    }
}

/// Which header controls work for the active cluster: none while its connection failed (no
/// namespaces to pick, nothing to watch; the main area offers Retry), and the namespace
/// filter only for a namespaced kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeaderControls {
    namespaces: bool,
    watch_mode: bool,
}

fn header_controls(
    cluster: &ClusterSummary,
    list: Option<&super::list::ResourceList>,
) -> HeaderControls {
    let failed = matches!(cluster.state, ConnectionState::Failed(_));
    HeaderControls {
        namespaces: !failed && list.is_none_or(|l| l.namespaced()),
        watch_mode: !failed,
    }
}

fn centered(content: impl IntoElement) -> AnyElement {
    v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .items_center()
        .justify_center()
        .p_6()
        .overflow_hidden()
        .child(content)
        .into_any_element()
}

fn mode_name(mode: WatchMode) -> &'static str {
    match mode {
        WatchMode::All => "all kinds",
        WatchMode::OnView => "open views only",
    }
}

/// "All namespaces", "demo", or "3 namespaces".
pub fn namespace_summary(filter: &[String]) -> String {
    match filter {
        [] => "All namespaces".to_owned(),
        [one] => one.clone(),
        many => format!("{} namespaces", many.len()),
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let window_width = f32::from(window.viewport_size().width);
        let window_height = f32::from(window.viewport_size().height);
        let cx: &Context<Self> = cx;
        let theme = cx.theme();
        let has_cluster = self.model.read(cx).active_cluster().is_some();
        let widths = self.layout(window_width, cx);
        h_flex()
            .id("workspace")
            .key_context(registry::WORKSPACE)
            .map(|el| self.register_actions(el, cx))
            .child(
                // The fallback focus target: zero-sized, so no click lands on it.
                div()
                    .id("workspace-focus")
                    .absolute()
                    .size_0()
                    .track_focus(&self.focus),
            )
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<PaneDrag>, _, cx| {
                let pane = e.drag(cx).0;
                let x = f32::from(e.event.position.x - e.bounds.origin.x);
                let width = f32::from(e.bounds.size.width);
                this.drag_pane(pane, x, width, cx);
            }))
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DockDrag>, _, cx| {
                let bottom = e.bounds.origin.y + e.bounds.size.height;
                let height = f32::from(bottom - e.event.position.y);
                this.resize_dock(height, f32::from(e.bounds.size.height), cx);
            }))
            .child(self.render_sidebar(&widths, theme, cx))
            .when(has_cluster, |el| {
                el.child(self.render_nav_column(widths.navigation, theme))
            })
            .child(
                // The main area over the dock, which spans it whichever cluster is active.
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(h_flex().flex_1().min_h_0().w_full().child(self.render_main(
                        window_width,
                        theme,
                        cx,
                    )))
                    .when_some(self.action_error.clone(), |el, why| {
                        el.child(notice("action-error", why, theme.danger))
                    })
                    .when(self.dock.read(cx).is_visible(), |el| {
                        el.child(
                            div()
                                .id("dock-pane")
                                .h(px(self.dock_height(window_height)))
                                .flex_none()
                                .w_full()
                                .relative()
                                .border_t_1()
                                .border_color(theme.border)
                                .child(self.dock.clone())
                                .child(
                                    resize_handle::<DockDrag, DockDrag>(
                                        "dock-edge",
                                        Axis::Vertical,
                                    )
                                    .inside(HandleEdge::Leading)
                                    .on_drag(
                                        DockDrag,
                                        |drag, _, _, cx| {
                                            cx.stop_propagation();
                                            cx.new(|_| *drag)
                                        },
                                    ),
                                )
                                .test_support(),
                        )
                    }),
            )
    }
}

/// How long the reason a Logs or Shell did not open stays shown.
pub(in crate::ui) const ACTION_ERROR_SHOWN: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Whether a tab opened for a detail action takes focus (see [`TakeFocus`]).
fn takes_focus(focus: &TakeFocus, window: &Window, cx: &App) -> bool {
    match focus {
        TakeFocus::Yes => true,
        TakeFocus::IfStill(then) => window.focused(cx) == *then,
    }
}

#[path = "workspace_actions.rs"]
mod actions;

#[cfg(test)]
#[path = "leak_tests.rs"]
mod leak_tests;

#[cfg(any(test, feature = "automation"))]
#[path = "workspace_state.rs"]
mod state;

#[cfg(test)]
#[path = "workspace_view_tests.rs"]
mod view_tests;

#[cfg(test)]
mod tests {
    use super::{Layout, MIN_LIST_WIDTH, layout, namespace_summary, no_contexts_text};
    use flint_core::settings::PaneWidths;

    fn list_width(widths: Layout, window: f32) -> f32 {
        window - widths.clusters - widths.navigation - widths.detail
    }

    // covers UP-23: opening an object must leave the list readable in a narrow window.
    #[test]
    fn a_narrow_window_shrinks_the_side_panes_before_the_list() {
        let saved = PaneWidths::default();
        // Wide enough: every pane as saved (the defaults).
        let wide = layout(&saved, 1600., true, true);
        assert_eq!(
            wide,
            Layout {
                clusters: 200.,
                navigation: 220.,
                detail: 460.
            }
        );
        // The detail pane gives way first.
        let medium = layout(&saved, 1100., true, true);
        assert_eq!((medium.clusters, medium.navigation), (200., 220.));
        assert_eq!(list_width(medium, 1100.), MIN_LIST_WIDTH);
        // A third of a 2560 px screen: everything at its minimum, and the list gets the rest.
        let narrow = layout(&saved, 853., true, true);
        assert_eq!(
            narrow,
            Layout {
                clusters: 140.,
                navigation: 150.,
                detail: 300.
            }
        );
        assert_eq!(list_width(narrow, 853.), 263.);
        // With the detail pane closed, the side panes keep their widths while the list fits.
        let closed = layout(&saved, 853., true, false);
        assert_eq!(
            (closed.clusters, closed.navigation, closed.detail),
            (200., 220., 0.)
        );
        // No active cluster: navigation is hidden and takes no room from the sidebar.
        let welcome = layout(&saved, 650., false, false);
        assert_eq!((welcome.clusters, welcome.navigation), (200., 0.));
    }

    // covers UP-07, UP-08
    #[test]
    fn the_expose_confirmation_names_the_context_and_servers() {
        let dialog = super::expose_dialog("kind-dev", "https://127.0.0.1:6443", None);
        assert_eq!(
            (dialog.title, dialog.ok, dialog.cancel),
            ("Expose to AI agents?", "Expose", "Cancel")
        );
        assert!(
            dialog
                .text
                .contains("Context: kind-dev\nServer: https://127.0.0.1:6443")
        );
        assert!(dialog.text.contains("cannot change anything"));
        assert!(dialog.text.contains("Only Secret values are hidden"));
        assert!(dialog.text.contains("pod logs and event messages included"));
        assert!(!dialog.text.contains("different server"));
        let moved = super::expose_dialog(
            "kind-dev",
            "https://127.0.0.1:6443",
            Some("https://10.0.0.1:6443"),
        );
        assert!(moved.text.contains("Server: https://127.0.0.1:6443"));
        assert!(
            moved
                .text
                .contains("exposed at https://10.0.0.1:6443, which is a different server")
        );
    }

    // covers UP-21
    #[test]
    fn namespace_summary_names_the_filter() {
        assert_eq!(namespace_summary(&[]), "All namespaces");
        assert_eq!(namespace_summary(&["demo".into()]), "demo");
        assert_eq!(namespace_summary(&["a".into(), "b".into()]), "2 namespaces");
    }

    // covers UP-02: the empty state names where Flint looked and what to do, without local
    // paths beyond `~` (the sidebar lists the files).
    #[test]
    fn no_contexts_names_the_sources_and_the_fix() {
        let unset = no_contexts_text(None, false);
        let set = no_contexts_text(
            Some(std::ffi::OsStr::new("/Users/someone/private/kube.yaml")),
            false,
        );
        for text in [&unset, &set] {
            let all = text.lines.join(" ");
            assert_eq!(text.title, "No clusters found");
            assert!(all.contains("KUBECONFIG"), "{all}");
            assert!(all.contains("kubeconfig_paths"), "{all}");
            assert!(
                !all.contains("/Users") && !all.contains("someone"),
                "a local path leaked: {all}"
            );
        }
        assert!(
            unset.lines[0].contains("~/.kube/config"),
            "{:?}",
            unset.lines
        );
        assert!(unset.lines[0].contains("KUBECONFIG is not set"));
        assert!(set.lines[0].contains("the files KUBECONFIG names"));
        let empty = no_contexts_text(Some(std::ffi::OsStr::new("")), false);
        assert!(
            empty.lines[0].contains("~/.kube/config (KUBECONFIG is empty)"),
            "an empty KUBECONFIG falls back to ~/.kube/config, as with kubectl: {:?}",
            empty.lines
        );
        // A file that failed to parse is the likely cause: said first.
        let broken = no_contexts_text(None, true);
        assert!(
            broken.lines[0].contains("could not be read"),
            "{:?}",
            broken.lines
        );
        assert_eq!(broken.lines[1..], unset.lines[..]);
    }
}
