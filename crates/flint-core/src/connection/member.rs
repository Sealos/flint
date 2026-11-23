//! The state of one cluster in a running set, as a pure transition function.
//!
//! [`Member::apply`] changes the state and returns the one thing the caller must do. A pause is
//! an overlay on the state: reads already running continue, nothing new starts, and an attempt
//! in flight when the pause begins no longer counts, so its result is discarded and the member
//! is queued again when the pause ends. An activation during a pause starts nothing, and is
//! carried out in the foreground when the last pause ends.

use std::fmt;
use std::time::{Duration, Instant};

use super::schedule::{AttemptId, Jitter, backoff_delay};
use crate::cluster::SafeErrorText;
use crate::redact;

/// How long a member may stay [`MemberState::Reconnecting`] before it fails (ADR-0012).
pub(crate) const RECONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// The API server a member was joined for. Compares by the exact URL, so any change to it counts
/// as a re-point. The URL may hold credentials, so `Debug` shows it redacted and nothing else
/// exposes it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ServerIdentity(String);

impl ServerIdentity {
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }
}

impl fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ServerIdentity")
            .field(&redact::server_url(&self.0))
            .finish()
    }
}

/// What a member's connection is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberState {
    /// Waiting for an attempt, or with one in flight.
    Connecting,
    Live,
    /// Watches lost contact at `since`, and none has synced again.
    Reconnecting {
        since: Instant,
        error: SafeErrorText,
    },
    /// Needs the user: never retried until the member is activated.
    SignInNeeded {
        reason: SafeErrorText,
    },
    /// Retried from `retry_at`.
    Failed {
        error: SafeErrorText,
        retry_at: Instant,
    },
}

/// Why a member is paused. Both can apply at once, and it stays paused until both have ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pause {
    Held,
    Provisional,
}

/// How a connection attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    Connected,
    /// A network or server failure, worth retrying.
    Failed(SafeErrorText),
    /// The credentials were rejected (401 or auth).
    AuthRejected(SafeErrorText),
}

/// Something that happened to a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberEvent {
    AttemptFinished {
        attempt: AttemptId,
        outcome: AttemptOutcome,
    },
    /// The health of the member's watches: whether any kind is synced, and the error of a kind
    /// that is reconnecting.
    Watches {
        any_synced: bool,
        error: Option<SafeErrorText>,
    },
    /// A running connection got a 401, or its credential refresh was denied.
    CredentialLost {
        reason: SafeErrorText,
    },
    /// The user made this member the active cluster.
    Activated,
    /// The user made another cluster active, so an activation waiting on a pause no longer holds.
    Deactivated,
    /// The kubeconfig now gives this `server` for the member's context. `confirmed` is false when
    /// the server may be a fallback showing while a higher-priority file is unreadable.
    Repointed {
        server: ServerIdentity,
        confirmed: bool,
    },
    Paused(Pause),
    Resumed(Pause),
    /// The slot's connection is gone and a new one is due (its definition changed, or a pause
    /// dropped it). Any earlier failure or sign-in was for the old connection.
    ConnectionLost,
    /// Time passed: checks the reconnect timeout and the retry time.
    Tick,
}

/// What the caller must do after a transition.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberAction {
    None,
    /// Put the member in the background queue.
    Enqueue,
    /// Take the member out of the background queue.
    Dequeue,
    /// Start an attempt now, bypassing the queue (the active cluster).
    StartNow,
    /// Stop the member's watches.
    StopWatches,
    /// Drop the member and its connection. Nothing connects to the new server.
    Remove,
}

/// One cluster in a running set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    state: MemberState,
    server: ServerIdentity,
    held: bool,
    provisional: bool,
    /// Activated during a pause, and not yet started or withdrawn.
    activation_pending: bool,
    failures: u32,
    attempt: Option<AttemptId>,
}

impl Member {
    /// A member joining the set: [`MemberState::SignInNeeded`] when the credential policy
    /// denied it (`denied`), otherwise [`MemberState::Connecting`], queued unless `paused`.
    pub fn join(
        server: ServerIdentity,
        denied: Option<SafeErrorText>,
        paused: Option<Pause>,
    ) -> (Self, MemberAction) {
        let (state, action) = match denied {
            Some(reason) => (MemberState::SignInNeeded { reason }, MemberAction::None),
            None if paused.is_some() => (MemberState::Connecting, MemberAction::None),
            None => (MemberState::Connecting, MemberAction::Enqueue),
        };
        let member = Self {
            state,
            server,
            held: paused == Some(Pause::Held),
            provisional: paused == Some(Pause::Provisional),
            activation_pending: false,
            failures: 0,
            attempt: None,
        };
        (member, action)
    }

    pub fn state(&self) -> &MemberState {
        &self.state
    }

    pub fn server(&self) -> &ServerIdentity {
        &self.server
    }

    /// Consecutive failures since the member was last live or activated.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// A reason the member is paused, if any.
    pub fn paused(&self) -> Option<Pause> {
        if self.held {
            Some(Pause::Held)
        } else if self.provisional {
            Some(Pause::Provisional)
        } else {
            None
        }
    }

    /// Whether a new health or alert check may run: only when live and not paused.
    pub fn checkable(&self) -> bool {
        self.state == MemberState::Live && self.paused().is_none()
    }

    /// Records that `attempt` is starting. `false` means it must not start: the member is
    /// paused, is not connecting, or already has an attempt. The caller then finishes the
    /// attempt in the schedule to free its slot.
    #[must_use]
    pub fn begin_attempt(&mut self, attempt: AttemptId) -> bool {
        if self.state != MemberState::Connecting
            || self.paused().is_some()
            || self.attempt.is_some()
        {
            return false;
        }
        self.attempt = Some(attempt);
        true
    }

    /// Applies `event` at `now` and returns what the caller must do.
    pub fn apply(
        &mut self,
        event: MemberEvent,
        now: Instant,
        jitter: &mut dyn Jitter,
    ) -> MemberAction {
        match event {
            MemberEvent::AttemptFinished { attempt, outcome } => {
                self.attempt_finished(attempt, outcome, now, jitter)
            }
            MemberEvent::Watches { any_synced, error } => self.watches(any_synced, error, now),
            MemberEvent::CredentialLost { reason } => self.credential_lost(reason),
            MemberEvent::Activated => self.activated(),
            MemberEvent::Deactivated => {
                self.activation_pending = false;
                MemberAction::None
            }
            MemberEvent::Repointed { server, confirmed } => self.repointed(&server, confirmed),
            MemberEvent::Paused(pause) => self.pause(pause),
            MemberEvent::Resumed(pause) => self.resume(pause),
            MemberEvent::ConnectionLost => self.connection_lost(),
            MemberEvent::Tick => self.tick(now, jitter),
        }
    }

    fn connection_lost(&mut self) -> MemberAction {
        self.attempt = None;
        self.failures = 0;
        self.state = MemberState::Connecting;
        if self.paused().is_some() {
            MemberAction::None
        } else {
            MemberAction::Enqueue
        }
    }

    fn attempt_finished(
        &mut self,
        attempt: AttemptId,
        outcome: AttemptOutcome,
        now: Instant,
        jitter: &mut dyn Jitter,
    ) -> MemberAction {
        // A pause clears the attempt, so this also discards a result landing during one.
        if self.attempt != Some(attempt) {
            return MemberAction::None;
        }
        self.attempt = None;
        match outcome {
            AttemptOutcome::Connected => {
                self.failures = 0;
                self.state = MemberState::Live;
            }
            AttemptOutcome::Failed(error) => self.fail(error, now, jitter),
            AttemptOutcome::AuthRejected(reason) => {
                self.failures = 0;
                self.state = MemberState::SignInNeeded { reason };
            }
        }
        MemberAction::None
    }

    fn watches(
        &mut self,
        any_synced: bool,
        error: Option<SafeErrorText>,
        now: Instant,
    ) -> MemberAction {
        match (&mut self.state, any_synced, error) {
            (MemberState::Live, false, Some(error)) => {
                self.state = MemberState::Reconnecting { since: now, error };
            }
            (MemberState::Reconnecting { .. }, true, _) => {
                self.failures = 0;
                self.state = MemberState::Live;
            }
            (MemberState::Reconnecting { error: current, .. }, false, Some(error)) => {
                *current = error;
            }
            _ => {}
        }
        MemberAction::None
    }

    fn credential_lost(&mut self, reason: SafeErrorText) -> MemberAction {
        match self.state {
            MemberState::Live | MemberState::Reconnecting { .. } => {
                self.failures = 0;
                self.state = MemberState::SignInNeeded { reason };
                MemberAction::StopWatches
            }
            _ => MemberAction::None,
        }
    }

    fn activated(&mut self) -> MemberAction {
        if self.paused().is_some() {
            self.activation_pending = true;
            return MemberAction::None;
        }
        self.start_foreground()
    }

    fn start_foreground(&mut self) -> MemberAction {
        match self.state {
            MemberState::SignInNeeded { .. } | MemberState::Failed { .. } => {
                self.failures = 0;
                self.state = MemberState::Connecting;
                MemberAction::StartNow
            }
            MemberState::Connecting if self.attempt.is_none() => MemberAction::StartNow,
            _ => MemberAction::None,
        }
    }

    /// A different server removes the member only once confirmed. Until then it may be a
    /// fallback, so the member keeps its server and pauses.
    fn repointed(&mut self, server: &ServerIdentity, confirmed: bool) -> MemberAction {
        if *server == self.server {
            MemberAction::None
        } else if confirmed {
            self.activation_pending = false;
            MemberAction::Remove
        } else {
            self.pause(Pause::Provisional)
        }
    }

    fn pause(&mut self, pause: Pause) -> MemberAction {
        let was_paused = self.paused().is_some();
        match pause {
            Pause::Held => self.held = true,
            Pause::Provisional => self.provisional = true,
        }
        if was_paused || self.state != MemberState::Connecting {
            return MemberAction::None;
        }
        self.attempt = None;
        MemberAction::Dequeue
    }

    fn resume(&mut self, pause: Pause) -> MemberAction {
        let flag = match pause {
            Pause::Held => &mut self.held,
            Pause::Provisional => &mut self.provisional,
        };
        let was_set = std::mem::replace(flag, false);
        if !was_set || self.paused().is_some() {
            return MemberAction::None;
        }
        if std::mem::take(&mut self.activation_pending) {
            return self.start_foreground();
        }
        if self.state == MemberState::Connecting {
            MemberAction::Enqueue
        } else {
            MemberAction::None
        }
    }

    fn tick(&mut self, now: Instant, jitter: &mut dyn Jitter) -> MemberAction {
        match &self.state {
            MemberState::Reconnecting { since, error }
                if now.saturating_duration_since(*since) >= RECONNECT_TIMEOUT =>
            {
                let error = error.clone();
                self.fail(error, now, jitter);
                MemberAction::StopWatches
            }
            MemberState::Failed { retry_at, .. } if now >= *retry_at && self.paused().is_none() => {
                self.state = MemberState::Connecting;
                MemberAction::Enqueue
            }
            _ => MemberAction::None,
        }
    }

    fn fail(&mut self, error: SafeErrorText, now: Instant, jitter: &mut dyn Jitter) {
        self.failures = self.failures.saturating_add(1);
        self.state = MemberState::Failed {
            error,
            retry_at: now + backoff_delay(self.failures, jitter),
        };
    }
}

#[cfg(test)]
#[path = "member_tests.rs"]
mod tests;
