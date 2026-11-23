//! Comparing an object across clusters, opened from its row's menu (#449).

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::fake_api::{FakeApiServer, Gvr};
use http::Method;
use serde_json::{Value, json};

use crate::support::*;

const NAME: &str = "compare-me";
const PATH: &str = "/api/v1/namespaces/demo/configmaps/compare-me";

fn seed(fake: &FakeApiServer, mode: &str) {
    fake.set_object(
        &Gvr::core("configmaps"),
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": NAME, "namespace": "demo", "uid": format!("uid-{mode}"),
                         "resourceVersion": "1"},
            "data": {"mode": mode, "same": "1"},
        }),
    );
}

/// How many times `fake` was asked for the compared object.
fn reads(fake: &FakeApiServer) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.method == Method::GET && r.path == PATH)
        .count()
}

fn wait_compare(app: &App, compare: Value) -> Result<Value> {
    Ok(app.wait_for(
        "/dock/tabs",
        Any(json!({"kind": "compare", "compare": compare})),
    )?)
}

// covers #449
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn compare_an_object_from_its_row_menu() -> Result<()> {
    let two = TwoClusters::start()?;
    seed(&two.a, "blue");
    seed(&two.b, "green");
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    connect(&app, TwoClusters::B)?;
    connect(&app, TwoClusters::A)?;
    let state = open_live(&app, "configmaps", 1)?;
    let row = state["list"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == NAME))
        .and_then(|r| r["id"].as_str())
        .context("the object's row")?
        .to_owned();

    app.right_click(&row)?;
    app.wait_for(
        "/menu/entries",
        Any(json!({"label": "Compare Across Clusters", "enabled": true})),
    )?;
    app.press_key("down")?;
    app.press_key("enter")?;

    let state = wait_compare(&app, json!({"verdict": "1 field differs."}))?;
    let compare = state["dock"]["tabs"]
        .as_array()
        .and_then(|tabs| tabs.iter().find(|t| t["kind"] == "compare"))
        .map(|t| &t["compare"])
        .context("the compare tab")?;
    assert_eq!(compare["sides"][0]["name"], TwoClusters::A);
    assert_eq!(compare["sides"][1]["name"], TwoClusters::B);
    assert_eq!(compare["rows"], json!(["data.mode: blue, green"]));

    app.dispatch("compare::ToggleCompareSame", Value::Null)?;
    wait_compare(&app, json!({"show_same": true}))?;
    app.dispatch("compare::ToggleCompareVolatile", Value::Null)?;
    wait_compare(&app, json!({"include_volatile": true}))?;

    let before = reads(&two.b);
    seed(&two.b, "red");
    app.dispatch("compare::RefreshCompare", Value::Null)?;
    app.wait_for(
        "/dock/tabs/0/compare/rows",
        Any(json!("data.mode: blue, red")),
    )?;
    assert!(reads(&two.b) > before, "Refresh read the cluster again");

    assert!(two.a.writes().is_empty(), "{:?}", two.a.writes());
    assert!(two.b.writes().is_empty(), "{:?}", two.b.writes());
    app.quit()?;
    Ok(())
}
