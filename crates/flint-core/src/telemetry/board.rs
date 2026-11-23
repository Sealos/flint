//! The feeds running now, readable from the front end without going through the manager's loop.

// module-tests: ../manager.rs

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use super::{SideReads, Telemetry, TelemetrySnapshot};
use crate::cluster::ClusterId;
use crate::metrics_trend::TrendStore;

/// What is known about one cluster with a running feed.
#[derive(Clone, Debug)]
pub struct ClusterTelemetry {
    pub snapshot: Arc<TelemetrySnapshot>,
    /// Warning events, claims and usage, each with its own state.
    pub reads: SideReads,
    trends: Arc<Mutex<TrendStore>>,
}

impl ClusterTelemetry {
    /// Runs `read` on this feed's trends, whose series are under the cluster's name. Keep it
    /// short: the feed's metrics poll waits on the same lock.
    pub fn with_trends<R>(&self, read: impl FnOnce(&TrendStore) -> R) -> R {
        read(&self.trends.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Wakes when any cluster's telemetry changes or a feed starts or stops.
#[derive(Clone, Debug)]
pub struct TelemetryUpdates(watch::Receiver<u64>);

impl TelemetryUpdates {
    /// Waits for the next change. The first call resolves at once. `false` once the core stopped.
    pub async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }
}

#[derive(Debug)]
struct Entry {
    latest: watch::Receiver<Arc<TelemetrySnapshot>>,
    side: watch::Receiver<SideReads>,
    trends: Arc<Mutex<TrendStore>>,
}

impl Entry {
    /// `None` once the feed's tasks have ended, so a stopped feed is never read as live.
    fn read(&self) -> Option<ClusterTelemetry> {
        self.latest.has_changed().ok()?;
        Some(ClusterTelemetry {
            snapshot: self.latest.borrow().clone(),
            reads: self.side.borrow().clone(),
            trends: Arc::clone(&self.trends),
        })
    }
}

/// The running feeds by cluster. The manager writes it; clones read it.
#[derive(Clone, Debug)]
pub(crate) struct TelemetryBoard {
    feeds: Arc<Mutex<BTreeMap<ClusterId, Entry>>>,
    version: watch::Sender<u64>,
}

impl Default for TelemetryBoard {
    fn default() -> Self {
        Self {
            feeds: Arc::default(),
            version: watch::channel(0).0,
        }
    }
}

impl TelemetryBoard {
    fn feeds(&self) -> MutexGuard<'_, BTreeMap<ClusterId, Entry>> {
        self.feeds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lists `feed` as `cluster`'s, in place of any before it.
    pub(crate) fn publish(
        &self,
        cluster: &str,
        feed: &mut Telemetry,
        trends: Arc<Mutex<TrendStore>>,
    ) {
        feed.notify_changes(self.version.clone());
        let entry = Entry {
            latest: feed.latest.clone(),
            side: feed.side.clone(),
            trends,
        };
        self.feeds().insert(cluster.to_owned(), entry);
        self.bump();
    }

    /// Keeps only the clusters `keep` accepts.
    pub(crate) fn retain(&self, keep: impl Fn(&str) -> bool) {
        let removed = {
            let mut feeds = self.feeds();
            let before = feeds.len();
            feeds.retain(|cluster, _| keep(cluster));
            feeds.len() != before
        };
        if removed {
            self.bump();
        }
    }

    fn bump(&self) {
        self.version.send_modify(|version| *version += 1);
    }

    pub(crate) fn get(&self, cluster: &str) -> Option<ClusterTelemetry> {
        self.feeds().get(cluster).and_then(Entry::read)
    }

    pub(crate) fn all(&self) -> Vec<ClusterTelemetry> {
        self.feeds().values().filter_map(Entry::read).collect()
    }

    pub(crate) fn updates(&self) -> TelemetryUpdates {
        let mut updates = self.version.subscribe();
        updates.mark_changed();
        TelemetryUpdates(updates)
    }
}
