//! The main window: the cluster sidebar, the active cluster's navigation, and the main area
//! with a header (breadcrumb, namespace filter, watch mode), the resource list, and the detail
//! pane, above the bottom dock (log tabs, which outlive cluster switches).

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use flint_core::redact::safe_error_summary;
use flint_core::settings::{AiExposureState, AiScope, PaneWidths, State, TrustState};
use flint_core::{
    ClusterId, ClusterSummary, ConnectionState, KindId, NavEntry, Protection, SyncState, WatchMode,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::base::actions::Cancel;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::resizable::{HandleEdge, resize_handle};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::closing;
use super::cluster_tag;
use super::clusters::{
    ClustersModel, Presence, cluster_details, cluster_tooltip, connection_stopped, state_label,
};
use super::confirm::{self, DialogText};
use super::connection_progress;
use super::dashboard::{DashboardEvent, DashboardView};
use super::detail::{
    DetailEvent, DetailTab, DetailView, ObjectRef, Opening, TakeFocus, protected_reason, refused,
};
use super::dock::{self, Dock, DockEvent, DockItem as _, DockTab, TabKey};
use super::dock_model::{LiveSessions, TabCluster, confirmed_servers};
use super::focus::{cycle_focus_within, release_unfocused_input};
use super::icons::FlintIcon;
use super::keep_watching;
use super::list::{ListEvent, ResourceList};
use super::log_view::{LogRequest, LogView};
use super::nav::{NavEvent, NavPanel};
use super::notice::{ServerMove, ServerMoves, notice};
use super::palette::CommandPalette;
use super::registry::{
    self, AddNamespace, BackToList, DefaultWatchAllKinds, DefaultWatchOpenViewsOnly,
    DisconnectCluster, EditYaml, ExposeToAi, FocusFilter, HideFromAi, LockCluster, OnView,
    OpenClusterOverview, OpenObject, OpenView, RenameCluster, RetryConnection, SelectCluster,
    SelectRow, ShowAgentActivity, ShowAllNamespaces, ShowHelmReleases, ToggleKeepWatching,
    ToggleNamespace, TrustCluster, UnlockCluster, UntrustCluster, UseDefaultWatchMode,
    WatchAllKinds, WatchOpenViewsOnly,
};
use super::resource_actions;
use super::row_menu;
use super::scope_picker::{self, ScopePicker};
use super::startup::StartupView;
use super::terminal_model::ShellRequest;
use super::terminal_view::{RestartBlocked, TerminalView};
use super::theme;
use super::ui_state::{self, UiState};
use super::write_confirm;
use flint_core::display::{self, ClusterNames};
use flint_core::keep_watching::{BackgroundSignIn, KeepWatching};
use flint_core::settings::ClusterTag;

/// The height of the main area's header and the list's toolbar.
pub const HEADER_HEIGHT: f32 = 32.;

/// Below this main area width the header puts its access mode, namespace and watch controls on a
/// second row, in their short forms. Fixed, so the layout does not change with label lengths
/// while resizing: it holds the padding, Back, the longest labels of the three, and a shortened
/// context name.
const HEADER_ONE_ROW_MIN_WIDTH: f32 = 640.;

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

/// The smallest window the layout works at (width, height).
pub(in crate::ui) const MIN_WINDOW_SIZE: (f32, f32) = (760., 480.);

/// The narrowest list worth showing beside the detail pane.
const MIN_SPLIT_LIST_WIDTH: f32 = 260.;

/// Whether a window this wide is too narrow for the list and the detail side by side, with every
/// side pane at its minimum.
fn too_narrow_for_split(window_width: f32) -> bool {
    let side_panes: f32 = [Pane::Clusters, Pane::Navigation, Pane::Detail]
        .map(|pane| pane.limits().0)
        .iter()
        .sum();
    window_width < side_panes + MIN_SPLIT_LIST_WIDTH
}

/// What the main area shows of the list and the open detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainPanes {
    /// Side by side, or the list alone with no detail open.
    Split,
    /// A narrow window, on the detail.
    DetailAlone,
    /// A narrow window, gone Back to the list; the detail stays open behind it.
    ListAlone,
}

/// Pane widths are written to `state.toml` once a drag has paused this long.
const PANE_SAVE_DELAY: Duration = Duration::from_millis(400);

/// The most characters the namespace filter's button shows: a longer name ends in an ellipsis.
const NAMESPACE_LABEL_MAX_CHARS: usize = 18;

/// The key context entry holding a namespace row's index in the picker's list.
const NAMESPACE_ROW: &str = "namespace_row";

/// Padding around each row of the picker's scrolling list, which clips: the focus ring is painted
/// this far outside a checkbox. On the row, so scrolling a row into view brings its ring too.
const NAMESPACE_RING_ROOM: gpui_kit::Pixels = px(3.);

/// How focus comes back after a dialog closed the namespace picker.
#[derive(Clone, Copy)]
enum PickerDialogFocus {
    /// Nothing to do.
    Settled,
    /// The dialog is up; focus moves to the picker's button when it goes.
    DialogOpen,
    /// The dialog has gone and may still hand focus back to a picker control, now hidden.
    Restoring,
}

pub(super) use sets::SettingsDir;

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
    /// The generation of `cluster`'s slot the views are of: a new one (the context started over
    /// in the core, re-pointed or back after being removed) closes them, as a switch does, so
    /// nothing is restored from the old catalog (#255).
    generation: Option<u64>,
    list: Option<Entity<ResourceList>>,
    problem_feed: problems::ProblemFeed,
    detail: Option<Entity<DetailView>>,
    /// The active cluster's overview, in place of a list.
    dashboard: Option<(Entity<DashboardView>, Subscription)>,
    /// The sidebar's choice of clusters, saved sets and the running set's combined list.
    sets: sets::ClusterSets,
    /// The Settings view and its draft, which outlives leaving it.
    settings_page: super::settings_view::SettingsPage,
    /// The window is too narrow for the list and the detail side by side, as of the last draw.
    narrow: bool,
    /// The main header shows two rows, as of the last draw: the dock leaves it more room.
    two_row_header: bool,
    /// The active context's last move to another API server, for the main area's notice.
    server_moves: ServerMoves,
    /// In a narrow window, the list is in front of the open detail.
    list_in_front: bool,
    /// A right button went down since the last draw: the row it selects stays on show, as it
    /// hosts the context menu.
    right_press: bool,
    /// A right-click selected a row whose detail has not opened yet: the list stays in front
    /// when it does.
    menu_row_pending: bool,
    /// The detail pane, when it outlived its list to keep an edit.
    held_edit: Option<unapplied::HeldEdit>,
    /// What last had focus in the list or the detail with no dialog open, and the layout that
    /// hides it: a dialog gives focus back to it, though it may have been hidden meanwhile.
    pane_focus: Option<(WeakFocusHandle, MainPanes)>,
    /// The bottom dock. It does not change with the active cluster.
    dock: Entity<Dock>,
    /// The command palette (or the resource switcher), while it is open.
    palette: Option<(Entity<CommandPalette>, Subscription)>,
    /// Reads the objects in memory for the resource switcher.
    _switcher: Task<()>,
    /// What the open switcher was built from, to list anew when it changes.
    switcher_key: Option<keys::SwitcherKey>,
    /// The cluster sidebar's own focus, for its keys (UP-33).
    sidebar_focus: FocusHandle,
    /// Scrolls the cluster rows, so the keyboard cursor's row is brought into view.
    sidebar_scroll: ScrollHandle,
    /// The sidebar's keyboard cursor, once the arrows (or a click) moved it
    /// ([`Workspace::sidebar_cursor`]).
    sidebar_cursor: Option<String>,
    /// The places shown before on the active cluster, for Go Back.
    history: keys::History,
    /// Namespace names of the active cluster, for the namespace filter.
    namespaces: Vec<String>,
    /// Whether the user may list namespaces (`Failed` when not), for the namespace filter.
    namespaces_state: SyncState,
    /// Types a namespace into the filter (for users who may not list namespaces).
    namespace_input: Entity<InputState>,
    /// Whether the namespace filter's popover is open: held here, so opening the palette can
    /// close it wherever focus is.
    namespace_popover_open: bool,
    /// The namespace picker's content, which holds its keys.
    namespace_picker_focus: FocusHandle,
    /// The namespace filter's button, where focus returns when the picker closes.
    namespace_button_focus: FocusHandle,
    /// Where focus stands after a dialog closed the picker.
    namespace_focus_after_dialog: PickerDialogFocus,
    /// The picker's list of namespaces, scrolled to keep the focused row in view.
    namespace_list_scroll: ScrollHandle,
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
    /// The reason stays for all of [`ACTION_ERROR_SHOWN`]: it is about the cluster changing, so
    /// that change, and the list reopening after it, must not take it away.
    action_error_held: bool,
    _action_error: Task<()>,
    /// The review a resource action is waiting for.
    _write_review: Task<()>,
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
            this.follow_problem_connections(cx);
            cx.notify();
        });
        let nav = cx.new(NavPanel::new);
        // Enter on a kind in the navigation opens it, as a click on it does.
        let on_nav = cx.subscribe_in(&nav, window, |this, _, event, window, cx| match event {
            NavEvent::Open(kind) => {
                let open = OpenView {
                    kind: kind.to_string(),
                    ..Default::default()
                };
                this.open_view(&open, window, cx);
            }
            NavEvent::OpenOverview => {
                this.leave_settings_for_view();
                this.open_overview(window, cx)
            }
        });
        // Quitting right after a drag (or another change) still saves state.toml.
        let on_quit = cx.on_app_quit(|this: &mut Self, cx| {
            let save = this.ui_state.set_panes(this.panes);
            let pending = this.saving.take();
            let flush = super::quit_flush::QuitFlush::join(cx);
            async move {
                if let Some(pending) = pending {
                    pending.await;
                }
                if let Some(save) = save {
                    ui_state::save(save);
                }
                // A failed save logs a warning: it must reach the file before the process ends.
                flush.finish().await;
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
        // Refresh the watch states shared by the navigation and the cluster sidebar.
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    this.refresh_problems(cx);
                    let Some(cluster) = this.cluster.clone() else {
                        return;
                    };
                    // The elapsed time and the retry countdown.
                    if this.model.read(cx).can_cancel_connection() {
                        cx.notify();
                    }
                    let states = this.model.read(cx).core().sync_states(&cluster);
                    if this.nav.update(cx, |nav, cx| nav.set_states(states, cx)) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let dock = cx.new(Dock::new);
        let on_dock = cx.subscribe_in(&dock, window, |this, _, event, window, cx| match event {
            DockEvent::Blurred => this.recover_focus(window, cx),
            DockEvent::Restart(id) => this.restart_tab(*id, window, cx),
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        // Focus left with a closed view (a cluster switch drops the list): take it back, so keys
        // and registered actions still reach the workspace.
        let on_focus_lost =
            cx.on_focus_lost(window, |this, window, cx| this.recover_focus(window, cx));
        let guards = Self::settings_guards(Self::set_guards(Self::guards(cx.entity().downgrade())))
            .when::<OpenClusterOverview>(|ws, _| ws.cluster.is_some() || ws.sets.is_open());
        registry::install(focus.clone(), guards, cx);
        let cluster_sets = sets::ClusterSets::new(window, cx);
        // The window's close button asks first while a session would end, as Close Window does.
        closing::guard_close_button(window, cx);
        let mut this = Self {
            model,
            focus,
            forced_appearance,
            chrome: None,
            panes: ui_state.panes(),
            ui_state,
            nav,
            cluster: None,
            generation: None,
            list: None,
            problem_feed: Default::default(),
            detail: None,
            dashboard: None,
            sets: cluster_sets,
            settings_page: super::settings_view::SettingsPage::new(cx),
            narrow: false,
            two_row_header: false,
            server_moves: ServerMoves::default(),
            list_in_front: false,
            right_press: false,
            menu_row_pending: false,
            held_edit: None,
            pane_focus: None,
            dock,
            palette: None,
            _switcher: Task::ready(()),
            switcher_key: None,
            sidebar_focus: cx.focus_handle(),
            sidebar_cursor: None,
            sidebar_scroll: ScrollHandle::new(),
            history: keys::History::default(),
            namespaces: Vec::new(),
            namespaces_state: SyncState::Idle,
            namespace_input,
            namespace_popover_open: false,
            namespace_picker_focus: cx.focus_handle(),
            namespace_button_focus: cx.focus_handle(),
            namespace_focus_after_dialog: PickerDialogFocus::Settled,
            namespace_list_scroll: ScrollHandle::new(),
            startup_yaml: startup_view.as_ref().is_some_and(|v| v.yaml),
            startup_logs: startup_view.as_ref().is_some_and(|v| v.logs),
            action_error: None,
            action_error_held: false,
            _action_error: Task::ready(()),
            _write_review: Task::ready(()),
            startup_view,
            saving: None,
            _save_panes: Task::ready(()),
            _namespaces: Task::ready(()),
            _tick: tick,
            _list_subscriptions: Vec::new(),
            _detail_subscriptions: Vec::new(),
            _subscriptions: vec![
                // A write that starts or ends, in any window, changes what the menu offers.
                cx.observe_global::<resource_actions::RunningWrites>(|_, cx| cx.notify()),
                observe,
                appearance,
                on_namespace_input,
                on_quit,
                on_focus_lost,
                on_dock,
                on_nav,
            ],
        };
        this.show_appearance(window, cx);
        this.load_saved_sets(cx);
        if let Some(alerts) = cx.try_global::<super::alert_center::Alerts>() {
            let center = alerts.0.clone();
            this._subscriptions
                .push(cx.observe_in(&center, window, |this, _, window, cx| {
                    this.show_alert_overview(false, window, cx);
                }));
        }
        this
    }

    /// The sessions closing this window would end: the dock's shells and following log tabs.
    pub fn live_sessions(&self, cx: &App) -> LiveSessions {
        self.dock.read(cx).live_sessions(cx)
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
        dock::dock_height(self.panes.dock, window_height, self.two_row_header)
    }

    /// Sets the dock's height within its limits and saves it once changes pause. Dragging its
    /// edge and `SetDockHeight` both end here.
    fn resize_dock(&mut self, height: f32, window_height: f32, cx: &mut Context<Self>) {
        let shown = self.dock_height(window_height);
        let height = dock::dock_height(
            Some(height.max(0.).round() as u32),
            window_height,
            self.two_row_header,
        );
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
            let opening = if request.previous {
                Opening::PreviousLogs
            } else {
                Opening::Logs
            };
            self.refuse(refused(opening, &reason), cx);
            return;
        }
        let member = match self.session_member(&request.cluster, cx) {
            Ok(member) => member,
            Err(why) => {
                let opening = if request.previous {
                    Opening::PreviousLogs
                } else {
                    Opening::Logs
                };
                self.refuse(refused(opening, &why), cx);
                return;
            }
        };
        let core = self.session_core(member.as_ref(), cx);
        let key = TabKey::Logs(request.clone());
        let cluster = self.tab_cluster(&key, cx);
        let server = match &member {
            Some(member) => Some(member.server().to_owned()),
            None => self.shown_server(&request.cluster, cx),
        };
        let id = self.open_tab(
            key,
            cluster,
            |window, cx| DockTab::Logs(cx.new(|cx| LogView::at(core, request, server, window, cx))),
            focus,
            window,
            cx,
        );
        self.sets.keep_tab_pin(id, member);
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
    ) -> u64 {
        self.clear_action_error(cx);
        let kept = (!focus).then(|| window.focused(cx)).flatten();
        let in_dock = kept.is_some() && self.dock.read(cx).has_focus(window, cx);
        let id = self.dock.update(cx, |dock, cx| {
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
        id
    }

    /// Starts tab `id`'s ended session again (Reconnect Logs, Restart Shell) in a new tab beside
    /// it, keeping the ended one's output. A tab opened from a cluster set's row goes back only
    /// to the server it was admitted on, once the core confirms the context never left it.
    fn restart_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(member) = self.sets.tab_pin(id).cloned() else {
            self.restart_tab_on(id, None, window, cx);
            return;
        };
        let Some(key) = self.dock.read(cx).tab(id).map(|t| t.item(cx).tab_key()) else {
            return;
        };
        let opening = match &key {
            TabKey::Logs(_) => Opening::ReconnectLogs,
            TabKey::Shell(_) => Opening::RestartShell,
            TabKey::Activity(_) | TabKey::Compare(..) | TabKey::Forwards(_) | TabKey::Helm(_) => {
                return;
            }
        };
        let check = self.model.read(cx).core().check_member(&member);
        self.sets.restart = cx.spawn_in(window, async move |this, cx| {
            let changed = check.await;
            let _ = this.update_in(cx, |ws, window, cx| match changed {
                Some(why) => ws.refuse(refused(opening, &why), cx),
                None => ws.restart_tab_on(id, Some(member), window, cx),
            });
        });
    }

    /// [`Self::restart_tab`] once checked: on `member`'s server if it has one, else on the server
    /// its context points at now. Refused, saying why, while the cluster is not connected.
    fn restart_tab_on(
        &mut self,
        id: u64,
        member: Option<flint_core::handle::AdmittedMember>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.dock.read(cx).tab(id).map(|t| t.item(cx).tab_key()) else {
            return;
        };
        let opening = match &key {
            TabKey::Logs(_) => Opening::ReconnectLogs,
            TabKey::Shell(_) => Opening::RestartShell,
            // Runs no session to start again.
            TabKey::Activity(_) | TabKey::Compare(..) | TabKey::Forwards(_) | TabKey::Helm(_) => {
                return;
            }
        };
        if let Some(reason) = self.sessions_blocked(key.context(), cx) {
            self.refuse(refused(opening, &reason), cx);
            return;
        }
        if matches!(key, TabKey::Shell(_))
            && let Some(why) = self.writes_paused_why(key.context(), cx)
        {
            self.refuse(refused(opening, &why), cx);
            return;
        }
        if matches!(key, TabKey::Shell(_)) && self.is_protected(key.context(), cx) {
            let shown = self.cluster_names(cx).exact(key.context());
            self.refuse(refused(opening, &protected_reason(&shown)), cx);
            return;
        }
        self.clear_action_error(cx);
        let cluster = self.tab_cluster(&key, cx);
        let server = match &member {
            Some(member) => Some(member.server().to_owned()),
            None => self.shown_server(key.context(), cx),
        };
        let core = self.session_core(member.as_ref(), cx);
        let opened = self.dock.update(cx, |dock, cx| {
            dock.open_beside(
                id,
                cluster,
                move |window, cx| match key {
                    TabKey::Logs(request) => {
                        DockTab::Logs(cx.new(|cx| LogView::at(core, request, server, window, cx)))
                    }
                    TabKey::Shell(request) => {
                        let session =
                            core.exec(request.exec_request(Default::default()).at(server));
                        DockTab::Shell(cx.new(|cx| TerminalView::new(session, request, window, cx)))
                    }
                    // It returned above: an activity tab has no session to start again.
                    TabKey::Activity(_)
                    | TabKey::Compare(..)
                    | TabKey::Forwards(_)
                    | TabKey::Helm(_) => {
                        unreachable!("these tabs do not restart")
                    }
                },
                window,
                cx,
            )
        });
        if let Some(opened) = opened {
            self.sets.keep_tab_pin(opened, member);
        }
    }

    /// Says why Logs or Shell did not open, over the dock, for [`ACTION_ERROR_SHOWN`].
    pub(in crate::ui) fn refuse(&mut self, why: String, cx: &mut Context<Self>) {
        self.show_action_error(why, false, cx);
    }

    /// [`Self::refuse`] for a reason that is about the cluster changing: it outlasts that change.
    pub(in crate::ui) fn refuse_held(&mut self, why: String, cx: &mut Context<Self>) {
        self.show_action_error(why, true, cx);
    }

    fn show_action_error(&mut self, why: String, held: bool, cx: &mut Context<Self>) {
        tracing::warn!("{why}");
        self.action_error = Some(why);
        self.action_error_held = held;
        let timer = cx.background_executor().timer(ACTION_ERROR_SHOWN);
        self._action_error = cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |ws, cx| {
                ws.action_error_held = false;
                ws.clear_action_error(cx);
            });
        });
        cx.notify();
    }

    /// Takes the reason a Logs or Shell did not open away: it no longer applies.
    fn clear_action_error(&mut self, cx: &mut Context<Self>) {
        if self.action_error_held {
            return;
        }
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
            self.refuse(refused(Opening::Shell, &reason), cx);
            return;
        }
        // The core refuses too; this says so without opening a tab that fails at once.
        if self.is_protected(&request.cluster, cx) {
            let shown = self.cluster_names(cx).exact(&request.cluster);
            self.refuse(refused(Opening::Shell, &protected_reason(&shown)), cx);
            return;
        }
        let member = match self.session_member(&request.cluster, cx) {
            Ok(member) => member,
            Err(why) => {
                self.refuse(refused(Opening::Shell, &why), cx);
                return;
            }
        };
        if let Some(why) = self.writes_paused_why(&request.cluster, cx) {
            self.refuse(refused(Opening::Shell, &why), cx);
            return;
        }
        let core = self.session_core(member.as_ref(), cx);
        let key = TabKey::Shell(request.clone());
        let cluster = self.tab_cluster(&key, cx);
        let server = match &member {
            Some(member) => Some(member.server().to_owned()),
            None => self.shown_server(&request.cluster, cx),
        };
        let id = self.open_tab(
            key,
            cluster,
            |window, cx| {
                let session = core.exec(request.exec_request(Default::default()).at(server));
                DockTab::Shell(cx.new(|cx| TerminalView::new(session, request, window, cx)))
            },
            focus,
            window,
            cx,
        );
        self.sets.keep_tab_pin(id, member);
    }

    /// The cluster a session opened on `context` now runs on: the context and its server.
    fn tab_cluster(&self, key: &TabKey, cx: &App) -> TabCluster {
        let context = key.context();
        let summary = self.cluster_summary(context, cx);
        let definition = match key {
            // The core ends a shell on any change of the exact URL, and a log session only when
            // the endpoint changes. Agent activity depends on neither.
            TabKey::Shell(_) | TabKey::Helm(_) => summary.map_or(0, |c| c.definition),
            TabKey::Logs(_) => summary.map_or(0, |c| c.endpoint),
            TabKey::Activity(_) | TabKey::Compare(..) => 0,
            // Pinned like a shell: the core ends a forward on the same changes.
            TabKey::Forwards(_) => summary.map_or(0, |c| c.definition),
        };
        TabCluster::new(context, self.shown_server(context, cx).as_deref()).defined_by(definition)
    }

    /// The server `context` shows now, as the cluster list has it: what a tab opened on it is
    /// labelled with, and what its log session is pinned to.
    fn shown_server(&self, context: &str, cx: &App) -> Option<String> {
        self.cluster_summary(context, cx)
            .and_then(|c| c.info.server.clone())
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
        // By server only: a context that started over at the same server (a namespace edit, a
        // rotated token) is the same cluster to a shell. One re-pointed and put back between
        // reads is not reused (`TabCluster`), and is ended by the core, which checks each shell's
        // pin against the context's history. A server the core has not confirmed may be a
        // fallback while a kubeconfig file can't be read: its shells are left running.
        let servers = confirmed_servers(&self.model.read(cx).clusters);
        self.dock.update(cx, |dock, cx| {
            dock.check_servers(|context| servers.get(context).cloned(), cx)
        });
    }

    /// Tells the detail pane whether sessions can open on its object's cluster, so its Logs and
    /// Shell are disabled, saying why, while the cluster is not connected.
    pub(super) fn tell_detail_sessions_blocked(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.detail.clone() else {
            return;
        };
        let cluster = detail.read(cx).object().cluster.clone();
        let why = self.sessions_blocked(&cluster, cx);
        let protected = self.is_protected(&cluster, cx);
        let writes_paused = self.writes_paused_why(&cluster, cx);
        let label = self.cluster_names(cx).exact(&cluster);
        // A set's row stays on its admitted server, which the write gate then holds it to.
        let server = match self.sets.member(&cluster) {
            Some(member) => Some(member.server().to_owned()),
            None => self
                .cluster_summary(&cluster, cx)
                .and_then(|c| c.info.server.clone()),
        };
        // `None` for a context that is gone; an unconfirmed one says nothing yet.
        let endpoint = self
            .cluster_summary(&cluster, cx)
            .map(|c| c.confirmed.then_some(c.endpoint));
        let (connection, unsettled) =
            self.cluster_summary(&cluster, cx)
                .map_or((None, false), |c| {
                    (
                        Some((c.generation, c.definition)),
                        c.provisional || c.paused,
                    )
                });
        detail.update(cx, |d, cx| {
            d.set_sessions_blocked(why, cx);
            d.set_protected(protected, label, cx);
            d.set_row_writes_paused(writes_paused, cx);
            d.set_server(server);
            match endpoint {
                Some(Some(endpoint)) => d.note_endpoint(endpoint, cx),
                Some(None) => {}
                None => d.note_context_gone(cx),
            }
            d.set_access_connection(connection, unsettled, cx);
        });
    }

    /// Confirms a reviewed edit from the detail pane, naming its cluster, and runs it. `reviewed_on`
    /// guards the connection the review started on: the confirmation is bound to that, not to the
    /// one it opens on.
    fn confirm_detail_write(
        &mut self,
        review: &flint_core::write::WriteReview,
        reviewed_on: Option<super::write_binding::ConnectionGuard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        // The handle it was reviewed through: a set's row runs bound to its member too.
        let core = detail.read(cx).write_core().clone();
        let detail = detail.downgrade();
        let names = self.cluster_names(cx);
        let handle = window.window_handle();
        // The same claim as the row actions: one write at a time per object.
        let key = resource_actions::WriteKey::of(review.target());
        // Only for the server the review named, on the connection the review started on:
        // otherwise it is already about another connection.
        let guard = self
            .cluster_summary(&review.target().cluster, cx)
            .filter(|c| c.info.server == review.target().server)
            .and(reviewed_on)
            .filter(|guard| guard.holds(cx));
        let Some(guard) = guard else {
            self.refuse(super::write_binding::connection_changed_text(), cx);
            return;
        };
        let begin = {
            let (key, detail) = (key.clone(), detail.clone());
            // A context moved under the open confirmation stays refused even if it came back.
            move |cx: &mut App| match match detail
                .read_with(cx, |d, _| d.edit_moved_refusal())
                .ok()
                .flatten()
            {
                Some(why) => Err(why),
                None => resource_actions::claim(&key, resource_actions::RunningWrite::YamlEdit, cx),
            } {
                Ok(()) => {
                    let _ = detail.update(cx, |d, cx| d.edit_applying(cx));
                    true
                }
                Err(why) => {
                    let detail = detail.clone();
                    // This runs inside the dialog's update of this window, which cannot be
                    // updated again until that ends.
                    cx.defer(move |cx| {
                        let _ = handle.update(cx, |_, window, cx| {
                            let _ = detail.update(cx, |d, cx| {
                                d.edit_applied(
                                    Err(flint_core::write::WriteError::Failed(why)),
                                    window,
                                    cx,
                                )
                            });
                        });
                    });
                    false
                }
            }
        };
        write_confirm::open_noted(
            window,
            cx,
            core,
            review.clone(),
            &names,
            None,
            guard,
            begin,
            move |result, cx| {
                // First, and outside the window: a closed one must not keep the object claimed.
                resource_actions::release(&key, cx);
                let detail = detail.clone();
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = detail.update(cx, |d, cx| d.edit_applied(result, window, cx));
                });
            },
        );
    }

    /// Tells each shell tab why its Restart shell cannot run, if it cannot: its context is
    /// gone or has no server, or its cluster is protected, when Unlock… is offered beside it
    /// (UP-54).
    fn tell_shells_protected(&mut self, cx: &mut Context<Self>) {
        let shells: Vec<_> = self.dock.read(cx).terminals().cloned().collect();
        for shell in shells {
            let cluster = shell.read(cx).tab_key().context().to_owned();
            let blocked = self.restart_blocked(&cluster, cx);
            shell.update(cx, |s, cx| s.set_restart_blocked(blocked, cx));
        }
        let logs: Vec<_> = self.dock.read(cx).log_views().cloned().collect();
        for log in logs {
            let cluster = log.read(cx).tab_key().context().to_owned();
            let blocked = self.sessions_blocked(&cluster, cx);
            log.update(cx, |l, cx| l.set_reconnect_blocked(blocked, cx));
        }
    }

    /// Why a shell on `cluster` cannot start again, as far as the context and its protection
    /// say.
    fn restart_blocked(&self, cluster: &str, cx: &App) -> Option<RestartBlocked> {
        let blocked = |why: String, unlock| Some(RestartBlocked { why, unlock });
        match self.cluster_summary(cluster, cx) {
            None => blocked(format!("no kubeconfig context named {cluster}"), false),
            Some(c) if c.info.server.is_none() => {
                blocked(format!("{cluster} has no API server in kubeconfig"), false)
            }
            Some(c) if self.model.read(cx).writes_paused(c) => blocked(
                writes_paused_reason(&self.cluster_names(cx).exact(cluster)),
                false,
            ),
            Some(_) if self.is_protected(cluster, cx) => blocked(
                protected_reason(&self.cluster_names(cx).exact(cluster)),
                true,
            ),
            Some(_) => None,
        }
    }

    /// Brings the navigation and main area in line with the model: the active cluster and its
    /// discovery.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if confirm::newest_is_stale(window, cx) {
            let this = cx.weak_entity();
            window.defer(cx, move |window, cx| {
                if confirm::close_stale(window, cx) {
                    let _ = this.update(cx, |ws, cx| ws.confirmation_closed(window, cx));
                }
            });
        }
        self.check_dock_servers(cx);
        self.tell_detail_sessions_blocked(cx);
        self.tell_shells_protected(cx);
        self.check_set_row(cx);
        let model = self.model.read(cx);
        let active = model.active.clone();
        let core = model.core().clone();
        let failed = model
            .active_cluster()
            .is_some_and(|c| matches!(c.state, ConnectionState::Failed(_)));
        let waiting = model
            .active_cluster()
            .is_some_and(|c| c.state == ConnectionState::AwaitingConfirmation);
        let discovered = active
            .as_ref()
            .and_then(|c| model.discovered(c))
            .cloned()
            .unwrap_or_default();
        match model.active_cluster() {
            Some(c) => self.server_moves.observe(
                &c.info.name,
                c.endpoint,
                c.confirmed,
                c.info.server.as_deref(),
            ),
            None => self.server_moves.clear(),
        }
        let generation = model.active_cluster().map(|c| c.generation);
        let cluster_changed = active != self.cluster || generation != self.generation;
        if cluster_changed {
            self.generation = generation;
            // Switching clusters closes the old cluster's views; dropping the list drops its
            // subscription (UP-24).
            let previous_cluster = std::mem::replace(&mut self.cluster, active.clone());
            self.forget_places();
            // Selecting a member (to sign in to it, say) keeps the set's list open.
            // So does disconnecting the member that was selected.
            let member_left = active.is_none()
                && previous_cluster
                    .as_deref()
                    .is_some_and(|c| self.sets.member_disconnected(c));
            if !self.sets.has_member(active.as_deref()) && !member_left {
                self.close_list(cx);
            }
            // Its filter or YAML, if one had focus that has moved on (to a dock tab, say): focus
            // may never come back through `reclaim_focus`, nor a list open here. One that still
            // has focus goes when the workspace takes focus back.
            release_unfocused_input(window, cx);
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
            nav.set_awaiting_confirmation(waiting, cx);
            nav.set_cluster(
                active.clone(),
                discovered.discovery.clone(),
                discovered.error.clone(),
                cx,
            );
            if cluster_changed {
                nav.set_states(Vec::new(), cx);
            }
        });
        // The overview first: opening it closes a set, whose held members then need no feed.
        self.show_alert_overview(cluster_changed, window, cx);
        self.refresh_set_kinds(cx);
        // An open switcher follows the active cluster and its discovery.
        self.refresh_switcher(cx);
        let (Some(cluster), Some(discovery)) = (active, discovered.discovery) else {
            if discovered.error.is_some() {
                self.open_overview_without_discovery(window, cx);
            }
            return;
        };
        if self.list.is_some() || self.dashboard.is_some() || self.sets.is_open() {
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
                self.restore_kind(&entry, window, cx);
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
            self.restore_kind(&entry, window, cx);
        }
        // A saved kind in a group discovery could not vouch for may come with its next retry.
        let saved_may_come = self
            .ui_state
            .last_view(&cluster)
            .is_some_and(|kind| may_still_be_discovered(&discovery, kind));
        if self.list.is_none() && !saved_may_come {
            self.open_default_overview(window, cx);
        }
    }

    /// Discovery failed with nothing asked for, so there are no kinds to pick: the overview
    /// needs none. An explicit or saved kind waits for discovery to work instead.
    fn open_overview_without_discovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.startup_view.is_some()
            || self
                .cluster
                .as_deref()
                .is_some_and(|cluster| self.ui_state.last_view(cluster).is_some());
        if !pending && self.list.is_none() && self.dashboard.is_none() && !self.sets.is_open() {
            self.open_default_overview(window, cx);
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
        self.leave_set(cx);
        self.list = None;
        self.drop_pane_unless_edited(cx);
        self._list_subscriptions.clear();
        if self.dashboard.take().is_some() {
            self.nav.update(cx, |nav, cx| nav.set_overview(false, cx));
        }
    }

    #[cfg(test)]
    pub(in crate::ui) fn dashboard_view(&self) -> Option<Entity<DashboardView>> {
        self.dashboard.as_ref().map(|(view, _)| view.clone())
    }

    #[cfg(test)]
    pub(in crate::ui) fn save_last_view_for_test(&mut self, cluster: &str, kind: &str) {
        let _ = self.ui_state.set_last_view(cluster, kind);
    }

    #[cfg(test)]
    pub(in crate::ui) fn sidebar_focus_for_test(&self) -> FocusHandle {
        self.sidebar_focus.clone()
    }

    #[cfg(test)]
    pub(in crate::ui) fn saved_namespace_filter(&self, cluster: &str) -> Vec<String> {
        self.ui_state.namespace_filter(cluster).to_vec()
    }

    /// Shows the overview an opened alert asked for, once this is its cluster. A move to another
    /// cluster first drops the request.
    fn show_alert_overview(&mut self, moved: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(center) = cx
            .try_global::<super::alert_center::Alerts>()
            .map(|alerts| alerts.0.clone())
        else {
            return;
        };
        let Some(wanted) = center.read(cx).wanted_overview().cloned() else {
            return;
        };
        let here = self.cluster.as_deref() == Some(wanted.context.as_str())
            && self.generation == Some(wanted.generation);
        if here || moved {
            center.update(cx, |center, _| center.clear_overview());
        }
        if here {
            self.open_dashboard(window, cx);
        }
    }

    /// Opens a connected cluster on its overview when nothing else was asked for. A paused
    /// cluster stays as it is, so no feed starts, and a set being admitted is not cancelled.
    fn open_default_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let ready = model.active_cluster().is_some_and(|cluster| {
            matches!(cluster.state, ConnectionState::Connected(_))
                && !model.sessions_paused(cluster)
        });
        if ready && !self.sets.is_opening() {
            // Connecting from the sidebar leaves focus there, so its arrows keep working.
            let take_focus =
                self.palette.is_none() && !self.sidebar_focus.contains_focused(window, cx);
            self.show_dashboard(take_focus, window, cx);
        }
    }

    /// Shows the active cluster's overview in place of its list. Go Back returns to the list.
    fn open_dashboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_dashboard(true, window, cx);
    }

    fn show_dashboard(&mut self, take_focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(cluster), Some(generation)) = (self.cluster.clone(), self.generation) else {
            return;
        };
        if let Some((view, _)) = &self.dashboard {
            if take_focus {
                let focus = view.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            return;
        }
        if self.asks_before_leaving(window, cx, |ws, window, cx| ws.open_dashboard(window, cx)) {
            return;
        }
        // Any place other than the list shown: the overview is not one Go Back returns to.
        let overview = keys::Place {
            kind: KindId::new("flint.overview", "overview"),
            object: None,
            seen: None,
        };
        self.remember_place(&overview, cx);
        self.close_list(cx);
        let model = self.model.clone();
        let view = cx.new(|cx| DashboardView::following(model, cluster, generation, cx));
        let events = cx.subscribe_in(&view, window, Self::on_dashboard_event);
        if take_focus {
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        release_unfocused_input(window, cx);
        self.dashboard = Some((view, events));
        self.nav.update(cx, |nav, cx| nav.set_overview(true, cx));
        cx.notify();
    }

    /// A problem tile opens its kind's list, narrowed to the objects it counted.
    fn on_dashboard_event(
        &mut self,
        view: &Entity<DashboardView>,
        event: &DashboardEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let problems = match event {
            DashboardEvent::OpenFilteredList(category) => view.read(cx).problem_list(*category),
            DashboardEvent::OpenShortWorkloads(kind) => view.read(cx).short_workloads(kind),
            DashboardEvent::CannotOpen(why) => {
                self.refuse(why.clone(), cx);
                return;
            }
        };
        let Some(problems) = problems.cloned() else {
            return;
        };
        let Some(entry) = self.nav_entry(problems.kind, cx) else {
            self.refuse(
                format!("This cluster does not list {}.", problems.filter.label),
                cx,
            );
            return;
        };
        self.open_kind(&entry, window, cx);
        if let Some(list) = &self.list {
            list.update(cx, |list, cx| list.show_only(Some(problems.filter), cx));
        }
    }

    /// Opens `entry`'s list at the user's asking, in front of any detail in a narrow window.
    fn open_kind(&mut self, entry: &NavEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.list_in_front = true;
        self.show_kind(entry, true, true, window, cx);
        cx.notify();
    }

    /// Opens `entry`'s list on its own (the view last open on the cluster, `FLINT_VIEW`), not
    /// at the user's asking: it takes focus only if the palette, the switcher or a dialog is not
    /// open, so they stay open with what was typed and keep their keys.
    fn restore_kind(&mut self, entry: &NavEntry, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = gpui_kit::component::WindowExt::has_active_dialog(window, cx);
        let take_focus = self.palette.is_none() && !dialog;
        self.show_kind(entry, true, take_focus, window, cx);
    }

    /// Opens `entry`'s list (unless it is open), with focus when `take_focus`. With `remember`, Go Back can
    /// return to the list and object shown before.
    fn show_kind(
        &mut self,
        entry: &NavEntry,
        remember: bool,
        take_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.show_kind_in_set(entry, window, cx) {
            return;
        }
        let Some(cluster) = self.cluster.clone() else {
            return;
        };
        if let Some(list) = self
            .list
            .as_ref()
            .filter(|l| *l.read(cx).kind() == entry.id)
        {
            // Open already: it takes focus, as a new one would (Enter on it in the
            // navigation).
            if take_focus {
                let focus = list.read(cx).focus_handle().clone();
                window.focus(&focus, cx);
                release_unfocused_input(window, cx);
                // Asking for the kind again shows all of it.
                list.update(cx, |list, cx| list.show_only(None, cx));
            }
            return;
        }
        if remember {
            let next = keys::Place {
                kind: entry.id.clone(),
                object: None,
                seen: None,
            };
            self.remember_place(&next, cx);
        }
        self.close_list(cx);
        let core = self.model.read(cx).core().clone();
        let namespaces = self.ui_state.namespace_filter(&cluster).to_vec();
        let sort = self
            .ui_state
            .list_sort(&cluster, entry.id.as_str())
            .cloned();
        let list = cx.new(|cx| {
            ResourceList::new(core, cluster.clone(), entry, namespaces, sort, window, cx)
        });
        // Every kind: any object can be compared, and a menu with no entries never shows.
        let menu = row_menu::row_menu(cx.weak_entity());
        list.update(cx, |list, _| list.set_row_menu(menu));
        self._list_subscriptions = vec![cx.subscribe_in(&list, window, Self::on_list_event)];
        if take_focus {
            let focus = list.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
        }
        // The closed list's filter, if it had focus.
        release_unfocused_input(window, cx);
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
                if self.unapplied_edit(cx).is_some() {
                    if list.update(cx, |list, cx| list.undo_selection(cx)) {
                        let list = list.clone();
                        self.asks_before_leaving(window, cx, move |_, _, cx| {
                            list.update(cx, |list, cx| list.clear_selection(cx));
                        });
                    }
                    return;
                }
                self.menu_row_pending = false;
                self.clear_action_error(cx);
                self.detail = None;
                self._detail_subscriptions.clear();
                let focus = list.read(cx).focus_handle().clone();
                window.focus(&focus, cx);
                // The closed pane's YAML, if it had focus.
                release_unfocused_input(window, cx);
            }
            ListEvent::Selected(Some(selected)) => {
                let rows = list.read(cx);
                let object = ObjectRef {
                    cluster: rows.cluster().to_owned(),
                    kind: rows.kind().clone(),
                    kind_label: rows.label().to_owned(),
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
                if self.unapplied_edit(cx).is_some() {
                    if list.update(cx, |list, cx| list.undo_selection(cx)) {
                        let (list, wanted) = (list.clone(), selected.clone());
                        self.asks_before_leaving(window, cx, move |_, window, cx| {
                            list.update(cx, |list, cx| list.reselect(&wanted, window, cx));
                        });
                    }
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
                let guards = super::write_binding::ConnectionGuards::new(&self.model);
                let detail = cx.new(|cx| {
                    let mut detail = DetailView::new(core, object, tab, window, cx);
                    detail.set_connection_guards(guards);
                    if startup_logs {
                        detail.open_logs_when_loaded(cx);
                    }
                    detail
                });
                self._detail_subscriptions =
                    vec![
                        cx.subscribe_in(&detail, window, move |this, _, event, window, cx| {
                            match event {
                                DetailEvent::Close => this.close_detail(window, cx),
                                DetailEvent::OpenLogs(request, focus) => {
                                    let focus = takes_focus(focus, window, cx);
                                    this.open_logs(request.clone(), focus, window, cx)
                                }
                                DetailEvent::OpenShell(request, focus) => {
                                    let focus = takes_focus(focus, window, cx);
                                    this.open_shell(request.clone(), focus, window, cx)
                                }
                                DetailEvent::Refused(why) => this.refuse(why.clone(), cx),
                                DetailEvent::ConfirmWrite(review, reviewed_on) => this
                                    .confirm_detail_write(review, reviewed_on.clone(), window, cx),
                            }
                        }),
                    ];
                self.detail = Some(detail);
                // The replaced pane's YAML, if it had focus and focus has moved on.
                release_unfocused_input(window, cx);
                self.tell_detail_sessions_blocked(cx);
            }
            ListEvent::SortChanged(sort) => {
                let list = list.read(cx);
                let save =
                    self.ui_state
                        .set_list_sort(list.cluster(), list.kind().as_str(), sort.clone());
                self.persist(save, cx);
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
        traffic_lights: Pixels,
        theme: &Theme,
        focused: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let model = self.model.read(cx);
        let active = model.active.clone();

        // Scrolls by itself, so the keyboard cursor can bring a cluster row into view
        // (`Workspace::move_sidebar_cursor`): its rows are its children.
        let mut list = v_flex()
            .id("cluster-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.sidebar_scroll)
            .gap_px()
            .px_1p5()
            .py_1();
        if let Some(error) = &model.settings_error {
            list = list.child(sidebar_error(
                "settings-error",
                "settings.toml is invalid. Until it is fixed, nothing is exposed to AI or trusted, and clusters on this machine are protected too.",
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
        // The keyboard cursor shows only while the sidebar has focus.
        let cursor = focused.then(|| self.sidebar_cursor(cx)).flatten();
        let names = model.names();
        let states = self.nav.read(cx).states();
        for cluster in &model.clusters {
            let is_active = active.as_deref() == Some(cluster.info.name.as_str());
            let presence = Presence::of(&cluster.state, is_active, states);
            let exposure = (model.ai_exposure(cluster), model.ai_scope(cluster));
            let settings_ok = model.settings_error.is_none();
            let protection = (
                model.protection(cluster),
                model.trust(cluster),
                model.writes_paused(cluster),
            );
            let ring = cursor.as_deref() == Some(cluster.info.name.as_str());
            let keep = model
                .keep_watching(&cluster.info.name)
                .map_or((KeepWatching::Off, BackgroundSignIn::NotNeeded), |view| {
                    (view.status, view.sign_in)
                });
            list = list.child(self.render_cluster_row(
                cluster,
                (
                    is_active,
                    ring,
                    presence,
                    cluster_details(model, cluster, states),
                ),
                exposure,
                protection,
                (
                    settings_ok,
                    &names,
                    model.cluster_tag(cluster),
                    keep,
                    self.can_disconnect(&cluster.info.name, cx),
                ),
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

        let sidebar = v_flex()
            .id("cluster-sidebar")
            .key_context(registry::SIDEBAR)
            .track_focus(&self.sidebar_focus);
        Self::bind_sidebar_actions(OnView::new(sidebar, cx))
            .into_element()
            .w(px(widths.clusters))
            .h_full()
            .flex_none()
            .relative()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(pane_title("CLUSTERS", traffic_lights, theme))
            .child(list)
            .child(self.render_sets_panel(theme, cx))
            .child(self.render_settings_entry(theme))
            .child(pane_handle(
                "clusters-edge",
                Pane::Clusters,
                HandleEdge::Trailing,
            ))
    }

    fn render_cluster_row(
        &self,
        cluster: &ClusterSummary,
        // Whether it is the active cluster, whether the sidebar's cursor is on it, its presence
        // and its full description.
        (is_active, ring, presence, tooltip): (bool, bool, Presence<'_>, String),
        (exposure, ai_scope): (AiExposureState, Option<AiScope>),
        (protection, trust, writes_paused): (Protection, TrustState, bool),
        (settings_ok, names, tag, (keep, keep_sign_in), can_disconnect): (
            bool,
            &ClusterNames,
            Option<ClusterTag>,
            (KeepWatching, BackgroundSignIn),
            bool,
        ),
        theme: &Theme,
    ) -> AnyElement {
        let name = cluster.info.name.clone();
        let server = cluster.info.server.clone();
        let title = names.of(&name);
        // With a custom name, it starts with the real one: the row always says which cluster.
        let host = cluster.info.server.as_deref().map(shown_host);
        let subtitle = names.subtitle(&name, host.as_deref());
        let changed = matches!(exposure, AiExposureState::ServerChanged { .. });
        let dot = presence
            .marker(theme)
            .id(SharedString::from(format!("cluster-dot-{name}")))
            .flex_none()
            .test_support();

        let tip = (name.clone(), self.model.clone(), self.nav.clone());
        let rename = name.clone();
        let control_title = control_name(&title, &name);
        let row = h_flex()
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
            // The keyboard cursor, while the sidebar has focus.
            // Every row has the border, so the ring moving shifts nothing.
            .border_1()
            .border_color(super::theme::cursor_ring(theme, ring))
            .tooltip(move |_, cx| cluster_tooltip(tip.0.clone(), &tip.1, &tip.2, cx))
            // While choosing clusters a click chooses: it must not connect to this one.
            // So does a Cmd-click, which starts choosing.
            .on_click({
                let choose = registry::on_click(registry::TogglePickedCluster {
                    cluster: name.clone(),
                });
                let select = registry::on_click(SelectCluster {
                    cluster: name.clone(),
                });
                let picking = self.sets.picking();
                move |event: &ClickEvent, window, cx| {
                    if sets::click_chooses(picking, event.modifiers(), cx) {
                        choose(event, window, cx);
                    } else {
                        select(event, window, cx);
                    }
                }
            })
            .children(self.render_pick_box(&name))
            .child(dot)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .child(
                                div()
                                    .min_w_0()
                                    .text_sm()
                                    .line_height(px(18.))
                                    .truncate()
                                    .child(title),
                            )
                            .when_some(tag, |line, tag| {
                                line.child(cluster_tag::tag_dot(&name, tag, theme))
                            }),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("cluster-server-{name}")))
                            .aria_label(subtitle.clone())
                            .text_xs()
                            .line_height(px(14.))
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(subtitle)
                            .test_support(),
                    )
                    .when_some(keep_watching::badge(keep), |col, badge| {
                        col.child(
                            div()
                                .id(SharedString::from(format!("cluster-watching-{name}")))
                                .aria_label(badge)
                                .pt_0p5()
                                .flex()
                                .child(if keep == KeepWatching::Watching {
                                    Tag::secondary().outline().xsmall().child(badge)
                                } else {
                                    Tag::warning().outline().xsmall().child(badge)
                                })
                                .tooltip(move |window, cx| {
                                    Tooltip::new(keep_watching::toggle_tip(keep)).build(window, cx)
                                })
                                .test_support(),
                        )
                    })
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
            // Neutral: the toggle next to it already shows the exposure in the accent. "AI·" when
            // agents may read only part of the cluster; a click shows what agents asked for
            // (UP-16), with Edit AI Scope there.
            .when_some(ai_scope.filter(|_| exposure.is_exposed()), |row, scope| {
                let label = if scope.is_everything() { "AI" } else { "AI·" };
                let tip = format!(
                    "AI agents may read {}. Click to see what they asked for.",
                    scope_picker::summary(&scope)
                );
                row.child(
                    div()
                        .id(SharedString::from(format!("ai-scope-{name}")))
                        .flex_none()
                        .cursor_pointer()
                        .aria_label(label)
                        .aria_description(tip.clone())
                        .child(Tag::secondary().outline().xsmall().child(label))
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                        .on_click(registry::on_click_contained(ShowAgentActivity {
                            cluster: name.clone(),
                        }))
                        .test_support(),
                )
            })
            .children(render_lock_button(
                &name,
                &control_title,
                server.is_some(),
                protection,
                &trust,
                settings_ok,
                writes_paused,
            ))
            .child(self.render_exposure_toggle(
                &name,
                &control_title,
                server,
                exposure,
                settings_ok,
            ));
        let row = row.test_support();
        let row = div()
            .id(SharedString::from(format!("cluster-menu-{name}")))
            .w_full()
            .child(row);
        if !settings_ok {
            return row.into_any_element();
        }
        let (has_server, endpoint) = (cluster.info.server.is_some(), cluster.endpoint);
        row.context_menu(move |menu, window, cx| {
            let menu = menu.item(PopupMenuItem::new("Rename…").on_click(registry::on_click(
                RenameCluster {
                    cluster: rename.clone(),
                },
            )));
            if !has_server {
                return menu;
            }
            let menu = menu.item(PopupMenuItem::new("Helm Releases…").on_click(
                registry::on_click(ShowHelmReleases {
                    cluster: rename.clone(),
                }),
            ));
            let menu = menu.item(
                PopupMenuItem::new(keep_watching::toggle_label(keep, keep_sign_in)).on_click(
                    registry::on_click(ToggleKeepWatching {
                        cluster: rename.clone(),
                    }),
                ),
            );
            let menu = menu.when(can_disconnect, |menu| {
                menu.item(
                    PopupMenuItem::new("Disconnect").on_click(registry::on_click(
                        DisconnectCluster {
                            cluster: rename.clone(),
                        },
                    )),
                )
            });
            cluster_tag::tag_submenu(menu, rename.clone(), endpoint, tag, window, cx)
        })
        .into_any_element()
    }

    /// The colour strip over the detail pane, from the tag of the cluster its object is in.
    fn detail_tag_strip(&self, theme: &Theme, cx: &App) -> Option<impl IntoElement> {
        let detail = self.detail.as_ref()?.read(cx);
        let summary = self.cluster_summary(&detail.object().cluster, cx)?;
        let tag = self.model.read(cx).cluster_tag(summary)?;
        Some(cluster_tag::tag_strip(tag, theme))
    }

    /// The Expose to AI button. Exposing asks for confirmation naming the context and server;
    /// hiding does not. It is not a tab stop, so a stray Space or Enter cannot flip it.
    fn render_exposure_toggle(
        &self,
        name: &str,
        title: &str,
        server: Option<String>,
        exposure: AiExposureState,
        settings_ok: bool,
    ) -> AnyElement {
        let cluster = name.to_owned();
        let button = Button::new(SharedString::from(format!("expose-{name}")))
            .icon(IconName::Bot)
            .accessibility_label(exposure_label(title, &exposure))
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
            .child(pane_title("RESOURCES", px(0.), theme))
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
        window_width: f32,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let name = cluster.info.name.clone();
        let filter = self.ui_state.namespace_filter(&name).to_vec();
        let list = self.list.as_ref().map(|l| l.read(cx));
        let controls = header_controls(cluster, list);
        let ns_summary = namespace_button_text(&filter, list.and_then(|l| l.limited_to()));
        // Only a namespace's own name can be long; the limited text must stay whole.
        let ns_label = match filter.as_slice() {
            [one] if *one == ns_summary => ellipsize(&ns_summary, NAMESPACE_LABEL_MAX_CHARS),
            _ => ns_summary.clone(),
        };

        let mut crumbs: Vec<(SharedString, String)> = vec![(
            self.model.read(cx).names().of(&name).into(),
            format!("{name}\n{}", server_text(cluster.info.server.as_deref())),
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
        // Clipped: a cluster filling the row leaves no room for the chevrons after it.
        let mut breadcrumb = h_flex()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .gap_1()
            .text_sm();
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
                    .aria_label(text.clone())
                    .test_support()
                    .min_w_0()
                    .truncate()
                    // The other crumbs give way first: the cluster is cut only when it alone
                    // is wider than the breadcrumb.
                    .when(i == 0, |el| el.flex_none().max_w_full())
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

        let detail_alone = self.main_panes(cx) == MainPanes::DetailAlone;
        let two_rows = self.header_has_two_rows(window_width, cx);
        let namespaces = controls
            .namespaces
            .then(|| self.render_namespace_filter(&ns_summary, ns_label, cx));
        let watch_mode = controls
            .watch_mode
            .then(|| self.render_watch_mode(&name, two_rows, cx));
        let keep_watching = self
            .dashboard
            .is_some()
            .then(|| self.render_keep_watching(&name, None, cx));
        let alerts = super::alert_center::render_button(cx);
        let first_row = super::theme::moves_window(h_flex())
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_3()
            .gap_2()
            .when(detail_alone, |el| {
                el.child(
                    div().flex_none().child(
                        Button::new("detail-back")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronLeft)
                            .label("Back")
                            .tooltip_with_action(
                                registry::label::<BackToList>(),
                                &BackToList,
                                Some(registry::WORKSPACE),
                            )
                            .on_click(registry::on_click(BackToList)),
                    ),
                )
            })
            .child(breadcrumb);
        let access_mode = self.render_access_mode(cluster, two_rows, theme, cx);
        if two_rows {
            v_flex()
                .id("main-header")
                .w_full()
                .flex_none()
                .border_b_1()
                .border_color(theme.border)
                .child(first_row.w_full())
                .child(
                    h_flex()
                        .id("header-controls-row")
                        .w_full()
                        .h(px(HEADER_HEIGHT))
                        .flex_none()
                        .px_3()
                        .gap_2()
                        .justify_end()
                        .child(access_mode)
                        .children(namespaces)
                        .children(keep_watching)
                        .children(watch_mode)
                        .children(alerts)
                        .test_support(),
                )
                .test_support()
                .into_any_element()
        } else {
            first_row
                .border_b_1()
                .border_color(theme.border)
                .child(access_mode)
                .children(namespaces)
                .children(keep_watching)
                .children(watch_mode)
                .children(alerts)
                .into_any_element()
        }
    }

    /// The overview's keep-watching toggle, saying what it would do and any state to act on.
    /// `member` names a set member's copy of it, which needs ids of its own.
    pub(super) fn render_keep_watching(
        &self,
        cluster: &str,
        member: Option<&str>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let model = self.model.read(cx);
        let (status, sign_in) = model
            .keep_watching(cluster)
            .map_or((KeepWatching::Off, BackgroundSignIn::NotNeeded), |view| {
                (view.status, view.sign_in)
            });
        let settings_ok = model.settings_error.is_none();
        Button::new(member_id("keep-watching", member))
            .xsmall()
            .ghost()
            .label(keep_watching::toggle_label(status, sign_in))
            .tooltip(keep_watching::toggle_tip(status))
            .disabled(!settings_ok)
            .on_click(registry::on_click(ToggleKeepWatching {
                cluster: cluster.to_owned(),
            }))
    }

    /// The overview's toggle for running this cluster's sign-in command in the background, naming
    /// the command. None for a credential that runs nothing.
    fn render_background_sign_in(
        &self,
        cluster: &str,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        self.dashboard.as_ref()?;
        self.background_sign_in_row(cluster, None, None, theme, cx)
    }

    /// The background sign-in toggle of `cluster` and the command it names, after `lead`.
    /// `member` names a set member's copy of it, which needs ids of its own. None for a
    /// credential that runs nothing.
    pub(super) fn background_sign_in_row(
        &self,
        cluster: &str,
        member: Option<&str>,
        lead: Option<AnyElement>,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.model.read(cx).keep_watching(cluster)?.clone();
        if view.sign_in == BackgroundSignIn::NotNeeded {
            return None;
        }
        let enabled =
            keep_watching::sign_in_enabled(view.status, view.sign_in, view.refusal.is_some());
        let model = self.model.clone();
        let name = cluster.to_owned();
        let shown = view.shown_sign_in;
        let toggle = Switch::new(member_id("background-sign-in", member))
            .small()
            .checked(view.sign_in == BackgroundSignIn::On)
            .disabled(!enabled)
            .label(keep_watching::SIGN_IN_LABEL)
            .tooltip(keep_watching::sign_in_tip(view.status, view.sign_in))
            // Deliberately not a registered action: consent comes from this control alone, never
            // from the command palette, a key binding or automation.
            .on_change(move |wanted, _, cx| {
                if *wanted && shown.is_none() {
                    return;
                }
                let shown = shown.filter(|_| *wanted);
                model.update(cx, |m, cx| {
                    m.turn_background_sign_in(name.clone(), shown, cx);
                });
            });
        let command = view.command.map(|command| {
            div()
                .id(member_id("background-sign-in-command", member))
                .min_w_0()
                .truncate()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child(display::single_line(&command).to_string())
                .test_support()
        });
        Some(
            h_flex()
                .id(member_id("background-sign-in-row", member))
                .w_full()
                .flex_none()
                .px_3()
                .py_1()
                .gap_3()
                .border_b_1()
                .border_color(theme.border)
                .children(lead)
                .child(div().flex_none().child(toggle))
                .children(command)
                .test_support()
                .into_any_element(),
        )
    }

    /// The active cluster's access mode, in words, from the same check the sidebar's lock shows.
    /// Modes that allow writes get a filled, bold badge. Changing the mode stays on the lock.
    /// `compact` shortens the words drawn; the accessible name and tooltip keep the whole mode.
    fn render_access_mode(
        &self,
        cluster: &ClusterSummary,
        compact: bool,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let model = self.model.read(cx);
        let protection = model.protection(cluster);
        // Checked first: whatever the protection, the core refuses writes on this definition.
        let paused = model.writes_paused(cluster);
        let tip = if paused {
            WRITES_PAUSED_TIP.to_owned()
        } else {
            protection_text(
                protection,
                &model.trust(cluster),
                cluster.info.server.as_deref(),
            )
        };
        let text = if paused {
            WRITES_PAUSED
        } else {
            access_mode_text(protection)
        };
        let short = if paused {
            WRITES_PAUSED
        } else {
            access_mode_short_text(protection)
        };
        let (icon, writable) = match protection {
            _ if paused => (FlintIcon::Lock, false),
            Protection::Protected => (FlintIcon::Lock, false),
            Protection::Local | Protection::Unlocked => (FlintIcon::LockOpen, true),
            Protection::Trusted => (FlintIcon::ShieldCheck, true),
        };
        let color = if writable {
            theme.warning
        } else {
            theme.muted_foreground
        };
        h_flex()
            .id("access-mode")
            .aria_label(text)
            .test_support()
            .flex_none()
            .items_center()
            .gap_1()
            .px_1p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(color.opacity(if writable { 0.7 } else { 0.3 }))
            .when(writable, |el| el.bg(color.opacity(0.12)).font_semibold())
            .text_xs()
            .text_color(color)
            .child(Icon::new(icon).xsmall())
            .child(if compact { short } else { text })
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
    }

    fn render_namespace_filter(
        &self,
        summary: &str,
        label: String,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let this = cx.weak_entity();
        let input = self.namespace_input.clone();
        let on_open = this.clone();
        let picker = self.namespace_picker_focus.clone();
        let button = self.namespace_button_focus.clone();
        let list_scroll = self.namespace_list_scroll.clone();
        Popover::new("namespace-filter")
            .anchor(Anchor::TopRight)
            .track_focus(&picker)
            .open(self.namespace_popover_open)
            // After the change in hand: it may come while the workspace is being updated (the
            // palette dismissing it).
            .on_open_change(move |open, _, cx| {
                let (open, this) = (*open, on_open.clone());
                cx.defer(move |cx| {
                    let _ = this.update(cx, |ws, cx| {
                        if ws.namespace_popover_open != open {
                            ws.namespace_popover_open = open;
                            cx.notify();
                        }
                    });
                });
            })
            .trigger(
                Button::new("namespace-filter-button")
                    .xsmall()
                    .outline()
                    .label(label)
                    // The label is cut to a few characters, so the watch mode button beside it
                    // stays on screen at the minimum width (UP-33). The accessible name and the
                    // tooltip keep the whole name.
                    .accessibility_label(summary.to_owned())
                    .track_focus(&self.namespace_button_focus)
                    .tab_stop(false)
                    .tooltip(format!(
                        "{summary}\nFilter by namespace (remembered per cluster)"
                    )),
            )
            .content(move |_, window, cx| {
                // Just opened: its controls have tab stops once this frame is drawn.
                if picker.is_focused(window) {
                    let picker = picker.clone();
                    window.defer(cx, move |window, cx| {
                        if picker.is_focused(window) {
                            cycle_focus_within(&picker, true, window, cx);
                        }
                    });
                }
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
                let back = button.clone();
                v_flex()
                    .track_focus(&picker)
                    .key_context(registry::NAMESPACE_PICKER)
                    // Focus moves first, so the popover's own Cancel, next, leaves it there. The
                    // workspace may be mid-update here (the palette dismissing the picker).
                    .on_action(move |_: &Cancel, window, cx| {
                        back.focus(window, cx);
                        cx.propagate();
                    })
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
                            .track_scroll(&list_scroll)
                            // Out by the rows' padding, so they line up with the controls around.
                            .mx(-NAMESPACE_RING_ROOM)
                            .when(names.is_empty(), |el| {
                                el.child(
                                    div()
                                        .id("ns-empty")
                                        .p(NAMESPACE_RING_ROOM)
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(empty_text),
                                )
                            })
                            .children(names.into_iter().enumerate().map(|(row, ns)| {
                                let checked = filter.contains(&ns);
                                let mut context = KeyContext::default();
                                context.set(NAMESPACE_ROW, row.to_string());
                                div().key_context(context).p(NAMESPACE_RING_ROOM).child(
                                    Checkbox::new(SharedString::from(format!("ns-{ns}")))
                                        .label(ns.clone())
                                        .checked(checked)
                                        .on_click(registry::on_click(ToggleNamespace {
                                            namespace: ns,
                                        })),
                                )
                            }))
                            .test_support(),
                    )
                    .child(div().h_px().bg(theme.border))
                    .child(Input::new(&input).id("ns-add").small())
            })
    }

    /// Whether the header shows the namespace filter now.
    fn namespace_filter_shown(&self, cx: &App) -> bool {
        let model = self.model.read(cx);
        model.active_cluster().is_some_and(|cluster| {
            header_controls(cluster, self.list.as_ref().map(|l| l.read(cx))).namespaces
        })
    }

    /// Moves focus to the picker's next or previous control, and scrolls a row into view.
    pub(super) fn move_in_namespace_picker(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cycle_focus_within(&self.namespace_picker_focus, forward, window, cx);
        let row = window
            .context_stack()
            .iter()
            .rev()
            .find_map(|context| context.get(NAMESPACE_ROW)?.parse().ok());
        if let Some(row) = row {
            self.namespace_list_scroll.scroll_to_item(row);
            cx.notify();
        }
    }

    /// A dialog closes the picker, which is drawn above everything. Once the dialog is gone,
    /// focus returns to the button, or to the workspace if the button is no longer shown.
    ///
    /// The dialog gives focus back to what had it, at once or after its closing delay, so a
    /// hidden picker control that gets it then is sent on to the same place.
    fn sync_namespace_picker_with_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if gpui_kit::component::WindowExt::has_active_dialog(window, cx) {
            if self.namespace_popover_open {
                self.namespace_popover_open = false;
                self.namespace_focus_after_dialog = PickerDialogFocus::DialogOpen;
            }
            return;
        }
        if self.namespace_popover_open {
            self.namespace_focus_after_dialog = PickerDialogFocus::Settled;
            return;
        }
        let move_focus = match self.namespace_focus_after_dialog {
            PickerDialogFocus::Settled => false,
            PickerDialogFocus::DialogOpen => {
                self.namespace_focus_after_dialog = PickerDialogFocus::Restoring;
                true
            }
            PickerDialogFocus::Restoring => {
                self.namespace_picker_focus.is_focused(window)
                    || self
                        .namespace_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
            }
        };
        if move_focus {
            let target = if self.namespace_filter_shown(cx) {
                self.namespace_button_focus.clone()
            } else {
                self.focus.clone()
            };
            // After this draw: a focus change during one schedules no frame.
            window.defer(cx, move |window, cx| window.focus(&target, cx));
        }
    }

    /// The watch mode switch: this cluster's override, and the global default (ADR-0008).
    /// `compact` draws a short label; the accessible name and tooltip keep the whole mode.
    fn render_watch_mode(
        &self,
        cluster: &str,
        compact: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let settings = self.model.read(cx).settings.clone();
        let effective = settings.watch_mode_for(cluster);
        let label = watch_mode_label(effective);
        let model = self.model.clone();
        let cluster = cluster.to_owned();
        let settings_ok = self.model.read(cx).settings_error.is_none();
        Button::new("watch-mode")
            .xsmall()
            .ghost()
            .label(if compact {
                watch_mode_short_label(effective)
            } else {
                label
            })
            .accessibility_label(label)
            .disabled(!settings_ok)
            .tab_stop(false)
            .tooltip(format!(
                "{label}\nAll kinds: every kind is watched. On view: the kinds of open views and the problem kinds are watched"
            ))
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
                        PopupMenuItem::new("On view")
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
                        PopupMenuItem::new("On view")
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

    /// The pane widths as shown now, with the panes that are showing. Only a split has a detail
    /// pane beside the list: a detail shown alone fills the main area, so the sidebar keeps its
    /// width and its rows can still be told apart.
    fn layout(&self, window_width: f32, cx: &App) -> Layout {
        let detail_beside = self.detail.is_some() && self.main_panes(cx) == MainPanes::Split;
        layout(&self.panes, window_width, self.nav_shown(cx), detail_beside)
    }

    /// Whether the navigation is on screen: for the active cluster, or for an open set, which
    /// needs its kinds even with nothing active.
    fn nav_shown(&self, cx: &App) -> bool {
        self.model.read(cx).active_cluster().is_some() || self.sets.is_open()
    }

    /// Whether the main header puts its controls on a second row in a window this wide.
    fn header_has_two_rows(&self, window_width: f32, cx: &App) -> bool {
        header_needs_two_rows(self.main_width(window_width, cx))
    }

    /// The main area's width in a window this wide.
    fn main_width(&self, window_width: f32, cx: &App) -> f32 {
        let widths = self.layout(window_width, cx);
        window_width - widths.clusters - widths.navigation
    }

    /// What the main area shows of the list and the open detail. A failed connection shows
    /// neither, so there is nothing to go Back from.
    fn main_panes(&self, cx: &App) -> MainPanes {
        let stopped = self
            .model
            .read(cx)
            .active_cluster()
            .is_some_and(|c| connection_stopped(&c.state));
        if !self.narrow || self.detail.is_none() || self.list.is_none() || stopped {
            MainPanes::Split
        } else if self.list_in_front {
            MainPanes::ListAlone
        } else {
            MainPanes::DetailAlone
        }
    }

    /// Brings the list in front of the detail, which stays open with its selection.
    fn back_to_list(&mut self, _: &BackToList, window: &mut Window, cx: &mut Context<Self>) {
        // A key reaches this without the registry's guard.
        if self.main_panes(cx) != MainPanes::DetailAlone {
            cx.propagate();
            return;
        }
        self.list_in_front = true;
        if let Some(list) = &self.list {
            let focus = list.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
        }
        // The detail's YAML field, if it had focus: it is no longer drawn.
        release_unfocused_input(window, cx);
        cx.notify();
    }

    /// Follows the window's width, and moves focus off a pane the narrow layout no longer draws
    /// (the selected row takes focus when clicked, for one), so the keys keep working.
    fn sync_narrow_layout(
        &mut self,
        window_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.narrow = too_narrow_for_split(window_width);
        self.right_press = false;
        if self.detail.is_some() {
            self.menu_row_pending = false;
        }
        if !self.narrow || (self.detail.is_none() && !self.menu_row_pending) {
            self.list_in_front = false;
        }
        let (Some(list), Some(detail)) = (&self.list, &self.detail) else {
            return;
        };
        let list = list.read(cx).focus_handle().clone();
        let detail = detail.read(cx).focus_handle().clone();
        let dialog = gpui_kit::component::WindowExt::has_active_dialog(window, cx);
        if !dialog {
            let hidden_in = if list.contains_focused(window, cx) {
                Some(MainPanes::DetailAlone)
            } else if detail.contains_focused(window, cx) {
                Some(MainPanes::ListAlone)
            } else {
                None
            };
            if let (Some(hidden_in), Some(focused)) = (hidden_in, window.focused(cx)) {
                self.pane_focus = Some((focused.downgrade(), hidden_in));
            }
        }
        let panes = self.main_panes(cx);
        let (hidden, shown) = match panes {
            MainPanes::Split => return,
            MainPanes::DetailAlone => (list, detail),
            MainPanes::ListAlone => (detail, list),
        };
        // A dialog gave focus back to a control of a pane hidden while it was open: no longer
        // drawn, it is not found inside the pane.
        let restored_hidden = !dialog
            && self
                .pane_focus
                .as_ref()
                .is_some_and(|(focused, hidden_in)| {
                    *hidden_in == panes && focused.upgrade().is_some_and(|f| f.is_focused(window))
                });
        if hidden.contains_focused(window, cx) || restored_hidden {
            // After this draw: a focus change during one schedules no frame.
            window.defer(cx, move |window, cx| {
                window.focus(&shown, cx);
                release_unfocused_input(window, cx);
            });
        }
    }

    fn render_main(&self, window_width: f32, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        if let Some(settings) = self.render_settings_page(theme, cx) {
            return settings;
        }
        if let Some(set_view) = self.render_set_view(window_width, theme, cx) {
            return set_view;
        }
        if let Some(detail) = self.edit_shown_alone(cx) {
            return div()
                .id("held-edit-pane")
                .flex_1()
                .min_w_0()
                .h_full()
                .relative()
                .child(detail)
                .children(self.detail_tag_strip(theme, cx))
                .test_support()
                .into_any_element();
        }
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
        let dashboard = self
            .dashboard
            .as_ref()
            .filter(|_| !connection_stopped(&cluster.state));
        let overview_notice = dashboard
            .filter(|_| matches!(cluster.state, ConnectionState::Connecting))
            .and_then(|_| {
                connection_progress::connecting_notice(&cluster, model.active_progress(), theme)
            });
        let content: AnyElement = match (&self.list, &cluster.state) {
            _ if dashboard.is_some() => div()
                .flex_1()
                .min_h_0()
                .children(dashboard.map(|(view, _)| view.clone()))
                .into_any_element(),
            // A failed connection stops the watches; say why instead of an endless "Loading".
            (Some(_), state)
                if !connection_stopped(state) && self.main_panes(cx) == MainPanes::DetailAlone =>
            {
                h_flex()
                    .id("detail-alone-pane")
                    .flex_1()
                    .min_h_0()
                    .children(self.detail.clone().map(|detail| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .relative()
                            .child(detail)
                            .children(self.detail_tag_strip(theme, cx))
                    }))
                    .test_support()
                    .into_any_element()
            }
            (Some(list), state) if !connection_stopped(state) => h_flex()
                .flex_1()
                .min_h_0()
                .child(div().flex_1().min_w_0().h_full().child(list.clone()))
                .when_some(
                    self.detail
                        .clone()
                        .filter(|_| self.main_panes(cx) == MainPanes::Split),
                    |el, detail| {
                        el.child(
                            div()
                                .w(px(self.detail_width(window_width)))
                                .h_full()
                                .flex_none()
                                .relative()
                                .border_l_1()
                                .border_color(theme.border)
                                .child(detail)
                                .children(self.detail_tag_strip(theme, cx))
                                .child(pane_handle(
                                    "detail-edge",
                                    Pane::Detail,
                                    HandleEdge::Leading,
                                )),
                        )
                    },
                )
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
            (_, ConnectionState::AwaitingConfirmation) => centered(
                v_flex()
                    .id("connection-waiting")
                    .aria_label(state_label(&ConnectionState::AwaitingConfirmation))
                    .test_support()
                    .gap_1()
                    .max_w(px(640.))
                    .child(div().child(state_label(&ConnectionState::AwaitingConfirmation)))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(
                        "A kubeconfig file that may define this cluster can't be read. \
                                 Flint connects once it can.",
                    )),
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
                    .children(connection_progress::retry_notice(
                        model.active_progress(),
                        theme,
                    ))
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
            _ => centered(connection_progress::connecting(
                &cluster,
                model.active_progress(),
                theme,
            )),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_header(&cluster, window_width, theme, cx))
            .children(self.render_background_sign_in(&cluster.info.name, theme, cx))
            .children(self.server_moves.moved().map(|moved| {
                let text = repointed_text(&model.names().with_real(&cluster.info.name), moved);
                notice("context-repointed", text.clone(), theme.warning)
                    .aria_label(text)
                    .test_support()
            }))
            .children(overview_notice)
            .child(content)
            .into_any_element()
    }
}

/// A side pane's title. On macOS it is the top row, as tall as the main header, and moves the
/// window. A pane under the traffic lights (`traffic_lights` above zero) leaves the row to them
/// and shows no title.
/// Whether `kind` (`plural` or `plural.group`) is in a group `discovery` could not vouch for, so
/// its absence proves nothing.
fn may_still_be_discovered(discovery: &flint_core::ClusterDiscovery, kind: &str) -> bool {
    let group = kind.split_once('.').map_or("", |(_, group)| group);
    discovery.incomplete_groups.iter().any(|g| g == group)
}

fn pane_title(title: &'static str, traffic_lights: Pixels, theme: &Theme) -> impl IntoElement {
    let row = div()
        .px_3()
        .text_xs()
        .font_semibold()
        .text_color(theme.muted_foreground)
        .when(traffic_lights == px(0.), |el| el.child(title));
    if cfg!(target_os = "macos") {
        super::theme::moves_window(row)
            .flex()
            .flex_none()
            .items_center()
            .h(px(HEADER_HEIGHT))
    } else {
        row.pt_3().pb_1()
    }
}

/// What a cluster's protection is, in its row's tooltip.
pub(super) fn protection_text(
    protection: Protection,
    trust: &TrustState,
    server: Option<&str>,
) -> String {
    let text = match protection {
        Protection::Protected => "Protected: read-only in Flint until you unlock or trust it",
        Protection::Local => "On this machine: unlocked",
        Protection::Unlocked => "Unlocked until Flint quits",
        Protection::Trusted => "Trusted: unlocked across restarts",
    };
    match trust {
        TrustState::ServerChanged { trusted_server } if protection != Protection::Trusted => {
            let trusted = changed_from(server, trusted_server);
            format!("{text}. Cluster changed: trusted at {trusted}, now points elsewhere")
        }
        _ => text.to_owned(),
    }
}

/// A cluster as its row's controls name it: the title, then the full context when the title is
/// not the context, since two contexts can share a title. The context is quoted and escaped, so
/// no context or title can be written to read as another cluster's name.
/// `base`, or for a set member's copy of a control, `base-<member>`.
fn member_id(base: &'static str, member: Option<&str>) -> SharedString {
    match member {
        Some(member) => format!("{base}-{member}").into(),
        None => base.into(),
    }
}

fn control_name(title: &str, context: &str) -> String {
    let shown = flint_core::display::single_line(title);
    if title == context && !context.contains('"') {
        shown
    } else {
        format!("{shown} (context {context:?})")
    }
}

/// The header's words for a cluster's access mode.
fn access_mode_text(protection: Protection) -> &'static str {
    match protection {
        Protection::Protected => "Read-only",
        Protection::Local => "Local: writable",
        Protection::Unlocked => "Unlocked: writable",
        Protection::Trusted => "Trusted: writable",
    }
}

/// The access mode while the core refuses writes on a definition it can't confirm yet.
const WRITES_PAUSED: &str = "Writes paused";

pub(super) const WRITES_PAUSED_TIP: &str =
    "Writes are paused until Flint can read your kubeconfig files and confirm this cluster";

/// Why a write or a new shell cannot start on a cluster whose writes are paused.
fn writes_paused_reason(cluster: &str) -> String {
    format!("a kubeconfig file can't be read, so writes and new shells on {cluster} are paused")
}

/// The access mode on a narrow header's second row: still tells every mode apart.
fn access_mode_short_text(protection: Protection) -> &'static str {
    match protection {
        Protection::Protected => "Read-only",
        Protection::Local => "Local",
        Protection::Unlocked => "Unlocked",
        Protection::Trusted => "Trusted",
    }
}

/// The accessible name of a cluster's lock button: which cluster, and its protection now.
fn lock_label(title: &str, protection: Protection) -> String {
    let state = match protection {
        Protection::Protected => "protected, read-only",
        Protection::Local => "on this machine, unlocked",
        Protection::Unlocked => "unlocked until Flint quits",
        Protection::Trusted => "trusted, unlocked across restarts",
    };
    format!("Lock for {title}: {state}")
}

/// The accessible name of a cluster's AI exposure button: which cluster, and its exposure now.
/// A context re-pointed at another server counts as hidden, so its name says so.
fn exposure_label(title: &str, exposure: &AiExposureState) -> String {
    let state = match exposure {
        AiExposureState::Hidden => "hidden",
        AiExposureState::Exposed => "exposed, read-only",
        AiExposureState::ServerChanged { .. } => "cluster changed, hidden",
    };
    format!("AI exposure for {title}: {state}")
}

/// An entry of a cluster's lock menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockEntry {
    Unlock,
    LockAgain,
    Trust,
    /// Remove the context's pin from `settings.toml`: Stop Trusting, or Remove the Old Trust
    /// when the pin is for another server.
    Untrust {
        stale: bool,
    },
}

impl LockEntry {
    fn label(self) -> &'static str {
        match self {
            LockEntry::Unlock => "Unlock for This Session…",
            LockEntry::LockAgain => "Lock Again",
            LockEntry::Trust => "Trust This Cluster…",
            LockEntry::Untrust { stale: false } => "Stop Trusting",
            LockEntry::Untrust { stale: true } => "Remove the Old Trust",
        }
    }

    fn action(self, cluster: String) -> Box<dyn Action> {
        match self {
            LockEntry::Unlock => Box::new(UnlockCluster { cluster }),
            LockEntry::LockAgain => Box::new(LockCluster { cluster }),
            LockEntry::Trust => Box::new(TrustCluster { cluster }),
            LockEntry::Untrust { .. } => Box::new(UntrustCluster { cluster }),
        }
    }
}

/// What a cluster's lock menu offers. Removing the trust is offered whenever `settings.toml`
/// holds a pin for the context (`trust` is not `NotTrusted`), whether or not it grants access
/// now: a pin whose removal failed after Stop Trusting (the cluster is protected meanwhile),
/// or one for another server. Unlocking and trusting need a server to pin to.
fn lock_menu(
    has_server: bool,
    protection: Protection,
    trust: &TrustState,
    settings_ok: bool,
) -> Vec<LockEntry> {
    let protected = protection == Protection::Protected && has_server;
    let unlocked = protection == Protection::Unlocked;
    let mut entries = Vec::new();
    if protected {
        entries.push(LockEntry::Unlock);
    }
    if unlocked {
        entries.push(LockEntry::LockAgain);
    }
    if (protected || unlocked) && settings_ok {
        entries.push(LockEntry::Trust);
    }
    if !matches!(trust, TrustState::NotTrusted) && settings_ok {
        entries.push(LockEntry::Untrust {
            stale: matches!(trust, TrustState::ServerChanged { .. }),
        });
    }
    entries
}

/// The lock button of a cluster's row (UP-55): a closed lock while protected, an open one while
/// unlocked for this session, a shield while trusted, and nothing for a cluster on this machine
/// (unless an old trust is left to remove). Its menu ([`lock_menu`]) unlocks, locks again,
/// trusts or stops trusting. It is not a tab stop, so a stray Space or Enter cannot open it.
fn render_lock_button(
    name: &str,
    title: &str,
    has_server: bool,
    protection: Protection,
    trust: &TrustState,
    settings_ok: bool,
    writes_paused: bool,
) -> Option<AnyElement> {
    let entries = lock_menu(has_server, protection, trust, settings_ok);
    let label = if writes_paused {
        format!("Lock for {title}: writes paused")
    } else {
        lock_label(title, protection)
    };
    let button = Button::new(SharedString::from(format!("lock-{name}")))
        .accessibility_label(label)
        .xsmall()
        .tab_stop(false);
    let pinned = !matches!(trust, TrustState::NotTrusted);
    let (button, tip) = match protection {
        Protection::Local if entries.is_empty() => return None,
        Protection::Local => (
            button.ghost().icon(FlintIcon::LockOpen),
            "On this machine: unlocked. Click to remove an old trust.",
        ),
        Protection::Protected if entries.is_empty() => {
            return Some(
                button
                    .ghost()
                    .icon(FlintIcon::Lock)
                    .disabled(true)
                    .tooltip("Protected. This context has no API server, so it cannot be unlocked.")
                    .into_any_element(),
            );
        }
        Protection::Protected if !has_server => (
            button.ghost().icon(FlintIcon::Lock),
            "Protected. This context has no API server now. Click to remove its old trust.",
        ),
        Protection::Protected if pinned => (
            button.ghost().icon(FlintIcon::Lock),
            "Protected: read-only in Flint, though settings.toml still holds a trust for it. \
             Click to unlock it, trust it, or remove that trust.",
        ),
        Protection::Protected => (
            button.ghost().icon(FlintIcon::Lock),
            "Protected: read-only in Flint. Click to unlock or trust it.",
        ),
        Protection::Unlocked => (
            button.warning().icon(FlintIcon::LockOpen),
            "Unlocked until Flint quits. Click to lock it again (shells already open keep \
             running) or trust it.",
        ),
        Protection::Trusted => (
            button.ghost().icon(FlintIcon::ShieldCheck),
            "Trusted: unlocked across restarts. Click to stop trusting it (shells already open \
             keep running).",
        ),
    };
    // The menu stays, so the lock can still be changed, but nothing here says unlocked.
    let (button, tip) = if writes_paused {
        (
            button.ghost().icon(FlintIcon::Lock),
            "Writes paused until Flint can read your kubeconfig files. Click for the lock options.",
        )
    } else {
        (button, tip)
    };
    let cluster = name.to_owned();
    let menu =
        button
            .tooltip(tip)
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                entries.iter().fold(menu, |menu, entry| {
                    menu.item(
                        PopupMenuItem::new(entry.label()).on_click(registry::on_click_or(
                            entry.action(cluster.clone()),
                            |_, _, _| {},
                        )),
                    )
                })
            });
    Some(
        div()
            .flex_none()
            // not-an-action: keeps the click on the menu button from selecting the row.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(menu)
            .into_any_element(),
    )
}

/// A server as the UI shows it, by its origin ([`flint_core::redact::server_origin`]: no path),
/// or "no server". Pins are compared on the exact URL, never on this text.
pub(super) fn server_text(server: Option<&str>) -> String {
    server.map_or("no server".to_owned(), flint_core::redact::server_origin)
}

/// The main area's notice that the active context moved. Servers are sanitized and kept on one
/// line, so a kubeconfig value can neither show a credential nor pass for other text.
fn repointed_text(cluster: &str, moved: &ServerMove) -> String {
    let shown = |server: Option<&str>| display::single_line(&server_text(server));
    let (old, new) = (shown(moved.old.as_deref()), shown(moved.new.as_deref()));
    if old == new {
        return format!(
            "{cluster} now points at a different API server. Both show as {new}: \
             what changed is in a part of the URL Flint hides."
        );
    }
    format!("{cluster} now points at a different API server. Was {old}, now {new}.")
}

/// The host and port of a server URL as the UI shows it, for a row's compact second line.
fn shown_host(server: &str) -> String {
    let shown = flint_core::redact::server_url(server);
    let rest = shown.split_once("://").map_or(shown.as_str(), |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_owned()
}

/// How a dialog names the server a pin was made for. Two different URLs can show as the same
/// text, and "trusted at X, which is a different server" would then read as a contradiction.
/// Only the old URL's origin is named: its path is not needed to tell the two apart.
fn previous_address(server: &str, previous: &str) -> String {
    use flint_core::redact::{server_origin, server_url};
    let origin = server_origin(previous);
    if server_url(previous) == server_url(server) {
        "another URL that shows the same here".to_owned()
    } else if origin == server_origin(server) {
        format!("another path on {origin}")
    } else {
        origin
    }
}

/// [`previous_address`] for a context that may have no server now, as a sidebar tooltip says it.
pub(super) fn changed_from(server: Option<&str>, previous: &str) -> String {
    server.map_or_else(
        || flint_core::redact::server_origin(previous),
        |server| previous_address(server, previous),
    )
}

/// Asks before unlocking a protected cluster until Flint quits, naming the context and its
/// server (UP-55).
fn confirm_unlock(
    window: &mut Window,
    cx: &mut App,
    model: Entity<ClustersModel>,
    shown: &str,
    context: String,
    server: String,
) {
    let dialog = unlock_dialog(shown, &context, &server);
    confirm::open(window, cx, dialog, move |_, cx| {
        model.update(cx, |m, cx| m.unlock(&context, &server, cx));
    });
}

/// The unlock confirmation, naming the context and the server it points at. `shown` is what the
/// title calls it ([`ClusterNames::with_real`]).
fn unlock_dialog(shown: &str, context: &str, server: &str) -> DialogText {
    let context = flint_core::display::single_line(context);
    let server = flint_core::redact::server_origin(server);
    DialogText {
        title: format!("Unlock {shown}?"),
        text: format!(
            "Context: {context}\nServer: {server}\n\n\
             Until Flint quits or you lock it again, changes and code execution (shell, edit) \
             stop asking. This holds only while the context points at this server. Locking \
             stops new shells; open shells keep running."
        ),
        ok: "Unlock".into(),
        cancel: "Cancel".into(),
        id: "unlock-description",
        // Unlocking takes the button: Enter cancels, as closing and quitting do.
        default_cancel: true,
    }
}

/// Asks before trusting a cluster, naming the context and its server (UP-55).
fn confirm_trust(
    window: &mut Window,
    cx: &mut App,
    model: Entity<ClustersModel>,
    shown: &str,
    context: String,
    server: String,
    previous: Option<String>,
) {
    let dialog = trust_dialog(shown, &context, &server, previous.as_deref());
    confirm::open(window, cx, dialog, move |_, cx| {
        model.update(cx, |m, _| m.trust_cluster(context.clone(), server.clone()));
    });
}

/// The trust confirmation, naming the context and the server it points at (and the server it
/// was trusted at before, when the context moved).
fn trust_dialog(shown: &str, context: &str, server: &str, previous: Option<&str>) -> DialogText {
    let context = flint_core::display::single_line(context);
    let previous = previous.map(|previous| previous_address(server, previous));
    let server = flint_core::redact::server_origin(server);
    let mut text = format!("Context: {context}\nServer: {server}\n\n");
    if let Some(previous) = previous {
        text.push_str(&format!(
            "This context was trusted at {previous}, which is a different server.\n\n"
        ));
    }
    text.push_str(
        "Trust survives a restart: changes and code execution (shell, edit) stop asking. It is \
         stored in settings.toml and pinned to this server. If the context moves, the cluster \
         is protected again.",
    );
    DialogText {
        title: format!("Trust {shown}?"),
        text,
        ok: "Trust".into(),
        cancel: "Cancel".into(),
        id: "trust-description",
        // Trusting takes the button: Enter cancels.
        default_cancel: true,
    }
}

/// Asks before exposing a cluster to AI agents (or before changing what they may read of an
/// exposed one), naming the context and the server it points at, with the scope pickers under
/// the text. Confirming runs `save` with the context, the server and the scope checked: one
/// write of the entry and its scope. A scope with nothing chosen cannot be confirmed, and one
/// that adds access to what is saved asks again, naming what it adds.
fn confirm_expose(
    window: &mut Window,
    cx: &mut App,
    model: Entity<ClustersModel>,
    (context, server): (String, String),
    dialog: DialogText,
    picker: Entity<ScopePicker>,
    save: impl Fn(&mut ClustersModel, String, String, AiScope) + 'static,
) {
    let content = picker.clone().into();
    let state = {
        let picker = picker.clone();
        move |cx: &App| picker.read(cx).confirm_state(cx)
    };
    let save = Rc::new(save);
    confirm::open_gated(window, cx, dialog, content, state, move |window, cx| {
        // A name still typed in the namespace field is added first; one that is not a
        // namespace name keeps the dialog open, saying so, rather than being dropped.
        let Ok(scope) = picker.update(cx, |p, cx| p.confirmed_scope(window, cx)) else {
            return false;
        };
        let added = picker.read(cx).added_access(&scope);
        if added.is_empty() {
            model.update(cx, |m, _| save(m, context.clone(), server.clone(), scope));
            return true;
        }
        let question = broaden_dialog(&context, &server, &added, &scope);
        let (model, save, context, server) =
            (model.clone(), save.clone(), context.clone(), server.clone());
        let answered = std::cell::Cell::new(false);
        // The context and server stay on show, however long the list of what is added.
        confirm::open_pinned(window, cx, question, 2, move |window, cx| {
            if answered.replace(true) {
                return;
            }
            let scope = scope.clone();
            model.update(cx, |m, _| save(m, context.clone(), server.clone(), scope));
            // This question closes by itself; the pickers under it close with it.
            window.defer(cx, confirm::close_newest);
        });
        false
    });
}

/// Asks before a save that lets agents read more than the saved scope, naming what it adds.
/// Enter goes back to the pickers: only the button accepts it.
fn broaden_dialog(context: &str, server: &str, added: &[String], scope: &AiScope) -> DialogText {
    let context = flint_core::display::single_line(context);
    let server = flint_core::redact::server_origin(server);
    let added = flint_core::display::single_line(&added.join("; "));
    let grant = flint_core::display::single_line(&scope_picker::summary(scope));
    DialogText {
        title: "Give AI agents more access?".into(),
        text: format!(
            "This adds: {added}.\n\nAgents will be able to read {grant}.\n\n\
             Context: {context}\nServer: {server}"
        ),
        ok: "Add Access".into(),
        cancel: "Go Back".into(),
        id: "scope-broaden-description",
        default_cancel: true,
    }
}

/// The Edit AI Scope dialog of an exposed cluster: what agents may read now, and the pickers.
fn scope_dialog(context: &str, server: &str, scope: &AiScope) -> DialogText {
    let context = flint_core::display::single_line(context);
    let server = flint_core::redact::server_origin(server);
    DialogText {
        title: "What may AI agents read?".into(),
        text: format!(
            "Agents using Flint's MCP server read only the namespaces and kinds chosen below; \
             anything else looks to them as if the cluster were not exposed. Now: {}.\n\n\
             Context: {context}\nServer: {server}",
            scope_picker::summary(scope)
        ),
        ok: "Save".into(),
        cancel: "Cancel".into(),
        id: "scope-description",
        default_cancel: false,
    }
}

/// The Expose to AI confirmation, naming the context and the server it points at.
fn expose_dialog(context: &str, server: &str, previous: Option<&str>) -> DialogText {
    let context = flint_core::display::single_line(context);
    let previous = previous.map(|previous| previous_address(server, previous));
    let server = flint_core::redact::server_origin(server);
    let mut text = format!(
        "AI agents using Flint's MCP server will be able to read this cluster: resources, events \
         and pod logs. They cannot change anything. Secret values are hidden, and so are values in \
         pod logs, event messages and container arguments that look like credentials (tokens, \
         passwords, keys). \
         Everything else, ConfigMaps and env values included, reaches the agent as the cluster \
         returns it, and a credential in an unusual format can get through. Below, you can limit \
         them to some namespaces and kinds.\n\n\
         Context: {context}\nServer: {server}"
    );
    if let Some(previous) = previous {
        text.push_str(&format!(
            "\n\nThis context was exposed at {previous}, which is a different server. What \
             agents could read there is checked below."
        ));
    }
    DialogText {
        title: "Expose to AI agents?".into(),
        text,
        ok: "Expose".into(),
        cancel: "Cancel".into(),
        id: "expose-description",
        default_cancel: false,
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

/// Whether a main area this wide puts the detail-alone header's controls on a second row.
fn header_needs_two_rows(main_width: f32) -> bool {
    main_width < HEADER_ONE_ROW_MIN_WIDTH
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
    let stopped = connection_stopped(&cluster.state);
    HeaderControls {
        namespaces: !stopped && list.is_none_or(|l| l.namespaced()),
        watch_mode: !stopped,
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
        WatchMode::OnView => "on view",
    }
}

/// The watch mode button's label: "Live: all kinds" or "Live: on view".
fn watch_mode_label(mode: WatchMode) -> &'static str {
    match mode {
        WatchMode::All => "Live: all kinds",
        WatchMode::OnView => "Live: on view",
    }
}

/// The watch mode button's label on a narrow header's second row.
fn watch_mode_short_label(mode: WatchMode) -> &'static str {
    match mode {
        WatchMode::All => "Live: all",
        WatchMode::OnView => "Live: on view",
    }
}

/// `text` cut to `max` characters, with an ellipsis in place of the cut part.
fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// "All namespaces", "demo", or "3 namespaces".
pub fn namespace_summary(filter: &[String]) -> String {
    match filter {
        [] => "All namespaces".to_owned(),
        [one] => one.clone(),
        many => format!("{} namespaces", many.len()),
    }
}

/// The namespace button's text: [`namespace_summary`], or, on a list that fell back to the
/// namespaces the account can read and does not cover the whole selection, what it really covers.
fn namespace_button_text(filter: &[String], limit: Option<&flint_core::NamespaceLimit>) -> String {
    let Some(limit) = limit else {
        return namespace_summary(filter);
    };
    if filter.is_empty() {
        return match limit.listed.len() {
            0 => "No namespaces (limited)".to_owned(),
            1 => "1 namespace (limited)".to_owned(),
            n => format!("{n} namespaces (limited)"),
        };
    }
    let covered = filter.iter().filter(|n| limit.listed.contains(n)).count();
    match filter.len() {
        selected if covered == selected => namespace_summary(filter),
        1 => "0 of 1 namespace (limited)".to_owned(),
        selected => format!("{covered} of {selected} namespaces (limited)"),
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_namespace_picker_with_dialog(window, cx);
        self.close_finished_held_edit(window, cx);
        self.settings_follow_main();
        let window_width = f32::from(window.viewport_size().width);
        let window_height = f32::from(window.viewport_size().height);
        self.sync_narrow_layout(window_width, window, cx);
        self.two_row_header = self.header_has_two_rows(window_width, cx);
        let cx: &Context<Self> = cx;
        let theme = cx.theme();
        let has_cluster = self.model.read(cx).active_cluster().is_some();
        let widths = self.layout(window_width, cx);
        let traffic_lights = super::theme::traffic_light_inset(window);
        h_flex()
            .id("workspace")
            .key_context(registry::WORKSPACE)
            .map(|el| self.register_actions(el, cx))
            .map(|el| self.register_set_actions(el, cx))
            .map(|el| self.register_settings_actions(el, cx))
            // Captured, as the list reports no change for the row already selected: asking for
            // a row or object shows the detail, and asking for the filter shows the list. A
            // right-click selects its row without leaving it: the row hosts the context menu.
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
                this.right_press = event.button == MouseButton::Right;
            }))
            .capture_action(cx.listener(|this, _: &SelectRow, _, _| {
                let right_click = std::mem::take(&mut this.right_press);
                this.list_in_front = right_click;
                this.menu_row_pending = right_click;
            }))
            .capture_action(cx.listener(|this, _: &OpenObject, _, _| {
                this.list_in_front = false;
                this.leave_settings_for_view();
            }))
            .capture_action(cx.listener(|this, _: &EditYaml, _, _| this.list_in_front = false))
            .capture_action(cx.listener(|this, _: &FocusFilter, _, _| this.list_in_front = true))
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
            .child(self.render_sidebar(
                &widths,
                traffic_lights,
                theme,
                self.sidebar_focus.is_focused(window),
                cx,
            ))
            .when(self.nav_shown(cx), |el| {
                el.child(self.render_nav_column(widths.navigation, theme))
            })
            .child(
                // The main area over the dock, which spans it whichever cluster is active.
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .relative()
                    // With no cluster there is no header, and the window still needs a top to
                    // drag it by.
                    .when(!has_cluster, |el| {
                        el.child(
                            super::theme::moves_window(div())
                                .absolute()
                                .top_0()
                                .left_0()
                                .w_full()
                                .h(px(HEADER_HEIGHT)),
                        )
                    })
                    .child(h_flex().flex_1().min_h_0().w_full().child(self.render_main(
                        window_width,
                        theme,
                        cx,
                    )))
                    .children(resource_actions::drain_strips(
                        || self.cluster_names(cx),
                        theme,
                        cx,
                    ))
                    .when_some(self.action_error.clone(), |el, why| {
                        el.child(refusal_strip(why, theme))
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
            .when_some(self.palette.as_ref(), |el, (palette, _)| {
                // Over everything, near the top, as in Zed. The overlay itself takes no clicks.
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .justify_center()
                        // As tall as its content: a click below it is outside it.
                        .items_start()
                        .pt(px(56.))
                        .px_4()
                        .child(palette.clone()),
                )
            })
    }
}

/// Why a Logs or Shell did not open: a row of its own between the main area and the dock, so
/// it covers neither the list nor the pane (they give up its height while it shows), one line
/// high whatever it says (the full sentence is in its tooltip), with a warning icon.
fn refusal_strip(why: String, theme: &Theme) -> impl IntoElement {
    let full: SharedString = why.clone().into();
    h_flex()
        .id("action-error")
        .flex_none()
        .w_full()
        .gap_2()
        .px_3()
        .py_1()
        .text_xs()
        .bg(theme.warning.opacity(0.1))
        .border_t_1()
        .border_color(theme.border)
        .child(
            Icon::new(IconName::TriangleAlert)
                .xsmall()
                .flex_none()
                .text_color(theme.warning),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.foreground)
                .child(why),
        )
        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
        .test_support()
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

#[path = "workspace_disconnect.rs"]
mod disconnect;

#[path = "workspace_forwards.rs"]
mod forwards;

#[path = "workspace_keys.rs"]
mod keys;

#[path = "workspace_problems.rs"]
pub(in crate::ui) mod problems;

#[path = "workspace_settings.rs"]
mod settings_entry;

#[path = "workspace_sets.rs"]
mod sets;

#[cfg(test)]
#[path = "keyboard_tests.rs"]
mod keyboard_tests;

#[cfg(test)]
#[path = "leak_tests.rs"]
mod leak_tests;

#[cfg(test)]
#[path = "narrow_layout_tests.rs"]
mod narrow_layout_tests;

#[cfg(any(test, feature = "automation"))]
#[path = "workspace_state.rs"]
mod state;

#[path = "workspace_unapplied.rs"]
mod unapplied;

#[cfg(test)]
#[path = "workspace_unapplied_tests.rs"]
mod unapplied_tests;

#[cfg(test)]
#[path = "workspace_view_tests.rs"]
mod view_tests;

#[cfg(test)]
mod tests {
    use super::{
        Layout, LockEntry, MIN_LIST_WIDTH, ServerMove, access_mode_short_text, control_name,
        exposure_label, layout, lock_label, lock_menu, mode_name, namespace_summary,
        no_contexts_text, repointed_text, watch_mode_label,
    };
    use flint_core::settings::{AiExposureState, PaneWidths, TrustState};
    use flint_core::{Protection, WatchMode};

    #[test]
    fn the_short_access_modes_stay_distinct() {
        let short = [
            Protection::Protected,
            Protection::Local,
            Protection::Unlocked,
            Protection::Trusted,
        ]
        .map(access_mode_short_text);
        assert_eq!(short, ["Read-only", "Local", "Unlocked", "Trusted"]);
    }

    #[test]
    fn a_move_between_servers_that_show_alike_says_so() {
        let moved = ServerMove {
            old: Some("https://proxy.example.com/k8s/@old".to_owned()),
            new: Some("https://proxy.example.com/k8s/@new".to_owned()),
        };
        let text = repointed_text("dev", &moved);
        assert!(text.contains("Flint hides"), "{text}");
        assert!(!text.contains("Was "), "{text}");
        assert!(!text.contains("@old") && !text.contains("@new"), "{text}");
    }

    #[test]
    fn notices_and_dialogs_show_only_the_server_origin() {
        let old = "https://proxy.example.com:6443/k8s/clusters/c-old?x=1";
        let new = "https://other.example.com/k8s/clusters/c-new";
        let moved = ServerMove {
            old: Some(old.to_owned()),
            new: Some(new.to_owned()),
        };
        let mut texts = vec![
            repointed_text("dev", &moved),
            super::server_text(Some(old)),
            super::unlock_dialog("dev", "dev", old).text,
            super::trust_dialog("dev", "dev", old, Some(new)).text,
            super::expose_dialog("dev", old, Some(new)).text,
            super::scope_dialog("dev", old, &Default::default()).text,
            super::broaden_dialog("dev", old, &[], &Default::default()).text,
        ];
        assert!(
            texts[0].contains("Was https://proxy.example.com:6443, now https://other.example.com.")
        );
        for text in texts.drain(..) {
            assert!(!text.contains("/k8s") && !text.contains("x=1"), "{text}");
            assert!(text.contains("https://proxy.example.com:6443"), "{text}");
        }
    }

    // A title that is the context stays short; any other carries the context, and a context
    // written to look like another cluster's control name still gets a name of its own.
    #[test]
    fn control_names_tell_clusters_apart() {
        assert_eq!(control_name("kind-dev", "kind-dev"), "kind-dev");
        let arn = "arn:aws:eks:us-east-1:000000000000:cluster/prod";
        assert_eq!(
            control_name("prod", arn),
            format!("prod (context \"{arn}\")")
        );
        let lookalike = "prod (context \"kind-dev\")";
        assert_ne!(
            control_name(lookalike, lookalike),
            control_name("prod", "kind-dev")
        );
    }

    // A control character in a name never reaches the accessible name, plain or quoted.
    #[test]
    fn control_names_hide_control_characters() {
        let plain = control_name("dev\nprod", "dev\nprod");
        assert!(!plain.chars().any(char::is_control), "{plain:?}");
        assert_eq!(plain, "dev\\u{a}prod");
        let quoted = control_name("a\u{202e}b", "ctx\nx");
        assert!(!quoted.chars().any(char::is_control), "{quoted:?}");
        assert!(!quoted.contains('\u{202e}'), "{quoted:?}");
    }

    // Each state has a name of its own, and a re-pointed context never reads as exposed.
    #[test]
    fn protection_control_names_say_the_state() {
        for (protection, state) in [
            (Protection::Protected, "protected, read-only"),
            (Protection::Local, "on this machine, unlocked"),
            (Protection::Unlocked, "unlocked until Flint quits"),
            (Protection::Trusted, "trusted, unlocked across restarts"),
        ] {
            assert_eq!(
                lock_label("prod", protection),
                format!("Lock for prod: {state}")
            );
        }
        let changed = AiExposureState::ServerChanged {
            exposed_server: "https://10.0.0.1:6443".into(),
        };
        for (exposure, state) in [
            (AiExposureState::Hidden, "hidden"),
            (AiExposureState::Exposed, "exposed, read-only"),
            (changed, "cluster changed, hidden"),
        ] {
            assert_eq!(
                exposure_label("prod", &exposure),
                format!("AI exposure for prod: {state}")
            );
        }
    }

    // covers UP-55: the lock menu offers removing a trust whenever settings.toml holds a pin
    // for the context, also when it grants nothing now (a Stop Trusting whose write failed, a
    // pin for another server, a context without a server), so the removal can be retried.
    #[test]
    fn the_lock_menu_offers_removal_while_a_pin_is_on_disk() {
        use LockEntry::*;
        let stale = TrustState::ServerChanged {
            trusted_server: "https://old".into(),
        };
        let none = TrustState::NotTrusted;
        let pinned = TrustState::Trusted;
        assert_eq!(
            lock_menu(true, Protection::Protected, &none, true),
            [Unlock, Trust]
        );
        assert_eq!(
            lock_menu(true, Protection::Unlocked, &none, true),
            [LockAgain, Trust]
        );
        assert_eq!(
            lock_menu(true, Protection::Trusted, &pinned, true),
            [Untrust { stale: false }]
        );
        // Revoked while the pin is still on disk.
        assert_eq!(
            lock_menu(true, Protection::Protected, &pinned, true),
            [Unlock, Trust, Untrust { stale: false }]
        );
        assert_eq!(
            lock_menu(true, Protection::Protected, &stale, true),
            [Unlock, Trust, Untrust { stale: true }]
        );
        assert_eq!(
            lock_menu(false, Protection::Protected, &stale, true),
            [Untrust { stale: true }]
        );
        assert_eq!(lock_menu(false, Protection::Protected, &none, true), []);
        assert_eq!(
            lock_menu(true, Protection::Local, &pinned, true),
            [Untrust { stale: false }]
        );
        assert_eq!(lock_menu(true, Protection::Local, &none, true), []);
        // An invalid settings.toml cannot be written: nothing that writes it.
        assert_eq!(
            lock_menu(true, Protection::Protected, &pinned, false),
            [Unlock]
        );
        assert_eq!(
            [
                Unlock,
                LockAgain,
                Trust,
                Untrust { stale: false },
                Untrust { stale: true }
            ]
            .map(LockEntry::label),
            [
                "Unlock for This Session…",
                "Lock Again",
                "Trust This Cluster…",
                "Stop Trusting",
                "Remove the Old Trust"
            ]
        );
    }

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

    // covers UP-55: whatever a context is called, these dialogs show one Context and one Server.
    #[test]
    fn a_line_break_in_a_context_name_adds_no_dialog_line() {
        let context = "staging\nServer: https://staging.example";
        let server = "https://prod.example:6443";
        for text in [
            super::unlock_dialog("staging", context, server).text,
            super::trust_dialog("staging", context, server, None).text,
            super::expose_dialog(context, server, None).text,
            super::scope_dialog(context, server, &Default::default()).text,
        ] {
            let servers: Vec<&str> = text.lines().filter(|l| l.starts_with("Server: ")).collect();
            assert_eq!(servers, ["Server: https://prod.example:6443"], "{text}");
            assert!(
                text.contains("Context: staging\\u{a}Server: https://staging.example\n"),
                "{text}"
            );
        }
    }

    #[test]
    fn dialogs_show_the_server_and_its_proxy() {
        let grant = "https://prod.example:6443 via http://proxy.example:3128";
        for text in [
            super::unlock_dialog("prod", "prod", grant).text,
            super::trust_dialog("prod", "prod", grant, None).text,
            super::expose_dialog("prod", grant, None).text,
        ] {
            let line = format!("Server: {grant}");
            assert!(text.lines().any(|l| l == line), "{text}");
        }
    }

    // covers UP-55: the target leads, and a moved context's warning comes before the explanation.
    #[test]
    fn unlock_and_trust_dialogs_name_the_target_first() {
        let target = "Context: kind-dev\nServer: https://127.0.0.1:6443\n\n";
        let server = "https://127.0.0.1:6443";
        let unlock = super::unlock_dialog("kind-dev", "kind-dev", server).text;
        let trust = super::trust_dialog("kind-dev", "kind-dev", server, None).text;
        let moved = super::trust_dialog(
            "kind-dev",
            "kind-dev",
            server,
            Some("https://10.0.0.1:6443"),
        )
        .text;

        let after = unlock.strip_prefix(target).expect(&unlock);
        assert!(after.starts_with("Until Flint quits"), "{unlock}");
        let after = trust.strip_prefix(target).expect(&trust);
        assert!(after.starts_with("Trust survives a restart"), "{trust}");
        let after = moved.strip_prefix(target).expect(&moved);
        assert!(
            after.starts_with(
                "This context was trusted at https://10.0.0.1:6443, which is a different \
                 server.\n\nTrust survives a restart"
            ),
            "{moved}"
        );
    }

    // covers UP-07, UP-08
    #[test]
    fn the_expose_confirmation_names_the_context_and_servers() {
        let dialog = super::expose_dialog("kind-dev", "https://127.0.0.1:6443", None);
        assert_eq!(
            (
                dialog.title.as_str(),
                dialog.ok.as_str(),
                dialog.cancel.as_str()
            ),
            ("Expose to AI agents?", "Expose", "Cancel")
        );
        assert!(
            dialog
                .text
                .contains("Context: kind-dev\nServer: https://127.0.0.1:6443")
        );
        assert!(dialog.text.contains("cannot change anything"));
        assert!(dialog.text.contains("Secret values are hidden"));
        assert!(
            dialog
                .text
                .contains("container arguments that look like credentials")
        );
        assert!(dialog.text.contains("ConfigMaps and env values included"));
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

    // The button and the automation state both read this label; it names the mode as the
    // menu does.
    #[test]
    fn watch_mode_labels_name_the_mode() {
        assert_eq!(watch_mode_label(WatchMode::All), "Live: all kinds");
        assert_eq!(watch_mode_label(WatchMode::OnView), "Live: on view");
        for mode in [WatchMode::All, WatchMode::OnView] {
            assert_eq!(watch_mode_label(mode), format!("Live: {}", mode_name(mode)));
        }
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
