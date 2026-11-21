//! flint-core against the fake API server while the kubeconfig changes on disk.

use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::{KindId, Settings, SyncState, WatchMode, WatchTiming};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Gvr, write_kubeconfigs};

const WAIT: Duration = Duration::from_secs(30);

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
