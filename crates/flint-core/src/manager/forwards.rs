//! The running port-forwards, as the manager's loop sees them.
//!
//! Each is bound to the connection it started on. [`ClusterManager::sync_forwards`] runs after
//! every message of the loop and ends those that may not go on, so no path that drops a
//! connection, changes a definition or turns keep watching off has to remember them.

// module-tests: ../../../flint-e2e/tests/port_forward_keep.rs

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::watch;

use super::ClusterManager;
use crate::ClusterId;
use crate::credential::CredentialId;
use crate::handle::AdmittedMember;
use crate::port_forward::{ForwardFailure, ForwardGrant, ForwardState, gate_failure};
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

#[derive(Default)]
pub(super) struct Forwards {
    next: u64,
    /// The active cluster when last looked at.
    active: Option<ClusterId>,
    live: HashMap<u64, LiveForward>,
}

impl ClusterManager {
    /// Admits a forward on `cluster`'s current connection, in one step with the gate.
    pub(super) fn admit_forward(
        &mut self,
        cluster: ClusterId,
        server: Option<Option<String>>,
        admitted: Option<&Arc<AdmittedMember>>,
    ) -> Result<ForwardGrant, WriteError> {
        let (_, pin, credential) = self.bound_write_client(&cluster, server)?;
        if admitted.is_some_and(|member| !member.holds(&cluster, &pin)) {
            return Err(WriteRefusal::Stale { cluster }.into());
        }
        self.forward_gate(&cluster, &pin)?;
        self.forwards.next += 1;
        let id = self.forwards.next;
        let connection = credential.id();
        let (ended, ended_rx) = watch::channel(None);
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
        })
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
            if let (Some(live), Some(end)) = (self.forwards.live.remove(&id), end) {
                live.ended.send_replace(Some(end));
            }
        }
    }
}
