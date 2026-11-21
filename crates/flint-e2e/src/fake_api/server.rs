//! The HTTPS server: routes requests to the fixtures, runs watch streams and applies faults.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Full, StreamBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::clock::Clock;
use super::exec::{self as remote, Execs};
use super::faults::{self, Effect, Fault, Faults};
use super::fixtures::{FIXTURES_DIR, Fixtures, Gvr, Layout};
use super::kubeconfig;
use super::logs::{self, Chunk, LogQuery, Logs};
use super::route::{Query, Route, Verb};
use super::row::FakeRow;
use super::state::{ApiError, Change, State};
use super::tls;
use super::watch::{self as stream, Filter, Outgoing, Stream, WatchStep};

type Body = UnsyncBoxBody<Bytes, io::Error>;

/// Content type of aggregated discovery responses.
const DISCOVERY_CONTENT_TYPE: &str =
    "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList";

/// How to start a fake server.
#[derive(Debug, Clone)]
pub struct Options {
    /// The fixtures directory (default: the ones committed with this crate).
    pub fixtures: PathBuf,
    /// Whether streaming lists (`sendInitialEvents=true`) are served. When off, they are
    /// rejected the way a server without the WatchList feature rejects them, so clients fall
    /// back to list and watch.
    pub streaming_lists: bool,
    /// The server's "now": fixture timestamps are shifted so the recording instant is this.
    pub now: SystemTime,
    /// Resources answered without Tables (plain lists and plain watch events), as aggregated
    /// API servers that ignore `as=Table` do.
    pub plain_only: Vec<Gvr>,
    /// Whether the kubelet's exec refusals (no streams, a container not running) come before
    /// the upgrade, as plain-text HTTP errors (the default, recorded on kind v1.37), or after
    /// it, as an `InternalError` status on the error channel. See `exec`'s module docs for
    /// which clusters do which.
    pub kubelet_websockets: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fixtures: PathBuf::from(FIXTURES_DIR),
            streaming_lists: true,
            now: SystemTime::now(),
            plain_only: Vec::new(),
            kubelet_websockets: true,
        }
    }
}

/// One request the server received.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub method: Method,
    pub path: String,
    /// The raw query string.
    pub query: String,
    pub verb: Verb,
    /// `None` when the connection was dropped instead of answered.
    pub status: Option<u16>,
    /// The connection it came on: connections are numbered from 1, in the order the server
    /// accepts them ([`connection_mark_at`]).
    pub connection: u64,
}

impl RequestRecord {
    pub fn parsed_query(&self) -> Query {
        Query::parse(Some(&self.query))
    }

    /// Whether this request was about `gvr` (in any namespace).
    pub fn is_about(&self, gvr: &Gvr) -> bool {
        Route::parse(&self.path).gvr() == Some(gvr)
    }
}

impl std::fmt::Display for RequestRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = self
            .status
            .map_or_else(|| "dropped".to_owned(), |s| s.to_string());
        write!(
            f,
            "{} {}?{} -> {status}",
            self.method, self.path, self.query
        )
    }
}

struct Watcher {
    tx: mpsc::UnboundedSender<Outgoing>,
}

/// Scripts by resource and namespace (`None`: a watch in any scope).
type Scripts = HashMap<(Gvr, Option<String>), VecDeque<Vec<WatchStep>>>;

struct Inner {
    fixtures: Arc<Fixtures>,
    options: Options,
    clock: Clock,
    state: Arc<Mutex<State>>,
    /// Pod logs and their open streams. Locked after `state` when both are needed.
    logs: Mutex<Logs>,
    /// Exec sessions and the images tests set. Locked after `state` when both are needed.
    execs: Mutex<Execs>,
    faults: Mutex<Faults>,
    scripts: Mutex<Scripts>,
    watchers: Mutex<HashMap<Gvr, Vec<Watcher>>>,
    requests: Mutex<Vec<RequestRecord>>,
    /// Every request as it arrived, before faults or answers: what the app sent, and when.
    arrivals: Mutex<Vec<RequestRecord>>,
    /// Requests whose handler is running: answered or not yet, before their response (a
    /// stream's body is not counted). A client that gives up on a request closes its connection,
    /// which ends the handler ([`FakeApiServer::requests_in_flight`]).
    in_flight: AtomicUsize,
    /// How many connections have been accepted: the number of the last one.
    connections: AtomicU64,
    /// Each accepted connection's number by its client's address, until
    /// [`connection_mark_at`] looks its own probe up (the others are never removed; a test opens
    /// few connections).
    accepted: Mutex<HashMap<SocketAddr, u64>>,
    shutdown: watch::Receiver<()>,
}

/// Every fake server running in this process, by URL, so the app harness can read the requests
/// of the servers a kubeconfig names ([`arrivals_at`]).
static RUNNING: Mutex<Vec<(String, Weak<Inner>)>> = Mutex::new(Vec::new());

/// The requests that have reached the fake server at `url` in this process so far, as they
/// arrived (before any fault delays them, and without a status), oldest first; `None` when no
/// fake server runs there (another cluster, such as kind, or a closed port).
pub fn arrivals_at(url: &str) -> Option<Vec<RequestRecord>> {
    let url = url.trim_end_matches('/');
    let running = lock(&RUNNING);
    // The newest first: a port can be reused while a stopped server's tasks still wind down.
    let inner = running
        .iter()
        .rev()
        .find(|(u, _)| u == url)
        .and_then(|(_, inner)| inner.upgrade())?;
    drop(running);
    Some(lock(&inner.arrivals).clone())
}

/// A mark between the connections the fake server at `url` has accepted so far and those it
/// accepts from now on: requests on connections numbered above it come from clients that
/// connected after this call. `None` when no fake server runs there.
///
/// The server may still be reading requests sent on older connections (a client that has just
/// exited, say), so counting requests is not enough to tell old clients from new ones. This
/// opens a connection of its own and waits until the server has accepted it: connections are
/// accepted in order, so every one opened before is numbered at or below the mark.
pub fn connection_mark_at(url: &str) -> Option<u64> {
    let url = url.trim_end_matches('/');
    let inner = {
        let running = lock(&RUNNING);
        running
            .iter()
            .rev()
            .find(|(u, _)| u == url)
            .and_then(|(_, inner)| inner.upgrade())?
    };
    let addr: SocketAddr = url.strip_prefix("https://")?.parse().ok()?;
    let Ok(probe) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)) else {
        return Some(inner.connections.load(Ordering::SeqCst));
    };
    let Ok(local) = probe.local_addr() else {
        return Some(inner.connections.load(Ordering::SeqCst));
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(id) = lock(&inner.accepted).remove(&local) {
            return Some(id);
        }
        if std::time::Instant::now() > deadline {
            return Some(inner.connections.load(Ordering::SeqCst));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Counts a request in [`Inner::in_flight`] until its handler ends, answered or dropped with its
/// connection.
struct InFlight(Arc<Inner>);

impl InFlight {
    fn enter(inner: &Arc<Inner>) -> InFlight {
        inner.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(inner.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("fake API server lock")
}

/// A fake Kubernetes API server on `https://127.0.0.1:<port>`. It stops when dropped.
///
/// Methods that take a [`Gvr`] panic when the fixtures do not serve it, with a suggestion:
/// a typo should fail the test, not wait for a timeout.
pub struct FakeApiServer {
    inner: Arc<Inner>,
    addr: SocketAddr,
    ca_pem: String,
    accept: JoinHandle<()>,
    _shutdown: watch::Sender<()>,
    /// The runtime of a server started with [`FakeApiServer::start_blocking`].
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for FakeApiServer {
    fn drop(&mut self) {
        self.accept.abort();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl FakeApiServer {
    /// Starts a server on the committed fixtures. Must be called inside a tokio runtime, which
    /// then runs the server.
    pub async fn start() -> Result<Self> {
        FakeApiServer::start_with(Options::default()).await
    }

    pub async fn start_with(options: Options) -> Result<Self> {
        let fixtures = if options.fixtures == Path::new(FIXTURES_DIR) {
            Fixtures::shared_default()?
        } else {
            Arc::new(Fixtures::load(&Layout::new(&options.fixtures))?)
        };
        for gvr in &options.plain_only {
            if fixtures.resources.get(gvr).is_none() {
                bail!("Options::plain_only names {gvr}, which the fixtures do not serve");
            }
        }
        let tls = tls::generate()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("binding the fake API server")?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, shutdown) = watch::channel(());
        let clock = Clock::new(options.now);
        let inner = Arc::new(Inner {
            state: Arc::new(Mutex::new(State::new(&fixtures))),
            logs: Mutex::new(Logs::new(clock.clone())),
            execs: Mutex::default(),
            clock,
            fixtures,
            options,
            faults: Mutex::default(),
            scripts: Mutex::default(),
            watchers: Mutex::default(),
            requests: Mutex::default(),
            arrivals: Mutex::default(),
            in_flight: AtomicUsize::new(0),
            connections: AtomicU64::new(0),
            accepted: Mutex::default(),
            shutdown,
        });
        let acceptor = tokio_rustls::TlsAcceptor::from(tls.server_config.clone());
        let accept = tokio::spawn(accept_loop(listener, acceptor, inner.clone()));
        {
            let mut running = lock(&RUNNING);
            running.retain(|(_, inner)| inner.strong_count() > 0);
            running.push((format!("https://{addr}"), Arc::downgrade(&inner)));
        }
        Ok(FakeApiServer {
            inner,
            addr,
            ca_pem: tls.ca_pem,
            accept,
            _shutdown: shutdown_tx,
            runtime: None,
        })
    }

    /// Starts a server on its own runtime, for callers without one (GPUI tests, process
    /// harnesses). Its async helpers still need a runtime to be awaited on.
    pub fn start_blocking(options: Options) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("starting the fake API server's runtime")?;
        let mut server = runtime.block_on(FakeApiServer::start_with(options))?;
        server.runtime = Some(runtime);
        Ok(server)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `https://127.0.0.1:<port>`
    pub fn url(&self) -> String {
        format!("https://{}", self.addr)
    }

    /// The PEM of the CA that signed the server's certificate.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// The recorded fixtures the server started from.
    pub fn fixtures(&self) -> &Fixtures {
        &self.inner.fixtures
    }

    /// A kubeconfig for this server, context [`kubeconfig::CONTEXT`].
    pub fn kubeconfig(&self, namespace: Option<&str>) -> kube::config::Kubeconfig {
        self.kubeconfig_named(kubeconfig::CONTEXT, namespace)
    }

    /// A kubeconfig for this server with its own context name, to merge with others
    /// ([`kubeconfig::merge`], [`write_kubeconfigs`]).
    pub fn kubeconfig_named(
        &self,
        context: &str,
        namespace: Option<&str>,
    ) -> kube::config::Kubeconfig {
        kubeconfig::kubeconfig(&self.url(), &self.ca_pem, context, namespace)
    }

    /// Writes [`FakeApiServer::kubeconfig`] to `path`.
    pub fn write_kubeconfig(&self, path: &Path, namespace: Option<&str>) -> Result<()> {
        kubeconfig::write(path, &self.kubeconfig(namespace))
    }

    /// A kube client for this server, built the way Flint builds them.
    pub async fn client(&self) -> Result<kube::Client> {
        Ok(flint_core::cluster::connect(&self.kubeconfig(None), kubeconfig::CONTEXT).await?)
    }

    /// A client that can reach this server's exec endpoints, for tests of them. Flint's own
    /// sessions get theirs from the write gate.
    #[cfg(test)]
    pub(crate) async fn exec_client(&self) -> Result<kube::Client> {
        Ok(flint_core::cluster::connect_unrestricted_for_tests(
            &self.kubeconfig(None),
            kubeconfig::CONTEXT,
        )
        .await?)
    }

    /// Panics unless the fixtures serve `gvr`, suggesting what was meant.
    fn require(&self, gvr: &Gvr) {
        let resources = &self.inner.fixtures.resources;
        if resources.get(gvr).is_some() {
            return;
        }
        let hint = resources
            .suggest(&gvr.resource)
            .map(|r| format!("; did you mean {}?", r.gvr))
            .unwrap_or_default();
        panic!("the fake API server does not serve {gvr}{hint}");
    }

    /// The listable resource with this plural (`"deployments"` is `apps/v1/deployments`; for
    /// a plural in several groups the core group wins). Panics if there is none.
    pub fn gvr(&self, plural: &str) -> Gvr {
        let resources = &self.inner.fixtures.resources;
        match resources.by_plural(plural) {
            Some(r) => r.gvr.clone(),
            None => {
                let hint = resources
                    .suggest(plural)
                    .map(|r| format!("; did you mean {:?}?", r.gvr.resource))
                    .unwrap_or_default();
                panic!("the fake API server serves no {plural:?}{hint}")
            }
        }
    }

    /// The current rows of a resource in all namespaces.
    pub fn rows(&self, gvr: &Gvr) -> Vec<FakeRow> {
        self.require(gvr);
        lock(&self.inner.state).rows(gvr, None, &Query::default())
    }

    /// One current row.
    pub fn row(&self, gvr: &Gvr, namespace: Option<&str>, name: &str) -> Option<FakeRow> {
        self.require(gvr);
        lock(&self.inner.state).row(gvr, namespace, name)
    }

    /// The index of a Table column by name.
    pub fn column(&self, gvr: &Gvr, name: &str) -> Option<usize> {
        self.require(gvr);
        let columns = lock(&self.inner.state).columns(gvr);
        columns
            .as_array()?
            .iter()
            .position(|c| c.get("name").and_then(Value::as_str) == Some(name))
    }

    /// A new row with the named cells set (`&[("Ready", "1/1")]`, parsed for the column type)
    /// and every other cell at its default; the Name cell is `name`. Panics on an unknown
    /// column.
    pub fn new_row(
        &self,
        gvr: &Gvr,
        namespace: Option<&str>,
        name: &str,
        cells: &[(&str, &str)],
    ) -> FakeRow {
        self.require(gvr);
        lock(&self.inner.state)
            .new_row(gvr, namespace, name, cells)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Adds or replaces a row without telling open watches: the change shows only in the next
    /// list, and watches resuming from before it get a 410, as for a change missed during a
    /// broken watch. The row's object follows.
    pub fn set_row(&self, gvr: &Gvr, row: FakeRow) {
        self.require(gvr);
        lock(&self.inner.state).silent_upsert(gvr, row);
    }

    /// Removes a row and its object without telling open watches (see
    /// [`FakeApiServer::set_row`]).
    pub fn remove_row(&self, gvr: &Gvr, namespace: Option<&str>, name: &str) -> bool {
        self.require(gvr);
        lock(&self.inner.state).silent_remove(gvr, namespace, name)
    }

    /// Adds or replaces a full object (what plain GETs and lists return), silently; a row is
    /// made for it if there is none. Panics if it has no `metadata.name`.
    pub fn set_object(&self, gvr: &Gvr, object: Value) {
        self.require(gvr);
        lock(&self.inner.state)
            .set_object(gvr, object)
            .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Removes an object and its row, silently.
    pub fn remove_object(&self, gvr: &Gvr, namespace: Option<&str>, name: &str) -> bool {
        self.remove_row(gvr, namespace, name)
    }

    /// Queues a script for the next watch request on `gvr`, in any namespace. Each watch takes
    /// the next queued script; after its steps (and without one), the stream stays open until
    /// the request's `timeoutSeconds`, sending only what [`FakeApiServer::send_watch`] sends.
    /// Row steps change the rows; only the scripted watch is told at once.
    pub fn script_watch(&self, gvr: &Gvr, steps: Vec<WatchStep>) {
        self.require(gvr);
        lock(&self.inner.scripts)
            .entry((gvr.clone(), None))
            .or_default()
            .push_back(steps);
    }

    /// Like [`FakeApiServer::script_watch`], for the next watch in `namespace` only. These are
    /// taken before scripts for any namespace.
    pub fn script_watch_in(&self, gvr: &Gvr, namespace: &str, steps: Vec<WatchStep>) {
        self.require(gvr);
        lock(&self.inner.scripts)
            .entry((gvr.clone(), Some(namespace.to_owned())))
            .or_default()
            .push_back(steps);
    }

    /// Drops every queued script.
    pub fn clear_scripts(&self) {
        lock(&self.inner.scripts).clear();
    }

    /// Sends a step to every open watch on `gvr`: row steps change the rows once, then every
    /// watch whose namespace and selectors match gets the event (a `MODIFIED` that leaves a
    /// selector arrives as `DELETED`). Returns how many watches were open. A `Deleted` of an
    /// unknown object changes nothing and returns 0.
    pub fn send_watch(&self, gvr: &Gvr, step: WatchStep) -> usize {
        self.require(gvr);
        let mut state = lock(&self.inner.state);
        let outgoing = match step.row_change() {
            Some(("DELETED", row)) if state.row(gvr, row.namespace(), row.name()).is_none() => {
                return 0;
            }
            Some((ty, row)) => Outgoing::Event(Box::new(state.change(gvr, ty, row.clone()))),
            None => Outgoing::Step(step),
        };
        self.broadcast(gvr, &outgoing)
    }

    /// Sends to every open watch on `gvr`; returns how many were open. Called with the state
    /// locked, so watches get changes in the order they were made.
    fn broadcast(&self, gvr: &Gvr, outgoing: &Outgoing) -> usize {
        let mut watchers = lock(&self.inner.watchers);
        let list = watchers.entry(gvr.clone()).or_default();
        list.retain(|w| w.tx.send(outgoing.clone()).is_ok());
        list.len()
    }

    /// How many watches on `gvr` are open now.
    pub fn open_watches(&self, gvr: &Gvr) -> usize {
        self.require(gvr);
        let mut watchers = lock(&self.inner.watchers);
        let list = watchers.entry(gvr.clone()).or_default();
        list.retain(|w| !w.tx.is_closed());
        list.len()
    }

    /// Waits until at least `count` watches on `gvr` are open.
    pub async fn wait_for_watches(&self, gvr: &Gvr, count: usize, within: Duration) -> Result<()> {
        self.require(gvr);
        tokio::time::timeout(within, async {
            while self.open_watches(gvr) < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "no {count} open watch(es) on {gvr} within {within:?}; last requests:\n{}",
                self.last_requests(10)
            )
        })
    }

    /// Injects a fault into matching requests until cleared (or used up).
    pub fn inject(&self, fault: Fault) {
        if let Some(gvr) = fault.resource() {
            self.require(gvr);
        }
        lock(&self.inner.faults).add(fault);
    }

    pub fn clear_faults(&self) {
        lock(&self.inner.faults).clear();
    }

    /// Every request answered (or dropped) so far, oldest first.
    pub fn requests(&self) -> Vec<RequestRecord> {
        lock(&self.inner.requests).clone()
    }

    /// How many requests the server is handling now: arrived and not yet answered (a fault may
    /// be delaying them), until the client gives up on them and closes the connection. A
    /// stream's body, once its headers are sent, does not count.
    pub fn requests_in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::SeqCst)
    }

    /// Every request that has arrived so far, oldest first, including those still delayed by a
    /// fault or not yet answered (their `status` is `None`).
    pub fn arrivals(&self) -> Vec<RequestRecord> {
        lock(&self.inner.arrivals).clone()
    }

    fn last_requests(&self, n: usize) -> String {
        let requests = lock(&self.inner.requests);
        requests[requests.len().saturating_sub(n)..]
            .iter()
            .map(|r| format!("  {r}\n"))
            .collect()
    }

    /// Waits until a request matching `pred` was received.
    pub async fn wait_for_request(
        &self,
        within: Duration,
        pred: impl Fn(&RequestRecord) -> bool,
    ) -> Result<RequestRecord> {
        tokio::time::timeout(within, async {
            loop {
                if let Some(r) = self.requests().into_iter().find(&pred) {
                    return r;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "no matching request within {within:?}; last requests:\n{}",
                self.last_requests(10)
            )
        })
    }
}

/// Writes one kubeconfig for several servers, each under its own context name. The first is
/// the current context.
pub fn write_kubeconfigs(
    path: &Path,
    servers: &[(&FakeApiServer, &str, Option<&str>)],
) -> Result<()> {
    let configs = servers
        .iter()
        .map(|(server, context, ns)| server.kubeconfig_named(context, *ns))
        .collect();
    kubeconfig::write(path, &kubeconfig::merge(configs))
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    inner: Arc<Inner>,
) {
    loop {
        let Ok((tcp, peer)) = listener.accept().await else {
            continue;
        };
        let connection = inner.connections.fetch_add(1, Ordering::SeqCst) + 1;
        lock(&inner.accepted).insert(peer, connection);
        let acceptor = acceptor.clone();
        let inner = inner.clone();
        let mut shutdown = inner.shutdown.clone();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else {
                return;
            };
            let service = service_fn(move |req| handle(inner.clone(), req, connection));
            let conn = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tls), service)
                .with_upgrades();
            tokio::select! {
                _ = conn => {}
                _ = shutdown.changed() => {}
            }
        });
    }
}

fn full(code: u16, content_type: &str, text: String) -> Response<Body> {
    Response::builder()
        .status(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(http::header::CONTENT_TYPE, content_type)
        .body(
            Full::new(Bytes::from(text))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .expect("a valid response")
}

fn json_with(code: u16, content_type: &str, body: &Value) -> Response<Body> {
    let mut text = body.to_string();
    text.push('\n');
    full(code, content_type, text)
}

fn error(err: &ApiError) -> Response<Body> {
    json_with(err.code, "application/json", &err.body)
}

fn reason(code: u16) -> &'static str {
    match code {
        400 => "BadRequest",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "NotFound",
        405 => "MethodNotAllowed",
        409 => "Conflict",
        410 => "Expired",
        422 => "Invalid",
        429 => "TooManyRequests",
        503 => "ServiceUnavailable",
        504 => "Timeout",
        _ => "InternalError",
    }
}

async fn handle(
    inner: Arc<Inner>,
    req: Request<Incoming>,
    connection: u64,
) -> Result<Response<Body>, io::Error> {
    let route = Route::parse(req.uri().path());
    let query = Query::parse(req.uri().query());
    let verb = route.verb(req.method(), &query);
    let accept = req
        .headers()
        .get(http::header::ACCEPT)
        .and_then(|a| a.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut record = RequestRecord {
        method: req.method().clone(),
        path: req.uri().path().to_owned(),
        query: req.uri().query().unwrap_or("").to_owned(),
        verb,
        status: None,
        connection,
    };
    lock(&inner.arrivals).push(record.clone());
    let _in_flight = InFlight::enter(&inner);
    let upgrade = remote::is_websocket_upgrade(req.headers());
    let effect = lock(&inner.faults).take(verb, &route, upgrade);
    let response = match effect {
        Some(Effect::DropConnection) => {
            lock(&inner.requests).push(record);
            return Err(io::Error::other("fault: connection dropped"));
        }
        Some(Effect::RefuseUpgrade) => execs::refused_upgrade(),
        Some(Effect::Forbidden) => error(&ApiError::new(
            403,
            "Forbidden",
            faults::forbidden_message(verb, &route),
        )),
        Some(Effect::ForbiddenAs(rbac_verb)) => error(&ApiError::new(
            403,
            "Forbidden",
            faults::forbidden_message_as(&rbac_verb, &route),
        )),
        Some(Effect::Status(code)) => error(&ApiError::new(
            code,
            reason(code),
            format!("injected fault: {code}"),
        )),
        Some(Effect::Delay(delay)) => {
            tokio::time::sleep(delay).await;
            serve(&inner, req, upgrade, &route, verb, &query, &accept)
        }
        Some(Effect::StripUpgrade) => serve(&inner, req, false, &route, verb, &query, &accept),
        None => serve(&inner, req, upgrade, &route, verb, &query, &accept),
    };
    record.status = Some(response.status().as_u16());
    lock(&inner.requests).push(record);
    Ok(response)
}

/// Answers a request: an exec is upgraded (or refused), anything else answered at once.
fn serve(
    inner: &Arc<Inner>,
    req: Request<Incoming>,
    upgrade: bool,
    route: &Route,
    verb: Verb,
    query: &Query,
    accept: &str,
) -> Response<Body> {
    match route {
        Route::Subresource {
            gvr,
            namespace: Some(ns),
            name,
            subresource,
        } if *gvr == Gvr::core("pods") && subresource == "exec" => {
            execs::open_exec(inner, req, upgrade, ns, name, query)
        }
        _ => respond(inner, req.method(), route, verb, query, accept),
    }
}

/// A JSON response in the server's time.
fn rebased(inner: &Inner, mut body: Value) -> Response<Body> {
    inner.clock.rebase(&mut body);
    json_with(200, "application/json", &body)
}

fn respond(
    inner: &Arc<Inner>,
    method: &Method,
    route: &Route,
    verb: Verb,
    query: &Query,
    accept: &str,
) -> Response<Body> {
    if method != Method::GET {
        return error(&ApiError::new(
            405,
            "MethodNotAllowed",
            "the fake API server is read-only",
        ));
    }
    let fixtures = &inner.fixtures;
    let table_for =
        |gvr: &Gvr| accept.contains("as=Table") && !inner.options.plain_only.contains(gvr);
    let result = match route {
        Route::Version => Ok(json_with(200, "application/json", &fixtures.version)),
        Route::DiscoveryCore | Route::DiscoveryGroups => {
            if accept.contains("as=APIGroupDiscoveryList") {
                let body = if *route == Route::DiscoveryCore {
                    &fixtures.discovery_core
                } else {
                    &fixtures.discovery_groups
                };
                Ok(json_with(200, DISCOVERY_CONTENT_TYPE, body))
            } else {
                Err(ApiError::not_found(
                    "only aggregated discovery is recorded; ask for apidiscovery.k8s.io/v2",
                ))
            }
        }
        Route::OpenApiIndex => Ok(json_with(200, "application/json", &fixtures.openapi_index)),
        Route::OpenApiDocument(path) => fixtures
            .openapi
            .get(path)
            .map(|d| json_with(200, "application/json", d))
            .ok_or_else(|| ApiError::not_found(format!("OpenAPI document {path} is not recorded"))),
        Route::Collection { gvr, namespace } => {
            known(inner, gvr, namespace.as_deref()).and_then(|resource| {
                let ns = namespace.as_deref();
                if verb == Verb::Watch {
                    if !resource.supports("watch") {
                        return Err(ApiError::new(
                            405,
                            "MethodNotAllowed",
                            format!(
                                "watch is not supported on resources of kind \"{}.{}\"",
                                gvr.resource, gvr.group
                            ),
                        ));
                    }
                    start_watch(inner, gvr, ns, query, !table_for(gvr))
                } else if !resource.supports("list") {
                    Err(ApiError::new(
                        405,
                        "MethodNotAllowed",
                        "list is not supported",
                    ))
                } else if query.send_initial_events {
                    Err(ApiError::new(
                        422,
                        "Invalid",
                        "sendInitialEvents: Forbidden: sendInitialEvents is forbidden for list",
                    ))
                } else {
                    let state = lock(&inner.state);
                    let body = if table_for(gvr) {
                        state.list_table(gvr, ns, query)
                    } else {
                        state.list_objects(gvr, ns, query)
                    };
                    body.map(|v| rebased(inner, v))
                }
            })
        }
        Route::Object {
            gvr,
            namespace,
            name,
        } => known(inner, gvr, namespace.as_deref()).and_then(|_| {
            let state = lock(&inner.state);
            let ns = namespace.as_deref();
            let body = if table_for(gvr) {
                state.table_object(gvr, ns, name)
            } else {
                state.object(gvr, ns, name)
            };
            body.map(|v| rebased(inner, v))
        }),
        Route::Subresource {
            gvr,
            namespace: Some(ns),
            name,
            subresource,
        } if *gvr == Gvr::core("pods") && subresource == "log" => open_log(inner, ns, name, query),
        Route::Subresource { subresource, .. } => Err(ApiError::not_found(format!(
            "subresource {subresource} is not recorded"
        ))),
        Route::Other => Err(ApiError::not_found(
            "the server could not find the requested resource",
        )),
    };
    result.unwrap_or_else(|e| error(&e))
}

/// A pod's log: checked as the API server and the kubelet check it (see [`logs`]), then streamed.
fn open_log(
    inner: &Inner,
    namespace: &str,
    pod: &str,
    query: &Query,
) -> Result<Response<Body>, ApiError> {
    let query = LogQuery::parse(query, pod)?;
    let state = lock(&inner.state);
    let pods = Gvr::core("pods");
    let object = state.object(&pods, Some(namespace), pod)?;
    let container = logs::select_container(&object, query.container.as_deref())?;
    let instance = logs::instance(&object, &container, query.previous)?;
    let key = (namespace.to_owned(), pod.to_owned(), container);
    let chunks = lock(&inner.logs).open(&key, instance, &query);
    drop(state);
    let frames = futures::stream::unfold(chunks, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk: Chunk| (chunk.map(hyper::body::Frame::data), rx))
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/plain")
        .body(StreamBody::new(frames).boxed_unsync())
        .expect("a valid response"))
}

/// The resource of a request, if the server serves it in that scope.
fn known<'a>(
    inner: &'a Inner,
    gvr: &Gvr,
    namespace: Option<&str>,
) -> Result<&'a super::resources::ResourceInfo, ApiError> {
    inner
        .fixtures
        .resources
        .get(gvr)
        .filter(|r| r.namespaced || namespace.is_none())
        .ok_or_else(|| ApiError::not_found("the server could not find the requested resource"))
}

/// What a new watch sends before live events: the initial state, replayed changes, or a 410.
fn opening_events(
    state: &State,
    gvr: &Gvr,
    namespace: Option<&str>,
    query: &Query,
) -> Result<Vec<Outgoing>, ApiError> {
    let added = |state: &State| {
        state
            .rows(gvr, namespace, query)
            .into_iter()
            .map(|row| {
                Outgoing::Event(Box::new(Change {
                    gvr: gvr.clone(),
                    resource_version: state.resource_version(),
                    ty: "ADDED",
                    object: state.object_of(gvr, &row),
                    row,
                    previous: None,
                }))
            })
            .collect::<Vec<_>>()
    };
    if query.send_initial_events {
        let mut events = added(state);
        events.push(Outgoing::InitialEventsEnd);
        return Ok(events);
    }
    match query.resource_version.as_deref() {
        // No version: the current state as synthetic ADDED events, then changes.
        None | Some("" | "0") => Ok(added(state)),
        Some(rv) => {
            let rv: u64 = rv.parse().map_err(|_| {
                ApiError::new(
                    400,
                    "BadRequest",
                    format!("invalid resource version {rv:?}"),
                )
            })?;
            Ok(match state.changes_since(gvr, rv) {
                Some(changes) => changes
                    .into_iter()
                    .map(|c| Outgoing::Event(Box::new(c)))
                    .collect(),
                None => vec![Outgoing::Step(WatchStep::Expired)],
            })
        }
    }
}

fn start_watch(
    inner: &Arc<Inner>,
    gvr: &Gvr,
    namespace: Option<&str>,
    query: &Query,
    plain: bool,
) -> Result<Response<Body>, ApiError> {
    if query.send_initial_events {
        if !inner.options.streaming_lists {
            return Err(ApiError::new(
                422,
                "Invalid",
                "sendInitialEvents: Forbidden: sendInitialEvents is forbidden for watch unless \
                 the WatchList feature gate is enabled",
            ));
        }
        if query.resource_version_match.as_deref() != Some("NotOlderThan") {
            return Err(ApiError::new(
                422,
                "Invalid",
                "resourceVersionMatch: Forbidden: sendInitialEvents requires setting \
                 resourceVersionMatch to NotOlderThan",
            ));
        }
    }
    if !plain {
        State::check_table_fields(query)?;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    {
        // Opening events and registration under the state lock, so no change falls between.
        let state = lock(&inner.state);
        for outgoing in opening_events(&state, gvr, namespace, query)? {
            let _ = tx.send(outgoing);
        }
        let mut scripts = lock(&inner.scripts);
        let script = namespace
            .and_then(|ns| {
                scripts
                    .get_mut(&(gvr.clone(), Some(ns.to_owned())))
                    .and_then(VecDeque::pop_front)
            })
            .or_else(|| {
                scripts
                    .get_mut(&(gvr.clone(), None))
                    .and_then(VecDeque::pop_front)
            });
        for step in script.into_iter().flatten() {
            let _ = tx.send(Outgoing::Step(step));
        }
        lock(&inner.watchers)
            .entry(gvr.clone())
            .or_default()
            .push(Watcher { tx });
    }
    let (body_tx, body_rx) = mpsc::channel(64);
    tokio::spawn(stream::run(Stream {
        state: inner.state.clone(),
        clock: inner.clock.clone(),
        filter: Filter {
            gvr: gvr.clone(),
            namespace: namespace.map(str::to_owned),
            query: query.clone(),
            plain,
        },
        steps: rx,
        body: body_tx,
        shutdown: inner.shutdown.clone(),
    }));
    let frames = futures::stream::unfold(body_rx, |mut rx| async move {
        rx.recv().await.map(|frame| (frame, rx))
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(StreamBody::new(frames).boxed_unsync())
        .expect("a valid response"))
}

#[path = "server_pods.rs"]
mod pods;

#[path = "server_exec.rs"]
mod execs;

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
