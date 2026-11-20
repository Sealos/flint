//! Records the fixtures from the dev cluster (see [`crate::fake_api::fixtures`] for the layout).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail};
use kube::Client;
use serde_json::Value;

use super::normalize::{self, UidMap};
use crate::fake_api::clock;
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
/// (later kept only in `default` and `demo`). Metrics change on every scrape and are only
/// recorded as Tables.
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

/// What was recorded, by path relative to the fixtures directory, with its size in bytes.
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

/// Tries per request; the waits between them double from [`RETRY_WAIT`].
const ATTEMPTS: u32 = 4;
const RETRY_WAIT: Duration = Duration::from_millis(500);

async fn get(client: &Client, path: &str, accept: &str) -> Result<Value> {
    let mut attempt = 0;
    let text = loop {
        let request = http::Request::get(path)
            .header(http::header::ACCEPT, accept)
            .body(Vec::new())?;
        match client.request_text(request).await {
            Ok(text) => break text,
            Err(e) if attempt + 1 < ATTEMPTS => {
                eprintln!("  GET {path} failed ({e}); retrying");
                tokio::time::sleep(RETRY_WAIT * 2u32.pow(attempt)).await;
                attempt += 1;
            }
            Err(e) => {
                let hint = match &e {
                    kube::Error::Api(s) if s.code == 503 && path.contains("metrics.k8s.io") => {
                        " (metrics-server is probably still starting; wait a minute and retry)"
                    }
                    _ => "",
                };
                return Err(anyhow::Error::new(e).context(format!("GET {path}{hint}")));
            }
        }
    };
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
        eprintln!("  {gvr}");
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

/// Scrubs and normalizes a recording in place; `recorded_at` is when it was fetched (Unix
/// seconds).
fn clean(recording: &mut Recording, recorded_at: i64) {
    if let Some(list) = recording.objects.get_mut(&Gvr::core("configmaps")) {
        normalize::scrub_helm_configmaps(list);
    }
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
        normalize::to_fixture_time(value, recorded_at);
    }
}

/// Every fixture file's text, by path relative to the fixtures directory.
fn render(recording: &Recording) -> Result<BTreeMap<PathBuf, String>> {
    let layout = Layout::new("");
    let mut files = BTreeMap::new();
    for (path, value) in &recording.openapi {
        files.insert(
            layout.openapi_document(path),
            fixtures::json_text(value, true)?,
        );
    }
    let mut pretty: Vec<(PathBuf, &Value)> = vec![
        (layout.version(), &recording.version),
        (layout.discovery_core(), &recording.discovery_core),
        (layout.discovery_groups(), &recording.discovery_groups),
        (layout.openapi_index(), &recording.openapi_index),
    ];
    pretty.extend(recording.tables.iter().map(|(g, v)| (layout.table(g), v)));
    pretty.extend(
        recording
            .objects
            .iter()
            .map(|(g, v)| (layout.objects(g), v)),
    );
    for (path, value) in pretty {
        files.insert(path, fixtures::json_text(value, false)?);
    }
    Ok(files)
}

/// Fails if any file still holds a credential or a local path. Runs before anything is written.
fn check_leaks(files: &BTreeMap<PathBuf, String>) -> Result<()> {
    for (path, text) in files {
        let leaks = normalize::find_leaks(text);
        if !leaks.is_empty() {
            bail!(
                "{} would contain {leaks:?}; fix the scrubbing",
                path.display()
            );
        }
    }
    Ok(())
}

/// Refuses to record into `out` unless it is the committed fixtures directory `fixtures`
/// (replaced as a whole, as always) or a directory that does not exist yet or is empty:
/// [`install`] swaps the destination out whole, so anything else in it would be lost.
pub fn check_destination(out: &Path, fixtures: &Path) -> Result<()> {
    let same = |a: &Path, b: &Path| match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(out, fixtures) {
        return Ok(());
    }
    match fs::read_dir(out) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("reading {}", out.display())),
        Ok(mut entries) => {
            if entries.next().is_some() {
                bail!(
                    "refusing to record into {}: it is not empty, and recording replaces the \
                     whole directory (give a new or empty one)",
                    out.display()
                );
            }
            Ok(())
        }
    }
}

/// Writes `files` to a new directory next to `out` and swaps it in once complete and loadable,
/// so a failed run leaves the previous fixtures untouched.
pub(crate) fn install(files: &BTreeMap<PathBuf, String>, out: &Path) -> Result<Summary> {
    check_leaks(files)?;
    let name = out
        .file_name()
        .and_then(|n| n.to_str())
        .context("the fixtures directory has no name")?;
    let staging = out.with_file_name(format!(".{name}.new"));
    let previous = out.with_file_name(format!(".{name}.old"));
    for dir in [&staging, &previous] {
        if dir.exists() {
            fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
        }
    }
    let mut summary = Summary::default();
    for (path, text) in files {
        fixtures::write_text(&staging.join(path), text)?;
        summary.files.insert(path.clone(), text.len());
    }
    if let Err(e) = Fixtures::load(&Layout::new(&staging)) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e.context("the new fixtures do not load; the old ones are kept"));
    }
    if out.exists() {
        fs::rename(out, &previous).with_context(|| format!("moving {} aside", out.display()))?;
    }
    fs::rename(&staging, out).with_context(|| format!("installing {}", out.display()))?;
    if previous.exists() {
        fs::remove_dir_all(&previous)?;
    }
    Ok(summary)
}

/// Records every fixture from `client` into `out`, replacing what was there only once the new
/// set is complete, clean and loadable.
pub async fn record(client: &Client, out: PathBuf) -> Result<Summary> {
    let recorded_at = clock::unix(SystemTime::now());
    let mut recording = fetch(client).await?;
    clean(&mut recording, recorded_at);
    install(&render(&recording)?, &out)
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
    fn only_the_fixtures_or_a_new_or_empty_directory_are_recorded_into() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = dir.path().join("fixtures");
        fs::create_dir(&fixtures).unwrap();
        fs::write(fixtures.join("version.json"), "{}").unwrap();
        // The committed set is replaced, as always.
        check_destination(&fixtures, &fixtures).unwrap();
        // Elsewhere: a new directory, or an empty one.
        check_destination(&dir.path().join("new"), &fixtures).unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        check_destination(&empty, &fixtures).unwrap();
        // Not one with something in it, which would be lost.
        let used = dir.path().join("used");
        fs::create_dir(&used).unwrap();
        fs::write(used.join("notes.txt"), "keep me").unwrap();
        let error = check_destination(&used, &fixtures).unwrap_err().to_string();
        assert!(error.contains("not empty"), "{error}");
        assert!(used.join("notes.txt").exists());
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
        clean(&mut recording, clock::FIXTURE_NOW_SECONDS);
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

    #[test]
    fn a_failed_install_keeps_the_old_fixtures() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("fixtures");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("version.json"), "{\"old\": true}\n").unwrap();
        let leaking = BTreeMap::from([(PathBuf::from("version.json"), "MIIBkTCB".to_owned())]);
        assert!(
            install(&leaking, &out)
                .unwrap_err()
                .to_string()
                .contains("MII")
        );
        let incomplete = BTreeMap::from([(PathBuf::from("version.json"), "{}\n".to_owned())]);
        assert!(install(&incomplete, &out).is_err());
        assert_eq!(
            fs::read_to_string(out.join("version.json")).unwrap(),
            "{\"old\": true}\n"
        );
        assert!(!dir.path().join(".fixtures.new").exists());

        // A complete set replaces the old one.
        let complete = Fixtures::load_default().unwrap();
        let recording = Recording {
            version: complete.version.clone(),
            discovery_core: complete.discovery_core.clone(),
            discovery_groups: complete.discovery_groups.clone(),
            openapi_index: complete.openapi_index.clone(),
            openapi: complete.openapi.clone(),
            tables: complete.tables.clone(),
            objects: complete.objects.clone(),
        };
        let summary = install(&render(&recording).unwrap(), &out).unwrap();
        assert!(
            summary
                .files
                .contains_key(Path::new("tables/core/v1/pods.json"))
        );
        assert!(Fixtures::load(&Layout::new(&out)).is_ok());
        assert!(!dir.path().join(".fixtures.old").exists());
    }
}
