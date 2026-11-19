//! One Table row as the API server sends it with `includeObject=Metadata`: cells plus a
//! `PartialObjectMetadata`. Tests take rows from the fixtures and change them to script watch
//! events.

use serde_json::{Value, json};

use crate::record::normalize::stable_uid;

/// A Table row (`{"cells": [...], "object": {"kind": "PartialObjectMetadata", ...}}`).
#[derive(Debug, Clone, PartialEq)]
pub struct FakeRow(Value);

impl FakeRow {
    /// Wraps a row as found in a Table's `rows`.
    pub fn new(row: Value) -> Self {
        FakeRow(row)
    }

    /// A row with these cells for an object that exists only in the test.
    pub fn build(namespace: Option<&str>, name: &str, cells: Vec<Value>) -> Self {
        let mut metadata = json!({
            "name": name,
            "uid": stable_uid(&format!("test/{}/{name}", namespace.unwrap_or(""))),
            "resourceVersion": "1",
            "creationTimestamp": "2026-01-01T00:00:00Z",
        });
        if let Some(ns) = namespace {
            metadata["namespace"] = ns.into();
        }
        FakeRow(json!({
            "cells": cells,
            "object": {"kind": "PartialObjectMetadata", "apiVersion": "meta.k8s.io/v1", "metadata": metadata},
        }))
    }

    fn meta(&self, key: &str) -> Option<&str> {
        self.0
            .pointer(&format!("/object/metadata/{key}"))
            .and_then(Value::as_str)
    }

    pub fn name(&self) -> &str {
        self.meta("name").unwrap_or("")
    }

    pub fn namespace(&self) -> Option<&str> {
        self.meta("namespace")
    }

    pub fn uid(&self) -> &str {
        self.meta("uid").unwrap_or("")
    }

    pub fn resource_version(&self) -> &str {
        self.meta("resourceVersion").unwrap_or("")
    }

    pub fn cells(&self) -> &[Value] {
        self.0
            .get("cells")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// The same object under another name: a new uid, and the name cell follows.
    pub fn renamed(mut self, name: &str) -> Self {
        let old = self.name().to_owned();
        let uid = stable_uid(&format!(
            "renamed/{}/{name}",
            self.namespace().unwrap_or("")
        ));
        if let Some(meta) = self.metadata_mut() {
            meta.insert("name".into(), name.into());
            meta.insert("uid".into(), uid.into());
            meta.remove("generateName");
        }
        if let Some(first) = self.cells_mut().and_then(|c| c.first_mut())
            && first.as_str() == Some(old.as_str())
        {
            *first = name.into();
        }
        self
    }

    /// Replaces one cell.
    pub fn with_cell(mut self, index: usize, value: impl Into<Value>) -> Self {
        if let Some(cell) = self.cells_mut().and_then(|c| c.get_mut(index)) {
            *cell = value.into();
        }
        self
    }

    /// Sets a label on the row's metadata.
    pub fn with_label(mut self, key: &str, value: &str) -> Self {
        if let Some(meta) = self.metadata_mut() {
            let labels = meta.entry("labels").or_insert_with(|| json!({}));
            if let Some(labels) = labels.as_object_mut() {
                labels.insert(key.into(), value.into());
            }
        }
        self
    }

    pub fn as_json(&self) -> &Value {
        &self.0
    }

    pub(crate) fn set_resource_version(&mut self, rv: u64) {
        if let Some(meta) = self.metadata_mut() {
            meta.insert("resourceVersion".into(), rv.to_string().into());
        }
    }

    /// Whether this row is the object `namespace`/`name`.
    pub(crate) fn is(&self, namespace: Option<&str>, name: &str) -> bool {
        self.namespace() == namespace && self.name() == name
    }

    fn metadata_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        self.0
            .pointer_mut("/object/metadata")
            .and_then(Value::as_object_mut)
    }

    fn cells_mut(&mut self) -> Option<&mut Vec<Value>> {
        self.0.get_mut("cells").and_then(Value::as_array_mut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> FakeRow {
        FakeRow::new(json!({
            "cells": ["web", "2/2", 2, "1d"],
            "object": {"kind": "PartialObjectMetadata", "apiVersion": "meta.k8s.io/v1",
                       "metadata": {"name": "web", "namespace": "demo", "uid": "u1", "resourceVersion": "1",
                                    "generateName": "web-"}},
        }))
    }

    #[test]
    fn reads_identity_and_cells() {
        let row = web();
        assert_eq!(
            (row.namespace(), row.name(), row.uid()),
            (Some("demo"), "web", "u1")
        );
        assert_eq!(row.cells().len(), 4);
        assert!(row.is(Some("demo"), "web"));
        assert!(!row.is(None, "web"));
    }

    #[test]
    fn renaming_gives_a_new_object() {
        let row = web()
            .renamed("web-2")
            .with_cell(1, "0/2")
            .with_label("app", "web");
        assert_eq!(row.name(), "web-2");
        assert_ne!(row.uid(), "u1");
        assert_eq!(row.cells()[0], "web-2");
        assert_eq!(row.cells()[1], "0/2");
        assert_eq!(row.as_json()["object"]["metadata"]["labels"]["app"], "web");
        assert!(
            row.as_json()["object"]["metadata"]
                .get("generateName")
                .is_none()
        );
        let mut row = row.with_cell(99, "ignored");
        row.set_resource_version(42);
        assert_eq!(row.resource_version(), "42");
    }

    #[test]
    fn built_rows_are_complete() {
        let row = FakeRow::build(Some("demo"), "cm", vec!["cm".into(), 1.into()]);
        assert_eq!(row.namespace(), Some("demo"));
        assert!(!row.uid().is_empty());
        assert_eq!(FakeRow::build(None, "n", vec![]).namespace(), None);
    }
}
