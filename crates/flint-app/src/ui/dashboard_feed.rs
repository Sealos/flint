//! Keeps an open dashboard current from the cluster's telemetry feed.
//!
//! The view is rebuilt when the feed changes, and on a slow timer so that Warning events and
//! samples age out while a feed is quiet. Never per frame. The work runs off the UI thread, one
//! read at a time.

// module-tests: dashboard_wiring_tests.rs

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use flint_core::ClusterId;
use flint_core::dashboard::{
    ProblemCategory, ProblemSnapshot, ProblemSummariser, count_pending_claims,
    count_recent_warnings,
};
use flint_core::metrics_trend::TrendStore;
use flint_core::problems::{ProblemSubject, WorkloadKind};
use flint_core::telemetry::{
    Availability, ClusterTelemetry, KindCheck, MetricsCheck, SideRead, TelemetrySnapshot,
    WatchedKind,
};
use flint_core::thresholds::Thresholds;
use gpui_kit::*;
use jiff::Timestamp;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use super::super::clusters::ClustersModel;
use super::super::list::{ObjectFilter, object_key};
use super::{DashboardInput, DashboardModel, DashboardView, metrics};

/// How often an open dashboard reads again with no change from the feed.
const AGE_OUT_INTERVAL: Duration = Duration::from_secs(15);

/// The list behind one problem tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemList {
    /// The kind to open, as `OpenView` takes it.
    pub kind: &'static str,
    /// Empty when the tile counts nothing: its kind then opens whole.
    pub filter: ObjectFilter,
}

pub type ProblemLists = HashMap<ProblemCategory, ProblemList>;

/// What keeps a [`DashboardView`] current. Dropping it stops the reads.
#[derive(Default)]
pub(super) struct Feed {
    source: Option<(WeakEntity<ClustersModel>, ClusterId)>,
    reading: bool,
    /// Something changed during a read: read again when it ends.
    dirty: bool,
    _read: Option<Task<()>>,
    _updates: Option<Task<()>>,
    _age_out: Option<Task<()>>,
}

impl DashboardView {
    /// A dashboard of `cluster`, kept current until it is dropped.
    pub fn following(
        model: Entity<ClustersModel>,
        cluster: ClusterId,
        cx: &mut Context<Self>,
    ) -> Self {
        let (waiting, lists) = read(None, &cluster, &Thresholds::default(), Timestamp::now());
        let mut view = Self::new(waiting, cx);
        view.lists = lists;
        let mut updates = model.read(cx).core().telemetry_updates();
        view.feed.source = Some((model.downgrade(), cluster));
        view.feed._updates = Some(cx.spawn(async move |this, cx| {
            while updates.changed().await {
                if this.update(cx, |view, cx| view.refresh(cx)).is_err() {
                    break;
                }
            }
        }));
        view.feed._age_out = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AGE_OUT_INTERVAL).await;
                if this.update(cx, |view, cx| view.refresh(cx)).is_err() {
                    break;
                }
            }
        }));
        view
    }

    /// Reads the feed off the UI thread, unless a read is running; then it reads again after.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.feed.reading {
            self.feed.dirty = true;
            return;
        }
        let Some((model, cluster)) = self.feed.source.clone() else {
            return;
        };
        let Some(model) = model.upgrade() else {
            return;
        };
        let model = model.read(cx);
        // The workspace closes this view on a switch; until it does, nothing new is shown.
        let Some(active) = model
            .active_cluster()
            .filter(|active| active.info.name == cluster)
        else {
            return;
        };
        let thresholds = model
            .settings
            .thresholds_for(&cluster, active.info.server.as_deref());
        let telemetry = model.core().telemetry(&cluster);
        self.feed.reading = true;
        self.feed.dirty = false;
        let job = cx.background_spawn(async move {
            read(telemetry.as_ref(), &cluster, &thresholds, Timestamp::now())
        });
        self.feed._read = Some(cx.spawn(async move |this, cx| {
            let (model, lists) = job.await;
            let _ = this.update(cx, |view, cx| {
                view.feed.reading = false;
                view.set_data(model, lists, cx);
                if view.feed.dirty {
                    view.refresh(cx);
                }
            });
        }));
    }
}

/// The dashboard of `telemetry`. No feed reads as still starting, so nothing shows as zero.
pub(super) fn read(
    telemetry: Option<&ClusterTelemetry>,
    cluster: &str,
    thresholds: &Thresholds,
    now: Timestamp,
) -> (DashboardModel, ProblemLists) {
    let Some(telemetry) = telemetry else {
        let snapshot = waiting_snapshot(cluster, now);
        let trends = TrendStore::new();
        let model = DashboardModel::build(&DashboardInput {
            snapshot: &snapshot,
            trends: &trends,
            thresholds,
            warning_events: None,
            pending_claims: None,
            now,
        });
        return (model, ProblemLists::new());
    };
    let reads = &telemetry.reads;
    // The feed's metrics poll waits on the trends' lock, so only the metrics are built under it.
    let no_trends = TrendStore::new();
    let input = DashboardInput {
        snapshot: &telemetry.snapshot,
        trends: &no_trends,
        thresholds,
        warning_events: reads.warning_events.current(),
        pending_claims: reads.pending_claims.current(),
        now,
    };
    let mut model = DashboardModel::build(&input);
    model.metrics = telemetry.with_trends(|trends| metrics(&DashboardInput { trends, ..input }));
    (model, problem_lists(telemetry, now))
}

fn waiting_snapshot(cluster: &str, now: Timestamp) -> TelemetrySnapshot {
    TelemetrySnapshot {
        cluster: cluster.into(),
        at: now,
        availability: Availability::Starting,
        kinds: WatchedKind::ALL.map(|kind| (kind, KindCheck::Starting)),
        problems: Arc::from([]),
        unchecked: Vec::new(),
        counts: ProblemSummariser::default().summarise(&ProblemSnapshot::unavailable(), now),
        metrics: MetricsCheck::Waiting,
    }
}

fn meta_key(meta: &ObjectMeta) -> Option<String> {
    Some(object_key(meta.namespace.as_deref(), meta.name.as_deref()?))
}

fn side_keys<T>(read: &SideRead<T>, counts: impl Fn(&T) -> Option<String>) -> HashSet<String> {
    read.current()
        .map(|items| items.iter().filter_map(counts).collect())
        .unwrap_or_default()
}

/// The objects each tile counts.
fn problem_lists(telemetry: &ClusterTelemetry, now: Timestamp) -> ProblemLists {
    let reads = &telemetry.reads;
    let mut pods = HashSet::new();
    let mut nodes = HashSet::new();
    let mut jobs = HashSet::new();
    let mut deployments = HashSet::new();
    let mut stateful_sets = HashSet::new();
    let mut daemon_sets = HashSet::new();
    for problem in telemetry.snapshot.problems.iter() {
        match &problem.id.subject {
            ProblemSubject::Container { namespace, pod, .. } => {
                pods.insert(object_key(Some(namespace), pod));
            }
            ProblemSubject::Node { name, .. } => {
                nodes.insert(object_key(None, name));
            }
            ProblemSubject::Job {
                namespace, name, ..
            } => {
                jobs.insert(object_key(Some(namespace), name));
            }
            ProblemSubject::Workload {
                kind,
                namespace,
                name,
                ..
            } => {
                let set = match kind {
                    WorkloadKind::Deployment => &mut deployments,
                    WorkloadKind::StatefulSet => &mut stateful_sets,
                    WorkloadKind::DaemonSet => &mut daemon_sets,
                };
                set.insert(object_key(Some(namespace), name));
            }
        }
    }
    let warnings = side_keys(&reads.warning_events, |event| {
        (count_recent_warnings([event], now) == 1)
            .then(|| meta_key(&event.metadata))
            .flatten()
    });
    let claims = side_keys(&reads.pending_claims, |claim| {
        (count_pending_claims([claim]) == 1)
            .then(|| meta_key(&claim.metadata))
            .flatten()
    });
    // A list shows one kind: short workloads open the kind with the most.
    let (short_kind, short_label, short) = [
        ("deployments", "short Deployments", deployments),
        ("statefulsets", "short StatefulSets", stateful_sets),
        ("daemonsets", "short DaemonSets", daemon_sets),
    ]
    .into_iter()
    .rev()
    .max_by_key(|(_, _, keys)| keys.len())
    .expect("three workload kinds");
    [
        (ProblemCategory::FailingPods, "pods", "failing pods", pods),
        (
            ProblemCategory::NotReadyNodes,
            "nodes",
            "nodes not ready",
            nodes,
        ),
        (
            ProblemCategory::WarningEvents,
            "events",
            "recent Warning events",
            warnings,
        ),
        (
            ProblemCategory::PendingClaims,
            "persistentvolumeclaims",
            "pending claims",
            claims,
        ),
        (ProblemCategory::FailedJobs, "jobs", "failed jobs", jobs),
        (
            ProblemCategory::ShortWorkloads,
            short_kind,
            short_label,
            short,
        ),
    ]
    .into_iter()
    .map(|(category, kind, label, keys)| {
        let filter = ObjectFilter {
            label: label.to_owned(),
            keys: Arc::new(keys),
        };
        (category, ProblemList { kind, filter })
    })
    .collect()
}
