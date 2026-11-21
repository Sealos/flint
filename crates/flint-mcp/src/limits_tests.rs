use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use super::*;

fn limits(running: usize, waiting: usize) -> Limits {
    Limits {
        running,
        waiting,
        reply_budget: 1000,
        deadline: Duration::from_secs(60),
        unanswered: 8,
    }
}

/// Polls `future` once: its output if it is ready.
fn poll_once<F: Future>(future: &mut Pin<Box<F>>) -> Option<F::Output> {
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    }
}

// covers UP-08: calls past the running limit wait for a turn; past the waiting limit too, they
// are refused at once; a call that stops waiting gives its place back.
#[tokio::test]
async fn calls_run_wait_or_are_refused() {
    let admission = Admission::new(limits(2, 1));
    let first = admission.enter().await.unwrap();
    let _second = admission.enter().await.unwrap();
    assert_eq!(admission.running(), 2);
    let mut third = Box::pin(admission.enter());
    assert!(poll_once(&mut third).is_none(), "the third waits");
    assert_eq!(admission.admitted(), 3);
    let refused = poll_once(&mut Box::pin(admission.enter())).unwrap();
    assert_eq!(refused.err(), Some(Busy { admitted: 3 }));
    drop(first);
    let _third = third.await.expect("the third runs once a turn is free");
    assert_eq!(admission.running(), 2);

    let mut waiting = Box::pin(admission.enter());
    assert!(poll_once(&mut waiting).is_none());
    assert_eq!(admission.admitted(), 3);
    drop(waiting);
    assert_eq!(admission.admitted(), 2, "a call that stops waiting leaves");
}

// covers UP-16: a cancellation already signalled beats a free turn, so the call is not admitted
// and its unvalidated arguments are never read for its record.
#[tokio::test]
async fn a_cancelled_call_is_not_admitted_even_with_a_turn_free() {
    let admission = Admission::new(limits(1, 1));
    for _ in 0..64 {
        let entered = admission.enter_unless(std::future::ready(())).await;
        assert!(entered.is_none());
        assert_eq!(admission.admitted(), 0);
        assert_eq!(admission.running(), 0);
    }
    let turn = admission.enter_unless(std::future::pending()).await;
    assert!(matches!(turn, Some(Ok(_))), "not cancelled: it runs");
}

// covers UP-08: while replies not yet written pass the budget, no call starts.
#[tokio::test]
async fn no_call_starts_while_replies_pass_the_budget() {
    let admission = Admission::new(limits(4, 4));
    let held = admission.replies().hold(1001);
    let mut next = Box::pin(admission.enter());
    assert!(poll_once(&mut next).is_none(), "over the budget");
    // It holds its turn while it waits: no more than the running calls get past the check.
    assert_eq!(admission.running(), 1);
    drop(held);
    assert!(next.await.is_ok(), "within the budget again");
}

/// A transport whose sends finish when the test says so.
struct Gated {
    gates: Vec<tokio::sync::oneshot::Sender<()>>,
    /// What `receive` returns, then the end of the stream.
    incoming: std::collections::VecDeque<RxJsonRpcMessage<RoleServer>>,
}

impl Gated {
    fn new(incoming: impl IntoIterator<Item = RxJsonRpcMessage<RoleServer>>) -> Self {
        Gated {
            gates: Vec::new(),
            incoming: incoming.into_iter().collect(),
        }
    }
}

/// A `tools/call` with this id, as read from the client.
fn tool_call(id: u64) -> RxJsonRpcMessage<RoleServer> {
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": "list_clusters", "arguments": {}},
    }))
    .unwrap()
}

impl Transport<RoleServer> for Gated {
    type Error = std::io::Error;

    fn send(
        &mut self,
        _item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let (gate, open) = tokio::sync::oneshot::channel();
        self.gates.push(gate);
        async move {
            let _ = open.await;
            Ok(())
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        std::future::ready(self.incoming.pop_front())
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

// covers UP-08: a reply's bytes count from when it is handed over until it is written.
#[tokio::test]
async fn reply_bytes_count_until_written() {
    let replies = Arc::new(ReplyBytes::default());
    let mut transport = Counted::new(Gated::new([]), replies.clone(), &limits(1, 1));
    let reply: TxJsonRpcMessage<RoleServer> = rmcp::model::JsonRpcMessage::response(
        rmcp::model::ServerResult::empty(()),
        rmcp::model::NumberOrString::Number(7),
    );
    let line = serde_json::to_vec(&reply).unwrap().len() + 1;
    let sending = tokio::spawn(transport.send(reply.clone()));
    let second = transport.send(reply);
    assert_eq!(replies.outstanding(), 2 * line);
    drop(second);
    assert_eq!(
        replies.outstanding(),
        line,
        "a dropped send releases its bytes"
    );
    transport.inner.gates.remove(0).send(()).unwrap();
    sending.await.unwrap().unwrap();
    assert_eq!(replies.outstanding(), 0);
}

// covers UP-08: when stdin ends, the stream ends only after a refusal already handed over is
// written, even with every request answered.
#[tokio::test]
async fn the_end_of_stdin_waits_for_refusals() {
    let replies = Arc::new(ReplyBytes::default());
    let incoming = [tool_call(1), tool_call(1)];
    let mut transport = Counted::new(Gated::new(incoming), replies.clone(), &limits(1, 1));
    assert!(transport.receive().await.is_some());
    {
        let mut end = Box::pin(transport.receive());
        assert!(
            poll_once(&mut end).is_none(),
            "the first call is unanswered"
        );
        replies.answered(&rmcp::model::NumberOrString::Number(1));
        assert!(poll_once(&mut end).is_none(), "the refusal is not written");
    }
    transport.inner.gates.remove(0).send(()).unwrap();
    assert!(transport.receive().await.is_none());
}

// covers UP-08: with no reply written, work cannot hold the end of stdin past the deadline and
// the grace period, and replies past the budget do not hide that stdin ended, even behind
// requests still unread.
#[tokio::test(start_paused = true)]
async fn the_wait_after_stdin_ends_is_bounded() {
    let replies = Arc::new(ReplyBytes::default());
    let incoming = [tool_call(1), tool_call(2), tool_call(3)];
    let mut transport = Counted::new(Gated::new(incoming), replies.clone(), &limits(1, 1));
    assert!(transport.receive().await.is_some());
    let _stuck = replies.hold(1001);
    let started = tokio::time::Instant::now();
    assert!(transport.receive().await.is_none());
    assert_eq!(started.elapsed(), limits(1, 1).deadline + SHUTDOWN_GRACE);
    assert_eq!(replies.unanswered_count(), 1, "nothing read ahead was run");
}

/// The id of a request.
fn request_id(message: &RxJsonRpcMessage<RoleServer>) -> Option<RequestId> {
    match message {
        JsonRpcMessage::Request(request) => Some(request.id.clone()),
        _ => None,
    }
}

// covers UP-08: while replies pass the budget, reading ahead stops at the byte budget, and once
// they are written the queue drains in order and reading resumes.
#[tokio::test]
async fn read_ahead_stops_at_its_byte_budget_and_resumes() {
    let replies = Arc::new(ReplyBytes::default());
    let incoming = [tool_call(1), tool_call(2), tool_call(3), tool_call(4)];
    let size = serialized_len(&incoming[0]).unwrap();
    let mut transport = Counted::new(Gated::new(incoming), replies.clone(), &limits(1, 1));
    transport.read_ahead_budget = 2 * size;
    let stuck = replies.hold(1001);
    assert!(poll_once(&mut Box::pin(transport.receive())).is_none());
    assert_eq!(transport.read_ahead.len(), 2);
    assert_eq!(transport.read_ahead_bytes, 2 * size);
    assert_eq!(transport.inner.incoming.len(), 2, "the rest stays unread");

    drop(stuck);
    for id in 1..=4 {
        let message = transport.receive().await.unwrap();
        assert_eq!(
            request_id(&message),
            Some(rmcp::model::NumberOrString::Number(id))
        );
        replies.answered(&rmcp::model::NumberOrString::Number(id));
    }
    assert_eq!(transport.read_ahead_bytes, 0, "each size is released");
    assert!(transport.read_ahead.is_empty());
}

// covers UP-08: one message larger than the byte budget is still read and proceeds, so the
// server cannot wedge on it; nothing more is read behind it meanwhile.
#[tokio::test]
async fn a_message_over_the_read_ahead_budget_still_proceeds() {
    let replies = Arc::new(ReplyBytes::default());
    let mut transport = Counted::new(
        Gated::new([tool_call(1), tool_call(2)]),
        replies.clone(),
        &limits(1, 1),
    );
    transport.read_ahead_budget = 1;
    let stuck = replies.hold(1001);
    assert!(poll_once(&mut Box::pin(transport.receive())).is_none());
    assert_eq!(transport.read_ahead.len(), 1);
    assert_eq!(transport.inner.incoming.len(), 1);

    drop(stuck);
    let first = transport.receive().await.unwrap();
    assert_eq!(
        request_id(&first),
        Some(rmcp::model::NumberOrString::Number(1))
    );
    assert_eq!(transport.read_ahead_bytes, 0);
    assert!(transport.receive().await.is_some(), "reading goes on");
}

// covers UP-08: a client that closed stdin and keeps reading stdout gets its slower calls
// answered: each reply written moves the wait on.
#[tokio::test(start_paused = true)]
async fn replies_being_written_keep_the_end_of_stdin_waiting() {
    let replies = Arc::new(ReplyBytes::default());
    let mut transport = Counted::new(Gated::new([tool_call(1)]), replies.clone(), &limits(1, 1));
    assert!(transport.receive().await.is_some());
    let deadline = limits(1, 1).deadline;
    let reply = replies.hold(1);
    tokio::spawn(async move {
        tokio::time::sleep(deadline).await;
        drop(reply);
    });
    let started = tokio::time::Instant::now();
    assert!(transport.receive().await.is_none());
    assert_eq!(started.elapsed(), deadline + deadline + SHUTDOWN_GRACE);
}

// covers UP-08: a request counts until its reply reaches the transport (where its bytes count
// from then on), however its handler ended; a cancelled one (no reply comes) until nothing
// works on it any more: a tool call once its handler ended, anything else at once.
#[test]
fn requests_count_until_answered_or_done_with() {
    let replies = ReplyBytes::default();
    let id = |n| rmcp::model::NumberOrString::Number(n);
    let tool = Work {
        tool: true,
        ..Work::default()
    };
    for n in 1..=4 {
        replies.unanswered().insert(id(n), tool);
    }
    replies.unanswered().insert(id(5), Work::default());

    // 1: answered after its handler ended.
    replies.handler_ended(&id(1), false);
    assert!(
        replies.unanswered().contains_key(&id(1)),
        "the reply is on its way"
    );
    replies.answered(&id(1));
    // 2: cancelled while running, then its handler ends.
    replies.cancelled(&id(2));
    assert!(replies.unanswered().contains_key(&id(2)), "still running");
    replies.handler_ended(&id(2), false);
    // 3: cancelled after its handler ended (rmcp drops the reply).
    replies.handler_ended(&id(3), false);
    replies.cancelled(&id(3));
    // 4: its handler panicked: no reply comes.
    replies.handler_ended(&id(4), true);
    // 5: anything else, cancelled.
    replies.cancelled(&id(5));
    assert_eq!(replies.unanswered_count(), 0);
}
