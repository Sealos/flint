//! Protected clusters (UP-54, UP-55, UP-92) on default settings, against the fake API server
//! under a context name no local tool uses, so Flint treats it as a remote cluster. Every refused
//! write is checked on the server's recorder: nothing was sent for the object it names.

use std::path::PathBuf;
use std::time::Duration;

use flint_core::handle::{Command, Event};
use flint_core::settings::load_settings;
use flint_core::write::{WriteError, WriteProposal, WriteTarget};
use flint_core::{KindId, Settings, WriteRefusal};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Gvr, REMOTE_CONTEXT, write_kubeconfigs};
use serde_json::json;

mod core_support;

use core_support::{Core, EDITED, WAIT, deployments, editable, three_replicas, write_target};

const NODE: &str = "worker-1";

fn seed(server: &FakeApiServer) {
    server.set_object(&deployments(), editable("5000", json!({})));
    server.set_object(
        &Gvr::core("nodes"),
        json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {"name": NODE, "uid": "uid-node", "resourceVersion": "5001"},
            "spec": {},
        }),
    );
}

fn target(server: &FakeApiServer) -> WriteTarget {
    WriteTarget {
        cluster: REMOTE_CONTEXT.into(),
        ..write_target(server)
    }
}

fn node_target(server: &FakeApiServer) -> WriteTarget {
    WriteTarget {
        kind: KindId::new("", "nodes"),
        namespace: None,
        name: NODE.to_owned(),
        ..target(server)
    }
}

fn protected() -> WriteError {
    WriteError::Refused(WriteRefusal::Protected {
        cluster: REMOTE_CONTEXT.to_owned(),
    })
}

/// The requests that reached `server` for a seeded object, and any that was not a read.
fn sent_for_objects(server: &FakeApiServer) -> Vec<String> {
    server
        .arrivals()
        .iter()
        .filter(|r| {
            r.method != http::Method::GET
                || r.path.ends_with(&format!("/{EDITED}"))
                || r.path.ends_with(&format!("/nodes/{NODE}"))
        })
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

fn replicas(server: &FakeApiServer) -> i64 {
    server
        .object(&deployments(), Some("demo"), EDITED)
        .expect("the object")["spec"]["replicas"]
        .as_i64()
        .expect("a replica count")
}

impl Core {
    /// A core on default settings, on a kubeconfig with only the remote-looking context.
    async fn start_remote(server: &FakeApiServer) -> (Core, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server
            .write_remote_kubeconfig(&kubeconfig, Some("demo"))
            .unwrap();
        let paths = vec![kubeconfig.clone()];
        let core = Core::start_on_context(dir, paths, Settings::default(), REMOTE_CONTEXT).await;
        (core, kubeconfig)
    }

    /// Quits and starts again on the same config directory and kubeconfig.
    async fn relaunch(self, kubeconfig: PathBuf) -> Core {
        let Core { handle, dir } = self;
        drop(handle);
        let settings = load_settings(&dir.path().join("config")).expect("readable settings");
        Core::start_on_context(dir, vec![kubeconfig], settings, REMOTE_CONTEXT).await
    }

    async fn scale(&self, target: WriteTarget, to: u32) -> Result<(), WriteError> {
        let review = self
            .handle
            .review_write(target, WriteProposal::Scale { to })
            .await?;
        self.handle
            .run_write(review.into_intent())
            .await
            .map(|_| ())
    }

    /// Trusts the remote context at `server` and waits until the trust is saved.
    async fn trust_remote(&self, server: &FakeApiServer) {
        let events = self.handle.events();
        self.handle.trust(REMOTE_CONTEXT, &server.url());
        tokio::time::timeout(WAIT, async {
            loop {
                match events.recv().await.expect("core running") {
                    Event::SettingsChanged { settings, .. }
                        if !settings.trusted_clusters.is_empty() =>
                    {
                        return;
                    }
                    Event::Error(e) => panic!("{e}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("trusted in time");
    }

    /// Re-points the remote context at `server` and waits until it shows there, discovered.
    async fn repoint_remote(&self, kubeconfig: &std::path::Path, server: &FakeApiServer) {
        server
            .write_remote_kubeconfig(kubeconfig, Some("demo"))
            .unwrap();
        let events = self.handle.events();
        self.handle.send(Command::ReloadKubeconfig);
        let url = server.url();
        tokio::time::timeout(WAIT, async {
            loop {
                if let Event::ClustersChanged(snapshot) = events.recv().await.expect("core running")
                    && snapshot.clusters.iter().any(|c| {
                        c.info.name == REMOTE_CONTEXT
                            && c.info.server.as_deref() == Some(url.as_str())
                    })
                {
                    break;
                }
            }
            while self.handle.discovery(REMOTE_CONTEXT).await.is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the context shows the server");
    }
}

// covers UP-54, UP-92: on default settings every kind of write to a remote-looking cluster is
// refused at review, before any request for the object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_write_to_a_remote_looking_cluster_sends_nothing() {
    let server = FakeApiServer::start().await.unwrap();
    seed(&server);
    let (core, _) = Core::start_remote(&server).await;
    let before = server.object(&deployments(), Some("demo"), EDITED);

    for (target, proposal) in [
        (target(&server), three_replicas()),
        (target(&server), WriteProposal::Delete),
        (target(&server), WriteProposal::Scale { to: 3 }),
        (target(&server), WriteProposal::RolloutRestart),
        (node_target(&server), WriteProposal::Cordon),
        (node_target(&server), WriteProposal::Uncordon),
        (
            node_target(&server),
            WriteProposal::Drain {
                delete_emptydir_data: false,
                force: false,
            },
        ),
    ] {
        let refused = core.handle.review_write(target, proposal).await;
        assert_eq!(refused.unwrap_err(), protected());
    }

    assert_eq!(sent_for_objects(&server), Vec::<String>::new());
    assert!(server.writes().is_empty(), "nothing was written");
    assert_eq!(
        server.object(&deployments(), Some("demo"), EDITED),
        before,
        "the object is as it was"
    );
}

// covers UP-55: the local fake starts unlocked, with nothing to confirm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_known_local_context_starts_unlocked() {
    let server = FakeApiServer::start().await.unwrap();
    seed(&server);
    let core = Core::start_with(&server, Some("demo"), Settings::default()).await;
    core.scale(write_target(&server), 2)
        .await
        .expect("a local cluster can be written");
    assert_eq!(replicas(&server), 2);
}

// covers UP-54, UP-55: with a local and a remote-looking context side by side, an unlock covers
// only the context and server it names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unlock_covers_only_its_context_and_server() {
    let (local, remote) = (
        FakeApiServer::start().await.unwrap(),
        FakeApiServer::start().await.unwrap(),
    );
    seed(&local);
    seed(&remote);
    let dir = tempfile::tempdir().unwrap();
    let kubeconfig = dir.path().join("kubeconfig");
    write_kubeconfigs(
        &kubeconfig,
        &[
            (&local, CONTEXT, Some("demo")),
            (&remote, REMOTE_CONTEXT, Some("demo")),
        ],
    )
    .unwrap();
    let core =
        Core::start_on_context(dir, vec![kubeconfig], Settings::default(), REMOTE_CONTEXT).await;
    core.handle.send(Command::Activate(CONTEXT.to_owned()));
    tokio::time::timeout(WAIT, async {
        while core.handle.discovery(CONTEXT).await.is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the local context connects");

    core.scale(write_target(&local), 2)
        .await
        .expect("the local context is writable");
    assert_eq!(
        core.scale(target(&remote), 2).await.unwrap_err(),
        protected()
    );

    // Neither an unlock at another server nor one of another context covers it.
    core.handle.unlock(REMOTE_CONTEXT, &local.url());
    core.handle.unlock(CONTEXT, &remote.url());
    assert_eq!(
        core.scale(target(&remote), 2).await.unwrap_err(),
        protected()
    );
    assert_eq!(sent_for_objects(&remote), Vec::<String>::new());

    core.handle.unlock(REMOTE_CONTEXT, &remote.url());
    core.scale(target(&remote), 2)
        .await
        .expect("unlocked at its own server");
    assert_eq!(replicas(&remote), 2);
}

// covers UP-54, UP-55: Lock Again refuses new writes and a write already reviewed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_again_refuses_a_reviewed_write() {
    let server = FakeApiServer::start().await.unwrap();
    seed(&server);
    let (core, _) = Core::start_remote(&server).await;
    core.handle.unlock(REMOTE_CONTEXT, &server.url());
    let review = core
        .handle
        .review_write(target(&server), WriteProposal::Scale { to: 4 })
        .await
        .expect("an unlocked cluster can be reviewed");
    let sent = server.writes().len();

    assert!(core.handle.lock(REMOTE_CONTEXT));

    assert_eq!(
        core.handle
            .run_write(review.into_intent())
            .await
            .unwrap_err(),
        protected()
    );
    assert_eq!(
        core.scale(target(&server), 5).await.unwrap_err(),
        protected()
    );
    assert_eq!(server.writes().len(), sent, "nothing after the lock");
    assert_eq!(replicas(&server), 1);
}

// covers UP-55: an unlock lasts until quit, and a trust survives a relaunch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unlock_ends_at_quit_and_a_trust_survives_it() {
    let server = FakeApiServer::start().await.unwrap();
    seed(&server);
    let (core, kubeconfig) = Core::start_remote(&server).await;
    core.handle.unlock(REMOTE_CONTEXT, &server.url());
    core.scale(target(&server), 2).await.expect("unlocked");

    let core = core.relaunch(kubeconfig.clone()).await;
    let sent = server.writes().len();
    assert_eq!(
        core.scale(target(&server), 3).await.unwrap_err(),
        protected()
    );
    assert_eq!(server.writes().len(), sent, "the unlock ended at quit");

    core.trust_remote(&server).await;
    let core = core.relaunch(kubeconfig).await;
    core.scale(target(&server), 4)
        .await
        .expect("the trust is still there after a relaunch");
    assert_eq!(replicas(&server), 4);
}

// covers UP-55: a context re-pointed at another server is protected again, after an unlock and
// after a trust.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repointing_a_remote_context_protects_it_again() {
    for trusted in [false, true] {
        let (first, second) = (
            FakeApiServer::start().await.unwrap(),
            FakeApiServer::start().await.unwrap(),
        );
        seed(&first);
        seed(&second);
        let (core, kubeconfig) = Core::start_remote(&first).await;
        if trusted {
            core.trust_remote(&first).await;
        } else {
            core.handle.unlock(REMOTE_CONTEXT, &first.url());
        }
        core.scale(target(&first), 2).await.expect("allowed");

        core.repoint_remote(&kubeconfig, &second).await;

        assert_eq!(
            core.scale(target(&second), 3).await.unwrap_err(),
            protected(),
            "trusted: {trusted}"
        );
        assert_eq!(sent_for_objects(&second), Vec::<String>::new());
        assert_eq!(replicas(&second), 1);
    }
}

// covers UP-55: Stop Trusting protects at once, before the settings file is written back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_trusting_refuses_the_next_write_at_once() {
    let server = FakeApiServer::start().await.unwrap();
    seed(&server);
    let (core, _) = Core::start_remote(&server).await;
    core.trust_remote(&server).await;
    core.scale(target(&server), 2).await.expect("trusted");
    let sent = server.writes().len();

    core.handle.untrust(REMOTE_CONTEXT);

    assert_eq!(
        core.scale(target(&server), 3).await.unwrap_err(),
        protected()
    );
    assert_eq!(server.writes().len(), sent, "nothing after Stop Trusting");
    assert_eq!(replicas(&server), 2);
}
