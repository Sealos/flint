//! M4 user paths: curated views and metrics (docs/USER-PATHS.md).
//!
//! The pod and node lists' readings are in `dashboard_usage.rs`; here the app's pod list shows the
//! CPU and Memory columns against the fake metrics-server, and the hint without one.
//!
//! Not here: UP-60 (the detail pane's container states, rollout progress and endpoints) has no
//! automation state to read yet, UP-62 to UP-64 need the diagram, which is not built, and
//! UP-66's sparkline needs a clock control. UP-66's figures and UP-65's counts are in
//! `dashboard_usage.rs` and `dashboard_counts.rs`.

use anyhow::Result;
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::clock::FIXTURE_NOW_SECONDS;
use flint_e2e::fake_api::{Gvr, MetricsTarget, Options};
use k8s_openapi::jiff::Timestamp;
use serde_json::{Value, json};

use crate::support::*;

const HINT_ID: &str = "list-usage-hint";

fn fake_app(options: Options) -> Result<(TestCluster, App)> {
    let cluster = TestCluster::fake(options)?;
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    Ok((cluster, app))
}

/// Whether the list shows CPU or Memory. Fails if it lists no columns at all.
fn has_usage_columns(state: &Value) -> bool {
    let columns = state["list"]["columns"].as_array().expect("list columns");
    assert!(!columns.is_empty(), "the list has no columns");
    columns.contains(&json!("CPU")) || columns.contains(&json!("Memory"))
}

// covers UP-61: with metrics-server the pod and workload lists show the CPU and Memory columns,
// wide enough that none is shed.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_61_with_metrics_server_the_pod_and_workload_lists_show_cpu_and_memory() -> Result<()> {
    let (cluster, app) = fake_app(Options::default())?;
    cluster.fake_server().expect("a fake server").set_metrics(
        &MetricsTarget::pod("demo", "crash-loop"),
        "30m",
        "20Mi",
    );
    open_live(&app, "pods", 17)?;
    app.wait_for("/list/columns", Contains(json!("CPU")))?;
    app.wait_for("/list/columns", Contains(json!("Memory")))?;
    app.screenshot_themes("up61-pod-usage-bars")?;

    // The workload lists carry the same columns, as totals over their pods.
    app.open_view("deployments")?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    app.wait_for("/list/columns", Contains(json!("CPU")))?;
    app.wait_for("/list/columns", Contains(json!("Memory")))?;
    Ok(())
}

// covers UP-61: without metrics-server the columns are hidden and a hint says why.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_61_without_metrics_server_the_columns_are_hidden_with_a_hint() -> Result<()> {
    let (_cluster, app) = fake_app(Options {
        no_metrics: true,
        ..Options::default()
    })?;
    open_live(&app, "pods", 17)?;
    let state = app.wait_for("/list/banners", Any(json!({"id": HINT_ID})))?;
    assert!(!has_usage_columns(&state), "{}", state["list"]["columns"]);
    let banners = state["list"]["banners"].to_string();
    assert!(banners.contains("metrics-server"), "{banners}");
    app.screenshot_themes("up61-no-metrics")?;
    Ok(())
}

/// The feed's row for the event `reason`, if there is one.
fn feed_row<'a>(state: &'a Value, reason: &str) -> Option<&'a Value> {
    state["dashboard"]["warnings"]
        .as_array()?
        .iter()
        .find(|row| row["reason"] == reason)
}

// covers UP-68: a Warning event shows in the dashboard's activity feed, and a click on its row
// opens the object it is about. A Normal event and one past the hour stay out.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_68_activity_feed() -> Result<()> {
    const REASON: &str = "BackOffForTheFeed";
    const POD: &str = "web-69c6f74b8b-qv4xv";
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let events = Gvr::core("events");
    let event = |name: &str, kind: &str, reason: &str, minutes: i64| {
        let seen = Timestamp::from_second(FIXTURE_NOW_SECONDS - minutes * 60).unwrap();
        json!({
            "apiVersion": "v1", "kind": "Event",
            "metadata": {"name": name, "namespace": "demo", "uid": format!("uid-{name}"),
                         "creationTimestamp": seen.to_string()},
            "type": kind, "reason": reason, "message": "Back-off restarting failed container",
            "involvedObject": {"kind": "Pod", "apiVersion": "v1", "namespace": "demo", "name": POD},
            "lastTimestamp": seen.to_string(), "count": 3,
        })
    };

    // The events are there before the app reads them: it polls events every half minute.
    fake.set_object(&events, event("feed-warning", "Warning", REASON, 5));
    fake.set_object(
        &events,
        event("feed-normal", "Normal", "NormalForTheFeed", 5),
    );
    fake.set_object(
        &events,
        event("feed-old", "Warning", "TooOldForTheFeed", 180),
    );
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    // The overview is the first thing shown.
    let state = app.wait_for("/dashboard/warnings", Any(json!({"reason": REASON})))?;
    let row = feed_row(&state, REASON).expect("the Warning's row");
    assert_eq!(row["kind"], "Pod.v1", "{row}");
    assert_eq!(row["namespace"], "demo", "{row}");
    assert_eq!(row["name"], POD, "{row}");
    assert_eq!(row["opens"], true, "{row}");
    assert_eq!(row["occurrences"], 3, "{row}");
    assert_eq!(feed_row(&state, "NormalForTheFeed"), None);
    assert_eq!(feed_row(&state, "TooOldForTheFeed"), None);
    app.screenshot_themes("up68-activity-feed")?;

    // A click on the row opens the pod it is about.
    let id = row["id"].as_str().expect("the row's id").to_owned();
    app.click(&id)?;
    let state = app.wait_for("/detail/name", Equals(POD.into()))?;
    assert_eq!(state["detail"]["namespace"], "demo", "{}", state["detail"]);
    assert_eq!(state["dashboard"], Value::Null);
    assert!(fake.writes().is_empty());
    Ok(())
}
