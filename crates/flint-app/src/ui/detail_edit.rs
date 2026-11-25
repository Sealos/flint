//! Editing an object's YAML in the detail pane (UP-51, UP-52): edit, Review (a server dry run
//! and its diff), Apply through the shared confirmation, and a reload when the object changed.
//!
//! Every check that matters is in `flint_core::write`. This keeps the editor's text and which
//! step it is at. A Secret opens with its values masked, as the YAML tab shows them. A mask left
//! in place keeps the value on the cluster: the core sends only what changed and never a mask
//! (`flint_core::secret_edit`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use flint_core::write::WriteTarget;
use flint_core::write::{WriteAction, WriteError, WriteOutcome, WriteProposal, WriteReview};
use flint_core::{ObjectDetail, WriteRefusal};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::diff::{DiffFile, DiffState};
use gpui_kit::component::input::EditorState;
use gpui_kit::*;
use serde_json::Value;

use super::access::Gated;
use super::{DetailEvent, DetailTab, DetailView, Fetch, protected_reason};
use crate::ui::write_binding::ConnectionGuard;
use crate::ui::write_confirm;
use crate::ui::yaml_diff::patch;

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
    /// The text as opened, moved to each reloaded `resourceVersion`: what counts as no change.
    baseline: String,
    /// The confirmed endpoint number it was opened on. A context moved and put back has the same
    /// URL but another number.
    endpoint: Option<u64>,
    /// Its context moved or left since: the text stays, and it can no longer be sent.
    moved: bool,
    /// A question opened while the editor was hidden: focus goes back to it once it shows.
    refocus: bool,
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
    /// The connection the last review started on: its confirmation closes once that changes,
    /// even if the context came back to the same server meanwhile.
    reviewed_on: Option<ConnectionGuard>,
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

/// The object the editor opens: a Secret's values (and annotation values that may copy them)
/// masked, as the YAML tab shows them.
fn shown_for_edit(object: &Value, is_secret: bool) -> Value {
    let mut shown = object.clone();
    if is_secret || flint_core::redact::is_secret_object(object) {
        flint_core::redact::mask_secret(&mut shown, flint_core::secret_edit::MASK);
    }
    shown
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

/// `object` without what the server sets in `metadata`. `status` is kept: an edit may set it on
/// a kind without a status subresource, and then a change to it on the cluster competes.
fn without_server_owned(object: &Value) -> Value {
    let mut kept = object.clone();
    if let Some(metadata) = kept.get_mut("metadata").and_then(Value::as_object_mut) {
        for field in SERVER_OWNED_METADATA {
            metadata.remove(field);
        }
    }
    kept
}

/// The paths that differ between any state in `seen` and `latest` and that `yaml` would set to
/// something other than `latest`, sorted. Text that does not parse has none: its review fails.
/// `status` counts only when the text sets it: otherwise the write keeps the cluster's.
fn competing_changes(seen: &[Value], latest: &Value, yaml: &str) -> Vec<String> {
    let Ok(mut edited) = serde_saphyr::from_str::<Value>(yaml) else {
        return Vec::new();
    };
    if flint_core::redact::is_secret_object(latest) {
        // A mask left in place keeps the latest value: it overwrites nothing. A `stringData`
        // value lands in `data` under its key, so it is compared there.
        edited = flint_core::secret_edit::effective_data(
            &flint_core::secret_edit::with_live_values(&edited, latest),
        );
    }
    let sets_status = edited.get("status").is_some();
    let compared = |object: &Value| {
        let mut kept = without_server_owned(object);
        if let Some(fields) = kept.as_object_mut().filter(|_| !sets_status) {
            fields.remove("status");
        }
        kept
    };
    let latest = compared(latest);
    let edited = compared(&edited);
    let seen: Vec<Value> = seen.iter().map(compared).collect();
    let mut found = BTreeSet::new();
    for earlier in &seen {
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

/// Why an edit whose context moved cannot be reviewed or applied.
const EDIT_MOVED: &str = "This context changed since the edit was opened, so it cannot be \
     applied. Copy your text, then open the object again.";

impl DetailView {
    /// Whether the open edit's context moved or left since it was opened, even if it came back.
    pub(in crate::ui) fn edit_moved(&self) -> bool {
        self.edit.as_ref().is_some_and(|edit| edit.moved)
    }

    /// Why the open edit may not be written, if its context moved.
    pub(in crate::ui) fn edit_moved_refusal(&self) -> Option<String> {
        self.edit_moved().then(|| EDIT_MOVED.to_owned())
    }

    /// Takes in the context's confirmed endpoint number.
    pub(in crate::ui) fn note_endpoint(&mut self, endpoint: u64, cx: &mut Context<Self>) {
        self.endpoint = Some(endpoint);
        let Some(edit) = &mut self.edit else {
            return;
        };
        match edit.endpoint {
            None => edit.endpoint = Some(endpoint),
            Some(bound) if bound != endpoint => self.mark_edit_moved(cx),
            Some(_) => {}
        }
    }

    /// The context is gone from the kubeconfig files: if it returns, it is not the one edited.
    pub(in crate::ui) fn note_context_gone(&mut self, cx: &mut Context<Self>) {
        self.endpoint = None;
        self.mark_edit_moved(cx);
    }

    fn mark_edit_moved(&mut self, cx: &mut Context<Self>) {
        if let Some(edit) = self.edit.as_mut().filter(|edit| !edit.moved) {
            edit.moved = true;
            edit.error = Some(EDIT_MOVED.to_owned());
            cx.notify();
        }
    }

    /// Whether an edit is open, changed or not.
    pub(in crate::ui) fn has_open_edit(&self) -> bool {
        self.edit.is_some()
    }

    /// Whether the edit's confirmed write is running.
    pub(in crate::ui) fn is_applying_edit(&self) -> bool {
        matches!(self.edit.as_ref().map(|e| &e.phase), Some(Phase::Applying))
    }

    /// Hands focus to the editor once a question opened over a hidden one has closed.
    pub(super) fn finish_focus_return(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.as_mut().filter(|edit| edit.refocus) else {
            return;
        };
        match edit.phase {
            Phase::Reviewing | Phase::Applying => {}
            Phase::Reviewed(_) => edit.refocus = false,
            Phase::Editing | Phase::Conflict => {
                if !window.has_active_dialog(cx) && self.focus.is_focused(window) {
                    edit.refocus = false;
                    cx.defer_in(window, |view, window, cx| {
                        view.focus_shown_editor(window, cx)
                    });
                }
            }
        }
    }

    /// Why this object cannot be edited now, as the disabled button says it.
    pub(in crate::ui) fn edit_blocked(&self) -> Option<String> {
        if self.edit_moved() {
            return Some(EDIT_MOVED.to_owned());
        }
        if self.gone {
            return Some("This object was deleted".to_owned());
        }
        if let Some(why) = &self.sessions_blocked {
            return Some(why.clone());
        }
        if let Some(why) = self.row_writes_paused() {
            return Some(why.to_owned());
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

    /// The loaded object as a manifest for other clusters: as the editor opens it, without
    /// what one cluster's copy carries (uid, version and the like). `None` until it is loaded.
    pub(in crate::ui) fn manifest_yaml(&self) -> Option<Result<String, String>> {
        let Fetch::Loaded { detail, .. } = &self.fetch else {
            return None;
        };
        Some(editable_yaml(&detail.object).and_then(|yaml| {
            flint_core::write::portable_manifest(&yaml).map_err(|e| e.to_string())
        }))
    }

    /// Whether [`Self::manifest_yaml`] has an object to read.
    pub(in crate::ui) fn has_loaded_object(&self) -> bool {
        matches!(self.fetch, Fetch::Loaded { .. })
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
        let text = match editable_yaml(&shown_for_edit(&detail.object, self.is_secret)) {
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
        let baseline = editor.read(cx).value().to_string();
        let diff = cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx));
        self.tab = DetailTab::Yaml;
        self.edit = Some(YamlEdit {
            editor,
            diff,
            target,
            baseline,
            endpoint: self.endpoint,
            moved: false,
            refocus: false,
            phase: Phase::Editing,
            error: None,
            seen: vec![opened],
            latest: None,
            competing: Vec::new(),
            reviewed_on: None,
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

    /// Focuses the editor if it is on show and can be typed in.
    pub(in crate::ui) fn focus_shown_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab == DetailTab::Yaml
            && let Some(edit) = &self.edit
            && matches!(edit.phase, Phase::Editing | Phase::Conflict)
        {
            edit.editor.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    /// Moves focus from the editor to this pane as the editor is about to be hidden. Left on a
    /// control no longer drawn, focus would fall through to the list.
    pub(super) fn keep_focus_from_hidden_editor(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(edit) = &self.edit
            && edit.editor.read(cx).focus_handle(cx).is_focused(window)
        {
            self.focus.focus(window, cx);
        }
    }

    /// [`Self::focus_shown_editor`] for a result that arrives later: only while focus is still
    /// in this pane, so it never pulls focus from where it was moved since the request.
    fn return_focus_to_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus.contains_focused(window, cx) {
            self.focus_shown_editor(window, cx);
        }
    }

    pub(super) fn review_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_review_edit() {
            return;
        }
        // The pane holds focus while the review runs, and gives it back to the editor after.
        self.focus.focus(window, cx);
        self.run_review(window, cx);
        // Refused at once: the editor is still on show.
        self.focus_shown_editor(window, cx);
    }

    /// Starts a review. A caller decides where focus goes if it is refused at once.
    fn run_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_review_edit() {
            return;
        }
        let core = self.core.clone();
        let guards = self.connection_guards.clone();
        let Some(edit) = &mut self.edit else {
            return;
        };
        let yaml = edit.editor.read(cx).value().to_string();
        edit.phase = Phase::Reviewing;
        edit.error = None;
        edit.competing.clear();
        // Before the review is asked for: a change while it runs is a change of connection.
        edit.reviewed_on = guards.and_then(|g| g.guard(&edit.target.cluster, cx));
        let compare = edit
            .latest
            .clone()
            .map(|latest| (edit.seen.clone(), latest, yaml.clone()));
        let run = core.review_write(edit.target.clone(), WriteProposal::ApplyYaml { yaml });
        let name = self.object.name.clone();
        edit._task = cx.spawn_in(window, async move |this, cx| {
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
            let _ = this.update_in(cx, |view, window, cx| {
                view.edit_reviewed(result, patch, competing, window, cx)
            });
        });
        cx.notify();
    }

    fn edit_reviewed(
        &mut self,
        result: Result<WriteReview, WriteError>,
        patch: Option<String>,
        competing: Vec<String>,
        window: &mut Window,
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
        self.return_focus_to_editor(window, cx);
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
    pub(super) fn apply_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_apply_edit() {
            return;
        }
        // The confirmation hands focus back to the pane, which gives it to the editor on a
        // refusal.
        self.focus.focus(window, cx);
        if let Some(edit) = &self.edit
            && let Phase::Reviewed(review) = &edit.phase
        {
            let review = review.clone();
            write_confirm::set_edit_note(review.target(), edit.competing_warning(), cx);
            cx.emit(DetailEvent::ConfirmWrite(review, edit.reviewed_on.clone()));
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
                // After the update under way, which may still be closing the confirmation.
                cx.defer_in(window, |view, window, cx| {
                    view.return_focus_to_editor(window, cx)
                });
            }
        }
        cx.notify();
    }

    /// Fetches the object as it is now, keeps the edit, and reviews it against that.
    pub(super) fn reload_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_reload_edit() {
            return;
        }
        self.focus.focus(window, cx);
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
                self.return_focus_to_editor(window, cx);
                cx.notify();
                return;
            }
        };
        let text = |pointer: &str| detail.object.pointer(pointer).and_then(Value::as_str);
        if text("/metadata/uid") != edit.target.uid.as_deref() {
            edit.phase = Phase::Editing;
            edit.error = Some(WriteError::Replaced.to_string());
            self.return_focus_to_editor(window, cx);
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
            edit.baseline = with_resource_version(&edit.baseline, version);
            edit.target.resource_version = Some(version.to_owned());
        }
        edit.phase = Phase::Editing;
        self.run_review(window, cx);
        // Not reviewed (the cluster was locked meanwhile): the editor is back on show.
        if matches!(self.edit.as_ref().map(|e| &e.phase), Some(Phase::Editing)) {
            self.return_focus_to_editor(window, cx);
        }
    }

    pub(super) fn back_to_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_go_back_to_edit()
            && let Some(edit) = &mut self.edit
        {
            edit.phase = Phase::Editing;
            self.focus_shown_editor(window, cx);
            cx.notify();
        }
    }

    pub(super) fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.can_cancel_edit() {
            self.edit = None;
            cx.notify();
        }
    }

    /// Whether leaving this pane would lose typed text. Also while a confirmed write runs: the
    /// server can still reject it, and the text is what is left to fix it from.
    pub(in crate::ui) fn has_unapplied_edit(&self, cx: &App) -> bool {
        self.edit
            .as_ref()
            .is_some_and(|edit| edit.editor.read(cx).value().as_ref() != edit.baseline.as_str())
    }

    /// Shows the edit and gives it focus, so that a question about it opens over it and Keep
    /// editing returns there, also if the editor only comes back while the question is open.
    pub(in crate::ui) fn reveal_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = DetailTab::Yaml;
        self.focus.focus(window, cx);
        self.focus_shown_editor(window, cx);
        if let Some(edit) = &mut self.edit {
            edit.refocus = matches!(edit.phase, Phase::Reviewing | Phase::Applying);
        }
        cx.notify();
    }

    /// Drops the edit at the person's answer, leaving focus on this pane. A write already
    /// confirmed runs on: its result finds no edit to report to.
    pub(in crate::ui) fn discard_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        }
    }
}

#[cfg(test)]
impl DetailView {
    /// Replaces `from` with `to` in the text being edited.
    pub(in crate::ui) fn replace_in_edit(
        &self,
        from: &str,
        to: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edit = self.edit.as_ref().expect("an edit is open");
        let text = edit.editor.read(cx).value().replace(from, to);
        edit.editor
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    /// Puts the open edit in review, as a slow server would keep it.
    pub(in crate::ui) fn set_edit_reviewing(&mut self, cx: &mut Context<Self>) {
        if let Some(edit) = &mut self.edit {
            edit.phase = Phase::Reviewing;
            cx.notify();
        }
    }

    /// The server the open edit is bound to.
    pub(in crate::ui) fn open_edit_server(&self) -> Option<&str> {
        self.edit.as_ref()?.target.server.as_deref()
    }

    pub(in crate::ui) fn edit_error(&self) -> Option<&str> {
        self.edit.as_ref()?.error.as_deref()
    }

    pub(in crate::ui) fn editor_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.edit
            .as_ref()
            .is_some_and(|e| e.editor.read(cx).focus_handle(cx).is_focused(window))
    }
}

#[cfg(test)]
mod tests {
    use flint_core::write::WriteError;
    use flint_core::{KindId, ObjectDetail, SyncState, WriteRefusal};
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{AppContext as _, TestAppContext};
    use serde_json::json;

    use super::{
        DetailTab, Fetch, Phase, Step, competing_text, edit_error_text, editable_yaml, path_text,
        shown_for_edit, with_resource_version, without_server_owned,
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

    fn secret(first: &str, second: &str) -> serde_json::Value {
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": "db", "resourceVersion": "7",
                         "annotations": {"copy": "c2Vjb25k"}},
            "data": {"alpha": first, "beta": second},
        })
    }

    // A Secret opens masked: keys shown, no value in the editor's text.
    #[test]
    fn a_secret_opens_with_its_values_masked() {
        let text = editable_yaml(&shown_for_edit(&secret("Zmlyc3Q=", "c2Vjb25k"), true)).unwrap();
        assert!(text.contains("alpha:") && text.contains("beta:"), "{text}");
        assert!(text.contains(flint_core::secret_edit::MASK), "{text}");
        assert!(
            !text.contains("Zmlyc3Q") && !text.contains("c2Vjb25k"),
            "{text}"
        );
    }

    // A mask left in place keeps the latest value, so a remote change to it is not overwritten;
    // a value typed over a remote change is named, without either value.
    #[test]
    fn a_mask_left_in_place_competes_with_nothing() {
        let base = secret("Zmlyc3Q=", "c2Vjb25k");
        let latest = secret("dGhpcmQ=", "c2Vjb25k");
        let opened = editable_yaml(&shown_for_edit(&base, true)).unwrap();
        assert!(competing_changes(&base, &latest, &opened).is_empty());
        let masked = format!("alpha: {}", flint_core::secret_edit::MASK);
        let typed = opened.replace(&masked, "alpha: Zm91cnRo");
        assert_ne!(typed, opened);
        let paths = competing_changes(&base, &latest, &typed);
        assert_eq!(paths, vec!["data.alpha"]);
        let text = competing_text(&paths).unwrap();
        for value in ["Zmlyc3Q", "dGhpcmQ", "Zm91cnRo"] {
            assert!(!text.contains(value), "{text}");
        }
    }

    // Kubernetes writes stringData over data: a stringData value typed over a key rotated on the
    // cluster is named, though its data line was left masked.
    #[test]
    fn string_data_over_a_rotated_key_is_named() {
        let base = secret("Zmlyc3Q=", "c2Vjb25k");
        let latest = secret("dGhpcmQ=", "c2Vjb25k");
        let opened = editable_yaml(&shown_for_edit(&base, true)).unwrap();
        let typed = format!("{opened}stringData:\n  alpha: typed-text\n");
        let paths = competing_changes(&base, &latest, &typed);
        assert_eq!(paths, vec!["data.alpha"]);
        let text = competing_text(&paths).unwrap();
        assert!(!text.contains("typed-text"), "{text}");
        // On a key nobody changed, it competes with nothing.
        let quiet = format!("{opened}stringData:\n  beta: typed-text\n");
        assert!(competing_changes(&base, &latest, &quiet).is_empty());
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
        let mut opened = base.clone();
        opened.as_object_mut().unwrap().remove("status");
        assert!(competing_changes(&base, &only_server, &yaml(&opened)).is_empty());
    }

    // A kind without a status subresource takes a typed status: a status that changed on the
    // cluster since is then overwritten, and listed.
    #[test]
    fn a_typed_status_competes_with_the_clusters() {
        let at = |ready: u32| json!({"metadata": {"name": "a"}, "status": {"ready": ready}});
        let typed = yaml(&at(9));
        assert_eq!(competing_changes(&at(1), &at(5), &typed), ["status.ready"]);
        assert!(competing_changes(&at(1), &at(1), &typed).is_empty());
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
    // A reload's follow-up review, refused at once, leaves focus where it moved meanwhile.
    #[gpui_kit::test]
    async fn a_refusal_after_a_reload_keeps_focus_moved_away(cx: &mut TestAppContext) {
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
        let moved_to = cx.update(|cx| cx.focus_handle());
        cx.update_window(t.window, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.show_fetched(detail.clone(), window, cx);
                v.start_edit(window, cx);
                // Its context moved meanwhile: the follow-up review is refused at once.
                v.mark_edit_moved(cx);
                let edit = v.edit.as_mut().expect("the edit started");
                edit.phase = Phase::Reviewing;
                window.focus(&moved_to, cx);
                v.edit_reloaded(Ok(std::sync::Arc::new(detail)), window, cx);
                // Read here: this handle is in no drawn view, so a frame would reclaim focus.
                assert_eq!(window.focused(cx).as_ref(), Some(&moved_to));
            })
        })
        .unwrap();
        cx.run_until_parked();
        view.read_with(cx, |v, _| {
            let (error, blocked) = (v.edit_error(), v.edit_blocked());
            assert!(
                error.is_some_and(|e| e.contains("open the object again")),
                "{error:?} {blocked:?}"
            );
        });
    }

    // covers UP-51: a Secret is editable, and its editor shows masks, never a value.
    #[gpui_kit::test]
    async fn a_secret_edit_starts_masked(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let view = show_secret(cx, &t).await;
        cx.update_window(t.window, |_, window, cx| {
            view.update(cx, |v, cx| {
                assert_eq!(v.edit_blocked(), None);
                v.start_edit(window, cx);
            })
        })
        .unwrap();
        view.read_with(cx, |v, cx| {
            let edit = v.edit.as_ref().expect("the edit started");
            let text = edit.editor.read(cx).value().to_string();
            assert!(text.contains(flint_core::secret_edit::MASK), "{text}");
            assert!(!text.contains("Zmlyc3Q"), "{text}");
        });
    }

    // covers UP-54: on a protected cluster, a Secret's Edit is disabled with Unlock beside it.
    #[gpui_kit::test]
    async fn a_protected_secret_offers_unlock_beside_edit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.toml"),
            "protect_local_clusters = true\n",
        )
        .unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let view = show_secret(cx, &t).await;
        view.update(cx, |v, _| v.tab = DetailTab::Yaml);
        cx.run_until_parked();
        cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
        let drawn = cx
            .update_window(t.window, |_, window, _| {
                let state = |id| window.try_find(id).map(|b| b.disabled() == Some(true));
                (state("yaml-edit"), state("yaml-edit-unlock"))
            })
            .unwrap();
        assert_eq!(drawn, (Some(true), Some(false)));
    }

    /// Opens Secret `demo/db` in the detail pane, its fetch answered by hand.
    async fn show_secret(
        cx: &mut TestAppContext,
        t: &crate::ui::support_tests::TestWorkspace,
    ) -> gpui_kit::Entity<super::DetailView> {
        let kind = KindId::new("", "secrets");
        t.connect_alpha(cx, standard_sections()).await;
        t.open_kind(
            cx,
            kind.plural(),
            objects(kind.clone(), &[("demo", "db", 0)], SyncState::Synced, 1),
        )
        .await;
        t.dispatch(
            cx,
            "resource_list::SelectRow",
            json!({"namespace": "demo", "name": "db"}),
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
            "kind": "Secret",
            "metadata": {
                "name": "db", "namespace": "demo", "uid": "uid-db", "resourceVersion": "1"
            },
            "data": {"alpha": "Zmlyc3Q="},
        });
        let detail = ObjectDetail {
            yaml: serde_saphyr::to_string(&object).unwrap(),
            object,
            managed_fields: None,
            events: Vec::new(),
            events_truncated: false,
            events_error: None,
        };
        cx.update_window(t.window, |_, window, cx| {
            view.update(cx, |v, cx| v.show_fetched(detail, window, cx))
        })
        .unwrap();
        cx.run_until_parked();
        view
    }

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
