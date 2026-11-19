//! Shared setup for integration tests against the dev cluster.

use std::path::{Path, PathBuf};
use std::time::Duration;

use flint_core::handle::{ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::{Settings, cluster, kubeconfig};
use kube::Client;
use kube::config::{Kubeconfig, NamedContext};

pub(super) fn test_kubeconfig() -> PathBuf {
    match std::env::var_os("FLINT_TEST_KUBECONFIG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => panic!("FLINT_TEST_KUBECONFIG must point at the dev cluster's kubeconfig"),
    }
}

/// Copies the dev kubeconfig and adds a second context for the same cluster. With an explicit
/// alias, the current context is preserved. Otherwise the first context is selected and its
/// name gets a `-second` suffix for the copy. Returns the path and both context names.
pub(super) fn two_context_kubeconfig(dir: &Path, alias: Option<&str>) -> (PathBuf, String, String) {
    let mut config = Kubeconfig::read_from(test_kubeconfig()).unwrap();
    let first = config.contexts[0].clone();
    let second = match alias {
        Some(alias) => alias.to_owned(),
        None => {
            config.current_context = Some(first.name.clone());
            format!("{}-second", first.name)
        }
    };
    config.contexts.push(NamedContext {
        name: second.clone(),
        ..first.clone()
    });
    let path = dir.join("kubeconfig");
    // JSON is YAML, and kubeconfig is read as YAML.
    std::fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();
    (path, first.name, second)
}

pub(super) async fn client(path: &Path) -> Client {
    let loaded = kubeconfig::load(&[path.to_owned()]);
    let context = &loaded.contexts[0].name;
    cluster::connect(&loaded.merged, context).await.unwrap()
}

/// Sends core logs to stderr, filtered by `RUST_LOG`.
pub(super) fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
}

pub(super) struct Core {
    pub(super) handle: CoreHandle,
    events: async_channel::Receiver<Event>,
}

impl Core {
    pub(super) fn start(kubeconfig: PathBuf, config_dir: PathBuf, settings: Settings) -> Self {
        let handle = CoreHandle::start(CoreConfig {
            config_dir,
            settings,
            settings_error: None,
            kubeconfig_base: vec![kubeconfig],
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        })
        .unwrap();
        let events = handle.events();
        Self { handle, events }
    }

    /// The first event for which `pick` returns something, failing on any connection error.
    #[allow(
        dead_code,
        reason = "exec waits for connection events for one context at a time"
    )]
    pub(super) async fn wait_for<T>(
        &self,
        secs: u64,
        mut pick: impl FnMut(Event) -> Option<T>,
    ) -> T {
        self.wait_for_event(secs, |event| {
            if let Event::ConnectionChanged {
                state: ConnectionState::Failed(e),
                ..
            } = &event
            {
                panic!("connection failed: {e}");
            }
            pick(event)
        })
        .await
        .expect("event arrived in time")
    }

    pub(super) async fn wait_for_event<T>(
        &self,
        secs: u64,
        mut pick: impl FnMut(Event) -> Option<T>,
    ) -> Result<T, tokio::time::error::Elapsed> {
        tokio::time::timeout(Duration::from_secs(secs), async {
            loop {
                let event = self.events.recv().await.expect("core running");
                if let Some(value) = pick(event) {
                    return value;
                }
            }
        })
        .await
    }
}
