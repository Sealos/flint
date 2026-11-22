//! The [`Limits`] end to end: the server served over an in-memory pipe, the test as its client.

use std::time::Duration;

use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

use super::FlintMcp;
use crate::limits::Limits;

/// A server that accepts connections and never answers; each accepted socket is handed over,
/// so a test can see it closed.
async fn silent_server() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<tokio::net::TcpStream>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let _ = tx.send(socket);
        }
    });
    (url, rx)
}

/// A server whose settings expose `silent` at `server` (none when `None`).
fn server(dir: &std::path::Path, silent: Option<&str>, limits: Limits) -> FlintMcp {
    server_with(dir, silent, "", limits)
}

/// [`server`], with `cluster_extra` added to the kubeconfig's cluster entry.
fn server_with(
    dir: &std::path::Path,
    silent: Option<&str>,
    cluster_extra: &str,
    limits: Limits,
) -> FlintMcp {
    let url = silent.unwrap_or("http://127.0.0.1:1");
    std::fs::write(
        dir.join("kubeconfig"),
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: silent\n  cluster:\n    server: {url}\n{cluster_extra}\
             users:\n- name: u\n  user:\n    token: not-a-real-token\ncontexts:\n- name: silent\n  \
             context:\n    cluster: silent\n    user: u\n"
        ),
    )
    .unwrap();
    let settings = silent.map_or(String::new(), |url| {
        format!("[[ai_exposed_clusters]]\ncontext = \"silent\"\nserver = \"{url}\"\n")
    });
    std::fs::write(dir.join("settings.toml"), settings).unwrap();
    FlintMcp::with_limits(dir.to_owned(), vec![dir.join("kubeconfig")], None, limits)
}

/// The test's end of the pipe: JSON-RPC lines.
struct Client {
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    read: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    /// Ends when the server has shut down.
    served: tokio::task::JoinHandle<()>,
}

impl Client {
    /// Serves `server` over a pipe holding `buffer` bytes each way, and initializes it.
    async fn start(server: &FlintMcp, buffer: usize) -> Self {
        let (ours, theirs) = tokio::io::duplex(buffer);
        let transport =
            rmcp::transport::IntoTransport::<rmcp::RoleServer, std::io::Error, _>::into_transport(
                tokio::io::split(theirs),
            );
        let counted = server.counted(transport);
        let tracked = server.tracked();
        let served = tokio::spawn(async move {
            if let Ok(service) = tracked.serve(counted).await {
                let _ = service.waiting().await;
            }
        });
        let (read, write) = tokio::io::split(ours);
        let mut client = Client {
            write,
            read: BufReader::new(read),
            served,
        };
        client
            .send(
                json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0"},
                }}),
            )
            .await;
        let init = client.next().await;
        assert_eq!(init["id"], json!(0), "{init}");
        client
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        client
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.unwrap();
        self.write.flush().await.unwrap();
    }

    async fn call(&mut self, id: u64, tool: &str, arguments: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}}))
            .await;
    }

    async fn cancel(&mut self, id: u64) {
        self.send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": "test"}}),
        )
        .await;
    }

    /// The next message, within 10 seconds.
    async fn next(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), self.read.read_line(&mut line))
            .await
            .expect("a message within 10 s")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// The next message, if one comes within `wait`.
    async fn next_within(&mut self, wait: Duration) -> Option<Value> {
        let mut line = String::new();
        match tokio::time::timeout(wait, self.read.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => Some(serde_json::from_str(&line).unwrap()),
            _ => None,
        }
    }
}

/// The text of a tool result.
fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}

/// Waits up to 10 seconds for `ready`.
async fn wait_for(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{what}");
}

// covers UP-08: calls against a cluster that never answers run up to the limit; the next ones
// wait, and past those the rest are refused at once, saying why. Cancelling the calls stops
// them, Kubernetes requests included: the server sees its connections closed.
#[tokio::test]
async fn calls_past_the_limit_wait_or_are_refused_and_cancelling_stops_them() {
    let dir = tempfile::tempdir().unwrap();
    let (url, mut sockets) = silent_server().await;
    let limits = Limits {
        running: 2,
        waiting: 2,
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let admission = || &server.inner.admission;
    let mut client = Client::start(&server, 64 * 1024).await;
    for id in 1..=6 {
        client
            .call(id, "list_resource_kinds", json!({"cluster": "silent"}))
            .await;
    }
    let mut refused = Vec::new();
    for _ in 0..2 {
        let reply = client.next().await;
        assert!(
            text(&reply).starts_with("too many tool calls in flight"),
            "{reply}"
        );
        refused.push(reply["id"].as_u64().unwrap());
    }
    wait_for("two calls running", || admission().running() == 2).await;
    assert!(
        client
            .next_within(Duration::from_millis(500))
            .await
            .is_none(),
        "the others wait"
    );
    assert_eq!(admission().running(), 2);
    assert_eq!(admission().admitted(), 4);

    let mut open = Vec::new();
    while let Ok(socket) = sockets.try_recv() {
        open.push(socket);
    }
    assert!(!open.is_empty(), "the running calls reached the server");
    for id in (1..=6).filter(|id| !refused.contains(id)) {
        client.cancel(id).await;
    }
    wait_for("the cancelled calls left", || admission().admitted() == 0).await;
    for mut socket in open {
        let mut buf = [0; 4096];
        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "a cancelled call's request is closed");
    }
}

// covers UP-08: a call past its deadline ends with a timeout naming the server.
#[tokio::test]
async fn a_call_past_its_deadline_names_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        deadline: Duration::from_millis(300),
        ..Limits::default()
    };
    // A server path that looks like a credential is redacted in the reply, as in any other.
    let secret = format!("{}{}", "hunter2", "-not-really");
    let exposed = format!("{url}/password={secret}");
    let server = server(dir.path(), Some(&exposed), limits);
    let mut client = Client::start(&server, 64 * 1024).await;
    client
        .call(1, "list_resource_kinds", json!({"cluster": "silent"}))
        .await;
    let reply = client.next().await;
    assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
    let said = text(&reply);
    assert!(
        said.starts_with(&format!("timed out after 300ms reaching {url}/")),
        "{said}"
    );
    assert!(!said.contains(&secret), "{said}");
    assert_eq!(server.inner.admission.running(), 0);
}

// covers UP-08: a client that stops reading while it keeps calling holds at most the reply
// budget (plus what the running calls produce) in replies not written yet; no reply is lost.
#[tokio::test]
async fn a_stalled_client_holds_the_reply_budget_at_most() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        running: 2,
        waiting: 1000,
        reply_budget: 4 * 1024,
        unanswered: 8,
        ..Limits::default()
    };
    let server = server(dir.path(), None, limits);
    let client = Client::start(&server, 1024).await;
    let Client {
        mut write, read, ..
    } = client;
    let mut client_read = read;
    let calls = 200u64;
    // The writer blocks once the server stops reading; it runs on its own.
    let writer = tokio::spawn(async move {
        for id in 1..=calls {
            let line = format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {"name": "list_clusters", "arguments": {}}})
            );
            write.write_all(line.as_bytes()).await.unwrap();
        }
    });
    // Nothing is read meanwhile.
    let replies = server.inner.admission.replies();
    let mut most = 0;
    for _ in 0..50 {
        most = most.max(replies.outstanding());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let one_reply = 2 * 1024;
    assert!(
        most <= limits.reply_budget + limits.running * one_reply,
        "{most} bytes held"
    );
    assert!(most > 0, "replies were held");
    assert!(!writer.is_finished(), "the server stopped reading requests");
    // Reading again, every reply comes.
    let mut ids = Vec::new();
    let mut line = String::new();
    for _ in 0..calls {
        line.clear();
        tokio::time::timeout(Duration::from_secs(10), client_read.read_line(&mut line))
            .await
            .expect("replies keep coming")
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert!(reply["result"]["isError"] != json!(true), "{reply}");
        ids.push(reply["id"].as_u64().unwrap());
    }
    ids.sort_unstable();
    assert_eq!(ids, (1..=calls).collect::<Vec<_>>());
    writer.await.unwrap();
}

// covers UP-08: calls a stalled client keeps sending are not each answered into memory: while
// replies not written pass the budget, the server reads no more requests, so refusals (and any
// other reply) stop piling up too.
#[tokio::test]
async fn refusals_to_a_stalled_client_stay_within_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        running: 1,
        waiting: 0,
        reply_budget: 2 * 1024,
        unanswered: 8,
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let client = Client::start(&server, 1024).await;
    let Client {
        mut write,
        mut read,
        ..
    } = client;
    // The writer blocks once the server stops reading; it runs on its own.
    let writer = tokio::spawn(async move {
        for id in 1..=3000u64 {
            let line = format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {"name": "list_resource_kinds", "arguments": {"cluster": "silent"}}})
            );
            if write.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
    });
    let replies = server.inner.admission.replies();
    let mut most = 0;
    for _ in 0..100 {
        most = most.max(replies.outstanding());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // At most the budget, plus the replies to requests already read.
    assert!(
        most <= limits.reply_budget + limits.unanswered * 1024,
        "{most} bytes held"
    );
    assert!(replies.unanswered_count() <= limits.unanswered);
    assert!(!writer.is_finished(), "the server stopped reading requests");
    // Reading again, the refusals come and the rest are read.
    let mut refused = 0;
    let mut line = String::new();
    while refused < 2999 {
        line.clear();
        tokio::time::timeout(Duration::from_secs(10), read.read_line(&mut line))
            .await
            .expect("replies keep coming")
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        // Refused by the admission, or at the transport when too many wait there.
        let at_door = reply["error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("too many requests in flight"));
        assert!(
            at_door || text(&reply).starts_with("too many tool calls in flight"),
            "{reply}"
        );
        refused += 1;
    }
    writer.await.unwrap();
}

// covers UP-08: a request whose id is still in flight is refused at the door: rmcp never
// starts a second handler for it.
#[tokio::test]
async fn a_repeated_request_id_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let server = server(dir.path(), Some(&url), Limits::default());
    let mut client = Client::start(&server, 64 * 1024).await;
    for _ in 0..2 {
        client
            .call(7, "list_resource_kinds", json!({"cluster": "silent"}))
            .await;
    }
    let reply = client.next().await;
    assert_eq!(reply["id"], json!(7), "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("still in flight"),
        "{reply}"
    );
    wait_for("one call running", || server.inner.admission.running() == 1).await;
    assert_eq!(server.inner.admission.admitted(), 1);
}

// covers UP-08: a call still running when the client closes its end (longer than rmcp waits for
// handlers then) is still answered: the transport ends the stream only once every request read
// is answered. The real deadline path, on a paused clock.
#[tokio::test(start_paused = true)]
async fn a_call_outliving_the_close_is_still_answered() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        deadline: Duration::from_secs(7),
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let client = Client::start(&server, 64 * 1024).await;
    let Client {
        mut write,
        mut read,
        ..
    } = client;
    let line = format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "list_resource_kinds", "arguments": {"cluster": "silent"}}})
    );
    write.write_all(line.as_bytes()).await.unwrap();
    write.shutdown().await.unwrap();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(30), read.read_line(&mut answer))
        .await
        .expect("answered")
        .unwrap();
    let reply: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(reply["id"], json!(1), "{reply}");
    assert!(text(&reply).starts_with("timed out after 7s"), "{reply}");
}

// covers UP-08: rmcp rejects this call before `call_tool` (its protocol version is unsupported)
// and, the cancellation having been read first, drops the error. The call must not keep its
// place, and the server must still exit when stdin closes.
#[tokio::test]
async fn a_call_cancelled_before_call_tool_lets_go_and_shutdown_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(dir.path(), None, Limits::default());
    let mut client = Client::start(&server, 64 * 1024).await;
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
        "name": "list_clusters", "arguments": {},
        "_meta": {"io.modelcontextprotocol/protocolVersion": "1900-01-01"},
    }});
    let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 1, "reason": "test"}});
    // One write, so the cancellation is read before the handler task gets to run.
    let lines = format!("{call}\n{cancel}\n");
    client.write.write_all(lines.as_bytes()).await.unwrap();
    client.write.flush().await.unwrap();
    let replies = server.inner.admission.replies();
    wait_for("the cancelled call let go", || {
        replies.unanswered_count() == 0
    })
    .await;
    client.write.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), client.served)
        .await
        .expect("the server exits")
        .unwrap();
}

// covers UP-08: a client that closes stdin while it does not read stdout, with a reply stuck in
// the pipe, cannot keep the server alive.
#[tokio::test(start_paused = true)]
async fn shutdown_finishes_when_the_client_stopped_reading() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        reply_budget: 1,
        deadline: Duration::from_secs(7),
        ..Limits::default()
    };
    let server = server(dir.path(), None, limits);
    let mut client = Client::start(&server, 256).await;
    client
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let replies = server.inner.admission.replies();
    wait_for("the reply is stuck", || replies.outstanding() > 1).await;
    client.write.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), client.served)
        .await
        .expect("the server exits")
        .unwrap();
}

// covers UP-08: at the limit of unanswered requests, a request is refused at once (and, a tool
// call, recorded), while a cancellation still gets through and frees a place.
#[tokio::test]
async fn at_the_request_limit_calls_are_refused_and_cancellations_pass() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        unanswered: 2,
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let admission = || &server.inner.admission;
    let mut client = Client::start(&server, 64 * 1024).await;
    for (id, kind) in [(1, "pods"), (2, "pods"), (3, "services")] {
        client
            .call(
                id,
                "list_resources",
                json!({"cluster": "silent", "kind": kind}),
            )
            .await;
    }
    let refused = client.next().await;
    assert_eq!(refused["id"], json!(3), "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("too many requests in flight"),
        "{refused}"
    );
    wait_for("two calls running", || admission().running() == 2).await;
    client.cancel(1).await;
    wait_for("the cancelled call left", || admission().running() == 1).await;
    // Its turn goes before its handler has recorded it and ended; only then is a place free.
    let replies = admission().replies();
    wait_for("the cancelled call is answered", || {
        replies.unanswered_count() == 1
    })
    .await;
    client
        .call(4, "list_resource_kinds", json!({"cluster": "silent"}))
        .await;
    wait_for("the fourth runs", || admission().entered() == 3).await;
    let records = || flint_core::activity::read(dir.path());
    wait_for("the refused and the cancelled call are recorded", || {
        records().len() == 2
    })
    .await;
    // The refused call is recorded by its tool alone: its arguments were never validated.
    let refused_record = records()
        .into_iter()
        .find(|r| r.cluster.is_none())
        .expect("the refused call");
    assert_eq!(refused_record.tool, "list_resources");
    assert_eq!(refused_record.kind, None);
    assert!(
        records()
            .iter()
            .all(|r| r.outcome == flint_core::activity::Outcome::Failed)
    );
}

// covers UP-16: a call naming a context in the kubeconfig is listed under that context, though it
// was denied or stopped at its arguments, and contexts with similar names keep separate keys. A
// name that is no context is not keyed as written.
#[tokio::test]
async fn a_call_that_names_a_context_keeps_its_key_unconnected() {
    use flint_core::activity::cluster_key;
    let dir = tempfile::tempdir().unwrap();
    let mut kubeconfig = String::from("apiVersion: v1\nkind: Config\nclusters:\n");
    for name in ["token:prod", "token:staging"] {
        kubeconfig += &format!("- name: {name}\n  cluster:\n    server: http://127.0.0.1:1\n");
    }
    kubeconfig += "users:\n- name: u\n  user:\n    token: not-a-real-token\ncontexts:\n";
    for name in ["token:prod", "token:staging"] {
        kubeconfig += &format!("- name: {name}\n  context:\n    cluster: {name}\n    user: u\n");
    }
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig).unwrap();
    std::fs::write(dir.path().join("settings.toml"), "").unwrap();
    let server = FlintMcp::with_limits(
        dir.path().to_owned(),
        vec![dir.path().join("kubeconfig")],
        None,
        Limits::default(),
    );
    let mut client = Client::start(&server, 64 * 1024).await;
    let calls = [
        ("list_resource_kinds", json!({"cluster": "token:prod"})),
        // No `kind`: stopped before the tool runs.
        (
            "get_resource",
            json!({"cluster": "token:staging", "namespace": "web", "name": "x"}),
        ),
        ("list_resource_kinds", json!({"cluster": "token:other"})),
    ];
    for (id, (tool, arguments)) in (1..).zip(calls) {
        client.call(id, tool, arguments).await;
        client.next().await;
    }
    let records = || flint_core::activity::read(dir.path());
    wait_for("the calls are recorded", || records().len() == 3).await;
    let records = records();
    assert_eq!(records[0].cluster_key, Some(cluster_key("token:prod")));
    assert_eq!(records[1].cluster_key, Some(cluster_key("token:staging")));
    assert_eq!(records[2].cluster.as_deref(), Some("…(+11 bytes)"));
    assert_eq!(records[2].cluster_key, None);
}

// covers UP-16: a refused call's arguments are unvalidated, so its record holds the tool and the
// failure only: no value, and no cluster to attribute it to.
#[tokio::test]
async fn a_refused_call_records_no_argument_values() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let server = server(dir.path(), Some(&url), Limits::default());
    let mut client = Client::start(&server, 64 * 1024).await;
    let calls = [
        json!({"name": "raw-value"}),
        json!({"cluster": "raw-value"}),
    ];
    for (id, arguments) in (1..).zip(calls) {
        client.call(id, "list_clusters", arguments).await;
        let reply = client.next().await;
        assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
    }
    let records = || flint_core::activity::read(dir.path());
    wait_for("the refused calls are recorded", || records().len() == 2).await;
    for record in records() {
        assert_eq!(record.tool, "list_clusters");
        assert_eq!(record.outcome, flint_core::activity::Outcome::Failed);
        assert_eq!(record.cluster, None, "{record:?}");
        assert_eq!(record.name, None, "{record:?}");
        assert_eq!(record.kind, None, "{record:?}");
        assert_eq!(record.namespace, None, "{record:?}");
        assert_eq!(record.container, None, "{record:?}");
    }
    let stored = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| std::fs::read(e.ok()?.path()).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect::<String>();
    assert!(!stored.is_empty());
    assert!(!stored.contains("raw-value"), "{stored}");
}

/// Values a call sends that must never reach the activity log, in every field a record has.
fn raw_arguments() -> Value {
    json!({
        "cluster": "raw-value-cluster",
        "kind": "raw-value-kind",
        "namespace": "raw-value-namespace",
        "name": "raw-value-name",
        "container": "raw-value-container",
    })
}

/// Waits for `count` records of `tool` with no argument value or cluster, and checks that no
/// file in `dir` holds a value of [`raw_arguments`].
async fn assert_tool_only_records(dir: &std::path::Path, tool: &str, count: usize) {
    let bare = || {
        flint_core::activity::read(dir)
            .into_iter()
            .filter(|r| r.tool == tool && r.cluster.is_none())
            .collect::<Vec<_>>()
    };
    wait_for("the refused calls are recorded", || bare().len() >= count).await;
    for record in bare() {
        assert_eq!(record.outcome, flint_core::activity::Outcome::Failed);
        assert_eq!(record.kind, None, "{record:?}");
        assert_eq!(record.namespace, None, "{record:?}");
        assert_eq!(record.name, None, "{record:?}");
        assert_eq!(record.container, None, "{record:?}");
        assert_eq!(record.server, None, "{record:?}");
    }
    let stored = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| std::fs::read(e.ok()?.path()).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect::<String>();
    assert!(!stored.contains("raw-value"), "{stored}");
}

// covers UP-16: a call refused at the transport, past the request limit or with a repeated id,
// records its tool only.
#[tokio::test]
async fn a_call_refused_at_the_transport_records_no_argument_values() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        unanswered: 2,
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let mut client = Client::start(&server, 64 * 1024).await;
    let held = json!({"cluster": "silent", "kind": "pods"});
    client.call(1, "list_resources", held.clone()).await;
    client.call(2, "list_resources", held).await;
    wait_for("two calls running", || {
        server.inner.admission.running() == 2
    })
    .await;
    for id in [3, 1] {
        client.call(id, "get_resource", raw_arguments()).await;
        let refused = client.next().await;
        assert_eq!(refused["id"], json!(id), "{refused}");
        assert!(refused["error"].is_object(), "{refused}");
    }
    assert_tool_only_records(dir.path(), "get_resource", 2).await;
}

// covers UP-16: a call refused because too many are in flight, and one cancelled while it waits
// for a turn, record their tool only.
#[tokio::test]
async fn a_call_refused_or_cancelled_before_admission_records_no_argument_values() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        running: 1,
        waiting: 1,
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let admission = || &server.inner.admission;
    let mut client = Client::start(&server, 64 * 1024).await;
    client
        .call(
            1,
            "list_resources",
            json!({"cluster": "silent", "kind": "pods"}),
        )
        .await;
    wait_for("one call running", || admission().running() == 1).await;
    client.call(2, "get_resource", raw_arguments()).await;
    wait_for("one call waiting", || admission().admitted() == 2).await;

    client.call(3, "get_resource", raw_arguments()).await;
    let busy = client.next().await;
    assert_eq!(busy["id"], json!(3), "{busy}");
    assert!(text(&busy).starts_with("too many tool calls"), "{busy}");
    assert_tool_only_records(dir.path(), "get_resource", 1).await;

    client.cancel(2).await;
    wait_for("the waiting call left", || admission().admitted() == 1).await;
    assert_tool_only_records(dir.path(), "get_resource", 2).await;
}

// covers UP-10: an argument of the wrong type is refused naming what was expected, without
// repeating the value, even one holding quotes. A missing argument is still named.
#[tokio::test]
async fn a_wrong_type_argument_is_not_echoed() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(dir.path(), None, Limits::default());
    let mut client = Client::start(&server, 64 * 1024).await;
    for (id, limit) in [
        (1, json!("raw-value")),
        (2, json!("a\"raw-value\"b")),
        (3, json!(-77_001)),
    ] {
        let arguments = json!({"cluster": "silent", "kind": "pods", "limit": limit});
        client.call(id, "list_resources", arguments).await;
        let reply = client.next().await;
        assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
        let said = text(&reply);
        assert!(said.contains("expected u32"), "{reply}");
        assert!(
            !said.contains("raw-value") && !said.contains("77001"),
            "{reply}"
        );
        assert!(!reply.to_string().contains("raw-value"), "{reply}");
    }
    client
        .call(4, "list_resources", json!({"cluster": "silent"}))
        .await;
    let missing = client.next().await;
    assert!(text(&missing).contains("missing field `kind`"), "{missing}");
}

// covers UP-16: a `list_clusters` call ended by its deadline while it probes is still recorded
// with the clusters it read about, so it shows in their activity.
#[tokio::test(start_paused = true)]
async fn a_probe_past_the_deadline_keeps_its_clusters() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        deadline: Duration::from_secs(7),
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let mut client = Client::start(&server, 64 * 1024).await;
    client.call(1, "list_clusters", json!({})).await;
    let reply = client.next().await;
    assert_eq!(reply["id"], json!(1), "{reply}");
    assert!(text(&reply).starts_with("timed out after 7s"), "{reply}");
    let records = || flint_core::activity::read(dir.path());
    wait_for("the call is recorded", || records().len() == 1).await;
    let record = records().remove(0);
    assert_eq!(record.tool, "list_clusters");
    assert_eq!(record.outcome, flint_core::activity::Outcome::Failed);
    assert_eq!(record.probed.len(), 1, "{record:?}");
}

/// A server answering every request with a 403 whose message is `message`.
async fn forbidding_server(message: impl Into<std::sync::Arc<str>>) -> String {
    let message: std::sync::Arc<str> = message.into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let message = std::sync::Arc::clone(&message);
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                let body = json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "message": &*message, "reason": "Forbidden", "code": 403})
                .to_string();
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    url
}

/// The one row `list_clusters` returns from `server`.
async fn cluster_row(server: &FlintMcp) -> Value {
    let mut client = Client::start(server, 64 * 1024).await;
    client.call(1, "list_clusters", json!({})).await;
    let reply = client.next().await;
    let out: Value = serde_json::from_str(text(&reply)).unwrap();
    out["clusters"][0].clone()
}

#[tokio::test]
async fn a_cluster_row_tidies_the_proxy_field() {
    let dir = tempfile::tempdir().unwrap();
    let url = forbidding_server("denied").await;
    let proxy = "    proxy-url: http://proxyuser:proxy-pass-123@127.0.0.1:9\n";
    let server = server_with(dir.path(), Some(&url), proxy, Limits::default());
    let row = cluster_row(&server).await;
    let error = row["error"].as_str().expect("the probe failed");
    assert!(error.contains("[REDACTED:password]"), "{row}");
    assert!(!row.to_string().contains("proxy-pass-123"), "{row}");
}

#[tokio::test]
async fn a_cluster_row_tidies_a_quoted_server_message() {
    let dir = tempfile::tempdir().unwrap();
    let url = forbidding_server(
        "denied for Authorization: Bearer abcdefghijklmnop0123 password=example-value",
    )
    .await;
    let server = server(dir.path(), Some(&url), Limits::default());
    let row = cluster_row(&server).await;
    let error = row["error"].as_str().expect("the probe failed");
    assert!(error.contains("[REDACTED:authorization]"), "{row}");
    assert!(!error.contains("abcdefghijklmnop0123"), "{row}");
    assert!(!error.contains("example-value"), "{row}");
}

#[tokio::test]
async fn a_cluster_row_tidies_a_labelled_server_path() {
    let dir = tempfile::tempdir().unwrap();
    let url = forbidding_server("denied").await;
    let with_path = format!("{url}/password=example-value");
    let server = server(dir.path(), Some(&with_path), Limits::default());
    let row = cluster_row(&server).await;
    let shown = row["server"].as_str().expect("a server");
    assert!(shown.starts_with(&url), "{row}");
    assert!(!shown.contains("example-value"), "{row}");
}

// covers UP-16: a server message is shortened after it is tidied, wherever the cut falls.
#[tokio::test]
async fn a_cluster_row_tidies_a_url_across_the_cut() {
    for pad in 230..=300 {
        let dir = tempfile::tempdir().unwrap();
        let message = format!(
            "{} postgres://admin:example-value-123@db.internal:5432/app",
            "x".repeat(pad)
        );
        let url = forbidding_server(message).await;
        let server = server(dir.path(), Some(&url), Limits::default());
        let row = cluster_row(&server).await;
        let error = row["error"].as_str().expect("the probe failed");
        assert!(!error.contains("exam"), "pad {pad}: {row}");
    }
}

/// The records and the raw log left by `calls`, sent in order.
async fn recorded(calls: &[(&str, Value)]) -> (Vec<flint_core::activity::ActivityRecord>, String) {
    let dir = tempfile::tempdir().unwrap();
    let server = server(dir.path(), None, Limits::default());
    let mut client = Client::start(&server, 64 * 1024).await;
    for (id, (tool, arguments)) in (1..).zip(calls) {
        client.call(id, tool, arguments.clone()).await;
        client.next().await;
    }
    let records = || flint_core::activity::read(dir.path());
    wait_for("the calls are recorded", || records().len() == calls.len()).await;
    let log = std::fs::read_to_string(dir.path().join("agent-activity.jsonl"));
    (records(), log.unwrap())
}

// covers UP-16: a value that is no name is recorded by its size, whether or not the call's
// other arguments were accepted, and the call is still listed.
#[tokio::test]
async fn a_value_that_is_no_name_is_recorded_by_size() {
    let value = r#"{"apiVersion":"v1","kind":"Secret","data":{"session":"b3BhcXVlLXZhbHVl"}}"#;
    let size = format!("…(+{} bytes)", value.len());
    let multiline = "web\nnext";
    let calls = [
        (
            "get_events",
            json!({"cluster": "silent", "object_name": value, "limit": 5}),
        ),
        (
            "get_events",
            json!({"cluster": "silent", "object_name": value, "limit": "x"}),
        ),
        (
            "get_events",
            json!({"cluster": "silent", "namespace": value}),
        ),
        (
            "get_events",
            json!({"cluster": "silent", "object_kind": value}),
        ),
        (
            "get_pod_logs",
            json!({"cluster": "silent", "namespace": "web", "pod": "web-1", "container": value}),
        ),
        ("get_events", json!({"cluster": value})),
        (
            "get_events",
            json!({"cluster": "silent", "object_kind": "Pod.Debug"}),
        ),
        (
            "get_pod_logs",
            json!({"cluster": "silent", "namespace": "web", "pod": "Bad_Pod"}),
        ),
        (
            "get_events",
            json!({"cluster": "silent", "object_name": multiline, "limit": 5}),
        ),
        (
            "get_events",
            json!({"cluster": "silent", "object_name": multiline, "limit": "x"}),
        ),
        ("list_resource_kinds", json!({"cluster": "a\u{1b}[2Jb"})),
    ];
    let (records, log) = recorded(&calls).await;
    assert!(!log.contains("b3BhcXVlLXZhbHVl"), "{log}");
    assert_eq!(records[0].name.as_ref(), Some(&size));
    assert_eq!(records[1].name.as_ref(), Some(&size));
    assert_eq!(records[2].namespace.as_ref(), Some(&size));
    assert_eq!(records[3].kind.as_ref(), Some(&size));
    assert_eq!(records[4].container.as_ref(), Some(&size));
    assert_eq!(records[5].cluster.as_ref(), Some(&size));
    assert_eq!(records[6].kind.as_deref(), Some("…(+9 bytes)"));
    assert_eq!(records[7].name.as_deref(), Some("…(+7 bytes)"));
    assert_eq!(records[8].name.as_deref(), Some("…(+8 bytes)"));
    assert_eq!(records[9].name, records[8].name);
    assert_eq!(records[10].cluster.as_deref(), Some("…(+6 bytes)"));
    assert_eq!(records[10].cluster_key, None);
}

// covers UP-16: ordinary names are recorded as written.
#[tokio::test]
async fn valid_names_are_recorded_as_written() {
    let calls = [
        (
            "get_events",
            json!({"cluster": "silent", "namespace": "web", "object_name": "web-1",
                "object_kind": "Pod", "limit": 5}),
        ),
        (
            "get_events",
            json!({"cluster": "silent", "object_name": "system:controller:job", "limit": "x"}),
        ),
        (
            "get_pod_logs",
            json!({"cluster": "silent", "namespace": "web", "pod": "web-1", "container": "app"}),
        ),
        (
            "get_resource",
            json!({"cluster": "team/gone", "kind": "deployments.apps", "name": "web"}),
        ),
    ];
    let (records, log) = recorded(&calls).await;
    assert_eq!(records[0].cluster.as_deref(), Some("silent"));
    assert_eq!(
        records[0].cluster_key,
        Some(flint_core::activity::cluster_key("silent"))
    );
    assert_eq!(records[0].namespace.as_deref(), Some("web"));
    assert_eq!(records[0].name.as_deref(), Some("web-1"));
    assert_eq!(records[0].kind.as_deref(), Some("Pod"));
    assert_eq!(records[1].name.as_deref(), Some("system:controller:job"));
    assert_eq!(records[2].container.as_deref(), Some("app"));
    assert_eq!(records[3].cluster.as_deref(), Some("…(+9 bytes)"));
    assert_eq!(records[3].cluster_key, None);
    assert!(!log.contains("team/gone"), "{log}");
    assert_eq!(records[3].kind.as_deref(), Some("deployments.apps"));
}

// covers UP-16: a context's name is kept whatever its characters, so its tab lists the call.
#[tokio::test]
async fn a_context_with_any_name_keeps_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let name = "team a/password=placeholder-context";
    std::fs::write(
        dir.path().join("kubeconfig"),
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
             http://127.0.0.1:1\nusers:\n- name: u\n  user:\n    token: not-a-real-token\n\
             contexts:\n- name: {name}\n  context:\n    cluster: c\n    user: u\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.path().join("settings.toml"), "").unwrap();
    let server = FlintMcp::with_limits(
        dir.path().to_owned(),
        vec![dir.path().join("kubeconfig")],
        None,
        Limits::default(),
    );
    let mut client = Client::start(&server, 64 * 1024).await;
    client
        .call(1, "list_resource_kinds", json!({"cluster": name}))
        .await;
    client.next().await;
    let records = || flint_core::activity::read(dir.path());
    wait_for("the call is recorded", || records().len() == 1).await;
    let record = &records()[0];
    assert_eq!(
        record.cluster.as_deref(),
        Some("team a/password=[REDACTED:password]")
    );
    assert_eq!(
        record.cluster_key,
        Some(flint_core::activity::cluster_key(name))
    );
}

#[test]
fn a_padded_kind_selector_is_recorded_as_the_kind_it_read() {
    let recorded = |tool: &str, kind: &str| {
        let record = flint_core::activity::ActivityRecord {
            at_ms: 0,
            tool: tool.into(),
            cluster: None,
            server: None,
            kind: Some(kind.into()),
            namespace: None,
            name: None,
            container: None,
            items: None,
            bytes: 0,
            truncated: false,
            outcome: flint_core::activity::Outcome::Denied,
            cluster_key: None,
            server_key: None,
            probed: Vec::new(),
            probed_more: 0,
        };
        super::names_only(record).kind.unwrap()
    };
    assert_eq!(recorded("list_resources", " pods "), "pods");
    assert_eq!(recorded("get_resource", "\npods\n"), "pods");
    assert_eq!(recorded("list_resources", "po ds"), "…(+5 bytes)");
    assert_eq!(recorded("get_events", " Pod "), "…(+5 bytes)");
}
