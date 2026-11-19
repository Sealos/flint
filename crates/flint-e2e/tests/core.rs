//! flint-core against the fake API server: discovery, Table lists and watches, the relist after
//! a 410, and 403s. No cluster needed; these run in plain `cargo test`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::read::{self, ListQuery};
use flint_core::{
    KindId, KindSnapshot, Settings, Subscription, SyncState, WatchMode, cluster, table,
};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, FakeRow, Fault, Gvr, Options, Verb, WatchStep};

const WAIT: Duration = Duration::from_secs(30);

/// A core on a scratch config dir, watching only what is open, connected to `server`.
struct Core {
    handle: CoreHandle,
    _dir: tempfile::TempDir,
}

impl Core {
    async fn start(server: &FakeApiServer, namespace: Option<&str>) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server.write_kubeconfig(&kubeconfig, namespace).unwrap();
        let settings = Settings {
            watch_mode: WatchMode::OnView,
            ..Settings::default()
        };
        let handle = CoreHandle::start(CoreConfig {
            config_dir: dir.path().join("config"),
            settings,
            settings_error: None,
            kubeconfig_base: vec![kubeconfig],
            home: None,
            watch_files: false,
        })
        .unwrap();
        let core = Core { handle, _dir: dir };
        core.activate().await;
        core
    }

    /// Activates the fake context and waits for its discovery.
    async fn activate(&self) {
        let events = self.handle.events();
        self.handle.send(Command::Activate(CONTEXT.to_owned()));
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

    fn subscribe(&self, kind: &KindId) -> Subscription {
        self.handle.subscribe(CONTEXT, kind)
    }
}

async fn until(
    sub: &mut Subscription,
    what: &str,
    pred: impl Fn(&KindSnapshot) -> bool,
) -> Arc<KindSnapshot> {
    tokio::time::timeout(WAIT, async {
        loop {
            let snap = sub.latest();
            if pred(&snap) {
                return snap;
            }
            assert!(sub.changed().await, "core stopped");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: never reached, last state {:?}", sub.peek().state))
}

fn names(snap: &KindSnapshot, namespace: &str) -> Vec<String> {
    snap.rows
        .iter()
        .filter(|r| r.namespace.as_deref() == Some(namespace))
        .map(|r| r.name.to_string())
        .collect()
}

fn configmaps() -> (Gvr, KindId) {
    (Gvr::core("configmaps"), KindId::new("", "configmaps"))
}

fn kubeconfig_of(server: &FakeApiServer, dir: &Path) -> flint_core::kubeconfig::LoadedKubeconfig {
    let path = dir.join("kubeconfig");
    server.write_kubeconfig(&path, None).unwrap();
    flint_core::kubeconfig::load(&[path])
}

// covers UP-03
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_and_discovers_every_recorded_kind() {
    let server = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let loaded = kubeconfig_of(&server, dir.path());
    let (client, version) = cluster::connect_and_check(&loaded.merged, CONTEXT, WAIT)
        .await
        .unwrap();
    assert_eq!(version.git_version, server.fixtures().version["gitVersion"]);
    let kinds = read::discover_kinds(&client).await.unwrap();
    let find = |group: &str, plural: &str| {
        kinds
            .iter()
            .find(|k| k.group() == group && k.plural() == plural)
            .unwrap_or_else(|| panic!("{group}/{plural} not discovered"))
    };
    assert!(find("", "pods").is_watchable() && find("", "pods").namespaced);
    assert!(find("apps", "deployments").is_listable());
    assert!(!find("", "nodes").namespaced);
    let metrics = find("metrics.k8s.io", "pods");
    assert!(metrics.is_listable() && !metrics.is_watchable());
    // Every recorded Table belongs to a discovered kind at its preferred version.
    for gvr in server.fixtures().tables.keys() {
        let kind = find(&gvr.group, &gvr.resource);
        assert_eq!(kind.resource.version, gvr.version, "{gvr}");
    }

    // The same through the core, as the app connects.
    let core = Core::start(&server, None).await;
    let discovery = core.handle.discovery(CONTEXT).await.expect("discovery");
    assert!(discovery.kind(&KindId::pods()).is_some());
    assert!(!discovery.navigation.sections.is_empty());
}

// covers UP-20, UP-23
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_tables_with_columns_and_pages() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
    let kinds = read::discover_kinds(&client).await.unwrap();
    let kind = |plural: &str| kinds.iter().find(|k| k.plural() == plural).unwrap();

    let deployments = table::list_table(
        &client,
        kind("deployments"),
        &ListQuery {
            namespace: Some("demo".into()),
            ..ListQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(deployments.format, flint_core::ListFormat::Table);
    let columns: Vec<&str> = deployments.columns.iter().map(|c| &*c.name).collect();
    assert!(
        columns.contains(&"Ready") && columns.contains(&"Up-to-date"),
        "{columns:?}"
    );
    let mut names: Vec<&str> = deployments.rows.iter().map(|r| &*r.name).collect();
    names.sort_unstable();
    assert_eq!(names, ["podinfo", "web"]);
    assert!(
        deployments
            .rows
            .iter()
            .all(|r| !r.uid.is_empty() && r.created.is_some())
    );

    let total = server.rows(&Gvr::core("pods")).len();
    let first = table::list_table(
        &client,
        kind("pods"),
        &ListQuery {
            limit: Some(2),
            ..ListQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(first.rows.len(), 2);
    assert!(first.more);
    let rest = table::list_table(
        &client,
        kind("pods"),
        &ListQuery {
            continue_token: first.continue_token.clone(),
            ..ListQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(first.rows.len() + rest.rows.len(), total);

    // The detail view: the object and the events about it, matched by uid.
    let core = Core::start(&server, None).await;
    let detail = core
        .handle
        .fetch_object(
            CONTEXT,
            &KindId::new("apps", "deployments"),
            Some("demo"),
            "web",
        )
        .await
        .unwrap();
    assert_eq!(detail.object["kind"], "Deployment");
    assert!(detail.managed_fields.is_some());
    assert!(
        !detail.events.is_empty(),
        "the recorded events about demo/web are found"
    );
}

// covers UP-20
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_lists_follow_watch_events() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, None).await;
    let (gvr, kind) = configmaps();
    let recorded = names_of(&server, &gvr, "demo");
    let mut sub = core.subscribe(&kind);
    let synced = until(&mut sub, "synced", |s| s.state == SyncState::Synced).await;
    assert_eq!(sorted(names(&synced, "demo")), recorded);
    // The initial sync was a streaming list that stays open as the watch.
    server.wait_for_watches(&gvr, 1, WAIT).await.unwrap();

    let data = server.column(&gvr, "Data").unwrap();
    let fresh = FakeRow::build(
        Some("demo"),
        "fresh",
        vec!["fresh".into(), 1.into(), "1d".into()],
    );
    assert_eq!(server.send_watch(&gvr, WatchStep::Added(fresh.clone())), 1);
    until(&mut sub, "added", |s| {
        names(s, "demo").contains(&"fresh".to_owned())
    })
    .await;

    server.send_watch(&gvr, WatchStep::Modified(fresh.clone().with_cell(data, 3)));
    until(&mut sub, "modified", |s| {
        // The core's columns may differ from the server's (it adds Namespace).
        let shown = s.columns.iter().position(|c| &*c.name == "Data");
        s.rows
            .iter()
            .any(|r| &*r.name == "fresh" && shown.is_some_and(|i| r.cell_text(i, 0) == "3"))
    })
    .await;

    server.send_watch(&gvr, WatchStep::Bookmark);
    server.send_watch(&gvr, WatchStep::Deleted(fresh));
    let gone = until(&mut sub, "deleted", |s| {
        !names(s, "demo").contains(&"fresh".to_owned())
    })
    .await;
    assert_eq!(gone.state, SyncState::Synced);
    assert_eq!(sorted(names(&gone, "demo")), recorded);
    assert_eq!(
        server.open_watches(&gvr),
        1,
        "bookmarks and events never broke the watch"
    );
}

fn names_of(server: &FakeApiServer, gvr: &Gvr, namespace: &str) -> Vec<String> {
    sorted(
        server
            .rows(gvr)
            .iter()
            .filter(|r| r.namespace() == Some(namespace))
            .map(|r| r.name().to_owned())
            .collect(),
    )
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// A 410 inside the watch makes the core list again: a change it missed shows up.
async fn relists_after_expiry(streaming_lists: bool) {
    let server = FakeApiServer::start_with(Options {
        streaming_lists,
        ..Options::default()
    })
    .await
    .unwrap();
    let core = Core::start(&server, None).await;
    let (gvr, kind) = configmaps();
    let mut sub = core.subscribe(&kind);
    let synced = until(&mut sub, "synced", |s| s.state == SyncState::Synced).await;
    let victim = names(&synced, "demo")
        .into_iter()
        .next()
        .expect("a demo configmap");
    server.wait_for_watches(&gvr, 1, WAIT).await.unwrap();
    let syncs_before = initial_syncs(&server, &gvr);

    // Missed while the watch was down: only a new list can show it.
    assert!(server.remove_row(&gvr, Some("demo"), &victim));
    server.send_watch(&gvr, WatchStep::Expired);
    until(&mut sub, "relisted", |s| {
        s.state == SyncState::Synced && !names(s, "demo").contains(&victim)
    })
    .await;
    assert_eq!(initial_syncs(&server, &gvr), syncs_before + 1);
    server.wait_for_watches(&gvr, 1, WAIT).await.unwrap();
}

/// Lists, or streaming lists, of `gvr` so far.
fn initial_syncs(server: &FakeApiServer, gvr: &Gvr) -> usize {
    server
        .requests()
        .iter()
        .filter(|r| r.is_about(gvr) && r.status == Some(200))
        .filter(|r| r.verb == Verb::List || r.parsed_query().send_initial_events)
        .count()
}

// covers UP-27
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relists_after_a_410_with_list_and_watch() {
    relists_after_expiry(false).await;
}

// covers UP-27
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relists_after_a_410_with_streaming_lists() {
    relists_after_expiry(true).await;
}

// covers UP-27
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_watch_reconnects() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, None).await;
    let (gvr, kind) = configmaps();
    let mut sub = core.subscribe(&kind);
    until(&mut sub, "synced", |s| s.state == SyncState::Synced).await;
    server.wait_for_watches(&gvr, 1, WAIT).await.unwrap();
    server.inject(Fault::slow(Duration::from_millis(300)).on(&gvr).times(1));
    server.send_watch(&gvr, WatchStep::Drop);
    let stale = until(&mut sub, "reconnecting", |s| {
        matches!(s.state, SyncState::Reconnecting { .. })
    })
    .await;
    assert!(!stale.rows.is_empty(), "rows are kept while reconnecting");
    until(&mut sub, "synced again", |s| s.state == SyncState::Synced).await;
    server.wait_for_watches(&gvr, 1, WAIT).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forbidden_lists_fail_or_fall_back_to_the_default_namespace() {
    let server = FakeApiServer::start().await.unwrap();
    let secrets = Gvr::core("secrets");
    let nodes = Gvr::core("nodes");
    for gvr in [&secrets, &nodes] {
        server.inject(
            Fault::forbidden()
                .on(gvr)
                .verb(Verb::List)
                .verb(Verb::Watch)
                .cluster_wide(),
        );
    }

    // A one-shot list reports the refusal.
    let client = server.client().await.unwrap();
    let kinds = read::discover_kinds(&client).await.unwrap();
    let node_kind = kinds.iter().find(|k| k.plural() == "nodes").unwrap();
    let err = table::list_table(&client, node_kind, &ListQuery::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("forbidden"), "{err}");

    let core = Core::start(&server, Some("demo")).await;
    // A cluster-scoped kind cannot fall back: the list fails and says why.
    let mut node_sub = core.subscribe(&KindId::new("", "nodes"));
    let failed = until(&mut node_sub, "nodes failed", |s| {
        matches!(s.state, SyncState::Failed { .. })
    })
    .await;
    let SyncState::Failed { error } = &failed.state else {
        unreachable!()
    };
    assert!(error.contains("forbidden"), "{error}");

    // A namespaced kind is listed in the context's namespace instead.
    let mut secret_sub = core.subscribe(&KindId::new("", "secrets"));
    let limited = until(&mut secret_sub, "secrets in demo", |s| {
        s.state == SyncState::Synced && s.limited_to.is_some()
    })
    .await;
    assert_eq!(limited.limited_to.as_ref().unwrap().listed, ["demo"]);
    assert_eq!(
        sorted(names(&limited, "demo")),
        names_of(&server, &secrets, "demo")
    );
    assert!(
        limited
            .rows
            .iter()
            .all(|r| r.namespace.as_deref() == Some("demo"))
    );
}
