use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;

const NOW: &str = "2026-06-01T12:00:00Z";

fn now() -> Timestamp {
    NOW.parse().unwrap()
}

/// `minutes` before [`NOW`], as the API writes it.
fn ago(minutes: i64) -> String {
    (now() - SignedDuration::from_secs(minutes * 60)).to_string()
}

fn classifier() -> Classifier {
    Classifier::new("cluster-a", now())
}

fn pod_with(phase: &str, init: Value, containers: Value, extra_meta: Value) -> Pod {
    let mut metadata = json!({"name": "p", "namespace": "ns", "uid": "uid-1"});
    if let Value::Object(extra) = extra_meta {
        metadata.as_object_mut().unwrap().extend(extra);
    }
    let spec = json!({
        "initContainers": spec_containers(&init),
        "containers": spec_containers(&containers),
    });
    serde_json::from_value(json!({
        "metadata": metadata,
        "spec": spec,
        "status": {
            "phase": phase,
            "initContainerStatuses": init,
            "containerStatuses": containers,
        },
    }))
    .unwrap()
}

/// Spec entries, with no restart settings of their own, for a list of container statuses.
fn spec_containers(statuses: &Value) -> Vec<Value> {
    let statuses = statuses.as_array().unwrap();
    statuses
        .iter()
        .map(|s| json!({"name": s["name"]}))
        .collect()
}

fn status(restarts: i32, state: Value, last: Value) -> Value {
    json!({
        "name": "c", "image": "i", "imageID": "", "ready": false,
        "restartCount": restarts, "state": state, "lastState": last,
    })
}

fn waiting(reason: &str) -> Value {
    json!({"waiting": {"reason": reason}})
}

fn running(started_minutes_ago: i64) -> Value {
    json!({"running": {"startedAt": ago(started_minutes_ago)}})
}

fn ended(reason: &str, code: i32, started_minutes_ago: i64, finished_minutes_ago: i64) -> Value {
    json!({"terminated": {
        "reason": reason, "exitCode": code,
        "startedAt": ago(started_minutes_ago), "finishedAt": ago(finished_minutes_ago),
    }})
}

fn none() -> Value {
    json!({})
}

fn pod_problems(pod: &Pod) -> Vec<Problem> {
    let mut out = Vec::new();
    classifier().pod(pod, &mut out);
    out
}

#[test]
fn containers_are_classified() {
    use ProblemKind::{CrashLoop, OomKilled};
    use ProblemReason as R;
    type Expected = Option<(ProblemKind, ProblemReason)>;
    let looping = Some((CrashLoop, R::CrashLoopBackOff));
    let oom_loop = Some((CrashLoop, R::OomKilled));
    let oom = Some((OomKilled, R::OomKilled));
    let cases: Vec<(&str, &str, Value, Expected)> = vec![
        (
            "starting normally",
            "Pending",
            status(0, waiting("ContainerCreating"), none()),
            None,
        ),
        ("healthy", "Running", status(0, running(30), none()), None),
        (
            "first crashes are not yet a loop",
            "Running",
            status(2, waiting("CrashLoopBackOff"), ended("Error", 1, 2, 1)),
            None,
        ),
        (
            "back-off at the restart threshold",
            "Running",
            status(3, waiting("CrashLoopBackOff"), ended("Error", 1, 2, 1)),
            looping,
        ),
        (
            "back-off with no timing still trusts the kubelet",
            "Running",
            status(5, waiting("CrashLoopBackOff"), none()),
            looping,
        ),
        (
            "back-off after a long healthy run is a one-off",
            "Running",
            status(9, waiting("CrashLoopBackOff"), ended("Error", 1, 600, 0)),
            None,
        ),
        (
            "short run between back-offs stays a loop",
            "Running",
            status(6, running(1), ended("Error", 1, 3, 2)),
            looping,
        ),
        (
            "just exited stays a loop",
            "Running",
            status(6, ended("Error", 1, 2, 0), ended("Error", 1, 5, 4)),
            looping,
        ),
        (
            "run past the stable period has recovered",
            "Running",
            status(6, running(11), ended("Error", 1, 13, 12)),
            None,
        ),
        (
            "restart after a long run is not a loop",
            "Running",
            status(40, running(1), ended("Error", 1, 9000, 2)),
            None,
        ),
        (
            "start long after the last exit is not the same loop",
            "Running",
            status(6, running(1), ended("Error", 1, 180, 179)),
            None,
        ),
        (
            "start at the longest restart gap stays a loop",
            "Running",
            status(6, running(1), ended("Error", 1, 8, 7)),
            looping,
        ),
        (
            "start just past the longest restart gap is not a loop",
            "Running",
            status(6, running(1), ended("Error", 1, 9, 8)),
            None,
        ),
        (
            "one exit after a long healthy run is not a loop",
            "Running",
            status(6, ended("Error", 1, 2, 0), ended("Error", 1, 600, 3)),
            None,
        ),
        (
            "one exit long after the previous run is not a loop",
            "Running",
            status(6, ended("Error", 1, 2, 0), ended("Error", 1, 200, 199)),
            None,
        ),
        (
            "one exit with no previous run on record is not a loop",
            "Running",
            status(6, ended("Error", 1, 2, 0), none()),
            None,
        ),
        (
            "other waiting reasons are not a loop",
            "Running",
            status(6, waiting("ContainerCreating"), ended("Error", 1, 3, 2)),
            None,
        ),
        (
            "an OOM loop is one problem",
            "Running",
            status(
                6,
                waiting("CrashLoopBackOff"),
                ended("OOMKilled", 137, 2, 1),
            ),
            oom_loop,
        ),
        (
            "recent OOM kill, recovered",
            "Running",
            status(1, running(5), ended("OOMKilled", 137, 60, 6)),
            oom,
        ),
        (
            "OOM kill just inside the window",
            "Running",
            status(1, running(59), ended("OOMKilled", 137, 90, 59)),
            oom,
        ),
        (
            "OOM kill past the window is history",
            "Running",
            status(1, running(60), ended("OOMKilled", 137, 90, 60)),
            None,
        ),
        (
            "OOM kill with no time is not reported",
            "Running",
            status(
                1,
                running(5),
                json!({"terminated": {"reason": "OOMKilled", "exitCode": 137}}),
            ),
            None,
        ),
        (
            "a clean run after an OOM kill hides the old one",
            "Running",
            status(
                1,
                ended("Completed", 0, 5, 1),
                ended("OOMKilled", 137, 9, 8),
            ),
            None,
        ),
        (
            "failed pod whose final run was OOM killed",
            "Failed",
            status(0, ended("OOMKilled", 137, 9, 8), none()),
            oom,
        ),
        (
            "failed pod will not loop again",
            "Failed",
            status(6, ended("Error", 1, 3, 2), ended("Error", 1, 6, 5)),
            None,
        ),
        (
            "failed pod ignores an OOM in an earlier attempt",
            "Failed",
            status(1, ended("Error", 1, 3, 2), ended("OOMKilled", 137, 6, 5)),
            None,
        ),
        (
            "succeeded pod ignores an earlier OOM",
            "Succeeded",
            status(
                1,
                ended("Completed", 0, 3, 2),
                ended("OOMKilled", 137, 6, 5),
            ),
            None,
        ),
    ];
    for (name, phase, container, expected) in cases {
        let pod = pod_with(phase, json!([]), json!([container]), none());
        let got: Vec<_> = pod_problems(&pod)
            .iter()
            .map(|p| (p.kind(), p.reason.unwrap()))
            .collect();
        assert_eq!(got, expected.into_iter().collect::<Vec<_>>(), "{name}");
    }
}

#[test]
fn init_containers_are_only_problems_when_they_fail() {
    let cases = [
        (
            "still running",
            status(0, running(1), none()),
            waiting("PodInitializing"),
            0,
        ),
        (
            "finished after retries",
            status(4, ended("Completed", 0, 2, 1), ended("Error", 1, 4, 3)),
            running(1),
            0,
        ),
        (
            "crash-looping",
            status(4, waiting("CrashLoopBackOff"), ended("Error", 1, 3, 2)),
            waiting("PodInitializing"),
            1,
        ),
    ];
    for (name, init, main_state, expected) in cases {
        let main = status(0, main_state, none());
        let pod = pod_with("Pending", json!([init]), json!([main]), none());
        assert_eq!(pod_problems(&pod).len(), expected, "{name}");
    }
}

/// A running pod whose container `c` has just exited with `code` after earlier restarts.
fn exited_pod(pod_policy: Option<&str>, container: Value, code: i32) -> Pod {
    let exited = status(6, ended("Error", code, 2, 0), ended("Error", 42, 5, 4));
    let mut pod = pod_with("Running", json!([]), json!([exited]), none());
    let mut spec = json!({"containers": [container]});
    if let Some(policy) = pod_policy {
        spec["restartPolicy"] = json!(policy);
    }
    pod.spec = Some(serde_json::from_value(spec).unwrap());
    pod
}

fn exit_rule(action: &str, operator: &str, values: &[i32]) -> Value {
    json!({"action": action, "exitCodes": {"operator": operator, "values": values}})
}

#[test]
fn a_just_exited_container_is_a_loop_only_if_it_will_restart() {
    let plain = json!({"name": "c"});
    let with_policy = |policy: &str| json!({"name": "c", "restartPolicy": policy});
    let with_rules = |policy: &str, rules: Vec<Value>| json!({"name": "c", "restartPolicy": policy, "restartPolicyRules": rules});
    let restart_on_42 = || vec![exit_rule("Restart", "In", &[42])];
    let cases = [
        ("pod default restarts", None, plain.clone(), 1, true),
        ("pod Always", Some("Always"), plain.clone(), 1, true),
        ("pod OnFailure", Some("OnFailure"), plain.clone(), 1, true),
        ("pod Never", Some("Never"), plain.clone(), 1, false),
        (
            "container Never overrides the pod",
            Some("Always"),
            with_policy("Never"),
            1,
            false,
        ),
        (
            "container Always overrides the pod",
            Some("Never"),
            with_policy("Always"),
            1,
            true,
        ),
        (
            "matching rule restarts",
            Some("Never"),
            with_rules("Never", restart_on_42()),
            42,
            true,
        ),
        (
            "unmatched rule falls back to Never",
            Some("Never"),
            with_rules("Never", restart_on_42()),
            1,
            false,
        ),
        (
            "unmatched rule falls back to OnFailure",
            Some("Never"),
            with_rules("OnFailure", restart_on_42()),
            1,
            true,
        ),
        (
            "NotIn matches other codes",
            Some("Never"),
            with_rules("Never", vec![exit_rule("Restart", "NotIn", &[42])]),
            1,
            true,
        ),
        (
            "NotIn does not match a listed code",
            Some("Never"),
            with_rules("Never", vec![exit_rule("Restart", "NotIn", &[1])]),
            1,
            false,
        ),
        (
            "matching RestartAllContainers rule restarts",
            Some("Never"),
            with_rules("Never", vec![exit_rule("RestartAllContainers", "In", &[1])]),
            1,
            true,
        ),
        (
            "unmatched RestartAllContainers rule falls back to Never",
            Some("Never"),
            with_rules(
                "Never",
                vec![exit_rule("RestartAllContainers", "In", &[42])],
            ),
            1,
            false,
        ),
        (
            "first matching rule wins",
            Some("Always"),
            with_rules(
                "Always",
                vec![
                    exit_rule("Unknown", "In", &[1]),
                    exit_rule("Restart", "In", &[1]),
                ],
            ),
            1,
            false,
        ),
        (
            "unknown operator is not a clear restart",
            Some("Always"),
            with_rules("Always", vec![exit_rule("Restart", "Unknown", &[1])]),
            1,
            false,
        ),
        (
            "rule without exit codes is not a clear restart",
            Some("Always"),
            with_rules("Always", vec![json!({"action": "Restart"})]),
            1,
            false,
        ),
        (
            "unknown policy is not a clear restart",
            None,
            with_policy("Sometimes"),
            1,
            false,
        ),
        (
            "container missing from the spec",
            Some("Always"),
            json!({"name": "other"}),
            1,
            false,
        ),
    ];
    for (name, pod_policy, container, code, expected) in cases {
        let pod = exited_pod(pod_policy, container, code);
        let looping = pod_problems(&pod)
            .iter()
            .any(|p| p.kind() == ProblemKind::CrashLoop);
        assert_eq!(looping, expected, "{name}");
    }
}

#[test]
fn a_just_exited_container_without_a_spec_is_not_a_loop() {
    let mut pod = exited_pod(Some("Always"), json!({"name": "c"}), 1);
    pod.spec = None;
    assert!(pod_problems(&pod).is_empty());
}

#[test]
fn a_restartable_init_container_uses_its_own_policy() {
    let exited = status(6, ended("Error", 1, 2, 0), ended("Error", 1, 5, 4));
    let main = json!({"name": "main", "image": "i", "imageID": "", "ready": true,
        "restartCount": 0, "state": running(30)});
    let mut pod = pod_with("Running", json!([exited]), json!([main]), none());
    pod.spec = Some(
        serde_json::from_value(json!({
            "restartPolicy": "Never",
            "initContainers": [{"name": "c", "restartPolicy": "Always"}],
            "containers": [{"name": "main"}],
        }))
        .unwrap(),
    );
    let problems = pod_problems(&pod);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].kind(), ProblemKind::CrashLoop);
}

#[test]
fn ephemeral_containers_report_only_their_own_oom_kill() {
    let cases = [
        (
            "recent OOM kill",
            status(0, ended("OOMKilled", 137, 9, 8), none()),
            Some(ProblemKind::OomKilled),
        ),
        (
            "old OOM kill",
            status(0, ended("OOMKilled", 137, 90, 61), none()),
            None,
        ),
        (
            "a failure is never a crash loop",
            status(6, ended("Error", 1, 2, 0), ended("Error", 1, 5, 4)),
            None,
        ),
        (
            "a back-off is never a crash loop",
            status(6, waiting("CrashLoopBackOff"), ended("Error", 1, 3, 2)),
            None,
        ),
        (
            "only the current state counts",
            status(1, running(1), ended("OOMKilled", 137, 9, 8)),
            None,
        ),
    ];
    for (name, mut ephemeral, expected) in cases {
        ephemeral["name"] = json!("debug");
        let main = status(0, running(30), none());
        let mut pod = pod_with("Running", json!([]), json!([main]), none());
        pod.status.as_mut().unwrap().ephemeral_container_statuses =
            Some(vec![serde_json::from_value(ephemeral).unwrap()]);
        let problems = pod_problems(&pod);
        assert_eq!(
            problems.iter().map(Problem::kind).next(),
            expected,
            "{name}"
        );
        if expected.is_some() {
            let ProblemSubject::Container { container, .. } = &problems[0].id.subject else {
                panic!("{name}: not a container");
            };
            assert_eq!(container, "debug", "{name}");
        }
    }
}

#[test]
fn a_deleting_pod_has_no_problems() {
    let container = status(6, waiting("CrashLoopBackOff"), ended("Error", 1, 3, 2));
    let pod = pod_with(
        "Running",
        json!([]),
        json!([container]),
        json!({"deletionTimestamp": ago(1)}),
    );
    assert!(pod_problems(&pod).is_empty());
}

fn node(created_minutes_ago: i64, ready: Option<(&str, &str, i64)>, deleting: bool) -> Node {
    let conditions: Vec<Value> = ready
        .map(|(status, reason, minutes)| {
            json!({"type": "Ready", "status": status, "reason": reason,
                   "lastTransitionTime": ago(minutes)})
        })
        .into_iter()
        .collect();
    let mut metadata = json!({
        "name": "n", "uid": "node-uid", "creationTimestamp": ago(created_minutes_ago),
    });
    if deleting {
        metadata["deletionTimestamp"] = json!(ago(1));
    }
    serde_json::from_value(json!({"metadata": metadata, "status": {"conditions": conditions}}))
        .unwrap()
}

#[test]
fn nodes_are_classified() {
    use ProblemReason as R;
    let cases = [
        (
            "ready",
            node(900, Some(("True", "KubeletReady", 800)), false),
            None,
        ),
        (
            "joining",
            node(5, Some(("False", "KubeletNotReady", 5)), false),
            None,
        ),
        ("joining with no condition yet", node(1, None, false), None),
        (
            "brief blip",
            node(900, Some(("False", "KubeletNotReady", 1)), false),
            None,
        ),
        (
            "not ready past the grace",
            node(900, Some(("False", "KubeletNotReady", 2)), false),
            Some(Some(R::KubeletNotReady)),
        ),
        (
            "stopped reporting",
            node(900, Some(("Unknown", "NodeStatusUnknown", 5)), false),
            Some(Some(R::NodeStatusUnknown)),
        ),
        (
            "an unknown reason is dropped",
            node(900, Some(("False", "SomethingCustom", 5)), false),
            Some(None),
        ),
        (
            "no condition past joining",
            node(900, None, false),
            Some(None),
        ),
        (
            "deleting",
            node(900, Some(("False", "KubeletNotReady", 5)), true),
            None,
        ),
    ];
    for (name, node, expected) in cases {
        let got = classifier().node(&node);
        assert_eq!(got.as_ref().map(|p| p.reason), expected, "{name}");
        if let Some(problem) = got {
            assert_eq!(problem.kind(), ProblemKind::NodeNotReady, "{name}");
        }
    }
}

fn job(conditions: Value, deleting: bool) -> Job {
    let mut metadata = json!({"name": "j", "namespace": "ns", "uid": "job-uid"});
    if deleting {
        metadata["deletionTimestamp"] = json!(ago(1));
    }
    serde_json::from_value(json!({"metadata": metadata, "status": {"conditions": conditions}}))
        .unwrap()
}

fn job_condition(type_: &str, status: &str, reason: &str) -> Value {
    json!({"type": type_, "status": status, "reason": reason, "lastTransitionTime": ago(5)})
}

#[test]
fn jobs_are_classified() {
    use ProblemReason as R;
    let failed = |reason| json!([job_condition("Failed", "True", reason)]);
    let cases = [
        ("running", job(json!([]), false), None),
        (
            "completed",
            job(
                json!([job_condition("Complete", "True", "CompletionsReached")]),
                false,
            ),
            None,
        ),
        (
            "still stopping its pods",
            job(
                json!([job_condition(
                    "FailureTarget",
                    "True",
                    "BackoffLimitExceeded"
                )]),
                false,
            ),
            None,
        ),
        (
            "failed condition not true",
            job(
                json!([job_condition("Failed", "False", "BackoffLimitExceeded")]),
                false,
            ),
            None,
        ),
        (
            "out of retries",
            job(failed("BackoffLimitExceeded"), false),
            Some(Some(R::BackoffLimitExceeded)),
        ),
        (
            "past its deadline",
            job(failed("DeadlineExceeded"), false),
            Some(Some(R::DeadlineExceeded)),
        ),
        (
            "pod failure policy",
            job(failed("PodFailurePolicy"), false),
            Some(Some(R::PodFailurePolicy)),
        ),
        (
            "failed indexes",
            job(failed("FailedIndexes"), false),
            Some(Some(R::FailedIndexes)),
        ),
        (
            "too many failed indexes",
            job(failed("MaxFailedIndexesExceeded"), false),
            Some(Some(R::MaxFailedIndexesExceeded)),
        ),
        (
            "an unknown reason is dropped",
            job(failed("Custom"), false),
            Some(None),
        ),
        ("deleting", job(failed("BackoffLimitExceeded"), true), None),
    ];
    for (name, job, expected) in cases {
        let got = classifier().job(&job);
        assert_eq!(got.as_ref().map(|p| p.reason), expected, "{name}");
        if let Some(problem) = got {
            assert_eq!(problem.kind(), ProblemKind::JobFailed, "{name}");
            assert_eq!(problem.since, Some(now() - SignedDuration::from_secs(300)));
        }
    }
}

fn crash_looping_pod(uid: &str, restarts: i32, finished_minutes_ago: i64) -> Pod {
    let last = ended("Error", 1, finished_minutes_ago + 1, finished_minutes_ago);
    pod_with(
        "Running",
        json!([]),
        json!([status(restarts, waiting("CrashLoopBackOff"), last)]),
        json!({"uid": uid}),
    )
}

#[test]
fn identity_survives_restarts_and_changes_with_the_object() {
    let first = pod_problems(&crash_looping_pod("uid-1", 3, 5)).remove(0);
    let later = pod_problems(&crash_looping_pod("uid-1", 9, 1)).remove(0);
    assert_eq!(first.id, later.id, "restarts and times are not identity");
    assert_ne!(first.since, later.since);

    let recreated = pod_problems(&crash_looping_pod("uid-2", 3, 5)).remove(0);
    assert_ne!(first.id, recreated.id, "a recreated pod is a new problem");

    let mut other_cluster = Vec::new();
    Classifier::new("cluster-b", now()).pod(&crash_looping_pod("uid-1", 3, 5), &mut other_cluster);
    assert_ne!(
        first.id, other_cluster[0].id,
        "the cluster is part of identity"
    );
    assert_eq!(&*other_cluster[0].id.cluster, "cluster-b");
}

#[test]
fn a_loop_keeps_its_identity_through_its_short_runs() {
    let backing_off = pod_with(
        "Running",
        json!([]),
        json!([status(
            5,
            waiting("CrashLoopBackOff"),
            ended("Error", 1, 4, 3)
        )]),
        none(),
    );
    let between = pod_with(
        "Running",
        json!([]),
        json!([status(6, running(1), ended("Error", 1, 4, 3))]),
        none(),
    );
    assert_eq!(
        pod_problems(&backing_off)[0].id,
        pod_problems(&between)[0].id
    );
}

#[test]
fn a_notifier_can_tell_new_reported_and_returned_problems_apart() {
    let ids = |pods: &[&Pod]| -> HashSet<ProblemId> {
        classifier()
            .pods(pods.iter().copied())
            .into_iter()
            .map(|p| p.id)
            .collect()
    };
    let broken = crash_looping_pod("uid-1", 4, 2);
    let healthy = pod_with(
        "Running",
        json!([]),
        json!([status(4, running(30), none())]),
        none(),
    );
    let first = ids(&[&broken]);
    let again = ids(&[&broken]);
    let cleared = ids(&[&healthy]);
    let returned = ids(&[&broken]);
    assert_eq!(first.len(), 1);
    assert_eq!(first, again, "already reported");
    assert!(cleared.is_empty(), "cleared");
    assert_eq!(first, returned, "came back under the same id");
}

#[test]
fn text_never_carries_server_messages() {
    const SECRET: &str = "token=hunter2";
    let container = json!({
        "name": "c", "image": "i", "imageID": "", "ready": false, "restartCount": 6,
        "state": {"waiting": {"reason": "CrashLoopBackOff", "message": SECRET}},
        "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137, "message": SECRET,
            "startedAt": ago(3), "finishedAt": ago(2)}},
    });
    let pod = pod_with("Running", json!([]), json!([container]), none());
    let not_ready: Node = serde_json::from_value(json!({
        "metadata": {"name": "n"},
        "status": {"conditions": [{"type": "Ready", "status": "False",
            "reason": SECRET, "message": SECRET, "lastTransitionTime": ago(9)}]},
    }))
    .unwrap();
    let failed: Job = serde_json::from_value(json!({
        "metadata": {"name": "j", "namespace": "ns"},
        "status": {"conditions": [{"type": "Failed", "status": "True",
            "reason": SECRET, "message": SECRET}]},
    }))
    .unwrap();

    let mut problems = pod_problems(&pod);
    problems.extend(classifier().node(&not_ready));
    problems.extend(classifier().job(&failed));
    assert_eq!(problems.len(), 3);
    for problem in problems {
        let printed = format!("{problem:?} {}", problem.summary());
        assert!(!printed.contains(SECRET), "{printed}");
    }
}

#[test]
fn severity_follows_the_kind() {
    assert!(ProblemSeverity::Critical > ProblemSeverity::Warning);
    for (kind, severity) in [
        (ProblemKind::CrashLoop, ProblemSeverity::Critical),
        (ProblemKind::NodeNotReady, ProblemSeverity::Critical),
        (ProblemKind::OomKilled, ProblemSeverity::Warning),
        (ProblemKind::JobFailed, ProblemSeverity::Warning),
        (ProblemKind::ImagePull, ProblemSeverity::Critical),
        (ProblemKind::WorkloadShort, ProblemSeverity::Warning),
    ] {
        assert_eq!(kind.severity(), severity, "{kind:?}");
    }
}

#[test]
fn reasons_keep_their_kubernetes_spelling() {
    for (name, reason) in [
        ("BackoffLimitExceeded", ProblemReason::BackoffLimitExceeded),
        ("DeadlineExceeded", ProblemReason::DeadlineExceeded),
        ("PodFailurePolicy", ProblemReason::PodFailurePolicy),
        ("FailedIndexes", ProblemReason::FailedIndexes),
        (
            "MaxFailedIndexesExceeded",
            ProblemReason::MaxFailedIndexesExceeded,
        ),
    ] {
        assert_eq!(ProblemReason::of_job(Some(name)), Some(reason));
        assert_eq!(reason.as_str(), name);
    }
    for (name, reason) in [
        ("KubeletNotReady", ProblemReason::KubeletNotReady),
        ("NodeStatusUnknown", ProblemReason::NodeStatusUnknown),
    ] {
        assert_eq!(ProblemReason::of_node(Some(name)), Some(reason));
        assert_eq!(reason.as_str(), name);
    }
    for (name, reason) in [
        ("ErrImagePull", ProblemReason::ErrImagePull),
        ("ImagePullBackOff", ProblemReason::ImagePullBackOff),
        ("InvalidImageName", ProblemReason::InvalidImageName),
        ("ErrImageNeverPull", ProblemReason::ErrImageNeverPull),
        ("RegistryUnavailable", ProblemReason::RegistryUnavailable),
        (
            "SignatureValidationFailed",
            ProblemReason::SignatureValidationFailed,
        ),
    ] {
        assert_eq!(ProblemReason::of_image_pull(Some(name)), Some(reason));
        assert_eq!(reason.as_str(), name);
    }
    assert_eq!(
        ProblemReason::ProgressDeadlineExceeded.as_str(),
        "ProgressDeadlineExceeded"
    );
    assert_eq!(
        ProblemReason::MinimumReplicasUnavailable.as_str(),
        "MinimumReplicasUnavailable"
    );
    assert_eq!(ProblemReason::CrashLoopBackOff.as_str(), "CrashLoopBackOff");
    assert_eq!(ProblemReason::OomKilled.as_str(), "OOMKilled");
}

fn aged_pod(phase: &str, created_seconds_ago: i64, container: Value) -> Pod {
    let created = (now() - SignedDuration::from_secs(created_seconds_ago)).to_string();
    pod_with(
        phase,
        json!([]),
        json!([container]),
        json!({"creationTimestamp": created}),
    )
}

#[test]
fn image_pull_failures_are_classified() {
    use ProblemReason as R;
    let cases: Vec<(&str, &str, i64, Value, Option<ProblemReason>)> = vec![
        (
            "failed pull past the grace",
            "Pending",
            60,
            status(0, waiting("ErrImagePull"), none()),
            Some(R::ErrImagePull),
        ),
        (
            "back-off past the grace",
            "Pending",
            600,
            status(0, waiting("ImagePullBackOff"), none()),
            Some(R::ImagePullBackOff),
        ),
        (
            "failed pull inside the grace may still clear",
            "Pending",
            59,
            status(0, waiting("ErrImagePull"), none()),
            None,
        ),
        (
            "back-off inside the grace may still clear",
            "Pending",
            59,
            status(0, waiting("ImagePullBackOff"), none()),
            None,
        ),
        (
            "an invalid name cannot clear, so it has no grace",
            "Pending",
            1,
            status(0, waiting("InvalidImageName"), none()),
            Some(R::InvalidImageName),
        ),
        (
            "an image that may not be pulled and is missing",
            "Pending",
            60,
            status(0, waiting("ErrImageNeverPull"), none()),
            Some(R::ErrImageNeverPull),
        ),
        (
            "an unavailable registry",
            "Pending",
            60,
            status(0, waiting("RegistryUnavailable"), none()),
            Some(R::RegistryUnavailable),
        ),
        (
            "a failed signature check",
            "Pending",
            60,
            status(0, waiting("SignatureValidationFailed"), none()),
            Some(R::SignatureValidationFailed),
        ),
        (
            "a missing image may still be loaded inside the grace",
            "Pending",
            59,
            status(0, waiting("ErrImageNeverPull"), none()),
            None,
        ),
        (
            "an unavailable registry may still clear inside the grace",
            "Pending",
            59,
            status(0, waiting("RegistryUnavailable"), none()),
            None,
        ),
        (
            "a failed signature check may still clear inside the grace",
            "Pending",
            59,
            status(0, waiting("SignatureValidationFailed"), none()),
            None,
        ),
        (
            "a pull still running is not a failure",
            "Pending",
            600,
            status(0, waiting("ContainerCreating"), none()),
            None,
        ),
        (
            "a failed pod will not pull again",
            "Failed",
            600,
            status(0, waiting("ImagePullBackOff"), none()),
            None,
        ),
        (
            "the current pull failure outranks an earlier OOM kill",
            "Running",
            600,
            status(
                1,
                waiting("ImagePullBackOff"),
                ended("OOMKilled", 137, 9, 8),
            ),
            Some(R::ImagePullBackOff),
        ),
    ];
    for (name, phase, age, container, expected) in cases {
        let problems = pod_problems(&aged_pod(phase, age, container));
        let got: Vec<_> = problems.iter().map(|p| (p.kind(), p.reason)).collect();
        let want: Vec<_> = expected
            .map(|reason| (ProblemKind::ImagePull, Some(reason)))
            .into_iter()
            .collect();
        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn an_image_pull_keeps_its_identity_between_retries() {
    let failed = aged_pod("Pending", 600, status(0, waiting("ErrImagePull"), none()));
    let backing_off = aged_pod(
        "Pending",
        600,
        status(0, waiting("ImagePullBackOff"), none()),
    );
    assert_eq!(
        pod_problems(&failed)[0].id,
        pod_problems(&backing_off)[0].id
    );
}

#[test]
fn a_debug_container_with_a_bad_image_is_not_a_problem() {
    let mut pod = aged_pod("Running", 600, status(0, running(5), none()));
    let debug = serde_json::from_value(status(0, waiting("ImagePullBackOff"), none())).unwrap();
    pod.status.as_mut().unwrap().ephemeral_container_statuses = Some(vec![debug]);
    assert!(pod_problems(&pod).is_empty());
}

/// An object created an hour before [`NOW`], with `extra` merged into its metadata.
fn workload_meta(extra: Value) -> Value {
    let mut meta = json!({
        "name": "w", "namespace": "ns", "uid": "uid-w", "generation": 2,
        "creationTimestamp": ago(60),
    });
    meta.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    meta
}

fn condition(type_: &str, status: &str, reason: &str, minutes_ago: i64) -> Value {
    json!({"type": type_, "status": status, "reason": reason, "lastTransitionTime": ago(minutes_ago)})
}

fn deployment(
    meta: Value,
    spec: Value,
    available: i32,
    updated: i32,
    conditions: Value,
) -> Deployment {
    let mut full_spec = json!({"selector": {}, "template": {}});
    full_spec
        .as_object_mut()
        .unwrap()
        .extend(spec.as_object().unwrap().clone());
    serde_json::from_value(json!({
        "metadata": workload_meta(meta),
        "spec": full_spec,
        "status": {
            "observedGeneration": 2, "availableReplicas": available,
            "updatedReplicas": updated, "conditions": conditions,
        },
    }))
    .unwrap()
}

#[test]
fn deployments_are_classified() {
    use ProblemReason as R;
    let settled = condition("Progressing", "True", "NewReplicaSetAvailable", 30);
    let rolling = condition("Progressing", "True", "ReplicaSetUpdated", 1);
    let stalled = condition("Progressing", "False", "ProgressDeadlineExceeded", 3);
    let down = condition("Available", "False", "MinimumReplicasUnavailable", 3);
    let just_down = condition("Available", "False", "MinimumReplicasUnavailable", 1);
    let up = condition("Available", "True", "MinimumReplicasAvailable", 30);
    let three = json!({"replicas": 3});
    type Case = (
        &'static str,
        Value,
        Value,
        i32,
        i32,
        Value,
        Option<Option<ProblemReason>>,
    );
    let cases: Vec<Case> = vec![
        (
            "healthy",
            none(),
            three.clone(),
            3,
            3,
            json!([settled, up]),
            None,
        ),
        (
            "settled and down past the grace",
            none(),
            three.clone(),
            1,
            3,
            json!([settled, down]),
            Some(Some(R::MinimumReplicasUnavailable)),
        ),
        (
            "settled and only just down",
            none(),
            three.clone(),
            1,
            3,
            json!([settled, just_down]),
            None,
        ),
        (
            "short within maxUnavailable stays available",
            none(),
            json!({"replicas": 100}),
            90,
            100,
            json!([settled, up]),
            None,
        ),
        (
            "a rollout is moving, even with Available false",
            none(),
            three.clone(),
            1,
            1,
            json!([rolling, down]),
            None,
        ),
        (
            "a stalled rollout is reported",
            none(),
            three.clone(),
            2,
            1,
            json!([stalled, up]),
            Some(Some(R::ProgressDeadlineExceeded)),
        ),
        (
            "a rollout stopped by a create error is reported once down",
            none(),
            three.clone(),
            1,
            1,
            json!([
                condition("Progressing", "False", "ReplicaSetCreateError", 5),
                down
            ]),
            Some(Some(R::MinimumReplicasUnavailable)),
        ),
        (
            "a stalled rollout that has caught up is not",
            none(),
            three.clone(),
            3,
            3,
            json!([stalled, up]),
            None,
        ),
        (
            "paused mid-rollout",
            none(),
            json!({"replicas": 3, "paused": true}),
            1,
            1,
            json!([stalled, down]),
            None,
        ),
        (
            "scaled to zero",
            none(),
            json!({"replicas": 0}),
            0,
            0,
            json!([settled, down]),
            None,
        ),
        (
            "replicas defaults to one",
            none(),
            json!({}),
            0,
            1,
            json!([settled, down]),
            Some(Some(R::MinimumReplicasUnavailable)),
        ),
        (
            "no progress deadline, mid-rollout",
            none(),
            three.clone(),
            1,
            1,
            json!([down]),
            None,
        ),
        (
            "no progress deadline, settled",
            none(),
            three.clone(),
            1,
            3,
            json!([down]),
            Some(Some(R::MinimumReplicasUnavailable)),
        ),
        (
            "the controller has not seen the new spec",
            json!({"generation": 3}),
            three.clone(),
            1,
            3,
            json!([settled, down]),
            None,
        ),
        (
            "first start",
            json!({"creationTimestamp": ago(4)}),
            three.clone(),
            1,
            3,
            json!([settled, down]),
            None,
        ),
        (
            "deleting",
            json!({"deletionTimestamp": ago(1)}),
            three.clone(),
            1,
            3,
            json!([settled, down]),
            None,
        ),
    ];
    for (name, meta, spec, available, updated, conditions, expected) in cases {
        let object = deployment(meta, spec, available, updated, conditions);
        let got = classifier().deployment(&object);
        assert_eq!(got.as_ref().map(|p| p.reason), expected, "{name}");
        if let Some(problem) = got {
            assert_eq!(problem.kind(), ProblemKind::WorkloadShort, "{name}");
        }
    }
}

#[test]
fn a_stalled_rollout_is_judged_by_its_new_replicas() {
    let stalled = || {
        json!([condition(
            "Progressing",
            "False",
            "ProgressDeadlineExceeded",
            3
        )])
    };
    let rolling = json!([condition("Progressing", "True", "ReplicaSetUpdated", 1)]);
    // (name, conditions, total, available, updated, reported)
    let cases = [
        (
            "old pods still serve every replica",
            stalled(),
            4,
            3,
            1,
            true,
        ),
        (
            "new pods are up but an old one remains",
            stalled(),
            4,
            3,
            3,
            true,
        ),
        (
            "every replica is new and available",
            stalled(),
            3,
            3,
            3,
            false,
        ),
        ("scaling down after catching up", stalled(), 5, 5, 5, false),
        (
            "a moving rollout with old pods serving",
            rolling,
            4,
            3,
            1,
            false,
        ),
    ];
    for (name, conditions, total, available, updated, expected) in cases {
        let mut object = deployment(
            none(),
            json!({"replicas": 3}),
            available,
            updated,
            conditions,
        );
        object.status.as_mut().unwrap().replicas = Some(total);
        let got = classifier().deployment(&object);
        assert_eq!(got.is_some(), expected, "{name}");
        if let Some(problem) = got {
            assert_eq!(
                problem.reason,
                Some(ProblemReason::ProgressDeadlineExceeded),
                "{name}"
            );
        }
    }
}

fn stateful_set(meta: Value, spec: Value, status: Value) -> StatefulSet {
    let mut full_spec = json!({"selector": {}, "serviceName": "s", "template": {}});
    full_spec
        .as_object_mut()
        .unwrap()
        .extend(spec.as_object().unwrap().clone());
    let mut full_status = json!({
        "observedGeneration": 2, "replicas": 3, "readyReplicas": 1, "updatedReplicas": 3,
        "currentRevision": "r1", "updateRevision": "r1",
    });
    full_status
        .as_object_mut()
        .unwrap()
        .extend(status.as_object().unwrap().clone());
    serde_json::from_value(json!({
        "metadata": workload_meta(meta),
        "spec": full_spec,
        "status": full_status,
    }))
    .unwrap()
}

#[test]
fn stateful_sets_are_classified() {
    let three = || json!({"replicas": 3});
    let new_revision = || json!({"updateRevision": "r2", "updatedReplicas": 1});
    let partitioned = json!({"replicas": 3, "updateStrategy": {
        "type": "RollingUpdate", "rollingUpdate": {"partition": 2},
    }});
    let on_delete = json!({"replicas": 3, "updateStrategy": {"type": "OnDelete"}});
    let cases: Vec<(&str, Value, Value, Value, bool)> = vec![
        ("settled and short", none(), three(), none(), true),
        (
            "healthy",
            none(),
            three(),
            json!({"readyReplicas": 3}),
            false,
        ),
        ("scaling up", none(), three(), json!({"replicas": 2}), false),
        (
            "every existing pod is ready but the next was not created",
            none(),
            three(),
            json!({"replicas": 2, "readyReplicas": 2}),
            true,
        ),
        (
            "no pod could be created",
            none(),
            three(),
            json!({"replicas": 0, "readyReplicas": 0}),
            true,
        ),
        (
            "a parallel set creates all its pods at once, so a gap is not ordering",
            none(),
            json!({"replicas": 3, "podManagementPolicy": "Parallel"}),
            json!({"replicas": 2}),
            true,
        ),
        (
            "a rollout that is short of a pod is still moving",
            none(),
            three(),
            json!({"replicas": 2, "readyReplicas": 2, "updateRevision": "r2", "updatedReplicas": 1}),
            false,
        ),
        ("rolling out", none(), three(), new_revision(), false),
        (
            "the last replica of a rollout is still starting",
            none(),
            three(),
            json!({"updateRevision": "r2", "updatedReplicas": 3}),
            false,
        ),
        (
            "a partition is done rolling",
            none(),
            partitioned.clone(),
            new_revision(),
            true,
        ),
        (
            "a partition still rolling",
            none(),
            partitioned,
            json!({"updateRevision": "r2", "updatedReplicas": 0}),
            false,
        ),
        (
            "OnDelete is never rolling",
            none(),
            on_delete,
            new_revision(),
            true,
        ),
        (
            "scaled to zero",
            none(),
            json!({"replicas": 0}),
            json!({"replicas": 0, "readyReplicas": 0}),
            false,
        ),
        (
            "stale status",
            json!({"generation": 3}),
            three(),
            none(),
            false,
        ),
        (
            "first start",
            json!({"creationTimestamp": ago(4)}),
            three(),
            none(),
            false,
        ),
        (
            "deleting",
            json!({"deletionTimestamp": ago(1)}),
            three(),
            none(),
            false,
        ),
    ];
    for (name, meta, spec, status, expected) in cases {
        let got = classifier().stateful_set(&stateful_set(meta, spec, status));
        assert_eq!(got.is_some(), expected, "{name}");
    }
}

fn daemon_set(meta: Value, strategy: &str, status: Value) -> DaemonSet {
    let mut full_status = json!({
        "observedGeneration": 2, "desiredNumberScheduled": 3, "currentNumberScheduled": 3,
        "numberReady": 2, "numberMisscheduled": 0, "updatedNumberScheduled": 3,
    });
    full_status
        .as_object_mut()
        .unwrap()
        .extend(status.as_object().unwrap().clone());
    serde_json::from_value(json!({
        "metadata": workload_meta(meta),
        "spec": {"selector": {}, "template": {}, "updateStrategy": {"type": strategy}},
        "status": full_status,
    }))
    .unwrap()
}

#[test]
fn daemon_sets_are_classified() {
    const ROLLING: &str = "RollingUpdate";
    let cases: Vec<(&str, Value, &str, Value, bool)> = vec![
        ("settled and short", none(), ROLLING, none(), true),
        ("healthy", none(), ROLLING, json!({"numberReady": 3}), false),
        (
            "placing on a new node",
            none(),
            ROLLING,
            json!({"currentNumberScheduled": 2, "updatedNumberScheduled": 2, "numberReady": 2}),
            true,
        ),
        (
            "rolling out",
            none(),
            ROLLING,
            json!({"updatedNumberScheduled": 1}),
            false,
        ),
        (
            "rolling out while a node has no pod",
            none(),
            ROLLING,
            json!({"currentNumberScheduled": 2, "updatedNumberScheduled": 1}),
            false,
        ),
        (
            "OnDelete with a node missing a pod",
            none(),
            "OnDelete",
            json!({"currentNumberScheduled": 2, "updatedNumberScheduled": 1}),
            false,
        ),
        (
            "OnDelete is never rolling",
            none(),
            "OnDelete",
            json!({"updatedNumberScheduled": 1}),
            true,
        ),
        (
            "no matching nodes",
            none(),
            ROLLING,
            json!({"desiredNumberScheduled": 0, "currentNumberScheduled": 0, "numberReady": 0}),
            false,
        ),
        (
            "stale status",
            json!({"generation": 3}),
            ROLLING,
            none(),
            false,
        ),
        (
            "first start",
            json!({"creationTimestamp": ago(4)}),
            ROLLING,
            none(),
            false,
        ),
        (
            "deleting",
            json!({"deletionTimestamp": ago(1)}),
            ROLLING,
            none(),
            false,
        ),
    ];
    for (name, meta, strategy, status, expected) in cases {
        let got = classifier().daemon_set(&daemon_set(meta, strategy, status));
        assert_eq!(got.is_some(), expected, "{name}");
        if let Some(problem) = got {
            assert_eq!(problem.hold_down(), Some(WORKLOAD_HOLD_DOWN), "{name}");
        }
    }
}

#[test]
fn only_workloads_without_timestamps_need_a_hold_down() {
    let three = json!({"replicas": 3});
    let set = classifier()
        .stateful_set(&stateful_set(none(), three.clone(), none()))
        .unwrap();
    let daemon = classifier()
        .daemon_set(&daemon_set(none(), "RollingUpdate", none()))
        .unwrap();
    let stalled = json!([condition(
        "Progressing",
        "False",
        "ProgressDeadlineExceeded",
        3
    )]);
    let timed = classifier()
        .deployment(&deployment(none(), three, 1, 1, stalled))
        .unwrap();
    assert_eq!(set.hold_down(), Some(WORKLOAD_HOLD_DOWN));
    assert_eq!(daemon.hold_down(), Some(WORKLOAD_HOLD_DOWN));
    assert_eq!(timed.hold_down(), None);
}

#[test]
fn workload_identity_ignores_counts_and_separates_kinds() {
    let short = stateful_set(none(), json!({"replicas": 3}), none());
    let shorter = stateful_set(none(), json!({"replicas": 3}), json!({"readyReplicas": 0}));
    let recreated = stateful_set(json!({"uid": "uid-new"}), json!({"replicas": 3}), none());
    let id = |set: &StatefulSet| classifier().stateful_set(set).unwrap().id;
    assert_eq!(id(&short), id(&shorter));
    assert_ne!(id(&short), id(&recreated));
    let daemon = classifier().daemon_set(&daemon_set(none(), "RollingUpdate", none()));
    assert_ne!(id(&short), daemon.unwrap().id);
}

#[test]
fn new_kinds_never_carry_server_messages() {
    const SECRET: &str = "token=hunter2";
    let container = json!({
        "name": "c", "image": SECRET, "imageID": "", "ready": false, "restartCount": 0,
        "state": {"waiting": {"reason": "ErrImagePull", "message": SECRET}},
    });
    let pod = aged_pod("Pending", 600, container);
    let stalled = json!([{"type": "Progressing", "status": "False", "message": SECRET,
        "reason": "ProgressDeadlineExceeded", "lastTransitionTime": ago(3)}]);
    let unknown = json!([
        condition("Progressing", "True", "NewReplicaSetAvailable", 30),
        {"type": "Available", "status": "False", "reason": SECRET, "message": SECRET,
            "lastTransitionTime": ago(3)},
    ]);
    let three = json!({"replicas": 3});
    let mut problems = pod_problems(&pod);
    problems.extend(classifier().deployment(&deployment(none(), three.clone(), 1, 1, stalled)));
    problems.extend(classifier().deployment(&deployment(none(), three, 1, 3, unknown)));
    assert_eq!(problems.len(), 3);
    for problem in problems {
        let printed = format!("{problem:?} {}", problem.summary());
        assert!(!printed.contains(SECRET), "{printed}");
    }
}

#[test]
fn ten_thousand_pods_classify_quickly() {
    const PODS: usize = 10_000;
    const BROKEN_EVERY: usize = 10;
    // Far above the few milliseconds this takes, so a loaded CI runner cannot fail it, but low
    // enough to catch a pass that stops being linear.
    const LIMIT: Duration = Duration::from_secs(2);

    let healthy = pod_with(
        "Running",
        json!([]),
        json!([status(0, running(30), none())]),
        none(),
    );
    let broken = crash_looping_pod("uid-broken", 6, 1);
    let pods: Vec<Pod> = (0..PODS)
        .map(|i| {
            let mut pod = if i % BROKEN_EVERY == 0 {
                broken.clone()
            } else {
                healthy.clone()
            };
            pod.metadata.name = Some(format!("p-{i}"));
            pod.metadata.uid = Some(format!("uid-{i}"));
            pod
        })
        .collect();

    let started = Instant::now();
    let problems = classifier().pods(&pods);
    let elapsed = started.elapsed();

    assert_eq!(problems.len(), PODS / BROKEN_EVERY);
    let distinct: HashSet<_> = problems.iter().map(|p| &p.id).collect();
    assert_eq!(
        distinct.len(),
        problems.len(),
        "every problem has its own id"
    );
    assert!(elapsed < LIMIT, "took {elapsed:?}");
}

#[test]
fn owners_are_read_from_metadata() {
    type Expected = Option<(&'static str, &'static str, &'static str)>;
    let refs = |api: &str, kind: &str, name: &str, controller: bool| {
        json!([{
            "apiVersion": api, "kind": kind, "name": name, "uid": "u", "controller": controller,
        }])
    };
    let rs = |name: &str| refs("apps/v1", "ReplicaSet", name, true);
    let rs_meta = |name: &str, owners: serde_json::Value| {
        Some(json!({"name": name, "uid": "u", "ownerReferences": owners}))
    };
    let deployment_refs = refs("apps/v1", "Deployment", "web", true);
    let owned_by_deployment = |deployment: &str, rs_name: &str| {
        rs_meta(rs_name, refs("apps/v1", "Deployment", deployment, true))
    };
    let cases: Vec<(&str, serde_json::Value, Option<serde_json::Value>, Expected)> = vec![
        (
            "deployment",
            rs("web-abc12"),
            owned_by_deployment("web", "web-abc12"),
            Some(("apps", "Deployment", "web")),
        ),
        (
            "hyphenated deployment",
            rs("my-web-abc12"),
            owned_by_deployment("my-web", "my-web-abc12"),
            Some(("apps", "Deployment", "my-web")),
        ),
        (
            "no replicaset metadata",
            rs("web-abc12"),
            None,
            Some(("apps", "ReplicaSet", "web-abc12")),
        ),
        (
            "orphaned replicaset",
            rs("web-abc12"),
            rs_meta("web-abc12", json!([])),
            Some(("apps", "ReplicaSet", "web-abc12")),
        ),
        (
            "deployment reference is not the controller",
            rs("web-abc12"),
            rs_meta("web-abc12", refs("apps/v1", "Deployment", "web", false)),
            Some(("apps", "ReplicaSet", "web-abc12")),
        ),
        (
            "replicaset owned by another kind",
            rs("web-abc12"),
            rs_meta("web-abc12", refs("apps/v1", "StatefulSet", "web", true)),
            Some(("apps", "ReplicaSet", "web-abc12")),
        ),
        (
            "metadata of a different replicaset",
            rs("web-abc12"),
            owned_by_deployment("web", "web-other"),
            Some(("apps", "ReplicaSet", "web-abc12")),
        ),
        (
            "same name with another uid",
            rs("web-abc12"),
            Some(json!({"name": "web-abc12", "uid": "new", "ownerReferences": deployment_refs})),
            Some(("apps", "ReplicaSet", "web-abc12")),
        ),
        (
            "same name and uid in another namespace",
            rs("web-abc12"),
            Some(json!({
                "name": "web-abc12", "namespace": "other", "uid": "u",
                "ownerReferences": deployment_refs,
            })),
            Some(("apps", "ReplicaSet", "web-abc12")),
        ),
        (
            "other api group",
            refs("example.io/v1", "ReplicaSet", "web-abc12", true),
            owned_by_deployment("web", "web-abc12"),
            Some(("example.io", "ReplicaSet", "web-abc12")),
        ),
        (
            "statefulset",
            refs("apps/v1", "StatefulSet", "db", true),
            None,
            Some(("apps", "StatefulSet", "db")),
        ),
        (
            "core group",
            refs("v1", "ReplicationController", "rc", true),
            None,
            Some(("", "ReplicationController", "rc")),
        ),
        (
            "not a controller",
            refs("apps/v1", "StatefulSet", "db", false),
            None,
            None,
        ),
        ("no owners", json!([]), None, None),
    ];
    for (case, owners, replica_set, expected) in cases {
        let meta: ObjectMeta = serde_json::from_value(json!({"ownerReferences": owners})).unwrap();
        let replica_set: Option<ObjectMeta> =
            replica_set.map(|rs| serde_json::from_value(rs).unwrap());
        let expected = expected.map(|(group, kind, name)| ProblemOwner {
            group: group.into(),
            kind: kind.into(),
            name: name.into(),
        });
        assert_eq!(
            ProblemOwner::of_meta(&meta, replica_set.as_ref()),
            expected,
            "{case}"
        );
    }
}

/// A crash-looping pod in `namespace` whose controller is ReplicaSet `web-abc12` with `rs_uid`.
fn replica_set_pod(namespace: &str, rs_uid: &str) -> Pod {
    let mut pod = crash_looping_pod("uid-1", CRASH_LOOP_MIN_RESTARTS, 1);
    pod.metadata.namespace = Some(namespace.to_owned());
    pod.metadata.owner_references = serde_json::from_value(json!([{
        "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "web-abc12",
        "uid": rs_uid, "controller": true,
    }]))
    .unwrap();
    pod
}

fn replica_set_meta(namespace: &str, uid: &str, deployment: &str) -> ObjectMeta {
    serde_json::from_value(json!({
        "name": "web-abc12", "namespace": namespace, "uid": uid,
        "ownerReferences": [{
            "apiVersion": "apps/v1", "kind": "Deployment", "name": deployment,
            "uid": "deployment-uid", "controller": true,
        }],
    }))
    .unwrap()
}

fn owner(group: &str, kind: &str, name: &str) -> Option<ProblemOwner> {
    Some(ProblemOwner {
        group: group.into(),
        kind: kind.into(),
        name: name.into(),
    })
}

#[test]
fn a_pod_problem_resolves_its_replica_set_to_the_deployment() {
    let pod = replica_set_pod("ns", "rs-uid");
    let fallback = owner("apps", "ReplicaSet", "web-abc12");
    let cases = [
        (
            "matching metadata",
            vec![replica_set_meta("ns", "rs-uid", "web")],
            owner("apps", "Deployment", "web"),
        ),
        ("no metadata", vec![], fallback.clone()),
        (
            "same name in another namespace",
            vec![replica_set_meta("other", "rs-other", "elsewhere")],
            fallback.clone(),
        ),
        (
            "recreated with a new uid",
            vec![replica_set_meta("ns", "rs-new", "web")],
            fallback.clone(),
        ),
        (
            "uid in another namespace",
            vec![replica_set_meta("other", "rs-uid", "elsewhere")],
            fallback,
        ),
    ];
    for (case, replica_sets, expected) in cases {
        let index: ReplicaSetIndex<'_> = replica_sets.iter().collect();
        let problems = classifier().pods_with_replica_sets([&pod], &index);
        assert_eq!(problems.len(), 1, "{case}");
        assert_eq!(problems[0].owner, expected, "{case}");
    }
}

#[test]
fn an_owner_is_carried_for_every_subject_and_is_not_identity() {
    let pod = replica_set_pod("ns", "rs-uid");
    let plain = classifier().pods([&pod]);
    assert_eq!(plain[0].owner, owner("apps", "ReplicaSet", "web-abc12"));

    let replica_sets = [replica_set_meta("ns", "rs-uid", "web")];
    let index: ReplicaSetIndex<'_> = replica_sets.iter().collect();
    let resolved = classifier().pods_with_replica_sets([&pod], &index);
    assert_eq!(plain[0].id, resolved[0].id, "the owner is not in the id");
    assert_eq!(plain[0].hold_down(), resolved[0].hold_down());

    let mut job_pod = crash_looping_pod("uid-1", CRASH_LOOP_MIN_RESTARTS, 1);
    job_pod.metadata.owner_references = serde_json::from_value(json!([{
        "apiVersion": "batch/v1", "kind": "Job", "name": "backup", "uid": "j", "controller": true,
    }]))
    .unwrap();
    assert_eq!(
        classifier().pods([&job_pod])[0].owner,
        owner("batch", "Job", "backup")
    );

    let conditions = json!([job_condition("Failed", "True", "BackoffLimitExceeded")]);
    let mut failed = job(conditions, false);
    assert_eq!(classifier().job(&failed).unwrap().owner, None);
    failed.metadata.owner_references = serde_json::from_value(json!([{
        "apiVersion": "batch/v1", "kind": "CronJob", "name": "nightly", "uid": "c",
        "controller": true,
    }]))
    .unwrap();
    assert_eq!(
        classifier().job(&failed).unwrap().owner,
        owner("batch", "CronJob", "nightly")
    );

    let down = node(60, Some(("False", "KubeletNotReady", 30)), false);
    assert_eq!(classifier().node(&down).unwrap().owner, None);

    let meta = ObjectMeta {
        name: Some("db".into()),
        namespace: Some("ns".into()),
        ..ObjectMeta::default()
    };
    for (kind, expected) in [
        (WorkloadKind::Deployment, "Deployment"),
        (WorkloadKind::StatefulSet, "StatefulSet"),
        (WorkloadKind::DaemonSet, "DaemonSet"),
    ] {
        let problem = classifier().workload(kind, &meta, None, None).unwrap();
        assert_eq!(problem.owner, owner("apps", expected, "db"));
    }
}

fn resolved(pod: &Pod, index: &ReplicaSetIndex<'_>) -> (Option<ProblemOwner>, bool) {
    let problem = classifier().pods_with_replica_sets([pod], index).remove(0);
    (problem.owner, problem.owner_known)
}

#[test]
fn an_index_tells_a_read_replica_set_from_an_unknown_one() {
    let pod = replica_set_pod("ns", "rs-uid");
    let replica_set = owner("apps", "ReplicaSet", "web-abc12");

    let read = [replica_set_meta("ns", "rs-uid", "web")];
    let index: ReplicaSetIndex<'_> = read.iter().collect();
    assert_eq!(
        resolved(&pod, &index),
        (owner("apps", "Deployment", "web"), true)
    );

    let mut orphan = replica_set_meta("ns", "rs-uid", "web");
    orphan.owner_references = None;
    let orphans = [orphan];
    let index: ReplicaSetIndex<'_> = orphans.iter().collect();
    assert_eq!(
        resolved(&pod, &index),
        (replica_set.clone(), true),
        "read with no Deployment: the ReplicaSet is the real owner"
    );

    let elsewhere = [replica_set_meta("other", "rs-uid", "web")];
    let recreated = [replica_set_meta("ns", "rs-new", "web")];
    for index in [
        ReplicaSetIndex::unavailable(),
        std::iter::empty::<&ObjectMeta>().collect(),
        elsewhere.iter().collect(),
        recreated.iter().collect(),
    ] {
        assert_eq!(resolved(&pod, &index), (replica_set.clone(), false));
    }
    assert!(
        !classifier().pods([&pod])[0].owner_known,
        "the plain entry point cannot pass a ReplicaSet off as a known owner"
    );
}

#[test]
fn only_a_replica_set_owner_can_be_unknown() {
    let mut stateful = crash_looping_pod("uid-1", CRASH_LOOP_MIN_RESTARTS, 1);
    stateful.metadata.owner_references = serde_json::from_value(json!([{
        "apiVersion": "apps/v1", "kind": "StatefulSet", "name": "db", "uid": "s",
        "controller": true,
    }]))
    .unwrap();
    let bare = crash_looping_pod("uid-2", CRASH_LOOP_MIN_RESTARTS, 1);
    for pod in [&stateful, &bare] {
        assert!(classifier().pods([pod])[0].owner_known);
    }
}
