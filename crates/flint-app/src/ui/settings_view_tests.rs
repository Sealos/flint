//! View tests for the Settings view (#740): opening it from its key and the palette, the draft
//! with Save and Revert, the banner when `settings.toml` changes under a draft, and the
//! Save / Discard / Cancel question at quit and window close.

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::closing::Quits;
use super::registry::Scope;
use super::registry::tests::assert_binds_scope;
use super::support_tests::{TestWorkspace, WAIT, open_workspace};
use super::view_state::{AppState, DialogState, Focus, Main};

fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> AppState {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.view_state(window, cx))
    })
    .unwrap()
}

fn quits(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0))
}

/// The workspace, the front window, with Settings opened by its key.
async fn open_settings(cx: &mut TestAppContext, dir: &std::path::Path) -> TestWorkspace {
    let t = open_workspace(cx, dir).await;
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    t.focus_workspace(cx);
    t.press_key_of(cx, "flint::OpenSettings");
    t
}

fn edit_appearance(cx: &mut TestAppContext, t: &TestWorkspace, appearance: &str) {
    t.dispatch(
        cx,
        "settings::EditAppearance",
        json!({ "appearance": appearance }),
    )
    .unwrap();
}

/// Waits until the core has reported `wanted` of the settings it holds.
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

// covers UP-126: Cmd-, (Ctrl-, on Linux) opens Settings in the main area with focus in it, and
// Esc leaves it. The palette offers it too, and the menu item is the same registered action.
#[gpui_kit::test]
async fn the_shortcut_opens_settings_and_esc_leaves_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.focus_workspace(cx);
    assert!(t.available(cx).contains(&"flint::OpenSettings"));
    assert!(!state(cx, &t).settings.open);

    t.press_key_of(cx, "flint::OpenSettings");
    let s = state(cx, &t);
    assert!(s.settings.open);
    assert_eq!(s.main, Main::Settings);
    assert_eq!(s.focus, Focus::Settings);
    assert!(t.find(cx, "settings-view").is_some());
    assert_eq!(
        s.settings.appearance, "system",
        "it shows settings.toml: {s:?}"
    );
    assert!(!s.settings.dirty && !s.settings.can_save);

    t.dispatch(
        cx,
        "settings::ShowSettingsSection",
        json!({ "section": "watching" }),
    )
    .unwrap();
    assert!(t.find(cx, "settings-watch-on-view").is_some());

    t.press(cx, "escape");
    let s = state(cx, &t);
    assert!(!s.settings.open);
    assert_ne!(s.main, Main::Settings);
    assert!(t.find(cx, "settings-view").is_none());
}

// covers UP-126: edits wait in a draft; Save writes only the edited fields, through the same
// commands as the other controls, and the window takes the new appearance.
#[gpui_kit::test]
async fn save_writes_only_the_edited_fields(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    edit_appearance(cx, &t, "dark");
    let s = state(cx, &t);
    assert!(s.settings.dirty && s.settings.can_save);
    assert_eq!(s.settings.edited, ["appearance"]);
    assert_eq!(s.settings.appearance, "dark");
    assert_eq!(s.appearance, "system", "nothing applies before Save");
    assert!(t.settings_toml().is_empty(), "nothing written before Save");

    // Changing a control back leaves nothing to save.
    t.dispatch(cx, "settings::EditWatchMode", json!({ "mode": "on_view" }))
        .unwrap();
    t.dispatch(cx, "settings::EditWatchMode", json!({ "mode": "all" }))
        .unwrap();
    assert_eq!(state(cx, &t).settings.edited, ["appearance"]);

    t.click(cx, "settings-save");
    wait_for_file(cx, &t, |s| s.appearance == flint_core::Appearance::Dark).await;
    let s = state(cx, &t);
    assert!(!s.settings.dirty);
    assert_eq!(s.appearance, "dark");
    let written = t.settings_toml();
    assert!(written.contains("appearance = \"dark\""), "{written}");
    assert!(!written.contains("watch_mode"), "{written}");
    assert!(!written.contains("desktop_notifications"), "{written}");
}

// covers UP-126: Revert drops the draft; the draft outlives leaving Settings, and the sidebar's
// entry shows a dot meanwhile.
#[gpui_kit::test]
async fn revert_drops_the_draft_which_survives_leaving_settings(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(
        cx,
        "settings::EditDesktopNotifications",
        json!({ "enabled": true }),
    )
    .unwrap();
    assert!(t.find(cx, "settings-entry-dot").is_some());

    // Another cluster in the main area: Settings gives way, and the draft stays.
    t.activate(cx, "kind-alpha").await;
    let s = state(cx, &t);
    assert!(!s.settings.open, "{s:?}");
    assert!(s.settings.dirty);
    assert!(s.settings.desktop_notifications);
    assert!(t.find(cx, "settings-entry-dot").is_some());

    t.click(cx, "open-settings");
    assert!(state(cx, &t).settings.open);
    assert!(state(cx, &t).settings.dirty);
    t.click(cx, "settings-revert");
    let s = state(cx, &t);
    assert!(!s.settings.dirty);
    assert!(!s.settings.desktop_notifications);
    assert!(t.find(cx, "settings-entry-dot").is_none());
    assert!(t.settings_toml().is_empty());
}

// covers UP-126: a change to settings.toml under a draft (here from the Alerts menu) raises the
// banner. Save anyway writes the edited field alone, so the outside change survives.
#[gpui_kit::test]
async fn a_change_under_the_draft_shows_the_banner_and_save_anyway_keeps_it(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    edit_appearance(cx, &t, "light");
    assert!(!state(cx, &t).settings.changed_on_disk);
    assert!(t.find(cx, "settings-changed-on-disk").is_none());

    // What the Alerts menu's toggle does (installed app-wide in the app, not in these tests).
    t.model
        .update(cx, |model, _| model.set_desktop_notifications(true));
    wait_for_file(cx, &t, |s| s.desktop_notifications).await;
    let s = state(cx, &t);
    assert!(s.settings.changed_on_disk, "{s:?}");
    assert!(
        s.settings.desktop_notifications,
        "unedited fields follow the file"
    );
    assert_eq!(s.settings.appearance, "light");
    assert!(t.find(cx, "settings-changed-on-disk").is_some());

    t.click(cx, "settings-save-anyway");
    wait_for_file(cx, &t, |s| s.appearance == flint_core::Appearance::Light).await;
    let written = t.settings_toml();
    assert!(written.contains("appearance = \"light\""), "{written}");
    assert!(
        written.contains("desktop_notifications = true"),
        "{written}"
    );
    assert!(!state(cx, &t).settings.changed_on_disk);
}

// covers UP-126: Reload on the banner drops the draft and shows the file.
#[gpui_kit::test]
async fn reload_shows_the_file(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(cx, "settings::EditWatchMode", json!({ "mode": "on_view" }))
        .unwrap();
    // What View > Appearance > Dark does.
    t.model.update(cx, |model, _| {
        model.set_appearance(flint_core::Appearance::Dark)
    });
    wait_for_file(cx, &t, |s| s.appearance == flint_core::Appearance::Dark).await;
    assert!(state(cx, &t).settings.changed_on_disk);

    t.click(cx, "settings-reload");
    let s = state(cx, &t);
    assert!(!s.settings.dirty && !s.settings.changed_on_disk);
    assert_eq!(s.settings.appearance, "dark");
    assert_eq!(s.settings.watch_mode, "all");
    assert!(!t.settings_toml().contains("watch_mode"));
}

// covers UP-126: with settings.toml invalid, the view says so and Save is disabled.
#[gpui_kit::test]
async fn an_invalid_file_disables_save(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.toml"), "appearance = [\n").unwrap();
    let t = open_settings(cx, dir.path()).await;
    edit_appearance(cx, &t, "dark");
    let s = state(cx, &t);
    assert!(s.settings.invalid && s.settings.dirty);
    assert!(!s.settings.can_save);
    assert!(t.find(cx, "settings-invalid").is_some());
    assert!(
        t.dispatch(cx, "settings::SaveSettings", Value::Null)
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("settings.toml")).unwrap(),
        "appearance = [\n"
    );
}

// covers UP-126: quitting with a draft asks Save / Discard / Cancel. Cancel stays, Discard quits
// without writing.
#[gpui_kit::test]
async fn quit_with_a_draft_asks_and_discard_quits(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    edit_appearance(cx, &t, "dark");

    t.press_key_of(cx, "flint::Quit");
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(dialog.title, "Save your settings?");
    assert!(dialog.text.contains("Quitting"), "{}", dialog.text);
    assert!(t.find(cx, "alternative").is_some(), "Discard is offered");
    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    assert_eq!(quits(cx), 0);
    assert!(state(cx, &t).settings.dirty, "Cancel keeps the draft");

    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.click(cx, "alternative");
    cx.wait_for(t.window, WAIT, |_, cx| {
        cx.try_global::<Quits>().is_some_and(|q| q.0 == 1)
    })
    .await;
    assert!(!state(cx, &t).settings.dirty);
    assert!(t.settings_toml().is_empty(), "Discard writes nothing");
}

// covers UP-126: Save in the quit question writes the draft, then quits.
#[gpui_kit::test]
async fn quit_with_a_draft_saves_then_quits(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(cx, "settings::EditWatchMode", json!({ "mode": "on_view" }))
        .unwrap();

    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |_, cx| {
        cx.try_global::<Quits>().is_some_and(|q| q.0 == 1)
    })
    .await;
    let written = t.settings_toml();
    assert!(written.contains("watch_mode = \"on_view\""), "{written}");
}

// covers UP-126: closing the window with a draft asks too; Cancel keeps the window.
#[gpui_kit::test]
async fn close_window_with_a_draft_asks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    edit_appearance(cx, &t, "light");
    t.dispatch(cx, "flint::CloseWindow", Value::Null).unwrap();
    let dialog = shown_dialog(cx, &t).await;
    assert!(
        dialog.text.contains("Closing the window"),
        "{}",
        dialog.text
    );
    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    assert!(cx.update_window(t.window, |_, _, _| ()).is_ok());
    assert!(state(cx, &t).settings.dirty);
}

#[test]
fn every_settings_action_is_bound() {
    assert_binds_scope(
        Scope::Settings,
        super::workspace::Workspace::bind_settings_actions,
    );
}

// covers UP-126: opening the kind shown under Settings brings it to the front.
#[gpui_kit::test]
async fn opening_the_kind_under_settings_leaves_settings(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    t.focus_workspace(cx);
    t.press_key_of(cx, "flint::OpenSettings");
    edit_appearance(cx, &t, "dark");
    assert_eq!(state(cx, &t).main, Main::Settings);
    t.dispatch(cx, "workspace::OpenView", json!({ "kind": "pods" }))
        .unwrap();
    let s = state(cx, &t);
    assert!(!s.settings.open, "{s:?}");
    assert_eq!(s.main, Main::List);
    assert!(s.settings.dirty, "the draft stays");
}

// covers UP-126: a Save keeps its edits until the core says they are written, and a Quit pressed
// meanwhile waits for that answer instead of exiting first.
#[gpui_kit::test]
async fn quit_while_a_save_is_pending_waits_for_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(cx, "settings::EditWatchMode", json!({ "mode": "on_view" }))
        .unwrap();
    let workspace = t.workspace.clone();
    let pending = cx
        .update_window(t.window, |_, window, cx| {
            super::registry::dispatch("settings::SaveSettings", Value::Null, window, cx).unwrap();
            let pending = workspace.read(cx).has_settings_draft();
            super::registry::dispatch("flint::Quit", Value::Null, window, cx).unwrap();
            pending
        })
        .unwrap();
    assert!(
        pending,
        "the edits stay in the draft until the core answers"
    );
    cx.wait_for(t.window, WAIT, |_, cx| {
        cx.try_global::<Quits>().is_some_and(|q| q.0 == 1)
    })
    .await;
    assert!(
        !cx.update_window(t.window, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    );
    let written = t.settings_toml();
    assert!(written.contains("watch_mode = \"on_view\""), "{written}");
}

// covers UP-126: a Save the core refuses keeps the edits and says why.
#[gpui_kit::test]
async fn a_refused_save_keeps_the_edits(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    edit_appearance(cx, &t, "dark");
    // Broken behind the core's back (these tests do not watch the file): the write fails.
    std::fs::write(dir.path().join("settings.toml"), "appearance = [\n").unwrap();
    t.click(cx, "settings-save");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |window, cx| {
        !workspace
            .update(cx, |ws, cx| ws.view_state(window, cx))
            .settings
            .saving
    })
    .await;
    let s = state(cx, &t);
    assert!(s.settings.dirty, "{s:?}");
    assert_eq!(s.settings.appearance, "dark");
    let note = s.settings.note.expect("the footer says why");
    assert!(note.starts_with("Not saved"), "{note}");
}

// covers UP-126: Tab and Shift-Tab go round the controls of Settings, never the panes hidden
// under it, and Enter or Space works the control under the cursor.
#[gpui_kit::test]
async fn the_keyboard_alone_changes_and_saves_a_setting(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.press(cx, "tab");
    assert_eq!(state(cx, &t).settings.cursor, Some("settings-close"));
    // Done, the sections, then System, Light and Dark.
    for _ in 0..super::settings_view::Section::ALL.len() + 3 {
        t.press(cx, "tab");
    }
    let s = state(cx, &t);
    assert_eq!(s.settings.cursor, Some("settings-appearance-dark"));
    assert_eq!(s.focus, Focus::Settings, "Tab stays in Settings");
    t.press(cx, "enter");
    assert_eq!(state(cx, &t).settings.edited, ["appearance"]);

    // Round to the start and back: Shift-Tab from Done wraps to Save.
    t.press(cx, "tab");
    t.press(cx, "tab");
    assert_eq!(state(cx, &t).settings.cursor, Some("settings-save"));
    t.press(cx, "tab");
    assert_eq!(state(cx, &t).settings.cursor, Some("settings-close"));
    t.press(cx, "shift-tab");
    assert_eq!(state(cx, &t).settings.cursor, Some("settings-save"));
    t.press(cx, "space");
    wait_for_file(cx, &t, |s| s.appearance == flint_core::Appearance::Dark).await;
    assert!(!state(cx, &t).settings.dirty);

    // A section, from the keyboard.
    t.press(cx, "tab");
    t.press(cx, "tab");
    t.press(cx, "tab");
    assert_eq!(
        state(cx, &t).settings.cursor,
        Some("settings-section-alerts")
    );
    t.press(cx, "enter");
    assert!(t.find(cx, "settings-desktop-notifications").is_some());
}

// covers UP-126: with settings.toml invalid, the quit question only discards, and Enter cancels
// it: discarding takes the button.
#[gpui_kit::test]
async fn enter_does_not_discard_when_the_file_is_invalid(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.toml"), "appearance = [\n").unwrap();
    let t = open_settings(cx, dir.path()).await;
    edit_appearance(cx, &t, "dark");
    t.press_key_of(cx, "flint::Quit");
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(dialog.title, "Discard unsaved settings?");
    t.press(cx, "enter");
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    assert_eq!(quits(cx), 0);
    assert!(state(cx, &t).settings.dirty);
}

// covers UP-127: a Save that stops part way names what was saved, what was refused and why, and
// what was not tried.
#[test]
fn a_partial_save_says_what_was_and_was_not_saved() {
    use super::settings_draft::Change;
    use super::workspace::settings_entry::save_failure_note;
    use flint_core::settings::SettingsWriteFailure;
    let writes = [
        Change::DesktopNotifications(true),
        Change::LogBufferLines(5_000),
        Change::Path(Some("/bin".into())),
        Change::Helm(None),
    ];
    let failure = |written| SettingsWriteFailure {
        written,
        why: "disk full".into(),
    };
    assert_eq!(
        save_failure_note(&writes, &failure(2)),
        "Saved desktop notifications and the log buffer size. Not saved: the PATH override: \
         disk full. Also not saved: the helm path"
    );
    assert_eq!(
        save_failure_note(&writes, &failure(0)),
        "Not saved: desktop notifications: disk full. Also not saved: the log buffer size, the \
         PATH override and the helm path"
    );
    assert_eq!(
        save_failure_note(&writes[..1], &failure(0)),
        "Not saved: desktop notifications: disk full"
    );
}
