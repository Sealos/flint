use serde_json::{Value, json};

use super::*;

fn now() -> Timestamp {
    Timestamp::from_second(1_800_000_000).unwrap()
}

fn ago(minutes: i64) -> String {
    Timestamp::from_second(1_800_000_000 - minutes * 60)
        .unwrap()
        .to_string()
}

fn condition(status: &str, reason: &str, minutes_ago: i64) -> Value {
    json!({
        "type": "Progressing", "status": status, "reason": reason,
        "message": format!("{reason} message"), "lastUpdateTime": ago(minutes_ago),
        "lastTransitionTime": ago(minutes_ago + 5),
    })
}

/// A Deployment of three replicas at generation 2, with `status` and `spec` fields merged in.
fn deployment(status: Value, spec: Value) -> Deployment {
    let mut full_status = json!({"observedGeneration": 2, "replicas": 3});
    full_status
        .as_object_mut()
        .unwrap()
        .extend(status.as_object().unwrap().clone());
    let mut full_spec = json!({"replicas": 3, "selector": {}, "template": {}});
    full_spec
        .as_object_mut()
        .unwrap()
        .extend(spec.as_object().unwrap().clone());
    serde_json::from_value(json!({
        "metadata": {"name": "web", "namespace": "shop", "uid": "u1", "generation": 2},
        "spec": full_spec,
        "status": full_status,
    }))
    .unwrap()
}

fn state_of(status: Value) -> Option<RolloutState> {
    rollout_of(&deployment(status, json!({})), now()).map(|r| r.state)
}

#[test]
fn a_change_the_controller_has_not_seen_has_started() {
    let rollout = rollout_of(
        &deployment(
            json!({"observedGeneration": 1, "updatedReplicas": 3, "availableReplicas": 3,
                   "conditions": [condition("True", "NewReplicaSetAvailable", 90)]}),
            json!({}),
        ),
        now(),
    )
    .unwrap();
    assert_eq!(rollout.state, RolloutState::Started);
    assert_eq!(rollout.at, None);
}

#[test]
fn a_new_revision_with_no_replicas_up_has_started() {
    let state = state_of(json!({
        "updatedReplicas": 0, "availableReplicas": 3,
        "conditions": [condition("True", "NewReplicaSetCreated", 0)],
    }));
    assert_eq!(state, Some(RolloutState::Started));
}

#[test]
fn replicas_coming_up_are_progressing() {
    let rollout = rollout_of(
        &deployment(
            json!({"updatedReplicas": 1, "availableReplicas": 3, "replicas": 4,
                   "conditions": [condition("True", "ReplicaSetUpdated", 1)]}),
            json!({}),
        ),
        now(),
    )
    .unwrap();
    assert_eq!(rollout.state, RolloutState::Progressing);
    assert_eq!(rollout.summary(), "1 of 3 updated, 3 available");
    assert_eq!(
        rollout.at,
        Some(Timestamp::from_second(1_800_000_000 - 60).unwrap())
    );
}

#[test]
fn every_replica_up_before_the_controller_says_so_is_still_progressing() {
    let state = state_of(json!({
        "updatedReplicas": 3, "availableReplicas": 3,
        "conditions": [condition("True", "ReplicaSetUpdated", 0)],
    }));
    assert_eq!(state, Some(RolloutState::Progressing));
}

#[test]
fn a_finished_rollout_is_complete_for_an_hour() {
    let done = |minutes| {
        state_of(json!({
            "updatedReplicas": 3, "availableReplicas": 3,
            "conditions": [condition("True", "NewReplicaSetAvailable", minutes)],
        }))
    };
    assert_eq!(done(2), Some(RolloutState::Complete));
    assert_eq!(done(59), Some(RolloutState::Complete));
    assert_eq!(done(61), None);
}

#[test]
fn a_rollout_past_its_deadline_is_stalled_with_the_controllers_words() {
    let rollout = rollout_of(
        &deployment(
            json!({"updatedReplicas": 1, "availableReplicas": 2,
                   "conditions": [condition("False", "ProgressDeadlineExceeded", 3)]}),
            json!({}),
        ),
        now(),
    )
    .unwrap();
    assert_eq!(rollout.state, RolloutState::Stalled);
    assert_eq!(rollout.summary(), "ProgressDeadlineExceeded message");
}

#[test]
fn a_stalled_rollout_that_has_caught_up_is_not_stalled() {
    let state = state_of(json!({
        "updatedReplicas": 3, "availableReplicas": 3,
        "conditions": [condition("False", "ProgressDeadlineExceeded", 3)],
    }));
    assert_eq!(state, None);
}

#[test]
fn a_rollout_moves_through_its_states_as_one_entry() {
    let rollouts: Vec<_> = [
        json!({"updatedReplicas": 0, "availableReplicas": 3,
               "conditions": [condition("True", "NewReplicaSetCreated", 0)]}),
        json!({"updatedReplicas": 2, "availableReplicas": 3, "replicas": 4,
               "conditions": [condition("True", "ReplicaSetUpdated", 0)]}),
        json!({"updatedReplicas": 3, "availableReplicas": 3,
               "conditions": [condition("True", "NewReplicaSetAvailable", 0)]}),
    ]
    .into_iter()
    .map(|status| rollout_of(&deployment(status, json!({})), now()).unwrap())
    .collect();
    let identity = |r: &Rollout| (r.namespace.clone(), r.name.clone(), r.uid.clone());
    assert!(
        rollouts
            .iter()
            .all(|r| identity(r) == identity(&rollouts[0]))
    );
    let moves: Vec<_> = rollouts.iter().map(|r| r.state).collect();
    assert_eq!(
        moves,
        [
            RolloutState::Started,
            RolloutState::Progressing,
            RolloutState::Complete
        ]
    );
}

#[test]
fn a_deployment_that_never_rolled_or_is_paused_has_no_rollout() {
    assert_eq!(
        state_of(json!({"updatedReplicas": 3, "availableReplicas": 3})),
        None
    );
    let paused = deployment(
        json!({"updatedReplicas": 1, "availableReplicas": 3,
               "conditions": [condition("True", "DeploymentPaused", 1)]}),
        json!({"paused": true}),
    );
    assert_eq!(rollout_of(&paused, now()), None);
}

#[test]
fn without_a_progress_condition_the_replica_counts_decide() {
    assert_eq!(
        state_of(json!({"updatedReplicas": 1, "availableReplicas": 3})),
        Some(RolloutState::Progressing)
    );
}

#[test]
fn a_stopped_rollout_for_another_reason_is_not_listed() {
    let state = state_of(json!({
        "updatedReplicas": 1, "availableReplicas": 3,
        "conditions": [condition("False", "ReplicaSetCreateError", 1)],
    }));
    assert_eq!(state, None);
}

#[test]
fn each_deployment_is_one_rollout() {
    let moving = deployment(
        json!({"updatedReplicas": 1, "availableReplicas": 3,
               "conditions": [condition("True", "ReplicaSetUpdated", 1)]}),
        json!({}),
    );
    assert_eq!(rollouts_of(&[moving.clone(), moving], now()).len(), 2);
}
