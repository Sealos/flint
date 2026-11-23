//! When background connection attempts may start, and how long a failed one waits.
//!
//! The queue is first in, first out, so a member that keeps failing goes behind the others and
//! none is starved. Time is passed in; the caller asks [`Schedule::next_admission`] when to call
//! [`Schedule::admit`] again.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How many background attempts may hold an admission slot at once.
pub(crate) const MAX_BACKGROUND_ATTEMPTS: usize = 2;

/// The shortest time between two background attempts starting, so a mass reconnect is spread
/// out instead of reaching every API server and credential source at once.
pub(crate) const ATTEMPT_STAGGER: Duration = Duration::from_millis(750);

/// How long an attempt with no result keeps its slot, so a server that never answers cannot
/// block the others. The attempt itself may go on running.
pub(crate) const SLOT_RELEASE_AFTER: Duration = Duration::from_secs(10);

/// The nominal wait after the first failure.
pub(crate) const BACKOFF_INITIAL: Duration = Duration::from_secs(2);

/// The longest wait between retries, jitter included.
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How much of the nominal wait jitter may remove. Jitter only shortens the wait, so it cannot
/// exceed [`BACKOFF_MAX`], and members at the cap still spread out.
pub(crate) const BACKOFF_JITTER_PERCENT: u64 = 25;

/// A source of random values for backoff jitter, injected so tests are deterministic.
pub(crate) trait Jitter {
    /// A value uniform over the whole `u32` range.
    fn sample(&mut self) -> u32;
}

/// The wait before retrying after `failures` consecutive failures (`0` counts as the first):
/// [`BACKOFF_INITIAL`] doubling up to [`BACKOFF_MAX`], less up to
/// [`BACKOFF_JITTER_PERCENT`] percent.
pub(crate) fn backoff_delay(failures: u32, jitter: &mut dyn Jitter) -> Duration {
    let initial = BACKOFF_INITIAL.as_millis() as u64;
    let max = BACKOFF_MAX.as_millis() as u64;
    let doublings = failures.saturating_sub(1).min(max.ilog2());
    let nominal = initial.saturating_mul(1 << doublings).min(max);
    let spread = nominal * BACKOFF_JITTER_PERCENT / 100;
    Duration::from_millis(nominal - u64::from(jitter.sample()) % (spread + 1))
}

/// One connection attempt, so a late result cannot be taken for a newer attempt's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AttemptId(u64);

/// A background attempt the caller may now start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Admission<K> {
    pub key: K,
    pub attempt: AttemptId,
}

#[derive(Debug)]
struct Slot {
    attempt: AttemptId,
    released_at: Instant,
}

/// The queue of members waiting for a background attempt, and the slots in use.
#[derive(Debug)]
pub(crate) struct Schedule<K> {
    queue: VecDeque<K>,
    slots: Vec<Slot>,
    last_start: Option<Instant>,
    next_attempt: u64,
}

impl<K> Default for Schedule<K> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            slots: Vec::new(),
            last_start: None,
            next_attempt: 0,
        }
    }
}

impl<K: Eq> Schedule<K> {
    /// Queues `key` at the back, unless it is already waiting (it keeps its place).
    pub fn enqueue(&mut self, key: K) {
        if !self.queue.contains(&key) {
            self.queue.push_back(key);
        }
    }

    /// Takes `key` out of the queue. A slot it already holds stays in use until its result or
    /// [`SLOT_RELEASE_AFTER`], because the attempt may still be running.
    pub fn dequeue(&mut self, key: &K) {
        self.queue.retain(|queued| queued != key);
    }

    /// The next background attempt, if a slot is free and the stagger has passed. Starts at
    /// most one per call. A clock that reads earlier than the last start admits nothing.
    pub fn admit(&mut self, now: Instant) -> Option<Admission<K>> {
        self.slots.retain(|slot| now < slot.released_at);
        if self.slots.len() >= MAX_BACKGROUND_ATTEMPTS {
            return None;
        }
        if self
            .last_start
            .is_some_and(|last| now.saturating_duration_since(last) < ATTEMPT_STAGGER)
        {
            return None;
        }
        let key = self.queue.pop_front()?;
        let attempt = self.new_attempt();
        self.slots.push(Slot {
            attempt,
            released_at: now + SLOT_RELEASE_AFTER,
        });
        self.last_start = Some(now);
        Some(Admission { key, attempt })
    }

    /// An attempt for the active cluster, which bypasses the queue: it starts now, takes no
    /// slot and does not delay the next background attempt.
    pub fn start_active(&mut self, key: &K) -> AttemptId {
        self.dequeue(key);
        self.new_attempt()
    }

    /// Records the first result of `attempt`, releasing its slot if it still holds one.
    pub fn finish(&mut self, attempt: AttemptId) {
        self.slots.retain(|slot| slot.attempt != attempt);
    }

    /// When [`Self::admit`] can next succeed, or `None` with nothing queued. A result arriving
    /// earlier can only bring this forward.
    pub fn next_admission(&self, now: Instant) -> Option<Instant> {
        if self.queue.is_empty() {
            return None;
        }
        let stagger = self.last_start.map(|last| last + ATTEMPT_STAGGER);
        let live = || self.slots.iter().filter(|slot| now < slot.released_at);
        let slot = (live().count() >= MAX_BACKGROUND_ATTEMPTS)
            .then(|| live().map(|slot| slot.released_at).min())
            .flatten();
        Some(now.max(stagger.unwrap_or(now)).max(slot.unwrap_or(now)))
    }

    /// How many admission slots are held.
    #[cfg(test)]
    pub fn slots_in_use(&self) -> usize {
        self.slots.len()
    }

    fn new_attempt(&mut self) -> AttemptId {
        let attempt = AttemptId(self.next_attempt);
        self.next_attempt += 1;
        attempt
    }
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod tests;
