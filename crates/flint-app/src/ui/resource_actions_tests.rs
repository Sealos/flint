//! View tests for the resource actions (UP-53, UP-54): the row menu per kind, a protected
//! cluster, the scale and drain dialogs, and a whole write against a small local API server
//! whose first write answers with a conflict.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flint_core::{ConnectionState, Event, KindId, ObjectDetail, SyncState};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::row_actions_tests::{draw, menu, right_click, show_fetched};
use crate::ui::confirm;
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

/// A small local API server: a Deployment `web` with 2 replicas. Its first PATCH
/// answers with a conflict, as when someone else changed the object first.
struct LocalApi {
    url: String,
    patches: Arc<AtomicUsize>,
}

impl LocalApi {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let patches = Arc::new(AtomicUsize::new(0));
        let counted = patches.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let counted = counted.clone();
                std::thread::spawn(move || serve(stream, &counted));
            }
        });
        LocalApi { url, patches }
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
fn serve(stream: TcpStream, patches: &AtomicUsize) {
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
        let (status, answer) = answer(method, path, patches);
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

fn answer(method: &str, path: &str, patches: &AtomicUsize) -> (u16, Value) {
    let resource = |name: &str, kind: &str, namespaced: bool| {
        json!({"name": name, "singularName": kind.to_lowercase(), "namespaced": namespaced,
               "kind": kind, "verbs": ["get", "list", "watch", "patch", "delete"]})
    };
    const WEB: &str = "/apis/apps/v1/namespaces/demo/deployments/web";
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
        ("PATCH", WEB) => {
            if patches.fetch_add(1, Ordering::SeqCst) == 0 {
                (
                    409,
                    json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                           "reason": "Conflict", "code": 409, "message": "it changed"}),
                )
            } else {
                (200, deployment())
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

/// Waits until the open confirmation's text contains `wanted`, and returns it.
async fn wait_for_confirmation(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    wanted: &'static str,
) -> String {
    cx.wait_for(t.window, WAIT, move |window, _| {
        window
            .try_find("write-confirm-description")
            .and_then(|e| e.label().map(|l| l.contains(wanted)))
            == Some(true)
    })
    .await;
    draw(cx, t);
    t.label(cx, "write-confirm-description").unwrap()
}

// covers UP-52, UP-53: a write the server answers with a conflict is not run again: a new
// review opens, saying the object changed, and only its confirmation writes.
#[gpui_kit::test]
async fn a_conflict_opens_a_new_review(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start();
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
    assert_eq!(api.patches.load(Ordering::SeqCst), 0, "nothing ran yet");

    t.click(cx, "ok");
    let again = wait_for_confirmation(cx, &t, "changed since the last review").await;
    assert!(again.contains("Replicas: 2 to 5."), "{again}");
    assert_eq!(
        api.patches.load(Ordering::SeqCst),
        1,
        "the conflict is not retried"
    );
    let shown = cx
        .update_window(t.window, |_, window, cx| confirm::shown(window, cx))
        .unwrap()
        .expect("the new review");
    assert_eq!(shown.title, "Scale web on alpha?");

    t.click(cx, "ok");
    let patches = api.patches.clone();
    cx.wait_for(t.window, WAIT, move |_, _| {
        patches.load(Ordering::SeqCst) == 2
    })
    .await;
    draw(cx, &t);
    assert!(!t.has_dialog(cx));
}
