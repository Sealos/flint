//! Ends a log session from outside, with a reason its tab can show.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::watch;

use super::session::{FailedHop, Feed, LogState, Output};

/// A feed that ends when its stop signal turns `true`. The inner feed is dropped at that moment,
/// with its open streams and the client they hold. Dropping the sender ends nothing.
struct UntilStopped {
    feed: Option<Box<dyn Feed>>,
    stop: Pin<Box<dyn Future<Output = ()> + Send>>,
    why: String,
}

/// `feed`, ended with the message `why` once `stop` turns `true`.
pub(crate) fn until_stopped(
    feed: Box<dyn Feed>,
    mut stop: watch::Receiver<bool>,
    why: String,
) -> Box<dyn Feed> {
    let stop = Box::pin(async move {
        if stop.wait_for(|stopped| *stopped).await.is_err() {
            std::future::pending::<()>().await;
        }
    });
    Box::new(UntilStopped {
        feed: Some(feed),
        stop,
        why,
    })
}

impl Feed for UntilStopped {
    fn poll(&mut self, cx: &mut Context<'_>, out: &mut Output) -> Poll<()> {
        let Some(feed) = &mut self.feed else {
            return Poll::Pending;
        };
        if self.stop.as_mut().poll(cx).is_pending() {
            return feed.poll(cx, out);
        }
        self.feed = None;
        // A session that already ended keeps saying how it ended.
        if !out.has_ended() {
            out.set_state(&LogState::Ended {
                error: Some(std::mem::take(&mut self.why)),
                hop: FailedHop::Unknown,
                pod_deleted: false,
            });
        }
        Poll::Ready(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::session::{LogSnapshot, SessionConfig, run};
    use super::*;

    /// A feed that never produces anything, and says when it was dropped.
    struct Held(#[allow(dead_code)] Arc<()>);

    impl Feed for Held {
        fn poll(&mut self, _: &mut Context<'_>, _: &mut Output) -> Poll<()> {
            Poll::Pending
        }
    }

    /// A feed that ends by itself at its first poll.
    struct EndsAtOnce(bool);

    impl Feed for EndsAtOnce {
        fn poll(&mut self, _: &mut Context<'_>, out: &mut Output) -> Poll<()> {
            if std::mem::replace(&mut self.0, true) {
                return Poll::Pending;
            }
            out.set_state(&LogState::Ended {
                error: None,
                hop: FailedHop::Unknown,
                pod_deleted: true,
            });
            Poll::Ready(())
        }
    }

    fn start(
        feed: Box<dyn Feed>,
    ) -> (
        watch::Sender<bool>,
        watch::Receiver<Arc<LogSnapshot>>,
        tokio::task::JoinHandle<()>,
        watch::Sender<bool>,
    ) {
        let (stop, stop_rx) = watch::channel(false);
        let (tx, rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (paused, paused_rx) = watch::channel(false);
        let feed = until_stopped(feed, stop_rx, "disconnected".to_owned());
        let task = tokio::spawn(run(
            feed,
            SessionConfig::new(100, 1 << 20),
            tx,
            paused_rx,
            None,
        ));
        (stop, rx, task, paused)
    }

    async fn until_ended(rx: &mut watch::Receiver<Arc<LogSnapshot>>) -> LogState {
        let ended = rx.wait_for(|s| matches!(s.state, LogState::Ended { .. }));
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .expect("the session ends")
            .expect("the session runs")
            .state
            .clone()
    }

    #[tokio::test]
    async fn a_stop_ends_the_session_with_its_reason_and_drops_the_feed() {
        let held = Arc::new(());
        let (stop, mut rx, task, _paused) = start(Box::new(Held(Arc::clone(&held))));
        stop.send_replace(true);
        let state = until_ended(&mut rx).await;
        assert!(matches!(
            state,
            LogState::Ended { error: Some(ref e), .. } if e == "disconnected"
        ));
        assert_eq!(Arc::strong_count(&held), 1, "the feed is gone");
        task.abort();
    }

    #[tokio::test]
    async fn a_session_that_ended_keeps_its_own_ending() {
        let (stop, mut rx, task, _paused) = start(Box::new(EndsAtOnce(false)));
        until_ended(&mut rx).await;
        stop.send_replace(true);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(matches!(
            rx.borrow().state,
            LogState::Ended {
                error: None,
                pod_deleted: true,
                ..
            }
        ));
        task.abort();
    }

    #[tokio::test]
    async fn dropping_the_signal_ends_nothing() {
        let (stop, rx, task, _paused) = start(Box::new(Held(Arc::new(()))));
        drop(stop);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!matches!(rx.borrow().state, LogState::Ended { .. }));
        task.abort();
    }
}
