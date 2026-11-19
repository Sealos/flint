//! The fake server's current objects: the fixtures plus what tests changed, a short log of
//! changes for watches that resume from an older resource version, and pod logs. Renders lists,
//! objects, Tables and errors the way the API server does. Timestamps stay in fixture time here;
//! the server rebases responses on the way out.

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Value, json};

use super::fixtures::{Fixtures, Gvr};
use super::resources::ResourceIndex;
use super::route::Query;
use super::row::FakeRow;
use super::selector::{FieldSelector, LabelSelector};

/// Resource versions start here, above every recorded one.
const FIRST_RESOURCE_VERSION: u64 = 1000;

/// Changes kept for watches that resume from an older resource version; older ones are
/// "compacted", and watching from before them gets a 410.
const LOG_CAPACITY: usize = 256;

/// Field selectors a Table list can evaluate (its rows carry metadata only).
const TABLE_FIELDS: &[&str] = &["metadata.name", "metadata.namespace"];

/// An API error: the HTTP status and its `Status` body.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub code: u16,
    pub body: Value,
}

impl ApiError {
    pub fn new(code: u16, reason: &str, message: impl Into<String>) -> Self {
        ApiError {
            code,
            body: status(code, reason, &message.into()),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError::new(404, "NotFound", message)
    }
}

/// A `Status` object.
pub fn status(code: u16, reason: &str, message: &str) -> Value {
    json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure",
           "message": message, "reason": reason, "code": code})
}

/// One change, as watches see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub gvr: Gvr,
    pub resource_version: u64,
    /// `ADDED`, `MODIFIED` or `DELETED`.
    pub ty: &'static str,
    pub row: FakeRow,
    /// The row before a `MODIFIED`, so a watch with a selector can tell an object left it.
    pub previous: Option<FakeRow>,
}

#[derive(Debug, Clone)]
struct TableData {
    columns: Value,
    rows: Vec<FakeRow>,
}

type Key = (Option<String>, String);

fn key_of(object: &Value) -> Key {
    let field = |k: &str| {
        object
            .pointer(&format!("/metadata/{k}"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    (field("namespace"), field("name").unwrap_or_default())
}

/// What the server holds now.
#[derive(Debug)]
pub struct State {
    resources: ResourceIndex,
    resource_version: u64,
    /// The oldest resource version a watch may resume from.
    floor: u64,
    changes: VecDeque<Change>,
    tables: BTreeMap<Gvr, TableData>,
    /// Recorded (or test-provided) full objects.
    objects: BTreeMap<Gvr, BTreeMap<Key, Value>>,
    /// Pod logs set by tests, by namespace and name.
    logs: BTreeMap<(String, String), String>,
}

fn matches(object: &Value, namespace: Option<&str>, query: &Query) -> bool {
    let in_namespace = namespace.is_none_or(|ns| {
        object
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            == Some(ns)
    });
    in_namespace
        && query
            .field_selector
            .as_deref()
            .is_none_or(|s| FieldSelector::parse(s).matches(object))
        && query
            .label_selector
            .as_deref()
            .is_none_or(|s| LabelSelector::parse(s).matches(object))
}

/// One page of `items` from the query's `limit` and `continue` (an offset).
fn page<T: Clone>(items: &[T], query: &Query) -> Result<(Vec<T>, Value), ApiError> {
    let start = match &query.continue_token {
        Some(token) => token
            .strip_prefix("fake-")
            .and_then(|o| o.parse::<usize>().ok())
            .filter(|o| *o <= items.len())
            .ok_or_else(|| ApiError::new(400, "BadRequest", "invalid continue token"))?,
        None => 0,
    };
    let end = query
        .limit
        .map_or(items.len(), |l| (start + l).min(items.len()));
    let mut meta = json!({});
    if end < items.len() {
        meta["continue"] = format!("fake-{end}").into();
        meta["remainingItemCount"] = (items.len() - end).into();
    }
    Ok((items[start..end].to_vec(), meta))
}

/// metrics.k8s.io builds its own Tables: no resource version, and `limit` is ignored.
fn is_metrics(gvr: &Gvr) -> bool {
    gvr.group == "metrics.k8s.io"
}

/// The default of a cell of a column type, for rows built from a few named cells.
fn default_cell(ty: &str) -> Value {
    match ty {
        "integer" | "number" => 0.into(),
        "boolean" => false.into(),
        _ => "".into(),
    }
}

/// A cell value parsed for its column type.
fn typed_cell(ty: &str, text: &str) -> Value {
    match ty {
        "integer" => text
            .parse::<i64>()
            .map_or_else(|_| text.into(), Value::from),
        "number" => text
            .parse::<f64>()
            .map_or_else(|_| text.into(), Value::from),
        "boolean" => text
            .parse::<bool>()
            .map_or_else(|_| text.into(), Value::from),
        _ => text.into(),
    }
}

impl State {
    pub fn new(fixtures: &Fixtures) -> Self {
        let tables = fixtures
            .tables
            .iter()
            .map(|(gvr, table)| {
                let rows = table
                    .get("rows")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned()
                    .map(FakeRow::new)
                    .collect();
                let columns = table.get("columnDefinitions").cloned().unwrap_or_default();
                (gvr.clone(), TableData { columns, rows })
            })
            .collect();
        let objects = fixtures
            .objects
            .iter()
            .map(|(gvr, list)| {
                let api_version = list.get("apiVersion").cloned().unwrap_or_default();
                let kind = fixtures
                    .resources
                    .get(gvr)
                    .map(|r| r.kind.clone())
                    .unwrap_or_default();
                let items = list
                    .get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|item| {
                        let mut item = item.clone();
                        // Items of a list carry no kind; single GETs do.
                        if item.get("kind").is_none() {
                            item["kind"] = kind.clone().into();
                            item["apiVersion"] = api_version.clone();
                        }
                        (key_of(&item), item)
                    })
                    .collect();
                (gvr.clone(), items)
            })
            .collect();
        State {
            resources: fixtures.resources.clone(),
            resource_version: FIRST_RESOURCE_VERSION,
            floor: FIRST_RESOURCE_VERSION,
            changes: VecDeque::new(),
            tables,
            objects,
            logs: BTreeMap::new(),
        }
    }

    pub fn resource_version(&self) -> u64 {
        self.resource_version
    }

    /// Advances the resource version, as every write does.
    pub fn bump(&mut self) -> u64 {
        self.resource_version += 1;
        self.resource_version
    }

    /// The kind of a resource (`Pod`), empty if unknown.
    pub fn kind(&self, gvr: &Gvr) -> String {
        self.resources
            .get(gvr)
            .map(|r| r.kind.clone())
            .unwrap_or_default()
    }

    /// The column definitions of a resource's Table.
    pub fn columns(&self, gvr: &Gvr) -> Value {
        self.tables
            .get(gvr)
            .map(|t| t.columns.clone())
            .unwrap_or_else(|| json!([]))
    }

    /// The current rows matching a namespace and the query's selectors.
    pub fn rows(&self, gvr: &Gvr, namespace: Option<&str>, query: &Query) -> Vec<FakeRow> {
        self.tables
            .get(gvr)
            .map(|t| {
                t.rows
                    .iter()
                    .filter(|r| matches(&r.as_json()["object"], namespace, query))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn row(&self, gvr: &Gvr, namespace: Option<&str>, name: &str) -> Option<FakeRow> {
        self.tables
            .get(gvr)?
            .rows
            .iter()
            .find(|r| r.is(namespace, name))
            .cloned()
    }

    /// A row for `namespace`/`name` with the named cells set (by column name, parsed for the
    /// column type) and every other cell at its type's default; the name column is the name.
    pub fn new_row(
        &self,
        gvr: &Gvr,
        namespace: Option<&str>,
        name: &str,
        cells: &[(&str, &str)],
    ) -> Result<FakeRow, String> {
        let columns = self.columns(gvr);
        let columns = columns.as_array().cloned().unwrap_or_default();
        let names: Vec<&str> = columns
            .iter()
            .map(|c| c["name"].as_str().unwrap_or(""))
            .collect();
        if let Some((unknown, _)) = cells.iter().find(|(c, _)| !names.contains(c)) {
            return Err(format!(
                "{gvr} has no column {unknown:?}; columns: {names:?}"
            ));
        }
        let values = columns
            .iter()
            .map(|c| {
                let column = c["name"].as_str().unwrap_or("");
                let ty = c["type"].as_str().unwrap_or("string");
                match cells.iter().find(|(n, _)| *n == column) {
                    Some((_, text)) => typed_cell(ty, text),
                    None if column == "Name" => name.into(),
                    None if column == "Namespace" => namespace.unwrap_or("").into(),
                    None => default_cell(ty),
                }
            })
            .collect();
        Ok(FakeRow::build(namespace, name, values))
    }

    fn remember(&mut self, change: Change) -> Change {
        self.changes.push_back(change.clone());
        if self.changes.len() > LOG_CAPACITY
            && let Some(dropped) = self.changes.pop_front()
        {
            self.floor = self.floor.max(dropped.resource_version);
        }
        change
    }

    /// Updates the full object (if one is held) from a row's metadata.
    fn sync_object(&mut self, gvr: &Gvr, row: &FakeRow) {
        let key = (row.namespace().map(str::to_owned), row.name().to_owned());
        if let Some(object) = self.objects.get_mut(gvr).and_then(|o| o.get_mut(&key))
            && let (Some(meta), Some(from)) = (
                object.get_mut("metadata").and_then(Value::as_object_mut),
                row.metadata().as_object(),
            )
        {
            for (k, v) in from {
                meta.insert(k.clone(), v.clone());
            }
        }
    }

    fn upsert(&mut self, gvr: &Gvr, mut row: FakeRow) -> (FakeRow, Option<FakeRow>) {
        let rv = self.bump();
        row.set_resource_version(rv);
        self.sync_object(gvr, &row);
        let table = self.tables.entry(gvr.clone()).or_insert_with(|| TableData {
            columns: json!([]),
            rows: Vec::new(),
        });
        let previous = match table
            .rows
            .iter_mut()
            .find(|r| r.is(row.namespace(), row.name()))
        {
            Some(existing) => Some(std::mem::replace(existing, row.clone())),
            None => {
                table.rows.push(row.clone());
                None
            }
        };
        (row, previous)
    }

    fn remove(&mut self, gvr: &Gvr, namespace: Option<&str>, name: &str) -> Option<FakeRow> {
        let table = self.tables.get_mut(gvr)?;
        let index = table.rows.iter().position(|r| r.is(namespace, name))?;
        let mut row = table.rows.remove(index);
        if let Some(objects) = self.objects.get_mut(gvr) {
            objects.remove(&(namespace.map(str::to_owned), name.to_owned()));
        }
        row.set_resource_version(self.bump());
        Some(row)
    }

    /// A change watches see: `ADDED` and `MODIFIED` add or replace the row (and update the
    /// object), `DELETED` removes both (an unknown row is still reported, at a new version).
    pub fn change(&mut self, gvr: &Gvr, ty: &'static str, row: FakeRow) -> Change {
        let (row, previous) = if ty == "DELETED" {
            let gone = match self.remove(gvr, row.namespace(), row.name()) {
                Some(gone) => gone,
                None => {
                    let mut gone = row;
                    gone.set_resource_version(self.bump());
                    gone
                }
            };
            (gone, None)
        } else {
            self.upsert(gvr, row)
        };
        let resource_version = self.resource_version;
        self.remember(Change {
            gvr: gvr.clone(),
            resource_version,
            ty,
            row,
            previous,
        })
    }

    /// A change no watch is told about: watches that resume from before it get a 410, so only
    /// a new list shows it.
    pub fn silent_upsert(&mut self, gvr: &Gvr, row: FakeRow) {
        self.upsert(gvr, row);
        self.floor = self.resource_version;
    }

    /// Removes a row and its object silently (see [`State::silent_upsert`]).
    pub fn silent_remove(&mut self, gvr: &Gvr, namespace: Option<&str>, name: &str) -> bool {
        let removed = self.remove(gvr, namespace, name).is_some();
        if removed {
            self.floor = self.resource_version;
        }
        removed
    }

    /// The changes of `gvr` after `resource_version`, or `None` if they were compacted.
    pub fn changes_since(&self, gvr: &Gvr, resource_version: u64) -> Option<Vec<Change>> {
        (resource_version >= self.floor).then(|| {
            self.changes
                .iter()
                .filter(|c| c.gvr == *gvr && c.resource_version > resource_version)
                .cloned()
                .collect()
        })
    }

    /// Adds or replaces a full object silently; a row is made for it if there is none.
    pub fn set_object(&mut self, gvr: &Gvr, mut object: Value) -> Result<(), String> {
        let (namespace, name) = key_of(&object);
        if name.is_empty() {
            return Err("the object has no metadata.name".into());
        }
        if object.get("kind").is_none()
            && let Some(resource) = self.resources.get(gvr)
        {
            object["kind"] = resource.kind.clone().into();
            object["apiVersion"] = gvr.api_version().into();
        }
        let row = match self.row(gvr, namespace.as_deref(), &name) {
            Some(row) => row,
            None => self.new_row(gvr, namespace.as_deref(), &name, &[])?,
        };
        let mut row_json = row.as_json().clone();
        if let (Some(meta), Some(from)) = (
            row_json
                .pointer_mut("/object/metadata")
                .and_then(Value::as_object_mut),
            object.get("metadata").and_then(Value::as_object),
        ) {
            for (k, v) in from {
                if k != "managedFields" {
                    meta.insert(k.clone(), v.clone());
                }
            }
        }
        self.objects
            .entry(gvr.clone())
            .or_default()
            .insert((namespace, name), object);
        self.silent_upsert(gvr, FakeRow::new(row_json));
        Ok(())
    }

    /// A Table with `rows`, as lists and watch events carry them.
    pub fn table(&self, gvr: &Gvr, rows: Vec<Value>, meta: Value, with_columns: bool) -> Value {
        json!({
            "kind": "Table",
            "apiVersion": "meta.k8s.io/v1",
            "metadata": meta,
            "columnDefinitions": if with_columns { self.columns(gvr) } else { Value::Null },
            "rows": rows,
        })
    }

    /// Refuses field selectors a Table cannot evaluate, as the API server refuses unknown field
    /// labels.
    pub fn check_table_fields(query: &Query) -> Result<(), ApiError> {
        let Some(selector) = query.field_selector.as_deref() else {
            return Ok(());
        };
        let parsed = FieldSelector::parse(selector);
        match parsed.fields().find(|f| !TABLE_FIELDS.contains(f)) {
            Some(field) => Err(ApiError::new(
                400,
                "BadRequest",
                format!("field label not supported: {field}"),
            )),
            None => Ok(()),
        }
    }

    /// A Table list.
    pub fn list_table(
        &self,
        gvr: &Gvr,
        namespace: Option<&str>,
        query: &Query,
    ) -> Result<Value, ApiError> {
        State::check_table_fields(query)?;
        let rows: Vec<Value> = self
            .rows(gvr, namespace, query)
            .into_iter()
            .map(|r| r.as_json().clone())
            .collect();
        if is_metrics(gvr) {
            return Ok(self.table(gvr, rows, json!({}), true));
        }
        let (rows, mut meta) = page(&rows, query)?;
        meta["resourceVersion"] = self.resource_version.to_string().into();
        Ok(self.table(gvr, rows, meta, true))
    }

    /// A one-row Table for a single object.
    pub fn table_object(
        &self,
        gvr: &Gvr,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Value, ApiError> {
        let row = self
            .row(gvr, namespace, name)
            .ok_or_else(|| ApiError::not_found(format!("{} \"{name}\" not found", gvr.resource)))?;
        let meta = json!({"resourceVersion": row.resource_version()});
        Ok(self.table(gvr, vec![row.as_json().clone()], meta, true))
    }

    /// The full object of a row: the recorded one, else a minimal one made from the row's
    /// metadata.
    pub fn object_of(&self, gvr: &Gvr, row: &FakeRow) -> Value {
        let key = (row.namespace().map(str::to_owned), row.name().to_owned());
        if let Some(object) = self.objects.get(gvr).and_then(|o| o.get(&key)) {
            return object.clone();
        }
        json!({"apiVersion": gvr.api_version(), "kind": self.kind(gvr), "metadata": row.metadata()})
    }

    /// A plain list (`PodList`): the full object of every row, recorded or made from the row.
    pub fn list_objects(
        &self,
        gvr: &Gvr,
        namespace: Option<&str>,
        query: &Query,
    ) -> Result<Value, ApiError> {
        let namespace_only = Query::default();
        let items: Vec<Value> = self
            .rows(gvr, namespace, &namespace_only)
            .iter()
            .map(|row| self.object_of(gvr, row))
            .filter(|object| matches(object, None, query))
            .collect();
        let list_kind = self
            .resources
            .get(gvr)
            .map(|r| r.list_kind())
            .unwrap_or_default();
        let (items, meta) = if is_metrics(gvr) {
            (items, json!({}))
        } else {
            let (items, mut meta) = page(&items, query)?;
            meta["resourceVersion"] = self.resource_version.to_string().into();
            (items, meta)
        };
        Ok(
            json!({"kind": list_kind, "apiVersion": gvr.api_version(), "metadata": meta, "items": items}),
        )
    }

    /// One object: recorded, or made from its row.
    pub fn object(
        &self,
        gvr: &Gvr,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Value, ApiError> {
        self.row(gvr, namespace, name)
            .map(|row| self.object_of(gvr, &row))
            .ok_or_else(|| ApiError::not_found(format!("{} \"{name}\" not found", gvr.resource)))
    }

    pub fn set_log(&mut self, namespace: &str, pod: &str, text: &str) {
        self.logs
            .insert((namespace.to_owned(), pod.to_owned()), text.to_owned());
    }

    /// A pod's log: what a test set, else a few canned lines. `tail` keeps the last lines.
    pub fn pod_log(
        &self,
        namespace: &str,
        pod: &str,
        tail: Option<usize>,
    ) -> Result<String, ApiError> {
        let pods = Gvr::core("pods");
        if self.row(&pods, Some(namespace), pod).is_none() {
            return Err(ApiError::not_found(format!("pods \"{pod}\" not found")));
        }
        let text = self
            .logs
            .get(&(namespace.to_owned(), pod.to_owned()))
            .cloned()
            .unwrap_or_else(|| {
                (1..=3)
                    .map(|n| format!("fake log line {n} of {namespace}/{pod}\n"))
                    .collect()
            });
        Ok(match tail {
            Some(n) => {
                let lines: Vec<&str> = text.lines().collect();
                let mut out = lines[lines.len().saturating_sub(n)..].join("\n");
                if !out.is_empty() {
                    out.push('\n');
                }
                out
            }
            None => text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> std::sync::Arc<Fixtures> {
        Fixtures::shared_default().unwrap()
    }

    #[test]
    fn lists_tables_by_namespace_with_paging() {
        let state = State::new(&fixtures());
        let pods = Gvr::core("pods");
        let all = state.list_table(&pods, None, &Query::default()).unwrap();
        assert_eq!(all["kind"], "Table");
        assert_eq!(all["metadata"]["resourceVersion"], "1000");
        assert!(all["columnDefinitions"].as_array().unwrap().len() > 3);
        let total = all["rows"].as_array().unwrap().len();
        let demo = state
            .list_table(&pods, Some("demo"), &Query::default())
            .unwrap();
        let in_demo = demo["rows"].as_array().unwrap().len();
        assert!(in_demo >= 2 && in_demo < total);

        let first = Query {
            limit: Some(2),
            ..Query::default()
        };
        let page1 = state.list_table(&pods, None, &first).unwrap();
        assert_eq!(page1["rows"].as_array().unwrap().len(), 2);
        assert_eq!(page1["metadata"]["remainingItemCount"], total - 2);
        let next = Query {
            continue_token: page1["metadata"]["continue"].as_str().map(str::to_owned),
            limit: Some(1000),
            ..Query::default()
        };
        let page2 = state.list_table(&pods, None, &next).unwrap();
        assert_eq!(page2["rows"].as_array().unwrap().len(), total - 2);
        assert!(page2["metadata"].get("continue").is_none());
        let bad = Query {
            continue_token: Some("x".into()),
            ..Query::default()
        };
        assert_eq!(state.list_table(&pods, None, &bad).unwrap_err().code, 400);
    }

    #[test]
    fn metrics_lists_have_no_version_and_ignore_limits() {
        let state = State::new(&fixtures());
        let metrics = Gvr::new("metrics.k8s.io", "v1beta1", "pods");
        let query = Query {
            limit: Some(1),
            ..Query::default()
        };
        let table = state.list_table(&metrics, None, &query).unwrap();
        assert_eq!(table["metadata"], json!({}));
        assert!(table["rows"].as_array().unwrap().len() > 1);
        let list = state.list_objects(&metrics, None, &query).unwrap();
        assert_eq!(list["metadata"], json!({}));
    }

    #[test]
    fn table_lists_refuse_fields_they_cannot_evaluate() {
        let state = State::new(&fixtures());
        let by_name = Query {
            field_selector: Some("metadata.name=web".into()),
            ..Query::default()
        };
        let deployments = Gvr::new("apps", "v1", "deployments");
        let table = state.list_table(&deployments, None, &by_name).unwrap();
        assert_eq!(table["rows"].as_array().unwrap().len(), 1);
        let by_phase = Query {
            field_selector: Some("status.phase=Running".into()),
            ..Query::default()
        };
        let err = state
            .list_table(&Gvr::core("pods"), None, &by_phase)
            .unwrap_err();
        assert_eq!(err.code, 400);
        assert!(
            err.body["message"]
                .as_str()
                .unwrap()
                .contains("status.phase")
        );
    }

    #[test]
    fn objects_are_recorded_or_made_from_rows() {
        let state = State::new(&fixtures());
        let deployments = Gvr::new("apps", "v1", "deployments");
        let web = state.object(&deployments, Some("demo"), "web").unwrap();
        assert_eq!(
            (web["kind"].as_str(), web["apiVersion"].as_str()),
            (Some("Deployment"), Some("apps/v1"))
        );
        assert!(web["spec"].is_object());
        let missing = state.object(&deployments, Some("demo"), "nope");
        assert_eq!(missing.unwrap_err().code, 404);
        // kube-system pods are listed but not recorded: a minimal object stands in.
        let pods = Gvr::core("pods");
        let system = state.rows(&pods, Some("kube-system"), &Query::default());
        let first = state
            .object(&pods, Some("kube-system"), system[0].name())
            .unwrap();
        assert_eq!(first["kind"], "Pod");
        assert_eq!(first["metadata"]["uid"], system[0].uid());
        let list = state
            .list_objects(&pods, Some("kube-system"), &Query::default())
            .unwrap();
        assert_eq!(list["kind"], "PodList");
        assert_eq!(list["items"].as_array().unwrap().len(), system.len());
        let one = state
            .table_object(&deployments, Some("demo"), "web")
            .unwrap();
        assert_eq!(one["rows"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn selectors_filter_events() {
        let state = State::new(&fixtures());
        let events = Gvr::core("events");
        let all = state
            .list_objects(&events, None, &Query::default())
            .unwrap();
        let uid = all["items"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|e| e["involvedObject"]["uid"].as_str())
            .expect("an event about an object with a uid");
        let query = Query {
            field_selector: Some(format!("involvedObject.uid={uid}")),
            ..Query::default()
        };
        let matched = state.list_objects(&events, None, &query).unwrap();
        let matched = matched["items"].as_array().unwrap();
        assert!(!matched.is_empty());
        assert!(matched.iter().all(|e| e["involvedObject"]["uid"] == uid));
    }

    #[test]
    fn changes_bump_versions_follow_objects_and_are_logged() {
        let mut state = State::new(&fixtures());
        let deployments = Gvr::new("apps", "v1", "deployments");
        let web = state.row(&deployments, Some("demo"), "web").unwrap();
        let changed = state.change(&deployments, "MODIFIED", web.with_label("tier", "front"));
        assert_eq!(changed.resource_version, 1001);
        assert!(changed.previous.is_some());
        let object = state.object(&deployments, Some("demo"), "web").unwrap();
        assert_eq!(object["metadata"]["labels"]["tier"], "front");
        assert_eq!(object["metadata"]["resourceVersion"], "1001");
        assert!(object["spec"].is_object(), "the recorded spec is kept");

        let fresh = state
            .new_row(&deployments, Some("demo"), "fresh", &[("Ready", "0/1")])
            .unwrap();
        state.change(&deployments, "ADDED", fresh.clone());
        assert_eq!(state.changes_since(&deployments, 1000).unwrap().len(), 2);
        assert_eq!(state.changes_since(&deployments, 1001).unwrap().len(), 1);
        state.change(&deployments, "DELETED", fresh);
        assert!(state.row(&deployments, Some("demo"), "fresh").is_none());
        assert!(
            state.changes_since(&deployments, 999).is_none(),
            "before the first version"
        );

        // A silent change compacts the log: resuming from before it is too old.
        assert!(state.silent_remove(&deployments, Some("demo"), "podinfo"));
        assert!(state.changes_since(&deployments, 1003).is_none());
        assert_eq!(state.changes_since(&deployments, 1004).unwrap(), vec![]);
        assert!(state.object(&deployments, Some("demo"), "podinfo").is_err());
    }

    #[test]
    fn the_change_log_is_short() {
        let mut state = State::new(&fixtures());
        let cms = Gvr::core("configmaps");
        let row = state.new_row(&cms, Some("demo"), "busy", &[]).unwrap();
        for _ in 0..LOG_CAPACITY + 10 {
            state.change(&cms, "MODIFIED", row.clone());
        }
        assert!(state.changes_since(&cms, 1005).is_none());
        let recent = state
            .changes_since(&cms, state.resource_version() - 3)
            .unwrap();
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn new_rows_fill_cells_by_column_type() {
        let state = State::new(&fixtures());
        let deployments = Gvr::new("apps", "v1", "deployments");
        let row = state
            .new_row(&deployments, Some("demo"), "x", &[("Ready", "1/1")])
            .unwrap();
        let columns = state.columns(&deployments);
        let index = |n: &str| {
            columns
                .as_array()
                .unwrap()
                .iter()
                .position(|c| c["name"] == n)
                .unwrap()
        };
        assert_eq!(row.cells()[index("Name")], "x");
        assert_eq!(row.cells()[index("Ready")], "1/1");
        assert_eq!(row.cells()[index("Available")], "");
        // Integer columns (CRD printer columns) get numbers.
        assert_eq!(typed_cell("integer", "3"), 3);
        assert_eq!(default_cell("integer"), 0);
        assert_eq!(typed_cell("number", "1.5"), 1.5);
        assert_eq!(typed_cell("boolean", "true"), true);
        assert_eq!(typed_cell("integer", "x"), "x");
        assert_eq!(default_cell("boolean"), false);
        let err = state
            .new_row(&deployments, None, "x", &[("Nope", "1")])
            .unwrap_err();
        assert!(err.contains("no column \"Nope\""));
    }

    #[test]
    fn set_object_adds_objects_with_rows() {
        let mut state = State::new(&fixtures());
        let secrets = Gvr::core("secrets");
        let secret = json!({"metadata": {"name": "applied", "namespace": "demo", "annotations": {
            "kubectl.kubernetes.io/last-applied-configuration": "{\"data\":{\"password\":\"aHVudGVyMg==\"}}"}},
            "type": "Opaque", "data": {"password": "aHVudGVyMg=="}});
        state.set_object(&secrets, secret).unwrap();
        let object = state.object(&secrets, Some("demo"), "applied").unwrap();
        assert_eq!(
            (object["kind"].as_str(), object["apiVersion"].as_str()),
            (Some("Secret"), Some("v1"))
        );
        let row = state.row(&secrets, Some("demo"), "applied").unwrap();
        assert!(row.metadata()["annotations"].is_object());
        assert!(state.set_object(&secrets, json!({"metadata": {}})).is_err());
    }

    #[test]
    fn pod_logs_are_canned_or_set() {
        let mut state = State::new(&fixtures());
        let pods = state.rows(&Gvr::core("pods"), Some("demo"), &Query::default());
        let pod = pods[0].name().to_owned();
        let canned = state.pod_log("demo", &pod, None).unwrap();
        assert_eq!(canned.lines().count(), 3);
        state.set_log("demo", &pod, "a\nb\nc\n");
        assert_eq!(state.pod_log("demo", &pod, Some(2)).unwrap(), "b\nc\n");
        assert_eq!(state.pod_log("demo", "nope", None).unwrap_err().code, 404);
    }
}
