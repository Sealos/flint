//! Changes to a cluster: one preflight and one way to run them (#16, #17, #211, ADR-0015).
//!
//! A front end describes a change as a [`WriteTarget`] and a [`WriteProposal`].
//! [`CoreHandle::review_write`] reads the live object, says what the front end must warn about,
//! and returns a [`WriteReview`]. Its [`WriteIntent`] is what [`CoreHandle::run_write`] takes.
//!
//! - A confirmation authorizes nothing: the write gate is asked again before **every** request
//!   (`RequestGate`), so locking a cluster stops a multi-step action at its next request. Each
//!   time it reads the kubeconfig files, which must still show the exact definition reviewed.
//! - A review is bound to what it read: its cluster, server, kind, namespace, name, uid and
//!   `resourceVersion`. A [`ReviewedEdit`] can only be made here, and is refused for any other
//!   target.
//! - A review is bound to the credential it was made with (`credential`): any change refuses
//!   the write, and the request sends the credential that was checked.
//! - A write is never retried: a conflict goes back to a new review.
//! - A resource action rechecks the live object before it runs, and a different uid or GitOps
//!   manager is a conflict.
//!
//! To add an action: a [`WriteProposal`] variant, a [`WriteAction`] variant, and an arm in
//! `prepare` and in `execute`. Each request goes through `RequestGate::client`. The resource
//! actions (delete, scale, restart, cordon, drain) are in `write_actions`.

use std::future::Future;
use std::time::Duration;

use kube::Client;
use kube::api::{DynamicObject, Patch, PatchParams, PostParams};
use serde_json::Value;

use crate::ClusterId;
use crate::api_error::ApiError;
use crate::credential::CredentialBinding;
use crate::gitops::{self, ManagedBy};
use crate::handle::CoreHandle;
use crate::protection::WriteRefusal;
use crate::read::{self, KindId, KindInfo};
use crate::recheck::ContextPin;
use crate::redact;
use crate::secret_edit;
use crate::write_actions;
pub use crate::write_actions::{BlockedReason, DrainProgress, DrainReport, ManagedPod, PodName};

/// How long one request of a write may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The field manager Flint's writes are recorded under.
const FIELD_MANAGER: &str = "flint";

/// What a Secret's values read as in a review's diff.
pub(crate) const HIDDEN_VALUE: &str = "<hidden>";
/// A hidden value the change replaces.
pub(crate) const HIDDEN_CHANGED: &str = "<hidden, changed>";

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
    ApplyYaml {
        yaml: String,
    },
    /// Create the object from this manifest, or replace it if it exists. One manifest for
    /// several clusters: the fields a cluster's own copy carries (`uid`, `resourceVersion`,
    /// `status` and the like) are left out, and each cluster's are used.
    ApplyManifest {
        yaml: String,
    },
    Delete,
    /// Set a workload's replicas. The review reads the count it changes from.
    Scale {
        to: u32,
    },
    /// Restart a Deployment's, DaemonSet's or StatefulSet's pods.
    RolloutRestart,
    Cordon,
    Uncordon,
    /// Cordon a node and evict its pods.
    Drain {
        /// Also evict pods with `emptyDir` volumes, whose data is lost.
        delete_emptydir_data: bool,
        /// Also evict pods no controller will recreate.
        force: bool,
    },
}

/// A reviewed change, ready to run.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WriteAction {
    ApplyYaml(ReviewedEdit),
    Resource(ReviewedAction),
}

impl WriteAction {
    /// The connection definition this action was reviewed on.
    fn pin(&self) -> &ContextPin {
        match self {
            WriteAction::ApplyYaml(edit) => &edit.pin,
            WriteAction::Resource(action) => &action.pin,
        }
    }

    /// The credential this action was reviewed with.
    fn credential(&self) -> &CredentialBinding {
        match self {
            WriteAction::ApplyYaml(edit) => &edit.credential,
            WriteAction::Resource(action) => &action.credential,
        }
    }
}

/// A resource action as reviewed. Only a review makes one, and it holds what it was reviewed
/// against.
#[derive(Debug, Clone)]
pub struct ReviewedAction {
    bound: WriteTarget,
    /// The connection definition it was reviewed on.
    pin: ContextPin,
    /// The credential it was reviewed with.
    credential: CredentialBinding,
    change: ResourceChange,
    reviewed: write_actions::Reviewed,
}

impl ReviewedAction {
    pub fn change(&self) -> &ResourceChange {
        &self.change
    }

    /// For a restart: the pod template's `restartedAt` as reviewed (`Some(None)`: none). A copy
    /// with another one shows a restart ran since the review.
    pub fn restarted_at(&self) -> Option<Option<&str>> {
        self.reviewed.restarted_at.as_ref().map(Option::as_deref)
    }

    /// For a scale: where the count reviewed was read, as the kind's definition said then. A copy
    /// is read at these paths to tell whether the count changed since the review.
    pub fn scale_paths(&self) -> Option<&crate::action_kinds::ScalePaths> {
        self.reviewed.scale.as_ref()
    }
}

/// What a reviewed resource action does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceChange {
    Delete,
    /// From the count the review read: another count by then is a conflict.
    Scale {
        from: u32,
        to: u32,
    },
    RolloutRestart,
    Cordon,
    Uncordon,
    Drain {
        delete_emptydir_data: bool,
        force: bool,
    },
}

/// What [`CoreHandle::run_write`] runs.
#[derive(Debug, Clone)]
pub struct WriteIntent {
    pub target: WriteTarget,
    pub action: WriteAction,
}

impl WriteIntent {
    /// This intent, refused if the object's `spec` is no longer the one reviewed.
    pub(crate) fn holding_spec(mut self) -> Self {
        if let WriteAction::Resource(action) = &mut self.action {
            action.reviewed.hold_spec = true;
        }
        self
    }
}

/// What a confirmation must say before a change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteWarning {
    /// A GitOps controller manages the object and will likely put it back on its next sync.
    LikelyReverted(ManagedBy),
    /// A drain would evict these pods, which GitOps controllers manage.
    EvictsManaged(Vec<ManagedPod>),
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
    /// The connection definition it was reviewed on.
    pin: ContextPin,
    /// The credential it was reviewed with.
    credential: CredentialBinding,
    /// The object to `PUT`. For a Secret, the edit as typed, masks and all: the merge patch is
    /// built from it against the live object at each request ([`secret_edit`]).
    body: Value,
    /// A Secret, whose masked values are kept: written as a merge patch of what changed.
    secret: bool,
    /// Whether it creates the object, which did not exist when reviewed: run, it is refused if
    /// the object exists by then.
    creates: bool,
    before: String,
    after: String,
}

impl ReviewedEdit {
    /// Whether it creates the object rather than replacing it.
    pub fn creates(&self) -> bool {
        self.creates
    }

    /// The live object as YAML, without `status` and `managedFields`, Secret values hidden.
    /// Empty when it creates the object.
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
    /// What a drain did, also when it stopped early.
    pub drain: Option<DrainReport>,
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
    /// The context's connection definition is not the reviewed one.
    #[error("this cluster's connection changed; review the change again")]
    ConnectionChanged,
    /// A kubeconfig file that could define this context can't be read.
    #[error("a kubeconfig file can't be read; fix it, then review the change again")]
    KubeconfigUnreadable,
    /// The credential is not the one the review was made with.
    #[error("this cluster's credential changed since the review; review the change again")]
    CredentialChanged,
    /// A token, certificate or key file can't be read.
    #[error(
        "a credential file for this cluster can't be read; fix it, then review the change again"
    )]
    CredentialUnreadable,
    /// The server did not say who the credential authenticates as.
    #[error(
        "Flint could not confirm who this cluster sees you as, so the next request was refused; \
         review the change again"
    )]
    IdentityUnverified,
    /// The credential comes from a plugin and the server has no `SelfSubjectReview`.
    #[error(
        "this cluster does not serve SelfSubjectReview (Kubernetes 1.28 or later), which Flint \
         needs to confirm a plugin's identity, so the next request was refused; review the \
         change again once it does"
    )]
    IdentityUnavailable,
    /// The server refused it (validation, an admission webhook, RBAC).
    #[error("{0}")]
    Rejected(String),
    #[error("{0}")]
    Failed(String),
    /// A drain stopped between listing a node's pods and checking the node again.
    #[error("{0}; the node could not be checked again, so its listed pods were not tried")]
    NodeNotRechecked(Box<WriteError>),
}

/// The write gate, asked before each request of one action. The context must stay as it was at
/// the first request: re-pointed and back in between is another cluster to this action.
pub(crate) struct RequestGate {
    core: CoreHandle,
    cluster: ClusterId,
    server: Option<String>,
    pin: Option<ContextPin>,
    /// The credential every request of this review goes out with.
    credential: Option<CredentialBinding>,
    /// How many disconnects of the cluster were asked for when this gate was made: one asked
    /// for since stops it, even before the core runs it.
    disconnects: u64,
}

impl RequestGate {
    fn new(core: CoreHandle, target: &WriteTarget) -> Self {
        let disconnects = core.disconnects_of(&target.cluster);
        RequestGate {
            core,
            cluster: target.cluster.clone(),
            server: target.server.clone(),
            pin: None,
            credential: None,
            disconnects,
        }
    }

    /// Refuses once a disconnect of the cluster was asked for since this gate was made.
    fn check_disconnects(&self) -> Result<(), WriteError> {
        if self
            .core
            .disconnect_ends_write(&self.cluster, self.disconnects)
        {
            return Err(WriteRefusal::NotConnected {
                cluster: self.cluster.to_string(),
            }
            .into());
        }
        Ok(())
    }

    /// The pin every request of this review went out on.
    fn reviewed_pin(&self) -> Result<ContextPin, WriteError> {
        self.pin.clone().ok_or(WriteRefusal::Stopped.into())
    }

    /// The credential every request of this review went out with.
    fn reviewed_credential(&self) -> Result<CredentialBinding, WriteError> {
        self.credential.clone().ok_or(WriteRefusal::Stopped.into())
    }

    /// A client for the next request. Call it again for every request: never keep the client.
    pub(crate) async fn client(&mut self) -> Result<Client, WriteError> {
        self.check_disconnects()?;
        let (client, pin, credential) = self
            .core
            .bound_write_client(&self.cluster, Some(self.server.clone()), None)
            .await?;
        // A set's row stays on the definition admitted, from before its credential is touched.
        self.core.check_admitted(&self.cluster, &pin)?;
        // A pin can be stale from the start: a fallback showing while a higher file is unreadable.
        self.core
            .check_write_pin(&self.cluster, pin.clone())
            .await?;
        if let Some(first) = self.pin.as_ref().filter(|first| **first != pin) {
            return Err(if first.only_definition_differs(&pin) {
                WriteError::ConnectionChanged
            } else {
                WriteRefusal::Moved {
                    cluster: self.cluster.to_string(),
                }
                .into()
            });
        }
        self.pin = Some(pin.clone());
        let presented = credential.present(client.into_client()).await?;
        if let Some(reviewed) = &self.credential {
            reviewed.holds(&presented.binding)?;
        }
        // Reading the credential can take a while: the gate has the last word.
        self.core.check_write_pin(&self.cluster, pin).await?;
        self.check_disconnects()?;
        self.credential = Some(presented.binding);
        Ok(presented.client)
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
            let live = match read_live(&client, &kind, &target).await {
                // Only a manifest creates, and only an object that was never shown.
                Err(WriteError::Gone) if creates_when_absent(&target, &proposal) => {
                    let action = prepare_create(&mut gate, &kind, &target, proposal).await?;
                    return Ok(WriteReview {
                        target,
                        action,
                        warnings: Vec::new(),
                    });
                }
                live => live?,
            };
            let target = bound_to_live(target, &live)?;
            let mut warnings = warnings_for(&live);
            let action = prepare(&mut gate, &kind, &target, &live, proposal).await?;
            if let WriteAction::Resource(action) = &action
                && !action.reviewed.pods.is_empty()
            {
                warnings.push(WriteWarning::EvictsManaged(action.reviewed.pods.clone()));
            }
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
        self.run_write_telling(intent, None)
    }

    /// [`CoreHandle::run_write`], telling a drain's progress to `progress` as it runs. The
    /// progress only reports: it changes nothing the drain does. `progress` is dropped once the
    /// write has ended, so its receiver ends before the write's result is ready.
    pub fn run_write_telling(
        &self,
        intent: WriteIntent,
        progress: Option<async_channel::Sender<DrainProgress>>,
    ) -> impl Future<Output = Result<WriteOutcome, WriteError>> + Send + 'static {
        let core = self.clone();
        let task = self.on_runtime(async move {
            check_bound(&intent)?;
            let mut gate = RequestGate::new(core.clone(), &intent.target);
            gate.pin = Some(intent.action.pin().clone());
            gate.credential = Some(intent.action.credential().clone());
            let kind = kind_of(&core, &intent.target).await?;
            execute(&mut gate, &kind, intent, progress).await
        });
        async move { task.await.unwrap_or(Err(WriteRefusal::Stopped.into())) }
    }

    /// Asks the server to validate a reviewed change without storing it, through the same gate
    /// as the write. An edit's review already did. Works on any executor.
    pub(crate) fn dry_run_write(
        &self,
        intent: WriteIntent,
    ) -> impl Future<Output = Result<(), WriteError>> + Send + 'static {
        let core = self.clone();
        let task = self.on_runtime(async move {
            check_bound(&intent)?;
            let WriteAction::Resource(action) = &intent.action else {
                return Ok(());
            };
            let mut gate = RequestGate::new(core.clone(), &intent.target);
            gate.pin = Some(action.pin.clone());
            gate.credential = Some(action.credential.clone());
            let kind = kind_of(&core, &intent.target).await?;
            write_actions::dry_run(
                &mut gate,
                &kind,
                &intent.target,
                action.change(),
                &action.reviewed,
            )
            .await
        });
        async move { task.await.unwrap_or(Err(WriteRefusal::Stopped.into())) }
    }
}

async fn kind_of(core: &CoreHandle, target: &WriteTarget) -> Result<KindInfo, WriteError> {
    let discovery = core.discovery(&target.cluster).await;
    if let Some(kind) = discovery.as_ref().and_then(|d| d.kind(&target.kind)) {
        return Ok(kind.clone());
    }
    // A set's row: the replacement server's kinds say nothing about the server it was shown on.
    if core.is_bound()
        && let Err(moved @ WriteRefusal::Moved { .. }) = core
            .write_client(&target.cluster, Some(target.server.clone()))
            .await
    {
        return Err(moved.into());
    }
    // Refused either way: a paused context has no discovery, so this only picks the words.
    let refusal = match core.bound_write_client(&target.cluster, None, None).await {
        Err(unreadable @ WriteError::KubeconfigUnreadable) => unreadable,
        Err(protected @ WriteError::Refused(WriteRefusal::Protected { .. }))
            if discovery.is_none() =>
        {
            protected
        }
        _ => return Err(WriteError::NotServed(target.kind.clone())),
    };
    // A target on a server the context no longer shows has moved, whatever else holds.
    match core
        .write_client(&target.cluster, Some(target.server.clone()))
        .await
    {
        Err(moved @ WriteRefusal::Moved { .. }) => Err(moved.into()),
        _ => Err(refusal),
    }
}

pub(crate) async fn read_live(
    client: &Client,
    kind: &KindInfo,
    target: &WriteTarget,
) -> Result<Value, WriteError> {
    let namespace = target.namespace.as_deref();
    let api = read::dynamic_api(client, kind, namespace, Some(&target.name))
        .map_err(write_actions::ActionError::from)?;
    let object = tokio::time::timeout(WRITE_TIMEOUT, api.get(&target.name))
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
            code => WriteError::Rejected(redact::safe_server_error(
                &status
                    .explanation()
                    .unwrap_or_else(|| crate::api_error::status_meaning(code, None)),
            )),
        };
    }
    WriteError::Failed(redact::safe_server_error(&error.to_string()))
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
    gitops::managed_by_live_object(live)
        .map(WriteWarning::LikelyReverted)
        .into_iter()
        .collect()
}

fn check_bound(intent: &WriteIntent) -> Result<(), WriteError> {
    match &intent.action {
        WriteAction::ApplyYaml(edit) if edit.bound == intent.target => Ok(()),
        WriteAction::Resource(action) if action.bound == intent.target => Ok(()),
        WriteAction::ApplyYaml(_) | WriteAction::Resource(_) => Err(WriteError::ReviewMismatch),
    }
}

/// Whether a missing object is created rather than refused: a manifest, for a target that
/// names no uid or version, so no object it was shown as.
fn creates_when_absent(target: &WriteTarget, proposal: &WriteProposal) -> bool {
    matches!(proposal, WriteProposal::ApplyManifest { .. })
        && target.uid.is_none()
        && target.resource_version.is_none()
}

/// The review of a manifest for an object that does not exist: a server dry run of its create.
async fn prepare_create(
    gate: &mut RequestGate,
    kind: &KindInfo,
    target: &WriteTarget,
    proposal: WriteProposal,
) -> Result<WriteAction, WriteError> {
    let WriteProposal::ApplyManifest { yaml } = proposal else {
        return Err(WriteError::Gone);
    };
    let body = edit_body(&portable_manifest(&yaml)?, kind, target)?;
    let client = gate.client().await?;
    let result = create(&client, kind, target, &body, true).await?;
    let result = serde_json::to_value(&result)
        .map_err(|e| WriteError::Failed(format!("cannot read the dry run: {e}")))?;
    let result = with_type_of(result, &body);
    Ok(WriteAction::ApplyYaml(ReviewedEdit {
        bound: target.clone(),
        pin: gate.reviewed_pin()?,
        credential: gate.reviewed_credential()?,
        before: String::new(),
        after: shown_yaml(&result)?,
        body,
        secret: false,
        creates: true,
    }))
}

/// Fields of one cluster's copy of an object, which mean nothing on another. Owner references
/// name owners by uid: kept, they would have another cluster's collector delete the object.
const CLUSTER_OWNED_METADATA: [&str; 7] = [
    "uid",
    "ownerReferences",
    "resourceVersion",
    "creationTimestamp",
    "generation",
    "managedFields",
    "selfLink",
];

/// `yaml` without the fields one cluster's copy carries, as YAML again: what a manifest for
/// several clusters keeps. Not YAML, or not an object, is left for the review to refuse.
pub fn portable_manifest(yaml: &str) -> Result<String, WriteError> {
    let options = serde_saphyr::options! { with_snippet: false };
    let Ok(mut body) = serde_saphyr::from_str_with_options::<Value>(yaml, options) else {
        return Ok(yaml.to_owned());
    };
    let Some(fields) = body.as_object_mut() else {
        return Ok(yaml.to_owned());
    };
    fields.remove("status");
    if let Some(metadata) = fields.get_mut("metadata").and_then(Value::as_object_mut) {
        for field in CLUSTER_OWNED_METADATA {
            metadata.remove(field);
        }
    }
    if is_service(&body)
        && let Some(spec) = body.get_mut("spec").and_then(Value::as_object_mut)
        // A headless Service's "None" is chosen, not allocated: every cluster keeps it.
        && spec.get("clusterIP").and_then(Value::as_str) != Some(HEADLESS)
    {
        for field in SERVICE_ALLOCATED {
            spec.remove(field);
        }
    }
    render(&body)
}

/// The `clusterIP` of a headless Service.
const HEADLESS: &str = "None";

/// What a cluster allocates a Service: another cluster's would not be free, or would be another
/// Service's.
const SERVICE_ALLOCATED: [&str; 2] = ["clusterIP", "clusterIPs"];

fn is_service(object: &Value) -> bool {
    text_at(object, "/apiVersion") == Some("v1") && text_at(object, "/kind") == Some("Service")
}

/// A manifest replacing `live` keeps what this cluster gave the object, which the manifest left
/// out ([`portable_manifest`]): its owners, and a Service's allocated addresses. Without them an
/// unchanged apply would orphan a controller's object, or ask for new addresses.
fn keep_destination_owned(body: &mut Value, live: &Value) {
    if let (Some(owners), Some(metadata)) = (
        live.pointer("/metadata/ownerReferences"),
        body.get_mut("metadata").and_then(Value::as_object_mut),
    ) && !metadata.contains_key("ownerReferences")
    {
        metadata.insert("ownerReferences".to_owned(), owners.clone());
    }
    if !is_service(body) {
        return;
    }
    let Some(spec) = body.get_mut("spec").and_then(Value::as_object_mut) else {
        return;
    };
    for field in SERVICE_ALLOCATED {
        if let Some(value) = live.pointer(&format!("/spec/{field}"))
            && !spec.contains_key(field)
        {
            spec.insert(field.to_owned(), value.clone());
        }
    }
}

async fn prepare(
    gate: &mut RequestGate,
    kind: &KindInfo,
    target: &WriteTarget,
    live: &Value,
    proposal: WriteProposal,
) -> Result<WriteAction, WriteError> {
    let (yaml, manifest) = match proposal {
        WriteProposal::ApplyYaml { yaml } => (yaml, false),
        WriteProposal::ApplyManifest { yaml } => (portable_manifest(&yaml)?, true),
        proposal => {
            // A scale reads its count where the definition says now, not where discovery said.
            let scale = match proposal {
                WriteProposal::Scale { .. } => {
                    Some(write_actions::current_scale_paths(gate, kind).await?)
                }
                _ => None,
            };
            let change = write_actions::review(kind, live, proposal, scale.as_ref())?;
            let reviewed =
                write_actions::reviewed(gate, &target.name, live, &change, scale).await?;
            return Ok(WriteAction::Resource(ReviewedAction {
                bound: target.clone(),
                pin: gate.reviewed_pin()?,
                credential: gate.reviewed_credential()?,
                change,
                reviewed,
            }));
        }
    };
    let mut body = edit_body(&yaml, kind, target)?;
    if manifest {
        keep_destination_owned(&mut body, live);
    }
    let secret = redact::is_secret_object(live);
    let result = if secret {
        let patch = secret_edit::patch(&body, live, target)?;
        let client = gate.client().await?;
        patch_object(&client, kind, target, &patch, true).await?
    } else {
        keep_live_status(&mut body, live);
        let client = gate.client().await?;
        replace(&client, kind, target, &body, true).await?
    };
    let result = serde_json::to_value(&result)
        .map_err(|e| WriteError::Failed(format!("cannot read the dry run: {e}")))?;
    let result = with_type_of(result, &body);
    Ok(WriteAction::ApplyYaml(ReviewedEdit {
        bound: target.clone(),
        pin: gate.reviewed_pin()?,
        credential: gate.reviewed_credential()?,
        before: shown_yaml(live)?,
        after: shown_after(live, &result)?,
        body,
        secret,
        creates: false,
    }))
}

async fn execute(
    gate: &mut RequestGate,
    kind: &KindInfo,
    intent: WriteIntent,
    progress: Option<async_channel::Sender<DrainProgress>>,
) -> Result<WriteOutcome, WriteError> {
    match &intent.action {
        WriteAction::ApplyYaml(edit) if edit.secret && !edit.creates => {
            // The patch is built from the Secret as it is now, which must still be the one
            // reviewed: a mask keeps that object's value, not whatever is there later.
            let client = gate.client().await?;
            let live = read_live(&client, kind, &intent.target).await?;
            let bound = bound_to_live(intent.target.clone(), &live)?;
            if bound != intent.target || !redact::is_secret_object(&live) {
                return Err(WriteError::Conflict);
            }
            let patch = secret_edit::patch(&edit.body, &live, &intent.target)?;
            let client = gate.client().await?;
            let written = patch_object(&client, kind, &intent.target, &patch, false).await?;
            Ok(WriteOutcome {
                resource_version: written.metadata.resource_version,
                drain: None,
            })
        }
        WriteAction::ApplyYaml(edit) => {
            let client = gate.client().await?;
            let written = if edit.creates {
                create(&client, kind, &intent.target, &edit.body, false).await?
            } else {
                replace(&client, kind, &intent.target, &edit.body, false).await?
            };
            Ok(WriteOutcome {
                resource_version: written.metadata.resource_version,
                drain: None,
            })
        }
        WriteAction::Resource(action) => {
            write_actions::run(
                gate,
                kind,
                &intent.target,
                action.change(),
                &action.reviewed,
                progress,
            )
            .await
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
    let api = read::dynamic_api(
        client,
        kind,
        target.namespace.as_deref(),
        Some(&target.name),
    )
    .map_err(write_actions::ActionError::from)?;
    tokio::time::timeout(WRITE_TIMEOUT, api.replace(&target.name, &params, &object))
        .await
        .map_err(|_| timed_out())?
        .map_err(write_error)
}

/// Sends a Secret's JSON Merge Patch. Its `uid` and `resourceVersion` are preconditions: a
/// different object or version is a conflict. A refusal is worded without the server's message,
/// which may quote a value ([`secret_edit::write_error`]).
async fn patch_object(
    client: &Client,
    kind: &KindInfo,
    target: &WriteTarget,
    patch: &Value,
    dry_run: bool,
) -> Result<DynamicObject, WriteError> {
    let params = PatchParams {
        dry_run,
        field_manager: Some(FIELD_MANAGER.to_owned()),
        ..PatchParams::default()
    };
    let api = read::dynamic_api(
        client,
        kind,
        target.namespace.as_deref(),
        Some(&target.name),
    )
    .map_err(write_actions::ActionError::from)?;
    tokio::time::timeout(
        WRITE_TIMEOUT,
        api.patch(&target.name, &params, &Patch::Merge(patch)),
    )
    .await
    .map_err(|_| timed_out())?
    .map_err(|error| secret_edit::write_error(error, patch))
}

/// Creates the object `body` describes, which `target` names. An object of that name by then
/// is a conflict: the server refuses the create. A Secret is refused while it holds a mask or a
/// placeholder, and its refusal is worded without the server's message, which may quote a value.
async fn create(
    client: &Client,
    kind: &KindInfo,
    target: &WriteTarget,
    body: &Value,
    dry_run: bool,
) -> Result<DynamicObject, WriteError> {
    let secret = redact::is_secret_object(body);
    if secret {
        secret_edit::check_new(body)?;
    }
    let object: DynamicObject = serde_json::from_value(body.clone())
        .map_err(|e| WriteError::InvalidYaml(format!("not a Kubernetes object: {e}")))?;
    let params = PostParams {
        dry_run,
        field_manager: Some(FIELD_MANAGER.to_owned()),
    };
    let api = read::dynamic_api(
        client,
        kind,
        target.namespace.as_deref(),
        Some(&target.name),
    )
    .map_err(write_actions::ActionError::from)?;
    tokio::time::timeout(WRITE_TIMEOUT, api.create(&params, &object))
        .await
        .map_err(|_| timed_out())?
        .map_err(|error| {
            if secret {
                secret_edit::write_error(error, body)
            } else {
                write_error(error)
            }
        })
}

fn is_secret_kind(kind: &KindInfo) -> bool {
    kind.api_version() == "v1" && kind.kind() == "Secret"
}

/// Why `yaml` did not parse. For a Secret only where: the parser's words can quote any of its
/// text, quoted or not (a duplicate key from a pasted value, say).
fn yaml_error(error: &serde_saphyr::Error, secret: bool) -> WriteError {
    if !secret {
        return WriteError::InvalidYaml(format!(
            "not valid YAML: {}",
            redact::redact_quoted(&error.to_string())
        ));
    }
    let at = error.location().map_or_else(String::new, |at| {
        format!(" at line {}, column {}", at.line(), at.column())
    });
    WriteError::InvalidYaml(format!(
        "not valid YAML{at}; the parser's message is not shown, because it may quote a Secret \
         value"
    ))
}

/// The body to `PUT` for `yaml`: the same object as `target`, at the version reviewed. An edit
/// cannot move it to another name, namespace or kind, or drop the version that makes a
/// concurrent change a conflict.
fn edit_body(yaml: &str, kind: &KindInfo, target: &WriteTarget) -> Result<Value, WriteError> {
    // No source snippet, and no quoted scalars: the text can be a Secret's.
    let options = serde_saphyr::options! { with_snippet: false };
    let mut body: Value = serde_saphyr::from_str_with_options(yaml, options)
        .map_err(|e| yaml_error(&e, is_secret_kind(kind)))?;
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
