//! Cached clients and discovery results are released when their context is removed, revoked,
//! paused or left idle, without weakening the pause on a changed definition.

use std::time::{Duration, Instant};

use serde_json::json;

use super::confirmed_tests::{
    assert_every_tool_waits, assert_reachable, config, fake_api, sent, two_files,
};
use super::{FlintMcp, SLOT_IDLE};

const NO_CONTEXTS: &str = "apiVersion: v1\nkind: Config\n";

/// Fills the client and discovery slots for `dev`. The fake API has no discovery routes, so the
/// discovery slot stays empty, which is enough to show it is stored and released.
async fn warm(server: &FlintMcp) {
    assert_reachable(server).await;
    let params = serde_json::from_value(json!({"cluster": "dev"})).unwrap();
    let _ = server.do_list_kinds(params).await;
    assert!(holds(server));
    assert!(server.inner.discovery.holds("dev"));
}

fn holds(server: &FlintMcp) -> bool {
    server.inner.clients.holds("dev")
}

fn assert_released(server: &FlintMcp) {
    assert!(!holds(server), "the client slot was released");
    assert!(
        !server.inner.discovery.holds("dev"),
        "the discovery slot was released"
    );
}

fn expire_idle(server: &FlintMcp) {
    let later = Instant::now() + SLOT_IDLE;
    server.inner.clients.expire(later);
    server.inner.discovery.expire(later);
}

#[tokio::test]
async fn removing_a_context_releases_its_slots() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    warm(&server).await;

    std::fs::write(&first, NO_CONTEXTS).unwrap();
    server.context().await.unwrap();
    assert_released(&server);
}

#[tokio::test]
async fn revoking_exposure_releases_its_slots() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, _first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    warm(&server).await;

    std::fs::write(dir.path().join("settings.toml"), "").unwrap();
    server.context().await.unwrap();
    assert_released(&server);
}

#[tokio::test]
async fn unreadable_settings_release_the_slots() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, _first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    warm(&server).await;

    std::fs::write(dir.path().join("settings.toml"), "not [valid").unwrap();
    assert!(server.context().await.is_err());
    assert_released(&server);
}

#[tokio::test]
async fn an_idle_context_is_released_and_rebuilt_on_use() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, _first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    warm(&server).await;

    expire_idle(&server);
    assert_released(&server);
    let before = sent(&requests, "firsttoken");
    assert_reachable(&server).await;
    assert!(holds(&server));
    assert!(sent(&requests, "firsttoken") > before);
}

#[tokio::test]
async fn a_call_in_flight_cannot_reinsert_a_revoked_context() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, _first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    let in_flight = server.context().await.unwrap();

    std::fs::write(dir.path().join("settings.toml"), "").unwrap();
    server.context().await.unwrap();
    assert!(server.client(&in_flight, "dev").await.is_err());
    assert_released(&server);
    assert_eq!(requests.lock().unwrap().len(), 0, "nothing was sent");
}

// Reads can overlap: the newest view may predate the revocation, so the slot is stored, and the
// check made after storing it has to take it back out.
#[tokio::test]
async fn a_slot_stored_from_an_overlapping_read_is_taken_back() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, _first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    let in_flight = server.context().await.unwrap();

    // Revoked with no read since, so the newest view still shows the context as live.
    std::fs::write(dir.path().join("settings.toml"), "").unwrap();
    assert!(server.client(&in_flight, "dev").await.is_err());
    assert_released(&server);
    assert_eq!(requests.lock().unwrap().len(), 0, "nothing was sent");
}

#[tokio::test]
async fn a_call_in_flight_cannot_reinsert_a_removed_context() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    let in_flight = server.context().await.unwrap();

    std::fs::write(&first, NO_CONTEXTS).unwrap();
    server.context().await.unwrap();
    assert!(server.client(&in_flight, "dev").await.is_err());
    assert_released(&server);
    assert_eq!(requests.lock().unwrap().len(), 0, "nothing was sent");
}

#[tokio::test]
async fn a_call_cancelled_during_its_check_stores_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, _first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    let ctx = server.context().await.unwrap();

    // Polls the call once, up to its check, and drops it there.
    tokio::select! {
        biased;
        _ = server.client(&ctx, "dev") => panic!("the call was still checking"),
        () = std::future::ready(()) => {}
    }
    // The check it left behind has finished, and still stores nothing.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!holds(&server));
}

#[tokio::test]
async fn a_refused_send_check_releases_the_slots() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, _first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    warm(&server).await;
    let ctx = server.context().await.unwrap();
    let conn = server.client(&ctx, "dev").await.unwrap();

    std::fs::write(dir.path().join("settings.toml"), "").unwrap();
    assert!(!server.inner.still_connectable("dev", conn.fingerprint));
    assert_released(&server);
}

#[tokio::test]
async fn a_client_in_use_is_not_idle() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, _first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    let ctx = server.context().await.unwrap();
    let conn = server.client(&ctx, "dev").await.unwrap();

    expire_idle(&server);
    assert!(holds(&server), "a call still holds this client");
    drop(conn);
    expire_idle(&server);
    assert!(!holds(&server));
}

#[tokio::test]
async fn a_paused_context_stays_paused_after_its_slot_is_released() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let (one, two) = (
        config(&url, "    token: firsttoken\n"),
        config(&url, "    token: secondtoken\n"),
    );
    let (server, first) = two_files(dir.path(), &url, &one, &two);
    assert_reachable(&server).await;
    let reached = requests.lock().unwrap().len();

    std::fs::write(&first, "clusters: [not valid").unwrap();
    server.context().await.unwrap();
    assert!(!holds(&server), "a paused context keeps no client");
    assert_every_tool_waits(&server, "secondtoken", dir.path()).await;
    assert!(!holds(&server));
    assert_eq!(requests.lock().unwrap().len(), reached, "nothing was sent");

    std::fs::write(&first, &one).unwrap();
    assert_reachable(&server).await;
    assert_eq!(sent(&requests, "secondtoken"), 0);
}

#[tokio::test]
async fn idle_eviction_does_not_forget_the_confirmed_definition() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let (one, two) = (
        config(&url, "    token: firsttoken\n"),
        config(&url, "    token: secondtoken\n"),
    );
    let (server, first) = two_files(dir.path(), &url, &one, &two);
    assert_reachable(&server).await;
    let reached = requests.lock().unwrap().len();

    expire_idle(&server);
    assert!(!holds(&server));
    std::fs::write(&first, "clusters: [not valid").unwrap();
    assert_every_tool_waits(&server, "secondtoken", dir.path()).await;
    assert_eq!(requests.lock().unwrap().len(), reached, "nothing was sent");
    assert_eq!(sent(&requests, "secondtoken"), 0);
}

/// Set for the child process [`a_saved_proxy_change_rebuilds_the_clients_under_it`] runs in.
const PROXY_CHILD: &str = "FLINT_TEST_MCP_PROXY_CHILD";

// covers UP-125: a running server puts a proxy saved in settings.toml since it started in use as
// a fresh start would: the context's route is checked again, exposure given on the old route no
// longer holds, and the client is built again on the new route. Saving no proxy again goes back.
// In a child process, since the setting in use is the process's.
#[test]
fn a_saved_proxy_change_rebuilds_the_clients_under_it() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "server::slots_tests::saved_proxy_change_child",
            "--exact",
            "--nocapture",
        ])
        .env(PROXY_CHILD, "1")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .status()
        .unwrap();
    assert!(status.success(), "the child process failed");
}

#[tokio::test]
async fn saved_proxy_change_child() {
    if std::env::var_os(PROXY_CHILD).is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = fake_api();
    let one = config(&url, "    token: firsttoken\n");
    let (server, first) = two_files(dir.path(), &url, &one, NO_CONTEXTS);
    let settings = dir.path().join("settings.toml");
    let direct = std::fs::read_to_string(&settings).unwrap();
    warm(&server).await;
    assert_eq!(flint_core::settings::proxy_generation(), 0);
    let reached = requests.lock().unwrap().len();

    // Nothing listens there: a connection through it could not reach the server.
    let proxy = "[proxy]\nurl = \"http://127.0.0.1:9\"\n";
    std::fs::write(&settings, format!("{direct}{proxy}")).unwrap();
    server.context().await.unwrap();
    assert_eq!(flint_core::settings::proxy_generation(), 1);
    assert_released(&server);

    // Exposed again on the route the proxy gives it.
    let loaded = flint_core::kubeconfig::load(&[first, dir.path().join("second")]);
    let route = loaded.context("dev").unwrap().grant_server().unwrap();
    assert!(route.ends_with(" via http://127.0.0.1:9"), "{route}");
    let exposed = format!("[[ai_exposed_clusters]]\ncontext = \"dev\"\nserver = \"{route}\"\n");
    std::fs::write(&settings, format!("{exposed}{proxy}")).unwrap();
    let listed = server.do_list_clusters().await.unwrap();
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let row = &listed["clusters"][0];
    assert_eq!(row["reachable"], json!(false), "{listed}");
    let error = row["error"].as_str().unwrap();
    assert!(
        error.contains("plain HTTP"),
        "the new route was checked: {error}"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        reached,
        "nothing went direct"
    );

    std::fs::write(&settings, &direct).unwrap();
    assert_reachable(&server).await;
    assert_eq!(flint_core::settings::proxy_generation(), 2);
    assert!(requests.lock().unwrap().len() > reached, "direct again");
}
