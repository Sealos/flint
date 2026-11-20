//! What may be shown of Secrets and of error messages, shared by the app and the MCP server so
//! both hide the same things (ADR-0009).
//!
//! Secret `data` and `stringData` values are hidden, and so is every annotation value outside a
//! short allowlist: tools store whole objects in annotations (kubectl's last-applied
//! configuration, kapp, Rancher), and those copies include the Secret's values. Each front end
//! picks its own marker (the MCP server says `<redacted>`, the app shows dots).
//!
//! [`redact_credentials`] hides credential-shaped values in free text (pod logs, event
//! messages). Only the MCP server uses it: the app shows a cluster's logs and events as they are.

mod text;

pub use text::{Redacted, credential_flag, redact_credentials};

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

/// The first non-empty line of `text`, trimmed, cut to `max_chars` characters (with `…`).
pub fn first_line(text: &str, max_chars: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    match line.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_owned(),
    }
}

/// Shown instead of a server URL that cannot be shown safely.
pub const HIDDEN_SERVER_URL: &str = "<server URL not shown>";

/// What may be shown of an API server URL: `scheme://host[:port][/path]`. User info, query and
/// fragment are dropped, since a hand-written kubeconfig could put a credential there. A URL that
/// does not parse, or that still holds an `@` after that (a password with a `/` or `?` in it),
/// becomes [`HIDDEN_SERVER_URL`]. The rest is what kubeconfig, `settings.toml` (ADR-0009) and the
/// app's sidebar already show.
pub fn server_url(url: &str) -> String {
    let Ok(uri) = url.trim().parse::<http::Uri>() else {
        return HIDDEN_SERVER_URL.to_owned();
    };
    let Some(host) = uri.host().filter(|h| !h.is_empty()) else {
        return HIDDEN_SERVER_URL.to_owned();
    };
    let authority = uri.authority().map_or("", |a| a.as_str());
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    // An `@` outside the authority means a `/`, `?` or `#` in a password split the URL early,
    // and a port that is not a number is the rest of a user name and password: either way,
    // what parsed as the host is part of a credential.
    let port = if host_port.starts_with('[') {
        host_port.rsplit_once("]:").map(|(_, port)| port)
    } else {
        host_port.rsplit_once(':').map(|(_, port)| port)
    };
    let bad_port = port.is_some_and(|p| p.parse::<u16>().is_err());
    if (url.contains('@') && !authority.contains('@')) || bad_port {
        return HIDDEN_SERVER_URL.to_owned();
    }
    let mut out = String::new();
    if let Some(scheme) = uri.scheme_str() {
        out.push_str(scheme);
        out.push_str("://");
    }
    out.push_str(host);
    if let Some(port) = uri.port_u16() {
        out.push_str(&format!(":{port}"));
    }
    let path = uri.path();
    if path != "/" {
        out.push_str(path);
    }
    if out.contains('@') {
        return HIDDEN_SERVER_URL.to_owned();
    }
    out
}

/// What may be shown of an exec auth plugin's command: the program's file name (`aws`), never
/// its directory (which can name the user's home), arguments or environment. kubeconfig's
/// `command` is one program path, but it may have been written with arguments after it, and a
/// path may hold spaces: with whitespace in it, nothing can tell which is which without looking
/// at the disk (which a stalled mount can hang), so nothing of it is shown (`None`: say "the exec
/// auth plugin"). A spaced path like `/opt/Cloud Tools/kubelogin` is therefore not named, which
/// is the price of never showing an argument.
pub fn command_name(command: &str) -> Option<String> {
    let command = command.trim();
    if command.is_empty() || command.contains(char::is_whitespace) {
        return None;
    }
    let name = std::path::Path::new(command)
        .file_name()
        .map_or_else(|| command.to_owned(), |n| n.to_string_lossy().into_owned());
    Some(name)
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
    fn first_lines_are_cut() {
        assert_eq!(first_line("\n  one \"x\"\ntwo", 50), "one \"x\"");
        assert_eq!(first_line("abcdef", 3), "abc…");
        assert_eq!(first_line("", 3), "");
    }

    #[test]
    fn server_urls_keep_scheme_host_port_and_path_only() {
        for (url, shown) in [
            ("https://127.0.0.1:6443", "https://127.0.0.1:6443"),
            ("https://127.0.0.1:6443/", "https://127.0.0.1:6443"),
            (
                " https://admin:hunter2@api.example.com:6443/k8s/clusters/c-1?token=x#y ",
                "https://api.example.com:6443/k8s/clusters/c-1",
            ),
            ("https://[::1]:6443", "https://[::1]:6443"),
            ("u:p@host:443", "host:443"),
        ] {
            assert_eq!(server_url(url), shown, "{url}");
        }
    }

    #[test]
    fn server_urls_never_show_user_info() {
        // Passwords with `/`, `?` or `#` move the rest of the user info into the path or query.
        for url in [
            "https://admin:pa/ss@host",
            "https://u:p?x@host",
            "https://u:123/x@host",
            "https://u:p#x@host",
            "not a url",
            "",
        ] {
            assert_eq!(server_url(url), HIDDEN_SERVER_URL, "{url}");
        }
    }

    #[test]
    fn command_names_show_a_plain_program_by_its_file_name() {
        assert_eq!(command_name("kubelogin").as_deref(), Some("kubelogin"));
        assert_eq!(
            command_name("/usr/local/bin/kubelogin").as_deref(),
            Some("kubelogin")
        );
        assert_eq!(
            command_name("/Users/someone/bin/gke-gcloud-auth-plugin").as_deref(),
            Some("gke-gcloud-auth-plugin")
        );
        assert_eq!(command_name(""), None);
    }

    // With whitespace in the command, none of it is shown: an argument (slashes and all) never
    // is, and so a spaced path is not named either. Nothing reads the disk.
    #[test]
    fn command_names_show_nothing_of_a_command_with_whitespace() {
        let token = format!("{}{}", "abc", "/def-secret");
        assert_eq!(command_name(&format!("/opt/bin/aws --token={token}")), None);
        assert_eq!(command_name(&format!("aws --token={token}")), None);
        assert_eq!(command_name(" /opt/bin/aws eks get-token --secret x"), None);
        assert_eq!(command_name("/opt/Cloud Tools/kubelogin"), None);
    }
}
