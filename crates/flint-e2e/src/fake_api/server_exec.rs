//! The server's exec endpoint and helpers: `pods/<pod>/exec` is checked as a cluster checks it
//! ([`super::super::exec`]), upgraded to a WebSocket, and served by the scripted shell
//! ([`super::super::shell`]). Tests find the sessions, read what the client sent, print, end
//! them and break them.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt as _, StreamExt as _};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role};

use super::super::exec::{
    self, CLOSE, Control, ERROR, ExecEnd, ExecImage, ExecQuery, FakeExec, Launch, RESIZE, STDERR,
    STDIN, STDOUT, TtySize, V5,
};
use super::super::logs;
use super::super::shell::{Event, Host, Shell};
use super::super::state::ApiError;
use super::{Body, FakeApiServer, Gvr, Inner, Query, RequestRecord, Route, error, full, lock};

/// How long a session waits for the client to go after its close frame.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

impl FakeApiServer {
    /// Sets what an exec finds in a container (`None`: the pod's only container, picked as the
    /// API server picks it): whether it has bash, runs as root, and how its runtime reports a
    /// missing command. Without this, the image in the pod's spec decides ([`ExecImage::of`]):
    /// the fixtures' nginx pods have bash, podinfo and pods made with [`crate::fake_api::FakePod::new`]
    /// (busybox) only `sh`. Panics if the pod or container does not exist.
    pub fn set_exec_image(
        &self,
        namespace: &str,
        pod: &str,
        container: Option<&str>,
        image: ExecImage,
    ) {
        let state = lock(&self.inner.state);
        let object = state
            .object(&Gvr::core("pods"), Some(namespace), pod)
            .unwrap_or_else(|e| panic!("{}", e.body["message"]));
        let container = logs::select_container(&object, container)
            .unwrap_or_else(|e| panic!("{}", e.body["message"]));
        let key = (namespace.to_owned(), pod.to_owned(), container);
        lock(&self.inner.execs).set_image(key, image);
    }

    /// Every exec session the server upgraded so far, oldest first, open or ended. A command
    /// that could not start (bash missing) is a session that ended at once.
    pub fn exec_sessions(&self) -> Vec<FakeExec> {
        lock(&self.inner.execs).sessions()
    }

    /// The newest exec session.
    pub fn last_exec(&self) -> Option<FakeExec> {
        self.exec_sessions().pop()
    }

    /// What the client of the newest exec session sent on stdin (empty without one).
    pub fn last_exec_stdin(&self) -> Vec<u8> {
        self.last_exec().map(|s| s.stdin()).unwrap_or_default()
    }

    /// How many exec sessions are open.
    pub fn open_exec_sessions(&self) -> usize {
        self.exec_sessions().iter().filter(|s| s.is_open()).count()
    }

    /// Waits until exactly `count` exec sessions are open: up after opening shells, down to 0
    /// after closing them.
    pub async fn wait_for_exec_sessions(&self, count: usize, within: Duration) -> Result<()> {
        tokio::time::timeout(within, async {
            while self.open_exec_sessions() != count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "not {count} open exec session(s) within {within:?} but {}; last requests:\n{}",
                self.open_exec_sessions(),
                self.last_requests(10)
            )
        })
    }

    /// Breaks every open exec session's connection without a close frame. Returns how many.
    pub fn drop_exec_sessions(&self) -> usize {
        self.exec_sessions()
            .iter()
            .filter(|s| s.drop_connection())
            .count()
    }

    /// Every exec request so far, oldest first, refused or not. [`RequestRecord::parsed_query`]
    /// shows its `command` and `container`.
    pub fn exec_requests(&self) -> Vec<RequestRecord> {
        self.requests()
            .into_iter()
            .filter(|r| Route::parse(&r.path).subresource() == Some("exec"))
            .collect()
    }
}

/// A plain-text refusal, as the kubelet sends one through the API server.
fn text(code: u16, message: &str) -> Response<Body> {
    full(code, "text/plain; charset=utf-8", message.to_owned())
}

/// What a proxy that does not pass upgrades on answers.
pub(super) fn refused_upgrade() -> Response<Body> {
    text(200, "this proxy does not pass WebSocket upgrades on\n")
}

/// An exec request: refused as the API server and the kubelet refuse it, or upgraded to a
/// session.
pub(super) fn open_exec(
    inner: &Arc<Inner>,
    req: Request<Incoming>,
    namespace: &str,
    pod: &str,
    query: &Query,
) -> Response<Body> {
    if req.method() != http::Method::GET && req.method() != http::Method::POST {
        return error(&ApiError::new(
            405,
            "MethodNotAllowed",
            "exec takes GET or POST",
        ));
    }
    let query = ExecQuery::new(query);
    let state = lock(&inner.state);
    let object = match state.object(&Gvr::core("pods"), Some(namespace), pod) {
        Ok(object) => object,
        Err(e) => return error(&e),
    };
    drop(state);
    let container = match logs::select_container(&object, query.container.as_deref()) {
        Ok(container) => container,
        Err(e) => return error(&e),
    };
    if let Err(message) = exec::check_host(&object) {
        return error(&ApiError::new(400, "BadRequest", message));
    }
    if !exec::is_websocket_upgrade(req.headers()) {
        return error(&ApiError::new(
            400,
            "BadRequest",
            "Upgrade request required",
        ));
    }
    if let Some(message) = query.streams_error() {
        return text(400, message);
    }
    if let Err(message) = exec::check_running(&object, &container) {
        return text(500, &message);
    }
    let offered = req
        .headers()
        .get(http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok());
    let Some(protocol) = exec::negotiate(offered) else {
        return text(
            400,
            &format!(
                "requested protocol(s) are not supported: {offered:?}; supports [{V5} {}]",
                exec::V4
            ),
        );
    };
    let key = req
        .headers()
        .get(http::header::SEC_WEBSOCKET_KEY)
        .map(|k| k.as_bytes().to_vec())
        .unwrap_or_default();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(&key);

    let key = (namespace.to_owned(), pod.to_owned(), container);
    let mut execs = lock(&inner.execs);
    let image = execs.image(&key, &object);
    let launch = exec::launch(&query.command, image, &execs.next_id());
    let (session, control) = FakeExec::new(&key, &query, protocol);
    execs.add(session.clone());
    drop(execs);
    let host = Host {
        hostname: pod.to_owned(),
        root: image.root,
    };
    let run = Run {
        session,
        launch,
        host,
        query,
        v5: protocol == V5,
        control,
        shutdown: inner.shutdown.clone(),
    };
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let ws =
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)
                        .await;
                run.serve(ws).await;
            }
            Err(_) => run.session.record_end(ExecEnd::ClientGone),
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::CONNECTION, "Upgrade")
        .header(http::header::SEC_WEBSOCKET_ACCEPT, accept)
        .header(http::header::SEC_WEBSOCKET_PROTOCOL, protocol)
        .body(Empty::new().map_err(|never| match never {}).boxed_unsync())
        .expect("a valid response")
}

type Ws = WebSocketStream<TokioIo<Upgraded>>;

/// An upgraded session, ready to run.
struct Run {
    session: FakeExec,
    launch: Launch,
    host: Host,
    query: ExecQuery,
    v5: bool,
    control: mpsc::UnboundedReceiver<Control>,
    shutdown: watch::Receiver<()>,
}

/// The sending half of a session.
struct Out {
    sink: SplitSink<Ws, Message>,
    session: FakeExec,
    stdout: bool,
    stderr: bool,
}

/// The client went away while the server wrote.
struct Gone;

impl Out {
    async fn send(&mut self, channel: u8, bytes: &[u8]) -> Result<(), Gone> {
        let wanted = match channel {
            STDOUT => self.stdout,
            STDERR => self.stderr,
            _ => true,
        };
        if !wanted {
            return Ok(());
        }
        if channel == STDOUT {
            self.session.record_stdout(bytes);
        }
        let frame = Bytes::from(exec::frame(channel, bytes));
        self.sink
            .send(Message::Binary(frame))
            .await
            .map_err(|_| Gone)
    }

    async fn close(&mut self) -> Result<(), Gone> {
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        };
        self.sink
            .send(Message::Close(Some(frame)))
            .await
            .map_err(|_| Gone)
    }

    /// The command ended: its status, then a close frame.
    async fn status(&mut self, status: &Value) -> Result<(), Gone> {
        self.send(ERROR, status.to_string().as_bytes()).await?;
        self.close().await
    }

    /// Sends what the shell did; returns how the session ended, if it did.
    async fn deliver(&mut self, events: Vec<Event>) -> Option<ExecEnd> {
        for event in events {
            let sent = match event {
                Event::Stdout(bytes) => self.send(STDOUT, &bytes).await,
                Event::Stderr(bytes) => self.send(STDERR, &bytes).await,
                Event::Exit(code) => {
                    return Some(match self.status(&exec::exit_status(code)).await {
                        Ok(()) => ExecEnd::Exited(code),
                        Err(Gone) => ExecEnd::ClientGone,
                    });
                }
            };
            if sent.is_err() {
                return Some(ExecEnd::ClientGone);
            }
        }
        None
    }
}

/// A resize message: `{"Width":80,"Height":24}`.
fn parse_resize(data: &[u8]) -> Option<TtySize> {
    let value: Value = serde_json::from_slice(data).ok()?;
    let dimension = |key: &str| value[key].as_u64().and_then(|v| u16::try_from(v).ok());
    Some(TtySize {
        width: dimension("Width")?,
        height: dimension("Height")?,
    })
}

impl Run {
    async fn serve(self, ws: Ws) {
        let (sink, mut stream) = ws.split();
        let session = self.session.clone();
        let mut out = Out {
            sink,
            session: session.clone(),
            stdout: self.query.stdout,
            stderr: self.query.stderr,
        };
        let end = self.drive(&mut out, &mut stream).await;
        session.record_end(end.clone());
        if end == ExecEnd::Dropped {
            // Gone without a close frame or TLS's close_notify, as a broken network leaves it.
            return;
        }
        // Wait for the client to answer the close frame or go away, then close the connection
        // as Go's TLS does (close_notify), so the client reads a clean end.
        let _ = tokio::time::timeout(CLOSE_WAIT, async {
            while let Some(Ok(_)) = stream.next().await {}
        })
        .await;
        if let Ok(mut ws) = out.sink.reunite(stream) {
            let _ = tokio::time::timeout(CLOSE_WAIT, ws.get_mut().shutdown()).await;
        }
    }

    async fn drive(self, out: &mut Out, stream: &mut SplitStream<Ws>) -> ExecEnd {
        let Run {
            session,
            launch,
            host,
            query,
            v5,
            mut control,
            mut shutdown,
        } = self;
        // The empty stdout frame a real server sends first.
        if out.send(STDOUT, b"").await.is_err() {
            return ExecEnd::ClientGone;
        }
        let mut shell = match launch {
            Launch::Fail(status) => {
                let message = status["message"].as_str().unwrap_or("").to_owned();
                return match out.status(&status).await {
                    Ok(()) => ExecEnd::Failed(message),
                    Err(Gone) => ExecEnd::ClientGone,
                };
            }
            Launch::Print(bytes, code) => {
                let events = vec![Event::Stdout(bytes), Event::Exit(code)];
                return out.deliver(events).await.unwrap_or(ExecEnd::ClientGone);
            }
            Launch::Run(program) => {
                let (shell, events) = Shell::start(program, host, query.tty);
                if let Some(end) = out.deliver(events).await {
                    return end;
                }
                shell
            }
        };
        loop {
            let events = tokio::select! {
                biased;
                _ = shutdown.changed() => return ExecEnd::Dropped,
                control = control.recv() => match control {
                    Some(Control::Print(channel, bytes)) => {
                        if out.send(channel, &bytes).await.is_err() {
                            return ExecEnd::ClientGone;
                        }
                        continue;
                    }
                    Some(Control::Exit(code)) => vec![Event::Exit(code)],
                    Some(Control::Close) => {
                        return match out.close().await {
                            Ok(()) => ExecEnd::Closed,
                            Err(Gone) => ExecEnd::ClientGone,
                        };
                    }
                    Some(Control::Drop) | None => return ExecEnd::Dropped,
                },
                message = stream.next() => match message {
                    Some(Ok(Message::Binary(bytes))) => match bytes.split_first() {
                        Some((&STDIN, data)) => {
                            session.record_stdin(data);
                            shell.input(data)
                        }
                        Some((&RESIZE, data)) => match parse_resize(data) {
                            Some(size) => {
                                session.record_resize(size);
                                shell.resize(size)
                            }
                            None => Vec::new(),
                        },
                        // v4 has no close signal: the frame is for a channel that does not exist.
                        Some((&CLOSE, [STDIN])) if v5 => {
                            session.record_stdin_closed();
                            shell.close_stdin()
                        }
                        _ => Vec::new(),
                    },
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return ExecEnd::ClientGone,
                    Some(Ok(_)) => Vec::new(),
                },
                () = std::future::ready(()), if shell.is_flooding() => shell.pump(),
            };
            if let Some(end) = out.deliver(events).await {
                return end;
            }
        }
    }
}

#[cfg(test)]
#[path = "server_exec_tests.rs"]
mod tests;
