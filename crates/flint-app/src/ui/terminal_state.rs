//! A shell tab's part of the visible view tree ([`crate::ui::view_state`]): its state, size,
//! cursor and the visible screen as text, so a test can wait for a prompt or a command's output.
//! Only the screen goes out, never the scrollback.

use flint_core::{ExecState, TerminalSnapshot};
use gpui_kit::*;

use super::super::terminal_model::{self, failure_words};
use super::super::view_state::{CursorState, Focus, ShellTabState, TabStatusState};
use super::TerminalView;

impl TerminalView {
    /// The tab as data.
    pub fn view_state(&self) -> ShellTabState {
        let terminal = &self.snapshot.terminal;
        ShellTabState {
            cols: terminal.size.cols,
            rows: terminal.size.rows,
            live: self.is_live(),
            exit_code: match self.snapshot.state {
                ExecState::Exited { code } => code,
                _ => None,
            },
            cursor: terminal.cursor.map(|c| CursorState {
                row: c.row,
                col: c.col,
            }),
            scrolled_back: self.is_scrolled_back(),
            screen: screen_lines(terminal, ShellTabState::SCREEN_LIMIT),
        }
    }

    /// The shell's state, and the status as the toolbar says it.
    pub fn status_state(&self) -> (TabStatusState, String) {
        let text = terminal_model::toolbar_state(&self.snapshot.state, self.ended.is_some()).1;
        let status = match &self.ended {
            Some(why) => TabStatusState::Ended {
                reason: Some(why.clone()),
                pod_deleted: false,
            },
            None => status(&self.snapshot.state),
        };
        (status, text)
    }

    /// Whether focus is on the terminal.
    pub fn focus_in(&self, window: &Window, cx: &App) -> Option<Focus> {
        self.focus
            .contains_focused(window, cx)
            .then_some(Focus::Shell)
    }
}

/// A shell's state, a failure in the words its banner uses.
pub fn status(state: &ExecState) -> TabStatusState {
    match state {
        ExecState::Connecting => TabStatusState::Connecting,
        ExecState::Running { command } => TabStatusState::Running {
            command: command.join(" "),
        },
        ExecState::Exited { code } => TabStatusState::Exited { code: *code },
        ExecState::Failed(failure) => TabStatusState::Failed {
            reason: failure_words(failure).0.to_owned(),
        },
        ExecState::Closed => TabStatusState::Closed,
    }
}

/// The screen's rows as text, without the blank rows at its bottom, at most the last `limit`.
pub fn screen_lines(terminal: &TerminalSnapshot, limit: usize) -> Vec<String> {
    let mut rows: Vec<String> = (0..terminal.size.rows)
        .map(|r| terminal.row_text(r))
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    let skip = rows.len().saturating_sub(limit);
    rows.split_off(skip)
}

#[cfg(test)]
mod tests {
    use flint_core::terminal::{TerminalModel, TerminalSize};
    use flint_core::{ExecFailure, ExecFailureKind, ExecState};
    use serde_json::json;

    use super::{screen_lines, status};

    #[test]
    fn statuses_say_their_state_in_plain_words() {
        let json = |state: &ExecState| serde_json::to_value(status(state)).unwrap();
        assert_eq!(json(&ExecState::Connecting), json!({"state": "connecting"}));
        assert_eq!(
            json(&ExecState::Running {
                command: vec!["/bin/sh".into(), "-i".into()],
            }),
            json!({"state": "running", "command": "/bin/sh -i"})
        );
        assert_eq!(
            json(&ExecState::Exited { code: Some(3) }),
            json!({"state": "exited", "code": 3})
        );
        assert_eq!(
            json(&ExecState::Failed(ExecFailure {
                kind: ExecFailureKind::Server,
                message: "pods \"web\" is forbidden: User cannot create pods/exec".into(),
            })),
            json!({"state": "failed",
                   "reason": "You don't have permission to open a shell in this pod."})
        );
        assert_eq!(json(&ExecState::Closed), json!({"state": "closed"}));
    }

    #[test]
    fn the_screen_goes_out_without_its_blank_bottom() {
        let mut term = TerminalModel::new(TerminalSize::new(20, 5));
        term.advance(b"$ echo hi\r\nhi\r\n$ ");
        let screen = term.snapshot();
        assert_eq!(screen_lines(&screen, 50), ["$ echo hi", "hi", "$"]);
        assert_eq!(screen_lines(&screen, 2), ["hi", "$"]);
        let blank = TerminalModel::new(TerminalSize::new(20, 5)).snapshot();
        assert!(screen_lines(&blank, 50).is_empty());
    }
}
