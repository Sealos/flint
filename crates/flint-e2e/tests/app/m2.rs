//! M2 user paths: logs and shells in the dock (docs/USER-PATHS.md).
//!
//! Not here yet: the rest of UP-40 and UP-43 (#122), UP-41 against the fake exec (#123),
//! UP-42 across clusters (#124) and UP-45 (#125). Pause, search, toggles and copy are view
//! tests (`log_tests.rs`).

use anyhow::Result;
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{ContainerLog, Options};
use serde_json::{Value, json};

use crate::support::*;

/// The running pod whose log the test reads: one container, `nginx`.
const POD: &str = "web-69c6f74b8b-qv4xv";

/// The tab with id `id` in the state's dock.
fn tab(state: &Value, id: u64) -> &Value {
    state["dock"]["tabs"]
        .as_array()
        .and_then(|tabs| tabs.iter().find(|t| t["id"] == id))
        .unwrap_or(&Value::Null)
}

// covers UP-40, UP-42
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_40_read_pod_logs() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    let state = open_live(&app, "pods", 17)?;
    let row = state["list"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == POD))
        .expect("the pod's row");
    let row_id = row["id"].as_str().expect("the row's element id").to_owned();
    assert_eq!(app.state()?["dock"]["visible"], false);

    // The row's menu: a right-click selects the row and offers Logs, Previous Logs and Shell.
    app.right_click(&row_id)?;
    let state = app.wait_for("/menu/entries", Any(json!({"label": "Logs"})))?;
    assert!(
        state["menu"]["owner"]
            .as_str()
            .is_some_and(|o| o.starts_with("row-menu-")),
        "{}",
        state["menu"]
    );
    app.wait_for("/detail/name", Equals(POD.into()))?;
    // Once the pod has loaded, Previous Logs says why it can't run: nginx never restarted.
    let state = app.wait_for(
        "/menu/entries",
        Any(json!({"label": "Previous Logs", "enabled": false})),
    )?;
    let previous = state["menu"]["entries"]
        .as_array()
        .and_then(|e| e.iter().find(|e| e["label"] == "Previous Logs"))
        .expect("Previous Logs");
    assert!(
        previous["reason"]
            .as_str()
            .is_some_and(|r| r.contains("has not restarted")),
        "{previous}"
    );
    app.screenshot("up40-row-menu")?;
    app.press_key("escape")?;
    app.wait_for("/menu", Equals(Value::Null))?;

    // Logs opens a tab that streams the pod's log: its canned lines, then a line appended now.
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    app.dispatch("detail::OpenLogs", Value::Null)?;
    let state = app.wait_for(
        "/dock/tabs",
        Any(json!({"kind": "pod_logs", "status": {"state": "streaming"}})),
    )?;
    let id = state["dock"]["active"].as_u64().expect("the tab is shown");
    let shown = tab(&state, id);
    assert!(
        shown["title"].as_str().is_some_and(|t| t.starts_with(POD)),
        "{shown}"
    );
    assert_eq!(shown["cluster"]["context"], cluster.context.as_str());
    assert_eq!(state["dock"]["visible"], true);
    assert_eq!(state["focus"], "log");
    let index = state["dock"]["tabs"]
        .as_array()
        .and_then(|tabs| tabs.iter().position(|t| t["id"] == id))
        .expect("the tab");
    app.wait_for(&format!("/dock/tabs/{index}/logs/lines"), Test::at_least(3))?;
    until(
        "one open log stream",
        || fake.open_log_streams(),
        |n| *n == 1,
    )?;
    let log = ContainerLog::new("demo", POD);
    let mut sent = 0;
    // The stream may still be catching up when the line is written; it gets it either way.
    sent += fake.append_log(&log, "ERROR e2e line reached the tab");
    let state = app.wait_for(
        &format!("/dock/tabs/{index}/logs/last_lines"),
        Any(json!({"text": "ERROR e2e line reached the tab", "severity": "error"})),
    )?;
    assert_eq!(sent, 1, "one stream got the line");
    assert_eq!(tab(&state, id)["logs"]["follows"], true);
    app.screenshot_themes("up40-pod-logs")?;

    // Closing the tab ends its stream (UP-42: sessions end only when closed).
    app.dispatch("dock::CloseTab", json!({"tab": id}))?;
    let state = app.wait_for("/dock/tabs", Equals(json!([])))?;
    assert_eq!(state["dock"]["visible"], false);
    until(
        "no open log stream",
        || fake.open_log_streams(),
        |n| *n == 0,
    )?;
    assert_eq!(
        fake.log_requests().len(),
        1,
        "one log request for one tab, no reconnect"
    );
    Ok(())
}
