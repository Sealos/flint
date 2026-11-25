//! Unit tests for the Settings draft: edits, Revert, Save of the edited fields alone, and the
//! on-disk change that raises the banner.

use std::sync::Arc;

use flint_core::{Appearance, Settings, WatchMode};

use super::{Change, Field, SettingsDraft};

fn file() -> Arc<Settings> {
    Arc::new(Settings::default())
}

#[test]
fn without_edits_the_view_follows_the_file() {
    let draft = SettingsDraft::default();
    let on_disk = Settings {
        appearance: Appearance::Dark,
        ..Settings::default()
    };
    assert!(!draft.is_dirty());
    assert_eq!(draft.shown(&on_disk), on_disk);
    assert!(!draft.changed_on_disk(&on_disk));
}

#[test]
fn an_edit_shows_over_the_file_until_saved() {
    let file = file();
    let mut draft = SettingsDraft::default();
    draft.edit(Change::Appearance(Appearance::Dark), &file);
    assert!(draft.is_dirty());
    assert!(draft.is_edited(Field::Appearance));
    assert!(!draft.is_edited(Field::WatchMode));
    let shown = draft.shown(&file);
    assert_eq!(shown.appearance, Appearance::Dark);
    assert_eq!(shown.watch_mode, file.watch_mode);
}

#[test]
fn changing_a_control_back_leaves_nothing_to_save() {
    let file = file();
    let mut draft = SettingsDraft::default();
    draft.edit(Change::WatchMode(WatchMode::OnView), &file);
    draft.edit(Change::WatchMode(WatchMode::All), &file);
    assert!(!draft.is_dirty());
    assert!(draft.edits().is_empty());
}

#[test]
fn revert_drops_every_edit() {
    let file = file();
    let mut draft = SettingsDraft::default();
    draft.edit(Change::Appearance(Appearance::Light), &file);
    draft.edit(Change::DesktopNotifications(true), &file);
    draft.revert();
    assert!(!draft.is_dirty());
    assert_eq!(draft.shown(&file), *file);
}

#[test]
fn save_takes_only_the_edited_fields() {
    let file = file();
    let mut draft = SettingsDraft::default();
    draft.edit(Change::DesktopNotifications(true), &file);
    draft.edit(Change::Appearance(Appearance::Dark), &file);
    // Edited twice: the last value is the one saved.
    draft.edit(Change::Appearance(Appearance::Light), &file);
    let writes = draft.edits();
    draft.saved(&writes);
    assert_eq!(
        writes,
        [
            Change::Appearance(Appearance::Light),
            Change::DesktopNotifications(true),
        ]
    );
    assert!(!draft.is_dirty());
    assert!(writes.iter().all(|c| c.field() != Field::WatchMode));
}

#[test]
fn a_change_on_disk_under_a_draft_raises_the_banner() {
    let file = file();
    let mut draft = SettingsDraft::default();
    draft.edit(Change::Appearance(Appearance::Dark), &file);
    assert!(!draft.changed_on_disk(&file));

    // The Alerts menu (or an editor) turns notifications on meanwhile.
    let mut outside = (*file).clone();
    outside.desktop_notifications = true;
    let outside = Arc::new(outside);
    assert!(draft.changed_on_disk(&outside));
    // The draft shows the outside change for the fields it did not edit.
    let shown = draft.shown(&outside);
    assert!(shown.desktop_notifications);
    assert_eq!(shown.appearance, Appearance::Dark);

    // Save anyway writes the edited field alone, so the outside change survives.
    let writes = draft.edits();
    assert_eq!(writes, [Change::Appearance(Appearance::Dark)]);
    draft.saved(&writes);
    assert!(!draft.changed_on_disk(&outside));
}

#[test]
fn reload_discards_the_draft_and_shows_the_file() {
    let file = file();
    let mut draft = SettingsDraft::default();
    draft.edit(Change::WatchMode(WatchMode::OnView), &file);
    let mut outside = (*file).clone();
    outside.appearance = Appearance::Light;
    assert!(draft.changed_on_disk(&outside));
    draft.revert();
    assert!(!draft.changed_on_disk(&outside));
    assert_eq!(draft.shown(&outside), outside);
}

#[test]
fn fields_name_their_keys() {
    assert_eq!(Field::Appearance.key(), "appearance");
    assert_eq!(Field::DesktopNotifications.key(), "desktop_notifications");
    assert_eq!(Field::WatchMode.key(), "watch_mode");
}

#[test]
fn an_edit_stays_until_the_core_says_it_is_written() {
    let file = file();
    let mut draft = SettingsDraft::default();
    draft.edit(Change::Appearance(Appearance::Dark), &file);
    draft.edit(Change::WatchMode(WatchMode::OnView), &file);
    let writes = draft.edits();
    assert!(draft.is_dirty(), "sent, not yet written");
    // Only the watch mode was written; the appearance was edited again meanwhile.
    draft.edit(Change::Appearance(Appearance::Light), &file);
    draft.saved(&writes);
    assert!(draft.is_edited(Field::Appearance));
    assert!(!draft.is_edited(Field::WatchMode));
    assert_eq!(draft.shown(&file).appearance, Appearance::Light);
}

#[test]
fn changes_map_to_the_core_writes() {
    use flint_core::settings::SettingsWrite;
    assert_eq!(
        Change::Appearance(Appearance::Dark).into_write(None),
        SettingsWrite::Appearance(Appearance::Dark)
    );
    assert_eq!(
        Change::DesktopNotifications(true).into_write(None),
        SettingsWrite::DesktopNotifications(true)
    );
    assert_eq!(
        Change::WatchMode(WatchMode::OnView).into_write(None),
        SettingsWrite::WatchMode(WatchMode::OnView)
    );
}

// covers UP-127: only an edit that turns protection off loosens it; turning it back on leaves
// nothing to confirm.
#[test]
fn only_turning_protection_off_loosens_it() {
    let protected = Arc::new(Settings {
        protect_local_clusters: true,
        ..Settings::default()
    });
    let mut draft = SettingsDraft::default();
    draft.edit(Change::LogBufferLines(5_000), &protected);
    assert!(!draft.loosens_protection());
    draft.edit(Change::ProtectLocalClusters(false), &protected);
    assert!(draft.loosens_protection());
    assert!(!draft.shown(&protected).protect_local_clusters);
    draft.edit(Change::ProtectLocalClusters(true), &protected);
    assert!(!draft.loosens_protection());
    assert!(!draft.is_edited(Field::ProtectLocalClusters));

    let mut draft = SettingsDraft::default();
    draft.edit(Change::ProtectLocalClusters(true), &file());
    assert!(!draft.loosens_protection(), "turning it on is no loosening");
    let confirmed = flint_core::settings::SettingsRevision::of(&protected);
    assert_eq!(
        Change::ProtectLocalClusters(false).into_write(Some(confirmed.clone())),
        flint_core::settings::SettingsWrite::ProtectLocalClusters {
            on: false,
            confirmed: Some(confirmed.clone()),
        }
    );
    assert_eq!(
        Change::ProtectLocalClusters(true).into_write(Some(confirmed)),
        flint_core::settings::SettingsWrite::ProtectLocalClusters {
            on: true,
            confirmed: None,
        },
        "turning it on needs no confirmation"
    );
}

#[test]
fn forget_drops_one_field_and_the_base_with_the_last() {
    let file = file();
    let mut draft = SettingsDraft::default();
    draft.edit(Change::LogBufferMib(32), &file);
    draft.edit(Change::Helm(Some("/opt/helm".into())), &file);
    draft.forget(Field::LogBufferMib);
    assert!(!draft.is_edited(Field::LogBufferMib));
    assert!(draft.is_edited(Field::Helm));
    draft.forget(Field::Helm);
    assert!(!draft.is_dirty());
    let other = Settings {
        appearance: Appearance::Dark,
        ..Settings::default()
    };
    assert!(!draft.changed_on_disk(&other), "no draft, no banner");
}
