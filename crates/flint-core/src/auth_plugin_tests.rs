use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use kube::config::{KubeConfigOptions, Kubeconfig};
use secrecy::ExposeSecret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::cluster::{self, ClusterError};
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
    assert!(resolve(&mut config).await.unwrap().is_none());
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
    assert!(resolve(&mut config).await.unwrap().is_none());
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
    resolve(&mut config).await.unwrap();
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
    assert!(resolve(&mut config).await.unwrap().is_none());
    assert!(started.elapsed() >= DRAIN_GRACE);
    assert_eq!(config.auth_info.token.unwrap().expose_secret(), "t-held");
    let helper: libc::pid_t = std::fs::read_to_string(plugin.dir.path().join("child"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_dead(helper).await;
}

// covers UP-04: a refresh that hangs is a sign-in failure naming the plugin, not a server
// that cannot be reached. A plugin that may prompt is not run again for a while after that.
#[tokio::test(start_paused = true)]
async fn a_hanging_refresh_is_a_sign_in_failure() {
    let plugin = FakePlugin::new(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         if [ \"$(wc -l < \"$PLUGIN_DIR/starts\" | tr -d ' ')\" = 1 ]; then\n\
         printf '{\"status\":{\"token\":\"t-1\",\"expirationTimestamp\":\"2000-01-01T00:00:00Z\"}}'\n\
         exit 0\n\
         fi\n\
         exec sleep 600\n",
    );
    let mut kubeconfig = plugin.kubeconfig("http://127.0.0.1:1");
    let exec = kubeconfig.auth_infos[0]
        .auth_info
        .as_mut()
        .unwrap()
        .exec
        .as_mut()
        .unwrap();
    exec.interactive_mode = Some(ExecInteractiveMode::IfAvailable);
    let client = cluster::connect(&kubeconfig, "ctx").await.unwrap();
    let Err(err) = cluster::server_version(&client).await else {
        panic!("the refresh hangs");
    };
    assert!(!err.is_timeout(), "{err}");
    assert_eq!(
        err.to_string(),
        "authentication failed: the exec auth plugin `plugin` did not finish within 1m0s; it may \
         be waiting for a login. Run it in a terminal (for example, log in again) and retry."
    );
    let hung = plugin.starts()[1];
    wait_reaped(hung).await;
    // Within the back-off, no new plugin starts (no browser login opens again).
    assert!(cluster::server_version(&client).await.is_err());
    assert_eq!(plugin.starts().len(), 2);
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
                exe.display()
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
    let Ok(out) = cmd
        .env(TTY_CHILD, &done)
        .stdin(std::process::Stdio::null())
        .output()
    else {
        eprintln!("skipped: no `script` to run a terminal with");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        std::fs::read_to_string(&done).is_ok_and(|t| t == "ok"),
        "the terminal run failed:\n{text}"
    );
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
    let plugin = FakePlugin::new(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         tpgid=$(ps -o tpgid= -p $$ | tr -d ' ')\n\
         pgid=$(ps -o pgid= -p $$ | tr -d ' ')\n\
         [ \"$tpgid\" = \"$pgid\" ] && echo yes > \"$PLUGIN_DIR/foreground\"\n\
         sleep 600 &\n\
         echo $! > \"$PLUGIN_DIR/child.tmp\" && mv \"$PLUGIN_DIR/child.tmp\" \"$PLUGIN_DIR/child\"\n\
         wait\n",
    );
    let mut kubeconfig = plugin.kubeconfig("http://127.0.0.1:1");
    kubeconfig.auth_infos[0]
        .auth_info
        .as_mut()
        .unwrap()
        .exec
        .as_mut()
        .unwrap()
        .interactive_mode = Some(ExecInteractiveMode::IfAvailable);
    let attempt = tokio::spawn(async move { cluster::connect(&kubeconfig, "ctx").await });
    let (leader, child) = plugin.wait_started().await;
    assert!(
        std::fs::read_to_string(plugin.dir.path().join("foreground")).is_ok(),
        "the plugin did not get the terminal"
    );
    attempt.abort();
    assert!(attempt.await.is_err_and(|e| e.is_cancelled()));
    wait_reaped(leader).await;
    wait_dead(child).await;
    assert!(foreground(), "the terminal was not given back");
    std::fs::write(done, "ok").unwrap();
}
