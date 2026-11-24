//! View tests for the combined list, in a headless GPUI window, from fixture snapshots.

use std::sync::Arc;

use flint_core::combined::{
    CLUSTER_COLUMN_KEY, ColumnKey, CombinedMember, CombinedObjects, CombinedQuery, CombinedRowId,
};
use flint_core::settings::ClusterTag;
use flint_core::{Cell, Column, ColumnRole, ColumnType, KindId, KindSnapshot, Row, SyncState};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

use super::super::super::list_model::combined::{CombinedSource, EmptyState};
use super::super::super::view_state::ListState;
use super::{CombinedListEvent, CombinedResourceList, ShowAllCombinedRows, compute_view, row_id};

fn column(name: &str, role: ColumnRole) -> Column {
    Column {
        name: name.into(),
        ty: ColumnType::String,
        format: "".into(),
        description: "".into(),
        priority: 0,
        role,
    }
}

/// A pods snapshot of `cluster` with one Value column, `value_column`. Rows are (name, value)
/// with a uid shared across clusters, so only the cluster tells two of one name apart.
fn snapshot(
    cluster: &str,
    value_column: &str,
    rows: &[(&str, &str)],
    state: SyncState,
) -> Arc<KindSnapshot> {
    let columns: Arc<[Column]> = vec![
        column("Name", ColumnRole::Name),
        column("Namespace", ColumnRole::Namespace),
        column(value_column, ColumnRole::Value),
    ]
    .into();
    let mut rows: Vec<Arc<Row>> = rows
        .iter()
        .map(|(name, value)| {
            Arc::new(Row {
                uid: format!("uid-{name}").into(),
                name: (*name).into(),
                namespace: Some("default".into()),
                resource_version: "1".into(),
                generation: None,
                created: Some(0),
                labels: Box::new([]),
                cells: Box::new([Cell::Name, Cell::Namespace, Cell::Text((*value).into())]),
            })
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Arc::new(KindSnapshot {
        cluster: cluster.into(),
        kind: KindId::pods(),
        namespaced: true,
        columns,
        rows,
        state,
        version: 1,
        ..Default::default()
    })
}

fn member(snapshot: Arc<KindSnapshot>, name: &str, tag: Option<ClusterTag>) -> CombinedMember {
    CombinedMember {
        snapshot,
        display_name: name.to_owned(),
        tag,
    }
}

/// `prod` has Status and `staging` has Node; both hold a pod named `api`.
fn fixture() -> CombinedSource {
    CombinedSource::new(vec![
        member(
            snapshot(
                "ctx-prod",
                "Status",
                &[("api", "Running"), ("web", "Pending")],
                SyncState::Synced,
            ),
            "prod",
            Some(ClusterTag::Red),
        ),
        member(
            snapshot(
                "ctx-staging",
                "Node",
                &[("api", "node-1"), ("db", "node-2")],
                SyncState::Reconnecting { error: None },
            ),
            "staging",
            None,
        ),
    ])
    .unwrap()
}

fn open(
    cx: &mut TestAppContext,
    source: CombinedSource,
) -> (AnyWindowHandle, Entity<CombinedResourceList>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::ui::registry::init(cx);
    });
    let opened = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1000.), px(600.)),
        };
        let (window, list) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    CombinedResourceList::new("Pods".into(), source, Vec::new(), window, cx)
                })
            },
        )
        .unwrap();
        window
            .update(cx, |_, window, cx| {
                let focus = list.read(cx).focus_handle().clone();
                window.focus(&focus, cx);
            })
            .unwrap();
        (window, list)
    });
    cx.run_until_parked();
    opened
}

fn state(cx: &mut TestAppContext, list: &Entity<CombinedResourceList>) -> ListState {
    cx.run_until_parked();
    list.read_with(cx, |list, _| list.view_state())
}

/// The rows as (cluster cell, name), in display order.
fn order(state: &ListState) -> Vec<(String, String)> {
    state
        .rows
        .iter()
        .map(|r| (r.cells_by_column["Cluster"].clone(), r.name.clone()))
        .collect()
}

fn pair(cluster: &str, name: &str) -> (String, String) {
    (cluster.to_owned(), name.to_owned())
}

fn key(key: &str) -> ColumnKey {
    ColumnKey {
        key: key.to_owned(),
        occurrence: 0,
    }
}

#[gpui_kit::test]
async fn problems_only_closes_a_selected_row_it_hides(cx: &mut TestAppContext) {
    use super::super::problems_tests::checked_with_pod;

    let (window, list) = open(cx, fixture());
    click(cx, window, "row-ctx-prod/uid-api");
    assert!(list.read_with(cx, |list, _| list.selected.is_some()));
    list.update(cx, |list, cx| {
        list.problems.toggle();
        list.set_problems(
            std::collections::HashMap::from([(
                "ctx-prod".to_owned(),
                checked_with_pod("web", "uid-web", &["uid-api"]),
            )]),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(list.read_with(cx, |list, _| list.selected.clone()), None);
}

#[gpui_kit::test]
async fn problems_only_reconciles_the_selection_before_new_rows_land(cx: &mut TestAppContext) {
    use super::super::problems_tests::checked_with_pod;

    let (window, list) = open(cx, fixture());
    click(cx, window, "row-ctx-prod/uid-api");
    list.update(cx, |list, cx| {
        list.problems.toggle();
        list.set_problems(
            std::collections::HashMap::from([(
                "ctx-prod".to_owned(),
                checked_with_pod("web", "uid-web", &["uid-api"]),
            )]),
            cx,
        );
        assert_eq!(list.selected, None, "cleared before the new view lands");
    });
    // The hidden row is still drawn until the view lands, and cannot be chosen again.
    window
        .update(cx, |_, window, cx| {
            list.update(cx, |list, cx| {
                let id = CombinedRowId {
                    cluster: "ctx-prod".into(),
                    uid: "uid-api".into(),
                };
                let line =
                    super::model::line_of(&list.shown.list, &list.view, &id).expect("still drawn");
                list.select_line(line, window, cx);
                assert_eq!(list.selected, None);
            });
        })
        .unwrap();
}

#[gpui_kit::test]
async fn the_problems_toggle_does_nothing_for_a_kind_it_cannot_judge(cx: &mut TestAppContext) {
    let configmaps = |cluster: &str| {
        let mut shown = (*snapshot(cluster, "Data", &[("a", "1")], SyncState::Synced)).clone();
        shown.kind = KindId::new("", "configmaps");
        Arc::new(shown)
    };
    let source = CombinedSource::new(vec![
        member(configmaps("ctx-prod"), "prod", None),
        member(configmaps("ctx-staging"), "staging", None),
    ])
    .unwrap();
    let (window, list) = open(cx, source);
    assert!(!list.read_with(cx, |list, _| list.has_problem_filter()));
    window
        .update(cx, |_, window, cx| {
            list.update(cx, |list, cx| {
                list.on_problems(&super::ToggleCombinedProblems, window, cx);
            });
        })
        .unwrap();
    assert!(!list.read_with(cx, |list, _| list.problems.is_on()));
}

#[gpui_kit::test]
async fn problems_only_hides_checked_rows_and_keeps_an_unchecked_member(cx: &mut TestAppContext) {
    use super::super::problems::ClusterProblems;
    use super::super::problems_tests::checked_with_pod;

    let (_window, list) = open(cx, fixture());
    list.update(cx, |list, cx| {
        list.problems.toggle();
        list.set_problems(
            std::collections::HashMap::from([
                (
                    "ctx-prod".to_owned(),
                    checked_with_pod("web", "uid-web", &["uid-api"]),
                ),
                ("ctx-staging".to_owned(), ClusterProblems::unchecked()),
            ]),
            cx,
        );
    });
    let shown = state(cx, &list);
    assert_eq!(
        order(&shown),
        [
            pair("staging", "api"),
            pair("staging", "db"),
            pair("prod", "web"),
        ]
    );
    let note = shown
        .banners
        .iter()
        .find(|banner| banner.id == super::super::problems::UNCHECKED_BANNER)
        .expect("the unchecked member is named");
    assert!(note.text.contains("staging") && !note.text.contains("prod"));
}

#[gpui_kit::test]
async fn columns_are_the_union_with_blanks(cx: &mut TestAppContext) {
    let (window, list) = open(cx, fixture());
    let shown = state(cx, &list);
    assert_eq!(
        shown.columns,
        ["Cluster", "Name", "Namespace", "Status", "Node"]
    );
    let cell = |cluster: &str, name: &str, column: &str| {
        shown
            .rows
            .iter()
            .find(|r| r.id == row_id(cluster, &format!("uid-{name}")))
            .unwrap()
            .cells_by_column[column]
            .clone()
    };
    assert_eq!(cell("ctx-prod", "api", "Status"), "Running");
    assert_eq!(cell("ctx-prod", "api", "Node"), "");
    assert_eq!(cell("ctx-staging", "api", "Node"), "node-1");
    assert_eq!(cell("ctx-staging", "api", "Status"), "");

    // Drawn too: the tagged cluster's dot, and each row under its own id.
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("row-ctx-prod/uid-api").is_some());
        assert!(window.try_find("row-ctx-staging/uid-api").is_some());
        let prod = window.within("row-ctx-prod/uid-api");
        assert!(prod.try_find("cluster-tag-ctx-prod").is_some());
        let note = window.within("member-ctx-prod");
        assert!(note.try_find("cluster-tag-ctx-prod").is_some());
        assert!(window.try_find("cluster-tag-ctx-staging").is_none());
        assert!(window.try_find("member-ctx-staging").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
async fn sorts_by_cluster_and_keeps_blanks_last(cx: &mut TestAppContext) {
    let (_window, list) = open(cx, fixture());
    let cluster = key(CLUSTER_COLUMN_KEY);
    list.update(cx, |list, cx| list.toggle_sort(&cluster, cx));
    assert_eq!(
        order(&state(cx, &list)),
        [
            pair("prod", "api"),
            pair("prod", "web"),
            pair("staging", "api"),
            pair("staging", "db"),
        ]
    );
    list.update(cx, |list, cx| list.toggle_sort(&cluster, cx));
    let shown = state(cx, &list);
    assert_eq!(shown.rows[0].cells_by_column["Cluster"], "staging");
    assert!(shown.sort.as_ref().is_some_and(|s| s.descending));

    // Descending on Status: staging lacks it, so its rows stay last.
    let status = key("server:Status");
    list.update(cx, |list, cx| {
        list.toggle_sort(&status, cx);
        list.toggle_sort(&status, cx);
    });
    let names = order(&state(cx, &list));
    assert_eq!(names[0], pair("prod", "api"));
    assert_eq!(names[1], pair("prod", "web"));
    assert!(names[2..].iter().all(|(c, _)| c == "staging"));
}

#[gpui_kit::test]
async fn filters_on_cluster(cx: &mut TestAppContext) {
    let (_window, list) = open(cx, fixture());
    list.update(cx, |list, cx| list.toggle_cluster_filter("ctx-staging", cx));
    let shown = state(cx, &list);
    assert_eq!(
        order(&shown),
        [pair("staging", "api"), pair("staging", "db")]
    );
    assert_eq!(shown.count, "2 of 4");
    list.update(cx, |list, cx| list.toggle_cluster_filter("ctx-staging", cx));
    assert_eq!(state(cx, &list).shown, 4);
}

#[gpui_kit::test]
async fn groups_by_cluster_and_collapses(cx: &mut TestAppContext) {
    let (window, list) = open(cx, fixture());
    list.update(cx, |list, cx| list.set_group_by_cluster(true, cx));
    assert_eq!(
        order(&state(cx, &list)),
        [
            pair("prod", "api"),
            pair("prod", "web"),
            pair("staging", "api"),
            pair("staging", "db"),
        ]
    );
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("group-ctx-prod").is_some());
        assert!(window.try_find("group-ctx-staging").is_some());
    })
    .unwrap();

    list.update(cx, |list, cx| list.toggle_group("ctx-prod", cx));
    let shown = state(cx, &list);
    assert_eq!(
        order(&shown),
        [pair("staging", "api"), pair("staging", "db")]
    );
    // A collapsed group's rows still count as matching.
    assert_eq!(shown.shown, 4);
}

#[gpui_kit::test]
async fn a_row_resolves_to_its_own_cluster(cx: &mut TestAppContext) {
    let (window, list) = open(cx, fixture());
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = events.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&list, move |_, event: &CombinedListEvent, _| {
            if let CombinedListEvent::Selected(id) = event {
                seen.lock().unwrap().push(id.clone());
            }
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("row-ctx-staging/uid-api", cx);
    })
    .unwrap();
    cx.run_until_parked();

    let expected = CombinedRowId {
        cluster: "ctx-staging".into(),
        uid: "uid-api".into(),
    };
    assert_eq!(
        list.read_with(cx, |list, _| list.selected().cloned()),
        Some(expected.clone())
    );
    assert_eq!(*events.lock().unwrap(), [Some(expected)]);
    let shown = state(cx, &list);
    let selected: Vec<_> = shown.rows.iter().filter(|r| r.selected).collect();
    assert_eq!(
        selected.len(),
        1,
        "the other cluster's api stays unselected"
    );
    assert_eq!(selected[0].id, "row-ctx-staging/uid-api");
}

#[gpui_kit::test]
async fn a_selection_ends_only_when_its_own_cluster_drops_the_row(cx: &mut TestAppContext) {
    let (window, list) = open(cx, fixture());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("row-ctx-prod/uid-api", cx);
    })
    .unwrap();
    cx.run_until_parked();

    // Staging loses its `api`: prod's row of that name stays selected.
    let without_staging_api = CombinedSource::new(vec![
        member(
            snapshot(
                "ctx-prod",
                "Status",
                &[("api", "Running")],
                SyncState::Synced,
            ),
            "prod",
            None,
        ),
        member(
            snapshot("ctx-staging", "Node", &[("db", "n")], SyncState::Synced),
            "staging",
            None,
        ),
    ])
    .unwrap();
    list.update(cx, |list, cx| list.set_source(without_staging_api, cx));
    cx.run_until_parked();
    let selected = list.read_with(cx, |list, _| list.selected().cloned());
    assert_eq!(
        selected.map(|id| id.cluster.to_string()),
        Some("ctx-prod".into())
    );

    // A reconnecting cluster's missing row proves nothing.
    let reconnecting = CombinedSource::new(vec![member(
        snapshot(
            "ctx-prod",
            "Status",
            &[],
            SyncState::Reconnecting { error: None },
        ),
        "prod",
        None,
    )])
    .unwrap();
    list.update(cx, |list, cx| list.set_source(reconnecting, cx));
    cx.run_until_parked();
    assert!(list.read_with(cx, |list, _| list.selected().is_some()));

    // Current and without it: gone.
    let current = CombinedSource::new(vec![member(
        snapshot("ctx-prod", "Status", &[], SyncState::Synced),
        "prod",
        None,
    )])
    .unwrap();
    list.update(cx, |list, cx| list.set_source(current, cx));
    cx.run_until_parked();
    assert!(list.read_with(cx, |list, _| list.selected().is_none()));
}

#[gpui_kit::test]
async fn a_namespace_filter_with_no_rows_says_so(cx: &mut TestAppContext) {
    let (_window, list) = open(cx, fixture());
    list.update(cx, |list, cx| list.set_namespaces(vec!["other".into()], cx));
    let shown = state(cx, &list);
    assert_eq!(shown.shown, 0);
    assert_eq!(shown.placeholder, Some("no_match"));
}

#[gpui_kit::test]
async fn an_unknown_cluster_is_not_called_empty(cx: &mut TestAppContext) {
    let failed = || SyncState::Reconnecting { error: None };
    let source = |prod: &[(&str, &str)], other: SyncState| {
        CombinedSource::new(vec![
            member(
                snapshot("ctx-prod", "Status", prod, SyncState::Synced),
                "prod",
                None,
            ),
            member(snapshot("ctx-staging", "Node", &[], other), "staging", None),
        ])
        .unwrap()
    };
    let (_window, list) = open(cx, source(&[], failed()));
    assert_eq!(state(cx, &list).placeholder, Some("failed"));

    list.update(cx, |list, cx| {
        list.set_source(source(&[], SyncState::Synced), cx)
    });
    assert_eq!(state(cx, &list).placeholder, Some("empty"));

    // Filtered to a loading cluster while another holds rows: still loading, not "no match".
    list.update(cx, |list, cx| {
        list.set_source(source(&[("api", "Running")], SyncState::Loading), cx);
        list.toggle_cluster_filter("ctx-staging", cx);
    });
    assert_eq!(state(cx, &list).placeholder, Some("loading"));
}

#[gpui_kit::test]
async fn a_server_column_named_cluster_keeps_the_row_identity(cx: &mut TestAppContext) {
    let source = CombinedSource::new(vec![member(
        snapshot(
            "ctx-prod",
            "Cluster",
            &[("api", "workload-a")],
            SyncState::Synced,
        ),
        "prod",
        None,
    )])
    .unwrap();
    let (_window, list) = open(cx, source);
    let shown = state(cx, &list);
    assert_eq!(shown.rows[0].cells_by_column["Cluster"], "prod");
    assert!(shown.rows[0].cells.contains(&"workload-a".to_owned()));
}

/// `snapshot`, listed namespace by namespace: `default` at `revision`, and `other` denied.
fn limited(snapshot: Arc<KindSnapshot>, listed: &[&str], revision: u64) -> Arc<KindSnapshot> {
    let mut limited = (*snapshot).clone();
    limited.revision = revision;
    limited.limited_to = Some(Arc::new(flint_core::NamespaceLimit {
        listed: listed.iter().map(|n| (*n).to_owned()).collect(),
        denied: vec!["other".into()],
        reason: "pods is forbidden".into(),
        revisions: vec![revision; listed.len()],
    }));
    Arc::new(limited)
}

fn one(snapshot: Arc<KindSnapshot>) -> CombinedSource {
    CombinedSource::new(vec![member(snapshot, "prod", None)]).unwrap()
}

fn prod(rows: &[(&str, &str)]) -> Arc<KindSnapshot> {
    snapshot("ctx-prod", "Status", rows, SyncState::Synced)
}

fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

#[gpui_kit::test]
async fn a_limited_member_says_which_namespaces_it_covers(cx: &mut TestAppContext) {
    let (window, list) = open(
        cx,
        one(limited(prod(&[("api", "Running")]), &["default"], 3)),
    );
    let shown = state(cx, &list);
    let banner = shown
        .banners
        .iter()
        .find(|b| b.id == "list-limited-ctx-prod")
        .expect("the limited member's banner");
    assert!(banner.text.contains("default"), "{}", banner.text);
    assert!(banner.text.contains("other"), "{}", banner.text);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("member-limited-ctx-prod").is_some());
    })
    .unwrap();

    // No rows in what it covers is not "no rows".
    list.update(cx, |list, cx| {
        list.set_source(one(limited(prod(&[]), &["default"], 4)), cx)
    });
    cx.run_until_parked();
    let empty = list.read_with(cx, |list, _| list.empty_state());
    assert_eq!(
        empty,
        Some(super::super::super::list_model::combined::EmptyState::NoRowsListed)
    );
}

#[gpui_kit::test]
async fn a_selection_survives_until_its_namespace_is_listed_anew(cx: &mut TestAppContext) {
    let rows = [("api", "Running")];
    let (window, list) = open(cx, one(limited(prod(&rows), &["default"], 5)));
    click(cx, window, "row-ctx-prod/uid-api");
    let selected = |cx: &mut TestAppContext| list.read_with(cx, |l, _| l.selected().is_some());
    assert!(selected(cx));

    // Its namespace left the watch: the object may still exist.
    list.update(cx, |list, cx| {
        list.set_source(one(limited(prod(&[]), &["team-a"], 6)), cx)
    });
    cx.run_until_parked();
    assert!(selected(cx), "a namespace not listed proves nothing");

    // Published again from the same rows of its namespace.
    list.update(cx, |list, cx| {
        list.set_source(one(limited(prod(&[]), &["default"], 5)), cx)
    });
    cx.run_until_parked();
    assert!(selected(cx), "rows no newer than it was seen in");

    // Its own namespace was listed anew without it.
    list.update(cx, |list, cx| {
        list.set_source(one(limited(prod(&[]), &["default"], 7)), cx)
    });
    cx.run_until_parked();
    assert!(!selected(cx));
}

#[gpui_kit::test]
async fn a_key_steps_from_the_selection_in_the_new_view(cx: &mut TestAppContext) {
    let (window, list) = open(cx, fixture());
    // Computed before the click, over a list where staging's rows come first.
    let reordered = CombinedSource::new(vec![
        member(
            snapshot("ctx-staging", "Node", &[("db", "n")], SyncState::Synced),
            "a-staging",
            None,
        ),
        member(
            prod(&[("api", "Running"), ("web", "Pending")]),
            "prod",
            None,
        ),
    ])
    .unwrap();
    let query = list.read_with(cx, |list, _| list.query.clone());
    let computed = super::compute_view(&reordered, &query, None, None, None, None, 0);
    click(cx, window, "row-ctx-prod/uid-web");

    let expected = CombinedRowId {
        cluster: "ctx-prod".into(),
        uid: "uid-web".into(),
    };
    let line = list.update(cx, |list, cx| {
        list.install(computed, reordered.clone(), None, false, cx);
        // Before the follow-up computation runs.
        let line = list.selected_line.expect("the selection's line");
        let flint_core::combined::CombinedLine::Row(at) = list.view.line(line).unwrap() else {
            panic!("not a row");
        };
        list.shown.list.row_id(at)
    });
    assert_eq!(line, Some(expected));
}

#[gpui_kit::test]
async fn a_chip_of_a_cluster_that_left_cannot_filter(cx: &mut TestAppContext) {
    let member_of = |cluster: &str, name: &str| {
        member(
            snapshot(cluster, "Status", &[("api", "x")], SyncState::Synced),
            name,
            None,
        )
    };
    let three = CombinedSource::new(vec![
        member_of("ctx-a", "a"),
        member_of("ctx-b", "b"),
        member_of("ctx-c", "c"),
    ])
    .unwrap();
    let two = CombinedSource::new(vec![member_of("ctx-a", "a"), member_of("ctx-b", "b")]).unwrap();
    let (_window, list) = open(cx, three);
    list.update(cx, |list, cx| {
        list.set_source(two, cx);
        // Its chip is still drawn until the new view lands.
        list.toggle_cluster_filter("ctx-c", cx);
        list.toggle_group("ctx-c", cx);
    });
    let shown = state(cx, &list);
    assert_eq!(shown.shown, 2);
    list.read_with(cx, |list, _| {
        assert!(list.query.clusters.is_empty());
        assert!(list.query.collapsed.is_empty());
    });
}

#[gpui_kit::test]
async fn the_filter_keys_move_between_filter_and_rows(cx: &mut TestAppContext) {
    use crate::ui::registry::{FocusFilter, FocusRows, SelectFromFilter};
    use gpui_kit::Focusable as _;

    let (window, list) = open(cx, fixture());
    let in_filter = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| {
            list.read(cx).filter.focus_handle(cx).is_focused(window)
        })
        .unwrap()
    };
    let dispatch = |cx: &mut TestAppContext, action: Box<dyn gpui_kit::Action>| {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.dispatch_action(action, cx);
        })
        .unwrap();
        cx.run_until_parked();
    };

    dispatch(cx, Box::new(FocusFilter));
    assert!(in_filter(cx));
    dispatch(cx, Box::new(FocusRows));
    assert!(!in_filter(cx));
    assert!(list.read_with(cx, |list, _| list.selected().is_none()));

    dispatch(cx, Box::new(FocusFilter));
    dispatch(cx, Box::new(SelectFromFilter));
    assert!(!in_filter(cx));
    assert!(list.read_with(cx, |list, _| list.selected().is_some()));

    // The registry's keys reach them from inside the filter's input.
    let press = |cx: &mut TestAppContext, key: &'static str| {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.press(key, cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    for key in ["escape", "enter", "down"] {
        dispatch(cx, Box::new(FocusFilter));
        assert!(in_filter(cx));
        press(cx, key);
        assert!(!in_filter(cx), "{key} returns to the rows");
    }
}

#[gpui_kit::test]
async fn a_list_is_loading_until_its_first_view(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::ui::registry::init(cx);
    });
    let (_window, list) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| CombinedResourceList::new("Pods".into(), fixture(), Vec::new(), window, cx))
        })
        .unwrap()
    });
    let before = list.read_with(cx, |list, _| list.view_state());
    assert_eq!(before.placeholder, Some("loading"));
    assert_eq!(before.count, "4");
    let after = state(cx, &list);
    assert_eq!(after.placeholder, None);
    assert_eq!(after.shown, 4);
}

#[gpui_kit::test]
async fn filtering_keeps_the_measured_widths(cx: &mut TestAppContext) {
    let (_window, list) = open(cx, fixture());
    let widths = |cx: &mut TestAppContext| {
        cx.run_until_parked();
        list.read_with(cx, |list, _| list.measured.as_ref().unwrap().widths.clone())
    };
    let first = widths(cx);
    list.update(cx, |list, cx| list.toggle_cluster_filter("ctx-prod", cx));
    assert!(
        Arc::ptr_eq(&first, &widths(cx)),
        "a filter does not measure"
    );
    list.update(cx, |list, cx| {
        list.set_namespaces(vec!["default".into()], cx)
    });
    assert!(!Arc::ptr_eq(&first, &widths(cx)), "a new scope measures");
}

#[gpui_kit::test]
async fn a_member_that_is_not_current_gets_a_note(cx: &mut TestAppContext) {
    let (_window, list) = open(cx, fixture());
    let shown = state(cx, &list);
    assert_eq!(shown.sync, "reconnecting");
    assert_eq!(shown.banners.len(), 1);
    assert_eq!(shown.banners[0].id, "list-member-ctx-staging");
    assert!(shown.banners[0].text.starts_with("staging:"));
}

fn only(cluster: &str, uid: &str) -> Arc<CombinedObjects> {
    let mut objects = CombinedObjects::default();
    objects.extend(cluster, [uid.to_owned()], []);
    Arc::new(objects)
}

#[gpui_kit::test]
async fn an_opening_filter_hides_the_earlier_rows_at_once(cx: &mut TestAppContext) {
    let (_window, list) = open(cx, fixture());
    assert_eq!(state(cx, &list).rows.len(), 4);
    list.update(cx, |list, cx| {
        list.show_objects("failing pods".into(), only("ctx-prod", "uid-web"), cx);
        assert_eq!(list.view.line_count(), 0);
        assert_eq!(list.empty_state(), Some(EmptyState::Loading));
    });
    assert_eq!(order(&state(cx, &list)), [pair("prod", "web")]);
}

#[gpui_kit::test]
async fn a_result_for_an_earlier_filter_or_membership_is_discarded(cx: &mut TestAppContext) {
    let (_window, list) = open(cx, fixture());
    cx.run_until_parked();
    let stale = |list: &CombinedResourceList| {
        let source = list.latest.clone();
        let query = CombinedQuery::default();
        (
            compute_view(&source, &query, None, None, None, None, 0),
            source,
            list.filter_revision,
        )
    };
    let (computed, source, revision) = list.read_with(cx, |list, _| stale(list));
    list.update(cx, |list, cx| {
        list.show_objects("failing pods".into(), only("ctx-prod", "uid-web"), cx);
        list.install_at(revision, computed, source, None, false, cx);
        assert_eq!(list.view.line_count(), 0, "an earlier filter's rows");
    });
    assert_eq!(order(&state(cx, &list)), [pair("prod", "web")]);

    let (computed, source, revision) = list.read_with(cx, |list, _| stale(list));
    list.update(cx, |list, cx| {
        list.set_source(one(prod(&[("web", "Pending")])), cx);
        assert_ne!(list.filter_revision, revision);
        list.install_at(revision, computed, source, None, false, cx);
        assert!(list.view.line_count() <= 1, "an earlier membership's rows");
    });
    assert_eq!(order(&state(cx, &list)), [pair("prod", "web")]);
}

#[gpui_kit::test]
async fn a_dashboard_filter_suspends_the_search_and_collapsed_groups(cx: &mut TestAppContext) {
    let (window, list) = open(cx, fixture());
    list.update(cx, |list, cx| {
        list.query.text = "db".to_owned();
        list.set_group_by_cluster(true, cx);
        list.toggle_group("ctx-prod", cx);
    });
    cx.run_until_parked();
    list.update(cx, |list, cx| {
        list.show_objects("failing pods".into(), only("ctx-prod", "uid-web"), cx);
    });
    assert_eq!(order(&state(cx, &list)), [pair("prod", "web")]);
    cx.update_window(window, |_, window, cx| {
        list.update(cx, |list, cx| {
            list.on_show_all(&ShowAllCombinedRows, window, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    list.read_with(cx, |list, _| {
        assert_eq!(list.query.collapsed, ["ctx-prod"]);
        assert_eq!(list.query.text, "db");
        assert_eq!(list.view.matched(), 1);
    });
}
