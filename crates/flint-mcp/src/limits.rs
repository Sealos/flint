//! How much work `flint mcp` takes on at once (#252). rmcp starts a task for every request and
//! for every reply, and limits neither, so a client that sends many calls while a cluster is
//! slow, or stops reading stdout while it keeps sending, would make the server grow without
//! bound.
//!
//! - **Admission:** at most [`Limits::running`] tool calls run at once, and at most
//!   [`Limits::waiting`] more wait for a turn; a call past both is refused at once with a tool
//!   error saying so ([`Admission::enter`]).
//! - **Reply bytes:** replies handed to the transport but not written yet are counted
//!   ([`Counted`]); while they pass [`Limits::reply_budget`], or while [`Limits::unanswered`]
//!   requests are read and not answered, the server reads no more requests, and no admitted call
//!   starts its work. A client that stops reading stdout therefore holds at most about the
//!   budget plus that many replies — not one more per call it keeps sending, refusals and
//!   replies to other requests included.
//! - **Deadline:** each call ends after [`Limits::deadline`] (`ServerHandler::call_tool`).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use std::collections::{HashMap, VecDeque};

use rmcp::RoleServer;
use rmcp::model::{ClientNotification, ClientRequest, JsonRpcMessage, RequestId};
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
    /// Requests read and not answered yet past which no more are read.
    pub unanswered: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            running: 16,
            waiting: 48,
            reply_budget: 8 * 1024 * 1024,
            deadline: Duration::from_secs(60),
            unanswered: 128,
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

/// Bytes of replies handed to the transport and not written yet, and the requests read and not
/// answered yet.
#[derive(Default)]
pub(crate) struct ReplyBytes {
    outstanding: AtomicUsize,
    /// Requests read and not done with yet, and whether each is a tool call.
    unanswered: std::sync::Mutex<HashMap<RequestId, Work>>,
    released: Notify,
}

/// What a request read is, for when it counts as done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    /// A tool call: done when its handler has ended (`ServerHandler::call_tool`), which a
    /// cancellation does not do at once (it may still write its activity record).
    Tool,
    /// Anything else: done when answered, or when cancelled (rmcp then sends nothing).
    Other,
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
        self.until(|| self.outstanding() <= budget).await;
    }

    /// Resolves once `ready` holds, checked again whenever bytes or requests are released.
    async fn until(&self, ready: impl Fn() -> bool) {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if ready() {
                return;
            }
            released.await;
        }
    }

    fn unanswered(&self) -> std::sync::MutexGuard<'_, HashMap<RequestId, Work>> {
        self.unanswered.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Requests read and not done with yet.
    pub(crate) fn unanswered_count(&self) -> usize {
        self.unanswered().len()
    }

    fn done(&self, id: &RequestId, only: Option<Work>) {
        let mut unanswered = self.unanswered();
        let removed = match (unanswered.get(id), only) {
            (Some(work), Some(only)) if *work != only => false,
            (Some(_), _) => unanswered.remove(id).is_some(),
            (None, _) => false,
        };
        drop(unanswered);
        if removed {
            self.released.notify_waiters();
        }
    }

    /// Request `id` is answered.
    fn answered(&self, id: &RequestId) {
        self.done(id, None);
    }

    /// The client cancelled request `id`: done with unless it is a tool call still running.
    fn cancelled(&self, id: &RequestId) {
        self.done(id, Some(Work::Other));
    }

    /// The handler of tool call `id` ended: it holds nothing any more.
    pub(crate) fn handler_ended(&self, id: &RequestId) {
        self.done(id, None);
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
    /// Requests read while [`Limits::unanswered`] were not done with: passed on in order as
    /// those are. Reading goes on meanwhile (a cancellation must get through), up to as many
    /// again.
    deferred: VecDeque<RxJsonRpcMessage<RoleServer>>,
    /// Past this many reply bytes not written ([`Limits::reply_budget`]), or this many
    /// requests not answered ([`Limits::unanswered`]), no more requests are read.
    budget: usize,
    unanswered: usize,
}

impl<T> Counted<T> {
    pub(crate) fn new(inner: T, replies: Arc<ReplyBytes>, limits: &Limits) -> Self {
        Counted {
            inner,
            replies,
            deferred: VecDeque::new(),
            budget: limits.reply_budget,
            unanswered: limits.unanswered,
        }
    }
}

/// The request a reply answers, if it is one.
fn answers(message: &TxJsonRpcMessage<RoleServer>) -> Option<&RequestId> {
    match message {
        JsonRpcMessage::Response(response) => Some(&response.id),
        JsonRpcMessage::Error(error) => error.id.as_ref(),
        _ => None,
    }
}

/// The id of `message`, if it is a request.
fn request_id(message: &RxJsonRpcMessage<RoleServer>) -> Option<&RequestId> {
    match message {
        JsonRpcMessage::Request(request) => Some(&request.id),
        _ => None,
    }
}

fn work(request: &rmcp::model::JsonRpcRequest<ClientRequest>) -> Work {
    match request.request {
        ClientRequest::CallToolRequest(_) => Work::Tool,
        _ => Work::Other,
    }
}

/// Answers a request whose id is still in flight with an error, without passing it to rmcp
/// (which would start a second handler). The reply is counted until written.
fn refuse_duplicate<T: Transport<RoleServer>>(
    inner: &mut T,
    replies: &Arc<ReplyBytes>,
    id: RequestId,
) {
    let error = rmcp::ErrorData::invalid_request(
        "a request with this id is still in flight; use a new id",
        None,
    );
    let reply: TxJsonRpcMessage<RoleServer> = JsonRpcMessage::error(error, Some(id));
    let bytes = serde_json::to_vec(&reply).map_or(0, |line| line.len() + 1);
    let held = replies.hold(bytes);
    let send = inner.send(reply);
    tokio::spawn(async move {
        let _ = send.await;
        drop(held);
    });
}

/// The request a client's `notifications/cancelled` names, if `message` is one.
fn cancels(message: &RxJsonRpcMessage<RoleServer>) -> Option<RequestId> {
    let JsonRpcMessage::Notification(notification) = message else {
        return None;
    };
    let ClientNotification::CancelledNotification(cancelled) = &notification.notification else {
        return None;
    };
    cancelled.params.request_id.clone()
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
        if let Some(id) = answers(&item) {
            self.replies.answered(id);
        }
        let send = self.inner.send(item);
        async move {
            let result = send.await;
            drop(held);
            result
        }
    }

    /// The next message for rmcp. Nothing is read while replies not written pass the budget: a
    /// client that does not read stdout gets nothing more run or answered (refusals and replies
    /// to other requests included) until it does. A request read while [`Limits::unanswered`]
    /// are not done with waits ([`Counted::deferred`]) for one of them; notifications, a
    /// cancellation in particular, pass at once. A request whose id is still in flight is
    /// refused here, its reply counted like any other.
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let (replies, budget, most) = (self.replies.clone(), self.budget, self.unanswered);
        let (inner, deferred) = (&mut self.inner, &mut self.deferred);
        async move {
            loop {
                let free = || replies.unanswered_count() < most;
                if free()
                    && let Some(request) = deferred.pop_front()
                {
                    if let JsonRpcMessage::Request(r) = &request {
                        replies.unanswered().insert(r.id.clone(), work(r));
                    }
                    return Some(request);
                }
                let within = || replies.outstanding() <= budget;
                let reading = within() && deferred.len() < most;
                let message = tokio::select! {
                    message = inner.receive(), if reading => message?,
                    () = replies.until(|| free() && !deferred.is_empty()),
                        if !deferred.is_empty() => continue,
                    () = replies.until(|| within() && deferred.len() < most), if !reading => continue,
                };
                match &message {
                    JsonRpcMessage::Request(request) => {
                        let id = &request.id;
                        let in_flight = replies.unanswered().contains_key(id)
                            || deferred.iter().any(|d| request_id(d) == Some(id));
                        if in_flight {
                            refuse_duplicate(inner, &replies, id.clone());
                        } else if free() {
                            replies.unanswered().insert(id.clone(), work(request));
                            return Some(message);
                        } else {
                            deferred.push_back(message);
                        }
                    }
                    _ => {
                        if let Some(id) = cancels(&message) {
                            // One still waiting here never reaches rmcp.
                            deferred.retain(|d| request_id(d) != Some(&id));
                            replies.cancelled(&id);
                        }
                        return Some(message);
                    }
                }
            }
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
