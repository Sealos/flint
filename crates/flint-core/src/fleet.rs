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
//! To add an action: a [`FleetAction`] variant and its [`WriteProposal`]. The run loop does not
//! change.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ClusterId;
use crate::handle::{AdmittedMember, CoreHandle};
use crate::read::KindId;
use crate::write::{
    ResourceChange, WriteAction, WriteError, WriteIntent, WriteProposal, WriteTarget, WriteWarning,
};

/// What a fleet action does to the object on each cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FleetAction {
    RolloutRestart,
    Scale { to: u32 },
}

impl FleetAction {
    fn proposal(self) -> WriteProposal {
        match self {
            FleetAction::RolloutRestart => WriteProposal::RolloutRestart,
            FleetAction::Scale { to } => WriteProposal::Scale { to },
        }
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
}

/// An action, its object, and the admitted members it targets, in run order.
#[derive(Debug, Clone)]
pub struct FleetPlan {
    action: FleetAction,
    object: FleetObject,
    members: Vec<AdmittedMember>,
}

impl FleetPlan {
    pub fn new(
        action: FleetAction,
        object: FleetObject,
        members: Vec<AdmittedMember>,
    ) -> Result<Self, FleetPlanError> {
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
        })
    }

    pub fn action(&self) -> FleetAction {
        self.action
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

    pub fn is_dropped(&self) -> bool {
        self.dropped
    }
}

/// A plan's per-cluster review and server dry run. Only [`CoreHandle::preview_fleet`] makes one,
/// and a run consumes it.
#[derive(Debug)]
pub struct FleetPreview {
    action: FleetAction,
    object: FleetObject,
    entries: Vec<FleetPreviewEntry>,
}

impl FleetPreview {
    pub fn action(&self) -> FleetAction {
        self.action
    }

    pub fn object(&self) -> &FleetObject {
        &self.object
    }

    pub fn entries(&self) -> &[FleetPreviewEntry] {
        &self.entries
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
}

/// How one cluster of a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FleetStatus {
    Done,
    Failed(WriteError),
    Skipped(FleetSkip),
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
            } = plan;
            let mut entries = Vec::with_capacity(members.len());
            for member in members {
                let review = review_member(&core, &member, action, &object).await;
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
            }
        }
    }

    /// Runs `preview` one cluster at a time, stopping at the first failure. `on_result` hears
    /// each cluster as it ends. A cluster refused in the preview and not dropped fails at its
    /// turn, with no request. Writes are never retried.
    pub fn run_fleet(
        &self,
        preview: FleetPreview,
        cancel: FleetCancel,
        on_result: impl FnMut(&FleetResult) + Send + 'static,
    ) -> impl Future<Output = FleetReport> + Send + 'static {
        let core = self.clone();
        let steps = preview
            .entries
            .into_iter()
            .map(|entry| Step {
                cluster: entry.member.context().to_owned(),
                dropped: entry.dropped,
                work: (entry.member, entry.review),
            })
            .collect();
        drive(steps, cancel, on_result, move |(member, review)| {
            let core = core.clone();
            async move {
                let reviewed = review?;
                core.bound_to(member)
                    .run_write(reviewed.intent)
                    .await
                    .map(drop)
            }
        })
    }
}

/// The review and server dry run of `action` on one member, on a handle bound to it.
async fn review_member(
    core: &CoreHandle,
    member: &AdmittedMember,
    action: FleetAction,
    object: &FleetObject,
) -> Result<ReviewedMember, WriteError> {
    let bound = core.bound_to(member.clone());
    let target = WriteTarget {
        cluster: member.context().to_owned(),
        server: Some(member.server().to_owned()),
        kind: object.kind.clone(),
        namespace: object.namespace.clone(),
        name: object.name.clone(),
        uid: None,
        resource_version: None,
    };
    let review = bound.review_write(target, action.proposal()).await?;
    let warnings = review.warnings().to_vec();
    let intent = review.into_intent().holding_spec();
    bound.dry_run_write(intent.clone()).await?;
    Ok(ReviewedMember { intent, warnings })
}

/// One cluster's turn in [`drive`].
struct Step<W> {
    cluster: ClusterId,
    dropped: bool,
    work: W,
}

/// The run loop: knows nothing about what a step does. After a failure or a cancel, no later
/// step's work starts.
async fn drive<W, F>(
    steps: Vec<Step<W>>,
    cancel: FleetCancel,
    mut on_result: impl FnMut(&FleetResult),
    mut run: impl FnMut(W) -> F,
) -> FleetReport
where
    F: Future<Output = Result<(), WriteError>>,
{
    let mut results = Vec::with_capacity(steps.len());
    let mut failed = false;
    for step in steps {
        let status = if step.dropped {
            FleetStatus::Skipped(FleetSkip::Dropped)
        } else if failed {
            FleetStatus::Skipped(FleetSkip::AfterFailure)
        } else if cancel.is_cancelled() {
            FleetStatus::Skipped(FleetSkip::Cancelled)
        } else {
            match run(step.work).await {
                Ok(()) => FleetStatus::Done,
                Err(error) => {
                    failed = true;
                    FleetStatus::Failed(error)
                }
            }
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
