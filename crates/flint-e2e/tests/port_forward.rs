//! Port-forwards through the public `CoreHandle` API against the fake API server: the same gate
//! as a shell, bytes across a real WebSocket, and a forward that ends with its permission.

use std::net::Ipv4Addr;
use std::time::Duration;

use flint_core::Settings;
use flint_core::port_forward::{
    ForwardFailureKind, ForwardRequest, ForwardSnapshot, ForwardState, ForwardTarget, PortForward,
    wait_for,
};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Fault};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

#[allow(dead_code)]
mod core_support;

use core_support::{Core, WAIT};

/// A pod of the fixtures' web deployment, the first by name behind the `web` Service.
const WEB: &str = "web-69c6f74b8b-qv4xv";

fn request(target: ForwardTarget, remote_port: u16) -> ForwardRequest {
    ForwardRequest {
        cluster: CONTEXT.into(),
        namespace: "demo".into(),
        target,
        remote_port,
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

// covers UP-70, UP-73.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_forward_carries_bytes_and_stop_closes_it() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let port = listening(&forward).await;
    let mut stream = echoes(port).await;
    let shown = until(&forward, |s| s.pod.is_some()).await;
    assert_eq!(shown.pod.as_deref(), Some(WEB));
    assert!(server.forward_requests()[0].path.contains(WEB));

    forward.stop();
    until(&forward, |s| s.state == ForwardState::Stopped).await;
    let mut buf = [0; 8];
    let read = tokio::time::timeout(WAIT, stream.read(&mut buf))
        .await
        .expect("the connection ended");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    assert!(
        TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_err()
    );
}

// covers UP-74: a Service forward reaches a ready pod behind it, at the port it maps to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_service_forward_goes_through_a_ready_pod() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Service("web".into()), 80));
    let port = listening(&forward).await;
    let _stream = echoes(port).await;
    let requests = server.forward_requests();
    assert!(requests[0].path.contains(WEB), "{:?}", requests[0].path);
}

// covers UP-54: protected means nothing listens and nothing reaches the cluster; locking again
// ends a running forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_cluster_takes_no_forward_and_locking_ends_one() {
    let server = FakeApiServer::start().await.unwrap();
    let settings = Settings {
        protect_local_clusters: true,
        ..Settings::default()
    };
    let core = Core::start_with(&server, Some("demo"), settings).await;
    let refused = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let shown = until(&refused, |s| s.state.is_finished()).await;
    let ForwardState::Failed(failure) = shown.state else {
        panic!("{shown:?}");
    };
    assert_eq!(failure.kind, ForwardFailureKind::Refused);
    assert_eq!(shown.local_port, None, "nothing listened");
    assert!(server.forward_requests().is_empty());

    core.handle.unlock(CONTEXT, &server.url());
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let port = listening(&forward).await;
    let _stream = echoes(port).await;
    assert!(core.handle.lock(CONTEXT));
    let shown = until(&forward, |s| s.state.is_finished()).await;
    assert!(
        matches!(&shown.state, ForwardState::Failed(f) if f.kind == ForwardFailureKind::Refused),
        "{shown:?}"
    );
    let sent = server.forward_requests().len();
    assert!(
        TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(server.forward_requests().len(), sent);
}

// covers UP-06: a context pointed elsewhere ends the forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_re_pointed_context_ends_the_forward() {
    let server = FakeApiServer::start().await.unwrap();
    let other = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let port = listening(&forward).await;
    let _stream = echoes(port).await;

    let kubeconfig = core.dir.path().join("kubeconfig");
    other.write_kubeconfig(&kubeconfig, Some("demo")).unwrap();
    core.handle.reload_kubeconfig().await.unwrap();
    let shown = until(&forward, |s| s.state.is_finished()).await;
    assert!(
        matches!(&shown.state, ForwardState::Failed(f) if f.kind == ForwardFailureKind::Refused),
        "{shown:?}"
    );
    assert!(other.forward_requests().is_empty());
}

/// `WEB` deleted and created again under its name, with another uid.
fn recreate_web(server: &FakeApiServer) {
    let again = server.pod("demo", WEB).unwrap().sibling(WEB);
    server.replace_pod("demo", WEB, again);
}

fn ended_as(shown: &ForwardSnapshot, kind: ForwardFailureKind) -> bool {
    matches!(&shown.state, ForwardState::Failed(f) if f.kind == kind)
}

// A pod recreated under its name is another pod: no new connection reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_recreated_under_its_name_ends_the_forward() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let port = listening(&forward).await;
    let _first = echoes(port).await;
    let opened = server.forward_requests().len();

    recreate_web(&server);
    let _second = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let shown = until(&forward, |s| s.state.is_finished()).await;
    assert!(
        ended_as(&shown, ForwardFailureKind::TargetGone),
        "{shown:?}"
    );
    assert_eq!(
        server.forward_requests().len(),
        opened,
        "nothing was opened"
    );
}

// The pod is replaced while its upgrade is on the way: the tunnel carries nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_replaced_during_the_upgrade_gets_no_bytes() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let port = listening(&forward).await;
    let _first = echoes(port).await;

    let (fault, hold) = Fault::hold();
    server.inject(fault.subresource("portforward").times(1));
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    tokio::time::timeout(WAIT, hold.reached())
        .await
        .expect("the upgrade reached the server");
    recreate_web(&server);
    hold.release();
    let shown = until(&forward, |s| s.state.is_finished()).await;
    assert!(
        ended_as(&shown, ForwardFailureKind::TargetGone),
        "{shown:?}"
    );
    let mut buf = [0; 4];
    let read = tokio::time::timeout(WAIT, stream.read(&mut buf))
        .await
        .expect("the connection ended");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
}

// A slice that still names a replaced pod's uid: the Service forward takes the pod that matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_service_forward_skips_an_endpoint_of_a_replaced_pod() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    recreate_web(&server);
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Service("web".into()), 80));
    let port = listening(&forward).await;
    let _stream = echoes(port).await;
    let requests = server.forward_requests();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].path.contains(WEB), "{:?}", requests[0].path);
}

// A refused upgrade is sent once, whatever the status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_upgrade_is_not_sent_again() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    server.inject(Fault::status(503).subresource("portforward"));
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let port = listening(&forward).await;
    let _stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    until(&forward, |s| s.warning.is_some()).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(server.forward_requests().len(), 1);
}

// An error body that never ends is not read: the connection fails at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endless_error_body_is_not_read() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    server.inject(Fault::endless_error().subresource("portforward"));
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let port = listening(&forward).await;
    let _stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let mut updates = forward.updates();
    tokio::time::timeout(
        Duration::from_secs(3),
        wait_for(&mut updates, |s| s.warning.is_some()),
    )
    .await
    .expect("the refusal was seen without reading its body");
}

// A role without `create` on a server enforcing it: the forward says so and ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_requiring_create_ends_the_forward_as_forbidden() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    server.inject(Fault::forbidden_as("create").subresource("portforward"));
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod(WEB.into()), 8080));
    let port = listening(&forward).await;
    let _stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let shown = until(&forward, |s| s.state.is_finished()).await;
    assert!(ended_as(&shown, ForwardFailureKind::Forbidden), "{shown:?}");
}

// A pod that is gone ends the forward, saying so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_pod_ends_the_forward_on_its_first_connection() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    let forward = core
        .handle
        .port_forward(request(ForwardTarget::Pod("no-such-pod".into()), 8080));
    let port = listening(&forward).await;
    let _stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let shown = until(&forward, |s| s.state.is_finished()).await;
    assert!(
        matches!(&shown.state, ForwardState::Failed(f) if f.kind == ForwardFailureKind::TargetGone),
        "{shown:?}"
    );
}
