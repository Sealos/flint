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
        watch_timing: Default::default(),
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
    open_sized(cx, core, "Pods", 900.)
}

/// A pods list titled `label`, in a window `width` wide.
fn open_sized(
    cx: &mut TestAppContext,
    core: CoreHandle,
    label: &'static str,
    width: f32,
) -> (AnyWindowHandle, Entity<ResourceList>) {
    open_kind(cx, core, "pods", label, width)
}

/// A list of the core kind `plural` titled `label`, in a window `width` wide.
fn open_kind(
    cx: &mut TestAppContext,
    core: CoreHandle,
    plural: &'static str,
    label: &'static str,
    width: f32,
) -> (AnyWindowHandle, Entity<ResourceList>) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::ui::registry::init(cx);
    });
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(width), px(600.)),
        };
        let (window, list) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    let entry = crate::ui::support_tests::entry("", plural, "Pod", label, true);
                    let mut list = ResourceList::new(
                        core,
                        "test".into(),
                        &entry,
                        Vec::new(),
                        None,
                        window,
                        cx,
                    );
                    list.stop_watching();
                    list
                })
            },
        )
        .unwrap();
        // The list has focus, as when opened in the app, so clicks have a place to dispatch
        // their actions from.
        window
            .update(cx, |_, window, cx| {
                let focus = list.read(cx).focus_handle().clone();
                window.focus(&focus, cx);
            })
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

// covers UP-21: beside an open detail pane the list is about 280 px wide, and a long kind name
// must not push the filter out of the toolbar.
#[gpui_kit::test]
async fn a_long_kind_name_leaves_the_filter_usable_in_a_narrow_list(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open_sized(cx, core, "Custom Resource Definitions", 280.);
    list.update(cx, |list, cx| {
        list.set_snapshot(pods(&[("default", "api", 0)], SyncState::Synced, 1), cx)
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("list-filter").is_some()
    })
    .await;
    cx.update_window(window, |_, window, _| {
        let filter = window.try_find("list-filter").unwrap().bounds();
        assert!(
            f32::from(filter.size.width) >= 72.,
            "filter is {:?} wide",
            filter.size.width
        );
        assert!(
            f32::from(filter.origin.x + filter.size.width) <= 280.,
            "filter ends at {:?}, past the list's edge",
            filter.origin.x + filter.size.width
        );
    })
    .unwrap();
}

/// The drawn width of the header cell of `column`.
fn header_width(cx: &mut TestAppContext, window: AnyWindowHandle, column: &str) -> f32 {
    let id = format!("sort-{column}");
    cx.update_window(window, |_, window, _| {
        f32::from(window.find(id).bounds().size.width)
    })
    .unwrap()
}

// covers UP-20, UP-25, UP-27: in a wide list a long name shows in full; the room goes to
// Name, not to a filler after the last column. Narrower, Namespace gives way before Name.
#[gpui_kit::test]
async fn the_name_column_takes_the_spare_width(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let long = "podinfo-frontend-canary-7b9c8d5f4-abcde-with-a-long-suffix-xy";
    assert_eq!(long.chars().count(), 61);
    let (window, list) = open_sized(cx, core, "Pods", 1000.);
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &[("local-path-storage", long, 0), ("demo", "api", 0)],
                SyncState::Synced,
                1,
            ),
            cx,
        )
    });
    cx.wait_for(window, WAIT, |window, _| {
        window
            .try_find("sort-Name")
            .is_some_and(|h| f32::from(h.bounds().size.width) > 500.)
    })
    .await;
    // 61 characters at 8.5 px, plus padding: well past the 48-character cap.
    let name = header_width(cx, window, "Name");
    assert!(name >= 61. * 8.5 + 16. - 0.5, "Name is {name} wide");
}

// covers UP-21: typing a filter leaves every column where it was, even when the row it hides is
// the widest; choosing a namespace still fits the columns to that namespace.
#[gpui_kit::test]
async fn a_text_filter_keeps_the_column_widths(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let long = "podinfo-frontend-canary-7b9c8d5f4-abcde-with-a-long-suffix-xy";
    let (window, list) = open_sized(cx, core, "Pods", 1000.);
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &[("demo", long, 0), ("other", "api", 0)],
                SyncState::Synced,
                1,
            ),
            cx,
        )
    });
    cx.wait_for(window, WAIT, |window, _| {
        window
            .try_find("sort-Name")
            .is_some_and(|h| f32::from(h.bounds().size.width) > 500.)
    })
    .await;
    cx.run_until_parked();
    let headers = ["Name", "Namespace", "Restarts"];
    let before = headers.map(|column| header_width(cx, window, column));
    cx.update_window(window, |_, window, cx| {
        window.click("list-filter", cx);
        window.input("api", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let state = list.read_with(cx, |list, _| list.view_state());
    assert_eq!(state.rows.len(), 1, "the filter leaves one row");
    let after = headers.map(|column| header_width(cx, window, column));
    assert_eq!(after, before, "columns moved while filtering");

    list.update(cx, |list, cx| {
        list.set_namespaces(vec!["other".to_owned()], cx)
    });
    // Back to the Name column's minimum (16 characters): "api" is all that namespace has.
    cx.wait_for(window, WAIT, |window, _| {
        window
            .try_find("sort-Name")
            .is_some_and(|h| f32::from(h.bounds().size.width) < 16. * 8.5 + 16. + 0.5)
    })
    .await;
}

// covers UP-23: beside an open detail pane the list is narrow; the names of one workload must
// stay distinguishable (cut in the middle), and NAMESPACE keeps room for its header.
#[gpui_kit::test]
async fn a_narrow_list_cuts_names_in_the_middle(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open_sized(cx, core, "Pods", 360.);
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &[
                    ("local-path-storage", "podinfo-7b9c8d5f4-abcde", 0),
                    ("local-path-storage", "podinfo-7b9c8d5f4-vwxyz", 0),
                ],
                SyncState::Synced,
                1,
            ),
            cx,
        )
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("sort-Namespace").is_some()
    })
    .await;
    cx.run_until_parked();
    let namespace = header_width(cx, window, "Namespace");
    assert!(
        namespace >= 11. * 8.5 + 16. - 0.5,
        "Namespace is {namespace} wide"
    );
    let state = list.read_with(cx, |list, _| list.view_state());
    assert_eq!(state.rows.len(), 2);
    let names = cx
        .update_window(window, |_, window, _| {
            gpui_kit::base::test_support::texts(window)
                .into_iter()
                .map(|t| t.text().to_owned())
                .filter(|t| t.starts_with("podi"))
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(names.len(), 2, "{names:?}");
    assert_ne!(
        names[0], names[1],
        "truncated names look the same: {names:?}"
    );
    assert!(
        names.iter().all(|n| n.contains('…')),
        "names are not cut: {names:?}"
    );
}

/// The texts painted in the window's last frame.
fn painted_texts(cx: &mut TestAppContext, window: AnyWindowHandle) -> Vec<String> {
    cx.update_window(window, |_, window, _| {
        gpui_kit::base::test_support::texts(window)
            .into_iter()
            .map(|t| t.text().to_owned())
            .collect()
    })
    .unwrap()
}

/// Moves the pointer over `row`'s cell in `column` (cells line up with their headers), lets a
/// tooltip's delay pass, and returns the text of the tooltips built meanwhile.
fn hover_cell(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    row: &str,
    column: &str,
) -> Vec<String> {
    super::CELL_TOOLTIPS.with_borrow_mut(Vec::clear);
    let (row, header) = (format!("row-{row}"), format!("sort-{column}"));
    cx.update_window(window, |_, window, cx| {
        let row = window.find(row).bounds();
        let header = window.find(header).bounds();
        let at = Point::new(
            header.origin.x + header.size.width / 2.,
            row.origin.y + row.size.height / 2.,
        );
        window.simulate_mouse_move(at, cx);
    })
    .unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    super::CELL_TOOLTIPS.with_borrow(Clone::clone)
}

// covers UP-20: a Secret list shows only identity columns; a printer column added by an
// extension could hold a value, so it is masked.
#[gpui_kit::test]
async fn a_secret_list_masks_its_non_safe_columns(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open_kind(cx, core, "secrets", "Secrets", 900.);
    let columns: Arc<[Column]> = vec![
        column("Name", ColumnRole::Name, ColumnType::String, 0),
        column("Type", ColumnRole::Value, ColumnType::String, 0),
        column("Token", ColumnRole::Value, ColumnType::String, 0),
    ]
    .into();
    let row = Arc::new(Row {
        uid: "uid-creds".into(),
        name: "creds".into(),
        namespace: Some("demo".into()),
        resource_version: "1".into(),
        generation: None,
        created: Some(0),
        labels: Box::new([]),
        cells: Box::new([
            Cell::Name,
            Cell::Text("Opaque".into()),
            Cell::Text("printed-value".into()),
        ]),
    });
    let snapshot = Arc::new(KindSnapshot {
        cluster: "test".into(),
        kind: KindId::new("", "secrets"),
        namespaced: true,
        columns,
        rows: vec![row],
        state: SyncState::Synced,
        version: 1,
        ..Default::default()
    });
    list.update(cx, |list, cx| list.set_snapshot(snapshot, cx));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("row-uid-creds").is_some()
    })
    .await;
    cx.run_until_parked();
    let texts = painted_texts(cx, window);
    assert!(texts.iter().any(|t| t == "creds"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "Opaque"), "{texts:?}");
    assert!(
        texts.iter().any(|t| t == crate::ui::object::MASK),
        "{texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("printed-value")),
        "{texts:?}"
    );
}

/// A Secret list with a masked `Token` column, in the order the tokens would not sort to.
fn secrets_with_tokens() -> Arc<KindSnapshot> {
    let rows = [("a", "z-value"), ("b", "m-value"), ("c", "a-value")]
        .into_iter()
        .map(|(name, token)| {
            Arc::new(Row {
                uid: format!("uid-{name}").into(),
                name: name.into(),
                namespace: Some("demo".into()),
                resource_version: "1".into(),
                generation: None,
                created: Some(0),
                labels: Box::new([]),
                cells: Box::new([Cell::Name, Cell::Text(token.into())]),
            })
        })
        .collect();
    Arc::new(KindSnapshot {
        cluster: "test".into(),
        kind: KindId::new("", "secrets"),
        namespaced: true,
        columns: vec![
            column("Name", ColumnRole::Name, ColumnType::String, 0),
            column("Token", ColumnRole::Value, ColumnType::String, 0),
        ]
        .into(),
        rows,
        state: SyncState::Synced,
        version: 1,
        ..Default::default()
    })
}

// covers UP-21: a masked column cannot be sorted, so no arrow claims an order the rows lack.
#[gpui_kit::test]
async fn a_masked_column_header_does_not_sort(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open_kind(cx, core, "secrets", "Secrets", 900.);
    list.update(cx, |list, cx| list.set_snapshot(secrets_with_tokens(), cx));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("sort-Token").is_some()
    })
    .await;
    cx.update_window(window, |_, window, cx| window.click("sort-Token", cx))
        .unwrap();
    cx.run_until_parked();
    list.read_with(cx, |list, _| {
        assert_eq!(list.query.sort, None);
        assert!(list.view_state().sort.is_none());
    });
    // The palette and automation refuse it up front instead of reporting a no-op.
    list.read_with(cx, |list, _| {
        let sort = |column: &str, key: Option<&str>| crate::ui::registry::SortBy {
            column: column.into(),
            key: key.map(str::to_owned),
        };
        assert!(list.check_sort(&sort("Token", None)).is_err());
        assert!(list.check_sort(&sort("", Some("server:Token"))).is_err());
        assert!(list.check_sort(&sort("Name", None)).is_ok());
        let offered = list.drawn_column_names();
        assert!(offered.iter().any(|c| c == "Name"), "{offered:?}");
        assert!(!offered.iter().any(|c| c == "Token"), "{offered:?}");
    });
    // A shown column still sorts.
    cx.update_window(window, |_, window, cx| window.click("sort-Name", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(list.read_with(cx, |list, _| list.view_state().sort.is_some()));
}

// covers UP-21: a sort remembered on a masked column stays suspended, without an arrow.
#[gpui_kit::test]
async fn a_remembered_sort_on_a_masked_column_is_suspended(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open_kind(cx, core, "secrets", "Secrets", 900.);
    list.update(cx, |list, cx| {
        list.query.sort = Some(super::SortSpec {
            column: "server:Token".into(),
            descending: false,
        });
        list.set_snapshot(secrets_with_tokens(), cx)
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("row-uid-a").is_some()
    })
    .await;
    cx.run_until_parked();
    let state = list.read_with(cx, |list, _| list.view_state());
    assert!(state.sort.is_none());
    let names: Vec<_> = state.rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
}

// covers UP-20: a column is not safe because of its header alone.
#[gpui_kit::test]
async fn a_repeated_safe_header_is_masked_in_a_secret_list(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open_kind(cx, core, "secrets", "Secrets", 1600.);
    let columns: Arc<[Column]> = vec![
        column("Name", ColumnRole::Name, ColumnType::String, 0),
        column("Namespace", ColumnRole::Namespace, ColumnType::String, 0),
        column("Type", ColumnRole::Value, ColumnType::String, 0),
        column("Data", ColumnRole::Value, ColumnType::Integer, 0),
        column("Age", ColumnRole::Age, ColumnType::Date, 0),
        column("Name", ColumnRole::Value, ColumnType::String, 0),
        column("Namespace", ColumnRole::Value, ColumnType::String, 0),
        column("Type", ColumnRole::Value, ColumnType::String, 0),
        column("Data", ColumnRole::Value, ColumnType::String, 0),
    ]
    .into();
    let row = Arc::new(Row {
        uid: "uid-creds".into(),
        name: "creds".into(),
        namespace: Some("demo".into()),
        resource_version: "1".into(),
        generation: None,
        created: Some(0),
        labels: Box::new([]),
        cells: Box::new([
            Cell::Name,
            Cell::Namespace,
            Cell::Text("Opaque".into()),
            Cell::Int(1),
            Cell::Age,
            Cell::Text("raw-name".into()),
            Cell::Text("raw-namespace".into()),
            Cell::Text("raw-type".into()),
            Cell::Text("raw-data".into()),
        ]),
    });
    let snapshot = Arc::new(KindSnapshot {
        cluster: "test".into(),
        kind: KindId::new("", "secrets"),
        namespaced: true,
        columns,
        rows: vec![row],
        state: SyncState::Synced,
        version: 1,
        ..Default::default()
    });
    list.update(cx, |list, cx| list.set_snapshot(snapshot, cx));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("row-uid-creds").is_some()
    })
    .await;
    cx.run_until_parked();
    let texts = painted_texts(cx, window);
    assert!(!texts.iter().any(|t| t.starts_with("raw-")), "{texts:?}");
    assert!(texts.iter().any(|t| t == "creds"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "demo"), "{texts:?}");
    // All nine columns are drawn: both copies of Type and Data, and the extra Name and
    // Namespace, each with the mask.
    let drawn = list.read_with(cx, |list, _| list.drawn.len());
    assert_eq!(drawn, 9);
    let masks = texts
        .iter()
        .filter(|t| *t == crate::ui::object::MASK)
        .count();
    assert_eq!(masks, 6, "{texts:?}");
}

// covers UP-23: a cell that is cut offers its full text in a tooltip; one that fits has none.
#[gpui_kit::test]
async fn a_cut_cell_has_a_tooltip_and_a_fitting_cell_has_none(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let name = "podinfo-frontend-canary-7b9c8d5f4-abcde";
    let row = format!("uid-{name}");
    let (window, list) = open_sized(cx, core, "Pods", 360.);
    list.update(cx, |list, cx| {
        list.set_snapshot(pods(&[("demo", name, 0)], SyncState::Synced, 1), cx)
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("sort-Restarts").is_some()
    })
    .await;
    cx.run_until_parked();
    let painted = painted_texts(cx, window);
    assert!(!painted.iter().any(|t| t == name), "not cut: {painted:?}");
    let fitting = hover_cell(cx, window, &row, "Restarts");
    assert!(
        fitting.is_empty(),
        "a fitting cell has a tooltip: {fitting:?}"
    );
    let cut = hover_cell(cx, window, &row, "Name");
    assert_eq!(cut.last().map(String::as_str), Some(name), "{cut:?}");
}

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
    // UP-21: counted out of the 3 rows in the selected namespaces, not the 4 in the cluster,
    // which the tooltip adds.
    let count = list.read_with(cx, |list, _| list.row_count());
    assert_eq!(count.text, "1 of 3");
    assert_eq!(
        count.tip,
        "1 match the filter\n3 in 2 namespaces\n4 in all namespaces"
    );
    assert_eq!(
        list.read_with(cx, |list, _| list.view_state().count),
        "1 of 3"
    );

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

// covers UP-23 (selection opens the detail); also the list's part of UP-33 (arrows)
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

// The registry drives the list by name, through the handlers its keys and clicks use.
#[gpui_kit::test]
async fn registered_actions_drive_the_list(cx: &mut TestAppContext) {
    use crate::ui::registry::{self, ActionError};
    use serde_json::json;

    let (core, _dir) = core();
    let (window, list) = open(cx, core);
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &[("demo", "api", 0), ("demo", "web", 2), ("data", "db", 1)],
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
    let focus = list.read_with(cx, |list, _| list.focus_handle().clone());
    let dispatch = |cx: &mut TestAppContext, name: &str, args: serde_json::Value| {
        cx.update_window(window, |_, window, cx| {
            window.focus(&focus, cx);
            registry::dispatch(name, args, window, cx)
        })
        .unwrap()
    };
    let selected = |cx: &mut TestAppContext| {
        list.read_with(cx, |list, _| list.selected.as_ref().map(|s| s.name.clone()))
    };

    dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    cx.run_until_parked();
    assert_eq!(selected(cx).as_deref(), Some("web"));

    dispatch(cx, "resource_list::SelectFirst", serde_json::Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(selected(cx).as_deref(), Some("db"));

    // Sorting by a column, as a header click does: ascending restarts.
    dispatch(cx, "resource_list::SortBy", json!({"column": "Restarts"})).unwrap();
    let expect = ["uid-api", "uid-db", "uid-web"];
    cx.wait_for(window, WAIT, move |window, _| {
        visible_rows(window, &expect) == expect
    })
    .await;

    dispatch(cx, "resource_list::ToggleWide", serde_json::Value::Null).unwrap();
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("sort-Node").is_some()
    })
    .await;

    dispatch(cx, "resource_list::CloseDetail", serde_json::Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(selected(cx), None);

    // Nothing in this window handles the detail pane's actions, and bad input is refused.
    assert_eq!(
        dispatch(cx, "detail::Close", serde_json::Value::Null),
        Err(ActionError::NotAvailable("detail::Close".into()))
    );
    assert!(matches!(
        dispatch(cx, "resource_list::SelectRow", json!({"row": 1})),
        Err(ActionError::InvalidArgs { .. })
    ));
    assert_eq!(
        dispatch(cx, "nope::Nothing", serde_json::Value::Null),
        Err(ActionError::Unknown("nope::Nothing".into()))
    );
}

// The list's part of the automation `state`: rows as drawn, in order, with the selection.
#[gpui_kit::test]
async fn view_state_reports_the_rows_as_drawn(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open(cx, core);
    let state = list.read_with(cx, |list, _| list.view_state());
    assert_eq!(state.placeholder, Some("loading"));
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &[("demo", "api", 0), ("demo", "web", 2)],
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
    list.update(cx, |list, cx| {
        list.select_when_listed(Some("demo".into()), "web".into(), cx)
    });
    cx.run_until_parked();
    let state = list.read_with(cx, |list, _| list.view_state());
    assert_eq!(state.sync, "live");
    assert_eq!((state.total, state.shown), (2, 2));
    assert_eq!(state.placeholder, None);
    assert_eq!(state.columns, ["Name", "Namespace", "Restarts"]);
    let rows: Vec<(&str, bool, &str)> = state
        .rows
        .iter()
        .map(|r| (r.name.as_str(), r.selected, r.cells[2].as_str()))
        .collect();
    assert_eq!(rows, [("api", false, "0"), ("web", true, "2")]);
    let row = |name: &str| crate::ui::registry::SelectRow {
        namespace: Some("demo".into()),
        name: name.into(),
    };
    assert!(list.read_with(cx, |list, _| list.check_select_row(&row("api")).is_ok()));
    assert!(list.read_with(cx, |list, _| list.check_select_row(&row("nope")).is_err()));
}

// A snapshot with other columns replaces the drawn ones at once: the automation state read
// before the next frame names the new columns (it named the old ones, or panicked when the new
// snapshot had fewer).
#[gpui_kit::test]
async fn new_columns_are_reported_before_the_next_frame(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open(cx, core);
    list.update(cx, |list, cx| {
        list.set_snapshot(pods(&[("demo", "api", 0)], SyncState::Synced, 1), cx)
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("row-uid-api").is_some()
    })
    .await;
    let mut fewer = (*pods(&[("demo", "api", 0)], SyncState::Synced, 2)).clone();
    fewer.columns = vec![
        column("Name", ColumnRole::Name, ColumnType::String, 0),
        column("Age", ColumnRole::Value, ColumnType::String, 0),
    ]
    .into();
    fewer.rows = vec![Arc::new(Row {
        cells: Box::new([Cell::Name, Cell::Text("1d".into())]),
        ..(*fewer.rows[0]).clone()
    })];
    let state = list.update(cx, |list, cx| {
        list.apply(Arc::new(fewer), vec![0].into(), None, None, cx);
        list.view_state()
    });
    assert_eq!(state.columns, ["Name", "Age"]);
    assert_eq!(state.rows[0].cells, ["api", "1d"]);
    // And back to more columns, which the stale indexes reported only in part.
    let state = list.update(cx, |list, cx| {
        let more = pods(&[("demo", "api", 3)], SyncState::Synced, 3);
        list.apply(more, vec![0].into(), None, None, cx);
        list.view_state()
    });
    assert_eq!(state.columns, ["Name", "Namespace", "Restarts"]);
    assert_eq!(state.rows[0].cells, ["api", "demo", "3"]);
}

// covers UP-21, UP-27: banners over the rows. A list the user may not read in every namespace
// says which namespaces it shows; a failed list that still has rows keeps them under a banner
// saying they may be out of date, until it syncs again.
#[gpui_kit::test]
async fn banners_say_what_the_rows_cover(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, list) = open(cx, core);
    let rows = [("demo", "web", 0), ("team-a", "api", 0)];
    let mut limited = (*pods(&rows, SyncState::Synced, 1)).clone();
    limited.limited_to = Some(Arc::new(flint_core::NamespaceLimit {
        listed: vec!["demo".into(), "team-a".into()],
        denied: Vec::new(),
        reason: "pods is forbidden".into(),
        revisions: vec![1, 1],
    }));
    list.update(cx, |list, cx| list.set_snapshot(Arc::new(limited), cx));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("list-limited").is_some()
            && window.try_find("row-uid-api").is_some()
            && window.try_find("list-banner").is_none()
    })
    .await;

    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(
                &rows,
                SyncState::Failed {
                    error: "connection refused".into(),
                },
                2,
            ),
            cx,
        )
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("list-banner").is_some()
            && window.try_find("row-uid-web").is_some()
            && window.try_find("list-limited").is_none()
    })
    .await;

    list.update(cx, |list, cx| {
        list.set_snapshot(pods(&rows, SyncState::Synced, 3), cx)
    });
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("list-banner").is_none() && window.try_find("row-uid-web").is_some()
    })
    .await;
}

// covers UP-21: in the main window, Home and End jump to the ends of the list, the arrows move
// the selection, Focus Filter's key moves focus to the text filter (where typing narrows the
// list), and the arrows leave the selection alone while typing there.
#[gpui_kit::test]
async fn keys_drive_the_list_in_the_main_window(cx: &mut TestAppContext) {
    use crate::ui::support_tests::open_workspace;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(
        cx,
        &[("demo", "api", 0), ("demo", "db", 0), ("demo", "web", 0)],
    )
    .await;
    t.press(cx, "end");
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    t.press(cx, "up");
    assert_eq!(t.selected(cx).as_deref(), Some("db"));
    t.press(cx, "home");
    assert_eq!(t.selected(cx).as_deref(), Some("api"));

    t.press_key_of(cx, "resource_list::FocusFilter");
    let filter_focused = cx
        .update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            window.find("input-state").focused()
        })
        .unwrap();
    assert_eq!(filter_focused, Some(true));
    t.type_text(cx, "we");
    let list = t.list(cx).unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| list.read(cx).shown().1 == 1)
        .await;
    t.press(cx, "down");
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
}

// covers UP-21 (#406): with nothing selected, Down from the filter selects the first row left
// by the filter and opens it. Esc and Enter only move focus back to the rows.
#[gpui_kit::test]
async fn down_from_the_filter_selects_the_first_row(cx: &mut TestAppContext) {
    use crate::ui::support_tests::open_workspace;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(
        cx,
        &[
            ("demo", "api", 0),
            ("demo", "web-a", 0),
            ("demo", "web-b", 0),
        ],
    )
    .await;
    assert_eq!(t.selected(cx), None);

    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "web");
    let list = t.list(cx).unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| list.read(cx).shown().1 == 2)
        .await;
    for key in ["escape", "enter"] {
        t.press(cx, key);
        assert_eq!(t.selected(cx), None, "{key} selects nothing");
        t.press_key_of(cx, "resource_list::FocusFilter");
    }

    t.press(cx, "down");
    assert_eq!(t.selected(cx).as_deref(), Some("web-a"));
    assert!(t.detail(cx).is_some(), "the row opened");
    let filter_focused = cx
        .update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            window.find("input-state").focused()
        })
        .unwrap();
    assert_ne!(filter_focused, Some(true), "focus left the filter");
}

// covers UP-21 (#406): Down while the filter's rows are still being computed waits for them, so
// it never opens a row the filter hides. Going back to the filter meanwhile cancels it.
#[gpui_kit::test]
async fn down_from_the_filter_waits_for_its_rows(cx: &mut TestAppContext) {
    use crate::ui::support_tests::open_workspace;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(
        cx,
        &[
            ("demo", "api", 0),
            ("demo", "web-a", 0),
            ("demo", "web-b", 0),
        ],
    )
    .await;
    let list = t.list(cx).unwrap();
    t.press_key_of(cx, "resource_list::FocusFilter");

    let permits = list.update(cx, |list, _| list.hold_computes());
    t.type_text(cx, "web-b");
    t.press(cx, "down");
    assert_eq!(t.selected(cx), None, "the old rows are not selected");
    t.press_key_of(cx, "resource_list::FocusFilter");
    for _ in 0..8 {
        permits.try_send(()).unwrap();
    }
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 1)
        .await;
    cx.run_until_parked();
    assert_eq!(t.selected(cx), None, "back in the filter, it was cancelled");

    let permits = list.update(cx, |list, _| list.hold_computes());
    t.press(cx, "backspace");
    t.press(cx, "down");
    assert_eq!(t.selected(cx), None, "still computing");
    for _ in 0..8 {
        permits.try_send(()).unwrap();
    }
    cx.wait_for(t.window, WAIT, move |_, cx| {
        list.read(cx).selected_name().as_deref() == Some("web-a")
    })
    .await;
    assert!(t.detail(cx).is_some(), "the row opened");
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

// A closed list is freed, with its filter: when another kind replaces it (focus moves to the
// new list), and when a cluster switch closes it (the workspace takes focus back). Each list is
// closed with its filter focused, so the window holds the filter as its focused text field
// (gpui-component's `WindowState`, a strong handle) until asked after focus moved on. Nothing
// else the list registered (for the next frame, its tasks) keeps it alive either. With
// GPUI_LEAK_DETECTION=1 a failure lists the live handles.
//
// Not `assert_no_new_leaks`: each cluster switch also rebuilds the other cluster's screen (its
// Retry buttons' focus handles are new and alive), which is not the list's.
#[gpui_kit::test]
async fn a_closed_list_is_released(cx: &mut TestAppContext) {
    use crate::ui::support_tests::{
        assert_focused_input, open_workspace, pods, standard_sections, weaken,
    };

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    let filtering = |cx: &mut TestAppContext| {
        t.press_key_of(cx, "resource_list::FocusFilter");
        cx.run_until_parked();
        assert_focused_input(cx, t.window, "the filter");
        let list = t.list(cx).unwrap();
        let filter = list.read_with(cx, |l, _| l.filter.downgrade());
        (weaken(list), filter)
    };

    // Replaced by another kind.
    t.open_kind(cx, "pods", pods(&[("demo", "api", 0)])).await;
    let (pods_list, pods_filter) = filtering(cx);
    t.dispatch(
        cx,
        "workspace::OpenView",
        serde_json::json!({"kind": "services"}),
    )
    .unwrap();
    cx.run_until_parked();
    pods_list.assert_released();
    pods_filter.assert_released();

    // Closed by a cluster switch.
    let (services_list, services_filter) = filtering(cx);
    t.activate(cx, "beta").await;
    cx.run_until_parked();
    assert!(t.list(cx).is_none());
    services_list.assert_released();
    services_filter.assert_released();
}

// A list whose filter had focus when a dock tab took it, closed by a cluster switch before it
// draws again, is freed: no list opens on the new cluster (no saved view) and focus stays in
// the dock, so only the switch itself can let the window drop the field.
#[gpui_kit::test]
async fn a_list_closed_by_a_switch_from_the_dock_is_released(cx: &mut TestAppContext) {
    use crate::ui::log_view::LogRequest;
    use crate::ui::support_tests::{
        assert_focused_input, open_workspace, pods, standard_sections, weaken,
    };
    use flint_core::{LogSubject, LogTarget};

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.open_kind(cx, "pods", pods(&[("demo", "api", 0)])).await;
    t.press_key_of(cx, "resource_list::FocusFilter");
    cx.run_until_parked();
    assert_focused_input(cx, t.window, "the filter");
    let list = t.list(cx).unwrap();
    let filter = list.read_with(cx, |l, _| l.filter.downgrade());
    let list = weaken(list);

    // A log tab takes focus, and the cluster switches before the list draws again.
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            let request = LogRequest {
                cluster: "alpha".into(),
                subject: LogSubject::Pod(LogTarget {
                    namespace: "demo".into(),
                    pod: "api".into(),
                    container: None,
                }),
                previous: false,
            };
            ws.open_logs(request, true, window, cx);
        });
        // The core reports the switch (as after SelectCluster) in the same update, before
        // anything is drawn.
        t.model.update(cx, |m, cx| {
            m.feed(flint_core::Event::ActiveChanged(Some("beta".into())), cx)
        });
    })
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, crate::ui::support_tests::WAIT, move |_, cx| {
        workspace.read(cx).shown_cluster() == Some("beta")
    })
    .await;
    cx.run_until_parked();
    assert!(t.list(cx).is_none());
    list.assert_released();
    filter.assert_released();
}
