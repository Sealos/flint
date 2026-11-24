//! Runs a telemetry feed for each wanted cluster, on the connection it already has.
//!
//! [`ClusterManager::sync_telemetry`] compares what should run with what does after every message
//! of the manager's loop, so no path that pauses, drops or replaces a connection has to remember
//! the feed. A feed never connects: it starts only from a slot's live connection, and its client
//! is that connection's read client, so it is bound by the same credential grant.

// module-tests: ../manager.rs

use std::sync::{Arc, Mutex};

use kube::config::Kubeconfig;
use tokio::sync::watch;

use super::{Cluster, ClusterManager};
use crate::ClusterId;
use crate::cluster;
use crate::connection::policy::Role;
use crate::credential::CredentialSource;
use crate::handle::ConnectionState;
use crate::metrics_trend::TrendStore;
use crate::settings::WatchMode;
use crate::telemetry::Telemetry;

/// A running feed and the connection attempt it follows.
pub(super) struct SlotFeed {
    attempt: u64,
    /// Held to keep it running: dropping it stops its tasks.
    feed: Telemetry,
}

/// An overview showing, and the slot it was opened on.
pub(super) struct OpenOverview {
    cluster: ClusterId,
    generation: u64,
    /// Tells a bound overview whether it may read this slot's feed.
    live: Option<watch::Sender<bool>>,
}

impl ClusterManager {
    /// Records an overview open on `cluster`'s current connection generation. A context that
    /// starts over needs a new one, so an overview of its old definition starts nothing.
    pub(super) fn open_overview(&mut self, id: u64, cluster: ClusterId) {
        if let Some(generation) = self.cluster(&cluster).map(|slot| slot.generation) {
            let overview = OpenOverview {
                cluster,
                generation,
                live: None,
            };
            self.overviews.insert(id, overview);
        }
    }

    /// Records an overview only if `cluster` is still the slot `generation`.
    pub(super) fn open_overview_on(
        &mut self,
        id: u64,
        cluster: ClusterId,
        generation: u64,
        live: watch::Sender<bool>,
    ) {
        if self
            .cluster(&cluster)
            .is_some_and(|slot| slot.generation == generation)
        {
            live.send_replace(true);
            let overview = OpenOverview {
                cluster,
                generation,
                live: Some(live),
            };
            self.overviews.insert(id, overview);
        }
    }

    /// Ends every overview whose slot is gone, before a later slot's feed can be listed.
    fn end_replaced_overviews(&mut self) {
        let clusters = &self.clusters;
        self.overviews.retain(|_, overview| {
            let holds = clusters
                .iter()
                .any(|c| c.info.name == overview.cluster && c.generation == overview.generation);
            if !holds && let Some(live) = &overview.live {
                live.send_replace(false);
            }
            holds
        });
    }

    pub(super) fn close_overview(&mut self, id: u64) {
        self.overviews.remove(&id);
    }

    /// Whether the overview of `id` is showing: it is the active cluster, on the generation the
    /// overview was opened for.
    fn overview_showing(&self, id: &str) -> bool {
        self.active.as_deref() == Some(id)
            && self.cluster(id).is_some_and(|slot| {
                self.overviews
                    .values()
                    .any(|open| open.cluster == id && open.generation == slot.generation)
            })
    }

    /// Whether `id` should have a feed now: it is selected or in the running set, in watch mode
    /// All or showing its overview, its connection is up and not paused, and that connection's
    /// credential can go on being used in its role. A credential that runs a program is used
    /// only in the foreground.
    ///
    /// On view promises no watch of a kind without an open view. A feed watches seven kinds
    /// whatever is open, so in that mode it runs only for the overview, which is a view of them.
    pub(super) fn wants_telemetry(&self, id: &str) -> bool {
        if !self.wants_watches(id)
            || (self.settings.watch_mode_for(id) != WatchMode::All
                && !self.overview_showing(id)
                && !self.keep_watching(id))
        {
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
            // A member's connection made under background sign-in consent already runs its
            // plugin: its feed reads through the same client and serves keep watching.
            Role::Background => !credential.runs_a_command() || slot.access.unattended(),
            Role::Paused | Role::Retired => false,
        }
    }

    /// Stops every feed no longer wanted or on an older connection, and starts those missing.
    pub(super) fn sync_telemetry(&mut self) {
        self.stop_unwanted_telemetry();
        for index in 0..self.clusters.len() {
            let id = self.clusters[index].info.name.clone();
            if self.clusters[index].telemetry.is_none() && self.wants_telemetry(&id) {
                self.start_telemetry(index, &id);
            }
        }
    }

    /// Stops every feed no longer wanted or on an older connection and takes it off the board.
    /// Starts nothing, so a load can call it before saying what changed.
    pub(super) fn stop_unwanted_telemetry(&mut self) {
        self.end_replaced_overviews();
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
        }
        let running: Vec<&str> = self
            .clusters
            .iter()
            .filter(|slot| slot.telemetry.is_some())
            .map(|slot| slot.info.name.as_str())
            .collect();
        let keep = &self.keep;
        self.telemetry
            .retain(|cluster| running.contains(&cluster) || keep.holds_board_entry(cluster));
    }

    /// The seconds a pod on `id` may stay Pending, at the server the context points to now.
    pub(super) fn pending_after(&self, id: &str) -> u32 {
        let server = self
            .cluster(id)
            .and_then(|slot| slot.info.server.as_deref());
        self.settings
            .thresholds_for(id, server)
            .get(crate::thresholds::ThresholdMetric::PendingPodSeconds)
    }

    /// Gives every running feed its Pending threshold, so a change applies without a restart.
    pub(super) fn apply_pending_thresholds(&self) {
        for slot in &self.clusters {
            if let Some(running) = &slot.telemetry {
                running
                    .feed
                    .set_pending_after(self.pending_after(&slot.info.name));
            }
        }
        self.apply_kept_pending_thresholds();
    }

    fn start_telemetry(&mut self, index: usize, id: &str) {
        let target = cluster::Target::of(&self.kubeconfig.merged, id);
        let timing = self.telemetry_timing;
        let pending_after = self.pending_after(id);
        let requests = self
            .telemetry
            .request_gate(reads_token_file(&self.kubeconfig.merged, id));
        let pin = self
            .contexts
            .pin(id, self.clusters[index].info.server.as_deref());
        let slot: &mut Cluster = &mut self.clusters[index];
        let generation = slot.generation;
        let Some(client) = slot.client.as_ref().map(|clients| clients.read.clone()) else {
            return;
        };
        // Its own store, so a stopping feed clearing its series can't touch its successor's.
        let trends = Arc::new(Mutex::new(TrendStore::new()));
        let store = Arc::clone(&trends);
        let mut feed = Telemetry::start(
            id.into(),
            client,
            target,
            store,
            timing,
            requests,
            pending_after,
        );
        self.telemetry
            .publish(id, generation, pin, &mut feed, trends);
        slot.telemetry = Some(SlotFeed {
            attempt: slot.attempt,
            feed,
        });
    }
}

/// Whether `context`'s user names a token file, which the client rereads as it expires.
pub(super) fn reads_token_file(config: &Kubeconfig, context: &str) -> bool {
    let user = config
        .contexts
        .iter()
        .find(|named| named.name == context)
        .and_then(|named| named.context.as_ref())
        .and_then(|context| context.user.as_deref());
    user.is_some_and(|user| {
        config.auth_infos.iter().any(|named| {
            named.name == user
                && named
                    .auth_info
                    .as_ref()
                    .is_some_and(|auth| auth.token_file.is_some())
        })
    })
}
