//! View tests for remembered sorts (#205): a header click is saved to `state.toml` per cluster
//! and kind, and restored when the list opens; a saved column that is gone falls back to the
//! kind's default order.

use std::sync::Arc;

use flint_core::metrics::{ContainerMetrics, Cpu, Memory, PodMetrics, Usage};
use flint_core::settings::{ListSort, STATE_FILE};
use flint_core::{Cell, Column, ColumnRole, ColumnType, KindId, KindSnapshot, Row, SyncState};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::list_usage::UsageSource;
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

/// Waits until the open list shows `want`: its sort and its rows in order (the core sorts off
/// the UI thread).
async fn expect_rows(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    want: (Option<SortState>, Vec<String>),
) {
    let list = t.list(cx).expect("a list is open");
    let seen = want.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let state = list.read(cx).view_state();
        let rows: Vec<String> = state.rows.into_iter().map(|r| r.name).collect();
        (state.sort, rows) == seen
    })
    .await;
    assert_eq!(sort_and_rows(cx, t), want);
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

/// Usage for the pods of [`PODS`]: `web` and `db` report, `api` does not.
fn pod_usage() -> UsageSource {
    let sample = |millis: u64| {
        PodMetrics {
            namespace: String::new(),
            name: String::new(),
            // After the test rows' creation time.
            timestamp: Some(jiff::Timestamp::from_second(1_000).unwrap()),
            window: Some(jiff::SignedDuration::from_secs(15)),
            containers: vec![ContainerMetrics {
                name: "app".into(),
                usage: Usage {
                    cpu: Cpu::from_nanos(millis * 1_000_000),
                    memory: Memory::from_bytes(0),
                },
            }],
        }
        .sample()
    };
    UsageSource::Pods(Arc::new(
        [
            (("demo".to_owned(), "web".to_owned()), sample(5)),
            (("data".to_owned(), "db".to_owned()), sample(40)),
        ]
        .into_iter()
        .collect(),
    ))
}

fn cpu_cells(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<(String, Option<String>)> {
    let list = t.list(cx).expect("a list is open");
    list.read_with(cx, |list, _| {
        let rows = list.view_state().rows;
        rows.into_iter()
            .map(|row| {
                let cpu = row
                    .cells_by_column
                    .into_iter()
                    .find(|(column, _)| column == "CPU")
                    .map(|(_, cell)| cell);
                (row.name, cpu)
            })
            .collect()
    })
}

/// Waits until the rows drawn have a CPU column. The core computes them on its own thread, so
/// `run_until_parked` does not wait for them.
async fn wait_for_cpu_column(cx: &mut TestAppContext, t: &TestWorkspace) {
    let list = t.list(cx).expect("a list is open");
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let rows = list.read(cx).view_state().rows;
        !rows.is_empty()
            && rows.iter().all(|row| {
                row.cells_by_column
                    .iter()
                    .any(|(column, _)| column == "CPU")
            })
    })
    .await;
}

#[gpui_kit::test]
async fn usage_columns_show_sort_and_keep_unknown_last(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, PODS).await;
    let list = t.list(cx).expect("a list is open");
    list.update(cx, |list, cx| list.pin_usage_source(pod_usage(), cx));
    wait_for_cpu_column(cx, &t).await;

    let unsorted = cpu_cells(cx, &t);
    let cell = |name: &str| {
        unsorted
            .iter()
            .find(|(row, _)| row == name)
            .and_then(|(_, cpu)| cpu.clone())
    };
    assert_eq!(cell("web").as_deref(), Some("5m"));
    assert_eq!(cell("api").as_deref(), Some("unknown"));

    t.click(cx, "sort-CPU");
    expect_rows(
        cx,
        &t,
        (
            sorted("CPU", false),
            vec!["web".into(), "db".into(), "api".into()],
        ),
    )
    .await;
    t.click(cx, "sort-CPU");
    expect_rows(
        cx,
        &t,
        (
            sorted("CPU", true),
            vec!["db".into(), "web".into(), "api".into()],
        ),
    )
    .await;
}

#[gpui_kit::test]
async fn without_metrics_server_the_columns_hide_and_a_hint_shows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, PODS).await;
    let list = t.list(cx).expect("a list is open");
    list.update(cx, |list, cx| list.pin_usage_source(pod_usage(), cx));
    wait_for_cpu_column(cx, &t).await;

    list.update(cx, |list, cx| {
        list.pin_usage_source(UsageSource::NotInstalled, cx);
    });
    cx.run_until_parked();
    assert!(cpu_cells(cx, &t).iter().all(|(_, cpu)| cpu.is_none()));
    let hinted = list.read_with(cx, |list, _| {
        list.view_state()
            .banners
            .iter()
            .any(|banner| banner.id == "list-usage-hint")
    });
    assert!(hinted);
}

#[gpui_kit::test]
async fn with_no_feed_the_toolbar_says_so_and_rows_keep_their_room(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, PODS).await;
    let list = t.list(cx).expect("a list is open");
    list.update(cx, |list, cx| {
        list.pin_usage_source(UsageSource::NoFeed, cx);
    });
    cx.run_until_parked();
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(t.find(cx, "list-usage-no-feed").is_some());
    assert!(t.find(cx, "list-usage-hint").is_none());
    assert!(cpu_cells(cx, &t).iter().all(|(_, cpu)| cpu.is_none()));

    list.update(cx, |list, cx| {
        list.pin_usage_source(UsageSource::NotInstalled, cx);
    });
    cx.run_until_parked();
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(t.find(cx, "list-usage-no-feed").is_none());
    assert!(t.find(cx, "list-usage-hint").is_some());
}

#[gpui_kit::test]
async fn a_404_during_a_sort_hides_the_old_readings_at_once(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, PODS).await;
    let list = t.list(cx).expect("a list is open");
    list.update(cx, |list, cx| list.pin_usage_source(pod_usage(), cx));
    wait_for_cpu_column(cx, &t).await;

    // A sort is computing when the poll finds the API gone.
    let permits = list.update(cx, |list, _| list.hold_computes());
    t.click(cx, "sort-CPU");
    cx.run_until_parked();
    list.update(cx, |list, cx| {
        list.pin_usage_source(UsageSource::NotInstalled, cx);
    });
    assert!(
        cpu_cells(cx, &t).iter().all(|(_, cpu)| cpu.is_none()),
        "the hint must not show beside old readings"
    );

    // Neither the sort that was running nor the one after it brings them back.
    let done = list.read_with(cx, |list, _| list.computes_done());
    permits.send(()).await.unwrap();
    permits.send(()).await.unwrap();
    let finished = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        finished.read(cx).computes_done() >= done + 2
    })
    .await;
    assert!(cpu_cells(cx, &t).iter().all(|(_, cpu)| cpu.is_none()));
}

// covers UP-21
#[gpui_kit::test]
async fn a_sort_is_remembered_per_cluster_and_kind(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, PODS).await;
    expect_rows(
        cx,
        &t,
        (None, vec!["db".into(), "api".into(), "web".into()]),
    )
    .await;

    // Two clicks: descending by Restarts, saved by the column's key.
    t.click(cx, "sort-Restarts");
    t.click(cx, "sort-Restarts");
    cx.run_until_parked();
    wait_for_saved(cx, &t, "server:Restarts").await;
    assert_eq!(
        UiState::load(dir.path().to_owned()).list_sort("kind-alpha", "pods"),
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
    expect_rows(
        cx,
        &t,
        (None, vec!["db".into(), "api".into(), "web".into()]),
    )
    .await;

    // A cluster-scoped list with nothing saved, as Namespaces is by default: by name.
    let nodes = &[("", "node-c", 0), ("", "node-a", 0), ("", "node-b", 0)];
    t.open_kind(
        cx,
        "nodes",
        objects(KindId::new("", "nodes"), nodes, SyncState::Synced, 1),
    )
    .await;
    expect_rows(
        cx,
        &t,
        (
            None,
            vec!["node-a".into(), "node-b".into(), "node-c".into()],
        ),
    )
    .await;

    // Back to Pods: sorted as left.
    t.open_kind(cx, "pods", pods(PODS)).await;
    expect_rows(
        cx,
        &t,
        (
            sorted("Restarts", true),
            vec!["db".into(), "web".into(), "api".into()],
        ),
    )
    .await;

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
        UiState::load(dir.path().to_owned()).list_sort("kind-alpha", "pods"),
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
        "[list_sorts.kind-alpha]\n\
         pods = { column = \"server:Gone\", descending = true }\n\
         nodes = { column = \"flint:name\", descending = true }\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, PODS).await;
    expect_rows(
        cx,
        &t,
        (None, vec!["db".into(), "api".into(), "web".into()]),
    )
    .await;

    // A click sorts as usual and replaces the stale choice.
    t.click(cx, "sort-Restarts");
    cx.run_until_parked();
    expect_rows(
        cx,
        &t,
        (
            sorted("Restarts", false),
            vec!["api".into(), "web".into(), "db".into()],
        ),
    )
    .await;
    wait_for_saved(cx, &t, "server:Restarts").await;

    // The Name sort is restored on a cluster-scoped list too, by key.
    let nodes = &[("", "node-a", 0), ("", "node-c", 0), ("", "node-b", 0)];
    t.open_kind(
        cx,
        "nodes",
        objects(KindId::new("", "nodes"), nodes, SyncState::Synced, 1),
    )
    .await;
    expect_rows(
        cx,
        &t,
        (
            sorted("Name", true),
            vec!["node-c".into(), "node-b".into(), "node-a".into()],
        ),
    )
    .await;
}

/// `PODS` with one more column, `header`, of `priority` (1 and up: wide), whose cell for each
/// pod is `cell(name)`.
fn pods_with(header: &str, priority: i32, cell: fn(&str) -> &'static str) -> Arc<KindSnapshot> {
    let base = pods(PODS);
    let mut columns: Vec<Column> = base.columns.to_vec();
    columns.push(Column {
        name: header.into(),
        ty: ColumnType::String,
        format: "".into(),
        description: "".into(),
        priority,
        role: ColumnRole::Value,
    });
    let rows = base
        .rows
        .iter()
        .map(|row| {
            let mut cells = row.cells.to_vec();
            cells.push(Cell::Text(cell(&row.name).into()));
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

/// `PODS` with a wide Node column: web on node-a, db on node-b, api on node-c.
fn pods_with_nodes() -> Arc<KindSnapshot> {
    pods_with("Node", 1, |name| match name {
        "web" => "node-a",
        "db" => "node-b",
        _ => "node-c",
    })
}

// covers UP-20, UP-21
#[gpui_kit::test]
async fn a_saved_sort_on_a_wide_column_waits_for_wide(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(STATE_FILE),
        "[list_sorts.kind-alpha]\npods = { column = \"server:Node\" }\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, super::support_tests::standard_sections())
        .await;
    t.open_kind(cx, "pods", pods_with_nodes()).await;
    // Node is not shown: the default order, and no sort to see.
    expect_rows(
        cx,
        &t,
        (None, vec!["db".into(), "api".into(), "web".into()]),
    )
    .await;

    t.dispatch(cx, "resource_list::ToggleWide", Value::Null)
        .unwrap();
    cx.run_until_parked();
    expect_rows(
        cx,
        &t,
        (
            sorted("Node", false),
            vec!["web".into(), "db".into(), "api".into()],
        ),
    )
    .await;

    // Sorting by header name, as the palette does, is saved by key too.
    t.dispatch(cx, "resource_list::SortBy", json!({"column": "Restarts"}))
        .unwrap();
    cx.run_until_parked();
    wait_for_saved(cx, &t, "server:Restarts").await;
    expect_rows(
        cx,
        &t,
        (
            sorted("Restarts", false),
            vec!["api".into(), "web".into(), "db".into()],
        ),
    )
    .await;
}

// covers UP-20, UP-21: Wide is on, but the pane is too narrow for Node.
#[gpui_kit::test]
async fn a_saved_sort_waits_while_a_narrow_pane_leaves_its_column_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(STATE_FILE),
        "[list_sorts.kind-alpha]\npods = { column = \"server:Node\" }\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, super::support_tests::standard_sections())
        .await;
    t.open_kind(cx, "pods", pods_with_nodes()).await;
    t.dispatch(cx, "resource_list::ToggleWide", Value::Null)
        .unwrap();
    let applied = (
        sorted("Node", false),
        vec!["web".into(), "db".into(), "api".into()],
    );
    expect_rows(cx, &t, applied.clone()).await;

    // Narrow: Node is left out, and so is its sort; the preference stays saved.
    cx.simulate_window_resize(
        t.window,
        gpui_kit::size(gpui_kit::px(560.), gpui_kit::px(700.)),
    );
    cx.run_until_parked();
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("sort-Node").is_none()
    })
    .await;
    expect_rows(
        cx,
        &t,
        (None, vec!["db".into(), "api".into(), "web".into()]),
    )
    .await;
    assert!(
        std::fs::read_to_string(dir.path().join(STATE_FILE))
            .unwrap()
            .contains("server:Node")
    );

    // Wide enough again: sorted by Node.
    cx.simulate_window_resize(
        t.window,
        gpui_kit::size(gpui_kit::px(1100.), gpui_kit::px(700.)),
    );
    cx.run_until_parked();
    expect_rows(cx, &t, applied).await;
}

// covers UP-21: a header that looks like a key is still a header.
#[gpui_kit::test]
async fn sorting_by_header_never_matches_a_key(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, super::support_tests::standard_sections())
        .await;
    // A CRD printer column headed "flint:name": web z, db y, api x.
    let snapshot = pods_with("flint:name", 0, |name| match name {
        "web" => "z",
        "db" => "y",
        _ => "x",
    });
    t.open_kind(cx, "pods", snapshot).await;
    // As the palette sends it: by header.
    t.dispatch(cx, "resource_list::SortBy", json!({"column": "flint:name"}))
        .unwrap();
    expect_rows(
        cx,
        &t,
        (
            sorted("flint:name", false),
            vec!["api".into(), "db".into(), "web".into()],
        ),
    )
    .await;
    wait_for_saved(cx, &t, "server:flint:name").await;
    // By key, as a header click sends it: the built-in Name column.
    t.dispatch(
        cx,
        "resource_list::SortBy",
        json!({"column": "Name", "key": "flint:name"}),
    )
    .unwrap();
    expect_rows(
        cx,
        &t,
        (
            sorted("Name", false),
            vec!["api".into(), "db".into(), "web".into()],
        ),
    )
    .await;
    // A key never matches a header, nor a header a key.
    assert!(
        t.dispatch(
            cx,
            "resource_list::SortBy",
            json!({"column": "server:Restarts"})
        )
        .is_err()
    );
    assert!(
        t.dispatch(
            cx,
            "resource_list::SortBy",
            json!({"column": "Restarts", "key": "Restarts"})
        )
        .is_err()
    );
}

// covers UP-20, UP-21: rows sorted for a layout that changed meanwhile are never drawn under the
// new header.
#[gpui_kit::test]
async fn rows_and_their_sort_arrow_always_agree(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(STATE_FILE),
        "[list_sorts.kind-alpha]\npods = { column = \"server:Node\" }\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, super::support_tests::standard_sections())
        .await;
    t.open_kind(cx, "pods", pods_with_nodes()).await;
    t.dispatch(cx, "resource_list::ToggleWide", Value::Null)
        .unwrap();
    let by_node = (
        sorted("Node", false),
        vec!["web".into(), "db".into(), "api".into()],
    );
    let default = (None, vec!["db".into(), "api".into(), "web".into()]);
    expect_rows(cx, &t, by_node.clone()).await;

    // A new snapshot is sorted by Node while the pane narrows and leaves Node out. The sort is
    // held until the narrow layout is drawn, then the jobs are let through one at a time: the
    // rows sorted for the old layout are dropped, never shown under the new header.
    let list = t.list(cx).unwrap();
    let permits = list.update(cx, |list, _| list.hold_computes());
    let done = |cx: &mut TestAppContext| list.read_with(cx, |list, _| list.computes_done());
    let before = done(cx);
    let newer = Arc::new(KindSnapshot {
        version: 2,
        ..(*pods_with_nodes()).clone()
    });
    list.update(cx, |list, cx| list.set_snapshot(newer, cx));
    cx.simulate_window_resize(
        t.window,
        gpui_kit::size(gpui_kit::px(560.), gpui_kit::px(700.)),
    );
    // Drawn twice: the width is known after the first layout.
    for _ in 0..2 {
        cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("sort-Node").is_none()
    })
    .await;
    assert_eq!(done(cx), before, "the sort is held");
    let state = |cx: &mut TestAppContext| {
        list.read_with(cx, |list, _| {
            let version = list.shown().0.version;
            let state = list.view_state();
            let rows: Vec<String> = state.rows.into_iter().map(|r| r.name).collect();
            (version, (state.sort, rows))
        })
    };
    // The new snapshot's states, after each job comes back, until it is drawn sorted for the
    // narrow layout.
    let mut seen = Vec::new();
    for job in 1..=5 {
        permits.try_send(()).unwrap();
        let list = list.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            list.read(cx).computes_done() >= before + job
        })
        .await;
        cx.run_until_parked();
        let (version, shown) = state(cx);
        if version == 2 {
            seen.push(shown.clone());
            if shown == default {
                break;
            }
        }
    }
    assert_eq!(seen.last(), Some(&default), "seen: {seen:?}");
    for state in &seen {
        assert!(*state == by_node || *state == default, "drawn: {state:?}");
    }
}
