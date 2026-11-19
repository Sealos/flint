//! The fake server's current objects: the fixtures, plus what tests changed. Renders lists,
//! objects, Tables and errors the way the API server does.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::fixtures::{Fixtures, Gvr};
use super::resources::ResourceIndex;
use super::route::Query;
use super::row::FakeRow;
use super::selector::{FieldSelector, LabelSelector};

/// Resource versions start here, above every recorded one.
const FIRST_RESOURCE_VERSION: u64 = 1000;

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

#[derive(Debug, Clone)]
struct TableData {
    columns: Value,
    rows: Vec<FakeRow>,
}

#[derive(Debug, Clone)]
struct ObjectList {
    kind: String,
    api_version: String,
    items: Vec<Value>,
}

/// What the server holds now.
#[derive(Debug)]
pub struct State {
    resource_version: u64,
    tables: BTreeMap<Gvr, TableData>,
    objects: BTreeMap<Gvr, ObjectList>,
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
                let field = |k: &str| list.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                let items = list
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                (
                    gvr.clone(),
                    ObjectList {
                        kind: field("kind"),
                        api_version: field("apiVersion"),
                        items,
                    },
                )
            })
            .collect();
        State {
            resource_version: FIRST_RESOURCE_VERSION,
            tables,
            objects,
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

    /// Adds or replaces a row (by namespace and name) at a new resource version.
    pub fn upsert_row(&mut self, gvr: &Gvr, mut row: FakeRow) -> FakeRow {
        let rv = self.bump();
        row.set_resource_version(rv);
        let table = self.tables.entry(gvr.clone()).or_insert_with(|| TableData {
            columns: json!([]),
            rows: Vec::new(),
        });
        match table
            .rows
            .iter_mut()
            .find(|r| r.is(row.namespace(), row.name()))
        {
            Some(existing) => *existing = row.clone(),
            None => table.rows.push(row.clone()),
        }
        row
    }

    /// Removes a row (and the recorded object of the same name) at a new resource version.
    /// Returns the last state of the row.
    pub fn remove_row(
        &mut self,
        gvr: &Gvr,
        namespace: Option<&str>,
        name: &str,
    ) -> Option<FakeRow> {
        let table = self.tables.get_mut(gvr)?;
        let index = table.rows.iter().position(|r| r.is(namespace, name))?;
        let mut row = table.rows.remove(index);
        if let Some(list) = self.objects.get_mut(gvr) {
            list.items.retain(|i| {
                i.pointer("/metadata/namespace").and_then(Value::as_str) != namespace
                    || i.pointer("/metadata/name").and_then(Value::as_str) != Some(name)
            });
        }
        row.set_resource_version(self.bump());
        Some(row)
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

    /// A Table list.
    pub fn list_table(
        &self,
        gvr: &Gvr,
        namespace: Option<&str>,
        query: &Query,
    ) -> Result<Value, ApiError> {
        let rows: Vec<Value> = self
            .rows(gvr, namespace, query)
            .into_iter()
            .map(|r| r.as_json().clone())
            .collect();
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

    /// A plain list (`PodList`) of the recorded objects; empty for resources without any.
    pub fn list_objects(
        &self,
        resources: &ResourceIndex,
        gvr: &Gvr,
        namespace: Option<&str>,
        query: &Query,
    ) -> Result<Value, ApiError> {
        let (kind, items) = match self.objects.get(gvr) {
            Some(list) => (
                list.kind.clone(),
                list.items
                    .iter()
                    .filter(|i| matches(i, namespace, query))
                    .cloned()
                    .collect(),
            ),
            None => (
                resources
                    .get(gvr)
                    .map(|r| r.list_kind())
                    .unwrap_or_default(),
                Vec::new(),
            ),
        };
        let (items, mut meta) = page(&items, query)?;
        meta["resourceVersion"] = self.resource_version.to_string().into();
        Ok(json!({"kind": kind, "apiVersion": gvr.api_version(), "metadata": meta, "items": items}))
    }

    /// One recorded object, with its `kind` and `apiVersion`.
    pub fn object(
        &self,
        resources: &ResourceIndex,
        gvr: &Gvr,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Value, ApiError> {
        let not_found = || ApiError::not_found(format!("{} \"{name}\" not found", gvr.resource));
        let list = self.objects.get(gvr).ok_or_else(not_found)?;
        let mut object = list
            .items
            .iter()
            .find(|i| {
                i.pointer("/metadata/namespace").and_then(Value::as_str) == namespace
                    && i.pointer("/metadata/name").and_then(Value::as_str) == Some(name)
            })
            .cloned()
            .ok_or_else(not_found)?;
        if object.get("kind").is_none() {
            let kind = resources
                .get(gvr)
                .map(|r| r.kind.clone())
                .unwrap_or_else(|| list.kind.trim_end_matches("List").to_owned());
            object["kind"] = kind.into();
            object["apiVersion"] = list.api_version.clone().into();
        }
        Ok(object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Fixtures {
        Fixtures::load_default().unwrap()
    }

    #[test]
    fn lists_tables_by_namespace_with_paging() {
        let fixtures = fixtures();
        let state = State::new(&fixtures);
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
    fn serves_recorded_objects_and_empty_lists() {
        let fixtures = fixtures();
        let state = State::new(&fixtures);
        let deployments = Gvr::new("apps", "v1", "deployments");
        let web = state
            .object(&fixtures.resources, &deployments, Some("demo"), "web")
            .unwrap();
        assert_eq!(
            (web["kind"].as_str(), web["apiVersion"].as_str()),
            (Some("Deployment"), Some("apps/v1"))
        );
        let missing = state.object(&fixtures.resources, &deployments, Some("demo"), "nope");
        assert_eq!(missing.unwrap_err().code, 404);
        let jobs = Gvr::new("batch", "v1", "jobs");
        let list = state
            .list_objects(&fixtures.resources, &jobs, Some("demo"), &Query::default())
            .unwrap();
        assert_eq!(list["kind"], "JobList");
        assert!(list["items"].as_array().unwrap().is_empty());
        let one = state
            .table_object(&deployments, Some("demo"), "web")
            .unwrap();
        assert_eq!(one["rows"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn selectors_filter_events() {
        let fixtures = fixtures();
        let state = State::new(&fixtures);
        let events = Gvr::core("events");
        let all = state
            .list_objects(&fixtures.resources, &events, None, &Query::default())
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
        let matched = state
            .list_objects(&fixtures.resources, &events, None, &query)
            .unwrap();
        let matched = matched["items"].as_array().unwrap();
        assert!(!matched.is_empty());
        assert!(matched.iter().all(|e| e["involvedObject"]["uid"] == uid));
    }

    #[test]
    fn writes_bump_the_resource_version() {
        let fixtures = fixtures();
        let mut state = State::new(&fixtures);
        let cms = Gvr::core("configmaps");
        let row = FakeRow::build(
            Some("demo"),
            "new",
            vec!["new".into(), 0.into(), "1d".into()],
        );
        let added = state.upsert_row(&cms, row);
        assert_eq!(added.resource_version(), "1001");
        assert!(state.row(&cms, Some("demo"), "new").is_some());
        let changed = state.upsert_row(&cms, added.with_cell(1, 1));
        assert_eq!(changed.resource_version(), "1002");
        assert_eq!(
            state
                .rows(&cms, Some("demo"), &Query::default())
                .iter()
                .filter(|r| r.name() == "new")
                .count(),
            1
        );
        let removed = state.remove_row(&cms, Some("demo"), "new").unwrap();
        assert_eq!(removed.resource_version(), "1003");
        assert!(state.remove_row(&cms, Some("demo"), "new").is_none());
        assert_eq!(state.resource_version(), 1003);
        let err = ApiError::new(403, "Forbidden", "no");
        assert_eq!(err.body["code"], 403);
    }
}
