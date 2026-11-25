//! The front end's queues into the cluster manager (#730): one for [`crate::handle::Command`]s,
//! one for [`crate::handle::Request`]s.
//!
//! Both are unbounded, and this type is the only way to make one, so a send never fails for a
//! full queue and never waits: [`CoreQueue::push`] returns at once from the UI thread, keeps the
//! order items were pushed in, and fails only once the manager has stopped. Nothing a front end
//! sends (a settings change, a consent, a disconnect, a write's gate check) can be lost to a
//! burst of other traffic. The queue's memory is bounded by what the front end sends, which is
//! user actions and the views it opens.

/// Pushed after the manager stopped: the item was dropped, and nothing will answer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CoreStopped;

/// The sending side of an unbounded queue into the manager. Cheap to clone.
pub(crate) struct CoreQueue<T>(async_channel::Sender<T>);

impl<T> std::fmt::Debug for CoreQueue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreQueue")
            .field("queued", &self.0.len())
            .finish()
    }
}

impl<T> Clone for CoreQueue<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> CoreQueue<T> {
    /// A new queue and the manager's end of it.
    pub(crate) fn new() -> (Self, async_channel::Receiver<T>) {
        let (sender, receiver) = async_channel::unbounded();
        (Self(sender), receiver)
    }

    /// Queues `item` behind everything pushed before it. Never blocks; fails only once the
    /// manager has stopped.
    pub(crate) fn push(&self, item: T) -> Result<(), CoreStopped> {
        self.0.try_send(item).map_err(|refused| {
            // Unbounded: the only refusal left is a closed queue.
            debug_assert!(refused.is_closed());
            CoreStopped
        })
    }

    /// [`Self::push`] when the future is first polled, for a sender that should not queue
    /// anything if it is dropped first.
    pub(crate) async fn send(&self, item: T) -> Result<(), CoreStopped> {
        self.push(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flood_is_kept_whole_and_in_order() {
        let (queue, receiver) = CoreQueue::new();
        for n in 0..100_000u32 {
            assert_eq!(queue.push(n), Ok(()));
        }
        let received: Vec<u32> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        assert_eq!(received, (0..100_000).collect::<Vec<_>>());
    }

    #[test]
    fn pushing_after_the_manager_stopped_says_so() {
        let (queue, receiver) = CoreQueue::new();
        drop(receiver);
        assert_eq!(queue.push(1), Err(CoreStopped));
    }

    #[tokio::test]
    async fn send_queues_nothing_until_polled() {
        let (queue, receiver) = CoreQueue::new();
        let pending = queue.send(1);
        queue.push(2).unwrap();
        drop(pending);
        assert_eq!(receiver.try_recv(), Ok(2));
        assert!(receiver.try_recv().is_err());
    }
}
