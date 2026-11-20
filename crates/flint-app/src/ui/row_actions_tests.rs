//! View tests for Logs, Previous Logs and Shell on pod and workload rows (UP-47): from a row's
//! context menu, from the keys and through the registry, over the whole window
//! (`support_tests`). The test servers never answer, so the tests hand the detail pane its
//! object and a workload's pods, and feed log tabs their lines.

use std::sync::Arc;

use flint_core::logs::LogBuffer;
use flint_core::{
    Cell, Column, ColumnRole, ColumnType, ConnectionState, KindId, KindSnapshot, LineSource,
    LogLine, LogSnapshot, LogState, NavEntry, ObjectDetail, PodSetStatus, Row, SafeErrorText,
    StreamStatus, SyncState, WorkloadPod,
};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, SharedString, TestAppContext};
use serde_json::{Value, json};

use crate::ui::dock::DockTab;
use crate::ui::list::{ListEvent, Selected};
use crate::ui::registry::{self, ActionError};
use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace, pods};

async fn activate(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &'static str) {
    dispatch(
        cx,
        t,
        "workspace::SelectCluster",
        json!({ "cluster": cluster }),
    )
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some(cluster)
    })
    .await;
}

fn dispatch(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    name: &str,
    args: Value,
) -> Result<(), ActionError> {
    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch(name, args, window, cx)
    })
    .unwrap()
}

fn draw(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.run_until_parked();
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

/// Opens a list of `entry`'s kind on the active cluster with `snapshot`, fed by hand, and waits
/// for `first`'s row.
async fn open_list(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    entry: NavEntry,
    snapshot: Arc<KindSnapshot>,
    first: &str,
) {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.open_kind(&entry, window, cx);
            let list = ws.list.clone().unwrap();
            list.update(cx, |list, cx| {
                list.stop_watching();
                list.set_snapshot(snapshot, cx);
            });
        })
    })
    .unwrap();
    let row = SharedString::from(format!("row-uid-{first}"));
    cx.wait_for(t.window, WAIT, move |window, _| {
        window.try_find(row.clone()).is_some()
    })
    .await;
}

fn pods_entry() -> NavEntry {
    NavEntry {
        id: KindId::pods(),
        label: "Pods".into(),
        kind: "Pod".into(),
        api_version: "v1".into(),
        namespaced: true,
        listable: true,
        watchable: true,
    }
}

fn deployments_entry() -> NavEntry {
    NavEntry {
        id: KindId::new("apps", "deployments"),
        label: "Deployments".into(),
        kind: "Deployment".into(),
        api_version: "apps/v1".into(),
        namespaced: true,
        listable: true,
        watchable: true,
    }
}

/// A Deployments list of `names` in `demo`, synced.
fn deployments(names: &[&str]) -> Arc<KindSnapshot> {
    let column = |name: &str, role| Column {
        name: name.into(),
        ty: ColumnType::String,
        format: "".into(),
        description: "".into(),
        priority: 0,
        role,
    };
    let rows = names
        .iter()
        .map(|name| {
            Arc::new(Row {
                uid: format!("uid-{name}").into(),
                name: (*name).into(),
                namespace: Some("demo".into()),
                resource_version: "1".into(),
                generation: None,
                created: Some(0),
                labels: Box::new([]),
                cells: Box::new([Cell::Name, Cell::Namespace]),
            })
        })
        .collect();
    Arc::new(KindSnapshot {
        cluster: "alpha".into(),
        kind: KindId::new("apps", "deployments"),
        namespaced: true,
        columns: vec![
            column("Name", ColumnRole::Name),
            column("Namespace", ColumnRole::Namespace),
        ]
        .into(),
        rows,
        state: SyncState::Synced,
        version: 1,
        ..Default::default()
    })
}

/// Pod `name` as fetched: containers `app` (restarted once, so it has previous logs) and
/// `sidecar`.
fn pod_detail(name: &str) -> ObjectDetail {
    detail(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": "demo", "uid": format!("uid-{name}")},
        "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]},
        "status": {"containerStatuses": [
            {"name": "app", "restartCount": 1, "lastState": {"terminated": {"exitCode": 1}}},
            {"name": "sidecar", "restartCount": 0}
        ]}
    }))
}

fn detail(object: Value) -> ObjectDetail {
    ObjectDetail {
        object,
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    }
}

/// Shows `object` in the detail pane, as if its fetch had returned.
fn show_fetched(cx: &mut TestAppContext, t: &TestWorkspace, object: ObjectDetail) {
    let detail = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_fetched(object, window, cx))
    })
    .unwrap();
    draw(cx, t);
}

/// Shows the detail pane's object as still loading, as on a slow server.
fn show_loading(cx: &mut TestAppContext, t: &TestWorkspace) {
    let detail = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    detail.update(cx, |d, cx| d.show_loading(cx));
    draw(cx, t);
}

/// Hands the detail pane a workload's pods, as if listed.
fn show_pods(cx: &mut TestAppContext, t: &TestWorkspace, pods: Vec<WorkloadPod>) {
    let detail = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    detail.update(cx, |d, cx| d.show_pods(Ok(pods.into()), cx));
    draw(cx, t);
}

fn workload_pod(name: &str, ready: bool, containers: &[&str]) -> WorkloadPod {
    WorkloadPod {
        name: name.into(),
        ready,
        containers: containers.iter().map(|c| (*c).to_owned()).collect(),
        default_container: containers.first().map(|c| (*c).to_owned()),
    }
}

/// Right-clicks the row of `name` and waits for its menu.
async fn right_click(cx: &mut TestAppContext, t: &TestWorkspace, name: &str) {
    let row = SharedString::from(format!("row-uid-{name}"));
    cx.update_window(t.window, |_, window, cx| window.right_click(row, cx))
        .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("popup-menu").is_some()
    })
    .await;
}

/// The open menu's entries' labels. (Whether one is disabled is not exposed; the tests click
/// it and check nothing opened.)
fn menu(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<String> {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        let menu = window.within("popup-menu");
        (0usize..)
            .map_while(|ix| menu.try_find(ix))
            .map(|item| item.label().unwrap_or_default().to_owned())
            .collect()
    })
    .unwrap()
}

/// Clicks the open menu's entry `ix`.
fn click_entry(cx: &mut TestAppContext, t: &TestWorkspace, ix: usize) {
    cx.update_window(t.window, |_, window, cx| {
        window.within("popup-menu").click(ix, cx)
    })
    .unwrap();
    draw(cx, t);
}

/// The dock: its tabs' titles, left to right, and the active one's position.
fn dock_tabs(cx: &mut TestAppContext, t: &TestWorkspace) -> (Vec<String>, Option<usize>) {
    t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        let ids = dock.tab_ids();
        let titles = ids
            .iter()
            .map(|id| dock.tab(*id).unwrap().item(cx).tab_title().to_string())
            .collect();
        let active = ids.iter().position(|id| Some(*id) == dock.active_id());
        (titles, active)
    })
}

fn press(cx: &mut TestAppContext, t: &TestWorkspace, key: &str) {
    cx.update_window(t.window, |_, window, cx| window.press(key, cx))
        .unwrap();
    cx.run_until_parked();
}

fn focus_list(cx: &mut TestAppContext, t: &TestWorkspace) {
    let focus = t.workspace.read_with(cx, |ws, cx| {
        ws.list.as_ref().unwrap().read(cx).focus_handle().clone()
    });
    cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    cx.run_until_parked();
}

// covers UP-47, UP-40, UP-41: a pod row's context menu selects the row and offers Logs,
// Previous Logs and Shell; chosen before the pod has loaded, an entry waits for it; once it
// has, Shell becomes a container picker. Repeating Logs shows the open tab.
#[gpui_kit::test]
async fn a_pod_rows_menu_opens_logs_previous_logs_and_a_shell(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;

    // No row selected: nothing to act on.
    assert_eq!(
        dispatch(cx, &t, "detail::OpenLogs", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenLogs".into()))
    );

    right_click(cx, &t, "web").await;
    let selected = t.workspace.read_with(cx, |ws, cx| {
        ws.list.as_ref().unwrap().read(cx).selected_name()
    });
    assert_eq!(selected.as_deref(), Some("web"), "a right-click selects");
    press(cx, &t, "escape");
    // The pod is still loading (on a slow server): the entries wait for it.
    show_loading(cx, &t);
    right_click(cx, &t, "web").await;
    assert_eq!(menu(cx, &t), ["Logs", "Previous Logs", "Shell"]);
    click_entry(cx, &t, 0);
    assert_eq!(
        dock_tabs(cx, &t).0,
        Vec::<String>::new(),
        "waits for the pod"
    );
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t), (vec!["web · app".to_owned()], Some(0)));

    // Loaded: Previous Logs applies (app restarted), Shell picks a container.
    right_click(cx, &t, "web").await;
    assert_eq!(menu(cx, &t), ["Logs", "Previous Logs", "Shell"]);
    click_entry(cx, &t, 1);
    assert_eq!(
        dock_tabs(cx, &t),
        (
            vec!["web · app".to_owned(), "web · app (previous)".to_owned()],
            Some(1)
        )
    );
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 2);
    let containers: Vec<String> = cx
        .update_window(t.window, |_, window, _| {
            let sub = window.within("submenu");
            (1usize..)
                .map_while(|ix| sub.try_find(ix))
                .map(|item| item.label().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap();
    assert_eq!(containers, ["app", "sidecar"]);
    cx.update_window(t.window, |_, window, cx| {
        window.within("submenu").click(2usize, cx)
    })
    .unwrap();
    draw(cx, &t);
    assert_eq!(dock_tabs(cx, &t).0[2], "$ web · sidecar");

    // Logs again (the menu, then Cmd-L from the list) shows the open tab, not a second one.
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 0);
    assert_eq!(dock_tabs(cx, &t).1, Some(0));
    assert_eq!(dock_tabs(cx, &t).0.len(), 3);
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-l");
    let (titles, active) = dock_tabs(cx, &t);
    assert_eq!((titles.len(), active), (3, Some(1)));
}

// covers UP-47, UP-40: keys and the registry act on the selected row too; an entry that does
// not apply is disabled.
#[gpui_kit::test]
async fn keys_act_on_the_selected_pod_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(
        cx,
        &t,
        pods_entry(),
        pods(&[("demo", "api", 0), ("demo", "web", 1)]),
        "api",
    )
    .await;
    let api = || {
        detail(json!({
            "kind": "Pod",
            "metadata": {"name": "api", "namespace": "demo"},
            "spec": {"containers": [{"name": "app"}]},
            "status": {"containerStatuses": [{"name": "app", "restartCount": 0}]}
        }))
    };
    focus_list(cx, &t);
    press(cx, &t, "down");
    show_loading(cx, &t);
    // It never restarted: Previous Logs in its menu does nothing.
    right_click(cx, &t, "api").await;
    show_fetched(cx, &t, api());
    assert_eq!(menu(cx, &t), ["Logs", "Previous Logs", "Shell"]);
    click_entry(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
    press(cx, &t, "escape");
    // Cmd-L before the pod has loaded opens its logs once it has.
    show_loading(cx, &t);
    focus_list(cx, &t);
    press(cx, &t, "secondary-l");
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
    show_fetched(cx, &t, api());
    assert_eq!(dock_tabs(cx, &t).0, ["api · app"]);
    assert_eq!(
        dispatch(cx, &t, "detail::OpenPreviousLogs", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenPreviousLogs".into()))
    );
    // Shift-Cmd-E opens a shell in the only container.
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-e");
    assert_eq!(dock_tabs(cx, &t).0, ["api · app", "$ api · app"]);
}

// covers UP-45, UP-47: Logs on a Deployment row opens one tab for every pod, titled with the
// kind and name; it shows each pod's lines prefixed and counts the streams. Asking again shows
// the same tab.
#[gpui_kit::test]
async fn a_workload_rows_logs_merge_its_pods(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    right_click(cx, &t, "web").await;
    assert_eq!(menu(cx, &t), ["Logs of All Pods", "Shell"]);
    click_entry(cx, &t, 0);
    assert_eq!(
        dock_tabs(cx, &t),
        (vec!["deployment/web".to_owned()], Some(0))
    );

    // The tab's session, fed by hand: two pods' lines, merged.
    let view = t
        .workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).active_log().unwrap().clone());
    let mut buffer = LogBuffer::new(100);
    for (pod, text) in [
        ("web-1", "GET /a"),
        ("web-2", "GET /b"),
        ("web-1", "GET /c"),
    ] {
        let mut line = LogLine::parse(&format!("2024-05-01T12:00:00Z {text}"), false);
        line.source = Some(Arc::new(LineSource::new(pod, "app")));
        buffer.push(line);
    }
    let stream = |pod: &str| StreamStatus {
        source: Arc::new(LineSource::new(pod, "app")),
        state: LogState::Streaming,
    };
    let status = PodSetStatus {
        selector: Some("app=web".into()),
        pods: 2,
        streams: vec![stream("web-1"), stream("web-2")],
        max_streams: 32,
        ..Default::default()
    };
    view.update(cx, |v, cx| {
        v.stop_streaming();
        v.set_snapshot(
            Arc::new(LogSnapshot {
                lines: buffer.view(),
                state: LogState::Streaming,
                paused: false,
                held_back: 0,
                version: 1,
                pods: Some(Arc::new(status)),
            }),
            cx,
        );
    });
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("log-streams").is_some() && window.try_find(("log-line", 2usize)).is_some()
    })
    .await;
    press(cx, &t, "secondary-a");
    press(cx, &t, "secondary-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(
        copied,
        "[web-1/app] GET /a\n[web-2/app] GET /b\n[web-1/app] GET /c"
    );

    // Logs again, from the menu or Cmd-L in the list: the same tab.
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 0);
    focus_list(cx, &t);
    press(cx, &t, "secondary-l");
    assert_eq!(
        dock_tabs(cx, &t),
        (vec!["deployment/web".to_owned()], Some(0))
    );
    // A workload has no previous logs of its own.
    assert_eq!(
        dispatch(cx, &t, "detail::OpenPreviousLogs", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenPreviousLogs".into()))
    );
}

// covers UP-47, UP-41: Shell on a workload opens in its first ready pod (by name), in the
// default container or the one picked; with no ready pod it is disabled and says why.
#[gpui_kit::test]
async fn a_workload_rows_shell_opens_in_a_ready_pod(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    dispatch(
        cx,
        &t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    draw(cx, &t);
    show_pods(cx, &t, vec![workload_pod("web-a", false, &["app"])]);
    assert_eq!(
        dispatch(cx, &t, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into())),
        "no pod is ready"
    );
    right_click(cx, &t, "web").await;
    assert_eq!(menu(cx, &t), ["Logs of All Pods", "Shell"]);
    click_entry(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new(), "disabled");
    press(cx, &t, "escape");

    // A menu opened while the pods are listed is rebuilt once they are: it names the pod.
    show_loading(cx, &t);
    right_click(cx, &t, "web").await;
    assert_eq!(menu(cx, &t), ["Logs of All Pods", "Shell"]);

    show_pods(
        cx,
        &t,
        vec![
            workload_pod("web-a", false, &["app"]),
            workload_pod("web-c", true, &["app"]),
            workload_pod("web-b", true, &["app"]),
        ],
    );
    assert_eq!(menu(cx, &t), ["Logs of All Pods", "Shell in web-b"]);
    click_entry(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).0, ["$ web-b · app"]);
    let request = t.workspace.read_with(cx, |ws, cx| {
        ws.dock
            .read(cx)
            .active_terminal()
            .unwrap()
            .read(cx)
            .request()
            .clone()
    });
    assert_eq!(
        (request.namespace.as_str(), request.pod.as_str()),
        ("demo", "web-b")
    );

    // Several containers: the picker, through the registry here.
    show_pods(cx, &t, vec![workload_pod("web-b", true, &["app", "proxy"])]);
    dispatch(
        cx,
        &t,
        "detail::OpenContainerShell",
        json!({"container": "proxy"}),
    )
    .unwrap();
    draw(cx, &t);
    assert_eq!(dock_tabs(cx, &t).0, ["$ web-b · app", "$ web-b · proxy"]);
    assert!(matches!(
        dispatch(
            cx,
            &t,
            "detail::OpenContainerShell",
            json!({"container": "nope"})
        ),
        Err(ActionError::Rejected { .. })
    ));
}

// covers UP-47, UP-45: the selector logs action opens one tab for the pods a label selector
// matches, and refuses an empty selector.
#[gpui_kit::test]
async fn selector_logs_open_from_the_palette(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    assert_eq!(
        dispatch(
            cx,
            &t,
            "logs::OpenSelectorLogs",
            json!({"namespace": "demo", "selector": "app=web"})
        ),
        Err(ActionError::NotAvailable("logs::OpenSelectorLogs".into())),
        "no active cluster"
    );
    activate(cx, &t, "alpha").await;
    assert!(matches!(
        dispatch(
            cx,
            &t,
            "logs::OpenSelectorLogs",
            json!({"namespace": "demo", "selector": " "})
        ),
        Err(ActionError::Rejected { .. })
    ));
    for _ in 0..2 {
        dispatch(
            cx,
            &t,
            "logs::OpenSelectorLogs",
            json!({"namespace": "demo", "selector": "app=web"}),
        )
        .unwrap();
        draw(cx, &t);
    }
    assert_eq!(dock_tabs(cx, &t), (vec!["-l app=web".to_owned()], Some(0)));
}

// covers UP-47: once the row's cluster is not connected, Logs and Shell are disabled and the
// workspace says why.
#[gpui_kit::test]
async fn a_cluster_that_is_not_connected_disables_the_actions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    dispatch(
        cx,
        &t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    draw(cx, &t);
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dispatch(cx, &t, "detail::OpenShell", Value::Null), Ok(()));
    t.model.update(cx, |model, cx| {
        for cluster in &mut model.clusters {
            cluster.state = ConnectionState::Failed(SafeErrorText::reduced("connection refused"));
        }
        cx.notify();
    });
    draw(cx, &t);
    for action in [
        "detail::OpenLogs",
        "detail::OpenPreviousLogs",
        "detail::OpenShell",
        "logs::OpenSelectorLogs",
    ] {
        let args = match action {
            "logs::OpenSelectorLogs" => json!({"namespace": "demo", "selector": "app=web"}),
            _ => Value::Null,
        };
        assert_eq!(
            dispatch(cx, &t, action, args),
            Err(ActionError::NotAvailable(action.into()))
        );
    }
    let reason = t
        .workspace
        .read_with(cx, |ws, cx| ws.sessions_blocked("alpha", cx));
    assert_eq!(
        reason.as_deref(),
        Some("alpha is not connected: connection refused")
    );
    assert_eq!(dock_tabs(cx, &t).0, ["$ web · app"]);
}

// covers UP-42, UP-47: a log tab opened from a row's menu frees everything once the tab closes
// and the row goes: the tab's view and session, and the menu (docs/TESTING.md, "Memory leaks").
#[gpui_kit::test]
async fn a_tab_opened_from_a_row_leaks_nothing_once_closed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    dispatch(
        cx,
        &t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    draw(cx, &t);
    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 0);
    let (id, view) = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        let id = dock.active_id().unwrap();
        let DockTab::Logs(view) = dock.tab(id).unwrap() else {
            panic!("a log tab")
        };
        (id, view.downgrade())
    });
    dispatch(cx, &t, "dock::CloseTab", json!({ "tab": id })).unwrap();
    draw(cx, &t);
    assert!(view.upgrade().is_none(), "closing the tab ends its session");
    // gpui-component keeps a trigger's last menu until the trigger is no longer drawn: the
    // Deployment is deleted. The detail pane then shows it as deleted, with what that draws
    // (a disabled Shell), until it is closed too.
    let list = t.workspace.read_with(cx, |ws, _| ws.list.clone().unwrap());
    list.update(cx, |list, cx| list.set_snapshot(deployments(&[]), cx));
    draw(cx, &t);
    dispatch(cx, &t, "resource_list::CloseDetail", Value::Null).unwrap();
    draw(cx, &t);
    draw(cx, &t);
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
}

/// Selects the row of `name` in `demo` and draws.
fn select_row(cx: &mut TestAppContext, t: &TestWorkspace, name: &str) {
    dispatch(
        cx,
        t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": name}),
    )
    .unwrap();
    draw(cx, t);
}

// covers UP-47: an action that comes right after moving to another row, before the window
// draws again (a quick key, an automation step), acts on the new row once it has loaded: not
// on the row left, and it is not lost.
#[gpui_kit::test]
async fn an_action_right_after_moving_rows_acts_on_the_new_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(
        cx,
        &t,
        pods_entry(),
        pods(&[("demo", "api", 0), ("demo", "web", 1)]),
        "api",
    )
    .await;
    select_row(cx, &t, "api");
    show_fetched(cx, &t, pod_detail("api"));

    // The test platform draws after every update, so select web and ask for its logs in one
    // update, as two events the app handles before its next frame. The pane left stays alive
    // meanwhile, as an entity may.
    let api = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    let workspace = t.workspace.clone();
    let list = t.workspace.read_with(cx, |ws, _| ws.list.clone().unwrap());
    cx.update_window(t.window, |_, window, cx| {
        let selected = Selected {
            uid: "uid-web".into(),
            name: "web".into(),
            namespace: Some("demo".into()),
            resource_version: "1".into(),
        };
        workspace.update(cx, |ws, cx| {
            ws.on_list_event(&list, &ListEvent::Selected(Some(selected)), window, cx);
            // Still loading, as on a slow server.
            let web = ws.detail.clone().unwrap();
            web.update(cx, |d, cx| d.show_loading(cx));
        });
        registry::dispatch("detail::OpenLogs", Value::Null, window, cx).unwrap();
    })
    .unwrap();
    drop(api);
    cx.run_until_parked();
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new(), "waits for web");
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t).0, ["web · app"]);
}

// covers UP-47, UP-41: once a workload changed, its pods are listed again, and Shell waits for
// that list instead of opening in a pod listed before, which may be gone or no longer ready.
#[gpui_kit::test]
async fn a_workload_shell_waits_for_its_pods_listed_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    show_pods(cx, &t, vec![workload_pod("web-a", true, &["app"])]);

    let detail = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    detail.update(cx, |d, cx| d.show_relisting(cx));
    draw(cx, &t);
    dispatch(cx, &t, "detail::OpenShell", Value::Null).unwrap();
    draw(cx, &t);
    assert_eq!(
        dock_tabs(cx, &t).0,
        Vec::<String>::new(),
        "waits for the list"
    );
    // web-a was replaced by web-b.
    show_pods(cx, &t, vec![workload_pod("web-b", true, &["app"])]);
    assert_eq!(dock_tabs(cx, &t).0, ["$ web-b · app"]);
}

// covers UP-47, UP-41: a deleted workload or pod has no shell: Shell is disabled and says why,
// though the pods listed before still show.
#[gpui_kit::test]
async fn a_deleted_object_has_no_shell(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    show_pods(cx, &t, vec![workload_pod("web-a", true, &["app"])]);
    assert_eq!(
        dispatch(cx, &t, "detail::OpenShell", Value::Null),
        Ok(()),
        "before"
    );
    let shells = dock_tabs(cx, &t).0.len();

    let list = t.workspace.read_with(cx, |ws, _| ws.list.clone().unwrap());
    list.update(cx, |list, cx| list.set_snapshot(deployments(&[]), cx));
    draw(cx, &t);
    assert_eq!(
        dispatch(cx, &t, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into()))
    );
    let reason = t.workspace.read_with(cx, |ws, cx| {
        ws.detail
            .as_ref()
            .unwrap()
            .read(cx)
            .shell_plan::<crate::ui::registry::OpenShell>(None)
    });
    assert!(
        matches!(&reason, Err(ActionError::Rejected { reason, .. }) if reason == "web was deleted"),
        "{reason:?}"
    );

    // The same for a pod.
    open_list(cx, &t, pods_entry(), pods(&[("demo", "api", 0)]), "api").await;
    select_row(cx, &t, "api");
    show_fetched(cx, &t, pod_detail("api"));
    let list = t.workspace.read_with(cx, |ws, _| ws.list.clone().unwrap());
    list.update(cx, |list, cx| list.set_snapshot(pods(&[]), cx));
    draw(cx, &t);
    assert_eq!(
        dispatch(cx, &t, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into()))
    );
    assert_eq!(dock_tabs(cx, &t).0.len(), shells);
}
