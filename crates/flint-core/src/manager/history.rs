//! Each cluster's CPU and memory history, owned by the manager rather than by a feed (ADR-0017).
//!
//! A feed writes its samples into the history of its cluster and definition, and never drops it:
//! the slot's feed and the kept feed hand the same history to each other, and a reconnect, a
//! lapsed sign-in or a fallback to the slot only leaves a gap. [`ClusterManager::sync_histories`]
//! drops a history once nothing watches its cluster any more or its definition changed, so a
//! different server or credential never inherits old samples.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use super::ClusterManager;
use crate::ClusterId;
use crate::keep_watching::KeepWatching;
use crate::metrics_trend::TrendStore;
use crate::thresholds::ThresholdMetric;

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

    /// Drops `cluster`'s history and empties it, so last values still listed for a stopped feed
    /// show none of its samples either. A feed still holding it writes on into a store nothing
    /// keeps.
    pub(crate) fn forget(&mut self, cluster: &str) {
        if let Some(history) = self.clusters.remove(cluster) {
            history
                .trends
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drop_cluster(cluster);
        }
    }
}

impl ClusterManager {
    /// The hours of history kept for `id`: its override, else the global value, at the server
    /// the context points to now.
    pub(super) fn history_hours(&self, id: &str) -> u32 {
        let server = self
            .cluster(id)
            .and_then(|slot| slot.info.server.as_deref());
        self.settings
            .thresholds_for(id, server)
            .get(ThresholdMetric::HistoryHours)
    }

    /// The history of `id` on definition `fingerprint` for a feed to write into, with the window
    /// the settings give it.
    pub(super) fn history_store(&mut self, id: &str, fingerprint: Option<u64>) -> SharedTrends {
        let trends = self.histories.store(id, fingerprint);
        let hours = self.history_hours(id);
        set_hours(&trends, hours);
        trends
    }

    /// Gives every kept history the window its cluster's settings say, so a change applies at
    /// once: a shorter one trims, a longer one keeps what was collected.
    pub(super) fn apply_history_windows(&self) {
        for (id, history) in &self.histories.clusters {
            set_hours(&history.trends, self.history_hours(id));
        }
    }

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
            // A paused context may show a fallback from another file, which is no confirmed
            // change: the history waits for the files to settle. No feed runs meanwhile.
            Some(slot) if slot.fingerprint != Some(fingerprint) => {
                (self.paused(id) || self.unsettled(id)) && (watched || self.wants_watches(id))
            }
            Some(_) => watched || self.wants_watches(id),
            // Missing from a load that could not read every file: paused, not removed. A later
            // load on another definition drops it then.
            None => watched,
        }
    }
}

pub(super) fn set_hours(trends: &SharedTrends, hours: u32) {
    trends
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .set_window_hours(hours);
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
