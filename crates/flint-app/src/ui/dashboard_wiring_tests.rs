//! The cluster overview in the main window: opening it, its tiles, and a cluster switch.

use std::collections::HashMap;
use std::sync::Arc;

use flint_core::dashboard::{ProblemCategory, ProblemCounts, ProblemSnapshot, ProblemSummariser};
use flint_core::problems::{
    Problem, ProblemId, ProblemKind, ProblemSubject, WORKLOAD_HOLD_DOWN, WorkloadKind,
};
use gpui_kit::{Entity, TestAppContext};
use jiff::Timestamp;
use serde_json::{Value, json};

use super::dashboard::{
    DashboardModel, DashboardView, FeedId, ProblemList, ProblemLists, ProblemTile, lists_of,
};
use super::list::{CountedObjects, ObjectFilter};
use super::registry::{Scope, tests::assert_binds_scope};
use super::support_tests::{TestWorkspace, open_workspace, pods, standard_sections};
use flint_core::thresholds::ThresholdLevel;

const OPEN: &str = "workspace::OpenClusterOverview";
const OPEN_LIST: &str = "dashboard::OpenProblemList";
const OPEN_SHORT: &str = "dashboard::OpenShortWorkloads";

fn dashboard(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<Entity<DashboardView>> {
    t.workspace.read_with(cx, |ws, _| ws.dashboard_view())
}

async fn open(cx: &mut TestAppContext, t: &TestWorkspace) -> Entity<DashboardView> {
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    t.dispatch(cx, OPEN, Value::Null).unwrap();
    dashboard(cx, t).expect("the overview is open")
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
async fn each_counted_kind_opens_its_own_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let view = open(cx, &t).await;
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
    let view = open(cx, &t).await;
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
    let view = open(cx, &t).await;
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
    let view = open(cx, &t).await;
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
    let view = open(cx, &t).await;
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
    cx.run_until_parked();
    assert!(list.read_with(cx, |list, _| list.only().is_none()));
    assert_eq!(t.row_count(cx), 2);
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
