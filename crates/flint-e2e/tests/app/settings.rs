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
