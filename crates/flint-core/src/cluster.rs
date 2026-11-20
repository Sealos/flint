//! Connecting to a cluster (one kubeconfig context) and checking it is alive.

use std::error::Error as StdError;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use kube::Client;
use kube::config::{KubeConfigOptions, Kubeconfig};

use crate::api_error::{self, ApiError, Transport};
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
    /// found"`, RBAC's `"forbidden: User \"jane\" cannot …"`). The names it quotes are what the
    /// user needs to act on, so they are shown, on one line.
    #[error("{0}")]
    Api(String),
    /// A request or the whole attempt took too long. `after` is the limit, when known (a
    /// connection timeout inside kube does not say). Build it with [`ClusterError::timeout`] or
    /// [`ClusterError::kube`]; [`ClusterError::for_target`] names the server and the plugin.
    /// Both are already safe to show ([`Target`]).
    #[error("{}", timeout_message(*after, server.as_deref(), exec_command.as_deref()))]
    Timeout {
        after: Option<Duration>,
        server: Option<String>,
        exec_command: Option<String>,
    },
    /// The exec auth plugin's program is not on `PATH`. `command` is its file name only
    /// ([`redact::command_name`]), filled in by [`ClusterError::for_target`].
    #[error("{}", exec_not_found_message(command.as_deref()))]
    ExecPluginNotFound { command: Option<String> },
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
fn timeout_message(after: Option<Duration>, server: Option<&str>, exec: Option<&str>) -> String {
    let mut out = "timed out".to_owned();
    if let Some(after) = after {
        out.push_str(&format!(" after {}", go_duration(after)));
    }
    match server {
        Some(server) => out.push_str(&format!(" reaching {server}")),
        None => out.push_str(" connecting"),
    }
    if let Some(exec) = exec {
        out.push_str(&format!(" (including exec plugin `{exec}`)"));
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

/// A duration as Go (and so kubectl) prints it: `500ms`, `10s`, `1.5s`, `1m0s`.
fn go_duration(d: Duration) -> String {
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
    /// URL and plugin name are already safe, [`Target`]); a server's `Status` on one line, its
    /// quoted names kept; and other text from kube or the kubeconfig reduced to one line with
    /// quoted values hidden ([`redact::safe_error_summary`]).
    pub fn summary(&self) -> SafeErrorText {
        SafeErrorText(match self {
            ClusterError::Kube(message) => redact::safe_error_summary(message),
            ClusterError::Config { context, message } => format!(
                "invalid kubeconfig for context \"{context}\": {}",
                redact::safe_error_summary(message)
            ),
            ClusterError::Api(message) => redact::first_line(message, SHOWN_CHARS),
            other => other.to_string(),
        })
    }

    /// What an AI agent reads (the MCP server): like [`ClusterError::summary`], but text from
    /// the server or kube keeps its quoted values (an agent needs the names to act) and is cut
    /// to one line of at most 300 characters, so a server cannot hand it a long, multi-line
    /// message.
    pub fn for_agent(&self) -> String {
        match self {
            ClusterError::Kube(_) | ClusterError::Api(_) | ClusterError::Config { .. } => {
                redact::first_line(&self.to_string(), AGENT_CHARS)
            }
            other => other.to_string(),
        }
    }

    /// A timeout while reaching `server` (a kubeconfig `server` URL, shown through
    /// [`redact::server_url`]).
    pub fn timeout(after: Duration, server: Option<&str>) -> Self {
        ClusterError::Timeout {
            after: Some(after),
            server: server.map(redact::server_url),
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
                exec_command,
            } => ClusterError::Timeout {
                after,
                server: server.or_else(|| target.server.clone()),
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
        let exec_command = ctx
            .and_then(|c| c.user.as_ref())
            .and_then(|u| kubeconfig.auth_infos.iter().find(|n| &n.name == u))
            .and_then(|n| n.auth_info.as_ref())
            .and_then(|a| a.exec.as_ref())
            .and_then(|e| e.command.as_deref())
            .map(redact::command_name);
        Target {
            server,
            exec_command,
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
    if !kubeconfig.contexts.iter().any(|c| c.name == context) {
        return Err(ClusterError::UnknownContext(context.to_owned()));
    }
    let options = KubeConfigOptions {
        context: Some(context.to_owned()),
        cluster: None,
        user: None,
    };
    let mut config = kube::Config::from_custom_kubeconfig(kubeconfig.clone(), &options)
        .await
        .map_err(|e| ClusterError::Config {
            context: context.to_owned(),
            message: error_chain(&e),
        })?;
    let target = Target::of(kubeconfig, context);
    let token = crate::auth_plugin::resolve(&mut config)
        .await
        .map_err(|e| ClusterError::kube(kube::Error::Auth(e)).for_target(&target))?;
    tokio::task::spawn_blocking(move || build_client(config, token))
        .await
        .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?
        .map_err(|e| ClusterError::kube(e).for_target(&target))
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
/// Flint's features (rustls, no proxy support, no gzip): TLS, kube's connect, read and write
/// timeouts, base URI, retries, auth, and extra headers. Left out: kube's debug-level HTTP
/// tracing spans, and the client's `valid_until` (an exec plugin certificate's expiry, which
/// kube only records and Flint never reads). A kubeconfig with a proxy goes through kube's own
/// builder, which reports that proxies are not supported, as before. `token` is the refreshing
/// token of an exec auth plugin ([`crate::auth_plugin::resolve`]), set after kube's auth layer.
fn build_client(
    config: kube::Config,
    token: Option<crate::auth_plugin::TokenLayer>,
) -> Result<Client, kube::Error> {
    use kube::client::ConfigExt;
    use kube::client::retry::RetryPolicy;
    use tower::ServiceBuilder;
    use tower::retry::RetryLayer;

    if config.proxy_url.is_some() {
        return Client::try_from(config);
    }
    let https = config.rustls_https_connector_with_connector(http_connector())?;
    let mut connector = hyper_timeout::TimeoutConnector::new(https);
    connector.set_connect_timeout(config.connect_timeout);
    connector.set_read_timeout(config.read_timeout);
    connector.set_write_timeout(config.write_timeout);
    connector.set_reset_reader_on_write(config.reset_reader_on_write);
    let http: hyper_util::client::legacy::Client<_, kube::client::Body> =
        hyper_util::client::legacy::Builder::new(hyper_util::rt::TokioExecutor::new())
            .build(connector);
    let service = ServiceBuilder::new()
        .layer(config.base_uri_layer())
        .option_layer(
            config
                .default_retry
                .then(|| RetryLayer::new(RetryPolicy::server_retry())),
        )
        .option_layer(config.auth_layer()?)
        .option_layer(token)
        .layer(config.extra_headers_layer()?)
        .map_err(tower::BoxError::from)
        .service(http);
    Ok(kube::client::ClientBuilder::new(service, config.default_namespace).build())
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
    let target = Target::of(kubeconfig, context);
    tokio::time::timeout(timeout, async {
        let client = connect(kubeconfig, context).await?;
        let version = server_version(&client).await?;
        Ok((client, version))
    })
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
    // The kube types are not `Hash`; their serialized form is a stable stand-in. It goes through
    // `serde_json::Value`, whose objects keep their keys sorted: exec `env` items are `HashMap`s,
    // serialized in a different order at each load, which would make every load look like a
    // change (and the MCP server connect again, and run the plugin again, at every call). The
    // string holds credentials, so it is hashed right away and never stored.
    let text = serde_json::to_value((named, cluster, user))
        .ok()?
        .to_string();
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
fn auth_error_message(err: &kube::client::AuthError) -> String {
    use kube::client::AuthError as A;
    match err {
        A::AuthExecRun { out, .. } => format!(
            "authentication failed: the exec auth plugin exited with {}. Run it in a terminal \
             (for example, log in again) and retry.",
            out.status
        ),
        A::AuthExecParse(_) => {
            "authentication failed: the exec auth plugin's output could not be parsed".to_owned()
        }
        A::AuthExec(_) => {
            "authentication failed: the exec auth plugin reported an error".to_owned()
        }
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
