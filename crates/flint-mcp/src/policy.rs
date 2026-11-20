//! What the MCP server may show (ADR-0009): opt-in clusters, their scopes (ADR-0013),
//! redaction, and output bounds. Everything here is pure so it can be tested without a cluster.

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
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::ClusterNotFound(name) => write!(
                f,
                "cluster \"{name}\" not found. Call list_clusters for the available clusters."
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

/// A hint for the agent when list_clusters has nothing useful to show.
pub fn list_clusters_hint(exposed: usize, changed: usize) -> Option<String> {
    let changed_note = match changed {
        0 => String::new(),
        1 => " One cluster the user exposed now points to a different API server, so it is hidden \
              until they expose it again in Flint."
            .to_owned(),
        n => format!(
            " {n} clusters the user exposed now point to a different API server, so they are \
             hidden until they expose them again in Flint."
        ),
    };
    if exposed == 0 {
        Some(format!(
            "No clusters are exposed to AI agents. Ask the user to open Flint and turn on \
             Expose to AI (the robot button) next to a cluster in the sidebar.{changed_note}"
        ))
    } else if changed > 0 {
        Some(changed_note.trim_start().to_owned())
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

/// The error for anything outside a cluster's scope: exactly the one a cluster that is not
/// exposed gives, so an agent cannot tell what exists beyond its scope.
pub fn out_of_scope(cluster: &str) -> PolicyError {
    PolicyError::ClusterNotFound(cluster.to_owned())
}

/// The kinds of `kinds` the agent may read: those the scope allows. Under a namespace scope,
/// cluster-scoped kinds only when the kinds scope names them.
pub fn kinds_in_scope(scope: &AiScope, kinds: &[KindInfo]) -> Vec<KindInfo> {
    kinds
        .iter()
        .filter(|k| scope.allows_kind(k.id().as_str(), k.namespaced))
        .cloned()
        .collect()
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

/// Whether the agent may read pod logs under `scope`: pods are in the kinds scope (the
/// namespace is checked with [`namespaces_for`]).
pub fn logs_in_scope(scope: &AiScope) -> bool {
    scope.allows_kind("pods", true)
}

/// Makes an object safe to show: removes `metadata.managedFields`, redacts Secret values, and
/// redacts credentials in message fields and container arguments ([`redact_object_text`]).
/// Works on single objects and on lists (`items`). Returns how many credentials were replaced.
pub fn sanitize(value: &mut Value) -> usize {
    strip(value, false);
    redact_object_text(value)
}

/// [`sanitize`] for what was read as a core v1 Secret: the object, and every item if it is a
/// list, is redacted as a Secret whatever its `kind` field says.
pub fn sanitize_secret(value: &mut Value) -> usize {
    strip(value, true);
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

/// Redacts credentials in `text` in place. Returns how many were replaced.
pub fn redact_text(text: &mut String) -> usize {
    let redacted = flint_core::redact::redact_credentials(text);
    let count = redacted.count;
    if let std::borrow::Cow::Owned(new) = redacted.text {
        *text = new;
    }
    count
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
/// at a line boundary. Redacting before the byte cut sees each value next to its label, and the
/// cap then applies to the text actually returned.
pub fn log_output(fetched: &str, max_lines: usize, max_bytes: usize) -> LogOutput {
    let (text, cut_lines) = tail_lines(fetched, max_lines);
    let redacted = flint_core::redact::redact_credentials(text);
    let (kept, cut_bytes) = tail_bytes(&redacted.text, max_bytes);
    // Only markers the redactor wrote into the kept lines, not ones the workload logged.
    let count = redacted.count_from(redacted.text.len() - kept.len());
    let note = (kept.is_empty() && !redacted.text.is_empty()).then(|| {
        let limit = if max_bytes.is_multiple_of(1024) {
            format!("{} KiB", max_bytes / 1024)
        } else {
            format!("{max_bytes} bytes")
        };
        format!("the last line is longer than the {limit} limit and was left out")
    });
    LogOutput {
        text: kept.to_owned(),
        truncated: cut_lines || cut_bytes,
        redacted: count,
        note,
    }
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
        let hint = list_clusters_hint(0, 1).unwrap();
        assert!(hint.contains("Expose to AI") && hint.contains("different API server"));
    }

    // covers UP-09
    #[test]
    fn hint_only_when_useful() {
        assert!(list_clusters_hint(0, 0).unwrap().contains("Expose to AI"));
        assert_eq!(list_clusters_hint(2, 0), None);
        assert!(list_clusters_hint(1, 2).unwrap().starts_with("2 clusters"));
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

    // covers UP-14: anything out of scope fails exactly as an unexposed cluster does, in text
    // and in type, so nothing can be probed.
    #[test]
    fn out_of_scope_is_the_unexposed_error() {
        let kc = kubeconfig(&["dev", "prod"]);
        let s = settings(&["dev"]);
        let unexposed = exposed_cluster(&s, &kc, "prod").unwrap_err();
        assert_eq!(out_of_scope("prod"), unexposed);
        assert_eq!(out_of_scope("prod").to_string(), unexposed.to_string());
        let scope = scoped(&["shop"], &[]);
        assert_eq!(
            namespaces_for(&scope, "prod", true, Some("kube-system")),
            Err(unexposed)
        );
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
        let out = log_output(&long, 100, 32);
        assert_eq!(
            (out.text.as_str(), out.truncated, out.redacted),
            ("", true, 0)
        );
        assert_eq!(
            out.note.as_deref(),
            Some("the last line is longer than the 32 bytes limit and was left out")
        );
        let out = log_output(&long, 100, 64 * 1024);
        assert_eq!((out.redacted, out.note), (1, None));
        let out = log_output(&format!("{long}\nlast\n"), 100, 32);
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
        let out = log_output(&log, 100, 40);
        assert_eq!(out.text, "password=[REDACTED:password] bbbbb\nlast\n");
        assert_eq!((out.truncated, out.redacted), (true, 1));
        // Markers longer than the values they replace still respect the cap.
        let log = "token=a\n".repeat(10);
        let out = log_output(&log, 100, 64);
        assert!(out.text.len() <= 64 && out.truncated, "{out:?}");
        assert_eq!(out.redacted, out.text.lines().count());
        // A credential only in a line the cap dropped is not counted.
        let out = log_output("token=abc\nplain line\n", 100, 12);
        assert_eq!((out.text.as_str(), out.redacted), ("plain line\n", 0));
        // Markers the workload logged itself are not counted, cut or not.
        let log = "old token=abc\nlogged [REDACTED:password] and token=def\n";
        assert_eq!(log_output(log, 100, 1_000).redacted, 2);
        let out = log_output(log, 100, 60);
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
