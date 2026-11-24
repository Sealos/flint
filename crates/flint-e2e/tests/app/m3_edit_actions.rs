//! M3 user paths the other app tests leave open (UP-51, UP-52, UP-53, UP-55, UP-92), read from the
//! fake API server's write recorder: the YAML editor against a refusing webhook and a competing
//! edit, the node actions, a drain a PodDisruptionBudget holds back, and how long an unlock lasts.
//!
//! Inline validation (UP-50) is not built yet (#31), so it has no test here.

use std::time::Duration;

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{
    FakeApiServer, FakeExec, FakePod, Fault, Gvr, Options, RequestRecord, Verb, WriteRecord,
};
use http::Method;
use serde_json::{Value, json};

use crate::support::*;

const PROTECTED: &str = "protect_local_clusters = true\n";
const EDITED: &str = "edit-me";
const EDITED_PATH: &str = "/api/v1/namespaces/demo/configmaps/edit-me";
const DENIED: &str = "this change is not allowed";
const NODE: &str = "drain-node";
/// A pod of the fixtures that takes a shell.
const POD: &str = "web-69c6f74b8b-qv4xv";
/// How many times the focus test opens the editor right after a dialog closes.
const EDITOR_OPENS: usize = 20;
/// Longer than a dialog's 250 ms exit animation. The tests type before and after it, so a focus
/// change tied to the animation's end would show.
const PAST_DIALOG_EXIT: Duration = Duration::from_millis(400);

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

fn succeeded(write: &WriteRecord) -> bool {
    write.status.is_some_and(|code| (200..300).contains(&code))
}

fn dry_runs(fake: &FakeApiServer) -> usize {
    fake.writes().iter().filter(|w| w.is_dry_run()).count()
}

fn sent_version(write: &WriteRecord) -> Result<String> {
    let body: Value = serde_json::from_slice(&write.body).context("a write's body")?;
    body["metadata"]["resourceVersion"]
        .as_str()
        .map(str::to_owned)
        .context("no resourceVersion in the write")
}

/// Whether the object was read between the refused write and the dry run of the new review, so
/// that review was made from the object as it is now.
fn read_between_conflict_and_review(fake: &FakeApiServer) -> bool {
    let requests = fake.requests();
    let put = |r: &RequestRecord| r.method == Method::PUT && r.path == EDITED_PATH;
    let refused = requests
        .iter()
        .position(|r| put(r) && r.status == Some(409));
    let reviewed = requests
        .iter()
        .rposition(|r| put(r) && r.parsed_query().dry_run);
    match (refused, reviewed) {
        (Some(refused), Some(reviewed)) if refused < reviewed => requests[refused..reviewed]
            .iter()
            .any(|r| r.method == Method::GET && r.path == EDITED_PATH && r.status == Some(200)),
        _ => false,
    }
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

/// Every write to `pod` or a subresource of it, a direct DELETE included.
fn pod_writes(fake: &FakeApiServer, pod: &str) -> Vec<WriteRecord> {
    let path = format!("/api/v1/namespaces/demo/pods/{pod}");
    let below = format!("{path}/");
    fake.writes()
        .into_iter()
        .filter(|w| w.path == path || w.path.starts_with(&below))
        .collect()
}

/// Waits for the finished drain's report and opens it: returns its title and text.
fn drain_report(app: &App) -> Result<(String, String)> {
    until(
        "the drain's report",
        || {
            app.dispatch("workspace::ShowDrainReport", Value::Null)
                .is_ok()
        },
        |shown| *shown,
    )?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    let field = |name: &str| {
        state["dialog"][name]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    Ok((field("title"), field("text")))
}

// covers UP-51 (#394): closing the pane over an unapplied edit asks. Keep editing leaves the edit
// in the editor, Discard drops it, and neither answer sends a write.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_51_unapplied_edit_asks_before_closing() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    seed_configmap(fake, "5000", json!({"a": "1"}));
    let app = launch_unlocked(&cluster)?;
    start_edit(&app)?;

    app.dispatch("detail::Close", Value::Null)?;
    app.wait_for("/dialog/title", Equals("Discard unapplied edit?".into()))?;
    app.dispatch("ui::Cancel", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    app.wait_for("/focus", Equals("detail_yaml".into()))?;

    app.dispatch("detail::Close", Value::Null)?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    app.wait_for("/detail", Equals(Value::Null))?;
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    app.quit()?;
    Ok(())
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
    assert_eq!(dry_runs(fake), 2, "{:?}", fake.writes());
    app.dispatch("detail::ApplyYamlEdit", Value::Null)?;
    app.wait_for("/dialog/title", Exists(true))?;
    assert!(real(fake).is_empty(), "{:?}", real(fake));
    dismiss(&app)?;
    assert!(real(fake).is_empty(), "{:?}", real(fake));
    assert_eq!(dry_runs(fake), 2, "Cancel sends no dry run");

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
    assert!(succeeded(&sent[1]), "{sent:?}");
    assert_eq!(stored(fake)?["immutable"], true);

    app.quit()?;
    assert_eq!(real(fake).len(), 2, "{:?}", real(fake));
    // The refused one, the one Cancel followed, and the new review: Cancel added none.
    assert_eq!(dry_runs(fake), 3, "{:?}", fake.writes());
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
    // The fake numbers versions itself, so the competing edit's is read back.
    let latest = object["metadata"]["resourceVersion"]
        .as_str()
        .context("no resourceVersion")?
        .to_owned();
    assert_ne!(
        sent_version(&sent[0])?,
        latest,
        "the refused write was stale"
    );

    app.dispatch("detail::ReloadYamlEdit", Value::Null)?;
    app.wait_for("/detail/edit", Equals("reviewed".into()))?;
    assert_eq!(real(fake).len(), 1, "a review writes nothing");
    // The diff's text is not in the app's state, so this checks what it is computed from: the
    // object read after the conflict, and a dry run at that object's version.
    assert!(
        read_between_conflict_and_review(fake),
        "{:?}",
        fake.requests()
    );
    let writes = fake.writes();
    let dry_run = writes
        .iter()
        .rev()
        .find(|w| w.is_dry_run())
        .context("no dry run")?;
    assert_eq!(sent_version(dry_run)?, latest);
    assert!(succeeded(dry_run), "{dry_run:?}");

    confirm_apply(&app)?;
    app.wait_for("/detail/edit", Equals(Value::Null))?;
    let sent = answered(fake, 2)?;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent_version(&sent[1])?, latest);
    assert!(succeeded(&sent[1]), "{sent:?}");
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

    // Confirmed: the node has no pods to evict, so the cordon is the drain's one write.
    let options = open_action(&app, "workspace::DrainNode")?;
    past_drain_options(&app, &options)?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    let (title, text) = drain_report(&app)?;
    assert_eq!(title, format!("Drained {NODE}"), "{text}");
    assert!(text.lines().any(|l| l == "Cordoned: yes."), "{text}");
    assert!(text.lines().any(|l| l == "Evicted 0 pods."), "{text}");
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    let writes = fake.writes();
    assert_eq!(writes.len(), before + 1, "{writes:?}");
    assert_eq!(writes[before].path, format!("/api/v1/nodes/{NODE}"));
    assert!(succeeded(&writes[before]), "{writes:?}");
    assert_eq!(unschedulable(), Some(true));

    app.quit()?;
    assert_eq!(fake.writes().len(), before + 1, "{:?}", fake.writes());
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
    let (title, text) = drain_report(&app)?;
    assert_eq!(title, format!("Drain of {NODE} left pods"), "{text}");
    // Whole lines: each pod is tied to its own outcome and reason.
    for line in [
        "Evicted 1 pod: demo/free",
        "Blocked, 1 pod (a PodDisruptionBudget does not allow evicting them now): demo/guarded",
        "Skipped (DaemonSet or mirror), 1 pod: demo/agent",
    ] {
        assert!(text.lines().any(|l| l == line), "no {line:?} in {text}");
    }
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;

    assert_eq!(
        fake.object(&nodes, None, NODE).context("no node")?["spec"]["unschedulable"],
        true
    );
    assert_eq!(evictions(fake, "guarded"), 1, "not retried");
    assert_eq!(pod_writes(fake, "guarded").len(), 1, "no other write");
    assert_eq!(evictions(fake, "free"), 1);
    let agent = pod_writes(fake, "agent");
    assert!(agent.is_empty(), "the DaemonSet pod is left: {agent:?}");
    let pods = Gvr::core("pods");
    assert!(fake.object(&pods, Some("demo"), "agent").is_some());
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
        let writes = until(
            "the write's answer",
            || fake.writes(),
            |writes| writes.len() > before && writes.iter().all(|w| w.status.is_some()),
        )?;
        assert_eq!(writes.len(), before + 1, "{name}: {writes:?}");
        assert_eq!(
            writes[before].path,
            format!("/apis/apps/v1/namespaces/demo/deployments/{name}")
        );
        assert!(succeeded(&writes[before]), "{name}: {writes:?}");
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

/// Shows [`POD`]'s YAML and opens a shell in it: the session and its dock tab, with focus there.
fn open_pod_shell(app: &App, fake: &FakeApiServer) -> Result<(FakeExec, u64)> {
    show_pod_yaml(app)?;
    app.dispatch("detail::OpenShell", Value::Null)?;
    let sessions = until(
        "an exec session",
        || fake.exec_sessions(),
        |s| !s.is_empty(),
    )?;
    let state = app.wait_for("/dock/tabs/0/status/state", Equals("running".into()))?;
    app.wait_for("/focus", Equals("shell".into()))?;
    let tab = state["dock"]["tabs"][0]["id"]
        .as_u64()
        .context("no dock tab id")?;
    Ok((sessions[0].clone(), tab))
}

fn show_pod_yaml(app: &App) -> Result<()> {
    open_live(app, "pods", 17)?;
    app.select_row(Some("demo"), POD)?;
    app.wait_for("/detail/name", Equals(POD.into()))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    app.dispatch("detail::ShowYaml", Value::Null)?;
    Ok(())
}

fn focus_tab(app: &App, tab: u64) -> Result<()> {
    app.dispatch("dock::ActivateTab", json!({"tab": tab}))?;
    app.wait_for("/focus", Equals("shell".into()))?;
    Ok(())
}

fn confirm_a_dialog(app: &App) -> Result<()> {
    app.dispatch("flint::About", Value::Null)?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    Ok(())
}

/// Types into the open editor at once and again after a dialog's exit: focus stays there and the
/// dock's session gets none of it.
fn assert_editor_keeps_keys(app: &App, exec: &FakeExec) -> Result<()> {
    let sent = exec.stdin().len();
    app.wait_for("/focus", Equals("detail_yaml".into()))?;
    // Comments, so the text stays valid however many times it is typed.
    app.type_text("# first\n")?;
    std::thread::sleep(PAST_DIALOG_EXIT);
    assert_eq!(app.state()?["focus"], "detail_yaml");
    app.type_text("# second\n")?;
    std::thread::sleep(PAST_DIALOG_EXIT);
    assert_eq!(app.state()?["focus"], "detail_yaml");
    assert_eq!(exec.stdin().len(), sent, "keys left the editor");
    Ok(())
}

// The editor opened right after a dialog closes keeps focus, every time.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn editor_keeps_focus_after_a_dialog_closes() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch_unlocked(&cluster)?;
    let (exec, tab) = open_pod_shell(&app, fake)?;
    for _ in 0..EDITOR_OPENS {
        focus_tab(&app, tab)?;
        confirm_a_dialog(&app)?;
        app.dispatch("detail::EditYaml", Value::Null)?;
        app.wait_for("/detail/edit", Equals("editing".into()))?;
        assert_editor_keeps_keys(&app, &exec)?;
        app.dispatch("detail::CancelYamlEdit", Value::Null)?;
        app.wait_for("/detail/edit", Equals(Value::Null))?;
    }
    Ok(())
}

// With nothing newly focused, closing a dialog gives focus back to the dock's tab.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn closing_a_dialog_returns_focus_to_the_dock() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch_unlocked(&cluster)?;
    let (exec, _) = open_pod_shell(&app, fake)?;
    confirm_a_dialog(&app)?;
    app.wait_for("/focus", Equals("shell".into()))?;
    std::thread::sleep(PAST_DIALOG_EXIT);
    assert_eq!(app.state()?["focus"], "shell");
    app.type_text("back")?;
    until(
        "the keys in the dock's tab",
        || exec.stdin(),
        |stdin| stdin.windows(4).any(|w| w == b"back"),
    )?;
    Ok(())
}

// Tab in the editor opened right after a dialog closes does not move focus to another pane.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn tab_stays_in_the_editor_after_a_dialog_closes() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch_unlocked(&cluster)?;
    let (exec, _) = open_pod_shell(&app, fake)?;
    confirm_a_dialog(&app)?;
    app.dispatch("detail::EditYaml", Value::Null)?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    app.wait_for("/focus", Equals("detail_yaml".into()))?;
    app.press_key("tab")?;
    assert_editor_keeps_keys(&app, &exec)?;
    Ok(())
}

// The same through the command palette, opened right after the dialog closes.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn editor_opened_from_the_palette_keeps_focus() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch_unlocked(&cluster)?;
    let (exec, _) = open_pod_shell(&app, fake)?;
    confirm_a_dialog(&app)?;
    app.press_key("secondary-shift-p")?;
    app.wait_for("/focus", Equals("palette".into()))?;
    app.type_text("edit yaml")?;
    app.wait_for("/palette/rows/0/action", Equals("detail::EditYaml".into()))?;
    app.press_key("enter")?;
    app.wait_for("/palette", Equals(Value::Null))?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    assert_editor_keeps_keys(&app, &exec)?;
    Ok(())
}

// Every way back to an editable editor gives it focus, wherever focus was.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn every_return_to_the_editor_takes_focus() -> Result<()> {
    let refused = || {
        Fault::admission_denied("policy.example.com", DENIED)
            .on(&Gvr::core("pods"))
            .verb(Verb::Other)
            .times(1)
    };
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch_unlocked(&cluster)?;
    let (exec, tab) = open_pod_shell(&app, fake)?;
    app.dispatch("detail::EditYaml", Value::Null)?;
    app.wait_for("/focus", Equals("detail_yaml".into()))?;
    app.press_key("secondary-down")?;
    app.type_text("immutable: true\n")?;

    // Back from the review.
    review(&app)?;
    focus_tab(&app, tab)?;
    app.dispatch("detail::BackToYamlEdit", Value::Null)?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    assert_editor_keeps_keys(&app, &exec)?;

    // The YAML tab shown again, with the edit kept.
    app.dispatch("detail::ShowOverview", Value::Null)?;
    // The hidden editor gives up focus as the next frame draws.
    app.wait_for("/focus", Equals("detail".into()))?;
    focus_tab(&app, tab)?;
    app.dispatch("detail::ShowYaml", Value::Null)?;
    assert_editor_keeps_keys(&app, &exec)?;

    // A review that is refused.
    focus_tab(&app, tab)?;
    fake.inject(refused());
    app.dispatch("detail::ReviewYamlEdit", Value::Null)?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    assert_editor_keeps_keys(&app, &exec)?;

    // An apply that is refused.
    review(&app)?;
    focus_tab(&app, tab)?;
    fake.inject(refused());
    confirm_apply(&app)?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    assert_editor_keeps_keys(&app, &exec)?;
    Ok(())
}

// A dock tab on one cluster keeps none of the keys typed into an editor on another.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn editor_on_another_cluster_keeps_focus() -> Result<()> {
    let clusters = TwoClusters::start()?;
    let app = App::launch(AppOptions::new(clusters.kubeconfig()).settings(PROTECTED))?;
    connect(&app, TwoClusters::A)?;
    unlock(&app, TwoClusters::A)?;
    let (exec, tab) = open_pod_shell(&app, &clusters.a)?;

    connect(&app, TwoClusters::B)?;
    show_pod_yaml(&app)?;
    focus_tab(&app, tab)?;
    // Unlocking B is the dialog that closes just before the edit.
    unlock(&app, TwoClusters::B)?;
    app.wait_for("/detail/edit_blocked", Equals(Value::Null))?;
    app.dispatch("detail::EditYaml", Value::Null)?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    assert_eq!(app.state()?["active_cluster"], TwoClusters::B);
    assert_editor_keeps_keys(&app, &exec)?;
    Ok(())
}
