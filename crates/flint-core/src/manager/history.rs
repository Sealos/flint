//! Each cluster's CPU and memory history, owned by the manager rather than by a feed (ADR-0017).
//!
//! A feed writes its samples into the history of its cluster and definition, and never drops it:
//! the slot's feed and the kept feed hand the same history to each other, and a reconnect, a
//! lapsed sign-in or a fallback to the slot only leaves a gap. [`ClusterManager::sync_histories`]
//! drops a history once nothing watches its cluster any more or its definition changed, so a
//! different server or credential never inherits old samples.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::ClusterManager;
use crate::ClusterId;
use crate::keep_watching::KeepWatching;
use crate::metrics_trend::TrendStore;

/// One cluster's samples, shared by the feeds that write them and the readers that show them.
pub(crate) type SharedTrends = Arc<Mutex<TrendStore>>;

/// A cluster's history and the definition it was recorded on.
struct History {
    /// The context fingerprint ([`crate::cluster::context_fingerprint`]) its samples came from.
    fingerprint: u64,
    trends: SharedTrends,
}

/// The history of each cluster, for the life of the app.
#[derive(Default)]
pub(crate) struct TrendHistories {
    clusters: HashMap<ClusterId, History>,
}

impl TrendHistories {
    /// The history of `cluster` on definition `fingerprint`, for a feed to write into. One on
    /// another definition is replaced by an empty one. With no fingerprint nothing can show the
    /// definition is the same, so the store is the caller's alone, kept by no one, and the
    /// history kept leaves it alone.
    pub(crate) fn store(&mut self, cluster: &str, fingerprint: Option<u64>) -> SharedTrends {
        let Some(fingerprint) = fingerprint else {
            return SharedTrends::default();
        };
        match self.clusters.get(cluster) {
            Some(history) if history.fingerprint == fingerprint => Arc::clone(&history.trends),
            _ => {
                let trends = SharedTrends::default();
                let history = History {
                    fingerprint,
                    trends: Arc::clone(&trends),
                };
                self.clusters.insert(cluster.to_owned(), history);
                trends
            }
        }
    }

    /// The history kept for `cluster`, if any.
    #[cfg(test)]
    pub(crate) fn get(&self, cluster: &str) -> Option<&SharedTrends> {
        self.clusters.get(cluster).map(|history| &history.trends)
    }

    /// Drops `cluster`'s history. A feed still holding it writes on into a store nothing reads.
    pub(crate) fn forget(&mut self, cluster: &str) {
        self.clusters.remove(cluster);
    }
}

impl ClusterManager {
    /// Drops each history whose cluster is no longer watched or whose definition changed. Runs
    /// after the feeds were synced, so a hand-off within one message keeps its history.
    pub(super) fn sync_histories(&mut self) {
        let ended: Vec<ClusterId> = self
            .histories
            .clusters
            .iter()
            .filter(|(id, history)| !self.keeps_history(id, history.fingerprint))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ended {
            self.histories.forget(&id);
        }
    }

    /// Whether `id`'s history recorded on `fingerprint` stays: the context still has that
    /// definition, and the cluster is selected, in the running set, or kept watching, even while
    /// it waits for a sign-in or its files to settle. Turning keep watching off or ending its
    /// consent leaves a cluster in the background unwatched, which drops it.
    fn keeps_history(&self, id: &str, fingerprint: u64) -> bool {
        let watched = matches!(
            self.keep_status(id),
            KeepWatching::Watching | KeepWatching::SignInNeeded | KeepWatching::Paused
        );
        match self.cluster(id) {
            Some(slot) => {
                slot.fingerprint == Some(fingerprint) && (watched || self.wants_watches(id))
            }
            // Missing from a load that could not read every file: paused, not removed. A later
            // load on another definition drops it then.
            None => watched,
        }
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
