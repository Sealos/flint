//! Runs the node metrics poll into a [`TrendStore`] for as long as the feed lives.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use kube::Client;

use super::{MetricsCheck, Shared};
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

/// Polls until the task is stopped, which also ends the poll.
pub(super) async fn run(
    cluster: Arc<str>,
    client: Client,
    trends: Arc<Mutex<TrendStore>>,
    shared: Arc<Shared>,
    every: Duration,
) {
    let _forget = ForgetOnStop {
        cluster: Arc::clone(&cluster),
        trends: Arc::clone(&trends),
    };
    let mut poll = MetricsPoll::start(every, move || {
        let client = client.clone();
        async move { fetch_node_metrics(&client).await }
    });
    while poll.changed().await {
        let Some(result) = poll.latest() else {
            continue;
        };
        lock(&trends).record(&cluster, Timestamp::now(), &result);
        shared.set_metrics(check_of(&result));
    }
}

#[cfg(test)]
mod tests {
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
