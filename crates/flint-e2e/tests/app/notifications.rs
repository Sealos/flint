//! Desktop notifications across clusters (#210): the decision, the OS's answer as reported, and
//! opening an alert on the exact cluster that raised it.

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::fake_api::{Gvr, WatchStep, write_kubeconfigs};
use serde_json::{Value, json};

use crate::support::*;

fn failed_job() -> Value {
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": "nightly", "namespace": "demo", "uid": "job-1",
                     "resourceVersion": "1"},
        "status": {"conditions": [{"type": "Failed", "status": "True", "reason": "BackoffLimitExceeded"}]},
    })
}

fn only_alert_tag(state: &Value) -> Result<String> {
    let recent = state["alerts"]["recent"]
        .as_array()
        .context("the recent alerts")?;
    anyhow::ensure!(recent.len() == 1, "expected one alert, got {recent:?}");
    Ok(recent[0]["tag"].as_str().context("its tag")?.to_owned())
}

// covers #210
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn an_alert_reports_delivery_and_opens_only_its_own_cluster() -> Result<()> {
    let two = TwoClusters::start()?;
    let app =
        App::launch(AppOptions::new(two.kubeconfig()).settings("desktop_notifications = true\n"))?;
    connect(&app, TwoClusters::B)?;
    connect(&app, TwoClusters::A)?;
    app.wait_for("/alerts/enabled", Equals(true.into()))?;
    app.wait_for("/alerts/checked", Contains(TwoClusters::A.into()))?;

    // Stored silently, then read by the list the feed makes after its watch expires.
    let jobs = Gvr::new("batch", "v1", "jobs");
    two.a.set_object(&jobs, failed_job());
    two.a.send_watch(&jobs, WatchStep::Expired);
    app.wait_for(
        "/alerts/recent",
        Any(json!({"cluster": TwoClusters::A, "delivery": "needs_app_bundle"})),
    )?;
    // This test binary is not an app bundle, so macOS cannot have been asked to show anything.
    let state = app.wait_for("/alerts/delivery", Equals("needs_app_bundle".into()))?;
    let tag = only_alert_tag(&state)?;

    // Opened from another cluster, it goes to the cluster that raised it.
    connect(&app, TwoClusters::B)?;
    app.dispatch("workspace::OpenAlert", json!({"tag": tag}))?;
    app.wait_for("/active_cluster", Equals(TwoClusters::A.into()))?;

    // Investigating a cluster not exposed to AI is refused: nothing opens and nothing is written.
    connect(&app, TwoClusters::B)?;
    app.dispatch("workspace::InvestigateAlert", json!({"tag": tag}))?;
    let state = app.wait_for("/banners", Test::at_least(1))?;
    let banners = state["banners"].to_string();
    anyhow::ensure!(banners.contains("not exposed"), "{banners}");
    anyhow::ensure!(state["active_cluster"] == TwoClusters::B, "{state}");
    anyhow::ensure!(two.a.writes().is_empty() && two.b.writes().is_empty());

    // Once that context is gone, it is refused and the active cluster stays.
    connect(&app, TwoClusters::A)?;
    connect(&app, TwoClusters::B)?;
    app.edit_kubeconfig([two.kubeconfig()], || {
        write_kubeconfigs(&two.kubeconfig(), &[(&two.b, TwoClusters::B, None)])
    })?;
    app.wait_for("/clusters", Test::at_most(1))?;
    app.dispatch("workspace::OpenAlert", json!({"tag": tag}))?;
    let state = app.wait_for("/banners", Test::at_least(1))?;
    let banners = state["banners"].to_string();
    anyhow::ensure!(banners.contains("no longer available"), "{banners}");
    anyhow::ensure!(state["alerts"]["recent"] == json!([]), "{state}");
    anyhow::ensure!(state["active_cluster"] == TwoClusters::B, "{state}");
    Ok(())
}
