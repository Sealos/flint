//! View tests for narrow windows: below the breakpoint the detail shows alone, with a Back control
//! that returns to the list and keeps the selection, and a confirmation stays fully usable at the
//! minimum window size.

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AppContext as _, Bounds, Context, ElementId, InteractiveElement as _, IntoElement, Pixels,
    Render, Styled as _, TestAppContext, VisualTestContext, Window, div, px, size,
};
use serde_json::Value;
use std::time::Duration;

use crate::ui::confirm::{self, DialogText};
use crate::ui::support_tests::{TestWorkspace, open_workspace, standard_sections};
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

// #433: a cluster name wider than the breadcrumb is cut and stays clear of the header's controls.
#[gpui_kit::test]
async fn a_cluster_name_too_long_to_fit_truncates_inside_the_header(cx: &mut TestAppContext) {
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
    assert!(cluster.size.width < whole, "the name is cut");
    let controls = bounds(cx, &t, "namespace-filter-button");
    assert!(
        cluster.right() <= controls.left(),
        "{cluster:?} runs into {controls:?}"
    );
}

// #421: the main window cannot be made smaller than the layout supports.
#[gpui_kit::test]
fn the_main_window_has_a_minimum_size(cx: &mut TestAppContext) {
    let bounds = Bounds::default();
    let options = cx.update(|_| crate::ui::window_options(bounds));
    assert_eq!(options.window_min_size, Some(size(px(760.), px(480.))));
}
