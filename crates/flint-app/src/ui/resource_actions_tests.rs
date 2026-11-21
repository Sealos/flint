//! View tests for the resource actions (UP-53, UP-54): the row menu per kind, a protected
//! cluster, the scale and drain dialogs, and a whole write against a small local API server
//! whose first write answers with a conflict.

use std::collections::VecDeque;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flint_core::{ConnectionState, Event, KindId, ObjectDetail, SyncState};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::row_actions_tests::{draw, menu, right_click, show_fetched};
use crate::ui::confirm;
use crate::ui::registry;
use crate::ui::resource_actions::{ResourceAction, WriteKey, claim, release, request, running_on};
use crate::ui::row_menu;
use crate::ui::support_tests::{
    CURRENT_GENERATION, TestWorkspace, WAIT, discovery, kubeconfig_yaml, objects,
    open_workspace_with, standard_sections,
};

const WRITES: [&str; 2] = ["Scale…", "Rollout Restart…"];

/// Connects `alpha` with a discovery in which every kind can be deleted.
async fn connect_deletable(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.connect_alpha(cx, standard_sections()).await;
    let mut found = discovery(standard_sections());
    for kind in &mut found.kinds {
        kind.verbs.push("delete".into());
    }
    t.feed(
        cx,
        Event::DiscoveryChanged {
            cluster: "alpha".into(),
            generation: CURRENT_GENERATION,
            discovery: Some(Arc::new(found)),
            error: None,
        },
    );
}

fn fetched(object: Value) -> ObjectDetail {
    ObjectDetail {
        object,
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    }
}

fn node(name: &str, cordoned: bool) -> ObjectDetail {
    fetched(json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {"name": name, "uid": format!("uid-{name}")},
        "spec": {"unschedulable": cordoned},
    }))
}

async fn open_nodes(cx: &mut TestAppContext, t: &TestWorkspace) {
    let nodes = objects(
        KindId::new("", "nodes"),
        &[("", "node-1", 0)],
        SyncState::Synced,
        1,
    );
    t.open_kind(cx, "nodes", nodes).await;
}

async fn open_deployments(cx: &mut TestAppContext, t: &TestWorkspace) {
    let deployments = objects(
        KindId::new("apps", "deployments"),
        &[("demo", "web", 0)],
        SyncState::Synced,
        1,
    );
    t.open_kind(cx, "deployments.apps", deployments).await;
}

async fn open_alpha(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    settings: &str,
) -> TestWorkspace {
    std::fs::write(dir.join("settings.toml"), settings).unwrap();
    open_workspace_with(cx, dir, crate::ui::support_tests::alpha_and_beta).await
}

// covers UP-53: a node's menu offers Cordon or Uncordon, whichever applies, Drain and Delete.
#[gpui_kit::test]
async fn a_nodes_menu_follows_whether_it_is_cordoned(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_alpha(cx, dir.path(), "").await;
    connect_deletable(cx, &t).await;
    open_nodes(cx, &t).await;
    right_click(cx, &t, "node-1").await;
    show_fetched(cx, &t, node("node-1", false));
    assert_eq!(menu(cx, &t), ["Cordon…", "Drain…", "Delete…"]);
    show_fetched(cx, &t, node("node-1", true));
    assert_eq!(menu(cx, &t), ["Uncordon…", "Drain…", "Delete…"]);
    assert!(
        t.dispatch(cx, "workspace::CordonNode", Value::Null)
            .is_err()
    );
    assert!(t.available(cx).contains(&"workspace::UncordonNode"));
}

// covers UP-53: a kind with no logs or shell gets only what it can do.
#[gpui_kit::test]
async fn a_deletable_kind_has_a_menu_with_delete(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_alpha(cx, dir.path(), "").await;
    connect_deletable(cx, &t).await;
    let services = objects(
        KindId::new("", "services"),
        &[("demo", "api", 0)],
        SyncState::Synced,
        1,
    );
    t.open_kind(cx, "services", services).await;
    right_click(cx, &t, "api").await;
    assert_eq!(menu(cx, &t), ["Delete…"]);
}

// covers UP-54: on a protected cluster each action is disabled, says why once, and Unlock is
// offered in place; the automation state describes the same entries.
#[gpui_kit::test]
async fn a_protected_clusters_actions_are_disabled_with_unlock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_alpha(cx, dir.path(), "protect_local_clusters = true\n").await;
    connect_deletable(cx, &t).await;
    open_deployments(cx, &t).await;
    right_click(cx, &t, "web").await;
    let rows = menu(cx, &t);
    let start = rows.iter().position(|r| r == "Scale…").expect("Scale");
    // The Unlock entry draws its own label, read below.
    assert_eq!(
        rows[start..rows.len() - 1],
        [
            WRITES[0],
            WRITES[1],
            "Delete…",
            crate::ui::detail::PROTECTED_NOTE,
        ]
    );
    let unlock = t.find(cx, "menu-unlock").expect("the Unlock entry");
    assert_eq!(unlock.label(), Some("Unlock alpha…"));
    assert_eq!(
        rows.iter()
            .filter(|r| *r == crate::ui::detail::PROTECTED_NOTE)
            .count(),
        1,
        "said once: {rows:?}"
    );
    let actions = t
        .workspace
        .read_with(cx, |ws, cx| row_menu::row_actions(ws, cx).unwrap());
    let (_, entries) = row_menu::menu_state(&actions);
    for label in ["Scale…", "Rollout Restart…", "Delete…"] {
        let entry = entries.iter().find(|e| e.label == label).expect(label);
        assert!(!entry.enabled, "{label}");
        assert_eq!(
            entry.reason.as_deref(),
            Some(crate::ui::detail::PROTECTED_NOTE)
        );
    }
    for action in ["DeleteObject", "ScaleObject", "RestartObject"] {
        assert!(
            t.dispatch(cx, &format!("workspace::{action}"), Value::Null)
                .is_err(),
            "{action} is refused"
        );
    }
    assert!(!t.has_dialog(cx));
}

// covers UP-53: the scale dialog keeps asking until the count is a non-negative whole number.
#[gpui_kit::test]
async fn the_scale_dialog_validates_the_count(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_alpha(cx, dir.path(), "").await;
    connect_deletable(cx, &t).await;
    open_deployments(cx, &t).await;
    right_click(cx, &t, "web").await;
    t.press(cx, "escape");
    show_fetched(
        cx,
        &t,
        fetched(json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web"},
            "spec": {"replicas": 3},
        })),
    );
    t.dispatch(cx, "workspace::ScaleObject", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "scale-input").await;
    let text = t.label(cx, "scale-description").unwrap();
    assert!(text.starts_with("Replicas now: 3."), "{text}");
    t.press(cx, "secondary-a");
    t.type_text(cx, "-2");
    t.click(cx, "ok");
    draw(cx, &t);
    assert_eq!(
        t.label(cx, "scale-error").as_deref(),
        Some("Enter a whole number of replicas, 0 or more.")
    );
    assert!(t.has_dialog(cx), "it stays open");
}

// covers UP-53: a drain's two options start unchecked and each says what is lost.
#[gpui_kit::test]
async fn the_drain_dialog_says_what_each_option_loses(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_alpha(cx, dir.path(), "").await;
    connect_deletable(cx, &t).await;
    open_nodes(cx, &t).await;
    right_click(cx, &t, "node-1").await;
    t.press(cx, "escape");
    show_fetched(cx, &t, node("node-1", false));
    t.dispatch(cx, "workspace::DrainNode", Value::Null).unwrap();
    t.wait_for_element(cx, "drain-emptydir").await;
    draw(cx, &t);
    for id in ["drain-emptydir", "drain-force"] {
        let checked = t.find(cx, id).expect(id).checked();
        assert_eq!(checked, Some(false), "{id} starts unchecked");
    }
    assert!(
        t.label(cx, "drain-emptydir-note")
            .unwrap()
            .contains("data is lost")
    );
    assert!(
        t.label(cx, "drain-force-note")
            .unwrap()
            .contains("Nothing recreates them")
    );
    t.click(cx, "drain-force");
    draw(cx, &t);
    assert_eq!(t.find(cx, "drain-force").unwrap().checked(), Some(true));
    assert_eq!(t.find(cx, "drain-emptydir").unwrap().checked(), Some(false));
}

// covers UP-53: a refusal from the core is said the way a refused action is.
#[gpui_kit::test]
async fn a_refused_review_says_why(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_alpha(cx, dir.path(), "").await;
    connect_deletable(cx, &t).await;
    open_deployments(cx, &t).await;
    right_click(cx, &t, "web").await;
    t.press(cx, "escape");
    t.dispatch(cx, "workspace::DeleteObject", Value::Null)
        .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).action_error().is_some()
    })
    .await;
    let why = t
        .workspace
        .read_with(cx, |ws, _| ws.action_error().map(str::to_owned))
        .unwrap();
    assert!(why.starts_with("Can't delete it: "), "{why}");
    assert!(!t.has_dialog(cx), "no confirmation without a review");
}

/// How the local API server answers.
#[derive(Default)]
struct Control {
    /// PATCH requests received.
    patches: AtomicUsize,
    /// The status of the next PATCH requests; 200 once empty.
    statuses: Mutex<VecDeque<u16>>,
    /// PATCH requests wait while this is set, so a write stays running.
    hold: AtomicBool,
    /// A drain's list of the node's pods fails.
    fail_pods: AtomicBool,
}

/// A small local API server: a Deployment `web` with 2 replicas and a Node `node-1`.
struct LocalApi {
    url: String,
    control: Arc<Control>,
}

impl LocalApi {
    fn start(statuses: &[u16]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let control = Arc::new(Control {
            statuses: Mutex::new(statuses.iter().copied().collect()),
            ..Default::default()
        });
        let shared = control.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                std::thread::spawn(move || serve(stream, &shared));
            }
        });
        LocalApi { url, control }
    }

    fn patches(&self) -> usize {
        self.control.patches.load(Ordering::SeqCst)
    }

    fn hold(&self, held: bool) {
        self.control.hold.store(held, Ordering::SeqCst);
    }
}

fn deployment() -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web", "resourceVersion": "7"},
        "spec": {"replicas": 2},
    })
}

/// Answers one connection's requests.
fn serve(stream: TcpStream, control: &Control) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut stream = stream;
    loop {
        let mut request = String::new();
        if reader.read_line(&mut request).unwrap_or(0) == 0 {
            return;
        }
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            if header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let mut parts = request.split_whitespace();
        let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if query.contains("watch=true") || query.contains("watch=1") {
            // A watch with nothing to say: held open until the client goes.
            let _ = reader.read_to_end(&mut Vec::new());
            return;
        }
        let (status, answer) = answer(method, path, query, control);
        let text = answer.to_string();
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            text.len()
        );
        if stream.write_all(head.as_bytes()).is_err() || stream.write_all(text.as_bytes()).is_err()
        {
            return;
        }
    }
}

fn answer(method: &str, path: &str, query: &str, control: &Control) -> (u16, Value) {
    let resource = |name: &str, kind: &str, namespaced: bool| {
        json!({"name": name, "singularName": kind.to_lowercase(), "namespaced": namespaced,
               "kind": kind, "verbs": ["get", "list", "watch", "patch", "delete"]})
    };
    const WEB: &str = "/apis/apps/v1/namespaces/demo/deployments/web";
    const WEB_SCALE: &str = "/apis/apps/v1/namespaces/demo/deployments/web/scale";
    const NODE: &str = "/api/v1/nodes/node-1";
    match (method, path) {
        ("GET", "/version") => (
            200,
            json!({"major": "1", "minor": "33", "gitVersion": "v1.33.1", "platform": "linux/arm64"}),
        ),
        ("GET", "/api") => (200, json!({"kind": "APIVersions", "versions": ["v1"]})),
        ("GET", "/apis") => (
            200,
            json!({"kind": "APIGroupList", "apiVersion": "v1", "groups": [{
                "name": "apps",
                "versions": [{"groupVersion": "apps/v1", "version": "v1"}],
                "preferredVersion": {"groupVersion": "apps/v1", "version": "v1"},
            }]}),
        ),
        ("GET", "/api/v1") => (
            200,
            json!({"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "v1",
                   "resources": [resource("pods", "Pod", true), resource("nodes", "Node", false)]}),
        ),
        ("GET", "/apis/apps/v1") => (
            200,
            json!({"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "apps/v1",
                   "resources": [resource("deployments", "Deployment", true)]}),
        ),
        ("GET", WEB) => (200, deployment()),
        ("GET", NODE) => (
            200,
            json!({"apiVersion": "v1", "kind": "Node",
                   "metadata": {"name": "node-1", "uid": "uid-node-1", "resourceVersion": "3"},
                   "spec": {}}),
        ),
        ("GET", "/api/v1/pods")
            if query.contains("fieldSelector") && control.fail_pods.load(Ordering::SeqCst) =>
        {
            (
                500,
                json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                       "reason": "InternalError", "code": 500, "message": "pods unavailable"}),
            )
        }
        ("PATCH", WEB | WEB_SCALE | NODE) => {
            control.patches.fetch_add(1, Ordering::SeqCst);
            while control.hold.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            let status = control.statuses.lock().unwrap().pop_front().unwrap_or(200);
            match status {
                200 if path == NODE => (
                    200,
                    json!({"apiVersion": "v1", "kind": "Node",
                           "metadata": {"name": "node-1", "uid": "uid-node-1"}, "spec": {}}),
                ),
                200 => (200, deployment()),
                code => (
                    code,
                    json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                           "reason": "Conflict", "code": code, "message": "it changed"}),
                ),
            }
        }
        ("GET", _) => (
            200,
            json!({"kind": "List", "apiVersion": "v1", "metadata": {"resourceVersion": "1"},
                   "items": []}),
        ),
        _ => (
            404,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                   "reason": "NotFound", "code": 404, "message": "not found"}),
        ),
    }
}

/// Waits until an open confirmation's text contains `wanted`, and returns it.
async fn wait_for_confirmation(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    wanted: &'static str,
) -> String {
    let matching = move |window: &gpui_kit::Window| {
        window
            .find_all("write-confirm-description")
            .iter()
            .filter_map(|e| e.label().map(str::to_owned))
            .find(|l| l.contains(wanted))
    };
    cx.wait_for(t.window, WAIT, move |window, _| matching(window).is_some())
        .await;
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| matching(window))
        .unwrap()
        .expect("the confirmation")
}

/// Waits until only one dialog's OK button is drawn: one that closed is gone.
async fn wait_for_one_ok(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.wait_for(t.window, WAIT, |window, _| window.find_all("ok").len() == 1)
        .await;
    draw(cx, t);
}

/// Waits until no dialog is open.
async fn wait_for_no_dialog(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.wait_for(t.window, WAIT, |window, cx| {
        !gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;
    draw(cx, t);
}

// covers UP-52, UP-53: a write the server answers with a conflict is not run again: a new
// review opens, saying the object changed, and only its confirmation writes.
#[gpui_kit::test]
async fn a_conflict_opens_a_new_review(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[409]);
    let url = api.url.clone();
    let t = open_workspace_with(cx, dir.path(), move |dir, _| {
        let path = dir.join("kubeconfig");
        std::fs::write(&path, kubeconfig_yaml(&[("alpha", &url)])).unwrap();
        vec![path]
    })
    .await;
    t.activate(cx, "alpha").await;
    t.wait_for_state(cx, "alpha", |s| matches!(s, ConnectionState::Connected(_)))
        .await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .discovered("alpha")
            .is_some_and(|d| d.discovery.is_some())
    })
    .await;
    open_deployments(cx, &t).await;
    right_click(cx, &t, "web").await;
    let detail = t.detail(cx).unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        detail.read(cx).replicas() == Some(2)
    })
    .await;

    // The menu's entry, clicked for real: the count field takes focus from the closing menu.
    let scale = menu(cx, &t)
        .iter()
        .position(|r| r == "Scale…")
        .expect("Scale");
    cx.update_window(t.window, |_, window, cx| {
        window.within("popup-menu").click(scale, cx)
    })
    .unwrap();
    t.wait_for_element(cx, "scale-input").await;
    draw(cx, &t);
    t.press(cx, "secondary-a");
    t.type_text(cx, "5");
    t.click(cx, "ok");
    let first = wait_for_confirmation(cx, &t, "Replicas: 2 to 5.").await;
    assert!(!first.contains("changed since"), "{first}");
    assert_eq!(api.patches(), 0, "nothing ran yet");

    wait_for_one_ok(cx, &t).await;
    t.click(cx, "ok");
    let again = wait_for_confirmation(cx, &t, "changed since the last review").await;
    assert!(again.contains("Replicas: 2 to 5."), "{again}");
    assert_eq!(api.patches(), 1, "the conflict is not retried");
    let shown = cx
        .update_window(t.window, |_, window, cx| confirm::shown(window, cx))
        .unwrap()
        .expect("the new review");
    assert_eq!(shown.title, "Scale web on alpha?");

    wait_for_one_ok(cx, &t).await;
    t.click(cx, "ok");
    let control = api.control.clone();
    cx.wait_for(t.window, WAIT, move |_, _| {
        control.patches.load(Ordering::SeqCst) == 2
    })
    .await;
    draw(cx, &t);
    assert!(!t.has_dialog(cx));
}

fn web_key() -> WriteKey {
    WriteKey::new(
        "alpha",
        &KindId::new("apps", "deployments"),
        Some("demo"),
        "web",
    )
}

fn node_key(name: &str) -> WriteKey {
    WriteKey::new("alpha", &KindId::new("", "nodes"), None, name)
}

/// Every resource action the selected row's menu has, with why it cannot run.
fn writes(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<(ResourceAction, Result<(), String>)> {
    t.workspace.read_with(cx, |ws, cx| {
        row_menu::row_actions(ws, cx)
            .unwrap()
            .writes
            .into_iter()
            .map(|w| (w.action, w.can))
            .collect()
    })
}

/// The workspace connected for real to `api`, its discovery in.
async fn connected(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    api: &LocalApi,
) -> TestWorkspace {
    let url = api.url.clone();
    let t = open_workspace_with(cx, dir, move |dir, _| {
        let path = dir.join("kubeconfig");
        std::fs::write(&path, kubeconfig_yaml(&[("alpha", &url)])).unwrap();
        vec![path]
    })
    .await;
    t.activate(cx, "alpha").await;
    t.wait_for_state(cx, "alpha", |s| matches!(s, ConnectionState::Connected(_)))
        .await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .discovered("alpha")
            .is_some_and(|d| d.discovery.is_some())
    })
    .await;
    t
}

/// Selects `web`, or `node-1`, and waits for its object.
async fn select_loaded(cx: &mut TestAppContext, t: &TestWorkspace, name: &str) {
    right_click(cx, t, name).await;
    t.press(cx, "escape");
    let detail = t.detail(cx).unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let d = detail.read(cx);
        d.replicas().is_some() || d.node_cordoned().is_ok()
    })
    .await;
    draw(cx, t);
}

/// Confirms the dialog on top, as Enter does.
fn confirm_top(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch("ui::Confirm", Value::Null, window, cx)
    })
    .unwrap()
    .unwrap();
    cx.run_until_parked();
}

async fn wait_running(cx: &mut TestAppContext, t: &TestWorkspace, key: WriteKey, running: bool) {
    cx.wait_for(t.window, WAIT, move |_, cx| {
        running_on(&key, cx).is_some() == running
    })
    .await;
    draw(cx, t);
}

// covers UP-53: while a write runs on an object, every action on it says so, in the menu, the
// guards and the handler; it is available again once the write ended.
#[gpui_kit::test]
async fn a_second_action_is_disabled_while_one_runs(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_alpha(cx, dir.path(), "").await;
    connect_deletable(cx, &t).await;
    open_deployments(cx, &t).await;
    right_click(cx, &t, "web").await;
    t.press(cx, "escape");
    cx.update(|cx| claim(&web_key(), ResourceAction::Scale, cx))
        .unwrap();
    draw(cx, &t);
    let shown = writes(cx, &t);
    assert_eq!(shown.len(), 3);
    for (action, can) in shown {
        assert_eq!(can, Err("web is being scaled".to_owned()), "{action:?}");
    }
    assert!(
        t.dispatch(cx, "workspace::DeleteObject", Value::Null)
            .is_err()
    );
    right_click(cx, &t, "web").await;
    assert!(
        menu(cx, &t).iter().any(|r| r == "web is being scaled"),
        "the menu says why"
    );
    t.press(cx, "escape");
    assert_eq!(
        cx.update(|cx| claim(&web_key(), ResourceAction::Delete, cx)),
        Err("web is being scaled".to_owned()),
        "a claim on a claimed object fails"
    );
    cx.update(|cx| release(&web_key(), cx));
    draw(cx, &t);
    assert!(writes(cx, &t).iter().all(|(_, can)| can.is_ok()));
}

// covers UP-53: during a drain nothing else runs on that node, and another node stays available.
#[gpui_kit::test]
async fn a_draining_node_blocks_every_action_on_it_only(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_alpha(cx, dir.path(), "").await;
    connect_deletable(cx, &t).await;
    let nodes = objects(
        KindId::new("", "nodes"),
        &[("", "node-1", 0), ("", "node-2", 0)],
        SyncState::Synced,
        1,
    );
    t.open_kind(cx, "nodes", nodes).await;
    cx.update(|cx| claim(&node_key("node-1"), ResourceAction::Drain, cx))
        .unwrap();
    right_click(cx, &t, "node-1").await;
    t.press(cx, "escape");
    show_fetched(cx, &t, node("node-1", true));
    for (action, can) in writes(cx, &t) {
        assert_eq!(can, Err("node-1 is being drained".to_owned()), "{action:?}");
    }
    right_click(cx, &t, "node-2").await;
    t.press(cx, "escape");
    show_fetched(cx, &t, node("node-2", false));
    assert!(writes(cx, &t).iter().all(|(_, can)| can.is_ok()));
    assert_eq!(
        t.label(cx, "drain-status").as_deref(),
        Some("Draining node-1…")
    );
}

// covers UP-53: two confirmations open for one object run one write: the second is refused at
// the moment it is confirmed, saying what runs.
#[gpui_kit::test]
async fn two_confirmations_run_one_write(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected(cx, dir.path(), &api).await;
    open_deployments(cx, &t).await;
    select_loaded(cx, &t, "web").await;
    api.hold(true);

    t.dispatch(cx, "workspace::RestartObject", Value::Null)
        .unwrap();
    wait_for_confirmation(cx, &t, "Replaces its pods").await;
    // A second review of the object, as another window would open: this window's open dialog
    // keeps its own keys and menus from asking.
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            request(ws, ResourceAction::RolloutRestart, window, cx)
        })
    })
    .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| {
        window.find_all("write-confirm-description").len() == 2
    })
    .await;
    draw(cx, &t);

    confirm_top(cx, &t);
    wait_running(cx, &t, web_key(), true).await;
    let control = api.control.clone();
    cx.wait_for(t.window, WAIT, move |_, _| {
        control.patches.load(Ordering::SeqCst) == 1
    })
    .await;
    wait_for_one_ok(cx, &t).await;
    t.click(cx, "ok");
    draw(cx, &t);
    let why = t
        .workspace
        .read_with(cx, |ws, _| ws.action_error().map(str::to_owned));
    assert_eq!(
        why.as_deref(),
        Some("Can't restart it: web is being restarted.")
    );
    assert_eq!(api.patches(), 1, "the second confirmation sent nothing");

    api.hold(false);
    wait_running(cx, &t, web_key(), false).await;
    assert_eq!(api.patches(), 1, "exactly one write");
}

// covers UP-53: a claim ends with its write, on success, on failure and on a conflict, and a
// conflict's new review can run.
#[gpui_kit::test]
async fn the_claim_is_released_however_a_write_ends(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[200, 500, 409]);
    let t = connected(cx, dir.path(), &api).await;
    open_deployments(cx, &t).await;
    select_loaded(cx, &t, "web").await;

    for (ix, status) in [200, 500, 409].into_iter().enumerate() {
        api.hold(true);
        t.dispatch(cx, "workspace::RestartObject", Value::Null)
            .unwrap();
        wait_for_confirmation(cx, &t, "Replaces its pods").await;
        confirm_top(cx, &t);
        wait_for_no_dialog(cx, &t).await;
        wait_running(cx, &t, web_key(), true).await;
        let control = api.control.clone();
        cx.wait_for(t.window, WAIT, move |_, _| {
            control.patches.load(Ordering::SeqCst) == ix + 1
        })
        .await;
        api.hold(false);
        wait_running(cx, &t, web_key(), false).await;
        assert!(
            writes(cx, &t).iter().all(|(_, can)| can.is_ok()),
            "after {status}"
        );
    }
    let again = wait_for_confirmation(cx, &t, "changed since the last review").await;
    assert!(again.contains("Replaces its pods"), "{again}");
    confirm_top(cx, &t);
    let control = api.control.clone();
    cx.wait_for(t.window, WAIT, move |_, _| {
        control.patches.load(Ordering::SeqCst) == 4
    })
    .await;
    wait_running(cx, &t, web_key(), false).await;
}

/// Starts a drain of `node-1` through its two dialogs.
async fn start_drain(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.dispatch(cx, "workspace::DrainNode", Value::Null).unwrap();
    t.wait_for_element(cx, "drain-emptydir").await;
    draw(cx, t);
    t.click(cx, "ok");
    wait_for_confirmation(cx, t, "DaemonSet and mirror pods stay").await;
    wait_for_one_ok(cx, t).await;
    t.click(cx, "ok");
    wait_for_no_dialog(cx, t).await;
    wait_running(cx, t, node_key("node-1"), true).await;
}

// covers UP-53: a running drain shows in a strip; its end replaces it with a button, no dialog
// opens by itself, and the button opens the report, for a finished and for a stopped drain.
#[gpui_kit::test]
async fn a_drain_shows_in_a_strip_then_offers_its_report(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected(cx, dir.path(), &api).await;
    open_nodes(cx, &t).await;
    select_loaded(cx, &t, "node-1").await;

    for (fails, ends, says) in [
        (false, "Drain of node-1 finished", "Drained node-1"),
        (true, "Drain of node-1 stopped", "Drain of node-1 stopped"),
    ] {
        api.control.fail_pods.store(fails, Ordering::SeqCst);
        api.hold(true);
        start_drain(cx, &t).await;
        t.wait_for_label(cx, "drain-status", "Draining node-1…")
            .await;
        api.hold(false);
        wait_running(cx, &t, node_key("node-1"), false).await;
        t.wait_for_label(cx, "drain-result", ends).await;
        assert!(
            t.find(cx, "drain-status").is_none(),
            "the strip is replaced"
        );
        assert!(!t.has_dialog(cx), "nothing pops up by itself");

        t.click(cx, "drain-view-result");
        t.wait_for_element(cx, "drain-report-description").await;
        draw(cx, &t);
        let shown = cx
            .update_window(t.window, |_, window, cx| confirm::shown(window, cx))
            .unwrap()
            .expect("the report");
        assert_eq!(shown.title, says);
        assert!(shown.text.contains("Cordoned: yes."), "{}", shown.text);
        assert_eq!(
            shown.text.contains("Stopped early:"),
            fails,
            "{}",
            shown.text
        );
        confirm_top(cx, &t);
        wait_for_no_dialog(cx, &t).await;
        assert!(t.find(cx, "drain-result").is_none(), "viewed");
    }
}
