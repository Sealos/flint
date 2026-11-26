//! The running port-forwards, as the manager's loop sees them.
//!
//! Each is bound to the connection it started on. [`ClusterManager::sync_forwards`] runs after
//! every message of the loop and ends those that may not go on, so no path that drops a
//! connection, changes a definition or turns keep watching off has to remember them.
//!
//! A proxy change is the one move from a connection to another: each forward is held, keeping
//! its local port, until its cluster is back on the new setting. It is then bound to the new
//! connection only if that is the same definition on the same server, unchanged meanwhile, and
//! the gate lets it run on its new route as for a new forward; otherwise it ends, saying why.

// module-tests: ../../../flint-e2e/tests/port_forward_keep.rs

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

use super::ClusterManager;
use super::proxy_change::Outcome;
use crate::ClusterId;
use crate::credential::CredentialId;
use crate::handle::{AdmittedMember, ForwardReconnect};
use crate::keep_watching::KeepWatching;
use crate::port_forward::{
    DialError, ForwardBinding, ForwardFailure, ForwardFailureKind, ForwardGrant, ForwardState,
    REBINDING, gate_failure, unauthorized,
};
use crate::protection::WriteRefusal;
use crate::recheck::ContextPin;
use crate::write::WriteError;

struct LiveForward {
    cluster: ClusterId,
    pin: ContextPin,
    connection: CredentialId,
    /// Its cluster was kept watching at some point of its life.
    kept: bool,
    /// Its cluster was a running-set member when it started.
    in_set: bool,
    ended: watch::Sender<Option<ForwardState>>,
    /// What its task opens connections on: `pin` and `connection`, or that it waits.
    binding: watch::Sender<ForwardBinding>,
    /// What it forwards, for a proxy change's report.
    target: String,
    /// The port it listens on, `0` until it does.
    local_port: Arc<AtomicU16>,
    /// Held by a proxy change: its cluster's definition revision when the change began, which
    /// must not have moved when it is bound again.
    rebinding: Option<u64>,
}

impl LiveForward {
    /// How it came back from a proxy change: `ended` says why it did not.
    fn outcome(&self, ended: Option<String>) -> ForwardReconnect {
        let port = self.local_port.load(Ordering::Relaxed);
        ForwardReconnect {
            cluster: self.cluster.clone(),
            target: self.target.clone(),
            local_port: (port != 0).then_some(port),
            ended,
        }
    }
}

/// What a forward held by a proxy change is bound to next, or why it ends; `None` while its
/// cluster may still come back.
type Rebind = Option<Result<(ContextPin, CredentialId), ForwardFailure>>;

/// How to end running forwards without the manager's queue, which a contended settings save can
/// hold up. Shared with [`crate::CoreHandle`].
#[derive(Clone, Default)]
pub(crate) struct ForwardSignals(Arc<Mutex<Signals>>);

#[derive(Default)]
struct Signals {
    quitting: bool,
    live: HashMap<u64, Signal>,
}

struct Signal {
    cluster: ClusterId,
    connection: CredentialId,
    ended: watch::Sender<Option<ForwardState>>,
}

/// Why a forward a proxy change held ended, from what ended it.
fn ended_text(end: Option<&ForwardState>) -> String {
    match end {
        Some(ForwardState::Failed(failure)) => failure.message.clone(),
        _ => "it was stopped".to_owned(),
    }
}

impl ForwardSignals {
    fn lock(&self) -> MutexGuard<'_, Signals> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ends every forward on `cluster`, now.
    pub(crate) fn end_cluster(&self, cluster: &str) {
        for signal in self.lock().live.values() {
            if signal.cluster == cluster {
                signal.ended.send_replace(Some(ForwardState::Stopped));
            }
        }
    }

    /// The server refused `connection`'s sign-in: ends every forward on it, now.
    pub(crate) fn end_connection(&self, connection: &CredentialId) {
        for signal in self.lock().live.values() {
            if signal.connection.is(connection) {
                signal
                    .ended
                    .send_replace(Some(ForwardState::Failed(unauthorized())));
            }
        }
    }

    /// Ends every forward, now, and admits no more.
    pub(crate) fn quit(&self) {
        let mut signals = self.lock();
        signals.quitting = true;
        for signal in signals.live.values() {
            signal.ended.send_replace(Some(ForwardState::Stopped));
        }
    }

    /// Records a forward; `false` once quitting.
    fn add(
        &self,
        id: u64,
        cluster: &str,
        connection: &CredentialId,
        ended: &watch::Sender<Option<ForwardState>>,
    ) -> bool {
        let mut signals = self.lock();
        if signals.quitting {
            return false;
        }
        signals.live.insert(
            id,
            Signal {
                cluster: cluster.to_owned(),
                connection: connection.clone(),
                ended: ended.clone(),
            },
        );
        true
    }

    fn remove(&self, id: u64) {
        self.lock().live.remove(&id);
    }

    /// Forward `id` is bound to `connection` now.
    fn rebound(&self, id: u64, connection: &CredentialId) {
        if let Some(signal) = self.lock().live.get_mut(&id) {
            signal.connection = connection.clone();
        }
    }
}

#[derive(Default)]
pub(super) struct Forwards {
    next: u64,
    /// The active cluster when last looked at.
    active: Option<ClusterId>,
    live: HashMap<u64, LiveForward>,
    signals: ForwardSignals,
}

impl ClusterManager {
    /// How to end forwards without going through the queue.
    pub(crate) fn forward_signals(&self) -> ForwardSignals {
        self.forwards.signals.clone()
    }

    /// Admits a forward on `cluster`'s current connection, in one step with the gate. One bound
    /// to a set's member needs that member to still be in the set.
    pub(super) fn admit_forward(
        &mut self,
        cluster: ClusterId,
        server: Option<Option<String>>,
        target: String,
        admitted: Option<&Arc<AdmittedMember>>,
    ) -> Result<ForwardGrant, WriteError> {
        let (_, pin, credential) = self.bound_write_client(&cluster, server, None)?;
        if admitted.is_some_and(|member| {
            !member.holds(&cluster, &pin) || !self.members.contains_key(&cluster)
        }) {
            return Err(WriteRefusal::Stale { cluster }.into());
        }
        self.forward_gate(&cluster, &pin)?;
        self.add_forward(cluster, pin, credential.id(), target)
            .ok_or_else(|| WriteRefusal::Stopped.into())
    }

    /// Records a forward on `cluster` bound to `pin` and `connection`; `None` once quitting.
    fn add_forward(
        &mut self,
        cluster: ClusterId,
        pin: ContextPin,
        connection: CredentialId,
        target: String,
    ) -> Option<ForwardGrant> {
        self.forwards.next += 1;
        let id = self.forwards.next;
        let (ended, ended_rx) = watch::channel(None);
        if !self.forwards.signals.add(id, &cluster, &connection, &ended) {
            return None;
        }
        let (binding, binding_rx) = watch::channel(ForwardBinding {
            pin: pin.clone(),
            connection: connection.clone(),
            rebinding: false,
            holds: 0,
        });
        let local_port = Arc::new(AtomicU16::new(0));
        let kept = self.keep_watching(&cluster);
        let in_set = self.members.contains_key(&cluster);
        self.forwards.live.insert(
            id,
            LiveForward {
                cluster,
                pin,
                connection,
                kept,
                in_set,
                ended,
                binding,
                target,
                local_port: Arc::clone(&local_port),
                rebinding: None,
            },
        );
        Some(ForwardGrant {
            id,
            binding: binding_rx,
            ended: ended_rx,
            signals: self.forwards.signals.clone(),
            local_port,
        })
    }

    /// A running forward on `cluster`, without a connection, and how it hears that it ended.
    #[cfg(test)]
    pub(super) fn forward_for_tests(
        &mut self,
        cluster: &str,
    ) -> watch::Receiver<Option<ForwardState>> {
        let credential = crate::credential::WriteCredential::new(
            Vec::new(),
            crate::credential::CredentialSource::Definition,
            None,
            None,
        );
        self.add_forward(
            cluster.to_owned(),
            ContextPin::stale(),
            credential.id(),
            "pod/web port 80 in demo".to_owned(),
        )
        .expect("not quitting")
        .ended
    }

    /// A forward on `cluster`'s current connection, listening on `port`, as admitted with no
    /// gate asked.
    #[cfg(test)]
    pub(super) fn bound_forward_for_tests(&mut self, cluster: &str, port: u16) -> ForwardGrant {
        let pin = self.session_pin(cluster);
        let connection = self
            .cluster(cluster)
            .and_then(|slot| slot.write_credential.as_ref())
            .expect("connected")
            .id();
        let grant = self
            .add_forward(
                cluster.to_owned(),
                pin,
                connection,
                "pod/web port 80 in demo".to_owned(),
            )
            .expect("not quitting");
        grant.local_port.store(port, Ordering::Relaxed);
        grant
    }

    /// Whether forward `id` may go on: a dropped connection while a proxy change holds it. One
    /// already ended is refused.
    pub(super) fn check_forward(&self, id: u64) -> Result<(), DialError> {
        let live = self
            .forwards
            .live
            .get(&id)
            .ok_or_else(|| DialError::Fatal(gate_failure(&WriteError::ConnectionChanged)))?;
        if live.rebinding.is_some() {
            return Err(DialError::Dropped(REBINDING.to_owned()));
        }
        self.live_forward_gate(live)
            .map_err(|e| DialError::Fatal(gate_failure(&e)))
    }

    fn live_forward_gate(&self, live: &LiveForward) -> Result<(), WriteError> {
        self.forward_gate(&live.cluster, &live.pin)?;
        let same_connection = self
            .cluster(&live.cluster)
            .and_then(|slot| slot.write_credential.as_ref())
            .is_some_and(|credential| credential.id().is(&live.connection));
        if same_connection {
            Ok(())
        } else {
            Err(WriteError::ConnectionChanged)
        }
    }

    /// Whether it started on a set's member that is now neither in the set nor active.
    fn left_its_set(&self, live: &LiveForward) -> bool {
        live.in_set
            && !self.members.contains_key(&live.cluster)
            && self.active.as_deref() != Some(live.cluster.as_str())
    }

    fn end_forward(&mut self, id: u64, end: ForwardState) {
        self.finish_forward(id, Some(end));
    }

    /// Forgets forward `id`, telling it `end` if it must end. One a proxy change held goes in
    /// that change's report.
    fn finish_forward(&mut self, id: u64, end: Option<ForwardState>) {
        self.forwards.signals.remove(id);
        let Some(live) = self.forwards.live.remove(&id) else {
            return;
        };
        if live.rebinding.is_some() {
            let why = match &end {
                Some(end) => ended_text(Some(end)),
                None => ended_text(live.ended.borrow().as_ref()),
            };
            self.note_forward(live.outcome(Some(why)));
        }
        if let Some(end) = end {
            live.ended.send_replace(Some(end));
        }
    }

    /// A kubeconfig file was rewritten. Edits can cancel out between two reads, so what the
    /// files show now says nothing of what they defined meanwhile: every forward ends.
    pub(super) fn invalidate_forwards(&mut self) {
        let ids: Vec<u64> = self.forwards.live.keys().copied().collect();
        for id in ids {
            let failure = gate_failure(&WriteError::ConnectionChanged);
            self.end_forward(id, ForwardState::Failed(failure));
        }
    }

    /// The proxy setting changed, so the connection every forward is bound to is going: each
    /// one keeps its port and waits for its cluster to come back on the new setting. One held by
    /// an earlier change waits on. Returns how many wait.
    pub(super) fn hold_forwards_for_proxy(&mut self) -> usize {
        let contexts = &self.contexts;
        for live in self.forwards.live.values_mut() {
            if live.rebinding.is_none() {
                live.rebinding = Some(contexts.definition_revision(&live.cluster));
                live.binding.send_modify(|binding| {
                    binding.rebinding = true;
                    binding.holds += 1;
                });
            }
        }
        self.forwards.live.len()
    }

    /// Whether a forward still waits to be bound again after a proxy change.
    pub(super) fn forwards_rebinding(&self) -> bool {
        self.forwards
            .live
            .values()
            .any(|live| live.rebinding.is_some())
    }

    /// Ends each forward still waiting to be bound again, for `why`.
    pub(super) fn end_rebinding_forwards(&mut self, why: &str) {
        let waiting: Vec<(u64, String)> = self
            .forwards
            .live
            .iter()
            .filter(|(_, live)| live.rebinding.is_some())
            .map(|(id, live)| (*id, format!("{} {why}", live.cluster)))
            .collect();
        for (id, why) in waiting {
            let failure = ForwardFailure::new(ForwardFailureKind::Refused, why);
            self.end_forward(id, ForwardState::Failed(failure));
        }
    }

    /// What forward `live`, held by a proxy change since its cluster's definition was at
    /// `revision`, is bound to next. Only to the same definition on the same server, unchanged
    /// since, and only through the gate a new forward takes: the new route must be unlocked or
    /// trusted on its own.
    fn rebind(&self, live: &LiveForward, revision: u64) -> Rebind {
        let cluster = live.cluster.as_str();
        let Some(slot) = self.cluster(cluster) else {
            let removed = WriteRefusal::Removed {
                cluster: cluster.to_owned(),
            };
            return Some(Err(gate_failure(&removed.into())));
        };
        if self.held(cluster) {
            return None;
        }
        if let Outcome::Failed(why) = self.proxy_outcome(cluster)? {
            return Some(Err(ForwardFailure::new(
                ForwardFailureKind::Refused,
                format!("{cluster} did not reconnect on the new proxy setting: {why}"),
            )));
        }
        // Back with a read feed only, as a cluster watched in the background is: nothing to
        // forward on.
        let Some(credential) = slot.write_credential.as_ref() else {
            return Some(Err(ForwardFailure::new(
                ForwardFailureKind::Refused,
                format!(
                    "{cluster} reconnected for watching only; select it and start the forward \
                     again"
                ),
            )));
        };
        let pin = self.session_pin(cluster);
        let same = pin.same_definition_on_any_route(&live.pin)
            && self.contexts.definition_revision(cluster) == revision;
        if !same {
            return Some(Err(ForwardFailure::new(
                ForwardFailureKind::Refused,
                format!(
                    "{cluster}'s connection changed while it reconnected; start the forward again"
                ),
            )));
        }
        // A set's member was admitted on its old route, which a set's forward stays on.
        if live.in_set && pin != live.pin {
            return Some(Err(ForwardFailure::new(
                ForwardFailureKind::Refused,
                format!(
                    "{cluster} joined the running set on its old route; start the forward again"
                ),
            )));
        }
        if let Err(refusal) = self.forward_gate(cluster, &pin) {
            return Some(Err(gate_failure(&refusal)));
        }
        Some(Ok((pin, credential.id())))
    }

    /// Why a forward held by a proxy change ends once nothing keeps its cluster running.
    fn no_longer_kept(&self, cluster: &str) -> ForwardFailure {
        let why = match self.keep_status(cluster) {
            KeepWatching::NeedsConfirming => {
                "keep watching must be confirmed again for its new route"
            }
            KeepWatching::SignInNeeded => "it needs a sign-in to be watched in the background",
            _ => "it is no longer selected or watched in the background",
        };
        ForwardFailure::new(
            ForwardFailureKind::Refused,
            format!("{cluster} did not reconnect on the new proxy setting: {why}"),
        )
    }

    /// Binds forward `id` to `pin` and `connection`, and reports it reconnected.
    fn bind_forward(&mut self, id: u64, pin: ContextPin, connection: CredentialId) {
        self.forwards.signals.rebound(id, &connection);
        let Some(live) = self.forwards.live.get_mut(&id) else {
            return;
        };
        live.pin = pin.clone();
        live.connection = connection.clone();
        live.rebinding = None;
        live.binding.send_modify(|binding| {
            binding.pin = pin;
            binding.connection = connection;
            binding.rebinding = false;
        });
        let outcome = live.outcome(None);
        self.note_forward(outcome);
    }

    /// Ends the forwards that may not go on, binds those a proxy change held whose cluster is
    /// back, and forgets those that are over.
    pub(super) fn sync_forwards(&mut self) {
        let switched = self.forwards.active != self.active;
        if switched {
            self.forwards.active.clone_from(&self.active);
        }
        if self.forwards.live.is_empty() {
            return;
        }
        let mut over = Vec::new();
        let mut now_kept = Vec::new();
        let mut rebound = Vec::new();
        for (id, live) in &self.forwards.live {
            if live.ended.is_closed() {
                over.push((*id, None));
                continue;
            }
            let kept = self.keep_watching(&live.cluster);
            let unneeded = !kept && (switched || live.kept || self.left_its_set(live));
            if let Some(revision) = live.rebinding {
                // Its old connection is gone, so it asks no gate until it is bound again. Keep
                // watching paused until the files under the new setting settle may still resume.
                let resumable = self.keep_status(&live.cluster) == KeepWatching::Paused;
                if unneeded && !resumable {
                    let failure = self.no_longer_kept(&live.cluster);
                    over.push((*id, Some(ForwardState::Failed(failure))));
                    continue;
                }
                match self.rebind(live, revision) {
                    None => {}
                    Some(Ok(binding)) => rebound.push((*id, binding)),
                    Some(Err(failure)) => {
                        over.push((*id, Some(ForwardState::Failed(failure))));
                        continue;
                    }
                }
            } else if let Err(refusal) = self.live_forward_gate(live) {
                over.push((*id, Some(ForwardState::Failed(gate_failure(&refusal)))));
                continue;
            } else if unneeded {
                over.push((*id, Some(ForwardState::Stopped)));
                continue;
            }
            if kept && !live.kept {
                now_kept.push(*id);
            }
        }
        for id in now_kept {
            if let Some(live) = self.forwards.live.get_mut(&id) {
                live.kept = true;
            }
        }
        for (id, (pin, connection)) in rebound {
            self.bind_forward(id, pin, connection);
        }
        for (id, end) in over {
            self.finish_forward(id, end);
        }
    }
}
