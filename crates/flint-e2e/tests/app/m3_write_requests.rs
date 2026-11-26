//! What the write actions put on the wire (UP-51, UP-53, UP-54, UP-55, UP-92), read from the fake
//! API server: each action's exact request, one attempt when it fails, and nothing at all from a
//! cluster that is protected, locked or re-pointed while its confirmation is open.
//!
//! Inline validation (UP-50) is not built yet (#31), so it has no test here.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{FakeApiServer, Fault, Gvr, Options, Verb, WriteRecord};
use http::Method;
use serde_json::{Value, json};

use crate::support::*;

const MERGE_PATCH: &str = "application/merge-patch+json";
const JSON_PATCH: &str = "application/json-patch+json";
const JSON: &str = "application/json";
/// The query of a PATCH, whose URL Flint builds itself.
const FIELD_MANAGER: &str = "fieldManager=flint";
/// The query of a PUT built by the Kubernetes client, which starts it with `&`.
const CLIENT_FIELD_MANAGER: &str = "&fieldManager=flint";
const WEB: &str = "/apis/apps/v1/namespaces/demo/deployments/web";
const RESTARTED_AT: &str = "kubectl.kubernetes.io/restartedAt";
const NODE: &str = "requests-node";
/// A pod of the fixtures that takes a shell.
const POD: &str = "web-69c6f74b8b-qv4xv";
const EDITED: &str = "edit-me";
const EDITED_PATH: &str = "/api/v1/namespaces/demo/configmaps/edit-me";
/// Long enough for a repeated request to arrive once the action has ended.
const RETRY_WINDOW: Duration = Duration::from_secs(1);

/// Runs `steps`; on an error the teardown reports a dialog left open without hiding that error.
fn reporting(app: &App, steps: impl FnOnce() -> Result<()>) -> Result<()> {
    let result = steps();
    if result.is_err() {
        app.mark_failing();
    }
    result
}

/// Opens `action` and answers its dialogs up to, and including, the final Confirm.
fn run(app: &App, action: &str) -> Result<()> {
    let first = open_dialog(app, action)?;
    match action {
        "workspace::ScaleObject" => {
            app.press_key("secondary-a")?;
            app.type_text("5")?;
            app.dispatch("ui::Confirm", Value::Null)?;
            app.wait_for("/dialog/text", Contains("to 5.".into()))?;
        }
        "workspace::DrainNode" => {
            app.dispatch("ui::Confirm", Value::Null)?;
            until(
                "the drain's review",
                || app.state().unwrap_or(Value::Null),
                |state| {
                    state["dialog"]["text"]
                        .as_str()
                        .is_some_and(|text| text != first)
                },
            )?;
        }
        _ => {}
    }
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    Ok(())
}

fn body(write: &WriteRecord) -> Value {
    serde_json::from_slice(&write.body).unwrap_or(Value::Null)
}

fn uid(fake: &FakeApiServer, gvr: &Gvr, namespace: Option<&str>, name: &str) -> Result<String> {
    let object = fake
        .object(gvr, namespace, name)
        .with_context(|| format!("no {name}"))?;
    object["metadata"]["uid"]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("{name} has no uid"))
}

/// Asserts one write is exactly this request, and that it succeeded.
fn assert_sent(
    write: &WriteRecord,
    method: Method,
    path: &str,
    query: &str,
    content_type: &str,
    expected: &Value,
) {
    assert_eq!(write.method, method, "{write:?}");
    assert_eq!(write.path, path, "{write:?}");
    assert_eq!(write.query, query, "{write:?}");
    assert_eq!(write.content_type, content_type, "{write:?}");
    assert_eq!(&body(write), expected, "{write:?}");
    assert!(
        write.status.is_some_and(|code| (200..300).contains(&code)),
        "{write:?}"
    );
}

fn seed_configmap(fake: &FakeApiServer) {
    fake.set_object(
        &Gvr::core("configmaps"),
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": EDITED, "namespace": "demo", "uid": "uid-edit-me",
                         "resourceVersion": "5000"},
            "data": {"a": "1"},
        }),
    );
}

/// Opens the seeded ConfigMap's editor, adds a field, and reviews it.
fn edit_and_review(app: &App) -> Result<()> {
    app.open_view("configmaps")?;
    app.wait_for(
        "/list/rows",
        Any(json!({"namespace": "demo", "name": EDITED})),
    )?;
    select(app, Some("demo"), EDITED)?;
    app.dispatch("detail::ShowYaml", Value::Null)?;
    app.wait_for("/detail/edit_blocked", Equals(Value::Null))?;
    app.dispatch("detail::EditYaml", Value::Null)?;
    app.wait_for("/detail/edit", Equals("editing".into()))?;
    app.wait_for("/focus", Equals("detail_yaml".into()))?;
    app.press_key("secondary-down")?;
    app.type_text("immutable: true\n")?;
    app.dispatch("detail::ReviewYamlEdit", Value::Null)?;
    app.wait_for("/detail/edit", Equals("reviewed".into()))?;
    Ok(())
}

// covers UP-51, UP-53, UP-92: each action sends exactly one request, with this method, path,
// query, content type and body.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_53_each_action_sends_its_exact_request() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let deployments = fake.gvr("deployments");
    let web_uid = uid(fake, &deployments, Some("demo"), "web")?;
    let from = fake
        .object(&deployments, Some("demo"), "web")
        .context("no web")?["spec"]["replicas"]
        .as_u64()
        .context("no replicas")?;
    assert_ne!(from, 5, "the fixture must change");
    let nodes = add_node(fake, NODE, false)?;
    let node_uid = uid(fake, &nodes, None, NODE)?;
    fake.add_pod(pod_on_node("evicted", NODE, "ReplicaSet"));
    seed_configmap(fake);
    let app = launch_protected(&cluster)?;
    lift_protection(&app, &cluster.context, "UnlockCluster", "unlocked")?;

    // Rollout restart: a JSON Patch that tests the uid and the template's annotations as read,
    // or their absence by the metadata they would be in. The timestamp is the one value the test
    // cannot know.
    let metadata = fake
        .object(&deployments, Some("demo"), "web")
        .context("no web")?["spec"]["template"]["metadata"]
        .clone();
    open_live(&app, "deployments", 5)?;
    select(&app, Some("demo"), "web")?;
    run(&app, "workspace::RestartObject")?;
    let writes = answered(fake, 1)?;
    let mut sent = body(&writes[0]);
    let written = sent
        .pointer_mut("/2/value")
        .and_then(Value::as_object_mut)
        .context("no annotations written")?;
    let at = written
        .insert(RESTARTED_AT.to_owned(), "at".into())
        .context("no restartedAt")?;
    assert!(
        at.as_str()
            .is_some_and(|at| at.contains('T') && at.ends_with('Z')),
        "{at}"
    );
    let annotations = "/spec/template/metadata/annotations";
    let expected = match metadata.get("annotations") {
        Some(read) => {
            let mut stamped = read.clone();
            stamped[RESTARTED_AT] = "at".into();
            json!([
                {"op": "test", "path": "/metadata/uid", "value": web_uid},
                {"op": "test", "path": annotations, "value": read},
                {"op": "add", "path": annotations, "value": stamped},
            ])
        }
        None => json!([
            {"op": "test", "path": "/metadata/uid", "value": web_uid},
            {"op": "test", "path": "/spec/template/metadata", "value": metadata},
            {"op": "add", "path": annotations, "value": {RESTARTED_AT: "at"}},
        ]),
    };
    assert_eq!(sent, expected);
    assert_eq!(writes[0].method, Method::PATCH);
    assert_eq!(writes[0].path, WEB);
    assert_eq!(writes[0].query, FIELD_MANAGER);
    assert_eq!(writes[0].content_type, JSON_PATCH);

    // Scale: the JSON Patch tests the uid and the reviewed count.
    run(&app, "workspace::ScaleObject")?;
    let writes = answered(fake, 2)?;
    assert_sent(
        &writes[1],
        Method::PATCH,
        &format!("{WEB}/scale"),
        FIELD_MANAGER,
        JSON_PATCH,
        &json!([
            {"op": "test", "path": "/metadata/uid", "value": web_uid},
            {"op": "test", "path": "/spec/replicas", "value": from},
            {"op": "replace", "path": "/spec/replicas", "value": 5},
        ]),
    );

    // Delete: bound to the object's uid.
    run(&app, "workspace::DeleteObject")?;
    let writes = answered(fake, 3)?;
    assert_sent(
        &writes[2],
        Method::DELETE,
        WEB,
        "",
        JSON,
        &json!({"preconditions": {"uid": web_uid}}),
    );

    // Cordon and uncordon: a merge patch bound to the node's uid.
    let node_path = format!("/api/v1/nodes/{NODE}");
    open_live(&app, "nodes", 2)?;
    select(&app, None, NODE)?;
    for (index, action, cordoned) in [
        (3, "workspace::CordonNode", true),
        (4, "workspace::UncordonNode", false),
    ] {
        run(&app, action)?;
        let writes = answered(fake, index + 1)?;
        assert_sent(
            &writes[index],
            Method::PATCH,
            &node_path,
            FIELD_MANAGER,
            MERGE_PATCH,
            &json!({"metadata": {"uid": node_uid}, "spec": {"unschedulable": cordoned}}),
        );
    }

    // Drain: the cordon, then one eviction bound to the pod's uid, in that order.
    run(&app, "workspace::DrainNode")?;
    let writes = answered(fake, 7)?;
    assert_sent(
        &writes[5],
        Method::PATCH,
        &node_path,
        FIELD_MANAGER,
        MERGE_PATCH,
        &json!({"metadata": {"uid": node_uid}, "spec": {"unschedulable": true}}),
    );
    assert_sent(
        &writes[6],
        Method::POST,
        "/api/v1/namespaces/demo/pods/evicted/eviction",
        "",
        JSON,
        &json!({
            "metadata": {"name": "evicted"},
            "deleteOptions": {"preconditions": {"uid": "uid-evicted"}},
        }),
    );
    until(
        "the drain's report",
        || {
            app.dispatch("workspace::ShowDrainReport", Value::Null)
                .is_ok()
        },
        |shown| *shown,
    )?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;

    // Apply: a dry run, then the same object for real; its dialog names where it goes.
    edit_and_review(&app)?;
    let writes = answered(fake, 8)?;
    let reviewed = body(&writes[7]);
    assert_eq!(writes[7].method, Method::PUT);
    assert_eq!(writes[7].path, EDITED_PATH);
    assert_eq!(
        writes[7].query,
        format!("&dryRun=All{CLIENT_FIELD_MANAGER}")
    );
    assert_eq!(writes[7].content_type, JSON);
    assert_eq!(reviewed["immutable"], true, "{reviewed}");
    assert_eq!(reviewed["data"], json!({"a": "1"}), "{reviewed}");
    assert_eq!(reviewed["metadata"]["uid"], "uid-edit-me", "{reviewed}");
    app.dispatch("detail::ApplyYamlEdit", Value::Null)?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    let text = state["dialog"]["text"].as_str().unwrap_or_default();
    for part in [
        format!("Context: {}", cluster.context),
        cluster.server.clone(),
        "Namespace: demo".to_owned(),
        EDITED.to_owned(),
    ] {
        assert!(text.contains(&part), "no {part} in {text}");
    }
    assert_eq!(fake.writes().len(), 8, "nothing before Confirm");
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/detail/edit", Equals(Value::Null))?;
    let writes = answered(fake, 9)?;
    assert_sent(
        &writes[8],
        Method::PUT,
        EDITED_PATH,
        CLIENT_FIELD_MANAGER,
        JSON,
        &reviewed,
    );

    std::thread::sleep(RETRY_WINDOW);
    app.quit()?;
    assert_eq!(fake.writes().len(), 9, "{:?}", fake.writes());
    Ok(())
}

// covers UP-53: a write answered with a server error, or whose connection dropped, is sent once.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_53_a_failed_write_is_not_retried() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let deployments = fake.gvr("deployments");
    let original = fake
        .object(&deployments, Some("demo"), "web")
        .context("no web")?;
    let app = launch_protected(&cluster)?;
    lift_protection(&app, &cluster.context, "UnlockCluster", "unlocked")?;
    open_live(&app, "deployments", 5)?;
    select(&app, Some("demo"), "web")?;

    for (what, fault) in [
        ("503", Fault::status(503)),
        ("a dropped connection", Fault::drop_connection()),
    ] {
        for action in [
            "workspace::RestartObject",
            "workspace::ScaleObject",
            "workspace::DeleteObject",
        ] {
            let before = fake.writes().len();
            fake.inject(fault.clone().on(&deployments).verb(Verb::Other).times(1));
            run(&app, action)?;
            until(
                "the write's arrival",
                || fake.writes().len(),
                |count| *count > before,
            )?;
            // Offered again: the action has ended, so anything further would be a retry.
            wait_offered(&app, action)?;
            std::thread::sleep(RETRY_WINDOW);
            let writes = fake.writes();
            assert_eq!(writes.len(), before + 1, "{what}, {action}: {writes:?}");
            let stored = fake
                .object(&deployments, Some("demo"), "web")
                .context("web is gone")?;
            assert_eq!(stored["spec"], original["spec"], "{what}, {action}");
        }
    }
    app.quit()?;
    assert_eq!(fake.writes().len(), 6, "{:?}", fake.writes());
    Ok(())
}

// covers UP-54: locking a cluster while a confirmation is open refuses its Confirm: nothing is
// sent, then or after unlocking again.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_54_locking_during_a_confirmation_sends_nothing() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let context = cluster.context.as_str();
    seed_configmap(fake);
    let app = launch_protected(&cluster)?;
    reporting(&app, || {
        // Workspace actions are out of reach behind a dialog, so what protects the cluster here
        // is its trust leaving the settings file.
        lift_protection(&app, context, "TrustCluster", "trusted")?;
        let trusted = std::fs::read_to_string(app.config_dir().join("settings.toml"))?;

        // A delete's confirmation.
        open_live(&app, "deployments", 5)?;
        select(&app, Some("demo"), "web")?;
        open_dialog(&app, "workspace::DeleteObject")?;
        app.write_settings(PROTECTED)?;
        app.wait_for(
            "/clusters",
            Any(json!({"name": context, "protection": "protected"})),
        )?;
        app.dispatch("ui::Confirm", Value::Null)?;
        app.wait_for("/dialog", Equals(Value::Null))?;
        std::thread::sleep(RETRY_WINDOW);
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());

        // Trusted again, the refused confirmation does not run.
        app.write_settings(&trusted)?;
        app.wait_for(
            "/clusters",
            Any(json!({"name": context, "protection": "trusted"})),
        )?;
        std::thread::sleep(RETRY_WINDOW);
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
        assert!(
            fake.object(&fake.gvr("deployments"), Some("demo"), "web")
                .is_some()
        );

        // A reviewed edit's confirmation: its dry run is the only request.
        edit_and_review(&app)?;
        app.dispatch("detail::ApplyYamlEdit", Value::Null)?;
        app.wait_for("/dialog/title", Exists(true))?;
        app.write_settings(PROTECTED)?;
        app.wait_for(
            "/clusters",
            Any(json!({"name": context, "protection": "protected"})),
        )?;
        app.dispatch("ui::Confirm", Value::Null)?;
        app.wait_for("/dialog", Equals(Value::Null))?;
        std::thread::sleep(RETRY_WINDOW);
        Ok(())
    })?;
    app.quit()?;
    let writes = fake.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].is_dry_run(), "{writes:?}");
    let stored = fake
        .object(&Gvr::core("configmaps"), Some("demo"), EDITED)
        .context("no edited object")?;
    assert_eq!(stored["immutable"], Value::Null, "{stored}");
    Ok(())
}

// covers UP-55: re-pointing a context closes a confirmation open on it, says why, and leaves the
// keyboard working on what is behind it.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_55_repointing_during_a_confirmation_closes_it() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let other = FakeApiServer::start_blocking(Options::default())?;
    let app = launch_protected(&cluster)?;
    reporting(&app, || {
        lift_protection(&app, &cluster.context, "UnlockCluster", "unlocked")?;
        open_live(&app, "deployments", 5)?;
        select(&app, Some("demo"), "web")?;
        open_dialog(&app, "workspace::DeleteObject")?;

        app.edit_kubeconfig([&cluster.kubeconfig], || {
            other.write_kubeconfig(&cluster.kubeconfig, None)
        })?;
        app.wait_for(
            "/clusters",
            Any(json!({"name": cluster.context, "protection": "protected"})),
        )?;
        let state = app.wait_for("/dialog", Equals(Value::Null))?;
        assert_ne!(state["focus"], "none", "{}", state["focus"]);
        assert!(
            state["banners"].to_string().contains("connection changed"),
            "{}",
            state["banners"]
        );
        std::thread::sleep(RETRY_WINDOW);
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
        assert!(other.writes().is_empty(), "{:?}", other.writes());
        // Enter and Escape reach what is behind it, and answer nothing.
        app.press_key("enter")?;
        app.press_key("escape")?;
        assert!(app.state()?["dialog"].is_null());
        std::thread::sleep(RETRY_WINDOW);
        Ok(())
    })?;
    app.quit()?;
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert!(other.writes().is_empty(), "{:?}", other.writes());
    Ok(())
}

// covers UP-55: an edit reviewed before its cluster was disconnected writes nothing, whatever
// is answered once the cluster is selected again. A confirmation is modal, so the disconnect
// comes between the review and the confirmation.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_55_an_edit_reviewed_before_a_disconnect_writes_nothing() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    seed_configmap(fake);
    // Unprotected: the gate is open, so only the disconnect refuses.
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    // Escape keeps the edit if a question about losing it is open.
    let keep_editing = || -> Result<()> {
        if !app.state()?["dialog"].is_null() {
            app.press_key("escape")?;
            app.wait_for("/dialog", Equals(Value::Null))?;
        }
        Ok(())
    };
    reporting(&app, || {
        edit_and_review(&app)?;
        // Counted after the review's dry run.
        let reviewed = fake.writes().len();
        app.dispatch(
            "workspace::DisconnectCluster",
            json!({"cluster": cluster.context}),
        )?;
        app.wait_for("/active_cluster", Equals(Value::Null))?;
        keep_editing()?;
        connect(&app, &cluster.context)?;
        keep_editing()?;
        let _ = app.dispatch("detail::ApplyYamlEdit", Value::Null);
        for _ in 0..3 {
            if app.state()?["dialog"].is_null() {
                break;
            }
            let _ = app.click("ok");
            std::thread::sleep(RETRY_WINDOW);
        }
        std::thread::sleep(RETRY_WINDOW);
        assert_eq!(writes_since(fake, reviewed), Vec::<String>::new());
        Ok(())
    })?;
    app.quit()?;
    let stored = fake
        .object(&Gvr::core("configmaps"), Some("demo"), EDITED)
        .context("no edited object")?;
    assert_eq!(stored["immutable"], Value::Null, "{stored}");
    Ok(())
}

/// A dialog of a write action, as far as it is opened before the context moves.
#[derive(Clone, Copy, Debug)]
enum Asked {
    Delete,
    Restart,
    ScaleCount,
    ScaleReview,
    Cordon,
    Uncordon,
    DrainOptions,
    DrainReview,
    Apply,
}

const ASKED: [Asked; 9] = [
    Asked::Delete,
    Asked::Restart,
    Asked::ScaleCount,
    Asked::ScaleReview,
    Asked::Cordon,
    Asked::Uncordon,
    Asked::DrainOptions,
    Asked::DrainReview,
    Asked::Apply,
];

/// Gives `fake` every object the dialogs act on, the same on both servers.
fn seed_targets(fake: &FakeApiServer, asked: Asked) -> Result<()> {
    let nodes = add_node(fake, NODE, false)?;
    if matches!(asked, Asked::Uncordon) {
        let mut node = fake.object(&nodes, None, NODE).context("no node")?;
        node["spec"]["unschedulable"] = true.into();
        fake.set_object(&nodes, node);
    }
    fake.add_pod(pod_on_node("evicted", NODE, "ReplicaSet"));
    seed_configmap(fake);
    Ok(())
}

/// Opens `asked`'s dialog on its object.
fn open_asked(app: &App, asked: Asked) -> Result<()> {
    let (view, count, namespace, name) = match asked {
        Asked::Apply => return open_apply(app),
        Asked::Delete | Asked::Restart | Asked::ScaleCount | Asked::ScaleReview => {
            ("deployments", 5, Some("demo"), "web")
        }
        _ => ("nodes", 2, None, NODE),
    };
    open_live(app, view, count)?;
    select(app, namespace, name)?;
    match asked {
        Asked::Delete => open_dialog(app, "workspace::DeleteObject").map(drop),
        Asked::Restart => open_dialog(app, "workspace::RestartObject").map(drop),
        Asked::Cordon => open_dialog(app, "workspace::CordonNode").map(drop),
        Asked::Uncordon => open_dialog(app, "workspace::UncordonNode").map(drop),
        Asked::ScaleCount => open_dialog(app, "workspace::ScaleObject").map(drop),
        Asked::DrainOptions => open_dialog(app, "workspace::DrainNode").map(drop),
        Asked::ScaleReview => {
            open_dialog(app, "workspace::ScaleObject")?;
            app.press_key("secondary-a")?;
            app.type_text("5")?;
            app.dispatch("ui::Confirm", Value::Null)?;
            app.wait_for("/dialog/text", Contains("to 5.".into()))?;
            Ok(())
        }
        Asked::DrainReview => {
            let first = open_dialog(app, "workspace::DrainNode")?;
            app.dispatch("ui::Confirm", Value::Null)?;
            until(
                "the drain's review",
                || app.state().unwrap_or(Value::Null),
                |state| {
                    state["dialog"]["text"]
                        .as_str()
                        .is_some_and(|text| text != first)
                },
            )?;
            Ok(())
        }
        Asked::Apply => unreachable!("returned above"),
    }
}

fn open_apply(app: &App) -> Result<()> {
    edit_and_review(app)?;
    app.dispatch("detail::ApplyYamlEdit", Value::Null)?;
    app.wait_for("/dialog/title", Exists(true))?;
    Ok(())
}

/// Where each server's arrival log stood at the moment a context moved.
#[derive(Clone, Copy)]
struct Moved {
    /// Requests the server it left had received. Anything later came after the move.
    from: usize,
    /// Requests the server it reached had received.
    to: usize,
}

/// Points the context from `from` at `to`, and waits until the app has reached it. The marks are
/// taken immediately before the file changes, once access reviews have settled, so a request
/// that arrives after the move can never count as earlier.
fn repoint(
    app: &App,
    kubeconfig: &std::path::Path,
    from: &FakeApiServer,
    to: &FakeApiServer,
) -> Result<Moved> {
    repoint_as(app, kubeconfig, from, to, &to.kubeconfig(None))
}

/// [`repoint`], writing `config` for `to`: the same server under other credentials, say.
fn repoint_as(
    app: &App,
    kubeconfig: &std::path::Path,
    from: &FakeApiServer,
    to: &FakeApiServer,
    config: &kube::config::Kubeconfig,
) -> Result<Moved> {
    let mut last = (access_reviews(from, 0, None).len(), Instant::now());
    until(
        "the access reviews to settle",
        || {
            let count = access_reviews(from, 0, None).len();
            if count != last.0 {
                last = (count, Instant::now());
            }
            last.1.elapsed()
        },
        |still| *still >= RETRY_WINDOW,
    )?;
    let moved = app.edit_kubeconfig([kubeconfig], || {
        let moved = Moved {
            from: from.arrivals().len(),
            to: to.arrivals().len(),
        };
        flint_e2e::fake_api::kubeconfig::write(kubeconfig, config)?;
        Ok(moved)
    })?;
    until(
        "a request to the new server",
        || to.arrivals().len(),
        |count| *count > moved.to,
    )?;
    std::thread::sleep(RETRY_WINDOW);
    Ok(moved)
}

/// Answers the open dialog from the keyboard: Enter, or the action Enter is bound to when nothing
/// in the dialog holds focus.
fn confirm_by_key(app: &App) -> Result<()> {
    let shown = app.state()?["dialog"].clone();
    app.press_key("enter")?;
    if app.state()?["dialog"] == shown {
        let _ = app.dispatch("ui::Confirm", Value::Null);
    }
    Ok(())
}

/// Every write after the first `from`, with what the server answered.
fn writes_since(fake: &FakeApiServer, from: usize) -> Vec<String> {
    fake.writes()[from..]
        .iter()
        .map(|w| format!("{} {}?{} -> {:?}", w.method, w.path, w.query, w.status))
        .collect()
}

/// The permission checks among `fake`'s arrivals from `from` up to `to` (the end if `None`). The
/// fake does not log them as writes.
fn access_reviews(fake: &FakeApiServer, from: usize, to: Option<usize>) -> Vec<String> {
    let arrivals = fake.arrivals();
    arrivals[from..to.unwrap_or(arrivals.len())]
        .iter()
        .filter(|r| r.method == Method::POST && r.path.ends_with("/selfsubjectaccessreviews"))
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

// covers UP-55: whatever answers a write's dialog after its context moved, by mouse or by key,
// neither the new server nor the one it was reviewed on receives a write.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_55_answering_a_confirmation_after_a_repoint_writes_nowhere() -> Result<()> {
    let mut reached = Vec::new();
    for asked in ASKED {
        for back in [false, true] {
            for key in [false, true] {
                let cluster = TestCluster::fake(Options::default())?;
                let fake = cluster.fake_server().expect("a fake server");
                let other = FakeApiServer::start_blocking(Options::default())?;
                seed_targets(fake, asked)?;
                seed_targets(&other, asked)?;
                // Unprotected: the gate is open for both servers, so only the binding refuses.
                let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
                connect(&app, &cluster.context)?;
                let mut left_open = false;
                reporting(&app, || {
                    open_asked(&app, asked)?;
                    let (on_fake, on_other) = (fake.writes().len(), other.writes().len());
                    let moved = repoint(&app, &cluster.kubeconfig, fake, &other)?;
                    if back {
                        repoint(&app, &cluster.kubeconfig, &other, fake)?;
                    }
                    if !app.state()?["dialog"].is_null() {
                        reached.push(format!("{asked:?} back={back}: the dialog stayed open"));
                    }
                    // Answered if it is still there. A first step may lead to a review: that is
                    // answered too.
                    for answer in 0..3 {
                        if app.state()?["dialog"].is_null() {
                            break;
                        }
                        // Enter cancels a drain's review: only its button confirms it.
                        let by_button = answer > 0 && matches!(asked, Asked::DrainOptions);
                        if key && !by_button {
                            confirm_by_key(&app)?;
                        } else {
                            let _ = app.click("ok");
                        }
                        std::thread::sleep(RETRY_WINDOW);
                    }
                    let mut first = writes_since(fake, on_fake);
                    first.extend(access_reviews(fake, moved.from, None));
                    let mut second = writes_since(&other, on_other);
                    second.extend(access_reviews(&other, 0, None));
                    if !first.is_empty() || !second.is_empty() {
                        reached.push(format!(
                            "{asked:?} back={back} key={key}: original {first:?} new {second:?}"
                        ));
                    }
                    left_open = !app.state()?["dialog"].is_null();
                    Ok(())
                })?;
                if left_open {
                    app.mark_failing();
                    drop(app);
                } else {
                    app.quit()?;
                }
            }
        }
    }
    assert!(reached.is_empty(), "{reached:#?}");
    Ok(())
}

/// `fake`'s kubeconfig with a user of its own: the same server under another credential.
fn with_token(fake: &FakeApiServer, token: &str) -> Result<kube::config::Kubeconfig> {
    let mut config = serde_json::to_value(fake.kubeconfig(None))?;
    config["users"][0]["user"] = json!({ "token": token });
    Ok(serde_json::from_value(config)?)
}

// covers UP-55: a context moved away and back to the same server under another user closes the
// dialog open on it, and whatever answers it after, nothing reaches either server.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_55_a_move_back_under_another_user_writes_nowhere() -> Result<()> {
    let mut reached = Vec::new();
    for asked in ASKED {
        let cluster = TestCluster::fake(Options::default())?;
        let fake = cluster.fake_server().expect("a fake server");
        let other = FakeApiServer::start_blocking(Options::default())?;
        seed_targets(fake, asked)?;
        seed_targets(&other, asked)?;
        // Unprotected: the gate is open for both servers, so only the binding refuses.
        let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
        connect(&app, &cluster.context)?;
        let mut left_open = false;
        reporting(&app, || {
            open_asked(&app, asked)?;
            let (on_fake, on_other) = (fake.writes().len(), other.writes().len());
            let moved = repoint(&app, &cluster.kubeconfig, fake, &other)?;
            let back = with_token(fake, "another-user")?;
            repoint_as(&app, &cluster.kubeconfig, &other, fake, &back)?;
            if !app.state()?["dialog"].is_null() {
                reached.push(format!("{asked:?}: the dialog stayed open"));
            }
            for _ in 0..3 {
                if app.state()?["dialog"].is_null() {
                    break;
                }
                let _ = app.click("ok");
                std::thread::sleep(RETRY_WINDOW);
            }
            let mut first = writes_since(fake, on_fake);
            first.extend(access_reviews(fake, moved.from, None));
            let mut second = writes_since(&other, on_other);
            second.extend(access_reviews(&other, 0, None));
            if !first.is_empty() || !second.is_empty() {
                reached.push(format!("{asked:?}: original {first:?} new {second:?}"));
            }
            left_open = !app.state()?["dialog"].is_null();
            Ok(())
        })?;
        if left_open {
            app.mark_failing();
            drop(app);
        } else {
            app.quit()?;
        }
    }
    assert!(reached.is_empty(), "{reached:#?}");
    Ok(())
}

// covers UP-54: on a protected cluster every refused action adds no request but the exact
// read-only list and watch GETs of the feeds already open.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_54_refused_actions_send_no_request() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    add_node(fake, NODE, false)?;
    let app = launch_protected(&cluster)?;
    reporting(&app, || {
        open_live(&app, "nodes", 2)?;
        select(&app, None, NODE)?;
        assert_refused_silently(
            &app,
            fake,
            &[("/api/v1/nodes", None)],
            &["workspace::CordonNode", "workspace::DrainNode"],
        )?;
        open_live(&app, "deployments", 5)?;
        select(&app, Some("demo"), "web")?;
        assert_refused_silently(
            &app,
            fake,
            &[
                ("/apis/apps/v1/deployments", None),
                // The selected Deployment's pods, which its detail pane follows.
                (
                    "/api/v1/namespaces/demo/pods",
                    Some("labelSelector=app%3Dweb"),
                ),
            ],
            &[
                "workspace::ScaleObject",
                "workspace::RestartObject",
                "workspace::DeleteObject",
                "detail::EditYaml",
            ],
        )?;
        open_live(&app, "pods", 17)?;
        select(&app, Some("demo"), POD)?;
        assert_refused_silently(
            &app,
            fake,
            &[("/api/v1/pods", None)],
            &["detail::OpenShell"],
        )?;
        Ok(())
    })?;
    app.quit()?;
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert!(
        fake.exec_requests().is_empty(),
        "{:?}",
        fake.exec_requests()
    );
    Ok(())
}

/// Dispatches each action, asserts it is refused, and that only list and watch GETs of exactly
/// `feeds` arrived meanwhile: each a path, with the one label selector it must carry, if any. The
/// cluster feed's timed list GETs ([`is_feed_poll`]) may land in the window too.
fn assert_refused_silently(
    app: &App,
    fake: &FakeApiServer,
    feeds: &[(&str, Option<&str>)],
    actions: &[&str],
) -> Result<()> {
    let before = fake.arrivals().len();
    for action in actions {
        assert!(app.dispatch(action, Value::Null).is_err(), "{action} ran");
    }
    std::thread::sleep(RETRY_WINDOW);
    let arrivals = fake.arrivals();
    let other: Vec<_> = arrivals[before..]
        .iter()
        .filter(|r| {
            let selector = r.query.split('&').find(|p| p.starts_with("labelSelector="));
            let is_feed = feeds
                .iter()
                .any(|(path, wanted)| r.path == *path && selector == *wanted);
            let read = r.method == Method::GET && matches!(r.verb, Verb::List | Verb::Watch);
            !((read && is_feed) || is_feed_poll(r))
        })
        .map(ToString::to_string)
        .collect();
    assert!(other.is_empty(), "{actions:?} sent {other:?}");
    Ok(())
}

// covers UP-55: a trust survives a relaunch with no unlock, and the write it allows arrives once.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_55_trust_survives_a_relaunch_and_writes_once() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let context = cluster.context.as_str();
    let app = launch_protected(&cluster)?;
    lift_protection(&app, context, "TrustCluster", "trusted")?;
    let scratch = app.quit()?;
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());

    let app = App::launch(AppOptions::new(&cluster.kubeconfig).reuse(scratch))?;
    connect(&app, context)?;
    assert_eq!(sidebar(&app.state()?, context)["protection"], "trusted");
    open_live(&app, "deployments", 5)?;
    select(&app, Some("demo"), "web")?;
    run(&app, "workspace::RestartObject")?;
    let writes = answered(fake, 1)?;
    assert_eq!(writes[0].method, Method::PATCH);
    assert_eq!(writes[0].path, WEB);
    std::thread::sleep(RETRY_WINDOW);
    app.quit()?;
    assert_eq!(fake.writes().len(), 1, "{:?}", fake.writes());
    Ok(())
}
