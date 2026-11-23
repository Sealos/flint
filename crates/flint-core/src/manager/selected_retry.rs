//! Carries out [`SelectedRetry`] for the selected cluster: its timer, its progress event, and
//! Cancel. An automatic retry goes through [`ClusterManager::ensure_connected`], and unlike a
//! selection it does not wake a resting auth plugin.
// module-tests: ../connection/retry_tests.rs

use std::time::Instant;

use tokio::task::AbortHandle;

use super::running_set::SetJitter;
use super::{ClusterManager, Internal};
use crate::SafeErrorText;
use crate::connection::member::MemberEvent;
use crate::connection::policy::Role;
use crate::connection::retry::{RetryPhase, RetryTarget, SelectedRetry};
use crate::handle::{ConnectionProgress, ConnectionState, Event};

const CANCELED: &str = "connection canceled";

/// The selected cluster's retry state and the timer for its next attempt.
#[derive(Default)]
pub(super) struct RetryDriver {
    pub(super) retry: SelectedRetry,
    shown: Option<ConnectionProgress>,
    timer: Option<AbortHandle>,
    /// Which timer is current, so one that already fired cannot start a later retry early.
    pub(super) timer_id: u64,
}

impl ClusterManager {
    /// What retrying needs to know about the selected cluster now.
    fn observe_selected(&self) -> Option<(RetryTarget, RetryPhase)> {
        let id = self.active.as_deref()?;
        let slot = self.cluster(id)?;
        let target = RetryTarget {
            cluster: id.to_owned(),
            generation: slot.generation,
            definition: self
                .contexts
                .shown(id, slot.info.server.as_deref())
                .definition,
        };
        let phase = match slot.state {
            ConnectionState::Connecting => RetryPhase::Connecting,
            ConnectionState::Failed(_) => RetryPhase::Failed {
                attempt: slot.attempt,
                transient: slot.transient_attempt == Some(slot.attempt),
                may_retry: !self.paused(id)
                    && !self.unsettled(id)
                    && self.connection_role(id) == Role::Foreground
                    && !self.runs_a_command(id),
            },
            _ => RetryPhase::Settled,
        };
        Some((target, phase))
    }

    /// Brings the retry state up to date, and its timer and progress event with it.
    pub(super) fn sync_selected_retry(&mut self) {
        let observed = self.observe_selected();
        self.retries
            .retry
            .observe(observed, Instant::now(), &mut SetJitter);
        let progress = self.retries.retry.progress();
        if progress == self.retries.shown {
            return;
        }
        let next = progress.as_ref().and_then(|p| p.next_retry);
        if next != self.retries.shown.as_ref().and_then(|p| p.next_retry) {
            self.arm_retry_timer(next);
        }
        self.retries.shown.clone_from(&progress);
        self.emit(Event::ConnectionProgress(progress));
    }

    fn arm_retry_timer(&mut self, next: Option<Instant>) {
        if let Some(timer) = self.retries.timer.take() {
            timer.abort();
        }
        self.retries.timer_id += 1;
        let Some(at) = next else {
            return;
        };
        let timer = self.retries.timer_id;
        let wait = at.saturating_duration_since(Instant::now());
        let internal = self.internal_tx.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = internal.send(Internal::SelectedRetryDue { timer });
        });
        self.retries.timer = Some(task.abort_handle());
    }

    /// A retry's time came. Everything that forbids one is checked again in this step.
    pub(super) fn selected_retry_due(&mut self, timer: u64) {
        if timer != self.retries.timer_id {
            return;
        }
        let observed = self.observe_selected();
        self.retries
            .retry
            .observe(observed, Instant::now(), &mut SetJitter);
        if !self.retries.retry.take_due() {
            return;
        }
        if let Some(id) = self.active.clone() {
            self.ensure_connected(&id);
        }
    }

    /// Ends the selected cluster's attempt and its retries. It stays failed until selected or
    /// retried. Ignored for another cluster or an older slot of this one.
    pub(super) fn cancel_connection(&mut self, id: &str, generation: u64) {
        if self.active.as_deref() != Some(id) {
            return;
        }
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if slot.generation != generation {
            return;
        }
        let reason = match &slot.state {
            ConnectionState::Connecting => SafeErrorText::reduced(CANCELED),
            ConnectionState::Failed(error) => error.clone(),
            _ => return,
        };
        let connecting = matches!(slot.state, ConnectionState::Connecting);
        let mut attempt = None;
        if connecting {
            if let Some(task) = slot.connect_task.take() {
                task.abort();
            }
            // No attempt has id 0, so a result already queued is ignored.
            slot.attempt = 0;
            slot.discovering = false;
            slot.attempt_lease = None;
            attempt = slot.member_attempt.take();
            slot.clear_connection();
            slot.state = ConnectionState::Failed(reason.clone());
        }
        self.retries.retry.cancel();
        if let Some(attempt) = attempt {
            self.schedule.finish(attempt);
        }
        self.apply_member(
            id,
            MemberEvent::Canceled {
                reason: reason.clone(),
            },
            Instant::now(),
        );
        if connecting {
            self.emit(Event::ConnectionChanged {
                cluster: id.to_owned(),
                state: ConnectionState::Failed(reason),
            });
            self.reconcile(id);
        }
    }
}
