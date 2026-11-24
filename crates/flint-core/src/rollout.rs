//! Deployment rollouts for the dashboard's recent activity.
//!
//! A rollout is read from a Deployment's own status, so the dashboard needs no watch beyond the
//! one the telemetry feed already keeps. Each Deployment is at most one rollout, which moves
//! through [`RolloutState`] as its status does: one evolving entry, never a line per update.

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::jiff::{SignedDuration, Timestamp};

use crate::dashboard::WARNING_EVENT_WINDOW;

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
    let desired = spec.replicas.unwrap_or(DEFAULT_REPLICAS);
    let updated = status.updated_replicas.unwrap_or(0);
    let available = status.available_replicas.unwrap_or(0);
    let total = status.replicas.unwrap_or(0);
    let caught_up = available >= desired && updated >= desired && total <= updated;
    let conditions = status.conditions.as_deref().unwrap_or_default();
    let progressing = conditions
        .iter()
        .find(|c| c.type_.as_deref() == Some(PROGRESSING));
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
    if !caught_up && stopped && reason == Some(PROGRESS_DEADLINE_EXCEEDED) {
        let message = progressing.and_then(|c| c.message.clone());
        return Some(rollout(RolloutState::Stalled, stamp, message));
    }
    if spec.paused == Some(true) {
        return None;
    }
    let observed = match (status.observed_generation, meta.generation) {
        (Some(observed), Some(generation)) => observed >= generation,
        _ => true,
    };
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
