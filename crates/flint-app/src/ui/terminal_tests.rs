//! View tests for a shell tab, in a headless GPUI window (`#[gpui_kit::test]`), over a session
//! the test drives ([`FakeExec`]): it prints output, sets the state, and reads back what the tab
//! sent. Keys go through the registry's key bindings, as in the app.

use flint_core::exec::fake::FakeExec;
use flint_core::terminal::TerminalSize;
use flint_core::{ExecState, TerminalSnapshot};
use gpui_kit::test::{ClickOptions, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, ClipboardItem, Entity, InputHandler as _, Point,
    ScrollDelta, TestAppContext, WindowBounds, WindowOptions, point, px, size,
};
use serde_json::json;

use super::{RESIZE_DELAY, TerminalInput, TerminalView};
use crate::ui::dock::{DockItem as _, TabKey, TabStatus};
use crate::ui::registry::{self, SelectText, TERMINAL_COPY, TERMINAL_PASTE};
use crate::ui::terminal_model::{PADDING, ShellRequest};

fn request() -> ShellRequest {
    ShellRequest {
        cluster: "test".into(),
        namespace: "demo".into(),
        pod: "web-1".into(),
        container: Some("app".into()),
    }
}

struct Shell {
    window: AnyWindowHandle,
    view: Entity<TerminalView>,
    fake: FakeExec,
}

/// A shell tab alone in a window 800 by 400, focused and drawn once, its shell running.
fn open(cx: &mut TestAppContext) -> Shell {
    cx.update(|cx| {
        gpui_kit::init(cx);
        registry::init(cx);
    });
    let (session, mut fake) = FakeExec::open(request().exec_request(TerminalSize::default()));
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(800.), px(400.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| TerminalView::new(session, request(), window, cx)),
        )
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        window.render_frame(cx);
    })
    .unwrap();
    fake.set_state(ExecState::Running {
        command: vec!["bash".into()],
    });
    cx.run_until_parked();
    fake.pump();
    fake.clear_written();
    cx.run_until_parked();
    Shell { window, view, fake }
}

impl Shell {
    fn press(&self, cx: &mut TestAppContext, key: &str) {
        cx.update_window(self.window, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn input(&self, cx: &mut TestAppContext, text: &str) {
        cx.update_window(self.window, |_, window, cx| window.input(text, cx))
            .unwrap();
        cx.run_until_parked();
    }

    /// Prints `bytes` from the shell and lets the tab draw them.
    fn print(&mut self, cx: &mut TestAppContext, bytes: &[u8]) {
        self.fake.print(bytes);
        self.settle(cx);
    }

    /// Lets the tab take in the session's newest snapshot and draw it.
    fn settle(&mut self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update_window(self.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }

    /// What the tab sent since the last call.
    fn sent(&mut self, cx: &mut TestAppContext) -> Vec<u8> {
        cx.run_until_parked();
        self.fake.pump();
        let sent = self.fake.written().to_vec();
        self.fake.clear_written();
        self.settle(cx);
        sent
    }

    fn screen(&self, cx: &mut TestAppContext) -> TerminalSnapshot {
        self.view
            .read_with(cx, |v, _| v.snapshot().terminal.clone())
    }

    fn dispatch(
        &self,
        cx: &mut TestAppContext,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(), registry::ActionError> {
        let result = cx
            .update_window(self.window, |_, window, cx| {
                registry::dispatch(name, args, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        result
    }

    /// The center of screen cell (`row`, `col`), in window coordinates.
    fn cell_center(&self, cx: &mut TestAppContext, row: u16, col: u16) -> Point<gpui_kit::Pixels> {
        let layout = self.view.read_with(cx, |v, _| v.layout()).expect("drawn");
        layout.cell_bounds(row, col, 1).center()
    }
}

// covers UP-41: typed text, Return, Ctrl-C, Tab, Esc and the arrows reach the shell, not the
// app's bindings for those keys (focus traversal, lists, dialogs).
#[gpui_kit::test]
async fn keys_and_text_reach_the_shell(cx: &mut TestAppContext) {
    let mut shell = open(cx);
    shell.input(cx, "ls -la");
    shell.press(cx, "enter");
    assert_eq!(shell.sent(cx), b"ls -la\r");
    shell.input(cx, "é界");
    assert_eq!(shell.sent(cx), "é界".as_bytes());

    for key in [
        "tab",
        "escape",
        "ctrl-c",
        "ctrl-d",
        "ctrl-w",
        "up",
        "backspace",
    ] {
        shell.press(cx, key);
    }
    assert_eq!(shell.sent(cx), b"\t\x1b\x03\x04\x17\x1b[A\x7f");

    // A full-screen program turns application cursor keys on (as vi, less and top do).
    shell.print(cx, b"\x1b[?1h");
    shell.press(cx, "up");
    shell.press(cx, "f1");
    assert_eq!(shell.sent(cx), b"\x1bOA\x1bOP");
    // App shortcuts stay the app's: a Cmd (Ctrl-Shift on Linux) key sends nothing.
    shell.press(cx, "secondary-shift-l");
    assert_eq!(shell.sent(cx), b"");
}

// covers UP-41: text composed with an input method shows at the cursor until it is committed,
// then goes to the shell.
#[gpui_kit::test]
async fn composed_text_is_sent_once_committed(cx: &mut TestAppContext) {
    let mut shell = open(cx);
    let weak = shell.view.downgrade();
    cx.update_window(shell.window, |_, window, cx| {
        let mut input = TerminalInput {
            view: weak,
            bounds: Bounds::default(),
        };
        assert!(input.accepts_text_input(window, cx));
        input.replace_and_mark_text_in_range(None, "に", None, window, cx);
        assert_eq!(input.marked_text_range(window, cx), Some(0..1));
        assert!(input.bounds_for_range(0..1, window, cx).is_some());
        input.replace_text_in_range(None, "日本", window, cx);
        assert_eq!(input.marked_text_range(window, cx), None);
    })
    .unwrap();
    assert_eq!(shell.sent(cx), "日本".as_bytes());
}

// covers UP-41: the grid fills the tab and follows its size, telling the pod once the size
// settles.
#[gpui_kit::test]
async fn the_grid_resizes_with_the_window(cx: &mut TestAppContext) {
    let mut shell = open(cx);
    let first = shell.fake.resizes().to_vec();
    assert_eq!(first.len(), 1, "the first layout resizes at once");
    let layout = shell.view.read_with(cx, |v, _| v.layout()).unwrap();
    assert_eq!(first[0], layout.size);
    assert!(
        layout.size.cols > 80 && layout.size.rows > 10,
        "{:?}",
        layout.size
    );
    assert_eq!(shell.screen(cx).size, layout.size);

    cx.simulate_window_resize(shell.window, size(px(500.), px(300.)));
    shell.settle(cx);
    shell.fake.pump();
    assert_eq!(
        shell.fake.resizes().len(),
        1,
        "waits for the size to settle"
    );
    cx.executor().advance_clock(RESIZE_DELAY);
    cx.run_until_parked();
    shell.fake.pump();
    let smaller = shell.view.read_with(cx, |v, _| v.layout()).unwrap().size;
    assert_eq!(shell.fake.resizes(), &[first[0], smaller]);
    assert!(smaller.cols < first[0].cols && smaller.rows < first[0].rows);

    // Redrawing at the same size sends nothing more.
    shell.settle(cx);
    cx.executor().advance_clock(RESIZE_DELAY * 2);
    cx.run_until_parked();
    shell.fake.pump();
    assert_eq!(shell.fake.resizes().len(), 2);
}

// covers UP-41: dragging selects, Copy puts the text on the clipboard, Paste sends the
// clipboard (bracketed when the program asked).
#[gpui_kit::test]
async fn copy_and_paste(cx: &mut TestAppContext) {
    let mut shell = open(cx);
    shell.print(cx, b"hello world\r\nsecond line");
    assert!(
        !shell.view.read_with(cx, |v, _| v.has_selection()),
        "nothing selected"
    );

    // A drag over "hello".
    let from = shell.cell_center(cx, 0, 0);
    let to = shell.cell_center(cx, 0, 4);
    let from = point(from.x - px(2.), from.y);
    let to = point(to.x + px(2.), to.y);
    cx.update_window(shell.window, |_, window, cx| window.drag(from, to, cx))
        .unwrap();
    shell.fake.pump();
    shell.settle(cx);
    assert!(shell.view.read_with(cx, |v, _| v.has_selection()));
    let selected: String = shell.screen(cx).row(0)[..6]
        .iter()
        .map(|c| if c.selected { 'x' } else { '.' })
        .collect();
    assert_eq!(selected, "xxxxx.");
    shell.press(cx, TERMINAL_COPY[0]);
    shell.fake.pump();
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
        Some("hello")
    );
    assert!(shell.fake.written().is_empty(), "copying sends nothing");

    // The same through the registry, as automation selects.
    shell
        .dispatch(
            cx,
            "terminal::SelectText",
            json!({"start_row": 0, "start_col": 6, "end_row": 1, "end_col": 5}),
        )
        .unwrap();
    shell.press(cx, TERMINAL_COPY[0]);
    shell.fake.pump();
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
        Some("world\nsecond")
    );

    cx.write_to_clipboard(ClipboardItem::new_string("echo a\necho b".into()));
    shell.press(cx, TERMINAL_PASTE[0]);
    assert_eq!(shell.sent(cx), b"echo a\recho b");
    shell.print(cx, b"\x1b[?2004h");
    shell.press(cx, TERMINAL_PASTE[0]);
    assert_eq!(shell.sent(cx), b"\x1b[200~echo a\necho b\x1b[201~");
}

// covers UP-41: the wheel scrolls back through the output; typing returns to the bottom. In a
// full-screen program the wheel sends arrow keys instead.
#[gpui_kit::test]
async fn the_wheel_scrolls_the_history(cx: &mut TestAppContext) {
    let mut shell = open(cx);
    let mut output = Vec::new();
    for n in 0..200 {
        output.extend(format!("line {n}\r\n").bytes());
    }
    shell.print(cx, &output);
    let wheel = |cx: &mut TestAppContext, shell: &Shell, lines: f32| {
        cx.update_window(shell.window, |_, window, cx| {
            window.scroll(
                "terminal-grid",
                ScrollDelta::Lines(point(px(0.).into(), lines)),
                cx,
            )
        })
        .unwrap();
        cx.run_until_parked();
    };
    wheel(cx, &shell, 5.);
    shell.fake.pump();
    shell.settle(cx);
    assert_eq!(shell.fake.scrolls(), &[5]);
    assert_eq!(shell.screen(cx).display_offset, 5);
    assert!(shell.view.read_with(cx, |v, _| v.is_scrolled_back()));
    shell
        .dispatch(cx, "terminal::ScrollToBottom", json!(null))
        .unwrap();
    shell.fake.pump();
    shell.settle(cx);
    assert_eq!(shell.screen(cx).display_offset, 0);

    // The alternate screen (less, vi): arrows.
    shell.print(cx, b"\x1b[?1049h\x1b[?1h");
    wheel(cx, &shell, -2.);
    assert_eq!(shell.sent(cx), b"\x1bOB\x1bOB");
}

// covers UP-41: when the command exits, its code is shown, keystrokes stop, and the screen stays
// readable and copyable until the tab closes.
#[gpui_kit::test]
async fn an_exited_shell_shows_its_code_and_keeps_its_screen(cx: &mut TestAppContext) {
    let mut shell = open(cx);
    assert_eq!(
        shell.view.read_with(cx, |v, _| v.tab_status()),
        TabStatus::Live
    );
    // While it runs, its tab answers for its container (Shell shows it instead of a new one).
    let running = shell.view.read_with(cx, |v, _| v.tab_key());
    assert!(
        matches!(running, TabKey::Shell(ref key) if key.ended.is_none() && key.request == request())
    );
    shell.print(cx, b"$ exit 3\r\n");
    shell.fake.set_state(ExecState::Exited { code: Some(3) });
    shell.settle(cx);
    assert_eq!(
        shell.view.read_with(cx, |v, _| v.tab_status()),
        TabStatus::Ended
    );
    cx.update_window(shell.window, |_, window, _| {
        assert!(window.try_find("terminal-ended").is_some());
        assert!(window.try_find("terminal-paste").is_some());
    })
    .unwrap();
    assert_eq!(shell.screen(cx).row_text(0), "$ exit 3");
    shell.input(cx, "ls");
    shell.press(cx, "enter");
    assert_eq!(shell.sent(cx), b"", "the command is gone");

    shell
        .dispatch(
            cx,
            "terminal::SelectText",
            json!({"start_row": 0, "start_col": 2, "end_row": 0, "end_col": 7}),
        )
        .unwrap();
    shell.press(cx, TERMINAL_COPY[0]);
    shell.fake.pump();
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
        Some("exit 3")
    );

    // An ended shell's tab no longer answers for its container: Shell opens a new one.
    let key = shell.view.read_with(cx, |v, _| v.tab_key());
    let TabKey::Shell(key) = key else {
        panic!("a shell key")
    };
    assert!(key.ended.is_some());

    shell
        .fake
        .set_state(ExecState::Failed("pods \"web-1\" not found".into()));
    shell.settle(cx);
    assert_eq!(
        shell.view.read_with(cx, |v, _| v.tab_status()),
        TabStatus::Failed
    );
}

// covers UP-41: the bell flashes the screen for a moment.
#[gpui_kit::test]
async fn the_bell_flashes(cx: &mut TestAppContext) {
    let mut shell = open(cx);
    shell.print(cx, b"\x07");
    assert!(shell.view.read_with(cx, |v, _| v.bell));
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert!(!shell.view.read_with(cx, |v, _| v.bell));
}

// covers UP-41: a click alone selects nothing; a double click selects a word, a triple click
// the line.
#[gpui_kit::test]
async fn clicks_select_words_and_lines(cx: &mut TestAppContext) {
    let mut shell = open(cx);
    shell.print(cx, b"one two three");
    let layout = shell.view.read_with(cx, |v, _| v.layout()).unwrap();
    // Offsets inside the grid element: the cells start after its padding.
    let at = |col: u16| {
        point(
            px(PADDING + (f32::from(col) + 0.5) * layout.cell.width),
            px(PADDING + layout.cell.height / 2.),
        )
    };
    let click = |cx: &mut TestAppContext, shell: &mut Shell, col: u16, count: usize| {
        cx.update_window(shell.window, |_, window, cx| {
            window.click_with_options(
                "terminal-grid",
                ClickOptions::new().with_offset(at(col)).with_count(count),
                cx,
            )
        })
        .unwrap();
        shell.fake.pump();
        shell.settle(cx);
    };
    let copied = |cx: &mut TestAppContext, shell: &mut Shell| {
        shell.press(cx, TERMINAL_COPY[0]);
        shell.fake.pump();
        cx.run_until_parked();
        cx.read_from_clipboard().and_then(|i| i.text())
    };
    click(cx, &mut shell, 1, 1);
    assert!(!shell.view.read_with(cx, |v, _| v.has_selection()));
    click(cx, &mut shell, 5, 2);
    assert_eq!(copied(cx, &mut shell).as_deref(), Some("two"));
    click(cx, &mut shell, 5, 3);
    assert_eq!(copied(cx, &mut shell).as_deref(), Some("one two three\n"));
    // A single click clears the selection again.
    click(cx, &mut shell, 1, 1);
    assert!(!shell.view.read_with(cx, |v, _| v.has_selection()));

    // SelectText takes cells on the screen, in order.
    let check = |cx: &mut TestAppContext, start: (u16, u16), end: (u16, u16)| {
        shell.view.read_with(cx, |v, _| {
            v.check_cells(&SelectText {
                start_row: start.0,
                start_col: start.1,
                end_row: end.0,
                end_col: end.1,
            })
        })
    };
    assert!(check(cx, (0, 0), (0, 3)).is_ok());
    assert!(check(cx, (0, 0), (500, 0)).is_err());
    assert!(check(cx, (1, 0), (0, 3)).is_err());
}
