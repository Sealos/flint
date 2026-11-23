//! Compares one object across clusters (#449), matched by kind, namespace and name.
//!
//! Only reads what a front end already fetched: nothing here connects, runs a credential command
//! or writes. A Secret's hidden values are dropped before fields are compared, so a result cannot
//! say whether two of them are equal. Fields that differ on every cluster by nature (`uid`,
//! `resourceVersion`, `status`, ...) are left out unless asked for.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

use crate::read::{KindId, ObjectDetail, with_managed_fields};
use crate::redact;

/// `metadata` fields each cluster assigns for itself.
const VOLATILE_METADATA: &[&str] = &[
    "creationTimestamp",
    "deletionTimestamp",
    "generation",
    "managedFields",
    "resourceVersion",
    "selfLink",
    "uid",
];

/// Annotations that record a cluster's own history rather than the object's configuration.
const VOLATILE_ANNOTATIONS: &[&str] = &[
    "deployment.kubernetes.io/revision",
    "kubectl.kubernetes.io/last-applied-configuration",
];

/// What the server's error says for an object that does not exist, after
/// [`crate::cluster::ClusterError::summary`] hid the quoted name.
const HIDDEN_NAME: &str = "\"…\"";

/// Why a side is unavailable when its kind has another scope there, so the read found a different
/// object from the one compared.
pub const OTHER_SCOPE: &str = "this kind has a different scope on this cluster";

/// Ends the path of the row holding a named list's order. No other path can end like this: keys
/// are written `.key` or `["key"]`, and items `[0]` or `[name="..."]`.
const ORDER: &str = "[order]";

/// The object compared on each cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareTarget {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
}

/// What one cluster answered.
#[derive(Debug, Clone)]
pub enum SideRead {
    Reading,
    Present(Arc<ObjectDetail>),
    /// The cluster answered that it has no such object.
    Missing,
    /// The cluster could not say, with why, as safe to show.
    Unavailable(String),
}

impl SideRead {
    /// Reads the answer of [`crate::CoreHandle::fetch_object`]. Only the API server's own "not
    /// found" for this kind counts as missing: anything else leaves the object's existence
    /// unknown. An object outside the target's namespace is another object: a cluster where the
    /// kind is cluster-scoped ignores the namespace asked for.
    pub fn from_fetch(target: &CompareTarget, fetched: Result<Arc<ObjectDetail>, String>) -> Self {
        match fetched {
            Ok(detail) if namespace_of(&detail) != target.namespace.as_deref() => {
                Self::Unavailable(OTHER_SCOPE.to_owned())
            }
            Ok(detail) => Self::Present(detail),
            Err(error) if says_not_found(&target.kind, &error) => Self::Missing,
            Err(error) => Self::Unavailable(error),
        }
    }
}

fn namespace_of(detail: &ObjectDetail) -> Option<&str> {
    detail
        .object
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .filter(|namespace| !namespace.is_empty())
}

fn says_not_found(kind: &KindId, error: &str) -> bool {
    error
        .strip_prefix("NotFound: ")
        .and_then(|rest| rest.strip_suffix(" not found"))
        .and_then(|rest| rest.strip_suffix(HIDDEN_NAME))
        .is_some_and(|resource| resource.trim_end() == kind.as_str())
}

/// What a comparison includes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompareOptions {
    /// Keep the fields each cluster assigns for itself.
    pub include_volatile: bool,
}

/// A cluster's state in a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideState {
    Reading,
    Present,
    Missing,
    Unavailable(String),
}

/// One field on one cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Value(Value),
    /// Set, with a value that is never read.
    Hidden,
    /// The object is there without this field.
    NotSet,
    /// The cluster has no such object.
    NoObject,
    /// The cluster has not answered, or could not.
    Unknown,
}

/// How a field compares across the clusters that have the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Same,
    Differs,
    /// Set on every cluster compared, with values that are hidden and so not compared.
    Hidden,
    /// Fewer than two clusters have the object.
    NotCompared,
}

/// One field across the clusters, `values` in the order of the clusters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareRow {
    pub path: String,
    pub values: Vec<FieldValue>,
    pub state: RowState,
}

/// The comparison's overall answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Reading,
    /// Fewer than two clusters have a readable object.
    NotComparable,
    Differs {
        fields: usize,
    },
    /// The readable objects match, but a cluster lacks the object or could not be read.
    MatchOnSome,
    /// Everything comparable matches; these hidden fields were not compared.
    SameExceptHidden {
        fields: usize,
    },
    Same,
}

/// A whole comparison, `sides` in the order of the clusters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comparison {
    pub sides: Vec<SideState>,
    pub rows: Vec<CompareRow>,
}

impl Comparison {
    pub fn verdict(&self) -> Verdict {
        if self.sides.contains(&SideState::Reading) {
            return Verdict::Reading;
        }
        let present = self
            .sides
            .iter()
            .filter(|side| **side == SideState::Present)
            .count();
        if present < 2 {
            return Verdict::NotComparable;
        }
        let count = |state| self.rows.iter().filter(|row| row.state == state).count();
        match (count(RowState::Differs), count(RowState::Hidden)) {
            (fields @ 1.., _) => Verdict::Differs { fields },
            _ if present < self.sides.len() => Verdict::MatchOnSome,
            (0, fields @ 1..) => Verdict::SameExceptHidden { fields },
            _ => Verdict::Same,
        }
    }
}

/// Compares `target` as read on each cluster.
pub fn compare(target: &CompareTarget, reads: &[SideRead], options: CompareOptions) -> Comparison {
    let fields: Vec<Option<BTreeMap<String, Field>>> = reads
        .iter()
        .map(|read| match read {
            SideRead::Present(detail) => Some(fields_of(&target.kind, detail, options)),
            _ => None,
        })
        .collect();
    let paths: BTreeSet<&String> = fields.iter().flatten().flat_map(BTreeMap::keys).collect();
    let rows = paths
        .into_iter()
        .map(|path| {
            let values: Vec<FieldValue> = reads
                .iter()
                .zip(&fields)
                .map(|(read, fields)| value_at(read, fields.as_ref(), path))
                .collect();
            let state = row_state(path, reads, &values);
            CompareRow {
                path: path.clone(),
                values,
                state,
            }
        })
        // An order row only exists to report a reorder.
        .filter(|row| !row.path.ends_with(ORDER) || row.state == RowState::Differs)
        .collect();
    Comparison {
        sides: reads.iter().map(side_state).collect(),
        rows,
    }
}

fn side_state(read: &SideRead) -> SideState {
    match read {
        SideRead::Reading => SideState::Reading,
        SideRead::Present(_) => SideState::Present,
        SideRead::Missing => SideState::Missing,
        SideRead::Unavailable(why) => SideState::Unavailable(why.clone()),
    }
}

fn value_at(read: &SideRead, fields: Option<&BTreeMap<String, Field>>, path: &str) -> FieldValue {
    match (read, fields) {
        (SideRead::Missing, _) => FieldValue::NoObject,
        (_, Some(fields)) => match fields.get(path) {
            Some(Field::Value(value)) => FieldValue::Value(value.clone()),
            Some(Field::Hidden) => FieldValue::Hidden,
            None => FieldValue::NotSet,
        },
        _ => FieldValue::Unknown,
    }
}

fn row_state(path: &str, reads: &[SideRead], values: &[FieldValue]) -> RowState {
    let compared: Vec<&FieldValue> = reads
        .iter()
        .zip(values)
        .filter(|(read, _)| matches!(read, SideRead::Present(_)))
        .map(|(_, value)| value)
        .collect();
    let Some(first) = compared.first() else {
        return RowState::NotCompared;
    };
    if compared.len() < 2 {
        return RowState::NotCompared;
    }
    if path.ends_with(ORDER) {
        return order_state(&compared);
    }
    let not_set = |value: &&FieldValue| **value == FieldValue::NotSet;
    if compared.iter().any(not_set) {
        // Which keys exist is shown either way, so a key on one side only is a real difference.
        return RowState::Differs;
    }
    if compared.iter().any(|value| **value == FieldValue::Hidden) {
        return RowState::Hidden;
    }
    if compared.iter().all(|value| value == first) {
        RowState::Same
    } else {
        RowState::Differs
    }
}

/// Whether the names two clusters share come in a different order. An added or removed item is
/// already a row of its own, so it does not count as a reorder.
fn order_state(compared: &[&FieldValue]) -> RowState {
    let lists: Vec<&[Value]> = compared
        .iter()
        .filter_map(|value| match value {
            FieldValue::Value(Value::Array(names)) => Some(names.as_slice()),
            _ => None,
        })
        .collect();
    let reordered = lists.iter().enumerate().any(|(ix, first)| {
        lists[ix + 1..]
            .iter()
            .any(|second| shared(first, second) != shared(second, first))
    });
    if reordered {
        RowState::Differs
    } else {
        RowState::Same
    }
}

/// The names of `list` that `other` also has, in `list`'s order.
fn shared<'a>(list: &'a [Value], other: &[Value]) -> Vec<&'a str> {
    let other: BTreeSet<&str> = other.iter().filter_map(Value::as_str).collect();
    list.iter()
        .filter_map(Value::as_str)
        .filter(|name| other.contains(name))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Value(Value),
    Hidden,
}

/// The object's fields by path, with hidden values already gone.
fn fields_of(
    kind: &KindId,
    detail: &ObjectDetail,
    options: CompareOptions,
) -> BTreeMap<String, Field> {
    let is_secret =
        redact::is_secret_list(kind.as_str()) || redact::is_secret_object(&detail.object);
    let mut object = if options.include_volatile && !is_secret {
        with_managed_fields(&detail.object, detail.managed_fields.as_ref())
    } else {
        detail.object.clone()
    };
    if !options.include_volatile {
        strip_volatile(&mut object);
    }
    let mut out = BTreeMap::new();
    if is_secret {
        take_secret_values(&mut object, &mut out);
    }
    walk("", &object, &mut out);
    out
}

fn strip_volatile(object: &mut Value) {
    let Some(root) = object.as_object_mut() else {
        return;
    };
    root.remove("status");
    let Some(meta) = root.get_mut("metadata").and_then(Value::as_object_mut) else {
        return;
    };
    for field in VOLATILE_METADATA {
        meta.remove(*field);
    }
    if let Some(annotations) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
        for key in VOLATILE_ANNOTATIONS {
            annotations.remove(*key);
        }
        if annotations.is_empty() {
            meta.remove("annotations");
        }
    }
    if let Some(owners) = meta
        .get_mut("ownerReferences")
        .and_then(Value::as_array_mut)
    {
        for owner in owners.iter_mut().filter_map(Value::as_object_mut) {
            owner.remove("uid");
        }
    }
}

/// Moves what a Secret hides out of `object`, recording only that each key is set. Every
/// annotation is hidden, so neither what is drawn nor the verdict depends on one's value. A value
/// of an unexpected shape is hidden whole.
fn take_secret_values(object: &mut Value, out: &mut BTreeMap<String, Field>) {
    let Some(root) = object.as_object_mut() else {
        return;
    };
    for field in ["data", "stringData"] {
        if let Some(values) = root.remove(field) {
            hide(field, &values, out);
        }
    }
    let annotations = root
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .and_then(|meta| meta.remove("annotations"));
    if let Some(annotations) = annotations {
        hide("metadata.annotations", &annotations, out);
    }
}

/// Records each key of `values` at `path` as hidden.
fn hide(path: &str, values: &Value, out: &mut BTreeMap<String, Field>) {
    match values {
        Value::Object(map) if !map.is_empty() => {
            for key in map.keys() {
                out.insert(child_path(path, key), Field::Hidden);
            }
        }
        Value::Object(_) => {}
        _ => {
            out.insert(path.to_owned(), Field::Hidden);
        }
    }
}

fn walk(path: &str, value: &Value, out: &mut BTreeMap<String, Field>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                walk(&child_path(path, key), child, out);
            }
        }
        Value::Array(items) if !items.is_empty() => match item_names(items) {
            Some(names) => {
                let order = names.iter().map(|name| Value::from(*name)).collect();
                out.insert(format!("{path}{ORDER}"), Field::Value(Value::Array(order)));
                for (name, item) in names.into_iter().zip(items) {
                    let quoted = Value::from(name).to_string();
                    walk(&format!("{path}[name={quoted}]"), item, out);
                }
            }
            None => {
                for (index, item) in items.iter().enumerate() {
                    walk(&format!("{path}[{index}]"), item, out);
                }
            }
        },
        leaf => {
            out.insert(path.to_owned(), Field::Value(leaf.clone()));
        }
    }
}

/// Each item's `name`, when all have a different one: containers, ports and env vars then match
/// by name, so one inserted item doesn't shift the rest. Their order can matter (init containers
/// run in it, env vars refer to earlier ones), so it is compared as a row of its own.
fn item_names(items: &[Value]) -> Option<Vec<&str>> {
    let names: Vec<&str> = items
        .iter()
        .map(|item| item.get("name").and_then(Value::as_str))
        .collect::<Option<_>>()?;
    let unique: BTreeSet<&&str> = names.iter().collect();
    (unique.len() == names.len()).then_some(names)
}

/// `path.key`, or `path["key"]` for a key that is not a plain word, so that a key holding dots
/// (`app.kubernetes.io/name`) cannot read as a nested path.
fn child_path(path: &str, key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    match (plain, path.is_empty()) {
        (true, true) => key.to_owned(),
        (true, false) => format!("{path}.{key}"),
        (false, _) => format!("{path}[{}]", Value::from(key)),
    }
}

#[cfg(test)]
#[path = "compare_tests.rs"]
mod tests;
