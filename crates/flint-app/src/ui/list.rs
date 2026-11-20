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
    Cell, ClusterId, ColumnRole, CoreHandle, Feed, KindId, KindSnapshot, ListFormat, NavEntry, Row,
    SyncState, ViewQuery,
};
use gpui_kit::base::{TestSupportExt as _, TestTextExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
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
use super::row_menu::RowMenu;

use super::registry::{
    self, CloseDetail, SelectFirst, SelectLast, SelectNext, SelectPrev, SelectRow, SortBy,
    ToggleWide,
};

/// The list's key context; the registry binds the list's keys in it.
pub const CONTEXT: &str = "ResourceList";
pub const ROW_HEIGHT: f32 = 24.;
/// Approximate advance of one character at the table's text size, for column widths.
const CHAR_WIDTH: f32 = 8.5;
const CELL_PADDING: f32 = 16.;
/// How narrow Name and Namespace get in a tight list (characters), so status columns fit. The
/// Namespace column keeps room for its header and the sort arrow, as measured.
const SQUEEZED_NAME_CHARS: usize = 12;
const SQUEEZED_NAMESPACE_CHARS: usize = 11;

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
    /// The user changed the sort (`None`: back to the default order), to remember for the
    /// cluster and kind.
    SortChanged(Option<SortSpec>),
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
    /// Whether the kind is namespaced, as discovery says: known before the first rows arrive.
    namespaced: bool,
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
    /// The pending object is waited for only until the first current snapshot (the resource
    /// switcher, Go Back), not for good (`FLINT_VIEW`).
    pending_once: bool,
    /// An object asked for ([`ResourceList::select_listed`]) that the list did not have: what
    /// the list says about it, until another selection.
    missing: Option<(Option<String>, String)>,
    /// The version of the snapshot the subscription started with. That one may be a watch's
    /// last publish from long ago (a kind viewed earlier keeps it), so an object is judged gone
    /// only on a snapshot published since.
    subscribed_version: u64,
    /// For a namespace-limited list, which list of each namespace the subscription started
    /// with: a merged list published since may still hold a namespace's old rows.
    subscribed_lanes: Option<Arc<flint_core::NamespaceLimit>>,
    /// The revision of the rows the pending object was last seen in (Go Back), if known: a
    /// list newer than that without it proves it gone, also one the subscription started with.
    pending_seen: Option<u64>,
    /// The revision of the newest rows the selected object was seen in ([`rows_revision`]),
    /// for Go Back.
    selected_seen: Option<u64>,
    computing: bool,
    /// Something changed while a computation ran: run again when it ends.
    dirty: bool,
    /// The namespace or text filter changed: the next measurement starts the column widths
    /// over, so they fit the rows now shown. Live updates under the same filters only grow them.
    remeasure: bool,
    /// Rows of the shown snapshot in the selected namespaces (all rows with no filter), which
    /// the count is out of.
    scoped: usize,
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
    /// Whether the last recompute applied the sort: its column was drawn then. When that
    /// changes with the layout (Wide toggled, the pane resized), the rows are sorted again.
    sort_drawn: bool,
    /// Tests: each filter-and-sort job waits for a permit from here before it finishes
    /// ([`ResourceList::hold_computes`]).
    #[cfg(test)]
    compute_gate: Option<async_channel::Receiver<()>>,
    /// Tests: how many filter-and-sort jobs have come back (applied or dropped).
    #[cfg(test)]
    computes_done: usize,
    focus: FocusHandle,
    filter: Entity<InputState>,
    /// A row's context menu, for kinds that have one (pods and workloads: UP-47).
    row_menu: Option<RowMenu>,
    _watch: Task<()>,
    _compute: Task<()>,
    _tick: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ResourceList {
    /// A list of `entry`'s kind on `cluster`, filtered to `namespaces` and sorted by `sort` (the
    /// one remembered for the cluster and kind, if any).
    pub fn new(
        core: CoreHandle,
        cluster: ClusterId,
        entry: &NavEntry,
        namespaces: Vec<String>,
        sort: Option<SortSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = entry.id.clone();
        let mut subscription = core.subscribe(&cluster, &kind);
        let latest = subscription.peek();
        let subscribed_version = latest.version;
        let subscribed_lanes = latest.limited_to.clone();
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
                this.remeasure = true;
                this.recompute(cx);
            }
        });
        let mut list = ResourceList {
            core,
            cluster,
            kind,
            label: entry.label.clone(),
            namespaced: entry.namespaced,
            shown: latest.clone(),
            latest,
            rows: Arc::from(Vec::new()),
            query: Query {
                namespaces,
                sort,
                ..Default::default()
            },
            show_wide: false,
            widths: ColumnWidths::default(),
            selected: None,
            selected_ix: None,
            selected_gone: false,
            reveal_selected: false,
            pending_select: None,
            pending_once: false,
            missing: None,
            subscribed_version,
            subscribed_lanes,
            pending_seen: None,
            selected_seen: None,
            computing: false,
            dirty: false,
            remeasure: false,
            scoped: 0,
            scroll: UniformListScrollHandle::new(),
            hscroll: ScrollHandle::new(),
            drawn: Vec::new(),
            dropped_wide: 0,
            laid_out_width: None,
            sort_drawn: false,
            #[cfg(test)]
            compute_gate: None,
            #[cfg(test)]
            computes_done: 0,
            focus: cx.focus_handle(),
            filter,
            row_menu: None,
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

    /// Whether the kind is namespaced (known before any rows are listed).
    pub fn namespaced(&self) -> bool {
        self.namespaced
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

    /// Gives each row a context menu built by `menu`; right-clicking a row selects it first.
    pub fn set_row_menu(&mut self, menu: RowMenu) {
        self.row_menu = Some(menu);
    }

    /// The snapshot being drawn and how many of its rows pass the filters, for tests.
    #[cfg(test)]
    pub fn shown(&self) -> (&Arc<KindSnapshot>, usize) {
        (&self.shown, self.rows.len())
    }

    /// Holds every filter-and-sort job started from now on until the test sends a permit on the
    /// returned sender, one job per permit, so a test can act while a job is pending.
    #[cfg(test)]
    pub fn hold_computes(&mut self) -> async_channel::Sender<()> {
        let (permits, gate) = async_channel::unbounded();
        self.compute_gate = Some(gate);
        permits
    }

    /// How many filter-and-sort jobs have come back, applied or dropped, for tests.
    #[cfg(test)]
    pub fn computes_done(&self) -> usize {
        self.computes_done
    }

    /// Drops the core subscription, so a test can feed snapshots by hand.
    #[cfg(test)]
    pub fn stop_watching(&mut self) {
        self._watch = Task::ready(());
    }

    pub fn set_namespaces(&mut self, namespaces: Vec<String>, cx: &mut Context<Self>) {
        if self.query.namespaces != namespaces {
            self.query.namespaces = namespaces;
            self.remeasure = true;
            self.recompute(cx);
        }
    }

    /// Feeds a snapshot in, as the subscription does. Tests use it to draw synthetic rows.
    pub fn set_snapshot(&mut self, snapshot: Arc<KindSnapshot>, cx: &mut Context<Self>) {
        self.latest = snapshot;
        if let Some((namespace, name)) = self.pending_select.take() {
            if find_row(&self.latest, namespace.as_deref(), &name).is_some() {
                self.select_named(namespace.as_deref(), &name, cx);
            } else if self.pending_once
                && self.latest.state.is_current()
                && self.listed_since_subscribed(namespace.as_deref())
            {
                // Listed in full since the list opened, and not there: it is gone. Stop
                // waiting, so an object made later with the name is not selected out of the
                // blue.
                self.missing = Some((namespace, name));
                cx.notify();
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
        self.pending_once = false;
        self.missing = None;
        self.pending_select = Some((namespace, name));
        let latest = self.latest.clone();
        self.set_snapshot(latest, cx);
    }

    /// Selects the object with `name` in `namespace` once the list is current, as a click
    /// would: the resource switcher and Go Back. The selection shown before is cleared at once;
    /// if the first current list lacks the object, the list says it no longer exists and stops
    /// waiting.
    ///
    /// `seen`: the revision of the rows it was last seen in ([`ResourceList::selected_seen`]),
    /// when known (Go Back): rows newer than that without it prove it gone, even if they are
    /// the rows the list subscribed with.
    pub fn select_listed(
        &mut self,
        namespace: Option<String>,
        name: String,
        seen: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if self
            .selected
            .as_ref()
            .is_some_and(|s| s.namespace != namespace || s.name != name)
        {
            self.clear_selection(cx);
        }
        self.missing = None;
        self.pending_once = true;
        self.pending_seen = seen;
        self.pending_select = Some((namespace, name));
        let latest = self.latest.clone();
        self.set_snapshot(latest, cx);
    }

    /// Whether the newest snapshot lists `namespace` (or the whole kind) anew since the list
    /// subscribed, so an object missing from it is gone. A namespace-limited list is merged
    /// from one list per namespace, and may be published again from a namespace's old rows:
    /// that namespace's own list must be newer.
    fn listed_since_subscribed(&self, namespace: Option<&str>) -> bool {
        if let Some(seen) = self.pending_seen {
            return rows_revision(&self.latest, namespace).is_some_and(|now| now > seen);
        }
        if self.latest.version <= self.subscribed_version {
            return false;
        }
        let (Some(limit), Some(namespace)) = (&self.latest.limited_to, namespace) else {
            return true;
        };
        let Some(now) = limit.revision_of(namespace) else {
            // Not a namespace this list covers: it cannot tell.
            return false;
        };
        let then = self
            .subscribed_lanes
            .as_ref()
            .and_then(|l| l.revision_of(namespace))
            .unwrap_or(0);
        now > then
    }

    /// What the list says about an object asked for that it does not have.
    pub(in crate::ui) fn missing_text(&self) -> Option<String> {
        let (namespace, name) = self.missing.as_ref()?;
        Some(match namespace {
            Some(ns) => format!("{name} no longer exists in {ns}."),
            None => format!("{name} no longer exists."),
        })
    }

    /// The revision of the newest rows the selected object was seen in, for Go Back.
    pub(in crate::ui) fn selected_seen(&self) -> Option<u64> {
        self.selected.as_ref().and(self.selected_seen)
    }

    fn select_named(&mut self, namespace: Option<&str>, name: &str, cx: &mut Context<Self>) {
        let Some(row) = find_row(&self.latest, namespace, name) else {
            return;
        };
        self.missing = None;
        self.selected_seen = rows_revision(&self.latest, namespace);
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
            sort: list_model::resolve_sort(
                &snapshot.columns,
                self.query.sort.as_ref(),
                &self.drawn_in(&snapshot.columns),
            ),
        };
        self.sort_drawn = query.sort.is_some();
        let job_sort = query.sort;
        let view = self.core.compute_view(snapshot.clone(), query);
        let selected_uid = self.selected.as_ref().map(|s| s.uid.clone());
        let restart = std::mem::take(&mut self.remeasure);
        let namespaces = self.query.namespaces.clone();
        let job_snapshot = snapshot.clone();
        #[cfg(test)]
        let gate = self.compute_gate.clone();
        let job = cx.background_spawn(async move {
            #[cfg(test)]
            if let Some(gate) = gate {
                let _ = gate.recv().await;
            }
            let rows = view.await;
            let widths = list_model::measure_columns(&job_snapshot, &rows, unix_now());
            let scoped = list_model::rows_in_namespaces(&job_snapshot, &namespaces);
            let position = selected_uid.as_ref().and_then(|uid| {
                rows.iter()
                    .position(|&i| job_snapshot.rows[i as usize].uid == *uid)
            });
            (rows, widths, scoped, selected_uid, position)
        });
        self._compute = cx.spawn(async move |this, cx| {
            let (rows, widths, scoped, uid, position) = job.await;
            let _ = this.update(cx, |list, cx| {
                list.computing = false;
                #[cfg(test)]
                {
                    list.computes_done += 1;
                }
                // Sorted for a layout that has changed since (Wide toggled, the pane resized, the
                // sort clicked): drawing these rows under the new header would show one order
                // with the other's arrow. Sort again for the layout now.
                let now = list_model::resolve_sort(
                    &snapshot.columns,
                    list.query.sort.as_ref(),
                    &list.drawn_in(&snapshot.columns),
                );
                if now != job_sort {
                    list.remeasure |= restart;
                    list.recompute(cx);
                    return;
                }
                list.widths.update(&snapshot.columns, &widths, restart);
                list.scoped = scoped;
                list.apply(snapshot, rows, uid, position, cx);
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
        uid: Option<Box<str>>,
        position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.shown = snapshot;
        self.rows = rows;
        // `drawn` indexes into the snapshot's columns, and the automation state and `SortBy` read
        // it before the next frame lays it out, so lay it out again for these columns now.
        let (drawn, dropped) = self.layout(self.laid_out_width);
        self.drawn = drawn;
        self.dropped_wide = dropped;
        self.resort_if_drawn_changed(cx);
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
                        self.selected_seen =
                            rows_revision(&self.shown, selected.namespace.as_deref());
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
        let seen = rows_revision(&self.shown, selected.namespace.as_deref());
        window.focus(&self.focus, cx);
        // A user's choice wins over a startup selection still waiting for its object.
        self.pending_select = None;
        self.missing = None;
        self.selected_seen = seen;
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
        // An object still waited for would be selected later, out of the blue.
        self.pending_select = None;
        self.missing = None;
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

    /// Sorts by the drawn column `action` names, as a header click does, and reports the new
    /// sort to remember.
    fn toggle_sort(&mut self, action: &SortBy, cx: &mut Context<Self>) {
        let Some(key) = self.sort_key(action) else {
            return;
        };
        self.query.sort = list_model::next_sort(self.query.sort.as_ref(), &key);
        cx.emit(ListEvent::SortChanged(self.query.sort.clone()));
        self.recompute(cx);
        cx.notify();
    }

    /// The columns of `columns` (the newest snapshot's) that are drawn now, matched by key: the
    /// ones a sort may apply to. Before the first layout, the columns the Wide setting shows.
    fn drawn_in(&self, columns: &[flint_core::Column]) -> Vec<usize> {
        if self.drawn.is_empty() {
            return list_model::visible_columns(columns, self.show_wide);
        }
        let drawn: Vec<usize> = self.drawn.iter().map(|&(col, _)| col).collect();
        list_model::map_drawn(&self.shown.columns, &drawn, columns)
    }

    /// The drawn column the sort applies to, if it is drawn.
    pub(super) fn drawn_sort_column(&self) -> Option<usize> {
        let drawn: Vec<usize> = self.drawn.iter().map(|&(col, _)| col).collect();
        list_model::sort_column(&self.shown.columns, self.query.sort.as_ref(), &drawn)
    }

    /// Sorts again when the sort's column came into view or left it with the layout, so a
    /// remembered sort is suspended while its column is not drawn and applies when it is.
    fn resort_if_drawn_changed(&mut self, cx: &mut Context<Self>) {
        if self.query.sort.is_some()
            && !self.drawn.is_empty()
            && self.drawn_sort_column().is_some() != self.sort_drawn
        {
            self.recompute(cx);
        }
    }

    /// The columns to draw and their widths, for a pane `available` points wide (unknown
    /// before the first layout). Wide columns that do not fit are left out; spare width goes to
    /// the Name column first.
    fn layout(&self, available: Option<f32>) -> (Vec<(usize, f32)>, usize) {
        let columns = &self.shown.columns;
        let width = |chars: usize| chars as f32 * CHAR_WIDTH + CELL_PADDING;
        let (mut drawn, dropped) = list_model::fit_columns(
            columns,
            |i| width(self.widths.chars(i, &columns[i])),
            self.show_wide,
            available,
        );
        if let Some(room) = available {
            list_model::squeeze_identity(
                &mut drawn,
                columns,
                room,
                width(SQUEEZED_NAME_CHARS),
                width(SQUEEZED_NAMESPACE_CHARS),
            );
            if let Some(name) = columns.iter().position(|c| c.role == ColumnRole::Name) {
                let wanted = width(self.widths.wanted(name, &columns[name]));
                list_model::widen_name(&mut drawn, columns, room, wanted);
            }
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
            let select = SelectRow {
                namespace: row.namespace.as_deref().map(str::to_owned),
                name: row.name.to_string(),
            };
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
                // By name, not position: rows may have been reordered by the time the click
                // is handled.
                .on_click(registry::on_click(select.clone()));
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
                // Names of one workload share their start: cut them in the middle, so the ends
                // that tell them apart stay in view. Other cells truncate at the end.
                let room = ((width - CELL_PADDING) / CHAR_WIDTH).floor().max(0.) as usize;
                let shown_text = match is_name.then(|| list_model::fit_name(&text, room)) {
                    Some(Some(cut)) => SharedString::from(cut),
                    _ => text.clone(),
                };
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
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(shown_text.clone())
                            .test_text(shown_text),
                    )
                    .when(!fits, |el| {
                        el.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
                    });
                el = el.child(cell);
            }
            let el = el.child(div().flex_1()).test_support();
            elements.push(match &self.row_menu {
                // A right-click selects the row (a mouse down, before the menu opens on it), so
                // the menu acts on it.
                Some(menu) => {
                    let menu = menu.clone();
                    div()
                        .id(SharedString::from(format!("row-menu-{}", row.uid)))
                        .w_full()
                        .on_mouse_down(MouseButton::Right, registry::on_click_passing(select))
                        .child(el)
                        .context_menu(move |m, window, cx| menu(m, window, cx))
                        .into_any_element()
                }
                None => el.into_any_element(),
            });
        }
        elements
    }

    fn render_header(&self, theme: &Theme, _cx: &Context<Self>) -> impl IntoElement {
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
        let sorted_col = self.drawn_sort_column();
        for &(col, width) in &self.drawn {
            let column = &self.shown.columns[col];
            let name = column.name.to_string();
            let sorted = (sorted_col == Some(col))
                .then(|| self.query.sort.as_ref().map(|s| s.descending))
                .flatten();
            let numeric = list_model::is_numeric(column);
            let tip = if column.description.is_empty() {
                name.clone()
            } else {
                format!("{name}\n{}", column.description)
            };
            // By key: a header shared by two columns sorts the one clicked.
            let click = SortBy {
                column: name.clone(),
                key: Some(column.key()),
            };
            header = header.child(
                h_flex()
                    .id(SharedString::from(header_id(&self.shown.columns, col)))
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
                    .on_click(registry::on_click(click))
                    .test_support(),
            );
        }
        header.child(div().flex_1())
    }

    fn render_toolbar(&self, theme: &Theme, _cx: &Context<Self>) -> impl IntoElement {
        let snapshot = &self.shown;
        let count = self.row_count();
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
                    .child(count.text)
                    .tooltip(move |window, cx| Tooltip::new(count.tip.clone()).build(window, cx)),
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
                        .on_click(registry::on_click(ToggleWide)),
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

    /// The toolbar's count: rows passing the text filter out of those in the selected
    /// namespaces, with the cluster's total in its tooltip.
    pub fn row_count(&self) -> list_model::RowCount {
        let namespaces: &[String] = if self.shown.namespaced {
            &self.query.namespaces
        } else {
            &[]
        };
        list_model::row_count(
            self.rows.len(),
            self.scoped,
            self.shown.rows.len(),
            namespaces,
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
                    || (self.namespaced && !self.query.namespaces.is_empty());
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

    /// What the list says when the selected object's namespace is outside the namespace
    /// filter (opened from the resource switcher): its row is hidden, its details show.
    pub(in crate::ui) fn filtered_out_text(&self) -> Option<String> {
        let selected = self.selected.as_ref()?;
        let namespace = selected.namespace.as_deref()?;
        let filter = &self.query.namespaces;
        if !self.namespaced || filter.is_empty() || filter.iter().any(|n| n == namespace) {
            return None;
        }
        Some(format!(
            "{} is in namespace {namespace}, outside the namespace filter, so its row is hidden; \
             its details are shown.",
            selected.name
        ))
    }

    /// Banners over the rows: an object asked for that is gone, or whose row the namespace
    /// filter hides; not current (reconnecting, failing), limited to some namespaces by
    /// permissions, or without server columns.
    fn banners(&self, theme: &Theme) -> Vec<AnyElement> {
        let snapshot = &self.shown;
        let mut banners = Vec::new();
        if let Some(text) = self.missing_text() {
            banners.push(
                notice("list-missing", text, theme.warning)
                    .test_support()
                    .into_any_element(),
            );
        }
        if let Some(text) = self.filtered_out_text() {
            banners.push(
                notice("list-filtered-out", text, theme.info)
                    .test_support()
                    .into_any_element(),
            );
        }
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

/// The revision of the rows of `snapshot` an object in `namespace` is in: that namespace's own
/// list on a namespace-limited list, else the whole list's. `None` when no watch built it.
fn rows_revision(snapshot: &KindSnapshot, namespace: Option<&str>) -> Option<u64> {
    let revision = match (&snapshot.limited_to, namespace) {
        (Some(limit), Some(namespace)) => limit.revision_of(namespace)?,
        _ => snapshot.revision,
    };
    (revision > 0).then_some(revision)
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

/// The element id of column `col`'s header: `sort-<header>`, with the column's index appended
/// when another column has the same header.
fn header_id(columns: &[flint_core::Column], col: usize) -> String {
    let name = &columns[col].name;
    if columns.iter().filter(|c| c.name == *name).count() > 1 {
        format!("sort-{name}-{col}")
    } else {
        format!("sort-{name}")
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
        .child(super::theme::status_dot(color, true))
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
            // Weak: a closed list must not live on until the next frame.
            let this = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                let _ = this.update(cx, |_, cx| cx.notify());
            });
        }
        let (drawn, dropped) = self.layout(available);
        self.drawn = drawn;
        self.dropped_wide = dropped;
        self.resort_if_drawn_changed(cx);
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
        // The rows have focus (not the filter): ringed, as Tab between panes shows (UP-33).
        let focused = self.focus.is_focused(window);
        v_flex()
            .id("resource-list")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .map(|el| Self::register_actions(el, cx))
            .size_full()
            .min_w_0()
            .relative()
            .child(self.render_toolbar(theme, cx))
            .children(self.banners(theme))
            .child(body)
            .when(focused, |el| {
                el.child(
                    super::theme::pane_focus_ring(theme)
                        .id("list-focus-ring")
                        .test_support(),
                )
            })
    }
}

#[path = "list_actions.rs"]
mod actions;

#[cfg(any(test, feature = "automation"))]
#[path = "list_state.rs"]
mod state;

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
