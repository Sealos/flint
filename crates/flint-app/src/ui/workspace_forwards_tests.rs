//! The port-forward actions in the workspace: Forward Port opens the panel on the selected pod,
//! is refused as a shell is, and a change of cluster stops what runs.

use flint_core::ObjectDetail;
use flint_core::access::Access;
use flint_core::port_forward::{FakeForward, ForwardRequest, ForwardTarget};
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use serde_json::{Value, json};

use crate::ui::detail::access::Gated;
use crate::ui::port_forward_view::PortForwardsView;
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{
    TestWorkspace, WAIT, alpha_and_beta, open_workspace, open_workspace_with,
};

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

// Switching the active cluster stops every forward; the row stays until dismissed.
#[gpui_kit::test]
async fn changing_cluster_stops_forwards(cx: &mut TestAppContext) {
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
        Some("http://localhost:18080")
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
    assert!(fake.is_stopped(), "stopped on the change of cluster");
    assert!(view.read_with(cx, |v, _| v.has_row(id)), "the row stays");

    t.dispatch(cx, "port_forward::StopPortForward", json!({"id": id}))
        .unwrap();
    assert!(!view.read_with(cx, |v, _| v.has_row(id)));
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
