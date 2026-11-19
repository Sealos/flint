//! The generic resource list (#9): one kind on one cluster, as a virtualized table.
//!
//! Rows come from a core [`flint_core::Subscription`]; filtering and sorting run on the core's
//! threads ([`CoreHandle::compute_view`]) and column measuring on GPUI's background executor.
//! The UI thread only lays out the rows in view (ADR-0002), so a list of 10,000 rows draws like
//! a list of 50, however fast the watch changes.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use flint_core::table::unix_now;
use flint_core::{
    Cell, ClusterId, ColumnRole, CoreHandle, Feed, KindId, KindSnapshot, ListFormat, Row,
    SyncState, ViewQuery,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _, Theme, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use flint_core::redact::is_safe_secret_column;

use super::list_model::{self, ColumnWidths, SortSpec};
use super::notice::notice;

actions!(
    resource_list,
    [SelectPrev, SelectNext, SelectFirst, SelectLast, CloseDetail]
);

const CONTEXT: &str = "ResourceList";
pub const ROW_HEIGHT: f32 = 24.;
/// Approximate advance of one character at the table's text size, for column widths.
const CHAR_WIDTH: f32 = 8.5;
const CELL_PADDING: f32 = 16.;
/// How narrow Name and Namespace get in a tight list (characters), so status columns fit.
const SQUEEZED_NAME_CHARS: usize = 12;
const SQUEEZED_NAMESPACE_CHARS: usize = 8;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, Some(CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(CONTEXT)),
        KeyBinding::new("home", SelectFirst, Some(CONTEXT)),
        KeyBinding::new("end", SelectLast, Some(CONTEXT)),
        KeyBinding::new("escape", CloseDetail, Some(CONTEXT)),
    ]);
}

/// The object a list has selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub uid: Box<str>,
    pub name: String,
    pub namespace: Option<String>,
    pub resource_version: Box<str>,
}

pub enum ListEvent {
    /// The user selected an object (open its detail), or cleared the selection.
    Selected(Option<Selected>),
    /// The selected object changed on the server: its resource version moved.
    SelectedChanged,
    /// The selected object is gone from a synced list (deleted).
    SelectedGone,
}

impl EventEmitter<ListEvent> for ResourceList {}

/// What the list is filtered and sorted by.
#[derive(Debug, Clone, Default)]
struct Query {
    namespaces: Vec<String>,
    text: String,
    sort: Option<SortSpec>,
}

pub struct ResourceList {
    core: CoreHandle,
    cluster: ClusterId,
    kind: KindId,
    label: String,
    /// The newest snapshot from the subscription.
    latest: Arc<KindSnapshot>,
    /// The snapshot being drawn, and its rows in display order (indexes into `shown.rows`).
    /// Always replaced together, so an index never points into the wrong snapshot.
    shown: Arc<KindSnapshot>,
    rows: Arc<[u32]>,
    query: Query,
    show_wide: bool,
    widths: ColumnWidths,
    selected: Option<Selected>,
    /// The selected row's position in `rows`, if it is shown.
    selected_ix: Option<usize>,
    /// The selected object disappeared from a synced list; reported once.
    selected_gone: bool,
    /// Scroll the selection into view once its position is known (`FLINT_VIEW`).
    reveal_selected: bool,
    /// An object to select as soon as it is listed (`FLINT_VIEW`): namespace and name.
    pending_select: Option<(Option<String>, String)>,
    computing: bool,
    /// Something changed while a computation ran: run again when it ends.
    dirty: bool,
    scroll: UniformListScrollHandle,
    /// The sideways scroll of the table, when its columns are wider than the pane.
    hscroll: ScrollHandle,
    /// The columns drawn this frame and their widths, computed once in `render` for the header
    /// and every row.
    drawn: Vec<(usize, f32)>,
    /// Wide columns left out this frame because the pane is too narrow.
    dropped_wide: usize,
    /// The pane width this frame was laid out for. The width is known only after layout, so
    /// when it changed, the next frame draws again with the new one.
    laid_out_width: Option<f32>,
    focus: FocusHandle,
    filter: Entity<InputState>,
    _watch: Task<()>,
    _compute: Task<()>,
    _tick: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ResourceList {
    pub fn new(
        core: CoreHandle,
        cluster: ClusterId,
        kind: KindId,
        label: String,
        namespaces: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscription = core.subscribe(&cluster, &kind);
        let latest = subscription.peek();
        let watch = cx.spawn(async move |this, cx| {
            while subscription.changed().await {
                let snapshot = subscription.latest();
                if this
                    .update(cx, |list, cx| list.set_snapshot(snapshot, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        // Ages keep counting: redraw the rows in view once a second, for kinds that show ages
        // or times and only while the window is in front.
        let tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update_in(cx, |list, window, cx| {
                    if window.is_window_active()
                        && !list.rows.is_empty()
                        && list_model::has_ticking_columns(&list.shown.columns)
                    {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by name, label…"));
        let on_filter = cx.subscribe_in(&filter, window, |this, input, event, _, cx| {
            if let InputEvent::Change = event {
                this.query.text = input.read(cx).value().to_string();
                this.recompute(cx);
            }
        });
        let mut list = ResourceList {
            core,
            cluster,
            kind,
            label,
            shown: latest.clone(),
            latest,
            rows: Arc::from(Vec::new()),
            query: Query {
                namespaces,
                ..Default::default()
            },
            show_wide: false,
            widths: ColumnWidths::default(),
            selected: None,
            selected_ix: None,
            selected_gone: false,
            reveal_selected: false,
            pending_select: None,
            computing: false,
            dirty: false,
            scroll: UniformListScrollHandle::new(),
            hscroll: ScrollHandle::new(),
            drawn: Vec::new(),
            dropped_wide: 0,
            laid_out_width: None,
            focus: cx.focus_handle(),
            filter,
            _watch: watch,
            _compute: Task::ready(()),
            _tick: tick,
            _subscriptions: vec![on_filter],
        };
        list.recompute(cx);
        list
    }

    pub fn kind(&self) -> &KindId {
        &self.kind
    }

    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The snapshot being drawn and how many of its rows pass the filters.
    pub fn shown(&self) -> (&Arc<KindSnapshot>, usize) {
        (&self.shown, self.rows.len())
    }

    /// Drops the core subscription, so a test can feed snapshots by hand.
    #[cfg(test)]
    pub fn stop_watching(&mut self) {
        self._watch = Task::ready(());
    }

    pub fn set_namespaces(&mut self, namespaces: Vec<String>, cx: &mut Context<Self>) {
        if self.query.namespaces != namespaces {
            self.query.namespaces = namespaces;
            self.recompute(cx);
        }
    }

    /// Feeds a snapshot in, as the subscription does. Tests use it to draw synthetic rows.
    pub fn set_snapshot(&mut self, snapshot: Arc<KindSnapshot>, cx: &mut Context<Self>) {
        self.latest = snapshot;
        if let Some((namespace, name)) = self.pending_select.take() {
            if find_row(&self.latest, namespace.as_deref(), &name).is_some() {
                self.select_named(namespace.as_deref(), &name, cx);
            } else {
                self.pending_select = Some((namespace, name));
            }
        }
        // The state shows at once, even before the rows are recomputed; the rows redraw when
        // the recompute lands.
        let shown = &self.shown;
        let state_changed = shown.state != self.latest.state
            || shown.feed != self.latest.feed
            || shown.limited_to != self.latest.limited_to;
        self.recompute(cx);
        if state_changed {
            cx.notify();
        }
    }

    /// Selects the object with `name` in `namespace` as soon as it is listed, as a click would.
    /// For `FLINT_VIEW`.
    pub fn select_when_listed(
        &mut self,
        namespace: Option<String>,
        name: String,
        cx: &mut Context<Self>,
    ) {
        self.pending_select = Some((namespace, name));
        let latest = self.latest.clone();
        self.set_snapshot(latest, cx);
    }

    fn select_named(&mut self, namespace: Option<&str>, name: &str, cx: &mut Context<Self>) {
        let Some(row) = find_row(&self.latest, namespace, name) else {
            return;
        };
        let row = &self.latest.rows[row];
        self.selected = Some(Selected {
            uid: row.uid.clone(),
            name: row.name.to_string(),
            namespace: row.namespace.as_deref().map(str::to_owned),
            resource_version: row.resource_version.clone(),
        });
        self.selected_ix = None;
        self.selected_gone = false;
        self.reveal_selected = true;
        cx.emit(ListEvent::Selected(self.selected.clone()));
        self.recompute(cx);
    }

    /// Starts a filter-and-sort over the newest snapshot, unless one is running; then it runs
    /// again when that one ends, so bursts of changes coalesce into one recompute at a time.
    fn recompute(&mut self, cx: &mut Context<Self>) {
        if self.computing {
            self.dirty = true;
            return;
        }
        self.computing = true;
        self.dirty = false;
        let snapshot = self.latest.clone();
        let query = ViewQuery {
            namespaces: self.query.namespaces.clone(),
            text: self.query.text.clone(),
            sort: list_model::resolve_sort(&snapshot.columns, self.query.sort.as_ref()),
        };
        let view = self.core.compute_view(snapshot.clone(), query);
        let selected_uid = self.selected.as_ref().map(|s| s.uid.clone());
        let job_snapshot = snapshot.clone();
        let job = cx.background_spawn(async move {
            let rows = view.await;
            let widths = list_model::measure_columns(&job_snapshot, unix_now());
            let position = selected_uid.as_ref().and_then(|uid| {
                rows.iter()
                    .position(|&i| job_snapshot.rows[i as usize].uid == *uid)
            });
            (rows, widths, selected_uid, position)
        });
        self._compute = cx.spawn(async move |this, cx| {
            let (rows, widths, uid, position) = job.await;
            let _ = this.update(cx, |list, cx| {
                list.computing = false;
                list.apply(snapshot, rows, &widths, uid, position, cx);
                if list.dirty {
                    list.recompute(cx);
                }
            });
        });
    }

    fn apply(
        &mut self,
        snapshot: Arc<KindSnapshot>,
        rows: Arc<[u32]>,
        widths: &[usize],
        uid: Option<Box<str>>,
        position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.widths.update(&snapshot.columns, widths);
        self.shown = snapshot;
        self.rows = rows;
        match &mut self.selected {
            None => self.selected_ix = None,
            Some(selected) if Some(&selected.uid) == uid.as_ref() => {
                self.selected_ix = position;
                if let Some(ix) = position.filter(|_| self.reveal_selected) {
                    self.reveal_selected = false;
                    self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
                }
                let found = find_row(&self.shown, selected.namespace.as_deref(), &selected.name)
                    .map(|i| &self.shown.rows[i])
                    .filter(|row| row.uid == selected.uid);
                match found {
                    Some(row) => {
                        self.selected_gone = false;
                        if row.resource_version != selected.resource_version {
                            selected.resource_version = row.resource_version.clone();
                            cx.emit(ListEvent::SelectedChanged);
                        }
                    }
                    // Only a synced list proves the object is gone; a reconnecting one may be
                    // empty or stale.
                    None if self.shown.state.is_current() && !self.selected_gone => {
                        self.selected_gone = true;
                        cx.emit(ListEvent::SelectedGone);
                    }
                    None => {}
                }
            }
            // The selection changed while this ran: find it again.
            Some(_) => self.dirty = true,
        }
        cx.notify();
    }

    fn select_index(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&row_ix) = self.rows.get(ix) else {
            return;
        };
        let row = &self.shown.rows[row_ix as usize];
        let selected = Selected {
            uid: row.uid.clone(),
            name: row.name.to_string(),
            namespace: row.namespace.as_deref().map(str::to_owned),
            resource_version: row.resource_version.clone(),
        };
        window.focus(&self.focus, cx);
        // A user's choice wins over a startup selection still waiting for its object.
        self.pending_select = None;
        self.selected_ix = Some(ix);
        self.selected_gone = false;
        self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
        if self.selected.as_ref() != Some(&selected) {
            self.selected = Some(selected.clone());
            cx.emit(ListEvent::Selected(Some(selected)));
        }
        cx.notify();
    }

    /// The position of the row with `uid` in the drawn order. Linear, so only for clicks.
    fn position_of(&self, uid: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|&i| &*self.shown.rows[i as usize].uid == uid)
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selected.take().is_some() {
            self.selected_ix = None;
            cx.emit(ListEvent::Selected(None));
            cx.notify();
        }
    }

    fn select_prev(&mut self, _: &SelectPrev, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_ix.map_or(0, |i| i.saturating_sub(1));
        self.select_index(ix, window, cx);
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.rows.len().saturating_sub(1);
        let ix = self.selected_ix.map_or(0, |i| (i + 1).min(last));
        self.select_index(ix, window, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.select_index(0, window, cx);
    }

    fn select_last(&mut self, _: &SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        self.select_index(self.rows.len().saturating_sub(1), window, cx);
    }

    fn close_detail(&mut self, _: &CloseDetail, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_selection(cx);
    }

    fn toggle_sort(&mut self, column: &str, cx: &mut Context<Self>) {
        self.query.sort = list_model::next_sort(self.query.sort.as_ref(), column);
        self.recompute(cx);
        cx.notify();
    }

    /// The columns to draw and their widths, for a pane `available` points wide (unknown
    /// before the first layout). Wide columns that do not fit are left out.
    fn layout(&self, available: Option<f32>) -> (Vec<(usize, f32)>, usize) {
        let columns = &self.shown.columns;
        let (mut drawn, dropped) = list_model::fit_columns(
            columns,
            |i| self.widths.chars(i, &columns[i]) as f32 * CHAR_WIDTH + CELL_PADDING,
            self.show_wide,
            available,
        );
        if let Some(room) = available {
            let min = |chars: usize| chars as f32 * CHAR_WIDTH + CELL_PADDING;
            list_model::squeeze_identity(
                &mut drawn,
                columns,
                room,
                min(SQUEEZED_NAME_CHARS),
                min(SQUEEZED_NAMESPACE_CHARS),
            );
        }
        (drawn, dropped)
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let cx: &Context<Self> = cx;
        let theme = cx.theme();
        let now = unix_now();
        let stale = !self.shown.state.is_current();
        // Secret rows show only identity columns, as the MCP server does: a printer column
        // added by an extension could show a value.
        let secrets = self.kind.as_str() == "secrets";
        let health = list_model::has_health_columns(self.kind.as_str());
        let selected_uid = self.selected.as_ref().map(|s| &*s.uid);
        let ncols = self.shown.columns.len().max(1);
        let mut elements = Vec::with_capacity(range.len());
        for ix in range {
            let Some(&row_ix) = self.rows.get(ix) else {
                break;
            };
            let row = &self.shown.rows[row_ix as usize];
            let is_selected = selected_uid == Some(&*row.uid);
            let mut el = h_flex()
                .id(SharedString::from(format!("row-{}", row.uid)))
                .h(px(ROW_HEIGHT))
                .w_full()
                .border_b_1()
                .border_color(theme.table_row_border)
                .when(ix % 2 == 1, |el| el.bg(theme.table_even))
                .relative()
                .when(is_selected, |el| {
                    el.bg(theme.table_active)
                        .child(super::theme::selection_edge(
                            theme.table_active_border,
                            px(0.),
                        ))
                })
                .when(!is_selected, |el| el.hover(|el| el.bg(theme.table_hover)))
                .when(stale, |el| el.text_color(theme.muted_foreground))
                .on_click({
                    // By uid, not position: rows may have been reordered by the time the
                    // click is handled.
                    let uid = row.uid.clone();
                    cx.listener(move |this, _, window, cx| {
                        if let Some(ix) = this.position_of(&uid) {
                            this.select_index(ix, window, cx);
                        }
                    })
                });
            let finished = health
                && list_model::row_finished(&self.shown.columns, |i| {
                    cell_text(row, i, now).to_string()
                });
            for &(col, width) in &self.drawn {
                let column = &self.shown.columns[col];
                let text = if secrets && !is_safe_secret_column(&column.name) {
                    SharedString::new_static(super::object::MASK)
                } else {
                    cell_text(row, col, now)
                };
                // Stale rows are already muted; masked secrets have no status.
                let tone = if stale || !health {
                    list_model::Tone::Normal
                } else {
                    list_model::cell_tone(&column.name, &text, finished)
                };
                let numeric = list_model::is_numeric(column);
                let is_name = column.role == ColumnRole::Name;
                let fits = (text.chars().count() as f32) * CHAR_WIDTH + CELL_PADDING <= width;
                let cell = div()
                    .id(("cell", ix * ncols + col))
                    .h_full()
                    .flex()
                    .items_center()
                    .px_2()
                    .min_w_0()
                    .w(px(width))
                    .flex_none()
                    .when(is_name, |el| el.font_family(theme.mono_font_family.clone()))
                    .map(|el| match tone {
                        list_model::Tone::Normal => el,
                        list_model::Tone::Danger => el.text_color(theme.danger),
                        list_model::Tone::Warning => el.text_color(theme.warning),
                        list_model::Tone::Muted => el.text_color(theme.muted_foreground),
                    })
                    .when(numeric, |el| {
                        el.justify_end().font_features(tabular_figures())
                    })
                    .child(div().min_w_0().truncate().child(text.clone()))
                    .when(!fits, |el| {
                        el.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
                    });
                el = el.child(cell);
            }
            elements.push(el.child(div().flex_1()).test_support().into_any_element());
        }
        elements
    }

    fn render_header(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let mut header = h_flex()
            .h(px(ROW_HEIGHT + 2.))
            .w_full()
            .flex_none()
            .bg(theme.table_head)
            .text_color(theme.table_head_foreground)
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .font_semibold();
        for &(col, width) in &self.drawn {
            let column = &self.shown.columns[col];
            let name = column.name.to_string();
            let sorted = self
                .query
                .sort
                .as_ref()
                .filter(|s| s.column == name)
                .map(|s| s.descending);
            let numeric = list_model::is_numeric(column);
            let tip = if column.description.is_empty() {
                name.clone()
            } else {
                format!("{name}\n{}", column.description)
            };
            let click_name = name.clone();
            header = header.child(
                h_flex()
                    .id(SharedString::from(format!("sort-{name}")))
                    .h_full()
                    .px_2()
                    .gap_1()
                    .min_w_0()
                    .cursor_pointer()
                    .hover(|el| el.bg(theme.table_hover))
                    .w(px(width))
                    .flex_none()
                    .when(numeric, |el| el.justify_end())
                    .child(div().truncate().child(name.to_uppercase()))
                    .when_some(sorted, |el, descending| {
                        el.child(
                            Icon::new(if descending {
                                IconName::SortDescending
                            } else {
                                IconName::SortAscending
                            })
                            .xsmall()
                            .text_color(theme.foreground),
                        )
                    })
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_sort(&click_name, cx);
                    }))
                    .test_support(),
            );
        }
        header.child(div().flex_1())
    }

    fn render_toolbar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let snapshot = &self.shown;
        let total = snapshot.rows.len();
        let shown = self.rows.len();
        let count = if shown == total {
            format!("{total}")
        } else {
            format!("{shown} of {total}")
        };
        let wide = list_model::has_wide_columns(&snapshot.columns);
        let dropped = self.dropped_wide;
        h_flex()
            .h(px(super::workspace::HEADER_HEIGHT))
            .px_3()
            .gap_2()
            .flex_none()
            .min_w_0()
            .overflow_hidden()
            .border_b_1()
            .border_color(theme.border)
            // When the pane is narrow the filter gives way first, down to its minimum; only then
            // does the kind's name truncate, so the filter stays usable (UP-21).
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .flex_shrink(1.)
                    .min_w(px(40.))
                    .max_w(px(200.))
                    .truncate()
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .id("row-count")
                    .text_xs()
                    .flex_none()
                    .font_features(tabular_figures())
                    .text_color(theme.muted_foreground)
                    .child(count),
            )
            .child(state_chip(&self.latest.state, self.latest.feed, theme))
            .child(div().flex_1())
            .when(wide, |el| {
                el.child(
                    Button::new("toggle-wide")
                        .xsmall()
                        .ghost()
                        .label(if dropped > 0 {
                            format!("Wide ({dropped} hidden)")
                        } else {
                            "Wide".to_owned()
                        })
                        .selected(self.show_wide)
                        .tooltip(if dropped > 0 {
                            "Some -o wide columns do not fit; widen the list to see them"
                        } else {
                            "Show the extra columns of kubectl get -o wide"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_wide = !this.show_wide;
                            cx.notify();
                        })),
                )
            })
            .child(
                // Shrinks with the pane, down to a usable minimum.
                div()
                    .w(px(240.))
                    .min_w(px(72.))
                    // Far more willing to shrink than the title, so it reaches its minimum first.
                    .flex_shrink(1000.)
                    .overflow_hidden()
                    .child(
                        Input::new(&self.filter)
                            .id("list-filter")
                            .small()
                            .cleanable(true)
                            .prefix(Icon::new(IconName::Search).small()),
                    ),
            )
    }

    /// The message shown instead of rows, if any.
    fn placeholder(&self, theme: &Theme) -> Option<AnyElement> {
        let snapshot = &self.shown;
        let label = &self.label;
        let message = |title: String, detail: Option<String>, color: Hsla| {
            v_flex()
                .id("list-placeholder")
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .p_6()
                .child(div().text_sm().text_color(color).child(title))
                .when_some(detail, |el, detail| {
                    el.child(
                        div()
                            .max_w(px(560.))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    )
                })
                .test_support()
                .into_any_element()
        };
        match &snapshot.state {
            SyncState::Idle | SyncState::Loading if snapshot.rows.is_empty() => Some(
                h_flex()
                    .id("list-placeholder")
                    .size_full()
                    .justify_center()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(Spinner::new().small())
                    .child(format!("Loading {label}…"))
                    .test_support()
                    .into_any_element(),
            ),
            SyncState::Unavailable => Some(message(
                format!("This cluster does not serve {label}."),
                Some(format!("{} is not in its API discovery.", self.kind)),
                theme.muted_foreground,
            )),
            SyncState::Failed { error } if snapshot.rows.is_empty() => Some(message(
                format!("Could not list {label}"),
                Some(match &snapshot.limited_to {
                    // Nothing to fall back to: the error says how to pick namespaces.
                    Some(_) => error.clone(),
                    None => format!("{error}\nFlint retries in the background."),
                }),
                theme.danger,
            )),
            SyncState::Reconnecting { error } if snapshot.rows.is_empty() => Some(message(
                format!("Reconnecting to list {label}…"),
                error.clone(),
                theme.warning,
            )),
            _ if self.rows.is_empty() => {
                let filtered = !self.query.text.trim().is_empty()
                    || (snapshot.namespaced && !self.query.namespaces.is_empty());
                Some(if snapshot.rows.is_empty() {
                    message(format!("No {label}"), None, theme.muted_foreground)
                } else if filtered {
                    message(
                        format!("No {label} match the filters"),
                        Some(format!(
                            "{} hidden by the namespace or text filter.",
                            snapshot.rows.len()
                        )),
                        theme.muted_foreground,
                    )
                } else {
                    message(format!("No {label}"), None, theme.muted_foreground)
                })
            }
            _ => None,
        }
    }

    /// Banners over the rows: not current (reconnecting, failing), limited to some namespaces
    /// by permissions, or without server columns.
    fn banners(&self, theme: &Theme) -> Vec<AnyElement> {
        let snapshot = &self.shown;
        let mut banners = Vec::new();
        if let Some(limit) = &snapshot.limited_to
            && !limit.listed.is_empty()
        {
            banners.push(
                notice("list-limited", limited_text(&self.label, limit), theme.info)
                    .test_support()
                    .into_any_element(),
            );
        }
        let state = match &snapshot.state {
            SyncState::Reconnecting { error } => Some((
                theme.warning,
                match error {
                    Some(e) => format!("Reconnecting. These rows may be out of date: {e}"),
                    None => "Reconnecting. These rows may be out of date.".to_owned(),
                },
            )),
            SyncState::Failed { error } if !snapshot.rows.is_empty() => Some((
                theme.danger,
                format!("The list failed and is retried. These rows may be out of date: {error}"),
            )),
            _ if snapshot.format == ListFormat::Fallback => Some((
                theme.muted_foreground,
                "This API returned no table: showing name, namespace and age only.".to_owned(),
            )),
            _ => None,
        };
        if let Some((color, text)) = state {
            banners.push(
                notice("list-banner", text, color)
                    .test_support()
                    .into_any_element(),
            );
        }
        banners
    }
}

/// "Showing Pods in 2 namespaces only (demo, team-a): you may not list Pods in all namespaces."
pub fn limited_text(label: &str, limit: &flint_core::NamespaceLimit) -> String {
    let names = limit.listed.join(", ");
    let count = match limit.listed.len() {
        1 => "1 namespace".to_owned(),
        n => format!("{n} namespaces"),
    };
    let mut text = format!(
        "Showing {label} in {count} only ({names}): you may not list {label} in all namespaces."
    );
    if !limit.denied.is_empty() {
        text.push_str(&format!(" Not allowed in: {}.", limit.denied.join(", ")));
    }
    text
}

/// A cell's text, without copying text the row already shares.
fn cell_text(row: &Row, col: usize, now: i64) -> SharedString {
    match row.cells.get(col) {
        Some(Cell::Text(text)) => SharedString::from(text.clone()),
        _ => SharedString::from(row.cell_text(col, now).into_owned()),
    }
}

/// The row of `name` in `namespace`, found by binary search (rows are sorted by namespace,
/// then name).
pub fn find_row(snapshot: &KindSnapshot, namespace: Option<&str>, name: &str) -> Option<usize> {
    let range = snapshot.namespace_range(namespace.unwrap_or(""));
    let start = range.start;
    snapshot.rows[range]
        .binary_search_by(|r| (*r.name).cmp(name))
        .ok()
        .map(|i| start + i)
}

/// Tabular figures, so digits line up in numeric columns.
pub fn tabular_figures() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// A short, subtle description of a list's sync state: a green "live" for a healthy watched
/// list, "polled" for APIs that cannot be watched, and the problem otherwise.
fn state_chip(state: &SyncState, feed: Feed, theme: &Theme) -> AnyElement {
    let (text, color, detail) = match state {
        SyncState::Synced if feed == Feed::Poll => (
            "polled",
            theme.muted_foreground,
            Some("This API cannot be watched; it is listed again on an interval.".to_owned()),
        ),
        SyncState::Synced => ("live", theme.success, None),
        SyncState::Idle => ("not watched", theme.muted_foreground, None),
        SyncState::Loading => ("loading", theme.muted_foreground, None),
        SyncState::Reconnecting { error } => ("reconnecting", theme.warning, error.clone()),
        SyncState::Failed { error } => ("failed", theme.danger, Some(error.clone())),
        SyncState::Unavailable => ("unavailable", theme.muted_foreground, None),
    };
    h_flex()
        .id("sync-state")
        .gap_1()
        .flex_none()
        .text_xs()
        .text_color(color)
        .child(div().size_1p5().rounded_full().bg(color))
        .child(text)
        .when_some(detail, |el, detail| {
            el.tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
        })
        .into_any_element()
}

impl Render for ResourceList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The pane width from the last layout decides which wide columns fit. While it keeps
        // changing (a pane drag, a window resize), draw again next frame with the new width,
        // until it settles.
        let width = self.hscroll.bounds().size.width;
        let available = (width > px(0.)).then(|| f32::from(width));
        if available != self.laid_out_width {
            self.laid_out_width = available;
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
        let (drawn, dropped) = self.layout(available);
        self.drawn = drawn;
        self.dropped_wide = dropped;
        let total_width: f32 = self.drawn.iter().map(|(_, w)| w).sum();

        let cx: &Context<Self> = cx;
        let theme = cx.theme();
        let body = match self.placeholder(theme) {
            Some(placeholder) => placeholder,
            None => div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    div()
                        .id("list-hscroll")
                        .size_full()
                        .overflow_x_scroll()
                        .track_scroll(&self.hscroll)
                        .child(
                            v_flex()
                                .h_full()
                                .w_full()
                                .min_w(px(total_width))
                                .child(self.render_header(theme, cx))
                                .child(
                                    uniform_list(
                                        "rows",
                                        self.rows.len(),
                                        cx.processor(Self::render_rows),
                                    )
                                    .track_scroll(&self.scroll)
                                    .flex_1()
                                    .text_sm(),
                                ),
                        ),
                )
                // Visible whenever the columns are wider than the pane, so it is clear that
                // more columns are to the right.
                .when(available.is_some_and(|w| total_width > w), |el| {
                    el.child(
                        div().absolute().inset_0().child(
                            Scrollbar::horizontal(&self.hscroll)
                                .id("list-hscrollbar")
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                        ),
                    )
                })
                .into_any_element(),
        };
        v_flex()
            .id("resource-list")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::close_detail))
            .size_full()
            .min_w_0()
            .child(self.render_toolbar(theme, cx))
            .children(self.banners(theme))
            .child(body)
    }
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
