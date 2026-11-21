//! How much work `flint mcp` takes on at once (#252). rmcp starts a task for every request and
//! for every reply, and limits neither, so a client that sends many calls while a cluster is
//! slow, or stops reading stdout while it keeps sending, would make the server grow without
//! bound.
//!
//! - **Admission:** at most [`Limits::running`] tool calls run at once, and at most
//!   [`Limits::waiting`] more wait for a turn; a call past both is refused at once with a tool
//!   error saying so ([`Admission::enter`]).
//! - **Reply bytes:** replies handed to the transport but not written yet are counted
//!   ([`Counted`]); while they pass [`Limits::reply_budget`], the server reads no more requests,
//!   and no admitted call starts its work. A request read while [`Limits::unanswered`] are not
//!   answered is refused at once. A client that stops reading stdout therefore holds at most
//!   about the budget plus that many replies — not one more per call it keeps sending, refusals
//!   and replies to other requests included.
//! - **Deadline:** each call ends after [`Limits::deadline`] (`ServerHandler::call_tool`).
//! - **Shutdown:** when the client closes stdin, requests and refusals still on their way are
//!   given until [`Limits::deadline`] plus [`SHUTDOWN_GRACE`], so a client that stopped reading
//!   stdout cannot keep the process alive.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use std::collections::HashMap;

use rmcp::RoleServer;
use rmcp::model::{ClientNotification, ClientRequest, JsonRpcMessage, RequestId};
use rmcp::service::{
    NotificationContext, RequestContext, RxJsonRpcMessage, Service, ServiceRole, TxJsonRpcMessage,
};
use rmcp::transport::Transport;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// How long past [`Limits::deadline`] the end of stdin waits for work still on its way, and how
/// long closing the transport waits for a write that is stuck.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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
    #[cfg(test)]
    entered: AtomicUsize,
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
            #[cfg(test)]
            entered: AtomicUsize::new(0),
        }
    }

    pub(crate) fn limits(&self) -> Limits {
        self.limits
    }

    /// The reply byte count the transport keeps ([`Counted`]).
    pub(crate) fn replies(&self) -> Arc<ReplyBytes> {
        self.replies.clone()
    }

    /// Calls admitted since the start.
    #[cfg(test)]
    pub(crate) fn entered(&self) -> usize {
        self.entered.load(Ordering::Acquire)
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
        #[cfg(test)]
        self.entered.fetch_add(1, Ordering::AcqRel);
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
    /// Told of a tool call refused at the transport (it never reaches a handler): its tool and
    /// arguments, so it is recorded.
    dropped_call: std::sync::OnceLock<DroppedCall>,
    outstanding: AtomicUsize,
    /// Requests read and not done with yet, and whether each is a tool call.
    unanswered: std::sync::Mutex<HashMap<RequestId, Work>>,
    /// Refusals handed to the transport and not written yet ([`refuse`]).
    refusing: AtomicUsize,
    released: Notify,
}

/// A request read and not done with yet. It is done when its reply reaches the transport
/// ([`Counted`]'s `send`, which counts the reply's bytes from then on), or, once cancelled (rmcp
/// then sends no reply), when nothing works on it any more: at once for most requests, when its
/// handler has ended for a tool call ([`Tracked`]), which may still be writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Work {
    tool: bool,
    /// The tool call's handler has ended (its reply may still be on its way to the transport).
    ended: bool,
    cancelled: bool,
}

impl Work {
    /// Whether nothing will hold anything for it any more.
    fn done(self) -> bool {
        self.cancelled && (!self.tool || self.ended)
    }
}

/// What [`ReplyBytes`] tells of a dropped tool call.
pub(crate) type DroppedCall = Box<
    dyn Fn(
            String,
            Option<serde_json::Map<String, serde_json::Value>>,
        ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

impl ReplyBytes {
    /// Sets what is told of a dropped tool call; only the first one set counts.
    pub(crate) fn on_dropped_call(&self, tell: DroppedCall) {
        let _ = self.dropped_call.set(tell);
    }

    /// Tells of a tool call refused at the transport, which never reaches a handler, so it is
    /// recorded; awaited, so a slow activity log slows reading.
    async fn dropped(&self, message: &RxJsonRpcMessage<RoleServer>) {
        if let (JsonRpcMessage::Request(request), Some(tell)) = (message, self.dropped_call.get())
            && let ClientRequest::CallToolRequest(call) = &request.request
        {
            tell(call.params.name.to_string(), call.params.arguments.clone()).await;
        }
    }

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

    /// Updates request `id` with `change`, and lets it go once it is done ([`Work`]).
    fn update(&self, id: &RequestId, change: impl FnOnce(&mut Work), answered: bool) {
        let mut unanswered = self.unanswered();
        let removed = match unanswered.get_mut(id) {
            Some(work) => {
                change(work);
                (answered || work.done()) && unanswered.remove(id).is_some()
            }
            None => false,
        };
        drop(unanswered);
        if removed {
            self.released.notify_waiters();
        }
    }

    /// The reply to request `id` reached the transport: its bytes are counted from now on.
    fn answered(&self, id: &RequestId) {
        self.update(id, |_| {}, true);
    }

    /// The client cancelled request `id`: rmcp will send no reply.
    fn cancelled(&self, id: &RequestId) {
        self.update(id, |work| work.cancelled = true, false);
    }

    /// The handler of tool call `id` ended. Its reply, if it is not cancelled, is still on its way
    /// to the transport and counts until it arrives; `failed` (it panicked) sends none.
    fn handler_ended(&self, id: &RequestId, failed: bool) {
        self.update(
            id,
            |work| {
                work.ended = true;
                work.cancelled |= failed;
            },
            false,
        );
    }
}

/// Marks a tool call's handler as ended when dropped ([`ReplyBytes::handler_ended`]).
struct HandlerEnded {
    replies: Arc<ReplyBytes>,
    id: RequestId,
}

impl Drop for HandlerEnded {
    fn drop(&mut self) {
        // A handler that panicked sends no reply: nothing else would let its request go.
        self.replies
            .handler_ended(&self.id, std::thread::panicking());
    }
}

/// `service`, telling [`ReplyBytes`] when each tool call's handler ends. It wraps the whole
/// handler rather than `call_tool`, because rmcp can reject a call before `call_tool` (an
/// unsupported `_meta.protocolVersion`, say): if the client's cancellation was read first, rmcp
/// drops that error, and nothing else would let the request go.
pub(crate) struct Tracked<S> {
    service: S,
    replies: Arc<ReplyBytes>,
}

impl<S> Tracked<S> {
    pub(crate) fn new(service: S, replies: Arc<ReplyBytes>) -> Self {
        Tracked { service, replies }
    }
}

impl<S: Service<RoleServer>> Service<RoleServer> for Tracked<S> {
    async fn handle_request(
        &self,
        request: ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<<RoleServer as ServiceRole>::Resp, rmcp::ErrorData> {
        // Until this handler ends, the call counts as unanswered, cancelled or not: it may still
        // be writing its activity record.
        let _ended = matches!(request, ClientRequest::CallToolRequest(_)).then(|| HandlerEnded {
            replies: self.replies.clone(),
            id: context.id.clone(),
        });
        self.service.handle_request(request, context).await
    }

    async fn handle_notification(
        &self,
        notification: ClientNotification,
        context: NotificationContext<RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.service
            .handle_notification(notification, context)
            .await
    }

    fn get_info(&self) -> <RoleServer as ServiceRole>::Info {
        self.service.get_info()
    }

    fn supported_protocol_versions(
        &self,
    ) -> std::borrow::Cow<'static, [rmcp::model::ProtocolVersion]> {
        self.service.supported_protocol_versions()
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

/// One refusal not written yet, counted until dropped.
struct Refusing(Arc<ReplyBytes>);

impl Drop for Refusing {
    fn drop(&mut self) {
        self.0.refusing.fetch_sub(1, Ordering::AcqRel);
        self.0.released.notify_waiters();
    }
}

/// A transport that counts the bytes of what it sends until each send completes, into
/// [`ReplyBytes`]. Everything else passes through.
pub(crate) struct Counted<T> {
    inner: T,
    replies: Arc<ReplyBytes>,
    /// Past this many reply bytes not written ([`Limits::reply_budget`]), or this many
    /// requests not answered ([`Limits::unanswered`]), no more requests are read.
    budget: usize,
    unanswered: usize,
    /// How long the end of stdin waits for work still on its way.
    drain: Duration,
    /// One message read while replies pass the budget, so the end of stdin is still seen. Kept
    /// here because rmcp drops the `receive` future whenever something else is ready.
    read_ahead: Option<RxJsonRpcMessage<RoleServer>>,
    /// When the wait after the end of stdin runs out, once stdin has ended.
    closed: Option<tokio::time::Instant>,
}

impl<T> Counted<T> {
    pub(crate) fn new(inner: T, replies: Arc<ReplyBytes>, limits: &Limits) -> Self {
        Counted {
            inner,
            replies,
            budget: limits.reply_budget,
            unanswered: limits.unanswered,
            drain: limits.deadline + SHUTDOWN_GRACE,
            read_ahead: None,
            closed: None,
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

fn work(request: &rmcp::model::JsonRpcRequest<ClientRequest>) -> Work {
    Work {
        tool: matches!(request.request, ClientRequest::CallToolRequest(_)),
        ..Work::default()
    }
}

/// Why a request is refused at the transport.
const DUPLICATE: &str = "a request with this id is still in flight; use a new id";
const BUSY: &str = "too many requests in flight; retry when some have been answered";

/// Answers a request with an error, without passing it to rmcp: one whose id is still in
/// flight (rmcp would start a second handler), or one past what may wait. The reply is counted
/// until written.
fn refuse<T: Transport<RoleServer>>(
    inner: &mut T,
    replies: &Arc<ReplyBytes>,
    id: RequestId,
    why: &'static str,
) {
    let error = rmcp::ErrorData::invalid_request(why, None);
    let reply: TxJsonRpcMessage<RoleServer> = JsonRpcMessage::error(error, Some(id));
    let bytes = serde_json::to_vec(&reply).map_or(0, |line| line.len() + 1);
    let held = replies.hold(bytes);
    replies.refusing.fetch_add(1, Ordering::AcqRel);
    let refusing = Refusing(replies.clone());
    let send = inner.send(reply);
    tokio::spawn(async move {
        let _ = send.await;
        drop(held);
        drop(refusing);
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

    /// The next message for rmcp. Nothing is acted on while replies not written pass the budget:
    /// a client that does not read stdout gets nothing more run or answered (refusals and replies
    /// to other requests included) until it does. One message is read ahead meanwhile, so the end
    /// of stdin is seen. Otherwise reading goes on, so a cancellation always gets through. A
    /// request read while [`Limits::unanswered`] are not done with is refused here at once
    /// ("busy"), as is one whose id is still in flight; a tool call refused so is recorded. When
    /// the client closes its end, the stream ends once every request read is answered and every
    /// refusal written, or when the wait for them runs out (rmcp waits only briefly for running
    /// handlers after it ends).
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let (replies, budget, most, drain) = (
            self.replies.clone(),
            self.budget,
            self.unanswered,
            self.drain,
        );
        let (inner, read_ahead, closed) = (&mut self.inner, &mut self.read_ahead, &mut self.closed);
        async move {
            loop {
                if let Some(until) = *closed {
                    let drained = replies.until(|| {
                        replies.unanswered_count() == 0
                            && replies.refusing.load(Ordering::Acquire) == 0
                    });
                    if tokio::time::timeout_at(until, drained).await.is_err() {
                        tracing::warn!("stdin closed with requests or refusals still unfinished");
                    }
                    return None;
                }
                if read_ahead.is_none() {
                    match inner.receive().await {
                        Some(message) => *read_ahead = Some(message),
                        None => {
                            *closed = Some(tokio::time::Instant::now() + drain);
                            continue;
                        }
                    }
                }
                replies.within(budget).await;
                let Some(message) = read_ahead.take() else {
                    continue;
                };
                match &message {
                    JsonRpcMessage::Request(request) => {
                        let id = &request.id;
                        if replies.unanswered().contains_key(id) {
                            refuse(inner, &replies, id.clone(), DUPLICATE);
                            replies.dropped(&message).await;
                        } else if replies.unanswered_count() >= most {
                            refuse(inner, &replies, id.clone(), BUSY);
                            replies.dropped(&message).await;
                        } else {
                            replies.unanswered().insert(id.clone(), work(request));
                            return Some(message);
                        }
                    }
                    _ => {
                        if let Some(id) = cancels(&message) {
                            replies.cancelled(&id);
                        }
                        return Some(message);
                    }
                }
            }
        }
    }

    /// Closes the transport, without waiting for long on a write a stalled client holds up.
    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let close = self.inner.close();
        async move {
            tokio::time::timeout(SHUTDOWN_GRACE, close)
                .await
                .unwrap_or(Ok(()))
        }
    }
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
