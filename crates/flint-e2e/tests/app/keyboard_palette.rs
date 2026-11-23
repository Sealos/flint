//! The keys of UP-33, UP-30 and UP-31 that `m2/keyboard.rs` leaves to the view tests, pressed in
//! the running app: the panes in reverse, sections, Go Back and Back to List; the palette's
//! selection and argument steps; the switcher's arrows and swap; and a write reached from the
//! palette and answered from the keyboard.

use anyhow::{Result, bail};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::Options;
use http::Method;
use serde_json::{Value, json};

use crate::support::*;

/// The pods in the fake server's fixtures.
const FIXTURE_PODS: usize = 17;
/// The deployments in the fake server's fixtures.
const FIXTURE_DEPLOYMENTS: usize = 5;
const WEB: &str = "/apis/apps/v1/namespaces/demo/deployments/web";

/// A window wide enough to show the list and the detail side by side, whatever the display's
/// default gave: below the split, a row's detail shows alone and takes focus.
const SPLIT_WIDTH: f64 = 1280.0;

fn launch(cluster: &TestCluster) -> Result<App> {
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    Ok(app)
}

fn widen(app: &App) -> Result<()> {
    let applied = app.request(
        "resize_window",
        json!({"width": SPLIT_WIDTH, "height": 800}),
    )?;
    if applied["width"].as_f64() != Some(SPLIT_WIDTH) {
        bail!("the window did not become {SPLIT_WIDTH} wide: {applied}");
    }
    Ok(())
}

/// Presses `key` and waits until the state at `pointer` is `wanted`.
fn press(app: &App, key: &str, pointer: &str, wanted: impl Into<Value>) -> Result<()> {
    app.press_key(key)?;
    app.wait_for(pointer, Equals(wanted.into()))?;
    Ok(())
}

/// Opens the palette (`secondary-shift-p`) or the switcher (`secondary-p`) with focus in it.
fn open_palette(app: &App, key: &str, mode: &str) -> Result<()> {
    press(app, key, "/palette/mode", mode)?;
    app.wait_for("/focus", Equals("palette".into()))?;
    Ok(())
}

/// The navigation section holding `kind`.
fn section_of<'a>(state: &'a Value, kind: &str) -> Option<&'a Value> {
    state["nav"]["sections"].as_array()?.iter().find(|s| {
        s["entries"]
            .as_array()
            .is_some_and(|e| e.iter().any(|e| e["kind"] == kind))
    })
}

/// Moves the navigation's cursor to `kind` with the arrows, opening sections on the way.
fn arrow_to_kind(app: &App, kind: &str) -> Result<()> {
    for _ in 0..80 {
        let state = app.state()?;
        let on_kind = section_of(&state, kind).is_some_and(|s| {
            s["entries"]
                .as_array()
                .is_some_and(|e| e.iter().any(|e| e["kind"] == kind && e["cursor"] == true))
        });
        if on_kind {
            return Ok(());
        }
        let on_closed = state["nav"]["sections"].as_array().is_some_and(|all| {
            all.iter()
                .any(|s| s["cursor"] == true && s["collapsed"] == true)
        });
        app.press_key(if on_closed { "right" } else { "down" })?;
    }
    bail!("the arrows never reached {kind}")
}

// covers UP-33
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_33_panes_sections_and_go_back_by_key() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch(&cluster)?;
    let state = open_live(&app, "pods", FIXTURE_PODS)?;
    widen(&app)?;
    app.wait_for("/focus", Equals("list".into()))?;
    let first = row_names(&state).remove(0);
    press(&app, "down", "/detail/name", first.as_str())?;
    app.wait_for("/focus", Equals("list".into()))?;

    // Shift-Tab walks the panes backwards and Tab forwards.
    press(&app, "shift-tab", "/focus", "navigation")?;
    press(&app, "shift-tab", "/focus", "sidebar")?;
    press(&app, "tab", "/focus", "navigation")?;

    // Left closes the section of the kind under the cursor, and Right opens it again.
    arrow_to_kind(&app, "pods")?;
    let state = app.state()?;
    let title = section_of(&state, "pods").expect("a section")["title"].clone();
    let section = |state: &Value| {
        state["nav"]["sections"]
            .as_array()
            .and_then(|all| all.iter().find(|s| s["title"] == title).cloned())
    };
    app.press_key("left")?;
    let closed = until(
        "the section to close",
        || section(&app.state().unwrap_or_default()),
        |s| s.as_ref().is_some_and(|s| s["collapsed"] == true),
    )?;
    assert_eq!(closed.expect("a section")["cursor"], true);
    app.press_key("right")?;
    until(
        "the section to open",
        || section(&app.state().unwrap_or_default()),
        |s| s.as_ref().is_some_and(|s| s["collapsed"] == false),
    )?;

    // Enter opens another kind, and Cmd-[ returns to the kind and object shown before.
    arrow_to_kind(&app, "services")?;
    press(&app, "enter", "/list/kind", "services")?;
    app.wait_for("/focus", Equals("list".into()))?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    press(&app, "secondary-[", "/list/kind", "pods")?;
    app.wait_for("/detail/name", Equals(first.as_str().into()))?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    app.wait_for("/focus", Equals("list".into()))?;

    // Esc and Down leave the text filter for the rows, and keep what it holds.
    for key in ["escape", "down"] {
        press(&app, "secondary-f", "/focus", "list_filter")?;
        if app.state()?["list"]["filter"] == "" {
            app.type_text("crash")?;
            app.wait_for("/list/filter", Equals("crash".into()))?;
        }
        press(&app, key, "/focus", "list")?;
        assert_eq!(app.state()?["list"]["filter"], "crash", "after {key}");
    }
    press(&app, "secondary-f", "/focus", "list_filter")?;
    for _ in 0.."crash".len() {
        app.press_key("backspace")?;
    }
    app.wait_for("/list/filter", Equals("".into()))?;
    press(&app, "escape", "/focus", "list")?;
    app.wait_for("/detail/name", Equals(first.as_str().into()))?;

    // In a narrow window Cmd-Shift-B returns to the list, and Enter brings the detail back.
    app.request("resize_window", json!({"width": 500, "height": 600}))?;
    press(&app, "secondary-shift-b", "/focus", "list")?;
    app.wait_for("/detail/name", Equals(first.as_str().into()))?;
    press(&app, "enter", "/focus", "detail")?;
    app.wait_for("/detail/name", Equals(first.as_str().into()))?;

    app.quit()?;
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    Ok(())
}

// covers UP-30
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_30_palette_selection_and_steps_by_key() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch(&cluster)?;
    open_live(&app, "pods", FIXTURE_PODS)?;
    app.wait_for("/focus", Equals("list".into()))?;

    // The arrows and Ctrl-N/P move the selection, and it wraps at the ends.
    open_palette(&app, "secondary-shift-p", "actions")?;
    app.type_text("open")?;
    app.wait_for("/palette/query", Equals("open".into()))?;
    let state = app.wait_for("/palette/selected", Equals(0.into()))?;
    let last = state["palette"]["total"].as_u64().expect("a total") - 1;
    assert!(last >= 2, "too few rows to move through: {last}");
    for (key, wanted) in [
        ("down", 1),
        ("ctrl-n", 2),
        ("ctrl-p", 1),
        ("up", 0),
        ("up", last),
        ("down", 0),
    ] {
        press(&app, key, "/palette/selected", wanted)?;
    }
    press(&app, "escape", "/palette", Value::Null)?;
    app.wait_for("/focus", Equals("list".into()))?;

    // An action that needs a kind asks for it; Backspace in the empty field and Esc go back.
    open_palette(&app, "secondary-shift-p", "actions")?;
    app.type_text("open kind")?;
    app.wait_for("/palette/rows/0/label", Equals("Open Kind…".into()))?;
    press(&app, "enter", "/palette/argument", "kind")?;
    app.wait_for("/palette/step", Equals("Open Kind…".into()))?;
    app.type_text("s")?;
    app.wait_for("/palette/query", Equals("s".into()))?;
    press(&app, "backspace", "/palette/query", "")?;
    assert_eq!(app.state()?["palette"]["argument"], "kind");
    press(&app, "backspace", "/palette/step", Value::Null)?;
    app.wait_for("/palette/query", Equals("open kind".into()))?;
    press(&app, "enter", "/palette/argument", "kind")?;
    press(&app, "escape", "/palette/step", Value::Null)?;

    // Forward again: the pick runs the action, and focus lands on the list it opened.
    press(&app, "enter", "/palette/argument", "kind")?;
    app.type_text("services")?;
    app.wait_for("/palette/rows/0/label", Equals("Services".into()))?;
    press(&app, "enter", "/palette", Value::Null)?;
    app.wait_for("/list/kind", Equals("services".into()))?;
    app.wait_for("/focus", Equals("list".into()))?;

    // A scattered query listing only Quit selects nothing, so Enter leaves the app running.
    open_palette(&app, "secondary-shift-p", "actions")?;
    app.type_text("qtf")?;
    app.wait_for("/palette/rows/0/label", Equals("Quit Flint".into()))?;
    let state = app.wait_for("/palette/total", Equals(1.into()))?;
    assert_eq!(state["palette"]["selected"], Value::Null);
    app.press_key("enter")?;
    let state = app.state()?;
    assert_eq!(state["palette"]["query"], "qtf", "Enter closed the palette");
    press(&app, "escape", "/palette", Value::Null)?;

    // `quit` fails unless this app was still running to be asked.
    app.quit()?;
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    Ok(())
}

// covers UP-30, UP-53: a delete run from the palette and answered only with keys. Enter cancels
// wherever focus is, and only Space on OK sends the one request.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_30_palette_delete_is_confirmed_by_key() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let options = AppOptions::new(&cluster.kubeconfig).settings("protect_local_clusters = true\n");
    let app = App::launch(options)?;
    connect(&app, &cluster.context)?;
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
    open_live(&app, "deployments", FIXTURE_DEPLOYMENTS)?;
    widen(&app)?;
    app.wait_for("/focus", Equals("list".into()))?;

    // Down from the filter returns to the rows, and Down there opens the one row left.
    press(&app, "secondary-f", "/focus", "list_filter")?;
    app.type_text("web")?;
    app.wait_for("/list/filter", Equals("web".into()))?;
    app.wait_for("/list/rows/0/name", Equals("web".into()))?;
    press(&app, "down", "/focus", "list")?;
    if app.state()?["detail"]["name"] != "web" {
        press(&app, "down", "/detail/name", "web")?;
    }
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;

    let open_delete = || -> Result<()> {
        open_palette(&app, "secondary-shift-p", "actions")?;
        app.type_text("delete selected")?;
        app.wait_for(
            "/palette/rows/0/action",
            Equals("workspace::DeleteObject".into()),
        )?;
        app.press_key("enter")?;
        let state = app.wait_for("/dialog/title", Exists(true))?;
        let text = state["dialog"]["text"].as_str().unwrap_or_default();
        assert!(text.contains("web"), "the dialog does not name web: {text}");
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
        Ok(())
    };

    // Each of these closes the dialog and sends nothing.
    for keys in [
        &["escape"][..],
        &["enter"],
        &["tab", "enter"],
        &["shift-tab", "enter"],
    ] {
        open_delete()?;
        for key in keys {
            app.press_key(key)?;
        }
        app.wait_for("/dialog", Equals(Value::Null))?;
        app.wait_for("/focus", Equals("list".into()))?;
        assert!(fake.writes().is_empty(), "{keys:?}: {:?}", fake.writes());
    }

    // Shift-Tab reaches OK, and Space on it sends exactly one delete, of this object.
    open_delete()?;
    app.press_key("shift-tab")?;
    press(&app, "space", "/dialog", Value::Null)?;
    let writes = until(
        "the delete's answer",
        || fake.writes(),
        |writes| !writes.is_empty() && writes.iter().all(|w| w.status.is_some()),
    )?;
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert_eq!(writes[0].method, Method::DELETE);
    assert_eq!(writes[0].path, WEB);
    assert!(!writes[0].is_dry_run(), "{:?}", writes[0]);
    assert!(
        writes[0].status.is_some_and(|c| (200..300).contains(&c)),
        "{:?}",
        writes[0]
    );

    // After quitting, so a repeated write would have arrived.
    app.quit()?;
    assert_eq!(fake.writes().len(), 1, "{:?}", fake.writes());
    Ok(())
}

// covers UP-31
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_31_switcher_keys_and_a_filtered_object() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = launch(&cluster)?;
    open_live(&app, "pods", FIXTURE_PODS)?;
    app.wait_for("/focus", Equals("list".into()))?;

    // The arrows move the selection.
    open_palette(&app, "secondary-p", "resources")?;
    app.wait_for("/palette/rows/1/label", Exists(true))?;
    app.wait_for("/palette/selected", Equals(0.into()))?;
    press(&app, "down", "/palette/selected", 1)?;
    press(&app, "up", "/palette/selected", 0)?;

    // Cmd-Shift-P swaps it for the palette, Cmd-P swaps back, and Esc returns focus.
    press(&app, "secondary-shift-p", "/palette/mode", "actions")?;
    press(&app, "secondary-p", "/palette/mode", "resources")?;
    press(&app, "escape", "/palette", Value::Null)?;
    app.wait_for("/focus", Equals("list".into()))?;

    // An object the namespace filter hides: the filter stays and the list says so.
    app.dispatch(
        "workspace::SetNamespaces",
        json!({"namespaces": ["kube-system"]}),
    )?;
    app.wait_for("/header/namespaces", Equals(json!(["kube-system"])))?;
    open_palette(&app, "secondary-p", "resources")?;
    app.type_text("crash-loop")?;
    app.wait_for("/palette/rows/0/label", Equals("demo/crash-loop".into()))?;
    press(&app, "enter", "/palette", Value::Null)?;
    let state = app.wait_for("/list/banners", Any(json!({"id": "list-filtered-out"})))?;
    let banners = state["list"]["banners"].as_array().expect("banners");
    let banner = banners
        .iter()
        .find(|b| b["id"] == "list-filtered-out")
        .expect("the banner");
    let text = banner["text"].as_str().unwrap_or_default();
    assert!(text.contains("crash-loop"), "{text}");
    assert_eq!(state["header"]["namespaces"], json!(["kube-system"]));

    app.quit()?;
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    Ok(())
}
