//! The "Problems now" counts of a cluster dashboard (ADR-0017).
//!
//! Everything here is pure: callers pass what they already hold and the current time, and nothing
//! touches the network. [`ProblemSummariser`] turns one cluster's [`ProblemSnapshot`] into
//! [`ProblemCounts`], and [`ProblemTotals`] adds the counts of several clusters for a cluster set.
//!
//! # Counting
//!
//! A count is the number of objects its filtered list would show, so a pod with two failing
//! containers is one failing pod.
//!
//! # Missing data
//!
//! A category that could not be read is unknown, never zero, and stays unknown in
//! [`ProblemCounts::level`]. Cluster-set totals report how many clusters they could not count, and
//! [`AttentionRank`] never orders an unreadable cluster as the healthiest.
//!
//! # Hold-down
//!
//! A StatefulSet or DaemonSet shortfall only counts once it has lasted [`Problem::hold_down`], so
//! the summariser keeps one per cluster across passes. A pass that could not check workloads does
//! not restart that wait.

// module-tests: dashboard_tests.rs

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use k8s_openapi::api::core::v1::{Event, PersistentVolumeClaim};
use k8s_openapi::jiff::{SignedDuration, Timestamp};

use crate::problems::{Problem, ProblemId, ProblemKind, ProblemSubject, WorkloadKind};
use crate::thresholds::{ThresholdLevel, ThresholdMetric, Thresholds};

/// How far back a Warning event counts.
pub const WARNING_EVENT_WINDOW: SignedDuration = SignedDuration::from_secs(60 * 60);

/// One count on the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProblemCategory {
    /// Pods with a crash-looping, OOM-killed or image-pull-failing container.
    FailingPods,
    /// Nodes that are not ready.
    NotReadyNodes,
    /// Warning events in the last hour.
    WarningEvents,
    /// PersistentVolumeClaims still pending.
    PendingClaims,
    /// Jobs that finished as failed.
    FailedJobs,
    /// Deployments, StatefulSets and DaemonSets short of their desired replicas.
    ShortWorkloads,
}

impl ProblemCategory {
    /// Every category, in display order.
    pub const ALL: [Self; 6] = [
        Self::FailingPods,
        Self::NotReadyNodes,
        Self::WarningEvents,
        Self::PendingClaims,
        Self::FailedJobs,
        Self::ShortWorkloads,
    ];

    /// The category a classified problem counts under.
    pub const fn of_kind(kind: ProblemKind) -> Self {
        match kind {
            ProblemKind::CrashLoop | ProblemKind::OomKilled | ProblemKind::ImagePull => {
                Self::FailingPods
            }
            ProblemKind::NodeNotReady => Self::NotReadyNodes,
            ProblemKind::JobFailed => Self::FailedJobs,
            ProblemKind::WorkloadShort => Self::ShortWorkloads,
        }
    }

    /// The configurable threshold of this category. Without one, any count needs attention.
    pub const fn threshold(self) -> Option<ThresholdMetric> {
        match self {
            Self::WarningEvents => Some(ThresholdMetric::WarningEventsLastHour),
            Self::FailingPods
            | Self::NotReadyNodes
            | Self::PendingClaims
            | Self::FailedJobs
            | Self::ShortWorkloads => None,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// What is known about one cluster's problems at one moment.
#[derive(Clone, Copy, Debug)]
pub struct ProblemSnapshot<'a> {
    /// The classifier's problems for this cluster.
    pub problems: &'a [Problem],
    /// Categories that could not be read. Their counts and problems are ignored.
    pub unchecked: &'a [ProblemCategory],
    /// From [`count_recent_warnings`].
    pub warning_events: u32,
    /// From [`count_pending_claims`].
    pub pending_claims: u32,
}

impl ProblemSnapshot<'static> {
    /// A snapshot of a cluster that could not be read at all.
    pub const fn unavailable() -> Self {
        Self {
            problems: &[],
            unchecked: &ProblemCategory::ALL,
            warning_events: 0,
            pending_claims: 0,
        }
    }
}

/// One cluster's counts. A category is `None` when it could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProblemCounts {
    counts: [Option<u32>; ProblemCategory::ALL.len()],
}

impl ProblemCounts {
    /// The count of `category`, if it could be read.
    pub fn get(&self, category: ProblemCategory) -> Option<u32> {
        self.counts[category.index()]
    }

    /// The sum of every known count, saturating at `u32::MAX`.
    ///
    /// An unknown category adds nothing, so an unreadable cluster totals zero like a healthy one.
    /// Do not order clusters by this; use [`Self::attention_rank`].
    pub fn known_total(&self) -> u32 {
        self.counts
            .iter()
            .flatten()
            .fold(0, |total, count| total.saturating_add(*count))
    }

    /// How many categories could not be read.
    fn unknown_categories(&self) -> usize {
        self.counts.iter().filter(|count| count.is_none()).count()
    }

    /// This cluster's place in a worst-first order. See [`AttentionRank`].
    pub fn attention_rank(&self) -> AttentionRank {
        AttentionRank {
            known_problems: Reverse(self.known_total()),
            unknown_categories: Reverse(self.unknown_categories()),
        }
    }

    /// How `category` stands against `thresholds`.
    pub fn level(&self, category: ProblemCategory, thresholds: &Thresholds) -> ThresholdLevel {
        let count = self.get(category);
        match (category.threshold(), count) {
            (Some(metric), _) => thresholds.evaluate(metric, count.map(u64::from)),
            (None, None) => ThresholdLevel::Unknown,
            (None, Some(0)) => ThresholdLevel::Healthy,
            (None, Some(_)) => ThresholdLevel::Warning,
        }
    }

    /// The level of the category that needs the most attention.
    pub fn overall_level(&self, thresholds: &Thresholds) -> ThresholdLevel {
        ProblemCategory::ALL
            .into_iter()
            .map(|category| self.level(category, thresholds))
            .max()
            .unwrap_or(ThresholdLevel::Unknown)
    }
}

/// A sort key that puts the cluster needing the most attention first, so an ascending sort is
/// worst first.
///
/// Clusters compare by known problems, most first, then by unknown categories, most first. A
/// confirmed problem therefore outranks missing data: one known problem sorts ahead of a fully
/// unreadable cluster, while any unknown category sorts ahead of a fully healthy cluster.
///
/// Clusters with the same two numbers rank equal, whichever categories they fall in. Known
/// problems saturate as in [`ProblemCounts::known_total`]. Callers that need a stable display
/// order break ties themselves, for example by cluster name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AttentionRank {
    // The derived order compares fields top to bottom, so their order is the ranking.
    known_problems: Reverse<u32>,
    unknown_categories: Reverse<usize>,
}

/// Counts added across clusters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProblemTotals {
    known: [u32; ProblemCategory::ALL.len()],
    unknown_clusters: [u32; ProblemCategory::ALL.len()],
}

impl ProblemTotals {
    /// Adds one cluster's counts.
    pub fn add(&mut self, counts: &ProblemCounts) {
        for category in ProblemCategory::ALL {
            let index = category.index();
            match counts.get(category) {
                Some(count) => self.known[index] = self.known[index].saturating_add(count),
                None => {
                    self.unknown_clusters[index] = self.unknown_clusters[index].saturating_add(1)
                }
            }
        }
    }

    /// The sum over the clusters that could read `category`.
    pub fn known(&self, category: ProblemCategory) -> u32 {
        self.known[category.index()]
    }

    /// How many clusters could not read `category`, so [`Self::known`] may be an undercount.
    pub fn unknown_clusters(&self, category: ProblemCategory) -> u32 {
        self.unknown_clusters[category.index()]
    }
}

impl<'a> FromIterator<&'a ProblemCounts> for ProblemTotals {
    fn from_iter<I: IntoIterator<Item = &'a ProblemCounts>>(clusters: I) -> Self {
        let mut totals = Self::default();
        for counts in clusters {
            totals.add(counts);
        }
        totals
    }
}

/// Summarises successive snapshots of one cluster. See the [module docs](self#hold-down).
#[derive(Clone, Debug, Default)]
pub struct ProblemSummariser {
    /// When each problem with a hold-down was first seen. Holds only currently open problems.
    first_seen: HashMap<ProblemId, Timestamp>,
}

impl ProblemSummariser {
    /// The counts of `snapshot`, taken at `now`.
    pub fn summarise(&mut self, snapshot: &ProblemSnapshot<'_>, now: Timestamp) -> ProblemCounts {
        let unchecked = |category: ProblemCategory| snapshot.unchecked.contains(&category);
        let mut counts = ProblemCategory::ALL.map(|category| (!unchecked(category)).then_some(0));
        let mut set = |category: ProblemCategory, value: u32| {
            if let Some(count) = &mut counts[category.index()] {
                *count = value;
            }
        };
        set(ProblemCategory::WarningEvents, snapshot.warning_events);
        set(ProblemCategory::PendingClaims, snapshot.pending_claims);

        let mut counted = HashSet::new();
        let mut held = HashSet::new();
        for problem in snapshot.problems {
            let category = ProblemCategory::of_kind(problem.kind());
            // The wait runs even while the category's total is unknown: another kind in it may be
            // unreadable, and that says nothing about how long this problem has lasted.
            let mut waiting = false;
            if let Some(hold_down) = problem.hold_down() {
                held.insert(&problem.id);
                let first = self.first_seen.entry(problem.id.clone()).or_insert(now);
                // A clock that moved back must not leave the wait longer than it should be.
                *first = (*first).min(now);
                waiting = now.duration_since(*first) < hold_down;
            }
            let Some(count) = &mut counts[category.index()] else {
                continue;
            };
            if waiting {
                continue;
            }
            if counted.insert(CountedObject::of(category, &problem.id.subject)) {
                *count = count.saturating_add(1);
            }
        }
        self.first_seen
            .retain(|id, _| held.contains(id) || unchecked(ProblemCategory::of_kind(id.kind)));
        ProblemCounts { counts }
    }

    /// Whether `problem`, from the pass just summarised, is not counted yet because its hold-down
    /// has not passed. One this summariser has no start time for has not been held at all.
    pub fn is_settling(&self, problem: &Problem, now: Timestamp) -> bool {
        problem.hold_down().is_some_and(|hold_down| {
            self.first_seen
                .get(&problem.id)
                .is_none_or(|first| now.duration_since(*first) < hold_down)
        })
    }
}

/// The object a problem is counted against, so several problems of one object count once.
#[derive(PartialEq, Eq, Hash)]
struct CountedObject<'a> {
    category: ProblemCategory,
    workload: Option<WorkloadKind>,
    namespace: &'a str,
    name: &'a str,
    uid: Option<&'a str>,
}

impl<'a> CountedObject<'a> {
    fn of(category: ProblemCategory, subject: &'a ProblemSubject) -> Self {
        let (workload, namespace, name, uid) = match subject {
            ProblemSubject::Container {
                namespace,
                pod,
                pod_uid,
                ..
            } => (None, namespace.as_str(), pod, pod_uid),
            ProblemSubject::Node { name, uid } => (None, "", name, uid),
            ProblemSubject::Job {
                namespace,
                name,
                uid,
            } => (None, namespace.as_str(), name, uid),
            ProblemSubject::Workload {
                kind,
                namespace,
                name,
                uid,
            } => (Some(*kind), namespace.as_str(), name, uid),
        };
        Self {
            category,
            workload,
            namespace,
            name,
            uid: uid.as_deref(),
        }
    }
}

/// How many of `events` are Warnings last seen within [`WARNING_EVENT_WINDOW`] of `now`.
///
/// Counts event objects, as the filtered list shows them, not their repeat counts. A time ahead
/// of `now` (clock skew) counts as recent, and an event with no time at all does not count.
pub fn count_recent_warnings<'a>(
    events: impl IntoIterator<Item = &'a Event>,
    now: Timestamp,
) -> u32 {
    saturating_count(events.into_iter().filter(|event| {
        event.type_.as_deref() == Some("Warning")
            && last_seen(event).is_some_and(|seen| now.duration_since(seen) < WARNING_EVENT_WINDOW)
    }))
}

/// How many of `claims` are pending and not being deleted.
pub fn count_pending_claims<'a>(
    claims: impl IntoIterator<Item = &'a PersistentVolumeClaim>,
) -> u32 {
    saturating_count(claims.into_iter().filter(|claim| {
        claim.metadata.deletion_timestamp.is_none()
            && claim
                .status
                .as_ref()
                .is_some_and(|status| status.phase.as_deref() == Some("Pending"))
    }))
}

/// The latest time an event records.
fn last_seen(event: &Event) -> Option<Timestamp> {
    let series = event.series.as_ref();
    [
        series
            .and_then(|s| s.last_observed_time.as_ref())
            .map(|t| t.0),
        event.last_timestamp.as_ref().map(|t| t.0),
        event.event_time.as_ref().map(|t| t.0),
        event.metadata.creation_timestamp.as_ref().map(|t| t.0),
    ]
    .into_iter()
    .flatten()
    .max()
}

fn saturating_count<T>(items: impl Iterator<Item = T>) -> u32 {
    u32::try_from(items.count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;
