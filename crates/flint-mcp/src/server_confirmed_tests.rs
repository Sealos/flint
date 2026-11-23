//! While a kubeconfig file can't be read, every tool waits on a context whose confirmed definition
//! differs from the one shown. One unchanged from it, or never confirmed, keeps connecting.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{FlintMcp, ToolError};

const VERSION: &str = r#"{"major":"1","minor":"31","gitVersion":"v1.31.0","gitCommit":"x","gitTreeState":"clean","buildDate":"2026-01-01T00:00:00Z","goVersion":"go1.23","compiler":"gc","platform":"linux/amd64"}"#;

/// A fake API server answering `/version`, keeping the head of every request it gets.
fn fake_api() -> (String, Arc<Mutex<Vec<String>>>) {
    serve_api(false)
}

/// [`fake_api`], answering its first request 503 with a `Retry-After`.
fn fake_api_busy_once() -> (String, Arc<Mutex<Vec<String>>>) {
    serve_api(true)
}

fn serve_api(mut busy: bool) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for mut socket in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut byte = [0; 1];
            while !head.ends_with(b"\r\n\r\n") && matches!(socket.read(&mut byte), Ok(1)) {
                head.push(byte[0]);
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&head).into_owned());
            if std::mem::take(&mut busy) {
                let _ = write!(
                    socket,
                    "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                continue;
            }
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{VERSION}",
                VERSION.len()
            );
        }
    });
    (url, requests)
}

fn config(server: &str, user: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {server}\n\
         contexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n\
         users:\n- name: u\n  user:\n{user}"
    )
}

/// A server exposing `dev`, with `first` unreadable and `second` defining it.
fn behind_broken_file(dir: &Path, server: &str, user: &str) -> (FlintMcp, PathBuf) {
    let (first, second) = (dir.join("first"), dir.join("second"));
    std::fs::write(&first, "clusters: [not valid").unwrap();
    std::fs::write(&second, config(server, user)).unwrap();
    std::fs::write(
        dir.join("settings.toml"),
        format!("[[ai_exposed_clusters]]\ncontext = \"dev\"\nserver = \"{server}\"\n"),
    )
    .unwrap();
    (
        FlintMcp::new(dir.to_owned(), vec![first.clone(), second], None),
        first,
    )
}

fn params<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn assert_waiting(result: Result<String, ToolError>, secret: &str, dir: &Path) {
    let Err(error) = result else {
        panic!("a call went through on a definition that differs from the confirmed one");
    };
    assert!(
        error
            .message
            .contains("waiting for its kubeconfig to be readable"),
        "{}",
        error.message
    );
    assert!(!error.message.contains(secret), "{}", error.message);
    assert!(
        !error.message.contains(&*dir.to_string_lossy()),
        "{}",
        error.message
    );
}

/// Calls every tool that connects and asserts each one waits.
async fn assert_every_tool_waits(server: &FlintMcp, secret: &str, dir: &Path) {
    let listed = server.do_list_clusters().await.unwrap();
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let row = &listed["clusters"][0];
    assert_eq!(row["reachable"], json!(false), "{listed}");
    let error = row["error"].as_str().unwrap();
    assert!(
        error.contains("waiting for its kubeconfig to be readable"),
        "{error}"
    );
    assert!(
        !error.contains(secret) && !error.contains(&*dir.to_string_lossy()),
        "{error}"
    );
    assert!(!listed.to_string().contains(secret), "{listed}");

    let cluster = json!({"cluster": "dev"});
    let kind = json!({"cluster": "dev", "kind": "pods"});
    let named = json!({"cluster": "dev", "kind": "pods", "namespace": "demo", "name": "web"});
    let logs = json!({"cluster": "dev", "namespace": "demo", "pod": "web"});
    assert_waiting(
        server.do_list_kinds(params(cluster.clone())).await,
        secret,
        dir,
    );
    assert_waiting(server.do_list_resources(params(kind)).await, secret, dir);
    assert_waiting(server.do_get_resource(params(named)).await, secret, dir);
    assert_waiting(server.do_get_events(params(cluster)).await, secret, dir);
    assert_waiting(server.do_get_pod_logs(params(logs)).await, secret, dir);
}

async fn assert_reachable(server: &FlintMcp) {
    let listed = server.do_list_clusters().await.unwrap();
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["clusters"][0]["reachable"], json!(true), "{listed}");
}

// covers UP-54: a context first seen behind an unreadable file has no confirmed definition, so
// its fallback connects.
#[tokio::test]
async fn a_first_seen_fallback_connects() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let (server, _first) = behind_broken_file(dir.path(), &url, "    token: fallbacktoken\n");

    assert_reachable(&server).await;
    assert_eq!(sent(&requests, "fallbacktoken"), 1);
}

// covers UP-54: with the first file unreadable, a context whose confirmed definition changed to
// the later file's credential waits and sends nothing, and goes on once the file is back.
#[tokio::test]
async fn a_changed_definition_waits_for_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let (one, two) = (
        config(&url, "    token: firsttoken\n"),
        config(&url, "    token: secondtoken\n"),
    );
    let (server, first) = two_files(dir.path(), &url, &one, &two);
    assert_reachable(&server).await;
    assert_eq!(sent(&requests, "firsttoken"), 1);

    std::fs::write(&first, "clusters: [not valid").unwrap();
    assert_every_tool_waits(&server, "secondtoken", dir.path()).await;
    assert_eq!(requests.lock().unwrap().len(), 1, "nothing more was sent");

    std::fs::write(&first, &one).unwrap();
    assert_reachable(&server).await;
    assert_eq!(sent(&requests, "firsttoken"), 2);
    assert_eq!(sent(&requests, "secondtoken"), 0);
}

// covers UP-54: a context unchanged from its confirmed definition keeps connecting while the
// first file can't be read.
#[tokio::test]
async fn an_unchanged_definition_keeps_connecting() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let same = config(&url, "    token: sametoken\n");
    let (server, first) = two_files(dir.path(), &url, &same, &same);
    assert_reachable(&server).await;

    std::fs::write(&first, "clusters: [not valid").unwrap();
    assert_reachable(&server).await;
    assert_eq!(sent(&requests, "sametoken"), 2);
}

/// A server exposing `dev` from two files, in order.
fn two_files(dir: &Path, server: &str, first: &str, second: &str) -> (FlintMcp, PathBuf) {
    let (first_path, second_path) = (dir.join("first"), dir.join("second"));
    std::fs::write(&first_path, first).unwrap();
    std::fs::write(&second_path, second).unwrap();
    std::fs::write(
        dir.join("settings.toml"),
        format!("[[ai_exposed_clusters]]\ncontext = \"dev\"\nserver = \"{server}\"\n"),
    )
    .unwrap();
    (
        FlintMcp::new(dir.to_owned(), vec![first_path.clone(), second_path], None),
        first_path,
    )
}

fn sent(requests: &Mutex<Vec<String>>, token: &str) -> usize {
    let needle = format!("authorization: bearer {token}");
    let requests = requests.lock().unwrap();
    requests
        .iter()
        .filter(|r| r.to_lowercase().contains(&needle))
        .count()
}

// covers UP-54: a warmed client stops when the first file loses read access, which changes
// neither its time nor its size, and the fallback's token is not sent.
#[tokio::test]
async fn losing_read_access_stops_a_warmed_client() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let (one, two) = (
        config(&url, "    token: firsttoken\n"),
        config(&url, "    token: secondtoken\n"),
    );
    let (server, first) = two_files(dir.path(), &url, &one, &two);
    assert_reachable(&server).await;
    assert_eq!(sent(&requests, "firsttoken"), 1);

    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&first).is_ok() {
        return; // Running as a user that permissions don't apply to.
    }
    assert_every_tool_waits(&server, "secondtoken", dir.path()).await;
    assert_eq!(requests.lock().unwrap().len(), 1, "nothing more was sent");
}

// covers UP-54: a higher-priority link whose target is gone counts as a read failure, so a context
// confirmed from it does not switch to the lower file's token.
#[tokio::test]
async fn a_dangling_link_holds_back_the_lower_file() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let two = config(&url, "    token: LOWERSECRET\n");
    let (server, first) = two_files(dir.path(), &url, &one, &two);
    assert_reachable(&server).await;
    std::fs::remove_file(&first).unwrap();
    std::os::unix::fs::symlink(dir.path().join("unmounted"), &first).unwrap();

    assert_every_tool_waits(&server, "LOWERSECRET", dir.path()).await;
    assert_eq!(requests.lock().unwrap().len(), 1, "nothing more was sent");
}

// covers UP-54: a retry waiting out a 503 does not send a credential that stopped being the
// confirmed one during the wait.
#[tokio::test]
async fn a_retry_rechecks_the_definition() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api_busy_once();
    let (old, replacement) = (
        config(&url, "    token: oldtoken\n"),
        config(&url, "    token: newtoken\n"),
    );
    let (server, first) = two_files(dir.path(), &url, &old, &old);

    let change = async {
        while requests.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        std::fs::write(&first, &replacement).unwrap();
        assert!(server.context().await.is_ok_and(|c| c.paused.is_empty()));
        std::fs::write(&first, "clusters: [not valid").unwrap();
        assert!(
            server
                .context()
                .await
                .is_ok_and(|c| c.paused.contains("dev"))
        );
    };
    let (listed, ()) = tokio::join!(server.do_list_clusters(), change);
    let listed: Value = serde_json::from_str(&listed.unwrap()).unwrap();
    assert_eq!(listed["clusters"][0]["reachable"], json!(false), "{listed}");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(sent(&requests, "oldtoken"), 1, "the retry was not sent");
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// A user entry running a credential command that touches `dir/ran` and prints a token.
fn command_user(dir: &Path) -> (String, PathBuf) {
    let ran = dir.join("ran");
    let plugin = dir.join("plugin");
    std::fs::write(
        &plugin,
        format!(
            "#!/bin/sh\ntouch '{}'\nprintf '%s' '{{\"apiVersion\":\
             \"client.authentication.k8s.io/v1\",\"kind\":\"ExecCredential\",\
             \"status\":{{\"token\":\"COMMANDSECRET\"}}}}'\n",
            ran.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let user = format!(
        "    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      command: {}\n      \
         interactiveMode: Never\n",
        plugin.display()
    );
    (user, ran)
}

// covers UP-54: a context first seen behind an unreadable file may use a fallback credential
// command.
#[tokio::test]
async fn a_first_seen_fallback_command_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let (user, ran) = command_user(dir.path());
    let (server, _first) = behind_broken_file(dir.path(), &url, &user);

    assert_reachable(&server).await;
    assert!(ran.exists());
    assert_eq!(sent(&requests, "commandsecret"), 1);
}

// covers UP-54: a broken earlier file does not switch a working context to a later file's
// credential command at the same URL: it never runs.
#[tokio::test]
async fn a_later_files_command_does_not_replace_a_confirmed_user() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let (user, ran) = command_user(dir.path());
    let one = config(&url, "    token: firsttoken\n");
    let (server, first) = two_files(dir.path(), &url, &one, &config(&url, &user));
    assert_reachable(&server).await;

    std::fs::write(&first, "clusters: [not valid").unwrap();
    assert_every_tool_waits(&server, "COMMANDSECRET", dir.path()).await;
    assert!(!ran.exists(), "the command never ran");
    assert_eq!(requests.lock().unwrap().len(), 1, "nothing more was sent");
}
