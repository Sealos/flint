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
use crate::credential::{CredentialSource, WriteCredential};
use crate::handle::{ConnectionState, Event, MemberReport, MemberStatus};
use crate::store::SyncState;

/// The sizes a running set may have; an empty list clears it.
pub(crate) const SET_SIZES: std::ops::RangeInclusive<usize> = 2..=5;

/// How often members' watch health, retries and queue are looked at.
const SET_TICK: Duration = Duration::from_millis(250);

const SIGN_IN_IN_FRONT: &str = "this cluster's sign-in runs a program; select it to connect";

const CREDENTIAL_REFUSED: &str = "the server refused this cluster's sign-in; select it to retry";

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

/// Whether any of `states` is synced, and the first reconnect error. A routine relist reconnects
/// with no error, and is not an outage.
pub(super) fn health_of<'a>(
    states: impl IntoIterator<Item = &'a SyncState>,
) -> (bool, Option<SafeErrorText>) {
    let mut any_synced = false;
    let mut error = None;
    for state in states {
        match state {
            SyncState::Synced => any_synced = true,
            SyncState::Reconnecting { error: Some(text) } if error.is_none() => {
                error = Some(SafeErrorText::reduced(text));
            }
            _ => {}
        }
    }
    (any_synced, error)
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
                slot.access.revoke();
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
    /// A retained connection answers for itself, since the kubeconfig may no longer show what it
    /// was made from. Without one, a definition that can't be known counts as running one.
    pub(super) fn runs_a_command(&self, id: &str) -> bool {
        if let Some(credential) = self.cluster(id).and_then(|s| s.write_credential.as_ref()) {
            return credential.runs_a_command();
        }
        if self.unsettled(id) {
            return true;
        }
        let merged = &self.kubeconfig.merged;
        let Some(context) = merged
            .contexts
            .iter()
            .find(|c| c.name == id)
            .and_then(|c| c.context.as_ref())
        else {
            return true;
        };
        let Some(user) = &context.user else {
            return false;
        };
        merged
            .auth_infos
            .iter()
            .find(|a| &a.name == user)
            .and_then(|a| a.auth_info.as_ref())
            .is_none_or(|auth| CredentialSource::of(auth).runs_a_command())
    }

    /// Asks for `id`'s next connection through its member state: at once for the active cluster,
    /// through the queue otherwise. A member that failed or needs a sign-in keeps its own rules.
    pub(super) fn request_member_attempt(&mut self, id: &str) {
        let now = Instant::now();
        let active = self.active.as_deref() == Some(id);
        let Some(member) = self.members.get(id) else {
            return;
        };
        match member.state() {
            MemberState::Live | MemberState::Reconnecting { .. } => {
                self.apply_member(id, MemberEvent::ConnectionLost, now);
            }
            MemberState::Connecting if !active && member.paused().is_none() => {
                self.schedule.enqueue(id.to_owned());
            }
            _ => {}
        }
        if active {
            self.apply_member(id, MemberEvent::Activated, now);
        } else {
            self.pump_members(now);
        }
    }

    /// Connects the active cluster again after a load replaced or released its slot. A member's
    /// new connection is bound to a foreground attempt, so its result reaches the member.
    pub(super) fn reconnect_active(&mut self, id: &str) {
        let due = self.members.contains_key(id)
            && !self.paused(id)
            && self.cluster(id).is_some_and(|slot| slot.reconnect);
        if due {
            let now = Instant::now();
            let attempt = self.cluster_mut(id).and_then(|s| s.member_attempt.take());
            if let Some(attempt) = attempt {
                self.schedule.finish(attempt);
            }
            self.apply_member(id, MemberEvent::ConnectionLost, now);
            self.apply_member(id, MemberEvent::Activated, now);
        }
        self.ensure_connected(id);
    }

    /// The role a connection of `id` has now. A pause outranks the rest, and a cluster outside
    /// the set stays in the foreground, as its log and shell sessions run while it is not active.
    pub(super) fn connection_role(&self, id: &str) -> Role {
        let member = self.members.contains_key(id);
        if self.paused(id) || (member && !self.member_on_its_server(id)) {
            Role::Paused
        } else if member && self.active.as_deref() != Some(id) {
            Role::Background
        } else {
            Role::Foreground
        }
    }

    /// Gives each slot's connection the role it has now.
    pub(super) fn sync_roles(&mut self) {
        for slot in &self.clusters {
            slot.access.set_role(self.connection_role(&slot.info.name));
        }
    }

    /// Ends connections a pause left unable to authenticate, once it is over. They count as lost,
    /// so the usual paths connect them again.
    pub(super) fn drop_spent_connections(&mut self) {
        let spent: Vec<ClusterId> = self
            .clusters
            .iter()
            .filter(|slot| {
                slot.write_credential
                    .as_ref()
                    .is_some_and(WriteCredential::spent)
                    && self.connection_role(&slot.info.name) != Role::Paused
            })
            .map(|slot| slot.info.name.clone())
            .collect();
        for id in spent {
            let Some(slot) = self.cluster_mut(&id) else {
                continue;
            };
            slot.clear_connection();
            // No attempt has id 0, so a discovery still running is ignored.
            slot.attempt = 0;
            slot.discovering = false;
            slot.reconnect = true;
            slot.state = ConnectionState::Disconnected;
            self.emit(Event::ConnectionChanged {
                cluster: id,
                state: ConnectionState::Disconnected,
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
        slot.clear_connection();
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
    /// connection that is up ends as signed out. One still being made, or held by a member not
    /// yet live, is replaced by a queued attempt that the policy refuses. With no connection and
    /// a definition that can't be known yet, nothing can run, and the attempt after the pause
    /// asks the policy.
    pub(super) fn demote_member(&mut self, id: &str, now: Instant) {
        if !self.runs_a_command(id) {
            return;
        }
        let Some(member) = self.members.get(id) else {
            return;
        };
        let member_connecting = *member.state() == MemberState::Connecting;
        let unsettled = self.unsettled(id);
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        // Whatever happens to the connection below, it never runs its command again.
        slot.access.revoke();
        if !member_connecting && !matches!(slot.state, ConnectionState::Connecting) {
            if slot.write_credential.is_some() || !unsettled {
                let reason = SafeErrorText::reduced(SIGN_IN_IN_FRONT);
                self.apply_member(id, MemberEvent::CredentialLost { reason }, now);
            }
            return;
        }
        if let Some(task) = slot.connect_task.take() {
            task.abort();
        }
        slot.attempt = 0;
        slot.discovering = false;
        slot.clear_connection();
        slot.state = ConnectionState::Disconnected;
        let attempt = slot.member_attempt.take();
        if let Some(attempt) = attempt {
            self.schedule.finish(attempt);
        }
        self.emit(Event::ConnectionChanged {
            cluster: id.to_owned(),
            state: ConnectionState::Disconnected,
        });
        self.apply_member(id, MemberEvent::ConnectionLost, now);
    }

    /// A pause voids an attempt in flight: its task ends, so a result from before the pause can
    /// never be taken for the connection made after it.
    fn void_attempt_for_pause(&mut self, id: &str) {
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if !matches!(slot.state, ConnectionState::Connecting) {
            return;
        }
        if let Some(task) = slot.connect_task.take() {
            task.abort();
        }
        slot.access.revoke();
        slot.attempt = 0;
        slot.state = ConnectionState::Disconnected;
        slot.reconnect = true;
        let attempt = slot.member_attempt.take();
        if let Some(attempt) = attempt {
            self.schedule.finish(attempt);
        }
        self.emit(Event::ConnectionChanged {
            cluster: id.to_owned(),
            state: ConnectionState::Disconnected,
        });
    }

    /// Brings the members' pauses in line with the manager's. Pauses are applied before
    /// resumes, so a change from one reason to the other never looks like a pause ending.
    pub(super) fn sync_member_pauses(&mut self, now: Instant) {
        let ids: Vec<ClusterId> = self.members.keys().cloned().collect();
        for id in ids {
            let held = self.held(&id);
            // Kept through an unconfirmed move, a member must not connect to the server shown.
            let provisional = !held && (self.paused(&id) || !self.member_on_its_server(&id));
            let was_paused = self.members.get(&id).is_some_and(|m| m.paused().is_some());
            for (pause, on) in [(Pause::Held, held), (Pause::Provisional, provisional)] {
                if on {
                    self.apply_member(&id, MemberEvent::Paused(pause), now);
                }
            }
            if !was_paused && (held || provisional) {
                self.void_attempt_for_pause(&id);
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
            self.take_refusal(&id, now);
            if let Some((any_synced, error)) = self.watch_health(&id) {
                self.apply_member(&id, MemberEvent::Watches { any_synced, error }, now);
            }
            if self.active.as_deref() != Some(id.as_str()) {
                self.apply_member(&id, MemberEvent::Tick, now);
            }
        }
        self.pump_members(now);
    }

    /// Acts on a 401 recorded on `id`'s connection since this was last asked. The active cluster
    /// has its own sign-in path, so there the record is only cleared.
    pub(super) fn take_refusal(&mut self, id: &str, now: Instant) {
        let refused = self
            .cluster(id)
            .and_then(|slot| slot.write_credential.as_ref())
            .is_some_and(WriteCredential::take_refused);
        if refused && self.active.as_deref() != Some(id) {
            let reason = SafeErrorText::reduced(CREDENTIAL_REFUSED);
            self.apply_member(id, MemberEvent::CredentialLost { reason }, now);
        }
    }

    /// Whether any running watch of `id` is synced, and the error of one reconnecting.
    fn watch_health(&self, id: &str) -> Option<(bool, Option<SafeErrorText>)> {
        let running = self.cluster(id)?.watches.running();
        if running.is_empty() {
            return None;
        }
        let states = self.hubs.states(id);
        Some(health_of(
            states
                .iter()
                .filter(|(kind, _)| running.contains_key(kind))
                .map(|(_, state)| state),
        ))
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
