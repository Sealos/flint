//! View tests for the combined list, in a headless GPUI window, from fixture snapshots.

use std::sync::Arc;

use flint_core::combined::{CLUSTER_COLUMN_KEY, ColumnKey, CombinedMember, CombinedRowId};
use flint_core::settings::ClusterTag;
use flint_core::{Cell, Column, ColumnRole, ColumnType, KindId, KindSnapshot, Row, SyncState};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

use super::super::super::list_model::combined::CombinedSource;
use super::super::super::view_state::ListState;
use super::{CombinedListEvent, CombinedResourceList, row_id};

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
            let CombinedListEvent::Selected(id) = event;
            seen.lock().unwrap().push(id.clone());
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
async fn a_member_that_is_not_current_gets_a_note(cx: &mut TestAppContext) {
    let (_window, list) = open(cx, fixture());
    let shown = state(cx, &list);
    assert_eq!(shown.sync, "reconnecting");
    assert_eq!(shown.banners.len(), 1);
    assert_eq!(shown.banners[0].id, "list-member-ctx-staging");
    assert!(shown.banners[0].text.starts_with("staging:"));
}
