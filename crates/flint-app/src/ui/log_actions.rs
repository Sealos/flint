//! A log tab's registered actions ([`crate::ui::registry`]), their handlers, and the checks the
//! workspace's guards share with them. The toolbar's buttons, the save menu and clicks on lines
//! dispatch these actions.

use std::sync::Arc;

use flint_core::{LineId, LineSelection};
use gpui_kit::*;

use super::super::registry::{
    ActionError, Bind, ClearSelection, CopyLines, DragSelection, ExtendSelectionDown,
    ExtendSelectionUp, FocusSearch, JumpToEnd, JumpToStart, LeaveSearch, NextMatch, OnView,
    PrevMatch, SaveLines, SaveLogs, SaveMatchingLines, SaveScope, SaveSelectedLines,
    SelectAllLines, SelectLine, SelectNextLine, SelectPrevLine, SetLogContext, ToggleErrorsOnly,
    ToggleMatchingOnly, TogglePause, ToggleSources, ToggleTimestamps, ToggleWrap,
};
use super::{LogView, log_model};
use crate::ui::log_model::Tone;

/// What [`LogView::shown_matches`] was found from: when any of it changes, it is found again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) struct ShownMatchesKey {
    search: u64,
    matches: (usize, Option<LineId>, Option<LineId>),
    filter: u64,
    /// Bumped when rows are replaced: rows with the same ends can differ in between.
    rows_generation: u64,
    rows: (usize, Option<LineId>, Option<LineId>),
    first: LineId,
}

impl LogView {
    /// Every log action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<TogglePause>(Self::toggle_pause)
            .bind::<JumpToEnd>(|view, _, _, cx| {
                // Following means new lines show again: a pause ends too (the selection stays).
                if view.is_paused() {
                    view.user_paused = false;
                    view.selection_paused = false;
                    view.sync_pause();
                }
                view.list.set_follow_mode(FollowMode::Tail);
                cx.notify();
            })
            .bind::<JumpToStart>(|view, _, _, cx| {
                if view.has_lines() {
                    view.list.pause_following_tail();
                    view.list.scroll_to(ListOffset::default());
                    cx.notify();
                }
            })
            .bind::<ToggleTimestamps>(|view, _, _, cx| {
                view.timestamps = !view.timestamps;
                // Day separators come and go with timestamps, so rows change height.
                view.list.remeasure();
                cx.notify();
            })
            .bind::<ToggleSources>(|view, _, _, cx| {
                if !view.is_workload() {
                    return;
                }
                view.sources = !view.sources;
                if view.wrap {
                    view.list.remeasure();
                }
                // The search covers sources only while they are shown.
                let text = std::mem::take(&mut view.query.text);
                view.query = Default::default();
                view.set_query(text, cx);
                cx.notify();
            })
            .bind::<ToggleWrap>(|view, _, _, cx| {
                view.wrap = !view.wrap;
                view.list.remeasure();
                cx.notify();
            })
            .bind::<ToggleMatchingOnly>(|view, _, _, cx| {
                view.matching_only = !view.matching_only;
                view.refilter(None, cx);
            })
            .bind::<ToggleErrorsOnly>(|view, _, _, cx| {
                view.errors_only = !view.errors_only;
                view.refilter(None, cx);
            })
            .bind::<SetLogContext>(|view, action, _, cx| {
                let context = (action.lines as usize).min(flint_core::logs::MAX_CONTEXT);
                if context != view.context {
                    view.context = context;
                    // Also while a first scan runs: it restarts with the new context.
                    if view.filter_spec().is_active() {
                        view.refilter(None, cx);
                    }
                    cx.notify();
                }
            })
            .bind::<FocusSearch>(|view, _, window, cx| {
                let focus = view.search_input.focus_handle(cx);
                window.focus(&focus, cx);
            })
            .bind::<NextMatch>(Self::next_match)
            .bind::<PrevMatch>(Self::prev_match)
            .bind::<SelectPrevLine>(|view, _, window, cx| view.move_line(-1, false, window, cx))
            .bind::<SelectNextLine>(|view, _, window, cx| view.move_line(1, false, window, cx))
            .bind::<ExtendSelectionUp>(|view, _, window, cx| view.move_line(-1, true, window, cx))
            .bind::<ExtendSelectionDown>(|view, _, window, cx| view.move_line(1, true, window, cx))
            .bind::<SelectAllLines>(|view, _, window, cx| {
                if let (Some(first), Some(last)) = (view.row_id(0), view.last_line()) {
                    view.select(LineSelection::new(first, last), false, window, cx);
                }
            })
            // Esc in the lines clears the selection, and nothing else: it never falls through
            // to the detail pane (whose Esc closes it) while the log tab has focus.
            .bind::<ClearSelection>(|view, _, _, cx| {
                if view.selection.is_some() {
                    view.end_selection();
                    cx.notify();
                }
            })
            .bind::<LeaveSearch>(Self::leave_search)
            .bind::<CopyLines>(Self::copy_lines)
            .bind::<SaveLogs>(|view, _, window, cx| {
                if view.check_save(SaveScope::All).is_ok() {
                    view.save(SaveScope::All, None, window, cx);
                }
            })
            .bind::<SaveMatchingLines>(|view, _, window, cx| {
                view.save_scope(SaveScope::Matches, window, cx)
            })
            .bind::<SaveSelectedLines>(|view, _, window, cx| {
                view.save_scope(SaveScope::Selection, window, cx)
            })
            .bind::<SaveLines>(
                |view, action, window, cx| match view.check_save(action.lines) {
                    Ok(()) => view.save(action.lines, action.timestamps, window, cx),
                    Err(e) => tracing::warn!("{e}"),
                },
            )
            .bind::<SelectLine>(Self::select_line)
            .bind::<DragSelection>(Self::drag_selection)
    }

    /// Saves the lines of `scope` with timestamps as shown, if it has any.
    fn save_scope(&mut self, scope: SaveScope, window: &mut Window, cx: &mut Context<Self>) {
        match self.check_save(scope) {
            Ok(()) => self.save(scope, None, window, cx),
            Err(e) => tracing::warn!("{e}"),
        }
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }

    /// The newest line shown (in a filtered tab, the newest row).
    fn last_line(&self) -> Option<LineId> {
        self.last_row()
    }

    /// Lines are shown (rows, in a filtered tab), so there is something to select, search or
    /// save.
    pub(in crate::ui) fn has_lines(&self) -> bool {
        self.row_count() > 0
    }

    /// Jump to Newest applies: there are rows, or a filter shows none yet (following the tail
    /// again shows what comes).
    pub(in crate::ui) fn can_jump_to_end(&self) -> bool {
        self.has_lines() || self.is_filtered()
    }

    /// A selection exists, shown or hidden by a filter: clearing it also ends the pause it
    /// started.
    pub(in crate::ui) fn is_selecting(&self) -> bool {
        self.selection.is_some()
    }

    /// Lines are selected, and some of them shown (a filter can hide them all).
    pub(in crate::ui) fn has_selection(&self) -> bool {
        self.selected_rows() > 0
    }

    /// How many selected lines are shown: what copy and save would write. Counted from where
    /// the selection's ends fall among the rows, without listing them.
    pub(in crate::ui) fn selected_rows(&self) -> u64 {
        match self.selection {
            None => 0,
            Some(s) if self.is_filtered() => self.rows_count_between(s.start(), s.end()) as u64,
            Some(s) => s.present_len(&self.snapshot.lines),
        }
    }

    /// The matches among the rows shown in a filtered tab, oldest first. Found again only when
    /// the search's matches or the rows changed, in one walk over both (they are sorted), so a
    /// redraw does not go over every match. While lines only come and go (same search, filter
    /// and rows), only the end is walked again: see [`LogView::shown_matches_from`].
    pub(in crate::ui) fn shown_matches(&self) -> Arc<[LineId]> {
        let rows = self.filter_rows();
        let matches = self.search.matches();
        let key = ShownMatchesKey {
            search: self.search_generation,
            matches: (
                matches.len(),
                matches.front().copied(),
                matches.back().copied(),
            ),
            filter: self.filter_generation,
            rows_generation: self.rows_generation,
            rows: rows.map_or((0, None, None), |r| {
                (r.len(), r.front().copied(), r.back().copied())
            }),
            first: self.snapshot.lines.first_id(),
        };
        let mut cache = self.shown_matches.borrow_mut();
        if let Some((cached, shown)) = &*cache
            && *cached == key
        {
            return shown.clone();
        }
        let shown: Arc<[LineId]> = match rows {
            None => Arc::from([]),
            Some(rows) => {
                let (kept, from) = match &*cache {
                    Some((old, shown))
                        if old.search == key.search
                            && old.filter == key.filter
                            && old.rows_generation == key.rows_generation =>
                    {
                        let from = self.shown_matches_from(old);
                        let first = key.first;
                        let start = shown.partition_point(|id| *id < first);
                        let end = shown.partition_point(|id| *id < from);
                        (&shown[start..end.max(start)], from)
                    }
                    _ => (&[][..], 0),
                };
                let mut rows = rows
                    .range(rows.partition_point(|id| *id < from)..)
                    .peekable();
                let found = matches
                    .range(matches.partition_point(|id| *id < from)..)
                    .copied()
                    .filter(|id| {
                        while rows.next_if(|row| **row < *id).is_some() {}
                        rows.peek().is_some_and(|row| **row == *id)
                            && self.snapshot.lines.contains(*id)
                    });
                kept.iter().copied().chain(found).collect()
            }
        };
        *cache = Some((key, shown.clone()));
        shown
    }

    /// From which line [`LogView::shown_matches`] found from `old` can change while only lines
    /// come and go: line ids only grow, a new match comes after the last one scanned, and a new
    /// row at most `context` lines before the last row (the context before a new match). Before
    /// that, only evicted lines drop out.
    fn shown_matches_from(&self, old: &ShownMatchesKey) -> LineId {
        let context = self.filter.as_ref().map_or(0, |f| f.spec().context) as LineId;
        let rows = old.rows.2.map_or(0, |back| back.saturating_sub(context));
        let matches = old.matches.2.map_or(0, |back| back + 1);
        rows.min(matches)
    }

    /// Whether a line shown matches the search, counting lines that arrived since the last
    /// scan and not evicted ones: what Next and Previous Match, and Save Matching Lines, need.
    /// `false` while a scan runs on the core's threads, whose matches are not known yet.
    pub(in crate::ui) fn has_matches(&self) -> bool {
        if self.searching {
            return false;
        }
        match self.filter_rows() {
            None => self.search.has_match_in(&self.snapshot.lines),
            // Filtered: a match among the rows shown.
            Some(_) => !self.shown_matches().is_empty(),
        }
    }

    /// The matches Next and Previous Match go to: every match, or in a filtered tab those
    /// among the rows shown. How many there are, and which of them is the current one.
    pub(in crate::ui) fn shown_match_count(&self) -> (usize, Option<usize>) {
        if !self.is_filtered() {
            let lines = &self.snapshot.lines;
            let evicted = self
                .search
                .matches()
                .partition_point(|m| *m < lines.first_id());
            let at = self.current_match.and_then(|id| {
                self.search
                    .matches()
                    .binary_search(&id)
                    .ok()
                    .map(|ix| ix - evicted)
            });
            return (self.search.count_in(lines), at);
        }
        let shown = self.shown_matches();
        let at = self
            .current_match
            .and_then(|id| shown.binary_search(&id).ok());
        (shown.len(), at)
    }

    /// [`SelectLine`] and [`DragSelection`] take a line shown now.
    pub(in crate::ui) fn check_line<A: Action>(&self, line: LineId) -> Result<(), ActionError> {
        if self.row_index(line).is_some() {
            Ok(())
        } else {
            Err(ActionError::rejected::<A>(format!(
                "line {line} is not shown (it left the buffer, has not arrived, or is filtered \
                 out)"
            )))
        }
    }

    /// [`SaveLines`] needs lines in the chosen scope: a search with matches, or a selection.
    pub(in crate::ui) fn check_save(&self, scope: SaveScope) -> Result<(), ActionError> {
        let reason = match scope {
            _ if !self.has_lines() => Some("no lines are shown"),
            SaveScope::All => None,
            SaveScope::Matches if self.searching => Some("still searching…"),
            SaveScope::Matches if !self.has_matches() => Some("no lines match the search"),
            SaveScope::Matches => None,
            SaveScope::Selection if self.selection.is_none() => Some("no lines are selected"),
            SaveScope::Selection if !self.has_selection() => {
                Some("no selected line is shown (the filter hides them)")
            }
            SaveScope::Selection => None,
        };
        match reason {
            Some(reason) => Err(ActionError::rejected::<SaveLines>(reason)),
            None => Ok(()),
        }
    }

    /// Esc in the search field: the first clears the search, the next moves focus back to the
    /// lines.
    fn leave_search(&mut self, _: &LeaveSearch, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search_input.read(cx).value().to_string();
        if text.is_empty() {
            window.focus(&self.focus, cx);
        } else {
            self.search_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.set_query(String::new(), cx);
        }
        cx.notify();
    }

    fn toggle_pause(&mut self, _: &TogglePause, _: &mut Window, cx: &mut Context<Self>) {
        if !self.can_pause() {
            return;
        }
        if self.is_paused() {
            // Resuming also ends the pause a selection started; the selection stays.
            self.user_paused = false;
            self.selection_paused = false;
        } else {
            self.user_paused = true;
        }
        self.sync_pause();
        cx.notify();
    }

    /// Goes to the next match, wrapping around. Nothing while a scan runs on the core's threads
    /// (the guard refuses it then).
    pub(super) fn next_match(&mut self, _: &NextMatch, _: &mut Window, cx: &mut Context<Self>) {
        if self.searching {
            return;
        }
        self.catch_up_now();
        let next = self.shown_match(self.current_match, true);
        self.go_to_match(next, cx);
    }

    /// Goes to the previous match, wrapping around; see [`LogView::next_match`].
    pub(super) fn prev_match(&mut self, _: &PrevMatch, _: &mut Window, cx: &mut Context<Self>) {
        if self.searching {
            return;
        }
        self.catch_up_now();
        let prev = self.shown_match(self.current_match, false);
        self.go_to_match(prev, cx);
    }

    /// The next (or previous) match after `from` that is shown, wrapping around: in a tab
    /// filtered to errors, matches that are not errors are skipped.
    fn shown_match(&self, from: Option<LineId>, forward: bool) -> Option<LineId> {
        let step = |at: Option<LineId>| {
            if forward {
                self.search.next_after(at)
            } else {
                self.search.prev_before(at)
            }
        };
        if !self.is_filtered() {
            // Over matches already evicted.
            let mut at = step(from).or_else(|| step(None));
            for _ in 0..self.search.count() {
                let id = at?;
                if self.row_index(id).is_some() {
                    return Some(id);
                }
                at = step(Some(id)).or_else(|| step(None));
            }
            return None;
        }
        // Filtered: the next match among the rows shown, wrapping around.
        let shown = self.shown_matches();
        let next = match (from, forward) {
            (Some(at), true) => shown.get(shown.partition_point(|id| *id <= at)),
            (Some(at), false) => shown
                .partition_point(|id| *id < at)
                .checked_sub(1)
                .and_then(|ix| shown.get(ix)),
            (None, _) => None,
        };
        next.or(if forward { shown.first() } else { shown.last() })
            .copied()
    }

    fn go_to_match(&mut self, id: Option<LineId>, cx: &mut Context<Self>) {
        if let Some(id) = id {
            self.current_match = Some(id);
            self.reveal(id);
            cx.notify();
        }
    }

    /// Up and Down move a one-line selection; with Shift they extend it. With nothing selected
    /// they start at the newest line.
    fn move_line(&mut self, delta: i64, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(last) = self.last_line() else {
            return;
        };
        let lines = &self.snapshot.lines;
        let selection = match self.selection {
            None => LineSelection::line(last),
            // A filtered tab moves from row to row, over the lines it hides.
            Some(s) if self.is_filtered() => {
                let Some(head) = self.row_from(s.head, delta) else {
                    return;
                };
                if extend {
                    s.extend_to(head)
                } else {
                    LineSelection::line(head)
                }
            }
            Some(s) if extend => s.move_head(delta, lines),
            Some(s) => LineSelection::line(s.move_head(delta, lines).head),
        };
        self.select(selection, true, window, cx);
    }

    fn select_line(&mut self, action: &SelectLine, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.check_line::<SelectLine>(action.line) {
            tracing::warn!("{e}");
            return;
        }
        let selection = match self.selection {
            Some(s) if action.extend => s.extend_to(action.line),
            _ => LineSelection::line(action.line),
        };
        self.drag_selecting = true;
        self.select(selection, false, window, cx);
    }

    /// Extends the selection while the mouse button that started it is held.
    fn drag_selection(
        &mut self,
        action: &DragSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.selection.filter(|_| self.drag_selecting) else {
            return;
        };
        if selection.head != action.line && self.check_line::<DragSelection>(action.line).is_ok() {
            self.select(selection.extend_to(action.line), false, window, cx);
        }
    }

    /// Copies the selected lines as shown now. The text is built off the UI thread (a million
    /// lines take a fifth of a second), then put on the clipboard, also if the tab closed
    /// meanwhile (as a save finishes). Only the app's latest copy lands ([`LatestCopy`]): an
    /// older one still being built, in this tab or another, never replaces it.
    fn copy_lines(&mut self, _: &CopyLines, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            cx.propagate();
            return;
        };
        let lines = self.snapshot.lines.clone();
        let format = self.format();
        // A filtered tab copies the selected rows it shows, not the lines it hides between.
        let rows = self
            .is_filtered()
            .then(|| self.rows_between(selection.start(), selection.end()));
        let count = rows
            .as_ref()
            .map_or_else(|| selection.present_len(&lines), |r| r.len() as u64);
        if count == 0 {
            self.set_note(
                Tone::Muted,
                "Nothing copied: no selected line is shown.".to_owned(),
                cx,
            );
            cx.notify();
            return;
        }
        let copy = LatestCopy::start(cx);
        cx.spawn(async move |this, cx| {
            let text = cx
                .background_spawn(async move {
                    match rows {
                        Some(rows) => {
                            // As `text_as` copies: newlines between lines, none after the last,
                            // so a pasted line is not sent as typed and entered.
                            log_model::save_text(&lines, SaveScope::Matches, &rows, None, format)
                                .map(|(mut text, _)| {
                                    if text.ends_with('\n') {
                                        text.pop();
                                    }
                                    text
                                })
                                .unwrap_or_default()
                        }
                        None => selection.text_as(&lines, format),
                    }
                })
                .await;
            if !cx.update(|cx| LatestCopy::land(copy, text, cx)) {
                return;
            }
            let _ = this.update(cx, |view, cx| {
                view.set_note(
                    Tone::Muted,
                    format!(
                        "Copied {} as shown, not redacted.",
                        log_model::line_count(count)
                    ),
                    cx,
                );
                cx.notify();
            });
        })
        .detach();
    }
}

/// The app's latest copy of log lines, by number: copies are built off the UI thread, and
/// one asked for later (in any tab) must not be replaced by an older one that took longer.
#[derive(Default)]
pub(in crate::ui) struct LatestCopy(u64);

impl Global for LatestCopy {}

impl LatestCopy {
    /// A new copy, now the latest: its number.
    pub(in crate::ui) fn start(cx: &mut App) -> u64 {
        let latest = cx.default_global::<LatestCopy>();
        latest.0 += 1;
        latest.0
    }

    /// Puts copy `copy`'s `text` on the clipboard if no copy was asked for since. Whether it
    /// did.
    pub(in crate::ui) fn land(copy: u64, text: String, cx: &mut App) -> bool {
        let latest = cx.try_global::<LatestCopy>().is_some_and(|l| l.0 == copy);
        if latest {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    // covers UP-44: of copies in flight, the latest asked for wins, whichever lands last.
    #[gpui_kit::test]
    fn only_the_latest_copy_lands(cx: &mut gpui_kit::TestAppContext) {
        use super::LatestCopy;
        let clipboard =
            |cx: &mut gpui_kit::TestAppContext| cx.read_from_clipboard().and_then(|c| c.text());
        let (slow, fast) = cx.update(|cx| (LatestCopy::start(cx), LatestCopy::start(cx)));
        assert!(cx.update(|cx| LatestCopy::land(fast, "fast".into(), cx)));
        assert!(!cx.update(|cx| LatestCopy::land(slow, "slow".into(), cx)));
        assert_eq!(clipboard(cx).as_deref(), Some("fast"));
        // In order, each lands.
        let next = cx.update(LatestCopy::start);
        assert!(cx.update(|cx| LatestCopy::land(next, "next".into(), cx)));
        assert_eq!(clipboard(cx).as_deref(), Some("next"));
    }

    // The handlers run through a window in `log_tests.rs`.
    #[test]
    fn every_log_action_is_bound() {
        assert_binds_scope(Scope::Logs, super::LogView::bind_actions);
    }
}
