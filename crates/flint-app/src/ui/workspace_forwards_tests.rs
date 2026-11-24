//! The port-forward actions in the workspace: Forward Port opens the panel on the selected pod,
//! is refused as a shell is, and a change of cluster stops what runs.

use flint_core::ObjectDetail;
use flint_core::access::Access;
use flint_core::port_forward::{FakeForward, ForwardRequest, ForwardTarget};
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use serde_json::{Value, json};

use crate::ui::closing::Quits;
use crate::ui::detail::access::Gated;
use crate::ui::port_forward_view::PortForwardsView;
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{
    TestWorkspace, WAIT, alpha_and_beta, open_workspace, open_workspace_with,
};
use crate::ui::view_state::DialogState;

async fn show_web(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    let detail = t.detail(cx).expect("the detail pane is open");
    let pod = ObjectDetail {
        object: json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web"},
            "spec": {"containers": [{"name": "app", "ports": [
                {"name": "http", "containerPort": 8080}
            ]}]},
            "status": {"phase": "Running"}
        }),
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    };
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_fetched(pod, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
}

fn panel(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<Entity<PortForwardsView>> {
    t.workspace.read_with(cx, |ws, cx| {
        ws.dock.read(cx).forward_views().next().cloned()
    })
}

fn request() -> ForwardRequest {
    ForwardRequest {
        cluster: "kind-alpha".into(),
        namespace: "demo".into(),
        target: ForwardTarget::Pod("web".into()),
        remote_port: 8080,
        local_port: None,
        server: None,
        uid: None,
    }
}

// covers UP-70: from the palette's action, the panel opens with the pod and its port.
#[gpui_kit::test]
async fn forward_port_opens_the_panel_on_the_selected_pod(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    t.dispatch(cx, "detail::ForwardPort", Value::Null).unwrap();
    let view = panel(cx, &t).expect("the panel opened");
    let (target, remote) = view.read_with(cx, |v, cx| (v.form_target(), v.remote_text(cx)));
    assert_eq!(target.as_deref(), Some("demo/pod/web"));
    assert_eq!(remote, "8080", "its declared port");
}

// covers UP-54: on a protected cluster nothing opens, and the action says why.
#[gpui_kit::test]
async fn forward_port_is_refused_on_a_protected_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "protect_local_clusters = true\n",
    )
    .unwrap();
    let t = open_workspace_with(cx, dir.path(), alpha_and_beta).await;
    show_web(cx, &t).await;
    let refused = t.dispatch(cx, "detail::ForwardPort", Value::Null);
    assert!(
        matches!(refused, Err(ActionError::NotAvailable(_))),
        "{refused:?}"
    );
    assert!(panel(cx, &t).is_none());
}

// RBAC's clear refusal disables it, as for a shell.
#[gpui_kit::test]
async fn forward_port_is_refused_when_rbac_forbids_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    let detail = t.detail(cx).unwrap();
    detail.update(cx, |d, cx| {
        d.set_access_for_tests(Gated::PortForward, Access::Forbidden, cx)
    });
    cx.run_until_parked();
    let why = detail.read_with(cx, |d, _| d.forbidden(Gated::PortForward));
    assert_eq!(
        why.as_deref(),
        Some("your access does not allow get on pods/portforward in demo")
    );
    assert!(t.dispatch(cx, "detail::ForwardPort", Value::Null).is_err());
    assert!(panel(cx, &t).is_none());
}

// The core decides which forwards a switch ends (`port_forward_keep` in flint-e2e): the window
// stops none itself, so one on a cluster kept watching goes on, and its row and address stay.
#[gpui_kit::test]
async fn changing_cluster_leaves_the_decision_to_the_core(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    t.dispatch(cx, "detail::ForwardPort", Value::Null).unwrap();
    let view = panel(cx, &t).unwrap();
    let (forward, fake) = FakeForward::new(request());
    fake.listen(18080);
    let id = view.update(cx, |v, cx| v.adopt_for_tests(forward, cx));
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |v, _| v.url(id)).as_deref(),
        Some("http://127.0.0.1:18080")
    );
    assert!(!fake.is_stopped());

    t.dispatch(
        cx,
        "workspace::SelectCluster",
        json!({"cluster": "kind-beta"}),
    )
    .unwrap();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).active.as_deref() == Some("kind-beta")
    })
    .await;
    cx.run_until_parked();
    assert!(!fake.is_stopped(), "not stopped by the window");
    assert!(view.read_with(cx, |v, _| v.has_row(id)), "the row stays");
    assert_eq!(
        view.read_with(cx, |v, _| v.url(id)).as_deref(),
        Some("http://127.0.0.1:18080")
    );

    t.dispatch(cx, "port_forward::StopPortForward", json!({"id": id}))
        .unwrap();
    assert!(!view.read_with(cx, |v, _| v.has_row(id)));
}

// A target selected outside a set does not start through the set's admitted connection.
#[gpui_kit::test]
async fn a_form_from_before_a_set_is_not_started_in_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    t.dispatch(cx, "detail::ForwardPort", Value::Null).unwrap();
    let view = panel(cx, &t).unwrap();
    assert!(view.read_with(cx, |v, _| v.form_target().is_some()));

    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    for cluster in ["kind-alpha", "kind-beta"] {
        t.dispatch(
            cx,
            "cluster_sets::TogglePickedCluster",
            json!({ "cluster": cluster }),
        )
        .unwrap();
    }
    t.dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "cluster-set-view").await;
    let shown = t
        .workspace
        .read_with(cx, |ws, _| ws.sets.member("kind-alpha").is_some());
    assert!(shown, "kind-alpha is a member now");
    let member = t
        .workspace
        .read_with(cx, |ws, cx| ws.session_member("kind-alpha", cx));
    assert!(matches!(member, Ok(Some(_))), "{member:?}");

    let window = t.window;
    let workspace = t.workspace.clone();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.start_port_forward(&crate::ui::registry::StartPortForward, window, cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
    let (rows, form) = view.read_with(cx, |v, _| (v.row_labels(), v.form_target()));
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(form, None, "it has to be selected again");
}

// Stopping one forward from its row leaves the others.
#[gpui_kit::test]
async fn stop_ends_one_forward(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    t.dispatch(cx, "detail::ForwardPort", Value::Null).unwrap();
    let view = panel(cx, &t).unwrap();
    let (first, first_fake) = FakeForward::new(request());
    let (second, second_fake) = FakeForward::new(request());
    first_fake.listen(18081);
    second_fake.listen(18082);
    let id = view.update(cx, |v, cx| v.adopt_for_tests(first, cx));
    view.update(cx, |v, cx| v.adopt_for_tests(second, cx));
    t.dispatch(cx, "port_forward::StopPortForward", json!({"id": id}))
        .unwrap();
    assert!(first_fake.is_stopped());
    assert!(!second_fake.is_stopped());
}

/// The panel open with `count` listening forwards, each with its fake.
fn adopt(cx: &mut TestAppContext, view: &Entity<PortForwardsView>, count: u16) -> Vec<FakeForward> {
    (0..count)
        .map(|n| {
            let (forward, fake) = FakeForward::new(request());
            fake.listen(18090 + n);
            view.update(cx, |v, cx| v.adopt_for_tests(forward, cx));
            fake
        })
        .collect()
}

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

fn quits(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0))
}

// covers UP-42 (#701): quitting with forwards running asks once, naming how many; Cancel keeps
// them running, confirming quits.
#[gpui_kit::test]
async fn quitting_with_forwards_running_asks_first(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    t.dispatch(cx, "detail::ForwardPort", Value::Null).unwrap();
    let view = panel(cx, &t).unwrap();
    let fakes = adopt(cx, &view, 2);

    t.press_key_of(cx, "flint::Quit");
    let asked = shown_dialog(cx, &t).await;
    assert_eq!(asked.title, "Quit Flint?");
    assert!(
        asked.text.contains("stops 2 running port forwards"),
        "{}",
        asked.text
    );
    assert_eq!(asked.buttons, ["Quit", "Cancel"]);
    assert_eq!(quits(cx), 0);

    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(quits(cx), 0, "Cancel keeps the app");
    assert!(fakes.iter().all(|f| !f.is_stopped()), "and the forwards");

    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    assert_eq!(quits(cx), 1);
}

// covers UP-42 (#701): once every forward has stopped, quitting asks nothing.
#[gpui_kit::test]
async fn quitting_with_no_forward_running_asks_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    t.dispatch(cx, "detail::ForwardPort", Value::Null).unwrap();
    let view = panel(cx, &t).unwrap();
    let (forward, fake) = FakeForward::new(request());
    fake.listen(18095);
    let id = view.update(cx, |v, cx| v.adopt_for_tests(forward, cx));
    t.dispatch(cx, "port_forward::StopPortForward", json!({"id": id}))
        .unwrap();
    cx.run_until_parked();
    assert!(fake.is_stopped());
    t.press_key_of(cx, "flint::Quit");
    assert_eq!(quits(cx), 1);
    assert!(!t.has_dialog(cx));
}

// covers UP-42 (#701): an unapplied edit and a running forward share one question.
#[gpui_kit::test]
async fn an_edit_and_a_forward_share_one_question(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    t.dispatch(cx, "detail::ForwardPort", Value::Null).unwrap();
    let view = panel(cx, &t).unwrap();
    let fakes = adopt(cx, &view, 1);
    let detail = t.detail(cx).unwrap();
    super::super::unapplied_tests::type_edit(cx, &t, &detail, "demo", "web");

    t.press_key_of(cx, "flint::Quit");
    let asked = shown_dialog(cx, &t).await;
    assert_eq!(asked.title, "Quit Flint?");
    assert!(
        asked.text.contains("stops 1 running port forward."),
        "{}",
        asked.text
    );
    assert!(
        asked.text.contains("1 unapplied YAML edit"),
        "{}",
        asked.text
    );

    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(quits(cx), 0);
    assert!(
        super::super::unapplied_tests::has_edit(cx, &detail),
        "the edit stays"
    );
    assert!(!fakes[0].is_stopped());

    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    assert_eq!(quits(cx), 1, "one answer quits");
}

// covers UP-42 (#701): Close Window asks the same, and Cancel keeps the window.
#[gpui_kit::test]
async fn closing_the_window_with_a_forward_running_asks_first(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_web(cx, &t).await;
    t.dispatch(cx, "detail::ForwardPort", Value::Null).unwrap();
    let view = panel(cx, &t).unwrap();
    adopt(cx, &view, 1);

    t.press_key_of(cx, "flint::CloseWindow");
    let asked = shown_dialog(cx, &t).await;
    assert_eq!(asked.title, "Close the window?");
    assert!(
        asked.text.contains("stops 1 running port forward"),
        "{}",
        asked.text
    );
    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    cx.run_until_parked();
    assert!(cx.update_window(t.window, |_, _, _| ()).is_ok());
}
