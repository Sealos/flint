//! [`LogBudget`]: one memory budget shared by every open log session (#254).
//!
//! Each session keeps its own line and byte caps (`log_buffer_lines`, `log_buffer_mib`), but
//! nothing else bounds how many tabs are open, so many busy tabs could hold gigabytes. The
//! budget (`log_total_mib`) caps their sum. Sessions report the bytes their buffers hold when
//! they publish (at most about ten times a second). When the total goes over the budget, the
//! budget asks the least recently viewed sessions to drop their oldest blocks, and they do on
//! their own task: streams keep running, only history goes.
//!
//! Order: sessions that are neither shown nor paused go first, least recently viewed first;
//! then paused ones (someone may be reading them); the shown tab never, beyond its own cap. A
//! trimmed session says so in its snapshots ([`super::LogSnapshot::trimmed`]).
//!
//! The total stays within the budget plus what sessions take in between two reports (a block
//! per session, and up to 100 ms of lines), and the shown tab's own cap when it alone is over.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// The shared budget. Cheap to clone through `Arc`.
#[derive(Debug)]
pub struct LogBudget {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Bytes all sessions together may hold.
    limit: usize,
    /// Advances on every change of what is shown: the order of viewing.
    clock: u64,
    sessions: HashMap<u64, Entry>,
}

#[derive(Debug)]
struct Entry {
    /// What the session's buffer held at its last report.
    bytes: usize,
    /// How much of it the session can give back: its sealed blocks (the open tail block
    /// stays). Asking for more would free nothing and skip sessions that could.
    trimmable: usize,
    /// `clock` when it was last shown, or opened.
    viewed: u64,
    shown: bool,
    paused: bool,
    /// Bytes the session should trim its buffer down to, until it does.
    trim_to: Option<usize>,
    wake: Arc<Notify>,
}

impl LogBudget {
    /// A budget of `limit` bytes for all sessions together.
    pub fn new(limit: usize) -> Arc<LogBudget> {
        Arc::new(LogBudget {
            inner: Mutex::new(Inner {
                limit,
                clock: 0,
                sessions: HashMap::new(),
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Changes the budget (the setting changed), trimming at once if the sessions are over it.
    pub fn set_limit(&self, limit: usize) {
        let mut inner = self.lock();
        inner.limit = limit;
        inner.rebalance();
    }

    /// Session `id` starts; it trims when the returned [`Notify`] fires
    /// ([`LogBudget::take_trim`]). A new session counts as just viewed.
    pub(crate) fn register(&self, id: u64) -> Arc<Notify> {
        let mut inner = self.lock();
        inner.clock += 1;
        let wake = Arc::new(Notify::new());
        let viewed = inner.clock;
        inner.sessions.insert(
            id,
            Entry {
                bytes: 0,
                trimmable: 0,
                viewed,
                shown: false,
                paused: false,
                trim_to: None,
                wake: wake.clone(),
            },
        );
        wake
    }

    /// Session `id` ended: its bytes no longer count.
    pub(crate) fn unregister(&self, id: u64) {
        self.lock().sessions.remove(&id);
    }

    /// Whether session `id`'s tab is the one shown. Hiding it counts as having viewed it until
    /// now, so the most recently hidden tab is trimmed last.
    pub fn set_shown(&self, id: u64, shown: bool) {
        let mut inner = self.lock();
        inner.clock += 1;
        let now = inner.clock;
        if let Some(entry) = inner.sessions.get_mut(&id)
            && entry.shown != shown
        {
            entry.shown = shown;
            entry.viewed = now;
            inner.rebalance();
        }
    }

    /// Whether session `id`'s tab is the one shown.
    pub fn is_shown(&self, id: u64) -> bool {
        self.lock().sessions.get(&id).is_some_and(|e| e.shown)
    }

    /// Session `id`'s buffer holds `bytes` now, `trimmable` of them in sealed blocks, and it
    /// is or is not paused. A trim it was asked for and has not done yet still counts as done,
    /// so the excess is not asked of others too.
    pub(crate) fn report(&self, id: u64, bytes: usize, trimmable: usize, paused: bool) {
        let mut inner = self.lock();
        if let Some(entry) = inner.sessions.get_mut(&id) {
            match entry.trim_to {
                Some(target) => {
                    let gone = bytes.saturating_sub(target);
                    entry.bytes = bytes.min(target);
                    entry.trimmable = trimmable.saturating_sub(gone);
                }
                None => {
                    entry.bytes = bytes;
                    entry.trimmable = trimmable;
                }
            }
            entry.paused = paused;
            inner.rebalance();
        }
    }

    /// Session `id` trimmed as asked and holds `bytes` now, `trimmable` of them in sealed
    /// blocks. Recorded without asking anyone again; the next [`LogBudget::report`] looks
    /// again.
    pub(crate) fn trimmed(&self, id: u64, bytes: usize, trimmable: usize) {
        if let Some(entry) = self.lock().sessions.get_mut(&id) {
            entry.bytes = bytes;
            entry.trimmable = trimmable;
        }
    }

    /// What session `id` should trim its buffer down to, if anything; asked once.
    pub(crate) fn take_trim(&self, id: u64) -> Option<usize> {
        self.lock()
            .sessions
            .get_mut(&id)
            .and_then(|e| e.trim_to.take())
    }

    /// Bytes all sessions held at their last reports.
    pub fn total(&self) -> usize {
        self.lock().sessions.values().map(|e| e.bytes).sum()
    }
}

impl Inner {
    /// While over the limit, asks sessions to give bytes back: idle ones least recently viewed
    /// first, then paused ones; never the shown one. Each is asked once; it reports again after
    /// trimming.
    fn rebalance(&mut self) {
        let total: usize = self.sessions.values().map(|e| e.bytes).sum();
        let Some(mut excess) = total.checked_sub(self.limit).filter(|e| *e > 0) else {
            return;
        };
        let mut order: Vec<(&u64, &mut Entry)> = self
            .sessions
            .iter_mut()
            .filter(|(_, e)| !e.shown && e.trimmable > 0)
            .collect();
        order.sort_by_key(|(id, e)| (e.paused, e.viewed, **id));
        for (_, entry) in order {
            if excess == 0 {
                break;
            }
            let cut = excess.min(entry.trimmable);
            let target = entry.bytes - cut;
            // Count what it will give back now, so the next report does not ask again.
            entry.bytes = target;
            entry.trimmable -= cut;
            entry.trim_to = Some(entry.trim_to.map_or(target, |t| t.min(target)));
            entry.wake.notify_one();
            excess -= cut;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    #[test]
    fn the_least_recently_viewed_session_gives_bytes_back_first() {
        let budget = LogBudget::new(100 * MIB);
        for id in 1..=3 {
            budget.register(id);
        }
        // 1 was viewed first, then 2; 3 is shown.
        budget.set_shown(1, true);
        budget.set_shown(1, false);
        budget.set_shown(2, true);
        budget.set_shown(2, false);
        budget.set_shown(3, true);
        budget.report(1, 40 * MIB, 40 * MIB, false);
        budget.report(2, 40 * MIB, 40 * MIB, false);
        assert_eq!(budget.take_trim(1), None, "within the budget");
        budget.report(3, 50 * MIB, 50 * MIB, false);
        // 30 MiB over: all of it from 1, the least recently viewed.
        assert_eq!(budget.take_trim(1), Some(10 * MIB));
        assert_eq!(budget.take_trim(2), None);
        assert_eq!(budget.take_trim(3), None, "the shown tab keeps its own cap");
        assert_eq!(budget.take_trim(1), None, "asked once");
        assert_eq!(budget.total(), 100 * MIB);
    }

    #[test]
    fn paused_sessions_go_after_idle_ones_and_the_shown_one_never() {
        let budget = LogBudget::new(10 * MIB);
        for id in 1..=3 {
            budget.register(id);
        }
        budget.set_shown(3, true);
        // 1 is paused (being read), 2 idle and viewed more recently than 1.
        budget.report(1, 8 * MIB, 8 * MIB, true);
        budget.report(2, 8 * MIB, 8 * MIB, false);
        budget.report(3, 8 * MIB, 8 * MIB, false);
        assert_eq!(
            budget.take_trim(2),
            Some(0),
            "the idle one first, all of it"
        );
        assert_eq!(budget.take_trim(1), Some(2 * MIB), "then the paused one");
        assert_eq!(budget.take_trim(3), None);
        // The shown tab alone over the budget: its own cap is its limit.
        budget.report(1, 0, 0, true);
        budget.report(2, 0, 0, false);
        budget.report(3, 64 * MIB, 64 * MIB, false);
        assert_eq!(budget.take_trim(3), None);
    }

    #[test]
    fn a_session_is_asked_only_for_what_it_can_give_back() {
        let budget = LogBudget::new(10 * MIB);
        for id in 1..=2 {
            budget.register(id);
        }
        // 1, the least recently viewed, holds only its open tail block; 2 has sealed blocks.
        budget.report(1, 3 * MIB, 0, false);
        budget.report(2, 9 * MIB, 8 * MIB, false);
        // A report before 2 has trimmed does not undo the request, nor ask anyone else.
        budget.report(2, 9 * MIB, 8 * MIB, false);
        assert_eq!(budget.total(), 10 * MIB);
        assert_eq!(budget.take_trim(1), None, "nothing it could free");
        assert_eq!(budget.take_trim(2), Some(7 * MIB));
    }

    #[test]
    fn closing_a_session_or_raising_the_budget_frees_room() {
        let budget = LogBudget::new(10 * MIB);
        budget.register(1);
        budget.register(2);
        budget.report(1, 6 * MIB, 6 * MIB, false);
        budget.unregister(1);
        budget.report(2, 6 * MIB, 6 * MIB, false);
        assert_eq!(budget.take_trim(2), None);
        budget.set_limit(4 * MIB);
        assert_eq!(budget.take_trim(2), Some(4 * MIB));
    }
}
