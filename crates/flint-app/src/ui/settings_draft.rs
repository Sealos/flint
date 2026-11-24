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
//! An edit is the core's own [`SettingsWrite`] (here [`Change`]), keyed by its
//! [`SettingsField`] (here [`Field`]): a slice that adds a setting extends those in
//! `flint_core::settings`, and nothing here changes.

use std::collections::BTreeMap;
use std::sync::Arc;

use flint_core::Settings;
pub use flint_core::settings::{SettingsField as Field, SettingsWrite as Change};

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
    pub fn is_edited(&self, field: &Field) -> bool {
        self.edits.contains_key(field)
    }

    /// The edited fields, in a stable order.
    #[cfg(any(test, feature = "automation"))]
    pub fn edited(&self) -> impl Iterator<Item = &Field> + '_ {
        self.edits.keys()
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
        self.edits.values().cloned().collect()
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
