//! An exec session driven by a test instead of a cluster (feature `test-support`), so a front
//! end's terminal view can be tested without a pod: the test prints output, sets the state, and
//! reads back what the view sent.
//!
//! The screen is a real [`TerminalModel`], and resizes, scrolling and selections apply to it as
//! in a live session, so snapshots and [`ExecSession::selection_text`] behave the same. Nothing
//! runs on its own: the test calls [`FakeExec::pump`] to take in what the view sent, and each
//! change publishes a snapshot at once (there is no frame pacing). Until it does, keystrokes
//! and pastes wait, held against the session's input budget as in a live session whose
//! command is not reading them, so a test can fill it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{
    BracketedPaste, ExecRequest, ExecSession, ExecSnapshot, ExecState, Input, SessionChannels,
    ShellAttempt,
};
use crate::terminal::{TerminalModel, TerminalSize};

/// The test's side of a session from [`FakeExec::open`].
pub struct FakeExec {
    channels: SessionChannels,
    state: ExecState,
    written: Vec<u8>,
    resizes: Vec<TerminalSize>,
    scrolls: Vec<i32>,
    input_not_replayed: bool,
}

impl FakeExec {
    /// A session for `request` that starts out connecting, and the test's side of it.
    pub fn open(request: ExecRequest) -> (ExecSession, FakeExec) {
        let (session, channels) = ExecSession::new(request);
        let fake = FakeExec {
            channels,
            state: ExecState::Connecting,
            written: Vec::new(),
            resizes: Vec::new(),
            scrolls: Vec::new(),
            input_not_replayed: false,
        };
        (session, fake)
    }

    fn model(&mut self) -> &mut TerminalModel {
        &mut self.channels.model
    }

    fn publish(&mut self) {
        let snapshot = Arc::new(ExecSnapshot {
            state: self.state.clone(),
            terminal: self.channels.model.snapshot(),
            input_not_replayed: self.input_not_replayed,
        });
        self.channels.out.send_replace(snapshot);
    }

    /// Moves the session to `state`.
    pub fn set_state(&mut self, state: ExecState) {
        self.state = state;
        self.publish();
    }

    /// Starts the fallback shell attempt.
    pub fn fallback(&mut self) {
        self.channels
            .attempt
            .store(ShellAttempt::next(), Ordering::Release);
        self.set_state(ExecState::Connecting);
    }

    /// Says, as a live session does, that what was typed while the first shell was tried was
    /// not kept for the shell that started ([`ExecSnapshot::input_not_replayed`]).
    pub fn input_not_replayed(&mut self) {
        self.input_not_replayed = true;
        self.publish();
    }

    /// Prints `bytes` as the remote program would.
    pub fn print(&mut self, bytes: &[u8]) {
        // Replies (cursor position reports) would go to the program; a test has none.
        let _ = self.model().advance(bytes);
        self.publish();
    }

    /// Takes in everything the session's handle sent since the last call, as a live session
    /// would: keystrokes and pastes are recorded (pastes encoded as they would be sent), giving
    /// their bytes back to the budget, and resizes, scrolling and selections change the
    /// screen. Closing discards the queue first. Otherwise resizes come first, the newest
    /// only, as a live session takes them. Publishes a snapshot.
    pub fn pump(&mut self) {
        if *self.channels.close.borrow() || self.channels.inputs.is_closed() {
            self.channels.inputs.close();
            while self.channels.inputs.try_recv().is_ok() {}
            if !self.state.is_finished() {
                self.set_state(ExecState::Closed);
            }
            return;
        }
        let finished = self.state.is_finished();
        if self.channels.size.has_changed().unwrap_or(false) {
            let size = *self.channels.size.borrow_and_update();
            self.resizes.push(size);
            self.model().resize(size);
        }
        let attempt = self.channels.attempt.load(Ordering::Acquire);
        while let Ok(input) = self.channels.inputs.try_recv() {
            match input {
                Input::Write(bytes, _) if !finished => {
                    self.written.extend(bytes);
                    self.model().scroll_to_bottom();
                }
                Input::Paste(text, sent, reply)
                    if !finished && (reply.is_none() || sent.attempt == attempt) =>
                {
                    let bytes = self.channels.model.paste_bytes(&text);
                    self.written.extend(bytes);
                    self.model().scroll_to_bottom();
                    if let Some(reply) = reply {
                        let _ = reply.send(());
                    }
                }
                Input::PasteIfBracketed(text, _, reply) => {
                    let result = if finished {
                        BracketedPaste::Discarded
                    } else if self.channels.model.bracketed_paste() {
                        let bytes = self.channels.model.paste_bytes(&text);
                        self.written.extend(bytes);
                        self.model().scroll_to_bottom();
                        BracketedPaste::Pasted
                    } else {
                        BracketedPaste::Unbracketed(ShellAttempt(attempt))
                    };
                    let _ = reply.send(result);
                }
                Input::Write(..) | Input::Paste(..) => {}
                Input::Resize(size) => {
                    self.resizes.push(size);
                    self.model().resize(size);
                }
                Input::Scroll(lines) => {
                    self.scrolls.push(lines);
                    self.model().scroll(lines);
                }
                Input::ScrollToBottom => self.model().scroll_to_bottom(),
                Input::StartSelection {
                    kind,
                    row,
                    col,
                    side,
                } => self.model().start_selection(kind, row, col, side),
                Input::UpdateSelection { row, col, side } => {
                    self.model().update_selection(row, col, side)
                }
                Input::ClearSelection => self.model().clear_selection(),
                Input::SelectionText(reply) => {
                    let _ = reply.send(self.channels.model.selection_text());
                }
                // The session sends neither through the queue.
                Input::Close => {}
            }
        }
        self.publish();
    }

    /// The keystrokes and pastes sent so far, as bytes.
    pub fn written(&self) -> &[u8] {
        &self.written
    }

    /// Forgets the bytes sent so far.
    pub fn clear_written(&mut self) {
        self.written.clear();
    }

    /// The resizes asked for so far, in order.
    pub fn resizes(&self) -> &[TerminalSize] {
        &self.resizes
    }

    /// The scrolls asked for so far, in lines (positive: up into the history).
    pub fn scrolls(&self) -> &[i32] {
        &self.scrolls
    }

    /// Whether the handle closed the session, or was dropped.
    pub fn is_closed(&mut self) -> bool {
        self.pump();
        self.channels.inputs.is_closed()
    }

    /// The snapshot the session shows now.
    pub fn snapshot(&self) -> Arc<ExecSnapshot> {
        self.channels.out.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{SelectionKind, Side};

    fn open() -> (ExecSession, FakeExec) {
        FakeExec::open(ExecRequest::shell("kind", "ns", "pod").size(TerminalSize::new(20, 4)))
    }

    #[test]
    fn records_input_and_applies_screen_changes() {
        let (session, mut fake) = open();
        fake.set_state(ExecState::Running {
            command: vec!["bash".into()],
        });
        fake.print(b"hello world");
        assert_eq!(session.peek().terminal.row_text(0), "hello world");

        session.write(b"ls\r".to_vec()).unwrap();
        session.paste("a\nb").unwrap();
        session.resize(TerminalSize::new(30, 6));
        session.scroll(3);
        fake.pump();
        assert_eq!(fake.written(), b"ls\ra\rb");
        assert_eq!(fake.resizes(), &[TerminalSize::new(30, 6)]);
        assert_eq!(fake.scrolls(), &[3]);
        assert_eq!(session.peek().terminal.size, TerminalSize::new(30, 6));

        session.start_selection(SelectionKind::Simple, 0, 0, Side::Left);
        session.update_selection(0, 4, Side::Right);
        let text = session.selection_text();
        fake.pump();
        let text = futures::FutureExt::now_or_never(text).expect("answered by the pump");
        assert_eq!(text.as_deref(), Some("hello"));
    }

    #[test]
    fn unpumped_input_holds_the_budget_until_taken_in() {
        let (session, mut fake) = open();
        fake.set_state(ExecState::Running {
            command: vec!["bash".into()],
        });
        let paste = "x".repeat(super::super::INPUT_BUDGET / 2);
        session.paste(paste.clone()).unwrap();
        let refused = session.paste(paste.clone()).unwrap_err();
        assert_eq!(refused.queued, session.queued_input());
        fake.pump();
        assert_eq!(session.queued_input(), 0);
        session.paste(paste).unwrap();
    }

    #[test]
    fn closing_discards_pending_input_before_it_can_be_recorded() {
        let (session, mut fake) = open();
        fake.set_state(ExecState::Running {
            command: vec!["bash".into()],
        });
        fake.print(b"\x1b[?2004h");
        session.write(b"typed".to_vec()).unwrap();
        session.paste("pasted").unwrap();
        let bracketed = session.paste_if_bracketed("a\nb").unwrap();

        session.close();
        fake.pump();

        assert_eq!(fake.snapshot().state, ExecState::Closed);
        assert!(fake.written().is_empty());
        assert_eq!(session.queued_input(), 0);
        assert_eq!(
            futures::FutureExt::now_or_never(bracketed),
            Some(BracketedPaste::Discarded)
        );
    }

    #[test]
    fn a_finished_session_drops_keystrokes_and_dropping_the_handle_closes_it() {
        let (session, mut fake) = open();
        fake.set_state(ExecState::Exited { code: Some(0) });
        session.write(b"x".to_vec()).unwrap();
        fake.pump();
        assert!(fake.written().is_empty());
        assert_eq!(fake.snapshot().state, ExecState::Exited { code: Some(0) });
        assert!(!fake.is_closed());
        drop(session);
        assert!(fake.is_closed());
    }
}
