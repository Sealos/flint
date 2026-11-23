//! M3 user paths the other app tests leave open (UP-51, UP-52, UP-53, UP-55, UP-92), read from the
//! fake API server's write recorder: the YAML editor against a refusing webhook and a competing
//! edit, the node actions, a drain a PodDisruptionBudget holds back, and how long an unlock lasts.
//!
//! Inline validation (UP-50) is not built yet (#31), so it has no test here.

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{FakeApiServer, FakePod, Fault, Gvr, Options, Verb, WriteRecord};
use http::Method;
use serde_json::{Value, json};

use crate::support::*;

const PROTECTED: &str = "protect_local_clusters = true\n";
const EDITED: &str = "edit-me";
const EDITED_PATH: &str = "/api/v1/namespaces/demo/configmaps/edit-me";
const DENIED: &str = "this change is not allowed";
const NODE: &str = "drain-node";

fn launch_unlocked(cluster: &TestCluster) -> Result<App> {
    let app = App::launch(AppOptions::new(&cluster.kubeconfig).settings(PROTECTED))?;
    connect(&app, &cluster.context)?;
    unlock(&app, &cluster.context)?;
    Ok(app)
}

fn unlock(app: &App, context: &str) -> Result<()> {
    app.dispatch("workspace::UnlockCluster", json!({"cluster": context}))?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": context, "protection": "unlocked"})),
    )?;
    Ok(())
}

fn configmaps() -> Gvr {
    Gvr::core("configmaps")
}

fn seed_configmap(fake: &FakeApiServer, version: &str, data: Value) {
    fake.set_object(
        &configmaps(),
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": EDITED, "namespace": "demo", "uid": "uid-edit-me",
                         "resourceVersion": version},
            "data": data,
        }),
    );
}

fn stored(fake: &FakeApiServer) -> Result<Value> {
    fake.object(&configmaps(), Some("demo"), EDITED)
        .context("no edited object")
}

/// Opens the editor on the seeded ConfigMap and adds a top-level field.
fn start_edit(app: &App) -> Result<()> {
    app.open_view("configmaps")?;
    app.wait_for(
        "/list/rows",
        Any(json!({"namespace": "demo", "name": EDITED})),
    )?;
    app.select_row(Some("demo"), EDITED)?;
    app.wait_for("/detail/name", Equals(EDITED.into()))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    app.dispatch("detail::ShowYaml", Value::Null)?;
    app.wait_for("/detail/edit_blocked", Equals(Value::Null))?;
    app.dispatch("detail::EditYaml", Value::Null)?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    app.wait_for("/focus", Equals("detail_yaml".into()))?;
    app.press_key("secondary-down")?;
    app.type_text("immutable: true\n")?;
    Ok(())
}

fn review(app: &App) -> Result<()> {
    app.dispatch("detail::ReviewYamlEdit", Value::Null)?;
    app.wait_for("/detail/edit", Equals("reviewed".into()))?;
    Ok(())
}

fn confirm_apply(app: &App) -> Result<()> {
    app.dispatch("detail::ApplyYamlEdit", Value::Null)?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    Ok(())
}

fn denied() -> Fault {
    Fault::admission_denied("policy.example.com", DENIED)
        .on(&configmaps())
        .verb(Verb::Other)
        .times(1)
}

/// The writes that were not dry runs, with their answers.
fn real(fake: &FakeApiServer) -> Vec<WriteRecord> {
    fake.writes()
        .into_iter()
        .filter(|w| !w.is_dry_run())
        .collect()
}

fn statuses(writes: &[WriteRecord]) -> Vec<Option<u16>> {
    writes.iter().map(|w| w.status).collect()
}

/// Waits until `count` real writes were answered.
fn answered(fake: &FakeApiServer, count: usize) -> Result<Vec<WriteRecord>> {
    until(
        "the write's answer",
        || real(fake),
        |writes| writes.len() >= count && writes.iter().all(|w| w.status.is_some()),
    )
}

/// Adds a schedulable node with no pods on it.
fn add_node(fake: &FakeApiServer, unschedulable: bool) -> Result<Gvr> {
    let nodes = fake.gvr("nodes");
    let first = fake.rows(&nodes)[0].name().to_owned();
    let mut node = fake.object(&nodes, None, &first).context("no node")?;
    node["metadata"]["name"] = NODE.into();
    node["metadata"]["uid"] = "uid-drain-node".into();
    node["spec"]["unschedulable"] = unschedulable.into();
    fake.set_object(&nodes, node);
    Ok(nodes)
}

fn pod_on_node(name: &str, owner: &str) -> FakePod {
    FakePod::from_json(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name, "namespace": "demo", "uid": format!("uid-{name}"),
            "resourceVersion": "5002",
            "creationTimestamp": "2026-01-01T00:00:00Z",
            "labels": {},
            "ownerReferences": [{"apiVersion": "apps/v1", "kind": owner, "name": "owner",
                                 "uid": "uid-owner", "controller": true}],
        },
        "spec": {"nodeName": NODE, "containers": [{"name": "c", "image": "busybox:1.37.0"}]},
        "status": {"phase": "Running", "containerStatuses": [{
            "name": "c", "ready": true, "restartCount": 0, "image": "busybox:1.37.0",
            "imageID": "", "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}},
        }]},
    }))
}

fn select_node(app: &App) -> Result<()> {
    app.select_row(None, NODE)?;
    app.wait_for("/detail/name", Equals(NODE.into()))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    Ok(())
}

/// Waits until the registry offers `action`, then opens its dialog: returns the dialog's text.
fn open_action(app: &App, action: &str) -> Result<String> {
    until(
        action,
        || {
            app.actions().is_ok_and(|actions| {
                actions["actions"].as_array().is_some_and(|all| {
                    all.iter()
                        .any(|a| a["name"] == action && a["enabled"] == true)
                })
            })
        },
        |offered| *offered,
    )?;
    app.dispatch(action, Value::Null)?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    Ok(state["dialog"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// Confirms the drain's options and waits for the review's dialog that follows them.
fn past_drain_options(app: &App, options: &str) -> Result<String> {
    app.dispatch("ui::Confirm", Value::Null)?;
    let state = until(
        "the drain's review",
        || app.state().unwrap_or(Value::Null),
        |state| {
            state["dialog"]["text"]
                .as_str()
                .is_some_and(|text| text != options)
        },
    )?;
    Ok(state["dialog"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

fn dismiss(app: &App) -> Result<()> {
    app.press_key("escape")?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    Ok(())
}

fn evictions(fake: &FakeApiServer, pod: &str) -> usize {
    let path = format!("/api/v1/namespaces/demo/pods/{pod}/eviction");
    fake.writes()
        .iter()
        .filter(|w| w.method == Method::POST && w.path == path)
        .count()
}

// covers UP-51: a webhook refusing the dry run or the write is shown in the editor, keeps the
// edit, and is not retried; Cancel sends nothing, and the edit then applies once.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_51_webhook_refusals_keep_the_edit() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    seed_configmap(fake, "5000", json!({"a": "1"}));
    let app = launch_unlocked(&cluster)?;
    start_edit(&app)?;

    // Refused at the dry run: no review, and nothing real was sent.
    fake.inject(denied());
    app.dispatch("detail::ReviewYamlEdit", Value::Null)?;
    app.wait_for("/detail/edit_error", Contains(DENIED.into()))?;
    assert_eq!(app.state()?["detail"]["edit"], "editing");
    let writes = fake.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].is_dry_run());
    assert_eq!(writes[0].status, Some(403));

    // Reviewed, then cancelled at the confirmation: still nothing real.
    review(&app)?;
    app.dispatch("detail::ApplyYamlEdit", Value::Null)?;
    app.wait_for("/dialog/title", Exists(true))?;
    assert!(real(fake).is_empty(), "{:?}", real(fake));
    dismiss(&app)?;
    assert!(real(fake).is_empty(), "{:?}", real(fake));

    // Refused at the write: reported, sent once, and the object is as it was.
    fake.inject(denied());
    confirm_apply(&app)?;
    app.wait_for("/detail/edit_error", Contains(DENIED.into()))?;
    let sent = answered(fake, 1)?;
    assert_eq!(statuses(&sent), [Some(403)], "{sent:?}");
    assert_eq!(stored(fake)?["immutable"], Value::Null);

    // The edit is still there: a new review applies it, once.
    review(&app)?;
    confirm_apply(&app)?;
    app.wait_for("/detail/edit", Equals(Value::Null))?;
    let sent = answered(fake, 2)?;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1].path, EDITED_PATH);
    assert!(
        sent[1]
            .status
            .is_some_and(|code| (200..300).contains(&code))
    );
    assert_eq!(stored(fake)?["immutable"], true);

    app.quit()?;
    assert_eq!(real(fake).len(), 2, "{:?}", real(fake));
    Ok(())
}

// covers UP-52: a competing edit is refused with a reload offered and is not overwritten; the
// edit, reviewed again against the current object, needs its own confirmation and applies once.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_52_conflict_refused_on_the_fake() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    seed_configmap(fake, "5000", json!({"a": "1"}));
    let app = launch_unlocked(&cluster)?;
    start_edit(&app)?;
    review(&app)?;

    seed_configmap(fake, "6000", json!({"a": "1", "b": "2"}));
    confirm_apply(&app)?;
    app.wait_for("/detail/edit", Equals("conflict".into()))?;
    let sent = answered(fake, 1)?;
    assert_eq!(statuses(&sent), [Some(409)], "{sent:?}");
    let object = stored(fake)?;
    assert_eq!(object["immutable"], Value::Null, "nothing overwritten");
    assert_eq!(object["data"]["b"], "2");

    app.dispatch("detail::ReloadYamlEdit", Value::Null)?;
    app.wait_for("/detail/edit", Equals("reviewed".into()))?;
    assert_eq!(real(fake).len(), 1, "a review writes nothing");
    confirm_apply(&app)?;
    app.wait_for("/detail/edit", Equals(Value::Null))?;
    let sent = answered(fake, 2)?;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(stored(fake)?["immutable"], true);
    app.quit()?;
    Ok(())
}

// covers UP-53, UP-92: cordon, uncordon and drain name the node, send nothing when cancelled, and
// one write after Confirm.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_53_node_actions_confirm() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let nodes = add_node(fake, false)?;
    let app = launch_unlocked(&cluster)?;
    open_live(&app, "nodes", 2)?;
    select_node(&app)?;
    let unschedulable = || {
        fake.object(&nodes, None, NODE)
            .map(|node| node["spec"]["unschedulable"] == true)
    };

    for (action, after) in [
        ("workspace::CordonNode", true),
        ("workspace::UncordonNode", false),
    ] {
        let before = fake.writes().len();
        let text = open_action(&app, action)?;
        assert!(text.contains(NODE), "{action}: {text}");
        assert!(text.contains(&cluster.context), "{action}: {text}");
        dismiss(&app)?;
        assert_eq!(fake.writes().len(), before, "{action}: cancelled");

        open_action(&app, action)?;
        app.dispatch("ui::Confirm", Value::Null)?;
        app.wait_for("/dialog", Equals(Value::Null))?;
        let writes = until(
            "the write's answer",
            || fake.writes(),
            |writes| writes.len() > before && writes.iter().all(|w| w.status.is_some()),
        )?;
        assert_eq!(writes.len(), before + 1, "{action}: {writes:?}");
        assert_eq!(writes[before].path, format!("/api/v1/nodes/{NODE}"));
        assert_eq!(unschedulable(), Some(after), "{action}");
    }

    // Drain: cancelled at its options and at its review, nothing is sent.
    let before = fake.writes().len();
    let options = open_action(&app, "workspace::DrainNode")?;
    assert!(options.contains(NODE), "{options}");
    dismiss(&app)?;
    let options = open_action(&app, "workspace::DrainNode")?;
    let text = past_drain_options(&app, &options)?;
    assert!(text.contains(NODE), "{text}");
    assert!(text.contains(&cluster.context), "{text}");
    dismiss(&app)?;
    assert_eq!(fake.writes().len(), before, "a cancelled drain");
    assert_eq!(unschedulable(), Some(false));

    app.quit()?;
    assert_eq!(fake.writes().len(), before, "{:?}", fake.writes());
    Ok(())
}

// covers UP-53: a drain ends and reports the pod a PodDisruptionBudget holds back, after one try,
// and leaves DaemonSet pods alone.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_53_drain_reports_pdb() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let nodes = add_node(fake, false)?;
    fake.add_pod(pod_on_node("agent", "DaemonSet"));
    fake.add_pod(pod_on_node("guarded", "ReplicaSet"));
    fake.add_pod(pod_on_node("free", "ReplicaSet"));
    fake.inject(
        Fault::disruption_budget()
            .on(&Gvr::core("pods"))
            .named("guarded")
            .subresource("eviction"),
    );
    let app = launch_unlocked(&cluster)?;
    open_live(&app, "nodes", 2)?;
    select_node(&app)?;

    let options = open_action(&app, "workspace::DrainNode")?;
    past_drain_options(&app, &options)?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    until(
        "the drain's report",
        || {
            app.dispatch("workspace::ShowDrainReport", Value::Null)
                .is_ok()
        },
        |shown| *shown,
    )?;
    let state = app.wait_for("/dialog/title", Contains("left pods".into()))?;
    let text = state["dialog"]["text"].as_str().unwrap_or_default();
    for part in [
        "PodDisruptionBudget",
        "demo/guarded",
        "demo/free",
        "demo/agent",
    ] {
        assert!(text.contains(part), "no {part} in {text}");
    }
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;

    assert_eq!(
        fake.object(&nodes, None, NODE).context("no node")?["spec"]["unschedulable"],
        true
    );
    assert_eq!(evictions(fake, "guarded"), 1, "not retried");
    assert_eq!(evictions(fake, "free"), 1);
    assert_eq!(evictions(fake, "agent"), 0);
    let pods = Gvr::core("pods");
    assert!(fake.object(&pods, Some("demo"), "guarded").is_some());
    assert!(fake.object(&pods, Some("demo"), "free").is_none());
    app.quit()?;
    Ok(())
}

// covers UP-55: the local fake starts writable with nothing to confirm; with protection on, one
// unlock covers every later write, and it ends when the app quits.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_55_unlock_asks_once_and_lasts_until_quit() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let context = cluster.context.as_str();

    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, context)?;
    assert_eq!(sidebar(&app.state()?, context)["protection"], "local");
    app.quit()?;

    let app = launch_unlocked(&cluster)?;
    open_live(&app, "deployments", 5)?;
    for (name, action) in [
        ("web", "workspace::RestartObject"),
        ("podinfo", "workspace::RestartObject"),
    ] {
        let before = fake.writes().len();
        app.select_row(Some("demo"), name)?;
        app.wait_for("/detail/name", Equals(name.into()))?;
        app.wait_for("/detail/fetch", Equals("loaded".into()))?;
        // The dialog is the action's own: no second unlock is asked for.
        let text = open_action(&app, action)?;
        assert!(text.contains(name), "{text}");
        app.dispatch("ui::Confirm", Value::Null)?;
        app.wait_for("/dialog", Equals(Value::Null))?;
        until(
            "the write's answer",
            || fake.writes(),
            |writes| writes.len() > before && writes.iter().all(|w| w.status.is_some()),
        )?;
    }
    assert_eq!(sidebar(&app.state()?, context)["protection"], "unlocked");
    let scratch = app.quit()?;
    let sent = fake.writes().len();
    assert_eq!(sent, 2, "{:?}", fake.writes());

    let app = App::launch(AppOptions::new(&cluster.kubeconfig).reuse(scratch))?;
    connect(&app, context)?;
    assert_eq!(sidebar(&app.state()?, context)["protection"], "protected");
    open_live(&app, "deployments", 5)?;
    app.select_row(Some("demo"), "web")?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    assert!(
        app.dispatch("workspace::RestartObject", Value::Null)
            .is_err()
    );
    app.quit()?;
    assert_eq!(fake.writes().len(), sent, "nothing after the relaunch");
    Ok(())
}
