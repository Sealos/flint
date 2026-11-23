//! Whether a `helm` run may start on a cluster, and what it is held to.

// module-tests: ../../../flint-e2e/tests/helm_releases.rs

use super::ClusterManager;
use crate::cluster;
use crate::handle::AdmittedMember;
use crate::helm::{HelmError, HelmPin, HelmTarget, runs_root};
use crate::recheck::Pinned;
use crate::settings::expand_tilde;

impl ClusterManager {
    /// The pin of a Helm view opening on `id` now.
    pub(super) fn helm_pin(&self, id: &str) -> HelmPin {
        HelmPin {
            pin: self.session_pin(id),
            revision: self.contexts.definition_revision(id),
        }
    }

    /// The target of a `helm` run on `id` at `pin`, in one step with every check: the set member
    /// it is bound to, a settled and unpaused context, a live connection built from exactly the
    /// pinned definition, and no credential that runs a program. Protection is not asked: these
    /// runs only read.
    pub(super) fn helm_target(
        &self,
        id: &str,
        HelmPin { pin, revision }: HelmPin,
        member: Option<&AdmittedMember>,
    ) -> Result<HelmTarget, HelmError> {
        if member.is_some_and(|m| m.context() != id || self.member_changed(m).is_some()) {
            return Err(HelmError::Changed);
        }
        if self.held(id) || self.contexts.provisional(id) || self.paused(id) {
            return Err(HelmError::Unsettled);
        }
        // Before the connection: a changed context often has none, and this is why it is refused.
        let definition = pin.definition();
        let holds = definition.is_some()
            && self.contexts.definition_revision(id) == revision
            && self.contexts.check(Pinned::Shell, id, &pin).is_none()
            && !self.contexts.gate_moved(id, &pin)
            && self.contexts.definition_holds(id, &pin)
            && cluster::context_fingerprint(&self.kubeconfig.merged, id) == definition
            && member.is_none_or(|m| m.holds(id, &pin));
        if !holds {
            return Err(HelmError::Changed);
        }
        let slot = self.cluster(id).ok_or(HelmError::NotConnected)?;
        if slot.client.is_none() {
            return Err(HelmError::NotConnected);
        }
        if slot.fingerprint != definition {
            return Err(HelmError::Changed);
        }
        let credential = slot
            .write_credential
            .clone()
            .ok_or(HelmError::NotConnected)?;
        let home = std::env::home_dir();
        let configured = self
            .settings
            .helm
            .as_deref()
            .map(|path| expand_tilde(path, home.as_deref()));
        HelmTarget::new(
            &self.kubeconfig.merged,
            id,
            pin,
            credential,
            configured,
            runs_root(&self.config.config_dir),
        )
    }
}
