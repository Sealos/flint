//! How much work `flint mcp` takes on at once (#252). rmcp starts a task for every request and
//! for every reply, and limits neither, so a client that sends many calls while a cluster is
//! slow, or stops reading stdout while it keeps sending, would make the server grow without
//! bound.
//!
//! - **Admission:** at most [`Limits::running`] tool calls run at once, and at most
//!   [`Limits::waiting`] more wait for a turn; a call past both is refused at once with a tool
//!   error saying so ([`Admission::enter`]).
//! - **Reply bytes:** replies handed to the transport but not written yet are counted
//!   ([`Counted`]); while they pass [`Limits::reply_budget`], no call starts its work. A stalled
//!   client therefore holds at most the budget plus what the running calls produce.
//! - **Deadline:** each call ends after [`Limits::deadline`] (`ServerHandler::call_tool`).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmcp::RoleServer;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// The limits of one server.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Tool calls that run at once.
    pub running: usize,
    /// Tool calls that wait for a turn; more are refused.
    pub waiting: usize,
    /// Reply bytes not yet written past which no new call starts.
    pub reply_budget: usize,
    /// How long one tool call may take, from when it starts running.
    pub deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            running: 16,
            waiting: 48,
            reply_budget: 8 * 1024 * 1024,
            deadline: Duration::from_secs(60),
        }
    }
}

/// Who may run a tool call now.
pub(crate) struct Admission {
    limits: Limits,
    turns: Arc<Semaphore>,
    /// Calls admitted: running or waiting.
    admitted: Arc<AtomicUsize>,
    replies: Arc<ReplyBytes>,
}

/// A call refused because [`Limits::running`] plus [`Limits::waiting`] calls are admitted.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Busy {
    pub admitted: usize,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "too many tool calls in flight ({} running or waiting); retry this call when some \
             have finished",
            self.admitted
        )
    }
}

/// A running call's turn: held until the call ends.
pub(crate) struct Turn {
    _permit: OwnedSemaphorePermit,
    _admitted: Admitted,
}

/// One admitted call, counted until dropped.
struct Admitted(Arc<AtomicUsize>);

impl Drop for Admitted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Admission {
    pub(crate) fn new(limits: Limits) -> Self {
        Admission {
            limits,
            turns: Arc::new(Semaphore::new(limits.running)),
            admitted: Arc::default(),
            replies: Arc::new(ReplyBytes::default()),
        }
    }

    pub(crate) fn limits(&self) -> Limits {
        self.limits
    }

    /// The reply byte count the transport keeps ([`Counted`]).
    pub(crate) fn replies(&self) -> Arc<ReplyBytes> {
        self.replies.clone()
    }

    /// Calls running or waiting now.
    #[cfg(test)]
    pub(crate) fn admitted(&self) -> usize {
        self.admitted.load(Ordering::Acquire)
    }

    /// Calls running now.
    #[cfg(test)]
    pub(crate) fn running(&self) -> usize {
        self.limits.running - self.turns.available_permits()
    }

    /// Admits a call, or refuses it at once when too many are admitted. An admitted call waits
    /// for a turn, then until replies not yet written are within the budget. Cancel-safe:
    /// dropping the future gives up the place.
    pub(crate) async fn enter(&self) -> Result<Turn, Busy> {
        let most = self.limits.running + self.limits.waiting;
        let before = self.admitted.fetch_add(1, Ordering::AcqRel);
        if before >= most {
            self.admitted.fetch_sub(1, Ordering::AcqRel);
            return Err(Busy { admitted: before });
        }
        // Counted until the turn ends, or until this future is dropped while it waits.
        let admitted = Admitted(self.admitted.clone());
        let permit = self
            .turns
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        // Checked with the turn held: only running calls are past it at once, so replies not
        // written stay within the budget plus what those produce.
        self.replies.within(self.limits.reply_budget).await;
        Ok(Turn {
            _permit: permit,
            _admitted: admitted,
        })
    }
}

/// Bytes of replies handed to the transport and not written yet.
#[derive(Default)]
pub(crate) struct ReplyBytes {
    outstanding: AtomicUsize,
    released: Notify,
}

impl ReplyBytes {
    pub(crate) fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::Acquire)
    }

    fn hold(self: &Arc<Self>, bytes: usize) -> Held {
        self.outstanding.fetch_add(bytes, Ordering::AcqRel);
        Held {
            replies: self.clone(),
            bytes,
        }
    }

    /// Resolves once at most `budget` bytes are outstanding.
    async fn within(&self, budget: usize) {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if self.outstanding() <= budget {
                return;
            }
            released.await;
        }
    }
}

/// Bytes of one reply, released when it is written (or its send fails or is dropped).
struct Held {
    replies: Arc<ReplyBytes>,
    bytes: usize,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.replies
            .outstanding
            .fetch_sub(self.bytes, Ordering::AcqRel);
        self.replies.released.notify_waiters();
    }
}

/// A transport that counts the bytes of what it sends until each send completes, into
/// [`ReplyBytes`]. Everything else passes through.
pub(crate) struct Counted<T> {
    inner: T,
    replies: Arc<ReplyBytes>,
}

impl<T> Counted<T> {
    pub(crate) fn new(inner: T, replies: Arc<ReplyBytes>) -> Self {
        Counted { inner, replies }
    }
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for Counted<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        // Its size as the transport writes it: one JSON line.
        let bytes = serde_json::to_vec(&item).map_or(0, |line| line.len() + 1);
        let held = self.replies.hold(bytes);
        let send = self.inner.send(item);
        async move {
            let result = send.await;
            drop(held);
            result
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        self.inner.receive()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
