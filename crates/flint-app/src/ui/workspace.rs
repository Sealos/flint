//! The main window: the cluster sidebar, the active cluster's navigation, and the main area
//! with a header (breadcrumb, namespace filter, watch mode), the resource list, and the detail
//! pane.

use std::path::PathBuf;
use std::time::Duration;

use flint_core::redact::safe_error_summary;
use flint_core::settings::{AiExposureState, PaneWidths, State};
use flint_core::{
    ClusterId, ClusterSummary, ConnectionState, KindId, NavEntry, SyncState, WatchMode,
};
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

use super::clusters::{ClustersModel, state_label};
use super::detail::{DetailEvent, DetailTab, DetailView, ObjectRef};
use super::list::{ListEvent, ResourceList};
use super::nav::{NavEvent, NavPanel};
use super::startup::{self, StartupView};
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

/// The list keeps at least this much room when the detail pane is dragged wider.
const MIN_LIST_WIDTH: f32 = 280.;
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
    /// The last write of `state.toml`; the next one waits for it, so writes land in order.
    saving: Option<Task<()>>,
    /// Writes the pane widths once a drag pauses.
    _save_panes: Task<()>,
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
            cx.notify();
        });
        let nav = cx.new(|_| NavPanel::new());
        let on_nav = cx.subscribe_in(&nav, window, |this, _, event, window, cx| match event {
            NavEvent::Open(entry) => this.open_kind(entry, window, cx),
            NavEvent::Retry => this.retry(cx),
        });
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
                if let InputEvent::PressEnter { .. } = event {
                    let name = input.read(cx).value().trim().to_owned();
                    if name.is_empty() {
                        return;
                    }
                    let Some(cluster) = this.cluster.clone() else {
                        return;
                    };
                    let mut next = this.ui_state.namespace_filter(&cluster).to_vec();
                    next.push(name);
                    this.set_namespace_filter(ui_state::normalize(next), cx);
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
        let mut this = Self {
            model,
            forced_appearance,
            chrome: None,
            panes: ui_state.panes(),
            ui_state,
            nav,
            cluster: None,
            list: None,
            detail: None,
            namespaces: Vec::new(),
            namespaces_state: SyncState::Idle,
            namespace_input,
            startup_yaml: startup_view.as_ref().is_some_and(|v| v.yaml),
            startup_view,
            saving: None,
            _save_panes: Task::ready(()),
            _namespaces: Task::ready(()),
            _tick: tick,
            _list_subscriptions: Vec::new(),
            _detail_subscriptions: Vec::new(),
            _subscriptions: vec![observe, appearance, on_nav, on_namespace_input, on_quit],
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
        let clusters = Pane::Clusters.get(&self.panes);
        let width = match pane {
            Pane::Clusters => x,
            Pane::Navigation => x - clusters,
            Pane::Detail => {
                let nav = Pane::Navigation.get(&self.panes);
                let room = window_width - clusters - nav - MIN_LIST_WIDTH;
                (window_width - x).min(room)
            }
        };
        let before = self.panes;
        pane.set(&mut self.panes, width);
        if self.panes == before {
            return;
        }
        cx.notify();
        let panes = self.panes;
        self._save_panes = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PANE_SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                let save = this.ui_state.set_panes(panes);
                this.persist(save, cx);
            });
        });
    }

    /// Brings the navigation and main area in line with the model: the active cluster and its
    /// discovery.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            self.close_list();
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
            let wanted = &view.kind;
            let entry = entries()
                .find(|e| e.id.as_str() == wanted)
                .or_else(|| entries().find(|e| startup::kind_matches(e.id.as_str(), wanted)))
                .cloned();
            match entry {
                Some(entry) => {
                    self.open_kind(&entry, window, cx);
                    if let (Some(list), Some(name)) = (&self.list, view.name) {
                        list.update(cx, |list, cx| {
                            list.select_when_listed(view.namespace, name, cx)
                        });
                    }
                }
                None => tracing::warn!(
                    kind = view.kind,
                    "FLINT_VIEW names a kind this cluster does not serve"
                ),
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

    fn close_list(&mut self) {
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
        self.close_list();
        let core = self.model.read(cx).core().clone();
        let namespaces = self.ui_state.namespace_filter(&cluster).to_vec();
        let (kind, label) = (entry.id.clone(), entry.label.clone());
        let list = cx.new(|cx| {
            ResourceList::new(core, cluster.clone(), kind, label, namespaces, window, cx)
        });
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
                let core = self.model.read(cx).core().clone();
                let detail = cx.new(|cx| DetailView::new(core, object, tab, window, cx));
                let list = self.list.clone();
                self._detail_subscriptions =
                    vec![cx.subscribe(&detail, move |_, _, event, cx| match event {
                        DetailEvent::Close => {
                            if let Some(list) = &list {
                                list.update(cx, |list, cx| list.clear_selection(cx));
                            }
                        }
                    })];
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

    fn render_sidebar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
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
            list = list.child(
                div()
                    .id(SharedString::from(format!("kubeconfig-error-{i}")))
                    .px_2()
                    .pt_2()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(format!("{}: {summary}", display::file_name(path)))
                    .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx)),
            );
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
            .w(px(Pane::Clusters.get(&self.panes)))
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
        let state_text = match &cluster.state {
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
        let dot = match cluster.state {
            ConnectionState::Disconnected => theme.muted_foreground.opacity(0.4),
            ConnectionState::Connecting => theme.warning,
            ConnectionState::Connected(_) => theme.success,
            ConnectionState::Failed(_) => theme.danger,
        };

        let row_model = self.model.clone();
        let row_name = name.clone();

        h_flex()
            .id(SharedString::from(format!("cluster-{name}")))
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
            .on_click(move |_, _, cx| {
                row_model.update(cx, |m, _| m.activate(row_name.clone()));
            })
            .child(div().size_2().flex_none().rounded_full().bg(dot))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().line_height(px(18.)).truncate().child(title))
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(14.))
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(subtitle),
                    ),
            )
            // Neutral: the toggle next to it already shows the exposure in the accent.
            .when(exposure.is_exposed(), |row| {
                row.child(Tag::secondary().outline().xsmall().flex_none().child("AI"))
            })
            .when(
                matches!(exposure, AiExposureState::ServerChanged { .. }),
                |row| {
                    row.child(
                        div()
                            .id(SharedString::from(format!("cluster-changed-{name}")))
                            .flex_none()
                            .child(Tag::warning().outline().xsmall().child("Changed"))
                            .tooltip(|window, cx| {
                                Tooltip::new(
                                    "Cluster changed: this context points at a different server \
                                     than the one exposed to AI agents.",
                                )
                                .build(window, cx)
                            }),
                    )
                },
            )
            .child(self.render_exposure_toggle(&name, server, exposure, settings_ok))
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
        let model = self.model.clone();
        let context = name.to_owned();
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
        let Some(server) = server else {
            if matches!(exposure, AiExposureState::ServerChanged { .. }) {
                // A stale entry for a context that lost its server: it can only be removed.
                return button
                    .ghost()
                    .tooltip("Cluster changed: this context has no API server now. Click to remove its old AI exposure.")
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        model.update(cx, |m, _| m.hide_from_ai(context.clone()));
                    })
                    .into_any_element();
            }
            return button
                .ghost()
                .disabled(true)
                .tooltip("This context has no API server, so it cannot be exposed to AI.")
                .into_any_element();
        };
        match exposure {
            AiExposureState::Exposed => button
                .primary()
                .tooltip("Exposed to AI agents (flint mcp, read-only). Click to hide.")
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    model.update(cx, |m, _| m.hide_from_ai(context.clone()));
                })
                .into_any_element(),
            AiExposureState::ServerChanged { exposed_server } => {
                // Either expose the new server (after confirming) or drop the stale entry. The
                // wrapper keeps the click from also selecting the cluster row.
                let menu = button
                    .ghost()
                    .tooltip(
                        "Cluster changed: this context now points at a different server. Click \
                         to expose it again or to remove the old exposure.",
                    )
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        let (expose_model, expose_context, expose_server, previous) = (
                            model.clone(),
                            context.clone(),
                            server.clone(),
                            exposed_server.clone(),
                        );
                        let (remove_model, remove_context) = (model.clone(), context.clone());
                        menu.item(PopupMenuItem::new("Expose at the new server…").on_click(
                            move |_, window, cx| {
                                confirm_expose(
                                    window,
                                    cx,
                                    expose_model.clone(),
                                    expose_context.clone(),
                                    expose_server.clone(),
                                    Some(previous.clone()),
                                );
                            },
                        ))
                        .item(
                            PopupMenuItem::new("Remove the old exposure").on_click(
                                move |_, _, cx| {
                                    remove_model
                                        .update(cx, |m, _| m.hide_from_ai(remove_context.clone()));
                                },
                            ),
                        )
                    });
                div()
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(menu)
                    .into_any_element()
            }
            AiExposureState::Hidden => button
                .ghost()
                .tooltip("Hidden from AI agents. Click to expose it to flint mcp (read-only).")
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    confirm_expose(
                        window,
                        cx,
                        model.clone(),
                        context.clone(),
                        server.clone(),
                        None,
                    );
                })
                .into_any_element(),
        }
    }

    fn render_nav_column(&self, theme: &Theme) -> impl IntoElement {
        v_flex()
            .w(px(Pane::Navigation.get(&self.panes)))
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
        let namespaced = list.is_none_or(|l| l.shown().0.namespaced);
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
            .when(namespaced, |el| {
                el.child(self.render_namespace_filter(&ns_summary, cx))
            })
            .child(self.render_watch_mode(&name, cx))
    }

    fn render_namespace_filter(&self, summary: &str, cx: &Context<Self>) -> impl IntoElement {
        let this = cx.entity();
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
                let all = this.clone();
                v_flex()
                    .w(px(260.))
                    .gap_1()
                    .child(
                        Checkbox::new("ns-all")
                            .label("All namespaces")
                            .checked(filter.is_empty())
                            .on_click(move |_, _, cx| {
                                all.update(cx, |ws, cx| ws.set_namespace_filter(Vec::new(), cx))
                            }),
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
                                let ws = this.clone();
                                let current = filter.clone();
                                Checkbox::new(SharedString::from(format!("ns-{ns}")))
                                    .label(ns.clone())
                                    .checked(checked)
                                    .on_click(move |_, _, cx| {
                                        let next = ui_state::toggle_namespace(&current, &ns);
                                        ws.update(cx, |ws, cx| ws.set_namespace_filter(next, cx))
                                    })
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
            WatchMode::All => "Watch: all kinds",
            WatchMode::OnView => "Watch: open views",
        };
        let model = self.model.clone();
        let cluster = cluster.to_owned();
        let settings_ok = self.model.read(cx).settings_error.is_none();
        Button::new("watch-mode")
            .xsmall()
            .ghost()
            .label(label)
            .disabled(!settings_ok)
            .tooltip("Which kinds Flint watches on this cluster")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, cx| {
                let settings = model.read(cx).settings.clone();
                let own = settings.cluster_watch_mode.get(&cluster).copied();
                let global = settings.watch_mode;
                let item = |label: &str, checked: bool, target: Option<ClusterId>, mode| {
                    let model = model.clone();
                    PopupMenuItem::new(label.to_owned())
                        .checked(checked)
                        .on_click(move |_, _, cx| {
                            let target = target.clone();
                            model.update(cx, |m, _| m.set_watch_mode(target, mode));
                        })
                };
                let this = Some(cluster.clone());
                menu.label("This cluster")
                    .item(item(
                        &format!("Use the default ({})", mode_name(global)),
                        own.is_none(),
                        this.clone(),
                        None,
                    ))
                    .item(item(
                        "All kinds",
                        own == Some(WatchMode::All),
                        this.clone(),
                        Some(WatchMode::All),
                    ))
                    .item(item(
                        "Open views only",
                        own == Some(WatchMode::OnView),
                        this,
                        Some(WatchMode::OnView),
                    ))
                    .separator()
                    .label("Default for all clusters")
                    .item(item(
                        "All kinds",
                        global == WatchMode::All,
                        None,
                        Some(WatchMode::All),
                    ))
                    .item(item(
                        "Open views only",
                        global == WatchMode::OnView,
                        None,
                        Some(WatchMode::OnView),
                    ))
            })
    }

    /// The detail pane's width: as dragged, but narrower when the window is, so the list keeps
    /// [`MIN_LIST_WIDTH`] (a remembered width from a wider window would squeeze it).
    fn detail_width(&self, window_width: f32) -> f32 {
        let room = window_width
            - Pane::Clusters.get(&self.panes)
            - Pane::Navigation.get(&self.panes)
            - MIN_LIST_WIDTH;
        Pane::Detail
            .get(&self.panes)
            .min(room.max(Pane::Detail.limits().0))
    }

    fn render_main(&self, window_width: f32, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let model = self.model.read(cx);
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
                            .child(message.clone()),
                    )
                    .child(
                        div().pt_2().child(
                            Button::new("connection-retry")
                                .small()
                                .outline()
                                .label("Retry")
                                .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
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
    previous: Option<String>,
) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let mut description = format!(
            "AI agents using flint mcp will be able to read this cluster: resources, events and \
             pod logs. They cannot change anything, and Secret values stay redacted.\n\n\
             Context: {context}\nServer: {server}"
        );
        if let Some(previous) = &previous {
            description.push_str(&format!(
                "\n\nThis context was exposed at {previous}, which is a different server."
            ));
        }
        let (model, context, server) = (model.clone(), context.clone(), server.clone());
        alert
            .confirm()
            .title("Expose to AI agents?")
            .description(description)
            .ok_text("Expose")
            .cancel_text("Cancel")
            .on_ok(move |_, _, cx| {
                model.update(cx, |m, _| m.expose_to_ai(context.clone(), server.clone()));
                true
            })
    });
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
        .child("No contexts found in your kubeconfig files.")
        .child(
            div()
                .text_xs()
                .pt_1()
                .child(format!("KUBECONFIG: {kubeconfig_env}")),
        )
        .child(div().text_xs().child("Searched:"))
        .children(model.kubeconfig_paths.iter().map(|(path, exists)| {
            let exists = *exists;
            div().text_xs().pl_2().child(format!(
                "{}{}",
                path.display(),
                if exists { "" } else { " (missing)" }
            ))
        }))
        .when(model.kubeconfig_paths.is_empty(), |el| {
            el.child(div().text_xs().pl_2().child("(no paths: HOME is not set)"))
        })
        .into_any_element()
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
        let cx: &Context<Self> = cx;
        let theme = cx.theme();
        let has_cluster = self.model.read(cx).active_cluster().is_some();
        h_flex()
            .id("workspace")
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<PaneDrag>, _, cx| {
                let pane = e.drag(cx).0;
                let x = f32::from(e.event.position.x - e.bounds.origin.x);
                let width = f32::from(e.bounds.size.width);
                this.drag_pane(pane, x, width, cx);
            }))
            .child(self.render_sidebar(theme, cx))
            .when(has_cluster, |el| el.child(self.render_nav_column(theme)))
            .child(self.render_main(window_width, theme, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::namespace_summary;

    // covers UP-21
    #[test]
    fn namespace_summary_names_the_filter() {
        assert_eq!(namespace_summary(&[]), "All namespaces");
        assert_eq!(namespace_summary(&["demo".into()]), "demo");
        assert_eq!(namespace_summary(&["a".into(), "b".into()]), "2 namespaces");
    }
}
