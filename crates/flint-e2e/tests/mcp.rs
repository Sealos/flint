//! MCP user paths, end to end: Flint's MCP server (`flint-mcp`, the same server as `flint mcp`)
//! over stdio, against fake API servers. No GUI and no cluster, so these run in plain
//! `cargo test` on Linux too. The kind versions are in `crates/flint-mcp/tests/mcp_stdio.rs`.
//!
//! Each test gets scratch kubeconfigs and a scratch config directory; nothing reads the user's
//! own files.

use std::time::Duration;

use anyhow::Result;
use flint_e2e::fake_api::{FakeApiServer, Options, WatchStep, write_kubeconfigs};
use flint_e2e::mcp::Mcp;
use flint_e2e::scratch::Scratch;
use serde_json::{Value, json};

const EXPOSED: &str = "fake-exposed";
const HIDDEN: &str = "fake-hidden";

/// Settings exposing `context` at `server`.
fn exposing(context: &str, server: &str) -> String {
    format!("\n[[ai_exposed_clusters]]\ncontext = \"{context}\"\nserver = \"{server}\"\n")
}

/// Two fake servers in one kubeconfig: `EXPOSED` on `a`, `HIDDEN` on `b`.
struct Two {
    a: FakeApiServer,
    b: FakeApiServer,
    scratch: Scratch,
}

impl Two {
    fn start() -> Result<Two> {
        let a = FakeApiServer::start_blocking(Options::default())?;
        let b = FakeApiServer::start_blocking(Options::default())?;
        let scratch = Scratch::new()?;
        write_kubeconfigs(
            &scratch.path().join("kubeconfig"),
            &[(&a, EXPOSED, None), (&b, HIDDEN, None)],
        )?;
        Ok(Two { a, b, scratch })
    }

    fn mcp(&self) -> Result<Mcp> {
        Mcp::start(
            self.scratch.path().join("kubeconfig"),
            &self.scratch.config_dir(),
        )
    }
}

// covers UP-09
#[test]
fn up_09_agent_lists_only_exposed_clusters() -> Result<()> {
    let two = Two::start()?;
    two.scratch.write_settings("")?;
    let mut mcp = two.mcp()?;
    assert_eq!(
        mcp.tools()?,
        [
            "get_events",
            "get_pod_logs",
            "get_resource",
            "list_clusters",
            "list_resource_kinds",
            "list_resources"
        ]
    );

    // Nothing exposed: no clusters, and a hint pointing at the toggle in Flint.
    let clusters = mcp.call_json("list_clusters", json!({}))?;
    assert_eq!(clusters["clusters"], json!([]), "{clusters}");
    let hint = clusters["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("Expose to AI"), "{clusters}");

    // Exposed in settings (as the app writes it): listed, reachable, with its version.
    two.scratch
        .write_settings(&exposing(EXPOSED, &two.a.url()))?;
    let clusters = mcp.call_json("list_clusters", json!({}))?;
    let listed = clusters["clusters"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{clusters}");
    assert_eq!(listed[0]["name"], EXPOSED);
    assert_eq!(listed[0]["reachable"], true, "{clusters}");
    assert!(listed[0]["server_version"].as_str().is_some(), "{clusters}");
    assert!(clusters.get("hint").is_none(), "{clusters}");

    // The hidden cluster's server never heard from the agent.
    assert!(two.b.requests().is_empty(), "{:?}", two.b.requests());
    Ok(())
}

// covers UP-10
#[test]
fn up_10_agent_inspects_a_cluster() -> Result<()> {
    let two = Two::start()?;
    two.scratch
        .write_settings(&exposing(EXPOSED, &two.a.url()))?;
    // A Secret applied with kubectl: its value also sits in the last-applied annotation.
    let secrets = two.a.gvr("secrets");
    let plaintext = "flint-e2e-plaintext-value";
    let encoded = "ZmxpbnQtZTJlLXBsYWludGV4dC12YWx1ZQ==";
    two.a.set_object(
        &secrets,
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": "applied",
                "namespace": "demo",
                "annotations": {
                    "kubectl.kubernetes.io/last-applied-configuration":
                        format!("{{\"apiVersion\":\"v1\",\"kind\":\"Secret\",\"stringData\":{{\"password\":\"{plaintext}\"}}}}")
                }
            },
            "type": "Opaque",
            "data": {"password": encoded}
        }),
    );
    two.a.set_log(
        "demo",
        "web-69c6f74b8b-qv4xv",
        "starting nginx\nlistening on :80\nGET / 200\n",
    );
    let mut mcp = two.mcp()?;

    let kinds = mcp.call_json("list_resource_kinds", json!({"cluster": EXPOSED}))?;
    assert!(kinds["count"].as_u64().unwrap() > 20, "{kinds}");

    // Pods as kubectl get shows them, then page by page with the continue token.
    let pods = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "po", "namespace": "demo"}),
    )?;
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
    let all: Vec<String> = names(&pods);
    assert_eq!(all.len(), 7, "{pods}");
    assert_eq!(pods["truncated"], false);
    let mut paged = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = mcp.call_json(
            "list_resources",
            json!({"cluster": EXPOSED, "kind": "Pod", "namespace": "demo", "limit": 3, "continue": token}),
        )?;
        paged.extend(names(&page));
        token = page["continue"].as_str().map(str::to_owned);
        assert_eq!(page["truncated"], token.is_some(), "{page}");
        if token.is_none() {
            break;
        }
        assert!(paged.len() < 20, "paging does not end");
    }
    assert_eq!(paged, all);

    // One object, by short kind name, without managedFields.
    let deploy = mcp.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "deploy", "namespace": "demo", "name": "web"}),
    )?;
    assert!(!deploy.is_error, "{}", deploy.text);
    assert!(deploy.text.contains("name: web"), "{}", deploy.text);
    assert!(!deploy.text.contains("managedFields"), "{}", deploy.text);

    // Secret values and their copy in the annotation are redacted.
    let secret = mcp.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "secret", "namespace": "demo", "name": "applied"}),
    )?;
    assert!(!secret.is_error, "{}", secret.text);
    assert!(secret.text.contains("password"), "{}", secret.text);
    assert!(
        !secret.text.contains(encoded) && !secret.text.contains(plaintext),
        "{}",
        secret.text
    );

    // Events and logs.
    let events = mcp.call_json(
        "get_events",
        json!({"cluster": EXPOSED, "namespace": "demo", "object_name": "crash-loop"}),
    )?;
    assert!(
        events.to_string().contains("BackOff"),
        "crash-loop's events: {events}"
    );
    let logs = mcp.call(
        "get_pod_logs",
        json!({"cluster": EXPOSED, "namespace": "demo", "pod": "web-69c6f74b8b-qv4xv"}),
    )?;
    assert!(!logs.is_error, "{}", logs.text);
    assert!(logs.text.contains("listening on :80"), "{}", logs.text);

    // Errors are sentences, not debug dumps.
    let missing = mcp.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "pod", "namespace": "demo", "name": "nope"}),
    )?;
    assert!(missing.is_error);
    assert!(
        !missing.text.contains("Api(") && !missing.text.contains("{ "),
        "{}",
        missing.text
    );
    Ok(())
}

fn names(list: &Value) -> Vec<String> {
    list["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

// covers UP-08, UP-11
#[test]
fn up_11_unexposed_repointed_and_unknown_clusters_look_the_same() -> Result<()> {
    let two = Two::start()?;
    // EXPOSED was exposed at another server: re-pointed, so not exposed. HIDDEN never was.
    two.scratch
        .write_settings(&exposing(EXPOSED, "https://127.0.0.1:1"))?;
    let mut mcp = two.mcp()?;

    let clusters = mcp.call_json("list_clusters", json!({}))?;
    assert_eq!(clusters["clusters"], json!([]), "{clusters}");
    let hint = clusters["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("different API server"), "{clusters}");
    assert!(!clusters.to_string().contains(EXPOSED), "{clusters}");

    let args = |cluster: &str| json!({"cluster": cluster, "kind": "pods", "namespace": "demo"});
    let repointed = mcp.call("list_resources", args(EXPOSED))?;
    let hidden = mcp.call("list_resources", args(HIDDEN))?;
    let unknown = mcp.call("list_resources", args("no-such-cluster"))?;
    assert!(repointed.is_error && hidden.is_error && unknown.is_error);
    let same = |r: &flint_e2e::mcp::ToolResult, name: &str| r.text.replace(name, "X");
    assert_eq!(same(&repointed, EXPOSED), same(&unknown, "no-such-cluster"));
    assert_eq!(same(&hidden, HIDDEN), same(&unknown, "no-such-cluster"));

    // No connection was attempted to either server.
    assert!(two.a.requests().is_empty(), "{:?}", two.a.requests());
    assert!(two.b.requests().is_empty(), "{:?}", two.b.requests());
    Ok(())
}

// covers UP-12
#[test]
fn up_12_settings_edited_by_hand_apply_on_the_next_call() -> Result<()> {
    let two = Two::start()?;
    two.scratch.write_settings("")?;
    let mut mcp = two.mcp()?;
    assert_eq!(
        mcp.call_json("list_clusters", json!({}))?["clusters"],
        json!([])
    );

    two.scratch
        .write_settings(&exposing(HIDDEN, &two.b.url()))?;
    let clusters = mcp.call_json("list_clusters", json!({}))?;
    assert_eq!(clusters["clusters"][0]["name"], HIDDEN, "{clusters}");

    // A malformed file exposes nothing, and says so without quoting the file.
    two.scratch
        .write_settings("ai_exposed_clusters = [\"fake-hidden\"\n")?;
    let broken = mcp.call("list_clusters", json!({}))?;
    assert!(broken.is_error, "{}", broken.text);
    assert!(
        broken.text.contains("settings.toml is invalid"),
        "{}",
        broken.text
    );
    let pods = mcp.call("list_resources", json!({"cluster": HIDDEN, "kind": "pods"}))?;
    assert!(pods.is_error, "{}", pods.text);
    Ok(())
}

// covers UP-10: what the agent reads is current, not a stale copy.
#[test]
fn up_10_agent_reads_current_data() -> Result<()> {
    let two = Two::start()?;
    two.scratch
        .write_settings(&exposing(EXPOSED, &two.a.url()))?;
    let mut mcp = two.mcp()?;
    let pods = two.a.gvr("pods");
    let args = json!({"cluster": EXPOSED, "kind": "pods", "namespace": "demo"});
    assert!(!names(&mcp.call_json("list_resources", args.clone())?).contains(&"fresh".to_owned()));
    let fresh = two
        .a
        .new_row(&pods, Some("demo"), "fresh", &[("Status", "Running")]);
    two.a.send_watch(&pods, WatchStep::Added(fresh));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !names(&mcp.call_json("list_resources", args.clone())?).contains(&"fresh".to_owned()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the new pod never showed"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
