//! flint-core against the fake API server while the kubeconfig changes on disk.

use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::settings::{self, AiScope};
use flint_core::write::{WriteError, WriteProposal, WriteReview, WriteTarget};
use flint_core::{KindId, Settings, SyncState, WatchMode, WatchTiming, WriteRefusal, kubeconfig};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Gvr, Verb, write_kubeconfigs};

const WAIT: Duration = Duration::from_secs(30);
const EDITED: &str = "edit-me";
/// Parses as neither YAML nor a kubeconfig.
const BROKEN: &str = "clusters:\n- name: [\n\"cut";

/// What the core said about the cluster list and the active context so far.
#[derive(Default)]
struct Seen {
    /// Every cluster list, as its context names and how many files failed to load.
    lists: Vec<(Vec<String>, usize)>,
    active_cleared: bool,
    notices: Vec<String>,
}

impl Seen {
    fn record(&mut self, event: Event) -> Option<(Vec<String>, usize)> {
        match event {
            Event::ClustersChanged(snapshot) => {
                let names = snapshot
                    .clusters
                    .iter()
                    .map(|c| c.info.name.clone())
                    .collect();
                let list = (names, snapshot.errors.len());
                self.lists.push(list.clone());
                return Some(list);
            }
            Event::ActiveChanged(None) => self.active_cleared = true,
            Event::Error(notice) => self.notices.push(notice),
            Event::ConnectionChanged {
                state: ConnectionState::Failed(e),
                ..
            } => panic!("connection failed: {e}"),
            _ => {}
        }
        None
    }
}

/// Reads events until a cluster list satisfies `done`, recording everything on the way.
async fn until_list(
    events: &flint_core::events::Events,
    seen: &mut Seen,
    what: &str,
    done: impl Fn(&[String], usize) -> bool,
) {
    tokio::time::timeout(WAIT, async {
        loop {
            let event = events.recv().await.expect("core running");
            if let Some((names, errors)) = seen.record(event)
                && done(&names, errors)
            {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: never reached"));
}

/// Whether a notice names the context and says it was removed.
fn removal_noticed(notices: &[String]) -> bool {
    notices
        .iter()
        .any(|n| n.contains(CONTEXT) && n.contains("was removed"))
}

// covers UP-06
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_active_context_deselects_it_and_stops_its_watches() {
    let server = FakeApiServer::start().await.unwrap();
    let gvr = Gvr::core("configmaps");
    let kind = KindId::new("", "configmaps");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    server.write_kubeconfig(&path, None).unwrap();

    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().join("config"),
        settings: Settings {
            watch_mode: WatchMode::OnView,
            ..Settings::default()
        },
        settings_error: None,
        kubeconfig_base: vec![path.clone()],
        home: None,
        watch_files: true,
        watch_timing: WatchTiming {
            backoff_min: Duration::from_millis(10),
            backoff_max: Duration::from_millis(50),
            denied_backoff_min: Duration::from_millis(10),
        },
    })
    .unwrap();
    let events = core.events();
    let mut seen = Seen::default();
    until_list(&events, &mut seen, "first load", |names, _| {
        names == [CONTEXT]
    })
    .await;

    core.send(Command::Activate(CONTEXT.to_owned()));
    let mut sub = core.subscribe(CONTEXT, &kind);
    tokio::time::timeout(WAIT, async {
        while sub.latest().state != SyncState::Synced {
            assert!(sub.changed().await, "core stopped");
        }
    })
    .await
    .expect("the live list syncs");
    server.wait_for_watches(&gvr, 1, WAIT).await.unwrap();

    assert_eq!(server.open_watches(&gvr), 1);
    assert!(!seen.active_cleared && seen.notices.is_empty());

    // Removing the active context deselects it with a notice and stops its watches.
    // The replacement is renamed over the watched file, so a reload never sees it half written.
    let replacement = dir.path().join("kubeconfig.next");
    write_kubeconfigs(&replacement, &[(&server, "another", None)]).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    until_list(&events, &mut seen, "the context's removal", |names, _| {
        names == ["another"]
    })
    .await;
    tokio::time::timeout(WAIT, async {
        while !(seen.active_cleared && removal_noticed(&seen.notices)) {
            let event = events.recv().await.expect("core running");
            seen.record(event);
        }
    })
    .await
    .expect("deselected with a notice");
    assert!(
        removal_noticed(&seen.notices),
        "notices: {:?}",
        seen.notices
    );
    tokio::time::timeout(WAIT, async {
        while server.open_watches(&gvr) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the removed context's watches stop");
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

// covers UP-06
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_half_written_kubeconfig_keeps_clusters_and_watches() {
    let server = FakeApiServer::start().await.unwrap();
    let gvr = Gvr::core("configmaps");
    let kind = KindId::new("", "configmaps");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    server.write_kubeconfig(&path, None).unwrap();
    let whole = std::fs::read_to_string(&path).unwrap();

    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().join("config"),
        settings: Settings {
            watch_mode: WatchMode::OnView,
            ..Settings::default()
        },
        settings_error: None,
        kubeconfig_base: vec![path.clone()],
        home: None,
        watch_files: true,
        watch_timing: WatchTiming {
            backoff_min: Duration::from_millis(10),
            backoff_max: Duration::from_millis(50),
            denied_backoff_min: Duration::from_millis(10),
        },
    })
    .unwrap();
    let events = core.events();
    let mut seen = Seen::default();
    until_list(&events, &mut seen, "first load", |names, _| {
        names == [CONTEXT]
    })
    .await;

    core.send(Command::Activate(CONTEXT.to_owned()));
    let mut sub = core.subscribe(CONTEXT, &kind);
    tokio::time::timeout(WAIT, async {
        while sub.latest().state != SyncState::Synced {
            assert!(sub.changed().await, "core stopped");
        }
    })
    .await
    .expect("the live list syncs");
    server.wait_for_watches(&gvr, 1, WAIT).await.unwrap();
    let syncs = initial_syncs(&server, &gvr);

    // Half the file, cut mid-token as a crashed writer leaves it, held past the reload debounce.
    let half = format!("{}\n\"cut", &whole[..whole.len() / 2]);
    std::fs::write(&path, half).unwrap();
    assert!(
        !kubeconfig::load(std::slice::from_ref(&path))
            .errors
            .is_empty(),
        "the cut file must not parse, or the test proves nothing"
    );
    // The reload that saw the half file reports it.
    until_list(
        &events,
        &mut seen,
        "reload of the half file",
        |_, errors| errors > 0,
    )
    .await;
    let halves = seen.lists.len();

    // Completing the file reloads it.
    std::fs::write(&path, &whole).unwrap();
    until_list(
        &events,
        &mut seen,
        "reload of the whole file",
        |_, errors| errors == 0,
    )
    .await;
    for (names, errors) in &seen.lists[1..] {
        assert_eq!(
            names,
            &[CONTEXT],
            "the context left the list ({errors} files failed)"
        );
    }
    assert!(seen.lists.len() > halves);
    assert!(!seen.active_cleared, "the active context was deselected");
    assert!(seen.notices.is_empty(), "notices: {:?}", seen.notices);
    assert_eq!(sub.latest().state, SyncState::Synced);
    assert_eq!(server.open_watches(&gvr), 1, "the list kept its watch");
    assert_eq!(
        initial_syncs(&server, &gvr),
        syncs,
        "the list was not synced again"
    );

    // Removing the active context deselects it with a notice and stops its watches.
    let replacement = dir.path().join("kubeconfig.next");
    write_kubeconfigs(&replacement, &[(&server, "another", None)]).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    until_list(&events, &mut seen, "the context's removal", |names, _| {
        names == ["another"]
    })
    .await;
    tokio::time::timeout(WAIT, async {
        while !(seen.active_cleared && removal_noticed(&seen.notices)) {
            let event = events.recv().await.expect("core running");
            seen.record(event);
        }
    })
    .await
    .expect("deselected with a notice");
    tokio::time::timeout(WAIT, async {
        while server.open_watches(&gvr) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the removed context's watches stop");
}

fn deployments() -> Gvr {
    Gvr::new("apps", "v1", "deployments")
}

fn target(server: &FakeApiServer) -> WriteTarget {
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

fn proposal() -> WriteProposal {
    WriteProposal::ApplyYaml {
        yaml: format!(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {EDITED}\nspec:\n  replicas: 3\n"
        ),
    }
}

/// Every request that reached `server` and could change something, dry runs included.
fn mutating(server: &FakeApiServer) -> usize {
    server
        .arrivals()
        .iter()
        .filter(|r| r.method != http::Method::GET)
        .count()
}

/// A core on `path` with a deployment to edit on `server`, connected and discovered.
async fn core_with_deployment(
    server: &FakeApiServer,
    dir: &std::path::Path,
    path: &std::path::Path,
) -> CoreHandle {
    core_with_settings(server, dir, path, Settings::default()).await
}

/// [`core_with_deployment`], starting from `settings`.
async fn core_with_settings(
    server: &FakeApiServer,
    dir: &std::path::Path,
    path: &std::path::Path,
    settings: Settings,
) -> CoreHandle {
    server.set_object(
        &deployments(),
        serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "name": EDITED,
                "namespace": "demo",
                "uid": "uid-edit",
                "resourceVersion": "5000",
            },
            "spec": {"replicas": 1},
        }),
    );
    server.write_kubeconfig(path, Some("demo")).unwrap();
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.join("config"),
        settings: Settings {
            watch_mode: WatchMode::OnView,
            ..settings
        },
        settings_error: None,
        kubeconfig_base: vec![path.to_owned()],
        home: None,
        watch_files: false,
        watch_timing: WatchTiming::default(),
    })
    .unwrap();
    core.send(Command::Activate(CONTEXT.to_owned()));
    tokio::time::timeout(WAIT, async {
        while core.discovery(CONTEXT).await.is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("discovery in time");
    core
}

/// A core where the fake cluster, on this machine, is protected as a remote one would be, in
/// `settings.toml` too, since saving a pin reads the file back.
async fn protected_core(
    server: &FakeApiServer,
    dir: &std::path::Path,
    path: &std::path::Path,
) -> CoreHandle {
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join(settings::SETTINGS_FILE),
        "protect_local_clusters = true\n",
    )
    .unwrap();
    let settings = settings::load_settings(&config).unwrap();
    assert!(settings.protect_local_clusters);
    core_with_settings(server, dir, path, settings).await
}

fn expose(core: &CoreHandle, server: Option<String>) {
    core.send(Command::SetAiExposed {
        cluster: CONTEXT.to_owned(),
        server,
        scope: AiScope::default(),
    });
}

/// Runs `review`, waiting out a reconnect after a reload.
async fn run_when_settled(core: &CoreHandle, review: &WriteReview) -> Result<(), WriteError> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let result = core.run_write(review.clone().into_intent()).await;
        let waiting = matches!(
            &result,
            Err(WriteError::Refused(WriteRefusal::NotConnected { .. }))
                | Err(WriteError::NotServed(_))
        );
        if !waiting || tokio::time::Instant::now() > deadline {
            return result.map(drop);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// covers UP-06, UP-54: with the file broken, a new review and a run of an earlier one are refused
// and send nothing that could change the cluster. The same file back, the earlier review runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_kubeconfig_refuses_writes_until_the_same_file_is_back() {
    let server = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    let core = core_with_deployment(&server, dir.path(), &path).await;
    let whole = std::fs::read_to_string(&path).unwrap();
    let review = core
        .review_write(target(&server), proposal())
        .await
        .unwrap();
    let before = mutating(&server);

    std::fs::write(&path, BROKEN).unwrap();
    core.reload_kubeconfig().await.unwrap();
    assert_eq!(
        core.review_write(target(&server), proposal())
            .await
            .unwrap_err(),
        WriteError::KubeconfigUnreadable
    );
    assert_eq!(
        core.run_write(review.clone().into_intent())
            .await
            .unwrap_err(),
        WriteError::KubeconfigUnreadable
    );
    assert_eq!(mutating(&server), before, "no mutating request");

    std::fs::write(&path, &whole).unwrap();
    core.reload_kubeconfig().await.unwrap();
    run_when_settled(&core, &review).await.unwrap();
    let object = server.object(&deployments(), Some("demo"), EDITED).unwrap();
    assert_eq!(object["spec"]["replicas"], 3);
}

// covers UP-06, UP-54: a file back with another token or server does not run the earlier review.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_kubeconfig_back_with_another_definition_refuses_the_old_review() {
    let server = FakeApiServer::start().await.unwrap();
    let elsewhere = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    let core = core_with_deployment(&server, dir.path(), &path).await;
    let whole = std::fs::read_to_string(&path).unwrap();
    assert!(whole.contains("user: {}"), "{whole}");
    let review = core
        .review_write(target(&server), proposal())
        .await
        .unwrap();
    let before = mutating(&server);

    std::fs::write(&path, BROKEN).unwrap();
    core.reload_kubeconfig().await.unwrap();
    std::fs::write(&path, whole.replace("user: {}", "user: {token: another}")).unwrap();
    core.reload_kubeconfig().await.unwrap();
    assert_eq!(
        run_when_settled(&core, &review).await.unwrap_err(),
        WriteError::ConnectionChanged
    );

    std::fs::write(&path, BROKEN).unwrap();
    core.reload_kubeconfig().await.unwrap();
    elsewhere.write_kubeconfig(&path, Some("demo")).unwrap();
    core.reload_kubeconfig().await.unwrap();
    assert!(run_when_settled(&core, &review).await.is_err());
    assert_eq!(mutating(&server), before, "nothing sent to the old server");
    assert_eq!(mutating(&elsewhere), 0, "nothing sent to the new server");
}

// covers UP-06, UP-55: while its file can't be read a context takes no new Trust or exposure, so
// a file back with other credentials at the same URL is still protected and gets no write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_context_takes_no_new_pin() {
    let server = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    let core = protected_core(&server, dir.path(), &path).await;
    let whole = std::fs::read_to_string(&path).unwrap();
    assert!(whole.contains("user: {}"), "{whole}");

    std::fs::write(&path, BROKEN).unwrap();
    core.reload_kubeconfig().await.unwrap();
    let events = core.events();
    core.trust(CONTEXT, &server.url());
    expose(&core, Some(server.url()));
    tokio::time::timeout(WAIT, async {
        let (mut trust, mut exposure) = (false, false);
        while !(trust && exposure) {
            if let Event::Error(notice) = events.recv().await.expect("core running") {
                trust |= notice.contains("was not trusted");
                exposure |= notice.contains("was not exposed");
            }
        }
    })
    .await
    .expect("both pins refused in time");
    let saved = settings::load_settings(&dir.path().join("config")).unwrap();
    assert!(saved.trusted_clusters.is_empty());
    assert!(saved.ai_exposed_clusters.is_empty());

    std::fs::write(&path, whole.replace("user: {}", "user: {token: another}")).unwrap();
    core.reload_kubeconfig().await.unwrap();
    let refusal = tokio::time::timeout(WAIT, async {
        loop {
            match core.review_write(target(&server), proposal()).await {
                Err(WriteError::Refused(WriteRefusal::NotConnected { .. }))
                | Err(WriteError::NotServed(_)) => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                other => return other.err(),
            }
        }
    })
    .await
    .expect("an answer in time");
    assert_eq!(
        refusal,
        Some(WriteError::Refused(WriteRefusal::Protected {
            cluster: CONTEXT.to_owned(),
        }))
    );
    assert_eq!(mutating(&server), 0, "no mutating request");
}

// covers UP-06, UP-55: a pin made before the file broke can still be removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_context_can_lose_its_pins() {
    let server = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    let core = protected_core(&server, dir.path(), &path).await;
    let config = dir.path().join("config");
    core.trust(CONTEXT, &server.url());
    expose(&core, Some(server.url()));
    until_pins(&config, 1).await;

    std::fs::write(&path, BROKEN).unwrap();
    core.reload_kubeconfig().await.unwrap();
    core.untrust(CONTEXT);
    expose(&core, None);
    until_pins(&config, 0).await;
}

/// Waits until `settings.toml` holds `count` Trust pins and `count` exposures.
async fn until_pins(config: &std::path::Path, count: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(saved) = settings::load_settings(config)
                && saved.trusted_clusters.len() == count
                && saved.ai_exposed_clusters.len() == count
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the pins saved in time");
}
