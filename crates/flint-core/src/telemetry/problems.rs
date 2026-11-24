//! Turns what a feed holds into a [`TelemetrySnapshot`], using the shared classifier and
//! summariser, on every change and on a timer as grace periods pass.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use jiff::Timestamp;
use tokio::sync::watch;

use super::capacity::{Inventory, capacity_of};
use super::checked::CheckedObjects;
use super::objects::Stores;
use super::{
    Availability, CheckFailure, KindCheck, MetricsCheck, Shared, TelemetrySnapshot,
    TelemetryTiming, VersionCheck, WatchedKind,
};
use crate::dashboard::{ProblemCategory, ProblemSnapshot, ProblemSummariser};
use crate::problems::{Classifier, Problem, ProblemKind, ReplicaSetIndex, ReplicaSetRead};

/// Every kind of problem.
pub(super) const ALL_PROBLEM_KINDS: [ProblemKind; 7] = [
    ProblemKind::CrashLoop,
    ProblemKind::OomKilled,
    ProblemKind::NodeNotReady,
    ProblemKind::JobFailed,
    ProblemKind::ImagePull,
    ProblemKind::WorkloadShort,
    ProblemKind::PodPending,
];

/// Publishes a snapshot after each change, at most once per [`TelemetryTiming::evaluate_gap`].
pub(super) async fn run(
    cluster: Arc<str>,
    shared: Arc<Shared>,
    sender: watch::Sender<Arc<TelemetrySnapshot>>,
    timing: TelemetryTiming,
) {
    let mut summariser = ProblemSummariser::default();
    let mut tick = tokio::time::interval(timing.evaluate_every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shared.dirty.notified() => {}
            _ = tick.tick() => {}
        }
        let metrics = shared.metrics();
        let version = shared.version();
        let next = {
            let stores = shared.stores();
            evaluate(
                &cluster,
                &stores,
                &mut summariser,
                metrics,
                version,
                Timestamp::now(),
                shared.pending_after.load(Ordering::Relaxed),
            )
        };
        sender.send_if_modified(|current| {
            let changed = !current.same_as(&next);
            *current = Arc::new(next);
            changed
        });
        tokio::time::sleep(timing.evaluate_gap).await;
    }
}

/// One pass over the objects held.
fn evaluate(
    cluster: &Arc<str>,
    stores: &Stores,
    summariser: &mut ProblemSummariser,
    metrics: MetricsCheck,
    version: VersionCheck,
    now: Timestamp,
    pending_after: u32,
) -> TelemetrySnapshot {
    let classifier = Classifier::new(Arc::clone(cluster), now).with_pending_after(pending_after);
    let read = stores
        .replica_sets
        .current()
        .map_or(ReplicaSetRead::Failed, ReplicaSetRead::Complete);
    let replica_sets = ReplicaSetIndex::from_read(read);

    let mut problems: Vec<Problem> = Vec::new();
    if let Some(pods) = stores.pods.current() {
        problems = classifier.pods_with_replica_sets(pods, &replica_sets);
    }
    extend(&mut problems, stores.nodes.current(), |n| {
        classifier.node(n)
    });
    extend(&mut problems, stores.jobs.current(), |j| classifier.job(j));
    extend(&mut problems, stores.deployments.current(), |d| {
        classifier.deployment(d)
    });
    extend(&mut problems, stores.stateful_sets.current(), |s| {
        classifier.stateful_set(s)
    });
    extend(&mut problems, stores.daemon_sets.current(), |d| {
        classifier.daemon_set(d)
    });

    let kinds = stores.checks();
    let current = |kind: WatchedKind| kinds[kind as usize].1 == KindCheck::Current;
    let unchecked_categories = unchecked_categories(&current);
    let unchecked: Vec<ProblemKind> = ALL_PROBLEM_KINDS
        .into_iter()
        .filter(|kind| unchecked_categories.contains(&ProblemCategory::of_kind(*kind)))
        .collect();
    let counts = summariser.summarise(
        &ProblemSnapshot {
            problems: &problems,
            unchecked: &unchecked_categories,
            warning_events: 0,
            pending_claims: 0,
        },
        now,
    );
    let settling = problems
        .iter()
        .filter(|problem| summariser.is_settling(problem, now))
        .map(|problem| problem.id.clone())
        .collect();
    let mut checked = CheckedObjects::default();
    checked.record(WatchedKind::Pods, stores.pods.current());
    checked.record(WatchedKind::Nodes, stores.nodes.current());
    checked.record(WatchedKind::Jobs, stores.jobs.current());
    checked.record(WatchedKind::Deployments, stores.deployments.current());
    checked.record(WatchedKind::StatefulSets, stores.stateful_sets.current());
    checked.record(WatchedKind::DaemonSets, stores.daemon_sets.current());
    TelemetrySnapshot {
        cluster: Arc::clone(cluster),
        at: now,
        availability: availability(&kinds),
        kinds,
        problems: problems.into(),
        unchecked,
        settling,
        checked: Arc::new(checked),
        counts,
        metrics,
        capacity: capacity_of(stores),
        inventory: Inventory::of(stores),
        version,
    }
}

fn extend<K>(
    problems: &mut Vec<Problem>,
    items: Option<&[K]>,
    classify: impl Fn(&K) -> Option<Problem>,
) {
    problems.extend(items.into_iter().flatten().filter_map(classify));
}

/// The categories this pass cannot count. Warning events and pending claims are not watched. A
/// category with several kinds behind it is known only when all of them are. ReplicaSets decide
/// who owns a pod, not whether it fails, so they do not affect any count.
fn unchecked_categories(current: &dyn Fn(WatchedKind) -> bool) -> Vec<ProblemCategory> {
    let mut unchecked = vec![
        ProblemCategory::WarningEvents,
        ProblemCategory::PendingClaims,
    ];
    let read = [
        (ProblemCategory::FailingPods, current(WatchedKind::Pods)),
        (ProblemCategory::NotReadyNodes, current(WatchedKind::Nodes)),
        (ProblemCategory::FailedJobs, current(WatchedKind::Jobs)),
        (
            ProblemCategory::ShortWorkloads,
            current(WatchedKind::Deployments)
                && current(WatchedKind::StatefulSets)
                && current(WatchedKind::DaemonSets),
        ),
    ];
    unchecked.extend(read.into_iter().filter(|(_, ok)| !ok).map(|(c, _)| c));
    unchecked
}

fn availability(kinds: &[(WatchedKind, KindCheck)]) -> Availability {
    let is = |check: fn(&KindCheck) -> bool| kinds.iter().any(|(_, k)| check(k));
    if kinds.iter().all(|(_, k)| *k == KindCheck::Current) {
        Availability::Checked
    } else if is(|k| *k == KindCheck::Current) {
        Availability::Partial
    } else if is(|k| *k == KindCheck::Starting) {
        Availability::Starting
    } else {
        Availability::CannotBeChecked(first_failure(kinds))
    }
}

fn first_failure(kinds: &[(WatchedKind, KindCheck)]) -> Option<CheckFailure> {
    kinds.iter().find_map(|(_, check)| match check {
        KindCheck::Failed(failure) => Some(failure.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{Node, NodeCondition, NodeStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;
    use crate::cluster::SafeErrorText;
    use crate::telemetry::FailureCause;

    fn now() -> Timestamp {
        Timestamp::from_second(1_800_000_000).unwrap()
    }

    fn failure(cause: FailureCause) -> KindCheck {
        KindCheck::Failed(CheckFailure {
            cause,
            detail: SafeErrorText::reduced("refused"),
        })
    }

    fn all_current() -> Stores {
        let mut stores = Stores::default();
        stores.pods.check = KindCheck::Current;
        stores.nodes.check = KindCheck::Current;
        stores.jobs.check = KindCheck::Current;
        stores.deployments.check = KindCheck::Current;
        stores.stateful_sets.check = KindCheck::Current;
        stores.daemon_sets.check = KindCheck::Current;
        stores.replica_sets.check = KindCheck::Current;
        stores
    }

    fn pass(stores: &Stores) -> TelemetrySnapshot {
        evaluate(
            &Arc::from("one"),
            stores,
            &mut ProblemSummariser::default(),
            MetricsCheck::Waiting,
            VersionCheck::Waiting,
            now(),
            60,
        )
    }

    fn pending_pod(created: Timestamp) -> k8s_openapi::api::core::v1::Pod {
        serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": "stuck", "namespace": "ns", "uid": "stuck-uid",
                "creationTimestamp": created.to_string(),
            },
            "spec": {"containers": []},
            "status": {"phase": "Pending"},
        }))
        .unwrap()
    }

    fn shared_with(stores: Stores, pending_after: u32) -> Arc<Shared> {
        Arc::new(Shared {
            stores: std::sync::Mutex::new(stores),
            metrics: std::sync::Mutex::default(),
            version: std::sync::Mutex::default(),
            dirty: tokio::sync::Notify::new(),
            pending_after: std::sync::atomic::AtomicU32::new(pending_after),
        })
    }

    async fn wait_for_pending(latest: &mut watch::Receiver<Arc<TelemetrySnapshot>>, wanted: bool) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let found = latest
                    .borrow_and_update()
                    .problems
                    .iter()
                    .any(|p| p.kind() == ProblemKind::PodPending);
                if found == wanted {
                    return;
                }
                latest.changed().await.unwrap();
            }
        })
        .await
        .expect("the pod's state to be picked up");
    }

    #[tokio::test]
    async fn the_shared_timer_moves_a_pod_in_and_out_with_no_event() {
        let created = Timestamp::now() - jiff::SignedDuration::from_secs(1);
        let mut stores = all_current();
        stores.pods.replace(vec![(pending_pod(created), None)]);
        let shared = shared_with(stores, 2);
        let first = Arc::new(TelemetrySnapshot::starting("one".into(), Timestamp::now()));
        let (sender, mut latest) = watch::channel(first);
        let timing = TelemetryTiming {
            evaluate_every: std::time::Duration::from_millis(50),
            evaluate_gap: std::time::Duration::from_millis(1),
            ..TelemetryTiming::default()
        };
        let task = tokio::spawn(run("one".into(), Arc::clone(&shared), sender, timing));
        wait_for_pending(&mut latest, true).await;

        // Stops being Pending, and nothing says so but the next tick.
        let mut running = pending_pod(created);
        running.status.as_mut().unwrap().phase = Some("Running".to_owned());
        shared.stores().pods.replace(vec![(running, None)]);
        wait_for_pending(&mut latest, false).await;
        task.abort();
    }

    #[tokio::test]
    async fn a_changed_threshold_applies_to_a_running_feed() {
        let created = Timestamp::now() - jiff::SignedDuration::from_secs(30);
        let mut stores = all_current();
        stores.pods.replace(vec![(pending_pod(created), None)]);
        let shared = shared_with(stores, 60);
        let first = Arc::new(TelemetrySnapshot::starting("one".into(), Timestamp::now()));
        let (sender, mut latest) = watch::channel(first);
        // The timer is out of reach, so only the change can wake the pass.
        let timing = TelemetryTiming {
            evaluate_every: std::time::Duration::from_secs(3600),
            evaluate_gap: std::time::Duration::from_millis(1),
            ..TelemetryTiming::default()
        };
        let task = tokio::spawn(run("one".into(), Arc::clone(&shared), sender, timing));
        wait_for_pending(&mut latest, false).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        shared.pending_after.store(10, Ordering::Relaxed);
        shared.dirty.notify_one();
        wait_for_pending(&mut latest, true).await;

        shared.pending_after.store(120, Ordering::Relaxed);
        shared.dirty.notify_one();
        wait_for_pending(&mut latest, false).await;
        task.abort();
    }

    #[test]
    fn ten_thousand_pending_pods_are_judged_in_one_bounded_pass() {
        let created = now() - jiff::SignedDuration::from_secs(120);
        let pods = (0..10_000)
            .map(|i| {
                let mut pod = pending_pod(created);
                pod.metadata.name = Some(format!("pod-{i}"));
                pod.metadata.uid = Some(format!("uid-{i}"));
                (pod, None)
            })
            .collect();
        let mut stores = all_current();
        stores.pods.replace(pods);
        let started = std::time::Instant::now();
        let snapshot = pass(&stores);
        assert_eq!(snapshot.problems.len(), 10_000);
        assert!(snapshot.checked.contains(WatchedKind::Pods, "uid-9999"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    fn not_ready_node() -> Node {
        Node {
            metadata: ObjectMeta {
                name: Some("node-1".into()),
                uid: Some("node-1".into()),
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
        }
    }

    #[test]
    fn a_healthy_read_is_zero_but_what_is_not_watched_stays_unknown() {
        let snapshot = pass(&all_current());
        assert_eq!(snapshot.availability, Availability::Checked);
        assert!(snapshot.unchecked.is_empty());
        for category in [
            ProblemCategory::FailingPods,
            ProblemCategory::NotReadyNodes,
            ProblemCategory::FailedJobs,
            ProblemCategory::ShortWorkloads,
        ] {
            assert_eq!(snapshot.counts.get(category), Some(0), "{category:?}");
        }
        assert_eq!(snapshot.counts.get(ProblemCategory::WarningEvents), None);
        assert_eq!(snapshot.counts.get(ProblemCategory::PendingClaims), None);
    }

    #[test]
    fn a_kind_that_fails_is_unknown_and_never_zero() {
        let mut stores = all_current();
        stores.nodes.invalidate(failure(FailureCause::Forbidden));
        let snapshot = pass(&stores);
        assert_eq!(snapshot.availability, Availability::Partial);
        assert_eq!(snapshot.counts.get(ProblemCategory::NotReadyNodes), None);
        assert_eq!(snapshot.counts.get(ProblemCategory::FailingPods), Some(0));
        assert_eq!(snapshot.unchecked, [ProblemKind::NodeNotReady]);
    }

    #[test]
    fn one_workload_kind_failing_makes_the_workload_count_unknown() {
        let mut stores = all_current();
        stores
            .daemon_sets
            .invalidate(failure(FailureCause::Forbidden));
        let snapshot = pass(&stores);
        assert_eq!(snapshot.counts.get(ProblemCategory::ShortWorkloads), None);
        assert_eq!(snapshot.unchecked, [ProblemKind::WorkloadShort]);
    }

    #[test]
    fn replica_sets_failing_leaves_pod_counts_known() {
        let mut stores = all_current();
        stores
            .replica_sets
            .invalidate(failure(FailureCause::Failed));
        let snapshot = pass(&stores);
        assert_eq!(snapshot.availability, Availability::Partial);
        assert_eq!(snapshot.counts.get(ProblemCategory::FailingPods), Some(0));
        assert!(snapshot.unchecked.is_empty());
    }

    #[test]
    fn no_kind_readable_means_it_cannot_be_checked() {
        let mut stores = Stores::default();
        let login = failure(FailureCause::LoginFailed);
        stores.pods.invalidate(login.clone());
        stores.nodes.invalidate(login.clone());
        stores.jobs.invalidate(login.clone());
        stores.deployments.invalidate(login.clone());
        stores.stateful_sets.invalidate(login.clone());
        stores.daemon_sets.invalidate(login.clone());
        stores.replica_sets.invalidate(login);
        let snapshot = pass(&stores);
        let Availability::CannotBeChecked(Some(first)) = &snapshot.availability else {
            panic!("{:?}", snapshot.availability);
        };
        assert_eq!(first.cause, FailureCause::LoginFailed);
        assert!(matches!(
            snapshot.alert_snapshot(),
            crate::alerts::ClusterSnapshot::Unavailable
        ));
        for category in ProblemCategory::ALL {
            assert_eq!(snapshot.counts.get(category), None, "{category:?}");
        }
    }

    #[test]
    fn a_kind_still_starting_keeps_the_cluster_starting_until_something_is_read() {
        let mut stores = Stores::default();
        stores.nodes.invalidate(failure(FailureCause::Forbidden));
        assert_eq!(pass(&stores).availability, Availability::Starting);
    }

    #[test]
    fn a_held_problem_is_classified_and_counted() {
        let mut stores = all_current();
        stores.nodes.replace(vec![(not_ready_node(), ())]);
        let snapshot = pass(&stores);
        assert_eq!(snapshot.problems.len(), 1);
        assert_eq!(snapshot.problems[0].kind(), ProblemKind::NodeNotReady);
        assert_eq!(snapshot.counts.get(ProblemCategory::NotReadyNodes), Some(1));
    }
}
