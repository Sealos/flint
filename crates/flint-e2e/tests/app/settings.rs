//! UP-126 and UP-67: the Settings view (#740, #741), in the running app. Its draft, Revert, the
//! on-disk banner and the quit question are view-tested in `flint-app` (`settings_view_tests.rs`,
//! `settings_thresholds_tests.rs`); here the shortcut opens it, edits (an appearance, a threshold
//! and a cluster override) wait for Save, and Save writes `settings.toml`.

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
