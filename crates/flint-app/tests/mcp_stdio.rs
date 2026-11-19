//! Drives `flint mcp` over stdio against the dev cluster. Ignored by default; run it with
//! `--include-ignored` and `FLINT_TEST_KUBECONFIG` set (see `scripts/dev-cluster.sh`). The server
//! gets that kubeconfig (plus one extra context that is not exposed) and a scratch config dir,
//! never the user's own files. The test also uses `kubectl` with that kubeconfig, only.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

const EXPOSED: &str = "kind-flint-dev";
const UNEXPOSED: &str = "kind-flint-dev-unexposed";
/// Exposed by name, but at a server it does not point to: must count as not exposed.
const REPOINTED: &str = "kind-flint-dev-repointed";
/// A Secret created by `kubectl apply`, so it carries the last-applied annotation.
const APPLIED_SECRET: &str = "flint-test-applied";
const APPLIED_VALUE: &str = "flint-test-plaintext-value";

/// The dev cluster's kubeconfig. Panics when unset: this test only runs when asked for.
fn test_kubeconfig() -> std::path::PathBuf {
    match std::env::var_os("FLINT_TEST_KUBECONFIG").filter(|v| !v.is_empty()) {
        Some(p) => p.into(),
        None => panic!(
            "FLINT_TEST_KUBECONFIG must point at the dev cluster's kubeconfig (scripts/dev-cluster.sh)"
        ),
    }
}

/// Runs kubectl against the dev cluster only.
fn kubectl(kubeconfig: &std::path::Path, args: &[&str], stdin: Option<&str>) {
    let mut child = Command::new("kubectl")
        .args(args)
        .env("KUBECONFIG", kubeconfig)
        .stdin(Stdio::piped())
        .spawn()
        .expect("kubectl on PATH");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "kubectl {args:?} failed");
}

/// The API server of the (single) cluster in the dev kubeconfig.
fn server_of(kubeconfig: &std::path::Path) -> String {
    let out = Command::new("kubectl")
        .args([
            "config",
            "view",
            "-o",
            "jsonpath={.clusters[0].cluster.server}",
        ])
        .env("KUBECONFIG", kubeconfig)
        .output()
        .expect("kubectl on PATH");
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Server {
    /// Starts `flint mcp` with only the given kubeconfig list and config dir, and initializes it.
    fn start(kubeconfig: &std::ffi::OsStr, config_dir: &std::path::Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_flint"))
            .arg("mcp")
            .env("KUBECONFIG", kubeconfig)
            .env("FLINT_CONFIG_DIR", config_dir)
            .env("FLINT_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = Server {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        let init = server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "flint-test", "version": "0"}
            }),
        );
        assert_eq!(init["serverInfo"]["name"], "flint");
        server.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(60))
                .expect("response within 60 s");
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout must carry only JSON-RPC ({e}): {line}"));
            if msg["id"] == json!(id) {
                assert!(msg.get("error").is_none(), "{method} failed: {msg}");
                return msg["result"].clone();
            }
        }
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Calls a tool; returns (is_error, text).
    fn call(&mut self, tool: &str, args: Value) -> (bool, String) {
        let result = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        (result["isError"] == json!(true), text)
    }

    fn call_json(&mut self, tool: &str, args: Value) -> Value {
        let (is_error, text) = self.call(tool, args.clone());
        assert!(!is_error, "{tool} {args} failed: {text}");
        serde_json::from_str(&text).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Settings that expose `context` at `server`, with a fixed PATH so no login shell runs.
fn exposing(context: &str, server: &str) -> String {
    format!(
        "path = \"/usr/bin:/bin\"\n\n[[ai_exposed_clusters]]\ncontext = \"{context}\"\nserver = \"{server}\"\n"
    )
}

/// Runs on every `cargo test`: no cluster. The only server named is 127.0.0.1:1, which refuses
/// connections, so nothing leaves the machine.
// covers UP-02, UP-08, UP-09, UP-11, UP-12
#[test]
fn mcp_policy_without_a_cluster() {
    let scratch = tempfile::tempdir().unwrap();
    let kubeconfig = scratch.path().join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: https://127.0.0.1:1\nusers:\n- name: u\n  user:\n    token: SECRETTOKEN\ncontexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n- name: other\n  context:\n    cluster: c\n    user: u\n",
    )
    .unwrap();
    let broken = scratch.path().join("broken");
    std::fs::write(
        &broken,
        "users:\n- name: x\n  user:\n    token: BROKENSECRET\nclusters: [\n",
    )
    .unwrap();
    let config_dir = scratch.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let settings = config_dir.join("settings.toml");
    // `dev` was exposed at another server: it is re-pointed, so not exposed.
    std::fs::write(&settings, exposing("dev", "https://old.invalid:6443")).unwrap();

    let mut list = std::ffi::OsString::from(&kubeconfig);
    list.push(":");
    list.push(&broken);
    let mut server = Server::start(&list, &config_dir);

    let clusters = server.call_json("list_clusters", json!({}));
    assert_eq!(clusters["clusters"], json!([]), "{clusters}");
    let hint = clusters["hint"].as_str().unwrap();
    assert!(
        hint.contains("Expose to AI") && hint.contains("different API server"),
        "{hint}"
    );
    let errors = clusters["kubeconfig_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{clusters}");
    let text = clusters.to_string();
    assert!(
        !text.contains("SECRETTOKEN") && !text.contains("BROKENSECRET"),
        "{text}"
    );
    assert!(
        !text.contains("\"dev\""),
        "a re-pointed cluster was named: {text}"
    );

    let args = |c: &str| json!({"cluster": c, "kind": "pods"});
    let (e1, repointed) = server.call("list_resources", args("dev"));
    let (e2, unexposed) = server.call("list_resources", args("other"));
    let (e3, unknown) = server.call("list_resources", args("nope"));
    assert!(e1 && e2 && e3);
    assert_eq!(repointed.replace("dev", "X"), unknown.replace("nope", "X"));
    assert_eq!(
        unexposed.replace("other", "X"),
        unknown.replace("nope", "X")
    );

    // Settings are re-read on every call: exposing `dev` at its real server shows it at once.
    std::fs::write(&settings, exposing("dev", "https://127.0.0.1:1")).unwrap();
    let clusters = server.call_json("list_clusters", json!({}));
    let listed = clusters["clusters"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{clusters}");
    assert_eq!(listed[0]["name"], "dev");
    assert_eq!(listed[0]["reachable"], false);
    assert!(clusters.get("hint").is_none(), "{clusters}");

    // A malformed file exposes nothing, and the error does not quote the file.
    std::fs::write(
        &settings,
        "path = \"/usr/bin\"\nai_exposed_clusters = [\"dev\"\n",
    )
    .unwrap();
    let (is_error, text) = server.call("list_clusters", json!({}));
    assert!(is_error, "{text}");
    assert!(text.contains("settings.toml is invalid"), "{text}");
    assert!(!text.contains('\n'), "{text}");
}

// covers UP-08, UP-09, UP-10, UP-11
#[test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
fn mcp_over_stdio() {
    let kubeconfig = test_kubeconfig();
    let server = server_of(&kubeconfig);
    // A Secret applied with kubectl: its values also sit in the last-applied annotation.
    kubectl(
        &kubeconfig,
        &["apply", "-f", "-"],
        Some(&format!(
            "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {APPLIED_SECRET}\n  namespace: demo\n  annotations:\n    flint-test/note: {APPLIED_VALUE}-note\nstringData:\n  password: {APPLIED_VALUE}\n"
        )),
    );
    let scratch = tempfile::tempdir().unwrap();
    // Two more contexts for the same cluster. UNEXPOSED is not exposed; REPOINTED is exposed at a
    // server it does not point to. Both must look like they do not exist.
    let extra = scratch.path().join("extra-kubeconfig");
    std::fs::write(
        &extra,
        format!(
            "apiVersion: v1\nkind: Config\ncontexts:\n- name: {UNEXPOSED}\n  context:\n    cluster: {EXPOSED}\n    user: {EXPOSED}\n- name: {REPOINTED}\n  context:\n    cluster: {EXPOSED}\n    user: {EXPOSED}\n"
        ),
    )
    .unwrap();
    let config_dir = scratch.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("settings.toml"),
        format!(
            "path = {:?}\n\n[[ai_exposed_clusters]]\ncontext = \"{EXPOSED}\"\nserver = \"{server}\"\n\n[[ai_exposed_clusters]]\ncontext = \"{REPOINTED}\"\nserver = \"https://127.0.0.1:1\"\n",
            std::env::var("PATH").unwrap_or_default()
        ),
    )
    .unwrap();

    let mut kubeconfig_list = std::ffi::OsString::from(&kubeconfig);
    kubeconfig_list.push(":");
    kubeconfig_list.push(&extra);
    let mut server = Server::start(&kubeconfig_list, &config_dir);

    let tools = server.request("tools/list", json!({}));
    let mut names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "get_events",
            "get_pod_logs",
            "get_resource",
            "list_clusters",
            "list_resource_kinds",
            "list_resources"
        ]
    );
    for tool in tools["tools"].as_array().unwrap() {
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
    }

    // Only the exposed cluster is listed, and it is reachable. The re-pointed one is counted in
    // the hint, without its name.
    let clusters = server.call_json("list_clusters", json!({}));
    let listed = clusters["clusters"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{clusters}");
    assert_eq!(listed[0]["name"], EXPOSED);
    assert_eq!(listed[0]["reachable"], true, "{clusters}");
    let hint = clusters["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("different API server"), "{clusters}");
    assert!(!clusters.to_string().contains(REPOINTED), "{clusters}");

    // Unexposed, re-pointed and unknown clusters fail the same way.
    let args = |cluster: &str| json!({"cluster": cluster, "kind": "pods", "namespace": "demo"});
    let (err1, unexposed) = server.call("list_resources", args(UNEXPOSED));
    let (err2, unknown) = server.call("list_resources", args("no-such-cluster"));
    let (err3, repointed) = server.call("list_resources", args(REPOINTED));
    assert!(err1 && err2 && err3);
    assert_eq!(
        unexposed.replace(UNEXPOSED, "X"),
        unknown.replace("no-such-cluster", "X")
    );
    assert_eq!(
        repointed.replace(REPOINTED, "X"),
        unknown.replace("no-such-cluster", "X")
    );

    let kinds = server.call_json("list_resource_kinds", json!({"cluster": EXPOSED}));
    assert!(kinds["count"].as_u64().unwrap() > 20);

    let pods = server.call_json("list_resources", args(EXPOSED));
    let items = pods["items"].as_array().unwrap();
    assert!(items.len() >= 3, "{pods}");
    assert_eq!(pods["truncated"], false);
    let web_pod = items
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .find(|n| n.starts_with("web-"))
        .expect("a web pod")
        .to_owned();

    // Paging with the continue token visits every pod exactly once.
    let mut paged = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = server.call_json(
            "list_resources",
            json!({"cluster": EXPOSED, "kind": "Pod", "namespace": "demo", "limit": 1, "continue": token}),
        );
        assert_eq!(page["count"], 1, "{page}");
        paged.push(page["items"][0]["name"].as_str().unwrap().to_owned());
        token = page["continue"].as_str().map(str::to_owned);
        assert_eq!(page["truncated"], token.is_some(), "{page}");
        if token.is_none() {
            break;
        }
        assert!(paged.len() <= items.len(), "paging does not end");
    }
    let mut all: Vec<String> = items
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    all.sort();
    paged.sort();
    assert_eq!(paged, all);

    // Secrets: keys kept, values redacted, managedFields gone. The Helm release Secret is found
    // by its labels and name, not by list order.
    let secrets = server.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "secrets", "namespace": "demo", "label_selector": "owner=helm,name=podinfo"}),
    );
    let secret_name = secrets["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .find(|n| n.starts_with("sh.helm.release.v1.podinfo."))
        .expect("the podinfo Helm release Secret in demo")
        .to_owned();
    let (is_error, yaml) = server.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "Secret", "namespace": "demo", "name": secret_name}),
    );
    assert!(!is_error, "{yaml}");
    assert!(yaml.contains("kind: Secret"), "{yaml}");
    assert!(yaml.contains("<redacted>"), "{yaml}");
    assert!(!yaml.contains("managedFields"), "{yaml}");
    // Helm release payloads are long base64 strings; none may survive.
    assert!(
        !yaml.lines().any(|l| l.len() > 200),
        "a long value was not redacted"
    );

    let (is_error, yaml) = server.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "Secret", "namespace": "demo", "name": APPLIED_SECRET}),
    );
    assert!(!is_error, "{yaml}");
    assert!(
        yaml.contains("kubectl.kubernetes.io/last-applied-configuration: <redacted>"),
        "{yaml}"
    );
    assert!(yaml.contains("password: <redacted>"), "{yaml}");
    assert!(
        !yaml.contains(APPLIED_VALUE),
        "a Secret value leaked: {yaml}"
    );
    // The plain value base64-encoded, as it sits in `data`.
    assert!(
        !yaml.contains("ZmxpbnQtdGVzdC1wbGFpbnRleHQtdmFsdWU"),
        "{yaml}"
    );

    // API errors are readable sentences, not debug dumps.
    let (is_error, text) = server.call(
        "get_pod_logs",
        json!({"cluster": EXPOSED, "namespace": "demo", "pod": "no-such-pod"}),
    );
    assert!(is_error);
    assert!(text.starts_with("NotFound: "), "{text}");
    assert!(!text.contains("Status {"), "{text}");

    let (is_error, yaml) = server.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "deployments.apps", "namespace": "demo", "name": "web"}),
    );
    assert!(!is_error, "{yaml}");
    assert!(yaml.contains("replicas: 2") && !yaml.contains("managedFields"));

    let events = server.call_json(
        "get_events",
        json!({"cluster": EXPOSED, "namespace": "demo", "limit": 5}),
    );
    assert!(events["count"].as_u64().unwrap() <= 5);

    let logs = server.call_json(
        "get_pod_logs",
        json!({"cluster": EXPOSED, "namespace": "demo", "pod": web_pod, "tail_lines": 3}),
    );
    assert!(logs["lines"].as_u64().unwrap() <= 3, "{logs}");
    // since_seconds 0 means no time limit, not an invalid request.
    let logs = server.call_json(
        "get_pod_logs",
        json!({"cluster": EXPOSED, "namespace": "demo", "pod": web_pod, "since_seconds": 0}),
    );
    assert!(logs["log"].is_string(), "{logs}");

    drop(server);
    kubectl(
        &kubeconfig,
        &[
            "-n",
            "demo",
            "delete",
            "secret",
            APPLIED_SECRET,
            "--ignore-not-found",
        ],
        None,
    );
}
