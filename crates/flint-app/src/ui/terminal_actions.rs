//! A shell tab's registered actions ([`crate::ui::registry`]), their handlers, and the checks
//! the workspace's guards share with them. The toolbar's buttons dispatch these actions; mouse
//! selection and keystrokes for the shell are not actions (they are the program's input).

use flint_core::exec::{BracketedPaste, ConfirmedPaste, ShellAttempt};
use gpui_kit::component::input;
use gpui_kit::*;

use super::super::registry::{
    ActionError, Bind, CopyText, OnView, PasteText, ScrollPageDown, ScrollPageUp, ScrollToBottom,
    ScrollToTop, SelectText,
};
use super::{NoteKind, TerminalView};
use crate::ui::confirm::{self, DialogText};
use crate::ui::log_model::Tone;
use crate::ui::terminal_model::{self, PasteCheck};

impl TerminalView {
    /// Every shell tab action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<CopyText>(Self::copy_text)
            .bind::<PasteText>(|view, _, window, cx| view.paste_clipboard(window, cx))
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

    /// Pastes `text` into the shell: one line at once; several lines at once only if the
    /// program takes pastes bracketed when they reach it (the core checks its live mode), else
    /// after asking, since each line would run as it arrives; never when it is larger than
    /// [`terminal_model::MAX_PASTE_BYTES`], nor while the shell is not reading what it was
    /// sent (the tab says so either way).
    fn paste(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        match terminal_model::paste_check(&text) {
            PasteCheck::Paste => {
                let sent = self.session.paste(text);
                self.input_sent(sent, true);
                cx.notify();
            }
            PasteCheck::TooLarge { bytes } => {
                self.set_note(Tone::Warning, terminal_model::paste_too_large(bytes));
                cx.notify();
            }
            PasteCheck::Lines { lines } => {
                let refusal_sequence = self.refusal_sequence;
                let focus_stint = self.focus_stint;
                let pasted = match self.session.paste_if_bracketed(text.clone()) {
                    Ok(pasted) => pasted,
                    Err(refused) => {
                        self.input_sent(Err(refused), true);
                        cx.notify();
                        return;
                    }
                };
                cx.spawn_in(window, async move |this, cx| match pasted.await {
                    BracketedPaste::Pasted => {
                        let _ = this.update(cx, |view, cx| {
                            view.paste_sent(refusal_sequence, cx);
                        });
                    }
                    BracketedPaste::Unbracketed(attempt) => {
                        let _ = this.update_in(cx, |view, window, cx| {
                            if view.focus_stint == focus_stint && view.holds_focus(window, cx) {
                                view.ask_to_paste(text, lines, attempt, window, cx);
                            } else if view.is_live() {
                                view.set_shell_input_note(
                                    NoteKind::PasteDiscarded,
                                    "Focus left the shell and your paste was not sent. Paste it \
                                     again."
                                        .into(),
                                );
                                cx.notify();
                            }
                        });
                    }
                    BracketedPaste::Discarded => {}
                })
                .detach();
            }
        }
    }

    /// Asks before pasting `lines` lines that would each run as they arrive.
    fn ask_to_paste(
        &mut self,
        text: String,
        lines: usize,
        attempt: ShellAttempt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_live() {
            return;
        }
        let (title, question) = terminal_model::paste_question(lines);
        let dialog = DialogText {
            title,
            text: question,
            ok: "Paste".into(),
            cancel: "Cancel".into(),
            id: "paste-description",
            default_cancel: false,
        };
        let view = cx.weak_entity();
        confirm::open(window, cx, dialog, move |_, cx| {
            let text = text.clone();
            let _ = view.update(cx, |view, cx| {
                view.paste_confirmed(text, attempt, cx);
            });
        });
    }

    fn paste_confirmed(&mut self, text: String, attempt: ShellAttempt, cx: &mut Context<Self>) {
        if !self.is_live() {
            return;
        }
        // The question holds focus, so what can be checked here is that the tab is still shown.
        if !self.dock_shown.get() {
            self.set_shell_input_note(
                NoteKind::PasteDiscarded,
                "The shell is no longer shown and your paste was not sent. Paste it again.".into(),
            );
            cx.notify();
            return;
        }
        let refusal_sequence = self.refusal_sequence;
        let pasted = match self.session.paste_confirmed(text, attempt) {
            Ok(pasted) => pasted,
            Err(refused) => {
                self.input_sent(Err(refused), true);
                cx.notify();
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let result = pasted.await;
            let _ = this.update(cx, |view, cx| match result {
                ConfirmedPaste::Pasted => view.paste_sent(refusal_sequence, cx),
                ConfirmedPaste::Discarded
                    if view.is_live() && !view.session.peek().state.is_finished() =>
                {
                    view.set_shell_input_note(
                        NoteKind::PasteDiscarded,
                        "The shell changed and your paste was not sent. Paste it again.".into(),
                    );
                    cx.notify();
                }
                ConfirmedPaste::Discarded => {}
            });
        })
        .detach();
    }

    fn paste_sent(&mut self, refusal_sequence: u64, cx: &mut Context<Self>) {
        if self.refusal_sequence == refusal_sequence {
            self.clear_input_refusal();
            cx.notify();
        }
    }

    /// Pastes the clipboard's text, if any, while the shell runs ([`TerminalView::paste`]).
    fn paste_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.holds_focus(window, cx) {
            self.set_note(
                Tone::Warning,
                "Click the shell to paste into it.".to_owned(),
            );
            cx.notify();
            return;
        }
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        match text {
            Some(text) if self.is_live() && !text.is_empty() => self.paste(text, window, cx),
            _ => {}
        }
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        let element = Self::bind_actions(OnView::new(element, cx)).into_element();
        // Edit > Copy and Edit > Paste (a text field's actions) do the tab's own copy and paste
        // while it has focus. Only here, not forwarded: their keys are bound in text fields only,
        // so the shell keeps its own.
        element
            .on_action(cx.listener(|view, _: &input::Copy, window, cx| {
                view.copy_text(&CopyText, window, cx)
            }))
            .on_action(
                cx.listener(|view, _: &input::Paste, window, cx| view.paste_clipboard(window, cx)),
            )
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
        // Never propagated: the key would reach the shell (on Linux, Ctrl-Shift-C as ^C). The
        // core is asked even when the flag says nothing is selected: a snapshot may lag behind
        // a selection just made.
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
