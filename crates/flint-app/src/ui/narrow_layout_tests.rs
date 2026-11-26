//! View tests for narrow windows: below the breakpoint the detail shows alone, with a Back control
//! that returns to the list and keeps the selection, and a confirmation stays fully usable at the
//! minimum window size. In a short window the list keeps several rows beside the dock.

use flint_core::{ConnectionState, Event, ServerVersion};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AppContext as _, Bounds, Context, ElementId, InteractiveElement as _, IntoElement, Pixels,
    Render, Styled as _, TestAppContext, VisualTestContext, Window, div, px, size,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::ui::confirm::{self, DialogText};
use crate::ui::support_tests::{
    CURRENT_GENERATION, TestWorkspace, discovery, kubeconfig_yaml, objects, open_workspace,
    open_workspace_with, pods, standard_sections,
};
use crate::ui::view_state::Focus;
use flint_core::{KindId, SyncState};

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

// UP-33: a long namespace in the filter is cut short in its button, so the watch mode button
// next to it stays on screen at the minimum width with the detail alone.
#[gpui_kit::test]
async fn a_long_namespace_keeps_the_watch_mode_button_on_screen(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let long = "a-very-long-namespace-name-for-a-team-that-keeps-going-and-going-on";
    detail_alone(cx, &t).await;
    t.dispatch(
        cx,
        "workspace::ToggleNamespace",
        serde_json::json!({ "namespace": long }),
    )
    .unwrap();
    // The button's name and tooltip keep the whole namespace, while its text is cut short.
    t.wait_for_label(cx, "namespace-filter-button", long).await;
    draw(cx, &t);
    let tooltip = t.description(cx, "namespace-filter-button");
    assert!(
        tooltip.is_some_and(|text| text.contains(long)),
        "the tooltip names the whole namespace"
    );
    draw(cx, &t);
    for id in ["namespace-filter-button", "watch-mode"] {
        let at = bounds(cx, &t, id);
        assert!(
            at.origin.x >= px(0.) && at.right() <= px(760.),
            "{id} is on screen: {at:?}"
        );
        assert!(at.size.width > px(40.), "{id} stays clickable: {at:?}");
    }
}

/// Asserts the header's access mode and controls are whole and inside the window's width.
#[track_caller]
fn assert_header_on_screen(cx: &mut TestAppContext, t: &TestWorkspace) {
    draw(cx, t);
    for id in ["access-mode", "namespace-filter-button", "watch-mode"] {
        let at = bounds(cx, t, id);
        assert!(
            at.origin.x >= px(0.) && at.right() <= px(760.),
            "{id} is on screen: {at:?}"
        );
    }
    let mode = bounds(cx, t, "access-mode");
    let filter = bounds(cx, t, "namespace-filter-button");
    assert!(
        mode.right() <= filter.origin.x || mode.bottom() <= filter.origin.y,
        "the access mode and the filter do not overlap: {mode:?} {filter:?}"
    );
}

// #432: the access mode stays on screen at the minimum width, over the list and the detail.
#[gpui_kit::test]
async fn the_access_mode_fits_the_minimum_width(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    resize(cx, &t, 760., 480.);
    assert_header_on_screen(cx, &t);
    detail_alone(cx, &t).await;
    assert_header_on_screen(cx, &t);
}

// #412: the longer limited label still fits beside the access mode and the watch mode.
#[gpui_kit::test]
async fn the_limited_namespace_label_fits_the_minimum_width(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, ROWS).await;
    let mut limited = (*pods(ROWS)).clone();
    limited.limited_to = Some(Arc::new(flint_core::NamespaceLimit {
        listed: (0..12).map(|i| format!("team-{i:02}")).collect(),
        denied: Vec::new(),
        reason: "pods is forbidden".into(),
        revisions: vec![1; 12],
    }));
    let list = t.list(cx).unwrap();
    list.update(cx, |list, cx| list.set_snapshot(Arc::new(limited), cx));
    t.wait_for_label(cx, "namespace-filter-button", "12 namespaces (limited)")
        .await;
    resize(cx, &t, 760., 480.);
    assert_header_on_screen(cx, &t);

    // The longest form: a selection the list only partly covers.
    for i in 0..13 {
        t.dispatch(
            cx,
            "workspace::ToggleNamespace",
            serde_json::json!({"namespace": format!("team-{i:02}")}),
        )
        .unwrap();
    }
    t.wait_for_label(
        cx,
        "namespace-filter-button",
        "12 of 13 namespaces (limited)",
    )
    .await;
    assert_header_on_screen(cx, &t);
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

    resize(cx, &t, 949., 600.);
    assert!(!shown(cx, &t, "row-uid-web"));
    assert_eq!(focus(cx, &t), Focus::Detail);

    t.click(cx, "detail-back");
    draw(cx, &t);
    resize(cx, &t, 950., 600.);
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

/// Asserts the target lines are inside the open dialog and Confirm is on screen.
fn assert_target_shown(cx: &mut TestAppContext, t: &TestWorkspace) {
    let surface = bounds(cx, t, "surface");
    let target = bounds(cx, t, "confirm-target");
    let ok = bounds(cx, t, "ok");
    assert!(target.size.height > px(0.), "{target:?}");
    assert!(
        target.origin.y >= surface.origin.y && target.bottom() <= surface.bottom(),
        "{target:?} inside {surface:?}"
    );
    assert!(target.bottom() <= ok.origin.y, "{target:?} above {ok:?}");
    assert!(
        ok.origin.y >= surface.origin.y && ok.bottom() <= surface.bottom().min(px(480.)),
        "{ok:?} inside {surface:?}"
    );
}

// #421: a write confirmation always shows its context, server, namespace and object, at the
// minimum size and with the longest warnings, and the warnings scroll by keyboard.
#[gpui_kit::test]
async fn a_write_confirmation_keeps_its_target_on_show(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    let warnings = vec!["Managed by a controller; this will likely be reverted."; 200].join("\n");
    let long = "a-very-long-generated-name-".repeat(6);
    let delete = "Context: alpha\nServer: https://alpha.example.invalid:6443\nNamespace: demo\n\
                  Object: pods web"
        .to_owned();
    // A node has no namespace.
    let drain = format!(
        "Context: {long}\nServer: https://alpha.example.invalid:6443\nObject: nodes {long}"
    );
    for (title, ok, target, pinned) in [
        ("Delete web?", "Delete", delete, 4),
        ("Drain node-1?", "Drain", drain, 3),
    ] {
        let confirmed = std::rc::Rc::new(std::cell::Cell::new(false));
        let set = confirmed.clone();
        let dialog = DialogText {
            title: title.into(),
            text: format!("{warnings}\n\n{target}"),
            ok: ok.into(),
            cancel: "Cancel".into(),
            id: "write-confirm-description",
            default_cancel: true,
        };
        cx.update_window(t.window, |_, window, cx| {
            confirm::open_pinned(window, cx, dialog, pinned, move |_, _| set.set(true))
        })
        .unwrap();
        draw(cx, &t);
        assert_target_shown(cx, &t);

        // Tab reaches the warnings, and the keys scroll them under the pinned lines.
        let before = bounds(cx, &t, "confirm-text-body").origin.y;
        t.press(cx, "tab");
        draw(cx, &t);
        let focused = cx
            .update_window(t.window, |_, window, _| {
                window.find("confirm-text").focused()
            })
            .unwrap();
        assert_eq!(focused, Some(true), "the warnings are a Tab stop");
        t.press(cx, "pagedown");
        draw(cx, &t);
        assert!(bounds(cx, &t, "confirm-text-body").origin.y < before);
        t.press(cx, "end");
        draw(cx, &t);
        assert_target_shown(cx, &t);
        t.press(cx, "home");
        draw(cx, &t);
        assert_eq!(bounds(cx, &t, "confirm-text-body").origin.y, before);

        // Enter on the warnings cancels a deliberate action, as it does everywhere in it.
        t.press(cx, "enter");
        draw(cx, &t);
        assert!(!t.has_dialog(cx));
        assert!(!confirmed.get(), "{title}");
    }
}

// #421: right-clicking a row in a narrow window selects it and leaves it drawn, with its menu.
#[gpui_kit::test]
async fn a_right_click_keeps_the_row_and_its_menu(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, ROWS).await;
    resize(cx, &t, 760., 480.);
    cx.update_window(t.window, |_, window, cx| {
        window.right_click("row-uid-web", cx)
    })
    .unwrap();
    cx.wait_for(t.window, Duration::from_secs(5), |window, _| {
        window.try_find("popup-menu").is_some()
    })
    .await;
    draw(cx, &t);
    assert!(shown(cx, &t, "popup-menu"), "the menu stays open");
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert!(shown(cx, &t, "row-uid-web"), "the row stays");
    assert!(!shown(cx, &t, "detail-back"));

    // A plain click on it still opens the detail.
    t.press(cx, "escape");
    draw(cx, &t);
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"));
}

// covers UP-33: Enter on the selected row brings its detail back in front, as a click does; an
// open menu or dialog keeps Enter.
#[gpui_kit::test]
async fn enter_on_the_selected_row_shows_its_detail(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    t.click(cx, "detail-back");
    draw(cx, &t);
    assert_eq!(focus(cx, &t), Focus::List);

    // A dialog over the list: Enter answers it (here, Cancel) and the list stays in front.
    let dialog = DialogText {
        title: "Delete web?".into(),
        text: "Object: pods web".into(),
        ok: "Delete".into(),
        cancel: "Cancel".into(),
        id: "write-confirm-description",
        default_cancel: true,
    };
    let confirmed = std::rc::Rc::new(std::cell::Cell::new(false));
    let set = confirmed.clone();
    cx.update_window(t.window, |_, window, cx| {
        confirm::open(window, cx, dialog, move |_, _| set.set(true))
    })
    .unwrap();
    draw(cx, &t);
    t.press(cx, "enter");
    draw(cx, &t);
    assert!(!confirmed.get(), "Enter took the dialog's default, Cancel");
    assert!(!shown(cx, &t, "write-confirm-description"));
    assert!(shown(cx, &t, "row-uid-web"), "the list stays in front");

    // A row's menu: Enter is the menu's, so the list stays in front.
    cx.update_window(t.window, |_, window, cx| {
        window.right_click("row-uid-web", cx)
    })
    .unwrap();
    cx.wait_for(t.window, Duration::from_secs(5), |window, _| {
        window.try_find("popup-menu").is_some()
    })
    .await;
    draw(cx, &t);
    t.press(cx, "enter");
    draw(cx, &t);
    assert!(!shown(cx, &t, "detail-back"), "the menu kept Enter");
    if shown(cx, &t, "popup-menu") {
        t.press(cx, "escape");
        draw(cx, &t);
    }
    assert_eq!(focus(cx, &t), Focus::List);

    t.press(cx, "enter");
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"), "Enter shows the detail");
    assert!(!shown(cx, &t, "row-uid-web"));
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert_eq!(focus(cx, &t), Focus::Detail);
}

// #421: Back's key is a letter, which native key events report the same on every platform.
#[test]
fn the_back_key_survives_key_normalization() {
    let spec = crate::ui::registry::find("workspace::BackToList").unwrap();
    assert_eq!(spec.keys, ["secondary-shift-b"]);
}

// #421: Edit YAML from behind the list brings the detail forward, with its editor.
#[gpui_kit::test]
async fn editing_from_behind_the_list_shows_the_detail(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    let object = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web", "resourceVersion": "1"},
    });
    let fetched = flint_core::ObjectDetail {
        yaml: serde_saphyr::to_string(&object).unwrap(),
        object,
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    };
    let view = t.detail(cx).unwrap();
    cx.update_window(t.window, |_, window, cx| {
        view.update(cx, |view, cx| view.show_fetched(fetched, window, cx))
    })
    .unwrap();
    draw(cx, &t);
    t.click(cx, "detail-back");
    draw(cx, &t);
    assert!(!shown(cx, &t, "detail-back"));
    t.dispatch(cx, "detail::EditYaml", Value::Null).unwrap();
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"), "the detail is in front");
    assert_eq!(focus(cx, &t), Focus::DetailYaml);
}

// #421 (UP-33): opening the current kind again brings its list in front of the detail.
#[gpui_kit::test]
async fn reopening_the_current_kind_shows_the_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    t.dispatch(
        cx,
        "workspace::OpenView",
        serde_json::json!({"kind": "pods"}),
    )
    .unwrap();
    draw(cx, &t);
    assert!(shown(cx, &t, "row-uid-web"));
    assert!(!shown(cx, &t, "detail-back"));
    assert_eq!(focus(cx, &t), Focus::List);
    assert!(t.detail(cx).is_some(), "the detail stays open behind it");
}

// #421: a control focused before a dialog, in a pane hidden while it was open, does not keep
// focus once the dialog closes: the keys keep working from the pane on show.
#[gpui_kit::test]
async fn closing_a_dialog_over_a_hidden_pane_restores_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, ROWS).await;
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    t.dispatch(cx, "resource_list::FocusFilter", Value::Null)
        .unwrap();
    draw(cx, &t);
    assert_eq!(focus(cx, &t), Focus::ListFilter);
    let dialog = DialogText {
        title: "Asked".into(),
        text: "Asked this.".into(),
        ok: "Go".into(),
        cancel: "Cancel".into(),
        id: "test-description",
        default_cancel: false,
    };
    cx.update_window(t.window, |_, window, cx| {
        confirm::open(window, cx, dialog, |_, _| {})
    })
    .unwrap();
    draw(cx, &t);
    resize(cx, &t, 760., 480.);
    t.press(cx, "escape");
    draw(cx, &t);
    assert!(!t.has_dialog(cx));
    assert!(shown(cx, &t, "detail-back"));
    assert_eq!(focus(cx, &t), Focus::Detail);
}

// #421: outside the detail-only layout Back's key does nothing and is left to others.
#[gpui_kit::test]
async fn the_back_key_only_acts_on_the_detail_alone(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, ROWS).await;
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Detail, "side by side, on the detail");
    t.press_key_of(cx, "workspace::BackToList");
    assert_eq!(focus(cx, &t), Focus::Detail, "focus stays where it was");
    assert!(!t.available(cx).contains(&"workspace::BackToList"));
}

// #420: in a short window the dock gives way so the list keeps several rows, and a taller window
// gets the remembered height back.
#[gpui_kit::test]
async fn a_short_window_keeps_list_rows_beside_the_dock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let rows = [("a", "row-uid-a"), ("b", "row-uid-b"), ("c", "row-uid-c")]
        .into_iter()
        .chain([("d", "row-uid-d"), ("e", "row-uid-e"), ("f", "row-uid-f")])
        .collect::<Vec<_>>();
    let pods: Vec<_> = rows.iter().map(|(name, _)| ("demo", *name, 0)).collect();
    t.open_pods(cx, &pods).await;
    t.dispatch(cx, "dock::ToggleDock", Value::Null).unwrap();
    t.dispatch(
        cx,
        "workspace::SetDockHeight",
        serde_json::json!({"height": 400}),
    )
    .unwrap();
    resize(cx, &t, 700., 420.);

    let dock = bounds(cx, &t, "dock-pane");
    assert_eq!(dock.size.height, px(140.));
    for (_, id) in rows {
        let row = bounds(cx, &t, id);
        assert!(row.bottom() <= dock.top(), "{row:?} above {dock:?}");
    }
    assert_eq!(t.workspace.read_with(cx, |ws, _| ws.panes.dock), Some(400));

    resize(cx, &t, 1000., 800.);
    assert_eq!(bounds(cx, &t, "dock-pane").size.height, px(400.));
}

// #433: at the minimum size a long object name gives way before the cluster, which stays whole.
#[gpui_kit::test]
async fn the_breadcrumb_keeps_the_cluster_at_the_minimum_size(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let long = "web-".repeat(60);
    let rows = [("demo", long.as_str(), 0)];
    t.open_pods(cx, &rows).await;
    let cluster = ElementId::from(("crumb", 0usize));
    let whole = crumb_width(cx, &t, &cluster);

    resize(cx, &t, 760., 480.);
    t.click(cx, ElementId::Name(format!("row-uid-{long}").into()));
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"));

    assert_eq!(t.label(cx, cluster.clone()).as_deref(), Some("kind-alpha"));
    let narrow = crumb_width(cx, &t, &cluster);
    assert_eq!(narrow, whole, "the cluster's name is not cut");
}

fn crumb_width(cx: &mut TestAppContext, t: &TestWorkspace, id: &ElementId) -> Pixels {
    crumb_bounds(cx, t, id).size.width
}

fn crumb_bounds(cx: &mut TestAppContext, t: &TestWorkspace, id: &ElementId) -> Bounds<Pixels> {
    let id = id.clone();
    cx.update_window(t.window, |_, window, _| window.find(id).bounds())
        .unwrap()
}

/// A workspace showing `kind-alpha` under the custom name `name`.
async fn workspace_with_cluster_name(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    name: &str,
) -> TestWorkspace {
    let settings = format!("[cluster_names]\nkind-alpha = \"{name}\"\n");
    std::fs::write(dir.join("settings.toml"), settings).unwrap();
    open_workspace(cx, dir).await
}

/// The cluster crumb's width with room for all of it.
fn full_cluster_width(cx: &mut TestAppContext, t: &TestWorkspace) -> Pixels {
    resize(cx, t, 2400., 900.);
    crumb_width(cx, t, &ElementId::from(("crumb", 0usize)))
}

// #433: alone in the breadcrumb, a long cluster name uses the whole row.
#[gpui_kit::test]
async fn a_long_cluster_name_alone_shows_in_full(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let name = "prod-eu-1";
    let t = workspace_with_cluster_name(cx, dir.path(), name).await;
    t.connect_alpha(cx, standard_sections()).await;
    assert!(t.list(cx).is_none(), "the cluster is the only crumb");
    let whole = full_cluster_width(cx, &t);

    resize(cx, &t, 760., 480.);
    let cluster = ElementId::from(("crumb", 0usize));
    assert_eq!(t.label(cx, cluster.clone()).as_deref(), Some(name));
    assert_eq!(crumb_width(cx, &t, &cluster), whole);
}

// #433: a long object name gives way before a long cluster name that fits.
#[gpui_kit::test]
async fn a_long_object_gives_way_before_a_long_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = workspace_with_cluster_name(cx, dir.path(), "production-eu").await;
    let object = "web-".repeat(60);
    let rows = [("demo", object.as_str(), 0)];
    t.open_pods(cx, &rows).await;
    let whole = full_cluster_width(cx, &t);

    resize(cx, &t, 760., 480.);
    t.click(cx, ElementId::Name(format!("row-uid-{object}").into()));
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"));

    let cluster = ElementId::from(("crumb", 0usize));
    assert_eq!(crumb_width(cx, &t, &cluster), whole);
    let shown_object = crumb_width(cx, &t, &ElementId::from(("crumb", 2usize)));
    assert!(
        shown_object < px(760.),
        "the object is cut: {shown_object:?}"
    );
}

/// A context name that stays wider than the header's first row once shortened to its display
/// length: each of these characters is drawn twice as wide as a Latin capital in tests.
const WIDE_CONTEXT: &str = "𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀𠀀";

/// A workspace over the one context `context`, connected, with Pods open.
async fn workspace_on_context(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    context: &'static str,
) -> TestWorkspace {
    let t = open_workspace_with(cx, dir, |dir, servers| {
        let kubeconfig = dir.join("kubeconfig");
        let yaml = kubeconfig_yaml(&[(context, &servers.silent)]);
        std::fs::write(&kubeconfig, yaml).unwrap();
        vec![kubeconfig]
    })
    .await;
    t.activate(cx, context).await;
    t.wait_for_state(cx, context, |s| matches!(s, ConnectionState::Connecting))
        .await;
    t.feed(
        cx,
        Event::ConnectionChanged {
            cluster: context.into(),
            state: ConnectionState::Connected(ServerVersion {
                git_version: "v1.33.1".into(),
                platform: "linux/arm64".into(),
            }),
        },
    );
    t.feed(
        cx,
        Event::DiscoveryChanged {
            cluster: context.into(),
            generation: CURRENT_GENERATION,
            discovery: Some(Arc::new(discovery(standard_sections()))),
            error: None,
        },
    );
    t.open_kind(cx, "pods", pods(ROWS)).await;
    t
}

// #433: a cluster name wider than the first row is cut there, clear of Back and the controls.
#[gpui_kit::test]
async fn a_cluster_name_too_long_to_fit_truncates_inside_the_header(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = workspace_on_context(cx, dir.path(), WIDE_CONTEXT).await;
    let whole = full_cluster_width(cx, &t);

    resize(cx, &t, 760., 480.);
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"));

    let cluster = crumb_bounds(cx, &t, &ElementId::from(("crumb", 0usize)));
    assert!(cluster.size.width < whole, "the name is cut");
    let header = bounds(cx, &t, "main-header");
    assert!(
        cluster.right() <= header.right() - px(12.),
        "{cluster:?} stays inside the row of {header:?}"
    );
    let back = bounds(cx, &t, "detail-back");
    assert!(
        cluster.left() >= back.right(),
        "{cluster:?} clear of {back:?}"
    );
    for id in ["namespace-filter-button", "watch-mode"] {
        let control = bounds(cx, &t, id);
        assert!(
            cluster.bottom() <= control.top(),
            "{cluster:?} above {id}: {control:?}"
        );
    }
}

// #433: the longest custom name fits the first row whole, above the controls.
#[gpui_kit::test]
async fn the_longest_custom_name_shows_whole_above_the_controls(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let name = "W".repeat(flint_core::display::MAX_CUSTOM_NAME_CHARS);
    let t = workspace_with_cluster_name(cx, dir.path(), &name).await;
    t.open_pods(cx, ROWS).await;
    let whole = full_cluster_width(cx, &t);

    resize(cx, &t, 760., 480.);
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    assert!(shown(cx, &t, "detail-back"));

    let cluster = crumb_bounds(cx, &t, &ElementId::from(("crumb", 0usize)));
    assert_eq!(cluster.size.width, whole, "the name is not cut");
    let controls = bounds(cx, &t, "namespace-filter-button");
    assert!(
        cluster.bottom() <= controls.top(),
        "{cluster:?} above {controls:?}"
    );
}

// #433: with the controls on the breadcrumb's row, a long cluster name is cut before them.
#[gpui_kit::test]
async fn a_long_cluster_name_stops_before_controls_on_its_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let name = "W".repeat(flint_core::display::MAX_CUSTOM_NAME_CHARS);
    let t = workspace_with_cluster_name(cx, dir.path(), &name).await;
    t.open_pods(cx, ROWS).await;
    let whole = full_cluster_width(cx, &t);

    // Just wide enough for one row.
    resize(cx, &t, 1062., 480.);
    assert!(!shown(cx, &t, "main-header"), "the list keeps one row");
    let cluster = crumb_bounds(cx, &t, &ElementId::from(("crumb", 0usize)));
    assert!(cluster.size.width < whole, "the name is cut");
    let controls = bounds(cx, &t, "access-mode");
    assert!(
        cluster.right() <= controls.left(),
        "{cluster:?} runs into {controls:?}"
    );
}

// #433: the controls on the second row still work through their registered actions.
#[gpui_kit::test]
async fn the_second_row_controls_work_by_their_actions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;

    t.dispatch(cx, "workspace::OpenNamespaceFilter", Value::Null)
        .unwrap();
    draw(cx, &t);
    assert!(shown(cx, &t, "ns-all"), "the picker opens");
    t.press(cx, "escape");
    draw(cx, &t);
    assert!(!shown(cx, &t, "ns-all"));
    let focused = t
        .find(cx, "namespace-filter-button")
        .and_then(|button| button.focused());
    assert_eq!(focused, Some(true), "focus returns to the button");

    t.dispatch(cx, "workspace::WatchOpenViewsOnly", Value::Null)
        .unwrap();
    t.wait_for_label(cx, "watch-mode", "Live: on view").await;
}

// #433: at the minimum size the detail-alone header moves its controls to a second row, inside
// the window and above the detail.
#[gpui_kit::test]
async fn the_detail_alone_header_puts_its_controls_on_a_second_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;

    let back = bounds(cx, &t, "detail-back");
    let header = bounds(cx, &t, "main-header");
    let detail = bounds(cx, &t, "detail-alone-pane");
    for id in ["namespace-filter-button", "watch-mode"] {
        let at = bounds(cx, &t, id);
        assert!(at.top() >= back.bottom(), "{id} is below Back: {at:?}");
        assert!(
            at.left() >= px(0.) && at.right() <= px(760.) && at.bottom() <= px(480.),
            "{id} is inside the window: {at:?}"
        );
        assert!(at.bottom() <= header.bottom(), "{id} is in the header");
        assert!(at.bottom() <= detail.top(), "{at:?} above {detail:?}");
    }
    assert_eq!(detail.top(), header.bottom(), "the detail starts below it");
    assert_eq!(detail.bottom(), px(480.), "the detail is not cut off");
}

// #433: with the dock at its tallest, the two-row header still leaves a failed detail its Retry.
#[gpui_kit::test]
async fn a_failed_detail_keeps_retry_above_a_tall_dock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, ROWS).await;
    t.click(cx, "row-uid-web");
    draw(cx, &t);
    let view = t.detail(cx).unwrap();
    view.update(cx, |view, cx| {
        view.show_failed("connection refused".into(), cx)
    });
    t.dispatch(cx, "dock::ToggleDock", Value::Null).unwrap();
    // Taller than the dock gets at the minimum window height.
    t.dispatch(
        cx,
        "workspace::SetDockHeight",
        serde_json::json!({"height": 320}),
    )
    .unwrap();
    resize(cx, &t, 760., 480.);
    assert!(shown(cx, &t, "header-controls-row"));

    let pane = bounds(cx, &t, "detail-alone-pane");
    let dock = bounds(cx, &t, "dock-pane");
    let retry = bounds(cx, &t, "detail-retry");
    assert!(pane.size.height >= px(128.), "{pane:?}");
    assert!(
        retry.top() >= pane.top() && retry.bottom() <= pane.bottom(),
        "{retry:?} inside {pane:?}"
    );
    assert!(retry.bottom() <= dock.top(), "{retry:?} above {dock:?}");

    // The limit does not rewrite the saved height: a taller window gets it back.
    let saved = t.workspace.read_with(cx, |ws, _| ws.panes.dock);
    assert_eq!(saved, Some(320));
    t.click(cx, "detail-back");
    draw(cx, &t);
    assert_eq!(bounds(cx, &t, "dock-pane").size.height, px(200.));
    resize(cx, &t, 760., 800.);
    assert_eq!(bounds(cx, &t, "dock-pane").size.height, px(320.));
}

#[gpui_kit::test]
async fn a_long_failed_detail_keeps_retry_above_a_tall_dock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    t.dispatch(cx, "dock::ToggleDock", Value::Null).unwrap();
    t.dispatch(
        cx,
        "workspace::SetDockHeight",
        serde_json::json!({"height": 320}),
    )
    .unwrap();
    let error = "The API server refused the connection while fetching the object.\n".repeat(64);
    t.detail(cx)
        .unwrap()
        .update(cx, |view, cx| view.show_failed(error, cx));
    draw(cx, &t);

    let pane = bounds(cx, &t, "detail-alone-pane");
    let dock = bounds(cx, &t, "dock-pane");
    let retry = bounds(cx, &t, "detail-retry");
    assert!(retry.size.width > px(0.) && retry.size.height > px(0.));
    assert!(
        retry.left() >= pane.left()
            && retry.right() <= pane.right()
            && retry.top() >= pane.top()
            && retry.bottom() <= pane.bottom().min(dock.top()),
        "{retry:?} inside {pane:?} and above {dock:?}"
    );
}

// #421: the main window cannot be made smaller than the layout supports.
#[gpui_kit::test]
fn the_main_window_has_a_minimum_size(cx: &mut TestAppContext) {
    let bounds = Bounds::default();
    let options = cx.update(|_| crate::ui::window_options(bounds));
    assert_eq!(options.window_min_size, Some(size(px(760.), px(480.))));
}

// #424: with the dock at its tallest, a long reconnect notice still leaves rows under the headers.
#[gpui_kit::test]
async fn a_long_notice_leaves_rows_beside_a_tall_dock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, ROWS).await;
    t.dispatch(cx, "dock::ToggleDock", Value::Null).unwrap();
    t.dispatch(
        cx,
        "workspace::SetDockHeight",
        serde_json::json!({"height": 5000}),
    )
    .unwrap();
    resize(cx, &t, 760., 480.);
    let error = "connection refused by the API server, is it running ".repeat(16);
    let state = SyncState::Reconnecting { error: Some(error) };
    t.feed_list(cx, objects(KindId::pods(), ROWS, state, 2))
        .await;
    draw(cx, &t);

    let dock = bounds(cx, &t, "dock-pane");
    let banner = bounds(cx, &t, "list-banner");
    assert!(banner.bottom() <= dock.top(), "{banner:?} above {dock:?}");
    assert!(shown(cx, &t, "row-uid-api"), "a row is drawn");
    let row = bounds(cx, &t, "row-uid-api");
    assert!(row.top() >= banner.bottom(), "{row:?} under {banner:?}");
    assert!(row.bottom() <= dock.top(), "{row:?} above {dock:?}");
}

// #424: a long reason under the detail's header wraps instead of ending mid-word.
#[gpui_kit::test]
async fn a_long_caption_wraps_under_the_header(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    detail_alone(cx, &t).await;
    let detail = t.detail(cx).unwrap();
    detail.update(cx, |d, cx| {
        d.set_sessions_blocked(Some("the context changed and needs a look ".repeat(8)), cx);
    });
    draw(cx, &t);
    let height = bounds(cx, &t, "detail-captions").size.height;
    assert!(
        height > px(32.),
        "the caption takes several lines: {height:?}"
    );
}
