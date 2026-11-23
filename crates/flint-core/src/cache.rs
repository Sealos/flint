//! A keyed cache of values that are built at most once, even when callers race, and released once
//! their key stops being live or goes unused.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;

/// One cache entry. Concurrent callers wait on the same `OnceCell` instead of each building the
/// value (connecting, running discovery). A failed build leaves the cell empty, so the next
/// caller tries again.
pub struct Slot<T> {
    /// What the value was built from (for example a context fingerprint). A different
    /// fingerprint replaces the slot.
    pub fingerprint: u64,
    pub created: Instant,
    pub cell: OnceCell<T>,
}

impl<T> Slot<T> {
    pub fn new(fingerprint: u64) -> Self {
        Self {
            fingerprint,
            created: Instant::now(),
            cell: OnceCell::new(),
        }
    }
}

/// The keys that may hold a slot, each with the fingerprint it is live at.
pub type LiveKeys = Arc<HashMap<String, u64>>;

struct Entry<T> {
    slot: Arc<Slot<T>>,
    used: Instant,
}

struct State<T> {
    slots: HashMap<String, Entry<T>>,
    /// Slots being checked, shared so that racing callers still build once. Weak: a caller
    /// cancelled during its check leaves nothing behind.
    pending: HashMap<String, Weak<Slot<T>>>,
    live: LiveKeys,
    /// Moves whenever the live keys change, so a check made before the change cannot publish.
    epoch: u64,
}

/// What [`SlotCache::acquire`] found.
pub enum Acquired<T> {
    /// Stored earlier, ready to use.
    Stored(Arc<Slot<T>>),
    /// Not stored. Check that the key is live by a read made after this call, then
    /// [`SlotCache::publish`] it with `epoch`.
    Pending { slot: Arc<Slot<T>>, epoch: u64 },
}

/// Slots keyed by name (usually a cluster), held only for keys in the latest [`LiveKeys`].
pub struct SlotCache<T> {
    /// How long a value is reused at most, used or not.
    ttl: Option<Duration>,
    /// How long a slot nobody is using is kept.
    idle: Duration,
    state: Mutex<State<T>>,
}

impl<T> SlotCache<T> {
    pub fn new(ttl: Option<Duration>, idle: Duration) -> Self {
        Self {
            ttl,
            idle,
            state: Mutex::new(State {
                slots: HashMap::new(),
                pending: HashMap::new(),
                live: LiveKeys::default(),
                epoch: 0,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_fresh(&self, slot: &Slot<T>, now: Instant) -> bool {
        self.ttl
            .is_none_or(|ttl| now.duration_since(slot.created) < ttl)
    }

    /// The stored slot for `key`, or one to check and publish when none matches `fingerprint`
    /// or it outlived the TTL.
    pub fn acquire(&self, key: &str, fingerprint: u64) -> Acquired<T> {
        let mut guard = self.state();
        let state = &mut *guard;
        let now = Instant::now();
        if let Some(entry) = state.slots.get_mut(key)
            && entry.slot.fingerprint == fingerprint
            && self.is_fresh(&entry.slot, now)
        {
            entry.used = now;
            return Acquired::Stored(entry.slot.clone());
        }
        let shared = state
            .pending
            .get(key)
            .and_then(Weak::upgrade)
            .filter(|slot| slot.fingerprint == fingerprint && self.is_fresh(slot, now));
        let slot = shared.unwrap_or_else(|| {
            let slot = Arc::new(Slot::new(fingerprint));
            state.pending.insert(key.to_owned(), Arc::downgrade(&slot));
            slot
        });
        Acquired::Pending {
            slot,
            epoch: state.epoch,
        }
    }

    /// Stores a checked `slot`, unless the live keys changed since `epoch` or no longer cover
    /// it. A slot refused here still serves the call that holds it.
    pub fn publish(&self, key: &str, slot: &Arc<Slot<T>>, epoch: u64) {
        let mut state = self.state();
        if state.epoch != epoch || state.live.get(key) != Some(&slot.fingerprint) {
            return;
        }
        let now = Instant::now();
        if let Some(entry) = state.slots.get(key)
            && entry.slot.fingerprint == slot.fingerprint
            && self.is_fresh(&entry.slot, now)
        {
            return;
        }
        let entry = Entry {
            slot: slot.clone(),
            used: now,
        };
        state.slots.insert(key.to_owned(), entry);
    }

    /// Makes `live` the live keys and releases every slot it does not cover. Reads can overlap
    /// and land in any order: one that is out of date can release a slot, which is built again,
    /// but only [`Self::publish`] stores one.
    pub fn retain_live(&self, live: &LiveKeys) {
        let mut state = self.state();
        if state.live != *live {
            state.epoch += 1;
            state.live = live.clone();
        }
        state
            .slots
            .retain(|key, entry| live.get(key) == Some(&entry.slot.fingerprint));
        state.pending.retain(|key, slot| {
            slot.upgrade()
                .is_some_and(|slot| live.get(key) == Some(&slot.fingerprint))
        });
    }

    /// Releases the slots past their TTL, and those idle at `now` that no caller still holds.
    pub fn expire(&self, now: Instant) {
        let mut state = self.state();
        state.slots.retain(|_, entry| {
            let active =
                now.duration_since(entry.used) < self.idle || Arc::strong_count(&entry.slot) > 1;
            self.is_fresh(&entry.slot, now) && active
        });
        state.pending.retain(|_, slot| slot.strong_count() > 0);
    }

    /// Whether a slot is stored for `key`.
    pub fn holds(&self, key: &str) -> bool {
        self.state().slots.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(60);

    fn live(entries: &[(&str, u64)]) -> LiveKeys {
        Arc::new(entries.iter().map(|(k, f)| ((*k).to_owned(), *f)).collect())
    }

    fn cache(ttl: Option<Duration>, entries: &[(&str, u64)]) -> SlotCache<u32> {
        let cache = SlotCache::new(ttl, IDLE);
        cache.retain_live(&live(entries));
        cache
    }

    /// The slot for `key`, published at once if it was pending.
    fn stored(map: &SlotCache<u32>, key: &str, fingerprint: u64) -> Arc<Slot<u32>> {
        match map.acquire(key, fingerprint) {
            Acquired::Stored(slot) => slot,
            Acquired::Pending { slot, epoch } => {
                map.publish(key, &slot, epoch);
                slot
            }
        }
    }

    #[tokio::test]
    async fn slot_is_reused_until_fingerprint_changes() {
        let map = cache(None, &[("c", 1)]);
        let a = stored(&map, "c", 1);
        a.cell.get_or_init(|| async { 7 }).await;
        assert_eq!(stored(&map, "c", 1).cell.get(), Some(&7));
        map.retain_live(&live(&[("c", 2)]));
        assert!(!map.holds("c"), "the old definition's value is released");
        assert_eq!(stored(&map, "c", 2).cell.get(), None);
    }

    #[test]
    fn an_expired_slot_is_replaced() {
        let map = cache(Some(Duration::ZERO), &[("c", 1)]);
        let a = stored(&map, "c", 1);
        assert!(!Arc::ptr_eq(&a, &stored(&map, "c", 1)));
    }

    #[test]
    fn racing_callers_share_a_pending_slot() {
        let map = cache(None, &[("c", 1)]);
        let (Acquired::Pending { slot: a, .. }, Acquired::Pending { slot: b, .. }) =
            (map.acquire("c", 1), map.acquire("c", 1))
        else {
            panic!("nothing was published");
        };
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!map.holds("c"), "not stored before its check");
    }

    #[test]
    fn a_key_no_longer_live_is_released() {
        let map = cache(None, &[("a", 1), ("b", 2)]);
        drop((stored(&map, "a", 1), stored(&map, "b", 2)));
        map.retain_live(&live(&[("b", 2)]));
        assert!(!map.holds("a"));
        assert!(map.holds("b"));
    }

    #[test]
    fn a_key_not_live_is_not_published() {
        let map = cache(None, &[("c", 1)]);
        map.retain_live(&live(&[]));
        drop(stored(&map, "c", 1));
        assert!(!map.holds("c"));
    }

    // An out-of-date read that lands last shows the key as live again: the check made before
    // the revocation still cannot publish.
    #[test]
    fn a_check_from_before_a_change_cannot_publish() {
        let map = cache(None, &[("c", 1)]);
        let Acquired::Pending { slot, epoch } = map.acquire("c", 1) else {
            panic!("nothing was published");
        };
        map.retain_live(&live(&[]));
        map.retain_live(&live(&[("c", 1)]));
        map.publish("c", &slot, epoch);
        assert!(!map.holds("c"));
    }

    #[test]
    fn a_dropped_pending_slot_leaves_nothing() {
        let map = cache(None, &[("c", 1)]);
        let first = map.acquire("c", 1);
        let Acquired::Pending { slot, .. } = &first else {
            panic!("nothing was published");
        };
        let weak = Arc::downgrade(slot);
        drop(first);
        assert_eq!(weak.strong_count(), 0);
        map.expire(Instant::now());
        assert!(map.state().pending.is_empty());
    }

    #[test]
    fn idle_slots_expire_unless_a_caller_holds_them() {
        let map = cache(None, &[("held", 1), ("idle", 2)]);
        let held = stored(&map, "held", 1);
        drop(stored(&map, "idle", 2));
        map.expire(Instant::now());
        assert!(map.holds("idle"), "not idle yet");
        map.expire(Instant::now() + IDLE);
        assert!(!map.holds("idle"));
        assert!(map.holds("held"));
        drop(held);
        map.expire(Instant::now() + IDLE);
        assert!(!map.holds("held"));
    }

    #[test]
    fn the_ttl_expires_a_slot_even_while_held() {
        let ttl = Duration::from_secs(5);
        let map = cache(Some(ttl), &[("c", 1)]);
        let _held = stored(&map, "c", 1);
        map.expire(Instant::now() + ttl);
        assert!(!map.holds("c"));
    }
}
