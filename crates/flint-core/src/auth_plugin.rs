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

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::process::Stdio;
use std::sync::Arc;
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

use crate::cluster::{CONNECT_TIMEOUT, PluginProblem};
use crate::connection::policy::{CredentialGrant, CredentialLease};
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
/// How often a plugin that must prompt checks whether Flint came to the foreground.
const FOREGROUND_POLL: Duration = Duration::from_millis(100);
/// How long a prompting plugin waits for another one to give the terminal back.
pub(crate) const TERMINAL_WAIT: Duration = Duration::from_millis(1500);
/// After a refresh of a plugin that may prompt timed out, refreshes fail at once for this long,
/// so a browser login nobody completes does not open again every minute.
const PROMPT_BACK_OFF: Duration = Duration::from_secs(5 * 60);

/// How [`AuthError::AuthExec`] texts of Flint's own start, so [`problem`] can read them: kube's
/// texts there may quote the plugin's output, Flint's name only the plugin's program.
const TIMED_OUT: &str = "flint:timed-out:";
const TOO_LARGE: &str = "flint:too-large:";
const RESTING: &str = "flint:resting:";
const STOPPED: &str = "flint:stopped:";
const NOT_ALLOWED: &str = "flint:not-allowed";

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

/// What [`resolve_with_grant`] left for the client.
#[derive(Default)]
pub(crate) struct Resolved {
    /// A token with an expiry, for the client stack ([`TokenLayer`]), which refreshes it.
    pub(crate) token: Option<RefreshingToken>,
    /// The plugin's certificate and key replaced any the kubeconfig names.
    pub(crate) replaced_certificate: bool,
}

/// Runs the exec plugin of `config`'s user, if kube would, and puts its credential in `config`
/// in place of the plugin. Cancel-safe: dropping the future kills the plugin. So does `grant`
/// being revoked, and a plugin it does not allow is not started.
pub(crate) async fn resolve_with_grant(
    config: &mut kube::Config,
    context: &str,
    grant: &CredentialGrant,
) -> Result<Resolved, AuthError> {
    let auth = &config.auth_info;
    // kube's precedence (`Auth::try_from`): all of these win over an exec plugin.
    let other = auth.auth_provider.is_some()
        || (auth.username.is_some() && auth.password.is_some())
        || auth.token.is_some()
        || auth.token_file.is_some();
    if other || auth.exec.is_none() {
        return Ok(Resolved::default());
    }
    let Some(exec) = config.auth_info.exec.take() else {
        return Ok(Resolved::default());
    };
    let auth = &mut config.auth_info;
    let lease = grant.lease();
    let terminal = Terminal::acquire(exec.interactive_mode == Some(ExecInteractiveMode::Always));
    let credential = tokio::select! {
        biased;
        () = lease.revoked() => return Err(not_allowed()),
        credential = run(&exec, &lease, terminal, || {}) => credential?,
    };
    if !lease.holds() {
        return Err(not_allowed());
    }
    Ok(match credential {
        Credential::Certificate { certificate, key } => {
            // kube reads these as base64; the plugin's certificate replaces any in kubeconfig.
            let base64 = base64::engine::general_purpose::STANDARD;
            auth.client_certificate_data = Some(base64.encode(certificate));
            auth.client_key_data = Some(SecretString::from(base64.encode(key.expose_secret())));
            auth.client_certificate = None;
            auth.client_key = None;
            Resolved {
                token: None,
                replaced_certificate: true,
            }
        }
        Credential::Token {
            token,
            expires: None,
        } => {
            auth.token = Some(token);
            Resolved::default()
        }
        Credential::Token {
            token,
            expires: Some(expires),
        } => Resolved {
            token: Some(RefreshingToken(Arc::new(TokenShared {
                grant: grant.clone(),
                task: std::sync::Mutex::default(),
                state: Arc::new(tokio::sync::Mutex::new(TokenState {
                    context: context.to_owned(),
                    exec,
                    token,
                    expires,
                    refreshing: None,
                    resting: None,
                })),
            }))),
            replaced_certificate: false,
        },
        Credential::None => Resolved::default(),
    })
}

/// Runs `exec` once and reads its `ExecCredential`, unless `lease` has ended by the moment it
/// would start. Dropping the future kills the plugin's process group.
async fn run(
    exec: &ExecConfig,
    lease: &CredentialLease,
    terminal: impl Future<Output = Acquired>,
    on_start: impl FnOnce(),
) -> Result<Credential, AuthError> {
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
    let mut interactive = exec.interactive_mode != Some(ExecInteractiveMode::Never);
    // Taken before the plugin starts, so a second prompting plugin waits for the first to give
    // the terminal back instead of starting in the background, where reading it stops it. With
    // `IfAvailable` it waits [`TERMINAL_WAIT`] at most: then it runs without prompting (it may
    // still have what it needs cached), rather than spend its whole timeout behind another
    // plugin's login. `Always` must prompt, so it waits as long as its caller does.
    let terminal = if interactive {
        match terminal.await {
            Acquired::Terminal(terminal) => Some(*terminal),
            // It could not prompt now: it runs without (and may have what it needs cached).
            Acquired::Busy | Acquired::Background => {
                interactive = false;
                None
            }
            Acquired::None => None,
        }
    } else {
        None
    };
    // A plugin that may not prompt gets a session of its own, without a controlling terminal:
    // opening `/dev/tty` fails, so it cannot write into another plugin's prompt or stop on
    // reading the terminal. Otherwise a group of its own. Either way a kill also ends what the
    // plugin started (`gcloud` runs Python, a shell wrapper runs the real binary).
    if interactive {
        cmd.process_group(0);
    } else {
        // SAFETY: the closure runs in the child between fork and exec and only calls setsid,
        // which is async-signal-safe. The child is no group leader yet, so setsid succeeds.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
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
    cmd.kill_on_drop(true);
    // Its own error and not `not_allowed`, which a waiting request answers by trying again while
    // the role allows it.
    if let Some(refusal) = lease.launch_refusal().await {
        return Err(AuthError::AuthExec(refusal));
    }
    let mut child = lease
        .start_if_held(|| cmd.spawn())
        .ok_or_else(not_allowed)?
        .map_err(AuthError::AuthExecStart)?;
    on_start();
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
            redact::command_name(program).unwrap_or_default()
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
            cmd: redact::command_name(program).unwrap_or_default(),
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
    /// The terminal's settings when the plugin got it: put back when Flint takes it back, so a
    /// plugin killed in a password prompt does not leave echo off or the terminal raw.
    saved: Option<libc::termios>,
    /// How SIGTTOU was handled before the plugin got the terminal. Meanwhile Flint is in the
    /// background of it, where with `stty tostop` a write to it (a log line) would stop the
    /// whole app; SIGTTOU is ignored then, which lets the write through.
    ttou: Option<libc::sigaction>,
}

/// What [`Terminal::acquire`] got.
enum Acquired {
    // Boxed: `Terminal` holds termios and sigaction, which are large on Linux.
    Terminal(Box<Terminal>),
    /// Another plugin has had the terminal for [`TERMINAL_WAIT`]: this one may not prompt.
    Busy,
    /// Flint runs in the background of its terminal (a shell job): a plugin reading it would
    /// stop, and `fg` would not continue it. An `IfAvailable` plugin may not prompt then.
    Background,
    /// No terminal at all (started from Finder or the Dock): as before, the plugin inherits
    /// Flint's stdin.
    None,
}

impl Terminal {
    /// Flint's controlling terminal, once no other plugin has it and Flint is in its
    /// foreground. A plugin that must prompt (`must_prompt`, `interactiveMode: Always`) waits for
    /// both as long as its caller does, waiting for the foreground without holding the terminal
    /// (no other plugin is kept waiting meanwhile); another waits [`TERMINAL_WAIT`] for the
    /// terminal, and does not wait for Flint to come to the foreground. A terminal that hung up
    /// (`tcgetpgrp` fails) is no terminal.
    async fn acquire(must_prompt: bool) -> Acquired {
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
        else {
            return Acquired::None;
        };
        // SAFETY: a plain call.
        let ours = unsafe { libc::getpgrp() };
        let foreground = |fd| {
            // SAFETY: a plain call on a descriptor `acquire_on` owns.
            let group = unsafe { libc::tcgetpgrp(fd) };
            (group != -1).then_some(group == ours)
        };
        Self::acquire_on(file.into(), &TERMINAL, foreground, must_prompt).await
    }

    async fn acquire_on(
        fd: OwnedFd,
        queue: &Arc<tokio::sync::Mutex<()>>,
        foreground: impl Fn(RawFd) -> Option<bool>,
        must_prompt: bool,
    ) -> Acquired {
        loop {
            // Join the queue first: the running plugin may still have the foreground.
            let lock = queue.clone().lock_owned();
            let owner = if must_prompt {
                lock.await
            } else {
                match tokio::time::timeout(TERMINAL_WAIT, lock).await {
                    Ok(owner) => owner,
                    Err(_) => return Acquired::Busy,
                }
            };
            match foreground(fd.as_raw_fd()) {
                None => return Acquired::None,
                Some(true) => {
                    return Acquired::Terminal(Box::new(Terminal {
                        fd,
                        _owner: owner,
                        saved: None,
                        ttou: None,
                    }));
                }
                Some(false) if must_prompt => {
                    // An external job has the foreground. Let other plugins try while
                    // waiting for someone to bring Flint to the foreground (`fg`).
                    drop(owner);
                    tokio::time::sleep(FOREGROUND_POLL).await;
                }
                Some(false) => return Acquired::Background,
            }
        }
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
    fn hand_over_terminal(&mut self) {
        if let (Some(terminal), Some(pgid)) = (&mut self.terminal, self.group) {
            // SAFETY: an all-zero termios is valid; tcgetattr only writes into it.
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: a plain call on a descriptor `terminal` owns, writing into `saved`.
            if unsafe { libc::tcgetattr(terminal.fd.as_raw_fd(), &raw mut saved) } == 0 {
                terminal.saved = Some(saved);
            }
            // SAFETY: an all-zero sigaction is valid; sigaction writes the old one into it. The
            // plugin was started before, so it does not inherit the ignored disposition.
            unsafe {
                let mut ignore: libc::sigaction = std::mem::zeroed();
                ignore.sa_sigaction = libc::SIG_IGN;
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(libc::SIGTTOU, &raw const ignore, &raw mut old) == 0 {
                    terminal.ttou = Some(old);
                }
            }
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
        if let Some(saved) = &terminal.saved {
            // SAFETY: a plain call on a descriptor `terminal` owns; Flint is in the foreground.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, saved) };
        }
    }
    if let Some(old) = &terminal.ttou {
        // SAFETY: puts back the disposition sigaction gave; one plugin has the terminal at a time,
        // so no other hand-over changed it meanwhile.
        unsafe { libc::sigaction(libc::SIGTTOU, old, std::ptr::null_mut()) };
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

/// What an [`AuthError::AuthExec`] of Flint's own says: the plugin's program name and the
/// problem, or `None` for kube's (whose text may quote the plugin's output, so it is not shown).
pub(crate) fn problem(text: &str) -> Option<(Option<String>, PluginProblem)> {
    let named = |name: &str| (!name.is_empty()).then(|| name.to_owned());
    if let Some(name) = text.strip_prefix(TIMED_OUT) {
        return Some((named(name), PluginProblem::TimedOut));
    }
    if let Some((until, name)) = text
        .strip_prefix(RESTING)
        .and_then(|rest| rest.split_once('|'))
    {
        let until = until.to_owned();
        return Some((named(name), PluginProblem::Resting { until }));
    }
    if text == NOT_ALLOWED {
        return Some((None, PluginProblem::NotAllowed));
    }
    if let Some(name) = text.strip_prefix(STOPPED) {
        return Some((named(name), PluginProblem::Stopped));
    }
    let name = text.strip_prefix(TOO_LARGE)?;
    Some((named(name), PluginProblem::TooLarge))
}

/// The error for a credential its [`CredentialGrant`] does not allow to run now.
pub(crate) fn not_allowed() -> AuthError {
    AuthError::AuthExec(NOT_ALLOWED.to_owned())
}

fn is_not_allowed(error: &AuthError) -> bool {
    matches!(error, AuthError::AuthExec(text) if text == NOT_ALLOWED)
}

/// Per context, bumped by [`retry_now`]: a rest taken before the latest bump is over.
static RETRY_EPOCHS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, u64>>> =
    std::sync::LazyLock::new(Default::default);

pub(crate) fn retry_epoch(context: &str) -> u64 {
    let epochs = RETRY_EPOCHS.lock().unwrap_or_else(|e| e.into_inner());
    epochs.get(context).copied().unwrap_or_default()
}

/// Someone asked to connect to `context` now (selected or retried it): its plugin, if resting
/// after a timed-out refresh ([`PROMPT_BACK_OFF`]), may run again. Requests Flint makes by itself,
/// and other contexts, keep waiting it out.
pub fn retry_now(context: &str) {
    let mut epochs = RETRY_EPOCHS.lock().unwrap_or_else(|e| e.into_inner());
    *epochs.entry(context.to_owned()).or_default() += 1;
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
pub(crate) struct RefreshingToken(Arc<TokenShared>);

/// What every clone of one client's [`RefreshingToken`] shares.
struct TokenShared {
    state: Arc<tokio::sync::Mutex<TokenState>>,
    /// Whether the plugin may run: asked before a refresh starts, and ends one that is running.
    grant: CredentialGrant,
    /// The refresh task, aborted when the last clone goes (the client is dropped): its plugin is
    /// killed then, and the terminal given back, instead of running on for nobody.
    task: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl Drop for TokenShared {
    fn drop(&mut self) {
        let task = self.task.get_mut().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = task.take() {
            task.abort();
        }
    }
}

struct TokenState {
    /// The context the client is for: [`retry_now`] names it.
    context: String,
    exec: ExecConfig,
    token: SecretString,
    expires: Timestamp,
    /// The refresh running, which requests wait for: it resolves to the token it got or why it
    /// failed. Its task takes it down when it ends; one still here once done ended unexpectedly.
    refreshing: Option<Shared<BoxFuture<'static, Refreshed>>>,
    /// A prompting plugin whose refresh timed out rests until then (as an instant and as wall
    /// time, to say), unless [`retry_now`] was called since (the epoch it was taken at).
    resting: Option<(Instant, Timestamp, u64)>,
}

impl TokenState {
    fn fresh(&self) -> bool {
        Timestamp::now() + REFRESH_MARGIN < self.expires
    }

    fn name(&self) -> String {
        redact::command_name(self.exec.command.as_deref().unwrap_or_default()).unwrap_or_default()
    }

    /// Until when a timed-out prompting plugin rests, if it still does.
    fn rests_until(&self) -> Option<Timestamp> {
        let (until, wall, epoch) = self.resting?;
        (Instant::now() < until && epoch == retry_epoch(&self.context)).then_some(wall)
    }
}

/// What a refresh got, for every request that waited for it.
type Refreshed = Result<SecretString, Arc<AuthError>>;

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
    /// The `Authorization` value to send now, refreshing the token first if it is due.
    pub(crate) async fn header(&self) -> Result<HeaderValue, BoxError> {
        loop {
            let refresh = {
                let mut state = self.0.state.lock().await;
                // The margin decides whether to refresh; a refresh's own result is used as it is
                // (as kube does), even a token that expires within it.
                if state.fresh() {
                    return bearer(&state.token);
                }
                if !self.0.grant.allowed() {
                    return Err(not_allowed().into());
                }
                if let Some(until) = state.rests_until() {
                    return Err(resting(&state.name(), until).into());
                }
                // Done, yet not taken down by its task: the task ended unexpectedly.
                if state
                    .refreshing
                    .as_ref()
                    .is_some_and(|r| r.peek().is_some())
                {
                    state.refreshing = None;
                }
                match &state.refreshing {
                    Some(refresh) => refresh.clone(),
                    None => {
                        let terminal = Terminal::acquire(
                            state.exec.interactive_mode == Some(ExecInteractiveMode::Always),
                        );
                        let refresh =
                            self.start_refresh(state.exec.clone(), state.context.clone(), terminal);
                        state.refreshing = Some(refresh.clone());
                        refresh
                    }
                }
            };
            return match refresh.await {
                Ok(token) => bearer(&token),
                // That refresh was ended by a demotion, and the role has allowed it again since:
                // its task has cleared it, so this request starts or joins a new one.
                Err(error) if is_not_allowed(&error) && self.0.grant.allowed() => continue,
                Err(error) => Err(SharedAuthError(error).into()),
            };
        }
    }

    /// Runs the plugin again in a task of its own, which stores the token when it gets one. The
    /// returned future resolves to what it got, or to [`STOPPED`] if the task ended otherwise.
    /// If the grant is revoked the plugin is killed, and what it printed by then is not kept.
    fn start_refresh(
        &self,
        exec: ExecConfig,
        context: String,
        terminal: impl Future<Output = Acquired> + Send + 'static,
    ) -> Shared<BoxFuture<'static, Refreshed>> {
        let state = self.0.state.clone();
        // Taken here, not in the task: a demotion before its first poll still ends it.
        let lease = self.0.grant.lease();
        let name =
            redact::command_name(exec.command.as_deref().unwrap_or_default()).unwrap_or_default();
        let stopped = AuthError::AuthExec(format!("{STOPPED}{name}"));
        let task = tokio::spawn(async move {
            let mut started = false;
            let result = tokio::select! {
                biased;
                () = lease.revoked() => None,
                result = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    run(&exec, &lease, terminal, || started = true),
                ) => Some(result),
            };
            let mut state = state.lock().await;
            state.refreshing = None;
            // Asked again with the lock held: a demotion may have come as the plugin finished.
            let Some(result) = result.filter(|_| lease.holds()) else {
                return Err(Arc::new(not_allowed()));
            };
            match result {
                Ok(Ok(Credential::Token {
                    token,
                    expires: Some(expires),
                })) => {
                    state.token = token.clone();
                    state.expires = expires;
                    state.resting = None;
                    Ok(token)
                }
                // kube's answer when a refresh no longer returns an expiring token.
                Ok(Ok(_)) => Err(Arc::new(AuthError::UnrefreshableTokenResponse)),
                Ok(Err(e)) => Err(Arc::new(e)),
                Err(_) => {
                    if started && exec.interactive_mode != Some(ExecInteractiveMode::Never) {
                        state.resting = Some((
                            Instant::now() + PROMPT_BACK_OFF,
                            Timestamp::now() + PROMPT_BACK_OFF,
                            retry_epoch(&context),
                        ));
                    }
                    Err(Arc::new(AuthError::AuthExec(format!(
                        "{TIMED_OUT}{}",
                        state.name()
                    ))))
                }
            }
        });
        let mut slot = self.0.task.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(task.abort_handle());
        async move { task.await.unwrap_or_else(|_| Err(Arc::new(stopped))) }
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
