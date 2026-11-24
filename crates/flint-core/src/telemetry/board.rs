//! The feeds running now, readable from the front end without going through the manager's loop.

// module-tests: ../manager.rs

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use jiff::Timestamp;
use tokio::sync::watch;

use super::{
    Availability, CheckFailure, FailureCause, KeptCoverage, KeptDiscovery, RequestGate, SideReads,
    Telemetry, TelemetrySnapshot,
};
use crate::cluster::{ClusterId, SafeErrorText};
use crate::metrics_trend::TrendStore;
use crate::recheck::ContextPin;

/// `good` as last values: readable, its age said in the status, and worth no alert change, since
/// a pass that could not check the cluster clears nothing and raises nothing.
fn aged(good: &TelemetrySnapshot, reason: &str, now: Timestamp) -> Arc<TelemetrySnapshot> {
    let minutes = now.duration_since(good.at).as_secs().max(0) / 60;
    let age = match minutes {
        0 => "under a minute old".to_owned(),
        1 => "1 minute old".to_owned(),
        minutes => format!("{minutes} minutes old"),
    };
    let mut snapshot = good.clone();
    snapshot.availability = Availability::CannotBeChecked(Some(CheckFailure {
        cause: FailureCause::Failed,
        detail: SafeErrorText::reduced(&format!("{reason}; showing last values, {age}")),
    }));
    Arc::new(snapshot)
}

/// What is known about one cluster with a running feed.
#[derive(Clone, Debug)]
pub struct ClusterTelemetry {
    pub snapshot: Arc<TelemetrySnapshot>,
    /// Warning events, claims and usage, each with its own state.
    pub reads: SideReads,
    /// Where the catalog that may vouch for this feed's kinds comes from.
    pub discovery: FeedDiscovery,
    /// The slot this feed was started for. A summary of any other slot is not about this feed.
    pub generation: u64,
    trends: Arc<Mutex<TrendStore>>,
}

/// The discovery that belongs with a feed.
#[derive(Clone, Copy, Debug)]
pub enum FeedDiscovery {
    /// The feed runs on the cluster's slot, whose discovery applies.
    Slot,
    /// The feed runs on a kept-watching connection, and only that connection's discovery applies.
    Kept(KeptCoverage),
}

impl ClusterTelemetry {
    /// Runs `read` on this feed's trends, whose series are under the cluster's name. Keep it
    /// short: the feed's metrics poll waits on the same lock.
    pub fn with_trends<R>(&self, read: impl FnOnce(&TrendStore) -> R) -> R {
        read(&self.trends.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Wakes when any cluster's telemetry changes or a feed starts or stops.
#[derive(Clone, Debug)]
pub struct TelemetryUpdates(watch::Receiver<u64>);

impl TelemetryUpdates {
    /// Waits for the next change. The first call resolves at once. `false` once the core stopped.
    pub async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }
}

#[derive(Debug)]
struct Entry {
    latest: watch::Receiver<Arc<TelemetrySnapshot>>,
    side: watch::Receiver<SideReads>,
    trends: Arc<Mutex<TrendStore>>,
    /// The newest pass that could read the cluster, for showing it with its age.
    last_good: Arc<Mutex<Option<Arc<TelemetrySnapshot>>>>,
    /// The feed has stopped: its last values stay, in memory only, until the entry is removed.
    held: Option<SideReads>,
    /// The definition the feed's connection was on when it started.
    pin: ContextPin,
    /// A kept feed's own discovery. `None` for a feed on a slot.
    kept: Option<watch::Receiver<KeptCoverage>>,
    /// The slot this feed was started for.
    generation: u64,
}

impl Entry {
    /// What may vouch for this feed's kinds. A stopped feed's connection vouches for nothing.
    fn discovery(&self) -> FeedDiscovery {
        match &self.kept {
            None => FeedDiscovery::Slot,
            Some(_) if self.held.is_some() => FeedDiscovery::Kept(KeptCoverage::NONE),
            Some(found) => FeedDiscovery::Kept(*found.borrow()),
        }
    }

    /// `None` once the feed's tasks have ended, so a stopped feed is never read as live. A feed
    /// that can no longer read its cluster shows what it last read, with its age.
    fn read(&self) -> Option<ClusterTelemetry> {
        let last_good = || {
            self.last_good
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        };
        let telemetry = |snapshot, reads| ClusterTelemetry {
            snapshot,
            reads,
            discovery: self.discovery(),
            generation: self.generation,
            trends: Arc::clone(&self.trends),
        };
        if let Some(reads) = &self.held {
            let good = last_good()?;
            let snapshot = aged(&good, "sign-in needed", Timestamp::now());
            return Some(telemetry(snapshot, reads.clone()));
        }
        self.latest.has_changed().ok()?;
        let latest = self.latest.borrow().clone();
        let snapshot = match (&latest.availability, last_good()) {
            (Availability::CannotBeChecked(_), Some(good)) => {
                aged(&good, "cluster unreachable", Timestamp::now())
            }
            _ => latest,
        };
        Some(telemetry(snapshot, self.side.borrow().clone()))
    }
}

/// The running feeds by cluster. The manager writes it; clones read it.
#[derive(Clone, Debug)]
pub(crate) struct TelemetryBoard {
    feeds: Arc<Mutex<BTreeMap<ClusterId, Entry>>>,
    version: watch::Sender<u64>,
    /// Shared by every feed of this core whose credential is a token file.
    token_file_requests: RequestGate,
}

impl Default for TelemetryBoard {
    fn default() -> Self {
        Self {
            feeds: Arc::default(),
            version: watch::channel(0).0,
            token_file_requests: RequestGate::token_file_limit(),
        }
    }
}

impl TelemetryBoard {
    /// The gate for a feed, by whether its client rereads a token file.
    pub(crate) fn request_gate(&self, reads_token_file: bool) -> RequestGate {
        if reads_token_file {
            self.token_file_requests.clone()
        } else {
            RequestGate::default()
        }
    }

    fn feeds(&self) -> MutexGuard<'_, BTreeMap<ClusterId, Entry>> {
        self.feeds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lists `feed` as `cluster`'s, in place of any before it.
    pub(crate) fn publish(
        &self,
        cluster: &str,
        generation: u64,
        pin: ContextPin,
        feed: &mut Telemetry,
        trends: Arc<Mutex<TrendStore>>,
    ) {
        self.list(cluster, generation, pin, feed, trends, None);
    }

    /// Lists a kept connection's `feed`, which runs `discovery` for as long as it runs.
    pub(crate) fn publish_kept(
        &self,
        cluster: &str,
        generation: u64,
        pin: ContextPin,
        feed: &mut Telemetry,
        trends: Arc<Mutex<TrendStore>>,
        discovery: KeptDiscovery,
    ) {
        // A channel per feed: a result of an earlier connection has nowhere to land.
        let (found, kept) = watch::channel(KeptCoverage::NONE);
        feed.follow_discovery(discovery, found, self.version.clone());
        self.list(cluster, generation, pin, feed, trends, Some(kept));
    }

    fn list(
        &self,
        cluster: &str,
        generation: u64,
        pin: ContextPin,
        feed: &mut Telemetry,
        trends: Arc<Mutex<TrendStore>>,
        kept: Option<watch::Receiver<KeptCoverage>>,
    ) {
        let last_good = Arc::default();
        feed.notify_changes(self.version.clone(), Arc::clone(&last_good));
        let entry = Entry {
            latest: feed.latest.clone(),
            side: feed.side.clone(),
            trends,
            last_good,
            held: None,
            pin,
            kept,
            generation,
        };
        self.feeds().insert(cluster.to_owned(), entry);
        self.bump();
    }

    /// Keeps `cluster`'s last values after its feed stops, or removes it if it read nothing yet.
    pub(crate) fn hold(&self, cluster: &str) {
        {
            let mut feeds = self.feeds();
            if let Some(entry) = feeds.get_mut(cluster) {
                entry.held = Some(entry.side.borrow().clone());
                let empty = entry
                    .last_good
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_none();
                if empty {
                    feeds.remove(cluster);
                }
            }
        }
        self.bump();
    }

    /// Keeps only the clusters `keep` accepts.
    pub(crate) fn retain(&self, keep: impl Fn(&str) -> bool) {
        let removed = {
            let mut feeds = self.feeds();
            let before = feeds.len();
            feeds.retain(|cluster, _| keep(cluster));
            feeds.len() != before
        };
        if removed {
            self.bump();
        }
    }

    fn bump(&self) {
        self.version.send_modify(|version| *version += 1);
    }

    pub(crate) fn get(&self, cluster: &str) -> Option<ClusterTelemetry> {
        self.feeds().get(cluster).and_then(Entry::read)
    }

    /// [`Self::get`], only if the feed started on the endpoint of `pin`.
    pub(crate) fn get_on(&self, cluster: &str, pin: &ContextPin) -> Option<ClusterTelemetry> {
        self.feeds()
            .get(cluster)
            .filter(|entry| entry.pin.same_endpoint_as(pin))
            .and_then(Entry::read)
    }

    pub(crate) fn all(&self) -> Vec<ClusterTelemetry> {
        self.feeds().values().filter_map(Entry::read).collect()
    }

    pub(crate) fn updates(&self) -> TelemetryUpdates {
        let mut updates = self.version.subscribe();
        updates.mark_changed();
        TelemetryUpdates(updates)
    }
}
