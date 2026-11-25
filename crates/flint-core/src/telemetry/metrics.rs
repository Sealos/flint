//! Runs the node metrics poll into a [`TrendStore`] for as long as the feed lives.
//!
//! The store is the cluster's history, owned by the manager: stopping the feed leaves its samples
//! for the feed that follows, and time without a poll reads as a gap.
//!
//! The poll runs in the feed's own task, so stopping the feed ends any request in flight. The
//! trend lock is shared with readers on other threads and is never waited on while blocking: a
//! held lock leaves this task cancellable, and no further request starts until it is released.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use jiff::Timestamp;
use k8s_openapi::api::core::v1::Node;
use kube::Client;
use tokio::sync::watch;

use super::usage::{Bounded, read_nodes};
use super::{
    KindCheck, MetricsCheck, RequestGate, Shared, SideReads, TelemetrySnapshot, UsageRead,
    WatchedKind,
};
use crate::cluster::SafeErrorText;
use crate::metrics::{MetricsError, MetricsList, MetricsResult, NodeMetrics};
use crate::metrics_trend::TrendStore;

/// The longest the first poll waits for the initial node check.
const NODES_WAIT: Duration = Duration::from_secs(5);

/// How soon a poll tries a held trend lock again.
const LOCK_RETRY: Duration = Duration::from_millis(5);

fn try_lock(trends: &Mutex<TrendStore>) -> Option<MutexGuard<'_, TrendStore>> {
    match trends.try_lock() {
        Ok(store) => Some(store),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// A list in which no node could be read says nothing about the cluster's nodes. A list that is
/// really empty stays a result.
fn readable(result: MetricsResult<NodeMetrics>) -> MetricsResult<NodeMetrics> {
    match result {
        Ok(list) if list.items.is_empty() && list.skipped > 0 => Err(MetricsError::Failed(
            SafeErrorText::reduced("no node's metrics could be read"),
        )),
        other => other,
    }
}

fn check_of(result: &MetricsResult<NodeMetrics>) -> MetricsCheck {
    match result {
        Ok(list) => MetricsCheck::Current {
            nodes: list.items.len(),
        },
        Err(err) => MetricsCheck::Unavailable(err.clone()),
    }
}

/// How many nodes `list` says nothing about, of those the feed watches. Not knowing the nodes
/// counts as one, since the list cannot then be shown to cover the cluster.
fn unreported_nodes(list: &MetricsList<NodeMetrics>, watched: Option<&[Node]>) -> usize {
    let Some(watched) = watched else { return 1 };
    let reported: HashSet<&str> = list.items.iter().map(|node| node.name.as_str()).collect();
    watched
        .iter()
        .filter_map(|node| node.metadata.name.as_deref())
        .filter(|name| !reported.contains(name))
        .count()
}

/// Marks a list that misses a watched node as skipping it, so its sum is not recorded as the
/// cluster's total. The nodes it does report keep their own series.
fn note_coverage(result: &mut MetricsResult<NodeMetrics>, shared: &Shared) {
    if let Ok(list) = result {
        let missing = unreported_nodes(list, shared.stores().nodes.current());
        list.skipped = list.skipped.saturating_add(missing);
    }
}

/// Records `result`, waiting for the lock in a way the task can be stopped during.
async fn record(trends: &Mutex<TrendStore>, cluster: &str, result: &MetricsResult<NodeMetrics>) {
    loop {
        if let Some(mut store) = try_lock(trends) {
            store.record(cluster, Timestamp::now(), result);
            return;
        }
        tokio::time::sleep(LOCK_RETRY).await;
    }
}

/// Polls until the task is stopped.
pub(super) async fn run(
    mut latest: watch::Receiver<Arc<TelemetrySnapshot>>,
    client: Client,
    requests: RequestGate,
    trends: Arc<Mutex<TrendStore>>,
    shared: Arc<Shared>,
    every: Duration,
    reads: watch::Sender<SideReads>,
) {
    // A total needs the node inventory. Polling before the initial check can leave the
    // dashboard without a total for a full interval even after the watch has opened, so the
    // first poll waits for it, but never longer than `NODES_WAIT`.
    let cluster = {
        let ended = {
            let waited = tokio::time::timeout(
                NODES_WAIT,
                latest
                    .wait_for(|snapshot| snapshot.kind(WatchedKind::Nodes) != &KindCheck::Starting),
            )
            .await;
            match waited {
                Ok(Ok(snapshot)) => Some(Arc::clone(&snapshot.cluster)),
                Ok(Err(_)) => return,
                Err(_) => None,
            }
        };
        ended.unwrap_or_else(|| Arc::clone(&latest.borrow().cluster))
    };
    drop(latest);
    let fetch = || read_nodes(&client, &requests);
    poll(cluster, fetch, trends, shared, every, reads).await;
}

async fn poll<F, Fut>(
    cluster: Arc<str>,
    mut fetch: F,
    trends: Arc<Mutex<TrendStore>>,
    shared: Arc<Shared>,
    every: Duration,
    reads: watch::Sender<SideReads>,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Bounded<NodeMetrics>>,
{
    loop {
        let (mut result, usage) = match fetch().await {
            Bounded::Listed(result) => {
                let result = readable(result);
                let usage = SideReads::node_usage_of(&result);
                (result, usage)
            }
            Bounded::TooLarge => (
                Err(MetricsError::Failed(SafeErrorText::reduced(
                    "the cluster has too many nodes to read",
                ))),
                UsageRead::TooLarge,
            ),
        };
        note_coverage(&mut result, &shared);
        record(&trends, &cluster, &result).await;
        // A sample extends the trends even when usage is unchanged, so listeners are told.
        let sampled = result.as_ref().is_ok_and(|list| !list.items.is_empty());
        reads.send_if_modified(|current| {
            let changed = current.node_usage != usage;
            current.node_usage = usage;
            changed || sampled
        });
        shared.set_metrics(check_of(&result));
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;
    use crate::metrics::Usage;
    use crate::metrics_trend::{TrendResource, TrendScope};
    use crate::telemetry::{Telemetry, TelemetryTiming};

    #[tokio::test(start_paused = true)]
    #[allow(clippy::disallowed_methods)] // No cluster or credentials: an in-memory transport.
    async fn the_first_poll_records_a_total_when_the_node_watch_takes_longer_to_open() {
        let service = tower::service_fn(|request: http::Request<kube::client::Body>| async move {
            let (status, body) = match request.uri().path() {
                "/api/v1/nodes"
                    if request
                        .uri()
                        .query()
                        .is_some_and(|q| q.contains("watch=true")) =>
                {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    (200, "")
                }
                "/api/v1/nodes" => (
                    200,
                    r#"{"metadata":{"resourceVersion":"1"},
                    "items":[{"metadata":{"name":"a"}}]}"#,
                ),
                "/apis/metrics.k8s.io/v1beta1/nodes" => (
                    200,
                    r#"{"items":[
                    {"metadata":{"name":"a"},"usage":{"cpu":"250m","memory":"64Mi"}}
                ]}"#,
                ),
                _ => (404, r#"{"kind":"Status","status":"Failure","code":404}"#),
            };
            Ok::<_, std::convert::Infallible>(
                http::Response::builder()
                    .status(status)
                    .body(kube::client::Body::from(body.as_bytes().to_vec()))
                    .unwrap(),
            )
        });
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        let mut feed = Telemetry::start(
            "member".into(),
            Client::new(service, "default"),
            crate::cluster::Target::of(&Default::default(), "member"),
            Arc::clone(&trends),
            TelemetryTiming::default(),
            RequestGate::default(),
            60,
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = feed.latest();
                if snapshot.kind(WatchedKind::Nodes) == &KindCheck::Current
                    && snapshot.metrics == (MetricsCheck::Current { nodes: 1 })
                {
                    break;
                }
                assert!(feed.changed().await);
            }
        })
        .await
        .expect("the node watch and first metrics poll answer within one poll interval");
        {
            let store = trends.lock().unwrap();
            for (resource, expected) in [
                (TrendResource::Cpu, 250_000_000),
                (TrendResource::Memory, 64 * 1024 * 1024),
            ] {
                let total = store
                    .series("member", TrendScope::Total, resource, Timestamp::now())
                    .and_then(Iterator::last)
                    .flatten();
                assert_eq!(total, Some(expected), "{resource:?}");
            }
        }
        feed.stop().await;
    }

    #[tokio::test(start_paused = true)]
    #[allow(clippy::disallowed_methods)] // No cluster or credentials: an in-memory transport.
    async fn metrics_are_polled_when_the_node_check_never_finishes() {
        let service = tower::service_fn(|request: http::Request<kube::client::Body>| async move {
            let (status, body) = match request.uri().path() {
                "/api/v1/nodes"
                    if request
                        .uri()
                        .query()
                        .is_some_and(|q| q.contains("watch=true")) =>
                {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    (200, "")
                }
                "/api/v1/nodes" => std::future::pending().await,
                "/apis/metrics.k8s.io/v1beta1/nodes" => (
                    200,
                    r#"{"items":[
                    {"metadata":{"name":"a"},"usage":{"cpu":"250m","memory":"64Mi"}}
                ]}"#,
                ),
                _ => (404, r#"{"kind":"Status","status":"Failure","code":404}"#),
            };
            Ok::<_, std::convert::Infallible>(
                http::Response::builder()
                    .status(status)
                    .body(kube::client::Body::from(body.as_bytes().to_vec()))
                    .unwrap(),
            )
        });
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        let mut feed = Telemetry::start(
            "member".into(),
            Client::new(service, "default"),
            crate::cluster::Target::of(&Default::default(), "member"),
            Arc::clone(&trends),
            TelemetryTiming::default(),
            RequestGate::default(),
            60,
        );
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = feed.latest();
                if snapshot.metrics == (MetricsCheck::Current { nodes: 1 }) {
                    break;
                }
                assert!(feed.changed().await);
            }
        })
        .await
        .expect("the first metrics poll starts without the node check");
        assert_eq!(feed.latest().kind(WatchedKind::Nodes), &KindCheck::Starting);
        feed.stop().await;
    }

    fn node_list() -> MetricsResult<NodeMetrics> {
        Ok(MetricsList {
            items: vec![NodeMetrics {
                name: "a".into(),
                timestamp: None,
                window: None,
                usage: Usage::default(),
            }],
            skipped: 0,
        })
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            stores: Mutex::default(),
            metrics: Mutex::default(),
            version: Mutex::default(),
            dirty: Notify::new(),
            pending_after: crate::thresholds::ThresholdMetric::PendingPodSeconds
                .default_value()
                .into(),
        })
    }

    #[test]
    fn results_map_to_their_check() {
        assert_eq!(check_of(&node_list()), MetricsCheck::Current { nodes: 1 });
        assert_eq!(
            check_of(&Err(MetricsError::NotInstalled)),
            MetricsCheck::Unavailable(MetricsError::NotInstalled)
        );
    }

    #[test]
    fn a_list_of_only_unreadable_nodes_is_unavailable_but_an_empty_one_is_not() {
        let unreadable = readable(Ok(MetricsList {
            items: Vec::new(),
            skipped: 2,
        }));
        assert!(matches!(
            check_of(&unreadable),
            MetricsCheck::Unavailable(MetricsError::Failed(_))
        ));
        let empty = readable(Ok(MetricsList {
            items: Vec::new(),
            skipped: 0,
        }));
        assert_eq!(check_of(&empty), MetricsCheck::Current { nodes: 0 });
    }

    #[tokio::test(start_paused = true)]
    async fn steady_usage_still_wakes_listeners_for_each_sample() {
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        let (reads, mut seen) = watch::channel(SideReads::default());
        let every = Duration::from_secs(15);
        let task = tokio::spawn(poll(
            "one".into(),
            || async { Bounded::Listed(node_list()) },
            trends,
            shared(),
            every,
            reads,
        ));
        seen.changed().await.unwrap();
        seen.changed().await.unwrap();
        task.abort();
    }

    #[expect(
        clippy::await_holding_lock,
        reason = "the lock is held on purpose, and the poll only ever tries it"
    )]
    #[tokio::test(start_paused = true)]
    async fn stopping_while_the_lock_is_held_starts_no_further_fetch() {
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        let fetches = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&fetches);
        let held = trends.lock().unwrap();
        let (reads, _seen) = watch::channel(SideReads::default());
        let task = tokio::spawn(poll(
            "one".into(),
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                async { Bounded::Listed(node_list()) }
            },
            Arc::clone(&trends),
            shared(),
            Duration::from_millis(1),
            reads,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "waits on the lock");

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(held);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_poll_missing_a_watched_node_records_no_cluster_total() {
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        let shared = shared();
        {
            let mut stores = shared.stores();
            let node = |name: &str| Node {
                metadata: kube::api::ObjectMeta {
                    name: Some(name.to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            };
            stores.nodes.replace(vec![(node("a"), ()), (node("b"), ())]);
            stores.nodes.check = super::super::KindCheck::Current;
        }
        let (reads, mut seen) = watch::channel(SideReads::default());
        let task = tokio::spawn(poll(
            "one".into(),
            || async { Bounded::Listed(node_list()) },
            Arc::clone(&trends),
            shared,
            Duration::from_secs(15),
            reads,
        ));
        seen.changed().await.unwrap();
        let total: Vec<_> = try_lock(&trends)
            .unwrap()
            .series(
                "one",
                TrendScope::Total,
                TrendResource::Cpu,
                Timestamp::now(),
            )
            .unwrap()
            .flatten()
            .collect();
        task.abort();
        assert!(total.is_empty(), "a partial poll is not a total: {total:?}");
    }
}
