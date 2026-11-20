//! View tests for remembered sorts (#205): a header click is saved to `state.toml` per cluster
//! and kind, and restored when the list opens; a saved column that is gone falls back to the
//! kind's default order.

use std::sync::Arc;

use flint_core::settings::{ListSort, STATE_FILE};
use flint_core::{Cell, Column, ColumnRole, ColumnType, KindId, KindSnapshot, Row, SyncState};
use gpui_kit::TestAppContext;
use gpui_kit::test::TestAppContextExt as _;
use serde_json::{Value, json};

use super::support_tests::{TestWorkspace, WAIT, objects, open_workspace, pods};
use super::ui_state::UiState;
use super::view_state::SortState;

const PODS: &[(&str, &str, i64)] = &[("demo", "web", 3), ("demo", "api", 0), ("data", "db", 7)];

/// The open list's sort and row names, as drawn.
fn sort_and_rows(cx: &mut TestAppContext, t: &TestWorkspace) -> (Option<SortState>, Vec<String>) {
    let list = t.list(cx).expect("a list is open");
    list.read_with(cx, |list, _| {
        let state = list.view_state();
        (state.sort, state.rows.into_iter().map(|r| r.name).collect())
    })
}

fn sorted(column: &str, descending: bool) -> Option<SortState> {
    Some(SortState {
        column: column.to_owned(),
        descending,
    })
}

/// Waits until `state.toml` holds `text` (it is written off the UI thread).
async fn wait_for_saved(cx: &mut TestAppContext, t: &TestWorkspace, text: &'static str) {
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let path = t.dir.join(STATE_FILE);
    cx.wait_for(t.window, WAIT, move |_, _| {
        std::fs::read_to_string(&path).is_ok_and(|s| s.contains(text))
    })
    .await;
}

// covers UP-21
#[gpui_kit::test]
async fn a_sort_is_remembered_per_cluster_and_kind(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, PODS).await;
    assert_eq!(
        sort_and_rows(cx, &t),
        (None, vec!["db".into(), "api".into(), "web".into()]),
        "the default order: namespace, then name"
    );

    // Two clicks: descending by Restarts, saved by the column's key.
    t.click(cx, "sort-Restarts");
    t.click(cx, "sort-Restarts");
    cx.run_until_parked();
    wait_for_saved(cx, &t, "server:Restarts").await;
    assert_eq!(
        UiState::load(dir.path().to_owned()).list_sort("alpha", "pods"),
        Some(&ListSort {
            column: "server:Restarts".into(),
            descending: true,
        })
    );

    // Another kind keeps its own (default) order.
    let deployments = KindId::new("apps", "deployments");
    t.open_kind(
        cx,
        "deployments.apps",
        objects(deployments, PODS, SyncState::Synced, 1),
    )
    .await;
    assert_eq!(
        sort_and_rows(cx, &t),
        (None, vec!["db".into(), "api".into(), "web".into()])
    );

    // A cluster-scoped list with nothing saved, as Namespaces is by default: by name.
    let nodes = &[("", "node-c", 0), ("", "node-a", 0), ("", "node-b", 0)];
    t.open_kind(
        cx,
        "nodes",
        objects(KindId::new("", "nodes"), nodes, SyncState::Synced, 1),
    )
    .await;
    assert_eq!(
        sort_and_rows(cx, &t),
        (
            None,
            vec!["node-a".into(), "node-b".into(), "node-c".into()]
        )
    );

    // Back to Pods: sorted as left.
    t.open_kind(cx, "pods", pods(PODS)).await;
    assert_eq!(
        sort_and_rows(cx, &t),
        (
            sorted("Restarts", true),
            vec!["db".into(), "web".into(), "api".into()]
        )
    );

    // A third click goes back to the default order, and forgets the sort.
    t.click(cx, "sort-Restarts");
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let path = dir.path().join(STATE_FILE);
    cx.wait_for(t.window, WAIT, move |_, _| {
        std::fs::read_to_string(&path).is_ok_and(|s| !s.contains("list_sorts"))
    })
    .await;
    assert_eq!(
        UiState::load(dir.path().to_owned()).list_sort("alpha", "pods"),
        None
    );
}

// covers UP-21, UP-22
#[gpui_kit::test]
async fn a_saved_sort_on_a_column_that_is_gone_keeps_the_default_order(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // Saved before a CRD lost its printer column, and a sort on Name for Nodes.
    std::fs::write(
        dir.path().join(STATE_FILE),
        "[list_sorts.alpha]\n\
         pods = { column = \"server:Gone\", descending = true }\n\
         nodes = { column = \"flint:name\", descending = true }\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, PODS).await;
    assert_eq!(
        sort_and_rows(cx, &t),
        (None, vec!["db".into(), "api".into(), "web".into()])
    );

    // A click sorts as usual and replaces the stale choice.
    t.click(cx, "sort-Restarts");
    cx.run_until_parked();
    assert_eq!(
        sort_and_rows(cx, &t),
        (
            sorted("Restarts", false),
            vec!["api".into(), "web".into(), "db".into()]
        )
    );
    wait_for_saved(cx, &t, "server:Restarts").await;

    // The Name sort is restored on a cluster-scoped list too, by key.
    let nodes = &[("", "node-a", 0), ("", "node-c", 0), ("", "node-b", 0)];
    t.open_kind(
        cx,
        "nodes",
        objects(KindId::new("", "nodes"), nodes, SyncState::Synced, 1),
    )
    .await;
    assert_eq!(
        sort_and_rows(cx, &t),
        (
            sorted("Name", true),
            vec!["node-c".into(), "node-b".into(), "node-a".into()]
        )
    );
}

/// `PODS` with a wide Node column: web on node-a, db on node-b, api on node-c.
fn pods_with_nodes() -> Arc<KindSnapshot> {
    let base = pods(PODS);
    let mut columns: Vec<Column> = base.columns.to_vec();
    columns.push(Column {
        name: "Node".into(),
        ty: ColumnType::String,
        format: "".into(),
        description: "".into(),
        priority: 1,
        role: ColumnRole::Value,
    });
    let rows = base
        .rows
        .iter()
        .map(|row| {
            let node = match &*row.name {
                "web" => "node-a",
                "db" => "node-b",
                _ => "node-c",
            };
            let mut cells = row.cells.to_vec();
            cells.push(Cell::Text(node.into()));
            Arc::new(Row {
                cells: cells.into_boxed_slice(),
                ..(**row).clone()
            })
        })
        .collect();
    Arc::new(KindSnapshot {
        columns: columns.into(),
        rows,
        ..(*base).clone()
    })
}

// covers UP-20, UP-21
#[gpui_kit::test]
async fn a_saved_sort_on_a_wide_column_waits_for_wide(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(STATE_FILE),
        "[list_sorts.alpha]\npods = { column = \"server:Node\" }\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, super::support_tests::standard_sections())
        .await;
    t.open_kind(cx, "pods", pods_with_nodes()).await;
    // Node is not shown: the default order, and no sort to see.
    assert_eq!(
        sort_and_rows(cx, &t),
        (None, vec!["db".into(), "api".into(), "web".into()])
    );

    t.dispatch(cx, "resource_list::ToggleWide", Value::Null)
        .unwrap();
    cx.run_until_parked();
    let list = t.list(cx).unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        list.read(cx).view_state().sort.is_some()
    })
    .await;
    assert_eq!(
        sort_and_rows(cx, &t),
        (
            sorted("Node", false),
            vec!["web".into(), "db".into(), "api".into()]
        )
    );

    // Sorting by header name, as the palette does, is saved by key too.
    t.dispatch(cx, "resource_list::SortBy", json!({"column": "Restarts"}))
        .unwrap();
    cx.run_until_parked();
    wait_for_saved(cx, &t, "server:Restarts").await;
    assert_eq!(sort_and_rows(cx, &t).0, sorted("Restarts", false));
}
