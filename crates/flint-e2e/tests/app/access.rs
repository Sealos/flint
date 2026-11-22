//! App e2e for #267: what an identity limited to one namespace sees in the row menu and the action
//! registry, against the fake API server's access reviews.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use flint_e2e::app::Test::{Any, Equals, Exists};
use flint_e2e::app::{App, AppOptions, WAIT};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{AccessPolicy, Options};
use serde_json::{Value, json};

const PROTECTED: &str = "protect_local_clusters = true\n";
const DELETE_REASON: &str = "your access does not allow delete on deployments in demo";

fn enabled(app: &App, action: &str) -> Result<bool> {
    let actions = app.actions()?;
    let entry = actions["actions"]
        .as_array()
        .and_then(|all| all.iter().find(|a| a["name"] == action))
        .with_context(|| format!("no {action}"))?;
    Ok(entry["enabled"] == true)
}

/// Waits until the registry shows `action` as `wanted`.
fn wait_enabled(app: &App, action: &str, wanted: bool) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    while enabled(app, action)? != wanted {
        ensure!(Instant::now() < deadline, "{action} never became {wanted}");
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

// covers #267
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn a_namespace_limited_user_sees_forbidden_actions_disabled() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    fake.set_access(AccessPolicy::only_namespace("demo").deny("delete", "deployments"));

    let app = App::launch(AppOptions::new(&cluster.kubeconfig).settings(PROTECTED))?;
    app.select_cluster(&cluster.context)?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": cluster.context, "connection": "connected"})),
    )?;
    app.wait_for("/nav/status", Equals("ready".into()))?;
    // Reviews are sent only where the gate allows changes.
    app.dispatch(
        "workspace::UnlockCluster",
        json!({"cluster": cluster.context}),
    )?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": cluster.context, "protection": "unlocked"})),
    )?;

    app.open_view("deployments")?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    let state = app.wait_for("/list/rows", Any(json!({"name": "web"})))?;
    let row = state["list"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == "web"))
        .and_then(|r| r["id"].as_str())
        .context("no row web")?
        .to_owned();
    app.select_row(Some("demo"), "web")?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;

    // What is forbidden is disabled and refused; what is allowed stays enabled.
    wait_enabled(&app, "workspace::DeleteObject", false)?;
    wait_enabled(&app, "workspace::ScaleObject", true)?;
    wait_enabled(&app, "workspace::RestartObject", true)?;
    assert!(
        app.dispatch("workspace::DeleteObject", Value::Null)
            .is_err(),
        "a forbidden delete ran"
    );

    // The row menu says why.
    app.right_click(&row)?;
    app.wait_for(
        "/menu/entries",
        Any(json!({"label": "Delete…", "enabled": false, "reason": DELETE_REASON})),
    )?;
    app.wait_for(
        "/menu/entries",
        Any(json!({"label": "Scale…", "enabled": true})),
    )?;
    app.press_key("escape")?;
    app.wait_for("/menu", Equals(Value::Null))?;

    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    app.quit()?;
    Ok(())
}
