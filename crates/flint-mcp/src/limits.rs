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
//! - **Shutdown:** when the client closes stdin, requests and refusals still on their way finish
//!   as long as replies keep being written. After [`Limits::deadline`] plus [`SHUTDOWN_GRACE`]
//!   with none written, the server stops waiting, so a client that stopped reading stdout cannot
//!   keep the process alive.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use std::collections::{HashMap, VecDeque};

use rmcp::RoleServer;
use rmcp::model::{ClientNotification, ClientRequest, JsonRpcMessage, RequestId};
use rmcp::service::{
    NotificationContext, RequestContext, RxJsonRpcMessage, Service, ServiceRole, TxJsonRpcMessage,
};
use rmcp::transport::Transport;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// How long past [`Limits::deadline`] the end of stdin waits for a reply to be written, and how
/// long closing the transport waits for a write that is stuck.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Bytes of input read ahead, as serialized, past which no more is read while replies pass the
/// budget ([`Counted`]).
const READ_AHEAD_BYTES: usize = 8 * 1024 * 1024;

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
    /// Told of a tool call refused at the transport (it never reaches a handler): its tool, so
    /// it is recorded. Its arguments are unvalidated and are not passed on.
    dropped_call: std::sync::OnceLock<DroppedCall>,
    outstanding: AtomicUsize,
    /// Replies whose send has finished since the start: progress, while stdin has ended.
    written: AtomicUsize,
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
pub(crate) type DroppedCall =
    Box<dyn Fn(String) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

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
            tell(call.params.name.to_string()).await;
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

    /// Whether no request is unanswered and no refusal unwritten.
    fn idle(&self) -> bool {
        self.unanswered_count() == 0 && self.refusing.load(Ordering::Acquire) == 0
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
        self.replies.written.fetch_add(1, Ordering::AcqRel);
        self.replies.released.notify_waiters();
    }
}

/// The wait after stdin ended: it runs out at `until`, moved on whenever a reply is written.
#[derive(Clone, Copy)]
struct Closed {
    written: usize,
    until: tokio::time::Instant,
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
    /// How long after stdin ended the server waits with no reply written.
    quiet: Duration,
    /// Messages read while replies pass the budget and not acted on yet, each with its size, so
    /// the end of stdin is still seen. Kept here because rmcp drops the `receive` future whenever
    /// something else is ready. Reading stops at [`Limits::unanswered`] messages or once
    /// `read_ahead_budget` bytes are held. An empty queue always takes one more message, so one
    /// larger than the budget still proceeds: the stdio transport discards a line over 16 MiB
    /// (rmcp's `DEFAULT_MAX_LINE_LENGTH`), so this holds less than 24 MiB of JSON.
    read_ahead: VecDeque<(RxJsonRpcMessage<RoleServer>, usize)>,
    /// Bytes of the messages in `read_ahead`.
    read_ahead_bytes: usize,
    read_ahead_budget: usize,
    /// Set once stdin has ended.
    closed: Option<Closed>,
}

impl<T> Counted<T> {
    pub(crate) fn new(inner: T, replies: Arc<ReplyBytes>, limits: &Limits) -> Self {
        Counted {
            inner,
            replies,
            budget: limits.reply_budget,
            unanswered: limits.unanswered,
            quiet: limits.deadline + SHUTDOWN_GRACE,
            read_ahead: VecDeque::new(),
            read_ahead_bytes: 0,
            read_ahead_budget: READ_AHEAD_BYTES,
            closed: None,
        }
    }
}

/// Counts what is written to it.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The size of `message` as JSON, without building it in memory.
fn serialized_len(message: &RxJsonRpcMessage<RoleServer>) -> Option<usize> {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, message).ok()?;
    Some(count.0)
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
    /// to other requests included) until it does. A bounded queue of messages is read ahead
    /// meanwhile (`read_ahead`), so the end of stdin is seen unless more than that is waiting.
    /// Otherwise reading goes on, so a cancellation
    /// always gets through. A request read while [`Limits::unanswered`] are not done with is
    /// refused here at once ("busy"), as is one whose id is still in flight; a tool call refused
    /// so is recorded. When the client closes its end, the stream ends once every request read is
    /// answered and every refusal written, or once no reply has been written for
    /// [`Limits::deadline`] plus [`SHUTDOWN_GRACE`] (rmcp waits only briefly for running handlers
    /// after it ends).
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let (replies, budget, most, quiet, ahead_budget) = (
            self.replies.clone(),
            self.budget,
            self.unanswered,
            self.quiet,
            self.read_ahead_budget,
        );
        let (inner, read_ahead, ahead_bytes, closed) = (
            &mut self.inner,
            &mut self.read_ahead,
            &mut self.read_ahead_bytes,
            &mut self.closed,
        );
        async move {
            loop {
                let over = replies.outstanding() > budget;
                let Some((message, size)) = (!over).then(|| read_ahead.pop_front()).flatten()
                else {
                    if let Some(wait) = closed {
                        let written = replies.written.load(Ordering::Acquire);
                        if written != wait.written {
                            *wait = Closed {
                                written,
                                until: tokio::time::Instant::now() + quiet,
                            };
                        }
                        let queued = !read_ahead.is_empty();
                        let ready = replies.until(|| {
                            replies.written.load(Ordering::Acquire) != written
                                || if queued {
                                    replies.outstanding() <= budget
                                } else {
                                    replies.idle()
                                }
                        });
                        if tokio::time::timeout_at(wait.until, ready).await.is_err() {
                            tracing::warn!("stdin closed and no reply was written in time");
                            return None;
                        }
                        if !queued && replies.idle() {
                            return None;
                        }
                    } else if read_ahead.len() >= most || *ahead_bytes >= ahead_budget {
                        replies.within(budget).await;
                    } else {
                        tokio::select! {
                            biased;
                            message = inner.receive() => match message {
                                Some(message) => {
                                    // Unmeasurable counts as the whole budget: reading stops.
                                    let size = serialized_len(&message).unwrap_or(ahead_budget);
                                    *ahead_bytes += size;
                                    read_ahead.push_back((message, size));
                                }
                                None => {
                                    *closed = Some(Closed {
                                        written: replies.written.load(Ordering::Acquire),
                                        until: tokio::time::Instant::now() + quiet,
                                    });
                                }
                            },
                            () = replies.within(budget), if over => {}
                        }
                    }
                    continue;
                };
                *ahead_bytes -= size;
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
