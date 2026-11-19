//! What may be shown of Secrets and of error messages, shared by the app and the MCP server so
//! both hide the same things (ADR-0009).
//!
//! Secret `data` and `stringData` values are hidden, and so is every annotation value outside a
//! short allowlist: tools store whole objects in annotations (kubectl's last-applied
//! configuration, kapp, Rancher), and those copies include the Secret's values. Each front end
//! picks its own marker (the MCP server says `<redacted>`, the app shows dots).

use serde_json::Value;

/// Secret annotations whose values are shown.
const SAFE_SECRET_ANNOTATIONS: &[&str] = &[
    "kubernetes.io/service-account.name",
    "kubernetes.io/service-account.uid",
    "meta.helm.sh/release-name",
    "meta.helm.sh/release-namespace",
];

/// Secret Table columns whose cells are shown: identity, type and key count, never values.
const SAFE_SECRET_COLUMNS: &[&str] = &["Name", "Namespace", "Type", "Data", "Age"];

/// Whether a Secret annotation's value may be shown.
pub fn is_safe_secret_annotation(key: &str) -> bool {
    SAFE_SECRET_ANNOTATIONS.contains(&key)
}

/// Whether a Secret Table column may be shown. Any other column is hidden, in case a server or
/// an extension adds a printer column that shows a value.
pub fn is_safe_secret_column(name: &str) -> bool {
    SAFE_SECRET_COLUMNS.contains(&name)
}

/// Whether `object` is a core v1 Secret.
pub fn is_secret_object(object: &Value) -> bool {
    object.get("kind").and_then(Value::as_str) == Some("Secret")
        && object.get("apiVersion").and_then(Value::as_str) == Some("v1")
}

/// Replaces every `data` and `stringData` value of a Secret, and every annotation value outside
/// the allowlist, with `marker`. Keys are kept. Call it only on Secrets.
pub fn mask_secret(object: &mut Value, marker: &str) {
    if let Some(annotations) = object
        .get_mut("metadata")
        .and_then(|m| m.get_mut("annotations"))
        .and_then(Value::as_object_mut)
    {
        for (key, value) in annotations.iter_mut() {
            if !is_safe_secret_annotation(key) {
                *value = Value::String(marker.to_owned());
            }
        }
    }
    for field in ["data", "stringData"] {
        if let Some(map) = object.get_mut(field).and_then(Value::as_object_mut) {
            for value in map.values_mut() {
                *value = Value::String(marker.to_owned());
            }
        }
    }
}

/// Replaces every double-quoted and backquoted string in `text` with `"…"`. Parse errors can
/// quote a value from the file they read ("invalid type: string \"...\"", "unknown variant
/// `...`"), which may be a credential.
pub fn redact_quoted(text: &str) -> String {
    redact_between(&redact_between(text, '"'), '`')
}

fn redact_between(text: &str, quote: char) -> String {
    let mut out = String::with_capacity(text.len());
    let mut parts = text.split(quote);
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    let mut inside = true;
    for part in parts {
        if inside {
            out.push(quote);
            out.push('…');
        } else {
            out.push(quote);
            out.push_str(part);
        }
        inside = !inside;
    }
    out
}

/// What may be shown of an error from parsing a file that can hold credentials (kubeconfig,
/// settings): its first line, at most 200 characters, with quoted values hidden. Use it
/// everywhere such an error is displayed, tooltips included.
pub fn safe_error_summary(error: &str) -> String {
    redact_quoted(&crate::kubeconfig::error_summary(error))
}

/// What may be shown of an API server URL: scheme, host, port and path. User info, query and
/// fragment are dropped, since a hand-written kubeconfig could put a credential there. The rest
/// is what kubeconfig, `settings.toml` (ADR-0009) and the app's sidebar already show.
pub fn server_url(url: &str) -> String {
    let url = url.trim();
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or_default();
    match scheme {
        Some(scheme) => format!("{scheme}://{host}{path}"),
        None => format!("{host}{path}"),
    }
}

/// What may be shown of an exec auth plugin's command: the program's file name (`aws`), never
/// its directory (which can name the user's home), arguments or environment.
pub fn command_name(command: &str) -> String {
    let command = command.trim();
    std::path::Path::new(command)
        .file_name()
        .map_or_else(|| command.to_owned(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_values_and_unsafe_annotations_keeping_keys() {
        let mut secret = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"annotations": {
                "kubectl.kubernetes.io/last-applied-configuration": "{\"data\":{\"p\":\"x\"}}",
                "meta.helm.sh/release-name": "web"
            }},
            "data": {"p": "eA=="},
            "stringData": {"q": "y"}
        });
        assert!(is_secret_object(&secret));
        mask_secret(&mut secret, "*");
        assert_eq!(secret["data"], json!({"p": "*"}));
        assert_eq!(secret["stringData"], json!({"q": "*"}));
        let annotations = &secret["metadata"]["annotations"];
        assert_eq!(
            annotations["kubectl.kubernetes.io/last-applied-configuration"],
            "*"
        );
        assert_eq!(annotations["meta.helm.sh/release-name"], "web");
        assert!(!is_secret_object(
            &json!({"apiVersion": "v2", "kind": "Secret"})
        ));
    }

    #[test]
    fn error_summaries_hide_quoted_values_and_later_lines() {
        let error = "invalid type: string \"hunter2\", expected a map at line 3\n  token: hunter2";
        let summary = safe_error_summary(error);
        assert_eq!(
            summary,
            "invalid type: string \"…\", expected a map at line 3"
        );
        assert_eq!(
            redact_quoted("unknown variant `abc`"),
            "unknown variant `…`"
        );
    }

    #[test]
    fn server_urls_drop_user_info_query_and_fragment() {
        assert_eq!(
            server_url("https://127.0.0.1:6443"),
            "https://127.0.0.1:6443"
        );
        assert_eq!(
            server_url(" https://admin:hunter2@api.example.com:6443/k8s/clusters/c-1?token=x#y "),
            "https://api.example.com:6443/k8s/clusters/c-1"
        );
        assert_eq!(server_url("u:p@host:443"), "host:443");
    }

    #[test]
    fn command_names_drop_the_directory() {
        assert_eq!(command_name("aws"), "aws");
        assert_eq!(
            command_name("/Users/someone/bin/gke-gcloud-auth-plugin"),
            "gke-gcloud-auth-plugin"
        );
    }
}
