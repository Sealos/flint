//! The cluster overview in the main window: opening it, its tiles, and a cluster switch.

use std::collections::HashMap;
use std::sync::Arc;

use flint_core::compare::SideRead as ObjectRead;
use flint_core::dashboard::{ProblemCategory, ProblemCounts, ProblemSnapshot, ProblemSummariser};
use flint_core::problems::{
    Problem, ProblemId, ProblemKind, ProblemSeverity, ProblemSubject, WORKLOAD_HOLD_DOWN,
    WorkloadKind,
};
use flint_core::{ClusterDiscovery, ConnectionState, Event, ObjectDetail, ServerVersion};
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{Entity, TestAppContext};
use jiff::Timestamp;
use serde_json::{Value, json};

use super::dashboard::{
    DashboardModel, DashboardView, FeedId, ProblemList, ProblemLists, ProblemRow, ProblemTile,
    ProblemsPanel, WarningRow, WarningsPanel, lists_of, slot_to_read,
};
use super::list::{CountedObjects, ObjectFilter};
use super::registry::{OpenObject, Scope, tests::assert_binds_scope};
use super::support_tests::{
    CURRENT_GENERATION, TestWorkspace, WAIT, discovery, open_workspace, pods, standard_sections,
};
use flint_core::thresholds::ThresholdLevel;

const OPEN: &str = "workspace::OpenClusterOverview";
const OPEN_LIST: &str = "dashboard::OpenProblemList";
const OPEN_SHORT: &str = "dashboard::OpenShortWorkloads";
/// The button of the row `show_warning_row` draws.
const WARNING_WEB_0: &str = "dashboard-warning-event-web-0";

fn dashboard(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<Entity<DashboardView>> {
    t.workspace.read_with(cx, |ws, _| ws.dashboard_view())
}

async fn open(cx: &mut TestAppContext, t: &TestWorkspace) -> Entity<DashboardView> {
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    t.dispatch(cx, OPEN, Value::Null).unwrap();
    dashboard(cx, t).expect("the overview is open")
}

/// The overview with its feed held, so a background read cannot replace values set by hand.
async fn open_held(cx: &mut TestAppContext, t: &TestWorkspace) -> Entity<DashboardView> {
    let view = open(cx, t).await;
    let feed = FeedId::for_test("kind-alpha");
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(Some(feed.clone()));
        let (model, lists) = (view.model().clone(), ProblemLists::new());
        view.finish_read_for_test(Some(feed), model, lists, cx);
    });
    view
}

/// `(namespace, name, uid)` of each object. The fake lists give a row the UID `uid-<name>`.
fn counted(objects: &[(&str, &str, &str)]) -> CountedObjects {
    let mut counted = CountedObjects::default();
    for (namespace, name, uid) in objects {
        counted.insert(Some(namespace), name, Some(uid));
    }
    counted
}

fn model_counting(view: &DashboardView, category: ProblemCategory, count: u32) -> DashboardModel {
    let mut model: DashboardModel = view.model().clone();
    let at = ProblemCategory::ALL
        .iter()
        .position(|c| *c == category)
        .expect("every category has a tile");
    model.tiles[at] = ProblemTile {
        category,
        label: "Problems",
        count: Some(count),
        level: ThresholdLevel::Warning,
    };
    model
}

fn pod_list(objects: CountedObjects) -> ProblemLists {
    let list = ProblemList {
        kind: "pods",
        filter: ObjectFilter {
            label: "failing pods".to_owned(),
            objects: Arc::new(objects),
        },
    };
    HashMap::from([(ProblemCategory::FailingPods, vec![list])])
}

/// Shows failing pods, as a read of the feed would.
fn show_failing_pods(
    cx: &mut TestAppContext,
    view: &Entity<DashboardView>,
    objects: &[(&str, &str, &str)],
) {
    let lists = pod_list(counted(objects));
    let count = objects.len() as u32;
    view.update(cx, |view, cx| {
        let model = model_counting(view, ProblemCategory::FailingPods, count);
        view.set_data(model, lists, cx);
    });
    cx.run_until_parked();
}

fn short(kind: WorkloadKind, name: &str) -> Problem {
    Problem {
        id: ProblemId {
            cluster: Arc::from("kind-alpha"),
            kind: ProblemKind::WorkloadShort,
            subject: ProblemSubject::Workload {
                kind,
                namespace: "demo".to_owned(),
                name: name.to_owned(),
                uid: Some(format!("uid-{name}")),
            },
        },
        owner: None,
        owner_known: true,
        reason: None,
        since: None,
    }
}

fn counts(
    problems: &[Problem],
    summariser: &mut ProblemSummariser,
    at: Timestamp,
) -> ProblemCounts {
    summariser.summarise(
        &ProblemSnapshot {
            problems,
            unchecked: &[],
            warning_events: 0,
            pending_claims: 0,
        },
        at,
    )
}

fn short_kinds(problems: &[Problem], counts: &ProblemCounts) -> Vec<(&'static str, usize)> {
    lists_of(
        problems,
        counts,
        CountedObjects::default(),
        CountedObjects::default(),
    )[&ProblemCategory::ShortWorkloads]
        .iter()
        .map(|list| (list.kind, list.filter.objects.len()))
        .collect()
}

#[test]
fn short_workloads_list_only_what_the_tile_counted() {
    let start = Timestamp::UNIX_EPOCH;
    let later = start + WORKLOAD_HOLD_DOWN;
    let mut summariser = ProblemSummariser::default();
    let both = [
        short(WorkloadKind::Deployment, "web"),
        short(WorkloadKind::StatefulSet, "db"),
    ];
    // Inside its grace period the StatefulSet is not counted, so it is not listed.
    let first = counts(&both, &mut summariser, start);
    assert_eq!(first.get(ProblemCategory::ShortWorkloads), Some(1));
    assert_eq!(short_kinds(&both, &first), [("deployments", 1)]);
    // Once it counts, each kind has its own list.
    let second = counts(&both, &mut summariser, later);
    assert_eq!(second.get(ProblemCategory::ShortWorkloads), Some(2));
    assert_eq!(
        short_kinds(&both, &second),
        [("deployments", 1), ("statefulsets", 1)]
    );
    // One held StatefulSet of two: no list can say which, so none is offered.
    let mixed = [
        short(WorkloadKind::StatefulSet, "db"),
        short(WorkloadKind::StatefulSet, "cache"),
    ];
    let third = counts(&mixed, &mut summariser, later);
    assert_eq!(third.get(ProblemCategory::ShortWorkloads), Some(1));
    assert!(short_kinds(&mixed, &third).is_empty());
}

#[gpui_kit::test]
async fn an_opened_alert_shows_the_overview_only_on_its_own_cluster(cx: &mut TestAppContext) {
    use super::alert_center::{AlertCenter, Alerts};
    use crate::notifications::FreedesktopSender;
    use gpui_kit::AppContext as _;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    let model = t.model.clone();
    let center = cx.update(|cx| {
        let center = cx.new(|cx| AlertCenter::new(model, Arc::new(FreedesktopSender), cx));
        cx.set_global(Alerts(center.clone()));
        center
    });
    // On a list, so that only the alert can bring the overview back.
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "pods"}))
        .unwrap();
    cx.run_until_parked();
    assert!(
        dashboard(cx, &t).is_none(),
        "the list replaced the overview"
    );
    let generation = t.model.read_with(cx, |model, _| {
        model.active_cluster().expect("alpha is active").generation
    });
    let ask = |cx: &mut TestAppContext, context: &'static str, generation: u64| {
        center.update(cx, |center, _| center.want_overview(context, generation));
        t.model.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    };

    ask(cx, "kind-beta", generation);
    assert!(dashboard(cx, &t).is_none(), "another cluster's alert");
    ask(cx, "kind-alpha", generation + 1);
    assert!(dashboard(cx, &t).is_none(), "a re-pointed cluster's alert");

    ask(cx, "kind-alpha", generation);
    assert!(dashboard(cx, &t).is_some(), "the active cluster's alert");
    assert!(center.read_with(cx, |center, _| center.wanted_overview().is_none()));
}

#[gpui_kit::test]
async fn each_counted_kind_opens_its_own_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let lists = HashMap::from([(
        ProblemCategory::ShortWorkloads,
        vec![
            ProblemList {
                kind: "deployments",
                filter: ObjectFilter {
                    label: "short Deployments".to_owned(),
                    objects: Arc::new(counted(&[("demo", "web", "uid-web")])),
                },
            },
            ProblemList {
                kind: "statefulsets",
                filter: ObjectFilter {
                    label: "short StatefulSets".to_owned(),
                    objects: Arc::new(counted(&[("demo", "db", "uid-db")])),
                },
            },
        ],
    )]);
    view.update(cx, |view, cx| {
        let model = model_counting(view, ProblemCategory::ShortWorkloads, 2);
        view.set_data(model, lists, cx);
    });
    cx.run_until_parked();
    // The tile alone cannot open two kinds.
    let _ = t.dispatch(cx, OPEN_LIST, json!({"category": "short_workloads"}));
    assert!(dashboard(cx, &t).is_some());
    t.dispatch(cx, OPEN_SHORT, json!({"kind": "deployments"}))
        .unwrap();
    let list = t.list(cx).expect("the Deployments list is open");
    let filter = list.read_with(cx, |list, _| list.only().cloned());
    assert_eq!(filter.expect("narrowed").label, "short Deployments");
}

#[gpui_kit::test]
async fn a_read_of_a_stopped_feed_is_not_shown_or_opened(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open(cx, &t).await;
    let old = FeedId::for_test("kind-alpha");
    let new = FeedId::for_test("kind-alpha");
    let lists = pod_list(counted(&[("demo", "web-0", "uid-web-0")]));
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(Some(old.clone()));
        let model = model_counting(view, ProblemCategory::FailingPods, 1);
        view.finish_read_for_test(Some(old.clone()), model, lists.clone(), cx);
    });
    let failing = |view: &DashboardView| view.model().tiles[0].count;
    assert_eq!(view.read_with(cx, |v, _| failing(v)), Some(1));

    // A restart under the same cluster name: the old read no longer publishes.
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(Some(new.clone()));
        let model = model_counting(view, ProblemCategory::FailingPods, 9);
        view.finish_read_for_test(Some(old.clone()), model, lists.clone(), cx);
    });
    assert_eq!(view.read_with(cx, |v, _| failing(v)), Some(1));

    // The tile still drawn opens nothing, and takes the old values down.
    let _ = t.dispatch(cx, OPEN_LIST, json!({"category": "failing_pods"}));
    cx.run_until_parked();
    assert!(t.list(cx).is_none());
    assert_eq!(view.read_with(cx, |v, _| failing(v)), None);

    // Paused: nothing comes back from the old feed.
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(None);
        view.refresh(cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(failing(view), None);
        assert!(view.problem_list(ProblemCategory::FailingPods).is_none());
    });
}

#[gpui_kit::test]
async fn a_tile_opens_objects_outside_the_namespace_picker(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    t.dispatch(
        cx,
        "workspace::ToggleNamespace",
        json!({"namespace": "dev"}),
    )
    .unwrap();
    show_failing_pods(cx, &view, &[("prod", "api", "uid-api")]);
    t.dispatch(cx, OPEN_LIST, json!({"category": "failing_pods"}))
        .unwrap();
    t.feed_list(cx, pods(&[("dev", "web", 0), ("prod", "api", 0)]))
        .await;
    assert_eq!(t.row_count(cx), 1, "the counted pod shows");
    let saved = t
        .workspace
        .read_with(cx, |ws, _| ws.saved_namespace_filter("kind-alpha"));
    assert_eq!(saved, ["dev"], "the saved picker is unchanged");
    // Asking for the kind again returns to the picker's scope.
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "pods"}))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(t.row_count(cx), 1);
    let list = t.list(cx).unwrap();
    assert!(list.read_with(cx, |list, _| list.only().is_none()));
}

#[gpui_kit::test]
async fn a_tile_counting_zero_opens_an_empty_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    show_failing_pods(cx, &view, &[]);
    t.dispatch(cx, OPEN_LIST, json!({"category": "failing_pods"}))
        .unwrap();
    t.feed_list(cx, pods(&[("demo", "web-0", 0)])).await;
    assert_eq!(t.row_count(cx), 0);
}

#[gpui_kit::test]
async fn a_replaced_pod_of_the_same_name_is_not_listed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    show_failing_pods(cx, &view, &[("demo", "web-0", "uid-of-the-old-pod")]);
    t.dispatch(cx, OPEN_LIST, json!({"category": "failing_pods"}))
        .unwrap();
    t.feed_list(cx, pods(&[("demo", "web-0", 0)])).await;
    assert_eq!(t.row_count(cx), 0);
}

#[test]
fn the_dashboard_binds_its_actions() {
    assert_binds_scope(Scope::Dashboard, DashboardView::bind_actions);
}

#[gpui_kit::test]
async fn overview_opens_from_the_navigation_for_the_active_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    assert!(!t.available(cx).contains(&OPEN), "no cluster is active");
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    t.click(cx, "nav-overview");
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_some());
    assert!(t.list(cx).is_none());
    // Opening a kind replaces it.
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "pods"}))
        .unwrap();
    assert!(dashboard(cx, &t).is_none());
    assert!(t.list(cx).is_some());
}

#[gpui_kit::test]
async fn with_no_feed_every_tile_is_unavailable_and_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open(cx, &t).await;
    view.read_with(cx, |view, _| {
        assert!(view.model().tiles.iter().all(|tile| tile.count.is_none()));
    });
    let _ = t.dispatch(cx, OPEN_LIST, json!({"category": "pending_claims"}));
    assert!(dashboard(cx, &t).is_some(), "an unread tile opens no list");
    assert!(t.list(cx).is_none());
}

#[gpui_kit::test]
async fn a_problem_tile_opens_its_objects_and_the_kind_shows_all_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    show_failing_pods(cx, &view, &[("demo", "web-0", "uid-web-0")]);
    t.dispatch(cx, OPEN_LIST, json!({"category": "failing_pods"}))
        .unwrap();
    assert!(dashboard(cx, &t).is_none());
    let list = t.list(cx).expect("the pods list is open");
    t.feed_list(cx, pods(&[("demo", "web-0", 0), ("demo", "web-1", 0)]))
        .await;
    assert_eq!(t.row_count(cx), 1);
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "pods"}))
        .unwrap();
    assert!(list.read_with(cx, |list, _| list.only().is_none()));
    // The rows are computed on a core thread, which `run_until_parked` does not wait for.
    let widened = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| widened.read(cx).shown().1 != 1)
        .await;
    cx.run_until_parked();
    // The list filters off the UI thread, so the wider result lands a moment later.
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 2)
        .await;
    assert_eq!(t.row_count(cx), 2);
}

/// The overview showing one problem row that opens `web-0`, for `cluster` on `server`.
fn show_problem_row(
    cx: &mut TestAppContext,
    view: &Entity<DashboardView>,
    cluster: &str,
    server: Option<String>,
) {
    let row = ProblemRow {
        severity: ProblemSeverity::Critical,
        summary: "Container keeps crashing and restarting",
        object: "Pod demo/web-0 (app)".to_owned(),
        open: OpenObject {
            kind: "pods".to_owned(),
            namespace: Some("demo".to_owned()),
            name: "web-0".to_owned(),
            cluster: Some(cluster.to_owned()),
            server,
        },
    };
    view.update(cx, |view, cx| {
        let mut model = view.model().clone();
        model.problems = ProblemsPanel::Rows {
            rows: vec![row],
            total: 1,
            settling: 0,
            incomplete: false,
        };
        view.set_model(model, cx);
    });
    cx.run_until_parked();
}

#[gpui_kit::test]
async fn a_later_slot_does_not_label_this_feed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _view = open(cx, &t).await;
    let summary = t
        .workspace
        .read_with(cx, |ws, cx| ws.cluster_summary("kind-alpha", cx).cloned())
        .expect("connected");
    let followed = summary.generation;
    assert!(slot_to_read(Some(&summary), "kind-alpha", followed).is_some());

    let mut moved = summary;
    moved.generation = followed + 1;
    moved.info.server = Some("https://moved.example".to_owned());
    assert!(
        slot_to_read(Some(&moved), "kind-alpha", followed).is_none(),
        "the moved context's server must not label the followed slot's feed"
    );
}

#[gpui_kit::test]
async fn an_overview_link_of_a_gone_feed_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open(cx, &t).await;
    let server = t.workspace.read_with(cx, |ws, cx| {
        ws.cluster_summary("kind-alpha", cx)
            .and_then(|summary| summary.info.server.clone())
    });
    let old = FeedId::for_test("kind-alpha");
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(Some(old.clone()));
        let (model, lists) = (view.model().clone(), ProblemLists::new());
        view.finish_read_for_test(Some(old.clone()), model, lists, cx);
    });
    show_problem_row(cx, &view, "kind-alpha", server);
    let link = view.read_with(cx, |view, _| match &view.model().problems {
        ProblemsPanel::Rows { rows, .. } => rows[0].open.clone(),
        ProblemsPanel::Unknown => panic!("no rows"),
    });

    // Replaced: the row is still drawn, and its click must not reach the workspace.
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(Some(FeedId::for_test("kind-alpha")));
        assert!(view.refuses_link(|model| model.links_object(&link), cx));
    });
    t.click(cx, ("dashboard-problem", 0usize));
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_some(), "the stale row opened nothing");
    assert!(t.list(cx).is_none());
}

#[gpui_kit::test]
async fn a_link_from_the_frame_before_a_feed_stopped_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open(cx, &t).await;
    let old = FeedId::for_test("kind-alpha");
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(Some(old.clone()));
        let (model, lists) = (view.model().clone(), ProblemLists::new());
        view.finish_read_for_test(Some(old.clone()), model, lists, cx);
    });
    show_problem_row(cx, &view, "kind-alpha", Some("https://one".to_owned()));
    let link = view.read_with(cx, |view, _| match &view.model().problems {
        ProblemsPanel::Rows { rows, .. } => rows[0].open.clone(),
        ProblemsPanel::Unknown => panic!("no rows"),
    });
    // Cleared but not yet redrawn: the old frame's row is still under the pointer.
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(None);
        view.refresh(cx);
        assert!(view.refuses_link(|model| model.links_object(&link), cx));
    });
}

#[gpui_kit::test]
async fn a_problem_row_opens_its_exact_object(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = t.workspace.read_with(cx, |ws, cx| {
        ws.cluster_summary("kind-alpha", cx)
            .and_then(|summary| summary.info.server.clone())
    });
    show_problem_row(cx, &view, "kind-alpha", server);
    t.click(cx, ("dashboard-problem", 0usize));
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_none(), "the overview gave way");
    t.feed_list(cx, pods(&[("demo", "web-0", 0), ("demo", "web-1", 0)]))
        .await;
    assert_eq!(t.selected(cx).as_deref(), Some("web-0"));
}

#[gpui_kit::test]
async fn a_problem_row_from_another_server_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    show_problem_row(
        cx,
        &view,
        "kind-alpha",
        Some("https://replaced.invalid".to_owned()),
    );
    t.click(cx, ("dashboard-problem", 0usize));
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_some(), "the overview stays");
    assert!(t.list(cx).is_none());
}

fn show_warning_row(
    cx: &mut TestAppContext,
    view: &Entity<DashboardView>,
    server: Option<String>,
) -> OpenObject {
    let open = OpenObject {
        kind: "Pod.v1".to_owned(),
        namespace: Some("demo".to_owned()),
        name: "web-0".to_owned(),
        cluster: Some("kind-alpha".to_owned()),
        server,
    };
    let row = WarningRow {
        age: "1m".to_owned(),
        reason: "BackOff".to_owned(),
        object: "Pod demo/web-0".to_owned(),
        message: "Back-off restarting failed container".to_owned(),
        occurrences: 1,
        open: Some(open.clone()),
        object_uid: Some("uid-web-0".to_owned()),
        key: "event-web-0".to_owned(),
        rollout: None,
    };
    view.update(cx, |view, cx| {
        view.set_object_read_for_test(object_with_uid("web-0", "uid-web-0"));
        let mut model = view.model().clone();
        model.warnings = WarningsPanel::Rows {
            rows: vec![row],
            total: 1,
        };
        view.set_model(model, cx);
    });
    cx.run_until_parked();
    open
}

/// The API server's answer: `name` in `demo`, with `uid`.
fn object_with_uid(name: &str, uid: &str) -> ObjectRead {
    ObjectRead::Present(Arc::new(ObjectDetail {
        object: json!({"metadata": {"namespace": "demo", "name": name, "uid": uid}}),
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    }))
}

fn warning_on(name: &str, key: &str, server: Option<String>) -> WarningRow {
    WarningRow {
        age: "1m".to_owned(),
        reason: "BackOff".to_owned(),
        object: format!("Pod demo/{name}"),
        message: "Back-off restarting failed container".to_owned(),
        occurrences: 1,
        open: Some(OpenObject {
            kind: "Pod.v1".to_owned(),
            namespace: Some("demo".to_owned()),
            name: name.to_owned(),
            cluster: Some("kind-alpha".to_owned()),
            server,
        }),
        object_uid: Some(format!("uid-{name}")),
        key: key.to_owned(),
        rollout: None,
    }
}

fn show_warnings(cx: &mut TestAppContext, view: &Entity<DashboardView>, rows: Vec<WarningRow>) {
    view.update(cx, |view, cx| {
        let mut model = view.model().clone();
        let total = rows.len();
        model.warnings = WarningsPanel::Rows { rows, total };
        view.set_model(model, cx);
    });
    cx.run_until_parked();
}

fn action_error(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    t.workspace
        .read_with(cx, |ws, _| ws.action_error().map(str::to_owned))
}

fn incomplete_without_widgets() -> ClusterDiscovery {
    let sections = standard_sections()
        .into_iter()
        .filter(|section| section.title != "example.com")
        .collect();
    let mut partial = discovery(sections);
    partial.incomplete_groups = vec!["example.com".to_owned()];
    partial
}

fn feed_discovery(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    found: Option<ClusterDiscovery>,
    error: Option<&str>,
) {
    t.feed(
        cx,
        Event::DiscoveryChanged {
            cluster: "kind-alpha".into(),
            generation: CURRENT_GENERATION,
            discovery: found.map(Arc::new),
            error: error.map(str::to_owned),
        },
    );
}

/// `kind-alpha` connected, with no discovery reported yet.
async fn connect_alpha_undiscovered(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.activate(cx, "kind-alpha").await;
    t.wait_for_state(cx, "kind-alpha", |s| {
        matches!(s, ConnectionState::Connecting)
    })
    .await;
    t.feed(
        cx,
        Event::ConnectionChanged {
            cluster: "kind-alpha".into(),
            state: ConnectionState::Connected(ServerVersion {
                git_version: "v1.33.1".into(),
                platform: "linux/arm64".into(),
            }),
        },
    );
}

fn alpha_server(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    t.workspace.read_with(cx, |ws, cx| {
        ws.cluster_summary("kind-alpha", cx)
            .and_then(|summary| summary.info.server.clone())
    })
}

#[gpui_kit::test]
async fn a_connected_cluster_opens_on_its_overview(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_some());
    assert!(t.list(cx).is_none());
    let nav = t.workspace.read_with(cx, |ws, _| ws.nav_view());
    assert!(nav.read_with(cx, |nav, _| nav.overview_open()));
}

#[gpui_kit::test]
async fn a_paused_cluster_does_not_open_an_overview(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.model.update(cx, |model, cx| {
        let alpha = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        alpha.paused = true;
        cx.notify();
    });
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_none(), "no overview, so no feed");
}

#[gpui_kit::test]
async fn a_warning_row_opens_its_exact_object(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = alpha_server(cx, &t);
    show_warning_row(cx, &view, server);
    t.click(cx, WARNING_WEB_0);
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_none(), "the overview gave way");
    t.feed_list(cx, pods(&[("demo", "web-0", 0), ("demo", "web-1", 0)]))
        .await;
    assert_eq!(t.selected(cx).as_deref(), Some("web-0"));
}

// The waiting overview has no other link, so the first Down lands on the row.
#[gpui_kit::test]
async fn a_warning_row_opens_from_the_keyboard(cx: &mut TestAppContext) {
    use gpui_kit::AppContext as _;
    use gpui_kit::test::TestWindowExt as _;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = alpha_server(cx, &t);
    show_warning_row(cx, &view, server);
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    t.press(cx, "down");
    t.press(cx, "enter");
    assert!(dashboard(cx, &t).is_none(), "the overview gave way");
    t.feed_list(cx, pods(&[("demo", "web-0", 0), ("demo", "web-1", 0)]))
        .await;
    assert_eq!(t.selected(cx).as_deref(), Some("web-0"));
}

#[gpui_kit::test]
async fn a_warning_row_from_another_server_opens_nothing_and_says_so(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    show_warning_row(cx, &view, Some("https://replaced.invalid".to_owned()));
    t.click(cx, WARNING_WEB_0);
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_some(), "the overview stays");
    assert!(t.list(cx).is_none());
    let error = t
        .workspace
        .read_with(cx, |ws, _| ws.action_error().map(str::to_owned));
    assert_eq!(
        error.as_deref(),
        Some("Cannot open Pod demo/web-0 on this cluster.")
    );
}

#[gpui_kit::test]
async fn a_warning_row_of_a_gone_feed_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = alpha_server(cx, &t);
    let link = show_warning_row(cx, &view, server);
    view.update(cx, |view, cx| {
        view.set_running_feed_for_test(Some(FeedId::for_test("kind-alpha")));
        assert!(view.refuses_link(|model| model.links_object(&link), cx));
    });
    t.click(cx, WARNING_WEB_0);
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_some(), "the stale row opened nothing");
    assert!(t.list(cx).is_none());
}

#[gpui_kit::test]
async fn a_warning_for_a_replaced_object_stays_on_the_overview(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = alpha_server(cx, &t);
    show_warning_row(cx, &view, server);
    view.update(cx, |view, _| {
        view.set_object_read_for_test(object_with_uid("web-0", "uid-of-the-replacement"));
    });
    t.click(cx, WARNING_WEB_0);
    assert!(dashboard(cx, &t).is_some(), "the overview stays");
    assert!(t.list(cx).is_none());
    assert_eq!(
        action_error(cx, &t).as_deref(),
        Some("Pod demo/web-0 was replaced by a new object with the same name.")
    );
}

#[gpui_kit::test]
async fn a_warning_for_a_deleted_object_stays_on_the_overview(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = alpha_server(cx, &t);
    show_warning_row(cx, &view, server);
    view.update(cx, |view, _| {
        view.set_object_read_for_test(ObjectRead::Missing);
    });
    t.click(cx, WARNING_WEB_0);
    assert!(dashboard(cx, &t).is_some(), "the overview stays");
    assert!(t.list(cx).is_none());
    assert_eq!(
        action_error(cx, &t).as_deref(),
        Some("Pod demo/web-0 no longer exists.")
    );
}

#[gpui_kit::test]
async fn a_warning_whose_object_cannot_be_checked_stays_on_the_overview(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = alpha_server(cx, &t);
    show_warning_row(cx, &view, server);
    view.update(cx, |view, _| {
        view.set_object_read_for_test(ObjectRead::Unavailable("timed out".to_owned()));
    });
    t.click(cx, WARNING_WEB_0);
    assert!(dashboard(cx, &t).is_some(), "the overview stays");
    assert!(t.list(cx).is_none());
    assert_eq!(
        action_error(cx, &t).as_deref(),
        Some("Could not check Pod demo/web-0, so it was not opened.")
    );
}

// The click lands on the last frame, which still draws a row the model has dropped.
#[gpui_kit::test]
async fn a_warning_gone_from_the_model_opens_nothing_from_the_last_frame(cx: &mut TestAppContext) {
    use gpui_kit::{
        AppContext as _, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
        PlatformInput,
    };

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = alpha_server(cx, &t);
    show_warning_row(cx, &view, server);
    let position = t
        .find(cx, WARNING_WEB_0)
        .expect("the row is drawn")
        .bounds()
        .center();
    let held = view.clone();
    // Raw events: `window.click` draws the changed model first, which is the frame not yet here.
    cx.update_window(t.window, move |_, window, cx| {
        held.update(cx, |view, cx| {
            let mut model = view.model().clone();
            model.warnings = WarningsPanel::Rows {
                rows: Vec::new(),
                total: 0,
            };
            view.set_model(model, cx);
        });
        let (button, modifiers) = (MouseButton::Left, Modifiers::default());
        let events = [
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers,
            }),
            PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
            }),
            PlatformInput::MouseUp(MouseUpEvent {
                button,
                position,
                modifiers,
                click_count: 1,
            }),
        ];
        for event in events {
            window.dispatch_event(event, cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        dashboard(cx, &t).is_some(),
        "the removed row opened nothing"
    );
    assert!(t.list(cx).is_none());
}

// A newer event lands above the focused row: Enter still opens the row that had focus.
#[gpui_kit::test]
async fn keyboard_focus_stays_on_its_warning_when_a_newer_one_arrives(cx: &mut TestAppContext) {
    use gpui_kit::AppContext as _;
    use gpui_kit::test::TestWindowExt as _;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    let server = alpha_server(cx, &t);
    let older = warning_on("web-0", "event-web-0", server.clone());
    let newer = warning_on("web-1", "event-web-1", server);
    show_warnings(cx, &view, vec![older.clone()]);
    view.update(cx, |view, _| {
        view.set_object_read_for_test(object_with_uid("web-0", "uid-web-0"));
    });
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    t.press(cx, "down");
    show_warnings(cx, &view, vec![newer, older]);
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    t.press(cx, "enter");
    assert!(dashboard(cx, &t).is_none(), "the overview gave way");
    t.feed_list(cx, pods(&[("demo", "web-0", 0), ("demo", "web-1", 0)]))
        .await;
    assert_eq!(t.selected(cx).as_deref(), Some("web-0"));
}

#[gpui_kit::test]
async fn a_saved_kind_missing_from_partial_discovery_is_restored_on_retry(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.workspace.update(cx, |ws, _| {
        ws.save_last_view_for_test("kind-alpha", "widgets.example.com");
    });
    connect_alpha_undiscovered(cx, &t).await;
    feed_discovery(cx, &t, Some(incomplete_without_widgets()), None);
    assert!(dashboard(cx, &t).is_none(), "the saved kind may still come");
    feed_discovery(cx, &t, Some(discovery(standard_sections())), None);
    assert!(dashboard(cx, &t).is_none());
    let list = t.list(cx).expect("the saved kind is restored");
    let kind = list.read_with(cx, |list, _| list.kind().as_str().to_owned());
    assert_eq!(kind, "widgets.example.com");
}

#[gpui_kit::test]
async fn a_saved_kind_the_cluster_no_longer_serves_opens_the_overview(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.workspace.update(cx, |ws, _| {
        ws.save_last_view_for_test("kind-alpha", "gadgets.example.com");
    });
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    assert!(
        dashboard(cx, &t).is_some(),
        "complete discovery rules it out"
    );
}

#[gpui_kit::test]
async fn failing_discovery_opens_the_overview_when_nothing_is_pending(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    connect_alpha_undiscovered(cx, &t).await;
    feed_discovery(cx, &t, None, Some("discovery failed"));
    assert!(dashboard(cx, &t).is_some(), "no kinds to pick from");
}

#[gpui_kit::test]
async fn failing_discovery_keeps_a_saved_kind_pending(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.workspace.update(cx, |ws, _| {
        ws.save_last_view_for_test("kind-alpha", "widgets.example.com");
    });
    connect_alpha_undiscovered(cx, &t).await;
    feed_discovery(cx, &t, None, Some("discovery failed"));
    assert!(
        dashboard(cx, &t).is_none(),
        "the saved kind is still wanted"
    );
    feed_discovery(cx, &t, Some(discovery(standard_sections())), None);
    assert!(t.list(cx).is_some(), "restored once discovery works");
}

#[gpui_kit::test]
async fn connecting_from_the_sidebar_keeps_its_focus(cx: &mut TestAppContext) {
    use gpui_kit::AppContext as _;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let sidebar = t
        .workspace
        .read_with(cx, |ws, _| ws.sidebar_focus_for_test());
    let focus = sidebar.clone();
    cx.update_window(t.window, move |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    assert!(dashboard(cx, &t).is_some());
    cx.update_window(t.window, |_, window, _| {
        assert!(sidebar.is_focused(window), "Down still moves the sidebar");
    })
    .unwrap();
}

#[gpui_kit::test]
async fn opening_the_overview_focuses_the_panel_not_a_link(cx: &mut TestAppContext) {
    use gpui_kit::AppContext as _;
    use gpui_kit::Focusable as _;
    use gpui_kit::test::TestWindowExt as _;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open_held(cx, &t).await;
    // A warning row makes the overview hold a link, so the check below tells the two apart.
    let server = alpha_server(cx, &t);
    show_warning_row(cx, &view, server);
    cx.run_until_parked();
    let overview = view.read_with(cx, |view, cx| view.focus_handle(cx));
    cx.update_window(t.window, move |_, window, cx| {
        window.render_frame(cx);
        assert!(overview.is_focused(window), "the overview itself has focus");
        let on_link = window.focused(cx).is_some_and(|h| h != overview);
        assert!(!on_link, "no link has focus");
    })
    .unwrap();
}

#[gpui_kit::test]
async fn tab_reaches_the_overview_and_a_lost_focus_returns_to_it(cx: &mut TestAppContext) {
    use gpui_kit::AppContext as _;
    use gpui_kit::Focusable as _;
    use gpui_kit::test::TestWindowExt as _;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open(cx, &t).await;
    let overview = view.read_with(cx, |view, cx| view.focus_handle(cx));
    let sidebar = t
        .workspace
        .read_with(cx, |ws, _| ws.sidebar_focus_for_test());
    let focus = sidebar.clone();
    cx.update_window(t.window, move |_, window, cx| {
        window.render_frame(cx);
        window.focus(&focus, cx);
    })
    .unwrap();
    // Sidebar, navigation, then the overview.
    t.press(cx, "tab");
    t.press(cx, "tab");
    let held = overview.clone();
    cx.update_window(t.window, move |_, window, _| {
        assert!(held.is_focused(window), "Tab reaches the overview");
    })
    .unwrap();
    cx.update_window(t.window, |_, window, cx| {
        window.blur(cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(t.window, move |_, window, _| {
        assert!(overview.is_focused(window), "recovery returns to it");
    })
    .unwrap();
}

#[gpui_kit::test]
async fn a_cluster_switch_closes_the_overview_and_its_values(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open(cx, &t).await;
    show_failing_pods(cx, &view, &[("demo", "web-0", "uid-web-0")]);
    t.activate(cx, "kind-beta").await;
    cx.run_until_parked();
    assert!(
        dashboard(cx, &t).is_none(),
        "the old cluster's overview closed"
    );
    let nav = t.workspace.read_with(cx, |ws, _| ws.nav_view());
    assert!(!nav.read_with(cx, |nav, _| nav.overview_open()));
}
