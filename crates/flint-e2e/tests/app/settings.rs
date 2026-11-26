//! UP-126 and UP-67: the Settings view (#740, #741), a modal over the main window (#854), in the
//! running app. Its draft, Revert, the on-disk banner, the Discard / Keep editing question and the
//! quit question are view-tested in `flint-app` (`settings_view_tests.rs`,
//! `settings_thresholds_tests.rs`); here the shortcut opens it, edits (an appearance, a threshold
//! and a cluster override) wait for Save, and Save writes `settings.toml`.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::Options;
use serde_json::{Value, json};

use crate::support::*;

// covers UP-126
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_126_settings_opens_from_its_key_and_saves_the_appearance() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;

    let main = app.state()?["main"]["view"].clone();
    app.press_key("secondary-,")?;
    let state = app.wait_for("/settings/open", Equals(true.into()))?;
    // A modal over the main window: the main area stays as it was.
    assert_eq!(state["main"]["view"], main, "{state}");
    assert_eq!(state["settings"]["appearance"], "system");

    // The Dark button's action: an edit, not written yet.
    app.dispatch("settings::EditAppearance", json!({ "appearance": "dark" }))?;
    let state = app.wait_for("/settings/dirty", Equals(true.into()))?;
    assert_eq!(state["settings"]["edited"], json!(["appearance"]));
    assert_eq!(state["appearance"], "system", "{state}");
    let path = app.config_dir().join("settings.toml");
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(!before.contains("appearance"), "{before}");
    app.screenshot("up126-settings-draft")?;

    app.dispatch("settings::SaveSettings", Value::Null)?;
    app.wait_for("/appearance", Equals("dark".into()))?;
    app.wait_for("/settings/dirty", Equals(false.into()))?;
    let stored = std::fs::read_to_string(&path)?;
    assert!(stored.contains("appearance = \"dark\""), "{stored}");
    assert!(!stored.contains("watch_mode"), "{stored}");
    assert!(!stored.contains("desktop_notifications"), "{stored}");

    // Saved: Settings stayed open, and Esc closes it without asking.
    assert_eq!(app.state()?["settings"]["open"], true);
    app.press_key("escape")?;
    app.wait_for("/settings/open", Equals(false.into()))?;
    assert_eq!(app.state()?["dialog"], Value::Null);
    Ok(())
}

// covers UP-67
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_67_settings_sets_a_threshold_and_a_cluster_override() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;

    app.press_key("secondary-,")?;
    app.wait_for("/settings/open", Equals(true.into()))?;
    app.dispatch(
        "settings::ShowSettingsSection",
        json!({ "section": "thresholds" }),
    )?;

    // The global CPU threshold, typed in its field.
    app.click("settings-threshold-cpu_percent")?;
    app.wait_for(
        "/settings/thresholds/typing",
        Equals("settings-threshold-cpu_percent".into()),
    )?;
    app.type_text("80")?;
    app.press_key("enter")?;
    let state = app.wait_for("/settings/thresholds/global/cpu_percent", Equals(80.into()))?;
    assert_eq!(
        state["settings"]["edited"],
        json!(["thresholds.cpu_percent"])
    );

    // Out of range: kept, said why, and Save is blocked.
    app.dispatch(
        "settings::EditThreshold",
        json!({ "metric": "memory_percent", "value": "120" }),
    )?;
    let state = app.wait_for("/settings/can_save", Equals(false.into()))?;
    assert_eq!(
        state["settings"]["thresholds"]["rejected"][0]["why"],
        "Memory must be a whole number from 1 to 100."
    );
    app.dispatch(
        "settings::EditThreshold",
        json!({ "metric": "memory_percent", "value": "" }),
    )?;
    app.wait_for("/settings/can_save", Equals(true.into()))?;

    // An override for this cluster alone.
    app.dispatch(
        "settings::ShowSettingsSection",
        json!({ "section": "cluster_overrides" }),
    )?;
    app.click("settings-override-add")?;
    app.wait_for(
        "/settings/thresholds/candidates",
        Equals(json!([cluster.context])),
    )?;
    app.click("settings-override-pick-0")?;
    app.wait_for(
        "/settings/thresholds/overrides/0/cluster",
        Equals(cluster.context.clone().into()),
    )?;
    app.dispatch(
        "settings::EditThreshold",
        json!({ "cluster": cluster.context, "metric": "cpu_percent", "value": "70" }),
    )?;
    app.wait_for(
        "/settings/thresholds/overrides/0/thresholds/cpu_percent",
        Equals(70.into()),
    )?;
    app.screenshot("up67-settings-overrides")?;

    let path = app.config_dir().join("settings.toml");
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(!before.contains("thresholds"), "{before}");
    app.dispatch("settings::SaveSettings", Value::Null)?;
    app.wait_for("/settings/dirty", Equals(false.into()))?;
    let stored = std::fs::read_to_string(&path)?;
    assert!(stored.contains("[thresholds]"), "{stored}");
    assert!(stored.contains("cpu_percent = 80"), "{stored}");
    assert!(stored.contains("[[thresholds.clusters]]"), "{stored}");
    assert!(
        stored.contains(&format!("context = \"{}\"", cluster.context)),
        "{stored}"
    );
    assert!(stored.contains("cpu_percent = 70"), "{stored}");
    assert!(!stored.contains("memory_percent"), "{stored}");

    // Reset drops the override from the file.
    app.click("settings-override-0-reset")?;
    app.wait_for("/settings/thresholds/overrides", Equals(json!([])))?;
    app.dispatch("settings::SaveSettings", Value::Null)?;
    app.wait_for("/settings/dirty", Equals(false.into()))?;
    let stored = std::fs::read_to_string(&path)?;
    assert!(!stored.contains("thresholds.clusters"), "{stored}");
    assert!(stored.contains("cpu_percent = 80"), "{stored}");
    Ok(())
}

// covers UP-127
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_127_settings_saves_the_log_buffer_and_asks_before_unprotecting() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let app = App::launch(
        AppOptions::new(&cluster.kubeconfig).settings("protect_local_clusters = true"),
    )?;
    connect(&app, &cluster.context)?;
    let path = app.config_dir().join("settings.toml");

    app.press_key("secondary-,")?;
    app.wait_for("/settings/open", Equals(true.into()))?;
    app.dispatch(
        "settings::ShowSettingsSection",
        json!({ "section": "logs" }),
    )?;
    assert_eq!(app.state()?["settings"]["log_buffer_lines"], 100_000);

    // Out of range: an error under the field, and Save is not offered.
    app.dispatch("settings::EditLogBufferLines", json!({ "lines": 500 }))?;
    let state = app.wait_for("/settings/can_save", Equals(false.into()))?;
    assert!(
        state["settings"]["field_errors"]["settings-log-buffer-lines"].is_string(),
        "{state}"
    );
    app.dispatch("settings::EditLogBufferLines", json!({ "lines": 20000 }))?;
    let state = app.wait_for("/settings/can_save", Equals(true.into()))?;
    assert_eq!(state["settings"]["edited"], json!(["log_buffer_lines"]));
    assert!(!std::fs::read_to_string(&path)?.contains("log_buffer_lines"));
    app.screenshot("up127-settings-logs")?;

    app.dispatch("settings::SaveSettings", Value::Null)?;
    app.wait_for("/settings/dirty", Equals(false.into()))?;
    let stored = std::fs::read_to_string(&path)?;
    assert!(stored.contains("log_buffer_lines = 20000"), "{stored}");

    // Turning protection off asks at Save; Escape cancels it and nothing is written.
    app.dispatch(
        "settings::ShowSettingsSection",
        json!({ "section": "safety" }),
    )?;
    app.dispatch(
        "settings::EditProtectLocalClusters",
        json!({ "enabled": false }),
    )?;
    app.dispatch("settings::SaveSettings", Value::Null)?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    assert_eq!(
        state["dialog"]["title"], "Turn off protection for local clusters?",
        "{state}"
    );
    app.screenshot("up127-settings-unprotect")?;
    app.press_key("escape")?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    assert_eq!(app.state()?["settings"]["dirty"], true);
    let stored = std::fs::read_to_string(&path)?;
    assert!(stored.contains("protect_local_clusters = true"), "{stored}");

    app.dispatch("settings::SaveSettings", Value::Null)?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.click("ok")?;
    app.wait_for("/settings/dirty", Equals(false.into()))?;
    let stored = std::fs::read_to_string(&path)?;
    assert!(
        stored.contains("protect_local_clusters = false"),
        "{stored}"
    );
    Ok(())
}

/// What the fake proxy saw: each `CONNECT` request's head.
type Heads = Arc<Mutex<Vec<String>>>;

/// A proxy answering `CONNECT` by tunnelling to `upstream`, wherever the client asked to go.
/// Returns its `host:port` and what it saw.
fn connect_proxy(upstream: SocketAddr) -> Result<(String, Heads)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let heads = Heads::default();
    let log = Arc::clone(&heads);
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let log = Arc::clone(&log);
            std::thread::spawn(move || tunnel(client, upstream, &log));
        }
    });
    Ok((addr.to_string(), heads))
}

fn tunnel(mut client: TcpStream, upstream: SocketAddr, heads: &Mutex<Vec<String>>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if client.read_exact(&mut byte).is_err() {
            return;
        }
        head.push(byte[0]);
    }
    heads
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&head).into_owned());
    let Ok(mut server) = TcpStream::connect(upstream) else {
        return;
    };
    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .is_err()
    {
        return;
    }
    let (Ok(mut from_client), Ok(mut to_client)) = (client.try_clone(), server.try_clone()) else {
        return;
    };
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut from_client, &mut server);
        let _ = server.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut to_client, &mut client);
    let _ = client.shutdown(Shutdown::Write);
}

// covers #743: a proxy typed in Settings' Network section is applied after the confirmation, and
// the selected cluster reconnects through it. Its password is never in what the app shows.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn settings_applies_a_proxy_and_the_cluster_reconnects_through_it() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let upstream: SocketAddr = cluster
        .server
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .parse()?;
    let (proxy, heads) = connect_proxy(upstream)?;
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    assert!(heads.lock().unwrap().is_empty(), "direct at first");

    app.press_key("secondary-,")?;
    app.wait_for("/settings/open", Equals(true.into()))?;
    app.dispatch(
        "settings::ShowSettingsSection",
        json!({ "section": "network" }),
    )?;
    app.dispatch(
        "settings::EditProxyUrl",
        json!({ "url": format!("http://user:proxy-secret@{proxy}") }),
    )?;
    app.wait_for("/settings/network/can_apply", Equals(true.into()))?;

    app.dispatch("settings::ApplyProxy", Value::Null)?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    assert_eq!(state["dialog"]["title"], "Apply the new proxy?", "{state}");
    let text = state["dialog"]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("writes waiting for review are refused"),
        "{text}"
    );
    assert!(!state.to_string().contains("proxy-secret"), "{state}");
    app.screenshot("settings-proxy-apply")?;
    app.click("ok")?;

    let state = app.wait_for(
        "/settings/network/reconnected",
        Equals(json!([cluster.context])),
    )?;
    assert!(!state.to_string().contains("proxy-secret"), "{state}");
    assert_eq!(
        state["settings"]["network"]["saved_url"],
        format!("http://•••@{proxy}"),
        "{state}"
    );
    let heads = heads.lock().unwrap().clone();
    assert!(
        !heads.is_empty(),
        "the cluster reconnected through the proxy"
    );
    // base64 of `user:proxy-secret`.
    assert!(
        heads
            .iter()
            .all(|head| head.contains("dXNlcjpwcm94eS1zZWNyZXQ=")),
        "{heads:?}"
    );
    let stored = std::fs::read_to_string(app.config_dir().join("settings.toml"))?;
    assert!(stored.contains(&proxy), "{stored}");
    Ok(())
}
