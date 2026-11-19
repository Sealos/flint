//! One snapshot channel per cluster and kind, shared by the watch task that fills it and every
//! subscription that reads it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, watch};

use crate::cluster::ClusterId;
use crate::read::KindId;
use crate::store::{Feed, KindSnapshot, SyncState};

pub(crate) struct Hub {
    tx: watch::Sender<Arc<KindSnapshot>>,
    /// Signalled when a subscriber arrives, so the watch task publishes changes it held back
    /// while nobody was listening.
    pub wake: Notify,
    /// The epoch of the task allowed to publish. Bumped when a task starts or is stopped, so a
    /// stopped task that is still unwinding cannot overwrite the manager's "idle" snapshot.
    owner: Mutex<u64>,
    /// The watch task's current state, kept even while nobody subscribes and so no snapshot is
    /// published. For status displays ([`HubRegistry::states`]).
    state: Mutex<SyncState>,
    /// How the watch task actually feeds the kind: it falls back to polling when watching is
    /// refused. For [`crate::CoreHandle::watched_kinds`].
    feed: Mutex<Feed>,
    /// Told about every publish. Set for the per-namespace hubs a namespace-limited list merges
    /// (see `engine::Engine::run_namespaced`), so the merger never polls.
    on_publish: Option<Arc<Notify>>,
}

/// Snapshot versions come from one counter for the whole process, so a version never repeats
/// for a cluster and kind, even after its hub is dropped and created again.
static VERSION: AtomicU64 = AtomicU64::new(0);

impl Hub {
    pub fn new(cluster: ClusterId, kind: KindId) -> Self {
        let (tx, _) = watch::channel(Arc::new(KindSnapshot::empty(
            cluster,
            kind,
            SyncState::Idle,
        )));
        Hub {
            tx,
            wake: Notify::new(),
            owner: Mutex::new(0),
            state: Mutex::new(SyncState::Idle),
            feed: Mutex::new(Feed::Watch),
            on_publish: None,
        }
    }

    /// A hub outside the registry that tells `notify` whenever it publishes.
    pub fn child(cluster: ClusterId, kind: KindId, notify: Arc<Notify>) -> Self {
        Hub {
            on_publish: Some(notify),
            ..Hub::new(cluster, kind)
        }
    }

    /// Live subscriptions (receivers).
    pub fn listeners(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<KindSnapshot>> {
        self.tx.subscribe()
    }

    pub fn latest(&self) -> Arc<KindSnapshot> {
        self.tx.borrow().clone()
    }

    /// Publishes `snapshot` if `epoch` still owns the hub.
    pub fn publish(&self, epoch: u64, snapshot: KindSnapshot) -> bool {
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        if *owner != epoch {
            return false;
        }
        self.send(snapshot);
        true
    }

    /// Records the watch task's state if `epoch` still owns the hub. Cheap: no snapshot.
    pub fn set_state(&self, epoch: u64, state: &SyncState) {
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        if *owner == epoch {
            *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
            // A merger follows the state of its lanes even while they publish no snapshots.
            if let Some(notify) = &self.on_publish {
                notify.notify_one();
            }
        }
    }

    /// The watch task's current state, whether or not it was published.
    pub fn state(&self) -> SyncState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Records how the watch task feeds the kind if `epoch` still owns the hub.
    pub fn set_feed(&self, epoch: u64, feed: Feed) {
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        if *owner == epoch {
            *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = feed;
        }
    }

    pub fn feed(&self) -> Feed {
        *self.feed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes the hub away from any running task, publishes `snapshot`, and returns the new
    /// epoch for the next task.
    pub fn take_over(&self, snapshot: KindSnapshot) -> u64 {
        let mut owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        *owner += 1;
        self.send(snapshot);
        *owner
    }

    fn send(&self, mut snapshot: KindSnapshot) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = snapshot.state.clone();
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = snapshot.feed;
        snapshot.version = VERSION.fetch_add(1, Ordering::Relaxed) + 1;
        self.tx.send_replace(Arc::new(snapshot));
        if let Some(notify) = &self.on_publish {
            notify.notify_one();
        }
    }
}

type HubMap = HashMap<(ClusterId, KindId), Arc<Hub>>;

/// Every hub of a core, shared between the manager (which starts and stops watches) and the
/// front-end handle (which subscribes synchronously).
#[derive(Clone, Default)]
pub(crate) struct HubRegistry(Arc<Mutex<HubMap>>);

impl HubRegistry {
    pub fn get_or_create(&self, cluster: &str, kind: &KindId) -> Arc<Hub> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entry(&mut map, cluster, kind)
    }

    /// Subscribes under the registry lock, so [`HubRegistry::release`] cannot drop the hub in
    /// between.
    pub fn subscribe(
        &self,
        cluster: &str,
        kind: &KindId,
    ) -> (Arc<Hub>, watch::Receiver<Arc<KindSnapshot>>) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let hub = entry(&mut map, cluster, kind);
        let rx = hub.subscribe();
        (hub, rx)
    }

    /// The sync state of every kind of `cluster` that has a hub (watched, polled, or
    /// subscribed), without subscribing. Sorted by kind.
    pub fn states(&self, cluster: &str) -> Vec<(KindId, SyncState)> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut states: Vec<_> = map
            .iter()
            .filter(|((c, _), _)| c == cluster)
            .map(|((_, kind), hub)| (kind.clone(), hub.state()))
            .collect();
        drop(map);
        states.sort_by(|a, b| a.0.cmp(&b.0));
        states
    }

    /// Forgets a hub nobody reads. Only call it for kinds no task is running for.
    pub fn release(&self, cluster: &str, kind: &KindId) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let key = (cluster.to_owned(), kind.clone());
        if map.get(&key).is_some_and(|h| h.listeners() == 0) {
            map.remove(&key);
        }
    }
}

fn entry(map: &mut HubMap, cluster: &str, kind: &KindId) -> Arc<Hub> {
    map.entry((cluster.to_owned(), kind.clone()))
        .or_insert_with(|| Arc::new(Hub::new(cluster.to_owned(), kind.clone())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-27
    #[test]
    fn states_report_the_watch_state_without_subscribing() {
        let registry = HubRegistry::default();
        let pods = KindId::pods();
        let hub = registry.get_or_create("c1", &pods);
        let other = registry.get_or_create("c2", &KindId::namespaces());
        let epoch = hub.take_over(KindSnapshot::empty(
            "c1".into(),
            pods.clone(),
            SyncState::Loading,
        ));
        other.take_over(KindSnapshot::empty(
            "c2".into(),
            KindId::namespaces(),
            SyncState::Synced,
        ));
        assert_eq!(registry.states("c1"), [(pods.clone(), SyncState::Loading)]);

        // Nobody listens, so nothing is published, but the state is still visible.
        hub.set_state(epoch, &SyncState::Reconnecting { error: None });
        assert_eq!(hub.listeners(), 0);
        assert_eq!(hub.latest().state, SyncState::Loading);
        assert_eq!(
            registry.states("c1"),
            [(pods.clone(), SyncState::Reconnecting { error: None })]
        );

        // A stopped task cannot overwrite the state of its successor.
        let next = hub.take_over(KindSnapshot::empty(
            "c1".into(),
            pods.clone(),
            SyncState::Idle,
        ));
        hub.set_state(epoch, &SyncState::Synced);
        assert_eq!(registry.states("c1"), [(pods.clone(), SyncState::Idle)]);
        hub.set_state(next, &SyncState::Synced);
        assert_eq!(registry.states("c1"), [(pods, SyncState::Synced)]);
    }
}
