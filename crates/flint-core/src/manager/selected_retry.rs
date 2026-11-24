//! Carries out [`SelectedRetry`] for the selected cluster: its timer, its progress event, and
//! Cancel. An automatic retry goes through [`ClusterManager::ensure_connected`], and unlike a
//! selection it does not wake a resting auth plugin.
// module-tests: ../connection/retry_tests.rs

use std::time::Instant;

use tokio::task::AbortHandle;

use super::running_set::SetJitter;
use super::{ClusterManager, Internal};
use crate::SafeErrorText;
use crate::connection::member::{AttemptOutcome, MemberEvent};
use crate::connection::policy::Role;
use crate::connection::retry::{RetryPhase, RetryTarget, SelectedRetry};
use crate::connection::schedule::AttemptId;
use crate::handle::{ConnectionProgress, ConnectionState, Event};

const CANCELED: &str = "connection canceled";
const NO_LONGER_VALID: &str = "connection stopped because its context changed";
const OUT_OF_TIME: &str = "connection timed out";

/// Sent instead of a timer's id when a retry under way reaches the series' deadline. Timers count
/// from 1.
const DEADLINE: u64 = 0;

/// An automatic retry under way, and what it was started for.
struct RetryAttempt {
    attempt: u64,
    target: RetryTarget,
    deadline: Instant,
    watchdog: AbortHandle,
}

impl Drop for RetryAttempt {
    fn drop(&mut self) {
        self.watchdog.abort();
    }
}

/// The selected cluster's retry state and the timer for its next attempt.
#[derive(Default)]
pub(super) struct RetryDriver {
    pub(super) retry: SelectedRetry,
    shown: Option<ConnectionProgress>,
    timer: Option<AbortHandle>,
    /// Which timer is current, so one that already fired cannot start a later retry early.
    pub(super) timer_id: u64,
    under_way: Option<RetryAttempt>,
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
            ConnectionState::AwaitingConfirmation | ConnectionState::Disconnected => {
                RetryPhase::Waiting
            }
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
        self.end_invalid_retry();
        self.observe_retry();
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

    fn observe_retry(&mut self) {
        let observed = self.observe_selected();
        self.retries
            .retry
            .observe(observed, Instant::now(), &mut SetJitter);
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

    /// A retry's time came. Everything that forbids one is checked again in this step, the files
    /// on disk included, since the last load can be behind them.
    pub(super) async fn selected_retry_due(&mut self, timer: u64) {
        self.end_invalid_retry();
        if timer == DEADLINE || timer != self.retries.timer_id {
            return;
        }
        self.observe_retry();
        if !self.retries.retry.is_pending() {
            return;
        }
        let Some(id) = self.active.clone() else {
            return;
        };
        let server = self.cluster(&id).and_then(|slot| slot.info.server.clone());
        let holds = match &server {
            Some(server) => self.files_still_show(&id, server).await,
            None => false,
        };
        // That read can have applied a load, so the state is looked at afresh.
        self.observe_retry();
        if !holds {
            self.retries.retry.cancel();
            return;
        }
        if self.active.as_deref() != Some(id.as_str()) || !self.retries.retry.take_due() {
            return;
        }
        let before = self.next_attempt;
        self.ensure_connected(&id);
        if self.next_attempt != before {
            self.track_retry_attempt(&id);
        }
    }

    /// Remembers the attempt just started for `id`, and when it must be over.
    fn track_retry_attempt(&mut self, id: &str) {
        let Some((target, _)) = self.observe_selected() else {
            return;
        };
        let Some(attempt) = self.cluster(id).map(|slot| slot.attempt) else {
            return;
        };
        let deadline = self.retries.retry.deadline().unwrap_or_else(Instant::now);
        let wait = deadline.saturating_duration_since(Instant::now());
        let internal = self.internal_tx.clone();
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = internal.send(Internal::SelectedRetryDue { timer: DEADLINE });
        })
        .abort_handle();
        self.retries.under_way = Some(RetryAttempt {
            attempt,
            target,
            deadline,
            watchdog,
        });
    }

    /// The selected cluster was disconnected: no attempt or timer of its retries goes on.
    pub(super) fn end_selected_retry(&mut self) {
        self.retries.under_way = None;
        self.retries.retry.cancel();
    }

    /// Ends the automatic retry under way if what it was started for no longer holds. Its result,
    /// even one already queued, is then ignored.
    pub(super) fn end_invalid_retry(&mut self) {
        let Some(under_way) = &self.retries.under_way else {
            return;
        };
        let id = under_way.target.cluster.clone();
        let (attempt, generation, definition) = (
            under_way.attempt,
            under_way.target.generation,
            under_way.target.definition,
        );
        let overdue = Instant::now() >= under_way.deadline;
        let Some(slot) = self
            .cluster(&id)
            .filter(|slot| slot.attempt == attempt)
            .filter(|slot| matches!(slot.state, ConnectionState::Connecting))
        else {
            // It finished, or something else already replaced it.
            self.retries.under_way = None;
            return;
        };
        let same_target = slot.generation == generation
            && self
                .contexts
                .shown(&id, slot.info.server.as_deref())
                .definition
                == definition;
        let selected = self.active.as_deref() == Some(id.as_str());
        if selected
            && same_target
            && !overdue
            && !self.paused(&id)
            && !self.unsettled(&id)
            && !self.runs_a_command(&id)
        {
            return;
        }
        self.retries.under_way = None;
        let reason = SafeErrorText::reduced(if overdue {
            OUT_OF_TIME
        } else {
            NO_LONGER_VALID
        });
        let member_attempt = self.stop_attempt(&id, reason.clone());
        if selected {
            if let Some(slot) = self.cluster_mut(&id) {
                slot.awaits_selection = true;
            }
            self.retries.retry.cancel();
            if let Some(member_attempt) = member_attempt {
                self.schedule.finish(member_attempt);
            }
            let event = MemberEvent::Canceled {
                reason: reason.clone(),
            };
            self.apply_member(&id, event, Instant::now());
        } else {
            // In the background, a member goes on by its own rules.
            let outcome = AttemptOutcome::Failed(reason.clone());
            self.finish_member_attempt(&id, member_attempt, outcome, Instant::now());
        }
        self.emit(Event::ConnectionChanged {
            cluster: id.clone(),
            state: ConnectionState::Failed(reason),
        });
        self.reconcile(&id);
    }

    /// Stops `id`'s attempt and leaves it failed with `reason`. Returns the member attempt it
    /// carried, for the caller to finish.
    fn stop_attempt(&mut self, id: &str, reason: SafeErrorText) -> Option<AttemptId> {
        let slot = self.cluster_mut(id)?;
        if let Some(task) = slot.connect_task.take() {
            task.abort();
        }
        // No attempt has id 0, so a result already queued is ignored.
        slot.attempt = 0;
        slot.discovering = false;
        slot.attempt_lease = None;
        let attempt = slot.member_attempt.take();
        slot.clear_connection();
        slot.state = ConnectionState::Failed(reason);
        attempt
    }

    /// A load replaced `id`'s slot while its retries were running. The new definition waits for
    /// the person, in the member's state too.
    pub(super) fn retry_replaced(&mut self, id: &str) {
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        let reason = SafeErrorText::reduced(NO_LONGER_VALID);
        slot.awaits_selection = true;
        slot.reconnect = false;
        slot.state = ConnectionState::Failed(reason.clone());
        self.apply_member(id, MemberEvent::Canceled { reason }, Instant::now());
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
        let attempt = if connecting {
            self.stop_attempt(id, reason.clone())
        } else {
            None
        };
        self.retries.under_way = None;
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
