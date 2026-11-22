use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kube::config::Kubeconfig;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::auth_plugin::tests::{FakePlugin, wait_dead, wait_reaped};
use crate::cluster::{self, ClusterError, PluginProblem};
use crate::write::WriteError;

/// An API server on a local port that answers `/version` and counts the requests it got.
struct CountingApi {
    url: String,
    requests: Arc<AtomicUsize>,
}

impl CountingApi {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let counted = counted.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    counted.fetch_add(1, Ordering::SeqCst);
                    let body = r#"{"major":"1","minor":"34","gitVersion":"v1.34.0","gitCommit":"","gitTreeState":"","buildDate":"","goVersion":"","compiler":"","platform":"linux/arm64"}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         connection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        CountingApi { url, requests }
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// A plugin whose first run prints an expired token, so the next request refreshes it, and whose
/// later runs do `then` (shell). Every run adds a line to the `runs` marker file.
fn expiring(then: &str) -> FakePlugin {
    FakePlugin::new(&format!(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         echo run >> \"$PLUGIN_DIR/runs\"\n\
         n=$(wc -l < \"$PLUGIN_DIR/runs\" | tr -d ' ')\n\
         if [ \"$n\" = 1 ]; then\n\
         printf '{{\"status\":{{\"token\":\"t-first\",\
         \"expirationTimestamp\":\"2000-01-01T00:00:00Z\"}}}}'\n\
         exit 0\n\
         fi\n\
         {then}\n"
    ))
}

fn refreshes() -> FakePlugin {
    expiring(
        "printf '{\"status\":{\"token\":\"t-next\",\
         \"expirationTimestamp\":\"2999-01-01T00:00:00Z\"}}'",
    )
}

fn runs(plugin: &FakePlugin) -> usize {
    std::fs::read_to_string(plugin.dir.path().join("runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

fn foreground_only(role: Role) -> CredentialAccess {
    CredentialAccess::new(Arc::new(ForegroundOnly), role)
}

fn is_not_allowed(error: &ClusterError) -> bool {
    matches!(
        error,
        ClusterError::ExecPlugin {
            problem: PluginProblem::NotAllowed,
            ..
        }
    )
}

/// A kubeconfig whose user is an auth provider running a script that counts its runs in `count`
/// and prints a token that is already expired, so that each use runs it.
fn provider_kubeconfig(server: &str, dir: &Path) -> (Kubeconfig, PathBuf) {
    let (script, count) = (dir.join("provider"), dir.join("count"));
    std::fs::write(
        &script,
        format!(
            "echo run >> {count}; printf \
             '{{\"token\":\"t-provider\",\"expiry\":\"2020-01-01T00:00:00Z\"}}'\n",
            count = count.display(),
        ),
    )
    .unwrap();
    let kubeconfig = Kubeconfig::from_yaml(&format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {server}\n\
         users:\n- name: u\n  user:\n    auth-provider:\n      name: gcp\n      config:\n        \
         cmd-path: /bin/sh\n        cmd-args: {script}\n        token-key: '{{.token}}'\n        \
         expiry-key: '{{.expiry}}'\ncontexts:\n- name: ctx\n  context:\n    cluster: c\n    \
         user: u\n",
        script = script.display(),
    ))
    .unwrap();
    (kubeconfig, count)
}

fn count_of(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn always_allow_allows_every_source_in_every_role() {
    for source in [
        CredentialSource::Definition,
        CredentialSource::TokenFile("/token".into()),
        CredentialSource::Plugin,
        CredentialSource::AuthProvider,
    ] {
        assert!(AlwaysAllow.may_run(&source, Role::Foreground));
        assert!(AlwaysAllow.may_run(&source, Role::Background));
    }
}

#[test]
fn foreground_only_stops_what_runs_a_program_in_the_background() {
    let policy = ForegroundOnly;
    for source in [
        CredentialSource::Definition,
        CredentialSource::TokenFile("/token".into()),
    ] {
        assert!(policy.may_run(&source, Role::Foreground));
        assert!(policy.may_run(&source, Role::Background));
    }
    for source in [CredentialSource::Plugin, CredentialSource::AuthProvider] {
        assert!(policy.may_run(&source, Role::Foreground));
        assert!(!policy.may_run(&source, Role::Background));
    }
}

#[tokio::test]
async fn a_grant_follows_the_role() {
    let access = foreground_only(Role::Foreground);
    let grant = access.grant(CredentialSource::Plugin);
    assert!(grant.allowed());
    let revoked = tokio::spawn({
        let grant = grant.clone();
        async move { grant.revoked().await }
    });
    tokio::task::yield_now().await;
    assert!(!revoked.is_finished());
    access.set_role(Role::Background);
    tokio::time::timeout(Duration::from_secs(5), revoked)
        .await
        .unwrap()
        .unwrap();
    assert!(!grant.allowed());
    access.set_role(Role::Foreground);
    assert!(grant.allowed());
}

#[tokio::test]
async fn a_denied_connect_starts_no_plugin() {
    let plugin = refreshes();
    let api = CountingApi::start().await;
    let access = foreground_only(Role::Background);
    let Err(error) = cluster::connect_gated_and_check_with_policy(
        &plugin.kubeconfig(&api.url),
        "ctx",
        Duration::from_secs(5),
        &access,
    )
    .await
    else {
        panic!("a denied connect must fail");
    };
    assert!(is_not_allowed(&error));
    assert!(!plugin.dir.path().join("runs").exists());
    assert_eq!(api.requests(), 0);
}

#[tokio::test]
async fn a_denied_refresh_starts_no_plugin_and_sends_nothing() {
    let plugin = refreshes();
    let api = CountingApi::start().await;
    let access = foreground_only(Role::Foreground);
    let connection =
        cluster::connect_gated_with_policy(&plugin.kubeconfig(&api.url), "ctx", &access)
            .await
            .unwrap_or_else(|_| panic!("the foreground connect runs the plugin"));
    assert_eq!(runs(&plugin), 1);

    access.set_role(Role::Background);
    let error = cluster::server_version(&connection.clients.read)
        .await
        .unwrap_err();
    assert!(is_not_allowed(&error));
    assert!(!error.to_string().contains("t-first"));
    assert_eq!(runs(&plugin), 1);
    assert_eq!(api.requests(), 0);

    // Back in the foreground the same client refreshes.
    access.set_role(Role::Foreground);
    cluster::server_version(&connection.clients.read)
        .await
        .unwrap();
    assert_eq!(runs(&plugin), 2);
    assert_eq!(api.requests(), 1);
}

#[tokio::test]
async fn a_demotion_kills_a_running_refresh() {
    let plugin = expiring(
        "sleep 600 &\n\
         echo $! > \"$PLUGIN_DIR/child.tmp\" && mv \"$PLUGIN_DIR/child.tmp\" \"$PLUGIN_DIR/child\"\n\
         wait\n",
    );
    let api = CountingApi::start().await;
    let access = foreground_only(Role::Foreground);
    let connection =
        cluster::connect_gated_with_policy(&plugin.kubeconfig(&api.url), "ctx", &access)
            .await
            .unwrap_or_else(|_| panic!("the foreground connect runs the plugin"));
    let read = connection.clients.read.clone();
    let request = tokio::spawn(async move { cluster::server_version(&read).await });
    let child_file = plugin.dir.path().join("child");
    for _ in 0..500 {
        if child_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let child: libc::pid_t = std::fs::read_to_string(&child_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let login = plugin.starts()[1];

    access.set_role(Role::Background);

    let error = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(is_not_allowed(&error));
    wait_reaped(login).await;
    wait_dead(child).await;
    assert_eq!(api.requests(), 0);
}

#[tokio::test]
async fn a_denied_write_runs_nothing_and_sends_nothing() {
    let plugin = refreshes();
    let api = CountingApi::start().await;
    let access = foreground_only(Role::Foreground);
    let connection =
        cluster::connect_gated_with_policy(&plugin.kubeconfig(&api.url), "ctx", &access)
            .await
            .unwrap_or_else(|_| panic!("the foreground connect runs the plugin"));
    access.set_role(Role::Background);

    let refused = connection
        .credential
        .present(connection.clients.read.clone())
        .await;
    assert!(matches!(refused, Err(WriteError::IdentityUnverified)));
    assert_eq!(runs(&plugin), 1);
    assert_eq!(api.requests(), 0);
}

#[tokio::test]
async fn a_denied_auth_provider_runs_nothing_and_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let api = CountingApi::start().await;
    let (kubeconfig, count) = provider_kubeconfig(&api.url, dir.path());

    let refused = foreground_only(Role::Background);
    let Err(error) = cluster::connect_gated_with_policy(&kubeconfig, "ctx", &refused).await else {
        panic!("a denied connect must fail");
    };
    assert!(is_not_allowed(&error));
    assert_eq!(count_of(&count), 0);

    let access = foreground_only(Role::Foreground);
    let connection = cluster::connect_gated_with_policy(&kubeconfig, "ctx", &access)
        .await
        .unwrap_or_else(|_| panic!("the foreground connect builds the auth layer"));
    let ran = count_of(&count);
    access.set_role(Role::Background);

    let error = cluster::server_version(&connection.clients.read)
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("t-provider"));
    let write = connection
        .credential
        .present(connection.clients.read.clone())
        .await;
    assert!(matches!(write, Err(WriteError::IdentityUnverified)));
    assert_eq!(count_of(&count), ran);
    assert_eq!(api.requests(), 0);
}

#[tokio::test]
async fn always_allow_refreshes_in_any_role() {
    let plugin = refreshes();
    let api = CountingApi::start().await;
    let access = CredentialAccess::new(Arc::new(AlwaysAllow), Role::Background);
    let (connection, _) = cluster::connect_gated_and_check_with_policy(
        &plugin.kubeconfig(&api.url),
        "ctx",
        Duration::from_secs(10),
        &access,
    )
    .await
    .unwrap_or_else(|_| panic!("every credential may run"));
    assert_eq!(runs(&plugin), 2);
    access.set_role(Role::Foreground);
    access.set_role(Role::Background);
    cluster::server_version(&connection.clients.read)
        .await
        .unwrap();
    assert_eq!(runs(&plugin), 2);
    assert_eq!(api.requests(), 2);
}

#[test]
fn the_not_allowed_error_names_no_command() {
    let text = cluster::ClusterError::kube(kube::Error::Auth(auth_plugin::not_allowed()));
    assert!(is_not_allowed(&text));
    assert!(text.to_string().contains("select the cluster"));
}
