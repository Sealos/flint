//! Writes over real HTTPS: what is recorded, created, dry-run and refused.

use serde_json::{Value, json};

use super::*;

const CONFIGMAPS: &str = "/api/v1/namespaces/demo/configmaps";
const WEB: &str = "/apis/apps/v1/namespaces/demo/deployments/web";
const MERGE: &str = "application/merge-patch+json";
const JSON_PATCH: &str = "application/json-patch+json";

async fn send(
    client: &kube::Client,
    method: Method,
    path: &str,
    content_type: &str,
    body: &Value,
) -> Result<Value, kube::Error> {
    let request = http::Request::builder()
        .method(method)
        .uri(path)
        .header(http::header::CONTENT_TYPE, content_type)
        .body(serde_json::to_vec(body).unwrap())
        .unwrap();
    client.request::<Value>(request).await
}

fn code(result: Result<Value, kube::Error>) -> u16 {
    match result {
        Err(kube::Error::Api(status)) => status.code,
        other => panic!("not an API error: {other:?}"),
    }
}

fn configmap(name: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap",
           "metadata": {"name": name}, "data": {"key": "value"}})
}

async fn creating_server() -> FakeApiServer {
    FakeApiServer::start_with(Options {
        accept_creates: true,
        ..Options::default()
    })
    .await
    .unwrap()
}

fn version(server: &FakeApiServer) -> u64 {
    lock(&server.inner.state).resource_version()
}

#[tokio::test]
async fn every_write_is_recorded_with_its_exact_body_and_answer() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let patch = json!({"metadata": {"labels": {"tier": "front"}}});
    send(
        &client,
        Method::PATCH,
        &format!("{WEB}?fieldManager=flint"),
        MERGE,
        &patch,
    )
    .await
    .unwrap();
    let missing = format!("{CONFIGMAPS}/missing");
    let gone = send(
        &client,
        Method::DELETE,
        &missing,
        "application/json",
        &json!({}),
    )
    .await;
    assert_eq!(code(gone), 404);
    // Refused by the read-only default, and still what the client sent.
    let post = send(
        &client,
        Method::POST,
        CONFIGMAPS,
        "application/json",
        &configmap("a"),
    )
    .await;
    assert_eq!(code(post), 405);
    client
        .request::<Value>(http::Request::get(WEB).body(Vec::new()).unwrap())
        .await
        .unwrap();

    let writes = server.writes();
    assert_eq!(writes.len(), 3, "reads are not writes: {writes:?}");
    assert_eq!(writes[0].method, Method::PATCH);
    assert_eq!(writes[0].path, WEB);
    assert_eq!(writes[0].query, "fieldManager=flint");
    assert_eq!(writes[0].content_type, MERGE);
    assert_eq!(writes[0].json(), Some(patch));
    assert_eq!(writes[0].status, Some(200));
    assert!(writes[0].is_about(&Gvr::new("apps", "v1", "deployments")));
    assert_eq!(
        writes[0].parsed_query().field_manager.as_deref(),
        Some("flint")
    );
    assert_eq!(
        (&writes[1].method, writes[1].status),
        (&Method::DELETE, Some(404))
    );
    assert_eq!(
        (&writes[2].method, writes[2].status),
        (&Method::POST, Some(405))
    );
    assert_eq!(writes[2].json(), Some(configmap("a")));
}

#[tokio::test]
async fn a_dropped_or_delayed_write_is_recorded_unanswered() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    server.inject(Fault::drop_connection().verb(Verb::Other).times(1));
    let patch = json!({"metadata": {"labels": {"a": "b"}}});
    send(&client, Method::PATCH, WEB, MERGE, &patch)
        .await
        .unwrap_err();
    let writes = server.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!((writes[0].status, writes[0].json()), (None, Some(patch)));

    server.inject(
        Fault::delay(Duration::from_millis(300))
            .verb(Verb::Other)
            .times(1),
    );
    let slow = json!({"metadata": {"labels": {"c": "d"}}});
    let (answered, seen) = tokio::join!(send(&client, Method::PATCH, WEB, MERGE, &slow), async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(write) = server.writes().get(1).cloned() {
                    return write;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    });
    answered.unwrap();
    assert_eq!((seen.status, seen.json()), (None, Some(slow)));
    assert_eq!(server.writes()[1].status, Some(200));
}

#[tokio::test]
async fn post_creates_an_object_and_tells_watches() {
    let server = creating_server().await;
    let client = server.unrestricted_client().await.unwrap();
    let configmaps = Gvr::core("configmaps");
    let before = version(&server);
    let mut sent = configmap("made");
    sent["metadata"]["uid"] = "client-uid".into();
    sent["metadata"]["resourceVersion"] = "7".into();
    let created = send(&client, Method::POST, CONFIGMAPS, "application/json", &sent)
        .await
        .unwrap();
    assert_eq!(created["metadata"]["namespace"], "demo");
    assert_ne!(created["metadata"]["uid"], "client-uid");
    assert_ne!(created["metadata"]["resourceVersion"], "7");
    let stored = server.object(&configmaps, Some("demo"), "made").unwrap();
    assert_eq!(stored["data"]["key"], "value");
    assert!(server.row(&configmaps, Some("demo"), "made").is_some());
    let changes = lock(&server.inner.state)
        .changes_since(&configmaps, before)
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].ty, "ADDED");
    assert_eq!(server.writes()[0].status, Some(201));

    let again = send(&client, Method::POST, CONFIGMAPS, "application/json", &sent).await;
    assert_eq!(code(again), 409);
    let unnamed = json!({"metadata": {}, "data": {}});
    let refused = send(
        &client,
        Method::POST,
        CONFIGMAPS,
        "application/json",
        &unnamed,
    )
    .await;
    assert_eq!(code(refused), 422);
    let elsewhere = json!({"metadata": {"name": "x", "namespace": "other"}});
    let refused = send(
        &client,
        Method::POST,
        CONFIGMAPS,
        "application/json",
        &elsewhere,
    )
    .await;
    assert_eq!(code(refused), 400);
    let generated = json!({"metadata": {"generateName": "made-"}, "data": {}});
    let made = send(
        &client,
        Method::POST,
        CONFIGMAPS,
        "application/json",
        &generated,
    )
    .await
    .unwrap();
    let name = made["metadata"]["name"].as_str().unwrap();
    assert!(name.starts_with("made-") && name.len() > "made-".len());
    assert!(server.row(&configmaps, Some("demo"), name).is_some());
    // A resource discovery says cannot be created.
    let metrics = "/apis/metrics.k8s.io/v1beta1/namespaces/demo/pods";
    let refused = send(
        &client,
        Method::POST,
        metrics,
        "application/json",
        &configmap("m"),
    )
    .await;
    assert_eq!(code(refused), 405);
}

#[tokio::test]
async fn dry_runs_answer_and_change_nothing() {
    let server = creating_server().await;
    let client = server.unrestricted_client().await.unwrap();
    let deployments = Gvr::new("apps", "v1", "deployments");
    let pods = Gvr::core("pods");
    let pod = server.rows(&pods)[0].clone();
    let pod_path = format!(
        "/api/v1/namespaces/{}/pods/{}",
        pod.namespace().unwrap(),
        pod.name()
    );
    let web = server.object(&deployments, Some("demo"), "web").unwrap();
    let replicas = web["spec"]["replicas"].clone();
    let before = version(&server);

    let created = send(
        &client,
        Method::POST,
        &format!("{CONFIGMAPS}?dryRun=All"),
        "application/json",
        &configmap("dry"),
    )
    .await
    .unwrap();
    assert_eq!(created["metadata"]["name"], "dry");
    let patched = send(
        &client,
        Method::PATCH,
        &format!("{WEB}?dryRun=All"),
        MERGE,
        &json!({"spec": {"replicas": 9}}),
    )
    .await
    .unwrap();
    assert_eq!(patched["spec"]["replicas"], 9);
    let mut replaced = web.clone();
    replaced["spec"]["replicas"] = 8.into();
    let put = send(
        &client,
        Method::PUT,
        &format!("{WEB}?dryRun=All"),
        "application/json",
        &replaced,
    )
    .await
    .unwrap();
    assert_eq!(put["spec"]["replicas"], 8);
    let scale = send(
        &client,
        Method::PATCH,
        &format!("{WEB}/scale?dryRun=All"),
        JSON_PATCH,
        &json!([{"op": "add", "path": "/spec/replicas", "value": 7}]),
    )
    .await
    .unwrap();
    assert_eq!(scale["spec"]["replicas"], 7);
    send(
        &client,
        Method::POST,
        &format!("{pod_path}/eviction?dryRun=All"),
        "application/json",
        &json!({"apiVersion": "policy/v1", "kind": "Eviction"}),
    )
    .await
    .unwrap();
    send(
        &client,
        Method::DELETE,
        &format!("{WEB}?dryRun=All"),
        "application/json",
        &json!({}),
    )
    .await
    .unwrap();
    // A dry run still checks.
    let stale = send(
        &client,
        Method::DELETE,
        &format!("{WEB}?dryRun=All"),
        "application/json",
        &json!({"preconditions": {"uid": "another"}}),
    )
    .await;
    assert_eq!(code(stale), 409);

    assert_eq!(version(&server), before, "no version moved");
    assert_eq!(
        lock(&server.inner.state)
            .changes_since(&deployments, before)
            .unwrap(),
        vec![]
    );
    let after = server.object(&deployments, Some("demo"), "web").unwrap();
    assert_eq!(after["spec"]["replicas"], replicas);
    assert!(server.row(&pods, pod.namespace(), pod.name()).is_some());
    assert!(
        server
            .object(&Gvr::core("configmaps"), Some("demo"), "dry")
            .is_none()
    );
    let writes = server.writes();
    assert_eq!(writes.len(), 7);
    assert!(writes.iter().all(WriteRecord::is_dry_run));
}

#[tokio::test]
async fn conflicts_and_admission_refusals_stop_writes_and_leave_reads_alone() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.unrestricted_client().await.unwrap();
    let deployments = Gvr::new("apps", "v1", "deployments");
    let before = version(&server);
    server.inject(Fault::conflict().on(&deployments).named("web").times(1));
    server.inject(
        Fault::admission_denied("policy.example.com", "replicas must be odd")
            .on(&deployments)
            .times(1),
    );
    let get = || http::Request::get(WEB).body(Vec::new()).unwrap();
    client.request::<Value>(get()).await.unwrap();

    let patch = json!({"spec": {"replicas": 4}});
    let conflict = send(&client, Method::PATCH, WEB, MERGE, &patch).await;
    let Err(kube::Error::Api(status)) = conflict else {
        panic!("not a conflict: {conflict:?}");
    };
    assert_eq!((status.code, status.reason.as_str()), (409, "Conflict"));
    let denied = send(&client, Method::PATCH, WEB, MERGE, &patch).await;
    let Err(kube::Error::Api(status)) = denied else {
        panic!("not refused: {denied:?}");
    };
    assert_eq!(status.code, 403);
    assert!(
        status.message.contains("replicas must be odd"),
        "{}",
        status.message
    );
    assert_eq!(version(&server), before, "a refused write changes nothing");
    let statuses: Vec<_> = server.writes().iter().map(|w| w.status).collect();
    assert_eq!(statuses, [Some(409), Some(403)]);

    // Both were used up: the next write lands.
    send(&client, Method::PATCH, WEB, MERGE, &patch)
        .await
        .unwrap();
    let web = server.object(&deployments, Some("demo"), "web").unwrap();
    assert_eq!(web["spec"]["replicas"], 4);
}
