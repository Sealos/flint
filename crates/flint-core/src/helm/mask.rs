//! Hiding secrets in a release's values and manifest before anything shows them.
//!
//! Secret objects are masked whole ([`crate::redact::mask_secret`]). Elsewhere masking goes by
//! key name and by the shape of the value ([`crate::redact::redact_credentials`]), so it errs
//! towards hiding and a credential in an unusual format can still get through. What Values hides
//! is also hidden wherever a template copied it into the manifest ([`HiddenValues`]). A manifest
//! document that cannot be parsed, or that still shows a hidden value, is withheld.

// module-tests: ../helm_tests.rs

use std::collections::HashSet;

use base64::Engine as _;
use serde_json::Value;

use super::HelmError;
use crate::redact::{is_secret_object, mask_secret, redact_credentials};

/// What stands in for a hidden value.
pub const MASK: &str = "••••••";
/// Stands in for a manifest document that could not be parsed.
const WITHHELD: &str = "# Withheld: Flint could not parse this document to mask its secrets.\n";
/// The shortest hidden value searched for inside longer text. A shorter one is only hidden where
/// it is a whole value: inside text it would match by chance.
const MIN_NEEDLE: usize = 4;

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

/// The values `helm get values` hides, to hide again where a manifest repeats them.
#[derive(Debug, Default)]
pub(super) struct HiddenValues {
    whole: HashSet<String>,
    /// As written and in base64, longest first.
    needles: Vec<String>,
}

impl HiddenValues {
    /// From `helm get values --output=json`.
    pub(super) fn from_output(out: &[u8]) -> Result<Self, HelmError> {
        let values: Value = serde_json::from_slice(out).map_err(|_| HelmError::BadOutput)?;
        let mut hidden = Self::default();
        hidden.collect(&values);
        hidden.needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        hidden.needles.dedup();
        Ok(hidden)
    }

    fn add(&mut self, text: String) {
        if text.len() >= MIN_NEEDLE {
            self.needles
                .push(base64::engine::general_purpose::STANDARD.encode(&text));
            self.needles.push(text.clone());
        }
        if !text.is_empty() {
            self.whole.insert(text);
        }
    }

    /// Every scalar at or below `value`.
    fn add_all(&mut self, value: &Value) {
        match value {
            Value::String(text) => self.add(text.clone()),
            Value::Number(number) => self.add(number.to_string()),
            Value::Array(items) => items.iter().for_each(|item| self.add_all(item)),
            Value::Object(map) => map.values().for_each(|item| self.add_all(item)),
            Value::Null | Value::Bool(_) => {}
        }
    }

    /// What [`mask_tree`] hides by key. A field naming another object is left out: the same
    /// name in a manifest is that object's name.
    fn collect(&mut self, value: &Value) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| self.collect(item)),
            Value::Object(map) => {
                if env_style_is_sensitive(map)
                    && let Some(item) = map.get("value")
                {
                    self.add_all(item);
                }
                for (key, item) in map {
                    match (is_sensitive(key), names_another_object(key)) {
                        (true, true) => {}
                        (true, false) => self.add_all(item),
                        (false, _) => self.collect(item),
                    }
                }
            }
            _ => {}
        }
    }

    fn scrub(&self, value: &mut Value) {
        match value {
            Value::String(text) => {
                if self.whole.contains(text.as_str()) {
                    MASK.clone_into(text);
                    return;
                }
                for needle in &self.needles {
                    if text.contains(needle.as_str()) {
                        *text = text.replace(needle.as_str(), MASK);
                    }
                }
            }
            Value::Number(number) => {
                if self.whole.contains(&number.to_string()) {
                    *value = Value::String(MASK.to_owned());
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.scrub(item)),
            Value::Object(map) => map.values_mut().for_each(|item| self.scrub(item)),
            Value::Null | Value::Bool(_) => {}
        }
    }

    /// Whether `text` still shows a hidden value, in a key or a comment, say.
    fn shows(&self, text: &str) -> bool {
        self.needles.iter().any(|n| text.contains(n.as_str()))
    }

    /// `helm get manifest` output with its secrets and these values masked, document by document.
    pub(super) fn mask_manifest(&self, manifest: &str) -> String {
        split_documents(manifest)
            .iter()
            .filter(|d| !d.trim().is_empty())
            .map(|d| format!("---\n{}", self.mask_document(d)))
            .collect()
    }

    /// One document of a manifest, masked, with its `# Source:` line kept.
    fn mask_document(&self, document: &str) -> String {
        let source: String = document
            .lines()
            .take_while(|line| line.starts_with('#') || line.trim().is_empty())
            .filter(|line| line.starts_with("# Source:"))
            .map(|line| format!("{line}\n"))
            .collect();
        let masked = match serde_saphyr::from_str::<Value>(document) {
            Err(_) => format!("{source}{WITHHELD}"),
            Ok(object) if object.is_null() => source,
            Ok(mut object) => {
                mask_object(&mut object);
                self.scrub(&mut object);
                match serde_saphyr::to_string(&object) {
                    Ok(yaml) => format!("{source}{yaml}"),
                    Err(_) => format!("{source}{WITHHELD}"),
                }
            }
        };
        if self.shows(&masked) {
            WITHHELD.to_owned()
        } else {
            masked
        }
    }
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
                    (field, item @ (Value::Object(_) | Value::Array(_)))
                        if is_sensitive(field) && !names_another_object(field) =>
                    {
                        let field = normalized(field);
                        hide_subtree(
                            item,
                            field.ends_with("secret") || field.ends_with("secrets"),
                        );
                    }
                    (_, item) => mask_fields(item),
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Hides a whole subtree under a credential field. References to a Secret stay readable:
/// `secretName`, anything under a `...Ref`, and, when the field itself holds Secrets
/// (`imagePullSecrets`, a volume's `secret`), their `name` and `namespace`.
fn hide_subtree(value: &mut Value, names_secrets: bool) {
    match value {
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| hide_subtree(item, names_secrets)),
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let field = normalized(key);
                let reference = field == "secretname"
                    || (names_secrets && matches!(field.as_str(), "name" | "namespace"));
                if reference && matches!(item, Value::String(_)) {
                    continue;
                }
                if field.ends_with("ref") && item.is_object() {
                    mask_fields(item);
                } else {
                    hide_subtree(item, names_secrets);
                }
            }
        }
        Value::String(_) | Value::Number(_) => *value = Value::String(MASK.to_owned()),
        Value::Null | Value::Bool(_) => {}
    }
}

/// `helm get manifest` output with its secrets masked, document by document.
pub fn mask_manifest(manifest: &str) -> String {
    HiddenValues::default().mask_manifest(manifest)
}

/// The documents of a manifest, split at its `---` lines.
fn split_documents(manifest: &str) -> Vec<String> {
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
}
