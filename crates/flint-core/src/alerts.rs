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
//!
//! # Text
//!
//! [`AlertEvent::text`] is Flint's wording plus counts. It holds no object names and no
//! server-supplied message. Names are in typed fields, for the front end to display safely.

// module-tests: alerts_tests.rs

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::jiff::{SignedDuration, Timestamp};

use crate::problems::{Problem, ProblemKind, ProblemSeverity, ProblemSubject};

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

/// The label the Deployment controller puts on its ReplicaSets' pods.
const POD_TEMPLATE_HASH: &str = "pod-template-hash";

/// Every kind of problem, for tracking which have had their first check.
const ALL_KINDS: [ProblemKind; 4] = [
    ProblemKind::CrashLoop,
    ProblemKind::OomKilled,
    ProblemKind::NodeNotReady,
    ProblemKind::JobFailed,
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

/// The controller that owns an object.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AlertOwner {
    /// The API group, empty for the core group.
    pub group: String,
    pub kind: String,
    pub name: String,
}

impl AlertOwner {
    /// The controller of an object, seen through a ReplicaSet to its Deployment.
    ///
    /// A pod names only its ReplicaSet, which changes on each rollout. The Deployment controller
    /// names a ReplicaSet `<deployment>-<pod-template-hash>`, so the Deployment is known when
    /// the pod's hash label matches that suffix. Otherwise the ReplicaSet itself is the owner.
    pub fn of_meta(meta: &ObjectMeta) -> Option<Self> {
        let owners = meta.owner_references.as_ref()?;
        let owner = owners.iter().find(|o| o.controller == Some(true))?;
        let group = owner
            .api_version
            .rsplit_once('/')
            .map_or("", |(group, _)| group);
        let deployment = (group == "apps" && owner.kind == "ReplicaSet")
            .then(|| meta.labels.as_ref()?.get(POD_TEMPLATE_HASH))
            .flatten()
            .and_then(|hash| owner.name.strip_suffix(hash.as_str()))
            .and_then(|rest| rest.strip_suffix('-'))
            .filter(|name| !name.is_empty());
        Some(Self {
            group: group.to_owned(),
            kind: deployment.map_or_else(|| owner.kind.clone(), |_| "Deployment".to_owned()),
            name: deployment.unwrap_or(&owner.name).to_owned(),
        })
    }

    fn is_bounded(&self) -> bool {
        [&self.group, &self.kind, &self.name]
            .iter()
            .all(|part| part.len() <= MAX_NAME_BYTES)
    }
}

/// What an alert is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AlertSubject {
    /// Everything under one controller.
    Workload {
        namespace: String,
        owner: AlertOwner,
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
                namespace.len() <= MAX_NAME_BYTES && owner.is_bounded()
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

/// A problem with the owner of its object, which [`Problem`] does not carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub problem: Problem,
    /// From [`AlertOwner::of_meta`] on the pod or Job. Ignored for nodes.
    pub owner: Option<AlertOwner>,
}

impl Observation {
    fn key(&self) -> AlertKey {
        let subject = match (&self.problem.id.subject, &self.owner) {
            (ProblemSubject::Node { name, .. }, _) => AlertSubject::Node { name: name.clone() },
            (
                ProblemSubject::Container { namespace, .. } | ProblemSubject::Job { namespace, .. },
                Some(owner),
            ) => AlertSubject::Workload {
                namespace: namespace.clone(),
                owner: owner.clone(),
            },
            (ProblemSubject::Container { namespace, pod, .. }, None) => AlertSubject::Pod {
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
            kind: self.problem.kind(),
            subject,
        }
    }
}

/// One pass over a cluster.
#[derive(Clone, Copy, Debug)]
pub enum ClusterSnapshot<'a> {
    /// The cluster was read. Problems of an `unchecked` kind could not be determined, so what is
    /// known about them is kept as unknown.
    Available {
        observations: &'a [Observation],
        unchecked: &'a [ProblemKind],
    },
    /// The cluster could not be read.
    Unavailable,
}

impl<'a> ClusterSnapshot<'a> {
    /// A snapshot in which every kind of problem was checked.
    pub fn complete(observations: &'a [Observation]) -> Self {
        Self::Available {
            observations,
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

/// What a caller should deliver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlertEvent {
    /// A new problem group.
    Raised(Alert),
    /// A raised group has stayed clear for the cooldown.
    Resolved(Alert),
    /// Several alerts in one message.
    Summary(AlertSummary),
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
            Self::Summary(summary) => summary_text(summary),
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
        ProblemKind::CrashLoop => "Container is no longer crashing",
        ProblemKind::OomKilled => "Out-of-memory kill no longer reported",
        ProblemKind::NodeNotReady => "Node no longer reported as not ready",
        ProblemKind::JobFailed => "Job failure no longer reported",
    }
}

fn summary_text(summary: &AlertSummary) -> String {
    let parts = [
        (summary.new, "new problem"),
        (summary.resolved, "resolved problem"),
        (summary.untracked, "further problem"),
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
    window: RateWindow,
    unavailable_since: Option<Timestamp>,
    unavailable_reported: bool,
    /// How many groups did not fit in the last pass, so only growth is reported.
    last_untracked: u32,
}

impl ClusterState {
    fn new(now: Timestamp) -> Self {
        Self {
            tracked: HashMap::new(),
            baselined: HashSet::new(),
            window: RateWindow::new(now),
            unavailable_since: None,
            unavailable_reported: false,
            last_untracked: 0,
        }
    }

    /// Pulls stored times back to `now`, so a wall clock set back cannot stall a timer.
    fn clamp_times(&mut self, now: Timestamp) {
        self.window.start = self.window.start.min(now);
        self.unavailable_since = self.unavailable_since.map(|since| since.min(now));
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
}

impl AlertEngine {
    /// An engine with no history.
    pub fn new(config: AlertConfig) -> Self {
        Self {
            config,
            clusters: HashMap::new(),
        }
    }

    /// Applies one pass over `cluster` at time `now`, returning what to deliver.
    ///
    /// Observations naming another cluster are ignored. An unavailable cluster that has never
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
                observations,
                unchecked,
            } => {
                if !self.clusters.contains_key(cluster) {
                    if self.clusters.len() >= MAX_CLUSTERS {
                        return Err(ClusterLimitReached);
                    }
                    self.clusters
                        .insert(Arc::clone(cluster), ClusterState::new(now));
                }
                Ok(self.observe_available(cluster, now, observations, unchecked))
            }
        }
    }

    /// Drops all history for a cluster, so its next check is a baseline again.
    pub fn forget(&mut self, cluster: &str) {
        self.clusters.remove(cluster);
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
        for entry in state.tracked.values_mut() {
            entry.status = Status::Unknown;
        }
        let since = *state.unavailable_since.get_or_insert(now);
        let mut events = Vec::new();
        if !state.unavailable_reported && now.duration_since(since) >= self.config.unavailable_after
        {
            state.unavailable_reported = true;
            events.push(AlertEvent::ClusterUnavailable {
                cluster: Arc::clone(cluster),
                open: saturating_u32(state.tracked.len()),
            });
        }
        events.extend(state.window.take_summary(cluster));
        events
    }

    fn observe_available(
        &mut self,
        cluster: &Arc<str>,
        now: Timestamp,
        observations: &[Observation],
        unchecked: &[ProblemKind],
    ) -> Vec<AlertEvent> {
        let config = self.config;
        let Some(state) = self.clusters.get_mut(cluster) else {
            return Vec::new();
        };
        state.clamp_times(now);
        state.window.roll(now, config.burst_window);

        let mut events = Vec::new();
        state.unavailable_since = None;
        if std::mem::take(&mut state.unavailable_reported) {
            events.push(AlertEvent::ClusterAvailable {
                cluster: Arc::clone(cluster),
            });
        }

        let (mut seen, mut untracked) = group(cluster, observations, unchecked);
        let resolved = update_tracked(state, &mut seen, unchecked, now, config.cooldown);
        for (key, entry) in resolved {
            if !entry.raised {
                continue;
            }
            if state.window.resolved < config.burst_limit {
                state.window.resolved += 1;
                events.push(AlertEvent::Resolved(alert(cluster, key, &entry)));
            } else {
                state.window.pending_resolved += 1;
            }
        }

        let mut new: Vec<(AlertKey, Seen)> = seen.into_iter().collect();
        new.sort_by(|(a, _), (b, _)| alert_order(a).cmp(&alert_order(b)));
        let mut untracked_highest = None;
        for (key, group) in new {
            if state.tracked.len() >= tracking_limit(key.kind.severity()) {
                untracked += 1;
                untracked_highest = untracked_highest.max(Some(key.kind.severity()));
                continue;
            }
            let baseline = !config.alert_on_existing && !state.baselined.contains(&key.kind);
            let mut raised = false;
            if !baseline {
                if state.window.raised < config.burst_limit {
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
        let growth = untracked.saturating_sub(state.last_untracked);
        if growth > 0 {
            state.window.pending_untracked += growth;
            state.window.pending_highest = state.window.pending_highest.max(untracked_highest);
        }
        state.last_untracked = untracked;
        events.extend(state.window.take_summary(cluster));
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

/// Groups a pass by key, returning the groups and how many observations cannot be kept.
fn group(
    cluster: &Arc<str>,
    observations: &[Observation],
    unchecked: &[ProblemKind],
) -> (HashMap<AlertKey, Seen>, u32) {
    let mut seen: HashMap<AlertKey, Seen> = HashMap::new();
    let mut oversized = 0;
    for observation in observations {
        let problem = &observation.problem;
        if *problem.id.cluster != **cluster || unchecked.contains(&problem.kind()) {
            continue;
        }
        let key = observation.key();
        if !key.subject.is_bounded() {
            oversized += 1;
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
        let expired = clearing_since.is_some_and(|since| now.duration_since(since) >= cooldown);
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
