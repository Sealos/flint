//! The structure of a fixtures directory, apart from its values, to tell when the fake API server
//! has drifted from a real one (#128): a fresh recording from kind is compared with the committed
//! fixtures (`fixture-drift`, run nightly in CI).
//!
//! The structure is what the fake serves and the tests rely on:
//!
//! - the files recorded: which kinds have Tables and objects, and which OpenAPI documents;
//! - discovery: every group, version and resource, with its kind, scope, verbs, short names and
//!   subresources, and the fields a discovery entry has;
//! - each Table's columns (name, type, format, priority) and the fields of a Table and its rows;
//! - the fields of `/version`, and the kind of each recorded object list (not the fields of the
//!   objects, which depend on their state).
//!
//! Values are left out: names, cells, counts, versions, timestamps and descriptions differ
//! between two recordings of the same cluster.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::Value;

/// The key prefix of a Table's row fields, which exist only when it has rows.
const ROW_FIELDS: &str = "row-fields ";

/// One structural fact per key (`columns tables/apps/v1/deployments.json`), with its value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shape(BTreeMap<String, String>);

impl Shape {
    /// The structure of the fixtures in `dir`.
    pub fn of(dir: &Path) -> Result<Shape> {
        let mut shape = Shape::default();
        let mut files = Vec::new();
        collect_json(dir, dir, &mut files)?;
        for rel in files {
            let path = dir.join(&rel);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let json: Value = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            shape.insert(format!("file {rel}"), String::new());
            if rel == "version.json" {
                shape.insert("fields version.json".into(), keys(&json));
            } else if rel.starts_with("discovery/") {
                shape.discovery(&rel, &json);
            } else if rel.starts_with("tables/") {
                shape.table(&rel, &json);
            } else if rel.starts_with("objects/") {
                shape.insert(format!("list-kind {rel}"), text_of(&json["kind"]));
            }
        }
        Ok(shape)
    }

    fn insert(&mut self, key: String, value: String) {
        self.0.insert(key, value);
    }

    /// Aggregated discovery (`APIGroupDiscoveryList`): one entry per group, version and
    /// resource, and the union of the fields of each level.
    fn discovery(&mut self, rel: &str, json: &Value) {
        let mut group_fields = BTreeSet::new();
        let mut version_fields = BTreeSet::new();
        let mut resource_fields = BTreeSet::new();
        for group in array(&json["items"]) {
            group_fields.extend(object_keys(group));
            let name = text_of(&group["metadata"]["name"]);
            for version in array(&group["versions"]) {
                version_fields.extend(object_keys(version));
                let v = text_of(&version["version"]);
                for resource in array(&version["resources"]) {
                    resource_fields.extend(object_keys(resource));
                    let subresources: Vec<String> = array(&resource["subresources"])
                        .map(|s| text_of(&s["subresource"]))
                        .collect();
                    let sorted = |field: &str| {
                        let mut list: Vec<String> = array(&resource[field]).map(text_of).collect();
                        list.sort();
                        list.join(",")
                    };
                    self.insert(
                        format!(
                            "resource {}/{v}/{}",
                            if name.is_empty() { "core" } else { &name },
                            text_of(&resource["resource"])
                        ),
                        format!(
                            "kind={} scope={} verbs=[{}] short=[{}] subresources=[{}]",
                            text_of(&resource["responseKind"]["kind"]),
                            text_of(&resource["scope"]),
                            sorted("verbs"),
                            sorted("shortNames"),
                            subresources.join(",")
                        ),
                    );
                }
            }
        }
        self.insert(format!("fields {rel}"), keys(json));
        self.insert(format!("group-fields {rel}"), join(&group_fields));
        self.insert(format!("version-fields {rel}"), join(&version_fields));
        self.insert(format!("resource-fields {rel}"), join(&resource_fields));
    }

    /// A Table list: its columns, and the fields of the Table and of its rows.
    fn table(&mut self, rel: &str, json: &Value) {
        let columns: Vec<String> = array(&json["columnDefinitions"])
            .map(|c| {
                format!(
                    "{}:{}:{}:{}",
                    text_of(&c["name"]),
                    text_of(&c["type"]),
                    text_of(&c["format"]),
                    c["priority"].as_i64().unwrap_or(0)
                )
            })
            .collect();
        self.insert(format!("columns {rel}"), columns.join(" | "));
        self.insert(format!("fields {rel}"), keys(json));
        let row_fields: BTreeSet<String> = array(&json["rows"]).flat_map(object_keys).collect();
        // A kind with no objects has no rows to say anything about.
        if !row_fields.is_empty() {
            self.insert(format!("{ROW_FIELDS}{rel}"), join(&row_fields));
        }
    }

    /// What changed from `self` (the committed fixtures) to `recorded`, one line each, or
    /// nothing.
    pub fn diff(&self, recorded: &Shape) -> Vec<String> {
        let mut lines = Vec::new();
        for (key, value) in &self.0 {
            match recorded.0.get(key) {
                // Rows depend on what the cluster holds at the time: compared when both have some.
                None if key.starts_with(ROW_FIELDS) => {}
                None => lines.push(format!("- {key}")),
                Some(new) if new != value => lines.push(format!(
                    "~ {key}\n    committed: {value}\n    recorded:  {new}"
                )),
                Some(_) => {}
            }
        }
        for key in recorded.0.keys() {
            if !self.0.contains_key(key) && !key.starts_with(ROW_FIELDS) {
                lines.push(format!("+ {key}"));
            }
        }
        lines
    }
}

/// A report of `diff`'s lines for a person reading the CI log.
pub fn report(lines: &[String]) -> String {
    let mut text = format!(
        "{} structural difference(s) between the committed fixtures and a fresh recording \
         (- only committed, + only recorded, ~ changed):",
        lines.len()
    );
    for line in lines {
        let _ = write!(text, "\n{line}");
    }
    text
}

/// The `.json` files below `dir`, relative to `root`, with `/` separators, sorted.
fn collect_json(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_json(root, &path, out)?;
        } else if path.extension().is_some_and(|e| e == "json") {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push(parts.join("/"));
        }
    }
    Ok(())
}

fn array(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

fn object_keys(value: &Value) -> impl Iterator<Item = String> + '_ {
    value
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys().cloned())
}

/// The sorted top-level fields of `value`.
fn keys(value: &Value) -> String {
    join(&object_keys(value).collect())
}

fn join(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(",")
}

fn text_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_api::fixtures::FIXTURES_DIR;

    /// A copy of the committed fixtures, to change.
    fn copy() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(Path::new(FIXTURES_DIR), dir.path());
        dir
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn edit(dir: &Path, rel: &str, change: impl FnOnce(&mut Value)) {
        let path = dir.join(rel);
        let mut json: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        change(&mut json);
        std::fs::write(path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    }

    fn committed() -> Shape {
        Shape::of(Path::new(FIXTURES_DIR)).unwrap()
    }

    #[test]
    fn the_committed_fixtures_have_the_structure_the_fake_serves() {
        let shape = committed();
        assert!(shape.diff(&shape).is_empty());
        assert_eq!(
            shape.0["resource apps/v1/deployments"],
            "kind=Deployment scope=Namespaced \
             verbs=[create,delete,deletecollection,get,list,patch,update,watch] short=[deploy] \
             subresources=[scale,status]"
        );
        assert!(
            shape.0["columns tables/apps/v1/deployments.json"]
                .starts_with("Name:string:name:0 | Ready:string::0"),
            "{}",
            shape.0["columns tables/apps/v1/deployments.json"]
        );
        assert_eq!(
            shape.0["row-fields tables/apps/v1/deployments.json"],
            "cells,object"
        );
        assert_eq!(
            shape.0["list-kind objects/apps/v1/deployments.json"],
            "DeploymentList"
        );
        assert!(shape.0.contains_key("file openapi/v3/apis/apps/v1.json"));
    }

    #[test]
    fn values_never_count_as_drift() {
        let dir = copy();
        edit(dir.path(), "tables/apps/v1/deployments.json", |t| {
            t["rows"][0]["cells"][1] = "0/9".into();
            t["rows"].as_array_mut().unwrap().truncate(1);
        });
        // No pods at all this time: no rows to compare.
        edit(dir.path(), "tables/core/v1/pods.json", |t| {
            t["rows"].as_array_mut().unwrap().clear();
        });
        edit(dir.path(), "tables/apps/v1/deployments.json", |t| {
            t["columnDefinitions"][0]["description"] = "reworded".into();
            t["metadata"]["resourceVersion"] = "999".into();
        });
        edit(dir.path(), "version.json", |v| {
            v["gitVersion"] = "v9.99.0".into();
        });
        edit(dir.path(), "discovery/apis.json", |d| {
            d["items"][0]["metadata"]["resourceVersion"] = "42".into();
            d["items"][0]["versions"][0]["freshness"] = "Stale".into();
        });
        edit(dir.path(), "objects/apps/v1/deployments.json", |o| {
            o["items"].as_array_mut().unwrap().clear();
        });
        assert_eq!(
            committed().diff(&Shape::of(dir.path()).unwrap()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn structural_changes_are_reported() {
        let dir = copy();
        edit(dir.path(), "tables/apps/v1/deployments.json", |t| {
            t["columnDefinitions"][1]["type"] = "integer".into();
            for row in t["rows"].as_array_mut().unwrap() {
                row.as_object_mut().unwrap().remove("object");
            }
        });
        edit(dir.path(), "discovery/apis.json", |d| {
            let resource = &mut d["items"][0]["versions"][0]["resources"][0];
            resource["scope"] = "Namespaced".into();
            resource["newField"] = true.into();
            // A verb lost: the fake would refuse watches.
            resource["verbs"] = serde_json::json!(["get", "list"]);
        });
        edit(dir.path(), "version.json", |v| {
            v.as_object_mut().unwrap().remove("platform");
        });
        std::fs::remove_file(dir.path().join("tables/batch/v1/jobs.json")).unwrap();
        std::fs::write(dir.path().join("tables/apps/v1/new.json"), "{}").unwrap();

        let lines = committed().diff(&Shape::of(dir.path()).unwrap());
        let text = report(&lines);
        for expected in [
            "~ columns tables/apps/v1/deployments.json",
            "~ row-fields tables/apps/v1/deployments.json",
            "~ resource apiregistration.k8s.io/v1/apiservices",
            "~ resource-fields discovery/apis.json",
            "~ fields version.json",
            "- file tables/batch/v1/jobs.json",
            "- columns tables/batch/v1/jobs.json",
            "+ file tables/apps/v1/new.json",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(text.contains("Ready:integer::0"), "{text}");
        assert!(text.contains("scope=Namespaced verbs=[get,list]"), "{text}");
        assert!(text.starts_with(&format!("{} structural difference(s)", lines.len())));
    }
}
