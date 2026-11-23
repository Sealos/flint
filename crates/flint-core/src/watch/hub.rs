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
    /// The outage of this hub's cluster, ended by any of its kinds listing again.
    outage: Option<OutageSlot>,
    /// Channels of subscriptions bound to an admitted member, by subscription id. Each gets what
    /// the hub publishes only until [`Hub::detach_member`], so it never sees another server's rows.
    member_feeds: Mutex<HashMap<u64, MemberFeed>>,
}

/// The channel of one subscription bound to an admitted member.
pub(crate) type MemberFeed = watch::Sender<Arc<KindSnapshot>>;

/// A cluster that lost its connection: the error, and the last rows of the kinds whose watches
/// stopped meanwhile, so reopening one shows them, marked stale, instead of an empty list.
struct Outage {
    error: String,
    rows: HashMap<KindId, Arc<KindSnapshot>>,
}

type OutageSlot = Arc<Mutex<Option<Outage>>>;

/// What a kind starting during an outage begins from.
pub(crate) struct Resume {
    pub state: SyncState,
    /// Its rows from before, if it had any.
    pub rows: Option<Arc<KindSnapshot>>,
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
            outage: None,
            member_feeds: Mutex::new(HashMap::new()),
        }
    }

    /// A hub outside the registry that tells `notify` whenever it publishes.
    pub fn child(cluster: ClusterId, kind: KindId, notify: Arc<Notify>) -> Self {
        Hub {
            on_publish: Some(notify),
            ..Hub::new(cluster, kind)
        }
    }

    /// Live subscriptions (receivers), those bound to a member included.
    pub fn listeners(&self) -> usize {
        let feeds = self.member_feeds.lock().unwrap_or_else(|e| e.into_inner());
        self.tx.receiver_count() + feeds.values().filter(|f| !f.is_closed()).count()
    }

    #[cfg(test)]
    pub fn member_feeds_empty(&self) -> bool {
        self.member_feeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Sends `snapshot` to every subscription, those bound to a member included.
    fn deliver(&self, snapshot: Arc<KindSnapshot>) {
        self.tx.send_replace(snapshot.clone());
        let mut feeds = self.member_feeds.lock().unwrap_or_else(|e| e.into_inner());
        feeds.retain(|_, feed| !feed.is_closed());
        for feed in feeds.values() {
            feed.send_replace(snapshot.clone());
        }
    }

    /// Starts `feed` on what the hub holds now. Under the lock [`Hub::deliver`] takes last, so no
    /// publish falls between the two.
    fn attach_member(&self, id: u64, feed: MemberFeed) {
        let mut feeds = self.member_feeds.lock().unwrap_or_else(|e| e.into_inner());
        feed.send_replace(self.latest());
        feeds.insert(id, feed);
        drop(feeds);
        self.wake.notify_one();
    }

    /// Ends subscription `id`'s feed for good: it reads as not watched, with no rows.
    fn detach_member(&self, id: u64) {
        let mut feeds = self.member_feeds.lock().unwrap_or_else(|e| e.into_inner());
        let Some(feed) = feeds.remove(&id) else {
            return;
        };
        let latest = self.latest();
        let mut idle =
            KindSnapshot::empty(latest.cluster.clone(), latest.kind.clone(), SyncState::Idle);
        idle.namespaced = latest.namespaced;
        idle.version = VERSION.fetch_add(1, Ordering::Relaxed) + 1;
        feed.send_replace(Arc::new(idle));
    }

    /// Forgets subscription `id`'s channel: the subscription has closed.
    fn remove_member(&self, id: u64) {
        let mut feeds = self.member_feeds.lock().unwrap_or_else(|e| e.into_inner());
        feeds.remove(&id);
    }

    /// Replaces what the hub holds with nothing, for a connection that has ended. Open receivers
    /// stay connected and read idle until the next connection's watch publishes.
    fn reset(&self) {
        let latest = self.latest();
        if self.state() == SyncState::Idle
            && latest.state == SyncState::Idle
            && latest.rows.is_empty()
        {
            return;
        }
        self.take_over(KindSnapshot::empty(
            latest.cluster.clone(),
            latest.kind.clone(),
            SyncState::Idle,
        ));
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
            self.end_outage_if_synced(state);
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
        self.end_outage_if_synced(&state);
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = snapshot.feed;
        snapshot.version = VERSION.fetch_add(1, Ordering::Relaxed) + 1;
        self.deliver(Arc::new(snapshot));
        drop(state);
        if let Some(notify) = &self.on_publish {
            notify.notify_one();
        }
    }

    /// A kind that lists again proves the cluster is back, so what the outage kept goes.
    fn end_outage_if_synced(&self, state: &SyncState) {
        if *state == SyncState::Synced
            && let Some(outage) = &self.outage
        {
            *outage.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// Makes what a new subscriber reads first agree with an outage, so it never starts on an
    /// empty "loading" list. Only the published snapshot changes: the recorded state stays the
    /// watch task's, so a kind nothing watches never counts as reconnecting.
    fn show_outage(&self, error: &str, rows: Option<&Arc<KindSnapshot>>) {
        let _owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        let latest = self.latest();
        let (base, state) = match self.state() {
            SyncState::Idle => (
                rows.unwrap_or(&latest),
                SyncState::Reconnecting {
                    error: Some(error.to_owned()),
                },
            ),
            // A running watch that had nobody to publish to.
            state @ SyncState::Reconnecting { error: Some(_) } => (&latest, state),
            _ => return,
        };
        if latest.state == state {
            return;
        }
        let snapshot = KindSnapshot {
            state,
            version: VERSION.fetch_add(1, Ordering::Relaxed) + 1,
            ..(**base).clone()
        };
        self.deliver(Arc::new(snapshot));
    }

    /// Takes back what [`Hub::show_outage`] put on a hub no watch owns.
    fn clear_shown_outage(&self) {
        let _owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        let latest = self.latest();
        if self.state() != SyncState::Idle || latest.state == SyncState::Idle {
            return;
        }
        let mut idle =
            KindSnapshot::empty(latest.cluster.clone(), latest.kind.clone(), SyncState::Idle);
        idle.namespaced = latest.namespaced;
        idle.version = VERSION.fetch_add(1, Ordering::Relaxed) + 1;
        self.deliver(Arc::new(idle));
    }
}

type HubMap = HashMap<(ClusterId, KindId), Arc<Hub>>;

#[derive(Default)]
struct Hubs {
    map: HubMap,
    outages: HashMap<ClusterId, OutageSlot>,
}

impl Hubs {
    /// The hub of `kind`, showing the cluster's outage to whoever subscribes next.
    fn entry_showing_outage(&mut self, cluster: &str, kind: &KindId) -> Arc<Hub> {
        let hub = entry(self, cluster, kind);
        match self.kept(cluster, kind) {
            Some((error, rows)) => hub.show_outage(&error, Some(&rows)),
            None => {
                if let Some(error) = self.outage_error(cluster) {
                    hub.show_outage(&error, None);
                }
            }
        }
        hub
    }

    /// The connection error of `cluster`, if it is in an outage: no kind is current and one
    /// reports a connection error, or one did when the last watch stopped.
    fn outage_error(&self, cluster: &str) -> Option<String> {
        let mut live: Option<(&KindId, String)> = None;
        for ((c, kind), hub) in &self.map {
            if c != cluster {
                continue;
            }
            match hub.state() {
                SyncState::Synced => return None,
                SyncState::Reconnecting { error: Some(error) }
                    if live.as_ref().is_none_or(|(k, _)| kind < *k) =>
                {
                    live = Some((kind, error));
                }
                _ => {}
            }
        }
        live.map(|(_, error)| error).or_else(|| {
            let slot = self.outages.get(cluster)?;
            let outage = slot.lock().unwrap_or_else(|e| e.into_inner());
            outage.as_ref().map(|o| o.error.clone())
        })
    }

    /// The rows the outage kept for `kind`, with the error they were kept under. A quiet watch
    /// still reporting synced has not noticed the drop, so it does not overrule them.
    fn kept(&self, cluster: &str, kind: &KindId) -> Option<(String, Arc<KindSnapshot>)> {
        let slot = self.outages.get(cluster)?;
        let outage = slot.lock().unwrap_or_else(|e| e.into_inner());
        let outage = outage.as_ref()?;
        Some((outage.error.clone(), outage.rows.get(kind)?.clone()))
    }

    fn clear_shown_outages(&self, cluster: &str) {
        for ((c, _), hub) in &self.map {
            if c == cluster {
                hub.clear_shown_outage();
            }
        }
    }
}

/// Every hub of a core, shared between the manager (which starts and stops watches) and the
/// front-end handle (which subscribes synchronously).
#[derive(Clone, Default)]
pub(crate) struct HubRegistry(Arc<Mutex<Hubs>>);

impl HubRegistry {
    pub fn get_or_create(&self, cluster: &str, kind: &KindId) -> Arc<Hub> {
        let mut hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entry(&mut hubs, cluster, kind)
    }

    /// Subscribes under the registry lock, so [`HubRegistry::release`] cannot drop the hub in
    /// between.
    pub fn subscribe(
        &self,
        cluster: &str,
        kind: &KindId,
    ) -> (Arc<Hub>, watch::Receiver<Arc<KindSnapshot>>) {
        let mut hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let hub = hubs.entry_showing_outage(cluster, kind);
        let rx = hub.subscribe();
        (hub, rx)
    }

    /// Connects the channel of subscription `id`, bound to a member that holds now, to the hub.
    /// Only the manager calls this, in the step that checked the member.
    pub fn attach_member(&self, cluster: &str, kind: &KindId, id: u64, feed: MemberFeed) {
        let mut hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        hubs.entry_showing_outage(cluster, kind)
            .attach_member(id, feed);
    }

    /// Cuts subscription `id` off from the hub: its member no longer stands for the context.
    pub fn detach_member(&self, cluster: &str, kind: &KindId, id: u64) {
        let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hub) = hubs.map.get(&(cluster.to_owned(), kind.clone())) {
            hub.detach_member(id);
        }
    }

    /// Removes the channel of subscription `id`, which has closed.
    pub fn remove_member(&self, cluster: &str, kind: &KindId, id: u64) {
        let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hub) = hubs.map.get(&(cluster.to_owned(), kind.clone())) {
            hub.remove_member(id);
        }
    }

    /// Drops everything `cluster`'s ended connection left: its outage and each hub's snapshot,
    /// including those of kinds no watch runs for. Open receivers stay connected.
    pub fn reset_connection(&self, cluster: &str) {
        let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = hubs.outages.get(cluster) {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        for ((c, _), hub) in &hubs.map {
            if c == cluster {
                hub.reset();
            }
        }
    }

    /// Keeps the last rows of a kind whose watch is stopping, if that watch has lost its
    /// connection. Call it before the hub's rows are replaced.
    pub fn keep_for_outage(&self, cluster: &str, kind: &KindId, hub: &Hub) {
        let SyncState::Reconnecting { error: Some(error) } = hub.state() else {
            return;
        };
        let latest = hub.latest();
        let mut hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let slot = hubs.outages.entry(cluster.to_owned()).or_default();
        let mut outage = slot.lock().unwrap_or_else(|e| e.into_inner());
        let outage = outage.get_or_insert_with(|| Outage {
            error: String::new(),
            rows: HashMap::new(),
        });
        outage.error = error;
        if !latest.rows.is_empty() {
            outage.rows.insert(kind.clone(), latest);
        }
    }

    /// What `kind` starts from if its cluster is in an outage. Its kept rows move to the caller.
    pub fn resume(&self, cluster: &str, kind: &KindId) -> Option<Resume> {
        let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let kept = hubs.kept(cluster, kind);
        let error = match &kept {
            Some((error, _)) => error.clone(),
            None => hubs.outage_error(cluster)?,
        };
        if kept.is_some()
            && let Some(slot) = hubs.outages.get(cluster)
            && let Some(outage) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            outage.rows.remove(kind);
        }
        Some(Resume {
            state: SyncState::Reconnecting { error: Some(error) },
            rows: kept.map(|(_, rows)| rows),
        })
    }

    /// Drops what an outage kept: the cluster has a new connection, or none. That includes rows
    /// already shown on a hub no watch owns, so nobody reads the old connection's rows.
    pub fn forget_outage(&self, cluster: &str) {
        let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = hubs.outages.get(cluster) {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        hubs.clear_shown_outages(cluster);
    }

    /// Drops the kept rows of a cluster going cold (ADR-0008: a cluster that is not active holds
    /// no rows). The error stays, so coming back during the outage says reconnecting.
    pub fn release_kept_rows(&self, cluster: &str) {
        let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = hubs.outages.get(cluster)
            && let Some(outage) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            outage.rows = HashMap::new();
        }
        hubs.clear_shown_outages(cluster);
    }

    /// Drops the rows kept for `kind`: discovery no longer serves them under the same resource.
    pub fn forget_kept(&self, cluster: &str, kind: &KindId) {
        let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = hubs.outages.get(cluster)
            && let Some(outage) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            outage.rows.remove(kind);
        }
        if let Some(hub) = hubs.map.get(&(cluster.to_owned(), kind.clone())) {
            hub.clear_shown_outage();
        }
    }

    /// The sync state of every kind of `cluster` that has a hub (watched, polled, or
    /// subscribed), without subscribing. Sorted by kind.
    pub fn states(&self, cluster: &str) -> Vec<(KindId, SyncState)> {
        let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut states: Vec<_> = hubs
            .map
            .iter()
            .filter(|((c, _), _)| c == cluster)
            .map(|((_, kind), hub)| (kind.clone(), hub.state()))
            .collect();
        drop(hubs);
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
            let hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
            hubs.map
                .iter()
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
        let mut hubs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let key = (cluster.to_owned(), kind.clone());
        if hubs.map.get(&key).is_some_and(|h| h.listeners() == 0) {
            hubs.map.remove(&key);
        }
    }
}

fn entry(hubs: &mut Hubs, cluster: &str, kind: &KindId) -> Arc<Hub> {
    let outage = hubs.outages.entry(cluster.to_owned()).or_default().clone();
    hubs.map
        .entry((cluster.to_owned(), kind.clone()))
        .or_insert_with(|| {
            Arc::new(Hub {
                outage: Some(outage),
                ..Hub::new(cluster.to_owned(), kind.clone())
            })
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_member_feed_gets_nothing_more_and_reads_idle() {
        let registry = HubRegistry::default();
        let pods = KindId::pods();
        let hub = registry.get_or_create("c1", &pods);
        let epoch = hub.take_over(KindSnapshot::empty(
            "c1".into(),
            pods.clone(),
            SyncState::Synced,
        ));
        let (feed, mut rx) = watch::channel(Arc::new(KindSnapshot::default()));
        registry.attach_member("c1", &pods, 7, feed);
        assert_eq!(rx.borrow_and_update().state, SyncState::Synced);
        assert_eq!(hub.listeners(), 1, "keeps the hub and its publishing alive");

        registry.detach_member("c1", &pods, 7);
        assert_eq!(rx.borrow_and_update().state, SyncState::Idle);
        assert!(hub.publish(
            epoch,
            KindSnapshot::empty("c1".into(), pods.clone(), SyncState::Loading)
        ));
        assert!(rx.has_changed().is_err(), "nothing after the cut");
        assert_eq!(hub.listeners(), 0);
    }

    #[test]
    fn a_member_feed_reads_the_outage_of_its_own_connection() {
        let registry = HubRegistry::default();
        let pods = KindId::pods();
        let hub = registry.get_or_create("c1", &pods);
        let error = SyncState::Reconnecting {
            error: Some("gone".to_owned()),
        };
        hub.take_over(KindSnapshot::empty(
            "c1".into(),
            pods.clone(),
            error.clone(),
        ));
        let (feed, rx) = watch::channel(Arc::new(KindSnapshot::default()));
        registry.attach_member("c1", &pods, 1, feed);
        assert_eq!(rx.borrow().state, error);
    }

    #[test]
    fn a_closed_member_feed_leaves_the_hub_without_a_publish() {
        let registry = HubRegistry::default();
        let pods = KindId::pods();
        let hub = registry.get_or_create("c1", &pods);
        let (feed, rx) = watch::channel(Arc::new(KindSnapshot::default()));
        registry.attach_member("c1", &pods, 1, feed);
        drop(rx);

        registry.remove_member("c1", &pods, 1);
        assert!(hub.member_feeds_empty());
    }

    #[test]
    fn a_connection_reset_clears_a_kind_no_watch_runs_and_keeps_its_receivers() {
        let registry = HubRegistry::default();
        let pods = KindId::pods();
        let hub = registry.get_or_create("c1", &pods);
        let mut plain = hub.subscribe();
        hub.take_over(KindSnapshot::empty(
            "c1".into(),
            pods.clone(),
            SyncState::Unavailable,
        ));
        plain.borrow_and_update();

        registry.reset_connection("c1");
        assert!(plain.has_changed().unwrap(), "the receiver stays connected");
        assert_eq!(plain.borrow_and_update().state, SyncState::Idle);
        let (feed, rx) = watch::channel(Arc::new(KindSnapshot::default()));
        registry.attach_member("c1", &pods, 1, feed);
        assert_eq!(rx.borrow().state, SyncState::Idle);
    }

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
        let hubs = registry.0.lock().unwrap();
        let map = &hubs.map;
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
