//! Editing an object's YAML in the detail pane (UP-51, UP-52): edit, Review (a server dry run
//! and its diff), Apply through the shared confirmation, and a reload when the object changed.
//!
//! Every check that matters is in `flint_core::write`. This keeps the editor's text and which
//! step it is at. A Secret is not editable: its values are masked here, and a mask must never
//! be written back.

use std::sync::Arc;

use flint_core::ObjectDetail;
use flint_core::write::WriteTarget;
use flint_core::write::{WriteAction, WriteError, WriteOutcome, WriteProposal, WriteReview};
use gpui_kit::component::diff::{DiffFile, DiffState};
use gpui_kit::component::input::EditorState;
use gpui_kit::*;
use serde_json::Value;

use super::{DetailEvent, DetailTab, DetailView, Fetch, protected_reason};

pub(super) enum Phase {
    Editing,
    /// A dry run, or the reload before one, is running.
    Reviewing,
    Reviewed(Box<WriteReview>),
    /// Confirmed: the write is running.
    Applying,
    /// The object changed since it was opened: nothing was written.
    Conflict,
}

impl Phase {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Phase::Editing => "editing",
            Phase::Reviewing => "reviewing",
            Phase::Reviewed(_) => "reviewed",
            Phase::Applying => "applying",
            Phase::Conflict => "conflict",
        }
    }
}

pub(super) struct YamlEdit {
    pub editor: Entity<EditorState>,
    pub diff: Entity<DiffState>,
    target: WriteTarget,
    pub phase: Phase,
    /// Why the last step failed, in the server's or the core's words.
    pub error: Option<String>,
    _task: Task<()>,
}

/// The object as the editor opens it: no `status` or `managedFields`, `resourceVersion` kept.
pub(super) fn editable_yaml(object: &Value) -> Result<String, String> {
    let mut shown = object.clone();
    if let Some(fields) = shown.as_object_mut() {
        fields.remove("status");
    }
    if let Some(metadata) = shown.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("managedFields");
    }
    serde_saphyr::to_string(&shown).map_err(|e| format!("cannot render YAML: {e}"))
}

/// `yaml` with `metadata.resourceVersion` set to `version`, every other line as typed.
pub(super) fn with_resource_version(yaml: &str, version: &str) -> String {
    let mut in_metadata = false;
    let mut out = String::with_capacity(yaml.len() + version.len());
    for line in yaml.split_inclusive('\n') {
        if !line.starts_with([' ', '\t', '#', '\n', '\r']) {
            in_metadata = line.trim_end().trim_end_matches(' ') == "metadata:";
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        if in_metadata && indent > 0 && line[indent..].starts_with("resourceVersion:") {
            let end = if line.ends_with('\n') { "\n" } else { "" };
            out.push_str(&format!(
                "{}resourceVersion: \"{version}\"{end}",
                &line[..indent]
            ));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The review's change as a file for the diff view, or `None` if it changes nothing.
fn diff_file(name: &str, before: &str, after: &str) -> Option<DiffFile> {
    if before == after {
        return None;
    }
    let patch = similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{name}.yaml"), &format!("b/{name}.yaml"))
        .to_string();
    let file = DiffFile::parse(&patch).ok()?.into_iter().next()?;
    Some(file.with_language("yaml"))
}

fn texts(review: &WriteReview) -> Option<(&str, &str)> {
    match review.action() {
        WriteAction::ApplyYaml(edit) => Some((edit.before(), edit.after())),
        _ => None,
    }
}

impl DetailView {
    /// Why this object cannot be edited now, as the disabled button says it.
    pub(in crate::ui) fn edit_blocked(&self) -> Option<String> {
        if self.is_secret {
            return Some("Secret values are hidden, so a Secret cannot be edited here".to_owned());
        }
        if self.gone {
            return Some("This object was deleted".to_owned());
        }
        if let Some(why) = &self.sessions_blocked {
            return Some(why.clone());
        }
        if self.protected {
            return Some(protected_reason(&self.object.cluster));
        }
        (!matches!(self.fetch, Fetch::Loaded { .. })).then(|| "The object is not loaded".to_owned())
    }

    pub(in crate::ui) fn can_start_edit(&self) -> bool {
        self.edit.is_none() && self.edit_blocked().is_none()
    }

    pub(in crate::ui) fn can_review_edit(&self) -> bool {
        self.edit_blocked().is_none()
            && matches!(self.edit.as_ref().map(|e| &e.phase), Some(Phase::Editing))
    }

    pub(in crate::ui) fn can_apply_edit(&self) -> bool {
        self.edit_blocked().is_none()
            && matches!(
                self.edit.as_ref().map(|e| &e.phase),
                Some(Phase::Reviewed(_))
            )
    }

    pub(in crate::ui) fn can_reload_edit(&self) -> bool {
        self.edit_blocked().is_none()
            && matches!(self.edit.as_ref().map(|e| &e.phase), Some(Phase::Conflict))
    }

    pub(in crate::ui) fn can_go_back_to_edit(&self) -> bool {
        matches!(
            self.edit.as_ref().map(|e| &e.phase),
            Some(Phase::Reviewed(_))
        )
    }

    pub(in crate::ui) fn can_cancel_edit(&self) -> bool {
        self.edit
            .as_ref()
            .is_some_and(|e| !matches!(e.phase, Phase::Applying))
    }

    pub(super) fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_start_edit() {
            return;
        }
        let Fetch::Loaded { detail, .. } = &self.fetch else {
            return;
        };
        let target = self.edit_target(detail);
        let text = match editable_yaml(&detail.object) {
            Ok(text) => text,
            Err(why) => {
                cx.emit(DetailEvent::Refused(format!(
                    "Can't edit this object: {why}."
                )));
                return;
            }
        };
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("yaml")
                .line_number(true)
                .soft_wrap(false);
            state.set_value(text, window, cx);
            state
        });
        editor.read(cx).focus_handle(cx).focus(window, cx);
        let diff = cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx));
        self.tab = DetailTab::Yaml;
        self.edit = Some(YamlEdit {
            editor,
            diff,
            target,
            phase: Phase::Editing,
            error: None,
            _task: Task::ready(()),
        });
        cx.notify();
    }

    fn edit_target(&self, detail: &ObjectDetail) -> WriteTarget {
        let text = |pointer: &str| {
            detail
                .object
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        WriteTarget {
            cluster: self.object.cluster.clone(),
            server: self.server.clone(),
            kind: self.object.kind.clone(),
            namespace: self.object.namespace.clone(),
            name: self.object.name.clone(),
            uid: text("/metadata/uid"),
            resource_version: text("/metadata/resourceVersion"),
        }
    }

    pub(super) fn review_edit(&mut self, cx: &mut Context<Self>) {
        if !self.can_review_edit() {
            return;
        }
        let core = self.core.clone();
        let Some(edit) = &mut self.edit else {
            return;
        };
        let yaml = edit.editor.read(cx).value().to_string();
        edit.phase = Phase::Reviewing;
        edit.error = None;
        let run = core.review_write(edit.target.clone(), WriteProposal::ApplyYaml { yaml });
        edit._task = cx.spawn(async move |this, cx| {
            let result = run.await;
            let _ = this.update(cx, |view, cx| view.edit_reviewed(result, cx));
        });
        cx.notify();
    }

    fn edit_reviewed(&mut self, result: Result<WriteReview, WriteError>, cx: &mut Context<Self>) {
        let name = self.object.name.clone();
        let Some(edit) = &mut self.edit else {
            return;
        };
        match result {
            Ok(review) => {
                edit.target = review.target().clone();
                let file = texts(&review).and_then(|(a, b)| diff_file(&name, a, b));
                match file {
                    Some(file) => {
                        edit.diff.update(cx, |diff, cx| diff.set_files([file], cx));
                        edit.phase = Phase::Reviewed(Box::new(review));
                    }
                    None => {
                        edit.phase = Phase::Editing;
                        edit.error = Some("No changes to apply.".to_owned());
                    }
                }
            }
            Err(error) => Self::edit_failed(edit, error),
        }
        cx.notify();
    }

    fn edit_failed(edit: &mut YamlEdit, error: WriteError) {
        if error == WriteError::Conflict {
            edit.phase = Phase::Conflict;
            edit.error = None;
        } else {
            edit.phase = Phase::Editing;
            edit.error = Some(error.to_string());
        }
    }

    /// Asks the window to confirm the reviewed change.
    pub(super) fn apply_edit(&mut self, cx: &mut Context<Self>) {
        if !self.can_apply_edit() {
            return;
        }
        if let Some(YamlEdit {
            phase: Phase::Reviewed(review),
            ..
        }) = &self.edit
        {
            let review = review.clone();
            cx.emit(DetailEvent::ConfirmWrite(review));
        }
    }

    /// The confirmation was accepted: the write is running.
    pub(in crate::ui) fn edit_applying(&mut self, cx: &mut Context<Self>) {
        if let Some(edit) = &mut self.edit {
            edit.phase = Phase::Applying;
            edit.error = None;
            cx.notify();
        }
    }

    pub(in crate::ui) fn edit_applied(
        &mut self,
        result: Result<WriteOutcome, WriteError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(_) => {
                self.edit = None;
                self.refresh(window, cx);
            }
            Err(error) => {
                if let Some(edit) = &mut self.edit {
                    Self::edit_failed(edit, error);
                }
            }
        }
        cx.notify();
    }

    /// Fetches the object as it is now, keeps the edit, and reviews it against that.
    pub(super) fn reload_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_reload_edit() {
            return;
        }
        let o = &self.object;
        let fetch = self
            .core
            .fetch_object(&o.cluster, &o.kind, o.namespace.as_deref(), &o.name);
        let Some(edit) = &mut self.edit else {
            return;
        };
        edit.phase = Phase::Reviewing;
        edit.error = None;
        edit._task = cx.spawn_in(window, async move |this, cx| {
            let result = fetch.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.edit_reloaded(result, window, cx)
            });
        });
        cx.notify();
    }

    fn edit_reloaded(
        &mut self,
        result: Result<Arc<ObjectDetail>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = &mut self.edit else {
            return;
        };
        let detail = match result {
            Ok(detail) => detail,
            Err(why) => {
                edit.phase = Phase::Conflict;
                edit.error = Some(format!("Could not reload the object: {why}"));
                cx.notify();
                return;
            }
        };
        let text = |pointer: &str| detail.object.pointer(pointer).and_then(Value::as_str);
        if text("/metadata/uid") != edit.target.uid.as_deref() {
            edit.phase = Phase::Editing;
            edit.error = Some(WriteError::Replaced.to_string());
            cx.notify();
            return;
        }
        if let Some(version) = text("/metadata/resourceVersion") {
            let yaml = with_resource_version(&edit.editor.read(cx).value(), version);
            edit.editor
                .update(cx, |state, cx| state.set_value(yaml, window, cx));
            edit.target.resource_version = Some(version.to_owned());
        }
        edit.phase = Phase::Editing;
        self.review_edit(cx);
    }

    pub(super) fn back_to_edit(&mut self, cx: &mut Context<Self>) {
        if self.can_go_back_to_edit()
            && let Some(edit) = &mut self.edit
        {
            edit.phase = Phase::Editing;
            cx.notify();
        }
    }

    pub(super) fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.can_cancel_edit() {
            self.edit = None;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{diff_file, editable_yaml, with_resource_version};

    // covers UP-51: the editor opens without status or managedFields, at its version.
    #[test]
    fn the_editor_opens_without_status_and_managed_fields() {
        let text = editable_yaml(&json!({
            "kind": "ConfigMap",
            "metadata": {"name": "a", "resourceVersion": "7", "managedFields": [{"manager": "x"}]},
            "data": {"k": "v"},
            "status": {"phase": "x"},
        }))
        .unwrap();
        assert!(text.contains("resourceVersion"), "{text}");
        assert!(text.contains("k: v"), "{text}");
        assert!(!text.contains("managedFields"), "{text}");
        assert!(!text.contains("status"), "{text}");
    }

    // covers UP-52: a reload moves only metadata's version and keeps what was typed.
    #[test]
    fn a_reload_changes_only_the_objects_version() {
        let yaml = "kind: X\nmetadata:\n  name: a\n  resourceVersion: \"7\"\nspec:\n  \
                    resourceVersion: keep\n  replicas: 3\n";
        let moved = with_resource_version(yaml, "8");
        assert_eq!(
            moved,
            "kind: X\nmetadata:\n  name: a\n  resourceVersion: \"8\"\nspec:\n  resourceVersion: \
             keep\n  replicas: 3\n"
        );
    }

    #[test]
    fn a_review_without_a_change_has_no_diff() {
        assert!(diff_file("a", "x: 1\n", "x: 1\n").is_none());
        assert!(diff_file("a", "x: 1\n", "x: 2\n").is_some());
    }
}
