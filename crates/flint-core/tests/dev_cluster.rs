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

/// Before a test writes: `context` is a kind cluster on this machine, as `kind` knows it.
fn assert_local_kind(ours: &kube::config::Kubeconfig, context: &str) {
    use kube::config::Kubeconfig;

    let name = context
        .strip_prefix("kind-")
        .unwrap_or_else(|| panic!("refusing to write to {context}: not a kind context"));
    let output = std::process::Command::new("kind")
        .args(["get", "kubeconfig", "--name", name])
        .output()
        .expect("running `kind` to check the cluster before writing to it");
    assert!(output.status.success(), "kind knows no cluster {name:?}");
    let kinds = Kubeconfig::from_yaml(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let cluster = |config: &Kubeconfig| {
        let wanted = config
            .contexts
            .iter()
            .find(|c| c.name == context)
            .and_then(|c| c.context.as_ref())
            .map(|c| c.cluster.clone())?;
        config
            .clusters
            .iter()
            .find(|c| c.name == wanted)
            .and_then(|c| c.cluster.clone())
    };
    let (ours, theirs) = (
        cluster(ours).expect("our cluster"),
        cluster(&kinds).expect("kind's"),
    );
    assert!(
        ours.proxy_url.is_none(),
        "refusing to write through a proxy"
    );
    assert!(
        ours.server.is_some() && ours.server == theirs.server,
        "not kind's server"
    );
}

// covers UP-54: against a real API server, a read-only client still reads, and nothing it refuses
// changes the cluster.
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn the_read_only_client_holds_against_a_real_server() {
    use k8s_openapi::api::core::v1::Namespace;
    use k8s_openapi::api::rbac::v1::ClusterRole;
    use kube::Api;
    use kube::api::{DeleteParams, ObjectMeta, PostParams};

    let loaded = kubeconfig::load(&[test_kubeconfig()]);
    let context = loaded.contexts.first().expect("a context").name.clone();
    assert_local_kind(&loaded.merged, &context);
    let reader = cluster::connect(&loaded.merged, &context).await.unwrap();
    let admin = cluster::connect_unrestricted_for_tests(&loaded.merged, &context)
        .await
        .unwrap();

    let suffix = std::process::id();
    let namespace = format!("flint-read-only-{suffix}");
    let role = format!("flint-read-only;{suffix}");
    let namespaces: Api<Namespace> = Api::all(admin.clone());
    let roles: Api<ClusterRole> = Api::all(admin.clone());
    namespaces
        .create(
            &PostParams::default(),
            &Namespace {
                metadata: ObjectMeta {
                    name: Some(namespace.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Nothing here panics, so the fixtures are always removed.
    let mut failures = match probe_read_only(&reader, &admin, &namespace, &role).await {
        Ok(failures) => failures,
        Err(e) => vec![format!("setting up: {e}")],
    };
    let gone = |e: &kube::Error| matches!(e, kube::Error::Api(s) if s.code == 404);
    if let Err(e) = roles.delete(&role, &DeleteParams::default()).await
        && !gone(&e)
    {
        failures.push(format!("removing the role: {e}"));
    }
    if let Err(e) = namespaces
        .delete(&namespace, &DeleteParams::default())
        .await
    {
        failures.push(format!("removing the namespace: {e}"));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Makes the fixtures in the scratch `namespace`, and returns what the read-only client got wrong.
async fn probe_read_only(
    reader: &kube::Client,
    admin: &kube::Client,
    namespace: &str,
    role: &str,
) -> Result<Vec<String>, kube::Error> {
    use http::{Method, Request};
    use k8s_openapi::api::core::v1::{ConfigMap, Pod};
    use k8s_openapi::api::rbac::v1::ClusterRole;
    use kube::Api;
    use kube::api::{ListParams, ObjectMeta, PostParams};
    use kube::client::Body;

    let marker = "marker";
    let absent = "absent";
    let maps: Api<ConfigMap> = Api::namespaced(admin.clone(), namespace);
    let before = maps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: ObjectMeta {
                    name: Some(marker.to_owned()),
                    labels: Some([("flint-test".to_owned(), "read-only".to_owned())].into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?;
    Api::<ClusterRole>::all(admin.clone())
        .create(
            &PostParams::default(),
            &ClusterRole {
                metadata: ObjectMeta {
                    name: Some(role.to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?;

    let mut failures = Vec::new();
    let pods: Api<Pod> = Api::namespaced(reader.clone(), namespace);
    if let Err(e) = pods.list(&ListParams::default()).await {
        failures.push(format!("listing pods failed: {e}"));
    }
    let readable: Api<ConfigMap> = Api::namespaced(reader.clone(), namespace);
    if let Err(e) = readable.get(marker).await {
        failures.push(format!("reading the marker failed: {e}"));
    }

    let collection = format!("/api/v1/namespaces/{namespace}/configmaps");
    let map_path = format!("{collection}/{marker}");
    let pod_path = format!("/api/v1/namespaces/{namespace}/pods/absent");
    let body =
        format!(r#"{{"apiVersion":"v1","kind":"ConfigMap","metadata":{{"name":"{absent}"}}}}"#);
    let refused = [
        (Method::POST, collection.clone(), body),
        (Method::PUT, map_path.clone(), String::new()),
        (Method::PATCH, map_path.clone(), "{}".to_owned()),
        (Method::DELETE, map_path, String::new()),
        (
            Method::DELETE,
            format!("{collection}?labelSelector=flint-test%3Dread-only"),
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
            .expect("a valid request");
        match reader.send(request).await {
            Err(e) if e.to_string().contains("read-only") => {}
            Err(e) => failures.push(format!("{method} {path}: other error: {e}")),
            Ok(response) => failures.push(format!("{method} {path}: {}", response.status())),
        }
    }

    match Api::<ClusterRole>::all(reader.clone()).get(role).await {
        Err(e) if e.to_string().contains("name contains `;`") => {}
        other => failures.push(format!("a role named with `;`: {other:?}")),
    }
    match maps.get(marker).await {
        Ok(after) if after.metadata.resource_version == before.metadata.resource_version => {}
        other => failures.push(format!("the marker changed: {other:?}")),
    }
    match maps.get_opt(absent).await {
        Ok(None) => {}
        other => failures.push(format!("a refused create reached the server: {other:?}")),
    }
    Ok(failures)
}
