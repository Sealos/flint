//! A log tab's registered actions ([`crate::ui::registry`]), their handlers, and the checks the
//! workspace's guards share with them. The toolbar's buttons, the save menu and clicks on lines
//! dispatch these actions.

use flint_core::{LineId, LineSelection};
use gpui_kit::*;

use super::super::registry::{
    ActionError, Bind, ClearSelection, CopyLines, DragSelection, ExtendSelectionDown,
    ExtendSelectionUp, FocusSearch, JumpToEnd, JumpToStart, LeaveSearch, NextMatch, OnView,
    PrevMatch, ReloadLogs, SaveLines, SaveLogs, SaveMatchingLines, SaveScope, SaveSelectedLines,
    SelectAllLines, SelectLine, SelectNextLine, SelectPrevLine, TogglePause, ToggleSources,
    ToggleTimestamps, ToggleWrap,
};
use super::{LogView, log_model};
use crate::ui::log_model::Tone;

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
                if let Some(last) = view.last_line() {
                    let first = view.snapshot.lines.first_id();
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
            .bind::<ReloadLogs>(|view, _, _, cx| view.reload(cx))
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

    /// The newest line shown.
    fn last_line(&self) -> Option<LineId> {
        let lines = &self.snapshot.lines;
        (!lines.is_empty()).then(|| lines.end_id() - 1)
    }

    /// Lines are shown, so there is something to select, search or save.
    pub(in crate::ui) fn has_lines(&self) -> bool {
        !self.snapshot.lines.is_empty()
    }

    pub(in crate::ui) fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    pub(in crate::ui) fn match_count(&self) -> usize {
        self.search.count()
    }

    /// [`SelectLine`] and [`DragSelection`] take a line shown now.
    pub(in crate::ui) fn check_line<A: Action>(&self, line: LineId) -> Result<(), ActionError> {
        if self.snapshot.lines.contains(line) {
            Ok(())
        } else {
            Err(ActionError::rejected::<A>(format!(
                "line {line} is not shown (it left the buffer, or has not arrived)"
            )))
        }
    }

    /// [`SaveLines`] needs lines in the chosen scope: a search with matches, or a selection.
    pub(in crate::ui) fn check_save(&self, scope: SaveScope) -> Result<(), ActionError> {
        let reason = match scope {
            _ if !self.has_lines() => Some("no lines are shown"),
            SaveScope::All => None,
            SaveScope::Matches if self.search.count() == 0 => Some("no lines match the search"),
            SaveScope::Matches => None,
            SaveScope::Selection if self.selection.is_none() => Some("no lines are selected"),
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

    pub(super) fn next_match(&mut self, _: &NextMatch, _: &mut Window, cx: &mut Context<Self>) {
        let next = self
            .search
            .next_after(self.current_match)
            .or_else(|| self.search.next_after(None));
        self.go_to_match(next, cx);
    }

    pub(super) fn prev_match(&mut self, _: &PrevMatch, _: &mut Window, cx: &mut Context<Self>) {
        let prev = self
            .search
            .prev_before(self.current_match)
            .or_else(|| self.search.prev_before(None));
        self.go_to_match(prev, cx);
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

    fn copy_lines(&mut self, _: &CopyLines, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            cx.propagate();
            return;
        };
        let count = selection.lines(&self.snapshot.lines).count() as u64;
        let text = self.selection_text().unwrap_or_default();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.set_note(
            Tone::Muted,
            format!(
                "Copied {} as shown, not redacted.",
                log_model::line_count(count)
            ),
            cx,
        );
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    // The handlers run through a window in `log_tests.rs`.
    #[test]
    fn every_log_action_is_bound() {
        assert_binds_scope(Scope::Logs, super::LogView::bind_actions);
    }
}
