//! The MCP tools. Each call re-reads `settings.toml` (so the app's Expose to AI toggle applies at
//! once), recomputes the kubeconfig file list from it, reloads kubeconfig when a file changed,
//! checks the cluster against the opt-in list, and only then touches the network.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flint_core::cache::{SlotMap, slot};
use flint_core::cluster::{self, ClusterError};
use flint_core::kubeconfig::{self, ContextInfo, FileStamps, LoadedKubeconfig};
use flint_core::read::{self, KindInfo};
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
struct ToolError(String);

impl<E: std::fmt::Display> From<E> for ToolError {
    fn from(e: E) -> Self {
        ToolError(e.to_string())
    }
}

type ToolResult = Result<CallToolResult, rmcp::ErrorData>;

fn finish(result: Result<String, ToolError>) -> ToolResult {
    Ok(match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(ToolError(msg)) => CallToolResult::error(vec![ContentBlock::text(msg)]),
    })
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
    /// Kind, plural, or singular name: "Pod", "pods", "deployments.apps", or a CRD kind.
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
    /// Kind, plural, or singular name: "Pod", "pods", "deployments.apps", or a CRD kind.
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
        description = "Get one object as YAML. managedFields are removed and Secret values redacted.",
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
    instructions = "Read-only access to Kubernetes clusters the user has opted in to in Flint (Expose to AI). Start with list_clusters. Nothing can be created, changed, or deleted. Secret values are always redacted, and large outputs are cut short with truncated=true."
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
    async fn client(&self, ctx: &CallContext, name: &str) -> Result<(Client, u64), ToolError> {
        let info = policy::exposed_cluster(&ctx.settings, &ctx.kubeconfig, name)?;
        let fingerprint = cluster::context_fingerprint(&ctx.kubeconfig.merged, &info.name)
            .ok_or_else(|| ToolError(format!("cluster \"{name}\" not found")))?;
        let slot = slot(&self.inner.clients, name, fingerprint, None);
        let client = slot
            .cell
            .get_or_try_init(|| async {
                tokio::time::timeout(
                    TOOL_CONNECT_TIMEOUT,
                    cluster::connect(&ctx.kubeconfig.merged, &info.name),
                )
                .await
                .map_err(|_| ClusterError::Timeout(TOOL_CONNECT_TIMEOUT))?
            })
            .await?;
        Ok((client.clone(), fingerprint))
    }

    async fn kinds(
        &self,
        cluster: &str,
        client: &Client,
        fingerprint: u64,
    ) -> Result<Arc<Vec<KindInfo>>, ToolError> {
        let slot = slot(
            &self.inner.discovery,
            cluster,
            fingerprint,
            Some(DISCOVERY_TTL),
        );
        let kinds = slot
            .cell
            .get_or_try_init(|| async { read::discover_kinds(client).await.map(Arc::new) })
            .await?;
        Ok(kinds.clone())
    }

    async fn resolve(
        &self,
        cluster: &str,
        kind: &str,
        api_version: Option<&str>,
    ) -> Result<(Client, KindInfo), ToolError> {
        let ctx = self.context().await?;
        let (client, fp) = self.client(&ctx, cluster).await?;
        let kinds = self.kinds(cluster, &client, fp).await?;
        let kind = read::resolve_kind(&kinds, kind, api_version)
            .map_err(|e| ToolError(format!("{e}. Call list_resource_kinds for the kinds.")))?
            .clone();
        Ok((client, kind))
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
                "server": info.server,
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
            let this = self.clone();
            let ctx = ctx.clone();
            set.spawn(async move {
                let probe = async {
                    let (client, _) = this.client(&ctx, &name).await.map_err(|e| e.0)?;
                    cluster::server_version(&client)
                        .await
                        .map(|v| v.git_version)
                        .map_err(|e| e.to_string())
                };
                let result = tokio::time::timeout(PROBE_TIMEOUT, probe)
                    .await
                    .unwrap_or_else(|_| Err(format!("timed out after {PROBE_TIMEOUT:?}")));
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
        let (client, fp) = self.client(&ctx, &p.cluster).await?;
        let kinds = self.kinds(&p.cluster, &client, fp).await?;
        let rows: Vec<Value> = kinds
            .iter()
            .map(|k| {
                json!({
                    "kind": k.kind(),
                    "apiVersion": k.api_version(),
                    "plural": k.plural(),
                    "namespaced": k.namespaced,
                    "listable": k.is_listable(),
                })
            })
            .collect();
        to_json(&json!({ "cluster": p.cluster, "count": rows.len(), "kinds": rows }))
    }

    async fn do_list_resources(&self, p: ListResourcesParams) -> Result<String, ToolError> {
        let (client, kind) = self
            .resolve(&p.cluster, &p.kind, p.api_version.as_deref())
            .await?;
        if !kind.is_listable() {
            return Err(ToolError(format!("{} cannot be listed", kind.kind())));
        }
        let limit = policy::OBJECTS.clamp(p.limit);
        let page = table::list_table(
            &client,
            &kind,
            &read::ListQuery {
                namespace: p.namespace.clone(),
                label_selector: p.label_selector.clone(),
                field_selector: None,
                limit: Some(limit),
                continue_token: p.continue_token.clone(),
            },
        )
        .await?;
        let is_secret = kind.group().is_empty() && kind.kind() == "Secret";
        let (columns, rows, cut) =
            table_rows(&page, is_secret, table::unix_now(), policy::LIST_BYTES);
        let mut out = json!({
            "cluster": p.cluster,
            "kind": kind.kind(),
            "apiVersion": kind.api_version(),
            "namespace": p.namespace,
            "count": rows.len(),
            "truncated": page.more,
            "continue": page.continue_token,
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
        let (client, kind) = self
            .resolve(&p.cluster, &p.kind, p.api_version.as_deref())
            .await?;
        let object = read::get(&client, &kind, p.namespace.as_deref(), &p.name).await?;
        let mut value = serde_json::to_value(&object)?;
        if value.get("apiVersion").is_none() {
            value["apiVersion"] = json!(kind.api_version());
            value["kind"] = json!(kind.kind());
        }
        policy::sanitize(&mut value);
        let yaml = serde_saphyr::to_string(&value)?;
        let (text, truncated) = policy::truncate_bytes(&yaml, policy::OBJECT_BYTES);
        Ok(if truncated {
            format!(
                "# truncated=true: output cut at {} bytes\n{text}",
                policy::OBJECT_BYTES
            )
        } else {
            text.to_owned()
        })
    }

    async fn do_get_events(&self, p: GetEventsParams) -> Result<String, ToolError> {
        let ctx = self.context().await?;
        let (client, _) = self.client(&ctx, &p.cluster).await?;
        let limit = policy::EVENTS.clamp(p.limit) as usize;
        let involved = read::InvolvedObject {
            kind: p.object_kind.clone(),
            name: p.object_name.clone(),
        };
        let (events, more) = read::events(
            &client,
            &read::EventQuery {
                namespace: p.namespace.clone(),
                involved: Some(involved),
                field_selector: None,
                fetch_limit: Some(EVENTS_FETCH),
            },
        )
        .await?;
        let truncated = more || events.len() > limit;
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
                    "message": e.message,
                })
            })
            .collect();
        to_json(&json!({
            "cluster": p.cluster,
            "count": rows.len(),
            "truncated": truncated,
            "events": rows,
        }))
    }

    async fn do_get_pod_logs(&self, p: GetPodLogsParams) -> Result<String, ToolError> {
        let ctx = self.context().await?;
        let (client, _) = self.client(&ctx, &p.cluster).await?;
        let lines = policy::LOG_LINES.clamp(p.tail_lines);
        // Ask for one extra line to learn whether older lines exist.
        let mut tail = i64::from(lines) + 1;
        let mut cut_fetch;
        let mut text;
        loop {
            text = read::pod_logs(
                &client,
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
            .await?;
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
        let (text, cut_lines) = policy::tail_lines(&text, lines as usize);
        let (text, cut_bytes) = policy::tail_bytes(text, policy::LOG_BYTES);
        to_json(&json!({
            "cluster": p.cluster,
            "namespace": p.namespace,
            "pod": p.pod,
            "container": p.container,
            "lines": text.lines().count(),
            "truncated": cut_lines || cut_bytes || cut_fetch,
            "log": text,
        }))
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
/// `max_bytes` of JSON; the flag says whether any were dropped.
fn table_rows(
    page: &TablePage,
    is_secret: bool,
    now: i64,
    max_bytes: usize,
) -> (Vec<Value>, Vec<Value>, bool) {
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
    let mut rows = Vec::new();
    for row in &page.rows {
        let value = {
            let cells: Vec<String> = (0..page.columns.len())
                .map(|i| {
                    let text = row.cell_text(i, now);
                    if is_secret && !policy::is_safe_secret_column(&page.columns[i].name) {
                        policy::REDACTED.to_owned()
                    } else {
                        text.into_owned()
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
        rows.push(value);
    }
    (columns, rows, cut)
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
        let (columns, rows, cut) = table_rows(&page, false, 1_090, policy::LIST_BYTES);
        assert!(!cut);
        let (_, few, cut) = table_rows(&page, false, 1_090, 10);
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
        let (_, rows, _) = table_rows(&page, true, 1_000, policy::LIST_BYTES);
        let text = rows[0].to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert_eq!(
            rows[0]["cells"],
            json!(["web", "Opaque", "2", policy::REDACTED, "0s"])
        );
    }
}
