//! View tests for Settings' Thresholds and Cluster overrides (#741): typing a global threshold,
//! range validation blocking Save, and the override table (add, edit, reset), each saved to
//! `settings.toml` and followed by the thresholds the dashboard reads.

use flint_core::settings::ClusterPin;
use flint_core::thresholds::ThresholdMetric;
use flint_core::{Settings, WatchMode};
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::closing::Quits;
use super::settings_view::REJECTED;
use super::support_tests::{
    TestWorkspace, WAIT, alpha_and_beta, kubeconfig_yaml, open_workspace, open_workspace_with,
};
use super::view_state::{AppState, DialogState, Focus};

fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> AppState {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.view_state(window, cx))
    })
    .unwrap()
}

/// The workspace with Settings open on `section`.
async fn open_section(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    section: &str,
) -> TestWorkspace {
    let t = open_workspace(cx, dir).await;
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    t.focus_workspace(cx);
    t.press_key_of(cx, "flint::OpenSettings");
    t.dispatch(
        cx,
        "settings::ShowSettingsSection",
        json!({ "section": section }),
    )
    .unwrap();
    t
}

async fn wait_for_file(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    wanted: impl Fn(&Settings) -> bool + 'static,
) {
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        wanted(&model.read(cx).settings)
    })
    .await;
}

fn alpha_server(cx: &mut TestAppContext, t: &TestWorkspace) -> String {
    t.model.read_with(cx, |model, _| {
        model
            .clusters
            .iter()
            .find(|c| c.info.name == "kind-alpha")
            .and_then(|c| c.info.server.clone())
            .unwrap()
    })
}

fn in_force(cx: &mut TestAppContext, t: &TestWorkspace, context: &str, server: &str) -> u32 {
    t.model.read_with(cx, |model, _| {
        model
            .settings
            .thresholds_for(context, Some(server))
            .get(ThresholdMetric::CpuPercent)
    })
}

fn edit(cx: &mut TestAppContext, t: &TestWorkspace, action: &str, args: Value) {
    t.dispatch(cx, action, args).unwrap();
}

// covers UP-67: a global threshold is typed in its field, waits in the draft, and Save writes
// it; the dashboard's thresholds follow.
#[gpui_kit::test]
async fn a_global_threshold_is_typed_and_saved(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    let s = state(cx, &t).settings.thresholds;
    assert_eq!(s.global["cpu_percent"], None, "the built-in default");
    assert_eq!(
        t.label(cx, "settings-threshold-cpu_percent").as_deref(),
        Some("CPU threshold: 90. The built-in default, 90.")
    );

    t.click(cx, "settings-threshold-cpu_percent");
    let s = state(cx, &t);
    assert_eq!(
        s.settings.thresholds.typing.as_deref(),
        Some("settings-threshold-cpu_percent")
    );
    assert!(t.find(cx, "settings-threshold-cpu_percent-input").is_some());
    t.type_text(cx, "80");
    t.press(cx, "enter");
    let s = state(cx, &t);
    assert_eq!(s.settings.thresholds.typing, None);
    assert_eq!(s.focus, Focus::Settings, "focus goes back to Settings");
    assert_eq!(s.settings.edited, ["thresholds.cpu_percent"]);
    assert_eq!(s.settings.thresholds.global["cpu_percent"], Some(80));
    assert!(s.settings.can_save);
    assert!(t.settings_toml().is_empty(), "nothing written before Save");

    let server = alpha_server(cx, &t);
    assert_eq!(in_force(cx, &t, "kind-alpha", &server), 90);
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| {
        s.thresholds.global.get(ThresholdMetric::CpuPercent) == Some(80)
    })
    .await;
    assert!(!state(cx, &t).settings.dirty);
    let written = t.settings_toml();
    assert!(written.contains("[thresholds]"), "{written}");
    assert!(written.contains("cpu_percent = 80"), "{written}");
    assert_eq!(in_force(cx, &t, "kind-alpha", &server), 80);

    // Emptying the field follows the built-in default again.
    edit(
        cx,
        &t,
        "settings::EditThreshold",
        json!({ "metric": "cpu_percent", "value": "" }),
    );
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| {
        s.thresholds
            .global
            .get(ThresholdMetric::CpuPercent)
            .is_none()
    })
    .await;
    assert!(!t.settings_toml().contains("cpu_percent"));
}

// covers UP-67: a value out of its metric's range, or not a number, is kept with an inline
// error and blocks Save until it is fixed.
#[gpui_kit::test]
async fn an_out_of_range_threshold_blocks_save(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    edit(
        cx,
        &t,
        "settings::EditWatchMode",
        json!({ "mode": "on_view" }),
    );
    edit(
        cx,
        &t,
        "settings::EditThreshold",
        json!({ "metric": "memory_percent", "value": "150" }),
    );
    let s = state(cx, &t).settings;
    assert!(s.dirty && !s.can_save, "{s:?}");
    assert_eq!(s.thresholds.rejected.len(), 1);
    assert_eq!(s.thresholds.rejected[0].field, "thresholds.memory_percent");
    assert_eq!(s.thresholds.rejected[0].text, "150");
    let why = "Memory must be a whole number from 1 to 100.";
    assert_eq!(s.thresholds.rejected[0].why, why);
    assert_eq!(
        t.label(cx, "settings-threshold-memory_percent-error")
            .as_deref(),
        Some(why)
    );
    assert_eq!(
        t.label(cx, "settings-draft-note").as_deref(),
        Some(REJECTED)
    );
    assert!(!t.available(cx).contains(&"settings::SaveSettings"));
    t.click(cx, "settings-save");
    assert!(t.settings_toml().is_empty(), "Save is blocked");

    // Typed text that is not a number is rejected too, and shows as typed.
    t.click(cx, "settings-threshold-pending_pod_seconds");
    t.type_text(cx, "5m");
    t.press(cx, "enter");
    let s = state(cx, &t).settings;
    assert_eq!(s.thresholds.rejected.len(), 2);
    assert_eq!(
        t.label(cx, "settings-threshold-pending_pod_seconds")
            .as_deref(),
        Some("Pending pods threshold: 5m. Pending pods must be a whole number from 1 to 86400.")
    );

    // Opening the field again shows the rejected text, ready to fix.
    t.click(cx, "settings-threshold-pending_pod_seconds");
    t.type_text(cx, "600");
    t.press(cx, "enter");
    edit(
        cx,
        &t,
        "settings::EditThreshold",
        json!({ "metric": "memory_percent", "value": "85" }),
    );
    let s = state(cx, &t).settings;
    assert!(s.thresholds.rejected.is_empty() && s.can_save, "{s:?}");
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| {
        s.thresholds.global.get(ThresholdMetric::MemoryPercent) == Some(85)
    })
    .await;
    let written = t.settings_toml();
    assert!(written.contains("pending_pod_seconds = 600"), "{written}");
    assert!(written.contains("watch_mode = \"on_view\""), "{written}");
}

// covers UP-67: Esc drops what is typed; Revert drops rejected values.
#[gpui_kit::test]
async fn esc_drops_the_typed_text_and_revert_drops_rejected_values(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    t.click(cx, "settings-threshold-restarts_last_hour");
    t.type_text(cx, "7");
    t.press(cx, "escape");
    let s = state(cx, &t);
    assert!(s.settings.open, "Esc in the field does not leave Settings");
    assert_eq!(s.settings.thresholds.typing, None);
    assert!(!s.settings.dirty);

    edit(
        cx,
        &t,
        "settings::EditThreshold",
        json!({ "metric": "restarts_last_hour", "value": "0" }),
    );
    assert!(state(cx, &t).settings.dirty);
    t.click(cx, "settings-revert");
    let s = state(cx, &t).settings;
    assert!(!s.dirty && s.thresholds.rejected.is_empty());
}

// covers UP-67: from the keyboard alone, Tab reaches a threshold, Enter opens its field, and
// Enter enters the number.
#[gpui_kit::test]
async fn the_keyboard_alone_sets_a_threshold(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    // Done, five sections, then the first threshold.
    for _ in 0..7 {
        t.press(cx, "tab");
    }
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-threshold-cpu_percent")
    );
    t.press(cx, "tab");
    t.press(cx, "enter");
    assert_eq!(
        state(cx, &t).settings.thresholds.typing.as_deref(),
        Some("settings-threshold-memory_percent")
    );
    t.type_text(cx, "75");
    t.press(cx, "enter");
    let s = state(cx, &t).settings;
    assert_eq!(s.thresholds.global["memory_percent"], Some(75));
    assert_eq!(
        s.cursor.as_deref(),
        Some("settings-threshold-memory_percent")
    );
    t.press(cx, "tab");
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-threshold-restarts_last_hour")
    );
}

// covers UP-67: Add override picks another cluster; its thresholds, alerts and watch mode are
// edited in the table and saved, the dashboard's thresholds follow for that cluster alone, and
// Reset clears the row.
#[gpui_kit::test]
async fn the_override_table_adds_edits_and_resets_a_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "cluster_overrides").await;
    assert!(t.find(cx, "settings-overrides-empty").is_some());
    assert!(state(cx, &t).settings.thresholds.overrides.is_empty());

    t.click(cx, "settings-override-add");
    let s = state(cx, &t).settings;
    assert_eq!(
        s.thresholds.candidates,
        Some(vec!["kind-alpha".to_owned(), "kind-beta".to_owned()])
    );
    t.click(cx, "settings-override-pick-0");
    let s = state(cx, &t).settings;
    assert_eq!(s.thresholds.candidates, None, "picking closes the list");
    assert_eq!(s.thresholds.overrides.len(), 1);
    let row = &s.thresholds.overrides[0];
    assert_eq!(row.cluster, "kind-alpha");
    assert!(row.known && row.bound);
    assert_eq!(row.alerts, "default");
    assert!(!s.dirty, "an empty row is nothing to save");
    assert_eq!(
        t.label(cx, "settings-override-0-cpu_percent").as_deref(),
        Some("CPU threshold for kind-alpha: 90. Follows the global value, 90."),
        "an empty cell shows the global value"
    );

    t.click(cx, "settings-override-0-cpu_percent");
    t.type_text(cx, "70");
    t.press(cx, "enter");
    t.click(cx, "settings-override-0-alerts");
    t.click(cx, "settings-override-0-alerts");
    t.click(cx, "settings-override-0-watch");
    t.click(cx, "settings-override-0-watch");
    let s = state(cx, &t).settings;
    let row = &s.thresholds.overrides[0];
    assert_eq!(row.thresholds["cpu_percent"], Some(70));
    assert_eq!(row.alerts, "off");
    assert_eq!(row.watch_mode, "on_view");
    assert_eq!(
        s.edited,
        [
            "thresholds.clusters.kind-alpha.cpu_percent",
            "cluster_alerts.kind-alpha",
            "cluster_watch_mode.kind-alpha",
        ]
    );

    let server = alpha_server(cx, &t);
    let pin = ClusterPin::new("kind-alpha", server.clone());
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| s.cluster_watch_mode.contains_key("kind-alpha")).await;
    let written = t.settings_toml();
    assert!(written.contains("[[thresholds.clusters]]"), "{written}");
    assert!(written.contains("context = \"kind-alpha\""), "{written}");
    assert!(written.contains("cpu_percent = 70"), "{written}");
    assert!(written.contains("[[cluster_alerts]]"), "{written}");
    assert!(written.contains("kind-alpha = \"on_view\""), "{written}");
    t.model.read_with(cx, |model, _| {
        assert!(!model.settings.alerts_on("kind-alpha", Some(&server)));
        assert_eq!(
            model.settings.cluster_watch_mode.get("kind-alpha"),
            Some(&WatchMode::OnView)
        );
        assert_eq!(
            model
                .settings
                .thresholds
                .explicit(Some(&pin), ThresholdMetric::CpuPercent),
            Some(70)
        );
    });
    assert_eq!(in_force(cx, &t, "kind-alpha", &server), 70);
    let beta = t.model.read_with(cx, |model, _| {
        model
            .clusters
            .iter()
            .find(|c| c.info.name == "kind-beta")
            .and_then(|c| c.info.server.clone())
            .unwrap()
    });
    assert_eq!(
        in_force(cx, &t, "kind-beta", &beta),
        90,
        "other clusters keep the default"
    );

    // A row out of range is held like a global value.
    edit(
        cx,
        &t,
        "settings::EditThreshold",
        json!({ "cluster": "kind-alpha", "metric": "restarts_last_hour", "value": "20000" }),
    );
    let s = state(cx, &t).settings;
    assert!(!s.can_save);
    assert_eq!(
        s.thresholds.rejected[0].field,
        "thresholds.clusters.kind-alpha.restarts_last_hour"
    );
    assert!(
        t.find(cx, "settings-override-0-restarts_last_hour-error")
            .is_some()
    );

    // Reset clears the whole row, the rejected value included.
    t.click(cx, "settings-override-0-reset");
    let s = state(cx, &t).settings;
    assert!(s.thresholds.overrides.is_empty(), "{s:?}");
    assert!(s.thresholds.rejected.is_empty());
    assert!(s.can_save);
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| {
        s.cluster_watch_mode.is_empty() && s.cluster_alerts.is_empty()
    })
    .await;
    let written = t.settings_toml();
    assert!(!written.contains("kind-alpha"), "{written}");
    assert_eq!(in_force(cx, &t, "kind-alpha", &server), 90);
}

// covers UP-67: a cluster's overrides already in the file (written by the Alerts menu or by
// hand) show as a row.
#[gpui_kit::test]
async fn a_cluster_with_overrides_in_the_file_has_a_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let settings = format!(
            "[[thresholds.clusters]]\ncontext = \"kind-beta\"\nserver = \"{}\"\n\
             pending_pod_seconds = 900\n",
            servers.refused
        );
        std::fs::write(dir.join("settings.toml"), settings).unwrap();
        alpha_and_beta(dir, servers)
    })
    .await;
    t.focus_workspace(cx);
    t.press_key_of(cx, "flint::OpenSettings");
    t.dispatch(
        cx,
        "settings::ShowSettingsSection",
        json!({ "section": "cluster_overrides" }),
    )
    .unwrap();
    let s = state(cx, &t).settings;
    assert_eq!(s.thresholds.overrides.len(), 1);
    assert_eq!(s.thresholds.overrides[0].cluster, "kind-beta");
    assert_eq!(
        s.thresholds.overrides[0].thresholds["pending_pod_seconds"],
        Some(900)
    );
    assert_eq!(s.thresholds.overrides[0].thresholds["cpu_percent"], None);
    assert!(!s.dirty);
    assert_eq!(
        t.label(cx, "settings-override-0-pending_pod_seconds")
            .as_deref(),
        Some(
            "Pending pods threshold for kind-beta: 900. Set here. Clear the field to follow the level below."
        )
    );
}

// covers UP-67: quitting with a rejected value cannot save, so it asks Discard or Cancel.
#[gpui_kit::test]
async fn quit_with_a_rejected_value_asks_to_discard(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    edit(
        cx,
        &t,
        "settings::EditThreshold",
        json!({ "metric": "cpu_percent", "value": "0" }),
    );
    t.press_key_of(cx, "flint::Quit");
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    let dialog: DialogState = state(cx, &t).dialog.expect("a dialog is open");
    assert_eq!(dialog.title, "Discard unsaved settings?");
    assert!(dialog.text.contains("not accepted"), "{}", dialog.text);
}

// covers UP-67: a value still being typed is part of Save, and switching section enters it
// rather than dropping it.
#[gpui_kit::test]
async fn a_value_still_being_typed_is_entered_by_save_and_by_switching_section(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    t.click(cx, "settings-threshold-cpu_percent");
    t.type_text(cx, "85");
    edit(
        cx,
        &t,
        "settings::ShowSettingsSection",
        json!({ "section": "watching" }),
    );
    let s = state(cx, &t);
    assert_eq!(s.settings.thresholds.typing, None);
    assert_eq!(s.settings.thresholds.global["cpu_percent"], Some(85));
    assert_eq!(
        s.focus,
        Focus::Settings,
        "focus is not left on a field no longer drawn"
    );

    edit(
        cx,
        &t,
        "settings::ShowSettingsSection",
        json!({ "section": "thresholds" }),
    );
    t.click(cx, "settings-threshold-memory_percent");
    t.type_text(cx, "60");
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| {
        s.thresholds.global.get(ThresholdMetric::MemoryPercent) == Some(60)
    })
    .await;
    let written = t.settings_toml();
    assert!(written.contains("cpu_percent = 85"), "{written}");
    assert!(written.contains("memory_percent = 60"), "{written}");
}

// covers UP-67: a row whose last override is cleared stays until Save, so the keyboard cursor
// does not move onto another cluster; after Reset the cursor stands on Add override.
#[gpui_kit::test]
async fn a_cleared_row_stays_under_the_cursor_until_save(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "cluster_overrides").await;
    edit(
        cx,
        &t,
        "settings::EditClusterWatchMode",
        json!({ "cluster": "kind-alpha", "mode": "on_view" }),
    );
    edit(
        cx,
        &t,
        "settings::EditClusterWatchMode",
        json!({ "cluster": "kind-beta", "mode": "all" }),
    );
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| s.cluster_watch_mode.len() == 2).await;
    assert_eq!(state(cx, &t).settings.thresholds.overrides.len(), 2);

    // Watching on kind-alpha's row: On view, then Default, its last override.
    for _ in 0..21 {
        t.press(cx, "tab");
        if state(cx, &t).settings.cursor.as_deref() == Some("settings-override-0-watch") {
            break;
        }
    }
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-override-0-watch")
    );
    t.press(cx, "enter");
    let s = state(cx, &t).settings;
    assert_eq!(s.thresholds.overrides[0].cluster, "kind-alpha");
    assert_eq!(s.thresholds.overrides[0].watch_mode, "default");
    t.press(cx, "enter");
    let s = state(cx, &t).settings;
    assert_eq!(s.thresholds.overrides.len(), 2, "the cleared row stays");
    assert_eq!(s.thresholds.overrides[0].cluster, "kind-alpha");
    assert_eq!(s.thresholds.overrides[0].watch_mode, "all");
    assert_eq!(
        s.thresholds.overrides[1].watch_mode, "all",
        "kind-beta is untouched"
    );

    // Reset from the keyboard, then Enter again: Add override, not kind-beta's Reset.
    t.press(cx, "tab");
    t.press(cx, "enter");
    let s = state(cx, &t).settings;
    assert_eq!(s.cursor.as_deref(), Some("settings-override-add"));
    t.press(cx, "enter");
    let s = state(cx, &t).settings;
    assert!(
        s.thresholds.candidates.is_some(),
        "Enter opened Add override's list"
    );
    assert!(
        s.thresholds
            .overrides
            .iter()
            .any(|r| r.cluster == "kind-beta")
    );
}

// covers UP-67: an action naming a cluster it cannot apply to is refused with the reason.
#[gpui_kit::test]
async fn threshold_actions_refuse_an_unknown_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "cluster_overrides").await;
    let refused = t.dispatch(
        cx,
        "settings::EditThreshold",
        json!({ "cluster": "nowhere", "metric": "cpu_percent", "value": "50" }),
    );
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        t.dispatch(
            cx,
            "settings::ResetClusterOverrides",
            json!({ "cluster": "kind-alpha" }),
        )
        .is_err()
    );
    assert!(!state(cx, &t).settings.dirty);
}

/// Saves watch mode overrides for both clusters, so each has a row: kind-alpha first.
async fn rows_for_both(cx: &mut TestAppContext, t: &TestWorkspace) {
    for (cluster, mode) in [("kind-alpha", "on_view"), ("kind-beta", "on_view")] {
        edit(
            cx,
            t,
            "settings::EditClusterWatchMode",
            json!({ "cluster": cluster, "mode": mode }),
        );
    }
    t.click(cx, "settings-save");
    wait_for_file(cx, t, |s| s.cluster_watch_mode.len() == 2).await;
}

// covers UP-126, UP-67: a threshold still being typed is an unsaved edit: quitting asks.
#[gpui_kit::test]
async fn quit_with_only_a_value_being_typed_asks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    t.click(cx, "settings-threshold-cpu_percent");
    t.type_text(cx, "80");
    assert!(!state(cx, &t).settings.dirty, "not entered yet");
    t.press_key_of(cx, "flint::Quit");
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    let dialog: DialogState = state(cx, &t).dialog.expect("a dialog is open");
    assert_eq!(dialog.title, "Save your settings?");
    assert_eq!(
        cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0)),
        0
    );
    assert_eq!(
        state(cx, &t).settings.thresholds.global["cpu_percent"],
        Some(80)
    );
}

// covers UP-67: the first edit, still being typed, is saved by Save, and Revert drops it.
#[gpui_kit::test]
async fn save_and_revert_work_on_a_value_still_being_typed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    t.click(cx, "settings-threshold-restarts_last_hour");
    t.type_text(cx, "3");
    assert!(t.available(cx).contains(&"settings::RevertSettings"));
    t.click(cx, "settings-revert");
    let s = state(cx, &t).settings;
    assert!(!s.dirty && s.thresholds.typing.is_none(), "{s:?}");

    t.click(cx, "settings-threshold-restarts_last_hour");
    t.type_text(cx, "3");
    assert!(t.available(cx).contains(&"settings::SaveSettings"));
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| {
        s.thresholds.global.get(ThresholdMetric::RestartsLastHour) == Some(3)
    })
    .await;
    assert!(t.settings_toml().contains("restarts_last_hour = 3"));

    // Typed out of range, Save enters it and stops there.
    t.click(cx, "settings-threshold-restarts_last_hour");
    t.press(cx, "secondary-a");
    t.type_text(cx, "0");
    t.click(cx, "settings-save");
    let s = state(cx, &t).settings;
    assert_eq!(s.thresholds.rejected.len(), 1, "{s:?}");
    assert!(!s.saving);
}

// covers UP-67: Reset on another cluster keeps what is typed for this one.
#[gpui_kit::test]
async fn reset_of_another_cluster_keeps_the_value_being_typed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "cluster_overrides").await;
    rows_for_both(cx, &t).await;
    t.click(cx, "settings-override-0-cpu_percent");
    t.type_text(cx, "70");
    t.click(cx, "settings-override-1-reset");
    let s = state(cx, &t).settings;
    let alpha = s
        .thresholds
        .overrides
        .iter()
        .find(|r| r.cluster == "kind-alpha")
        .unwrap();
    assert_eq!(alpha.thresholds["cpu_percent"], Some(70));
    assert_eq!(alpha.watch_mode, "on_view");
}

// covers UP-67: the keyboard cursor follows its cluster when a row appears above it.
#[gpui_kit::test]
async fn the_cursor_follows_its_cluster_when_rows_change(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "cluster_overrides").await;
    edit(
        cx,
        &t,
        "settings::EditClusterWatchMode",
        json!({ "cluster": "kind-beta", "mode": "on_view" }),
    );
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| s.cluster_watch_mode.len() == 1).await;
    for _ in 0..30 {
        t.press(cx, "tab");
        if state(cx, &t).settings.cursor.as_deref() == Some("settings-override-0-watch") {
            break;
        }
    }
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-override-0-watch")
    );
    // kind-alpha gets a row above, from outside Settings (its header's watch mode menu).
    t.model.update(cx, |model, _| {
        model.set_watch_mode(Some("kind-alpha".into()), Some(WatchMode::All))
    });
    wait_for_file(cx, &t, |s| s.cluster_watch_mode.len() == 2).await;
    t.press(cx, "enter");
    let s = state(cx, &t).settings;
    assert_eq!(s.cursor.as_deref(), Some("settings-override-1-watch"));
    let mode = |name: &str| {
        s.thresholds
            .overrides
            .iter()
            .find(|r| r.cluster == name)
            .unwrap()
            .watch_mode
    };
    assert_eq!(mode("kind-alpha"), "all", "kind-alpha is untouched");
    assert_eq!(
        mode("kind-beta"),
        "default",
        "Enter worked kind-beta's control"
    );
}

// covers UP-67: after switching section, Enter works the section list, not a threshold of the
// section left.
#[gpui_kit::test]
async fn switching_section_moves_the_cursor_to_the_section_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "thresholds").await;
    for _ in 0..7 {
        t.press(cx, "tab");
    }
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-threshold-cpu_percent")
    );
    t.click(cx, "settings-section-watching");
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-section-watching")
    );
    t.press(cx, "enter");
    let s = state(cx, &t).settings;
    assert_eq!(s.thresholds.typing, None);
    assert_eq!(s.section, super::settings_view::Section::Watching);
}

// covers UP-67: a value rejected for an override whose context now points to another server
// has nowhere to be fixed, so it no longer blocks Save.
#[gpui_kit::test]
async fn a_rejected_override_of_a_repointed_context_is_retired(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "cluster_overrides").await;
    edit(
        cx,
        &t,
        "settings::EditThreshold",
        json!({ "cluster": "kind-alpha", "metric": "cpu_percent", "value": "0" }),
    );
    edit(
        cx,
        &t,
        "settings::EditWatchMode",
        json!({ "mode": "on_view" }),
    );
    assert!(!state(cx, &t).settings.can_save);
    repoint_alpha(cx, &t, dir.path()).await;
    t.find(cx, "settings-view");
    let s = state(cx, &t).settings;
    assert!(s.thresholds.rejected.is_empty(), "{s:?}");
    assert!(s.can_save);
}

/// Points kind-alpha to another server in the kubeconfig, and waits for the app to show it.
async fn repoint_alpha(cx: &mut TestAppContext, t: &TestWorkspace, dir: &std::path::Path) {
    let beta = t.model.read_with(cx, |model, _| {
        model
            .clusters
            .iter()
            .find(|c| c.info.name == "kind-beta")
            .and_then(|c| c.info.server.clone())
            .unwrap()
    });
    std::fs::write(
        dir.join("kubeconfig"),
        kubeconfig_yaml(&[("kind-alpha", "https://127.0.0.1:9"), ("kind-beta", &beta)]),
    )
    .unwrap();
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    core.reload_kubeconfig().await.unwrap();
    cx.run_until_parked();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).clusters.iter().any(|c| {
            c.info.name == "kind-alpha" && c.info.server.as_deref() == Some("https://127.0.0.1:9")
        })
    })
    .await;
}

// covers UP-67: Reset on a context that now points to another server also drops the edits made
// for the old one, hidden since, so a later Save does not write them.
#[gpui_kit::test]
async fn reset_of_a_repointed_context_drops_its_hidden_edits(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_section(cx, dir.path(), "cluster_overrides").await;
    edit(
        cx,
        &t,
        "settings::EditThreshold",
        json!({ "cluster": "kind-alpha", "metric": "cpu_percent", "value": "70" }),
    );
    edit(
        cx,
        &t,
        "settings::EditClusterWatchMode",
        json!({ "cluster": "kind-alpha", "mode": "on_view" }),
    );
    repoint_alpha(cx, &t, dir.path()).await;
    t.find(cx, "settings-view");
    assert!(state(cx, &t).settings.dirty);
    edit(
        cx,
        &t,
        "settings::ResetClusterOverrides",
        json!({ "cluster": "kind-alpha" }),
    );
    let s = state(cx, &t).settings;
    assert!(!s.dirty, "nothing is left to save: {s:?}");
}
