//! The byte budget of an exec session's input (#256).
//!
//! Input waits in three places before the pod reads it: the queue from the front end to the
//! session task, the input kept for the next shell while the first one may turn out not to
//! exist, and the queue to the writer of the pod's stdin. A pod that stops reading its stdin
//! would let all three grow without end. One budget per session counts every byte held in any
//! of them: the front end reserves a [`Charge`] when it sends input (a non-blocking atomic
//! update, so the UI thread never waits), the charge travels with the bytes, and dropping it
//! gives the bytes back: when the writer has handed them to kube's stdin pipe, or when they
//! are dropped on any path (the session ended, the writer was aborted, a shell was dropped).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How many input bytes a session holds before the command reads them. 1 MiB: room for three
/// of the largest pastes a shell tab sends (256 KiB each, with their paste markers) and for
/// a copy of one kept for the next shell, far more than a shell that reads its input ever
/// leaves waiting (the writer hands bytes on as fast as kube's pipe takes them), and little
/// memory per tab.
pub const INPUT_BUDGET: usize = 1024 * 1024;

/// How far past the budget the terminal's replies to the program (cursor position, device
/// attributes) may go. They are a few bytes each and the program waits for them, so a budget
/// full of typed input must not refuse them; a program that keeps asking without reading its
/// stdin is still bounded.
const REPLY_ROOM: usize = 16 * 1024;

/// Input a session refused because its budget is spent: the command is not reading what it
/// was sent before. Nothing of it was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRefused {
    /// The size of what was refused, in bytes.
    pub bytes: usize,
    /// How many bytes already wait for the command to read them.
    pub queued: usize,
}

impl std::fmt::Display for InputRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} bytes of input refused: {} bytes already wait for the command to read them",
            self.bytes, self.queued
        )
    }
}

impl std::error::Error for InputRefused {}

/// One session's count of held input bytes, shared by its handle and its task.
#[derive(Debug)]
pub(crate) struct InputBudget {
    used: AtomicUsize,
    limit: usize,
}

impl InputBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
        })
    }

    /// How many bytes are held now.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    fn take(&self, bytes: usize, room: usize) -> bool {
        let most = self.limit.saturating_add(room);
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(total) = used.checked_add(bytes).filter(|&total| total <= most) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(now) => used = now,
            }
        }
    }
}

/// Reserves `bytes` of input, or refuses them when they do not fit in the budget. Never blocks.
pub(crate) fn reserve(budget: &Arc<InputBudget>, bytes: usize) -> Result<Charge, InputRefused> {
    if budget.take(bytes, 0) {
        Ok(Charge::new(budget, bytes))
    } else {
        Err(InputRefused {
            bytes,
            queued: budget.used(),
        })
    }
}

/// Reserves `bytes` for a terminal reply, which may go [`REPLY_ROOM`] past the budget.
pub(crate) fn reserve_reply(budget: &Arc<InputBudget>, bytes: usize) -> Option<Charge> {
    budget
        .take(bytes, REPLY_ROOM)
        .then(|| Charge::new(budget, bytes))
}

/// Bytes held against a session's budget. Dropping it gives them back.
#[derive(Debug)]
pub(crate) struct Charge {
    budget: Arc<InputBudget>,
    bytes: usize,
}

impl Charge {
    fn new(budget: &Arc<InputBudget>, bytes: usize) -> Self {
        Self {
            budget: budget.clone(),
            bytes,
        }
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Holds exactly `bytes` from now on: gives back what is no longer needed (a paste reserves
    /// room for its bracketed-paste markers before it knows whether it gets them), or takes
    /// the difference even past the budget (never more than those markers).
    pub fn set(&mut self, bytes: usize) {
        if bytes < self.bytes {
            self.budget
                .used
                .fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        } else {
            self.budget
                .used
                .fetch_add(bytes - self.bytes, Ordering::AcqRel);
        }
        self.bytes = bytes;
    }

    /// Takes over `other`'s bytes (of the same budget).
    pub fn absorb(&mut self, mut other: Charge) {
        debug_assert!(Arc::ptr_eq(&self.budget, &other.budget));
        self.bytes += std::mem::take(&mut other.bytes);
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_fit_the_budget_and_come_back_when_dropped() {
        let budget = InputBudget::new(10);
        let a = reserve(&budget, 6).unwrap();
        assert_eq!(
            reserve(&budget, 5).unwrap_err(),
            InputRefused {
                bytes: 5,
                queued: 6
            }
        );
        let b = reserve(&budget, 4).unwrap();
        assert_eq!(budget.used(), 10);
        drop(a);
        assert_eq!(budget.used(), 4);
        drop(b);
        assert_eq!(budget.used(), 0);
        // Nothing is ever too small to fit.
        let budget = InputBudget::new(10);
        let _full = reserve(&budget, 10).unwrap();
        assert!(reserve(&budget, 0).is_ok());
        assert!(reserve(&budget, 1).is_err());
    }

    #[test]
    fn replies_go_a_little_past_the_budget() {
        let budget = InputBudget::new(10);
        let _full = reserve(&budget, 10).unwrap();
        let reply = reserve_reply(&budget, 6).expect("room for a reply");
        assert_eq!(budget.used(), 16);
        assert!(reserve_reply(&budget, REPLY_ROOM).is_none(), "bounded");
        drop(reply);
        assert_eq!(budget.used(), 10);
    }

    #[test]
    fn charges_shrink_grow_and_merge() {
        let budget = InputBudget::new(100);
        let mut a = reserve(&budget, 20).unwrap();
        a.set(8);
        assert_eq!(budget.used(), 8);
        a.set(12);
        assert_eq!(budget.used(), 12);
        let b = reserve(&budget, 30).unwrap();
        a.absorb(b);
        assert_eq!((a.bytes(), budget.used()), (42, 42));
        drop(a);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn refusals_say_how_much_waits() {
        let refused = InputRefused {
            bytes: 3,
            queued: 1024,
        };
        assert_eq!(
            refused.to_string(),
            "3 bytes of input refused: 1024 bytes already wait for the command to read them"
        );
    }
}
