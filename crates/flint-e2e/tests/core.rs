//! flint-core against the fake API server: discovery, Table lists and watches, the relist after
//! a 410, 403s, log sessions on streaming logs through pod churn, and exec sessions over the
//! fake's WebSocket. No cluster needed; these run in plain `cargo test`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::read::{self, ListQuery};
use flint_core::{
    ExecFailureKind, ExecRequest, ExecSession, ExecSnapshot, ExecState, KindId, KindSnapshot,
    LogOptions, LogSession, LogSnapshot, LogState, LogTarget, PodSet, Settings, Subscription,
    SyncState, TerminalSize, WatchMode, WatchTiming, WorkloadKind, WorkloadTarget, cluster, table,
};
use flint_e2e::fake_api::{
    CONTEXT, ContainerLog, ExecEnd, FakeApiServer, FakePod, Fault, Gvr, Options, TtySize, Verb,
    WatchStep,
};

const WAIT: Duration = Duration::from_secs(30);

/// A core on a scratch config dir, watching only what is open, connected to `server`.
struct Core {
    handle: CoreHandle,
    _dir: tempfile::TempDir,
}

impl Core {
    async fn start(server: &FakeApiServer, namespace: Option<&str>) -> Core {
        Core::start_with(server, namespace, Settings::default()).await
    }

    /// [`Core::start`] with `settings` (watching only what is open).
    async fn start_with(
        server: &FakeApiServer,
        namespace: Option<&str>,
        settings: Settings,
    ) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server.write_kubeconfig(&kubeconfig, namespace).unwrap();
        let settings = Settings {
            watch_mode: WatchMode::OnView,
            ..settings
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

// covers UP-40
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_gone_before_its_status_caught_up_ends_as_deleted() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    server.add_pod(FakePod::new("demo", "lagging", &["app"]).label("app", "lagging"));
    let log = ContainerLog::new("demo", "lagging");
    let target = LogTarget {
        namespace: "demo".into(),
        pod: "lagging".into(),
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
    server.append_log(&log, "bye\n");
    until_log(&mut session, "the line", |s| log_texts(s) == ["bye"]).await;

    // The kubelet ends the stream before the pod's status shows the container stopped: Flint
    // looks, finds it Terminating but running, and waits to reconnect. Meanwhile the pod goes.
    server.begin_delete_pod("demo", "lagging");
    assert_eq!(server.end_log_streams(&log), 1);
    server
        .wait_for_request(WAIT, |r| {
            r.path == "/api/v1/namespaces/demo/pods/lagging" && r.status == Some(200)
        })
        .await
        .unwrap();
    server.finish_delete("demo", "lagging");

    // The reopen finds no pod: it was deleted, which is no error.
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
    assert_eq!(log_texts(&ended), ["bye"]);
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

async fn until_exec(
    session: &mut ExecSession,
    what: &str,
    pred: impl Fn(&ExecSnapshot) -> bool,
) -> Arc<ExecSnapshot> {
    tokio::time::timeout(WAIT, async {
        loop {
            let snap = session.latest();
            if pred(&snap) {
                return snap;
            }
            assert!(session.changed().await, "exec session stopped");
        }
    })
    .await
    .unwrap_or_else(|_| {
        let snap = session.peek();
        panic!(
            "{what}: never reached, last state {:?}\n{}",
            snap.state,
            snap.terminal.text()
        )
    })
}

fn has_line(snap: &ExecSnapshot, line: &str) -> bool {
    (0..snap.terminal.size.rows).any(|r| snap.terminal.row_text(r).trim_end() == line)
}

fn running(command: &str) -> ExecState {
    ExecState::Running {
        command: vec![command.to_owned()],
    }
}

/// The pod of the fixtures' web deployment: nginx, which has bash.
const WEB: &str = "web-69c6f74b8b-qv4xv";

/// Opens a shell in [`WEB`] and waits until it fails, returning its failure's kind.
async fn shell_failure(core: &Core) -> ExecFailureKind {
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    let snap = until_exec(&mut shell, "the failure", |s| s.state.is_finished()).await;
    match &snap.state {
        ExecState::Failed(failure) => failure.kind,
        other => panic!("{other:?}"),
    }
}

/// Opens a shell in [`WEB`] and waits for its bash prompt.
async fn shell_runs(core: &Core) {
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    let prompt = format!("root@{WEB}:/#");
    until_exec(&mut shell, "the bash prompt", |s| {
        s.state == running("bash") && has_line(s, &prompt)
    })
    .await;
}

// covers UP-54, UP-55: the core refuses a shell on a protected cluster before anything reaches
// the API server; an unlock pinned to the server lets it through until Lock Again, and so
// does trust, written to settings.toml. (The fake server is on this machine, which starts
// unlocked; `protect_local_clusters` protects it.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_cluster_refuses_shells_until_unlocked_or_trusted() {
    let server = FakeApiServer::start().await.unwrap();
    let settings = Settings {
        protect_local_clusters: true,
        ..Settings::default()
    };
    let core = Core::start_with(&server, Some("demo"), settings).await;
    assert_eq!(shell_failure(&core).await, ExecFailureKind::Protected);
    assert!(
        server.exec_requests().is_empty(),
        "nothing reached the cluster"
    );

    // An unlock at another server does not cover it.
    core.handle
        .unlock(CONTEXT, "https://elsewhere.example:6443");
    assert_eq!(shell_failure(&core).await, ExecFailureKind::Protected);
    core.handle.unlock(CONTEXT, &server.url());
    shell_runs(&core).await;
    assert_eq!(server.exec_requests().len(), 1);
    assert!(core.handle.lock(CONTEXT));
    assert_eq!(shell_failure(&core).await, ExecFailureKind::Protected);
    assert_eq!(server.exec_requests().len(), 1);

    let events = core.handle.events();
    core.handle.trust(CONTEXT, &server.url());
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
    shell_runs(&core).await;
    assert_eq!(server.exec_requests().len(), 2);
}

// covers UP-41
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_gets_a_bash_prompt_echoes_typing_and_resizes() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    let request = ExecRequest::shell(CONTEXT, "demo", WEB).size(TerminalSize::new(90, 20));
    let mut shell = core.handle.exec(request);
    let prompt = format!("root@{WEB}:/#");
    until_exec(&mut shell, "the bash prompt", |s| {
        s.state == running("bash") && has_line(s, &prompt)
    })
    .await;
    let fake = server.last_exec().unwrap();
    assert_eq!(fake.command(), ["bash"]);
    assert!(fake.tty());
    // The terminal's size goes to the TTY before anything the test does.
    fake.wait_for_resizes(1, WAIT).await.unwrap();
    assert_eq!(
        fake.resizes()[0],
        TtySize {
            width: 90,
            height: 20
        }
    );

    // Enter is a carriage return; the echo and the output reach the screen.
    shell.write(b"echo hi\r".to_vec());
    fake.wait_for_stdin(b"echo hi\r", WAIT).await.unwrap();
    until_exec(&mut shell, "the echo", |s| {
        has_line(s, &format!("{prompt} echo hi")) && has_line(s, "hi")
    })
    .await;

    shell.resize(TerminalSize::new(100, 30));
    fake.wait_for_resizes(2, WAIT).await.unwrap();
    assert_eq!(
        fake.resizes()[1],
        TtySize {
            width: 100,
            height: 30
        }
    );
    shell.write(b"stty size\r".to_vec());
    until_exec(&mut shell, "the new size", |s| has_line(s, "30 100")).await;

    // bash turned bracketed paste on, so a paste arrives bracketed; Ctrl-C drops the line.
    shell.paste("echo a\necho b");
    fake.wait_for_stdin(b"\x1b[200~echo a\necho b\x1b[201~", WAIT)
        .await
        .unwrap();
    shell.write(vec![0x03]);
    fake.wait_for_stdin(b"\x1b[201~\x03", WAIT).await.unwrap();
    // Output the test pushes shows too.
    assert!(fake.print("\r\npushed by the test\r\n"));
    until_exec(&mut shell, "pushed output", |s| {
        has_line(s, "pushed by the test")
    })
    .await;
    assert_eq!(server.exec_requests().len(), 1);
}

// covers UP-41
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_container_without_bash_falls_back_to_sh() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    let mut shell = core.handle.exec(ExecRequest::shell(
        CONTEXT,
        "demo",
        "podinfo-c6497595d-b8chj",
    ));
    until_exec(&mut shell, "sh to start", |s| {
        s.state == running("/bin/sh") && has_line(s, "~ $")
    })
    .await;
    let commands: Vec<Vec<String>> = server
        .exec_requests()
        .iter()
        .map(|r| r.parsed_query().command)
        .collect();
    assert_eq!(commands, [["bash"], ["/bin/sh"]]);
    let sessions = server.exec_sessions();
    assert!(matches!(
        sessions[0].wait_for_end(WAIT).await.unwrap(),
        ExecEnd::Failed(_)
    ));
    // busybox asked where the cursor is, and the terminal answered.
    sessions[1]
        .wait_for_stdin(b"\x1b[1;5R", WAIT)
        .await
        .unwrap();
    shell.write(b"echo from-sh\r".to_vec());
    until_exec(&mut shell, "the echo", |s| has_line(s, "from-sh")).await;
    assert_eq!(server.open_exec_sessions(), 1);
}

// covers UP-41
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_codes_end_the_session_and_keep_the_screen() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    until_exec(&mut shell, "bash", |s| s.state == running("bash")).await;
    server.wait_for_exec_sessions(1, WAIT).await.unwrap();
    shell.write(b"echo bye\r".to_vec());
    until_exec(&mut shell, "the echo", |s| has_line(s, "bye")).await;
    shell.write(b"exit 3\r".to_vec());
    let snap = until_exec(&mut shell, "the exit", |s| s.state.is_finished()).await;
    assert_eq!(snap.state, ExecState::Exited { code: Some(3) });
    assert!(has_line(&snap, "bye") && has_line(&snap, "exit"));
    let fake = server.last_exec().unwrap();
    assert_eq!(fake.wait_for_end(WAIT).await.unwrap(), ExecEnd::Exited(3));
    server.wait_for_exec_sessions(0, WAIT).await.unwrap();
    // Typing after the exit goes nowhere. The paste is answered after the write was handled
    // (inputs are handled in order), and says no program takes input any more: that is the
    // check that can fail. The screen and stdin checks below only document the outcome (Flint
    // doesn't echo locally, and the fake has closed the session).
    shell.write(b"echo late\r".to_vec());
    assert!(!shell.paste_if_bracketed("x").await);
    let snap = shell.latest();
    assert!(
        !snap.terminal.text().contains("late"),
        "{}",
        snap.terminal.text()
    );
    assert!(!String::from_utf8_lossy(&fake.stdin()).contains("late"));

    // Ctrl-D on an empty line ends bash with the last status.
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    until_exec(&mut shell, "bash", |s| s.state == running("bash")).await;
    shell.write(vec![0x04]);
    let snap = until_exec(&mut shell, "the exit", |s| s.state.is_finished()).await;
    assert_eq!(snap.state, ExecState::Exited { code: Some(0) });
    assert!(server.last_exec_stdin().ends_with(&[0x04]));
}

// covers UP-41
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_connection_fails_the_session() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    until_exec(&mut shell, "bash", |s| s.state == running("bash")).await;
    server.wait_for_exec_sessions(1, WAIT).await.unwrap();
    // Flint shows Running before it sends the first resize; break the connection after it, so
    // the resize does not fail on a connection already gone.
    let fake = server.last_exec().unwrap();
    fake.wait_for_resizes(1, WAIT).await.unwrap();
    assert_eq!(server.drop_exec_sessions(), 1);
    let snap = until_exec(&mut shell, "the failure", |s| s.state.is_finished()).await;
    match &snap.state {
        ExecState::Failed(failure) => {
            assert_eq!(failure.kind, ExecFailureKind::Connection, "{failure}");
            assert!(failure.contains("the exec connection broke"), "{failure}");
        }
        other => panic!("{other:?}"),
    }

    // A clean close without a status is an exit with no code.
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    until_exec(&mut shell, "bash", |s| s.state == running("bash")).await;
    server.wait_for_exec_sessions(1, WAIT).await.unwrap();
    // As above: closed before the first resize, the resize's failure would win.
    let fake = server.last_exec().unwrap();
    fake.wait_for_resizes(1, WAIT).await.unwrap();
    assert!(fake.close());
    let snap = until_exec(&mut shell, "the end", |s| s.state.is_finished()).await;
    assert_eq!(snap.state, ExecState::Exited { code: None });
}

// covers UP-41
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_upgrades_and_rbac_fail_with_their_cause() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    server.inject(Fault::refuse_upgrade().times(1));
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    let snap = until_exec(&mut shell, "the failure", |s| s.state.is_finished()).await;
    match &snap.state {
        ExecState::Failed(failure) => {
            assert_eq!(failure.kind, ExecFailureKind::Upgrade, "{failure}");
        }
        other => panic!("{other:?}"),
    }
    // Only one request: a refused upgrade is not a missing shell.
    assert_eq!(server.exec_requests().len(), 1);
    assert!(server.exec_sessions().is_empty());

    server.inject(
        Fault::forbidden()
            .on(&Gvr::core("pods"))
            .subresource("exec")
            .times(1),
    );
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    let snap = until_exec(&mut shell, "the failure", |s| s.state.is_finished()).await;
    match &snap.state {
        ExecState::Failed(failure) => {
            assert_eq!(failure.kind, ExecFailureKind::Server, "{failure}");
            assert!(
                failure.contains("cannot get resource \"pods/exec\""),
                "{failure}"
            );
        }
        other => panic!("{other:?}"),
    }
}

// covers UP-41
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxy_that_strips_the_upgrade_is_an_upgrade_failure() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;
    server.inject(Fault::strip_upgrade().times(1));
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    let snap = until_exec(&mut shell, "the failure", |s| s.state.is_finished()).await;
    match &snap.state {
        ExecState::Failed(failure) => {
            assert_eq!(failure.kind, ExecFailureKind::Upgrade, "{failure}");
            assert!(failure.contains("Upgrade request required"), "{failure}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(server.exec_requests().len(), 1);
}

// covers UP-41
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_container_fails_with_the_kubelets_reason_before_or_after_the_upgrade() {
    for kubelet_websockets in [true, false] {
        let server = FakeApiServer::start_with(Options {
            kubelet_websockets,
            ..Options::default()
        })
        .await
        .unwrap();
        let core = Core::start(&server, Some("demo")).await;
        let mut shell = core
            .handle
            .exec(ExecRequest::shell(CONTEXT, "demo", "fixture-job-qd9jf"));
        let snap = until_exec(&mut shell, "the failure", |s| s.state.is_finished()).await;
        // After the upgrade the status says why; before it, the kubelet's plain-text reason,
        // which kube wraps as an unparsed body, is read as the server's too.
        let ExecState::Failed(failure) = &snap.state else {
            panic!("{kubelet_websockets}: {:?}", snap.state);
        };
        assert_eq!(
            failure.kind,
            ExecFailureKind::Server,
            "{kubelet_websockets}: {failure}"
        );
        assert!(
            failure.contains("container not found (\"job\")"),
            "{kubelet_websockets}: {failure}"
        );
        // Not a missing shell: `/bin/sh` is not tried.
        assert_eq!(server.exec_requests().len(), 1, "{kubelet_websockets}");
        assert_eq!(
            server.exec_sessions().len(),
            usize::from(!kubelet_websockets)
        );
    }
}

// covers UP-41, UP-42
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_or_dropping_a_shell_closes_its_connection() {
    let server = FakeApiServer::start().await.unwrap();
    let core = Core::start(&server, Some("demo")).await;

    // Dropped, as closing a dock tab does. The shell keeps running; only Flint can end it.
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    until_exec(&mut shell, "bash", |s| s.state == running("bash")).await;
    server.wait_for_exec_sessions(1, WAIT).await.unwrap();
    let fake = server.last_exec().unwrap();
    drop(shell);
    assert_eq!(fake.wait_for_end(WAIT).await.unwrap(), ExecEnd::ClientGone);
    server.wait_for_exec_sessions(0, WAIT).await.unwrap();

    // Closed explicitly.
    let mut shell = core.handle.exec(ExecRequest::shell(CONTEXT, "demo", WEB));
    until_exec(&mut shell, "bash", |s| s.state == running("bash")).await;
    server.wait_for_exec_sessions(1, WAIT).await.unwrap();
    let fake = server.last_exec().unwrap();
    shell.close();
    assert_eq!(fake.wait_for_end(WAIT).await.unwrap(), ExecEnd::ClientGone);
    server.wait_for_exec_sessions(0, WAIT).await.unwrap();
    assert_eq!(server.exec_sessions().len(), 2);
}

// covers UP-15
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_reads_logs_and_events_as_written() {
    // Redaction is for agents (`flint mcp`); people see their own cluster's text unchanged.
    let server = FakeApiServer::start().await.unwrap();
    let token = "Authorization: Bearer s3cr3t-t0ken-value-0123";
    let url = "https://bot:hunter2@registry.example.com";
    server.set_log("demo", "web-69c6f74b8b-qv4xv", &format!("{token}\n"));
    server.set_object(
        &server.gvr("events"),
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Event",
            "metadata": {"name": "web.pull", "namespace": "demo"},
            "involvedObject": {"kind": "Pod", "name": "web-69c6f74b8b-qv4xv", "namespace": "demo"},
            "reason": "Failed",
            "message": format!("Failed to pull from {url}"),
            "type": "Warning",
        }),
    );

    let core = Core::start(&server, Some("demo")).await;
    let target = LogTarget {
        namespace: "demo".into(),
        pod: "web-69c6f74b8b-qv4xv".into(),
        container: None,
    };
    let mut session = core
        .handle
        .open_logs(CONTEXT, target, LogOptions::default());
    let snap = until_log(&mut session, "the line", |s| !s.lines.is_empty()).await;
    assert_eq!(log_texts(&snap), [token]);

    let dir = tempfile::tempdir().unwrap();
    let loaded = kubeconfig_of(&server, dir.path());
    let (client, _) = cluster::connect_and_check(&loaded.merged, CONTEXT, WAIT)
        .await
        .unwrap();
    let (events, _) = read::events(
        &client,
        &read::EventQuery {
            namespace: Some("demo".into()),
            involved: Some(read::InvolvedObject {
                kind: None,
                name: Some("web-69c6f74b8b-qv4xv".into()),
            }),
            field_selector: None,
            fetch_limit: None,
        },
    )
    .await
    .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message.as_deref().is_some_and(|m| m.contains(url))),
        "{events:?}"
    );
}

/// Events about objects never get an answer: the server accepts the list and stalls.
fn stall_events(server: &FakeApiServer) {
    server.inject(
        Fault::delay(Duration::from_secs(3600))
            .on(&Gvr::core("events"))
            .verb(Verb::List),
    );
}

/// Waits up to `within` until the fake server is handling exactly `count` requests.
async fn until_in_flight(server: &FakeApiServer, count: usize, within: Duration, what: &str) {
    tokio::time::timeout(within, async {
        while server.requests_in_flight() != count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("{what}: requests in flight did not reach {count} within {within:?}")
    });
}

/// How many events requests have reached the fake server.
fn events_requests(server: &FakeApiServer) -> usize {
    server
        .arrivals()
        .iter()
        .filter(|r| r.is_about(&Gvr::core("events")))
        .count()
}

/// Waits until `count` events requests have arrived and the server holds exactly those: the
/// detail reads got past their object and stalled on the events.
async fn until_events_stalled(server: &FakeApiServer, count: usize, held: usize, what: &str) {
    tokio::time::timeout(WAIT, async {
        while events_requests(server) < count || server.requests_in_flight() != held {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: never stalled on the events"));
}

/// Far shorter than `read::DETAIL_TIMEOUT`, so a read let go only by its deadline fails here.
const GIVEN_UP: Duration = Duration::from_secs(5);

// covers UP-23: a closed detail pane stops its read, and with it the object it holds (#258).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_detail_request_cancels_its_read() {
    let server = FakeApiServer::start().await.unwrap();
    stall_events(&server);
    let core = Core::start(&server, None).await;
    let deployments = KindId::new("apps", "deployments");
    let fetch = core
        .handle
        .fetch_object(CONTEXT, &deployments, Some("demo"), "web");
    // The object came back; the events request is held.
    until_events_stalled(&server, 1, 1, "web's detail").await;
    // The pane closes: nobody waits for the answer any more.
    drop(fetch);
    until_in_flight(&server, 0, GIVEN_UP, "the stalled events request given up").await;
}

// covers UP-23: stepping through many rows on a stalled server leaves no reads behind (#258).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepping_through_rows_leaves_no_detail_reads_behind() {
    let server = FakeApiServer::start().await.unwrap();
    stall_events(&server);
    let core = Core::start(&server, None).await;
    let pods = KindId::pods();
    let names: Vec<String> = server
        .rows(&Gvr::core("pods"))
        .into_iter()
        .filter(|r| r.namespace() == Some("demo"))
        .map(|r| r.name().to_string())
        .collect();
    assert!(names.len() >= 5, "{names:?}");
    // Each row visited asks for its detail, and the next one replaces it.
    for round in 0..4 {
        for name in &names {
            let asked = events_requests(&server) + 1;
            let fetch = core.handle.fetch_object(CONTEXT, &pods, Some("demo"), name);
            until_events_stalled(&server, asked, 1, &format!("round {round}: {name}")).await;
            drop(fetch);
            until_in_flight(
                &server,
                0,
                GIVEN_UP,
                &format!("round {round}: {name} given up"),
            )
            .await;
        }
    }
    // Several at once, then all closed.
    let asked = events_requests(&server) + names.len();
    let fetches: Vec<_> = names
        .iter()
        .map(|name| core.handle.fetch_object(CONTEXT, &pods, Some("demo"), name))
        .collect();
    until_events_stalled(&server, asked, names.len(), "every row").await;
    drop(fetches);
    until_in_flight(&server, 0, GIVEN_UP, "every read given up").await;
}

// covers UP-23: events that never come still show the object, with why they are missing (#258).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_events_request_returns_the_object_after_the_deadline() {
    let server = FakeApiServer::start().await.unwrap();
    stall_events(&server);
    let client = server.client().await.unwrap();
    let kinds = read::discover_kinds(&client).await.unwrap().kinds;
    let deployments = kinds.iter().find(|k| k.plural() == "deployments").unwrap();
    let started = std::time::Instant::now();
    // Bounded here too: without a working deadline the read would wait out the fault's hour.
    let detail = tokio::time::timeout(
        Duration::from_secs(10),
        read::object_detail_within(
            &client,
            deployments,
            Some("demo"),
            "web",
            Duration::from_secs(1),
        ),
    )
    .await
    .expect("the deadline ends the read")
    .expect("the object, despite the events");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(detail.object["kind"], "Deployment");
    assert!(detail.events.is_empty());
    let error = detail.events_error.expect("why the events are missing");
    assert!(error.contains("timed out"), "{error}");
    // Timing out dropped the events request.
    until_in_flight(&server, 0, GIVEN_UP, "the timed-out events request").await;
}
