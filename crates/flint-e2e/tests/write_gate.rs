//! The write gate between the requests of one action, against the fake API server: each test
//! holds a review's `GET` on the server and changes the cluster's state before its dry run.

use std::time::Duration;

use flint_core::handle::{Command, Event};
use flint_core::write::{WriteError, WriteReview};
use flint_core::{Settings, WriteRefusal};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Fault, REMOTE_CONTEXT, RequestHold, Verb};
use tokio::task::JoinHandle;

mod core_support;

use core_support::{Core, EDITED, WAIT, deployments, editable, three_replicas, write_target};

impl Core {
    /// Points the context at `server` and waits until it shows there, connected and discovered.
    async fn point_at(&self, server: &FakeApiServer) {
        server
            .write_kubeconfig(&self.dir.path().join("kubeconfig"), Some("demo"))
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

    /// Starts a review of the edit on `server` and returns once its `GET` is held there, until
    /// the returned hold is released.
    async fn review_held_at_get(
        &self,
        server: &FakeApiServer,
    ) -> (JoinHandle<Result<WriteReview, WriteError>>, RequestHold) {
        let (fault, hold) = Fault::hold();
        server.inject(
            fault
                .on(&deployments())
                .verb(Verb::Get)
                .named(EDITED)
                .times(1),
        );
        let review = tokio::spawn(
            self.handle
                .review_write(write_target(server), three_replicas()),
        );
        tokio::time::timeout(WAIT, hold.reached())
            .await
            .expect("the review's GET arrives");
        (review, hold)
    }
}

fn object() -> serde_json::Value {
    editable("5000", serde_json::json!({}))
}

/// Whether the review's `GET` is still unanswered. Checked after a state change, it proves the
/// review's next request sees that state.
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
    let core = Core::start_with(&a, Some("demo"), Settings::default()).await;

    let (review, hold) = core.review_held_at_get(&a).await;
    core.point_at(&b).await;
    core.point_at(&a).await;
    assert!(get_is_held(&a), "the GET must outlast the re-point");
    assert!(!review.is_finished(), "the review waits for its GET");
    hold.release();

    assert_eq!(
        review.await.unwrap().unwrap_err(),
        WriteError::Refused(WriteRefusal::Moved {
            cluster: CONTEXT.to_owned(),
        })
    );
    assert_eq!(puts(&a), 0, "no dry run on the first server");
    assert_eq!(puts(&b), 0, "nothing on the second server");

    core.handle
        .review_write(write_target(&a), three_replicas())
        .await
        .expect("a new review starts from the context as it is now");
}

// covers UP-54, UP-55: on default settings, a loopback context without a local tool's name starts
// protected: nothing reaches the server until it is unlocked for that server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_looking_context_refuses_writes_until_unlocked() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&deployments(), object());
    let dir = tempfile::tempdir().unwrap();
    let kubeconfig = dir.path().join("kubeconfig");
    server
        .write_remote_kubeconfig(&kubeconfig, Some("demo"))
        .unwrap();
    let core =
        Core::start_on_context(dir, vec![kubeconfig], Settings::default(), REMOTE_CONTEXT).await;
    let target = || flint_core::write::WriteTarget {
        cluster: REMOTE_CONTEXT.into(),
        ..write_target(&server)
    };

    assert_eq!(
        core.handle
            .review_write(target(), three_replicas())
            .await
            .unwrap_err(),
        WriteError::Refused(WriteRefusal::Protected {
            cluster: REMOTE_CONTEXT.to_owned(),
        })
    );
    assert_eq!(puts(&server), 0, "a protected cluster gets no dry run");
    assert!(server.writes().is_empty(), "nothing was written");

    core.handle.unlock(REMOTE_CONTEXT, &server.url());
    let review = core
        .handle
        .review_write(target(), three_replicas())
        .await
        .expect("an unlocked cluster can be reviewed");
    core.handle
        .run_write(review.into_intent())
        .await
        .expect("an unlocked cluster can be written");
    let written = server.object(&deployments(), Some("demo"), EDITED).unwrap();
    assert_eq!(written["spec"]["replicas"], 3);
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
    let core = Core::start_with(&server, Some("demo"), settings).await;
    core.handle.unlock(CONTEXT, &server.url());

    let (review, hold) = core.review_held_at_get(&server).await;
    assert!(core.handle.lock(CONTEXT));
    assert!(get_is_held(&server), "the GET must outlast the lock");
    assert!(!review.is_finished(), "the review waits for its GET");
    hold.release();

    assert_eq!(
        review.await.unwrap().unwrap_err(),
        WriteError::Refused(WriteRefusal::Protected {
            cluster: CONTEXT.to_owned(),
        })
    );
    assert_eq!(puts(&server), 0, "the dry run never reached the server");
}
