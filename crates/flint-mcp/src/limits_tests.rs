use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use super::*;

fn limits(running: usize, waiting: usize) -> Limits {
    Limits {
        running,
        waiting,
        reply_budget: 1000,
        deadline: Duration::from_secs(60),
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
        std::future::pending()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

// covers UP-08: a reply's bytes count from when it is handed over until it is written.
#[tokio::test]
async fn reply_bytes_count_until_written() {
    let replies = Arc::new(ReplyBytes::default());
    let mut transport = Counted::new(Gated { gates: Vec::new() }, replies.clone());
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
