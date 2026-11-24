//! Disconnecting a cluster: everything Flint runs for it stops, and nothing connects it again
//! until the person selects it or opens a set with it.
//!
//! The mark is asked where a connection starts ([`ClusterManager::ensure_connected`]), so no retry,
//! reload or log reconnect has to know about it. Shells belong to their tabs: the front end ends
//! them, after asking.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::ClusterManager;
use crate::ClusterId;
use crate::handle::{ConnectionState, Event};
use crate::logs::LogError;

/// What a log tab says once its cluster was disconnected.
pub(super) const LOGS_ENDED: &str = "disconnected from this cluster; select it to connect again";

/// How many times each cluster was disconnected, counted when the front end asks, before the
/// core takes the command. A request stamped with an older count was made before a disconnect
/// the core may not have run yet, so it starts nothing.
#[derive(Debug, Clone, Default)]
pub(crate) struct DisconnectCount(Arc<Mutex<HashMap<ClusterId, Counted>>>);

/// Disconnects asked for, and how many of them the core has run.
#[derive(Debug, Clone, Copy, Default)]
struct Counted {
    asked: u64,
    run: u64,
}

impl DisconnectCount {
    pub(crate) fn of(&self, cluster: &str) -> u64 {
        self.counts().get(cluster).map_or(0, |c| c.asked)
    }

    pub(crate) fn bump(&self, cluster: &str) {
        self.counts().entry(cluster.to_owned()).or_default().asked += 1;
    }

    /// The core took one disconnect of `cluster`.
    fn ran(&self, cluster: &str) {
        let mut counts = self.counts();
        let counted = counts.entry(cluster.to_owned()).or_default();
        counted.run += 1;
        // A command sent another way than the handle counts as asked too.
        counted.asked = counted.asked.max(counted.run);
    }

    /// Whether a write stamped with `asked` must stop: `cluster` was disconnected since, or a
    /// disconnect is asked for that the core has not run yet.
    pub(crate) fn ends_write(&self, cluster: &str, asked: u64) -> bool {
        let counted = self.counts().get(cluster).copied().unwrap_or_default();
        counted.asked != asked || counted.run < counted.asked
    }

    fn counts(&self) -> std::sync::MutexGuard<'_, HashMap<ClusterId, Counted>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The clusters disconnected, and how to end each log session.
#[derive(Default)]
pub(super) struct Disconnects {
    clusters: HashSet<ClusterId>,
    log_stops: HashMap<u64, (ClusterId, watch::Sender<bool>)>,
    pub(super) count: DisconnectCount,
}

impl Disconnects {
    pub(super) fn holds(&self, id: &str) -> bool {
        self.clusters.contains(id)
    }

    /// The person chose `id` again: it may connect.
    pub(super) fn lift(&mut self, id: &str) {
        self.clusters.remove(id);
    }

    /// The signal that ends log session `session` of `cluster`, asked for when `cluster` had
    /// been disconnected `asked` times: already given if it was disconnected since.
    pub(super) fn track_logs(
        &mut self,
        session: u64,
        cluster: &str,
        asked: u64,
        live: impl Fn(&u64) -> bool,
    ) -> watch::Receiver<bool> {
        self.log_stops.retain(|id, _| live(id));
        let (stop, stopped) = watch::channel(self.count.of(cluster) != asked);
        self.log_stops.insert(session, (cluster.to_owned(), stop));
        stopped
    }

    pub(super) fn forget_logs(&mut self, session: u64) {
        self.log_stops.remove(&session);
    }

    fn end_logs(&mut self, cluster: &str) {
        self.log_stops.retain(|_, (of, stop)| {
            let ends = of == cluster;
            if ends {
                stop.send_replace(true);
            }
            !ends
        });
    }
}

impl ClusterManager {
    pub(crate) fn disconnect_count(&self) -> DisconnectCount {
        self.disconnects.count.clone()
    }

    /// Why a new log read of `id` is refused, if it was disconnected.
    pub(super) fn disconnected_logs(&self, id: &str) -> Option<LogError> {
        self.disconnects
            .holds(id)
            .then(|| LogError::cluster(LOGS_ENDED))
    }

    /// Stops `id`'s watches, feeds, retries, log streams and clients, and ends its keep-watching
    /// consent. The settings are written last, so a slow or failed save holds up none of this.
    pub(super) async fn disconnect_cluster(&mut self, id: ClusterId) {
        self.disconnects.count.ran(&id);
        if self.cluster(&id).is_none() {
            self.emit(Event::Error(format!("cluster \"{id}\" no longer exists")));
            return;
        }
        let clusters = &self.clusters;
        self.disconnects
            .clusters
            .retain(|name| clusters.iter().any(|slot| slot.info.name == *name));
        self.disconnects.clusters.insert(id.clone());
        // First: a write reviewed or confirmed before now is refused from here on, after a
        // reconnect too, and the front end's dialogs on this connection close.
        self.contexts.end_writes(&id);
        let write_epoch = self.contexts.shown(&id, None).gate;
        self.emit(Event::WritesEnded {
            cluster: id.clone(),
            write_epoch,
        });
        self.keep.grants_revoke(&id);
        self.disconnects.end_logs(&id);
        if self.active.as_deref() == Some(id.as_str()) {
            self.active = None;
            self.end_selected_retry();
            self.emit(Event::ActiveChanged(None));
        }
        if self.members.contains_key(&id) {
            self.leave_set(&id);
            self.arm_set_ticker();
        }
        self.stop_slot(&id);
        self.cut_off_member_views(&id);
        if self.settings.watched_cluster(&id).is_some() {
            self.set_keep_watching(id, None).await;
        } else {
            if self.settings_error.is_some() {
                self.hold_keep_watching_off(&id);
            }
            self.revoke_background_sign_in(&id);
        }
    }

    /// Ends the slot's attempt and connection. Its discovery stays, as for a cluster switched
    /// away from (ADR-0008).
    fn stop_slot(&mut self, id: &str) {
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        for task in [slot.connect_task.take(), slot.discovery_task.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        slot.attempt_lease = None;
        // No attempt has id 0, so a result already queued is ignored, a discovery's included.
        slot.attempt = 0;
        slot.discovering = false;
        slot.rediscover = false;
        slot.reconnect = false;
        slot.awaits_selection = false;
        slot.rejected_attempt = None;
        slot.transient_attempt = None;
        let attempt = slot.member_attempt.take();
        slot.clear_connection();
        slot.state = ConnectionState::Disconnected;
        if let Some(attempt) = attempt {
            self.schedule.finish(attempt);
        }
        self.emit(Event::ConnectionChanged {
            cluster: id.to_owned(),
            state: ConnectionState::Disconnected,
        });
        self.reconcile(id);
    }

    /// A set's views of `id` end for good: selecting it later does not feed the open set.
    fn cut_off_member_views(&mut self, id: &str) {
        let views: Vec<u64> = self
            .subscriptions
            .iter()
            .filter(|(_, (context, _, member))| context == id && member.is_some())
            .map(|(view, _)| *view)
            .collect();
        for view in views {
            if let Some((context, kind, _)) = self.subscriptions.remove(&view) {
                self.hubs.detach_member(&context, &kind, view);
                self.release_hub_if_unwatched(&context, &kind);
            }
        }
    }
}

#[cfg(test)]
#[path = "disconnect_tests.rs"]
mod tests;
