//! The running set: clusters kept connected and watched beside the active one.
//!
//! [`Member`] decides what each cluster does and
//! [`Schedule`](crate::connection::schedule::Schedule) when a background attempt may start; this
//! carries out their answers on the cluster slots. Every attempt runs on its own task
//! and only its result comes back through the manager's loop, so a member that fails, hangs or
//! needs a sign-in holds up no other. Credentials that run a program run for a background member
//! only under its keep-watching consent, by that feature's rules (ADR-0012); without one it waits
//! as signed out until it is selected.
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
use crate::handle::{
    AdmittedMember, ConnectionState, Event, LeftOutMember, LeftOutReason, MemberReport,
    MemberStatus, OpenedSet,
};
use crate::recheck::Pinned;
use crate::settings::ClusterPin;
use crate::store::SyncState;

/// The sizes a running set may have; an empty list clears it.
pub(crate) const SET_SIZES: std::ops::RangeInclusive<usize> = 2..=5;

/// How often members' watch health, retries and queue are looked at.
const SET_TICK: Duration = Duration::from_millis(250);

const SIGN_IN_IN_FRONT: &str = "this cluster's sign-in runs a program; select it to connect";

const CREDENTIAL_REFUSED: &str = "the server refused this cluster's sign-in; select it to retry";

const CONSENT_SIGN_IN_FAILED: &str =
    "this cluster's background sign-in stopped; select it to sign in";

/// Why a member's connection made under consent ended.
pub(super) fn consent_ended() -> SafeErrorText {
    SafeErrorText::reduced(CONSENT_SIGN_IN_FAILED)
}

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

    /// Makes the running set exactly `pins`, each bound to its exact server. Every pin is checked
    /// before any member joins or leaves, so a refusal changes nothing.
    pub(super) fn admit_running_set(
        &mut self,
        pins: Vec<ClusterPin>,
    ) -> Result<Vec<AdmittedMember>, String> {
        let mut ids: Vec<ClusterId> = Vec::with_capacity(pins.len());
        for pin in &pins {
            if ids.contains(&pin.context) {
                return Err(format!("\"{}\" is in the set twice.", pin.context));
            }
            ids.push(pin.context.clone());
        }
        if !ids.is_empty() && !SET_SIZES.contains(&ids.len()) {
            return Err(format!(
                "A running set holds {} to {} clusters.",
                SET_SIZES.start(),
                SET_SIZES.end()
            ));
        }
        if let Some(problem) = self.set_problem(&ids) {
            return Err(problem);
        }
        let admitted = pins
            .into_iter()
            .map(|pin| self.admit_member(pin))
            .collect::<Result<Vec<_>, _>>()?;
        self.set_running_set(ids);
        Ok(admitted)
    }

    /// Classifies every pin and makes the running set those that can join, in one step, so no
    /// change can land between the two. Fewer than a set's minimum changes nothing and admits
    /// nobody. A malformed list is refused whole.
    pub(super) fn open_running_set(&mut self, pins: Vec<ClusterPin>) -> Result<OpenedSet, String> {
        let mut seen: Vec<&str> = Vec::with_capacity(pins.len());
        for pin in &pins {
            if seen.contains(&pin.context.as_str()) {
                return Err(format!("\"{}\" is in the set twice.", pin.context));
            }
            seen.push(&pin.context);
        }
        if !SET_SIZES.contains(&pins.len()) {
            return Err(format!(
                "A running set holds {} to {} clusters.",
                SET_SIZES.start(),
                SET_SIZES.end()
            ));
        }
        let mut members = Vec::with_capacity(pins.len());
        let mut left_out = Vec::new();
        for pin in pins {
            match self.classify_member(pin) {
                Ok(member) => members.push(member),
                Err(member) => left_out.push(member),
            }
        }
        if members.len() < *SET_SIZES.start() {
            return Ok(OpenedSet {
                members: Vec::new(),
                left_out,
            });
        }
        let ids: Vec<ClusterId> = members.iter().map(|m| m.context().to_owned()).collect();
        if let Some(problem) = self.set_problem(&ids) {
            return Err(problem);
        }
        self.set_running_set(ids);
        Ok(OpenedSet { members, left_out })
    }

    /// `pin` as an admitted member, or why it stays out. Anything [`Self::admit_member`] refuses
    /// stays out, so this is never less strict than admitting the pin alone.
    fn classify_member(&self, pin: ClusterPin) -> Result<AdmittedMember, LeftOutMember> {
        let id = pin.context.as_str();
        let reason = match self.cluster(id) {
            None => Some(LeftOutReason::Removed),
            Some(_) if self.paused(id) => Some(LeftOutReason::Paused),
            Some(slot)
                if self.unsettled(id)
                    || !self
                        .contexts
                        .shown(id, slot.info.server.as_deref())
                        .confirmed =>
            {
                Some(LeftOutReason::Provisional)
            }
            Some(_) => None,
        };
        let context = pin.context.clone();
        match reason {
            Some(reason) => Err(LeftOutMember { context, reason }),
            None => self.admit_member(pin).map_err(|_| LeftOutMember {
                context,
                reason: LeftOutReason::Changed,
            }),
        }
    }

    /// `pin` as an admitted member, if its context shows exactly that server, confirmed.
    fn admit_member(&self, pin: ClusterPin) -> Result<AdmittedMember, String> {
        let id = pin.context.as_str();
        let shown = self.cluster(id).and_then(|c| c.info.server.as_deref());
        if shown != Some(pin.server.as_str()) {
            return Err(format!("\"{id}\" now points at another server."));
        }
        if self.unsettled(id) {
            return Err(format!(
                "\"{id}\" can't join while a kubeconfig file defining it is unreadable."
            ));
        }
        let identity = ServerIdentity::new(pin.server.as_str());
        if self
            .members
            .get(id)
            .is_some_and(|member| *member.server() != identity)
        {
            return Err(format!("\"{id}\" is running on another server."));
        }
        let proxy = self.cluster(id).and_then(|c| c.info.proxy_url.as_deref());
        let at = self.contexts.pin_through(id, Some(&pin.server), proxy);
        if let Some(why) = self.contexts.check(Pinned::Logs, id, &at) {
            return Err(why);
        }
        let endpoint = self.contexts.shown(id, Some(&pin.server)).endpoint;
        Ok(AdmittedMember { pin, at, endpoint })
    }

    /// Why `member` no longer stands for its context, if so: it was removed, or moved, even if
    /// it has moved back, or now shows another server (a fallback included).
    pub(super) fn member_changed(&self, member: &AdmittedMember) -> Option<String> {
        let id = member.context();
        let changed = || format!("\"{id}\" changed since this set opened. Open the set again.");
        if self.contexts.check(Pinned::Logs, id, &member.at).is_some() {
            return Some(changed());
        }
        let shown = self.cluster(id).and_then(|c| c.info.server.as_deref());
        let on_pin = shown.is_some_and(|server| member.pin.matches(id, server));
        (!on_pin).then(changed)
    }

    /// Subscribes to `kind` on `member`, in one step with checking it still holds: a member that
    /// changed, or a handle bound to another context, starts no LIST or watch and gets no rows.
    pub(super) fn subscribe_member(
        &mut self,
        id: u64,
        cluster: &str,
        member: AdmittedMember,
        kind: crate::KindId,
        feed: crate::watch::MemberFeed,
    ) {
        self.member_feeds.insert(id, feed.clone());
        if member.context() != cluster || self.member_changed(&member).is_some() {
            return;
        }
        self.hubs.attach_member(cluster, &kind, id, feed);
        self.subscriptions
            .insert(id, (cluster.to_owned(), kind, Some(member)));
        self.reconcile(cluster);
    }

    /// Cuts the subscriptions whose member changed off from their hubs for good, on `only` or on
    /// every context, and ends their interest in the kind: a member that moves back does not
    /// bring them back. Returns the contexts that lost one, whose watches need reconciling.
    pub(super) fn cut_off_changed_members(&mut self, only: Option<&str>) -> Vec<ClusterId> {
        let changed: Vec<u64> = self
            .subscriptions
            .iter()
            .filter(|(_, (context, _, member))| {
                only.is_none_or(|c| c == context)
                    && member
                        .as_ref()
                        .is_some_and(|m| self.member_changed(m).is_some())
            })
            .map(|(id, _)| *id)
            .collect();
        let mut contexts = Vec::new();
        for id in changed {
            let Some((context, kind, _)) = self.subscriptions.remove(&id) else {
                continue;
            };
            self.hubs.detach_member(&context, &kind, id);
            self.release_hub_if_unwatched(&context, &kind);
            if !contexts.contains(&context) {
                contexts.push(context);
            }
        }
        contexts
    }

    /// Forgets the hub of `kind` if no watch runs for it and nobody reads it.
    pub(super) fn release_hub_if_unwatched(&self, cluster: &str, kind: &crate::KindId) {
        let running = self
            .cluster(cluster)
            .is_some_and(|slot| slot.watches.is_running(kind));
        if !running {
            self.hubs.release(cluster, kind);
        }
    }

    /// The read client of `member`, checked in the same step as it is taken.
    pub(super) fn member_client(&self, member: &AdmittedMember) -> Result<kube::Client, String> {
        if let Some(why) = self.member_changed(member) {
            return Err(why);
        }
        let id = member.context();
        self.client(id)
            .ok_or_else(|| format!("\"{id}\" is not connected"))
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
        let denied = (!active && self.runs_a_command(id) && self.member_consent(id).is_none())
            .then(|| SafeErrorText::reduced(SIGN_IN_IN_FRONT));
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
    /// as it does for a cluster switched away from, and one still being made is dropped. One made
    /// under consent is dropped either way, and the selected cluster connects again.
    fn leave_set(&mut self, id: &str) {
        self.members.remove(id);
        self.schedule.dequeue(&id.to_owned());
        let active = self.active.as_deref() == Some(id);
        let mut dropped = false;
        if let Some(slot) = self.cluster_mut(id) {
            let attempt = slot.member_attempt.take();
            let connecting = matches!(slot.state, ConnectionState::Connecting);
            // Consent covers members only: a connection made under it never outlives membership.
            let consented = slot.access.unattended()
                && (connecting || matches!(slot.state, ConnectionState::Connected(_)));
            if (!active && connecting) || consented {
                if let Some(task) = slot.connect_task.take() {
                    task.abort();
                }
                slot.clear_connection();
                slot.attempt = 0;
                slot.discovering = false;
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
            if active {
                self.ensure_connected(id);
            }
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
        self.unsettled(id) || self.definition_runs_a_command(id)
    }

    /// Whether the definition the kubeconfig shows for `id` now would run a program. A context
    /// with no user runs none, and one that can't be resolved counts as running one.
    pub(super) fn definition_runs_a_command(&self, id: &str) -> bool {
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

    /// The role a connection of `id` has now. A pause outranks the rest, and only the selected
    /// cluster is in the foreground. One switched away from keeps its token and sessions, and
    /// asks for a sign-in rather than run a command when the token is due.
    pub(super) fn connection_role(&self, id: &str) -> Role {
        let member = self.members.contains_key(id);
        if self.paused(id) || (member && !self.member_on_its_server(id)) {
            Role::Paused
        } else if self.active.as_deref() == Some(id) {
            Role::Foreground
        } else {
            Role::Background
        }
    }

    /// Gives each slot's connection the role it has now.
    pub(super) fn sync_roles(&mut self) {
        for slot in &self.clusters {
            slot.access.set_role(self.connection_role(&slot.info.name));
        }
    }

    /// Ends connections left unable to authenticate, once they could be replaced. They count as
    /// lost, so the usual paths connect them again. One switched away from outside the set stays
    /// for the sessions it still carries, until it is selected again.
    pub(super) fn drop_spent_connections(&mut self) {
        let spent: Vec<ClusterId> = self
            .clusters
            .iter()
            .filter(|slot| {
                let id = &slot.info.name;
                slot.write_credential
                    .as_ref()
                    .is_some_and(WriteCredential::spent)
                    && match self.connection_role(id) {
                        Role::Foreground => true,
                        Role::Background => self.members.contains_key(id),
                        Role::Paused | Role::Retired => false,
                    }
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
    /// asks the policy. A member with keep-watching consent keeps a connection made under it,
    /// and has any other replaced by a queued attempt under it.
    pub(super) fn demote_member(&mut self, id: &str, now: Instant) {
        if !self.runs_a_command(id) {
            return;
        }
        let Some(member) = self.members.get(id) else {
            return;
        };
        let member_connecting = *member.state() == MemberState::Connecting;
        let unsettled = self.unsettled(id);
        let consented = self.member_consent(id).is_some();
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if consented && slot.access.unattended() {
            return;
        }
        // Whatever happens to the connection below, it never runs its command again.
        slot.access.revoke();
        let replace = consented && slot.client.is_some();
        if !replace && !member_connecting && !matches!(slot.state, ConnectionState::Connecting) {
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

    /// The credential of `id`'s connection made under consent failed or was refused. In the
    /// background it waits for a sign-in, with no retry. Selected, it connects again in the
    /// foreground, where a prompt is expected. A connection still being made answers through
    /// its result.
    pub(super) fn member_credential_failed(&mut self, id: &str, attempt: u64) {
        let now = Instant::now();
        let active = self.active.as_deref() == Some(id);
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if slot.attempt != attempt || !matches!(slot.state, ConnectionState::Connected(_)) {
            return;
        }
        if !active {
            let reason = consent_ended();
            self.apply_member(id, MemberEvent::CredentialLost { reason }, now);
            return;
        }
        self.reconnect_selected(id);
    }

    /// Replaces the selected cluster's connection with a new one in the foreground.
    pub(super) fn reconnect_selected(&mut self, id: &str) {
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if let Some(task) = slot.connect_task.take() {
            task.abort();
        }
        if let Some(attempt) = slot.member_attempt.take() {
            self.schedule.finish(attempt);
        }
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        slot.clear_connection();
        // No attempt has id 0, so a discovery still running is ignored.
        slot.attempt = 0;
        slot.discovering = false;
        slot.reconnect = true;
        slot.state = ConnectionState::Disconnected;
        self.emit(Event::ConnectionChanged {
            cluster: id.to_owned(),
            state: ConnectionState::Disconnected,
        });
        self.reconnect_active(id);
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
