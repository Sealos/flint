//! The cluster overview in the main window: opening it, its tiles, and a cluster switch.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use flint_core::dashboard::ProblemCategory;
use gpui_kit::{Entity, TestAppContext};
use serde_json::{Value, json};

use super::dashboard::{DashboardModel, DashboardView, ProblemList, ProblemTile};
use super::list::ObjectFilter;
use super::registry::{Scope, tests::assert_binds_scope};
use super::support_tests::{TestWorkspace, open_workspace, pods, standard_sections};
use flint_core::thresholds::ThresholdLevel;

const OPEN: &str = "workspace::OpenClusterOverview";
const OPEN_LIST: &str = "dashboard::OpenProblemList";

fn dashboard(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<Entity<DashboardView>> {
    t.workspace.read_with(cx, |ws, _| ws.dashboard_view())
}

async fn open(cx: &mut TestAppContext, t: &TestWorkspace) -> Entity<DashboardView> {
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    t.dispatch(cx, OPEN, Value::Null).unwrap();
    dashboard(cx, t).expect("the overview is open")
}

/// Shows `count` failing pods, as a read of the feed would.
fn show_failing_pods(cx: &mut TestAppContext, view: &Entity<DashboardView>, keys: &[&str]) {
    view.update(cx, |view, cx| {
        let mut model: DashboardModel = view.model().clone();
        model.tiles[0] = ProblemTile {
            category: ProblemCategory::FailingPods,
            label: "Failing pods",
            count: Some(keys.len() as u32),
            level: ThresholdLevel::Warning,
        };
        let list = ProblemList {
            kind: "pods",
            filter: ObjectFilter {
                label: "failing pods".to_owned(),
                keys: Arc::new(keys.iter().map(|k| (*k).to_owned()).collect::<HashSet<_>>()),
            },
        };
        view.set_data(
            model,
            HashMap::from([(ProblemCategory::FailingPods, list)]),
            cx,
        );
    });
    cx.run_until_parked();
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
    show_failing_pods(cx, &view, &["demo/web-0"]);
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
    show_failing_pods(cx, &view, &["demo/web-0"]);
    t.activate(cx, "kind-beta").await;
    cx.run_until_parked();
    assert!(
        dashboard(cx, &t).is_none(),
        "the old cluster's overview closed"
    );
    let nav = t.workspace.read_with(cx, |ws, _| ws.nav_view());
    assert!(!nav.read_with(cx, |nav, _| nav.overview_open()));
}
