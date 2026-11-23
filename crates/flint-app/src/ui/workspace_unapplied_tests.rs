//! View tests for asking before an unapplied YAML edit is lost (#394).

use flint_core::{KindId, ObjectDetail};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use serde_json::{Value, json};

use super::unapplied::dialog;
use crate::ui::closing::Quits;
use crate::ui::detail::{DetailView, ObjectRef};
use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace};

/// The workspace with pods `web` and `api` listed, `web` open with an edit typed.
async fn editing(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
) -> (TestWorkspace, Entity<DetailView>) {
    let t = open_workspace(cx, dir).await;
    t.open_pods(cx, &[("demo", "web", 0), ("demo", "api", 0)])
        .await;
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    cx.run_until_parked();
    let detail = t.detail(cx).expect("web is open");
    type_edit(cx, &t, &detail, "demo", "web");
    (t, detail)
}

/// Loads `name` into the open pane and types an edit into its YAML.
pub(super) fn type_edit(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    detail: &Entity<DetailView>,
    namespace: &str,
    name: &str,
) {
    let object = ObjectDetail {
        object: json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": name, "namespace": namespace, "uid": format!("uid-{name}"),
                "resourceVersion": "1",
            },
        }),
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    };
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_fetched(object, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    t.dispatch(cx, "detail::EditYaml", Value::Null).unwrap();
    cx.run_until_parked();
    let (from, to) = (format!("name: {name}"), format!("name: {name} # typed"));
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| d.replace_in_edit(&from, &to, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
}

pub(super) fn has_edit(cx: &mut TestAppContext, detail: &Entity<DetailView>) -> bool {
    detail.read_with(cx, |d, cx| d.has_unapplied_edit(cx))
}

fn shown_name(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    t.detail(cx)
        .map(|d| d.read_with(cx, |d, _| d.object().name.clone()))
}

pub(super) async fn asked(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.wait_for(t.window, WAIT, |window, cx| {
        gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;
    let workspace = t.workspace.clone();
    let shown = cx
        .update_window(t.window, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.view_state(window, cx).dialog)
        })
        .unwrap()
        .expect("a dialog is open");
    assert_eq!(shown.title, "Discard unapplied edit?");
    assert_eq!(shown.buttons, ["Discard", "Keep editing"]);
}

pub(super) async fn answered(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.wait_for(t.window, WAIT, |window, cx| {
        !gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;
    cx.run_until_parked();
}

fn editor_focused(cx: &mut TestAppContext, t: &TestWorkspace, detail: &Entity<DetailView>) -> bool {
    let detail = detail.clone();
    cx.update_window(t.window, |_, window, cx| {
        detail.read(cx).editor_has_focus(window, cx)
    })
    .unwrap()
}

// #394: selecting another row asks. Keep editing leaves the edit, the row and focus; Discard
// moves to the row asked for.
#[gpui_kit::test]
async fn selecting_another_row_asks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;
    let select = json!({"namespace": "demo", "name": "api"});

    t.dispatch(cx, "resource_list::SelectRow", select.clone())
        .unwrap();
    asked(cx, &t).await;
    assert_eq!(shown_name(cx, &t).as_deref(), Some("web"));

    t.press(cx, "escape");
    answered(cx, &t).await;
    assert!(has_edit(cx, &detail), "Keep editing keeps the text");
    assert_eq!(shown_name(cx, &t).as_deref(), Some("web"));
    let list = t.list(cx).unwrap();
    assert_eq!(
        list.read_with(cx, |l, _| l.selected_name()).as_deref(),
        Some("web"),
        "the row stays selected"
    );
    assert!(
        editor_focused(cx, &t, &detail),
        "focus is back in the editor"
    );

    t.dispatch(cx, "resource_list::SelectRow", select).unwrap();
    asked(cx, &t).await;
    t.click(cx, "ok");
    answered(cx, &t).await;
    assert_eq!(shown_name(cx, &t).as_deref(), Some("api"));
}

// #394: closing the pane asks, by key and by the same action its button sends. Enter keeps.
#[gpui_kit::test]
async fn closing_the_pane_asks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;

    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    asked(cx, &t).await;
    t.press(cx, "enter");
    answered(cx, &t).await;
    assert!(has_edit(cx, &detail));
    assert!(t.detail(cx).is_some(), "Enter keeps editing");

    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    asked(cx, &t).await;
    t.click(cx, "ok");
    answered(cx, &t).await;
    assert!(t.detail(cx).is_none(), "Discard closes the pane");
}

// #394: another kind and another cluster ask before the pane goes.
#[gpui_kit::test]
async fn leaving_the_kind_or_cluster_asks(cx: &mut TestAppContext) {
    for (action, args) in [
        ("workspace::OpenView", json!({"kind": "deployments"})),
        ("workspace::SelectCluster", json!({"cluster": "kind-beta"})),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (t, detail) = editing(cx, dir.path()).await;
        t.dispatch(cx, action, args.clone()).unwrap();
        asked(cx, &t).await;
        t.press(cx, "escape");
        answered(cx, &t).await;
        assert!(has_edit(cx, &detail), "{action}");
        assert_eq!(shown_name(cx, &t).as_deref(), Some("web"), "{action}");
        let active = t.model.read_with(cx, |m, _| m.active.clone());
        assert_eq!(active.as_deref(), Some("kind-alpha"), "{action}");

        t.dispatch(cx, action, args).unwrap();
        asked(cx, &t).await;
        t.click(cx, "ok");
        answered(cx, &t).await;
        assert!(t.detail(cx).is_none(), "{action}: Discard leaves");
    }
}

// #394: a context re-pointed under an edit keeps the text and its binding to the old server, so
// the write gate refuses it. Nothing was asked: the person did not leave. Close still asks.
#[gpui_kit::test]
async fn a_re_pointed_context_keeps_the_edit_bound(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;
    let bound = detail.read_with(cx, |d, _| d.open_edit_server().map(str::to_owned));
    assert!(bound.is_some());
    t.model.update(cx, |model, cx| {
        let cluster = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        cluster.generation += 1;
        cluster.info.server = Some("https://10.0.0.9:6443".into());
        cx.notify();
    });
    cx.run_until_parked();

    assert!(!t.has_dialog(cx));
    assert_eq!(t.detail(cx).as_ref(), Some(&detail), "the same pane");
    assert!(has_edit(cx, &detail));
    let now = detail.read_with(cx, |d, _| d.open_edit_server().map(str::to_owned));
    assert_eq!(now, bound, "bound to the server it was opened on");

    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    asked(cx, &t).await;
    t.click(cx, "ok");
    answered(cx, &t).await;
    assert!(
        t.detail(cx).is_none(),
        "no row to clear, and the pane closes"
    );
}

// #394: Discard on closing the window goes on to close it.
#[gpui_kit::test]
async fn discarding_closes_the_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, _detail) = editing(cx, dir.path()).await;
    t.press_key_of(cx, "flint::CloseWindow");
    asked(cx, &t).await;
    t.click(cx, "ok");
    cx.run_until_parked();
    assert!(cx.update_window(t.window, |_, _, _| ()).is_err());
}

// #394: a namespace change keeps the pane, so nothing is asked and the edit stays.
#[gpui_kit::test]
async fn a_namespace_change_keeps_the_edit(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;
    t.dispatch(
        cx,
        "workspace::SetNamespaces",
        json!({"namespaces": ["other"]}),
    )
    .unwrap();
    cx.run_until_parked();
    assert!(!t.has_dialog(cx));
    assert!(has_edit(cx, &detail));
}

// #394: quitting and closing the window ask; Keep editing stays, Discard goes on.
#[gpui_kit::test]
async fn quitting_and_closing_the_window_ask(cx: &mut TestAppContext) {
    let quits =
        |cx: &mut TestAppContext| cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0));
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;

    t.press_key_of(cx, "flint::Quit");
    asked(cx, &t).await;
    t.press(cx, "escape");
    answered(cx, &t).await;
    assert_eq!(quits(cx), 0);
    assert!(has_edit(cx, &detail));

    assert!(!VisualTestContext::from_window(t.window, cx).simulate_close());
    cx.run_until_parked();
    asked(cx, &t).await;
    t.press(cx, "enter");
    answered(cx, &t).await;
    assert!(
        cx.update_window(t.window, |_, _, _| ()).is_ok(),
        "the window stays"
    );

    t.press_key_of(cx, "flint::Quit");
    asked(cx, &t).await;
    t.click(cx, "ok");
    answered(cx, &t).await;
    assert_eq!(quits(cx), 1, "Discard goes on to quit");
}

// #394: an edit with no change is not asked about.
#[gpui_kit::test]
async fn an_edit_typed_back_is_not_asked_about(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| {
            d.replace_in_edit("name: web # typed", "name: web", window, cx)
        })
    })
    .unwrap();
    assert!(!has_edit(cx, &detail));
    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    cx.run_until_parked();
    assert!(!t.has_dialog(cx));
    assert!(t.detail(cx).is_none());
}

fn quits(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0))
}

fn close_the_list(cx: &mut TestAppContext, t: &TestWorkspace) {
    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |ws, cx| ws.close_list(cx))
    })
    .unwrap();
    cx.run_until_parked();
}

fn move_endpoint(cx: &mut TestAppContext, t: &TestWorkspace, server: &str) {
    let server = server.to_owned();
    t.model.update(cx, |model, cx| {
        let cluster = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        cluster.generation += 1;
        cluster.endpoint += 1;
        cluster.confirmed = true;
        cluster.info.server = Some(server);
        cx.notify();
    });
    cx.run_until_parked();
}

// A list rebuilt under an edit (a re-point, then rediscovery) keeps the pane on show.
#[gpui_kit::test]
async fn a_rebuilt_list_keeps_the_edit_on_show(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;
    close_the_list(cx, &t);
    assert_eq!(t.detail(cx).as_ref(), Some(&detail), "the same pane");
    assert!(has_edit(cx, &detail));
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let drawn = cx
        .update_window(t.window, |_, window, _| {
            window.try_find("held-edit-pane").is_some()
        })
        .unwrap();
    assert!(drawn, "with no list, the edit is still drawn");
}

// A context moved to another server and put back has the old URL but another endpoint: the
// kept edit stays refused until it is opened again.
#[gpui_kit::test]
async fn a_context_moved_and_put_back_stays_refused(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;
    let server = detail
        .read_with(cx, |d, _| d.open_edit_server().map(str::to_owned))
        .expect("bound to a server");
    assert!(detail.read_with(cx, |d, _| d.can_review_edit()));
    move_endpoint(cx, &t, "https://10.0.0.9:6443");
    move_endpoint(cx, &t, &server);
    assert!(has_edit(cx, &detail), "the text is kept");
    assert!(
        !detail.read_with(cx, |d, _| d.can_review_edit()),
        "no review after A to B to A"
    );
    assert!(detail.read_with(cx, |d, _| d.edit_moved()));
}

// Discard goes on with what was asked for even if the pane changed under the question.
#[gpui_kit::test]
async fn discard_goes_on_to_quit_after_the_list_closed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, _detail) = editing(cx, dir.path()).await;
    t.press_key_of(cx, "flint::Quit");
    asked(cx, &t).await;
    close_the_list(cx, &t);
    t.click(cx, "ok");
    answered(cx, &t).await;
    assert_eq!(quits(cx), 1);
}

// Leaving during a confirmed write asks, and a rejection keeps the text. Discard cannot stall
// Quit: it drops the edit while the write runs on.
#[gpui_kit::test]
async fn an_edit_being_applied_is_asked_about(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;
    detail.update(cx, |d, cx| d.edit_applying(cx));
    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    asked(cx, &t).await;
    t.press(cx, "escape");
    answered(cx, &t).await;
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| {
            d.edit_applied(
                Err(flint_core::write::WriteError::Failed("rejected".into())),
                window,
                cx,
            )
        })
    })
    .unwrap();
    assert!(has_edit(cx, &detail), "the rejected text is still there");

    detail.update(cx, |d, cx| d.edit_applying(cx));
    t.press_key_of(cx, "flint::Quit");
    asked(cx, &t).await;
    t.click(cx, "ok");
    answered(cx, &t).await;
    assert_eq!(quits(cx), 1, "Quit is not held up by the running write");
}

// A right-click on another row asks about the edit and opens no menu over the question.
#[gpui_kit::test]
async fn a_right_click_on_another_row_opens_no_menu(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, _detail) = editing(cx, dir.path()).await;
    let list = t.workspace.read_with(cx, |ws, _| ws.list.clone().unwrap());
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    cx.update_window(t.window, |_, window, cx| {
        list.update(cx, |list, cx| list.press_row_menu("uid-api", window, cx))
    })
    .unwrap();
    asked(cx, &t).await;
    cx.run_until_parked();
    assert!(!list.read_with(cx, |list, _| list.has_open_row_menu()));
}

// A review that fails while the question is open: Keep editing returns to the editor.
#[gpui_kit::test]
async fn keep_editing_after_a_failed_review_focuses_the_editor(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing(cx, dir.path()).await;
    detail.update(cx, |d, cx| d.set_edit_reviewing(cx));
    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    asked(cx, &t).await;
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| {
            d.edit_applied(
                Err(flint_core::write::WriteError::Failed("refused".into())),
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();
    t.press(cx, "escape");
    answered(cx, &t).await;
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
    assert!(editor_focused(cx, &t, &detail));
}

// The question names the object and its cluster on one line each, and Enter cannot discard.
#[test]
fn the_question_names_the_edit() {
    let text = dialog(&ObjectRef {
        cluster: "kind-alpha".into(),
        kind: KindId::pods(),
        kind_label: "Pods".into(),
        namespace: Some("demo".into()),
        name: "web\nx".into(),
        uid: "u".into(),
    });
    assert!(
        text.text.contains("Pods demo/web\\u{a}x on kind-alpha"),
        "{}",
        text.text
    );
    assert_eq!(
        (text.ok.as_str(), text.cancel.as_str()),
        ("Discard", "Keep editing")
    );
    assert!(text.default_cancel);
}
