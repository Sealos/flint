//! M3 user paths for the write actions (UP-53, UP-54, UP-55, UP-92), read from the fake API
//! server's write recorder.
//!
//! Every test turns on `protect_local_clusters`, and the ones that write unlock the cluster
//! through its dialog, so none depends on the default protection of a loopback address.

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions, Test};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{FakeApiServer, Gvr, Options, WriteRecord};
use http::Method;
use serde_json::{Value, json};

use crate::support::*;

const PROTECTED: &str = "protect_local_clusters = true\n";
const PODINFO: &str = "/apis/apps/v1/namespaces/demo/deployments/podinfo";
const RESTARTED_AT: &str = "/spec/template/metadata/annotations/kubectl.kubernetes.io~1restartedAt";
/// A node the protection test adds, already cordoned.
const CORDONED: &str = "cordoned-node";

fn launch(cluster: &TestCluster) -> Result<App> {
    let app = App::launch(AppOptions::new(&cluster.kubeconfig).settings(PROTECTED))?;
    connect(&app, &cluster.context)?;
    assert_eq!(
        sidebar(&app.state()?, &cluster.context)["protection"],
        "protected"
    );
    Ok(app)
}

/// Answers the dialog of `action` (`UnlockCluster`, `TrustCluster`) and waits for `protection`.
fn lift_protection(app: &App, context: &str, action: &str, protection: &str) -> Result<()> {
    app.dispatch(&format!("workspace::{action}"), json!({"cluster": context}))?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": context, "protection": protection})),
    )?;
    Ok(())
}

/// Selects `name` and waits until its object has loaded.
fn select(app: &App, namespace: Option<&str>, name: &str) -> Result<()> {
    app.select_row(namespace, name)?;
    app.wait_for("/detail/name", Equals(name.into()))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    Ok(())
}

/// Selects `name` in `demo` and opens the confirmation of `action` once the registry offers it
/// (a write still running on the object holds it back): returns what the dialog says.
fn confirmation(app: &App, name: &str, action: &str) -> Result<String> {
    select(app, Some("demo"), name)?;
    app_offers(app, action)?;
    app.dispatch(action, Value::Null)?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    Ok(state["dialog"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// Whether the registry offers `action` now.
fn enabled(app: &App, action: &str) -> Result<bool> {
    let actions = app.actions()?;
    let entry = actions["actions"]
        .as_array()
        .and_then(|all| all.iter().find(|a| a["name"] == action))
        .with_context(|| format!("no {action}"))?;
    Ok(entry["enabled"] == true)
}

/// Waits until the registry offers `action`.
fn app_offers(app: &App, action: &str) -> Result<()> {
    until(
        action,
        || enabled(app, action).unwrap_or(false),
        |offered| *offered,
    )?;
    Ok(())
}

/// Asserts the registry does not offer `action` and that dispatching it anyway is refused.
fn assert_refused(app: &App, action: &str) -> Result<()> {
    assert!(!enabled(app, action)?, "{action} is offered");
    assert!(app.dispatch(action, Value::Null).is_err(), "{action} ran");
    Ok(())
}

/// Adds a copy of the object `name` called `copy`, with its own uid and `change` applied.
fn add_copy(
    fake: &FakeApiServer,
    gvr: &Gvr,
    namespace: Option<&str>,
    name: &str,
    copy: &str,
    change: impl FnOnce(&mut Value),
) -> Result<()> {
    let mut object = fake
        .object(gvr, namespace, name)
        .with_context(|| format!("no {name}"))?;
    object["metadata"]["name"] = copy.into();
    object["metadata"]["uid"] = format!("uid-{copy}").into();
    change(&mut object);
    fake.set_object(gvr, object);
    Ok(())
}

fn dismiss(app: &App, key: &str) -> Result<()> {
    app.press_key(key)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    Ok(())
}

fn assert_names(text: &str, cluster: &TestCluster, name: &str) {
    for part in [
        format!("Context: {}", cluster.context),
        cluster.server.clone(),
        "Namespace: demo".to_owned(),
        name.to_owned(),
    ] {
        assert!(text.contains(&part), "no {part} in {text}");
    }
}

/// The one write sent since `before` writes: not a dry run, and successful.
fn one_write(writes: &[WriteRecord], before: usize) -> &WriteRecord {
    let new = &writes[before..];
    assert_eq!(new.len(), 1, "{new:?}");
    assert!(!new[0].is_dry_run(), "{:?}", new[0]);
    assert!(
        new[0].status.is_some_and(|code| (200..300).contains(&code)),
        "{:?}",
        new[0]
    );
    &new[0]
}

/// Waits until `count` writes were answered.
fn answered(fake: &FakeApiServer, count: usize) -> Result<Vec<WriteRecord>> {
    until(
        "the write's answer",
        || fake.writes(),
        |writes| writes.len() >= count && writes.iter().all(|w| w.status.is_some()),
    )
}

// covers UP-53, UP-92
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_53_destructive_actions_confirm() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch(&cluster)?;
    lift_protection(&app, &cluster.context, "UnlockCluster", "unlocked")?;
    open_live(&app, "deployments", 5)?;

    // Delete: Escape and Enter both cancel it, and nothing is sent before Confirm.
    for key in ["escape", "enter"] {
        let text = confirmation(&app, "web", "workspace::DeleteObject")?;
        assert_names(&text, &cluster, "web");
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
        dismiss(&app, key)?;
        assert!(fake.writes().is_empty(), "{key}: {:?}", fake.writes());
    }
    confirmation(&app, "web", "workspace::DeleteObject")?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    let writes = answered(fake, 1)?;
    let delete = one_write(&writes, 0);
    assert_eq!(delete.method, Method::DELETE);
    assert_eq!(delete.path, "/apis/apps/v1/namespaces/demo/deployments/web");
    app.wait_for("/list/rows", Test::at_most(4))?;

    // Rollout restart: cancelled, then sent once, to podinfo alone.
    let deployments = fake.gvr("deployments");
    let stored = || fake.object(&deployments, Some("demo"), "podinfo");
    let original = stored().context("no podinfo")?;
    assert!(original.pointer(RESTARTED_AT).is_none(), "{original}");
    assert_ne!(original.pointer("/spec/replicas"), Some(&json!(5)));
    let before = fake.writes().len();
    let text = confirmation(&app, "podinfo", "workspace::RestartObject")?;
    assert_names(&text, &cluster, "podinfo");
    dismiss(&app, "escape")?;
    assert_eq!(fake.writes().len(), before);
    confirmation(&app, "podinfo", "workspace::RestartObject")?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    let writes = answered(fake, before + 1)?;
    let restart = one_write(&writes, before);
    assert_eq!(restart.method, Method::PATCH);
    assert_eq!(restart.path, PODINFO);
    let restarted = stored().context("no podinfo")?;
    assert!(
        restarted
            .pointer(RESTARTED_AT)
            .is_some_and(Value::is_string),
        "{restarted}"
    );

    // Scale: the count comes first, then a confirmation showing it; only that one writes.
    let before = writes.len();
    confirmation(&app, "podinfo", "workspace::ScaleObject")?;
    app.press_key("secondary-a")?;
    app.type_text("5")?;
    app.dispatch("ui::Confirm", Value::Null)?;
    let state = app.wait_for("/dialog/text", Contains("to 5.".into()))?;
    assert_names(
        state["dialog"]["text"].as_str().unwrap_or_default(),
        &cluster,
        "podinfo",
    );
    assert_eq!(fake.writes().len(), before, "nothing before Confirm");
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    let writes = answered(fake, before + 1)?;
    let scale = one_write(&writes, before);
    assert_eq!(scale.method, Method::PATCH);
    assert_eq!(scale.path, format!("{PODINFO}/scale"));
    let scaled = stored().context("no podinfo")?;
    assert_eq!(
        scaled.pointer("/spec/replicas"),
        Some(&json!(5)),
        "{scaled}"
    );
    // After quitting, so a repeated write would have arrived.
    app.quit()?;
    assert_eq!(fake.writes().len(), 3, "{:?}", fake.writes());
    Ok(())
}

// covers UP-54
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_54_protected_cluster_is_read_only() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let nodes = fake.gvr("nodes");
    let node = fake.rows(&nodes)[0].name().to_owned();
    add_copy(fake, &nodes, None, &node, CORDONED, |object| {
        object["spec"]["unschedulable"] = true.into();
    })?;
    let app = launch(&cluster)?;

    // Each action is unavailable, and dispatching it anyway is refused.
    open_live(&app, "nodes", 2)?;
    select(&app, None, &node)?;
    for action in ["workspace::CordonNode", "workspace::DrainNode"] {
        assert_refused(&app, action)?;
    }
    select(&app, None, CORDONED)?;
    assert_refused(&app, "workspace::UncordonNode")?;
    open_live(&app, "deployments", 5)?;
    select(&app, Some("demo"), "web")?;
    for action in [
        "workspace::ScaleObject",
        "workspace::RestartObject",
        "workspace::DeleteObject",
    ] {
        assert_refused(&app, action)?;
    }

    // The YAML editor is refused too, saying why.
    app.dispatch("detail::ShowYaml", Value::Null)?;
    app.wait_for("/detail/edit_blocked", Contains("protected".into()))?;
    assert!(app.dispatch("detail::EditYaml", Value::Null).is_err());
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());

    // Protection was the reason: unlocked, the same actions are offered.
    lift_protection(&app, &cluster.context, "UnlockCluster", "unlocked")?;
    for action in [
        "workspace::ScaleObject",
        "workspace::RestartObject",
        "workspace::DeleteObject",
    ] {
        app_offers(&app, action)?;
    }
    open_live(&app, "nodes", 2)?;
    select(&app, None, &node)?;
    for action in ["workspace::CordonNode", "workspace::DrainNode"] {
        app_offers(&app, action)?;
    }
    select(&app, None, CORDONED)?;
    app_offers(&app, "workspace::UncordonNode")?;
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    app.quit()?;
    Ok(())
}

// covers UP-55: an unlock and a trust are pinned to the server: a re-pointed context is protected
// again, its writes are refused on the new server, and nothing is written to either server.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_55_repointing_protects_again() -> Result<()> {
    const ONLY_ON_SECOND: &str = "only-on-second";
    const WRITES: [&str; 3] = [
        "workspace::ScaleObject",
        "workspace::RestartObject",
        "workspace::DeleteObject",
    ];
    for (action, protection) in [("UnlockCluster", "unlocked"), ("TrustCluster", "trusted")] {
        let cluster = TestCluster::fake(Options::default())?;
        let fake = cluster.fake_server().expect("a fake server");
        let other = FakeApiServer::start_blocking(Options::default())?;
        let deployments = other.gvr("deployments");
        add_copy(
            &other,
            &deployments,
            Some("demo"),
            "web",
            ONLY_ON_SECOND,
            |_| {},
        )?;
        let app = launch(&cluster)?;
        lift_protection(&app, &cluster.context, action, protection)?;

        // Lifted: the first server's object offers every write.
        open_live(&app, "deployments", 5)?;
        select(&app, Some("demo"), "web")?;
        for write in WRITES {
            app_offers(&app, write)?;
        }

        app.edit_kubeconfig([&cluster.kubeconfig], || {
            other.write_kubeconfig(&cluster.kubeconfig, None)
        })?;
        app.wait_for(
            "/clusters",
            Any(json!({"name": cluster.context, "protection": "protected"})),
        )?;

        // The app reconnects by itself. Views can open again once it has reached the second
        // server, and the row only that server has shows the list reads from it.
        until(
            "a request to the second server",
            || other.requests().len(),
            |count| *count > 0,
        )?;
        app.wait_for(
            "/clusters",
            Any(json!({"name": cluster.context, "connection": "connected"})),
        )?;
        app.wait_for("/nav/status", Equals("ready".into()))?;
        until(
            "the deployments view",
            || app.open_view("deployments").is_ok(),
            |opened| *opened,
        )?;
        app.wait_for("/list/rows", Any(json!({"name": ONLY_ON_SECOND})))?;
        select(&app, Some("demo"), ONLY_ON_SECOND)?;
        for write in WRITES {
            assert_refused(&app, write)?;
        }

        // Protection was the reason: lifted for this server, the same object offers them.
        lift_protection(&app, &cluster.context, action, protection)?;
        for write in WRITES {
            app_offers(&app, write)?;
        }

        assert!(fake.writes().is_empty(), "{action}: {:?}", fake.writes());
        assert!(other.writes().is_empty(), "{action}: {:?}", other.writes());
        // After quitting, so a write still on its way would have arrived.
        app.quit()?;
        assert!(fake.writes().is_empty(), "{action}: {:?}", fake.writes());
        assert!(other.writes().is_empty(), "{action}: {:?}", other.writes());
    }
    Ok(())
}
