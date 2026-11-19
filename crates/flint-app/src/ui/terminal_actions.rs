//! A shell tab's registered actions ([`crate::ui::registry`]), their handlers, and the checks
//! the workspace's guards share with them. The toolbar's buttons dispatch these actions; mouse
//! selection and keystrokes for the shell are not actions (they are the program's input).

use gpui_kit::*;

use super::super::registry::{
    ActionError, Bind, CopyText, OnView, PasteText, ScrollPageDown, ScrollPageUp, ScrollToBottom,
    ScrollToTop, SelectText,
};
use super::TerminalView;
use crate::ui::log_model::Tone;

impl TerminalView {
    /// Every shell tab action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<CopyText>(Self::copy_text)
            .bind::<PasteText>(|view, _, _, cx| {
                let text = cx.read_from_clipboard().and_then(|item| item.text());
                match text {
                    Some(text) if view.is_live() && !text.is_empty() => {
                        view.session.paste(text);
                        cx.notify();
                    }
                    _ => {}
                }
            })
            .bind::<ScrollPageUp>(|view, _, _, _| {
                let page = i32::from(view.snapshot.terminal.size.rows);
                view.session.scroll(page);
            })
            .bind::<ScrollPageDown>(|view, _, _, _| {
                let page = i32::from(view.snapshot.terminal.size.rows);
                view.session.scroll(-page);
            })
            .bind::<ScrollToTop>(|view, _, _, _| {
                let history = view.snapshot.terminal.history_lines;
                view.session
                    .scroll(i32::try_from(history).unwrap_or(i32::MAX));
            })
            .bind::<ScrollToBottom>(|view, _, _, _| view.session.scroll_to_bottom())
            .bind::<SelectText>(|view, action, _, cx| match view.check_cells(action) {
                Ok(()) => view.select(
                    (action.start_row, action.start_col),
                    (action.end_row, action.end_col),
                    cx,
                ),
                Err(e) => tracing::warn!("{e}"),
            })
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }

    /// [`SelectText`] takes cells on the screen, the first not after the last.
    pub(in crate::ui) fn check_cells(&self, action: &SelectText) -> Result<(), ActionError> {
        let size = self.snapshot.terminal.size;
        let on_screen = |row: u16, col: u16| row < size.rows && col < size.cols;
        if !on_screen(action.start_row, action.start_col)
            || !on_screen(action.end_row, action.end_col)
        {
            return Err(ActionError::rejected::<SelectText>(format!(
                "the screen is {} columns by {} rows",
                size.cols, size.rows
            )));
        }
        if (action.start_row, action.start_col) > (action.end_row, action.end_col) {
            return Err(ActionError::rejected::<SelectText>(
                "the first cell comes after the last",
            ));
        }
        Ok(())
    }

    /// Puts the selection on the clipboard as the core reads it (history included), and says
    /// how much was copied.
    fn copy_text(&mut self, _: &CopyText, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection {
            cx.propagate();
            return;
        }
        let text = self.session.selection_text();
        cx.spawn(async move |this, cx| {
            let Some(text) = text.await else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                let count = text.chars().count();
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                view.set_note(
                    Tone::Muted,
                    format!(
                        "Copied {count} character{}.",
                        if count == 1 { "" } else { "s" }
                    ),
                );
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    // The handlers run through a window in `terminal_tests.rs`.
    #[test]
    fn every_terminal_action_is_bound() {
        assert_binds_scope(Scope::Terminal, super::TerminalView::bind_actions);
    }
}
