//! What the API server answers to a reviewed write (UP-51, UP-52, UP-53), read from the fake API
//! server's write recorder: a refusal at the dry run writes nothing, a refusal at the write is
//! reported and never retried, and a competing edit is not overwritten.
//!
//! The context is trusted by its exact server, with local clusters protected, so these tests do
//! not depend on the default protection of a loopback address.

use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::settings::ClusterPin;
use flint_core::write::{WriteError, WriteProposal, WriteTarget};
use flint_core::{KindId, Settings, WatchMode};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Fault, Gvr, Verb, WriteRecord};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);
const NAME: &str = "edit-me";
const WEBHOOK: &str = "policy.example.com";
const DENIED: &str = "replicas are over the limit";

struct Core {
    handle: CoreHandle,
    _dir: tempfile::TempDir,
}

impl Core {
    async fn start(server: &FakeApiServer) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server.write_kubeconfig(&kubeconfig, Some("demo")).unwrap();
        let handle = CoreHandle::start(CoreConfig {
            config_dir: dir.path().join("config"),
            settings: Settings {
                watch_mode: WatchMode::OnView,
                protect_local_clusters: true,
                trusted_clusters: vec![ClusterPin::new(CONTEXT, server.url())],
                ..Settings::default()
            },
            settings_error: None,
            kubeconfig_base: vec![kubeconfig],
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
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
}

fn deployments() -> Gvr {
    Gvr::new("apps", "v1", "deployments")
}

fn object(replicas: u32, version: &str) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": NAME, "namespace": "demo", "uid": "uid-edit",
                     "resourceVersion": version},
        "spec": {"replicas": replicas, "template": {"metadata": {}}},
    })
}

fn target(server: &FakeApiServer) -> WriteTarget {
    WriteTarget {
        cluster: CONTEXT.into(),
        server: Some(server.url()),
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: NAME.to_owned(),
        uid: None,
        resource_version: None,
    }
}

fn edit() -> WriteProposal {
    WriteProposal::ApplyYaml {
        yaml: format!(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {NAME}\nspec:\n  replicas: 9\n"
        ),
    }
}

fn denied() -> Fault {
    Fault::admission_denied(WEBHOOK, DENIED)
        .on(&deployments())
        .verb(Verb::Other)
        .times(1)
}

fn real(server: &FakeApiServer) -> Vec<WriteRecord> {
    server
        .writes()
        .into_iter()
        .filter(|w| !w.is_dry_run())
        .collect()
}

fn replicas(server: &FakeApiServer) -> Value {
    server
        .object(&deployments(), Some("demo"), NAME)
        .expect("the object")["spec"]["replicas"]
        .clone()
}

fn assert_denied(error: WriteError) {
    // An edit reports it as `Rejected`, a resource action as `Failed`; both show the reason.
    let (WriteError::Rejected(text) | WriteError::Failed(text)) = error else {
        panic!("not a refusal: {error:?}");
    };
    // Quoted server text is masked, so the webhook's name is not shown.
    assert!(
        text.contains("admission webhook") && text.contains(DENIED),
        "{text}"
    );
}

// covers UP-51: a webhook that refuses the dry run stops the review, and nothing is applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_webhook_refusing_the_dry_run_stops_the_review() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&deployments(), object(1, "5000"));
    let core = Core::start(&server).await;
    server.inject(denied());

    let error = core
        .handle
        .review_write(target(&server), edit())
        .await
        .unwrap_err();

    assert_denied(error);
    let writes = server.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].is_dry_run());
    assert_eq!(writes[0].status, Some(403));
    assert_eq!(replicas(&server), 1);
}

// covers UP-51, UP-53: a refusal at the write itself is reported, sent once, for every kind of
// request an action makes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_webhook_refusing_the_write_is_reported_and_not_retried() {
    for proposal in [
        edit(),
        WriteProposal::Delete,
        WriteProposal::Scale { to: 9 },
        WriteProposal::RolloutRestart,
    ] {
        let server = FakeApiServer::start().await.unwrap();
        server.set_object(&deployments(), object(1, "5000"));
        let core = Core::start(&server).await;
        let review = core
            .handle
            .review_write(target(&server), proposal.clone())
            .await
            .unwrap();
        server.inject(denied());

        let error = core.handle.run_write(review.into_intent()).await;

        assert_denied(error.unwrap_err());
        let sent = real(&server);
        assert_eq!(sent.len(), 1, "{proposal:?}: {sent:?}");
        assert_eq!(sent[0].status, Some(403), "{proposal:?}");
        assert_eq!(replicas(&server), 1, "{proposal:?}");
    }
}

// covers UP-52: an edit made elsewhere after the review is not overwritten, and a new review of
// the current object applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_competing_edit_is_refused_and_a_new_review_applies() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&deployments(), object(1, "5000"));
    let core = Core::start(&server).await;
    let stale = core
        .handle
        .review_write(target(&server), edit())
        .await
        .unwrap();
    server.set_object(&deployments(), object(4, "6000"));

    assert_eq!(
        core.handle.run_write(stale.into_intent()).await,
        Err(WriteError::Conflict)
    );
    let sent = real(&server);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].status, Some(409));
    assert_eq!(replicas(&server), 4, "the other edit stays");

    let fresh = core
        .handle
        .review_write(target(&server), edit())
        .await
        .unwrap();
    core.handle.run_write(fresh.into_intent()).await.unwrap();
    assert_eq!(real(&server).len(), 2);
    assert_eq!(replicas(&server), 9);
}

// covers UP-52: a 409 from the server is a conflict, and the write is not sent again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflict_from_the_server_is_not_retried() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&deployments(), object(1, "5000"));
    let core = Core::start(&server).await;
    let review = core
        .handle
        .review_write(target(&server), edit())
        .await
        .unwrap();
    server.inject(
        Fault::conflict()
            .on(&deployments())
            .verb(Verb::Other)
            .times(1),
    );

    assert_eq!(
        core.handle.run_write(review.into_intent()).await,
        Err(WriteError::Conflict)
    );
    let sent = real(&server);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].status, Some(409));
    assert_eq!(replicas(&server), 1);
}
