//! View tests for closing the window and quitting (#177): Cmd-W closes the dock's tab before the
//! window, and closing the window or quitting asks first while a session would end.

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use serde_json::Value;

use super::closing::{Ending, Losses, Quits, dialog};
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

// covers UP-42 (#177): in the quit and close confirmations Enter cancels, as Escape does, also
// once Tab or Shift-Tab moved focus onto a button: ending the sessions takes a click on the
// confirm button. The app runs on, the window stays, the shell runs on.
#[gpui_kit::test]
async fn enter_and_escape_cancel_the_quit_and_close_confirmations(cx: &mut TestAppContext) {
    // Quitting last: the count of quits is the app's.
    for (ask, title) in [
        ("flint::CloseWindow", "Close the window?"),
        ("flint::Quit", "Quit Flint?"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let t = open(cx, dir.path()).await;
        let (_, mut web) = open_fake_shell_in(cx, &t, "web");
        focus_list(cx, &t);
        for keys in [
            &["enter"][..],
            &["escape"],
            &["tab", "enter"],
            &["tab", "tab", "enter"],
            &["shift-tab", "enter"],
            &["tab", "escape"],
        ] {
            t.press_key_of(cx, ask);
            assert_eq!(shown_dialog(cx, &t).await.title, title, "{ask}");
            assert_eq!(focused_button(cx, &t), None, "the description has focus");
            let mut focus = None;
            for key in keys {
                t.press(cx, key);
                if key.ends_with("tab") {
                    focus = next_button(focus, key);
                    assert_eq!(
                        focused_button(cx, &t),
                        focus,
                        "{ask}, {keys:?}: after {key}"
                    );
                }
            }
            cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
                .await;
            assert_eq!(quits(cx), 0, "{ask}, {keys:?}: nothing quits");
            assert!(window_is_open(cx, &t), "{ask}, {keys:?}: the window stays");
            assert_eq!(tab_count(cx, &t), 1, "{ask}, {keys:?}");
            assert!(!web.is_closed(), "{ask}, {keys:?}: the shell runs on");
        }
        // Its buttons answer it: Cancel keeps everything, the confirm button ends the sessions.
        t.press_key_of(cx, ask);
        shown_dialog(cx, &t).await;
        t.click(cx, "cancel");
        assert!(!t.has_dialog(cx), "{ask}: Cancel closes the dialog");
        assert!(window_is_open(cx, &t) && !web.is_closed() && quits(cx) == 0);
        t.press_key_of(cx, ask);
        shown_dialog(cx, &t).await;
        t.click(cx, "ok");
        match ask {
            "flint::Quit" => assert_eq!(quits(cx), 1, "the Quit button quits"),
            _ => assert!(!window_is_open(cx, &t), "Close Window closes it"),
        }
    }
}

/// The dialog button that has keyboard focus, `ok` or `cancel`, if one has.
fn focused_button(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<&'static str> {
    use gpui_kit::test::TestWindowExt as _;

    cx.update_window(t.window, |_, window, cx| {
        window.render_frame(cx);
        ["ok", "cancel"]
            .into_iter()
            .find(|id| window.find(*id).focused() == Some(true))
    })
    .unwrap()
}

/// Where Tab or Shift-Tab moves focus among the buttons, Cancel then OK, from `from`.
fn next_button(from: Option<&'static str>, key: &str) -> Option<&'static str> {
    match (key, from) {
        ("tab", Some("cancel")) | ("shift-tab", None | Some("cancel")) => Some("ok"),
        ("tab" | "shift-tab", _) => Some("cancel"),
        _ => from,
    }
}

/// Waits for the open dialog to close.
async fn closed(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
}

// covers UP-42 (#177): Space on the focused confirm button confirms, as macOS keyboard access
// does for any focused button; on the focused Cancel button it cancels.
#[gpui_kit::test]
async fn space_on_a_focused_button_answers_the_confirmation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let (_, mut web) = open_fake_shell_in(cx, &t, "web");
    focus_list(cx, &t);

    // Tab reaches Cancel first.
    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.press(cx, "tab");
    assert_eq!(focused_button(cx, &t), Some("cancel"));
    t.press(cx, "space");
    closed(cx, &t).await;
    assert_eq!(quits(cx), 0, "Space on Cancel cancels");
    assert!(!web.is_closed());

    // Shift-Tab reaches Quit.
    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.press(cx, "shift-tab");
    assert_eq!(focused_button(cx, &t), Some("ok"));
    t.press(cx, "space");
    assert_eq!(quits(cx), 1, "Space on the focused Quit button quits");
}

// covers UP-42 (#177): a click on the title-bar strip above the dialog leaves focus on the dialog
// itself; the very next Escape or Enter cancels there, and nothing confirms.
#[gpui_kit::test]
async fn escape_cancels_with_focus_on_the_dialog_itself(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, point, px};

    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let (_, mut web) = open_fake_shell_in(cx, &t, "web");
    focus_list(cx, &t);
    let click_strip = |cx: &mut TestAppContext| {
        VisualTestContext::from_window(t.window, cx)
            .simulate_click(point(px(300.), px(5.)), Modifiers::none());
    };

    for key in ["escape", "enter"] {
        t.press_key_of(cx, "flint::CloseWindow");
        shown_dialog(cx, &t).await;
        // The key comes right after the click, with nothing drawn in between.
        click_strip(cx);
        t.press(cx, key);
        closed(cx, &t).await;
        assert!(window_is_open(cx, &t), "{key}");
        assert!(!web.is_closed(), "{key}: the shell runs on");
    }
    assert_eq!(quits(cx), 0);
}

// covers UP-42 (#177): the confirmation the window's close button opens is answered the same
// way: Enter and Escape cancel it.
#[gpui_kit::test]
async fn enter_and_escape_cancel_the_close_buttons_confirmation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let (_, mut web) = open_fake_shell_in(cx, &t, "web");
    focus_list(cx, &t);
    for key in ["enter", "escape"] {
        assert!(!VisualTestContext::from_window(t.window, cx).simulate_close());
        cx.run_until_parked();
        assert_eq!(shown_dialog(cx, &t).await.title, "Close the window?");
        t.press(cx, key);
        closed(cx, &t).await;
        assert!(window_is_open(cx, &t), "{key}: the window stays");
        assert_eq!(tab_count(cx, &t), 1, "{key}");
        assert!(!web.is_closed(), "{key}: the shell runs on");
    }
    assert_eq!(quits(cx), 0);
}

// covers UP-42 (#177): Window > Close Dock Tab never closes the window. With no tab shown it is
// unavailable (the menu disables it), and dispatching it as the menu does changes nothing;
// Cmd-W still falls through to Close Window.
#[gpui_kit::test]
async fn the_close_dock_tab_menu_item_never_closes_the_window(cx: &mut TestAppContext) {
    use super::registry::CloseActiveTab;

    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let available =
        |cx: &mut TestAppContext| cx.update(|cx| cx.is_action_available(&CloseActiveTab));
    let from_menu = |cx: &mut TestAppContext| {
        cx.update(|cx| cx.dispatch_action(&CloseActiveTab));
        cx.run_until_parked();
    };

    // No tab at all.
    focus_list(cx, &t);
    assert!(!available(cx), "disabled with no tab");
    from_menu(cx);
    assert!(window_is_open(cx, &t));

    // A tab, shown: the item closes it.
    let (_, mut web) = open_fake_shell_in(cx, &t, "web");
    let (_, mut api) = open_fake_shell_in(cx, &t, "api");
    focus_list(cx, &t);
    assert!(available(cx), "enabled while a tab is shown");
    from_menu(cx);
    assert_eq!(tab_count(cx, &t), 1);
    assert!(api.is_closed());

    // The dock hidden, its tab still running: disabled, and the item does nothing.
    t.dispatch(cx, "dock::ToggleDock", Value::Null).unwrap();
    focus_list(cx, &t);
    assert!(!available(cx), "disabled with the dock hidden");
    from_menu(cx);
    assert!(window_is_open(cx, &t));
    assert!(!t.has_dialog(cx), "nothing is asked");
    assert_eq!(tab_count(cx, &t), 1);
    assert!(!web.is_closed());
}

// covers UP-42 (#177): asking again while the question is open opens no second dialog: one
// Cancel answers Cmd-Q pressed twice, and the close button pressed after it.
#[gpui_kit::test]
async fn asking_twice_opens_one_dialog(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let (_, _web) = open_fake_shell_in(cx, &t, "web");
    focus_list(cx, &t);

    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.press_key_of(cx, "flint::Quit");
    assert!(!VisualTestContext::from_window(t.window, cx).simulate_close());
    cx.run_until_parked();
    assert_eq!(shown_dialog(cx, &t).await.title, "Quit Flint?");
    cancel(cx, &t).await;
    assert!(!t.has_dialog(cx), "one Cancel closes the only dialog");
    assert_eq!(quits(cx), 0);
    assert!(window_is_open(cx, &t));
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
    let live = |shells, logs| Losses {
        live: LiveSessions { shells, logs },
        ..Losses::default()
    };
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
    assert!(
        window.default_cancel && app.default_cancel,
        "Enter cancels both"
    );
}

// The confirmation counts port forwards, in the singular and the plural, and joins sessions and
// an unapplied edit in one text.
#[test]
fn the_dialog_names_the_forwards_that_stop() {
    let losses = |forwards, shells, edits| Losses {
        live: LiveSessions { shells, logs: 0 },
        forwards,
        edits,
    };
    assert_eq!(
        dialog(Ending::App, losses(1, 0, 0)).text,
        "Quitting stops 1 running port forward."
    );
    assert_eq!(
        dialog(Ending::Window, losses(3, 0, 0)).text,
        "Closing the window stops 3 running port forwards."
    );
    assert_eq!(
        dialog(Ending::App, losses(2, 1, 0)).text,
        "Quitting ends 1 session still running in the dock: 1 shell, and stops 2 running port \
         forwards. Its output is not kept."
    );
    assert_eq!(
        dialog(Ending::App, losses(1, 0, 1)).text,
        "Quitting stops 1 running port forward. It also discards 1 unapplied YAML edit, which \
         cannot be undone."
    );
}

// Minimize and Zoom act on the active window: the native menu enables them while there is one,
// also with a dialog holding focus, and disables them when there is none. Hiding stays the app's.
// The test window implements neither, so this asks and does not dispatch them.
#[gpui_kit::test]
async fn minimize_and_zoom_need_an_active_window(cx: &mut TestAppContext) {
    use crate::ui::registry::{Hide, MinimizeWindow, ZoomWindow};
    let offered = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            [
                cx.is_action_available(&MinimizeWindow),
                cx.is_action_available(&ZoomWindow),
            ]
        })
    };
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let (_, _web) = open_fake_shell_in(cx, &t, "web");
    focus_list(cx, &t);
    assert_eq!(offered(cx), [true, true], "with the window active");

    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    assert_eq!(offered(cx), [true, true], "with a dialog holding focus");
    cancel(cx, &t).await;

    cx.update_window(t.window, |_, window, cx| window.blur(cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(offered(cx), [true, true], "with nothing focused");

    VisualTestContext::from_window(t.window, cx).deactivate_window();
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.active_window()), None);
    assert_eq!(offered(cx), [false, false], "with no active window");
    assert!(cx.update(|cx| cx.is_action_available(&Hide)));
}

// The Window menu holds Minimize and Zoom, and the File menu the closing items. The test window
// implements neither, so this checks their place and does not dispatch them.
#[gpui_kit::test]
async fn the_window_and_help_menus_hold_the_standard_items(cx: &mut TestAppContext) {
    use gpui_kit::OwnedMenuItem;
    let dir = tempfile::tempdir().unwrap();
    let t = open(cx, dir.path()).await;
    let menus = cx.update(|cx| cx.get_menus()).unwrap();
    let names = |menu: &str| -> Vec<String> {
        let menu = menus.iter().find(|m| m.name == menu).unwrap();
        menu.items
            .iter()
            .filter_map(|item| match item {
                OwnedMenuItem::Action { action, .. } => Some(action.name().to_owned()),
                OwnedMenuItem::SystemMenu(menu) => Some(menu.name.to_string()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        names("Window"),
        ["flint::MinimizeWindow", "flint::ZoomWindow"]
    );
    assert_eq!(
        names("File")[3..],
        // Close Dock Tab holds an action nothing handles while no dock tab is shown.
        ["menu::Unavailable", "flint::CloseWindow"]
    );
    assert_eq!(names("Help"), ["flint::OpenHelp"]);
    assert!(names("Flint").contains(&"Services".to_owned()));

    t.dispatch(cx, "flint::OpenHelp", Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.opened_url().as_deref(),
        Some(env!("CARGO_PKG_REPOSITORY"))
    );
}
