//! What a reviewed YAML edit sends, and how a vanished object is reported, against the fake API
//! server. The fake stores a `PUT` body as sent, apart from the metadata only a server sets, so
//! the stored object shows what the write sent.

use std::time::Duration;

use flint_core::KindId;
use flint_core::Settings;
use flint_core::WatchMode;
use flint_core::WatchTiming;
use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::write::{WriteError, WriteProposal, WriteTarget};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Gvr};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);
const NAME: &str = "edit-me";
const NAMESPACE: &str = "demo";

struct Core {
    handle: CoreHandle,
    _dir: tempfile::TempDir,
}

impl Core {
    async fn start(server: &FakeApiServer) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server
            .write_kubeconfig(&kubeconfig, Some(NAMESPACE))
            .unwrap();
        let handle = CoreHandle::start(CoreConfig {
            config_dir: dir.path().join("config"),
            settings: Settings {
                watch_mode: WatchMode::OnView,
                ..Settings::default()
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

    /// Reviews `yaml` and runs it.
    async fn apply(&self, server: &FakeApiServer, yaml: String) {
        let review = self
            .handle
            .review_write(target(server), WriteProposal::ApplyYaml { yaml })
            .await
            .unwrap();
        self.handle.run_write(review.into_intent()).await.unwrap();
    }
}

fn deployments() -> Gvr {
    Gvr::new("apps", "v1", "deployments")
}

fn target(server: &FakeApiServer) -> WriteTarget {
    WriteTarget {
        cluster: CONTEXT.into(),
        server: Some(server.url()),
        kind: KindId::new("apps", "deployments"),
        namespace: Some(NAMESPACE.to_owned()),
        name: NAME.to_owned(),
        uid: None,
        resource_version: None,
    }
}

fn live(extra_metadata: Value, status: Option<Value>) -> Value {
    let mut object = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": NAME,
            "namespace": NAMESPACE,
            "uid": "uid-edit",
            "resourceVersion": "5000",
        },
        "spec": {"replicas": 1},
    });
    if let (Some(metadata), Some(extra)) = (
        object["metadata"].as_object_mut(),
        extra_metadata.as_object(),
    ) {
        metadata.extend(extra.clone());
    }
    if let Some(status) = status {
        object["status"] = status;
    }
    object
}

fn edit(extra_metadata: &str) -> String {
    format!(
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {NAME}\n{extra_metadata}spec:\n  replicas: 3\n"
    )
}

fn stored(server: &FakeApiServer) -> Value {
    server
        .object(&deployments(), Some(NAMESPACE), NAME)
        .expect("the object")
}

fn puts(server: &FakeApiServer) -> (usize, usize) {
    let all: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.method == http::Method::PUT)
        .collect();
    let dry = all.iter().filter(|r| r.parsed_query().dry_run).count();
    (dry, all.len() - dry)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_keeps_the_live_status_the_edit_omits() {
    let server = FakeApiServer::start().await.unwrap();
    let status = json!({"phase": "Ready", "readyReplicas": 1});
    server.set_object(&deployments(), live(json!({}), Some(status.clone())));
    let core = Core::start(&server).await;

    core.apply(&server, edit("")).await;

    let written = stored(&server);
    assert_eq!(written["spec"]["replicas"], 3, "the edit was applied");
    assert_eq!(written["status"], status);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_drops_managed_fields_from_the_edit() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&deployments(), live(json!({}), None));
    let core = Core::start(&server).await;

    core.apply(
        &server,
        edit("  managedFields:\n  - manager: from-the-editor\n    operation: Update\n"),
    )
    .await;

    let written = stored(&server);
    assert_eq!(written["spec"]["replicas"], 3, "the edit was applied");
    assert!(
        written["metadata"].get("managedFields").is_none(),
        "{written}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_object_deleted_after_review_is_reported_as_gone() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&deployments(), live(json!({}), None));
    let core = Core::start(&server).await;
    let review = core
        .handle
        .review_write(target(&server), WriteProposal::ApplyYaml { yaml: edit("") })
        .await
        .unwrap();
    assert!(server.remove_object(&deployments(), Some(NAMESPACE), NAME));

    let error = core
        .handle
        .run_write(review.into_intent())
        .await
        .unwrap_err();

    assert_eq!(error, WriteError::Gone);
    assert_eq!(puts(&server), (1, 1), "sent once, not retried");
    assert!(
        server
            .object(&deployments(), Some(NAMESPACE), NAME)
            .is_none(),
        "the write did not create it again"
    );
}
