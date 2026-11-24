//! The dashboard's view model and its drawing, from fixtures.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use flint_core::cluster::SafeErrorText;
use flint_core::dashboard::{ProblemSnapshot, ProblemSummariser};
use flint_core::metrics::{Cpu, Memory, MetricsList, NodeMetrics, POLL_INTERVAL, Usage};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, Bounds, Entity, Point, TestAppContext, WindowBounds, WindowOptions, px, size,
};
use k8s_openapi::api::core::v1::{EventSeries, ObjectReference, PersistentVolumeClaimStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, Time};

use super::tiles::{tile_content_id, tile_id};
use super::*;
use core::prelude::v1::test;

const CLUSTER: &str = "one";

fn now() -> Timestamp {
    Timestamp::from_second(1_800_000_000).unwrap()
}

fn ago(seconds: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 - seconds).unwrap()
}

fn snapshot(availability: Availability, metrics: MetricsCheck) -> TelemetrySnapshot {
    let readable = matches!(availability, Availability::Checked | Availability::Partial);
    let unreadable = ProblemSnapshot::unavailable();
    let healthy = ProblemSnapshot {
        problems: &[],
        unchecked: &[
            ProblemCategory::WarningEvents,
            ProblemCategory::PendingClaims,
        ],
        warning_events: 0,
        pending_claims: 0,
    };
    let counts = ProblemSummariser::default()
        .summarise(if readable { &healthy } else { &unreadable }, now());
    let check = if readable {
        KindCheck::Current
    } else {
        KindCheck::Starting
    };
    TelemetrySnapshot {
        cluster: Arc::from(CLUSTER),
        at: now(),
        availability,
        kinds: WatchedKind::ALL.map(|kind| (kind, check.clone())),
        problems: Arc::from([]),
        unchecked: Vec::new(),
        settling: Vec::new(),
        checked: Arc::default(),
        counts,
        metrics,
        capacity: None,
        inventory: Default::default(),
        rollouts: Default::default(),
        version: Default::default(),
    }
}

fn checked() -> TelemetrySnapshot {
    snapshot(Availability::Checked, MetricsCheck::Current { nodes: 1 })
}

fn failure(cause: FailureCause, text: &str) -> CheckFailure {
    CheckFailure {
        cause,
        detail: SafeErrorText::reduced(text),
    }
}

fn warning(name: &str, seen: Timestamp) -> KubeEvent {
    KubeEvent {
        type_: Some("Warning".into()),
        reason: Some("BackOff".into()),
        message: Some("Back-off restarting failed container".into()),
        involved_object: ObjectReference {
            kind: Some("Pod".into()),
            namespace: Some("shop".into()),
            name: Some(name.into()),
            ..Default::default()
        },
        last_timestamp: Some(Time(seen)),
        metadata: ObjectMeta {
            name: Some(format!("{name}.1")),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn pending_claim() -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        status: Some(PersistentVolumeClaimStatus {
            phase: Some("Pending".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A store with one node sampled at each of `samples` steps, the last one at [`now`].
fn trends(samples: &[(u64, u64)]) -> TrendStore {
    let mut store = TrendStore::new();
    let step = POLL_INTERVAL.as_secs() as i64;
    for (index, (cpu, memory)) in samples.iter().enumerate() {
        let back = (samples.len() - 1 - index) as i64 * step;
        store.record(
            CLUSTER,
            ago(back),
            &Ok(MetricsList {
                items: vec![NodeMetrics {
                    name: "node-1".into(),
                    timestamp: None,
                    window: None,
                    usage: Usage {
                        cpu: Cpu::from_nanos(*cpu),
                        memory: Memory::from_bytes(*memory),
                    },
                }],
                skipped: 0,
            }),
        );
    }
    store
}

fn build(
    snapshot: &TelemetrySnapshot,
    trends: &TrendStore,
    events: Option<&[KubeEvent]>,
    claims: Option<&[PersistentVolumeClaim]>,
) -> DashboardModel {
    DashboardModel::build(&DashboardInput {
        snapshot,
        trends,
        thresholds: &Thresholds::default(),
        warning_events: events,
        pending_claims: claims,
        node_usage: &UsageRead::Waiting,
        server: None,
        now: now(),
    })
}

fn tile(model: &DashboardModel, category: ProblemCategory) -> &ProblemTile {
    model
        .tiles
        .iter()
        .find(|tile| tile.category == category)
        .unwrap()
}

fn empty_model() -> DashboardModel {
    build(&checked(), &TrendStore::new(), Some(&[]), Some(&[]))
}

fn unavailable_model() -> DashboardModel {
    let snapshot = snapshot(
        Availability::CannotBeChecked(Some(failure(FailureCause::Forbidden, "denied"))),
        MetricsCheck::Unavailable(MetricsError::Forbidden(SafeErrorText::reduced("denied"))),
    );
    build(&snapshot, &TrendStore::new(), None, None)
}

fn no_metrics_model() -> DashboardModel {
    let snapshot = snapshot(
        Availability::Checked,
        MetricsCheck::Unavailable(MetricsError::NotInstalled),
    );
    build(&snapshot, &TrendStore::new(), Some(&[]), Some(&[]))
}

fn full_model() -> DashboardModel {
    let events = [warning("web-0", ago(30)), warning("web-1", ago(300))];
    let claims = [pending_claim()];
    build(
        &checked(),
        &trends(&[(500_000_000, 1 << 30), (1_500_000_000, 2 << 30)]),
        Some(&events),
        Some(&claims),
    )
}

// View model

#[test]
fn a_healthy_cluster_counts_zero_everywhere() {
    let model = empty_model();
    assert_eq!(model.status, DashboardStatus::Checked);
    assert_eq!(model.tiles.len(), ProblemCategory::ALL.len());
    for tile in &model.tiles {
        assert_eq!(tile.count, Some(0), "{}", tile.label);
        assert_eq!(tile.level, ThresholdLevel::Healthy, "{}", tile.label);
        assert!(tile.opens_list());
    }
    assert_eq!(
        model.warnings,
        WarningsPanel::Rows {
            rows: Vec::new(),
            total: 0
        }
    );
}

#[test]
fn a_cluster_that_cannot_be_read_is_unknown_never_zero() {
    let model = unavailable_model();
    assert_eq!(
        model.status,
        DashboardStatus::CannotBeChecked("This cluster cannot be checked: no permission.".into())
    );
    for tile in &model.tiles {
        assert_eq!(tile.count, None, "{}", tile.label);
        assert_eq!(tile.level, ThresholdLevel::Unknown, "{}", tile.label);
        assert_eq!(tile.value(), NO_VALUE);
        assert!(!tile.opens_list());
    }
    assert_eq!(model.warnings, WarningsPanel::Unavailable);
}

#[test]
fn events_and_claims_do_not_depend_on_the_watched_kinds() {
    let snapshot = snapshot(Availability::CannotBeChecked(None), MetricsCheck::Waiting);
    let events = [warning("web-0", ago(30))];
    let claims = [pending_claim()];
    let model = build(&snapshot, &TrendStore::new(), Some(&events), Some(&claims));
    assert_eq!(tile(&model, ProblemCategory::WarningEvents).count, Some(1));
    assert_eq!(tile(&model, ProblemCategory::PendingClaims).count, Some(1));
    assert_eq!(tile(&model, ProblemCategory::FailingPods).count, None);
    let WarningsPanel::Rows { rows, total } = &model.warnings else {
        panic!("{:?}", model.warnings);
    };
    assert_eq!((rows.len(), *total), (1, 1));

    let model = build(&snapshot, &TrendStore::new(), None, Some(&claims));
    assert_eq!(tile(&model, ProblemCategory::WarningEvents).count, None);
    assert_eq!(tile(&model, ProblemCategory::PendingClaims).count, Some(1));
    assert_eq!(model.warnings, WarningsPanel::Unavailable);
}

#[test]
fn events_and_claims_that_were_not_read_stay_unknown() {
    let model = build(&checked(), &TrendStore::new(), None, None);
    assert_eq!(tile(&model, ProblemCategory::WarningEvents).count, None);
    assert_eq!(tile(&model, ProblemCategory::PendingClaims).count, None);
    assert_eq!(tile(&model, ProblemCategory::FailingPods).count, Some(0));
    assert_eq!(model.warnings, WarningsPanel::Unavailable);
}

#[test]
fn a_partial_check_names_what_is_missing() {
    let mut snapshot = snapshot(Availability::Partial, MetricsCheck::Waiting);
    snapshot.kinds[WatchedKind::Jobs as usize].1 =
        KindCheck::Failed(failure(FailureCause::Forbidden, "denied"));
    snapshot.kinds[WatchedKind::Nodes as usize].1 = KindCheck::TooLarge;
    snapshot.kinds[WatchedKind::Pods as usize].1 = KindCheck::Starting;
    let model = build(&snapshot, &TrendStore::new(), None, None);
    assert_eq!(
        model.status,
        DashboardStatus::Partial(
            "Not everything could be checked: Pods (still loading), Nodes (too many objects), \
             Jobs (no permission)."
                .into()
        )
    );
}

#[test]
fn supplied_events_and_claims_are_counted() {
    let model = full_model();
    assert_eq!(tile(&model, ProblemCategory::WarningEvents).count, Some(2));
    let claims = tile(&model, ProblemCategory::PendingClaims);
    assert_eq!(claims.count, Some(1));
    assert_eq!(claims.level, ThresholdLevel::Warning);
}

#[test]
fn the_warning_list_agrees_with_its_count() {
    let mut normal = warning("normal", ago(10));
    normal.type_ = Some("Normal".into());
    let mut timeless = warning("timeless", ago(10));
    timeless.last_timestamp = None;
    let events = [
        warning("old", ago(3600)),
        warning("recent", ago(3599)),
        warning("ahead", ago(-30)),
        normal,
        timeless,
    ];
    let model = build(&checked(), &TrendStore::new(), Some(&events), Some(&[]));
    let WarningsPanel::Rows { rows, total } = &model.warnings else {
        panic!("{:?}", model.warnings);
    };
    assert_eq!(*total, 2);
    assert_eq!(
        tile(&model, ProblemCategory::WarningEvents).count,
        Some(*total as u32)
    );
    assert_eq!(rows[0].object, "Pod shop/ahead");
    assert_eq!(rows[0].age, "now");
    assert_eq!(rows[1].object, "Pod shop/recent");
    assert_eq!(rows[1].age, "59m");
}

#[test]
fn the_newest_time_of_an_event_decides_its_place() {
    let mut repeated = warning("repeated", ago(3000));
    repeated.series = Some(EventSeries {
        count: Some(7),
        last_observed_time: Some(MicroTime(ago(5))),
    });
    let events = [warning("single", ago(60)), repeated];
    let model = build(&checked(), &TrendStore::new(), Some(&events), Some(&[]));
    let WarningsPanel::Rows { rows, .. } = &model.warnings else {
        panic!("{:?}", model.warnings);
    };
    assert_eq!(rows[0].object, "Pod shop/repeated");
    assert_eq!(rows[0].occurrences, 7);
    assert_eq!(rows[0].age, "5s");
}

#[test]
fn the_warning_list_is_bounded_but_reports_the_total() {
    let events: Vec<KubeEvent> = (0..MAX_WARNING_ROWS as i64 + 5)
        .map(|index| warning(&format!("pod-{index}"), ago(index)))
        .collect();
    let model = build(&checked(), &TrendStore::new(), Some(&events), Some(&[]));
    let WarningsPanel::Rows { rows, total } = &model.warnings else {
        panic!("{:?}", model.warnings);
    };
    assert_eq!(rows.len(), MAX_WARNING_ROWS);
    assert_eq!(*total, MAX_WARNING_ROWS + 5);
    assert_eq!(rows[0].object, "Pod shop/pod-0");
}

#[test]
fn text_from_the_cluster_cannot_break_a_row() {
    let mut event = warning("web\u{202e}-0", ago(5));
    event.message = Some(format!("line\nbreak{}", "x".repeat(MAX_MESSAGE_CHARS)));
    let model = build(&checked(), &TrendStore::new(), Some(&[event]), Some(&[]));
    let WarningsPanel::Rows { rows, .. } = &model.warnings else {
        panic!("{:?}", model.warnings);
    };
    assert_eq!(rows[0].object, "Pod shop/web -0");
    assert!(rows[0].message.starts_with("line break"));
    assert!(rows[0].message.ends_with('…'));
    assert_eq!(rows[0].message.chars().count(), MAX_MESSAGE_CHARS + 1);
}

#[test]
fn metrics_show_the_newest_sample_and_the_hour() {
    let model = full_model();
    let MetricsPanel::Tiles {
        cpu,
        memory,
        notice,
    } = &model.metrics
    else {
        panic!("{:?}", model.metrics);
    };
    assert_eq!(cpu.value.as_deref(), Some("1.50 cores"));
    assert_eq!(memory.value.as_deref(), Some("2.0 GiB"));
    assert_eq!(*notice, None);
    assert!(!cpu.shape.is_empty());
}

#[test]
fn an_old_sample_is_not_passed_off_as_current() {
    let store = trends(&[(500_000_000, 1 << 30)]);
    let later = now()
        .checked_add(jiff::SignedDuration::from_secs(
            POLL_INTERVAL.as_secs() as i64 * 10,
        ))
        .unwrap();
    let model = DashboardModel::build(&DashboardInput {
        snapshot: &checked(),
        trends: &store,
        thresholds: &Thresholds::default(),
        warning_events: Some(&[]),
        pending_claims: Some(&[]),
        node_usage: &UsageRead::Waiting,
        server: None,
        now: later,
    });
    let MetricsPanel::Tiles { cpu, .. } = &model.metrics else {
        panic!("{:?}", model.metrics);
    };
    assert_eq!(cpu.value, None);
    assert!(!cpu.shape.is_empty(), "the history stays");
}

fn node_metrics(name: &str, cpu: u64) -> NodeMetrics {
    NodeMetrics {
        name: name.into(),
        timestamp: None,
        window: None,
        usage: Usage {
            cpu: Cpu::from_nanos(cpu),
            memory: Memory::from_bytes(1 << 30),
        },
    }
}

fn metric_values(model: &DashboardModel) -> (Option<&str>, Option<&str>, bool) {
    let MetricsPanel::Tiles { cpu, memory, .. } = &model.metrics else {
        panic!("{:?}", model.metrics);
    };
    (
        cpu.value.as_deref(),
        memory.value.as_deref(),
        !cpu.shape.is_empty() && !memory.shape.is_empty(),
    )
}

#[test]
fn a_poll_missing_a_node_shows_no_current_total() {
    let step = POLL_INTERVAL.as_secs() as i64;
    let mut store = TrendStore::new();
    let both = vec![
        node_metrics("node-1", 500_000_000),
        node_metrics("node-2", 500_000_000),
    ];
    store.record(
        CLUSTER,
        ago(step),
        &Ok(MetricsList {
            items: both,
            skipped: 0,
        }),
    );
    store.record(
        CLUSTER,
        now(),
        &Ok(MetricsList {
            items: vec![node_metrics("node-1", 500_000_000)],
            skipped: 0,
        }),
    );
    let snapshot = snapshot(Availability::Checked, MetricsCheck::Current { nodes: 1 });
    let model = build(&snapshot, &store, Some(&[]), Some(&[]));
    assert_eq!(metric_values(&model), (None, None, true));
}

#[test]
fn an_empty_poll_shows_no_current_total() {
    let mut store = trends(&[(500_000_000, 1 << 30)]);
    store.record(
        CLUSTER,
        now(),
        &Ok(MetricsList {
            items: Vec::new(),
            skipped: 0,
        }),
    );
    let snapshot = snapshot(Availability::Checked, MetricsCheck::Current { nodes: 0 });
    let model = build(&snapshot, &store, Some(&[]), Some(&[]));
    assert_eq!(metric_values(&model), (None, None, true));
}

#[test]
fn a_complete_poll_stays_current_while_the_next_is_slow() {
    let store = trends(&[(500_000_000, 1 << 30)]);
    let step = POLL_INTERVAL.as_secs() as i64;
    let read_at = |steps: i64| {
        DashboardModel::build(&DashboardInput {
            snapshot: &checked(),
            trends: &store,
            thresholds: &Thresholds::default(),
            warning_events: Some(&[]),
            pending_claims: Some(&[]),
            node_usage: &UsageRead::Waiting,
            server: None,
            now: now()
                .checked_add(jiff::SignedDuration::from_secs(step * steps))
                .unwrap(),
        })
    };
    assert_eq!(
        metric_values(&read_at(2)),
        (Some("500m"), Some("1.0 GiB"), true)
    );
    assert_eq!(metric_values(&read_at(3)), (None, None, true));
}

#[test]
fn a_failed_poll_keeps_the_history_without_a_current_value() {
    let snapshot = snapshot(
        Availability::Checked,
        MetricsCheck::Unavailable(MetricsError::Failed(SafeErrorText::reduced("timed out"))),
    );
    let store = trends(&[(500_000_000, 1 << 30)]);
    let model = build(&snapshot, &store, Some(&[]), Some(&[]));
    let MetricsPanel::Tiles { cpu, notice, .. } = &model.metrics else {
        panic!("{:?}", model.metrics);
    };
    assert_eq!(cpu.value, None);
    assert!(!cpu.shape.is_empty());
    assert!(notice.as_deref().unwrap().contains("cannot be checked"));
}

#[test]
fn missing_metrics_server_is_its_own_state() {
    assert_eq!(no_metrics_model().metrics, MetricsPanel::NotInstalled);
}

#[test]
fn before_the_first_poll_metrics_are_waiting() {
    let snapshot = snapshot(Availability::Starting, MetricsCheck::Waiting);
    let model = build(&snapshot, &TrendStore::new(), None, None);
    assert_eq!(model.status, DashboardStatus::Starting);
    assert_eq!(model.metrics, MetricsPanel::Waiting);
}

#[test]
fn amounts_read_in_their_units() {
    assert_eq!(format_cpu(250_000_000), "250m");
    assert_eq!(format_cpu(1_000_000_000), "1.00 cores");
    assert_eq!(format_memory(512 << 10), "512 KiB");
    assert_eq!(format_memory(300 << 20), "300 MiB");
    assert_eq!(format_memory(3 << 30), "3.0 GiB");
}

// Drawing

fn open(
    cx: &mut TestAppContext,
    model: DashboardModel,
) -> (AnyWindowHandle, Entity<DashboardView>) {
    cx.update(gpui_kit::init);
    let mut view = None;
    let window = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(900.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, cx| {
                    let entity = cx.new(|cx| DashboardView::new(model, cx));
                    view = Some(entity.clone());
                    entity
                },
            )
        })
        .unwrap()
        .0;
    cx.run_until_parked();
    (window, view.unwrap())
}

/// The label of the element `id` in a fresh frame, if it is drawn.
fn label(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    id: impl Into<ElementId>,
) -> Option<String> {
    let id = id.into();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .try_find(id)
            .map(|found| found.label().unwrap_or_default().to_owned())
    })
    .unwrap()
}

#[gpui_kit::test]
async fn draws_a_full_dashboard(cx: &mut TestAppContext) {
    let model = full_model();
    let first = model.tiles[2].clone();
    let WarningsPanel::Rows { rows, .. } = &model.warnings else {
        panic!("the full model has Warning rows");
    };
    let warning_id = ElementId::Name(format!("dashboard-warning-{}", rows[0].key).into());
    let (window, _) = open(cx, model);
    assert_eq!(
        label(cx, window, tile_id(&first)).as_deref(),
        Some("Warning events: 2")
    );
    assert_eq!(
        label(cx, window, tile_content_id(&first)).as_deref(),
        Some(""),
        "the name is on the control, not repeated on its content"
    );
    assert_eq!(
        label(cx, window, "dashboard-cpu").as_deref(),
        Some("CPU: 1.50 cores")
    );
    assert_eq!(
        label(cx, window, "dashboard-memory").as_deref(),
        Some("Memory: 2.0 GiB")
    );
    let row = label(cx, window, warning_id).unwrap();
    assert!(row.contains("Pod shop/web-0"), "{row}");
    assert_eq!(label(cx, window, "dashboard-status"), None);
}

#[gpui_kit::test]
async fn draws_the_empty_state(cx: &mut TestAppContext) {
    let (window, _) = open(cx, empty_model());
    assert_eq!(
        label(cx, window, "dashboard-warnings-empty").as_deref(),
        Some(NO_WARNINGS)
    );
}

#[gpui_kit::test]
async fn draws_the_unavailable_state(cx: &mut TestAppContext) {
    let model = unavailable_model();
    let first = model.tiles[0].clone();
    let (window, _) = open(cx, model);
    assert_eq!(
        label(cx, window, "dashboard-status").as_deref(),
        Some("This cluster cannot be checked: no permission.")
    );
    assert_eq!(
        label(cx, window, tile_content_id(&first)).as_deref(),
        Some("Failing pods: cannot be checked")
    );
    assert_eq!(
        label(cx, window, "dashboard-warnings-unavailable").as_deref(),
        Some(WARNINGS_UNAVAILABLE)
    );
    assert_eq!(
        label(cx, window, "dashboard-cpu").as_deref(),
        Some("CPU: —")
    );
}

#[gpui_kit::test]
async fn draws_the_install_hint_without_metrics_server(cx: &mut TestAppContext) {
    let (window, _) = open(cx, no_metrics_model());
    assert_eq!(
        label(cx, window, "dashboard-metrics-hint").as_deref(),
        Some(INSTALL_HINT)
    );
    assert_eq!(
        label(cx, window, "dashboard-install-command").as_deref(),
        Some(INSTALL_COMMAND)
    );
    assert_eq!(label(cx, window, "dashboard-cpu"), None);
}

#[gpui_kit::test]
async fn clicking_a_tile_asks_for_its_filtered_list(cx: &mut TestAppContext) {
    let model = full_model();
    let target = tile(&model, ProblemCategory::PendingClaims).clone();
    let (window, view) = open(cx, model);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let record = seen.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&view, move |_, event: &DashboardEvent, _| {
            record.borrow_mut().push(event.clone());
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(tile_id(&target), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        *seen.borrow(),
        vec![DashboardEvent::OpenFilteredList(
            ProblemCategory::PendingClaims
        )]
    );
}

#[gpui_kit::test]
async fn a_stale_click_on_an_unknown_tile_opens_nothing(cx: &mut TestAppContext) {
    let (window, view) = open(cx, unavailable_model());
    let seen = Rc::new(RefCell::new(Vec::new()));
    let record = seen.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&view, move |_, event: &DashboardEvent, _| {
            record.borrow_mut().push(event.clone());
        })
    });
    cx.update_window(window, |_, window, cx| {
        let focus = view.read(cx).focus.clone();
        focus.dispatch_action(
            &OpenProblemList {
                category: ProblemCategory::FailingPods,
            },
            window,
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    assert!(seen.borrow().is_empty());
}

#[gpui_kit::test]
async fn draws_capacity_and_inventory_with_usage_unavailable(cx: &mut TestAppContext) {
    use flint_core::telemetry::{
        Allocatable, ClusterCapacity, Inventory, NodeCapacity, Reserved, VersionCheck,
    };
    let scope = OpenScope {
        cluster: CLUSTER.to_owned(),
        server: None,
    };
    let capacity = ClusterCapacity {
        nodes: vec![NodeCapacity {
            name: "node-1".to_owned(),
            allocatable: Allocatable {
                cpu: Some(Cpu::from_nanos(4_000_000_000)),
                memory: Some(Memory::from_bytes(8 << 30)),
                pods: Some(110),
            },
            requests: Reserved {
                cpu: Some(Cpu::from_nanos(1_000_000_000)),
                memory: Some(Memory::from_bytes(2 << 30)),
            },
            limits: Reserved::default(),
            pods: Some(11),
        }]
        .into(),
    };
    let mut model = no_metrics_model();
    model.capacity = capacity_panel(
        Some(&capacity),
        &UsageRead::Unavailable(MetricsError::NotInstalled),
    );
    model.inventory = inventory_panel(
        &Inventory {
            nodes: Some(1),
            ..Inventory::default()
        },
        &VersionCheck::Known("v1.31.2".to_owned()),
        &scope,
    );
    let (window, _) = open(cx, model);
    assert_eq!(
        label(cx, window, ("dashboard-capacity", 0usize)).as_deref(),
        Some(
            "CPU: 3.00 cores left of 4.00 cores, requested 1.00 cores (25%), used unavailable, \
             limits unknown"
        )
    );
    assert_eq!(
        label(cx, window, ("dashboard-node", 0usize)).as_deref(),
        Some(
            "node-1: CPU requested 1.00 cores (25%), memory requested 2.0 GiB (25%), pods 11 / \
             110, CPU used unavailable, memory used unavailable"
        )
    );
    assert_eq!(
        label(cx, window, "dashboard-version").as_deref(),
        Some("Kubernetes v1.31.2")
    );
    assert_eq!(
        label(cx, window, ("dashboard-count-unknown", 1usize)).as_deref(),
        Some("Pods: unknown")
    );
}

#[test]
fn a_warning_links_only_to_an_object_it_names_exactly() {
    let scope = OpenScope {
        cluster: CLUSTER.to_owned(),
        server: Some("https://one".to_owned()),
    };
    let target = |kind: Option<&str>, api_version: Option<&str>, name: Option<&str>| {
        let mut event = warning("web-0", ago(10));
        event.involved_object.kind = kind.map(str::to_owned);
        event.involved_object.api_version = api_version.map(str::to_owned);
        event.involved_object.name = name.map(str::to_owned);
        warning_target(&event, &scope)
    };
    let core = target(Some("Pod"), Some("v1"), Some("web-0")).expect("a core object");
    assert_eq!(core.kind, "Pod.v1");
    assert_eq!(core.namespace.as_deref(), Some("shop"));
    assert_eq!(core.name, "web-0");
    assert_eq!(core.cluster.as_deref(), Some(CLUSTER));
    assert_eq!(core.server.as_deref(), Some("https://one"));
    let grouped = target(Some("Widget"), Some("example.io/v1beta1"), Some("w")).unwrap();
    assert_eq!(grouped.kind, "Widget.example.io");

    assert_eq!(target(None, Some("v1"), Some("web-0")), None);
    assert_eq!(target(Some("Pod"), None, Some("web-0")), None);
    assert_eq!(target(Some("Pod"), Some("v1"), None), None);
    assert_eq!(target(Some("Pod"), Some("v1"), Some("  ")), None);
    assert_eq!(target(Some("Pod.apps"), Some("v1"), Some("web-0")), None);
}

#[gpui_kit::test]
async fn a_new_model_redraws(cx: &mut TestAppContext) {
    let (window, view) = open(cx, unavailable_model());
    view.update(cx, |view, cx| view.set_model(empty_model(), cx));
    cx.run_until_parked();
    assert_eq!(label(cx, window, "dashboard-status"), None);
    assert_eq!(
        label(cx, window, "dashboard-warnings-empty").as_deref(),
        Some(NO_WARNINGS)
    );
}

fn rollout(name: &str, state: RolloutState, at: Option<Timestamp>) -> Rollout {
    Rollout {
        namespace: "shop".into(),
        name: name.into(),
        uid: Some(format!("uid-{name}")),
        state,
        at,
        desired: 3,
        updated: 1,
        available: 2,
        message: None,
    }
}

fn with_rollouts(rollouts: Vec<Rollout>) -> TelemetrySnapshot {
    TelemetrySnapshot {
        rollouts: rollouts.into(),
        ..checked()
    }
}

fn rows_of(model: &DashboardModel) -> (&[WarningRow], usize) {
    let WarningsPanel::Rows { rows, total } = &model.warnings else {
        panic!("{:?}", model.warnings);
    };
    (rows, *total)
}

#[test]
fn rollouts_share_the_feed_with_warnings_newest_first() {
    let snapshot = with_rollouts(vec![
        rollout("web", RolloutState::Progressing, Some(ago(60))),
        rollout("api", RolloutState::Stalled, Some(ago(600))),
    ]);
    let events = [warning("web-0", ago(300))];
    let model = build(&snapshot, &TrendStore::new(), Some(&events), None);
    let (rows, total) = rows_of(&model);
    assert_eq!(total, 3);
    let reasons: Vec<&str> = rows.iter().map(|row| row.reason.as_str()).collect();
    assert_eq!(
        reasons,
        ["Rollout progressing", "BackOff", "Rollout stalled"]
    );
    assert_eq!(rows[0].object, "Deployment shop/web");
    assert_eq!(rows[0].message, "1 of 3 updated, 2 available");
    assert_eq!(rows[0].rollout, Some(RolloutState::Progressing));
    assert_eq!(rows[1].rollout, None);
}

#[test]
fn a_rollout_row_opens_its_deployment() {
    let snapshot = with_rollouts(vec![rollout("web", RolloutState::Started, None)]);
    let model = build(&snapshot, &TrendStore::new(), Some(&[]), None);
    let (rows, _) = rows_of(&model);
    assert_eq!(rows[0].age, "now");
    assert_eq!(rows[0].object_uid.as_deref(), Some("uid-web"));
    let open = rows[0].open.clone().unwrap();
    assert_eq!(
        (
            open.kind.as_str(),
            open.namespace.as_deref(),
            open.name.as_str()
        ),
        ("Deployment.apps", Some("shop"), "web")
    );
    assert_eq!(open.cluster.as_deref(), Some(CLUSTER));
}

#[test]
fn one_rollout_is_one_row_with_one_key_as_it_changes_state() {
    let key_in = |state| {
        let snapshot = with_rollouts(vec![rollout("web", state, Some(ago(30)))]);
        let model = build(&snapshot, &TrendStore::new(), Some(&[]), None);
        let (rows, total) = rows_of(&model);
        assert_eq!((rows.len(), total), (1, 1));
        rows[0].key.clone()
    };
    let keys = [
        RolloutState::Started,
        RolloutState::Progressing,
        RolloutState::Complete,
    ]
    .map(key_in);
    assert!(keys.iter().all(|key| *key == keys[0]), "{keys:?}");
}

#[test]
fn a_finished_rollout_leaves_the_feed_after_an_hour() {
    let snapshot = with_rollouts(vec![
        rollout("new", RolloutState::Complete, Some(ago(60))),
        rollout("old", RolloutState::Complete, Some(ago(3_700))),
        rollout("stuck", RolloutState::Stalled, Some(ago(7_200))),
    ]);
    let model = build(&snapshot, &TrendStore::new(), Some(&[]), None);
    let (rows, total) = rows_of(&model);
    let objects: Vec<&str> = rows.iter().map(|row| row.object.as_str()).collect();
    assert_eq!(objects, ["Deployment shop/new", "Deployment shop/stuck"]);
    assert_eq!(total, 2);
}
