//! Keeps an open dashboard current from the cluster's telemetry feed.
//!
//! The view is rebuilt when the feed changes, and on a slow timer so that Warning events and
//! samples age out while a feed is quiet. Never per frame. The work runs off the UI thread, one
//! read at a time.

// module-tests: dashboard_wiring_tests.rs

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use flint_core::compare::{CompareTarget, SideRead as ObjectRead};
use flint_core::dashboard::{
    ProblemCategory, ProblemCounts, ProblemSnapshot, ProblemSummariser, count_pending_claims,
    count_recent_warnings,
};
use flint_core::metrics_trend::TrendStore;
use flint_core::problems::{Problem, ProblemSubject, WorkloadKind};
use flint_core::telemetry::{
    Availability, ClusterTelemetry, Inventory, KindCheck, MetricsCheck, SideRead,
    TelemetrySnapshot, UsageRead, VersionCheck, WatchedKind,
};
use flint_core::thresholds::Thresholds;
use flint_core::{ClusterId, ClusterSummary, OverviewInterest};
use gpui_kit::*;
use jiff::Timestamp;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use super::super::clusters::ClustersModel;
use super::super::list::{CountedObjects, ObjectFilter};
use super::super::registry::{self, OpenObject};
use super::{DashboardEvent, DashboardInput, DashboardModel, DashboardView, WarningRow, metrics};

/// How often an open dashboard reads again with no change from the feed.
const AGE_OUT_INTERVAL: Duration = Duration::from_secs(15);

/// The list behind one problem tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemList {
    /// The kind to open, as `OpenView` takes it.
    pub kind: &'static str,
    /// Empty when the tile counts nothing: the list then shows no rows.
    pub filter: ObjectFilter,
}

/// The lists behind each tile: one, one per kind for a tile that counts several, or none while
/// the counted objects cannot be told apart.
pub type ProblemLists = HashMap<ProblemCategory, Vec<ProblemList>>;

/// One run of a cluster's feed: a restarted feed has its own, though the cluster is the same.
#[derive(Clone, Debug)]
pub(in crate::ui) struct FeedId(Arc<str>);

impl FeedId {
    fn of(telemetry: &ClusterTelemetry) -> Self {
        Self(Arc::clone(&telemetry.snapshot.cluster))
    }

    #[cfg(test)]
    pub(in crate::ui) fn for_test(name: &str) -> Self {
        Self(Arc::from(name))
    }
}

impl PartialEq for FeedId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What keeps a [`DashboardView`] current. Dropping it stops the reads.
#[derive(Default)]
pub(super) struct Feed {
    source: Option<(WeakEntity<ClustersModel>, ClusterId)>,
    /// The slot this view follows: its server is the one that labels the feed's values.
    generation: u64,
    /// Keeps the cluster's feed running in watch mode On view while this view lives, and is the
    /// only way this view reads it, so it never shows a later slot of the same context.
    interest: Option<OverviewInterest>,
    /// The feed the values on screen came from. `None` for the waiting state.
    shown: Option<FeedId>,
    /// Values taken down since the last frame: their links may still be under the pointer.
    taken_down: Option<DashboardModel>,
    /// Stands in for the core's running feed.
    #[cfg(test)]
    test_current: Option<Option<FeedId>>,
    /// Stands in for the API server's answer about a Warning's object.
    #[cfg(test)]
    test_object: Option<ObjectRead>,
    reading: bool,
    /// Something changed during a read: read again when it ends.
    dirty: bool,
    /// The check of the Warning row last opened. A newer one replaces it.
    _object_check: Option<Task<()>>,
    _read: Option<Task<()>>,
    _updates: Option<Task<()>>,
    _registered: Option<Task<()>>,
    _age_out: Option<Task<()>>,
}

impl DashboardView {
    /// A dashboard of `cluster`'s slot `generation`, kept current until it is dropped.
    pub fn following(
        model: Entity<ClustersModel>,
        cluster: ClusterId,
        generation: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        let (waiting, lists) = read(
            None,
            &cluster,
            None,
            &Thresholds::default(),
            Timestamp::now(),
        );
        let mut view = Self::new(waiting, cx);
        view.lists = lists;
        let mut updates = model.read(cx).core().telemetry_updates();
        let interest = model.read(cx).core().open_overview_on(&cluster, generation);
        let registered = interest.registered();
        view.feed.interest = Some(interest);
        view.feed.source = Some((model.downgrade(), cluster));
        view.feed.generation = generation;
        // A feed already running changes nothing on the board when this view registers.
        view.feed._registered = Some(cx.spawn(async move |this, cx| {
            registered.await;
            let _ = this.update(cx, |view, cx| view.refresh(cx));
        }));
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

    /// The feed running for this view's cluster now.
    fn current_feed(&self) -> Option<FeedId> {
        #[cfg(test)]
        if let Some(current) = &self.feed.test_current {
            return current.clone();
        }
        let telemetry = self.feed.interest.as_ref()?.telemetry()?;
        Some(FeedId::of(&telemetry))
    }

    /// Whether what is shown came from the feed running now.
    pub(super) fn shows_current_feed(&self, _: &App) -> bool {
        self.feed.shown == self.current_feed()
    }

    /// Takes down values whose feed has stopped or been replaced.
    pub(super) fn clear_if_feed_gone(&mut self, cx: &mut Context<Self>) {
        if self.feed.shown.is_none() || self.shows_current_feed(cx) {
            return;
        }
        self.feed.shown = None;
        self.feed.taken_down = Some(self.model.clone());
        let cluster = self
            .feed
            .source
            .as_ref()
            .map_or("", |(_, cluster)| cluster.as_str());
        let (model, lists) = read(
            None,
            cluster,
            None,
            &Thresholds::default(),
            Timestamp::now(),
        );
        self.set_data(model, lists, cx);
    }

    /// Shows a finished read, unless its feed is no longer the one running.
    fn publish(
        &mut self,
        feed: Option<FeedId>,
        model: DashboardModel,
        lists: ProblemLists,
        cx: &mut Context<Self>,
    ) {
        if self.current_feed() != feed {
            self.feed.dirty = true;
            return;
        }
        self.feed.shown = feed;
        self.set_data(model, lists, cx);
    }

    /// Whether `link`, drawn from this view's values, must not open: its feed has gone.
    pub(in crate::ui) fn refuses_link(
        &self,
        link: impl Fn(&DashboardModel) -> bool,
        cx: &App,
    ) -> bool {
        self.feed.taken_down.as_ref().is_some_and(&link)
            || (link(&self.model) && !self.shows_current_feed(cx))
    }

    /// A new frame is on screen, so no click can come from the values taken down before it.
    pub(super) fn frame_drawn(&mut self) {
        self.feed.taken_down = None;
    }

    /// Reads the feed off the UI thread, unless a read is running; then it reads again after.
    pub(in crate::ui) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.clear_if_feed_gone(cx);
        #[cfg(test)]
        if self.feed.test_current.is_some() {
            return;
        }
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
        let Some(active) = slot_to_read(model.active_cluster(), &cluster, self.feed.generation)
        else {
            return;
        };
        let thresholds = model
            .settings
            .thresholds_for(&cluster, active.info.server.as_deref());
        let server = active.info.server.clone();
        let telemetry = self.feed.interest.as_ref().and_then(|i| i.telemetry());
        let feed = telemetry.as_ref().map(FeedId::of);
        self.feed.reading = true;
        self.feed.dirty = false;
        let job = cx.background_spawn(async move {
            read(
                telemetry.as_ref(),
                &cluster,
                server.as_deref(),
                &thresholds,
                Timestamp::now(),
            )
        });
        self.feed._read = Some(cx.spawn(async move |this, cx| {
            let (model, lists) = job.await;
            let _ = this.update(cx, |view, cx| {
                view.feed.reading = false;
                view.publish(feed, model, lists, cx);
                if view.feed.dirty {
                    view.refresh(cx);
                }
            });
        }));
    }
}

/// The links of one drawn frame, bound to the feed its values came from. A click can land after
/// the model moved on and before the next frame, so each is checked against the model then.
#[derive(Clone)]
pub(super) struct DrawnLinks {
    view: WeakEntity<DashboardView>,
    feed: Option<FeedId>,
    pub(super) focus: FocusHandle,
}

/// How a confirmed link is performed: [`registry::dispatch_in`].
type Dispatch =
    fn(&FocusHandle, &dyn Action, &mut Window, &mut App) -> Result<(), registry::ActionError>;

impl DrawnLinks {
    /// Whether the link `holds` in the values on screen, from the feed it was drawn from. If
    /// not, what is shown is read again.
    fn hold(&self, holds: impl Fn(&DashboardModel) -> bool, cx: &mut App) -> bool {
        self.view
            .update(cx, |view, cx| {
                let current = view.feed.shown == self.feed
                    && view.shows_current_feed(cx)
                    && holds(&view.model);
                if !current {
                    view.refresh(cx);
                }
                current
            })
            .unwrap_or(false)
    }

    /// Dispatches `action` while `holds` says the model still links to it.
    pub(super) fn on_click<E: ?Sized, A: 'static>(
        &self,
        action: A,
        holds: fn(&DashboardModel, &A) -> bool,
        perform: impl Fn(&E, &mut Window, &mut App) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        let links = self.clone();
        move |event, window, cx| {
            if links.hold(|model| holds(model, &action), cx) {
                perform(event, window, cx);
            }
        }
    }

    /// Opens a Warning's object through `dispatch` once the API server confirms it is the one
    /// the event is about.
    pub(super) fn on_warning_click<E: ?Sized>(
        &self,
        row: &WarningRow,
        open: OpenObject,
        dispatch: Dispatch,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        let links = self.clone();
        let (key, object, uid) = (row.key.clone(), row.object.clone(), row.object_uid.clone());
        move |_, window, cx| {
            if !links.hold(|model| model.links_warning(&key, &open), cx) {
                return;
            }
            let target = WarningTarget {
                open: open.clone(),
                object: object.clone(),
                uid: uid.clone(),
            };
            // A link this cluster refuses sends no request.
            if registry::check_in(&links.focus, &open, window, cx).is_err() {
                let why = target.cannot_open();
                let _ = links
                    .view
                    .update(cx, |_, cx| cx.emit(DashboardEvent::CannotOpen(why)));
                return;
            }
            let focus = links.focus.clone();
            let _ = links.view.update(cx, |view, cx| {
                view.check_warning_object(target, focus, dispatch, window, cx);
            });
        }
    }
}

/// The object a clicked Warning row is about.
struct WarningTarget {
    open: OpenObject,
    /// Its name as shown.
    object: String,
    uid: Option<String>,
}

impl WarningTarget {
    fn cannot_open(&self) -> String {
        format!("Cannot open {} on this cluster.", self.object)
    }

    /// Why `read` does not show this very object. With no UID, only that it exists is known.
    fn refusal(&self, read: &ObjectRead) -> Option<String> {
        let object = &self.object;
        match read {
            ObjectRead::Present(detail) => {
                let now = detail
                    .object
                    .pointer("/metadata/uid")
                    .and_then(|uid| uid.as_str());
                let replaced = self.uid.as_deref().is_some_and(|uid| now != Some(uid));
                replaced
                    .then(|| format!("{object} was replaced by a new object with the same name."))
            }
            ObjectRead::Missing => Some(format!("{object} no longer exists.")),
            ObjectRead::Reading | ObjectRead::Unavailable(_) => {
                Some(format!("Could not check {object}, so it was not opened."))
            }
        }
    }
}

impl DashboardView {
    pub(super) fn drawn_links(&self, cx: &Context<Self>) -> DrawnLinks {
        DrawnLinks {
            view: cx.weak_entity(),
            feed: self.feed.shown.clone(),
            focus: self.focus.clone(),
        }
    }

    /// Whether this view's slot is still the active cluster's.
    fn follows_active_slot(&self, cx: &App) -> bool {
        let Some((model, cluster)) = &self.feed.source else {
            return false;
        };
        model.upgrade().is_some_and(|model| {
            slot_to_read(
                model.read(cx).active_cluster(),
                cluster,
                self.feed.generation,
            )
            .is_some()
        })
    }

    /// One GET of `open`, on the definition this view follows. `None`: it cannot be asked.
    fn read_warning_object(
        &self,
        open: &OpenObject,
        cx: &mut Context<Self>,
    ) -> Option<Task<ObjectRead>> {
        #[cfg(test)]
        if let Some(read) = &self.feed.test_object {
            return Some(Task::ready(read.clone()));
        }
        let (model, cluster) = self.feed.source.as_ref()?;
        let model = model.upgrade()?;
        let model = model.read(cx);
        let slot = slot_to_read(model.active_cluster(), cluster, self.feed.generation)?;
        let entry = model
            .discovered(cluster)?
            .discovery
            .as_ref()?
            .nav_entry(&open.kind)
            .ok()?;
        let target = CompareTarget {
            kind: entry.id.clone(),
            namespace: open.namespace.clone().filter(|_| entry.namespaced),
            name: open.name.clone(),
        };
        let fetch = model.core().fetch_pinned_object(
            cluster,
            &slot.read_pin(),
            &target.kind,
            target.namespace.as_deref(),
            &target.name,
        );
        Some(cx.background_spawn(async move { ObjectRead::from_fetch(&target, fetch.await) }))
    }

    /// Asks for the object, then opens it if it is the event's and this view's feed still runs.
    /// Until then the overview stays.
    fn check_warning_object(
        &mut self,
        target: WarningTarget,
        focus: FocusHandle,
        dispatch: Dispatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(read) = self.read_warning_object(&target.open, cx) else {
            cx.emit(DashboardEvent::CannotOpen(target.cannot_open()));
            return;
        };
        let feed = self.feed.shown.clone();
        self.feed._object_check = Some(cx.spawn_in(window, async move |this, cx| {
            let read = read.await;
            let confirmed = this.update(cx, |view, cx| {
                if view.feed.shown != feed
                    || !view.shows_current_feed(cx)
                    || !view.follows_active_slot(cx)
                {
                    view.refresh(cx);
                    return false;
                }
                match target.refusal(&read) {
                    Some(why) => {
                        cx.emit(DashboardEvent::CannotOpen(why));
                        false
                    }
                    None => true,
                }
            });
            if !confirmed.unwrap_or(false) {
                return;
            }
            // Outside the view's update: the workspace closes this view as it opens the object.
            let opened = cx.update(|window, cx| dispatch(&focus, &target.open, window, cx).is_ok());
            if matches!(opened, Ok(false)) {
                let why = target.cannot_open();
                let _ = this.update(cx, |_, cx| cx.emit(DashboardEvent::CannotOpen(why)));
            }
        }));
    }
}

/// The active cluster's summary, if it describes the slot this view follows.
pub(in crate::ui) fn slot_to_read<'a>(
    active: Option<&'a ClusterSummary>,
    cluster: &str,
    generation: u64,
) -> Option<&'a ClusterSummary> {
    active.filter(|active| active.info.name == cluster && active.generation == generation)
}

#[cfg(test)]
impl DashboardView {
    /// Pretends `feed` is the one running now.
    pub fn set_running_feed_for_test(&mut self, feed: Option<FeedId>) {
        self.feed.test_current = Some(feed);
    }

    /// Answers the check of a Warning's object with `read`.
    pub fn set_object_read_for_test(&mut self, read: ObjectRead) {
        self.feed.test_object = Some(read);
    }

    /// A read of `feed` finishing now.
    pub fn finish_read_for_test(
        &mut self,
        feed: Option<FeedId>,
        model: DashboardModel,
        lists: ProblemLists,
        cx: &mut Context<Self>,
    ) {
        self.publish(feed, model, lists, cx);
    }
}

/// The dashboard of `telemetry`. No feed reads as still starting, so nothing shows as zero.
pub(super) fn read(
    telemetry: Option<&ClusterTelemetry>,
    cluster: &str,
    server: Option<&str>,
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
            node_usage: &UsageRead::Waiting,
            server,
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
        node_usage: &reads.node_usage,
        server,
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
        settling: Vec::new(),
        counts: ProblemSummariser::default().summarise(&ProblemSnapshot::unavailable(), now),
        metrics: MetricsCheck::Waiting,
        capacity: None,
        inventory: Inventory::default(),
        version: VersionCheck::Waiting,
    }
}

fn side_objects<T>(
    read: &SideRead<T>,
    counts: impl Fn(&T) -> bool,
    meta: impl Fn(&T) -> &ObjectMeta,
) -> CountedObjects {
    let mut objects = CountedObjects::default();
    for item in read.current().unwrap_or_default() {
        let meta = meta(item);
        if let (true, Some(name)) = (counts(item), meta.name.as_deref()) {
            objects.insert(meta.namespace.as_deref(), name, meta.uid.as_deref());
        }
    }
    objects
}

/// The objects each tile counts.
fn problem_lists(telemetry: &ClusterTelemetry, now: Timestamp) -> ProblemLists {
    let reads = &telemetry.reads;
    let warnings = side_objects(
        &reads.warning_events,
        |event| count_recent_warnings([event], now) == 1,
        |event| &event.metadata,
    );
    let claims = side_objects(
        &reads.pending_claims,
        |claim| count_pending_claims([claim]) == 1,
        |claim| &claim.metadata,
    );
    lists_of(
        &telemetry.snapshot.problems,
        &telemetry.snapshot.counts,
        warnings,
        claims,
    )
}

fn list(kind: &'static str, label: &str, objects: CountedObjects) -> ProblemList {
    ProblemList {
        kind,
        filter: ObjectFilter {
            label: label.to_owned(),
            objects: Arc::new(objects),
        },
    }
}

/// The lists behind each tile, from the problems and counts of one snapshot.
pub(in crate::ui) fn lists_of(
    problems: &[Problem],
    counts: &ProblemCounts,
    warnings: CountedObjects,
    claims: CountedObjects,
) -> ProblemLists {
    let mut pods = CountedObjects::default();
    let mut nodes = CountedObjects::default();
    let mut jobs = CountedObjects::default();
    let mut deployments = CountedObjects::default();
    let mut stateful_sets = CountedObjects::default();
    let mut daemon_sets = CountedObjects::default();
    for problem in problems {
        match &problem.id.subject {
            ProblemSubject::Container {
                namespace,
                pod,
                pod_uid,
                ..
            } => pods.insert(Some(namespace), pod, pod_uid.as_deref()),
            ProblemSubject::Node { name, uid } => nodes.insert(None, name, uid.as_deref()),
            ProblemSubject::Job {
                namespace,
                name,
                uid,
            } => jobs.insert(Some(namespace), name, uid.as_deref()),
            ProblemSubject::Workload {
                kind,
                namespace,
                name,
                uid,
            } => {
                let set = match kind {
                    WorkloadKind::Deployment => &mut deployments,
                    WorkloadKind::StatefulSet => &mut stateful_sets,
                    WorkloadKind::DaemonSet => &mut daemon_sets,
                };
                set.insert(Some(namespace), name, uid.as_deref());
            }
        }
    }
    let short = short_lists(
        deployments,
        stateful_sets,
        daemon_sets,
        counts.get(ProblemCategory::ShortWorkloads),
    );
    ProblemLists::from([
        (
            ProblemCategory::FailingPods,
            vec![list("pods", "failing pods", pods)],
        ),
        (
            ProblemCategory::NotReadyNodes,
            vec![list("nodes", "nodes not ready", nodes)],
        ),
        (
            ProblemCategory::WarningEvents,
            vec![list("events", "recent Warning events", warnings)],
        ),
        (
            ProblemCategory::PendingClaims,
            vec![list("persistentvolumeclaims", "pending claims", claims)],
        ),
        (
            ProblemCategory::FailedJobs,
            vec![list("jobs", "failed jobs", jobs)],
        ),
        (ProblemCategory::ShortWorkloads, short),
    ])
}

/// One list per kind the short-workloads tile counts.
///
/// StatefulSets and DaemonSets count only after their hold-down, which the feed tracks privately.
/// Deployments always count, so what is left of the count says how many of the others do: all or
/// none identifies them. Between the two it cannot be told which, so this returns no list rather
/// than one that disagrees with the tile.
fn short_lists(
    deployments: CountedObjects,
    stateful_sets: CountedObjects,
    daemon_sets: CountedObjects,
    counted: Option<u32>,
) -> Vec<ProblemList> {
    let held = stateful_sets.len() + daemon_sets.len();
    let held_counted = (counted.unwrap_or(0) as usize).saturating_sub(deployments.len());
    let (stateful_sets, daemon_sets) = if held_counted == 0 {
        (CountedObjects::default(), CountedObjects::default())
    } else if held_counted == held {
        (stateful_sets, daemon_sets)
    } else {
        return Vec::new();
    };
    let lists: Vec<ProblemList> = [
        ("deployments", "short Deployments", deployments),
        ("statefulsets", "short StatefulSets", stateful_sets),
        ("daemonsets", "short DaemonSets", daemon_sets),
    ]
    .into_iter()
    .filter(|(_, _, objects)| objects.len() > 0)
    .map(|(kind, label, objects)| list(kind, label, objects))
    .collect();
    if lists.is_empty() {
        vec![list(
            "deployments",
            "short workloads",
            CountedObjects::default(),
        )]
    } else {
        lists
    }
}
