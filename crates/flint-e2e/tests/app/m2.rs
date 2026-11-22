//! M2 user paths: logs in the dock (docs/USER-PATHS.md).
//!
//! Shells (UP-41) and sessions across navigation (UP-42, UP-24) are in `m2_shell.rs`. Pause,
//! match navigation, wrap, copy and save are view tests (`log_tests.rs`); these prove the seams:
//! the action, the log request, the streamed line and the status the tab shows.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use flint_e2e::app::{App, AppError, AppOptions, WAIT};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{ContainerLog, FakeApiServer, FakePod, Fault, Gvr, Options};
use serde_json::{Value, json};

use crate::support::*;

mod keyboard;

/// The running pod whose log the test reads: one container, `nginx`.
const POD: &str = "web-69c6f74b8b-qv4xv";
/// The other pod of Deployment `web`.
const OTHER: &str = "web-69c6f74b8b-x4gss";

/// The tabs in the state's dock.
fn tabs(state: &Value) -> &[Value] {
    state["dock"]["tabs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// The tab with id `id` in the state's dock.
fn tab(state: &Value, id: u64) -> &Value {
    tabs(state)
        .iter()
        .find(|t| t["id"] == id)
        .unwrap_or(&Value::Null)
}

/// Where tab `id` is in the dock's list now.
fn tab_index(app: &App, id: u64) -> Result<usize> {
    let state = app.state()?;
    tabs(&state)
        .iter()
        .position(|t| t["id"] == id)
        .with_context(|| format!("no tab {id} in {}", state["dock"]))
}

/// A failed wait on tab `id`, with the channel's words and the whole tab as it is now: the
/// channel's own message and state dump are cut short before they reach one tab's lines.
fn tab_failure(app: &App, id: u64, wanted: &str, error: AppError) -> anyhow::Error {
    let now = app
        .state()
        .map(|state| serde_json::to_string_pretty(tab(&state, id)).unwrap_or_default())
        .unwrap_or_else(|e| format!("(no state: {e})"));
    anyhow!(
        "tab {id} never had {wanted}: {}: {}\n---- tab {id} now ----\n{now}",
        error.code,
        error.message
    )
}

/// The JSON pointers (from the tab) and values of every leaf of `fields`.
fn leaves(prefix: &str, fields: &Value, out: &mut Vec<(String, Value)>) {
    match fields {
        Value::Object(map) => {
            for (key, value) in map {
                leaves(&format!("{prefix}/{key}"), value, out);
            }
        }
        leaf => out.push((prefix.to_owned(), leaf.clone())),
    }
}

/// Equality with numbers compared by value, as `wait_for` compares them.
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Waits until tab `id` has every field of `fields` at once (nested objects field by field),
/// and returns the state. Each field is waited for at its own pointer, so a timeout names it;
/// the failure carries the tab as it is.
fn wait_tab_is(app: &App, id: u64, fields: Value) -> Result<Value> {
    let mut wanted = Vec::new();
    leaves("", &fields, &mut wanted);
    let deadline = Instant::now() + WAIT;
    loop {
        let index = tab_index(app, id)?;
        for (pointer, value) in &wanted {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(200));
            app.wait_for_within(
                &format!("/dock/tabs/{index}{pointer}"),
                Equals(value.clone()),
                left,
            )
            .map_err(|e| tab_failure(app, id, &format!("{pointer} = {value}"), e))?;
        }
        // The fields came one at a time: check they hold together.
        let state = app.state()?;
        let shown = tab(&state, id);
        if wanted
            .iter()
            .all(|(pointer, value)| shown.pointer(pointer).is_some_and(|got| same(got, value)))
        {
            return Ok(state);
        }
        if Instant::now() > deadline {
            bail!(
                "tab {id} never had {fields} at once\n---- tab {id} now ----\n{}",
                serde_json::to_string_pretty(shown).unwrap_or_default()
            );
        }
    }
}

/// Waits until `rest` (`/logs/last_lines`) of tab `id` passes `test`, and returns the state.
/// The pointer is taken from the tabs listed now; a failure carries the tab as it is.
fn wait_tab(app: &App, id: u64, rest: &str, test: Test) -> Result<Value> {
    let index = tab_index(app, id)?;
    app.wait_for(&format!("/dock/tabs/{index}{rest}"), test.clone())
        .map_err(|e| tab_failure(app, id, &format!("{rest} {test:?}"), e))
}

/// Waits until tab `id` shows a line with `text` among its newest lines, and returns the state.
fn wait_line(app: &App, id: u64, text: &str) -> Result<Value> {
    wait_tab(app, id, "/logs/last_lines", Any(json!({ "text": text })))
}

/// The newest lines of a tab, oldest first.
fn lines(tab: &Value) -> &[Value] {
    tab["logs"]["last_lines"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// The texts of a tab's newest lines, oldest first.
fn texts(tab: &Value) -> Vec<String> {
    lines(tab)
        .iter()
        .filter_map(|l| l["text"].as_str().map(str::to_owned))
        .collect()
}

/// Asserts that no line of the tab shows twice.
fn assert_no_repeats(tab: &Value) {
    let shown = texts(tab);
    let mut unique = shown.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), shown.len(), "a line shown twice: {shown:#?}");
}

/// Dispatches `action`, which opens a new tab, and returns the new tab's id once it is shown.
fn open_tab(app: &App, action: &str, args: Value) -> Result<u64> {
    let before: Vec<Value> = tabs(&app.state()?)
        .iter()
        .map(|t| t["id"].clone())
        .collect();
    app.dispatch(action, args)?;
    let state = app.wait_for("/dock/tabs", Test::at_least(before.len() as u32 + 1))?;
    let id = state["dock"]["active"]
        .as_u64()
        .context("a new tab is shown")?;
    assert!(
        !before.contains(&id.into()),
        "the new tab is the one shown: {}",
        state["dock"]
    );
    Ok(id)
}

/// Selects the object `name` in `demo` in the list shown (once its row is listed), and waits
/// until the detail pane has loaded it.
fn show(app: &App, name: &str) -> Result<Value> {
    app.wait_for("/list/rows", Any(json!({ "name": name })))?;
    app.select_row(Some("demo"), name)?;
    app.wait_for("/detail/name", Equals(name.into()))?;
    Ok(app.wait_for("/detail/fetch", Equals("loaded".into()))?)
}

/// A workload tab's `streams.each`: one `nginx` stream per pod, in pod name order.
fn each(streams: &[(&str, &str)]) -> Value {
    streams
        .iter()
        .map(|(pod, state)| json!({"pod": pod, "container": "nginx", "state": state}))
        .collect()
}

/// The fake's pods, for faults.
fn pods(fake: &FakeApiServer) -> Gvr {
    fake.gvr("pods")
}

/// The log requests for one pod so far.
fn log_requests_of(fake: &FakeApiServer, pod: &str) -> usize {
    let path = format!("/api/v1/namespaces/demo/pods/{pod}/log");
    fake.log_requests()
        .iter()
        .filter(|r| r.path == path)
        .count()
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

    // Logs opens a tab that says it is connecting (the server holds the first request back a
    // moment), then streams the pod's log: its canned lines, then a line appended now.
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    fake.inject(
        Fault::delay(Duration::from_secs(2))
            .on(&pods(fake))
            .named(POD)
            .subresource("log")
            .times(1),
    );
    let id = open_tab(&app, "detail::OpenLogs", Value::Null)?;
    let state = wait_tab_is(
        &app,
        id,
        json!({"kind": "pod_logs", "status": {"state": "connecting"}, "status_text": "Connecting…"}),
    )?;
    assert!(
        lines(tab(&state, id)).is_empty(),
        "no line before the log answers: {}",
        tab(&state, id)
    );
    app.screenshot("up40-connecting")?;
    let state = wait_tab_is(&app, id, json!({"status": {"state": "streaming"}}))?;
    let shown = tab(&state, id);
    assert!(
        shown["title"].as_str().is_some_and(|t| t.starts_with(POD)),
        "{shown}"
    );
    assert_eq!(shown["cluster"]["context"], cluster.context.as_str());
    assert_eq!(state["dock"]["visible"], true);
    assert_eq!(state["focus"], "log");
    wait_tab(&app, id, "/logs/lines", Test::at_least(3))?;
    until(
        "one open log stream",
        || fake.open_log_streams(),
        |n| *n == 1,
    )?;
    let log = ContainerLog::new("demo", POD);
    // The stream may still be catching up when the line is written; it gets it either way.
    let sent = fake.append_log(&log, "ERROR e2e line reached the tab");
    let state = wait_tab(
        &app,
        id,
        "/logs/last_lines",
        Any(json!({"text": "ERROR e2e line reached the tab", "severity": "error"})),
    )?;
    assert_eq!(sent, 1, "one stream got the line");
    assert_eq!(tab(&state, id)["logs"]["follows"], true);
    app.screenshot_themes("up40-pod-logs")?;

    // Logs again for the same container shows the open tab (the dock was hidden): no second
    // tab, no second request. A line written after it arrives on the one stream, so any
    // request the reopen made would have been seen by then.
    app.dispatch("dock::ToggleDock", Value::Null)?;
    app.wait_for("/dock/visible", Equals(false.into()))?;
    app.dispatch("detail::OpenLogs", Value::Null)?;
    app.wait_for("/dock/visible", Equals(true.into()))?;
    let state = app.wait_for("/dock/active", Equals(id.into()))?;
    assert_eq!(tabs(&state).len(), 1, "one tab: {}", state["dock"]);
    fake.append_log(&log, "after opening it again");
    wait_line(&app, id, "after opening it again")?;
    assert_eq!(
        fake.log_requests().len(),
        1,
        "the open tab is shown again, with no new request"
    );
    assert_eq!(fake.open_log_streams(), 1, "one stream");

    // The connection breaks mid-stream, just after lines that share a second. The server holds
    // the reconnect back a moment, so the tab is seen saying so. Then it streams again from the
    // last line's second, and the lines of that second, which the server sends again, are not
    // shown twice.
    fake.append_log(&log, "same second one\nsame second two\nsame second three");
    wait_line(&app, id, "same second three")?;
    fake.inject(
        Fault::delay(Duration::from_secs(2))
            .on(&pods(fake))
            .named(POD)
            .subresource("log")
            .times(1),
    );
    assert_eq!(fake.drop_log_streams(&log), 1);
    let state = wait_tab_is(
        &app,
        id,
        json!({"status": {"state": "reconnecting", "attempt": 1, "back_off": false}}),
    )?;
    let reconnecting = tab(&state, id);
    assert!(
        reconnecting["status_text"]
            .as_str()
            .is_some_and(|t| t.starts_with("Reconnecting (attempt 1)")),
        "{reconnecting}"
    );
    app.screenshot("up40-reconnecting")?;
    wait_tab_is(&app, id, json!({"status": {"state": "streaming"}}))?;
    until(
        "the stream open again",
        || fake.open_log_streams(),
        |n| *n == 1,
    )?;
    fake.append_log(&log, "after the reconnect");
    let state = wait_line(&app, id, "after the reconnect")?;
    let shown = tab(&state, id);
    assert_no_repeats(shown);
    let shown = texts(shown);
    let at = |text: &str| {
        shown
            .iter()
            .position(|t| t == text)
            .unwrap_or_else(|| panic!("{text:?} shown: {shown:#?}"))
    };
    assert!(
        at("same second three") < at("after the reconnect"),
        "the lines in the order written: {shown:#?}"
    );
    let requests = fake.log_requests();
    assert_eq!(requests.len(), 2, "one reconnect: {requests:#?}");
    let resume = requests[1].parsed_query();
    assert!(
        resume.follow && resume.since_time.is_some() && resume.tail_lines.is_none(),
        "the reconnect resumes from the last line's time: {resume:?}"
    );

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
        2,
        "no request after the tab closed"
    );
    Ok(())
}

// covers UP-43, UP-40
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_43_spot_errors_in_a_log_stream() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    open_live(&app, "pods", 17)?;
    show(&app, POD)?;
    let id = open_tab(&app, "detail::OpenLogs", Value::Null)?;
    wait_tab_is(&app, id, json!({"status": {"state": "streaming"}}))?;
    until(
        "one open log stream",
        || fake.open_log_streams(),
        |n| *n == 1,
    )?;

    // Each line carries its severity: whole words, any case.
    let log = ContainerLog::new("demo", POD);
    let expected = [
        ("ERROR connection to db lost", "error"),
        ("WARN slow query took 3s", "warning"),
        ("panic: runtime error: index out of range", "error"),
        ("java.lang.FooException: bad input", "error"),
        ("Warning: disk 91% full", "warning"),
        ("FATAL cannot continue", "error"),
        ("GET /errors/warnings 200", "normal"),
    ];
    let text: Vec<&str> = expected.iter().map(|(t, _)| *t).collect();
    fake.append_log(&log, &text.join("\n"));
    let state = wait_line(&app, id, "GET /errors/warnings 200")?;
    let shown = lines(tab(&state, id));
    for (text, severity) in expected {
        let line = shown
            .iter()
            .find(|l| l["text"] == text)
            .unwrap_or_else(|| panic!("{text:?} in {shown:#?}"));
        assert_eq!(line["severity"], severity, "{line}");
    }
    app.screenshot_themes("up43-severity")?;

    // Timestamps come and go without reading the log again.
    let requests = fake.log_requests().len();
    let shown = tab(&state, id)["logs"]["timestamps"]
        .as_bool()
        .context("the timestamps toggle")?;
    app.dispatch("logs::ToggleTimestamps", Value::Null)?;
    wait_tab_is(&app, id, json!({"logs": {"timestamps": !shown}}))?;
    app.screenshot_themes(if shown {
        "up43-without-timestamps"
    } else {
        "up43-timestamps"
    })?;
    app.dispatch("logs::ToggleTimestamps", Value::Null)?;
    wait_tab_is(&app, id, json!({"logs": {"timestamps": shown}}))?;
    // A line after the toggles still arrives on the same stream.
    fake.append_log(&log, "after the toggles");
    wait_line(&app, id, "after the toggles")?;
    assert_eq!(
        fake.log_requests().len(),
        requests,
        "toggling timestamps sent no log request"
    );
    assert_eq!(
        fake.open_log_streams(),
        1,
        "the one stream is still open after the toggles"
    );

    // A search keeps up with the stream: new matching lines are counted as they arrive.
    app.dispatch("logs::FocusSearch", Value::Null)?;
    app.wait_for("/focus", Equals("log_search".into()))?;
    app.type_text("boom")?;
    wait_tab_is(
        &app,
        id,
        json!({"logs": {"search": {"query": "boom", "matches": 0, "searching": false}}}),
    )?;
    fake.append_log(&log, "ERROR first boom");
    wait_tab_is(&app, id, json!({"logs": {"search": {"matches": 1}}}))?;
    fake.append_log(&log, "nothing to see\nWARN second BOOM");
    wait_tab_is(&app, id, json!({"logs": {"search": {"matches": 2}}}))?;
    wait_line(&app, id, "WARN second BOOM")?;
    app.screenshot_themes("up40-search")?;
    assert_eq!(fake.log_requests().len(), requests, "searching sent none");
    Ok(())
}

// covers UP-40
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_40_pick_a_container_and_read_the_previous_instance() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    // A pod with two containers, whose first crashed once and runs again.
    let pod = "multi";
    fake.add_pod(
        FakePod::new("demo", pod, &["app", "sidecar"])
            .label("app", pod)
            .restarted("app"),
    );
    let app_log = ContainerLog::new("demo", pod).container("app");
    let sidecar_log = ContainerLog::new("demo", pod).container("sidecar");
    fake.set_container_log(
        &app_log.clone().previous(),
        "starting up\nERROR out of memory\npanic: the previous instance's last words",
    );
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    let state = open_live(&app, "pods", 18)?;
    let row_id = state["list"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == pod))
        .and_then(|r| r["id"].as_str())
        .expect("the pod's row")
        .to_owned();

    // The row's menu picks a container: Logs lists both, the default marked; Previous Logs
    // lists the one that restarted.
    app.right_click(&row_id)?;
    app.wait_for("/detail/name", Equals(pod.into()))?;
    let state = app.wait_for(
        "/menu/entries",
        Any(json!({"label": "Logs", "enabled": true, "submenu": [
            {"label": "app (default)", "enabled": true},
            {"label": "sidecar", "enabled": true},
        ]})),
    )?;
    let previous = state["menu"]["entries"]
        .as_array()
        .and_then(|e| e.iter().find(|e| e["label"] == "Previous Logs"))
        .expect("Previous Logs");
    assert_eq!(
        previous["submenu"],
        json!([{"label": "app (default)", "enabled": true}]),
        "{previous}"
    );
    app.screenshot_themes("up40-container-picker")?;
    app.press_key("escape")?;
    app.wait_for("/menu", Equals(Value::Null))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;

    // Logs opens the default container. (The picker's entries dispatch these actions.)
    let first = open_tab(&app, "detail::OpenLogs", Value::Null)?;
    let state = wait_tab_is(
        &app,
        first,
        json!({"kind": "pod_logs", "status": {"state": "streaming"}}),
    )?;
    assert_eq!(tab(&state, first)["title"], "multi · app");
    until(
        "the app container's stream",
        || fake.open_log_streams_of(&app_log),
        |n| *n == 1,
    )?;
    fake.append_log(&app_log, "app line");
    wait_line(&app, first, "app line")?;

    // The other container opens in a tab of its own, reading only its own log.
    let second = open_tab(
        &app,
        "detail::OpenContainerLogs",
        json!({"container": "sidecar"}),
    )?;
    let state = wait_tab_is(
        &app,
        second,
        json!({"kind": "pod_logs", "status": {"state": "streaming"}}),
    )?;
    assert_eq!(tab(&state, second)["title"], "multi · sidecar");
    until(
        "the sidecar's stream",
        || fake.open_log_streams_of(&sidecar_log),
        |n| *n == 1,
    )?;
    fake.append_log(&sidecar_log, "sidecar line");
    let state = wait_line(&app, second, "sidecar line")?;
    assert!(
        !texts(tab(&state, second)).contains(&"app line".to_owned()),
        "the sidecar's tab shows only its own lines: {}",
        tab(&state, second)
    );
    assert!(
        !texts(tab(&state, first)).contains(&"sidecar line".to_owned()),
        "the app's tab shows only its own lines: {}",
        tab(&state, first)
    );
    let containers: Vec<Option<String>> = fake
        .log_requests()
        .iter()
        .map(|r| r.parsed_query().container)
        .collect();
    assert_eq!(
        containers,
        [Some("app".to_owned()), Some("sidecar".to_owned())]
    );
    app.screenshot_themes("up40-second-container")?;

    // Asking for the first container again shows its tab (the sidecar's was shown), with no
    // new request: a line written after it arrives on the stream already open.
    app.dispatch("detail::OpenContainerLogs", json!({"container": "app"}))?;
    let state = app.wait_for("/dock/active", Equals(first.into()))?;
    assert_eq!(tabs(&state).len(), 2, "no third tab: {}", state["dock"]);
    fake.append_log(&app_log, "app line again");
    wait_line(&app, first, "app line again")?;
    assert_eq!(
        fake.log_requests().len(),
        2,
        "the open tab is shown again, with no new request"
    );

    // Previous Logs reads the crashed instance to its end, then ends: nothing more will come.
    let old = open_tab(&app, "detail::OpenPreviousLogs", Value::Null)?;
    let state = wait_tab_is(
        &app,
        old,
        json!({"kind": "previous_logs",
               "status": {"state": "ended", "reason": null, "pod_deleted": false},
               "status_text": "Ended"}),
    )?;
    let shown = tab(&state, old);
    assert_eq!(shown["title"], "multi · app (previous)");
    assert_eq!(
        texts(shown),
        [
            "starting up",
            "ERROR out of memory",
            "panic: the previous instance's last words"
        ]
    );
    // Nothing more will come, so there is nothing to pause.
    let refused = app
        .dispatch("logs::TogglePause", Value::Null)
        .expect_err("an ended tab can't pause");
    assert_eq!(refused.code, "not_available", "{refused}");
    let state = app.state()?;
    assert_eq!(
        tab(&state, old)["logs"]["paused"],
        false,
        "{}",
        tab(&state, old)
    );
    let request = fake
        .log_requests()
        .pop()
        .expect("the previous log's request");
    let query = request.parsed_query();
    assert!(
        query.previous && query.container.as_deref() == Some("app"),
        "{query:?}"
    );
    app.screenshot_themes("up40-previous-logs")?;
    // The live tabs read on.
    assert_eq!(fake.open_log_streams(), 2);
    Ok(())
}

// covers UP-40
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_40_log_tabs_say_why_they_stopped() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    // `talker` gets deleted, `looper` crash-loops, `locked` refuses its log, and `rotated`
    // restarted but its previous instance's log is gone: the kubelet answers with a notice
    // instead of the log.
    for name in ["talker", "looper", "locked"] {
        fake.add_pod(FakePod::new("demo", name, &["app"]).label("app", name));
    }
    fake.add_pod(
        FakePod::new("demo", "rotated", &["app"])
            .label("app", "rotated")
            .restarted("app"),
    );
    fake.set_log_notice(
        &ContainerLog::new("demo", "rotated").previous(),
        "unable to retrieve container logs for containerd://0123456789abcdef",
    );
    fake.inject(
        Fault::forbidden()
            .on(&pods(fake))
            .named("locked")
            .subresource("log"),
    );
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    open_live(&app, "pods", 21)?;

    // The pod is deleted while its log streams: the tab reads its last line, then ends, saying
    // so as an ordinary end rather than an error.
    show(&app, "talker")?;
    let talker = open_tab(&app, "detail::OpenLogs", Value::Null)?;
    wait_tab_is(&app, talker, json!({"status": {"state": "streaming"}}))?;
    until("talker's stream", || fake.open_log_streams(), |n| *n == 1)?;
    let log = ContainerLog::new("demo", "talker");
    fake.append_log(&log, "shutting down");
    wait_line(&app, talker, "shutting down")?;
    // Paused while it streams; the end lifts the pause, since nothing more will come.
    app.dispatch("logs::TogglePause", Value::Null)?;
    wait_tab_is(&app, talker, json!({"logs": {"paused": true}}))?;
    fake.delete_pod("demo", "talker");
    let state = wait_tab_is(
        &app,
        talker,
        json!({"status": {"state": "ended", "reason": "pod deleted", "pod_deleted": true}}),
    )?;
    let ended = tab(&state, talker);
    assert_eq!(ended["status_text"], "Ended · pod deleted", "{ended}");
    assert_eq!(
        ended["logs"]["paused"], false,
        "the end lifts the pause: {ended}"
    );
    assert_no_repeats(ended);
    let refused = app
        .dispatch("logs::TogglePause", Value::Null)
        .expect_err("an ended tab can't pause");
    assert_eq!(refused.code, "not_available", "{refused}");
    app.screenshot_themes("up40-pod-deleted")?;
    fake.finish_delete("demo", "talker");
    until("no open stream", || fake.open_log_streams(), |n| *n == 0)?;

    // A crash loop: the container stops and the kubelet backs off before restarting it. The
    // tab says it waits for the restart, then reads the next instance, never showing the
    // stopped one's lines twice (the kubelet serves them again meanwhile).
    show(&app, "looper")?;
    let looper = open_tab(&app, "detail::OpenLogs", Value::Null)?;
    wait_tab_is(&app, looper, json!({"status": {"state": "streaming"}}))?;
    until("looper's stream", || fake.open_log_streams(), |n| *n == 1)?;
    let log = ContainerLog::new("demo", "looper");
    fake.append_log(&log, "run 1\nERROR run 1 crashed");
    wait_line(&app, looper, "ERROR run 1 crashed")?;
    fake.crash_container("demo", "looper", "app");
    fake.back_off_container("demo", "looper", "app");
    let state = wait_tab_is(
        &app,
        looper,
        json!({"status": {"state": "reconnecting", "back_off": true},
               "status_text": "Waiting for the container to restart (back-off)"}),
    )?;
    assert_eq!(fake.open_log_streams(), 0, "{}", tab(&state, looper));
    app.screenshot_themes("up40-back-off")?;
    fake.start_container("demo", "looper", "app");
    wait_tab_is(&app, looper, json!({"status": {"state": "streaming"}}))?;
    until(
        "looper's next stream",
        || fake.open_log_streams(),
        |n| *n == 1,
    )?;
    fake.append_log(&log, "run 2");
    let state = wait_line(&app, looper, "run 2")?;
    assert_eq!(
        texts(tab(&state, looper)),
        ["run 1", "ERROR run 1 crashed", "run 2"],
        "each line once, in order"
    );
    app.dispatch("dock::CloseTab", json!({"tab": looper}))?;
    until("no open stream", || fake.open_log_streams(), |n| *n == 0)?;

    // No permission: the tab says so in plain words.
    show(&app, "locked")?;
    let locked = open_tab(&app, "detail::OpenLogs", Value::Null)?;
    let state = wait_tab_is(
        &app,
        locked,
        json!({"status": {"state": "failed",
                          "reason": "You don't have permission to read this pod's logs."},
               "status_text": "Ended · no permission"}),
    )?;
    let refused = tab(&state, locked);
    assert!(lines(refused).is_empty(), "{refused}");
    assert_eq!(log_requests_of(fake, "locked"), 1, "a 403 is not retried");
    app.screenshot_themes("up40-forbidden")?;

    // The previous instance's log is gone: the tab says so, and the kubelet's notice is not
    // shown as a line.
    show(&app, "rotated")?;
    let rotated = open_tab(&app, "detail::OpenPreviousLogs", Value::Null)?;
    let state = wait_tab_is(
        &app,
        rotated,
        json!({"kind": "previous_logs",
               "status": {"state": "failed",
                          "reason": "The previous instance's logs aren't available (the container may have been removed)."},
               "status_text": "Ended · not available"}),
    )?;
    let gone = tab(&state, rotated);
    assert!(
        texts(gone)
            .iter()
            .all(|t| !t.contains("unable to retrieve")),
        "{gone}"
    );
    app.screenshot_themes("up40-previous-not-available")?;
    // Each ended tab keeps its words while the others end.
    let state = app.state()?;
    assert_eq!(
        tab(&state, talker)["status_text"],
        "Ended · pod deleted",
        "{}",
        state["dock"]
    );
    Ok(())
}

// covers UP-45
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_45_follow_workload_logs() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    fake.set_log("demo", POD, "a1");
    fake.set_log("demo", OTHER, "b1");
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    app.dispatch("workspace::SetNamespaces", json!({"namespaces": ["demo"]}))?;
    open_live(&app, "deployments.apps", 2)?;
    show(&app, "web")?;

    // Logs on Deployment web: one tab merging both pods' lines, each with its pod and
    // container.
    let id = open_tab(&app, "detail::OpenLogs", Value::Null)?;
    let state = wait_tab_is(
        &app,
        id,
        json!({"kind": "workload_logs", "status": {"state": "streaming"},
               "logs": {"sources": true,
                        "streams": {"pods": 2, "live": 2, "failed": 0, "waiting": 0}}}),
    )?;
    assert_eq!(tab(&state, id)["title"], "web (Deployment)");
    until("two open streams", || fake.open_log_streams(), |n| *n == 2)?;
    wait_tab(&app, id, "/logs/lines", Test::at_least(2))?;
    let source = |pod: &str| format!("{pod}/nginx");
    let state = wait_tab(
        &app,
        id,
        "/logs/last_lines",
        Any(json!({"text": "b1", "source": source(OTHER)})),
    )?;
    assert!(
        lines(tab(&state, id))
            .iter()
            .any(|l| l["text"] == "a1" && l["source"] == source(POD)),
        "{}",
        tab(&state, id)
    );
    fake.append_log(&ContainerLog::new("demo", POD), "ERROR a2");
    fake.append_log(&ContainerLog::new("demo", OTHER), "b2");
    wait_tab(
        &app,
        id,
        "/logs/last_lines",
        Any(json!({"text": "ERROR a2", "source": source(POD), "severity": "error"})),
    )?;
    wait_tab(
        &app,
        id,
        "/logs/last_lines",
        Any(json!({"text": "b2", "source": source(OTHER)})),
    )?;
    app.screenshot_themes("up45-workload-logs")?;

    // The prefix hides and shows again without reading the logs again.
    let requests = fake.log_requests().len();
    app.dispatch("logs::ToggleSources", Value::Null)?;
    wait_tab_is(&app, id, json!({"logs": {"sources": false}}))?;
    app.screenshot("up45-without-prefix")?;
    app.dispatch("logs::ToggleSources", Value::Null)?;
    wait_tab_is(&app, id, json!({"logs": {"sources": true}}))?;
    // Lines written after the toggles arrive on the streams already open.
    fake.append_log(&ContainerLog::new("demo", POD), "a after the toggles");
    fake.append_log(&ContainerLog::new("demo", OTHER), "b after the toggles");
    wait_line(&app, id, "a after the toggles")?;
    wait_line(&app, id, "b after the toggles")?;
    assert_eq!(
        fake.log_requests().len(),
        requests,
        "toggling the prefix sent no log request"
    );

    // Scale up: the new pod is Pending first, and its log is read once it started, without
    // reopening the tab.
    let new = "web-69c6f74b8b-new01";
    let template = fake.pod("demo", POD).expect("the pod's object");
    fake.add_pod(
        template
            .clone()
            .sibling(new)
            .waiting("nginx", "ContainerCreating"),
    );
    wait_tab_is(
        &app,
        id,
        json!({"logs": {"streams": {"pods": 3, "waiting": 1, "live": 2}}}),
    )?;
    app.screenshot("up45-pending-pod")?;
    assert_eq!(
        log_requests_of(fake, new),
        0,
        "no log request before the container started"
    );
    fake.start_container("demo", new, "nginx");
    wait_tab_is(
        &app,
        id,
        json!({"logs": {"streams": {"pods": 3, "waiting": 0, "live": 3}}}),
    )?;
    wait_tab(
        &app,
        id,
        "/logs/streams/each",
        Equals(each(&[
            (new, "streaming"),
            (POD, "streaming"),
            (OTHER, "streaming"),
        ])),
    )?;
    until(
        "three open streams",
        || fake.open_log_streams(),
        |n| *n == 3,
    )?;
    fake.append_log(&ContainerLog::new("demo", new), "n1");
    wait_tab(
        &app,
        id,
        "/logs/last_lines",
        Any(json!({"text": "n1", "source": source(new)})),
    )?;

    // A rollout replaces a pod: the new one is followed, the old one's stream is gone.
    let replacement = "web-69c6f74b8b-new02";
    fake.replace_pod("demo", OTHER, template.clone().sibling(replacement));
    wait_tab(
        &app,
        id,
        "/logs/streams/each",
        Equals(each(&[
            (new, "streaming"),
            (replacement, "streaming"),
            (POD, "streaming"),
        ])),
    )?;
    wait_tab_is(
        &app,
        id,
        json!({"logs": {"streams": {"pods": 3, "live": 3, "failed": 0}}}),
    )?;
    until(
        "the replacement's stream",
        || fake.open_log_streams_of(&ContainerLog::new("demo", replacement)),
        |n| *n == 1,
    )?;
    fake.append_log(&ContainerLog::new("demo", replacement), "r1");
    wait_tab(
        &app,
        id,
        "/logs/last_lines",
        Any(json!({"text": "r1", "source": source(replacement)})),
    )?;

    // One pod's log is refused (403, which is final): its stream fails, the others read on.
    let refused = "web-69c6f74b8b-new03";
    fake.inject(
        Fault::forbidden()
            .on(&pods(fake))
            .named(refused)
            .subresource("log"),
    );
    fake.replace_pod("demo", new, template.clone().sibling(refused));
    wait_tab(
        &app,
        id,
        "/logs/streams/each",
        Equals(each(&[
            (replacement, "streaming"),
            (refused, "failed"),
            (POD, "streaming"),
        ])),
    )?;
    wait_tab_is(
        &app,
        id,
        json!({"status": {"state": "streaming"},
               "logs": {"streams": {"pods": 3, "live": 2, "failed": 1}}}),
    )?;
    fake.append_log(&ContainerLog::new("demo", POD), "a3 after the refusal");
    fake.append_log(
        &ContainerLog::new("demo", replacement),
        "r2 after the refusal",
    );
    wait_line(&app, id, "a3 after the refusal")?;
    wait_line(&app, id, "r2 after the refusal")?;
    app.screenshot_themes("up45-one-stream-failed")?;
    // Once allowed, a restart of its container revives the failed stream.
    fake.clear_faults();
    fake.restart_container("demo", refused, "nginx");
    wait_tab(
        &app,
        id,
        "/logs/streams/each",
        Equals(each(&[
            (replacement, "streaming"),
            (refused, "streaming"),
            (POD, "streaming"),
        ])),
    )?;
    until(
        "the revived stream",
        || fake.open_log_streams_of(&ContainerLog::new("demo", refused)),
        |n| *n == 1,
    )?;
    fake.append_log(&ContainerLog::new("demo", refused), "x1 revived");
    wait_tab(
        &app,
        id,
        "/logs/last_lines",
        Any(json!({"text": "x1 revived", "source": source(refused)})),
    )?;

    // A 500 on one pod's log is not final: that stream reconnects, again and again, while the
    // others read on; once the server answers, it streams again.
    fake.inject(
        Fault::status(500)
            .on(&pods(fake))
            .named(replacement)
            .subresource("log"),
    );
    let before = log_requests_of(fake, replacement);
    assert_eq!(
        fake.drop_log_streams(&ContainerLog::new("demo", replacement)),
        1
    );
    until(
        "two refused reconnects",
        || log_requests_of(fake, replacement),
        |n| *n >= before + 2,
    )?;
    let state = wait_tab(
        &app,
        id,
        "/logs/streams/each",
        Equals(each(&[
            (replacement, "reconnecting"),
            (refused, "streaming"),
            (POD, "streaming"),
        ])),
    )?;
    assert_eq!(
        tab(&state, id)["logs"]["streams"]["failed"],
        0,
        "a 500 fails no stream: {}",
        tab(&state, id)
    );
    fake.append_log(&ContainerLog::new("demo", POD), "a4 during the 500s");
    wait_line(&app, id, "a4 during the 500s")?;
    app.screenshot("up45-reconnecting-stream")?;
    fake.clear_faults();
    wait_tab(
        &app,
        id,
        "/logs/streams/each",
        Equals(each(&[
            (replacement, "streaming"),
            (refused, "streaming"),
            (POD, "streaming"),
        ])),
    )?;
    fake.append_log(&ContainerLog::new("demo", replacement), "r3 after the 500s");
    wait_line(&app, id, "r3 after the 500s")?;

    // A crash loop: each time the container stops, the kubelet backs off (CrashLoopBackOff) and
    // serves the stopped instance again; the stream waits for the next instance and reads on
    // from it, never showing a line twice.
    for round in 1..=2 {
        fake.crash_container("demo", POD, "nginx");
        fake.back_off_container("demo", POD, "nginx");
        wait_tab(
            &app,
            id,
            "/logs/streams/each",
            Equals(each(&[
                (replacement, "streaming"),
                (refused, "streaming"),
                (POD, "reconnecting"),
            ])),
        )?;
        wait_tab_is(
            &app,
            id,
            json!({"status": {"state": "streaming"},
                   "logs": {"streams": {"live": 2, "failed": 0}}}),
        )?;
        if round == 1 {
            app.screenshot("up45-crash-loop")?;
        }
        fake.start_container("demo", POD, "nginx");
        wait_tab(
            &app,
            id,
            "/logs/streams/each",
            Equals(each(&[
                (replacement, "streaming"),
                (refused, "streaming"),
                (POD, "streaming"),
            ])),
        )?;
        until(
            "the restarted container's stream",
            || fake.open_log_streams_of(&ContainerLog::new("demo", POD)),
            |n| *n == 1,
        )?;
        let line = format!("a after restart {round}");
        fake.append_log(&ContainerLog::new("demo", POD), &line);
        wait_tab(
            &app,
            id,
            "/logs/last_lines",
            Any(json!({"text": line, "source": source(POD)})),
        )?;
    }
    let state = app.state()?;
    assert_no_repeats(tab(&state, id));

    // A pod is deleted: its stream ends quietly and the session reads on.
    fake.delete_pod("demo", replacement);
    fake.finish_delete("demo", replacement);
    wait_tab(
        &app,
        id,
        "/logs/streams/each",
        Equals(each(&[(refused, "streaming"), (POD, "streaming")])),
    )?;
    wait_tab_is(
        &app,
        id,
        json!({"status": {"state": "streaming"},
               "logs": {"streams": {"pods": 2, "live": 2, "failed": 0}}}),
    )?;
    fake.append_log(&ContainerLog::new("demo", POD), "still here");
    let state = wait_line(&app, id, "still here")?;
    assert_no_repeats(tab(&state, id));
    assert_eq!(state["dock"]["tabs"].as_array().map(Vec::len), Some(1));
    app.screenshot_themes("up45-after-churn")?;
    Ok(())
}
