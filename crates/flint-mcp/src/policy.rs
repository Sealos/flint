//! What the MCP server may show (ADR-0009): opt-in clusters, redaction, and output bounds.
//! Everything here is pure so it can be tested without a cluster.

use flint_core::Settings;
use flint_core::kubeconfig::{ContextInfo, LoadedKubeconfig};
use flint_core::settings::AiExposureState;
use serde_json::Value;

/// Replaces every redacted value.
pub const REDACTED: &str = "<redacted>";

/// Annotation `kubectl apply` writes with the whole object, Secret values included.
#[cfg(test)]
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

/// Makes an object safe to show: removes `metadata.managedFields`, redacts Secret values, and
/// redacts credentials in message fields ([`redact_messages`]). Works on single objects and on
/// lists (`items`). Returns how many credentials were replaced in messages.
pub fn sanitize(value: &mut Value) -> usize {
    let mut count = 0;
    if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            count += sanitize(item);
        }
    }
    flint_core::read::strip_managed_fields(value);
    if flint_core::redact::is_secret_object(value) {
        flint_core::redact::mask_secret(value, REDACTED);
    }
    count + redact_messages(value)
}

/// Whether an object field holds free text that a workload or controller wrote: an Event's
/// `message` (`note` in `events.k8s.io`), status and condition messages, container termination
/// messages (which can be the container's last log lines), and operators' `failureMessage`,
/// `errorMessage` or `statusMessage`.
fn is_message_field(key: &str) -> bool {
    key == "note"
        || key
            .get(key.len().saturating_sub("message".len())..)
            .is_some_and(|end| end.eq_ignore_ascii_case("message"))
}

/// Replaces credential-shaped values ([`flint_core::redact::redact_credentials`]) in every
/// string under a `note` key or a key ending in `message`, at any depth. Returns how many were
/// replaced.
pub fn redact_messages(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => map
            .iter_mut()
            .map(|(key, value)| match value {
                Value::String(text) if is_message_field(key) => redact_text(text),
                _ => redact_messages(value),
            })
            .sum(),
        Value::Array(items) => items.iter_mut().map(redact_messages).sum(),
        _ => 0,
    }
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

/// Keeps the last `max_bytes` of `text`, starting at a line boundary when possible. For logs,
/// where the newest lines matter most.
pub fn tail_bytes(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let cut = &text[start..];
    (cut.find('\n').map_or(cut, |i| &cut[i + 1..]), true)
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
                .map(|n| AiExposure {
                    context: (*n).into(),
                    server: server(n),
                })
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
            ai_exposed_clusters: vec![AiExposure {
                context: "dev".into(),
                server: "https://old-dev".into(),
            }],
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
        assert_eq!(tail_bytes("ab\ncd\nef", 5), ("ef", true));
        assert_eq!(tail_bytes("ab\ncd\nef\n", 7), ("cd\nef\n", true));
    }
}
