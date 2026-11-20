//! The MCP tools. Each call re-reads `settings.toml` (so the app's Expose to AI toggle applies at
//! once), recomputes the kubeconfig file list from it, reloads kubeconfig when a file changed,
//! checks the cluster against the opt-in list, and only then touches the network.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flint_core::cache::{SlotMap, slot};
use flint_core::cluster::{self, ClusterError, Target};
use flint_core::kubeconfig::{self, ContextInfo, FileStamps, LoadedKubeconfig};
use flint_core::read::{self, KindInfo, ResolveKindError};
use flint_core::settings;
use flint_core::table::{self, TablePage};
use kube::Client;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::policy;

/// How long `list_clusters` waits for each cluster's `/version`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a tool call may wait for the connection (exec auth plugins can be slow).
const TOOL_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long discovery results are reused.
const DISCOVERY_TTL: Duration = Duration::from_secs(120);
/// Events fetched from the server before sorting by time.
const EVENTS_FETCH: u32 = 1000;
/// Bytes requested from the server per log fetch, so a few huge lines are not all pulled into
/// memory. Larger than the output cap so the newest lines usually survive the cut.
const LOG_FETCH_BYTES: i64 = 4 * policy::LOG_BYTES as i64;

#[derive(Clone)]
pub struct FlintMcp {
    inner: Arc<Inner>,
    tool_router: ToolRouter<Self>,
}

struct Inner {
    config_dir: PathBuf,
    /// Kubeconfig files from `KUBECONFIG` or `~/.kube/config`; the settings' extra paths are
    /// appended on each call.
    kubeconfig_base: Vec<PathBuf>,
    home: Option<PathBuf>,
    /// The last parsed kubeconfig and the file stamps it was parsed from.
    kubeconfig: Mutex<Option<(FileStamps, Arc<LoadedKubeconfig>)>>,
    clients: SlotMap<Client>,
    discovery: SlotMap<Kinds>,
}

/// Discovery results for one cluster.
type Kinds = Arc<Vec<KindInfo>>;

/// A failed call, shown to the agent as a tool error.
///
/// There is deliberately no `From<ClusterError>`: a cluster error must name its target first,
/// through [`Conn::fail`], so it says which server timed out or which plugin is missing.
#[derive(Debug)]
struct ToolError(String);

macro_rules! tool_error_from {
    ($($t:ty),* $(,)?) => {$(
        impl From<$t> for ToolError {
            fn from(e: $t) -> Self {
                ToolError(e.to_string())
            }
        }
    )*};
}

tool_error_from!(
    serde_json::Error,
    serde_saphyr::ser::Error,
    tokio::task::JoinError,
    policy::PolicyError,
);

/// A connection to an exposed cluster, with what its errors may name.
struct Conn {
    client: Client,
    fingerprint: u64,
    target: Target,
}

impl Conn {
    /// A cluster error as the agent reads it: naming the server and the exec plugin, with the
    /// server's own text cut to one short line ([`ClusterError::for_agent`]).
    fn fail(&self, e: ClusterError) -> ToolError {
        ToolError(e.for_target(&self.target).for_agent())
    }
}

type ToolResult = Result<CallToolResult, rmcp::ErrorData>;

fn finish(result: Result<String, ToolError>) -> ToolResult {
    Ok(match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        // A server's error text can quote what a workload wrote (webhooks, aggregated APIs).
        Err(ToolError(mut msg)) => {
            policy::redact_text(&mut msg);
            CallToolResult::error(vec![ContentBlock::text(msg)])
        }
    })
}

/// One row of list_resource_kinds. `shortNames` only when the kind has some.
fn kind_row(k: &KindInfo) -> Value {
    let mut row = json!({
        "kind": k.kind(),
        "apiVersion": k.api_version(),
        "plural": k.plural(),
        "namespaced": k.namespaced,
        "listable": k.is_listable(),
    });
    if !k.short_names.is_empty() {
        row["shortNames"] = json!(k.short_names);
    }
    row
}

/// What the agent reads when a kind does not resolve: how to find the kinds, or which of the
/// candidates to name.
fn kind_error(e: ResolveKindError) -> ToolError {
    match e {
        ResolveKindError::NotFound(_) => {
            ToolError(format!("{e}. Call list_resource_kinds for the kinds."))
        }
        ResolveKindError::Ambiguous { .. } => ToolError(format!("{e}.")),
    }
}

/// What the API server answers for an object that does not exist (`NotFound: pods "x" not
/// found`), where `resource` is the plural, or the kind as the caller named it. A name or
/// namespace that is not a single valid path segment gets this answer without any request, so
/// it can never reach another path, and the caller learns nothing it would not from a missing
/// object.
fn not_found(resource: &str, name: &str) -> ToolError {
    ToolError(format!("NotFound: {resource} {name:?} not found"))
}

/// Refuses a namespace that is not a DNS-1123 label, as [`not_found`] for the namespace.
fn check_namespace(namespace: Option<&str>) -> Result<(), ToolError> {
    match namespace {
        Some(ns) if !policy::is_label_name(ns) => Err(not_found("namespaces", ns)),
        _ => Ok(()),
    }
}

/// Whether `kind` is core v1 Secret, whose values are never shown.
fn is_secret_kind(kind: &KindInfo) -> bool {
    kind.group().is_empty() && kind.kind() == "Secret"
}

fn to_json<T: Serialize>(value: &T) -> Result<String, ToolError> {
    Ok(serde_json::to_string_pretty(value)?)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClusterParams {
    /// Cluster name, as returned by list_clusters.
    pub cluster: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListResourcesParams {
    /// Cluster name, as returned by list_clusters.
    pub cluster: String,
    /// The kind as kubectl accepts it, ignoring case: Kind, plural, singular, or short name
    /// ("Deployment", "deployments", "deployment", "deploy", "po", "svc", or a CRD's), optionally
    /// with ".group" ("deploy.apps") when it exists in several groups.
    pub kind: String,
    /// Optional apiVersion or group ("apps/v1", "v1", "apps") when a kind exists in several groups.
    pub api_version: Option<String>,
    /// Namespace. Omit to list across all namespaces.
    pub namespace: Option<String>,
    /// Label selector, as in kubectl -l (for example "app=web,tier!=db").
    pub label_selector: Option<String>,
    /// Maximum objects to return (default 100, maximum 200).
    pub limit: Option<u32>,
    /// The `continue` token from a previous list_resources result, to get the next page. Use the
    /// same kind, namespace, and label selector as that call.
    #[serde(rename = "continue")]
    pub continue_token: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetResourceParams {
    /// Cluster name, as returned by list_clusters.
    pub cluster: String,
    /// The kind as kubectl accepts it, ignoring case: Kind, plural, singular, or short name
    /// ("Deployment", "deployments", "deployment", "deploy", "po", "svc", or a CRD's), optionally
    /// with ".group" ("deploy.apps") when it exists in several groups.
    pub kind: String,
    /// Optional apiVersion or group when a kind exists in several groups.
    pub api_version: Option<String>,
    /// Namespace. Required for namespaced kinds.
    pub namespace: Option<String>,
    /// Object name.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetEventsParams {
    /// Cluster name, as returned by list_clusters.
    pub cluster: String,
    /// Namespace. Omit for all namespaces.
    pub namespace: Option<String>,
    /// Only events about the object with this name.
    pub object_name: Option<String>,
    /// Only events about objects of this kind (for example "Pod").
    pub object_kind: Option<String>,
    /// Maximum events to return, newest first (default 50, maximum 200).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetPodLogsParams {
    /// Cluster name, as returned by list_clusters.
    pub cluster: String,
    pub namespace: String,
    pub pod: String,
    /// Container name. Required when the pod has more than one container.
    pub container: Option<String>,
    /// Number of most recent lines (default 100, maximum 500).
    pub tail_lines: Option<u32>,
    /// Logs of the previous, terminated container instance.
    pub previous: Option<bool>,
    /// Only lines newer than this many seconds. 0 or omitted means no time limit.
    pub since_seconds: Option<u32>,
}

#[tool_router]
impl FlintMcp {
    /// `kubeconfig_base` is the `KUBECONFIG` list (or `~/.kube/config`); the settings'
    /// `kubeconfig_paths`, expanded with `home`, are appended on every call.
    pub fn new(config_dir: PathBuf, kubeconfig_base: Vec<PathBuf>, home: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                config_dir,
                kubeconfig_base,
                home,
                kubeconfig: Mutex::default(),
                clients: Mutex::default(),
                discovery: Mutex::default(),
            }),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List the Kubernetes clusters the user exposed to AI agents in Flint, with \
        server URL and whether a connection works.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_clusters(&self) -> ToolResult {
        finish(self.do_list_clusters().await)
    }

    #[tool(
        description = "List the resource kinds a cluster serves, CRDs included, from API discovery.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_resource_kinds(&self, Parameters(p): Parameters<ClusterParams>) -> ToolResult {
        finish(self.do_list_kinds(p).await)
    }

    #[tool(
        description = "List objects of one kind as the rows `kubectl get -o wide` prints: \
        the server's columns (ready counts, status, restarts, replicas, CRD printer columns) plus \
        name, namespace and age. Use get_resource for a full object. When truncated is true, pass the returned \
        continue token to get the next page; if continue is null, narrow by namespace or label.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_resources(&self, Parameters(p): Parameters<ListResourcesParams>) -> ToolResult {
        finish(self.do_list_resources(p).await)
    }

    #[tool(
        description = "Get one object as YAML. managedFields are removed and Secret values redacted. Credentials in messages, annotations and container args and commands are replaced with [REDACTED:<kind>] markers; a first line `# redacted=N` says how many.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_resource(&self, Parameters(p): Parameters<GetResourceParams>) -> ToolResult {
        finish(self.do_get_resource(p).await)
    }

    #[tool(
        description = "Get recent events, newest first, for a namespace or for one object.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_events(&self, Parameters(p): Parameters<GetEventsParams>) -> ToolResult {
        finish(self.do_get_events(p).await)
    }

    #[tool(
        description = "Get the last lines of a pod container's logs.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_pod_logs(&self, Parameters(p): Parameters<GetPodLogsParams>) -> ToolResult {
        finish(self.do_get_pod_logs(p).await)
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "flint",
    instructions = "Read-only access to Kubernetes clusters the user has opted in to in Flint (Expose to AI). Start with list_clusters. Nothing can be created, changed, or deleted. Secret values are always redacted. Values that look like credentials in pod logs, event messages, list cells, object messages, annotations and container args and commands are replaced with [REDACTED:<kind>] markers, and each result's `redacted` count says how many. Large outputs are cut short with truncated=true."
)]
impl ServerHandler for FlintMcp {}

/// Settings and kubeconfig as of this call.
struct CallContext {
    settings: flint_core::Settings,
    kubeconfig: Arc<LoadedKubeconfig>,
}

impl FlintMcp {
    /// Re-reads settings and recomputes the kubeconfig file list from them. Kubeconfig is parsed
    /// again only when a file's modification time or size changed. If settings cannot be read,
    /// the call fails rather than guessing which clusters are exposed.
    async fn context(&self) -> Result<CallContext, ToolError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            // The full TOML error quotes lines of settings.toml; give the agent one line.
            let settings = settings::load_settings(&inner.config_dir).map_err(|e| {
                ToolError(format!(
                    "Flint's settings.toml is invalid ({}), so no cluster is exposed. Ask the \
                     user to fix it; the Flint app shows the details.",
                    flint_core::redact::safe_error_summary(&e.to_string())
                ))
            })?;
            let paths = kubeconfig::paths_for_settings(
                &inner.kubeconfig_base,
                &settings,
                inner.home.as_deref(),
            );
            let kubeconfig = inner.load_kubeconfig(&paths);
            Ok(CallContext {
                settings,
                kubeconfig,
            })
        })
        .await?
    }

    /// The client for an exposed cluster. Clients are cached per context and rebuilt when the
    /// context's kubeconfig entries change.
    async fn client(&self, ctx: &CallContext, name: &str) -> Result<Conn, ToolError> {
        let info = policy::exposed_cluster(&ctx.settings, &ctx.kubeconfig, name)?;
        let fingerprint = cluster::context_fingerprint(&ctx.kubeconfig.merged, &info.name)
            .ok_or_else(|| ToolError(format!("cluster \"{name}\" not found")))?;
        // Derived from the same kubeconfig entries as the fingerprint, so it always matches the
        // cached client.
        let target = Target::of(&ctx.kubeconfig.merged, &info.name);
        let slot = slot(&self.inner.clients, name, fingerprint, None);
        let client = slot
            .cell
            .get_or_try_init(|| async {
                tokio::time::timeout(
                    TOOL_CONNECT_TIMEOUT,
                    cluster::connect(&ctx.kubeconfig.merged, &info.name),
                )
                .await
                .map_err(|_| ClusterError::timeout(TOOL_CONNECT_TIMEOUT, None))?
            })
            .await
            .map_err(|e| e.for_target(&target).for_agent())
            .map_err(ToolError)?
            .clone();
        Ok(Conn {
            client,
            fingerprint,
            target,
        })
    }

    async fn kinds(&self, cluster: &str, conn: &Conn) -> Result<Kinds, ToolError> {
        let slot = slot(
            &self.inner.discovery,
            cluster,
            conn.fingerprint,
            Some(DISCOVERY_TTL),
        );
        // Groups skipped this time are missing until the cache expires; the agent can retry.
        let kinds = slot
            .cell
            .get_or_try_init(|| async {
                read::discover_kinds(&conn.client)
                    .await
                    .map(|found| Arc::new(found.kinds))
            })
            .await
            .map_err(|e| conn.fail(e))?;
        Ok(kinds.clone())
    }

    /// The connection and the resolved kind. `check` runs once the cluster is known to be
    /// exposed and before discovery, so a call it refuses sends no request at all.
    async fn resolve(
        &self,
        cluster: &str,
        kind: &str,
        api_version: Option<&str>,
        check: impl FnOnce() -> Result<(), ToolError>,
    ) -> Result<(Conn, KindInfo), ToolError> {
        let ctx = self.context().await?;
        let conn = self.client(&ctx, cluster).await?;
        check()?;
        let kinds = self.kinds(cluster, &conn).await?;
        let kind = read::resolve_kind(&kinds, kind, api_version)
            .map_err(kind_error)?
            .clone();
        Ok((conn, kind))
    }

    async fn do_list_clusters(&self) -> Result<String, ToolError> {
        let ctx = self.context().await?;
        let exposed: Vec<ContextInfo> = policy::exposed_clusters(&ctx.settings, &ctx.kubeconfig)
            .into_iter()
            .cloned()
            .collect();
        let changed = policy::changed_exposures(&ctx.settings, &ctx.kubeconfig);
        let kubeconfig_errors: Vec<Value> = ctx
            .kubeconfig
            .errors
            .iter()
            .map(|(path, error)| {
                json!({
                    "file": path.display().to_string(),
                    "error": flint_core::redact::safe_error_summary(error),
                })
            })
            .collect();
        let mut rows = Vec::new();
        let results = self.probe_all(ctx, &exposed).await;
        for info in &exposed {
            let (reachable, version, error) = match results.get(&info.name) {
                Some(Ok(v)) => (true, Some(v.clone()), None),
                Some(Err(e)) => (false, None, Some(e.clone())),
                None => (false, None, None),
            };
            rows.push(json!({
                "name": info.name,
                "server": info.server.as_deref().map(flint_core::redact::server_url),
                "default_namespace": info.namespace,
                "reachable": reachable,
                "server_version": version,
                "error": error,
            }));
        }
        let mut out = json!({ "clusters": rows });
        if let Some(hint) = policy::list_clusters_hint(exposed.len(), changed) {
            out["hint"] = json!(hint);
        }
        if !kubeconfig_errors.is_empty() {
            out["kubeconfig_errors"] = json!(kubeconfig_errors);
        }
        to_json(&out)
    }

    /// Connects to each cluster and calls `/version`, all concurrently.
    async fn probe_all(
        &self,
        ctx: CallContext,
        exposed: &[ContextInfo],
    ) -> BTreeMap<String, Result<String, String>> {
        let ctx = Arc::new(ctx);
        let mut set = tokio::task::JoinSet::new();
        for info in exposed {
            let name = info.name.clone();
            let target = Target::of(&ctx.kubeconfig.merged, &info.name);
            let this = self.clone();
            let ctx = ctx.clone();
            set.spawn(async move {
                let probe = async {
                    let conn = this.client(&ctx, &name).await?;
                    cluster::server_version(&conn.client)
                        .await
                        .map(|v| v.git_version)
                        .map_err(|e| conn.fail(e))
                };
                let result = tokio::time::timeout(PROBE_TIMEOUT, probe)
                    .await
                    .unwrap_or_else(|_| {
                        Err(ToolError(
                            ClusterError::timeout(PROBE_TIMEOUT, None)
                                .for_target(&target)
                                .to_string(),
                        ))
                    })
                    .map_err(|e| e.0);
                (name, result)
            });
        }
        let mut out = BTreeMap::new();
        while let Some(joined) = set.join_next().await {
            if let Ok((name, result)) = joined {
                out.insert(name, result);
            }
        }
        out
    }

    async fn do_list_kinds(&self, p: ClusterParams) -> Result<String, ToolError> {
        let ctx = self.context().await?;
        let conn = self.client(&ctx, &p.cluster).await?;
        let kinds = self.kinds(&p.cluster, &conn).await?;
        let rows: Vec<Value> = kinds.iter().map(kind_row).collect();
        to_json(&json!({ "cluster": p.cluster, "count": rows.len(), "kinds": rows }))
    }

    async fn do_list_resources(&self, p: ListResourcesParams) -> Result<String, ToolError> {
        let (conn, kind) = self
            .resolve(&p.cluster, &p.kind, p.api_version.as_deref(), || {
                check_namespace(p.namespace.as_deref())
            })
            .await?;
        if !kind.is_listable() {
            return Err(ToolError(format!("{} cannot be listed", kind.kind())));
        }
        let limit = policy::OBJECTS.clamp(p.limit);
        let page = table::list_table(
            &conn.client,
            &kind,
            &read::ListQuery {
                namespace: p.namespace.clone(),
                label_selector: p.label_selector.clone(),
                field_selector: None,
                limit: Some(limit),
                continue_token: p.continue_token.clone(),
            },
        )
        .await
        .map_err(|e| conn.fail(e))?;
        let is_secret = is_secret_kind(&kind);
        let (columns, rows, cut, redacted) =
            table_rows(&page, is_secret, table::unix_now(), policy::LIST_BYTES);
        let mut out = json!({
            "cluster": p.cluster,
            "kind": kind.kind(),
            "apiVersion": kind.api_version(),
            "namespace": p.namespace,
            "count": rows.len(),
            "truncated": page.more,
            "continue": page.continue_token,
            "redacted": redacted,
            "columns": columns,
            "rows": rows,
        });
        if cut {
            // Rows were dropped from the middle of a server page; a continue token would skip
            // them.
            out["truncated"] = json!(true);
            out["continue"] = Value::Null;
            out["hint"] = json!(format!(
                "Output was cut at {} bytes. Use a smaller limit, a namespace, or a label selector.",
                policy::LIST_BYTES
            ));
        }
        to_json(&out)
    }

    async fn do_get_resource(&self, p: GetResourceParams) -> Result<String, ToolError> {
        // The name and namespace go into the request path: one segment each, or not found,
        // checked before discovery, so the kind is named as the caller gave it.
        let (conn, kind) = self
            .resolve(&p.cluster, &p.kind, p.api_version.as_deref(), || {
                let bad_namespace = p
                    .namespace
                    .as_deref()
                    .is_some_and(|ns| !policy::is_label_name(ns));
                if bad_namespace || !policy::is_object_name(&p.name) {
                    return Err(not_found(&p.kind, &p.name));
                }
                Ok(())
            })
            .await?;
        let object = read::get(&conn.client, &kind, p.namespace.as_deref(), &p.name)
            .await
            .map_err(|e| conn.fail(e))?;
        let mut value = serde_json::to_value(&object)?;
        if value.get("apiVersion").is_none() {
            value["apiVersion"] = json!(kind.api_version());
            value["kind"] = json!(kind.kind());
        }
        // Redacted as a Secret by what was asked for, not only by what came back.
        let redacted = if is_secret_kind(&kind) {
            policy::sanitize_secret(&mut value)
        } else {
            policy::sanitize(&mut value)
        };
        let yaml = serde_saphyr::to_string(&value)?;
        let (text, truncated) = policy::truncate_bytes(&yaml, policy::OBJECT_BYTES);
        let mut out = String::new();
        if redacted > 0 {
            out.push_str(&format!(
                "# redacted={redacted}: credentials were replaced with [REDACTED:<kind>] markers\n"
            ));
        }
        if truncated {
            out.push_str(&format!(
                "# truncated=true: output cut at {} bytes\n",
                policy::OBJECT_BYTES
            ));
        }
        out.push_str(text);
        Ok(out)
    }

    async fn do_get_events(&self, p: GetEventsParams) -> Result<String, ToolError> {
        let ctx = self.context().await?;
        let conn = self.client(&ctx, &p.cluster).await?;
        check_namespace(p.namespace.as_deref())?;
        let limit = policy::EVENTS.clamp(p.limit) as usize;
        let involved = read::InvolvedObject {
            kind: p.object_kind.clone(),
            name: p.object_name.clone(),
        };
        let (events, more) = read::events(
            &conn.client,
            &read::EventQuery {
                namespace: p.namespace.clone(),
                involved: Some(involved),
                field_selector: None,
                fetch_limit: Some(EVENTS_FETCH),
            },
        )
        .await
        .map_err(|e| conn.fail(e))?;
        let truncated = more || events.len() > limit;
        let mut redacted = 0;
        let rows: Vec<Value> = events
            .iter()
            .take(limit)
            .map(|e| {
                json!({
                    "time": read::event_time(e).map(|t| t.to_string()),
                    "type": e.type_,
                    "reason": e.reason,
                    "object": format!(
                        "{}/{}",
                        e.involved_object.kind.as_deref().unwrap_or(""),
                        e.involved_object.name.as_deref().unwrap_or("")
                    ),
                    "namespace": e.metadata.namespace,
                    "count": e.count,
                    "message": e.message.clone().map(|mut m| {
                        redacted += policy::redact_text(&mut m);
                        m
                    }),
                })
            })
            .collect();
        to_json(&json!({
            "cluster": p.cluster,
            "count": rows.len(),
            "truncated": truncated,
            "redacted": redacted,
            "events": rows,
        }))
    }

    async fn do_get_pod_logs(&self, p: GetPodLogsParams) -> Result<String, ToolError> {
        let ctx = self.context().await?;
        let conn = self.client(&ctx, &p.cluster).await?;
        // The namespace and pod go into the request path, the container into its query.
        if !policy::is_label_name(&p.namespace) || !policy::is_pod_name(&p.pod) {
            return Err(not_found("pods", &p.pod));
        }
        if let Some(c) = p.container.as_deref().filter(|c| !policy::is_label_name(c)) {
            // As the API server words a container the pod does not have.
            return Err(ToolError(format!(
                "BadRequest: container {c:?} is not valid for pod {:?}",
                p.pod
            )));
        }
        let lines = policy::LOG_LINES.clamp(p.tail_lines);
        // Ask for one extra line to learn whether older lines exist.
        let mut tail = i64::from(lines) + 1;
        let mut cut_fetch;
        let mut text;
        loop {
            text = read::pod_logs(
                &conn.client,
                &read::LogQuery {
                    namespace: p.namespace.clone(),
                    pod: p.pod.clone(),
                    container: p.container.clone(),
                    tail_lines: Some(tail),
                    previous: p.previous.unwrap_or(false),
                    since_seconds: p.since_seconds.filter(|&s| s > 0).map(i64::from),
                    limit_bytes: Some(LOG_FETCH_BYTES),
                },
            )
            .await
            .map_err(|e| conn.fail(e))?;
            // The server stops after `limit_bytes` counted from the oldest requested line, so a
            // full response may be missing the newest lines. Ask again for fewer lines, down to
            // the newest one, so what is returned is always the latest output.
            cut_fetch = text.len() as i64 >= LOG_FETCH_BYTES;
            if !cut_fetch || tail == 1 {
                break;
            }
            let complete = text.lines().count().saturating_sub(1) as i64;
            tail = complete.clamp(1, tail / 2);
        }
        let out = policy::log_output(&text, lines as usize, policy::LOG_BYTES);
        let mut result = json!({
            "cluster": p.cluster,
            "namespace": p.namespace,
            "pod": p.pod,
            "container": p.container,
            "lines": out.text.lines().count(),
            "truncated": out.truncated || cut_fetch,
            "redacted": out.redacted,
            "log": out.text,
        });
        if let Some(note) = out.note {
            // Says why `log` is empty: a line too long to return without cutting it.
            result["note"] = json!(note);
        }
        to_json(&result)
    }
}

impl Inner {
    /// The merged kubeconfig for `paths`, parsed again only when a file changed. Exec auth
    /// plugins are set to never prompt: stdin is the MCP stream.
    fn load_kubeconfig(&self, paths: &[PathBuf]) -> Arc<LoadedKubeconfig> {
        let stamps = FileStamps::of(paths);
        if let Some((cached, loaded)) = &*self.kubeconfig.lock().unwrap()
            && *cached == stamps
        {
            return loaded.clone();
        }
        let mut loaded = kubeconfig::load(paths);
        cluster::disable_interactive_exec(&mut loaded.merged);
        let loaded = Arc::new(loaded);
        *self.kubeconfig.lock().unwrap() = Some((stamps, loaded.clone()));
        loaded
    }
}

/// Column names and rows for list_resources. Each row is `{name, namespace, created, cells}`,
/// with `cells` in column order as `kubectl get` prints them (ages computed from `now`).
/// Wide (priority > 0) columns are marked with `"wide": true`. Rows stop once they add up to
/// `max_bytes` of JSON; the flag says whether any were dropped. Cells pass through the
/// credential redactor (printer columns can show event and status messages); the last value
/// is how many credentials it replaced.
fn table_rows(
    page: &TablePage,
    is_secret: bool,
    now: i64,
    max_bytes: usize,
) -> (Vec<Value>, Vec<Value>, bool, usize) {
    let columns: Vec<Value> = page
        .columns
        .iter()
        .map(|c| {
            let mut col = json!({ "name": c.name });
            if c.priority > 0 {
                col["wide"] = json!(true);
            }
            col
        })
        .collect();
    let mut bytes = 0;
    let mut cut = false;
    let mut redacted = 0;
    let mut rows = Vec::new();
    for row in &page.rows {
        let mut row_redacted = 0;
        let value = {
            let cells: Vec<String> = (0..page.columns.len())
                .map(|i| {
                    let text = row.cell_text(i, now);
                    if is_secret && !policy::is_safe_secret_column(&page.columns[i].name) {
                        policy::REDACTED.to_owned()
                    } else {
                        let mut text = text.into_owned();
                        row_redacted += policy::redact_text(&mut text);
                        text
                    }
                })
                .collect();
            json!({
                "name": row.name,
                "namespace": row.namespace.as_deref(),
                "created": row.created.and_then(table::format_timestamp),
                "cells": cells,
            })
        };
        bytes += value.to_string().len();
        if bytes > max_bytes {
            cut = true;
            break;
        }
        // Counted only for rows that are returned.
        redacted += row_redacted;
        rows.push(value);
    }
    (columns, rows, cut, redacted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_core::table::{Cell, Column, ColumnRole, ColumnType, ListFormat, Row};

    fn col(name: &str, role: ColumnRole, priority: i32) -> Column {
        Column {
            name: name.into(),
            ty: ColumnType::String,
            format: "".into(),
            description: "".into(),
            priority,
            role,
        }
    }

    fn page(columns: Vec<Column>, cells: Vec<Cell>) -> TablePage {
        TablePage {
            columns: columns.into(),
            rows: vec![Row {
                uid: "u".into(),
                name: "web".into(),
                namespace: Some("demo".into()),
                resource_version: "1".into(),
                generation: None,
                created: Some(1_000),
                labels: Box::new([]),
                cells: cells.into(),
            }],
            format: ListFormat::Table,
            more: false,
            continue_token: None,
        }
    }

    // covers UP-10
    #[test]
    fn kind_errors_point_to_list_resource_kinds_or_name_the_candidates() {
        let missing = kind_error(ResolveKindError::NotFound("sprocket".into())).0;
        assert_eq!(
            missing,
            "no resource kind matches \"sprocket\". Call list_resource_kinds for the kinds."
        );
        let ambiguous = kind_error(ResolveKindError::Ambiguous {
            query: "wd".into(),
            candidates: vec!["gadgets.example.org".into(), "widgets.example.com".into()],
        })
        .0;
        assert_eq!(
            ambiguous,
            "\"wd\" matches several resource kinds. Pass one of these as kind: \
             gadgets.example.org, widgets.example.com."
        );
    }

    // covers UP-10
    #[test]
    fn rows_carry_server_columns_and_computed_age() {
        let page = page(
            vec![
                col("Name", ColumnRole::Name, 0),
                col("Namespace", ColumnRole::Namespace, 0),
                col("Ready", ColumnRole::Value, 0),
                col("Age", ColumnRole::Age, 0),
                col("Node", ColumnRole::Value, 1),
            ],
            vec![
                Cell::Name,
                Cell::Namespace,
                Cell::Text("1/1".into()),
                Cell::Age,
                Cell::Text("n1".into()),
            ],
        );
        let (columns, rows, cut, _) = table_rows(&page, false, 1_090, policy::LIST_BYTES);
        assert!(!cut);
        let (_, few, cut, _) = table_rows(&page, false, 1_090, 10);
        assert!(cut && few.is_empty(), "the byte cap applies");
        assert_eq!(columns[4], json!({"name": "Node", "wide": true}));
        assert_eq!(rows[0]["cells"], json!(["web", "demo", "1/1", "90s", "n1"]));
        assert_eq!(rows[0]["created"], json!("1970-01-01T00:16:40Z"));
    }

    // covers UP-10
    #[test]
    fn secret_rows_never_carry_values() {
        // A printer column that could show a value (an extension or a future server).
        let page = page(
            vec![
                col("Name", ColumnRole::Name, 0),
                col("Type", ColumnRole::Value, 0),
                col("Data", ColumnRole::Value, 0),
                col("Password", ColumnRole::Value, 0),
                col("Age", ColumnRole::Age, 0),
            ],
            vec![
                Cell::Name,
                Cell::Text("Opaque".into()),
                Cell::Int(2),
                Cell::Text("hunter2".into()),
                Cell::Age,
            ],
        );
        let (_, rows, _, _) = table_rows(&page, true, 1_000, policy::LIST_BYTES);
        let text = rows[0].to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert_eq!(
            rows[0]["cells"],
            json!(["web", "Opaque", "2", policy::REDACTED, "0s"])
        );
    }

    // covers UP-15
    #[test]
    fn agents_are_told_about_the_markers_and_the_count() {
        let server = FlintMcp::new(PathBuf::from("/nonexistent"), Vec::new(), None);
        let instructions = server.get_info().instructions.unwrap_or_default();
        assert!(instructions.contains("[REDACTED:<kind>]"), "{instructions}");
        assert!(instructions.contains("`redacted` count"), "{instructions}");
        assert!(instructions.contains("container args"), "{instructions}");
        let get = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == "get_resource")
            .expect("get_resource is listed");
        let description = get.description.unwrap_or_default();
        assert!(description.contains("[REDACTED:<kind>]"), "{description}");
        assert!(description.contains("# redacted=N"), "{description}");
    }

    // covers UP-15
    #[test]
    fn row_cells_are_redacted_and_counted() {
        // An Event row: the Message column shows the event message.
        let page = page(
            vec![
                col("Name", ColumnRole::Name, 0),
                col("Reason", ColumnRole::Value, 0),
                col("Message", ColumnRole::Value, 0),
            ],
            vec![
                Cell::Name,
                Cell::Text("Failed".into()),
                Cell::Text("pull from https://bot:hunter2@registry.example.com failed".into()),
            ],
        );
        let (_, rows, _, redacted) = table_rows(&page, false, 1_000, policy::LIST_BYTES);
        assert_eq!(redacted, 1);
        // A row that does not fit is not returned, so its credentials are not counted.
        let (_, rows_cut, cut, redacted) = table_rows(&page, false, 1_000, 10);
        assert!(cut && rows_cut.is_empty());
        assert_eq!(redacted, 0);
        assert_eq!(
            rows[0]["cells"],
            json!([
                "web",
                "Failed",
                "pull from https://bot:[REDACTED:password]@registry.example.com failed"
            ])
        );
    }

    // covers UP-10
    #[test]
    fn kind_rows_list_short_names_only_when_there_are_some() {
        let mut deploy = KindInfo {
            resource: kube::api::ApiResource {
                group: "apps".into(),
                version: "v1".into(),
                api_version: "apps/v1".into(),
                kind: "Deployment".into(),
                plural: "deployments".into(),
            },
            namespaced: true,
            verbs: vec!["list".into()],
            singular: "deployment".into(),
            short_names: vec!["deploy".into()],
        };
        assert_eq!(kind_row(&deploy)["shortNames"], json!(["deploy"]));
        deploy.short_names.clear();
        assert!(kind_row(&deploy).get("shortNames").is_none());
    }

    /// A server exposing `contexts` (name, server, user YAML) from a kubeconfig in a scratch
    /// directory, never the user's files.
    fn scratch_server(dir: &std::path::Path, contexts: &[(&str, &str, &str)]) -> FlintMcp {
        let mut kubeconfig = String::from("apiVersion: v1\nkind: Config\nclusters:\n");
        let mut settings = String::new();
        for (name, server, _) in contexts {
            kubeconfig += &format!("- name: {name}\n  cluster:\n    server: {server}\n");
            settings += &format!(
                "[[ai_exposed_clusters]]\ncontext = \"{name}\"\nserver = \"{server}\"\n\n"
            );
        }
        kubeconfig += "users:\n";
        for (name, _, user) in contexts {
            kubeconfig += &format!("- name: {name}\n  user:\n{user}");
        }
        kubeconfig += "contexts:\n";
        for (name, _, _) in contexts {
            kubeconfig +=
                &format!("- name: {name}\n  context:\n    cluster: {name}\n    user: {name}\n");
        }
        let path = dir.join("kubeconfig");
        std::fs::write(&path, kubeconfig).unwrap();
        std::fs::write(dir.join("settings.toml"), settings).unwrap();
        FlintMcp::new(dir.to_owned(), vec![path], None)
    }

    /// Whether `pid` names a process, zombies included.
    fn exists(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// Waits for `path` to hold a line, then returns its first line.
    async fn first_line(path: &std::path::Path) -> String {
        for _ in 0..500 {
            if let Some(line) = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| t.lines().next().map(str::to_owned))
            {
                return line;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{} was not written", path.display());
    }

    // covers UP-04: tool calls on a context whose exec auth plugin never answers (a login that
    // never comes) share one plugin run, and when they give up the plugin and what it started
    // are killed, so repeated calls never pile up plugin processes.
    #[tokio::test]
    async fn tool_calls_on_a_hanging_plugin_leave_at_most_one_plugin() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let traces = dir.path().join("traces");
        std::fs::create_dir(&traces).unwrap();
        let plugin = dir.path().join("plugin");
        std::fs::write(
            &plugin,
            "#!/bin/sh\necho $$ >> \"$PLUGIN_DIR/starts\"\nsleep 600 &\n\
             echo $! > \"$PLUGIN_DIR/child.tmp\" && mv \"$PLUGIN_DIR/child.tmp\" \"$PLUGIN_DIR/child\"\n\
             wait\n",
        )
        .unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let user = format!(
            "    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      command: {}\n      \
             interactiveMode: Never\n      env:\n      - name: PLUGIN_DIR\n        value: {}\n",
            plugin.display(),
            traces.display()
        );
        let server = scratch_server(dir.path(), &[("hang", "http://127.0.0.1:1", &user)]);
        let starts = || {
            std::fs::read_to_string(traces.join("starts"))
                .unwrap()
                .lines()
                .count()
        };

        for round in 1..=2 {
            let _ = std::fs::remove_file(traces.join("child"));
            let calls: Vec<_> = (0..3)
                .map(|_| {
                    let server = server.clone();
                    tokio::spawn(async move {
                        server
                            .do_list_kinds(ClusterParams {
                                cluster: "hang".into(),
                            })
                            .await
                    })
                })
                .collect();
            let child = first_line(&traces.join("child")).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(starts(), round, "one plugin for every call in flight");
            let leader = std::fs::read_to_string(traces.join("starts"))
                .unwrap()
                .lines()
                .last()
                .unwrap()
                .to_owned();
            assert!(exists(&leader) && exists(&child));
            // The calls give up, as at their timeout.
            for call in calls {
                call.abort();
            }
            for pid in [&leader, &child] {
                for _ in 0..500 {
                    if !exists(pid) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert!(!exists(pid), "plugin process {pid} outlived its calls");
            }
        }
    }

    // covers UP-04, UP-10
    #[tokio::test(start_paused = true)]
    async fn tool_errors_name_the_server_and_the_missing_plugin() {
        let dir = tempfile::tempdir().unwrap();
        // Accepts connections and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent = format!("http://{}", listener.local_addr().unwrap());
        let _keep = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let server = scratch_server(
            dir.path(),
            &[
                ("silent", &silent, "    token: not-a-real-token\n"),
                (
                    "noplugin",
                    "https://127.0.0.1:1",
                    "    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      \
                     command: /nowhere/flint-test-missing-plugin\n      args: [--secret-arg]\n",
                ),
            ],
        );
        let params = |cluster: &str| ClusterParams {
            cluster: cluster.into(),
        };

        let Err(ToolError(timeout)) = server.do_list_kinds(params("silent")).await else {
            panic!("a silent server has no kinds");
        };
        assert_eq!(timeout, format!("timed out after 10s reaching {silent}"));

        let Err(ToolError(missing)) = server.do_list_kinds(params("noplugin")).await else {
            panic!("a missing plugin cannot connect");
        };
        assert!(
            missing.starts_with("exec auth plugin `flint-test-missing-plugin` not found on PATH ("),
            "{missing}"
        );
        assert!(
            !missing.contains("nowhere") && !missing.contains("secret-arg"),
            "{missing}"
        );

        let listed = server.do_list_clusters().await.unwrap();
        assert!(
            listed.contains(&format!("timed out after 10s reaching {silent}")),
            "{listed}"
        );
        assert!(
            listed.contains("`flint-test-missing-plugin` not found"),
            "{listed}"
        );
    }
}
