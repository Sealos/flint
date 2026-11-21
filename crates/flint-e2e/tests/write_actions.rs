//! The resource actions (UP-53, UP-54) through `review_write` and `run_write`, against the fake
//! API server: delete, scale, rollout restart, cordon, uncordon and drain. No cluster needed.

use std::path::PathBuf;
use std::time::Duration;

use flint_core::WriteRefusal;
use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::write::{
    BlockedReason, DrainReport, PodName, WriteError, WriteOutcome, WriteProposal, WriteTarget,
    WriteWarning,
};
use flint_core::{KindId, Settings, WatchMode, WatchTiming};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, FakePod, Fault, Gvr, Verb};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);
const NODE: &str = "worker-1";
const FLUX_LABEL: &str = "kustomize.toolkit.fluxcd.io/name";

struct Core {
    handle: CoreHandle,
    _dir: tempfile::TempDir,
}

impl Core {
    async fn start(server: &FakeApiServer, settings: Settings) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server.write_kubeconfig(&kubeconfig, Some("demo")).unwrap();
        Core::start_on(dir, vec![kubeconfig], settings).await
    }

    /// A core reading the kubeconfig files `paths`, in order, with its config in `dir`.
    async fn start_on(dir: tempfile::TempDir, paths: Vec<PathBuf>, settings: Settings) -> Core {
        let handle = CoreHandle::start(CoreConfig {
            config_dir: dir.path().join("config"),
            settings: Settings {
                watch_mode: WatchMode::OnView,
                ..settings
            },
            settings_error: None,
            kubeconfig_base: paths,
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
    FakePod::from_json(pod_json(name, owner))
}

/// A pod on the node with no controller.
fn bare_pod(name: &str) -> FakePod {
    let mut pod = pod_json(name, "ReplicaSet");
    pod["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("ownerReferences");
    FakePod::from_json(pod)
}

/// A pod on the node that Flux manages.
fn managed_pod(name: &str, owner: &str) -> FakePod {
    let mut pod = pod_json(name, owner);
    pod["metadata"]["labels"] = json!({FLUX_LABEL: "apps"});
    FakePod::from_json(pod)
}

fn pod_json(name: &str, owner: &str) -> Value {
    json!({
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
    })
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

// covers UP-53: scaling from zero goes through the scale subresource too, which omits the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scale_from_zero() {
    let server = FakeApiServer::start().await.unwrap();
    workload(&server, 0);
    let core = Core::start(&server, Settings::default()).await;
    core.act(workload_target(&server), WriteProposal::Scale { to: 2 })
        .await
        .unwrap();
    assert_eq!(live(&server)["spec"]["replicas"], 2);
    let path = "/apis/apps/v1/namespaces/demo/deployments/act/scale";
    assert!(
        server
            .requests()
            .iter()
            .any(|r| r.method == http::Method::PATCH && r.path == path)
    );
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
        Fault::disruption_budget()
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

const ACTIONS: usize = 6;

/// Every action, on the object it acts on: its target, what it does, and that object's path.
fn every_action(server: &FakeApiServer) -> Vec<(WriteTarget, WriteProposal, Gvr, &'static str)> {
    let workload_path = "/apis/apps/v1/namespaces/demo/deployments/act";
    let node_path = "/api/v1/nodes/worker-1";
    vec![
        (
            workload_target(server),
            WriteProposal::Delete,
            deployments(),
            workload_path,
        ),
        (
            workload_target(server),
            WriteProposal::Scale { to: 3 },
            deployments(),
            workload_path,
        ),
        (
            workload_target(server),
            WriteProposal::RolloutRestart,
            deployments(),
            workload_path,
        ),
        (
            node_target(server),
            WriteProposal::Cordon,
            nodes(),
            node_path,
        ),
        (
            node_target(server),
            WriteProposal::Uncordon,
            nodes(),
            node_path,
        ),
        (node_target(server), drain(), nodes(), node_path),
    ]
}

/// Adds Flux's label to the object `target` names.
fn add_marker(server: &FakeApiServer, gvr: &Gvr, namespace: Option<&str>, name: &str) {
    let mut object = server.object(gvr, namespace, name).expect("the object");
    object["metadata"]["labels"] = json!({FLUX_LABEL: "apps"});
    server.set_object(gvr, object);
}

/// The requests since the first `from`, without reads of the API's discovery.
fn since(server: &FakeApiServer, from: usize) -> Vec<(http::Method, String)> {
    server.requests()[from..]
        .iter()
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect()
}

fn writes_in(requests: &[(http::Method, String)]) -> usize {
    requests
        .iter()
        .filter(|(method, _)| *method != http::Method::GET)
        .count()
}

// covers UP-51, UP-53: a GitOps marker added after the review is a conflict, and nothing is
// written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marker_added_after_the_review_is_a_conflict() {
    for action in 0..ACTIONS {
        // A server of its own: the marker stays on the object's row.
        let server = FakeApiServer::start().await.unwrap();
        workload(&server, 2);
        node(&server);
        let core = Core::start(&server, Settings::default()).await;
        let (target, proposal, gvr, path) = every_action(&server).swap_remove(action);
        let review = core
            .handle
            .review_write(target.clone(), proposal)
            .await
            .unwrap();
        assert!(review.warnings().is_empty());
        add_marker(&server, &gvr, target.namespace.as_deref(), &target.name);
        let from = server.requests().len();
        let result = core.handle.run_write(review.into_intent()).await;
        assert_eq!(result.unwrap_err(), WriteError::Conflict, "{path}");
        let seen = since(&server, from);
        assert_eq!(writes_in(&seen), 0, "{seen:?}");
        assert_eq!(seen[0], (http::Method::GET, path.to_owned()));
    }
}

/// Runs `review` once the core has reconnected after a reload.
async fn run_when_connected(
    core: &Core,
    review: &flint_core::write::WriteReview,
) -> Result<WriteOutcome, WriteError> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let result = core.handle.run_write(review.clone().into_intent()).await;
        let waiting = matches!(
            &result,
            Err(WriteError::Refused(WriteRefusal::NotConnected { .. }))
                | Err(WriteError::NotServed(_))
        );
        if !waiting || tokio::time::Instant::now() > deadline {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// covers UP-54: every action is bound to the connection definition it was reviewed on. A changed
// user entry, or a definition a file that can't be read could change, sends no request for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_or_provisional_definition_sends_no_action() {
    for provisional in [false, true] {
        for action in 0..ACTIONS {
            let server = FakeApiServer::start().await.unwrap();
            workload(&server, 2);
            node(&server);
            let dir = tempfile::tempdir().unwrap();
            let (first, kubeconfig) = (dir.path().join("first"), dir.path().join("kubeconfig"));
            std::fs::write(&first, "apiVersion: v1\nkind: Config\n").unwrap();
            server.write_kubeconfig(&kubeconfig, Some("demo")).unwrap();
            let paths = vec![first.clone(), kubeconfig.clone()];
            let core = Core::start_on(dir, paths, Settings::default()).await;
            let (target, proposal, _, path) = every_action(&server).swap_remove(action);
            let review = core.handle.review_write(target, proposal).await.unwrap();

            if provisional {
                std::fs::write(&first, "not: [valid").unwrap();
            } else {
                let text = std::fs::read_to_string(&kubeconfig).unwrap();
                assert!(text.contains("user: {}"), "{text}");
                let edited = text.replace("user: {}", "user:\n    token: another");
                std::fs::write(&kubeconfig, edited).unwrap();
            }
            core.handle.reload_kubeconfig().await.unwrap();
            let from = server.requests().len();
            let result = run_when_connected(&core, &review).await;
            let expected = if provisional {
                WriteError::KubeconfigUnreadable
            } else {
                WriteError::ConnectionChanged
            };
            assert_eq!(result.unwrap_err(), expected, "{path}");
            let seen = since(&server, from);
            assert_eq!(writes_in(&seen), 0, "{path}: {seen:?}");
            assert!(!seen.iter().any(|(_, p)| p == path), "{path}: {seen:?}");
        }
    }
}

// covers UP-53: an unchanged object is read once, and then written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_object_is_read_then_written() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Settings::default()).await;
    for (target, proposal, _, path) in every_action(&server) {
        workload(&server, 2);
        node(&server);
        let review = core.handle.review_write(target, proposal).await.unwrap();
        let from = server.requests().len();
        core.handle.run_write(review.into_intent()).await.unwrap();
        let seen = since(&server, from);
        assert_eq!(seen[0], (http::Method::GET, path.to_owned()), "{seen:?}");
        let write = seen
            .iter()
            .position(|(method, _)| *method != http::Method::GET)
            .expect("a write");
        assert!(write > 0, "{seen:?}");
        let reads = seen[..write].iter().filter(|(_, p)| p == path).count();
        assert_eq!(reads, 1, "{seen:?}");
    }
}

// covers UP-53: a recheck that cannot read the object sends no write and is not retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_recheck_sends_no_write() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Settings::default()).await;
    for (target, proposal, gvr, path) in every_action(&server) {
        workload(&server, 2);
        node(&server);
        let name = target.name.clone();
        let review = core.handle.review_write(target, proposal).await.unwrap();
        server.inject(Fault::status(500).on(&gvr).verb(Verb::Get).named(&name));
        let from = server.requests().len();
        let result = core.handle.run_write(review.into_intent()).await;
        server.clear_faults();
        assert!(result.is_err(), "{path}: {result:?}");
        let seen = since(&server, from);
        assert_eq!(writes_in(&seen), 0, "{seen:?}");
        assert_eq!(
            seen.iter().filter(|(_, p)| p == path).count(),
            1,
            "{seen:?}"
        );
    }
}

// covers UP-51, UP-53: a drain warns about the managed pods it would evict, and only those. The
// node ends cordoned with a normal and a managed pod gone, and the rest reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drain_warns_about_the_managed_pods_it_evicts() {
    let server = FakeApiServer::start().await.unwrap();
    node(&server);
    server.add_pod(pod_on_node("web", "ReplicaSet"));
    server.add_pod(managed_pod("synced", "ReplicaSet"));
    server.add_pod(managed_pod("agent", "DaemonSet"));
    server.add_pod(bare_pod("bare"));
    server.add_pod(pod_on_node("guarded", "ReplicaSet"));
    let core = Core::start(&server, Settings::default()).await;
    let review = core
        .handle
        .review_write(node_target(&server), drain())
        .await
        .unwrap();
    match review.warnings() {
        [WriteWarning::EvictsManaged(pods)] => {
            assert_eq!(pods.len(), 1, "{pods:?}");
            assert_eq!(pods[0].pod, named("synced"));
            assert_eq!(pods[0].managed.to_string(), "Flux: apps");
        }
        other => panic!("{other:?}"),
    }
    server.inject(
        Fault::disruption_budget()
            .on(&Gvr::core("pods"))
            .named("guarded")
            .subresource("eviction"),
    );
    let drained = report(core.handle.run_write(review.into_intent()).await.unwrap());
    server.clear_faults();

    assert!(drained.cordoned);
    let mut evicted = drained.evicted.clone();
    evicted.sort();
    assert_eq!(evicted, [named("synced"), named("web")]);
    assert_eq!(drained.skipped, [named("agent")]);
    assert!(
        drained
            .blocked
            .contains(&(named("bare"), BlockedReason::Unmanaged))
    );
    assert!(
        drained
            .blocked
            .contains(&(named("guarded"), BlockedReason::DisruptionBudget))
    );
    assert_eq!(drained.stopped, None);
    let pods = Gvr::core("pods");
    for gone in ["web", "synced"] {
        assert!(server.object(&pods, Some("demo"), gone).is_none(), "{gone}");
    }
    for kept in ["agent", "bare", "guarded"] {
        assert!(server.object(&pods, Some("demo"), kept).is_some(), "{kept}");
    }
    assert_eq!(
        server.object(&nodes(), None, NODE).unwrap()["spec"]["unschedulable"],
        true
    );
}

// covers UP-51, UP-53: a pod that gains a GitOps marker after the review stops the drain before
// the node is cordoned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_marked_after_the_review_stops_the_drain() {
    let server = FakeApiServer::start().await.unwrap();
    node(&server);
    server.add_pod(pod_on_node("web", "ReplicaSet"));
    let core = Core::start(&server, Settings::default()).await;
    let review = core
        .handle
        .review_write(node_target(&server), drain())
        .await
        .unwrap();
    assert!(review.warnings().is_empty());
    add_marker(&server, &Gvr::core("pods"), Some("demo"), "web");
    let from = server.requests().len();
    let result = core.handle.run_write(review.into_intent()).await;
    assert_eq!(result.unwrap_err(), WriteError::Conflict);
    let seen = since(&server, from);
    assert_eq!(writes_in(&seen), 0, "{seen:?}");
    assert!(server.object(&nodes(), None, NODE).unwrap()["spec"]["unschedulable"].is_null());
}

// covers UP-53: a Deployment paused after the review is a conflict, and no patch is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_paused_after_the_review_is_not_restarted() {
    let server = FakeApiServer::start().await.unwrap();
    workload(&server, 2);
    let core = Core::start(&server, Settings::default()).await;
    let review = core
        .handle
        .review_write(workload_target(&server), WriteProposal::RolloutRestart)
        .await
        .unwrap();
    let mut paused = live(&server);
    paused["spec"]["paused"] = json!(true);
    server.set_object(&deployments(), paused);
    let from = server.requests().len();
    let result = core.handle.run_write(review.into_intent()).await;
    assert_eq!(result.unwrap_err(), WriteError::Conflict);
    let seen = since(&server, from);
    assert_eq!(writes_in(&seen), 0, "{seen:?}");
}

// covers UP-53: a pod the confirmed options no longer cover when its turn comes stays, and no
// eviction is sent for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_that_lost_its_controller_is_not_evicted() {
    let server = FakeApiServer::start().await.unwrap();
    node(&server);
    for name in ["pod-a", "pod-b"] {
        server.add_pod(pod_on_node(name, "ReplicaSet"));
    }
    let core = Core::start(&server, Settings::default()).await;
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
    let running = tokio::spawn(core.handle.run_write(review.into_intent()));
    tokio::time::timeout(WAIT, async {
        let first = "/api/v1/namespaces/demo/pods/pod-a/eviction";
        while !server.arrivals().iter().any(|r| r.path == first) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first eviction");
    let pods = Gvr::core("pods");
    let mut orphan = server.object(&pods, Some("demo"), "pod-b").unwrap();
    orphan["metadata"]["ownerReferences"] = json!([]);
    server.set_object(&pods, orphan);

    let drained = report(running.await.unwrap().unwrap());
    assert_eq!(drained.evicted, [named("pod-a")]);
    assert_eq!(
        drained.blocked,
        [(named("pod-b"), BlockedReason::Unmanaged)]
    );
    assert_eq!(evictions(&server, "pod-b"), 0);
    assert!(server.object(&pods, Some("demo"), "pod-b").is_some());
}
