use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kube::config::Kubeconfig;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::auth_plugin;
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

// Every clone sees a revocation, and no later role undoes it. A credential that runs nothing is
// not affected.
#[test]
fn a_revoked_access_stays_denied_for_every_clone() {
    let access = foreground_only(Role::Foreground);
    let command = access.grant(CredentialSource::Plugin);
    let cloned = command.clone();
    let lease = command.lease();
    let fixed = access.clone().grant(CredentialSource::Definition);

    access.revoke();
    access.set_role(Role::Foreground);

    assert!(!command.allowed() && !cloned.allowed() && !lease.holds());
    assert!(!access.grant(CredentialSource::Plugin).lease().holds());
    assert!(fixed.allowed());
}

// Only a server answering nothing but 503 since the last run counts as down: one API that is
// missing, as a metrics server can be, does not stop a watch.
#[test]
fn only_503s_since_the_last_run_mean_the_server_is_down() {
    use http::StatusCode as S;
    let kept = CredentialAccess::consented(Arc::new(|| {}), None, PluginRuns::default())
        .grant(CredentialSource::Plugin);
    assert!(!kept.server_down());
    kept.note_response(S::SERVICE_UNAVAILABLE);
    assert!(kept.server_down());
    kept.note_response(S::OK);
    assert!(!kept.server_down(), "another API still answers");
    kept.refresh_starting();
    kept.note_response(S::SERVICE_UNAVAILABLE);
    assert!(kept.server_down(), "an answer from before the run is gone");
    kept.refresh_starting();
    assert!(!kept.server_down());

    let foreground = foreground_only(Role::Foreground).grant(CredentialSource::Plugin);
    foreground.note_response(S::SERVICE_UNAVAILABLE);
    assert!(!foreground.server_down(), "only a kept connection stops");
}

// A pause denies commands, and only a use begun after it may run one.
#[test]
fn a_pause_ends_a_use_and_a_resume_allows_a_new_one() {
    let access = foreground_only(Role::Foreground);
    let grant = access.grant(CredentialSource::Plugin);
    let before = grant.lease();

    access.set_role(Role::Paused);
    assert!(!grant.allowed());
    access.set_role(Role::Foreground);

    assert!(grant.allowed() && grant.lease().holds());
    assert!(!before.holds());
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

#[tokio::test]
async fn a_demotion_undone_at_once_still_ends_the_running_refresh() {
    let plugin = expiring(
        "if [ \"$n\" = 2 ]; then\n\
         sleep 600 &\n\
         echo $! > \"$PLUGIN_DIR/child.tmp\" && mv \"$PLUGIN_DIR/child.tmp\" \"$PLUGIN_DIR/child\"\n\
         wait\n\
         fi\n\
         printf '{\"status\":{\"token\":\"t-next\",\
         \"expirationTimestamp\":\"2999-01-01T00:00:00Z\"}}'",
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

    // With no await between them, a watcher of the role sees only the last value.
    access.set_role(Role::Background);
    access.set_role(Role::Foreground);

    wait_reaped(login).await;
    wait_dead(child).await;
    // The old run is over; being in front again, the request gets a run of its own.
    tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap_or_else(|_| panic!("the foreground request refreshes again"));
    assert_eq!(runs(&plugin), 3);
    assert_eq!(api.requests(), 1);
}

#[tokio::test]
async fn a_plugin_token_with_no_expiry_keeps_reading_after_a_demotion() {
    let plugin = FakePlugin::new(
        "echo run >> \"$PLUGIN_DIR/runs\"\n\
         printf '{\"status\":{\"token\":\"t-lasting\"}}'\n",
    );
    let api = CountingApi::start().await;
    let access = foreground_only(Role::Foreground);
    let connection =
        cluster::connect_gated_with_policy(&plugin.kubeconfig(&api.url), "ctx", &access)
            .await
            .unwrap_or_else(|_| panic!("the foreground connect runs the plugin"));
    access.set_role(Role::Background);

    cluster::server_version(&connection.clients.read)
        .await
        .unwrap_or_else(|_| panic!("a cached credential needs nothing run"));
    assert_eq!(runs(&plugin), 1);
    assert_eq!(api.requests(), 1);

    let write = connection
        .credential
        .present(connection.clients.read.clone())
        .await;
    assert!(matches!(write, Err(WriteError::IdentityUnverified)));
    assert_eq!(runs(&plugin), 1);
}

/// A provider whose first run after a `hold` file appears waits for a `release` file, for at most
/// ten seconds, since a command kube started cannot be killed.
fn held_provider_kubeconfig(server: &str, dir: &Path) -> (Kubeconfig, PathBuf) {
    let (script, count) = (dir.join("provider"), dir.join("count"));
    let (hold, started) = (dir.join("hold"), dir.join("started"));
    let release = dir.join("release");
    std::fs::write(
        &script,
        format!(
            "echo run >> {count}\n\
             if [ -e {hold} ] && [ ! -e {started} ]; then\n\
             : > {started}\n\
             i=0\n\
             while [ ! -e {release} ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done\n\
             fi\n\
             printf '{{\"token\":\"t-provider\",\"expiry\":\"2020-01-01T00:00:00Z\"}}'\n",
            count = count.display(),
            hold = hold.display(),
            started = started.display(),
            release = release.display(),
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

#[tokio::test]
async fn a_demoted_provider_starts_no_queued_command_and_stays_unusable() {
    let dir = tempfile::tempdir().unwrap();
    let api = CountingApi::start().await;
    let (kubeconfig, count) = held_provider_kubeconfig(&api.url, dir.path());
    let access = foreground_only(Role::Foreground);
    let connection = cluster::connect_gated_with_policy(&kubeconfig, "ctx", &access)
        .await
        .unwrap_or_else(|_| panic!("the foreground connect builds the auth layer"));
    let ran = count_of(&count);
    std::fs::write(dir.path().join("hold"), "").unwrap();

    let read = connection.clients.read.clone();
    let running = tokio::spawn(async move { cluster::server_version(&read).await });
    let started = dir.path().join("started");
    for _ in 0..500 {
        if started.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(started.exists());
    let read = connection.clients.read.clone();
    let queued = tokio::spawn(async move { cluster::server_version(&read).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    access.set_role(Role::Background);
    std::fs::write(dir.path().join("release"), "").unwrap();

    for request in [running, queued] {
        let error = tokio::time::timeout(Duration::from_secs(15), request)
            .await
            .unwrap()
            .unwrap()
            .expect_err("a demoted connection sends nothing");
        assert!(is_not_allowed(&error));
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        count_of(&count),
        ran + 1,
        "no command starts after the demotion"
    );

    // Only a new connection may run the command again.
    access.set_role(Role::Foreground);
    let error = cluster::server_version(&connection.clients.read)
        .await
        .unwrap_err();
    assert!(is_not_allowed(&error));
    let write = connection
        .credential
        .present(connection.clients.read.clone())
        .await;
    assert!(matches!(write, Err(WriteError::IdentityUnverified)));
    assert_eq!(count_of(&count), ran + 1);
    assert_eq!(api.requests(), 0);
}

// A request that gave up leaves kube's command running. The next one must not get past the lease
// check and sit on kube's lock, from where it would start the command after a demotion.
#[tokio::test]
async fn a_request_queued_behind_an_abandoned_command_starts_nothing_after_a_demotion() {
    let dir = tempfile::tempdir().unwrap();
    let api = CountingApi::start().await;
    let (kubeconfig, count) = held_provider_kubeconfig(&api.url, dir.path());
    let access = foreground_only(Role::Foreground);
    let connection = cluster::connect_gated_with_policy(&kubeconfig, "ctx", &access)
        .await
        .unwrap_or_else(|_| panic!("the foreground connect builds the auth layer"));
    let ran = count_of(&count);
    std::fs::write(dir.path().join("hold"), "").unwrap();

    let read = connection.clients.read.clone();
    let abandoned = tokio::spawn(async move { cluster::server_version(&read).await });
    let started = dir.path().join("started");
    for _ in 0..500 {
        if started.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(started.exists());
    abandoned.abort();
    let _ = abandoned.await;

    let read = connection.clients.read.clone();
    let queued = tokio::spawn(async move { cluster::server_version(&read).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    access.set_role(Role::Background);
    std::fs::write(dir.path().join("release"), "").unwrap();

    let error = tokio::time::timeout(Duration::from_secs(15), queued)
        .await
        .unwrap()
        .unwrap()
        .expect_err("a demoted connection sends nothing");
    assert!(is_not_allowed(&error));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(count_of(&count), ran + 1, "the queued request ran nothing");
    assert_eq!(api.requests(), 0);
}

// A start that stalls past its check holds up no role change, and nothing starts after one.
#[test]
fn a_role_change_does_not_wait_for_a_stalled_start() {
    let access = foreground_only(Role::Foreground);
    let lease = access.grant(CredentialSource::Plugin).lease();
    let (done, finished) = std::sync::mpsc::channel();
    let demoting = access.clone();
    let landed = lease.start_if_held(|| {
        std::thread::spawn(move || {
            demoting.set_role(Role::Background);
            done.send(()).unwrap();
        });
        finished.recv_timeout(Duration::from_secs(5)).is_ok()
    });
    assert_eq!(landed, Some(true), "the demotion landed during the start");
    assert!(!lease.holds());
    assert_eq!(lease.start_if_held(|| ()), None, "nothing starts after it");
}

// A revocation after the last check outside the task, before the task's first poll, still stops
// the provider's command.
#[tokio::test]
async fn a_revocation_before_the_first_poll_runs_no_provider_command() {
    let dir = tempfile::tempdir().unwrap();
    let api = CountingApi::start().await;
    let (kubeconfig, count) = provider_kubeconfig(&api.url, dir.path());
    let options = kube::config::KubeConfigOptions {
        context: Some("ctx".to_owned()),
        ..Default::default()
    };
    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .unwrap();
    let layer = Arc::new(
        kube::client::ConfigExt::auth_layer(&config)
            .unwrap()
            .expect("the provider has an auth layer"),
    );
    let access = foreground_only(Role::Foreground);
    let lease = access.grant(CredentialSource::AuthProvider).lease();
    let guarded = crate::credential::GuardedProvider::guarding(&layer, &lease)
        .expect("a provider a role can stop is guarded");
    let ran = count_of(&count);

    // On this single-threaded runtime, one poll passes the outer check and spawns the task, which
    // cannot run before this test yields.
    let mut header = Box::pin(guarded.header());
    assert!(futures::poll!(header.as_mut()).is_pending());
    access.revoke();

    assert!(header.await.is_err());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(count_of(&count), ran, "the command never started");
}

#[tokio::test]
async fn always_allow_keeps_an_auth_provider_running_in_any_role() {
    let dir = tempfile::tempdir().unwrap();
    let api = CountingApi::start().await;
    let (kubeconfig, count) = provider_kubeconfig(&api.url, dir.path());
    let access = CredentialAccess::new(Arc::new(AlwaysAllow), Role::Foreground);
    let connection = cluster::connect_gated_with_policy(&kubeconfig, "ctx", &access)
        .await
        .unwrap_or_else(|_| panic!("every credential may run"));
    let ran = count_of(&count);
    access.set_role(Role::Background);
    access.set_role(Role::Foreground);
    access.set_role(Role::Background);
    cluster::server_version(&connection.clients.read)
        .await
        .unwrap_or_else(|_| panic!("every credential may run"));
    assert_eq!(count_of(&count), ran + 1);
    assert_eq!(api.requests(), 1);
}

#[test]
fn the_not_allowed_error_names_no_command() {
    let text = cluster::ClusterError::kube(kube::Error::Auth(auth_plugin::not_allowed()));
    assert!(is_not_allowed(&text));
    assert!(text.to_string().contains("select the cluster"));
}
