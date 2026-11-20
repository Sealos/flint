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
    let url = silent.unwrap_or("http://127.0.0.1:1");
    std::fs::write(
        dir.join("kubeconfig"),
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: silent\n  cluster:\n    server: {url}\n\
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
        let served = server.clone();
        tokio::spawn(async move {
            if let Ok(service) = served.serve(counted).await {
                let _ = service.waiting().await;
            }
        });
        let (read, write) = tokio::io::split(ours);
        let mut client = Client {
            write,
            read: BufReader::new(read),
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
    let Client { mut write, read } = client;
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
            .is_some_and(|m| m.starts_with("too many requests waiting"));
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

// covers UP-08: at the limit of unanswered requests, a cancellation still gets through: the
// cancelled call ends, and a request waiting behind it runs.
#[tokio::test]
async fn a_cancellation_gets_through_at_the_request_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        unanswered: 2,
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let admission = || &server.inner.admission;
    let mut client = Client::start(&server, 64 * 1024).await;
    for id in 1..=4 {
        client
            .call(id, "list_resource_kinds", json!({"cluster": "silent"}))
            .await;
    }
    wait_for("two calls running", || admission().running() == 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        admission().entered(),
        2,
        "the third and fourth wait at the transport"
    );
    // The fourth is cancelled while it waits there: it never runs, and is recorded.
    client.cancel(4).await;
    client.cancel(1).await;
    // The third takes the cancelled first one's place.
    wait_for("the third reached a turn", || admission().entered() == 3).await;
    wait_for("two calls running", || admission().running() == 2).await;
    let records = || flint_core::activity::read(dir.path());
    wait_for("both cancelled calls are recorded", || records().len() == 2).await;
    assert!(
        records()
            .iter()
            .all(|r| r.outcome == flint_core::activity::Outcome::Failed)
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(admission().entered(), 3, "the fourth never ran");
    assert_eq!(admission().replies().unanswered_count(), 2);
}

// covers UP-08: when the client closes its end with calls still waiting at the transport, they
// still run and are answered, then the stream ends.
#[tokio::test]
async fn calls_waiting_when_the_client_closes_still_run() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _sockets) = silent_server().await;
    let limits = Limits {
        unanswered: 2,
        deadline: Duration::from_millis(300),
        ..Limits::default()
    };
    let server = server(dir.path(), Some(&url), limits);
    let client = Client::start(&server, 64 * 1024).await;
    let Client {
        mut write,
        mut read,
    } = client;
    for id in 1..=3u64 {
        let line = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": "list_resource_kinds", "arguments": {"cluster": "silent"}}})
        );
        write.write_all(line.as_bytes()).await.unwrap();
    }
    write.shutdown().await.unwrap();
    let mut ids = Vec::new();
    let mut line = String::new();
    for _ in 0..3 {
        line.clear();
        tokio::time::timeout(Duration::from_secs(10), read.read_line(&mut line))
            .await
            .expect("each call is answered")
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert!(text(&reply).starts_with("timed out after 300ms"), "{reply}");
        ids.push(reply["id"].as_u64().unwrap());
    }
    ids.sort_unstable();
    assert_eq!(ids, [1, 2, 3]);
}

// covers UP-08: connections are made by at most a few tasks at once, however many clusters are
// called, and a finished one leaves no entry behind (its client stays cached).
#[tokio::test]
async fn connections_are_made_a_few_at_a_time() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let traces = dir.path().join("traces");
    std::fs::create_dir(&traces).unwrap();
    let plugin = dir.path().join("plugin");
    std::fs::write(
        &plugin,
        "#!/bin/sh\necho $$ >> \"$PLUGIN_DIR/starts\"\nexec sleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let names: Vec<String> = (0..6).map(|n| format!("c{n}")).collect();
    let mut kubeconfig = String::from("apiVersion: v1\nkind: Config\nclusters:\n");
    let mut settings = String::new();
    for name in &names {
        kubeconfig += &format!("- name: {name}\n  cluster:\n    server: http://127.0.0.1:1\n");
        settings += &format!(
            "[[ai_exposed_clusters]]\ncontext = \"{name}\"\nserver = \"http://127.0.0.1:1\"\n"
        );
    }
    kubeconfig += &format!(
        "users:\n- name: u\n  user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      \
         command: {}\n      interactiveMode: Never\n      env:\n      - name: PLUGIN_DIR\n        \
         value: {}\ncontexts:\n",
        plugin.display(),
        traces.display()
    );
    for name in &names {
        kubeconfig += &format!("- name: {name}\n  context:\n    cluster: {name}\n    user: u\n");
    }
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig).unwrap();
    std::fs::write(dir.path().join("settings.toml"), settings).unwrap();
    let server = FlintMcp::with_limits(
        dir.path().to_owned(),
        vec![dir.path().join("kubeconfig")],
        None,
        Limits::default(),
    );
    let mut client = Client::start(&server, 64 * 1024).await;
    for (id, name) in (1..).zip(&names) {
        client
            .call(id, "list_resource_kinds", json!({"cluster": name}))
            .await;
    }
    let starts = || {
        std::fs::read_to_string(traces.join("starts"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    wait_for("connections start", || starts() >= super::CONNECTS_AT_ONCE).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(starts(), super::CONNECTS_AT_ONCE, "a few at a time");

    // A connection that finishes takes its entry away, and keeps its client.
    let quick = tempfile::tempdir().unwrap();
    // A server that refuses: the connection (the client) is made at once, the call then fails.
    let fast = self::server(quick.path(), Some("http://127.0.0.1:1"), Limits::default());
    let mut fast_client = Client::start(&fast, 64 * 1024).await;
    fast_client
        .call(1, "list_resource_kinds", json!({"cluster": "silent"}))
        .await;
    let _ = fast_client.next().await;
    wait_for("no connection left in flight", || {
        fast.inner.connecting.lock().unwrap().is_empty()
    })
    .await;
}
