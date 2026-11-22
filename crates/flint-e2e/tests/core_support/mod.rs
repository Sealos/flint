//! What the flint-core tests against the fake API server share: a started core and the object
//! their writes edit.

use std::path::PathBuf;
use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::write::{WriteProposal, WriteTarget};
use flint_core::{KindId, Settings, WatchMode, WatchTiming};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Gvr};

pub const WAIT: Duration = Duration::from_secs(30);
/// The name of the object the write tests edit.
pub const EDITED: &str = "edit-me";

/// A core on a scratch config dir, watching only what is open, connected to a server.
pub struct Core {
    pub handle: CoreHandle,
    /// Holds the config dir and, for [`Core::start_with`], the `kubeconfig` file.
    pub dir: tempfile::TempDir,
}

impl Core {
    /// A core with `settings` (watching only what is open) on a kubeconfig for `server`.
    pub async fn start_with(
        server: &FakeApiServer,
        namespace: Option<&str>,
        settings: Settings,
    ) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server.write_kubeconfig(&kubeconfig, namespace).unwrap();
        Core::start_on(dir, vec![kubeconfig], settings).await
    }

    /// A core reading the kubeconfig files `paths`, in order, with its config in `dir`.
    pub async fn start_on(dir: tempfile::TempDir, paths: Vec<PathBuf>, settings: Settings) -> Core {
        Core::start_on_context(dir, paths, settings, CONTEXT).await
    }

    /// [`Core::start_on`], activating `context`.
    pub async fn start_on_context(
        dir: tempfile::TempDir,
        paths: Vec<PathBuf>,
        settings: Settings,
        context: &str,
    ) -> Core {
        let settings = Settings {
            watch_mode: WatchMode::OnView,
            ..settings
        };
        let handle = CoreHandle::start(CoreConfig {
            config_dir: dir.path().join("config"),
            settings,
            settings_error: None,
            kubeconfig_base: paths,
            home: None,
            watch_files: false,
            // The fake server answers at once; real waits would only slow the tests down.
            watch_timing: WatchTiming {
                backoff_min: Duration::from_millis(10),
                backoff_max: Duration::from_millis(50),
                denied_backoff_min: Duration::from_millis(10),
            },
        })
        .unwrap();
        let core = Core { handle, dir };
        core.activate(context).await;
        core
    }

    /// Activates `context` and waits for its discovery.
    async fn activate(&self, context: &str) {
        let events = self.handle.events();
        self.handle.send(Command::Activate(context.to_owned()));
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
    }
}

pub fn deployments() -> Gvr {
    Gvr::new("apps", "v1", "deployments")
}

/// A Deployment to edit, at version `version`, with `labels`.
pub fn editable(version: &str, labels: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": EDITED,
            "namespace": "demo",
            "uid": "uid-edit",
            "resourceVersion": version,
            "labels": labels,
        },
        "spec": {"replicas": 1},
    })
}

pub fn write_target(server: &FakeApiServer) -> WriteTarget {
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

pub fn three_replicas() -> WriteProposal {
    WriteProposal::ApplyYaml {
        yaml: format!(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {EDITED}\nspec:\n  replicas: 3\n"
        ),
    }
}
