//! Runs the node metrics poll into a [`TrendStore`] for as long as the feed lives.
//!
//! The poll runs in the feed's own task, so stopping the feed ends any request in flight. The
//! trend lock is shared with readers on other threads and is never waited on while blocking: a
//! held lock leaves this task cancellable, and no further request starts until it is released.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Duration;

use jiff::Timestamp;
use kube::Client;
use tokio::sync::watch;

use super::usage::{Bounded, read_nodes};
use super::{MetricsCheck, RequestGate, Shared, SideReads, UsageRead};
use crate::cluster::SafeErrorText;
use crate::metrics::{MetricsError, MetricsResult, NodeMetrics};
use crate::metrics_trend::TrendStore;

/// How soon a poll tries a held trend lock again.
const LOCK_RETRY: Duration = Duration::from_millis(5);

fn try_lock(trends: &Mutex<TrendStore>) -> Option<MutexGuard<'_, TrendStore>> {
    match trends.try_lock() {
        Ok(store) => Some(store),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Removes a cluster's series when the task holding it ends, however it ends. With the lock held
/// by a reader, that is left to a blocking thread rather than stalling this worker.
struct ForgetOnStop {
    cluster: Arc<str>,
    trends: Arc<Mutex<TrendStore>>,
}

impl Drop for ForgetOnStop {
    fn drop(&mut self) {
        if let Some(mut store) = try_lock(&self.trends) {
            store.drop_cluster(&self.cluster);
            return;
        }
        let (cluster, trends) = (Arc::clone(&self.cluster), Arc::clone(&self.trends));
        let forget = move || {
            trends
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drop_cluster(&cluster);
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn_blocking(forget));
        } else {
            forget();
        }
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
    cluster: Arc<str>,
    client: Client,
    requests: RequestGate,
    trends: Arc<Mutex<TrendStore>>,
    shared: Arc<Shared>,
    every: Duration,
    reads: watch::Sender<SideReads>,
) {
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
    let _forget = ForgetOnStop {
        cluster: Arc::clone(&cluster),
        trends: Arc::clone(&trends),
    };
    loop {
        let (result, usage) = match fetch().await {
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
    use crate::metrics::{MetricsList, Usage};
    use crate::metrics_trend::{TrendResource, TrendScope};

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
            dirty: Notify::new(),
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

    #[test]
    fn stopping_removes_only_that_clusters_series() {
        let at = Timestamp::from_second(1_800_000_000).unwrap();
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        try_lock(&trends).unwrap().record("one", at, &node_list());
        try_lock(&trends).unwrap().record("two", at, &node_list());
        drop(ForgetOnStop {
            cluster: "one".into(),
            trends: Arc::clone(&trends),
        });
        let store = try_lock(&trends).unwrap();
        let read = |name| {
            store
                .series(name, TrendScope::Total, TrendResource::Cpu, at)
                .is_some()
        };
        assert!(!read("one"));
        assert!(read("two"));
    }
}
