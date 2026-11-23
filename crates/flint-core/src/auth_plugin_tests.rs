use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use kube::config::{KubeConfigOptions, Kubeconfig};
use secrecy::ExposeSecret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::cluster::{self, ClusterError};
use crate::connection::policy::CredentialAccess;
use crate::credential::CredentialSource;
use kube::config::ExecInteractiveMode;

/// A fake exec auth plugin: a shell script in a scratch directory, which it gets as
/// `$PLUGIN_DIR` and where it leaves its traces.
pub(crate) struct FakePlugin {
    pub(crate) dir: tempfile::TempDir,
}

impl FakePlugin {
    /// A plugin that runs `body` (shell).
    pub(crate) fn new(body: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        FakePlugin { dir }
    }

    /// A plugin that never answers, like one waiting for a browser login: it notes its PID in
    /// `starts` (one line per start), starts a child that sleeps, notes that child's PID in
    /// `child`, and waits for it.
    pub(crate) fn hanging() -> Self {
        Self::new(
            "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
             sleep 600 &\n\
             echo $! > \"$PLUGIN_DIR/child.tmp\" && mv \"$PLUGIN_DIR/child.tmp\" \"$PLUGIN_DIR/child\"\n\
             wait\n",
        )
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.dir.path().join("plugin")
    }

    /// The `user:` entry of a kubeconfig that runs this plugin, indented for a `users` item.
    pub(crate) fn user_yaml(&self) -> String {
        format!(
            "    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      \
             command: {}\n      interactiveMode: Never\n      env:\n      \
             - name: PLUGIN_DIR\n        value: {}\n",
            self.path().display(),
            self.dir.path().display()
        )
    }

    /// A kubeconfig whose context `ctx` points at `server` and authenticates with this plugin.
    pub(crate) fn kubeconfig(&self, server: &str) -> Kubeconfig {
        Kubeconfig::from_yaml(&format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {server}\n\
             users:\n- name: u\n  user:\n{}contexts:\n- name: ctx\n  context:\n    cluster: c\n    \
             user: u\n",
            self.user_yaml()
        ))
        .unwrap()
    }

    /// The PIDs of every start so far.
    pub(crate) fn starts(&self) -> Vec<libc::pid_t> {
        std::fs::read_to_string(self.dir.path().join("starts"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect()
    }

    /// Waits for the hanging plugin's sleeping child, then returns the PIDs of the plugin's
    /// first start and of that child.
    pub(crate) async fn wait_started(&self) -> (libc::pid_t, libc::pid_t) {
        let child = self.dir.path().join("child");
        for _ in 0..500 {
            if let Ok(text) = std::fs::read_to_string(&child) {
                return (self.starts()[0], text.trim().parse().unwrap());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the plugin did not start");
    }
}

/// Whether `pid` names a process, zombies included.
fn exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Whether `pid` is running: it exists and is not a zombie. A process Flint did not start is
/// reaped by whoever adopted it, which in a container may be no one, so its zombie stays.
pub(crate) fn live(pid: libc::pid_t) -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        return exists(pid);
    };
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// Waits up to 10 seconds for `gone` to say `pid` is gone.
async fn wait_until_gone(pid: libc::pid_t, gone: impl Fn(libc::pid_t) -> bool) {
    for _ in 0..500 {
        if gone(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("process {pid} is still there");
}

/// Waits until Flint's own child `pid` is gone, reaped included.
pub(crate) async fn wait_reaped(pid: libc::pid_t) {
    wait_until_gone(pid, |pid| !exists(pid)).await;
}

/// Waits until `pid`, started by Flint's child, no longer runs.
pub(crate) async fn wait_dead(pid: libc::pid_t) {
    wait_until_gone(pid, |pid| !live(pid)).await;
}

fn always_allowed() -> CredentialGrant {
    CredentialAccess::always().grant(CredentialSource::Plugin)
}

/// [`resolve_with_grant`] under the policy every connection has today.
async fn resolve(config: &mut kube::Config, context: &str) -> Result<Resolved, AuthError> {
    let grant = CredentialAccess::always().grant(CredentialSource::of(&config.auth_info));
    resolve_with_grant(config, context, &grant).await
}

async fn config(kubeconfig: &Kubeconfig) -> kube::Config {
    let options = KubeConfigOptions {
        context: Some("ctx".into()),
        cluster: None,
        user: None,
    };
    kube::Config::from_custom_kubeconfig(kubeconfig.clone(), &options)
        .await
        .unwrap()
}

/// A plugin that prints `status` as its ExecCredential status.
fn printing(status: &str) -> FakePlugin {
    FakePlugin::new(&format!(
        "echo run >> \"$PLUGIN_DIR/runs\"\n\
         printf '%s' '{{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
         \"kind\":\"ExecCredential\",\"status\":{status}}}'\n"
    ))
}

fn runs(plugin: &FakePlugin) -> usize {
    std::fs::read_to_string(plugin.dir.path().join("runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// An HTTP/1.1 server that answers `/version` on `count` connections, one request each, and
/// returns the `authorization` header of each request.
async fn version_server(count: usize) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "the request ended early");
                request.extend_from_slice(&buf[..n]);
            }
            let request = String::from_utf8_lossy(&request).to_lowercase();
            seen.push(
                request
                    .lines()
                    .find_map(|l| l.strip_prefix("authorization: "))
                    .unwrap_or_default()
                    .to_owned(),
            );
            let body = r#"{"major":"1","minor":"34","gitVersion":"v1.34.0","gitCommit":"","gitTreeState":"","buildDate":"","goVersion":"","compiler":"","platform":"linux/arm64"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\
                 content-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        seen
    });
    (addr, server)
}

// covers UP-04: a plugin waiting for a login that never comes is killed at the connect
// timeout, with the process it started, and reaped; no blocking thread is left waiting for it.
#[tokio::test]
async fn a_hanging_plugin_is_killed_at_the_connect_timeout() {
    let plugin = FakePlugin::hanging();
    let kubeconfig = plugin.kubeconfig("http://127.0.0.1:1");
    let attempt = tokio::spawn(async move {
        cluster::connect_and_check(&kubeconfig, "ctx", Duration::from_secs(2)).await
    });
    let (leader, child) = plugin.wait_started().await;
    assert!(exists(leader) && exists(child));
    let Err(err) = attempt.await.unwrap() else {
        panic!("a hanging plugin cannot connect");
    };
    assert!(err.is_timeout(), "{err}");
    wait_reaped(leader).await;
    wait_dead(child).await;
    assert_eq!(plugin.starts().len(), 1);
}

// covers UP-04: cancelling an attempt (dropping its future) ends the plugin the same way.
#[tokio::test]
async fn a_dropped_attempt_kills_its_plugin() {
    let plugin = FakePlugin::hanging();
    let kubeconfig = plugin.kubeconfig("http://127.0.0.1:1");
    let attempt = tokio::spawn(async move { cluster::connect(&kubeconfig, "ctx").await });
    let (leader, child) = plugin.wait_started().await;
    attempt.abort();
    assert!(attempt.await.is_err_and(|e| e.is_cancelled()));
    wait_reaped(leader).await;
    wait_dead(child).await;
}

// covers UP-03: a token that expires during a session is refreshed by running the plugin
// again, once, and the new token is used from then on.
#[tokio::test]
async fn an_expired_token_is_refreshed_by_running_the_plugin_again() {
    let plugin = FakePlugin::new(
        "echo run >> \"$PLUGIN_DIR/runs\"\n\
         n=$(wc -l < \"$PLUGIN_DIR/runs\" | tr -d ' ')\n\
         if [ \"$n\" = 1 ]; then exp=2000-01-01T00:00:00Z; else exp=2999-01-01T00:00:00Z; fi\n\
         printf '{\"apiVersion\":\"client.authentication.k8s.io/v1\",\"kind\":\"ExecCredential\",\
         \"status\":{\"token\":\"t-%s\",\"expirationTimestamp\":\"%s\"}}' \"$n\" \"$exp\"\n",
    );
    let (server, seen) = version_server(2).await;
    let client = cluster::connect(&plugin.kubeconfig(&server), "ctx")
        .await
        .unwrap();
    assert_eq!(runs(&plugin), 1);
    // The first token is already expired: the first request refreshes it, the second reuses
    // the new one.
    for _ in 0..2 {
        cluster::server_version(&client).await.unwrap();
    }
    assert_eq!(seen.await.unwrap(), ["bearer t-2", "bearer t-2"]);
    assert_eq!(runs(&plugin), 2);
}

// covers UP-03
#[tokio::test]
async fn a_token_without_expiry_is_used_as_it_is() {
    let plugin = printing(r#"{"token":"t-static"}"#);
    let (server, seen) = version_server(2).await;
    let client = cluster::connect(&plugin.kubeconfig(&server), "ctx")
        .await
        .unwrap();
    for _ in 0..2 {
        cluster::server_version(&client).await.unwrap();
    }
    assert_eq!(seen.await.unwrap(), ["bearer t-static", "bearer t-static"]);
    assert_eq!(runs(&plugin), 1);
}

// covers UP-03
#[tokio::test]
async fn a_plugin_certificate_becomes_the_client_identity() {
    let plugin = printing(r#"{"clientCertificateData":"CERT","clientKeyData":"KEY"}"#);
    let mut config = config(&plugin.kubeconfig("https://127.0.0.1:1")).await;
    assert!(resolve(&mut config, "ctx").await.unwrap().token.is_none());
    let auth = &config.auth_info;
    assert!(auth.exec.is_none(), "kube must not run the plugin again");
    assert_eq!(auth.client_certificate_data.as_deref(), Some("Q0VSVA=="));
    assert_eq!(
        auth.client_key_data.as_ref().map(|k| k.expose_secret()),
        Some("S0VZ")
    );
}

// covers UP-03: kube's precedence holds; a token in kubeconfig wins and the plugin never runs.
#[tokio::test]
async fn a_kubeconfig_token_wins_over_the_plugin() {
    let plugin = printing(r#"{"token":"t-plugin"}"#);
    let mut kubeconfig = plugin.kubeconfig("https://127.0.0.1:1");
    kubeconfig.auth_infos[0].auth_info.as_mut().unwrap().token = Some("t-file".into());
    let mut config = config(&kubeconfig).await;
    assert!(resolve(&mut config, "ctx").await.unwrap().token.is_none());
    assert_eq!(runs(&plugin), 0);
    assert!(config.auth_info.exec.is_some());
}

// covers UP-03: the plugin gets what kube passes it.
#[tokio::test]
async fn the_plugin_gets_kubes_arguments_and_environment() {
    let plugin = FakePlugin::new(
        "printf '%s|%s|%s' \"$1\" \"$KUBERNETES_EXEC_INFO\" \"${DROPPED-unset}\" \
         > \"$PLUGIN_DIR/seen\"\n\
         printf '{\"status\":{\"token\":\"t\"}}'\n",
    );
    let mut kubeconfig = plugin.kubeconfig("https://127.0.0.1:1");
    let exec = kubeconfig.auth_infos[0]
        .auth_info
        .as_mut()
        .unwrap()
        .exec
        .as_mut()
        .unwrap();
    exec.args = Some(vec!["get-token".into()]);
    // Set by kubeconfig, then dropped, as kube applies them: `dropEnv` wins.
    exec.env.get_or_insert_default().push(
        [("name", "DROPPED"), ("value", "yes")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    );
    exec.drop_env = Some(vec!["DROPPED".into()]);
    let mut config = config(&kubeconfig).await;
    resolve(&mut config, "ctx").await.unwrap();
    let seen = std::fs::read_to_string(plugin.dir.path().join("seen")).unwrap();
    let (arg, rest) = seen.split_once('|').unwrap();
    let (info, dropped) = rest.rsplit_once('|').unwrap();
    assert_eq!(arg, "get-token");
    assert_eq!(dropped, "unset");
    let info: serde_json::Value = serde_json::from_str(info).unwrap();
    assert_eq!(
        info,
        serde_json::json!({
            "kind": "ExecCredential",
            "apiVersion": "client.authentication.k8s.io/v1",
            "spec": {"interactive": false},
        })
    );
    assert_eq!(config.auth_info.token.unwrap().expose_secret(), "t");
}

// covers UP-04, UP-10: a plugin that fails reads as before, without its output or arguments.
#[tokio::test]
async fn a_failing_plugin_is_reported_without_its_output() {
    let plugin = FakePlugin::new("echo 'sso expired s3cr3t' >&2\necho token-abc\nexit 3\n");
    let Err(err) = cluster::connect(&plugin.kubeconfig("https://127.0.0.1:1"), "ctx").await else {
        panic!("a failing plugin cannot connect");
    };
    let message = err.to_string();
    assert!(
        message.starts_with("authentication failed: the exec auth plugin exited with"),
        "{message}"
    );
    let dir = plugin.dir.path().display().to_string();
    for secret in ["s3cr3t", "token-abc", &dir] {
        assert!(!message.contains(secret), "{message}");
    }
    let missing = FakePlugin::new("");
    std::fs::remove_file(missing.path()).unwrap();
    let Err(err) = cluster::connect(&missing.kubeconfig("https://127.0.0.1:1"), "ctx").await else {
        panic!("a missing plugin cannot connect");
    };
    assert!(
        matches!(err, ClusterError::ExecPluginNotFound { .. }),
        "{err}"
    );
}

// The error value itself, whose `Debug` and `Display` would quote what it holds.
#[tokio::test]
async fn a_failed_run_keeps_only_its_exit_status() {
    let plugin = FakePlugin::new("echo 'sso expired s3cr3t' >&2\necho token-abc\nexit 3\n");
    let mut config = config(&plugin.kubeconfig("https://127.0.0.1:1")).await;
    let Err(error) = resolve(&mut config, "ctx").await else {
        panic!("a failing plugin gives no credential");
    };
    let AuthError::AuthExecRun { out, .. } = &error else {
        panic!("not a failed run: {error:?}");
    };
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    for shown in [format!("{error}"), format!("{error:?}")] {
        assert!(!shown.contains("s3cr3t") && !shown.contains("token-abc"));
    }
}

/// A parsed token and its expiry, if the credential is a token.
fn token(credential: Credential) -> Option<(String, Option<Timestamp>)> {
    match credential {
        Credential::Token { token, expires } => Some((token.expose_secret().to_owned(), expires)),
        _ => None,
    }
}

#[test]
fn credentials_parse_from_json_or_yaml() {
    let parsed = |text: &str| parse(text.as_bytes());
    assert_eq!(
        token(
            parsed(r#"{"status":{"token":"a","expirationTimestamp":"2030-01-02T03:04:05Z"}}"#)
                .unwrap()
        ),
        Some(("a".into(), Some("2030-01-02T03:04:05Z".parse().unwrap())))
    );
    assert_eq!(
        token(parsed("status:\n  token: b\n").unwrap()),
        Some(("b".into(), None))
    );
    let certificate = parsed(r#"{"status":{"clientCertificateData":"C","clientKeyData":"K"}}"#);
    assert!(matches!(
        certificate.unwrap(),
        Credential::Certificate { certificate, key }
            if certificate == "C" && key.expose_secret() == "K"
    ));
    assert!(matches!(
        parsed(r#"{"status":{}}"#).unwrap(),
        Credential::None
    ));
    assert!(matches!(parsed("{}"), Err(AuthError::ExecPluginFailed)));
    assert!(matches!(
        parsed(r#"{"status":{"token":"a","expirationTimestamp":"soon"}}"#),
        Err(AuthError::MalformedTokenExpirationDate(_))
    ));
    assert!(matches!(
        parsed("[not: a credential"),
        Err(AuthError::AuthExecParse(_))
    ));
}

// Debug output never shows a credential.
#[test]
fn credentials_do_not_print_their_secrets() {
    let parsed = parse(br#"{"status":{"token":"t-secret-1"}}"#).unwrap();
    assert!(!format!("{parsed:?}").contains("t-secret-1"));
    let parsed =
        parse(br#"{"status":{"clientCertificateData":"C","clientKeyData":"k-secret-2"}}"#).unwrap();
    assert!(!format!("{parsed:?}").contains("k-secret-2"));
}

// covers UP-04: a proxy is refused before the plugin runs, as kube's builder did, so no login
// opens for a client that cannot be built.
#[tokio::test]
async fn a_proxy_is_refused_before_the_plugin_runs() {
    let plugin = printing(r#"{"token":"t"}"#);
    let mut kubeconfig = plugin.kubeconfig("https://127.0.0.1:1");
    kubeconfig.clusters[0].cluster.as_mut().unwrap().proxy_url =
        Some("http://127.0.0.1:3128".into());
    let Err(err) = cluster::connect(&kubeconfig, "ctx").await else {
        panic!("a proxy is not supported");
    };
    assert!(err.to_string().contains("proxy"), "{err}");
    assert_eq!(runs(&plugin), 0);
}

// covers UP-04: a plugin that prints more than a credential takes is killed and said so.
#[tokio::test]
async fn output_past_the_cap_is_refused() {
    let plugin = FakePlugin::new("head -c 2000000 /dev/zero\n");
    let Err(err) = cluster::connect(&plugin.kubeconfig("https://127.0.0.1:1"), "ctx").await else {
        panic!("too much output");
    };
    assert_eq!(
        err.to_string(),
        "authentication failed: the exec auth plugin `plugin` printed more output than a \
         credential takes"
    );
}

// covers UP-03: something the plugin started that keeps its output open (a cache helper)
// does not cost the credential: after a short grace, what the plugin printed is used, and the
// helper, still in the plugin's group, is ended.
#[tokio::test]
async fn a_helper_holding_the_output_does_not_cost_the_credential() {
    let plugin = FakePlugin::new(
        "printf '{\"status\":{\"token\":\"t-held\"}}'\n\
         sleep 600 &\n\
         echo $! > \"$PLUGIN_DIR/child\"\n",
    );
    let mut config = config(&plugin.kubeconfig("https://127.0.0.1:1")).await;
    let started = std::time::Instant::now();
    assert!(resolve(&mut config, "ctx").await.unwrap().token.is_none());
    assert!(started.elapsed() >= DRAIN_GRACE);
    assert_eq!(config.auth_info.token.unwrap().expose_secret(), "t-held");
    let helper: libc::pid_t = std::fs::read_to_string(plugin.dir.path().join("child"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_dead(helper).await;
}

/// A plugin whose first run prints an expired token (so the first request refreshes), and
/// whose later runs wait for `$PLUGIN_DIR/release` (a login being completed), then print a
/// fresh token `t-<run>`.
fn slow_login() -> FakePlugin {
    FakePlugin::new(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         n=$(wc -l < \"$PLUGIN_DIR/starts\" | tr -d ' ')\n\
         if [ \"$n\" = 1 ]; then\n\
         printf '{\"status\":{\"token\":\"t-1\",\"expirationTimestamp\":\"2000-01-01T00:00:00Z\"}}'\n\
         exit 0\n\
         fi\n\
         while [ ! -e \"$PLUGIN_DIR/release\" ]; do sleep 0.05; done\n\
         printf '{\"status\":{\"token\":\"t-%s\",\"expirationTimestamp\":\"2999-01-01T00:00:00Z\"}}' \"$n\"\n",
    )
}

/// Waits (in real time) until the plugin has started `count` times.
async fn wait_starts(plugin: &FakePlugin, count: usize) {
    for _ in 0..500 {
        if plugin.starts().len() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the plugin did not start {count} times");
}

fn prompting(kubeconfig: &mut Kubeconfig) {
    kubeconfig.auth_infos[0]
        .auth_info
        .as_mut()
        .unwrap()
        .exec
        .as_mut()
        .unwrap()
        .interactive_mode = Some(ExecInteractiveMode::IfAvailable);
}

#[tokio::test(start_paused = true)]
async fn an_always_plugin_queues_before_a_later_if_available_plugin() {
    let queue = Arc::new(tokio::sync::Mutex::new(()));
    let current = queue.lock().await;
    let foreground = std::cell::Cell::new(false);
    let acquire = |must_prompt| {
        Terminal::acquire_on(
            std::fs::File::open("/dev/null").unwrap().into(),
            &queue,
            |_| Some(foreground.get()),
            must_prompt,
        )
    };
    let always = acquire(true);
    let if_available = acquire(false);
    tokio::pin!(always, if_available);
    assert!(always.as_mut().now_or_never().is_none());
    assert!(if_available.as_mut().now_or_never().is_none());

    // The current plugin returns the terminal to Flint and releases the queue.
    foreground.set(true);
    drop(current);
    assert!(if_available.as_mut().now_or_never().is_none());
    let Some(Acquired::Terminal(owner)) = always.as_mut().now_or_never() else {
        panic!("the earlier Always plugin should get the terminal first");
    };
    drop(owner);
    assert!(matches!(if_available.await, Acquired::Terminal(_)));
}

#[tokio::test(start_paused = true)]
async fn an_always_plugin_releases_the_queue_while_waiting_for_the_foreground() {
    let queue = Arc::new(tokio::sync::Mutex::new(()));
    let acquire = |must_prompt| {
        Terminal::acquire_on(
            std::fs::File::open("/dev/null").unwrap().into(),
            &queue,
            |_| Some(false),
            must_prompt,
        )
    };
    let always = acquire(true);
    tokio::pin!(always);
    assert!(always.as_mut().now_or_never().is_none());
    assert!(matches!(
        acquire(false).now_or_never(),
        Some(Acquired::Background)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_refresh_waiting_for_the_foreground_times_out_without_cooldown() {
    let plugin = printing(r#"{"token":"t-new","expirationTimestamp":"2999-01-01T00:00:00Z"}"#);
    let mut exec = config(&plugin.kubeconfig("https://127.0.0.1:1"))
        .await
        .auth_info
        .exec
        .unwrap();
    exec.interactive_mode = Some(ExecInteractiveMode::Always);
    let context = "refresh-waiting-for-foreground".to_owned();
    let state = Arc::new(tokio::sync::Mutex::new(TokenState {
        context: context.clone(),
        exec: exec.clone(),
        token: SecretString::from("t-old"),
        expires: "2000-01-01T00:00:00Z".parse().unwrap(),
        refreshing: None,
        resting: None,
    }));
    let token = RefreshingToken(Arc::new(TokenShared {
        state: state.clone(),
        grant: always_allowed(),
        task: std::sync::Mutex::default(),
    }));
    let waiting_for_foreground = async {
        Terminal::acquire_on(
            std::fs::File::open("/dev/null").unwrap().into(),
            &Arc::new(tokio::sync::Mutex::new(())),
            |_| Some(false),
            true,
        )
        .await
    };
    let Err(error) = token
        .start_refresh(exec, context, waiting_for_foreground)
        .await
    else {
        panic!("the plugin should time out waiting for the foreground");
    };
    assert!(matches!(
        &*error,
        AuthError::AuthExec(message) if message == "flint:timed-out:plugin"
    ));
    assert_eq!(runs(&plugin), 0);
    assert!(state.lock().await.rests_until().is_none());
}

// covers UP-04: a refresh that hangs is a sign-in failure naming the plugin, not a server that
// cannot be reached. A plugin that may prompt then rests: requests say until when, without
// running it, until someone retries.
#[tokio::test]
async fn a_hanging_refresh_is_a_sign_in_failure() {
    let plugin = slow_login();
    let mut kubeconfig = plugin.kubeconfig("http://127.0.0.1:1");
    prompting(&mut kubeconfig);
    let client = cluster::connect(&kubeconfig, "ctx").await.unwrap();
    let request = tokio::spawn({
        let client = client.clone();
        async move { cluster::server_version(&client).await }
    });
    // The refresh's plugin is running; from now on time runs ahead, to its timeout.
    wait_starts(&plugin, 2).await;
    tokio::time::pause();
    let Err(err) = request.await.unwrap() else {
        panic!("the refresh hangs");
    };
    assert!(!err.is_timeout(), "{err}");
    assert_eq!(
        err.to_string(),
        "authentication failed: the exec auth plugin `plugin` did not finish within 1m0s; it may \
         be waiting for a login. Run it in a terminal (for example, log in again) and retry."
    );
    // Real time again: the killed plugin is reaped when the system says so, not by the clock.
    tokio::time::resume();
    wait_reaped(plugin.starts()[1]).await;

    // Resting: no new plugin starts (no browser login opens again), and the error says so.
    let Err(err) = cluster::server_version(&client).await else {
        panic!("the plugin rests");
    };
    let message = err.to_string();
    assert!(
        message.starts_with("authentication failed: the exec auth plugin `plugin` timed out, so it is not run again until ")
            && message.ends_with("; retry to try now"),
        "{message}"
    );
    assert_eq!(plugin.starts().len(), 2);

    // A retry runs it again.
    retry_now("ctx");
    let request = tokio::spawn(async move { cluster::server_version(&client).await });
    wait_starts(&plugin, 3).await;
    request.abort();
}

// covers UP-04: a request that gives up (its own short deadline) does not end a refresh that
// waits for a slow login: the next request waits for the same plugin run, which then succeeds.
#[tokio::test]
async fn a_short_request_does_not_restart_a_slow_login() {
    let plugin = slow_login();
    let (server, seen) = version_server(1).await;
    let client = cluster::connect(&plugin.kubeconfig(&server), "ctx")
        .await
        .unwrap();
    let short = tokio::time::timeout(Duration::from_millis(300), cluster::server_version(&client));
    assert!(short.await.is_err(), "the login is not done yet");
    wait_starts(&plugin, 2).await;
    let login = plugin.starts()[1];
    assert!(exists(login), "the refresh goes on");
    std::fs::write(plugin.dir.path().join("release"), "").unwrap();
    cluster::server_version(&client).await.unwrap();
    assert_eq!(seen.await.unwrap(), ["bearer t-2"]);
    assert_eq!(plugin.starts().len(), 2, "one plugin run for both requests");
}

// covers UP-03: a plugin that is chatty on stderr still signs in: stderr past its cap is
// dropped, not an error.
#[tokio::test]
async fn a_chatty_plugin_still_signs_in() {
    let plugin = FakePlugin::new(
        "head -c 200000 /dev/zero >&2\n\
         printf '{\"status\":{\"token\":\"t-chatty\"}}'\n",
    );
    let mut config = config(&plugin.kubeconfig("https://127.0.0.1:1")).await;
    resolve(&mut config, "ctx").await.unwrap();
    assert_eq!(config.auth_info.token.unwrap().expose_secret(), "t-chatty");
}

/// Set for the copy of this test binary that `script` runs on a terminal.
const TTY_CHILD: &str = "FLINT_TEST_TTY_CHILD";

// covers UP-04: with Flint in the foreground of a terminal, a plugin that may prompt gets the
// terminal (it is the foreground group) and is still killed with what it started; Flint takes
// the terminal back. Runs the test below on a terminal through `script`.
#[test]
fn a_prompting_plugin_on_a_terminal_is_killed_and_the_terminal_returned() {
    let dir = tempfile::tempdir().unwrap();
    let done = dir.path().join("done");
    let exe = std::env::current_exe().unwrap();
    let test = "auth_plugin::tests::on_a_terminal";
    let mut cmd = std::process::Command::new("script");
    if cfg!(target_os = "linux") {
        cmd.args(["-qec"])
            .arg(format!(
                "{} --exact {test} --ignored --test-threads=1",
                shell_quoted(&exe.display().to_string())
            ))
            .arg("/dev/null");
    } else {
        cmd.args(["-q", "/dev/null"]).arg(&exe).args([
            "--exact",
            test,
            "--ignored",
            "--test-threads=1",
        ]);
    }
    let output = dir.path().join("output");
    let Ok(mut run) = cmd
        .env(TTY_CHILD, &done)
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&output).unwrap())
        .spawn()
    else {
        eprintln!("skipped: no `script` to run a terminal with");
        return;
    };
    // A run that hangs fails here instead of never ending. (A write to the terminal that SIGTTOU
    // would stop fails with EIO here instead: under `script` the test's process group is
    // orphaned. Either way the test sees it.)
    let started = std::time::Instant::now();
    while run.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(90) {
            let _ = run.kill();
            panic!(
                "the terminal run did not finish:\n{}",
                std::fs::read_to_string(&output).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = std::fs::read_to_string(&output).unwrap_or_default();
    assert!(
        std::fs::read_to_string(&done).is_ok_and(|t| t == "ok"),
        "the terminal run failed:\n{text}"
    );
    // Plugins without the terminal could not write on it.
    assert!(
        !text.contains(TTY_LEAK),
        "a plugin wrote on the terminal:\n{text}"
    );
}

/// `text` as one shell word.
fn shell_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[test]
fn shell_quoting_keeps_a_path_one_word() {
    assert_eq!(shell_quoted("/a b/it's"), r"'/a b/it'\''s'");
}

/// What plugins that may not use the terminal try to write on it.
const TTY_LEAK: &str = "flint-tty-leak-8471";

/// Shell lines that try to write [`TTY_LEAK`] on the terminal, noting in `$PLUGIN_DIR/tty`
/// whether `/dev/tty` could not be opened.
fn try_the_terminal() -> String {
    format!(
        "if ( echo {TTY_LEAK} > /dev/tty ) 2>/dev/null; then echo wrote > \"$PLUGIN_DIR/tty\"; \
         else echo refused > \"$PLUGIN_DIR/tty\"; fi\n"
    )
}

fn tty_attempt(plugin: &FakePlugin) -> String {
    std::fs::read_to_string(plugin.dir.path().join("tty"))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[tokio::test]
#[ignore = "run on a terminal by a_prompting_plugin_on_a_terminal_is_killed_and_the_terminal_returned"]
async fn on_a_terminal() {
    let Some(done) = std::env::var_os(TTY_CHILD) else {
        return;
    };
    // SAFETY: plain calls on the standard input descriptor.
    let foreground = || unsafe { libc::tcgetpgrp(0) == libc::getpgrp() };
    assert!(foreground(), "not in the terminal's foreground");
    // `stty tostop`: a background write to the terminal stops its writer.
    // SAFETY: an all-zero termios is valid; plain calls on standard input.
    unsafe {
        let mut settings: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(0, &raw mut settings), 0);
        settings.c_lflag |= libc::TOSTOP;
        assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &raw const settings), 0);
    }

    // A prompting plugin that cannot be started never takes the terminal.
    let mut missing = FakePlugin::new("").kubeconfig("http://127.0.0.1:1");
    prompting(&mut missing);
    missing.auth_infos[0]
        .auth_info
        .as_mut()
        .unwrap()
        .exec
        .as_mut()
        .unwrap()
        .command = Some("/nonexistent/flint-test-plugin".into());
    let Err(err) = cluster::connect(&missing, "ctx").await else {
        panic!("a missing plugin cannot connect");
    };
    assert!(
        matches!(err, ClusterError::ExecPluginNotFound { .. }),
        "{err}"
    );
    assert!(foreground(), "a missing plugin took the terminal");

    let plugin = FakePlugin::new(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         fg() { [ \"$(ps -o tpgid= -p $$ | tr -d ' ')\" = \"$(ps -o pgid= -p $$ | tr -d ' ')\" ]; }\n\
         i=0; while ! fg && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done\n\
         fg && echo yes > \"$PLUGIN_DIR/foreground\"\n\
         stty -echo\n\
         sleep 600 &\n\
         echo $! > \"$PLUGIN_DIR/child.tmp\" && mv \"$PLUGIN_DIR/child.tmp\" \"$PLUGIN_DIR/child\"\n\
         wait\n",
    );
    let mut kubeconfig = plugin.kubeconfig("http://127.0.0.1:1");
    prompting(&mut kubeconfig);
    let attempt = tokio::spawn({
        let kubeconfig = kubeconfig.clone();
        async move { cluster::connect(&kubeconfig, "ctx").await }
    });
    let (leader, child) = plugin.wait_started().await;
    assert!(
        std::fs::read_to_string(plugin.dir.path().join("foreground")).is_ok(),
        "the plugin did not get the terminal"
    );
    // Flint logs while the plugin has the terminal: with `tostop`, it must not be stopped for it.
    // Straight to the terminal (the test harness captures `eprintln!`).
    std::io::Write::write_all(
        &mut std::io::stderr(),
        b"flint logs while a plugin has the terminal\n",
    )
    .unwrap();
    // A second prompting plugin (another cluster) waits for the terminal briefly instead of
    // starting in the background; then it runs without prompting, and one that has what it
    // needs answers at once, not after the first one's login.
    let second = FakePlugin::new(&format!(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         printf '%s' \"$KUBERNETES_EXEC_INFO\" > \"$PLUGIN_DIR/info\"\n\
         {}\
         printf '{{\"status\":{{\"token\":\"t-cached\"}}}}'\n",
        try_the_terminal()
    ));
    let mut other = second.kubeconfig("http://127.0.0.1:1");
    prompting(&mut other);
    let mut config = config(&other).await;
    let started = std::time::Instant::now();
    resolve(&mut config, "other").await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= TERMINAL_WAIT && waited < TERMINAL_WAIT + Duration::from_secs(3),
        "{waited:?}"
    );
    assert_eq!(config.auth_info.token.unwrap().expose_secret(), "t-cached");
    let info: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(second.dir.path().join("info")).unwrap())
            .unwrap();
    assert_eq!(info["spec"]["interactive"], false, "it may not prompt");
    assert!(exists(leader), "the first plugin still has the terminal");

    assert_eq!(
        tty_attempt(&second),
        "refused",
        "without the terminal, no /dev/tty"
    );

    // A plugin that never prompts has no terminal at all either.
    let never = FakePlugin::new(&format!(
        "{}printf '{{\"status\":{{\"token\":\"t-never\"}}}}'\n",
        try_the_terminal()
    ));
    let mut never_config = super::tests::config(&never.kubeconfig("http://127.0.0.1:1")).await;
    resolve(&mut never_config, "never").await.unwrap();
    assert_eq!(tty_attempt(&never), "refused");

    // A plugin that must prompt (`Always`) waits for the terminal, past the fallback.
    let always = FakePlugin::new(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         fg() { [ \"$(ps -o tpgid= -p $$ | tr -d ' ')\" = \"$(ps -o pgid= -p $$ | tr -d ' ')\" ]; }\n\
         i=0; while ! fg && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done\n\
         fg && echo yes > \"$PLUGIN_DIR/foreground\"\n\
         printf '{\"status\":{\"token\":\"t-always\"}}'\n",
    );
    let mut must = always.kubeconfig("http://127.0.0.1:1");
    must.auth_infos[0]
        .auth_info
        .as_mut()
        .unwrap()
        .exec
        .as_mut()
        .unwrap()
        .interactive_mode = Some(ExecInteractiveMode::Always);
    let mut must_config = super::tests::config(&must).await;
    let waiting = tokio::spawn(async move {
        resolve(&mut must_config, "always").await.unwrap();
        must_config
            .auth_info
            .token
            .unwrap()
            .expose_secret()
            .to_owned()
    });
    tokio::time::sleep(TERMINAL_WAIT + Duration::from_secs(1)).await;
    assert!(
        always.starts().is_empty(),
        "`Always` did not wait for the terminal"
    );

    attempt.abort();
    assert!(attempt.await.is_err_and(|e| e.is_cancelled()));
    wait_reaped(leader).await;
    wait_dead(child).await;
    assert_eq!(waiting.await.unwrap(), "t-always");
    assert!(
        always.dir.path().join("foreground").exists(),
        "`Always` got the terminal once it was free"
    );
    assert!(foreground(), "the terminal was not given back");
    // The plugin turned echo off (a password prompt) and was killed: echo is back on.
    // SAFETY: an all-zero termios is valid; tcgetattr writes into it.
    let mut now: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: a plain call on standard input, writing into `now`.
    assert_eq!(unsafe { libc::tcgetattr(0, &raw mut now) }, 0);
    assert_ne!(now.c_lflag & libc::ECHO, 0, "echo was left off");

    in_the_background().await;
    std::fs::write(done, "ok").unwrap();
}

/// With Flint a background job of its terminal (another group in the foreground), a plugin that
/// may prompt runs without prompting, and one that must prompt waits until Flint is brought to
/// the foreground (`fg`).
async fn in_the_background() {
    // SAFETY: plain calls on standard input.
    let foreground = || unsafe { libc::tcgetpgrp(0) == libc::getpgrp() };
    let mut other = std::process::Command::new("sleep");
    other.arg("60");
    std::os::unix::process::CommandExt::process_group(&mut other, 0);
    let mut other = other.spawn().unwrap();
    let other_group = libc::pid_t::try_from(other.id()).unwrap();
    set_foreground(0, other_group);
    assert!(!foreground());

    let cached = FakePlugin::new(
        "printf '%s' \"$KUBERNETES_EXEC_INFO\" > \"$PLUGIN_DIR/info\"\n\
         printf '{\"status\":{\"token\":\"t-cached\"}}'\n",
    );
    let mut kubeconfig = cached.kubeconfig("http://127.0.0.1:1");
    prompting(&mut kubeconfig);
    let mut config = super::tests::config(&kubeconfig).await;
    resolve(&mut config, "background").await.unwrap();
    let info: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cached.dir.path().join("info")).unwrap())
            .unwrap();
    assert_eq!(
        info["spec"]["interactive"], false,
        "a background Flint may not prompt"
    );

    let always = FakePlugin::new(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         printf '{\"status\":{\"token\":\"t-always\"}}'\n",
    );
    let mut must = always.kubeconfig("http://127.0.0.1:1");
    must.auth_infos[0]
        .auth_info
        .as_mut()
        .unwrap()
        .exec
        .as_mut()
        .unwrap()
        .interactive_mode = Some(ExecInteractiveMode::Always);
    let mut must_config = super::tests::config(&must).await;
    let waiting = tokio::spawn(async move {
        resolve(&mut must_config, "background").await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        always.starts().is_empty(),
        "`Always` waits for the foreground"
    );
    // It waits without holding the terminal: a plugin that may prompt is not kept waiting for
    // it, and runs without prompting at once (Flint is in the background).
    let mut again = super::tests::config(&kubeconfig).await;
    let started = std::time::Instant::now();
    resolve(&mut again, "background").await.unwrap();
    assert!(started.elapsed() < TERMINAL_WAIT, "{:?}", started.elapsed());
    // `fg`.
    // SAFETY: a plain call.
    set_foreground(0, unsafe { libc::getpgrp() });
    tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("`Always` runs once Flint is in the foreground")
        .unwrap();
    assert_eq!(always.starts().len(), 1);
    let _ = other.kill();
    let _ = other.wait();
}

// covers UP-04: a client that is dropped while its token refreshes kills the refresh's plugin
// (and so gives the terminal back), rather than letting it run on for nobody.
#[tokio::test]
async fn dropping_the_client_ends_its_refresh() {
    let plugin = slow_login();
    let client = cluster::connect(&plugin.kubeconfig("http://127.0.0.1:1"), "ctx")
        .await
        .unwrap();
    let request = tokio::spawn({
        let client = client.clone();
        async move { cluster::server_version(&client).await }
    });
    wait_starts(&plugin, 2).await;
    let login = plugin.starts()[1];
    request.abort();
    let _ = request.await;
    drop(client);
    wait_reaped(login).await;
}

// covers UP-04: a refresh that ended without storing anything (its task panicked) does not
// block the client: the next request starts a new one.
#[tokio::test]
async fn a_refresh_that_died_is_replaced() {
    let plugin = printing(r#"{"token":"t-new","expirationTimestamp":"2999-01-01T00:00:00Z"}"#);
    let exec = config(&plugin.kubeconfig("https://127.0.0.1:1"))
        .await
        .auth_info
        .exec
        .unwrap();
    let died = async { Err(Arc::new(AuthError::ExecPluginFailed)) }
        .boxed()
        .shared();
    let _ = died.clone().await;
    let token = RefreshingToken(Arc::new(TokenShared {
        grant: always_allowed(),
        task: std::sync::Mutex::default(),
        state: Arc::new(tokio::sync::Mutex::new(TokenState {
            context: "ctx".into(),
            exec,
            token: SecretString::from("t-old"),
            expires: "2000-01-01T00:00:00Z".parse().unwrap(),
            refreshing: Some(died),
            resting: None,
        })),
    }));
    let header = token.header().await.unwrap();
    assert_eq!(header.to_str().unwrap(), "Bearer t-new");
    assert_eq!(runs(&plugin), 1);
}

// covers UP-04: retrying one cluster ends its plugin's rest only; another cluster's resting
// plugin keeps resting.
#[test]
fn a_retry_ends_the_rest_of_its_own_cluster_only() {
    let resting = |context: &str| TokenState {
        context: context.into(),
        exec: ExecConfig::default(),
        token: SecretString::from("t"),
        expires: Timestamp::now(),
        refreshing: None,
        resting: Some((
            Instant::now() + PROMPT_BACK_OFF,
            Timestamp::now() + PROMPT_BACK_OFF,
            retry_epoch(context),
        )),
    };
    let (a, b) = (resting("rest-a"), resting("rest-b"));
    retry_now("rest-b");
    assert!(
        a.rests_until().is_some(),
        "selecting b does not end a's rest"
    );
    assert!(b.rests_until().is_none());
    retry_now("rest-a");
    assert!(a.rests_until().is_none());
}

// covers UP-03: a refresh's own result is used, even a token that already expires within the
// refresh margin (as kube does): the margin only decides when to refresh.
// The last check is the one made at the spawn itself, past every earlier wait.
#[tokio::test]
async fn a_lease_ended_before_the_spawn_starts_no_plugin() {
    let plugin = FakePlugin::new("echo run >> \"$0.runs\"\n");
    let exec = ExecConfig {
        command: Some(plugin.path().display().to_string()),
        interactive_mode: Some(ExecInteractiveMode::Never),
        ..ExecConfig::default()
    };
    let access = CredentialAccess::new(
        std::sync::Arc::new(crate::connection::policy::ForegroundOnly),
        crate::connection::policy::Role::Foreground,
    );
    let lease = access.grant(CredentialSource::Plugin).lease();
    access.set_role(crate::connection::policy::Role::Background);

    let mut started = false;
    let result = run(&exec, &lease, std::future::ready(Acquired::None), || {
        started = true
    })
    .await;

    assert!(matches!(result, Err(ref error) if is_not_allowed(error)));
    assert!(!started);
    assert!(!plugin.dir.path().join("plugin.runs").exists());
}

/// A check that holds for its first `allowed` calls.
fn holding_for(allowed: usize) -> cluster::SendCheck {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    cluster::SendCheck::new(move || {
        let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::ready(if call < allowed {
            Ok(())
        } else {
            Err("paused".to_owned())
        }))
    })
}

const EXPIRED_TOKEN: &str = r#"{"token":"t","expirationTimestamp":"2000-01-01T00:00:00Z"}"#;

// covers UP-54: a check that stops holding once setup has begun keeps the plugin from starting.
#[tokio::test]
async fn a_failed_check_during_setup_starts_no_plugin() {
    let plugin = printing(EXPIRED_TOKEN);
    let kubeconfig = plugin.kubeconfig("http://127.0.0.1:1");
    let connected = cluster::connect_checked(&kubeconfig, "ctx", holding_for(1)).await;
    assert!(connected.is_err());
    assert_eq!(runs(&plugin), 0);
}

// covers UP-54: a check that stops holding after a request passed it keeps the refresh that
// request would start from running, and the request ends instead of trying again.
#[tokio::test]
async fn a_failed_check_before_a_refresh_starts_no_plugin() {
    let plugin = printing(EXPIRED_TOKEN);
    let (server, seen) = version_server(1).await;
    let kubeconfig = plugin.kubeconfig(&server);
    // Setup asks three times, and the request once before it reaches the token.
    let client = cluster::connect_checked(&kubeconfig, "ctx", holding_for(4))
        .await
        .unwrap();
    let sent = tokio::time::timeout(Duration::from_secs(5), cluster::server_version(&client));
    assert!(sent.await.unwrap().is_err());
    assert_eq!(runs(&plugin), 1, "only setup ran it");
    assert!(!seen.is_finished(), "nothing was sent");
    seen.abort();
}

// covers UP-54: a pause that lands while a request waits for a refresh stops that request before
// it is sent.
#[tokio::test]
async fn a_failed_check_after_a_refresh_sends_nothing() {
    let plugin = FakePlugin::new(&format!(
        "[ -e \"$PLUGIN_DIR/ran\" ] && touch \"$PLUGIN_DIR/paused\"\n\
         touch \"$PLUGIN_DIR/ran\"\n\
         printf '%s' '{{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
         \"kind\":\"ExecCredential\",\"status\":{EXPIRED_TOKEN}}}'\n"
    ));
    let paused = plugin.dir.path().join("paused");
    let check = cluster::SendCheck::new(move || {
        let holds = !paused.exists();
        Box::pin(std::future::ready(
            holds.then_some(()).ok_or_else(|| "paused".to_owned()),
        ))
    });
    let (server, seen) = version_server(1).await;
    let kubeconfig = plugin.kubeconfig(&server);
    let client = cluster::connect_checked(&kubeconfig, "ctx", check)
        .await
        .unwrap();
    assert!(cluster::server_version(&client).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!seen.is_finished(), "nothing was sent");
    seen.abort();
}

#[tokio::test]
async fn a_refreshed_token_is_used_even_when_it_expires_soon() {
    let plugin = FakePlugin::new(
        "echo run >> \"$PLUGIN_DIR/runs\"\n\
         n=$(wc -l < \"$PLUGIN_DIR/runs\" | tr -d ' ')\n\
         printf '{\"status\":{\"token\":\"t-%s\",\"expirationTimestamp\":\"2000-01-01T00:00:00Z\"}}' \"$n\"\n",
    );
    let (server, seen) = version_server(1).await;
    let client = cluster::connect(&plugin.kubeconfig(&server), "ctx")
        .await
        .unwrap();
    cluster::server_version(&client).await.unwrap();
    assert_eq!(seen.await.unwrap(), ["bearer t-2"]);
}

// covers UP-04: a plugin's own problems keep its name in what the app shows.
#[test]
fn plugin_problems_name_the_plugin_in_the_summary() {
    let err = kube::Error::Auth(AuthError::AuthExec(format!("{TIMED_OUT}kubelogin")));
    let error = ClusterError::kube(err);
    assert!(matches!(
        &error,
        ClusterError::ExecPlugin { command, problem: crate::cluster::PluginProblem::TimedOut }
            if command.as_deref() == Some("kubelogin")
    ));
    assert!(
        error
            .summary()
            .as_str()
            .contains("`kubelogin` did not finish within 1m0s"),
        "{}",
        error.summary().as_str()
    );
    // A command with whitespace is not named, so no argument shows: the generic words.
    let unnamed = ClusterError::kube(kube::Error::Auth(AuthError::AuthExec(TIMED_OUT.to_owned())));
    assert!(
        unnamed
            .summary()
            .as_str()
            .starts_with("authentication failed: the exec auth plugin did not finish within"),
        "{}",
        unnamed.summary().as_str()
    );
    let resting = ClusterError::kube(kube::Error::Auth(resting("kubelogin", Timestamp::now())));
    assert!(
        resting
            .summary()
            .as_str()
            .contains("`kubelogin` timed out, so it is not run again until ")
    );
}
