//! Which port-forwards outlive a change of cluster, against fake API servers: one on a cluster
//! kept watching does, and it ends with its connection, definition or the keep-watching choice.

use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flint_core::handle::{Command, Event};
use flint_core::keep_watching::KeepWatching;
use flint_core::port_forward::{
    ForwardRequest, ForwardSnapshot, ForwardState, ForwardTarget, PortForward, wait_for,
};
use flint_core::settings::ClusterPin;
use flint_core::{CoreHandle, KindId, Settings};
use flint_e2e::fake_api::{FakeApiServer, Fault, Gvr, WatchStep};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

#[allow(dead_code)]
mod core_support;

use core_support::{Core, WAIT};

const WEB: &str = "web-69c6f74b8b-qv4xv";
/// Longer than a running forward's periodic check of its gate.
const SETTLE: Duration = Duration::from_millis(2500);

/// A kubeconfig with context `a` on `a`, and `b` on `b`. `user`, if any, is `a`'s.
fn config(a: &FakeApiServer, b: &FakeApiServer, user: Option<serde_json::Value>) -> String {
    let mut merged = serde_json::to_value(a.kubeconfig_named("a", Some("demo"))).unwrap();
    if let Some(user) = user {
        merged["users"][0]["user"] = user;
    }
    let mut other = serde_json::to_value(b.kubeconfig_named("b", Some("demo"))).unwrap();
    other["users"][0]["name"] = "b-user".into();
    other["clusters"][0]["name"] = "b-cluster".into();
    other["contexts"][0]["context"]["user"] = "b-user".into();
    other["contexts"][0]["context"]["cluster"] = "b-cluster".into();
    for key in ["users", "clusters", "contexts"] {
        let entry = other[key][0].clone();
        merged[key].as_array_mut().unwrap().push(entry);
    }
    merged.to_string()
}

struct Fixture {
    core: Core,
    a: FakeApiServer,
    b: FakeApiServer,
    file: PathBuf,
}

async fn fixture_with(user: impl FnOnce(&Path) -> Option<serde_json::Value>) -> Fixture {
    let a = FakeApiServer::start().await.unwrap();
    let b = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let user = user(dir.path());
    let file = dir.path().join("kubeconfig");
    std::fs::write(&file, config(&a, &b, user)).unwrap();
    let core = Core::start_on_context(dir, vec![file.clone()], Settings::default(), "a").await;
    // Its name is not a local cluster's, so it starts protected.
    core.handle.unlock("a", &a.url());
    Fixture { core, a, b, file }
}

async fn fixture() -> Fixture {
    fixture_with(|_| None).await
}

fn request() -> ForwardRequest {
    ForwardRequest {
        cluster: "a".into(),
        namespace: "demo".into(),
        target: ForwardTarget::Pod(WEB.into()),
        remote_port: 8080,
        local_port: None,
        server: None,
        uid: None,
    }
}

async fn until(forward: &PortForward, check: impl Fn(&ForwardSnapshot) -> bool) -> ForwardSnapshot {
    let mut updates = forward.updates();
    tokio::time::timeout(WAIT, wait_for(&mut updates, check))
        .await
        .expect("the forward's state")
}

async fn listening(forward: &PortForward) -> u16 {
    let shown = until(forward, |s| s.state != ForwardState::Starting).await;
    assert_eq!(shown.state, ForwardState::Listening, "{shown:?}");
    shown.local_port.expect("a local port")
}

async fn echoes(port: u16) -> TcpStream {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut back = [0; 4];
    tokio::time::timeout(WAIT, stream.read_exact(&mut back))
        .await
        .expect("an answer")
        .unwrap();
    assert_eq!(&back, b"ping");
    stream
}

async fn ended(forward: &PortForward, stream: &mut TcpStream) {
    until(forward, |s| s.state.is_finished()).await;
    let mut buf = [0; 8];
    let read = tokio::time::timeout(WAIT, stream.read(&mut buf))
        .await
        .expect("the connection ended");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
}

async fn keep_status(handle: &CoreHandle) -> KeepWatching {
    handle.keep_watching().await["a"].status
}

async fn keep_a(handle: &CoreHandle) {
    let definition = handle.keep_watching().await["a"].definition;
    handle.send(Command::SetKeepWatching {
        cluster: "a".to_owned(),
        definition,
    });
    tokio::time::timeout(WAIT, async {
        while keep_status(handle).await != KeepWatching::Watching {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("keep watching should turn on");
}

async fn switch_to(handle: &CoreHandle, cluster: &str) {
    let events = handle.events();
    handle.send(Command::Activate(cluster.to_owned()));
    tokio::time::timeout(WAIT, async {
        loop {
            if let Event::ActiveChanged(Some(active)) = events.recv().await.expect("core running")
                && active == cluster
            {
                return;
            }
        }
    })
    .await
    .expect("the switch");
}

// covers UP-24: without keep watching, a forward stops on a switch, as Stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_stops_a_forward_not_kept() {
    let f = fixture().await;
    let forward = f.core.handle.port_forward(request());
    let port = listening(&forward).await;
    let mut stream = echoes(port).await;
    switch_to(&f.core.handle, "b").await;
    ended(&forward, &mut stream).await;
    assert_eq!(forward.snapshot().state, ForwardState::Stopped);
}

// covers UP-24: on a cluster kept watching, it keeps carrying bytes, old and new connections.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_forward_survives_a_switch() {
    let f = fixture().await;
    keep_a(&f.core.handle).await;
    let forward = f.core.handle.port_forward(request());
    let port = listening(&forward).await;
    let mut stream = echoes(port).await;
    switch_to(&f.core.handle, "b").await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(forward.snapshot().state, ForwardState::Listening);
    stream.write_all(b"pong").await.unwrap();
    let mut back = [0; 4];
    tokio::time::timeout(WAIT, stream.read_exact(&mut back))
        .await
        .expect("an answer")
        .unwrap();
    let _new = echoes(port).await;
    assert!(f.b.forward_requests().is_empty());
}

// covers UP-06: re-pointed and put back while in the background, it ends and reaches neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_forward_stops_on_a_to_b_to_a() {
    let f = fixture().await;
    keep_a(&f.core.handle).await;
    let forward = f.core.handle.port_forward(request());
    let port = listening(&forward).await;
    let mut stream = echoes(port).await;
    switch_to(&f.core.handle, "b").await;
    let opened = f.a.forward_requests().len();

    let elsewhere = FakeApiServer::start().await.unwrap();
    std::fs::write(&f.file, config(&elsewhere, &f.b, None)).unwrap();
    f.core.handle.reload_kubeconfig().await.unwrap();
    std::fs::write(&f.file, config(&f.a, &f.b, None)).unwrap();
    f.core.handle.reload_kubeconfig().await.unwrap();

    ended(&forward, &mut stream).await;
    assert!(
        TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_err()
    );
    assert!(elsewhere.forward_requests().is_empty());
    assert_eq!(f.a.forward_requests().len(), opened);
}

// Turning keep watching off ends it, on the active cluster and in the background alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_keep_watching_off_stops_the_forward() {
    let f = fixture().await;
    keep_a(&f.core.handle).await;
    let active = f.core.handle.port_forward(request());
    let mut on_active = echoes(listening(&active).await).await;
    let off = Command::SetKeepWatching {
        cluster: "a".to_owned(),
        definition: None,
    };
    f.core.handle.send(off.clone());
    ended(&active, &mut on_active).await;

    keep_a(&f.core.handle).await;
    let kept = f.core.handle.port_forward(request());
    let mut in_background = echoes(listening(&kept).await).await;
    switch_to(&f.core.handle, "b").await;
    f.core.handle.send(off);
    ended(&kept, &mut in_background).await;
}

// A member signed out after a 401 loses its connection, and the forward goes with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_out_ends_the_forward() {
    let f = fixture().await;
    let pins = vec![
        ClusterPin::new("a", f.a.url()),
        ClusterPin::new("b", f.b.url()),
    ];
    let _watching = f.core.handle.subscribe("a", &KindId::new("", "pods"));
    let admitted = f.core.handle.admit_running_set(pins).await.unwrap();
    let member = admitted
        .iter()
        .find(|m| m.context() == "a")
        .unwrap()
        .clone();
    switch_to(&f.core.handle, "b").await;
    let forward = f.core.handle.bound_to(member).port_forward(request());
    let port = listening(&forward).await;
    let mut stream = echoes(port).await;

    let pods = Gvr::new("", "v1", "pods");
    f.a.wait_for_watches(&pods, 1, WAIT).await.unwrap();
    f.a.inject(Fault::status(401));
    f.a.send_watch(&pods, WatchStep::End);
    ended(&forward, &mut stream).await;
}

// Closing the set leaves a member that is not active with no reason to carry a forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_set_stops_a_background_members_forward() {
    let f = fixture().await;
    let pins = vec![
        ClusterPin::new("a", f.a.url()),
        ClusterPin::new("b", f.b.url()),
    ];
    let admitted = f.core.handle.admit_running_set(pins).await.unwrap();
    let member = admitted
        .iter()
        .find(|m| m.context() == "a")
        .unwrap()
        .clone();
    switch_to(&f.core.handle, "b").await;
    let forward = f.core.handle.bound_to(member).port_forward(request());
    let mut stream = echoes(listening(&forward).await).await;
    f.core.handle.admit_running_set(Vec::new()).await.unwrap();
    ended(&forward, &mut stream).await;
    assert_eq!(forward.snapshot().state, ForwardState::Stopped);
}

/// A plugin that records each run and prints a token.
fn plugin_user(dir: &Path) -> serde_json::Value {
    let script = dir.join("plugin.sh");
    let text = format!(
        "#!/bin/sh\necho run >> '{}'\nprintf '{{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
         \"kind\":\"ExecCredential\",\"status\":{{\"token\":\"t\"}}}}'\n",
        dir.join("plugin-runs").display()
    );
    std::fs::write(&script, text).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    serde_json::json!({"exec": {
        "apiVersion": "client.authentication.k8s.io/v1",
        "command": script.to_str().unwrap(),
        "interactiveMode": "Never",
    }})
}

fn plugin_runs(core: &Core) -> usize {
    std::fs::read_to_string(core.dir.path().join("plugin-runs"))
        .map_or(0, |text| text.lines().count())
}

// A kept forward's new connection in the background launches nothing, and revoking the
// background sign-in ends it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_plugin_forward_launches_nothing_and_stops_on_revoke() {
    let f = fixture_with(|dir| Some(plugin_user(dir))).await;
    let handle = &f.core.handle;
    let definition = handle.keep_watching().await["a"].definition;
    handle.send(Command::SetKeepWatching {
        cluster: "a".to_owned(),
        definition,
    });
    let shown = tokio::time::timeout(WAIT, async {
        loop {
            let view = handle.keep_watching().await["a"].clone();
            if view.status != KeepWatching::Off
                && let Some(shown) = view.shown_sign_in
            {
                return shown;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the toggle");
    let offer = handle.offer_keep_watching("a", shown).await.unwrap();
    handle.send(Command::ApproveKeepWatching(offer.approve()));
    tokio::time::timeout(WAIT, async {
        while keep_status(handle).await != KeepWatching::Watching {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watching");

    let forward = handle.port_forward(request());
    let port = listening(&forward).await;
    let mut stream = echoes(port).await;
    switch_to(handle, "b").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let runs = plugin_runs(&f.core);
    let opened = f.a.forward_requests().len();

    let _new = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    until(&forward, |s| s.warning.is_some()).await;
    assert_eq!(forward.snapshot().state, ForwardState::Listening);
    assert_eq!(f.a.forward_requests().len(), opened, "nothing was opened");

    handle.send(Command::RevokeBackgroundSignIn("a".to_owned()));
    ended(&forward, &mut stream).await;
    // The kept feed may have started before the revoke; this forward itself ran nothing more.
    assert!(plugin_runs(&f.core) <= runs + 1);
}

/// Holds the settings file's save lock until dropped, with a save waiting for it, so the
/// manager's queue is stuck.
async fn stall_the_manager(f: &Fixture) -> std::fs::File {
    use std::os::fd::AsRawFd as _;
    let config_dir = f.core.dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let lock = std::fs::File::create(config_dir.join("settings.toml.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let definition = f.core.handle.keep_watching().await["a"].definition;
    f.core.handle.send(Command::SetKeepWatching {
        cluster: "a".to_owned(),
        definition,
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    lock
}

/// A kept forward with an open connection, on a manager stuck behind a save.
async fn kept_forward_on_a_stuck_manager(f: &Fixture) -> (PortForward, TcpStream, std::fs::File) {
    keep_a(&f.core.handle).await;
    let forward = f.core.handle.port_forward(request());
    let stream = echoes(listening(&forward).await).await;
    let lock = stall_the_manager(f).await;
    (forward, stream, lock)
}

async fn ended_at_once(forward: &PortForward, stream: &mut TcpStream) {
    tokio::time::timeout(Duration::from_secs(1), ended(forward, stream))
        .await
        .expect("it should not wait for the manager");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_watching_off_ends_a_forward_without_waiting_for_a_save() {
    let f = fixture().await;
    let (forward, mut stream, _lock) = kept_forward_on_a_stuck_manager(&f).await;
    f.core.handle.send(Command::SetKeepWatching {
        cluster: "a".to_owned(),
        definition: None,
    });
    ended_at_once(&forward, &mut stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_consent_ends_a_forward_without_waiting_for_a_save() {
    let f = fixture().await;
    let (forward, mut stream, _lock) = kept_forward_on_a_stuck_manager(&f).await;
    f.core
        .handle
        .send(Command::RevokeBackgroundSignIn("a".to_owned()));
    ended_at_once(&forward, &mut stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quitting_ends_a_forward_without_waiting_for_a_save() {
    let f = fixture().await;
    let (forward, mut stream, _lock) = kept_forward_on_a_stuck_manager(&f).await;
    let _quit = f.core.handle.stop_keep_watching();
    ended_at_once(&forward, &mut stream).await;
}

// A re-point while the pod is being looked up, before any reload has seen it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_re_point_during_lookup_sends_no_upgrade() {
    let f = fixture().await;
    let forward = f.core.handle.port_forward(request());
    let port = listening(&forward).await;
    let (fault, hold) = Fault::hold();
    f.a.inject(fault.on(&Gvr::core("pods")).named(WEB).times(1));
    let _stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    hold.reached().await;

    let elsewhere = FakeApiServer::start().await.unwrap();
    std::fs::write(&f.file, config(&elsewhere, &f.b, None)).unwrap();
    hold.release();

    until(&forward, |s| s.state.is_finished()).await;
    assert!(f.a.forward_requests().is_empty());
    assert!(elsewhere.forward_requests().is_empty());
}

// Both edits land inside one watcher debounce, so the read after it finds the same definition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_to_b_to_a_within_one_debounce_stops_the_forward() {
    let a = FakeApiServer::start().await.unwrap();
    let b = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("kubeconfig");
    std::fs::write(&file, config(&a, &b, None)).unwrap();
    let core = Core::start_watching_files(dir, vec![file.clone()], "a").await;
    core.handle.unlock("a", &a.url());
    // The watches start on their own thread, and report what predates them.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let forward = core.handle.port_forward(request());
    let mut stream = echoes(listening(&forward).await).await;

    let elsewhere = FakeApiServer::start().await.unwrap();
    std::fs::write(&file, config(&elsewhere, &b, None)).unwrap();
    std::fs::write(&file, config(&a, &b, None)).unwrap();

    ended(&forward, &mut stream).await;
    assert!(elsewhere.forward_requests().is_empty());
}

// A refused sign-in on one connection says the credential is no good for any of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_answered_401_ends_every_forward_on_the_connection() {
    let f = fixture().await;
    let first = f.core.handle.port_forward(request());
    let mut first_stream = echoes(listening(&first).await).await;
    let second = f.core.handle.port_forward(request());
    let second_port = listening(&second).await;
    let mut second_stream = echoes(second_port).await;

    f.a.inject(Fault::status(401).subresource("portforward"));
    let _refused = TcpStream::connect((Ipv4Addr::LOCALHOST, second_port))
        .await
        .unwrap();

    ended(&second, &mut second_stream).await;
    ended(&first, &mut first_stream).await;
    assert!(matches!(first.snapshot().state, ForwardState::Failed(_)));
}

// The refusal reaches the connection's other forwards with the manager stuck behind a save.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_sign_in_ends_forwards_without_waiting_for_a_save() {
    let f = fixture().await;
    keep_a(&f.core.handle).await;
    let first = f.core.handle.port_forward(request());
    let mut first_stream = echoes(listening(&first).await).await;
    let second = f.core.handle.port_forward(request());
    let second_port = listening(&second).await;

    let (fault, hold) = Fault::hold();
    f.a.inject(fault.subresource("portforward").times(1));
    let _refused = TcpStream::connect((Ipv4Addr::LOCALHOST, second_port))
        .await
        .unwrap();
    hold.reached().await;
    let _lock = stall_the_manager(&f).await;
    f.a.inject(Fault::status(401).on(&Gvr::core("pods")).named(WEB));
    hold.release();

    ended_at_once(&first, &mut first_stream).await;
    assert!(matches!(first.snapshot().state, ForwardState::Failed(_)));
}

// The slice moves off the pod during the upgrade, and the pod itself lives on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backend_withdrawn_during_the_upgrade_carries_nothing() {
    let f = fixture().await;
    let forward = f.core.handle.port_forward(ForwardRequest {
        target: ForwardTarget::Service("web".into()),
        remote_port: 80,
        ..request()
    });
    let port = listening(&forward).await;
    let (fault, hold) = Fault::hold();
    f.a.inject(fault.subresource("portforward").times(1));
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    hold.reached().await;

    let slices = Gvr::new("discovery.k8s.io", "v1", "endpointslices");
    let mut slice = f.a.object(&slices, Some("demo"), "web-8wjpf").unwrap();
    slice["endpoints"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["targetRef"]["name"] != WEB);
    f.a.set_object(&slices, slice);
    hold.release();

    let mut buf = [0; 4];
    let read = tokio::time::timeout(WAIT, stream.read(&mut buf))
        .await
        .expect("the connection ended");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
}

// Closed before the forward is admitted: the member's old binding admits nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_sets_member_admits_no_forward() {
    let f = fixture().await;
    let pins = vec![
        ClusterPin::new("a", f.a.url()),
        ClusterPin::new("b", f.b.url()),
    ];
    let admitted = f.core.handle.admit_running_set(pins).await.unwrap();
    let member = admitted
        .iter()
        .find(|m| m.context() == "a")
        .unwrap()
        .clone();
    switch_to(&f.core.handle, "b").await;
    f.core.handle.admit_running_set(Vec::new()).await.unwrap();

    let forward = f.core.handle.bound_to(member).port_forward(request());
    let shown = until(&forward, |s| s.state != ForwardState::Starting).await;
    assert!(matches!(shown.state, ForwardState::Failed(_)), "{shown:?}");
    assert_eq!(shown.local_port, None);
}
