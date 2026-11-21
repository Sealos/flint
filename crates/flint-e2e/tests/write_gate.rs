//! The write gate between the requests of one action, against the fake API server: each test
//! holds a review's `GET` on the server and changes the cluster's state before its dry run.

use std::path::PathBuf;
use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::write::{WriteError, WriteProposal, WriteReview, WriteTarget};
use flint_core::{KindId, Settings, WatchMode, WriteRefusal};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Fault, Gvr, Verb};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(30);
/// How long the review's `GET` is held: longer than the changes made while it is.
const HELD: Duration = Duration::from_secs(5);
const EDITED: &str = "edit-me";

struct Core {
    handle: CoreHandle,
    kubeconfig: PathBuf,
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
            kubeconfig_base: vec![kubeconfig.clone()],
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
        Core {
            handle,
            kubeconfig,
            _dir: dir,
        }
    }

    /// Points the context at `server` and waits until it shows there, connected and discovered.
    async fn point_at(&self, server: &FakeApiServer) {
        server
            .write_kubeconfig(&self.kubeconfig, Some("demo"))
            .unwrap();
        let events = self.handle.events();
        self.handle.send(Command::ReloadKubeconfig);
        let url = server.url();
        tokio::time::timeout(WAIT, async {
            loop {
                if let Event::ClustersChanged(snapshot) = events.recv().await.expect("core running")
                    && snapshot.clusters.iter().any(|c| {
                        c.info.name == CONTEXT && c.info.server.as_deref() == Some(url.as_str())
                    })
                {
                    break;
                }
            }
            while self.handle.discovery(CONTEXT).await.is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the context shows the server");
    }

    /// Starts a review of the edit on `server` and returns once its `GET` is held there.
    async fn review_held_at_get(
        &self,
        server: &FakeApiServer,
    ) -> JoinHandle<Result<WriteReview, WriteError>> {
        server.inject(
            Fault::delay(HELD)
                .on(&deployments())
                .verb(Verb::Get)
                .named(EDITED)
                .times(1),
        );
        let review = tokio::spawn(self.handle.review_write(target(server), proposal()));
        tokio::time::timeout(WAIT, async {
            while !server
                .arrivals()
                .iter()
                .any(|r| r.method == http::Method::GET && r.path.ends_with(EDITED))
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the review's GET arrives");
        review
    }
}

fn deployments() -> Gvr {
    Gvr::new("apps", "v1", "deployments")
}

fn object() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": EDITED,
            "namespace": "demo",
            "uid": "uid-edit",
            "resourceVersion": "5000",
        },
        "spec": {"replicas": 1},
    })
}

fn target(server: &FakeApiServer) -> WriteTarget {
    WriteTarget {
        cluster: CONTEXT.into(),
        server: Some(server.url()),
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: EDITED.to_owned(),
        uid: None,
        resource_version: None,
    }
}

fn proposal() -> WriteProposal {
    WriteProposal::ApplyYaml {
        yaml: format!(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {EDITED}\nspec:\n  replicas: 3\n"
        ),
    }
}

/// Whether the review's `GET` is still unanswered. Checked after a state change, it proves the
/// review's next request sees that state: a slow run fails here instead of passing early.
fn get_is_held(server: &FakeApiServer) -> bool {
    !server
        .requests()
        .iter()
        .any(|r| r.method == http::Method::GET && r.path.ends_with(EDITED))
}

fn puts(server: &FakeApiServer) -> usize {
    server
        .arrivals()
        .iter()
        .filter(|r| r.method == http::Method::PUT)
        .count()
}

// covers UP-06, UP-54: a context re-pointed and back between two requests of one action names
// the same URL but is another definition, so the action stops at its next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_context_repointed_and_back_between_requests_is_refused() {
    let (a, b) = (
        FakeApiServer::start().await.unwrap(),
        FakeApiServer::start().await.unwrap(),
    );
    a.set_object(&deployments(), object());
    let core = Core::start(&a, Settings::default()).await;

    let review = core.review_held_at_get(&a).await;
    core.point_at(&b).await;
    core.point_at(&a).await;
    assert!(get_is_held(&a), "the GET must outlast the re-point");

    assert_eq!(
        review.await.unwrap().unwrap_err(),
        WriteError::Refused(WriteRefusal::Moved {
            cluster: CONTEXT.to_owned(),
        })
    );
    assert_eq!(puts(&a), 0, "no dry run on the first server");
    assert_eq!(puts(&b), 0, "nothing on the second server");

    core.handle
        .review_write(target(&a), proposal())
        .await
        .expect("a new review starts from the context as it is now");
}

// covers UP-54: a cluster locked after the review's GET gets no dry run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cluster_locked_after_the_get_gets_no_dry_run() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&deployments(), object());
    let settings = Settings {
        protect_local_clusters: true,
        ..Settings::default()
    };
    let core = Core::start(&server, settings).await;
    core.handle.unlock(CONTEXT, &server.url());

    let review = core.review_held_at_get(&server).await;
    assert!(core.handle.lock(CONTEXT));
    assert!(get_is_held(&server), "the GET must outlast the lock");

    assert_eq!(
        review.await.unwrap().unwrap_err(),
        WriteError::Refused(WriteRefusal::Protected {
            cluster: CONTEXT.to_owned(),
        })
    );
    assert_eq!(puts(&server), 0, "the dry run never reached the server");
}
