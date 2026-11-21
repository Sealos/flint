//! Changes to a cluster: one preflight and one way to run them (#16, #17, #211, ADR-0015).
//!
//! A front end describes a change as a [`WriteTarget`] and a [`WriteProposal`].
//! [`CoreHandle::review_write`] reads the live object, says what the front end must warn about,
//! and returns a [`WriteReview`]. Its [`WriteIntent`] is what [`CoreHandle::run_write`] takes.
//!
//! - A confirmation authorizes nothing: the write gate is asked again before **every** request
//!   (`RequestGate`), so locking a cluster stops a multi-step action at its next request.
//! - A review is bound to what it read: its cluster, server, kind, namespace, name, uid and
//!   `resourceVersion`. A [`ReviewedEdit`] can only be made here, and is refused for any other
//!   target.
//! - A write is never retried: a conflict goes back to a new review.
//!
//! To add an action: a [`WriteProposal`] variant, a [`WriteAction`] variant, and an arm in
//! `prepare` and in `execute`. Each request goes through `RequestGate::client`.

use std::future::Future;
use std::time::Duration;

use kube::Client;
use kube::api::{DynamicObject, PostParams};
use serde_json::Value;

use crate::ClusterId;
use crate::api_error::ApiError;
use crate::gitops::{self, ManagedBy};
use crate::handle::CoreHandle;
use crate::protection::WriteRefusal;
use crate::read::{self, KindId, KindInfo};
use crate::recheck::ContextPin;
use crate::redact;

/// How long one request of a write may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The field manager Flint's writes are recorded under.
const FIELD_MANAGER: &str = "flint";

/// What a Secret's values read as in a review's diff.
const HIDDEN_VALUE: &str = "<hidden>";
/// A hidden value the change replaces.
const HIDDEN_CHANGED: &str = "<hidden, changed>";

/// The object a change is for, as the front end showed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTarget {
    pub cluster: ClusterId,
    /// The API server shown: a context that points elsewhere now is refused.
    pub server: Option<String>,
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
    /// The object's uid as shown, if known: another object of the same name is refused.
    pub uid: Option<String>,
    /// The version the change is based on, if known: a newer live object is a conflict.
    pub resource_version: Option<String>,
}

/// A change to review.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteProposal {
    /// Replace the object with this YAML.
    ApplyYaml { yaml: String },
}

/// A reviewed change, ready to run.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WriteAction {
    ApplyYaml(ReviewedEdit),
}

/// What [`CoreHandle::run_write`] runs.
#[derive(Debug, Clone)]
pub struct WriteIntent {
    pub target: WriteTarget,
    pub action: WriteAction,
}

/// What a confirmation must say before a change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteWarning {
    /// A GitOps controller manages the object and will likely put it back on its next sync.
    LikelyReverted(ManagedBy),
}

/// The result of [`CoreHandle::review_write`]: the only thing a confirmation renders.
#[derive(Debug, Clone)]
pub struct WriteReview {
    target: WriteTarget,
    action: WriteAction,
    warnings: Vec<WriteWarning>,
}

impl WriteReview {
    /// The target, with the live object's uid and `resourceVersion`.
    pub fn target(&self) -> &WriteTarget {
        &self.target
    }

    pub fn action(&self) -> &WriteAction {
        &self.action
    }

    pub fn warnings(&self) -> &[WriteWarning] {
        &self.warnings
    }

    /// What to pass to [`CoreHandle::run_write`] once confirmed.
    pub fn into_intent(self) -> WriteIntent {
        WriteIntent {
            target: self.target,
            action: self.action,
        }
    }
}

/// A YAML edit the server accepted in a dry run. Only a review makes one, and it holds what it
/// was reviewed against.
#[derive(Clone)]
pub struct ReviewedEdit {
    bound: WriteTarget,
    body: Value,
    before: String,
    after: String,
}

impl ReviewedEdit {
    /// The live object as YAML, without `status` and `managedFields`, Secret values hidden.
    pub fn before(&self) -> &str {
        &self.before
    }

    /// The object the dry run produced, shown the same way.
    pub fn after(&self) -> &str {
        &self.after
    }
}

// By hand: the body can hold a Secret's values.
impl std::fmt::Debug for ReviewedEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewedEdit")
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WriteOutcome {
    /// The object's `resourceVersion` after the write, if it returned one.
    pub resource_version: Option<String>,
}

/// Why a review or a write did not go through.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WriteError {
    /// The write gate refused: protected, not connected, or the context moved.
    #[error(transparent)]
    Refused(#[from] WriteRefusal),
    #[error("this cluster does not serve {0}")]
    NotServed(KindId),
    #[error("the object no longer exists")]
    Gone,
    /// Another object now has this name.
    #[error("the object was deleted and created again; review it again")]
    Replaced,
    /// The object changed since it was read: reload, and review again.
    #[error("the object changed since it was opened; reload it and review again")]
    Conflict,
    #[error("{0}")]
    InvalidYaml(String),
    /// The edit would change which object this is.
    #[error("the edit changes the object's {0}, which cannot be changed here")]
    IdentityChanged(&'static str),
    /// The intent's target is not what its review was made for.
    #[error("this review was made for another object; review the change again")]
    ReviewMismatch,
    /// The server refused it (validation, an admission webhook, RBAC).
    #[error("{0}")]
    Rejected(String),
    #[error("{0}")]
    Failed(String),
}

/// The write gate, asked before each request of one action. The context must stay as it was at
/// the first request: re-pointed and back in between is another cluster to this action.
pub(crate) struct RequestGate {
    core: CoreHandle,
    cluster: ClusterId,
    server: Option<String>,
    pin: Option<ContextPin>,
}

impl RequestGate {
    fn new(core: CoreHandle, target: &WriteTarget) -> Self {
        RequestGate {
            core,
            cluster: target.cluster.clone(),
            server: target.server.clone(),
            pin: None,
        }
    }

    /// A client for the next request. Call it again for every request: never keep the client.
    pub(crate) async fn client(&mut self) -> Result<Client, WriteError> {
        let (client, pin) = self
            .core
            .write_client(&self.cluster, Some(self.server.clone()))
            .await?;
        if self.pin.as_ref().is_some_and(|first| *first != pin) {
            return Err(WriteRefusal::Moved {
                cluster: self.cluster.to_string(),
            }
            .into());
        }
        // A pin can be stale from the start: a fallback showing while a higher file is unreadable.
        self.core
            .check_write_pin(&self.cluster, pin.clone())
            .await?;
        self.pin = Some(pin);
        Ok(client.into_client())
    }
}

impl CoreHandle {
    /// Reads the live object, checks it is the one `target` names, and reviews `proposal`
    /// against it (a server dry run for an edit, which is why a protected cluster refuses it).
    /// Changes nothing. Works on any executor.
    pub fn review_write(
        &self,
        target: WriteTarget,
        proposal: WriteProposal,
    ) -> impl Future<Output = Result<WriteReview, WriteError>> + Send + 'static {
        let core = self.clone();
        let task = self.on_runtime(async move {
            let mut gate = RequestGate::new(core.clone(), &target);
            let kind = kind_of(&core, &target).await?;
            let client = gate.client().await?;
            let live = read_live(&client, &kind, &target).await?;
            let target = bound_to_live(target, &live)?;
            let warnings = warnings_for(&live);
            let action = prepare(&mut gate, &kind, &target, &live, proposal).await?;
            Ok(WriteReview {
                target,
                action,
                warnings,
            })
        });
        async move { task.await.unwrap_or(Err(WriteRefusal::Stopped.into())) }
    }

    /// Runs a reviewed change. The gate is asked again first, and the change is refused if the
    /// intent's target is not the one reviewed. Never retried. Works on any executor.
    pub fn run_write(
        &self,
        intent: WriteIntent,
    ) -> impl Future<Output = Result<WriteOutcome, WriteError>> + Send + 'static {
        let core = self.clone();
        let task = self.on_runtime(async move {
            check_bound(&intent)?;
            let mut gate = RequestGate::new(core.clone(), &intent.target);
            let kind = kind_of(&core, &intent.target).await?;
            execute(&mut gate, &kind, intent).await
        });
        async move { task.await.unwrap_or(Err(WriteRefusal::Stopped.into())) }
    }
}

async fn kind_of(core: &CoreHandle, target: &WriteTarget) -> Result<KindInfo, WriteError> {
    core.discovery(&target.cluster)
        .await
        .and_then(|d| d.kind(&target.kind).cloned())
        .ok_or_else(|| WriteError::NotServed(target.kind.clone()))
}

async fn read_live(
    client: &Client,
    kind: &KindInfo,
    target: &WriteTarget,
) -> Result<Value, WriteError> {
    let namespace = target.namespace.as_deref();
    let object = tokio::time::timeout(
        WRITE_TIMEOUT,
        read::dynamic_api(client, kind, namespace).get(&target.name),
    )
    .await
    .map_err(|_| timed_out())?
    .map_err(write_error)?;
    let mut value = serde_json::to_value(&object)
        .map_err(|e| WriteError::Failed(format!("cannot read the object: {e}")))?;
    if value.get("apiVersion").is_none() {
        value["apiVersion"] = kind.api_version().into();
        value["kind"] = kind.kind().into();
    }
    Ok(value)
}

fn timed_out() -> WriteError {
    WriteError::Failed(format!(
        "the API server did not answer within {} s; the change may or may not have applied",
        WRITE_TIMEOUT.as_secs()
    ))
}

fn write_error(error: kube::Error) -> WriteError {
    if let Some(status) = ApiError::from_kube(&error) {
        return match status.code {
            404 => WriteError::Gone,
            409 => WriteError::Conflict,
            code => WriteError::Rejected(redact::safe_error_summary(
                &status
                    .explanation()
                    .unwrap_or_else(|| crate::api_error::status_meaning(code, None)),
            )),
        };
    }
    WriteError::Failed(redact::safe_error_summary(&error.to_string()))
}

fn text_at<'a>(object: &'a Value, pointer: &str) -> Option<&'a str> {
    object.pointer(pointer).and_then(Value::as_str)
}

/// `target` with the live object's uid and version, if it is the object `target` was shown as.
fn bound_to_live(mut target: WriteTarget, live: &Value) -> Result<WriteTarget, WriteError> {
    let uid = text_at(live, "/metadata/uid").map(str::to_owned);
    let version = text_at(live, "/metadata/resourceVersion").map(str::to_owned);
    if target.uid.is_some() && target.uid != uid {
        return Err(WriteError::Replaced);
    }
    if target.resource_version.is_some() && target.resource_version != version {
        return Err(WriteError::Conflict);
    }
    target.uid = uid;
    target.resource_version = version;
    Ok(target)
}

/// From the live object only: an edit cannot remove the warning by removing the label.
fn warnings_for(live: &Value) -> Vec<WriteWarning> {
    gitops::managed_by_object(live)
        .map(WriteWarning::LikelyReverted)
        .into_iter()
        .collect()
}

fn check_bound(intent: &WriteIntent) -> Result<(), WriteError> {
    match &intent.action {
        WriteAction::ApplyYaml(edit) if edit.bound == intent.target => Ok(()),
        WriteAction::ApplyYaml(_) => Err(WriteError::ReviewMismatch),
    }
}

async fn prepare(
    gate: &mut RequestGate,
    kind: &KindInfo,
    target: &WriteTarget,
    live: &Value,
    proposal: WriteProposal,
) -> Result<WriteAction, WriteError> {
    match proposal {
        WriteProposal::ApplyYaml { yaml } => {
            let mut body = edit_body(&yaml, kind, target)?;
            keep_live_status(&mut body, live);
            let client = gate.client().await?;
            let result = replace(&client, kind, target, &body, true).await?;
            let result = serde_json::to_value(&result)
                .map_err(|e| WriteError::Failed(format!("cannot read the dry run: {e}")))?;
            let result = with_type_of(result, &body);
            Ok(WriteAction::ApplyYaml(ReviewedEdit {
                bound: target.clone(),
                before: shown_yaml(live)?,
                after: shown_after(live, &result)?,
                body,
            }))
        }
    }
}

async fn execute(
    gate: &mut RequestGate,
    kind: &KindInfo,
    intent: WriteIntent,
) -> Result<WriteOutcome, WriteError> {
    match &intent.action {
        WriteAction::ApplyYaml(edit) => {
            let client = gate.client().await?;
            let written = replace(&client, kind, &intent.target, &edit.body, false).await?;
            Ok(WriteOutcome {
                resource_version: written.metadata.resource_version,
            })
        }
    }
}

async fn replace(
    client: &Client,
    kind: &KindInfo,
    target: &WriteTarget,
    body: &Value,
    dry_run: bool,
) -> Result<DynamicObject, WriteError> {
    let object: DynamicObject = serde_json::from_value(body.clone())
        .map_err(|e| WriteError::InvalidYaml(format!("not a Kubernetes object: {e}")))?;
    let params = PostParams {
        dry_run,
        field_manager: Some(FIELD_MANAGER.to_owned()),
    };
    let api = read::dynamic_api(client, kind, target.namespace.as_deref());
    tokio::time::timeout(WRITE_TIMEOUT, api.replace(&target.name, &params, &object))
        .await
        .map_err(|_| timed_out())?
        .map_err(write_error)
}

/// The body to `PUT` for `yaml`: the same object as `target`, at the version reviewed. An edit
/// cannot move it to another name, namespace or kind, or drop the version that makes a
/// concurrent change a conflict.
fn edit_body(yaml: &str, kind: &KindInfo, target: &WriteTarget) -> Result<Value, WriteError> {
    // No source snippet, and no quoted scalars: the text can be a Secret's.
    let options = serde_saphyr::options! { with_snippet: false };
    let mut body: Value = serde_saphyr::from_str_with_options(yaml, options).map_err(|e| {
        WriteError::InvalidYaml(format!(
            "not valid YAML: {}",
            redact::redact_quoted(&e.to_string())
        ))
    })?;
    if !body.is_object() {
        return Err(WriteError::InvalidYaml(
            "not a Kubernetes object".to_owned(),
        ));
    }
    if text_at(&body, "/apiVersion") != Some(kind.api_version()) {
        return Err(WriteError::IdentityChanged("apiVersion"));
    }
    if text_at(&body, "/kind") != Some(kind.kind()) {
        return Err(WriteError::IdentityChanged("kind"));
    }
    if text_at(&body, "/metadata/name") != Some(target.name.as_str()) {
        return Err(WriteError::IdentityChanged("name"));
    }
    let namespace = text_at(&body, "/metadata/namespace");
    if namespace.is_some() && namespace != target.namespace.as_deref() {
        return Err(WriteError::IdentityChanged("namespace"));
    }
    let uid = text_at(&body, "/metadata/uid");
    if uid.is_some() && uid != target.uid.as_deref() {
        return Err(WriteError::IdentityChanged("uid"));
    }
    let version = text_at(&body, "/metadata/resourceVersion");
    if version.is_some() && version != target.resource_version.as_deref() {
        return Err(WriteError::Conflict);
    }
    let metadata = body["metadata"].as_object_mut().expect("it has a name");
    metadata.remove("managedFields");
    if let Some(namespace) = &target.namespace {
        metadata.insert("namespace".to_owned(), namespace.as_str().into());
    }
    if let Some(uid) = &target.uid {
        metadata.insert("uid".to_owned(), uid.as_str().into());
    }
    if let Some(version) = &target.resource_version {
        metadata.insert("resourceVersion".to_owned(), version.as_str().into());
    }
    Ok(body)
}

/// A dry run's answer can leave out `apiVersion` and `kind`.
fn with_type_of(mut object: Value, body: &Value) -> Value {
    for field in ["apiVersion", "kind"] {
        if object.get(field).is_none() {
            object[field] = body[field].clone();
        }
    }
    object
}

/// The editor leaves `status` out, and a `PUT` to a kind without a status subresource would
/// erase it.
fn keep_live_status(body: &mut Value, live: &Value) {
    if body.get("status").is_none()
        && let Some(status) = live.get("status")
    {
        body["status"] = status.clone();
    }
}

/// `object` as a review shows it: no `status` or `managedFields`, Secret values hidden.
fn shown_value(object: &Value) -> Value {
    let mut shown = object.clone();
    if let Some(fields) = shown.as_object_mut() {
        fields.remove("status");
    }
    if let Some(metadata) = shown.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("managedFields");
    }
    if redact::is_secret_object(&shown) {
        redact::mask_secret(&mut shown, HIDDEN_VALUE);
    }
    shown
}

fn shown_yaml(object: &Value) -> Result<String, WriteError> {
    render(&shown_value(object))
}

/// The result of an edit as shown. A hidden value that changed says so, or the diff of a rotated
/// Secret key would be empty.
fn shown_after(live: &Value, after: &Value) -> Result<String, WriteError> {
    let mut shown = shown_value(after);
    for fields in ["/data", "/metadata/annotations"] {
        let was = live.pointer(fields).and_then(Value::as_object);
        let now = after.pointer(fields).and_then(Value::as_object);
        let map = shown.pointer_mut(fields).and_then(Value::as_object_mut);
        let (Some(was), Some(now), Some(map)) = (was, now, map) else {
            continue;
        };
        for (key, value) in map {
            let hidden = value.as_str() == Some(HIDDEN_VALUE);
            if hidden && was.get(key).is_some_and(|old| Some(old) != now.get(key)) {
                *value = HIDDEN_CHANGED.into();
            }
        }
    }
    render(&shown)
}

fn render(shown: &Value) -> Result<String, WriteError> {
    serde_saphyr::to_string(shown)
        .map_err(|e| WriteError::Failed(format!("cannot render YAML: {e}")))
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
