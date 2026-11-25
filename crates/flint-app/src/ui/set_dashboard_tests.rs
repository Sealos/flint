//! The set dashboard: worst-first order, honest totals, and links bound to the member's feed.

use std::sync::Arc;

use flint_core::cluster::SafeErrorText;
use flint_core::dashboard::{ProblemCategory, ProblemCounts};
use flint_core::metrics::{Cpu, Memory, MetricsError, MetricsList, NodeMetrics, Usage};
use flint_core::metrics_trend::TrendStore;
use flint_core::telemetry::{Availability, ClusterTelemetry, MetricsCheck};
use flint_core::thresholds::{ThresholdLevel, Thresholds};
use flint_core::{KindId, KindSnapshot, SyncState};
<<<<<<< HEAD
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowOptions};
=======
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
>>>>>>> origin/master
use jiff::Timestamp;
use serde_json::{Value, json};

use super::super::super::list::{CountedObjects, ObjectFilter};
use super::super::super::support_tests::{TestWorkspace, WAIT, objects, open_workspace};
use super::super::feed::{FeedId, ProblemList, ProblemLists};
use super::super::{METRICS_WAITING, NO_VALUE, ProblemTile, category_label};
use super::{
    CardState, MemberRead, SetCard, SetDashboardModel, SetDashboardView, TestFeeds, TotalTile,
    assemble, build, card, kind_entry, total_kind,
};

fn tiles(count: impl Fn(ProblemCategory) -> Option<u32>) -> Vec<ProblemTile> {
    ProblemCategory::ALL
        .into_iter()
        .map(|category| ProblemTile {
            category,
            label: category_label(category),
            count: count(category),
            level: ThresholdLevel::Healthy,
        })
        .collect()
}

fn card_with(cluster: &str, state: CardState, failing: Option<u32>, feed: &str) -> SetCard {
    let counting = state.counts();
    let count = move |category| {
        if !counting {
            None
        } else if category == ProblemCategory::FailingPods {
            failing
        } else {
            Some(0)
        }
    };
    SetCard {
        cluster: cluster.to_owned(),
        shown: cluster.to_owned(),
        tag: None,
        tiles: tiles(count),
        counts: ProblemCounts::from_fn(count),
        level: ThresholdLevel::Healthy,
        cpu: None,
        memory: None,
        metrics_notice: None,
        short_kinds: vec!["deployments"],
        feed: counting.then(|| FeedId::for_test(feed)),
        state,
    }
}

fn order(model: &SetDashboardModel) -> Vec<&str> {
    model.cards.iter().map(|c| c.cluster.as_str()).collect()
}

fn total(model: &SetDashboardModel, category: ProblemCategory) -> &TotalTile {
    model
        .totals
        .iter()
        .find(|t| t.category == category)
        .expect("every category has a total")
}

#[test]
fn cards_run_worst_first_and_missing_data_is_never_healthiest() {
    let model = assemble(vec![
        card_with("healthy", CardState::Live, Some(0), "a"),
        card_with("paused", CardState::Blocked("Paused".into()), None, "b"),
        card_with("broken", CardState::Live, Some(4), "c"),
    ]);
    assert_eq!(order(&model), ["broken", "paused", "healthy"]);
}

#[test]
fn equal_cards_keep_a_stable_order_by_name() {
    let model = assemble(vec![
        card_with("b", CardState::Live, Some(1), "b"),
        card_with("a", CardState::Live, Some(1), "a"),
    ]);
    assert_eq!(order(&model), ["a", "b"]);
}

#[test]
fn a_total_lacking_a_member_says_it_is_partial() {
    for state in [
        CardState::Blocked("Paused".into()),
        CardState::Blocked("Changed, left out of this set.".into()),
        CardState::Unavailable("This cluster cannot be checked.".into()),
        CardState::Waiting,
    ] {
        let model = assemble(vec![
            card_with("a", CardState::Live, Some(2), "a"),
            card_with("b", state, None, "b"),
        ]);
        let failing = total(&model, ProblemCategory::FailingPods);
        assert_eq!(failing.value(2), "2+");
        assert!(failing.description(2).contains("1 of 2 clusters"));
        assert!(model.partial.as_deref().unwrap().contains('b'));
    }
}

#[test]
fn a_full_total_is_exact_and_one_with_no_member_shows_no_number() {
    let full = assemble(vec![
        card_with("a", CardState::Live, Some(2), "a"),
        card_with("b", CardState::Live, Some(3), "b"),
    ]);
    assert_eq!(total(&full, ProblemCategory::FailingPods).value(2), "5");
    assert_eq!(full.partial, None);
    let none = assemble(vec![
        card_with("a", CardState::Blocked("Paused".into()), None, "a"),
        card_with("b", CardState::Waiting, None, "b"),
    ]);
    let failing = total(&none, ProblemCategory::FailingPods);
    assert_eq!(failing.value(2), NO_VALUE);
    assert!(!failing.opens);
}

#[test]
fn a_total_of_several_short_kinds_opens_no_single_list() {
    let short = |cluster: &str, kind: &'static str| {
        let count = |category| Some(u32::from(category == ProblemCategory::ShortWorkloads));
        SetCard {
            short_kinds: vec![kind],
            tiles: tiles(count),
            counts: ProblemCounts::from_fn(count),
            ..card_with(cluster, CardState::Live, Some(0), cluster)
        }
    };
    let mixed = assemble(vec![short("a", "deployments"), short("b", "daemonsets")]);
    assert!(!total(&mixed, ProblemCategory::ShortWorkloads).opens);
    let same = assemble(vec![short("a", "deployments"), short("b", "deployments")]);
    assert!(total(&same, ProblemCategory::ShortWorkloads).opens);
}

#[test]
fn a_blocked_member_shows_no_values_even_with_a_feed_on_the_board() {
    let member = MemberRead {
        cluster: "a".into(),
        shown: "a".into(),
        tag: None,
        blocked: Some("a now points at another server".into()),
        telemetry: None,
        server: "https://a".into(),
        thresholds: Thresholds::default(),
    };
    let card = card_with_read(member);
    assert!(matches!(card.state, CardState::Blocked(_)));
    assert!(card.tiles.iter().all(|tile| tile.count.is_none()));
    assert_eq!(card.feed, None);
    assert_eq!(card.level, ThresholdLevel::Unknown);
}

// In either watch mode a member's feed runs, so one not yet read is waiting for it (#715).
#[test]
fn a_member_without_a_feed_is_waiting_for_it() {
    let member = MemberRead {
        cluster: "a".into(),
        shown: "a".into(),
        tag: None,
        blocked: None,
        telemetry: None,
        server: "https://a".into(),
        thresholds: Thresholds::default(),
    };
    let card = card_with_read(member);
    assert_eq!(card.state, CardState::Waiting);
    assert_eq!(card.note().as_deref(), Some(METRICS_WAITING));
}

fn card_with_read(member: MemberRead) -> SetCard {
    card(member, Timestamp::now())
}

fn member_with_metrics(
    cluster: &str,
    check: MetricsCheck,
    sample: Option<(Timestamp, u64, u64)>,
) -> MemberRead {
    let mut telemetry =
        ClusterTelemetry::for_tests(Availability::Checked, Default::default(), false);
    let snapshot = Arc::make_mut(&mut telemetry.snapshot);
    snapshot.cluster = cluster.into();
    snapshot.metrics = check;
    let mut trends = TrendStore::new();
    if let Some((at, cpu_millis, memory_mib)) = sample {
        trends.record(
            cluster,
            at,
            &Ok(MetricsList {
                items: vec![NodeMetrics {
                    name: "node".into(),
                    timestamp: None,
                    window: None,
                    usage: Usage {
                        cpu: Cpu::from_nanos(cpu_millis * 1_000_000),
                        memory: Memory::from_bytes(memory_mib * 1024 * 1024),
                    },
                }],
                skipped: 0,
            }),
        );
    }
    MemberRead {
        cluster: cluster.into(),
        shown: cluster.into(),
        tag: None,
        blocked: None,
        telemetry: Some(telemetry.with_trends_for_tests(trends)),
        server: format!("https://{cluster}"),
        thresholds: Thresholds::default(),
    }
}

#[test]
fn cards_show_each_members_own_metrics() {
    let now = Timestamp::from_second(1_800_000_000).unwrap();
    let current = MetricsCheck::Current { nodes: 1 };
    let model = build(
        vec![
            member_with_metrics("active", current.clone(), Some((now, 250, 64))),
            member_with_metrics("member", current, Some((now, 750, 128))),
        ],
        now,
    );
    for (cluster, cpu, memory) in [("active", "250m", "64 MiB"), ("member", "750m", "128 MiB")] {
        let card = model.card(cluster).unwrap();
        assert_eq!(card.cpu.as_ref().unwrap().tile.value.as_deref(), Some(cpu));
        assert_eq!(
            card.memory.as_ref().unwrap().tile.value.as_deref(),
            Some(memory)
        );
        assert_eq!(card.note(), None);
    }
}

#[test]
fn cards_explain_when_cpu_and_memory_are_unavailable() {
    let now = Timestamp::from_second(1_800_000_000).unwrap();
    let old = Timestamp::from_second(now.as_second() - 120).unwrap();
    for (check, sample, reason) in [
        (
            MetricsCheck::Waiting,
            None,
            "Waiting for the first metrics sample",
        ),
        (
            MetricsCheck::Unavailable(MetricsError::NotInstalled),
            None,
            "No metrics-server installed",
        ),
        (
            MetricsCheck::Unavailable(MetricsError::Forbidden(SafeErrorText::reduced("denied"))),
            Some((now, 250, 64)),
            "no permission to read metrics",
        ),
        (
            MetricsCheck::Unavailable(MetricsError::Failed(SafeErrorText::reduced("timed out"))),
            Some((now, 250, 64)),
            "cannot be checked",
        ),
        (
            MetricsCheck::Current { nodes: 0 },
            Some((now, 250, 64)),
            "No node metrics were returned",
        ),
        (
            MetricsCheck::Current { nodes: 1 },
            None,
            "Waiting for fresh metrics from all nodes",
        ),
        (
            MetricsCheck::Current { nodes: 1 },
            Some((old, 250, 64)),
            "Waiting for fresh metrics from all nodes",
        ),
    ] {
        let card = card(member_with_metrics("member", check, sample), now);
        assert_eq!(card.state, CardState::Live);
        assert!(
            [card.cpu.as_ref(), card.memory.as_ref()]
                .into_iter()
                .flatten()
                .all(|metric| {
                    metric.tile.value.is_none() && metric.level == ThresholdLevel::Unknown
                })
        );
        assert!(
            card.description().contains(reason),
            "{}",
            card.description()
        );
    }
}

#[test]
fn every_problem_list_kind_has_an_entry() {
    for kind in [
        "pods",
        "nodes",
        "events",
        "persistentvolumeclaims",
        "jobs",
        "deployments",
        "statefulsets",
        "daemonsets",
    ] {
        assert!(kind_entry(kind).is_some(), "{kind}");
    }
    assert!(kind_entry("configmaps").is_none());
}

// View tests.

#[gpui_kit::test]
async fn draws_metrics_and_an_unavailable_reason_on_their_own_cards(cx: &mut TestAppContext) {
    let now = Timestamp::from_second(1_800_000_000).unwrap();
    let model = build(
        vec![
            member_with_metrics(
                "a",
                MetricsCheck::Current { nodes: 1 },
                Some((now, 250, 64)),
            ),
            member_with_metrics(
                "b",
                MetricsCheck::Unavailable(MetricsError::NotInstalled),
                None,
            ),
        ],
        now,
    );
    cx.update(gpui_kit::init);
    let (window, _) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|cx| {
                    let mut view = SetDashboardView::new(cx);
                    view.set_model(model, cx);
                    view
                })
            })
        })
        .unwrap();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        for (id, label) in [
            ("set-card-cpu-a", "CPU: 250m"),
            ("set-card-memory-a", "Memory: 64 MiB"),
            ("set-card-note-b", "No metrics-server installed."),
        ] {
            assert_eq!(window.try_find(id).unwrap().label(), Some(label));
        }
    })
    .unwrap();
}

const OPEN: &str = "cluster_sets::OpenClusterSetDashboard";
const OPEN_PROBLEM: &str = "cluster_sets::OpenSetProblem";
const OPEN_MEMBER: &str = "cluster_sets::OpenSetMember";

fn pods_of(cluster: &str, rows: &[(&str, &str, i64)]) -> Arc<KindSnapshot> {
    let mut snapshot = (*objects(KindId::pods(), rows, SyncState::Synced, 1)).clone();
    snapshot.cluster = cluster.into();
    Arc::new(snapshot)
}

fn failing_lists(uid: &str) -> ProblemLists {
    let mut counted = CountedObjects::default();
    counted.insert(Some("default"), "web", Some(uid));
    ProblemLists::from([(
        ProblemCategory::FailingPods,
        vec![ProblemList {
            kind: "pods",
            filter: ObjectFilter {
                label: "failing pods".to_owned(),
                objects: Arc::new(counted),
            },
        }],
    )])
}

/// A set of both test clusters with a pod `web` and a pod `api` in each, its dashboard open on
/// hand-fed values: `kind-beta` has one failing pod.
async fn open_set(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
) -> (TestWorkspace, Entity<SetDashboardView>) {
    let t = open_workspace(cx, dir).await;
    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    for cluster in ["kind-alpha", "kind-beta"] {
        t.dispatch(
            cx,
            "cluster_sets::TogglePickedCluster",
            json!({ "cluster": cluster }),
        )
        .unwrap();
    }
    t.dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "cluster-set-view").await;
    let (window, workspace) = (t.window, t.workspace.clone());
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            let rows = [("default", "web", 1), ("default", "api", 2)];
            let snapshots = ["kind-alpha", "kind-beta"].map(|cluster| pods_of(cluster, &rows));
            ws.feed_set_rows_for_test(snapshots.into(), window, cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
    t.dispatch(cx, OPEN, Value::Null).unwrap();
    let view = t
        .workspace
        .read_with(cx, |ws, _| ws.set_dashboard())
        .expect("the dashboard is open");
    let (alpha, beta) = (FeedId::for_test("alpha"), FeedId::for_test("beta"));
    view.update(cx, |view, cx| {
        view.test_feeds = Some(TestFeeds {
            feeds: [
                ("kind-alpha".to_owned(), alpha.clone()),
                ("kind-beta".to_owned(), beta.clone()),
            ]
            .into(),
            lists: [
                ("kind-alpha".to_owned(), failing_lists("no-such-uid")),
                ("kind-beta".to_owned(), failing_lists("uid-web")),
            ]
            .into(),
            ..TestFeeds::default()
        });
        let mut quiet = card_with("kind-alpha", CardState::Live, Some(0), "alpha");
        quiet.feed = Some(alpha);
        let mut failing = card_with("kind-beta", CardState::Live, Some(1), "beta");
        failing.feed = Some(beta);
        view.set_model(assemble(vec![quiet, failing]), cx);
    });
    cx.run_until_parked();
    (t, view)
}

fn shown_label(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    t.workspace.read_with(cx, |ws, cx| {
        let list = ws.set_list_for_test()?;
        list.read(cx).objects_label().map(str::to_owned)
    })
}

fn dashboard_open(cx: &mut TestAppContext, t: &TestWorkspace) -> bool {
    t.workspace
        .read_with(cx, |ws, _| ws.set_dashboard().is_some())
}

fn matched(cx: &mut TestAppContext, t: &TestWorkspace) -> usize {
    t.workspace.read_with(cx, |ws, cx| {
        ws.set_list_for_test()
            .map_or(0, |list| list.read(cx).view.matched())
    })
}

#[gpui_kit::test]
async fn a_count_opens_the_combined_list_of_exactly_what_it_counted(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, _view) = open_set(cx, dir.path()).await;
    t.dispatch(
        cx,
        OPEN_PROBLEM,
        json!({ "category": "failing_pods", "cluster": "kind-beta" }),
    )
    .unwrap();
    cx.run_until_parked();
    assert!(!dashboard_open(cx, &t));
    assert!(shown_label(cx, &t).unwrap().contains("kind-beta"));
    assert_eq!(matched(cx, &t), 1);
}

#[gpui_kit::test]
async fn a_card_opens_the_list_of_its_member(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, _view) = open_set(cx, dir.path()).await;
    t.dispatch(cx, OPEN_MEMBER, json!({ "cluster": "kind-beta" }))
        .unwrap();
    cx.run_until_parked();
    assert!(!dashboard_open(cx, &t));
    assert_eq!(matched(cx, &t), 2);
}

#[gpui_kit::test]
async fn a_card_of_a_replaced_feed_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, view) = open_set(cx, dir.path()).await;
    view.update(cx, |view, _| {
        let test = view.test_feeds.as_mut().unwrap();
        test.feeds
            .insert("kind-beta".to_owned(), FeedId::for_test("beta"));
    });
    for (action, args) in [
        (
            OPEN_PROBLEM,
            json!({ "category": "failing_pods", "cluster": "kind-beta" }),
        ),
        (OPEN_MEMBER, json!({ "cluster": "kind-beta" })),
        (OPEN_PROBLEM, json!({ "category": "failing_pods" })),
    ] {
        t.dispatch(cx, action, args).unwrap();
        cx.run_until_parked();
        assert!(dashboard_open(cx, &t), "{action} left the dashboard");
        assert_eq!(shown_label(cx, &t), None);
    }
}

#[gpui_kit::test]
async fn a_read_of_a_gone_feed_is_not_shown(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, view) = open_set(cx, dir.path()).await;
    let before = view.read_with(cx, |view, _| view.model.clone());
    let mut late = before.clone();
    late.cards[0].feed = Some(FeedId::for_test("older"));
    late.cards[0].shown = "from an older feed".to_owned();
    let (workspace, shown) = (t.workspace.clone(), view.clone());
    workspace.update(cx, |ws, cx| ws.finish_set_read(&shown, late, cx));
    assert_eq!(view.read_with(cx, |view, _| view.model.clone()), before);
}

/// `kind-beta`'s card and the failing total, read with no build having finished.
fn beta_and_total(cx: &mut TestAppContext, view: &Entity<SetDashboardView>) -> (bool, String) {
    view.read_with(cx, |view, _| {
        let card = view.model.card("kind-beta").unwrap();
        assert_eq!(card.feed, None);
        assert!(card.tiles.iter().all(|tile| tile.count.is_none()));
        assert!(view.model.partial.as_deref().unwrap().contains("kind-beta"));
        let failing = total(&view.model, ProblemCategory::FailingPods);
        (card.state.counts(), failing.value(2))
    })
}

#[gpui_kit::test]
async fn a_member_that_left_its_server_is_taken_down_during_a_build(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, view) = open_set(cx, dir.path()).await;
    view.update(cx, |view, _| view.reading = true);
    t.workspace
        .update(cx, |ws, cx| ws.discard_member_for_test("kind-beta", cx));
    assert_eq!(beta_and_total(cx, &view), (false, "0+".to_owned()));
    assert!(view.read_with(cx, |view, _| view.dirty));
}

#[gpui_kit::test]
async fn an_unreachable_feed_is_taken_down_and_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, view) = open_set(cx, dir.path()).await;
    view.update(cx, |view, _| {
        let test = view.test_feeds.as_mut().unwrap();
        test.availability
            .insert("kind-beta".to_owned(), Availability::CannotBeChecked(None));
    });
    // The click comes first: the aged snapshot keeps the feed's id.
    t.dispatch(cx, OPEN_MEMBER, json!({ "cluster": "kind-beta" }))
        .unwrap();
    cx.run_until_parked();
    assert!(dashboard_open(cx, &t));
    t.workspace
        .update(cx, |ws, cx| ws.refresh_set_dashboard_for_test(cx));
    assert_eq!(beta_and_total(cx, &view), (false, "0+".to_owned()));
}

#[gpui_kit::test]
async fn a_feed_replaced_during_a_click_never_supplies_its_objects(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, view) = open_set(cx, dir.path()).await;
    view.update(cx, |view, _| {
        let test = view.test_feeds.as_mut().unwrap();
        *test.replace_after_read.borrow_mut() = Some((
            "kind-beta".to_owned(),
            FeedId::for_test("replacement"),
            failing_lists("no-such-uid"),
        ));
    });
    t.dispatch(
        cx,
        OPEN_PROBLEM,
        json!({ "category": "failing_pods", "cluster": "kind-beta" }),
    )
    .unwrap();
    cx.run_until_parked();
    // One read: the rendered feed's own objects, none of the replacement's.
    assert_eq!(matched(cx, &t), 1);
}

#[gpui_kit::test]
async fn a_link_drawn_before_a_feed_changed_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, view) = open_set(cx, dir.path()).await;
    let drawn = view.read_with(cx, |view, _| view.drawn_feeds);
    view.update(cx, |view, cx| {
        let mut model = view.model.clone();
        let replaced = FeedId::for_test("replacement");
        model
            .cards
            .iter_mut()
            .find(|card| card.cluster == "kind-beta")
            .unwrap()
            .feed = Some(replaced.clone());
        view.test_feeds
            .as_mut()
            .unwrap()
            .feeds
            .insert("kind-beta".to_owned(), replaced);
        view.set_model(model, cx);
    });
    assert_ne!(view.read_with(cx, |view, _| view.drawn_feeds), drawn);
    for (action, args) in [
        (
            OPEN_PROBLEM,
            json!({ "category": "failing_pods", "cluster": "kind-beta", "drawn": drawn }),
        ),
        (
            OPEN_MEMBER,
            json!({ "cluster": "kind-beta", "drawn": drawn }),
        ),
    ] {
        t.dispatch(cx, action, args).unwrap();
        cx.run_until_parked();
        assert!(dashboard_open(cx, &t), "{action} left the dashboard");
    }
}

fn short_lists(kind: &'static str, uid: &str) -> ProblemLists {
    let mut counted = CountedObjects::default();
    counted.insert(Some("default"), "web", Some(uid));
    ProblemLists::from([(
        ProblemCategory::ShortWorkloads,
        vec![ProblemList {
            kind,
            filter: ObjectFilter {
                label: format!("short {kind}"),
                objects: Arc::new(counted),
            },
        }],
    )])
}

#[gpui_kit::test]
async fn a_total_whose_members_now_disagree_on_kind_opens_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, view) = open_set(cx, dir.path()).await;
    view.update(cx, |view, _| {
        let test = view.test_feeds.as_mut().unwrap();
        test.lists.insert(
            "kind-alpha".to_owned(),
            short_lists("deployments", "uid-web"),
        );
        test.lists.insert(
            "kind-beta".to_owned(),
            short_lists("statefulsets", "uid-api"),
        );
    });
    for args in [
        json!({ "category": "short_workloads" }),
        json!({ "category": "short_workloads", "kind": "deployments" }),
    ] {
        t.dispatch(cx, OPEN_PROBLEM, args).unwrap();
        cx.run_until_parked();
        assert!(dashboard_open(cx, &t));
        assert_eq!(shown_label(cx, &t), None);
    }
}

#[test]
fn a_total_link_names_the_kind_its_counted_members_show() {
    let short = |cluster: &str, kind: &'static str, count: u32| {
        let counts =
            move |category| Some(u32::from(category == ProblemCategory::ShortWorkloads) * count);
        SetCard {
            short_kinds: vec![kind],
            tiles: tiles(counts),
            counts: ProblemCounts::from_fn(counts),
            ..card_with(cluster, CardState::Live, Some(0), cluster)
        }
    };
    let cards = [short("a", "deployments", 0), short("b", "statefulsets", 2)];
    assert_eq!(
        total_kind(&cards, ProblemCategory::ShortWorkloads),
        Some("statefulsets")
    );
    assert_eq!(total_kind(&cards, ProblemCategory::FailingPods), None);
}

#[gpui_kit::test]
async fn the_navigation_overview_opens_the_set_dashboard(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, _view) = open_set(cx, dir.path()).await;
    t.dispatch(cx, "cluster_sets::ShowClusterSetList", Value::Null)
        .unwrap();
    assert!(!dashboard_open(cx, &t));
    t.dispatch(cx, "workspace::OpenClusterOverview", Value::Null)
        .unwrap();
    assert!(dashboard_open(cx, &t));
    assert!(t.workspace.read_with(cx, |ws, _| ws.set_is_open_for_test()));
}

#[gpui_kit::test]
async fn closing_the_set_closes_its_dashboard(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, _view) = open_set(cx, dir.path()).await;
    t.dispatch(cx, "cluster_sets::CloseClusterSet", Value::Null)
        .unwrap();
    // An unsaved set asks before it closes.
    cx.wait_for(t.window, WAIT, |window, cx| {
        gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;
    t.click(cx, "ok");
    cx.run_until_parked();
    assert!(!dashboard_open(cx, &t));
    assert!(!t.workspace.read_with(cx, |ws, _| ws.set_is_open_for_test()));
}
