//! Connecting to a cluster (one kubeconfig context) and checking it is alive.

use std::error::Error as StdError;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use kube::Client;
use kube::config::{KubeConfigOptions, Kubeconfig};

use crate::api_error::{self, ApiError, Transport};
use crate::connection::policy::{CredentialAccess, CredentialGrant};
use crate::connection::proxy;
use crate::credential::{self, CredentialSource, WriteCredential};
use crate::protection::{ClusterClients, WriteAccess};
use crate::redact;

/// How long a connection attempt may take, exec auth plugin included. Some plugins (SSO logins)
/// are slow, so this is generous.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// TCP keepalive for API server connections, as client-go sets it (Go's dialer `KeepAlive`):
/// the first probe after this much idle time, then one every interval, [`KEEPALIVE_PROBES`]
/// times. Without it a connection that died silently (the laptop slept, the network or VPN
/// changed) is never noticed, and followed logs, watches and shells wait on it forever. A read
/// timeout would end healthy long-lived streams that are just quiet, so there is none.
pub const KEEPALIVE: Duration = Duration::from_secs(30);

/// Unanswered keepalive probes before the connection is given up: Go's default count.
pub const KEEPALIVE_PROBES: u32 = 9;

/// A cluster's identity in Flint: its kubeconfig context name.
pub type ClusterId = String;

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("context \"{0}\" is not in the loaded kubeconfig files")]
    UnknownContext(String),
    #[error("invalid kubeconfig for context \"{context}\": {message}")]
    Config { context: String, message: String },
    /// Text from kube or below it (an error chain, a client error), which can quote anything.
    #[error("{0}")]
    Kube(String),
    /// A `Status` the API server sent that explains itself (`"NotFound: pods \"x\" not
    /// found"`, RBAC's `"forbidden: User \"jane\" cannot …"`), or, when launching an exec, a
    /// known kubelet refusal sent as a plain-text 500 (`container not found ("job")`). The
    /// names it quotes are what the user
    /// needs to act on, so they are shown, on one line.
    #[error("{0}")]
    Api(String),
    /// A request or the whole attempt took too long. `after` is the limit, when known (a
    /// connection timeout inside kube does not say). Build it with [`ClusterError::timeout`] or
    /// [`ClusterError::kube`]; [`ClusterError::for_target`] names the server and the plugin.
    /// Both are already safe to show ([`Target`]).
    #[error("{}", timeout_message(*after, server.as_deref(), *has_exec_plugin, exec_command.as_deref()))]
    Timeout {
        after: Option<Duration>,
        server: Option<String>,
        has_exec_plugin: bool,
        exec_command: Option<String>,
    },
    /// The exec auth plugin's program is not on `PATH`. `command` is its file name only
    /// ([`redact::command_name`]), filled in by [`ClusterError::for_target`].
    #[error("{}", exec_not_found_message(command.as_deref()))]
    ExecPluginNotFound { command: Option<String> },
    /// An exec auth plugin Flint ran gave no credential for a reason Flint words itself
    /// ([`PluginProblem`]). `command` is its file name only ([`redact::command_name`]), so the
    /// app can say which plugin to run again.
    #[error("{}", plugin_problem_message(command.as_deref(), problem))]
    ExecPlugin {
        command: Option<String>,
        problem: PluginProblem,
    },
    /// The network failed below HTTP: refused, DNS, TLS, reset, dropped ([`Transport`]).
    /// `server` is filled in by [`ClusterError::for_target`].
    #[error("{}", problem.message(server.as_deref()))]
    Transport {
        problem: Transport,
        server: Option<String>,
    },
    /// The server answered with an HTTP error and no Kubernetes `Status` that explains it (a
    /// proxy's HTML page, an empty body), or with 401, whose `Status` only says "Unauthorized".
    /// `server` is filled in by [`ClusterError::for_target`].
    #[error("{}", api_error::status_meaning(*code, server.as_deref()))]
    Http { code: u16, server: Option<String> },
}

/// `timed out after 10s reaching https://host:6443 (including exec plugin `aws`)`.
fn timeout_message(
    after: Option<Duration>,
    server: Option<&str>,
    has_exec_plugin: bool,
    exec_command: Option<&str>,
) -> String {
    let mut out = "timed out".to_owned();
    if let Some(after) = after {
        out.push_str(&format!(" after {}", go_duration(after)));
    }
    match server {
        Some(server) => out.push_str(&format!(" reaching {server}")),
        None => out.push_str(" connecting"),
    }
    if let Some(exec) = exec_command {
        out.push_str(&format!(" (including exec plugin `{exec}`)"));
    } else if has_exec_plugin {
        out.push_str(" (including exec plugin)");
    }
    out
}

/// The one wording for a missing exec auth plugin, wherever it is reported.
fn exec_not_found_message(command: Option<&str>) -> String {
    let plugin = match command {
        Some(command) => format!("exec auth plugin `{command}`"),
        None => "exec auth plugin".to_owned(),
    };
    format!(
        "{plugin} not found on PATH ({}); install it, or set `path` in Flint's settings.toml",
        crate::env_path::process_path_origin()
    )
}

/// Why an exec auth plugin Flint ran (`auth_plugin`) gave no credential, other than failing
/// on its own (an exit status, unreadable output: kube's [`kube::client::AuthError`]s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginProblem {
    /// A token refresh did not finish within [`CONNECT_TIMEOUT`]: it may wait for a login.
    TimedOut,
    /// It timed out before and may prompt, so it is not run again until `until` (local
    /// `HH:MM`), unless the cluster is selected or retried.
    Resting { until: String },
    /// The token refresh's task ended unexpectedly.
    Stopped,
    /// It printed more than a credential takes, and was killed.
    TooLarge,
    /// The connection's credential policy does not let it run now, so it was not started, or
    /// was ended.
    NotAllowed,
}

fn plugin_problem_message(command: Option<&str>, problem: &PluginProblem) -> String {
    let plugin = match command {
        Some(command) => format!("the exec auth plugin `{command}`"),
        None => "the exec auth plugin".to_owned(),
    };
    match problem {
        PluginProblem::TimedOut => format!(
            "authentication failed: {plugin} did not finish within {}; it may be waiting for a \
             login. Run it in a terminal (for example, log in again) and retry.",
            go_duration(CONNECT_TIMEOUT)
        ),
        PluginProblem::Resting { until } => format!(
            "authentication failed: {plugin} timed out, so it is not run again until {until}; \
             retry to try now"
        ),
        PluginProblem::Stopped => format!(
            "authentication failed: refreshing the token of {plugin} stopped unexpectedly; retry"
        ),
        PluginProblem::TooLarge => {
            format!("authentication failed: {plugin} printed more output than a credential takes")
        }
        PluginProblem::NotAllowed => {
            "authentication failed: this cluster's credential is not run in the background; \
             select the cluster to sign in"
                .to_owned()
        }
    }
}

/// A duration as Go (and so kubectl) prints it: `500ms`, `10s`, `1.5s`, `1m0s`.
pub(crate) fn go_duration(d: Duration) -> String {
    let millis = d.as_millis();
    if millis < 1000 {
        return format!("{millis}ms");
    }
    let secs = d.as_secs();
    let frac = d.subsec_millis();
    let seconds = |s: u64| {
        if frac == 0 {
            format!("{s}s")
        } else {
            format!("{}s", format!("{s}.{frac:03}").trim_end_matches('0'))
        }
    };
    if secs < 60 {
        seconds(secs)
    } else {
        format!("{}m{}", secs / 60, seconds(secs % 60))
    }
}

impl ClusterError {
    /// API errors become `"<reason>: <message>"` (for example `"NotFound: pods \"x\" not found"`),
    /// or [`ClusterError::Http`] when the server's answer does not explain itself. Network
    /// failures become [`ClusterError::Timeout`] or [`ClusterError::Transport`]. Other errors
    /// keep their full source chain, except auth plugin errors (see [`error_chain`]).
    pub fn kube(err: kube::Error) -> Self {
        Self::kube_ref(&err)
    }

    /// [`ClusterError::kube`] without taking the error, for a caller that still reads it.
    pub(crate) fn kube_ref(err: &kube::Error) -> Self {
        if let Some(kube::client::AuthError::AuthExec(text)) = find_auth_error(err)
            && let Some((command, problem)) = crate::auth_plugin::problem(text)
        {
            return ClusterError::ExecPlugin { command, problem };
        }
        match ApiError::from_kube(err) {
            Some(api) => match api.explanation() {
                Some(message) => ClusterError::Api(message),
                None => ClusterError::Http {
                    code: api.code,
                    server: None,
                },
            },
            None if is_exec_not_found(err) => ClusterError::ExecPluginNotFound { command: None },
            // kube's own connection timeout: the server did not answer at all.
            None if is_transport_timeout(err) => ClusterError::Timeout {
                after: None,
                server: None,
                has_exec_plugin: false,
                exec_command: None,
            },
            // An auth provider's failure can hold a refused or reset connection of its own (a
            // token endpoint): it is not the API server's.
            None if find_auth_error(err).is_some() => ClusterError::Kube(error_chain(err)),
            None => match api_error::classify_transport(err) {
                Some(problem) => ClusterError::Transport {
                    problem,
                    server: None,
                },
                None => ClusterError::Kube(error_chain(err)),
            },
        }
    }

    /// An error reading a response that was already streaming (a watch, a log): a dropped
    /// connection in plain words, naming `target`'s server, or `"the <what> broke: <chain>"`.
    pub fn stream(what: &str, err: &(dyn StdError + 'static), target: &Target) -> Self {
        match api_error::classify_transport(err) {
            Some(problem) => ClusterError::Transport {
                problem,
                server: target.server.clone(),
            },
            None => ClusterError::Kube(format!("the {what} broke: {}", error_chain(err))),
        }
    }

    /// What the app shows and logs ([`SafeErrorText`]): Flint's own wording whole (the server
    /// URL and plugin name are already safe, [`Target`]); text from a server or kube with its
    /// values hidden whole ([`redact::hide_server_values`]) and only then cut to one line; and
    /// kubeconfig text reduced to one line with quoted values hidden
    /// ([`redact::safe_error_summary`]). For display only: classify on the raw error.
    pub fn summary(&self) -> SafeErrorText {
        SafeErrorText(match self {
            ClusterError::Kube(message) | ClusterError::Api(message) => {
                redact::first_line(&redact::hide_server_values(message), SHOWN_CHARS)
            }
            ClusterError::Config { context, message } => format!(
                "invalid kubeconfig for context \"{context}\": {}",
                redact::safe_error_summary(message)
            ),
            other => other.to_string(),
        })
    }

    /// Error text for MCP replies. Bounded messages keep quoted names and have credentials
    /// redacted before the first line is shortened.
    pub fn for_agent(&self) -> String {
        match self {
            ClusterError::Kube(message) | ClusterError::Api(message) => agent_error_text(message),
            ClusterError::Config { .. } => agent_error_text(&self.to_string()),
            other => other.to_string(),
        }
    }

    /// A timeout while reaching `server` (a kubeconfig `server` URL, shown through
    /// [`redact::server_url`]).
    pub fn timeout(after: Duration, server: Option<&str>) -> Self {
        ClusterError::Timeout {
            after: Some(after),
            server: server.map(redact::server_url),
            has_exec_plugin: false,
            exec_command: None,
        }
    }

    /// Whether the cluster could not be reached in time (the server did not answer).
    pub fn is_timeout(&self) -> bool {
        matches!(self, ClusterError::Timeout { .. })
    }

    /// Names what failed: the API server a request was reaching, the plugin that is missing.
    /// Only details missing from the error are filled in.
    pub fn for_target(self, target: &Target) -> Self {
        match self {
            ClusterError::Timeout {
                after,
                server,
                has_exec_plugin,
                exec_command,
            } => ClusterError::Timeout {
                after,
                server: server.or_else(|| target.server.clone()),
                has_exec_plugin: has_exec_plugin || target.has_exec_plugin,
                exec_command: exec_command.or_else(|| target.exec_command.clone()),
            },
            ClusterError::ExecPluginNotFound { command: None } => {
                ClusterError::ExecPluginNotFound {
                    command: target.exec_command.clone(),
                }
            }
            ClusterError::Transport { problem, server } => ClusterError::Transport {
                problem,
                server: server.or_else(|| target.server.clone()),
            },
            ClusterError::Http { code, server } => ClusterError::Http {
                code,
                server: server.or_else(|| target.server.clone()),
            },
            other => other,
        }
    }
}

/// How much of a server's or kube's text the app shows, and an agent reads.
const SHOWN_CHARS: usize = 200;
const AGENT_CHARS: usize = 300;
const AGENT_BYTES: usize = 4 * 1024;
/// Extra redaction passes over an error before it is cut.
const REDACT_PASSES: usize = 3;
/// How far the cut may move back so that the `…` is not taken for a value.
const CUT_BACKOFF: usize = 64;

fn agent_error_text(message: &str) -> String {
    // A clipped credential can lose the suffix that identifies it, and redaction can bring
    // the exposed prefix into the visible line, so a longer message is not clipped and shown.
    if message.len() > AGENT_BYTES {
        return "Error details omitted because the message is too long".to_owned();
    }
    // A masked value can itself be recognised by a later pass (a header whose value was
    // masked), so the text is redacted until it is stable, before it is cut.
    let mut text = redact::redact_credentials(message).text.into_owned();
    for _ in 0..REDACT_PASSES {
        let next = redact::redact_credentials(&text).text.into_owned();
        if next == text {
            break;
        }
        text = next;
    }
    agent_line(&text)
}

/// The first line of redacted `text`, cut to [`AGENT_CHARS`] (with `…`). A `[REDACTED:<kind>]`
/// marker across the cut goes whole. The cut text must survive a second redaction unchanged
/// (a later pass can take the `…` after `Cookie: ` for a value), so the cut moves back until it
/// does, and the `…` stays.
fn agent_line(text: &str) -> String {
    let line = redact::first_line(text, usize::MAX);
    for limit in (AGENT_CHARS.saturating_sub(CUT_BACKOFF)..=AGENT_CHARS).rev() {
        let Some(shorter) = cut_line(&line, limit) else {
            return line;
        };
        if redact::redact_credentials(&shorter).text == shorter {
            return shorter;
        }
    }
    // Nothing near the cap is stable: drop the line down to its stable start.
    "…".to_owned()
}

/// `line` cut to `limit` chars with a trailing `…`, or `None` when it already fits.
fn cut_line(line: &str, limit: usize) -> Option<String> {
    let (mut cut, _) = line.char_indices().nth(limit)?;
    let last_marker = line
        .match_indices("[REDACTED:")
        .map(|(start, _)| start)
        .take_while(|start| *start < cut)
        .last();
    if let Some(start) = last_marker
        && line[start..]
            .find(']')
            .is_some_and(|end| start + end >= cut)
    {
        cut = start;
    }
    Some(format!("{}…", &line[..cut]))
}

/// Error text that is safe to show in the window, in logs and in the automation state. Only
/// flint-core builds it ([`ClusterError::summary`]), so nothing unsanitized can be rendered as
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeErrorText(String);

impl SafeErrorText {
    /// Text Flint wrote itself, with nothing from outside in it.
    #[cfg(test)]
    pub(crate) fn fixed(text: impl Into<String>) -> Self {
        SafeErrorText(text.into())
    }

    /// Any text, reduced as [`redact::safe_error_summary`] does: safe whatever its source, so
    /// public (the app's tests build failed states with it).
    pub fn reduced(text: &str) -> Self {
        SafeErrorText(redact::safe_error_summary(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SafeErrorText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a context connects to, as far as error messages may say: safe to show to the user, in
/// logs, and to an AI agent. Holds no credential.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// The API server URL, through [`redact::server_url`].
    pub server: Option<String>,
    /// Whether an exec auth plugin is configured, even if its command is hidden.
    pub has_exec_plugin: bool,
    /// The exec auth plugin's program name, through [`redact::command_name`].
    pub exec_command: Option<String>,
}

impl Target {
    /// The target of `context` in `kubeconfig`. Missing entries leave fields empty.
    pub fn of(kubeconfig: &Kubeconfig, context: &str) -> Self {
        let ctx = kubeconfig
            .contexts
            .iter()
            .find(|c| c.name == context)
            .and_then(|c| c.context.as_ref());
        let server = ctx
            .and_then(|c| kubeconfig.clusters.iter().find(|n| n.name == c.cluster))
            .and_then(|n| n.cluster.as_ref())
            .and_then(|c| c.server.as_deref())
            .map(redact::server_url);
        let exec = ctx
            .and_then(|c| c.user.as_ref())
            .and_then(|u| kubeconfig.auth_infos.iter().find(|n| &n.name == u))
            .and_then(|n| n.auth_info.as_ref())
            .and_then(|a| a.exec.as_ref());
        Target {
            server,
            has_exec_plugin: exec.is_some(),
            exec_command: exec
                .and_then(|e| e.command.as_deref())
                .and_then(redact::command_name),
        }
    }
}

/// The API server's `/version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVersion {
    pub git_version: String,
    pub platform: String,
}

/// Builds a client for `context` from the merged kubeconfig. Auth (certificates, tokens, exec
/// plugins) is handled by kube. Must be called inside a tokio runtime.
///
/// An exec auth plugin is run by Flint (`auth_plugin`), not by kube, so `connect` stays
/// cancellable: a timeout around it, or dropping it, kills a plugin that waits for a login that
/// never comes. The rest of the client setup (reading certificate files) runs on a blocking
/// thread.
pub async fn connect(kubeconfig: &Kubeconfig, context: &str) -> Result<Client, ClusterError> {
    let ready = prepare(kubeconfig, context, &CredentialAccess::always()).await?;
    let target = ready.target;
    let (config, token) = (ready.config, ready.token);
    tokio::task::spawn_blocking(move || {
        let auth = kube::client::ConfigExt::auth_layer(&config)?;
        build_client(&config, token, auth.as_ref(), None, Access::ReadOnly)
    })
    .await
    .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?
    .map_err(|e| ClusterError::kube(e).for_target(&target))
}

/// Asked before a client's credential is prepared, before its program starts, and around the
/// credential step of each attempt of each request, retries included. An `Err` stops the attempt
/// before the credential is resolved or sent.
#[derive(Clone)]
pub struct SendCheck(
    std::sync::Arc<
        dyn Fn() -> futures::future::BoxFuture<'static, Result<(), String>> + Send + Sync,
    >,
);

impl SendCheck {
    pub fn new(
        check: impl Fn() -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        SendCheck(std::sync::Arc::new(check))
    }

    pub(crate) async fn refusal(&self) -> Result<(), String> {
        (self.0)().await
    }

    async fn holds(&self) -> Result<(), ClusterError> {
        self.refusal().await.map_err(ClusterError::Kube)
    }
}

impl<B: Send + 'static> tower::filter::AsyncPredicate<http::Request<B>> for SendCheck {
    type Future = futures::future::BoxFuture<'static, Result<http::Request<B>, tower::BoxError>>;
    type Request = http::Request<B>;

    fn check(&mut self, request: http::Request<B>) -> Self::Future {
        let check = self.0.clone();
        Box::pin(async move {
            check().await.map_err(tower::BoxError::from)?;
            Ok(request)
        })
    }
}

/// As [`connect`], with `check` asked before the credential is prepared, right before its program
/// starts, before the client is built, and before and after the credential step of each attempt
/// of each request the client sends.
pub async fn connect_checked(
    kubeconfig: &Kubeconfig,
    context: &str,
    check: SendCheck,
) -> Result<Client, ClusterError> {
    check.holds().await?;
    let access = CredentialAccess::always_checked(check.clone());
    let ready = match prepare(kubeconfig, context, &access).await {
        Ok(ready) => ready,
        // A check that stopped holding is the reason, whatever failed because of it.
        Err(error) => {
            check.holds().await?;
            return Err(error);
        }
    };
    check.holds().await?;
    let target = ready.target;
    let (config, token) = (ready.config, ready.token);
    tokio::task::spawn_blocking(move || {
        let auth = kube::client::ConfigExt::auth_layer(&config)?;
        build_observed_client(
            &config,
            token,
            auth.as_ref(),
            None,
            Access::ReadOnly,
            None,
            Some(&check),
        )
    })
    .await
    .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?
    .map_err(|e| ClusterError::kube(e).for_target(&target))
}

/// A connection's clients, and the credential its write client presents.
pub(crate) struct GatedConnection {
    pub(crate) clients: ClusterClients,
    pub(crate) credential: WriteCredential,
}

/// As [`connect`], with the client that can write, closed behind the write gate. Its certificate
/// and key files are read once here, and both clients are built from those bytes. `access` says
/// whether its credential may run, now and for as long as the connection lives.
pub(crate) async fn connect_gated_with_policy(
    kubeconfig: &Kubeconfig,
    context: &str,
    access: &CredentialAccess,
) -> Result<GatedConnection, ClusterError> {
    route_preflight(kubeconfig, context)?;
    let mut held = kubeconfig.clone();
    let name = context.to_owned();
    let (held, files) = tokio::task::spawn_blocking(move || {
        let files = credential::hold_credential_files(&mut held, &name);
        (held, files)
    })
    .await
    .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?;
    let ready = prepare(&held, context, access).await?;
    let target = ready.target;
    let files = credential::files_in_use(files, ready.replaced_certificate);
    let (config, token, source, grant) = (ready.config, ready.token, ready.source, ready.grant);
    let lease = grant.lease();
    tokio::task::spawn_blocking(move || {
        // Each of these steps can run a credential command, which cannot be stopped once started:
        // the role is asked before each, and after the last.
        let still_held = || {
            if lease.holds() {
                Ok(())
            } else {
                Err(kube::Error::Auth(crate::auth_plugin::not_allowed()))
            }
        };
        still_held()?;
        // Once: both clients then share it.
        let auth = kube::client::ConfigExt::auth_layer(&config)?.map(std::sync::Arc::new);
        // Taken now, while the layer still holds what it just read from the file.
        let admitted_token = match (&source, &auth) {
            (credential::CredentialSource::TokenFile(_), Some(layer)) => {
                tokio::runtime::Handle::current().block_on(credential::client_token(layer))
            }
            _ => None,
        };
        let guarded = auth
            .as_ref()
            .and_then(|layer| credential::GuardedProvider::guarding(layer, &lease));
        still_held()?;
        let read = build_observed_client(
            &config,
            token.clone(),
            auth.as_deref(),
            guarded.as_ref(),
            Access::ReadOnly,
            Some(&grant),
            None,
        )?;
        still_held()?;
        let write = build_client(
            &config,
            token.clone(),
            auth.as_deref(),
            guarded.as_ref(),
            Access::Unrestricted,
        )?;
        still_held()?;
        Ok::<_, kube::Error>(GatedConnection {
            clients: ClusterClients {
                read,
                writes: WriteAccess::new(write),
            },
            credential: WriteCredential::with_grant(files, source, token, auth, grant, guarded)
                .with_admitted_token(admitted_token),
        })
    })
    .await
    .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?
    .map_err(|e| ClusterError::kube(e).for_target(&target))
}

/// A client with no read-only rule, for tests of the exec endpoints themselves. Flint's own
/// sessions get theirs from the write gate.
#[cfg(any(test, feature = "test-support"))]
pub async fn connect_unrestricted_for_tests(
    kubeconfig: &Kubeconfig,
    context: &str,
) -> Result<Client, ClusterError> {
    let ready = prepare(kubeconfig, context, &CredentialAccess::always()).await?;
    let target = ready.target;
    let (config, token) = (ready.config, ready.token);
    tokio::task::spawn_blocking(move || {
        let auth = kube::client::ConfigExt::auth_layer(&config)?;
        build_client(&config, token, auth.as_ref(), None, Access::Unrestricted)
    })
    .await
    .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?
    .map_err(|e| ClusterError::kube(e).for_target(&target))
}

/// What building a context's clients takes.
struct Prepared {
    config: kube::Config,
    token: Option<crate::auth_plugin::RefreshingToken>,
    /// Where the credential comes from, read before an exec plugin is replaced by its output.
    source: CredentialSource,
    /// Whether that credential may run, for the life of the connection.
    grant: CredentialGrant,
    /// An exec plugin's certificate and key replaced any the kubeconfig names.
    replaced_certificate: bool,
    target: Target,
}

/// Reads `context`'s connection settings and runs its exec auth plugin, if any and if `access`
/// allows it. A credential it does not allow is refused before anything could run it.
async fn prepare(
    kubeconfig: &Kubeconfig,
    context: &str,
    access: &CredentialAccess,
) -> Result<Prepared, ClusterError> {
    if !kubeconfig.contexts.iter().any(|c| c.name == context) {
        return Err(ClusterError::UnknownContext(context.to_owned()));
    }
    // kube's loader reads certificate and key files.
    route_preflight(kubeconfig, context)?;
    let options = KubeConfigOptions {
        context: Some(context.to_owned()),
        cluster: None,
        user: None,
    };
    let mut loadable = kubeconfig.clone();
    let entry = swap_proxy_entry(&mut loadable, context);
    let mut config = kube::Config::from_custom_kubeconfig(loadable, &options)
        .await
        .map_err(|e| ClusterError::Config {
            context: context.to_owned(),
            message: error_chain(&e),
        })?;
    // Before anything runs a credential, so a route Flint refuses opens no login.
    config.proxy_url = proxy_of(
        entry.as_deref(),
        &config.cluster_url.to_string(),
        config.accept_invalid_certs,
    )
    .map_err(|refused| ClusterError::Config {
        context: context.to_owned(),
        message: refused.to_string(),
    })?;
    // What a plugin is told of the cluster holds the proxy's address, never its credentials.
    if let Some(cluster) = config
        .auth_info
        .exec
        .as_mut()
        .and_then(|exec| exec.cluster.as_mut())
    {
        cluster.proxy_url = entry
            .as_deref()
            .filter(|e| !e.trim().is_empty())
            .map(proxy::shown);
    }
    let target = Target::of(kubeconfig, context);
    let source = CredentialSource::of(&config.auth_info);
    let grant = access.grant(source.clone());
    if !grant.allowed() {
        let denied = kube::Error::Auth(crate::auth_plugin::not_allowed());
        return Err(ClusterError::kube(denied).for_target(&target));
    }
    let resolved = crate::auth_plugin::resolve_with_grant(&mut config, context, &grant)
        .await
        .map_err(|e| ClusterError::kube(kube::Error::Auth(e)).for_target(&target))?;
    Ok(Prepared {
        config,
        token: resolved.token,
        source,
        grant,
        replaced_certificate: resolved.replaced_certificate,
        target,
    })
}

/// Refuses `context`'s route from the kubeconfig's own values, before any credential file is
/// opened. A context or server that is missing is left for kube's loader to report.
fn route_preflight(kubeconfig: &Kubeconfig, context: &str) -> Result<(), ClusterError> {
    let cluster = kubeconfig
        .contexts
        .iter()
        .find(|c| c.name == context)
        .and_then(|c| c.context.as_ref())
        .and_then(|c| kubeconfig.clusters.iter().find(|n| n.name == c.cluster))
        .and_then(|n| n.cluster.as_ref());
    let Some((cluster, server)) = cluster.and_then(|c| Some((c, c.server.as_deref()?))) else {
        return Ok(());
    };
    let skips_verify = cluster.insecure_skip_tls_verify == Some(true);
    proxy_of(cluster.proxy_url.as_deref(), server, skips_verify)
        .map(drop)
        .map_err(|refused| ClusterError::Config {
            context: context.to_owned(),
            message: refused.to_string(),
        })
}

/// Gives `context`'s cluster entry [`proxy::LOADER_PLACEHOLDER`] as its proxy, and returns the
/// `proxy-url` it had.
fn swap_proxy_entry(kubeconfig: &mut Kubeconfig, context: &str) -> Option<String> {
    let cluster = kubeconfig
        .contexts
        .iter()
        .find(|c| c.name == context)
        .and_then(|c| c.context.as_ref())
        .map(|c| c.cluster.clone())?;
    kubeconfig
        .clusters
        .iter_mut()
        .find(|n| n.name == cluster)?
        .cluster
        .as_mut()?
        .proxy_url
        .replace(proxy::LOADER_PLACEHOLDER.to_owned())
}

/// The proxy to reach `server` through, given the cluster entry's `proxy-url`, checked to be one
/// Flint can use on this route. kube's loader applies `NO_PROXY` to a `proxy-url` too, or not at
/// all without its proxy features, so the choice is made here ([`proxy::chosen`]).
fn proxy_of(
    entry: Option<&str>,
    server: &str,
    skips_verify: bool,
) -> Result<Option<http::Uri>, proxy::ProxyRefused> {
    let environment = proxy::ProxyEnvironment::current();
    let Some(chosen) = proxy::chosen(entry, Some(server), &environment) else {
        return Ok(None);
    };
    proxy::ProxyTarget::parse(chosen)?;
    proxy::check_route(server, skips_verify)?;
    proxy::check_account_key(chosen)?;
    chosen
        .trim()
        .parse()
        .map(Some)
        .map_err(|_| proxy::ProxyRefused::Unreadable)
}

/// What a client built by [`build_client`] may send.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    /// Reads only ([`crate::read_only`]): every client but the one behind the write gate.
    ReadOnly,
    Unrestricted,
}

/// kube's retry policy for `GET`s only. A write is sent once: each one asks the write gate first,
/// and a refused or conflicting one goes back to the user.
#[derive(Clone)]
struct ReadRetry(kube::client::retry::RetryPolicy);

/// A plain `GET`. A WebSocket upgrade is a `GET` too, but it opens a channel into the cluster, so
/// it is sent once.
fn retryable<B>(request: &http::Request<B>) -> bool {
    request.method() == http::Method::GET && !request.headers().contains_key(http::header::UPGRADE)
}

impl<Res>
    tower::retry::Policy<http::Request<kube::client::Body>, http::Response<Res>, tower::BoxError>
    for ReadRetry
{
    type Future = tokio::time::Sleep;

    fn retry(
        &mut self,
        request: &mut http::Request<kube::client::Body>,
        result: &mut Result<http::Response<Res>, tower::BoxError>,
    ) -> Option<Self::Future> {
        if !retryable(request) {
            return None;
        }
        tower::retry::Policy::retry(&mut self.0, request, result)
    }

    fn clone_request(
        &mut self,
        request: &http::Request<kube::client::Body>,
    ) -> Option<http::Request<kube::client::Body>> {
        if !retryable(request) {
            return None;
        }
        <kube::client::retry::RetryPolicy as tower::retry::Policy<
            http::Request<kube::client::Body>,
            http::Response<Res>,
            tower::BoxError,
        >>::clone_request(&mut self.0, request)
        .map(|mut copy| {
            // A retry goes out with the credential the request was checked with.
            if let Some(bound) = request.extensions().get::<credential::BoundHeader>() {
                copy.extensions_mut().insert(bound.clone());
            }
            copy
        })
    }
}

/// The TCP connector under every client: kube's (`enforce_http(false)`, so HTTPS is allowed),
/// with [`KEEPALIVE`].
fn http_connector() -> hyper_util::client::legacy::connect::HttpConnector {
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.enforce_http(false);
    connector.set_keepalive(Some(KEEPALIVE));
    connector.set_keepalive_interval(Some(KEEPALIVE));
    connector.set_keepalive_retries(Some(KEEPALIVE_PROBES));
    connector
}

/// `Client::try_from(config)` with TCP keepalive. kube 5.1 creates its `HttpConnector` inside
/// `ClientBuilder::try_from` and has no hook to configure it, so this builds the same stack
/// from kube's public parts (`ConfigExt`), as kube 5.1's `make_generic_builder` does for
/// Flint's features (rustls, no gzip): TLS, kube's connect, read and write timeouts, base URI,
/// retries, auth, and extra headers. Left out: kube's debug-level HTTP tracing spans, and the
/// client's `valid_until` (an exec plugin certificate's expiry, which kube only records and
/// Flint never reads). `config.proxy_url` was checked by [`prepare`]; TLS to the API server runs
/// inside its tunnel. `token` is the refreshing token of an exec auth plugin
/// ([`crate::auth_plugin::resolve_with_grant`]), set after kube's auth layer. `guarded`, if any,
/// stands in for `auth` on requests, because `auth` could run a command a role may forbid.
fn build_client(
    config: &kube::Config,
    token: Option<crate::auth_plugin::RefreshingToken>,
    auth: Option<&kube::client::middleware::AuthLayer>,
    guarded: Option<&credential::GuardedProvider>,
    access: Access,
) -> Result<Client, kube::Error> {
    build_observed_client(config, token, auth, guarded, access, None, None)
}

/// [`build_client`], telling `observer` of each 401 the server answers, and asking `check` before
/// each attempt: inside the retries, and before anything that resolves a credential.
fn build_observed_client(
    config: &kube::Config,
    token: Option<crate::auth_plugin::RefreshingToken>,
    auth: Option<&kube::client::middleware::AuthLayer>,
    guarded: Option<&credential::GuardedProvider>,
    access: Access,
    observer: Option<&CredentialGrant>,
    check: Option<&SendCheck>,
) -> Result<Client, kube::Error> {
    let Some(proxy_url) = &config.proxy_url else {
        let connector = http_connector();
        return assemble_client(
            connector, config, token, auth, guarded, access, observer, check,
        );
    };
    // Refused again rather than connecting directly, whatever set the URL.
    let target = proxy::ProxyTarget::parse(&proxy_url.to_string())
        .and_then(|target| {
            proxy::check_route(&config.cluster_url.to_string(), config.accept_invalid_certs)
                .map(|()| target)
        })
        .map_err(|refused| kube::Error::Service(refused.to_string().into()))?;
    let connector = target.tunnel(http_connector());
    assemble_client(
        connector, config, token, auth, guarded, access, observer, check,
    )
}

/// The client stack of [`build_client`], over `connector`.
#[expect(
    clippy::disallowed_types,
    reason = "the one place that assembles a client"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "build_observed_client's arguments and its connector"
)]
fn assemble_client<C>(
    connector: C,
    config: &kube::Config,
    token: Option<crate::auth_plugin::RefreshingToken>,
    auth: Option<&kube::client::middleware::AuthLayer>,
    guarded: Option<&credential::GuardedProvider>,
    access: Access,
    observer: Option<&CredentialGrant>,
    check: Option<&SendCheck>,
) -> Result<Client, kube::Error>
where
    C: tower::Service<http::Uri> + Clone + Send + Sync + 'static,
    C::Response: hyper_util::client::legacy::connect::Connection
        + hyper::rt::Read
        + hyper::rt::Write
        + Send
        + Unpin
        + 'static,
    C::Future: Send + 'static,
    C::Error: std::error::Error + Send + Sync + 'static,
{
    use kube::client::ConfigExt;
    use kube::client::retry::RetryPolicy;
    use tower::ServiceBuilder;
    use tower::retry::RetryLayer;

    let https = config.rustls_https_connector_with_connector(connector)?;
    let mut connector = hyper_timeout::TimeoutConnector::new(https);
    connector.set_connect_timeout(config.connect_timeout);
    connector.set_read_timeout(config.read_timeout);
    connector.set_write_timeout(config.write_timeout);
    connector.set_reset_reader_on_write(config.reset_reader_on_write);
    let http: hyper_util::client::legacy::Client<_, kube::client::Body> =
        hyper_util::client::legacy::Builder::new(hyper_util::rt::TokioExecutor::new())
            .build(connector);
    let transport = ServiceBuilder::new()
        .layer(config.extra_headers_layer()?)
        .map_err(tower::BoxError::from)
        .service(http);
    let authed = ServiceBuilder::new()
        .option_layer(guarded.cloned().map(tower::filter::AsyncFilterLayer::new))
        .option_layer(auth.filter(|_| guarded.is_none()))
        .option_layer(token.map(crate::auth_plugin::TokenLayer::new))
        // Again once the credential is resolved: that can wait for a refresh.
        .option_layer(check.cloned().map(tower::filter::AsyncFilterLayer::new))
        .service(transport.clone());
    let service = ServiceBuilder::new()
        // Outermost, so it judges the path as asked for, before a server URL's prefix is added.
        .option_layer((access == Access::ReadOnly).then(crate::read_only::layer))
        .layer(config.base_uri_layer())
        .option_layer(
            config
                .default_retry
                .then(|| RetryLayer::new(ReadRetry(RetryPolicy::server_retry()))),
        )
        .option_layer(observer.cloned().map(|grant| {
            tower::util::MapResponseLayer::new(move |response: http::Response<_>| {
                grant.note_response(response.status());
                response
            })
        }))
        .option_layer(check.cloned().map(tower::filter::AsyncFilterLayer::new))
        // A write's checked credential goes out as it is, with no auth layer resolving another.
        .service(credential::AuthUnlessBound {
            authed,
            direct: transport,
        });
    Ok(kube::client::ClientBuilder::new(service, config.default_namespace.clone()).build())
}

/// `client`, whose every request goes out with `credential`. It adds nothing else: each request
/// still passes through the stack [`build_client`] gave `client`.
#[expect(
    clippy::disallowed_methods,
    reason = "wraps a client build_client made, for one request of a write"
)]
pub(crate) fn sending_credential(client: Client, credential: credential::BoundHeader) -> Client {
    let namespace = client.default_namespace().to_owned();
    let service = tower::service_fn(move |mut request: http::Request<kube::client::Body>| {
        let client = client.clone();
        request.extensions_mut().insert(credential.clone());
        async move { client.send(request).await }
    });
    Client::new(service, namespace)
}

/// `client`, for one WebSocket upgrade: `check` is asked right before it is sent, and a refusal's
/// body is dropped unread, so its size costs nothing. The status is all that is used of it.
#[expect(
    clippy::disallowed_methods,
    reason = "wraps a client build_client made, for one upgrade"
)]
pub(crate) fn upgrade_client(client: Client, check: SendCheck) -> Client {
    let namespace = client.default_namespace().to_owned();
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let (client, check) = (client.clone(), check.clone());
        async move {
            check
                .refusal()
                .await
                .map_err(|why| kube::Error::Service(why.into()))?;
            let response = client.send(request).await?;
            if response.status() == http::StatusCode::SWITCHING_PROTOCOLS {
                return Ok::<_, kube::Error>(response);
            }
            let (parts, _unread) = response.into_parts();
            Ok(http::Response::from_parts(
                parts,
                kube::client::Body::empty(),
            ))
        }
    });
    Client::new(service, namespace)
}

/// `client`, whose requests use the exec token it holds now and never run the plugin for another.
/// Each request still passes through the whole stack of `client`.
#[expect(
    clippy::disallowed_methods,
    reason = "wraps a client build_client made, adding only a marker to each request"
)]
pub(crate) fn reusing_held_token(client: Client) -> Client {
    let namespace = client.default_namespace().to_owned();
    let service = tower::service_fn(move |mut request: http::Request<kube::client::Body>| {
        let client = client.clone();
        request
            .extensions_mut()
            .insert(crate::auth_plugin::HeldTokenOnly);
        async move { client.send(request).await }
    });
    Client::new(service, namespace)
}

/// Stops exec auth plugins from prompting: they get no stdin and cannot ask for input. Used when
/// stdin is not a terminal the user is watching, such as the MCP server, whose stdin is the
/// protocol stream.
pub fn disable_interactive_exec(kubeconfig: &mut Kubeconfig) {
    for named in &mut kubeconfig.auth_infos {
        if let Some(exec) = named.auth_info.as_mut().and_then(|a| a.exec.as_mut()) {
            exec.interactive_mode = Some(kube::config::ExecInteractiveMode::Never);
        }
    }
}

/// Calls `/version`. The first request also runs the exec auth plugin, if any.
pub async fn server_version(client: &Client) -> Result<ServerVersion, ClusterError> {
    let info = client
        .apiserver_version()
        .await
        .map_err(ClusterError::kube)?;
    Ok(ServerVersion {
        git_version: info.git_version,
        platform: info.platform,
    })
}

/// [`connect`] then [`server_version`], bounded by `timeout`.
pub async fn connect_and_check(
    kubeconfig: &Kubeconfig,
    context: &str,
    timeout: Duration,
) -> Result<(Client, ServerVersion), ClusterError> {
    bounded(kubeconfig, context, timeout, async {
        let client = connect(kubeconfig, context).await?;
        let version = server_version(&client).await?;
        Ok((client, version))
    })
    .await
}

/// As [`connect_and_check`], for a cluster the app manages: with the client that can write,
/// closed behind the write gate.
#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn connect_gated_and_check(
    kubeconfig: &Kubeconfig,
    context: &str,
    timeout: Duration,
) -> Result<(GatedConnection, ServerVersion), ClusterError> {
    let access = CredentialAccess::always();
    connect_gated_and_check_with_policy(kubeconfig, context, timeout, &access).await
}

/// As [`connect_and_check`], for a cluster the app manages: with the client that can write, closed
/// behind the write gate, and `access` deciding whether the credential may run.
pub(crate) async fn connect_gated_and_check_with_policy(
    kubeconfig: &Kubeconfig,
    context: &str,
    timeout: Duration,
    access: &CredentialAccess,
) -> Result<(GatedConnection, ServerVersion), ClusterError> {
    bounded(kubeconfig, context, timeout, async {
        let connection = connect_gated_with_policy(kubeconfig, context, access).await?;
        let version = server_version(&connection.clients.read).await?;
        Ok((connection, version))
    })
    .await
}

/// A read-only client for `context` with `access` deciding whether its credential may run, and
/// no write client at all: what a background feed connects with.
pub(crate) async fn connect_read_only_with_policy(
    kubeconfig: &Kubeconfig,
    context: &str,
    timeout: Duration,
    access: &CredentialAccess,
) -> Result<Client, ClusterError> {
    bounded(kubeconfig, context, timeout, async {
        let ready = prepare(kubeconfig, context, access).await?;
        let target = ready.target;
        let (mut config, token, grant) = (ready.config, ready.token, ready.grant);
        // Each retry could run the plugin again for a short-lived token. The feed has its own
        // backoff, so every request here is sent once.
        config.default_retry = false;
        let client = tokio::task::spawn_blocking(move || {
            let auth = kube::client::ConfigExt::auth_layer(&config)?;
            build_observed_client(
                &config,
                token,
                auth.as_ref(),
                None,
                Access::ReadOnly,
                Some(&grant),
                None,
            )
        })
        .await
        .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?
        .map_err(|e| ClusterError::kube(e).for_target(&target))?;
        server_version(&client).await?;
        Ok(client)
    })
    .await
}

/// `attempt`, bounded by `timeout`, its error naming `context`'s server.
async fn bounded<T>(
    kubeconfig: &Kubeconfig,
    context: &str,
    timeout: Duration,
    attempt: impl Future<Output = Result<T, ClusterError>>,
) -> Result<T, ClusterError> {
    let target = Target::of(kubeconfig, context);
    tokio::time::timeout(timeout, attempt)
        .await
        .unwrap_or_else(|_| Err(ClusterError::timeout(timeout, None)))
        .map_err(|e| e.for_target(&target))
}

/// A hash of everything that decides how a context connects: the context, its cluster entry, and
/// its user entry. When it changes after a kubeconfig reload, the cached client is stale.
/// Returns `None` if the context does not exist.
pub fn context_fingerprint(kubeconfig: &Kubeconfig, context: &str) -> Option<u64> {
    let named = kubeconfig.contexts.iter().find(|c| c.name == context)?;
    let ctx = named.context.as_ref();
    let cluster = ctx.and_then(|c| kubeconfig.clusters.iter().find(|n| n.name == c.cluster));
    let user = ctx
        .and_then(|c| c.user.as_ref())
        .and_then(|u| kubeconfig.auth_infos.iter().find(|n| &n.name == u));
    // The kube types are not `Hash`; their serialized form is a stable stand-in, with object keys
    // sorted: exec `env` items are `HashMap`s, serialized in a different order at each load, which
    // would make every load look like a change (and the MCP server connect again, and run the
    // plugin again, at every call). The sort is explicit because the app builds serde_json with
    // `preserve_order`. The string holds credentials, so it is hashed right away and never stored.
    let mut value = serde_json::to_value((named, cluster, user)).ok()?;
    value.sort_all_objects();
    let text = value.to_string();
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    Some(hasher.finish())
}

/// Formats an error with its sources, since kube's top-level messages are often vague
/// ("ServiceError: client error (Connect)"). Network failures are said in plain words instead
/// ([`api_error::classify_transport`]); [`ClusterError::kube`] does the same and can name the server.
///
/// Exec auth plugin errors are summarised instead: kube formats them with the command's `Debug`
/// form, which includes the environment set from kubeconfig (for example AWS keys), and with the
/// plugin's output, which may hold a token. Neither may reach the UI, logs, or an AI agent.
pub fn error_chain(err: &(dyn StdError + 'static)) -> String {
    if let Some(auth) = find_auth_error(err) {
        return auth_error_message(auth);
    }
    if let Some(problem) = api_error::classify_transport(err) {
        return problem.message(None);
    }
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        let msg = e.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        source = e.source();
    }
    out
}

/// Whether `err` is a connection or read that timed out below kube (its `connect_timeout`).
fn is_transport_timeout(err: &(dyn StdError + 'static)) -> bool {
    api_error::causes(err).any(|e| {
        e.is::<tokio::time::error::Elapsed>()
            || e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
    })
}

/// Whether `err` is an exec auth plugin whose program could not be found.
fn is_exec_not_found(err: &(dyn StdError + 'static)) -> bool {
    matches!(
        find_auth_error(err),
        Some(kube::client::AuthError::AuthExecStart(e)) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// The first auth error in `err`'s chain, if any.
pub(crate) fn find_auth_error<'a>(
    err: &'a (dyn StdError + 'static),
) -> Option<&'a kube::client::AuthError> {
    let mut current = Some(err);
    while let Some(e) = current {
        if let Some(kube::Error::Auth(auth)) = e.downcast_ref::<kube::Error>() {
            return Some(auth);
        }
        if let Some(auth) = e.downcast_ref::<kube::client::AuthError>() {
            return Some(auth);
        }
        current = e.source();
    }
    None
}

/// A message for an auth error that never includes the plugin's command, environment, or output.
pub(crate) fn auth_error_message(err: &kube::client::AuthError) -> String {
    use kube::client::AuthError as A;
    match err {
        A::AuthExecRun { out, .. } => format!(
            "authentication failed: the exec auth plugin exited with {}. Run it in a terminal \
             (for example, log in again) and retry.",
            out.status
        ),
        A::AuthExecParse(_) => {
            format!(
                "authentication failed: {}",
                crate::auth_plugin::UNREADABLE_OUTPUT
            )
        }
        A::AuthExec(text) if text == crate::auth_plugin::UNREADABLE_OUTPUT => {
            format!("authentication failed: {text}")
        }
        A::AuthExec(text) => match crate::auth_plugin::problem(text) {
            Some((command, problem)) => plugin_problem_message(command.as_deref(), &problem),
            None => "authentication failed: the exec auth plugin reported an error".to_owned(),
        },
        A::AuthExecStart(e) if e.kind() == std::io::ErrorKind::NotFound => {
            exec_not_found_message(None)
        }
        A::AuthExecStart(e) => format!(
            "authentication failed: the exec auth plugin could not be started ({}). Is it on PATH?",
            e.kind()
        ),
        // The remaining variants describe the failure without quoting secrets.
        other => format!("authentication failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_text_is_tidied_before_it_is_cut() {
        for pad in 230..=300 {
            let message = format!(
                "{} postgres://admin:example-value-123@db.internal:5432/app",
                "x".repeat(pad)
            );
            let shown = ClusterError::Api(message).for_agent();
            assert!(!shown.contains("exam"), "pad {pad}: {shown}");
            assert!(
                shown.chars().count() <= AGENT_CHARS + 1,
                "pad {pad}: {shown}"
            );
            // A marker is whole, or not there at all.
            if let Some(at) = shown.find("[R") {
                assert!(shown[at..].contains(']'), "pad {pad}: {shown}");
            }
        }
    }

    #[test]
    fn agent_text_keeps_its_cap_when_a_cut_ends_in_a_cookie_header() {
        for pad in 280..=300 {
            let message = format!("{} Cookie: abcdef", "x".repeat(pad));
            let shown = ClusterError::Api(message).for_agent();
            assert!(
                shown.chars().count() <= AGENT_CHARS + 1,
                "pad {pad}: {shown}"
            );
            assert!(shown.ends_with('…'), "pad {pad}: {shown}");
            assert_eq!(redact::redact_credentials(&shown).text, shown, "pad {pad}");
        }
    }

    #[test]
    fn agent_text_keeps_diagnostics_after_a_masked_header_value() {
        let message = format!(
            "Authorization: password=abc {} then the real cause: connection refused",
            "y ".repeat(20)
        );
        let shown = ClusterError::Api(message).for_agent();
        assert!(shown.contains("connection refused"), "{shown}");
        assert!(!shown.contains("abc"), "{shown}");
        let long = format!("Authorization: password=abc {}", "z".repeat(400));
        let shown = ClusterError::Api(long).for_agent();
        assert!(shown.chars().count() <= AGENT_CHARS + 1, "{shown}");
        assert!(shown.ends_with('…'), "{shown}");
        assert!(shown.contains("zzzz"), "{shown}");
    }

    #[test]
    fn a_marker_across_the_agent_cut_goes_whole() {
        let marker = "[REDACTED:password]";
        for inside in 1..marker.len() {
            let line = format!("{}{marker} rest", "x".repeat(AGENT_CHARS - inside));
            assert_eq!(
                agent_line(&line),
                format!("{}…", "x".repeat(AGENT_CHARS - inside))
            );
        }
        let whole = format!("{}{marker}{}", "x".repeat(10), "y".repeat(AGENT_CHARS));
        assert!(agent_line(&whole).contains(marker));
    }

    #[test]
    fn agent_text_is_one_line_of_a_tidied_block() {
        let message = "YWJjZGVmZ2hpamtsbW5vcA==\n-----END PRIVATE KEY-----\ndenied";
        assert_eq!(
            ClusterError::Kube(message.into()).for_agent(),
            "[REDACTED:private-key]"
        );
    }

    #[test]
    fn agent_text_hides_credentials_across_the_byte_bound() {
        let url = "postgres://admin:";
        let credential = "qéwerty-example-value";
        for offset in [1, 2, 3, 8, 17] {
            let pad = AGENT_BYTES - offset - url.len() - "password= ".len();
            let start = format!("password={} ", "x".repeat(pad));
            let short = redact::redact_credentials(&start);
            assert!(short.text.chars().count() + url.len() + offset < AGENT_CHARS);
            assert_eq!(start.len() + url.len() + offset, AGENT_BYTES);
            let message = format!("{start}{url}{credential}@db.internal/app");
            let shown = ClusterError::Api(message).for_agent();
            assert!(!shown.contains('q'), "offset {offset}: {shown}");
        }
    }

    #[test]
    fn agent_text_limits_work_on_a_malformed_cookie_list() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut body = String::from("Cookie: [");
            const MARKER: &str = "[REDACTED:";
            const BODY_BYTES: usize = 1024 * 1024;
            body.push_str(&MARKER.repeat((BODY_BYTES - body.len()).div_ceil(MARKER.len())));
            body.truncate(BODY_BYTES);
            let bounded = ClusterError::Api(body[..AGENT_BYTES].to_owned()).for_agent();
            let oversized = ClusterError::Api(body).for_agent();
            tx.send((bounded, oversized)).unwrap();
        });
        let (bounded, oversized) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(bounded.chars().count() <= AGENT_CHARS + 1);
        assert_eq!(
            oversized,
            "Error details omitted because the message is too long"
        );
    }

    // covers UP-04, UP-10
    #[test]
    fn exec_plugin_errors_hide_command_env_and_output() {
        use std::os::unix::process::ExitStatusExt;
        let mut cmd = std::process::Command::new("aws");
        cmd.env("AWS_SECRET_ACCESS_KEY", "s3cr3t").arg("eks");
        let err = kube::Error::Auth(kube::client::AuthError::AuthExecRun {
            cmd: format!("{cmd:?}"),
            out: Box::new(std::process::Output {
                status: std::process::ExitStatus::from_raw(255 << 8),
                stdout: b"token-abc".to_vec(),
                stderr: b"sso expired s3cr3t".to_vec(),
            }),
        });
        let message = ClusterError::kube(err).to_string();
        assert!(message.starts_with("authentication failed"), "{message}");
        assert!(
            !message.contains("s3cr3t") && !message.contains("token-abc"),
            "{message}"
        );
        assert!(!message.contains("aws"), "{message}");
    }

    fn kubeconfig(server: &str) -> Kubeconfig {
        Kubeconfig::from_yaml(&format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {server}\nusers:\n- name: u\n  user:\n    token: t\ncontexts:\n- name: ctx\n  context:\n    cluster: c\n    user: u\n"
        ))
        .unwrap()
    }

    // covers UP-06
    #[test]
    fn fingerprint_changes_with_server() {
        let a = context_fingerprint(&kubeconfig("https://a"), "ctx").unwrap();
        let a2 = context_fingerprint(&kubeconfig("https://a"), "ctx").unwrap();
        let b = context_fingerprint(&kubeconfig("https://b"), "ctx").unwrap();
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(context_fingerprint(&kubeconfig("https://a"), "nope"), None);
    }

    // covers UP-06: exec `env` items are maps, read in a different order at each load; the
    // fingerprint must not see that as a change.
    #[test]
    fn fingerprint_ignores_the_order_of_exec_env_items() {
        let env: String = (0..8)
            .map(|i| format!("      - name: N{i}\n        value: v{i}\n"))
            .collect();
        let load = || {
            Kubeconfig::from_yaml(&format!(
                "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
                 https://a\nusers:\n- name: u\n  user:\n    exec:\n      apiVersion: \
                 client.authentication.k8s.io/v1\n      command: aws\n      env:\n{env}\
                 contexts:\n- name: ctx\n  context:\n    cluster: c\n    user: u\n"
            ))
            .unwrap()
        };
        let first = context_fingerprint(&load(), "ctx").unwrap();
        for _ in 0..20 {
            assert_eq!(context_fingerprint(&load(), "ctx"), Some(first));
        }
    }

    // covers UP-09
    #[test]
    fn interactive_exec_can_be_disabled() {
        let mut kc = Kubeconfig::from_yaml(
            "apiVersion: v1\nkind: Config\nusers:\n- name: u\n  user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      command: aws\n      interactiveMode: IfAvailable\n- name: t\n  user:\n    token: x\n",
        )
        .unwrap();
        disable_interactive_exec(&mut kc);
        let exec = kc.auth_infos[0]
            .auth_info
            .as_ref()
            .unwrap()
            .exec
            .as_ref()
            .unwrap();
        assert_eq!(
            exec.interactive_mode,
            Some(kube::config::ExecInteractiveMode::Never)
        );
        assert!(kc.auth_infos[1].auth_info.as_ref().unwrap().exec.is_none());
    }

    // covers UP-04, UP-10
    #[test]
    fn api_errors_are_reason_and_message() {
        let status = kube::core::Status {
            code: 404,
            reason: "NotFound".into(),
            message: "pods \"x\" not found".into(),
            ..Default::default()
        };
        let err = ClusterError::kube(kube::Error::Api(Box::new(status)));
        assert_eq!(err.to_string(), "NotFound: pods \"x\" not found");
        let bare = kube::core::Status {
            code: 500,
            ..Default::default()
        };
        let err = ClusterError::kube(kube::Error::Api(Box::new(bare)));
        assert_eq!(
            err.to_string(),
            "the API server failed with an internal error (500 Internal Server Error); try \
             again, or check the API server's logs"
        );
    }

    // covers UP-04
    #[test]
    fn a_long_status_message_is_shortened_after_its_values_are_hidden() {
        let padding = "p".repeat(150);
        let value = "placeholder-value-".repeat(5);
        let status = kube::core::Status {
            code: 403,
            reason: "Forbidden".into(),
            message: format!("{padding} https://placeholder:{value}@api.example.invalid/ denied"),
            ..Default::default()
        };
        let err = ClusterError::kube(kube::Error::Api(Box::new(status)));

        let shown = err.summary();
        assert!(!shown.as_str().contains("placeholder-value"), "{shown}");
        assert!(shown.as_str().chars().count() <= SHOWN_CHARS + 1, "{shown}");
        assert!(shown.as_str().starts_with("Forbidden: "), "{shown}");
        assert!(err.to_string().contains(&value), "the raw error is kept");
    }

    // covers UP-04
    #[test]
    fn auth_failures_are_not_blamed_on_the_api_server() {
        // An auth provider that cannot reach its own endpoint.
        let err = kube::Error::Auth(kube::client::AuthError::AuthExecStart(
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        ));
        let message = ClusterError::kube(err).to_string();
        assert!(message.starts_with("authentication failed"), "{message}");
        assert!(!message.contains("connection refused by"), "{message}");
    }

    // covers UP-04
    #[test]
    fn http_statuses_say_what_they_mean() {
        let server = Some("https://h:6443");
        for (code, message) in [
            (
                401,
                "https://h:6443 rejected the credentials (401 Unauthorized); log in again or renew \
                 the credentials",
            ),
            (
                403,
                "https://h:6443 denied access (403 Forbidden); check your RBAC permissions",
            ),
            (
                404,
                "https://h:6443 answered 404 Not Found; is the server URL (and its path) right?",
            ),
            (
                429,
                "https://h:6443 is limiting requests (429 Too Many Requests); try again later",
            ),
            (
                503,
                "https://h:6443 is unavailable (503 Service Unavailable); it may be starting or \
                 overloaded",
            ),
            (
                504,
                "a proxy in front of https://h:6443 timed out waiting for it (504 Gateway Timeout)",
            ),
            (418, "https://h:6443 answered 418 I'm a teapot"),
            (599, "https://h:6443 answered HTTP 599"),
        ] {
            assert_eq!(api_error::status_meaning(code, server), message);
        }
        // An empty or unparsed body says nothing more; a 401's Status never does.
        let unparsed = kube::core::Status {
            code: 503,
            reason: "Failed to parse error data".into(),
            message: "<html>".into(),
            ..Default::default()
        };
        let target = Target::of(&kubeconfig("https://u:p@h:6443/?token=x"), "ctx");
        assert_eq!(
            ClusterError::kube(kube::Error::Api(Box::new(unparsed)))
                .for_target(&target)
                .to_string(),
            "https://h:6443 is unavailable (503 Service Unavailable); it may be starting or \
             overloaded"
        );
    }

    // covers UP-04
    // covers UP-53: a write answered 429 is sent once; only reads are retried.
    #[tokio::test]
    async fn writes_are_never_retried() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use http::Request;
        use kube::client::Body;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buffer = [0; 4096];
                let _ = stream.read(&mut buffer).await;
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\n\
                          connection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        let config = kube::Config::new(url.parse().unwrap());
        assert!(config.default_retry, "the retry layer is on");
        let client = build_client(&config, None, None, None, Access::Unrestricted).unwrap();
        let request = Request::post("/api/v1/namespaces/demo/pods/web/eviction")
            .body(Body::from(b"{}".to_vec()))
            .unwrap();
        let response = client.send(request).await.unwrap();
        assert_eq!(response.status(), 429);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    // covers UP-54: a read-only client refuses writes and tunnels without touching the server,
    // and still sends reads.
    #[tokio::test]
    async fn a_read_only_client_stops_writes_before_the_server() {
        use http::{Method, Request};
        use kube::client::Body;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let config = kube::Config::new(url.parse().unwrap());
        let client = build_client(&config, None, None, None, Access::ReadOnly).unwrap();
        let pod = "/api/v1/namespaces/demo/pods/web";
        for (method, path) in [
            (Method::POST, "/api/v1/namespaces/demo/pods".to_owned()),
            (Method::PUT, pod.to_owned()),
            (Method::PATCH, pod.to_owned()),
            (Method::DELETE, pod.to_owned()),
            (Method::GET, format!("{pod}/exec?command=sh")),
            (Method::GET, format!("{pod}/attach")),
            (Method::GET, format!("{pod}/portforward?ports=80")),
            (Method::GET, format!("{pod}/proxy/admin")),
            (Method::GET, format!("{pod}/%65xec")),
        ] {
            let request = Request::builder()
                .method(method.clone())
                .uri(&path)
                .body(Body::empty())
                .unwrap();
            let refused = client.send(request).await.unwrap_err().to_string();
            assert!(refused.contains("read-only"), "{method} {path}: {refused}");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "a refused request reached the server"
        );

        let reader = client.clone();
        let read = tokio::spawn(async move {
            let request = Request::get("/version").body(Body::empty()).unwrap();
            reader.send(request).await
        });
        tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("a read reaches the server")
            .unwrap();
        read.abort();
    }

    #[tokio::test]
    async fn unknown_context_is_an_error() {
        let result = connect(&kubeconfig("https://a"), "nope").await;
        assert!(matches!(result, Err(ClusterError::UnknownContext(_))));
    }

    fn exec_not_found() -> kube::Error {
        kube::Error::Auth(kube::client::AuthError::AuthExecStart(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        )))
    }

    fn exec_kubeconfig(command: &str) -> Kubeconfig {
        Kubeconfig::from_yaml(&format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: https://admin:pw@eks.example.com:443/x?token=t\nusers:\n- name: u\n  user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      command: {command}\n      args: [eks, get-token, --secret-arg]\n      env:\n      - name: AWS_SECRET_ACCESS_KEY\n        value: s3cr3t\ncontexts:\n- name: ctx\n  context:\n    cluster: c\n    user: u\n"
        ))
        .unwrap()
    }

    // covers UP-04, UP-05
    #[test]
    fn missing_exec_plugin_names_the_program_and_the_path_source() {
        let target = Target::of(&exec_kubeconfig("/Users/someone/bin/aws"), "ctx");
        assert_eq!(target.exec_command.as_deref(), Some("aws"));
        let message = ClusterError::kube(exec_not_found())
            .for_target(&target)
            .to_string();
        assert_eq!(
            message,
            "exec auth plugin `aws` not found on PATH (inherited PATH); install it, or set \
             `path` in Flint's settings.toml"
        );
        for secret in ["s3cr3t", "secret-arg", "someone", "pw", "token"] {
            assert!(!message.contains(secret), "{message}");
        }
        // What the app shows keeps the name; foreign text is still reduced.
        let shown = ClusterError::kube(exec_not_found())
            .for_target(&target)
            .summary();
        assert_eq!(shown.as_str(), message);
        assert_eq!(
            ClusterError::Kube("bad value \"s3cr3t\"\nmore".into())
                .summary()
                .as_str(),
            "bad value \"…\""
        );
        let config = ClusterError::Config {
            context: "ctx".into(),
            message: "unknown variant `s3cr3t`".into(),
        };
        assert_eq!(
            config.summary().as_str(),
            "invalid kubeconfig for context \"ctx\": unknown variant `…`"
        );
        // Without a target, and in strings built from the chain (watch errors), the wording is
        // the same, without the command.
        let anonymous = "exec auth plugin not found on PATH (inherited PATH); install it, or set \
                         `path` in Flint's settings.toml";
        assert_eq!(ClusterError::kube(exec_not_found()).to_string(), anonymous);
        assert_eq!(error_chain(&exec_not_found()), anonymous);
    }

    // covers UP-04
    #[test]
    fn timeouts_name_the_api_server_and_plugin_without_credentials() {
        let target = Target::of(&exec_kubeconfig("aws"), "ctx");
        assert_eq!(
            target.server.as_deref(),
            Some("https://eks.example.com:443/x")
        );
        let err = ClusterError::timeout(Duration::from_secs(10), None).for_target(&target);
        assert_eq!(
            err.to_string(),
            "timed out after 10s reaching https://eks.example.com:443/x (including exec plugin `aws`)"
        );
        let direct = ClusterError::timeout(
            Duration::from_millis(1500),
            Some("https://u:p@127.0.0.1:6443"),
        );
        assert_eq!(
            direct.to_string(),
            "timed out after 1.5s reaching https://127.0.0.1:6443"
        );
        // A server already named is kept.
        let token_target = Target::of(&kubeconfig("https://other:6443"), "ctx");
        assert_eq!(
            direct.for_target(&token_target).to_string(),
            "timed out after 1.5s reaching https://127.0.0.1:6443"
        );
        assert_eq!(
            ClusterError::timeout(CONNECT_TIMEOUT, None).to_string(),
            "timed out after 1m0s connecting"
        );
        assert_eq!(
            Target::of(&exec_kubeconfig("aws"), "nope"),
            Target::default()
        );
    }

    #[test]
    fn timeouts_keep_the_exec_plugin_hint_when_the_command_is_hidden() {
        let target = Target::of(&exec_kubeconfig("/opt/private path/plugin"), "ctx");
        let err = ClusterError::timeout(Duration::from_secs(10), None).for_target(&target);
        assert_eq!(
            err.summary().as_str(),
            "timed out after 10s reaching https://eks.example.com:443/x (including exec plugin)"
        );
    }

    // covers UP-04
    #[test]
    fn durations_print_as_go_does() {
        for (d, s) in [
            (Duration::from_millis(500), "500ms"),
            (Duration::from_secs(10), "10s"),
            (Duration::from_millis(1500), "1.5s"),
            (Duration::from_millis(2250), "2.25s"),
            (Duration::from_secs(60), "1m0s"),
            (Duration::from_secs(90), "1m30s"),
        ] {
            assert_eq!(go_duration(d), s);
        }
    }

    // covers UP-04
    #[test]
    fn transport_timeouts_become_timeouts() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "deadline has elapsed");
        let err = ClusterError::kube(kube::Error::Service(Box::new(io)));
        assert!(err.is_timeout(), "{err}");
        let target = Target::of(&kubeconfig("https://10.255.255.1:6443"), "ctx");
        assert_eq!(
            err.for_target(&target).to_string(),
            "timed out reaching https://10.255.255.1:6443"
        );
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(!ClusterError::kube(kube::Error::Service(Box::new(refused))).is_timeout());
    }

    // covers UP-04
    #[tokio::test(start_paused = true)]
    async fn connect_and_check_timeouts_name_the_server() {
        // A server that accepts connections and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _keep = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let kc = kubeconfig(&format!("http://{addr}"));
        let Err(err) = connect_and_check(&kc, "ctx", Duration::from_secs(2)).await else {
            panic!("a server that never answers cannot connect");
        };
        assert_eq!(
            err.to_string(),
            format!("timed out after 2s reaching http://{addr}")
        );
    }

    /// Reads an integer socket option.
    fn sockopt(fd: std::os::fd::RawFd, level: libc::c_int, name: libc::c_int) -> libc::c_int {
        let mut value: libc::c_int = 0;
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()).unwrap();
        // SAFETY: `value` and `len` are valid for writes of the sizes passed.
        let rc =
            unsafe { libc::getsockopt(fd, level, name, (&raw mut value).cast(), &raw mut len) };
        assert_eq!(rc, 0, "getsockopt: {}", std::io::Error::last_os_error());
        value
    }

    // covers UP-40, UP-41
    #[tokio::test]
    async fn api_server_connections_use_tcp_keepalive() {
        use std::os::fd::AsRawFd;
        use tower::ServiceExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _accept = tokio::spawn(async move { listener.accept().await });
        let uri: http::Uri = format!("http://{addr}").parse().unwrap();
        let stream = http_connector().oneshot(uri).await.unwrap();
        let fd = stream.inner().as_raw_fd();
        let secs = libc::c_int::try_from(KEEPALIVE.as_secs()).unwrap();
        assert_ne!(sockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE), 0);
        #[cfg(target_os = "macos")]
        let idle = libc::TCP_KEEPALIVE;
        #[cfg(not(target_os = "macos"))]
        let idle = libc::TCP_KEEPIDLE;
        assert_eq!(sockopt(fd, libc::IPPROTO_TCP, idle), secs);
        assert_eq!(sockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL), secs);
        assert_eq!(
            sockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT),
            libc::c_int::try_from(KEEPALIVE_PROBES).unwrap()
        );
    }

    // covers UP-03
    #[tokio::test]
    async fn clients_reach_the_api_server_through_the_keepalive_stack() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A minimal HTTP/1.1 server that answers `/version` once.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "the request ended early");
                request.extend_from_slice(&buf[..n]);
            }
            let body = r#"{"major":"1","minor":"34","gitVersion":"v1.34.0","gitCommit":"","gitTreeState":"","buildDate":"","goVersion":"","compiler":"","platform":"linux/arm64"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        let client = connect(&kubeconfig(&format!("http://{addr}")), "ctx")
            .await
            .unwrap();
        let version = server_version(&client).await.unwrap();
        assert_eq!(version.git_version, "v1.34.0");
        let request = server.await.unwrap();
        assert!(request.starts_with("GET /version"), "{request}");
        // The auth layer is in the stack.
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer t\r\n"),
            "{request}"
        );
    }
}
