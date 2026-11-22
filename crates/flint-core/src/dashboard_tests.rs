use std::sync::Arc;

use k8s_openapi::api::core::v1::Pod;
use serde_json::json;

use super::*;
use crate::problems::{Classifier, WORKLOAD_HOLD_DOWN};
use crate::thresholds::ThresholdOverrides;

const NOW: &str = "2026-06-01T12:00:00Z";

fn now() -> Timestamp {
    NOW.parse().unwrap()
}

fn ago(minutes: i64) -> String {
    (now() - SignedDuration::from_secs(minutes * 60)).to_string()
}

fn problem(kind: ProblemKind, subject: ProblemSubject) -> Problem {
    Problem {
        id: ProblemId {
            cluster: Arc::from("cluster-a"),
            kind,
            subject,
        },
        owner: None,
        owner_known: true,
        reason: None,
        since: None,
    }
}

fn container(pod: &str, uid: &str, container: &str) -> ProblemSubject {
    ProblemSubject::Container {
        namespace: "ns".to_owned(),
        pod: pod.to_owned(),
        pod_uid: Some(uid.to_owned()),
        container: container.to_owned(),
    }
}

fn node(name: &str) -> ProblemSubject {
    ProblemSubject::Node {
        name: name.to_owned(),
        uid: Some(format!("uid-{name}")),
    }
}

fn job(name: &str) -> ProblemSubject {
    ProblemSubject::Job {
        namespace: "ns".to_owned(),
        name: name.to_owned(),
        uid: Some(format!("uid-{name}")),
    }
}

fn workload(kind: WorkloadKind, name: &str) -> Problem {
    problem(
        ProblemKind::WorkloadShort,
        ProblemSubject::Workload {
            kind,
            namespace: "ns".to_owned(),
            name: name.to_owned(),
            uid: Some(format!("uid-{name}")),
        },
    )
}

fn snapshot(problems: &[Problem]) -> ProblemSnapshot<'_> {
    ProblemSnapshot {
        problems,
        unchecked: &[],
        warning_events: 0,
        pending_claims: 0,
    }
}

fn counts(problems: &[Problem]) -> ProblemCounts {
    ProblemSummariser::default().summarise(&snapshot(problems), now())
}

fn counts_of(values: [Option<u32>; 6]) -> ProblemCounts {
    ProblemCounts { counts: values }
}

fn event(value: serde_json::Value) -> Event {
    let mut object = json!({"metadata": {"name": "e", "namespace": "ns"}, "involvedObject": {}});
    object
        .as_object_mut()
        .unwrap()
        .extend(value.as_object().unwrap().clone());
    serde_json::from_value(object).unwrap()
}

fn claim(phase: Option<&str>, deleting: bool) -> PersistentVolumeClaim {
    let mut metadata = json!({"name": "c", "namespace": "ns"});
    if deleting {
        metadata["deletionTimestamp"] = json!(NOW);
    }
    let status = phase.map_or(json!({}), |phase| json!({"phase": phase}));
    serde_json::from_value(json!({"metadata": metadata, "spec": {}, "status": status})).unwrap()
}

#[test]
fn every_kind_counts_under_its_category() {
    let cases = [
        (ProblemKind::CrashLoop, ProblemCategory::FailingPods),
        (ProblemKind::OomKilled, ProblemCategory::FailingPods),
        (ProblemKind::ImagePull, ProblemCategory::FailingPods),
        (ProblemKind::NodeNotReady, ProblemCategory::NotReadyNodes),
        (ProblemKind::JobFailed, ProblemCategory::FailedJobs),
        (ProblemKind::WorkloadShort, ProblemCategory::ShortWorkloads),
    ];
    for (kind, category) in cases {
        assert_eq!(ProblemCategory::of_kind(kind), category);
    }
}

#[test]
fn all_lists_each_category_at_its_index() {
    for (index, category) in ProblemCategory::ALL.into_iter().enumerate() {
        assert_eq!(category.index(), index);
    }
}

#[test]
fn a_pod_with_several_failing_containers_counts_once() {
    let result = counts(&[
        problem(ProblemKind::CrashLoop, container("a", "1", "one")),
        problem(ProblemKind::ImagePull, container("a", "1", "two")),
        problem(ProblemKind::OomKilled, container("b", "2", "one")),
    ]);
    assert_eq!(result.get(ProblemCategory::FailingPods), Some(2));
}

#[test]
fn a_recreated_pod_is_a_different_pod() {
    let result = counts(&[
        problem(ProblemKind::CrashLoop, container("a", "1", "one")),
        problem(ProblemKind::CrashLoop, container("a", "2", "one")),
    ]);
    assert_eq!(result.get(ProblemCategory::FailingPods), Some(2));
}

#[test]
fn counts_real_classifier_output_per_pod() {
    let statuses = ["one", "two"].map(|name| {
        json!({
            "name": name, "image": "i", "imageID": "", "ready": false, "restartCount": 0,
            "state": {"waiting": {"reason": "ImagePullBackOff"}},
        })
    });
    let pod: Pod = serde_json::from_value(json!({
        "metadata": {
            "name": "p", "namespace": "ns", "uid": "uid-1", "creationTimestamp": ago(30),
        },
        "spec": {"containers": [{"name": "one"}, {"name": "two"}]},
        "status": {"phase": "Pending", "containerStatuses": statuses},
    }))
    .unwrap();
    let problems = Classifier::new("cluster-a", now()).pods([&pod]);
    assert_eq!(problems.len(), 2);
    assert_eq!(counts(&problems).get(ProblemCategory::FailingPods), Some(1));
}

#[test]
fn counts_each_category_and_the_known_total() {
    let problems = [
        problem(ProblemKind::CrashLoop, container("a", "1", "one")),
        problem(ProblemKind::NodeNotReady, node("n1")),
        problem(ProblemKind::NodeNotReady, node("n2")),
        problem(ProblemKind::JobFailed, job("j")),
        workload(WorkloadKind::Deployment, "d"),
    ];
    let result = ProblemSummariser::default().summarise(
        &ProblemSnapshot {
            warning_events: 7,
            pending_claims: 3,
            ..snapshot(&problems)
        },
        now(),
    );
    let expected = [1, 2, 7, 3, 1, 1];
    for (category, count) in ProblemCategory::ALL.into_iter().zip(expected) {
        assert_eq!(result.get(category), Some(count), "{category:?}");
    }
    assert_eq!(result.known_total(), 15);
}

#[test]
fn an_unchecked_category_is_unknown_and_ignores_its_data() {
    let problems = [
        problem(ProblemKind::NodeNotReady, node("n1")),
        problem(ProblemKind::JobFailed, job("j")),
    ];
    let result = ProblemSummariser::default().summarise(
        &ProblemSnapshot {
            unchecked: &[
                ProblemCategory::NotReadyNodes,
                ProblemCategory::WarningEvents,
            ],
            warning_events: 9,
            ..snapshot(&problems)
        },
        now(),
    );
    assert_eq!(result.get(ProblemCategory::NotReadyNodes), None);
    assert_eq!(result.get(ProblemCategory::WarningEvents), None);
    assert_eq!(result.get(ProblemCategory::FailedJobs), Some(1));
    assert_eq!(result.known_total(), 1);
}

#[test]
fn an_unavailable_cluster_is_unknown_everywhere() {
    let result = ProblemSummariser::default().summarise(&ProblemSnapshot::unavailable(), now());
    for category in ProblemCategory::ALL {
        assert_eq!(result.get(category), None);
    }
    assert_eq!(
        result.overall_level(&Thresholds::default()),
        ThresholdLevel::Unknown
    );
}

#[test]
fn a_deployment_shortfall_counts_at_once() {
    let result = counts(&[workload(WorkloadKind::Deployment, "d")]);
    assert_eq!(result.get(ProblemCategory::ShortWorkloads), Some(1));
}

#[test]
fn a_held_shortfall_counts_only_after_its_hold_down() {
    for kind in [WorkloadKind::StatefulSet, WorkloadKind::DaemonSet] {
        let problems = [workload(kind, "w")];
        let mut summariser = ProblemSummariser::default();
        let short = |summariser: &mut ProblemSummariser, at| {
            summariser
                .summarise(&snapshot(&problems), at)
                .get(ProblemCategory::ShortWorkloads)
        };
        assert_eq!(short(&mut summariser, now()), Some(0));
        let before = now() + WORKLOAD_HOLD_DOWN - SignedDuration::from_secs(1);
        assert_eq!(short(&mut summariser, before), Some(0));
        assert_eq!(short(&mut summariser, now() + WORKLOAD_HOLD_DOWN), Some(1));
    }
}

#[test]
fn a_shortfall_that_clears_starts_its_hold_down_again() {
    let problems = [workload(WorkloadKind::DaemonSet, "w")];
    let mut summariser = ProblemSummariser::default();
    summariser.summarise(&snapshot(&problems), now());
    summariser.summarise(&snapshot(&[]), now() + SignedDuration::from_secs(10));
    assert!(summariser.first_seen.is_empty());
    let later = now() + WORKLOAD_HOLD_DOWN;
    let result = summariser.summarise(&snapshot(&problems), later);
    assert_eq!(result.get(ProblemCategory::ShortWorkloads), Some(0));
}

#[test]
fn an_unchecked_pass_keeps_the_hold_down_running() {
    let problems = [workload(WorkloadKind::StatefulSet, "w")];
    let mut summariser = ProblemSummariser::default();
    summariser.summarise(&snapshot(&problems), now());
    summariser.summarise(
        &ProblemSnapshot::unavailable(),
        now() + SignedDuration::from_secs(30),
    );
    let result = summariser.summarise(&snapshot(&problems), now() + WORKLOAD_HOLD_DOWN);
    assert_eq!(result.get(ProblemCategory::ShortWorkloads), Some(1));
}

#[test]
fn a_clock_moved_back_does_not_extend_the_hold_down() {
    let problems = [workload(WorkloadKind::StatefulSet, "w")];
    let mut summariser = ProblemSummariser::default();
    summariser.summarise(&snapshot(&problems), now());
    let earlier = now() - SignedDuration::from_secs(3600);
    summariser.summarise(&snapshot(&problems), earlier);
    let result = summariser.summarise(&snapshot(&problems), earlier + WORKLOAD_HOLD_DOWN);
    assert_eq!(result.get(ProblemCategory::ShortWorkloads), Some(1));
}

#[test]
fn levels_use_the_warning_event_threshold() {
    let mut overrides = ThresholdOverrides::default();
    overrides
        .set(ThresholdMetric::WarningEventsLastHour, Some(3))
        .unwrap();
    let thresholds = Thresholds::default().merged(&overrides);
    let level = |events| {
        counts_of([Some(0), Some(0), events, Some(0), Some(0), Some(0)])
            .level(ProblemCategory::WarningEvents, &thresholds)
    };
    assert_eq!(level(Some(2)), ThresholdLevel::Healthy);
    assert_eq!(level(Some(3)), ThresholdLevel::Warning);
    assert_eq!(level(None), ThresholdLevel::Unknown);
}

#[test]
fn any_count_without_a_threshold_is_a_warning() {
    let thresholds = Thresholds::default();
    let result = counts_of([Some(1), Some(0), Some(0), None, Some(0), Some(0)]);
    let level = |category| result.level(category, &thresholds);
    assert_eq!(level(ProblemCategory::FailingPods), ThresholdLevel::Warning);
    assert_eq!(
        level(ProblemCategory::NotReadyNodes),
        ThresholdLevel::Healthy
    );
    assert_eq!(
        level(ProblemCategory::PendingClaims),
        ThresholdLevel::Unknown
    );
}

#[test]
fn overall_level_never_reports_missing_data_as_healthy() {
    let thresholds = Thresholds::default();
    let level = |values| counts_of(values).overall_level(&thresholds);
    assert_eq!(level([Some(0); 6]), ThresholdLevel::Healthy);
    assert_eq!(
        level([Some(0), None, Some(0), Some(0), Some(0), Some(0)]),
        ThresholdLevel::Unknown
    );
    assert_eq!(
        level([Some(1), None, Some(0), Some(0), Some(0), Some(0)]),
        ThresholdLevel::Warning
    );
}

#[test]
fn totals_add_known_counts_and_name_the_gaps() {
    let clusters = [
        counts_of([Some(2), Some(1), None, Some(0), Some(0), Some(0)]),
        counts_of([Some(3), None, None, Some(1), Some(0), Some(0)]),
    ];
    let totals: ProblemTotals = clusters.iter().collect();
    assert_eq!(totals.known(ProblemCategory::FailingPods), 5);
    assert_eq!(totals.unknown_clusters(ProblemCategory::FailingPods), 0);
    assert_eq!(totals.known(ProblemCategory::NotReadyNodes), 1);
    assert_eq!(totals.unknown_clusters(ProblemCategory::NotReadyNodes), 1);
    assert_eq!(totals.known(ProblemCategory::WarningEvents), 0);
    assert_eq!(totals.unknown_clusters(ProblemCategory::WarningEvents), 2);
}

#[test]
fn totals_saturate() {
    let cluster = counts_of([Some(u32::MAX); 6]);
    let totals: ProblemTotals = [&cluster, &cluster].into_iter().collect();
    assert_eq!(totals.known(ProblemCategory::FailingPods), u32::MAX);
    assert_eq!(cluster.known_total(), u32::MAX);
}

#[test]
fn counts_only_recent_warning_events() {
    let events = [
        event(json!({"type": "Warning", "lastTimestamp": ago(59)})),
        event(json!({"type": "Warning", "lastTimestamp": ago(60)})),
        event(json!({"type": "Normal", "lastTimestamp": ago(1)})),
        event(json!({"type": "Warning"})),
        event(json!({"lastTimestamp": ago(1)})),
    ];
    assert_eq!(count_recent_warnings(&events, now()), 1);
}

#[test]
fn a_warning_event_uses_its_latest_time() {
    let micro = |minutes: i64| {
        (now() - SignedDuration::from_secs(minutes * 60))
            .strftime("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string()
    };
    let events = [
        event(json!({
            "type": "Warning", "eventTime": micro(120),
            "series": {"count": 5, "lastObservedTime": micro(5)},
        })),
        event(json!({"type": "Warning", "eventTime": micro(5)})),
        event(json!({
            "type": "Warning",
            "metadata": {"name": "e", "creationTimestamp": ago(5)},
        })),
        event(
            json!({"type": "Warning", "lastTimestamp": (now() + SignedDuration::from_secs(60)).to_string()}),
        ),
    ];
    assert_eq!(count_recent_warnings(&events, now()), 4);
}

#[test]
fn counts_pending_claims_that_are_not_deleting() {
    let claims = [
        claim(Some("Pending"), false),
        claim(Some("Pending"), true),
        claim(Some("Bound"), false),
        claim(Some("Lost"), false),
        claim(None, false),
    ];
    assert_eq!(count_pending_claims(&claims), 1);
}
