//! View tests for the resource list, in a headless GPUI window (`#[gpui_kit::test]`). Rows are
//! fed by hand; filtering and sorting run through a real core's `compute_view`.

use std::sync::Arc;
use std::time::Duration;

use flint_core::{
    Cell, Column, ColumnRole, ColumnType, CoreConfig, CoreHandle, KindId, KindSnapshot, Row,
    Settings, SyncState,
};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, Window, WindowBounds,
    WindowOptions, px, size,
};

use super::ResourceList;

const WAIT: Duration = Duration::from_secs(10);

fn core() -> (CoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().to_owned(),
        settings: Settings::default(),
        settings_error: None,
        kubeconfig_base: Vec::new(),
        home: None,
        watch_files: false,
    })
    .unwrap();
    (core, dir)
}

fn column(name: &str, role: ColumnRole, ty: ColumnType, priority: i32) -> Column {
    Column {
        name: name.into(),
        ty,
        format: "".into(),
        description: "".into(),
        priority,
        role,
    }
}

/// Pods as (namespace, name, restarts), in snapshot order (namespace, then name).
fn pods(rows: &[(&str, &str, i64)], state: SyncState, version: u64) -> Arc<KindSnapshot> {
    let columns: Arc<[Column]> = vec![
        column("Name", ColumnRole::Name, ColumnType::String, 0),
        column("Namespace", ColumnRole::Namespace, ColumnType::String, 0),
        column("Restarts", ColumnRole::Value, ColumnType::Integer, 0),
        column("Node", ColumnRole::Value, ColumnType::String, 1),
    ]
    .into();
    let mut rows: Vec<Arc<Row>> = rows
        .iter()
        .map(|(ns, name, restarts)| {
            Arc::new(Row {
                uid: format!("uid-{name}").into(),
                name: (*name).into(),
                namespace: Some((*ns).into()),
                resource_version: "1".into(),
                generation: None,
                created: Some(0),
                labels: Box::new([]),
                cells: Box::new([
                    Cell::Name,
                    Cell::Namespace,
                    Cell::Int(*restarts),
                    Cell::Text("node-1".into()),
                ]),
            })
        })
        .collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Arc::new(KindSnapshot {
        cluster: "test".into(),
        kind: KindId::pods(),
        namespaced: true,
        columns,
        rows,
        state,
        version,
        ..Default::default()
    })
}

fn open(cx: &mut TestAppContext, core: CoreHandle) -> (AnyWindowHandle, Entity<ResourceList>) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        super::init(cx);
    });
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(900.), px(600.)),
        };
        let (window, list) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    let mut list = ResourceList::new(
                        core,
                        "test".into(),
                        KindId::pods(),
                        "Pods".into(),
                        Vec::new(),
                        window,
                        cx,
                    );
                    list.stop_watching();
                    list
                })
            },
        )
        .unwrap();
        (window, list)
    })
}

/// The rows on screen, top to bottom, by uid.
fn visible_rows(window: &Window, uids: &[&str]) -> Vec<String> {
    let mut found: Vec<(Option<f32>, String)> = uids
        .iter()
        .map(|uid| {
            let y = window
                .try_find(format!("row-{uid}"))
                .map(|row| f32::from(row.bounds().origin.y));
            (y, uid.to_string())
        })
        .filter(|(y, _)| y.is_some())
        .collect();
    found.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    found.into_iter().map(|(_, uid)| uid).collect()
}

const ALL: &[&str] = &["uid-api", "uid-web", "uid-db", "uid-dns"];

// covers UP-20, UP-21
#[gpui_kit::test]
async fn list_sorts_by_header_and_filters_by_namespace_and_text(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open(cx, core);
    let data = pods(
        &[
            ("demo", "web", 3),
            ("demo", "api", 0),
            ("data", "db", 7),
            ("kube-system", "dns", 1),
        ],
        SyncState::Synced,
        1,
    );
    list.update(cx, |list, cx| list.set_snapshot(data.clone(), cx));

    // Snapshot order: namespace, then name.
    let expect = |order: &'static [&'static str]| {
        move |window: &mut Window, _: &mut gpui_kit::App| visible_rows(window, ALL) == order
    };
    cx.wait_for(
        window,
        WAIT,
        expect(&["uid-db", "uid-api", "uid-web", "uid-dns"]),
    )
    .await;

    // Header clicks: ascending, descending, back to the default order.
    cx.update_window(window, |_, window, cx| window.click("sort-Restarts", cx))
        .unwrap();
    cx.wait_for(
        window,
        WAIT,
        expect(&["uid-api", "uid-dns", "uid-web", "uid-db"]),
    )
    .await;
    cx.update_window(window, |_, window, cx| window.click("sort-Restarts", cx))
        .unwrap();
    cx.wait_for(
        window,
        WAIT,
        expect(&["uid-db", "uid-web", "uid-dns", "uid-api"]),
    )
    .await;

    // The sort survives an update that moves rows and changes the data.
    let updated = pods(
        &[
            ("demo", "web", 3),
            ("demo", "api", 9),
            ("data", "db", 7),
            ("kube-system", "dns", 1),
        ],
        SyncState::Synced,
        2,
    );
    list.update(cx, |list, cx| list.set_snapshot(updated, cx));
    cx.wait_for(
        window,
        WAIT,
        expect(&["uid-api", "uid-db", "uid-web", "uid-dns"]),
    )
    .await;

    // Namespace filter (several namespaces), then a text filter typed into the input.
    list.update(cx, |list, cx| {
        list.set_namespaces(vec!["demo".into(), "kube-system".into()], cx)
    });
    cx.wait_for(window, WAIT, expect(&["uid-api", "uid-web", "uid-dns"]))
        .await;
    cx.update_window(window, |_, window, cx| {
        window.click("list-filter", cx);
        window.input("we", cx);
    })
    .unwrap();
    cx.wait_for(window, WAIT, expect(&["uid-web"])).await;
    let count = list.read_with(cx, |list, _| list.shown().1);
    assert_eq!(count, 1);

    // Nothing matches: the empty state explains that filters hide the rows.
    cx.update_window(window, |_, window, cx| window.input("zzz", cx))
        .unwrap();
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("list-placeholder").is_some() && visible_rows(window, ALL).is_empty()
    })
    .await;
}

// covers UP-20, UP-27
#[gpui_kit::test]
async fn list_shows_loading_stale_and_failed_states(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open(cx, core);

    list.update(cx, |list, cx| {
        list.set_snapshot(pods(&[], SyncState::Loading, 1), cx)
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("list-placeholder").is_some()
    })
    .await;

    // Reconnecting keeps the rows, marked as possibly out of date.
    let rows = [("demo", "web", 0)];
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &rows,
                SyncState::Reconnecting {
                    error: Some("watch expired".into()),
                },
                2,
            ),
            cx,
        )
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("row-uid-web").is_some() && window.try_find("list-banner").is_some()
    })
    .await;

    // Synced again: the banner goes away.
    list.update(cx, |list, cx| {
        list.set_snapshot(pods(&rows, SyncState::Synced, 3), cx)
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("row-uid-web").is_some() && window.try_find("list-banner").is_none()
    })
    .await;

    // Failed with no rows: the error replaces the table.
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &[],
                SyncState::Failed {
                    error: "forbidden".into(),
                },
                4,
            ),
            cx,
        )
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("list-placeholder").is_some() && window.try_find("row-uid-web").is_none()
    })
    .await;
}

// covers UP-20
#[gpui_kit::test]
async fn wide_columns_appear_only_when_toggled(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open(cx, core);
    list.update(cx, |list, cx| {
        list.set_snapshot(pods(&[("demo", "web", 0)], SyncState::Synced, 1), cx)
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("sort-Restarts").is_some() && window.try_find("sort-Node").is_none()
    })
    .await;
    cx.update_window(window, |_, window, cx| window.click("toggle-wide", cx))
        .unwrap();
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("sort-Node").is_some()
    })
    .await;
}

// covers UP-33 (basic arrows), UP-23 (selection opens the detail)
#[gpui_kit::test]
async fn clicking_and_arrows_select_rows(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open(cx, core);
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&list, move |_, event: &super::ListEvent, _| {
            if let super::ListEvent::Selected(selected) = event {
                seen.borrow_mut()
                    .push(selected.as_ref().map(|s| s.name.clone()));
            }
        })
    });
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &[("demo", "api", 0), ("demo", "web", 0)],
                SyncState::Synced,
                1,
            ),
            cx,
        )
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("row-uid-web").is_some()
    })
    .await;
    cx.update_window(window, |_, window, cx| {
        window.click("row-uid-api", cx);
        window.press("down", cx);
        window.press("escape", cx);
    })
    .unwrap();
    assert_eq!(
        *events.borrow(),
        [Some("api".to_owned()), Some("web".to_owned()), None]
    );
}

/// A quick measure of what one frame of a 10,000-row list costs on the UI thread (layout and
/// paint of the visible rows, in the headless test window). Ignored: run it in release mode
/// with `--ignored --nocapture` to compare changes to the row rendering.
#[gpui_kit::test]
#[ignore = "measurement: run with --release --ignored --nocapture"]
async fn measure_frame_cost_with_10k_rows(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open(cx, core);
    let rows: Vec<(String, String, i64)> = (0..10_000)
        .map(|i| (format!("ns-{}", i % 50), format!("pod-{i:05}"), i % 7))
        .collect();
    let refs: Vec<(&str, &str, i64)> = rows
        .iter()
        .map(|(ns, n, r)| (ns.as_str(), n.as_str(), *r))
        .collect();
    let data = pods(&refs, SyncState::Synced, 1);
    list.update(cx, |list, cx| list.set_snapshot(data, cx));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("row-uid-pod-00000").is_some()
    })
    .await;
    let frames = 300;
    let started = std::time::Instant::now();
    for _ in 0..frames {
        cx.update_window(window, |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
    }
    let per_frame = started.elapsed() / frames;
    eprintln!("10k rows: {per_frame:?} per frame (headless, {frames} frames)");
}
