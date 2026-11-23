//! Hiding secrets in a release's values and manifest before anything shows them.
//!
//! Secret objects are masked whole ([`crate::redact::mask_secret`]). Elsewhere masking goes by
//! key name and by the shape of the value ([`crate::redact::redact_credentials`]), so it errs
//! towards hiding and a credential in an unusual format can still get through. A manifest document
//! that cannot be parsed is withheld, since nothing could be said of what it holds.

// module-tests: ../helm_tests.rs

use serde_json::Value;

use super::HelmError;
use crate::redact::{is_secret_object, mask_secret, redact_credentials};

/// What stands in for a hidden value.
pub const MASK: &str = "••••••";
/// Stands in for a manifest document that could not be parsed.
const WITHHELD: &str = "# Withheld: Flint could not parse this document to mask its secrets.\n";

/// Parts of a normalized key that say its value is a credential.
const SENSITIVE: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "token",
    "credential",
    "apikey",
    "accesskey",
    "privatekey",
    "connectionstring",
    "dsn",
    "kubeconfig",
    "cookie",
    "authorization",
];

/// Lower case, without the separators key styles differ by.
fn normalized(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a value under `key` is hidden. A bare `key` is not: tolerations and selectors use it
/// for plain names, and a key's material is caught by its shape.
fn is_sensitive(key: &str) -> bool {
    let key = normalized(key);
    SENSITIVE.iter().any(|part| key.contains(part)) || (key.len() > 3 && key.ends_with("key"))
}

/// Hides every value under a sensitive key. Booleans and nulls stay: `auth.password: null` or
/// `tokenEnabled: true` say nothing of a credential.
fn hide(value: &mut Value) {
    match value {
        Value::Null | Value::Bool(_) => {}
        Value::Array(items) => items.iter_mut().for_each(hide),
        Value::Object(map) => map.values_mut().for_each(hide),
        Value::String(_) | Value::Number(_) => *value = Value::String(MASK.to_owned()),
    }
}

/// Hides credential-shaped text in `text`, in place.
fn redact_string(text: &mut String) {
    let redacted = redact_credentials(text);
    if redacted.count > 0 {
        let owned = redacted.text.into_owned();
        *text = owned;
    }
}

fn mask_tree(value: &mut Value) {
    match value {
        Value::String(text) => redact_string(text),
        Value::Array(items) => items.iter_mut().for_each(mask_tree),
        Value::Object(map) => {
            if env_style_is_sensitive(map) {
                map.get_mut("value").into_iter().for_each(hide);
            }
            for (key, item) in map.iter_mut() {
                if is_sensitive(key) {
                    hide(item);
                } else {
                    mask_tree(item);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// A `{name: DB_PASSWORD, value: x}` entry, as in a container's `env`.
fn env_style_is_sensitive(map: &serde_json::Map<String, Value>) -> bool {
    map.contains_key("value")
        && map
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(is_sensitive)
}

/// `helm get values` output as masked YAML.
pub fn mask_values(mut values: Value) -> Result<String, HelmError> {
    if values.is_null() {
        return Ok(String::new());
    }
    mask_tree(&mut values);
    serde_saphyr::to_string(&values).map_err(|_| HelmError::BadOutput)
}

/// Masks one object of a manifest. A `List` is masked item by item.
fn mask_object(object: &mut Value) {
    if let Some(items) = object.get_mut("items").and_then(Value::as_array_mut) {
        items.iter_mut().for_each(mask_object);
    }
    if is_secret_object(object) {
        mask_secret(object, MASK);
    }
    // Annotations can hold a whole copy of the object (last-applied configuration).
    if let Some(annotations) = object
        .get_mut("metadata")
        .and_then(|m| m.get_mut("annotations"))
        .and_then(Value::as_object_mut)
        && let Some(copy) = annotations.get_mut("kubectl.kubernetes.io/last-applied-configuration")
    {
        *copy = Value::String(MASK.to_owned());
    }
    mask_fields(object);
}

/// Whether a field names an object rather than holding a value: `secretName`, `tokenSecretRef`.
fn names_another_object(field: &str) -> bool {
    let field = normalized(field);
    field.ends_with("name") || field.ends_with("ref")
}

/// [`mask_tree`] for a Kubernetes object, where a sensitive field can be a reference to a Secret
/// (`secretName`, `secretKeyRef`), which stays readable. Any other scalar under a sensitive
/// field, `data` style maps, `env` entries and credential-shaped text are hidden.
fn mask_fields(value: &mut Value) {
    match value {
        Value::String(text) => redact_string(text),
        Value::Array(items) => items.iter_mut().for_each(mask_fields),
        Value::Object(map) => {
            if env_style_is_sensitive(map) {
                map.get_mut("value").into_iter().for_each(hide);
            }
            for (key, item) in map.iter_mut() {
                match (key.as_str(), item) {
                    ("data" | "stringData" | "binaryData", Value::Object(entries)) => {
                        for (entry, content) in entries.iter_mut() {
                            if is_sensitive(entry) {
                                hide(content);
                            } else {
                                mask_fields(content);
                            }
                        }
                    }
                    (field, item @ (Value::String(_) | Value::Number(_)))
                        if is_sensitive(field) && !names_another_object(field) =>
                    {
                        hide(item);
                    }
                    (_, item) => mask_fields(item),
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// One document of a manifest, masked, with its `# Source:` line kept.
fn mask_document(document: &str) -> String {
    let source: String = document
        .lines()
        .take_while(|line| line.starts_with('#') || line.trim().is_empty())
        .filter(|line| line.starts_with("# Source:"))
        .map(|line| format!("{line}\n"))
        .collect();
    let Ok(mut object) = serde_saphyr::from_str::<Value>(document) else {
        return format!("{source}{WITHHELD}");
    };
    if object.is_null() {
        return source;
    }
    mask_object(&mut object);
    match serde_saphyr::to_string(&object) {
        Ok(yaml) => format!("{source}{yaml}"),
        Err(_) => format!("{source}{WITHHELD}"),
    }
}

/// `helm get manifest` output with its secrets masked, document by document.
pub fn mask_manifest(manifest: &str) -> String {
    let mut documents = Vec::new();
    let mut current = String::new();
    for line in manifest.lines() {
        // A separator is `---` alone, or followed by a space and a comment.
        if line == "---" || line.starts_with("--- ") {
            documents.push(std::mem::take(&mut current));
            // Content on the separator's own line cannot be told from a comment safely.
            if let Some(rest) = line.strip_prefix("--- ")
                && !rest.trim_start().starts_with('#')
            {
                current.push_str(rest);
                current.push('\n');
            }
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    documents.push(current);
    documents
        .iter()
        .filter(|d| !d.trim().is_empty())
        .map(|d| format!("---\n{}", mask_document(d)))
        .collect()
}
