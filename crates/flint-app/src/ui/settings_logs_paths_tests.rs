//! Tests for the Logs, Paths and Write safety sections of Settings (#742): the range checks and
//! path checks, the kubeconfig list, and the confirmation before Save turns local cluster
//! protection off. The view tests run against loopback fixtures only, never a real kubeconfig.

use std::path::{Path, PathBuf};

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::closing::Quits;
use super::settings_draft::Change;
use super::settings_logs_paths::{
    TextField, UNPROTECT_STALE, UNPROTECT_TITLE, content_row, existing_file, paths_stops,
    remove_id, with_kubeconfig,
};
use super::settings_view::{Section, Stop, stops};
use super::support_tests::{TestWorkspace, WAIT, open_workspace};
use super::view_state::{AppState, DialogState};

#[test]
fn log_limits_are_range_checked() {
    let parse = |field: TextField, text: &str| field.parse(text, None);
    assert_eq!(
        parse(TextField::LogBufferLines, "1000"),
        Ok(Change::LogBufferLines(1000))
    );
    assert_eq!(
        parse(TextField::LogBufferLines, " 1,000,000 "),
        Ok(Change::LogBufferLines(1_000_000))
    );
    assert_eq!(
        parse(TextField::LogBufferLines, "20_000"),
        Ok(Change::LogBufferLines(20_000))
    );
    for bad in ["999", "1000001", "", "lots", "-5", "1.5", "99999999999"] {
        let why = parse(TextField::LogBufferLines, bad).unwrap_err();
        assert!(why.contains("1,000 to 1,000,000 lines"), "{bad}: {why}");
    }
    assert_eq!(
        parse(TextField::LogBufferMib, "8"),
        Ok(Change::LogBufferMib(8))
    );
    assert!(parse(TextField::LogBufferMib, "7").is_err());
    assert!(parse(TextField::LogBufferMib, "1025").is_err());
    assert_eq!(
        parse(TextField::LogTotalMib, "16384"),
        Ok(Change::LogTotalMib(16_384))
    );
    assert!(parse(TextField::LogTotalMib, "63").is_err());
    assert!(parse(TextField::LogTotalMib, "16385").is_err());
}

#[test]
fn the_path_override_takes_absolute_folders_only() {
    let parse = |text: &str| TextField::Path.parse(text, None);
    assert_eq!(parse(""), Ok(Change::Path(None)));
    assert_eq!(parse("  "), Ok(Change::Path(None)));
    assert_eq!(
        parse("/opt/homebrew/bin:/usr/bin"),
        Ok(Change::Path(Some("/opt/homebrew/bin:/usr/bin".into())))
    );
    for bad in [
        "bin",
        "/usr/bin:bin",
        "/usr/bin::/bin",
        ":/usr/bin",
        "/usr/bin:",
        "~/bin",
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn helm_and_kubeconfig_paths_must_be_files() {
    let dir = tempfile::tempdir().unwrap();
    let helm = dir.path().join("helm");
    std::fs::write(&helm, "").unwrap();
    let home = Some(dir.path());

    assert_eq!(TextField::Helm.parse("", home), Ok(Change::Helm(None)));
    assert_eq!(
        TextField::Helm.parse(helm.to_str().unwrap(), home),
        Ok(Change::Helm(Some(helm.clone())))
    );
    // `~` is the home folder.
    assert_eq!(
        TextField::Helm.parse("~/helm", home),
        Ok(Change::Helm(Some(PathBuf::from("~/helm"))))
    );
    assert!(TextField::Helm.parse("helm", home).is_err(), "relative");
    assert!(TextField::Helm.parse("~/missing", home).is_err());
    let folder = TextField::Helm.parse(dir.path().to_str().unwrap(), home);
    assert!(folder.unwrap_err().contains("not a file"));
    assert!(existing_file(Path::new("~/helm"), None).is_err(), "no HOME");

    let listed = with_kubeconfig(&[], &helm, home).unwrap();
    assert_eq!(listed, std::slice::from_ref(&helm));
    let twice = with_kubeconfig(&listed, &helm, home).unwrap_err();
    assert!(twice.contains("already"), "{twice}");
    // The same file under `~` is the same file.
    let tilde = with_kubeconfig(&listed, Path::new("~/helm"), home).unwrap_err();
    assert!(tilde.contains("already"), "{tilde}");
    assert!(with_kubeconfig(&listed, Path::new("/no/such/config"), home).is_err());
}

#[test]
fn the_keyboard_reaches_every_control_of_the_sections() {
    let mut shown = flint_core::Settings::default();
    let ids = |section, shown: &flint_core::Settings| {
        stops(
            section,
            false,
            Default::default(),
            shown.kubeconfig_paths.len(),
        )
        .into_iter()
        .map(|stop| stop.id().to_string())
        .collect::<Vec<_>>()
    };
    let logs = ids(Section::Logs, &shown);
    for id in [
        "settings-log-buffer-lines",
        "settings-log-buffer-mib",
        "settings-log-total-mib",
    ] {
        assert!(logs.iter().any(|l| l == id), "{id}");
    }
    shown.kubeconfig_paths = vec!["/a".into(), "/b".into()];
    let paths = ids(Section::Paths, &shown);
    for id in [
        "settings-kubeconfig-remove-0",
        "settings-kubeconfig-remove-1",
        "settings-kubeconfig-add",
        "settings-path",
        "settings-helm",
    ] {
        assert!(paths.iter().any(|p| p == id), "{id}");
    }
    assert!(
        ids(Section::Safety, &shown)
            .iter()
            .any(|id| id == "settings-protect-local-clusters")
    );
    assert_eq!(remove_id(1), remove_id(1));
    assert!(std::ptr::eq(remove_id(3), remove_id(3)), "made once");
}

fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> AppState {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.view_state(window, cx))
    })
    .unwrap()
}

async fn open_settings(cx: &mut TestAppContext, dir: &Path) -> TestWorkspace {
    let t = open_workspace(cx, dir).await;
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    t.focus_workspace(cx);
    t.press_key_of(cx, "flint::OpenSettings");
    t
}

fn show(cx: &mut TestAppContext, t: &TestWorkspace, section: &str) {
    t.dispatch(
        cx,
        "settings::ShowSettingsSection",
        json!({ "section": section }),
    )
    .unwrap();
}

async fn wait_for_file(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    wanted: impl Fn(&flint_core::Settings) -> bool + 'static,
) {
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        wanted(&model.read(cx).settings)
    })
    .await;
}

async fn shown_dialog(cx: &mut TestAppContext, t: &TestWorkspace) -> DialogState {
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    state(cx, t).dialog.expect("a dialog is open")
}

/// Waits for the dialog to close, and for its exit to finish drawing, so the next one's buttons
/// are the only ones.
async fn no_dialog(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
}

fn has_dialog(cx: &mut TestAppContext, t: &TestWorkspace) -> bool {
    cx.update_window(t.window, |_, window, cx| window.has_active_dialog(cx))
        .unwrap()
}

/// The protection the sidebar shows for the loopback `kind-alpha` context, a known local one.
fn alpha_protection(cx: &mut TestAppContext, t: &TestWorkspace) -> &'static str {
    state(cx, t)
        .clusters
        .iter()
        .find(|c| c.name == "kind-alpha")
        .expect("kind-alpha is listed")
        .protection
}

/// Settings opened over a settings.toml that protects local clusters.
async fn open_protected(cx: &mut TestAppContext, dir: &Path) -> TestWorkspace {
    std::fs::write(dir.join("settings.toml"), "protect_local_clusters = true\n").unwrap();
    let t = open_settings(cx, dir).await;
    assert!(
        t.model
            .read_with(cx, |m, _| m.settings.protect_local_clusters)
    );
    t
}

// covers UP-127: a log limit typed into its field is an edit; out of range it is an error that
// keeps Save disabled; Save writes it to settings.toml.
#[gpui_kit::test]
async fn a_log_limit_is_checked_as_typed_and_saved(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "logs");
    assert!(t.find(cx, "settings-log-buffer-lines").is_some());

    // From the keyboard: Tab onto the field puts focus in it.
    let stops_to_field = 1 + Section::ALL.len() + 1;
    for _ in 0..stops_to_field {
        t.press(cx, "tab");
    }
    let s = state(cx, &t);
    assert_eq!(
        s.settings.cursor.as_deref(),
        Some("settings-log-buffer-lines")
    );
    t.press(cx, "secondary-a");
    t.type_text(cx, "500");
    let s = state(cx, &t);
    assert!(
        s.settings.field_errors["settings-log-buffer-lines"].contains("1,000 to 1,000,000"),
        "{s:?}"
    );
    assert!(!s.settings.can_save);
    assert!(t.find(cx, "settings-log-buffer-lines-error").is_some());
    // Save shows the field instead of writing.
    t.dispatch(cx, "settings::SaveSettings", Value::Null)
        .unwrap();
    assert!(!dir.path().join("settings.toml").exists());

    t.press(cx, "secondary-a");
    t.type_text(cx, "20000");
    let s = state(cx, &t);
    assert!(s.settings.field_errors.is_empty(), "{s:?}");
    assert_eq!(s.settings.edited, ["log_buffer_lines"]);
    assert_eq!(s.settings.log_buffer_lines, 20_000);
    assert!(s.settings.can_save);
    // Escape leaves the field for the view, Settings stays open; Tab goes on to the next
    // control, into its field.
    t.press(cx, "escape");
    let s = state(cx, &t);
    assert!(s.settings.open);
    assert_eq!(
        s.settings.cursor.as_deref(),
        Some("settings-log-buffer-lines")
    );
    t.press(cx, "tab");
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-log-buffer-mib")
    );
    t.type_text(cx, "9999");
    assert!(
        state(cx, &t)
            .settings
            .field_errors
            .contains_key("settings-log-buffer-mib"),
        "typing goes into the field Tab reached"
    );
    t.press(cx, "secondary-a");
    t.type_text(cx, "64");
    t.press(cx, "shift-tab");
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-log-buffer-lines")
    );

    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| s.log_buffer_lines == 20_000).await;
    let written = t.settings_toml();
    assert!(written.contains("log_buffer_lines = 20000"), "{written}");
    assert!(!written.contains("log_buffer_mib"), "{written}");
    assert!(!state(cx, &t).settings.dirty);
}

// covers UP-127: the palette's edit goes through the same check; Revert puts the file's value
// back in the field and clears its error.
#[gpui_kit::test]
async fn revert_clears_a_field_error(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "logs");
    t.dispatch(cx, "settings::EditLogTotalMib", json!({ "mib": 20 }))
        .unwrap();
    let s = state(cx, &t);
    assert!(
        s.settings
            .field_errors
            .contains_key("settings-log-total-mib")
    );
    assert!(
        s.settings.dirty && s.settings.edited.is_empty(),
        "an invalid value is held as rejected, not as an edit"
    );
    t.click(cx, "settings-revert");
    let s = state(cx, &t);
    assert!(s.settings.field_errors.is_empty());
    assert_eq!(s.settings.log_total_mib, 512);
    let text = t.workspace.read_with(cx, |ws, cx| {
        ws.settings_field_text(TextField::LogTotalMib, cx)
    });
    assert_eq!(text.as_deref(), Some("512"));
}

// covers UP-127: Add… opens a file picker and adds what is chosen; Remove drops a file; Save
// writes the list, and the clusters reload from it.
#[gpui_kit::test]
async fn kubeconfig_files_are_added_and_removed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let extra = dir.path().join("extra-config");
    std::fs::write(&extra, "apiVersion: v1\nkind: Config\n").unwrap();
    let other = dir.path().join("other-config");
    std::fs::write(&other, "apiVersion: v1\nkind: Config\n").unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "paths");

    t.click(cx, "settings-kubeconfig-add");
    assert!(cx.did_prompt_for_paths());
    let chosen = vec![extra.clone(), other.clone()];
    cx.simulate_path_prompt_response(move |_| Some(chosen));
    cx.run_until_parked();
    let s = state(cx, &t);
    let listed = [extra.display().to_string(), other.display().to_string()];
    assert_eq!(s.settings.kubeconfig_paths, listed);
    assert_eq!(s.settings.edited, ["kubeconfig_paths"]);

    // The same file again is refused, and says why.
    t.dispatch(
        cx,
        "settings::AddKubeconfigPath",
        json!({ "path": extra.display().to_string() }),
    )
    .unwrap();
    let note = state(cx, &t).settings.kubeconfig_note.expect("a note");
    assert!(note.contains("already"), "{note}");

    t.click(cx, "settings-kubeconfig-remove-0");
    assert_eq!(
        state(cx, &t).settings.kubeconfig_paths,
        [other.display().to_string()]
    );
    t.click(cx, "settings-save");
    let wanted = other.clone();
    wait_for_file(cx, &t, move |s| s.kubeconfig_paths == [wanted.clone()]).await;
    let written = t.settings_toml();
    assert!(written.contains("kubeconfig_paths"), "{written}");
    assert!(!written.contains("extra-config"), "{written}");
}

// covers UP-127: PATH and helm are text fields; empty is the default and leaves no key.
#[gpui_kit::test]
async fn path_and_helm_are_saved_and_cleared(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let helm = dir.path().join("helm");
    std::fs::write(&helm, "").unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "paths");
    t.dispatch(
        cx,
        "settings::EditPathOverride",
        json!({ "path": "relative/bin" }),
    )
    .unwrap();
    assert!(
        state(cx, &t)
            .settings
            .field_errors
            .contains_key("settings-path")
    );
    t.dispatch(
        cx,
        "settings::EditPathOverride",
        json!({ "path": "/opt/bin:/usr/bin" }),
    )
    .unwrap();
    t.dispatch(
        cx,
        "settings::EditHelmPath",
        json!({ "path": helm.display().to_string() }),
    )
    .unwrap();
    let s = state(cx, &t);
    assert!(s.settings.field_errors.is_empty(), "{s:?}");
    assert_eq!(s.settings.edited, ["path", "helm"]);
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| s.helm.is_some()).await;
    let written = t.settings_toml();
    assert!(
        written.contains("path = \"/opt/bin:/usr/bin\""),
        "{written}"
    );

    t.dispatch(cx, "settings::EditHelmPath", json!({ "path": "" }))
        .unwrap();
    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| s.helm.is_none()).await;
    assert!(!t.settings_toml().contains("helm"));
}

// covers UP-127: turning local cluster protection on needs no confirmation, and the core's gate
// protects the local cluster once it is saved.
#[gpui_kit::test]
async fn turning_protection_on_saves_without_asking(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    assert_eq!(alpha_protection(cx, &t), "local");
    show(cx, &t, "safety");
    t.click(cx, "settings-protect-local-clusters");
    let s = state(cx, &t);
    assert!(s.settings.protect_local_clusters);
    assert!(t.find(cx, "settings-unprotect-note").is_none());
    t.click(cx, "settings-save");
    assert!(!has_dialog(cx, &t), "no confirmation to protect");
    wait_for_file(cx, &t, |s| s.protect_local_clusters).await;
    assert!(t.settings_toml().contains("protect_local_clusters = true"));
    assert_eq!(alpha_protection(cx, &t), "protected");
}

// covers UP-127: turning protection off asks at Save, saying what it exposes. Enter and Cancel
// leave the whole draft unsaved; only the button turns it off.
#[gpui_kit::test]
async fn turning_protection_off_asks_and_cancel_keeps_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_protected(cx, dir.path()).await;
    assert_eq!(alpha_protection(cx, &t), "protected");
    show(cx, &t, "safety");
    t.dispatch(
        cx,
        "settings::EditProtectLocalClusters",
        json!({ "enabled": false }),
    )
    .unwrap();
    t.dispatch(cx, "settings::EditLogBufferMib", json!({ "mib": 32 }))
        .unwrap();
    assert!(t.find(cx, "settings-unprotect-note").is_some());
    let before = t.settings_toml();

    t.click(cx, "settings-save");
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(dialog.title, UNPROTECT_TITLE);
    assert!(
        dialog.text.contains("no longer be gated"),
        "{}",
        dialog.text
    );
    assert!(!state(cx, &t).settings.saving, "nothing is sent yet");
    // Enter cancels this one.
    t.press(cx, "enter");
    no_dialog(cx, &t).await;
    let s = state(cx, &t);
    assert!(s.settings.dirty);
    assert_eq!(
        s.settings.edited,
        ["protect_local_clusters", "log_buffer_mib"]
    );
    assert_eq!(t.settings_toml(), before, "Cancel saves nothing at all");
    assert_eq!(alpha_protection(cx, &t), "protected");

    // The palette's Save asks too, and Cancel does the same.
    t.dispatch(cx, "settings::SaveSettings", Value::Null)
        .unwrap();
    shown_dialog(cx, &t).await;
    t.click(cx, "cancel");
    no_dialog(cx, &t).await;
    assert_eq!(t.settings_toml(), before);

    t.click(cx, "settings-save");
    shown_dialog(cx, &t).await;
    t.click(cx, "ok");
    wait_for_file(cx, &t, |s| !s.protect_local_clusters).await;
    let written = t.settings_toml();
    assert!(
        written.contains("protect_local_clusters = false"),
        "{written}"
    );
    assert!(written.contains("log_buffer_mib = 32"), "{written}");
    assert!(!state(cx, &t).settings.dirty);
    assert_eq!(alpha_protection(cx, &t), "local");
}

// covers UP-127: if settings.toml changes while the confirmation is open, confirming saves
// nothing; the next Save asks again, saying the file changed, and keeps the outside change.
#[gpui_kit::test]
async fn a_change_on_disk_during_the_confirmation_saves_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_protected(cx, dir.path()).await;
    show(cx, &t, "safety");
    t.click(cx, "settings-protect-local-clusters");
    assert!(!state(cx, &t).settings.protect_local_clusters);

    t.click(cx, "settings-save");
    shown_dialog(cx, &t).await;
    // What the Alerts menu's toggle does, while the question is open.
    t.model
        .update(cx, |model, _| model.set_desktop_notifications(true));
    wait_for_file(cx, &t, |s| s.desktop_notifications).await;
    t.click(cx, "ok");
    no_dialog(cx, &t).await;
    let s = state(cx, &t);
    assert_eq!(s.settings.note.as_deref(), Some(UNPROTECT_STALE));
    assert!(s.settings.dirty && s.settings.changed_on_disk, "{s:?}");
    assert!(
        t.settings_toml().contains("protect_local_clusters = true"),
        "not loosened"
    );
    assert_eq!(alpha_protection(cx, &t), "protected");

    // Save anyway asks again, over the file as it is now.
    t.click(cx, "settings-save-anyway");
    let dialog = shown_dialog(cx, &t).await;
    assert!(
        dialog.text.contains("changed outside Settings"),
        "{}",
        dialog.text
    );
    t.click(cx, "ok");
    wait_for_file(cx, &t, |s| !s.protect_local_clusters).await;
    let written = t.settings_toml();
    assert!(
        written.contains("desktop_notifications = true"),
        "{written}"
    );
    assert!(!state(cx, &t).settings.dirty);
}

// covers UP-127: Save in the quit question asks about protection too; cancelling that keeps the
// app open and the draft unsaved.
#[gpui_kit::test]
async fn quit_with_protection_off_asks_before_saving(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_protected(cx, dir.path()).await;
    t.dispatch(
        cx,
        "settings::EditProtectLocalClusters",
        json!({ "enabled": false }),
    )
    .unwrap();

    t.press_key_of(cx, "flint::Quit");
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(dialog.title, "Save your settings?");
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    cx.run_until_parked();
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(dialog.title, UNPROTECT_TITLE);
    t.click(cx, "cancel");
    no_dialog(cx, &t).await;
    assert_eq!(
        cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0)),
        0
    );
    assert!(state(cx, &t).settings.dirty);
    assert!(t.settings_toml().contains("protect_local_clusters = true"));
}

// covers UP-127: Revert puts the file's value back in the field being typed in too.
#[gpui_kit::test]
async fn revert_resets_the_focused_field(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "logs");
    for _ in 0..1 + Section::ALL.len() + 1 {
        t.press(cx, "tab");
    }
    t.press(cx, "secondary-a");
    t.type_text(cx, "5000");
    assert_eq!(state(cx, &t).settings.edited, ["log_buffer_lines"]);
    t.dispatch(cx, "settings::RevertSettings", Value::Null)
        .unwrap();
    let text = t.workspace.read_with(cx, |ws, cx| {
        ws.settings_field_text(TextField::LogBufferLines, cx)
    });
    assert_eq!(text.as_deref(), Some("100000"));
    assert!(!state(cx, &t).settings.dirty);
}

// covers UP-127: Save with a field's text invalid writes nothing and shows that field: its
// section, the cursor on it. Such a draft is unsaved too: quitting asks to discard it.
#[gpui_kit::test]
async fn save_shows_the_invalid_field_and_quit_still_asks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "logs");
    t.dispatch(cx, "settings::EditLogTotalMib", json!({ "mib": 20 }))
        .unwrap();
    let s = state(cx, &t);
    assert!(s.settings.dirty && !s.settings.can_save, "{s:?}");
    show(cx, &t, "appearance");

    t.click(cx, "settings-save");
    let s = state(cx, &t);
    assert_eq!(s.settings.section, Section::Logs);
    assert_eq!(s.settings.cursor.as_deref(), Some("settings-log-total-mib"));
    assert!(!dir.path().join("settings.toml").exists());

    t.press_key_of(cx, "flint::Quit");
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(dialog.title, "Discard unsaved settings?");
    t.click(cx, "cancel");
    no_dialog(cx, &t).await;
    assert_eq!(
        cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0)),
        0
    );
}

// covers UP-127: text typed into a field while a Save is being written joins the draft once the
// Save is answered; a quit that waited on the Save waits for that edit too.
#[gpui_kit::test]
async fn text_typed_while_saving_stays_in_the_draft(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "logs");
    t.dispatch(
        cx,
        "settings::EditLogBufferLines",
        json!({ "lines": 20_000 }),
    )
    .unwrap();
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        super::registry::dispatch("settings::SaveSettings", Value::Null, window, cx).unwrap();
        workspace.update(cx, |ws, cx| {
            assert!(ws.settings_saving());
            ws.type_settings_text(TextField::LogBufferMib, "128", window, cx);
        });
    })
    .unwrap();
    t.press_key_of(cx, "flint::Quit");
    wait_for_file(cx, &t, |s| s.log_buffer_lines == 20_000).await;
    cx.run_until_parked();

    let s = state(cx, &t);
    assert_eq!(s.settings.edited, ["log_buffer_mib"], "{s:?}");
    assert_eq!(s.settings.log_buffer_mib, 128);
    assert!(
        s.settings
            .note
            .as_deref()
            .is_some_and(|n| n.contains("Edits made while saving")),
        "{s:?}"
    );
    assert_eq!(
        cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0)),
        0,
        "the quit waits for the new edit"
    );
}

// covers UP-127: every control of the Logs and Paths sections has a row of its own to scroll to,
// in the order the keyboard reaches them.
#[test]
fn each_control_has_a_row_to_scroll_to() {
    for kubeconfigs in [0, 1, 5] {
        let rows: Vec<usize> = paths_stops(kubeconfigs)
            .into_iter()
            .map(|stop| content_row(stop, kubeconfigs).expect("in the content"))
            .collect();
        assert!(rows.windows(2).all(|w| w[0] < w[1]), "{rows:?}");
        assert_eq!(rows.last(), Some(&(kubeconfigs.max(1) + 3)));
    }
    assert_eq!(content_row(Stop::Text(TextField::LogTotalMib), 0), Some(2));
    assert_eq!(content_row(Stop::Save, 0), None);
}

// covers UP-127: Remove on a file listed twice drops the row clicked, keeping the others in order.
#[gpui_kit::test]
async fn remove_drops_the_row_clicked(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "kubeconfig_paths = [\"/k/a\", \"/k/b\", \"/k/a\"]\n",
    )
    .unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "paths");
    t.dispatch(
        cx,
        "settings::RemoveKubeconfigPath",
        json!({ "path": "/k/a", "index": 2 }),
    )
    .unwrap();
    assert_eq!(
        state(cx, &t).settings.kubeconfig_paths,
        ["/k/a", "/k/b"],
        "the last row went"
    );
}

// covers UP-127: text typed while saving that matches what the file held before the Save is
// still an edit: the Save changes that value.
#[gpui_kit::test]
async fn the_old_value_typed_while_saving_stays_an_edit(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    show(cx, &t, "logs");
    t.dispatch(
        cx,
        "settings::EditLogBufferLines",
        json!({ "lines": 20_000 }),
    )
    .unwrap();
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        super::registry::dispatch("settings::SaveSettings", Value::Null, window, cx).unwrap();
        workspace.update(cx, |ws, cx| {
            assert!(ws.settings_saving());
            ws.type_settings_text(TextField::LogBufferLines, "100000", window, cx);
        });
    })
    .unwrap();
    wait_for_file(cx, &t, |s| s.log_buffer_lines == 20_000).await;
    cx.run_until_parked();
    let s = state(cx, &t);
    assert_eq!(s.settings.edited, ["log_buffer_lines"], "{s:?}");
    assert_eq!(s.settings.log_buffer_lines, 100_000);
}
