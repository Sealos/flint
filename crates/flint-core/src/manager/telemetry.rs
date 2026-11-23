//! Runs a telemetry feed for each wanted cluster, on the connection it already has.
//!
//! [`ClusterManager::sync_telemetry`] compares what should run with what does after every message
//! of the manager's loop, so no path that pauses, drops or replaces a connection has to remember
//! the feed. A feed never connects: it starts only from a slot's live connection, and its client
//! is that connection's read client, so it is bound by the same credential grant.

// module-tests: ../manager.rs

use std::sync::{Arc, Mutex};

use super::{Cluster, ClusterManager};
use crate::cluster;
use crate::connection::policy::Role;
use crate::credential::CredentialSource;
use crate::handle::ConnectionState;
use crate::metrics_trend::TrendStore;
use crate::telemetry::Telemetry;

/// A running feed and the connection attempt it follows.
pub(super) struct SlotFeed {
    attempt: u64,
    /// Held to keep it running: dropping it stops its tasks.
    _feed: Telemetry,
}

impl ClusterManager {
    /// Whether `id` should have a feed now: it is selected or in the running set, its connection
    /// is up and not paused, and that connection's credential can go on being used in its role.
    /// A credential that runs a program is used only in the foreground.
    fn wants_telemetry(&self, id: &str) -> bool {
        if !self.wants_watches(id) {
            return false;
        }
        let Some(slot) = self.cluster(id) else {
            return false;
        };
        let Some(credential) = slot.write_credential.as_ref() else {
            return false;
        };
        if slot.client.is_none()
            || !matches!(slot.state, ConnectionState::Connected(_))
            || credential.spent()
        {
            return false;
        }
        // The slot's access answers for a revoked grant, which the role computed here can't show.
        if credential.runs_a_command() && !slot.access.grant(CredentialSource::Plugin).allowed() {
            return false;
        }
        match self.connection_role(id) {
            Role::Foreground => true,
            Role::Background => !credential.runs_a_command(),
            Role::Paused | Role::Retired => false,
        }
    }

    /// Stops every feed no longer wanted or on an older connection, and starts those missing.
    pub(super) fn sync_telemetry(&mut self) {
        for index in 0..self.clusters.len() {
            let id = self.clusters[index].info.name.clone();
            let wanted = self.wants_telemetry(&id);
            let slot = &mut self.clusters[index];
            if slot
                .telemetry
                .as_ref()
                .is_some_and(|feed| !wanted || feed.attempt != slot.attempt)
            {
                slot.telemetry = None;
            }
            if wanted && slot.telemetry.is_none() {
                self.start_telemetry(index, &id);
            }
        }
        let running: Vec<&str> = self
            .clusters
            .iter()
            .filter(|slot| slot.telemetry.is_some())
            .map(|slot| slot.info.name.as_str())
            .collect();
        self.telemetry.retain(|cluster| running.contains(&cluster));
    }

    fn start_telemetry(&mut self, index: usize, id: &str) {
        let target = cluster::Target::of(&self.kubeconfig.merged, id);
        let timing = self.telemetry_timing;
        let slot: &mut Cluster = &mut self.clusters[index];
        let Some(clients) = slot.client.as_ref() else {
            return;
        };
        // Its own store, so a stopping feed clearing its series can't touch its successor's.
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        let mut feed = Telemetry::start(id.into(), clients, target, Arc::clone(&trends), timing);
        self.telemetry.publish(id, &mut feed, trends);
        slot.telemetry = Some(SlotFeed {
            attempt: slot.attempt,
            _feed: feed,
        });
    }
}
