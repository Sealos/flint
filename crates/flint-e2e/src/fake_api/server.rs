//! The HTTPS server: routes requests to the fixtures, runs watch streams and applies faults.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
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

use super::faults::{self, Effect, Fault, Faults};
use super::fixtures::{FIXTURES_DIR, Fixtures, Gvr, Layout};
use super::kubeconfig;
use super::route::{Query, Route, Verb};
use super::row::FakeRow;
use super::state::{ApiError, State};
use super::tls;
use super::watch::{self as stream, Filter, Outgoing, WatchStep};

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
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fixtures: PathBuf::from(FIXTURES_DIR),
            streaming_lists: true,
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

struct Watcher {
    tx: mpsc::UnboundedSender<Outgoing>,
}

struct Inner {
    fixtures: Fixtures,
    options: Options,
    state: Arc<Mutex<State>>,
    faults: Mutex<Faults>,
    scripts: Mutex<HashMap<Gvr, VecDeque<Vec<WatchStep>>>>,
    watchers: Mutex<HashMap<Gvr, Vec<Watcher>>>,
    requests: Mutex<Vec<RequestRecord>>,
    shutdown: watch::Receiver<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("fake API server lock")
}

/// A fake Kubernetes API server on `https://127.0.0.1:<port>`. It stops when dropped.
pub struct FakeApiServer {
    inner: Arc<Inner>,
    addr: SocketAddr,
    ca_pem: String,
    accept: JoinHandle<()>,
    _shutdown: watch::Sender<()>,
}

impl Drop for FakeApiServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl FakeApiServer {
    /// Starts a server on the committed fixtures. Must be called inside a tokio runtime.
    pub async fn start() -> Result<Self> {
        FakeApiServer::start_with(Options::default()).await
    }

    pub async fn start_with(options: Options) -> Result<Self> {
        let fixtures = Fixtures::load(&Layout::new(&options.fixtures))?;
        let tls = tls::generate()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("binding the fake API server")?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, shutdown) = watch::channel(());
        let inner = Arc::new(Inner {
            state: Arc::new(Mutex::new(State::new(&fixtures))),
            fixtures,
            options,
            faults: Mutex::default(),
            scripts: Mutex::default(),
            watchers: Mutex::default(),
            requests: Mutex::default(),
            shutdown,
        });
        let acceptor = tokio_rustls::TlsAcceptor::from(tls.server_config.clone());
        let accept = tokio::spawn(accept_loop(listener, acceptor, inner.clone()));
        Ok(FakeApiServer {
            inner,
            addr,
            ca_pem: tls.ca_pem,
            accept,
            _shutdown: shutdown_tx,
        })
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

    /// A kubeconfig for this server (context [`kubeconfig::CONTEXT`]).
    pub fn kubeconfig(&self, namespace: Option<&str>) -> kube::config::Kubeconfig {
        kubeconfig::kubeconfig(&self.url(), &self.ca_pem, namespace)
    }

    /// Writes [`FakeApiServer::kubeconfig`] to `path`.
    pub fn write_kubeconfig(&self, path: &Path, namespace: Option<&str>) -> Result<()> {
        kubeconfig::write(path, &self.kubeconfig(namespace))
    }

    /// A kube client for this server, built the way Flint builds them.
    pub async fn client(&self) -> Result<kube::Client> {
        Ok(flint_core::cluster::connect(&self.kubeconfig(None), kubeconfig::CONTEXT).await?)
    }

    /// The current rows of a resource in all namespaces.
    pub fn rows(&self, gvr: &Gvr) -> Vec<FakeRow> {
        lock(&self.inner.state).rows(gvr, None, &Query::default())
    }

    /// One current row.
    pub fn row(&self, gvr: &Gvr, namespace: Option<&str>, name: &str) -> Option<FakeRow> {
        lock(&self.inner.state).row(gvr, namespace, name)
    }

    /// The index of a Table column by name.
    pub fn column(&self, gvr: &Gvr, name: &str) -> Option<usize> {
        let columns = lock(&self.inner.state).columns(gvr);
        columns
            .as_array()?
            .iter()
            .position(|c| c.get("name").and_then(Value::as_str) == Some(name))
    }

    /// Adds or replaces a row without telling open watches: the change shows only in the next
    /// list, as a change missed during a broken watch would.
    pub fn set_row(&self, gvr: &Gvr, row: FakeRow) {
        lock(&self.inner.state).upsert_row(gvr, row);
    }

    /// Removes a row without telling open watches (see [`FakeApiServer::set_row`]).
    pub fn remove_row(&self, gvr: &Gvr, namespace: Option<&str>, name: &str) -> bool {
        lock(&self.inner.state)
            .remove_row(gvr, namespace, name)
            .is_some()
    }

    /// Queues a script for the next watch request on `gvr`. Each watch request takes the next
    /// queued script; after its steps (and without one), the stream stays open until the
    /// request's `timeoutSeconds`, sending only what [`FakeApiServer::send_watch`] sends.
    pub fn script_watch(&self, gvr: &Gvr, steps: Vec<WatchStep>) {
        lock(&self.inner.scripts)
            .entry(gvr.clone())
            .or_default()
            .push_back(steps);
    }

    /// Sends a step to every open watch on `gvr` (row steps change the rows once, then every
    /// watch whose namespace and selectors match gets the event). Returns how many open
    /// watches there were.
    pub fn send_watch(&self, gvr: &Gvr, step: WatchStep) -> usize {
        let mut state = lock(&self.inner.state);
        let outgoing = match &step {
            WatchStep::Added(row) => Outgoing::Event("ADDED", state.upsert_row(gvr, row.clone())),
            WatchStep::Modified(row) => {
                Outgoing::Event("MODIFIED", state.upsert_row(gvr, row.clone()))
            }
            WatchStep::Deleted(row) => match state.remove_row(gvr, row.namespace(), row.name()) {
                Some(last) => Outgoing::Event("DELETED", last),
                None => return 0,
            },
            _ => Outgoing::Step(step),
        };
        let mut watchers = lock(&self.inner.watchers);
        let list = watchers.entry(gvr.clone()).or_default();
        list.retain(|w| w.tx.send(outgoing.clone()).is_ok());
        list.len()
    }

    /// How many watches on `gvr` are open now.
    pub fn open_watches(&self, gvr: &Gvr) -> usize {
        let mut watchers = lock(&self.inner.watchers);
        let list = watchers.entry(gvr.clone()).or_default();
        list.retain(|w| !w.tx.is_closed());
        list.len()
    }

    /// Waits until at least `count` watches on `gvr` are open.
    pub async fn wait_for_watches(&self, gvr: &Gvr, count: usize, within: Duration) -> Result<()> {
        tokio::time::timeout(within, async {
            while self.open_watches(gvr) < count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .with_context(|| format!("no {count} open watch(es) on {gvr} within {within:?}"))
    }

    /// Injects a fault into matching requests until cleared (or used up).
    pub fn inject(&self, fault: Fault) {
        lock(&self.inner.faults).add(fault);
    }

    pub fn clear_faults(&self) {
        lock(&self.inner.faults).clear();
    }

    /// Every request so far, oldest first.
    pub fn requests(&self) -> Vec<RequestRecord> {
        lock(&self.inner.requests).clone()
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
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("no matching request")
    }
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    inner: Arc<Inner>,
) {
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            continue;
        };
        let acceptor = acceptor.clone();
        let inner = inner.clone();
        let mut shutdown = inner.shutdown.clone();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else {
                return;
            };
            let service = service_fn(move |req| handle(inner.clone(), req));
            let conn = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tls), service);
            tokio::select! {
                _ = conn => {}
                _ = shutdown.changed() => {}
            }
        });
    }
}

fn full(code: u16, content_type: &str, body: &Value) -> Response<Body> {
    let mut text = body.to_string();
    text.push('\n');
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

fn json(body: &Value) -> Response<Body> {
    full(200, "application/json", body)
}

fn error(err: &ApiError) -> Response<Body> {
    full(err.code, "application/json", &err.body)
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

async fn handle(inner: Arc<Inner>, req: Request<Incoming>) -> Result<Response<Body>, io::Error> {
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
    };
    let effect = lock(&inner.faults).take(verb, &route);
    let response = match effect {
        Some(Effect::DropConnection) => {
            lock(&inner.requests).push(record);
            return Err(io::Error::other("fault: connection dropped"));
        }
        Some(Effect::Forbidden) => error(&ApiError::new(
            403,
            "Forbidden",
            faults::forbidden_message(verb, &route),
        )),
        Some(Effect::Status(code)) => error(&ApiError::new(
            code,
            reason(code),
            format!("injected fault: {code}"),
        )),
        Some(Effect::Delay(delay)) => {
            tokio::time::sleep(delay).await;
            respond(&inner, req.method(), &route, verb, &query, &accept)
        }
        None => respond(&inner, req.method(), &route, verb, &query, &accept),
    };
    record.status = Some(response.status().as_u16());
    lock(&inner.requests).push(record);
    Ok(response)
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
    let wants_table = accept.contains("as=Table");
    let result =
        match route {
            Route::Version => Ok(json(&fixtures.version)),
            Route::DiscoveryCore | Route::DiscoveryGroups => {
                if accept.contains("as=APIGroupDiscoveryList") {
                    let body = if *route == Route::DiscoveryCore {
                        &fixtures.discovery_core
                    } else {
                        &fixtures.discovery_groups
                    };
                    Ok(full(200, DISCOVERY_CONTENT_TYPE, body))
                } else {
                    Err(ApiError::not_found(
                        "only aggregated discovery is recorded; ask for apidiscovery.k8s.io/v2",
                    ))
                }
            }
            Route::OpenApiIndex => Ok(json(&fixtures.openapi_index)),
            Route::OpenApiDocument(path) => fixtures.openapi.get(path).map(json).ok_or_else(|| {
                ApiError::not_found(format!("OpenAPI document {path} is not recorded"))
            }),
            Route::Collection { gvr, namespace } => known(inner, gvr, namespace.as_deref())
                .and_then(|resource| {
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
                        start_watch(inner, gvr, ns, query)
                    } else if !resource.supports("list") {
                        Err(ApiError::new(
                            405,
                            "MethodNotAllowed",
                            "list is not supported",
                        ))
                    } else {
                        let state = lock(&inner.state);
                        if wants_table {
                            state.list_table(gvr, ns, query).map(|v| json(&v))
                        } else {
                            state
                                .list_objects(&fixtures.resources, gvr, ns, query)
                                .map(|v| json(&v))
                        }
                    }
                }),
            Route::Object {
                gvr,
                namespace,
                name,
            } => known(inner, gvr, namespace.as_deref()).and_then(|_| {
                let state = lock(&inner.state);
                let ns = namespace.as_deref();
                if wants_table {
                    state.table_object(gvr, ns, name).map(|v| json(&v))
                } else {
                    state
                        .object(&fixtures.resources, gvr, ns, name)
                        .map(|v| json(&v))
                }
            }),
            Route::Subresource { subresource, .. } => Err(ApiError::not_found(format!(
                "subresource {subresource} is not recorded"
            ))),
            Route::Other => Err(ApiError::not_found(
                "the server could not find the requested resource",
            )),
        };
    result.unwrap_or_else(|e| error(&e))
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

fn start_watch(
    inner: &Arc<Inner>,
    gvr: &Gvr,
    namespace: Option<&str>,
    query: &Query,
) -> Result<Response<Body>, ApiError> {
    if query.send_initial_events && !inner.options.streaming_lists {
        return Err(ApiError::new(
            422,
            "Invalid",
            "sendInitialEvents: Forbidden: sendInitialEvents is forbidden for watch unless the \
             WatchList feature gate is enabled",
        ));
    }
    let (tx, rx) = mpsc::unbounded_channel();
    {
        // Initial events and registration under the state lock, so no change falls between.
        let state = lock(&inner.state);
        if query.send_initial_events {
            for row in state.rows(gvr, namespace, query) {
                let _ = tx.send(Outgoing::Event("ADDED", row));
            }
            let _ = tx.send(Outgoing::InitialEventsEnd);
        }
        if let Some(steps) = lock(&inner.scripts)
            .get_mut(gvr)
            .and_then(VecDeque::pop_front)
        {
            for step in steps {
                let _ = tx.send(Outgoing::Step(step));
            }
        }
        lock(&inner.watchers)
            .entry(gvr.clone())
            .or_default()
            .push(Watcher { tx });
    }
    let (body_tx, body_rx) = mpsc::channel(64);
    let filter = Filter {
        gvr: gvr.clone(),
        namespace: namespace.map(str::to_owned),
        query: query.clone(),
    };
    tokio::spawn(stream::run(
        inner.state.clone(),
        filter,
        rx,
        body_tx,
        inner.shutdown.clone(),
    ));
    let frames = futures::stream::unfold(body_rx, |mut rx| async move {
        rx.recv().await.map(|frame| (frame, rx))
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(StreamBody::new(frames).boxed_unsync())
        .expect("a valid response"))
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
