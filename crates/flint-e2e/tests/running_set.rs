//! The running set against fake API servers: members connect and watch side by side, one's
//! failure or hang leaves the others live, and leaving the set or moving to another server stops
//! a member's watches.

use std::path::PathBuf;
use std::time::Duration;

use flint_core::handle::{Command, CoreHandle, MemberStatus, Subscription};
use flint_core::settings::ClusterPin;
use flint_core::{KindId, Settings, SyncState};
use flint_e2e::fake_api::{FakeApiServer, FakeRow, Fault, Gvr, WatchStep, write_kubeconfigs};

#[allow(dead_code)]
mod core_support;

use core_support::{Core, WAIT};

fn pods() -> Gvr {
    Gvr::new("", "v1", "pods")
}

fn pod_kind() -> KindId {
    KindId::new("", "pods")
}

async fn servers<const N: usize>() -> [FakeApiServer; N] {
    let mut started = Vec::with_capacity(N);
    for _ in 0..N {
        started.push(FakeApiServer::start().await.unwrap());
    }
    started.try_into().ok().unwrap()
}

/// A core with one context per server, named `a`, `b`, ..., and `a` active.
async fn core_on(servers: &[&FakeApiServer]) -> (Core, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    write_config(&path, servers);
    let core = Core::start_on_context(dir, vec![path.clone()], Settings::default(), "a").await;
    (core, path)
}

fn write_config(path: &std::path::Path, servers: &[&FakeApiServer]) {
    const NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];
    let entries: Vec<_> = servers
        .iter()
        .zip(NAMES)
        .map(|(server, name)| (*server, name, None))
        .collect();
    write_kubeconfigs(path, &entries).unwrap();
}

fn set(handle: &CoreHandle, names: &[&str]) {
    handle.send(Command::SetRunningSet(
        names.iter().map(|n| (*n).to_owned()).collect(),
    ));
}

async fn until_status(handle: &CoreHandle, cluster: &str, wanted: impl Fn(&MemberStatus) -> bool) {
    tokio::time::timeout(WAIT, async {
        loop {
            let reports = handle.running_set().await;
            if reports
                .iter()
                .any(|r| r.cluster == cluster && wanted(&r.status))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{cluster} did not reach the state in time"));
}

async fn until_no_watches(server: &FakeApiServer) {
    tokio::time::timeout(WAIT, async {
        while server.open_watches(&pods()) > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the watch should have stopped");
}

#[tokio::test]
async fn members_connect_and_watch_side_by_side() {
    let [a, b, c] = servers().await;
    let (core, _path) = core_on(&[&a, &b, &c]).await;
    let _on_b = core.handle.subscribe("b", &pod_kind());
    let _on_c = core.handle.subscribe("c", &pod_kind());

    set(&core.handle, &["b", "c"]);
    until_status(&core.handle, "b", |s| *s == MemberStatus::Live).await;
    until_status(&core.handle, "c", |s| *s == MemberStatus::Live).await;
    b.wait_for_watches(&pods(), 1, WAIT).await.unwrap();
    c.wait_for_watches(&pods(), 1, WAIT).await.unwrap();
}

#[tokio::test]
async fn a_failing_and_a_hung_member_leave_another_live() {
    let [a, live, failing, hung] = servers().await;
    failing.inject(Fault::status(500));
    let (fault, hold) = Fault::hold();
    hung.inject(fault);
    let (core, _path) = core_on(&[&a, &live, &failing, &hung]).await;
    let _on_live = core.handle.subscribe("b", &pod_kind());

    // The hung member goes first, and its request is on the server before anything is judged.
    set(&core.handle, &["d", "b", "c"]);
    tokio::time::timeout(WAIT, hold.reached())
        .await
        .expect("the hung member's request should reach its server");
    until_status(&core.handle, "b", |s| *s == MemberStatus::Live).await;
    live.wait_for_watches(&pods(), 1, WAIT).await.unwrap();
    until_status(&core.handle, "c", |s| {
        matches!(s, MemberStatus::Failed { .. })
    })
    .await;
    let reports = core.handle.running_set().await;
    let d = reports.iter().find(|r| r.cluster == "d").unwrap();
    assert_eq!(d.status, MemberStatus::Connecting, "still hanging");
    assert_eq!(live.open_watches(&pods()), 1, "the live member goes on");
    hold.release();
}

#[tokio::test]
async fn a_background_watch_refused_with_401_needs_sign_in() {
    let [a, b] = servers().await;
    let (core, _path) = core_on(&[&a, &b]).await;
    let _on_b = core.handle.subscribe("b", &pod_kind());
    set(&core.handle, &["a", "b"]);
    until_status(&core.handle, "b", |s| *s == MemberStatus::Live).await;
    b.wait_for_watches(&pods(), 1, WAIT).await.unwrap();

    b.inject(Fault::status(401));
    b.send_watch(&pods(), WatchStep::End);

    until_status(&core.handle, "b", |s| {
        matches!(s, MemberStatus::SignInNeeded { .. })
    })
    .await;
    until_no_watches(&b).await;
}

#[tokio::test]
async fn replacing_the_set_stops_a_removed_members_watches() {
    let [a, b, c] = servers().await;
    let (core, _path) = core_on(&[&a, &b, &c]).await;
    let _on_b = core.handle.subscribe("b", &pod_kind());
    let _on_c = core.handle.subscribe("c", &pod_kind());
    set(&core.handle, &["a", "b"]);
    b.wait_for_watches(&pods(), 1, WAIT).await.unwrap();

    set(&core.handle, &["a", "c"]);
    until_no_watches(&b).await;
    c.wait_for_watches(&pods(), 1, WAIT).await.unwrap();
    let names: Vec<_> = core
        .handle
        .running_set()
        .await
        .into_iter()
        .map(|r| r.cluster)
        .collect();
    assert_eq!(names, ["a", "c"]);

    set(&core.handle, &[]);
    until_no_watches(&c).await;
    assert!(core.handle.running_set().await.is_empty());
}

#[tokio::test]
async fn a_repointed_member_leaves_and_never_reaches_the_new_server() {
    let [a, b, elsewhere] = servers().await;
    let (core, path) = core_on(&[&a, &b]).await;
    let _on_b = core.handle.subscribe("b", &pod_kind());
    set(&core.handle, &["a", "b"]);
    until_status(&core.handle, "b", |s| *s == MemberStatus::Live).await;
    b.wait_for_watches(&pods(), 1, WAIT).await.unwrap();

    write_config(&path, &[&a, &elsewhere]);
    core.handle.reload_kubeconfig().await.unwrap();

    until_no_watches(&b).await;
    let reports = core.handle.running_set().await;
    assert!(reports.iter().all(|r| r.cluster != "b"), "{reports:?}");
    assert!(
        elsewhere.requests().is_empty(),
        "nothing connects to the new server"
    );
}

#[tokio::test]
async fn a_combined_view_sends_nothing_to_the_server_its_active_member_moved_to() {
    let [a, b, elsewhere] = servers().await;
    let (core, path) = core_on(&[&a, &b]).await;
    let pins = vec![ClusterPin::new("a", a.url()), ClusterPin::new("b", b.url())];
    let admitted = core.handle.admit_running_set(pins).await.unwrap();
    let member = admitted.into_iter().find(|m| m.context() == "a").unwrap();
    let combined = core.handle.bound_to(member);
    let _view = combined.subscribe("a", &pod_kind());
    a.wait_for_watches(&pods(), 1, WAIT).await.unwrap();

    write_config(&path, &[&elsewhere, &b]);
    core.handle.reload_kubeconfig().await.unwrap();

    // The active cluster's own view does reach the new server, so its watches have been decided.
    let services = elsewhere.gvr("services");
    let _own_view = core.handle.subscribe("a", &KindId::new("", "services"));
    elsewhere
        .wait_for_watches(&services, 1, WAIT)
        .await
        .unwrap();
    until_no_watches(&a).await;
    let reached: Vec<_> = elsewhere
        .requests()
        .into_iter()
        .filter(|r| r.is_about(&pods()))
        .map(|r| r.path)
        .collect();
    assert!(reached.is_empty(), "{reached:?}");
}

fn pod_row(name: &str) -> FakeRow {
    FakeRow::build(Some("default"), name, vec![serde_json::json!(name)])
}

async fn until_row(view: &mut Subscription, name: &str) {
    tokio::time::timeout(WAIT, async {
        loop {
            if view.latest().names().contains(&name) {
                return;
            }
            view.changed().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name} did not arrive"));
}

#[tokio::test]
async fn a_member_view_never_shows_the_server_its_context_moved_to() {
    let [a, b, elsewhere] = servers().await;
    a.set_row(&pods(), pod_row("admitted-pod"));
    elsewhere.set_row(&pods(), pod_row("replacement-pod"));
    let (core, path) = core_on(&[&a, &b]).await;
    let pins = vec![ClusterPin::new("a", a.url()), ClusterPin::new("b", b.url())];
    let admitted = core.handle.admit_running_set(pins).await.unwrap();
    let member = admitted.into_iter().find(|m| m.context() == "a").unwrap();
    let combined = core.handle.bound_to(member);
    let mut member_view = combined.subscribe("a", &pod_kind());
    until_row(&mut member_view, "admitted-pod").await;

    write_config(&path, &[&elsewhere, &b]);
    core.handle.reload_kubeconfig().await.unwrap();
    let mut plain_view = core.handle.subscribe("a", &pod_kind());
    until_row(&mut plain_view, "replacement-pod").await;

    let mut refused = combined.subscribe("a", &pod_kind());
    assert!(refused.changed().await, "the first read");
    // Answered after the refused subscription was handled.
    core.handle.running_set().await;
    for snapshot in [member_view.latest(), refused.latest()] {
        assert!(snapshot.rows.is_empty(), "{:?}", snapshot.names());
        assert_eq!(snapshot.state, SyncState::Idle);
    }
    assert!(plain_view.latest().names().contains(&"replacement-pod"));
}

fn unserved_kind() -> KindId {
    KindId::new("example.io", "widgets")
}

async fn until_state(view: &mut Subscription, state: SyncState) {
    tokio::time::timeout(WAIT, async {
        while view.latest().state != state {
            view.changed().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the view did not reach {state:?}"));
}

#[tokio::test]
async fn a_member_view_inherits_nothing_from_the_previous_server() {
    let [a, b, replacement] = servers().await;
    let (core, path) = core_on(&[&a, &b]).await;
    let mut plain = core.handle.subscribe("a", &unserved_kind());
    until_state(&mut plain, SyncState::Unavailable).await;

    // The new server answers nothing yet, so its discovery can't have decided the kind.
    let (fault, hold) = Fault::hold();
    replacement.inject(fault);
    write_config(&path, &[&replacement, &b]);
    core.handle.reload_kubeconfig().await.unwrap();
    let pins = vec![
        ClusterPin::new("a", replacement.url()),
        ClusterPin::new("b", b.url()),
    ];
    let admitted = core.handle.admit_running_set(pins).await.unwrap();
    let member = admitted.into_iter().find(|m| m.context() == "a").unwrap();
    let combined = core.handle.bound_to(member);
    let mut bound = combined.subscribe("a", &unserved_kind());
    assert!(bound.changed().await, "the first read");
    // Answered after the subscription was handled.
    core.handle.running_set().await;
    assert_eq!(bound.latest().state, SyncState::Idle);

    hold.release();
    until_state(&mut plain, SyncState::Unavailable).await;
    until_state(&mut bound, SyncState::Unavailable).await;
}

#[tokio::test]
async fn a_member_view_cut_off_on_a_fallback_stays_cut_off_when_restored() {
    let [a, b, fallback] = servers().await;
    a.set_row(&pods(), pod_row("admitted-pod"));
    let dir = tempfile::tempdir().unwrap();
    let primary = dir.path().join("primary");
    let secondary = dir.path().join("secondary");
    write_config(&primary, &[&a, &b]);
    write_config(&secondary, &[&fallback, &b]);
    let restored = std::fs::read(&primary).unwrap();
    let core = Core::start_on_context(
        dir,
        vec![primary.clone(), secondary],
        Settings::default(),
        "a",
    )
    .await;
    let pins = vec![ClusterPin::new("a", a.url()), ClusterPin::new("b", b.url())];
    let admitted = core.handle.admit_running_set(pins).await.unwrap();
    let member = admitted.into_iter().find(|m| m.context() == "a").unwrap();
    let combined = core.handle.bound_to(member);
    let mut view = combined.subscribe("a", &pod_kind());
    until_row(&mut view, "admitted-pod").await;

    std::fs::write(&primary, "not: [valid").unwrap();
    core.handle.reload_kubeconfig().await.unwrap();
    until_no_watches(&a).await;
    std::fs::write(&primary, restored).unwrap();
    core.handle.reload_kubeconfig().await.unwrap();

    // The context's own view reaches its server again, so its watches have been decided.
    let services = a.gvr("services");
    let _own_view = core.handle.subscribe("a", &KindId::new("", "services"));
    a.wait_for_watches(&services, 1, WAIT).await.unwrap();
    assert_eq!(a.open_watches(&pods()), 0, "the cut view asks for nothing");
    let snapshot = view.latest();
    assert!(snapshot.rows.is_empty(), "{:?}", snapshot.names());
    assert_eq!(snapshot.state, SyncState::Idle);
}
