use super::*;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use crate::problems::{Classifier, ProblemId, ReplicaSetIndex, WorkloadKind};

const CLUSTER: &str = "cluster-a";

fn at(minute: i64) -> Timestamp {
    let start: Timestamp = "2026-06-01T12:00:00Z".parse().unwrap();
    start + SignedDuration::from_secs(minute * 60)
}

fn cluster() -> Arc<str> {
    Arc::from(CLUSTER)
}

fn config() -> AlertConfig {
    AlertConfig {
        cooldown: SignedDuration::from_secs(10 * 60),
        burst_limit: 3,
        burst_window: SignedDuration::from_secs(5 * 60),
        unavailable_after: SignedDuration::from_secs(2 * 60),
        alert_on_existing: false,
    }
}

fn observation(kind: ProblemKind, subject: ProblemSubject, owner: Option<&str>) -> Problem {
    Problem {
        id: ProblemId {
            cluster: cluster(),
            kind,
            subject,
        },
        owner: owner.map(|name| ProblemOwner::workload(WorkloadKind::Deployment, name)),
        owner_known: true,
        reason: None,
        since: None,
    }
}

/// A crash loop in a pod of Deployment `owner`.
fn crash(owner: &str, pod: &str) -> Problem {
    let subject = ProblemSubject::Container {
        namespace: "ns".into(),
        pod: pod.into(),
        pod_uid: Some(format!("uid-{pod}")),
        container: "c".into(),
    };
    observation(ProblemKind::CrashLoop, subject, Some(owner))
}

fn node(name: &str) -> Problem {
    let subject = ProblemSubject::Node {
        name: name.into(),
        uid: Some(format!("uid-{name}")),
    };
    observation(ProblemKind::NodeNotReady, subject, None)
}

fn job(name: &str) -> Problem {
    let subject = ProblemSubject::Job {
        namespace: "ns".into(),
        name: name.into(),
        uid: Some(format!("uid-{name}")),
    };
    observation(ProblemKind::JobFailed, subject, None)
}

/// A compact form of an event for the expectation tables.
fn render(event: &AlertEvent) -> String {
    let name = |alert: &Alert| match &alert.key.subject {
        AlertSubject::Workload { owner, .. } => owner.name.clone(),
        AlertSubject::Pod { name, .. }
        | AlertSubject::Job { name, .. }
        | AlertSubject::Node { name } => name.clone(),
    };
    match event {
        AlertEvent::Raised(a) => format!("raise {} x{}", name(a), a.affected),
        AlertEvent::Resolved(a) => format!("resolve {}", name(a)),
        AlertEvent::Summary(s) => format!(
            "summary new={} resolved={} untracked={}",
            s.new, s.resolved, s.untracked
        ),
        AlertEvent::ClusterUnavailable { open, .. } => format!("unavailable open={open}"),
        AlertEvent::ClusterAvailable { .. } => "available".to_owned(),
    }
}

enum Pass {
    Up(Vec<Problem>),
    Partial(Vec<Problem>, Vec<ProblemKind>),
    Down,
}
use Pass::{Down, Partial, Up};

/// A pass at a minute, and the events it must produce.
struct Step(i64, Pass, &'static [&'static str]);

fn nodes(names: &[&str]) -> Pass {
    Up(names.iter().map(|name| node(name)).collect())
}

fn no_nodes_checked() -> Pass {
    Partial(vec![], vec![ProblemKind::NodeNotReady])
}

#[test]
fn scenarios() {
    let web = || crash("web", "web-1");
    let cases: Vec<(&str, AlertConfig, Vec<Step>)> = vec![
        (
            "raises once and stays quiet while open",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, Up(vec![web()]), &["raise web x1"]),
                Step(2, Up(vec![web()]), &[]),
                Step(30, Up(vec![web()]), &[]),
            ],
        ),
        (
            "a brief clear inside the cooldown does not re-alert or resolve",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, Up(vec![web()]), &["raise web x1"]),
                Step(2, Up(vec![]), &[]),
                Step(9, Up(vec![web()]), &[]),
                Step(10, Up(vec![]), &[]),
                Step(19, Up(vec![]), &[]),
                Step(20, Up(vec![]), &["resolve web"]),
                Step(21, Up(vec![web()]), &["raise web x1"]),
            ],
        ),
        (
            "a recurrence first seen after the cooldown resolves and raises again",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, Up(vec![web()]), &["raise web x1"]),
                Step(2, Up(vec![]), &[]),
                Step(12, Up(vec![web()]), &["resolve web", "raise web x1"]),
                Step(13, Up(vec![web()]), &[]),
            ],
        ),
        (
            "a baseline group that recurs after the cooldown raises without a resolve",
            config(),
            vec![
                Step(0, Up(vec![web()]), &[]),
                Step(1, Up(vec![]), &[]),
                Step(20, Up(vec![web()]), &["raise web x1"]),
            ],
        ),
        (
            "a rollout's replacement pods stay one alert",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(
                    1,
                    Up(vec![crash("web", "a"), crash("web", "b")]),
                    &["raise web x2"],
                ),
                Step(2, Up(vec![crash("web", "c"), crash("web", "d")]), &[]),
            ],
        ),
        (
            "existing problems are baseline and resolve silently",
            config(),
            vec![
                Step(0, Up(vec![web(), node("n1")]), &[]),
                Step(1, Up(vec![web()]), &[]),
                Step(11, Up(vec![web()]), &[]),
                Step(12, Up(vec![]), &[]),
                Step(22, Up(vec![]), &[]),
                Step(23, Up(vec![web()]), &["raise web x1"]),
            ],
        ),
        (
            "existing problems raise when configured",
            AlertConfig {
                alert_on_existing: true,
                ..config()
            },
            vec![Step(0, Up(vec![web()]), &["raise web x1"])],
        ),
        (
            "an unavailable cluster never resolves and is reported once",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, Up(vec![web()]), &["raise web x1"]),
                Step(2, Down, &[]),
                Step(3, Down, &[]),
                Step(4, Down, &["unavailable open=1"]),
                Step(60, Down, &[]),
                Step(61, Up(vec![web()]), &["available"]),
                Step(62, Down, &[]),
                Step(63, Up(vec![]), &[]),
                Step(72, Up(vec![]), &[]),
                Step(73, Up(vec![]), &["resolve web"]),
            ],
        ),
        (
            "an unavailable pass restarts the cooldown",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, Up(vec![web()]), &["raise web x1"]),
                Step(2, Up(vec![]), &[]),
                Step(11, Down, &[]),
                Step(12, Up(vec![]), &[]),
                Step(21, Up(vec![]), &[]),
                Step(22, Up(vec![]), &["resolve web"]),
            ],
        ),
        (
            "an unchecked kind keeps its alerts and baselines on its first check",
            config(),
            vec![
                Step(0, no_nodes_checked(), &[]),
                Step(1, nodes(&["old"]), &[]),
                Step(2, nodes(&["old", "new"]), &["raise new x1"]),
                Step(3, no_nodes_checked(), &[]),
                Step(30, no_nodes_checked(), &[]),
                Step(31, nodes(&["old", "new"]), &[]),
            ],
        ),
        (
            "a burst becomes a summary, most urgent first",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(
                    1,
                    Up(vec![
                        job("j1"),
                        job("j2"),
                        node("n1"),
                        node("n2"),
                        node("n3"),
                    ]),
                    &[
                        "raise n1 x1",
                        "raise n2 x1",
                        "raise n3 x1",
                        "summary new=2 resolved=0 untracked=0",
                    ],
                ),
                Step(
                    2,
                    Up(vec![
                        job("j1"),
                        job("j2"),
                        node("n1"),
                        node("n2"),
                        node("n3"),
                    ]),
                    &[],
                ),
            ],
        ),
        (
            "only one summary per window, the rest wait for the next",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(
                    1,
                    nodes(&["a", "b", "c", "d"]),
                    &[
                        "raise a x1",
                        "raise b x1",
                        "raise c x1",
                        "summary new=1 resolved=0 untracked=0",
                    ],
                ),
                Step(2, nodes(&["a", "b", "c", "d", "e"]), &[]),
                Step(
                    6,
                    nodes(&["a", "b", "c", "d", "e"]),
                    &["summary new=1 resolved=0 untracked=0"],
                ),
            ],
        ),
        (
            "a mass recovery is summarised, and summarised groups stay silent",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(
                    1,
                    nodes(&["a", "b", "c", "d"]),
                    &[
                        "raise a x1",
                        "raise b x1",
                        "raise c x1",
                        "summary new=1 resolved=0 untracked=0",
                    ],
                ),
                Step(
                    6,
                    nodes(&["a", "b", "c", "d", "e", "f"]),
                    &["raise e x1", "raise f x1"],
                ),
                Step(7, Up(vec![]), &[]),
                Step(
                    17,
                    Up(vec![]),
                    &[
                        "resolve a",
                        "resolve b",
                        "resolve c",
                        "summary new=0 resolved=2 untracked=0",
                    ],
                ),
            ],
        ),
        (
            "a clock set back does not stall the cooldown",
            config(),
            vec![
                Step(100, Up(vec![]), &[]),
                Step(101, Up(vec![web()]), &["raise web x1"]),
                Step(102, Up(vec![]), &[]),
                Step(50, Up(vec![]), &[]),
                Step(60, Up(vec![]), &["resolve web"]),
            ],
        ),
    ];
    run_scenarios(cases);
}

fn run_scenarios(cases: Vec<(&str, AlertConfig, Vec<Step>)>) {
    for (name, config, steps) in cases {
        let mut engine = AlertEngine::new(config);
        for (index, Step(minute, pass, expected)) in steps.into_iter().enumerate() {
            let snapshot = match &pass {
                Up(problems) => ClusterSnapshot::complete(problems),
                Partial(problems, unchecked) => ClusterSnapshot::Available {
                    problems,
                    unchecked,
                },
                Down => ClusterSnapshot::Unavailable,
            };
            let events = engine.observe(&cluster(), at(minute), snapshot).unwrap();
            let got: Vec<String> = events.iter().map(render).collect();
            assert_eq!(got, expected, "{name}: step {index}");
        }
    }
}

/// A replica shortfall of workload `name`, whose object has UID `uid`.
fn shortfall(kind: WorkloadKind, name: &str, uid: &str) -> Problem {
    let subject = ProblemSubject::Workload {
        kind,
        namespace: "ns".into(),
        name: name.into(),
        uid: Some(uid.into()),
    };
    observation(ProblemKind::WorkloadShort, subject, None)
}

/// A pass in which StatefulSet `db` is short. Its hold-down is two minutes.
fn db_short() -> Pass {
    Up(vec![shortfall(WorkloadKind::StatefulSet, "db", "uid-db")])
}

/// The same StatefulSet after being deleted and created again.
fn db_recreated() -> Pass {
    Up(vec![shortfall(WorkloadKind::StatefulSet, "db", "uid-new")])
}

fn shortfalls_unchecked() -> Pass {
    Partial(vec![], vec![ProblemKind::WorkloadShort])
}

#[test]
fn hold_down_scenarios() {
    let on_existing = AlertConfig {
        alert_on_existing: true,
        ..config()
    };
    let agent = || Up(vec![shortfall(WorkloadKind::DaemonSet, "agent", "uid-a")]);
    let long_name = "d".repeat(MAX_NAME_BYTES + 1);
    let oversized = || {
        Up(vec![shortfall(
            WorkloadKind::StatefulSet,
            &long_name,
            "uid-long",
        )])
    };
    let cases: Vec<(&str, AlertConfig, Vec<Step>)> = vec![
        (
            "a shortfall raises only once it has lasted the hold-down",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(2, db_short(), &[]),
                Step(3, db_short(), &["raise db x1"]),
                Step(4, db_short(), &[]),
            ],
        ),
        (
            "a DaemonSet shortfall waits too",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, agent(), &[]),
                Step(3, agent(), &["raise agent x1"]),
            ],
        ),
        (
            "a shortfall that clears in time raises and resolves nothing",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(2, Up(vec![]), &[]),
                Step(30, Up(vec![]), &[]),
            ],
        ),
        (
            "the wait restarts when a shortfall returns",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(2, Up(vec![]), &[]),
                Step(3, db_short(), &[]),
                Step(4, db_short(), &[]),
                Step(5, db_short(), &["raise db x1"]),
            ],
        ),
        (
            "a recreated object starts its own wait",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(3, db_recreated(), &[]),
                Step(5, db_recreated(), &["raise db x1"]),
            ],
        ),
        (
            "an unchecked pass keeps the wait running",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(2, shortfalls_unchecked(), &[]),
                Step(3, db_short(), &["raise db x1"]),
            ],
        ),
        (
            "nothing raises or resolves while the kind is unchecked",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(3, shortfalls_unchecked(), &[]),
                Step(30, shortfalls_unchecked(), &[]),
                Step(31, db_short(), &["raise db x1"]),
            ],
        ),
        (
            "an unavailable pass keeps the wait running",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(2, Down, &[]),
                Step(3, db_short(), &["raise db x1"]),
            ],
        ),
        (
            "a clock set back does not extend the wait",
            config(),
            vec![
                Step(100, Up(vec![]), &[]),
                Step(101, db_short(), &[]),
                Step(50, db_short(), &[]),
                Step(51, db_short(), &[]),
                Step(52, db_short(), &["raise db x1"]),
            ],
        ),
        (
            "a clock set back during an unchecked pass does not extend the wait",
            config(),
            vec![
                Step(100, Up(vec![]), &[]),
                Step(101, db_short(), &[]),
                Step(50, shortfalls_unchecked(), &[]),
                Step(52, db_short(), &["raise db x1"]),
            ],
        ),
        (
            "an open alert that returns inside the cooldown does not wait",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(3, db_short(), &["raise db x1"]),
                Step(4, Up(vec![]), &[]),
                Step(5, db_short(), &[]),
                Step(6, Up(vec![]), &[]),
                Step(15, Up(vec![]), &[]),
                Step(16, Up(vec![]), &["resolve db"]),
            ],
        ),
        (
            "an open alert survives its object being recreated",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(3, db_short(), &["raise db x1"]),
                Step(4, db_recreated(), &[]),
                Step(20, db_recreated(), &[]),
            ],
        ),
        (
            "a return after the cooldown resolves, then waits again",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, db_short(), &[]),
                Step(3, db_short(), &["raise db x1"]),
                Step(4, Up(vec![]), &[]),
                Step(15, db_short(), &["resolve db"]),
                Step(16, db_short(), &[]),
                Step(17, db_short(), &["raise db x1"]),
            ],
        ),
        (
            "a shortfall at the first check stays baseline past the hold-down",
            config(),
            vec![
                Step(0, db_short(), &[]),
                Step(3, db_short(), &[]),
                Step(30, db_short(), &[]),
            ],
        ),
        (
            "alerting on existing problems still waits for the hold-down",
            on_existing,
            vec![
                Step(0, db_short(), &[]),
                Step(1, db_short(), &[]),
                Step(2, db_short(), &["raise db x1"]),
            ],
        ),
        (
            "a Deployment shortfall has no hold-down",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(
                    1,
                    Up(vec![shortfall(WorkloadKind::Deployment, "web", "uid-web")]),
                    &["raise web x1"],
                ),
            ],
        ),
        (
            "a waiting shortfall with an oversized name is not summarised early",
            config(),
            vec![
                Step(0, Up(vec![]), &[]),
                Step(1, oversized(), &[]),
                Step(3, oversized(), &["summary new=0 resolved=0 untracked=1"]),
            ],
        ),
    ];
    run_scenarios(cases);
}

#[test]
fn a_waiting_shortfall_is_not_listed_as_open() {
    let mut engine = AlertEngine::new(config());
    let short = [shortfall(WorkloadKind::StatefulSet, "db", "uid-db")];
    let empty = ClusterSnapshot::complete(&[]);
    engine.observe(&cluster(), at(0), empty).unwrap();
    let up = ClusterSnapshot::complete(&short);
    engine.observe(&cluster(), at(1), up).unwrap();
    assert!(engine.open(CLUSTER).is_empty());
}

#[test]
fn forget_clears_the_wait() {
    let mut engine = AlertEngine::new(AlertConfig {
        alert_on_existing: true,
        ..config()
    });
    let short = [shortfall(WorkloadKind::StatefulSet, "db", "uid-db")];
    let up = ClusterSnapshot::complete(&short);
    assert!(engine.observe(&cluster(), at(0), up).unwrap().is_empty());
    engine.forget(CLUSTER);
    assert!(engine.observe(&cluster(), at(2), up).unwrap().is_empty());
    let raised = engine.observe(&cluster(), at(4), up).unwrap();
    let raised: Vec<String> = raised.iter().map(render).collect();
    assert_eq!(raised, ["raise db x1"]);
}

#[test]
fn open_reports_unknown_while_unavailable() {
    let mut engine = AlertEngine::new(config());
    let seen = [node("n1")];
    let up = ClusterSnapshot::complete(&seen);
    engine.observe(&cluster(), at(0), up).unwrap();
    assert_eq!(engine.open(CLUSTER)[0].state, OpenAlertState::Open);
    engine
        .observe(&cluster(), at(1), ClusterSnapshot::Unavailable)
        .unwrap();
    assert_eq!(engine.open(CLUSTER)[0].state, OpenAlertState::Unknown);
    engine.observe(&cluster(), at(2), up).unwrap();
    assert_eq!(engine.open(CLUSTER)[0].state, OpenAlertState::Open);
}

#[test]
fn unavailable_before_any_read_tracks_nothing() {
    let mut engine = AlertEngine::new(config());
    for minute in [0, 10] {
        let events = engine.observe(&cluster(), at(minute), ClusterSnapshot::Unavailable);
        assert_eq!(events, Ok(vec![]));
    }
    assert!(engine.clusters.is_empty());
}

#[test]
fn tracking_is_bounded_and_overflow_is_summarised_once() {
    let mut engine = AlertEngine::new(config());
    engine
        .observe(&cluster(), at(0), ClusterSnapshot::complete(&[]))
        .unwrap();
    let extra = 10;
    let many: Vec<Problem> = (0..MAX_TRACKED_PER_CLUSTER + extra)
        .map(|i| node(&format!("n{i}")))
        .collect();
    let snapshot = ClusterSnapshot::complete(&many);
    let events = engine.observe(&cluster(), at(1), snapshot).unwrap();
    assert_eq!(engine.open(CLUSTER).len(), MAX_TRACKED_PER_CLUSTER);
    let Some(AlertEvent::Summary(summary)) = events.last() else {
        panic!("no summary: {events:?}");
    };
    assert_eq!(summary.untracked as usize, extra);
    let events = engine.observe(&cluster(), at(7), snapshot).unwrap();
    assert_eq!(events, vec![], "overflow must not repeat");
}

#[test]
fn warnings_cannot_take_the_room_kept_for_critical_groups() {
    let mut engine = AlertEngine::new(config());
    engine
        .observe(&cluster(), at(0), ClusterSnapshot::complete(&[]))
        .unwrap();
    let mut seen: Vec<Problem> = (0..MAX_TRACKED_PER_CLUSTER)
        .map(|i| job(&format!("j{i}")))
        .collect();
    engine
        .observe(&cluster(), at(1), ClusterSnapshot::complete(&seen))
        .unwrap();
    seen.push(node("n1"));
    let events = engine
        .observe(&cluster(), at(7), ClusterSnapshot::complete(&seen))
        .unwrap();
    let got: Vec<String> = events.iter().map(render).collect();
    assert!(got.contains(&"raise n1 x1".to_owned()), "{got:?}");
}

#[test]
fn growing_overflow_is_summarised_with_its_severity() {
    let mut engine = AlertEngine::new(config());
    engine
        .observe(&cluster(), at(0), ClusterSnapshot::complete(&[]))
        .unwrap();
    let nodes =
        |count: usize| -> Vec<Problem> { (0..count).map(|i| node(&format!("n{i:04}"))).collect() };
    let first = nodes(MAX_TRACKED_PER_CLUSTER + 1);
    engine
        .observe(&cluster(), at(1), ClusterSnapshot::complete(&first))
        .unwrap();
    let grown = nodes(MAX_TRACKED_PER_CLUSTER + 3);
    let events = engine
        .observe(&cluster(), at(7), ClusterSnapshot::complete(&grown))
        .unwrap();
    let Some(AlertEvent::Summary(summary)) = events.last() else {
        panic!("no summary: {events:?}");
    };
    assert_eq!(summary.untracked, 2);
    assert_eq!(summary.highest, Some(ProblemSeverity::Critical));
}

#[test]
fn resolved_text_states_what_was_observed() {
    let cases = [
        (
            ProblemKind::OomKilled,
            "Out-of-memory kill no longer reported",
        ),
        (
            ProblemKind::NodeNotReady,
            "Node no longer reported as not ready",
        ),
        (ProblemKind::JobFailed, "Job failure no longer reported"),
    ];
    for (kind, expected) in cases {
        assert_eq!(resolved_text(kind), expected);
    }
}

#[test]
fn oversized_names_are_not_kept() {
    let mut engine = AlertEngine::new(config());
    engine
        .observe(&cluster(), at(0), ClusterSnapshot::complete(&[]))
        .unwrap();
    let long = [node(&"x".repeat(MAX_NAME_BYTES + 1))];
    let events = engine
        .observe(&cluster(), at(1), ClusterSnapshot::complete(&long))
        .unwrap();
    assert!(engine.open(CLUSTER).is_empty());
    let got: Vec<String> = events.iter().map(render).collect();
    assert_eq!(got, ["summary new=0 resolved=0 untracked=1"]);
}

#[test]
fn cluster_limit_is_enforced_and_forget_frees_a_slot() {
    let mut engine = AlertEngine::new(config());
    let snapshot = ClusterSnapshot::complete(&[]);
    for i in 0..MAX_CLUSTERS {
        let name: Arc<str> = Arc::from(format!("c{i}"));
        engine.observe(&name, at(0), snapshot).unwrap();
    }
    let extra: Arc<str> = Arc::from("extra");
    assert_eq!(
        engine.observe(&extra, at(0), snapshot),
        Err(ClusterLimitReached)
    );
    engine.forget("c0");
    assert!(engine.observe(&extra, at(0), snapshot).is_ok());
}

#[test]
fn observations_of_another_cluster_are_ignored() {
    let mut engine = AlertEngine::new(config());
    let other: Arc<str> = Arc::from("cluster-b");
    engine
        .observe(&other, at(0), ClusterSnapshot::complete(&[]))
        .unwrap();
    let foreign = [node("n1")];
    let events = engine
        .observe(&other, at(1), ClusterSnapshot::complete(&foreign))
        .unwrap();
    assert_eq!(events, vec![]);
    assert!(engine.open("cluster-b").is_empty());
}

#[test]
fn text_is_flints_wording_without_names() {
    let secret = "server-supplied-name";
    let name = || Arc::<str>::from(secret);
    let alert = Alert {
        cluster: name(),
        key: AlertKey {
            kind: ProblemKind::CrashLoop,
            subject: AlertSubject::Node {
                name: secret.into(),
            },
        },
        affected: 2,
        summary: "Container keeps crashing and restarting",
    };
    let summary = AlertSummary {
        cluster: name(),
        new: 2,
        resolved: 1,
        untracked: 0,
        highest: Some(ProblemSeverity::Critical),
    };
    let unavailable = |open| AlertEvent::ClusterUnavailable {
        cluster: name(),
        open,
    };
    let cases = [
        (
            AlertEvent::Raised(alert.clone()),
            "Container keeps crashing and restarting (2 affected)",
        ),
        (AlertEvent::Resolved(alert), "Crash loop no longer reported"),
        (
            AlertEvent::Summary(summary),
            "2 new problems, 1 resolved problem",
        ),
        (
            unavailable(1),
            "Cluster can no longer be checked; 1 open problem now unknown",
        ),
        (unavailable(0), "Cluster can no longer be checked"),
        (
            AlertEvent::ClusterAvailable { cluster: name() },
            "Cluster can be checked again",
        ),
    ];
    for (event, expected) in cases {
        assert_eq!(event.text(), expected);
        assert!(!event.text().contains(secret));
    }
}

/// A crash-looping pod controlled by ReplicaSet `replica_set`, whose UID is `uid-<replica_set>`.
fn replica_set_pod(name: &str, replica_set: &str) -> Pod {
    serde_json::from_value(serde_json::json!({
        "metadata": {
            "name": name, "namespace": "ns", "uid": format!("uid-{name}"),
            "ownerReferences": [{
                "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": replica_set,
                "uid": format!("uid-{replica_set}"), "controller": true,
            }],
        },
        "spec": {"containers": [{"name": "c"}]},
        "status": {"phase": "Running", "containerStatuses": [{
            "name": "c", "image": "i", "imageID": "", "ready": false, "restartCount": 5,
            "state": {"waiting": {"reason": "CrashLoopBackOff"}},
        }]},
    }))
    .unwrap()
}

/// The metadata of ReplicaSet `name`, controlled by Deployment `deployment` if given.
fn replica_set(name: &str, deployment: Option<&str>) -> ObjectMeta {
    let owners: Vec<_> = deployment
        .into_iter()
        .map(|deployment| {
            serde_json::json!({
                "apiVersion": "apps/v1", "kind": "Deployment", "name": deployment,
                "uid": format!("uid-{deployment}"), "controller": true,
            })
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "name": name, "namespace": "ns", "uid": format!("uid-{name}"), "ownerReferences": owners,
    }))
    .unwrap()
}

/// An engine fed from classified pods and a ReplicaSet read.
struct OwnerHarness {
    engine: AlertEngine,
}

impl OwnerHarness {
    fn new() -> Self {
        Self {
            engine: AlertEngine::new(config()),
        }
    }

    /// One pass with `replica_sets` as the read metadata, or none if it could not be read.
    fn pass(
        &mut self,
        minute: i64,
        pods: &[Pod],
        replica_sets: Option<&[ObjectMeta]>,
    ) -> Vec<String> {
        let index: ReplicaSetIndex<'_> = match replica_sets {
            Some(metas) => metas.iter().collect(),
            None => ReplicaSetIndex::unavailable(),
        };
        let problems = Classifier::new(CLUSTER, at(minute)).pods_with_replica_sets(pods, &index);
        let snapshot = ClusterSnapshot::complete(&problems);
        let events = self
            .engine
            .observe(&cluster(), at(minute), snapshot)
            .unwrap();
        events.iter().map(render).collect()
    }

    fn open_names(&self) -> Vec<String> {
        let open = self.engine.open(CLUSTER);
        open.iter()
            .map(|alert| match &alert.key.subject {
                AlertSubject::Workload { owner, .. } => format!("{} {}", owner.kind, owner.name),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn open_states(&self) -> Vec<OpenAlertState> {
        let open = self.engine.open(CLUSTER);
        open.iter().map(|alert| alert.state).collect()
    }

    /// Passes over `minutes` that must all produce no event.
    fn quiet(
        &mut self,
        minutes: std::ops::RangeInclusive<i64>,
        pods: &[Pod],
        replica_sets: Option<&[ObjectMeta]>,
    ) {
        for minute in minutes {
            let events = self.pass(minute, pods, replica_sets);
            assert!(events.is_empty(), "minute {minute}: {events:?}");
        }
    }
}

#[test]
fn owner_metadata_going_missing_neither_duplicates_nor_resolves() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let read = [replica_set("web-abc12", Some("web"))];
    let mut owners = OwnerHarness::new();
    assert!(owners.pass(0, &[], Some(&read)).is_empty());
    assert_eq!(owners.pass(1, &pods, Some(&read)), ["raise web x1"]);
    owners.quiet(2..=30, &pods, None);
    assert_eq!(owners.open_names(), ["Deployment web"]);
    assert_eq!(owners.open_states(), [OpenAlertState::Unknown]);
}

#[test]
fn owner_metadata_coming_back_neither_duplicates_nor_resolves() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let read = [replica_set("web-abc12", Some("web"))];
    let mut owners = OwnerHarness::new();
    assert!(owners.pass(0, &[], Some(&read)).is_empty());
    assert_eq!(owners.pass(1, &pods, Some(&read)), ["raise web x1"]);
    assert!(owners.pass(2, &pods, None).is_empty());
    owners.quiet(3..=30, &pods, Some(&read));
    assert_eq!(owners.open_names(), ["Deployment web"]);
    assert_eq!(owners.open_states(), [OpenAlertState::Open]);
}

#[test]
fn a_partial_replica_set_read_does_not_split_a_deployment() {
    let pods = [
        replica_set_pod("web-abc12-1", "web-abc12"),
        replica_set_pod("web-def34-1", "web-def34"),
    ];
    let partial = [replica_set("web-abc12", Some("web"))];
    let full = [
        replica_set("web-abc12", Some("web")),
        replica_set("web-def34", Some("web")),
    ];
    let mut owners = OwnerHarness::new();
    assert!(owners.pass(0, &[], Some(&full)).is_empty());
    owners.quiet(1..=14, &pods, Some(&partial));
    assert!(owners.open_names().is_empty(), "nothing raises on a guess");
    assert_eq!(owners.pass(15, &pods, Some(&full)), ["raise web x2"]);
    owners.quiet(16..=20, &pods, Some(&partial));
    owners.quiet(21..=40, &pods, Some(&full));
    assert_eq!(owners.open_names(), ["Deployment web"]);
}

#[test]
fn web_and_web_api_never_affect_each_other() {
    let web = [replica_set_pod("web-abc12-1", "web-abc12")];
    let api = [replica_set_pod("web-api-xyz12-1", "web-api-xyz12")];
    let read = [
        replica_set("web-abc12", Some("web")),
        replica_set("web-api-xyz12", Some("web-api")),
    ];
    let mut owners = OwnerHarness::new();
    owners.pass(0, &[], Some(&read));
    assert_eq!(owners.pass(1, &web, Some(&read)), ["raise web x1"]);
    owners.quiet(2..=30, &api, None);
    assert_eq!(
        owners.pass(31, &api, Some(&read)),
        ["raise web-api x1"],
        "web-api raises as soon as it is known, without waiting for web"
    );
    owners.quiet(32..=40, &api, Some(&read));
    assert_eq!(
        owners.pass(41, &api, Some(&read)),
        ["resolve web"],
        "a web-api pod must not keep web open"
    );
    assert_eq!(owners.open_names(), ["Deployment web-api"]);
}

#[test]
fn a_rollout_during_an_outage_neither_duplicates_nor_resolves() {
    let old = [replica_set_pod("web-abc12-1", "web-abc12")];
    let new = [replica_set_pod("web-def34-1", "web-def34")];
    let read = [
        replica_set("web-abc12", Some("web")),
        replica_set("web-def34", Some("web")),
    ];
    let mut owners = OwnerHarness::new();
    owners.pass(0, &[], Some(&read));
    assert_eq!(owners.pass(1, &old, Some(&read)), ["raise web x1"]);
    owners.quiet(2..=5, &old, None);
    owners.quiet(6..=30, &new, None);
    owners.quiet(31..=60, &new, Some(&read));
    assert_eq!(owners.open_names(), ["Deployment web"]);
    assert_eq!(owners.open_states(), [OpenAlertState::Open]);
}

#[test]
fn a_problem_coming_and_going_in_an_outage_waits_for_its_owner() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let read = [replica_set("web-abc12", Some("web"))];
    let mut owners = OwnerHarness::new();
    owners.pass(0, &[], Some(&read));
    for minute in 1..=240 {
        let seen: &[Pod] = if minute % 2 == 1 { &pods } else { &[] };
        let events = owners.pass(minute, seen, None);
        assert!(events.is_empty(), "minute {minute}: {events:?}");
    }
    assert!(owners.open_names().is_empty());
    assert_eq!(owners.pass(241, &pods, Some(&read)), ["raise web x1"]);
    owners.quiet(242..=250, &pods, Some(&read));

    let mut gone = OwnerHarness::new();
    gone.pass(0, &[], Some(&read));
    gone.quiet(1..=5, &pods, None);
    gone.quiet(6..=20, &[], Some(&read));
    assert!(gone.open_names().is_empty(), "it never raised");
}

#[test]
fn a_real_recovery_in_an_outage_resolves_once_owners_are_known() {
    let web = replica_set_pod("web-abc12-1", "web-abc12");
    let api = replica_set_pod("api-xyz12-1", "api-xyz12");
    let read = [
        replica_set("web-abc12", Some("web")),
        replica_set("api-xyz12", Some("api")),
    ];
    let mut owners = OwnerHarness::new();
    owners.pass(0, &[], Some(&read));
    assert_eq!(
        owners.pass(1, std::slice::from_ref(&web), Some(&read)),
        ["raise web x1"]
    );
    // Web recovers, while another pod of the kind keeps the kind unchecked.
    let rest = [api];
    owners.quiet(2..=40, &rest, None);
    assert_eq!(owners.open_states(), [OpenAlertState::Unknown]);
    assert_eq!(owners.pass(41, &rest, Some(&read)), ["raise api x1"]);
    owners.quiet(42..=50, &rest, Some(&read));
    assert_eq!(owners.pass(51, &rest, Some(&read)), ["resolve web"]);
}

#[test]
fn a_pass_with_no_problem_of_a_kind_is_a_check_even_without_replica_sets() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let read = [replica_set("web-abc12", Some("web"))];
    let mut owners = OwnerHarness::new();
    owners.pass(0, &[], Some(&read));
    assert_eq!(owners.pass(1, &pods, Some(&read)), ["raise web x1"]);
    assert!(owners.pass(2, &pods, None).is_empty());
    owners.quiet(3..=12, &[], None);
    assert_eq!(owners.pass(13, &[], None), ["resolve web"]);
}

#[test]
fn a_new_crash_after_an_outage_raises_once() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let read = [replica_set("web-abc12", Some("web"))];
    let mut owners = OwnerHarness::new();
    owners.pass(0, &[], Some(&read));
    owners.quiet(1..=5, &[], None);
    owners.quiet(6..=10, &pods, None);
    assert_eq!(owners.pass(11, &pods, Some(&read)), ["raise web x1"]);
    owners.quiet(12..=30, &pods, Some(&read));
    assert_eq!(owners.open_names(), ["Deployment web"]);
}

#[test]
fn an_unknown_owner_leaves_other_kinds_checked() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let mut problems = Classifier::new(CLUSTER, at(1)).pods(&pods);
    problems.push(node("n1"));
    let mut engine = AlertEngine::new(config());
    engine
        .observe(&cluster(), at(0), ClusterSnapshot::complete(&[]))
        .unwrap();
    let events = engine
        .observe(&cluster(), at(1), ClusterSnapshot::complete(&problems))
        .unwrap();
    let got: Vec<String> = events.iter().map(render).collect();
    assert_eq!(got, ["raise n1 x1"]);
}

#[test]
fn an_unknown_owner_in_another_cluster_changes_nothing() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let foreign = Classifier::new("cluster-b", at(1)).pods(&pods);
    let unchecked = unchecked_kinds(&cluster(), &foreign, &[]);
    assert!(unchecked.is_empty());
}

#[test]
fn a_new_crash_alerts_as_soon_as_its_owner_is_known() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let read = [replica_set("web-abc12", Some("web"))];
    let mut owners = OwnerHarness::new();
    owners.pass(0, &[], Some(&[]));
    assert!(owners.pass(1, &pods, None).is_empty());
    assert_eq!(owners.pass(2, &pods, Some(&read)), ["raise web x1"]);
    assert_eq!(owners.open_names(), ["Deployment web"]);
}

#[test]
fn a_problem_of_unknown_owner_at_the_first_check_is_baseline() {
    let pods = [replica_set_pod("web-abc12-1", "web-abc12")];
    let read = [replica_set("web-abc12", Some("web"))];
    let mut owners = OwnerHarness::new();
    owners.quiet(0..=5, &pods, None);
    assert!(owners.pass(6, &pods, Some(&read)).is_empty());
    assert_eq!(owners.open_names(), ["Deployment web"]);
}

#[test]
fn a_replica_set_read_with_no_deployment_stays_its_own_owner() {
    let pods = [replica_set_pod("solo-abc12-1", "solo-abc12")];
    let read = [replica_set("solo-abc12", None)];
    let mut owners = OwnerHarness::new();
    owners.pass(0, &[], Some(&read));
    assert_eq!(owners.pass(1, &pods, Some(&read)), ["raise solo-abc12 x1"]);
    owners.quiet(2..=20, &pods, None);
    assert_eq!(owners.open_names(), ["ReplicaSet solo-abc12"]);
}
