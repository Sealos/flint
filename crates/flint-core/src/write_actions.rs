//! The bodies of the resource actions (UP-53): delete, scale, rollout restart, cordon, uncordon
//! and drain.
//!
//! Every request first asks the write gate ([`ActionGate::client`]) and never keeps the client,
//! so a drain stops at its next request once its cluster is locked. Each action names the object
//! the user reviewed by its uid (a `resourceVersion` would conflict with every status update of
//! a live workload or node): a conflict comes back as [`ActionError::Conflict`], for the review
//! to open again. Before an action runs, the live object is read again: another uid or GitOps
//! manager than the review saw is a conflict too. A fleet action also holds its write to the
//! `resourceVersion` of that read ([`Held`]), which a single-cluster action does not, for the
//! reason above. Nothing here retries.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, EvictParams, ListParams, Preconditions,
};
use kube::client::Body;
use kube::core::Status;
use kube::{Client, Resource, ResourceExt as _};
use serde_json::{Value, json};

use crate::action_kinds::{self, ScalePaths};
use crate::api_path::{self, PathError};
use crate::cluster::ClusterError;
use crate::gitops::{self, Identity, ManagedBy};
use crate::protection::WriteRefusal;
use crate::read::{KindId, KindInfo};
use crate::redact;
use crate::write::{
    RequestGate, ResourceChange, WriteError, WriteOutcome, WriteProposal, WriteTarget, read_live,
};

/// How long one request of an action may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Who changed the object, as the API server records it.
const FIELD_MANAGER: &str = "flint";

/// The annotation `kubectl rollout restart` sets on a workload's pod template.
const RESTARTED_AT: &str = "kubectl.kubernetes.io/restartedAt";

/// The annotation the kubelet puts on a static pod's mirror, which the API cannot evict.
const MIRROR_POD: &str = "kubernetes.io/config.mirror";

/// The write gate, asked before each request of one action.
pub(crate) trait ActionGate: Send {
    /// A client for the next request, if the gate allows it now. Never keep it.
    fn client(&mut self) -> impl Future<Output = Result<Client, ActionError>> + Send;
}

impl ActionGate for RequestGate {
    async fn client(&mut self) -> Result<Client, ActionError> {
        RequestGate::client(self)
            .await
            .map_err(|error| match error {
                WriteError::Refused(refusal) => ActionError::Refused(refusal),
                WriteError::ConnectionChanged => ActionError::ConnectionChanged,
                WriteError::KubeconfigUnreadable => ActionError::KubeconfigUnreadable,
                refused @ (WriteError::CredentialChanged
                | WriteError::CredentialUnreadable
                | WriteError::IdentityUnverified
                | WriteError::IdentityUnavailable) => ActionError::Credential(refused),
                other => ActionError::Failed(other.to_string()),
            })
    }
}

/// `request`, bounded by [`REQUEST_TIMEOUT`].
async fn timed<T>(request: impl Future<Output = Result<T, kube::Error>>) -> Result<T, ActionError> {
    match tokio::time::timeout(REQUEST_TIMEOUT, request).await {
        Ok(result) => result.map_err(ActionError::of),
        Err(_) => Err(ActionError::Failed(timed_out())),
    }
}

fn timed_out() -> String {
    format!(
        "the API server did not answer within {} s; the change may or may not have applied",
        REQUEST_TIMEOUT.as_secs()
    )
}

fn is_restartable(kind: &ApiResource) -> bool {
    action_kinds::restarts(&KindId::new(&kind.group, &kind.plural))
}

fn is_node(kind: &ApiResource) -> bool {
    kind.group.is_empty() && kind.plural == "nodes"
}

/// What `proposal` does to the live object, if its kind has that action.
pub(crate) fn review(
    kind: &KindInfo,
    live: &Value,
    proposal: WriteProposal,
) -> Result<ResourceChange, WriteError> {
    let resource = &kind.resource;
    let refuse = |action: &str| {
        Err(WriteError::Rejected(format!(
            "{} cannot be {action}",
            resource.plural
        )))
    };
    match proposal {
        WriteProposal::Delete => Ok(ResourceChange::Delete),
        WriteProposal::Scale { to } => {
            let Some(paths) = kind.scale_paths() else {
                return refuse("scaled");
            };
            // Where its `scale` subresource reads it: what the write tests.
            let from = paths
                .replicas(live)
                .ok_or_else(|| WriteError::Failed("the object has no replica count".to_owned()))?;
            Ok(ResourceChange::Scale { from, to })
        }
        WriteProposal::RolloutRestart if !is_restartable(resource) => refuse("restarted"),
        // It would be accepted, and no pod would restart.
        WriteProposal::RolloutRestart
            if live.pointer("/spec/paused").and_then(Value::as_bool) == Some(true) =>
        {
            Err(WriteError::Rejected(
                "this Deployment is paused: resume it before restarting it".to_owned(),
            ))
        }
        WriteProposal::RolloutRestart => Ok(ResourceChange::RolloutRestart),
        WriteProposal::Cordon | WriteProposal::Uncordon | WriteProposal::Drain { .. }
            if !is_node(resource) =>
        {
            refuse("cordoned or drained")
        }
        WriteProposal::Cordon => Ok(ResourceChange::Cordon),
        WriteProposal::Uncordon => Ok(ResourceChange::Uncordon),
        WriteProposal::Drain {
            delete_emptydir_data,
            force,
        } => Ok(ResourceChange::Drain {
            delete_emptydir_data,
            force,
        }),
        _ => refuse("changed this way"),
    }
}

/// What a review saw, which the live objects must still say when the action runs.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reviewed {
    pub(crate) managed: Option<ManagedBy>,
    /// For a drain: the pods it would evict that a GitOps controller manages.
    pub(crate) pods: Vec<ManagedPod>,
    /// The object's `spec` as reviewed.
    pub(crate) spec: ReviewedSpec,
    /// Whether another `spec` than [`Self::spec`] makes this review stale.
    pub(crate) hold_spec: bool,
    /// For a scale: where the count was read, which its definition must still say.
    pub(crate) scale: Option<ScalePaths>,
    /// For a restart: the pod template's `restartedAt` as reviewed (`Some(None)`: none). Another
    /// one means a restart ran since, and this one would be a second.
    pub(crate) restarted_at: Option<Option<String>>,
}

/// The `restartedAt` annotation of a workload's pod template.
pub(crate) fn restarted_at(live: &Value) -> Option<String> {
    live.pointer("/spec/template/metadata/annotations")
        .and_then(|a| a.get(RESTARTED_AT))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The replica paths a scale of `kind` may use now: what discovery says, and for a custom
/// resource what its definition says when read again. Another definition is a conflict, and one
/// that cannot be read refuses the scale.
async fn current_scale_paths(
    gate: &mut impl ActionGate,
    kind: &KindInfo,
) -> Result<ScalePaths, WriteError> {
    let refuse = || WriteError::Rejected(format!("{} cannot be scaled", kind.plural()));
    let known = kind.scale_paths().ok_or_else(refuse)?;
    if action_kinds::scales_built_in(&kind.id()) {
        return Ok(known);
    }
    let client = gate.client().await?;
    match action_kinds::definition_scale_paths(&client, &kind.resource, REQUEST_TIMEOUT).await {
        Some(read) if read == known => Ok(read),
        Some(_) => Err(WriteError::Conflict),
        None => Err(WriteError::Failed(format!(
            "the definition of {} could not be read to confirm where it keeps its replica count",
            kind.plural()
        ))),
    }
}

/// A live object's `spec`. Never printed: it can hold environment values.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct ReviewedSpec(Option<Value>);

impl ReviewedSpec {
    fn of(live: &Value) -> Self {
        Self(live.get("spec").cloned())
    }
}

impl std::fmt::Debug for ReviewedSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReviewedSpec(..)")
    }
}

/// What the review of `change` to the live object `live` of `kind` must remember. A scale's
/// paths are read from the definition again, so the count reviewed is at the path the
/// `scale` subresource uses.
pub(crate) async fn reviewed(
    gate: &mut impl ActionGate,
    kind: &KindInfo,
    name: &str,
    live: &Value,
    change: &ResourceChange,
) -> Result<Reviewed, WriteError> {
    let pods = match drain_options(change) {
        Some(options) => managed_pods(gate, name, &options).await?,
        None => Vec::new(),
    };
    let scale = match change {
        ResourceChange::Scale { .. } => Some(current_scale_paths(gate, kind).await?),
        _ => None,
    };
    Ok(Reviewed {
        managed: gitops::managed_by_live_object(live),
        pods,
        spec: ReviewedSpec::of(live),
        hold_spec: false,
        scale,
        restarted_at: (*change == ResourceChange::RolloutRestart).then(|| restarted_at(live)),
    })
}

/// The options of a drain, if `change` is one.
fn drain_options(change: &ResourceChange) -> Option<DrainOptions> {
    match change {
        ResourceChange::Drain {
            delete_emptydir_data,
            force,
        } => Some(DrainOptions {
            delete_emptydir_data: *delete_emptydir_data,
            force: *force,
            ..Default::default()
        }),
        _ => None,
    }
}

/// What a held write must still find: the version of the last read, and the pod template's
/// metadata its patch builds on. Not `Debug`: annotations can hold anything.
struct Held {
    resource_version: String,
    template_metadata: Option<Value>,
}

impl Held {
    fn of(live: &Value) -> Result<Self, WriteError> {
        let resource_version = live
            .pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
            .ok_or(WriteError::Conflict)?
            .to_owned();
        Ok(Self {
            resource_version,
            template_metadata: live.pointer("/spec/template/metadata").cloned(),
        })
    }

    fn test(&self) -> Value {
        json!({
            "op": "test",
            "path": "/metadata/resourceVersion",
            "value": self.resource_version,
        })
    }
}

/// How one action's PATCH is sent.
#[derive(Clone, Copy, Default)]
struct PatchMode<'a> {
    dry_run: bool,
    held: Option<&'a Held>,
}

/// Reads the live object once. Another uid or GitOps manager than the review saw, or a Deployment
/// paused for a restart, is a conflict, and a failed read refuses the action. Returns what a
/// review holding the object requires of the write that follows.
async fn recheck(
    gate: &mut impl ActionGate,
    kind: &KindInfo,
    target: &WriteTarget,
    change: &ResourceChange,
    reviewed: &Reviewed,
) -> Result<Option<Held>, WriteError> {
    let client = gate.client().await?;
    let live = read_live(&client, kind, target).await?;
    let uid = live.pointer("/metadata/uid").and_then(Value::as_str);
    let paused = live.pointer("/spec/paused").and_then(Value::as_bool) == Some(true);
    if uid != target.uid.as_deref()
        || gitops::managed_by_live_object(&live) != reviewed.managed
        || (paused && *change == ResourceChange::RolloutRestart)
        || (reviewed.hold_spec && ReviewedSpec::of(&live) != reviewed.spec)
        || reviewed
            .restarted_at
            .as_ref()
            .is_some_and(|at| *at != restarted_at(&live))
    {
        return Err(WriteError::Conflict);
    }
    if let ResourceChange::Scale { .. } = change {
        // Kubernetes scales at the path its definition names when the write arrives.
        if Some(current_scale_paths(gate, kind).await?) != reviewed.scale {
            return Err(WriteError::Conflict);
        }
    }
    reviewed.hold_spec.then(|| Held::of(&live)).transpose()
}

/// The object `target` names, by the uid its review read.
fn action_object(kind: &KindInfo, target: &WriteTarget) -> Result<ActionObject, WriteError> {
    Ok(ActionObject {
        resource: kind.resource.clone(),
        namespace: target.namespace.clone().filter(|_| kind.namespaced),
        name: target.name.clone(),
        uid: target
            .uid
            .clone()
            .ok_or_else(|| WriteError::Failed("the object has no uid".to_owned()))?,
    })
}

/// Asks the server to validate a reviewed action without storing it, once the live object still
/// matches its review. Only the actions with a server dry run here are accepted.
pub(crate) async fn dry_run(
    gate: &mut impl ActionGate,
    kind: &KindInfo,
    target: &WriteTarget,
    change: &ResourceChange,
    reviewed: &Reviewed,
) -> Result<(), WriteError> {
    let object = action_object(kind, target)?;
    let held = recheck(gate, kind, target, change, reviewed).await?;
    let mode = PatchMode {
        dry_run: true,
        held: held.as_ref(),
    };
    match change {
        ResourceChange::Scale { from, to } => {
            send_scale(gate, kind, &object, *from, *to, mode).await?;
        }
        ResourceChange::RolloutRestart => send_restart(gate, &object, mode).await?,
        _ => {
            return Err(WriteError::Rejected(
                "this action has no server dry run".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Runs a reviewed action on `target`, once the live object still matches its review.
pub(crate) async fn run(
    gate: &mut impl ActionGate,
    kind: &KindInfo,
    target: &WriteTarget,
    change: &ResourceChange,
    reviewed: &Reviewed,
) -> Result<WriteOutcome, WriteError> {
    let object = action_object(kind, target)?;
    let done = |drain| WriteOutcome {
        resource_version: None,
        drain,
    };
    let held = recheck(gate, kind, target, change, reviewed).await?;
    let mode = PatchMode {
        dry_run: false,
        held: held.as_ref(),
    };
    match change {
        ResourceChange::Delete => delete(gate, &object).await?,
        ResourceChange::Scale { from, to } => {
            send_scale(gate, kind, &object, *from, *to, mode).await?;
        }
        ResourceChange::RolloutRestart => send_restart(gate, &object, mode).await?,
        ResourceChange::Cordon => set_cordoned(gate, &object, true).await?,
        ResourceChange::Uncordon => set_cordoned(gate, &object, false).await?,
        ResourceChange::Drain {
            delete_emptydir_data,
            force,
        } => {
            let options = DrainOptions {
                delete_emptydir_data: *delete_emptydir_data,
                force: *force,
                warned: reviewed
                    .pods
                    .iter()
                    .map(|pod| (pod.uid.clone(), pod.managed.clone()))
                    .collect(),
                ..Default::default()
            };
            let now = managed_pods(gate, &object.name, &options).await?;
            if now
                .iter()
                .any(|pod| options.warned.get(&pod.uid) != Some(&pod.managed))
            {
                return Err(WriteError::Conflict);
            }
            let mut report = drain(gate, &object, &options).await;
            if !report.cordoned
                && let Some(error) = report.stopped.take()
            {
                return Err(error);
            }
            return Ok(done(Some(report)));
        }
    }
    Ok(done(None))
}

impl From<ActionError> for WriteError {
    fn from(error: ActionError) -> Self {
        match error {
            ActionError::Refused(refusal) => WriteError::Refused(refusal),
            ActionError::ConnectionChanged => WriteError::ConnectionChanged,
            ActionError::KubeconfigUnreadable => WriteError::KubeconfigUnreadable,
            ActionError::Credential(refused) => refused,
            ActionError::Conflict(_) => WriteError::Conflict,
            ActionError::Gone(_) => WriteError::Gone,
            ActionError::Unsupported(message) => WriteError::Rejected(message),
            ActionError::Failed(message) => WriteError::Failed(message),
        }
    }
}

/// The object an action was reviewed for.
#[derive(Debug, Clone)]
pub(crate) struct ActionObject {
    pub(crate) resource: ApiResource,
    pub(crate) namespace: Option<String>,
    pub(crate) name: String,
    pub(crate) uid: String,
}

impl ActionObject {
    fn api(&self, client: Client) -> Result<Api<DynamicObject>, ActionError> {
        self.check_path_segments()?;
        Ok(match &self.namespace {
            Some(namespace) => Api::namespaced_with(client, namespace, &self.resource),
            None => Api::all_with(client, &self.resource),
        })
    }

    fn check_path_segments(&self) -> Result<(), PathError> {
        api_path::validate_resource_path(
            &self.resource,
            self.namespace.as_deref(),
            Some(&self.name),
        )
    }

    /// The URL of a PATCH of this object, or of its `subresource`.
    fn patch_url(&self, subresource: Option<&str>, dry_run: bool) -> Result<String, ActionError> {
        self.check_path_segments()?;
        if let Some(subresource) = subresource {
            api_path::validate_resource_plural(subresource)?;
        }
        let collection = DynamicObject::url_path(&self.resource, self.namespace.as_deref());
        let mut url = format!("{collection}/{}", self.name);
        if let Some(subresource) = subresource {
            url.push('/');
            url.push_str(subresource);
        }
        url.push_str("?fieldManager=");
        url.push_str(FIELD_MANAGER);
        if dry_run {
            url.push_str("&dryRun=All");
        }
        Ok(url)
    }
}

/// A PATCH body, by the content type it is sent as.
enum PatchBody {
    Merge(Value),
    Json(Value),
}

/// Sends one PATCH through the gate, with the body `body` makes once the gate has answered. Only
/// a 2xx carrying the patched object is a success. A JSON Patch's failed test is a conflict.
async fn send_patch(
    gate: &mut impl ActionGate,
    object: &ActionObject,
    subresource: Option<&str>,
    body: impl FnOnce() -> PatchBody + Send,
    dry_run: bool,
) -> Result<(), ActionError> {
    let url = object.patch_url(subresource, dry_run)?;
    let client = gate.client().await?;
    let (content_type, body, tested) = match body() {
        PatchBody::Merge(body) => ("application/merge-patch+json", body, false),
        PatchBody::Json(body) => ("application/json-patch+json", body, true),
    };
    let request = http::Request::patch(url)
        .header(http::header::CONTENT_TYPE, content_type)
        .header(http::header::ACCEPT, "application/json")
        .body(Body::from(body.to_string().into_bytes()))
        .map_err(|e| ActionError::Failed(e.to_string()))?;
    let answer = async {
        let response = client.send(request).await?;
        let status = response.status();
        let bytes = response.into_body().collect_bytes().await?;
        Ok::<_, kube::Error>((status, bytes))
    };
    let Ok(answer) = tokio::time::timeout(REQUEST_TIMEOUT, answer).await else {
        return Err(ActionError::Failed(timed_out()));
    };
    let (status, bytes) = answer.map_err(ActionError::of)?;
    if status.is_success() && is_patched_object(&bytes, object, subresource) {
        return Ok(());
    }
    if !status.is_client_error() && !status.is_server_error() {
        // The body is not shown: it is not the API server's.
        return Err(ActionError::Failed(format!(
            "the answer ({}) was not from the API server; the change may or may not have applied",
            status.as_u16()
        )));
    }
    let error = kube::Error::Api(api_status(status, &bytes).boxed());
    if tested && is_failed_patch_test(&error) {
        return Err(ActionError::Conflict(
            "the object changed since it was reviewed".to_owned(),
        ));
    }
    Err(ActionError::of(error))
}

/// Whether a PATCH's answer is the object it was sent for, or that object's `subresource`.
fn is_patched_object(bytes: &[u8], object: &ActionObject, subresource: Option<&str>) -> bool {
    let Ok(answer) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    let kind = match subresource {
        Some("scale") => "Scale",
        _ => object.resource.kind.as_str(),
    };
    answer.get("kind").and_then(Value::as_str) == Some(kind)
        && answer.pointer("/metadata/uid").and_then(Value::as_str) == Some(object.uid.as_str())
}

/// An error answer as a `Status`, which a proxy's page is not.
fn api_status(code: http::StatusCode, bytes: &[u8]) -> Status {
    serde_json::from_slice(bytes).unwrap_or_else(|_| {
        let text = String::from_utf8_lossy(bytes);
        Status::failure(text.trim(), "Failed to parse error data").with_code(code.as_u16())
    })
}

/// Why an action did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionError {
    /// The write gate refused it.
    Refused(WriteRefusal),
    /// The connection definition is not the reviewed one.
    ConnectionChanged,
    /// A kubeconfig file that could define the context can't be read.
    KubeconfigUnreadable,
    /// The credential is not the reviewed one, or could not be confirmed.
    Credential(WriteError),
    /// The object changed since it was reviewed: review it again.
    Conflict(String),
    /// The object is gone.
    Gone(String),
    /// This kind has no such action.
    Unsupported(String),
    Failed(String),
}

impl ActionError {
    fn of(error: kube::Error) -> Self {
        let code = api_code(&error);
        let replaced = names_another_uid(&error);
        let message = error_text(error);
        match code {
            Some(409) => Self::Conflict(message),
            Some(422) if replaced => Self::Conflict(message),
            Some(404) => Self::Gone(message),
            _ => Self::Failed(message),
        }
    }
}

impl From<PathError> for ActionError {
    fn from(error: PathError) -> Self {
        Self::Unsupported(error.to_string())
    }
}

/// What may be shown of `error`: text from the server goes through
/// [`redact::safe_server_error`] whole, before anything shortens it.
fn error_text(error: kube::Error) -> String {
    match ClusterError::kube(error) {
        ClusterError::Api(text) | ClusterError::Kube(text) => redact::safe_server_error(&text),
        other => other.summary().to_string(),
    }
}

fn api_code(error: &kube::Error) -> Option<u16> {
    match error {
        kube::Error::Api(status) => Some(status.code),
        _ => None,
    }
}

/// Whether a patch was refused for naming another uid than the object's: the API server answers
/// a patch's `metadata.uid` that way on most resources, and with a conflict on `scale`.
fn names_another_uid(error: &kube::Error) -> bool {
    let kube::Error::Api(status) = error else {
        return false;
    };
    status
        .details
        .as_ref()
        .is_some_and(|d| d.causes.iter().any(|c| c.field == "metadata.uid"))
}

fn uid_precondition(uid: &str) -> Preconditions {
    Preconditions {
        uid: Some(uid.to_owned()),
        resource_version: None,
    }
}

/// Deletes `object`, if it is still the one with its uid.
pub(crate) async fn delete(
    gate: &mut impl ActionGate,
    object: &ActionObject,
) -> Result<(), ActionError> {
    // Refuse an unsafe name before the gate runs any credential check.
    object.check_path_segments()?;
    let client = gate.client().await?;
    let params = DeleteParams {
        preconditions: Some(uid_precondition(&object.uid)),
        ..Default::default()
    };
    timed(object.api(client)?.delete(&object.name, &params))
        .await
        .map(drop)
}

/// Changes a workload's replicas from `from`, the count reviewed, to `to`. A JSON Patch tests the
/// uid and the count, so another change to either is a conflict and a status update is not. It
/// goes through the `scale` subresource, which is what a scale-only role may patch, and which
/// shows a custom resource's own replica path as `spec.replicas`. That subresource omits
/// `replicas` at zero, so a count of zero is tested as an empty `spec`. A `Scale` carries its
/// workload's `resourceVersion`, so a held scale tests that too.
async fn send_scale(
    gate: &mut impl ActionGate,
    kind: &KindInfo,
    object: &ActionObject,
    from: u32,
    to: u32,
    mode: PatchMode<'_>,
) -> Result<(), ActionError> {
    if kind.scale_paths().is_none() || kind.resource != object.resource {
        return Err(ActionError::Unsupported(format!(
            "{} cannot be scaled",
            object.resource.plural
        )));
    }
    let mut operations =
        vec![json!({ "op": "test", "path": "/metadata/uid", "value": object.uid })];
    operations.extend(mode.held.map(Held::test));
    if from == 0 {
        operations.push(json!({ "op": "test", "path": "/spec", "value": {} }));
        operations.push(json!({ "op": "add", "path": "/spec/replicas", "value": to }));
    } else {
        operations.push(json!({ "op": "test", "path": "/spec/replicas", "value": from }));
        operations.push(json!({ "op": "replace", "path": "/spec/replicas", "value": to }));
    }
    let body = move || PatchBody::Json(Value::Array(operations));
    send_patch(gate, object, Some("scale"), body, mode.dry_run).await
}

/// The message of the API server's generic 422, which a failed JSON Patch test gets.
const GENERIC_INVALID: &str = "the server rejected our request due to an error in our request";

/// Whether a JSON Patch failed one of its tests: the API server answers that as Invalid with no
/// field named, where a real validation error names its field.
fn is_failed_patch_test(error: &kube::Error) -> bool {
    let kube::Error::Api(status) = error else {
        return false;
    };
    status.code == 422
        && status.message == GENERIC_INVALID
        && status.details.as_ref().is_none_or(|d| d.causes.is_empty())
}

/// Restarts a Deployment's, DaemonSet's or StatefulSet's pods, as `kubectl rollout restart`. A
/// held restart is a JSON Patch, whose test of the version applies to the object it changes.
async fn send_restart(
    gate: &mut impl ActionGate,
    object: &ActionObject,
    mode: PatchMode<'_>,
) -> Result<(), ActionError> {
    let resource = &object.resource;
    if !is_restartable(resource) {
        return Err(ActionError::Unsupported(format!(
            "{} cannot be restarted",
            resource.plural
        )));
    }
    let body = || {
        let now = jiff::Timestamp::now().to_string();
        match mode.held {
            Some(held) => PatchBody::Json(held_restart(&object.uid, held, &now)),
            None => PatchBody::Merge(json!({
                "metadata": { "uid": object.uid },
                "spec": { "template": { "metadata": { "annotations": { RESTARTED_AT: now } } } },
            })),
        }
    };
    send_patch(gate, object, None, body, mode.dry_run).await
}

/// The JSON Patch of a held restart. The version test makes the annotations it writes back the
/// ones it read.
fn held_restart(uid: &str, held: &Held, now: &str) -> Value {
    let (path, value) = match &held.template_metadata {
        Some(metadata) => {
            let mut annotations = metadata
                .get("annotations")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            annotations.insert(RESTARTED_AT.to_owned(), now.into());
            (
                "/spec/template/metadata/annotations",
                Value::Object(annotations),
            )
        }
        None => (
            "/spec/template/metadata",
            json!({ "annotations": { RESTARTED_AT: now } }),
        ),
    };
    json!([
        { "op": "test", "path": "/metadata/uid", "value": uid },
        held.test(),
        { "op": "add", "path": path, "value": value },
    ])
}

/// Marks a node unschedulable, or schedulable again.
pub(crate) async fn set_cordoned(
    gate: &mut impl ActionGate,
    node: &ActionObject,
    cordoned: bool,
) -> Result<(), ActionError> {
    let body = || {
        PatchBody::Merge(json!({
            "metadata": { "uid": node.uid },
            "spec": { "unschedulable": cordoned },
        }))
    };
    send_patch(gate, node, None, body, false).await
}

/// What a drain may evict beyond what `kubectl drain` does by default, and how long it waits.
#[derive(Debug, Clone)]
pub(crate) struct DrainOptions {
    /// Evict pods with `emptyDir` volumes, whose data is lost.
    pub(crate) delete_emptydir_data: bool,
    /// Evict pods no controller will recreate.
    pub(crate) force: bool,
    /// The GitOps managers the review warned about, by pod uid.
    pub(crate) warned: BTreeMap<String, ManagedBy>,
    /// How long to wait for evicted pods to terminate.
    pub(crate) wait: Duration,
    pub(crate) poll: Duration,
}

impl Default for DrainOptions {
    fn default() -> Self {
        Self {
            delete_emptydir_data: false,
            force: false,
            warned: BTreeMap::new(),
            wait: Duration::from_secs(60),
            poll: Duration::from_secs(1),
        }
    }
}

/// A pod, by namespace and name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PodName {
    pub namespace: String,
    pub name: String,
}

/// A pod a drain would evict, and the GitOps controller that manages it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ManagedPod {
    pub pod: PodName,
    pub managed: ManagedBy,
    pub(crate) uid: String,
}

/// Why a drain left a pod on the node.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockedReason {
    /// A PodDisruptionBudget does not allow it now.
    DisruptionBudget,
    /// No controller would recreate it.
    Unmanaged,
    /// It has `emptyDir` data that would be lost.
    LocalStorage,
    /// It was replaced since it was listed.
    Changed,
    /// A GitOps controller's marker on it is not the one reviewed.
    ManagedSinceReview,
    /// The API server is limiting requests.
    Throttled,
    Failed(String),
}

/// What a drain did, however it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DrainReport {
    pub cordoned: bool,
    /// Evictions the API server accepted.
    pub evicted: Vec<PodName>,
    /// Pods left on the node, and why.
    pub blocked: Vec<(PodName, BlockedReason)>,
    /// DaemonSet and mirror pods, which a drain leaves.
    pub skipped: Vec<PodName>,
    /// Pods not tried, because the drain stopped first.
    pub remaining: Vec<PodName>,
    /// Evicted pods not confirmed gone when the drain ended: they may still be terminating.
    pub terminating: Vec<PodName>,
    /// Why the drain stopped early.
    pub stopped: Option<WriteError>,
}

/// What a drain does with a pod.
#[derive(Debug, PartialEq, Eq)]
enum PodPlan {
    Evict,
    Skip,
    Block(BlockedReason),
}

/// As `kubectl drain --ignore-daemonsets`, with its two opt-ins.
fn plan(pod: &Pod, options: &DrainOptions) -> PodPlan {
    if pod.annotations().contains_key(MIRROR_POD) {
        return PodPlan::Skip;
    }
    let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
    if matches!(phase, Some("Succeeded" | "Failed")) {
        return PodPlan::Evict;
    }
    let controller = pod
        .owner_references()
        .iter()
        .find(|owner| owner.controller == Some(true));
    if controller.is_some_and(|owner| owner.kind == "DaemonSet") {
        return PodPlan::Skip;
    }
    if controller.is_none() && !options.force {
        return PodPlan::Block(BlockedReason::Unmanaged);
    }
    let has_empty_dir = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .is_some_and(|volumes| volumes.iter().any(|v| v.empty_dir.is_some()));
    if has_empty_dir && !options.delete_emptydir_data {
        return PodPlan::Block(BlockedReason::LocalStorage);
    }
    PodPlan::Evict
}

/// Who manages `pod`, from its labels and annotations.
fn pod_manager(pod: &Pod) -> Option<ManagedBy> {
    let name = pod.name_any();
    let namespace = pod.namespace();
    let identity = Identity {
        group: Some(""),
        kind: "Pod",
        namespace: namespace.as_deref(),
        name: &name,
    };
    let texts = |map: &std::collections::BTreeMap<String, String>| {
        serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
                .collect(),
        )
    };
    let metadata = serde_json::json!({
        "labels": texts(pod.labels()),
        "annotations": texts(pod.annotations()),
    });
    gitops::managed_by_live_metadata(&identity, &metadata)
}

/// Whether a GitOps controller manages `pod` in a way the review did not warn about.
fn is_unwarned(pod: &Pod, options: &DrainOptions) -> bool {
    pod_manager(pod)
        .is_some_and(|managed| options.warned.get(&pod.uid().unwrap_or_default()) != Some(&managed))
}

/// The pods on `node` a drain with `options` would evict that a GitOps controller manages.
async fn managed_pods(
    gate: &mut impl ActionGate,
    node: &str,
    options: &DrainOptions,
) -> Result<Vec<ManagedPod>, ActionError> {
    let client = gate.client().await?;
    let params = ListParams::default().fields(&format!("spec.nodeName={node}"));
    let pods = timed(Api::<Pod>::all(client).list(&params)).await?.items;
    Ok(pods
        .iter()
        .filter(|pod| plan(pod, options) == PodPlan::Evict)
        .filter_map(|pod| {
            Some(ManagedPod {
                managed: pod_manager(pod)?,
                pod: pod_name(pod),
                uid: pod.uid().unwrap_or_default(),
            })
        })
        .collect())
}

/// Reads `pod` just before its eviction: `false` if it is gone, and why to leave it if it is
/// another pod now, the drain's options no longer cover it, or it is not what the review warned
/// about.
async fn still_evictable(
    client: Client,
    pod: &Pod,
    options: &DrainOptions,
) -> Result<bool, BlockedReason> {
    match timed(read_pod(client, pod)).await {
        Ok(None) => Ok(false),
        Ok(Some(now)) if now.uid() != pod.uid() => Err(BlockedReason::Changed),
        Ok(Some(now)) => match plan(&now, options) {
            PodPlan::Evict if is_unwarned(&now, options) => Err(BlockedReason::ManagedSinceReview),
            PodPlan::Evict => Ok(true),
            PodPlan::Block(reason) => Err(reason),
            PodPlan::Skip => Err(BlockedReason::Changed),
        },
        Err(error) => Err(BlockedReason::Failed(WriteError::from(error).to_string())),
    }
}

/// Ends a drain at `name`: it and `rest` were not tried.
fn stop_at(
    report: &mut DrainReport,
    name: PodName,
    rest: impl Iterator<Item = Pod>,
    accepted: &[Pod],
    error: ActionError,
) {
    report.remaining.push(name);
    stop_before(report, rest, accepted, error);
}

/// Ends a drain with `rest` not tried.
fn stop_before(
    report: &mut DrainReport,
    rest: impl Iterator<Item = Pod>,
    accepted: &[Pod],
    error: ActionError,
) {
    report.remaining.extend(rest.map(|pod| pod_name(&pod)));
    report.terminating = sorted_names(accepted);
    report.stopped = Some(error.into());
}

/// Ends a drain whose node could not be checked after listing `pods`: none of them were tried.
fn stop_not_rechecked(report: &mut DrainReport, pods: &[Pod], error: ActionError) {
    report.remaining = pods.iter().map(pod_name).collect();
    report.stopped = Some(WriteError::NodeNotRechecked(Box::new(error.into())));
}

fn pod_name(pod: &Pod) -> PodName {
    PodName {
        namespace: pod.namespace().unwrap_or_default(),
        name: pod.name_any(),
    }
}

/// Cordons `node` and evicts its pods through the Eviction API, one try each. Ends with what was
/// done, also when the gate closes on the way.
pub(crate) async fn drain(
    gate: &mut impl ActionGate,
    node: &ActionObject,
    options: &DrainOptions,
) -> DrainReport {
    let mut report = DrainReport::default();
    if let Err(error) = set_cordoned(gate, node, true).await {
        report.stopped = Some(error.into());
        return report;
    }
    report.cordoned = true;

    let client = match gate.client().await {
        Ok(client) => client,
        Err(error) => {
            report.stopped = Some(error.into());
            return report;
        }
    };
    let params = ListParams::default().fields(&format!("spec.nodeName={}", node.name));
    let api = Api::<Pod>::all(client);
    let listed = timed(api.list(&params)).await;
    let pods = match listed {
        Ok(list) => list.items,
        Err(error) => {
            report.stopped = Some(error.into());
            return report;
        }
    };
    let client = match gate.client().await {
        Ok(client) => client,
        Err(error) => {
            stop_not_rechecked(&mut report, &pods, error);
            return report;
        }
    };
    // The list goes by the node's name: it is of the node reviewed only if that still has it.
    let recheck = async {
        let api = node.api(client)?;
        timed(api.get(&node.name)).await
    };
    match recheck.await {
        Ok(now) if now.metadata.uid.as_deref() == Some(node.uid.as_str()) => {}
        Ok(_) => {
            report.stopped = Some(WriteError::Replaced);
            return report;
        }
        Err(error) => {
            stop_not_rechecked(&mut report, &pods, error);
            return report;
        }
    }

    let mut to_evict = Vec::new();
    for pod in pods {
        match plan(&pod, options) {
            PodPlan::Evict if is_unwarned(&pod, options) => report
                .blocked
                .push((pod_name(&pod), BlockedReason::ManagedSinceReview)),
            PodPlan::Evict => to_evict.push(pod),
            PodPlan::Skip => report.skipped.push(pod_name(&pod)),
            PodPlan::Block(reason) => report.blocked.push((pod_name(&pod), reason)),
        }
    }

    let mut accepted = Vec::new();
    let mut pods = to_evict.into_iter();
    while let Some(pod) = pods.next() {
        let name = pod_name(&pod);
        let client = match gate.client().await {
            Ok(client) => client,
            Err(error) => {
                stop_at(&mut report, name, pods, &accepted, error);
                return report;
            }
        };
        match still_evictable(client, &pod, options).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(reason) => {
                report.blocked.push((name, reason));
                continue;
            }
        }
        let client = match gate.client().await {
            Ok(client) => client,
            Err(error) => {
                stop_at(&mut report, name, pods, &accepted, error);
                return report;
            }
        };
        let evicted = tokio::time::timeout(REQUEST_TIMEOUT, evict(client, &pod)).await;
        let Ok(evicted) = evicted else {
            report
                .blocked
                .push((name, BlockedReason::Failed(timed_out())));
            continue;
        };
        match evicted {
            Ok(()) => {
                report.evicted.push(name);
                accepted.push(pod);
            }
            // Nothing left to do for it, if it is really gone: a proxy can answer 404 too.
            Err(error) if api_code(&error) == Some(404) => {
                let client = match gate.client().await {
                    Ok(client) => client,
                    Err(refused) => {
                        let why = "the eviction was sent, but its outcome could not be verified";
                        report
                            .blocked
                            .push((name, BlockedReason::Failed(why.to_owned())));
                        stop_before(&mut report, pods, &accepted, refused);
                        return report;
                    }
                };
                if !is_gone(client, &pod).await {
                    let why = error_text(error);
                    report.blocked.push((name, BlockedReason::Failed(why)));
                }
            }
            Err(error) => {
                let reason = match api_code(&error) {
                    Some(429) if is_disruption_budget(&error) => BlockedReason::DisruptionBudget,
                    Some(429) => BlockedReason::Throttled,
                    Some(409) => BlockedReason::Changed,
                    _ => BlockedReason::Failed(error_text(error)),
                };
                report.blocked.push((name, reason));
            }
        }
    }
    let (terminating, stopped) = wait_until_gone(gate, accepted, options).await;
    report.terminating = terminating;
    report.stopped = stopped.map(Into::into);
    report
}

/// Whether a 429 from the Eviction API is a PodDisruptionBudget's, not a rate limit.
fn is_disruption_budget(error: &kube::Error) -> bool {
    let kube::Error::Api(status) = error else {
        return false;
    };
    status
        .details
        .as_ref()
        .is_some_and(|d| d.causes.iter().any(|c| c.reason == "DisruptionBudget"))
        || status
            .message
            .to_ascii_lowercase()
            .contains("disruption budget")
}

/// Whether `pod` is confirmed gone: no pod has its name, or another one does.
async fn is_gone(client: Client, pod: &Pod) -> bool {
    match timed(read_pod(client, pod)).await {
        Ok(Some(now)) => now.uid() != pod.uid(),
        Ok(None) => true,
        Err(_) => false,
    }
}

fn sorted_names(pods: &[Pod]) -> Vec<PodName> {
    let names: BTreeSet<PodName> = pods.iter().map(pod_name).collect();
    names.into_iter().collect()
}

fn pod_api(client: Client, pod: &Pod) -> Result<(Api<Pod>, String), kube::Error> {
    let namespace = pod.metadata.namespace.as_deref().unwrap_or_default();
    let name = pod.name_any();
    api_path::validate_names(Some(namespace), Some(&name)).map_err(|error| {
        kube::Error::BuildRequest(kube::core::request::Error::Validation(error.to_string()))
    })?;
    Ok((Api::namespaced(client, namespace), name))
}

async fn read_pod(client: Client, pod: &Pod) -> Result<Option<Pod>, kube::Error> {
    let (api, name) = pod_api(client, pod)?;
    api.get_opt(&name).await
}

async fn evict(client: Client, pod: &Pod) -> Result<(), kube::Error> {
    let (api, name) = pod_api(client, pod)?;
    let params = EvictParams {
        delete_options: Some(DeleteParams {
            preconditions: pod.uid().map(|uid| uid_precondition(&uid)),
            ..Default::default()
        }),
        ..Default::default()
    };
    api.evict(&name, &params).await.map(drop)
}

/// Waits for `pods` to go, or for their name to be someone else's. Returns those still there,
/// and the refusal if the gate closed meanwhile.
async fn wait_until_gone(
    gate: &mut impl ActionGate,
    pods: Vec<Pod>,
    options: &DrainOptions,
) -> (Vec<PodName>, Option<ActionError>) {
    let deadline = Instant::now() + options.wait;
    let mut left: Vec<Pod> = pods;
    loop {
        if left.is_empty() {
            return (Vec::new(), None);
        }
        let mut still = Vec::new();
        let mut polling = left.into_iter();
        while let Some(pod) = polling.next() {
            // The gate and the read share what is left of the wait: neither may hold the report.
            let budget = deadline
                .saturating_duration_since(Instant::now())
                .min(REQUEST_TIMEOUT);
            if budget.is_zero() {
                still.push(pod);
                still.extend(polling);
                return (sorted_names(&still), None);
            }
            let read = tokio::time::timeout(budget, async {
                let client = gate.client().await?;
                Ok::<_, ActionError>(read_pod(client, &pod).await)
            })
            .await;
            match read {
                Ok(Err(error)) => {
                    still.push(pod);
                    still.extend(polling);
                    return (sorted_names(&still), Some(error));
                }
                Ok(Ok(Ok(Some(now)))) if now.uid() == pod.uid() => still.push(pod),
                Ok(Ok(Ok(_))) => {}
                // Not known to be gone.
                Ok(Ok(Err(_))) | Err(_) => still.push(pod),
            }
        }
        left = still;
        if left.is_empty() || Instant::now() >= deadline {
            return (sorted_names(&left), None);
        }
        tokio::time::sleep(options.poll).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use http::{Request, Response};
    use serde_json::Value;

    use super::*;

    async fn scale(
        gate: &mut impl ActionGate,
        object: &ActionObject,
        from: u32,
        to: u32,
    ) -> Result<(), ActionError> {
        let kind = kind_info(object.resource.clone());
        send_scale(gate, &kind, object, from, to, PatchMode::default()).await
    }

    async fn rollout_restart(
        gate: &mut impl ActionGate,
        object: &ActionObject,
    ) -> Result<(), ActionError> {
        send_restart(gate, object, PatchMode::default()).await
    }

    /// A request the fake API server saw.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        path: String,
        query: String,
        body: Value,
    }

    type Reply = Arc<dyn Fn(&Seen) -> (u16, Value) + Send + Sync>;

    /// A client answered by `reply`, recording what it was sent.
    fn fake(reply: Reply) -> (Client, Arc<Mutex<Vec<Seen>>>) {
        fake_slow(reply, Duration::ZERO)
    }

    /// [`fake`], taking `slow` to answer a read of one pod after its eviction.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test client answered in process, which reaches no server"
    )]
    fn fake_slow(reply: Reply, slow: Duration) -> (Client, Arc<Mutex<Vec<Seen>>>) {
        let log: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let seen = log.clone();
        let service = tower::service_fn(move |request: Request<Body>| {
            let (seen, reply) = (seen.clone(), reply.clone());
            async move {
                let path = request.uri().path().to_owned();
                let evicted = format!("{path}/eviction");
                if request.method() == http::Method::GET
                    && path.contains("/namespaces/demo/pods/")
                    && seen.lock().unwrap().iter().any(|s| s.path == evicted)
                {
                    tokio::time::sleep(slow).await;
                }
                let (parts, body) = request.into_parts();
                let bytes = body.collect_bytes().await.unwrap();
                let entry = Seen {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_owned(),
                    query: parts.uri.query().unwrap_or_default().to_owned(),
                    body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                };
                seen.lock().unwrap().push(entry.clone());
                let (code, value) = reply(&entry);
                // A string is sent as it is, as a proxy's page would be.
                let bytes = match &value {
                    Value::String(text) => text.clone().into_bytes(),
                    other => serde_json::to_vec(other).unwrap(),
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(Body::from(bytes))
                        .unwrap(),
                )
            }
        });
        (Client::new(service, "default"), log)
    }

    /// A gate the test closes with its flag.
    struct TestGate {
        client: Client,
        closed: Arc<AtomicBool>,
    }

    impl ActionGate for TestGate {
        async fn client(&mut self) -> Result<Client, ActionError> {
            if self.closed.load(Ordering::SeqCst) {
                Err(ActionError::Refused(refusal()))
            } else {
                Ok(self.client.clone())
            }
        }
    }

    fn writes(client: Client, closed: Arc<AtomicBool>) -> TestGate {
        TestGate { client, closed }
    }

    fn refusal() -> WriteRefusal {
        WriteRefusal::Protected {
            cluster: "dev".into(),
        }
    }

    fn resource(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
        ApiResource {
            group: group.into(),
            version: version.into(),
            api_version: if group.is_empty() {
                version.into()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.into(),
            plural: plural.into(),
        }
    }

    fn deployment() -> ActionObject {
        ActionObject {
            resource: resource("apps", "v1", "Deployment", "deployments"),
            namespace: Some("demo".into()),
            name: "web".into(),
            uid: "uid-web".into(),
        }
    }

    fn node() -> ActionObject {
        ActionObject {
            resource: resource("", "v1", "Node", "nodes"),
            namespace: None,
            name: "worker".into(),
            uid: "uid-node".into(),
        }
    }

    /// What the API server answers a PATCH with: the object, or its `Scale`.
    fn patched(seen: &Seen) -> Value {
        let (kind, uid) = if seen.path.ends_with("/scale") {
            ("Scale", "uid-web")
        } else if seen.path.contains("/nodes/") {
            ("Node", "uid-node")
        } else {
            ("Deployment", "uid-web")
        };
        json!({"apiVersion": "v1", "kind": kind, "metadata": {"name": "x", "uid": uid}})
    }

    fn ok(seen: &Seen) -> (u16, Value) {
        if seen.method == "PATCH" {
            return (200, patched(seen));
        }
        (
            200,
            json!({"apiVersion": "v1", "kind": "Status", "status": "Success",
                     "metadata": {"name": "x"}}),
        )
    }

    fn status(code: u16, reason: &str) -> Value {
        json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "code": code, "reason": reason, "message": reason,
        })
    }

    fn open() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn kind_info(resource: ApiResource) -> KindInfo {
        KindInfo {
            resource,
            namespaced: true,
            verbs: Vec::new(),
            singular: String::new(),
            short_names: Vec::new(),
            scale: Default::default(),
        }
    }

    // covers UP-53: a review reads the count it changes from, and each action is for its kinds.
    #[test]
    fn a_review_reads_the_count_and_checks_the_kind() {
        let deployments = kind_info(resource("apps", "v1", "Deployment", "deployments"));
        let nodes = kind_info(resource("", "v1", "Node", "nodes"));
        let live = json!({"spec": {"replicas": 2}});
        assert_eq!(
            review(&deployments, &live, WriteProposal::Scale { to: 5 }),
            Ok(ResourceChange::Scale { from: 2, to: 5 })
        );
        assert!(matches!(
            review(
                &deployments,
                &json!({"spec": {}}),
                WriteProposal::Scale { to: 5 }
            ),
            Err(WriteError::Failed(_))
        ));
        assert!(matches!(
            review(&nodes, &live, WriteProposal::Scale { to: 5 }),
            Err(WriteError::Rejected(_))
        ));
        assert!(matches!(
            review(&nodes, &live, WriteProposal::RolloutRestart),
            Err(WriteError::Rejected(_))
        ));
        assert!(matches!(
            review(&deployments, &live, WriteProposal::Cordon),
            Err(WriteError::Rejected(_))
        ));
        let drain = WriteProposal::Drain {
            delete_emptydir_data: true,
            force: false,
        };
        assert_eq!(
            review(&nodes, &live, drain),
            Ok(ResourceChange::Drain {
                delete_emptydir_data: true,
                force: false
            })
        );
        assert_eq!(
            review(&deployments, &live, WriteProposal::Delete),
            Ok(ResourceChange::Delete)
        );
    }

    // covers UP-53: a custom resource is scaled from the count at its definition's path, and
    // only once that path is known.
    #[test]
    fn a_custom_resource_review_reads_its_own_replica_path() {
        use crate::action_kinds::{ScalePaths, ScaleSubresource};
        let unread = KindInfo {
            scale: ScaleSubresource::Unread,
            ..kind_info(resource("example.dev", "v1", "Widget", "widgets"))
        };
        let paths = ScalePaths::from_crd(".spec.size", ".status.size").unwrap();
        let widgets = KindInfo {
            scale: ScaleSubresource::Paths(paths),
            ..unread.clone()
        };
        let live = json!({"spec": {"size": 4, "replicas": 9}});
        assert_eq!(
            review(&widgets, &live, WriteProposal::Scale { to: 1 }),
            Ok(ResourceChange::Scale { from: 4, to: 1 })
        );
        assert!(matches!(
            review(
                &widgets,
                &json!({"spec": {"replicas": 9}}),
                WriteProposal::Scale { to: 1 }
            ),
            Err(WriteError::Failed(_))
        ));
        assert!(matches!(
            review(&unread, &live, WriteProposal::Scale { to: 1 }),
            Err(WriteError::Rejected(_))
        ));
        assert!(matches!(
            review(&widgets, &live, WriteProposal::RolloutRestart),
            Err(WriteError::Rejected(_))
        ));
    }

    // covers UP-53: delete names the reviewed object by uid.
    #[tokio::test]
    async fn delete_sends_the_uid() {
        let (client, log) = fake(Arc::new(ok));
        delete(&mut writes(client, open()), &deployment())
            .await
            .unwrap();
        let seen = log.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "DELETE");
        assert_eq!(
            seen[0].path,
            "/apis/apps/v1/namespaces/demo/deployments/web"
        );
        assert_eq!(seen[0].body["preconditions"]["uid"], "uid-web");
    }

    #[tokio::test]
    async fn api_path_allows_rbac_names_in_reads_and_deletes() {
        let name = "system:controller:job-controller";
        let resource = resource(
            "rbac.authorization.k8s.io",
            "v1",
            "ClusterRole",
            "clusterroles",
        );
        let kind = KindInfo {
            namespaced: false,
            ..kind_info(resource.clone())
        };
        let object = ActionObject {
            resource,
            namespace: None,
            name: name.into(),
            uid: "uid-role".into(),
        };
        let (client, log) = fake(Arc::new(move |seen| {
            if seen.method == "GET" {
                return (
                    200,
                    json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
                           "metadata": {"name": name, "uid": "uid-role"}}),
                );
            }
            ok(seen)
        }));

        let role = crate::read::get(&client, &kind, None, name).await.unwrap();
        assert_eq!(role.name_any(), name);
        delete(&mut writes(client, open()), &object).await.unwrap();

        let seen = log.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[1].method, "DELETE");
        let path = format!("/apis/rbac.authorization.k8s.io/v1/clusterroles/{name}");
        assert_eq!(seen[0].path, path);
        assert_eq!(seen[1].path, path);
    }

    #[tokio::test]
    async fn api_path_rejects_hostile_deletes_before_sending_a_request() {
        let (client, log) = fake(Arc::new(ok));
        let mut gate = writes(client, open());
        for name in ["a/b", "..", "x?y", "x#y", "%2F"] {
            let mut object = deployment();
            object.name = name.to_owned();
            assert!(matches!(
                delete(&mut gate, &object).await,
                Err(ActionError::Unsupported(_))
            ));
        }
        let mut object = deployment();
        object.namespace = Some("demo/deployments".into());
        assert!(matches!(
            delete(&mut gate, &object).await,
            Err(ActionError::Unsupported(_))
        ));
        object.namespace = Some("demo".into());
        object.resource.plural = "Deployments".into();
        assert!(matches!(
            delete(&mut gate, &object).await,
            Err(ActionError::Unsupported(_))
        ));
        assert!(log.lock().unwrap().is_empty());
    }

    // covers UP-52, UP-53: a conflict is said once, for a new review: nothing retries.
    #[tokio::test]
    async fn a_conflict_comes_back_without_a_retry() {
        let (client, log) = fake(Arc::new(|_| (409, status(409, "Conflict"))));
        let mut gated = writes(client, open());
        for result in [
            delete(&mut gated, &deployment()).await,
            scale(&mut gated, &deployment(), 2, 3).await,
            rollout_restart(&mut gated, &deployment()).await,
            set_cordoned(&mut gated, &node(), true).await,
        ] {
            assert!(
                matches!(result, Err(ActionError::Conflict(_))),
                "{result:?}"
            );
        }
        assert_eq!(log.lock().unwrap().len(), 4);
    }

    // covers UP-52: a patch refused for its uid means the object was replaced: review it again.
    // Any other invalid patch is a failure.
    #[tokio::test]
    async fn a_refused_uid_is_a_conflict() {
        let invalid = |field: &str| {
            json!({
                "apiVersion": "v1", "kind": "Status", "status": "Failure", "code": 422,
                "reason": "Invalid", "message": "invalid",
                "details": {"causes": [{"reason": "FieldValueInvalid", "field": field,
                                        "message": "invalid"}]},
            })
        };
        let uid = invalid("metadata.uid");
        let (client, _log) = fake(Arc::new(move |_| (422, uid.clone())));
        let mut gated = writes(client, open());
        let result = rollout_restart(&mut gated, &deployment()).await;
        assert!(
            matches!(result, Err(ActionError::Conflict(_))),
            "{result:?}"
        );
        let result = set_cordoned(&mut gated, &node(), true).await;
        assert!(
            matches!(result, Err(ActionError::Conflict(_))),
            "{result:?}"
        );

        let other = invalid("spec.replicas");
        let (client, _log) = fake(Arc::new(move |_| (422, other.clone())));
        let result = scale(&mut writes(client, open()), &deployment(), 2, 3).await;
        assert!(matches!(result, Err(ActionError::Failed(_))), "{result:?}");
    }

    // covers UP-54: with the gate closed, no action sends anything.
    #[tokio::test]
    async fn a_closed_gate_sends_nothing() {
        let (client, log) = fake(Arc::new(ok));
        let mut gated = writes(client, Arc::new(AtomicBool::new(true)));
        let refused = Err(ActionError::Refused(refusal()));
        assert_eq!(delete(&mut gated, &deployment()).await, refused);
        assert_eq!(scale(&mut gated, &deployment(), 2, 1).await, refused);
        assert_eq!(rollout_restart(&mut gated, &deployment()).await, refused);
        assert_eq!(set_cordoned(&mut gated, &node(), true).await, refused);
        let report = drain(&mut gated, &node(), &DrainOptions::default()).await;
        assert!(!report.cordoned);
        assert_eq!(report.stopped, Some(WriteError::Refused(refusal())));
        assert!(log.lock().unwrap().is_empty());
    }

    // covers UP-52, UP-53: scale tests the uid and the reviewed count, in one JSON Patch.
    #[tokio::test]
    async fn scale_tests_the_reviewed_count() {
        let (client, log) = fake(Arc::new(ok));
        scale(&mut writes(client, open()), &deployment(), 2, 3)
            .await
            .unwrap();
        let seen = log.lock().unwrap();
        assert_eq!(seen[0].method, "PATCH");
        assert_eq!(
            seen[0].path,
            "/apis/apps/v1/namespaces/demo/deployments/web/scale"
        );
        assert_eq!(
            seen[0].body,
            json!([
                {"op": "test", "path": "/metadata/uid", "value": "uid-web"},
                {"op": "test", "path": "/spec/replicas", "value": 2},
                {"op": "replace", "path": "/spec/replicas", "value": 3},
            ])
        );
    }

    // covers UP-53: a custom resource is scaled through its `scale` subresource, as a built-in
    // kind is, and not at all while its replica paths are not known.
    #[tokio::test]
    async fn a_custom_resource_scales_through_its_subresource() {
        use crate::action_kinds::{ScalePaths, ScaleSubresource};
        let (client, log) = fake(Arc::new(ok));
        let mut gate = writes(client, open());
        let widget = ActionObject {
            resource: resource("example.dev", "v1", "Widget", "widgets"),
            ..deployment()
        };
        let unread = KindInfo {
            scale: ScaleSubresource::Unread,
            ..kind_info(widget.resource.clone())
        };
        let result = send_scale(&mut gate, &unread, &widget, 2, 3, PatchMode::default()).await;
        assert!(matches!(result, Err(ActionError::Unsupported(_))));
        assert!(log.lock().unwrap().is_empty());
        let paths = ScalePaths::from_crd(".spec.size", ".status.size").unwrap();
        let widgets = KindInfo {
            scale: ScaleSubresource::Paths(paths),
            ..unread
        };
        send_scale(&mut gate, &widgets, &widget, 2, 3, PatchMode::default())
            .await
            .unwrap();
        let seen = log.lock().unwrap();
        assert_eq!(
            seen[0].path,
            "/apis/example.dev/v1/namespaces/demo/widgets/web/scale"
        );
        assert_eq!(
            seen[0].body,
            json!([
                {"op": "test", "path": "/metadata/uid", "value": "uid-web"},
                {"op": "test", "path": "/spec/replicas", "value": 2},
                {"op": "replace", "path": "/spec/replicas", "value": 3},
            ])
        );
    }

    // covers UP-53: the scale subresource omits a count of zero, so that is an empty spec.
    #[tokio::test]
    async fn scale_from_zero_tests_an_empty_spec() {
        let (client, log) = fake(Arc::new(ok));
        scale(&mut writes(client, open()), &deployment(), 0, 3)
            .await
            .unwrap();
        assert_eq!(
            log.lock().unwrap()[0].body,
            json!([
                {"op": "test", "path": "/metadata/uid", "value": "uid-web"},
                {"op": "test", "path": "/spec", "value": {}},
                {"op": "add", "path": "/spec/replicas", "value": 3},
            ])
        );
    }

    fn held_target() -> WriteTarget {
        WriteTarget {
            cluster: "dev".into(),
            server: None,
            kind: crate::read::KindId::new("apps", "deployments"),
            namespace: Some("demo".into()),
            name: "web".into(),
            uid: Some("uid-web".into()),
            resource_version: None,
        }
    }

    fn held_live() -> Value {
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web",
                         "resourceVersion": "42"},
            "spec": {"replicas": 2, "template": {"metadata": {"annotations": {"keep": "me"}}}},
        })
    }

    // covers UP-53: a restart reviewed before another restart ran is a conflict, and sends no
    // patch; a status update does not change the mark.
    #[tokio::test]
    async fn a_restart_after_another_restart_is_a_conflict() {
        let mut live = held_live();
        live["spec"]["template"]["metadata"]["annotations"][RESTARTED_AT] = json!("t1");
        let reviewed = Reviewed {
            restarted_at: Some(restarted_at(&live)),
            ..Default::default()
        };
        let run_on = |live: Value| {
            let reviewed = reviewed.clone();
            async move {
                let reply: Reply = Arc::new(move |seen| {
                    if seen.method == "GET" {
                        (200, live.clone())
                    } else {
                        ok(seen)
                    }
                });
                let (client, log) = fake(reply);
                let kind = kind_info(resource("apps", "v1", "Deployment", "deployments"));
                let target = held_target();
                let change = ResourceChange::RolloutRestart;
                let result = run(
                    &mut writes(client, open()),
                    &kind,
                    &target,
                    &change,
                    &reviewed,
                )
                .await
                .map(drop);
                let patches = log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|s| s.method == "PATCH")
                    .count();
                (result, patches)
            }
        };
        let mut status = live.clone();
        status["status"] = json!({"observedGeneration": 3});
        assert_eq!(run_on(status).await, (Ok(()), 1));
        let mut restarted = live.clone();
        restarted["spec"]["template"]["metadata"]["annotations"][RESTARTED_AT] = json!("t2");
        assert_eq!(run_on(restarted).await, (Err(WriteError::Conflict), 0));
    }

    // covers UP-53: a custom resource's scale reads its definition again before the patch: other
    // replica paths are a conflict, and a definition that cannot be read refuses it.
    #[tokio::test]
    async fn a_custom_scale_is_bound_to_the_reviewed_replica_paths() {
        use crate::action_kinds::ScaleSubresource;
        let reviewed_paths = ScalePaths::from_crd(".spec.size", ".status.size").unwrap();
        let kind = KindInfo {
            scale: ScaleSubresource::Paths(reviewed_paths.clone()),
            ..kind_info(resource("example.dev", "v1", "Widget", "widgets"))
        };
        let reviewed = Reviewed {
            scale: Some(reviewed_paths),
            ..Default::default()
        };
        let crd = |path: &str| {
            json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
                   "metadata": {"name": "widgets.example.dev"},
                   "spec": {"group": "example.dev", "names": {"plural": "widgets"},
                            "versions": [{"name": "v1", "served": true, "subresources": {"scale":
                                {"specReplicasPath": path, "statusReplicasPath": ".status.size"}}}]}})
        };
        let run_with = |definition: Option<Value>| {
            let (kind, reviewed) = (kind.clone(), reviewed.clone());
            async move {
                let reply: Reply = Arc::new(move |seen| {
                    if seen.method != "GET" {
                        return ok(seen);
                    }
                    if seen.path.contains("customresourcedefinitions") {
                        return match &definition {
                            Some(crd) => (200, crd.clone()),
                            None => (403, status(403, "Forbidden")),
                        };
                    }
                    (
                        200,
                        json!({"apiVersion": "example.dev/v1", "kind": "Widget",
                               "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web"},
                               "spec": {"size": 2, "other": 2}}),
                    )
                });
                let (client, log) = fake(reply);
                let change = ResourceChange::Scale { from: 2, to: 3 };
                let result = run(
                    &mut writes(client, open()),
                    &kind,
                    &held_target(),
                    &change,
                    &reviewed,
                )
                .await
                .map(drop);
                let patches: Vec<String> = log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|s| s.method == "PATCH")
                    .map(|s| s.path.clone())
                    .collect();
                (result, patches)
            }
        };
        let (result, patches) = run_with(Some(crd(".spec.size"))).await;
        assert_eq!(result, Ok(()));
        assert_eq!(
            patches,
            ["/apis/example.dev/v1/namespaces/demo/widgets/web/scale"]
        );
        let (result, patches) = run_with(Some(crd(".spec.other"))).await;
        assert_eq!((result, patches.len()), (Err(WriteError::Conflict), 0));
        let (result, patches) = run_with(None).await;
        assert!(matches!(result, Err(WriteError::Failed(_))), "{result:?}");
        assert!(patches.is_empty());
    }

    /// The PATCH a dry run of `change` sends, with the object at [`held_live`].
    async fn dry_run_patch(change: ResourceChange) -> Seen {
        let live = held_live();
        let reviewed = Reviewed {
            spec: ReviewedSpec::of(&live),
            hold_spec: true,
            scale: matches!(change, ResourceChange::Scale { .. }).then(ScalePaths::built_in),
            ..Default::default()
        };
        let reply: Reply = Arc::new(move |seen| {
            if seen.method == "GET" {
                (200, live.clone())
            } else {
                ok(seen)
            }
        });
        let (client, log) = fake(reply);
        let kind = kind_info(resource("apps", "v1", "Deployment", "deployments"));
        dry_run(
            &mut writes(client, open()),
            &kind,
            &held_target(),
            &change,
            &reviewed,
        )
        .await
        .unwrap();
        let seen = log.lock().unwrap();
        assert_eq!(seen.len(), 2, "one read, one patch");
        assert_eq!(seen[1].method, "PATCH");
        seen[1].clone()
    }

    // covers UP-100: a held scale tests the version the recheck read, on a dry run.
    #[tokio::test]
    async fn a_held_scale_tests_the_version_read() {
        let seen = dry_run_patch(ResourceChange::Scale { from: 2, to: 3 }).await;
        assert_eq!(
            seen.path,
            "/apis/apps/v1/namespaces/demo/deployments/web/scale"
        );
        assert_eq!(seen.query, "fieldManager=flint&dryRun=All");
        assert_eq!(
            seen.body,
            json!([
                {"op": "test", "path": "/metadata/uid", "value": "uid-web"},
                {"op": "test", "path": "/metadata/resourceVersion", "value": "42"},
                {"op": "test", "path": "/spec/replicas", "value": 2},
                {"op": "replace", "path": "/spec/replicas", "value": 3},
            ])
        );
    }

    // covers UP-100: a held restart tests the version read and keeps the other annotations.
    #[tokio::test]
    async fn a_held_restart_tests_the_version_read() {
        let seen = dry_run_patch(ResourceChange::RolloutRestart).await;
        assert_eq!(seen.path, "/apis/apps/v1/namespaces/demo/deployments/web");
        assert_eq!(seen.query, "fieldManager=flint&dryRun=All");
        let operations = seen.body.as_array().expect("a JSON Patch");
        assert_eq!(
            operations[..2],
            [
                json!({"op": "test", "path": "/metadata/uid", "value": "uid-web"}),
                json!({"op": "test", "path": "/metadata/resourceVersion", "value": "42"}),
            ]
        );
        assert_eq!(operations[2]["path"], "/spec/template/metadata/annotations");
        assert_eq!(operations[2]["value"]["keep"], "me");
        assert!(operations[2]["value"][RESTARTED_AT].is_string());
    }

    // covers UP-53: a name or namespace that is not one path segment reaches no PATCH.
    #[tokio::test]
    async fn a_patch_needs_plain_path_segments() {
        let (client, log) = fake(Arc::new(ok));
        let mut gated = writes(client, open());
        let names = ["web#x", "web?dryRun=All", "web/x", "web%23", "..", ""];
        let objects = names
            .iter()
            .map(|name| ActionObject {
                name: (*name).to_owned(),
                ..deployment()
            })
            .chain(
                ["demo#x", "demo/x", "demo.x"]
                    .iter()
                    .map(|namespace| ActionObject {
                        namespace: Some((*namespace).to_owned()),
                        ..deployment()
                    }),
            );
        for object in objects {
            for result in [
                scale(&mut gated, &object, 2, 3).await,
                rollout_restart(&mut gated, &object).await,
                set_cordoned(&mut gated, &object, true).await,
            ] {
                assert!(
                    matches!(result, Err(ActionError::Unsupported(_))),
                    "{object:?}: {result:?}"
                );
            }
        }
        assert!(log.lock().unwrap().is_empty());
    }

    // covers UP-53: only a 2xx carrying the object patched is a success, on every PATCH.
    #[tokio::test]
    async fn a_patch_answer_must_be_the_object() {
        let page = || json!("<html>Sign in</html>");
        let answers: Vec<(u16, Value)> = vec![
            (302, page()),
            (302, json!("")),
            (200, page()),
            (200, json!("")),
            (
                200,
                json!({"kind": "Status", "status": "Success", "metadata": {}}),
            ),
            (
                200,
                json!({"kind": "Deployment", "metadata": {"uid": "another"}}),
            ),
            (200, json!({"kind": "Node", "metadata": {"uid": "another"}})),
            (
                200,
                json!({"kind": "Scale", "metadata": {"uid": "another"}}),
            ),
        ];
        for (code, answer) in answers {
            let sent = answer.clone();
            let (client, _log) = fake(Arc::new(move |_| (code, sent.clone())));
            let mut gated = writes(client, open());
            for result in [
                scale(&mut gated, &deployment(), 2, 3).await,
                rollout_restart(&mut gated, &deployment()).await,
                set_cordoned(&mut gated, &node(), true).await,
            ] {
                assert!(
                    matches!(result, Err(ActionError::Failed(_))),
                    "{code} {answer}: {result:?}"
                );
            }
        }
    }

    // covers UP-53: a webhook's 422 without causes keeps its reason and is no stale review.
    #[tokio::test]
    async fn a_webhook_refusal_is_not_a_failed_patch_test() {
        let refused = json!({"apiVersion": "v1", "kind": "Status", "status": "Failure",
                             "code": 422, "reason": "Invalid", "details": {},
                             "message": "admission webhook denied the request: rollout freeze"});
        let (client, _log) = fake(Arc::new(move |_| (422, refused.clone())));
        let result = scale(&mut writes(client, open()), &deployment(), 2, 3).await;
        assert!(
            matches!(&result, Err(ActionError::Failed(text)) if text.contains("rollout freeze")),
            "{result:?}"
        );
    }

    /// A gate that takes a moment and says when it answered.
    struct SlowGate {
        client: Client,
        answered: Arc<Mutex<Option<jiff::Timestamp>>>,
    }

    impl ActionGate for SlowGate {
        async fn client(&mut self) -> Result<Client, ActionError> {
            tokio::time::sleep(Duration::from_millis(5)).await;
            *self.answered.lock().unwrap() = Some(jiff::Timestamp::now());
            Ok(self.client.clone())
        }
    }

    // covers UP-53: a restart is stamped once the gate has answered, not before a slow check.
    #[tokio::test]
    async fn a_restart_is_stamped_after_the_gate() {
        let (client, log) = fake(Arc::new(ok));
        let answered = Arc::default();
        let mut gate = SlowGate {
            client,
            answered: Arc::clone(&answered),
        };
        rollout_restart(&mut gate, &deployment()).await.unwrap();
        let answered = answered.lock().unwrap().expect("the gate was asked");
        let seen = log.lock().unwrap();
        let stamp: jiff::Timestamp = seen[0].body["spec"]["template"]["metadata"]["annotations"]
            [RESTARTED_AT]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(stamp >= answered, "{stamp} is before {answered}");
    }

    // covers UP-53: a paused Deployment would accept a restart and restart nothing.
    #[test]
    fn a_paused_deployment_is_not_restarted() {
        let deployments = kind_info(resource("apps", "v1", "Deployment", "deployments"));
        let paused = json!({"spec": {"paused": true}});
        assert!(matches!(
            review(&deployments, &paused, WriteProposal::RolloutRestart),
            Err(WriteError::Rejected(_))
        ));
    }

    // covers UP-52: a failed test names no field and is a conflict; a validation error names
    // one and is not.
    #[tokio::test]
    async fn a_failed_patch_test_is_a_conflict() {
        let bare = json!({"apiVersion": "v1", "kind": "Status", "status": "Failure", "code": 422,
                          "reason": "Invalid", "message": GENERIC_INVALID, "details": {}});
        let (client, log) = fake(Arc::new(move |_| (422, bare.clone())));
        let result = scale(&mut writes(client, open()), &deployment(), 2, 3).await;
        assert!(
            matches!(result, Err(ActionError::Conflict(_))),
            "{result:?}"
        );
        assert_eq!(log.lock().unwrap().len(), 1, "not retried");
    }

    // covers UP-53: only workloads with replicas can be scaled, and nothing is sent otherwise.
    #[tokio::test]
    async fn scale_refuses_other_kinds() {
        let (client, log) = fake(Arc::new(ok));
        let mut gated = writes(client, open());
        for (group, plural) in [
            ("", "pods"),
            ("apps", "daemonsets"),
            ("example.com", "widgets"),
        ] {
            let object = ActionObject {
                resource: resource(group, "v1", "Other", plural),
                ..deployment()
            };
            let result = scale(&mut gated, &object, 1, 2).await;
            assert!(
                matches!(result, Err(ActionError::Unsupported(_))),
                "{plural}"
            );
        }
        assert!(log.lock().unwrap().is_empty());
    }

    // covers UP-53
    #[tokio::test]
    async fn rollout_restart_annotates_the_pod_template() {
        let (client, log) = fake(Arc::new(ok));
        let mut gated = writes(client, open());
        rollout_restart(&mut gated, &deployment()).await.unwrap();
        {
            let seen = log.lock().unwrap();
            assert_eq!(
                seen[0].path,
                "/apis/apps/v1/namespaces/demo/deployments/web"
            );
            assert_eq!(seen[0].body["metadata"]["uid"], "uid-web");
            assert!(
                seen[0].body["spec"]["template"]["metadata"]["annotations"][RESTARTED_AT]
                    .is_string()
            );
        }
        let pod = ActionObject {
            resource: resource("", "v1", "Pod", "pods"),
            ..deployment()
        };
        assert!(matches!(
            rollout_restart(&mut gated, &pod).await,
            Err(ActionError::Unsupported(_))
        ));
        assert_eq!(log.lock().unwrap().len(), 1, "nothing sent for a pod");
    }

    // covers UP-53
    #[tokio::test]
    async fn cordon_and_uncordon_set_the_node_by_uid() {
        let (client, log) = fake(Arc::new(ok));
        let mut gated = writes(client, open());
        set_cordoned(&mut gated, &node(), true).await.unwrap();
        set_cordoned(&mut gated, &node(), false).await.unwrap();
        let seen = log.lock().unwrap();
        assert_eq!(seen[0].path, "/api/v1/nodes/worker");
        assert_eq!(seen[0].body["spec"]["unschedulable"], true);
        assert_eq!(seen[1].body["spec"]["unschedulable"], false);
        assert_eq!(seen[0].body["metadata"]["uid"], "uid-node");
    }

    fn pod(name: &str, owner: Option<&str>, extra: Value) -> Value {
        let mut value = json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "demo", "uid": format!("uid-{name}")},
            "spec": {"containers": [{"name": "c"}]},
            "status": {"phase": "Running"},
        });
        if let Some(kind) = owner {
            value["metadata"]["ownerReferences"] = json!([{
                "apiVersion": "apps/v1", "kind": kind, "name": "owner", "uid": "o",
                "controller": true,
            }]);
        }
        json_merge(&mut value, extra);
        value
    }

    fn json_merge(into: &mut Value, from: Value) {
        match (into, from) {
            (Value::Object(into), Value::Object(from)) => {
                for (key, value) in from {
                    json_merge(into.entry(key).or_insert(Value::Null), value);
                }
            }
            (into, from) => *into = from,
        }
    }

    fn pod_list(pods: Vec<Value>) -> Value {
        json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": pods})
    }

    fn named(name: &str) -> PodName {
        PodName {
            namespace: "demo".into(),
            name: name.into(),
        }
    }

    fn evicted_pod(path: &str) -> Option<&str> {
        path.strip_suffix("/eviction")?.rsplit('/').next()
    }

    fn quick() -> DrainOptions {
        DrainOptions {
            wait: Duration::from_millis(30),
            poll: Duration::from_millis(5),
            ..Default::default()
        }
    }

    /// Answers a drain: the node's patch, `pods`, evictions by `evict`, and every pod gone.
    fn drain_server(
        pods: Vec<Value>,
        evict: impl Fn(&str) -> (u16, Value) + Send + Sync + 'static,
    ) -> Reply {
        let tried = Mutex::new(BTreeSet::new());
        Arc::new(move |seen| match seen.method.as_str() {
            "PATCH" => (200, node_object("uid-node")),
            "POST" => {
                let name = evicted_pod(&seen.path).expect("an eviction");
                tried.lock().unwrap().insert(name.to_owned());
                evict(name)
            }
            _ if seen.path == "/api/v1/pods" => (200, pod_list(pods.clone())),
            _ if seen.path == "/api/v1/nodes/worker" => (200, node_object("uid-node")),
            _ => {
                let name = seen.path.rsplit('/').next().unwrap_or_default();
                let listed = pods.iter().find(|p| p["metadata"]["name"] == name);
                match listed {
                    Some(pod) if !tried.lock().unwrap().contains(name) => (200, pod.clone()),
                    _ => (404, status(404, "NotFound")),
                }
            }
        })
    }

    fn node_object(uid: &str) -> Value {
        json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "worker", "uid": uid}})
    }

    fn accepted() -> (u16, Value) {
        (
            201,
            json!({"apiVersion": "v1", "kind": "Status", "status": "Success"}),
        )
    }

    // covers UP-53: a drain cordons first, evicts what `kubectl drain` evicts through the
    // Eviction API by uid, and says what it left and why.
    #[tokio::test]
    async fn drain_cordons_then_evicts_and_reports_the_rest() {
        let pods = vec![
            pod("web", Some("ReplicaSet"), json!({})),
            pod("agent", Some("DaemonSet"), json!({})),
            pod(
                "static",
                None,
                json!({"metadata": {"annotations": {MIRROR_POD: "x"}}}),
            ),
            pod("bare", None, json!({})),
            pod(
                "cache",
                Some("ReplicaSet"),
                json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}]}}),
            ),
            pod("done", None, json!({"status": {"phase": "Succeeded"}})),
        ];
        let (client, log) = fake(drain_server(pods, |_| accepted()));
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;

        assert!(report.cordoned);
        assert_eq!(report.evicted, [named("web"), named("done")]);
        assert_eq!(report.skipped, [named("agent"), named("static")]);
        assert_eq!(
            report.blocked,
            [
                (named("bare"), BlockedReason::Unmanaged),
                (named("cache"), BlockedReason::LocalStorage),
            ]
        );
        assert!(report.remaining.is_empty());
        assert!(report.terminating.is_empty());
        assert_eq!(report.stopped, None);

        let seen = log.lock().unwrap();
        assert_eq!(seen[0].method, "PATCH", "cordoned first");
        let evictions: Vec<&Seen> = seen.iter().filter(|s| s.method == "POST").collect();
        assert_eq!(evictions.len(), 2);
        assert_eq!(
            evictions[0].path,
            "/api/v1/namespaces/demo/pods/web/eviction"
        );
        assert_eq!(
            evictions[0].body["deleteOptions"]["preconditions"]["uid"],
            "uid-web"
        );
    }

    // covers UP-53: the opt-ins evict what a drain otherwise leaves.
    #[tokio::test]
    async fn drain_options_evict_unmanaged_and_empty_dir_pods() {
        let pods = vec![
            pod("bare", None, json!({})),
            pod(
                "cache",
                Some("ReplicaSet"),
                json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}]}}),
            ),
        ];
        let (client, _log) = fake(drain_server(pods, |_| accepted()));
        let options = DrainOptions {
            force: true,
            delete_emptydir_data: true,
            ..quick()
        };
        let report = drain(&mut writes(client, open()), &node(), &options).await;
        assert_eq!(report.evicted, [named("bare"), named("cache")]);
        assert!(report.blocked.is_empty());
    }

    // covers UP-53: a pod a PodDisruptionBudget protects is reported after one try, and the
    // drain goes on with the others.
    #[tokio::test]
    async fn a_disruption_budget_is_reported_and_not_retried() {
        let pods = vec![
            pod("guarded", Some("ReplicaSet"), json!({})),
            pod("web", Some("ReplicaSet"), json!({})),
        ];
        let (client, log) = fake(drain_server(pods, |name| {
            if name == "guarded" {
                let mut refused = status(429, "TooManyRequests");
                refused["details"] = json!({"causes": [{"reason": "DisruptionBudget"}]});
                (429, refused)
            } else {
                accepted()
            }
        }));
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(
            report.blocked,
            [(named("guarded"), BlockedReason::DisruptionBudget)]
        );
        assert_eq!(report.evicted, [named("web")]);
        let tries = log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.path.ends_with("/guarded/eviction"))
            .count();
        assert_eq!(tries, 1);
    }

    // covers UP-54: the gate closing during a drain stops the next eviction, and the report
    // says what was done and what is left.
    #[tokio::test]
    async fn a_closed_gate_stops_the_next_eviction() {
        let pods = vec![
            pod("one", Some("ReplicaSet"), json!({})),
            pod("two", Some("ReplicaSet"), json!({})),
            pod("three", Some("ReplicaSet"), json!({})),
        ];
        let closed = open();
        let closing = closed.clone();
        let (client, log) = fake(drain_server(pods, move |_| {
            closing.store(true, Ordering::SeqCst);
            accepted()
        }));
        let report = drain(&mut writes(client, closed), &node(), &quick()).await;
        assert!(report.cordoned);
        assert_eq!(report.evicted, [named("one")]);
        assert_eq!(report.terminating, [named("one")]);
        assert_eq!(report.remaining, [named("two"), named("three")]);
        assert_eq!(report.stopped, Some(WriteError::Refused(refusal())));
        let evictions = log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.method == "POST")
            .count();
        assert_eq!(evictions, 1);
    }

    // covers UP-53: an identity refusal after an eviction keeps the partial report, and its text
    // speaks of the next request only.
    #[test]
    fn a_credential_refusal_does_not_deny_earlier_changes() {
        for refused in [
            WriteError::IdentityUnverified,
            WriteError::IdentityUnavailable,
        ] {
            let stopped = WriteError::from(ActionError::Credential(refused.clone()));
            assert_eq!(stopped, refused);
            let text = stopped.to_string();
            assert!(text.contains("the next request was refused"), "{text}");
            assert!(!text.contains("nothing was changed"), "{text}");
        }
    }

    // covers UP-54: the gate closing while evicted pods terminate ends the wait, saying so.
    #[tokio::test]
    async fn a_closed_gate_ends_the_wait_for_terminating_pods() {
        let pods = vec![pod("one", Some("ReplicaSet"), json!({}))];
        let closed = open();
        let closing = closed.clone();
        let (client, _log) = fake(drain_server(pods, move |_| {
            closing.store(true, Ordering::SeqCst);
            accepted()
        }));
        let report = drain(&mut writes(client, closed), &node(), &quick()).await;
        assert_eq!(report.evicted, [named("one")]);
        assert_eq!(report.terminating, [named("one")]);
        assert_eq!(report.stopped, Some(WriteError::Refused(refusal())));
    }

    // covers UP-53: a node replaced under its name after the review has none of its pods evicted.
    #[tokio::test]
    async fn a_replaced_node_stops_the_drain_before_any_eviction() {
        let pods = vec![pod("one", Some("ReplicaSet"), json!({}))];
        let reply: Reply = Arc::new(move |seen| match seen.method.as_str() {
            "PATCH" => (200, node_object("uid-node")),
            "POST" => accepted(),
            _ if seen.path == "/api/v1/pods" => (200, pod_list(pods.clone())),
            _ => (200, node_object("uid-new-node")),
        });
        let (client, log) = fake(reply);
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(report.stopped, Some(WriteError::Replaced));
        assert!(report.evicted.is_empty());
        assert!(!log.lock().unwrap().iter().any(|s| s.method == "POST"));
    }

    // covers UP-53: a 404 from the eviction is a failure while the pod is still there.
    #[tokio::test]
    async fn a_404_for_a_pod_still_there_is_reported() {
        let listed = vec![pod("stuck", Some("ReplicaSet"), json!({}))];
        let still = listed[0].clone();
        let reply: Reply = Arc::new(move |seen| match seen.method.as_str() {
            "PATCH" => (200, node_object("uid-node")),
            "POST" => (404, status(404, "NotFound")),
            _ if seen.path == "/api/v1/pods" => (200, pod_list(listed.clone())),
            _ if seen.path == "/api/v1/nodes/worker" => (200, node_object("uid-node")),
            _ => (200, still.clone()),
        });
        let (client, _log) = fake(reply);
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert!(report.evicted.is_empty());
        assert!(
            matches!(&report.blocked[..], [(pod, BlockedReason::Failed(_))] if *pod == named("stuck")),
            "{:?}",
            report.blocked
        );
    }

    // covers UP-53: a server that stalls after the evictions does not hold the report past the
    // wait, and the pods it could not confirm are named.
    #[tokio::test]
    async fn a_stalled_server_ends_the_wait_on_time() {
        let pods = vec![
            pod("one", Some("ReplicaSet"), json!({})),
            pod("two", Some("ReplicaSet"), json!({})),
        ];
        let (client, _log) = fake_slow(drain_server(pods, |_| accepted()), Duration::from_secs(20));
        let started = Instant::now();
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(report.terminating, [named("one"), named("two")]);
    }

    // covers UP-53: a drain stopped early still names evicted pods it could not confirm gone.
    #[tokio::test]
    async fn a_stopped_drain_keeps_unconfirmed_pods() {
        let pods = vec![
            pod("one", Some("ReplicaSet"), json!({})),
            pod("two", Some("ReplicaSet"), json!({})),
        ];
        let closed = open();
        let closing = closed.clone();
        let (client, _log) = fake(drain_server(pods, move |_| {
            closing.store(true, Ordering::SeqCst);
            accepted()
        }));
        let report = drain(&mut writes(client, closed), &node(), &quick()).await;
        assert_eq!(report.evicted, [named("one")]);
        assert_eq!(report.terminating, [named("one")]);
        assert_eq!(report.remaining, [named("two")]);
    }

    // covers UP-53: the gate is asked again before the node is read after the list.
    #[tokio::test]
    async fn a_gate_closed_after_the_pod_list_stops_before_the_node_read() {
        let closed = open();
        let closing = closed.clone();
        let inner = drain_server(vec![pod("one", Some("ReplicaSet"), json!({}))], |_| {
            accepted()
        });
        let (client, log) = fake(Arc::new(move |seen| {
            if seen.path == "/api/v1/pods" {
                closing.store(true, Ordering::SeqCst);
            }
            inner(seen)
        }));
        let report = drain(&mut writes(client, closed), &node(), &quick()).await;
        let stopped = report.stopped.expect("the drain stopped");
        assert_eq!(
            stopped,
            WriteError::NodeNotRechecked(Box::new(WriteError::Refused(refusal())))
        );
        assert!(stopped.to_string().contains("could not be checked again"));
        assert!(report.evicted.is_empty());
        assert_eq!(report.remaining, [named("one")]);
        let seen = log.lock().unwrap();
        assert!(
            !seen
                .iter()
                .any(|s| s.path == "/api/v1/nodes/worker" && s.method == "GET")
        );
        assert!(!seen.iter().any(|s| s.method == "POST"));
    }

    // covers UP-53: the gate is asked again before a 404 from the Eviction API is checked.
    #[tokio::test]
    async fn a_gate_closed_on_an_eviction_404_stops_before_the_check() {
        let pods = vec![
            pod("one", Some("ReplicaSet"), json!({})),
            pod("two", Some("ReplicaSet"), json!({})),
        ];
        let closed = open();
        let closing = closed.clone();
        let (client, log) = fake(drain_server(pods, move |_| {
            closing.store(true, Ordering::SeqCst);
            (404, status(404, "NotFound"))
        }));
        let report = drain(&mut writes(client, closed), &node(), &quick()).await;
        assert_eq!(report.stopped, Some(WriteError::Refused(refusal())));
        assert_eq!(report.remaining, [named("two")]);
        assert!(
            matches!(
                &report.blocked[..],
                [(pod, BlockedReason::Failed(why))]
                    if *pod == named("one") && why.contains("could not be verified")
            ),
            "{:?}",
            report.blocked
        );
        let seen = log.lock().unwrap();
        assert_eq!(seen.iter().filter(|s| s.method == "POST").count(), 1);
        let reads_of_one = seen
            .iter()
            .filter(|s| s.method == "GET" && s.path.ends_with("/pods/one"))
            .count();
        assert_eq!(reads_of_one, 1, "only the read before the eviction");
    }

    /// A gate that never answers once `stalled` is set, counting the calls made since.
    struct StallingGate {
        client: Client,
        stalled: Arc<AtomicBool>,
        asked: Arc<AtomicUsize>,
    }

    impl ActionGate for StallingGate {
        async fn client(&mut self) -> Result<Client, ActionError> {
            if self.stalled.load(Ordering::SeqCst) {
                self.asked.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            Ok(self.client.clone())
        }
    }

    // covers UP-53: a gate that does not answer during the wait ends it on time, is not asked
    // past the deadline, and the pods it could not confirm are named.
    #[tokio::test]
    async fn a_stalled_gate_ends_the_wait_on_time() {
        let pods = vec![
            pod("one", Some("ReplicaSet"), json!({})),
            pod("two", Some("ReplicaSet"), json!({})),
        ];
        let stalled = open();
        let stalling = stalled.clone();
        let evictions = AtomicUsize::new(0);
        let (client, _log) = fake(drain_server(pods, move |_| {
            if evictions.fetch_add(1, Ordering::SeqCst) == 1 {
                stalling.store(true, Ordering::SeqCst);
            }
            accepted()
        }));
        let asked = Arc::new(AtomicUsize::new(0));
        let mut gate = StallingGate {
            client,
            stalled,
            asked: asked.clone(),
        };
        let started = Instant::now();
        let report = drain(&mut gate, &node(), &quick()).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(report.evicted, [named("one"), named("two")]);
        assert_eq!(report.terminating, [named("one"), named("two")]);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    // covers UP-53: a lock during a poll is in the report, with the pods not yet confirmed gone.
    #[tokio::test]
    async fn a_gate_closed_during_a_poll_appears_in_the_report() {
        let pods = vec![
            pod("one", Some("ReplicaSet"), json!({})),
            pod("two", Some("ReplicaSet"), json!({})),
        ];
        let evictions = Arc::new(AtomicUsize::new(0));
        let counted = evictions.clone();
        let inner = drain_server(pods, move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            accepted()
        });
        let closed = open();
        let closing = closed.clone();
        let (client, log) = fake(Arc::new(move |seen| {
            if seen.method == "GET" && evictions.load(Ordering::SeqCst) == 2 {
                closing.store(true, Ordering::SeqCst);
            }
            inner(seen)
        }));
        let report = drain(&mut writes(client, closed), &node(), &quick()).await;
        assert_eq!(report.evicted, [named("one"), named("two")]);
        assert_eq!(report.stopped, Some(WriteError::Refused(refusal())));
        assert_eq!(report.terminating, [named("two")]);
        let seen = log.lock().unwrap();
        let reads_of_two = seen
            .iter()
            .filter(|s| s.method == "GET" && s.path.ends_with("/pods/two"))
            .count();
        assert_eq!(reads_of_two, 1, "only the read before the eviction");
    }

    // covers UP-54: server text in an action's error is reduced like any other.
    #[tokio::test]
    async fn an_action_error_shows_reduced_server_text() {
        let message = "denied: password \"hunter2-secret\" Authorization: Bearer abcdefghijklmnop";
        let body = json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "code": 403, "reason": "Forbidden", "message": message,
        });
        let (client, _log) = fake(Arc::new(move |_| (403, body.clone())));
        let result = scale(&mut writes(client, open()), &deployment(), 2, 3).await;
        let Err(ActionError::Failed(text)) = result else {
            panic!("{result:?}");
        };
        assert!(!text.contains("hunter2-secret"), "{text}");
        assert!(!text.contains("abcdefghijklmnop"), "{text}");
    }

    // covers UP-53: a pod replaced since it was listed is reported, and one already gone is not.
    #[tokio::test]
    async fn changed_and_gone_pods_during_a_drain() {
        let pods = vec![
            pod("moved", Some("ReplicaSet"), json!({})),
            pod("gone", Some("ReplicaSet"), json!({})),
        ];
        let (client, _log) = fake(drain_server(pods, |name| match name {
            "moved" => (409, status(409, "Conflict")),
            _ => (404, status(404, "NotFound")),
        }));
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(report.blocked, [(named("moved"), BlockedReason::Changed)]);
        assert!(report.evicted.is_empty());
    }

    // covers UP-53: a 429 with no budget behind it is rate limiting, reported once, and the drain
    // goes on to the next pod.
    #[tokio::test]
    async fn a_rate_limited_eviction_is_not_a_disruption_budget() {
        let pods = vec![
            pod("limited", Some("ReplicaSet"), json!({})),
            pod("web", Some("ReplicaSet"), json!({})),
        ];
        let (client, log) = fake(drain_server(pods, |name| match name {
            "limited" => (429, status(429, "TooManyRequests")),
            _ => accepted(),
        }));
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(
            report.blocked,
            [(named("limited"), BlockedReason::Throttled)]
        );
        assert_eq!(report.evicted, [named("web")]);
        let tries = log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.path.ends_with("/limited/eviction"))
            .count();
        assert_eq!(tries, 1, "not retried");
    }

    // covers UP-51, UP-53: a pod is read just before its eviction. One whose GitOps manager was
    // not reviewed stays, and so does one that cannot be read.
    #[tokio::test]
    async fn a_pod_is_rechecked_before_its_eviction() {
        let listed = vec![
            pod("marked", Some("ReplicaSet"), json!({})),
            pod("unreadable", Some("ReplicaSet"), json!({})),
            pod("plain", Some("ReplicaSet"), json!({})),
        ];
        let plain = listed[2].clone();
        let reply: Reply = Arc::new(move |seen| match seen.method.as_str() {
            "PATCH" => (200, node_object("uid-node")),
            "POST" => accepted(),
            _ if seen.path == "/api/v1/pods" => (200, pod_list(listed.clone())),
            _ if seen.path == "/api/v1/nodes/worker" => (200, node_object("uid-node")),
            _ if seen.path.ends_with("/marked") => (
                200,
                pod(
                    "marked",
                    Some("ReplicaSet"),
                    json!({"metadata": {"labels": {"kustomize.toolkit.fluxcd.io/name": "apps"}}}),
                ),
            ),
            _ if seen.path.ends_with("/unreadable") => (500, status(500, "InternalError")),
            _ if seen.path.ends_with("/plain") => (200, plain.clone()),
            _ => (404, status(404, "NotFound")),
        });
        let (client, log) = fake(reply);
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(report.evicted, [named("plain")]);
        assert_eq!(
            report.blocked[0],
            (named("marked"), BlockedReason::ManagedSinceReview)
        );
        assert!(
            matches!(&report.blocked[1], (pod, BlockedReason::Failed(_)) if *pod == named("unreadable")),
            "{:?}",
            report.blocked
        );
        let seen = log.lock().unwrap();
        let evictions: Vec<&str> = seen
            .iter()
            .filter(|s| s.method == "POST")
            .map(|s| s.path.as_str())
            .collect();
        assert_eq!(evictions, ["/api/v1/namespaces/demo/pods/plain/eviction"]);
        let at = |method: &str, path: &str| {
            seen.iter()
                .position(|s| s.method == method && s.path == path)
                .unwrap()
        };
        assert!(
            at("GET", "/api/v1/namespaces/demo/pods/plain")
                < at("POST", "/api/v1/namespaces/demo/pods/plain/eviction")
        );
    }

    // covers UP-53: evicted pods still there when the wait ends are reported as terminating; a
    // new pod under the same name is not the one evicted.
    #[tokio::test]
    async fn the_report_names_pods_still_terminating() {
        let listed = vec![
            pod("slow", Some("ReplicaSet"), json!({})),
            pod("replaced", Some("ReplicaSet"), json!({})),
        ];
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let replaced = Arc::new(AtomicBool::new(false));
        let reply: Reply = Arc::new(move |seen| match seen.method.as_str() {
            "PATCH" => (200, node_object("uid-node")),
            "POST" => {
                if seen.path.ends_with("/replaced/eviction") {
                    replaced.store(true, Ordering::SeqCst);
                }
                accepted()
            }
            _ if seen.path == "/api/v1/pods" => (200, pod_list(listed.clone())),
            _ if seen.path == "/api/v1/nodes/worker" => (200, node_object("uid-node")),
            _ if seen.path.ends_with("/slow") => {
                counted.fetch_add(1, Ordering::SeqCst);
                (200, pod("slow", Some("ReplicaSet"), json!({})))
            }
            _ if !replaced.load(Ordering::SeqCst) => {
                (200, pod("replaced", Some("ReplicaSet"), json!({})))
            }
            _ => (
                200,
                pod(
                    "replaced",
                    Some("ReplicaSet"),
                    json!({"metadata": {"uid": "a-new-uid"}}),
                ),
            ),
        });
        let (client, _log) = fake(reply);
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(report.terminating, [named("slow")]);
        assert!(reads.load(Ordering::SeqCst) > 1, "it polled");
    }
}
