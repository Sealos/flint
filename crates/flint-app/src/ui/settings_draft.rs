//! The Settings view's draft (#739): edits to `settings.toml` that wait for Save.
//!
//! The draft holds only the fields the user changed, each with its new value, over the file as
//! the core last reported it. What the view shows is the file with the edits on top
//! ([`SettingsDraft::shown`]), so without edits it simply follows the file. Save writes the
//! edited fields alone ([`SettingsDraft::edits`]), each through its own setter, so a change made
//! elsewhere to another field survives; they leave the draft only once the core says they are
//! written ([`SettingsDraft::saved`]). The draft also remembers the file it was started over:
//! when the file changes under a pending draft ([`SettingsDraft::changed_on_disk`]), the view
//! says so and offers Reload or Save anyway.
//!
//! A slice that adds a setting adds a [`Field`] and a [`Change`] variant; nothing else here
//! changes.

use std::collections::BTreeMap;
use std::sync::Arc;

use flint_core::settings::SettingsWrite;
use flint_core::{Appearance, Settings, WatchMode};

/// A setting the Settings view edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    Appearance,
    DesktopNotifications,
    WatchMode,
}

impl Field {
    /// The `settings.toml` key it is stored under.
    #[cfg(any(test, feature = "automation"))]
    pub fn key(self) -> &'static str {
        match self {
            Field::Appearance => "appearance",
            Field::DesktopNotifications => "desktop_notifications",
            Field::WatchMode => "watch_mode",
        }
    }
}

/// One edit: a field and the value it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Appearance(Appearance),
    DesktopNotifications(bool),
    /// The global watch mode. A cluster's own override is not this.
    WatchMode(WatchMode),
}

impl Change {
    pub fn field(&self) -> Field {
        match self {
            Change::Appearance(_) => Field::Appearance,
            Change::DesktopNotifications(_) => Field::DesktopNotifications,
            Change::WatchMode(_) => Field::WatchMode,
        }
    }

    /// Whether `settings` already have this value.
    pub fn holds_in(&self, settings: &Settings) -> bool {
        match *self {
            Change::Appearance(a) => settings.appearance == a,
            Change::DesktopNotifications(on) => settings.desktop_notifications == on,
            Change::WatchMode(m) => settings.watch_mode == m,
        }
    }

    /// The core's write for it.
    pub fn to_write(self) -> SettingsWrite {
        match self {
            Change::Appearance(a) => SettingsWrite::Appearance(a),
            Change::DesktopNotifications(on) => SettingsWrite::DesktopNotifications(on),
            Change::WatchMode(m) => SettingsWrite::WatchMode(m),
        }
    }

    fn apply(&self, settings: &mut Settings) {
        match *self {
            Change::Appearance(a) => settings.appearance = a,
            Change::DesktopNotifications(on) => settings.desktop_notifications = on,
            Change::WatchMode(m) => settings.watch_mode = m,
        }
    }
}

/// The edits not saved yet, and the file they were made over.
#[derive(Debug, Clone, Default)]
pub struct SettingsDraft {
    edits: BTreeMap<Field, Change>,
    /// The file as it was when the first edit was made; `None` without edits.
    base: Option<Arc<Settings>>,
}

impl SettingsDraft {
    /// Whether anything waits for Save.
    pub fn is_dirty(&self) -> bool {
        !self.edits.is_empty()
    }

    /// Records `change` over `file`, the settings as the core last reported them. A value the
    /// file already has drops that field's edit, so changing a control back leaves nothing to
    /// save.
    pub fn edit(&mut self, change: Change, file: &Arc<Settings>) {
        if self.edits.is_empty() {
            self.base = Some(file.clone());
        }
        if change.holds_in(file) {
            self.edits.remove(&change.field());
        } else {
            self.edits.insert(change.field(), change);
        }
        if self.edits.is_empty() {
            self.base = None;
        }
    }

    /// Whether `field` has an edit.
    pub fn is_edited(&self, field: Field) -> bool {
        self.edits.contains_key(&field)
    }

    /// The edited fields, in a stable order.
    #[cfg(any(test, feature = "automation"))]
    pub fn edited(&self) -> impl Iterator<Item = Field> + '_ {
        self.edits.keys().copied()
    }

    /// What the view shows: `file` with the edits on top.
    pub fn shown(&self, file: &Settings) -> Settings {
        let mut shown = file.clone();
        for change in self.edits.values() {
            change.apply(&mut shown);
        }
        shown
    }

    /// Whether `file` changed since the draft was started over it, while edits wait: the banner
    /// that offers Reload and Save anyway.
    pub fn changed_on_disk(&self, file: &Settings) -> bool {
        self.base.as_deref().is_some_and(|base| base != file)
    }

    /// Drops every edit (Revert, Reload, Discard).
    pub fn revert(&mut self) {
        self.edits.clear();
        self.base = None;
    }

    /// The edits to write (Save, Save anyway), in a stable order.
    pub fn edits(&self) -> Vec<Change> {
        self.edits.values().copied().collect()
    }

    /// Drops `written`, now in the file, from the draft. An edit changed since is kept.
    pub fn saved(&mut self, written: &[Change]) {
        for change in written {
            if self.edits.get(&change.field()) == Some(change) {
                self.edits.remove(&change.field());
            }
        }
        if self.edits.is_empty() {
            self.base = None;
        }
    }
}

#[cfg(test)]
#[path = "settings_draft_tests.rs"]
mod tests;
