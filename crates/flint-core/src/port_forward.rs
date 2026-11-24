//! Port-forwards (UP-70, UP-73, UP-74): a listener on `127.0.0.1` whose connections are carried to
//! a pod's port over the API server's WebSocket `portforward` endpoint.
//!
//! A forward opens a channel into the cluster, so it takes the same gate as exec (ADR-0015). The
//! client is asked for again for every connection, bound to the pin and the connection the
//! forward started on, and is never kept. The manager ends a forward as soon as the gate
//! refuses (protected, re-pointed, paused, removed), its connection goes, or a change of cluster
//! or of keep watching leaves it without a reason to run. The listener closes and every
//! connection ends.
//!
//! A forward to a pod reaches that pod only, by uid: one recreated under its name ends it. A
//! Service forward picks a ready pod for each new connection, so it moves to another pod when
//! its pod goes away. Reasons shown are Flint's own words, never a server's or a plugin's text.

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::ListParams;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::task::JoinSet;

use crate::cluster::{ClusterId, SendCheck, upgrade_client};
use crate::credential::{CredentialId, WriteCredential};
use crate::handle::{AdmittedMember, Request};
use crate::manager::ForwardSignals;
use crate::protection::{GatedClient, WriteRefusal};
use crate::recheck::ContextPin;
use crate::write::WriteError;

mod wire;

use wire::{AbortOnDrop, WireError};

/// How often a running forward asks the gate. An in-memory check.
const GATE_EVERY: Duration = Duration::from_secs(2);
/// How often a running forward checks that its target is still there.
const HEALTH_EVERY: Duration = Duration::from_secs(15);
/// How long opening one connection, or one health check, may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// The most connections one forward carries at once; further ones wait in the accept queue.
pub const MAX_CONNECTIONS: usize = 64;
/// The most connections all forwards carry at once, which with [`wire`]'s limits bounds what
/// they can queue. Further ones wait in their listener's accept queue.
const MAX_RELAYS: usize = 128;
/// How many of a Service's pods one connection tries before it gives up.
const MAX_BACKENDS_TRIED: usize = 8;
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
    /// For a pod, the uid selected. `None` pins the uid its first connection finds.
    pub uid: Option<String>,
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
    /// Its port is about to be released: nothing should be sent to it any more.
    Stopping,
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
    pub(crate) fn new(kind: ForwardFailureKind, message: impl Into<String>) -> Self {
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
    /// The server no longer accepts the connection's sign-in.
    Unauthorized,
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
    relays: Arc<Semaphore>,
}

impl ForwardChannels {
    /// `work`, unless the forward is stopped or its handle dropped first.
    pub(crate) async fn unless_stopped<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        let mut stop = self.stop.clone();
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => None,
            value = work => Some(value),
        }
    }

    pub(crate) fn say_stopped(&self) {
        self.out.send_modify(|s| s.state = ForwardState::Stopped);
    }
}

/// What every forward's connections share.
fn shared_relays() -> Arc<Semaphore> {
    static RELAYS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(RELAYS.get_or_init(|| Arc::new(Semaphore::new(MAX_RELAYS))))
}

impl PortForward {
    pub(crate) fn new(request: ForwardRequest) -> (Self, ForwardChannels) {
        Self::with_relays(request, shared_relays())
    }

    fn with_relays(request: ForwardRequest, relays: Arc<Semaphore>) -> (Self, ForwardChannels) {
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
                relays,
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
    /// Resolves with how the forward ends, once the core ends it.
    fn ended(&self) -> BoxFuture<'static, ForwardState>;
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
    _relay: OwnedSemaphorePermit,
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
    let ForwardChannels {
        mut stop,
        out,
        relays,
    } = channels;
    // Also resolves, with an error, once the handle is dropped.
    let mut stopped = Box::pin(async move {
        let _ = stop.wait_for(|stopped| *stopped).await;
    });
    let mut conns: JoinSet<Option<ForwardFailure>> = JoinSet::new();
    let mut ended = dialer.ended();
    let mut gate = Box::pin(watch_gate(dialer.clone()));
    let mut target = Box::pin(watch_target(dialer.clone(), out.clone()));
    // Accepted, and waiting for room in the budget all forwards share.
    let mut waiting: Option<TcpStream> = None;
    let end = loop {
        tokio::select! {
            biased;
            () = &mut stopped => break ForwardState::Stopped,
            state = &mut ended => break state,
            failure = &mut gate => break ForwardState::Failed(failure),
            failure = &mut target => break ForwardState::Failed(failure),
            Some(done) = conns.join_next(), if !conns.is_empty() => {
                if let Ok(Some(failure)) = done {
                    break ForwardState::Failed(failure);
                }
                let count = conns.len();
                out.send_modify(|s| s.connections = count);
            }
            permit = relays.clone().acquire_owned(), if waiting.is_some() => {
                if let (Ok(permit), Some(stream)) = (permit, waiting.take()) {
                    conns.spawn(relay(stream, dialer.clone(), out.clone(), remote, permit));
                    let count = conns.len();
                    out.send_modify(|s| s.connections = count);
                }
            }
            accepted = listener.accept(),
                if waiting.is_none() && conns.len() < MAX_CONNECTIONS => match accepted {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    waiting = Some(stream);
                }
                Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
            },
        }
    };
    // Said first, so nothing offers the address once another program could take the port.
    out.send_modify(|s| s.state = ForwardState::Stopping);
    drop(waiting);
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
    let Some(checked) = channels.unless_stopped(dialer.check()).await else {
        channels.say_stopped();
        return;
    };
    if let Err(failure) = checked {
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

/// What the core admitted a forward on: the definition, the connection, and how it hears that it
/// must end.
pub(crate) struct ForwardGrant {
    pub(crate) id: u64,
    pub(crate) pin: ContextPin,
    pub(crate) connection: CredentialId,
    pub(crate) ended: watch::Receiver<Option<ForwardState>>,
    pub(crate) signals: ForwardSignals,
}

/// The real [`Dialer`]: every connection asks the gate and takes a fresh client.
pub(crate) struct KubeDialer {
    access: Access,
    request: ForwardRequest,
    ended: watch::Receiver<Option<ForwardState>>,
    /// The uid a pod target must have.
    uid: Arc<Mutex<Option<String>>>,
}

/// What a dial needs, without borrowing the dialer into its future.
#[derive(Clone)]
struct Access {
    requests: crate::core_queue::CoreQueue<Request>,
    id: u64,
    cluster: ClusterId,
    server: Option<Option<String>>,
    pin: ContextPin,
    connection: CredentialId,
    admitted: Option<Arc<AdmittedMember>>,
    signals: ForwardSignals,
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
            .send(Request::CheckForward { id: self.id, reply })
            .await;
        if sent.is_err() {
            return Err(gate_failure(&WriteRefusal::Stopped.into()));
        }
        rx.await
            .unwrap_or_else(|_| Err(gate_failure(&WriteRefusal::Stopped.into())))
    }

    /// A client for one request, read in one step with the pin: refused unless it is the
    /// definition and the connection the forward started on.
    async fn client(&self) -> Result<(GatedClient, WriteCredential), ForwardFailure> {
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
        let (client, pin, credential) = rx
            .await
            .unwrap_or(Err(WriteRefusal::Stopped.into()))
            .map_err(|e| gate_failure(&e))?;
        if pin != self.pin || !credential.id().is(&self.connection) {
            return Err(gate_failure(&WriteError::ConnectionChanged));
        }
        Ok((client, credential))
    }

    /// The client with its credential already resolved, under the connection's own grant: what
    /// is sent with it runs nothing and waits for no refresh.
    async fn signed_in(&self) -> Result<kube::Client, DialError> {
        let (client, credential) = self.client().await.map_err(DialError::Fatal)?;
        match credential.resolved(client.into_client()).await {
            Ok(client) => Ok(client),
            Err(WriteError::IdentityUnverified) => Err(DialError::Dropped(format!(
                "{} must be selected to sign in again",
                self.cluster
            ))),
            Err(refusal) => Err(DialError::Fatal(gate_failure(&refusal))),
        }
    }

    /// Whether the files as they are now still give the definition the forward started on.
    async fn check_definition(&self) -> Result<(), ForwardFailure> {
        let (reply, rx) = oneshot::channel();
        let sent = self
            .requests
            .send(Request::CheckWritePin {
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

    /// The gate, for the moment an upgrade is sent: the files, then the manager's state. A
    /// refusal is left in `refused`, since the client's error can't carry it.
    fn send_check(&self, refused: Arc<Mutex<Option<ForwardFailure>>>) -> SendCheck {
        let access = self.clone();
        SendCheck::new(move || {
            let (access, refused) = (access.clone(), Arc::clone(&refused));
            async move {
                let checked = match access.check_definition().await {
                    Ok(()) => access.check().await,
                    Err(failure) => Err(failure),
                };
                checked.map_err(|failure| {
                    let text = failure.to_string();
                    *refused.lock().unwrap_or_else(|e| e.into_inner()) = Some(failure);
                    text
                })
            }
            .boxed()
        })
    }

    /// `result`, after ending every forward on this connection if its sign-in was refused.
    fn after<T>(&self, result: Result<T, DialError>) -> Result<T, DialError> {
        if let Err(DialError::Fatal(failure)) = &result
            && failure.kind == ForwardFailureKind::Unauthorized
        {
            self.signals.end_connection(&self.connection);
        }
        result
    }
}

/// The failure that ends every forward on a connection whose sign-in was refused.
pub(crate) fn unauthorized() -> ForwardFailure {
    ForwardFailure::new(
        ForwardFailureKind::Unauthorized,
        "the cluster no longer accepts this sign-in; sign in and start the forward again",
    )
}

/// A failed lookup for `target`, in Flint's words. `what` is what RBAC would have refused.
fn lookup_failure(error: WireError, target: &ForwardTarget, what: &str) -> DialError {
    match error {
        WireError::Status(401) => DialError::Fatal(unauthorized()),
        WireError::Status(403) => forbidden(what),
        WireError::Status(404) => gone(target),
        _ => DialError::Dropped(format!("Flint could not reach {target}")),
    }
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

/// The pod behind a connection is no longer the one chosen.
fn replaced(target: &ForwardTarget) -> DialError {
    match target {
        ForwardTarget::Pod(_) => DialError::Fatal(ForwardFailure::new(
            ForwardFailureKind::TargetGone,
            format!("{target} was replaced by another pod of that name"),
        )),
        ForwardTarget::Service(_) => DialError::Dropped(format!("the pod behind {target} changed")),
    }
}

/// A pod a connection may go to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Backend {
    pod: String,
    uid: String,
    port: u16,
}

fn uid_of(pod: &Pod) -> Option<&str> {
    pod.metadata.uid.as_deref()
}

fn has_finished(pod: &Pod) -> bool {
    let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
    matches!(phase, Some("Succeeded" | "Failed"))
}

/// The pod and the pod's port a connection goes to. A pod target must have the pinned uid.
async fn resolve(
    client: &kube::Client,
    namespace: &str,
    target: &ForwardTarget,
    remote: u16,
    pinned: &Mutex<Option<String>>,
) -> Result<Backend, DialError> {
    match target {
        ForwardTarget::Pod(name) => {
            let pod: Pod = wire::get(client, namespace, name)
                .await
                .map_err(|e| lookup_failure(e, target, "reading this pod"))?;
            let uid = uid_of(&pod)
                .ok_or_else(|| DialError::Dropped(format!("Flint could not confirm {target}")))?
                .to_owned();
            {
                let mut pinned = pinned.lock().unwrap_or_else(|e| e.into_inner());
                if pinned.get_or_insert_with(|| uid.clone()) != &uid {
                    return Err(replaced(target));
                }
            }
            if has_finished(&pod) {
                return Err(DialError::Fatal(ForwardFailure::new(
                    ForwardFailureKind::TargetGone,
                    format!("{target} has finished"),
                )));
            }
            Ok(Backend {
                pod: name.clone(),
                uid,
                port: remote,
            })
        }
        ForwardTarget::Service(name) => {
            for backend in service_backends(client, namespace, target, name, remote)
                .await?
                .into_iter()
                .take(MAX_BACKENDS_TRIED)
            {
                match wire::get::<Pod>(client, namespace, &backend.pod).await {
                    Ok(pod) if uid_of(&pod) == Some(&backend.uid) && !has_finished(&pod) => {
                        return Ok(backend);
                    }
                    Ok(_) | Err(WireError::Status(404)) => {}
                    Err(e) => return Err(lookup_failure(e, target, "reading this service's pods")),
                }
            }
            Err(DialError::Dropped(format!(
                "{target} has no ready pod for port {remote}"
            )))
        }
    }
}

/// The ready pods behind Service `name`'s port `remote`, as the server has them now.
async fn service_backends(
    client: &kube::Client,
    namespace: &str,
    target: &ForwardTarget,
    name: &str,
    remote: u16,
) -> Result<Vec<Backend>, DialError> {
    let unreachable = |e| lookup_failure(e, target, "reading this service and its endpoints");
    let service: Service = wire::get(client, namespace, name)
        .await
        .map_err(unreachable)?;
    let params = ListParams::default().labels(&format!("kubernetes.io/service-name={name}"));
    let slices: Vec<EndpointSlice> = wire::list(client, namespace, &params)
        .await
        .map_err(unreachable)?;
    Ok(pick_backends(&service, &slices, remote))
}

/// Whether `backend` is still the pod there and, for a Service, still its ready endpoint at that
/// port, read after its channel opened.
async fn confirm(
    client: &kube::Client,
    namespace: &str,
    target: &ForwardTarget,
    remote: u16,
    backend: &Backend,
) -> Result<(), DialError> {
    match wire::get::<Pod>(client, namespace, &backend.pod).await {
        Ok(pod) if uid_of(&pod) == Some(&backend.uid) && !has_finished(&pod) => {}
        Ok(_) | Err(WireError::Status(404)) => return Err(replaced(target)),
        Err(WireError::Status(401)) => return Err(DialError::Fatal(unauthorized())),
        Err(WireError::Status(403)) => return Err(forbidden("reading this pod")),
        Err(_) => {
            return Err(DialError::Dropped(format!(
                "Flint could not confirm {target}"
            )));
        }
    }
    if let ForwardTarget::Service(name) = target
        && !service_backends(client, namespace, target, name, remote)
            .await?
            .contains(backend)
    {
        return Err(replaced(target));
    }
    Ok(())
}

/// The ready pods behind `service`'s `port`, by name, each with its uid and the port it listens
/// on there. An endpoint that names no uid can't be checked, so it is left out.
fn pick_backends(service: &Service, slices: &[EndpointSlice], port: u16) -> Vec<Backend> {
    let declared = service
        .spec
        .as_ref()
        .and_then(|spec| spec.ports.as_ref())
        .and_then(|ports| ports.iter().find(|p| p.port == i32::from(port)));
    let Some(declared) = declared else {
        return Vec::new();
    };
    let wanted = declared.name.as_deref().unwrap_or("");
    let mut ready: Vec<Backend> = Vec::new();
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
                .and_then(|r| Some((r.name.clone()?, r.uid.clone()?)));
            if let (true, Some((pod, uid))) = (is_ready, pod) {
                ready.push(Backend {
                    pod,
                    uid,
                    port: number,
                });
            }
        }
    }
    ready.sort();
    ready.dedup();
    ready
}

impl Dialer for KubeDialer {
    fn check(&self) -> BoxFuture<'static, Result<(), ForwardFailure>> {
        let access = self.access.clone();
        async move { access.check().await }.boxed()
    }

    fn ended(&self) -> BoxFuture<'static, ForwardState> {
        let mut ended = self.ended.clone();
        async move {
            match ended.wait_for(Option::is_some).await {
                Ok(state) => state.clone().unwrap_or(ForwardState::Stopped),
                Err(_) => ForwardState::Failed(gate_failure(&WriteRefusal::Stopped.into())),
            }
        }
        .boxed()
    }

    fn probe(&self) -> BoxFuture<'static, Result<String, DialError>> {
        let access = self.access.clone();
        let request = self.request.clone();
        let uid = Arc::clone(&self.uid);
        async move {
            let probed = async {
                access.check().await.map_err(DialError::Fatal)?;
                let client = access.signed_in().await?;
                let backend = resolve(
                    &client,
                    &request.namespace,
                    &request.target,
                    request.remote_port,
                    &uid,
                )
                .await?;
                Ok(backend.pod)
            }
            .await;
            access.after(probed)
        }
        .boxed()
    }

    fn dial(&self) -> BoxFuture<'static, Result<Upstream, DialError>> {
        let access = self.access.clone();
        let request = self.request.clone();
        let uid = Arc::clone(&self.uid);
        async move {
            let dialed = dial_once(&access, &request, &uid).await;
            access.after(dialed)
        }
        .boxed()
    }
}

/// Opens one connection to the target, carrying nothing until it is confirmed.
async fn dial_once(
    access: &Access,
    request: &ForwardRequest,
    uid: &Mutex<Option<String>>,
) -> Result<Upstream, DialError> {
    access.check().await.map_err(DialError::Fatal)?;
    let client = access.signed_in().await?;
    let backend = resolve(
        &client,
        &request.namespace,
        &request.target,
        request.remote_port,
        uid,
    )
    .await?;
    let refused = Arc::new(Mutex::new(None));
    let upgrade = upgrade_client(client.clone(), access.send_check(Arc::clone(&refused)));
    let opened = wire::open(&upgrade, &request.namespace, &backend.pod, backend.port).await;
    let tunnel = match opened {
        Ok(tunnel) => tunnel,
        Err(error) => {
            // Refused by the gate at send time, or it closed meanwhile.
            if let Some(failure) = refused.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Err(DialError::Fatal(failure));
            }
            // Before the gate, which a stuck manager would hold this refusal behind.
            if error == WireError::Status(401) {
                return Err(DialError::Fatal(unauthorized()));
            }
            access.check().await.map_err(DialError::Fatal)?;
            return Err(match error {
                WireError::Status(403) => forbidden("port-forwarding to this pod"),
                WireError::Status(404) if matches!(request.target, ForwardTarget::Pod(_)) => {
                    gone(&request.target)
                }
                _ => DialError::Dropped(format!(
                    "Flint could not open a connection to {}",
                    backend.pod
                )),
            });
        }
    };
    // Nothing is carried until the channel is known to be on the pod chosen.
    confirm(
        &client,
        &request.namespace,
        &request.target,
        request.remote_port,
        &backend,
    )
    .await?;
    access.check().await.map_err(DialError::Fatal)?;
    Ok(Upstream {
        stream: Box::pin(tunnel.stream),
        failed: tunnel.failed.map(|reported| reported.is_ok()).boxed(),
        pod: backend.pod,
        _connection: Some(tunnel.task),
    })
}

impl KubeDialer {
    pub(crate) fn new(
        requests: crate::core_queue::CoreQueue<Request>,
        request: ForwardRequest,
        grant: ForwardGrant,
        admitted: Option<Arc<AdmittedMember>>,
    ) -> Self {
        let uid = match request.target {
            ForwardTarget::Pod(_) => request.uid.clone(),
            ForwardTarget::Service(_) => None,
        };
        Self {
            access: Access {
                requests,
                id: grant.id,
                cluster: request.cluster.clone(),
                server: request.server.clone(),
                pin: grant.pin,
                connection: grant.connection,
                admitted,
                signals: grant.signals,
            },
            request,
            ended: grant.ended,
            uid: Arc::new(Mutex::new(uid)),
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
    format!("{scheme}://127.0.0.1:{local_port}")
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
        slow_close: AtomicBool,
    }

    /// Takes a moment to drop, as a connection slow to close does.
    struct SlowDrop;

    impl Drop for SlowDrop {
        fn drop(&mut self) {
            std::thread::sleep(Duration::from_millis(200));
        }
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

        fn ended(&self) -> BoxFuture<'static, ForwardState> {
            futures::future::pending().boxed()
        }

        fn probe(&self) -> BoxFuture<'static, Result<String, DialError>> {
            let answer = self.probe.lock().unwrap().clone();
            async move { answer.map_or(Ok("web-1".to_owned()), Err) }.boxed()
        }

        fn dial(&self) -> BoxFuture<'static, Result<Upstream, DialError>> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let failure = self.fail.lock().unwrap().clone();
            let report = self.report_error.load(Ordering::SeqCst);
            let slow = self.slow_close.load(Ordering::SeqCst);
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
                    } else if slow {
                        async {
                            let _closing = SlowDrop;
                            futures::future::pending::<bool>().await
                        }
                        .boxed()
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
            uid: None,
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
            "targetRef": {"kind": "Pod", "name": pod, "uid": format!("uid-{pod}")},
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
        let first = |port| pick_backends(&svc, &slices, port).into_iter().next();
        let web_1 = |port| Backend {
            pod: "web-1".to_owned(),
            uid: "uid-web-1".to_owned(),
            port,
        };
        assert_eq!(first(80), Some(web_1(8080)));
        assert_eq!(first(9090), Some(web_1(9100)));
        assert_eq!(first(81), None, "not a Service port");
    }

    #[test]
    fn a_service_without_a_ready_pod_has_no_backend() {
        let svc = service(json!([{"port": 80}]));
        let slices = [slice(
            json!([{"port": 8080}]),
            json!([endpoint("web-0", false)]),
        )];
        assert!(pick_backends(&svc, &slices, 80).is_empty());
        assert!(pick_backends(&svc, &[], 80).is_empty());
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
            "http://127.0.0.1:8080"
        );
        assert_eq!(browser_url(8080, None, None), "http://127.0.0.1:8080");
        assert_eq!(
            browser_url(8443, Some("https"), None),
            "https://127.0.0.1:8443"
        );
        assert_eq!(
            browser_url(8443, Some("web"), Some("HTTPS")),
            "https://127.0.0.1:8443"
        );
    }

    // An endpoint without a uid can't be checked against the pod it names.
    #[test]
    fn an_endpoint_without_a_uid_is_left_out() {
        let svc = service(json!([{"port": 80}]));
        let slices = [slice(
            json!([{"port": 8080}]),
            json!([{"conditions": {"ready": true}, "addresses": ["10.0.0.1"],
                    "targetRef": {"kind": "Pod", "name": "web-0"}}]),
        )];
        assert!(pick_backends(&svc, &slices, 80).is_empty());
    }

    /// A gate that answers only once released.
    #[derive(Default)]
    struct SlowGate {
        release: tokio::sync::Notify,
    }

    impl Dialer for Arc<SlowGate> {
        fn check(&self) -> BoxFuture<'static, Result<(), ForwardFailure>> {
            let gate = Arc::clone(self);
            async move {
                gate.release.notified().await;
                Ok(())
            }
            .boxed()
        }

        fn ended(&self) -> BoxFuture<'static, ForwardState> {
            futures::future::pending().boxed()
        }

        fn probe(&self) -> BoxFuture<'static, Result<String, DialError>> {
            futures::future::pending().boxed()
        }

        fn dial(&self) -> BoxFuture<'static, Result<Upstream, DialError>> {
            futures::future::pending().boxed()
        }
    }

    // Closed while its admission is pending, it never takes the port.
    #[tokio::test]
    async fn a_forward_stopped_while_starting_never_binds() {
        let spare = bind_loopback(None).unwrap();
        let port = spare.local_addr().unwrap().port();
        drop(spare);
        let gate = Arc::new(SlowGate::default());
        let request = request(Some(port));
        let (forward, channels) = PortForward::new(request.clone());
        let task = tokio::spawn(run(Arc::new(gate.clone()), request, channels));
        tokio::task::yield_now().await;
        forward.stop();
        gate.release.notify_one();
        task.await.unwrap();
        assert_eq!(forward.snapshot().state, ForwardState::Stopped);
        assert_eq!(forward.snapshot().local_port, None);
        assert!(bind_loopback(Some(port)).is_ok(), "the port stayed free");
    }

    // The budget is shared: a second forward's connection waits for the first to end.
    #[tokio::test]
    async fn connections_of_all_forwards_share_one_budget() {
        let dialer = Arc::new(FakeDialer::default());
        let relays = Arc::new(Semaphore::new(1));
        let mut ports = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..2 {
            let request = request(None);
            let (forward, channels) = PortForward::with_relays(request.clone(), relays.clone());
            tokio::spawn(run(Arc::new(dialer.clone()), request, channels));
            let mut updates = forward.updates();
            let shown = wait_for(&mut updates, |s| s.local_port.is_some()).await;
            ports.push(shown.local_port.unwrap());
            handles.push(forward);
        }
        let first = round_trip(ports[0]).await;
        let mut second = TcpStream::connect((Ipv4Addr::LOCALHOST, ports[1]))
            .await
            .unwrap();
        second.write_all(b"ping").await.unwrap();
        let mut back = [0; 4];
        let early =
            tokio::time::timeout(Duration::from_millis(200), second.read_exact(&mut back)).await;
        assert!(early.is_err(), "over the budget");
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 1);
        drop(first);
        tokio::time::timeout(Duration::from_secs(5), second.read_exact(&mut back))
            .await
            .expect("carried once there was room")
            .unwrap();
    }

    // Nothing offers the address once the port can be taken by another program.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_says_stopping_before_it_releases_the_port() {
        let dialer = Arc::new(FakeDialer::default());
        dialer.slow_close.store(true, Ordering::SeqCst);
        let (forward, mut updates, port) = started(&dialer, None).await;
        // A connection slow to close holds it between the two states, to be seen there.
        let _stream = round_trip(port).await;
        wait_for(&mut updates, |s| s.connections == 1 && s.pod.is_some()).await;
        forward.stop();
        let first = wait_for(&mut updates, |s| s.state != ForwardState::Listening).await;
        assert_eq!(first.state, ForwardState::Stopping);
        wait_for(&mut updates, |s| s.state == ForwardState::Stopped).await;
        closed(port).await;
    }

    #[test]
    fn a_request_debug_leaves_out_the_server() {
        let mut shown = request(None);
        shown.server = Some(Some("https://user:secret@example.test".into()));
        assert!(!format!("{shown:?}").contains("secret"));
    }
}
