//! The telemetry feed against the fake API server: problems with no view open, live changes,
//! relists, real failures reported as "cannot be checked", metrics into trends, and a clean stop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use flint_core::dashboard::ProblemCategory;
use flint_core::metrics::MetricsError;
use flint_core::metrics_trend::{TrendResource, TrendScope, TrendStore};
use flint_core::telemetry::{
    Availability, FailureCause, KindCheck, MetricsCheck, Telemetry, TelemetrySnapshot,
    TelemetryTiming, WatchedKind, start_for_tests,
};
use flint_core::{ProblemKind, ProblemSubject};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, FakePod, Fault, Gvr, Options, Verb, WatchStep};

const WAIT: Duration = Duration::from_secs(30);

fn timing() -> TelemetryTiming {
    TelemetryTiming {
        evaluate_every: Duration::from_millis(100),
        evaluate_gap: Duration::from_millis(10),
        metrics_every: Duration::from_millis(100),
        backoff_min: Duration::from_millis(20),
        backoff_max: Duration::from_millis(200),
        ..TelemetryTiming::default()
    }
}

fn trends() -> Arc<Mutex<TrendStore>> {
    Arc::new(Mutex::new(TrendStore::new()))
}

async fn start(server: &FakeApiServer, trends: &Arc<Mutex<TrendStore>>) -> Telemetry {
    start_for_tests(
        &server.kubeconfig(None),
        CONTEXT,
        Arc::clone(trends),
        timing(),
    )
    .await
    .unwrap()
}

/// Waits for a snapshot `check` accepts.
async fn until(
    feed: &mut Telemetry,
    what: &str,
    check: impl Fn(&TelemetrySnapshot) -> bool,
) -> Arc<TelemetrySnapshot> {
    let waited = tokio::time::timeout(WAIT, async {
        loop {
            let latest = feed.latest();
            if check(&latest) {
                return latest;
            }
            assert!(feed.changed().await, "the feed ended");
        }
    })
    .await;
    waited.unwrap_or_else(|_| panic!("timed out on {what}: {:#?}", feed.latest()))
}

fn crash_looping(name: &str) -> FakePod {
    FakePod::new("demo", name, &["app"])
        .restarted("app")
        .restarted("app")
        .restarted("app")
        .restarted("app")
        .waiting("app", "CrashLoopBackOff")
}

fn reports_pod(snapshot: &TelemetrySnapshot, name: &str) -> bool {
    snapshot.problems.iter().any(|problem| {
        problem.kind() == ProblemKind::CrashLoop
            && matches!(&problem.id.subject, ProblemSubject::Container { pod, .. } if pod == name)
    })
}

fn checked(snapshot: &TelemetrySnapshot) -> bool {
    snapshot.availability == Availability::Checked
}

fn watched_gvrs() -> [Gvr; 7] {
    [
        Gvr::core("pods"),
        Gvr::core("nodes"),
        Gvr::new("batch", "v1", "jobs"),
        Gvr::new("apps", "v1", "deployments"),
        Gvr::new("apps", "v1", "statefulsets"),
        Gvr::new("apps", "v1", "daemonsets"),
        Gvr::new("apps", "v1", "replicasets"),
    ]
}

#[tokio::test]
async fn problems_follow_the_cluster_with_no_view_open() {
    let server = FakeApiServer::start().await.unwrap();
    server.add_pod(crash_looping("listed"));
    let store = trends();
    let mut feed = start(&server, &store).await;

    let first = until(&mut feed, "the first read", |s| {
        checked(s) && reports_pod(s, "listed")
    })
    .await;
    let failing = first.counts.get(ProblemCategory::FailingPods).unwrap();
    assert!(failing >= 1);
    assert_eq!(first.counts.get(ProblemCategory::WarningEvents), None);
    assert_eq!(first.counts.get(ProblemCategory::PendingClaims), None);

    server.add_pod(crash_looping("arrived"));
    until(&mut feed, "a pod that arrives", |s| {
        reports_pod(s, "arrived")
    })
    .await;

    server.delete_pod("demo", "arrived");
    until(&mut feed, "a pod that goes", |s| !reports_pod(s, "arrived")).await;

    feed.stop().await;
    assert!(server.writes().is_empty());
}

#[tokio::test]
async fn it_reads_only_the_watched_kinds_and_metrics() {
    let server = FakeApiServer::start().await.unwrap();
    let store = trends();
    let mut feed = start(&server, &store).await;
    until(&mut feed, "the first read", |s| {
        checked(s) && matches!(s.metrics, MetricsCheck::Current { .. })
    })
    .await;
    feed.stop().await;

    let allowed: Vec<String> = watched_gvrs()
        .iter()
        .map(|gvr| gvr.collection_path(None))
        .chain([
            "/version".to_owned(),
            "/apis/metrics.k8s.io/v1beta1/nodes".to_owned(),
        ])
        .collect();
    for request in server.requests() {
        assert_eq!(request.method, "GET", "{}", request.path);
        assert!(
            allowed.contains(&request.path),
            "unexpected read of {}",
            request.path
        );
    }
}

#[tokio::test]
async fn a_relist_after_an_expired_watch_catches_up() {
    let server = FakeApiServer::start().await.unwrap();
    let store = trends();
    let mut feed = start(&server, &store).await;
    until(&mut feed, "the first read", checked).await;

    let pods = Gvr::core("pods");
    server.wait_for_watches(&pods, 1, WAIT).await.unwrap();
    server.send_watch(&pods, WatchStep::Expired);
    server.add_pod(crash_looping("during-relist"));
    until(&mut feed, "the relist", |s| {
        checked(s) && reports_pod(s, "during-relist")
    })
    .await;
    feed.stop().await;
}

#[tokio::test]
async fn an_expired_login_is_reported_and_recovers() {
    let server = FakeApiServer::start().await.unwrap();
    let store = trends();
    let mut feed = start(&server, &store).await;
    until(&mut feed, "the first read", checked).await;

    server.inject(Fault::status(401));
    for gvr in watched_gvrs() {
        server.send_watch(&gvr, WatchStep::Drop);
    }
    let lost = until(&mut feed, "the failure", |s| {
        matches!(s.availability, Availability::CannotBeChecked(_))
    })
    .await;
    let Availability::CannotBeChecked(Some(failure)) = &lost.availability else {
        panic!("{:?}", lost.availability);
    };
    assert_eq!(failure.cause, FailureCause::LoginFailed);
    for category in ProblemCategory::ALL {
        assert_eq!(lost.counts.get(category), None, "{category:?}");
    }
    assert!(lost.problems.is_empty());

    server.clear_faults();
    until(&mut feed, "the recovery", checked).await;
    feed.stop().await;
}

#[tokio::test]
async fn a_kind_it_may_not_read_is_unknown_while_others_are_counted() {
    let server = FakeApiServer::start().await.unwrap();
    let nodes = Gvr::core("nodes");
    server.inject(Fault::forbidden().on(&nodes).verb(Verb::List));
    let store = trends();
    let mut feed = start(&server, &store).await;

    let partial = until(&mut feed, "the partial read", |s| {
        s.availability == Availability::Partial
            && matches!(s.kind(WatchedKind::Nodes), KindCheck::Failed(_))
            && s.kinds
                .iter()
                .filter(|(_, check)| *check == KindCheck::Current)
                .count()
                == WatchedKind::ALL.len() - 1
    })
    .await;
    let KindCheck::Failed(failure) = partial.kind(WatchedKind::Nodes) else {
        unreachable!();
    };
    assert_eq!(failure.cause, FailureCause::Forbidden);
    assert_eq!(partial.counts.get(ProblemCategory::NotReadyNodes), None);
    assert!(partial.counts.get(ProblemCategory::FailingPods).is_some());
    assert!(partial.unchecked.contains(&ProblemKind::NodeNotReady));
    feed.stop().await;
}

#[tokio::test]
async fn metrics_reach_the_trend_store_and_stopping_forgets_them() {
    let server = FakeApiServer::start().await.unwrap();
    let store = trends();
    let mut feed = start(&server, &store).await;
    let polled = until(
        &mut feed,
        "a metrics poll",
        |s| matches!(s.metrics, MetricsCheck::Current { nodes } if nodes > 0),
    )
    .await;
    let has_series = |store: &Arc<Mutex<TrendStore>>| {
        store
            .lock()
            .unwrap()
            .series(CONTEXT, TrendScope::Total, TrendResource::Cpu, polled.at)
            .is_some()
    };
    assert!(has_series(&store));

    feed.stop().await;
    assert!(!has_series(&store));
}

#[tokio::test]
async fn a_cluster_without_metrics_server_is_still_checked() {
    let server = FakeApiServer::start_with(Options {
        no_metrics: true,
        ..Options::default()
    })
    .await
    .unwrap();
    let store = trends();
    let mut feed = start(&server, &store).await;
    let snapshot = until(&mut feed, "the metrics answer", |s| {
        checked(s) && s.metrics != MetricsCheck::Waiting
    })
    .await;
    assert_eq!(
        snapshot.metrics,
        MetricsCheck::Unavailable(MetricsError::NotInstalled)
    );
    feed.stop().await;
}

#[tokio::test]
async fn stopping_closes_every_watch_and_sends_nothing_more() {
    let server = FakeApiServer::start().await.unwrap();
    let store = trends();
    let mut feed = start(&server, &store).await;
    until(&mut feed, "the first read", checked).await;
    for gvr in watched_gvrs() {
        server.wait_for_watches(&gvr, 1, WAIT).await.unwrap();
    }

    feed.stop().await;
    tokio::time::timeout(WAIT, async {
        while watched_gvrs().iter().any(|g| server.open_watches(g) > 0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("every watch closes");
    let after = server.requests().len();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(server.requests().len(), after);
}
