//! MCP user paths, end to end: Flint's MCP server (`flint-mcp`, the same server as `flint mcp`)
//! over stdio, against fake API servers. No GUI and no cluster, so these run in plain
//! `cargo test` on Linux too. UP-09 and UP-10 also run on the kind dev cluster with
//! `FLINT_E2E_BACKEND=kind FLINT_TEST_KUBECONFIG=…` (the `cluster-e2e` CI job), so the same
//! assertions hold on the fake and on a real API server. More kind tests are in
//! `crates/flint-mcp/tests/mcp_stdio.rs`.
//!
//! Each test gets scratch kubeconfigs and a scratch config directory; nothing reads the user's
//! own files.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use flint_e2e::backend::{Backend, TestCluster};
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

/// The cluster the agent inspects, from `Backend::from_env()` (the fake API server, or the kind
/// dev cluster with `FLINT_E2E_BACKEND=kind`), beside a fake one it must never reach. UP-09 and
/// UP-10 run on both, so the fake is checked against a real API server (#128).
struct Inspected {
    cluster: TestCluster,
    hidden: FakeApiServer,
    scratch: Scratch,
}

impl Inspected {
    /// `None` when the backend is kind without `FLINT_TEST_KUBECONFIG`: the test is skipped,
    /// with a message.
    fn start() -> Result<Option<Inspected>> {
        let Some(cluster) = Backend::from_env().start()? else {
            return Ok(None);
        };
        let hidden = FakeApiServer::start_blocking(Options::default())?;
        let scratch = Scratch::new()?;
        write_kubeconfigs(&scratch.path().join("hidden"), &[(&hidden, HIDDEN, None)])?;
        Ok(Some(Inspected {
            cluster,
            hidden,
            scratch,
        }))
    }

    /// The inspected cluster's context.
    fn context(&self) -> &str {
        &self.cluster.context
    }

    fn mcp(&self) -> Result<Mcp> {
        let list = std::env::join_paths([
            self.cluster.kubeconfig.clone(),
            self.scratch.path().join("hidden"),
        ])?;
        Mcp::start(list, &self.scratch.config_dir())
    }

    fn expose(&self) -> Result<()> {
        self.scratch
            .write_settings(&exposing(self.context(), &self.cluster.server))
    }

    /// Runs kubectl with `args` (and `stdin`), on kind only: against the test's one-context copy
    /// of the kind kubeconfig, once `TestCluster::verify_local_kind` has checked it is the kind
    /// cluster `kind` knows (no proxy, the same server), so a tunnel to another cluster on
    /// 127.0.0.1 is refused.
    fn kubectl(&self, args: &[&str], stdin: Option<&str>) -> Result<()> {
        self.cluster.verify_local_kind()?;
        let mut child = Command::new("kubectl")
            .arg("--kubeconfig")
            .arg(&self.cluster.kubeconfig)
            .arg("--context")
            .arg(self.context())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;
        if let Some(text) = stdin {
            child
                .stdin
                .take()
                .expect("kubectl's stdin")
                .write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        anyhow::ensure!(status.success(), "kubectl {args:?} failed: {status}");
        Ok(())
    }
}

/// A Secret created as `kubectl apply` leaves it (its value also in the last-applied
/// annotation), under a name of its own, and deleted again on kind when dropped. It never
/// touches an object it did not create.
struct AppliedSecret<'a> {
    on: &'a Inspected,
    /// `flint-test-mcp-<pid>-<millis>`: the cluster-e2e job checks no `flint-test` Secret is
    /// left behind.
    name: String,
}

impl<'a> AppliedSecret<'a> {
    const PLAINTEXT: &'static str = "flint-e2e-plaintext-value";
    const ENCODED: &'static str = "ZmxpbnQtZTJlLXBsYWludGV4dC12YWx1ZQ==";

    fn apply(on: &'a Inspected) -> Result<AppliedSecret<'a>> {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();
        let name = format!("flint-test-mcp-{}-{millis}", std::process::id());
        match on.cluster.fake_server() {
            // What `kubectl apply` leaves on the cluster: the value encoded in `data`, and in
            // plain text in the last-applied annotation.
            Some(fake) => {
                let annotation = format!(
                    "{{\"apiVersion\":\"v1\",\"kind\":\"Secret\",\"stringData\":{{\"password\":\"{}\"}}}}",
                    Self::PLAINTEXT
                );
                let secret = json!({
                    "apiVersion": "v1",
                    "kind": "Secret",
                    "metadata": {
                        "name": name,
                        "namespace": "demo",
                        "annotations": {
                            "kubectl.kubernetes.io/last-applied-configuration": annotation
                        }
                    },
                    "type": "Opaque",
                    "data": {"password": Self::ENCODED}
                });
                fake.set_object(&fake.gvr("secrets"), secret);
            }
            // `create --save-config` writes the annotation as `apply` does, with the plain-text
            // value it was given, and fails rather than change a Secret that already exists.
            None => {
                let secret = json!({
                    "apiVersion": "v1",
                    "kind": "Secret",
                    "metadata": {"name": name, "namespace": "demo"},
                    "type": "Opaque",
                    "stringData": {"password": Self::PLAINTEXT}
                });
                on.kubectl(
                    &["create", "--save-config", "-f", "-"],
                    Some(&secret.to_string()),
                )?;
            }
        }
        Ok(AppliedSecret { on, name })
    }
}

impl Drop for AppliedSecret<'_> {
    fn drop(&mut self) {
        if self.on.cluster.fake_server().is_none() {
            let _ = self.on.kubectl(
                &[
                    "-n",
                    "demo",
                    "delete",
                    "secret",
                    &self.name,
                    "--ignore-not-found",
                ],
                None,
            );
        }
    }
}

// covers UP-10, UP-90: a custom cluster name is only what Flint shows. Agents keep the context
// name, and the custom one does not reach the cluster.
#[test]
fn up_10_a_custom_cluster_name_does_not_change_mcp_output() -> Result<()> {
    let Some(on) = Inspected::start()? else {
        return Ok(());
    };
    on.expose()?;
    let cluster = on.context();
    let mut mcp = on.mcp()?;
    let before = mcp.call_json("list_resources", json!({"cluster": cluster, "kind": "ns"}))?;
    drop(mcp);

    let custom = "custom-name-for-test";
    let settings = format!(
        "{}\n[cluster_names]\n{cluster:?} = {custom:?}\n",
        exposing(cluster, &on.cluster.server)
    );
    on.scratch.write_settings(&settings)?;
    let mut mcp = on.mcp()?;
    let named = mcp.call_json("list_clusters", json!({}))?;
    assert!(named.to_string().contains(cluster), "{named}");
    assert!(!named.to_string().contains(custom), "{named}");
    let after = mcp.call_json("list_resources", json!({"cluster": cluster, "kind": "ns"}))?;
    assert_eq!(after["columns"], before["columns"]);
    let refused = mcp.call("list_resources", json!({"cluster": custom, "kind": "ns"}))?;
    assert!(refused.is_error, "{}", refused.text);
    Ok(())
}

// covers UP-09
#[test]
fn up_09_agent_lists_only_exposed_clusters() -> Result<()> {
    let Some(on) = Inspected::start()? else {
        return Ok(());
    };
    on.scratch.write_settings("")?;
    let mut mcp = on.mcp()?;
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
    on.expose()?;
    let clusters = mcp.call_json("list_clusters", json!({}))?;
    let listed = clusters["clusters"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{clusters}");
    assert_eq!(listed[0]["name"], on.context());
    assert_eq!(listed[0]["reachable"], true, "{clusters}");
    assert!(listed[0]["server_version"].as_str().is_some(), "{clusters}");
    assert!(clusters.get("hint").is_none(), "{clusters}");

    // The hidden cluster's server never heard from the agent.
    assert!(
        on.hidden.arrivals().is_empty(),
        "{:?}",
        on.hidden.arrivals()
    );
    Ok(())
}

// covers UP-10
#[test]
fn up_10_agent_inspects_a_cluster() -> Result<()> {
    let Some(on) = Inspected::start()? else {
        return Ok(());
    };
    on.expose()?;
    let cluster = on.context();
    let applied = AppliedSecret::apply(&on)?;
    if let Some(fake) = on.cluster.fake_server() {
        fake.set_log(
            "demo",
            "web-69c6f74b8b-qv4xv",
            "starting nginx\nlistening on :80\nGET / 200\n",
        );
    }
    let mut mcp = on.mcp()?;

    let kinds = mcp.call_json("list_resource_kinds", json!({"cluster": cluster}))?;
    assert!(kinds["count"].as_u64().unwrap() > 20, "{kinds}");

    // Pods as kubectl get shows them, then page by page with the continue token.
    let pods = mcp.call_json(
        "list_resources",
        json!({"cluster": cluster, "kind": "po", "namespace": "demo"}),
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
    // The recorded demo namespace on the fake; on kind, whatever it holds now.
    if on.cluster.fake_server().is_some() {
        assert_eq!(all.len(), 7, "{pods}");
    } else {
        assert!(all.len() >= 4, "{pods}");
    }
    assert_eq!(pods["truncated"], false);
    let mut paged = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = mcp.call_json(
            "list_resources",
            json!({"cluster": cluster, "kind": "Pod", "namespace": "demo", "limit": 3, "continue": token}),
        )?;
        paged.extend(names(&page));
        token = page["continue"].as_str().map(str::to_owned);
        assert_eq!(page["truncated"], token.is_some(), "{page}");
        if token.is_none() {
            break;
        }
        assert!(paged.len() < 50, "paging does not end");
    }
    assert_eq!(paged, all);

    // One object, by short kind name, without managedFields.
    let deploy = mcp.call(
        "get_resource",
        json!({"cluster": cluster, "kind": "deploy", "namespace": "demo", "name": "web"}),
    )?;
    assert!(!deploy.is_error, "{}", deploy.text);
    assert!(deploy.text.contains("name: web"), "{}", deploy.text);
    assert!(!deploy.text.contains("managedFields"), "{}", deploy.text);

    // Secret values and their copy in the annotation are redacted.
    let secret = mcp.call(
        "get_resource",
        json!({"cluster": cluster, "kind": "secret", "namespace": "demo", "name": applied.name}),
    )?;
    assert!(!secret.is_error, "{}", secret.text);
    assert!(secret.text.contains("password"), "{}", secret.text);
    assert!(
        !secret.text.contains(AppliedSecret::ENCODED)
            && !secret.text.contains(AppliedSecret::PLAINTEXT),
        "{}",
        secret.text
    );

    // Events: crash-loop's back-off (on a fresh kind cluster, once it has crashed twice).
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let events = mcp.call_json(
            "get_events",
            json!({"cluster": cluster, "namespace": "demo", "object_name": "crash-loop"}),
        )?;
        if events.to_string().contains("BackOff") {
            break;
        }
        assert!(Instant::now() < deadline, "crash-loop's events: {events}");
        std::thread::sleep(Duration::from_secs(2));
    }

    // Logs of a web pod.
    let web = all
        .iter()
        .find(|name| name.starts_with("web-"))
        .expect("a web pod in demo");
    let logs = mcp.call(
        "get_pod_logs",
        json!({"cluster": cluster, "namespace": "demo", "pod": web}),
    )?;
    assert!(!logs.is_error, "{}", logs.text);
    if on.cluster.fake_server().is_some() {
        assert!(logs.text.contains("listening on :80"), "{}", logs.text);
    }

    // Errors are sentences, not debug dumps.
    let missing = mcp.call(
        "get_resource",
        json!({"cluster": cluster, "kind": "pod", "namespace": "demo", "name": "nope"}),
    )?;
    assert!(missing.is_error);
    assert!(
        !missing.text.contains("Api(") && !missing.text.contains("{ "),
        "{}",
        missing.text
    );
    Ok(())
}

// covers UP-15
#[test]
fn up_15_credentials_in_logs_and_events_do_not_reach_the_agent() -> Result<()> {
    let two = Two::start()?;
    two.scratch
        .write_settings(&exposing(EXPOSED, &two.a.url()))?;
    let pod = "web-69c6f74b8b-qv4xv";
    let token = "s3cr3t-t0ken-value-0123";
    // Built at run time, so the source holds no access key id.
    let aws_key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
    two.a.set_log(
        "demo",
        pod,
        &format!(
            "starting nginx\nGET /api Authorization: Bearer {token}\nusing key {aws_key} for s3\nlistening on :80\n"
        ),
    );
    let events = two.a.gvr("events");
    let message = "Failed to pull image: https://bot:hunter2@registry.example.com/web: denied";
    // The row list_resources shows (with its Message column), then the object behind it.
    let row = two
        .a
        .new_row(&events, Some("demo"), "web.pull", &[("Message", message)]);
    two.a.set_row(&events, row);
    two.a.set_object(
        &events,
        json!({
            "apiVersion": "v1",
            "kind": "Event",
            "metadata": {"name": "web.pull", "namespace": "demo"},
            "involvedObject": {"kind": "Pod", "name": pod, "namespace": "demo"},
            "reason": "Failed",
            "message": message,
            "type": "Warning",
            "count": 1,
        }),
    );
    // A pod whose container arguments carry a password.
    let pods = two.a.gvr("pods");
    two.a.set_object(
        &pods,
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "api-0", "namespace": "demo"},
            "spec": {"containers": [{
                "name": "web",
                "image": "nginx",
                "command": ["/bin/web", "--db-url=postgres://app:hunter3@db/app"],
                "args": ["--password", "hunter4", "--port", "80"],
            }]},
        }),
    );
    let mut mcp = two.mcp()?;

    let logs = mcp.call_json(
        "get_pod_logs",
        json!({"cluster": EXPOSED, "namespace": "demo", "pod": pod}),
    )?;
    let log = logs["log"].as_str().unwrap_or_default();
    assert_eq!(
        log,
        "starting nginx\nGET /api Authorization: Bearer [REDACTED:authorization]\n\
         using key [REDACTED:aws-access-key-id] for s3\nlistening on :80\n",
        "{logs}"
    );
    assert_eq!(logs["redacted"], 2, "{logs}");

    let got = mcp.call_json(
        "get_events",
        json!({"cluster": EXPOSED, "namespace": "demo", "object_name": pod}),
    )?;
    let messages: Vec<&str> = got["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["message"].as_str())
        .collect();
    assert!(
        messages.contains(
            &"Failed to pull image: https://bot:[REDACTED:password]@registry.example.com/web: denied"
        ),
        "{got}"
    );
    assert_eq!(got["redacted"], 1, "{got}");

    // The same message through the other ways an agent can read it.
    let listed = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "events", "namespace": "demo"}),
    )?;
    let event = mcp.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "event", "namespace": "demo", "name": "web.pull"}),
    )?;
    assert!(!event.is_error, "{}", event.text);
    assert!(event.text.starts_with("# redacted=1"), "{}", event.text);
    let spec = mcp.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "pod", "namespace": "demo", "name": "api-0"}),
    )?;
    assert!(!spec.is_error, "{}", spec.text);
    assert!(spec.text.starts_with("# redacted=2"), "{}", spec.text);
    assert!(
        spec.text
            .contains("--db-url=postgres://app:[REDACTED:password]@db/app"),
        "{}",
        spec.text
    );
    assert!(spec.text.contains("--port"), "{}", spec.text);
    for text in [
        logs.to_string(),
        got.to_string(),
        listed.to_string(),
        event.text,
        spec.text,
    ] {
        for secret in [token, aws_key.as_str(), "hunter2", "hunter3", "hunter4"] {
            assert!(!text.contains(secret), "{secret} reached the agent: {text}");
        }
    }
    assert!(
        listed.to_string().contains("[REDACTED:password]"),
        "{listed}"
    );
    Ok(())
}

// covers UP-10, UP-15
#[test]
fn up_10_names_that_are_not_single_path_segments_are_never_sent() -> Result<()> {
    let two = Two::start()?;
    two.scratch
        .write_settings(&exposing(EXPOSED, &two.a.url()))?;
    let encoded = "ZmxpbnQtZTJlLXBhdGgtc2VjcmV0";
    two.a.set_object(
        &two.a.gvr("secrets"),
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": "db", "namespace": "demo"},
            "type": "Opaque",
            "data": {"password": encoded}
        }),
    );
    let mut mcp = two.mcp()?;

    // Each bad call, with what it answers and the not-found answer it must match: `ns` or
    // `pod`, with the name it quotes. Made first, on a cold discovery cache.
    let get = |kind: &str, namespace: Option<&str>, name: &str| json!({"cluster": EXPOSED, "kind": kind, "namespace": namespace, "name": name});
    let logs = |namespace: &str, pod: &str, container: Option<&str>| json!({"cluster": EXPOSED, "namespace": namespace, "pod": pod, "container": container});
    let calls = [
        (
            "get_resource",
            get("namespaces", None, "demo/secrets"),
            "ns",
            "demo/secrets",
        ),
        (
            "get_resource",
            get("namespaces", None, "demo%2Fsecrets"),
            "ns",
            "demo%2Fsecrets",
        ),
        (
            "get_resource",
            get("namespaces", None, "demo/secrets?x=1"),
            "ns",
            "demo/secrets?x=1",
        ),
        ("get_resource", get("namespaces", None, ".."), "ns", ".."),
        (
            "get_resource",
            get("pods", Some("demo"), "x/../db"),
            "pod",
            "x/../db",
        ),
        (
            "get_resource",
            get("pods", Some("demo"), "web#frag"),
            "pod",
            "web#frag",
        ),
        (
            "get_resource",
            get("pods", Some("demo/secrets"), "db"),
            "pod",
            "db",
        ),
        ("get_resource", get("pods", Some(".."), "db"), "pod", "db"),
        (
            "get_pod_logs",
            logs("demo", "db/../../secrets", None),
            "pod",
            "db/../../secrets",
        ),
        (
            "get_pod_logs",
            logs("demo/secrets", "db", None),
            "pod",
            "db",
        ),
        (
            "get_pod_logs",
            logs("demo", "%2E%2E", None),
            "pod",
            "%2E%2E",
        ),
        (
            "list_resources",
            json!({"cluster": EXPOSED, "kind": "pods", "namespace": "demo/secrets"}),
            "ns",
            "demo/secrets",
        ),
        (
            "get_events",
            json!({"cluster": EXPOSED, "namespace": "../demo"}),
            "ns",
            "../demo",
        ),
    ];
    let mut answers = Vec::new();
    for (tool, args, _, _) in &calls {
        let got = mcp.call(tool, args.clone())?;
        assert!(got.is_error, "{args}: {}", got.text);
        answers.push(got.text);
    }
    let nodes = mcp.call(
        "get_resource",
        get("nodes", None, "kind-control-plane/proxy/pods"),
    )?;
    assert!(nodes.is_error, "{}", nodes.text);
    assert!(nodes.text.starts_with("NotFound: nodes "), "{}", nodes.text);
    let container = mcp.call(
        "get_pod_logs",
        logs("demo", "web-69c6f74b8b-qv4xv", Some("web&follow=true")),
    )?;
    assert!(container.is_error, "{}", container.text);
    assert_eq!(
        container.text,
        "BadRequest: container \"web&follow=true\" is not valid for pod \"web-69c6f74b8b-qv4xv\""
    );
    // Not one request reached the server, not even discovery.
    assert!(two.a.arrivals().is_empty(), "{:?}", two.a.arrivals());

    // What a missing object looks like, from the server itself.
    let mut missing = |args: Value| -> Result<String> {
        let got = mcp.call("get_resource", args)?;
        assert!(got.is_error, "{}", got.text);
        Ok(got.text)
    };
    let missing_ns = missing(get("namespaces", None, "nope"))?;
    let missing_pod = missing(get("pods", Some("demo"), "nope"))?;
    assert_eq!(missing_ns, "NotFound: namespaces \"nope\" not found");
    assert_eq!(missing_pod, "NotFound: pods \"nope\" not found");
    for ((tool, args, like, name), answer) in calls.iter().zip(&answers) {
        let expected = if *like == "ns" {
            &missing_ns
        } else {
            &missing_pod
        };
        assert_eq!(
            answer,
            &expected.replace("\"nope\"", &format!("{name:?}")),
            "{tool} {args}"
        );
    }

    // The Secret itself is still readable by name, redacted.
    let secret = mcp.call("get_resource", get("secret", Some("demo"), "db"))?;
    assert!(!secret.is_error, "{}", secret.text);
    assert!(!secret.text.contains(encoded), "{}", secret.text);
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

/// Settings exposing `context` at `server`, limited to `namespaces` and `kinds`.
fn exposing_scoped(context: &str, server: &str, namespaces: &[&str], kinds: &[&str]) -> String {
    format!(
        "{}namespaces = {namespaces:?}\nkinds = {kinds:?}\n",
        exposing(context, server)
    )
}

// covers UP-14: a cluster limited to a namespace and kinds shows only those. Every request
// beyond the scope (another namespace, another kind, a cluster-scoped kind, a missing kind)
// fails with exactly the error of a cluster that is not exposed, so nothing can be probed.
// Lists across all namespaces, events and logs stay in the scope.
#[test]
fn up_14_scoped_exposure_shows_only_its_namespaces_and_kinds() -> Result<()> {
    let two = Two::start()?;
    write_kubeconfigs(
        &two.scratch.path().join("kubeconfig"),
        &[
            (&two.a, EXPOSED, Some("kube-system")),
            (&two.b, HIDDEN, None),
        ],
    )?;
    two.a.set_log("demo", "web-69c6f74b8b-qv4xv", "GET / 200\n");
    two.scratch
        .write_settings(&exposing_scoped(EXPOSED, &two.a.url(), &["demo"], &[]))?;
    let mut mcp = two.mcp()?;

    let clusters = mcp.call_json("list_clusters", json!({}))?;
    assert_eq!(
        clusters["clusters"][0]["scope"],
        json!({"namespaces": ["demo"], "kinds": []}),
        "{clusters}"
    );
    // Names go first, scoped or not: an object name or Kind that could add a field selector of
    // its own is not found, and nothing is sent.
    let before = two.a.requests().len();
    for args in [
        json!({"cluster": EXPOSED, "object_name": "a,involvedObject.namespace=kube-system"}),
        json!({"cluster": EXPOSED, "object_kind": "Pod,involvedObject.namespace=kube-system"}),
        json!({"cluster": EXPOSED, "namespace": "demo/../kube-system"}),
    ] {
        let got = mcp.call("get_events", args.clone())?;
        assert!(
            got.is_error && got.text.starts_with("NotFound:"),
            "{args}: {}",
            got.text
        );
    }
    assert_eq!(two.a.requests().len(), before, "nothing was sent");
    // The context's default namespace is out of scope: not named.
    assert_eq!(
        clusters["clusters"][0]["default_namespace"],
        Value::Null,
        "{clusters}"
    );

    // The not-found every out-of-scope request must match: an unknown cluster's, named alike.
    let unknown = mcp.call(
        "list_resources",
        json!({"cluster": "no-such-cluster", "kind": "pods", "namespace": "demo"}),
    )?;
    assert!(unknown.is_error);
    let expected = unknown.text.replace("no-such-cluster", EXPOSED);
    let mut refused = |tool: &str, args: Value| -> Result<()> {
        let got = mcp.call(tool, args.clone())?;
        assert!(got.is_error, "{tool} {args}: {}", got.text);
        assert_eq!(got.text, expected, "{tool} {args}");
        Ok(())
    };
    let pods_in = |ns: &str| json!({"cluster": EXPOSED, "kind": "pods", "namespace": ns});
    refused("list_resources", pods_in("kube-system"))?;
    refused(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "pods", "namespace": "kube-system", "name": "x"}),
    )?;
    // Cluster-scoped kinds are hidden under a namespace scope, as is a kind that does not exist.
    refused(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "nodes"}),
    )?;
    refused(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "namespaces", "name": "kube-system"}),
    )?;
    refused(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "gizmos"}),
    )?;
    // Events are read through get_events only, which keeps them in scope.
    refused(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "events", "namespace": "demo"}),
    )?;
    refused(
        "get_events",
        json!({"cluster": EXPOSED, "namespace": "default"}),
    )?;
    refused(
        "get_events",
        json!({"cluster": EXPOSED, "object_kind": "Node"}),
    )?;
    refused(
        "get_pod_logs",
        json!({"cluster": EXPOSED, "namespace": "kube-system", "pod": "x"}),
    )?;

    let kinds = mcp.call_json("list_resource_kinds", json!({"cluster": EXPOSED}))?;
    let kinds: Vec<&Value> = kinds["kinds"].as_array().unwrap().iter().collect();
    assert!(kinds.iter().any(|k| k["kind"] == "Pod"));
    assert!(kinds.iter().all(|k| k["namespaced"] == true), "{kinds:?}");

    // All namespaces: only the scope's.
    let pods = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "pods"}),
    )?;
    let rows = pods["rows"].as_array().unwrap();
    assert!(!rows.is_empty(), "{pods}");
    assert!(rows.iter().all(|r| r["namespace"] == "demo"), "{pods}");
    assert_eq!(pods["continue"], Value::Null);
    let events = mcp.call_json("get_events", json!({"cluster": EXPOSED}))?;
    let events = events["events"].as_array().unwrap();
    assert!(!events.is_empty());
    assert!(
        events.iter().all(
            |e| e["namespace"] == "demo" && !e["object"].as_str().unwrap().starts_with("Node/")
        ),
        "{events:?}"
    );
    let logs = mcp.call_json(
        "get_pod_logs",
        json!({"cluster": EXPOSED, "namespace": "demo", "pod": "web-69c6f74b8b-qv4xv"}),
    )?;
    assert_eq!(logs["log"], "GET / 200\n");

    // Kinds named: only those, a cluster-scoped one included because it is named. Logs need pods.
    two.scratch.write_settings(&exposing_scoped(
        EXPOSED,
        &two.a.url(),
        &["demo"],
        &["nodes", "configmaps"],
    ))?;
    let kinds = mcp.call_json("list_resource_kinds", json!({"cluster": EXPOSED}))?;
    let mut names: Vec<&str> = kinds["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["plural"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["configmaps", "nodes"]);
    let nodes = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "nodes"}),
    )?;
    assert!(!nodes["rows"].as_array().unwrap().is_empty(), "{nodes}");
    let expected = unknown.text.replace("no-such-cluster", EXPOSED);
    for (tool, args) in [
        ("list_resources", pods_in("demo")),
        (
            "get_pod_logs",
            json!({"cluster": EXPOSED, "namespace": "demo", "pod": "web-69c6f74b8b-qv4xv"}),
        ),
    ] {
        let got = mcp.call(tool, args.clone())?;
        assert!(got.is_error, "{tool}: {}", got.text);
        assert_eq!(got.text, expected, "{tool} {args}");
    }

    // Clearing the scope exposes everything again.
    two.scratch
        .write_settings(&exposing(EXPOSED, &two.a.url()))?;
    let clusters = mcp.call_json("list_clusters", json!({}))?;
    assert_eq!(clusters["clusters"][0]["default_namespace"], "kube-system");
    mcp.call_json("list_resources", pods_in("kube-system"))?;
    assert!(two.b.requests().is_empty());
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
