//! Deployment rollouts for the dashboard's recent activity.
//!
//! A rollout is read from a Deployment's own status, so the dashboard needs no watch beyond the
//! one the telemetry feed already keeps. Each Deployment is at most one rollout, which moves
//! through [`RolloutState`] as its status does: one evolving entry, never a line per update.
//!
//! A fleet run waiting for a member's rollout reads the same status through [`rollout_wait`],
//! which also knows StatefulSets and DaemonSets.

use k8s_openapi::api::apps::v1::{
    DaemonSet, Deployment, DeploymentCondition, DeploymentStatus, StatefulSet,
};
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use serde_json::Value;

use crate::dashboard::WARNING_EVENT_WINDOW;
use crate::read::KindId;

/// How long a finished rollout stays listed, as long as a Warning event does.
pub const COMPLETE_WINDOW: SignedDuration = WARNING_EVENT_WINDOW;

const DEFAULT_REPLICAS: i32 = 1;
const PROGRESSING: &str = "Progressing";
const NEW_REPLICA_SET_AVAILABLE: &str = "NewReplicaSetAvailable";
const PROGRESS_DEADLINE_EXCEEDED: &str = "ProgressDeadlineExceeded";

/// Where a rollout stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutState {
    /// The change has been made and no replica of the new revision is up yet.
    Started,
    /// Some replicas of the new revision are up, and the rest are on their way.
    Progressing,
    /// Every replica is on the new revision and available.
    Complete,
    /// The controller gave up waiting for progress (`ProgressDeadlineExceeded`).
    Stalled,
}

impl RolloutState {
    /// The state as the dashboard words it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Started => "Rollout started",
            Self::Progressing => "Rollout progressing",
            Self::Complete => "Rollout complete",
            Self::Stalled => "Rollout stalled",
        }
    }
}

/// One Deployment's rollout. Text from the cluster is not cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollout {
    pub namespace: String,
    pub name: String,
    pub uid: Option<String>,
    pub state: RolloutState,
    /// When the state last changed, as far as the status says. `None` for a rollout the
    /// controller has not observed yet, which is happening now.
    pub at: Option<Timestamp>,
    pub desired: i32,
    pub updated: i32,
    pub available: i32,
    /// The controller's own words for a stalled rollout.
    pub message: Option<String>,
}

impl Rollout {
    /// The rollout in a few words: how far it has got, or why it stalled.
    pub fn summary(&self) -> String {
        match (self.state, &self.message) {
            (RolloutState::Stalled, Some(message)) => message.clone(),
            (RolloutState::Complete, _) => {
                format!("{} of {} available", self.available, self.desired)
            }
            _ => format!(
                "{} of {} updated, {} available",
                self.updated, self.desired, self.available
            ),
        }
    }
}

/// The rollout `deployment` is in, if it is in one. A Complete rollout older than
/// [`COMPLETE_WINDOW`] at `now` is no longer news, and a paused or never rolled Deployment has
/// none.
pub fn rollout_of(deployment: &Deployment, now: Timestamp) -> Option<Rollout> {
    let meta = &deployment.metadata;
    let spec = &deployment.spec;
    let status = deployment.status.as_ref()?;
    let name = meta.name.clone().filter(|name| !name.is_empty())?;
    let Reading {
        desired,
        updated,
        available,
        total,
        caught_up,
        progressing,
        stalled,
        observed,
    } = Reading::of(deployment, status);
    let stamp = progressing.and_then(|c| {
        c.last_update_time
            .as_ref()
            .or(c.last_transition_time.as_ref())
            .map(|t| t.0)
    });
    let rollout = |state, at, message| Rollout {
        namespace: meta.namespace.clone().unwrap_or_default(),
        name: name.clone(),
        uid: meta.uid.clone().filter(|uid| !uid.is_empty()),
        state,
        at,
        desired,
        updated,
        available,
        message,
    };

    let reason = progressing.and_then(|c| c.reason.as_deref());
    let stopped = progressing.is_some_and(|c| c.status.as_deref() == Some("False"));
    if stalled {
        let message = progressing.and_then(|c| c.message.clone());
        return Some(rollout(RolloutState::Stalled, stamp, message));
    }
    if spec.paused == Some(true) {
        return None;
    }
    if !observed {
        return Some(rollout(RolloutState::Started, None, None));
    }
    if caught_up && reason == Some(NEW_REPLICA_SET_AVAILABLE) {
        let at = stamp?;
        return (now.duration_since(at) < COMPLETE_WINDOW)
            .then(|| rollout(RolloutState::Complete, Some(at), None));
    }
    // Without a condition (no progress deadline) only the replica counts say a rollout is moving.
    let moving = match progressing {
        Some(_) => !stopped && reason != Some(NEW_REPLICA_SET_AVAILABLE),
        None => !caught_up && (updated < desired || total > updated),
    };
    if !moving {
        return None;
    }
    // All replicas up but the controller has not said so yet: still the rollout, about to finish.
    let state = if updated == 0 {
        RolloutState::Started
    } else {
        RolloutState::Progressing
    };
    Some(rollout(state, stamp, None))
}

/// A Deployment's status read against its spec: what the dashboard's rollout and a wait for one
/// ([`rollout_wait`]) are both judged by.
struct Reading<'a> {
    desired: i32,
    updated: i32,
    available: i32,
    total: i32,
    /// Every replica asked for is on the new revision and available, and no old one is left.
    caught_up: bool,
    progressing: Option<&'a DeploymentCondition>,
    /// The controller gave up waiting for progress (`ProgressDeadlineExceeded`) before the
    /// rollout caught up.
    stalled: bool,
    /// The controller has seen the latest change to the spec.
    observed: bool,
}

impl<'a> Reading<'a> {
    fn of(deployment: &'a Deployment, status: &'a DeploymentStatus) -> Self {
        let desired = deployment.spec.replicas.unwrap_or(DEFAULT_REPLICAS);
        let updated = status.updated_replicas.unwrap_or(0);
        let available = status.available_replicas.unwrap_or(0);
        let total = status.replicas.unwrap_or(0);
        let caught_up = available >= desired && updated >= desired && total <= updated;
        let progressing = status
            .conditions
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|c| c.type_.as_deref() == Some(PROGRESSING));
        let stalled = !caught_up
            && progressing.is_some_and(|c| {
                c.status.as_deref() == Some("False")
                    && c.reason.as_deref() == Some(PROGRESS_DEADLINE_EXCEEDED)
            });
        let observed = match (status.observed_generation, deployment.metadata.generation) {
            (Some(observed), Some(generation)) => observed >= generation,
            _ => true,
        };
        Self {
            desired,
            updated,
            available,
            total,
            caught_up,
            progressing,
            stalled,
            observed,
        }
    }
}

/// Where a workload's rollout stands for someone waiting for it to finish, as `kubectl rollout
/// status` judges it. Text from the cluster is not cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolloutWait {
    /// Still on its way, and how far it has got.
    Waiting(String),
    /// Every replica asked for runs the latest spec and is available.
    Finished,
    /// The controller gave up, in its own words.
    Failed(String),
}

/// Where `object`'s rollout stands, if `kind` has rollouts: Deployments, StatefulSets and
/// DaemonSets ([`crate::action_kinds::restarts`]). `None` for any other kind, and for an object
/// that is not its kind. A StatefulSet or DaemonSet updated `OnDelete` is waited for until its
/// pods are up, not until they run the new revision.
pub fn rollout_wait(kind: &KindId, object: &Value) -> Option<RolloutWait> {
    if !crate::action_kinds::restarts(kind) {
        return None;
    }
    match kind.plural() {
        "deployments" => deployment_wait(&serde_json::from_value(object.clone()).ok()?),
        "statefulsets" => stateful_set_wait(&serde_json::from_value(object.clone()).ok()?),
        "daemonsets" => daemon_set_wait(&serde_json::from_value(object.clone()).ok()?),
        _ => None,
    }
}

const NOT_SEEN: &str = "the controller has not seen the change yet";

/// Judged only once the controller has seen the latest spec: a failure or a success it reports
/// before that is about an earlier change.
fn deployment_wait(deployment: &Deployment) -> Option<RolloutWait> {
    let Some(status) = deployment.status.as_ref() else {
        return Some(RolloutWait::Waiting(NOT_SEEN.to_owned()));
    };
    if !has_observed(deployment.metadata.generation, status.observed_generation) {
        return Some(RolloutWait::Waiting(NOT_SEEN.to_owned()));
    }
    let reading = Reading::of(deployment, status);
    Some(if reading.stalled {
        let message = reading.progressing.and_then(|c| c.message.clone());
        RolloutWait::Failed(message.unwrap_or_else(|| "progress deadline exceeded".to_owned()))
    } else if reading.caught_up && reading.total <= reading.desired {
        // Also no replica beyond those asked for: a scale-down has finished too.
        RolloutWait::Finished
    } else {
        RolloutWait::Waiting(format!(
            "{} of {} updated, {} available",
            reading.updated, reading.desired, reading.available
        ))
    })
}

/// Whether the controller has seen generation `generation` of the spec.
fn has_observed(generation: Option<i64>, observed: Option<i64>) -> bool {
    match (observed, generation) {
        (Some(observed), Some(generation)) => observed >= generation,
        (None, Some(_)) => false,
        _ => true,
    }
}

/// Whether a StatefulSet or DaemonSet replaces its pods only when they are deleted. Its pod
/// count is still waited for (a create or a scale), its revision is not.
fn on_delete(strategy: Option<&str>) -> bool {
    strategy == Some("OnDelete")
}

fn stateful_set_wait(set: &StatefulSet) -> Option<RolloutWait> {
    let spec = &set.spec;
    let strategy = spec.update_strategy.as_ref();
    let Some(status) = set.status.as_ref() else {
        return Some(RolloutWait::Waiting(NOT_SEEN.to_owned()));
    };
    if !has_observed(set.metadata.generation, status.observed_generation) {
        return Some(RolloutWait::Waiting(NOT_SEEN.to_owned()));
    }
    let desired = spec.replicas.unwrap_or(DEFAULT_REPLICAS);
    let ready = status.ready_replicas.unwrap_or(0);
    // A server without the field counts ready pods only.
    let available = status.available_replicas.unwrap_or(ready);
    if ready < desired || available < desired || status.replicas > desired {
        return Some(RolloutWait::Waiting(format!(
            "{} of {desired} ready, {available} available",
            ready.min(status.replicas)
        )));
    }
    if on_delete(strategy.and_then(|s| s.type_.as_deref())) {
        return Some(RolloutWait::Finished);
    }
    let updated = status.updated_replicas.unwrap_or(0);
    let partition = strategy
        .and_then(|s| s.rolling_update.as_ref())
        .and_then(|r| r.partition)
        .unwrap_or(0);
    if partition > 0 {
        let wanted = (desired - partition).max(0);
        return Some(if updated < wanted {
            RolloutWait::Waiting(format!("{updated} of {wanted} updated"))
        } else {
            RolloutWait::Finished
        });
    }
    Some(if status.update_revision != status.current_revision {
        RolloutWait::Waiting(format!("{updated} of {desired} updated"))
    } else {
        RolloutWait::Finished
    })
}

fn daemon_set_wait(set: &DaemonSet) -> Option<RolloutWait> {
    let strategy = set.spec.update_strategy.as_ref();
    let Some(status) = set.status.as_ref() else {
        return Some(RolloutWait::Waiting(NOT_SEEN.to_owned()));
    };
    if !has_observed(set.metadata.generation, status.observed_generation) {
        return Some(RolloutWait::Waiting(NOT_SEEN.to_owned()));
    }
    let desired = status.desired_number_scheduled;
    let updated = if on_delete(strategy.and_then(|s| s.type_.as_deref())) {
        desired
    } else {
        status.updated_number_scheduled.unwrap_or(0)
    };
    let available = status.number_available.unwrap_or(0);
    Some(if updated < desired || available < desired {
        RolloutWait::Waiting(format!(
            "{updated} of {desired} updated, {available} available"
        ))
    } else {
        RolloutWait::Finished
    })
}

/// The rollouts of `deployments`, one per Deployment, in no particular order.
pub fn rollouts_of(deployments: &[Deployment], now: Timestamp) -> Vec<Rollout> {
    deployments
        .iter()
        .filter_map(|d| rollout_of(d, now))
        .collect()
}

#[cfg(test)]
#[path = "rollout_tests.rs"]
mod tests;
