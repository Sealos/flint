//! Fleet actions: one change to the same object on several clusters of a set (#82, ADR-0015).
//!
//! A [`FleetPlan`] names the action, the object (by kind, namespace and name) and an explicit
//! list of admitted members. [`CoreHandle::preview_fleet`] reviews and server-dry-runs it on each
//! member and returns a [`FleetPreview`], from which a caller can drop clusters.
//! [`CoreHandle::run_fleet`] then runs it one cluster at a time and stops at the first failure.
//!
//! This module holds no client and decides nothing about access. Each request goes through
//! [`CoreHandle::review_write`], `CoreHandle::dry_run_write` or [`CoreHandle::run_write`] on a
//! handle bound to the member, so the write gate is asked immediately before it. A preview
//! grants nothing: a member that is locked, changed or provisional at its turn is refused there
//! and stops the run.
//!
//! Apply YAML sends one manifest to every member: each member's review is a server dry run of
//! replacing its object, or of creating it where it does not exist. Delete is reviewed and
//! dry-run per member, bound to that member's uid and `resourceVersion`: a member whose object
//! changed or went away since stops the run at its turn.
//!
//! A preview whose refused members are not all dropped does not run at all
//! ([`FleetPreview::is_ready`]).
//!
//! After a member's write to a kind with rollouts (Deployments, StatefulSets, DaemonSets), the
//! run waits for that rollout ([`crate::rollout::rollout_wait`]) before the next member. A
//! rollout that fails or does not finish within the plan's timeout fails the member and stops
//! the run. The wait only reads, each time through the gate the write went through: the same
//! member, pin and credential identity, and refused after a disconnect. A delete waits for
//! nothing.
//!
//! To add an action: a [`FleetAction`] variant and its [`WriteProposal`]. The run loop does not
//! change.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::ClusterId;
use crate::api_path::validate_names;
use crate::handle::{AdmittedMember, CoreHandle};
use crate::read::KindId;
use crate::rollout::{RolloutWait, rollout_wait};
use crate::write::{
    ResourceChange, ReviewedEdit, WriteAction, WriteError, WriteIntent, WriteProposal, WriteTarget,
    WriteWarning,
};

/// What a fleet action does to the object on each cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FleetAction {
    RolloutRestart,
    Scale {
        to: u32,
    },
    /// Replace the object with this manifest on each cluster, or create it where it is missing.
    ApplyYaml {
        manifest: FleetManifest,
    },
    Delete,
}

impl FleetAction {
    fn proposal(&self) -> WriteProposal {
        match self {
            FleetAction::RolloutRestart => WriteProposal::RolloutRestart,
            FleetAction::Scale { to } => WriteProposal::Scale { to: *to },
            FleetAction::ApplyYaml { manifest } => WriteProposal::ApplyManifest {
                yaml: manifest.0.to_string(),
            },
            FleetAction::Delete => WriteProposal::Delete,
        }
    }
}

/// The YAML an apply sends to every member. Never printed: it can hold anything.
#[derive(Clone, PartialEq, Eq)]
pub struct FleetManifest(Arc<str>);

impl FleetManifest {
    pub fn new(yaml: impl Into<Arc<str>>) -> Self {
        Self(yaml.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for FleetManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FleetManifest(..)")
    }
}

/// The object a fleet action is for, matched by these three on each cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetObject {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
}

/// Why a plan cannot be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FleetPlanError {
    #[error("a fleet action needs at least one cluster")]
    NoTargets,
    #[error("\"{0}\" is in the target list twice")]
    Duplicate(ClusterId),
    #[error("the object's name or namespace is not valid")]
    InvalidObject,
    #[error("\"{0}\" is not in the target list")]
    NotAMember(ClusterId),
}

/// An action, its object, and the admitted members it targets, in run order.
#[derive(Debug, Clone)]
pub struct FleetPlan {
    action: FleetAction,
    object: FleetObject,
    members: Vec<AdmittedMember>,
    /// The uid of the object as shown on a member, which binds that member's review to it.
    shown: Option<(ClusterId, String)>,
    rollout_timeout: Duration,
}

impl FleetPlan {
    pub fn new(
        action: FleetAction,
        object: FleetObject,
        members: Vec<AdmittedMember>,
    ) -> Result<Self, FleetPlanError> {
        // A fleet plan names one object for every member, so it keeps the stricter DNS-1123 rule.
        validate_names(object.namespace.as_deref(), Some(&object.name))
            .map_err(|_| FleetPlanError::InvalidObject)?;
        if !crate::api_path::is_dns1123_subdomain(&object.name) {
            return Err(FleetPlanError::InvalidObject);
        }
        if members.is_empty() {
            return Err(FleetPlanError::NoTargets);
        }
        for (index, member) in members.iter().enumerate() {
            if members[..index]
                .iter()
                .any(|earlier| earlier.context() == member.context())
            {
                return Err(FleetPlanError::Duplicate(member.context().to_owned()));
            }
        }
        Ok(Self {
            action,
            object,
            members,
            shown: None,
            rollout_timeout: ROLLOUT_TIMEOUT,
        })
    }

    /// How long a run waits for each member's rollout before that member fails
    /// ([`ROLLOUT_TIMEOUT`] unless set).
    pub fn with_rollout_timeout(mut self, timeout: Duration) -> Self {
        self.rollout_timeout = timeout;
        self
    }

    /// Binds `cluster`'s review to the object shown there, by its uid: another object of that
    /// name there by then is refused, and an apply does not create it again. The other members
    /// are matched by kind, namespace and name.
    pub fn with_shown_uid(mut self, cluster: &str, uid: &str) -> Result<Self, FleetPlanError> {
        if uid.is_empty() {
            return Err(FleetPlanError::InvalidObject);
        }
        if !self.members.iter().any(|m| m.context() == cluster) {
            return Err(FleetPlanError::NotAMember(cluster.to_owned()));
        }
        self.shown = Some((cluster.to_owned(), uid.to_owned()));
        Ok(self)
    }

    pub fn action(&self) -> &FleetAction {
        &self.action
    }

    pub fn object(&self) -> &FleetObject {
        &self.object
    }
}

/// A member's accepted review: what its run will send.
#[derive(Debug)]
struct ReviewedMember {
    intent: WriteIntent,
    warnings: Vec<WriteWarning>,
}

/// One cluster of a preview.
#[derive(Debug)]
pub struct FleetPreviewEntry {
    member: AdmittedMember,
    review: Result<ReviewedMember, WriteError>,
    dropped: bool,
}

impl FleetPreviewEntry {
    pub fn cluster(&self) -> &str {
        self.member.context()
    }

    pub fn server(&self) -> &str {
        self.member.server()
    }

    /// Why the review or the server dry run refused this cluster, if it did.
    pub fn refusal(&self) -> Option<&WriteError> {
        self.review.as_ref().err()
    }

    /// What the confirmation must say for this cluster, read from its live object.
    pub fn warnings(&self) -> &[WriteWarning] {
        self.review
            .as_ref()
            .map_or(&[], |reviewed| reviewed.warnings.as_slice())
    }

    /// What the action changes on this cluster, if its review was accepted.
    pub fn change(&self) -> Option<&ResourceChange> {
        match &self.review.as_ref().ok()?.intent.action {
            WriteAction::Resource(action) => Some(action.change()),
            _ => None,
        }
    }

    /// What an apply does on this cluster, if its review was accepted: whether it creates the
    /// object, and the object before and after, as its server dry run answered.
    pub fn edit(&self) -> Option<&ReviewedEdit> {
        match &self.review.as_ref().ok()?.intent.action {
            WriteAction::ApplyYaml(edit) => Some(edit),
            _ => None,
        }
    }

    pub fn is_dropped(&self) -> bool {
        self.dropped
    }

    /// Whether a run would ask the write gate for this cluster: it is neither dropped nor
    /// refused. The gate still decides at its turn.
    pub fn will_run(&self) -> bool {
        !self.dropped && self.review.is_ok()
    }
}

/// A plan's per-cluster review and server dry run. Only [`CoreHandle::preview_fleet`] makes one,
/// and a run consumes it.
#[derive(Debug)]
pub struct FleetPreview {
    action: FleetAction,
    object: FleetObject,
    entries: Vec<FleetPreviewEntry>,
    rollout_timeout: Duration,
}

impl FleetPreview {
    pub fn action(&self) -> &FleetAction {
        &self.action
    }

    pub fn object(&self) -> &FleetObject {
        &self.object
    }

    pub fn entries(&self) -> &[FleetPreviewEntry] {
        &self.entries
    }

    /// Takes every cluster whose review or dry run was refused out of the run, and returns them
    /// in run order. Left in, a refused cluster would fail at its turn and stop the run there;
    /// dropped, it gets no request and the others still run.
    pub fn drop_refused(&mut self) -> Vec<ClusterId> {
        self.entries
            .iter_mut()
            .filter(|entry| entry.review.is_err())
            .map(|entry| {
                entry.dropped = true;
                entry.cluster().to_owned()
            })
            .collect()
    }

    /// The clusters whose review or dry run was refused and that are still in the run, in run
    /// order. While there is one, the preview does not run.
    pub fn unresolved(&self) -> Vec<ClusterId> {
        self.entries
            .iter()
            .filter(|entry| !entry.dropped && entry.review.is_err())
            .map(|entry| entry.cluster().to_owned())
            .collect()
    }

    /// Whether a run would start: every refused cluster is dropped, and one is left to run.
    pub fn is_ready(&self) -> bool {
        self.unresolved().is_empty() && self.entries.iter().any(FleetPreviewEntry::will_run)
    }

    /// Takes `cluster` out of the run, or puts it back. Returns whether the preview has it.
    pub fn set_dropped(&mut self, cluster: &str, dropped: bool) -> bool {
        match self.entries.iter_mut().find(|e| e.cluster() == cluster) {
            Some(entry) => {
                entry.dropped = dropped;
                true
            }
            None => false,
        }
    }
}

/// Stops a run before its next cluster. Clones share the same flag.
#[derive(Debug, Clone, Default)]
pub struct FleetCancel(Arc<AtomicBool>);

impl FleetCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Why a cluster was not tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FleetSkip {
    /// The caller dropped it from the preview.
    Dropped,
    /// An earlier cluster failed.
    AfterFailure,
    /// The run was cancelled before its turn.
    Cancelled,
    /// The run did not start: a cluster refused in the preview was not dropped.
    NotStarted,
}

/// Why a member whose write was accepted failed while its rollout was awaited.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RolloutFailure {
    /// The controller gave up (`ProgressDeadlineExceeded`), in its own words.
    #[error("the rollout failed: {0}")]
    Failed(String),
    #[error("the rollout did not finish within {}", wait_text(*.0))]
    TimedOut(Duration),
    /// The object could not be read again: refused by the gate, gone, replaced or unanswered.
    #[error("the rollout could not be checked: {0}")]
    Unread(WriteError),
}

fn wait_text(timeout: Duration) -> String {
    let seconds = timeout.as_secs();
    if seconds >= 60 && seconds % 60 == 0 {
        let minutes = seconds / 60;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    } else {
        format!("{} s", timeout.as_secs_f32())
    }
}

/// How one cluster of a run ended, or, for [`FleetStatus::WaitingForRollout`], where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FleetStatus {
    /// Written, with no rollout to wait for (a delete, or a kind without rollout status).
    Done,
    /// Written, and the run waits for its rollout before the next cluster. Not an end: heard
    /// by `on_result` before the cluster's end, never in a [`FleetReport`].
    WaitingForRollout,
    /// Written, and its rollout finished.
    RolledOut,
    Failed(WriteError),
    /// Written, but its rollout failed or did not finish in time. Stops the run.
    RolloutFailed(RolloutFailure),
    /// Written, but the run was cancelled before its rollout finished.
    RolloutNotAwaited,
    Skipped(FleetSkip),
}

impl FleetStatus {
    /// Whether this cluster stops the run.
    fn stops_run(&self) -> bool {
        matches!(self, Self::Failed(_) | Self::RolloutFailed(_))
    }
}

/// One cluster's line of a [`FleetReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetResult {
    pub cluster: ClusterId,
    pub status: FleetStatus,
}

/// Every cluster of a run, in run order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetReport {
    pub results: Vec<FleetResult>,
}

impl CoreHandle {
    /// Reviews and server-dry-runs `plan` on each member, one at a time. A refusal is recorded
    /// on that member and does not stop the others: nothing here changes a cluster.
    pub fn preview_fleet(
        &self,
        plan: FleetPlan,
    ) -> impl Future<Output = FleetPreview> + Send + 'static {
        let core = self.clone();
        async move {
            let FleetPlan {
                action,
                object,
                members,
                shown,
                rollout_timeout,
            } = plan;
            let mut entries = Vec::with_capacity(members.len());
            for member in members {
                let uid = shown
                    .as_ref()
                    .filter(|(cluster, _)| cluster == member.context())
                    .map(|(_, uid)| uid.clone());
                let review = review_member(&core, &member, &action, &object, uid).await;
                entries.push(FleetPreviewEntry {
                    member,
                    review,
                    dropped: false,
                });
            }
            FleetPreview {
                action,
                object,
                entries,
                rollout_timeout,
            }
        }
    }

    /// Runs `preview` one cluster at a time, stopping at the first failure. After a write to a
    /// kind with rollouts, the next cluster starts only once that rollout has finished; a failed
    /// or timed-out rollout is a failure. `on_result` hears each cluster as it ends, and before
    /// that as [`FleetStatus::WaitingForRollout`] while its rollout is awaited. While a cluster
    /// refused in the preview is not dropped, nothing runs: that cluster fails with its refusal
    /// and every other is not started. Writes are never retried.
    pub fn run_fleet(
        &self,
        preview: FleetPreview,
        cancel: FleetCancel,
        on_result: impl FnMut(&FleetResult) + Send + 'static,
    ) -> impl Future<Output = FleetReport> + Send + 'static {
        let core = self.clone();
        let blocked = !preview.unresolved().is_empty();
        let waits = preview.action != FleetAction::Delete
            && crate::action_kinds::restarts(&preview.object.kind);
        let kind = preview.object.kind.clone();
        let timeout = preview.rollout_timeout;
        let steps = preview
            .entries
            .into_iter()
            .map(|entry| Step {
                cluster: entry.member.context().to_owned(),
                dropped: entry.dropped,
                blocked: blocked && entry.review.is_ok(),
                work: (entry.member, entry.review),
            })
            .collect();
        let waiting = cancel.clone();
        drive(steps, cancel, on_result, move |(member, review)| {
            let core = core.clone();
            let cancel = waiting.clone();
            let kind = kind.clone();
            async move {
                let reviewed = review?;
                let bound = core.bound_to(member);
                let intent = reviewed.intent;
                // Stamped before the write: a disconnect asked for from here on ends the wait.
                let disconnects = bound.disconnects_of(&intent.target.cluster);
                bound.run_write(intent.clone()).await?;
                if !waits {
                    return Ok(None);
                }
                let wait = Rollout {
                    core: bound,
                    intent,
                    kind,
                    disconnects,
                    cancel,
                    timeout,
                };
                Ok(Some(wait.awaited()))
            }
        })
    }
}

/// How long a run waits for one member's rollout unless its plan says otherwise: a Deployment's
/// default progress deadline, after which its controller reports the rollout failed.
pub const ROLLOUT_TIMEOUT: Duration = Duration::from_secs(600);

/// How often a waiting run reads the object again, at most.
const ROLLOUT_POLL: Duration = Duration::from_secs(2);

/// How often a waiting run looks for a cancel between reads.
const CANCEL_TICK: Duration = Duration::from_millis(100);

/// One member's rollout, awaited after its write.
struct Rollout {
    /// Bound to the member.
    core: CoreHandle,
    /// The write, whose gate every read goes through.
    intent: WriteIntent,
    kind: KindId,
    /// [`CoreHandle::disconnects_of`] the member before the write.
    disconnects: u64,
    cancel: FleetCancel,
    timeout: Duration,
}

impl Rollout {
    /// Reads the object until its rollout finishes, fails, times out, or the run is cancelled.
    /// Sends nothing but reads, on the core's runtime.
    async fn awaited(self) -> FleetStatus {
        let core = self.core.clone();
        let ended = core.on_runtime(self.run()).await;
        ended.unwrap_or(FleetStatus::RolloutFailed(RolloutFailure::Unread(
            crate::WriteRefusal::Stopped.into(),
        )))
    }

    async fn run(self) -> FleetStatus {
        use tokio::time::Instant;
        let failed = FleetStatus::RolloutFailed;
        let deadline = Instant::now() + self.timeout;
        let poll = ROLLOUT_POLL.min(self.timeout / 4).max(CANCEL_TICK);
        let mut uid = self.intent.target.uid.clone();
        loop {
            if self.cancel.is_cancelled() {
                return FleetStatus::RolloutNotAwaited;
            }
            if Instant::now() >= deadline {
                return failed(RolloutFailure::TimedOut(self.timeout));
            }
            let read = self.core.read_written(&self.intent, self.disconnects);
            let object = match tokio::time::timeout_at(deadline, read).await {
                Err(_) => return failed(RolloutFailure::TimedOut(self.timeout)),
                Ok(Err(error)) => return failed(RolloutFailure::Unread(error)),
                Ok(Ok(object)) => object,
            };
            // The object written, not one made since under its name.
            let live = object.pointer("/metadata/uid").and_then(Value::as_str);
            match (uid.as_deref(), live) {
                (Some(written), Some(live)) if written == live => {}
                (None, Some(live)) => uid = Some(live.to_owned()),
                _ => return failed(RolloutFailure::Unread(WriteError::Replaced)),
            }
            match rollout_wait(&self.kind, &object) {
                None => return FleetStatus::Done,
                Some(RolloutWait::Finished) => return FleetStatus::RolledOut,
                Some(RolloutWait::Failed(why)) => return failed(RolloutFailure::Failed(why)),
                Some(RolloutWait::Waiting(_)) => {}
            }
            let next = (Instant::now() + poll).min(deadline);
            while Instant::now() < next && !self.cancel.is_cancelled() {
                tokio::time::sleep(CANCEL_TICK.min(next - Instant::now())).await;
            }
        }
    }
}

/// The review and server dry run of `action` on one member, on a handle bound to it.
async fn review_member(
    core: &CoreHandle,
    member: &AdmittedMember,
    action: &FleetAction,
    object: &FleetObject,
    uid: Option<String>,
) -> Result<ReviewedMember, WriteError> {
    let bound = core.bound_to(member.clone());
    let target = WriteTarget {
        cluster: member.context().to_owned(),
        server: Some(member.server().to_owned()),
        kind: object.kind.clone(),
        namespace: object.namespace.clone(),
        name: object.name.clone(),
        uid,
        resource_version: None,
    };
    let review = bound.review_write(target, action.proposal()).await?;
    // An edit's review was its server dry run; an action's comes next, holding its object.
    let warnings = review.warnings().to_vec();
    let intent = review.into_intent().holding_spec();
    bound.dry_run_write(intent.clone()).await?;
    Ok(ReviewedMember { intent, warnings })
}

/// One cluster's turn in [`drive`].
struct Step<W> {
    cluster: ClusterId,
    dropped: bool,
    /// The run does not start: another cluster's refusal was not dropped.
    blocked: bool,
    work: W,
}

/// The run loop: knows nothing about what a step does. A step's work that leaves a wait
/// (its rollout) is heard as waiting, then ends when the wait does. After a failure or a cancel,
/// no later step's work starts.
async fn drive<W, F, A>(
    steps: Vec<Step<W>>,
    cancel: FleetCancel,
    mut on_result: impl FnMut(&FleetResult),
    mut run: impl FnMut(W) -> F,
) -> FleetReport
where
    F: Future<Output = Result<Option<A>, WriteError>>,
    A: Future<Output = FleetStatus>,
{
    let mut results = Vec::with_capacity(steps.len());
    let mut failed = false;
    for step in steps {
        let status = if step.dropped {
            FleetStatus::Skipped(FleetSkip::Dropped)
        } else if step.blocked {
            FleetStatus::Skipped(FleetSkip::NotStarted)
        } else if failed {
            FleetStatus::Skipped(FleetSkip::AfterFailure)
        } else if cancel.is_cancelled() {
            FleetStatus::Skipped(FleetSkip::Cancelled)
        } else {
            let status = match run(step.work).await {
                Ok(None) => FleetStatus::Done,
                Ok(Some(wait)) => {
                    on_result(&FleetResult {
                        cluster: step.cluster.clone(),
                        status: FleetStatus::WaitingForRollout,
                    });
                    wait.await
                }
                Err(error) => FleetStatus::Failed(error),
            };
            failed = status.stops_run();
            status
        };
        let result = FleetResult {
            cluster: step.cluster,
            status,
        };
        on_result(&result);
        results.push(result);
    }
    FleetReport { results }
}

#[cfg(test)]
#[path = "fleet_tests.rs"]
mod tests;
