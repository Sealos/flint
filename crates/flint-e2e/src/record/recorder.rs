//! Records the fixtures from the dev cluster (see [`crate::fake_api::fixtures`] for the layout).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use kube::Client;
use serde_json::Value;

use super::normalize::{self, UidMap};
use crate::fake_api::fixtures::{self, Fixtures, Gvr, Layout};
use crate::fake_api::resources::{ResourceIndex, ResourceInfo};

/// Accept header for aggregated discovery (what kube sends).
const ACCEPT_DISCOVERY: &str =
    "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList,application/json";

/// Accept header for Table lists (what flint-core sends).
const ACCEPT_TABLE: &str = "application/json;as=Table;v=v1;g=meta.k8s.io, application/json";

/// OpenAPI v3 documents to record. The rest are left out to keep the fixtures small.
const OPENAPI_DOCUMENTS: &[&str] = &["apis/apps/v1"];

/// The namespace whose objects are recorded in full (`scripts/dev-cluster.sh` fills it).
pub const OBJECT_NAMESPACE: &str = "demo";

/// Events are recorded in these namespaces only (node events land in `default`); the rest of a
/// fresh cluster's events are kube-system noise and most of the bytes.
const EVENT_NAMESPACES: &[&str] = &["default", OBJECT_NAMESPACE];

/// Cluster-scoped resources whose objects are recorded in full.
const CLUSTER_OBJECTS: &[&str] = &["namespaces", "nodes"];

/// Where to record the objects of a resource from, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectScope {
    AllNamespaces,
    Namespace(&'static str),
}

/// Objects are recorded for the demo namespace, a few cluster-scoped resources, and core events
/// in every namespace. Metrics change on every scrape and are only recorded as Tables.
pub fn object_scope(resource: &ResourceInfo) -> Option<ObjectScope> {
    let gvr = &resource.gvr;
    if gvr.group == "metrics.k8s.io" || gvr.group == "events.k8s.io" {
        return None;
    }
    if gvr.group.is_empty() && gvr.resource == "events" {
        return Some(ObjectScope::AllNamespaces);
    }
    if resource.namespaced {
        Some(ObjectScope::Namespace(OBJECT_NAMESPACE))
    } else if gvr.group.is_empty() && CLUSTER_OBJECTS.contains(&gvr.resource.as_str()) {
        Some(ObjectScope::AllNamespaces)
    } else {
        None
    }
}

/// Keeps only the recorded documents in an OpenAPI v3 index.
pub fn filter_openapi_index(index: &mut Value) {
    if let Some(paths) = index.get_mut("paths").and_then(Value::as_object_mut) {
        paths.retain(|path, _| OPENAPI_DOCUMENTS.contains(&path.as_str()));
    }
}

/// What was recorded, by file, with its size in bytes.
#[derive(Debug, Default)]
pub struct Summary {
    pub files: BTreeMap<PathBuf, usize>,
}

struct Recording {
    version: Value,
    discovery_core: Value,
    discovery_groups: Value,
    openapi_index: Value,
    openapi: BTreeMap<String, Value>,
    tables: BTreeMap<Gvr, Value>,
    objects: BTreeMap<Gvr, Value>,
}

async fn get(client: &Client, path: &str, accept: &str) -> Result<Value> {
    let request = http::Request::get(path)
        .header(http::header::ACCEPT, accept)
        .body(Vec::new())?;
    let text = client
        .request_text(request)
        .await
        .with_context(|| format!("GET {path}"))?;
    serde_json::from_str(&text).with_context(|| format!("parsing GET {path}"))
}

async fn fetch(client: &Client) -> Result<Recording> {
    let version = get(client, "/version", "application/json").await?;
    let discovery_core = get(client, "/api", ACCEPT_DISCOVERY).await?;
    let discovery_groups = get(client, "/apis", ACCEPT_DISCOVERY).await?;
    let resources = ResourceIndex::from_discovery(
        &fixtures::parse_discovery(&discovery_core)?,
        &fixtures::parse_discovery(&discovery_groups)?,
    );

    let mut openapi_index = get(client, "/openapi/v3", "application/json").await?;
    filter_openapi_index(&mut openapi_index);
    let mut openapi = BTreeMap::new();
    for doc in OPENAPI_DOCUMENTS {
        let url = openapi_index
            .pointer(&format!(
                "/paths/{}/serverRelativeURL",
                doc.replace('/', "~1")
            ))
            .and_then(Value::as_str)
            .map_or_else(|| format!("/openapi/v3/{doc}"), str::to_owned);
        let mut document = get(client, &url, "application/json").await?;
        normalize::strip_descriptions(&mut document);
        openapi.insert((*doc).to_owned(), document);
    }

    let mut tables = BTreeMap::new();
    let mut objects = BTreeMap::new();
    for resource in resources.preferred_listable() {
        let gvr = &resource.gvr;
        let path = format!("{}?includeObject=Metadata", gvr.collection_path(None));
        let mut table = get(client, &path, ACCEPT_TABLE).await?;
        if table.get("kind").and_then(Value::as_str) != Some("Table") {
            bail!("{gvr} did not return a Table");
        }
        normalize::strip_row_managed_fields(&mut table);
        if gvr.resource == "events" {
            normalize::retain_namespaces(&mut table, EVENT_NAMESPACES);
        }
        tables.insert(gvr.clone(), table);
        let namespace = match object_scope(resource) {
            None => continue,
            Some(ObjectScope::AllNamespaces) => None,
            Some(ObjectScope::Namespace(ns)) => Some(ns),
        };
        let mut list = get(client, &gvr.collection_path(namespace), "application/json").await?;
        if *gvr == Gvr::core("events") {
            // Events are many and nobody reads their field ownership.
            normalize::strip_item_managed_fields(&mut list);
            normalize::retain_namespaces(&mut list, EVENT_NAMESPACES);
        }
        if list
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|i| !i.is_empty())
        {
            objects.insert(gvr.clone(), list);
        }
    }
    Ok(Recording {
        version,
        discovery_core,
        discovery_groups,
        openapi_index,
        openapi,
        tables,
        objects,
    })
}

/// Scrubs and normalizes a recording in place.
fn clean(recording: &mut Recording) {
    let secrets = Gvr::core("secrets");
    if let Some(table) = recording.tables.get_mut(&secrets) {
        normalize::scrub_secret_table(table);
    }
    if let Some(list) = recording.objects.get_mut(&secrets) {
        normalize::scrub_secret_list(list);
    }
    for value in recording
        .tables
        .values_mut()
        .chain(recording.objects.values_mut())
    {
        normalize::sort_by_identity(value);
    }
    let mut uids = UidMap::default();
    for (gvr, value) in recording.tables.iter().chain(&recording.objects) {
        uids.register_all(&format!("{}/{}", gvr.group, gvr.resource), value);
    }
    let values = recording
        .tables
        .values_mut()
        .chain(recording.objects.values_mut())
        .chain(recording.openapi.values_mut())
        .chain([
            &mut recording.version,
            &mut recording.discovery_core,
            &mut recording.discovery_groups,
            &mut recording.openapi_index,
        ]);
    for value in values {
        uids.normalize(value);
    }
}

/// Records every fixture from `client` into `out`, replacing what was there.
pub async fn record(client: &Client, out: PathBuf) -> Result<Summary> {
    let mut recording = fetch(client).await?;
    clean(&mut recording);

    let layout = Layout::new(out);
    for dir in ["tables", "objects", "openapi", "discovery"] {
        let path = layout.root().join(dir);
        if path.exists() {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    for (path, value) in &recording.openapi {
        fixtures::write_compact_json(&layout.openapi_document(path), value)?;
    }
    let mut writes: Vec<(PathBuf, &Value)> = vec![
        (layout.version(), &recording.version),
        (layout.discovery_core(), &recording.discovery_core),
        (layout.discovery_groups(), &recording.discovery_groups),
        (layout.openapi_index(), &recording.openapi_index),
    ];
    writes.extend(recording.tables.iter().map(|(g, v)| (layout.table(g), v)));
    writes.extend(
        recording
            .objects
            .iter()
            .map(|(g, v)| (layout.objects(g), v)),
    );
    let mut summary = Summary::default();
    for (path, value) in writes {
        fixtures::write_json(&path, value)?;
    }
    for path in fixtures::json_files(layout.root())? {
        let text = fs::read_to_string(&path)?;
        let leaks = normalize::find_leaks(&text);
        if !leaks.is_empty() {
            bail!(
                "{} still contains {leaks:?}; fix the scrubbing",
                path.display()
            );
        }
        summary.files.insert(path, text.len());
    }
    // What was written must load as the fake server will load it.
    Fixtures::load(&layout).context("the new fixtures do not load")?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(group: &str, resource: &str, namespaced: bool) -> ResourceInfo {
        ResourceInfo {
            gvr: Gvr::new(group, "v1", resource),
            kind: "K".into(),
            namespaced,
            verbs: vec!["list".into()],
            preferred: true,
        }
    }

    #[test]
    fn records_objects_of_the_demo_namespace_and_a_few_cluster_kinds() {
        use ObjectScope::*;
        assert_eq!(
            object_scope(&info("", "pods", true)),
            Some(Namespace("demo"))
        );
        assert_eq!(object_scope(&info("", "events", true)), Some(AllNamespaces));
        assert_eq!(object_scope(&info("", "nodes", false)), Some(AllNamespaces));
        assert_eq!(
            object_scope(&info("rbac.authorization.k8s.io", "clusterroles", false)),
            None
        );
        assert_eq!(object_scope(&info("events.k8s.io", "events", true)), None);
        assert_eq!(object_scope(&info("metrics.k8s.io", "pods", true)), None);
    }

    #[test]
    fn the_openapi_index_lists_only_recorded_documents() {
        let mut index = json!({"paths": {"apis/apps/v1": {"serverRelativeURL": "/openapi/v3/apis/apps/v1?hash=A"},
                                         "api/v1": {"serverRelativeURL": "/openapi/v3/api/v1?hash=B"}}});
        filter_openapi_index(&mut index);
        assert_eq!(index["paths"].as_object().unwrap().len(), 1);
        assert!(index["paths"]["apis/apps/v1"].is_object());
    }

    #[test]
    fn cleaning_scrubs_secrets_and_normalizes_uids_across_files() {
        let uid = "0f5b6c1e-9d2a-4c1b-8e7f-1a2b3c4d5e6f";
        let mut recording = Recording {
            version: json!({}),
            discovery_core: json!({}),
            discovery_groups: json!({}),
            openapi_index: json!({}),
            openapi: BTreeMap::new(),
            tables: BTreeMap::from([(
                Gvr::core("secrets"),
                json!({
                "columnDefinitions": [{"name": "Name"}],
                "rows": [{"cells": ["s"], "object": {"metadata": {"name": "s", "namespace": "demo", "uid": uid}}}]}),
            )]),
            objects: BTreeMap::from([(
                Gvr::core("secrets"),
                json!({
                "items": [{"metadata": {"name": "s", "namespace": "demo", "uid": uid}, "data": {"k": "dg=="}}]}),
            )]),
        };
        clean(&mut recording);
        let stable = normalize::stable_uid("/secrets/demo/s");
        let secrets = Gvr::core("secrets");
        assert_eq!(
            recording.tables[&secrets]["rows"][0]["object"]["metadata"]["uid"],
            stable
        );
        assert_eq!(
            recording.objects[&secrets]["items"][0]["metadata"]["uid"],
            stable
        );
        assert_eq!(
            recording.objects[&secrets]["items"][0]["data"]["k"],
            normalize::SECRET_MARKER
        );
    }
}
