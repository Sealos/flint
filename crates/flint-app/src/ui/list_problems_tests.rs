//! The Problems filter against the fake API fixture, and what it says when it cannot check.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flint_core::dashboard::{ProblemSnapshot, ProblemSummariser};
use flint_core::problems::{
    Classifier, Problem, ProblemId, ProblemKind, ProblemSubject, ReplicaSetIndex,
};
use flint_core::telemetry::{Availability, KindCheck, TelemetrySnapshot, WatchedKind};
use flint_core::{Cell, ConnectionState, KindId, Row};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Options};
use gpui_kit::TestAppContext;
use gpui_kit::test::TestAppContextExt as _;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;

use super::problems::{ClusterProblems, ProblemBadge, ProblemFilter, nav_badges};
use crate::ui::support_tests::open_workspace_with;

const WAIT: Duration = Duration::from_secs(30);
const FIXTURE_PODS: &str = include_str!("../../../flint-e2e/fixtures/objects/core/v1/pods.json");

fn fixture_problems() -> Vec<Problem> {
    let list: serde_json::Value = serde_json::from_str(FIXTURE_PODS).unwrap();
    let pods: Vec<Pod> = serde_json::from_value(list["items"].clone()).unwrap();
    let now: Timestamp = "2026-01-01T01:00:00Z".parse().unwrap();
    Classifier::new("fixture", now).pods_with_replica_sets(&pods, &ReplicaSetIndex::unavailable())
}

fn snapshot(
    problems: Vec<Problem>,
    availability: Availability,
    pods: KindCheck,
) -> TelemetrySnapshot {
    let at = Timestamp::UNIX_EPOCH;
    let mut kinds = WatchedKind::ALL.map(|kind| (kind, KindCheck::Current));
    kinds[WatchedKind::Pods as usize].1 = pods;
    TelemetrySnapshot {
        cluster: "fixture".into(),
        at,
        availability,
        kinds,
        problems: problems.into(),
        unchecked: Vec::new(),
        settling: Vec::new(),
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

/// A cluster checked in full, whose one problem is the pod `name` with `uid`.
pub(super) fn checked_with_pod(name: &str, uid: &str) -> ClusterProblems {
    let problem = Problem {
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
    };
    ClusterProblems::read(Some(&snapshot(
        vec![problem],
        Availability::Checked,
        KindCheck::Current,
    )))
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
        assert_eq!(nav_badges(&[read])[&KindId::pods()].text, "?");
    }
}

#[test]
fn a_checked_kind_with_no_problem_has_no_badge() {
    let clean = ClusterProblems::read(Some(&snapshot(
        Vec::new(),
        Availability::Checked,
        KindCheck::Current,
    )));
    assert!(!nav_badges(&[clean]).contains_key(&KindId::pods()));
}

#[test]
fn a_set_counts_what_it_could_check_and_says_the_rest_is_unknown() {
    let found = ClusterProblems::read(Some(&snapshot(
        fixture_problems(),
        Availability::Checked,
        KindCheck::Current,
    )));
    let badges = nav_badges(&[found.clone(), ClusterProblems::unchecked()]);
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
    let base = ClusterProblems::read(Some(&snapshot(
        problems,
        Availability::Checked,
        KindCheck::Current,
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
