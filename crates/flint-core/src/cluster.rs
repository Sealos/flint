//! Connecting to a cluster (one kubeconfig context) and checking it is alive.

use std::error::Error as StdError;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use kube::Client;
use kube::config::{KubeConfigOptions, Kubeconfig};

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
    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

impl ClusterError {
    /// API errors become "<reason>: <message>" (for example "NotFound: pods \"x\" not found");
    /// other errors keep their full source chain.
    pub fn kube(err: kube::Error) -> Self {
        match &err {
            kube::Error::Api(status) => ClusterError::Kube(api_status_message(status)),
            _ => ClusterError::Kube(error_chain(&err)),
        }
    }
}

fn api_status_message(status: &kube::core::Status) -> String {
    let reason = if status.reason.is_empty() {
        format!("HTTP {}", status.code)
    } else {
        status.reason.clone()
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
    tokio::task::spawn_blocking(move || Client::try_from(config))
        .await
        .map_err(|e| ClusterError::Kube(format!("client setup failed: {e}")))?
        .map_err(ClusterError::kube)
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
    tokio::time::timeout(timeout, async {
        let client = connect(kubeconfig, context).await?;
        let version = server_version(&client).await?;
        Ok((client, version))
    })
    .await
    .map_err(|_| ClusterError::Timeout(timeout))?
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
}
