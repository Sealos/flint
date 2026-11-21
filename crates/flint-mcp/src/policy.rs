//! What the MCP server may show (ADR-0009): opt-in clusters, their scopes (ADR-0013),
//! redaction, and output bounds. Everything here is pure so it can be tested without a cluster.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use flint_core::Settings;
use flint_core::kubeconfig::{ContextInfo, LoadedKubeconfig};
use flint_core::read::{KindInfo, is_dns1123_label, is_dns1123_subdomain, is_path_segment};
use flint_core::settings::AiExposureState;
pub use flint_core::settings::AiScope;
use serde_json::Value;

/// Replaces every redacted value.
pub const REDACTED: &str = "<redacted>";

/// Annotation `kubectl apply` writes with the whole object, Secret values included.
const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

pub use flint_core::redact::{is_safe_secret_annotation, is_safe_secret_column, redact_quoted};

pub use flint_core::paging::Bound;

pub const OBJECTS: Bound = Bound {
    default: 100,
    max: 200,
};
pub const EVENTS: Bound = Bound {
    default: 50,
    max: 200,
};
pub const LOG_LINES: Bound = Bound {
    default: 100,
    max: 500,
};
/// Byte cap on log output, so a few huge lines cannot flood the agent.
pub const LOG_BYTES: usize = 256 * 1024;
/// Lines fetched before the requested tail, so text spanning lines is redacted with its start.
pub const LOG_LOOK_BEHIND: u32 = 200;
/// Byte cap on the rows of one list_resources result, so long printer-column cells cannot
/// flood the agent.
pub const LIST_BYTES: usize = 256 * 1024;
/// Byte cap on one object's YAML.
pub const OBJECT_BYTES: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum PolicyError {
    /// Used for clusters that do not exist and for clusters that are not exposed, so callers
    /// cannot tell the two apart.
    ClusterNotFound(String),
    /// A request outside the scope of a cluster the agent can see. It names only the cluster, so
    /// its text is the same whether what was asked for exists or not, and nothing beyond the
    /// scope can be probed.
    OutOfScope(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::ClusterNotFound(name) => write!(
                f,
                "cluster \"{name}\" not found. Call list_clusters for the available clusters."
            ),
            PolicyError::OutOfScope(name) => write!(
                f,
                "this request is outside the scope exposed to agents on cluster \"{name}\". Call \
                 list_clusters for the namespaces and kinds in scope."
            ),
        }
    }
}

impl std::error::Error for PolicyError {}

/// Whether the agent may see this context: `ai_exposed_clusters` has its name **and** its current
/// API server URL.
fn is_exposed(settings: &Settings, info: &ContextInfo) -> bool {
    settings
        .ai_exposure(&info.name, info.server.as_deref())
        .is_exposed()
}

/// Contexts the agent may see.
pub fn exposed_clusters<'a>(
    settings: &Settings,
    kubeconfig: &'a LoadedKubeconfig,
) -> Vec<&'a ContextInfo> {
    kubeconfig
        .contexts
        .iter()
        .filter(|c| is_exposed(settings, c))
        .collect()
}

/// How many exposed contexts now point at a different server than the one exposed. They are
/// hidden; only the count is reported, not their names.
pub fn changed_exposures(settings: &Settings, kubeconfig: &LoadedKubeconfig) -> usize {
    kubeconfig
        .contexts
        .iter()
        .filter(|c| {
            matches!(
                settings.ai_exposure(&c.name, c.server.as_deref()),
                AiExposureState::ServerChanged { .. }
            )
        })
        .count()
}

/// Which program serves MCP: it decides what setup advice can be followed where it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    /// `flint mcp`: the app is installed, with its Expose to AI toggle.
    App,
    /// `flint-mcp`: no app, so clusters are exposed by editing `settings.toml`.
    Headless,
}

impl Flavour {
    /// The flavour of the program named `program`.
    pub fn of_program(program: &str) -> Self {
        if program == "flint" {
            Flavour::App
        } else {
            Flavour::Headless
        }
    }
}

/// What a hint may tell the user to do: the flavour, and where its settings file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    flavour: Flavour,
    settings: String,
}

impl Setup {
    pub fn new(flavour: Flavour, config_dir: &Path) -> Self {
        Self {
            flavour,
            settings: shown_path(&config_dir.join("settings.toml")),
        }
    }

    /// How to expose a first cluster.
    fn expose(&self) -> String {
        match self.flavour {
            Flavour::App => "Ask the user to open Flint and turn on Expose to AI (the robot \
                             button) next to a cluster in the sidebar."
                .to_owned(),
            Flavour::Headless => format!(
                "Ask the user to add an [[ai_exposed_clusters]] entry with the cluster's context \
                 and server to {}. If the file already has an ai_exposed_clusters = [...] array, \
                 the entry goes in that array instead.",
                self.settings
            ),
        }
    }

    /// The advice when nothing is exposed and `changed` entries point at another server. Without
    /// the app, a new entry beside a re-pointed one would drop that entry's scope, so only the
    /// note on updating it is given then.
    fn nothing_exposed(&self, changed: usize) -> String {
        if changed > 0 && self.flavour == Flavour::Headless {
            String::new()
        } else {
            format!(" {}", self.expose())
        }
    }

    /// How to bring back clusters whose server changed, after "...so it is hidden until".
    fn expose_again(&self, one: bool) -> String {
        let (it, entry) = if one {
            ("it", "its entry")
        } else {
            ("them", "their entries")
        };
        match self.flavour {
            Flavour::App => format!("they expose {it} again in Flint."),
            Flavour::Headless => format!(
                "they update the server in {entry} under [[ai_exposed_clusters]] in {}.",
                self.settings
            ),
        }
    }

    /// The end of the error for a settings file that cannot be read.
    pub fn fix_settings(&self) -> &'static str {
        match self.flavour {
            Flavour::App => "Ask the user to fix it; the Flint app shows the details.",
            Flavour::Headless => "Ask the user to fix that file.",
        }
    }
}

/// A path as an agent may read it. A component with anything like a credential is hidden whole:
/// spaces are valid in a path, so a passphrase can run past where the redactor stops. The whole
/// path is then redacted too, for a credential that spans components.
fn shown_path(path: &Path) -> String {
    let mut shown = path
        .display()
        .to_string()
        .split('/')
        .map(|component| {
            let mut text = component.to_owned();
            if redact_text(&mut text) > 0 {
                "[REDACTED]".to_owned()
            } else {
                text
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    redact_text(&mut shown);
    shown
}

/// How rmcp starts the error for arguments that do not fit a tool's types.
const ARGUMENT_ERROR: &str = "failed to deserialize parameters: ";

/// `text` without the value it repeats, if it is such an error: serde's ``invalid type: string
/// "...", expected u32`` keeps only what was expected. It is rebuilt rather than masked, because
/// a value can hold the quotes a mask would look for. A missing or repeated field is left as it
/// is: it names a field of the tool's schema and no value.
pub fn argument_error(text: &str) -> Option<String> {
    let rest = text.strip_prefix(ARGUMENT_ERROR)?;
    if rest.starts_with("missing field `") || rest.starts_with("duplicate field `") {
        return None;
    }
    Some(match rest.rsplit_once(", expected ") {
        Some((_, expected)) => format!("{ARGUMENT_ERROR}wrong type or value, expected {expected}"),
        None => format!("{ARGUMENT_ERROR}wrong type or value"),
    })
}

/// One entry of `list_clusters`' `kubeconfig_errors`: the file and a one-line reason, each
/// redacted on its own, since a redacted value runs to the end of its line.
pub fn kubeconfig_error_row(path: &Path, error: &str) -> Value {
    // Redacted before the summary cuts it: a cut can remove what marks a value as a credential.
    let mut line = error
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned();
    redact_text(&mut line);
    let reason = flint_core::redact::safe_error_summary(&line);
    serde_json::json!({ "file": shown_path(path), "error": reason })
}

/// How many exposed contexts are in none of the loaded kubeconfig files. They are hidden; only
/// the count is reported, not their names.
pub fn missing_exposures(settings: &Settings, kubeconfig: &LoadedKubeconfig) -> usize {
    settings
        .ai_exposed_clusters
        .iter()
        .map(|exposure| exposure.context.as_str())
        .filter(|name| !kubeconfig.contexts.iter().any(|c| c.name == *name))
        .collect::<BTreeSet<_>>()
        .len()
}

/// A hint for the agent when list_clusters has nothing useful to show. For the `missing`
/// exposures, `paths` are the kubeconfig files this server checked, and `load_failed` says some of
/// them could not be loaded: the context may be in one, where adding the file again would not help.
pub fn list_clusters_hint(
    exposed: usize,
    changed: usize,
    missing: usize,
    paths: &[PathBuf],
    load_failed: bool,
    setup: &Setup,
) -> Option<String> {
    let changed_note = match changed {
        0 => String::new(),
        1 => format!(
            " One cluster the user exposed now points to a different API server, so it is hidden \
             until {}",
            setup.expose_again(true)
        ),
        n => format!(
            " {n} clusters the user exposed now point to a different API server, so they are \
             hidden until {}",
            setup.expose_again(false)
        ),
    };
    let missing_note = match missing {
        0 => String::new(),
        n => {
            let clusters = if n == 1 {
                "One cluster the user exposed is".to_owned()
            } else {
                format!("{n} clusters the user exposed are")
            };
            let files = paths
                .iter()
                // One path at a time: a redacted value runs to the end of its line.
                .map(|path| shown_path(path))
                .collect::<Vec<_>>()
                .join(", ");
            let advice = if load_failed {
                "Some of these files could not be loaded (see kubeconfig_errors): ask the user \
                 to fix them. If the cluster is in none of them, ask the user to add its \
                 kubeconfig file to kubeconfig_paths in Flint's settings.toml."
            } else {
                "Ask the user to add the missing kubeconfig file to kubeconfig_paths in Flint's \
                 settings.toml."
            };
            format!(
                " {clusters} not in the kubeconfig files this server checked ({files}). {advice}"
            )
        }
    };
    if exposed == 0 && missing == 0 {
        Some(format!(
            "No clusters are exposed to AI agents.{}{changed_note}",
            setup.nothing_exposed(changed)
        ))
    } else if changed > 0 || missing > 0 {
        Some(
            format!("{changed_note}{missing_note}")
                .trim_start()
                .to_owned(),
        )
    } else {
        None
    }
}

/// Looks up a cluster the agent asked for. Unknown and unexposed clusters give the same error.
pub fn exposed_cluster<'a>(
    settings: &Settings,
    kubeconfig: &'a LoadedKubeconfig,
    name: &str,
) -> Result<&'a ContextInfo, PolicyError> {
    kubeconfig
        .context(name)
        .filter(|c| is_exposed(settings, c))
        .ok_or_else(|| PolicyError::ClusterNotFound(name.to_owned()))
}

/// Whether a caller may name a namespace or a container with `name`: a DNS-1123 label.
pub fn is_label_name(name: &str) -> bool {
    is_path_segment(name) && is_dns1123_label(name)
}

/// Whether a caller may name a pod with `name`: a DNS-1123 subdomain.
pub fn is_pod_name(name: &str) -> bool {
    is_path_segment(name) && is_dns1123_subdomain(name)
}

/// Whether a caller may name an object of any kind with `name`: a DNS-1123 subdomain, or, for
/// the kinds whose names are only checked as path segments (RBAC roles and bindings such as
/// `system:controller:job-controller`), at most 253 letters, digits, `-`, `.`, `_`, `:` and `@`.
/// Always a single path segment.
pub fn is_object_name(name: &str) -> bool {
    is_path_segment(name)
        && name.len() <= 253
        && (is_dns1123_subdomain(name)
            || name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '@')))
}

/// What the agent may read of an exposed cluster (ADR-0013): the scope its entry in
/// `settings.toml` names, or everything for an entry without one.
pub fn cluster_scope(settings: &Settings, info: &ContextInfo) -> AiScope {
    settings
        .ai_scope(&info.name, info.server.as_deref())
        .unwrap_or_default()
}

/// The error for anything outside the scope of an exposed cluster. One text for every such
/// request, so an agent cannot tell what exists beyond its scope.
pub fn out_of_scope(cluster: &str) -> PolicyError {
    PolicyError::OutOfScope(cluster.to_owned())
}

/// A scope with its hand-written kind names resolved against a cluster's kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKinds {
    /// The scope with each name that resolved as its kind id. A name that did not stays as
    /// written and matches nothing, so the list never becomes empty, which would mean every kind.
    pub scope: AiScope,
    /// The names that resolved to no kind, to several, or to one that a group missing from
    /// discovery could change.
    pub unmatched: Vec<String>,
    /// Part of discovery could not be read and a name did not resolve: it may exist.
    pub discovery_incomplete: bool,
}

/// What `list_clusters` could tell about a scope on a cluster. Reports only what the scope itself
/// names, and counts: which API groups are failing is not in scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeCheck {
    /// Kinds the scope names that resolved to nothing.
    pub unmatched_kinds: Vec<String>,
    /// Namespaces the scope names that do not exist.
    pub unmatched_namespaces: Vec<String>,
    /// Namespaces of the scope not looked up (past the limit, out of time) or whose lookup
    /// failed. Any of them may be missing.
    pub unchecked_namespaces: usize,
    /// Part of API discovery could not be read, so an unmatched kind may exist.
    pub discovery_incomplete: bool,
    /// The check could not run at all.
    pub not_run: bool,
}

impl ScopeCheck {
    /// A check that could not run on a scope.
    pub fn not_run(scope: &AiScope) -> Self {
        ScopeCheck {
            unchecked_namespaces: scope.namespaces.len(),
            not_run: true,
            ..ScopeCheck::default()
        }
    }

    /// What the check could not cover, in a sentence or two, if anything.
    pub fn incomplete(&self) -> Option<String> {
        if self.not_run {
            return Some(
                "The scope could not be checked against this cluster: names that match nothing \
                 are not listed. It is enforced all the same."
                    .to_owned(),
            );
        }
        let mut notes = Vec::new();
        if self.unchecked_namespaces > 0 {
            notes.push(format!(
                "{} of the scope's namespaces could not be checked, so one that does not exist \
                 may not be listed.",
                self.unchecked_namespaces
            ));
        }
        if self.discovery_incomplete {
            notes.push(
                "Part of API discovery is failing, so a kind in unmatched_kinds may exist. It \
                 grants nothing until discovery recovers, or until it is written as its exact \
                 kind id."
                    .to_owned(),
            );
        }
        (!notes.is_empty()).then(|| notes.join(" "))
    }
}

/// The id of the one kind a hand-written scope name surely means. `name.group` stays bound to
/// that exact group, so a saved id whose kind is gone cannot be read as `plural.version.group`
/// and land in another group. A bare name needs complete discovery: a group in `skipped` could
/// hold a better or a second match.
fn scope_kind_id(name: &str, kinds: &[KindInfo], skipped: &[String]) -> Option<String> {
    let resolved = match name.split_once('.') {
        Some((base, group)) => {
            if skipped.iter().any(|g| g.eq_ignore_ascii_case(group)) {
                return None;
            }
            let in_group: Vec<KindInfo> = kinds
                .iter()
                .filter(|k| k.group().eq_ignore_ascii_case(group))
                .cloned()
                .collect();
            flint_core::read::resolve_kind(&in_group, base, None)
                .ok()?
                .id()
        }
        None if skipped.is_empty() => flint_core::read::resolve_kind(kinds, name, None).ok()?.id(),
        None => return None,
    };
    Some(resolved.as_str().to_owned())
}

/// Resolves `scope`'s kind names as the tools resolve theirs
/// ([`flint_core::read::resolve_kind`]): `pod`, `Deployment` and `deploy` name the kind they do
/// for `kubectl get`. `skipped` lists the API groups discovery could not read, or reports as
/// stale.
pub fn resolve_scope_kinds(
    scope: &AiScope,
    kinds: &[KindInfo],
    skipped: &[String],
) -> ResolvedKinds {
    let mut names = Vec::with_capacity(scope.kinds.len());
    let mut unmatched = Vec::new();
    for name in &scope.kinds {
        let is_id = kinds
            .iter()
            .any(|k| k.id().as_str().eq_ignore_ascii_case(name));
        if is_id {
            names.push(name.clone());
        } else if let Some(id) = scope_kind_id(name, kinds, skipped) {
            names.push(id);
        } else {
            names.push(name.clone());
            unmatched.push(name.clone());
        }
    }
    let discovery_incomplete = !skipped.is_empty() && !unmatched.is_empty();
    ResolvedKinds {
        scope: AiScope::new(scope.namespaces.clone(), names),
        unmatched,
        discovery_incomplete,
    }
}

/// The kinds of `kinds` the agent may read: those the scope allows. Under a namespace scope,
/// cluster-scoped kinds only when the kinds scope names them. On a scoped cluster, events are
/// read only through `get_events`, which drops those about objects out of scope: listed as a
/// kind, an event in an allowed namespace could name a node or a Secret the scope hides.
pub fn kinds_in_scope(scope: &AiScope, kinds: &[KindInfo]) -> Vec<KindInfo> {
    kinds
        .iter()
        .filter(|k| scope.is_everything() || !is_event_kind(k))
        .filter(|k| scope.allows_kind(k.id().as_str(), k.namespaced))
        .cloned()
        .collect()
}

/// Core v1 events and `events.k8s.io` events.
fn is_event_kind(kind: &KindInfo) -> bool {
    kind.plural() == "events" && matches!(kind.group(), "" | "events.k8s.io")
}

/// Which namespaces a request may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Namespaces {
    /// The one namespace asked for.
    One(String),
    /// Every namespace (no namespace scope), or none at all (a cluster-scoped kind).
    All,
    /// Every namespace of the scope, one by one: a request for all namespaces on a cluster
    /// limited to these.
    Only(Vec<String>),
}

/// Which namespaces a request for `requested` (`None`: all) may read, for a `namespaced` kind
/// on `cluster` under `scope`. A namespace outside the scope gives [`out_of_scope`].
pub fn namespaces_for(
    scope: &AiScope,
    cluster: &str,
    namespaced: bool,
    requested: Option<&str>,
) -> Result<Namespaces, PolicyError> {
    if !namespaced {
        return Ok(Namespaces::All);
    }
    match requested {
        Some(ns) if scope.allows_namespace(ns) => Ok(Namespaces::One(ns.to_owned())),
        Some(_) => Err(out_of_scope(cluster)),
        None if scope.namespaces.is_empty() => Ok(Namespaces::All),
        None => Ok(Namespaces::Only(scope.namespaces.clone())),
    }
}

/// Whether an event may be shown: its namespace is in scope, and the object it is about is of
/// a kind in scope (looked up in the cluster's `kinds` by Kind and API group; an object of a
/// kind discovery does not list is left out of a scoped cluster's events).
pub fn event_in_scope(
    scope: &AiScope,
    kinds: &[KindInfo],
    namespace: Option<&str>,
    involved_kind: Option<&str>,
    involved_api_version: Option<&str>,
) -> bool {
    if scope.is_everything() {
        return true;
    }
    if !scope.namespaces.is_empty() && !namespace.is_some_and(|ns| scope.allows_namespace(ns)) {
        return false;
    }
    let group = involved_api_version
        .and_then(|v| v.split_once('/').map(|(g, _)| g))
        .unwrap_or("");
    kinds
        .iter()
        .find(|k| Some(k.kind()) == involved_kind && k.group() == group)
        .is_some_and(|k| scope.allows_kind(k.id().as_str(), k.namespaced))
}

/// Whether objects of the Kind `kind` (as an event names its object, `Pod`) may be read under
/// `scope`: some kind of that name in `kinds` is in scope.
pub fn kind_name_in_scope(scope: &AiScope, kinds: &[KindInfo], kind: &str) -> bool {
    kinds
        .iter()
        .any(|k| k.kind() == kind && scope.allows_kind(k.id().as_str(), k.namespaced))
}

/// Whether the agent may read pod logs under `scope`: pods are in the kinds scope (the
/// namespace is checked with [`namespaces_for`]).
pub fn logs_in_scope(scope: &AiScope) -> bool {
    scope.allows_kind("pods", true)
}

/// Makes an object safe to show: removes `metadata.managedFields`, redacts Secret values at any
/// depth (`mask_nested_secrets`), and redacts credentials in message fields and container
/// arguments ([`redact_object_text`]). Works on single objects and on lists (`items`). Returns
/// how many credentials were replaced; masked Secret values are not counted.
pub fn sanitize(value: &mut Value) -> usize {
    strip(value, false);
    mask_nested_secrets(value, 0);
    redact_object_text(value)
}

/// [`sanitize`] for what was read as a core v1 Secret: the object, and every item if it is a
/// list, is redacted as a Secret whatever its `kind` field says.
pub fn sanitize_secret(value: &mut Value) -> usize {
    strip(value, true);
    mask_nested_secrets(value, 0);
    redact_object_text(value)
}

/// Whether `value` is a core v1 list of Secrets. The items of a `SecretList` carry no `kind` or
/// `apiVersion` of their own, so they are redacted as Secrets because of the list. A CRD's
/// `SecretList` in another group is not one.
fn is_secret_list(value: &Value) -> bool {
    value.get("kind").and_then(Value::as_str) == Some("SecretList")
        && value.get("apiVersion").and_then(Value::as_str) == Some("v1")
}

/// The structural part of [`sanitize`]: managed fields and Secret values. `secret` says the
/// value is a Secret (or a list of them) whatever its own `kind`.
fn strip(value: &mut Value, secret: bool) {
    let secret = secret || is_secret_list(value);
    if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            strip(item, secret);
        }
    }
    flint_core::read::strip_managed_fields(value);
    if secret || flint_core::redact::is_secret_object(value) {
        flint_core::redact::mask_secret(value, REDACTED);
    }
}

/// How many levels of JSON inside a string are parsed. Deeper text is matched as text.
const MAX_SERIALIZED_DEPTH: usize = 3;

/// Masks every Secret-shaped object under `value`, whatever holds it: an object of kind `Secret`
/// or `SecretList` whose `apiVersion` is `v1` or missing (embedded manifests may omit it), and
/// strings holding one as JSON or YAML ([`mask_serialized`]). Returns whether any was found.
fn mask_nested_secrets(value: &mut Value, depth: usize) -> bool {
    let (secret, list) = value.as_object().map_or((false, false), |map| {
        let core = map
            .get("apiVersion")
            .and_then(Value::as_str)
            .is_none_or(|version| version == "v1");
        let kind = map.get("kind").and_then(Value::as_str).unwrap_or("");
        (
            core && kind.eq_ignore_ascii_case("Secret"),
            core && kind.eq_ignore_ascii_case("SecretList"),
        )
    });
    if secret {
        mask_secret_fields(value);
    }
    if list && let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
        items.iter_mut().for_each(mask_secret_fields);
    }
    let mut found = secret || list;
    match value {
        Value::Object(map) => {
            for child in map.values_mut() {
                found |= mask_nested_secrets(child, depth);
            }
        }
        Value::Array(items) => {
            for child in items {
                found |= mask_nested_secrets(child, depth);
            }
        }
        Value::String(text) => found |= mask_serialized(text, depth),
        _ => {}
    }
    found
}

/// [`flint_core::redact::mask_secret`] that also hides a `data`, `stringData` or `annotations`
/// that is not a map, and a list item that is not an object.
fn mask_secret_fields(object: &mut Value) {
    let marker = || Value::String(REDACTED.to_owned());
    if !object.is_object() {
        *object = marker();
        return;
    }
    flint_core::redact::mask_secret(object, REDACTED);
    for field in ["data", "stringData"] {
        if let Some(value) = object.get_mut(field)
            && !value.is_object()
            && !value.is_null()
        {
            *value = marker();
        }
    }
    if let Some(annotations) = object
        .get_mut("metadata")
        .and_then(|metadata| metadata.get_mut("annotations"))
        && !annotations.is_object()
        && !annotations.is_null()
    {
        *annotations = marker();
    }
}

/// Masks a Secret held in a string. JSON is parsed and only the Secret's values change. YAML is
/// parsed under [`parse_yaml`]'s limits and replaced whole when it holds Secret values. Text that
/// cannot be parsed is replaced whole when it still reads as a Secret manifest.
fn mask_serialized(text: &mut String, depth: usize) -> bool {
    if depth < MAX_SERIALIZED_DEPTH
        && text.trim_start().starts_with(['{', '['])
        && let Ok(mut parsed) = serde_json::from_str::<Value>(text)
    {
        if !mask_nested_secrets(&mut parsed, depth + 1) {
            return false;
        }
        let newline = text.ends_with('\n');
        *text = parsed.to_string();
        if newline {
            text.push('\n');
        }
        return true;
    }
    if depth < MAX_SERIALIZED_DEPTH
        && let Some(documents) = parse_yaml(text)
    {
        let mut masked = documents.clone();
        for document in &mut masked {
            mask_nested_secrets(document, depth + 1);
        }
        // A reference to a Secret has no values, so nothing changes and the text is kept.
        let changed = masked != documents;
        if changed {
            REDACTED.clone_into(text);
        }
        return changed;
    }
    let manifest = names_secret_manifest(text);
    if manifest {
        REDACTED.clone_into(text);
    }
    manifest
}

/// Largest string parsed as YAML. Longer text is matched by [`names_secret_manifest`].
const MAX_YAML_BYTES: usize = OBJECT_BYTES;
/// Nesting limit for YAML held in a string.
const MAX_YAML_DEPTH: usize = 32;
/// Node limit for YAML held in a string, also the cap on events replayed from aliases.
const MAX_YAML_NODES: usize = 20_000;
/// Limit on aliases, anchors and merge keys in YAML held in a string.
const MAX_YAML_ALIASES: usize = 256;

/// The YAML documents in `text`, within the limits above. `None` when it is not YAML, breaks a
/// limit, or is one plain scalar with no structure to check.
fn parse_yaml(text: &str) -> Option<Vec<Value>> {
    if text.len() > MAX_YAML_BYTES || !text.contains(':') {
        return None;
    }
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_reader_input_bytes: Some(MAX_YAML_BYTES),
            max_depth: MAX_YAML_DEPTH,
            max_nodes: MAX_YAML_NODES,
            max_events: MAX_YAML_NODES * 2,
            max_documents: 64,
            max_aliases: MAX_YAML_ALIASES,
            max_anchors: MAX_YAML_ALIASES,
            max_merge_keys: MAX_YAML_ALIASES,
            max_total_scalar_bytes: MAX_YAML_BYTES,
        },
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: MAX_YAML_NODES,
            max_replay_stack_depth: MAX_YAML_DEPTH,
            max_alias_expansions_per_anchor: MAX_YAML_ALIASES,
        },
    };
    serde_saphyr::from_multiple_with_options::<Value>(text, options)
        .ok()
        .filter(|documents| !matches!(documents.as_slice(), [Value::String(s)] if s == text))
}

/// Whether `text` has a `kind: Secret` or `kind: SecretList` pair, in any quoting or escaping,
/// with the value on the same line or a later one, and mentions `data`. Errs toward yes.
fn names_secret_manifest(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("data")
        && lower.match_indices("kind").any(|(at, _)| {
            let rest = skip_separators(&lower[at + "kind".len()..]);
            let Some(rest) = rest.strip_prefix(':') else {
                return false;
            };
            let Some(after) = skip_separators(rest).strip_prefix("secret") else {
                return false;
            };
            after.starts_with("list") || !after.starts_with(|c: char| c.is_ascii_alphanumeric())
        })
}

/// `text` after any quotes, whitespace and backslash escapes of whitespace (lowercase).
fn skip_separators(mut text: &str) -> &str {
    loop {
        let rest = text.trim_start_matches(['"', '\'', ' ', '\t', '\r', '\n']);
        let rest = ["\\n", "\\r", "\\t", "\\"]
            .iter()
            .find_map(|escape| rest.strip_prefix(escape))
            .unwrap_or(rest);
        if rest.len() == text.len() {
            return rest;
        }
        text = rest;
    }
}

/// Whether an object field holds free text that a workload or controller wrote: an Event's
/// `message` (`note` in `events.k8s.io`), status and condition messages, container termination
/// messages (which can be the container's last log lines), operators' `failureMessage`,
/// `errorMessage` or `statusMessage`, and lists of them under a key ending in `messages`.
fn is_message_field(key: &str) -> bool {
    let ends_with = |suffix: &str| {
        key.get(key.len().saturating_sub(suffix.len())..)
            .is_some_and(|end| end.eq_ignore_ascii_case(suffix))
    };
    key == "note" || ends_with("message") || ends_with("messages")
}

/// Whether `object` is a core v1 ConfigMap, whose data is shown as the cluster returns it.
fn is_config_map(object: &serde_json::Map<String, Value>) -> bool {
    object.get("kind").and_then(Value::as_str) == Some("ConfigMap")
        && object.get("apiVersion").and_then(Value::as_str) == Some("v1")
}

/// Replaces credential-shaped values ([`flint_core::redact::redact_credentials`]), at any
/// depth, in every string under a `note` key or a key ending in `message` or `messages` (also
/// the strings of a list there), in every annotation value, and in container `args` and
/// `command` lists, where the element after a flag such as `--password` is hidden whole.
/// ConfigMaps are left as they are. Returns how many values were replaced.
pub fn redact_object_text(value: &mut Value) -> usize {
    match value {
        Value::Object(map) if is_config_map(map) => 0,
        Value::Object(map) => map
            .iter_mut()
            .map(|(key, value)| match value {
                Value::String(text) if is_message_field(key) => redact_text(text),
                Value::Array(items) if is_message_field(key) => items
                    .iter_mut()
                    .map(|item| match item {
                        Value::String(text) => redact_text(text),
                        other => redact_object_text(other),
                    })
                    .sum(),
                Value::Array(args) if key == "args" || key == "command" => redact_args(args),
                // Tools keep whole objects and free text in annotations.
                Value::Object(annotations) if key == "annotations" => annotations
                    .iter_mut()
                    .map(|(name, value)| match value {
                        Value::String(text) if name == LAST_APPLIED => redact_last_applied(text),
                        Value::String(text) => redact_text(text),
                        other => redact_object_text(other),
                    })
                    .sum(),
                _ => redact_object_text(value),
            })
            .sum(),
        Value::Array(items) => items.iter_mut().map(redact_object_text).sum(),
        _ => 0,
    }
}

/// Redacts the object `kubectl apply` kept in an annotation as the live object is redacted,
/// so a container argument after `--password` is hidden in the copy too, then the JSON text
/// as free text. Text that is not JSON is only redacted as free text.
fn redact_last_applied(text: &mut String) -> usize {
    let mut count = 0;
    if let Ok(mut applied) = serde_json::from_str::<Value>(text) {
        count = redact_object_text(&mut applied);
        if count > 0 {
            let newline = text.ends_with('\n');
            *text = applied.to_string();
            if newline {
                text.push('\n');
            }
        }
    }
    count + redact_text(text)
}

/// Redacts an argument list: each string through the text redactor, and the whole argument
/// after a credential flag with no value of its own (`["--password", "hunter2"]`).
fn redact_args(args: &mut [Value]) -> usize {
    let mut count = 0;
    let mut hide_next: Option<&'static str> = None;
    for arg in args {
        let Value::String(text) = arg else {
            hide_next = None;
            continue;
        };
        match hide_next.take() {
            Some(kind) if !text.is_empty() && !text.starts_with("[REDACTED:") => {
                *text = format!("[REDACTED:{kind}]");
                count += 1;
            }
            _ => count += redact_text(text),
        }
        hide_next = flint_core::redact::credential_flag(text);
    }
    count
}

/// The `get_resource` header line naming who manages `object`, which must already be redacted.
/// The value comes from the cluster: [`flint_core::gitops`] accepts only plain names of at most
/// 253 bytes (no whitespace or control characters, so it cannot forge another line), and one the
/// credential redactor would touch is left out rather than shown.
pub fn managed_by_line(object: &Value) -> Option<String> {
    manager_line(flint_core::gitops::managed_by_object(object)?)
}

/// [`managed_by_line`] for a Secret, read **before** it is masked: its labels count, and a tracking
/// id naming this Secret gives the tool only ([`flint_core::gitops::managed_by_secret`]).
pub fn managed_by_secret_line(unmasked: &Value) -> Option<String> {
    manager_line(flint_core::gitops::managed_by_secret(unmasked)?)
}

fn manager_line(manager: flint_core::gitops::ManagedBy) -> Option<String> {
    let mut manager = manager.to_string();
    (redact_text(&mut manager) == 0).then(|| format!("# managed_by={manager}\n"))
}

/// Redacts credentials in `text` in place. Returns how many were replaced.
pub fn redact_text(text: &mut String) -> usize {
    let redacted = flint_core::redact::redact_credentials(text);
    let count = redacted.count;
    if let std::borrow::Cow::Owned(new) = redacted.text {
        *text = new;
    }
    count
}

/// Redacts free text from a list cell or event message: a Secret held in it as JSON or YAML,
/// then credentials. Returns how many credentials were replaced; masked Secret values are not
/// counted.
pub fn redact_free_text(text: &mut String) -> usize {
    mask_serialized(text, 0);
    redact_text(text)
}

/// Cuts `text` to at most `max_bytes`, at a line boundary when possible. Returns whether it cut.
pub fn truncate_bytes(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &text[..end];
    (cut.rfind('\n').map_or(cut, |i| &cut[..=i]), true)
}

/// Keeps the last `max_bytes` of `text`, starting at a line boundary: a line cut in the middle
/// is dropped, even when it is the only one. For logs, where the newest lines matter most.
pub fn tail_bytes(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let cut = &text[start..];
    if text.as_bytes()[start - 1] == b'\n' {
        return (cut, true);
    }
    // Drop the partial first line, so no value is kept without its `password=` label.
    (cut.find('\n').map_or("", |i| &cut[i + 1..]), true)
}

/// What `get_pod_logs` returns of a fetched log.
#[derive(Debug, PartialEq, Eq)]
pub struct LogOutput {
    pub text: String,
    /// Whether lines or bytes were dropped.
    pub truncated: bool,
    /// How many credentials the returned text had replaced.
    pub redacted: usize,
    /// Why the text is empty although the log is not: its last line is longer than the cap.
    pub note: Option<String>,
}

/// The last `max_lines` lines of `fetched`, with credentials redacted, then cut to `max_bytes`
/// at a line boundary. Redacting everything fetched first sees each value next to its label and
/// each block with its opening line. Lines are counted in `fetched`, since a redacted block is
/// shorter than the lines it replaces; the byte cap applies to the text actually returned.
/// `older` says the log may go on before `fetched`: leading lines that continue a block whose
/// start is out of reach are then left out (`leading_cut_off_lines`).
pub fn log_output(fetched: &str, max_lines: usize, max_bytes: usize, older: bool) -> LogOutput {
    let redacted = flint_core::redact::redact_credentials(fetched);
    let all = &*redacted.text;
    let safe_start = if older { leading_cut_off_lines(all) } else { 0 };
    let (tail, cut_lines) = tail_lines(fetched, max_lines);
    let skipped = fetched[..fetched.len() - tail.len()].matches('\n').count();
    let window = &all[redacted_line_start(fetched, all, skipped).max(safe_start)..];
    let (kept, cut_bytes) = tail_bytes(window, max_bytes);
    // Only markers the redactor wrote into the kept lines, not ones the workload logged.
    let count = redacted.count_from(all.len() - kept.len());
    let note = if !kept.is_empty() || all.is_empty() {
        None
    } else if window.is_empty() {
        Some("the available lines continue a block that starts earlier and were left out".into())
    } else {
        let limit = if max_bytes.is_multiple_of(1024) {
            format!("{} KiB", max_bytes / 1024)
        } else {
            format!("{max_bytes} bytes")
        };
        Some(format!(
            "the last line is longer than the {limit} limit and was left out"
        ))
    };
    LogOutput {
        text: kept.to_owned(),
        truncated: cut_lines || cut_bytes || safe_start > 0,
        redacted: count,
        note,
    }
}

/// The byte offset in `redacted` of line `skipped` of `original`. Redaction keeps lines one to
/// one, except that a private key body becomes one marker line up to its END line (or the end of
/// the text). A line inside such a body maps to the marker.
fn redacted_line_start(original: &str, redacted: &str, skipped: usize) -> usize {
    const MARKER: &str = "[REDACTED:private-key]";
    let lines: Vec<&str> = original.split_inclusive('\n').collect();
    let mut line = 0;
    let mut offset = 0;
    for shown in redacted.split_inclusive('\n') {
        if line >= skipped {
            return offset;
        }
        if shown.trim() == MARKER && lines.get(line).is_none_or(|l| l.trim() != MARKER) {
            while line < lines.len() && !closes_private_key(lines[line]) {
                line += 1;
            }
            if line > skipped {
                return offset;
            }
        } else {
            line += 1;
        }
        offset += shown.len();
    }
    offset
}

/// Whether `line` holds the END line of a private key.
fn closes_private_key(line: &str) -> bool {
    line.contains("-----END ") && line.contains("PRIVATE KEY")
}

/// How many lines to request first for a tail of `lines`: the look-behind, plus one to learn
/// whether older lines exist.
pub fn log_fetch_tail(lines: u32) -> i64 {
    i64::from(lines) + i64::from(LOG_LOOK_BEHIND) + 1
}

/// The next, smaller request after a response hit the byte limit with `complete` whole lines.
/// The look-behind goes first, so it never costs requested lines.
pub fn next_log_tail(tail: i64, lines: u32, complete: i64) -> i64 {
    let wanted = i64::from(lines) + 1;
    if tail > wanted {
        wanted
    } else {
        complete.clamp(1, tail / 2)
    }
}

/// Where `text` stops continuing a block whose start was not fetched: 0 when it does not begin
/// with one.
fn leading_cut_off_lines(text: &str) -> usize {
    leading_key_lines(text).max(leading_cookie_items(text))
}

/// The end of the run of key-shaped lines at the start of `text`, optionally after a log prefix.
/// The run counts when an END line follows it, or when it has the shape of a PEM body
/// ([`PemBody`]).
fn leading_key_lines(text: &str) -> usize {
    let mut end = 0;
    let mut body = PemBody::default();
    for line in text.split_inclusive('\n') {
        let Some(token) = key_token(line) else {
            let closes = closes_private_key(line);
            return if body.is_complete() || (end > 0 && closes) {
                end
            } else {
                0
            };
        };
        body.push(line, token);
        end += line.len();
    }
    if body.is_complete() { end } else { 0 }
}

/// Line widths PEM encoders wrap base64 at.
const PEM_WIDTHS: [usize; 2] = [64, 76];
/// Full-width lines a run needs to count as a PEM body when no END line is in reach.
const MIN_PEM_LINES: usize = 2;

/// Tracks whether a run of lines reads as a PEM body: full lines of one standard width with a
/// mix of base64 characters, then at most one shorter line, each alone after a log prefix.
#[derive(Default)]
struct PemBody {
    width: usize,
    full_lines: usize,
    ended: bool,
    broken: bool,
}

impl PemBody {
    fn push(&mut self, line: &str, token: &str) {
        let width = token.len();
        let padding_inside = token.trim_end_matches('=').contains('=');
        if self.broken || self.ended || padding_inside || !is_log_prefix(line) {
            self.broken = true;
        } else if self.width == 0 {
            self.width = width;
            self.broken = !PEM_WIDTHS.contains(&width) || !mixed_base64(token);
            self.full_lines = 1;
        } else if width == self.width {
            self.broken = !mixed_base64(token);
            self.full_lines += 1;
        } else if width < self.width {
            self.ended = true;
        } else {
            self.broken = true;
        }
    }

    fn is_complete(&self) -> bool {
        !self.broken && self.full_lines >= MIN_PEM_LINES
    }
}

/// Whether `token` has upper case, lower case and digits, as base64 of random bytes does.
fn mixed_base64(token: &str) -> bool {
    token.bytes().any(|b| b.is_ascii_uppercase())
        && token.bytes().any(|b| b.is_ascii_lowercase())
        && token.bytes().any(|b| b.is_ascii_digit())
}

/// Whether what comes before the last word of `line` is nothing, a bare timestamp, or a prefix
/// closed by `|`, `]` or `>`, rather than a message the word belongs to.
fn is_log_prefix(line: &str) -> bool {
    let trimmed = line.trim_end();
    let word = trimmed.split_whitespace().next_back().unwrap_or("");
    let prefix = trimmed[..trimmed.len() - word.len()].trim_end();
    prefix.is_empty()
        || prefix.ends_with(['|', ']', '>'])
        || prefix.bytes().all(|b| {
            b.is_ascii_digit() || matches!(b, b'-' | b':' | b'.' | b'+' | b'T' | b'Z' | b' ')
        })
}

/// The last word of `line` when it could be a line of base64 key material.
fn key_token(line: &str) -> Option<&str> {
    let word = line.split_whitespace().next_back()?;
    let word = word.trim_end_matches([',', '"', '\'']);
    let word = word.strip_suffix("\\n").unwrap_or(word);
    (word.len() >= 16
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')))
    .then_some(word)
}

/// The end of the run of quoted `name=value` items at the start of `text` when it runs to a
/// closing `]` or to the end of the text: a JSON array whose opening line was not fetched.
fn leading_cookie_items(text: &str) -> usize {
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        let trimmed = trimmed.strip_suffix(',').unwrap_or(trimmed);
        let (body, closes) = match trimmed.strip_suffix(']') {
            Some(body) => (body.trim_end(), true),
            None => (trimmed, false),
        };
        let body = body.strip_suffix(',').unwrap_or(body);
        let item = body.find('"').map(|at| &body[at..]);
        if item.is_some_and(|i| i.len() > 2 && i.ends_with('"') && i.contains('=')) {
            end += line.len();
            if closes {
                return end;
            }
            continue;
        }
        return if closes { end } else { 0 };
    }
    end
}

/// Keeps the last `n` lines of `text`. Returns whether lines were dropped.
pub fn tail_lines(text: &str, n: usize) -> (&str, bool) {
    let total = text.lines().count();
    if total <= n {
        return (text, false);
    }
    let skip = total - n;
    let start = text
        .match_indices('\n')
        .nth(skip - 1)
        .map_or(0, |(i, _)| i + 1);
    (&text[start..], true)
}

#[cfg(test)]
#[path = "policy_masking_tests.rs"]
mod masking_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use flint_core::settings::AiExposure;
    use serde_json::json;
    use std::path::PathBuf;

    /// Contexts named `names`, each pointing at `https://<name>`.
    fn kubeconfig(names: &[&str]) -> LoadedKubeconfig {
        LoadedKubeconfig {
            contexts: names
                .iter()
                .map(|n| ContextInfo {
                    name: (*n).into(),
                    cluster: None,
                    user: None,
                    namespace: None,
                    server: Some(server(n)),
                    proxy_url: None,
                    source: PathBuf::from("/k"),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn server(name: &str) -> String {
        format!("https://{name}")
    }

    /// Exposes each name at the server [`kubeconfig`] gives it.
    fn settings(exposed: &[&str]) -> Settings {
        Settings {
            ai_exposed_clusters: exposed
                .iter()
                .map(|n| AiExposure::new(*n, server(n)))
                .collect(),
            ..Default::default()
        }
    }

    fn with_metadata(kind: &str, labels: Value, annotations: Value) -> Value {
        serde_json::json!({"apiVersion": "v1", "kind": kind, "metadata": {
            "name": "web", "namespace": "demo", "labels": labels, "annotations": annotations,
        }})
    }

    // covers UP-10: the value is the cluster's, so it cannot add a line, run long or carry a
    // credential.
    #[test]
    fn managed_by_line_shows_only_a_plain_name() {
        let key = "argocd.argoproj.io/instance";
        let line = |value: &str| {
            managed_by_line(&with_metadata(
                "ConfigMap",
                serde_json::json!({}),
                serde_json::json!({key: value}),
            ))
        };
        assert_eq!(
            line("shop").as_deref(),
            Some("# managed_by=Argo CD: shop\n")
        );
        for forged in [
            "shop\n# truncated=true",
            "shop\r\nkind: Forged",
            "shop\u{2028}x",
            "shop\u{7}",
            "shop\u{1b}[2J",
            "shop x",
        ] {
            assert_eq!(line(forged), None, "{forged:?}");
        }
        assert!(line(&"a".repeat(253)).is_some());
        assert_eq!(line(&"a".repeat(254)), None, "over the length cap");
        assert_eq!(line("AKIAIOSFODNN7EXAMPLE"), None, "a credential");
    }

    // covers UP-10: a Secret's annotations are masked first, so they name nothing.
    #[test]
    fn managed_by_line_follows_the_secret_rules() {
        let mut secret = with_metadata(
            "Secret",
            serde_json::json!({}),
            serde_json::json!({"argocd.argoproj.io/tracking-id": "shop:/Secret:demo/web"}),
        );
        sanitize_secret(&mut secret);
        assert_eq!(managed_by_line(&secret), None);

        // The masked id says nothing, so the common label beside it does not name an app.
        let mut labelled = with_metadata(
            "Secret",
            serde_json::json!({"app.kubernetes.io/instance": "shop"}),
            serde_json::json!({"argocd.argoproj.io/tracking-id": "shop:/Secret:other/web"}),
        );
        sanitize_secret(&mut labelled);
        assert_eq!(managed_by_line(&labelled), None);
    }

    // covers UP-10: read before masking, a Secret's own tracking id gives the tool only, and the
    // output holds no annotation value.
    #[test]
    fn managed_by_secret_line_shows_only_the_tool_of_a_tracked_secret() {
        let id = "plain-value:/Secret:demo/web";
        let mut secret = serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {
            "name": "web", "namespace": "demo",
            "annotations": {"argocd.argoproj.io/tracking-id": id, "note": "hunter2"},
        }});
        let line = managed_by_secret_line(&secret);
        assert_eq!(line.as_deref(), Some("# managed_by=Argo CD\n"));
        sanitize_secret(&mut secret);
        let masked = secret.to_string();
        assert!(
            !masked.contains(id) && !masked.contains("hunter2"),
            "{masked}"
        );
    }

    // covers UP-09, UP-11
    #[test]
    fn only_exposed_clusters_are_visible() {
        let kc = kubeconfig(&["dev", "prod", "staging"]);
        let s = settings(&["dev", "staging", "ghost"]);
        let names: Vec<_> = exposed_clusters(&s, &kc)
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["dev", "staging"]);
        assert!(exposed_cluster(&s, &kc, "dev").is_ok());
    }

    // covers UP-08, UP-11
    #[test]
    fn same_name_with_a_different_server_is_not_exposed() {
        let kc = kubeconfig(&["dev", "prod"]);
        let s = Settings {
            ai_exposed_clusters: vec![AiExposure::new("dev", "https://old-dev")],
            ..Default::default()
        };
        assert!(exposed_clusters(&s, &kc).is_empty());
        let changed = exposed_cluster(&s, &kc, "dev").unwrap_err().to_string();
        let unknown = exposed_cluster(&s, &kc, "nope").unwrap_err().to_string();
        assert_eq!(changed.replace("dev", "X"), unknown.replace("nope", "X"));
        assert_eq!(changed_exposures(&s, &kc), 1);
        let hint = list_clusters_hint(0, 1, 0, &[], false, &app()).unwrap();
        assert!(hint.contains("Expose to AI") && hint.contains("different API server"));
    }

    // covers UP-09
    #[test]
    fn hint_only_when_useful() {
        assert!(
            list_clusters_hint(0, 0, 0, &[], false, &app())
                .unwrap()
                .contains("Expose to AI")
        );
        assert_eq!(list_clusters_hint(2, 0, 0, &[], false, &app()), None);
        assert!(
            list_clusters_hint(1, 2, 0, &[], false, &app())
                .unwrap()
                .starts_with("2 clusters")
        );
    }

    // covers UP-09
    #[test]
    fn an_exposed_context_missing_from_the_kubeconfig_is_counted_once() {
        let kc = kubeconfig(&["dev"]);
        let mut s = settings(&["dev", "ghost", "other"]);
        s.ai_exposed_clusters
            .push(AiExposure::new("ghost", "https://moved-ghost"));
        assert_eq!(missing_exposures(&s, &kc), 2);
        // A loaded context on another server is "changed", not missing.
        let moved = Settings {
            ai_exposed_clusters: vec![AiExposure::new("dev", "https://old-dev")],
            ..Default::default()
        };
        assert_eq!(missing_exposures(&moved, &kc), 0);
    }

    // covers UP-09
    #[test]
    fn missing_hint_lists_the_files_read_and_never_the_context() {
        let paths = [PathBuf::from("/a/config"), PathBuf::from("/b/config")];
        let hint = list_clusters_hint(0, 0, 1, &paths, false, &app()).unwrap();
        assert!(hint.starts_with("One cluster"), "{hint}");
        assert!(hint.contains("/a/config, /b/config"), "{hint}");
        assert!(hint.contains("kubeconfig_paths"), "{hint}");
        // It is already exposed, so the advice to turn that on would be wrong.
        assert!(!hint.contains("turn on"), "{hint}");

        let both = list_clusters_hint(1, 1, 2, &paths, false, &app()).unwrap();
        assert!(both.contains("different API server"), "{both}");
        assert!(
            both.contains("2 clusters the user exposed are not"),
            "{both}"
        );
    }

    // covers UP-09
    #[test]
    fn missing_hint_points_at_files_that_failed_to_load() {
        let paths = [PathBuf::from("/a/config")];
        let hint = list_clusters_hint(0, 0, 1, &paths, true, &app()).unwrap();
        assert!(hint.contains("could not be loaded"), "{hint}");
        assert!(hint.contains("kubeconfig_errors"), "{hint}");
        let clean = list_clusters_hint(0, 0, 1, &paths, false, &app()).unwrap();
        assert!(!clean.contains("kubeconfig_errors"), "{clean}");
    }

    // covers UP-09
    #[test]
    fn missing_hint_redacts_credentials_in_paths_and_keeps_the_advice() {
        let paths = [
            PathBuf::from("/home/me/password=hunter2"),
            PathBuf::from("/b/config"),
        ];
        let hint = list_clusters_hint(0, 0, 1, &paths, false, &app()).unwrap();
        assert!(!hint.contains("hunter2"), "{hint}");
        assert!(hint.contains("/b/config"), "{hint}");
        assert!(hint.contains("kubeconfig_paths"), "{hint}");
    }

    // covers UP-09
    #[test]
    fn nothing_is_exposed_by_default() {
        let kc = kubeconfig(&["dev", "prod"]);
        assert!(exposed_clusters(&Settings::default(), &kc).is_empty());
    }

    // covers UP-11
    #[test]
    fn unexposed_and_unknown_clusters_give_identical_errors() {
        let kc = kubeconfig(&["dev", "prod"]);
        let s = settings(&["dev"]);
        let unexposed = exposed_cluster(&s, &kc, "prod").unwrap_err().to_string();
        let unknown = exposed_cluster(&s, &kc, "nope").unwrap_err().to_string();
        assert_eq!(unexposed.replace("prod", "X"), unknown.replace("nope", "X"));
        // Exposed in settings but not in kubeconfig is also just "not found".
        let s = settings(&["ghost"]);
        assert_eq!(
            exposed_cluster(&s, &kc, "ghost").unwrap_err().to_string(),
            unknown.replace("nope", "ghost")
        );
    }

    fn kind(group: &str, version: &str, kind: &str, plural: &str, namespaced: bool) -> KindInfo {
        let api_version = if group.is_empty() {
            version.to_owned()
        } else {
            format!("{group}/{version}")
        };
        KindInfo {
            resource: kube::discovery::ApiResource {
                group: group.into(),
                version: version.into(),
                api_version,
                kind: kind.into(),
                plural: plural.into(),
            },
            namespaced,
            verbs: ["get", "list"].map(String::from).to_vec(),
            singular: kind.to_lowercase(),
            short_names: Vec::new(),
        }
    }

    fn scoped(namespaces: &[&str], kinds: &[&str]) -> AiScope {
        AiScope::new(
            namespaces.iter().map(|s| (*s).to_owned()).collect(),
            kinds.iter().map(|s| (*s).to_owned()).collect(),
        )
    }

    fn app() -> Setup {
        Setup::new(Flavour::App, Path::new("/config"))
    }

    // covers UP-09: each flavour gets advice it can follow where it runs.
    #[test]
    fn setup_advice_follows_the_flavour() {
        assert_eq!(Flavour::of_program("flint"), Flavour::App);
        assert_eq!(Flavour::of_program("flint-mcp"), Flavour::Headless);
        let headless = Setup::new(Flavour::Headless, Path::new("/config"));
        for (exposed, changed) in [(0, 0), (0, 1), (1, 2)] {
            let hint = list_clusters_hint(exposed, changed, 0, &[], false, &headless).unwrap();
            assert!(hint.contains("[[ai_exposed_clusters]]"), "{hint}");
            assert!(hint.contains("/config/settings.toml"), "{hint}");
            assert!(!hint.contains("robot"), "{hint}");
            assert!(!hint.contains("in Flint."), "{hint}");
        }
        // Adding an entry beside a re-pointed one would drop its scope: only updating is advised.
        let repointed = list_clusters_hint(0, 1, 0, &[], false, &headless).unwrap();
        assert!(repointed.contains("update the server"), "{repointed}");
        assert!(!repointed.contains("add an"), "{repointed}");
        // A file may already hold the inline form, where a table would be a second definition.
        let first = list_clusters_hint(0, 0, 0, &[], false, &headless).unwrap();
        assert!(first.contains("ai_exposed_clusters = [...]"), "{first}");
        let hint = list_clusters_hint(0, 1, 0, &[], false, &app()).unwrap();
        assert!(
            hint.contains("robot button") && hint.contains("again in Flint."),
            "{hint}"
        );
        assert!(!hint.contains("[[ai_exposed_clusters]]"), "{hint}");
        assert!(headless.fix_settings().contains("fix that file"));
        assert!(app().fix_settings().contains("Flint app"));
    }

    // covers UP-15: a credential in a kubeconfig path or its error does not reach the agent, and
    // the other field survives it.
    #[test]
    fn kubeconfig_errors_are_redacted_field_by_field() {
        let row = kubeconfig_error_row(
            Path::new("/home/me/password=hunter2/config"),
            "could not read it: token=example-credential",
        );
        let text = row.to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("example-credential"), "{text}");
        assert!(
            row["file"].as_str().unwrap().starts_with("/home/me/"),
            "{row}"
        );
        assert!(
            row["error"]
                .as_str()
                .unwrap()
                .starts_with("could not read it"),
            "{row}"
        );

        // Spaces are valid in a path: the whole component goes, and the rest stays.
        let passphrase = kubeconfig_error_row(
            Path::new("/home/me/password=correct horse battery/config"),
            "bad yaml",
        );
        assert_eq!(
            passphrase["file"], "/home/me/[REDACTED]/config",
            "{passphrase}"
        );

        // A URL whose password starts before the summary's cut and ends after it.
        let long = format!(
            "bad url https://user:{}@example.com/ in {}",
            "p".repeat(300),
            "x".repeat(10)
        );
        let cut = kubeconfig_error_row(Path::new("/home/me/config"), &long);
        assert!(!cut["error"].as_str().unwrap().contains("ppp"), "{cut}");

        let plain = kubeconfig_error_row(Path::new("/home/me/config"), "bad yaml");
        assert_eq!(
            plain,
            json!({"file": "/home/me/config", "error": "bad yaml"})
        );
    }

    // covers UP-09: the settings path is cluster-independent but still redacted.
    #[test]
    fn the_settings_path_in_advice_is_redacted() {
        let setup = Setup::new(Flavour::Headless, Path::new("/home/me/password=hunter2"));
        let hint = list_clusters_hint(0, 0, 0, &[], false, &setup).unwrap();
        assert!(!hint.contains("hunter2"), "{hint}");
    }

    // covers UP-14: a hand-written name resolves as it does for the tools.
    #[test]
    fn scope_kinds_resolve_like_the_tools() {
        let mut deployments = kind("apps", "v1", "Deployment", "deployments", true);
        deployments.short_names = vec!["deploy".into()];
        let kinds = [
            kind("", "v1", "Pod", "pods", true),
            deployments,
            kind("", "v1", "Node", "nodes", false),
        ];
        let resolved = resolve_scope_kinds(
            &scoped(&["demo"], &["pod", "Deployment", "node"]),
            &kinds,
            &[],
        );
        assert_eq!(resolved.unmatched, Vec::<String>::new());
        assert_eq!(resolved.scope.kinds, ["deployments.apps", "nodes", "pods"]);
        let ids = |scope: &AiScope| -> Vec<String> {
            kinds_in_scope(scope, &kinds)
                .iter()
                .map(|k| k.id().as_str().to_owned())
                .collect()
        };
        assert_eq!(ids(&resolved.scope).len(), 3, "nodes were named");
        let short = resolve_scope_kinds(&scoped(&[], &["deploy"]), &kinds, &[]);
        assert_eq!(ids(&short.scope), ["deployments.apps"]);
    }

    // covers UP-14: a name that matches nothing, or several kinds, grants nothing, and a scope
    // of only such names never turns into the empty list that means everything.
    #[test]
    fn unmatched_scope_kinds_fail_closed() {
        let kinds = [
            kind("", "v1", "Pod", "pods", true),
            kind("a.example.com", "v1", "Widget", "widgets", true),
            kind("b.example.com", "v1", "Widget", "widgets", true),
        ];
        let resolved = resolve_scope_kinds(&scoped(&[], &["nope", "widgets"]), &kinds, &[]);
        assert_eq!(resolved.unmatched, ["nope", "widgets"]);
        assert!(!resolved.scope.is_everything());
        assert!(kinds_in_scope(&resolved.scope, &kinds).is_empty());
    }

    // covers UP-14: a saved `plural.group` id whose kind is gone is not read as
    // `plural.version.group`, which would grant another group's kind.
    #[test]
    fn a_saved_kind_id_stays_bound_to_its_group() {
        let kinds = [kind("example.com", "v1", "Widget", "widgets", true)];
        let saved = scoped(&[], &["widgets.v1.example.com"]);
        let resolved = resolve_scope_kinds(&saved, &kinds, &[]);
        assert_eq!(resolved.unmatched, ["widgets.v1.example.com"]);
        assert!(kinds_in_scope(&resolved.scope, &kinds).is_empty());
        let qualified = resolve_scope_kinds(&scoped(&[], &["widget.example.com"]), &kinds, &[]);
        assert_eq!(qualified.scope.kinds, ["widgets.example.com"]);
    }

    // covers UP-14: with a group missing from discovery, a bare name could mean a kind there, so
    // it grants nothing; an exact id and a name in another group still resolve.
    #[test]
    fn partial_discovery_grants_no_bare_name() {
        let kinds = [
            kind("", "v1", "Pod", "pods", true),
            kind("a.example.com", "v1", "Widget", "widgets", true),
        ];
        let skipped = ["b.example.com".to_owned()];
        let bare = resolve_scope_kinds(&scoped(&[], &["widgets"]), &kinds, &skipped);
        assert_eq!(bare.unmatched, ["widgets"]);
        assert!(kinds_in_scope(&bare.scope, &kinds).is_empty());
        let exact = resolve_scope_kinds(&scoped(&[], &["pods"]), &kinds, &skipped);
        assert_eq!(exact.unmatched, Vec::<String>::new());
        let other = resolve_scope_kinds(&scoped(&[], &["widget.a.example.com"]), &kinds, &skipped);
        assert_eq!(other.scope.kinds, ["widgets.a.example.com"]);
        let missing =
            resolve_scope_kinds(&scoped(&[], &["widget.b.example.com"]), &kinds, &skipped);
        assert_eq!(missing.unmatched, ["widget.b.example.com"]);
    }

    // covers UP-14: incomplete discovery is said only for a name it left unresolved: an exact
    // kind id still works.
    #[test]
    fn resolving_says_when_discovery_left_a_name_unresolved() {
        let kinds = [kind("", "v1", "Pod", "pods", true)];
        let stale = ["metrics.k8s.io".to_owned()];
        assert!(resolve_scope_kinds(&scoped(&[], &["po"]), &kinds, &stale).discovery_incomplete);
        let exact = resolve_scope_kinds(&scoped(&[], &["pods"]), &kinds, &stale);
        assert!(exact.unmatched.is_empty());
        assert!(!exact.discovery_incomplete);
        assert!(!resolve_scope_kinds(&scoped(&["a"], &[]), &kinds, &stale).discovery_incomplete);
        assert!(!resolve_scope_kinds(&scoped(&[], &["nope"]), &kinds, &[]).discovery_incomplete);
    }

    // covers UP-14: what a scope check could not cover is said, and no API group is named.
    #[test]
    fn a_scope_check_says_what_it_could_not_cover() {
        assert_eq!(ScopeCheck::default().incomplete(), None);
        let unmatched = ScopeCheck {
            unmatched_kinds: vec!["nope".into()],
            unmatched_namespaces: vec!["gone".into()],
            ..ScopeCheck::default()
        };
        assert_eq!(unmatched.incomplete(), None, "complete, with findings");

        let partial = ScopeCheck {
            unchecked_namespaces: 6,
            ..ScopeCheck::default()
        };
        assert!(
            partial
                .incomplete()
                .unwrap()
                .starts_with("6 of the scope's")
        );

        let both = ScopeCheck {
            unchecked_namespaces: 1,
            discovery_incomplete: true,
            ..ScopeCheck::default()
        };
        let note = both.incomplete().unwrap();
        assert!(note.contains("could not be checked"), "{note}");
        assert!(note.contains("API discovery is failing"), "{note}");

        let scope = scoped(&["a", "b"], &[]);
        let not_run = ScopeCheck::not_run(&scope);
        assert_eq!(not_run.unchecked_namespaces, 2);
        assert!(not_run.incomplete().unwrap().contains("enforced"));
    }

    // covers UP-14: the scope comes from the cluster's entry; one without a scope is whole.
    #[test]
    fn a_clusters_scope_comes_from_its_entry() {
        let kc = kubeconfig(&["dev", "prod"]);
        let mut s = settings(&["dev", "prod"]);
        s.ai_exposed_clusters[1].namespaces = vec!["shop".into()];
        let dev = exposed_cluster(&s, &kc, "dev").unwrap();
        let prod = exposed_cluster(&s, &kc, "prod").unwrap();
        assert!(cluster_scope(&s, dev).is_everything());
        assert_eq!(cluster_scope(&s, prod).namespaces, ["shop"]);
    }

    // covers UP-14: out of scope says so, names only the cluster, and stays distinct from a
    // cluster that is not exposed.
    #[test]
    fn out_of_scope_names_only_the_cluster() {
        let kc = kubeconfig(&["dev", "prod"]);
        let s = settings(&["dev"]);
        let unexposed = exposed_cluster(&s, &kc, "prod").unwrap_err();
        assert!(unexposed.to_string().contains("not found"));
        let text = out_of_scope("prod").to_string();
        assert!(text.contains("outside the scope"), "{text}");
        assert!(!text.contains("not found"), "{text}");
        let scope = scoped(&["shop"], &[]);
        for namespace in ["kube-system", "no-such-namespace"] {
            let error = namespaces_for(&scope, "prod", true, Some(namespace)).unwrap_err();
            assert_eq!(error, out_of_scope("prod"));
            assert!(!error.to_string().contains(namespace));
        }
    }

    // covers UP-14
    #[test]
    fn namespaces_follow_the_scope() {
        let everything = AiScope::default();
        assert_eq!(
            namespaces_for(&everything, "c", true, None),
            Ok(Namespaces::All)
        );
        assert_eq!(
            namespaces_for(&everything, "c", true, Some("a")),
            Ok(Namespaces::One("a".into()))
        );
        let scope = scoped(&["shop", "billing"], &[]);
        assert_eq!(
            namespaces_for(&scope, "c", true, None),
            Ok(Namespaces::Only(vec!["billing".into(), "shop".into()]))
        );
        assert_eq!(
            namespaces_for(&scope, "c", true, Some("shop")),
            Ok(Namespaces::One("shop".into()))
        );
        assert!(namespaces_for(&scope, "c", true, Some("other")).is_err());
        // A cluster-scoped kind (already allowed by name) has no namespace to check.
        assert_eq!(
            namespaces_for(&scope, "c", false, Some("x")),
            Ok(Namespaces::All)
        );
    }

    // covers UP-14: kinds outside the scope are hidden; under a namespace scope, cluster-scoped
    // kinds too unless named.
    #[test]
    fn kinds_follow_the_scope() {
        let kinds = [
            kind("", "v1", "Pod", "pods", true),
            kind("", "v1", "Secret", "secrets", true),
            kind("", "v1", "Node", "nodes", false),
            kind("apps", "v1", "Deployment", "deployments", true),
        ];
        let names = |scope: &AiScope| -> Vec<String> {
            kinds_in_scope(scope, &kinds)
                .iter()
                .map(|k| k.id().as_str().to_owned())
                .collect()
        };
        assert_eq!(names(&AiScope::default()).len(), 4);
        assert_eq!(
            names(&scoped(&["shop"], &[])),
            ["pods", "secrets", "deployments.apps"]
        );
        assert_eq!(names(&scoped(&[], &["pods", "nodes"])), ["pods", "nodes"]);
        assert_eq!(
            names(&scoped(&["shop"], &["deployments.apps", "nodes"])),
            ["nodes", "deployments.apps"]
        );
        // On a scoped cluster events are read through get_events only, even when named.
        let with_events = [
            kind("", "v1", "Pod", "pods", true),
            kind("", "v1", "Event", "events", true),
            kind("events.k8s.io", "v1", "Event", "events", true),
        ];
        let ids = |scope: &AiScope| -> Vec<String> {
            kinds_in_scope(scope, &with_events)
                .iter()
                .map(|k| k.id().as_str().to_owned())
                .collect()
        };
        assert_eq!(ids(&AiScope::default()).len(), 3);
        assert_eq!(ids(&scoped(&["default"], &[])), ["pods"]);
        assert_eq!(
            ids(&scoped(&[], &["events", "events.events.k8s.io"])),
            Vec::<String>::new()
        );
        assert!(logs_in_scope(&scoped(&["shop"], &[])));
        assert!(logs_in_scope(&scoped(&[], &["pods"])));
        assert!(!logs_in_scope(&scoped(&[], &["deployments.apps"])));
    }

    // covers UP-14: events follow the namespace scope, and the kinds scope through the object
    // they are about.
    #[test]
    fn events_follow_the_scope() {
        let kinds = [
            kind("", "v1", "Pod", "pods", true),
            kind("", "v1", "Secret", "secrets", true),
            kind("", "v1", "Node", "nodes", false),
            kind("apps", "v1", "Deployment", "deployments", true),
        ];
        let everything = AiScope::default();
        assert!(event_in_scope(
            &everything,
            &kinds,
            None,
            Some("Whatever"),
            None
        ));
        let ns = scoped(&["shop"], &[]);
        assert!(event_in_scope(
            &ns,
            &kinds,
            Some("shop"),
            Some("Pod"),
            Some("v1")
        ));
        assert!(!event_in_scope(
            &ns,
            &kinds,
            Some("kube-system"),
            Some("Pod"),
            Some("v1")
        ));
        // Node events land in "default": the node is a cluster-scoped kind, hidden here.
        let default = scoped(&["default"], &[]);
        assert!(!event_in_scope(
            &default,
            &kinds,
            Some("default"),
            Some("Node"),
            Some("v1")
        ));
        let pods = scoped(&[], &["pods"]);
        assert!(event_in_scope(
            &pods,
            &kinds,
            Some("a"),
            Some("Pod"),
            Some("v1")
        ));
        assert!(!event_in_scope(
            &pods,
            &kinds,
            Some("a"),
            Some("Secret"),
            Some("v1")
        ));
        let apps = scoped(&[], &["deployments.apps"]);
        assert!(event_in_scope(
            &apps,
            &kinds,
            Some("a"),
            Some("Deployment"),
            Some("apps/v1")
        ));
        assert!(!event_in_scope(
            &apps,
            &kinds,
            Some("a"),
            Some("Deployment"),
            Some("v1")
        ));
        assert!(kind_name_in_scope(&pods, &kinds, "Pod"));
        assert!(!kind_name_in_scope(&pods, &kinds, "Secret"));
        assert!(!kind_name_in_scope(&ns, &kinds, "Node"));
        assert!(!kind_name_in_scope(&ns, &kinds, "Gizmo"));
        // A kind discovery does not list is left out of a scoped cluster's events.
        assert!(!event_in_scope(
            &ns,
            &kinds,
            Some("shop"),
            Some("Gizmo"),
            Some("x.io/v1")
        ));
    }

    // covers UP-10
    #[test]
    fn secrets_are_redacted_keeping_keys() {
        let mut secret = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": "s",
                "managedFields": [{"manager": "kubectl"}],
                "annotations": {
                    LAST_APPLIED: "{\"data\":{\"password\":\"c2VjcmV0\"}}",
                    "kapp.k14s.io/original": "{\"data\":{\"password\":\"c2VjcmV0\"}}",
                    "meta.helm.sh/release-name": "kept"
                }
            },
            "data": {"password": "c2VjcmV0", "user": "YWRtaW4="},
            "stringData": {"token": "plain"},
            "type": "Opaque"
        });
        sanitize(&mut secret);
        assert_eq!(
            secret["data"],
            json!({"password": REDACTED, "user": REDACTED})
        );
        assert_eq!(secret["stringData"], json!({"token": REDACTED}));
        assert_eq!(secret["metadata"]["annotations"][LAST_APPLIED], REDACTED);
        assert_eq!(
            secret["metadata"]["annotations"]["kapp.k14s.io/original"],
            REDACTED
        );
        assert_eq!(
            secret["metadata"]["annotations"]["meta.helm.sh/release-name"],
            "kept"
        );
        assert!(secret["metadata"].get("managedFields").is_none());
        assert_eq!(secret["type"], "Opaque");
        let text = secret.to_string();
        assert!(!text.contains("c2VjcmV0") && !text.contains("plain"));
    }

    // covers UP-10
    #[test]
    fn non_secrets_keep_data_but_lose_managed_fields() {
        let mut cm = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "c", "managedFields": [{}]},
            "data": {"k": "v"}
        });
        sanitize(&mut cm);
        assert_eq!(cm["data"]["k"], "v");
        assert!(cm["metadata"].get("managedFields").is_none());
    }

    // covers UP-15
    #[test]
    fn messages_are_redacted_at_any_depth() {
        let mut list = json!({
            "kind": "List",
            "items": [
                {
                    "apiVersion": "v1",
                    "kind": "Event",
                    "metadata": {"name": "e", "annotations": {"message": "password=kept-in-metadata"}},
                    "reason": "Failed",
                    "message": "Failed to pull: Authorization: Basic dXNlcjpwYXNz"
                },
                {"apiVersion": "events.k8s.io/v1", "kind": "Event", "note": "token=abc123"},
                {"kind": "Machine", "status": {"failureMessage": "bootstrap failed: password=pw1"}},
                {
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "status": {"containerStatuses": [{"lastState": {"terminated": {
                        "message": "panic: connect postgres://app:hunter2@db:5432/app"
                    }}}]}
                }
            ]
        });
        assert_eq!(sanitize(&mut list), 5);
        let text = list.to_string();
        for leaked in [
            "dXNlcjpwYXNz",
            "abc123",
            "hunter2",
            "kept-in-metadata",
            "pw1",
        ] {
            assert!(!text.contains(leaked), "{leaked}: {text}");
        }
        assert_eq!(
            list["items"][0]["message"],
            "Failed to pull: Authorization: Basic [REDACTED:authorization]"
        );
        assert_eq!(list["items"][0]["reason"], "Failed");
        assert_eq!(list["items"][1]["note"], "token=[REDACTED:token]");

        let mut plain = json!({"kind": "ConfigMap", "data": {"message": "hello"}});
        assert_eq!(sanitize(&mut plain), 0);
        assert_eq!(plain["data"]["message"], "hello");
    }

    // covers UP-10
    #[test]
    fn lists_are_sanitized_item_by_item() {
        let mut list = json!({
            "kind": "List",
            "items": [
                {"apiVersion": "v1", "kind": "Secret", "metadata": {"managedFields": []}, "data": {"a": "b"}},
                {"apiVersion": "v1", "kind": "Pod", "metadata": {"managedFields": []}}
            ]
        });
        sanitize(&mut list);
        assert_eq!(list["items"][0]["data"]["a"], REDACTED);
        assert!(list["items"][1]["metadata"].get("managedFields").is_none());
    }

    // covers UP-10: a wrong-type error names what was expected and repeats no value, even one
    // holding the quotes a mask would rely on.
    #[test]
    fn argument_errors_repeat_no_value() {
        let error = |rest: &str| format!("{ARGUMENT_ERROR}{rest}");
        for (rest, expected) in [
            (r#"invalid type: string "s3cret", expected u32"#, "u32"),
            (r#"invalid type: string "a"s3cret"b", expected u32"#, "u32"),
            ("invalid value: integer `-7`, expected u32", "u32"),
            (
                r#"invalid type: string "x, expected s3cret", expected a boolean"#,
                "a boolean",
            ),
        ] {
            let safe = argument_error(&error(rest)).unwrap();
            assert_eq!(
                safe,
                error(&format!("wrong type or value, expected {expected}")),
                "{rest}"
            );
        }
        assert_eq!(
            argument_error(&error("invalid s3cret")).unwrap(),
            error("wrong type or value")
        );
        // Names a field of the schema, which the agent needs; and other text is not this error.
        assert_eq!(argument_error(&error("missing field `kind`")), None);
        assert_eq!(argument_error("NotFound: pods \"x\" not found"), None);
    }

    // covers UP-02, UP-09
    #[test]
    fn quoted_values_are_redacted() {
        assert_eq!(
            redact_quoted(r#"invalid type: string "s3cret", expected a boolean"#),
            r#"invalid type: string "…", expected a boolean"#
        );
        assert_eq!(redact_quoted("no quotes"), "no quotes");
        assert_eq!(
            redact_quoted("unknown variant `Sometimes`"),
            "unknown variant `…`"
        );
    }

    // covers UP-10
    #[test]
    fn bounds_clamp() {
        assert_eq!(OBJECTS.clamp(None), 100);
        assert_eq!(OBJECTS.clamp(Some(0)), 100);
        assert_eq!(OBJECTS.clamp(Some(5)), 5);
        assert_eq!(OBJECTS.clamp(Some(10_000)), 200);
        assert_eq!(LOG_LINES.clamp(Some(501)), 500);
    }

    // covers UP-10
    #[test]
    fn tail_lines_keeps_newest() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), ("b\nc\n", true));
        assert_eq!(tail_lines("a\nb\nc", 3), ("a\nb\nc", false));
        assert_eq!(tail_lines("a\nb\nc", 1), ("c", true));
        assert_eq!(tail_lines("", 5), ("", false));
    }

    // covers UP-10
    #[test]
    fn byte_truncation_respects_lines_and_chars() {
        assert_eq!(truncate_bytes("ab\ncd\nef", 7), ("ab\ncd\n", true));
        assert_eq!(truncate_bytes("short", 100), ("short", false));
        // "é" is two bytes; never split it.
        assert_eq!(truncate_bytes("éé", 3), ("é", true));
        assert_eq!(tail_bytes("ab\ncd\nef", 5), ("cd\nef", true));
        assert_eq!(tail_bytes("ab\ncd\nef", 4), ("ef", true));
        assert_eq!(tail_bytes("ab\ncd\nef\n", 7), ("cd\nef\n", true));
    }

    // covers UP-15
    #[test]
    fn a_line_cut_by_the_byte_cap_is_dropped_whole() {
        // The cap would keep "hunter2" without its label; the partial line goes.
        let log = "ok\nconnecting with password=hunter2";
        assert_eq!(tail_bytes(log, 10), ("", true));
        assert_eq!(tail_bytes("password=hunter2\nnext\n", 9), ("next\n", true));
        // One line longer than the cap: nothing of it is kept, not even after redaction.
        let long = format!("{} password=hunter2 {}", "a".repeat(64), "b".repeat(64));
        let out = log_output(&long, 100, 32, false);
        assert_eq!(
            (out.text.as_str(), out.truncated, out.redacted),
            ("", true, 0)
        );
        assert_eq!(
            out.note.as_deref(),
            Some("the last line is longer than the 32 bytes limit and was left out")
        );
        let out = log_output(&long, 100, 64 * 1024, false);
        assert_eq!((out.redacted, out.note), (1, None));
        let out = log_output(&format!("{long}\nlast\n"), 100, 32, false);
        assert_eq!((out.text.as_str(), out.redacted), ("last\n", 0));
    }

    // covers UP-15
    #[test]
    fn logs_are_redacted_before_the_byte_cap() {
        // The value sits right after the cut: redacted while its label is still there.
        let log = format!(
            "{}\npassword=hunter2 {}\nlast\n",
            "a".repeat(50),
            "b".repeat(5)
        );
        let out = log_output(&log, 100, 40, false);
        assert_eq!(out.text, "password=[REDACTED:password] bbbbb\nlast\n");
        assert_eq!((out.truncated, out.redacted), (true, 1));
        // Markers longer than the values they replace still respect the cap.
        let log = "token=a\n".repeat(10);
        let out = log_output(&log, 100, 64, false);
        assert!(out.text.len() <= 64 && out.truncated, "{out:?}");
        assert_eq!(out.redacted, out.text.lines().count());
        // A credential only in a line the cap dropped is not counted.
        let out = log_output("token=abc\nplain line\n", 100, 12, false);
        assert_eq!((out.text.as_str(), out.redacted), ("plain line\n", 0));
        // Markers the workload logged itself are not counted, cut or not.
        let log = "old token=abc\nlogged [REDACTED:password] and token=def\n";
        assert_eq!(log_output(log, 100, 1_000, false).redacted, 2);
        let out = log_output(log, 100, 60, false);
        assert_eq!(
            (out.text.as_str(), out.redacted),
            ("logged [REDACTED:password] and token=[REDACTED:token]\n", 1)
        );
    }

    // covers UP-15
    #[test]
    fn container_args_and_commands_are_redacted() {
        let mut pod = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "spec": {"containers": [{
                "command": ["/bin/app", "--db-url=postgres://app:hunter2@db/app"],
                "args": ["--password", "pw2", "--verbose", "--api-key", "k3", "token=t4", "--token"],
                "env": [{"name": "PASSWORD", "value": "kept"}]
            }]}
        });
        assert_eq!(sanitize(&mut pod), 4);
        let container = &pod["spec"]["containers"][0];
        assert_eq!(
            container["command"],
            json!([
                "/bin/app",
                "--db-url=postgres://app:[REDACTED:password]@db/app"
            ])
        );
        assert_eq!(
            container["args"],
            json!([
                "--password",
                "[REDACTED:password]",
                "--verbose",
                "--api-key",
                "[REDACTED:key]",
                "token=[REDACTED:token]",
                "--token"
            ])
        );
        assert_eq!(container["env"][0]["value"], "kept");
    }

    // covers UP-15
    #[test]
    fn annotations_and_message_lists_are_redacted() {
        let mut object = json!({
            "apiVersion": "example.com/v1",
            "kind": "Backup",
            "metadata": {"name": "b", "annotations": {
                "kubectl.kubernetes.io/last-applied-configuration":
                    r#"{"spec":{"target":"postgres://app:hunter2@db/app"}}"#,
                "example.com/notes": "rotate soon, password: correct horse battery",
                "example.com/owner": "team-data",
                "example.com/replicas": 3,
                "example.com/values": {"job": {"message": "seed failed: token=t6"}}
            }},
            "status": {
                "messages": ["upload failed: token=t5", "retrying in 30s"],
                "warningMessages": [{"text": "kept as is"}]
            }
        });
        assert_eq!(sanitize(&mut object), 4);
        let text = object.to_string();
        for leaked in ["hunter2", "horse", "t5", "t6"] {
            assert!(!text.contains(leaked), "{leaked}: {text}");
        }
        let annotations = &object["metadata"]["annotations"];
        assert_eq!(annotations["example.com/owner"], "team-data");
        assert_eq!(annotations["example.com/replicas"], 3);
        assert_eq!(
            annotations["example.com/notes"],
            "rotate soon, password: [REDACTED:password]"
        );
        assert_eq!(
            object["status"]["messages"],
            json!(["upload failed: token=[REDACTED:token]", "retrying in 30s"])
        );
        assert_eq!(object["status"]["warningMessages"][0]["text"], "kept as is");
    }

    // covers UP-15
    #[test]
    fn last_applied_configurations_are_redacted_like_the_live_object() {
        let applied = json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": {"template": {"spec": {"containers": [{
                "name": "app",
                "args": ["--password", "pw7", "--verbose"],
                "env": [{"name": "DSN", "value": "postgres://app:hunter2@db/app"}]
            }]}}}
        });
        let mut object = json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"annotations": {LAST_APPLIED: format!("{applied}\n")}}
        });
        assert_eq!(sanitize(&mut object), 2);
        let text = object["metadata"]["annotations"][LAST_APPLIED]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(text.ends_with('\n'), "{text}");
        let kept: Value = serde_json::from_str(&text).unwrap();
        let container = &kept["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(
            container["args"],
            json!(["--password", "[REDACTED:password]", "--verbose"])
        );
        assert_eq!(
            container["env"][0]["value"],
            "postgres://app:[REDACTED:password]@db/app"
        );

        // Nothing to hide: the annotation is kept byte for byte, and text that is not JSON is
        // still redacted as free text.
        let plain = r#"{"kind":"Service",  "spec":{"ports":[{"port":80}]}}"#;
        let mut object = json!({"kind": "Service", "metadata": {"annotations": {
            LAST_APPLIED: plain,
            "other": "{not json password=pw8"
        }}});
        assert_eq!(sanitize(&mut object), 1);
        assert_eq!(object["metadata"]["annotations"][LAST_APPLIED], plain);
        assert_eq!(
            object["metadata"]["annotations"]["other"],
            "{not json password=[REDACTED:password]"
        );
        let mut broken = json!({"kind": "Pod", "metadata": {"annotations": {
            LAST_APPLIED: "{\"args\":[\"--token\" token=t9"
        }}});
        assert_eq!(sanitize(&mut broken), 1);
        assert!(!broken.to_string().contains("t9"));
    }

    // covers UP-15
    #[test]
    fn config_maps_are_shown_as_returned() {
        let mut list = json!({"kind": "List", "items": [{
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "c"},
            "data": {"message": "password=example", "note": "token=t", "args": "x"}
        }]});
        let before = list["items"][0]["data"].clone();
        assert_eq!(sanitize(&mut list), 0);
        assert_eq!(list["items"][0]["data"], before);
    }

    // covers UP-15
    #[test]
    fn list_items_are_redacted_once() {
        // Built at run time, so the source holds no private key header.
        let pem = format!(
            "{}BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----",
            "-----"
        );
        let mut list = json!({"kind": "List", "items": [{"kind": "Event", "message": pem}]});
        assert_eq!(sanitize(&mut list), 1);
    }

    // covers UP-10
    #[test]
    fn names_must_be_single_path_segments() {
        for ok in ["web", "web-1", "a", "0", &"a".repeat(63)] {
            assert!(is_label_name(ok), "{ok}");
            assert!(is_pod_name(ok), "{ok}");
            assert!(is_object_name(ok), "{ok}");
        }
        for ok in ["web.1", "kube-dns.kube-system", "v1.apps"] {
            assert!(is_pod_name(ok) && is_object_name(ok), "{ok}");
            assert!(!is_label_name(ok), "{ok}");
        }
        for ok in [
            "system:controller:job-controller",
            "eks:node-bootstrapper",
            "user@example.com",
            "Admin_Role",
        ] {
            assert!(is_object_name(ok), "{ok}");
            assert!(!is_pod_name(ok) && !is_label_name(ok), "{ok}");
        }
        let long_label = "a".repeat(64);
        let long_name = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(62)
        );
        assert_eq!(long_name.len(), 254);
        assert!(!is_label_name(&long_label));
        assert!(is_pod_name(&long_name[1..]) && is_object_name(&long_name[1..]));
        assert!(!is_pod_name(&long_name) && !is_object_name(&long_name));
        // A subdomain's parts are not limited to 63 characters, only the whole name.
        let long_pod = "a".repeat(70);
        assert!(is_pod_name(&long_pod) && is_pod_name(&format!("{long_pod}.web")));
        assert!(is_pod_name(&"a".repeat(253)) && !is_pod_name(&"a".repeat(254)));
        let long_rbac = format!("system:{}", "a".repeat(247));
        assert!(!is_object_name(&long_rbac));
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "payments/secrets",
            "x/../y",
            "/",
            "a%2Fb",
            "%2F",
            "%2e%2e",
            "a?b",
            "a#b",
            "a b",
            "a\nb",
            "a\tb",
            "a\u{0}b",
            "a\u{7f}b",
            "\u{0430}",
        ] {
            assert!(!is_label_name(bad), "{bad:?}");
            assert!(!is_pod_name(bad), "{bad:?}");
            assert!(!is_object_name(bad), "{bad:?}");
        }
        // Single path segments, but not DNS-1123: not a namespace, container or pod name.
        for not_dns in ["Web", "-a", "a-", "a..b", "a_b"] {
            assert!(
                !is_label_name(not_dns) && !is_pod_name(not_dns),
                "{not_dns}"
            );
        }
    }

    // covers UP-10
    #[test]
    fn secret_lists_are_redacted_although_their_items_have_no_kind() {
        let mut list = json!({
            "apiVersion": "v1",
            "kind": "SecretList",
            "metadata": {"resourceVersion": "1"},
            "items": [{
                "metadata": {
                    "name": "db",
                    "namespace": "payments",
                    "managedFields": [{}],
                    "annotations": {LAST_APPLIED: "{\"data\":{\"password\":\"c2VjcmV0\"}}"}
                },
                "data": {"password": "c2VjcmV0"},
                "stringData": {"token": "plain"},
                "type": "Opaque"
            }]
        });
        sanitize(&mut list);
        let item = &list["items"][0];
        assert_eq!(item["data"], json!({"password": REDACTED}));
        assert_eq!(item["stringData"], json!({"token": REDACTED}));
        assert_eq!(item["metadata"]["annotations"][LAST_APPLIED], REDACTED);
        assert!(item["metadata"].get("managedFields").is_none());
        let text = list.to_string();
        assert!(
            !text.contains("c2VjcmV0") && !text.contains("plain"),
            "{text}"
        );
    }

    // covers UP-10
    #[test]
    fn what_was_read_as_a_secret_is_redacted_whatever_its_kind_says() {
        let mut lying = json!({"kind": "ConfigMap", "data": {"password": "c2VjcmV0"}});
        sanitize_secret(&mut lying);
        assert_eq!(lying["data"]["password"], REDACTED);
        let mut list = json!({"kind": "List", "items": [{"data": {"a": "c2VjcmV0"}}]});
        sanitize_secret(&mut list);
        assert_eq!(list["items"][0]["data"]["a"], REDACTED);
        // A CRD's SecretList in another group is not core Secrets.
        let mut custom = json!({
            "apiVersion": "example.com/v1",
            "kind": "SecretList",
            "items": [{"metadata": {"name": "x"}, "data": {"a": "b"}}]
        });
        sanitize(&mut custom);
        assert_eq!(custom["items"][0]["data"]["a"], "b");
        // Without the hint, a kindless item outside a SecretList is not a Secret.
        let mut plain = json!({"kind": "ConfigMapList", "items": [{"data": {"a": "b"}}]});
        sanitize(&mut plain);
        assert_eq!(plain["items"][0]["data"]["a"], "b");
    }
}
