//! Tests against a real cluster. They are ignored by default; run them with `--include-ignored`
//! and `FLINT_TEST_KUBECONFIG` pointing at the throwaway dev cluster (see `scripts/dev-cluster.sh`).
//! They use that file alone and never read `KUBECONFIG` or `~/.kube/config`.

use std::path::PathBuf;
use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::{cluster, kubeconfig, read};

/// The dev cluster's kubeconfig. Panics when unset: these tests only run when asked for.
fn test_kubeconfig() -> PathBuf {
    match std::env::var_os("FLINT_TEST_KUBECONFIG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => panic!(
            "FLINT_TEST_KUBECONFIG must point at the dev cluster's kubeconfig (scripts/dev-cluster.sh)"
        ),
    }
}

// covers UP-03, UP-91
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn core_handle_connects_and_reads_version() {
    let path = test_kubeconfig();
    let config_dir = tempfile::tempdir().unwrap();
    let core = CoreHandle::start(CoreConfig {
        config_dir: config_dir.path().to_owned(),
        settings: Default::default(),
        settings_error: None,
        kubeconfig_base: vec![path],
        home: None,
        watch_files: false,
    })
    .unwrap();
    let events = core.events();

    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let mut cluster = None;
        while let Ok(event) = events.recv().await {
            match event {
                Event::ClustersChanged(snapshot) => {
                    // Startup must not connect anywhere by itself.
                    assert!(
                        snapshot
                            .clusters
                            .iter()
                            .all(|c| c.state == ConnectionState::Disconnected)
                    );
                    let first = snapshot
                        .clusters
                        .first()
                        .expect("a context")
                        .info
                        .name
                        .clone();
                    core.send(Command::Activate(first.clone()));
                    cluster = Some(first);
                }
                Event::ConnectionChanged { cluster: c, state } => {
                    assert_eq!(Some(&c), cluster.as_ref());
                    match state {
                        ConnectionState::Connected(version) => return version,
                        ConnectionState::Failed(msg) => panic!("connection failed: {msg}"),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        panic!("event stream ended");
    })
    .await
    .expect("connected within 60 s");
    assert!(
        result.git_version.starts_with('v'),
        "unexpected version {result:?}"
    );
}

// covers UP-10
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn reads_kinds_and_objects() {
    let path = test_kubeconfig();
    let loaded = kubeconfig::load(&[path]);
    let context = &loaded.contexts.first().expect("a context").name;
    let (client, _) = cluster::connect_and_check(&loaded.merged, context, Duration::from_secs(30))
        .await
        .unwrap();

    let kinds = read::discover_kinds(&client).await.unwrap().kinds;
    let pods = read::resolve_kind(&kinds, "pods", None).unwrap();
    assert_eq!(pods.api_version(), "v1");
    assert!(pods.namespaced && pods.is_listable());
    // Short and singular names come from discovery, as in kubectl.
    for name in ["deploy", "deployment", "Deployment.apps"] {
        let deployments = read::resolve_kind(&kinds, name, None).unwrap();
        assert_eq!(deployments.id().as_str(), "deployments.apps", "{name}");
    }
    assert_eq!(
        read::resolve_kind(&kinds, "po", None)
            .unwrap()
            .id()
            .as_str(),
        "pods"
    );
    assert_eq!(
        read::resolve_kind(&kinds, "svc", None)
            .unwrap()
            .id()
            .as_str(),
        "services"
    );

    let page = read::list(
        &client,
        pods,
        &read::ListQuery {
            namespace: Some("kube-system".into()),
            limit: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.more, "kube-system has more than one pod");

    let namespaces = read::resolve_kind(&kinds, "Namespace", None).unwrap();
    let ns = read::get(&client, namespaces, None, "kube-system")
        .await
        .unwrap();
    assert_eq!(ns.metadata.name.as_deref(), Some("kube-system"));

    let (events, _) = read::events(
        &client,
        &read::EventQuery {
            namespace: Some("kube-system".into()),
            fetch_limit: Some(50),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let times: Vec<_> = events.iter().map(read::event_time).collect();
    assert!(
        times.windows(2).all(|w| w[0] >= w[1]),
        "events are newest first"
    );
}
