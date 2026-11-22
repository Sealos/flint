use super::*;
use crate::problems::{ProblemId, WorkloadKind};

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
