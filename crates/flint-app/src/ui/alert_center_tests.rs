//! The alert decisions over real telemetry snapshots, with time passed in.

use std::path::PathBuf;
use std::sync::Arc;

use flint_core::alerts::AlertEvent;
use flint_core::dashboard::{ProblemSnapshot, ProblemSummariser};
use flint_core::discovery::ClusterDiscovery;
use flint_core::handle::{ClusterSummary, ConnectionState};
use flint_core::kubeconfig::ContextInfo;
use flint_core::problems::{Classifier, Problem, ProblemKind};
use flint_core::settings::WatchMode;
use flint_core::telemetry::{
    Availability, KindCheck, MetricsCheck, TelemetrySnapshot, WatchedKind,
};
use jiff::Timestamp;
use k8s_openapi::api::core::v1::{Node, NodeCondition, NodeStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use super::{
    ALL_KINDS, AlertOrigin, AlertTracker, Delivery, MAX_RECENT, Reading, TrackerInput,
    check_origin, out_of_scope_kinds, reading,
};
use crate::notifications::{DeliveryError, Notification};

const PROD: &str = "prod";
const SERVER: &str = "https://prod.example:6443";

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 + seconds).unwrap()
}

fn summary(name: &str, state: ConnectionState) -> ClusterSummary {
    ClusterSummary {
        info: ContextInfo {
            name: name.into(),
            cluster: Some(name.into()),
            user: None,
            namespace: None,
            server: Some(SERVER.into()),
            proxy_url: None,
            source: PathBuf::from("kubeconfig"),
        },
        state,
        generation: 1,
        definition: 1,
        endpoint: 1,
        confirmed: true,
        provisional: false,
        paused: false,
    }
}

fn connected(name: &str) -> ClusterSummary {
    let version = flint_core::ServerVersion {
        git_version: "v1".into(),
        platform: "linux/arm64".into(),
    };
    summary(name, ConnectionState::Connected(version))
}

fn bad_node(cluster: &str, name: &str) -> Problem {
    let node = Node {
        metadata: ObjectMeta {
            name: Some(name.into()),
            uid: Some(name.into()),
            ..ObjectMeta::default()
        },
        status: Some(NodeStatus {
            conditions: Some(vec![NodeCondition {
                type_: "Ready".into(),
                status: "False".into(),
                ..NodeCondition::default()
            }]),
            ..NodeStatus::default()
        }),
        ..Node::default()
    };
    Classifier::new(cluster, at(0)).node(&node).unwrap()
}

fn snapshot(cluster: &str, problems: Vec<Problem>, now: Timestamp) -> TelemetrySnapshot {
    let counts = ProblemSummariser::default().summarise(&ProblemSnapshot::unavailable(), now);
    TelemetrySnapshot {
        cluster: Arc::from(cluster),
        at: now,
        availability: Availability::Checked,
        kinds: WatchedKind::ALL.map(|kind| (kind, KindCheck::Current)),
        problems: problems.into(),
        unchecked: Vec::new(),
        counts,
        metrics: MetricsCheck::Waiting,
    }
}

fn complete() -> ClusterDiscovery {
    ClusterDiscovery::new(Vec::new())
}

fn tracker() -> AlertTracker {
    AlertTracker::new("test".to_owned())
}

fn input<'a>(cluster: &ClusterSummary, reading: Reading<'a>) -> TrackerInput<'a> {
    TrackerInput {
        cluster: Arc::from(cluster.info.name.as_str()),
        origin: AlertOrigin::of(cluster),
        label: cluster.info.name.clone(),
        reading,
    }
}

/// One pass of `cluster` reading `problems` with complete discovery.
fn feed(
    tracker: &mut AlertTracker,
    cluster: &ClusterSummary,
    problems: Vec<Problem>,
    now: Timestamp,
) -> Vec<Notification> {
    let snapshot = snapshot(&cluster.info.name, problems, now);
    let discovery = complete();
    let reading = Reading::Feed {
        snapshot: &snapshot,
        discovery: Some(&discovery),
    };
    tracker.observe(&[input(cluster, reading)], now)
}

/// Takes a healthy baseline, then raises one alert on `name`.
fn raised(tracker: &mut AlertTracker, cluster: &ClusterSummary, name: &str) -> Notification {
    let id = &cluster.info.name;
    assert!(feed(tracker, cluster, Vec::new(), at(0)).is_empty());
    let mut sent = feed(tracker, cluster, vec![bad_node(id, name)], at(600));
    assert_eq!(sent.len(), 1, "{sent:?}");
    sent.remove(0)
}

#[test]
fn a_new_problem_notifies_once_and_names_the_cluster() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let sent = raised(&mut tracker, &prod, "n1");
    assert!(sent.title.contains(PROD));
    assert_eq!(&*sent.cluster, PROD);
    assert!(tracker.find(&sent.tag).is_some());
    let again = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(615));
    assert!(again.is_empty(), "{again:?}");
}

#[test]
fn what_already_exists_when_first_checked_is_a_baseline() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    assert!(feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(0)).is_empty());
    assert!(feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(600)).is_empty());
}

#[test]
fn a_reconnect_does_not_repeat_open_alerts() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let lost = summary(PROD, ConnectionState::Connecting);
    for second in [610, 620, 630] {
        let reading = reading(&lost, WatchMode::All, None, None);
        assert!(matches!(reading, Reading::Lost));
        assert!(
            tracker
                .observe(&[input(&lost, reading)], at(second))
                .is_empty()
        );
    }
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(640));
    assert!(back.is_empty(), "{back:?}");
}

#[test]
fn a_feed_restarting_while_connected_keeps_open_alerts() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let gap = reading(&prod, WatchMode::All, None, None);
    assert!(matches!(gap, Reading::NotWatched));
    assert!(tracker.observe(&[input(&prod, gap)], at(610)).is_empty());
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(620));
    assert!(back.is_empty(), "{back:?}");
}

#[test]
fn a_long_outage_is_reported_once_and_recovery_raises_nothing_old() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let lost = summary(
        PROD,
        ConnectionState::Failed(flint_core::SafeErrorText::reduced("x")),
    );
    let pass = |tracker: &mut AlertTracker, second| {
        tracker.observe(&[input(&lost, Reading::Lost)], at(second))
    };
    assert!(pass(&mut tracker, 610).is_empty());
    let reported = pass(&mut tracker, 800);
    assert_eq!(reported.len(), 1);
    assert!(reported[0].title.starts_with("Cluster unavailable"));
    assert!(pass(&mut tracker, 900).is_empty());
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(950));
    assert_eq!(back.len(), 1);
    assert!(back[0].title.starts_with("Cluster available"));
}

#[test]
fn missing_discovery_claims_nothing() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    // The node is gone, but with no discovery that proves nothing: it must not resolve.
    for second in [700, 1400, 2100] {
        let healthy = snapshot(PROD, Vec::new(), at(second));
        let reading = Reading::Feed {
            snapshot: &healthy,
            discovery: None,
        };
        let sent = tracker.observe(&[input(&prod, reading)], at(second));
        assert!(sent.is_empty(), "{sent:?}");
    }
}

#[test]
fn incomplete_discovery_makes_only_its_kinds_unchecked() {
    let incomplete =
        |group: &str| ClusterDiscovery::with_incomplete_groups(Vec::new(), vec![group.to_owned()]);
    assert_eq!(out_of_scope_kinds(None).len(), ALL_KINDS.len());
    assert!(out_of_scope_kinds(Some(&complete())).is_empty());
    assert!(out_of_scope_kinds(Some(&incomplete("metrics.k8s.io"))).is_empty());
    assert_eq!(
        out_of_scope_kinds(Some(&incomplete("BATCH"))),
        [ProblemKind::JobFailed]
    );
    let apps = out_of_scope_kinds(Some(&incomplete("apps")));
    assert!(apps.contains(&ProblemKind::WorkloadShort));
    assert!(apps.contains(&ProblemKind::CrashLoop));
    assert!(!apps.contains(&ProblemKind::NodeNotReady));
}

#[test]
fn a_problem_of_an_incomplete_group_raises_nothing() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let core_incomplete =
        ClusterDiscovery::with_incomplete_groups(Vec::new(), vec!["core".to_owned()]);
    assert!(feed(&mut tracker, &prod, Vec::new(), at(0)).is_empty());
    for second in [1, 600] {
        let bad = snapshot(PROD, vec![bad_node(PROD, "n1")], at(second));
        let reading = Reading::Feed {
            snapshot: &bad,
            discovery: Some(&core_incomplete),
        };
        assert!(
            tracker
                .observe(&[input(&prod, reading)], at(second))
                .is_empty()
        );
    }
}

#[test]
fn clusters_with_no_feed_of_their_own_are_never_read_as_lost() {
    let on_view = connected("view");
    assert!(matches!(
        reading(&on_view, WatchMode::OnView, None, None),
        Reading::NotWatched
    ));
    let idle = summary("idle", ConnectionState::Disconnected);
    assert!(matches!(
        reading(&idle, WatchMode::All, None, None),
        Reading::NotWatched
    ));
    let mut paused = connected("paused");
    paused.paused = true;
    assert!(matches!(
        reading(&paused, WatchMode::All, None, None),
        Reading::Lost
    ));
}

#[test]
fn a_cluster_left_unwatched_starts_over() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let unwatched = |tracker: &mut AlertTracker, second| {
        tracker.observe(&[input(&prod, Reading::NotWatched)], at(second))
    };
    assert!(unwatched(&mut tracker, 610).is_empty());
    assert!(unwatched(&mut tracker, 700).is_empty());
    // Back later with the same problem: a new baseline, so no repeat.
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(800));
    assert!(back.is_empty(), "{back:?}");
}

#[test]
fn a_repointed_context_shares_no_history() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let mut moved = connected(PROD);
    moved.generation = 2;
    moved.definition = 2;
    moved.info.server = Some("https://other.example:6443".into());
    // The new cluster's first check is a baseline: nothing about the old one is reported.
    let sent = feed(&mut tracker, &moved, vec![bad_node(PROD, "n1")], at(610));
    assert!(sent.is_empty(), "{sent:?}");
}

#[test]
fn turning_off_and_on_does_not_replay_a_backlog() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    tracker.reset();
    let bad = vec![bad_node(PROD, "n1"), bad_node(PROD, "n2")];
    assert!(feed(&mut tracker, &prod, bad.clone(), at(700)).is_empty());
    assert!(feed(&mut tracker, &prod, bad, at(1400)).is_empty());
}

#[test]
fn an_alert_only_opens_the_cluster_that_raised_it() {
    let prod = connected(PROD);
    let origin = AlertOrigin::of(&prod);
    let other = connected("other");

    let clusters = [other.clone(), prod.clone()];
    let opened = check_origin(PROD, &origin, &clusters, PROD).unwrap();
    assert_eq!(opened.info.name, PROD);

    let removed = check_origin(PROD, &origin, std::slice::from_ref(&other), PROD).unwrap_err();
    assert!(removed.contains("removed"), "{removed}");

    let mut repointed = prod.clone();
    repointed.info.server = Some("https://other.example:6443".into());
    let refused = check_origin(PROD, &origin, &[repointed], PROD).unwrap_err();
    assert!(refused.contains("changed"), "{refused}");

    // A move and back keeps the URL but not the definition.
    let mut moved_back = prod.clone();
    moved_back.definition = 2;
    assert!(check_origin(PROD, &origin, &[moved_back], PROD).is_err());

    let mut recreated = prod.clone();
    recreated.generation = 2;
    assert!(check_origin(PROD, &origin, &[recreated], PROD).is_err());

    let mut paused = prod;
    paused.paused = true;
    let refused = check_origin(PROD, &origin, &[paused], PROD).unwrap_err();
    assert!(refused.contains("paused"), "{refused}");
}

#[test]
fn a_refusal_cleans_the_name_it_shows() {
    let origin = AlertOrigin::of(&connected(PROD));
    let refused = check_origin(PROD, &origin, &[], "a\n<b>").unwrap_err();
    assert!(
        !refused.contains('\n') && !refused.contains('<'),
        "{refused}"
    );
}

#[test]
fn tags_differ_between_runs_and_alerts() {
    let prod = connected(PROD);
    let (mut one, mut two) = (AlertTracker::new("1".into()), AlertTracker::new("2".into()));
    let first = raised(&mut one, &prod, "n1");
    let second = raised(&mut two, &prod, "n1");
    assert_ne!(first.tag, second.tag);
    assert!(one.find(&second.tag).is_none());
    let next = feed(
        &mut one,
        &prod,
        vec![bad_node(PROD, "n1"), bad_node(PROD, "n2")],
        at(1300),
    );
    assert!(next.iter().all(|n| n.tag != first.tag));
}

#[test]
fn delivery_records_the_refusal_and_the_list_is_bounded() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let sent = raised(&mut tracker, &prod, "n1");
    assert_eq!(tracker.find(&sent.tag).unwrap().delivery, Delivery::Sending);
    tracker.delivered(&sent.tag, Delivery::Refused(DeliveryError::NotPermitted));
    let recorded = &tracker.find(&sent.tag).unwrap().delivery;
    assert_eq!(recorded.state(), "not_permitted");
    assert!(recorded.text().starts_with("Not delivered"));
    assert!(!Delivery::Accepted.text().contains("shown"));

    let input = input(&prod, Reading::Lost);
    for _ in 0..MAX_RECENT + 5 {
        tracker.record(
            AlertEvent::ClusterAvailable {
                cluster: Arc::from(PROD),
            },
            &input,
        );
    }
    assert_eq!(tracker.recent().count(), MAX_RECENT);
}
