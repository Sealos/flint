use super::*;
use crate::view::compare_cells;

const NOW: i64 = 1_000;

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

fn name_col() -> Column {
    column("Name", ColumnRole::Name)
}

fn ns_col() -> Column {
    column("Namespace", ColumnRole::Namespace)
}

fn age_col() -> Column {
    column("Age", ColumnRole::Age)
}

fn value(name: &str) -> Column {
    column(name, ColumnRole::Value)
}

fn row(namespace: &str, name: &str, uid: &str, values: &[Cell]) -> Arc<Row> {
    let mut cells = vec![Cell::Name, Cell::Namespace];
    cells.extend_from_slice(values);
    Arc::new(Row {
        uid: uid.into(),
        name: name.into(),
        namespace: Some(namespace.into()),
        resource_version: "1".into(),
        generation: None,
        created: Some(100),
        labels: Box::new([]),
        cells: cells.into(),
    })
}

fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}

fn snapshot(cluster: &str, columns: Vec<Column>, mut rows: Vec<Arc<Row>>) -> Arc<KindSnapshot> {
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Arc::new(KindSnapshot {
        cluster: cluster.to_owned(),
        kind: KindId::new("", "pods"),
        namespaced: true,
        columns: columns.into(),
        rows,
        ..Default::default()
    })
}

fn member(snapshot: Arc<KindSnapshot>) -> CombinedMember {
    CombinedMember {
        display_name: snapshot.cluster.clone(),
        snapshot,
        tag: None,
    }
}

fn list(snapshots: Vec<Arc<KindSnapshot>>) -> CombinedList {
    CombinedList::new(snapshots.into_iter().map(member).collect()).unwrap()
}

/// Two clusters with one same-named pod, and one column each that the other lacks.
fn two_clusters() -> CombinedList {
    list(vec![
        snapshot(
            "prod",
            vec![name_col(), ns_col(), value("Status"), value("Node")],
            vec![
                row("a", "web", "uid-1", &[text("Running"), text("node-1")]),
                row("b", "db", "uid-2", &[text("Pending"), text("node-2")]),
            ],
        ),
        snapshot(
            "dev",
            vec![name_col(), ns_col(), value("Status"), value("IP")],
            vec![
                row("a", "web", "uid-1", &[text("Crash"), text("10.0.0.1")]),
                row("c", "cache", "uid-3", &[text("Running"), text("10.0.0.2")]),
            ],
        ),
    ])
}

fn key(name: &str) -> ColumnKey {
    ColumnKey {
        key: format!("server:{name}"),
        occurrence: 0,
    }
}

fn sorted(column: ColumnKey, descending: bool) -> CombinedQuery {
    CombinedQuery {
        sort: Some(CombinedSort { column, descending }),
        ..Default::default()
    }
}

fn labels(list: &CombinedList, rows: &[RowRef]) -> Vec<String> {
    rows.iter()
        .map(|&at| {
            format!(
                "{}/{}",
                list.members()[at.member()].cluster(),
                list.row_info(at).unwrap().name
            )
        })
        .collect()
}

fn names(list: &CombinedList) -> Vec<&str> {
    list.columns().iter().map(|c| &*c.name).collect()
}

#[test]
fn columns_are_the_union_by_name_after_the_cluster_column() {
    let list = two_clusters();
    assert_eq!(
        names(&list),
        ["Cluster", "Name", "Namespace", "Status", "IP", "Node"]
    );
    assert_eq!(list.columns()[CLUSTER_COLUMN].role, None);
}

#[test]
fn a_new_column_lands_after_its_neighbour_not_after_age() {
    let list = list(vec![
        snapshot("a", vec![name_col(), value("Status"), age_col()], vec![]),
        snapshot("b", vec![name_col(), value("Node"), age_col()], vec![]),
    ]);
    assert_eq!(names(&list), ["Cluster", "Name", "Status", "Node", "Age"]);
}

#[test]
fn a_server_column_named_cluster_stays_its_own_column() {
    let list = list(vec![snapshot(
        "a",
        vec![name_col(), ns_col(), value("Cluster")],
        vec![row("n", "x", "1", &[text("from-server")])],
    )]);
    assert_eq!(names(&list), ["Cluster", "Name", "Namespace", "Cluster"]);
    let view = list.compute(&CombinedQuery::default(), NOW);
    assert_eq!(
        list.cell(view.rows()[0], 3, NOW),
        CombinedCell::Value("from-server".into())
    );
    assert!(matches!(
        list.cell(view.rows()[0], CLUSTER_COLUMN, NOW),
        CombinedCell::Cluster { name: "a", .. }
    ));
}

#[test]
fn a_repeated_header_keeps_each_column_apart() {
    let list = list(vec![snapshot(
        "a",
        vec![name_col(), ns_col(), value("Ready"), value("Ready")],
        vec![row("n", "x", "1", &[text("first"), text("second")])],
    )]);
    let view = list.compute(&CombinedQuery::default(), NOW);
    assert_eq!(
        list.cell(view.rows()[0], 3, NOW),
        CombinedCell::Value("first".into())
    );
    assert_eq!(
        list.cell(view.rows()[0], 4, NOW),
        CombinedCell::Value("second".into())
    );
}

#[test]
fn members_that_disagree_widen_the_column() {
    let mut wide = value("Node");
    wide.priority = 1;
    let mut number = value("Node");
    number.ty = ColumnType::Integer;
    let list = list(vec![
        snapshot("a", vec![wide], vec![]),
        snapshot("b", vec![number], vec![]),
    ]);
    assert_eq!(list.columns()[1].priority, 0);
    assert_eq!(list.columns()[1].ty, ColumnType::Other);
}

#[test]
fn a_cluster_lacking_a_column_shows_a_blank_cell() {
    let list = two_clusters();
    let ip = list.column_index(&key("IP")).unwrap();
    let node = list.column_index(&key("Node")).unwrap();
    let view = list.compute(&CombinedQuery::default(), NOW);
    let of = |cluster: &str| {
        *view
            .rows()
            .iter()
            .find(|at| list.members()[at.member()].cluster() == cluster)
            .unwrap()
    };
    assert_eq!(list.cell(of("prod"), ip, NOW), CombinedCell::Blank);
    assert_eq!(list.cell(of("dev"), node, NOW), CombinedCell::Blank);
    assert_eq!(
        list.cell(of("dev"), ip, NOW),
        CombinedCell::Value("10.0.0.1".into())
    );
}

#[test]
fn the_cluster_cell_carries_the_tag_for_its_dot() {
    let list = CombinedList::new(vec![
        CombinedMember {
            snapshot: snapshot("a", vec![name_col()], vec![row("n", "x", "1", &[])]),
            display_name: "a".into(),
            tag: Some(ClusterTag::Red),
        },
        member(snapshot(
            "b",
            vec![name_col()],
            vec![row("n", "y", "2", &[])],
        )),
    ])
    .unwrap();
    let view = list.compute(&CombinedQuery::default(), NOW);
    assert_eq!(
        list.cell(view.rows()[0], CLUSTER_COLUMN, NOW),
        CombinedCell::Cluster {
            name: "a",
            tag: Some(ClusterTag::Red)
        }
    );
    assert_eq!(
        list.cell(view.rows()[1], CLUSTER_COLUMN, NOW),
        CombinedCell::Cluster {
            name: "b",
            tag: None
        }
    );
}

#[test]
fn same_named_objects_in_two_clusters_have_different_ids() {
    let list = two_clusters();
    let view = list.compute(&CombinedQuery::default(), NOW);
    let webs: Vec<_> = view
        .rows()
        .iter()
        .filter(|&&at| list.row_info(at).unwrap().name == "web")
        .map(|&at| list.row_id(at).unwrap())
        .collect();
    assert_eq!(webs.len(), 2);
    assert_eq!(webs[0].uid, webs[1].uid, "the fixture shares a uid");
    assert_ne!(webs[0], webs[1]);
    let clusters: HashSet<_> = webs.iter().map(|id| &*id.cluster).collect();
    assert_eq!(clusters, HashSet::from(["prod", "dev"]));
}

#[test]
fn the_default_order_interleaves_by_namespace_then_name_then_cluster() {
    let list = two_clusters();
    let view = list.compute(&CombinedQuery::default(), NOW);
    assert_eq!(
        labels(&list, view.rows()),
        ["dev/web", "prod/web", "prod/db", "dev/cache"]
    );
}

#[test]
fn member_order_does_not_change_the_result() {
    let a = snapshot("a", vec![name_col()], vec![row("n", "x", "1", &[])]);
    let b = snapshot("b", vec![value("Other")], vec![row("n", "x", "2", &[])]);
    let forward = list(vec![a.clone(), b.clone()]);
    let backward = list(vec![b, a]);
    assert_eq!(names(&forward), names(&backward));
    assert_eq!(
        labels(
            &forward,
            forward.compute(&CombinedQuery::default(), NOW).rows()
        ),
        labels(
            &backward,
            backward.compute(&CombinedQuery::default(), NOW).rows()
        ),
    );
}

#[test]
fn sorting_on_the_cluster_column_orders_by_cluster() {
    let list = two_clusters();
    let cluster = list.columns()[CLUSTER_COLUMN].key.clone();
    let up = list.compute(&sorted(cluster.clone(), false), NOW);
    assert_eq!(
        labels(&list, up.rows()),
        ["dev/web", "dev/cache", "prod/web", "prod/db"]
    );
    let down = list.compute(&sorted(cluster, true), NOW);
    assert_eq!(
        labels(&list, down.rows()),
        ["prod/web", "prod/db", "dev/web", "dev/cache"]
    );
}

#[test]
fn a_shared_column_sorts_across_clusters() {
    let list = two_clusters();
    let view = list.compute(&sorted(key("Status"), false), NOW);
    assert_eq!(
        labels(&list, view.rows()),
        ["dev/web", "prod/db", "prod/web", "dev/cache"]
    );
}

#[test]
fn blanks_sort_last_in_both_directions() {
    let list = two_clusters();
    for descending in [false, true] {
        let view = list.compute(&sorted(key("IP"), descending), NOW);
        let got = labels(&list, view.rows());
        assert_eq!(
            &got[2..],
            ["prod/web", "prod/db"],
            "descending={descending}"
        );
    }
}

#[test]
fn sort_keys_order_like_a_single_cluster_list() {
    let cells = [
        Cell::Null,
        Cell::Bool(true),
        Cell::Int(9),
        Cell::Int(10),
        Cell::Float(9.5),
        Cell::Time(5),
        text(""),
        text("<none>"),
        text("pod-9"),
        text("pod-10"),
    ];
    let columns = vec![name_col(), ns_col(), value("V")];
    let rows: Vec<_> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            row(
                "n",
                &format!("r{i:02}"),
                &format!("{i}"),
                std::slice::from_ref(c),
            )
        })
        .collect();
    let single = snapshot("a", columns, rows);
    let list = list(vec![single.clone()]);
    let at = |row: usize| RowRef {
        member: 0,
        row: row as u32,
    };
    let column = list.column_index(&key("V")).unwrap();
    for a in 0..cells.len() {
        for b in 0..cells.len() {
            let ours = list
                .sort_key(at(a), column, NOW)
                .compare(&list.sort_key(at(b), column, NOW), false);
            let theirs = compare_cells(&single.rows[a], &single.rows[b], 2, NOW);
            assert_eq!(ours, theirs, "{:?} vs {:?}", cells[a], cells[b]);
        }
    }
}

#[test]
fn a_sort_on_a_column_no_longer_there_keeps_the_default_order() {
    let list = two_clusters();
    let view = list.compute(&sorted(key("Gone"), false), NOW);
    let default = list.compute(&CombinedQuery::default(), NOW);
    assert_eq!(view.rows(), default.rows());
}

#[test]
fn the_text_filter_matches_the_cluster_name() {
    let list = two_clusters();
    let query = CombinedQuery {
        text: " PRO ".into(),
        ..Default::default()
    };
    let view = list.compute(&query, NOW);
    assert_eq!(labels(&list, view.rows()), ["prod/web", "prod/db"]);
}

#[test]
fn the_text_filter_matches_cells_in_any_cluster() {
    let list = two_clusters();
    let query = CombinedQuery {
        text: "running".into(),
        ..Default::default()
    };
    let view = list.compute(&query, NOW);
    assert_eq!(labels(&list, view.rows()), ["prod/web", "dev/cache"]);
}

#[test]
fn the_cluster_and_namespace_filters_narrow_the_rows() {
    let list = two_clusters();
    let query = CombinedQuery {
        clusters: vec!["dev".into()],
        namespaces: vec!["a".into()],
        ..Default::default()
    };
    let view = list.compute(&query, NOW);
    assert_eq!(labels(&list, view.rows()), ["dev/web"]);
}

#[test]
fn grouping_gives_one_section_per_cluster_with_counts() {
    let list = two_clusters();
    let query = CombinedQuery {
        group_by_cluster: true,
        ..Default::default()
    };
    let view = list.compute(&query, NOW);
    let groups: Vec<_> = view
        .groups()
        .iter()
        .map(|g| (list.members()[g.member].cluster(), g.count))
        .collect();
    assert_eq!(groups, [("dev", 2), ("prod", 2)]);
    assert_eq!(view.line_count(), 6);
    let lines: Vec<String> = (0..view.line_count())
        .map(|i| match view.line(i).unwrap() {
            CombinedLine::Header(g) => format!("# {}", list.members()[g.member].cluster()),
            CombinedLine::Row(at) => labels(&list, &[at]).remove(0),
        })
        .collect();
    assert_eq!(
        lines,
        [
            "# dev",
            "dev/web",
            "dev/cache",
            "# prod",
            "prod/web",
            "prod/db"
        ]
    );
    assert_eq!(view.line(6), None);
}

#[test]
fn a_collapsed_group_keeps_its_count_and_hides_its_rows() {
    let list = two_clusters();
    let query = CombinedQuery {
        group_by_cluster: true,
        collapsed: vec!["dev".into()],
        ..Default::default()
    };
    let view = list.compute(&query, NOW);
    assert_eq!(view.groups()[0].count, 2);
    assert!(view.groups()[0].collapsed);
    assert_eq!(view.matched(), 4);
    assert_eq!(labels(&list, view.rows()), ["prod/web", "prod/db"]);
    assert!(matches!(view.line(1), Some(CombinedLine::Header(_))));
    assert!(matches!(view.line(2), Some(CombinedLine::Row(_))));
}

#[test]
fn a_group_with_no_match_stays_visible() {
    let list = two_clusters();
    let query = CombinedQuery {
        text: "cache".into(),
        group_by_cluster: true,
        ..Default::default()
    };
    let view = list.compute(&query, NOW);
    let counts: Vec<_> = view.groups().iter().map(|g| g.count).collect();
    assert_eq!(counts, [1, 0]);
}

#[test]
fn a_descending_cluster_sort_reverses_the_groups() {
    let list = two_clusters();
    let mut query = sorted(list.columns()[CLUSTER_COLUMN].key.clone(), true);
    query.group_by_cluster = true;
    let view = list.compute(&query, NOW);
    let order: Vec<_> = view
        .groups()
        .iter()
        .map(|g| list.members()[g.member].cluster())
        .collect();
    assert_eq!(order, ["prod", "dev"]);
    assert_eq!(view.line(1), Some(CombinedLine::Row(view.rows()[0])));
}

fn secrets(cluster: &str, token: &str) -> Arc<KindSnapshot> {
    let base = snapshot(
        cluster,
        vec![name_col(), ns_col(), value("Token")],
        vec![row("n", "s", "1", &[text(token)])],
    );
    Arc::new(KindSnapshot {
        kind: KindId::new("", "secrets"),
        ..(*base).clone()
    })
}

#[test]
fn a_secret_column_never_shows_filters_or_sorts() {
    let list = list(vec![secrets("a", "zzz-hidden"), secrets("b", "aaa-hidden")]);
    let token = list.column_index(&key("Token")).unwrap();
    let default = list.compute(&CombinedQuery::default(), NOW);
    for &at in default.rows() {
        assert_eq!(list.cell(at, token, NOW), CombinedCell::Masked);
    }
    let hit = list.compute(
        &CombinedQuery {
            text: "hidden".into(),
            ..Default::default()
        },
        NOW,
    );
    assert_eq!(hit.matched(), 0);
    let by_token = list.compute(&sorted(key("Token"), false), NOW);
    assert_eq!(by_token.rows(), default.rows());
}

#[test]
fn debug_output_and_row_info_never_hold_a_secret_cell() {
    let snapshot = secrets("a", "raw-token-value");
    let input = member(snapshot);
    assert!(!format!("{input:?}").contains("raw-token-value"));
    let list = CombinedList::new(vec![input]).unwrap();
    assert!(!format!("{list:?}").contains("raw-token-value"));
    let view = list.compute(&CombinedQuery::default(), NOW);
    let info = list.row_info(view.rows()[0]).unwrap();
    assert_eq!(info.name, "s");
    assert!(!format!("{info:?}").contains("raw-token-value"));
    assert_eq!(list.members()[0].row_count(), 1);
    assert_eq!(list.members()[0].state(), &SyncState::Idle);
}

fn named(id: &str, display_name: &str, row_name: &str) -> CombinedMember {
    CombinedMember {
        snapshot: snapshot(
            id,
            vec![name_col(), ns_col()],
            vec![row("n", row_name, "same-uid", &[])],
        ),
        display_name: display_name.into(),
        tag: None,
    }
}

#[test]
fn the_cluster_column_uses_the_display_name_and_filters_use_the_id() {
    let list = CombinedList::new(vec![
        named("ctx-zzz", "alpha", "one"),
        named("ctx-aaa", "beta", "two"),
    ])
    .unwrap();
    assert_eq!(list.members()[0].cluster(), "ctx-zzz");
    assert_eq!(list.members()[0].display_name(), "alpha");

    let cluster = list.columns()[CLUSTER_COLUMN].key.clone();
    let view = list.compute(&sorted(cluster, false), NOW);
    assert_eq!(labels(&list, view.rows()), ["ctx-zzz/one", "ctx-aaa/two"]);
    assert!(matches!(
        list.cell(view.rows()[0], CLUSTER_COLUMN, NOW),
        CombinedCell::Cluster { name: "alpha", .. }
    ));
    assert_eq!(&*list.row_id(view.rows()[0]).unwrap().cluster, "ctx-zzz");

    let search = |text: &str| {
        let query = CombinedQuery {
            text: text.into(),
            ..Default::default()
        };
        labels(&list, list.compute(&query, NOW).rows())
    };
    assert_eq!(search("BETA"), ["ctx-aaa/two"]);
    assert!(search("ctx-").is_empty(), "the id is not on screen");

    let by_id = CombinedQuery {
        clusters: vec!["ctx-aaa".into()],
        ..Default::default()
    };
    assert_eq!(
        labels(&list, list.compute(&by_id, NOW).rows()),
        ["ctx-aaa/two"]
    );
    let by_name = CombinedQuery {
        clusters: vec!["beta".into()],
        ..Default::default()
    };
    assert_eq!(list.compute(&by_name, NOW).matched(), 0);
}

#[test]
fn two_clusters_with_one_display_name_stay_apart() {
    let list =
        CombinedList::new(vec![named("id-b", "same", "x"), named("id-a", "same", "x")]).unwrap();
    assert_eq!(list.members()[0].cluster(), "id-a");
    let view = list.compute(&CombinedQuery::default(), NOW);
    assert_ne!(
        list.row_id(view.rows()[0]).unwrap(),
        list.row_id(view.rows()[1]).unwrap()
    );
}

#[test]
fn large_integers_sort_exactly() {
    let list = list(vec![snapshot(
        "a",
        vec![name_col(), ns_col(), value("V")],
        vec![
            row("n", "a-high", "1", &[Cell::Int(9_007_199_254_740_993)]),
            row("n", "b-low", "2", &[Cell::Int(9_007_199_254_740_992)]),
        ],
    )]);
    let view = list.compute(&sorted(key("V"), false), NOW);
    assert_eq!(labels(&list, view.rows()), ["a/b-low", "a/a-high"]);
    let view = list.compute(&sorted(key("V"), true), NOW);
    assert_eq!(labels(&list, view.rows()), ["a/a-high", "a/b-low"]);
}

#[test]
fn integers_and_floats_compare_exactly() {
    use Ordering::{Equal, Greater, Less};
    let big = 9_007_199_254_740_992_i64;
    for (int, float, expected) in [
        (9, 9.5, Less),
        (10, 9.5, Greater),
        (9, 9.0, Equal),
        (-1, -0.5, Less),
        (0, -0.5, Greater),
        (-2, -1.5, Less),
        (0, 0.0, Equal),
        (0, -0.0, Greater),
        (big + 1, big as f64, Greater),
        (big, big as f64, Equal),
        (i64::MAX, 9_223_372_036_854_775_808.0, Less),
        (i64::MIN, -9_223_372_036_854_775_808.0, Equal),
        (i64::MIN, -1e19, Greater),
        (0, f64::INFINITY, Less),
        (0, f64::NEG_INFINITY, Greater),
        (i64::MAX, f64::NAN, Less),
        (i64::MIN, -f64::NAN, Greater),
    ] {
        assert_eq!(
            compare_int_float(int, float),
            expected,
            "{int} vs {float:?}"
        );
        assert_eq!(
            SortKey::Float(float).compare(&SortKey::Int(int), false),
            expected.reverse(),
            "{float:?} vs {int}"
        );
    }
}

#[test]
fn mixed_kinds_and_repeated_clusters_are_refused() {
    let pods = snapshot("a", vec![], vec![]);
    assert!(matches!(
        CombinedList::new(vec![member(pods.clone()), member(secrets("b", "x"))]),
        Err(CombineError::MixedKinds { .. })
    ));
    assert_eq!(
        CombinedList::new(vec![member(pods.clone()), member(pods)]).unwrap_err(),
        CombineError::DuplicateCluster("a".into())
    );
}

#[test]
fn no_members_is_an_empty_list() {
    let list = list(vec![]);
    assert_eq!(names(&list), ["Cluster"]);
    let view = list.compute(&CombinedQuery::default(), NOW);
    assert_eq!(view.line_count(), 0);
    assert_eq!(view.line(0), None);
}

#[test]
fn large_lists_share_their_rows() {
    let rows: Vec<_> = (0..20_000)
        .map(|i| {
            row(
                "n",
                &format!("pod-{i:05}"),
                &format!("{i}"),
                &[Cell::Int(i)],
            )
        })
        .collect();
    let first = rows[0].clone();
    let columns = vec![name_col(), ns_col(), value("Restarts")];
    let members: Vec<_> = (0..5)
        .map(|c| snapshot(&format!("c{c}"), columns.clone(), rows.clone()))
        .collect();
    let before = Arc::strong_count(&first);
    let list = list(members);
    let view = list.compute(&sorted(key("Restarts"), true), NOW);
    assert_eq!(view.rows().len(), 100_000);
    assert_eq!(Arc::strong_count(&first), before);
    assert_eq!(std::mem::size_of::<RowRef>(), 8);
}
