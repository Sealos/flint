//! flint-core against the fake API server: discovery, Table lists and watches, the relist after
//! a 410, 403s, and log sessions on streaming logs through pod churn. No cluster needed; these
//! run in plain `cargo test`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::read::{self, ListQuery};
use flint_core::{
    KindId, KindSnapshot, LogOptions, LogSession, LogSnapshot, LogState, LogTarget, PodSet,
    Settings, Subscription, SyncState, WatchMode, WatchTiming, WorkloadKind, WorkloadTarget,
    cluster, table,
};
use flint_e2e::fake_api::{
    CONTEXT, ContainerLog, FakeApiServer, FakePod, Fault, Gvr, Options, Verb, WatchStep,
};

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
            // The fake server answers at once; real waits would only slow the tests down.
            watch_timing: WatchTiming {
                backoff_min: Duration::from_millis(10),
                backoff_max: Duration::from_millis(50),
                denied_backoff_min: Duration::from_millis(10),
            },
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
    let found = read::discover_kinds(&client).await.unwrap();
    assert!(
        found.skipped_groups.is_empty(),
        "{:?}",
        found.skipped_groups
    );
    let kinds = found.kinds;
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

    // The same through the core, as the app connects. Short names come from the recorded
    // aggregated discovery, so what users type resolves.
    let core = Core::start(&server, None).await;
    let discovery = core.handle.discovery(CONTEXT).await.expect("discovery");
    assert!(discovery.kind(&KindId::pods()).is_some());
    assert!(!discovery.navigation.sections.is_empty());
    let deploy = discovery.resolve("deploy").unwrap();
    assert_eq!((deploy.group(), deploy.plural()), ("apps", "deployments"));
    assert_eq!(discovery.resolve("po").unwrap().plural(), "pods");
    assert_eq!(
        discovery.resolve("gdg").unwrap().group(),
        "fixtures.flint.dev"
    );
    assert_eq!(
        discovery.nav_entry("deploy").unwrap().id,
        KindId::new("apps", "deployments")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_falls_back_per_group_only_when_aggregated_is_unsupported() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
    let discovery_requests = |server: &FakeApiServer| {
        server
            .requests()
            .into_iter()
            .filter(|r| r.verb == Verb::Discovery)
            .count()
    };
    // A server error is not "unsupported": discovery fails, no fallback.
    server.inject(Fault::status(500).verb(Verb::Discovery).times(1));
    let err = read::discover_kinds(&client).await.unwrap_err();
    assert_eq!(err.to_string(), "InternalError: injected fault: 500");
    assert_eq!(discovery_requests(&server), 1);

    // A 404 for aggregated discovery means "unsupported": the per-group fallback runs. The fake
    // server records no legacy discovery, so that fails too, but it was reached.
    server.inject(Fault::status(404).verb(Verb::Discovery).times(1));
    assert!(read::discover_kinds(&client).await.is_err());
    let after: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.verb == Verb::Discovery)
        .skip(1)
        .collect();
    assert!(after.len() >= 2, "{after:?}");
    assert!(after[1..].iter().all(|r| r.status == Some(404)));

    // Without faults aggregated discovery works again.
    assert!(
        !read::discover_kinds(&client)
            .await
            .unwrap()
            .kinds
            .is_empty()
    );
}

// covers UP-20, UP-23
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_tables_with_columns_and_pages() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
    let kinds = read::discover_kinds(&client).await.unwrap().kinds;
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
    let fresh = server.new_row(&gvr, Some("demo"), "fresh", &[("Data", "1")]);
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
    // Holds the reconnect long enough for a snapshot to show it (they come every 100 ms).
    server.inject(Fault::delay(Duration::from_millis(300)).on(&gvr).times(1));
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
    let kinds = read::discover_kinds(&client).await.unwrap().kinds;
    let node_kind = kinds.iter().find(|k| k.plural() == "nodes").unwrap();
    let err = table::list_table(&client, node_kind, &ListQuery::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("forbidden"), "{err}");

    let core = Core::start(&server, Some("demo")).await;
    // Both at once, so their retries overlap.
    let mut node_sub = core.subscribe(&KindId::new("", "nodes"));
    let mut secret_sub = core.subscribe(&KindId::new("", "secrets"));
    // A cluster-scoped kind cannot fall back: the list fails and says why.
    let failed = until(&mut node_sub, "nodes failed", |s| {
        matches!(s.state, SyncState::Failed { .. })
    })
    .await;
    let SyncState::Failed { error } = &failed.state else {
        unreachable!()
    };
    assert!(error.contains("forbidden"), "{error}");

    // A namespaced kind is listed in the context's namespace instead.
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

// covers UP-22
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_resources_show_their_printer_columns() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
    let kinds = read::discover_kinds(&client).await.unwrap().kinds;
    let gadgets = kinds
        .iter()
        .find(|k| k.group() == "fixtures.flint.dev" && k.plural() == "gadgets")
        .expect("the fixture CRD is discovered");
    let page = table::list_table(&client, gadgets, &ListQuery::default())
        .await
        .unwrap();
    let columns: Vec<&str> = page.columns.iter().map(|c| &*c.name).collect();
    assert!(
        columns.contains(&"Size") && columns.contains(&"Color"),
        "{columns:?}"
    );
    let size = page
        .columns
        .iter()
        .position(|c| &*c.name == "Size")
        .unwrap();
    let gadget = page.rows.iter().find(|r| &*r.name == "gadget-1").unwrap();
    assert_eq!(gadget.cell_text(size, 0), "3");
}

async fn until_log(
    session: &mut LogSession,
    what: &str,
    pred: impl Fn(&LogSnapshot) -> bool,
) -> Arc<LogSnapshot> {
    tokio::time::timeout(WAIT, async {
        loop {
            let snap = session.latest();
            if pred(&snap) {
                return snap;
            }
            assert!(session.changed().await, "log session stopped");
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{what}: never reached, last state {:?}",
            session.peek().state
        )
    })
}

/// The texts of a snapshot's lines, prefixed with `pod/container` in a workload session.
fn log_texts(snap: &LogSnapshot) -> Vec<String> {
    snap.lines
        .iter()
        .map(|(_, line)| match &line.source {
            Some(source) => format!("{} {}", source.label(), line.text),
            None => line.text.to_string(),
        })
        .collect()
}

// covers UP-40
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_log_streams_resumes_without_repeats_and_ends_with_its_pod() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    server.add_pod(FakePod::new("demo", "talker", &["app"]).label("app", "talker"));
    let log = ContainerLog::new("demo", "talker");
    let target = LogTarget {
        namespace: "demo".into(),
        pod: "talker".into(),
        container: None,
    };
    let mut session = core
        .handle
        .open_logs(CONTEXT, target, LogOptions::default());
    until_log(&mut session, "streaming", |s| {
        s.state == LogState::Streaming
    })
    .await;
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    server.append_log(&log, "one\ntwo\n");
    until_log(&mut session, "two lines", |s| {
        log_texts(s) == ["one", "two"]
    })
    .await;

    // The connection breaks: Flint reconnects from the last timestamp it showed, and the lines
    // of that second the server sends again are not shown twice.
    assert_eq!(server.drop_log_streams(&log), 1);
    until_log(&mut session, "reconnecting", |s| {
        matches!(s.state, LogState::Reconnecting { .. })
    })
    .await;
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    server.append_log(&log, "three\n");
    until_log(&mut session, "three lines", |s| {
        log_texts(s) == ["one", "two", "three"]
    })
    .await;
    let requests = server.log_requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    let resume = requests[1].parsed_query();
    assert!(resume.since_time.is_some() && resume.tail_lines.is_none() && resume.follow);
    assert!(resume.timestamps);

    // The pod is deleted as kubectl deletes it: it turns Terminating, its container stops and
    // the stream ends while the pod still exists (Succeeded), then it goes. The container's
    // last line reaches the log after the followed stream ended, while Flint looks the pod up
    // (held back here): only the last read (no follow) can catch it.
    server.inject(
        Fault::delay(Duration::from_millis(300))
            .on(&Gvr::core("pods"))
            .verb(Verb::Get)
            .named("talker")
            .times(1),
    );
    server.delete_pod("demo", "talker");
    assert_eq!(server.append_log(&log, "four\n"), 0, "no stream follows it");
    let ended = until_log(&mut session, "ended", |s| {
        matches!(s.state, LogState::Ended { .. })
    })
    .await;
    // The pod still exists when its stream ends, but it is being deleted: the session says so,
    // as an ordinary end rather than an error.
    assert_eq!(
        ended.state,
        LogState::Ended {
            error: None,
            pod_deleted: true,
        }
    );
    assert_eq!(log_texts(&ended), ["one", "two", "three", "four"]);
    // Flint read the stopped container to its end once more (no follow), without repeats.
    let last = server.log_requests().pop().unwrap();
    assert!(!last.parsed_query().follow && last.parsed_query().since_time.is_some());
    server.finish_delete("demo", "talker");
}

// covers UP-40
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminating_pod_keeps_its_log_until_its_container_stops() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    server.add_pod(FakePod::new("demo", "slow", &["app"]).label("app", "slow"));
    let log = ContainerLog::new("demo", "slow");
    let target = LogTarget {
        namespace: "demo".into(),
        pod: "slow".into(),
        container: None,
    };
    let mut session = core
        .handle
        .open_logs(CONTEXT, target, LogOptions::default());
    until_log(&mut session, "streaming", |s| {
        s.state == LogState::Streaming
    })
    .await;
    server.wait_for_log_streams(1, WAIT).await.unwrap();

    // Terminating, with a long grace period: the container still runs and logs its shutdown.
    server.begin_delete_pod("demo", "slow");
    server.append_log(&log, "draining\n");
    until_log(&mut session, "the first shutdown line", |s| {
        log_texts(s) == ["draining"]
    })
    .await;
    // The followed stream ends cleanly while the container still runs (an API server restart,
    // a proxy's idle timeout): Flint reconnects instead of ending the session.
    assert_eq!(server.end_log_streams(&log), 1);
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    server.append_log(&log, "closing connections\n");
    until_log(&mut session, "a shutdown line after the reconnect", |s| {
        log_texts(s) == ["draining", "closing connections"]
    })
    .await;

    // The container stops: now the session ends, saying why.
    server.finish_shutdown("demo", "slow");
    let ended = until_log(&mut session, "ended", |s| {
        matches!(s.state, LogState::Ended { .. })
    })
    .await;
    assert_eq!(
        ended.state,
        LogState::Ended {
            error: None,
            pod_deleted: true,
        }
    );
    assert_eq!(log_texts(&ended), ["draining", "closing connections"]);
    server.finish_delete("demo", "slow");
}

// covers UP-45
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workload_log_outlives_a_deleted_pod() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    let (a, b) = ("web-69c6f74b8b-qv4xv", "web-69c6f74b8b-x4gss");
    let target = WorkloadTarget {
        namespace: "demo".into(),
        pods: PodSet::Workload {
            kind: WorkloadKind::Deployment,
            name: "web".into(),
        },
        container: None,
    };
    let mut session = core
        .handle
        .open_logs(CONTEXT, target, LogOptions::default());
    until_log(&mut session, "two pods", |s| {
        s.pods.as_ref().is_some_and(|p| p.live() == 2)
    })
    .await;
    server.wait_for_log_streams(2, WAIT).await.unwrap();

    // One pod is deleted gracefully: its stream ends quietly, the session reads on.
    server.delete_pod("demo", b);
    server.finish_delete("demo", b);
    until_log(&mut session, "one pod left", |s| {
        s.pods
            .as_ref()
            .is_some_and(|p| p.pods == 1 && p.live() == 1 && p.failed() == 0)
    })
    .await;
    server.append_log(&ContainerLog::new("demo", a), "still here\n");
    let line = format!("{a}/nginx still here");
    let snap = until_log(&mut session, "the other pod's line", |s| {
        log_texts(s).contains(&line)
    })
    .await;
    assert_eq!(snap.state, LogState::Streaming);
}

// covers UP-42
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_live_pod_session_closes_its_stream() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    let target = LogTarget {
        namespace: "demo".into(),
        pod: "web-69c6f74b8b-qv4xv".into(),
        container: None,
    };
    let mut session = core
        .handle
        .open_logs(CONTEXT, target, LogOptions::default());
    until_log(&mut session, "streaming", |s| {
        s.state == LogState::Streaming
    })
    .await;
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    // The pod keeps running; only closing the session can close the stream.
    drop(session);
    server.wait_for_log_streams(0, WAIT).await.unwrap();
    assert!(server.pod("demo", "web-69c6f74b8b-qv4xv").is_some());
}

// covers UP-45
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workload_log_follows_its_pods_through_churn() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    let (a, b) = ("web-69c6f74b8b-qv4xv", "web-69c6f74b8b-x4gss");
    server.set_log("demo", a, "a1\n");
    server.set_log("demo", b, "b1\n");
    let target = WorkloadTarget {
        namespace: "demo".into(),
        pods: PodSet::Workload {
            kind: WorkloadKind::Deployment,
            name: "web".into(),
        },
        container: None,
    };
    let mut session = core
        .handle
        .open_logs(CONTEXT, target, LogOptions::default());
    let live = |n: usize| move |s: &LogSnapshot| s.pods.as_ref().is_some_and(|p| p.live() == n);
    until_log(&mut session, "two pods", live(2)).await;
    server.wait_for_log_streams(2, WAIT).await.unwrap();
    let both = until_log(&mut session, "both pods' lines", |s| s.lines.len() == 2).await;
    let mut texts = log_texts(&both);
    texts.sort();
    assert_eq!(texts, [format!("{a}/nginx a1"), format!("{b}/nginx b1")]);

    // Scale up: the new pod is Pending first; Flint opens its log only once it started, and
    // follows it without reopening the session.
    let new = "web-69c6f74b8b-new01";
    let pending = server
        .pod("demo", a)
        .unwrap()
        .sibling(new)
        .waiting("nginx", "ContainerCreating");
    server.add_pod(pending);
    until_log(&mut session, "a waiting pod", |s| {
        s.pods
            .as_ref()
            .is_some_and(|p| p.pods == 3 && p.waiting == 1 && p.live() == 2)
    })
    .await;
    let new_log_path = format!("/api/v1/namespaces/demo/pods/{new}/log");
    assert!(
        server.log_requests().iter().all(|r| r.path != new_log_path),
        "no log request before the container started"
    );
    server.start_container("demo", new, "nginx");
    until_log(&mut session, "three pods", live(3)).await;
    server.wait_for_log_streams(3, WAIT).await.unwrap();
    server.append_log(&ContainerLog::new("demo", new), "n1\n");
    let line = format!("{new}/nginx n1");
    until_log(&mut session, "the new pod's line", |s| {
        log_texts(s).contains(&line)
    })
    .await;

    // A rollout replaces b with a pod whose log is refused: that stream fails, the rest go on.
    let refused = "web-69c6f74b8b-new02";
    server.inject(
        Fault::forbidden()
            .on(&Gvr::core("pods"))
            .named(refused)
            .subresource("log"),
    );
    server.replace_pod("demo", b, server.pod("demo", a).unwrap().sibling(refused));
    let snap = until_log(&mut session, "one failed stream", |s| {
        s.pods
            .as_ref()
            .is_some_and(|p| p.failed() == 1 && p.live() == 2 && p.pods == 3)
    })
    .await;
    let pods = snap.pods.as_ref().unwrap();
    assert!(pods.streams.iter().all(|s| s.source.pod() != b), "{pods:?}");

    // A crash: until the container starts again, Flint's reconnects (from a1's second) read
    // the stopped instance, a1 included; it must not show a1 twice.
    let a_log_path = format!("/api/v1/namespaces/demo/pods/{a}/log");
    let before = server.log_requests().len();
    server.crash_container("demo", a, "nginx");
    let replay = server
        .wait_for_request(WAIT, |r| {
            r.path == a_log_path && r.status == Some(200) && r.parsed_query().since_time.is_some()
        })
        .await
        .unwrap();
    assert!(server.log_requests().len() > before, "{replay}");
    server.start_container("demo", a, "nginx");
    server.wait_for_log_streams(2, WAIT).await.unwrap();
    server.append_log(&ContainerLog::new("demo", a), "a2\n");
    let line = format!("{a}/nginx a2");
    let snap = until_log(&mut session, "the restarted pod's line", |s| {
        log_texts(s).contains(&line)
    })
    .await;
    let texts = log_texts(&snap);
    assert_eq!(
        texts.iter().filter(|t| t.ends_with(" a1")).count(),
        1,
        "{texts:?}"
    );
}
