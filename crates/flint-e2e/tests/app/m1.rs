//! M1 user paths: browsing resources (docs/USER-PATHS.md).
//!
//! Not here: UP-26 (10,000 pods stay smooth, within a memory budget) is a load test with
//! numbers recorded against kind (`crates/flint-core/tests/watch_load.rs`); an e2e pass or fail
//! says nothing about smoothness.

use std::time::{Duration, Instant};

use anyhow::Result;
use flint_e2e::app::{App, AppOptions, WAIT};
use flint_e2e::backend::{Backend, TestCluster};
use flint_e2e::fake_api::{FakeApiServer, Fault, Gvr, Options, WatchStep};
use serde_json::{Value, json};

use crate::support::*;

fn fake_app(options: Options) -> Result<(TestCluster, App)> {
    let cluster = TestCluster::fake(options)?;
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    Ok((cluster, app))
}

fn row<'a>(state: &'a Value, name: &str) -> &'a Value {
    state["list"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == name))
        .unwrap_or(&Value::Null)
}

/// A pod row for the fake server with every column `kubectl get pods` fills.
fn pod_row(fake: &FakeApiServer, name: &str, status: &str) -> flint_e2e::fake_api::FakeRow {
    let ready = if status == "Running" { "1/1" } else { "0/1" };
    fake.new_row(
        &fake.gvr("pods"),
        Some("demo"),
        name,
        &[("Ready", ready), ("Status", status), ("Restarts", "0")],
    )
}

// covers UP-20
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_20_browse_pods_with_live_updates() -> Result<()> {
    let (cluster, app) = fake_app(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let pods = fake.gvr("pods");
    let state = open_live(&app, "pods", 17)?;
    assert_eq!(
        state["list"]["columns"],
        json!(["Name", "Namespace", "Ready", "Status", "Restarts", "Age"])
    );
    assert_eq!(
        state["header"]["breadcrumb"],
        json!([cluster.context, "Pods"])
    );
    let crash = &row(&state, "crash-loop")["cells_by_column"];
    assert_eq!(crash["Status"], "CrashLoopBackOff");
    assert_eq!(crash["Namespace"], "demo");
    app.screenshot_themes("up20-pods")?;

    // A pod appears Pending, turns Running, and goes away, each within about a second.
    let pending = pod_row(fake, "scaled-up", "Pending");
    until("a pods watch", || fake.open_watches(&pods), |n| *n >= 1)?;
    let quick = Duration::from_secs(2);
    let start = Instant::now();
    fake.send_watch(&pods, WatchStep::Added(pending.clone()));
    app.wait_for_within(
        "/list/rows",
        Any(json!({"name": "scaled-up", "cells_by_column": {"Status": "Pending"}})),
        quick,
    )?;
    let running = pending
        .clone()
        .with_cell(fake.column(&pods, "Ready").unwrap(), "1/1")
        .with_cell(fake.column(&pods, "Status").unwrap(), "Running");
    fake.send_watch(&pods, WatchStep::Modified(running.clone()));
    app.wait_for_within(
        "/list/rows",
        Any(json!({"name": "scaled-up", "cells_by_column": {"Status": "Running", "Ready": "1/1"}})),
        quick,
    )?;
    fake.send_watch(&pods, WatchStep::Deleted(running));
    let state = app.wait_for_within("/list/shown", Equals(17.into()), quick)?;
    assert!(row(&state, "scaled-up").is_null());
    eprintln!("add, modify and delete shown in {:?}", start.elapsed());

    // Age keeps counting.
    let age = |s: &Value| row(s, "crash-loop")["cells_by_column"]["Age"].clone();
    let before = age(&app.state()?);
    let deadline = Instant::now() + WAIT;
    while age(&app.state()?) == before {
        assert!(Instant::now() < deadline, "Age stayed at {before}");
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

// covers UP-21
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_21_filter_and_sort() -> Result<()> {
    let two = TwoClusters::start()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    connect(&app, TwoClusters::A)?;
    open_live(&app, "pods", 17)?;

    // One namespace, then two.
    app.dispatch("workspace::SetNamespaces", json!({"namespaces": ["demo"]}))?;
    let state = app.wait_for("/list/shown", Equals(7.into()))?;
    assert_eq!(state["header"]["namespaces"], json!(["demo"]));
    assert!(
        state["list"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["namespace"] == "demo")
    );
    app.dispatch(
        "workspace::ToggleNamespace",
        json!({"namespace": "local-path-storage"}),
    )?;
    app.wait_for("/list/shown", Equals(8.into()))?;
    app.dispatch(
        "workspace::ToggleNamespace",
        json!({"namespace": "local-path-storage"}),
    )?;
    app.wait_for("/list/shown", Equals(7.into()))?;

    // A text filter, typed into the filter field.
    app.dispatch("resource_list::FocusFilter", Value::Null)?;
    app.wait_for("/focus", Equals("list_filter".into()))?;
    app.type_text("web")?;
    let state = app.wait_for("/list/shown", Equals(2.into()))?;
    assert_eq!(state["list"]["filter"], "web");
    assert!(row_names(&state).iter().all(|n| n.starts_with("web-")));
    app.screenshot_themes("up21-filtered")?;
    for _ in 0..3 {
        app.press_key("backspace")?;
    }
    app.wait_for("/list/shown", Equals(7.into()))?;

    // Sort by a column: ascending, then descending.
    app.dispatch("resource_list::SortBy", json!({"column": "Name"}))?;
    let state = app.wait_for(
        "/list/sort",
        Equals(json!({"column": "Name", "descending": false})),
    )?;
    let names = row_names(&state);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    app.dispatch("resource_list::SortBy", json!({"column": "Name"}))?;
    let state = app.wait_for("/list/sort/descending", Equals(true.into()))?;
    sorted.reverse();
    assert_eq!(row_names(&state), sorted);
    app.screenshot_themes("up21-sorted")?;

    // The namespace choice is per cluster, and survives a switch and a relaunch.
    connect(&app, TwoClusters::B)?;
    let state = app.state()?;
    assert_eq!(
        state["header"]["namespaces"],
        json!([]),
        "{}",
        state["header"]
    );
    connect(&app, TwoClusters::A)?;
    app.wait_for("/header/namespaces", Equals(json!(["demo"])))?;
    let (scratch, _) = app.quit()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()).reuse(scratch))?;
    connect(&app, TwoClusters::A)?;
    app.wait_for("/header/namespaces", Equals(json!(["demo"])))?;
    Ok(())
}

// covers UP-22
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_22_browse_a_crd() -> Result<()> {
    let Some(cluster) = Backend::from_env().start()? else {
        return Ok(());
    };
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    let state = app.wait_for("/nav/status", Equals("ready".into()))?;
    let sections = state["nav"]["sections"].as_array().unwrap();
    assert!(
        sections.iter().any(|s| s["title"] == "fixtures.flint.dev"),
        "the CRD's group is a section: {sections:?}"
    );
    app.dispatch(
        "workspace::ToggleNavSection",
        json!({"section": "fixtures.flint.dev"}),
    )?;
    let state = app.wait_for(
        "/nav/sections",
        Any(json!({"title": "fixtures.flint.dev", "collapsed": false})),
    )?;
    let section = state["nav"]["sections"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["title"] == "fixtures.flint.dev"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        section["entries"][0]["kind"], "gadgets.fixtures.flint.dev",
        "{section}"
    );
    let state = open_live(&app, "gadgets.fixtures.flint.dev", 1)?;
    assert_eq!(
        state["list"]["columns"],
        json!(["Name", "Namespace", "Size", "Age"])
    );
    assert_eq!(
        row(&state, "gadget-1")["cells_by_column"]["Size"],
        "3",
        "{}",
        state["list"]
    );
    // Color is a priority-1 printer column: shown with -o wide, as kubectl does.
    app.dispatch("resource_list::ToggleWide", Value::Null)?;
    let state = app.wait_for("/list/wide", Equals(true.into()))?;
    assert_eq!(
        state["list"]["columns"],
        json!(["Name", "Namespace", "Size", "Color", "Age"])
    );
    let gadget = &row(&state, "gadget-1")["cells_by_column"];
    assert_eq!(gadget["Size"], "3");
    assert_eq!(gadget["Color"], "teal");
    app.screenshot_themes("up22-crd")?;
    Ok(())
}

// covers UP-23
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_23_inspect_an_object() -> Result<()> {
    let Some(cluster) = Backend::from_env().start()? else {
        return Ok(());
    };
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    app.dispatch("workspace::SetNamespaces", json!({"namespaces": ["demo"]}))?;
    open_live(&app, "pods", 3)?;
    let web = row_names(&app.state()?)
        .into_iter()
        .find(|n| n.starts_with("web-"))
        .expect("a web pod");
    app.select_row(Some("demo"), &web)?;
    let state = app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    let detail = &state["detail"];
    assert_eq!(detail["name"], web);
    assert_eq!(detail["tab"], "overview");
    assert_eq!(detail["managed_fields_shown"], false);
    let count = |name: &str| {
        detail["sections"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["name"] == name))
            .and_then(|s| s["count"].as_u64())
    };
    assert!(count("Labels") >= Some(1), "{detail}");
    assert_eq!(count("Owners"), Some(1), "{detail}");
    assert!(count("Conditions") >= Some(1), "{detail}");
    assert!(count("Events") >= Some(1), "the pod's events: {detail}");
    assert_eq!(state["header"]["breadcrumb"][2], web);
    app.screenshot_themes("up23-detail-overview")?;

    app.dispatch("detail::ShowYaml", Value::Null)?;
    let state = app.wait_for("/detail/tab", Equals("yaml".into()))?;
    assert_eq!(state["detail"]["managed_fields_shown"], false);
    app.screenshot_themes("up23-detail-yaml")?;
    app.dispatch("detail::ShowOverview", Value::Null)?;
    app.wait_for("/detail/tab", Equals("overview".into()))?;

    // Escape in the list closes the detail.
    app.dispatch("resource_list::CloseDetail", Value::Null)?;
    app.wait_for("/detail", Equals(Value::Null))?;
    Ok(())
}

fn open_watches(fake: &FakeApiServer, gvrs: &[Gvr]) -> usize {
    gvrs.iter().map(|g| fake.open_watches(g)).sum()
}

// covers UP-24
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_24_switch_clusters() -> Result<()> {
    let two = TwoClusters::start()?;
    let gvrs = [
        two.a.gvr("pods"),
        two.a.gvr("deployments"),
        two.a.gvr("services"),
    ];
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    connect(&app, TwoClusters::A)?;
    open_live(&app, "pods", 17)?;
    until(
        "fake-a's watches",
        || open_watches(&two.a, &gvrs),
        |n| *n >= 3,
    )?;

    // Away to fake-b: fake-a's watches stop.
    connect(&app, TwoClusters::B)?;
    until(
        "fake-a's watches to stop",
        || open_watches(&two.a, &gvrs),
        |n| *n == 0,
    )?;
    open_live(&app, "pods", 17)?;

    // A change on fake-a while nobody watches; switching back loads fresh data.
    let pods = two.a.gvr("pods");
    two.a
        .set_row(&pods, pod_row(&two.a, "while-away", "Running"));
    app.select_cluster(TwoClusters::A)?;
    app.wait_for("/active_cluster", Equals(TwoClusters::A.into()))?;
    if app.state()?["list"].is_null() {
        app.open_view("pods")?;
    }
    app.wait_for("/list/rows", Any(json!({"name": "while-away"})))?;
    until(
        "fake-b's watches to stop",
        || open_watches(&two.b, &gvrs),
        |n| *n == 0,
    )?;
    Ok(())
}

// covers UP-25
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_25_change_the_watch_mode() -> Result<()> {
    let (cluster, app) = fake_app(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let (pods, deployments) = (fake.gvr("pods"), fake.gvr("deployments"));
    open_live(&app, "pods", 17)?;
    // Default mode, all kinds: kinds not open are watched too.
    until(
        "deployments watched",
        || fake.open_watches(&deployments),
        |n| *n >= 1,
    )?;

    // On view, for this cluster: only the open list is watched, and it persists.
    app.dispatch("workspace::WatchOpenViewsOnly", Value::Null)?;
    let state = app.wait_for("/header/watch_mode_source", Equals("cluster".into()))?;
    assert_eq!(state["header"]["watch_mode"], "Live: open views only");
    until(
        "deployments unwatched",
        || fake.open_watches(&deployments),
        |n| *n == 0,
    )?;
    assert!(fake.open_watches(&pods) >= 1);
    let settings = std::fs::read_to_string(app.config_dir().join("settings.toml"))?;
    assert!(
        settings.contains("cluster_watch_mode") && settings.contains(&cluster.context),
        "{settings}"
    );

    // Back to the default; then "on view" as the default for every cluster.
    app.dispatch("workspace::UseDefaultWatchMode", Value::Null)?;
    app.wait_for("/header/watch_mode_source", Equals("default".into()))?;
    until(
        "deployments watched again",
        || fake.open_watches(&deployments),
        |n| *n >= 1,
    )?;
    app.dispatch("workspace::DefaultWatchOpenViewsOnly", Value::Null)?;
    app.wait_for("/header/watch_mode", Equals("Live: open views only".into()))?;
    until(
        "deployments unwatched",
        || fake.open_watches(&deployments),
        |n| *n == 0,
    )?;
    let settings = std::fs::read_to_string(app.config_dir().join("settings.toml"))?;
    assert!(settings.contains("watch_mode = \"on_view\""), "{settings}");
    Ok(())
}

// covers UP-27
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_27_recover_from_a_broken_watch() -> Result<()> {
    let (cluster, app) = fake_app(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let pods = fake.gvr("pods");
    open_live(&app, "pods", 17)?;
    until("a pods watch", || fake.open_watches(&pods), |n| *n >= 1)?;

    // The watch expires (410) after a change it never sent: the list re-syncs on its own.
    fake.set_row(&pods, pod_row(fake, "missed-1", "Running"));
    fake.send_watch(&pods, WatchStep::Expired);
    let state = app.wait_for("/list/rows", Any(json!({"name": "missed-1"})))?;
    assert_eq!(state["list"]["sync"], "live");

    // The connection drops and the server stays away: "reconnecting", never "live".
    fake.inject(Fault::drop_connection().on(&pods));
    fake.send_watch(&pods, WatchStep::Drop);
    let state = app.wait_for("/list/sync", Equals("reconnecting".into()))?;
    // The rows it had stay, under the "reconnecting" chip and a banner, not as live data.
    assert!(!row(&state, "missed-1").is_null(), "{}", state["list"]);
    let banners = state["list"]["banners"].to_string();
    assert!(banners.contains("Reconnecting"), "{banners}");
    // In plain words ("the connection to https://… dropped"), not the client's chain
    // ("watch stream broke: ServiceError: error reading a body from connection").
    assert!(!banners.contains("ServiceError"), "{banners}");
    assert!(banners.contains("the connection to https://"), "{banners}");
    app.screenshot_themes("up27-reconnecting")?;
    assert_eq!(app.state()?["list"]["sync"], "reconnecting");

    // The server is back with a change made meanwhile: it shows, and the list is live again.
    fake.set_row(&pods, pod_row(fake, "missed-2", "Running"));
    fake.clear_faults();
    let state = app.wait_for("/list/rows", Any(json!({"name": "missed-2"})))?;
    let state = if state["list"]["sync"] == "live" {
        state
    } else {
        app.wait_for("/list/sync", Equals("live".into()))?
    };
    assert!(!row(&state, "missed-1").is_null());
    // Each re-sync is a fresh streaming list (a watch from the current state), not a resume.
    let resyncs = fake
        .requests()
        .iter()
        .filter(|r| r.is_about(&pods) && r.status == Some(200))
        .filter(|r| r.query.contains("sendInitialEvents=true"))
        .count();
    assert!(
        resyncs >= 3,
        "the list was synced again ({resyncs} streaming lists)"
    );
    Ok(())
}

// covers UP-28
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_28_an_api_without_tables() -> Result<()> {
    let configmaps = Gvr::core("configmaps");
    let (cluster, app) = fake_app(Options {
        plain_only: vec![configmaps],
        ..Options::default()
    })?;
    let state = open_live(&app, "configmaps", 1)?;
    assert_eq!(
        state["list"]["columns"],
        json!(["Name", "Namespace", "Age"])
    );
    app.screenshot_themes("up28-plain-list")?;

    // metrics.k8s.io is polled, not watched.
    app.open_view("pods.metrics.k8s.io")?;
    let state = app.wait_for("/list/sync", Equals("polled".into()))?;
    assert_eq!(state["list"]["kind"], "pods.metrics.k8s.io");
    app.wait_for("/list/rows", Test::at_least(1))?;
    app.screenshot_themes("up28-metrics-polled")?;
    let fake = cluster.fake_server().expect("a fake server");
    let watched: Vec<String> = fake
        .requests()
        .iter()
        .filter(|r| r.path.starts_with("/apis/metrics.k8s.io/") && r.query.contains("watch="))
        .map(ToString::to_string)
        .collect();
    assert!(watched.is_empty(), "metrics were watched: {watched:?}");
    let polled = fake
        .requests()
        .iter()
        .filter(|r| r.path.starts_with("/apis/metrics.k8s.io/") && r.path.ends_with("/pods"))
        .count();
    assert!(polled >= 1, "metrics were never listed");
    Ok(())
}
