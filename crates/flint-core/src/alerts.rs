//! Decides which alerts to raise from successive problem snapshots of each cluster.
//!
//! The engine sends nothing and has no clock or I/O: a caller passes each cluster's
//! [`ClusterSnapshot`] with the current time to [`AlertEngine::observe`] and delivers the
//! [`AlertEvent`]s it returns.
//!
//! # Grouping
//!
//! Alerts are per [`AlertKey`]: a kind of problem on one workload. A rollout's replacement pods
//! have new UIDs but the same owner, so they stay one alert. An object with no usable owner is
//! its own group, keyed by name, so a recreated object continues the same alert.
//!
//! # Life of an alert
//!
//! - A new group raises once, and stays open silently while it is seen.
//! - A group that disappears is held for [`AlertConfig::cooldown`]. If it returns in that time
//!   nothing is sent, so a crash loop's short healthy runs do not re-alert. Otherwise it
//!   resolves, and only a group that raised its own alert reports that.
//! - Missing data never resolves anything. While a cluster or a kind of problem cannot be
//!   checked, its groups are [`OpenAlertState::Unknown`], and the cooldown restarts once it can.
//! - The first check of each kind of problem is a baseline and raises nothing, unless
//!   [`AlertConfig::alert_on_existing`] is set.
//! - Availability changes are rate-limited: a cluster reports unavailable at most once per
//!   [`AlertConfig::burst_window`], all clusters share [`AlertConfig::burst_limit`] changes per
//!   window, and recovery is reported only by a pass that checked every kind.
//!
//! # Hold-down
//!
//! A problem with a [`Problem::hold_down`] has no time in its status, so it may simply still be
//! starting. It takes part in alerting only once its id has been present for that long.
//!
//! - One that clears first raises and resolves nothing, and its wait restarts if it returns.
//! - A pass that cannot check its kind, or an unavailable cluster, leaves the wait running.
//! - A group already open is not held back, including one in its cooldown.
//! - A problem at the first check of its kind is baseline at once, so it never raises later.
//!
//! # Unknown owners
//!
//! A pod names only its ReplicaSet, which changes on each rollout, so its alert group is known
//! only once the ReplicaSet has been read. A problem whose owner is not known
//! ([`Problem::owner_known`] is false) makes its whole kind unchecked for that pass, exactly as
//! if the caller had listed it in [`ClusterSnapshot::Available`]: nothing of that kind raises or
//! resolves, and its groups are [`OpenAlertState::Unknown`]. Owners are never guessed from names.
//!
//! Once every problem of the kind has a known owner, normal transitions continue. A group still
//! seen is open again without an event, one no longer seen starts a full cooldown, and a new
//! group raises, unless the kind has never been checked, when that pass is its baseline.
//!
//! The limit this accepts: while ReplicaSets cannot be read, crash, OOM and image-pull alerts for
//! ReplicaSet-owned pods wait, and the cluster's "could not be checked" state is what the person
//! sees. Other owners with a problem of that kind wait too, because the kind is the unit of
//! checking. A pass with no problem of a kind has nothing of unknown owner, so it still counts
//! as a check of that kind.
//!
//! # Text
//!
//! [`AlertEvent::text`] is Flint's wording plus counts. It holds no object names and no
//! server-supplied message. Names are in typed fields, for the front end to display safely.

// module-tests: alerts_tests.rs

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use k8s_openapi::jiff::{SignedDuration, Timestamp};

use crate::problems::{
    Problem, ProblemId, ProblemKind, ProblemOwner, ProblemSeverity, ProblemSubject,
};

/// How long a group must stay absent before it resolves and can raise again.
///
/// Longer than the kubelet's five-minute maximum restart back-off, so a crash loop at full
/// back-off cannot clear between two restarts.
pub const DEFAULT_COOLDOWN: SignedDuration = SignedDuration::from_secs(10 * 60);

/// How many raised, and separately resolved, alerts a cluster sends per window.
pub const DEFAULT_BURST_LIMIT: u32 = 5;

/// The length of the rate-limit window.
pub const DEFAULT_BURST_WINDOW: SignedDuration = SignedDuration::from_secs(5 * 60);

/// How long a cluster must stay unavailable before that is reported.
pub const DEFAULT_UNAVAILABLE_AFTER: SignedDuration = SignedDuration::from_secs(2 * 60);

/// The most clusters the engine tracks.
pub const MAX_CLUSTERS: usize = 256;

/// The most groups tracked per cluster. Further ones are counted in a summary.
pub const MAX_TRACKED_PER_CLUSTER: usize = 512;

/// How many of the tracked slots only critical groups can take, so that long-lived warnings
/// cannot crowd out a new critical problem.
pub const CRITICAL_RESERVED_PER_CLUSTER: usize = 64;

/// The longest name kept in a key: the Kubernetes limit for a DNS subdomain name.
pub const MAX_NAME_BYTES: usize = 253;

/// Every kind of problem, for tracking which have had their first check.
const ALL_KINDS: [ProblemKind; 7] = [
    ProblemKind::CrashLoop,
    ProblemKind::OomKilled,
    ProblemKind::NodeNotReady,
    ProblemKind::JobFailed,
    ProblemKind::ImagePull,
    ProblemKind::WorkloadShort,
    ProblemKind::PodPending,
];

/// Tuning for an [`AlertEngine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlertConfig {
    /// See [`DEFAULT_COOLDOWN`].
    pub cooldown: SignedDuration,
    /// See [`DEFAULT_BURST_LIMIT`].
    pub burst_limit: u32,
    /// See [`DEFAULT_BURST_WINDOW`].
    pub burst_window: SignedDuration,
    /// See [`DEFAULT_UNAVAILABLE_AFTER`].
    pub unavailable_after: SignedDuration,
    /// Raise problems that already exist at the first check, instead of taking them as baseline.
    pub alert_on_existing: bool,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            cooldown: DEFAULT_COOLDOWN,
            burst_limit: DEFAULT_BURST_LIMIT,
            burst_window: DEFAULT_BURST_WINDOW,
            unavailable_after: DEFAULT_UNAVAILABLE_AFTER,
            alert_on_existing: false,
        }
    }
}

/// What an alert is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AlertSubject {
    /// Everything under one controller.
    Workload {
        namespace: String,
        owner: ProblemOwner,
    },
    /// A pod with no usable owner.
    Pod { namespace: String, name: String },
    /// A Job with no usable owner.
    Job { namespace: String, name: String },
    /// A node.
    Node { name: String },
}

impl AlertSubject {
    fn is_bounded(&self) -> bool {
        match self {
            Self::Workload { namespace, owner } => {
                namespace.len() <= MAX_NAME_BYTES
                    && [&owner.group, &owner.kind, &owner.name]
                        .iter()
                        .all(|part| part.len() <= MAX_NAME_BYTES)
            }
            Self::Pod { namespace, name } | Self::Job { namespace, name } => {
                namespace.len() <= MAX_NAME_BYTES && name.len() <= MAX_NAME_BYTES
            }
            Self::Node { name } => name.len() <= MAX_NAME_BYTES,
        }
    }
}

/// The identity of an alert within a cluster.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AlertKey {
    pub kind: ProblemKind,
    pub subject: AlertSubject,
}

/// The alert group of a problem. A workload's own problem is keyed by the workload itself.
fn key_of(problem: &Problem) -> AlertKey {
    let subject = match (&problem.id.subject, &problem.owner) {
        (ProblemSubject::Node { name, .. }, _) => AlertSubject::Node { name: name.clone() },
        (
            ProblemSubject::Container { namespace, .. }
            | ProblemSubject::Pod { namespace, .. }
            | ProblemSubject::Job { namespace, .. },
            Some(owner),
        ) => AlertSubject::Workload {
            namespace: namespace.clone(),
            owner: owner.clone(),
        },
        (
            ProblemSubject::Workload {
                kind,
                namespace,
                name,
                ..
            },
            _,
        ) => AlertSubject::Workload {
            namespace: namespace.clone(),
            owner: ProblemOwner::workload(*kind, name),
        },
        (ProblemSubject::Container { namespace, pod, .. }, None)
        | (
            ProblemSubject::Pod {
                namespace,
                name: pod,
                ..
            },
            None,
        ) => AlertSubject::Pod {
            namespace: namespace.clone(),
            name: pod.clone(),
        },
        (
            ProblemSubject::Job {
                namespace, name, ..
            },
            None,
        ) => AlertSubject::Job {
            namespace: namespace.clone(),
            name: name.clone(),
        },
    };
    AlertKey {
        kind: problem.kind(),
        subject,
    }
}

/// One pass over a cluster.
#[derive(Clone, Copy, Debug)]
pub enum ClusterSnapshot<'a> {
    /// The cluster was read. Problems of an `unchecked` kind could not be determined, so what is
    /// known about them is kept as unknown.
    Available {
        problems: &'a [Problem],
        unchecked: &'a [ProblemKind],
    },
    /// The cluster could not be read.
    Unavailable,
}

impl<'a> ClusterSnapshot<'a> {
    /// A snapshot in which every kind of problem was checked.
    pub fn complete(problems: &'a [Problem]) -> Self {
        Self::Available {
            problems,
            unchecked: &[],
        }
    }
}

/// One alert to raise or resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub cluster: Arc<str>,
    pub key: AlertKey,
    /// How many problems the group held when last seen.
    pub affected: u32,
    /// Flint's description, from [`Problem::summary`].
    pub summary: &'static str,
}

impl Alert {
    /// How urgent the alert is.
    pub fn severity(&self) -> ProblemSeverity {
        self.key.kind.severity()
    }
}

/// Alerts held back by the rate limit or capacity, reported together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertSummary {
    pub cluster: Arc<str>,
    /// New problem groups not alerted on individually.
    pub new: u32,
    /// Resolved groups not reported individually.
    pub resolved: u32,
    /// Problem groups beyond what the engine can track.
    pub untracked: u32,
    /// The most urgent of the `new` groups and of the `untracked` groups with a known kind.
    pub highest: Option<ProblemSeverity>,
}

/// Alerts of several clusters held back by the app-wide budget, reported together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetSummary {
    /// The first of the clusters by name, which opening the notification shows.
    pub first: Arc<str>,
    /// How many clusters it covers, always more than one.
    pub clusters: u32,
    pub new: u32,
    pub resolved: u32,
    pub untracked: u32,
    pub highest: Option<ProblemSeverity>,
}

/// What a caller should deliver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlertEvent {
    /// A new problem group.
    Raised(Alert),
    /// A raised group has stayed clear for the cooldown.
    Resolved(Alert),
    /// Several alerts in one message.
    Summary(AlertSummary),
    /// Several alerts of several clusters in one message.
    FleetSummary(FleetSummary),
    /// The cluster has stayed unavailable; `open` groups are now unknown.
    ClusterUnavailable { cluster: Arc<str>, open: u32 },
    /// A cluster reported as unavailable can be read again.
    ClusterAvailable { cluster: Arc<str> },
}

impl AlertEvent {
    /// Flint's wording for this event. Never contains names or server-supplied text.
    pub fn text(&self) -> String {
        match self {
            Self::Raised(alert) if alert.affected > 1 => {
                format!("{} ({} affected)", alert.summary, alert.affected)
            }
            Self::Raised(alert) => alert.summary.to_owned(),
            Self::Resolved(alert) => resolved_text(alert.key.kind).to_owned(),
            Self::Summary(summary) => {
                summary_text(summary.new, summary.resolved, summary.untracked)
            }
            Self::FleetSummary(summary) => format!(
                "{} across {} clusters",
                summary_text(summary.new, summary.resolved, summary.untracked),
                summary.clusters
            ),
            Self::ClusterUnavailable { open: 0, .. } => "Cluster can no longer be checked".into(),
            Self::ClusterUnavailable { open, .. } => format!(
                "Cluster can no longer be checked; {} now unknown",
                counted(*open, "open problem")
            ),
            Self::ClusterAvailable { .. } => "Cluster can be checked again".to_owned(),
        }
    }
}

fn resolved_text(kind: ProblemKind) -> &'static str {
    match kind {
        ProblemKind::CrashLoop => "Crash loop no longer reported",
        ProblemKind::OomKilled => "Out-of-memory kill no longer reported",
        ProblemKind::NodeNotReady => "Node no longer reported as not ready",
        ProblemKind::JobFailed => "Job failure no longer reported",
        ProblemKind::ImagePull => "Image pull failure no longer reported",
        ProblemKind::WorkloadShort => "Missing replicas no longer reported",
        ProblemKind::PodPending => "Pod no longer reported as stuck Pending",
    }
}

fn summary_text(new: u32, resolved: u32, untracked: u32) -> String {
    let parts = [
        (new, "new problem"),
        (resolved, "resolved problem"),
        (untracked, "further problem"),
    ];
    let parts: Vec<String> = parts
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, noun)| counted(count, noun))
        .collect();
    parts.join(", ")
}

fn counted(count: u32, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{plural}")
}

/// Whether a tracked group is known to be happening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAlertState {
    /// Seen in the latest check, or absent for less than the cooldown.
    Open,
    /// Its cluster or kind of problem cannot be checked right now.
    Unknown,
}

/// A tracked group, for a front end to list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAlert {
    pub key: AlertKey,
    pub state: OpenAlertState,
    pub affected: u32,
}

/// [`AlertEngine::observe`] was given a new cluster while at [`MAX_CLUSTERS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClusterLimitReached;

impl fmt::Display for ClusterLimitReached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "alerts track at most {MAX_CLUSTERS} clusters")
    }
}

impl std::error::Error for ClusterLimitReached {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Open,
    Clearing { since: Timestamp },
    Unknown,
}

#[derive(Clone, Copy, Debug)]
struct Tracked {
    status: Status,
    /// Whether this group raised its own alert, and so reports its own resolution.
    raised: bool,
    affected: u32,
    summary: &'static str,
}

impl Tracked {
    /// Whether the group has been clear for the whole cooldown.
    fn cooldown_over(&self, now: Timestamp, cooldown: SignedDuration) -> bool {
        matches!(self.status, Status::Clearing { since } if now.duration_since(since) >= cooldown)
    }
}

#[derive(Debug)]
struct RateWindow {
    start: Timestamp,
    raised: u32,
    resolved: u32,
    summary_sent: bool,
    pending_new: u32,
    pending_resolved: u32,
    pending_untracked: u32,
    pending_highest: Option<ProblemSeverity>,
}

impl RateWindow {
    fn new(now: Timestamp) -> Self {
        Self {
            start: now,
            raised: 0,
            resolved: 0,
            summary_sent: false,
            pending_new: 0,
            pending_resolved: 0,
            pending_untracked: 0,
            pending_highest: None,
        }
    }

    fn roll(&mut self, now: Timestamp, length: SignedDuration) {
        if now.duration_since(self.start) >= length {
            self.start = now;
            self.raised = 0;
            self.resolved = 0;
            self.summary_sent = false;
        }
    }

    /// The pending summary, at most one per window.
    fn take_summary(&mut self, cluster: &Arc<str>) -> Option<AlertEvent> {
        let pending = self.pending_new + self.pending_resolved + self.pending_untracked;
        if pending == 0 || self.summary_sent {
            return None;
        }
        self.summary_sent = true;
        Some(AlertEvent::Summary(AlertSummary {
            cluster: Arc::clone(cluster),
            new: std::mem::take(&mut self.pending_new),
            resolved: std::mem::take(&mut self.pending_resolved),
            untracked: std::mem::take(&mut self.pending_untracked),
            highest: self.pending_highest.take(),
        }))
    }
}

#[derive(Debug)]
struct ClusterState {
    tracked: HashMap<AlertKey, Tracked>,
    /// The kinds of problem that have had their first check.
    baselined: HashSet<ProblemKind>,
    /// When each present problem with a hold-down was first seen.
    first_seen: HashMap<ProblemId, Timestamp>,
    window: RateWindow,
    unavailable_since: Option<Timestamp>,
    unavailable_reported: bool,
    /// When it was last reported unavailable, so a flapping cluster reports once per window.
    unavailable_announced: Option<Timestamp>,
    /// How many groups of each kind did not fit when it was last checked, so only growth is
    /// reported. A kind that cannot be checked keeps its count.
    last_untracked: HashMap<ProblemKind, u32>,
}

/// Availability changes sent across all clusters in the current window. A change held back here
/// is not lost: the cluster's state still differs from what was reported, so a later pass sends
/// it, and one that reverts first sends nothing.
#[derive(Clone, Copy, Debug, Default)]
struct AvailabilityBudget {
    start: Option<Timestamp>,
    sent: u32,
}

impl AvailabilityBudget {
    fn take(&mut self, now: Timestamp, config: &AlertConfig) -> bool {
        let expired = self
            .start
            .is_none_or(|start| start > now || now.duration_since(start) >= config.burst_window);
        if expired {
            self.start = Some(now);
            self.sent = 0;
        }
        if self.sent >= config.burst_limit {
            return false;
        }
        self.sent += 1;
        true
    }
}

/// What the app-wide budget held back for one cluster.
#[derive(Clone, Copy, Debug, Default)]
struct HeldCounts {
    new: u32,
    resolved: u32,
    untracked: u32,
    highest: Option<ProblemSeverity>,
}

/// What all clusters together may deliver per window, so one change that touches many clusters,
/// such as a lower threshold, cannot flood the desktop. Individual alerts stop at twice a
/// cluster's limit, and warnings at half of that, so critical alerts always keep a share. One
/// cluster's summary goes out as usual; the rest wait here and leave as one message.
#[derive(Debug, Default)]
struct FleetBudget {
    start: Option<Timestamp>,
    raised: u32,
    resolved: u32,
    summaries: u32,
    combined_sent: bool,
    held: BTreeMap<Arc<str>, HeldCounts>,
}

impl FleetBudget {
    const DIRECT_SUMMARIES: u32 = 1;

    fn roll(&mut self, now: Timestamp, config: &AlertConfig) {
        let expired = self
            .start
            .is_none_or(|start| start > now || now.duration_since(start) >= config.burst_window);
        if expired {
            self.start = Some(now);
            self.raised = 0;
            self.resolved = 0;
            self.summaries = 0;
            self.combined_sent = false;
        }
    }

    fn take_raised(&mut self, severity: ProblemSeverity, config: &AlertConfig) -> bool {
        let limit = match severity {
            ProblemSeverity::Critical => config.burst_limit.saturating_mul(2),
            ProblemSeverity::Warning => config.burst_limit,
        };
        let free = self.raised < limit;
        self.raised += u32::from(free);
        free
    }

    fn take_resolved(&mut self, config: &AlertConfig) -> bool {
        let free = self.resolved < config.burst_limit.saturating_mul(2);
        self.resolved += u32::from(free);
        free
    }

    /// Passes a cluster's summary on, or holds it for the combined one.
    fn route(&mut self, event: Option<AlertEvent>) -> Option<AlertEvent> {
        let event = event?;
        if self.summaries < Self::DIRECT_SUMMARIES {
            self.summaries += 1;
            return Some(event);
        }
        if let AlertEvent::Summary(summary) = &event {
            let held = self.held.entry(Arc::clone(&summary.cluster)).or_default();
            held.new = held.new.saturating_add(summary.new);
            held.resolved = held.resolved.saturating_add(summary.resolved);
            held.untracked = held.untracked.saturating_add(summary.untracked);
            held.highest = held.highest.max(summary.highest);
            return None;
        }
        Some(event)
    }

    /// Everything held, at most once per window: a single cluster's as its own summary.
    fn take_combined(&mut self) -> Option<AlertEvent> {
        if self.combined_sent || self.held.is_empty() {
            return None;
        }
        self.combined_sent = true;
        let held = std::mem::take(&mut self.held);
        let clusters = saturating_u32(held.len());
        let first = held.keys().next().map(Arc::clone)?;
        let total = held
            .values()
            .fold(HeldCounts::default(), |sum, one| HeldCounts {
                new: sum.new.saturating_add(one.new),
                resolved: sum.resolved.saturating_add(one.resolved),
                untracked: sum.untracked.saturating_add(one.untracked),
                highest: sum.highest.max(one.highest),
            });
        Some(if clusters == 1 {
            AlertEvent::Summary(AlertSummary {
                cluster: first,
                new: total.new,
                resolved: total.resolved,
                untracked: total.untracked,
                highest: total.highest,
            })
        } else {
            AlertEvent::FleetSummary(FleetSummary {
                first,
                clusters,
                new: total.new,
                resolved: total.resolved,
                untracked: total.untracked,
                highest: total.highest,
            })
        })
    }
}

impl ClusterState {
    fn new(now: Timestamp) -> Self {
        Self {
            tracked: HashMap::new(),
            baselined: HashSet::new(),
            first_seen: HashMap::new(),
            window: RateWindow::new(now),
            unavailable_since: None,
            unavailable_reported: false,
            unavailable_announced: None,
            last_untracked: HashMap::new(),
        }
    }

    /// Pulls stored times back to `now`, so a wall clock set back cannot stall a timer.
    fn clamp_times(&mut self, now: Timestamp) {
        self.window.start = self.window.start.min(now);
        self.unavailable_since = self.unavailable_since.map(|since| since.min(now));
        self.unavailable_announced = self.unavailable_announced.map(|at| at.min(now));
        for first in self.first_seen.values_mut() {
            *first = (*first).min(now);
        }
        for entry in self.tracked.values_mut() {
            if let Status::Clearing { since } = &mut entry.status {
                *since = (*since).min(now);
            }
        }
    }
}

/// A group seen in the current pass.
struct Seen {
    affected: u32,
    summary: &'static str,
}

/// The alert decisions for a set of clusters; see the [module docs](self).
#[derive(Debug, Default)]
pub struct AlertEngine {
    config: AlertConfig,
    clusters: HashMap<Arc<str>, ClusterState>,
    availability: AvailabilityBudget,
    fleet: FleetBudget,
}

impl AlertEngine {
    /// An engine with no history.
    pub fn new(config: AlertConfig) -> Self {
        Self {
            config,
            clusters: HashMap::new(),
            availability: AvailabilityBudget::default(),
            fleet: FleetBudget::default(),
        }
    }

    /// Whether `cluster` has history here.
    pub fn tracks(&self, cluster: &str) -> bool {
        self.clusters.contains_key(cluster)
    }

    /// Marks every group of `cluster` unknown because its data cannot be read for a moment. Sends
    /// nothing and starts no outage, but a cooldown in progress restarts on the next real check.
    pub fn mark_unknown(&mut self, cluster: &str) {
        if let Some(state) = self.clusters.get_mut(cluster) {
            for entry in state.tracked.values_mut() {
                entry.status = Status::Unknown;
            }
        }
    }

    /// Applies one pass over `cluster` at time `now`, returning what to deliver.
    ///
    /// Problems naming another cluster are ignored. An unavailable cluster that has never
    /// been read has nothing to keep and is not tracked.
    pub fn observe(
        &mut self,
        cluster: &Arc<str>,
        now: Timestamp,
        snapshot: ClusterSnapshot<'_>,
    ) -> Result<Vec<AlertEvent>, ClusterLimitReached> {
        match snapshot {
            ClusterSnapshot::Unavailable => Ok(self.observe_unavailable(cluster, now)),
            ClusterSnapshot::Available {
                problems,
                unchecked,
            } => {
                if !self.clusters.contains_key(cluster) {
                    if self.clusters.len() >= MAX_CLUSTERS {
                        return Err(ClusterLimitReached);
                    }
                    self.clusters
                        .insert(Arc::clone(cluster), ClusterState::new(now));
                }
                Ok(self.observe_available(cluster, now, problems, unchecked))
            }
        }
    }

    /// Drops all history for a cluster, so its next check is a baseline again.
    pub fn forget(&mut self, cluster: &str) {
        self.clusters.remove(cluster);
        self.fleet.held.remove(cluster);
    }

    /// What the app-wide budget held back, at most once per window. Call after each full pass.
    pub fn take_fleet_summary(&mut self, now: Timestamp) -> Option<AlertEvent> {
        self.fleet.roll(now, &self.config);
        self.fleet.take_combined()
    }

    /// The groups tracked for a cluster, most urgent first.
    pub fn open(&self, cluster: &str) -> Vec<OpenAlert> {
        let Some(state) = self.clusters.get(cluster) else {
            return Vec::new();
        };
        let mut open: Vec<OpenAlert> = state
            .tracked
            .iter()
            .map(|(key, entry)| OpenAlert {
                key: key.clone(),
                state: if entry.status == Status::Unknown {
                    OpenAlertState::Unknown
                } else {
                    OpenAlertState::Open
                },
                affected: entry.affected,
            })
            .collect();
        open.sort_by(|a, b| alert_order(&a.key).cmp(&alert_order(&b.key)));
        open
    }

    fn observe_unavailable(&mut self, cluster: &Arc<str>, now: Timestamp) -> Vec<AlertEvent> {
        let Some(state) = self.clusters.get_mut(cluster) else {
            return Vec::new();
        };
        state.clamp_times(now);
        state.window.roll(now, self.config.burst_window);
        self.fleet.roll(now, &self.config);
        for entry in state.tracked.values_mut() {
            entry.status = Status::Unknown;
        }
        let since = *state.unavailable_since.get_or_insert(now);
        let mut events = Vec::new();
        let spaced = state
            .unavailable_announced
            .is_none_or(|at| now.duration_since(at) >= self.config.burst_window);
        if !state.unavailable_reported
            && spaced
            && now.duration_since(since) >= self.config.unavailable_after
            && self.availability.take(now, &self.config)
        {
            state.unavailable_reported = true;
            state.unavailable_announced = Some(now);
            events.push(AlertEvent::ClusterUnavailable {
                cluster: Arc::clone(cluster),
                open: saturating_u32(state.tracked.len()),
            });
        }
        events.extend(self.fleet.route(state.window.take_summary(cluster)));
        events
    }

    fn observe_available(
        &mut self,
        cluster: &Arc<str>,
        now: Timestamp,
        problems: &[Problem],
        unchecked: &[ProblemKind],
    ) -> Vec<AlertEvent> {
        let config = self.config;
        let Some(state) = self.clusters.get_mut(cluster) else {
            return Vec::new();
        };
        state.clamp_times(now);
        state.window.roll(now, config.burst_window);
        self.fleet.roll(now, &config);

        let mut events = Vec::new();
        let unchecked = unchecked_kinds(cluster, problems, unchecked);
        let unchecked = unchecked.as_slice();
        // A pass that verified no kind says nothing about the cluster being readable, and only
        // one that verified every kind is reported as recovery.
        if ALL_KINDS.iter().any(|kind| !unchecked.contains(kind)) {
            state.unavailable_since = None;
        }
        if state.unavailable_reported
            && unchecked.is_empty()
            && self.availability.take(now, &config)
        {
            state.unavailable_reported = false;
            events.push(AlertEvent::ClusterAvailable {
                cluster: Arc::clone(cluster),
            });
        }

        let waiting = update_hold_downs(state, cluster, problems, unchecked, now, &config);
        let (mut seen, mut untracked) = group(cluster, problems, unchecked, &waiting);
        let resolved = update_tracked(state, &mut seen, unchecked, now, config.cooldown);
        for (key, entry) in resolved {
            if !entry.raised {
                continue;
            }
            if state.window.resolved < config.burst_limit && self.fleet.take_resolved(&config) {
                state.window.resolved += 1;
                events.push(AlertEvent::Resolved(alert(cluster, key, &entry)));
            } else {
                state.window.pending_resolved += 1;
            }
        }

        let mut new: Vec<(AlertKey, Seen)> = seen.into_iter().collect();
        new.sort_by(|(a, _), (b, _)| alert_order(a).cmp(&alert_order(b)));
        let mut over_capacity = HashSet::new();
        for (key, group) in new {
            if state.tracked.len() >= tracking_limit(key.kind.severity()) {
                *untracked.entry(key.kind).or_default() += 1;
                over_capacity.insert(key.kind);
                continue;
            }
            let baseline = !config.alert_on_existing && !state.baselined.contains(&key.kind);
            let mut raised = false;
            if !baseline {
                if state.window.raised < config.burst_limit
                    && self.fleet.take_raised(key.kind.severity(), &config)
                {
                    state.window.raised += 1;
                    raised = true;
                } else {
                    state.window.pending_new += 1;
                    let severity = key.kind.severity();
                    state.window.pending_highest = state.window.pending_highest.max(Some(severity));
                }
            }
            let entry = Tracked {
                status: Status::Open,
                raised,
                affected: group.affected,
                summary: group.summary,
            };
            if raised {
                events.push(AlertEvent::Raised(alert(cluster, key.clone(), &entry)));
            }
            state.tracked.insert(key, entry);
        }

        for kind in ALL_KINDS {
            if !unchecked.contains(&kind) {
                state.baselined.insert(kind);
            }
        }
        for kind in ALL_KINDS.into_iter().filter(|k| !unchecked.contains(k)) {
            let count = untracked.get(&kind).copied().unwrap_or(0);
            let last = state.last_untracked.insert(kind, count).unwrap_or(0);
            if count > last {
                state.window.pending_untracked += count - last;
                if over_capacity.contains(&kind) {
                    state.window.pending_highest =
                        state.window.pending_highest.max(Some(kind.severity()));
                }
            }
        }
        events.extend(self.fleet.route(state.window.take_summary(cluster)));
        events
    }
}

/// Most urgent first, then by key, so results do not depend on snapshot or hash order.
fn alert_order(key: &AlertKey) -> (Reverse<ProblemSeverity>, &AlertKey) {
    (Reverse(key.kind.severity()), key)
}

/// How many groups may be tracked before a new group of this severity no longer fits.
fn tracking_limit(severity: ProblemSeverity) -> usize {
    match severity {
        ProblemSeverity::Critical => MAX_TRACKED_PER_CLUSTER,
        ProblemSeverity::Warning => MAX_TRACKED_PER_CLUSTER - CRITICAL_RESERVED_PER_CLUSTER,
    }
}

/// The kinds this pass cannot judge: those the caller named, and any with a problem whose owner
/// is unknown. See [unknown owners](self#unknown-owners).
fn unchecked_kinds(
    cluster: &Arc<str>,
    problems: &[Problem],
    unchecked: &[ProblemKind],
) -> Vec<ProblemKind> {
    let mut kinds = unchecked.to_vec();
    for problem in problems {
        if !problem.owner_known
            && *problem.id.cluster == **cluster
            && !kinds.contains(&problem.kind())
        {
            kinds.push(problem.kind());
        }
    }
    kinds
}

/// Updates the [hold-down](self#hold-down) waits from a pass, returning the problems still held.
fn update_hold_downs<'a>(
    state: &mut ClusterState,
    cluster: &Arc<str>,
    problems: &'a [Problem],
    unchecked: &[ProblemKind],
    now: Timestamp,
    config: &AlertConfig,
) -> HashSet<&'a ProblemId> {
    let mut present = HashSet::new();
    let mut waiting = HashSet::new();
    for problem in problems {
        if *problem.id.cluster != **cluster || unchecked.contains(&problem.kind()) {
            continue;
        }
        let Some(hold_down) = problem.hold_down() else {
            continue;
        };
        present.insert(&problem.id);
        let first = *state.first_seen.entry(problem.id.clone()).or_insert(now);
        if now.duration_since(first) >= hold_down {
            continue;
        }
        let baseline = !config.alert_on_existing && !state.baselined.contains(&problem.kind());
        let open = state
            .tracked
            .get(&key_of(problem))
            .is_some_and(|entry| !entry.cooldown_over(now, config.cooldown));
        if !baseline && !open {
            waiting.insert(&problem.id);
        }
    }
    state
        .first_seen
        .retain(|id, _| present.contains(id) || unchecked.contains(&id.kind));
    waiting
}

/// Groups a pass by key, returning the groups and how many problems of each kind cannot be kept.
/// Problems in `waiting` are left out, as if not yet seen.
fn group(
    cluster: &Arc<str>,
    problems: &[Problem],
    unchecked: &[ProblemKind],
    waiting: &HashSet<&ProblemId>,
) -> (HashMap<AlertKey, Seen>, HashMap<ProblemKind, u32>) {
    let mut seen: HashMap<AlertKey, Seen> = HashMap::new();
    let mut oversized: HashMap<ProblemKind, u32> = HashMap::new();
    for problem in problems {
        if *problem.id.cluster != **cluster
            || unchecked.contains(&problem.kind())
            || waiting.contains(&problem.id)
        {
            continue;
        }
        let key = key_of(problem);
        if !key.subject.is_bounded() {
            *oversized.entry(key.kind).or_default() += 1;
            continue;
        }
        let summary = problem.summary();
        seen.entry(key)
            .and_modify(|group| {
                group.affected = group.affected.saturating_add(1);
                group.summary = group.summary.min(summary);
            })
            .or_insert(Seen {
                affected: 1,
                summary,
            });
    }
    (seen, oversized)
}

/// Updates known groups from a pass, removing them from `seen` and returning those that resolved.
fn update_tracked(
    state: &mut ClusterState,
    seen: &mut HashMap<AlertKey, Seen>,
    unchecked: &[ProblemKind],
    now: Timestamp,
    cooldown: SignedDuration,
) -> Vec<(AlertKey, Tracked)> {
    let mut resolved = Vec::new();
    state.tracked.retain(|key, entry| {
        if unchecked.contains(&key.kind) {
            entry.status = Status::Unknown;
            return true;
        }
        let clearing_since = match entry.status {
            Status::Clearing { since } => Some(since),
            Status::Open | Status::Unknown => None,
        };
        // A group back after its deadline stays in `seen`, to raise as a new occurrence.
        let expired = entry.cooldown_over(now, cooldown);
        if let Some(group) = (!expired).then(|| seen.remove(key)).flatten() {
            entry.status = Status::Open;
            entry.affected = group.affected;
            entry.summary = group.summary;
            return true;
        }
        let since = clearing_since.unwrap_or(now);
        if now.duration_since(since) >= cooldown {
            resolved.push((key.clone(), *entry));
            return false;
        }
        entry.status = Status::Clearing { since };
        true
    });
    resolved.sort_by(|(a, _), (b, _)| alert_order(a).cmp(&alert_order(b)));
    resolved
}

fn alert(cluster: &Arc<str>, key: AlertKey, entry: &Tracked) -> Alert {
    Alert {
        cluster: Arc::clone(cluster),
        key,
        affected: entry.affected,
        summary: entry.summary,
    }
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[path = "alerts_tests.rs"]
mod tests;
