//! The Problems filter against the fake API fixture, and what it says when it cannot check.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flint_core::dashboard::{ProblemSnapshot, ProblemSummariser};
use flint_core::problems::{
    Classifier, Problem, ProblemId, ProblemKind, ProblemSubject, ReplicaSetIndex,
};
use flint_core::telemetry::{
    Availability, CheckedObjects, KindCheck, TelemetrySnapshot, WatchedKind,
};
use flint_core::{Cell, ConnectionState, KindId, Row};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Options};
use gpui_kit::TestAppContext;
use gpui_kit::test::TestAppContextExt as _;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;

use super::problems::{ClusterProblems, ProblemBadge, ProblemFilter, nav_badges};
use crate::ui::support_tests::open_workspace_with;
use crate::ui::workspace::problems::{FeedIdentity, ProblemFeed};

const WAIT: Duration = Duration::from_secs(30);
const FIXTURE_PODS: &str = include_str!("../../../flint-e2e/fixtures/objects/core/v1/pods.json");

fn fixture_problems() -> Vec<Problem> {
    let list: serde_json::Value = serde_json::from_str(FIXTURE_PODS).unwrap();
    let pods: Vec<Pod> = serde_json::from_value(list["items"].clone()).unwrap();
    let now: Timestamp = "2026-01-01T01:00:00Z".parse().unwrap();
    Classifier::new("fixture", now).pods_with_replica_sets(&pods, &ReplicaSetIndex::unavailable())
}

/// The uids of the fixture's pods, and of the healthy row the tests add.
fn fixture_uids() -> Vec<String> {
    let list: serde_json::Value = serde_json::from_str(FIXTURE_PODS).unwrap();
    let items = list["items"].as_array().unwrap();
    items
        .iter()
        .map(|pod| pod["metadata"]["uid"].as_str().unwrap().to_owned())
        .chain(["uid-healthy".to_owned()])
        .collect()
}

fn snapshot(
    problems: Vec<Problem>,
    availability: Availability,
    pods: KindCheck,
) -> TelemetrySnapshot {
    snapshot_judging(problems, availability, pods, fixture_uids())
}

/// A snapshot whose pass judged exactly the pods with `judged` uids.
fn snapshot_judging(
    problems: Vec<Problem>,
    availability: Availability,
    pods: KindCheck,
    judged: Vec<String>,
) -> TelemetrySnapshot {
    let at = Timestamp::UNIX_EPOCH;
    let mut kinds = WatchedKind::ALL.map(|kind| (kind, KindCheck::Current));
    kinds[WatchedKind::Pods as usize].1 = pods;
    let mut checked = CheckedObjects::default();
    checked.record_uids(WatchedKind::Pods, judged);
    TelemetrySnapshot {
        cluster: "fixture".into(),
        at,
        availability,
        kinds,
        problems: problems.into(),
        unchecked: Vec::new(),
        settling: Vec::new(),
        checked: Arc::new(checked),
        counts: ProblemSummariser::default().summarise(&ProblemSnapshot::unavailable(), at),
        metrics: Default::default(),
        capacity: None,
        inventory: Default::default(),
        version: Default::default(),
    }
}

fn row(name: &str, uid: &str) -> Row {
    Row {
        uid: uid.into(),
        name: name.into(),
        namespace: Some("default".into()),
        resource_version: "1".into(),
        generation: None,
        created: Some(0),
        labels: Box::new([]),
        cells: Box::new([Cell::Name]),
    }
}

fn filter_of(cluster: &str, problems: ClusterProblems) -> ProblemFilter {
    let mut filter = ProblemFilter::default();
    filter.toggle();
    filter.update(HashMap::from([(cluster.to_owned(), problems)]));
    filter
}

/// A cluster checked in full, whose one problem is the pod `name` with `uid`, and whose pass also
/// judged the healthy pods `healthy`.
pub(super) fn checked_with_pod(name: &str, uid: &str, healthy: &[&str]) -> ClusterProblems {
    let problem = crash_loop(name, uid);
    let judged = healthy
        .iter()
        .copied()
        .chain([uid])
        .map(str::to_owned)
        .collect();
    ClusterProblems::read(Some(&snapshot_judging(
        vec![problem],
        Availability::Checked,
        KindCheck::Current,
        judged,
    )))
}

fn crash_loop(name: &str, uid: &str) -> Problem {
    Problem {
        id: ProblemId {
            cluster: "c".into(),
            kind: ProblemKind::CrashLoop,
            subject: ProblemSubject::Container {
                namespace: "default".to_owned(),
                pod: name.to_owned(),
                pod_uid: Some(uid.to_owned()),
                container: "app".to_owned(),
            },
        },
        owner: None,
        owner_known: true,
        reason: None,
        since: None,
    }
}

#[test]
fn a_recreated_pod_the_feed_has_not_judged_stays_and_is_named() {
    let problems = checked_with_pod("web", "uid-old", &["uid-healthy"]);
    let mut filter = filter_of("c", problems);
    let keep = filter.row_filter(&KindId::pods(), "c").unwrap();
    let recreated = row("web", "uid-new");
    assert!(keep.keeps(&recreated), "not judged yet, so not hidden");
    assert!(keep.awaits_check(&recreated));
    let healthy = row("healthy", "uid-healthy");
    assert!(!keep.keeps(&healthy), "judged and healthy, so hidden");
    assert!(!keep.awaits_check(&healthy));
    assert!(!keep.awaits_check(&row("web", "uid-old")));

    filter.set_awaiting(1);
    assert!(filter.awaiting_note().unwrap().contains("not been checked"));
    filter.toggle();
    assert_eq!(filter.awaiting_note(), None);
}

#[test]
fn a_badge_counts_only_the_namespaces_shown() {
    let problems = checked_with_pod("web", "uid-web", &[]);
    let in_scope = nav_badges(std::slice::from_ref(&problems), &["default".to_owned()]);
    assert_eq!(in_scope[&KindId::pods()].text, "1");
    let out_of_scope = nav_badges(std::slice::from_ref(&problems), &["other".to_owned()]);
    assert!(!out_of_scope.contains_key(&KindId::pods()));
    let all = nav_badges(&[problems], &[]);
    assert_eq!(all[&KindId::pods()].text, "1");
}

#[test]
fn a_problem_on_two_containers_of_one_pod_counts_once_in_its_namespace() {
    let mut second = crash_loop("web", "uid-web");
    if let ProblemSubject::Container { container, .. } = &mut second.id.subject {
        *container = "sidecar".to_owned();
    }
    let problems = ClusterProblems::read(Some(&snapshot(
        vec![crash_loop("web", "uid-web"), second],
        Availability::Checked,
        KindCheck::Current,
    )));
    let badges = nav_badges(&[problems], &["default".to_owned()]);
    assert_eq!(badges[&KindId::pods()].text, "1");
}

#[test]
fn a_stuck_pending_pod_is_kept_by_the_filter_and_counted() {
    let problem = Problem {
        id: ProblemId {
            cluster: "c".into(),
            kind: ProblemKind::PodPending,
            subject: ProblemSubject::Pod {
                namespace: "default".to_owned(),
                name: "stuck".to_owned(),
                uid: Some("uid-stuck".to_owned()),
            },
        },
        owner: None,
        owner_known: true,
        reason: None,
        since: None,
    };
    let problems = ClusterProblems::read(Some(&snapshot_judging(
        vec![problem],
        Availability::Checked,
        KindCheck::Current,
        vec!["uid-stuck".to_owned()],
    )));
    assert_eq!(
        nav_badges(std::slice::from_ref(&problems), &[])[&KindId::pods()].text,
        "1"
    );
    let filter = filter_of("c", problems);
    let keep = filter.row_filter(&KindId::pods(), "c").unwrap();
    assert!(keep.keeps(&row("stuck", "uid-stuck")));
}

fn identity(endpoint: u64) -> FeedIdentity {
    FeedIdentity {
        cluster: "c".to_owned(),
        generation: 1,
        endpoint,
    }
}

#[test]
fn a_repointed_context_drops_values_and_refuses_an_earlier_read() {
    let values = || HashMap::from([("c".to_owned(), checked_with_pod("web", "uid-web", &[]))]);
    let mut feed = ProblemFeed::default();
    feed.follow(vec![identity(1)]);
    let read = feed.begin_read();
    feed.finish(vec![identity(1)], &read, values());
    assert!(feed.value("c").is_some());

    let old_read = feed.begin_read();
    feed.follow(vec![identity(2)]);
    assert!(
        feed.value("c").is_none(),
        "the old server's values are gone"
    );
    feed.finish(vec![identity(2)], &old_read, values());
    assert!(
        feed.value("c").is_none(),
        "a read of the old server is dropped"
    );
    let read = feed.begin_read();
    feed.finish(vec![identity(2)], &read, values());
    assert!(feed.value("c").is_some());
}

#[test]
fn a_read_is_checked_against_the_live_connection_not_the_last_tick() {
    let values = || HashMap::from([("c".to_owned(), checked_with_pod("web", "uid-web", &[]))]);
    let mut feed = ProblemFeed::default();
    feed.follow(vec![identity(1)]);
    let read = feed.begin_read();
    // The context moved during the read, and no tick has noted it yet.
    feed.finish(vec![identity(2)], &read, values());
    assert!(feed.value("c").is_none());
}

#[test]
fn a_read_that_outlived_a_move_and_back_is_refused() {
    let values = || HashMap::from([("c".to_owned(), checked_with_pod("web", "uid-web", &[]))]);
    let mut feed = ProblemFeed::default();
    feed.follow(vec![identity(1)]);
    let read = feed.begin_read();
    feed.follow(vec![identity(2)]);
    feed.follow(vec![identity(1)]);
    feed.finish(vec![identity(1)], &read, values());
    assert!(
        feed.value("c").is_none(),
        "A to B to A is not the same read"
    );

    let read = feed.begin_read();
    feed.finish(vec![identity(1)], &read, values());
    assert!(feed.value("c").is_some());
}

#[test]
fn namespaces_arriving_in_reverse_are_collected_in_order() {
    // Keeping the namespaces ordered on every insert shifts the whole list when they arrive in
    // reverse, which is quadratic. Inserting must build no order, and reading sorts once.
    let sorts = || super::NAMESPACE_SORTS.with(std::cell::Cell::get);
    let before = sorts();
    let mut objects = super::CountedObjects::default();
    for n in (0..20_000).rev() {
        let namespace = format!("ns-{n:05}");
        objects.insert(Some(&namespace), "pod", Some(&format!("uid-{n}")));
        objects.insert(Some(&namespace), "other", Some(&format!("uid-other-{n}")));
    }
    assert_eq!(sorts(), before, "inserting sorts nothing");
    let first = objects.namespaces().as_ptr();
    assert_eq!(
        objects.namespaces().as_ptr(),
        first,
        "read again from the cache"
    );
    assert_eq!(sorts(), before + 1, "sorted once for any number of inserts");
    let namespaces = objects.namespaces();
    assert_eq!(namespaces.len(), 20_000);
    assert!(namespaces.windows(2).all(|pair| pair[0] < pair[1]));

    objects.insert(Some("ns-new"), "pod", Some("uid-new"));
    assert_eq!(objects.namespaces().len(), 20_001);
    assert_eq!(sorts(), before + 2, "a new namespace sorts again once");
}

#[test]
fn the_fixture_has_exactly_two_problem_pods() {
    let problems = ClusterProblems::read(Some(&snapshot(
        fixture_problems(),
        Availability::Checked,
        KindCheck::Current,
    )));
    let list: serde_json::Value = serde_json::from_str(FIXTURE_PODS).unwrap();
    let filter = filter_of("fixture", problems)
        .row_filter(&KindId::pods(), "fixture")
        .unwrap();
    let mut kept: Vec<String> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pod| {
            let meta = &pod["metadata"];
            let name = meta["name"].as_str().unwrap();
            let mut listed = row(name, meta["uid"].as_str().unwrap());
            listed.namespace = meta["namespace"].as_str().map(Into::into);
            listed
        })
        .filter(|listed| filter.keeps(listed))
        .map(|listed| listed.name.to_string())
        .collect();
    kept.sort();
    assert_eq!(kept, ["broken-image", "crash-loop"]);
}

#[test]
fn a_kind_that_cannot_be_checked_keeps_its_rows_and_is_never_zero() {
    for read in [
        ClusterProblems::read(None),
        ClusterProblems::read(Some(&snapshot(
            Vec::new(),
            Availability::Partial,
            KindCheck::Starting,
        ))),
        // Last values of an unreachable cluster still say Current for each kind.
        ClusterProblems::read(Some(&snapshot(
            Vec::new(),
            Availability::CannotBeChecked(None),
            KindCheck::Current,
        ))),
    ] {
        let filter = filter_of("c", read.clone());
        let keep = filter.row_filter(&KindId::pods(), "c").unwrap();
        assert!(keep.keeps(&row("healthy", "uid")));
        assert!(!filter.is_checked(&KindId::pods(), "c"));
        let note = filter.unchecked_note(&KindId::pods(), "Pods", [("c", "c")].into_iter());
        assert!(note.unwrap().contains("cannot be checked"));
        assert_eq!(nav_badges(&[read], &[])[&KindId::pods()].text, "?");
    }
}

#[test]
fn a_checked_kind_with_no_problem_has_no_badge() {
    let clean = ClusterProblems::read(Some(&snapshot(
        Vec::new(),
        Availability::Checked,
        KindCheck::Current,
    )));
    assert!(!nav_badges(&[clean], &[]).contains_key(&KindId::pods()));
}

/// What a member's feed found for the pod `stuck`, Pending for 90 seconds: a problem past a
/// 60-second threshold, nothing under a 300-second one. Either way the pass judged it.
fn pending_member(threshold_passed: bool) -> ClusterProblems {
    let problem = Problem {
        id: ProblemId {
            cluster: "c".into(),
            kind: ProblemKind::PodPending,
            subject: ProblemSubject::Pod {
                namespace: "default".to_owned(),
                name: "stuck".to_owned(),
                uid: Some("uid-stuck".to_owned()),
            },
        },
        owner: None,
        owner_known: true,
        reason: None,
        since: None,
    };
    ClusterProblems::read(Some(&snapshot_judging(
        threshold_passed.then_some(problem).into_iter().collect(),
        Availability::Checked,
        KindCheck::Current,
        vec!["uid-stuck".to_owned()],
    )))
}

#[test]
fn each_member_of_a_set_is_filtered_by_its_own_pending_threshold() {
    let mut filter = ProblemFilter::default();
    filter.toggle();
    filter.update(HashMap::from([
        ("short".to_owned(), pending_member(true)),
        ("long".to_owned(), pending_member(false)),
    ]));
    let keep = filter
        .member_filter(Some(&KindId::pods()), ["short", "long"].into_iter())
        .unwrap();
    let stuck = row("stuck", "uid-stuck");
    assert!(keep.keeps(0, &stuck), "past the member's 60 seconds");
    assert!(!keep.keeps(1, &stuck), "under the member's 300 seconds");
    let badges = nav_badges(&[pending_member(true), pending_member(false)], &[]);
    assert_eq!(badges[&KindId::pods()].text, "1");
}

#[test]
fn a_set_counts_what_it_could_check_and_says_the_rest_is_unknown() {
    let found = ClusterProblems::read(Some(&snapshot(
        fixture_problems(),
        Availability::Checked,
        KindCheck::Current,
    )));
    let badges = nav_badges(&[found.clone(), ClusterProblems::unchecked()], &[]);
    let ProblemBadge { text, detail } = &badges[&KindId::pods()];
    assert_eq!(text, "2+");
    assert!(detail.contains("1 of 2 clusters"));

    let mut filter = ProblemFilter::default();
    filter.toggle();
    filter.update(HashMap::from([
        ("a".to_owned(), found),
        ("b".to_owned(), ClusterProblems::unchecked()),
    ]));
    let keep = filter
        .member_filter(Some(&KindId::pods()), ["a", "b"].into_iter())
        .unwrap();
    let healthy = row("healthy", "uid-healthy");
    assert!(
        !keep.keeps(0, &healthy),
        "checked member hides healthy rows"
    );
    assert!(keep.keeps(1, &healthy), "unchecked member keeps its rows");
    let note = filter
        .unchecked_note(
            &KindId::pods(),
            "Pods",
            [("a", "A"), ("b", "B")].into_iter(),
        )
        .unwrap();
    assert!(note.contains('B') && !note.contains("A,"));
}

#[test]
fn other_kinds_have_no_filter() {
    let filter = filter_of("c", ClusterProblems::unchecked());
    assert!(
        filter
            .row_filter(&KindId::new("", "configmaps"), "c")
            .is_none()
    );
    assert!(
        filter
            .row_filter(&KindId::new("example.com", "pods"), "c")
            .is_none()
    );
}

#[test]
fn filtering_ten_thousand_rows_stays_responsive() {
    let cluster: Arc<str> = "c".into();
    let problems: Vec<Problem> = (0..10_000)
        .step_by(100)
        .map(|i| Problem {
            id: ProblemId {
                cluster: Arc::clone(&cluster),
                kind: ProblemKind::CrashLoop,
                subject: ProblemSubject::Container {
                    namespace: "default".to_owned(),
                    pod: format!("pod-{i}"),
                    pod_uid: Some(format!("uid-{i}")),
                    container: "app".to_owned(),
                },
            },
            owner: None,
            owner_known: true,
            reason: None,
            since: None,
        })
        .collect();
    let base = ClusterProblems::read(Some(&snapshot_judging(
        problems,
        Availability::Checked,
        KindCheck::Current,
        (0..10_000).map(|i| format!("uid-{i}")).collect(),
    )));
    let filter = filter_of("c", base);
    let keep = filter.row_filter(&KindId::pods(), "c").unwrap();
    let rows: Vec<Arc<Row>> = (0..10_000)
        .map(|i| Arc::new(row(&format!("pod-{i}"), &format!("uid-{i}"))))
        .collect();
    let started = Instant::now();
    let kept = rows.iter().filter(|row| keep.keeps(row)).count();
    assert_eq!(kept, 100);
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[gpui_kit::test]
async fn the_toggle_on_the_fake_api_shows_exactly_the_failing_pods(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, _| {
        let path = dir.join("kubeconfig");
        server.write_kubeconfig(&path, Some("demo")).unwrap();
        vec![path]
    })
    .await;
    t.activate(cx, CONTEXT).await;
    t.wait_for_state(cx, CONTEXT, |s| matches!(s, ConnectionState::Connected(_)))
        .await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .discovered(CONTEXT)
            .is_some_and(|d| d.discovery.is_some())
    })
    .await;
    t.dispatch(
        cx,
        "workspace::OpenView",
        serde_json::json!({ "kind": "pods" }),
    )
    .unwrap();
    let list = t.list(cx).expect("a list is open");
    t.dispatch(
        cx,
        "resource_list::ToggleProblemsOnly",
        serde_json::json!({}),
    )
    .unwrap();
    let watched = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let view = watched.read(cx).view_state();
        view.problems_only && view.banners.is_empty() && view.rows.len() == 2
    })
    .await;
    let names: Vec<String> = list.read_with(cx, |list, _| {
        list.view_state().rows.into_iter().map(|r| r.name).collect()
    });
    assert_eq!(names, ["broken-image", "crash-loop"]);
}

#[gpui_kit::test]
async fn a_hidden_row_cannot_become_the_target(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, _| {
        let path = dir.join("kubeconfig");
        server.write_kubeconfig(&path, Some("demo")).unwrap();
        vec![path]
    })
    .await;
    t.activate(cx, CONTEXT).await;
    t.wait_for_state(cx, CONTEXT, |s| matches!(s, ConnectionState::Connected(_)))
        .await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .discovered(CONTEXT)
            .is_some_and(|d| d.discovery.is_some())
    })
    .await;
    t.dispatch(
        cx,
        "workspace::OpenView",
        serde_json::json!({ "kind": "pods" }),
    )
    .unwrap();
    let list = t.list(cx).expect("a list is open");
    let toggle = serde_json::json!({});
    t.dispatch(cx, "resource_list::ToggleProblemsOnly", toggle.clone())
        .unwrap();
    let watched = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let view = watched.read(cx).view_state();
        view.banners.is_empty() && view.rows.len() == 2
    })
    .await;
    let (namespace, name) = list.read_with(cx, |list, _| {
        let healthy = list
            .latest
            .rows
            .iter()
            .find(|row| !["broken-image", "crash-loop"].contains(&&*row.name))
            .expect("a healthy pod");
        (
            healthy.namespace.as_deref().map(str::to_owned),
            healthy.name.to_string(),
        )
    });

    // Asked for by name, as the resource switcher does.
    list.update(cx, |list, cx| {
        list.select_listed(namespace.clone(), name.clone(), None, cx);
        assert!(list.selected.is_none(), "a row the filter hides");
        let note = list.problems_note().expect("the list says why");
        assert!(note.contains(&name) && note.contains("Problems"), "{note}");
    });

    t.dispatch(cx, "resource_list::ToggleProblemsOnly", toggle.clone())
        .unwrap();
    let watched = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        watched.read(cx).view_state().rows.len() > 2
    })
    .await;
    let select = serde_json::json!({ "namespace": namespace, "name": name });
    t.dispatch(cx, "resource_list::SelectRow", select.clone())
        .unwrap();
    assert!(list.read_with(cx, |list, _| list.selected.is_some()));

    // While the filtered rows are still being computed.
    let permits = list.update(cx, |list, _| list.hold_computes());
    t.dispatch(cx, "resource_list::ToggleProblemsOnly", toggle)
        .unwrap();
    assert!(
        list.read_with(cx, |list, _| list.selected.is_none()),
        "cleared before the new rows are published"
    );
    t.dispatch(cx, "resource_list::SelectRow", select).unwrap();
    assert!(
        list.read_with(cx, |list, _| list.selected.is_none()),
        "a row still drawn but already filtered out"
    );
    drop(permits);
}

#[gpui_kit::test]
async fn turning_problems_on_closes_a_row_it_hides(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, _| {
        let path = dir.join("kubeconfig");
        server.write_kubeconfig(&path, Some("demo")).unwrap();
        vec![path]
    })
    .await;
    t.activate(cx, CONTEXT).await;
    t.wait_for_state(cx, CONTEXT, |s| matches!(s, ConnectionState::Connected(_)))
        .await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .discovered(CONTEXT)
            .is_some_and(|d| d.discovery.is_some())
    })
    .await;
    t.dispatch(
        cx,
        "workspace::OpenView",
        serde_json::json!({ "kind": "pods" }),
    )
    .unwrap();
    let list = t.list(cx).expect("a list is open");
    let watched = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        watched.read(cx).view_state().rows.len() > 2
    })
    .await;
    let (namespace, name) = list.read_with(cx, |list, _| {
        let healthy = list
            .shown
            .rows
            .iter()
            .find(|row| !["broken-image", "crash-loop"].contains(&&*row.name))
            .expect("a healthy pod");
        (
            healthy.namespace.as_deref().map(str::to_owned),
            healthy.name.to_string(),
        )
    });
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        serde_json::json!({ "namespace": namespace, "name": name }),
    )
    .unwrap();
    assert!(list.read_with(cx, |list, _| list.selected.is_some()));
    t.dispatch(
        cx,
        "resource_list::ToggleProblemsOnly",
        serde_json::json!({}),
    )
    .unwrap();
    let watched = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let list = watched.read(cx);
        list.view_state().rows.len() == 2 && list.selected.is_none()
    })
    .await;
}
