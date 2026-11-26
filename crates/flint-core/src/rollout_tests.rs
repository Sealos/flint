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

fn wait_of(plural: &str, object: Value) -> Option<RolloutWait> {
    rollout_wait(&KindId::new("apps", plural), &object)
}

fn deployment_value(status: Value) -> Value {
    serde_json::to_value(deployment(status, json!({}))).unwrap()
}

#[test]
fn a_deployment_wait_finishes_once_every_replica_is_updated_and_available() {
    let done = deployment_value(json!({"updatedReplicas": 3, "availableReplicas": 3,
        "conditions": [condition("True", "NewReplicaSetAvailable", 1)]}));
    assert_eq!(wait_of("deployments", done), Some(RolloutWait::Finished));
    let moving = deployment_value(json!({"updatedReplicas": 1, "availableReplicas": 3,
        "conditions": [condition("True", "ReplicaSetUpdated", 1)]}));
    assert_eq!(
        wait_of("deployments", moving),
        Some(RolloutWait::Waiting("1 of 3 updated, 3 available".into()))
    );
}

#[test]
fn a_deployment_wait_goes_on_until_the_controller_sees_the_change() {
    let unseen = deployment_value(json!({"observedGeneration": 1, "updatedReplicas": 3,
        "availableReplicas": 3, "conditions": [condition("True", "NewReplicaSetAvailable", 1)]}));
    assert!(matches!(
        wait_of("deployments", unseen),
        Some(RolloutWait::Waiting(_))
    ));
    let mut no_status = deployment_value(json!({}));
    no_status.as_object_mut().unwrap().remove("status");
    assert!(matches!(
        wait_of("deployments", no_status),
        Some(RolloutWait::Waiting(_))
    ));
}

#[test]
fn a_deployment_past_its_progress_deadline_fails_the_wait() {
    let stalled = deployment_value(json!({"updatedReplicas": 1, "availableReplicas": 2,
        "conditions": [condition("False", "ProgressDeadlineExceeded", 1)]}));
    assert_eq!(
        wait_of("deployments", stalled),
        Some(RolloutWait::Failed(
            "ProgressDeadlineExceeded message".into()
        ))
    );
}

#[test]
fn a_stateful_set_wait_follows_ready_replicas_revisions_and_partition() {
    let set = |status: Value, strategy: Value| {
        json!({
            "metadata": {"name": "db", "generation": 2},
            "spec": {"replicas": 3, "selector": {}, "serviceName": "db", "template": {},
                     "updateStrategy": strategy},
            "status": status,
        })
    };
    let rolling = json!({"type": "RollingUpdate"});
    let done = json!({"observedGeneration": 2, "replicas": 3, "readyReplicas": 3,
        "updatedReplicas": 3, "currentRevision": "r2", "updateRevision": "r2"});
    assert_eq!(
        wait_of("statefulsets", set(done.clone(), rolling.clone())),
        Some(RolloutWait::Finished)
    );
    let moving = json!({"observedGeneration": 2, "replicas": 3, "readyReplicas": 3,
        "updatedReplicas": 1, "currentRevision": "r1", "updateRevision": "r2"});
    assert!(matches!(
        wait_of("statefulsets", set(moving.clone(), rolling)),
        Some(RolloutWait::Waiting(_))
    ));
    let partitioned = json!({"type": "RollingUpdate", "rollingUpdate": {"partition": 2}});
    assert_eq!(
        wait_of("statefulsets", set(moving, partitioned)),
        Some(RolloutWait::Finished)
    );
    let on_delete = json!({"type": "OnDelete"});
    assert_eq!(wait_of("statefulsets", set(done, on_delete)), None);
}

#[test]
fn a_daemon_set_wait_follows_updated_and_available_pods() {
    let set = |updated: i32, available: i32| {
        json!({
            "metadata": {"name": "agent", "generation": 4},
            "spec": {"selector": {}, "template": {}},
            "status": {"observedGeneration": 4, "desiredNumberScheduled": 2,
                       "currentNumberScheduled": 2, "numberMisscheduled": 0, "numberReady": 2,
                       "updatedNumberScheduled": updated, "numberAvailable": available},
        })
    };
    assert_eq!(
        wait_of("daemonsets", set(2, 2)),
        Some(RolloutWait::Finished)
    );
    assert!(matches!(
        wait_of("daemonsets", set(1, 2)),
        Some(RolloutWait::Waiting(_))
    ));
}

#[test]
fn a_kind_without_rollouts_has_no_wait() {
    let config = json!({"metadata": {"name": "c"}, "data": {}});
    assert_eq!(rollout_wait(&KindId::new("", "configmaps"), &config), None);
    assert_eq!(wait_of("replicasets", config), None);
}
