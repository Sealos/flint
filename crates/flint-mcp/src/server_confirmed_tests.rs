//! Every tool waits for a confirmed definition while a kubeconfig file before it can't be read.

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
        panic!("a call went through on an unconfirmed definition");
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

// covers UP-54: an inline token is not sent on a definition first seen behind an unreadable file,
// by any tool, and is sent once the file reads and confirms it.
#[tokio::test]
async fn an_inline_token_waits_for_a_confirmed_definition() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let user = "    token: INLINESECRET\n";
    let (server, first) = behind_broken_file(dir.path(), &url, user);

    assert_every_tool_waits(&server, "INLINESECRET", dir.path()).await;
    assert!(requests.lock().unwrap().is_empty(), "nothing was sent");

    std::fs::write(&first, config(&url, user)).unwrap();
    assert_reachable(&server).await;
    let requests = requests.lock().unwrap();
    assert!(
        requests.iter().any(|r| r
            .to_lowercase()
            .contains("authorization: bearer inlinesecret")),
        "{requests:?}"
    );
}

// covers UP-54: a credential command does not even run on a definition first seen behind an
// unreadable file, and runs once the file reads and confirms it.
#[tokio::test]
async fn a_credential_command_waits_for_a_confirmed_definition() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let ran = dir.path().join("ran");
    let plugin = dir.path().join("plugin");
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
    let (server, first) = behind_broken_file(dir.path(), &url, &user);

    assert_every_tool_waits(&server, "COMMANDSECRET", dir.path()).await;
    assert!(!ran.exists(), "the command never ran");
    assert!(requests.lock().unwrap().is_empty(), "nothing was sent");

    std::fs::write(&first, config(&url, &user)).unwrap();
    assert_reachable(&server).await;
    assert!(ran.exists());
    let requests = requests.lock().unwrap();
    assert!(
        requests.iter().any(|r| r
            .to_lowercase()
            .contains("authorization: bearer commandsecret")),
        "{requests:?}"
    );
}
