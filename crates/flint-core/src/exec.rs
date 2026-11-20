//! Exec sessions: an interactive shell (or any command) in a pod's container, over the API
//! server's WebSocket exec endpoint with a TTY.
//!
//! A front end opens one with [`crate::CoreHandle::exec`] and gets an [`ExecSession`]. The
//! session runs as its own task on the core runtime, separate from the cluster manager, so it
//! keeps running when the user switches clusters (or the kubeconfig reloads) and ends only when
//! it is closed or dropped. Output from the pod goes into a [`TerminalModel`] on the core
//! runtime; the front end reads coalesced [`ExecSnapshot`]s (at most one per [`FRAME`], the
//! first after a quiet spell at once, so typing echoes without delay) and sends keystrokes,
//! pastes, resizes, scrolling, and selections back through the session. After the command
//! exits, the screen stays readable, scrollable, and selectable until the session is closed.
//!
//! Without an explicit command, `bash` is tried first and `/bin/sh` when the container has no
//! `bash`: the runtime reports a missing executable on the exec status channel before the
//! command produces any output, which tells it apart from a shell that ran and failed. (Docker
//! prints it on the TTY and exits with 126 or 127 instead; that is recognised too.)
//!
//! Errors are shown through [`ClusterError::kube`] and [`crate::cluster::error_chain`], which
//! never quote exec auth plugin commands, environments, or output.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{FutureExt, SinkExt};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::Client;
use kube::api::{Api, AttachParams};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::cluster::{ClusterError, ClusterId};
use crate::protection::WriteRefusal;
use crate::recheck::Recheck;
use crate::terminal::{SelectionKind, Side, TerminalModel, TerminalSize, TerminalSnapshot};

#[cfg(any(test, feature = "test-support"))]
pub mod fake;

/// What the API server answers an exec that arrives without its upgrade headers.
const UPGRADE_REQUIRED: &str = "Upgrade request required";

/// Commands tried in order when the request names none.
pub const DEFAULT_SHELLS: &[&str] = &["bash", "/bin/sh"];

/// The shortest time between two snapshots while output streams in: one frame at 60 Hz.
/// Terminals need a tighter budget than lists (ADR-0002's ~100 ms) or typing feels laggy.
pub const FRAME: Duration = Duration::from_millis(16);

/// How much output is read (and parsed) at once, and the size of kube's stdin and stdout pipes
/// (its default of 1 KiB splits output into many small reads).
const IO_BUFFER: usize = 64 * 1024;

/// Output up to this size is kept to recognise a missing-executable error printed on the TTY.
const EARLY_OUTPUT: usize = 4096;

/// How long to wait for the exit status after the output stream ends.
const STATUS_WAIT: Duration = Duration::from_secs(2);

/// How long output left in the pipe is read after the connection ends. kube ends the stream
/// after a clean close, so this bound only matters when the connection broke: its message
/// loop then returns without ending the stream, which would otherwise never reach its end.
const DRAIN_WAIT: Duration = Duration::from_millis(250);

/// What to run, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub cluster: ClusterId,
    pub namespace: String,
    pub pod: String,
    /// `None` lets the API server pick, which works when the pod has one container.
    pub container: Option<String>,
    /// `None` runs a shell: [`DEFAULT_SHELLS`] in order.
    pub command: Option<Vec<String>>,
    /// The terminal's size at the start; later changes go through [`ExecSession::resize`].
    pub size: TerminalSize,
}

impl ExecRequest {
    /// A shell in `pod`, in its only (or default) container, at 80 by 24.
    pub fn shell(
        cluster: impl Into<ClusterId>,
        namespace: impl Into<String>,
        pod: impl Into<String>,
    ) -> Self {
        Self {
            cluster: cluster.into(),
            namespace: namespace.into(),
            pod: pod.into(),
            container: None,
            command: None,
            size: TerminalSize::default(),
        }
    }

    #[must_use]
    pub fn container(mut self, container: impl Into<String>) -> Self {
        self.container = Some(container.into());
        self
    }

    #[must_use]
    pub fn command(mut self, command: Vec<String>) -> Self {
        self.command = Some(command);
        self
    }

    #[must_use]
    pub fn size(mut self, size: TerminalSize) -> Self {
        self.size = size;
        self
    }

    /// The commands to try, in order.
    fn candidates(&self) -> Vec<Vec<String>> {
        match &self.command {
            Some(command) => vec![command.clone()],
            None => DEFAULT_SHELLS
                .iter()
                .map(|s| vec![(*s).to_owned()])
                .collect(),
        }
    }
}

/// Where a session is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecState {
    /// Opening the connection (again, while falling back to the next shell).
    Connecting,
    /// The command runs; `command` is the one that started.
    Running { command: Vec<String> },
    /// The command ended. `code` is its exit code when the API server reported one.
    Exited { code: Option<i32> },
    /// The session could not start or broke off: why, and where the failure came from.
    Failed(ExecFailure),
    /// The front end closed the session.
    Closed,
}

/// Why a session failed: a sentence safe to show, and its [`ExecFailureKind`], so a front end
/// can word it without guessing from the text. It reads as its message (`Deref<Target = str>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecFailure {
    pub kind: ExecFailureKind,
    pub message: String,
}

/// Where a session's failure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecFailureKind {
    /// The API server, or the kubelet and container runtime behind it, refused or failed the
    /// exec, and said why (RBAC, a pod or container not found or not running, a command that
    /// failed to start).
    Server,
    /// Flint could not sign in to the cluster: the exec auth plugin is missing or failed, a
    /// client certificate or key could not be read, the kubeconfig is invalid, or the server
    /// answered 401. A local problem: the error never came from the pod.
    SignIn,
    /// Flint could not reach the API server: the cluster is not connected, or the connection
    /// was refused, reset or timed out before the exec started.
    Unreachable,
    /// The exec connection broke after the command started.
    Connection,
    /// The connection to the cluster could not be upgraded to the WebSocket an exec needs: a
    /// proxy answered the upgrade with a plain response, or stripped its `Upgrade` and
    /// `Connection` headers, so the API server answered 400 "Upgrade request required".
    Upgrade,
    /// Neither shell Flint tries is in the container.
    MissingShell,
    /// The cluster is protected (ADR-0015): Flint opens no shell in it until the user unlocks
    /// or trusts it. Nothing reached the cluster.
    Protected,
    /// The session's context was removed, or now points to another API server than the one the
    /// session opened on, so Flint ended it (the message says which). Checked every
    /// 30 s while the session connects or runs.
    ContextChanged,
    /// Anything else: no cause Flint can name (the full text says what happened).
    Other,
}

impl ExecFailure {
    pub fn new(kind: ExecFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::ops::Deref for ExecFailure {
    type Target = str;
    fn deref(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ExecFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl ExecFailureKind {
    /// Where an error starting the exec came from: from kube's error itself (an auth error
    /// anywhere in its chain, a failed upgrade), then from what core made of it. Only the API
    /// server's own `Status` is `Server`; a bare HTTP code is not the pod's.
    fn of(err: &kube::Error, error: &ClusterError) -> Self {
        use kube::client::UpgradeConnectionError as U;
        match err {
            kube::Error::UpgradeConnection(U::ProtocolSwitch(code)) if code.as_u16() == 401 => {
                return ExecFailureKind::SignIn;
            }
            kube::Error::UpgradeConnection(_) => return ExecFailureKind::Upgrade,
            // The request reached the API server without its upgrade headers: a proxy on the
            // way stripped them (`UpgradeAwareHandler` refuses a plain request this way).
            kube::Error::Api(status)
                if status.code == 400 && status.message == UPGRADE_REQUIRED =>
            {
                return ExecFailureKind::Upgrade;
            }
            _ => {}
        }
        if crate::cluster::find_auth_error(err).is_some() {
            return ExecFailureKind::SignIn;
        }
        match error {
            ClusterError::Api(_) => ExecFailureKind::Server,
            ClusterError::Http { code: 401, .. } => ExecFailureKind::SignIn,
            ClusterError::Http { .. } => ExecFailureKind::Other,
            ClusterError::Timeout { .. } | ClusterError::Transport { .. } => {
                ExecFailureKind::Unreachable
            }
            ClusterError::UnknownContext(_)
            | ClusterError::Config { .. }
            | ClusterError::ExecPluginNotFound { .. }
            | ClusterError::ExecPlugin { .. } => ExecFailureKind::SignIn,
            ClusterError::Kube(_) => ExecFailureKind::Other,
        }
    }
}

impl ExecState {
    /// Whether the remote command is gone (exited, failed, or closed). Input is then ignored,
    /// but the screen can still be scrolled and selected.
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            ExecState::Exited { .. } | ExecState::Failed(_) | ExecState::Closed
        )
    }
}

/// A session's state and screen at one moment. Immutable; shared with front ends.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecSnapshot {
    pub state: ExecState,
    pub terminal: TerminalSnapshot,
}

/// What a front end asks of a running session.
pub(crate) enum Input {
    Write(Vec<u8>),
    Paste(String),
    /// Paste only if the program has bracketed paste on; the reply says whether it did.
    PasteIfBracketed(String, oneshot::Sender<bool>),
    Resize(TerminalSize),
    Scroll(i32),
    ScrollToBottom,
    StartSelection {
        kind: SelectionKind,
        row: u16,
        col: u16,
        side: Side,
    },
    UpdateSelection {
        row: u16,
        col: u16,
        side: Side,
    },
    ClearSelection,
    SelectionText(oneshot::Sender<Option<String>>),
    Close,
}

/// A front end's handle to one exec session, from [`crate::CoreHandle::exec`].
///
/// Every method returns at once and works on any executor. Dropping the session closes it.
pub struct ExecSession {
    request: ExecRequest,
    inputs: async_channel::Sender<Input>,
    rx: watch::Receiver<Arc<ExecSnapshot>>,
}

/// The core's side of a session: what [`run`] reads from and publishes to.
pub(crate) struct SessionChannels {
    inputs: async_channel::Receiver<Input>,
    out: watch::Sender<Arc<ExecSnapshot>>,
    model: TerminalModel,
}

impl ExecSession {
    pub(crate) fn new(request: ExecRequest) -> (Self, SessionChannels) {
        let model = TerminalModel::new(request.size);
        let first = Arc::new(ExecSnapshot {
            state: ExecState::Connecting,
            terminal: model.snapshot(),
        });
        let (out, mut rx) = watch::channel(first);
        rx.mark_changed();
        // Unbounded: input comes at human speed, and sending must never block the UI.
        let (inputs_tx, inputs) = async_channel::unbounded();
        (
            Self {
                request,
                inputs: inputs_tx,
                rx,
            },
            SessionChannels { inputs, out, model },
        )
    }

    /// What this session was opened with.
    pub fn request(&self) -> &ExecRequest {
        &self.request
    }

    fn send(&self, input: Input) {
        // Fails only once the session task has ended; there is nothing left to tell.
        let _ = self.inputs.try_send(input);
    }

    /// Sends keystrokes (already encoded for the terminal) to the command, and scrolls the view
    /// back to the bottom. Ignored once the command has finished.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        self.send(Input::Write(bytes.into()));
    }

    /// Pastes `text`, bracketed when the program asked for it (see
    /// [`TerminalModel::paste_bytes`]).
    pub fn paste(&self, text: impl Into<String>) {
        self.send(Input::Paste(text.into()));
    }

    /// Pastes `text` only if the program has bracketed paste on when the paste reaches it, and
    /// resolves to whether it did. A front end asks before pasting several lines that would
    /// each run; deciding from its last snapshot could miss the program turning the mode off
    /// (bash does while it runs a command). `false` once the session ended too.
    pub fn paste_if_bracketed(
        &self,
        text: impl Into<String>,
    ) -> impl Future<Output = bool> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        self.send(Input::PasteIfBracketed(text.into(), reply));
        async move { rx.await.unwrap_or(false) }
    }

    /// Resizes the terminal and tells the pod's TTY.
    pub fn resize(&self, size: TerminalSize) {
        self.send(Input::Resize(size));
    }

    /// Scrolls the view by `lines`: positive goes up into the history.
    pub fn scroll(&self, lines: i32) {
        self.send(Input::Scroll(lines));
    }

    pub fn scroll_to_bottom(&self) {
        self.send(Input::ScrollToBottom);
    }

    /// Starts a selection at screen cell (`row`, `col`).
    pub fn start_selection(&self, kind: SelectionKind, row: u16, col: u16, side: Side) {
        self.send(Input::StartSelection {
            kind,
            row,
            col,
            side,
        });
    }

    /// Extends the selection to screen cell (`row`, `col`).
    pub fn update_selection(&self, row: u16, col: u16, side: Side) {
        self.send(Input::UpdateSelection { row, col, side });
    }

    pub fn clear_selection(&self) {
        self.send(Input::ClearSelection);
    }

    /// The selected text, for copying. `None` without a selection or once the session ended.
    pub fn selection_text(&self) -> impl Future<Output = Option<String>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        self.send(Input::SelectionText(reply));
        async move { rx.await.ok().flatten() }
    }

    /// Ends the command and the session. Dropping the session does the same.
    pub fn close(&self) {
        self.send(Input::Close);
    }

    /// Waits until a snapshot newer than the last one read with [`ExecSession::latest`] is
    /// published. The first call resolves at once. Returns `false` once the session task has
    /// ended (after [`ExecSession::close`], or when the core stopped).
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    /// The newest snapshot, marking it as seen.
    pub fn latest(&mut self) -> Arc<ExecSnapshot> {
        self.rx.borrow_and_update().clone()
    }

    /// The newest snapshot, without marking it as seen.
    pub fn peek(&self) -> Arc<ExecSnapshot> {
        self.rx.borrow().clone()
    }

    /// A reader of this session's snapshots that a task can wait on while the session itself
    /// stays with whoever sends it input (a view). Its first [`ExecUpdates::changed`] resolves
    /// at once.
    pub fn updates(&self) -> ExecUpdates {
        let mut rx = self.rx.clone();
        rx.mark_changed();
        ExecUpdates { rx }
    }
}

/// Reads an [`ExecSession`]'s snapshots from elsewhere; see [`ExecSession::updates`].
#[derive(Debug)]
pub struct ExecUpdates {
    rx: watch::Receiver<Arc<ExecSnapshot>>,
}

impl ExecUpdates {
    /// Waits until a snapshot newer than the last one read with [`ExecUpdates::latest`] is
    /// published. Returns `false` once the session task has ended. Works on any executor.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    /// The newest snapshot, marking it as seen.
    pub fn latest(&mut self) -> Arc<ExecSnapshot> {
        self.rx.borrow_and_update().clone()
    }
}

impl std::fmt::Debug for ExecSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecSession")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// A started remote command: its TTY's streams, resize channel, and exit status.
pub(crate) struct Process {
    stdin: Pin<Box<dyn AsyncWrite + Send>>,
    stdout: Pin<Box<dyn AsyncRead + Send>>,
    resize: Option<futures::channel::mpsc::Sender<kube::api::TerminalSize>>,
    /// Resolves with the status the API server sends when the command ends, if it sends one.
    status: BoxFuture<'static, Option<Status>>,
    /// Resolves when the connection is gone, with why it broke, if it did. Dropping it, at any
    /// point, closes the connection.
    finish: BoxFuture<'static, Result<(), String>>,
}

/// Why a command could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchError {
    message: String,
    kind: ExecFailureKind,
    /// The container has no such executable: try the next shell.
    missing_command: bool,
}

impl LaunchError {
    fn new(message: String) -> Self {
        Self {
            message,
            kind: ExecFailureKind::Connection,
            missing_command: false,
        }
    }

    /// A kube error from starting `executable`, without exec plugin details (see
    /// [`ClusterError::kube`]). Only an error status from the API server can say the
    /// executable is missing; a local error such as a missing auth plugin or certificate file
    /// ("No such file or directory") must not make the next shell be tried.
    fn kube(err: kube::Error, executable: &str) -> Self {
        let from_server = matches!(err, kube::Error::Api(_));
        // Classified from kube's error as well as core's, which keeps less of it.
        let error = ClusterError::kube_ref(&err);
        let kind = ExecFailureKind::of(&err, &error);
        let message = error.to_string();
        let missing_command = from_server && is_missing_command(&message, executable);
        Self {
            kind: if missing_command {
                ExecFailureKind::MissingShell
            } else {
                kind
            },
            missing_command,
            message,
        }
    }
}

/// Starts commands in one container. A seam so the session logic is testable without a cluster.
pub(crate) trait Launcher: Send + Sync + 'static {
    fn launch(&self, command: Vec<String>) -> BoxFuture<'static, Result<Process, LaunchError>>;
}

/// Runs commands with `Api::<Pod>::exec`.
struct KubeLauncher {
    api: Api<Pod>,
    pod: String,
    container: Option<String>,
}

impl Launcher for KubeLauncher {
    fn launch(&self, command: Vec<String>) -> BoxFuture<'static, Result<Process, LaunchError>> {
        let api = self.api.clone();
        let pod = self.pod.clone();
        let mut params = AttachParams::interactive_tty()
            .max_stdin_buf_size(IO_BUFFER)
            .max_stdout_buf_size(IO_BUFFER);
        if let Some(container) = &self.container {
            params = params.container(container.clone());
        }
        async move {
            let executable = command.first().cloned().unwrap_or_default();
            let mut attached = api
                .exec(&pod, command, &params)
                .await
                .map_err(|e| LaunchError::kube(e, &executable))?;
            let missing = || LaunchError::new("the exec connection has no terminal".to_owned());
            let stdin = attached.stdin().ok_or_else(missing)?;
            let stdout = attached.stdout().ok_or_else(missing)?;
            // Resolves once kube's task has the status or has ended without one (its sender
            // lives in that task), so it says the connection ended without joining the task.
            let status = attached.take_status().ok_or_else(missing)?.shared();
            let ended = status.clone();
            let resize = attached.terminal_size();
            // `join` detaches kube's task from `attached`, whose drop is what aborts it (and
            // closes the WebSocket): join only once the task is ending, so dropping `finish`
            // before then (the session was closed) still closes the connection.
            let finish = async move {
                ended.await;
                attached
                    .join()
                    .await
                    .map_err(|e| crate::cluster::error_chain(&e))
            };
            Ok(Process {
                stdin: Box::pin(stdin),
                stdout: Box::pin(stdout),
                resize,
                status: status.boxed(),
                finish: finish.boxed(),
            })
        }
        .boxed()
    }
}

/// Says why a session must end because its context changed ([`crate::recheck::moved`]), or
/// `None` while the context still points at the API server the session opened on.
pub(crate) type ContextCheck = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Runs a session's [`ContextCheck`] every so often (see [`crate::recheck`]).
pub(crate) struct ContextWatch {
    check: ContextCheck,
    recheck: Recheck<Option<String>>,
}

impl ContextWatch {
    pub fn new(check: ContextCheck, every: Duration) -> Self {
        Self {
            check,
            recheck: Recheck::new(every),
        }
    }

    /// Resolves with why the session must end, once a check says so. Cancel-safe: the timer
    /// and a check in flight carry over to the next call.
    async fn changed(&mut self) -> String {
        let Self { check, recheck } = self;
        std::future::poll_fn(|cx| {
            loop {
                if let Some(reason) = std::task::ready!(recheck.poll_check(cx, || check())) {
                    return Poll::Ready(reason);
                }
            }
        })
        .await
    }
}

/// Runs a session on the core runtime: with a client, against the cluster, while `watch` finds
/// its context unchanged; without one, the session fails at once with the given reason (the
/// cluster is not connected, say).
pub(crate) async fn start(
    client: Result<(Client, ContextWatch), WriteRefusal>,
    request: ExecRequest,
    channels: SessionChannels,
) {
    match client {
        Ok((client, watch)) => {
            let launcher = KubeLauncher {
                api: Api::namespaced(client, &request.namespace),
                pod: request.pod.clone(),
                container: request.container.clone(),
            };
            run(launcher, request, channels, watch).await;
        }
        Err(refusal) => {
            let kind = match refusal {
                WriteRefusal::Protected { .. } => ExecFailureKind::Protected,
                WriteRefusal::NotConnected { .. } | WriteRefusal::Stopped => {
                    ExecFailureKind::Unreachable
                }
            };
            let mut session = Session::new(channels);
            session.set_state(ExecState::Failed(ExecFailure::new(
                kind,
                refusal.to_string(),
            )));
            session.linger().await;
        }
    }
}

/// The session task: tries each candidate command, drives the one that starts, then keeps the
/// screen available until the front end closes the session. Ends the command, closing its
/// connection, when `watch` finds the context changed.
pub(crate) async fn run(
    launcher: impl Launcher,
    request: ExecRequest,
    channels: SessionChannels,
    mut watch: ContextWatch,
) {
    let mut session = Session::new(channels);
    let candidates = request.candidates();
    // Typed before the command started; sent once it has.
    let mut pending = Vec::new();
    for (i, command) in candidates.iter().enumerate() {
        let last = i + 1 == candidates.len();
        session.set_state(ExecState::Connecting);
        let mut launch = launcher.launch(command.clone());
        let launched = loop {
            tokio::select! {
                result = &mut launch => break Some(result),
                input = session.next_input() => {
                    if session.handle_offline(input, Some(&mut pending)) == Flow::Close {
                        session.set_state(ExecState::Closed);
                        return;
                    }
                }
                reason = watch.changed() => {
                    session.set_state(ExecState::Failed(context_changed(reason)));
                    break None;
                }
            }
        };
        // The context changed: the connection being opened is dropped.
        let Some(launched) = launched else { break };
        let process = match launched {
            Ok(process) => process,
            Err(e) if e.missing_command && !last => {
                tracing::debug!(?command, "command not in the container; trying the next");
                continue;
            }
            Err(e) => {
                session.set_state(ExecState::Failed(ExecFailure::new(e.kind, e.message)));
                break;
            }
        };
        session.set_state(ExecState::Running {
            command: command.clone(),
        });
        let executable = command.first().map_or("", String::as_str);
        match session
            .drive(process, executable, &mut pending, !last, &mut watch)
            .await
        {
            Outcome::MissingCommand(_) if !last => {
                tracing::debug!(?command, "command not in the container; trying the next");
                // Some runtimes print the error on the TTY: start the next shell on a clean
                // screen.
                session.reset_screen();
                continue;
            }
            Outcome::MissingCommand(message) => session.set_state(ExecState::Failed(
                ExecFailure::new(ExecFailureKind::MissingShell, message),
            )),
            Outcome::Done(ExecState::Closed) => {
                session.set_state(ExecState::Closed);
                return;
            }
            Outcome::Done(state) => session.set_state(state),
        }
        break;
    }
    session.linger().await;
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Close,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The command does not exist in the container; nothing ran.
    MissingCommand(String),
    Done(ExecState),
}

/// Bytes and resizes for the remote TTY, written by their own task so a slow upload never
/// stops output from being read (kube's message loop does both on one task).
enum ToRemote {
    Bytes(Vec<u8>),
    Resize(TerminalSize),
}

struct Session {
    model: TerminalModel,
    state: ExecState,
    inputs: async_channel::Receiver<Input>,
    out: watch::Sender<Arc<ExecSnapshot>>,
    last_publish: Option<Instant>,
    /// Output arrived since the last snapshot.
    dirty: bool,
}

impl Session {
    fn new(channels: SessionChannels) -> Self {
        Self {
            model: channels.model,
            state: ExecState::Connecting,
            inputs: channels.inputs,
            out: channels.out,
            last_publish: None,
            dirty: false,
        }
    }

    fn publish(&mut self) {
        self.dirty = false;
        self.last_publish = Some(Instant::now());
        self.out.send_replace(Arc::new(ExecSnapshot {
            state: self.state.clone(),
            terminal: self.model.snapshot(),
        }));
    }

    fn set_state(&mut self, state: ExecState) {
        if state != self.state {
            self.state = state;
            self.publish();
        }
    }

    /// Output, scrolling or a selection changed the screen: publish now if the last snapshot is
    /// a frame old, else when it is (see `deadline`).
    fn screen_changed(&mut self) {
        match self.last_publish {
            Some(at) if at.elapsed() < FRAME => self.dirty = true,
            _ => self.publish(),
        }
    }

    /// When the held-back output must be published.
    fn deadline(&self) -> Option<Instant> {
        self.dirty
            .then(|| self.last_publish.map_or_else(Instant::now, |at| at + FRAME))
    }

    /// The next input, publishing held-back screen changes when their frame is due meanwhile.
    async fn next_input(&mut self) -> Result<Input, async_channel::RecvError> {
        loop {
            let Some(deadline) = self.deadline() else {
                return self.inputs.recv().await;
            };
            tokio::select! {
                input = self.inputs.recv() => return input,
                () = tokio::time::sleep_until(deadline) => self.publish(),
            }
        }
    }

    /// Handles input that does not need the remote command. `pending` collects keystrokes while
    /// the command starts; `None` drops them (the command has finished).
    fn handle_offline(
        &mut self,
        input: Result<Input, async_channel::RecvError>,
        pending: Option<&mut Vec<u8>>,
    ) -> Flow {
        let Ok(input) = input else {
            // The session handle is gone.
            return Flow::Close;
        };
        match input {
            Input::Write(bytes) => {
                if let Some(pending) = pending {
                    pending.extend(bytes);
                }
            }
            Input::Paste(text) => {
                if let Some(pending) = pending {
                    pending.extend(self.model.paste_bytes(&text));
                }
            }
            // No program runs yet (or any more), so none asked for bracketed paste.
            Input::PasteIfBracketed(_, reply) => {
                let _ = reply.send(false);
            }
            Input::Resize(size) => {
                self.model.resize(size);
                self.screen_changed();
            }
            Input::Scroll(lines) => {
                self.model.scroll(lines);
                self.screen_changed();
            }
            Input::ScrollToBottom => {
                self.model.scroll_to_bottom();
                self.screen_changed();
            }
            Input::StartSelection {
                kind,
                row,
                col,
                side,
            } => {
                self.model.start_selection(kind, row, col, side);
                self.screen_changed();
            }
            Input::UpdateSelection { row, col, side } => {
                self.model.update_selection(row, col, side);
                self.screen_changed();
            }
            Input::ClearSelection => {
                self.model.clear_selection();
                self.screen_changed();
            }
            Input::SelectionText(reply) => {
                let _ = reply.send(self.model.selection_text());
            }
            Input::Close => return Flow::Close,
        }
        Flow::Continue
    }

    /// Handles input while the command runs. `unconfirmed` keeps a copy of keystrokes until the
    /// command shows it exists, for the next shell if it does not.
    fn handle_live(
        &mut self,
        input: Result<Input, async_channel::RecvError>,
        remote: &mpsc::UnboundedSender<ToRemote>,
        unconfirmed: Option<&mut Vec<u8>>,
    ) -> Flow {
        let bytes = match input {
            Ok(Input::Write(bytes)) => bytes,
            Ok(Input::Paste(text)) => self.model.paste_bytes(&text),
            Ok(Input::PasteIfBracketed(text, reply)) => {
                let bracketed = self.model.bracketed_paste();
                let _ = reply.send(bracketed);
                if !bracketed {
                    return Flow::Continue;
                }
                self.model.paste_bytes(&text)
            }
            other => return self.handle_live_control(other, remote),
        };
        self.follow_output();
        if let Some(unconfirmed) = unconfirmed {
            unconfirmed.extend_from_slice(&bytes);
        }
        let _ = remote.send(ToRemote::Bytes(bytes));
        Flow::Continue
    }

    fn handle_live_control(
        &mut self,
        input: Result<Input, async_channel::RecvError>,
        remote: &mpsc::UnboundedSender<ToRemote>,
    ) -> Flow {
        match input {
            Ok(Input::Resize(size)) => {
                // The TTY gets the size the screen really has.
                let size = size.normalized();
                if size != self.model.size() {
                    let _ = remote.send(ToRemote::Resize(size));
                }
                self.handle_offline(Ok(Input::Resize(size)), None)
            }
            other => self.handle_offline(other, None),
        }
    }

    /// Typing scrolls the view back to the bottom, as in any terminal.
    fn follow_output(&mut self) {
        if self.model.display_offset() != 0 {
            self.model.scroll_to_bottom();
            self.publish();
        }
    }

    /// Forgets the screen, keeping its size.
    fn reset_screen(&mut self) {
        self.model = TerminalModel::new(self.model.size());
        self.publish();
    }

    /// Streams one started command (running `executable`) until it ends or the session is
    /// closed. `pending` holds keystrokes typed before it started, which are sent first. When
    /// another shell would be tried if this one turns out not to exist (`fallback`), `pending`
    /// keeps them (with what is typed next) until the output shows the command started (it
    /// cannot be a runtime's launch error any more); otherwise it is emptied at once. When
    /// `watch` finds the context changed, the command ends at once: returning drops the
    /// connection, as closing the session does.
    async fn drive(
        &mut self,
        process: Process,
        executable: &str,
        pending: &mut Vec<u8>,
        fallback: bool,
        watch: &mut ContextWatch,
    ) -> Outcome {
        let Process {
            stdin,
            mut stdout,
            resize,
            status,
            mut finish,
        } = process;
        let (remote, rx) = mpsc::unbounded_channel();
        let writer = AbortOnDrop(tokio::spawn(write_remote(stdin, resize, rx)));
        // The API server starts the TTY at a default size; set ours right away.
        let _ = remote.send(ToRemote::Resize(self.model.size()));
        if !pending.is_empty() {
            let _ = remote.send(ToRemote::Bytes(pending.clone()));
        }
        let mut pending = if fallback {
            Some(pending)
        } else {
            pending.clear();
            None
        };
        let mut buf = vec![0; IO_BUFFER];
        let mut saw_output = false;
        // The command has shown it started: its output cannot be a launch error any more.
        let mut started = false;
        // The first output, for runtimes that print a missing executable on the TTY.
        let mut early = Vec::new();
        // How the connection ended, once it has, and until when the output left in the pipe
        // is still read. kube's message loop can stop on an error without ending the output
        // stream, so the stream alone cannot tell that the connection is gone.
        let mut connection: Option<(Result<(), String>, Instant)> = None;
        loop {
            let deadline = self.deadline();
            let sync = self.model.sync_deadline().map(Instant::from_std);
            let drained = connection.as_ref().map(|(_, until)| *until);
            tokio::select! {
                read = stdout.read(&mut buf) => match read {
                    // End of output; a read error ends it too, and `finish` says why.
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        saw_output = true;
                        if early.len() <= EARLY_OUTPUT {
                            early.extend_from_slice(&buf[..n]);
                        }
                        if !started && !could_be_launch_error(&early) {
                            started = true;
                            if let Some(pending) = pending.as_deref_mut() {
                                pending.clear();
                            }
                        }
                        let reply = self.model.advance(&buf[..n]);
                        if !reply.is_empty() {
                            let _ = remote.send(ToRemote::Bytes(reply));
                        }
                        self.screen_changed();
                    }
                },
                input = self.inputs.recv() => {
                    let unconfirmed = pending.as_deref_mut().filter(|_| !started);
                    if self.handle_live(input, &remote, unconfirmed) == Flow::Close {
                        return Outcome::Done(ExecState::Closed);
                    }
                }
                result = &mut finish, if connection.is_none() => {
                    connection = Some((result, Instant::now() + DRAIN_WAIT));
                }
                // Never on another server than the one the session opened on.
                reason = watch.changed(), if connection.is_none() => {
                    self.model.end_sync();
                    return Outcome::Done(ExecState::Failed(context_changed(reason)));
                }
                // The connection is gone and the output stream never ended.
                () = tokio::time::sleep_until(drained.unwrap_or_else(Instant::now)),
                    if drained.is_some() => break,
                () = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)),
                    if deadline.is_some() => self.publish(),
                // A synchronized update the program never ended (it was interrupted, say).
                () = tokio::time::sleep_until(sync.unwrap_or_else(Instant::now)),
                    if sync.is_some() => {
                    let reply = self.model.end_sync();
                    if !reply.is_empty() {
                        let _ = remote.send(ToRemote::Bytes(reply));
                    }
                    self.screen_changed();
                }
            }
        }
        drop(remote);
        drop(writer);
        // Output held back for a synchronized update belongs on the final screen.
        self.model.end_sync();
        self.publish();
        let ending = async {
            let status = tokio::time::timeout(STATUS_WAIT, status)
                .await
                .ok()
                .flatten();
            let result = match (&status, connection) {
                (Some(_), _) => Ok(()),
                (None, Some((result, _))) => result,
                (None, None) => tokio::time::timeout(STATUS_WAIT, finish)
                    .await
                    .unwrap_or(Ok(())),
            };
            (status, result)
        };
        tokio::pin!(ending);
        let (status, result) = loop {
            tokio::select! {
                ended = &mut ending => break ended,
                input = self.next_input() => {
                    if self.handle_offline(input, pending.as_deref_mut()) == Flow::Close {
                        return Outcome::Done(ExecState::Closed);
                    }
                }
            }
        };
        let early = (early.len() <= EARLY_OUTPUT).then_some(early.as_slice());
        outcome(status.as_ref(), result, executable, saw_output, early)
    }

    /// After the command is gone: keeps the screen scrollable and selectable until the front end
    /// closes the session.
    async fn linger(&mut self) {
        loop {
            let input = self.next_input().await;
            if self.handle_offline(input, None) == Flow::Close {
                if !self.state.is_finished() {
                    self.set_state(ExecState::Closed);
                }
                return;
            }
        }
    }
}

fn context_changed(reason: String) -> ExecFailure {
    ExecFailure::new(ExecFailureKind::ContextChanged, reason)
}

/// A spawned task that is aborted when this is dropped, so the work stops with whoever waits
/// for it.
pub(crate) struct AbortOnDrop<T>(pub tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn write_remote(
    mut stdin: Pin<Box<dyn AsyncWrite + Send>>,
    mut resize: Option<futures::channel::mpsc::Sender<kube::api::TerminalSize>>,
    mut rx: mpsc::UnboundedReceiver<ToRemote>,
) {
    while let Some(message) = rx.recv().await {
        match message {
            ToRemote::Bytes(bytes) => {
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    return;
                }
            }
            ToRemote::Resize(size) => {
                if let Some(tx) = resize.as_mut() {
                    let size = kube::api::TerminalSize {
                        width: size.cols,
                        height: size.rows,
                    };
                    if tx.send(size).await.is_err() {
                        resize = None;
                    }
                }
            }
        }
    }
    let _ = stdin.shutdown().await;
}

/// How a command that started `executable` ended, from the exec status (if the server sent
/// one), the connection's result, whether the command printed anything, and all it printed if
/// that was little (`early`).
fn outcome(
    status: Option<&Status>,
    connection: Result<(), String>,
    executable: &str,
    saw_output: bool,
    early: Option<&[u8]>,
) -> Outcome {
    let Some(status) = status else {
        return Outcome::Done(match connection {
            Ok(()) => ExecState::Exited { code: None },
            Err(e) => ExecState::Failed(ExecFailure::new(
                ExecFailureKind::Connection,
                format!("the exec connection broke: {e}"),
            )),
        });
    };
    if status.status.as_deref() == Some("Success") {
        return Outcome::Done(ExecState::Exited { code: Some(0) });
    }
    if status.reason.as_deref() == Some("NonZeroExitCode") {
        let code = exit_code(status);
        // Docker prints "OCI runtime exec failed: ... executable file not found" on the TTY
        // and exits with 126 or 127 instead of reporting it on the status channel. A command
        // that ran and printed "No such file or directory" itself keeps its exit code.
        if matches!(code, Some(126 | 127))
            && let Some(early) = early
            && could_be_launch_error(early)
        {
            let printed = String::from_utf8_lossy(early);
            if is_missing_command(&printed, executable) {
                return Outcome::MissingCommand(printed.trim().to_owned());
            }
        }
        return Outcome::Done(ExecState::Exited { code });
    }
    let message = status_message(status);
    if !saw_output && is_missing_command(&message, executable) {
        Outcome::MissingCommand(message)
    } else {
        Outcome::Done(ExecState::Failed(ExecFailure::new(
            ExecFailureKind::Server,
            message,
        )))
    }
}

/// The exit code in a `NonZeroExitCode` status (a cause with reason `ExitCode`).
fn exit_code(status: &Status) -> Option<i32> {
    status
        .details
        .as_ref()?
        .causes
        .as_ref()?
        .iter()
        .find(|c| c.reason.as_deref() == Some("ExitCode"))?
        .message
        .as_deref()?
        .parse()
        .ok()
}

/// `"<reason>: <message>"`, as [`ClusterError::kube`] formats API errors.
fn status_message(status: &Status) -> String {
    let reason = status.reason.as_deref().filter(|r| !r.is_empty());
    let message = status.message.as_deref().filter(|m| !m.is_empty());
    match (reason, message) {
        (Some(r), Some(m)) => format!("{r}: {m}"),
        (Some(r), None) => r.to_owned(),
        (None, Some(m)) => m.to_owned(),
        (None, None) => format!(
            "the command failed (HTTP {})",
            status
                .code
                .map_or_else(|| "?".to_owned(), |c| c.to_string())
        ),
    }
}

/// Whether an exec error says `executable` does not exist in the container: it says so in
/// the container runtime's words (runc and crun through containerd or CRI-O, and Docker) and
/// names that executable, as `exec: "bash": executable file not found in $PATH` or
/// `stat /bin/sh: no such file or directory`.
fn is_missing_command(message: &str, executable: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let missing = lower.contains("executable file not found")
        || lower.contains("no such file or directory")
        || lower.contains("not found in $path");
    let named = !executable.is_empty()
        && [
            format!("\"{executable}\""),
            format!("`{executable}`"),
            format!("stat {executable}:"),
        ]
        .iter()
        .any(|form| message.contains(form.as_str()));
    missing && named
}

/// How Docker's launch errors begin when it prints them on the TTY.
const LAUNCH_ERROR_PREFIX: &[u8] = b"OCI runtime exec failed";

/// Whether `early`, everything the command printed so far, is (or could still turn into) a
/// launch error printed on the TTY.
fn could_be_launch_error(early: &[u8]) -> bool {
    let start = early
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(early.len());
    let text = &early[start..];
    if text.len() < LAUNCH_ERROR_PREFIX.len() {
        LAUNCH_ERROR_PREFIX.starts_with(text)
    } else {
        text.starts_with(LAUNCH_ERROR_PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{StatusCause, StatusDetails};
    use tokio::io::DuplexStream;

    use super::*;

    /// The pod's side of a fake process.
    struct Remote {
        /// What the session writes to the command's stdin.
        stdin: DuplexStream,
        /// Where the command's output is written.
        stdout: Option<Box<dyn AsyncWrite + Send + Unpin>>,
        resizes: futures::channel::mpsc::Receiver<kube::api::TerminalSize>,
        status: Option<oneshot::Sender<Status>>,
        /// Ends the connection with a result; dropping it ends it cleanly.
        finish: Option<oneshot::Sender<Result<(), String>>>,
    }

    impl Remote {
        async fn print(&mut self, bytes: &[u8]) {
            self.stdout
                .as_mut()
                .unwrap()
                .write_all(bytes)
                .await
                .unwrap();
        }

        /// Ends the command with `status`, as the API server does.
        fn exit(&mut self, status: Status) {
            let _ = self.status.take().unwrap().send(status);
            self.stdout = None;
            self.finish = None;
        }

        /// Breaks the connection as kube 5.1's message loop does when the WebSocket fails: it
        /// returns the error without sending a status or ending the output stream.
        fn break_connection(&mut self, error: &str) {
            self.status = None;
            self.stdout = None;
            let _ = self.finish.take().unwrap().send(Err(error.to_owned()));
        }

        async fn read_stdin(&mut self, len: usize) -> Vec<u8> {
            let mut buf = vec![0; len];
            tokio::time::timeout(Duration::from_secs(5), self.stdin.read_exact(&mut buf))
                .await
                .expect("stdin in time")
                .unwrap();
            buf
        }

        async fn next_resize(&mut self) -> (u16, u16) {
            use futures::StreamExt;
            let size = tokio::time::timeout(Duration::from_secs(5), self.resizes.next())
                .await
                .expect("a resize in time")
                .expect("resize channel open");
            (size.width, size.height)
        }
    }

    /// A fake process whose output stream ends when the remote drops it, as a duplex does.
    fn process() -> (Process, Remote) {
        let (remote_stdout, stdout) = tokio::io::duplex(IO_BUFFER);
        process_with(Box::pin(stdout), Box::new(remote_stdout))
    }

    /// A fake process with kube's output pipe: a simplex, whose reader never reaches the end
    /// when the writer is dropped without being shut down.
    fn kube_like_process() -> (Process, Remote) {
        let (stdout, remote_stdout) = tokio::io::simplex(IO_BUFFER);
        process_with(Box::pin(stdout), Box::new(remote_stdout))
    }

    fn process_with(
        stdout: Pin<Box<dyn AsyncRead + Send>>,
        remote_stdout: Box<dyn AsyncWrite + Send + Unpin>,
    ) -> (Process, Remote) {
        let (stdin, remote_stdin) = tokio::io::duplex(IO_BUFFER);
        let (resize_tx, resizes) = futures::channel::mpsc::channel(10);
        let (status_tx, status_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let process = Process {
            stdin: Box::pin(stdin),
            stdout,
            resize: Some(resize_tx),
            status: status_rx.map(Result::ok).boxed(),
            finish: finish_rx.map(|r| r.unwrap_or(Ok(()))).boxed(),
        };
        let remote = Remote {
            stdin: remote_stdin,
            stdout: Some(remote_stdout),
            resizes,
            status: Some(status_tx),
            finish: Some(finish_tx),
        };
        (process, remote)
    }

    #[derive(Clone, Default)]
    struct FakeLauncher {
        results: Arc<Mutex<VecDeque<Result<Process, LaunchError>>>>,
        launched: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FakeLauncher {
        fn push(&self, result: Result<Process, LaunchError>) {
            self.results.lock().unwrap().push_back(result);
        }

        fn launched(&self) -> Vec<Vec<String>> {
            self.launched.lock().unwrap().clone()
        }
    }

    impl Launcher for FakeLauncher {
        fn launch(&self, command: Vec<String>) -> BoxFuture<'static, Result<Process, LaunchError>> {
            self.launched.lock().unwrap().push(command);
            let result = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .expect("a scripted launch");
            async move { result }.boxed()
        }
    }

    fn open(launcher: &FakeLauncher, request: ExecRequest) -> ExecSession {
        let never: ContextCheck = Arc::new(|| async { None }.boxed());
        open_watched(launcher, request, never)
    }

    /// How often [`open_watched`] sessions check their context.
    const CHECK_EVERY: Duration = Duration::from_millis(20);

    fn open_watched(
        launcher: &(impl Launcher + Clone),
        request: ExecRequest,
        check: ContextCheck,
    ) -> ExecSession {
        let (session, channels) = ExecSession::new(request.clone());
        let watch = ContextWatch::new(check, CHECK_EVERY);
        tokio::spawn(run(launcher.clone(), request, channels, watch));
        session
    }

    /// A context the test re-points or removes: its server now (`None`: removed), and how
    /// many checks a session pinned to `https://staging` made, as [`ContextCheck`] does
    /// through the cluster manager.
    #[derive(Clone)]
    struct FakeContext {
        server: Arc<Mutex<Option<&'static str>>>,
        checks: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeContext {
        fn new() -> Self {
            Self {
                server: Arc::new(Mutex::new(Some("https://staging"))),
                checks: Arc::default(),
            }
        }

        fn point_to(&self, server: Option<&'static str>) {
            *self.server.lock().unwrap() = server;
        }

        fn check(&self) -> ContextCheck {
            let context = self.clone();
            Arc::new(move || {
                context
                    .checks
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let current = *context.server.lock().unwrap();
                let reason = crate::recheck::moved(
                    crate::recheck::Pinned::Shell,
                    "main",
                    Some("https://staging"),
                    current.map(Some),
                );
                async move { reason }.boxed()
            })
        }

        async fn checked(&self, times: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.checks.load(std::sync::atomic::Ordering::SeqCst) < times {
                    tokio::time::sleep(CHECK_EVERY).await;
                }
            })
            .await
            .expect("the session checks its context");
        }
    }

    /// Waits until the connection is dropped: kube's task, and with it the WebSocket, is gone.
    async fn connection_closed(remote: &mut Remote) {
        tokio::time::timeout(
            Duration::from_secs(5),
            remote.finish.as_mut().expect("still connected").closed(),
        )
        .await
        .expect("the connection was closed");
    }

    fn request() -> ExecRequest {
        ExecRequest::shell("kind", "ns", "pod").size(TerminalSize::new(40, 5))
    }

    async fn wait_for(
        session: &mut ExecSession,
        what: &str,
        pred: impl Fn(&ExecSnapshot) -> bool,
    ) -> Arc<ExecSnapshot> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snap = session.latest();
                if pred(&snap) {
                    return snap;
                }
                assert!(session.changed().await, "session ended waiting for {what}");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}: {:?}", session.peek().state))
    }

    fn status(status: &str, reason: &str, message: &str) -> Status {
        Status {
            status: Some(status.to_owned()),
            reason: Some(reason.to_owned()),
            message: Some(message.to_owned()),
            ..Default::default()
        }
    }

    fn exit_status(code: &str) -> Status {
        Status {
            details: Some(StatusDetails {
                causes: Some(vec![StatusCause {
                    reason: Some("ExitCode".to_owned()),
                    message: Some(code.to_owned()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..status(
                "Failure",
                "NonZeroExitCode",
                "command terminated with non-zero exit code",
            )
        }
    }

    /// An error status from the API server, as `Api::exec` returns when the upgrade fails.
    fn api_error(message: &str) -> kube::Error {
        kube::Error::Api(Box::new(kube::core::Status {
            code: 500,
            reason: "InternalError".into(),
            message: message.into(),
            ..Default::default()
        }))
    }

    const NOT_FOUND: &str = "Internal error occurred: error executing command in container: \
        failed to exec in container: OCI runtime exec failed: exec failed: unable to start \
        container process: exec: \"bash\": executable file not found in $PATH";

    // covers UP-41
    #[tokio::test]
    async fn output_reaches_the_screen_and_input_reaches_the_command() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        wait_for(&mut session, "running", |s| {
            s.state
                == ExecState::Running {
                    command: vec!["bash".into()],
                }
        })
        .await;
        assert_eq!(remote.next_resize().await, (40, 5), "the size is set first");
        remote.print(b"root@pod:/# ").await;
        let snap = wait_for(&mut session, "the prompt", |s| {
            s.terminal.row_text(0) == "root@pod:/#"
        })
        .await;
        assert_eq!(snap.terminal.cursor.unwrap().col, 12);
        session.write(b"ls\r".to_vec());
        assert_eq!(remote.read_stdin(3).await, b"ls\r");
        session.paste("a\nb");
        assert_eq!(remote.read_stdin(3).await, b"a\rb");
        // A paste that waits for bracketed paste mode is refused while it is off, and sent
        // bracketed once the program turns it on.
        assert!(!session.paste_if_bracketed("c\nd").await);
        remote.print(b"\x1b[?2004h").await;
        wait_for(&mut session, "bracketed paste", |s| {
            s.terminal
                .mode
                .contains(crate::terminal::TermMode::BRACKETED_PASTE)
        })
        .await;
        assert!(session.paste_if_bracketed("c\nd").await);
        assert_eq!(remote.read_stdin(9).await, b"\x1b[200~c\nd");
        assert_eq!(launcher.launched(), vec![vec!["bash".to_owned()]]);
    }

    // covers UP-41
    #[tokio::test]
    async fn terminal_queries_are_answered_to_the_command() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let _session = open(&launcher, request());
        remote.next_resize().await;
        remote.print(b"ab\x1b[6n").await;
        assert_eq!(remote.read_stdin(6).await, b"\x1b[1;3R");
    }

    // covers UP-41
    #[tokio::test]
    async fn resizes_reach_the_tty_and_the_screen() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        assert_eq!(remote.next_resize().await, (40, 5));
        session.resize(TerminalSize::new(100, 30));
        assert_eq!(remote.next_resize().await, (100, 30));
        wait_for(&mut session, "the new size", |s| {
            s.terminal.size == TerminalSize::new(100, 30)
        })
        .await;
    }

    // covers UP-41
    #[tokio::test(start_paused = true)]
    async fn output_bursts_are_published_at_most_once_per_frame() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        // After a quiet spell, output is published at once (typing echoes without delay).
        tokio::time::sleep(FRAME * 2).await;
        session.latest();
        remote.print(b"a").await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(session.rx.has_changed().unwrap());
        assert_eq!(session.latest().terminal.row_text(0), "a");
        // More output within the frame is held back, then published together.
        remote.print(b"b").await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        remote.print(b"c").await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(!session.rx.has_changed().unwrap(), "held back");
        tokio::time::sleep(FRAME).await;
        assert!(session.rx.has_changed().unwrap());
        assert_eq!(session.latest().terminal.row_text(0), "abc");
    }

    // covers UP-41
    #[tokio::test]
    async fn a_container_without_bash_falls_back_to_sh() {
        let launcher = FakeLauncher::default();
        let (bash, mut bash_remote) = process();
        let (sh, mut sh_remote) = process();
        launcher.push(Ok(bash));
        launcher.push(Ok(sh));
        let mut session = open(&launcher, request());
        // Typed while bash was being tried: it goes to the shell that starts.
        session.write(b"x".to_vec());
        bash_remote.next_resize().await;
        bash_remote.exit(status("Failure", "InternalError", NOT_FOUND));
        wait_for(&mut session, "sh", |s| {
            s.state
                == ExecState::Running {
                    command: vec!["/bin/sh".into()],
                }
        })
        .await;
        assert_eq!(sh_remote.next_resize().await, (40, 5));
        assert_eq!(sh_remote.read_stdin(1).await, b"x");
        assert_eq!(
            launcher.launched(),
            vec![vec!["bash".to_owned()], vec!["/bin/sh".to_owned()]]
        );
    }

    // covers UP-41
    #[tokio::test]
    async fn a_missing_command_reported_at_connect_also_falls_back() {
        let launcher = FakeLauncher::default();
        let (sh, _sh_remote) = process();
        launcher.push(Err(LaunchError::kube(api_error(NOT_FOUND), "bash")));
        launcher.push(Ok(sh));
        let mut session = open(&launcher, request());
        wait_for(
            &mut session,
            "sh",
            |s| matches!(&s.state, ExecState::Running { command } if command == &["/bin/sh"]),
        )
        .await;
    }

    // covers UP-41
    #[tokio::test]
    async fn an_explicit_command_does_not_fall_back() {
        let launcher = FakeLauncher::default();
        let (zsh, mut remote) = process();
        launcher.push(Ok(zsh));
        let mut session = open(&launcher, request().command(vec!["zsh".into()]));
        remote.next_resize().await;
        remote.exit(status("Failure", "InternalError", NOT_FOUND));
        let snap = wait_for(&mut session, "failure", |s| s.state.is_finished()).await;
        let ExecState::Failed(message) = &snap.state else {
            panic!("{:?}", snap.state);
        };
        assert!(message.contains("executable file not found"), "{message}");
        assert_eq!(launcher.launched(), vec![vec!["zsh".to_owned()]]);
    }

    // covers UP-41
    #[tokio::test]
    async fn exit_codes_are_reported_and_the_screen_stays_readable() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        remote.print(b"bye").await;
        remote.exit(exit_status("3"));
        let snap = wait_for(&mut session, "exit", |s| s.state.is_finished()).await;
        assert_eq!(snap.state, ExecState::Exited { code: Some(3) });
        assert_eq!(snap.terminal.row_text(0), "bye");
        // Still selectable after the exit.
        session.start_selection(SelectionKind::Lines, 0, 0, Side::Left);
        assert_eq!(session.selection_text().await.as_deref(), Some("bye\n"));
        session.close();
        let snap = wait_for(&mut session, "unchanged", |_| true).await;
        assert_eq!(snap.state, ExecState::Exited { code: Some(3) });
    }

    // covers UP-41
    #[tokio::test]
    async fn closing_the_session_closes_the_command() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        session.close();
        wait_for(&mut session, "closed", |s| s.state == ExecState::Closed).await;
        // The writer is gone: stdin reaches end of file.
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), remote.stdin.read_to_end(&mut rest))
            .await
            .expect("stdin closed")
            .unwrap();
        assert!(rest.is_empty());
        assert!(!session.changed().await, "the session task ended");
    }

    // covers UP-06, UP-41: `aws eks update-kubeconfig --alias main` re-points `main` while a
    // shell on it runs. The shell ends with the reason and its connection is closed, so it
    // never goes on running on a server `main` no longer means.
    #[tokio::test]
    async fn a_re_pointed_context_ends_the_shell_and_closes_its_connection() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let context = FakeContext::new();
        let mut session = open_watched(&launcher, request(), context.check());
        remote.next_resize().await;
        remote.print(b"staging$ ").await;
        wait_for(&mut session, "output", |s| {
            s.terminal.row_text(0) == "staging$"
        })
        .await;
        // Checks that find the context unchanged leave the shell running.
        context.checked(3).await;
        assert!(matches!(session.peek().state, ExecState::Running { .. }));

        context.point_to(Some("https://production"));
        let snap = wait_for(&mut session, "the end", |s| s.state.is_finished()).await;
        assert_eq!(
            snap.state,
            ExecState::Failed(ExecFailure::new(
                ExecFailureKind::ContextChanged,
                "\"main\" now points to a different API server; open the shell again to run \
                 one there"
            ))
        );
        connection_closed(&mut remote).await;
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), remote.stdin.read_to_end(&mut rest))
            .await
            .expect("stdin closed")
            .unwrap();
        // The screen stays readable, and keystrokes go nowhere.
        assert_eq!(snap.terminal.row_text(0), "staging$");
        session.write(b"rm -rf /tmp/x\r".to_vec());
        session.close();
        wait_for(&mut session, "closed", |s| s.state.is_finished()).await;
        assert!(!session.changed().await, "the session task ended");
        assert!(rest.is_empty());
    }

    // covers UP-06, UP-41
    #[tokio::test]
    async fn a_removed_context_ends_the_shell() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let context = FakeContext::new();
        let mut session = open_watched(&launcher, request(), context.check());
        remote.next_resize().await;
        context.point_to(None);
        let snap = wait_for(&mut session, "the end", |s| s.state.is_finished()).await;
        assert_eq!(
            snap.state,
            ExecState::Failed(ExecFailure::new(
                ExecFailureKind::ContextChanged,
                "\"main\" is no longer in your kubeconfig files"
            ))
        );
        connection_closed(&mut remote).await;
    }

    // covers UP-06, UP-41: a connection still being opened when the context moves is dropped,
    // and no shell starts.
    #[tokio::test]
    async fn a_context_that_moves_while_connecting_stops_the_connect() {
        /// A launch that never finishes, and says when it is dropped.
        #[derive(Clone)]
        struct Hanging(Arc<Mutex<Option<oneshot::Sender<()>>>>);
        impl Launcher for Hanging {
            fn launch(&self, _: Vec<String>) -> BoxFuture<'static, Result<Process, LaunchError>> {
                let dropped = self.0.lock().unwrap().take();
                async move {
                    let _dropped = dropped;
                    std::future::pending().await
                }
                .boxed()
            }
        }
        let (dropped, connect_dropped) = oneshot::channel();
        let launcher = Hanging(Arc::new(Mutex::new(Some(dropped))));
        let context = FakeContext::new();
        let mut session = open_watched(&launcher, request(), context.check());
        context.checked(2).await;
        assert_eq!(session.peek().state, ExecState::Connecting);
        context.point_to(Some("https://production"));
        let snap = wait_for(&mut session, "the end", |s| s.state.is_finished()).await;
        let ExecState::Failed(failure) = &snap.state else {
            panic!("{:?}", snap.state);
        };
        assert_eq!(failure.kind, ExecFailureKind::ContextChanged);
        tokio::time::timeout(Duration::from_secs(5), connect_dropped)
            .await
            .expect("the connect was dropped")
            .unwrap_err();
    }

    // covers UP-41
    #[tokio::test]
    async fn dropping_the_session_closes_the_command() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let session = open(&launcher, request());
        remote.next_resize().await;
        drop(session);
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), remote.stdin.read_to_end(&mut rest))
            .await
            .expect("stdin closed")
            .unwrap();
    }

    // covers UP-41
    #[tokio::test]
    async fn a_broken_connection_fails_the_session_even_if_output_never_ends() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = kube_like_process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        remote.print(b"partial").await;
        wait_for(&mut session, "output", |s| {
            s.terminal.row_text(0) == "partial"
        })
        .await;
        remote.print(b" line").await;
        remote.break_connection(
            "WebSocket protocol error: Connection reset without closing handshake",
        );
        let snap = wait_for(&mut session, "failure", |s| s.state.is_finished()).await;
        let ExecState::Failed(message) = &snap.state else {
            panic!("{:?}", snap.state);
        };
        assert!(
            message.starts_with("the exec connection broke: ") && message.contains("reset"),
            "{message}"
        );
        assert_eq!(message.kind, ExecFailureKind::Connection);
        // Output already in the pipe still reaches the screen.
        assert_eq!(snap.terminal.row_text(0), "partial line");
    }

    // covers UP-41
    #[tokio::test]
    async fn a_clean_close_with_kube_like_output_ends_the_session() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = kube_like_process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        remote.print(b"bye").await;
        // The server closes the WebSocket without a status; the stream is not shut down.
        remote.status = None;
        remote.stdout = None;
        remote.finish = None;
        let snap = wait_for(&mut session, "exit", |s| s.state.is_finished()).await;
        assert_eq!(snap.state, ExecState::Exited { code: None });
        assert_eq!(snap.terminal.row_text(0), "bye");
    }

    // covers UP-41
    #[tokio::test]
    async fn scrolling_back_and_typing_returns_to_the_bottom() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        for i in 0..10 {
            remote.print(format!("l{i}\r\n").as_bytes()).await;
        }
        wait_for(&mut session, "output", |s| s.terminal.history_lines > 0).await;
        session.scroll(2);
        wait_for(&mut session, "scrolled", |s| s.terminal.display_offset == 2).await;
        session.write(b"q".to_vec());
        wait_for(&mut session, "bottom", |s| s.terminal.display_offset == 0).await;
    }

    // covers UP-41, UP-04
    #[tokio::test]
    async fn connect_errors_fail_the_session_without_secrets() {
        use std::os::unix::process::ExitStatusExt;
        let auth = kube::Error::Auth(kube::client::AuthError::AuthExecRun {
            cmd: "aws eks get-token AWS_SECRET_ACCESS_KEY=s3cr3t".to_owned(),
            out: Box::new(std::process::Output {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: b"token-abc".to_vec(),
                stderr: b"s3cr3t".to_vec(),
            }),
        });
        let launcher = FakeLauncher::default();
        launcher.push(Err(LaunchError::kube(auth, "bash")));
        let mut session = open(&launcher, request());
        let snap = wait_for(&mut session, "failure", |s| s.state.is_finished()).await;
        let ExecState::Failed(message) = &snap.state else {
            panic!("{:?}", snap.state);
        };
        assert!(message.starts_with("authentication failed"), "{message}");
        // A local error: Flint could not sign in, the pod never answered.
        assert_eq!(message.kind, ExecFailureKind::SignIn);
        assert!(!message.contains("s3cr3t") && !message.contains("token-abc"));
        assert_eq!(launcher.launched().len(), 1, "auth errors are not retried");
    }

    // covers UP-41
    #[tokio::test]
    async fn without_a_client_the_session_fails_at_once() {
        let (mut session, channels) = ExecSession::new(request());
        let refusal = WriteRefusal::NotConnected {
            cluster: "dev".into(),
        };
        tokio::spawn(start(Err(refusal.clone()), request(), channels));
        let snap = wait_for(&mut session, "failure", |s| s.state.is_finished()).await;
        assert_eq!(
            snap.state,
            ExecState::Failed(ExecFailure::new(
                ExecFailureKind::Unreachable,
                refusal.to_string()
            ))
        );
    }

    // covers UP-54: on a protected cluster the session fails at once, saying so, and nothing
    // reaches the cluster.
    #[tokio::test]
    async fn on_a_protected_cluster_the_session_is_refused() {
        let (mut session, channels) = ExecSession::new(request());
        let refusal = WriteRefusal::Protected {
            cluster: "dev".into(),
        };
        tokio::spawn(start(Err(refusal.clone()), request(), channels));
        let snap = wait_for(&mut session, "failure", |s| s.state.is_finished()).await;
        assert_eq!(
            snap.state,
            ExecState::Failed(ExecFailure::new(
                ExecFailureKind::Protected,
                refusal.to_string()
            ))
        );
    }

    // covers UP-41
    #[tokio::test]
    async fn a_missing_command_printed_on_the_tty_falls_back_on_a_clean_screen() {
        let launcher = FakeLauncher::default();
        let (bash, mut bash_remote) = process();
        let (sh, mut sh_remote) = process();
        launcher.push(Ok(bash));
        launcher.push(Ok(sh));
        let mut session = open(&launcher, request());
        bash_remote.next_resize().await;
        bash_remote
            .print(b"OCI runtime exec failed: exec: \"bash\": executable file not found in $PATH")
            .await;
        wait_for(&mut session, "the diagnostic", |s| {
            s.terminal.text().contains("OCI runtime")
        })
        .await;
        // Typed after the diagnostic appeared: still replayed to the shell that starts.
        session.write(b"y".to_vec());
        assert_eq!(bash_remote.read_stdin(1).await, b"y");
        bash_remote.exit(exit_status("126"));
        let snap = wait_for(
            &mut session,
            "sh",
            |s| matches!(&s.state, ExecState::Running { command } if command == &["/bin/sh"]),
        )
        .await;
        assert_eq!(snap.terminal.text().trim(), "", "the error is cleared");
        sh_remote.next_resize().await;
        assert_eq!(sh_remote.read_stdin(1).await, b"y");
    }

    // covers UP-41
    #[tokio::test]
    async fn a_command_that_ran_keeps_its_exit_code() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(
            &launcher,
            request().command(vec!["bash".into(), "-c".into(), "/missing".into()]),
        );
        remote.next_resize().await;
        remote
            .print(b"bash: line 1: /missing: No such file or directory\r\n")
            .await;
        remote.exit(exit_status("127"));
        let snap = wait_for(&mut session, "exit", |s| s.state.is_finished()).await;
        assert_eq!(snap.state, ExecState::Exited { code: Some(127) });
    }

    // covers UP-41
    #[tokio::test(start_paused = true)]
    async fn the_last_change_of_a_burst_is_published_after_exit() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        for i in 0..10 {
            remote.print(format!("l{i}\r\n").as_bytes()).await;
        }
        remote.exit(exit_status("1"));
        wait_for(&mut session, "exit", |s| s.state.is_finished()).await;
        // Two scrolls within one frame: the second is held back, then published.
        session.scroll(1);
        session.scroll(1);
        tokio::time::sleep(FRAME * 3).await;
        assert_eq!(session.latest().terminal.display_offset, 2);
    }

    // covers UP-41
    #[tokio::test]
    async fn narrow_sizes_reach_the_tty_normalized() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let session = open(&launcher, request());
        assert_eq!(remote.next_resize().await, (40, 5));
        session.resize(TerminalSize { cols: 1, rows: 5 });
        assert_eq!(remote.next_resize().await, (2, 5));
    }

    // covers UP-41
    #[tokio::test]
    async fn an_unfinished_synchronized_update_is_shown_after_its_timeout() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        // The program began an update and was interrupted before ending it.
        remote.print(b"\x1b[?2026hprompt$ ").await;
        wait_for(&mut session, "the held-back prompt", |s| {
            s.terminal.row_text(0) == "prompt$"
        })
        .await;
    }

    // covers UP-41
    #[tokio::test]
    async fn an_unfinished_synchronized_update_is_shown_at_exit() {
        let launcher = FakeLauncher::default();
        let (proc_, mut remote) = process();
        launcher.push(Ok(proc_));
        let mut session = open(&launcher, request());
        remote.next_resize().await;
        remote.print(b"\x1b[?2026hbye").await;
        remote.exit(status("Success", "", ""));
        let snap = wait_for(&mut session, "exit", |s| s.state.is_finished()).await;
        assert_eq!(snap.terminal.row_text(0), "bye");
    }

    #[test]
    fn only_server_errors_can_mean_a_missing_command() {
        let local = kube::Error::Service(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        )));
        let local = LaunchError::kube(local, "bash");
        assert!(!local.missing_command);
        // Not the container's, nor known to be signing in: no cause to name.
        assert_eq!(local.kind, ExecFailureKind::Other);
        // The exec auth plugin is not on PATH ("… not found on PATH").
        let plugin = kube::Error::Auth(kube::client::AuthError::AuthExecStart(
            std::io::Error::from(std::io::ErrorKind::NotFound),
        ));
        assert_eq!(
            LaunchError::kube(plugin, "bash").kind,
            ExecFailureKind::SignIn
        );
        assert_eq!(
            LaunchError::kube(api_error(NOT_FOUND), "bash").kind,
            ExecFailureKind::MissingShell
        );
        assert_eq!(
            LaunchError::kube(api_error("pods \"web\" is forbidden"), "bash").kind,
            ExecFailureKind::Server
        );
        assert!(LaunchError::kube(api_error(NOT_FOUND), "bash").missing_command);
        assert!(!LaunchError::kube(api_error(NOT_FOUND), "/bin/sh").missing_command);
    }

    // covers UP-41: where an exec failure came from is decided by its real cause: an auth error
    // anywhere in kube's chain signs in, a failed WebSocket upgrade (a proxy answering 200) is
    // its own, a bare HTTP code is not the pod's, and the catch-all names nothing.
    #[test]
    fn exec_failures_are_classified_by_their_cause() {
        use kube::client::UpgradeConnectionError as U;
        let kind = |err: kube::Error| LaunchError::kube(err, "bash").kind;
        assert_eq!(
            kind(kube::Error::UpgradeConnection(U::ProtocolSwitch(
                http::StatusCode::OK
            ))),
            ExecFailureKind::Upgrade
        );
        assert_eq!(
            kind(kube::Error::UpgradeConnection(
                U::MissingUpgradeWebSocketHeader
            )),
            ExecFailureKind::Upgrade
        );
        assert_eq!(
            kind(kube::Error::UpgradeConnection(U::ProtocolSwitch(
                http::StatusCode::UNAUTHORIZED
            ))),
            ExecFailureKind::SignIn
        );
        // A token file Flint could not read is an auth error: signing in.
        assert_eq!(
            kind(kube::Error::Auth(kube::client::AuthError::ReadTokenFile(
                std::io::Error::from(std::io::ErrorKind::NotFound),
                "/var/run/token".into()
            ))),
            ExecFailureKind::SignIn
        );
        // A proxy that strips the upgrade headers leaves the API server's 400; any other 400 is
        // the server's own refusal.
        let bad_request = |message: &str| {
            kube::Error::Api(Box::new(kube::core::Status {
                code: 400,
                reason: "BadRequest".into(),
                message: message.into(),
                ..Default::default()
            }))
        };
        assert_eq!(
            kind(bad_request("Upgrade request required")),
            ExecFailureKind::Upgrade
        );
        assert_eq!(
            kind(bad_request("container app is not valid for pod web")),
            ExecFailureKind::Server
        );
        // The kubelet refuses a stopped container before the upgrade in plain text, which kube
        // wraps in a `Status` of its own: the kubelet's phrase is the server's reason, any
        // other plain body stays a bare HTTP code.
        let plain = |message: &str| {
            kube::Error::Api(Box::new(kube::core::Status {
                code: 500,
                reason: "Failed to parse error data".into(),
                message: message.into(),
                ..Default::default()
            }))
        };
        let stopped = LaunchError::kube(plain("container not found (\"job\")"), "bash");
        assert_eq!(stopped.kind, ExecFailureKind::Server);
        assert_eq!(stopped.message, "container not found (\"job\")");
        assert!(!stopped.missing_command);
        let other = LaunchError::kube(plain("<html>secret page</html>"), "bash");
        assert_eq!(other.kind, ExecFailureKind::Other);
        assert!(!other.message.contains("secret"), "{}", other.message);
        let any = kube::Error::LinesCodecMaxLineLengthExceeded;
        let http = |code| ClusterError::Http { code, server: None };
        // A bare 404 (a proxy's page, a wrong server path) is not a pod problem; a bare 401
        // is signing in.
        assert_eq!(
            ExecFailureKind::of(&any, &http(404)),
            ExecFailureKind::Other
        );
        assert_eq!(
            ExecFailureKind::of(&any, &http(401)),
            ExecFailureKind::SignIn
        );
        assert_eq!(
            ExecFailureKind::of(&any, &ClusterError::Kube("something odd".into())),
            ExecFailureKind::Other
        );
    }

    #[test]
    fn outcomes_follow_the_exec_status() {
        let ok = status("Success", "", "");
        assert_eq!(
            outcome(Some(&ok), Ok(()), "bash", true, None),
            Outcome::Done(ExecState::Exited { code: Some(0) })
        );
        assert_eq!(
            outcome(Some(&exit_status("127")), Ok(()), "bash", false, None),
            Outcome::Done(ExecState::Exited { code: Some(127) })
        );
        let missing = status("Failure", "InternalError", NOT_FOUND);
        assert!(matches!(
            outcome(Some(&missing), Ok(()), "bash", false, None),
            Outcome::MissingCommand(_)
        ));
        // Once the command printed something, it ran: no fallback.
        assert!(matches!(
            outcome(Some(&missing), Ok(()), "bash", true, None),
            Outcome::Done(ExecState::Failed(_))
        ));
        assert_eq!(
            outcome(None, Ok(()), "bash", true, None),
            Outcome::Done(ExecState::Exited { code: None })
        );
        // Exit codes 126 and 127 mean a missing command only when the output says so.
        let printed: &[u8] = b"OCI runtime exec failed: exec failed: exec: \"bash\": \
            executable file not found in $PATH: unknown\r\n";
        let exit = |code| {
            outcome(
                Some(&exit_status(code)),
                Ok(()),
                "bash",
                true,
                Some(printed),
            )
        };
        assert!(matches!(exit("127"), Outcome::MissingCommand(_)));
        assert_eq!(
            exit("1"),
            Outcome::Done(ExecState::Exited { code: Some(1) })
        );
        // Another executable's launch error is not this one's.
        assert_eq!(
            outcome(
                Some(&exit_status("127")),
                Ok(()),
                "zsh",
                true,
                Some(printed)
            ),
            Outcome::Done(ExecState::Exited { code: Some(127) })
        );
        // A command that ran and failed to find a file keeps its exit code.
        let ran: &[u8] = b"bash: line 1: /missing: No such file or directory\r\n";
        assert_eq!(
            outcome(Some(&exit_status("127")), Ok(()), "bash", true, Some(ran)),
            Outcome::Done(ExecState::Exited { code: Some(127) })
        );
        assert_eq!(
            outcome(None, Err("reset".into()), "bash", true, None),
            Outcome::Done(ExecState::Failed(ExecFailure::new(
                ExecFailureKind::Connection,
                "the exec connection broke: reset"
            )))
        );
    }

    #[test]
    fn missing_commands_are_recognised_by_runtime_wording_and_name() {
        assert!(is_missing_command(NOT_FOUND, "bash"));
        assert!(
            !is_missing_command(NOT_FOUND, "/bin/sh"),
            "another executable"
        );
        assert!(is_missing_command(
            "exec failed: unable to start container process: exec: \"/bin/sh\": stat /bin/sh: no such file or directory",
            "/bin/sh"
        ));
        assert!(is_missing_command(
            "executable file `bash` not found in $PATH: No such file or directory",
            "bash"
        ));
        assert!(!is_missing_command(
            "NotFound: pods \"web\" not found",
            "bash"
        ));
        assert!(!is_missing_command(
            "BadRequest: container sidecar is not valid for pod web",
            "bash"
        ));
        assert!(!is_missing_command(
            "bash: /missing: No such file or directory",
            "bash"
        ));
    }

    #[test]
    fn launch_errors_are_recognised_from_their_first_bytes() {
        assert!(could_be_launch_error(b""));
        assert!(could_be_launch_error(b"\r\nOCI run"));
        assert!(could_be_launch_error(b"OCI runtime exec failed: exec: x"));
        assert!(!could_be_launch_error(b"root@pod:/# "));
        assert!(!could_be_launch_error(b"OCI rx"));
    }

    #[test]
    fn status_messages_name_reason_and_message() {
        assert_eq!(
            status_message(&status("Failure", "Forbidden", "no")),
            "Forbidden: no"
        );
        assert_eq!(
            status_message(&Status {
                code: Some(500),
                ..Default::default()
            }),
            "the command failed (HTTP 500)"
        );
    }

    #[test]
    fn requests_default_to_bash_then_sh() {
        let request = ExecRequest::shell("c", "ns", "p").container("app");
        assert_eq!(request.container.as_deref(), Some("app"));
        assert_eq!(
            request.candidates(),
            vec![vec!["bash".to_owned()], vec!["/bin/sh".to_owned()]]
        );
        let request = request.command(vec!["top".into()]);
        assert_eq!(request.candidates(), vec![vec!["top".to_owned()]]);
    }
}
