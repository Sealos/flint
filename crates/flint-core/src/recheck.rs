//! Rechecking that a session's context still points at the API server the session opened on.
//!
//! Log sessions and exec sessions keep their client for as long as they run, so a context that
//! was re-pointed at another server (`aws eks update-kubeconfig --alias main` run for another
//! cluster, say) or removed would go unnoticed: the session would keep talking to the old
//! server while the context's name now means another cluster. Each such session checks its
//! context every [`RECHECK_EVERY`] with a [`Recheck`]: the cluster manager answers from its
//! [`ContextHistory`], never the server, and the session ends with the [`reason`] it gives.

use std::collections::{HashMap, HashSet};
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

/// What a pinned session is, for what its reason to end tells to do instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pinned {
    Logs,
    Shell,
}

/// What a session is pinned to: its context's generation in [`ContextHistory`] when the
/// session got its client. Read in the same step as the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextPin(u64);

/// A context as kubeconfig loads last saw it.
#[derive(Debug)]
struct Record {
    server: Option<String>,
    /// In the last complete load (or in a later partial one).
    present: bool,
    /// Bumped whenever the context's server changes or a complete load lacks it, so a change
    /// that a later reload undoes is still seen by sessions pinned before it.
    generation: u64,
}

/// Every context's history across kubeconfig loads, for the sessions pinned to them (UP-06).
///
/// A file that cannot be read or parsed (caught half-written, say) is skipped by the loader,
/// so its contexts are missing from that load. That says nothing about them: only a complete
/// load, where every file loaded, that lacks a context counts as removing it. A context that a
/// partial load still has is recorded as usual, since its clients are built from it.
#[derive(Debug, Default)]
pub(crate) struct ContextHistory {
    records: HashMap<String, Record>,
    /// The last generation handed out; generations are unique across contexts.
    last: u64,
}

impl ContextHistory {
    /// Records a load: the `(context, server)` pairs it found, and whether every file loaded.
    pub fn record<'a>(
        &mut self,
        contexts: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
        complete: bool,
    ) {
        let mut seen = HashSet::new();
        for (name, server) in contexts {
            if !seen.insert(name) {
                continue;
            }
            match self.records.get_mut(name) {
                Some(r) if r.present && r.server.as_deref() == server => {}
                Some(r) => {
                    self.last += 1;
                    *r = Record {
                        server: server.map(str::to_owned),
                        present: true,
                        generation: self.last,
                    };
                }
                None => {
                    self.last += 1;
                    self.records.insert(
                        name.to_owned(),
                        Record {
                            server: server.map(str::to_owned),
                            present: true,
                            generation: self.last,
                        },
                    );
                }
            }
        }
        if complete {
            for (name, r) in &mut self.records {
                if r.present && !seen.contains(name.as_str()) {
                    self.last += 1;
                    r.present = false;
                    r.generation = self.last;
                }
            }
        }
    }

    /// The pin for a session on `context` now. A context never seen gets a pin that is
    /// already stale.
    pub fn pin(&self, context: &str) -> ContextPin {
        ContextPin(self.records.get(context).map_or(0, |r| r.generation))
    }

    /// Why a session on `context` pinned at `pin` must end, or `None` while the context is
    /// unchanged since (or nothing conclusive says otherwise).
    pub fn check(&self, session: Pinned, context: &str, pin: ContextPin) -> Option<String> {
        match self.records.get(context) {
            Some(r) if r.generation == pin.0 => None,
            Some(r) if r.present => Some(reason(session, context, Change::Repointed)),
            _ => Some(reason(session, context, Change::Removed)),
        }
    }
}

/// How a session's context changed since it opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// It points to another API server (or did since the session opened).
    Repointed,
    /// A complete kubeconfig load lacks it.
    Removed,
}

/// Why a session on `context` ends, as shown.
pub(crate) fn reason(session: Pinned, context: &str, change: Change) -> String {
    match change {
        Change::Removed => format!("\"{context}\" is no longer in your kubeconfig files"),
        Change::Repointed => {
            let again = match session {
                Pinned::Logs => "open the logs again to read from it",
                Pinned::Shell => "open the shell again to run one there",
            };
            format!("\"{context}\" now points to a different API server; {again}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn reasons_keep_their_wording() {
        assert_eq!(
            reason(Pinned::Shell, "dev", Change::Repointed),
            "\"dev\" now points to a different API server; open the shell again to run one there"
        );
        assert_eq!(
            reason(Pinned::Logs, "dev", Change::Repointed),
            "\"dev\" now points to a different API server; open the logs again to read from it"
        );
        assert_eq!(
            reason(Pinned::Shell, "dev", Change::Removed),
            "\"dev\" is no longer in your kubeconfig files"
        );
    }

    fn history(contexts: &[(&str, Option<&str>)]) -> ContextHistory {
        let mut history = ContextHistory::default();
        history.record(contexts.iter().copied(), true);
        history
    }

    const STAGING: Option<&str> = Some("https://staging");
    const PROD: Option<&str> = Some("https://prod");

    // covers UP-06, UP-41
    #[test]
    fn a_pinned_session_ends_once_its_context_moves_or_goes() {
        let mut h = history(&[("main", STAGING), ("other", PROD)]);
        let pin = h.pin("main");
        assert_eq!(h.check(Pinned::Shell, "main", pin), None);
        // Another context changing is none of its business; nor is the same load again.
        h.record([("main", STAGING), ("other", STAGING)], true);
        h.record([("main", STAGING), ("other", STAGING)], true);
        assert_eq!(h.check(Pinned::Shell, "main", pin), None);

        h.record([("main", PROD)], true);
        let moved = Some(reason(Pinned::Shell, "main", Change::Repointed));
        assert_eq!(h.check(Pinned::Shell, "main", pin), moved);
        // A session opened now is pinned to the new server.
        assert_eq!(h.check(Pinned::Shell, "main", h.pin("main")), None);

        let pin = h.pin("main");
        h.record([("other", PROD)], true);
        let gone = Some(reason(Pinned::Shell, "main", Change::Removed));
        assert_eq!(h.check(Pinned::Shell, "main", pin), gone);
        // A context never seen is stale at once.
        assert_eq!(
            h.check(Pinned::Shell, "nowhere", h.pin("nowhere")),
            Some(reason(Pinned::Shell, "nowhere", Change::Removed))
        );
    }

    // covers UP-06, UP-41: re-pointed and put back between two checks still ends the session.
    #[test]
    fn a_change_undone_before_the_next_check_still_counts() {
        let mut h = history(&[("main", STAGING)]);
        let pin = h.pin("main");
        h.record([("main", PROD)], true);
        h.record([("main", STAGING)], true);
        assert_eq!(
            h.check(Pinned::Shell, "main", pin),
            Some(reason(Pinned::Shell, "main", Change::Repointed))
        );
        // Removed and added back, too.
        let pin = h.pin("main");
        h.record([], true);
        h.record([("main", STAGING)], true);
        assert!(h.check(Pinned::Logs, "main", pin).is_some());
    }

    // covers UP-06: a file caught half-written is skipped by the loader. Its contexts are
    // missing from that load, which does not remove them.
    #[test]
    fn a_partial_load_does_not_remove_a_context() {
        let mut h = history(&[("main", STAGING), ("other", PROD)]);
        let pin = h.pin("main");
        h.record([("other", PROD)], false);
        assert_eq!(h.check(Pinned::Shell, "main", pin), None);
        // The file comes back as it was.
        h.record([("main", STAGING), ("other", PROD)], true);
        assert_eq!(h.check(Pinned::Shell, "main", pin), None);
        // A partial load that has the context re-pointed is conclusive about it.
        h.record([("main", PROD)], false);
        assert!(h.check(Pinned::Shell, "main", pin).is_some());
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
