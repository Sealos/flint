//! A keyed cache of values that are built at most once, even when callers race, and released once
//! their key stops being live or goes unused.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
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
    live: LiveKeys,
    /// The newest view of the live keys applied, so an older one cannot bring a key back.
    view: u64,
}

/// Slots keyed by name (usually a cluster), held only for keys in the newest [`LiveKeys`].
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
                live: LiveKeys::default(),
                view: 0,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The slot for `key`, replaced when the fingerprint changed or it outlived the TTL. A caller
    /// working from a view that is no longer live gets a slot of its own that is never stored, so
    /// a call in flight cannot put back what was released while it ran.
    pub fn acquire(&self, key: &str, fingerprint: u64) -> Arc<Slot<T>> {
        let mut state = self.state();
        if state.live.get(key) != Some(&fingerprint) {
            return Arc::new(Slot::new(fingerprint));
        }
        let now = Instant::now();
        if let Some(entry) = state.slots.get_mut(key)
            && entry.slot.fingerprint == fingerprint
            && self
                .ttl
                .is_none_or(|ttl| now.duration_since(entry.slot.created) < ttl)
        {
            entry.used = now;
            return entry.slot.clone();
        }
        let slot = Arc::new(Slot::new(fingerprint));
        let entry = Entry {
            slot: slot.clone(),
            used: now,
        };
        state.slots.insert(key.to_owned(), entry);
        slot
    }

    /// Releases every slot view number `view` of the live keys does not cover, and makes it the
    /// view new slots are checked against unless a newer one is applied. Reads can overlap, so
    /// an older view still releases: at worst a value is built again.
    pub fn retain_live(&self, view: u64, live: &LiveKeys) {
        let mut state = self.state();
        if view > state.view {
            state.view = view;
            state.live = live.clone();
        }
        state
            .slots
            .retain(|key, entry| live.get(key) == Some(&entry.slot.fingerprint));
    }

    /// Releases `slot` if it is still the one stored for `key`.
    pub fn release(&self, key: &str, slot: &Arc<Slot<T>>) {
        let mut state = self.state();
        if state
            .slots
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.slot, slot))
        {
            state.slots.remove(key);
        }
    }

    /// Releases the slots past their TTL, and those idle at `now` that no caller still holds.
    pub fn expire(&self, now: Instant) {
        let (ttl, idle) = (self.ttl, self.idle);
        self.state().slots.retain(|_, entry| {
            let fresh = ttl.is_none_or(|ttl| now.duration_since(entry.slot.created) < ttl);
            let active =
                now.duration_since(entry.used) < idle || Arc::strong_count(&entry.slot) > 1;
            fresh && active
        });
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
        cache.retain_live(1, &live(entries));
        cache
    }

    #[tokio::test]
    async fn slot_is_reused_until_fingerprint_changes() {
        let map = cache(None, &[("c", 1)]);
        let a = map.acquire("c", 1);
        a.cell.get_or_init(|| async { 7 }).await;
        assert_eq!(map.acquire("c", 1).cell.get(), Some(&7));
        map.retain_live(2, &live(&[("c", 2)]));
        assert!(!map.holds("c"), "the old definition's value is released");
        assert_eq!(map.acquire("c", 2).cell.get(), None);
    }

    #[test]
    fn an_expired_slot_is_replaced() {
        let map = cache(Some(Duration::ZERO), &[("c", 1)]);
        let a = map.acquire("c", 1);
        assert!(!Arc::ptr_eq(&a, &map.acquire("c", 1)));
    }

    #[test]
    fn a_key_no_longer_live_is_released() {
        let map = cache(None, &[("a", 1), ("b", 2)]);
        drop((map.acquire("a", 1), map.acquire("b", 2)));
        map.retain_live(2, &live(&[("b", 2)]));
        assert!(!map.holds("a"));
        assert!(map.holds("b"));
    }

    #[test]
    fn a_call_in_flight_cannot_put_a_released_key_back() {
        let map = cache(None, &[("c", 1)]);
        map.retain_live(2, &live(&[]));
        let stale = map.acquire("c", 1);
        assert!(!map.holds("c"));
        assert!(!Arc::ptr_eq(&stale, &map.acquire("c", 1)), "never shared");
    }

    #[test]
    fn an_older_view_cannot_bring_a_key_back() {
        let map = cache(None, &[]);
        map.retain_live(3, &live(&[]));
        map.retain_live(2, &live(&[("c", 1)]));
        drop(map.acquire("c", 1));
        assert!(!map.holds("c"));
    }

    #[test]
    fn an_older_view_still_releases() {
        let map = cache(None, &[("c", 1)]);
        map.retain_live(3, &live(&[("c", 1)]));
        drop(map.acquire("c", 1));
        map.retain_live(2, &live(&[]));
        assert!(!map.holds("c"));
        drop(map.acquire("c", 1));
        assert!(map.holds("c"), "the newest view still covers it");
    }

    #[test]
    fn release_leaves_a_newer_slot_in_place() {
        let map = cache(None, &[("c", 1)]);
        let old = map.acquire("c", 1);
        map.release("c", &old);
        assert!(!map.holds("c"));
        let new = map.acquire("c", 1);
        map.release("c", &old);
        assert!(map.holds("c"));
        map.release("c", &new);
        assert!(!map.holds("c"));
    }

    #[test]
    fn idle_slots_expire_unless_a_caller_holds_them() {
        let map = cache(None, &[("held", 1), ("idle", 2)]);
        let held = map.acquire("held", 1);
        drop(map.acquire("idle", 2));
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
        let _held = map.acquire("c", 1);
        map.expire(Instant::now() + ttl);
        assert!(!map.holds("c"));
    }
}
