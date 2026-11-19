//! UP-27 on a real cluster: the API server goes away and comes back. Slow and disruptive (it
//! stops the kind control-plane container), so besides `--include-ignored` it needs
//! `FLINT_TEST_SLOW=1`; without it the test skips and says so. Run it on its own:
//!
//! ```text
//! FLINT_TEST_KUBECONFIG=.dev/kubeconfig FLINT_TEST_SLOW=1 \
//!   cargo test -p flint-core --test watch_recovery -- --ignored --nocapture
//! ```
//!
//! `FLINT_TEST_CONTROL_PLANE` names the container (default `flint-dev-control-plane`). The test
//! starts it again even when it fails, and creates objects only in namespace
//! `m1fix-recovery`, which it deletes.

use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::{KindId, KindSnapshot, Settings, Subscription, SyncState, cluster, kubeconfig};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace};
use kube::Client;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;

const NAMESPACE: &str = "m1fix-recovery";
/// How long the lists may take to be live again after the API server is back.
const RECOVERY_BOUND: Duration = Duration::from_secs(90);

fn docker(args: &[&str]) {
    let status = Process::new("docker")
        .args(args)
        .status()
        .expect("docker runs");
    assert!(status.success(), "docker {args:?} failed");
}

/// Starts the control plane again when dropped, so a failing test leaves the cluster up.
struct Restart(String);

impl Drop for Restart {
    fn drop(&mut self) {
        let _ = Process::new("docker").args(["start", &self.0]).status();
    }
}

async fn until(
    sub: &mut Subscription,
    within: Duration,
    what: &str,
    pred: impl Fn(&KindSnapshot) -> bool,
) -> Arc<KindSnapshot> {
    tokio::time::timeout(within, async {
        loop {
            let snap = sub.latest();
            if pred(&snap) {
                return snap;
            }
            assert!(sub.changed().await, "core stopped");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} not reached: {:?}", sub.peek().state))
}

/// A client that waits for the API server to answer again.
async fn admin(path: &PathBuf, within: Duration) -> Client {
    let loaded = kubeconfig::load(std::slice::from_ref(path));
    let context = loaded.contexts[0].name.clone();
    let deadline = Instant::now() + within;
    loop {
        if let Ok((client, _)) =
            cluster::connect_and_check(&loaded.merged, &context, Duration::from_secs(5)).await
        {
            return client;
        }
        assert!(
            Instant::now() < deadline,
            "the API server did not come back"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// covers UP-27
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow: stops the kind control plane; needs FLINT_TEST_KUBECONFIG and FLINT_TEST_SLOW=1"]
async fn lists_recover_after_the_api_server_restarts() {
    if std::env::var("FLINT_TEST_SLOW").as_deref() != Ok("1") {
        eprintln!(
            "SKIPPED lists_recover_after_the_api_server_restarts: it stops the kind control \
             plane for about a minute; set FLINT_TEST_SLOW=1 to run it"
        );
        return;
    }
    let path = PathBuf::from(
        std::env::var("FLINT_TEST_KUBECONFIG").expect("FLINT_TEST_KUBECONFIG must be set"),
    );
    let container = std::env::var("FLINT_TEST_CONTROL_PLANE")
        .unwrap_or_else(|_| "flint-dev-control-plane".to_owned());
    let kube = admin(&path, Duration::from_secs(10)).await;
    let namespaces: Api<Namespace> = Api::all(kube.clone());
    let _ = namespaces.delete(NAMESPACE, &DeleteParams::default()).await;
    while namespaces.get_opt(NAMESPACE).await.unwrap().is_some() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    namespaces
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({"metadata": {"name": NAMESPACE}})).unwrap(),
        )
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().to_owned(),
        settings: Settings::default(),
        settings_error: None,
        kubeconfig_base: vec![path.clone()],
        home: None,
        watch_files: false,
    })
    .unwrap();
    let events = core.events();
    let name = loop {
        if let Event::ClustersChanged(s) = events.recv().await.unwrap() {
            break s.clusters[0].info.name.clone();
        }
    };
    core.send(Command::Activate(name.clone()));
    let drain = tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            if let Event::ConnectionChanged {
                state: ConnectionState::Failed(msg),
                ..
            } = e
            {
                eprintln!("connection failed: {msg}");
            }
        }
    });
    // All mode: every kind is watched, as in daily use.
    let mut pods = core.subscribe(&name, &KindId::pods());
    let configmaps = KindId::new("", "configmaps");
    let mut cms = core.subscribe(&name, &configmaps);
    until(&mut pods, Duration::from_secs(60), "pods synced", |s| {
        s.state == SyncState::Synced
    })
    .await;
    until(
        &mut cms,
        Duration::from_secs(60),
        "configmaps synced",
        |s| s.state == SyncState::Synced,
    )
    .await;
    let watched = core.watched_kinds(&name).await.len();

    // The API server goes away: the lists keep their rows and say they are reconnecting.
    let restart = Restart(container.clone());
    docker(&["stop", &container]);
    let stale = until(
        &mut pods,
        Duration::from_secs(200),
        "pods reconnecting",
        |s| matches!(s.state, SyncState::Reconnecting { .. }),
    )
    .await;
    assert!(!stale.rows.is_empty(), "rows are kept, marked as stale");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // It comes back: the lists are live again on their own, within the bound.
    docker(&["start", &container]);
    drop(restart);
    let back = Instant::now();
    until(&mut pods, RECOVERY_BOUND, "pods synced again", |s| {
        s.state == SyncState::Synced
    })
    .await;
    until(&mut cms, RECOVERY_BOUND, "configmaps synced again", |s| {
        s.state == SyncState::Synced
    })
    .await;
    let recovered = back.elapsed();
    eprintln!("lists live again {recovered:?} after the control plane started");
    // Every watched kind recovers, none is left failed or reconnecting.
    let deadline = back + RECOVERY_BOUND;
    loop {
        let states = core.sync_states(&name);
        let stuck: Vec<_> = states
            .iter()
            .filter(|(_, s)| !matches!(s, SyncState::Synced | SyncState::Idle))
            .collect();
        if stuck.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "kinds still not live after {RECOVERY_BOUND:?}: {stuck:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(core.watched_kinds(&name).await.len(), watched);

    // And live updates flow again.
    let kube = admin(&path, Duration::from_secs(60)).await;
    let api: Api<ConfigMap> = Api::namespaced(kube.clone(), NAMESPACE);
    api.create(
        &PostParams::default(),
        &serde_json::from_value(json!({"metadata": {"name": "after-restart"}})).unwrap(),
    )
    .await
    .unwrap();
    until(
        &mut cms,
        Duration::from_secs(10),
        "a new object after the restart",
        |s| {
            s.rows
                .iter()
                .any(|r| r.namespace.as_deref() == Some(NAMESPACE) && &*r.name == "after-restart")
        },
    )
    .await;

    drain.abort();
    drop(pods);
    drop(cms);
    let namespaces: Api<Namespace> = Api::all(kube);
    let _ = namespaces.delete(NAMESPACE, &DeleteParams::default()).await;
}
