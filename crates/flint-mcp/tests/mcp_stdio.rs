//! Drives the MCP server over stdio, as the `flint-mcp` binary: the same server as `flint mcp`,
//! without GPUI, so this runs on Linux CI against kind. The cluster test is ignored by default;
//! run it with `--include-ignored` and `FLINT_TEST_KUBECONFIG` set (see `scripts/dev-cluster.sh`).
//! The server gets that kubeconfig (plus one extra context that is not exposed) and a scratch
//! config dir, never the user's own files. The test also uses `kubectl` with that kubeconfig, only.

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
    /// `None` once closed, which ends the server.
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Server {
    /// Starts `flint-mcp` with only the given kubeconfig list and config dir, and initializes it.
    fn start(kubeconfig: &std::ffi::OsStr, config_dir: &std::path::Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_flint-mcp"))
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
            stdin: Some(stdin),
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
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
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
    /// Closes stdin and expects a clean exit, so the server's shutdown runs in full (and, under
    /// LeakSanitizer, its leak check, which fails the exit status). Kills it if it hangs.
    fn drop(&mut self) {
        self.stdin = None;
        for _ in 0..200 {
            if let Ok(Some(status)) = self.child.try_wait() {
                if !std::thread::panicking() {
                    assert!(status.success(), "flint-mcp exited with {status}");
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !std::thread::panicking() {
            panic!("flint-mcp did not exit within 10 s of stdin closing");
        }
    }
}

/// Settings that expose `context` at `server`, with a fixed PATH so no login shell runs.
fn exposing(context: &str, server: &str) -> String {
    format!(
        "path = \"/usr/bin:/bin\"\n\n[[ai_exposed_clusters]]\ncontext = \"{context}\"\nserver = \"{server}\"\n"
    )
}

/// `serve_stdio_main` logs (here a settings warning and debug lines) to stderr only, and ends
/// when the client closes stdin. No cluster, no network.
#[test]
fn logs_stay_off_stdout_and_eof_ends_the_server() {
    let scratch = tempfile::tempdir().unwrap();
    let config_dir = scratch.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("settings.toml"), "path = [\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_flint-mcp"))
        .env("KUBECONFIG", scratch.path().join("none"))
        .env("FLINT_CONFIG_DIR", &config_dir)
        .env("FLINT_LOG", "debug")
        // Invalid settings carry no `path`; with no usable shell, no login shell runs.
        .env("SHELL", "/nonexistent")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // EOF before `initialize` ends the server with an error, which is logged too.
    assert!(
        out.stdout.is_empty(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Exactly the failure code: LeakSanitizer exits with its own code when the server leaks.
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("using default settings"), "{stderr}");
    assert!(stderr.contains("mcp server failed"), "{stderr}");
}

/// Without `FLINT_LOG` or `RUST_LOG`, the server logs warnings and errors but no INFO lines;
/// `--verbose` adds them. No cluster, no network.
#[test]
fn plain_runs_log_warnings_only_and_verbose_adds_info() {
    let scratch = tempfile::tempdir().unwrap();
    let config_dir = scratch.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    // Invalid settings: a warning to see, and no `path`, so with no usable shell no login
    // shell runs.
    std::fs::write(config_dir.join("settings.toml"), "path = [\n").unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_flint-mcp"))
            .args(args)
            .env("KUBECONFIG", scratch.path().join("none"))
            .env("FLINT_CONFIG_DIR", &config_dir)
            .env_remove("FLINT_LOG")
            .env_remove("RUST_LOG")
            .env("SHELL", "/nonexistent")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    let plain = run(&[]);
    assert!(plain.contains("using default settings"), "{plain}");
    assert!(plain.contains("mcp server failed"), "{plain}");
    assert!(!plain.contains(" INFO "), "{plain}");

    let verbose = run(&["--verbose"]);
    assert!(verbose.contains(" INFO "), "{verbose}");
    assert!(verbose.contains("config directory"), "{verbose}");
}

/// A local port nothing listens on: one the OS just handed out, released again.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Every call lands in the activity log, ended or denied, with what it named and never what came
/// back. No cluster: the exposed context points at a port nothing listens on.
// covers UP-16
#[test]
fn every_call_is_recorded_in_the_activity_log() {
    let scratch = tempfile::tempdir().unwrap();
    let kubeconfig = scratch.path().join("kubeconfig");
    let refusing = format!("https://127.0.0.1:{}", closed_port());
    std::fs::write(
        &kubeconfig,
        format!("apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {refusing}\nusers:\n- name: u\n  user:\n    token: SECRETTOKEN\ncontexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n- name: other\n  context:\n    cluster: c\n    user: u\n"),
    )
    .unwrap();
    let config_dir = scratch.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("settings.toml"),
        exposing("dev", &refusing) + "namespaces = [\"web\"]\n",
    )
    .unwrap();
    let mut server = Server::start(kubeconfig.as_os_str(), &config_dir);

    let clusters = server.call_json("list_clusters", json!({}));
    assert_eq!(clusters["clusters"].as_array().unwrap().len(), 1);
    // Not exposed: denied, though the agent reads "not found".
    let (failed, _) = server.call(
        "list_resources",
        json!({"cluster": "other", "kind": "secrets", "namespace": "web"}),
    );
    assert!(failed);
    // Outside the scope: denied.
    let (failed, _) = server.call(
        "get_pod_logs",
        json!({"cluster": "dev", "namespace": "kube-system", "pod": "etcd-0", "container": "etcd"}),
    );
    assert!(failed);
    // In scope, but the server is unreachable: failed.
    let (failed, _) = server.call(
        "get_resource",
        json!({"cluster": "dev", "kind": "pods", "namespace": "web", "name": "web-0"}),
    );
    assert!(failed);
    drop(server);

    let records = flint_core::activity::read(&config_dir);
    let summary: Vec<_> = records
        .iter()
        .map(|r| {
            (
                r.tool.as_str(),
                r.cluster.as_deref(),
                r.kind.as_deref(),
                r.namespace.as_deref(),
                r.name.as_deref(),
                r.container.as_deref(),
                r.outcome,
            )
        })
        .collect();
    use flint_core::activity::Outcome::{Denied, Failed, Ok};
    assert_eq!(
        summary,
        [
            ("list_clusters", None, None, None, None, None, Ok),
            (
                "list_resources",
                Some("other"),
                Some("secrets"),
                Some("web"),
                None,
                None,
                Denied
            ),
            (
                "get_pod_logs",
                Some("dev"),
                Some("pods"),
                Some("kube-system"),
                Some("etcd-0"),
                Some("etcd"),
                Denied
            ),
            (
                "get_resource",
                Some("dev"),
                Some("pods"),
                Some("web"),
                Some("web-0"),
                None,
                Failed
            ),
        ]
    );
    assert_eq!(records[0].items, Some(1));
    assert!(records.iter().all(|r| r.bytes > 0 && !r.truncated));
    // The server a cluster is exposed with; none for one that is not exposed.
    assert_eq!(records[2].server.as_deref(), Some(refusing.as_str()));
    assert_eq!(records[1].server, None);
    // What came back is never stored.
    let log =
        std::fs::read_to_string(config_dir.join(flint_core::activity::ACTIVITY_FILE)).unwrap();
    for absent in ["SECRETTOKEN", "not found", "reachable", "refused"] {
        assert!(!log.contains(absent), "{absent} in {log}");
    }
}

/// Runs on every `cargo test`: no cluster. The only server named is a local port nothing
/// listens on, which refuses connections, so nothing leaves the machine.
// covers UP-02, UP-08, UP-09, UP-11, UP-12
#[test]
fn mcp_policy_without_a_cluster() {
    let scratch = tempfile::tempdir().unwrap();
    let kubeconfig = scratch.path().join("kubeconfig");
    let refusing = format!("https://127.0.0.1:{}", closed_port());
    std::fs::write(
        &kubeconfig,
        format!("apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {refusing}\nusers:\n- name: u\n  user:\n    token: SECRETTOKEN\ncontexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n- name: other\n  context:\n    cluster: c\n    user: u\n"),
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
    std::fs::write(&settings, exposing("dev", &refusing)).unwrap();
    let clusters = server.call_json("list_clusters", json!({}));
    let listed = clusters["clusters"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{clusters}");
    assert_eq!(listed[0]["name"], "dev");
    assert_eq!(listed[0]["reachable"], false);
    // In the same words the app shows (UP-04).
    assert_eq!(
        listed[0]["error"],
        format!(
            "connection refused by {refusing}; is the API server running, and is the port right?"
        ),
        "{clusters}"
    );
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
    let items = pods["rows"].as_array().unwrap();
    assert!(items.len() >= 3, "{pods}");
    // Table columns, as kubectl get prints them, with the age computed.
    let columns: Vec<&str> = pods["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        &columns[..6],
        ["Name", "Namespace", "Ready", "Status", "Restarts", "Age"],
        "{pods}"
    );
    assert_eq!(items[0]["cells"][1], "demo", "{pods}");
    assert!(
        items[0]["cells"][5]
            .as_str()
            .unwrap()
            .ends_with(['s', 'm', 'h', 'd']),
        "{pods}"
    );
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
        paged.push(page["rows"][0]["name"].as_str().unwrap().to_owned());
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
    let secret_name = secrets["rows"]
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

    // Secret Table rows: type and key count only, never a value.
    let all_secrets = server.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "secrets", "namespace": "demo"}),
    );
    let text = all_secrets.to_string();
    assert!(text.contains(APPLIED_SECRET), "{text}");
    assert!(!text.contains(APPLIED_VALUE), "{text}");
    let secret_columns: Vec<&str> = all_secrets["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(secret_columns, ["Name", "Namespace", "Type", "Data", "Age"]);

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
