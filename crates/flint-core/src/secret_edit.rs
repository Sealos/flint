//! Editing a Secret whose values the editor shows masked (#394).
//!
//! The editor shows every `data` and `stringData` value of a Secret, and every annotation value
//! outside the allowlist, as [`MASK`] ([`redact::mask_secret`]). A mask left in place means
//! "keep the value on the cluster", and is never sent:
//!
//! - The write is a JSON Merge Patch of only what the edit changes, built against the live
//!   object and carrying its `uid` and `resourceVersion` as preconditions. An untouched value is
//!   not in it, so it stays as the cluster has it; a concurrent change is a conflict.
//! - A mask under a key the live object does not have in that field (a new key, a renamed key,
//!   a key moved between `data` and `stringData`) is refused: there is no value to keep.
//! - A changed `data` value must be base64, as Kubernetes stores it. Plain text goes under
//!   `stringData`, which the server encodes into `data`.
//! - A patch that would still carry a mask, or a review's hidden-value placeholder, is refused.
//! - No error names a value. Keys are named, as the review's diff shows them. A refusal from the
//!   server (validation, an admission webhook or policy) is reported by its code, a known reason,
//!   the webhook's name and the fields it names that the patch has, never by its message: a
//!   webhook can echo a value as bare text, which no redaction can tell from words.
//! - Kubernetes writes `stringData` over `data`: [`effective_data`] is what a Secret holds after.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::redact;
use crate::write::{HIDDEN_CHANGED, HIDDEN_VALUE, WriteError, WriteTarget};

/// What the editor shows in place of each hidden value.
pub const MASK: &str = redact::SECRET_MASK;

/// The maps whose values the editor masks: how an error names each, and where it is.
const MASKED: [(&str, &str); 3] = [
    ("data", "/data"),
    ("stringData", "/stringData"),
    ("annotation", "/metadata/annotations"),
];

/// What the server sets in `metadata`: never part of an edit's patch.
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

/// `edited` with each mask the live object has a value for replaced by that value, the rest as
/// typed: what the edit means, for comparing it with the live object. Never sent.
pub fn with_live_values(edited: &Value, live: &Value) -> Value {
    let mut resolved = edited.clone();
    for (_, pointer) in MASKED {
        let Some(map) = resolved.pointer_mut(pointer).and_then(Value::as_object_mut) else {
            continue;
        };
        for (key, value) in map.iter_mut() {
            if value.as_str() == Some(MASK)
                && let Some(kept) = live_value(live, pointer, key)
            {
                *value = kept.clone();
            }
        }
    }
    resolved
}

fn live_value<'a>(live: &'a Value, pointer: &str, key: &str) -> Option<&'a Value> {
    live.pointer(pointer)
        .and_then(Value::as_object)
        .and_then(|map| map.get(key))
}

/// The merge patch that makes the live Secret what `edited` (the editor's text, masks and all)
/// says, with `target`'s `uid` and `resourceVersion` as preconditions. Refuses a mask it cannot
/// keep and a value that cannot be written, naming the key and never the value.
pub(crate) fn patch(
    edited: &Value,
    live: &Value,
    target: &WriteTarget,
) -> Result<Value, WriteError> {
    let (Some(uid), Some(version)) = (&target.uid, &target.resource_version) else {
        // Nothing to bind the patch to: a concurrent change could not be told apart.
        return Err(WriteError::Conflict);
    };
    check_values(edited, live)?;
    let resolved = with_live_values(edited, live);
    let mut patch = diff(&comparable(live), &comparable(&resolved))
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(fields) = patch.as_object_mut() else {
        return Err(invalid("the edit is not a Kubernetes object"));
    };
    let metadata = fields
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(metadata) = metadata.as_object_mut() else {
        return Err(invalid("metadata must be a map"));
    };
    metadata.insert("uid".to_owned(), uid.as_str().into());
    metadata.insert("resourceVersion".to_owned(), version.as_str().into());
    if holds_hidden(&patch) {
        return Err(invalid(
            "the change would write a hidden value's mask or placeholder; type each new value \
             in full",
        ));
    }
    Ok(patch)
}

/// Refuses a new Secret that holds a mask or a review's placeholder: there is no live value to
/// keep, so it would be written as the value. Names no value.
pub(crate) fn check_new(object: &Value) -> Result<(), WriteError> {
    for (field, pointer) in MASKED {
        if object.pointer(pointer).is_some_and(holds_hidden) {
            return Err(invalid(&format!(
                "{field} holds a hidden value's mask or placeholder, and a new Secret has no \
                 value to keep; type each value in full"
            )));
        }
    }
    Ok(())
}

/// Checks each masked map's entries: a mask must have a live value to keep, and a changed value
/// must be one the server takes as typed.
fn check_values(edited: &Value, live: &Value) -> Result<(), WriteError> {
    for (field, pointer) in MASKED {
        let Some(map) = edited.pointer(pointer) else {
            continue;
        };
        let map = match map {
            Value::Object(map) => map,
            // Removes the whole map, as removing its lines does.
            Value::Null => continue,
            _ => return Err(invalid(&format!("{field} must be a map of keys to values"))),
        };
        for (key, value) in map {
            let live = live_value(live, pointer, key);
            if Some(value) == live {
                continue;
            }
            let Some(text) = value.as_str() else {
                return Err(invalid(&format!(
                    "{field} {key:?} must be a string; quote its value"
                )));
            };
            if text == MASK {
                if live.is_some() {
                    continue;
                }
                return Err(invalid(&format!(
                    "{field} {key:?} still shows the hidden value, but the cluster has no value \
                     there to keep; type its value in full, or remove it"
                )));
            }
            if text == HIDDEN_VALUE || text == HIDDEN_CHANGED || looks_masked(text) {
                return Err(invalid(&format!(
                    "{field} {key:?} holds a hidden value's placeholder, not a value; type its \
                     value in full, or put the mask back to keep the value on the cluster"
                )));
            }
            if field == "data" && !is_base64(text) {
                return Err(invalid(&format!(
                    "data {key:?} is not base64; put a plain-text value under stringData, which \
                     the server encodes"
                )));
            }
        }
    }
    Ok(())
}

/// Whether `text` reads as a mask edited by accident (a bullet removed or added): two or more
/// of the mask's bullets in a row. Refused rather than written as a value.
fn looks_masked(text: &str) -> bool {
    let bullet = MASK.chars().next().expect("the mask is not empty");
    text.contains([bullet, bullet].iter().collect::<String>().as_str())
}

/// Whether the API server decodes `text` as a `data` value: standard base64, padded, line
/// breaks ignored.
fn is_base64(text: &str) -> bool {
    use base64::Engine as _;
    let joined: String = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
    base64::engine::general_purpose::STANDARD
        .decode(joined)
        .is_ok()
}

fn invalid(why: &str) -> WriteError {
    WriteError::InvalidYaml(why.to_owned())
}

/// `object` without what an edit does not set: `status` and the server's metadata.
fn comparable(object: &Value) -> Value {
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

/// The JSON Merge Patch (RFC 7396) from `from` to `to`, or `None` when they are the same. A
/// list is replaced whole, as a merge patch does.
fn diff(from: &Value, to: &Value) -> Option<Value> {
    if from == to {
        return None;
    }
    let (Value::Object(from), Value::Object(to)) = (from, to) else {
        return Some(to.clone());
    };
    let mut out = Map::new();
    for (key, value) in to {
        match from.get(key) {
            Some(old) => {
                if let Some(changed) = diff(old, value) {
                    out.insert(key.clone(), changed);
                }
            }
            None => {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    for key in from.keys().filter(|key| !to.contains_key(*key)) {
        out.insert(key.clone(), Value::Null);
    }
    Some(Value::Object(out))
}

/// Whether any key or string in `value` is, or holds, a mask or a review's placeholder.
fn holds_hidden(value: &Value) -> bool {
    let hidden = |text: &str| text.contains(MASK) || text == HIDDEN_VALUE || text == HIDDEN_CHANGED;
    match value {
        Value::String(text) => hidden(text),
        Value::Array(items) => items.iter().any(holds_hidden),
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| hidden(key) || holds_hidden(value)),
        _ => false,
    }
}

/// `object` as the API server stores it: each `stringData` value encoded into `data` under its
/// key, over any `data` value there, and `stringData` gone. A value that is not a string stays
/// under `stringData`, which the server refuses.
pub fn effective_data(object: &Value) -> Value {
    use base64::Engine as _;
    let mut stored = object.clone();
    let Some(fields) = stored.as_object_mut() else {
        return stored;
    };
    let Some(Value::Object(plain)) = fields.remove("stringData") else {
        return stored;
    };
    let mut left = Map::new();
    for (key, value) in plain {
        match value.as_str() {
            Some(text) => {
                let data = fields
                    .entry("data")
                    .or_insert_with(|| Value::Object(Map::new()));
                if !data.is_object() {
                    *data = Value::Object(Map::new());
                }
                let encoded = base64::engine::general_purpose::STANDARD.encode(text);
                data[key.as_str()] = encoded.into();
            }
            None => {
                left.insert(key, value);
            }
        }
    }
    if !left.is_empty() {
        fields.insert("stringData".to_owned(), Value::Object(left));
    }
    stored
}

/// The reasons the API server sends: any other is the webhook's or a proxy's own text.
const KNOWN_REASONS: [&str; 18] = [
    "AlreadyExists",
    "BadRequest",
    "Conflict",
    "Expired",
    "Forbidden",
    "Gone",
    "InternalError",
    "Invalid",
    "MethodNotAllowed",
    "NotAcceptable",
    "NotFound",
    "RequestEntityTooLarge",
    "ServerTimeout",
    "ServiceUnavailable",
    "Timeout",
    "TooManyRequests",
    "Unauthorized",
    "UnsupportedMediaType",
];

/// A failed request of a Secret's write, from what cannot hold a value: the status code, a
/// reason the API server defines, the admission webhook's name, and the fields named that are
/// the patch's own keys. The server's message is never shown.
pub(crate) fn write_error(error: kube::Error, patch: &Value) -> WriteError {
    let kube::Error::Api(status) = error else {
        // Its text can carry a body that was not the API server's.
        return WriteError::Failed(
            "the request for this Secret failed, and its details are not shown because they may \
             quote a value; the change may or may not have applied"
                .to_owned(),
        );
    };
    match status.code {
        404 => return WriteError::Gone,
        409 => return WriteError::Conflict,
        _ => {}
    }
    let mut what = crate::api_error::status_meaning(status.code, None);
    if KNOWN_REASONS.contains(&status.reason.as_str()) {
        what.push_str(&format!(" [{}]", status.reason));
    }
    if let Some(webhook) = webhook_name(&status.message) {
        what.push_str(&format!("; admission webhook \"{webhook}\" denied it"));
    }
    let known = patch_fields(patch);
    let fields: BTreeSet<&str> = status
        .details
        .iter()
        .flat_map(|details| &details.causes)
        .map(|cause| cause.field.as_str())
        .filter(|field| known.contains(*field))
        .collect();
    if !fields.is_empty() {
        let named: Vec<&str> = fields.into_iter().collect();
        what.push_str(&format!("; fields: {}", named.join(", ")));
    }
    WriteError::Rejected(format!(
        "{what}. The server's message is not shown, because it may quote a Secret value"
    ))
}

/// The name in `admission webhook "<name>" denied the request`, if it is a DNS name, as a
/// webhook's must be.
fn webhook_name(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("admission webhook \"")?;
    let (name, rest) = rest.split_once('"')?;
    let dns = !name.is_empty()
        && name.len() <= 253
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    (dns && rest.starts_with(" denied the request")).then_some(name)
}

/// The field paths a server can name for what `patch` sets: its masked maps, each of their keys
/// in both of the API server's spellings, and its other top-level fields.
fn patch_fields(patch: &Value) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    let Some(top) = patch.as_object() else {
        return fields;
    };
    for key in top.keys().filter(|key| *key != "metadata") {
        fields.insert(key.clone());
    }
    for (path, pointer) in [
        ("data", "/data"),
        ("stringData", "/stringData"),
        ("metadata.annotations", "/metadata/annotations"),
    ] {
        let Some(map) = patch.pointer(pointer).and_then(Value::as_object) else {
            continue;
        };
        fields.insert(path.to_owned());
        for key in map.keys() {
            fields.insert(format!("{path}[{key}]"));
            fields.insert(format!("{path}.{key}"));
        }
    }
    fields
}

#[cfg(test)]
#[path = "secret_edit_tests.rs"]
mod tests;
