//! Exec over real HTTPS, through kube's `AttachedProcess` and raw WebSocket as Flint's client
//! stack builds them.

use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::api::{Api, AttachParams, AttachedProcess, TerminalSize};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::fake_api::{ExecEnd, ExecImage, FakePod, Fault, Options, TtySize};

const SHORT: Duration = Duration::from_secs(5);
const WEB: &str = "web-69c6f74b8b-qv4xv";
const PODINFO: &str = "podinfo-c6497595d-b8chj";

fn pods(client: &kube::Client) -> Api<Pod> {
    Api::namespaced(client.clone(), "demo")
}

async fn exec(client: &kube::Client, pod: &str, command: &[&str]) -> AttachedProcess {
    exec_with(client, pod, command, &AttachParams::interactive_tty())
        .await
        .unwrap()
}

async fn exec_with(
    client: &kube::Client,
    pod: &str,
    command: &[&str],
    params: &AttachParams,
) -> Result<AttachedProcess, kube::Error> {
    pods(client).exec(pod, command.to_vec(), params).await
}

/// Reads until the output so far ends with `end`; returns all of it.
async fn read_until(out: &mut (impl AsyncRead + Unpin), end: &str) -> String {
    let mut seen = Vec::new();
    let mut buf = [0; 4096];
    tokio::time::timeout(SHORT, async {
        while !seen.ends_with(end.as_bytes()) {
            let n = out.read(&mut buf).await.unwrap();
            assert!(n > 0, "output ended: {:?}", String::from_utf8_lossy(&seen));
            seen.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "never ended with {end:?}: {:?}",
            String::from_utf8_lossy(&seen)
        )
    });
    String::from_utf8(seen).unwrap()
}

async fn read_to_end(out: &mut (impl AsyncRead + Unpin)) -> String {
    let mut seen = Vec::new();
    tokio::time::timeout(SHORT, out.read_to_end(&mut seen))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(seen).unwrap()
}

async fn status_of(process: &mut AttachedProcess) -> Option<Status> {
    tokio::time::timeout(SHORT, process.take_status().unwrap())
        .await
        .unwrap()
}

/// The exit code in a `NonZeroExitCode` status.
fn code_of(status: &Status) -> Option<&str> {
    let cause = status.details.as_ref()?.causes.as_ref()?.first()?;
    (cause.reason.as_deref() == Some("ExitCode")).then_some(cause.message.as_deref()?)
}

/// The command ended with a `Success` status.
async fn succeeded(process: &mut AttachedProcess) -> bool {
    status_of(process).await.and_then(|s| s.status).as_deref() == Some("Success")
}

fn api_error(err: kube::Error) -> (u16, String) {
    match err {
        kube::Error::Api(status) => (status.code, status.message),
        other => panic!("not an API error: {other}"),
    }
}

const PROMPT: &str = "\x1b[?2004hroot@web-69c6f74b8b-qv4xv:/# ";

#[tokio::test]
async fn a_bash_session_echoes_resizes_and_exits_with_its_code() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    let mut process = exec(&client, WEB, &["bash"]).await;
    let mut stdin = process.stdin().unwrap();
    let mut stdout = process.stdout().unwrap();
    let mut resize = process.terminal_size().unwrap();
    assert_eq!(read_until(&mut stdout, PROMPT).await, PROMPT);
    server.wait_for_exec_sessions(1, SHORT).await.unwrap();
    let session = server.last_exec().unwrap();
    assert_eq!(
        (session.pod(), session.container(), session.command()),
        (WEB, "nginx", &["bash".to_owned()][..])
    );
    assert!(session.tty() && session.protocol() == V5);

    resize
        .send(TerminalSize {
            width: 100,
            height: 30,
        })
        .await
        .unwrap();
    session.wait_for_resizes(1, SHORT).await.unwrap();
    assert_eq!(
        session.resizes(),
        [TtySize {
            width: 100,
            height: 30
        }]
    );
    read_until(&mut stdout, "root@web-69c6f74b8b-qv4xv:/# ").await;
    stdin.write_all(b"stty size\r").await.unwrap();
    let out = read_until(&mut stdout, PROMPT).await;
    assert_eq!(out, format!("stty size\r\n\x1b[?2004l\r30 100\r\n{PROMPT}"));

    // The test pushes output; the client's bytes are recorded as sent.
    assert!(session.print("from the test\r\n"));
    read_until(&mut stdout, "from the test\r\n").await;
    stdin
        .write_all(b"\x1b[200~echo a\x1b[201~\x03")
        .await
        .unwrap();
    session.wait_for_stdin(b"\x03", SHORT).await.unwrap();
    assert!(
        server
            .last_exec_stdin()
            .ends_with(b"\x1b[200~echo a\x1b[201~\x03")
    );

    stdin.write_all(b"exit 3\r").await.unwrap();
    let status = status_of(&mut process).await.unwrap();
    assert_eq!(status.reason.as_deref(), Some("NonZeroExitCode"));
    assert_eq!(code_of(&status), Some("3"));
    assert!(read_to_end(&mut stdout).await.ends_with("exit\r\n"));
    process.join().await.unwrap();
    assert_eq!(
        session.wait_for_end(SHORT).await.unwrap(),
        ExecEnd::Exited(3)
    );
    server.wait_for_exec_sessions(0, SHORT).await.unwrap();
    assert!(session.stdout().starts_with(PROMPT.as_bytes()));
    let requests = server.exec_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].status, Some(101));
    assert_eq!(requests[0].parsed_query().command, ["bash"]);
}

#[tokio::test]
async fn a_container_without_bash_reports_it_on_the_status_channel() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    let mut process = exec(&client, PODINFO, &["bash"]).await;
    let mut stdout = process.stdout().unwrap();
    let status = status_of(&mut process).await.unwrap();
    assert_eq!(
        (status.reason.as_deref(), status.code),
        (Some("InternalError"), Some(500))
    );
    let message = status.message.unwrap();
    assert!(
        message.ends_with("exec: \"bash\": executable file not found in $PATH"),
        "{message}"
    );
    assert_eq!(read_to_end(&mut stdout).await, "");
    process.join().await.unwrap();
    let session = server.last_exec().unwrap();
    assert!(
        matches!(session.wait_for_end(SHORT).await.unwrap(), ExecEnd::Failed(m) if m == message)
    );

    // `/bin/sh` is there: podinfo runs as `app`, and busybox asks where the cursor is.
    let mut process = exec(&client, PODINFO, &["/bin/sh"]).await;
    let mut stdout = process.stdout().unwrap();
    assert_eq!(read_until(&mut stdout, "~ $ \x1b[6n").await, "~ $ \x1b[6n");
    let mut stdin = process.stdin().unwrap();
    stdin.write_all(b"\x04").await.unwrap();
    assert!(succeeded(&mut process).await);

    // Docker prints the error on the TTY and exits 126.
    server.set_exec_image("demo", PODINFO, None, ExecImage::sh_only().docker());
    let mut process = exec(&client, PODINFO, &["bash"]).await;
    let mut stdout = process.stdout().unwrap();
    let out = read_to_end(&mut stdout).await;
    assert!(out.starts_with("OCI runtime exec failed: "), "{out}");
    let status = status_of(&mut process).await.unwrap();
    assert_eq!(code_of(&status), Some("126"));
    // And a pod made in a test (busybox) has no bash either, unless a test says it has.
    server.add_pod(FakePod::new("demo", "box", &["app"]));
    let mut process = exec(&client, "box", &["bash"]).await;
    assert_eq!(status_of(&mut process).await.unwrap().code, Some(500));
    server.set_exec_image("demo", "box", Some("app"), ExecImage::with_bash());
    let mut process = exec(&client, "box", &["bash"]).await;
    let mut stdout = process.stdout().unwrap();
    read_until(&mut stdout, "root@box:/# ").await;
}

#[tokio::test]
async fn refusals_before_the_upgrade_match_the_api_server_and_kubelet() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    let tty = AttachParams::interactive_tty();
    let refused = |result: Result<AttachedProcess, kube::Error>| match result {
        Ok(_) => panic!("the exec started"),
        Err(e) => api_error(e),
    };
    assert_eq!(
        refused(exec_with(&client, "nope", &["sh"], &tty).await),
        (404, "pods \"nope\" not found".to_owned())
    );
    assert_eq!(
        refused(
            exec_with(
                &client,
                WEB,
                &["sh"],
                &AttachParams::interactive_tty().container("app")
            )
            .await
        ),
        (400, format!("container app is not valid for pod {WEB}"))
    );
    server.add_pod(FakePod::new("demo", "two", &["a", "b"]));
    assert_eq!(
        refused(exec_with(&client, "two", &["sh"], &tty).await),
        (
            400,
            "a container name must be specified for pod two, choose one of: [a b]".to_owned()
        )
    );
    // The kubelet's refusals are plain text.
    for pod in ["crash-loop", "broken-image", "fixture-job-qd9jf"] {
        let container = server.pod("demo", pod).unwrap().container_names()[0].clone();
        assert_eq!(
            refused(exec_with(&client, pod, &["sh"], &tty).await),
            (500, format!("container not found (\"{container}\")")),
            "{pod}"
        );
    }
    // kube refuses an exec with no streams itself; raw requests reach the kubelet's check.
    let raw = |path: &str| {
        http::Request::get(format!("/api/v1/namespaces/demo/pods/{WEB}/exec?{path}"))
            .body(Vec::new())
            .unwrap()
    };
    let err = client
        .connect(raw("command=sh&tty=true"))
        .await
        .err()
        .unwrap();
    assert_eq!(
        api_error(err),
        (
            400,
            "you must specify at least 1 of stdin, stdout, stderr".to_owned()
        )
    );
    // Without an upgrade.
    let err = client
        .request_text(raw("command=sh&stdout=true"))
        .await
        .unwrap_err();
    assert_eq!(api_error(err), (400, "Upgrade request required".to_owned()));

    server.inject(
        Fault::forbidden()
            .on(&Gvr::core("pods"))
            .subresource("exec")
            .times(1),
    );
    let (code, message) = refused(exec_with(&client, WEB, &["sh"], &tty).await);
    assert_eq!(code, 403);
    assert!(
        message.contains("cannot get resource \"pods/exec\""),
        "{message}"
    );

    server.inject(Fault::refuse_upgrade().times(1));
    match exec_with(&client, WEB, &["sh"], &tty).await {
        Err(kube::Error::UpgradeConnection(
            kube::client::UpgradeConnectionError::ProtocolSwitch(code),
        )) => assert_eq!(code, 200),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("the upgrade went through"),
    }
    assert!(server.exec_sessions().is_empty());
    let statuses: Vec<_> = server.exec_requests().iter().map(|r| r.status).collect();
    assert_eq!(statuses.last(), Some(&Some(200)));
}

#[tokio::test]
async fn the_server_can_end_close_or_break_a_session() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();

    let mut process = exec(&client, WEB, &["bash"]).await;
    let mut stdout = process.stdout().unwrap();
    read_until(&mut stdout, PROMPT).await;
    server.last_exec().unwrap().exit(0);
    assert!(succeeded(&mut process).await);
    process.join().await.unwrap();

    let mut process = exec(&client, WEB, &["bash"]).await;
    let mut stdout = process.stdout().unwrap();
    read_until(&mut stdout, PROMPT).await;
    let session = server.last_exec().unwrap();
    assert!(session.close());
    assert_eq!(status_of(&mut process).await, None);
    process.join().await.unwrap();
    assert_eq!(session.end(), Some(ExecEnd::Closed));

    let mut process = exec(&client, WEB, &["bash"]).await;
    let mut stdout = process.stdout().unwrap();
    read_until(&mut stdout, PROMPT).await;
    let status_rx = process.take_status().unwrap();
    assert_eq!(server.drop_exec_sessions(), 1);
    let joined = tokio::time::timeout(SHORT, process.join()).await.unwrap();
    assert!(joined.is_err(), "a dropped connection is an error");
    assert_eq!(status_rx.await, None);
    assert_eq!(server.last_exec().unwrap().end(), Some(ExecEnd::Dropped));
    assert_eq!(server.drop_exec_sessions(), 0);

    // The client going away ends the session too.
    let process = exec(&client, WEB, &["bash"]).await;
    server.wait_for_exec_sessions(1, SHORT).await.unwrap();
    drop(process);
    server.wait_for_exec_sessions(0, SHORT).await.unwrap();
    assert_eq!(server.last_exec().unwrap().end(), Some(ExecEnd::ClientGone));
    assert_eq!(server.exec_sessions().len(), 4);
}

#[tokio::test]
async fn stdin_close_ends_a_shell_only_without_a_tty() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    // With a TTY: nothing happens (the shell keeps its prompt).
    let mut process = exec(&client, WEB, &["bash"]).await;
    let mut stdout = process.stdout().unwrap();
    read_until(&mut stdout, PROMPT).await;
    let mut stdin = process.stdin().unwrap();
    stdin.shutdown().await.unwrap();
    let session = server.last_exec().unwrap();
    tokio::time::timeout(SHORT, async {
        while !session.stdin_closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(session.is_open());
    session.exit(0);

    // Without one: stdout and stderr apart, and closing stdin ends it with the last status.
    let params = AttachParams::default().stdin(true).stderr(true);
    let mut process = exec_with(&client, PODINFO, &["sh"], &params).await.unwrap();
    let mut stdin = process.stdin().unwrap();
    let mut stdout = process.stdout().unwrap();
    let mut stderr = process.stderr().unwrap();
    stdin.write_all(b"echo one\nnosuch\n").await.unwrap();
    assert_eq!(read_until(&mut stdout, "one\n").await, "one\n");
    assert_eq!(
        read_until(&mut stderr, "\n").await,
        "sh: nosuch: not found\n"
    );
    stdin.shutdown().await.unwrap();
    let status = status_of(&mut process).await.unwrap();
    assert_eq!(code_of(&status), Some("127"));
    let session = server.last_exec().unwrap();
    assert!(!session.tty() && session.stdin_closed());
    assert_eq!(
        session.wait_for_end(SHORT).await.unwrap(),
        ExecEnd::Exited(127)
    );
}

#[tokio::test]
async fn floods_stream_until_done_or_interrupted() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    let mut process = exec(&client, WEB, &["seq", "100000"]).await;
    let mut stdout = process.stdout().unwrap();
    let out = read_to_end(&mut stdout).await;
    assert_eq!(out.lines().count(), 100_000);
    assert!(out.ends_with("99999\r\n100000\r\n"));
    assert!(succeeded(&mut process).await);

    let mut process = exec(&client, WEB, &["bash"]).await;
    let mut stdin = process.stdin().unwrap();
    let mut stdout = process.stdout().unwrap();
    read_until(&mut stdout, PROMPT).await;
    stdin.write_all(b"yes\r").await.unwrap();
    let mut buf = vec![0; 64 * 1024];
    let mut flooded = 0;
    while flooded < 256 * 1024 {
        flooded += stdout.read(&mut buf).await.unwrap();
    }
    stdin.write_all(b"\x03").await.unwrap();
    read_until(&mut stdout, &format!("^C\r\n{PROMPT}")).await;
    stdin.write_all(b"echo $?\r").await.unwrap();
    assert!(read_until(&mut stdout, PROMPT).await.contains("130\r\n"));
}

#[tokio::test]
async fn frames_come_in_the_recorded_order() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    let request = http::Request::get(format!(
        "/api/v1/namespaces/demo/pods/{WEB}/exec?command=sh&command=-c&command=exit+3\
         &stdin=true&stdout=true&tty=true"
    ))
    .body(Vec::new())
    .unwrap();
    let connection = client.connect(request).await.unwrap();
    assert!(connection.supports_stream_close());
    let mut ws = connection.into_stream();
    let mut frames = Vec::new();
    while let Some(Ok(message)) = tokio::time::timeout(SHORT, ws.next()).await.unwrap() {
        frames.push(message);
    }
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert_eq!(frames[0], Message::binary(vec![1]));
    let Message::Binary(status) = &frames[1] else {
        panic!("{frames:?}");
    };
    assert_eq!(status[0], 3);
    let status: Value = serde_json::from_slice(&status[1..]).unwrap();
    assert_eq!(status["details"]["causes"][0]["message"], "3");
    let Message::Close(Some(close)) = &frames[2] else {
        panic!("{frames:?}");
    };
    assert_eq!(u16::from(close.code), 1000);
    assert_eq!(close.reason.as_str(), "");
    let _ = ws.close(None).await;
}

/// A raw exec connection whose frames nobody reads.
async fn unread(client: &kube::Client, query: &str) -> Ws2 {
    let request = http::Request::get(format!(
        "/api/v1/namespaces/demo/pods/{WEB}/exec?{query}&stdin=true&stdout=true&tty=true"
    ))
    .body(Vec::new())
    .unwrap();
    client.connect(request).await.unwrap().into_stream()
}

type Ws2 = tokio_tungstenite::WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

#[tokio::test]
async fn a_session_stuck_writing_still_ends_when_dropped_or_the_server_stops() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    // `yes` fills every buffer on the way to a client that never reads.
    let _stuck = unread(&client, "command=yes").await;
    server.wait_for_exec_sessions(1, SHORT).await.unwrap();
    let session = server.last_exec().unwrap();
    let flooded = |s: &FakeExec| s.stdout_len();
    tokio::time::timeout(SHORT, async {
        // Until writes stop going through.
        let mut last = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let now = flooded(&session);
            if now > 0 && now == last {
                break;
            }
            last = now;
        }
    })
    .await
    .unwrap();
    assert!(session.drop_connection());
    assert_eq!(session.wait_for_end(SHORT).await.unwrap(), ExecEnd::Dropped);

    let _stuck = unread(&client, "command=yes").await;
    server.wait_for_exec_sessions(1, SHORT).await.unwrap();
    let session = server.last_exec().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(server);
    assert_eq!(session.wait_for_end(SHORT).await.unwrap(), ExecEnd::Dropped);
}

#[tokio::test]
async fn a_stalled_session_goes_silent_until_dropped() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    let mut process = exec(&client, WEB, &["bash"]).await;
    let mut stdout = process.stdout().unwrap();
    let mut stdin = process.stdin().unwrap();
    read_until(&mut stdout, PROMPT).await;
    let session = server.last_exec().unwrap();
    assert!(session.stall());
    // Stalled: what the client types is neither read nor echoed, and nothing ends.
    stdin.write_all(b"echo hi\r").await.unwrap();
    let mut buf = [0; 64];
    let read = tokio::time::timeout(Duration::from_millis(300), stdout.read(&mut buf)).await;
    assert!(read.is_err(), "the stalled session wrote {read:?}");
    assert!(session.is_open());
    assert!(!session.stdin().ends_with(b"echo hi\r"));
    assert!(session.drop_connection());
    assert_eq!(session.wait_for_end(SHORT).await.unwrap(), ExecEnd::Dropped);
    assert!(
        tokio::time::timeout(SHORT, process.join())
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn stdin_after_the_end_is_recorded_though_nothing_reads_it() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    let mut ws = unread(&client, "command=sh&command=-c&command=exit+3").await;
    // The empty frame, then the status: the command is over.
    for _ in 0..2 {
        tokio::time::timeout(SHORT, ws.next()).await.unwrap();
    }
    ws.send(Message::binary(exec::frame(STDIN, b"late")))
        .await
        .unwrap();
    let session = server.last_exec().unwrap();
    session.wait_for_stdin("late", SHORT).await.unwrap();
    assert_eq!(session.end(), Some(ExecEnd::Exited(3)));
}

#[tokio::test]
async fn kubelet_refusals_come_after_the_upgrade_when_the_api_server_holds_the_websocket() {
    let server = FakeApiServer::start_with(Options {
        kubelet_websockets: false,
        ..Options::default()
    })
    .await
    .unwrap();
    let client = server.exec_client().await.unwrap();
    let mut process = exec(&client, "fixture-job-qd9jf", &["sh"]).await;
    let status = status_of(&mut process).await.unwrap();
    assert_eq!(status.code, Some(500));
    assert_eq!(
        status.message.as_deref(),
        Some(
            "Internal error occurred: unable to upgrade connection: container not found (\"job\")"
        )
    );
    let raw = http::Request::get(format!(
        "/api/v1/namespaces/demo/pods/{WEB}/exec?command=sh&tty=true"
    ))
    .body(Vec::new())
    .unwrap();
    let mut ws = client.connect(raw).await.unwrap().into_stream();
    let mut frames = Vec::new();
    while let Some(Ok(message)) = tokio::time::timeout(SHORT, ws.next()).await.unwrap() {
        frames.push(message);
    }
    // No stdout asked for, so no empty stdout frame: the status comes first.
    let Message::Binary(status) = &frames[0] else {
        panic!("{frames:?}");
    };
    assert_eq!(status[0], ERROR);
    assert!(
        String::from_utf8_lossy(status).contains("you must specify at least 1 of stdin"),
        "{frames:?}"
    );
}

#[tokio::test]
async fn proxies_that_strip_the_upgrade_and_create_rbac_refuse_as_the_api_server_does() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.exec_client().await.unwrap();
    let tty = AttachParams::interactive_tty();
    server.inject(Fault::strip_upgrade().times(1));
    match exec_with(&client, WEB, &["sh"], &tty).await {
        Err(e) => assert_eq!(api_error(e), (400, "Upgrade request required".to_owned())),
        Ok(_) => panic!("the upgrade went through"),
    }
    server.inject(Fault::forbidden_as("create").times(1));
    match exec_with(&client, WEB, &["sh"], &tty).await {
        Err(e) => {
            let (code, message) = api_error(e);
            assert_eq!(code, 403);
            assert!(
                message.contains("cannot create resource \"pods/exec\""),
                "{message}"
            );
        }
        Ok(_) => panic!("the exec started"),
    }
    // Both were used up; the next exec starts.
    let mut process = exec(&client, WEB, &["bash"]).await;
    read_until(&mut process.stdout().unwrap(), PROMPT).await;
}
