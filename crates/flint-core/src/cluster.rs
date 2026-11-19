//! Connecting to a cluster (one kubeconfig context) and checking it is alive.

use std::error::Error as StdError;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use kube::Client;
use kube::config::{KubeConfigOptions, Kubeconfig};

use crate::api_error::ApiError;
use crate::redact;

/// How long a connection attempt may take, exec auth plugin included. Some plugins (SSO logins)
/// are slow, so this is generous.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// A cluster's identity in Flint: its kubeconfig context name.
pub type ClusterId = String;

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("context \"{0}\" is not in the loaded kubeconfig files")]
    UnknownContext(String),
    #[error("invalid kubeconfig for context \"{context}\": {message}")]
    Config { context: String, message: String },
    #[error("{0}")]
    Kube(String),
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
    /// API errors become `"<reason>: <message>"` (for example `"NotFound: pods \"x\" not found"`);
    /// other errors keep their full source chain, except auth plugin errors (see
    /// [`error_chain`]).
    pub fn kube(err: kube::Error) -> Self {
        match ApiError::from_kube(&err) {
            Some(api) => ClusterError::Kube(api_status_message(api)),
            None if is_exec_not_found(&err) => ClusterError::ExecPluginNotFound { command: None },
            // kube's own connection timeout: the server did not answer at all.
            None if is_transport_timeout(&err) => ClusterError::Timeout {
                after: None,
                server: None,
                exec_command: None,
            },
            None => ClusterError::Kube(error_chain(&err)),
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

    /// Names what failed: the API server a timeout was reaching, the plugin that is missing.
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
            other => other,
        }
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

fn api_status_message(status: ApiError<'_>) -> String {
    let reason = if status.reason.is_empty() {
        format!("HTTP {}", status.code)
    } else {
        status.reason.to_owned()
    };
    if status.message.is_empty() {
        reason
    } else {
        format!("{reason}: {}", status.message)
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
/// kube runs exec auth plugins synchronously while building the client, so that step runs on a
/// blocking thread. This keeps the async caller cancellable: a timeout around `connect` fires even
/// while a plugin waits for a login that never comes (the plugin's thread is left to finish).
pub async fn connect(kubeconfig: &Kubeconfig, context: &str) -> Result<Client, ClusterError> {
    if !kubeconfig.contexts.iter().any(|c| c.name == context) {
        return Err(ClusterError::UnknownContext(context.to_owned()));
    }
    let options = KubeConfigOptions {
        context: Some(context.to_owned()),
        cluster: None,
        user: None,
    };
    let config = kube::Config::from_custom_kubeconfig(kubeconfig.clone(), &options)
        .await
        .map_err(|e| ClusterError::Config {
            context: context.to_owned(),
            message: error_chain(&e),
        })?;
    let target = Target::of(kubeconfig, context);
    tokio::task::spawn_blocking(move || Client::try_from(config))
        .await
        .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?
        .map_err(|e| ClusterError::kube(e).for_target(&target))
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
    // The kube types are not `Hash`; their serialized form is a stable stand-in. The string holds
    // credentials, so it is hashed right away and never stored.
    let text = serde_json::to_string(&(named, cluster, user)).ok()?;
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    Some(hasher.finish())
}

/// Formats an error with its sources, since kube's top-level messages are often vague
/// ("ServiceError: client error (Connect)").
///
/// Exec auth plugin errors are summarised instead: kube formats them with the command's `Debug`
/// form, which includes the environment set from kubeconfig (for example AWS keys), and with the
/// plugin's output, which may hold a token. Neither may reach the UI, logs, or an AI agent.
pub fn error_chain(err: &(dyn StdError + 'static)) -> String {
    if let Some(auth) = find_auth_error(err) {
        return auth_error_message(auth);
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
    let mut current = Some(err);
    while let Some(e) = current {
        if e.is::<tokio::time::error::Elapsed>()
            || e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
        {
            return true;
        }
        current = e.source();
    }
    false
}

/// Whether `err` is an exec auth plugin whose program could not be found.
fn is_exec_not_found(err: &(dyn StdError + 'static)) -> bool {
    matches!(
        find_auth_error(err),
        Some(kube::client::AuthError::AuthExecStart(e)) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// The first auth error in `err`'s chain, if any.
fn find_auth_error<'a>(err: &'a (dyn StdError + 'static)) -> Option<&'a kube::client::AuthError> {
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
        assert_eq!(err.to_string(), "HTTP 500");
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
}
