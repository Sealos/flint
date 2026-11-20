//! Exec auth plugins (`aws eks get-token`, `gke-gcloud-auth-plugin`, `kubelogin`, …), run by
//! Flint instead of kube so that a plugin that hangs can be ended.
//!
//! kube 5.1 runs the plugin with a blocking `Command::output()`, when it builds the client and
//! again at every token refresh. A timeout around that only stops the wait: the blocking thread
//! and the plugin process live on until the plugin exits by itself, and every retry starts
//! another. Here the plugin is a tokio child in a process group of its own. Dropping the future
//! that waits for it (a timeout, a cancelled connection attempt) kills the plugin and whatever it
//! started, and tokio reaps it. No blocking thread is involved.
//!
//! Otherwise this follows kube's `auth_exec` (kube-client 5.1): the same arguments, environment,
//! `KUBERNETES_EXEC_INFO` and interactive handling, YAML-tolerant parsing, kube's precedence of
//! credentials, a token refreshed a minute before it expires, and kube's error values
//! ([`AuthError`]), so errors read as before.

use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::Arc;

use base64::Engine;
use futures::future::BoxFuture;
use http::header::AUTHORIZATION;
use http::{HeaderValue, Request};
use jiff::{SignedDuration, Timestamp};
use kube::client::AuthError;
use kube::config::{ExecConfig, ExecInteractiveMode};
use secrecy::SecretString;
use serde::Deserialize;
use tower::BoxError;
use tower::filter::{AsyncFilterLayer, AsyncPredicate};

use crate::cluster::CONNECT_TIMEOUT;
use crate::redact;

/// A token this close to its expiry is refreshed first, as kube does.
const REFRESH_MARGIN: SignedDuration = SignedDuration::from_secs(60);

/// What a plugin run returned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Credential {
    Token {
        token: String,
        expires: Option<Timestamp>,
    },
    /// PEM text, as the plugin printed it.
    Certificate { certificate: String, key: String },
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
            auth.client_key_data = Some(SecretString::from(base64.encode(key)));
            auth.client_certificate = None;
            auth.client_key = None;
            None
        }
        Credential::Token {
            token,
            expires: None,
        } => {
            auth.token = Some(SecretString::from(token));
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
    // shell wrapper runs the real binary). Not when it may prompt on the terminal Flint was
    // started from (through stdin or `/dev/tty`): a background group stops when it does.
    let own_group = !(interactive && has_terminal());
    if own_group {
        cmd.process_group(0);
    }
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().map_err(AuthError::AuthExecStart)?;
    // Closed at once, as `Command::output()` does.
    drop(child.stdin.take());
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let mut running = Running {
        group: child.id().filter(|_| own_group),
        child,
    };
    let (status, stdout, stderr) =
        tokio::try_join!(running.child.wait(), read_all(stdout), read_all(stderr))
            .map_err(AuthError::AuthExecStart)?;
    // Finished: nothing is left to kill, and the group id may be reused.
    running.group = None;
    let out = std::process::Output {
        status,
        stdout,
        stderr,
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

/// Whether Flint has a controlling terminal a plugin could prompt on.
fn has_terminal() -> bool {
    std::io::stdin().is_terminal() || std::fs::File::open("/dev/tty").is_ok()
}

/// All of a pipe, or nothing for none.
async fn read_all(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

/// A running plugin. Dropped before it finished (`group` still set), it kills the plugin's
/// process group, then the plugin itself (`kill_on_drop`), which tokio reaps.
struct Running {
    child: tokio::process::Child,
    group: Option<u32>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(pgid) = self.group.and_then(|id| libc::pid_t::try_from(id).ok()) {
            // SAFETY: killpg takes plain integers. This runs before `child` is dropped, so tokio
            // has not reaped the leader through it. Should `wait` have reaped it already, the
            // pipe reads were still pending, so a process in the group still holds them open and
            // the group id still names this group.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
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
            (Some(certificate), Some(key), _) => Credential::Certificate { certificate, key },
            (_, _, Some(token)) => Credential::Token { token, expires },
            _ => Credential::None,
        },
    )
}

/// The client stack's layer that sets the plugin's token on each request.
pub(crate) type TokenLayer = AsyncFilterLayer<RefreshingToken>;

/// A plugin token that expires, shared by every request of one client. The first request that
/// finds it within [`REFRESH_MARGIN`] of its expiry runs the plugin again, bounded by
/// [`CONNECT_TIMEOUT`]; the others wait for that run, so one client runs at most one plugin at a
/// time.
#[derive(Clone)]
pub(crate) struct RefreshingToken(Arc<tokio::sync::Mutex<TokenState>>);

struct TokenState {
    exec: ExecConfig,
    token: String,
    expires: Timestamp,
}

impl RefreshingToken {
    async fn header(&self) -> Result<HeaderValue, BoxError> {
        let mut state = self.0.lock().await;
        if Timestamp::now() + REFRESH_MARGIN >= state.expires {
            match tokio::time::timeout(CONNECT_TIMEOUT, run(&state.exec)).await?? {
                Credential::Token {
                    token,
                    expires: Some(expires),
                } => {
                    state.token = token;
                    state.expires = expires;
                }
                // kube's answer when a refresh no longer returns an expiring token.
                _ => return Err(AuthError::UnrefreshableTokenResponse.into()),
            }
        }
        let mut value = HeaderValue::try_from(format!("Bearer {}", state.token))
            .map_err(AuthError::InvalidBearerToken)?;
        value.set_sensitive(true);
        Ok(value)
    }
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
