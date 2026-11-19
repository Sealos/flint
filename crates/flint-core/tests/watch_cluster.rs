//! Watch manager tests against the dev cluster, through the public `CoreHandle` API. Ignored by
//! default; run with `--include-ignored` and `FLINT_TEST_KUBECONFIG` pointing at the throwaway
//! kind cluster (`scripts/dev-cluster.sh`). They use that file alone, and only create objects in
//! namespaces prefixed `m1fix-` (plus one CRD in the `m1fix.flint.dev` group), which they delete
//! again and wait for. One test scales `demo/web` and scales it back.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::{
    Cell, ClusterDiscovery, Feed, KindId, KindSnapshot, Settings, Subscription, SyncState,
    WatchMode, cluster, kubeconfig,
};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::Client;
use kube::api::{Api, DeleteParams, DynamicObject, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, GroupVersionKind};
use serde_json::json;

/// How long a change may take to show in a live list: about a second on a laptop. Shared CI
/// runners are slower and noisier, so 5 s when `CI` is set. `FLINT_TEST_LATENCY_MS` overrides both.
fn latency_bound() -> Duration {
    let ms = std::env::var("FLINT_TEST_LATENCY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if std::env::var_os("CI").is_some() {
            5000
        } else {
            2000
        });
    Duration::from_millis(ms)
}

fn test_kubeconfig() -> PathBuf {
    match std::env::var_os("FLINT_TEST_KUBECONFIG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => panic!("FLINT_TEST_KUBECONFIG must point at the dev cluster's kubeconfig"),
    }
}

struct Core {
    handle: CoreHandle,
    events: async_channel::Receiver<Event>,
    _dir: tempfile::TempDir,
}

fn start(kubeconfig: PathBuf, settings: Settings) -> Core {
    // Core logs on stderr, filtered by RUST_LOG.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    // On disk too, as the app would have it: settings writes edit the file.
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(
        dir.path().join("config/settings.toml"),
        toml::to_string(&settings).unwrap(),
    )
    .unwrap();
    let handle = CoreHandle::start(CoreConfig {
        config_dir: dir.path().join("config"),
        settings,
        settings_error: None,
        kubeconfig_base: vec![kubeconfig],
        home: None,
        watch_files: false,
    })
    .unwrap();
    let events = handle.events();
    Core {
        handle,
        events,
        _dir: dir,
    }
}

impl Core {
    /// The first event for which `pick` returns something.
    async fn wait_for<T>(&self, secs: u64, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        tokio::time::timeout(Duration::from_secs(secs), async {
            loop {
                let event = self.events.recv().await.expect("core running");
                if let Event::ConnectionChanged {
                    state: ConnectionState::Failed(e),
                    ..
                } = &event
                {
                    panic!("connection failed: {e}");
                }
                if let Some(t) = pick(event) {
                    return t;
                }
            }
        })
        .await
        .expect("event arrived in time")
    }

    /// Activates the first context and waits for its discovery.
    async fn activate_first(&self) -> (String, Arc<ClusterDiscovery>) {
        let name = self
            .wait_for(10, |e| match e {
                Event::ClustersChanged(s) => Some(s.clusters[0].info.name.clone()),
                _ => None,
            })
            .await;
        self.activate(&name).await
    }

    async fn activate(&self, name: &str) -> (String, Arc<ClusterDiscovery>) {
        self.handle.send(Command::Activate(name.to_owned()));
        let discovery = self
            .wait_for(60, |e| match e {
                Event::DiscoveryChanged {
                    cluster,
                    discovery: Some(d),
                    ..
                } if cluster == name => Some(d),
                _ => None,
            })
            .await;
        (name.to_owned(), discovery)
    }

    async fn wait_watched(
        &self,
        cluster: &str,
        pred: impl Fn(&[(KindId, Feed)]) -> bool,
    ) -> Vec<(KindId, Feed)> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let watched = self.handle.watched_kinds(cluster).await;
            if pred(&watched) {
                return watched;
            }
            assert!(
                Instant::now() < deadline,
                "watched kinds never matched: {watched:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn until(
    sub: &mut Subscription,
    secs: u64,
    pred: impl Fn(&KindSnapshot) -> bool,
) -> Arc<KindSnapshot> {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let snap = sub.latest();
            if pred(&snap) {
                return snap;
            }
            assert!(sub.changed().await, "core stopped");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("snapshot condition not reached: {:?}", sub.peek().state))
}

async fn client(path: &Path) -> Client {
    let loaded = kubeconfig::load(&[path.to_owned()]);
    let context = &loaded.contexts[0].name;
    cluster::connect(&loaded.merged, context).await.unwrap()
}

fn web_pods(snap: &KindSnapshot) -> usize {
    snap.rows
        .iter()
        .filter(|r| r.namespace.as_deref() == Some("demo") && r.label("app") == Some("web"))
        .count()
}

async fn scale_web(client: &Client, replicas: i32) {
    Api::<Deployment>::namespaced(client.clone(), "demo")
        .patch_scale(
            "web",
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"replicas": replicas}})),
        )
        .await
        .unwrap();
}

// covers UP-20, UP-23
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn pods_list_then_live_update_after_scaling() {
    let path = test_kubeconfig();
    let core = start(path.clone(), Settings::default());
    let (cluster, _) = core.activate_first().await;
    let mut pods = core.handle.subscribe(&cluster, &KindId::pods());
    let snap = until(&mut pods, 30, |s| s.state == SyncState::Synced).await;
    let columns: Vec<&str> = snap.columns.iter().map(|c| &*c.name).collect();
    assert_eq!(
        &columns[..6],
        ["Name", "Namespace", "Ready", "Status", "Restarts", "Age"]
    );
    assert_eq!(web_pods(&snap), 2, "demo/web starts with 2 replicas");
    let before: Vec<String> = snap.rows.iter().map(|r| r.uid.to_string()).collect();

    let kube = client(&path).await;
    scale_web(&kube, 3).await;
    let scaled = Instant::now();
    let snap = until(&mut pods, 10, |s| web_pods(s) == 3).await;
    let latency = scaled.elapsed();
    eprintln!("scale-up visible after {latency:?}");
    let bound = latency_bound();
    assert!(
        latency < bound,
        "changes appear within about a second (bound {bound:?}, FLINT_TEST_LATENCY_MS)"
    );

    // The new pod's detail: full object without managedFields, and its events by uid.
    let new_pod = snap
        .rows
        .iter()
        .find(|r| r.label("app") == Some("web") && !before.contains(&r.uid.to_string()))
        .unwrap()
        .clone();
    let detail = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let d = core
                .handle
                .fetch_object(&cluster, &KindId::pods(), Some("demo"), &new_pod.name)
                .await
                .unwrap();
            if !d.events.is_empty() {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("the new pod has events");
    assert!(detail.object["metadata"].get("managedFields").is_none());
    assert!(!detail.yaml.contains("managedFields"));
    assert!(detail.yaml.contains("kind: Pod"));
    assert!(
        detail
            .events
            .iter()
            .all(|e| e.involved_object.uid.as_deref() == Some(&*new_pod.uid))
    );

    scale_web(&kube, 2).await;
    until(&mut pods, 60, |s| web_pods(s) == 2).await;
}

const CRD_GROUP: &str = "m1fix.flint.dev";
const CRD_NAMESPACE: &str = "m1fix-crd";

/// Waits until `get` reports the object gone (a delete is only a request; namespaces and CRDs
/// take a while to go).
async fn until_gone<F, Fut>(what: &str, get: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(120);
    while get().await {
        assert!(Instant::now() < deadline, "{what} was not deleted in time");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Deletes the test CRD and namespace, if there, and waits until both are gone, so a run
/// right after another (or after an interrupted one) starts clean.
async fn remove_crd_leftovers(kube: &Client) {
    let crds: Api<CustomResourceDefinition> = Api::all(kube.clone());
    let namespaces: Api<Namespace> = Api::all(kube.clone());
    let crd_name = format!("widgets.{CRD_GROUP}");
    let _ = namespaces
        .delete(CRD_NAMESPACE, &DeleteParams::default())
        .await;
    let _ = crds.delete(&crd_name, &DeleteParams::default()).await;
    until_gone("the test namespace", || async {
        namespaces.get_opt(CRD_NAMESPACE).await.unwrap().is_some()
    })
    .await;
    until_gone("the test CRD", || async {
        crds.get_opt(&crd_name).await.unwrap().is_some()
    })
    .await;
}

fn widget_crd() -> CustomResourceDefinition {
    serde_json::from_value(json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": format!("widgets.{CRD_GROUP}")},
        "spec": {
            "group": CRD_GROUP,
            "scope": "Namespaced",
            "names": {"plural": "widgets", "singular": "widget", "kind": "Widget"},
            "versions": [{
                "name": "v1", "served": true, "storage": true,
                "schema": {"openAPIV3Schema": {"type": "object", "properties": {
                    "spec": {"type": "object", "properties": {
                        "size": {"type": "integer"}, "color": {"type": "string"}}}}}},
                "additionalPrinterColumns": [
                    {"name": "Size", "type": "integer", "jsonPath": ".spec.size"},
                    {"name": "Color", "type": "string", "jsonPath": ".spec.color", "priority": 1},
                    {"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}
                ]
            }]
        }
    }))
    .unwrap()
}

// covers UP-22, UP-28
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn crd_with_printer_columns_appears_live() {
    let path = test_kubeconfig();
    let kube = client(&path).await;
    let crds: Api<CustomResourceDefinition> = Api::all(kube.clone());
    let namespaces: Api<Namespace> = Api::all(kube.clone());
    // Leftovers from an interrupted run, or the cleanup of the last one still running.
    remove_crd_leftovers(&kube).await;

    let core = start(path.clone(), Settings::default());
    let (cluster, discovery) = core.activate_first().await;
    let widgets_id = KindId::new(CRD_GROUP, "widgets");
    assert!(discovery.kind(&widgets_id).is_none());

    // Metrics are polled, not watched, and come back as a Table.
    let metrics_id = KindId::new("metrics.k8s.io", "pods");
    let mut metrics = core.handle.subscribe(&cluster, &metrics_id);
    let snap = until(&mut metrics, 60, |s| s.state == SyncState::Synced).await;
    assert_eq!(snap.feed, Feed::Poll);
    assert!(!snap.rows.is_empty());

    crds.create(&PostParams::default(), &widget_crd())
        .await
        .unwrap();
    // Discovery refreshes because the CRD list changed, without reconnecting.
    let discovery = core
        .wait_for(60, |e| match e {
            Event::DiscoveryChanged {
                discovery: Some(d), ..
            } if d.kind(&widgets_id).is_some() => Some(d),
            _ => None,
        })
        .await;
    let section = discovery
        .navigation
        .sections
        .iter()
        .find(|s| s.title == CRD_GROUP)
        .expect("a section for the CRD's group");
    assert_eq!(section.entries[0].label, "Widgets");
    assert!(section.entries[0].watchable && section.entries[0].namespaced);

    namespaces
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({"metadata": {"name": CRD_NAMESPACE}})).unwrap(),
        )
        .await
        .unwrap();
    let ar = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(CRD_GROUP, "v1", "Widget"),
        "widgets",
    );
    let widget: DynamicObject = serde_json::from_value(json!({
        "apiVersion": format!("{CRD_GROUP}/v1"), "kind": "Widget",
        "metadata": {"name": "w1", "namespace": CRD_NAMESPACE},
        "spec": {"size": 3, "color": "red"}
    }))
    .unwrap();
    Api::<DynamicObject>::namespaced_with(kube.clone(), CRD_NAMESPACE, &ar)
        .create(&PostParams::default(), &widget)
        .await
        .unwrap();

    let mut widgets = core.handle.subscribe(&cluster, &widgets_id);
    let snap = until(&mut widgets, 30, |s| {
        s.state == SyncState::Synced && s.rows.len() == 1
    })
    .await;
    let columns: Vec<(&str, i32)> = snap
        .columns
        .iter()
        .map(|c| (&*c.name, c.priority))
        .collect();
    assert_eq!(
        columns,
        [
            ("Name", 0),
            ("Namespace", 0),
            ("Size", 0),
            ("Color", 1),
            ("Age", 0)
        ]
    );
    let row = &snap.rows[0];
    assert_eq!(row.cells[2], Cell::Int(3));
    assert_eq!(row.cell_text(3, 0), "red");
    assert_eq!(row.cells[4], Cell::Age);

    // Cleanup; discovery drops the kind and the open view says so.
    remove_crd_leftovers(&kube).await;
    core.wait_for(60, |e| match e {
        Event::DiscoveryChanged {
            discovery: Some(d), ..
        } if d.kind(&widgets_id).is_none() => Some(()),
        _ => None,
    })
    .await;
    until(&mut widgets, 30, |s| s.state == SyncState::Unavailable).await;
}

/// A copy of the dev kubeconfig with a second context for the same cluster.
fn two_context_kubeconfig(dir: &Path) -> (PathBuf, String, String) {
    let mut kc = kube::config::Kubeconfig::read_from(test_kubeconfig()).unwrap();
    let first = kc.contexts[0].clone();
    let mut second = first.clone();
    second.name = format!("{}-second", first.name);
    kc.contexts.push(second.clone());
    kc.current_context = Some(first.name.clone());
    let path = dir.join("kubeconfig");
    std::fs::write(&path, serde_json::to_string(&kc).unwrap()).unwrap();
    (path, first.name, second.name)
}

// covers UP-24, UP-25
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn on_view_versus_all_and_switching_clusters() {
    let kc_dir = tempfile::tempdir().unwrap();
    let (path, first, second) = two_context_kubeconfig(kc_dir.path());
    let settings = Settings {
        watch_mode: WatchMode::OnView,
        ..Default::default()
    };
    let core = start(path, settings);
    core.wait_for(10, |e| matches!(e, Event::ClustersChanged(_)).then_some(()))
        .await;
    core.activate(&first).await;
    let crds = KindId::custom_resource_definitions();
    core.wait_watched(&first, |w| w.len() == 1 && w[0].0 == crds)
        .await;

    // On view: opening a list watches that kind.
    let configmaps = KindId::new("", "configmaps");
    let mut sub = core.handle.subscribe(&first, &configmaps);
    until(&mut sub, 30, |s| s.state == SyncState::Synced).await;
    core.wait_watched(&first, |w| w.iter().any(|(k, _)| *k == configmaps))
        .await;

    // All, for this cluster only: everything listable and watchable.
    core.handle.send(Command::SetWatchMode {
        cluster: Some(first.clone()),
        mode: Some(WatchMode::All),
    });
    let watched = core.wait_watched(&first, |w| w.len() > 20).await;
    assert!(watched.iter().all(|(_, f)| *f == Feed::Watch));
    let settings = core
        .wait_for(10, |e| match e {
            Event::SettingsChanged { settings, .. } => Some(settings),
            _ => None,
        })
        .await;
    assert_eq!(settings.watch_mode_for(&first), WatchMode::All);
    assert_eq!(
        settings.watch_mode_for(&second),
        WatchMode::OnView,
        "persisted per cluster"
    );

    // Switching clusters stops every watch of the old one, open views included.
    core.activate(&second).await;
    core.wait_watched(&first, |w| w.is_empty()).await;
    let snap = until(&mut sub, 10, |s| s.state == SyncState::Idle).await;
    assert!(snap.rows.is_empty());
    core.wait_watched(&second, |w| w.len() == 1).await;

    // Switching back lists again (the connection and discovery were kept).
    core.handle.send(Command::Activate(first.clone()));
    until(&mut sub, 30, |s| {
        s.state == SyncState::Synced && !s.rows.is_empty()
    })
    .await;
    core.wait_watched(&first, |w| w.len() > 20).await;
    core.wait_watched(&second, |w| w.is_empty()).await;
}

const RBAC_ALLOWED: [&str; 2] = ["m1fix-rbac-a", "m1fix-rbac-b"];
const RBAC_DENIED: &str = "m1fix-rbac-c";
const RBAC_USER: &str = "m1fix-viewer";

async fn remove_namespaces(kube: &Client, names: &[&str]) {
    let namespaces: Api<Namespace> = Api::all(kube.clone());
    for name in names {
        let _ = namespaces.delete(name, &DeleteParams::default()).await;
    }
    for name in names {
        until_gone(name, || async {
            namespaces.get_opt(name).await.unwrap().is_some()
        })
        .await;
    }
}

async fn create<K>(api: &Api<K>, value: serde_json::Value)
where
    K: kube::Resource + Clone + std::fmt::Debug + serde::de::DeserializeOwned + serde::Serialize,
{
    api.create(
        &PostParams::default(),
        &serde_json::from_value(value).unwrap(),
    )
    .await
    .unwrap();
}

/// A kubeconfig for the dev cluster's API server that authenticates as `token`, with two
/// contexts: `m1fix-viewer` (no default namespace) and `m1fix-viewer-ns` (default namespace
/// `default_ns`). Built from the dev kubeconfig's CA, so nothing else is reused.
fn token_kubeconfig(dir: &Path, token: &str, default_ns: &str) -> PathBuf {
    let dev = kube::config::Kubeconfig::read_from(test_kubeconfig()).unwrap();
    let cluster = dev.clusters[0].cluster.clone().unwrap();
    let kc = json!({
        "apiVersion": "v1",
        "kind": "Config",
        "clusters": [{"name": "dev", "cluster": {
            "server": cluster.server,
            "certificate-authority-data": cluster.certificate_authority_data,
        }}],
        "users": [{"name": RBAC_USER, "user": {"token": token}}],
        "contexts": [
            {"name": RBAC_USER, "context": {"cluster": "dev", "user": RBAC_USER}},
            {"name": format!("{RBAC_USER}-ns"), "context": {
                "cluster": "dev", "user": RBAC_USER, "namespace": default_ns}},
        ],
        "current-context": RBAC_USER,
    });
    let path = dir.join("kubeconfig");
    std::fs::write(&path, serde_json::to_string(&kc).unwrap()).unwrap();
    path
}

fn row_names(snap: &KindSnapshot) -> Vec<String> {
    snap.rows
        .iter()
        .map(|r| format!("{}/{}", r.namespace.as_deref().unwrap_or(""), r.name))
        .collect()
}

// covers UP-20, UP-21, UP-23
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn a_user_allowed_only_some_namespaces_can_still_browse() {
    use k8s_openapi::api::authentication::v1::TokenRequest;
    use k8s_openapi::api::core::v1::{ConfigMap, ServiceAccount};
    use k8s_openapi::api::rbac::v1::{Role, RoleBinding};

    let admin = client(&test_kubeconfig()).await;
    let mut all = RBAC_ALLOWED.to_vec();
    all.push(RBAC_DENIED);
    remove_namespaces(&admin, &all).await;

    // Three namespaces with a ConfigMap each. The service account may get, list and watch
    // ConfigMaps (not events) in the first two only, through Roles: nothing cluster-wide.
    let namespaces: Api<Namespace> = Api::all(admin.clone());
    for ns in &all {
        create(&namespaces, json!({"metadata": {"name": ns}})).await;
        let cms: Api<ConfigMap> = Api::namespaced(admin.clone(), ns);
        create(&cms, json!({"metadata": {"name": format!("cm-{ns}")}})).await;
    }
    let home = RBAC_ALLOWED[0];
    let sas: Api<ServiceAccount> = Api::namespaced(admin.clone(), home);
    create(&sas, json!({"metadata": {"name": RBAC_USER}})).await;
    for ns in RBAC_ALLOWED {
        let roles: Api<Role> = Api::namespaced(admin.clone(), ns);
        create(
            &roles,
            json!({"metadata": {"name": RBAC_USER}, "rules": [{
                "apiGroups": [""], "resources": ["configmaps"],
                "verbs": ["get", "list", "watch"]}]}),
        )
        .await;
        let bindings: Api<RoleBinding> = Api::namespaced(admin.clone(), ns);
        create(
            &bindings,
            json!({"metadata": {"name": RBAC_USER},
                "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": RBAC_USER},
                "subjects": [{"kind": "ServiceAccount", "name": RBAC_USER, "namespace": home}]}),
        )
        .await;
    }
    let token = sas
        .create_token_request(
            RBAC_USER,
            &PostParams::default(),
            &serde_json::from_value::<TokenRequest>(
                json!({"spec": {"audiences": [], "expirationSeconds": 3600}}),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .status
        .and_then(|s| s.token)
        .expect("a token");
    let dir = tempfile::tempdir().unwrap();
    let kubeconfig = token_kubeconfig(dir.path(), &token, home);

    let settings = Settings {
        watch_mode: WatchMode::OnView,
        ..Default::default()
    };
    let core = start(kubeconfig, settings);
    core.wait_for(10, |e| matches!(e, Event::ClustersChanged(_)).then_some(()))
        .await;
    let (cluster, _) = core.activate(RBAC_USER).await;
    let configmaps = KindId::new("", "configmaps");
    let mut sub = core.handle.subscribe(&cluster, &configmaps);

    // All namespaces are refused, and the user may not list namespaces either: there is
    // nothing to fall back to yet, and the list says how to get one.
    let snap = until(&mut sub, 60, |s| {
        matches!(s.state, SyncState::Failed { .. }) && s.limited_to.is_some()
    })
    .await;
    let SyncState::Failed { error } = &snap.state else {
        unreachable!()
    };
    assert!(error.contains("namespace filter"), "{error}");

    // The namespace filter names the namespaces: they are listed one by one and merged.
    core.handle.send(Command::SetNamespaceFilter {
        cluster: cluster.clone(),
        namespaces: all.iter().map(|n| n.to_string()).collect(),
    });
    let snap = until(&mut sub, 60, |s| {
        s.state == SyncState::Synced
            && s.limited_to
                .as_ref()
                .is_some_and(|l| l.denied == [RBAC_DENIED])
    })
    .await;
    let limit = snap.limited_to.as_ref().unwrap();
    assert_eq!(limit.listed, RBAC_ALLOWED);
    assert!(limit.reason.contains("forbidden"), "{}", limit.reason);
    let names = row_names(&snap);
    for ns in RBAC_ALLOWED {
        assert!(names.contains(&format!("{ns}/cm-{ns}")), "{names:?}");
    }
    assert!(
        snap.rows
            .iter()
            .all(|r| RBAC_ALLOWED.contains(&r.namespace.as_deref().unwrap())),
        "{names:?}"
    );
    let columns: Vec<&str> = snap.columns.iter().map(|c| &*c.name).collect();
    assert_eq!(&columns[..2], ["Name", "Namespace"]);
    assert_eq!(
        core.handle.watched_kinds(&cluster).await,
        [
            (configmaps.clone(), Feed::Watch),
            (KindId::custom_resource_definitions(), Feed::Watch),
        ]
    );

    // Live updates arrive in a namespace-limited list too.
    let cms: Api<ConfigMap> = Api::namespaced(admin.clone(), RBAC_ALLOWED[1]);
    create(&cms, json!({"metadata": {"name": "cm-live"}})).await;
    let live = format!("{}/cm-live", RBAC_ALLOWED[1]);
    until(&mut sub, 10, |s| row_names(s).contains(&live)).await;

    // The detail of an object the user may read, though not its events.
    let detail = core
        .handle
        .fetch_object(&cluster, &configmaps, Some(home), &format!("cm-{home}"))
        .await
        .expect("the object, even without events");
    assert!(detail.events.is_empty());
    assert!(
        detail
            .events_error
            .as_deref()
            .is_some_and(|e| e.contains("forbidden")),
        "{:?}",
        detail.events_error
    );

    // Without a filter, a context's default namespace is used.
    drop(sub);
    let (second, _) = core.activate(&format!("{RBAC_USER}-ns")).await;
    let mut sub = core.handle.subscribe(&second, &configmaps);
    let snap = until(&mut sub, 60, |s| {
        s.state == SyncState::Synced && s.limited_to.is_some()
    })
    .await;
    assert_eq!(snap.limited_to.as_ref().unwrap().listed, [home]);
    assert!(row_names(&snap).contains(&format!("{home}/cm-{home}")));

    drop(sub);
    drop(core);
    remove_namespaces(&admin, &all).await;
}
