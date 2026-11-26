//! View tests for the Settings view (#740) and its modal (#854): opening it from each entry
//! point, closing it by Esc, its button or a click outside, the Discard / Keep editing question,
//! the draft with Save and Revert, the banner when `settings.toml` changes under a draft, and the
//! Save / Discard / Cancel question at quit and window close.

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::closing::Quits;
use super::registry::Scope;
use super::registry::tests::assert_binds_scope;
use super::support_tests::{TestWorkspace, WAIT, open_workspace};
use super::view_state::{AppState, DialogState, Focus};

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
    let window = t.window;
    cx.wait_for(window, WAIT, |window, cx| {
        super::confirm::any_open(window, cx)
    })
    .await;
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    state(cx, t).dialog.expect("a dialog is open")
}

/// Whether the Settings modal is open, as a dialog over the window, and Settings knows it.
fn modal_open(cx: &mut TestAppContext, t: &TestWorkspace) -> bool {
    let open = state(cx, t).settings.open;
    assert_eq!(open, t.has_dialog(cx), "the modal and the view agree");
    assert_eq!(open, t.find(cx, "settings-modal").is_some());
    open
}

// covers UP-126, #854: Cmd-, (Ctrl-, on Linux) opens Settings as a modal over the main window
// with focus in it, and Esc closes it, giving focus back. The palette offers it too, and the
// menu item is the same registered action.
#[gpui_kit::test]
async fn the_shortcut_opens_settings_and_esc_leaves_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.focus_workspace(cx);
    assert!(t.available(cx).contains(&"flint::OpenSettings"));
    assert!(!modal_open(cx, &t));
    let main = state(cx, &t).main;

    t.press_key_of(cx, "flint::OpenSettings");
    let s = state(cx, &t);
    assert!(modal_open(cx, &t));
    assert_eq!(s.main, main, "Settings never replaces the main area");
    assert_eq!(s.focus, Focus::Settings);
    assert!(s.dialog.is_none(), "no confirmation: {s:?}");
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
    assert!(!modal_open(cx, &t));
    assert_eq!(s.focus, Focus::Workspace, "focus is back where it was");
    assert!(t.find(cx, "settings-view").is_none());
}

// covers #854: the sidebar's entry, the menu's action and Show Settings Section each open the
// modal, the last at its section; with it open, they open no second one.
#[gpui_kit::test]
async fn every_entry_point_opens_the_modal(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.focus_workspace(cx);

    t.click(cx, "open-settings");
    assert!(modal_open(cx, &t));
    assert_eq!(state(cx, &t).focus, Focus::Settings);
    t.click(cx, "settings-close");
    assert!(!modal_open(cx, &t));

    t.focus_workspace(cx);
    t.dispatch(cx, "flint::OpenSettings", Value::Null).unwrap();
    assert!(modal_open(cx, &t));
    t.press(cx, "escape");
    assert!(!modal_open(cx, &t));

    t.focus_workspace(cx);
    t.dispatch(
        cx,
        "settings::ShowSettingsSection",
        json!({ "section": "network" }),
    )
    .unwrap();
    assert!(modal_open(cx, &t));
    let s = state(cx, &t);
    assert_eq!(s.settings.section, super::settings_view::Section::Network);
    assert_eq!(s.focus, Focus::Settings);

    // Open already: the same modal, nothing stacked over it.
    cx.update_window(t.window, |_, window, cx| {
        super::registry::perform(&super::settings_view::OpenSettings, window, cx).unwrap();
    })
    .unwrap();
    cx.run_until_parked();
    t.press(cx, "escape");
    assert!(!modal_open(cx, &t), "one Esc closes the only modal");
}

// covers #854: a click outside the modal closes it, and the close button does too; without
// edits nothing is asked.
#[gpui_kit::test]
async fn a_click_outside_or_the_close_button_closes_it(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, VisualTestContext, point, px};

    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    assert!(modal_open(cx, &t));
    let viewport = cx
        .update_window(t.window, |_, window, _| window.viewport_size())
        .unwrap();
    // Low on the left edge: under the modal's backdrop, clear of the title bar.
    VisualTestContext::from_window(t.window, cx)
        .simulate_click(point(px(4.), viewport.height - px(4.)), Modifiers::none());
    cx.run_until_parked();
    assert!(!modal_open(cx, &t));
    assert!(!t.has_confirmation(cx), "nothing to ask");

    t.focus_workspace(cx);
    t.press_key_of(cx, "flint::OpenSettings");
    // A click inside does not close it.
    t.click(cx, "settings-section-alerts");
    assert!(modal_open(cx, &t));
    t.click(cx, "settings-close");
    assert!(!modal_open(cx, &t));
    assert_eq!(state(cx, &t).focus, Focus::Workspace);
}

// covers #854: closing with unsaved edits asks Discard / Keep editing, whichever way it is
// closed. Keep editing keeps the modal and the draft; Discard drops the draft and closes it, and
// Settings opens again on the file. The sidebar's entry has no unsaved-edits dot.
#[gpui_kit::test]
async fn closing_with_edits_asks_discard_or_keep_editing(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, VisualTestContext, point, px};

    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(
        cx,
        "settings::EditDesktopNotifications",
        json!({ "enabled": true }),
    )
    .unwrap();
    assert!(t.find(cx, "settings-entry-dot").is_none());

    // Esc: asks; Enter keeps editing.
    t.press(cx, "escape");
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(
        dialog.title,
        super::workspace::settings_entry::DISCARD_TITLE
    );
    assert_eq!(dialog.buttons, ["Discard", "Keep editing"]);
    t.press(cx, "enter");
    t.wait_for_no_confirmation(cx).await;
    assert!(modal_open(cx, &t));
    let s = state(cx, &t);
    assert!(s.settings.dirty && s.settings.desktop_notifications);
    assert_eq!(s.focus, Focus::Settings, "focus is back in Settings");

    // The close button: asks; Keep editing.
    t.click(cx, "settings-close");
    shown_dialog(cx, &t).await;
    t.click(cx, "cancel");
    t.wait_for_no_confirmation(cx).await;
    assert!(modal_open(cx, &t) && state(cx, &t).settings.dirty);

    // A click outside: asks; Discard.
    let viewport = cx
        .update_window(t.window, |_, window, _| window.viewport_size())
        .unwrap();
    VisualTestContext::from_window(t.window, cx)
        .simulate_click(point(px(4.), viewport.height - px(4.)), Modifiers::none());
    cx.run_until_parked();
    shown_dialog(cx, &t).await;
    t.click(cx, "ok");
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    let s = state(cx, &t);
    assert!(!s.settings.open && !s.settings.dirty, "{s:?}");
    assert!(t.settings_toml().is_empty(), "Discard writes nothing");

    t.click(cx, "open-settings");
    let s = state(cx, &t);
    assert!(s.settings.open && !s.settings.dirty);
    assert!(!s.settings.desktop_notifications, "it shows the file again");
}

// covers #854: a threshold field left open counts as an edit; Discard drops what was typed.
#[gpui_kit::test]
async fn an_open_threshold_field_asks_before_closing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(
        cx,
        "settings::ShowSettingsSection",
        json!({ "section": "thresholds" }),
    )
    .unwrap();
    t.click(cx, "settings-threshold-cpu_percent");
    assert!(state(cx, &t).settings.thresholds.typing.is_some());
    t.click(cx, "settings-close");
    assert_eq!(
        shown_dialog(cx, &t).await.title,
        super::workspace::settings_entry::DISCARD_TITLE
    );
    t.click(cx, "ok");
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    let s = state(cx, &t);
    assert!(!s.settings.open && s.settings.thresholds.typing.is_none());
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
    assert!(modal_open(cx, &t), "Save keeps Settings open");
    assert_eq!(s.focus, Focus::Settings);
    // Saved: nothing to ask, and Esc closes at once.
    t.press(cx, "escape");
    assert!(!modal_open(cx, &t));
    assert!(!t.has_confirmation(cx));
    let written = t.settings_toml();
    assert!(written.contains("appearance = \"dark\""), "{written}");
    assert!(!written.contains("watch_mode"), "{written}");
    assert!(!written.contains("desktop_notifications"), "{written}");
}

// covers UP-126: Revert drops the draft and keeps Settings open; with nothing left to lose,
// closing asks nothing.
#[gpui_kit::test]
async fn revert_drops_the_draft(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(
        cx,
        "settings::EditDesktopNotifications",
        json!({ "enabled": true }),
    )
    .unwrap();
    assert!(state(cx, &t).settings.dirty);
    t.click(cx, "settings-revert");
    let s = state(cx, &t);
    assert!(!s.settings.dirty);
    assert!(!s.settings.desktop_notifications);
    assert!(modal_open(cx, &t));
    assert!(t.settings_toml().is_empty());
    t.press(cx, "escape");
    assert!(!modal_open(cx, &t));
    assert!(!t.has_confirmation(cx));
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
    t.wait_for_no_confirmation(cx).await;
    assert_eq!(quits(cx), 0);
    assert!(state(cx, &t).settings.dirty, "Cancel keeps the draft");
    assert!(modal_open(cx, &t), "Settings stays open under the question");

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

// covers UP-126: the quit question says a failed Save keeps Flint open.
#[gpui_kit::test]
async fn the_quit_question_says_a_failed_save_stops_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    edit_appearance(cx, &t, "dark");
    t.press_key_of(cx, "flint::Quit");
    let dialog = shown_dialog(cx, &t).await;
    assert!(
        dialog.text.contains("if the save fails, Flint stays open"),
        "{}",
        dialog.text
    );
    assert!(!dialog.text.contains("either way"), "{}", dialog.text);
}

// covers UP-126: a proxy typed and not applied is asked about at quit and window close, with
// wording that offers no Save, and Discard drops it.
#[gpui_kit::test]
async fn quit_and_close_ask_about_an_unapplied_proxy(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(
        cx,
        "settings::ShowSettingsSection",
        json!({ "section": "network" }),
    )
    .unwrap();
    t.dispatch(
        cx,
        "settings::EditProxyUrl",
        json!({ "url": "http://127.0.0.1:9" }),
    )
    .unwrap();
    assert!(!state(cx, &t).settings.dirty, "only the proxy is typed");

    t.press_key_of(cx, "flint::Quit");
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(
        dialog.text,
        "The proxy you typed is not applied. Quitting drops it."
    );
    assert!(t.find(cx, "alternative").is_none(), "no Save is offered");
    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    t.wait_for_no_confirmation(cx).await;
    assert_eq!(quits(cx), 0);
    assert!(state(cx, &t).settings.network.typed, "Cancel keeps it");

    t.dispatch(cx, "flint::CloseWindow", Value::Null).unwrap();
    let dialog = shown_dialog(cx, &t).await;
    assert!(
        dialog
            .text
            .starts_with("The proxy you typed is not applied."),
        "{}",
        dialog.text
    );
    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    t.wait_for_no_confirmation(cx).await;

    t.press_key_of(cx, "flint::Quit");
    shown_dialog(cx, &t).await;
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |_, cx| {
        cx.try_global::<Quits>().is_some_and(|q| q.0 == 1)
    })
    .await;
    assert!(!state(cx, &t).settings.network.typed);
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
    t.wait_for_no_confirmation(cx).await;
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

// covers #854: closing while a Save is being written waits for it, then closes without asking.
#[gpui_kit::test]
async fn closing_while_a_save_is_pending_closes_once_written(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    t.dispatch(cx, "settings::EditWatchMode", json!({ "mode": "on_view" }))
        .unwrap();
    let saving = cx
        .update_window(t.window, |_, window, cx| {
            super::registry::dispatch("settings::SaveSettings", Value::Null, window, cx).unwrap();
            window.press("escape", cx);
            !super::confirm::any_open(window, cx)
        })
        .unwrap();
    assert!(saving, "nothing asked while the Save is written");
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    let s = state(cx, &t);
    assert!(!s.settings.open && !s.settings.dirty, "{s:?}");
    assert!(t.settings_toml().contains("watch_mode = \"on_view\""));
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
            let pending = workspace.read(cx).has_settings_draft(cx);
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
    assert!(!t.has_confirmation(cx));
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
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-close")
    );
    // Close, the sections, then System, Light and Dark.
    for _ in 0..super::settings_view::Section::ALL.len() + 3 {
        t.press(cx, "tab");
    }
    let s = state(cx, &t);
    assert_eq!(
        s.settings.cursor.as_deref(),
        Some("settings-appearance-dark")
    );
    assert_eq!(s.focus, Focus::Settings, "Tab stays in Settings");
    t.press(cx, "enter");
    assert_eq!(state(cx, &t).settings.edited, ["appearance"]);

    // Round to the start and back: Shift-Tab from Close wraps to Save.
    t.press(cx, "tab");
    t.press(cx, "tab");
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-save")
    );
    t.press(cx, "tab");
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-close")
    );
    t.press(cx, "shift-tab");
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
        Some("settings-save")
    );
    t.press(cx, "space");
    wait_for_file(cx, &t, |s| s.appearance == flint_core::Appearance::Dark).await;
    assert!(!state(cx, &t).settings.dirty);

    // A section, from the keyboard.
    t.press(cx, "tab");
    t.press(cx, "tab");
    t.press(cx, "tab");
    assert_eq!(
        state(cx, &t).settings.cursor.as_deref(),
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
    t.wait_for_no_confirmation(cx).await;
    assert_eq!(quits(cx), 0);
    assert!(state(cx, &t).settings.dirty);
}

// covers #854: focus stays in the modal: a view behind it that takes focus (the overview once
// discovery ends) gives it back.
#[gpui_kit::test]
async fn focus_taken_behind_the_modal_comes_back(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_settings(cx, dir.path()).await;
    assert_eq!(state(cx, &t).focus, Focus::Settings);
    t.focus_workspace(cx);
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(state(cx, &t).focus, Focus::Settings);
    assert!(modal_open(cx, &t));
}
