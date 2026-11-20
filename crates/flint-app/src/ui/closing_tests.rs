//! View tests for closing the window and quitting (#177): Cmd-W closes the dock's tab before the
//! window, and closing the window or quitting asks first while a session would end.

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use serde_json::Value;

use super::closing::{Ending, Quits, dialog};
use super::dock_model::LiveSessions;
use super::support_tests::{TestWorkspace, WAIT, open_fake_shell_in, open_workspace};
use super::view_state::DialogState;

/// The workspace with pods `web` and `api` listed, as the front window.
async fn open(cx: &mut TestAppContext, dir: &std::path::Path) -> TestWorkspace {
    let t = open_workspace(cx, dir).await;
    t.open_pods(cx, &[("demo", "web", 0), ("demo", "api", 0)])
        .await;
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    t
}

fn window_is_open(cx: &mut TestAppContext, t: &TestWorkspace) -> bool {
    cx.update_window(t.window, |_, _, _| ()).is_ok()
}

fn tab_count(cx: &mut TestAppContext, t: &TestWorkspace) -> usize {
    t.workspace
        .read_with(cx, |ws, cx| ws.dock_view().read(cx).tab_count())
}

fn quits(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0))
}

/// Moves focus to the resource list, as clicking a row does.
fn focus_list(cx: &mut TestAppContext, t: &TestWorkspace) {
    let list = t.list(cx).expect("the list is open");
    let focus = list.read_with(cx, |l, _| l.focus_handle().clone());
    cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    cx.run_until_parked();
    let focused = cx
        .update_window(t.window, |_, window, _| focus.is_focused(window))
        .unwrap();
    assert!(focused, "the list has focus");
}

/// The dialog open over the window, as automation reads it.
async fn shown_dialog(cx: &mut TestAppContext, t: &TestWorkspace) -> DialogState {
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.view_state(window, cx).dialog)
    })
    .unwrap()
    .expect("a dialog is open")
}

async fn cancel(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
}

// covers UP-42 (#177): Cmd-W with focus in the list closes the dock's active tab, ending its
// session, and keeps the window and the other tab; nothing is asked.
#[gpui_kit::test]
async fn cmd_w_in_the_list_closes_a_dock_tab_not_the_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let (_, mut web) = open_fake_shell_in(cx, &t, "web");
    let (_, mut api) = open_fake_shell_in(cx, &t, "api");
    assert_eq!(tab_count(cx, &t), 2);
    focus_list(cx, &t);

    t.press(cx, "secondary-w");
    assert!(window_is_open(cx, &t), "the window stays");
    assert!(!t.has_dialog(cx), "closing a tab asks nothing");
    assert_eq!(tab_count(cx, &t), 1);
    assert!(api.is_closed(), "the active tab's shell ends");
    assert!(!web.is_closed(), "the other tab's shell runs on");
    assert_eq!(quits(cx), 0);
}

// covers UP-42 (#177): with no dock tab, Cmd-W closes the window, at once.
#[gpui_kit::test]
async fn cmd_w_with_no_dock_tabs_closes_the_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    focus_list(cx, &t);
    t.press(cx, "secondary-w");
    assert!(!window_is_open(cx, &t));
    assert_eq!(quits(cx), 0, "closing the window is not quitting");
}

// covers UP-42 (#177): Cmd-Q with a live shell asks first, naming the session. Cancel keeps the
// window, the tab and its shell; confirming quits. With nothing running it quits at once.
#[gpui_kit::test]
async fn cmd_q_with_a_live_shell_asks_first(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let (_, mut web) = open_fake_shell_in(cx, &t, "web");
    focus_list(cx, &t);

    t.press_key_of(cx, "flint::Quit");
    let asked = shown_dialog(cx, &t).await;
    assert_eq!(asked.title, "Quit Flint?");
    assert!(asked.text.contains("1 session"), "{}", asked.text);
    assert!(asked.text.contains("1 shell"), "{}", asked.text);
    assert_eq!(asked.buttons, ["Quit", "Cancel"]);
    assert_eq!(quits(cx), 0, "nothing quits before the answer");

    cancel(cx, &t).await;
    assert_eq!(quits(cx), 0);
    assert!(window_is_open(cx, &t));
    assert_eq!(tab_count(cx, &t), 1);
    assert!(!web.is_closed(), "the shell runs on");

    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    assert_eq!(quits(cx), 1, "confirming quits");

    // Once the shell has exited, nothing is asked.
    web.set_state(flint_core::ExecState::Exited { code: Some(0) });
    web.pump();
    cx.run_until_parked();
    t.press_key_of(cx, "flint::Quit");
    assert_eq!(quits(cx), 2);
    assert!(!t.has_dialog(cx));
}

// covers UP-42 (#177): Close Window (Shift-Cmd-W), the window's close button and Cmd-W with the
// dock hidden ask first while a shell runs; Cancel keeps everything, confirming closes.
#[gpui_kit::test]
async fn closing_the_window_with_a_live_shell_asks_first(cx: &mut TestAppContext) {
    for how in ["key", "button", "hidden dock"] {
        let dir = tempfile::tempdir().unwrap();
        let t = open(cx, dir.path()).await;
        let (_, mut web) = open_fake_shell_in(cx, &t, "web");
        if how == "hidden dock" {
            t.dispatch(cx, "dock::ToggleDock", Value::Null).unwrap();
        }
        focus_list(cx, &t);
        let close = |cx: &mut TestAppContext| match how {
            "key" => t.press_key_of(cx, "flint::CloseWindow"),
            "button" => {
                let closed = VisualTestContext::from_window(t.window, cx).simulate_close();
                assert!(!closed, "the close button waits for the answer");
                cx.run_until_parked();
            }
            _ => t.press(cx, "secondary-w"),
        };

        close(cx);
        let asked = shown_dialog(cx, &t).await;
        assert_eq!(asked.title, "Close the window?", "{how}");
        assert!(asked.text.contains("1 shell"), "{how}: {}", asked.text);
        assert_eq!(asked.buttons, ["Close Window", "Cancel"]);
        cancel(cx, &t).await;
        assert!(window_is_open(cx, &t), "{how}");
        assert_eq!(tab_count(cx, &t), 1, "{how}");
        assert!(!web.is_closed(), "{how}: the shell runs on");

        close(cx);
        shown_dialog(cx, &t).await;
        t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
        assert!(
            !window_is_open(cx, &t),
            "{how}: confirming closes the window"
        );
        assert_eq!(quits(cx), 0, "{how}");
    }
}

// covers UP-42 (#177): with no session at work, the close button closes at once.
#[gpui_kit::test]
async fn the_close_button_closes_an_idle_window_at_once(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    assert!(VisualTestContext::from_window(t.window, cx).simulate_close());
    assert!(!t.has_dialog(cx));
}

// The confirmation names how many sessions end, and of which kinds.
#[test]
fn the_dialog_names_the_sessions_that_end() {
    let live = |shells, logs| LiveSessions { shells, logs };
    let window = dialog(Ending::Window, live(1, 2));
    assert_eq!(window.title, "Close the window?");
    assert_eq!(
        window.text,
        "Closing the window ends 3 sessions still running in the dock: 1 shell and 2 log \
         streams. Their output is not kept."
    );
    assert_eq!(
        (window.ok.as_str(), window.cancel.as_str()),
        ("Close Window", "Cancel")
    );
    let app = dialog(Ending::App, live(0, 1));
    assert_eq!(app.title, "Quit Flint?");
    assert_eq!(
        app.text,
        "Quitting ends 1 session still running in the dock: 1 log stream. Its output is not \
         kept."
    );
    assert_eq!(app.ok, "Quit");
    assert_ne!(window.id, app.id);
}
