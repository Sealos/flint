//! The resource actions (UP-53, UP-54) through `review_write` and `run_write`, against the fake
//! API server: delete, scale, rollout restart, cordon, uncordon and drain. No cluster needed.

use std::time::Duration;

use flint_core::WriteRefusal;
use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::write::{
    BlockedReason, DrainReport, PodName, WriteError, WriteOutcome, WriteProposal, WriteTarget,
};
use flint_core::{KindId, Settings, WatchMode, WatchTiming};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, FakePod, Fault, Gvr, Verb};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);
const NODE: &str = "worker-1";

struct Core {
    handle: CoreHandle,
    _dir: tempfile::TempDir,
}

impl Core {
    async fn start(server: &FakeApiServer, settings: Settings) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server.write_kubeconfig(&kubeconfig, Some("demo")).unwrap();
        let handle = CoreHandle::start(CoreConfig {
            config_dir: dir.path().join("config"),
            settings: Settings {
                watch_mode: WatchMode::OnView,
                ..settings
            },
            settings_error: None,
            kubeconfig_base: vec![kubeconfig],
            home: None,
            watch_files: false,
            watch_timing: WatchTiming {
                backoff_min: Duration::from_millis(10),
                backoff_max: Duration::from_millis(50),
                denied_backoff_min: Duration::from_millis(10),
            },
        })
        .unwrap();
        let events = handle.events();
        handle.send(Command::Activate(CONTEXT.to_owned()));
        tokio::time::timeout(WAIT, async {
            loop {
                match events.recv().await.expect("core running") {
                    Event::ConnectionChanged {
                        state: ConnectionState::Failed(e),
                        ..
                    } => panic!("connection failed: {e}"),
                    Event::DiscoveryChanged {
                        discovery: Some(_), ..
                    } => return,
                    _ => {}
                }
            }
        })
        .await
        .expect("discovery in time");
        Core { handle, _dir: dir }
    }

    async fn act(
        &self,
        target: WriteTarget,
        proposal: WriteProposal,
    ) -> Result<WriteOutcome, WriteError> {
        let review = self.handle.review_write(target, proposal).await?;
        self.handle.run_write(review.into_intent()).await
    }
}

fn deployments() -> Gvr {
    Gvr::new("apps", "v1", "deployments")
}

fn nodes() -> Gvr {
    Gvr::core("nodes")
}

fn workload(server: &FakeApiServer, replicas: u32) {
    server.set_object(
        &deployments(),
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "act", "namespace": "demo", "uid": "uid-act",
                         "resourceVersion": "5000"},
            "spec": {"replicas": replicas, "template": {"metadata": {}}},
        }),
    );
}

fn workload_target(server: &FakeApiServer) -> WriteTarget {
    WriteTarget {
        cluster: CONTEXT.into(),
        server: Some(server.url()),
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: "act".to_owned(),
        uid: None,
        resource_version: None,
    }
}

fn node_target(server: &FakeApiServer) -> WriteTarget {
    WriteTarget {
        cluster: CONTEXT.into(),
        server: Some(server.url()),
        kind: KindId::new("", "nodes"),
        namespace: None,
        name: NODE.to_owned(),
        uid: None,
        resource_version: None,
    }
}

fn node(server: &FakeApiServer) {
    server.set_object(
        &nodes(),
        json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {"name": NODE, "uid": "uid-node", "resourceVersion": "5001"},
            "spec": {},
        }),
    );
}

fn live(server: &FakeApiServer) -> Value {
    server
        .object(&deployments(), Some("demo"), "act")
        .expect("the workload")
}

/// A running pod on [`NODE`], owned by a controller of `owner` kind.
fn pod_on_node(name: &str, owner: &str) -> FakePod {
    FakePod::from_json(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name, "namespace": "demo", "uid": format!("uid-{name}"),
            "resourceVersion": "5002",
            "creationTimestamp": "2026-01-01T00:00:00Z",
            "labels": {},
            "ownerReferences": [{"apiVersion": "apps/v1", "kind": owner, "name": "owner",
                                 "uid": "uid-owner", "controller": true}],
        },
        "spec": {"nodeName": NODE, "containers": [{"name": "c", "image": "busybox:1.37.0"}]},
        "status": {"phase": "Running", "containerStatuses": [{
            "name": "c", "ready": true, "restartCount": 0, "image": "busybox:1.37.0",
            "imageID": "", "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}},
        }]},
    }))
}

fn named(name: &str) -> PodName {
    PodName {
        namespace: "demo".to_owned(),
        name: name.to_owned(),
    }
}

fn evictions(server: &FakeApiServer, pod: &str) -> usize {
    let path = format!("/api/v1/namespaces/demo/pods/{pod}/eviction");
    server
        .requests()
        .iter()
        .filter(|r| r.method == http::Method::POST && r.path == path)
        .count()
}

fn report(outcome: WriteOutcome) -> DrainReport {
    outcome.drain.expect("a drain report")
}

fn drain() -> WriteProposal {
    WriteProposal::Drain {
        delete_emptydir_data: false,
        force: false,
    }
}

// covers UP-53: delete, scale and rollout restart change the object they were reviewed for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workload_actions_change_the_reviewed_object() {
    let server = FakeApiServer::start().await.unwrap();
    workload(&server, 2);
    let core = Core::start(&server, Settings::default()).await;

    core.act(workload_target(&server), WriteProposal::Scale { to: 5 })
        .await
        .unwrap();
    assert_eq!(live(&server)["spec"]["replicas"], 5);

    core.act(workload_target(&server), WriteProposal::RolloutRestart)
        .await
        .unwrap();
    assert!(
        live(&server)
            .pointer("/spec/template/metadata/annotations/kubectl.kubernetes.io~1restartedAt")
            .is_some()
    );

    core.act(workload_target(&server), WriteProposal::Delete)
        .await
        .unwrap();
    assert!(server.object(&deployments(), Some("demo"), "act").is_none());
}

// covers UP-52: a count changed after the review is a conflict, sent once; a change to anything
// else, such as the status, is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scale_is_bound_to_the_reviewed_count() {
    let server = FakeApiServer::start().await.unwrap();
    workload(&server, 2);
    let core = Core::start(&server, Settings::default()).await;

    let review = core
        .handle
        .review_write(workload_target(&server), WriteProposal::Scale { to: 5 })
        .await
        .unwrap();
    let mut updated = live(&server);
    updated["status"] = json!({"observedGeneration": 2});
    server.set_object(&deployments(), updated);
    core.handle.run_write(review.into_intent()).await.unwrap();
    assert_eq!(live(&server)["spec"]["replicas"], 5);

    let review = core
        .handle
        .review_write(workload_target(&server), WriteProposal::Scale { to: 9 })
        .await
        .unwrap();
    let mut changed = live(&server);
    changed["spec"]["replicas"] = json!(7);
    server.set_object(&deployments(), changed);
    let before = server.requests().len();
    let result = core.handle.run_write(review.into_intent()).await;
    assert_eq!(result.unwrap_err(), WriteError::Conflict);
    assert_eq!(live(&server)["spec"]["replicas"], 7);
    let patches = server.requests()[before..]
        .iter()
        .filter(|r| r.method == http::Method::PATCH)
        .count();
    assert_eq!(patches, 1, "not retried");
}

// covers UP-53: cordon and uncordon set the node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cordon_and_uncordon() {
    let server = FakeApiServer::start().await.unwrap();
    node(&server);
    let core = Core::start(&server, Settings::default()).await;
    let unschedulable = |server: &FakeApiServer| {
        server.object(&nodes(), None, NODE).unwrap()["spec"]["unschedulable"].clone()
    };

    core.act(node_target(&server), WriteProposal::Cordon)
        .await
        .unwrap();
    assert_eq!(unschedulable(&server), true);
    core.act(node_target(&server), WriteProposal::Uncordon)
        .await
        .unwrap();
    assert_eq!(unschedulable(&server), false);
}

// covers UP-53: a drain cordons, evicts, leaves DaemonSet pods, and reports a pod a
// PodDisruptionBudget protects after one try.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_reports_pods_a_budget_protects() {
    let server = FakeApiServer::start().await.unwrap();
    node(&server);
    server.add_pod(pod_on_node("agent", "DaemonSet"));
    server.add_pod(pod_on_node("guarded", "ReplicaSet"));
    server.add_pod(pod_on_node("web", "ReplicaSet"));
    let core = Core::start(&server, Settings::default()).await;
    server.inject(
        Fault::status(429)
            .on(&Gvr::core("pods"))
            .named("guarded")
            .subresource("eviction"),
    );

    let drained = report(core.act(node_target(&server), drain()).await.unwrap());
    server.clear_faults();

    assert!(drained.cordoned);
    assert_eq!(
        server.object(&nodes(), None, NODE).unwrap()["spec"]["unschedulable"],
        true
    );
    assert_eq!(drained.evicted, [named("web")]);
    assert_eq!(drained.skipped, [named("agent")]);
    assert_eq!(
        drained.blocked,
        [(named("guarded"), BlockedReason::DisruptionBudget)]
    );
    assert!(drained.remaining.is_empty());
    assert_eq!(drained.stopped, None);
    assert_eq!(evictions(&server, "guarded"), 1, "not retried");
    assert_eq!(evictions(&server, "agent"), 0);
    let pods = Gvr::core("pods");
    assert!(server.object(&pods, Some("demo"), "web").is_none());
    assert!(server.object(&pods, Some("demo"), "guarded").is_some());
}

// covers UP-54: locking the cluster during a drain stops the next eviction, and the outcome says
// what was done and what is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locking_during_a_drain_stops_the_next_eviction() {
    let server = FakeApiServer::start().await.unwrap();
    node(&server);
    for name in ["pod-a", "pod-b", "pod-c"] {
        server.add_pod(pod_on_node(name, "ReplicaSet"));
    }
    let settings = Settings {
        protect_local_clusters: true,
        ..Settings::default()
    };
    let core = Core::start(&server, settings).await;
    core.handle.unlock(CONTEXT, &server.url());
    let review = core
        .handle
        .review_write(node_target(&server), drain())
        .await
        .unwrap();
    server.inject(
        Fault::delay(Duration::from_millis(400))
            .on(&Gvr::core("pods"))
            .verb(Verb::Other)
            .named("pod-a")
            .subresource("eviction")
            .times(1),
    );
    let run = core.handle.run_write(review.into_intent());
    let running = tokio::spawn(run);
    tokio::time::timeout(WAIT, async {
        let first = "/api/v1/namespaces/demo/pods/pod-a/eviction";
        while !server.arrivals().iter().any(|r| r.path == first) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first eviction");
    assert!(core.handle.lock(CONTEXT));

    let drained = report(running.await.unwrap().unwrap());
    assert!(drained.cordoned);
    assert_eq!(drained.evicted, [named("pod-a")]);
    assert_eq!(drained.remaining, [named("pod-b"), named("pod-c")]);
    assert_eq!(
        drained.stopped,
        Some(WriteError::Refused(WriteRefusal::Protected {
            cluster: CONTEXT.to_owned(),
        }))
    );
    assert_eq!(evictions(&server, "pod-b"), 0);
    assert_eq!(evictions(&server, "pod-c"), 0);
}

// covers UP-54: on a protected cluster an action is refused at review, and nothing is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_cluster_refuses_actions() {
    let server = FakeApiServer::start().await.unwrap();
    workload(&server, 2);
    node(&server);
    let settings = Settings {
        protect_local_clusters: true,
        ..Settings::default()
    };
    let core = Core::start(&server, settings).await;
    let protected = WriteError::Refused(WriteRefusal::Protected {
        cluster: CONTEXT.to_owned(),
    });
    for (target, proposal) in [
        (workload_target(&server), WriteProposal::Delete),
        (workload_target(&server), WriteProposal::Scale { to: 3 }),
        (workload_target(&server), WriteProposal::RolloutRestart),
        (node_target(&server), WriteProposal::Cordon),
        (node_target(&server), drain()),
    ] {
        assert_eq!(core.act(target, proposal).await.unwrap_err(), protected);
    }
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.method == http::Method::GET),
        "only reads reached the server"
    );
}
