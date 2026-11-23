//! Port-forwards (UP-70, UP-73, UP-74): a listener on `127.0.0.1` whose connections are carried to
//! a pod's port over the API server's WebSocket `portforward` endpoint.
//!
//! A forward opens a channel into the cluster, so it takes the same gate as exec (ADR-0015). The
//! client is asked for again for every connection, bound to the pin the forward started on, and
//! is never kept. The gate is also asked every two seconds while the forward runs: when it
//! refuses (protected, re-pointed, paused, removed), the listener closes and every connection
//! ends.
//!
//! A Service forward picks a ready pod for each new connection, so it moves to another pod when
//! its pod goes away. Reasons shown are Flint's own words, never a server's or a plugin's text.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{Api, ListParams};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;

use crate::cluster::ClusterId;
use crate::handle::{AdmittedMember, Request};
use crate::protection::{GatedClient, WriteRefusal};
use crate::recheck::ContextPin;
use crate::write::WriteError;

/// How often a running forward asks the gate. An in-memory check.
const GATE_EVERY: Duration = Duration::from_secs(2);
/// How often a running forward checks that its target is still there.
const HEALTH_EVERY: Duration = Duration::from_secs(15);
/// How long opening one connection, or one health check, may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// The most connections one forward carries at once; further ones wait in the accept queue.
pub const MAX_CONNECTIONS: usize = 64;
/// The wait after a failed `accept` (out of file descriptors, say), so it does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const LISTEN_BACKLOG: u32 = 128;

/// What a forward reaches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ForwardTarget {
    Pod(String),
    Service(String),
}

impl ForwardTarget {
    pub fn name(&self) -> &str {
        match self {
            ForwardTarget::Pod(name) | ForwardTarget::Service(name) => name,
        }
    }
}

impl std::fmt::Display for ForwardTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwardTarget::Pod(name) => write!(f, "pod/{name}"),
            ForwardTarget::Service(name) => write!(f, "service/{name}"),
        }
    }
}

/// What to forward. Its `Debug` leaves out the server: a kubeconfig URL can carry credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub cluster: ClusterId,
    pub namespace: String,
    pub target: ForwardTarget,
    /// The pod's port, or for a Service the Service's port.
    pub remote_port: u16,
    /// `None` lets the system choose a free port.
    pub local_port: Option<u16>,
    /// The server the forward is for, as the cluster list gave it. `None` for whatever the
    /// context shows when the core takes the request.
    pub server: Option<Option<String>>,
}

impl std::fmt::Debug for ForwardRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardRequest")
            .field("cluster", &self.cluster)
            .field("namespace", &self.namespace)
            .field("target", &self.target)
            .field("remote_port", &self.remote_port)
            .field("local_port", &self.local_port)
            .finish_non_exhaustive()
    }
}

/// Where a forward is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardState {
    Starting,
    Listening,
    Failed(ForwardFailure),
    Stopped,
}

impl ForwardState {
    pub fn is_finished(&self) -> bool {
        matches!(self, ForwardState::Failed(_) | ForwardState::Stopped)
    }
}

/// Why a forward ended, in words safe to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardFailure {
    pub kind: ForwardFailureKind,
    pub message: String,
}

impl ForwardFailure {
    fn new(kind: ForwardFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ForwardFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardFailureKind {
    /// The local port is taken. Nothing was sent to the cluster.
    PortInUse,
    /// The local port could not be opened for another reason.
    Bind,
    /// The write gate refused: protected, re-pointed, paused, removed or not connected.
    Refused,
    /// RBAC refused the forward.
    Forbidden,
    /// The pod is gone or has finished.
    TargetGone,
}

/// A forward's state at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardSnapshot {
    pub state: ForwardState,
    /// The port listened on, once bound. Kept after the forward ends, to say which it was.
    pub local_port: Option<u16>,
    /// The pod the last connection went to.
    pub pod: Option<String>,
    pub connections: usize,
    /// Why the last connection dropped, until one succeeds. The forward keeps listening.
    pub warning: Option<String>,
}

/// A front end's handle to one forward, from [`crate::CoreHandle::port_forward`]. Dropping it
/// stops the forward.
pub struct PortForward {
    request: ForwardRequest,
    stop: watch::Sender<bool>,
    rx: watch::Receiver<ForwardSnapshot>,
}

impl std::fmt::Debug for PortForward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortForward")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// The core's side of a forward.
pub(crate) struct ForwardChannels {
    stop: watch::Receiver<bool>,
    out: Arc<watch::Sender<ForwardSnapshot>>,
}

impl PortForward {
    pub(crate) fn new(request: ForwardRequest) -> (Self, ForwardChannels) {
        let (out, rx) = watch::channel(ForwardSnapshot {
            state: ForwardState::Starting,
            local_port: None,
            pod: None,
            connections: 0,
            warning: None,
        });
        let (stop_tx, stop) = watch::channel(false);
        (
            Self {
                request,
                stop: stop_tx,
                rx,
            },
            ForwardChannels {
                stop,
                out: Arc::new(out),
            },
        )
    }

    pub fn request(&self) -> &ForwardRequest {
        &self.request
    }

    /// Closes the listener and ends every connection.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    pub fn snapshot(&self) -> ForwardSnapshot {
        self.rx.borrow().clone()
    }

    /// A reader of this forward's snapshots that a task can wait on.
    pub fn updates(&self) -> ForwardUpdates {
        let mut rx = self.rx.clone();
        rx.mark_changed();
        ForwardUpdates { rx }
    }
}

/// Reads a [`PortForward`]'s snapshots from elsewhere.
#[derive(Debug)]
pub struct ForwardUpdates {
    rx: watch::Receiver<ForwardSnapshot>,
}

impl ForwardUpdates {
    /// Waits for a newer snapshot. `false` once the forward's task has ended. Works on any
    /// executor.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    pub fn latest(&mut self) -> ForwardSnapshot {
        self.rx.borrow_and_update().clone()
    }
}

/// Opens the local port on loopback only. Without `SO_REUSEADDR`, which on macOS would let this
/// bind beside a listener on the wildcard address and take some of its connections.
fn bind_loopback(port: Option<u16>) -> io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port.unwrap_or(0)).into())?;
    socket.listen(LISTEN_BACKLOG)
}

fn bind_failure(port: Option<u16>, error: &io::Error) -> ForwardFailure {
    let shown = port.map_or("a free port".to_owned(), |p| format!("port {p}"));
    match error.kind() {
        io::ErrorKind::AddrInUse => ForwardFailure::new(
            ForwardFailureKind::PortInUse,
            format!("{shown} is already in use on this computer; choose another"),
        ),
        io::ErrorKind::PermissionDenied => ForwardFailure::new(
            ForwardFailureKind::Bind,
            format!("this computer does not allow Flint to listen on {shown}; choose another"),
        ),
        _ => ForwardFailure::new(
            ForwardFailureKind::Bind,
            format!("Flint could not listen on {shown}"),
        ),
    }
}

trait AsyncStream: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> AsyncStream for T {}

/// An open connection to a pod's port.
pub(crate) struct Upstream {
    stream: Pin<Box<dyn AsyncStream>>,
    /// Resolves with `true` if the API server reported an error on the port.
    failed: BoxFuture<'static, bool>,
    pod: String,
    /// Held for the connection's life; dropping it closes the WebSocket.
    _connection: Option<AbortOnDrop>,
}

/// kube's forwarder leaves its task running when dropped: this ends it.
struct AbortOnDrop(kube::api::Portforwarder);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Why a connection could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DialError {
    /// The forward must end.
    Fatal(ForwardFailure),
    /// This connection failed; the forward keeps listening.
    Dropped(String),
}

/// Reaches the target. A seam, so forwarding is testable without a cluster.
pub(crate) trait Dialer: Send + Sync + 'static {
    /// Whether the gate still allows this forward.
    fn check(&self) -> BoxFuture<'static, Result<(), ForwardFailure>>;
    /// The pod a connection would reach now.
    fn probe(&self) -> BoxFuture<'static, Result<String, DialError>>;
    fn dial(&self) -> BoxFuture<'static, Result<Upstream, DialError>>;
}

fn is_local_close(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected
    )
}

fn dropped_text(remote: u16) -> String {
    format!("a connection to port {remote} dropped")
}

/// Carries one local connection. `Some` when the forward must end.
async fn relay<D: Dialer>(
    mut local: TcpStream,
    dialer: Arc<D>,
    out: Arc<watch::Sender<ForwardSnapshot>>,
    remote: u16,
) -> Option<ForwardFailure> {
    let dialed = tokio::time::timeout(DIAL_TIMEOUT, dialer.dial()).await;
    let mut upstream = match dialed {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(DialError::Fatal(failure))) => return Some(failure),
        Ok(Err(DialError::Dropped(why))) => {
            out.send_modify(|s| s.warning = Some(why));
            return None;
        }
        Err(_) => {
            out.send_modify(|s| s.warning = Some(format!("port {remote} did not answer in time")));
            return None;
        }
    };
    let pod = upstream.pod.clone();
    out.send_modify(|s| {
        s.pod = Some(pod);
        s.warning = None;
    });
    let dropped = tokio::select! {
        copied = tokio::io::copy_bidirectional(&mut local, &mut upstream.stream) => {
            copied.is_err_and(|e| !is_local_close(&e))
        }
        true = &mut upstream.failed => true,
    };
    if dropped {
        out.send_modify(|s| s.warning = Some(dropped_text(remote)));
    }
    None
}

/// Resolves with why the forward must end, once the gate refuses.
async fn watch_gate<D: Dialer>(dialer: Arc<D>) -> ForwardFailure {
    loop {
        tokio::time::sleep(GATE_EVERY).await;
        if let Err(failure) = dialer.check().await {
            return failure;
        }
    }
}

/// Resolves with why the forward must end, once its target is gone. Says meanwhile when no pod
/// can take a connection, so an idle forward does not look fine.
async fn watch_target<D: Dialer>(
    dialer: Arc<D>,
    out: Arc<watch::Sender<ForwardSnapshot>>,
) -> ForwardFailure {
    loop {
        tokio::time::sleep(HEALTH_EVERY).await;
        match tokio::time::timeout(DIAL_TIMEOUT, dialer.probe()).await {
            Ok(Ok(pod)) => out.send_if_modified(|s| {
                let changed = s.pod.as_ref() != Some(&pod) || s.warning.is_some();
                s.pod = Some(pod.clone());
                s.warning = None;
                changed
            }),
            Ok(Err(DialError::Fatal(failure))) => return failure,
            Ok(Err(DialError::Dropped(why))) => out.send_if_modified(|s| {
                let changed = s.warning.as_ref() != Some(&why);
                s.warning = Some(why.clone());
                changed
            }),
            // Slow is not gone: a connection will say.
            Err(_) => false,
        };
    }
}

/// Serves `listener` until the forward is stopped or must end. Closes the listener and every
/// connection before it says so.
pub(crate) async fn serve<D: Dialer>(
    listener: TcpListener,
    dialer: Arc<D>,
    channels: ForwardChannels,
    remote: u16,
) {
    let ForwardChannels { mut stop, out } = channels;
    // Also resolves, with an error, once the handle is dropped.
    let mut stopped = Box::pin(async move {
        let _ = stop.wait_for(|stopped| *stopped).await;
    });
    let mut conns: JoinSet<Option<ForwardFailure>> = JoinSet::new();
    let mut gate = Box::pin(watch_gate(dialer.clone()));
    let mut target = Box::pin(watch_target(dialer.clone(), out.clone()));
    let end = loop {
        tokio::select! {
            biased;
            () = &mut stopped => break ForwardState::Stopped,
            failure = &mut gate => break ForwardState::Failed(failure),
            failure = &mut target => break ForwardState::Failed(failure),
            Some(done) = conns.join_next(), if !conns.is_empty() => {
                if let Ok(Some(failure)) = done {
                    break ForwardState::Failed(failure);
                }
                let count = conns.len();
                out.send_modify(|s| s.connections = count);
            }
            accepted = listener.accept(), if conns.len() < MAX_CONNECTIONS => match accepted {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    conns.spawn(relay(stream, dialer.clone(), out.clone(), remote));
                    let count = conns.len();
                    out.send_modify(|s| s.connections = count);
                }
                Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
            },
        }
    };
    drop(listener);
    conns.shutdown().await;
    out.send_modify(|s| {
        s.state = end;
        s.connections = 0;
    });
}

/// Starts a forward whose gate answered: refuses a port in use before anything reaches the
/// cluster, then serves.
pub(crate) async fn run<D: Dialer>(
    dialer: Arc<D>,
    request: ForwardRequest,
    channels: ForwardChannels,
) {
    if let Err(failure) = dialer.check().await {
        channels
            .out
            .send_modify(|s| s.state = ForwardState::Failed(failure));
        return;
    }
    let listener = match bind_loopback(request.local_port) {
        Ok(listener) => listener,
        Err(e) => {
            let failure = bind_failure(request.local_port, &e);
            channels
                .out
                .send_modify(|s| s.state = ForwardState::Failed(failure));
            return;
        }
    };
    let local_port = listener.local_addr().ok().map(|a| a.port());
    channels.out.send_modify(|s| {
        s.local_port = local_port;
        s.state = ForwardState::Listening;
    });
    serve(listener, dialer, channels, request.remote_port).await;
}

/// Ends a forward that never got past the gate.
pub(crate) fn refuse(channels: &ForwardChannels, refusal: &WriteError) {
    let failure = gate_failure(refusal);
    channels
        .out
        .send_modify(|s| s.state = ForwardState::Failed(failure));
}

/// The gate's refusal in words about a forward: its own speak of shells.
pub(crate) fn gate_failure(refusal: &WriteError) -> ForwardFailure {
    let message = match refusal {
        WriteError::Refused(WriteRefusal::Protected { cluster }) => {
            format!("{cluster} is protected: unlock it to forward ports")
        }
        WriteError::Refused(WriteRefusal::NotConnected { cluster }) => {
            format!("{cluster} is not connected")
        }
        WriteError::Refused(WriteRefusal::Removed { cluster }) => {
            format!("{cluster} is no longer in your kubeconfig files")
        }
        WriteError::Refused(WriteRefusal::Stopped) => "Flint is shutting down".to_owned(),
        _ => "the cluster's connection changed; start the forward again".to_owned(),
    };
    ForwardFailure::new(ForwardFailureKind::Refused, message)
}

/// The real [`Dialer`]: every connection asks the gate and takes a fresh client.
pub(crate) struct KubeDialer {
    pub requests: async_channel::Sender<Request>,
    pub request: ForwardRequest,
    pub pin: ContextPin,
    pub admitted: Option<Arc<AdmittedMember>>,
}

/// What a dial needs, without borrowing the dialer into its future.
#[derive(Clone)]
struct Access {
    requests: async_channel::Sender<Request>,
    cluster: ClusterId,
    server: Option<Option<String>>,
    pin: ContextPin,
    admitted: Option<Arc<AdmittedMember>>,
}

impl Access {
    async fn check(&self) -> Result<(), ForwardFailure> {
        if let Some(member) = &self.admitted
            && !member.holds(&self.cluster, &self.pin)
        {
            return Err(gate_failure(&WriteError::ConnectionChanged));
        }
        let (reply, rx) = oneshot::channel();
        let sent = self
            .requests
            .send(Request::CheckForward {
                cluster: self.cluster.clone(),
                pin: self.pin.clone(),
                reply,
            })
            .await;
        if sent.is_err() {
            return Err(gate_failure(&WriteRefusal::Stopped.into()));
        }
        rx.await
            .unwrap_or(Err(WriteRefusal::Stopped.into()))
            .map_err(|e| gate_failure(&e))
    }

    /// A client for one request, read in one step with the pin: refused unless it is the
    /// definition the forward started on.
    async fn client(&self) -> Result<GatedClient, ForwardFailure> {
        let (reply, rx) = oneshot::channel();
        let sent = self
            .requests
            .send(Request::BoundWriteClient {
                cluster: self.cluster.clone(),
                server: self.server.clone(),
                reply,
            })
            .await;
        if sent.is_err() {
            return Err(gate_failure(&WriteRefusal::Stopped.into()));
        }
        let (client, pin, _) = rx
            .await
            .unwrap_or(Err(WriteRefusal::Stopped.into()))
            .map_err(|e| gate_failure(&e))?;
        if pin != self.pin {
            return Err(gate_failure(&WriteError::ConnectionChanged));
        }
        Ok(client)
    }
}

fn is_status(error: &kube::Error, code: u16) -> bool {
    matches!(error, kube::Error::Api(status) if status.code == code)
}

fn forbidden(what: &str) -> DialError {
    DialError::Fatal(ForwardFailure::new(
        ForwardFailureKind::Forbidden,
        format!("your access does not allow {what}"),
    ))
}

fn gone(target: &ForwardTarget) -> DialError {
    DialError::Fatal(ForwardFailure::new(
        ForwardFailureKind::TargetGone,
        format!("{target} no longer exists"),
    ))
}

/// The pod and the pod's port a connection goes to.
async fn resolve(
    client: &kube::Client,
    namespace: &str,
    target: &ForwardTarget,
    remote: u16,
) -> Result<(String, u16), DialError> {
    match target {
        ForwardTarget::Pod(name) => {
            let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
            let pod = pods.get(name).await.map_err(|e| {
                if is_status(&e, 404) {
                    gone(target)
                } else if is_status(&e, 403) {
                    forbidden("reading this pod")
                } else {
                    DialError::Dropped(format!("Flint could not reach {target}"))
                }
            })?;
            let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
            if matches!(phase, Some("Succeeded" | "Failed")) {
                return Err(DialError::Fatal(ForwardFailure::new(
                    ForwardFailureKind::TargetGone,
                    format!("{target} has finished"),
                )));
            }
            Ok((name.clone(), remote))
        }
        ForwardTarget::Service(name) => {
            let unreachable = |e: kube::Error| {
                if is_status(&e, 404) {
                    gone(target)
                } else if is_status(&e, 403) {
                    forbidden("reading this service and its endpoints")
                } else {
                    DialError::Dropped(format!("Flint could not reach {target}"))
                }
            };
            let services: Api<Service> = Api::namespaced(client.clone(), namespace);
            let service = services.get(name).await.map_err(unreachable)?;
            let slices: Api<EndpointSlice> = Api::namespaced(client.clone(), namespace);
            let params =
                ListParams::default().labels(&format!("kubernetes.io/service-name={name}"));
            let slices = slices.list(&params).await.map_err(unreachable)?;
            pick_backend(&service, &slices.items, remote).ok_or_else(|| {
                DialError::Dropped(format!("{target} has no ready pod for port {remote}"))
            })
        }
    }
}

/// The first ready pod behind `service`'s `port`, by name, with the port it listens on there.
fn pick_backend(service: &Service, slices: &[EndpointSlice], port: u16) -> Option<(String, u16)> {
    let declared = service
        .spec
        .as_ref()?
        .ports
        .as_ref()?
        .iter()
        .find(|p| p.port == i32::from(port))?;
    let wanted = declared.name.as_deref().unwrap_or("");
    let mut ready: Vec<(String, u16)> = Vec::new();
    for slice in slices {
        let Some(number) = slice
            .ports
            .iter()
            .flatten()
            .find(|p| p.name.as_deref().unwrap_or("") == wanted)
            .and_then(|p| p.port)
            .and_then(|p| u16::try_from(p).ok())
        else {
            continue;
        };
        for endpoint in slice.endpoints.iter().flatten() {
            let is_ready = endpoint
                .conditions
                .as_ref()
                .and_then(|c| c.ready)
                .unwrap_or(true);
            let pod = endpoint
                .target_ref
                .as_ref()
                .filter(|r| r.kind.as_deref() == Some("Pod"))
                .and_then(|r| r.name.clone());
            if let (true, Some(pod)) = (is_ready, pod) {
                ready.push((pod, number));
            }
        }
    }
    ready.into_iter().min()
}

impl Dialer for KubeDialer {
    fn check(&self) -> BoxFuture<'static, Result<(), ForwardFailure>> {
        let access = self.access();
        async move { access.check().await }.boxed()
    }

    fn probe(&self) -> BoxFuture<'static, Result<String, DialError>> {
        let access = self.access();
        let request = self.request.clone();
        async move {
            access.check().await.map_err(DialError::Fatal)?;
            let client = access
                .client()
                .await
                .map_err(DialError::Fatal)?
                .into_client();
            let (pod, _) = resolve(
                &client,
                &request.namespace,
                &request.target,
                request.remote_port,
            )
            .await?;
            Ok(pod)
        }
        .boxed()
    }

    fn dial(&self) -> BoxFuture<'static, Result<Upstream, DialError>> {
        let access = self.access();
        let request = self.request.clone();
        async move {
            access.check().await.map_err(DialError::Fatal)?;
            let client = access
                .client()
                .await
                .map_err(DialError::Fatal)?
                .into_client();
            let (pod, port) = resolve(
                &client,
                &request.namespace,
                &request.target,
                request.remote_port,
            )
            .await?;
            // The reads took a moment: asked again immediately before the channel opens.
            access.check().await.map_err(DialError::Fatal)?;
            let pods: Api<Pod> = Api::namespaced(client, &request.namespace);
            let mut forwarder = pods.portforward(&pod, &[port]).await.map_err(|e| {
                if is_status(&e, 403) {
                    forbidden("port-forwarding to this pod")
                } else if is_status(&e, 404) && matches!(request.target, ForwardTarget::Pod(_)) {
                    gone(&request.target)
                } else {
                    DialError::Dropped(format!("Flint could not open a connection to {pod}"))
                }
            })?;
            let missing = || DialError::Dropped(dropped_text(port));
            let stream = forwarder.take_stream(port).ok_or_else(missing)?;
            let errors = forwarder.take_error(port).ok_or_else(missing)?;
            Ok(Upstream {
                stream: Box::pin(stream),
                failed: errors.map(|message| message.is_some()).boxed(),
                pod,
                _connection: Some(AbortOnDrop(forwarder)),
            })
        }
        .boxed()
    }
}

impl KubeDialer {
    fn access(&self) -> Access {
        Access {
            requests: self.requests.clone(),
            cluster: self.request.cluster.clone(),
            server: self.request.server.clone(),
            pin: self.pin.clone(),
            admitted: self.admitted.clone(),
        }
    }
}

/// A port a target declares, for offering it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredPort {
    pub port: u16,
    pub name: Option<String>,
    pub app_protocol: Option<String>,
}

/// The ports a pod's containers or a Service declare, from its object, without duplicates.
pub fn declared_ports(object: &Value) -> Vec<DeclaredPort> {
    let read = |entry: &Value, number: &str| {
        Some(DeclaredPort {
            port: u16::try_from(entry.get(number)?.as_u64()?).ok()?,
            name: entry.get("name").and_then(Value::as_str).map(str::to_owned),
            app_protocol: entry
                .get("appProtocol")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    };
    let tcp = |entry: &&Value| {
        entry
            .get("protocol")
            .and_then(Value::as_str)
            .is_none_or(|p| p.eq_ignore_ascii_case("TCP"))
    };
    let spec = object.get("spec");
    let mut ports: Vec<DeclaredPort> =
        if object.get("kind").and_then(Value::as_str) == Some("Service") {
            spec.and_then(|s| s.get("ports"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(tcp)
                .filter_map(|p| read(p, "port"))
                .collect()
        } else {
            spec.and_then(|s| s.get("containers"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| c.get("ports").and_then(Value::as_array))
                .flatten()
                .filter(tcp)
                .filter_map(|p| read(p, "containerPort"))
                .collect()
        };
    let mut seen = std::collections::HashSet::new();
    ports.retain(|p| seen.insert(p.port));
    ports
}

/// The address to open a forwarded port at, by its name or `appProtocol`: `https` for a port
/// that says so, `http` otherwise.
pub fn browser_url(local_port: u16, name: Option<&str>, app_protocol: Option<&str>) -> String {
    let says = |text: Option<&str>| text.is_some_and(|t| t.eq_ignore_ascii_case("https"));
    let scheme = if says(name) || says(app_protocol) {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://localhost:{local_port}")
}

/// The core's side of a forward no task runs, for a front end's tests.
#[cfg(any(test, feature = "test-support"))]
pub struct FakeForward(ForwardChannels);

#[cfg(any(test, feature = "test-support"))]
impl FakeForward {
    /// A forward for `request`, and the side that drives it.
    pub fn new(request: ForwardRequest) -> (PortForward, Self) {
        let (forward, channels) = PortForward::new(request);
        (forward, Self(channels))
    }

    /// Says it listens on `port`.
    pub fn listen(&self, port: u16) {
        self.0.out.send_modify(|s| {
            s.local_port = Some(port);
            s.state = ForwardState::Listening;
        });
    }

    /// Whether it was told to stop, or its handle is gone.
    pub fn is_stopped(&self) -> bool {
        *self.0.stop.borrow() || self.0.stop.has_changed().is_err()
    }
}

/// Waits for a snapshot `check` accepts, for tests.
#[cfg(any(test, feature = "test-support"))]
pub async fn wait_for(
    updates: &mut ForwardUpdates,
    check: impl Fn(&ForwardSnapshot) -> bool,
) -> ForwardSnapshot {
    loop {
        let snapshot = updates.latest();
        if check(&snapshot) || !updates.changed().await {
            return updates.latest();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream, duplex};

    use super::*;

    /// A target that echoes, with switches for what can go wrong.
    #[derive(Default)]
    struct FakeDialer {
        refused: AtomicBool,
        dials: AtomicUsize,
        fail: Mutex<Option<DialError>>,
        probe: Mutex<Option<DialError>>,
        report_error: AtomicBool,
    }

    fn echo(mut side: DuplexStream) {
        tokio::spawn(async move {
            let mut buf = [0; 256];
            while let Ok(n) = side.read(&mut buf).await {
                if n == 0 || side.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });
    }

    impl Dialer for Arc<FakeDialer> {
        fn check(&self) -> BoxFuture<'static, Result<(), ForwardFailure>> {
            let refused = self.refused.load(Ordering::SeqCst);
            async move {
                if refused {
                    Err(gate_failure(
                        &WriteRefusal::Protected {
                            cluster: "dev".into(),
                        }
                        .into(),
                    ))
                } else {
                    Ok(())
                }
            }
            .boxed()
        }

        fn probe(&self) -> BoxFuture<'static, Result<String, DialError>> {
            let answer = self.probe.lock().unwrap().clone();
            async move { answer.map_or(Ok("web-1".to_owned()), Err) }.boxed()
        }

        fn dial(&self) -> BoxFuture<'static, Result<Upstream, DialError>> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let failure = self.fail.lock().unwrap().clone();
            let report = self.report_error.load(Ordering::SeqCst);
            async move {
                if let Some(failure) = failure {
                    return Err(failure);
                }
                let (near, far) = duplex(1024);
                echo(far);
                Ok(Upstream {
                    stream: Box::pin(near),
                    failed: if report {
                        async { true }.boxed()
                    } else {
                        futures::future::pending().boxed()
                    },
                    pod: "web-1".to_owned(),
                    _connection: None,
                })
            }
            .boxed()
        }
    }

    fn request(local_port: Option<u16>) -> ForwardRequest {
        ForwardRequest {
            cluster: "dev".into(),
            namespace: "demo".into(),
            target: ForwardTarget::Pod("web-1".into()),
            remote_port: 8080,
            local_port,
            server: None,
        }
    }

    async fn started(
        dialer: &Arc<FakeDialer>,
        local_port: Option<u16>,
    ) -> (PortForward, ForwardUpdates, u16) {
        let request = request(local_port);
        let (forward, channels) = PortForward::new(request.clone());
        tokio::spawn(run(Arc::new(dialer.clone()), request, channels));
        let mut updates = forward.updates();
        let shown = wait_for(&mut updates, |s| s.state != ForwardState::Starting).await;
        assert_eq!(shown.state, ForwardState::Listening);
        let port = shown.local_port.expect("a port");
        (forward, updates, port)
    }

    async fn round_trip(port: u16) -> TcpStream {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut back = [0; 4];
        stream.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
        stream
    }

    // covers UP-73: a free port is chosen and shown, and it listens on loopback only.
    #[tokio::test]
    async fn listens_on_loopback_and_carries_bytes() {
        let dialer = Arc::new(FakeDialer::default());
        let (_forward, mut updates, port) = started(&dialer, None).await;
        assert_ne!(port, 0);
        let listener = bind_loopback(None).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
        let _stream = round_trip(port).await;
        let shown = wait_for(&mut updates, |s| s.pod.is_some()).await;
        assert_eq!(shown.pod.as_deref(), Some("web-1"));
    }

    // covers UP-70, UP-73: a port in use is refused before anything reaches the cluster.
    #[tokio::test]
    async fn a_port_in_use_is_refused_before_any_dial() {
        let taken = bind_loopback(None).unwrap();
        let port = taken.local_addr().unwrap().port();
        let dialer = Arc::new(FakeDialer::default());
        let request = request(Some(port));
        let (forward, channels) = PortForward::new(request.clone());
        run(Arc::new(dialer.clone()), request, channels).await;
        let ForwardState::Failed(failure) = forward.snapshot().state else {
            panic!("not refused");
        };
        assert_eq!(failure.kind, ForwardFailureKind::PortInUse);
        assert!(failure.message.contains(&port.to_string()));
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
    }

    // A listener on the wildcard address takes the port on loopback too.
    #[tokio::test]
    async fn a_wildcard_listener_counts_as_in_use() {
        let taken = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        let bound = bind_loopback(Some(port));
        assert_eq!(
            bound.map(drop).map_err(|e| e.kind()),
            Err(io::ErrorKind::AddrInUse)
        );
    }

    #[tokio::test]
    async fn several_ports_of_one_target_run_at_once() {
        let dialer = Arc::new(FakeDialer::default());
        let (_a, _, first) = started(&dialer, None).await;
        let (_b, _, second) = started(&dialer, None).await;
        assert_ne!(first, second);
        let _one = round_trip(first).await;
        let _two = round_trip(second).await;
    }

    async fn closed(port: u16) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .is_ok()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the listener closed");
    }

    async fn ended(stream: &mut TcpStream) {
        let mut buf = [0; 8];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("the connection ended");
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    }

    #[tokio::test]
    async fn stop_closes_the_listener_and_its_connections() {
        let dialer = Arc::new(FakeDialer::default());
        let (forward, mut updates, port) = started(&dialer, None).await;
        let mut stream = round_trip(port).await;
        forward.stop();
        let shown = wait_for(&mut updates, |s| s.state.is_finished()).await;
        assert_eq!(shown.state, ForwardState::Stopped);
        assert_eq!(shown.local_port, Some(port), "still says which it was");
        ended(&mut stream).await;
        closed(port).await;
    }

    // Quitting drops the handle.
    #[tokio::test]
    async fn dropping_the_handle_stops_the_forward() {
        let dialer = Arc::new(FakeDialer::default());
        let (forward, _updates, port) = started(&dialer, None).await;
        let mut stream = round_trip(port).await;
        drop(forward);
        ended(&mut stream).await;
        closed(port).await;
    }

    // covers UP-54: a gate that starts refusing ends a running forward.
    #[tokio::test]
    async fn a_refusing_gate_ends_the_forward() {
        let dialer = Arc::new(FakeDialer::default());
        let (_forward, mut updates, port) = started(&dialer, None).await;
        let mut stream = round_trip(port).await;
        dialer.refused.store(true, Ordering::SeqCst);
        let shown = tokio::time::timeout(
            GATE_EVERY * 3,
            wait_for(&mut updates, |s| s.state.is_finished()),
        )
        .await
        .expect("ended");
        let ForwardState::Failed(failure) = shown.state else {
            panic!("not refused");
        };
        assert_eq!(failure.kind, ForwardFailureKind::Refused);
        ended(&mut stream).await;
        closed(port).await;
    }

    #[tokio::test]
    async fn a_refused_dial_ends_the_forward() {
        let dialer = Arc::new(FakeDialer::default());
        let (_forward, mut updates, port) = started(&dialer, None).await;
        *dialer.fail.lock().unwrap() = Some(DialError::Fatal(ForwardFailure::new(
            ForwardFailureKind::TargetGone,
            "pod/web-1 no longer exists",
        )));
        let _stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let shown = wait_for(&mut updates, |s| s.state.is_finished()).await;
        assert!(matches!(
            shown.state,
            ForwardState::Failed(ForwardFailure {
                kind: ForwardFailureKind::TargetGone,
                ..
            })
        ));
        closed(port).await;
    }

    // covers UP-70: a dropped connection is said, and the forward keeps listening.
    #[tokio::test]
    async fn a_dropped_connection_warns_and_keeps_listening() {
        let dialer = Arc::new(FakeDialer::default());
        let (_forward, mut updates, port) = started(&dialer, None).await;
        *dialer.fail.lock().unwrap() = Some(DialError::Dropped("no ready pod".into()));
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let shown = wait_for(&mut updates, |s| s.warning.is_some()).await;
        assert_eq!(shown.warning.as_deref(), Some("no ready pod"));
        assert_eq!(shown.state, ForwardState::Listening);
        ended(&mut stream).await;
        *dialer.fail.lock().unwrap() = None;
        let _stream = round_trip(port).await;
        let shown = wait_for(&mut updates, |s| s.warning.is_none()).await;
        assert_eq!(shown.state, ForwardState::Listening);
    }

    #[tokio::test]
    async fn an_error_on_the_port_is_reported_as_a_drop() {
        let dialer = Arc::new(FakeDialer::default());
        dialer.report_error.store(true, Ordering::SeqCst);
        let (_forward, mut updates, port) = started(&dialer, None).await;
        let _stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let shown = wait_for(&mut updates, |s| s.warning.is_some()).await;
        assert_eq!(shown.warning, Some(dropped_text(8080)));
    }

    fn service(ports: Value) -> Service {
        serde_json::from_value(json!({"metadata": {"name": "web"}, "spec": {"ports": ports}}))
            .unwrap()
    }

    fn slice(ports: Value, endpoints: Value) -> EndpointSlice {
        serde_json::from_value(json!({
            "metadata": {"name": "web-abc"},
            "addressType": "IPv4",
            "ports": ports,
            "endpoints": endpoints,
        }))
        .unwrap()
    }

    fn endpoint(pod: &str, ready: bool) -> Value {
        json!({
            "addresses": ["10.0.0.1"],
            "conditions": {"ready": ready},
            "targetRef": {"kind": "Pod", "name": pod},
        })
    }

    // covers UP-74: a Service forward takes a ready pod, at the port it maps to.
    #[test]
    fn a_service_port_maps_to_a_ready_pod() {
        let svc = service(json!([
            {"name": "metrics", "port": 9090},
            {"name": "http", "port": 80, "targetPort": "web"},
        ]));
        let slices = [slice(
            json!([{"name": "http", "port": 8080}, {"name": "metrics", "port": 9100}]),
            json!([
                endpoint("web-0", false),
                endpoint("web-2", true),
                endpoint("web-1", true)
            ]),
        )];
        assert_eq!(
            pick_backend(&svc, &slices, 80),
            Some(("web-1".to_owned(), 8080))
        );
        assert_eq!(
            pick_backend(&svc, &slices, 9090),
            Some(("web-1".to_owned(), 9100))
        );
        assert_eq!(pick_backend(&svc, &slices, 81), None, "not a Service port");
    }

    #[test]
    fn a_service_without_a_ready_pod_has_no_backend() {
        let svc = service(json!([{"port": 80}]));
        let slices = [slice(
            json!([{"port": 8080}]),
            json!([endpoint("web-0", false)]),
        )];
        assert_eq!(pick_backend(&svc, &slices, 80), None);
        assert_eq!(pick_backend(&svc, &[], 80), None);
    }

    #[test]
    fn declared_ports_read_pods_and_services() {
        let pod = json!({"kind": "Pod", "spec": {"containers": [
            {"ports": [{"name": "http", "containerPort": 8080}, {"containerPort": 53, "protocol": "UDP"}]},
            {"ports": [{"containerPort": 8080}, {"containerPort": 9000}]},
        ]}});
        let ports: Vec<u16> = declared_ports(&pod).iter().map(|p| p.port).collect();
        assert_eq!(ports, [8080, 9000], "TCP only, without duplicates");
        let service = json!({"kind": "Service", "spec": {"ports": [
            {"name": "web", "port": 443, "appProtocol": "https", "targetPort": 8443},
        ]}});
        assert_eq!(
            declared_ports(&service),
            [DeclaredPort {
                port: 443,
                name: Some("web".into()),
                app_protocol: Some("https".into()),
            }]
        );
    }

    // covers UP-74.
    #[test]
    fn the_browser_address_follows_the_port() {
        assert_eq!(
            browser_url(8080, Some("http"), None),
            "http://localhost:8080"
        );
        assert_eq!(browser_url(8080, None, None), "http://localhost:8080");
        assert_eq!(
            browser_url(8443, Some("https"), None),
            "https://localhost:8443"
        );
        assert_eq!(
            browser_url(8443, Some("web"), Some("HTTPS")),
            "https://localhost:8443"
        );
    }

    #[test]
    fn a_request_debug_leaves_out_the_server() {
        let mut shown = request(None);
        shown.server = Some(Some("https://user:secret@example.test".into()));
        assert!(!format!("{shown:?}").contains("secret"));
    }
}
