//! When the selected cluster's failed connection is tried again by itself.
//!
//! Pure, like the rest of this module: the manager reports what it sees with
//! [`SelectedRetry::observe`] and starts the attempt when told. A retry repeats an attempt the
//! person asked for, so it is only made for a failure that time can fix, on the definition they
//! selected, and never with a credential that runs a program.

use std::time::{Duration, Instant};

use super::schedule::{Jitter, backoff_delay};
use crate::api_error::Transport;
use crate::cluster::{ClusterError, ClusterId};
use crate::handle::ConnectionProgress;

/// How many automatic retries follow one selection or manual retry, about six minutes of backoff.
/// After that the failure stays on screen with its Retry button.
pub(crate) const MAX_AUTOMATIC_RETRIES: u32 = 10;

/// How long a series may run from its first attempt, delays and attempts together, so a server
/// that accepts connections and never answers cannot stretch it.
pub(crate) const RETRY_WINDOW: Duration = Duration::from_secs(390);

/// Whether the same request could succeed later with nothing changed on this side. Credentials,
/// TLS trust, configuration and refusals by the API need a person, so anything not known to be
/// passing is not retried.
pub(crate) fn transient(error: &ClusterError) -> bool {
    match error {
        ClusterError::Timeout { .. } => true,
        ClusterError::Transport { problem, .. } => !matches!(problem, Transport::Tls(_)),
        ClusterError::Http { code, .. } => matches!(code, 408 | 429 | 500 | 502 | 503 | 504),
        _ => false,
    }
}

/// The context and definition an attempt was made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetryTarget {
    pub cluster: ClusterId,
    pub generation: u64,
    pub definition: u64,
}

/// The selected cluster's connection, as far as retrying goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryPhase {
    Connecting,
    /// `attempt` failed. `may_retry` is false while the context is paused or provisional, or its
    /// credential runs a program.
    Failed {
        attempt: u64,
        transient: bool,
        may_retry: bool,
    },
    /// Paused or disconnected: nothing runs now, and what the series used stays used.
    Waiting,
    /// Connected, or not trying.
    Settled,
}

#[derive(Debug)]
struct Tracked {
    target: RetryTarget,
    since: Instant,
    retries: u32,
    failed_attempt: Option<u64>,
    retry_at: Option<Instant>,
    stopped: bool,
}

/// The retry state of the selected cluster.
#[derive(Debug, Default)]
pub(crate) struct SelectedRetry {
    /// What the person last selected. A later definition gets no automatic retry.
    selected: Option<RetryTarget>,
    current: Option<Tracked>,
}

impl SelectedRetry {
    /// The person selected the cluster or asked to retry it. With no attempt under way (`idle`),
    /// the series starts over.
    pub fn selected(&mut self, idle: bool) {
        self.selected = None;
        if idle {
            self.current = None;
        }
    }

    /// Stops retrying until the next selection or manual retry.
    pub fn cancel(&mut self) {
        if let Some(tracked) = &mut self.current {
            tracked.stopped = true;
            tracked.retry_at = None;
        }
    }

    /// Follows the selected cluster's state. `None` means nothing is selected or it is gone.
    pub fn observe(
        &mut self,
        observed: Option<(RetryTarget, RetryPhase)>,
        now: Instant,
        jitter: &mut dyn Jitter,
    ) {
        let Some((target, phase)) = observed else {
            *self = Self::default();
            return;
        };
        if self
            .selected
            .as_ref()
            .is_none_or(|selected| selected.cluster != target.cluster)
        {
            self.selected = Some(target.clone());
        }
        if self
            .current
            .as_ref()
            .is_some_and(|tracked| tracked.target != target)
        {
            self.current = None;
        }
        if phase == RetryPhase::Settled {
            self.current = None;
            return;
        }
        if phase == RetryPhase::Waiting {
            self.cancel();
            return;
        }
        let on_selected = self.selected.as_ref() == Some(&target);
        let tracked = self.current.get_or_insert(Tracked {
            target,
            since: now,
            retries: 0,
            failed_attempt: None,
            retry_at: None,
            // A failure first seen here came from no attempt this state watched start.
            stopped: !on_selected || matches!(phase, RetryPhase::Failed { .. }),
        });
        match phase {
            RetryPhase::Connecting => tracked.retry_at = None,
            RetryPhase::Failed {
                attempt,
                transient,
                may_retry,
            } => {
                if !may_retry {
                    tracked.stopped = true;
                    tracked.retry_at = None;
                }
                if tracked.failed_attempt != Some(attempt) {
                    tracked.failed_attempt = Some(attempt);
                    if !transient || tracked.retries >= MAX_AUTOMATIC_RETRIES {
                        tracked.stopped = true;
                    }
                    if !tracked.stopped {
                        let at = now + backoff_delay(tracked.retries + 1, jitter);
                        if at < tracked.since + RETRY_WINDOW {
                            tracked.retries += 1;
                            tracked.retry_at = Some(at);
                        } else {
                            tracked.stopped = true;
                        }
                    }
                }
            }
            RetryPhase::Waiting | RetryPhase::Settled => {}
        }
    }

    /// Whether a retry is waiting for its time.
    pub fn is_pending(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|tracked| tracked.retry_at.is_some())
    }

    /// When the current series must be over.
    pub fn deadline(&self) -> Option<Instant> {
        let tracked = self.current.as_ref()?;
        Some(tracked.since + RETRY_WINDOW)
    }

    /// Whether automatic retries have run on this slot, so that a change to its definition is
    /// for the person to act on.
    pub fn has_retried(&self, cluster: &str, generation: u64) -> bool {
        self.current.as_ref().is_some_and(|tracked| {
            tracked.retries > 0
                && tracked.target.cluster == cluster
                && tracked.target.generation == generation
        })
    }

    /// Takes the pending retry, if there is one.
    pub fn take_due(&mut self) -> bool {
        self.current
            .as_mut()
            .is_some_and(|tracked| tracked.retry_at.take().is_some())
    }

    /// What the front end shows, for the slot it was made for.
    pub fn progress(&self) -> Option<ConnectionProgress> {
        self.current.as_ref().map(|tracked| ConnectionProgress {
            cluster: tracked.target.cluster.clone(),
            generation: tracked.target.generation,
            since: tracked.since,
            retries: tracked.retries,
            next_retry: tracked.retry_at,
        })
    }
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
