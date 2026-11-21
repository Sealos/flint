//! The MCP tools. Each call re-reads `settings.toml` (so the app's Expose to AI toggle applies at
//! once), recomputes the kubeconfig file list from it, reloads kubeconfig when a file changed,
//! checks the cluster against the opt-in list, and only then touches the network.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flint_core::activity;
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

use crate::limits::{Admission, Counted, Limits, Tracked};
use crate::policy;

/// How long `list_clusters` waits for each cluster's `/version`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How many of a scope's namespaces `list_clusters` looks up to report the ones that do not exist.
const SCOPE_NAMESPACES_CHECKED: usize = 64;
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
    discovery: SlotMap<Discovered>,
    /// Which program this is, for advice that can be followed where it runs.
    flavour: policy::Flavour,
    /// Unmatched scopes already logged, so each is logged once.
    warned_scopes: std::sync::Mutex<std::collections::HashSet<String>>,
    /// How many tool calls run and wait, and the reply bytes not yet written (#252).
    admission: Admission,
}

/// The longest a call waits for its activity record to be written, rather than hold its reply.
/// The write itself gives up on the log's file lock (the one wait that is not this process's
/// own) after [`activity::LOCK_WAIT`], shorter than this, so a write never outlives its wait by
/// much.
const RECORD_WAIT: Duration = Duration::from_secs(2);
const _: () = assert!(activity::LOCK_WAIT.as_millis() < RECORD_WAIT.as_millis());

impl Inner {
    /// Writes `record` to the activity log, keying the servers it `reached`, on the call's own
    /// path, waiting at most [`RECORD_WAIT`]. A record that could not be written in time, or at
    /// all, is logged.
    async fn record(&self, record: activity::ActivityRecord, reached: activity::Reached) {
        let dir = self.config_dir.clone();
        let write =
            tokio::task::spawn_blocking(move || activity::append_call(&dir, &record, &reached));
        match tokio::time::timeout(RECORD_WAIT, write).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => tracing::warn!(error = %e, "cannot write the agent activity log"),
            Ok(Err(e)) => tracing::warn!(error = %e, "cannot write the agent activity log"),
            Err(_) => tracing::warn!("the agent activity log was not written in time"),
        }
    }
}

/// Discovery results for one cluster.
type Kinds = Arc<Vec<KindInfo>>;

/// A cluster's kinds, and the API groups discovery could not read this time.
#[derive(Clone)]
struct Discovered {
    kinds: Kinds,
    skipped_groups: Arc<Vec<String>>,
}

/// A failed call, shown to the agent as a tool error.
///
/// There is deliberately no `From<ClusterError>`: a cluster error must name its target first,
/// through [`Conn::fail`], so it says which server timed out or which plugin is missing.
#[derive(Debug)]
struct ToolError {
    message: String,
    /// The call named something the agent may not read: a cluster not exposed (the message is
    /// the same as for a cluster that does not exist), or a namespace or kind outside its scope
    /// (one message, whatever it named). Only [`policy::PolicyError`] sets it.
    denied: bool,
}

impl ToolError {
    fn new(message: impl Into<String>) -> Self {
        ToolError {
            message: message.into(),
            denied: false,
        }
    }
}

macro_rules! tool_error_from {
    ($($t:ty),* $(,)?) => {$(
        impl From<$t> for ToolError {
            fn from(e: $t) -> Self {
                ToolError::new(e.to_string())
            }
        }
    )*};
}

tool_error_from!(
    serde_json::Error,
    serde_saphyr::ser::Error,
    tokio::task::JoinError,
);

impl From<policy::PolicyError> for ToolError {
    fn from(e: policy::PolicyError) -> Self {
        ToolError {
            message: e.to_string(),
            denied: true,
        }
    }
}

/// A connection to an exposed cluster, with what its errors may name.
struct Conn {
    client: Client,
    fingerprint: u64,
    target: Target,
    /// What the agent may read of the cluster (ADR-0013).
    scope: policy::AiScope,
}

impl Conn {
    /// A cluster error as the agent reads it: naming the server and the exec plugin, with the
    /// server's own text cut to one short line ([`ClusterError::for_agent`]).
    fn fail(&self, e: ClusterError) -> ToolError {
        ToolError::new(e.for_target(&self.target).for_agent())
    }
}

type ToolResult = Result<CallToolResult, rmcp::ErrorData>;

/// What `list_clusters` learned of a cluster: its version or why not, and how its scope checked
/// out, if the cluster could be reached.
type Probed = (Result<String, String>, Option<policy::ScopeCheck>);

/// What a tool call named, as the agent wrote it: recorded in the activity log.
#[derive(Debug, Default, Clone)]
struct Named {
    cluster: Option<String>,
    kind: Option<String>,
    namespace: Option<String>,
    name: Option<String>,
    container: Option<String>,
}

impl Named {
    fn cluster(cluster: &str) -> Self {
        Named {
            cluster: Some(cluster.to_owned()),
            ..Named::default()
        }
    }
}

/// How many items a reply holds and whether it was cut short, read from the reply's own
/// fields (what the agent saw): `count` (or `lines` for logs, `clusters` for list_clusters) and
/// `truncated` in JSON, and a `# truncated=true` header line in get_resource's YAML.
fn reply_size(tool: &str, text: &str) -> (Option<u64>, bool) {
    if tool == "get_resource" {
        let truncated = text
            .lines()
            .take_while(|l| l.starts_with('#'))
            .any(|l| l.starts_with("# truncated=true"));
        return (Some(1), truncated);
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (None, false);
    };
    let items = value
        .get("count")
        .or_else(|| value.get("lines"))
        .and_then(Value::as_u64)
        .or_else(|| {
            let clusters = value.get("clusters")?.as_array()?;
            u64::try_from(clusters.len()).ok()
        });
    let truncated = value.get("truncated").and_then(Value::as_bool) == Some(true);
    (items, truncated)
}

impl FlintMcp {
    /// Turns a tool's result into its reply, and records the call in the activity log
    /// (ADR-0013, UP-16): what it named and how much came back, never the content.
    async fn finish(
        &self,
        tool: &'static str,
        named: Named,
        result: Result<String, ToolError>,
    ) -> ToolResult {
        let (text, outcome, ok) = match result {
            Ok(text) => (text, activity::Outcome::Ok, true),
            // A server's error text can quote what a workload wrote (webhooks, aggregated APIs).
            Err(ToolError {
                mut message,
                denied,
            }) => {
                policy::redact_text(&mut message);
                let outcome = if denied {
                    activity::Outcome::Denied
                } else {
                    activity::Outcome::Failed
                };
                (message, outcome, false)
            }
        };
        let (items, truncated) = if ok {
            reply_size(tool, &text)
        } else {
            (None, false)
        };
        // The server `client` found for this call's cluster, from the call's own settings and
        // kubeconfig: none for a call that names no cluster or one not exposed.
        let (server, endpoint, probed) = CALL
            .try_with(|c| {
                let c = c.borrow();
                (c.server.clone(), c.endpoint.clone(), c.probed.clone())
            })
            .unwrap_or_default();
        let (server, endpoint) = match named.cluster {
            Some(_) => (server, endpoint),
            None => (None, None),
        };
        let reached = activity::Reached {
            server: endpoint,
            probed,
        };
        let record = activity::ActivityRecord {
            at_ms: activity::now_ms(),
            tool: tool.to_owned(),
            server,
            cluster: named.cluster,
            kind: named.kind,
            namespace: named.namespace,
            name: named.name,
            container: named.container,
            items,
            bytes: text.len() as u64,
            truncated,
            outcome,
            cluster_key: None,
            server_key: None,
            probed: Vec::new(),
            probed_more: 0,
        };
        // Within `call_tool`, the record waits for the call's outcome there: a deadline that
        // fires after this still makes it a timeout. Called directly (tests), it is written now.
        if let Err((record, reached)) = CALL
            .try_with(|c| c.borrow_mut().record = Some((record.clone(), reached.clone())))
            .map_err(|_| (record, reached))
        {
            self.record(record, reached).await;
        }
        let content = vec![ContentBlock::text(text)];
        Ok(if ok {
            CallToolResult::success(content)
        } else {
            CallToolResult::error(content)
        })
    }

    /// Writes `record` to the activity log, keying the servers it `reached` ([`Inner::record`]).
    async fn record(&self, record: activity::ActivityRecord, reached: activity::Reached) {
        self.inner.record(record, reached).await;
    }
}

/// What one tool call learned on its way, for its record.
#[derive(Default)]
struct CallNote {
    /// The API server of the cluster the call reads, once [`FlintMcp::client`] found it exposed
    /// (without credentials).
    server: Option<String>,
    /// That server as the kubeconfig has it, for its key ([`activity::append_call`]).
    endpoint: Option<String>,
    /// The contexts `list_clusters` probed, with their servers as the kubeconfig has them.
    probed: Vec<(String, Option<String>)>,
    /// What its tool would record ([`FlintMcp::finish`]), and the servers it reached, written
    /// once the call's outcome is settled.
    record: Option<(activity::ActivityRecord, activity::Reached)>,
}

tokio::task_local! {
    /// The current tool call's note. Each call gets its own ([`ServerHandler::call_tool`]).
    static CALL: std::cell::RefCell<CallNote>;
}

/// What a call's raw arguments name, for a call rejected before its tool ran: only string
/// values without control characters, under the names the tools use. [`activity::append`] caps
/// their length.
fn named_in(arguments: Option<&serde_json::Map<String, Value>>) -> Named {
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| arguments?.get(*k)?.as_str())
            .filter(|value| !value.chars().any(char::is_control))
            .map(str::to_owned)
    };
    Named {
        cluster: text(&["cluster"]),
        kind: text(&["kind", "object_kind"]),
        namespace: text(&["namespace"]),
        name: text(&["name", "pod", "object_name"]),
        container: text(&["container"]),
    }
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
            ToolError::new(format!("{e}. Call list_resource_kinds for the kinds."))
        }
        ResolveKindError::Ambiguous { .. } => ToolError::new(format!("{e}.")),
    }
}

/// What the API server answers for an object that does not exist (`NotFound: pods "x" not
/// found`), where `resource` is the plural, or the kind as the caller named it. A name or
/// namespace that is not a single valid path segment gets this answer without any request, so
/// it can never reach another path, and the caller learns nothing it would not from a missing
/// object.
fn not_found(resource: &str, name: &str) -> ToolError {
    ToolError::new(format!("NotFound: {resource} {name:?} not found"))
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

/// `list_clusters` takes no arguments, and says so in its schema.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("properties" = {}))]
pub struct ListClustersParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClusterParams {
    /// Cluster name, as returned by list_clusters.
    pub cluster: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// How many unsupported arguments a refusal names, and how much of each: their names come from
/// the client and can be any length.
const UNSUPPORTED_NAMED: usize = 5;
const UNSUPPORTED_NAME_CHARS: usize = 64;
/// A longer name is not read at all: redacting it would hold a worker for as long as the client
/// likes, and a prefix of it could hold part of a credential.
const UNSUPPORTED_NAME_BYTES: usize = 1024;

/// `result` with an argument error's repeated value taken out ([`policy::argument_error`]). rmcp
/// returns that error as a failed tool result.
fn without_argument_values(
    mut result: Result<rmcp::model::CallToolResponse, rmcp::ErrorData>,
) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
    if let Ok(rmcp::model::CallToolResponse::Complete(done)) = &mut result
        && done.is_error == Some(true)
    {
        for block in &mut done.content {
            let safe = block
                .as_text()
                .and_then(|text| policy::argument_error(&text.text));
            if let Some(safe) = safe {
                *block = ContentBlock::text(safe);
            }
        }
    }
    result
}

/// The refusal for `arguments` that `tool`'s schema does not have, naming them and the accepted
/// ones. A tool that dropped them would answer as if a filter it never applied had matched.
/// `_meta` is not among them: it sits beside the arguments in the request.
fn unsupported_arguments(
    tool: &rmcp::model::Tool,
    arguments: Option<&serde_json::Map<String, Value>>,
) -> Option<String> {
    let accepted = tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object);
    // One past what is shown, to say there are more: the work stays bounded by the schema.
    let unsupported: Vec<&String> = arguments?
        .keys()
        .filter(|name| !accepted.is_some_and(|a| a.contains_key(name.as_str())))
        .take(UNSUPPORTED_NAMED + 1)
        .collect();
    let named: Vec<String> = unsupported
        .iter()
        .take(UNSUPPORTED_NAMED)
        .map(|name| {
            if name.len() > UNSUPPORTED_NAME_BYTES {
                return "<a name too long to show>".to_owned();
            }
            // Whole, before it is cut: a rule can need what comes after the cut. On its own, so
            // a rule that takes the rest of the line cannot take the accepted arguments too.
            let mut name = (*name).clone();
            policy::redact_text(&mut name);
            let short: String = name.chars().take(UNSUPPORTED_NAME_CHARS).collect();
            // Quoted with escapes: control characters never reach the reply.
            format!("{short:?}")
        })
        .collect();
    if named.is_empty() {
        return None;
    }
    let mut names = named.join(", ");
    if unsupported.len() > UNSUPPORTED_NAMED {
        names.push_str(" and more");
    }
    let accepted: Vec<&str> = accepted
        .into_iter()
        .flat_map(|a| a.keys().map(String::as_str))
        .collect();
    Some(if accepted.is_empty() {
        format!("{} takes no arguments. Unsupported: {names}.", tool.name)
    } else {
        format!(
            "{} does not support {names}. Accepted arguments: {}.",
            tool.name,
            accepted.join(", ")
        )
    })
}

#[tool_router]
impl FlintMcp {
    /// `kubeconfig_base` is the `KUBECONFIG` list (or `~/.kube/config`); the settings'
    /// `kubeconfig_paths`, expanded with `home`, are appended on every call.
    pub fn new(config_dir: PathBuf, kubeconfig_base: Vec<PathBuf>, home: Option<PathBuf>) -> Self {
        Self::with_limits(config_dir, kubeconfig_base, home, Limits::default())
    }

    /// [`FlintMcp::new`] with other [`Limits`].
    pub fn with_limits(
        config_dir: PathBuf,
        kubeconfig_base: Vec<PathBuf>,
        home: Option<PathBuf>,
        limits: Limits,
    ) -> Self {
        Self::built(
            config_dir,
            kubeconfig_base,
            home,
            limits,
            policy::Flavour::App,
        )
    }

    /// [`FlintMcp::new`] for the program `flavour`, whose advice it gives.
    pub fn with_flavour(
        config_dir: PathBuf,
        kubeconfig_base: Vec<PathBuf>,
        home: Option<PathBuf>,
        flavour: policy::Flavour,
    ) -> Self {
        Self::built(
            config_dir,
            kubeconfig_base,
            home,
            Limits::default(),
            flavour,
        )
    }

    fn built(
        config_dir: PathBuf,
        kubeconfig_base: Vec<PathBuf>,
        home: Option<PathBuf>,
        limits: Limits,
        flavour: policy::Flavour,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                flavour,
                config_dir,
                kubeconfig_base,
                home,
                kubeconfig: Mutex::default(),
                clients: Mutex::default(),
                discovery: Mutex::default(),
                warned_scopes: std::sync::Mutex::default(),
                admission: Admission::new(limits),
            }),
            tool_router: Self::tool_router(),
        }
    }

    /// `transport` with the bytes of replies not yet written counted, which [`Limits`] bounds.
    /// Serve the server over this.
    pub fn counted<T>(
        &self,
        transport: T,
    ) -> impl rmcp::transport::Transport<rmcp::RoleServer> + use<T>
    where
        T: rmcp::transport::Transport<rmcp::RoleServer> + 'static,
    {
        let admission = &self.inner.admission;
        // A tool call refused at the transport never reaches `call_tool`:
        // recorded from there.
        let inner = Arc::downgrade(&self.inner);
        admission.replies().on_dropped_call(Box::new(move |tool| {
            let inner = inner.clone();
            Box::pin(async move {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                // Its arguments are unvalidated: none is recorded, or trusted to say which
                // cluster the call was about.
                inner
                    .record(
                        activity::ActivityRecord {
                            at_ms: activity::now_ms(),
                            tool,
                            server: None,
                            cluster: None,
                            kind: None,
                            namespace: None,
                            name: None,
                            container: None,
                            items: None,
                            bytes: 0,
                            truncated: false,
                            outcome: activity::Outcome::Failed,
                            cluster_key: None,
                            server_key: None,
                            probed: Vec::new(),
                            probed_more: 0,
                        },
                        activity::Reached::default(),
                    )
                    .await;
            })
        }));
        Counted::new(transport, admission.replies(), &admission.limits())
    }

    /// This server as the service to run over [`Self::counted`]'s transport: it tells the
    /// transport when each tool call's handler ends.
    pub fn tracked(&self) -> impl rmcp::Service<rmcp::RoleServer> + use<> {
        Tracked::new(self.clone(), self.inner.admission.replies())
    }

    #[tool(
        description = "List the Kubernetes clusters the user exposed to AI agents in Flint, with \
        server URL and whether a connection works.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_clusters(&self, Parameters(_): Parameters<ListClustersParams>) -> ToolResult {
        let result = self.do_list_clusters().await;
        self.finish("list_clusters", Named::default(), result).await
    }

    #[tool(
        description = "List the resource kinds a cluster serves, CRDs included, from API discovery.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_resource_kinds(&self, Parameters(p): Parameters<ClusterParams>) -> ToolResult {
        let named = Named::cluster(&p.cluster);
        let result = self.do_list_kinds(p).await;
        self.finish("list_resource_kinds", named, result).await
    }

    #[tool(
        description = "List objects of one kind as the rows `kubectl get -o wide` prints: \
        the server's columns (ready counts, status, restarts, replicas, CRD printer columns) plus \
        name, namespace and age. Use get_resource for a full object. When truncated is true, pass the returned \
        continue token to get the next page; if continue is null, narrow by namespace or label.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_resources(&self, Parameters(p): Parameters<ListResourcesParams>) -> ToolResult {
        let named = Named {
            kind: Some(p.kind.clone()),
            namespace: p.namespace.clone(),
            ..Named::cluster(&p.cluster)
        };
        let result = self.do_list_resources(p).await;
        self.finish("list_resources", named, result).await
    }

    #[tool(
        description = "Get one object as YAML. A `# managed_by=` header line names the Argo CD or Flux object that manages it, whose sync may revert a change. managedFields are removed and Secret values redacted. Credentials in messages, annotations and container args and commands are replaced with [REDACTED:<kind>] markers; a first line `# redacted=N` says how many.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_resource(&self, Parameters(p): Parameters<GetResourceParams>) -> ToolResult {
        let named = Named {
            kind: Some(p.kind.clone()),
            namespace: p.namespace.clone(),
            name: Some(p.name.clone()),
            ..Named::cluster(&p.cluster)
        };
        let result = self.do_get_resource(p).await;
        self.finish("get_resource", named, result).await
    }

    #[tool(
        description = "Get recent events, newest first, for a namespace or for one object.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_events(&self, Parameters(p): Parameters<GetEventsParams>) -> ToolResult {
        let named = Named {
            kind: p.object_kind.clone(),
            namespace: p.namespace.clone(),
            name: p.object_name.clone(),
            ..Named::cluster(&p.cluster)
        };
        let result = self.do_get_events(p).await;
        self.finish("get_events", named, result).await
    }

    #[tool(
        description = "Get the last lines of a pod container's logs.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_pod_logs(&self, Parameters(p): Parameters<GetPodLogsParams>) -> ToolResult {
        let named = Named {
            kind: Some("pods".to_owned()),
            namespace: Some(p.namespace.clone()),
            name: Some(p.pod.clone()),
            container: p.container.clone(),
            ..Named::cluster(&p.cluster)
        };
        let result = self.do_get_pod_logs(p).await;
        self.finish("get_pod_logs", named, result).await
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "flint",
    instructions = "Read-only access to Kubernetes clusters the user has opted in to in Flint (Expose to AI). Start with list_clusters. Nothing can be created, changed, or deleted. Values of Secret objects are redacted, also when a Secret is nested in another object or held as JSON or YAML in an object field, list cell or event message; pod logs get credential redaction only, and other custom resource fields are shown as stored. Values that look like credentials in pod logs, event messages, list cells, object messages, annotations and container args and commands are replaced with [REDACTED:<kind>] markers, and each result's `redacted` count says how many. Large outputs are cut short with truncated=true."
)]
impl ServerHandler for FlintMcp {
    /// Every call, with its own `CALL` note, within the [`Limits`]: admitted (or refused at
    /// once when too many are in flight), stopped when the client cancels it, and ended at the
    /// deadline. A call the router answers without running its tool (an unknown tool, arguments
    /// that do not fit the tool's schema) is recorded here, as failed, with what its arguments
    /// name; so is one cancelled or past its deadline once admitted. One refused or cancelled
    /// before that is recorded with its tool only: its arguments are unvalidated. The others are
    /// recorded by their tool (`FlintMcp::finish`).
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let tool = request.name.to_string();
        // Nothing from the arguments until the call is admitted and they fit its tool.
        let mut named = Named::default();
        let cancelled = context.ct.clone();
        let admission = &self.inner.admission;
        let deadline = admission.limits().deadline;
        let turn = admission.enter_unless(cancelled.cancelled()).await;
        let failed = |text: &str| Ok(CallToolResult::error(vec![ContentBlock::text(text)]).into());
        // The reply, the record its tool left when it is the outcome, and what the call learned
        // on its way (for a record of its own otherwise).
        let (result, record, note) = match turn {
            // Cancelled while it waited for a turn.
            None => (
                Err(rmcp::ErrorData::internal_error("cancelled", None)),
                None,
                CallNote::default(),
            ),
            Some(Err(busy)) => (failed(&busy.to_string()), None, CallNote::default()),
            Some(Ok(turn)) => {
                let refused = self
                    .tool_router
                    .get(&request.name)
                    .and_then(|tool| unsupported_arguments(tool, request.arguments.as_ref()));
                if let Some(text) = refused {
                    drop(turn);
                    // Its arguments are unvalidated: none is recorded, or trusted to say which
                    // cluster the call was about.
                    (failed(&text), None, CallNote::default())
                } else {
                    named = named_in(request.arguments.as_ref());
                    let call =
                        rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
                    CALL.scope(std::cell::RefCell::default(), async {
                        // Dropping the call's future ends its Kubernetes requests.
                        let (result, finished) = tokio::select! {
                            result = self.tool_router.call(call) => (result, true),
                            () = cancelled.cancelled() => {
                                (Err(rmcp::ErrorData::internal_error("cancelled", None)), false)
                            }
                            () = tokio::time::sleep(deadline) => {
                                let server = CALL.with(|c| c.borrow().server.clone());
                                let mut text =
                                    ClusterError::timeout(deadline, server.as_deref()).for_agent();
                                // As every reply: a server URL's path can hold anything.
                                policy::redact_text(&mut text);
                                (failed(&text), false)
                            }
                        };
                        drop(turn);
                        let (record, note) = CALL.with(|c| {
                            let mut c = c.borrow_mut();
                            let record = c.record.take().filter(|_| finished);
                            (record, std::mem::take(&mut *c))
                        });
                        // A tool that ran left its record: its text is the cluster's, not rmcp's
                        // error for arguments that never reached it.
                        let result = if finished && record.is_none() {
                            without_argument_values(result)
                        } else {
                            result
                        };
                        (result, record, note)
                    })
                    .await
                }
            }
        };
        let (record, reached) = record.unwrap_or_else(|| {
            let names_cluster = named.cluster.is_some();
            let bytes = match &result {
                Ok(rmcp::model::CallToolResponse::Complete(done)) => done
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.len()))
                    .sum(),
                Ok(_) => 0,
                Err(error) => error.message.len(),
            };
            // A call that ended early is still shown with the clusters it read about
            // (`list_clusters`' probes).
            let reached = activity::Reached {
                server: note.endpoint.filter(|_| names_cluster),
                probed: note.probed,
            };
            let record = activity::ActivityRecord {
                at_ms: activity::now_ms(),
                tool,
                server: note.server.filter(|_| names_cluster),
                cluster: named.cluster,
                kind: named.kind,
                namespace: named.namespace,
                name: named.name,
                container: named.container,
                items: None,
                bytes: bytes as u64,
                truncated: false,
                outcome: activity::Outcome::Failed,
                cluster_key: None,
                server_key: None,
                probed: Vec::new(),
                probed_more: 0,
            };
            (record, reached)
        });
        self.record(record, reached).await;
        result
    }
}

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
            // The full TOML error quotes lines of settings.toml; give the agent one safe line.
            let settings = settings::load_settings(&inner.config_dir).map_err(|e| {
                let setup = policy::Setup::new(inner.flavour, &inner.config_dir);
                ToolError::new(format!(
                    "Flint's settings.toml is invalid ({}), so no cluster is exposed. {}",
                    e.safe_reason(),
                    setup.fix_settings()
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
        // The server this call's settings expose the cluster at: what its record names.
        let _ = CALL.try_with(|c| {
            let mut note = c.borrow_mut();
            note.server = info.server.as_deref().map(flint_core::redact::server_url);
            note.endpoint = info.server.clone();
        });
        let scope = policy::cluster_scope(&ctx.settings, info);
        let fingerprint = cluster::context_fingerprint(&ctx.kubeconfig.merged, &info.name)
            .ok_or_else(|| ToolError::new(format!("cluster \"{name}\" not found")))?;
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
            .map_err(ToolError::new)?
            .clone();
        Ok(Conn {
            client,
            fingerprint,
            target,
            scope,
        })
    }

    async fn kinds(&self, cluster: &str, conn: &Conn) -> Result<Kinds, ToolError> {
        Ok(self.discovered(cluster, conn).await?.kinds)
    }

    async fn discovered(&self, cluster: &str, conn: &Conn) -> Result<Discovered, ToolError> {
        let slot = slot(
            &self.inner.discovery,
            cluster,
            conn.fingerprint,
            Some(DISCOVERY_TTL),
        );
        // Groups skipped this time are missing until the cache expires; the agent can retry.
        let discovered = slot
            .cell
            .get_or_try_init(|| async {
                read::discover_kinds(&conn.client).await.map(|found| {
                    let skipped_groups = Arc::new(found.incomplete_groups());
                    Discovered {
                        kinds: Arc::new(found.kinds),
                        skipped_groups,
                    }
                })
            })
            .await
            .map_err(|e| conn.fail(e))?;
        Ok(discovered.clone())
    }

    /// The cluster's kinds, with `conn`'s scope resolved against them.
    async fn resolved_scope(
        &self,
        cluster: &str,
        conn: &Conn,
    ) -> Result<(Kinds, policy::ResolvedKinds), ToolError> {
        let found = self.discovered(cluster, conn).await?;
        let resolved =
            policy::resolve_scope_kinds(&conn.scope, &found.kinds, &found.skipped_groups);
        Ok((found.kinds, resolved))
    }

    /// The kinds of `cluster` the agent may read: all of them, or those its scope allows.
    async fn kinds_in_scope(&self, cluster: &str, conn: &Conn) -> Result<Kinds, ToolError> {
        if conn.scope.is_everything() {
            return self.kinds(cluster, conn).await;
        }
        let (kinds, resolved) = self.resolved_scope(cluster, conn).await?;
        Ok(Arc::new(policy::kinds_in_scope(&resolved.scope, &kinds)))
    }

    /// What in `conn`'s scope matches nothing on the cluster, and what could not be checked, for
    /// `list_clusters`. Each result is logged once. A namespace whose lookup fails is counted as
    /// unchecked, not reported as missing.
    async fn unmatched_scope(&self, cluster: &str, conn: &Conn) -> Option<policy::ScopeCheck> {
        if conn.scope.is_everything() {
            return None;
        }
        // One limit for the whole check, so a stalled lookup keeps what was already found.
        let until = tokio::time::Instant::now() + PROBE_TIMEOUT;
        let resolved = tokio::time::timeout_at(until, self.resolved_scope(cluster, conn)).await;
        let Ok(Ok((kinds, resolved))) = resolved else {
            return Some(policy::ScopeCheck::not_run(&conn.scope));
        };
        let discovery_incomplete = resolved.discovery_incomplete;
        let unmatched_kinds = resolved.unmatched;
        let mut unmatched_namespaces = Vec::new();
        let scoped = &conn.scope.namespaces;
        let mut unchecked_namespaces = scoped.len();
        let namespaces = kinds
            .iter()
            .find(|k| k.group().is_empty() && k.plural() == "namespaces");
        if let Some(namespaces) = namespaces {
            for name in scoped.iter().take(SCOPE_NAMESPACES_CHECKED) {
                // Never sent: it goes into the request path, and no namespace has such a name.
                if !policy::is_label_name(name) {
                    unmatched_namespaces.push(name.clone());
                    unchecked_namespaces -= 1;
                    continue;
                }
                let lookup = read::get(&conn.client, namespaces, None, name);
                match tokio::time::timeout_at(until, lookup).await {
                    Err(_) => break,
                    Ok(Err(ClusterError::Api(text))) if text.starts_with("NotFound:") => {
                        unmatched_namespaces.push(name.clone());
                        unchecked_namespaces -= 1;
                    }
                    Ok(Ok(_)) => unchecked_namespaces -= 1,
                    // Not known either way: it stays unchecked.
                    Ok(Err(_)) => {}
                }
            }
        }
        if !unmatched_kinds.is_empty() || !unmatched_namespaces.is_empty() {
            let key = format!("{cluster}\n{unmatched_kinds:?}\n{unmatched_namespaces:?}");
            let first = self
                .inner
                .warned_scopes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key);
            if first {
                tracing::warn!(
                    cluster,
                    ?unmatched_kinds,
                    ?unmatched_namespaces,
                    "the AI scope in settings.toml names something this cluster does not have"
                );
            }
        }
        Some(policy::ScopeCheck {
            unmatched_kinds,
            unmatched_namespaces,
            unchecked_namespaces,
            discovery_incomplete,
            not_run: false,
        })
    }

    /// The connection and the resolved kind. `check` (the names the call carries) runs once the
    /// cluster is known to be exposed and before discovery or the scope, so a call it refuses
    /// sends no request at all. The kind is resolved among those the agent may read: on a
    /// scoped cluster one that does not resolve gives the out-of-scope error, whether it
    /// is out of scope or does not exist, so the two cannot be told apart.
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
        let kinds = self.kinds_in_scope(cluster, &conn).await?;
        let kind = match read::resolve_kind(&kinds, kind, api_version) {
            Ok(kind) => kind.clone(),
            Err(ResolveKindError::NotFound(_)) if !conn.scope.is_everything() => {
                return Err(policy::out_of_scope(cluster).into());
            }
            Err(e) => return Err(kind_error(e)),
        };
        Ok((conn, kind))
    }

    async fn do_list_clusters(&self) -> Result<String, ToolError> {
        let ctx = self.context().await?;
        let exposed: Vec<ContextInfo> = policy::exposed_clusters(&ctx.settings, &ctx.kubeconfig)
            .into_iter()
            .cloned()
            .collect();
        // The clusters this call reads about, for its record.
        let _ = CALL.try_with(|c| {
            c.borrow_mut().probed = exposed
                .iter()
                .map(|info| (info.name.clone(), info.server.clone()))
                .collect();
        });
        let changed = policy::changed_exposures(&ctx.settings, &ctx.kubeconfig);
        let missing = policy::missing_exposures(&ctx.settings, &ctx.kubeconfig);
        let kubeconfig_paths = ctx.kubeconfig.paths.clone();
        let settings = ctx.settings.clone();
        let kubeconfig_errors: Vec<Value> = ctx
            .kubeconfig
            .errors
            .iter()
            .map(|(path, error)| policy::kubeconfig_error_row(path, error))
            .collect();
        let mut rows = Vec::new();
        let results = self.probe_all(ctx, &exposed).await;
        let scopes: BTreeMap<String, policy::AiScope> = exposed
            .iter()
            .map(|info| (info.name.clone(), policy::cluster_scope(&settings, info)))
            .collect();
        for info in &exposed {
            let (probe, unmatched) = match results.get(&info.name) {
                Some((probe, unmatched)) => (Some(probe), unmatched.as_ref()),
                None => (None, None),
            };
            let (reachable, version, error) = match probe {
                Some(Ok(v)) => (true, Some(v.clone()), None),
                Some(Err(e)) => (false, None, Some(e.clone())),
                None => (false, None, None),
            };
            let scope = scopes.get(&info.name);
            // The context's default namespace only when the agent may read it.
            let default_namespace = info
                .namespace
                .as_deref()
                .filter(|ns| scope.is_none_or(|s| s.allows_namespace(ns)));
            let mut row = json!({
                "name": info.name,
                "server": info.server.as_deref().map(flint_core::redact::server_url),
                "default_namespace": default_namespace,
                "reachable": reachable,
                "server_version": version,
                "error": error,
            });
            // What the user limited the agent to, so it asks within it.
            if let Some(scope) = scope.filter(|s| !s.is_everything()) {
                row["scope"] = json!({
                    "namespaces": scope.namespaces,
                    "kinds": scope.kinds,
                });
                let not_run = policy::ScopeCheck::not_run(scope);
                let check = unmatched.unwrap_or(&not_run);
                if !check.unmatched_kinds.is_empty() {
                    row["scope"]["unmatched_kinds"] = json!(check.unmatched_kinds);
                }
                if !check.unmatched_namespaces.is_empty() {
                    row["scope"]["unmatched_namespaces"] = json!(check.unmatched_namespaces);
                }
                if check.unchecked_namespaces > 0 {
                    row["scope"]["unchecked_namespaces"] = json!(check.unchecked_namespaces);
                }
                if let Some(note) = check.incomplete() {
                    row["scope"]["check_incomplete"] = json!(note);
                }
            }
            rows.push(row);
        }
        let mut out = json!({ "clusters": rows });
        if let Some(hint) = policy::list_clusters_hint(
            exposed.len(),
            changed,
            missing,
            &kubeconfig_paths,
            !kubeconfig_errors.is_empty(),
            &policy::Setup::new(self.inner.flavour, &self.inner.config_dir),
        ) {
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
    ) -> BTreeMap<String, Probed> {
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
                        Err(ToolError::new(
                            ClusterError::timeout(PROBE_TIMEOUT, None)
                                .for_target(&target)
                                .to_string(),
                        ))
                    })
                    .map_err(|e| e.message);
                // Apart from the probe, so a slow lookup cannot make the cluster look unreachable.
                let unmatched = match &result {
                    Ok(_) => {
                        match tokio::time::timeout(PROBE_TIMEOUT, this.client(&ctx, &name)).await {
                            Ok(Ok(conn)) => this.unmatched_scope(&name, &conn).await,
                            _ => None,
                        }
                    }
                    Err(_) => None,
                };
                (name, (result, unmatched))
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
        let kinds = self.kinds_in_scope(&p.cluster, &conn).await?;
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
            return Err(ToolError::new(format!("{} cannot be listed", kind.kind())));
        }
        let limit = policy::OBJECTS.clamp(p.limit);
        let namespaces = policy::namespaces_for(
            &conn.scope,
            &p.cluster,
            kind.namespaced,
            p.namespace.as_deref(),
        )?;
        let query = |namespace: Option<String>, limit: u32, continue_token: Option<String>| {
            read::ListQuery {
                namespace,
                label_selector: p.label_selector.clone(),
                field_selector: None,
                limit: Some(limit),
                continue_token,
            }
        };
        let page = match namespaces {
            policy::Namespaces::One(ns) => table::list_table(
                &conn.client,
                &kind,
                &query(Some(ns), limit, p.continue_token.clone()),
            )
            .await
            .map_err(|e| conn.fail(e))?,
            policy::Namespaces::All => table::list_table(
                &conn.client,
                &kind,
                &query(None, limit, p.continue_token.clone()),
            )
            .await
            .map_err(|e| conn.fail(e))?,
            policy::Namespaces::Only(namespaces) => {
                if p.continue_token.is_some() {
                    return Err(ToolError::new(
                        "This cluster's objects are listed namespace by namespace: pass a \
                         namespace from list_clusters' scope to page through it."
                            .to_owned(),
                    ));
                }
                self.list_in_namespaces(&conn, &kind, &namespaces, limit, &query)
                    .await?
            }
        };
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
        // One object: a namespaced kind needs a namespace in scope.
        if kind.namespaced
            && !conn.scope.namespaces.is_empty()
            && !p
                .namespace
                .as_deref()
                .is_some_and(|ns| conn.scope.allows_namespace(ns))
        {
            return Err(policy::out_of_scope(&p.cluster).into());
        }
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
        // After the other header lines, which callers expect first. Read from the redacted object.
        if let Some(line) = policy::managed_by_line(&value) {
            out.push_str(&line);
        }
        out.push_str(text);
        Ok(out)
    }

    async fn do_get_events(&self, p: GetEventsParams) -> Result<String, ToolError> {
        let ctx = self.context().await?;
        let conn = self.client(&ctx, &p.cluster).await?;
        check_namespace(p.namespace.as_deref())?;
        // The object's name and Kind go into a field selector: names only, so neither can add
        // a selector of its own (`a,involvedObject.namespace=…`).
        if let Some(kind) = p
            .object_kind
            .as_deref()
            .filter(|k| !k.chars().all(|c| c.is_ascii_alphanumeric()) || k.is_empty())
        {
            return Err(not_found("kinds", kind));
        }
        if let Some(name) = p
            .object_name
            .as_deref()
            .filter(|n| !policy::is_object_name(n))
        {
            return Err(not_found(
                p.object_kind.as_deref().unwrap_or("objects"),
                name,
            ));
        }
        let limit = policy::EVENTS.clamp(p.limit) as usize;
        let scoped = !conn.scope.is_everything();
        // Events about a kind outside the scope: refused before anything is fetched.
        let kinds = if scoped {
            let (kinds, resolved) = self.resolved_scope(&p.cluster, &conn).await?;
            let scope = resolved.scope;
            if let Some(kind) = p.object_kind.as_deref()
                && !policy::kind_name_in_scope(&scope, &kinds, kind)
            {
                return Err(policy::out_of_scope(&p.cluster).into());
            }
            Some((kinds, scope))
        } else {
            None
        };
        let involved = read::InvolvedObject {
            kind: p.object_kind.clone(),
            name: p.object_name.clone(),
        };
        let fetch = |namespace: Option<String>| {
            let query = read::EventQuery {
                namespace,
                involved: Some(involved.clone()),
                field_selector: None,
                fetch_limit: Some(EVENTS_FETCH),
            };
            let client = conn.client.clone();
            async move { read::events(&client, &query).await }
        };
        let (mut events, more) =
            match policy::namespaces_for(&conn.scope, &p.cluster, true, p.namespace.as_deref())? {
                policy::Namespaces::One(ns) => fetch(Some(ns)).await.map_err(|e| conn.fail(e))?,
                policy::Namespaces::All => fetch(None).await.map_err(|e| conn.fail(e))?,
                policy::Namespaces::Only(namespaces) => {
                    let mut all = Vec::new();
                    let mut more = false;
                    for ns in namespaces {
                        let (events, cut) = fetch(Some(ns)).await.map_err(|e| conn.fail(e))?;
                        all.extend(events);
                        more |= cut;
                    }
                    all.sort_by_key(|e| std::cmp::Reverse(read::event_time(e)));
                    (all, more)
                }
            };
        if let Some((kinds, scope)) = &kinds {
            events.retain(|e| {
                policy::event_in_scope(
                    scope,
                    kinds,
                    e.metadata.namespace.as_deref(),
                    e.involved_object.kind.as_deref(),
                    e.involved_object.api_version.as_deref(),
                )
            });
        }
        // On a scoped cluster, only events in scope count: whether the fetch stopped short
        // could be about hidden ones, so it is not reported.
        let truncated = (more && !scoped) || events.len() > limit;
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
                        redacted += policy::redact_free_text(&mut m);
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
            return Err(ToolError::new(format!(
                "BadRequest: container {c:?} is not valid for pod {:?}",
                p.pod
            )));
        }
        // Logs are a pod's: pods and the pod's namespace must be in scope.
        if !conn.scope.allows_namespace(&p.namespace) {
            return Err(policy::out_of_scope(&p.cluster).into());
        }
        // Discovery only for a kinds scope that does not name `pods` as written (`pod`, `po`).
        if !policy::logs_in_scope(&conn.scope) {
            // A failed discovery cannot show pods are allowed: the same denial as out of scope.
            let allowed = self
                .resolved_scope(&p.cluster, &conn)
                .await
                .is_ok_and(|(_, resolved)| policy::logs_in_scope(&resolved.scope));
            if !allowed {
                return Err(policy::out_of_scope(&p.cluster).into());
            }
        }
        let lines = policy::LOG_LINES.clamp(p.tail_lines);
        // Starts before the requested tail, so redaction sees blocks that open earlier.
        let mut tail = policy::log_fetch_tail(lines);
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
            tail = policy::next_log_tail(tail, lines, complete);
        }
        let more = cut_fetch || text.lines().count() as i64 >= tail;
        // A time window can also start in the middle of a block.
        let older = more || p.since_seconds.is_some_and(|s| s > 0);
        let out = policy::log_output(&text, lines as usize, policy::LOG_BYTES, older);
        let mut result = json!({
            "cluster": p.cluster,
            "namespace": p.namespace,
            "pod": p.pod,
            "container": p.container,
            "lines": out.text.lines().count(),
            "truncated": out.truncated || more,
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

impl FlintMcp {
    /// Lists `kind` in each of `namespaces` in turn, up to `limit` rows in all: a request for
    /// all namespaces on a cluster limited to these. The result carries no continue token
    /// (`more` says rows were left out); the agent pages one namespace at a time.
    async fn list_in_namespaces(
        &self,
        conn: &Conn,
        kind: &KindInfo,
        namespaces: &[String],
        limit: u32,
        query: &impl Fn(Option<String>, u32, Option<String>) -> read::ListQuery,
    ) -> Result<TablePage, ToolError> {
        let mut merged: Option<TablePage> = None;
        for ns in namespaces {
            let taken = merged.as_ref().map_or(0, |m| m.rows.len()) as u32;
            if taken >= limit {
                if let Some(m) = merged.as_mut() {
                    m.more = true;
                }
                break;
            }
            let page = table::list_table(
                &conn.client,
                kind,
                &query(Some(ns.clone()), limit - taken, None),
            )
            .await
            .map_err(|e| conn.fail(e))?;
            match merged.as_mut() {
                None => merged = Some(page),
                Some(m) => merge_page(m, page),
            }
        }
        let mut page = merged.expect("a scope names at least one namespace");
        page.continue_token = None;
        Ok(page)
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

/// Adds the rows of `page` (another namespace's) to `merged`. Each row's cells follow its own
/// page's columns; when the columns or their roles differ (a server that changed its printer
/// columns between the requests), every row falls back to the plain layout (name, namespace,
/// age) rather than mixing cells under the wrong headers. Roles matter: they decide which of a
/// Secret's columns are safe.
fn merge_page(merged: &mut TablePage, page: TablePage) {
    let same = merged.columns.len() == page.columns.len()
        && merged
            .columns
            .iter()
            .zip(page.columns.iter())
            .all(|(a, b)| a.name == b.name && a.role == b.role);
    if !same {
        merged.columns = Arc::from(Vec::new());
    }
    merged.rows.extend(page.rows);
    merged.more |= page.more;
}

/// Column names and rows for list_resources. Each row is `{name, namespace, created, cells}`,
/// with `cells` in column order as `kubectl get` prints them (ages computed from `now`).
/// Wide (priority > 0) columns are marked with `"wide": true`. Rows stop once they add up to
/// `max_bytes` of JSON; the flag says whether any were dropped. Cells pass through
/// [`policy::redact_free_text`] (printer columns can show event and status messages); the last
/// value is how many credentials it replaced.
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
                    if is_secret && !policy::is_safe_secret_column(&page.columns, i) {
                        policy::REDACTED.to_owned()
                    } else {
                        let mut text = text.into_owned();
                        row_redacted += policy::redact_free_text(&mut text);
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

    // covers UP-14: rows listed namespace by namespace share one column layout, or fall back to
    // the plain one when the pages' columns differ.
    #[test]
    fn pages_merge_under_common_columns_only() {
        let page = |columns: Vec<Column>, name: &str| TablePage {
            columns: columns.into(),
            rows: vec![Row {
                uid: name.into(),
                name: name.into(),
                namespace: Some("demo".into()),
                resource_version: "1".into(),
                generation: None,
                created: None,
                labels: Box::new([]),
                cells: Box::new([]),
            }],
            format: ListFormat::Table,
            more: false,
            continue_token: None,
        };
        let a = || {
            vec![
                col("Name", ColumnRole::Name, 0),
                col("Ready", ColumnRole::Value, 0),
            ]
        };
        let mut merged = page(a(), "one");
        merge_page(&mut merged, page(a(), "two"));
        assert_eq!(merged.columns.len(), 2);
        assert_eq!(merged.rows.len(), 2);
        let mut other = page(vec![col("Name", ColumnRole::Name, 0)], "three");
        other.more = true;
        merge_page(&mut merged, other);
        assert!(merged.columns.is_empty(), "the plain layout");
        assert_eq!(merged.rows.len(), 3);
        assert!(merged.more);
        let (columns, rows, _, _) = table_rows(&merged, false, 0, usize::MAX);
        assert!(columns.is_empty());
        assert_eq!(rows[2]["cells"], json!([]));
    }

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
        let missing = kind_error(ResolveKindError::NotFound("sprocket".into())).message;
        assert_eq!(
            missing,
            "no resource kind matches \"sprocket\". Call list_resource_kinds for the kinds."
        );
        let ambiguous = kind_error(ResolveKindError::Ambiguous {
            query: "wd".into(),
            candidates: vec!["gadgets.example.org".into(), "widgets.example.com".into()],
        })
        .message;
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

    // covers UP-10: a column is not safe because of its header alone.
    #[test]
    fn a_repeated_safe_secret_header_never_carries_a_value() {
        let standard = || {
            vec![
                col("Name", ColumnRole::Name, 0),
                col("Namespace", ColumnRole::Namespace, 0),
                col("Type", ColumnRole::Value, 0),
                col("Data", ColumnRole::Value, 0),
                col("Age", ColumnRole::Age, 0),
            ]
        };
        let cells = || {
            vec![
                Cell::Name,
                Cell::Namespace,
                Cell::Text("Opaque".into()),
                Cell::Int(2),
                Cell::Age,
            ]
        };
        let hidden = policy::REDACTED;
        for (header, role, extra, shown) in [
            (
                "Name",
                ColumnRole::Value,
                Cell::Text("hunter2".into()),
                ["web", "demo", "Opaque", "2", "0s"],
            ),
            (
                "Namespace",
                ColumnRole::Value,
                Cell::Text("hunter2".into()),
                ["web", "demo", "Opaque", "2", "0s"],
            ),
            (
                "Type",
                ColumnRole::Value,
                Cell::Text("hunter2".into()),
                ["web", "demo", hidden, "2", "0s"],
            ),
            (
                "Data",
                ColumnRole::Value,
                Cell::Text("hunter2".into()),
                ["web", "demo", "Opaque", hidden, "0s"],
            ),
            (
                "Age",
                ColumnRole::Age,
                Cell::Age,
                ["web", "demo", "Opaque", "2", "0s"],
            ),
        ] {
            let mut columns = standard();
            columns.push(col(header, role, 0));
            let mut row = cells();
            row.push(extra);
            let (_, rows, _, _) = table_rows(&page(columns, row), true, 1_000, policy::LIST_BYTES);
            let mut expected = shown.to_vec();
            // A second Age is computed from the object's metadata, like the first.
            expected.push(if header == "Age" { "0s" } else { hidden });
            assert_eq!(rows[0]["cells"], json!(expected), "{header}");
        }
    }

    // covers UP-14: pages with the same headers but other roles do not share a layout, so a
    // printer column is never taken for the safe column of another page.
    #[test]
    fn secret_pages_with_different_column_roles_do_not_merge() {
        let mut merged = page(
            vec![
                col("Name", ColumnRole::Name, 0),
                col("Namespace", ColumnRole::Namespace, 0),
            ],
            vec![Cell::Name, Cell::Namespace],
        );
        merge_page(
            &mut merged,
            page(
                vec![
                    col("Name", ColumnRole::Name, 0),
                    col("Namespace", ColumnRole::Value, 0),
                ],
                vec![Cell::Name, Cell::Text("hunter2".into())],
            ),
        );
        assert!(merged.columns.is_empty(), "the plain layout");
        let (_, rows, _, _) = table_rows(&merged, true, 1_000, policy::LIST_BYTES);
        let text = serde_json::to_string(&rows).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
    }

    fn arguments(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    // covers UP-10
    #[test]
    fn every_tool_schema_refuses_other_arguments() {
        let server = FlintMcp::new(PathBuf::from("/nonexistent"), Vec::new(), None);
        let tools = server.tool_router.list_all();
        assert_eq!(tools.len(), 6);
        for tool in tools {
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                Some(&json!(false)),
                "{}",
                tool.name
            );
            assert!(
                tool.input_schema
                    .get("properties")
                    .is_some_and(Value::is_object),
                "{}",
                tool.name
            );
        }
    }

    // covers UP-10
    #[test]
    fn a_refusal_names_the_unsupported_and_the_accepted_arguments() {
        let server = FlintMcp::new(PathBuf::from("/nonexistent"), Vec::new(), None);
        let list = server.tool_router.get("list_resources").unwrap();
        let known = arguments(json!({"cluster": "c", "kind": "pods", "continue": "t"}));
        assert_eq!(unsupported_arguments(list, Some(&known)), None);
        assert_eq!(unsupported_arguments(list, None), None);

        let text = unsupported_arguments(
            list,
            Some(&arguments(json!({"cluster": "c", "field_selector": "x"}))),
        )
        .unwrap();
        assert!(text.contains("\"field_selector\""), "{text}");
        assert!(
            text.contains("label_selector") && text.contains("continue"),
            "{text}"
        );

        let clusters = server.tool_router.get("list_clusters").unwrap();
        let text = unsupported_arguments(clusters, Some(&arguments(json!({"a": 1})))).unwrap();
        assert_eq!(
            text,
            "list_clusters takes no arguments. Unsupported: \"a\"."
        );
    }

    // covers UP-10: argument names come from the client.
    #[test]
    fn a_refusal_bounds_and_redacts_what_the_client_sent() {
        let server = FlintMcp::new(PathBuf::from("/nonexistent"), Vec::new(), None);
        let tool = server.tool_router.get("list_clusters").unwrap();
        let mut many = serde_json::Map::new();
        for i in 0..8 {
            many.insert(format!("{i}{}", "x".repeat(500)), json!(1));
        }
        let text = unsupported_arguments(tool, Some(&many)).unwrap();
        assert!(text.len() < 500, "{text}");
        assert!(text.ends_with(" and more."), "{text}");
        many.remove(many.keys().next().unwrap().clone().as_str());
        many.remove(many.keys().next().unwrap().clone().as_str());
        many.remove(many.keys().next().unwrap().clone().as_str());
        let five = unsupported_arguments(tool, Some(&many)).unwrap();
        assert!(!five.contains("and more"), "{five}");

        let control = arguments(json!({"a\nb\u{1b}[2J\u{7f}\u{202e}c": 1}));
        let text = unsupported_arguments(tool, Some(&control)).unwrap();
        assert!(
            !text.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "{text:?}"
        );

        // A value with control characters is not kept for the activity log.
        let named = named_in(Some(&arguments(
            json!({"cluster": "a\u{1b}[2Jb", "kind": "pods"}),
        )));
        assert_eq!(named.cluster, None);
        assert_eq!(named.kind.as_deref(), Some("pods"));

        // A name past the limit is not read or echoed, not even its start.
        let huge = format!("hunter2{}", "x".repeat(UNSUPPORTED_NAME_BYTES));
        let text = unsupported_arguments(tool, Some(&arguments(json!({ huge: 1 })))).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("too long"), "{text}");

        // A password whose closing `@` lies past the cut is still recognized.
        let url = format!("https://user:{}@example.com", "p".repeat(100));
        let text = unsupported_arguments(tool, Some(&arguments(json!({ url: 1 })))).unwrap();
        assert!(!text.contains("ppp"), "{text}");

        // A rule that takes the rest of the line leaves the accepted arguments in place.
        let list = server.tool_router.get("list_resources").unwrap();
        let header = "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345";
        let text = unsupported_arguments(list, Some(&arguments(json!({ header: 1 })))).unwrap();
        assert!(!text.contains("abcdefghijkl"), "{text}");
        assert!(text.contains("Accepted arguments:"), "{text}");
        assert!(text.contains("label_selector"), "{text}");

        let token = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let text = unsupported_arguments(tool, Some(&arguments(json!({ token: 1 })))).unwrap();
        assert!(!text.contains(token), "{text}");
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

    // covers UP-15
    #[test]
    fn row_cells_holding_a_serialized_secret_are_masked() {
        let manifest = "apiVersion: v1\nkind:\n  Secret\ndata:\n  k: placeholder-value\n";
        let page = page(
            vec![
                col("Name", ColumnRole::Name, 0),
                col("Source", ColumnRole::Value, 0),
            ],
            vec![Cell::Name, Cell::Text(manifest.into())],
        );
        let (_, rows, _, _) = table_rows(&page, false, 1_000, policy::LIST_BYTES);
        assert_eq!(rows[0]["cells"], json!(["web", policy::REDACTED]));
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
    // covers UP-16: the activity log's item count and cut-short flag come from each reply's
    // own fields, as the agent read them.
    #[test]
    fn reply_size_reads_what_the_agent_saw() {
        let list = json!({"count": 3, "truncated": true, "items": []}).to_string();
        assert_eq!(reply_size("list_resources", &list), (Some(3), true));
        let logs = json!({"lines": 120, "truncated": false, "logs": "a"}).to_string();
        assert_eq!(reply_size("get_pod_logs", &logs), (Some(120), false));
        let clusters = json!({"clusters": [{}, {}]}).to_string();
        assert_eq!(reply_size("list_clusters", &clusters), (Some(2), false));
        let cut = "# redacted=0\n# truncated=true: output cut at 262144 bytes\nkind: Pod\n";
        assert_eq!(reply_size("get_resource", cut), (Some(1), true));
        let marked = "# truncated=true: cut\n# managed_by=Argo CD: shop\nkind: Pod\n";
        assert_eq!(reply_size("get_resource", marked), (Some(1), true));
        let managed = "# managed_by=Argo CD: shop\nkind: Pod\n";
        assert_eq!(reply_size("get_resource", managed), (Some(1), false));
        let whole = "kind: Pod\nmetadata:\n  name: '# truncated=true'\n";
        assert_eq!(reply_size("get_resource", whole), (Some(1), false));
        assert_eq!(reply_size("list_resources", "not json"), (None, false));
    }

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

    /// Whether `pid` runs: it exists and is not a zombie. The plugin's own child is reaped by
    /// whoever adopted it, which in a container may be no one.
    fn live(pid: &str) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
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
            // The plugin is Flint's child, so it is reaped too; its own child need only be dead.
            let gone = [(&leader, exists as fn(&str) -> bool), (&child, live)];
            for (pid, running) in gone {
                for _ in 0..500 {
                    if !running(pid) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert!(!running(pid), "plugin process {pid} outlived its calls");
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

        let Err(ToolError {
            message: timeout, ..
        }) = server.do_list_kinds(params("silent")).await
        else {
            panic!("a silent server has no kinds");
        };
        assert_eq!(timeout, format!("timed out after 10s reaching {silent}"));

        let Err(ToolError {
            message: missing, ..
        }) = server.do_list_kinds(params("noplugin")).await
        else {
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

#[cfg(test)]
#[path = "server_limits_tests.rs"]
mod limits_tests;
