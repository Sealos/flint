//! The generic resource list (#9): one kind on one cluster, as a virtualized table.
//!
//! Rows come from a core [`flint_core::Subscription`]; filtering and sorting run on the core's
//! threads ([`CoreHandle::compute_view`]) and column measuring on GPUI's background executor.
//! The UI thread only lays out the rows in view (ADR-0002), so a list of 10,000 rows draws like
//! a list of 50, however fast the watch changes.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use flint_core::redact::{is_masked_column, is_secret_list};
use flint_core::table::unix_now;
use flint_core::{
    Cell, ClusterId, ColumnRole, CoreHandle, Feed, KindId, KindSnapshot, ListFormat, NavEntry, Row,
    SyncState, ViewQuery,
};
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
use list_model::{CELL_PADDING, CHAR_WIDTH};

use super::list_model::{self, ColumnWidths, SortSpec};
use super::notice::notice;
use super::row_menu::{OpenRowMenu, RowMenu};

use super::registry::{
    self, CloseDetail, SelectFirst, SelectLast, SelectNext, SelectPrev, SelectRow,
    ShowAllNamespaces, SortBy, ToggleWide,
};

/// The list's key context; the registry binds the list's keys in it.
pub const CONTEXT: &str = "ResourceList";
pub const ROW_HEIGHT: f32 = 24.;

// The kit's tooltips paint plain text that view tests cannot observe: this records what a cell's
// tooltip was built with.
#[cfg(test)]
thread_local! {
    static CELL_TOOLTIPS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// How narrow Namespace gets in a tight list (characters), so status columns fit. It keeps room
/// for its header and the sort arrow, as measured. Name stops at [`list_model::NAME_MIN_CHARS`].
const SQUEEZED_NAMESPACE_CHARS: usize = 11;

/// The object a list has selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub uid: Box<str>,
    pub name: String,
    pub namespace: Option<String>,
    pub resource_version: Box<str>,
}

/// A selection as it stood, to put back.
struct SelectionBefore {
    selected: Option<Selected>,
    ix: Option<usize>,
    seen: Option<u64>,
    gone: bool,
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
    /// Only these objects, when the list was opened from a dashboard problem tile.
    only: Option<ObjectFilter>,
}

/// A fixed set of objects a list is narrowed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectFilter {
    /// What the objects are, for the notice: `failing pods`.
    pub label: String,
    pub objects: Arc<CountedObjects>,
}

/// The objects a count was made of. An object is matched by UID, so one replaced under the same
/// name is not it; only an object that reported no UID is matched by name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CountedObjects {
    uids: std::collections::HashSet<String>,
    /// `namespace/name`, or `name` when cluster-scoped. A Kubernetes name holds no `/`.
    without_uid: std::collections::HashSet<String>,
    /// Sorted. Empty when no object is namespaced.
    namespaces: Vec<String>,
}

impl CountedObjects {
    pub fn insert(&mut self, namespace: Option<&str>, name: &str, uid: Option<&str>) {
        match uid.filter(|uid| !uid.is_empty()) {
            Some(uid) => {
                self.uids.insert(uid.to_owned());
            }
            None => {
                self.without_uid.insert(Self::key(namespace, name));
            }
        }
        if let Some(namespace) = namespace
            && let Err(at) = self
                .namespaces
                .binary_search_by(|n| n.as_str().cmp(namespace))
        {
            self.namespaces.insert(at, namespace.to_owned());
        }
    }

    pub fn len(&self) -> usize {
        self.uids.len() + self.without_uid.len()
    }

    fn matches(&self, namespace: Option<&str>, name: &str, uid: &str) -> bool {
        self.uids.contains(uid) || self.without_uid.contains(&Self::key(namespace, name))
    }

    fn key(namespace: Option<&str>, name: &str) -> String {
        match namespace {
            Some(namespace) => format!("{namespace}/{name}"),
            None => name.to_owned(),
        }
    }
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
    /// The selection the last change replaced, for [`ResourceList::undo_selection`].
    selection_before: Option<SelectionBefore>,
    computing: bool,
    /// Something changed while a computation ran: run again when it ends.
    dirty: bool,
    /// Down left the filter while its rows were being computed: select the first row in this
    /// window once they are current, if the rows still have focus.
    select_first_in: Option<AnyWindowHandle>,
    /// Go Back set the filter's text: rows computed for any other text are dropped, not drawn,
    /// until rows for it arrive ([`ResourceList::restore_filter`]).
    restoring_filter: bool,
    /// The text the rows drawn were computed for; `query.text` runs ahead of it while typing.
    rows_text: String,
    /// The selected object, when the rows drawn exist without it: the filters exclude it.
    hidden_selected: Option<Box<str>>,
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
    /// Whether the rows on screen were computed for exactly one namespace. A newly chosen scope
    /// does not count until its rows arrive: the old ones may still span several namespaces.
    rows_in_one_namespace: bool,
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
    /// The row menu now open. The list holds it, not the row: opening the detail pane narrows
    /// the row, which must not close the menu (#496).
    open_row_menu: Option<OpenRowMenu>,
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
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter by name, label, managed…"));
        let on_filter = cx.subscribe_in(&filter, window, |this, input, event, _, cx| {
            if let InputEvent::Change = event {
                this.query.text = input.read(cx).value().to_string();
                this.select_first_in = None;
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
            selection_before: None,
            computing: false,
            dirty: false,
            select_first_in: None,
            restoring_filter: false,
            rows_text: String::new(),
            hidden_selected: None,
            remeasure: false,
            scoped: 0,
            scroll: UniformListScrollHandle::new(),
            hscroll: ScrollHandle::new(),
            drawn: Vec::new(),
            dropped_wide: 0,
            laid_out_width: None,
            rows_in_one_namespace: false,
            sort_drawn: false,
            #[cfg(test)]
            compute_gate: None,
            #[cfg(test)]
            computes_done: 0,
            focus: cx.focus_handle(),
            filter,
            row_menu: None,
            open_row_menu: None,
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

    /// Whether an object opened by name waits for its row ([`ResourceList::select_listed`]),
    /// or the list says it is missing: either way, not the bare list.
    pub fn picking(&self) -> bool {
        self.pending_select.is_some() || self.missing.is_some()
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Gives each row a context menu built by `menu`; right-clicking a row selects it first.
    pub fn set_row_menu(&mut self, menu: RowMenu) {
        self.row_menu = Some(menu);
    }

    /// Opens the row menu of `uid` at `position`, replacing one already open. Built after the
    /// press has been handled, so the menu sees the row it selected.
    fn show_row_menu(
        &mut self,
        uid: String,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(build) = self.row_menu.clone() else {
            return;
        };
        let back_to = OpenRowMenu::focus_before(self.open_row_menu.take(), window, cx)
            .unwrap_or_else(|| self.focus.clone());
        cx.defer_in(window, move |list, window, cx| {
            // The selection did not move there (a question about an unapplied edit is open):
            // the menu's actions would be another row's.
            if list.selected.as_ref().map(|s| &*s.uid) != Some(uid.as_str()) {
                return;
            }
            let open = OpenRowMenu::open(&build, uid, position, back_to, window, cx);
            list.open_row_menu = Some(open);
            cx.notify();
        });
    }

    /// A right-press on the row `uid`, after its selection was asked for.
    #[cfg(test)]
    pub(in crate::ui) fn press_row_menu(
        &mut self,
        uid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_row_menu(uid.to_owned(), Point::default(), window, cx);
    }

    #[cfg(test)]
    pub(in crate::ui) fn has_open_row_menu(&self) -> bool {
        self.open_row_menu.is_some()
    }

    /// Called by the open menu when it is dismissed.
    pub(in crate::ui) fn row_menu_closed(&mut self, cx: &mut Context<Self>) {
        self.open_row_menu = None;
        cx.notify();
    }

    /// The open menu to draw. One whose row has left the list is dismissed instead: its entries
    /// would act on something no longer shown.
    fn drawn_row_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.open_row_menu.as_ref()?;
        let listed = self
            .rows
            .iter()
            .any(|&ix| &*self.shown.rows[ix as usize].uid == open.uid());
        if !listed {
            let menu = open.menu().clone();
            cx.defer_in(window, move |_, _, cx| {
                menu.update(cx, |_, cx| cx.emit(DismissEvent));
            });
            return None;
        }
        open.element(window, cx)
    }

    /// The namespaces the drawn list fell back to, when the account cannot list all of them.
    pub fn limited_to(&self) -> Option<&flint_core::NamespaceLimit> {
        self.shown.limited_to.as_deref()
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

    /// Narrows the list to `only`, or shows every object again with `None`.
    pub fn show_only(&mut self, only: Option<ObjectFilter>, cx: &mut Context<Self>) {
        if self.query.only != only {
            self.query.only = only;
            self.remeasure = true;
            self.recompute(cx);
            cx.notify();
        }
    }

    /// What the list is narrowed to, if anything.
    #[cfg(test)]
    pub fn only(&self) -> Option<&ObjectFilter> {
        self.query.only.as_ref()
    }

    /// The namespaces shown: a narrowed list covers its objects, whatever the picker holds.
    fn scope(&self) -> &[String] {
        match &self.query.only {
            Some(only) => &only.objects.namespaces,
            None => &self.query.namespaces,
        }
    }

    pub fn set_namespaces(&mut self, namespaces: Vec<String>, cx: &mut Context<Self>) {
        if self.query.namespaces != namespaces {
            self.query.namespaces = namespaces;
            // Choosing namespaces asks for those, so the narrowing ends.
            self.query.only = None;
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
        self.selection_before = Some(self.selection_now());
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
        let job_text = self.query.text.clone();
        let namespaces = self.scope().to_vec();
        let query = ViewQuery {
            namespaces: namespaces.clone(),
            text: self.query.text.clone(),
            sort: list_model::resolve_sort(
                &snapshot.columns,
                self.query.sort.as_ref(),
                &self.sortable_in(&snapshot.columns),
            ),
        };
        self.sort_drawn = query.sort.is_some();
        let job_sort = query.sort;
        let view = self.core.compute_view(snapshot.clone(), query);
        let selected_uid = self.selected.as_ref().map(|s| s.uid.clone());
        let restart = std::mem::take(&mut self.remeasure);
        let one_namespace = namespaces.len() == 1;
        let job_snapshot = snapshot.clone();
        let only = self.query.only.as_ref().map(|only| only.objects.clone());
        let job_only = only.clone();
        #[cfg(test)]
        let gate = self.compute_gate.clone();
        let job = cx.background_spawn(async move {
            #[cfg(test)]
            if let Some(gate) = gate {
                let _ = gate.recv().await;
            }
            let rows = view.await;
            let rows: Arc<[u32]> = match &job_only {
                Some(objects) => rows
                    .iter()
                    .copied()
                    .filter(|&i| {
                        let row = &job_snapshot.rows[i as usize];
                        objects.matches(row.namespace.as_deref(), &row.name, &row.uid)
                    })
                    .collect(),
                None => rows,
            };
            let measured = list_model::rows_to_measure(&job_snapshot, &namespaces);
            let widths = list_model::measure_columns(&job_snapshot, &measured, unix_now());
            let scoped = list_model::rows_in_namespaces(&job_snapshot, &namespaces);
            let position = selected_uid.as_ref().and_then(|uid| {
                rows.iter()
                    .position(|&i| job_snapshot.rows[i as usize].uid == *uid)
            });
            (rows, widths, scoped, selected_uid, position)
        });
        self._compute = cx.spawn(async move |this, cx| {
            let (rows, widths, scoped, uid, position) = job.await;
            let select_first = this.update(cx, |list, cx| {
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
                    &list.sortable_in(&snapshot.columns),
                );
                let only_now = list.query.only.as_ref().map(|only| &only.objects);
                if now != job_sort
                    || only_now != only.as_ref()
                    || (list.restoring_filter && job_text != list.query.text)
                {
                    list.remeasure |= restart;
                    list.recompute(cx);
                    return None;
                }
                list.restoring_filter = false;
                list.rows_text = job_text;
                list.widths.update(&snapshot.columns, &widths, restart);
                list.scoped = scoped;
                list.rows_in_one_namespace = one_namespace;
                list.apply(snapshot, rows, uid, position, cx);
                if list.dirty {
                    list.recompute(cx);
                }
                // Only rows that match the filter now may be selected.
                if list.computing {
                    None
                } else {
                    list.select_first_in.take()
                }
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
                self.hidden_selected = found
                    .filter(|_| position.is_none())
                    .map(|row| row.uid.clone());
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
                    // empty or stale, and a limited one must have listed its namespace anew.
                    None if self.shown.state.is_current()
                        && !self.selected_gone
                        && list_model::listing_proves_gone(
                            self.shown.limited_to.as_deref(),
                            selected.namespace.as_deref(),
                            self.selected_seen,
                        ) =>
                    {
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
        let before = self.selection_now();
        window.focus(&self.focus, cx);
        // A user's choice wins over a startup selection still waiting for its object.
        self.pending_select = None;
        self.missing = None;
        self.selected_seen = seen;
        self.selected_ix = Some(ix);
        self.selected_gone = false;
        self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
        if self.selected.as_ref() != Some(&selected) {
            self.selection_before = Some(before);
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
        self.select_first_in = None;
        self.missing = None;
        if self.selected.is_some() {
            self.selection_before = Some(self.selection_now());
            self.selected = None;
            self.selected_ix = None;
            cx.emit(ListEvent::Selected(None));
            cx.notify();
        }
    }

    pub(in crate::ui) fn has_selection(&self) -> bool {
        self.selected.is_some()
    }

    fn selection_now(&self) -> SelectionBefore {
        SelectionBefore {
            selected: self.selected.clone(),
            ix: self.selected_ix,
            seen: self.selected_seen,
            gone: self.selected_gone,
        }
    }

    /// Puts back the selection the last change replaced, saying nothing: whether there was one.
    pub(in crate::ui) fn undo_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(before) = self.selection_before.take() else {
            return false;
        };
        self.selected = before.selected;
        self.selected_ix = before.ix;
        self.selected_seen = before.seen;
        self.selected_gone = before.gone;
        self.reveal_selected = true;
        self.recompute(cx);
        cx.notify();
        true
    }

    /// Selects `wanted` again, as asked for before a question about leaving the row shown.
    pub(in crate::ui) fn reselect(
        &mut self,
        wanted: &Selected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.position_of(&wanted.uid) {
            Some(ix) => self.select_index(ix, window, cx),
            None => self.select_listed(wanted.namespace.clone(), wanted.name.clone(), None, cx),
        }
    }

    /// Selects the first row, unless a row is selected or waiting to be.
    fn select_first_if_none(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_none() && self.pending_select.is_none() {
            self.select_index(0, window, cx);
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

    /// Whether this list hides the values of the column at `index`, so nothing may be derived
    /// from them.
    pub(super) fn is_masked(&self, columns: &[flint_core::Column], index: usize) -> bool {
        is_masked_column(is_secret_list(self.kind.as_str()), columns, index)
    }

    /// The drawn columns of `columns` a sort may apply to: a remembered sort on a masked column
    /// stays suspended, with no arrow, since its rows are not ordered by it.
    fn sortable_in(&self, columns: &[flint_core::Column]) -> Vec<usize> {
        let mut drawn = self.drawn_in(columns);
        drawn.retain(|&col| !self.is_masked(columns, col));
        drawn
    }

    /// The drawn column the sort applies to, if it is drawn and not masked.
    pub(super) fn drawn_sort_column(&self) -> Option<usize> {
        let drawn: Vec<usize> = self
            .drawn
            .iter()
            .map(|&(col, _)| col)
            .filter(|&col| !self.is_masked(&self.shown.columns, col))
            .collect();
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
    /// before the first layout). Wide columns that do not fit are left out, then Ready and
    /// Namespace as [`list_model::shed_columns`] allows; spare width goes to the Name column.
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
            let name_min = width(list_model::NAME_MIN_CHARS);
            let namespace_min = width(SQUEEZED_NAMESPACE_CHARS);
            let sort = self.query.sort.as_ref();
            let rules = list_model::ShedRules {
                ready_repeats_status: list_model::ready_repeats_status(self.kind.as_str()),
                one_namespace: self.rows_in_one_namespace,
            };
            list_model::shed_columns(
                &mut drawn,
                columns,
                room,
                name_min,
                namespace_min,
                rules,
                |i| sort.is_some_and(|s| columns[i].key() == s.column),
            );
            list_model::squeeze_identity(&mut drawn, columns, room, name_min, namespace_min);
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
        let secrets = is_secret_list(self.kind.as_str());
        let health = list_model::has_health_columns(self.kind.as_str());
        let selected_uid = self.selected.as_ref().map(|s| &*s.uid);
        let ncols = self.shown.columns.len().max(1);
        let namespace_hidden = !self
            .drawn
            .iter()
            .any(|&(col, _)| self.shown.columns[col].role == ColumnRole::Namespace);
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
            let flags = list_model::RowFlags {
                stale,
                health,
                finished,
                masked: false,
            };
            for &(col, width) in &self.drawn {
                let column = &self.shown.columns[col];
                let list_model::CellStyle {
                    shown,
                    tooltip,
                    tone,
                    numeric,
                    mono,
                } = list_model::CellStyle::of(
                    column,
                    || cell_text(row, col, now),
                    list_model::text_width(column, row, secrets, width),
                    list_model::RowFlags {
                        masked: is_masked_column(secrets, &self.shown.columns, col),
                        ..flags
                    },
                );
                // With its column hidden, the Name cell is where the namespace can be read.
                let tooltip = match row.namespace.as_deref() {
                    Some(namespace) if namespace_hidden && column.role == ColumnRole::Name => {
                        Some(SharedString::from(format!("{namespace}/{}", row.name)))
                    }
                    _ => tooltip,
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
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(shown.clone())
                            .test_text(shown),
                    )
                    .when(column.role == ColumnRole::Name, |el| {
                        el.children(managed::marker(row, secrets, theme))
                    })
                    .when_some(tooltip, |el, text| {
                        el.tooltip(move |window, cx| {
                            #[cfg(test)]
                            CELL_TOOLTIPS.with_borrow_mut(|tips| tips.push(text.to_string()));
                            Tooltip::new(text.clone()).build(window, cx)
                        })
                    });
                el = el.child(cell);
            }
            let el = el.child(div().flex_1()).test_support();
            elements.push(match &self.row_menu {
                // A right-click selects the row (a mouse down, before the menu opens on it), so
                // the menu acts on it.
                Some(_) => {
                    let press = row_press(select, row.uid.to_string(), cx.weak_entity());
                    let on_right = press.clone();
                    div()
                        .id(SharedString::from(format!("row-menu-{}", row.uid)))
                        .relative()
                        .w_full()
                        // not-an-action: opens the menu; `row_press` selects through the registry.
                        .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                            on_right(event.position, window, cx);
                            cx.stop_propagation();
                        })
                        .child(el)
                        .child(long_press_area(press))
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
            let sortable = !self.is_masked(&self.shown.columns, col);
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
                    .when(sortable, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.bg(theme.table_hover))
                            .on_click(registry::on_click(click))
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
            self.scope()
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
        if snapshot.rows.is_empty()
            && let Some(title) =
                access_title(&snapshot.state, label, snapshot.limited_to.as_deref())
        {
            let detail = match &snapshot.state {
                // Nothing to fall back to: the error says how to pick namespaces.
                SyncState::Failed { error, .. } if snapshot.limited_to.is_some() => {
                    Some(error.clone())
                }
                SyncState::Failed { error, .. } => {
                    Some(format!("{error}\nFlint retries in the background."))
                }
                _ => None,
            };
            return Some(message(title, detail, theme.muted_foreground));
        }
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
            SyncState::Failed { error, .. } if snapshot.rows.is_empty() => Some(message(
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
            SyncState::RetryingRefusal { error, .. } if snapshot.rows.is_empty() => Some(message(
                format!("Reconnecting to list {label}…"),
                Some(error.clone()),
                theme.warning,
            )),
            _ if self.restoring_filter => Some(
                h_flex()
                    .id("list-placeholder")
                    .size_full()
                    .justify_center()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(Spinner::new().small())
                    .child(
                        div()
                            .id("list-filtering")
                            .child(format!("Filtering {label}…"))
                            .test_support(),
                    )
                    .test_support()
                    .into_any_element(),
            ),
            _ if self.rows.is_empty() && self.query.only.is_some() => {
                let only = self.query.only.as_ref().map_or("", |only| &only.label);
                Some(message(
                    format!("No {only}"),
                    Some(format!("Open {label} in the navigation to see all.")),
                    theme.muted_foreground,
                ))
            }
            _ if self.rows.is_empty() => {
                let filtered = !self.query.text.trim().is_empty()
                    || (self.namespaced && !self.query.namespaces.is_empty());
                Some(if snapshot.rows.is_empty() {
                    message(format!("No {label}"), None, theme.muted_foreground)
                } else if self.namespaced && !self.query.namespaces.is_empty() && self.scoped == 0 {
                    // The namespace filter names nothing the cluster has rows in (a typed
                    // name that does not exist): say which, and offer the way out.
                    let names = self.query.namespaces.join(", ");
                    v_flex()
                        .id("list-placeholder")
                        .size_full()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .p_6()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(format!("No {label} in namespace {names}")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{} in other namespaces.", snapshot.rows.len())),
                        )
                        .child(
                            Button::new("show-all-namespaces")
                                .small()
                                .label("Show All Namespaces")
                                .on_click(registry::on_click(ShowAllNamespaces)),
                        )
                        .test_support()
                        .into_any_element()
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

    /// What the list says when a filter hides the selected object's row and its details still
    /// show: its namespace is outside the namespace filter (opened from the resource switcher),
    /// or the rows computed for the current text exclude it.
    pub(in crate::ui) fn filtered_out_text(&self) -> Option<String> {
        let selected = self.selected.as_ref()?;
        let filter = self.scope();
        if let Some(namespace) = selected.namespace.as_deref()
            && self.namespaced
            && !filter.is_empty()
            && !filter.iter().any(|n| n == namespace)
        {
            return Some(format!(
                "{} is in namespace {namespace}, outside the namespace filter, so its row is \
                 hidden; its details are shown.",
                selected.name
            ));
        }
        let hidden_by_text = !self.restoring_filter
            && !self.query.text.trim().is_empty()
            && self.rows_text == self.query.text
            && self.hidden_selected.as_deref() == Some(&*selected.uid);
        hidden_by_text.then(|| {
            format!(
                "{} does not match the text filter, so its row is hidden; its details are shown.",
                selected.name
            )
        })
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
        if let Some(only) = &self.query.only {
            let across = if self.query.namespaces.is_empty() {
                ""
            } else {
                ", in any namespace"
            };
            let text = format!(
                "Showing the {} the overview found{across}. Open {} in the navigation to see all.",
                only.label, self.label
            );
            banners.push(
                notice("list-problem-filter", text.clone(), theme.info)
                    .aria_label(text)
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
        let has_rows = !snapshot.rows.is_empty();
        let state = match &snapshot.state {
            // With no rows the placeholder says it all: there is nothing to call out of date.
            s @ SyncState::RetryingRefusal { .. } if s.is_checking_access() && !has_rows => None,
            s @ SyncState::RetryingRefusal { error, .. } if s.is_checking_access() => Some((
                theme.muted_foreground,
                self.access_banner("Checking access", error),
            )),
            s @ SyncState::Failed { error, .. } if s.is_no_access() && has_rows => Some((
                theme.muted_foreground,
                self.access_banner("No access", error),
            )),
            SyncState::Reconnecting { error: Some(e) }
            | SyncState::RetryingRefusal { error: e, .. } => Some((
                theme.warning,
                format!("Reconnecting. These rows may be out of date: {e}"),
            )),
            SyncState::Reconnecting { error: None } => Some((
                theme.warning,
                "Reconnecting. These rows may be out of date.".to_owned(),
            )),
            SyncState::Failed { error, .. } if !snapshot.rows.is_empty() => Some((
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

    /// The banner for a denial over cached rows. Only rows the filters leave on show can be
    /// called out of date.
    fn access_banner(&self, status: &str, error: &str) -> String {
        if self.rows.is_empty() {
            format!("{status}: {error}")
        } else {
            format!("{status}. These rows may be out of date: {error}")
        }
    }
}

/// What the list is titled while an account's access is the question. `None` for anything that
/// is not a denial: a missing API must not read as a permissions problem. A settled denial of
/// a namespace-limited list names the namespaces tried.
fn access_title(
    state: &SyncState,
    label: &str,
    limit: Option<&flint_core::NamespaceLimit>,
) -> Option<String> {
    if state.is_checking_access() {
        Some(format!("Checking access to {label}…"))
    } else if state.is_no_access() {
        Some(
            match limit.map(|l| (l.listed.as_slice(), l.denied.as_slice())) {
                Some(([], [namespace])) => format!("No access to {label} in {namespace}"),
                Some(([], denied)) if !denied.is_empty() => {
                    format!("No access to {label} in {} namespaces", denied.len())
                }
                _ => format!("No access to {label}"),
            },
        )
    } else {
        None
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
    // A lane fails on a 403 or a 404, and the limit does not say which: claim neither.
    if !limit.denied.is_empty() {
        text.push_str(&format!(
            " Could not list {label} in: {}.",
            limit.denied.join(", ")
        ));
    }
    text
}

impl list_model::CellText for SharedString {
    fn from_static(text: &'static str) -> Self {
        SharedString::new_static(text)
    }
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
        SyncState::RetryingRefusal { error, .. } if state.is_checking_access() => (
            "checking access",
            theme.muted_foreground,
            Some(error.clone()),
        ),
        SyncState::RetryingRefusal { error, .. } => {
            ("reconnecting", theme.warning, Some(error.clone()))
        }
        SyncState::Failed { error, .. } if state.is_no_access() => {
            ("no access", theme.muted_foreground, Some(error.clone()))
        }
        SyncState::Failed { error, .. } => ("failed", theme.danger, Some(error.clone())),
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
        let row_menu = self.drawn_row_menu(window, cx);

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
                // more columns are to the right. Always mounted: it hides itself using this
                // frame's layout, while `available` is a frame old, and its state then lives
                // and dies with the list instead of following pane resizes.
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
            .children(row_menu)
    }
}

/// Pressing a row for its menu, at a window position.
type RowPress = Rc<dyn Fn(Point<Pixels>, &mut Window, &mut App)>;

/// What pressing a row for its menu does: selects the row, then opens the menu on it.
fn row_press(select: SelectRow, uid: String, list: WeakEntity<ResourceList>) -> RowPress {
    let select = registry::on_click::<(), _>(select);
    Rc::new(move |position, window, cx| {
        select(&(), window, cx);
        let _ = list.update(cx, |list, cx| {
            list.show_row_menu(uid.clone(), position, window, cx)
        });
    })
}

/// A finger has no right button: a long press on the row opens its menu too.
fn long_press_area(press: RowPress) -> impl IntoElement {
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |_, hitbox, window, _| {
            window.on_mouse_event(move |event: &LongPressEvent, phase, window, cx| {
                if phase.bubble()
                    && event.phase == TouchPhase::Started
                    && !window.default_prevented()
                    && hitbox.is_hovered(window)
                {
                    window.prevent_default();
                    press(event.start_position, window, cx);
                }
            });
        },
    )
    .absolute()
    .size_full()
}

#[path = "list_actions.rs"]
mod actions;

#[path = "list_managed.rs"]
mod managed;

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "opening a combined list is wired in later")
)]
#[path = "list_combined.rs"]
pub mod combined;

#[cfg(any(test, feature = "automation"))]
#[path = "list_state.rs"]
mod state;

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
