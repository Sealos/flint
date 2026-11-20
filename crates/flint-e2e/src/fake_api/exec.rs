//! Exec sessions, served as the API server serves `/api/v1/namespaces/<ns>/pods/<pod>/exec`
//! over a WebSocket, with a scripted shell ([`super::shell`]) behind it.
//!
//! What a real cluster does, recorded on the kind dev cluster (Kubernetes v1.37, containerd)
//! through kube-rs 5.1's `Client::connect`, and where it is decided:
//!
//! - **Before the upgrade**, as JSON `Status`es: the pod must exist (404 `pods "x" not found`);
//!   the container is picked as for logs (`validateContainer`, `pkg/registry/core/pod/strategy.go`:
//!   400 "a container name must be specified for pod X, choose one of: [a b]" or "container c is
//!   not valid for pod X"); the pod needs a node (400 "pod X does not have a host assigned",
//!   `streamLocation`); and the request must be an upgrade (400 "Upgrade request required",
//!   `UpgradeAwareHandler` in `k8s.io/apimachinery/pkg/util/proxy`). RBAC checks `get` on
//!   `pods/exec` for a WebSocket upgrade (kind's 403 says "cannot get resource \"pods/exec\"").
//!   With `AuthorizePodWebsocketUpgradeCreatePermission` it also checks `create`, and a role
//!   granting only `get` is refused "cannot create resource \"pods/exec\""
//!   ([`super::Fault::forbidden_as`]).
//! - **The kubelet's refusals**, still before the upgrade, come back as plain text that kube
//!   wraps in a `Status` it makes up (reason "Failed to parse error data"): 400 "you must
//!   specify at least 1 of stdin, stdout, stderr" (`NewOptions` in the
//!   kubelet's `cri/streaming/remotecommand/httpstream.go`, which also drops `stderr` with a
//!   `tty`), and 500 `container not found ("c")` for a container that is not running, whether
//!   it terminated or waits to start (`GetExec`, `pkg/kubelet/kubelet_pods.go`). That is the
//!   path where the kubelet serves the WebSocket itself (`ExtendWebSocketsToKubelet`, as on
//!   the recorded v1.37 cluster). Where the API server holds the WebSocket and translates to
//!   SPDY for the kubelet (Kubernetes 1.30 to 1.35, or that gate off), it upgrades first and
//!   the same refusals arrive after the 101 as a 500 `InternalError` status, "Internal error
//!   occurred: unable to upgrade connection: container not found (\"c\")". That shape follows
//!   `StreamTranslatorHandler` and the SPDY executor's upgrade error; it was not recorded.
//!   `Options::kubelet_websockets: false` serves it.
//! - **Protocol:** kube 5.1 offers `v5.channel.k8s.io, v4.channel.k8s.io` in one header
//!   (`kube-client/src/client/upgrade.rs`); the server takes the first it supports in the
//!   client's order (`handshake`, `k8s.io/apimachinery/pkg/util/httpstream/wsstream/conn.go`).
//!   Every binary frame starts with its channel: 0 stdin, 1 stdout, 2 stderr, 3 error (a
//!   `Status`), 4 resize (`{"Width":80,"Height":24}`). v5 adds 255: `[255, 0]` closes stdin.
//!   The fake also serves v4, where a 255 frame is ignored, and refuses a request offering
//!   neither with a 400 (kube always offers both).
//! - **After the upgrade:** the server first sends an empty stdout frame (`[1]`, which kube
//!   skips). When the command ends it sends one status on channel 3 and a close frame
//!   (1000, no reason), in that order (`StreamTranslatorHandler`,
//!   `k8s.io/apiserver/pkg/util/proxy/streamtranslator.go`): `{"metadata":{},"status":"Success"}`
//!   for 0, else a `NonZeroExitCode` failure with the code in an `ExitCode` cause (`codeExitToStatusError`).
//!   With a TTY, closing stdin does not end the shell.
//! - **A command that cannot start** (containerd and runc) is a status after the upgrade and
//!   before any output: a 500 `InternalError` whose message repeats "Internal error occurred:"
//!   and ends in runc's words, `exec: "bash": executable file not found in $PATH` (or
//!   `stat /bin/bash: no such file or directory` for a path, and `args must not be empty`
//!   for no command). Docker instead prints `OCI runtime exec failed: ...` on the TTY and exits
//!   126 ([`ExecImage::docker`]), which Flint's `exec.rs` recognises too.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::logs::Key;
use super::route::Query;
pub use super::shell::TtySize;
use super::shell::{Flavor, Program};

/// The remote command protocol with the stream close signal.
pub const V5: &str = "v5.channel.k8s.io";
/// The binary remote command protocol before it.
pub const V4: &str = "v4.channel.k8s.io";

pub(crate) const STDIN: u8 = 0;
pub(crate) const STDOUT: u8 = 1;
pub(crate) const STDERR: u8 = 2;
pub(crate) const ERROR: u8 = 3;
pub(crate) const RESIZE: u8 = 4;
/// v5's close signal; the channel to close follows.
pub(crate) const CLOSE: u8 = 255;

/// How a container's runtime reports a command it cannot find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecRuntime {
    /// containerd or CRI-O with runc (kind's): an error status before any output.
    Containerd,
    /// Docker (cri-dockerd): printed on the TTY, exit 126.
    Docker,
}

/// What an exec finds in a container: whether it has `bash` (every container has `sh` and
/// `/bin/sh`), whether it runs as root (for the prompt), and its runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecImage {
    pub bash: bool,
    pub root: bool,
    pub runtime: ExecRuntime,
}

impl ExecImage {
    /// bash and sh, as root (Debian's nginx).
    pub fn with_bash() -> Self {
        ExecImage {
            bash: true,
            root: true,
            runtime: ExecRuntime::Containerd,
        }
    }

    /// sh only, as root (busybox).
    pub fn sh_only() -> Self {
        ExecImage {
            bash: false,
            ..ExecImage::with_bash()
        }
    }

    /// Runs as a user other than root (podinfo's `app`): prompts end in `$`.
    pub fn non_root(mut self) -> Self {
        self.root = false;
        self
    }

    /// Reports a missing command the way Docker does.
    pub fn docker(mut self) -> Self {
        self.runtime = ExecRuntime::Docker;
        self
    }

    /// What the images the fixtures and [`super::FakePod`] use have: busybox, Alpine and
    /// podinfo (Alpine, user `app`) have no bash; anything else does.
    pub fn of(image: &str) -> Self {
        let reference = image
            .rsplit('/')
            .next()
            .unwrap_or(image)
            .to_ascii_lowercase();
        let (name, tag) = reference
            .split_once(['@', ':'])
            .unwrap_or((reference.as_str(), ""));
        match name {
            "podinfo" => ExecImage::sh_only().non_root(),
            "busybox" | "alpine" => ExecImage::sh_only(),
            _ if tag.contains("alpine") => ExecImage::sh_only(),
            _ => ExecImage::with_bash(),
        }
    }
}

/// The options of an exec request, as the kubelet reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecQuery {
    pub container: Option<String>,
    pub command: Vec<String>,
    pub stdin: bool,
    pub stdout: bool,
    /// Always off with a TTY.
    pub stderr: bool,
    pub tty: bool,
}

impl ExecQuery {
    pub fn new(query: &Query) -> Self {
        ExecQuery {
            container: query.container.clone(),
            command: query.command.clone(),
            stdin: query.stdin,
            stdout: query.stdout,
            stderr: query.stderr && !query.tty,
            tty: query.tty,
        }
    }

    /// The kubelet's refusal of options that open no stream.
    pub fn streams_error(&self) -> Option<&'static str> {
        (!self.stdin && !self.stdout && !self.stderr)
            .then_some("you must specify at least 1 of stdin, stdout, stderr")
    }
}

/// The subprotocol to speak, from a request's `Sec-WebSocket-Protocol`: the first the server
/// supports, in the client's order.
pub(crate) fn negotiate(offered: Option<&str>) -> Option<&'static str> {
    offered?
        .split(',')
        .map(str::trim)
        .find_map(|p| [V5, V4].into_iter().find(|s| *s == p))
}

/// Whether a request asks to upgrade to a WebSocket.
pub(crate) fn is_websocket_upgrade(headers: &http::HeaderMap) -> bool {
    let has = |name: http::header::HeaderName, token: &str| {
        headers.get_all(name).iter().any(|v| {
            v.to_str()
                .unwrap_or("")
                .split(',')
                .any(|t| t.trim().eq_ignore_ascii_case(token))
        })
    };
    has(http::header::CONNECTION, "upgrade")
        && has(http::header::UPGRADE, "websocket")
        && headers.contains_key(http::header::SEC_WEBSOCKET_KEY)
}

/// The kubelet's refusal of a container that is not running. A pod served from its row only
/// (no recorded status) counts as running.
pub(crate) fn check_running(pod: &Value, container: &str) -> Result<(), String> {
    if pod.get("status").is_none() {
        return Ok(());
    }
    let running = [
        "containerStatuses",
        "initContainerStatuses",
        "ephemeralContainerStatuses",
    ]
    .iter()
    .flat_map(|list| {
        pod.pointer(&format!("/status/{list}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    })
    .any(|s| s["name"] == container && s.pointer("/state/running").is_some());
    if running {
        Ok(())
    } else {
        Err(format!("container not found (\"{container}\")"))
    }
}

/// The API server's refusal of a pod not scheduled yet. A pod without a recorded spec passes.
pub(crate) fn check_host(pod: &Value) -> Result<(), String> {
    let name = pod
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    match pod.get("spec") {
        Some(spec)
            if spec
                .get("nodeName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty() =>
        {
            Err(format!("pod {name} does not have a host assigned"))
        }
        _ => Ok(()),
    }
}

/// The image of a container in a pod's spec, if recorded.
pub(crate) fn image_of(pod: &Value, container: &str) -> Option<String> {
    ["initContainers", "containers", "ephemeralContainers"]
        .iter()
        .flat_map(|list| {
            pod.pointer(&format!("/spec/{list}"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .find(|c| c["name"] == container)
        .and_then(|c| c["image"].as_str())
        .map(str::to_owned)
}

/// A frame on `channel`.
pub(crate) fn frame(channel: u8, bytes: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(bytes.len() + 1);
    frame.push(channel);
    frame.extend_from_slice(bytes);
    frame
}

/// The status of a command that exited with `code`.
pub(crate) fn exit_status(code: i32) -> Value {
    if code == 0 {
        return json!({"metadata": {}, "status": "Success"});
    }
    json!({
        "metadata": {},
        "status": "Failure",
        "message": format!(
            "command terminated with non-zero exit code: command terminated with exit code {code}"
        ),
        "reason": "NonZeroExitCode",
        "details": {"causes": [{"reason": "ExitCode", "message": code.to_string()}]},
    })
}

/// The status of a command the runtime could not start, `why` in runc's words.
fn launch_failure(exec_id: &str, why: &str) -> Value {
    let cause = format!(
        "Internal error occurred: error executing command in container: failed to exec in \
         container: failed to start exec \"{exec_id}\": OCI runtime exec failed: exec failed: {why}"
    );
    json!({
        "metadata": {},
        "status": "Failure",
        "message": format!("Internal error occurred: {cause}"),
        "reason": "InternalError",
        "details": {"causes": [{"message": cause}]},
        "code": 500,
    })
}

/// The status the API server sends after the upgrade when it, not the kubelet, holds the
/// WebSocket and the kubelet refuses (`Options::kubelet_websockets` off): its SPDY upgrade to
/// the kubelet failed with the kubelet's text, wrapped in `NewInternalError`.
pub(crate) fn upgrade_failure(kubelet: &str) -> Value {
    let cause = format!("unable to upgrade connection: {kubelet}");
    json!({
        "metadata": {},
        "status": "Failure",
        "message": format!("Internal error occurred: {cause}"),
        "reason": "InternalError",
        "details": {"causes": [{"message": cause}]},
        "code": 500,
    })
}

/// What an upgraded exec does first.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Launch {
    /// The command runs.
    Run(Program),
    /// It cannot start: this status, then the end.
    Fail(Value),
    /// It cannot start, and the runtime says so on the TTY with this exit code (Docker).
    Print(Vec<u8>, i32),
}

/// What `command` does in a container with `image`. `exec_id` stands for the runtime's id of
/// the exec in its errors.
pub(crate) fn launch(command: &[String], image: ExecImage, exec_id: &str) -> Launch {
    let Some(executable) = command.first() else {
        return Launch::Fail(launch_failure(exec_id, "args must not be empty"));
    };
    let program =
        Program::parse(command).filter(|p| image.bash || p.flavor() != Some(Flavor::Bash));
    if let Some(program) = program {
        return Launch::Run(program);
    }
    let why = if executable.contains('/') {
        format!(
            "unable to start container process: exec: \"{executable}\": stat {executable}: no \
             such file or directory"
        )
    } else {
        format!(
            "unable to start container process: exec: \"{executable}\": executable file not \
             found in $PATH"
        )
    };
    match image.runtime {
        ExecRuntime::Containerd => Launch::Fail(launch_failure(exec_id, &why)),
        ExecRuntime::Docker => Launch::Print(
            format!("OCI runtime exec failed: exec failed: {why}: unknown\r\n").into_bytes(),
            126,
        ),
    }
}

/// How an exec session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEnd {
    /// The command exited with this code (a status was sent, then a close frame).
    Exited(i32),
    /// The command could not start; the status said why.
    Failed(String),
    /// [`FakeExec::close`]: a close frame without a status.
    Closed,
    /// [`FakeExec::drop_connection`], or the server stopped: the connection was closed without a
    /// close frame.
    Dropped,
    /// The client closed the WebSocket or went away first.
    ClientGone,
}

/// What a test asks of a running session.
#[derive(Debug)]
pub(crate) enum Control {
    Print(u8, Vec<u8>),
    Exit(i32),
    Close,
    /// Stop reading and writing, and keep the connection open.
    Stall,
}

/// How much of a session's stdout is kept: the tail, so a flood does not pile up in memory.
pub const STDOUT_KEPT: usize = 1024 * 1024;

/// What a session saw and sent.
#[derive(Debug, Default)]
struct Record {
    stdin: Vec<u8>,
    resizes: Vec<TtySize>,
    stdin_closed: bool,
    /// The last [`STDOUT_KEPT`] bytes of stdout.
    stdout: VecDeque<u8>,
    /// Every byte sent on stdout.
    stdout_len: u64,
    end: Option<ExecEnd>,
}

#[derive(Debug)]
struct Session {
    namespace: String,
    pod: String,
    container: String,
    command: Vec<String>,
    tty: bool,
    protocol: &'static str,
    record: Mutex<Record>,
    control: mpsc::UnboundedSender<Control>,
    /// Set to break the connection at once, whatever the session is doing.
    kill: watch::Sender<bool>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("fake exec session lock")
}

/// One exec session the fake server upgraded: what the client sent, and controls to print,
/// exit or break the connection. Cheap to clone; every clone is the same session.
#[derive(Debug, Clone)]
pub struct FakeExec(Arc<Session>);

/// How often the `wait_for_*` helpers look.
const POLL: Duration = Duration::from_millis(10);

impl FakeExec {
    pub(crate) fn new(
        key: &Key,
        query: &ExecQuery,
        protocol: &'static str,
    ) -> (FakeExec, mpsc::UnboundedReceiver<Control>) {
        let (control, rx) = mpsc::unbounded_channel();
        let session = Session {
            namespace: key.0.clone(),
            pod: key.1.clone(),
            container: key.2.clone(),
            command: query.command.clone(),
            tty: query.tty,
            protocol,
            record: Mutex::default(),
            control,
            kill: watch::Sender::new(false),
        };
        (FakeExec(Arc::new(session)), rx)
    }

    pub fn namespace(&self) -> &str {
        &self.0.namespace
    }

    pub fn pod(&self) -> &str {
        &self.0.pod
    }

    /// The container, as the API server picked it.
    pub fn container(&self) -> &str {
        &self.0.container
    }

    /// The command and its arguments.
    pub fn command(&self) -> &[String] {
        &self.0.command
    }

    pub fn tty(&self) -> bool {
        self.0.tty
    }

    /// The subprotocol spoken ([`V5`] with kube).
    pub fn protocol(&self) -> &str {
        self.0.protocol
    }

    /// Every byte the client sent on stdin, in order: keystrokes, pastes with their
    /// bracketed-paste markers, Ctrl-C (`\x03`), Ctrl-D (`\x04`), and the terminal's answers to
    /// queries (busybox asks for the cursor position with every prompt).
    pub fn stdin(&self) -> Vec<u8> {
        lock(&self.0.record).stdin.clone()
    }

    /// Every resize the client sent, in order. Flint sends one as soon as the command starts.
    pub fn resizes(&self) -> Vec<TtySize> {
        lock(&self.0.record).resizes.clone()
    }

    /// Whether the client closed stdin (v5's close signal).
    pub fn stdin_closed(&self) -> bool {
        lock(&self.0.record).stdin_closed
    }

    /// What was sent on stdout: all of it, or its last [`STDOUT_KEPT`] bytes after a flood.
    pub fn stdout(&self) -> Vec<u8> {
        lock(&self.0.record).stdout.iter().copied().collect()
    }

    /// How many bytes were sent on stdout in all.
    pub fn stdout_len(&self) -> u64 {
        lock(&self.0.record).stdout_len
    }

    /// How the session ended; `None` while it is open.
    pub fn end(&self) -> Option<ExecEnd> {
        lock(&self.0.record).end.clone()
    }

    pub fn is_open(&self) -> bool {
        self.end().is_none()
    }

    fn control(&self, control: Control) -> bool {
        self.is_open() && self.0.control.send(control).is_ok()
    }

    /// Sends `bytes` on stdout as the command's output (written as they are: use `\r\n` on a
    /// TTY). Returns false once the session ended.
    pub fn print(&self, bytes: impl AsRef<[u8]>) -> bool {
        self.control(Control::Print(STDOUT, bytes.as_ref().to_vec()))
    }

    /// Sends `bytes` on stderr (a session without a TTY; with one, use [`FakeExec::print`]).
    pub fn print_stderr(&self, bytes: impl AsRef<[u8]>) -> bool {
        self.control(Control::Print(STDERR, bytes.as_ref().to_vec()))
    }

    /// Ends the command with `code`, as if it exited: the status, then a close frame.
    pub fn exit(&self, code: i32) -> bool {
        self.control(Control::Exit(code))
    }

    /// Closes the WebSocket cleanly without a status, as a server shutting down does.
    pub fn close(&self) -> bool {
        self.control(Control::Close)
    }

    /// Closes the connection abruptly, at once, even while a write is stuck: no status, no
    /// close frame and no TLS close_notify, just the TCP close. The client reads an unexpected
    /// end straight away, as when a proxy or the API server dies. (A network that silently
    /// stops delivering is [`FakeExec::stall`].)
    pub fn drop_connection(&self) -> bool {
        if !self.is_open() {
            return false;
        }
        self.0.kill.send_replace(true);
        true
    }

    /// Stops reading and writing while keeping the connection open, as a network that silently
    /// stops delivering does: the client sees no end, only silence (its writes fill the
    /// buffers). Detecting that is left to the client's keepalives, which these tests do not
    /// wait for; [`FakeExec::drop_connection`] ends a stalled session.
    pub fn stall(&self) -> bool {
        self.control(Control::Stall)
    }

    /// Resolves when [`FakeExec::drop_connection`] was called.
    pub(crate) async fn killed(&self) {
        let mut kill = self.0.kill.subscribe();
        let _ = kill.wait_for(|k| *k).await;
    }

    /// Waits until stdin holds `bytes` (anywhere, so earlier input does not matter).
    pub async fn wait_for_stdin(&self, bytes: impl AsRef<[u8]>, within: Duration) -> Result<()> {
        let needle = bytes.as_ref();
        self.wait(within, || contains(&self.stdin(), needle))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "stdin never held {:?} within {within:?}; it holds {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&self.stdin())
                )
            })
    }

    /// Waits until at least `count` resizes arrived.
    pub async fn wait_for_resizes(&self, count: usize, within: Duration) -> Result<()> {
        self.wait(within, || self.resizes().len() >= count)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "not {count} resize(s) within {within:?}: {:?}",
                    self.resizes()
                )
            })
    }

    /// Waits until the session ended, and says how.
    pub async fn wait_for_end(&self, within: Duration) -> Result<ExecEnd> {
        self.wait(within, || !self.is_open()).await.map_err(|_| {
            anyhow::anyhow!(
                "the exec session in {} is still open after {within:?}",
                self.0.pod
            )
        })?;
        Ok(self.end().expect("ended"))
    }

    async fn wait(&self, within: Duration, done: impl Fn() -> bool) -> Result<(), ()> {
        tokio::time::timeout(within, async {
            while !done() {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .map_err(|_| ())
    }

    pub(crate) fn record_stdin(&self, bytes: &[u8]) {
        lock(&self.0.record).stdin.extend_from_slice(bytes);
    }

    pub(crate) fn record_resize(&self, size: TtySize) {
        lock(&self.0.record).resizes.push(size);
    }

    pub(crate) fn record_stdin_closed(&self) {
        lock(&self.0.record).stdin_closed = true;
    }

    pub(crate) fn record_stdout(&self, bytes: &[u8]) {
        let mut record = lock(&self.0.record);
        record.stdout_len += bytes.len() as u64;
        let kept = &bytes[bytes.len().saturating_sub(STDOUT_KEPT)..];
        record.stdout.extend(kept);
        let excess = record.stdout.len().saturating_sub(STDOUT_KEPT);
        record.stdout.drain(..excess);
    }

    /// Records how the session ended; the first end counts.
    pub(crate) fn record_end(&self, end: ExecEnd) {
        lock(&self.0.record).end.get_or_insert(end);
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// The server's exec state: images set by tests, and every session so far.
#[derive(Debug, Default)]
pub(crate) struct Execs {
    images: HashMap<Key, ExecImage>,
    sessions: Vec<FakeExec>,
    serial: u64,
}

impl Execs {
    pub fn set_image(&mut self, key: Key, image: ExecImage) {
        self.images.insert(key, image);
    }

    /// The image of a container: set by a test, else guessed from its spec ([`ExecImage::of`]).
    pub fn image(&self, key: &Key, pod: &Value) -> ExecImage {
        self.images.get(key).copied().unwrap_or_else(|| {
            image_of(pod, &key.2).map_or_else(ExecImage::with_bash, |i| ExecImage::of(&i))
        })
    }

    /// A new exec id, 64 hex digits like containerd's.
    pub fn next_id(&mut self) -> String {
        self.serial += 1;
        format!("{:064x}", (0x00c0_ffee_u64 << 32) | self.serial)
    }

    pub fn add(&mut self, session: FakeExec) {
        self.sessions.push(session);
    }

    pub fn sessions(&self) -> Vec<FakeExec> {
        self.sessions.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn images_say_whether_bash_is_there() {
        assert_eq!(ExecImage::of("nginx:1.27"), ExecImage::with_bash());
        assert_eq!(
            ExecImage::of("docker.io/library/busybox:1.37.0"),
            ExecImage::sh_only()
        );
        assert_eq!(
            ExecImage::of("ghcr.io/stefanprodan/podinfo:6.17.0"),
            ExecImage::sh_only().non_root()
        );
        assert_eq!(ExecImage::of("nginx:1.27-alpine"), ExecImage::sh_only());
        assert_eq!(ExecImage::of("alpine@sha256:abc"), ExecImage::sh_only());
        assert_eq!(ExecImage::with_bash().docker().runtime, ExecRuntime::Docker);
    }

    #[test]
    fn the_first_offered_protocol_the_server_speaks_wins() {
        assert_eq!(
            negotiate(Some("v5.channel.k8s.io, v4.channel.k8s.io")),
            Some(V5)
        );
        assert_eq!(
            negotiate(Some("v4.channel.k8s.io,v5.channel.k8s.io")),
            Some(V4)
        );
        assert_eq!(
            negotiate(Some("channel.k8s.io, v4.channel.k8s.io")),
            Some(V4)
        );
        assert_eq!(negotiate(Some("base64.channel.k8s.io")), None);
        assert_eq!(negotiate(None), None);
    }

    #[test]
    fn upgrades_need_the_websocket_headers() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONNECTION,
            "keep-alive, Upgrade".parse().unwrap(),
        );
        headers.insert(http::header::UPGRADE, "WebSocket".parse().unwrap());
        assert!(!is_websocket_upgrade(&headers));
        headers.insert(http::header::SEC_WEBSOCKET_KEY, "a2V5".parse().unwrap());
        assert!(is_websocket_upgrade(&headers));
        headers.insert(http::header::UPGRADE, "h2c".parse().unwrap());
        assert!(!is_websocket_upgrade(&headers));
    }

    #[test]
    fn options_follow_the_kubelet() {
        let query = Query::parse(Some("command=sh&stdout=true&stderr=true&tty=true"));
        let exec = ExecQuery::new(&query);
        assert!(exec.stdout && exec.tty && !exec.stderr);
        assert_eq!(exec.streams_error(), None);
        let none = ExecQuery::new(&Query::parse(Some("command=sh&tty=true")));
        assert_eq!(
            none.streams_error(),
            Some("you must specify at least 1 of stdin, stdout, stderr")
        );
    }

    #[test]
    fn only_running_containers_on_a_node_take_an_exec() {
        let pod = json!({
            "metadata": {"name": "shell"},
            "spec": {"nodeName": "n", "containers": [
                {"name": "web", "image": "nginx:1.27"}, {"name": "done", "image": "busybox"}
            ]},
            "status": {"containerStatuses": [
                {"name": "web", "state": {"running": {}}},
                {"name": "done", "state": {"terminated": {"exitCode": 0}}},
                {"name": "pending", "state": {"waiting": {"reason": "ImagePullBackOff"}}}
            ]}
        });
        assert_eq!(check_running(&pod, "web"), Ok(()));
        assert_eq!(
            check_running(&pod, "done"),
            Err("container not found (\"done\")".into())
        );
        assert!(check_running(&pod, "pending").is_err());
        assert_eq!(check_running(&json!({"metadata": {}}), "x"), Ok(()));
        assert_eq!(check_host(&pod), Ok(()));
        assert_eq!(
            check_host(&json!({"metadata": {"name": "p"}, "spec": {}})),
            Err("pod p does not have a host assigned".into())
        );
        assert_eq!(image_of(&pod, "web").as_deref(), Some("nginx:1.27"));
        assert_eq!(image_of(&pod, "nope"), None);
    }

    #[test]
    fn statuses_match_the_recorded_ones() {
        // Recorded on kind: `exit 3` in bash, and `exit` in sh.
        let recorded: Value = serde_json::from_str(
            r#"{"metadata":{},"status":"Failure","message":"command terminated with non-zero exit code: command terminated with exit code 3","reason":"NonZeroExitCode","details":{"causes":[{"reason":"ExitCode","message":"3"}]}}"#,
        )
        .unwrap();
        assert_eq!(exit_status(3), recorded);
        assert_eq!(exit_status(0), json!({"metadata": {}, "status": "Success"}));
        let Launch::Fail(missing) = launch(&argv(&["bash"]), ExecImage::sh_only(), "8c31") else {
            panic!("bash is missing");
        };
        let recorded: Value = serde_json::from_str(
            r#"{"metadata":{},"status":"Failure","message":"Internal error occurred: Internal error occurred: error executing command in container: failed to exec in container: failed to start exec \"8c31\": OCI runtime exec failed: exec failed: unable to start container process: exec: \"bash\": executable file not found in $PATH","reason":"InternalError","details":{"causes":[{"message":"Internal error occurred: error executing command in container: failed to exec in container: failed to start exec \"8c31\": OCI runtime exec failed: exec failed: unable to start container process: exec: \"bash\": executable file not found in $PATH"}]},"code":500}"#,
        )
        .unwrap();
        assert_eq!(missing, recorded);
    }

    #[test]
    fn refusals_after_the_upgrade_wrap_the_kubelets_text() {
        let status = upgrade_failure("container not found (\"done\")");
        assert_eq!(status["code"], 500);
        assert_eq!(status["reason"], "InternalError");
        assert_eq!(
            status["message"],
            "Internal error occurred: unable to upgrade connection: container not found (\"done\")"
        );
        assert_eq!(
            status["details"]["causes"][0]["message"],
            "unable to upgrade connection: container not found (\"done\")"
        );
    }

    #[test]
    fn launches_run_fail_or_print_by_runtime() {
        assert!(matches!(
            launch(&argv(&["bash"]), ExecImage::with_bash(), "1"),
            Launch::Run(_)
        ));
        assert!(matches!(
            launch(&argv(&["/bin/sh"]), ExecImage::sh_only(), "1"),
            Launch::Run(_)
        ));
        let Launch::Fail(status) = launch(&[], ExecImage::with_bash(), "1") else {
            panic!("no command");
        };
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .ends_with("exec failed: args must not be empty")
        );
        let Launch::Fail(status) = launch(&argv(&["/bin/bash"]), ExecImage::sh_only(), "1") else {
            panic!("no bash");
        };
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .ends_with("exec: \"/bin/bash\": stat /bin/bash: no such file or directory")
        );
        assert!(matches!(
            launch(&argv(&["zsh"]), ExecImage::with_bash(), "1"),
            Launch::Fail(_)
        ));
        assert_eq!(
            launch(&argv(&["bash"]), ExecImage::sh_only().docker(), "1"),
            Launch::Print(
                b"OCI runtime exec failed: exec failed: unable to start container process: exec: \
                  \"bash\": executable file not found in $PATH: unknown\r\n"
                    .to_vec(),
                126
            )
        );
    }

    #[test]
    fn frames_lead_with_their_channel() {
        assert_eq!(frame(STDOUT, b"hi"), b"\x01hi");
        assert_eq!(frame(STDIN, b""), [STDIN]);
        assert_eq!([ERROR, RESIZE, CLOSE], [3, 4, 255]);
    }

    #[tokio::test]
    async fn sessions_record_what_they_saw_and_end_once() {
        let key = ("demo".to_owned(), "web".to_owned(), "nginx".to_owned());
        let query = ExecQuery::new(&Query::parse(Some("command=bash&stdin=1&stdout=1&tty=1")));
        let (session, mut rx) = FakeExec::new(&key, &query, V5);
        assert_eq!(
            (session.namespace(), session.pod(), session.container()),
            ("demo", "web", "nginx")
        );
        assert_eq!(session.command(), ["bash"]);
        assert!(session.tty() && session.protocol() == V5);
        session.record_stdin(b"echo ");
        session.record_stdin(b"hi\r");
        session.wait_for_stdin("hi\r", POLL).await.unwrap();
        assert!(session.wait_for_stdin("nope", POLL).await.is_err());
        session.record_resize(TtySize {
            width: 80,
            height: 24,
        });
        session.wait_for_resizes(1, POLL).await.unwrap();
        session.record_stdout(b"out");
        session.record_stdin_closed();
        assert!(session.stdin_closed() && session.stdout() == b"out");
        assert!(session.print("x"));
        assert!(matches!(rx.recv().await, Some(Control::Print(STDOUT, b)) if b == b"x"));
        // A flood keeps only its tail, and counts everything.
        let mut flood = vec![b'a'; STDOUT_KEPT];
        flood.extend_from_slice(b"tail");
        session.record_stdout(&flood);
        let kept = session.stdout();
        assert_eq!(kept.len(), STDOUT_KEPT);
        assert!(kept.ends_with(b"atail"));
        assert_eq!(session.stdout_len(), 3 + flood.len() as u64);
        session.record_stdout(b"!");
        assert!(session.stdout().ends_with(b"tail!") && session.stdout().len() == STDOUT_KEPT);
        assert!(session.stall());
        assert!(matches!(rx.recv().await, Some(Control::Stall)));
        assert!(session.drop_connection());
        tokio::time::timeout(POLL, session.killed()).await.unwrap();
        assert!(session.wait_for_end(POLL).await.is_err());
        session.record_end(ExecEnd::Exited(3));
        session.record_end(ExecEnd::ClientGone);
        assert_eq!(
            session.wait_for_end(POLL).await.unwrap(),
            ExecEnd::Exited(3)
        );
        assert!(!session.exit(0) && !session.close() && !session.drop_connection());
        assert!(!session.print_stderr("late"));

        let mut execs = Execs::default();
        let first = execs.next_id();
        assert_eq!(first.len(), 64);
        assert_ne!(first, execs.next_id());
        let pod = json!({"spec": {"containers": [{"name": "nginx", "image": "busybox"}]}});
        assert_eq!(execs.image(&key, &pod), ExecImage::sh_only());
        execs.set_image(key.clone(), ExecImage::with_bash().docker());
        assert_eq!(execs.image(&key, &pod), ExecImage::with_bash().docker());
        execs.add(session);
        assert_eq!(execs.sessions().len(), 1);
        assert!(contains(b"abc", b"") && contains(b"abc", b"bc") && !contains(b"ab", b"abc"));
    }
}
