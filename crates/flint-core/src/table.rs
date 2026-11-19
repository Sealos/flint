//! The server-side Table format (`as=Table;g=meta.k8s.io`), the rows built from it, and one-shot
//! Table lists.
//!
//! Lists and watches ask for Tables with `includeObject=Metadata`, so every row carries the
//! object's identity. See `docs/spikes/table-watch.md` for the wire format. Rows keep what a list
//! needs and nothing else: `managedFields`, annotations and owner references are never stored
//! (the detail view fetches the full object), the `Age` cell is never stored as text (the UI
//! computes it from `creationTimestamp`), and repeated strings are interned per kind.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use kube::Client;
use kube::api::{DynamicObject, ListParams, Request};
use kube::core::Resource;
use serde::Deserialize;
use serde_json::Value;

use crate::api_error::{ApiError, ApiErrorKind};
use crate::cluster::{self, ClusterError};
use crate::paging;
use crate::read::{KindInfo, ListQuery};

/// Asks for a Table and accepts plain JSON from servers that cannot produce one.
pub(crate) const TABLE_ACCEPT: &str =
    "application/json;as=Table;v=v1;g=meta.k8s.io, application/json";

// ---------------------------------------------------------------------------------------------
// Public row model

/// How a column's cells are typed, from the Table's `columnDefinitions[].type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnType {
    String,
    Integer,
    Number,
    Boolean,
    Date,
    Other,
}

impl ColumnType {
    fn parse(s: &str) -> Self {
        match s {
            "string" => ColumnType::String,
            "integer" => ColumnType::Integer,
            "number" => ColumnType::Number,
            "boolean" => ColumnType::Boolean,
            "date" => ColumnType::Date,
            _ => ColumnType::Other,
        }
    }
}

/// What a column shows, so front ends can treat the special ones specially.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnRole {
    /// The object name (always the first server column).
    Name,
    /// Added by Flint for namespaced kinds, right after Name. Servers omit it from Tables.
    Namespace,
    /// Computed from `creationTimestamp`; never stored as the server's frozen text.
    Age,
    Value,
}

/// One column. A kind's columns are shared (`Arc<[Column]>`) by every snapshot until the
/// server's definitions change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Column {
    pub name: Box<str>,
    pub ty: ColumnType,
    /// The server's `format` hint (`name`, `quantity`, `date`, …), possibly empty.
    pub format: Box<str>,
    pub description: Box<str>,
    /// 0 for the default `kubectl get` columns, 1 and up for `-o wide` extras.
    pub priority: i32,
    pub role: ColumnRole,
}

impl Column {
    fn synthetic(name: &str, ty: ColumnType, role: ColumnRole) -> Self {
        Column {
            name: name.into(),
            ty,
            format: "".into(),
            description: "".into(),
            priority: 0,
            role,
        }
    }
}

/// One cell, kept compact (24 bytes). Name, Namespace and Age refer to the row's own fields
/// instead of copying them.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    /// [`Row::name`].
    Name,
    /// [`Row::namespace`].
    Namespace,
    /// Age from [`Row::created`].
    Age,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Text, interned per kind.
    Text(Arc<str>),
    /// A point in time (Unix seconds). Date columns arrive as relative text ("5m") frozen when
    /// the server sent them; Flint turns that into an approximate timestamp so ages keep counting.
    Time(i64),
}

/// One object in a list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The object's uid; `namespace/name` for APIs that send none (metrics.k8s.io).
    pub uid: Box<str>,
    pub name: Box<str>,
    pub namespace: Option<Arc<str>>,
    pub resource_version: Box<str>,
    /// `metadata.generation`: changes with the spec only, unlike the resource version.
    pub generation: Option<i64>,
    /// `metadata.creationTimestamp`, Unix seconds.
    pub created: Option<i64>,
    pub labels: Box<[(Arc<str>, Arc<str>)]>,
    /// One per column of the snapshot.
    pub cells: Box<[Cell]>,
}

impl Row {
    /// The cell's display text, as `kubectl get` prints it. `now` is Unix seconds.
    pub fn cell_text(&self, index: usize, now: i64) -> Cow<'_, str> {
        match self.cells.get(index) {
            None | Some(Cell::Null) => Cow::Borrowed(""),
            Some(Cell::Name) => Cow::Borrowed(&self.name),
            Some(Cell::Namespace) => Cow::Borrowed(self.namespace.as_deref().unwrap_or("")),
            Some(Cell::Age) => match self.created {
                Some(t) => Cow::Owned(human_duration(now - t)),
                None => Cow::Borrowed("<unknown>"),
            },
            Some(Cell::Bool(b)) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Some(Cell::Int(i)) => Cow::Owned(i.to_string()),
            Some(Cell::Float(f)) => Cow::Owned(f.to_string()),
            Some(Cell::Text(s)) => Cow::Borrowed(s),
            Some(Cell::Time(t)) => Cow::Owned(human_duration(now - t)),
        }
    }

    /// Seconds since the object was created.
    pub fn age(&self, now: i64) -> Option<i64> {
        self.created.map(|c| now - c)
    }

    /// The label's value, if set.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| &**k == key)
            .map(|(_, v)| &**v)
    }
}

/// Whether a kind's rows came from a server Table or from the name/namespace/age fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListFormat {
    #[default]
    Table,
    /// The server did not return a Table: columns are Name, Namespace and Age only.
    Fallback,
}

/// Formats a duration like `kubectl get` does (`45s`, `5m30s`, `3h`, `2d5h`, `3y45d`).
pub fn human_duration(seconds: i64) -> String {
    if seconds < -1 {
        return "<invalid>".into();
    }
    let s = seconds.max(0);
    if s < 120 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 10 {
        return if s % 60 == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{}s", s % 60)
        };
    }
    if m < 180 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 8 {
        return if m % 60 == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{}m", m % 60)
        };
    }
    if h < 48 {
        return format!("{h}h");
    }
    let d = h / 24;
    if h < 24 * 8 {
        return if h % 24 == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{}h", h % 24)
        };
    }
    if h < 24 * 365 * 2 {
        return format!("{d}d");
    }
    let y = d / 365;
    if h < 24 * 365 * 8 {
        return if d % 365 == 0 {
            format!("{y}y")
        } else {
            format!("{y}y{}d", d % 365)
        };
    }
    format!("{y}y")
}

/// Parses a duration printed by [`human_duration`] (or the server's equivalent) into seconds.
pub(crate) fn parse_human_duration(text: &str) -> Option<i64> {
    let mut total: i64 = 0;
    let mut number: Option<i64> = None;
    let mut any = false;
    for c in text.trim().chars() {
        if let Some(d) = c.to_digit(10) {
            number = Some(
                number
                    .unwrap_or(0)
                    .checked_mul(10)?
                    .checked_add(i64::from(d))?,
            );
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'y' => 365 * 86_400,
            _ => return None,
        };
        total = total.checked_add(number.take()?.checked_mul(unit)?)?;
        any = true;
    }
    (any && number.is_none()).then_some(total)
}

/// Unix seconds of an RFC 3339 timestamp.
pub(crate) fn parse_timestamp(text: &str) -> Option<i64> {
    text.parse::<k8s_openapi::jiff::Timestamp>()
        .ok()
        .map(|t| t.as_second())
}

/// Unix seconds as an RFC 3339 timestamp (`2026-01-01T00:00:00Z`).
pub fn format_timestamp(seconds: i64) -> Option<String> {
    k8s_openapi::jiff::Timestamp::from_second(seconds)
        .ok()
        .map(|t| t.to_string())
}

/// The current time in Unix seconds.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------------------------
// Wire format

/// Any object a list or watch returns: a Table, a plain `*List` (fallback), a single plain object
/// (fallback watch event), or a `Status`. Unknown fields, `managedFields` included, are skipped
/// by the parser and never stored.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireObject {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub metadata: WireMeta,
    #[serde(default)]
    pub column_definitions: Option<Vec<WireColumn>>,
    #[serde(default)]
    pub rows: Option<Vec<WireRow>>,
    #[serde(default)]
    pub items: Option<Vec<WireItem>>,
    /// `Status` fields.
    #[serde(default)]
    pub code: Option<u16>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl WireObject {
    pub fn is_table(&self) -> bool {
        self.kind == "Table"
    }

    pub fn is_status(&self) -> bool {
        self.kind == "Status"
    }

    /// Whether this is the bookmark that ends a streaming list's initial events. The annotation
    /// sits on the Table's fake row object, or on the object itself for plain JSON.
    pub fn is_initial_events_end(&self) -> bool {
        let marked = |m: &WireMeta| {
            m.annotations
                .as_ref()
                .is_some_and(|a| a.initial_events_end.as_deref() == Some("true"))
        };
        marked(&self.metadata)
            || self
                .rows
                .iter()
                .flatten()
                .any(|r| r.object.as_ref().is_some_and(|o| marked(&o.metadata)))
    }

    /// A `Status` as a message, for errors.
    pub fn status_message(&self) -> String {
        let reason = self.reason.as_deref().unwrap_or("");
        let message = self.message.as_deref().unwrap_or("");
        match (reason.is_empty(), message.is_empty()) {
            (false, false) => format!("{reason}: {message}"),
            (false, true) => reason.to_owned(),
            (true, false) => message.to_owned(),
            (true, true) => format!("HTTP {}", self.code.unwrap_or(0)),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireMeta {
    #[serde(default)]
    pub resource_version: Option<String>,
    #[serde(default, rename = "continue")]
    pub continue_: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub creation_timestamp: Option<String>,
    #[serde(default)]
    pub generation: Option<i64>,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    /// Only the annotation Flint needs is parsed; the rest are skipped.
    #[serde(default)]
    pub annotations: Option<MarkerAnnotations>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct MarkerAnnotations {
    #[serde(default, rename = "k8s.io/initial-events-end")]
    pub initial_events_end: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireColumn {
    pub name: String,
    #[serde(rename = "type", default)]
    pub ty: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireRow {
    #[serde(default)]
    pub cells: Vec<Value>,
    #[serde(default)]
    pub object: Option<WireItem>,
}

/// A row's object (`PartialObjectMetadata`) or a fallback list item: only metadata is read.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireItem {
    #[serde(default)]
    pub metadata: WireMeta,
}

/// One line of a watch stream.
#[derive(Debug, Deserialize)]
pub(crate) struct WireEvent {
    #[serde(rename = "type")]
    pub ty: EventType,
    #[serde(default)]
    pub object: WireObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum EventType {
    Added,
    Modified,
    Deleted,
    Bookmark,
    Error,
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------------------------
// Building rows

/// Interns repeated strings (statuses, node names, label keys, namespaces) for one kind.
#[derive(Debug, Default)]
pub(crate) struct Interner {
    set: HashSet<Arc<str>>,
    inserts: usize,
}

impl Interner {
    pub fn intern(&mut self, s: &str) -> Arc<str> {
        if let Some(existing) = self.set.get(s) {
            return existing.clone();
        }
        let fresh: Arc<str> = s.into();
        self.set.insert(fresh.clone());
        self.inserts += 1;
        if self.inserts >= 4096 && self.inserts >= self.set.len() {
            self.prune();
        }
        fresh
    }

    /// Drops strings no row uses any more.
    pub fn prune(&mut self) {
        self.set.retain(|s| Arc::strong_count(s) > 1);
        self.inserts = 0;
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.set.len()
    }
}

/// How server cells map onto a kind's columns.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    /// The published columns, Namespace included.
    pub columns: Arc<[Column]>,
    /// Type and role of each server column, in server order.
    wire: Box<[(ColumnType, ColumnRole)]>,
    /// Where the synthetic Namespace cell goes, for namespaced kinds.
    namespace_at: Option<usize>,
    pub format: ListFormat,
}

impl Layout {
    /// The layout for a Table's column definitions. `previous` is reused when the columns did not
    /// change, so every snapshot shares one allocation.
    pub fn from_table(defs: &[WireColumn], namespaced: bool, previous: Option<&Layout>) -> Layout {
        let wire: Box<[(ColumnType, ColumnRole)]> = defs
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let ty = ColumnType::parse(&d.ty);
                let role = if i == 0 && (d.name == "Name" || d.format == "name") {
                    ColumnRole::Name
                } else if d.name.eq_ignore_ascii_case("age") {
                    ColumnRole::Age
                } else {
                    ColumnRole::Value
                };
                (ty, role)
            })
            .collect();
        let namespace_at = namespaced
            .then(|| usize::from(wire.first().is_some_and(|(_, r)| *r == ColumnRole::Name)));
        let mut columns: Vec<Column> = defs
            .iter()
            .zip(wire.iter())
            .map(|(d, (ty, role))| Column {
                name: d.name.as_str().into(),
                ty: *ty,
                format: d.format.as_str().into(),
                description: d.description.as_str().into(),
                priority: d.priority,
                role: *role,
            })
            .collect();
        if let Some(at) = namespace_at {
            columns.insert(
                at,
                Column::synthetic("Namespace", ColumnType::String, ColumnRole::Namespace),
            );
        }
        let columns = match previous {
            Some(p) if p.format == ListFormat::Table && *p.columns == *columns => p.columns.clone(),
            _ => columns.into(),
        };
        Layout {
            columns,
            wire,
            namespace_at,
            format: ListFormat::Table,
        }
    }

    /// Name, Namespace (for namespaced kinds) and Age, for servers that return no Table.
    pub fn fallback(namespaced: bool) -> Layout {
        let mut columns = vec![Column::synthetic(
            "Name",
            ColumnType::String,
            ColumnRole::Name,
        )];
        if namespaced {
            columns.push(Column::synthetic(
                "Namespace",
                ColumnType::String,
                ColumnRole::Namespace,
            ));
        }
        columns.push(Column::synthetic("Age", ColumnType::Date, ColumnRole::Age));
        Layout {
            columns: columns.into(),
            wire: Box::new([]),
            namespace_at: None,
            format: ListFormat::Fallback,
        }
    }

    fn cells(&self, wire_cells: Vec<Value>, interner: &mut Interner, now: i64) -> Box<[Cell]> {
        if self.format == ListFormat::Fallback {
            return self
                .columns
                .iter()
                .map(|c| match c.role {
                    ColumnRole::Name => Cell::Name,
                    ColumnRole::Namespace => Cell::Namespace,
                    _ => Cell::Age,
                })
                .collect();
        }
        let mut out = Vec::with_capacity(self.columns.len());
        let mut values = wire_cells.into_iter();
        for (i, (ty, role)) in self.wire.iter().enumerate() {
            if self.namespace_at == Some(i) {
                out.push(Cell::Namespace);
            }
            let value = values.next().unwrap_or(Value::Null);
            out.push(cell(value, *ty, *role, interner, now));
        }
        if self.namespace_at == Some(self.wire.len()) {
            out.push(Cell::Namespace);
        }
        out.into_boxed_slice()
    }
}

fn cell(value: Value, ty: ColumnType, role: ColumnRole, interner: &mut Interner, now: i64) -> Cell {
    match role {
        ColumnRole::Name => return Cell::Name,
        ColumnRole::Age => return Cell::Age,
        _ => {}
    }
    match value {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Cell::Int(i),
            None => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) if ty == ColumnType::Date => match parse_timestamp(&s) {
            Some(t) => Cell::Time(t),
            None => match parse_human_duration(&s) {
                Some(d) => Cell::Time(now - d),
                None => Cell::Text(interner.intern(&s)),
            },
        },
        Value::String(s) => Cell::Text(interner.intern(&s)),
        other => Cell::Text(interner.intern(&other.to_string())),
    }
}

/// Builds a row from an object's metadata and its server cells. `None` without a name (a
/// bookmark's fake row, for example).
pub(crate) fn build_row(
    layout: &Layout,
    meta: WireMeta,
    cells: Vec<Value>,
    interner: &mut Interner,
    now: i64,
) -> Option<Row> {
    let name = meta.name.filter(|n| !n.is_empty())?;
    let namespace = meta
        .namespace
        .filter(|n| !n.is_empty())
        .map(|n| interner.intern(&n));
    let uid = match meta.uid.filter(|u| !u.is_empty()) {
        Some(uid) => uid.into_boxed_str(),
        None => match &namespace {
            Some(ns) => format!("{ns}/{name}").into_boxed_str(),
            None => name.clone().into_boxed_str(),
        },
    };
    let labels = meta
        .labels
        .unwrap_or_default()
        .iter()
        .map(|(k, v)| (interner.intern(k), interner.intern(v)))
        .collect();
    Some(Row {
        uid,
        name: name.into_boxed_str(),
        namespace,
        resource_version: meta.resource_version.unwrap_or_default().into_boxed_str(),
        generation: meta.generation,
        created: meta.creation_timestamp.as_deref().and_then(parse_timestamp),
        labels,
        cells: layout.cells(cells, interner, now),
    })
}

/// The layout `object` asks for, if it differs from `current`: its column definitions, or the
/// fallback layout when it is not a Table. `None` keeps `current`. Columns equal to the current
/// ones reuse its allocation, so callers can compare layouts with [`Arc::ptr_eq`].
pub(crate) fn next_layout(
    object: &WireObject,
    namespaced: bool,
    current: Option<&Layout>,
) -> Option<Layout> {
    if object.is_table() {
        let defs = object
            .column_definitions
            .as_deref()
            .filter(|d| !d.is_empty())?;
        let next = Layout::from_table(defs, namespaced, current);
        let same = current
            .is_some_and(|c| c.format == next.format && Arc::ptr_eq(&c.columns, &next.columns));
        return (!same).then_some(next);
    }
    current
        .is_none_or(|l| l.format != ListFormat::Fallback)
        .then(|| Layout::fallback(namespaced))
}

/// The rows of one list response or watch event, built with `layout` (from [`next_layout`]).
/// Empty when no layout is known yet: a Table's cells cannot be mapped without one.
pub(crate) fn rows_with(
    object: WireObject,
    layout: Option<&Layout>,
    interner: &mut Interner,
    now: i64,
) -> Vec<Row> {
    let Some(layout) = layout else {
        return Vec::new();
    };
    if object.is_table() {
        return object
            .rows
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                let meta = r.object.map(|o| o.metadata).unwrap_or_default();
                build_row(layout, meta, r.cells, interner, now)
            })
            .collect();
    }
    match object.items {
        // A plain `*List`.
        Some(items) => items
            .into_iter()
            .filter_map(|i| build_row(layout, i.metadata, Vec::new(), interner, now))
            .collect(),
        // A plain object (fallback watch event).
        None => build_row(layout, object.metadata, Vec::new(), interner, now)
            .into_iter()
            .collect(),
    }
}

/// [`next_layout`] then [`rows_with`], updating `layout` in place.
pub(crate) fn object_rows(
    object: WireObject,
    namespaced: bool,
    layout: &mut Option<Layout>,
    interner: &mut Interner,
    now: i64,
) -> Vec<Row> {
    if let Some(next) = next_layout(&object, namespaced, layout.as_ref()) {
        *layout = Some(next);
    }
    rows_with(object, layout.as_ref(), interner, now)
}

// ---------------------------------------------------------------------------------------------
// Requests

/// Why a list or watch request failed, by what the caller should do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchError {
    /// 410: the resource version or continue token is too old. Relist.
    Expired(String),
    /// 405: the API cannot do this (metrics.k8s.io cannot watch). Poll instead.
    MethodNotAllowed(String),
    /// 403: the user may not do this. Can be transient while an API server's authorizer warms
    /// up after a restart, so callers retry a few times before trusting it.
    Denied(String),
    /// 404: the API is not served (a CRD was deleted, or is being recreated).
    NotFound(String),
    /// 400 or 422: the server rejected a parameter (streaming lists on a server without them).
    Invalid(String),
    Other(String),
}

impl FetchError {
    /// A failed request to `target`'s server, in [`ClusterError::kube`]'s words.
    pub fn from_kube(err: kube::Error, target: &cluster::Target) -> Self {
        PageError::from_kube(err).into_fetch(target)
    }

    pub fn from_code(code: u16, message: String) -> Self {
        Self::from_kind(ApiErrorKind::from_code(code), message)
    }

    fn from_kind(kind: ApiErrorKind, message: String) -> Self {
        match kind {
            ApiErrorKind::Gone => FetchError::Expired(message),
            ApiErrorKind::MethodNotAllowed => FetchError::MethodNotAllowed(message),
            ApiErrorKind::Forbidden => FetchError::Denied(message),
            ApiErrorKind::NotFound => FetchError::NotFound(message),
            ApiErrorKind::BadRequest | ApiErrorKind::UnprocessableEntity => {
                FetchError::Invalid(message)
            }
            ApiErrorKind::Unauthorized | ApiErrorKind::Other => FetchError::Other(message),
        }
    }

    pub fn message(&self) -> &str {
        match self {
            FetchError::Expired(m)
            | FetchError::MethodNotAllowed(m)
            | FetchError::Denied(m)
            | FetchError::NotFound(m)
            | FetchError::Invalid(m)
            | FetchError::Other(m) => m,
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// The collection URL of a kind, across all namespaces (`None`) or in one.
pub(crate) fn collection_path(kind: &KindInfo, namespace: Option<&str>) -> String {
    let namespace = namespace.filter(|_| kind.namespaced);
    DynamicObject::url_path(&kind.resource, namespace)
}

/// Turns a kube request into a Table request: Table Accept header and `includeObject=Metadata`.
pub(crate) fn as_table_request(
    mut req: http::Request<Vec<u8>>,
) -> Result<http::Request<Vec<u8>>, FetchError> {
    let uri = format!("{}&includeObject=Metadata", req.uri());
    *req.uri_mut() = uri
        .parse()
        .map_err(|e| FetchError::Other(format!("bad request URI: {e}")))?;
    req.headers_mut().insert(
        "accept",
        TABLE_ACCEPT
            .parse()
            .map_err(|e| FetchError::Other(format!("bad header: {e}")))?,
    );
    Ok(req)
}

/// Why a page request failed: how the API classified it ([`ApiErrorKind::Other`] when there
/// was no API status) and the error, which can still be told whose server it was
/// ([`ClusterError::for_target`]).
#[derive(Debug)]
pub(crate) struct PageError {
    pub kind: ApiErrorKind,
    pub error: ClusterError,
}

impl PageError {
    fn from_kube(err: kube::Error) -> Self {
        let kind = ApiError::from_kube(&err).map_or(ApiErrorKind::Other, |api| api.kind());
        PageError {
            kind,
            error: ClusterError::kube(err),
        }
    }

    /// The error as the watch engine sorts it, naming `target`'s server.
    pub fn into_fetch(self, target: &cluster::Target) -> FetchError {
        FetchError::from_kind(self.kind, self.error.for_target(target).to_string())
    }
}

impl From<FetchError> for PageError {
    fn from(err: FetchError) -> Self {
        PageError {
            kind: ApiErrorKind::Other,
            error: ClusterError::Kube(err.to_string()),
        }
    }
}

/// One page of a Table list.
pub(crate) async fn fetch_page(
    client: &Client,
    path: &str,
    params: &ListParams,
) -> Result<WireObject, PageError> {
    let req = Request::new(path)
        .list(params)
        .map_err(|e| FetchError::Other(cluster::error_chain(&e)))?;
    let req = as_table_request(req)?;
    let object: WireObject = client.request(req).await.map_err(PageError::from_kube)?;
    if object.is_status() {
        return Err(PageError {
            kind: ApiErrorKind::from_code(object.code.unwrap_or(0)),
            error: ClusterError::Kube(object.status_message()),
        });
    }
    Ok(object)
}

/// One page of a one-shot Table list.
#[derive(Debug, Clone)]
pub struct TablePage {
    pub columns: Arc<[Column]>,
    /// In server order (namespace, then name).
    pub rows: Vec<Row>,
    pub format: ListFormat,
    pub more: bool,
    /// Pass as [`ListQuery::continue_token`] to fetch the next page.
    pub continue_token: Option<String>,
}

/// Lists a kind once, as a Table: the columns `kubectl get` shows, CRD printer columns included.
/// Falls back to name/namespace/age columns when the server returns no Table. For live lists use
/// the watch manager instead ([`crate::CoreHandle::subscribe`]).
pub async fn list_table(
    client: &Client,
    kind: &KindInfo,
    query: &ListQuery,
) -> Result<TablePage, ClusterError> {
    let path = collection_path(kind, query.namespace.as_deref());
    let mut object = fetch_page(client, &path, &query.to_params())
        .await
        .map_err(|e| e.error)?;
    let token = object.metadata.continue_.take();
    let mut layout = None;
    let mut interner = Interner::default();
    let mut rows = object_rows(
        object,
        kind.namespaced,
        &mut layout,
        &mut interner,
        unix_now(),
    );
    let end = paging::enforce_limit(&mut rows, query.limit, token);
    let layout = layout.unwrap_or_else(|| Layout::fallback(kind.namespaced));
    Ok(TablePage {
        columns: layout.columns,
        rows,
        format: layout.format,
        more: end.more,
        continue_token: end.continue_token,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A Table watch event or list, as the server sends it (see the spike).
    pub(crate) fn pod_table(
        rows: &[(&str, &str, &str, &str)],
        with_columns: bool,
        rv: &str,
    ) -> Value {
        let columns = json!([
            {"name":"Name","type":"string","format":"name","description":"","priority":0},
            {"name":"Ready","type":"string","format":"","description":"","priority":0},
            {"name":"Status","type":"string","format":"","description":"","priority":0},
            {"name":"Restarts","type":"string","format":"","description":"","priority":0},
            {"name":"Age","type":"string","format":"","description":"","priority":0},
            {"name":"Node","type":"string","format":"","description":"","priority":1}
        ]);
        let rows: Vec<Value> = rows
            .iter()
            .map(|(uid, ns, name, status)| {
                json!({
                    "cells": [name, "1/1", status, "0", "81s", "node-1"],
                    "object": {"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
                        "metadata": {"name": name, "namespace": ns, "uid": uid, "resourceVersion": rv,
                            "creationTimestamp": "2026-01-01T00:00:00Z", "labels": {"app": "web"},
                            "annotations": {"big": "x".repeat(100)},
                            "managedFields": [{"manager":"kubectl","fieldsV1":{"f:spec":{}}}]}}
                })
            })
            .collect();
        json!({"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion": rv},
            "columnDefinitions": if with_columns { columns } else { Value::Null }, "rows": rows})
    }

    fn parse(v: Value) -> WireObject {
        serde_json::from_value(v).unwrap()
    }

    // covers UP-20
    #[test]
    fn table_rows_keep_identity_and_drop_heavy_metadata() {
        let mut layout = None;
        let mut interner = Interner::default();
        let rows = object_rows(
            parse(pod_table(&[("u1", "demo", "web-1", "Running")], true, "10")),
            true,
            &mut layout,
            &mut interner,
            1_767_225_700,
        );
        let layout = layout.unwrap();
        let names: Vec<&str> = layout.columns.iter().map(|c| &*c.name).collect();
        assert_eq!(
            names,
            [
                "Name",
                "Namespace",
                "Ready",
                "Status",
                "Restarts",
                "Age",
                "Node"
            ]
        );
        assert_eq!(layout.columns[5].role, ColumnRole::Age);
        assert_eq!(layout.columns[6].priority, 1);
        let row = &rows[0];
        assert_eq!(&*row.uid, "u1");
        assert_eq!(row.namespace.as_deref(), Some("demo"));
        assert_eq!(&*row.resource_version, "10");
        assert_eq!(row.created, Some(1_767_225_600));
        assert_eq!(row.label("app"), Some("web"));
        assert_eq!(row.cells[0], Cell::Name);
        assert_eq!(row.cells[1], Cell::Namespace);
        // Age is computed, never the server's frozen "81s".
        assert_eq!(row.cells[5], Cell::Age);
        assert_eq!(row.cell_text(5, 1_767_225_700), "100s");
        assert_eq!(row.cell_text(3, 0), "Running");
    }

    // covers UP-20
    #[test]
    fn only_the_first_event_has_column_definitions() {
        let mut layout = None;
        let mut interner = Interner::default();
        object_rows(
            parse(pod_table(&[("u1", "demo", "a", "Running")], true, "1")),
            true,
            &mut layout,
            &mut interner,
            0,
        );
        let first = layout.clone().unwrap().columns;
        let rows = object_rows(
            parse(pod_table(&[("u2", "demo", "b", "Pending")], false, "2")),
            true,
            &mut layout,
            &mut interner,
            0,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cell_text(3, 0), "Pending");
        assert!(Arc::ptr_eq(&first, &layout.as_ref().unwrap().columns));
        // Identical definitions again (every list page repeats them): still the same allocation.
        object_rows(
            parse(pod_table(&[("u3", "demo", "c", "Running")], true, "3")),
            true,
            &mut layout,
            &mut interner,
            0,
        );
        assert!(Arc::ptr_eq(&first, &layout.as_ref().unwrap().columns));
        // Repeated strings are shared between rows.
        assert!(interner.len() < 10, "{}", interner.len());
    }

    // covers UP-22
    #[test]
    fn crd_columns_keep_types() {
        let v = json!({"kind":"Table","metadata":{"resourceVersion":"5"},
            "columnDefinitions":[
                {"name":"Name","type":"string","format":"name","priority":0},
                {"name":"Size","type":"integer","format":"","priority":0},
                {"name":"Color","type":"string","format":"","priority":1},
                {"name":"Seen","type":"date","format":"","priority":0},
                {"name":"Age","type":"date","format":"","priority":0}],
            "rows":[{"cells":["w1",1,"red","5m","5s"],"object":{"metadata":{"name":"w1","uid":"x"}}}]});
        let mut layout = None;
        let rows = object_rows(parse(v), false, &mut layout, &mut Interner::default(), 1000);
        let layout = layout.unwrap();
        assert!(
            layout
                .columns
                .iter()
                .all(|c| c.role != ColumnRole::Namespace)
        );
        assert_eq!(layout.columns[1].ty, ColumnType::Integer);
        assert_eq!(rows[0].cells[1], Cell::Int(1));
        assert_eq!(rows[0].cells[3], Cell::Time(1000 - 300));
        assert_eq!(rows[0].cells[4], Cell::Age);
        assert_eq!(rows[0].cell_text(3, 1000 + 60), "6m");
    }

    // covers UP-28
    #[test]
    fn non_table_responses_fall_back_to_name_namespace_age() {
        let list = json!({"kind":"PodMetricsList","metadata":{},
            "items":[{"metadata":{"name":"p","namespace":"demo","creationTimestamp":"2026-01-01T00:00:00Z"},
                      "containers":[{"name":"c","usage":{"cpu":"1m"}}]}]});
        let mut layout = None;
        let rows = object_rows(parse(list), true, &mut layout, &mut Interner::default(), 0);
        let layout = layout.unwrap();
        assert_eq!(layout.format, ListFormat::Fallback);
        let names: Vec<&str> = layout.columns.iter().map(|c| &*c.name).collect();
        assert_eq!(names, ["Name", "Namespace", "Age"]);
        assert_eq!(&*rows[0].uid, "demo/p", "no uid: keyed by namespace/name");
        assert_eq!(&*rows[0].cells, &[Cell::Name, Cell::Namespace, Cell::Age]);
        // A plain watch event object.
        let event = json!({"kind":"Widget","metadata":{"name":"w","uid":"u"},"spec":{}});
        let rows = object_rows(parse(event), false, &mut None, &mut Interner::default(), 0);
        assert_eq!(&*rows[0].uid, "u");
    }

    // covers UP-20
    #[test]
    fn bookmark_rows_are_never_rows() {
        let bookmark = json!({"kind":"Table","metadata":{"resourceVersion":"846"},"columnDefinitions":null,
            "rows":[{"cells":["","0/0","","0","<unknown>","<none>"],
            "object":{"kind":"PartialObjectMetadata","metadata":{"resourceVersion":"846",
                "annotations":{"k8s.io/initial-events-end":"true"}}}}]});
        let object = parse(bookmark);
        assert!(object.is_initial_events_end());
        let mut layout = Some(Layout::fallback(true));
        assert!(object_rows(object, true, &mut layout, &mut Interner::default(), 0).is_empty());
        let plain = parse(pod_table(&[("u", "d", "n", "s")], true, "1"));
        assert!(!plain.is_initial_events_end());
    }

    #[test]
    fn durations_round_trip_like_kubectl() {
        for (secs, text) in [
            (0, "0s"),
            (81, "81s"),
            (330, "5m30s"),
            (600, "10m"),
            (3 * 3600 + 120, "3h2m"),
            (30 * 3600, "30h"),
            (2 * 86_400 + 5 * 3600, "2d5h"),
            (40 * 86_400, "40d"),
            (3 * 365 * 86_400 + 45 * 86_400, "3y45d"),
        ] {
            assert_eq!(human_duration(secs), text);
            assert_eq!(parse_human_duration(text), Some(secs), "{text}");
        }
        assert_eq!(human_duration(-5), "<invalid>");
        assert_eq!(parse_human_duration("<unknown>"), None);
        assert_eq!(parse_human_duration("5"), None);
        assert_eq!(parse_human_duration(""), None);
    }

    #[test]
    fn status_errors_map_to_actions() {
        let expired = json!({"type":"ERROR","object":{"kind":"Status","apiVersion":"v1","metadata":{},
            "status":"Failure","message":"too old resource version: 1 (773)","reason":"Expired","code":410}});
        let event: WireEvent = serde_json::from_value(expired).unwrap();
        assert_eq!(event.ty, EventType::Error);
        assert!(event.object.is_status());
        let err = FetchError::from_code(event.object.code.unwrap(), event.object.status_message());
        assert!(matches!(err, FetchError::Expired(m) if m.contains("too old")));
        assert!(matches!(
            FetchError::from_code(405, String::new()),
            FetchError::MethodNotAllowed(_)
        ));
        assert!(matches!(
            FetchError::from_code(403, String::new()),
            FetchError::Denied(_)
        ));
        assert!(matches!(
            FetchError::from_code(404, String::new()),
            FetchError::NotFound(_)
        ));
        let unknown: WireEvent =
            serde_json::from_str(r#"{"type":"WHATEVER","object":{}}"#).unwrap();
        assert_eq!(unknown.ty, EventType::Unknown);
    }
}
