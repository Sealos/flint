//! Keyboard paths through the real app: moving around without the mouse (UP-33), the command
//! palette (UP-30) and the resource switcher (UP-31). The view tests cover each key; these prove
//! the keys reach the running app and the fake API server behind it.

use anyhow::{Result, bail};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{FakeApiServer, Options};
use serde_json::Value;

use crate::support::*;

/// Generated pods for the switcher: enough that a search going to the API would show.
const GENERATED_PODS: usize = 3000;
/// The pods in the fake server's fixtures.
const FIXTURE_PODS: usize = 17;

fn launch(cluster: &TestCluster) -> Result<App> {
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    Ok(app)
}

/// Presses Tab until `pane` has focus.
fn tab_to(app: &App, pane: &str) -> Result<()> {
    for _ in 0..6 {
        if app.state()?["focus"] == pane {
            return Ok(());
        }
        app.press_key("tab")?;
    }
    bail!("Tab never reached {pane}: {}", app.state()?["focus"])
}

/// The cluster under the sidebar's cursor.
fn sidebar_cursor(state: &Value) -> Option<&str> {
    state["clusters"]
        .as_array()?
        .iter()
        .find(|c| c["cursor"] == true)?["name"]
        .as_str()
}

/// Moves the navigation's cursor to `kind` with the arrows, opening the sections on the way.
fn arrow_to_kind(app: &App, kind: &str) -> Result<()> {
    for _ in 0..80 {
        let state = app.state()?;
        let sections = state["nav"]["sections"].as_array().cloned();
        let sections = sections.unwrap_or_default();
        let on_kind = sections.iter().any(|s| {
            s["entries"]
                .as_array()
                .is_some_and(|e| e.iter().any(|e| e["kind"] == kind && e["cursor"] == true))
        });
        if on_kind {
            return Ok(());
        }
        let on_closed = sections
            .iter()
            .any(|s| s["cursor"] == true && s["collapsed"] == true);
        app.press_key(if on_closed { "right" } else { "down" })?;
    }
    bail!("the arrows never reached {kind}")
}

/// The name of the row at `index`.
fn row_name(state: &Value, index: usize) -> Result<String> {
    match state["list"]["rows"][index]["name"].as_str() {
        Some(name) => Ok(name.to_owned()),
        None => bail!("no row {index}"),
    }
}

/// Waits until a watch on each of `plurals` is open.
fn wait_for_watches(fake: &FakeApiServer, plurals: &[&str]) -> Result<()> {
    for plural in plurals {
        let gvr = fake.gvr(plural);
        until(
            &format!("a {plural} watch"),
            || fake.open_watches(&gvr),
            |n| *n >= 1,
        )?;
    }
    Ok(())
}

/// Waits for the app's socket to go, which it removes as it quits. No request can reach the app
/// after that, so the `flint::Quit` in [`App::quit`] cannot be what ended it.
fn wait_for_socket_removed(app: &App) -> Result<()> {
    let socket = app.config_dir().join("automation.sock");
    until(
        "the app to quit and remove its socket",
        || socket.symlink_metadata().is_err(),
        |gone| *gone,
    )?;
    Ok(())
}

// covers UP-33
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_33_keyboard_only() -> Result<()> {
    let two = TwoClusters::start()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;

    // The sidebar: the arrows move and stop at the ends, and nothing connects yet.
    tab_to(&app, "sidebar")?;
    assert_eq!(sidebar_cursor(&app.state()?), Some(TwoClusters::A));
    for (key, wanted) in [
        ("down", TwoClusters::B),
        ("down", TwoClusters::B),
        ("up", TwoClusters::A),
        ("down", TwoClusters::B),
    ] {
        app.press_key(key)?;
        assert_eq!(sidebar_cursor(&app.state()?), Some(wanted), "after {key}");
    }
    assert!(two.a.arrivals().is_empty(), "moving contacted fake-a");
    assert!(two.b.arrivals().is_empty(), "moving contacted fake-b");

    // Enter connects the cluster under the cursor. The teardown fails the test if fake-a,
    // which the cursor only passed over, was ever contacted.
    app.mark_selected(TwoClusters::B);
    app.press_key("enter")?;
    app.wait_for("/active_cluster", Equals(TwoClusters::B.into()))?;
    app.wait_for(
        "/clusters",
        Any(serde_json::json!({"name": TwoClusters::B, "connection": "connected"})),
    )?;
    app.wait_for("/nav/status", Equals("ready".into()))?;

    // The navigation: Enter on a kind opens its list, which takes focus.
    tab_to(&app, "navigation")?;
    arrow_to_kind(&app, "pods")?;
    app.press_key("enter")?;
    app.wait_for("/list/kind", Equals("pods".into()))?;
    app.wait_for("/focus", Equals("list".into()))?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    let state = app.wait_for("/list/rows", Test::at_least(FIXTURE_PODS as u32))?;
    let (first, second) = (row_name(&state, 0)?, row_name(&state, 1)?);

    // The list: with nothing selected Down opens the first row, and the arrows follow.
    for (key, wanted) in [("down", &first), ("down", &second), ("up", &first)] {
        app.press_key(key)?;
        app.wait_for("/detail/name", Equals(wanted.as_str().into()))?;
    }
    app.press_key("escape")?;
    app.wait_for("/detail", Equals(Value::Null))?;
    app.wait_for("/focus", Equals("list".into()))?;

    // Enter with nothing selected opens nothing, and in the filter it only returns to the rows.
    app.press_key("enter")?;
    assert_eq!(app.state()?["detail"], Value::Null);
    app.press_key("secondary-f")?;
    app.wait_for("/focus", Equals("list_filter".into()))?;
    app.press_key("enter")?;
    app.wait_for("/focus", Equals("list".into()))?;
    assert_eq!(app.state()?["detail"], Value::Null);

    // Enter on the selected row keeps it open, and writes nothing.
    app.press_key("down")?;
    app.wait_for("/detail/name", Equals(first.as_str().into()))?;
    app.press_key("enter")?;
    app.wait_for("/detail/name", Equals(first.as_str().into()))?;
    app.wait_for("/focus", Equals("list".into()))?;
    assert!(two.b.writes().is_empty(), "Enter wrote to the cluster");

    // Cmd-Q itself ends the app (it may go before it answers); `quit` then checks exit code 0.
    let _ = app.press_key("secondary-q");
    wait_for_socket_removed(&app)?;
    app.quit()?;
    Ok(())
}

// covers UP-30
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_30_command_palette() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let app = launch(&cluster)?;
    open_live(&app, "pods", FIXTURE_PODS)?;
    app.wait_for("/focus", Equals("list".into()))?;

    // Esc closes it and gives focus back.
    app.press_key("secondary-shift-p")?;
    app.wait_for("/palette/mode", Equals("actions".into()))?;
    app.wait_for("/focus", Equals("palette".into()))?;
    app.press_key("escape")?;
    app.wait_for("/palette", Equals(Value::Null))?;
    app.wait_for("/focus", Equals("list".into()))?;

    // Part of a name, then Enter, runs the action where focus was.
    assert_eq!(app.state()?["list"]["wide"], false);
    app.press_key("secondary-shift-p")?;
    app.wait_for("/focus", Equals("palette".into()))?;
    app.type_text("toggle wide")?;
    app.wait_for(
        "/palette/rows/0/action",
        Equals("resource_list::ToggleWide".into()),
    )?;
    app.press_key("enter")?;
    app.wait_for("/palette", Equals(Value::Null))?;
    app.wait_for("/list/wide", Equals(true.into()))?;
    app.wait_for("/focus", Equals("list".into()))?;
    app.quit()?;
    Ok(())
}

// covers UP-31
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_31_jump_to_a_resource() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let pods = fake.gvr("pods");
    for i in 0..GENERATED_PODS {
        let name = format!("generated-{i:05}");
        let row = fake.new_row(
            &pods,
            Some("demo"),
            &name,
            &[("Ready", "1/1"), ("Status", "Running"), ("Restarts", "0")],
        );
        fake.set_row(&pods, row);
    }
    // On View, so that once these watches are open nothing else is still starting.
    let options = AppOptions::new(&cluster.kubeconfig).settings("watch_mode = \"on_view\"\n");
    let app = App::launch(options)?;
    connect(&app, &cluster.context)?;
    app.open_view("pods")?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    let total = GENERATED_PODS + FIXTURE_PODS;
    app.wait_for("/list/total", Equals(total.into()))?;
    wait_for_watches(fake, &["pods", "namespaces", "customresourcedefinitions"])?;

    // Searching thousands of objects sends the API server nothing.
    let before = fake.arrivals().len();
    app.press_key("secondary-p")?;
    app.wait_for("/palette/mode", Equals("resources".into()))?;
    app.type_text("generated-02999")?;
    app.wait_for(
        "/palette/rows/0/label",
        Equals("demo/generated-02999".into()),
    )?;
    for _ in 0.."generated-02999".len() {
        app.press_key("backspace")?;
    }
    app.wait_for("/palette/query", Equals("".into()))?;
    app.type_text("crash-loop")?;
    app.wait_for("/palette/rows/0/label", Equals("demo/crash-loop".into()))?;
    app.press_key("escape")?;
    app.wait_for("/palette", Equals(Value::Null))?;

    // Opening another list is the fence: by the time its watch is up, a request the search had
    // sent has arrived, and the only new requests are that list's.
    app.open_view("services")?;
    app.wait_for("/list/kind", Equals("services".into()))?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    wait_for_watches(fake, &["services"])?;
    let arrivals = fake.arrivals();
    let sent: Vec<_> = arrivals[before..]
        .iter()
        .filter(|r| !r.path.ends_with("/services"))
        .collect();
    assert!(sent.is_empty(), "the search sent requests: {sent:?}");

    // Enter on an object from another list switches to its list, with it selected and its
    // details shown. Pods is loaded before every kind is watched, so its watch is settled and
    // answers the switcher's read while the other kinds start, and stays once Services opens.
    app.open_view("pods")?;
    app.wait_for("/list/kind", Equals("pods".into()))?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    app.wait_for("/list/total", Equals(total.into()))?;
    app.dispatch("workspace::WatchAllKinds", Value::Null)?;
    wait_for_watches(fake, &["deployments"])?;
    app.open_view("services")?;
    app.wait_for("/list/kind", Equals("services".into()))?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    app.press_key("secondary-p")?;
    app.wait_for("/palette/mode", Equals("resources".into()))?;
    app.type_text("crash-loop")?;
    app.wait_for("/palette/rows/0/label", Equals("demo/crash-loop".into()))?;
    assert_eq!(app.state()?["list"]["kind"], "services");
    app.press_key("enter")?;
    app.wait_for("/palette", Equals(Value::Null))?;
    app.wait_for("/list/kind", Equals("pods".into()))?;
    app.wait_for("/detail/name", Equals("crash-loop".into()))?;
    app.wait_for(
        "/list/rows",
        Any(serde_json::json!({"name": "crash-loop", "selected": true})),
    )?;

    // Enter on a kind opens its list.
    app.press_key("secondary-p")?;
    app.wait_for("/palette/mode", Equals("resources".into()))?;
    app.type_text("deployments")?;
    let state = app.wait_for("/palette/rows/0/kind", Equals("deployments.apps".into()))?;
    let detail = state["palette"]["rows"][0]["detail"].as_str();
    assert!(
        detail.is_some_and(|d| d.starts_with("Kind")),
        "the first row is not the kind: {detail:?}"
    );
    app.press_key("enter")?;
    app.wait_for("/palette", Equals(Value::Null))?;
    app.wait_for("/list/kind", Equals("deployments.apps".into()))?;
    app.quit()?;
    Ok(())
}
