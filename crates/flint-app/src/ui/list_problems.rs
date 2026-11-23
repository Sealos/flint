//! The "Problems" toggle of a list and the problem counts in the navigation (#273).
//!
//! What counts as a problem comes from the cluster's telemetry feed, which classifies typed
//! objects with [`flint_core::problems::Classifier`]. This module only reads a finished
//! [`TelemetrySnapshot`], so it adds no LIST or watch.
//!
//! A row is hidden only when its kind was checked on its cluster and the row is not a problem.
//! A kind that could not be checked keeps its rows and says so, and its count is never zero.
//!
//! Matching a row is one hash lookup, on the background job that already filters the list.

// module-tests: list_problems_tests.rs

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use flint_core::problems::{ProblemId, ProblemSubject, WorkloadKind};
use flint_core::telemetry::{Availability, KindCheck, TelemetrySnapshot, WatchedKind};
use flint_core::{ClusterId, KindId, Row};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Selectable as _, Sizable as _};

use super::CountedObjects;

/// The element id of the banner saying a kind could not be checked.
pub const UNCHECKED_BANNER: &str = "list-problems-unchecked";

/// The toolbar's toggle. The caller adds what a click does.
pub fn toggle_button(on: bool) -> Button {
    Button::new("toggle-problems")
        .xsmall()
        .ghost()
        .label("Problems")
        .selected(on)
        .tooltip("Show only rows with problems")
}

/// A list kind the shared problem rules can judge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProblemListKind {
    Pods,
    Nodes,
    Jobs,
    Deployments,
    StatefulSets,
    DaemonSets,
}

impl ProblemListKind {
    const ALL: [Self; 6] = [
        Self::Pods,
        Self::Nodes,
        Self::Jobs,
        Self::Deployments,
        Self::StatefulSets,
        Self::DaemonSets,
    ];

    /// The problem kind of a list, by exact API group: a CRD also named `pods` is not one.
    pub fn of(kind: &KindId) -> Option<Self> {
        match kind.as_str() {
            "pods" => Some(Self::Pods),
            "nodes" => Some(Self::Nodes),
            "jobs.batch" => Some(Self::Jobs),
            "deployments.apps" => Some(Self::Deployments),
            "statefulsets.apps" => Some(Self::StatefulSets),
            "daemonsets.apps" => Some(Self::DaemonSets),
            _ => None,
        }
    }

    fn kind_id(self) -> KindId {
        match self {
            Self::Pods => KindId::pods(),
            Self::Nodes => KindId::new("", "nodes"),
            Self::Jobs => KindId::new("batch", "jobs"),
            Self::Deployments => KindId::new("apps", "deployments"),
            Self::StatefulSets => KindId::new("apps", "statefulsets"),
            Self::DaemonSets => KindId::new("apps", "daemonsets"),
        }
    }

    fn watched(self) -> WatchedKind {
        match self {
            Self::Pods => WatchedKind::Pods,
            Self::Nodes => WatchedKind::Nodes,
            Self::Jobs => WatchedKind::Jobs,
            Self::Deployments => WatchedKind::Deployments,
            Self::StatefulSets => WatchedKind::StatefulSets,
            Self::DaemonSets => WatchedKind::DaemonSets,
        }
    }
}

/// The object a problem is about, and the list kind it appears in.
fn subject(subject: &ProblemSubject) -> (ProblemListKind, Option<&str>, &str, Option<&str>) {
    match subject {
        ProblemSubject::Container {
            namespace,
            pod,
            pod_uid,
            ..
        } => (
            ProblemListKind::Pods,
            Some(namespace),
            pod,
            pod_uid.as_deref(),
        ),
        ProblemSubject::Node { name, uid } => (ProblemListKind::Nodes, None, name, uid.as_deref()),
        ProblemSubject::Job {
            namespace,
            name,
            uid,
        } => (ProblemListKind::Jobs, Some(namespace), name, uid.as_deref()),
        ProblemSubject::Workload {
            kind,
            namespace,
            name,
            uid,
        } => {
            let kind = match kind {
                WorkloadKind::Deployment => ProblemListKind::Deployments,
                WorkloadKind::StatefulSet => ProblemListKind::StatefulSets,
                WorkloadKind::DaemonSet => ProblemListKind::DaemonSets,
            };
            (kind, Some(namespace), name, uid.as_deref())
        }
    }
}

/// What one cluster's feed says about one kind.
#[derive(Clone, Debug)]
enum KindProblems {
    Unchecked,
    Checked(Arc<CountedObjects>),
}

/// The objects with problems on one cluster, per kind. Cheap to clone.
#[derive(Clone, Debug)]
pub struct ClusterProblems {
    by_kind: [KindProblems; ProblemListKind::ALL.len()],
}

impl Default for ClusterProblems {
    fn default() -> Self {
        Self::unchecked()
    }
}

impl ClusterProblems {
    /// Nothing is known: no feed, or one that cannot read the cluster now.
    pub fn unchecked() -> Self {
        Self {
            by_kind: ProblemListKind::ALL.map(|_| KindProblems::Unchecked),
        }
    }

    /// What `snapshot` says. Last values kept from an unreachable cluster are not a check, and a
    /// problem still in its hold-down is not counted, as on the dashboard.
    pub fn read(snapshot: Option<&TelemetrySnapshot>) -> Self {
        let Some(snapshot) = snapshot.filter(|snapshot| {
            matches!(
                snapshot.availability,
                Availability::Checked | Availability::Partial
            )
        }) else {
            return Self::unchecked();
        };
        let settling: HashSet<&ProblemId> = snapshot.settling.iter().collect();
        let mut found: [CountedObjects; ProblemListKind::ALL.len()] = Default::default();
        for problem in snapshot.problems.iter() {
            if settling.contains(&problem.id) {
                continue;
            }
            let (kind, namespace, name, uid) = subject(&problem.id.subject);
            found[kind as usize].insert(namespace, name, uid);
        }
        let by_kind = ProblemListKind::ALL.map(|kind| {
            if *snapshot.kind(kind.watched()) == KindCheck::Current {
                KindProblems::Checked(Arc::new(std::mem::take(&mut found[kind as usize])))
            } else {
                KindProblems::Unchecked
            }
        });
        Self { by_kind }
    }

    fn of(&self, kind: ProblemListKind) -> &KindProblems {
        &self.by_kind[kind as usize]
    }

    fn checked(&self, kind: ProblemListKind) -> bool {
        matches!(self.of(kind), KindProblems::Checked(_))
    }

    /// Whether `row` stays with the toggle on: a problem, or of a kind that was not checked.
    fn keeps(&self, kind: ProblemListKind, row: &Row) -> bool {
        match self.of(kind) {
            KindProblems::Unchecked => true,
            KindProblems::Checked(objects) => {
                objects.matches(row.namespace.as_deref(), &row.name, &row.uid)
            }
        }
    }

    /// Whether both hold the same values, compared by identity so that a pass is O(kinds).
    fn same(&self, other: &Self) -> bool {
        self.by_kind
            .iter()
            .zip(&other.by_kind)
            .all(|pair| match pair {
                (KindProblems::Unchecked, KindProblems::Unchecked) => true,
                (KindProblems::Checked(a), KindProblems::Checked(b)) => Arc::ptr_eq(a, b) || a == b,
                _ => false,
            })
    }
}

/// A navigation entry's problem count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemBadge {
    /// `3`, `3+` when some clusters could not be checked, or `?` when none could.
    pub text: String,
    /// The same in words, for the tooltip and assistive technology.
    pub detail: String,
}

impl ProblemBadge {
    /// `None` when every cluster was checked and has no problem of this kind.
    fn new(found: usize, unchecked: usize, clusters: usize) -> Option<Self> {
        let objects = |n: usize| {
            if n == 1 {
                "1 object".to_owned()
            } else {
                format!("{n} objects")
            }
        };
        match (found, unchecked) {
            _ if clusters == 0 => None,
            (0, 0) => None,
            (found, 0) => Some(Self {
                text: found.to_string(),
                detail: format!("{} with problems", objects(found)),
            }),
            (0, unchecked) => Some(Self {
                text: "?".to_owned(),
                detail: if clusters == 1 {
                    "Problems cannot be checked".to_owned()
                } else {
                    format!("Problems cannot be checked on {unchecked} of {clusters} clusters")
                },
            }),
            (found, unchecked) => Some(Self {
                text: format!("{found}+"),
                detail: format!(
                    "{} with problems; {unchecked} of {clusters} clusters cannot be checked",
                    objects(found)
                ),
            }),
        }
    }
}

/// The navigation's badges over `clusters`.
pub fn nav_badges(clusters: &[ClusterProblems]) -> HashMap<KindId, ProblemBadge> {
    ProblemListKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let mut found = 0;
            let mut unchecked = 0;
            for cluster in clusters {
                match cluster.of(kind) {
                    KindProblems::Unchecked => unchecked += 1,
                    KindProblems::Checked(objects) => found += objects.len(),
                }
            }
            let badge = ProblemBadge::new(found, unchecked, clusters.len())?;
            Some((kind.kind_id(), badge))
        })
        .collect()
}

/// One list's toggle, and what is known about the clusters it shows.
#[derive(Clone, Debug, Default)]
pub struct ProblemFilter {
    on: bool,
    clusters: HashMap<ClusterId, ClusterProblems>,
}

impl ProblemFilter {
    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn toggle(&mut self) {
        self.on = !self.on;
    }

    /// Takes the newest values. Returns whether the rows must be filtered again.
    pub fn update(&mut self, clusters: HashMap<ClusterId, ClusterProblems>) -> bool {
        let same = self.clusters.len() == clusters.len()
            && clusters
                .iter()
                .all(|(id, new)| self.clusters.get(id).is_some_and(|known| known.same(new)));
        self.clusters = clusters;
        !same
    }

    fn cluster(&self, cluster: &str) -> ClusterProblems {
        self.clusters.get(cluster).cloned().unwrap_or_default()
    }

    /// What a single-cluster job filters by. `None` with the toggle off or for another kind.
    pub fn row_filter(&self, kind: &KindId, cluster: &str) -> Option<RowFilter> {
        let kind = ProblemListKind::of(kind).filter(|_| self.on)?;
        Some(RowFilter {
            kind,
            problems: self.cluster(cluster),
        })
    }

    /// What a combined job filters by, one entry per member.
    pub fn member_filter<'a>(
        &self,
        kind: Option<&KindId>,
        members: impl Iterator<Item = &'a str>,
    ) -> Option<MemberFilter> {
        let kind = kind.and_then(ProblemListKind::of).filter(|_| self.on)?;
        Some(MemberFilter {
            kind,
            members: members.map(|cluster| self.cluster(cluster)).collect(),
        })
    }

    /// Whether the toggle is hiding rows of `kind` on `cluster`.
    pub fn is_checked(&self, kind: &KindId, cluster: &str) -> bool {
        self.on && ProblemListKind::of(kind).is_some_and(|kind| self.cluster(cluster).checked(kind))
    }

    /// With the toggle on, says which of `clusters` (id, name on screen) could not be checked.
    pub fn unchecked_note<'a>(
        &self,
        kind: &KindId,
        label: &str,
        clusters: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> Option<String> {
        let list_kind = ProblemListKind::of(kind).filter(|_| self.on)?;
        let clusters: Vec<(&str, &str)> = clusters.collect();
        let missing: Vec<&str> = clusters
            .iter()
            .filter(|(id, _)| !self.cluster(id).checked(list_kind))
            .map(|&(_, shown)| shown)
            .collect();
        match missing.as_slice() {
            [] => None,
            _ if clusters.len() == 1 => Some(format!(
                "Problems in {label} cannot be checked right now, so every row is shown."
            )),
            names => Some(format!(
                "Problems in {label} cannot be checked on {}, so all of its rows are shown.",
                names.join(", ")
            )),
        }
    }
}

/// A single-cluster job's filter.
#[derive(Clone, Debug)]
pub struct RowFilter {
    kind: ProblemListKind,
    problems: ClusterProblems,
}

impl RowFilter {
    pub fn keeps(&self, row: &Row) -> bool {
        self.problems.keeps(self.kind, row)
    }
}

/// A combined job's filter.
#[derive(Clone, Debug)]
pub struct MemberFilter {
    kind: ProblemListKind,
    members: Vec<ClusterProblems>,
}

impl MemberFilter {
    /// A member with no entry was not checked, so its rows stay.
    pub fn keeps(&self, member: usize, row: &Row) -> bool {
        self.members
            .get(member)
            .is_none_or(|problems| problems.keeps(self.kind, row))
    }
}
