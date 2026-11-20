//! CoreHandle behavior that needs no cluster: loading, reloading on file change, and settings
//! writes. The kubeconfig files point at unreachable servers and nothing is activated, so no
//! network traffic happens.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flint_core::Settings;
use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};

fn kubeconfig(contexts: &[&str]) -> String {
    let mut s = String::from(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: https://127.0.0.1:1\nusers:\n- name: u\n  user:\n    token: t\ncontexts:\n",
    );
    for name in contexts {
        s += &format!("- name: {name}\n  context:\n    cluster: c\n    user: u\n");
    }
    s
}

async fn next_clusters(events: &flint_core::events::Events) -> Vec<String> {
    loop {
        match events.recv().await.unwrap() {
            Event::ClustersChanged(snapshot) => {
                return snapshot
                    .clusters
                    .iter()
                    .map(|c| c.info.name.clone())
                    .collect();
            }
            _ => continue,
        }
    }
}

fn start(dir: &Path, watch: bool) -> CoreHandle {
    CoreHandle::start(CoreConfig {
        config_dir: dir.join("config"),
        settings: Default::default(),
        settings_error: None,
        kubeconfig_base: vec![dir.join("kubeconfig")],
        home: None,
        watch_files: watch,
        watch_timing: Default::default(),
    })
    .unwrap()
}

// covers UP-06
#[tokio::test]
async fn reloads_when_kubeconfig_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    std::fs::write(&path, kubeconfig(&["one"])).unwrap();
    let core = start(dir.path(), true);
    let events = core.events();

    let first = tokio::time::timeout(Duration::from_secs(5), next_clusters(&events))
        .await
        .unwrap();
    assert_eq!(first, ["one"]);

    // Give the watcher a moment to start, then replace the file the way editors do.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let tmp = dir.path().join("kubeconfig.tmp");
    std::fs::write(&tmp, kubeconfig(&["one", "two"])).unwrap();
    std::fs::rename(&tmp, &path).unwrap();

    let second = tokio::time::timeout(Duration::from_secs(10), next_clusters(&events))
        .await
        .expect("reload after the file changed");
    assert_eq!(second, ["one", "two"]);
}

// covers UP-02: launched before ~/.kube exists, Flint picks up a kubeconfig created later,
// without a restart.
#[tokio::test]
async fn picks_up_a_kubeconfig_whose_directory_is_created_later() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".kube").join("config");
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().join("config"),
        settings: Default::default(),
        settings_error: None,
        kubeconfig_base: vec![path.clone()],
        home: None,
        watch_files: true,
        watch_timing: Default::default(),
    })
    .unwrap();
    let events = core.events();
    let first = tokio::time::timeout(Duration::from_secs(5), next_clusters(&events))
        .await
        .unwrap();
    assert!(first.is_empty());

    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::write(&path, kubeconfig(&["late"])).unwrap();
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let clusters = next_clusters(&events).await;
            if !clusters.is_empty() {
                return clusters;
            }
        }
    })
    .await
    .expect("the new kubeconfig is loaded");
    assert_eq!(found, ["late"]);
}

const SERVER: &str = "https://127.0.0.1:1";

/// The next `SettingsChanged` event that satisfies `pred`.
async fn settings_where(
    events: &flint_core::events::Events,
    pred: impl Fn(&Settings, Option<&str>) -> bool,
) -> (Arc<Settings>, Option<String>) {
    loop {
        if let Event::SettingsChanged { settings, error } = events.recv().await.unwrap()
            && pred(&settings, error.as_deref())
        {
            return (settings, error);
        }
    }
}

// covers UP-07
#[tokio::test]
async fn set_ai_exposed_writes_context_and_server() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one"])).unwrap();
    let core = start(dir.path(), false);
    let events = core.events();
    next_clusters(&events).await;
    core.send(Command::SetAiExposed {
        cluster: "one".into(),
        server: Some(SERVER.into()),
        scope: Default::default(),
    });
    let (settings, _) = tokio::time::timeout(
        Duration::from_secs(5),
        settings_where(&events, |s, _| {
            s.ai_exposure("one", Some(SERVER)).is_exposed()
        }),
    )
    .await
    .unwrap();
    assert_eq!(settings.ai_exposed_clusters.len(), 1);
    let on_disk = flint_core::settings::load_settings(&dir.path().join("config")).unwrap();
    assert!(on_disk.ai_exposure("one", Some(SERVER)).is_exposed());
}

// covers UP-32
#[tokio::test]
async fn set_appearance_is_written_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one"])).unwrap();
    let core = start(dir.path(), false);
    let events = core.events();
    next_clusters(&events).await;
    core.send(Command::SetAppearance(flint_core::Appearance::Dark));
    let (settings, error) = tokio::time::timeout(
        Duration::from_secs(5),
        settings_where(&events, |s, _| s.appearance == flint_core::Appearance::Dark),
    )
    .await
    .unwrap();
    assert_eq!(error, None);
    assert_eq!(settings.appearance, flint_core::Appearance::Dark);
    let on_disk = flint_core::settings::load_settings(&dir.path().join("config")).unwrap();
    assert_eq!(on_disk.appearance, flint_core::Appearance::Dark);
}

// covers UP-07, UP-08
#[tokio::test]
async fn exposing_a_server_the_context_no_longer_has_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one"])).unwrap();
    let core = start(dir.path(), false);
    let events = core.events();
    next_clusters(&events).await;
    core.send(Command::SetAiExposed {
        cluster: "one".into(),
        server: Some("https://somewhere-else".into()),
        scope: Default::default(),
    });
    let message = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Event::Error(message) = events.recv().await.unwrap() {
                return message;
            }
        }
    })
    .await
    .unwrap();
    assert!(message.contains("different server"), "{message}");
    let on_disk = flint_core::settings::load_settings(&dir.path().join("config")).unwrap();
    assert!(on_disk.ai_exposed_clusters.is_empty());
}

// covers UP-12
#[tokio::test]
async fn reloads_when_settings_change() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one"])).unwrap();
    std::fs::write(dir.path().join("extra"), kubeconfig(&["extra"])).unwrap();
    let core = start(dir.path(), true);
    let events = core.events();
    assert_eq!(next_clusters(&events).await, ["one"]);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Another writer (a second Flint, or the user) exposes "one" and adds a kubeconfig file.
    std::fs::write(
        config_dir.join("settings.toml"),
        format!(
            "kubeconfig_paths = [{:?}]\n[[ai_exposed_clusters]]\ncontext = \"one\"\nserver = \"{SERVER}\"\n",
            dir.path().join("extra")
        ),
    )
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        settings_where(&events, |s, _| {
            s.ai_exposure("one", Some(SERVER)).is_exposed()
        }),
    )
    .await
    .expect("settings reloaded");
    let clusters = tokio::time::timeout(Duration::from_secs(10), next_clusters(&events))
        .await
        .expect("kubeconfig reloaded with the new path");
    assert_eq!(clusters, ["one", "extra"]);

    // A broken file reports the error and exposes nothing.
    std::fs::write(config_dir.join("settings.toml"), "ai_exposed_clusters = [").unwrap();
    let (settings, error) = tokio::time::timeout(
        Duration::from_secs(10),
        settings_where(&events, |_, e| e.is_some()),
    )
    .await
    .expect("broken settings reported");
    assert!(settings.ai_exposed_clusters.is_empty());
    assert!(error.unwrap().contains("settings.toml"));
}

// covers UP-16: the activity log is read at start, and again whenever another process (`flint
// mcp`) appends to it or rotates it, so the app shows an agent's calls while it works.
#[tokio::test]
async fn reports_the_agent_activity_log_as_it_grows() {
    use flint_core::activity::{self, ActivityRecord, Outcome};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one"])).unwrap();
    let config = dir.path().join("config");
    let record = |n: i64| ActivityRecord {
        at_ms: n,
        tool: "list_resources".into(),
        cluster: Some("one".into()),
        server: Some(SERVER.into()),
        kind: Some("pods".into()),
        namespace: None,
        name: None,
        container: None,
        items: Some(3),
        bytes: 400,
        truncated: false,
        outcome: Outcome::Ok,
        cluster_key: Some(activity::cluster_key("one")),
        server_key: Some(activity::server_key(SERVER)),
    };
    activity::append(&config, &record(1)).unwrap();
    let core = start(dir.path(), true);
    let events = core.events();
    let next = || async {
        loop {
            if let Event::AgentActivity(records) = events.recv().await.unwrap() {
                return records;
            }
        }
    };
    let first = tokio::time::timeout(Duration::from_secs(5), next())
        .await
        .expect("the log is read at start");
    assert_eq!(*first, [record(1)]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    activity::append(&config, &record(2)).unwrap();
    // Other reloads may come first (the watcher starting); the one after the append has it.
    let grown = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let records = next().await;
            if records.len() > 1 {
                return records;
            }
        }
    })
    .await
    .expect("an append is noticed");
    assert_eq!(*grown, [record(1), record(2)]);
}

/// Rewrites kubeconfig `times` times (a cloud CLI in a loop), with `contexts` contexts.
async fn rewrite_kubeconfig(dir: &Path, times: usize, contexts: &[&str]) {
    for n in 0..times {
        let mut names: Vec<String> = contexts.iter().map(|c| (*c).to_owned()).collect();
        names.push(format!("burst-{}", n % 3));
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        std::fs::write(dir.join("kubeconfig"), kubeconfig(&names)).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The debounced reloads land.
    tokio::time::sleep(Duration::from_millis(800)).await;
}

// covers UP-06: a front end that holds the stream without reading it holds a few events,
// however many reloads the core runs: the latest of each kind.
#[tokio::test]
async fn events_nobody_reads_stay_bounded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one"])).unwrap();
    let core = start(dir.path(), true);
    let events = core.events();
    tokio::time::sleep(Duration::from_millis(500)).await;
    rewrite_kubeconfig(dir.path(), 50, &["one", "two"]).await;
    // Clusters, settings, activity: one each, however many times they changed; and at most one
    // discovery reset per context seen (one, two, burst-0..2).
    assert!(events.len() <= 3 + 5, "{} events queued", events.len());
    let mut latest = None;
    while !events.is_empty() {
        if let Event::ClustersChanged(snapshot) = events.recv().await.unwrap() {
            latest = Some(snapshot);
        }
    }
    let names: Vec<_> = latest
        .expect("the cluster list is there")
        .clusters
        .iter()
        .map(|c| c.info.name.clone())
        .collect();
    assert!(
        names.starts_with(&["one".to_owned(), "two".to_owned()]),
        "{names:?}"
    );
}

// covers UP-06: a context re-pointed and pointed back (or removed and added back) before the
// front end reads starts over in the core; whatever lists the reader skipped, a reset of its
// discovery is queued, so no old server's kinds are kept.
#[tokio::test]
async fn a_context_that_starts_over_resets_its_discovery() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one", "two"])).unwrap();
    let core = start(dir.path(), true);
    let events = core.events();
    next_clusters(&events).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let elsewhere = kubeconfig(&["one", "two"]).replace("127.0.0.1:1", "127.0.0.1:2");
    for text in [elsewhere, kubeconfig(&["one"]), kubeconfig(&["one", "two"])] {
        std::fs::write(dir.path().join("kubeconfig"), text).unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    let mut read = Vec::new();
    while !events.is_empty() {
        read.push(match events.recv().await.unwrap() {
            Event::ClustersChanged(_) => "clusters".to_owned(),
            Event::DiscoveryChanged {
                cluster,
                discovery: None,
                error: None,
            } => format!("reset {cluster}"),
            other => format!("{other:?}").chars().take(20).collect(),
        });
    }
    // Each reset is the last word on its cluster's discovery (nothing of the old slot follows).
    for reset in ["reset one", "reset two"] {
        assert!(read.iter().any(|e| e == reset), "{read:?}");
    }
    assert!(read.iter().any(|e| e == "clusters"), "{read:?}");
}

// covers UP-06: once the front end drops its stream, the core stops queueing events for it;
// a new reader starts from what comes next.
#[tokio::test]
async fn dropping_the_reader_stops_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one"])).unwrap();
    let core = start(dir.path(), true);
    let events = core.events();
    next_clusters(&events).await;
    drop(events);
    rewrite_kubeconfig(dir.path(), 10, &["one"]).await;
    let again = core.events();
    assert_eq!(again.len(), 0, "nothing was queued while nobody read");
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one", "later"])).unwrap();
    let names = tokio::time::timeout(Duration::from_secs(10), next_clusters(&again))
        .await
        .expect("events flow again");
    assert_eq!(names, ["one", "later"]);
}

// covers UP-04
#[tokio::test]
async fn unreachable_cluster_fails_with_a_readable_error() {
    // 127.0.0.1:1 refuses connections at once; nothing leaves the machine.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig(&["one", "two"])).unwrap();
    let core = start(dir.path(), false);
    let events = core.events();
    next_clusters(&events).await;
    core.send(Command::Activate("one".into()));
    let message = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Event::ConnectionChanged {
                state: ConnectionState::Failed(message),
                ..
            } = events.recv().await.unwrap()
            {
                return message;
            }
        }
    })
    .await
    .expect("the attempt fails well within the timeout");
    assert!(
        message.as_str().to_lowercase().contains("connect"),
        "{message}"
    );
    assert!(!message.as_str().contains("Status {"), "{message}");
    // The core still answers: another cluster can be selected.
    core.send(Command::Activate("two".into()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Event::ActiveChanged(Some(id)) = events.recv().await.unwrap()
                && id == "two"
            {
                return;
            }
        }
    })
    .await
    .unwrap();
}

/// Every file under `dir` with its contents, to compare before and after.
fn snapshot_files(dir: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            files.push((path.clone(), std::fs::read(&path).unwrap()));
        }
    }
    files.sort();
    files
}

// covers UP-90
#[tokio::test]
async fn commands_never_write_kubeconfig_files() {
    let dir = tempfile::tempdir().unwrap();
    let kube_dir = dir.path().join("kube");
    std::fs::create_dir_all(&kube_dir).unwrap();
    std::fs::write(kube_dir.join("config"), kubeconfig(&["one", "two"])).unwrap();
    let before = snapshot_files(&kube_dir);
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().join("config"),
        settings: Default::default(),
        settings_error: None,
        kubeconfig_base: vec![kube_dir.join("config")],
        home: None,
        watch_files: true,
        watch_timing: Default::default(),
    })
    .unwrap();
    let events = core.events();
    tokio::time::timeout(Duration::from_secs(5), next_clusters(&events))
        .await
        .unwrap();

    // Everything that writes something: exposure, watch modes, namespace filters, a reload,
    // and an activation (which fails to connect; the server is unreachable).
    core.send(Command::SetAiExposed {
        cluster: "one".into(),
        server: Some(SERVER.into()),
        scope: Default::default(),
    });
    core.send(Command::SetWatchMode {
        cluster: Some("one".into()),
        mode: Some(flint_core::WatchMode::OnView),
    });
    core.send(Command::SetNamespaceFilter {
        cluster: "one".into(),
        namespaces: vec!["demo".into()],
    });
    core.send(Command::ReloadKubeconfig);
    core.send(Command::Activate("two".into()));
    settings_where(&events, |s, _| {
        s.watch_mode_for("one") == flint_core::WatchMode::OnView
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(snapshot_files(&kube_dir), before, "kubeconfig untouched");
    // The writes went to Flint's own config directory.
    assert!(dir.path().join("config/settings.toml").exists());
}
