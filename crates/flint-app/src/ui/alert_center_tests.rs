//! The alert decisions over real telemetry snapshots, with time passed in.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flint_core::alerts::{AlertEvent, AlertSummary};
use flint_core::dashboard::{ProblemSnapshot, ProblemSummariser};
use flint_core::discovery::ClusterDiscovery;
use flint_core::handle::{ClusterSummary, ConnectionState};
use flint_core::kubeconfig::ContextInfo;
use flint_core::problems::{Classifier, Problem, ProblemKind, ProblemSeverity};
use flint_core::settings::{AiScope, WatchMode, set_ai_exposed, set_ai_scope};
use flint_core::telemetry::{
    Availability, KindCheck, MetricsCheck, TelemetrySnapshot, WatchedKind,
};
use gpui_kit::{AppContext as _, ClipboardItem, TestAppContext};
use jiff::Timestamp;
use k8s_openapi::api::core::v1::{Node, NodeCondition, NodeStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use super::super::support_tests::{open_workspace, standard_sections};
use super::{
    ALL_KINDS, AlertCenter, AlertOrigin, AlertTracker, Delivery, Discovered, MAX_RECENT,
    MutedClusters, Reading, SettingsDir, TrackerInput, check_origin, menu_row, out_of_scope_kinds,
    reading, send_batch, verified_discovery,
};
use crate::notifications::{DeliveryError, FreedesktopSender, Notification, NotificationSender};

const PROD: &str = "prod";
const SERVER: &str = "https://prod.example:6443";

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 + seconds).unwrap()
}

fn summary(name: &str, state: ConnectionState) -> ClusterSummary {
    ClusterSummary {
        info: ContextInfo {
            name: name.into(),
            cluster: Some(name.into()),
            user: None,
            namespace: None,
            server: Some(SERVER.into()),
            proxy_url: None,
            source: PathBuf::from("kubeconfig"),
        },
        state,
        generation: 1,
        definition: 1,
        endpoint: 1,
        confirmed: true,
        provisional: false,
        paused: false,
    }
}

fn connected(name: &str) -> ClusterSummary {
    let version = flint_core::ServerVersion {
        git_version: "v1".into(),
        platform: "linux/arm64".into(),
    };
    summary(name, ConnectionState::Connected(version))
}

fn bad_node(cluster: &str, name: &str) -> Problem {
    let node = Node {
        metadata: ObjectMeta {
            name: Some(name.into()),
            uid: Some(name.into()),
            ..ObjectMeta::default()
        },
        status: Some(NodeStatus {
            conditions: Some(vec![NodeCondition {
                type_: "Ready".into(),
                status: "False".into(),
                ..NodeCondition::default()
            }]),
            ..NodeStatus::default()
        }),
        ..Node::default()
    };
    Classifier::new(cluster, at(0)).node(&node).unwrap()
}

fn snapshot(cluster: &str, problems: Vec<Problem>, now: Timestamp) -> TelemetrySnapshot {
    let counts = ProblemSummariser::default().summarise(&ProblemSnapshot::unavailable(), now);
    TelemetrySnapshot {
        cluster: Arc::from(cluster),
        at: now,
        availability: Availability::Checked,
        kinds: WatchedKind::ALL.map(|kind| (kind, KindCheck::Current)),
        problems: problems.into(),
        unchecked: Vec::new(),
        settling: Vec::new(),
        checked: Arc::default(),
        counts,
        metrics: MetricsCheck::Waiting,
        capacity: None,
        inventory: Default::default(),
        version: Default::default(),
    }
}

fn complete() -> ClusterDiscovery {
    ClusterDiscovery::new(Vec::new())
}

fn tracker() -> AlertTracker {
    AlertTracker::new("test".to_owned())
}

fn input<'a>(cluster: &ClusterSummary, reading: Reading<'a>) -> TrackerInput<'a> {
    TrackerInput {
        cluster: Arc::from(cluster.info.name.as_str()),
        origin: AlertOrigin::of(cluster),
        label: cluster.info.name.clone(),
        reading,
    }
}

/// One pass of `cluster` reading `problems` with complete discovery.
fn feed(
    tracker: &mut AlertTracker,
    cluster: &ClusterSummary,
    problems: Vec<Problem>,
    now: Timestamp,
) -> Vec<Notification> {
    let snapshot = snapshot(&cluster.info.name, problems, now);
    let discovery = complete();
    let reading = Reading::Feed {
        snapshot: &snapshot,
        discovery: Some(&discovery),
    };
    tracker.observe(&[input(cluster, reading)], now)
}

/// Takes a healthy baseline, then raises one alert on `name`.
fn raised(tracker: &mut AlertTracker, cluster: &ClusterSummary, name: &str) -> Notification {
    let id = &cluster.info.name;
    assert!(feed(tracker, cluster, Vec::new(), at(0)).is_empty());
    let mut sent = feed(tracker, cluster, vec![bad_node(id, name)], at(600));
    assert_eq!(sent.len(), 1, "{sent:?}");
    sent.remove(0)
}

#[test]
fn a_new_problem_notifies_once_and_names_the_cluster() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let sent = raised(&mut tracker, &prod, "n1");
    assert!(sent.title.contains(PROD));
    assert_eq!(&*sent.cluster, PROD);
    assert!(tracker.find(&sent.tag).is_some());
    let again = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(615));
    assert!(again.is_empty(), "{again:?}");
}

#[test]
fn what_already_exists_when_first_checked_is_a_baseline() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    assert!(feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(0)).is_empty());
    assert!(feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(600)).is_empty());
}

#[test]
fn a_reconnect_does_not_repeat_open_alerts() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let lost = summary(PROD, ConnectionState::Connecting);
    for second in [610, 620, 630] {
        let reading = reading(&lost, WatchMode::All, None, None);
        assert!(matches!(reading, Reading::Lost));
        assert!(
            tracker
                .observe(&[input(&lost, reading)], at(second))
                .is_empty()
        );
    }
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(640));
    assert!(back.is_empty(), "{back:?}");
}

#[test]
fn a_feed_restarting_while_connected_keeps_open_alerts() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let gap = reading(&prod, WatchMode::All, None, None);
    assert!(matches!(gap, Reading::NotWatched));
    assert!(tracker.observe(&[input(&prod, gap)], at(610)).is_empty());
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(620));
    assert!(back.is_empty(), "{back:?}");
}

#[test]
fn a_long_outage_is_reported_once_and_recovery_raises_nothing_old() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let lost = summary(
        PROD,
        ConnectionState::Failed(flint_core::SafeErrorText::reduced("x")),
    );
    let pass = |tracker: &mut AlertTracker, second| {
        tracker.observe(&[input(&lost, Reading::Lost)], at(second))
    };
    assert!(pass(&mut tracker, 610).is_empty());
    let reported = pass(&mut tracker, 800);
    assert_eq!(reported.len(), 1);
    assert!(reported[0].title.starts_with("Cluster unavailable"));
    assert!(pass(&mut tracker, 900).is_empty());
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(950));
    assert_eq!(back.len(), 1);
    assert!(back[0].title.starts_with("Cluster available"));
}

#[test]
fn missing_discovery_claims_nothing() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    // The node is gone, but with no discovery that proves nothing: it must not resolve.
    for second in [700, 1400, 2100] {
        let healthy = snapshot(PROD, Vec::new(), at(second));
        let reading = Reading::Feed {
            snapshot: &healthy,
            discovery: None,
        };
        let sent = tracker.observe(&[input(&prod, reading)], at(second));
        assert!(sent.is_empty(), "{sent:?}");
    }
}

#[test]
fn incomplete_discovery_makes_only_its_kinds_unchecked() {
    let incomplete =
        |group: &str| ClusterDiscovery::with_incomplete_groups(Vec::new(), vec![group.to_owned()]);
    assert_eq!(out_of_scope_kinds(None).len(), ALL_KINDS.len());
    assert!(out_of_scope_kinds(Some(&complete())).is_empty());
    assert!(out_of_scope_kinds(Some(&incomplete("metrics.k8s.io"))).is_empty());
    assert_eq!(
        out_of_scope_kinds(Some(&incomplete("BATCH"))),
        [ProblemKind::JobFailed]
    );
    let apps = out_of_scope_kinds(Some(&incomplete("apps")));
    assert!(apps.contains(&ProblemKind::WorkloadShort));
    assert!(apps.contains(&ProblemKind::CrashLoop));
    assert!(!apps.contains(&ProblemKind::NodeNotReady));
}

#[test]
fn a_problem_of_an_incomplete_group_raises_nothing() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let core_incomplete =
        ClusterDiscovery::with_incomplete_groups(Vec::new(), vec!["core".to_owned()]);
    assert!(feed(&mut tracker, &prod, Vec::new(), at(0)).is_empty());
    for second in [1, 600] {
        let bad = snapshot(PROD, vec![bad_node(PROD, "n1")], at(second));
        let reading = Reading::Feed {
            snapshot: &bad,
            discovery: Some(&core_incomplete),
        };
        assert!(
            tracker
                .observe(&[input(&prod, reading)], at(second))
                .is_empty()
        );
    }
}

#[test]
fn clusters_with_no_feed_of_their_own_are_never_read_as_lost() {
    let on_view = connected("view");
    assert!(matches!(
        reading(&on_view, WatchMode::OnView, None, None),
        Reading::NotWatched
    ));
    let idle = summary("idle", ConnectionState::Disconnected);
    assert!(matches!(
        reading(&idle, WatchMode::All, None, None),
        Reading::NotWatched
    ));
    let mut paused = connected("paused");
    paused.paused = true;
    assert!(matches!(
        reading(&paused, WatchMode::All, None, None),
        Reading::Lost
    ));
}

#[test]
fn a_cluster_left_unwatched_starts_over() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let unwatched = |tracker: &mut AlertTracker, second| {
        tracker.observe(&[input(&prod, Reading::NotWatched)], at(second))
    };
    assert!(unwatched(&mut tracker, 610).is_empty());
    assert!(unwatched(&mut tracker, 700).is_empty());
    // Back later with the same problem: a new baseline, so no repeat.
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(800));
    assert!(back.is_empty(), "{back:?}");
}

#[test]
fn a_repointed_context_shares_no_history() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    let mut moved = connected(PROD);
    moved.generation = 2;
    moved.definition = 2;
    moved.info.server = Some("https://other.example:6443".into());
    // The new cluster's first check is a baseline: nothing about the old one is reported.
    let sent = feed(&mut tracker, &moved, vec![bad_node(PROD, "n1")], at(610));
    assert!(sent.is_empty(), "{sent:?}");
}

#[test]
fn turning_off_and_on_does_not_replay_a_backlog() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    tracker.reset();
    let bad = vec![bad_node(PROD, "n1"), bad_node(PROD, "n2")];
    assert!(feed(&mut tracker, &prod, bad.clone(), at(700)).is_empty());
    assert!(feed(&mut tracker, &prod, bad, at(1400)).is_empty());
}

#[test]
fn an_alert_only_opens_the_cluster_that_raised_it() {
    let prod = connected(PROD);
    let origin = AlertOrigin::of(&prod);
    let other = connected("other");

    let clusters = [other.clone(), prod.clone()];
    let opened = check_origin(PROD, &origin, &clusters, PROD).unwrap();
    assert_eq!(opened.info.name, PROD);

    let removed = check_origin(PROD, &origin, std::slice::from_ref(&other), PROD).unwrap_err();
    assert!(removed.contains("removed"), "{removed}");

    let mut repointed = prod.clone();
    repointed.info.server = Some("https://other.example:6443".into());
    let refused = check_origin(PROD, &origin, &[repointed], PROD).unwrap_err();
    assert!(refused.contains("changed"), "{refused}");

    // A move and back keeps the URL but not the definition.
    let mut moved_back = prod.clone();
    moved_back.definition = 2;
    assert!(check_origin(PROD, &origin, &[moved_back], PROD).is_err());

    let mut recreated = prod.clone();
    recreated.generation = 2;
    assert!(check_origin(PROD, &origin, &[recreated], PROD).is_err());

    let mut paused = prod;
    paused.paused = true;
    let refused = check_origin(PROD, &origin, &[paused], PROD).unwrap_err();
    assert!(refused.contains("paused"), "{refused}");
}

#[test]
fn a_refusal_cleans_the_name_it_shows() {
    let origin = AlertOrigin::of(&connected(PROD));
    let refused = check_origin(PROD, &origin, &[], "a\n<b>").unwrap_err();
    assert!(
        !refused.contains('\n') && !refused.contains('<'),
        "{refused}"
    );
}

#[test]
fn tags_differ_between_runs_and_alerts() {
    let prod = connected(PROD);
    let (mut one, mut two) = (AlertTracker::new("1".into()), AlertTracker::new("2".into()));
    let first = raised(&mut one, &prod, "n1");
    let second = raised(&mut two, &prod, "n1");
    assert_ne!(first.tag, second.tag);
    assert!(one.find(&second.tag).is_none());
    let next = feed(
        &mut one,
        &prod,
        vec![bad_node(PROD, "n1"), bad_node(PROD, "n2")],
        at(1300),
    );
    assert!(next.iter().all(|n| n.tag != first.tag));
}

#[test]
fn a_failed_rediscovery_vouches_for_nothing() {
    let catalog = Arc::new(complete());
    let found = |error: Option<&str>, generation| Discovered {
        discovery: Some(Arc::clone(&catalog)),
        error: error.map(str::to_owned),
        generation,
    };
    assert!(verified_discovery(Some(&found(None, 1)), 1).is_some());
    assert!(verified_discovery(Some(&found(Some("timed out"), 1)), 1).is_none());
    assert!(verified_discovery(Some(&found(None, 1)), 2).is_none());
    assert!(verified_discovery(None, 1).is_none());
}

#[test]
fn recovery_needs_a_pass_that_checked_every_kind() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    for second in [610, 800] {
        tracker.observe(&[input(&prod, Reading::Lost)], at(second));
    }
    let healthy = snapshot(PROD, Vec::new(), at(900));
    let blind = Reading::Feed {
        snapshot: &healthy,
        discovery: None,
    };
    let sent = tracker.observe(&[input(&prod, blind)], at(900));
    assert!(sent.is_empty(), "{sent:?}");
    let back = feed(&mut tracker, &prod, vec![bad_node(PROD, "n1")], at(915));
    assert_eq!(back.len(), 1, "{back:?}");
    assert!(back[0].title.starts_with("Cluster available"));
}

#[test]
fn a_feed_gap_does_not_complete_a_resolution_cooldown() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    raised(&mut tracker, &prod, "n1");
    // Clear from 610, so the cooldown would end at 1210, inside the gap.
    assert!(feed(&mut tracker, &prod, Vec::new(), at(610)).is_empty());
    let gap = tracker.observe(&[input(&prod, Reading::NotWatched)], at(1200));
    assert!(gap.is_empty(), "{gap:?}");
    assert!(tracker.checked.is_empty(), "a gap is not checked");
    let back = feed(&mut tracker, &prod, Vec::new(), at(1215));
    assert!(back.is_empty(), "the cooldown restarts: {back:?}");
    let done = feed(&mut tracker, &prod, Vec::new(), at(1815));
    assert_eq!(done.len(), 1, "{done:?}");
    assert!(done[0].title.starts_with("Alert resolved"));
}

#[test]
fn only_admitted_contexts_are_tracked() {
    let mut tracker = tracker();
    for index in 0..300 {
        let mut paused = connected(&format!("never-watched-{index}"));
        paused.paused = true;
        tracker.observe(&[input(&paused, Reading::Lost)], at(0));
    }
    assert!(tracker.origins.is_empty());
}

#[test]
fn removing_a_context_purges_its_alerts() {
    let prod = connected(PROD);
    let other = connected("other");
    let mut tracker = tracker();
    let sent = raised(&mut tracker, &prod, "n1");
    let reading = Reading::NotWatched;
    tracker.observe(&[input(&prod, reading), input(&other, reading)], at(610));
    assert!(tracker.find(&sent.tag).is_some());
    tracker.observe(&[input(&other, reading)], at(620));
    assert!(tracker.find(&sent.tag).is_none());
    assert_eq!(tracker.recent().count(), 0);
}

#[test]
fn turning_off_cancels_what_was_on_its_way() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let sent = raised(&mut tracker, &prod, "n1");
    tracker.reset();
    assert_eq!(
        tracker.find(&sent.tag).unwrap().delivery,
        Delivery::Cancelled
    );
}

#[derive(Default)]
struct CountingSender {
    sent: AtomicUsize,
}

impl NotificationSender for CountingSender {
    fn prepare(&self) -> Result<(), DeliveryError> {
        Ok(())
    }

    fn send(&self, _: &Notification) -> Result<(), DeliveryError> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn a_cancelled_batch_stops_before_its_next_submission() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let one = raised(&mut tracker, &prod, "n1");
    let batch = vec![one.clone(), one.clone(), one];
    let sender = CountingSender::default();
    let answers = send_batch(
        &sender,
        &batch,
        || sender.sent.load(Ordering::SeqCst) < 1,
        |_| false,
    );
    assert_eq!(answers.len(), 1);
    assert_eq!(sender.sent.load(Ordering::SeqCst), 1);
}

#[test]
fn a_cluster_turned_off_and_on_keeps_its_batch_cancelled() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let batch = vec![raised(&mut tracker, &prod, "n1")];
    let mut muted = MutedClusters::default();
    let since = muted.turned_off.clone();

    let (off, back_on) = muted.set(HashSet::from([PROD.to_owned()]));
    assert_eq!((off, back_on), (vec![PROD.to_owned()], false));
    let (off, back_on) = muted.set(HashSet::new());
    assert!(off.is_empty() && back_on);

    let sender = CountingSender::default();
    let answers = send_batch(&sender, &batch, || true, |c| muted.blocks(c, &since));
    assert!(answers.is_empty());
    assert_eq!(sender.sent.load(Ordering::SeqCst), 0);

    // A batch started after it came back on sends.
    let since = muted.turned_off.clone();
    let answers = send_batch(&sender, &batch, || true, |c| muted.blocks(c, &since));
    assert_eq!(answers.len(), 1);
}

#[gpui_kit::test]
async fn a_scope_narrowed_during_activation_stops_the_prompt(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    let whole = AiScope::default();
    let exposed = set_ai_exposed(&t.dir, "kind-alpha", Some(&t.server), &whole).unwrap();
    let model = t.model.clone();
    let center = cx.update(|cx| {
        cx.set_global(SettingsDir(t.dir.clone()));
        cx.write_to_clipboard(ClipboardItem::new_string("before".to_owned()));
        cx.new(|cx| AlertCenter::new(model, Arc::new(FreedesktopSender), cx))
    });
    t.model
        .update(cx, |model, _| model.settings = Arc::new(exposed));
    let tag = center.update(cx, |center, cx| {
        let summary = center.model.read(cx).active_cluster().unwrap().clone();
        let event = AlertEvent::Summary(AlertSummary {
            cluster: Arc::from("kind-alpha"),
            new: 2,
            resolved: 0,
            untracked: 0,
            highest: Some(ProblemSeverity::Critical),
        });
        let input = TrackerInput {
            cluster: Arc::from("kind-alpha"),
            origin: AlertOrigin::of(&summary),
            label: "kind-alpha".to_owned(),
            reading: Reading::Muted,
        };
        center.tracker.record(event, &input).tag
    });

    center.update(cx, |center, cx| center.investigate(&tag, cx));
    let narrowed = AiScope {
        namespaces: vec!["shop".to_owned()],
        ..AiScope::default()
    };
    set_ai_scope(&t.dir, "kind-alpha", &t.server, &whole, &narrowed).unwrap();
    cx.run_until_parked();

    let copied = cx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("before"));
}

#[test]
fn a_menu_row_keeps_the_whole_label() {
    let label = format!("{}-suffix [2]", "x".repeat(50));
    let notification = Notification {
        title: format!("Critical alert: {label}"),
        body: "y".repeat(200),
        cluster: Arc::from(PROD),
        tag: String::new(),
    };
    assert!(menu_row(&notification).contains(&label));
}

#[test]
fn raised_alerts_outlast_other_events_in_the_list() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let sent = raised(&mut tracker, &prod, "n1");
    let input = input(&prod, Reading::Lost);
    for _ in 0..MAX_RECENT + 5 {
        tracker.record(
            AlertEvent::ClusterAvailable {
                cluster: Arc::from(PROD),
            },
            &input,
        );
    }
    assert!(tracker.find(&sent.tag).is_some());
}

#[test]
fn a_muted_cluster_raises_nothing_and_comes_back_from_a_new_baseline() {
    let prod = connected(PROD);
    let discovery = complete();
    let mut tracker = tracker();
    let sent = raised(&mut tracker, &prod, "n1");
    assert!(tracker.origins.contains_key(PROD));

    let muted = tracker.observe(&[input(&prod, Reading::Muted)], at(10));
    assert!(muted.is_empty());
    assert!(!tracker.origins.contains_key(PROD));
    assert!(tracker.checked.is_empty());
    let kept = tracker.find(&sent.tag).expect("its alert stays listed");
    assert_eq!(kept.delivery, Delivery::Cancelled);

    let bad = snapshot(PROD, vec![bad_node(PROD, "n1")], at(20));
    let back = Reading::Feed {
        snapshot: &bad,
        discovery: Some(&discovery),
    };
    assert!(tracker.observe(&[input(&prod, back)], at(20)).is_empty());
}

#[test]
fn a_batch_skips_a_cluster_muted_while_it_is_sent() {
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<String>>);
    impl NotificationSender for Recording {
        fn prepare(&self) -> Result<(), DeliveryError> {
            Ok(())
        }
        fn send(&self, notification: &Notification) -> Result<(), DeliveryError> {
            self.0.lock().unwrap().push(notification.tag.clone());
            Ok(())
        }
    }
    let note = |cluster: &str, tag: &str| Notification {
        title: "t".into(),
        body: "b".into(),
        cluster: Arc::from(cluster),
        tag: tag.into(),
    };
    let sender = Recording::default();
    let batch = [note("off", "1"), note(PROD, "2")];
    let answers = send_batch(&sender, &batch, || true, |cluster| cluster == "off");
    assert_eq!(answers.len(), 1);
    assert_eq!(*sender.0.lock().unwrap(), ["2"]);
}

#[test]
fn delivery_records_the_refusal_and_the_list_is_bounded() {
    let prod = connected(PROD);
    let mut tracker = tracker();
    let sent = raised(&mut tracker, &prod, "n1");
    assert_eq!(tracker.find(&sent.tag).unwrap().delivery, Delivery::Sending);
    tracker.delivered(&sent.tag, Delivery::Refused(DeliveryError::NotPermitted));
    let recorded = &tracker.find(&sent.tag).unwrap().delivery;
    assert_eq!(recorded.state(), "not_permitted");
    assert!(recorded.text().starts_with("Not delivered"));
    assert!(!Delivery::Accepted.text().contains("shown"));

    let input = input(&prod, Reading::Lost);
    for _ in 0..MAX_RECENT + 5 {
        tracker.record(
            AlertEvent::ClusterAvailable {
                cluster: Arc::from(PROD),
            },
            &input,
        );
    }
    assert_eq!(tracker.recent().count(), MAX_RECENT);
}
