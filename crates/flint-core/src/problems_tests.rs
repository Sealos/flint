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
    serde_json::from_value(json!({
        "metadata": metadata,
        "status": {
            "phase": phase,
            "initContainerStatuses": init,
            "containerStatuses": containers,
        },
    }))
    .unwrap()
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
    assert_eq!(ProblemReason::CrashLoopBackOff.as_str(), "CrashLoopBackOff");
    assert_eq!(ProblemReason::OomKilled.as_str(), "OOMKilled");
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
