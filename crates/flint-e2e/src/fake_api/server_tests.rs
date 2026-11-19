//! The fake server over real HTTPS, through kube's client as Flint uses it.

use std::time::{Duration, Instant};

use futures::{AsyncBufReadExt as _, TryStreamExt as _};
use serde_json::{Value, json};

use super::*;

const TABLE: &str = "application/json;as=Table;v=v1;g=meta.k8s.io, application/json";

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

#[tokio::test]
async fn serves_version_discovery_and_openapi() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
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
    assert_eq!(
        api_code(
            fetch(&client, "/api", "application/json")
                .await
                .unwrap_err()
        ),
        404
    );
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
async fn serves_tables_lists_and_objects() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
    let table = fetch(
        &client,
        "/apis/apps/v1/namespaces/demo/deployments?includeObject=Metadata",
        TABLE,
    )
    .await
    .unwrap();
    assert_eq!(table["kind"], "Table");
    let names: Vec<&str> = table["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["object"]["metadata"]["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"web") && names.contains(&"podinfo"),
        "{names:?}"
    );

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

    for (path, code) in [
        ("/apis/apps/v1/namespaces/demo/deployments/nope", 404),
        ("/apis/nope.example.com/v1/things", 404),
        ("/api/v1/namespaces/demo/nodes", 404),
        ("/api/v1/namespaces/demo/pods/x/log", 404),
        ("/apis/metrics.k8s.io/v1beta1/pods?watch=1", 405),
        ("/healthz", 404),
    ] {
        let err = fetch(&client, path, "application/json").await.unwrap_err();
        assert_eq!(api_code(err), code, "{path}");
    }
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
async fn injects_forbidden_status_and_slow_responses() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
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
        Fault::slow(Duration::from_millis(400))
            .verb(Verb::Version)
            .times(1),
    );
    let started = Instant::now();
    client.apiserver_version().await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(400));
}

#[tokio::test]
async fn drops_connections() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
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
    let client = server.client().await.unwrap();
    let configmaps = Gvr::core("configmaps");
    let fresh = FakeRow::build(
        Some("demo"),
        "fresh",
        vec!["fresh".into(), 1.into(), "1d".into()],
    );
    server.script_watch(
        &configmaps,
        vec![
            WatchStep::Added(fresh.clone()),
            WatchStep::Wait(Duration::from_millis(10)),
            WatchStep::Modified(fresh.clone().with_cell(1, 2)),
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
    assert_eq!(events[1]["object"]["rows"][0]["cells"][1], 2);
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
    // The next watch has no script and stays open until its timeout.
    let started = Instant::now();
    let events = watch_lines(&client, "/api/v1/configmaps?watch=1&timeoutSeconds=1")
        .await
        .unwrap();
    assert!(events.is_empty() && started.elapsed() >= Duration::from_secs(1));
}

#[tokio::test]
async fn streaming_lists_send_initial_rows_then_an_end_marker() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
    let deployments = Gvr::new("apps", "v1", "deployments");
    server.script_watch(&deployments, vec![WatchStep::End]);
    let path = "/apis/apps/v1/namespaces/demo/deployments?watch=1&sendInitialEvents=true\
                &resourceVersionMatch=NotOlderThan&allowWatchBookmarks=true";
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
    let client = server.client().await.unwrap();
    let configmaps = Gvr::core("configmaps");
    let all = tokio::spawn({
        let client = client.clone();
        async move { watch_lines(&client, "/api/v1/configmaps?watch=1").await }
    });
    let other = tokio::spawn({
        let client = client.clone();
        async move { watch_lines(&client, "/api/v1/namespaces/other/configmaps?watch=1").await }
    });
    server
        .wait_for_watches(&configmaps, 2, Duration::from_secs(5))
        .await
        .unwrap();
    let row = FakeRow::build(
        Some("demo"),
        "live",
        vec!["live".into(), 0.into(), "1d".into()],
    );
    assert_eq!(
        server.send_watch(&configmaps, WatchStep::Added(row.clone())),
        2
    );
    assert!(server.row(&configmaps, Some("demo"), "live").is_some());
    assert_eq!(
        server.send_watch(&configmaps, WatchStep::Deleted(row.clone())),
        2
    );
    assert_eq!(
        server.send_watch(&configmaps, WatchStep::Deleted(row)),
        0,
        "already gone"
    );
    server.send_watch(&configmaps, WatchStep::End);
    let all = all.await.unwrap().unwrap();
    assert_eq!(types(&all), ["ADDED", "DELETED"]);
    assert!(
        other.await.unwrap().unwrap().is_empty(),
        "other namespace saw nothing"
    );
    assert_eq!(server.open_watches(&configmaps), 0);

    let broken = tokio::spawn({
        let client = client.clone();
        async move { watch_lines(&client, "/api/v1/configmaps?watch=1").await }
    });
    server
        .wait_for_watches(&configmaps, 1, Duration::from_secs(5))
        .await
        .unwrap();
    server.send_watch(&configmaps, WatchStep::Drop);
    assert!(
        broken.await.unwrap().is_err(),
        "a dropped watch is an error, not an end"
    );
}

#[tokio::test]
async fn silent_changes_show_in_the_next_list_only() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
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
    // The removed object is gone from plain reads too.
    let gone = fetch(
        &client,
        "/apis/apps/v1/namespaces/demo/deployments/podinfo",
        "application/json",
    );
    assert_eq!(api_code(gone.await.unwrap_err()), 404);
    let record = server
        .wait_for_request(Duration::from_secs(1), |r| {
            r.is_about(&deployments) && r.verb == Verb::List
        })
        .await
        .unwrap();
    assert_eq!(record.parsed_query(), Query::default());
    let _ = server.kubeconfig(Some("demo"));
    assert!(server.url().starts_with("https://127.0.0.1:"));
    assert!(server.ca_pem().contains("BEGIN CERTIFICATE"));
    assert_eq!(server.addr().ip().to_string(), "127.0.0.1");
    assert!(server.fixtures().tables.contains_key(&deployments));
}
