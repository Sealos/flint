//! Read-only access to cluster objects through kube's dynamic API: discovery, list, get, events,
//! and pod logs.
//!
//! These functions apply no policy. Callers decide limits, redaction, and who may see what (the
//! MCP server does this in `flint-mcp`; the UI will use these directly).

use futures::AsyncReadExt as _;
use k8s_openapi::api::core::v1::{Event, Pod};
use kube::Client;
use kube::api::{Api, ApiResource, DynamicObject, ListParams, LogParams};
use kube::discovery::{ApiCapabilities, Discovery, Scope, verbs};

use crate::cluster::ClusterError;
use crate::paging;

/// One kind found by discovery, at its group's preferred version.
#[derive(Debug, Clone)]
pub struct KindInfo {
    pub resource: ApiResource,
    pub namespaced: bool,
    pub verbs: Vec<String>,
}

impl KindInfo {
    pub fn group(&self) -> &str {
        &self.resource.group
    }
    pub fn kind(&self) -> &str {
        &self.resource.kind
    }
    pub fn plural(&self) -> &str {
        &self.resource.plural
    }
    pub fn api_version(&self) -> &str {
        &self.resource.api_version
    }
    pub fn supports(&self, verb: &str) -> bool {
        self.verbs.iter().any(|v| v == verb)
    }
    pub fn is_listable(&self) -> bool {
        self.supports(verbs::LIST)
    }
    /// Whether the kind supports `watch` (metrics.k8s.io, for one, does not).
    pub fn is_watchable(&self) -> bool {
        self.supports(verbs::WATCH)
    }
    /// The kind's stable identity across discovery refreshes: `plural.group` (`pods`,
    /// `deployments.apps`), as kubectl names resources.
    pub fn id(&self) -> KindId {
        KindId::new(self.group(), self.plural())
    }
}

/// A kind's identity, independent of the served version: `plural` for the core group, otherwise
/// `plural.group` (`deployments.apps`, `widgets.example.com`). Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct KindId(std::sync::Arc<str>);

impl KindId {
    pub fn new(group: &str, plural: &str) -> Self {
        if group.is_empty() {
            KindId(plural.into())
        } else {
            KindId(format!("{plural}.{group}").into())
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The plural resource name.
    pub fn plural(&self) -> &str {
        self.0.split_once('.').map_or(&self.0, |(p, _)| p)
    }
    /// The API group (empty for the core group).
    pub fn group(&self) -> &str {
        self.0.split_once('.').map_or("", |(_, g)| g)
    }
    pub fn pods() -> Self {
        Self::new("", "pods")
    }
    pub fn namespaces() -> Self {
        Self::new("", "namespaces")
    }
    pub fn events() -> Self {
        Self::new("", "events")
    }
    pub fn custom_resource_definitions() -> Self {
        Self::new("apiextensions.k8s.io", "customresourcedefinitions")
    }
}

impl std::fmt::Display for KindId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Runs API discovery and returns every kind, CRDs included, at each group's preferred version.
/// Core kinds come first. Uses aggregated discovery when the server supports it (1.30+), which
/// needs two requests instead of one per group version.
pub async fn discover_kinds(client: &Client) -> Result<Vec<KindInfo>, ClusterError> {
    let discovery = match Discovery::new(client.clone()).run_aggregated().await {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "aggregated discovery failed, falling back");
            Discovery::new(client.clone())
                .run()
                .await
                .map_err(ClusterError::kube)?
        }
    };
    let mut kinds: Vec<KindInfo> = discovery
        .groups()
        .flat_map(|g| g.recommended_resources())
        .map(|(resource, caps)| kind_info(resource, &caps))
        .collect();
    // `Discovery` keeps groups in a map; put the core group first, then sort by group and kind.
    kinds.sort_by(|a, b| {
        (!a.group().is_empty(), a.group(), a.kind()).cmp(&(
            !b.group().is_empty(),
            b.group(),
            b.kind(),
        ))
    });
    Ok(kinds)
}

fn kind_info(resource: ApiResource, caps: &ApiCapabilities) -> KindInfo {
    KindInfo {
        resource,
        namespaced: caps.scope == Scope::Namespaced,
        verbs: caps.operations.clone(),
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveKindError {
    #[error("no resource kind matches \"{0}\"")]
    NotFound(String),
}

/// Finds a kind by name the way people type it: `Pod`, `pods`, `pod`, `deployments.apps`, or
/// `Deployment.apps`. Matching is case-insensitive. `api_version` (`apps/v1`, `v1`, or a bare
/// group like `apps`) narrows the match when a kind exists in several groups. Without it, the
/// first match in discovery order wins, so core kinds beat same-named kinds in other groups.
pub fn resolve_kind<'a>(
    kinds: &'a [KindInfo],
    query: &str,
    api_version: Option<&str>,
) -> Result<&'a KindInfo, ResolveKindError> {
    let query = query.trim();
    let (name, group_suffix) = match query.split_once('.') {
        Some((n, g)) => (n, Some(g)),
        None => (query, None),
    };
    let matches_name = |k: &KindInfo| {
        k.kind().eq_ignore_ascii_case(name)
            || k.plural().eq_ignore_ascii_case(name)
            || (name.len() > 1
                && name.ends_with(['s', 'S'])
                && k.kind().eq_ignore_ascii_case(&name[..name.len() - 1]))
    };
    let matches_group = |k: &KindInfo| match group_suffix {
        Some(g) => k.group().eq_ignore_ascii_case(g),
        None => true,
    };
    let matches_version = |k: &KindInfo| match api_version.map(str::trim).filter(|v| !v.is_empty())
    {
        None => true,
        Some(v) if v.contains('/') => k.api_version().eq_ignore_ascii_case(v),
        // A bare value is either the core version ("v1") or a group name ("apps").
        Some(v) => {
            (k.group().is_empty() && k.resource.version.eq_ignore_ascii_case(v))
                || k.group().eq_ignore_ascii_case(v)
        }
    };
    kinds
        .iter()
        .find(|k| matches_name(k) && matches_group(k) && matches_version(k))
        .ok_or_else(|| ResolveKindError::NotFound(query.to_owned()))
}

/// Filters for [`list`].
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Namespace to list in. `None` lists across all namespaces (ignored for cluster-scoped kinds).
    pub namespace: Option<String>,
    pub label_selector: Option<String>,
    pub field_selector: Option<String>,
    /// Maximum number of objects the server returns. `None` means no limit.
    pub limit: Option<u32>,
    /// The `continue` token of the previous page, to fetch the next one.
    pub continue_token: Option<String>,
}

impl ListQuery {
    /// The request parameters for this query (selectors, limit, continue token). The namespace
    /// is part of the URL, not a parameter.
    pub fn to_params(&self) -> ListParams {
        let mut params = ListParams::default();
        if let Some(s) = &self.label_selector {
            params = params.labels(s);
        }
        if let Some(s) = &self.field_selector {
            params = params.fields(s);
        }
        if let Some(limit) = self.limit {
            params = params.limit(limit);
        }
        if let Some(token) = self.continue_token.as_deref().filter(|t| !t.is_empty()) {
            params = params.continue_token(token);
        }
        params
    }
}

/// One page of a list.
#[derive(Debug, Clone)]
pub struct ListPage {
    pub items: Vec<DynamicObject>,
    /// The server has more objects than were returned (the list hit `limit`).
    pub more: bool,
    /// Pass as [`ListQuery::continue_token`] to fetch the next page. `None` when there is no next
    /// page, or when the server ignored `limit` and the page was cut here (then narrow the query).
    pub continue_token: Option<String>,
}

fn dynamic_api(client: &Client, kind: &KindInfo, namespace: Option<&str>) -> Api<DynamicObject> {
    match namespace {
        Some(ns) if kind.namespaced => Api::namespaced_with(client.clone(), ns, &kind.resource),
        _ => Api::all_with(client.clone(), &kind.resource),
    }
}

/// Lists objects of a kind.
pub async fn list(
    client: &Client,
    kind: &KindInfo,
    query: &ListQuery,
) -> Result<ListPage, ClusterError> {
    let api = dynamic_api(client, kind, query.namespace.as_deref());
    let list = api
        .list(&query.to_params())
        .await
        .map_err(ClusterError::kube)?;
    let mut items = list.items;
    let end = paging::enforce_limit(&mut items, query.limit, list.metadata.continue_);
    Ok(ListPage {
        items,
        more: end.more,
        continue_token: end.continue_token,
    })
}

/// Gets one object. `namespace` is required for namespaced kinds and ignored otherwise.
pub async fn get(
    client: &Client,
    kind: &KindInfo,
    namespace: Option<&str>,
    name: &str,
) -> Result<DynamicObject, ClusterError> {
    if kind.namespaced && namespace.is_none() {
        return Err(ClusterError::Kube(format!(
            "{} is namespaced; a namespace is required",
            kind.kind()
        )));
    }
    dynamic_api(client, kind, namespace)
        .get(name)
        .await
        .map_err(ClusterError::kube)
}

/// Events fetched for one object's detail view.
const DETAIL_EVENTS: u32 = 200;

/// One object for a detail view.
#[derive(Debug, Clone)]
pub struct ObjectDetail {
    /// The full object, `metadata.managedFields` removed. `apiVersion` and `kind` are set.
    pub object: serde_json::Value,
    /// `object` as YAML.
    pub yaml: String,
    /// The `metadata.managedFields` that were removed, if the object had any. Hidden by default;
    /// a view can put them back on request ([`with_managed_fields`]).
    pub managed_fields: Option<serde_json::Value>,
    /// Events whose `involvedObject.uid` is this object, newest first.
    pub events: Vec<Event>,
    /// More events existed than were fetched.
    pub events_truncated: bool,
    /// Why the events could not be read (for example, the user may get the object but not list
    /// events). The object is still returned.
    pub events_error: Option<String>,
}

/// A copy of `object` with `managed_fields` put back as `metadata.managedFields`.
pub fn with_managed_fields(
    object: &serde_json::Value,
    managed_fields: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut copy = object.clone();
    if let (Some(fields), Some(meta)) = (
        managed_fields,
        copy.get_mut("metadata").and_then(|m| m.as_object_mut()),
    ) {
        meta.insert("managedFields".to_owned(), fields.clone());
    }
    copy
}

/// Fetches one object and the events about it (matched by uid, so events of an older object
/// with the same name are left out). `namespace` is required for namespaced kinds.
pub async fn object_detail(
    client: &Client,
    kind: &KindInfo,
    namespace: Option<&str>,
    name: &str,
) -> Result<ObjectDetail, ClusterError> {
    let object = get(client, kind, namespace, name).await?;
    let uid = object.metadata.uid.clone();
    let mut value = serde_json::to_value(&object)
        .map_err(|e| ClusterError::Kube(format!("cannot serialize the object: {e}")))?;
    let managed_fields = value
        .get_mut("metadata")
        .and_then(|m| m.as_object_mut())
        .and_then(|m| m.remove("managedFields"));
    if value.get("apiVersion").is_none() {
        value["apiVersion"] = kind.api_version().into();
        value["kind"] = kind.kind().into();
    }
    let yaml = serde_saphyr::to_string(&value)
        .map_err(|e| ClusterError::Kube(format!("cannot render YAML: {e}")))?;
    let (events, events_truncated, events_error) = match uid {
        Some(uid) => {
            let selector = format!("involvedObject.uid={uid}");
            let found = events(
                client,
                &EventQuery {
                    // Events about cluster-scoped objects (nodes) are in some namespace
                    // ("default" usually), so those are searched in all of them.
                    namespace: object.metadata.namespace.clone(),
                    involved: None,
                    field_selector: Some(selector),
                    fetch_limit: Some(DETAIL_EVENTS),
                },
            )
            .await;
            match found {
                Ok((events, truncated)) => (events, truncated, None),
                // Many roles may read an object but not list events: still show the object.
                Err(e) => (Vec::new(), false, Some(e.to_string())),
            }
        }
        None => (Vec::new(), false, None),
    };
    Ok(ObjectDetail {
        object: value,
        yaml,
        managed_fields,
        events,
        events_truncated,
        events_error,
    })
}

/// Removes `metadata.managedFields`, which is noise in every view.
pub fn strip_managed_fields(value: &mut serde_json::Value) {
    if let Some(meta) = value.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.remove("managedFields");
    }
}

/// Filters on the object an event is about, for [`events`].
#[derive(Debug, Clone, Default)]
pub struct InvolvedObject {
    pub kind: Option<String>,
    pub name: Option<String>,
}

impl InvolvedObject {
    /// The field selector, or `None` if no filter is set. Values are passed as given; a selector
    /// can only narrow a read-only list.
    fn field_selector(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(name) = &self.name {
            parts.push(format!("involvedObject.name={name}"));
        }
        if let Some(kind) = &self.kind {
            parts.push(format!("involvedObject.kind={kind}"));
        }
        (!parts.is_empty()).then(|| parts.join(","))
    }
}

#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    /// `None` means all namespaces.
    pub namespace: Option<String>,
    pub involved: Option<InvolvedObject>,
    /// An extra field selector, combined (AND) with `involved` (for example `involvedObject.uid=...`).
    pub field_selector: Option<String>,
    /// Maximum events fetched from the server (in pages) before sorting. `None` means all.
    pub fetch_limit: Option<u32>,
}

/// Page size when fetching events.
const EVENTS_PAGE: u32 = 500;

/// Core v1 events, newest first. The server returns events in name order, not time order, so all
/// of them (up to `fetch_limit`) are fetched before sorting. `more` is set when the server had
/// more than `fetch_limit`, in which case the newest events may be missing.
pub async fn events(
    client: &Client,
    query: &EventQuery,
) -> Result<(Vec<Event>, bool), ClusterError> {
    let api: Api<Event> = match &query.namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    };
    let mut params = ListParams::default().limit(EVENTS_PAGE);
    let selectors: Vec<String> = query
        .involved
        .as_ref()
        .and_then(InvolvedObject::field_selector)
        .into_iter()
        .chain(query.field_selector.clone())
        .collect();
    if !selectors.is_empty() {
        params = params.fields(&selectors.join(","));
    }
    let cap = query.fetch_limit.map_or(usize::MAX, |l| l as usize);
    let mut items: Vec<Event> = Vec::new();
    let mut more = false;
    loop {
        let page = api.list(&params).await.map_err(ClusterError::kube)?;
        items.extend(page.items);
        let next = page.metadata.continue_.filter(|c| !c.is_empty());
        if items.len() >= cap {
            more = next.is_some() || items.len() > cap;
            items.truncate(cap);
            break;
        }
        match next {
            Some(token) => params = params.continue_token(&token),
            None => break,
        }
    }
    items.sort_by_key(|e| std::cmp::Reverse(event_time(e)));
    Ok((items, more))
}

/// The most recent time an event happened, from whichever field the producer filled in.
pub fn event_time(event: &Event) -> Option<k8s_openapi::jiff::Timestamp> {
    event
        .last_timestamp
        .as_ref()
        .map(|t| t.0)
        .or_else(|| event.event_time.as_ref().map(|t| t.0))
        .or_else(|| event.first_timestamp.as_ref().map(|t| t.0))
        .or_else(|| event.metadata.creation_timestamp.as_ref().map(|t| t.0))
}

#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    pub tail_lines: Option<i64>,
    pub previous: bool,
    pub since_seconds: Option<i64>,
    pub limit_bytes: Option<i64>,
}

/// Fetches a pod's logs (not following). Bytes that are not valid UTF-8, including a character
/// cut in half by `limit_bytes`, become U+FFFD instead of failing the whole fetch.
pub async fn pod_logs(client: &Client, query: &LogQuery) -> Result<String, ClusterError> {
    let api: Api<Pod> = Api::namespaced(client.clone(), &query.namespace);
    let params = LogParams {
        container: query.container.clone(),
        tail_lines: query.tail_lines,
        previous: query.previous,
        since_seconds: query.since_seconds,
        limit_bytes: query.limit_bytes,
        ..LogParams::default()
    };
    let mut stream = api
        .log_stream(&query.pod, &params)
        .await
        .map_err(ClusterError::kube)?;
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| ClusterError::Kube(format!("reading logs failed: {e}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(group: &str, version: &str, kind: &str, plural: &str) -> KindInfo {
        let api_version = if group.is_empty() {
            version.to_owned()
        } else {
            format!("{group}/{version}")
        };
        KindInfo {
            resource: ApiResource {
                group: group.into(),
                version: version.into(),
                api_version,
                kind: kind.into(),
                plural: plural.into(),
            },
            namespaced: true,
            verbs: vec!["list".into()],
        }
    }

    fn kinds() -> Vec<KindInfo> {
        vec![
            kind("", "v1", "Pod", "pods"),
            kind("", "v1", "Event", "events"),
            kind("apps", "v1", "Deployment", "deployments"),
            kind("events.k8s.io", "v1", "Event", "events"),
            kind("networking.k8s.io", "v1", "Ingress", "ingresses"),
        ]
    }

    // covers UP-10
    #[test]
    fn resolves_by_kind_plural_and_singular() {
        let kinds = kinds();
        for q in ["Pod", "pod", "pods", "PODS"] {
            assert_eq!(resolve_kind(&kinds, q, None).unwrap().kind(), "Pod", "{q}");
        }
        assert_eq!(
            resolve_kind(&kinds, "ingresses", None).unwrap().kind(),
            "Ingress"
        );
        assert_eq!(
            resolve_kind(&kinds, "deployments.apps", None)
                .unwrap()
                .group(),
            "apps"
        );
    }

    // covers UP-10
    #[test]
    fn core_wins_unless_group_given() {
        let kinds = kinds();
        assert_eq!(resolve_kind(&kinds, "Event", None).unwrap().group(), "");
        assert_eq!(
            resolve_kind(&kinds, "Event", Some("events.k8s.io/v1"))
                .unwrap()
                .group(),
            "events.k8s.io"
        );
        assert_eq!(
            resolve_kind(&kinds, "events.events.k8s.io", None)
                .unwrap()
                .group(),
            "events.k8s.io"
        );
        assert_eq!(
            resolve_kind(&kinds, "Event", Some("v1")).unwrap().group(),
            ""
        );
    }

    // covers UP-10
    #[test]
    fn involved_object_selector() {
        let both = InvolvedObject {
            kind: Some("Pod".into()),
            name: Some("web".into()),
        };
        assert_eq!(
            both.field_selector().as_deref(),
            Some("involvedObject.name=web,involvedObject.kind=Pod")
        );
        let kind_only = InvolvedObject {
            kind: Some("Node".into()),
            name: None,
        };
        assert_eq!(
            kind_only.field_selector().as_deref(),
            Some("involvedObject.kind=Node")
        );
        assert_eq!(InvolvedObject::default().field_selector(), None);
    }

    // covers UP-10
    #[test]
    fn unknown_kind_is_not_found() {
        assert_eq!(
            resolve_kind(&kinds(), "Widget", None).unwrap_err(),
            ResolveKindError::NotFound("Widget".into())
        );
        assert!(resolve_kind(&kinds(), "Pod", Some("apps/v1")).is_err());
    }
}
