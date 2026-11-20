//! Rechecking that a session's context still points at the API server the session opened on.
//!
//! Log sessions and exec sessions keep their client for as long as they run, so a context that
//! was re-pointed at another server (`aws eks update-kubeconfig --alias main` run for another
//! cluster, say) or removed would go unnoticed: the session would keep talking to the old
//! server while the context's name now means another cluster. Each such session checks its
//! context every [`RECHECK_EVERY`] with a [`Recheck`], and ends with the reason [`moved`] gives.
//! The check asks the cluster manager, never the server.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::time::{Instant, Sleep};

/// How often a running session checks its context still points at the server it opened on.
pub(crate) const RECHECK_EVERY: Duration = Duration::from_secs(30);

/// A check that runs every `every`. It keeps its timer and the check in flight between polls,
/// so it can sit in a hand-written `poll` or in a `select!` loop that drops the future it
/// polled.
pub(crate) struct Recheck<T> {
    every: Duration,
    next: Pin<Box<Sleep>>,
    checking: Option<BoxFuture<'static, T>>,
}

impl<T> Recheck<T> {
    /// The first check is due `every` from now.
    pub fn new(every: Duration) -> Self {
        Recheck {
            every,
            next: Box::pin(tokio::time::sleep(every)),
            checking: None,
        }
    }

    /// Once a check is due, starts it with `start` and polls it. Ready with its result. The
    /// next check is due `every` after this one started.
    pub fn poll_check(
        &mut self,
        cx: &mut Context<'_>,
        start: impl FnOnce() -> BoxFuture<'static, T>,
    ) -> Poll<T> {
        if self.checking.is_none() {
            std::task::ready!(self.next.as_mut().poll(cx));
            self.next.as_mut().reset(Instant::now() + self.every);
            self.checking = Some(start());
        }
        let checking = self.checking.as_mut().expect("set above");
        let result = std::task::ready!(checking.poll_unpin(cx));
        self.checking = None;
        Poll::Ready(result)
    }
}

/// What a pinned session is, for what [`moved`] tells to do instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pinned {
    Logs,
    Shell,
}

/// Why a session opened on `context` at `pinned` (the context's API server then) must end, or
/// `None` while the context still points there. `current` is the context's server now, `None`
/// when the context is no longer in kubeconfig.
pub(crate) fn moved(
    session: Pinned,
    context: &str,
    pinned: Option<&str>,
    current: Option<Option<&str>>,
) -> Option<String> {
    let Some(current) = current else {
        return Some(format!(
            "\"{context}\" is no longer in your kubeconfig files"
        ));
    };
    if current == pinned {
        return None;
    }
    let again = match session {
        Pinned::Logs => "open the logs again to read from it",
        Pinned::Shell => "open the shell again to run one there",
    };
    Some(format!(
        "\"{context}\" now points to a different API server; {again}"
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn a_session_ends_when_its_context_moves_or_goes() {
        let dev = Some("https://dev");
        assert_eq!(moved(Pinned::Shell, "dev", dev, Some(dev)), None);
        assert_eq!(moved(Pinned::Logs, "dev", None, Some(None)), None);
        assert_eq!(
            moved(Pinned::Shell, "dev", dev, Some(Some("https://prod"))).as_deref(),
            Some(
                "\"dev\" now points to a different API server; open the shell again to run one \
                 there"
            )
        );
        assert_eq!(
            moved(Pinned::Logs, "dev", dev, Some(None)).as_deref(),
            Some(
                "\"dev\" now points to a different API server; open the logs again to read from it"
            )
        );
        assert_eq!(
            moved(Pinned::Shell, "dev", dev, None).as_deref(),
            Some("\"dev\" is no longer in your kubeconfig files")
        );
    }

    /// Polls `recheck` once; a check it starts takes the next of `answers`.
    async fn poll_once(
        recheck: &mut Recheck<u8>,
        answers: &mut VecDeque<oneshot::Receiver<u8>>,
        started: &mut usize,
    ) -> Poll<u8> {
        futures::future::poll_fn(|cx| {
            Poll::Ready(recheck.poll_check(cx, || {
                *started += 1;
                let rx = answers.pop_front().expect("a scripted answer");
                async move { rx.await.unwrap_or(0) }.boxed()
            }))
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn checks_run_every_interval_and_survive_dropped_polls() {
        let every = Duration::from_secs(30);
        let begin = Instant::now();
        let mut recheck = Recheck::new(every);
        let (first, rx) = oneshot::channel();
        let (second, rx2) = oneshot::channel();
        let mut answers = VecDeque::from([rx, rx2]);
        let mut started = 0;
        assert!(
            poll_once(&mut recheck, &mut answers, &mut started)
                .await
                .is_pending()
        );
        assert_eq!(started, 0, "not due yet");
        tokio::time::sleep(every).await;
        // Due: the check starts once, and stays in flight across polls that are dropped.
        assert!(
            poll_once(&mut recheck, &mut answers, &mut started)
                .await
                .is_pending()
        );
        assert!(
            poll_once(&mut recheck, &mut answers, &mut started)
                .await
                .is_pending()
        );
        assert_eq!(started, 1);
        first.send(7).unwrap();
        assert_eq!(
            poll_once(&mut recheck, &mut answers, &mut started).await,
            Poll::Ready(7)
        );
        // The next is due `every` after the first started, not before.
        tokio::time::sleep_until(begin + every * 2 - Duration::from_millis(1)).await;
        assert!(
            poll_once(&mut recheck, &mut answers, &mut started)
                .await
                .is_pending()
        );
        assert_eq!(started, 1);
        tokio::time::sleep_until(begin + every * 2).await;
        second.send(9).unwrap();
        assert_eq!(
            poll_once(&mut recheck, &mut answers, &mut started).await,
            Poll::Ready(9)
        );
        assert_eq!(started, 2);
    }
}
