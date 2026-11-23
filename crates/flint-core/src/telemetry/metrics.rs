//! Runs the node metrics poll into a [`TrendStore`] for as long as the feed lives.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use kube::Client;
use tokio::sync::{mpsc, watch};

use super::{MetricsCheck, Shared, SideReads};
use crate::metrics::{MetricsPoll, MetricsResult, NodeMetrics, fetch_node_metrics};
use crate::metrics_trend::TrendStore;

fn lock(trends: &Mutex<TrendStore>) -> MutexGuard<'_, TrendStore> {
    trends.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes a cluster's series when the task holding it ends, however it ends.
struct ForgetOnStop {
    cluster: Arc<str>,
    trends: Arc<Mutex<TrendStore>>,
}

impl Drop for ForgetOnStop {
    fn drop(&mut self) {
        lock(&self.trends).drop_cluster(&self.cluster);
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

/// A poll whose task and fetch in flight each hold `running`, so that the channel's receiver sees
/// it close only once the poll has really ended. Dropping a [`MetricsPoll`] only asks it to.
fn poll_holding<R, F, Fut>(
    every: Duration,
    running: mpsc::Sender<()>,
    mut fetch: F,
) -> MetricsPoll<R>
where
    R: Send + Sync + 'static,
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
{
    MetricsPoll::start(every, move || {
        let held = running.clone();
        let fetching = fetch();
        async move {
            let _held = held;
            fetching.await
        }
    })
}

/// Polls until the task is stopped, which also ends the poll.
pub(super) async fn run(
    cluster: Arc<str>,
    client: Client,
    trends: Arc<Mutex<TrendStore>>,
    shared: Arc<Shared>,
    every: Duration,
    running: mpsc::Sender<()>,
    reads: watch::Sender<SideReads>,
) {
    let _forget = ForgetOnStop {
        cluster: Arc::clone(&cluster),
        trends: Arc::clone(&trends),
    };
    let mut poll = poll_holding(every, running, move || {
        let client = client.clone();
        async move { fetch_node_metrics(&client).await }
    });
    while poll.changed().await {
        let Some(result) = poll.latest() else {
            continue;
        };
        lock(&trends).record(&cluster, Timestamp::now(), &result);
        let usage = SideReads::node_usage_of(&result);
        reads.send_if_modified(|current| {
            let changed = current.node_usage != usage;
            current.node_usage = usage;
            changed
        });
        shared.set_metrics(check_of(&result));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::metrics::{MetricsError, MetricsList, Usage};
    use crate::metrics_trend::{TrendResource, TrendScope};

    #[test]
    fn results_map_to_their_check() {
        let list = MetricsList {
            items: vec![NodeMetrics {
                name: "a".into(),
                timestamp: None,
                window: None,
                usage: Usage::default(),
            }],
            skipped: 0,
        };
        assert_eq!(check_of(&Ok(list)), MetricsCheck::Current { nodes: 1 });
        assert_eq!(
            check_of(&Err(MetricsError::NotInstalled)),
            MetricsCheck::Unavailable(MetricsError::NotInstalled)
        );
    }

    #[tokio::test]
    async fn the_channel_closes_only_after_a_fetch_in_flight_has_ended() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let (started, mut fetching) = mpsc::channel::<()>(1);
        let (running, mut ended) = mpsc::channel::<()>(1);
        let flag = Arc::clone(&dropped);
        let poll = poll_holding(Duration::from_secs(60), running, move || {
            let (flag, started) = (Flag(Arc::clone(&flag)), started.clone());
            async move {
                let _flag = flag;
                let _ = started.send(()).await;
                std::future::pending::<()>().await;
            }
        });
        fetching.recv().await.unwrap();
        assert!(!dropped.load(Ordering::SeqCst));

        drop(poll);
        assert!(ended.recv().await.is_none());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn stopping_removes_only_that_clusters_series() {
        let at = Timestamp::from_second(1_800_000_000).unwrap();
        let list = || {
            Ok(MetricsList {
                items: vec![NodeMetrics {
                    name: "a".into(),
                    timestamp: None,
                    window: None,
                    usage: Usage::default(),
                }],
                skipped: 0,
            })
        };
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        lock(&trends).record("one", at, &list());
        lock(&trends).record("two", at, &list());
        drop(ForgetOnStop {
            cluster: "one".into(),
            trends: Arc::clone(&trends),
        });
        let store = lock(&trends);
        let read = |name| {
            store
                .series(name, TrendScope::Total, TrendResource::Cpu, at)
                .is_some()
        };
        assert!(!read("one"));
        assert!(read("two"));
    }
}
