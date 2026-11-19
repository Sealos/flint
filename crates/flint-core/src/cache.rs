//! A keyed cache of values that are built at most once, even when callers race.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

/// Slots keyed by name (usually a cluster).
pub type SlotMap<T> = Mutex<HashMap<String, Arc<Slot<T>>>>;

/// The slot for `key`, replaced when the fingerprint changed or it is older than `ttl`.
pub fn slot<T>(
    map: &SlotMap<T>,
    key: &str,
    fingerprint: u64,
    ttl: Option<Duration>,
) -> Arc<Slot<T>> {
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(key)
        && existing.fingerprint == fingerprint
        && ttl.is_none_or(|ttl| existing.created.elapsed() < ttl)
    {
        return existing.clone();
    }
    let fresh = Arc::new(Slot::new(fingerprint));
    map.insert(key.to_owned(), fresh.clone());
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slot_is_reused_until_fingerprint_changes() {
        let map: SlotMap<u32> = Mutex::default();
        let a = slot(&map, "c", 1, None);
        a.cell.get_or_init(|| async { 7 }).await;
        let b = slot(&map, "c", 1, None);
        assert_eq!(b.cell.get(), Some(&7));
        let c = slot(&map, "c", 2, None);
        assert_eq!(c.cell.get(), None);
        let d = slot(&map, "c", 2, Some(Duration::ZERO));
        assert!(!Arc::ptr_eq(&c, &d), "expired slots are replaced");
    }
}
