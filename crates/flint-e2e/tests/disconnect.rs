//! Disconnecting a cluster, against fake API servers and a fake exec plugin: what ran for it
//! stops, and nothing reaches its server again until it is selected.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flint_core::events::Events;
use flint_core::handle::{Command, CoreHandle, Event};
use flint_core::keep_watching::KeepWatching;
use flint_core::logs::{LogOptions, LogState, LogTarget};
use flint_core::{ConnectionState, KindId, Settings};
use flint_e2e::fake_api::{FakeApiServer, FakePod, Gvr, write_kubeconfigs};

#[allow(dead_code)]
mod core_support;

use core_support::{Core, WAIT};

/// Long enough for a retry, a member tick or a log reconnect to have reached the server.
const QUIET: Duration = Duration::from_millis(1500);

fn pods() -> Gvr {
    Gvr::new("", "v1", "pods")
}

fn pod_kind() -> KindId {
    KindId::pods()
}

async fn two_clusters() -> (FakeApiServer, FakeApiServer, Core, PathBuf) {
    let a = FakeApiServer::start().await.unwrap();
    let b = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kubeconfig");
    write_kubeconfigs(&path, &[(&a, "a", None), (&b, "b", None)]).unwrap();
    let core = Core::start_on_context(dir, vec![path.clone()], Settings::default(), "a").await;
    (a, b, core, path)
}

/// Waits for `cluster` to be reported disconnected.
async fn until_disconnected(events: &Events, cluster: &str) {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Event::ConnectionChanged {
                cluster: name,
                state: ConnectionState::Disconnected,
            } = events.recv().await.expect("core running")
                && name == cluster
            {
                return;
            }
        }
    })
    .await
    .expect("the cluster should be reported disconnected");
}

async fn until_no_watches(server: &FakeApiServer) {
    tokio::time::timeout(WAIT, async {
        while server.open_watches(&pods()) > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the watch should have stopped");
}

/// Asserts `server` gets no request for [`QUIET`], a reload included.
async fn stays_quiet(core: &CoreHandle, server: &FakeApiServer) {
    let before = server.requests().len();
    core.reload_kubeconfig().await.unwrap();
    tokio::time::sleep(QUIET).await;
    assert_eq!(server.requests().len(), before, "the server was reached");
}

#[tokio::test]
async fn the_selected_cluster_stops_and_connects_only_when_selected() {
    let (a, _b, core, _path) = two_clusters().await;
    a.add_pod(FakePod::new("default", "talker", &["app"]));
    let _view = core.handle.subscribe("a", &pod_kind());
    a.wait_for_watches(&pods(), 1, WAIT).await.unwrap();
    let target = LogTarget {
        namespace: "default".into(),
        pod: "talker".into(),
        container: None,
    };
    let mut logs = core.handle.open_logs("a", target, LogOptions::default());
    a.wait_for_log_streams(1, WAIT).await.unwrap();

    let events = core.handle.events();
    core.handle.send(Command::DisconnectCluster("a".to_owned()));

    until_disconnected(&events, "a").await;
    until_no_watches(&a).await;
    let ended = tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = logs.latest();
            if let LogState::Ended { error, .. } = &snapshot.state {
                return error.clone();
            }
            assert!(logs.changed().await);
        }
    })
    .await
    .expect("the log session should end");
    assert!(ended.unwrap().contains("disconnected"));
    a.wait_for_log_streams(0, WAIT).await.unwrap();
    stays_quiet(&core.handle, &a).await;

    core.handle.send(Command::Activate("a".to_owned()));
    a.wait_for_watches(&pods(), 1, WAIT).await.unwrap();
}

#[tokio::test]
async fn a_set_member_leaves_while_the_others_go_on() {
    let (a, b, core, _path) = two_clusters().await;
    let _on_a = core.handle.subscribe("a", &pod_kind());
    let _on_b = core.handle.subscribe("b", &pod_kind());
    core.handle
        .send(Command::SetRunningSet(vec!["a".to_owned(), "b".to_owned()]));
    a.wait_for_watches(&pods(), 1, WAIT).await.unwrap();
    b.wait_for_watches(&pods(), 1, WAIT).await.unwrap();

    core.handle.send(Command::DisconnectCluster("b".to_owned()));

    until_no_watches(&b).await;
    let members = core.handle.running_set().await;
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].cluster, "a");
    assert_eq!(a.open_watches(&pods()), 1, "the other member goes on");
    stays_quiet(&core.handle, &b).await;
}

fn plugin(dir: &Path) -> (PathBuf, PathBuf) {
    let runs = dir.join("plugin-runs");
    let script = dir.join("plugin.sh");
    let text = format!(
        "#!/bin/sh\ncat > /dev/null\necho run >> '{}'\nprintf \
         '{{\"apiVersion\":\"client.authentication.k8s.io/v1\",\"kind\":\"ExecCredential\",\
         \"status\":{{\"token\":\"t\"}}}}'\n",
        runs.display()
    );
    std::fs::write(&script, text).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, runs)
}

/// A kubeconfig with context `a`, and context `b` signing in through `script`.
fn config_with_plugin(a: &FakeApiServer, b: &FakeApiServer, script: &Path) -> String {
    let mut merged = serde_json::to_value(a.kubeconfig_named("a", Some("demo"))).unwrap();
    let mut other = serde_json::to_value(b.kubeconfig_named("b", Some("demo"))).unwrap();
    other["users"][0]["name"] = "b-user".into();
    other["users"][0]["user"] = serde_json::json!({ "exec": {
        "apiVersion": "client.authentication.k8s.io/v1",
        "command": script.to_str().unwrap(),
        "interactiveMode": "IfAvailable",
    }});
    other["clusters"][0]["name"] = "b-cluster".into();
    other["contexts"][0]["context"]["user"] = "b-user".into();
    other["contexts"][0]["context"]["cluster"] = "b-cluster".into();
    for key in ["users", "clusters", "contexts"] {
        let entry = other[key][0].clone();
        merged[key].as_array_mut().unwrap().push(entry);
    }
    merged.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_cluster_loses_its_consent_and_stays_off() {
    let a = FakeApiServer::start().await.unwrap();
    let b = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (script, runs) = plugin(dir.path());
    let path = dir.path().join("kubeconfig");
    std::fs::write(&path, config_with_plugin(&a, &b, &script)).unwrap();
    let core = Core::start_on_context(dir, vec![path], Settings::default(), "a").await;

    let view = core.handle.keep_watching().await["b"].clone();
    core.handle.send(Command::SetKeepWatching {
        cluster: "b".to_owned(),
        definition: view.definition,
    });
    tokio::time::timeout(WAIT, async {
        while core.handle.keep_watching().await["b"].status == KeepWatching::Off {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("keep watching should turn on");
    let shown = view.shown_sign_in.expect("a plugin definition");
    let offer = core.handle.offer_keep_watching("b", shown).await.unwrap();
    core.handle
        .send(Command::ApproveKeepWatching(offer.approve()));
    tokio::time::timeout(WAIT, async {
        while core.handle.telemetry("b").is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a feed should have started");

    core.handle.send(Command::DisconnectCluster("b".to_owned()));

    tokio::time::timeout(WAIT, async {
        while core.handle.telemetry("b").is_some()
            || core.handle.keep_watching().await["b"].status != KeepWatching::Off
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the feed and the toggle should stop");
    let after = core.handle.keep_watching().await["b"].clone();
    assert!(after.refusal.is_some(), "it can't be kept until selected");
    assert!(
        core.handle.offer_keep_watching("b", shown).await.is_err(),
        "no consent can be offered"
    );
    let ran = std::fs::read_to_string(&runs).unwrap().lines().count();
    stays_quiet(&core.handle, &b).await;
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap().lines().count(),
        ran,
        "the plugin ran again"
    );
}
