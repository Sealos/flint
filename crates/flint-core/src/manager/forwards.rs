//! The running port-forwards, as the manager's loop sees them.
//!
//! Each is bound to the connection it started on. [`ClusterManager::sync_forwards`] runs after
//! every message of the loop and ends those that may not go on, so no path that drops a
//! connection, changes a definition or turns keep watching off has to remember them.

// module-tests: ../../../flint-e2e/tests/port_forward_keep.rs

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

use super::ClusterManager;
use crate::ClusterId;
use crate::credential::CredentialId;
use crate::handle::AdmittedMember;
use crate::port_forward::{
    ForwardFailure, ForwardFailureKind, ForwardGrant, ForwardState, gate_failure, unauthorized,
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
}

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
        admitted: Option<&Arc<AdmittedMember>>,
    ) -> Result<ForwardGrant, WriteError> {
        let (_, pin, credential) = self.bound_write_client(&cluster, server, None)?;
        if admitted.is_some_and(|member| {
            !member.holds(&cluster, &pin) || !self.members.contains_key(&cluster)
        }) {
            return Err(WriteRefusal::Stale { cluster }.into());
        }
        self.forward_gate(&cluster, &pin)?;
        self.forwards.next += 1;
        let id = self.forwards.next;
        let connection = credential.id();
        let (ended, ended_rx) = watch::channel(None);
        if !self.forwards.signals.add(id, &cluster, &connection, &ended) {
            return Err(WriteRefusal::Stopped.into());
        }
        let kept = self.keep_watching(&cluster);
        let in_set = self.members.contains_key(&cluster);
        self.forwards.live.insert(
            id,
            LiveForward {
                cluster,
                pin: pin.clone(),
                connection: connection.clone(),
                kept,
                in_set,
                ended,
            },
        );
        Ok(ForwardGrant {
            id,
            pin,
            connection,
            ended: ended_rx,
            signals: self.forwards.signals.clone(),
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
        let (ended, ended_rx) = watch::channel(None);
        self.forwards.next += 1;
        let id = self.forwards.next;
        let connection = credential.id();
        assert!(self.forwards.signals.add(id, cluster, &connection, &ended));
        self.forwards.live.insert(
            id,
            LiveForward {
                cluster: cluster.to_owned(),
                pin: ContextPin::stale(),
                connection,
                kept: false,
                in_set: false,
                ended,
            },
        );
        ended_rx
    }

    /// Whether forward `id` may go on. One already ended is refused.
    pub(super) fn check_forward(&self, id: u64) -> Result<(), ForwardFailure> {
        let live = self
            .forwards
            .live
            .get(&id)
            .ok_or_else(|| gate_failure(&WriteError::ConnectionChanged))?;
        self.live_forward_gate(live).map_err(|e| gate_failure(&e))
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
        self.forwards.signals.remove(id);
        if let Some(live) = self.forwards.live.remove(&id) {
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

    /// The proxy setting changed, so the connection every forward is bound to is going: each one
    /// ends, saying why. Returns how many did.
    pub(super) fn end_forwards_for_proxy(&mut self) -> usize {
        let ids: Vec<u64> = self.forwards.live.keys().copied().collect();
        for &id in &ids {
            let failure = ForwardFailure::new(
                ForwardFailureKind::Refused,
                "the proxy setting changed; start the forward again",
            );
            self.end_forward(id, ForwardState::Failed(failure));
        }
        ids.len()
    }

    /// Ends the forwards that may not go on, and forgets those that are over.
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
        for (id, live) in &self.forwards.live {
            if live.ended.is_closed() {
                over.push((*id, None));
                continue;
            }
            let kept = self.keep_watching(&live.cluster);
            if let Err(refusal) = self.live_forward_gate(live) {
                over.push((*id, Some(ForwardState::Failed(gate_failure(&refusal)))));
            } else if !kept && (switched || live.kept || self.left_its_set(live)) {
                over.push((*id, Some(ForwardState::Stopped)));
            } else if kept && !live.kept {
                now_kept.push(*id);
            }
        }
        for id in now_kept {
            if let Some(live) = self.forwards.live.get_mut(&id) {
                live.kept = true;
            }
        }
        for (id, end) in over {
            self.forwards.signals.remove(id);
            if let (Some(live), Some(end)) = (self.forwards.live.remove(&id), end) {
                live.ended.send_replace(Some(end));
            }
        }
    }
}
