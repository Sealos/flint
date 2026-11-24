//! Read-only access to cluster objects through kube's dynamic API: discovery, list, get, events,
//! and pod logs.
//!
//! These functions apply no policy. Callers decide limits, redaction, and who may see what (the
//! MCP server does this in `flint-mcp`; the UI will use these directly).

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use futures::{AsyncReadExt as _, StreamExt as _};
use k8s_openapi::api::core::v1::{Event, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::APIResourceList;
use kube::Client;
use kube::api::{Api, ApiResource, DynamicObject, ListParams, LogParams};
use kube::core::discovery::v2::APIGroupDiscovery;
use kube::discovery::verbs;

use crate::action_kinds::{self, ScaleSubresource};
use crate::api_error::{ApiError, Transport};
use crate::api_path::{self, PathError};
pub use crate::api_path::{is_dns1123_label, is_dns1123_subdomain, is_path_segment};
use crate::cluster::ClusterError;
use crate::paging;

/// One kind found by discovery, at its group's preferred version.
#[derive(Debug, Clone)]
pub struct KindInfo {
    pub resource: ApiResource,
    pub namespaced: bool,
    pub verbs: Vec<String>,
    /// The singular resource name (`deployment`). Empty when discovery did not give one.
    pub singular: String,
    /// The short names discovery suggests (`deploy`, `po`, `svc`), as kubectl accepts them.
    pub short_names: Vec<String>,
    /// Its `scale` subresource; see [`KindInfo::scale_paths`].
    pub scale: ScaleSubresource,
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

/// How long one detail read may take: the object, then the events about it. A server that
/// accepts the request and never answers would otherwise hold the read, and the object it
/// already fetched, for good (kube sets no read timeout).
pub const DETAIL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long each discovery request may take. A server that does not answer fails discovery
/// after this, instead of after kube's 30 s connection timeout.
pub const DISCOVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the whole of discovery may take (many groups on a slow server, without aggregated
/// discovery).
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
/// Group discovery requests in flight at once, without aggregated discovery.
const DISCOVERY_CONCURRENCY: usize = 8;

/// What [`discover_kinds`] found.
#[derive(Debug, Clone, Default)]
pub struct DiscoveredKinds {
    /// Every kind found, core kinds first, then by group and kind.
    pub kinds: Vec<KindInfo>,
    /// API groups whose discovery failed and that are missing from `kinds` (an aggregated API
    /// whose backend is down, often metrics.k8s.io). Sorted. Only servers without aggregated
    /// discovery report these; aggregated discovery lists such groups as stale instead.
    pub skipped_groups: Vec<String>,
    /// API groups whose preferred version aggregated discovery marks as stale: what it lists, if
    /// anything, may not be what it serves. Sorted.
    pub stale_groups: Vec<String>,
}

impl DiscoveredKinds {
    /// Every group whose kinds may be missing or out of date. Sorted.
    pub fn incomplete_groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = self
            .skipped_groups
            .iter()
            .chain(&self.stale_groups)
            .cloned()
            .collect();
        groups.sort();
        groups.dedup();
        groups
    }
}

/// Runs API discovery and returns every kind, CRDs included, at each group's preferred version.
/// Uses aggregated discovery when the server supports it (1.30+, beta from 1.26), which needs two
/// requests instead of one per group.
///
/// Falls back to one request per group only when the server does not support aggregated
/// discovery, never because the server could not be reached. Each request is bounded by
/// [`DISCOVERY_REQUEST_TIMEOUT`] and the whole by [`DISCOVERY_TIMEOUT`]; a timeout is a
/// [`ClusterError::Timeout`] (pass it through [`ClusterError::for_target`] to name the server).
///
/// Discovery is read here rather than through `kube::Discovery`, which drops the singular and
/// short names that [`resolve_kind`] matches (`deploy`, `po`, `svc`).
pub async fn discover_kinds(client: &Client) -> Result<DiscoveredKinds, ClusterError> {
    discover_kinds_within(client, DISCOVERY_REQUEST_TIMEOUT, DISCOVERY_TIMEOUT).await
}

async fn discover_kinds_within(
    client: &Client,
    per_request: Duration,
    overall: Duration,
) -> Result<DiscoveredKinds, ClusterError> {
    let discover = async {
        match discover_aggregated(client, per_request).await? {
            Some(found) => Ok(found),
            None => {
                tracing::debug!("no aggregated discovery, discovering each group");
                discover_per_group(client, per_request).await
            }
        }
    };
    let mut found = tokio::time::timeout(overall, discover)
        .await
        .unwrap_or_else(|_| Err(ClusterError::timeout(overall, None)))?;
    // Bounded on its own: a slow definition leaves its kind without Scale, never fails discovery.
    let scale = action_kinds::read_scale_paths(client, &mut found.kinds, per_request);
    if tokio::time::timeout(per_request, scale).await.is_err() {
        tracing::debug!("reading scale paths timed out; those kinds have no Scale");
    }
    // Put the core group first, then sort by group and kind.
    found.kinds.sort_by(|a, b| {
        (!a.group().is_empty(), a.group(), a.kind()).cmp(&(
            !b.group().is_empty(),
            b.group(),
            b.kind(),
        ))
    });
    Ok(found)
}

/// One discovery request, bounded by `limit`. The outer error is the timeout; the inner one is
/// kube's, kept so the caller can tell "not supported" from "failed".
async fn bounded<T>(
    limit: Duration,
    request: impl Future<Output = Result<T, kube::Error>>,
) -> Result<Result<T, kube::Error>, ClusterError> {
    tokio::time::timeout(limit, request)
        .await
        .map_err(|_| ClusterError::timeout(limit, None))
}

/// [`bounded`], with kube's error converted too.
async fn bounded_request<T>(
    limit: Duration,
    request: impl Future<Output = Result<T, kube::Error>>,
) -> Result<T, ClusterError> {
    bounded(limit, request).await?.map_err(ClusterError::kube)
}

/// Whether an aggregated discovery request failed because the server does not offer it: no such
/// path (404), the media type refused (406), or a body that is not the aggregated document.
fn aggregated_not_supported(err: &kube::Error) -> bool {
    match err {
        kube::Error::Api(status) => matches!(status.code, 404 | 406),
        kube::Error::SerdeError(_) => true,
        _ => false,
    }
}

/// Aggregated discovery: `/api` and `/apis`, each in one request. `None` when the server does
/// not support it.
async fn discover_aggregated(
    client: &Client,
    limit: Duration,
) -> Result<Option<DiscoveredKinds>, ClusterError> {
    let core = match bounded(limit, client.list_core_api_versions_aggregated()).await? {
        // A server without aggregated discovery answers with the plain document, which parses
        // as an empty list. The core group is always served, so empty means "not supported".
        Ok(core) if core.items.is_empty() => return Ok(None),
        Ok(core) => core,
        Err(e) if aggregated_not_supported(&e) => return Ok(None),
        Err(e) => return Err(ClusterError::kube(e)),
    };
    let apis = match bounded(limit, client.list_api_groups_aggregated()).await? {
        Ok(apis) => apis,
        Err(e) if aggregated_not_supported(&e) => return Ok(None),
        Err(e) => return Err(ClusterError::kube(e)),
    };
    let groups: Vec<APIGroupDiscovery> = core.items.into_iter().take(1).chain(apis.items).collect();
    let mut stale_groups: Vec<String> = groups
        .iter()
        .filter(|group| is_stale(group))
        .map(|group| {
            group
                .metadata
                .as_ref()
                .and_then(|m| m.name.clone())
                .unwrap_or_default()
        })
        .collect();
    stale_groups.sort();
    stale_groups.dedup();
    Ok(Some(DiscoveredKinds {
        kinds: groups.into_iter().flat_map(kinds_from_aggregated).collect(),
        skipped_groups: Vec::new(),
        stale_groups,
    }))
}

/// Whether the version of `group` Flint reads, its preferred one, is not reported as current. A
/// server that says nothing about freshness (the beta API) counts as current.
fn is_stale(group: &APIGroupDiscovery) -> bool {
    group
        .versions
        .first()
        .and_then(|version| version.freshness.as_deref())
        .is_some_and(|freshness| freshness != "Current")
}

impl From<PathError> for ClusterError {
    fn from(error: PathError) -> Self {
        ClusterError::Kube(error.to_string())
    }
}

/// Whether a discovered group, version and plural can be put into a request path as they are:
/// a DNS-1123 subdomain group (or the empty core group), a DNS-1123 label version, and a
/// lowercase plural. A server, or an extension API server, that advertises
/// anything else (`x/../../api/v1/secrets`) has that resource left out.
fn is_routable(group: &str, version: &str, plural: &str) -> bool {
    api_path::validate_group_version(group, version).is_ok()
        && api_path::validate_resource_plural(plural).is_ok()
}

/// The kinds of one group at its preferred version, which aggregated discovery lists first.
fn kinds_from_aggregated(group: APIGroupDiscovery) -> Vec<KindInfo> {
    let name = group.metadata.and_then(|m| m.name).unwrap_or_default();
    let Some(preferred) = group.versions.into_iter().next() else {
        return Vec::new();
    };
    let version = preferred.version.unwrap_or_default();
    preferred
        .resources
        .into_iter()
        .filter(|r| is_routable(&name, &version, r.resource.as_deref().unwrap_or_default()))
        .map(|r| KindInfo {
            resource: api_resource(
                &name,
                &version,
                r.response_kind.and_then(|k| k.kind).unwrap_or_default(),
                r.resource.unwrap_or_default(),
            ),
            namespaced: r.scope.as_deref() == Some("Namespaced"),
            verbs: r.verbs,
            singular: r.singular_resource.unwrap_or_default(),
            short_names: r.short_names,
            scale: if r
                .subresources
                .iter()
                .any(|s| s.subresource.as_deref() == Some("scale"))
            {
                ScaleSubresource::Unread
            } else {
                ScaleSubresource::Absent
            },
        })
        .collect()
}

/// Discovery for servers without aggregated discovery: one request per group at its preferred
/// version, [`DISCOVERY_CONCURRENCY`] at a time. A group whose request fails or times out is
/// left out and named in [`DiscoveredKinds::skipped_groups`] instead of failing the whole
/// cluster. The core group and the group list must succeed.
async fn discover_per_group(
    client: &Client,
    limit: Duration,
) -> Result<DiscoveredKinds, ClusterError> {
    let mut kinds = Vec::new();
    let core = bounded_request(limit, client.list_core_api_versions()).await?;
    if let Some(version) = core.versions.first() {
        api_path::validate_group_version("", version)?;
        let list = bounded_request(limit, client.list_core_api_resources(version)).await?;
        kinds.extend(kinds_from_resource_list("", version, list));
    }
    let groups = bounded_request(limit, client.list_api_groups())
        .await?
        .groups;
    let mut found: Vec<(String, Result<Vec<KindInfo>, GroupFailure>)> =
        futures::stream::iter(groups.into_iter().filter_map(|group| {
            let gv = group
                .preferred_version
                .or_else(|| group.versions.into_iter().next())?;
            Some((group.name, gv))
        }))
        .map(|(name, gv)| async move {
            if name.is_empty()
                || api_path::validate_api_version(&name, &gv.version, &gv.group_version).is_err()
            {
                return (
                    name,
                    Err(GroupFailure {
                        status: None,
                        category: "invalid group or version",
                    }),
                );
            }
            let result =
                match bounded(limit, client.list_api_group_resources(&gv.group_version)).await {
                    Ok(Ok(list)) => Ok(kinds_from_resource_list(&name, &gv.version, list)),
                    Ok(Err(e)) => Err(GroupFailure::of_request(&e)),
                    Err(e) => Err(GroupFailure::of(None, &e)),
                };
            (name, result)
        })
        .buffer_unordered(DISCOVERY_CONCURRENCY)
        .collect()
        .await;
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut skipped_groups = Vec::new();
    for (name, result) in found {
        match result {
            Ok(group_kinds) => kinds.extend(group_kinds),
            Err(failure) => {
                tracing::warn!(
                    group = loggable_group(&name),
                    status = failure.status,
                    failure = failure.category,
                    "discovery failed for an API group; leaving it out"
                );
                skipped_groups.push(name);
            }
        }
    }
    Ok(DiscoveredKinds {
        kinds,
        skipped_groups,
        stale_groups: Vec::new(),
    })
}

/// Why one group's discovery failed, for the log: an HTTP status and a category Flint words
/// itself, with no text from the response.
struct GroupFailure {
    status: Option<u16>,
    category: &'static str,
}

impl GroupFailure {
    fn of_request(err: &kube::Error) -> Self {
        let status = ApiError::from_kube(err).map(|api| api.code);
        Self::of(status, &ClusterError::kube_ref(err))
    }

    fn of(status: Option<u16>, err: &ClusterError) -> Self {
        let category = match err {
            ClusterError::Api(_) | ClusterError::Http { .. } => "http error",
            ClusterError::Timeout { .. } => "timeout",
            ClusterError::Transport { problem, .. } => match problem {
                Transport::Refused => "connection refused",
                Transport::Unresolved => "host not resolved",
                Transport::Tls(_) => "tls failure",
                Transport::Reset => "connection reset",
                Transport::Dropped => "connection dropped",
                Transport::Unreachable => "network unreachable",
                Transport::Proxy(_) => "proxy failure",
            },
            ClusterError::ExecPlugin { .. } | ClusterError::ExecPluginNotFound { .. } => {
                "auth plugin failure"
            }
            ClusterError::Kube(_) => "client error",
            ClusterError::UnknownContext(_) | ClusterError::Config { .. } => "invalid config",
        };
        Self { status, category }
    }
}

/// A group name as the log shows it: a valid name whole, anything else as a fixed placeholder.
fn loggable_group(name: &str) -> &str {
    if is_dns1123_subdomain(name) {
        name
    } else {
        "<invalid group name>"
    }
}

fn kinds_from_resource_list(group: &str, version: &str, list: APIResourceList) -> Vec<KindInfo> {
    let scaled: HashSet<String> = list
        .resources
        .iter()
        .filter_map(|r| r.name.strip_suffix("/scale").map(str::to_owned))
        .collect();
    list.resources
        .into_iter()
        // `pods/log` and the like are subresources, not kinds.
        .filter(|r| is_routable(group, version, &r.name))
        .map(|r| KindInfo {
            scale: if scaled.contains(&r.name) {
                ScaleSubresource::Unread
            } else {
                ScaleSubresource::Absent
            },
            resource: api_resource(group, version, r.kind, r.name),
            namespaced: r.namespaced,
            verbs: r.verbs,
            singular: r.singular_name,
            short_names: r.short_names.unwrap_or_default(),
        })
        .collect()
}

fn api_resource(group: &str, version: &str, kind: String, plural: String) -> ApiResource {
    let api_version = if group.is_empty() {
        version.to_owned()
    } else {
        format!("{group}/{version}")
    };
    ApiResource {
        group: group.to_owned(),
        version: version.to_owned(),
        api_version,
        kind,
        plural,
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ResolveKindError {
    #[error("no resource kind matches \"{0}\"")]
    NotFound(String),
    /// Several kinds match equally well. `candidates` are their ids (`plural.group`).
    #[error(
        "\"{query}\" matches several resource kinds. Pass one of these as kind: {}",
        candidates.join(", ")
    )]
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
}

/// Finds a kind by name the way `kubectl get` accepts it, ignoring case: the Kind (`Deployment`),
/// the plural (`deployments`), the singular (`deployment`), or a short name (`deploy`), each
/// optionally followed by `.group` (`deploy.apps`) or `.version.group` (`deployments.v1.apps`).
/// `api_version` (`apps/v1`, `v1`, or a bare group like `apps`) narrows the match the same way.
///
/// When several kinds match, a Kind, plural or singular match beats a short-name match. Among the
/// rest, the core group beats the built-in Kubernetes groups (`apps`, `*.k8s.io`), which beat
/// everything else (CRDs, aggregated APIs): `pods` is the core Pod, not metrics.k8s.io's
/// PodMetrics, as in kubectl. If more than one kind is left, the result is
/// [`ResolveKindError::Ambiguous`], listing them.
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
    let matches_group = |k: &KindInfo| match group_suffix {
        None => true,
        Some(g) => {
            k.group().eq_ignore_ascii_case(g)
                || g.split_once('.').map_or(
                    // `pods.v1`: a core kind with its version.
                    k.group().is_empty() && k.resource.version.eq_ignore_ascii_case(g),
                    |(v, g)| {
                        k.resource.version.eq_ignore_ascii_case(v)
                            && k.group().eq_ignore_ascii_case(g)
                    },
                )
        }
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
    let in_scope: Vec<&KindInfo> = kinds
        .iter()
        .filter(|k| !name.is_empty() && matches_group(k) && matches_version(k))
        .collect();
    let by_name: Vec<&KindInfo> = in_scope
        .iter()
        .copied()
        .filter(|k| {
            k.kind().eq_ignore_ascii_case(name)
                || k.plural().eq_ignore_ascii_case(name)
                || k.singular.eq_ignore_ascii_case(name)
        })
        .collect();
    let matched = if by_name.is_empty() {
        in_scope
            .into_iter()
            .filter(|k| k.short_names.iter().any(|s| s.eq_ignore_ascii_case(name)))
            .collect()
    } else {
        by_name
    };
    let Some(best) = matched.iter().map(|k| group_rank(k.group())).min() else {
        return Err(ResolveKindError::NotFound(query.to_owned()));
    };
    let mut winners = matched
        .into_iter()
        .filter(|k| group_rank(k.group()) == best);
    match (winners.next(), winners.next()) {
        (Some(only), None) => Ok(only),
        (Some(first), Some(second)) => {
            let mut candidates: Vec<String> = [first, second]
                .into_iter()
                .chain(winners)
                .map(|k| k.id().to_string())
                .collect();
            candidates.sort();
            Err(ResolveKindError::Ambiguous {
                query: query.to_owned(),
                candidates,
            })
        }
        (None, _) => Err(ResolveKindError::NotFound(query.to_owned())),
    }
}

/// How strongly a group is preferred when a name matches kinds in several groups: the core
/// group, then the groups Kubernetes itself serves (`apps`, `batch`, `*.k8s.io`), then the rest.
fn group_rank(group: &str) -> u8 {
    if group.is_empty() {
        0
    } else if !group.contains('.') || group.ends_with(".k8s.io") {
        1
    } else {
        2
    }
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

pub(crate) fn dynamic_api(
    client: &Client,
    kind: &KindInfo,
    namespace: Option<&str>,
    name: Option<&str>,
) -> Result<Api<DynamicObject>, PathError> {
    let namespace = namespace.filter(|_| kind.namespaced);
    api_path::validate_resource_path(&kind.resource, namespace, name)?;
    Ok(match namespace {
        Some(ns) => Api::namespaced_with(client.clone(), ns, &kind.resource),
        _ => Api::all_with(client.clone(), &kind.resource),
    })
}

/// Lists objects of a kind.
pub async fn list(
    client: &Client,
    kind: &KindInfo,
    query: &ListQuery,
) -> Result<ListPage, ClusterError> {
    let api = dynamic_api(client, kind, query.namespace.as_deref(), None)?;
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
    dynamic_api(client, kind, namespace, Some(name))?
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
    object_detail_within(client, kind, namespace, name, DETAIL_TIMEOUT).await
}

/// [`object_detail`] within `deadline` instead of [`DETAIL_TIMEOUT`].
pub async fn object_detail_within(
    client: &Client,
    kind: &KindInfo,
    namespace: Option<&str>,
    name: &str,
    deadline: Duration,
) -> Result<ObjectDetail, ClusterError> {
    object_detail_at(
        client,
        kind,
        namespace,
        name,
        &crate::cluster::Target::default(),
        deadline,
    )
    .await
}

/// [`object_detail_within`], with `events_error` naming `target`'s server, as safe to show
/// ([`ClusterError::summary`]). The object and its events share `deadline`: an object that does
/// not arrive in time is a timeout error; events that do not are an `events_error`, and the
/// object is still returned.
pub(crate) async fn object_detail_at(
    client: &Client,
    kind: &KindInfo,
    namespace: Option<&str>,
    name: &str,
    target: &crate::cluster::Target,
    deadline: Duration,
) -> Result<ObjectDetail, ClusterError> {
    let started = tokio::time::Instant::now();
    let (object, mut detail) = object_alone(client, kind, namespace, name, deadline).await?;
    let (events, events_truncated, events_error) = match object.metadata.uid.clone() {
        Some(uid) => {
            let selector = format!("involvedObject.uid={uid}");
            let query = EventQuery {
                // Events about cluster-scoped objects (nodes) are in some namespace
                // ("default" usually), so those are searched in all of them.
                namespace: object.metadata.namespace.clone(),
                involved: None,
                field_selector: Some(selector),
                fetch_limit: Some(DETAIL_EVENTS),
            };
            let left = deadline.saturating_sub(started.elapsed());
            let found = events_within(left, deadline, || events(client, &query)).await;
            match found {
                Ok((events, truncated)) => (events, truncated, None),
                // Many roles may read an object but not list events: still show the object.
                Err(e) => (
                    Vec::new(),
                    false,
                    Some(e.for_target(target).summary().to_string()),
                ),
            }
        }
        None => (Vec::new(), false, None),
    };
    detail.events = events;
    detail.events_truncated = events_truncated;
    detail.events_error = events_error;
    Ok(detail)
}

/// One object and nothing about it: exactly one GET, so a read that was allowed when it started
/// sends nothing later.
pub(crate) async fn object_only(
    client: &Client,
    kind: &KindInfo,
    namespace: Option<&str>,
    name: &str,
    deadline: Duration,
) -> Result<ObjectDetail, ClusterError> {
    object_alone(client, kind, namespace, name, deadline)
        .await
        .map(|(_, detail)| detail)
}

/// The object as fetched, and as a detail without events.
async fn object_alone(
    client: &Client,
    kind: &KindInfo,
    namespace: Option<&str>,
    name: &str,
    deadline: Duration,
) -> Result<(DynamicObject, ObjectDetail), ClusterError> {
    let object = tokio::time::timeout(deadline, get(client, kind, namespace, name))
        .await
        .map_err(|_| ClusterError::timeout(deadline, None))??;
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
    let detail = ObjectDetail {
        object: value,
        yaml,
        managed_fields,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    };
    Ok((object, detail))
}

/// The events of a detail read, from `request`, within `left` of its `deadline`. With no time
/// left, the request is never made: a zero timeout would still poll it once, which can queue
/// it, before giving up on it.
async fn events_within<F: Future<Output = Result<(Vec<Event>, bool), ClusterError>>>(
    left: Duration,
    deadline: Duration,
    request: impl FnOnce() -> F,
) -> Result<(Vec<Event>, bool), ClusterError> {
    if left.is_zero() {
        return Err(ClusterError::timeout(deadline, None));
    }
    tokio::time::timeout(left, request())
        .await
        .unwrap_or_else(|_| Err(ClusterError::timeout(deadline, None)))
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
    api_path::validate_names(query.namespace.as_deref(), None)?;
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
    api_path::validate_names(Some(&query.namespace), Some(&query.pod))?;
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

    #[tokio::test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the test uses an in-process transport"
    )]
    async fn api_path_rejects_hostile_object_reads_before_contacting_the_server() {
        use kube::client::Body;
        use std::convert::Infallible;
        use std::future::Ready;

        let service = tower::service_fn(
            |request: http::Request<Body>| -> Ready<Result<http::Response<Body>, Infallible>> {
                panic!("unexpected request to {}", request.uri());
            },
        );
        let client = Client::new(service, "default");
        let mut pods = kind("", "v1", "Pod", "pods");
        for name in ["a/b", "..", "x?y", "x#y", "%2F"] {
            let error = get(&client, &pods, Some("demo"), name).await.unwrap_err();
            assert_eq!(error.to_string(), PathError::Name.to_string());
        }
        let error = get(&client, &pods, Some("demo/pods"), "web")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), PathError::Namespace.to_string());
        pods.resource.plural = "pods/log".into();
        let error = get(&client, &pods, Some("demo"), "web").await.unwrap_err();
        assert_eq!(error.to_string(), PathError::Resource.to_string());
        pods.resource.plural = "pods".into();
        pods.resource.api_version = "v1/..".into();
        let error = get(&client, &pods, Some("demo"), "web").await.unwrap_err();
        assert_eq!(error.to_string(), PathError::GroupVersion.to_string());
    }

    // covers UP-23: a detail read whose object used the whole deadline asks for no events
    // (#258).
    #[tokio::test]
    async fn no_events_are_asked_for_once_the_deadline_is_spent() {
        let deadline = Duration::from_secs(30);
        let asked = std::sync::atomic::AtomicBool::new(false);
        let request = || {
            asked.store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok((Vec::new(), false)))
        };
        let error = events_within(Duration::ZERO, deadline, request)
            .await
            .unwrap_err();
        assert!(error.is_timeout(), "{error}");
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
        // With time left, it is asked.
        let request = || {
            asked.store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok((Vec::new(), false)))
        };
        assert!(
            events_within(Duration::from_secs(1), deadline, request)
                .await
                .is_ok()
        );
        assert!(asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    fn kind(group: &str, version: &str, kind: &str, plural: &str) -> KindInfo {
        KindInfo {
            resource: api_resource(group, version, kind.into(), plural.into()),
            namespaced: true,
            verbs: vec!["list".into()],
            singular: kind.to_ascii_lowercase(),
            short_names: Vec::new(),
            scale: Default::default(),
        }
    }

    fn with_short(mut kind: KindInfo, names: &[&str]) -> KindInfo {
        kind.short_names = names.iter().map(|n| (*n).to_owned()).collect();
        kind
    }

    fn kinds() -> Vec<KindInfo> {
        vec![
            with_short(kind("", "v1", "Pod", "pods"), &["po"]),
            with_short(kind("", "v1", "Event", "events"), &["ev"]),
            with_short(kind("", "v1", "Service", "services"), &["svc"]),
            with_short(kind("apps", "v1", "Deployment", "deployments"), &["deploy"]),
            with_short(kind("events.k8s.io", "v1", "Event", "events"), &["ev"]),
            kind("metrics.k8s.io", "v1beta1", "PodMetrics", "pods"),
            with_short(
                kind("networking.k8s.io", "v1", "Ingress", "ingresses"),
                &["ing"],
            ),
            kind(
                "networking.internal.knative.dev",
                "v1alpha1",
                "Ingress",
                "ingresses",
            ),
            with_short(kind("example.com", "v1", "Widget", "widgets"), &["wd"]),
            with_short(
                kind("other.example.org", "v1", "Gadget", "gadgets"),
                &["wd"],
            ),
        ]
    }

    fn id(kinds: &[KindInfo], query: &str, api_version: Option<&str>) -> String {
        match resolve_kind(kinds, query, api_version) {
            Ok(k) => k.id().to_string(),
            Err(e) => panic!("{query}: {e}"),
        }
    }

    // covers UP-10
    #[test]
    fn resolves_kind_plural_singular_and_short_names_ignoring_case() {
        let kinds = kinds();
        for q in ["Pod", "pod", "pods", "PODS", "po", "PO", " po "] {
            assert_eq!(id(&kinds, q, None), "pods", "{q}");
        }
        for q in [
            "deploy",
            "deployment",
            "deployments",
            "Deployment",
            "DEPLOY",
        ] {
            assert_eq!(id(&kinds, q, None), "deployments.apps", "{q}");
        }
        assert_eq!(id(&kinds, "svc", None), "services");
        assert_eq!(id(&kinds, "ingresses", None), "ingresses.networking.k8s.io");
        assert_eq!(id(&kinds, "wd.example.com", None), "widgets.example.com");
    }

    // covers UP-10
    #[test]
    fn group_and_version_suffixes_narrow_the_match() {
        let kinds = kinds();
        for q in [
            "deployments.apps",
            "Deployment.apps",
            "deploy.apps",
            "deployment.APPS",
            "deployments.v1.apps",
        ] {
            assert_eq!(id(&kinds, q, None), "deployments.apps", "{q}");
        }
        assert_eq!(id(&kinds, "pods.v1", None), "pods");
        assert_eq!(
            id(&kinds, "pods.metrics.k8s.io", None),
            "pods.metrics.k8s.io"
        );
        assert_eq!(
            id(&kinds, "ingress.networking.internal.knative.dev", None),
            "ingresses.networking.internal.knative.dev"
        );
        assert!(resolve_kind(&kinds, "deploy.batch", None).is_err());
        assert!(resolve_kind(&kinds, "deployments.v2.apps", None).is_err());
    }

    // covers UP-10
    #[test]
    fn core_then_builtin_groups_win_unless_a_group_is_given() {
        let kinds = kinds();
        // Core beats events.k8s.io and metrics.k8s.io, as in kubectl.
        assert_eq!(id(&kinds, "Event", None), "events");
        assert_eq!(id(&kinds, "ev", None), "events");
        assert_eq!(id(&kinds, "pods", None), "pods");
        // A built-in group beats a CRD with the same kind.
        assert_eq!(id(&kinds, "ingress", None), "ingresses.networking.k8s.io");
        assert_eq!(
            id(&kinds, "Event", Some("events.k8s.io/v1")),
            "events.events.k8s.io"
        );
        assert_eq!(
            id(&kinds, "events.events.k8s.io", None),
            "events.events.k8s.io"
        );
        assert_eq!(id(&kinds, "Event", Some("v1")), "events");
        assert_eq!(id(&kinds, "deploy", Some("apps")), "deployments.apps");
    }

    // covers UP-10
    #[test]
    fn a_name_beats_a_short_name() {
        let mut kinds = kinds();
        // A CRD whose short name is another kind's plural does not hijack it.
        kinds.push(with_short(
            kind("example.com", "v1", "Thing", "things"),
            &["pods"],
        ));
        assert_eq!(id(&kinds, "pods", None), "pods");
        assert_eq!(id(&kinds, "things", None), "things.example.com");
    }

    // covers UP-10
    #[test]
    fn ambiguous_names_list_the_candidates() {
        let err = resolve_kind(&kinds(), "wd", None).unwrap_err();
        assert_eq!(
            err,
            ResolveKindError::Ambiguous {
                query: "wd".into(),
                candidates: vec![
                    "gadgets.other.example.org".into(),
                    "widgets.example.com".into()
                ],
            }
        );
        assert_eq!(
            err.to_string(),
            "\"wd\" matches several resource kinds. Pass one of these as kind: \
             gadgets.other.example.org, widgets.example.com"
        );
    }

    // covers UP-10
    #[test]
    fn aggregated_discovery_keeps_singular_and_short_names() {
        let group: APIGroupDiscovery = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "apps"},
            "versions": [
                {"version": "v1", "resources": [{
                    "resource": "deployments",
                    "responseKind": {"group": "apps", "version": "v1", "kind": "Deployment"},
                    "scope": "Namespaced",
                    "singularResource": "deployment",
                    "shortNames": ["deploy"],
                    "verbs": ["get", "list", "watch"]
                }]},
                {"version": "v1beta2", "resources": [{"resource": "old"}]}
            ]
        }))
        .unwrap();
        let kinds = kinds_from_aggregated(group);
        assert_eq!(kinds.len(), 1, "only the preferred (first) version");
        let deploy = &kinds[0];
        assert_eq!(deploy.id().as_str(), "deployments.apps");
        assert_eq!(deploy.api_version(), "apps/v1");
        assert_eq!(deploy.kind(), "Deployment");
        assert!(deploy.namespaced && deploy.is_watchable());
        assert_eq!(deploy.singular, "deployment");
        assert_eq!(deploy.short_names, ["deploy"]);
    }

    // covers UP-10
    #[test]
    fn resource_lists_skip_subresources_and_keep_short_names() {
        let list: APIResourceList = serde_json::from_value(serde_json::json!({
            "groupVersion": "v1",
            "resources": [
                {"name": "pods", "singularName": "pod", "namespaced": true, "kind": "Pod",
                 "verbs": ["list"], "shortNames": ["po"]},
                {"name": "pods/log", "singularName": "", "namespaced": true, "kind": "Pod",
                 "verbs": ["get"]}
            ]
        }))
        .unwrap();
        let kinds = kinds_from_resource_list("", "v1", list);
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0].id().as_str(), "pods");
        assert_eq!(kinds[0].api_version(), "v1");
        assert_eq!(kinds[0].short_names, ["po"]);
        assert_eq!(kinds[0].singular, "pod");
        assert_eq!(kinds[0].scale, ScaleSubresource::Absent);
    }

    #[test]
    fn resource_lists_note_a_scale_subresource() {
        let list: APIResourceList = serde_json::from_value(serde_json::json!({
            "groupVersion": "example.dev/v1",
            "resources": [
                {"name": "widgets", "singularName": "widget", "namespaced": true,
                 "kind": "Widget", "verbs": ["list"]},
                {"name": "widgets/scale", "singularName": "", "namespaced": true,
                 "kind": "Scale", "verbs": ["get", "patch"]}
            ]
        }))
        .unwrap();
        let kinds = kinds_from_resource_list("example.dev", "v1", list);
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0].scale, ScaleSubresource::Unread);
    }

    // covers UP-10
    #[test]
    fn aggregated_discovery_leaves_out_resources_that_are_not_one_path_segment() {
        let group = |name: &str, version: &str, plural: &str| -> APIGroupDiscovery {
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": name},
                "versions": [{"version": version, "resources": [
                    {"resource": plural, "responseKind": {"kind": "Widget"}, "verbs": ["get"]},
                    {"resource": "widgets", "responseKind": {"kind": "Widget"}, "verbs": ["get"]}
                ]}]
            }))
            .unwrap()
        };
        let plurals = |g| -> Vec<String> {
            kinds_from_aggregated(g)
                .iter()
                .map(|k| k.plural().to_owned())
                .collect()
        };
        assert_eq!(
            plurals(group("example.com", "v1", "x/../../api/v1/secrets")),
            ["widgets"]
        );
        for plural in ["..", "a%2Fb", "a?b", ""] {
            assert_eq!(plurals(group("example.com", "v1", plural)), ["widgets"]);
        }
        assert!(plurals(group("example.com/../api", "v1", "gadgets")).is_empty());
        assert!(plurals(group("example.com", "v1/../../api/v1", "gadgets")).is_empty());
        assert_eq!(plurals(group("", "v1", "gadgets")), ["gadgets", "widgets"]);
    }

    // covers UP-10
    #[test]
    fn resource_lists_leave_out_resources_that_are_not_one_path_segment() {
        let list = || -> APIResourceList {
            serde_json::from_value(serde_json::json!({
                "groupVersion": "example.com/v1",
                "resources": [
                    {"name": "x/../../api/v1/secrets", "singularName": "", "namespaced": true,
                     "kind": "Secret", "verbs": ["get"]},
                    {"name": "..", "singularName": "", "namespaced": true, "kind": "X",
                     "verbs": ["get"]},
                    {"name": "widgets", "singularName": "widget", "namespaced": true,
                     "kind": "Widget", "verbs": ["get"]}
                ]
            }))
            .unwrap()
        };
        let plurals = |group: &str, version: &str| -> Vec<String> {
            kinds_from_resource_list(group, version, list())
                .iter()
                .map(|k| k.plural().to_owned())
                .collect()
        };
        assert_eq!(plurals("example.com", "v1"), ["widgets"]);
        assert!(plurals("example.com/../api", "v1").is_empty());
        assert!(plurals("example.com", "v1/../..").is_empty());
        assert!(plurals("Example.com", "v1").is_empty());
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
            resolve_kind(&kinds(), "Sprocket", None).unwrap_err(),
            ResolveKindError::NotFound("Sprocket".into())
        );
        assert!(resolve_kind(&kinds(), "", None).is_err());
        assert!(resolve_kind(&kinds(), ".apps", None).is_err());
        assert!(resolve_kind(&kinds(), "Pod", Some("apps/v1")).is_err());
    }

    /// A fake API server: answers each path in `routes` with its status and JSON body, anything
    /// else with 404, and counts connections. Plain HTTP on 127.0.0.1, one request per
    /// connection.
    async fn fake_api(
        routes: Vec<(&'static str, u16, serde_json::Value)>,
    ) -> (Client, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connections.clone();
        let routes = std::sync::Arc::new(routes);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&request);
                    let path = text
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/")
                        .to_owned();
                    let (code, body) = routes
                        .iter()
                        .find(|(p, _, _)| *p == path)
                        .map(|(_, code, body)| (*code, body.to_string()))
                        .unwrap_or_else(|| {
                            (
                                404,
                                serde_json::json!({"kind": "Status", "code": 404}).to_string(),
                            )
                        });
                    // Code 0: a server that never answers this path.
                    if code == 0 {
                        std::future::pending::<()>().await;
                    }
                    let response = format!(
                        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        let config = kube::Config::new(url.parse().unwrap());
        (Client::try_from(config).unwrap(), connections)
    }

    fn resource(name: &str, kind: &str, short: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "name": name, "singularName": kind.to_ascii_lowercase(), "namespaced": true,
            "kind": kind, "verbs": ["get", "list", "watch"], "shortNames": short,
        })
    }

    // covers UP-10, UP-22
    #[tokio::test]
    async fn without_aggregated_discovery_each_group_is_read_and_failures_are_skipped() {
        use serde_json::json;
        // The plain `/api` document parses as an empty aggregated list: "not supported".
        let (client, _) = fake_api(vec![
            ("/api", 200, json!({"kind": "APIVersions", "versions": ["v1"]})),
            (
                "/api/v1",
                200,
                json!({"groupVersion": "v1", "resources": [
                    resource("pods", "Pod", &["po"]),
                    resource("pods/log", "Pod", &[]),
                ]}),
            ),
            (
                "/apis",
                200,
                json!({"kind": "APIGroupList", "groups": [
                    {"name": "apps", "versions": [{"groupVersion": "apps/v1", "version": "v1"}],
                     "preferredVersion": {"groupVersion": "apps/v1", "version": "v1"}},
                    {"name": "metrics.k8s.io",
                     "versions": [{"groupVersion": "metrics.k8s.io/v1beta1", "version": "v1beta1"}]},
                    {"name": "broken.example.com",
                     "versions": [{"groupVersion": "broken.example.com/v1", "version": "v1"}]},
                ]}),
            ),
            (
                "/apis/apps/v1",
                200,
                json!({"groupVersion": "apps/v1", "resources": [
                    resource("deployments", "Deployment", &["deploy"]),
                ]}),
            ),
            (
                "/apis/metrics.k8s.io/v1beta1",
                503,
                json!({"kind": "Status", "code": 503, "message": "service unavailable"}),
            ),
            (
                "/apis/broken.example.com/v1",
                500,
                json!({"kind": "Status", "code": 500, "message": "internal error"}),
            ),
        ])
        .await;
        // kube retries a 503 until the per-request limit; a 500 fails at once. Both skip.
        let found =
            discover_kinds_within(&client, Duration::from_millis(500), Duration::from_secs(5))
                .await
                .unwrap();
        let ids: Vec<String> = found.kinds.iter().map(|k| k.id().to_string()).collect();
        assert_eq!(ids, ["pods", "deployments.apps"]);
        assert_eq!(
            found.skipped_groups,
            ["broken.example.com", "metrics.k8s.io"]
        );
        assert_eq!(id(&found.kinds, "deploy", None), "deployments.apps");
        assert_eq!(id(&found.kinds, "po", None), "pods");
    }

    // covers UP-53: a custom resource has Scale only with replica paths read from its definition.
    #[tokio::test]
    async fn custom_scale_paths_come_from_the_definition() {
        use serde_json::json;
        let scaled = |plural: &str, kind: &str| {
            json!({"resource": plural, "responseKind": {"kind": kind}, "scope": "Namespaced",
                   "verbs": ["get", "list"],
                   "subresources": [{"subresource": "scale", "verbs": ["get", "patch"]}]})
        };
        let crd = |plural: &str, path: &str| {
            json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
                   "metadata": {"name": format!("{plural}.example.dev")},
                   "spec": {"group": "example.dev", "names": {"plural": plural},
                            "versions": [{"name": "v1", "served": true, "subresources": {"scale":
                                {"specReplicasPath": path, "statusReplicasPath": ".status.n"}}}]}})
        };
        let (client, _) = fake_api(vec![
            ("/api", 200, json!({"items": [{"metadata": {"name": ""}, "versions": [
                {"version": "v1", "resources": [scaled("replicationcontrollers", "ReplicationController")]}]}]})),
            ("/apis", 200, json!({"items": [{"metadata": {"name": "example.dev"}, "versions": [
                {"version": "v1", "resources": [
                    scaled("widgets", "Widget"), scaled("gizmos", "Gizmo"),
                    scaled("sprockets", "Sprocket"),
                    {"resource": "plain", "responseKind": {"kind": "Plain"}, "verbs": ["list"]},
                ]}]}]})),
            (
                "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/widgets.example.dev",
                200,
                crd("widgets", ".spec.size"),
            ),
            (
                "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/gizmos.example.dev",
                200,
                crd("gizmos", ".spec.sizes[0]"),
            ),
            // sprockets.example.dev: no definition can be read.
        ])
        .await;
        let found = discover_kinds(&client).await.unwrap();
        let scale = |plural: &str| {
            let kind = found.kinds.iter().find(|k| k.plural() == plural).unwrap();
            (kind.scale.clone(), kind.scale_paths())
        };
        assert_eq!(
            scale("widgets").1,
            action_kinds::ScalePaths::from_crd(".spec.size", ".status.n")
        );
        assert_eq!(scale("gizmos"), (ScaleSubresource::Unread, None));
        assert_eq!(scale("sprockets"), (ScaleSubresource::Unread, None));
        assert_eq!(scale("plain"), (ScaleSubresource::Absent, None));
        assert_eq!(
            scale("replicationcontrollers").1,
            Some(action_kinds::ScalePaths::built_in())
        );
    }

    // covers UP-53: a definition that never answers costs only its own kind's Scale: what the
    // others answered before the deadline is kept.
    #[tokio::test]
    async fn a_stalled_definition_keeps_the_paths_already_read() {
        use serde_json::json;
        let crd = json!({"spec": {"group": "example.dev", "names": {"plural": "widgets"},
            "versions": [{"name": "v1", "served": true, "subresources": {"scale":
                {"specReplicasPath": ".spec.size", "statusReplicasPath": ".status.n"}}}]}});
        let (client, _) = fake_api(vec![
            (
                "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/widgets.example.dev",
                200,
                crd,
            ),
            (
                "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/gizmos.example.dev",
                0,
                json!({}),
            ),
        ])
        .await;
        let kind = |plural: &str| KindInfo {
            scale: ScaleSubresource::Unread,
            ..kind("example.dev", "v1", "Widget", plural)
        };
        let mut kinds = vec![kind("widgets"), kind("gizmos")];
        let read = action_kinds::read_scale_paths(&client, &mut kinds, Duration::from_secs(60));
        assert!(
            tokio::time::timeout(Duration::from_millis(500), read)
                .await
                .is_err()
        );
        assert!(kinds[0].scale_paths().is_some());
        assert_eq!(kinds[1].scale, ScaleSubresource::Unread);
    }

    // covers UP-10
    #[tokio::test]
    async fn aggregated_discovery_is_used_when_served() {
        use serde_json::json;
        let group = |name: &str, kind: &str, plural: &str| {
            json!({"metadata": {"name": name}, "versions": [{"version": "v1", "resources": [{
                "resource": plural, "responseKind": {"kind": kind}, "scope": "Namespaced",
                "verbs": ["list"], "shortNames": ["x"],
            }]}]})
        };
        let (client, _) = fake_api(vec![
            ("/api", 200, json!({"items": [group("", "Pod", "pods")]})),
            (
                "/apis",
                200,
                json!({"items": [group("apps", "Deployment", "deployments")]}),
            ),
        ])
        .await;
        let found = discover_kinds(&client).await.unwrap();
        let ids: Vec<String> = found.kinds.iter().map(|k| k.id().to_string()).collect();
        assert_eq!(ids, ["pods", "deployments.apps"]);
        assert!(found.skipped_groups.is_empty());
    }

    // covers UP-14: aggregated discovery marks a group whose backend is down as stale, often with
    // no resources: it is reported, whatever else is current.
    #[tokio::test]
    async fn stale_groups_are_reported() {
        use serde_json::json;
        let group = |name: &str, freshness: Option<&str>, resources: serde_json::Value| {
            let mut version = json!({"version": "v1", "resources": resources});
            if let Some(freshness) = freshness {
                version["freshness"] = json!(freshness);
            }
            json!({"metadata": {"name": name}, "versions": [version]})
        };
        let pods = json!([{"resource": "pods", "responseKind": {"kind": "Pod"},
                           "scope": "Namespaced", "verbs": ["list"]}]);
        let (client, _) = fake_api(vec![
            (
                "/api",
                200,
                json!({"items": [group("", Some("Current"), pods.clone())]}),
            ),
            (
                "/apis",
                200,
                json!({"items": [
                    group("metrics.k8s.io", Some("Stale"), json!([])),
                    group("a.example.com", None, json!([])),
                    group("b.example.com", Some("Current"), json!([])),
                    // Only the preferred version, listed first, is read.
                    json!({"metadata": {"name": "c.example.com"}, "versions": [
                        {"version": "v1", "freshness": "Current", "resources": []},
                        {"version": "v1beta1", "freshness": "Stale", "resources": []},
                    ]}),
                ]}),
            ),
        ])
        .await;
        let found = discover_kinds(&client).await.unwrap();
        assert_eq!(found.stale_groups, ["metrics.k8s.io"]);
        assert!(found.skipped_groups.is_empty());
        assert_eq!(found.incomplete_groups(), ["metrics.k8s.io"]);
    }

    // covers UP-04
    #[tokio::test]
    async fn an_unreachable_server_fails_fast_without_the_fallback() {
        // Accepts connections and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connections.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                held.push(socket);
            }
        });
        let client = Client::try_from(kube::Config::new(url.parse().unwrap())).unwrap();
        let limit = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let err = discover_kinds_within(&client, limit, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.is_timeout(), "{err}");
        assert_eq!(err.to_string(), "timed out after 300ms connecting");
        assert!(started.elapsed() < Duration::from_secs(2));
        // One request, no per-group fallback after it.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // covers UP-10
    #[test]
    fn only_missing_aggregated_discovery_means_not_supported() {
        let status = |code| {
            kube::Error::Api(Box::new(kube::core::Status {
                code,
                ..Default::default()
            }))
        };
        assert!(aggregated_not_supported(&status(404)));
        assert!(aggregated_not_supported(&status(406)));
        assert!(!aggregated_not_supported(&status(403)));
        assert!(!aggregated_not_supported(&status(503)));
        let parse = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(aggregated_not_supported(&kube::Error::SerdeError(parse)));
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "deadline has elapsed");
        assert!(!aggregated_not_supported(&kube::Error::Service(Box::new(
            io
        ))));
    }
}
