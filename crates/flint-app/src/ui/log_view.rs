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
//!   keeps up with new lines snapshot by snapshot; matches are highlighted with
//!   [`flint_core::SearchQuery::match_ranges`].
//! - **Selection** is a [`LineSelection`] of line ids: copy and save write its lines as shown
//!   (timestamps only when shown), without redaction. AI log analysis (#56) reads the same
//!   selection ([`LogView::selection_text`]).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use flint_core::{
    ClusterId, CoreHandle, LineFormat, LineId, LineSelection, LogOptions, LogSearch, LogSession,
    LogSnapshot, LogState, LogSubject, SearchQuery, Severity,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, Theme,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::dock::{DockItem, TabKey, TabStatus};
use super::log_model::{self, ListChange, Tone};
use super::notice::{notice, notice_with};
use super::registry::{
    self, DragSelection, JumpToEnd, NextMatch, PrevMatch, ReconnectLogs, SaveLines, SaveScope,
    SelectLine, TogglePause, ToggleSources, ToggleTimestamps, ToggleWrap,
};
use super::theme;

/// The height of one unwrapped log line.
pub const LINE_HEIGHT: f32 = 18.;
/// Lines kept above a line scrolled into view (a match, the selection's head).
const REVEAL_CONTEXT: usize = 3;
/// How long a search query must stay unchanged before the buffer is scanned for it.
const SEARCH_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

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

/// A line under the toolbar after a copy or a save.
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
    /// Bumped with every query, so a slow first scan of an old query is dropped.
    search_generation: u64,
    /// The first scan of `query` runs on the core's threads.
    searching: bool,
    current_match: Option<LineId>,
    search_input: Entity<InputState>,
    note: Option<Note>,
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
            search_generation: 0,
            searching: false,
            current_match: None,
            search_input,
            note: None,
            focus: cx.focus_handle(),
            _pump: pump,
            _search: Task::ready(()),
            _subscriptions: vec![on_search],
        }
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

    /// Takes in a newer snapshot of the session: splices the list, keeps the selection on the
    /// lines still present, and brings the search up to date.
    pub fn set_snapshot(&mut self, snapshot: Arc<LogSnapshot>, cx: &mut Context<Self>) {
        let old = &self.snapshot.lines;
        let new = &snapshot.lines;
        match log_model::list_change(old.first_id()..old.end_id(), new.first_id()..new.end_id()) {
            ListChange::Unchanged => {}
            ListChange::Splice { evicted, appended } => {
                if evicted > 0 {
                    self.list.splice(0..evicted, 0);
                }
                if appended > 0 {
                    let end = self.list.item_count();
                    self.list.splice(end..end, appended);
                }
            }
            ListChange::Reset(count) => self.list.reset(count),
        }
        self.snapshot = snapshot;
        let lines = self.snapshot.lines.clone();
        if let Some(selection) = self.selection {
            self.selection = selection.clamp(&lines);
            if self.selection.is_none() {
                self.end_selection();
            }
        }
        // While a first scan runs, it catches up when it lands; scanning here would scan the
        // whole buffer on the UI thread.
        if !self.searching {
            self.search.update(&lines);
        }
        if self.current_match.is_some_and(|id| !lines.contains(id)) {
            self.current_match = None;
        }
        cx.notify();
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
        if query.text.is_empty() {
            self.searching = false;
            self._search = Task::ready(());
            cx.notify();
            return;
        }
        self.searching = true;
        let generation = self.search_generation;
        let core = self.core.clone();
        self._search = cx.spawn(async move |this, cx| {
            // Typing replaces this task: only a query that stays for a moment scans the buffer.
            cx.background_executor().timer(SEARCH_DELAY).await;
            let Ok(lines) = this.read_with(cx, |view, _| view.snapshot.lines.clone()) else {
                return;
            };
            let search = core.search_logs(LogSearch::new(query), lines).await;
            let _ = this.update(cx, |view, cx| {
                if view.search_generation == generation {
                    view.search = search;
                    view.search.update(&view.snapshot.lines);
                    view.searching = false;
                    cx.notify();
                }
            });
        });
        cx.notify();
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
        self.selection = Some(selection);
        if reveal {
            self.reveal(selection.head);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Drops the selection, and resumes if only the selection paused.
    fn end_selection(&mut self) {
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
        let lines = &self.snapshot.lines;
        if !lines.contains(id) {
            return;
        }
        let ix = (id - lines.first_id()) as usize;
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

    /// The selected lines as shown, without redaction: what copy, save and AI log analysis
    /// (#56) use.
    pub fn selection_text(&self) -> Option<String> {
        let selection = self.selection?;
        Some(selection.text_as(&self.snapshot.lines, self.format()))
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
        let save = PendingSave {
            scope,
            format: LineFormat {
                timestamps: timestamps.unwrap_or(self.timestamps),
                ..self.format()
            },
            lines: self.snapshot.lines.clone(),
            matches: match scope {
                SaveScope::Matches => self.search.matches().iter().copied().collect(),
                _ => Vec::new(),
            },
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
                ", with timestamps"
            } else {
                ", without timestamps"
            },
        );
        let view = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, save) = (view.clone(), save.clone());
            alert
                .confirm()
                .title("Save logs to a file?")
                .description(description.clone())
                .ok_text("Choose File…")
                .cancel_text("Cancel")
                .on_ok(move |_, _, cx| {
                    let save = save.clone();
                    let _ = view.update(cx, |view, cx| view.write_lines(save, cx));
                    true
                })
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
                        view.set_note(Tone::Danger, format!("Could not open the save dialog: {e}"));
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
                    ),
                    Err(e) => view.set_note(Tone::Danger, format!("Could not save the logs: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_note(&mut self, tone: Tone, text: String) {
        self.note = Some(Note { tone, text });
    }

    fn tone_color(tone: Tone, theme: &Theme) -> Hsla {
        match tone {
            Tone::Muted => theme.muted_foreground,
            Tone::Live => theme.success,
            Tone::Warning => theme.warning,
            Tone::Danger => theme.danger,
        }
    }

    fn render_line(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some((id, line)) = self.snapshot.lines.at(ix) else {
            return div().into_any_element();
        };
        let max = (!self.wrap).then_some(log_model::UNWRAPPED_MAX_BYTES);
        let shown = log_model::shown_line(line, self.format(), &self.query, max);
        let selected = self.selection.is_some_and(|s| s.contains(id));
        let color = match line.severity {
            Severity::Error => theme.danger,
            Severity::Warning => theme.warning,
            Severity::Normal => theme.foreground,
        };
        let match_bg = if self.current_match == Some(id) {
            theme::log_current_match(theme)
        } else {
            theme::log_match(theme)
        };
        // The source prefix in the info color, so it reads apart from the line, the timestamp
        // muted, and matches in the foreground color over the match tint.
        let highlights = log_model::line_parts(&shown)
            .into_iter()
            .map(|(range, part)| {
                let style = match part {
                    log_model::Part::Source => HighlightStyle {
                        color: Some(theme.info),
                        ..Default::default()
                    },
                    log_model::Part::Timestamp => HighlightStyle {
                        color: Some(theme.muted_foreground),
                        ..Default::default()
                    },
                    log_model::Part::Match => HighlightStyle {
                        color: Some(theme.foreground),
                        background_color: Some(match_bg),
                        ..Default::default()
                    },
                };
                (range, style)
            });
        let text = StyledText::new(shown.text).with_highlights(highlights);
        h_flex()
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when(!self.wrap, |el| el.truncate())
                    .child(text),
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
            .test_support()
            .into_any_element()
    }

    fn render_toolbar(&self, theme: &Theme) -> impl IntoElement {
        let (tone, state) = log_model::state_text(&self.snapshot.state);
        let state_color = Self::tone_color(tone, theme);
        let full_state = state.clone();
        let lines = &self.snapshot.lines;
        let paused = self.snapshot.paused || self.is_paused();
        let following = self.list.is_following_tail();
        let has_query = !self.query.text.is_empty();
        let has_matches = self.search.count() > 0;
        let has_selection = self.selection.is_some();
        let timestamps = self.timestamps;
        h_flex()
            .id("log-toolbar")
            // A narrow dock wraps the controls onto a second row rather than clipping the last
            // ones (Save among them).
            .flex_wrap()
            .min_h(px(super::workspace::HEADER_HEIGHT))
            .py_0p5()
            .px_2()
            .gap_x_1p5()
            .flex_none()
            .min_w_0()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .child(
                h_flex()
                    .id("log-state")
                    .gap_1()
                    .flex_shrink(1.)
                    .min_w(px(60.))
                    .max_w(px(260.))
                    .text_color(state_color)
                    .child(div().size_1p5().flex_none().rounded_full().bg(state_color))
                    .child(div().min_w_0().truncate().child(state))
                    .tooltip(move |window, cx| Tooltip::new(full_state.clone()).build(window, cx))
                    .test_support(),
            )
            .child(
                div()
                    .id("log-count")
                    .flex_none()
                    .font_features(super::list::tabular_figures())
                    .text_color(theme.muted_foreground)
                    .child(log_model::line_count(lines.len() as u64))
                    .test_support(),
            )
            .when_some(self.snapshot.pods.clone(), |el, pods| {
                let selector = pods.selector.clone();
                el.child(
                    div()
                        .id("log-streams")
                        .flex_none()
                        .text_color(if pods.failed() > 0 {
                            theme.warning
                        } else {
                            theme.muted_foreground
                        })
                        .child(log_model::streams_text(&pods))
                        .when_some(selector, |el, selector| {
                            el.tooltip(move |window, cx| {
                                Tooltip::new(format!("Pods matching {selector}")).build(window, cx)
                            })
                        })
                        .test_support(),
                )
            })
            .when(paused, |el| {
                el.child(
                    div()
                        .id("log-paused")
                        .flex_none()
                        .text_color(theme.warning)
                        .child(log_model::pause_text(
                            self.snapshot.held_back,
                            self.selection_paused && !self.user_paused,
                        ))
                        .test_support(),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .w(px(220.))
                    .min_w(px(80.))
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
                el.child(
                    div()
                        .id("log-matches")
                        .flex_none()
                        .font_features(super::list::tabular_figures())
                        .text_color(theme.muted_foreground)
                        .child(if self.searching {
                            "Searching…".to_owned()
                        } else {
                            log_model::match_text(&self.search, self.current_match)
                        })
                        .test_support(),
                )
                .child(
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
                )
                .child(
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
                )
            })
            .child(
                Button::new("log-timestamps")
                    .xsmall()
                    .ghost()
                    .label("Timestamps")
                    .selected(timestamps)
                    .tooltip("Show or hide timestamps (T)")
                    .on_click(registry::on_click(ToggleTimestamps)),
            )
            .when(self.is_workload(), |el| {
                el.child(
                    Button::new("log-sources")
                        .xsmall()
                        .ghost()
                        .label("Pods")
                        .selected(self.sources)
                        .tooltip("Show or hide each line's pod and container (P)")
                        .on_click(registry::on_click(ToggleSources)),
                )
            })
            .child(
                Button::new("log-wrap")
                    .xsmall()
                    .ghost()
                    .label("Wrap")
                    .selected(self.wrap)
                    .tooltip("Wrap long lines (W)")
                    .on_click(registry::on_click(ToggleWrap)),
            )
            .when(self.follows(), |el| {
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
            .when(!following && !lines.is_empty(), |el| {
                el.child(
                    Button::new("log-jump")
                        .icon(IconName::ArrowDown)
                        .xsmall()
                        .ghost()
                        .tooltip("Jump to the newest line and follow (End)")
                        .on_click(registry::on_click(JumpToEnd)),
                )
            })
            .child(
                Button::new("log-save")
                    .icon(IconName::FileText)
                    .xsmall()
                    .ghost()
                    .tooltip("Save to a file")
                    .disabled(lines.is_empty())
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        let item = |label: &str, lines, enabled: bool| {
                            PopupMenuItem::new(label.to_owned())
                                .disabled(!enabled)
                                .on_click(registry::on_click(SaveLines {
                                    lines,
                                    timestamps: Some(timestamps),
                                }))
                        };
                        menu.label(if timestamps {
                            "As shown, with timestamps. Not redacted."
                        } else {
                            "As shown, without timestamps. Not redacted."
                        })
                        .item(item("Save All Lines…", SaveScope::All, true))
                        .item(item(
                            "Save Matching Lines…",
                            SaveScope::Matches,
                            has_matches,
                        ))
                        .item(item(
                            "Save Selected Lines…",
                            SaveScope::Selection,
                            has_selection,
                        ))
                    }),
            )
    }

    /// The strips under the toolbar: the selection, an ended session's error, and the last
    /// copy or save.
    fn render_notes(&self, theme: &Theme) -> Vec<AnyElement> {
        let mut notes = Vec::new();
        if let Some(selection) = self.selection {
            let lines = &self.snapshot.lines;
            let count = selection.lines(lines).count() as u64;
            let mut text = format!(
                "{} selected. {} copies and Save writes them as shown, not redacted. Esc clears.",
                log_model::line_count(count),
                registry::key_hint("secondary-c")
            );
            if selection.is_cut(lines) {
                text.push_str(" Some selected lines left the buffer.");
            }
            notes.push(
                notice("log-selection", text, theme.muted_foreground)
                    .test_support()
                    .into_any_element(),
            );
        }
        // A failure, or a followed stream that stopped on its own; a deleted pod's end shows in
        // the status ("Ended · pod deleted").
        if let LogState::Ended { error, pod_deleted } = &self.snapshot.state
            && !self.snapshot.lines.is_empty()
            && (error.is_some() || !(self.request.previous || *pod_deleted))
        {
            let (text, color) = match error {
                Some(error) => (format!("The stream ended: {error}"), theme.danger),
                None => ("The stream ended.".to_owned(), theme.muted_foreground),
            };
            notes.push(
                notice_with("log-ended", text, color, reconnect_button())
                    .test_support()
                    .into_any_element(),
            );
        }
        if let Some(note) = &self.note {
            notes.push(
                notice(
                    "log-note",
                    note.text.clone(),
                    Self::tone_color(note.tone, theme),
                )
                .test_support()
                .into_any_element(),
            );
        }
        notes
    }

    /// What to show instead of lines, while there are none.
    fn render_empty(&self, theme: &Theme) -> AnyElement {
        let (spinner, color, text, detail): (bool, Hsla, &str, Option<String>) =
            match &self.snapshot.state {
                LogState::Connecting => (true, theme.muted_foreground, "Connecting…", None),
                LogState::Streaming if self.follows() => (
                    false,
                    theme.muted_foreground,
                    "Waiting for log lines…",
                    None,
                ),
                LogState::Streaming => (false, theme.muted_foreground, "Reading…", None),
                LogState::Reconnecting { error, attempt, .. } => (
                    true,
                    theme.warning,
                    "Reconnecting…",
                    Some(match error {
                        Some(e) => format!("Attempt {attempt}: {e}"),
                        None => format!("Attempt {attempt}"),
                    }),
                ),
                LogState::Ended { error: Some(e), .. } => (
                    false,
                    theme.danger,
                    "Could not read the logs.",
                    Some(e.clone()),
                ),
                // The log was read; the pod just logged nothing before it was deleted.
                LogState::Ended {
                    pod_deleted: true, ..
                } => (
                    false,
                    theme.muted_foreground,
                    "No log lines.",
                    Some("The pod was deleted.".to_owned()),
                ),
                LogState::Ended { .. } => (false, theme.muted_foreground, "No log lines.", None),
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
                        .max_w(px(640.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(detail),
                )
            })
            // Not for a deleted pod: there is nothing to reconnect to.
            .when(
                matches!(
                    self.snapshot.state,
                    LogState::Ended {
                        pod_deleted: false,
                        ..
                    }
                ),
                |el| el.child(reconnect_button()),
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body = if self.snapshot.lines.is_empty() {
            self.render_empty(&theme)
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
            .child(self.render_toolbar(&theme))
            .children(self.render_notes(&theme))
            .child(body)
            .test_support()
    }
}

#[path = "log_actions.rs"]
mod actions;

#[cfg(test)]
#[path = "log_tests.rs"]
mod tests;
