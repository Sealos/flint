//! The Settings view's draft (#739): edits to `settings.toml` that wait for Save.
//!
//! The draft holds only the fields the user changed, each with its new value, over the file as
//! the core last reported it. What the view shows is the file with the edits on top
//! ([`SettingsDraft::shown`]), so without edits it simply follows the file. Save writes the
//! edited fields alone ([`SettingsDraft::edits`]), all in one save, so a change made elsewhere
//! to another field survives; they leave the draft only once the core says they are
//! written ([`SettingsDraft::saved`]). The draft also remembers the file it was started over:
//! when the file changes under a pending draft ([`SettingsDraft::changed_on_disk`]), the view
//! says so and offers Reload or Save anyway.
//!
//! A value typed into a field that its setting does not accept is held too
//! ([`SettingsDraft::reject`]), with why: it shows where it was typed and blocks Save until it
//! is fixed or reverted.
//!
//! An edit is the core's own [`SettingsWrite`] (here [`Change`]), keyed by its
//! [`SettingsField`] (here [`Field`]): a slice that adds a setting extends those in
//! `flint_core::settings`, and nothing here changes.
//!
//! [`SettingsWrite`]: flint_core::settings::SettingsWrite
//! [`SettingsField`]: flint_core::settings::SettingsField

use std::collections::BTreeMap;
use std::sync::Arc;

use flint_core::Settings;
use flint_core::settings::SettingsRevision;
pub use flint_core::settings::{SettingsField as Field, SettingsWrite as Change};

/// Text typed for a field that its setting does not accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub text: String,
    /// Why, as the field's inline error says it.
    pub why: String,
}

/// The edits not saved yet, and the file they were made over.
#[derive(Debug, Clone, Default)]
pub struct SettingsDraft {
    edits: BTreeMap<Field, Change>,
    /// Values typed that their setting does not accept: they block Save.
    rejected: BTreeMap<Field, Rejected>,
    /// The file as it was when the first edit was made; `None` without edits.
    base: Option<Arc<Settings>>,
}

impl SettingsDraft {
    /// Whether anything waits for Save, a rejected value included.
    pub fn is_dirty(&self) -> bool {
        !self.edits.is_empty() || !self.rejected.is_empty()
    }

    /// Whether the edits can be saved: there are some, and no value typed is rejected.
    pub fn is_ready(&self) -> bool {
        !self.edits.is_empty() && self.rejected.is_empty()
    }

    /// Records `change` over `file`, the settings as the core last reported them. A value the
    /// file already has drops that field's edit, so changing a control back leaves nothing to
    /// save. It replaces a value rejected for the field.
    pub fn edit(&mut self, change: Change, file: &Arc<Settings>) {
        self.edit_over(change, Some(file), file);
    }

    /// [`Self::edit`], where the value the file holds is not yet known from `file`: `holding` is
    /// what it holds once writes the core acknowledged land, or `None` when that is unknown and
    /// the edit is kept whatever its value.
    pub fn edit_over(&mut self, change: Change, holding: Option<&Settings>, file: &Arc<Settings>) {
        self.start(file);
        let field = change.field();
        self.rejected.remove(&field);
        if holding.is_some_and(|held| change.holds_in(held)) {
            self.edits.remove(&field);
        } else {
            self.edits.insert(field, change);
        }
        self.settle();
    }

    /// Records `text`, typed for `field`, as a value its setting does not accept, and why. The
    /// field's edit, if any, is dropped: the view shows the file's value under the text.
    pub fn reject(&mut self, field: Field, text: String, why: String, file: &Arc<Settings>) {
        self.start(file);
        self.edits.remove(&field);
        self.rejected.insert(field, Rejected { text, why });
    }

    /// The value rejected for `field`, if any.
    pub fn rejected(&self, field: &Field) -> Option<&Rejected> {
        self.rejected.get(field)
    }

    /// Whether a value typed is rejected, which blocks Save.
    pub fn has_rejected(&self) -> bool {
        !self.rejected.is_empty()
    }

    /// Keeps only the rejected values whose field `keep` accepts: one for an override that no
    /// longer applies to any cluster has nowhere to be fixed.
    pub fn retain_rejected(&mut self, mut keep: impl FnMut(&Field) -> bool) {
        self.rejected.retain(|field, _| keep(field));
        self.settle();
    }

    /// Drops the edit and the rejected value of every field `forget` names.
    pub fn forget(&mut self, mut forget: impl FnMut(&Field) -> bool) {
        self.edits.retain(|field, _| !forget(field));
        self.rejected.retain(|field, _| !forget(field));
        self.settle();
    }

    /// Every rejected value, in a stable order.
    #[cfg(any(test, feature = "automation"))]
    pub fn all_rejected(&self) -> impl Iterator<Item = (&Field, &Rejected)> + '_ {
        self.rejected.iter()
    }

    /// Whether `field` has an edit or a rejected value.
    pub fn is_edited(&self, field: &Field) -> bool {
        self.edits.contains_key(field) || self.rejected.contains_key(field)
    }

    /// Every field with an edit or a rejected value.
    pub fn touched(&self) -> impl Iterator<Item = &Field> + '_ {
        self.edits.keys().chain(self.rejected.keys())
    }

    /// The edited fields, in a stable order. Rejected values are not among them.
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
    /// The proxy does not count: it is applied from the Network section on its own, never
    /// through the draft, and Save leaves it as it is.
    pub fn changed_on_disk(&self, file: &Settings) -> bool {
        self.base.as_deref().is_some_and(|base| {
            // An edit only to keys Flint ignores, or to comments, changes the file too. A proxy
            // applied from the Network section rewrites it as well, so it is only compared while
            // the proxy is unchanged.
            if base.proxy == file.proxy {
                return base != file || !base.file_stamp.same(&file.file_stamp);
            }
            let mut base = base.clone();
            base.proxy = file.proxy.clone();
            base != *file
        })
    }

    /// Drops every edit and rejected value (Revert, Reload, Discard).
    pub fn revert(&mut self) {
        self.edits.clear();
        self.rejected.clear();
        self.base = None;
    }

    /// The edits to write (Save, Save anyway), in a stable order.
    pub fn edits(&self) -> Vec<Change> {
        self.edits.values().cloned().collect()
    }

    /// Whether saving would turn local cluster protection off: Save asks first, whatever the
    /// file says now, so an edit made over another file never loosens it unasked.
    pub fn loosens_protection(&self) -> bool {
        matches!(
            self.edits.get(&Field::ProtectLocalClusters),
            Some(Change::ProtectLocalClusters { on: false, .. })
        )
    }

    /// `edits` as the core is to write them: turning protection off carries `confirmed`, the
    /// settings it was confirmed over, without which the core refuses it.
    pub fn confirmed(edits: &[Change], confirmed: Option<&SettingsRevision>) -> Vec<Change> {
        edits
            .iter()
            .map(|change| match change {
                Change::ProtectLocalClusters { on: false, .. } => Change::ProtectLocalClusters {
                    on: false,
                    confirmed: confirmed.cloned(),
                },
                other => other.clone(),
            })
            .collect()
    }

    /// Drops `written`, now in the file, from the draft. An edit changed since is kept.
    pub fn saved(&mut self, written: &[Change]) {
        for change in written {
            if self.edits.get(&change.field()) == Some(change) {
                self.edits.remove(&change.field());
            }
        }
        self.settle();
    }

    /// Starts a draft over `file`, if none is started: an edit that follows is over it, not over
    /// the file as it is by then, so a change in between shows as made on disk.
    pub fn begin(&mut self, file: &Arc<Settings>) {
        self.start(file);
    }

    /// Remembers the file a draft starts over.
    fn start(&mut self, file: &Arc<Settings>) {
        if self.base.is_none() {
            self.base = Some(file.clone());
        }
    }

    /// Forgets the file once nothing waits.
    fn settle(&mut self) {
        if !self.is_dirty() {
            self.base = None;
        }
    }
}

#[cfg(test)]
#[path = "settings_draft_tests.rs"]
mod tests;
