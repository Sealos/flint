//! What may be shown of Secrets and of error messages, shared by the app and the MCP server so
//! both hide the same things (ADR-0009).
//!
//! Secret `data` and `stringData` values are hidden, and so is every annotation value outside a
//! short allowlist: tools store whole objects in annotations (kubectl's last-applied
//! configuration, kapp, Rancher), and those copies include the Secret's values. Each front end
//! picks its own marker (the MCP server says `<redacted>`, the app shows dots).
//!
//! [`redact_credentials`] hides credential-shaped values in free text (pod logs, event
//! messages). The MCP server applies it to cluster text, and logging applies it to every line
//! written to stderr or the log file; the app shows a cluster's logs and events as they are.

mod text;

pub use text::{Redacted, credential_flag, redact_credentials};

use serde_json::Value;

use crate::table::{Column, ColumnRole, wire_role};

/// Secret annotations whose values are shown.
const SAFE_SECRET_ANNOTATIONS: &[&str] = &[
    "kubernetes.io/service-account.name",
    "kubernetes.io/service-account.uid",
    "meta.helm.sh/release-name",
    "meta.helm.sh/release-namespace",
];

/// Whether a Secret annotation's value may be shown.
pub fn is_safe_secret_annotation(key: &str) -> bool {
    SAFE_SECRET_ANNOTATIONS.contains(&key)
}

/// Whether the column at `index` of a Secret Table may be shown. Any other column is hidden, in
/// case a server or an extension adds a printer column that shows a value.
pub fn is_safe_secret_column(columns: &[Column], index: usize) -> bool {
    is_safe_at(
        columns.len(),
        |i| (&*columns[i].name, columns[i].role),
        index,
    )
}

/// [`is_safe_secret_column`] for a Table's raw definitions, as `(name, format)`. Raw cells are
/// still the server's text, even under a metadata role, so any header that appears more than
/// once, ignoring ASCII case, is not safe. A caller keeps a `Name` or `Age` only by replacing it
/// from the object's metadata.
pub fn safe_secret_wire_columns(definitions: &[(&str, &str)]) -> Vec<bool> {
    let columns: Vec<(&str, ColumnRole)> = definitions
        .iter()
        .enumerate()
        .map(|(i, &(name, format))| (name, wire_role(i, name, format)))
        .collect();
    (0..columns.len())
        .map(|index| {
            is_safe_at(columns.len(), |i| columns[i], index)
                && columns
                    .iter()
                    .enumerate()
                    .all(|(i, c)| i == index || !c.0.eq_ignore_ascii_case(columns[index].0))
        })
        .collect()
}

/// The one rule for safe Secret columns, failing closed:
/// - `Name`, `Namespace` and `Age` by their role, whose cells come from the object's metadata and
///   never from the server's text, however many there are.
/// - `Type` and `Data` are server text, so only when no other column has the header: with two,
///   nothing tells which is the real one.
fn is_safe_at<'a>(
    len: usize,
    column: impl Fn(usize) -> (&'a str, ColumnRole),
    index: usize,
) -> bool {
    if index >= len {
        return false;
    }
    let (name, role) = column(index);
    match (name, role) {
        ("Name", ColumnRole::Name)
        | ("Namespace", ColumnRole::Namespace)
        | ("Age", ColumnRole::Age) => true,
        ("Type" | "Data", ColumnRole::Value) => (0..len).all(|i| i == index || column(i).0 != name),
        _ => false,
    }
}

/// Whether a list of `kind` (a [`crate::KindId`] string) is of core Secrets, whose columns are
/// masked. A CRD also named `secrets` has a group in its id, so it is not.
pub fn is_secret_list(kind: &str) -> bool {
    kind == "secrets"
}

/// Whether the column at `index` of a list hides its values. Everything derived from such a
/// column (text, filter matches, sort order, width) must not depend on its raw cells.
pub fn is_masked_column(secret_list: bool, columns: &[Column], index: usize) -> bool {
    secret_list && !is_safe_secret_column(columns, index)
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

/// What may be shown of an API server's error text about a write: [`safe_error_summary`] of it
/// with credentials hidden first, so that the cut to one short line cannot split a value. The
/// value after a bare `Bearer` is hidden too, whatever its characters.
pub fn safe_server_error(text: &str) -> String {
    safe_error_summary(&hide_bearer_values(&redact_credentials(text).text))
}

/// Stands for a value hidden after `Bearer`.
const BEARER_MARKER: &str = "[REDACTED:bearer]";

/// Words that can follow `Bearer` as a description of it, and lead to the next word.
const BEARER_NOUNS: &[&str] = &[
    "auth",
    "authentication",
    "authorization",
    "credential",
    "credentials",
    "header",
    "scheme",
    "token",
    "tokens",
];

/// Words that show the text after `Bearer` is a sentence. Anything else is taken for a value.
const BEARER_SENTENCE_WORDS: &[&str] = &[
    "and", "are", "cannot", "denied", "expired", "failed", "for", "from", "had", "has", "in",
    "invalid", "is", "missing", "must", "not", "of", "on", "or", "rejected", "required", "to",
    "was", "were", "with",
];

/// `text` with the value after each word `bearer`, in any case, replaced by [`BEARER_MARKER`].
fn hide_bearer_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let (mut copied, mut from) = (0, 0);
    while let Some(found) = lower[from..].find("bearer") {
        let start = from + found;
        from = start + "bearer".len();
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        if let Some(value) = bearer_value(text, from) {
            out.push_str(&text[copied..value.start]);
            out.push_str(BEARER_MARKER);
            copied = value.end;
            from = value.end;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Where the value after a `bearer` ending at `after` is, if what follows is not a sentence
/// about it or a value already hidden.
fn bearer_value(text: &str, after: usize) -> Option<std::ops::Range<usize>> {
    let is_quote = |c: char| matches!(c, '"' | '\'' | '`');
    let mut at = after;
    loop {
        let rest = &text[at..];
        let word = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ':' || c == '=');
        let first = word.chars().next()?;
        if word.len() == rest.len() && !is_quote(first) {
            return None;
        }
        if word.starts_with("[REDACTED:") {
            return None;
        }
        let start = text.len() - word.len();
        if is_quote(first) {
            let inner = start + first.len_utf8();
            let end = text[inner..]
                .find(first)
                .map_or(text.len(), |i| inner + i + first.len_utf8());
            return Some(start..end);
        }
        let len = word
            .find(|c: char| c.is_whitespace() || is_quote(c))
            .unwrap_or(word.len());
        let value = &word[..len];
        let plain = value
            .trim_end_matches(['.', ',', ';', '!', '?', ':', '=', ')', ']'])
            .to_ascii_lowercase();
        if BEARER_NOUNS.contains(&plain.as_str()) {
            if value.ends_with(['.', ',', ';', '!', '?']) {
                return None;
            }
            at = start + len;
        } else if BEARER_SENTENCE_WORDS.contains(&plain.as_str()) {
            return None;
        } else {
            return Some(start..start + len);
        }
    }
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
    use crate::table::ColumnType;
    use serde_json::json;

    fn column(name: &str, role: ColumnRole) -> Column {
        Column {
            name: name.into(),
            ty: ColumnType::String,
            format: "".into(),
            description: "".into(),
            priority: 0,
            role,
        }
    }

    fn standard() -> Vec<Column> {
        vec![
            column("Name", ColumnRole::Name),
            column("Namespace", ColumnRole::Namespace),
            column("Type", ColumnRole::Value),
            column("Data", ColumnRole::Value),
            column("Age", ColumnRole::Age),
        ]
    }

    fn safe(columns: &[Column]) -> Vec<bool> {
        (0..columns.len())
            .map(|i| is_safe_secret_column(columns, i))
            .collect()
    }

    #[test]
    fn the_standard_secret_columns_are_safe() {
        assert_eq!(safe(&standard()), [true; 5]);
        assert!(!is_safe_secret_column(&standard(), 5), "past the columns");
    }

    #[test]
    fn a_safe_header_needs_the_role_that_makes_it_safe() {
        for (header, role) in [
            ("Name", ColumnRole::Value),
            ("Namespace", ColumnRole::Value),
            ("Age", ColumnRole::Value),
            ("Type", ColumnRole::Name),
            ("Data", ColumnRole::Age),
            ("Token", ColumnRole::Name),
            ("type", ColumnRole::Value),
            ("age", ColumnRole::Age),
        ] {
            assert!(!safe(&[column(header, role)])[0], "{header} as {role:?}");
        }
    }

    #[test]
    fn a_second_age_is_computed_from_metadata_so_it_stays_safe() {
        let mut columns = standard();
        columns.push(column("Age", ColumnRole::Age));
        assert_eq!(safe(&columns), [true; 6]);
    }

    #[test]
    fn a_repeated_identity_header_hides_only_the_extra_column() {
        for (header, role) in [
            ("Name", ColumnRole::Value),
            ("Namespace", ColumnRole::Value),
            ("Age", ColumnRole::Value),
        ] {
            let mut columns = standard();
            columns.push(column(header, role));
            assert_eq!(
                safe(&columns),
                [true, true, true, true, true, false],
                "{header}"
            );
        }
    }

    #[test]
    fn a_repeated_type_or_data_header_hides_every_copy() {
        for (header, at) in [("Type", 2), ("Data", 3)] {
            // After the real column, and before it: position cannot tell them apart.
            let mut after = standard();
            after.push(column(header, ColumnRole::Value));
            let mut before = standard();
            before.insert(at, column(header, ColumnRole::Value));
            for columns in [after, before] {
                let shown = safe(&columns);
                for (i, c) in columns.iter().enumerate() {
                    assert_eq!(shown[i], &*c.name != header, "{header} at {i}");
                }
            }
        }
    }

    #[test]
    fn raw_definitions_follow_the_same_rule() {
        let plain = [("Name", ""), ("Type", ""), ("Data", ""), ("Age", "")];
        assert_eq!(safe_secret_wire_columns(&plain), [true; 4]);
        let repeated = [
            ("Name", ""),
            ("Type", ""),
            ("Data", ""),
            ("Age", ""),
            ("Name", ""),
            ("Namespace", ""),
            ("Type", ""),
            ("Data", ""),
            ("Age", ""),
        ];
        assert_eq!(
            safe_secret_wire_columns(&repeated),
            [false; 9],
            "every header in this table is repeated"
        );
        // The first `Age` has format `name`, so the two have different roles.
        assert_eq!(
            safe_secret_wire_columns(&[("Age", "name"), ("Age", "")]),
            [false, false]
        );
        // Case does not make a header different: both of these get the Age role.
        assert_eq!(
            safe_secret_wire_columns(&[("Name", ""), ("Age", ""), ("age", "")]),
            [true, false, false]
        );
        // A first column of format `name` holds the object's name, but its header is not safe.
        assert_eq!(safe_secret_wire_columns(&[("Token", "name")]), [false]);
    }

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
    fn server_error_text_hides_quoted_values_and_credentials() {
        let text = "denied: password \"hunter2-secret\" Authorization: Bearer abcdefghijklmnop";
        let shown = safe_server_error(text);
        assert!(!shown.contains("hunter2-secret"), "{shown}");
        assert!(!shown.contains("abcdefghijklmnop"), "{shown}");
        assert!(shown.starts_with("denied: password"), "{shown}");
    }

    #[test]
    fn server_error_text_hides_a_credential_past_the_summary_cut() {
        let token = "abcd1234".repeat(8);
        let text = format!("{} Authorization: Bearer {token}", "x".repeat(170));
        let shown = safe_server_error(&text);
        assert!(!shown.contains("abcd1234"), "{shown}");
    }

    #[test]
    fn server_error_text_hides_the_value_after_a_bare_bearer() {
        for (text, value) in [
            (
                "denied for Bearer abcdefghijklmnop upstream",
                "abcdefghijklmnop",
            ),
            ("denied for bearer Xy upstream", "Xy"),
            ("denied: BEARER ab12.cd-34_ef~5+/=", "ab12"),
            ("denied: bearer: lettersonly", "lettersonly"),
            ("denied: bearer=lettersonly", "lettersonly"),
            ("denied: bearer 'two words'", "two words"),
            ("denied: bearer token lettersonly", "lettersonly"),
            ("denied: bearer auth token: lettersonly", "lettersonly"),
        ] {
            let shown = safe_server_error(text);
            assert!(!shown.contains(value), "{text}: {shown}");
            assert!(shown.starts_with("denied"), "{text}: {shown}");
        }
    }

    #[test]
    fn server_error_text_keeps_sentences_about_bearer_auth() {
        for text in [
            "missing bearer token in request",
            "Bearer authentication is enabled",
            "the bearer token expired",
            "bearer token.",
            "auth scheme Bearer",
            "forbearer stays",
            "Authorization: Bearer [REDACTED:authorization]",
        ] {
            assert_eq!(safe_server_error(text), text);
        }
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
