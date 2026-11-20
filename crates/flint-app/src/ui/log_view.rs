//! A log tab in the bottom dock (#13, #75; UP-40, UP-42 to UP-46): one container's log, or a
//! workload's pods merged into one stream (#62), read by a core [`LogSession`] into a ring buffer
//! and drawn as a virtualized list. Workload lines carry a `[pod/container]` source prefix, a view
//! toggle like timestamps, and the toolbar counts the workload's streams.
//!
//! The session publishes at most about ten snapshots a second (ADR-0002). Each snapshot is diffed
//! against the one shown by line id ([`log_model::list_change`]), so following a chatty pod only
//! splices lines in at the bottom and out at the top. The list follows the newest line until the
//! user scrolls up, and follows again once they scroll back to the bottom.
//!
//! - **Pause** freezes the lines shown while the buffer keeps filling; the toolbar counts the
//!   lines held back. Selecting lines pauses too, until the selection is cleared.
//! - **Search** runs its first scan on the core's threads ([`CoreHandle::search_logs`]) and then
//!   keeps up with new lines when the tab is drawn ([`LogView::catch_up_search`]), so a hidden
//!   tab scans nothing until it is shown; matches are highlighted with
//!   [`flint_core::SearchQuery::match_ranges`].
//! - **Selection** is a [`LineSelection`] of line ids, counted without visiting its lines: copy
//!   and save write its lines as shown (timestamps only when shown, in full), without
//!   redaction, and build the text off the UI thread. AI log analysis (#56) reads the same
//!   selection.
//! - **Toolbar** is one row at any width ([`log_model::toolbar_fit`]): the status has a slot that
//!   truncates, and in a narrow tab the toggles and Save move into a "⋯" menu. Under it, one
//!   strip says the last transient thing (the selection, a copy, a save); a copy or save note
//!   times out. An ended session's error has its own banner, in plain words
//!   ([`log_model::explain`]).
//! - **Lines** show a fixed-width local timestamp and, in a workload, a short pod prefix in the
//!   pod's colour; wrapped text hangs past both. The full values are in tooltips, copies and
//!   saves ([`log_model`]).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use flint_core::{
    ClusterId, CoreHandle, LineFormat, LineId, LineSelection, LogFilter, LogOptions, LogSearch,
    LogSession, LogSnapshot, LogState, LogSubject, SearchQuery, Severity,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, Theme,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use jiff::tz::TimeZone;

use super::confirm::{self, DialogText};
use super::dock::{DockItem, TabKey, TabStatus};
use super::log_model::{self, ListChange, SourceLabels, Tone};
use super::menu_record;
use super::notice::notice;
use super::registry::{
    self, DragSelection, JumpToEnd, NextMatch, PrevMatch, ReconnectLogs, SaveLines, SaveScope,
    SelectLine, SetLogContext, ToggleErrorsOnly, ToggleMatchingOnly, TogglePause, ToggleSources,
    ToggleTimestamps, ToggleWrap,
};
use super::theme;

/// The height of one unwrapped log line.
pub const LINE_HEIGHT: f32 = 18.;
/// Lines kept above a line scrolled into view (a match, the selection's head).
const REVEAL_CONTEXT: usize = 3;
/// How long a search query must stay unchanged before the buffer is scanned for it.
const SEARCH_DELAY: std::time::Duration = std::time::Duration::from_millis(150);
/// A search at most this many lines behind catches up on the UI thread when the tab is drawn
/// (well under a millisecond); further behind, a tab hidden while its pod was busy, the scan
/// runs on the core's threads.
pub const CATCH_UP_ON_UI: u64 = 20_000;
/// How long a copy or save note stays under the toolbar.
pub const NOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// What a log tab shows: a pod's container or a workload's pods on a cluster, and whether it is
/// the previous (terminated) instance. Opening the same request again shows the existing tab.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogRequest {
    pub cluster: ClusterId,
    pub subject: LogSubject,
    pub previous: bool,
}

impl LogRequest {
    /// Follow the current instance; read the previous one to its end.
    pub fn options(&self) -> LogOptions {
        LogOptions {
            follow: !self.previous,
            previous: self.previous,
            ..LogOptions::default()
        }
    }
}

/// What a save writes, captured when the user asked for it (as shown then).
#[derive(Clone)]
struct PendingSave {
    scope: SaveScope,
    format: LineFormat,
    lines: flint_core::LogLines,
    matches: Vec<LineId>,
    selection: Option<LineSelection>,
}

/// A line under the toolbar after a copy or a save. A muted one times out
/// ([`NOTE_TIMEOUT`]); any one goes when the selection changes or another note comes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub tone: Tone,
    pub text: String,
}

pub struct LogView {
    request: LogRequest,
    core: CoreHandle,
    /// Dropping the view drops the session, which stops the stream.
    session: LogSession,
    /// The snapshot drawn. The list has exactly one item per line in it.
    snapshot: Arc<LogSnapshot>,
    list: ListState,
    timestamps: bool,
    /// Show workload lines' `[pod/container]` prefix (on by default; pod sessions have none).
    sources: bool,
    wrap: bool,
    /// Paused with the Pause button or key.
    user_paused: bool,
    /// Paused because a selection started; cleared with the selection, or by resuming.
    selection_paused: bool,
    selection: Option<LineSelection>,
    /// A mouse selection is being dragged.
    drag_selecting: bool,
    query: SearchQuery,
    search: LogSearch,
    /// The matches among the rows shown in a filtered tab, kept until the search or the rows
    /// change ([`LogView::shown_matches`]).
    shown_matches: std::cell::RefCell<Option<(actions::ShownMatchesKey, Arc<[LineId]>)>>,
    /// Bumped with every query, so a slow first scan of an old query is dropped.
    search_generation: u64,
    /// A scan of `search` runs on the core's threads: the first of `query`, or a long
    /// catch-up.
    searching: bool,
    current_match: Option<LineId>,
    /// Show only the lines matching the search, with `context` lines around each (UP-48).
    matching_only: bool,
    /// Show only warnings and errors.
    errors_only: bool,
    /// Lines shown before and after each line a filter shows.
    context: usize,
    /// The filtered view's rows, while a filter is on ([`LogView::is_filtered`]); the list has
    /// one item per row then.
    filter: Option<LogFilter>,
    /// The first scan of a new filter runs on the core's threads; `filter` (or every line)
    /// stays on screen until it lands.
    filtering: bool,
    /// A long catch-up of `filter` (a tab far behind, after a pause) runs on the core's
    /// threads; the rows shown stay until it lands.
    catching_up: bool,
    _catch_up: Task<()>,
    /// Bumped with every new filter, so a slow scan of an old one is dropped.
    filter_generation: u64,
    /// Bumped whenever `filter` is replaced (a scan or catch-up landing, the filter turned
    /// off), so what was found from the old rows is found again ([`LogView::shown_matches`]).
    rows_generation: u64,
    _filter: Task<()>,
    search_input: Entity<InputState>,
    note: Option<Note>,
    /// Clears a muted note after [`NOTE_TIMEOUT`]; replaced with every note.
    _note_timer: Task<()>,
    /// Short pod prefixes and their colours (workload sessions).
    labels: SourceLabels,
    /// The time zone timestamps show in: the system's, else UTC.
    tz: TimeZone,
    /// What the timestamps toggle calls that zone: "local time (Europe/Rome)", or UTC said as
    /// such when the system's could not be read.
    tz_name: String,
    /// Tracks the tab's bounds, so the toolbar knows how much fits.
    bounds: ScrollHandle,
    /// The width the toolbar was laid out for; it is known only after layout, so a change
    /// draws again.
    laid_out_width: Option<f32>,
    focus: FocusHandle,
    _pump: Task<()>,
    _search: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl LogView {
    pub fn new(
        core: CoreHandle,
        request: LogRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (session, pump) = Self::open_session(&core, &request, cx);
        let list = ListState::new(0, ListAlignment::Top, px(LINE_HEIGHT * 20.));
        list.set_follow_mode(FollowMode::Tail);
        // Redraw the toolbar's Jump button when scrolling starts or stops following.
        let was_following = Rc::new(Cell::new(true));
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event, _, cx| {
            if was_following.replace(event.is_following_tail) != event.is_following_tail {
                let _ = weak.update(cx, |_, cx| cx.notify());
            }
        });
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search…"));
        let on_search =
            cx.subscribe_in(
                &search_input,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::Change => {
                        let text = input.read(cx).value().to_string();
                        this.set_query(text, cx);
                    }
                    // Enter and Shift-Enter run NextMatch's and PrevMatch's handlers (this listener
                    // holds the view, so it cannot go through the registry's guards).
                    InputEvent::PressEnter { shift, .. } => {
                        if *shift {
                            this.prev_match(&PrevMatch, window, cx);
                        } else {
                            this.next_match(&NextMatch, window, cx);
                        }
                    }
                    _ => {}
                },
            );
        let snapshot = session.peek();
        let (tz, tz_name) = log_model::system_zone();
        LogView {
            request,
            core,
            session,
            snapshot: Arc::new(LogSnapshot {
                lines: Default::default(),
                ..(*snapshot).clone()
            }),
            list,
            timestamps: false,
            sources: true,
            wrap: false,
            user_paused: false,
            selection_paused: false,
            selection: None,
            drag_selecting: false,
            query: SearchQuery::default(),
            search: LogSearch::default(),
            shown_matches: Default::default(),
            search_generation: 0,
            searching: false,
            current_match: None,
            matching_only: false,
            errors_only: false,
            context: 0,
            filter: None,
            filtering: false,
            catching_up: false,
            _catch_up: Task::ready(()),
            filter_generation: 0,
            rows_generation: 0,
            _filter: Task::ready(()),
            search_input,
            note: None,
            _note_timer: Task::ready(()),
            labels: SourceLabels::default(),
            tz,
            tz_name,
            bounds: ScrollHandle::new(),
            laid_out_width: None,
            focus: cx.focus_handle(),
            _pump: pump,
            _search: Task::ready(()),
            _subscriptions: vec![on_search],
        }
    }

    /// Opens the session `request` names, and the task that hands its snapshots to the view.
    fn open_session(
        core: &CoreHandle,
        request: &LogRequest,
        cx: &mut Context<Self>,
    ) -> (LogSession, Task<()>) {
        let session = core.open_logs(&request.cluster, request.subject.clone(), request.options());
        let mut updates = session.updates();
        let pump = cx.spawn(async move |this, cx| {
            while updates.changed().await {
                let snapshot = updates.latest();
                if this
                    .update(cx, |view, cx| view.set_snapshot(snapshot, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        (session, pump)
    }

    #[cfg(test)]
    pub fn request(&self) -> &LogRequest {
        &self.request
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> &Arc<LogSnapshot> {
        &self.snapshot
    }

    /// Stops reading the session, so a test can feed snapshots by hand.
    #[cfg(test)]
    pub fn stop_streaming(&mut self) {
        self._pump = Task::ready(());
    }

    /// Takes in a newer snapshot of the session: splices the list and keeps the selection on the
    /// lines still present. The search catches up when the tab is drawn
    /// ([`LogView::catch_up_search`]).
    pub fn set_snapshot(&mut self, snapshot: Arc<LogSnapshot>, cx: &mut Context<Self>) {
        let old = &self.snapshot.lines;
        let new = &snapshot.lines;
        // A filtered list follows its rows instead ([`LogView::catch_up_filter`]); rows of
        // lines that left the buffer go at once, so the list never starts with a missing row.
        let change = if let Some(filter) = &mut self.filter {
            match filter.evict(new.first_id()) {
                0 => ListChange::Unchanged,
                evicted => ListChange::Splice {
                    evicted,
                    appended: 0,
                },
            }
        } else {
            log_model::list_change(old.first_id()..old.end_id(), new.first_id()..new.end_id())
        };
        match change {
            ListChange::Unchanged => {}
            ListChange::Splice { evicted, appended } => {
                if evicted > 0 {
                    self.list.splice(0..evicted, 0);
                    // The new first line may gain a day separator.
                    if self.timestamps && self.list.item_count() > 0 {
                        self.list.remeasure_items(0..1);
                    }
                }
                if appended > 0 {
                    let end = self.list.item_count();
                    self.list.splice(end..end, appended);
                }
            }
            ListChange::Reset(count) => self.list.reset(count),
        }
        self.snapshot = snapshot;
        if let Some(pods) = &self.snapshot.pods
            && self.labels.learn(pods)
            && self.wrap
        {
            self.list.remeasure();
        }
        // An ended session has nothing to hold back: show every line, and drop the pause.
        if log_model::has_ended(&self.snapshot.state) && self.is_paused() {
            self.user_paused = false;
            self.selection_paused = false;
            self.sync_pause();
        }
        let lines = self.snapshot.lines.clone();
        if let Some(selection) = self.selection {
            self.selection = selection.clamp(&lines);
            if self.selection.is_none() {
                self.end_selection();
            }
        }
        if self.current_match.is_some_and(|id| !lines.contains(id)) {
            self.current_match = None;
        }
        cx.notify();
    }

    /// Whether this tab is the one shown, for the shared log memory budget (see the dock).
    pub fn set_shown(&self, shown: bool) {
        self.session.set_shown(shown);
    }

    #[cfg(test)]
    pub fn is_shown(&self) -> bool {
        self.session.is_shown()
    }

    /// Whether the session reads a workload's pods, whose lines carry a source.
    pub fn is_workload(&self) -> bool {
        matches!(self.request.subject, LogSubject::Workload(_))
    }

    /// How lines are shown, copied and saved now.
    pub fn format(&self) -> LineFormat {
        LineFormat {
            timestamps: self.timestamps,
            source: self.sources && self.is_workload(),
        }
    }

    /// Searches for `text` from now on: the first scan runs on the core's threads. The search
    /// covers workload sources while their prefix is shown.
    fn set_query(&mut self, text: String, cx: &mut Context<Self>) {
        let query = SearchQuery {
            sources: self.format().source,
            ..SearchQuery::new(text)
        };
        if query == self.query {
            return;
        }
        self.query = query.clone();
        self.current_match = None;
        self.search_generation += 1;
        self.search = LogSearch::new(query.clone());
        // The matching lines shown follow the search, once it has stayed for a moment; a filter
        // of the old query on its way is dropped, and what is shown stays until the new one
        // lands.
        if self.matching_only {
            self.refilter(Some(SEARCH_DELAY), cx);
        }
        if query.text.is_empty() {
            self.searching = false;
            self._search = Task::ready(());
            cx.notify();
            return;
        }
        // Typing replaces the scan: only a query that stays for a moment scans the buffer.
        self.scan(LogSearch::new(query), Some(SEARCH_DELAY), cx);
        cx.notify();
    }

    /// Brings `search` up to the lines shown on the core's threads, after `delay`. Its result
    /// is dropped if the query changed meanwhile; lines that arrived meanwhile are caught up
    /// with when the tab is drawn next.
    fn scan(
        &mut self,
        search: LogSearch,
        delay: Option<std::time::Duration>,
        cx: &mut Context<Self>,
    ) {
        self.searching = true;
        let generation = self.search_generation;
        let core = self.core.clone();
        self._search = cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let Ok(lines) = this.read_with(cx, |view, _| view.snapshot.lines.clone()) else {
                return;
            };
            let search = core.search_logs(search, lines).await;
            let _ = this.update(cx, |view, cx| {
                if view.search_generation == generation {
                    view.search = search;
                    view.searching = false;
                    cx.notify();
                }
            });
        });
    }

    /// Brings the search up to the lines shown before the tab is drawn (actions that use its
    /// matches catch up at once: [`LogView::catch_up_now`]). Snapshots do not scan, so a hidden tab scans nothing until it is shown again; then
    /// it catches up at once, or on the core's threads when it is more than
    /// [`CATCH_UP_ON_UI`] lines behind. Nothing to do while a scan runs: it catches up when it
    /// lands.
    pub fn catch_up_search(&mut self, cx: &mut Context<Self>) {
        if self.searching {
            return;
        }
        let lines = &self.snapshot.lines;
        if self.search.pending(lines) > CATCH_UP_ON_UI && !self.query.text.is_empty() {
            self.scan(self.search.clone(), None, cx);
        } else {
            self.search.update(lines);
        }
    }

    /// Brings the search up to the lines shown at once, before an action uses its matches, if
    /// no scan runs on the core's threads (those actions are refused meanwhile:
    /// [`LogView::has_matches`]). A tab on screen is at most a frame behind, so this scans few
    /// lines.
    fn catch_up_now(&mut self) {
        if !self.searching {
            self.search.update(&self.snapshot.lines);
        }
    }

    /// Pauses delivery while the user paused or is selecting.
    fn sync_pause(&self) {
        self.session.set_paused(self.is_paused());
    }

    pub fn is_paused(&self) -> bool {
        self.user_paused || self.selection_paused
    }

    /// Whether the session follows new lines (not the previous instance's logs).
    pub fn follows(&self) -> bool {
        self.request.options().follow
    }

    /// Whether Pause applies: the session follows and has not ended.
    pub fn can_pause(&self) -> bool {
        self.follows() && !self.has_ended()
    }

    /// Nothing more will come.
    pub fn has_ended(&self) -> bool {
        log_model::has_ended(&self.snapshot.state)
    }

    /// What the toolbar's menus offer now.
    fn menu_facts(&self) -> log_model::MenuFacts {
        log_model::MenuFacts {
            timestamps: self.timestamps,
            workload: self.is_workload(),
            sources: self.sources,
            wrap: self.wrap,
            has_lines: self.has_lines(),
            has_matches: self.has_matches(),
            has_selection: self.has_selection(),
            errors_only: self.errors_only,
            matching_only: self.matching_only,
            context: self.filter_spec().is_active().then_some(self.context),
        }
    }

    /// How lines are drawn now.
    fn line_view(&self) -> log_model::LineView<'_> {
        log_model::LineView {
            format: self.format(),
            labels: &self.labels,
            tz: &self.tz,
        }
    }

    /// Selects `selection`, pausing when a selection starts, and focuses the tab.
    fn select(
        &mut self,
        selection: LineSelection,
        reveal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Selecting pauses following, also after a resume while lines stayed selected.
        if self.follows() && !self.is_paused() {
            self.selection_paused = true;
            self.sync_pause();
        }
        if self.selection != Some(selection) {
            self.clear_note();
        }
        self.selection = Some(selection);
        if reveal {
            self.reveal(selection.head);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Drops the selection, and resumes if only the selection paused.
    fn end_selection(&mut self) {
        if self.selection.is_some() {
            self.clear_note();
        }
        self.selection = None;
        self.drag_selecting = false;
        if self.selection_paused {
            self.selection_paused = false;
            self.sync_pause();
        }
    }

    /// Scrolls `id` into view and stops following, so the next frame does not snap back.
    ///
    /// By index, not by `scroll_to_reveal_item`: that measures the way by the heights of the rows
    /// in between, and rows never drawn have none yet, so a far jump could stop short.
    fn reveal(&mut self, id: LineId) {
        let Some(ix) = self.row_index(id) else {
            return;
        };
        self.list.pause_following_tail();
        let shown = self.list.item_is_above_viewport(ix) == Some(false)
            && self.list.item_is_below_viewport(ix) == Some(false);
        if !shown {
            // A few lines of context above the target.
            self.list.scroll_to(ListOffset {
                item_ix: ix.saturating_sub(REVEAL_CONTEXT),
                offset_in_item: px(0.),
            });
        }
    }

    /// Where the save dialog opens: Downloads, else the home folder.
    fn save_dir() -> PathBuf {
        match std::env::home_dir() {
            Some(home) if home.join("Downloads").is_dir() => home.join("Downloads"),
            Some(home) => home,
            None => std::env::temp_dir(),
        }
    }

    /// Says the file is written as shown and not redacted, then asks where to save and writes
    /// the lines `scope` names, as they were shown when the action ran.
    fn save(
        &mut self,
        scope: SaveScope,
        timestamps: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.catch_up_now();
        // A filtered tab writes the rows it shows: all of them, the selected ones, or the
        // matches among them, as a list of lines.
        let (scope, matches) = match (self.filter_rows(), scope) {
            (None, SaveScope::Matches) => (scope, self.search.matches().iter().copied().collect()),
            (None, _) => (scope, Vec::new()),
            (Some(rows), SaveScope::All) => (SaveScope::Matches, rows.iter().copied().collect()),
            (Some(rows), SaveScope::Matches) => (
                SaveScope::Matches,
                rows.iter()
                    .copied()
                    .filter(|id| self.search.is_match(*id))
                    .collect(),
            ),
            (Some(_), SaveScope::Selection) => (
                SaveScope::Matches,
                self.selection
                    .map(|s| self.rows_between(s.start(), s.end()))
                    .unwrap_or_default(),
            ),
        };
        let save = PendingSave {
            scope,
            format: LineFormat {
                timestamps: timestamps.unwrap_or(self.timestamps),
                ..self.format()
            },
            lines: self.snapshot.lines.clone(),
            matches,
            selection: self.selection,
        };
        // Counted, not built: the file's text is built off the UI thread once a path is chosen.
        let count =
            log_model::save_count(&save.lines, save.scope, &save.matches, save.selection) as u64;
        let description = format!(
            "{} will be written as shown{}. The file is not redacted: it holds whatever the \
             pod logged, secrets included.",
            log_model::line_count(count),
            if save.format.timestamps {
                ", with full timestamps (UTC)"
            } else {
                ", without timestamps"
            },
        );
        let view = cx.weak_entity();
        let dialog = DialogText {
            title: "Save logs to a file?".into(),
            text: description,
            ok: "Choose File…".into(),
            cancel: "Cancel".into(),
            id: "save-logs-description",
            default_cancel: false,
        };
        confirm::open(window, cx, dialog, move |_, cx| {
            let save = save.clone();
            let _ = view.update(cx, |view, cx| view.write_lines(save, cx));
        });
    }

    /// Asks where to save, then writes `save`. The write finishes even if the tab closes.
    fn write_lines(&mut self, save: PendingSave, cx: &mut Context<Self>) {
        let PendingSave {
            scope,
            format,
            lines,
            matches,
            selection,
        } = save;
        let name = log_model::suggested_file_name(&self.request.subject, self.request.previous);
        let path = cx.prompt_for_new_path(&Self::save_dir(), Some(&name));
        cx.spawn(async move |this, cx| {
            let path = match path.await {
                Ok(Ok(Some(path))) => path,
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.set_note(
                            Tone::Danger,
                            format!("Could not open the save dialog: {e}"),
                            cx,
                        );
                        cx.notify();
                    });
                    return;
                }
                // Cancelled.
                _ => return,
            };
            let written = cx
                .background_spawn(async move {
                    let (text, count) =
                        log_model::save_text(&lines, scope, &matches, selection, format)
                            .ok_or_else(|| "there are no such lines".to_owned())?;
                    std::fs::write(&path, text)
                        .map(|()| (path, count))
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                match written {
                    Ok((path, count)) => view.set_note(
                        Tone::Muted,
                        format!(
                            "Saved {} to {}, as shown and not redacted.",
                            log_model::line_count(count as u64),
                            path.display()
                        ),
                        cx,
                    ),
                    Err(e) => {
                        view.set_note(Tone::Danger, format!("Could not save the logs: {e}"), cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Shows `text` in the strip under the toolbar, in place of what was there. A muted note
    /// goes after [`NOTE_TIMEOUT`]; an error stays until the next change.
    fn set_note(&mut self, tone: Tone, text: String, cx: &mut Context<Self>) {
        let note = Note { tone, text };
        self.note = Some(note.clone());
        self._note_timer = if tone == Tone::Muted {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(NOTE_TIMEOUT).await;
                let _ = this.update(cx, |view, cx| {
                    if view.note.as_ref() == Some(&note) {
                        view.clear_note();
                        cx.notify();
                    }
                });
            })
        } else {
            Task::ready(())
        };
    }

    fn clear_note(&mut self) {
        self.note = None;
        self._note_timer = Task::ready(());
    }

    fn tone_color(tone: Tone, theme: &Theme) -> Hsla {
        match tone {
            Tone::Muted => theme.muted_foreground,
            Tone::Live => theme.success,
            Tone::Warning => theme.warning,
            Tone::Danger => theme.danger,
        }
    }

    /// How a run of a line is drawn: the prefix in its pod's colour, the timestamp muted, and
    /// matches over the match tint; the current match over a stronger tint, bold and underlined
    /// in the warning colour, so it stands out from the other matches.
    fn highlight(
        part: log_model::Part,
        current: bool,
        source_color: Hsla,
        theme: &Theme,
    ) -> HighlightStyle {
        match part {
            log_model::Part::Source => HighlightStyle {
                color: Some(source_color),
                ..Default::default()
            },
            log_model::Part::Timestamp => HighlightStyle {
                color: Some(theme.muted_foreground),
                ..Default::default()
            },
            log_model::Part::Match if current => HighlightStyle {
                color: Some(theme.foreground),
                background_color: Some(theme::log_current_match(theme)),
                font_weight: Some(FontWeight::BOLD),
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(theme.warning),
                    wavy: false,
                }),
                ..Default::default()
            },
            log_model::Part::Match => HighlightStyle {
                color: Some(theme.foreground),
                background_color: Some(theme::log_match(theme)),
                ..Default::default()
            },
        }
    }

    fn render_line(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        // The item's row: a line still in the buffer.
        let Some((id, line)) = self
            .row_id(ix)
            .and_then(|id| Some((id, self.snapshot.lines.get(id)?)))
        else {
            return div().into_any_element();
        };
        let max = (!self.wrap).then_some(log_model::UNWRAPPED_MAX_BYTES);
        let shown = log_model::shown_line(line, self.line_view(), &self.query, max);
        let selected = self.selection.is_some_and(|s| s.contains(id));
        let color = match line.severity {
            Severity::Error => theme.danger,
            Severity::Warning => theme.warning,
            Severity::Normal => theme.foreground,
        };
        let current = self.current_match == Some(id);
        let palette = theme::log_source_colors(theme);
        let source_color = shown
            .color
            .map_or(theme.info, |c| palette[c % palette.len()]);
        let style = |part| Self::highlight(part, current, source_color, theme);
        // The runs once, then the text split into its three parts without copying it again.
        let parts = log_model::line_parts(&shown);
        let runs = |range: std::ops::Range<usize>| {
            log_model::parts_in(&parts, range)
                .into_iter()
                .map(|(r, part)| (r, style(part)))
                .collect::<Vec<_>>()
        };
        let (source_runs, time_runs, body_runs) = (
            runs(0..shown.source),
            runs(shown.source..shown.prefix),
            runs(shown.prefix..shown.text.len()),
        );
        let mut text = shown.text;
        let body = text.split_off(shown.prefix);
        let time = text.split_off(shown.source);
        let (source_text, has_source, has_time) =
            (text, shown.source > 0, shown.prefix > shown.source);
        let source = line.source.clone().filter(|_| has_source);
        let timestamp = has_time.then_some(line.timestamp).flatten();
        let day = has_time
            .then(|| {
                let lines = &self.snapshot.lines;
                let before = (0..ix)
                    .rev()
                    .filter_map(|p| self.row_id(p).and_then(|id| lines.get(id)));
                log_model::day_separator(before, line, &self.tz)
            })
            .flatten();
        let tz = self.tz.clone();
        let row = h_flex()
            .id(("log-line", id as usize))
            .w_full()
            .px_2()
            .items_start()
            .text_color(color)
            .line_height(px(LINE_HEIGHT))
            .map(|el| {
                if self.wrap {
                    el.min_h(px(LINE_HEIGHT))
                } else {
                    el.h(px(LINE_HEIGHT)).overflow_hidden()
                }
            })
            .when(selected, |el| el.bg(theme.table_active))
            .when(current, |el| el.bg(theme::log_current_line(theme)))
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let extend = event.modifiers.shift;
                if let Err(e) = registry::perform(&SelectLine { line: id, extend }, window, cx) {
                    tracing::warn!("{e}");
                }
            })
            .on_mouse_move(move |event, window, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    let _ = registry::perform(&DragSelection { line: id }, window, cx);
                }
            })
            // The prefix and the timestamp keep their width; wrapped text hangs past them.
            .when(has_source, |el| {
                el.child(
                    div()
                        .id(("log-source", id as usize))
                        .flex_none()
                        .whitespace_nowrap()
                        .child(StyledText::new(source_text).with_highlights(source_runs))
                        .when_some(source, |el, source| {
                            el.tooltip(move |window, cx| {
                                Tooltip::new(format!(
                                    "Pod {}, container {}",
                                    source.pod(),
                                    source.container()
                                ))
                                .build(window, cx)
                            })
                        }),
                )
            })
            .when(has_time, |el| {
                el.child(
                    div()
                        .id(("log-time", id as usize))
                        .flex_none()
                        .whitespace_nowrap()
                        .child(StyledText::new(time).with_highlights(time_runs))
                        .when_some(timestamp, |el, ts| {
                            el.tooltip(move |window, cx| {
                                Tooltip::new(log_model::full_time(ts, &tz)).build(window, cx)
                            })
                        })
                        .test_support(),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when(!self.wrap, |el| el.truncate())
                    .child(StyledText::new(body).with_highlights(body_runs)),
            )
            .when(shown.cut, |el| {
                el.child(
                    div()
                        .id(("log-cut", id as usize))
                        .flex_none()
                        .pl_1()
                        .text_color(theme.muted_foreground)
                        .child("…")
                        .tooltip(|window, cx| {
                            Tooltip::new(
                                "This line is longer than shown; save or copy it, or turn on wrap.",
                            )
                            .build(window, cx)
                        }),
                )
            })
            .test_support();
        match day {
            None => row.into_any_element(),
            Some(day) => v_flex()
                .w_full()
                .child(
                    div()
                        .id(("log-day", id as usize))
                        .h(px(LINE_HEIGHT))
                        .px_2()
                        .flex_none()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_color(theme.muted_foreground)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(day)
                        .test_support(),
                )
                .child(row)
                .into_any_element(),
        }
    }

    /// The toolbar: one row at any width ([`log_model::toolbar_fit`]). The status has its own
    /// slot that truncates (the full text is in its tooltip); the toggles and icons are one
    /// group that never wraps, and in a narrow tab the toggles and Save fold into a "⋯" menu.
    fn render_toolbar(&self, theme: &Theme) -> impl IntoElement {
        let fit = log_model::toolbar_fit(self.laid_out_width);
        let status = log_model::state_text(&self.snapshot.state, !self.is_workload());
        let state_color = Self::tone_color(status.tone, theme);
        let lines = &self.snapshot.lines;
        let ended = self.has_ended();
        let paused = !ended && (self.snapshot.paused || self.is_paused());
        let pause_text = paused.then(|| {
            log_model::pause_text(
                self.snapshot.held_back,
                self.selection_paused && !self.user_paused,
            )
        });
        let following = self.list.is_following_tail();
        let has_query = !self.query.text.is_empty();
        let has_matches = self.has_matches();
        let timestamps = self.timestamps;
        let workload = self.is_workload();
        let sources = self.sources;
        let wrap = self.wrap;
        let count = if self.filtering || self.catching_up {
            "Filtering…".to_owned()
        } else {
            log_model::shown_count(self.is_filtered().then(|| self.row_count()), lines.len())
        };
        let streams = self
            .snapshot
            .pods
            .as_ref()
            .map(|p| log_model::streams_text(p));
        // The status's tooltip says everything the slot may cut, and what the toolbar left out.
        let trimmed = self.snapshot.trimmed;
        let tooltip = {
            let mut parts = vec![status.tooltip.clone().unwrap_or(status.text.clone())];
            parts.extend(pause_text.clone());
            if trimmed {
                parts.push(log_model::TRIMMED_TOOLTIP.to_owned());
            }
            parts.push(count.clone());
            parts.extend(streams.clone());
            if let Some(selector) = self.snapshot.pods.as_ref().and_then(|p| p.selector.clone()) {
                parts.push(format!("Pods matching {selector}"));
            }
            parts.join("\n")
        };
        let facts = self.menu_facts();
        let has_lines = !lines.is_empty();
        // Save writes the rows shown: none when a filter hides every line.
        let can_save = facts.has_lines;
        h_flex()
            .id("log-toolbar")
            .h(px(super::workspace::HEADER_HEIGHT))
            .px_2()
            .gap_x_1p5()
            .flex_none()
            .min_w_0()
            .overflow_hidden()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .whitespace_nowrap()
            // Everything but the controls: it gives way (and is cut) first, so the controls,
            // "⋯" included, stay on screen.
            .child(
                h_flex()
                    .id("log-main")
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .gap_x_1p5()
                    // The status slot: the state, and the pause, cut to fit.
                    .child(
                        h_flex()
                            .id("log-state")
                            .gap_1()
                            .flex_shrink(1.)
                            .min_w(px(56.))
                            .max_w(px(300.))
                            .overflow_hidden()
                            .text_color(state_color)
                            .child(super::theme::status_dot(state_color, true).flex_none())
                            .child(
                                div()
                                    .id("log-state-text")
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                                    .child(status.text)
                                    .test_support(),
                            )
                            .when_some(pause_text, |el, text| {
                                el.child(
                                    div()
                                        .id("log-paused")
                                        .flex_shrink(0.5)
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme.warning)
                                        .child(format!("· {text}"))
                                        .test_support(),
                                )
                            })
                            .when(trimmed, |el| {
                                el.child(
                                    div()
                                        .id("log-trimmed")
                                        .flex_shrink(0.5)
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("· {}", log_model::TRIMMED_TEXT))
                                        .test_support(),
                                )
                            })
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip.clone()).build(window, cx)
                            })
                            .test_support(),
                    )
                    .when(fit.counts, |el| {
                        el.child(
                            div()
                                .id("log-count")
                                .flex_none()
                                .font_features(super::list::tabular_figures())
                                .text_color(theme.muted_foreground)
                                .child(count)
                                .test_support(),
                        )
                        .when_some(
                            self.snapshot.pods.clone(),
                            |el, pods| {
                                el.child(
                                    div()
                                        .id("log-streams")
                                        .min_w_0()
                                        .flex_shrink(1.)
                                        .truncate()
                                        .text_color(if pods.failed() > 0 {
                                            theme.warning
                                        } else {
                                            theme.muted_foreground
                                        })
                                        .child(log_model::streams_text(&pods))
                                        .test_support(),
                                )
                            },
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .w(px(220.))
                            .min_w(px(72.))
                            .flex_shrink(1000.)
                            .overflow_hidden()
                            .child(
                                Input::new(&self.search_input)
                                    .id("log-search")
                                    .xsmall()
                                    .cleanable(true)
                                    .prefix(Icon::new(IconName::Search).xsmall()),
                            ),
                    )
                    .when(has_query, |el| {
                        el.when(fit.match_count, |el| {
                            el.child(
                                div()
                                    .id("log-matches")
                                    .flex_none()
                                    .font_features(super::list::tabular_figures())
                                    .text_color(theme.muted_foreground)
                                    .child(if self.searching {
                                        "Searching…".to_owned()
                                    } else {
                                        let (count, at) = self.shown_match_count();
                                        log_model::match_count_text(count, at)
                                    })
                                    .test_support(),
                            )
                        })
                        // Beside the count while it shows; else among the controls, which
                        // are never cut.
                        .when(fit.match_count, |el| {
                            el.children(Self::match_buttons(has_matches))
                        })
                    })
                    .test_support(),
            )
            // The controls: one group that never wraps.
            .child(
                h_flex()
                    .id("log-controls")
                    .flex_none()
                    .gap_x_1()
                    // How the search reads, and whether it filters: never cut, and in the "⋯"
                    // menu when the tab is too narrow.
                    .when(fit.toggles, |el| {
                        el.child(
                            Button::new("log-matching-only")
                                .xsmall()
                                .ghost()
                                .label("Matching")
                                .selected(self.matching_only)
                                .tooltip("Show only the lines matching the search (M)")
                                .on_click(registry::on_click(ToggleMatchingOnly)),
                        )
                        .when(self.filter_spec().is_active(), |el| {
                            let context = self.context;
                            el.child(
                                Button::new("log-context")
                                    .xsmall()
                                    .ghost()
                                    .label(format!("±{context}"))
                                    .tooltip("Lines shown around each line that passes the filter")
                                    .dropdown_menu(move |menu, _, _| {
                                        log_model::CONTEXT_CHOICES.iter().fold(
                                            menu,
                                            |menu, &lines| {
                                                menu.item(
                                                    PopupMenuItem::new(match lines {
                                                        0 => "No context".to_owned(),
                                                        1 => "1 line around".to_owned(),
                                                        n => format!("{n} lines around"),
                                                    })
                                                    .checked(lines == context)
                                                    .on_click(registry::on_click(SetLogContext {
                                                        lines: lines as u32,
                                                    })),
                                                )
                                            },
                                        )
                                    }),
                            )
                        })
                    })
                    .when(has_query && !fit.match_count, |el| {
                        el.children(Self::match_buttons(has_matches))
                    })
                    .when(fit.toggles, |el| {
                        el.child(
                            Button::new("log-timestamps")
                                .xsmall()
                                .ghost()
                                .label("Timestamps")
                                .selected(timestamps)
                                .tooltip(format!(
                                    "Show or hide timestamps (T), shown in {}",
                                    self.tz_name
                                ))
                                .on_click(registry::on_click(ToggleTimestamps)),
                        )
                        .when(workload, |el| {
                            el.child(
                                Button::new("log-sources")
                                    .xsmall()
                                    .ghost()
                                    .label("Pods")
                                    .selected(sources)
                                    .tooltip("Show or hide each line's pod (P)")
                                    .on_click(registry::on_click(ToggleSources)),
                            )
                        })
                        .child(
                            Button::new("log-wrap")
                                .xsmall()
                                .ghost()
                                .label("Wrap")
                                .selected(wrap)
                                .tooltip("Wrap long lines (W)")
                                .on_click(registry::on_click(ToggleWrap)),
                        )
                        .child(
                            Button::new("log-errors-only")
                                .xsmall()
                                .ghost()
                                .label("Errors")
                                .selected(self.errors_only)
                                .tooltip("Show only warnings and errors (E)")
                                .on_click(registry::on_click(ToggleErrorsOnly)),
                        )
                    })
                    .when(self.can_pause(), |el| {
                        el.child(
                            Button::new("log-pause")
                                .icon(if paused {
                                    IconName::Play
                                } else {
                                    IconName::Pause
                                })
                                .xsmall()
                                .ghost()
                                .tooltip(if paused {
                                    "Resume following (Space)"
                                } else {
                                    "Pause following (Space)"
                                })
                                .on_click(registry::on_click(TogglePause)),
                        )
                    })
                    .when(!following && has_lines, |el| {
                        // Only a live, followed read has anything to follow.
                        el.when(self.follows() && !ended, |el| {
                            el.child(
                                div()
                                    .id("log-not-following")
                                    .text_color(theme.muted_foreground)
                                    .child("Not following")
                                    .test_support(),
                            )
                        })
                        .child(
                            Button::new("log-jump")
                                .icon(IconName::ArrowDown)
                                .xsmall()
                                .ghost()
                                .when(fit.jump_label, |b| b.label("Jump to newest"))
                                .tooltip("Jump to the newest line and follow (End)")
                                .on_click(registry::on_click(JumpToEnd)),
                        )
                    })
                    .map(|el| {
                        if fit.toggles {
                            el.child(
                                Button::new("log-save")
                                    .icon(IconName::FileText)
                                    .xsmall()
                                    .ghost()
                                    .tooltip("Save to a file")
                                    .disabled(!can_save)
                                    .dropdown_menu_with_anchor(
                                        Anchor::TopRight,
                                        move |menu, _, cx| {
                                            fill_menu(
                                                menu,
                                                log_model::save_menu(&facts),
                                                facts.timestamps,
                                                "log-save",
                                                cx,
                                            )
                                        },
                                    ),
                            )
                        } else {
                            el.child(
                                Button::new("log-more")
                                    .icon(IconName::Ellipsis)
                                    .xsmall()
                                    .ghost()
                                    .tooltip("View options and Save")
                                    .dropdown_menu_with_anchor(
                                        Anchor::TopRight,
                                        move |menu, _, cx| {
                                            fill_menu(
                                                menu,
                                                log_model::more_menu(&facts),
                                                facts.timestamps,
                                                "log-more",
                                                cx,
                                            )
                                        },
                                    ),
                            )
                        }
                    })
                    .test_support(),
            )
            .test_support()
    }

    /// The search's previous and next buttons.
    fn match_buttons(has_matches: bool) -> [Button; 2] {
        [
            Button::new("log-prev-match")
                .icon(IconName::ChevronUp)
                .xsmall()
                .ghost()
                .disabled(!has_matches)
                .tooltip(format!(
                    "Previous match ({})",
                    registry::key_hint("secondary-shift-g")
                ))
                .on_click(registry::on_click(PrevMatch)),
            Button::new("log-next-match")
                .icon(IconName::ChevronDown)
                .xsmall()
                .ghost()
                .disabled(!has_matches)
                .tooltip(format!(
                    "Next match ({})",
                    registry::key_hint("secondary-g")
                ))
                .on_click(registry::on_click(NextMatch)),
        ]
    }

    /// The strip under the toolbar: the last transient thing said (a copy or save note, else
    /// the selection), one at a time.
    fn render_notice(&self, theme: &Theme) -> Option<AnyElement> {
        if let Some(note) = &self.note {
            return Some(
                notice(
                    "log-note",
                    note.text.clone(),
                    Self::tone_color(note.tone, theme),
                )
                .test_support()
                .into_any_element(),
            );
        }
        let selection = self.selection?;
        let lines = &self.snapshot.lines;
        let count = self.selected_rows();
        let mut text = format!(
            "{} selected. {} copies and Save writes them as shown, not redacted. Esc clears.",
            log_model::line_count(count),
            registry::key_hint("secondary-c")
        );
        if selection.is_cut(lines) {
            text.push_str(" Some selected lines left the buffer.");
        }
        Some(
            notice("log-selection", text, theme.muted_foreground)
                .test_support()
                .into_any_element(),
        )
    }

    /// An ended session's error, over lines it read: what it means in plain words, and the
    /// server's text under it, wrapped. (Without lines, the empty state says it.)
    fn render_banner(&self, theme: &Theme) -> Option<AnyElement> {
        let LogState::Ended { error, pod_deleted } = &self.snapshot.state else {
            return None;
        };
        // A failure, or a followed stream that stopped on its own; a deleted pod's end shows in
        // the status ("Ended · pod deleted"), and an empty tab says it in its body.
        if self.snapshot.lines.is_empty()
            || (error.is_none() && (self.request.previous || *pod_deleted))
        {
            return None;
        }
        let (color, headline, detail) = match (error, self.snapshot.state.previous_not_available())
        {
            (_, Some(said)) => (
                theme.warning,
                log_model::PREVIOUS_NOT_AVAILABLE.to_owned(),
                Some(said.to_owned()),
            ),
            (Some(error), None) => {
                let explained = log_model::explain(error, !self.is_workload());
                (
                    theme.danger,
                    format!("The stream ended. {}", explained.headline),
                    explained.detail,
                )
            }
            (None, None) => (theme.muted_foreground, "The stream ended.".to_owned(), None),
        };
        Some(
            h_flex()
                .id("log-ended")
                .px_3()
                .py_1()
                .gap_2()
                .items_start()
                .flex_none()
                .text_xs()
                .bg(color.opacity(0.08))
                .border_b_1()
                .border_color(color.opacity(0.3))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().text_color(color).child(headline))
                        .when_some(detail, |el, detail| {
                            el.child(
                                div()
                                    .id("log-ended-detail")
                                    .text_color(theme.muted_foreground)
                                    .child(detail)
                                    .test_support(),
                            )
                        }),
                )
                .when(self.can_restart(), |el| {
                    el.child(div().flex_none().child(reconnect_button()))
                })
                .test_support()
                .into_any_element(),
        )
    }

    /// What to show instead of lines, while there are none.
    fn render_empty(&self, theme: &Theme) -> AnyElement {
        let pod = !self.is_workload();
        let (spinner, color, text, detail): (bool, Hsla, String, Option<String>) = match &self
            .snapshot
            .state
        {
            LogState::Connecting => (true, theme.muted_foreground, "Connecting…".into(), None),
            LogState::Streaming if self.follows() => (
                false,
                theme.muted_foreground,
                "Waiting for log lines…".into(),
                None,
            ),
            LogState::Streaming => (false, theme.muted_foreground, "Reading…".into(), None),
            LogState::Reconnecting { back_off: true, .. } => (
                true,
                theme.warning,
                format!("{}…", log_model::BACK_OFF),
                Some(
                    "The container keeps crashing; the kubelet restarts it after a delay \
                         that grows with each crash."
                        .to_owned(),
                ),
            ),
            LogState::Reconnecting { error, attempt, .. } => (
                true,
                theme.warning,
                "Reconnecting…".into(),
                Some(match error {
                    Some(e) => {
                        let explained = log_model::explain(e, pod);
                        format!(
                            "Attempt {attempt}. {} {}",
                            explained.headline,
                            explained.detail.unwrap_or_default()
                        )
                    }
                    None => format!("Attempt {attempt}"),
                }),
            ),
            state @ LogState::Ended { error: Some(e), .. } => {
                match state.previous_not_available() {
                    Some(kubelet) => (
                        false,
                        theme.warning,
                        log_model::PREVIOUS_NOT_AVAILABLE.to_owned(),
                        Some(format!(
                            "The container may still be restarting. {} said: {kubelet}",
                            log_model::previous_speaker(kubelet)
                        )),
                    ),
                    None => {
                        let explained = log_model::explain(e, pod);
                        (false, theme.danger, explained.headline, explained.detail)
                    }
                }
            }
            // The log was read; the pod just logged nothing before it was deleted.
            LogState::Ended {
                pod_deleted: true, ..
            } => (
                false,
                theme.muted_foreground,
                "No log lines.".into(),
                Some("The pod was deleted.".to_owned()),
            ),
            LogState::Ended { .. } => (false, theme.muted_foreground, "No log lines.".into(), None),
        };
        v_flex()
            .id("log-empty")
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1()
            .p_4()
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(color)
                    .when(spinner, |el| el.child(Spinner::new().small()))
                    .child(text),
            )
            .when_some(detail, |el, detail| {
                el.child(
                    div()
                        .id("log-empty-detail")
                        .max_w(px(640.))
                        .text_xs()
                        .text_center()
                        .text_color(theme.muted_foreground)
                        .child(detail)
                        .test_support(),
                )
            })
            .when(self.can_restart(), |el| el.child(reconnect_button()))
            .test_support()
            .into_any_element()
    }

    /// A filtered tab with no rows: still filtering, or nothing passes (yet: new lines that
    /// pass show as they come).
    fn render_filtered_empty(&self, theme: &Theme) -> AnyElement {
        let (spinner, text) = if self.filtering || self.catching_up {
            (true, "Filtering…".to_owned())
        } else {
            (false, log_model::filter_empty_text(&self.filter_spec()))
        };
        v_flex()
            .id("log-filter-empty")
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1()
            .p_4()
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .when(spinner, |el| el.child(Spinner::new().small()))
                    .child(text),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} in the buffer; new lines that pass show as they come.",
                        log_model::line_count(self.snapshot.lines.len() as u64)
                    )),
            )
            .test_support()
            .into_any_element()
    }
}

/// Reconnect Logs, in an ended or failed tab's banner.
fn reconnect_button() -> Button {
    Button::new("log-reconnect")
        .xsmall()
        .label("Reconnect")
        .tab_stop(false)
        .tooltip(format!(
            "Open the logs again in a new tab beside this one ({})",
            registry::key_hint("secondary-r")
        ))
        .on_click(registry::on_click(ReconnectLogs))
}

/// A toolbar menu with `rows`, each entry performing its registered action, opened from the
/// button `owner`. Saves write timestamps as `timestamps` says (as shown when the menu opened,
/// as its note says). The menu stays as built until it is dismissed.
fn fill_menu(
    menu: PopupMenu,
    rows: Vec<log_model::MenuRow>,
    timestamps: bool,
    owner: &'static str,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    use log_model::{MenuRow, Toggle};
    menu_record::record(cx, owner, || log_model::menu_entries(&rows));
    rows.into_iter().fold(menu, |menu, row| match row {
        MenuRow::Label(text) => menu.label(text),
        MenuRow::Separator => menu.separator(),
        MenuRow::Toggle { label, on, toggle } => {
            let item = PopupMenuItem::new(label).checked(on);
            menu.item(match toggle {
                Toggle::Timestamps => item.on_click(registry::on_click(ToggleTimestamps)),
                Toggle::Sources => item.on_click(registry::on_click(ToggleSources)),
                Toggle::Wrap => item.on_click(registry::on_click(ToggleWrap)),
                Toggle::ErrorsOnly => item.on_click(registry::on_click(ToggleErrorsOnly)),
                Toggle::MatchingOnly => item.on_click(registry::on_click(ToggleMatchingOnly)),
                Toggle::Context(lines) => item.on_click(registry::on_click(SetLogContext {
                    lines: lines as u32,
                })),
            })
        }
        MenuRow::Save {
            label,
            scope,
            enabled,
        } => menu.item(
            PopupMenuItem::new(label)
                .disabled(!enabled)
                .on_click(registry::on_click(SaveLines {
                    lines: scope,
                    timestamps: Some(timestamps),
                })),
        ),
    })
}

impl DockItem for LogView {
    fn tab_key(&self) -> TabKey {
        TabKey::Logs(self.request.clone())
    }

    fn tab_title(&self) -> SharedString {
        log_model::tab_title(&self.request.subject, self.request.previous).into()
    }

    fn tab_tooltip(&self) -> SharedString {
        let r = &self.request;
        let mut text = match &r.subject {
            LogSubject::Pod(target) => {
                let mut text = format!("Logs of {}/{}", target.namespace, target.pod);
                if let Some(container) = &target.container {
                    text.push_str(&format!(", container {container}"));
                }
                text
            }
            LogSubject::Workload(target) => format!(
                "Logs of {} in {}, every pod merged",
                log_model::tab_title(&r.subject, false),
                target.namespace
            ),
        };
        if r.previous {
            text.push_str(" (previous instance)");
        }
        text.push_str(&format!("\non {}", r.cluster));
        text.into()
    }

    fn tab_status(&self) -> TabStatus {
        match &self.snapshot.state {
            LogState::Connecting => TabStatus::Starting,
            LogState::Streaming => TabStatus::Live,
            LogState::Reconnecting { .. } => TabStatus::Retrying,
            LogState::Ended { error: Some(_), .. } => TabStatus::Failed,
            LogState::Ended { .. } => TabStatus::Ended,
        }
    }

    fn tab_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Not a deleted pod's: there is nothing to reconnect to.
    fn can_restart(&self) -> bool {
        matches!(self.snapshot.state, LogState::Ended { pod_deleted, .. } if !pod_deleted)
    }
}

impl Render for LogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.catch_up_search(cx);
        self.catch_up_filter(cx);
        let theme = cx.theme().clone();
        // The toolbar needs the tab's width, known only after layout: when it changed, draw
        // again with the new one.
        let width = f32::from(self.bounds.bounds().size.width);
        let measured = (width > 0.).then_some(width);
        if measured != self.laid_out_width {
            self.laid_out_width = measured;
            // Weak: a closed tab must not live on until the next frame.
            let this = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                let _ = this.update(cx, |_, cx| cx.notify());
            });
        }
        let body = if self.snapshot.lines.is_empty() {
            self.render_empty(&theme)
        } else if self.is_filtered() && self.row_count() == 0 {
            self.render_filtered_empty(&theme)
        } else {
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .font_family(theme.mono_font_family.clone())
                .text_xs()
                .child(
                    list(self.list.clone(), cx.processor(Self::render_line))
                        .size_full()
                        .py_1(),
                )
                .child(
                    div().absolute().inset_0().child(
                        Scrollbar::vertical(&self.list)
                            .id("log-scrollbar")
                            .mode(ScrollbarMode::Hover),
                    ),
                )
                .into_any_element()
        };
        v_flex()
            .id("log-view")
            .key_context(registry::LOG_VIEW)
            .track_focus(&self.focus)
            .map(|el| Self::register_actions(el, cx))
            // not-an-action: ends a mouse selection; the selection itself is SelectLine.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_selecting = false),
            )
            // not-an-action: ends a mouse selection released outside the tab.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_selecting = false),
            )
            .size_full()
            .min_h_0()
            .bg(theme.background)
            .track_scroll(&self.bounds)
            .child(self.render_toolbar(&theme))
            .children(self.render_banner(&theme))
            .children(self.render_notice(&theme))
            .child(body)
            .test_support()
    }
}

#[path = "log_actions.rs"]
mod actions;

#[path = "log_filter.rs"]
mod filter;

#[cfg(any(test, feature = "automation"))]
#[path = "log_state.rs"]
mod state;

#[cfg(test)]
#[path = "log_tests.rs"]
mod tests;
