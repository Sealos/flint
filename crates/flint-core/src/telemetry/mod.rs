//! A cluster's problems and node metrics, kept current with no view open (#448, #20). In watch
//! mode On view it runs only while the cluster's overview is showing, which is a view of its kinds.
//!
//! [`Telemetry`] runs typed watches of the seven kinds in [`WatchedKind`], classifies them with
//! [`Classifier`](crate::problems::Classifier), counts them with
//! [`ProblemSummariser`], and polls node metrics into a [`TrendStore`]. Each change is published
//! as a [`TelemetrySnapshot`].
//!
//! # Read-only, through the connection path
//!
//! A feed starts only from a gated connection and uses only its read client, so it cannot write,
//! and it can only follow a connection the manager was allowed to make: a paused context has
//! none to give. It lists and watches the seven kinds, reads node metrics and asks once for the
//! server version, nothing else.
//! The owner of a connection stops the feed when it pauses or drops that connection.
//!
//! # Missing data is never zero
//!
//! A kind is usable only once its list has loaded and its watch has opened. Until then, and
//! after a failure, its categories are unknown in [`TelemetrySnapshot::counts`] and its problem
//! kinds are in [`TelemetrySnapshot::unchecked`]. Warning events and pending claims are not
//! watched: they are polled into [`SideReads`], each with its own state, and those two categories
//! of the snapshot stay unknown.
//!
//! # Bounds
//!
//! Objects are cut down to what classification reads, and pods' requests and limits to per-node
//! totals, at most [`MAX_OBJECTS_PER_KIND`] per kind.
//! A kind over that limit is reported as [`KindCheck::TooLarge`] rather than counted in part.
//! Retries back off up to [`TelemetryTiming::backoff_max`], and lists run a few at a time.

mod board;
mod capacity;
mod checked;
mod feed;
mod kept_discovery;
mod metrics;
mod objects;
mod problems;
mod side;
mod usage;

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Node, Pod};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::JoinSet;

pub use self::checked::CheckedObjects;
use crate::alerts::ClusterSnapshot;
use crate::cluster::{SafeErrorText, Target};
use crate::dashboard::{ProblemCounts, ProblemSnapshot, ProblemSummariser};
use crate::metrics::MetricsError;
use crate::metrics_trend::TrendStore;
use crate::problems::{Problem, ProblemId, ProblemKind};

pub(crate) use board::TelemetryBoard;
pub use board::{ClusterTelemetry, FeedDiscovery, TelemetryUpdates};
pub use capacity::{Allocatable, ClusterCapacity, Inventory, NodeCapacity, Reserved};
use feed::KindFeed;
pub use kept_discovery::KeptCoverage;
pub(crate) use kept_discovery::{DiscoveryPace, KeptDiscovery};
pub use objects::MAX_OBJECTS_PER_KIND;
use objects::Stores;
use side::SideFeed;
pub use side::{PodKey, SideRead, SideReads, UsageRead};

/// Lists running at once per cluster, so that starting stays polite to the API server.
const CONCURRENT_LISTS: usize = 3;

/// Telemetry requests running at once across clusters whose credential is a token file. The
/// client rereads that file synchronously, so a stalled mount blocks the worker running the
/// request, and this leaves the core's other workers free.
const CONCURRENT_TOKEN_FILE_REQUESTS: usize = 2;

/// Limits the requests of feeds whose client can block a worker. Open for any other feed, so that
/// a slow cluster does not hold up the rest.
#[derive(Clone, Debug, Default)]
pub(crate) struct RequestGate(Option<Arc<Semaphore>>);

impl RequestGate {
    /// The limit that the token-file feeds of one runtime share.
    pub(crate) fn token_file_limit() -> Self {
        Self(Some(Arc::new(Semaphore::new(
            CONCURRENT_TOKEN_FILE_REQUESTS,
        ))))
    }

    /// Runs `request` once there is room. Time spent waiting counts against a caller's timeout.
    pub(crate) async fn run<F: Future>(&self, request: F) -> F::Output {
        let _permit = match &self.0 {
            Some(limit) => limit.acquire().await.ok(),
            None => None,
        };
        request.await
    }
}

/// A kind of object the feed watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WatchedKind {
    Pods,
    Nodes,
    Jobs,
    Deployments,
    StatefulSets,
    DaemonSets,
    ReplicaSets,
}

impl WatchedKind {
    /// Every watched kind.
    pub const ALL: [Self; 7] = [
        Self::Pods,
        Self::Nodes,
        Self::Jobs,
        Self::Deployments,
        Self::StatefulSets,
        Self::DaemonSets,
        Self::ReplicaSets,
    ];
}

/// Why a kind could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureCause {
    /// The server no longer accepts the credential, or it could not be obtained.
    LoginFailed,
    /// This identity may not list or watch the kind.
    Forbidden,
    /// The cluster does not serve the kind.
    NotServed,
    /// Anything else: a server error, a timeout, a dropped connection.
    Failed,
}

/// A real failure to read a kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckFailure {
    pub cause: FailureCause,
    pub detail: SafeErrorText,
}

/// Whether a kind's objects can be relied on right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum KindCheck {
    /// Its first list and watch have not finished.
    #[default]
    Starting,
    /// Loaded and being watched.
    Current,
    /// It could not be read. The feed keeps retrying.
    Failed(CheckFailure),
    /// It holds more than [`MAX_OBJECTS_PER_KIND`] objects.
    TooLarge,
}

/// Whether the cluster as a whole could be checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// Nothing has been read yet.
    Starting,
    /// Every watched kind is current.
    Checked,
    /// Some kinds are current, and the rest are in [`TelemetrySnapshot::kinds`].
    Partial,
    /// No kind could be read. Holds the first failure.
    CannotBeChecked(Option<CheckFailure>),
}

/// The state of the node metrics poll.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MetricsCheck {
    /// No poll has finished.
    #[default]
    Waiting,
    /// The last poll returned this many nodes.
    Current { nodes: usize },
    /// The last poll failed. [`MetricsError::NotInstalled`] is the case for an install hint.
    Unavailable(MetricsError),
}

/// The cluster's Kubernetes version, read once per feed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum VersionCheck {
    /// The request has not finished.
    #[default]
    Waiting,
    /// As the server reports it.
    Known(String),
    /// The request failed.
    Unknown,
}

/// What is known about one cluster at one moment.
#[derive(Clone, Debug)]
pub struct TelemetrySnapshot {
    pub cluster: Arc<str>,
    /// When this pass ran.
    pub at: Timestamp,
    pub availability: Availability,
    /// Each watched kind's state, in the order of [`WatchedKind::ALL`].
    pub kinds: [(WatchedKind, KindCheck); WatchedKind::ALL.len()],
    /// Problems of the kinds that could be read.
    pub problems: Arc<[Problem]>,
    /// Problem kinds this pass could not fully determine.
    pub unchecked: Vec<ProblemKind>,
    /// Problems in [`Self::problems`] not counted yet, because their hold-down has not passed.
    pub settling: Vec<ProblemId>,
    /// The objects this pass judged, so a list can tell a healthy row from one not seen yet.
    pub checked: Arc<CheckedObjects>,
    pub counts: ProblemCounts,
    pub metrics: MetricsCheck,
    /// `None` while nodes cannot be relied on.
    pub capacity: Option<ClusterCapacity>,
    pub inventory: Inventory,
    pub version: VersionCheck,
}

impl TelemetrySnapshot {
    fn starting(cluster: Arc<str>, at: Timestamp) -> Self {
        let mut summariser = ProblemSummariser::default();
        Self {
            cluster,
            at,
            availability: Availability::Starting,
            kinds: WatchedKind::ALL.map(|kind| (kind, KindCheck::Starting)),
            problems: Arc::from([]),
            unchecked: problems::ALL_PROBLEM_KINDS.to_vec(),
            settling: Vec::new(),
            checked: Arc::default(),
            counts: summariser.summarise(&ProblemSnapshot::unavailable(), at),
            metrics: MetricsCheck::Waiting,
            capacity: None,
            inventory: Inventory::default(),
            version: VersionCheck::Waiting,
        }
    }

    /// The state of `kind`.
    pub fn kind(&self, kind: WatchedKind) -> &KindCheck {
        &self.kinds[kind as usize].1
    }

    /// This pass as the alert engine takes it.
    pub fn alert_snapshot(&self) -> ClusterSnapshot<'_> {
        match self.availability {
            Availability::Checked | Availability::Partial => ClusterSnapshot::Available {
                problems: &self.problems,
                unchecked: &self.unchecked,
            },
            Availability::Starting | Availability::CannotBeChecked(_) => {
                ClusterSnapshot::Unavailable
            }
        }
    }

    /// Whether `other` says the same, whenever it was taken.
    fn same_as(&self, other: &Self) -> bool {
        self.availability == other.availability
            && self.kinds == other.kinds
            && self.problems == other.problems
            && self.unchecked == other.unchecked
            && self.settling == other.settling
            && self.checked == other.checked
            && self.counts == other.counts
            && self.metrics == other.metrics
            && self.capacity == other.capacity
            && self.inventory == other.inventory
            && self.version == other.version
    }
}

/// How often the feed acts. The default suits a real cluster; tests shorten it.
#[derive(Clone, Copy, Debug)]
pub struct TelemetryTiming {
    /// How often problems are judged again with no change, since their grace periods pass.
    pub evaluate_every: Duration,
    /// The shortest time between two passes, so a burst of events is one pass.
    pub evaluate_gap: Duration,
    /// How often node and pod metrics are read.
    pub metrics_every: Duration,
    /// How often Warning events and claims are listed.
    pub lists_every: Duration,
    /// The first delay after a failure.
    pub backoff_min: Duration,
    /// The longest delay after a failure.
    pub backoff_max: Duration,
    /// The longest one page of a list may take.
    pub list_timeout: Duration,
    /// The server-side timeout of a watch, in seconds.
    pub watch_timeout_secs: u32,
    /// How long a kind that lost its watch may keep its objects as current while it lists again.
    pub relist_recovery: Duration,
}

impl Default for TelemetryTiming {
    fn default() -> Self {
        Self {
            evaluate_every: Duration::from_secs(15),
            evaluate_gap: Duration::from_secs(1),
            metrics_every: crate::metrics::POLL_INTERVAL,
            lists_every: Duration::from_secs(30),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            list_timeout: Duration::from_secs(30),
            watch_timeout_secs: 290,
            relist_recovery: Duration::from_secs(60),
        }
    }
}

/// What the tasks of one feed share.
#[derive(Debug)]
struct Shared {
    stores: Mutex<Stores>,
    metrics: Mutex<MetricsCheck>,
    version: Mutex<VersionCheck>,
    /// Something changed, so the next pass should run.
    dirty: Notify,
    /// Seconds a pod may stay Pending before it is a problem. Read on every pass.
    pending_after: AtomicU32,
}

impl Shared {
    fn stores(&self) -> MutexGuard<'_, Stores> {
        self.stores.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_metrics(&self, check: MetricsCheck) {
        *self.metrics.lock().unwrap_or_else(PoisonError::into_inner) = check;
        self.dirty.notify_one();
    }

    fn set_version(&self, check: VersionCheck) {
        *self.version.lock().unwrap_or_else(PoisonError::into_inner) = check;
        self.dirty.notify_one();
    }

    fn version(&self) -> VersionCheck {
        self.version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn metrics(&self) -> MetricsCheck {
        self.metrics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One cluster's running feed. Dropping it stops every task; [`Telemetry::stop`] also waits for
/// them to end.
#[derive(Debug)]
pub struct Telemetry {
    latest: watch::Receiver<Arc<TelemetrySnapshot>>,
    side: watch::Receiver<SideReads>,
    shared: Arc<Shared>,
    tasks: JoinSet<()>,
}

impl Telemetry {
    /// Starts the feed of `cluster` on `client`, a read client. Must run inside a tokio runtime.
    /// This cluster's series are removed from `trends` when the feed stops, so a feed that
    /// replaces this one needs a store of its own. Every request goes through `requests`.
    pub(crate) fn start(
        cluster: Arc<str>,
        client: kube::Client,
        target: Target,
        trends: Arc<Mutex<TrendStore>>,
        timing: TelemetryTiming,
        requests: RequestGate,
        pending_after: u32,
    ) -> Self {
        let shared = Arc::new(Shared {
            stores: Mutex::default(),
            metrics: Mutex::default(),
            version: Mutex::default(),
            dirty: Notify::new(),
            pending_after: AtomicU32::new(pending_after),
        });
        let first = Arc::new(TelemetrySnapshot::starting(
            Arc::clone(&cluster),
            Timestamp::now(),
        ));
        let (sender, latest) = watch::channel(first);
        let (reads, side) = watch::channel(SideReads::default());
        let limiter = Arc::new(Semaphore::new(CONCURRENT_LISTS));

        let mut tasks = JoinSet::new();
        let polls = SideFeed {
            client: client.clone(),
            limiter: Arc::clone(&limiter),
            requests: requests.clone(),
            target: target.clone(),
            timing,
            reads: reads.clone(),
        };
        tasks.spawn(polls.clone().run_warning_events());
        tasks.spawn(polls.clone().run_pending_claims());
        tasks.spawn(polls.run_pod_usage());
        let kind = KindFeed {
            client: client.clone(),
            shared: Arc::clone(&shared),
            limiter,
            requests: requests.clone(),
            target,
            timing,
        };
        tasks.spawn(kind.clone().run::<Pod>());
        tasks.spawn(kind.clone().run::<Node>());
        tasks.spawn(kind.clone().run::<Job>());
        tasks.spawn(kind.clone().run::<Deployment>());
        tasks.spawn(kind.clone().run::<StatefulSet>());
        tasks.spawn(kind.clone().run::<DaemonSet>());
        tasks.spawn(kind.run::<ReplicaSet>());
        tasks.spawn(read_version(
            client.clone(),
            requests.clone(),
            Arc::clone(&shared),
            timing.list_timeout,
        ));
        tasks.spawn(metrics::run(
            Arc::clone(&cluster),
            client,
            requests,
            trends,
            Arc::clone(&shared),
            timing.metrics_every,
            reads,
        ));
        tasks.spawn(problems::run(cluster, Arc::clone(&shared), sender, timing));
        Self {
            latest,
            side,
            shared,
            tasks,
        }
    }

    /// Judges Pending pods against `seconds` from the next pass, which runs now if it changed.
    pub(crate) fn set_pending_after(&self, seconds: u32) {
        if self.shared.pending_after.swap(seconds, Ordering::Relaxed) != seconds {
            self.shared.dirty.notify_one();
        }
    }

    /// The most recent snapshot.
    pub fn latest(&self) -> Arc<TelemetrySnapshot> {
        self.latest.borrow().clone()
    }

    /// Warning events, claims and usage as last read.
    pub fn side_reads(&self) -> SideReads {
        self.side.borrow().clone()
    }

    /// Runs `discovery` for as long as this feed runs.
    fn follow_discovery(
        &mut self,
        discovery: KeptDiscovery,
        found: watch::Sender<KeptCoverage>,
        version: watch::Sender<u64>,
    ) {
        self.tasks.spawn(discovery.follow(found, version));
    }

    /// Counts each change of this feed on `version`, for as long as the feed runs.
    fn notify_changes(
        &mut self,
        version: watch::Sender<u64>,
        last_good: Arc<Mutex<Option<Arc<TelemetrySnapshot>>>>,
    ) {
        let (mut latest, mut side) = (self.latest.clone(), self.side.clone());
        self.tasks.spawn(async move {
            loop {
                let changed = tokio::select! {
                    changed = latest.changed() => changed,
                    changed = side.changed() => changed,
                };
                if changed.is_err() {
                    return;
                }
                let snapshot = latest.borrow().clone();
                if matches!(
                    snapshot.availability,
                    Availability::Checked | Availability::Partial
                ) {
                    *last_good
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
                }
                version.send_modify(|count| *count += 1);
            }
        });
    }

    /// Waits for a snapshot that differs from the last one seen. `false` if the feed has ended.
    pub async fn changed(&mut self) -> bool {
        self.latest.changed().await.is_ok()
    }

    /// Stops every task and waits until they have ended.
    pub async fn stop(mut self) {
        self.tasks.shutdown().await;
    }
}

/// Asks for the server's version once. A failure stays unknown until the feed starts again.
async fn read_version(
    client: kube::Client,
    requests: RequestGate,
    shared: Arc<Shared>,
    timeout: Duration,
) {
    let read = requests.run(crate::cluster::server_version(&client));
    let check = match tokio::time::timeout(timeout, read).await {
        Ok(Ok(version)) => VersionCheck::Known(version.git_version),
        Ok(Err(_)) | Err(_) => VersionCheck::Unknown,
    };
    shared.set_version(check);
}

/// Connects to `context` the way the manager does and starts a feed on that connection.
#[cfg(any(test, feature = "test-support"))]
pub async fn start_for_tests(
    kubeconfig: &kube::config::Kubeconfig,
    context: &str,
    trends: Arc<Mutex<TrendStore>>,
    timing: TelemetryTiming,
) -> Result<Telemetry, crate::cluster::ClusterError> {
    let (connection, _) =
        crate::cluster::connect_gated_and_check(kubeconfig, context, Duration::from_secs(30))
            .await?;
    let target = Target::of(kubeconfig, context);
    Ok(Telemetry::start(
        context.into(),
        connection.clients.read.clone(),
        target,
        trends,
        timing,
        RequestGate::default(),
        crate::thresholds::ThresholdMetric::PendingPodSeconds.default_value(),
    ))
}
