//! UP-126: the Settings view (#740), in the running app. Its draft, Revert, the on-disk banner
//! and the quit question are view-tested in `flint-app` (`settings_view_tests.rs`); here the
//! shortcut opens it, an edit waits for Save, and Save writes `settings.toml`.

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

    app.press_key("secondary-,")?;
    app.wait_for("/settings/open", Equals(true.into()))?;
    app.wait_for("/main/view", Equals("settings".into()))?;
    assert_eq!(app.state()?["settings"]["appearance"], "system");

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

    app.press_key("escape")?;
    app.wait_for("/settings/open", Equals(false.into()))?;
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
