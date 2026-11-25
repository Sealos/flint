//! M4 user paths: curated views and metrics (docs/USER-PATHS.md).
//!
//! The pod and node lists' readings are in `dashboard_usage.rs`: the app's lists do not show the
//! CPU and Memory columns against the fake metrics-server yet, so only the hint is checked here.
//!
//! Not here: UP-60 (the detail pane's container states, rollout progress and endpoints) has no
//! automation state to read yet, UP-62 to UP-64 need the diagram, which is not built, and
//! UP-66's sparkline needs a clock control. UP-66's figures and UP-65's counts are in
//! `dashboard_usage.rs` and `dashboard_counts.rs`.

use anyhow::Result;
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::Options;
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
