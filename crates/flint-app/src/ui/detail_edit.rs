//! Editing an object's YAML in the detail pane (UP-51, UP-52): edit, Review (a server dry run
//! and its diff), Apply through the shared confirmation, and a reload when the object changed.
//!
//! Every check that matters is in `flint_core::write`. This keeps the editor's text and which
//! step it is at. A Secret is not editable: its values are masked here, and a mask must never
//! be written back.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use flint_core::write::WriteTarget;
use flint_core::write::{WriteAction, WriteError, WriteOutcome, WriteProposal, WriteReview};
use flint_core::{ObjectDetail, WriteRefusal};
use gpui_kit::component::diff::{DiffFile, DiffState};
use gpui_kit::component::input::EditorState;
use gpui_kit::*;
use serde_json::Value;

use super::access::Gated;
use super::{DetailEvent, DetailTab, DetailView, Fetch, protected_reason};
use crate::ui::write_confirm;

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

#[cfg(any(test, feature = "automation"))]
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
    /// Each distinct state this edit has seen, without server-owned fields, from the one it
    /// opened on. A value that left and came back is still someone's change.
    seen: Vec<Value>,
    /// The object at the last reload.
    latest: Option<Arc<ObjectDetail>>,
    /// Paths the last review would overwrite after they changed on the cluster.
    competing: Vec<String>,
    _task: Task<()>,
}

impl YamlEdit {
    /// The warning the review step shows, naming paths and never a value.
    pub(super) fn competing_warning(&self) -> Option<String> {
        if matches!(self.phase, Phase::Reviewed(_)) {
            competing_text(&self.competing)
        } else {
            None
        }
    }
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

/// `yaml` with `metadata.resourceVersion` set to `version`, every other line as typed. A form the
/// line edit cannot reach (flow style, a quoted key) is rendered again from its parsed value.
pub(super) fn with_resource_version(yaml: &str, version: &str) -> String {
    let moved = replace_version_line(yaml, version);
    let Ok(mut parsed) = serde_saphyr::from_str::<Value>(&moved) else {
        // Not valid YAML: the review says so.
        return moved;
    };
    let at = parsed.pointer("/metadata/resourceVersion");
    if at.and_then(Value::as_str) == Some(version) {
        return moved;
    }
    let Some(metadata) = parsed.get_mut("metadata").and_then(Value::as_object_mut) else {
        return moved;
    };
    metadata.insert("resourceVersion".to_owned(), version.into());
    serde_saphyr::to_string(&parsed).unwrap_or(moved)
}

/// Rewrites the `resourceVersion:` line among `metadata`'s direct children, not a nested key.
fn replace_version_line(yaml: &str, version: &str) -> String {
    let mut in_metadata = false;
    let mut child_indent = None;
    let mut out = String::with_capacity(yaml.len() + version.len());
    for line in yaml.split_inclusive('\n') {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let body = line[indent..].trim_end();
        if body.is_empty() || body.starts_with('#') {
            out.push_str(line);
            continue;
        }
        if indent == 0 {
            in_metadata = body == "metadata:";
            child_indent = None;
        } else if in_metadata && child_indent.is_none() {
            child_indent = Some(indent);
        }
        if in_metadata && child_indent == Some(indent) && body.starts_with("resourceVersion:") {
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

/// What the server sets in `metadata`: a difference here is not someone's edit.
const SERVER_OWNED_METADATA: [&str; 8] = [
    "creationTimestamp",
    "deletionGracePeriodSeconds",
    "deletionTimestamp",
    "generation",
    "managedFields",
    "resourceVersion",
    "selfLink",
    "uid",
];

/// One step of a path into an object. Kept structured so that two different paths stay two,
/// whatever their keys contain.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Step<'a> {
    Key(&'a str),
    /// A list item, by its `name`.
    Item(&'a str),
}

fn without_server_owned(object: &Value) -> Value {
    let mut kept = object.clone();
    if let Some(fields) = kept.as_object_mut() {
        fields.remove("status");
    }
    if let Some(metadata) = kept.get_mut("metadata").and_then(Value::as_object_mut) {
        for field in SERVER_OWNED_METADATA {
            metadata.remove(field);
        }
    }
    kept
}

/// The paths that differ between any state in `seen` and `latest` and that `yaml` would set to
/// something other than `latest`, sorted. Text that does not parse has none: its review fails.
fn competing_changes(seen: &[Value], latest: &Value, yaml: &str) -> Vec<String> {
    let Ok(edited) = serde_saphyr::from_str::<Value>(yaml) else {
        return Vec::new();
    };
    let latest = without_server_owned(latest);
    let edited = without_server_owned(&edited);
    let mut found = BTreeSet::new();
    for earlier in seen {
        collect_competing(
            Some(earlier),
            Some(&latest),
            Some(&edited),
            &mut Vec::new(),
            &mut found,
        );
    }
    found.iter().map(|path| path_text(path)).collect()
}

fn collect_competing<'a>(
    base: Option<&'a Value>,
    latest: Option<&'a Value>,
    edited: Option<&'a Value>,
    path: &mut Vec<Step<'a>>,
    found: &mut BTreeSet<Vec<Step<'a>>>,
) {
    if base == latest || latest == edited {
        return;
    }
    match (base, latest, edited) {
        (Some(Value::Object(b)), Some(Value::Object(l)), None | Some(Value::Object(_))) => {
            let e = edited.and_then(Value::as_object);
            for key in b.keys().chain(l.keys()).collect::<BTreeSet<_>>() {
                path.push(Step::Key(key));
                collect_competing(
                    b.get(key),
                    l.get(key),
                    e.and_then(|e| e.get(key)),
                    path,
                    found,
                );
                path.pop();
            }
        }
        (Some(Value::Array(b)), Some(Value::Array(l)), Some(Value::Array(e))) => {
            let (Some(nb), Some(nl), Some(ne)) = (by_name(b), by_name(l), by_name(e)) else {
                found.insert(path.clone());
                return;
            };
            for name in nb.keys().chain(nl.keys()).copied().collect::<BTreeSet<_>>() {
                path.push(Step::Item(name));
                collect_competing(
                    nb.get(name).copied(),
                    nl.get(name).copied(),
                    ne.get(name).copied(),
                    path,
                    found,
                );
                path.pop();
            }
            if undoes_remote_order(b, l, e) {
                found.insert(path.clone());
            }
        }
        _ => {
            found.insert(path.clone());
        }
    }
}

/// Whether `edited` puts two of `latest`'s items in the other order when that order came from the
/// cluster: they were swapped there, or one of them is new there and so was placed there.
/// Every item must have a unique name.
fn undoes_remote_order(base: &[Value], latest: &[Value], edited: &[Value]) -> bool {
    let positions = |items: &[Value]| -> BTreeMap<String, usize> {
        items
            .iter()
            .enumerate()
            .filter_map(|(at, item)| Some((item_name(item)?.to_owned(), at)))
            .collect()
    };
    let (in_base, in_edited) = (positions(base), positions(edited));
    // In `latest`'s order: where the text has each item, and where it was in `base`.
    let kept: Vec<(usize, Option<usize>)> = latest
        .iter()
        .filter_map(item_name)
        .filter_map(|name| Some((*in_edited.get(name)?, in_base.get(name).copied())))
        .collect();
    kept.iter().enumerate().any(|(at, (edited_x, base_x))| {
        kept[at + 1..].iter().any(|(edited_y, base_y)| {
            edited_x > edited_y
                && match (base_x, base_y) {
                    (Some(x), Some(y)) => x > y,
                    _ => true,
                }
        })
    })
}

fn item_name(item: &Value) -> Option<&str> {
    item.get("name").and_then(Value::as_str)
}

/// The list's items by `name`, if every item has its own: how Kubernetes lists containers, env
/// and volumes. Any other list is compared whole.
fn by_name(items: &[Value]) -> Option<BTreeMap<&str, &Value>> {
    let mut named = BTreeMap::new();
    for item in items {
        if named.insert(item_name(item)?, item).is_some() {
            return None;
        }
    }
    Some(named)
}

/// `path` on one line. Anything but a plain key or name is quoted and escaped, so a key cannot
/// read as a nested path or break the line.
fn path_text(path: &[Step]) -> String {
    let plain = |text: &str, also: &[char]| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || also.contains(&c))
    };
    let mut out = String::new();
    for step in path {
        match step {
            Step::Key(key) if plain(key, &[]) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Step::Key(key) => out.push_str(&format!("[\"{}\"]", key.escape_debug())),
            Step::Item(name) if plain(name, &['.']) => out.push_str(&format!("[{name}]")),
            Step::Item(name) => out.push_str(&format!("[name=\"{}\"]", name.escape_debug())),
        }
    }
    out
}

/// The warning for `paths`, or `None` when nothing would be overwritten: the count, then every
/// path on its own line. Where it is shown, it scrolls.
fn competing_text(paths: &[String]) -> Option<String> {
    let (count, it) = match paths.len() {
        0 => return None,
        1 => ("1 field".to_owned(), "it"),
        n => (format!("{n} fields"), "them"),
    };
    Some(format!(
        "{count} changed on the cluster since you opened this edit, and Apply would overwrite \
         {it}:\n{}",
        paths.join("\n")
    ))
}

/// Whether the text sets `status`, which a review cannot show: the editor opens without it.
fn sets_status(yaml: &str) -> bool {
    serde_saphyr::from_str::<Value>(yaml).is_ok_and(|value| value.get("status").is_some())
}

/// The longest a review's diff may take: past it, the diff is coarser but still correct.
const DIFF_DEADLINE: Duration = Duration::from_secs(1);

/// The change as a unified patch, or `None` if it changes nothing. Off the UI thread: a large
/// value replaced can take long.
fn patch(name: &str, before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let diff = similar::TextDiff::configure()
        .timeout(DIFF_DEADLINE)
        .diff_lines(before, after);
    let patch = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{name}.yaml"), &format!("b/{name}.yaml"))
        .to_string();
    Some(patch)
}

fn diff_file(patch: &str) -> Option<DiffFile> {
    let file = DiffFile::parse(patch).ok()?.into_iter().next()?;
    Some(file.with_language("yaml"))
}

fn texts(review: &WriteReview) -> Option<(&str, &str)> {
    match review.action() {
        WriteAction::ApplyYaml(edit) => Some((edit.before(), edit.after())),
        _ => None,
    }
}

/// A failed step as the editor says it. The core's wording for a context that moved is about a
/// shell tab; here the way forward is to open the object again, which starts a new edit, so the
/// text says to copy this one first. Only the wording differs: the refusal stands.
fn edit_error_text(error: &WriteError) -> String {
    const AGAIN: &str = "copy your edit if you want to keep it, then open the object again";
    match error {
        WriteError::Refused(WriteRefusal::Moved { cluster }) => {
            // One text for both causes: the context points at another server now, or a kubeconfig
            // file could not be read, so the server it points at cannot be confirmed.
            format!(
                "\"{cluster}\" points at another server than this object was opened from, or \
                 its kubeconfig could not be read to confirm it; {AGAIN}. If a kubeconfig file \
                 is unreadable, fix it before you open the object again"
            )
        }
        WriteError::Refused(WriteRefusal::Stale { cluster }) => {
            format!(
                "\"{cluster}\" changed in your kubeconfig files since this object was opened; \
                 {AGAIN}"
            )
        }
        other => other.to_string(),
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
            return Some(protected_reason(&self.cluster_label));
        }
        // An edit under way keeps its text and its buttons: the server answers its review.
        if self.edit.is_none()
            && let Some(why) = self.access.forbidden(Gated::Edit)
        {
            return Some(why);
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
        let opened = without_server_owned(&detail.object);
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
            seen: vec![opened],
            latest: None,
            competing: Vec::new(),
            _task: Task::ready(()),
        });
        cx.notify();
    }

    /// The handle this view's writes are reviewed through: bound to its member for a set's row.
    pub(in crate::ui) fn write_core(&self) -> &flint_core::CoreHandle {
        &self.core
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
        if sets_status(&yaml) {
            edit.error = Some("Remove status: it cannot be edited here.".to_owned());
            cx.notify();
            return;
        }
        edit.phase = Phase::Reviewing;
        edit.error = None;
        edit.competing.clear();
        let compare = edit
            .latest
            .clone()
            .map(|latest| (edit.seen.clone(), latest, yaml.clone()));
        let run = core.review_write(edit.target.clone(), WriteProposal::ApplyYaml { yaml });
        let name = self.object.name.clone();
        edit._task = cx.spawn(async move |this, cx| {
            let result = run.await;
            let shown = result
                .as_ref()
                .ok()
                .and_then(texts)
                .map(|(before, after)| (before.to_owned(), after.to_owned()));
            let compare = compare.filter(|_| result.is_ok());
            let (patch, competing) = cx
                .background_spawn(async move {
                    let competing = compare.map_or_else(Vec::new, |(seen, latest, yaml)| {
                        competing_changes(&seen, &latest.object, &yaml)
                    });
                    let patch = shown.and_then(|(before, after)| patch(&name, &before, &after));
                    (patch, competing)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.edit_reviewed(result, patch, competing, cx)
            });
        });
        cx.notify();
    }

    fn edit_reviewed(
        &mut self,
        result: Result<WriteReview, WriteError>,
        patch: Option<String>,
        competing: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = &mut self.edit else {
            return;
        };
        match result {
            Ok(review) => {
                edit.target = review.target().clone();
                match patch.as_deref().and_then(diff_file) {
                    Some(file) => {
                        edit.diff.update(cx, |diff, cx| diff.set_files([file], cx));
                        edit.competing = competing;
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
            edit.error = Some(edit_error_text(&error));
        }
    }

    /// Asks the window to confirm the reviewed change.
    pub(super) fn apply_edit(&mut self, cx: &mut Context<Self>) {
        if !self.can_apply_edit() {
            return;
        }
        if let Some(edit) = &self.edit
            && let Phase::Reviewed(review) = &edit.phase
        {
            let review = review.clone();
            write_confirm::set_edit_note(review.target(), edit.competing_warning(), cx);
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
        let state = without_server_owned(&detail.object);
        if !edit.seen.contains(&state) {
            edit.seen.push(state);
        }
        edit.latest = Some(detail.clone());
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
    use flint_core::write::WriteError;
    use flint_core::{KindId, ObjectDetail, SyncState, WriteRefusal};
    use gpui_kit::test::TestAppContextExt as _;
    use gpui_kit::{AppContext as _, TestAppContext};
    use serde_json::json;

    use super::{
        Fetch, Phase, Step, competing_text, edit_error_text, editable_yaml, patch, path_text,
        sets_status, with_resource_version, without_server_owned,
    };
    use crate::ui::support_tests::{WAIT, objects, open_workspace, standard_sections};

    fn competing_changes(
        base: &serde_json::Value,
        latest: &serde_json::Value,
        yaml: &str,
    ) -> Vec<String> {
        super::competing_changes(&[without_server_owned(base)], latest, yaml)
    }

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

    fn yaml(value: &serde_json::Value) -> String {
        serde_saphyr::to_string(value).unwrap()
    }

    // A stale value kept by a reload is named, and so is a new value over a remote change.
    #[test]
    fn a_competing_change_is_listed() {
        let base = json!({"metadata": {"name": "a"}, "spec": {"replicas": 1, "paused": false}});
        let latest = json!({"metadata": {"name": "a"}, "spec": {"replicas": 5, "paused": false}});
        assert_eq!(
            competing_changes(&base, &latest, &yaml(&base)),
            ["spec.replicas"]
        );
        let deliberate = yaml(&json!({
            "metadata": {"name": "a"}, "spec": {"replicas": 9, "paused": false}
        }));
        assert_eq!(
            competing_changes(&base, &latest, &deliberate),
            ["spec.replicas"]
        );
    }

    // A key added remotely is dropped by the stale text, one removed remotely comes back.
    #[test]
    fn a_remote_addition_or_removal_is_listed() {
        let base = json!({"data": {"gone": "1", "same": "2"}});
        let latest = json!({"data": {"new": "3", "same": "2"}});
        assert_eq!(
            competing_changes(&base, &latest, &yaml(&base)),
            ["data.gone", "data.new"]
        );
    }

    // A named list is compared per item: an edit to one container does not flag another.
    #[test]
    fn named_list_items_are_compared_by_name() {
        let pod = |a: &str, b: &str| {
            json!({"spec": {"containers": [
                {"name": "a", "image": a}, {"name": "b", "image": b},
            ]}})
        };
        let (base, latest) = (pod("1", "1"), pod("2", "1"));
        assert_eq!(
            competing_changes(&base, &latest, &yaml(&pod("1", "9"))),
            ["spec.containers[a].image"]
        );
        assert!(competing_changes(&base, &latest, &yaml(&pod("2", "9"))).is_empty());
    }

    // No warning when the text already has the remote value, or the change is the server's.
    #[test]
    fn a_kept_or_ignored_change_is_not_listed() {
        let base = json!({
            "metadata": {"name": "a", "resourceVersion": "1", "generation": 1,
                         "managedFields": [{"manager": "x"}]},
            "spec": {"replicas": 1, "paused": false},
            "status": {"ready": 1},
        });
        let latest = json!({
            "metadata": {"name": "a", "resourceVersion": "2", "generation": 2,
                         "managedFields": [{"manager": "y"}]},
            "spec": {"replicas": 5, "paused": false},
            "status": {"ready": 5},
        });
        let kept = yaml(&json!({
            "metadata": {"name": "a", "resourceVersion": "2"},
            "spec": {"replicas": 5, "paused": true},
        }));
        assert!(competing_changes(&base, &latest, &kept).is_empty());
        let only_server = json!({
            "metadata": {"name": "a", "resourceVersion": "2", "generation": 2},
            "spec": {"replicas": 1, "paused": false},
            "status": {"ready": 5},
        });
        assert!(competing_changes(&base, &only_server, &yaml(&base)).is_empty());
    }

    #[test]
    fn a_warning_names_paths_and_never_a_value() {
        let base = json!({"kind": "Secret", "data": {"password": "b2xkLXNlY3JldA=="}});
        let latest = json!({"kind": "Secret", "data": {"password": "bmV3LXNlY3JldA=="}});
        let paths = competing_changes(&base, &latest, &yaml(&base));
        let text = competing_text(&paths).unwrap();
        assert!(text.contains("data.password"), "{text}");
        assert!(!text.contains("b2xkLXNlY3JldA"), "{text}");
        assert!(!text.contains("bmV3LXNlY3JldA"), "{text}");
    }

    // Apply is never available with a path left out of the warning.
    #[test]
    fn a_warning_counts_and_names_every_path() {
        assert_eq!(competing_text(&[]), None);
        let paths: Vec<String> = (0..40).map(|n| format!("p{n}")).collect();
        let text = competing_text(&paths).unwrap();
        assert!(text.starts_with("40 fields changed"), "{text}");
        let lines: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(lines, paths);
    }

    // Open at 1, a teammate scales to 5, the draft takes 5, they scale back to 1.
    #[test]
    fn a_change_away_and_back_across_reloads_is_listed() {
        let at = |n: u32| json!({"metadata": {"name": "a"}, "spec": {"replicas": n}});
        let seen = [without_server_owned(&at(1)), without_server_owned(&at(5))];
        assert_eq!(
            super::competing_changes(&seen, &at(1), &yaml(&at(5))),
            ["spec.replicas"]
        );
        assert!(super::competing_changes(&seen, &at(1), &yaml(&at(1))).is_empty());
    }

    fn init(names: &[&str]) -> serde_json::Value {
        let items: Vec<_> = names.iter().map(|name| json!({"name": name})).collect();
        json!({"spec": {"initContainers": items}})
    }

    // A teammate's insertion keeps its place only if the text keeps it there.
    #[test]
    fn moving_a_remotely_added_item_is_listed() {
        let (base, latest) = (init(&["a", "b"]), init(&["a", "setup", "b"]));
        assert_eq!(
            competing_changes(&base, &latest, &yaml(&init(&["a", "b", "setup"]))),
            ["spec.initContainers"]
        );
        assert!(competing_changes(&base, &latest, &yaml(&latest)).is_empty());
    }

    // Reordering only what the cluster left alone is the edit's own change.
    #[test]
    fn an_own_reorder_of_unmoved_items_is_not_listed() {
        let (base, latest) = (init(&["a", "b"]), init(&["a", "b", "new"]));
        assert!(competing_changes(&base, &latest, &yaml(&init(&["b", "a", "new"]))).is_empty());
    }

    // A key that looks like nested keys is its own path, and cannot break the warning's lines.
    #[test]
    fn keys_that_look_like_paths_stay_distinct() {
        let base = json!({"data": {"a": {"b": "1"}, "a\"][\"b": "1"}});
        let latest = json!({"data": {"a": {"b": "2"}, "a\"][\"b": "2"}});
        let paths = competing_changes(&base, &latest, &yaml(&base));
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert_ne!(paths[0], paths[1]);
        let forged = path_text(&[Step::Key("x\ny\u{202e}")]);
        assert!(
            !forged.contains('\n') && !forged.contains('\u{202e}'),
            "{forged}"
        );
        assert_eq!(path_text(&[Step::Key("a"), Step::Item("b.c")]), "a[b.c]");
        assert_eq!(path_text(&[Step::Item("b]")]), "[name=\"b]\"]");
    }

    #[test]
    fn a_review_without_a_change_has_no_diff() {
        assert!(patch("a", "x: 1\n", "x: 1\n").is_none());
        assert!(patch("a", "x: 1\n", "x: 2\n").is_some());
    }

    #[test]
    fn a_reload_leaves_a_nested_version_key() {
        let yaml = "metadata:\n  annotations:\n    resourceVersion: keep\n  name: a\n  \
                    resourceVersion: \"7\"\n";
        let moved = with_resource_version(yaml, "8");
        assert!(moved.contains("resourceVersion: keep"), "{moved}");
        assert!(moved.contains("resourceVersion: \"8\""), "{moved}");
    }

    #[test]
    fn a_reload_reaches_a_flow_style_version() {
        let moved = with_resource_version("metadata: {name: a, resourceVersion: \"7\"}\n", "8");
        let value: serde_json::Value = serde_saphyr::from_str(&moved).unwrap();
        assert_eq!(value["metadata"]["resourceVersion"], "8", "{moved}");
        assert_eq!(value["metadata"]["name"], "a", "{moved}");
    }

    // A review hides status, so an edit that sets it is not sent.
    #[test]
    fn an_edit_that_sets_status_is_recognised() {
        assert!(sets_status("kind: X\nstatus:\n  phase: Ready\n"));
        assert!(!sets_status("kind: X\nspec:\n  status: keep\n"));
    }

    // The editor's own wording for a moved context: no mention of a shell, and the way forward.
    #[test]
    fn a_moved_context_says_to_open_the_object_again() {
        let cluster = "dev".to_owned();
        for refusal in [
            WriteRefusal::Moved {
                cluster: cluster.clone(),
            },
            WriteRefusal::Stale {
                cluster: cluster.clone(),
            },
        ] {
            let text = edit_error_text(&WriteError::Refused(refusal));
            assert!(text.contains("\"dev\""), "{text}");
            assert!(text.contains("open the object again"), "{text}");
            assert!(text.contains("copy your edit"), "{text}");
            assert!(!text.contains("shell"), "{text}");
            assert!(!text.contains("tab"), "{text}");
        }
    }

    #[test]
    fn other_failures_keep_the_cores_wording() {
        for error in [
            WriteError::Conflict,
            WriteError::Rejected("denied by policy".to_owned()),
            WriteError::Refused(WriteRefusal::Protected {
                cluster: "prod".to_owned(),
            }),
        ] {
            assert_eq!(edit_error_text(&error), error.to_string());
        }
    }

    // A refused apply keeps the typed text, back at the editing step, with the editor's wording.
    #[gpui_kit::test]
    async fn a_refused_apply_keeps_the_edit_and_says_to_reopen(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let kind = KindId::pods();
        t.connect_alpha(cx, standard_sections()).await;
        t.open_kind(
            cx,
            kind.plural(),
            objects(kind.clone(), &[("demo", "api", 0)], SyncState::Synced, 1),
        )
        .await;
        t.dispatch(
            cx,
            "resource_list::SelectRow",
            json!({"namespace": "demo", "name": "api"}),
        )
        .unwrap();
        let view = t.detail(cx).expect("the detail pane is open");
        let failed = view.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            matches!(&failed.read(cx).fetch, Fetch::Failed(_))
        })
        .await;
        let object = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "api", "namespace": "demo", "uid": "uid-api", "resourceVersion": "1"
            },
            "spec": {"containers": [{"name": "app", "image": "nginx"}]},
        });
        let detail = ObjectDetail {
            yaml: serde_saphyr::to_string(&object).unwrap(),
            object,
            managed_fields: None,
            events: Vec::new(),
            events_truncated: false,
            events_error: None,
        };
        let typed = "kind: Pod\nspec: typed\n";
        cx.update_window(t.window, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.show_fetched(detail, window, cx);
                v.start_edit(window, cx);
                let edit = v.edit.as_ref().expect("the edit started");
                edit.editor
                    .update(cx, |state, cx| state.set_value(typed, window, cx));
                v.edit_applying(cx);
                v.edit_applied(
                    Err(WriteError::Refused(WriteRefusal::Moved {
                        cluster: "kind-alpha".to_owned(),
                    })),
                    window,
                    cx,
                );
            })
        })
        .unwrap();
        cx.run_until_parked();
        view.read_with(cx, |v, cx| {
            let edit = v.edit.as_ref().expect("the edit is kept");
            assert!(matches!(edit.phase, Phase::Editing));
            assert_eq!(edit.editor.read(cx).value().to_string(), typed);
            let error = edit.error.as_deref().unwrap();
            assert!(error.contains("open the object again"), "{error}");
            assert!(!error.contains("shell"), "{error}");
        });
    }
}
