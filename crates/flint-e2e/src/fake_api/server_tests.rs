//! The fake server over real HTTPS, through kube's client as Flint uses it.

use std::time::{Duration, Instant};

use futures::{AsyncBufReadExt as _, TryStreamExt as _};
use serde_json::{Value, json};

use super::*;
use crate::fake_api::clock;

const TABLE: &str = "application/json;as=Table;v=v1;g=meta.k8s.io, application/json";
const SHORT: Duration = Duration::from_secs(5);

fn get(path: &str, accept: &str) -> http::Request<Vec<u8>> {
    http::Request::get(path)
        .header(http::header::ACCEPT, accept)
        .body(Vec::new())
        .unwrap()
}

async fn fetch(client: &kube::Client, path: &str, accept: &str) -> Result<Value, kube::Error> {
    client.request::<Value>(get(path, accept)).await
}

fn api_code(err: kube::Error) -> u16 {
    match err {
        kube::Error::Api(status) => status.code,
        other => panic!("not an API error: {other}"),
    }
}

/// Reads a watch until the stream ends, one JSON value per line.
async fn watch_lines(client: &kube::Client, path: &str) -> Result<Vec<Value>, std::io::Error> {
    let body = client
        .request_stream(get(path, TABLE))
        .await
        .map_err(std::io::Error::other)?;
    Box::pin(body)
        .lines()
        .map_ok(|l| serde_json::from_str::<Value>(&l).unwrap())
        .try_collect()
        .await
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

fn names(table: &Value) -> Vec<&str> {
    table["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["object"]["metadata"]["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn serves_version_discovery_and_openapi() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let version = client.apiserver_version().await.unwrap();
    assert_eq!(version.major, "1");
    let groups = client.list_api_groups_aggregated().await.unwrap();
    assert!(
        groups
            .items
            .iter()
            .any(|g| g.metadata.as_ref().and_then(|m| m.name.as_deref()) == Some("apps"))
    );
    let core = client.list_core_api_versions_aggregated().await.unwrap();
    assert_eq!(core.items.len(), 1);
    // Legacy discovery is not recorded.
    let legacy = fetch(&client, "/api", "application/json").await;
    assert_eq!(api_code(legacy.unwrap_err()), 404);
    let index = fetch(&client, "/openapi/v3", "application/json")
        .await
        .unwrap();
    let url = index["paths"]["apis/apps/v1"]["serverRelativeURL"]
        .as_str()
        .unwrap();
    let doc = fetch(&client, url, "application/json").await.unwrap();
    assert!(doc["components"]["schemas"].as_object().unwrap().len() > 10);
    let missing = fetch(&client, "/openapi/v3/api/v1", "application/json").await;
    assert_eq!(api_code(missing.unwrap_err()), 404);
}

#[tokio::test]
async fn serves_tables_lists_objects_and_logs() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let path = "/apis/apps/v1/namespaces/demo/deployments?includeObject=Metadata";
    let table = fetch(&client, path, TABLE).await.unwrap();
    assert_eq!(table["kind"], "Table");
    let found = names(&table);
    assert!(
        found.contains(&"web") && found.contains(&"podinfo"),
        "{found:?}"
    );
    // Timestamps are in the server's time, and Age is rendered from them.
    let created = table["rows"][0]["object"]["metadata"]["creationTimestamp"]
        .as_str()
        .unwrap();
    let (created, _) = clock::parse(created).unwrap();
    let now = clock::unix(SystemTime::now());
    assert!(
        created <= now && created > now - 400 * 86_400,
        "{created} vs {now}"
    );
    let age = server.column(&server.gvr("deployments"), "Age").unwrap();
    assert_ne!(table["rows"][0]["cells"][age], "1d");

    let list = fetch(
        &client,
        "/apis/apps/v1/namespaces/demo/deployments",
        "application/json",
    )
    .await
    .unwrap();
    assert_eq!(list["kind"], "DeploymentList");
    let web = fetch(
        &client,
        "/apis/apps/v1/namespaces/demo/deployments/web",
        "application/json",
    )
    .await
    .unwrap();
    assert_eq!(web["kind"], "Deployment");
    assert_eq!(web["spec"]["replicas"], 2);
    let one = fetch(
        &client,
        "/apis/apps/v1/namespaces/demo/deployments/web",
        TABLE,
    )
    .await
    .unwrap();
    assert_eq!(one["rows"][0]["cells"][0], "web");
    let namespace = fetch(&client, "/api/v1/namespaces/demo", "application/json")
        .await
        .unwrap();
    assert_eq!(namespace["kind"], "Namespace");

    let pod = "web-69c6f74b8b-qv4xv";
    let log_path = format!("/api/v1/namespaces/demo/pods/{pod}/log");
    let log = client.request_text(get(&log_path, "*/*")).await.unwrap();
    assert_eq!(log.lines().count(), 3);
    server.set_log("demo", pod, "one\ntwo\nthree\n");
    let tail = client
        .request_text(get(&format!("{log_path}?tailLines=1"), "*/*"))
        .await
        .unwrap();
    assert_eq!(tail, "three\n");

    for (path, code) in [
        ("/apis/apps/v1/namespaces/demo/deployments/nope", 404),
        ("/apis/nope.example.com/v1/things", 404),
        ("/api/v1/namespaces/demo/nodes", 404),
        ("/api/v1/namespaces/demo/pods/nope/log", 404),
        ("/api/v1/namespaces/demo/pods/broken-image/log", 400),
        ("/api/v1/namespaces/demo/pods/x/exec", 404),
        ("/apis/metrics.k8s.io/v1beta1/pods?watch=1", 405),
        ("/api/v1/pods?sendInitialEvents=true", 422),
        ("/healthz", 404),
    ] {
        let err = fetch(&client, path, "application/json").await.unwrap_err();
        assert_eq!(api_code(err), code, "{path}");
    }
    let err = fetch(
        &client,
        "/api/v1/pods?fieldSelector=status.phase%3DRunning",
        TABLE,
    )
    .await;
    assert_eq!(api_code(err.unwrap_err()), 400);
    let post = http::Request::post("/api/v1/namespaces/demo/configmaps")
        .body(b"{}".to_vec())
        .unwrap();
    assert_eq!(
        api_code(client.request::<Value>(post).await.unwrap_err()),
        405
    );
    let log = server.requests();
    assert!(
        log.iter()
            .any(|r| r.verb == Verb::List && r.status == Some(200))
    );
    assert!(
        log.iter()
            .any(|r| r.method == http::Method::POST && r.status == Some(405))
    );
}

#[tokio::test]
async fn objects_follow_rows_and_unrecorded_objects_are_made_up() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let pods = server.gvr("pods");
    let system = server
        .rows(&pods)
        .into_iter()
        .find(|r| r.namespace() == Some("kube-system"))
        .unwrap();
    let path = format!("/api/v1/namespaces/kube-system/pods/{}", system.name());
    let pod = fetch(&client, &path, "application/json").await.unwrap();
    assert_eq!(
        (pod["kind"].as_str(), pod["metadata"]["uid"].as_str()),
        (Some("Pod"), Some(system.uid()))
    );

    let deployments = server.gvr("deployments");
    let web = server.row(&deployments, Some("demo"), "web").unwrap();
    server.set_row(&deployments, web.with_label("tier", "front"));
    let path = "/apis/apps/v1/namespaces/demo/deployments/web";
    let object = fetch(&client, path, "application/json").await.unwrap();
    assert_eq!(object["metadata"]["labels"]["tier"], "front");
    assert!(object["spec"].is_object());

    let secrets = server.gvr("secrets");
    server.set_object(
        &secrets,
        json!({"metadata": {"name": "applied", "namespace": "demo", "annotations": {
            "kubectl.kubernetes.io/last-applied-configuration": "{\"data\":{\"p\":\"eA==\"}}"}},
            "type": "Opaque", "data": {"p": "eA=="}}),
    );
    let secret = fetch(
        &client,
        "/api/v1/namespaces/demo/secrets/applied",
        "application/json",
    )
    .await
    .unwrap();
    assert_eq!(secret["data"]["p"], "eA==");
    let table = fetch(&client, "/api/v1/namespaces/demo/secrets", TABLE)
        .await
        .unwrap();
    assert!(names(&table).contains(&"applied"));
    assert!(server.remove_object(&secrets, Some("demo"), "applied"));
    let gone = fetch(
        &client,
        "/api/v1/namespaces/demo/secrets/applied",
        "application/json",
    );
    assert_eq!(api_code(gone.await.unwrap_err()), 404);
}

#[tokio::test]
async fn quirks_plain_only_resources_and_metrics() {
    let configmaps = Gvr::core("configmaps");
    let server = FakeApiServer::start_with(Options {
        plain_only: vec![configmaps.clone()],
        ..Options::default()
    })
    .await
    .unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let list = fetch(&client, "/api/v1/configmaps", TABLE).await.unwrap();
    assert_eq!(list["kind"], "ConfigMapList");
    server.script_watch(&configmaps, vec![WatchStep::End]);
    let events = watch_lines(&client, "/api/v1/namespaces/demo/configmaps?watch=1")
        .await
        .unwrap();
    assert!(!events.is_empty());
    assert_eq!(events[0]["object"]["kind"], "ConfigMap");

    let metrics = fetch(&client, "/apis/metrics.k8s.io/v1beta1/pods?limit=1", TABLE)
        .await
        .unwrap();
    assert_eq!(metrics["metadata"], json!({}));
    assert!(metrics["rows"].as_array().unwrap().len() > 1);

    let bad = Options {
        plain_only: vec![Gvr::core("nope")],
        ..Options::default()
    };
    assert!(FakeApiServer::start_with(bad).await.is_err());
}

#[tokio::test]
async fn injects_forbidden_status_and_slow_responses() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let secrets = Gvr::core("secrets");
    server.inject(
        Fault::forbidden()
            .on(&secrets)
            .verb(Verb::List)
            .cluster_wide(),
    );
    server.inject(Fault::status(500).on(&Gvr::core("nodes")).times(1));
    let err = fetch(&client, "/api/v1/secrets", TABLE).await.unwrap_err();
    let kube::Error::Api(status) = err else {
        panic!("not an API error")
    };
    assert_eq!((status.code, status.reason.as_str()), (403, "Forbidden"));
    assert!(
        status.message.contains("cannot list resource \"secrets\""),
        "{}",
        status.message
    );
    // Only the cluster-wide list is refused.
    fetch(&client, "/api/v1/namespaces/demo/secrets", TABLE)
        .await
        .unwrap();
    // A counted fault runs out.
    assert_eq!(
        api_code(fetch(&client, "/api/v1/nodes", TABLE).await.unwrap_err()),
        500
    );
    fetch(&client, "/api/v1/nodes", TABLE).await.unwrap();
    server.clear_faults();
    fetch(&client, "/api/v1/secrets", TABLE).await.unwrap();

    server.inject(
        Fault::delay(Duration::from_millis(50))
            .verb(Verb::Version)
            .times(1),
    );
    let started = Instant::now();
    client.apiserver_version().await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(50));
}

#[tokio::test]
async fn drops_connections() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    server.inject(Fault::drop_connection().verb(Verb::List).times(1));
    let err = fetch(&client, "/api/v1/pods", TABLE).await.unwrap_err();
    assert!(!matches!(err, kube::Error::Api(_)), "{err}");
    fetch(&client, "/api/v1/pods", TABLE).await.unwrap();
    let dropped = server
        .requests()
        .into_iter()
        .filter(|r| r.status.is_none())
        .count();
    assert_eq!(dropped, 1);
}

#[tokio::test]
async fn scripted_watches_send_every_event_type() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let configmaps = Gvr::core("configmaps");
    let fresh = server.new_row(&configmaps, Some("demo"), "fresh", &[("Data", "1")]);
    let data = server.column(&configmaps, "Data").unwrap();
    server.script_watch(
        &configmaps,
        vec![
            WatchStep::Added(fresh.clone()),
            WatchStep::Wait(Duration::from_millis(10)),
            WatchStep::Modified(fresh.clone().with_cell(data, 2)),
            WatchStep::Bookmark,
            WatchStep::Deleted(fresh.clone()),
            WatchStep::Expired,
            WatchStep::Added(fresh.renamed("never-sent")),
        ],
    );
    let events = watch_lines(
        &client,
        "/api/v1/namespaces/demo/configmaps?watch=1&resourceVersion=1000&allowWatchBookmarks=true",
    )
    .await
    .unwrap();
    assert_eq!(
        types(&events),
        ["ADDED", "MODIFIED", "BOOKMARK", "DELETED", "ERROR"]
    );
    // Columns only in the first event.
    assert!(events[0]["object"]["columnDefinitions"].is_array());
    assert!(events[1]["object"]["columnDefinitions"].is_null());
    assert_eq!(events[1]["object"]["rows"][0]["cells"][data], 2);
    let rvs: Vec<u64> = events[..4]
        .iter()
        .map(|e| {
            e["object"]["metadata"]["resourceVersion"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(rvs, [1001, 1002, 1002, 1003]);
    // A bookmark's row is fake: empty cells and the resource version.
    let bookmark = &events[2]["object"]["rows"][0];
    assert_eq!(bookmark["cells"][0], "");
    assert_eq!(bookmark["object"]["metadata"]["resourceVersion"], "1002");
    assert_eq!(
        events[4]["object"],
        json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure",
               "message": "too old resource version: 1000 (1003)", "reason": "Expired", "code": 410})
    );
    // The rows followed the script; the step after the 410 never ran.
    assert!(server.row(&configmaps, Some("demo"), "fresh").is_none());
    assert!(
        server
            .row(&configmaps, Some("demo"), "never-sent")
            .is_none()
    );

    // A watch resuming from 1000 replays the logged changes; one from before the first
    // version is too old.
    server.script_watch(&configmaps, vec![WatchStep::End]);
    let replay = watch_lines(&client, "/api/v1/configmaps?watch=1&resourceVersion=1000")
        .await
        .unwrap();
    assert_eq!(types(&replay), ["ADDED", "MODIFIED", "DELETED"]);
    let old = watch_lines(&client, "/api/v1/configmaps?watch=1&resourceVersion=5")
        .await
        .unwrap();
    assert_eq!(old[0]["object"]["code"], 410);
    let bad = fetch(
        &client,
        "/api/v1/configmaps?watch=1&resourceVersion=x",
        TABLE,
    )
    .await;
    assert_eq!(api_code(bad.unwrap_err()), 400);
}

#[tokio::test]
async fn a_watch_without_version_starts_with_the_current_state() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let deployments = server.gvr("deployments");
    server.script_watch_in(&deployments, "demo", vec![WatchStep::End]);
    server.script_watch(&deployments, vec![WatchStep::Drop]);
    let events = watch_lines(&client, "/apis/apps/v1/namespaces/demo/deployments?watch=1")
        .await
        .unwrap();
    let in_demo = server
        .rows(&deployments)
        .iter()
        .filter(|r| r.namespace() == Some("demo"))
        .count();
    assert_eq!(events.len(), in_demo);
    assert!(types(&events).iter().all(|t| *t == "ADDED"));
    // The any-namespace script is still queued.
    assert!(
        watch_lines(
            &client,
            "/apis/apps/v1/deployments?watch=1&resourceVersion=1000"
        )
        .await
        .is_err()
    );
    server.script_watch(&deployments, vec![WatchStep::End]);
    server.clear_scripts();
    assert!(lock(&server.inner.scripts).is_empty());
}

#[tokio::test]
async fn streaming_lists_send_initial_rows_then_an_end_marker() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let deployments = Gvr::new("apps", "v1", "deployments");
    let path = "/apis/apps/v1/namespaces/demo/deployments?watch=1&sendInitialEvents=true\
                &resourceVersionMatch=NotOlderThan&allowWatchBookmarks=true";
    server.script_watch(&deployments, vec![WatchStep::End]);
    let events = watch_lines(&client, path).await.unwrap();
    let in_demo = server
        .rows(&deployments)
        .iter()
        .filter(|r| r.namespace() == Some("demo"))
        .count();
    assert_eq!(events.len(), in_demo + 1);
    assert!(types(&events[..in_demo]).iter().all(|t| *t == "ADDED"));
    let end = &events[in_demo];
    assert_eq!(end["type"], "BOOKMARK");
    assert_eq!(
        end["object"]["rows"][0]["object"]["metadata"]["annotations"]["k8s.io/initial-events-end"],
        "true"
    );
    // Without bookmarks there is no end marker.
    server.script_watch(&deployments, vec![WatchStep::End]);
    let quiet = watch_lines(&client, &path.replace("&allowWatchBookmarks=true", ""))
        .await
        .unwrap();
    assert_eq!(quiet.len(), in_demo);
    // resourceVersionMatch=NotOlderThan is required.
    let err = fetch(
        &client,
        &path.replace("&resourceVersionMatch=NotOlderThan", ""),
        TABLE,
    )
    .await;
    assert_eq!(api_code(err.unwrap_err()), 422);

    let off = FakeApiServer::start_with(Options {
        streaming_lists: false,
        ..Options::default()
    })
    .await
    .unwrap();
    let client = off.client().await.unwrap();
    let err = fetch(&client, path, TABLE).await.unwrap_err();
    let kube::Error::Api(status) = err else {
        panic!("not an API error")
    };
    assert_eq!(status.code, 422);
    assert!(status.message.contains("sendInitialEvents"));
}

#[tokio::test]
async fn live_events_reach_matching_open_watches_and_drops_break_them() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let configmaps = Gvr::core("configmaps");
    let start = "resourceVersion=1000";
    let all = tokio::spawn({
        let client = client.clone();
        async move { watch_lines(&client, &format!("/api/v1/configmaps?watch=1&{start}")).await }
    });
    let other = tokio::spawn({
        let client = client.clone();
        async move {
            watch_lines(
                &client,
                &format!("/api/v1/namespaces/other/configmaps?watch=1&{start}"),
            )
            .await
        }
    });
    let labelled = tokio::spawn({
        let client = client.clone();
        async move {
            watch_lines(
                &client,
                &format!("/api/v1/configmaps?watch=1&{start}&labelSelector=app%3Dweb"),
            )
            .await
        }
    });
    server
        .wait_for_watches(&configmaps, 3, SHORT)
        .await
        .unwrap();
    let row = server
        .new_row(&configmaps, Some("demo"), "live", &[])
        .with_label("app", "web");
    assert_eq!(
        server.send_watch(&configmaps, WatchStep::Added(row.clone())),
        3
    );
    assert!(server.row(&configmaps, Some("demo"), "live").is_some());
    // Leaving the selector: the labelled watch sees a DELETED.
    let relabelled = row.clone().with_label("app", "db");
    server.send_watch(&configmaps, WatchStep::Modified(relabelled.clone()));
    assert_eq!(
        server.send_watch(&configmaps, WatchStep::Deleted(relabelled.clone())),
        3
    );
    assert_eq!(
        server.send_watch(&configmaps, WatchStep::Deleted(relabelled)),
        0,
        "already gone"
    );
    server.send_watch(&configmaps, WatchStep::End);
    let all = all.await.unwrap().unwrap();
    assert_eq!(types(&all), ["ADDED", "MODIFIED", "DELETED"]);
    assert!(
        other.await.unwrap().unwrap().is_empty(),
        "other namespace saw nothing"
    );
    assert_eq!(
        types(&labelled.await.unwrap().unwrap()),
        ["ADDED", "DELETED"]
    );
    assert_eq!(server.open_watches(&configmaps), 0);

    let broken = tokio::spawn({
        let client = client.clone();
        async move { watch_lines(&client, "/api/v1/configmaps?watch=1&resourceVersion=1004").await }
    });
    server
        .wait_for_watches(&configmaps, 1, SHORT)
        .await
        .unwrap();
    // It stays open until told otherwise.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.open_watches(&configmaps), 1);
    server.send_watch(&configmaps, WatchStep::Drop);
    assert!(
        broken.await.unwrap().is_err(),
        "a dropped watch is an error, not an end"
    );
}

#[tokio::test]
async fn silent_changes_show_in_the_next_list_only() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let deployments = Gvr::new("apps", "v1", "deployments");
    let ready = server.column(&deployments, "Ready").unwrap();
    let web = server.row(&deployments, Some("demo"), "web").unwrap();
    server.set_row(&deployments, web.with_cell(ready, "0/2"));
    assert!(server.remove_row(&deployments, Some("demo"), "podinfo"));
    assert!(!server.remove_row(&deployments, Some("demo"), "podinfo"));
    let table = fetch(&client, "/apis/apps/v1/namespaces/demo/deployments", TABLE)
        .await
        .unwrap();
    let rows = table["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["cells"][ready], "0/2");
    assert_eq!(table["metadata"]["resourceVersion"], "1002");
    // A watch from before the silent changes cannot replay them: 410.
    let old = watch_lines(
        &client,
        "/apis/apps/v1/deployments?watch=1&resourceVersion=1000",
    )
    .await
    .unwrap();
    assert_eq!(old[0]["object"]["code"], 410);
    let record = server
        .wait_for_request(SHORT, |r| r.is_about(&deployments) && r.verb == Verb::List)
        .await
        .unwrap();
    assert_eq!(record.parsed_query(), Query::default());
    let missing = server
        .wait_for_request(Duration::from_millis(20), |r| r.path == "/never")
        .await
        .unwrap_err();
    assert!(
        missing.to_string().contains("GET /apis/apps/v1"),
        "{missing}"
    );
    assert!(server.url().starts_with("https://127.0.0.1:"));
    assert!(server.ca_pem().contains("BEGIN CERTIFICATE"));
    assert_eq!(server.addr().ip().to_string(), "127.0.0.1");
    assert!(server.fixtures().tables.contains_key(&deployments));
}

#[tokio::test]
async fn unknown_resources_fail_loudly_with_a_hint() {
    let server = FakeApiServer::start().await.unwrap();
    assert_eq!(
        server.gvr("deployments"),
        Gvr::new("apps", "v1", "deployments")
    );
    assert_eq!(server.gvr("pods"), Gvr::core("pods"));
    let typo = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        server.script_watch(&Gvr::core("deployments"), vec![])
    }));
    let message = typo.unwrap_err();
    let message = message.downcast_ref::<String>().unwrap();
    assert!(
        message.contains("did you mean apps/v1/deployments?"),
        "{message}"
    );
    let plural =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.gvr("deployment")));
    let message = plural.unwrap_err();
    assert!(
        message
            .downcast_ref::<String>()
            .unwrap()
            .contains("\"deployments\"")
    );
    for call in [
        Box::new(|| server.inject(Fault::forbidden().on(&Gvr::core("nope")))) as Box<dyn Fn()>,
        Box::new(|| server.set_row(&Gvr::core("nope"), FakeRow::build(None, "x", vec![]))),
        Box::new(|| {
            server.send_watch(&Gvr::core("nope"), WatchStep::Bookmark);
        }),
        Box::new(|| {
            server.new_row(&server.gvr("pods"), None, "x", &[("Nope", "1")]);
        }),
    ] {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).is_err());
    }
    let nope = Gvr::core("nope");
    let waited = std::panic::AssertUnwindSafe(server.wait_for_watches(&nope, 1, SHORT));
    assert!(futures::FutureExt::catch_unwind(waited).await.is_err());
}

#[test]
fn several_servers_share_one_kubeconfig_and_run_without_a_runtime() {
    let a = FakeApiServer::start_blocking(Options::default()).unwrap();
    let b = FakeApiServer::start_blocking(Options::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    write_kubeconfigs(&path, &[(&a, "kind-a", None), (&b, "kind-b", Some("demo"))]).unwrap();
    let loaded = flint_core::kubeconfig::load(std::slice::from_ref(&path));
    assert_eq!(loaded.contexts.len(), 2);
    assert_eq!(
        loaded.context("kind-b").unwrap().server.as_deref(),
        Some(b.url().as_str())
    );
    // Both answer, on their own runtimes, to a client on a third.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        for (server, context) in [(&a, "kind-a"), (&b, "kind-b")] {
            let client = flint_core::cluster::connect(&loaded.merged, context)
                .await
                .unwrap();
            client.apiserver_version().await.unwrap();
            assert_eq!(server.requests().len(), 1);
        }
    });
}

// A replacement that omits them keeps what only the API server sets, so a terminating pod stays
// terminating; `generation` goes up only with a new `spec`.
#[test]
fn a_replacement_keeps_the_metadata_the_server_owns() {
    let current = json!({
        "metadata": {
            "name": "web", "namespace": "demo", "uid": "u1", "resourceVersion": "7",
            "creationTimestamp": "2026-01-01T00:00:00Z", "generation": 4,
            "deletionTimestamp": "2026-01-02T00:00:00Z", "deletionGracePeriodSeconds": 30,
            "labels": {"app": "web"},
        },
        "spec": {"replicas": 1},
    });
    let replaced = |spec: Value| {
        let mut object = json!({"metadata": {"name": "web", "generation": 99}, "spec": spec});
        keep_server_metadata(&mut object, &current, Some("demo"));
        object
    };

    let same = replaced(json!({"replicas": 1}));
    assert_eq!(same["metadata"]["generation"], 4);
    assert_eq!(same["metadata"]["namespace"], "demo");
    assert_eq!(same["metadata"].get("labels"), None);
    for field in SERVER_METADATA.into_iter().filter(|f| *f != "generation") {
        assert_eq!(
            same["metadata"][field], current["metadata"][field],
            "{field}"
        );
    }
    assert_eq!(
        replaced(json!({"replicas": 3}))["metadata"]["generation"],
        5
    );
}
