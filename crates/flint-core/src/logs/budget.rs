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
//! A session takes a trim's target ([`LogBudget::begin_trim`]) and detaches one block at a time
//! under the budget's lock ([`LogBudget::while_planned`]), having checked the tab is still hidden
//! and the plan still current. So no block goes after [`LogBudget::set_shown`] returned, and the
//! UI thread waits for at most one constant-time detach; lines are freed outside the lock.
//!
//! The total stays within the budget plus what sessions take in between two reports (a block
//! per session, and up to 100 ms of lines), and the shown tab's own cap when it alone is over.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// How many reports sessions made.
    #[cfg(test)]
    reports: usize,
}

#[derive(Debug)]
struct Entry {
    /// What the session's buffer held at its last report (measured, never projected).
    bytes: usize,
    /// How much of it the session can give back: its sealed blocks (the open tail block
    /// stays). Asking for more would free nothing and skip sessions that could.
    trimmable: usize,
    /// `clock` when it was last shown, or opened.
    viewed: u64,
    shown: bool,
    paused: bool,
    /// Bytes the session should trim its buffer down to, as the current plan has it. Planned
    /// again from the measured sizes on every change ([`Inner::rebalance`]), so a tab shown
    /// since, or resumed, is never left with an old target.
    trim_to: Option<usize>,
    /// The target of the trim the session took and is carrying out, so a new plan can revoke it.
    in_flight: Option<usize>,
    /// Advances when what the session may give back changes: shown, hidden, paused, resumed, or
    /// planned again. A trim under way stops when it moves.
    plan: u64,
    /// The session's oldest line after its latest trim, recorded in the step that detached it.
    /// Atomic so a frame can read it without waiting on this budget's lock.
    cutoff: Arc<AtomicU64>,
    wake: Arc<Notify>,
}

/// A trim a session took: down to `to` bytes, while plan `plan` stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrimPlan {
    pub to: usize,
    plan: u64,
}

impl LogBudget {
    /// A budget of `limit` bytes for all sessions together.
    pub fn new(limit: usize) -> Arc<LogBudget> {
        Arc::new(LogBudget {
            inner: Mutex::new(Inner {
                limit,
                clock: 0,
                sessions: HashMap::new(),
                #[cfg(test)]
                reports: 0,
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
    /// ([`LogBudget::begin_trim`]). A new session counts as just viewed.
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
                in_flight: None,
                plan: 0,
                cutoff: Arc::default(),
                wake: wake.clone(),
            },
        );
        wake
    }

    /// Session `id` ended: its bytes no longer count, and the trims asked of the others are
    /// planned again without them.
    pub(crate) fn unregister(&self, id: u64) {
        let mut inner = self.lock();
        if inner.sessions.remove(&id).is_some() {
            inner.rebalance();
        }
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
            entry.plan += 1;
            inner.rebalance();
        }
    }

    /// Where session `id`'s cutoff is published: the first line it still holds after its trims.
    pub(crate) fn cutoff_of(&self, id: u64) -> Arc<AtomicU64> {
        self.lock()
            .sessions
            .get(&id)
            .map_or_else(Arc::default, |e| e.cutoff.clone())
    }

    #[cfg(test)]
    pub(crate) fn reports(&self) -> usize {
        self.lock().reports
    }

    /// Whether session `id`'s tab is the one shown.
    pub fn is_shown(&self, id: u64) -> bool {
        self.lock().sessions.get(&id).is_some_and(|e| e.shown)
    }

    /// Session `id` was paused or resumed: paused tabs are trimmed after idle ones, at once.
    pub(crate) fn set_paused(&self, id: u64, paused: bool) {
        let mut inner = self.lock();
        if let Some(entry) = inner.sessions.get_mut(&id)
            && entry.paused != paused
        {
            entry.paused = paused;
            entry.plan += 1;
            inner.rebalance();
        }
    }

    /// Session `id`'s buffer holds `bytes` now, `trimmable` of them in sealed blocks, and it
    /// is or is not paused.
    pub(crate) fn report(&self, id: u64, bytes: usize, trimmable: usize, paused: bool) {
        let mut inner = self.lock();
        #[cfg(test)]
        {
            inner.reports += 1;
        }
        if let Some(entry) = inner.sessions.get_mut(&id) {
            entry.bytes = bytes;
            entry.trimmable = trimmable;
            entry.paused = paused;
            inner.rebalance();
        }
    }

    /// Session `id` finished a trim and holds `bytes` now, `trimmable` of them in sealed blocks.
    /// Recorded without planning again: what a session cannot give back would otherwise have it
    /// asked again at once, in a loop. The next change plans again. A target planned while it
    /// trimmed stays.
    pub(crate) fn trimmed(&self, id: u64, bytes: usize, trimmable: usize) {
        if let Some(entry) = self.lock().sessions.get_mut(&id) {
            entry.bytes = bytes;
            entry.trimmable = trimmable;
            entry.in_flight = None;
        }
    }

    /// What session `id` should trim its buffer down to, if anything; asked once.
    pub(crate) fn begin_trim(&self, id: u64) -> Option<TrimPlan> {
        let mut inner = self.lock();
        let entry = inner.sessions.get_mut(&id)?;
        let to = entry.trim_to.take()?;
        entry.in_flight = Some(to);
        Some(TrimPlan {
            to,
            plan: entry.plan,
        })
    }

    /// Runs `detach` under the budget's lock if session `id`'s tab is still hidden and `plan`
    /// still stands, and counts the bytes it returns as gone in the same step, so a plan made
    /// right after does not ask another tab for them. `detach` sets the new cutoff it is handed
    /// ([`crate::LogSession::trim_cutoff`]). It must take constant time: the UI thread can wait
    /// on this lock.
    pub(crate) fn while_planned<T>(
        &self,
        id: u64,
        plan: TrimPlan,
        detach: impl FnOnce(&mut u64) -> Option<(T, usize)>,
    ) -> Option<T> {
        let mut inner = self.lock();
        let entry = inner.sessions.get_mut(&id)?;
        if entry.shown || entry.plan != plan.plan {
            return None;
        }
        let mut cutoff = entry.cutoff.load(Ordering::Relaxed);
        let (detached, freed) = detach(&mut cutoff)?;
        entry.cutoff.store(cutoff, Ordering::Release);
        entry.bytes = entry.bytes.saturating_sub(freed);
        entry.trimmable = entry.trimmable.saturating_sub(freed);
        Some(detached)
    }

    #[cfg(test)]
    fn take_trim(&self, id: u64) -> Option<usize> {
        self.begin_trim(id).map(|plan| plan.to)
    }

    /// Bytes all sessions hold as planned: what they reported, less the trims asked of them.
    pub fn total(&self) -> usize {
        self.lock()
            .sessions
            .values()
            .map(|e| {
                e.trim_to
                    .or(e.in_flight)
                    .map_or(e.bytes, |t| t.min(e.bytes))
            })
            .sum()
    }
}

impl Inner {
    /// Plans, from the sizes the sessions last measured, which ones give bytes back while
    /// over the limit: idle ones least recently viewed first, then paused ones, never the
    /// shown one, each at most what its sealed blocks hold. Every pending trim is planned again,
    /// so one asked of a tab that is shown or resumed since is withdrawn, and nothing counts as
    /// freed before it is. Sessions whose target changed are woken.
    fn rebalance(&mut self) {
        let before: HashMap<u64, Option<usize>> = self
            .sessions
            .iter_mut()
            .map(|(id, e)| (*id, e.trim_to.take()))
            .collect();
        let total: usize = self.sessions.values().map(|e| e.bytes).sum();
        let mut excess = total.saturating_sub(self.limit);
        if excess > 0 {
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
                entry.trim_to = Some(entry.bytes - cut);
                excess -= cut;
            }
        }
        for (id, entry) in &mut self.sessions {
            // A trim under way that the new plan still wants carries on; any other is revoked.
            if entry.in_flight.is_some() {
                if entry.trim_to == entry.in_flight {
                    entry.trim_to = None;
                    continue;
                }
                entry.in_flight = None;
                entry.plan += 1;
            }
            let was = before.get(id).copied().flatten();
            if was != entry.trim_to {
                entry.plan += 1;
            }
            if entry.trim_to.is_some() && was != entry.trim_to {
                entry.wake.notify_one();
            }
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
        assert_eq!(budget.total(), 100 * MIB, "as planned");
        assert_eq!(budget.take_trim(1), Some(10 * MIB));
        assert_eq!(budget.take_trim(2), None);
        assert_eq!(budget.take_trim(3), None, "the shown tab keeps its own cap");
        assert_eq!(budget.take_trim(1), None, "asked once");
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
        // A report before 2 has trimmed keeps the same plan, and asks no one else.
        budget.report(2, 9 * MIB, 8 * MIB, false);
        assert_eq!(budget.total(), 10 * MIB);
        assert_eq!(budget.take_trim(1), None, "nothing it could free");
        assert_eq!(budget.take_trim(2), Some(7 * MIB));
    }

    // covers UP-42: a tab shown, or resumed, after a trim was planned for it keeps its lines,
    // and the bytes are asked of others instead.
    #[test]
    fn a_trim_planned_for_a_tab_shown_since_is_withdrawn() {
        let budget = LogBudget::new(10 * MIB);
        for id in 1..=3 {
            budget.register(id);
        }
        budget.set_shown(3, true);
        budget.report(1, 6 * MIB, 6 * MIB, false);
        budget.report(2, 6 * MIB, 6 * MIB, false);
        budget.report(3, 2 * MIB, 2 * MIB, false);
        // 4 MiB over: planned for 1, the least recently viewed, before it has trimmed.
        assert_eq!(budget.total(), 10 * MIB);
        // 1 is shown now: its trim is withdrawn and asked of 2.
        budget.set_shown(3, false);
        budget.set_shown(1, true);
        assert_eq!(budget.total(), 10 * MIB, "as planned");
        assert_eq!(budget.take_trim(1), None, "the tab shown keeps its lines");
        assert_eq!(budget.take_trim(2), Some(2 * MIB));
    }

    // covers UP-42: a target the session already took stops counting once its tab is shown.
    #[test]
    fn a_trim_under_way_stops_when_the_tab_is_shown() {
        let budget = LogBudget::new(10 * MIB);
        budget.register(1);
        budget.register(2);
        budget.set_shown(2, true);
        budget.report(1, 20 * MIB, 20 * MIB, false);
        let plan = budget.begin_trim(1).expect("over the budget");
        assert_eq!(plan.to, 10 * MIB);
        assert_eq!(
            budget.while_planned(1, plan, |_| Some(("detached", MIB))),
            Some("detached")
        );
        budget.set_shown(1, true);
        let late = budget.while_planned(1, plan, |_| -> Option<((), usize)> {
            panic!("a block was detached from a tab shown")
        });
        assert_eq!(late, None);
    }

    // covers UP-42: hidden again, a trim taken before it was shown does not come back to life.
    #[test]
    fn a_trim_taken_before_the_tab_was_shown_stays_abandoned() {
        let budget = LogBudget::new(10 * MIB);
        budget.register(1);
        budget.report(1, 20 * MIB, 20 * MIB, false);
        let plan = budget.begin_trim(1).unwrap();
        budget.set_shown(1, true);
        budget.set_shown(1, false);
        assert_eq!(budget.while_planned(1, plan, |_| Some(((), 0))), None);
    }

    // covers UP-42: a trim under way stops once the budget no longer needs it.
    #[test]
    fn closing_a_tab_revokes_a_trim_under_way() {
        let budget = LogBudget::new(10 * MIB);
        budget.register(1);
        budget.register(2);
        budget.report(1, 6 * MIB, 6 * MIB, false);
        budget.report(2, 6 * MIB, 6 * MIB, false);
        let plan = budget.begin_trim(1).expect("2 MiB over");
        budget.unregister(2);
        assert_eq!(budget.while_planned(1, plan, |_| Some(((), MIB))), None);
    }

    // covers UP-42: a report that keeps the same plan does not stop a trim under way.
    #[test]
    fn a_plan_that_still_stands_lets_a_trim_carry_on() {
        let budget = LogBudget::new(10 * MIB);
        budget.register(1);
        budget.report(1, 20 * MIB, 20 * MIB, false);
        let plan = budget.begin_trim(1).unwrap();
        budget.report(1, 20 * MIB, 20 * MIB, false);
        assert_eq!(budget.while_planned(1, plan, |_| Some(((), MIB))), Some(()));
        assert_eq!(budget.begin_trim(1), None, "not asked twice");
    }

    // covers UP-42: what a trim detached counts at once, so showing the tab right after does not
    // ask another tab for the same bytes.
    #[test]
    fn detached_bytes_are_not_asked_of_another_tab() {
        let budget = LogBudget::new(10 * MIB);
        budget.register(1);
        budget.register(2);
        budget.report(1, 8 * MIB, 8 * MIB, false);
        budget.report(2, 4 * MIB, 4 * MIB, false);
        let plan = budget.begin_trim(1).expect("2 MiB over");
        assert_eq!(plan.to, 6 * MIB);
        assert_eq!(
            budget.while_planned(1, plan, |_| Some(((), 2 * MIB))),
            Some(())
        );
        budget.set_shown(1, true);
        assert_eq!(budget.take_trim(2), None, "already within the budget");
    }

    // covers UP-42: pausing counts at once, not with the next snapshot.
    #[test]
    fn pausing_or_resuming_a_tab_plans_again_at_once() {
        let budget = LogBudget::new(10 * MIB);
        for id in 1..=2 {
            budget.register(id);
        }
        budget.report(1, 6 * MIB, 6 * MIB, false);
        budget.report(2, 6 * MIB, 6 * MIB, false);
        // 2 MiB over: from 1, the least recently viewed.
        budget.set_paused(1, true);
        // Paused now: 2 gives instead, and 1 is not asked.
        assert_eq!(budget.take_trim(1), None);
        assert_eq!(budget.take_trim(2), Some(4 * MIB));
        // Resumed: 1 is first again.
        budget.trimmed(2, 6 * MIB, 6 * MIB);
        budget.set_paused(1, false);
        assert_eq!(budget.take_trim(1), Some(4 * MIB));
    }

    // covers UP-42: closing a tab plans again, so a trim it made necessary is withdrawn (#296).
    #[test]
    fn closing_a_tab_withdraws_trims_no_longer_needed() {
        let budget = LogBudget::new(10 * MIB);
        for id in 1..=2 {
            budget.register(id);
        }
        budget.report(1, 6 * MIB, 6 * MIB, false);
        budget.report(2, 6 * MIB, 6 * MIB, false);
        budget.unregister(2);
        assert_eq!(budget.take_trim(1), None, "6 MiB fits once 2 is closed");
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
