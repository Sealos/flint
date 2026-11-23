//! One kind across several clusters, as a virtualized table ([`flint_core::combined`]).
//!
//! A view of its own, so a single-cluster [`super::ResourceList`] is untouched. Filtering,
//! sorting and measuring run on GPUI's background executor. Every row is addressed by
//! [`RowRef`] and reported by [`CombinedRowId`], so two clusters' objects of one name stay apart.
//!
//! The workspace opens it with [`CombinedResourceList::new`] and a [`CombinedSource`], then calls
//! [`CombinedResourceList::set_source`] for each update.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use flint_core::ClusterId;
use flint_core::combined::{
    CLUSTER_COLUMN, ColumnKey, CombinedCell, CombinedGroup, CombinedLine, CombinedQuery,
    CombinedRowId, CombinedSort, CombinedView, RowRef,
};
use flint_core::table::unix_now;
use gpui_kit::base::{TestSupportExt as _, TestTextExt as _};
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

use super::super::cluster_tag::tag_dot;
use super::super::list_model::combined::{
    self as model, CombinedSource, EmptyState, MeasuredWidths, RowSeen, TrackedRow,
};
use super::super::list_model::{self, CELL_PADDING, CHAR_WIDTH, ColumnWidths};
use super::super::registry::{
    self, CloseDetail, FocusFilter, FocusRows, OpenSelectedRow, SelectFirst, SelectFromFilter,
    SelectLast, SelectNext, SelectPrev,
};
use super::problems::{self, ClusterProblems, MemberFilter, ProblemFilter, ProblemListKind};
use super::{CONTEXT, ROW_HEIGHT, limited_text, notice, state_chip, tabular_figures};

/// Selects a row by its cluster and uid, as clicking it does.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, Action)]
#[action(namespace = combined_list, no_json)]
#[serde(deny_unknown_fields)]
pub struct SelectCombinedRow {
    pub cluster: String,
    pub uid: String,
}

/// Sorts by a column, as clicking its header does.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, Action)]
#[action(namespace = combined_list, no_json)]
#[serde(deny_unknown_fields)]
pub struct SortCombined {
    pub key: String,
    pub occurrence: u32,
}

/// Adds a cluster to the filter or takes it out, as clicking its note does.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, Action)]
#[action(namespace = combined_list, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleClusterFilter {
    pub cluster: String,
}

/// Collapses or expands a cluster's group, as clicking its header does.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, Action)]
#[action(namespace = combined_list, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleClusterGroup {
    pub cluster: String,
}

/// Turns grouping by cluster on or off.
#[derive(Debug, Clone, Default, PartialEq, Action)]
#[action(namespace = combined_list, no_json)]
pub struct ToggleGroupByCluster;

/// Shows or hides the `-o wide` columns.
#[derive(Debug, Clone, Default, PartialEq, Action)]
#[action(namespace = combined_list, no_json)]
pub struct ToggleCombinedWide;

/// Shows only the rows with problems.
#[derive(Debug, Clone, Default, PartialEq, Action)]
#[action(namespace = combined_list, no_json)]
pub struct ToggleCombinedProblems;

/// The object a selected combined row names, in the row's own cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinedObject {
    pub cluster: String,
    pub namespace: Option<String>,
    pub name: String,
    pub uid: Box<str>,
}

pub enum CombinedListEvent {
    /// A row was selected, with its own cluster, or the selection was cleared.
    Selected(Option<CombinedRowId>),
    /// The selected row was asked for again (Enter, or another click): show its detail.
    Reopened(CombinedRowId),
}

impl EventEmitter<CombinedListEvent> for CombinedResourceList {}

pub struct CombinedResourceList {
    pub(in super::super) label: String,
    /// Which members lack this kind, or can't be confirmed to serve it.
    kind_note: Option<String>,
    latest: CombinedSource,
    /// The source being drawn and its view. Always replaced together: a [`RowRef`] means
    /// nothing in any other list.
    pub(in super::super) shown: CombinedSource,
    pub(in super::super) view: Arc<CombinedView>,
    /// The filters, and the sort asked for.
    pub(in super::super) query: CombinedQuery,
    /// The sort `view` was computed with, which the header's arrow shows.
    pub(in super::super) applied_sort: Option<CombinedSort>,
    pub(in super::super) show_wide: bool,
    pub(in super::super) problems: ProblemFilter,
    widths: ColumnWidths,
    pub(in super::super) selected: Option<CombinedRowId>,
    selected_line: Option<usize>,
    /// Where the selected row was last listed, to tell it gone from no longer covered.
    selected_seen: Option<RowSeen>,
    /// Whether `view` is a finished computation, not the empty one the list opens with.
    view_ready: bool,
    /// Down in the filter while rows were computing: select the first row once they land.
    select_first_in: Option<AnyWindowHandle>,
    /// The widths of the newest list and namespace scope.
    measured: Option<MeasuredWidths>,
    computing: bool,
    dirty: bool,
    remeasure: bool,
    pub(in super::super) drawn: Vec<(usize, f32)>,
    laid_out_width: Option<f32>,
    scroll: UniformListScrollHandle,
    hscroll: ScrollHandle,
    focus: FocusHandle,
    filter: Entity<InputState>,
    _compute: Task<()>,
    _tick: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl CombinedResourceList {
    /// A combined list titled `label`. Build `source` with [`CombinedSource::new`].
    pub fn new(
        label: String,
        source: CombinedSource,
        namespaces: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter by name, cluster…"));
        let on_filter = cx.subscribe_in(&filter, window, |this, input, event, _, cx| {
            if let InputEvent::Change = event {
                this.query.text = input.read(cx).value().to_string();
                this.recompute(cx);
            }
        });
        // Ages keep counting while the window is in front.
        let tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update_in(cx, |list, window, cx| {
                    if window.is_window_active()
                        && list.view.line_count() > 0
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
        let mut list = Self {
            label,
            kind_note: None,
            shown: source.clone(),
            latest: source,
            view: Arc::new(CombinedView::default()),
            query: CombinedQuery {
                namespaces,
                ..CombinedQuery::default()
            },
            applied_sort: None,
            show_wide: false,
            problems: ProblemFilter::default(),
            widths: ColumnWidths::default(),
            selected: None,
            selected_line: None,
            selected_seen: None,
            view_ready: false,
            select_first_in: None,
            measured: None,
            computing: false,
            dirty: false,
            remeasure: false,
            drawn: Vec::new(),
            laid_out_width: None,
            scroll: UniformListScrollHandle::new(),
            hscroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            filter,
            _compute: Task::ready(()),
            _tick: tick,
            _subscriptions: vec![on_filter],
        };
        list.recompute(cx);
        list
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The selected row, with the cluster it belongs to.
    pub fn selected(&self) -> Option<&CombinedRowId> {
        self.selected.as_ref()
    }

    /// The handlers only this list has. The workspace forwards them, so they work wherever
    /// focus is in the window.
    pub fn bind_actions<B: registry::Bind<Self>>(bind: B) -> B {
        bind.bind::<SelectCombinedRow>(Self::on_select_row)
            .bind::<SortCombined>(Self::on_sort)
            .bind::<ToggleClusterFilter>(Self::on_filter_cluster)
            .bind::<ToggleClusterGroup>(Self::on_toggle_group)
            .bind::<ToggleGroupByCluster>(Self::on_group_by)
            .bind::<ToggleCombinedWide>(Self::on_wide)
            .bind::<ToggleCombinedProblems>(Self::on_problems)
    }

    /// The list actions it shares with a single-cluster list, forwarded the same way.
    pub fn bind_list_actions<B: registry::Bind<Self>>(bind: B) -> B {
        bind.bind::<SelectPrev>(Self::on_prev)
            .bind::<SelectNext>(Self::on_next)
            .bind::<SelectFirst>(Self::on_first)
            .bind::<SelectLast>(Self::on_last)
            .bind::<CloseDetail>(Self::on_close)
            .bind::<OpenSelectedRow>(Self::on_open_selected)
            .bind::<FocusFilter>(Self::on_focus_filter)
    }

    /// The filter field's focus.
    #[cfg(test)]
    pub(in super::super) fn filter_handle(&self, cx: &App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }

    /// The selected row's object, while the row is listed.
    pub fn selected_object(&self) -> Option<CombinedObject> {
        let id = self.selected.as_ref()?;
        let line = model::line_of(&self.shown.list, &self.view, id)?;
        let Some(CombinedLine::Row(at)) = self.view.line(line) else {
            return None;
        };
        let info = self.shown.list.row_info(at)?;
        Some(CombinedObject {
            cluster: id.cluster.to_string(),
            namespace: info.namespace.map(str::to_owned),
            name: info.name.to_owned(),
            uid: info.uid.into(),
        })
    }

    /// Clears the selection, as Esc does.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected_line = None;
        self.selected_seen = None;
        if self.selected.take().is_some() {
            cx.emit(CombinedListEvent::Selected(None));
            cx.notify();
        }
    }

    /// Replaces what the list shows. Filters on clusters that left the list are dropped.
    pub fn set_source(&mut self, source: CombinedSource, cx: &mut Context<Self>) {
        let known = |id: &String| source.list.members().iter().any(|m| m.cluster() == id);
        self.query.clusters.retain(known);
        self.query.collapsed.retain(known);
        self.latest = source;
        self.recompute(cx);
    }

    pub fn set_namespaces(&mut self, namespaces: Vec<String>, cx: &mut Context<Self>) {
        if self.query.namespaces != namespaces {
            self.query.namespaces = namespaces;
            self.remeasure = true;
            self.recompute(cx);
        }
    }

    pub fn set_group_by_cluster(&mut self, grouped: bool, cx: &mut Context<Self>) {
        if self.query.group_by_cluster != grouped {
            self.query.group_by_cluster = grouped;
            self.recompute(cx);
        }
    }

    /// Whether `cluster` is still a member. Its chip or header can outlive it until the next
    /// view is drawn, and a filter on it would match nothing with no chip left to clear it.
    fn is_member(&self, cluster: &str) -> bool {
        self.latest
            .list
            .members()
            .iter()
            .any(|m| m.cluster() == cluster)
    }

    pub fn toggle_cluster_filter(&mut self, cluster: &str, cx: &mut Context<Self>) {
        if !self.is_member(cluster) {
            return;
        }
        let members = self.latest.list.members().len();
        model::toggle_cluster(&mut self.query.clusters, cluster, members);
        self.recompute(cx);
    }

    pub fn toggle_group(&mut self, cluster: &str, cx: &mut Context<Self>) {
        if !self.is_member(cluster) {
            return;
        }
        let collapsed = &mut self.query.collapsed;
        match collapsed.iter().position(|c| c == cluster) {
            Some(at) => {
                collapsed.remove(at);
            }
            None => collapsed.push(cluster.to_owned()),
        }
        self.recompute(cx);
    }

    /// Sorts by `column`: ascending, descending, then the default order.
    pub fn toggle_sort(&mut self, column: &ColumnKey, cx: &mut Context<Self>) {
        let Some(at) = self.latest.list.column_index(column) else {
            return;
        };
        if self.latest.masked[at] {
            return;
        }
        self.query.sort = model::next_sort(self.query.sort.as_ref(), column);
        self.recompute(cx);
    }

    /// Starts a filter-and-sort over the newest source, unless one is running; then it runs
    /// again when that one ends, so bursts of changes coalesce.
    fn recompute(&mut self, cx: &mut Context<Self>) {
        if self.computing {
            self.dirty = true;
            return;
        }
        self.computing = true;
        self.dirty = false;
        let source = self.latest.clone();
        let sort = model::effective_sort(&source, self.query.sort.as_ref(), self.show_wide);
        let query = CombinedQuery {
            sort: sort.clone(),
            ..self.query.clone()
        };
        let selected = self.selected.clone();
        let seen = self.selected_seen.clone();
        let cached = self.measured.clone();
        let restart = std::mem::take(&mut self.remeasure);
        let job_source = source.clone();
        let keep = self.problems.member_filter(
            source.kind.as_ref(),
            source.list.members().iter().map(|m| m.cluster()),
        );
        let job = cx.background_spawn(async move {
            compute_view(
                &job_source,
                &query,
                keep.as_ref(),
                selected,
                seen,
                cached,
                unix_now(),
            )
        });
        self._compute = cx.spawn(async move |this, cx| {
            let computed = job.await;
            let select_first = this.update(cx, |list, cx| {
                list.install(computed, source, sort, restart, cx)
            });
            if let Ok(Some(window)) = select_first {
                let _ = window.update(cx, |_, window, cx| {
                    let _ = this.update(cx, |list, cx| {
                        if list.focus.is_focused(window) {
                            list.select_first_if_none(window, cx);
                        }
                    });
                });
            }
        });
    }

    /// Publishes a finished computation: the source, its view and the selection's line
    /// together, so a key never steps from a line of another view. Returns the window to
    /// select the first row in, once no further computation is due.
    fn install(
        &mut self,
        computed: Computed,
        source: CombinedSource,
        sort: Option<CombinedSort>,
        restart: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyWindowHandle> {
        self.computing = false;
        self.view_ready = true;
        self.widths
            .update(&source.columns, &computed.measured.widths, restart);
        self.measured = Some(computed.measured);
        self.shown = source;
        self.view = Arc::new(computed.view);
        self.applied_sort = sort;
        self.drawn = self.layout(self.laid_out_width);
        if self.selected == computed.selected {
            let tracked = computed.tracked.unwrap_or(TrackedRow {
                line: None,
                seen: None,
                gone: false,
            });
            self.selected_line = tracked.line;
            self.selected_seen = tracked.seen;
            if tracked.gone {
                self.selected = None;
                self.selected_line = None;
                self.selected_seen = None;
                cx.emit(CombinedListEvent::Selected(None));
            }
        } else {
            // Chosen while this ran, on the view before it.
            self.selected_line = self
                .selected
                .as_ref()
                .and_then(|id| model::line_of(&self.shown.list, &self.view, id));
            self.dirty = true;
        }
        cx.notify();
        if self.dirty {
            self.recompute(cx);
        }
        if self.computing {
            None
        } else {
            self.select_first_in.take()
        }
    }

    /// The columns to draw and their widths. The table scrolls sideways when they do not fit;
    /// spare width goes to the Name column.
    fn layout(&self, available: Option<f32>) -> Vec<(usize, f32)> {
        let columns = &self.shown.columns;
        let width = |chars: usize| chars as f32 * CHAR_WIDTH + CELL_PADDING;
        let mut drawn: Vec<(usize, f32)> = list_model::visible_columns(columns, self.show_wide)
            .into_iter()
            .map(|i| (i, width(self.widths.chars(i, &columns[i]))))
            .collect();
        if let Some(room) = available
            && let Some(name) = columns
                .iter()
                .position(|c| c.role == flint_core::ColumnRole::Name)
        {
            let wanted = width(self.widths.wanted(name, &columns[name]));
            list_model::widen_name(&mut drawn, columns, room, wanted);
        }
        drawn
    }

    fn select_line(&mut self, line: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(CombinedLine::Row(at)) = self.view.line(line) else {
            return;
        };
        let Some(id) = self.shown.list.row_id(at) else {
            return;
        };
        window.focus(&self.focus, cx);
        self.selected_line = Some(line);
        self.selected_seen = model::row_seen(&self.shown, at);
        self.scroll.scroll_to_item(line, ScrollStrategy::Top);
        if self.selected.as_ref() != Some(&id) {
            self.selected = Some(id.clone());
            cx.emit(CombinedListEvent::Selected(Some(id)));
        }
        cx.notify();
    }

    fn on_select_row(
        &mut self,
        action: &SelectCombinedRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = CombinedRowId {
            cluster: action.cluster.as_str().into(),
            uid: action.uid.as_str().into(),
        };
        let Some(line) = model::line_of(&self.shown.list, &self.view, &id) else {
            return;
        };
        if self.selected.as_ref() == Some(&id) {
            window.focus(&self.focus, cx);
            cx.emit(CombinedListEvent::Reopened(id));
        } else {
            self.select_line(line, window, cx);
        }
    }

    /// Enter on the rows: asks for the selected row again, by its cluster and uid. The key stays
    /// with a control or dialog that has focus.
    fn on_open_selected(
        &mut self,
        _: &OpenSelectedRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus.is_focused(window)
            || gpui_kit::component::WindowExt::has_active_dialog(window, cx)
        {
            cx.propagate();
            return;
        }
        let Some(id) = self.selected.clone() else {
            return;
        };
        if model::line_of(&self.shown.list, &self.view, &id).is_some() {
            cx.emit(CombinedListEvent::Reopened(id));
        }
    }

    fn on_prev(&mut self, _: &SelectPrev, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(line) = model::step_row(&self.view, self.selected_line, false) {
            self.select_line(line, window, cx);
        }
    }

    fn on_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(line) = model::step_row(&self.view, self.selected_line, true) {
            self.select_line(line, window, cx);
        }
    }

    fn on_first(&mut self, _: &SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(line) = model::edge_row(&self.view, false) {
            self.select_line(line, window, cx);
        }
    }

    fn on_last(&mut self, _: &SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(line) = model::edge_row(&self.view, true) {
            self.select_line(line, window, cx);
        }
    }

    fn select_first_if_none(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_none()
            && let Some(line) = model::edge_row(&self.view, false)
        {
            self.select_line(line, window, cx);
        }
    }

    fn on_focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.select_first_in = None;
        let focus = self.filter.focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// Back to the rows from the filter; the filter keeps its text.
    fn on_focus_rows(&mut self, _: &FocusRows, window: &mut Window, cx: &mut Context<Self>) {
        self.select_first_in = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Down in the filter: back to the rows, on the first if none is selected. While rows are
    /// computing it waits for them, so it never lands on a row the filter hides.
    fn on_select_from_filter(
        &mut self,
        _: &SelectFromFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        if self.computing {
            self.select_first_in = Some(window.window_handle());
        } else {
            self.select_first_if_none(window, cx);
        }
        cx.notify();
    }

    fn on_close(&mut self, _: &CloseDetail, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_selection(cx);
    }

    fn on_sort(&mut self, action: &SortCombined, _: &mut Window, cx: &mut Context<Self>) {
        let key = ColumnKey {
            key: action.key.clone(),
            occurrence: action.occurrence,
        };
        self.toggle_sort(&key, cx);
    }

    fn on_filter_cluster(
        &mut self,
        action: &ToggleClusterFilter,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_cluster_filter(&action.cluster, cx);
    }

    fn on_toggle_group(
        &mut self,
        action: &ToggleClusterGroup,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_group(&action.cluster, cx);
    }

    fn on_group_by(&mut self, _: &ToggleGroupByCluster, _: &mut Window, cx: &mut Context<Self>) {
        self.set_group_by_cluster(!self.query.group_by_cluster, cx);
    }

    fn on_wide(&mut self, _: &ToggleCombinedWide, _: &mut Window, cx: &mut Context<Self>) {
        self.show_wide = !self.show_wide;
        self.drawn = self.layout(self.laid_out_width);
        self.recompute(cx);
        cx.notify();
    }

    fn on_problems(&mut self, _: &ToggleCombinedProblems, _: &mut Window, cx: &mut Context<Self>) {
        self.problems.toggle();
        self.recompute(cx);
        cx.notify();
    }

    /// Takes the newest problems of the members.
    pub fn set_problems(
        &mut self,
        values: std::collections::HashMap<ClusterId, ClusterProblems>,
        cx: &mut Context<Self>,
    ) {
        if self.problems.update(values) {
            if self.problems.is_on() {
                self.recompute(cx);
            }
            cx.notify();
        }
    }

    /// Names the members whose problems could not be checked, with the toggle on.
    pub(in super::super) fn problems_note(&self) -> Option<String> {
        let kind = self.shown.kind.as_ref()?;
        let members = self.shown.list.members();
        self.problems.unchecked_note(
            kind,
            &self.label,
            members.iter().map(|m| (m.cluster(), m.display_name())),
        )
    }

    fn render_lines(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let cx: &Context<Self> = cx;
        let theme = cx.theme();
        let now = unix_now();
        let mut elements = Vec::with_capacity(range.len());
        for ix in range {
            match self.view.line(ix) {
                Some(CombinedLine::Header(group)) => {
                    elements.push(self.render_group(group, theme));
                }
                Some(CombinedLine::Row(at)) => elements.push(self.render_row(ix, at, now, theme)),
                None => break,
            }
        }
        elements
    }

    fn render_group(&self, group: CombinedGroup, theme: &Theme) -> AnyElement {
        let member = &self.shown.list.members()[group.member];
        let cluster = member.cluster().to_owned();
        h_flex()
            .id(SharedString::from(format!("group-{cluster}")))
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_2()
            .gap_2()
            .bg(theme.table_head)
            .border_b_1()
            .border_color(theme.table_row_border)
            .text_xs()
            .font_semibold()
            .cursor_pointer()
            .hover(|el| el.bg(theme.table_hover))
            .on_click(registry::on_click_in(
                self.focus.clone(),
                ToggleClusterGroup {
                    cluster: cluster.clone(),
                },
            ))
            .child(
                Icon::new(if group.collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .xsmall(),
            )
            .when_some(member.tag(), |el, tag| {
                el.child(tag_dot(&cluster, tag, theme))
            })
            .child(member.display_name().to_owned())
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .font_features(tabular_figures())
                    .child(group.count.to_string()),
            )
            .test_support()
            .into_any_element()
    }

    fn render_row(&self, ix: usize, at: RowRef, now: i64, theme: &Theme) -> AnyElement {
        let list = &self.shown.list;
        let columns = &self.shown.columns;
        let member = &list.members()[at.member()];
        let Some(info) = list.row_info(at) else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let stale = !member.state().is_current();
        let is_selected = self
            .selected
            .as_ref()
            .is_some_and(|id| &*id.cluster == member.cluster() && &*id.uid == info.uid);
        let health = self
            .shown
            .kind
            .as_ref()
            .is_some_and(|kind| list_model::has_health_columns(kind.as_str()));
        let text_of = |col: usize| match list.cell(at, col, now) {
            CombinedCell::Value(text) => text.into_owned(),
            _ => String::new(),
        };
        let finished = health && list_model::row_finished(columns, text_of);
        let ncols = columns.len().max(1);
        let mut el = h_flex()
            .id(SharedString::from(row_id(member.cluster(), info.uid)))
            .h(px(ROW_HEIGHT))
            .w_full()
            .border_b_1()
            .border_color(theme.table_row_border)
            .when(ix % 2 == 1, |el| el.bg(theme.table_even))
            .relative()
            .when(is_selected, |el| {
                el.bg(theme.table_active)
                    .child(super::super::theme::selection_edge(
                        theme.table_active_border,
                        px(0.),
                    ))
            })
            .when(!is_selected, |el| el.hover(|el| el.bg(theme.table_hover)))
            .when(stale, |el| el.text_color(theme.muted_foreground))
            .on_click(registry::on_click_in(
                self.focus.clone(),
                SelectCombinedRow {
                    cluster: member.cluster().to_owned(),
                    uid: info.uid.to_owned(),
                },
            ));
        for &(col, width) in &self.drawn {
            let cell = list.cell(at, col, now);
            let dot = match &cell {
                CombinedCell::Cluster { tag, .. } => *tag,
                _ => None,
            };
            let masked = matches!(cell, CombinedCell::Masked);
            let raw = move || match cell {
                CombinedCell::Cluster { name, .. } => SharedString::from(name.to_owned()),
                CombinedCell::Value(text) => SharedString::from(text.into_owned()),
                CombinedCell::Blank | CombinedCell::Masked => SharedString::default(),
            };
            let list_model::CellStyle {
                shown,
                tooltip,
                tone,
                numeric,
                mono,
            } = list_model::CellStyle::of(
                &columns[col],
                raw,
                width,
                list_model::RowFlags {
                    stale,
                    health: health && col != CLUSTER_COLUMN,
                    finished,
                    masked,
                },
            );
            el = el.child(
                div()
                    .id(("cell", ix * ncols + col))
                    .h_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .min_w_0()
                    .w(px(width))
                    .flex_none()
                    .when(mono, |el| el.font_family(theme.mono_font_family.clone()))
                    .map(|el| match tone {
                        list_model::Tone::Normal => el,
                        list_model::Tone::Danger => el.text_color(theme.danger),
                        list_model::Tone::Warning => el.text_color(theme.warning),
                        list_model::Tone::Muted => el.text_color(theme.muted_foreground),
                    })
                    .when(numeric, |el| {
                        el.justify_end().font_features(tabular_figures())
                    })
                    .when_some(dot, |el, tag| {
                        el.child(tag_dot(member.cluster(), tag, theme))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(shown.clone())
                            .test_text(shown),
                    )
                    .when_some(tooltip, |el, text| {
                        el.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
                    }),
            );
        }
        el.child(div().flex_1()).test_support().into_any_element()
    }

    fn render_header(&self, theme: &Theme) -> impl IntoElement {
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
        let combined = self.shown.list.columns();
        for &(col, width) in &self.drawn {
            let column = &combined[col];
            let sorted = self
                .applied_sort
                .as_ref()
                .filter(|s| s.column == column.key)
                .map(|s| s.descending);
            let sortable = !self.shown.masked[col];
            let numeric = list_model::is_numeric(&self.shown.columns[col]);
            let name = column.name.to_string();
            let tip = if column.description.is_empty() {
                name.clone()
            } else {
                format!("{name}\n{}", column.description)
            };
            let click = SortCombined {
                key: column.key.key.clone(),
                occurrence: column.key.occurrence,
            };
            let focus = self.focus.clone();
            header = header.child(
                h_flex()
                    .id(SharedString::from(header_id(&column.key)))
                    .h_full()
                    .px_2()
                    .gap_1()
                    .min_w_0()
                    .when(sortable, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.bg(theme.table_hover))
                            .on_click(registry::on_click_in(focus, click))
                    })
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
                    .test_support(),
            );
        }
        header.child(div().flex_1())
    }

    fn render_toolbar(&self, theme: &Theme) -> impl IntoElement {
        let wide = list_model::has_wide_columns(&self.shown.columns);
        h_flex()
            .h(px(super::super::workspace::HEADER_HEIGHT))
            .px_3()
            .gap_2()
            .flex_none()
            .min_w_0()
            .overflow_hidden()
            .border_b_1()
            .border_color(theme.border)
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
                    .child(self.count())
                    .test_support(),
            )
            .child(div().flex_1())
            .child(
                Button::new("toggle-group-by-cluster")
                    .xsmall()
                    .ghost()
                    .label("Group by Cluster")
                    .selected(self.query.group_by_cluster)
                    .on_click(registry::on_click_in(
                        self.focus.clone(),
                        ToggleGroupByCluster,
                    )),
            )
            .when(
                self.shown
                    .kind
                    .as_ref()
                    .is_some_and(|kind| ProblemListKind::of(kind).is_some()),
                |el| {
                    el.child(problems::toggle_button(self.problems.is_on()).on_click(
                        registry::on_click_in(self.focus.clone(), ToggleCombinedProblems),
                    ))
                },
            )
            .when(wide, |el| {
                el.child(
                    Button::new("toggle-wide")
                        .xsmall()
                        .ghost()
                        .label("Wide")
                        .selected(self.show_wide)
                        .on_click(registry::on_click_in(
                            self.focus.clone(),
                            ToggleCombinedWide,
                        )),
                )
            })
            .child(
                div()
                    .w(px(240.))
                    .min_w(px(72.))
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

    /// One note per member: its tag, name, row count and sync state. Clicking it filters the
    /// list to that cluster.
    fn render_members(&self, theme: &Theme) -> impl IntoElement {
        let list = &self.shown.list;
        let chosen = &self.query.clusters;
        h_flex()
            .id("combined-members")
            .flex_wrap()
            .flex_none()
            .px_3()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .children(list.members().iter().enumerate().map(|(i, member)| {
                let cluster = member.cluster().to_owned();
                let active = chosen.contains(&cluster);
                h_flex()
                    .id(SharedString::from(format!("member-{cluster}")))
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(active, |el| el.bg(theme.table_active))
                    .hover(|el| el.bg(theme.table_hover))
                    .on_click(registry::on_click_in(
                        self.focus.clone(),
                        ToggleClusterFilter {
                            cluster: cluster.clone(),
                        },
                    ))
                    .when_some(member.tag(), |el, tag| {
                        el.child(tag_dot(&cluster, tag, theme))
                    })
                    .child(member.display_name().to_owned())
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .font_features(tabular_figures())
                            .child(member.row_count().to_string()),
                    )
                    .child(state_chip(member.state(), self.shown.feeds[i], theme))
                    .when_some(self.shown.coverage[i].limit.as_ref(), |el, limit| {
                        let note = SharedString::from(model::limited_note(limit));
                        let detail = limit_detail(&self.label, limit);
                        el.child(
                            div()
                                .id(SharedString::from(format!("member-limited-{cluster}")))
                                .max_w(px(240.))
                                .truncate()
                                .text_color(theme.warning)
                                .child(note.clone())
                                .test_text(note)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(detail.clone()).build(window, cx)
                                })
                                .test_support(),
                        )
                    })
                    .test_support()
            }))
    }

    /// Names the members without this kind under the member notes (`None`: all have it).
    pub fn set_kind_note(&mut self, note: Option<String>, cx: &mut Context<Self>) {
        if self.kind_note != note {
            self.kind_note = note;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(in super::super) fn kind_note(&self) -> Option<&str> {
        self.kind_note.as_deref()
    }

    fn render_kind_note(&self, theme: &Theme) -> Option<impl IntoElement + use<>> {
        let note = SharedString::from(self.kind_note.clone()?);
        Some(
            div()
                .id("combined-kind-note")
                .flex_none()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.warning)
                .aria_label(note.clone())
                .child(note)
                .test_support(),
        )
    }

    /// The toolbar's count. Until the first view lands, nothing is known to be filtered out.
    pub(super) fn count(&self) -> String {
        let total = self.shown.total_rows();
        let matched = if self.view_ready {
            self.view.matched()
        } else {
            total
        };
        model::count_text(matched, total)
    }

    /// What replaces the rows, if anything. Loading until the first view lands, since the
    /// empty view the list opens with says nothing about what matches.
    pub(super) fn empty_state(&self) -> Option<EmptyState> {
        if !self.view_ready {
            return Some(EmptyState::Loading);
        }
        model::empty_state(&self.shown, &self.view, &self.query.clusters)
    }

    fn placeholder(&self, theme: &Theme) -> Option<AnyElement> {
        let label = &self.label;
        let empty = self.empty_state()?;
        let base = h_flex()
            .id("list-placeholder")
            .size_full()
            .justify_center()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(theme.muted_foreground);
        Some(
            match empty {
                EmptyState::Loading => base
                    .child(Spinner::new().small())
                    .child(format!("Loading {label}…")),
                EmptyState::NoRows => base.child(format!("No {label}")),
                EmptyState::NoRowsListed => base.child(format!(
                    "No {label} in the namespaces listed. Each cluster's note says which."
                )),
                EmptyState::NoMatch => base.child(format!("No {label} match the filters")),
                EmptyState::Unresolved => base.child(format!(
                    "Could not list {label} on every cluster. Each cluster's note says why."
                )),
            }
            .test_support()
            .into_any_element(),
        )
    }
}

/// One background computation's result, for [`CombinedResourceList::install`].
struct Computed {
    view: CombinedView,
    measured: MeasuredWidths,
    selected: Option<CombinedRowId>,
    tracked: Option<TrackedRow>,
}

fn compute_view(
    source: &CombinedSource,
    query: &CombinedQuery,
    keep: Option<&MemberFilter>,
    selected: Option<CombinedRowId>,
    seen: Option<RowSeen>,
    cached: Option<MeasuredWidths>,
    now: i64,
) -> Computed {
    let view = source.list.compute_where(query, now, |member, row| {
        keep.is_none_or(|keep| keep.keeps(member, row))
    });
    // Widths follow the namespaces only, so typing or sorting moves no column.
    let measured = model::measure_scope(source, query, &view, cached, now);
    let tracked = selected
        .as_ref()
        .map(|id| model::track_selection(source, &view, id, seen.as_ref(), now));
    Computed {
        view,
        measured,
        selected,
        tracked,
    }
}

/// What a limited member's rows cover, in full.
pub(super) fn limit_detail(label: &str, limit: &flint_core::NamespaceLimit) -> String {
    if limit.listed.is_empty() {
        format!("Could not list {label} in any namespace: {}", limit.reason)
    } else {
        limited_text(label, limit)
    }
}

/// A row's element id. Holds the cluster, since two clusters can hold one uid.
pub(in super::super) fn row_id(cluster: &str, uid: &str) -> String {
    format!("row-{cluster}/{uid}")
}

/// A header's element id, by column key, so two columns of one header stay apart.
fn header_id(key: &ColumnKey) -> String {
    format!("sort-{}-{}", key.key, key.occurrence)
}

impl Render for CombinedResourceList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = self.hscroll.bounds().size.width;
        let available = (width > px(0.)).then(|| f32::from(width));
        if available != self.laid_out_width {
            self.laid_out_width = available;
            let this = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                let _ = this.update(cx, |_, cx| cx.notify());
            });
        }
        self.drawn = self.layout(available);
        let total_width: f32 = self.drawn.iter().map(|(_, w)| w).sum();
        let lines = self.view.line_count();
        let processor = cx.processor(Self::render_lines);
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
                                .child(self.render_header(theme))
                                .child(
                                    uniform_list("rows", lines, processor)
                                        .track_scroll(&self.scroll)
                                        .flex_1()
                                        .text_sm(),
                                ),
                        ),
                )
                .child(
                    div().absolute().inset_0().child(
                        Scrollbar::horizontal(&self.hscroll)
                            .id("list-hscrollbar")
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
                )
                .into_any_element(),
        };
        v_flex()
            .id("combined-list")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_prev))
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_first))
            .on_action(cx.listener(Self::on_last))
            .on_action(cx.listener(Self::on_close))
            .on_action(cx.listener(Self::on_focus_filter))
            .on_action(cx.listener(Self::on_focus_rows))
            .on_action(cx.listener(Self::on_select_from_filter))
            .on_action(cx.listener(Self::on_select_row))
            .on_action(cx.listener(Self::on_sort))
            .on_action(cx.listener(Self::on_filter_cluster))
            .on_action(cx.listener(Self::on_toggle_group))
            .on_action(cx.listener(Self::on_group_by))
            .on_action(cx.listener(Self::on_wide))
            .size_full()
            .min_w_0()
            .relative()
            .child(self.render_toolbar(theme))
            .child(self.render_members(theme))
            .children(self.render_kind_note(theme))
            .children(self.problems_note().map(|text| {
                notice(problems::UNCHECKED_BANNER, text.clone(), theme.warning)
                    .aria_label(text)
                    .test_support()
            }))
            .child(body)
    }
}

#[cfg(test)]
#[path = "list_combined_tests.rs"]
mod tests;
