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
        watch_timing: Default::default(),
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

// covers UP-54: against a real API server, a read-only client still reads, and nothing it refuses
// changes the cluster.
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn the_read_only_client_holds_against_a_real_server() {
    use http::{Method, Request};
    use k8s_openapi::api::core::v1::{ConfigMap, Pod};
    use k8s_openapi::api::rbac::v1::ClusterRole;
    use kube::Api;
    use kube::api::{DeleteParams, ListParams, ObjectMeta, PostParams};
    use kube::client::Body;

    let loaded = kubeconfig::load(&[test_kubeconfig()]);
    let context = &loaded.contexts.first().expect("a context").name;
    let reader = cluster::connect(&loaded.merged, context).await.unwrap();
    let admin = cluster::connect_unrestricted_for_tests(&loaded.merged, context)
        .await
        .unwrap();

    let suffix = std::process::id();
    let marker = format!("flint-read-only-{suffix}");
    let absent = format!("{marker}-absent");
    let role = format!("flint-read-only;{suffix}");
    let maps: Api<ConfigMap> = Api::namespaced(admin.clone(), "default");
    let roles: Api<ClusterRole> = Api::all(admin.clone());
    let meta = |name: &str| ObjectMeta {
        name: Some(name.to_owned()),
        ..Default::default()
    };
    let before = maps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: meta(&marker),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    roles
        .create(
            &PostParams::default(),
            &ClusterRole {
                metadata: meta(&role),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Collected, so the objects above are removed before the test fails.
    let mut failures = Vec::new();
    let pods: Api<Pod> = Api::namespaced(reader.clone(), "kube-system");
    let pod = match pods.list(&ListParams::default().limit(1)).await {
        Ok(list) => list.items[0].metadata.name.clone().unwrap(),
        Err(e) => panic!("a read-only client lists pods: {e}"),
    };
    let readable: Api<ConfigMap> = Api::namespaced(reader.clone(), "default");
    if let Err(e) = readable.get(&marker).await {
        failures.push(format!("reading the marker failed: {e}"));
    }

    let map_path = format!("/api/v1/namespaces/default/configmaps/{marker}");
    let pod_path = format!("/api/v1/namespaces/kube-system/pods/{pod}");
    let body =
        format!(r#"{{"apiVersion":"v1","kind":"ConfigMap","metadata":{{"name":"{absent}"}}}}"#);
    let refused = [
        (
            Method::POST,
            "/api/v1/namespaces/default/configmaps".to_owned(),
            body,
        ),
        (Method::PUT, map_path.clone(), String::new()),
        (Method::PATCH, map_path.clone(), "{}".to_owned()),
        (Method::DELETE, map_path, String::new()),
        (
            Method::DELETE,
            "/api/v1/namespaces/default/configmaps".to_owned(),
            String::new(),
        ),
        (
            Method::GET,
            format!("{pod_path}/exec?command=true"),
            String::new(),
        ),
        (Method::GET, format!("{pod_path}/attach"), String::new()),
        (
            Method::GET,
            format!("{pod_path}/portforward?ports=80"),
            String::new(),
        ),
        (Method::GET, format!("{pod_path}/proxy/"), String::new()),
    ];
    for (method, path, body) in refused {
        let request = Request::builder()
            .method(method.clone())
            .uri(&path)
            .body(Body::from(body.into_bytes()))
            .unwrap();
        match reader.send(request).await {
            Err(e) if e.to_string().contains("read-only") => {}
            Err(e) => failures.push(format!("{method} {path}: other error: {e}")),
            Ok(response) => failures.push(format!("{method} {path}: {}", response.status())),
        }
    }

    let named: Api<ClusterRole> = Api::all(reader.clone());
    match named.get(&role).await {
        Err(e) if e.to_string().contains("name contains `;`") => {}
        other => failures.push(format!("a role named with `;`: {other:?}")),
    }

    match maps.get(&marker).await {
        Ok(after) if after.metadata.resource_version == before.metadata.resource_version => {}
        other => failures.push(format!("the marker changed: {other:?}")),
    }
    match maps.get_opt(&absent).await {
        Ok(None) => {}
        other => failures.push(format!("a refused create reached the server: {other:?}")),
    }

    let _ = maps.delete(&marker, &DeleteParams::default()).await;
    let _ = maps.delete(&absent, &DeleteParams::default()).await;
    let _ = roles.delete(&role, &DeleteParams::default()).await;
    assert!(failures.is_empty(), "{failures:#?}");
}
