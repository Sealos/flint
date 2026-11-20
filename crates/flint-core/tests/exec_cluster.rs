//! Exec sessions against the dev cluster, through the public `CoreHandle` API. Ignored by
//! default; run with `--include-ignored` and `FLINT_TEST_KUBECONFIG` pointing at the throwaway
//! kind cluster (`scripts/dev-cluster.sh`). They use that file alone, and create one pod in a
//! namespace prefixed `exec-test-`, which they delete again.

use std::sync::Arc;
use std::time::{Duration, Instant};

use flint_core::handle::{Command, ConnectionState, Event};
use flint_core::{ExecRequest, ExecSession, ExecSnapshot, ExecState, TerminalSize};
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::Client;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;

mod support;

use support::{Core, client, init_logging, two_context_kubeconfig, unique_name};

/// A second context for the same cluster, to switch the active cluster away and back.
const ALIAS: &str = "exec-test-alias";

/// Creates the namespace and a pod with a `bash` container (nginx, Debian) and an `sh` one
/// (podinfo, Alpine: no bash), and waits until both run.
async fn create_pod(client: &Client, namespace: &str) {
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let ns = serde_json::from_value(json!({"metadata": {"name": namespace}})).unwrap();
    namespaces
        .create(&PostParams::default(), &ns)
        .await
        .unwrap();
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let pod = serde_json::from_value(json!({
        "metadata": {"name": "shell"},
        "spec": {
            "terminationGracePeriodSeconds": 0,
            "containers": [
                {"name": "bash", "image": "nginx:1.27", "imagePullPolicy": "IfNotPresent"},
                {"name": "sh", "image": "ghcr.io/stefanprodan/podinfo:6.17.0",
                 "imagePullPolicy": "IfNotPresent"}
            ]
        }
    }))
    .unwrap();
    pods.create(&PostParams::default(), &pod).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let pod = pods.get("shell").await.unwrap();
        let ready = pod
            .status
            .and_then(|s| s.container_statuses)
            .is_some_and(|cs| cs.len() == 2 && cs.iter().all(|c| c.ready));
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "the test pod did not start");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn delete_namespace(client: &Client, namespace: &str) {
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let _ = namespaces
        .delete(namespace, &DeleteParams::background().grace_period(0))
        .await;
}

impl Core {
    async fn connect(&self, name: &str) {
        self.handle.send(Command::Activate(name.to_owned()));
        self.wait_for_event(60, |event| match event {
            Event::ConnectionChanged { cluster, state } if cluster == name => match state {
                ConnectionState::Connected(_) => Some(()),
                ConnectionState::Failed(e) => panic!("connection failed: {e}"),
                _ => None,
            },
            _ => None,
        })
        .await
        .expect("connected within 60 s");
    }
}

async fn wait_for(
    session: &mut ExecSession,
    what: &str,
    pred: impl Fn(&ExecSnapshot) -> bool,
) -> Arc<ExecSnapshot> {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snap = session.latest();
            if pred(&snap) {
                return snap;
            }
            if !session.changed().await {
                panic!("session ended waiting for {what}");
            }
        }
    })
    .await;
    result.unwrap_or_else(|_| {
        let snap = session.peek();
        panic!(
            "timed out waiting for {what}: {:?}\n{}",
            snap.state,
            snap.terminal.text()
        )
    })
}

fn has_line(snap: &ExecSnapshot, line: &str) -> bool {
    (0..snap.terminal.size.rows).any(|r| snap.terminal.row_text(r) == line)
}

async fn shell_sessions(core: &Core, context: &str, namespace: &str) {
    core.connect(context).await;

    // Alpine has no bash: the session falls back to /bin/sh.
    let request = ExecRequest::shell(context, namespace, "shell")
        .container("sh")
        .size(TerminalSize::new(80, 24));
    let mut sh = core.handle.exec(request);
    wait_for(&mut sh, "sh to start", |s| {
        s.state
            == ExecState::Running {
                command: vec!["/bin/sh".to_owned()],
            }
    })
    .await;
    sh.write(b"echo $((6*7))\r".to_vec()).unwrap();
    wait_for(&mut sh, "the echo", |s| has_line(s, "42")).await;

    // The TTY follows resizes.
    sh.resize(TerminalSize::new(100, 30));
    // kube sends resizes and input on separate channels, so they can overtake each other, and
    // busybox's line editor drops a line typed while it handles the resize: give it a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sh.write(b"stty size\r".to_vec()).unwrap();
    wait_for(&mut sh, "the new size", |s| has_line(s, "30 100")).await;

    // Switching the active cluster leaves the session running (UP-24, UP-42).
    core.connect(ALIAS).await;
    sh.write(b"echo still-$((1+1))\r".to_vec()).unwrap();
    wait_for(&mut sh, "output after the switch", |s| {
        has_line(s, "still-2")
    })
    .await;

    sh.write(b"exit 3\r".to_vec()).unwrap();
    let snap = wait_for(&mut sh, "the exit", |s| s.state.is_finished()).await;
    assert_eq!(snap.state, ExecState::Exited { code: Some(3) });

    // Debian has bash.
    let mut bash = core
        .handle
        .exec(ExecRequest::shell(context, namespace, "shell").container("bash"));
    wait_for(&mut bash, "bash to start", |s| {
        s.state
            == ExecState::Running {
                command: vec!["bash".to_owned()],
            }
    })
    .await;
    bash.write(b"echo ${BASH_VERSION%%.*}-ok\r".to_vec())
        .unwrap();
    wait_for(&mut bash, "bash output", |s| {
        (0..s.terminal.size.rows).any(|r| {
            let line = s.terminal.row_text(r);
            line.ends_with("-ok") && !line.contains("echo")
        })
    })
    .await;
    bash.write(b"exit\r".to_vec()).unwrap();
    let snap = wait_for(&mut bash, "the exit", |s| s.state.is_finished()).await;
    assert_eq!(snap.state, ExecState::Exited { code: Some(0) });

    // A pod that does not exist fails with the API server's reason.
    let mut missing = core
        .handle
        .exec(ExecRequest::shell(context, namespace, "nope"));
    let snap = wait_for(&mut missing, "the failure", |s| s.state.is_finished()).await;
    match &snap.state {
        ExecState::Failed(message) => assert!(message.contains("NotFound"), "{message}"),
        other => panic!("{other:?}"),
    }
}

// covers UP-41, UP-42, UP-24
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn shell_into_a_container() {
    init_logging();
    let dir = tempfile::tempdir().unwrap();
    let (path, context, _) = two_context_kubeconfig(dir.path(), Some(ALIAS));
    let admin = client(&path).await;
    let namespace = unique_name("exec-test");
    let core = Core::start(path, dir.path().join("config"), Default::default());
    // Run the checks on their own task so the namespace is deleted even when one fails.
    let result = tokio::spawn({
        let namespace = namespace.clone();
        let admin = admin.clone();
        async move {
            create_pod(&admin, &namespace).await;
            shell_sessions(&core, &context, &namespace).await
        }
    })
    .await;
    delete_namespace(&admin, &namespace).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}
