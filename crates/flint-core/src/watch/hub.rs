//! One snapshot channel per cluster and kind, shared by the watch task that fills it and every
//! subscription that reads it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, oneshot, watch};

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
    /// One-off reads of the rows ([`HubRegistry::stored`]): the watch task answers each with a
    /// snapshot of its own, kept nowhere else. They are not subscribers, so they neither make
    /// the task publish, nor jump the list queue, nor keep the hub from being released.
    peeks: Mutex<Vec<oneshot::Sender<Arc<KindSnapshot>>>>,
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
            peeks: Mutex::new(Vec::new()),
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

    /// Asks the watch task for one snapshot of its rows, without subscribing.
    pub fn peek(&self) -> oneshot::Receiver<Arc<KindSnapshot>> {
        let (tx, rx) = oneshot::channel();
        let mut peeks = self.peeks.lock().unwrap_or_else(|e| e.into_inner());
        // Readers that gave up waiting.
        peeks.retain(|p| !p.is_closed());
        peeks.push(tx);
        drop(peeks);
        self.wake.notify_one();
        rx
    }

    /// Whether a one-off read is waiting for an answer.
    pub fn has_peeks(&self) -> bool {
        !self
            .peeks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Answers the waiting one-off reads with `build()`'s snapshot, if `epoch` still owns the
    /// hub. Nothing is kept in the hub.
    pub fn answer_peeks(&self, epoch: u64, build: impl FnOnce() -> KindSnapshot) {
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        if *owner != epoch {
            return;
        }
        drop(owner);
        let peeks = std::mem::take(&mut *self.peeks.lock().unwrap_or_else(|e| e.into_inner()));
        if peeks.iter().all(|p| p.is_closed()) {
            return;
        }
        let mut snapshot = build();
        snapshot.version = VERSION.fetch_add(1, Ordering::Relaxed) + 1;
        let snapshot = Arc::new(snapshot);
        for peek in peeks {
            let _ = peek.send(snapshot.clone());
        }
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
        // Held across the send, so whoever reads this state finds its rows published.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *state = snapshot.state.clone();
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = snapshot.feed;
        snapshot.version = VERSION.fetch_add(1, Ordering::Relaxed) + 1;
        self.tx.send_replace(Arc::new(snapshot));
        drop(state);
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

    /// What the in-memory stores of `cluster` hold now: one snapshot per kind whose watch has
    /// listed (synced, or reconnecting with its last rows), for a search over every object
    /// (the resource switcher).
    ///
    /// It only reads. It never creates a hub, never tells the manager (so no watch starts, in
    /// either watch mode) and never reaches the API server. A kind someone subscribes to gives
    /// what it last published; any other is asked for one snapshot ([`Hub::peek`]), which its
    /// watch task builds from the rows it holds and keeps nowhere: the hub still holds no rows
    /// afterwards, the task still publishes nothing, its place in the list queue and the hub's
    /// release do not change. A kind that does not answer within `wait` is left out.
    pub async fn stored(&self, cluster: &str, wait: std::time::Duration) -> Vec<Arc<KindSnapshot>> {
        let hubs: Vec<Arc<Hub>> = {
            let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
            map.iter()
                .filter(|((c, _), hub)| {
                    c == cluster
                        && matches!(
                            hub.state(),
                            SyncState::Synced
                                | SyncState::Reconnecting { .. }
                                | SyncState::RetryingRefusal { .. }
                        )
                })
                .map(|(_, hub)| hub.clone())
                .collect()
        };
        let deadline = tokio::time::Instant::now() + wait;
        let mut stored = Vec::with_capacity(hubs.len());
        let mut waiting = Vec::new();
        for hub in hubs {
            if hub.listeners() > 0 {
                // Someone subscribes: what it published is current.
                stored.push(hub.latest());
            } else {
                // Asked all at once, so they build their snapshots in parallel.
                waiting.push(hub.peek());
            }
        }
        for rx in waiting {
            if let Ok(Ok(snapshot)) = tokio::time::timeout_at(deadline, rx).await {
                stored.push(snapshot);
            }
        }
        stored.sort_by(|a, b| a.kind.cmp(&b.kind));
        stored
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

    // covers UP-31: reading the stores creates no hub, skips kinds that hold no rows, leaves
    // nothing in the hubs and does not hold them: a release during the read still happens.
    #[tokio::test(start_paused = true)]
    async fn stored_reads_listed_kinds_only_and_keeps_nothing() {
        let registry = HubRegistry::default();
        let pods = registry.get_or_create("c1", &KindId::pods());
        pods.take_over(KindSnapshot::empty(
            "c1".into(),
            KindId::pods(),
            SyncState::Synced,
        ));
        let read = pods.subscribe();
        registry
            .get_or_create("c1", &KindId::namespaces())
            .take_over(KindSnapshot::empty(
                "c1".into(),
                KindId::namespaces(),
                SyncState::Loading,
            ));
        let unread = registry.get_or_create("c2", &KindId::pods());
        unread.take_over(KindSnapshot::empty(
            "c2".into(),
            KindId::pods(),
            SyncState::Synced,
        ));
        // Subscribed: what it published.
        let stored = registry
            .stored("c1", std::time::Duration::from_millis(50))
            .await;
        let kinds: Vec<&KindId> = stored.iter().map(|s| &s.kind).collect();
        assert_eq!(kinds, [&KindId::pods()]);
        drop(read);
        // Not subscribed and no task to answer: left out, and nothing held meanwhile.
        let reading = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry
                    .stored("c2", std::time::Duration::from_millis(50))
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert_eq!(unread.listeners(), 0, "a read is not a subscriber");
        assert!(unread.has_peeks());
        registry.release("c2", &KindId::pods());
        assert!(reading.await.unwrap().is_empty());
        assert!(
            registry
                .stored("nowhere", Default::default())
                .await
                .is_empty()
        );
        let map = registry.0.lock().unwrap();
        assert_eq!(
            map.len(),
            2,
            "no hub was created, and the released one went"
        );
        assert!(map.values().all(|h| h.listeners() == 0));
    }

    // A peek is answered once, by the owning task only, and leaves the channel as it was.
    #[test]
    fn peeks_are_answered_by_the_owner_and_kept_nowhere() {
        let hub = Hub::new("c".into(), KindId::pods());
        let epoch = hub.take_over(KindSnapshot::default());
        let before = hub.latest();
        let mut rx = hub.peek();
        hub.answer_peeks(epoch + 1, KindSnapshot::default);
        assert!(rx.try_recv().is_err(), "a stopped task does not answer");
        hub.answer_peeks(epoch, || KindSnapshot {
            state: SyncState::Synced,
            ..Default::default()
        });
        assert_eq!(rx.try_recv().unwrap().state, SyncState::Synced);
        assert!(!hub.has_peeks());
        assert!(Arc::ptr_eq(&hub.latest(), &before), "nothing published");
    }
}
