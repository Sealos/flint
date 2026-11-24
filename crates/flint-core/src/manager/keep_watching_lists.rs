//! Warm resource lists for clusters the person keeps watching (ADR-0012, #702).
//!
//! While a watched cluster's kept connection is up on the slot and whole definition it was
//! started for, the slot's resource watches read through that connection's read client instead
//! of the slot's own. They start on it while the cluster is selected and keep running when it is
//! left, so selecting it again lists nothing anew. The slot's connection is unchanged: it carries
//! every write, each through the gate with a fresh check, and the kept connection has no write
//! client at all.
//!
//! Whatever drops the kept connection (a changed definition or credential, a pause, ended
//! consent, a failure) drops the warm lists with it: a selected cluster's watches move back to
//! the slot's client and list again, and a background cluster's stop.
//!
//! In the background a cluster keeps only the kinds that were running when it was left, without
//! the high-churn ones, and at most [`WARM_LIST_CLUSTERS`] clusters keep theirs, the most
//! recently left first, on top of the per-kind object cap every watch has.

use std::collections::HashSet;
use std::time::Instant;

use super::ClusterManager;
use crate::ClusterId;
use crate::cluster;
use crate::handle::ConnectionState;
use crate::read::KindId;
use crate::watch::KubeSource;

/// How many clusters keep their resource lists warm in the background.
pub(super) const WARM_LIST_CLUSTERS: usize = 3;

/// Kinds a cluster stops watching in the background, because they change often and no open view
/// needs them there: they start again when it is selected.
const HIGH_CHURN: &[(&str, &str)] = &[
    ("", "events"),
    ("events.k8s.io", "events"),
    ("coordination.k8s.io", "leases"),
    ("discovery.k8s.io", "endpointslices"),
];

fn high_churn(kind: &KindId) -> bool {
    HIGH_CHURN
        .iter()
        .any(|(group, plural)| kind.group() == *group && kind.plural() == *plural)
}

/// A slot whose watches read through its kept connection.
pub(super) struct WarmLists {
    /// The kept connection whose read client the watches use.
    attempt: u64,
    /// In the background: the kinds kept running, chosen when the cluster was left.
    kinds: Option<HashSet<KindId>>,
    /// When the cluster was last left, to keep the most recent ones under the cap.
    left: Option<Instant>,
}

impl ClusterManager {
    /// Moves each slot's watches onto its kept connection, or back off it, to match what may run
    /// now, and keeps the background ones under the cap.
    pub(super) fn sync_warm_lists(&mut self) {
        let ids: Vec<ClusterId> = self.clusters.iter().map(|c| c.info.name.clone()).collect();
        for id in &ids {
            let wanted = self.warm_attempt(id);
            let current = self
                .cluster(id)
                .and_then(|slot| slot.warm.as_ref().map(|warm| warm.attempt));
            if wanted != current {
                self.set_warm(id, wanted);
            }
        }
        let now = Instant::now();
        let mut background: Vec<(Instant, ClusterId)> = self
            .clusters
            .iter()
            .filter(|slot| self.active.as_ref() != Some(&slot.info.name))
            .filter_map(|slot| {
                let left = slot.warm.as_ref()?.left.unwrap_or(now);
                Some((left, slot.info.name.clone()))
            })
            .collect();
        background.sort_by(|a, b| b.cmp(a));
        for (_, id) in background.into_iter().skip(WARM_LIST_CLUSTERS) {
            self.set_warm(&id, None);
        }
    }

    /// The kept connection `id`'s watches should read through, if any. Lists become warm only
    /// while the cluster is selected and its slot is connected, which says what the server
    /// supports; after that they stay warm on the same connection, in the background too and
    /// while the slot reconnects.
    fn warm_attempt(&self, id: &str) -> Option<u64> {
        let (attempt, _) = self.kept_read_client(id)?;
        let slot = self.cluster(id)?;
        let warm = slot
            .warm
            .as_ref()
            .is_some_and(|warm| warm.attempt == attempt);
        let selected = self.active.as_deref() == Some(id);
        let connected = matches!(slot.state, ConnectionState::Connected(_));
        (warm || (selected && connected)).then_some(attempt)
    }

    /// Puts `id`'s watches on kept connection `attempt`, or back on the slot's own connection
    /// with `None`. Either change starts the watches over on the new client.
    fn set_warm(&mut self, id: &str, attempt: Option<u64>) {
        let kept = attempt.and_then(|_| self.kept_read_client(id));
        let target = cluster::Target::of(&self.kubeconfig.merged, id);
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        let version = match &slot.state {
            ConnectionState::Connected(version) => Some(version.clone()),
            _ => None,
        };
        match kept {
            Some((attempt, client)) => {
                slot.watches
                    .set_source(Some(KubeSource::new(client, version.as_ref(), target)));
                slot.warm = Some(WarmLists {
                    attempt,
                    kinds: None,
                    left: None,
                });
            }
            None => {
                slot.warm = None;
                let source =
                    slot.client
                        .as_ref()
                        .zip(version.as_ref())
                        .map(|(clients, version)| {
                            KubeSource::new(clients.read.clone(), Some(version), target) as _
                        });
                slot.watches.set_source(source);
            }
        }
        self.reconcile(id);
    }

    /// For [`Self::reconcile`]: the kinds `id` keeps running in the background on warm lists,
    /// which are those running when it was left, without the high-churn ones. `None` when its
    /// lists are not warm in the background. A selected cluster forgets them.
    pub(super) fn warm_background_kinds(&mut self, id: &str) -> Option<HashSet<KindId>> {
        let selected = self.wants_watches(id);
        let slot = self.cluster_mut(id)?;
        let running = slot.watches.running();
        let warm = slot.warm.as_mut()?;
        if selected {
            warm.kinds = None;
            warm.left = None;
            return None;
        }
        if warm.kinds.is_none() {
            warm.left = Some(Instant::now());
            warm.kinds = Some(
                running
                    .into_keys()
                    .filter(|kind| !high_churn(kind))
                    .collect(),
            );
        }
        warm.kinds.clone()
    }

    /// Whether `id`'s watches read through its kept connection.
    #[cfg(test)]
    pub(super) fn lists_warm(&self, id: &str) -> bool {
        self.cluster(id).is_some_and(|slot| slot.warm.is_some())
    }
}

#[cfg(test)]
mod churn_tests {
    use super::*;

    #[test]
    fn events_leases_and_endpoint_slices_are_high_churn() {
        assert!(high_churn(&KindId::events()));
        assert!(high_churn(&KindId::new("events.k8s.io", "events")));
        assert!(high_churn(&KindId::new("coordination.k8s.io", "leases")));
        assert!(high_churn(&KindId::new(
            "discovery.k8s.io",
            "endpointslices"
        )));
        assert!(!high_churn(&KindId::pods()));
        assert!(!high_churn(&KindId::custom_resource_definitions()));
    }
}
