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
//! - No error names a value. Keys are named, as the review's diff shows them.

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

#[cfg(test)]
#[path = "secret_edit_tests.rs"]
mod tests;
