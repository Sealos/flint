//! View tests for narrow windows: below the breakpoint the detail shows alone, with a Back control
//! that returns to the list and keeps the selection, and a confirmation stays fully usable at the
//! minimum window size.

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement, Pixels, Render,
    Styled as _, TestAppContext, VisualTestContext, Window, div, px, size,
};
use serde_json::Value;

use crate::ui::confirm::{self, DialogText};
use crate::ui::support_tests::{TestWorkspace, open_workspace};
use crate::ui::view_state::Focus;

const ROWS: &[(&str, &str, i64)] = &[("demo", "api", 0), ("demo", "web", 0)];

fn resize(cx: &mut TestAppContext, t: &TestWorkspace, width: f32, height: f32) {
    VisualTestContext::from_window(t.window, cx).simulate_resize(size(px(width), px(height)));
    draw(cx, t);
}

/// Draws until focus moved by a draw has settled.
fn draw(cx: &mut TestAppContext, t: &TestWorkspace) {
    for _ in 0..2 {
        cx.run_until_parked();
        cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
    cx.run_until_parked();
}

fn shown(cx: &mut TestAppContext, t: &TestWorkspace, id: &'static str) -> bool {
    cx.update_window(t.window, |_, window, _| window.try_find(id).is_some())
        .unwrap()
}

fn focus(cx: &mut TestAppContext, t: &TestWorkspace) -> Focus {
    draw(cx, t);
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace
            .update(cx, |ws, cx| ws.view_state(window, cx))
            .focus
    })
    .unwrap()
}

fn bounds(cx: &mut TestAppContext, t: &TestWorkspace, id: &'static str) -> Bounds<Pixels> {
    cx.update_window(t.window, |_, window, _| window.find(id).bounds())
        .unwrap()
}

/// A window at the minimum size, with `web` selected and its detail showing alone.
async fn detail_alone(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.open_pods(cx, ROWS).await;
    resize(cx, t, 760., 480.);
    t.click(cx, "row-uid-web");
    draw(cx, t);
    assert!(t.detail(cx).is_some());
}

// #421: below the breakpoint the detail replaces the list, and Back (a click) returns to the
// list with the row still selected and the detail still open behind it.
#[gpui_kit::test]
async fn a_narrow_window_shows_the_detail_alone_and_back_returns(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    assert!(!shown(cx, &t, "row-uid-web"), "the list gives way");
    assert!(shown(cx, &t, "detail-back"));
    assert_eq!(focus(cx, &t), Focus::Detail, "focus follows what is shown");

    t.click(cx, "detail-back");
    draw(cx, &t);
    assert!(shown(cx, &t, "row-uid-web"));
    assert!(!shown(cx, &t, "detail-back"));
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert!(t.detail(cx).is_some(), "Back does not close the detail");
    assert_eq!(focus(cx, &t), Focus::List);

    // The selected row opens it again, though the selection does not change.
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    assert!(!shown(cx, &t, "row-uid-web"));
    assert!(shown(cx, &t, "detail-back"));
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
}

// #421: Back has a key and a registered action, offered only while the detail shows alone.
#[gpui_kit::test]
async fn back_is_reachable_by_keyboard(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, ROWS).await;
    assert!(!t.available(cx).contains(&"workspace::BackToList"));
    resize(cx, &t, 760., 480.);
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    assert!(t.available(cx).contains(&"workspace::BackToList"));

    t.press_key_of(cx, "workspace::BackToList");
    draw(cx, &t);
    assert!(shown(cx, &t, "row-uid-web"));
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert_eq!(focus(cx, &t), Focus::List);
    assert!(!t.available(cx).contains(&"workspace::BackToList"));

    // Moving through the rows stays on the list, and the detail follows behind it.
    t.press(cx, "up");
    draw(cx, &t);
    assert!(shown(cx, &t, "row-uid-api"));
    assert_eq!(t.selected(cx).as_deref(), Some("api"));

    // Tab reaches the detail pane, which shows it; Shift-Tab goes back to the list.
    t.press(cx, "tab");
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"));
    assert_eq!(focus(cx, &t), Focus::Detail);
    t.press(cx, "shift-tab");
    draw(cx, &t);
    assert!(shown(cx, &t, "row-uid-api"));
    assert_eq!(focus(cx, &t), Focus::List);
}

// #421: closing the detail still works from the narrow layout, by key and by action.
#[gpui_kit::test]
async fn closing_the_detail_alone_returns_to_the_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    t.press(cx, "escape");
    draw(cx, &t);
    assert!(t.detail(cx).is_none());
    assert_eq!(t.selected(cx), None);
    assert!(shown(cx, &t, "row-uid-web"));
    assert!(!shown(cx, &t, "detail-back"));

    t.click(cx, "row-uid-api");
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"), "a new detail shows alone");
    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    draw(cx, &t);
    assert!(t.detail(cx).is_none());
    assert!(shown(cx, &t, "row-uid-api"));
}

// #421: resizing across the breakpoint moves between the layouts and keeps focus on a pane
// that is drawn.
#[gpui_kit::test]
async fn resizing_across_the_breakpoint_keeps_focus_on_a_shown_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, ROWS).await;
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    assert_eq!(focus(cx, &t), Focus::List);
    assert!(!shown(cx, &t, "detail-back"), "side by side when it fits");

    resize(cx, &t, 849., 600.);
    assert!(!shown(cx, &t, "row-uid-web"));
    assert_eq!(focus(cx, &t), Focus::Detail);

    t.click(cx, "detail-back");
    draw(cx, &t);
    resize(cx, &t, 850., 600.);
    assert!(shown(cx, &t, "row-uid-web"));
    assert!(!shown(cx, &t, "detail-back"));
    assert_eq!(t.selected(cx).as_deref(), Some("web"));

    // Narrowed again, the detail is what shows: going Back lasted until the window widened.
    resize(cx, &t, 760., 480.);
    assert!(!shown(cx, &t, "row-uid-web"));
    assert!(shown(cx, &t, "detail-back"));
}

// #421: at the minimum size a confirmation with a long text keeps its text inside the dialog,
// scrolling, with both buttons on screen and working.
#[gpui_kit::test]
async fn a_long_confirmation_fits_the_minimum_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    let warnings = vec!["Managed by a controller; this will likely be reverted."; 40].join("\n");
    let dialog = DialogText {
        title: "Delete web on a cluster with quite a long name?".into(),
        text: format!("{warnings}\n\nContext: alpha\nObject: pods web"),
        ok: "Delete".into(),
        cancel: "Cancel".into(),
        id: "write-confirm-description",
        default_cancel: false,
    };
    let confirmed = std::rc::Rc::new(std::cell::Cell::new(false));
    let set = confirmed.clone();
    cx.update_window(t.window, |_, window, cx| {
        confirm::open(window, cx, dialog, move |_, _| set.set(true))
    })
    .unwrap();
    draw(cx, &t);

    let text = bounds(cx, &t, "write-confirm-description");
    for button in ["ok", "cancel"] {
        let at = bounds(cx, &t, button);
        assert!(
            at.origin.y >= px(0.) && at.bottom() <= px(480.),
            "{button} is on screen: {at:?}"
        );
        assert!(at.right() <= px(760.), "{button} is on screen: {at:?}");
        assert!(
            text.bottom() <= at.origin.y,
            "the text stops above {button}"
        );
    }
    t.click(cx, "ok");
    draw(cx, &t);
    assert!(confirmed.get());
    assert!(!t.has_dialog(cx));
}

/// Controls as tall as the drain dialog's options.
struct Controls;

impl Render for Controls {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().id("confirm-controls").h(px(120.)).test_support()
    }
}

// #421: controls under a long text stay in the dialog, above its buttons, at the minimum size.
#[gpui_kit::test]
async fn controls_under_a_long_text_fit_the_minimum_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    let dialog = DialogText {
        title: "Drain a node with quite a long name".into(),
        text: vec!["Cordons the node and evicts its pods."; 40].join("\n"),
        ok: "Continue".into(),
        cancel: "Cancel".into(),
        id: "drain-description",
        default_cancel: false,
    };
    cx.update_window(t.window, |_, window, cx| {
        let content = cx.new(|_| Controls).into();
        confirm::open_with(window, cx, dialog, Some(content), |_, _| true)
    })
    .unwrap();
    draw(cx, &t);

    let controls = bounds(cx, &t, "confirm-controls");
    let ok = bounds(cx, &t, "ok");
    assert!(
        controls.bottom() <= ok.origin.y,
        "{controls:?} above {ok:?}"
    );
    assert!(ok.bottom() <= px(480.), "{ok:?}");
}

// #421: the main window cannot be made smaller than the layout supports.
#[gpui_kit::test]
fn the_main_window_has_a_minimum_size(cx: &mut TestAppContext) {
    let bounds = Bounds::default();
    let options = cx.update(|_| crate::ui::window_options(bounds));
    assert_eq!(options.window_min_size, Some(size(px(760.), px(480.))));
}
