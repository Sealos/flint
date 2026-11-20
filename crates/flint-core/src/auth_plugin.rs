//! Exec auth plugins (`aws eks get-token`, `gke-gcloud-auth-plugin`, `kubelogin`, …), run by
//! Flint instead of kube so that a plugin that hangs can be ended.
//!
//! kube 5.1 runs the plugin with a blocking `Command::output()`, when it builds the client and
//! again at every token refresh. A timeout around that only stops the wait: the blocking thread
//! and the plugin process live on until the plugin exits by itself, and every retry starts
//! another. Here the plugin is a tokio child in a process group of its own. Dropping the future
//! that waits for it (a timeout, a cancelled connection attempt) kills the group, so the plugin and
//! whatever it started, and tokio reaps the plugin. No blocking thread is involved.
//!
//! The group is only ever killed while its leader (the plugin) is not reaped yet, so its id cannot
//! name another group: the output is read to its end first, and the plugin is reaped last. A
//! descendant that left the group (a caching daemon) and keeps the output open gets
//! [`DRAIN_GRACE`] after the plugin exits; then what the plugin printed is used as it is.
//!
//! A plugin that may prompt, while Flint runs in the foreground of a terminal, is given that
//! terminal once it started (its group becomes the terminal's foreground group; one such plugin at
//! a time) and Flint takes it back after:
//! the plugin can still ask for a password, and still dies with its group.
//!
//! Otherwise this follows kube's `auth_exec` (kube-client 5.1): the same arguments, environment,
//! `KUBERNETES_EXEC_INFO` and interactive handling, YAML-tolerant parsing, kube's precedence of
//! credentials, a token refreshed a minute before it expires, and kube's error values
//! ([`AuthError`]), so errors read as before.

use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared};
use http::header::AUTHORIZATION;
use http::{HeaderValue, Request};
use jiff::{SignedDuration, Timestamp};
use kube::client::AuthError;
use kube::config::{ExecConfig, ExecInteractiveMode};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tower::BoxError;
use tower::filter::{AsyncFilterLayer, AsyncPredicate};

use crate::cluster::CONNECT_TIMEOUT;
use crate::redact;

/// A token this close to its expiry is refreshed first, as kube does.
const REFRESH_MARGIN: SignedDuration = SignedDuration::from_secs(60);
/// The most a plugin may print on stdout: an `ExecCredential` is a few KiB.
const STDOUT_CAP: usize = 1024 * 1024;
/// The most a plugin may print on stderr when it is captured.
const STDERR_CAP: usize = 64 * 1024;
/// How long the output may stay open after the plugin exited (held by something it started).
pub(crate) const DRAIN_GRACE: Duration = Duration::from_secs(1);
/// How often a running plugin is checked for having exited, while its output is still open.
const EXIT_POLL: Duration = Duration::from_millis(25);
/// After a refresh of a plugin that may prompt timed out, refreshes fail at once for this long,
/// so a browser login nobody completes does not open again every minute.
const PROMPT_BACK_OFF: Duration = Duration::from_secs(5 * 60);

/// How [`AuthError::AuthExec`] texts of Flint's own start, so [`describe`] can word them: kube's
/// texts there may quote the plugin's output, Flint's name only the plugin's program.
const TIMED_OUT: &str = "flint:timed-out:";
const TOO_LARGE: &str = "flint:too-large:";
const RESTING: &str = "flint:resting:";

/// What a plugin run returned.
#[derive(Debug)]
pub(crate) enum Credential {
    Token {
        token: SecretString,
        expires: Option<Timestamp>,
    },
    /// PEM text, as the plugin printed it.
    Certificate {
        certificate: String,
        key: SecretString,
    },
    /// A status with neither: kube sends no credential then.
    None,
}

/// Runs the exec plugin of `config`'s user, if kube would, and puts its credential in `config`
/// in place of the plugin. A token with an expiry comes back as a [`TokenLayer`] for the client
/// stack, which refreshes it. Cancel-safe: dropping the future kills the plugin.
pub(crate) async fn resolve(config: &mut kube::Config) -> Result<Option<TokenLayer>, AuthError> {
    let auth = &config.auth_info;
    // kube's precedence (`Auth::try_from`): all of these win over an exec plugin.
    let other = auth.auth_provider.is_some()
        || (auth.username.is_some() && auth.password.is_some())
        || auth.token.is_some()
        || auth.token_file.is_some();
    if other || auth.exec.is_none() {
        return Ok(None);
    }
    let Some(exec) = config.auth_info.exec.take() else {
        return Ok(None);
    };
    let auth = &mut config.auth_info;
    Ok(match run(&exec).await? {
        Credential::Certificate { certificate, key } => {
            // kube reads these as base64; the plugin's certificate replaces any in kubeconfig.
            let base64 = base64::engine::general_purpose::STANDARD;
            auth.client_certificate_data = Some(base64.encode(certificate));
            auth.client_key_data = Some(SecretString::from(base64.encode(key.expose_secret())));
            auth.client_certificate = None;
            auth.client_key = None;
            None
        }
        Credential::Token {
            token,
            expires: None,
        } => {
            auth.token = Some(token);
            None
        }
        Credential::Token {
            token,
            expires: Some(expires),
        } => Some(AsyncFilterLayer::new(RefreshingToken(Arc::new(
            tokio::sync::Mutex::new(TokenState {
                exec,
                token,
                expires,
                refreshing: None,
                failed: None,
                resting: None,
            }),
        )))),
        Credential::None => None,
    })
}

/// Runs `exec` once and reads its `ExecCredential`. Dropping the future kills the plugin's
/// process group.
pub(crate) async fn run(exec: &ExecConfig) -> Result<Credential, AuthError> {
    let program = exec.command.as_deref().ok_or(AuthError::MissingCommand)?;
    let mut cmd = tokio::process::Command::new(program);
    if let Some(args) = &exec.args {
        cmd.args(args);
    }
    if let Some(env) = &exec.env {
        cmd.envs(
            env.iter()
                .filter_map(|e| Some((e.get("name")?, e.get("value")?))),
        );
    }
    let interactive = exec.interactive_mode != Some(ExecInteractiveMode::Never);
    if interactive {
        cmd.stdin(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        cmd.stdin(Stdio::piped()).stderr(Stdio::piped());
    }
    cmd.stdout(Stdio::piped());
    let cluster = if exec.provide_cluster_info {
        Some(
            exec.cluster
                .as_ref()
                .ok_or(AuthError::ExecMissingClusterInfo)?,
        )
    } else {
        None
    };
    let mut spec = serde_json::Map::new();
    spec.insert("interactive".into(), interactive.into());
    if let Some(cluster) = cluster {
        spec.insert(
            "cluster".into(),
            serde_json::to_value(cluster).map_err(AuthError::AuthExecSerialize)?,
        );
    }
    let info = serde_json::json!({
        "kind": "ExecCredential",
        "apiVersion": exec.api_version,
        "spec": spec,
    });
    cmd.env("KUBERNETES_EXEC_INFO", info.to_string());
    for name in exec.drop_env.iter().flatten() {
        cmd.env_remove(name);
    }
    // A group of its own, so a kill also ends what the plugin started (`gcloud` runs Python, a
    // shell wrapper runs the real binary).
    cmd.process_group(0);
    // Taken before the plugin starts, so a second prompting plugin waits for the first to give
    // the terminal back instead of starting in the background, where reading it stops it.
    let terminal = if interactive {
        Terminal::acquire().await
    } else {
        None
    };
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().map_err(AuthError::AuthExecStart)?;
    // Closed at once, as `Command::output()` does.
    drop(child.stdin.take());
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let pid = child.id().and_then(|id| libc::pid_t::try_from(id).ok());
    let mut running = Running {
        child,
        pid,
        group: pid,
        terminal,
    };
    // Only once it started: a plugin that cannot be started never had the terminal.
    running.hand_over_terminal();

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let drained = {
        let pipes = async {
            tokio::try_join!(
                read_capped(stdout, &mut out, STDOUT_CAP, true),
                read_capped(stderr, &mut err, STDERR_CAP, false),
            )
        };
        tokio::pin!(pipes);
        tokio::select! {
            read = &mut pipes => Some(read),
            () = exited(pid) => tokio::time::timeout(DRAIN_GRACE, &mut pipes).await.ok(),
        }
    };
    let (too_large, held_open) = match drained {
        Some(Ok(_)) => (false, false),
        Some(Err(e)) if e.kind() == std::io::ErrorKind::FileTooLarge => (true, false),
        Some(Err(e)) => return Err(AuthError::AuthExecStart(e)),
        // Held open by something the plugin started: what it printed is all it will print.
        None => (false, true),
    };
    if too_large || held_open {
        // The plugin is not reaped yet, so the group id is still its own.
        running.kill_group();
    }
    let status = running
        .child
        .wait()
        .await
        .map_err(AuthError::AuthExecStart)?;
    // Reaped: the group id may name another group from now on. (The terminal is taken back only
    // from a foreground group with the plugin's id: one in Flint's session, so not reused yet.)
    running.group = None;
    running.give_back_terminal();
    if too_large {
        return Err(AuthError::AuthExec(format!(
            "{TOO_LARGE}{}",
            redact::command_name(program)
        )));
    }
    let out = std::process::Output {
        status,
        stdout: out,
        stderr: err,
    };
    if !out.status.success() {
        // kube puts the command's `Debug` form here, environment included; only the name is kept.
        return Err(AuthError::AuthExecRun {
            cmd: redact::command_name(program),
            out: Box::new(out),
        });
    }
    parse(&out.stdout)
}

/// Reads `pipe` to its end into `buf`, keeping at most `cap` bytes. Past them, with `fail` it
/// fails with `FileTooLarge` (stdout: no credential is that long); without, it reads on and drops
/// the rest (stderr: chatter does not cost the credential).
async fn read_capped(
    pipe: Option<impl AsyncRead + Unpin>,
    buf: &mut Vec<u8>,
    cap: usize,
    fail: bool,
) -> std::io::Result<()> {
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut chunk = [0; 8192];
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        if buf.len() + n > cap {
            if fail {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            let room = cap - buf.len();
            buf.extend_from_slice(&chunk[..room]);
            continue;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Resolves once process `pid` has exited, without reaping it: its group id stays its own.
async fn exited(pid: Option<libc::pid_t>) {
    let Some(pid) = pid else {
        return std::future::pending().await;
    };
    loop {
        if has_exited(pid) {
            return;
        }
        tokio::time::sleep(EXIT_POLL).await;
    }
}

/// Whether child `pid` has exited, leaving it unreaped (`WNOWAIT`).
fn has_exited(pid: libc::pid_t) -> bool {
    // SAFETY: an all-zero siginfo_t is valid; waitid only writes into it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let id = libc::id_t::try_from(pid).unwrap_or_default();
    // SAFETY: `info` is valid for writes; WNOHANG makes the call return at once.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    #[cfg(target_os = "linux")]
    // SAFETY: waitid filled `info` for a child event (or left it zeroed).
    let found = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let found = info.si_pid;
    // A failure (no such child) means there is nothing left to wait for.
    rc != 0 || found == pid
}

/// Who has Flint's terminal: one prompting plugin at a time.
static TERMINAL: std::sync::LazyLock<Arc<tokio::sync::Mutex<()>>> =
    std::sync::LazyLock::new(Arc::default);

/// The terminal Flint runs in the foreground of, held for one prompting plugin: it is given the
/// terminal while it runs. Released when dropped.
struct Terminal {
    fd: OwnedFd,
    _owner: tokio::sync::OwnedMutexGuard<()>,
}

impl Terminal {
    /// Flint's controlling terminal, once no other plugin has it, if Flint's group is its
    /// foreground group (otherwise no one Flint starts may read it anyway).
    async fn acquire() -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let fd = OwnedFd::from(file);
        let owner = TERMINAL.clone().lock_owned().await;
        // SAFETY: plain calls on a descriptor this function owns.
        let ours = unsafe { libc::tcgetpgrp(fd.as_raw_fd()) == libc::getpgrp() };
        ours.then_some(Terminal { fd, _owner: owner })
    }
}

/// Makes `group` the foreground group of the terminal `fd`, with SIGTTOU blocked in this thread
/// meanwhile (a background group may not otherwise do it).
fn set_foreground(fd: RawFd, group: libc::pid_t) {
    // SAFETY: plain calls on a valid descriptor and on signal sets this function owns.
    unsafe {
        let mut block: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut block);
        libc::sigaddset(&raw mut block, libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const block, &raw mut old);
        libc::tcsetpgrp(fd, group);
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
    }
}

/// A running plugin. Dropped before it was reaped (`group` still set), it kills the plugin's
/// process group, takes the terminal back, then kills the plugin itself (`kill_on_drop`), which
/// tokio reaps.
struct Running {
    child: tokio::process::Child,
    /// The plugin's pid, which is its group's id.
    pid: Option<libc::pid_t>,
    /// The plugin's group, while the plugin is not reaped.
    group: Option<libc::pid_t>,
    /// The terminal the plugin was given, to take back.
    terminal: Option<Terminal>,
}

impl Running {
    /// Gives the plugin's group the terminal, and continues it in case it read the terminal
    /// before and was stopped for it.
    fn hand_over_terminal(&self) {
        if let (Some(terminal), Some(pgid)) = (&self.terminal, self.group) {
            set_foreground(terminal.fd.as_raw_fd(), pgid);
            // SAFETY: killpg takes plain integers; the plugin is not reaped.
            unsafe { libc::killpg(pgid, libc::SIGCONT) };
        }
    }

    fn kill_group(&self) {
        if let Some(pgid) = self.group {
            // SAFETY: killpg takes plain integers. `group` is cleared when the plugin is reaped,
            // so until then the id is its group's.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }

    /// Makes Flint's group the terminal's foreground group again, if the plugin's still is.
    fn give_back_terminal(&mut self) {
        if let (Some(terminal), Some(pid)) = (self.terminal.take(), self.pid) {
            give_back(&terminal, pid);
        }
    }
}

/// Takes `terminal` back for Flint's group if `plugin`'s group is its foreground group.
fn give_back(terminal: &Terminal, plugin: libc::pid_t) {
    let fd = terminal.fd.as_raw_fd();
    // SAFETY: a plain call on a descriptor `terminal` owns.
    if unsafe { libc::tcgetpgrp(fd) } == plugin {
        // SAFETY: as above.
        set_foreground(fd, unsafe { libc::getpgrp() });
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.kill_group();
        self.give_back_terminal();
    }
}

#[derive(Deserialize)]
struct ExecCredential {
    status: Option<ExecCredentialStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecCredentialStatus {
    expiration_timestamp: Option<String>,
    token: Option<String>,
    client_certificate_data: Option<String>,
    client_key_data: Option<String>,
}

/// Reads a plugin's output. client-go decodes it with a YAML-tolerant codec, so some plugins
/// print YAML; YAML 1.2 includes JSON.
fn parse(stdout: &[u8]) -> Result<Credential, AuthError> {
    let creds: ExecCredential =
        serde_saphyr::from_slice(stdout).map_err(|e| AuthError::AuthExecParse(Box::new(e)))?;
    let status = creds.status.ok_or(AuthError::ExecPluginFailed)?;
    let expires = status
        .expiration_timestamp
        .map(|t| t.parse::<Timestamp>())
        .transpose()
        .map_err(AuthError::MalformedTokenExpirationDate)?;
    Ok(
        match (
            status.client_certificate_data,
            status.client_key_data,
            status.token,
        ) {
            (Some(certificate), Some(key), _) => Credential::Certificate {
                certificate,
                key: SecretString::from(key),
            },
            (_, _, Some(token)) => Credential::Token {
                token: SecretString::from(token),
                expires,
            },
            _ => Credential::None,
        },
    )
}

/// How an [`AuthError::AuthExec`] of Flint's own reads, or `None` for kube's (whose text may
/// quote the plugin's output, so it is not shown). Names only the plugin's program.
pub(crate) fn describe(text: &str) -> Option<String> {
    if let Some(name) = text.strip_prefix(TIMED_OUT) {
        return Some(format!(
            "authentication failed: the exec auth plugin `{name}` did not finish within {}; it \
             may be waiting for a login. Run it in a terminal (for example, log in again) and \
             retry.",
            crate::cluster::go_duration(CONNECT_TIMEOUT)
        ));
    }
    if let Some((until, name)) = text
        .strip_prefix(RESTING)
        .and_then(|rest| rest.split_once('|'))
    {
        return Some(format!(
            "authentication failed: the exec auth plugin `{name}` timed out, so it is not run \
             again until {until}; retry to try now"
        ));
    }
    let name = text.strip_prefix(TOO_LARGE)?;
    Some(format!(
        "authentication failed: the exec auth plugin `{name}` printed more output than a \
         credential takes"
    ))
}

/// Bumped by [`retry_now`]: a rest taken before the latest bump is over.
static RETRY_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Someone asked to connect now (selected or retried a cluster): plugins resting after a
/// timed-out refresh ([`PROMPT_BACK_OFF`]) may run again. Requests Flint makes by itself keep
/// waiting it out.
pub fn retry_now() {
    RETRY_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// The client stack's layer that sets the plugin's token on each request.
pub(crate) type TokenLayer = AsyncFilterLayer<RefreshingToken>;

/// A plugin token that expires, shared by every request of one client. The first request that
/// finds it within [`REFRESH_MARGIN`] of its expiry starts a refresh: the plugin runs again in a
/// task of its own, bounded by [`CONNECT_TIMEOUT`], and stores what it got. Requests wait for that
/// one refresh; one that gives up (its own deadline) stops waiting, and the refresh goes on for
/// the next. So one client runs at most one plugin at a time, and a slow login is not restarted
/// by every short request.
#[derive(Clone)]
pub(crate) struct RefreshingToken(Arc<tokio::sync::Mutex<TokenState>>);

struct TokenState {
    exec: ExecConfig,
    token: SecretString,
    expires: Timestamp,
    /// The refresh running, which requests wait for.
    refreshing: Option<Shared<BoxFuture<'static, ()>>>,
    /// Why the last refresh failed, for the requests that waited for it.
    failed: Option<Arc<AuthError>>,
    /// A prompting plugin whose refresh timed out rests until then (as an instant and as wall
    /// time, to say), unless [`retry_now`] was called since (the epoch it was taken at).
    resting: Option<(Instant, Timestamp, u64)>,
}

impl TokenState {
    fn fresh(&self) -> bool {
        Timestamp::now() + REFRESH_MARGIN < self.expires
    }

    fn name(&self) -> String {
        redact::command_name(self.exec.command.as_deref().unwrap_or_default())
    }

    /// Until when a timed-out prompting plugin rests, if it still does.
    fn rests_until(&self) -> Option<Timestamp> {
        let (until, wall, epoch) = self.resting?;
        (Instant::now() < until && epoch == RETRY_EPOCH.load(Ordering::Relaxed)).then_some(wall)
    }
}

/// An auth error shared by the requests that waited for one refresh. It reads as the error and
/// has it as its source, so it is found and worded like kube's own.
#[derive(Debug)]
struct SharedAuthError(Arc<AuthError>);

impl std::fmt::Display for SharedAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SharedAuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

impl RefreshingToken {
    async fn header(&self) -> Result<HeaderValue, BoxError> {
        let refresh = {
            let mut state = self.0.lock().await;
            if state.fresh() {
                return bearer(&state.token);
            }
            if let Some(until) = state.rests_until() {
                return Err(resting(&state.name(), until).into());
            }
            match &state.refreshing {
                Some(refresh) => refresh.clone(),
                None => {
                    let refresh = self.start_refresh(state.exec.clone());
                    state.failed = None;
                    state.refreshing = Some(refresh.clone());
                    refresh
                }
            }
        };
        refresh.await;
        let state = self.0.lock().await;
        if state.fresh() {
            return bearer(&state.token);
        }
        match &state.failed {
            Some(error) => Err(SharedAuthError(error.clone()).into()),
            None => Err(AuthError::UnrefreshableTokenResponse.into()),
        }
    }

    /// Runs the plugin again in a task of its own, which stores the token (or why it failed)
    /// when it ends. The returned future resolves then.
    fn start_refresh(&self, exec: ExecConfig) -> Shared<BoxFuture<'static, ()>> {
        let state = self.0.clone();
        let task = tokio::spawn(async move {
            let result = tokio::time::timeout(CONNECT_TIMEOUT, run(&exec)).await;
            let mut state = state.lock().await;
            state.refreshing = None;
            let failed = match result {
                Ok(Ok(Credential::Token {
                    token,
                    expires: Some(expires),
                })) => {
                    state.token = token;
                    state.expires = expires;
                    state.resting = None;
                    None
                }
                // kube's answer when a refresh no longer returns an expiring token.
                Ok(Ok(_)) => Some(AuthError::UnrefreshableTokenResponse),
                Ok(Err(e)) => Some(e),
                Err(_) => {
                    if exec.interactive_mode != Some(ExecInteractiveMode::Never) {
                        state.resting = Some((
                            Instant::now() + PROMPT_BACK_OFF,
                            Timestamp::now() + PROMPT_BACK_OFF,
                            RETRY_EPOCH.load(Ordering::Relaxed),
                        ));
                    }
                    Some(AuthError::AuthExec(format!("{TIMED_OUT}{}", state.name())))
                }
            };
            state.failed = failed.map(Arc::new);
        });
        async move {
            let _ = task.await;
        }
        .boxed()
        .shared()
    }
}

/// The error for a request while the plugin rests until `until`: it was not run.
fn resting(name: &str, until: Timestamp) -> AuthError {
    let until = until
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%H:%M")
        .to_string();
    AuthError::AuthExec(format!("{RESTING}{until}|{name}"))
}

fn bearer(token: &SecretString) -> Result<HeaderValue, BoxError> {
    let mut value = HeaderValue::try_from(format!("Bearer {}", token.expose_secret()))
        .map_err(AuthError::InvalidBearerToken)?;
    value.set_sensitive(true);
    Ok(value)
}

impl<B: Send + 'static> AsyncPredicate<Request<B>> for RefreshingToken {
    type Future = BoxFuture<'static, Result<Request<B>, BoxError>>;
    type Request = Request<B>;

    fn check(&mut self, mut request: Request<B>) -> Self::Future {
        let token = self.clone();
        Box::pin(async move {
            let value = token.header().await?;
            request.headers_mut().insert(AUTHORIZATION, value);
            Ok(request)
        })
    }
}

#[cfg(test)]
#[path = "auth_plugin_tests.rs"]
pub(crate) mod tests;
