//! The running set: clusters kept connected and watched beside the active one.
//!
//! [`Member`] decides what each cluster does and
//! [`Schedule`](crate::connection::schedule::Schedule) when a background attempt may start; this
//! carries out their answers on the cluster slots. Every attempt runs on its own task
//! and only its result comes back through the manager's loop, so a member that fails, hangs or
//! needs a sign-in holds up no other. Credentials that run a program never run for a background
//! member: it waits as signed out until it is selected.
//!
//! Membership grants nothing on the write path: writes ask the per-cluster gate as before.

// module-tests: ../manager.rs (and flint-e2e's running_set.rs, on the fake API)

use std::hash::{BuildHasher, RandomState};
use std::time::{Duration, Instant};

use super::ClusterManager;
use crate::cluster::{ClusterError, ClusterId, SafeErrorText};
use crate::connection::member::{
    AttemptOutcome, Member, MemberAction, MemberEvent, MemberState, Pause, ServerIdentity,
};
use crate::connection::policy::Role;
use crate::connection::schedule::{AttemptId, Jitter};
use crate::credential::CredentialSource;
use crate::handle::{ConnectionState, Event, MemberReport, MemberStatus};
use crate::store::SyncState;

/// The sizes a running set may have; an empty list clears it.
pub(crate) const SET_SIZES: std::ops::RangeInclusive<usize> = 2..=5;

/// How often members' watch health, retries and queue are looked at.
const SET_TICK: Duration = Duration::from_millis(250);

const SIGN_IN_IN_FRONT: &str = "this cluster's sign-in runs a program; select it to connect";

/// Backoff jitter from the standard library's per-instance random keys.
#[derive(Default)]
pub(super) struct SetJitter;

impl Jitter for SetJitter {
    fn sample(&mut self) -> u32 {
        RandomState::new().hash_one(0u8) as u32
    }
}

/// Whether a failed attempt needs the user rather than a retry.
pub(super) fn needs_sign_in(error: &ClusterError) -> bool {
    matches!(
        error,
        ClusterError::Http { code: 401, .. }
            | ClusterError::ExecPlugin { .. }
            | ClusterError::ExecPluginNotFound { .. }
    )
}

impl ClusterManager {
    /// Makes the running set exactly `ids`. A list that cannot be a set changes nothing.
    pub(super) fn set_running_set(&mut self, ids: Vec<ClusterId>) {
        let mut wanted: Vec<ClusterId> = Vec::with_capacity(ids.len());
        for id in ids {
            if !wanted.contains(&id) {
                wanted.push(id);
            }
        }
        if let Some(problem) = self.set_problem(&wanted) {
            self.emit(Event::Error(problem));
            return;
        }
        let leaving: Vec<ClusterId> = self
            .members
            .keys()
            .filter(|id| !wanted.contains(id))
            .cloned()
            .collect();
        for id in &leaving {
            self.leave_set(id);
        }
        let now = Instant::now();
        for id in &wanted {
            if !self.members.contains_key(id) {
                self.join_set(id, now);
            }
        }
        self.sync_roles();
        self.arm_set_ticker();
        self.pump_members(now);
    }

    fn set_problem(&self, wanted: &[ClusterId]) -> Option<String> {
        if !wanted.is_empty() && !SET_SIZES.contains(&wanted.len()) {
            return Some(format!(
                "A running set holds {} to {} clusters.",
                SET_SIZES.start(),
                SET_SIZES.end()
            ));
        }
        wanted.iter().find_map(|id| {
            let Some(slot) = self.cluster(id) else {
                return Some(format!("cluster \"{id}\" no longer exists"));
            };
            if slot.info.server.is_none() {
                return Some(format!("\"{id}\" names no server."));
            }
            // The server shown may be a fallback, which a member must not be bound to.
            (!self.members.contains_key(id) && self.unsettled(id)).then(|| {
                format!("\"{id}\" can't join while a kubeconfig file defining it is unreadable.")
            })
        })
    }

    fn join_set(&mut self, id: &str, now: Instant) {
        let Some(server) = self.cluster(id).and_then(|c| c.info.server.clone()) else {
            return;
        };
        let active = self.active.as_deref() == Some(id);
        let denied =
            (!active && self.runs_a_command(id)).then(|| SafeErrorText::reduced(SIGN_IN_IN_FRONT));
        let signed_out = denied.is_some();
        let (member, action) = Member::join(ServerIdentity::new(server), denied, None);
        self.members.insert(id.to_owned(), member);
        self.carry_out(id, action, now);
        if signed_out {
            self.stop_member_connection(id);
        } else if active {
            self.apply_member(id, MemberEvent::Activated, now);
        }
        self.reconcile(id);
    }

    /// Ends `id`'s membership. Its watches stop unless it is active; a connection already up stays,
    /// as it does for a cluster switched away from, and one still being made is dropped.
    fn leave_set(&mut self, id: &str) {
        self.members.remove(id);
        self.schedule.dequeue(&id.to_owned());
        let active = self.active.as_deref() == Some(id);
        let mut dropped = false;
        if let Some(slot) = self.cluster_mut(id) {
            let attempt = slot.member_attempt.take();
            if !active && matches!(slot.state, ConnectionState::Connecting) {
                if let Some(task) = slot.connect_task.take() {
                    task.abort();
                }
                slot.attempt = 0;
                slot.state = ConnectionState::Disconnected;
                dropped = true;
            }
            if let Some(attempt) = attempt {
                self.schedule.finish(attempt);
            }
        }
        if dropped {
            self.emit(Event::ConnectionChanged {
                cluster: id.to_owned(),
                state: ConnectionState::Disconnected,
            });
        }
        self.reconcile(id);
    }

    /// Whether `id`'s credential runs a program, which only the foreground connection may do.
    pub(super) fn runs_a_command(&self, id: &str) -> bool {
        let merged = &self.kubeconfig.merged;
        merged
            .contexts
            .iter()
            .find(|c| c.name == id)
            .and_then(|c| c.context.as_ref())
            .and_then(|c| c.user.as_ref())
            .and_then(|user| merged.auth_infos.iter().find(|a| &a.name == user))
            .and_then(|a| a.auth_info.as_ref())
            .is_some_and(|auth| {
                matches!(
                    CredentialSource::of(auth),
                    CredentialSource::Plugin | CredentialSource::AuthProvider
                )
            })
    }

    /// Gives each slot's credential the role it has now: background for a member that is not
    /// active, foreground otherwise.
    pub(super) fn sync_roles(&mut self) {
        let active = self.active.clone();
        let members: Vec<ClusterId> = self.members.keys().cloned().collect();
        for slot in &self.clusters {
            let background = active.as_deref() != Some(slot.info.name.as_str())
                && members.contains(&slot.info.name);
            slot.access.set_role(if background {
                Role::Background
            } else {
                Role::Foreground
            });
        }
    }

    /// Applies `event` to member `id`, if it is one, and carries out the answer.
    pub(super) fn apply_member(&mut self, id: &str, event: MemberEvent, now: Instant) {
        let mut jitter = SetJitter;
        let Some(member) = self.members.get_mut(id) else {
            return;
        };
        let action = member.apply(event, now, &mut jitter);
        self.carry_out(id, action, now);
    }

    fn carry_out(&mut self, id: &str, action: MemberAction, now: Instant) {
        let key = id.to_owned();
        match action {
            MemberAction::None => {}
            MemberAction::Enqueue => self.schedule.enqueue(key),
            MemberAction::Dequeue => self.schedule.dequeue(&key),
            MemberAction::StartNow => {
                let attempt = self.schedule.start_active(&key);
                let started = self
                    .members
                    .get_mut(id)
                    .is_some_and(|member| member.begin_attempt(attempt));
                if started {
                    self.start_member_attempt(id, attempt, now);
                }
            }
            MemberAction::StopWatches => self.stop_member_connection(id),
            MemberAction::Remove => self.leave_set(id),
        }
    }

    /// Starts every background attempt the schedule allows now.
    pub(super) fn pump_members(&mut self, now: Instant) {
        while let Some(admission) = self.schedule.admit(now) {
            let started = self
                .members
                .get_mut(&admission.key)
                .is_some_and(|member| member.begin_attempt(admission.attempt));
            if started {
                self.start_member_attempt(&admission.key, admission.attempt, now);
            } else {
                self.schedule.finish(admission.attempt);
            }
        }
    }

    /// Runs `attempt` on the slot: a connection already up counts as its result, and one already
    /// being made is adopted rather than started twice.
    fn start_member_attempt(&mut self, id: &str, attempt: AttemptId, now: Instant) {
        let Some(slot) = self.cluster_mut(id) else {
            self.schedule.finish(attempt);
            return;
        };
        if matches!(slot.state, ConnectionState::Connected(_)) {
            let discover = slot.discovery.is_none() && !slot.discovering;
            self.finish_member_attempt(id, Some(attempt), AttemptOutcome::Connected, now);
            if discover {
                self.start_discovery(id);
            }
            self.reconcile(id);
            return;
        }
        slot.member_attempt = Some(attempt);
        self.ensure_connected(id);
    }

    /// Records the result of a member's attempt, if the finished connection was one.
    pub(super) fn finish_member_attempt(
        &mut self,
        id: &str,
        attempt: Option<AttemptId>,
        outcome: AttemptOutcome,
        now: Instant,
    ) {
        let Some(attempt) = attempt else {
            return;
        };
        self.schedule.finish(attempt);
        self.apply_member(id, MemberEvent::AttemptFinished { attempt, outcome }, now);
    }

    /// Drops the connection of a member that failed or needs a sign-in, reporting why.
    fn stop_member_connection(&mut self, id: &str) {
        let reason = match self.members.get(id).map(Member::state) {
            Some(MemberState::Failed { error, .. }) => error.clone(),
            Some(MemberState::SignInNeeded { reason }) => reason.clone(),
            _ => return,
        };
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if let Some(task) = slot.connect_task.take() {
            task.abort();
        }
        // No attempt has id 0, so a result already queued is ignored, a discovery's included.
        slot.attempt = 0;
        slot.discovering = false;
        let attempt = slot.member_attempt.take();
        slot.watches.set_source(None);
        slot.client = None;
        slot.write_credential = None;
        slot.state = ConnectionState::Failed(reason.clone());
        if let Some(attempt) = attempt {
            self.schedule.finish(attempt);
        }
        self.emit(Event::ConnectionChanged {
            cluster: id.to_owned(),
            state: ConnectionState::Failed(reason),
        });
        self.reconcile(id);
    }

    /// `id` just moved to the background. A credential that runs a program may not go on: a
    /// connection that is up ends as signed out, and one still being made, which may have
    /// started before it joined and so under no policy, is replaced by a queued attempt that
    /// the policy refuses.
    pub(super) fn demote_member(&mut self, id: &str, now: Instant) {
        if !self.members.contains_key(id) || !self.runs_a_command(id) {
            return;
        }
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if !matches!(slot.state, ConnectionState::Connecting) {
            let reason = SafeErrorText::reduced(SIGN_IN_IN_FRONT);
            self.apply_member(id, MemberEvent::CredentialLost { reason }, now);
            return;
        }
        if let Some(task) = slot.connect_task.take() {
            task.abort();
        }
        slot.attempt = 0;
        slot.state = ConnectionState::Disconnected;
        let attempt = slot.member_attempt.take();
        if let Some(attempt) = attempt {
            self.schedule.finish(attempt);
        }
        self.apply_member(id, MemberEvent::ConnectionLost, now);
    }

    /// Brings the members' pauses in line with the manager's. Pauses are applied before
    /// resumes, so a change from one reason to the other never looks like a pause ending.
    pub(super) fn sync_member_pauses(&mut self, now: Instant) {
        let ids: Vec<ClusterId> = self.members.keys().cloned().collect();
        for id in ids {
            let held = self.held(&id);
            // Kept through an unconfirmed move, a member must not connect to the server shown.
            let provisional = !held && (self.paused(&id) || !self.member_on_its_server(&id));
            for (pause, on) in [(Pause::Held, held), (Pause::Provisional, provisional)] {
                if on {
                    self.apply_member(&id, MemberEvent::Paused(pause), now);
                }
            }
            for (pause, on) in [(Pause::Held, held), (Pause::Provisional, provisional)] {
                if !on {
                    self.apply_member(&id, MemberEvent::Resumed(pause), now);
                }
            }
        }
    }

    /// Feeds members their watch health and the time, then starts what is due. The active
    /// cluster keeps its own retry rules: it neither times out nor retries by itself here.
    pub(super) fn tick_members(&mut self, now: Instant) {
        let ids: Vec<ClusterId> = self.members.keys().cloned().collect();
        for id in ids {
            if let Some((any_synced, error)) = self.watch_health(&id) {
                self.apply_member(&id, MemberEvent::Watches { any_synced, error }, now);
            }
            if self.active.as_deref() != Some(id.as_str()) {
                self.apply_member(&id, MemberEvent::Tick, now);
            }
        }
        self.pump_members(now);
    }

    /// Whether any running watch of `id` is synced, and the error of one reconnecting.
    fn watch_health(&self, id: &str) -> Option<(bool, Option<SafeErrorText>)> {
        let running = self.cluster(id)?.watches.running();
        if running.is_empty() {
            return None;
        }
        let states = self.hubs.states(id);
        let mut running_states = states.iter().filter(|(k, _)| running.contains_key(k));
        let mut any_synced = false;
        let mut error = None;
        for (_, state) in &mut running_states {
            match state {
                SyncState::Synced => any_synced = true,
                SyncState::Reconnecting { error: text } if error.is_none() => {
                    error = Some(match text {
                        Some(text) => SafeErrorText::reduced(text),
                        None => SafeErrorText::reduced("reconnecting"),
                    });
                }
                _ => {}
            }
        }
        Some((any_synced, error))
    }

    /// Runs the tick while the set has members, and stops it when it has none.
    pub(super) fn arm_set_ticker(&mut self) {
        if self.members.is_empty() {
            if let Some(ticker) = self.set_ticker.take() {
                ticker.abort();
            }
            return;
        }
        if self.set_ticker.is_some() {
            return;
        }
        let internal = self.internal_tx.clone();
        self.set_ticker = Some(tokio::spawn(async move {
            let mut ticks = tokio::time::interval(SET_TICK);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                if internal.send(super::Internal::MemberTick).is_err() {
                    return;
                }
            }
        }));
    }

    /// Each member and its state, in kubeconfig order.
    pub(super) fn member_reports(&self) -> Vec<MemberReport> {
        self.clusters
            .iter()
            .filter_map(|slot| {
                let member = self.members.get(&slot.info.name)?;
                let status = match member.state() {
                    MemberState::Connecting => MemberStatus::Connecting,
                    MemberState::Live => MemberStatus::Live,
                    MemberState::Reconnecting { error, .. } => MemberStatus::Reconnecting {
                        error: error.clone(),
                    },
                    MemberState::SignInNeeded { reason } => MemberStatus::SignInNeeded {
                        reason: reason.clone(),
                    },
                    MemberState::Failed { error, .. } => MemberStatus::Failed {
                        error: error.clone(),
                    },
                };
                Some(MemberReport {
                    cluster: slot.info.name.clone(),
                    status,
                    paused: member.paused().is_some(),
                })
            })
            .collect()
    }
}
