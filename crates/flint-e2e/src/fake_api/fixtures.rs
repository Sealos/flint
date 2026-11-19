//! The recorded fixtures: where each response lives on disk, and loading them for replay.
//!
//! ```text
//! fixtures/
//!   version.json                         GET /version
//!   discovery/api.json                   GET /api   (aggregated discovery, apidiscovery.k8s.io/v2)
//!   discovery/apis.json                  GET /apis  (aggregated discovery)
//!   openapi/v3.json                      GET /openapi/v3 (only the recorded documents are listed)
//!   openapi/v3/<path>.json               GET /openapi/v3/<path>, e.g. apis/apps/v1
//!   tables/<group>/<version>/<resource>.json    Table list in all namespaces, includeObject=Metadata
//!   objects/<group>/<version>/<resource>.json   plain JSON list of the recorded objects
//! ```
//!
//! The core group is stored as `core`. `scripts/record-fixtures.sh` writes these files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use kube::core::discovery::v2::APIGroupDiscoveryList;
use serde_json::Value;

use super::resources::ResourceIndex;

/// The fixtures committed with this crate.
pub const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");

/// The directory name of the core (legacy, `/api`) group.
const CORE_DIR: &str = "core";

/// A group, version and resource (the plural, as in URLs).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Gvr {
    /// Empty for the core group.
    pub group: String,
    pub version: String,
    pub resource: String,
}

impl Gvr {
    pub fn new(group: &str, version: &str, resource: &str) -> Self {
        Gvr {
            group: group.to_owned(),
            version: version.to_owned(),
            resource: resource.to_owned(),
        }
    }

    /// A core group resource, such as `pods`.
    pub fn core(resource: &str) -> Self {
        Gvr::new("", "v1", resource)
    }

    /// `v1` or `apps/v1`.
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }

    /// The collection URL path, in all namespaces (`None`) or in one.
    pub fn collection_path(&self, namespace: Option<&str>) -> String {
        let base = if self.group.is_empty() {
            format!("/api/{}", self.version)
        } else {
            format!("/apis/{}/{}", self.group, self.version)
        };
        match namespace {
            Some(ns) => format!("{base}/namespaces/{ns}/{}", self.resource),
            None => format!("{base}/{}", self.resource),
        }
    }

    /// The fixture file of this resource below `dir` (`tables` or `objects`).
    fn file(&self, root: &Path, dir: &str) -> PathBuf {
        let group = if self.group.is_empty() {
            CORE_DIR
        } else {
            &self.group
        };
        root.join(dir)
            .join(group)
            .join(&self.version)
            .join(format!("{}.json", self.resource))
    }

    /// The inverse of [`Gvr::file`]: the resource a `<group>/<version>/<resource>.json` path
    /// relative to `tables` or `objects` names.
    fn from_relative(path: &Path) -> Option<Gvr> {
        let parts: Vec<&str> = path.iter().filter_map(|p| p.to_str()).collect();
        let [group, version, file] = parts.as_slice() else {
            return None;
        };
        let resource = file.strip_suffix(".json")?;
        let group = if *group == CORE_DIR { "" } else { group };
        Some(Gvr::new(group, version, resource))
    }
}

impl std::fmt::Display for Gvr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.api_version(), self.resource)
    }
}

/// Paths of the fixture files below a root directory.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn version(&self) -> PathBuf {
        self.root.join("version.json")
    }

    /// Aggregated discovery of the core group (`/api`).
    pub fn discovery_core(&self) -> PathBuf {
        self.root.join("discovery/api.json")
    }

    /// Aggregated discovery of the named groups (`/apis`).
    pub fn discovery_groups(&self) -> PathBuf {
        self.root.join("discovery/apis.json")
    }

    pub fn openapi_index(&self) -> PathBuf {
        self.root.join("openapi/v3.json")
    }

    /// An OpenAPI v3 document by its path below `/openapi/v3/`, such as `apis/apps/v1`.
    pub fn openapi_document(&self, path: &str) -> PathBuf {
        self.root.join("openapi/v3").join(format!("{path}.json"))
    }

    pub fn table(&self, gvr: &Gvr) -> PathBuf {
        gvr.file(&self.root, "tables")
    }

    pub fn objects(&self, gvr: &Gvr) -> PathBuf {
        gvr.file(&self.root, "objects")
    }
}

/// Every recorded response, parsed.
#[derive(Debug, Clone)]
pub struct Fixtures {
    pub version: Value,
    pub discovery_core: Value,
    pub discovery_groups: Value,
    pub resources: ResourceIndex,
    pub openapi_index: Value,
    /// By path below `/openapi/v3/`.
    pub openapi: BTreeMap<String, Value>,
    /// Table lists in all namespaces.
    pub tables: BTreeMap<Gvr, Value>,
    /// Plain lists (`{"kind": "PodList", "items": [...]}`).
    pub objects: BTreeMap<Gvr, Value>,
}

impl Fixtures {
    /// The fixtures committed with this crate.
    pub fn load_default() -> Result<Self> {
        Fixtures::load(&Layout::new(FIXTURES_DIR))
    }

    pub fn load(layout: &Layout) -> Result<Self> {
        let discovery_core = read_json(&layout.discovery_core())?;
        let discovery_groups = read_json(&layout.discovery_groups())?;
        let resources = ResourceIndex::from_discovery(
            &parse_discovery(&discovery_core)?,
            &parse_discovery(&discovery_groups)?,
        );
        let mut openapi = BTreeMap::new();
        let openapi_dir = layout.root().join("openapi/v3");
        for file in json_files(&openapi_dir)? {
            let rel = file
                .strip_prefix(&openapi_dir)
                .ok()
                .and_then(|p| p.to_str())
                .and_then(|p| p.strip_suffix(".json"))
                .map(str::to_owned);
            if let Some(rel) = rel {
                openapi.insert(rel, read_json(&file)?);
            }
        }
        Ok(Fixtures {
            version: read_json(&layout.version())?,
            discovery_core,
            discovery_groups,
            resources,
            openapi_index: read_json(&layout.openapi_index())?,
            openapi,
            tables: load_by_gvr(&layout.root().join("tables"))?,
            objects: load_by_gvr(&layout.root().join("objects"))?,
        })
    }
}

pub(crate) fn parse_discovery(value: &Value) -> Result<APIGroupDiscoveryList> {
    serde_json::from_value(value.clone()).context("aggregated discovery fixture")
}

/// Reads and parses one JSON file.
pub fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Writes `value` as pretty JSON with a trailing newline, creating parent directories.
pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    write_text(path, serde_json::to_string_pretty(value)?)
}

/// Writes `value` as JSON on one line: for large documents nobody reviews line by line.
pub fn write_compact_json(path: &Path, value: &Value) -> Result<()> {
    write_text(path, serde_json::to_string(value)?)
}

fn write_text(path: &Path, mut text: String) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    text.push('\n');
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Every `.json` file below `dir`, sorted. A missing directory has none.
pub fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    if !dir.exists() {
        return Ok(found);
    }
    let mut pending = vec![dir.to_owned()];
    while let Some(current) = pending.pop() {
        for entry in
            fs::read_dir(&current).with_context(|| format!("listing {}", current.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn load_by_gvr(dir: &Path) -> Result<BTreeMap<Gvr, Value>> {
    let mut map = BTreeMap::new();
    for file in json_files(dir)? {
        let rel = file.strip_prefix(dir).unwrap_or(&file);
        let Some(gvr) = Gvr::from_relative(rel) else {
            bail!("unexpected fixture file {}", file.display());
        };
        map.insert(gvr, read_json(&file)?);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_round_trip_and_name_the_core_group() {
        let layout = Layout::new("/f");
        let pods = Gvr::core("pods");
        assert_eq!(
            layout.table(&pods),
            PathBuf::from("/f/tables/core/v1/pods.json")
        );
        let deployments = Gvr::new("apps", "v1", "deployments");
        assert_eq!(
            layout.objects(&deployments),
            PathBuf::from("/f/objects/apps/v1/deployments.json")
        );
        assert_eq!(
            Gvr::from_relative(Path::new("core/v1/pods.json")),
            Some(pods.clone())
        );
        assert_eq!(Gvr::from_relative(Path::new("v1/pods.json")), None);
        assert_eq!(pods.api_version(), "v1");
        assert_eq!(
            pods.collection_path(Some("demo")),
            "/api/v1/namespaces/demo/pods"
        );
        assert_eq!(
            deployments.collection_path(None),
            "/apis/apps/v1/deployments"
        );
        assert_eq!(deployments.to_string(), "apps/v1/deployments");
    }

    #[test]
    fn writes_and_reads_back_json_trees() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let pods = Gvr::core("pods");
        write_json(&layout.table(&pods), &json!({"kind": "Table"})).unwrap();
        write_json(&layout.objects(&pods), &json!({"kind": "PodList"})).unwrap();
        let text = fs::read_to_string(layout.table(&pods)).unwrap();
        assert!(text.ends_with("}\n") && text.lines().count() == 3);
        let doc = layout.openapi_document("apis/apps/v1");
        write_compact_json(&doc, &json!({"a": {"b": 1}})).unwrap();
        assert_eq!(fs::read_to_string(&doc).unwrap(), "{\"a\":{\"b\":1}}\n");
        let tables = load_by_gvr(&dir.path().join("tables")).unwrap();
        assert_eq!(tables[&pods]["kind"], "Table");
        assert!(json_files(&dir.path().join("missing")).unwrap().is_empty());
        fs::write(dir.path().join("tables/stray.json"), "{}").unwrap();
        assert!(load_by_gvr(&dir.path().join("tables")).is_err());
    }

    #[test]
    fn the_committed_fixtures_load() {
        let fixtures = Fixtures::load_default().unwrap();
        assert_eq!(fixtures.version["major"], "1");
        assert!(fixtures.tables.contains_key(&Gvr::core("pods")));
        assert!(fixtures.resources.get(&Gvr::core("pods")).is_some());
        assert!(
            fixtures
                .objects
                .contains_key(&Gvr::new("apps", "v1", "deployments"))
        );
        assert!(!fixtures.openapi.is_empty());
    }
}
