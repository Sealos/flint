//! Pod logs and pod churn over real HTTPS, through kube's client as Flint uses it.

use std::time::{Duration, UNIX_EPOCH};

use futures::{AsyncBufReadExt as _, StreamExt as _, TryStreamExt as _};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, LogParams};
use serde_json::Value;

use crate::fake_api::{ContainerLog, FakeApiServer, FakePod, Fault, Gvr, Options, Verb};

const WAIT: Duration = Duration::from_secs(10);
const WEB: &str = "web-69c6f74b8b-qv4xv";

type Lines = futures::stream::BoxStream<'static, std::io::Result<String>>;

async fn pods(server: &FakeApiServer) -> Api<Pod> {
    Api::namespaced(server.client().await.unwrap(), "demo")
}

/// Opens a log stream as Flint does (`Api::log_stream`), line by line.
async fn open(api: &Api<Pod>, pod: &str, params: &LogParams) -> Result<Lines, kube::Error> {
    Ok(api.log_stream(pod, params).await?.lines().boxed())
}

fn follow() -> LogParams {
    LogParams {
        follow: true,
        timestamps: true,
        ..LogParams::default()
    }
}

/// The next line, which must come in time.
async fn next(lines: &mut Lines) -> Option<std::io::Result<String>> {
    tokio::time::timeout(WAIT, lines.next())
        .await
        .expect("a line or the end in time")
}

async fn read_all(api: &Api<Pod>, pod: &str, params: &LogParams) -> Vec<String> {
    let lines = open(api, pod, params).await.unwrap();
    tokio::time::timeout(WAIT, lines.try_collect())
        .await
        .expect("the log ends")
        .unwrap()
}

fn status_of(err: kube::Error) -> (u16, String) {
    match err {
        kube::Error::Api(status) => (status.code, status.message),
        other => panic!("not an API error: {other}"),
    }
}

/// The text of lines without their timestamps.
fn texts(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .map(|l| l.split_once(' ').map_or(l.as_str(), |(_, t)| t))
        .collect()
}

#[tokio::test]
async fn followed_logs_stream_appended_lines_until_the_container_stops() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let web = ContainerLog::new("demo", WEB);
    server.set_log("demo", WEB, "starting\n");
    let mut lines = open(&api, WEB, &follow()).await.unwrap();
    let first = next(&mut lines).await.unwrap().unwrap();
    let (stamp, text) = first.split_once(' ').unwrap();
    assert_eq!(text, "starting");
    // RFC 3339 in UTC with nine fractional digits, as the kubelet prints them.
    assert_eq!(
        (stamp.len(), stamp.as_bytes()[19], stamp.ends_with('Z')),
        (30, b'.', true)
    );
    assert!(stamp.parse::<Timestamp>().is_ok());
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    assert_eq!(server.open_log_streams_of(&web), 1);

    assert_eq!(server.append_log(&web, "GET / 200\nGET /x 404\n"), 1);
    let a = next(&mut lines).await.unwrap().unwrap();
    let b = next(&mut lines).await.unwrap().unwrap();
    assert_eq!(texts(&[a.clone(), b.clone()]), ["GET / 200", "GET /x 404"]);
    assert!(a < b, "later lines have later timestamps");

    // The container stops: a clean end.
    assert_eq!(server.end_log_streams(&web), 1);
    assert!(next(&mut lines).await.is_none());
    server.wait_for_log_streams(0, WAIT).await.unwrap();
    // The pod still runs, so a new stream reads everything written.
    let all = read_all(&api, WEB, &LogParams::default()).await;
    assert_eq!(all, ["starting", "GET / 200", "GET /x 404"]);
}

#[tokio::test]
async fn clients_that_go_away_close_their_stream_and_drops_break_it() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let web = ContainerLog::new("demo", WEB);
    let lines = open(&api, WEB, &follow()).await.unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    drop(lines);
    server.wait_for_log_streams(0, WAIT).await.unwrap();

    let mut lines = open(&api, WEB, &follow()).await.unwrap();
    for _ in 0..3 {
        next(&mut lines).await.unwrap().unwrap();
    }
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    assert_eq!(server.drop_log_streams(&web), 1);
    assert!(matches!(next(&mut lines).await, Some(Err(_))));
    assert_eq!(server.open_log_streams(), 0);
}

#[tokio::test]
async fn reconnecting_from_since_time_replays_that_second_only() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let web = ContainerLog::new("demo", WEB);
    server.set_log("demo", WEB, "");
    let at = |s: u64, ms: u64| UNIX_EPOCH + Duration::from_millis(s * 1000 + ms);
    server.append_log_at(&web, at(1_700_000_000, 100), "a\n");
    server.append_log_at(&web, at(1_700_000_001, 100), "b\n");
    server.append_log_at(&web, at(1_700_000_001, 600), "c\n");
    server.append_log_at(&web, at(1_700_000_002, 0), "d\n");
    // Flint resumes from the last timestamp it showed; kube sends it rounded down to the
    // second, so the lines of that second come again.
    let params = LogParams {
        since_time: Some(Timestamp::try_from(at(1_700_000_001, 600)).unwrap()),
        timestamps: true,
        ..LogParams::default()
    };
    let lines = read_all(&api, WEB, &params).await;
    assert_eq!(texts(&lines), ["b", "c", "d"]);
    assert_eq!(lines[0], "2023-11-14T22:13:21.100000000Z b");
    let request = server.log_requests().pop().unwrap();
    assert_eq!(
        request.parsed_query().since_time.as_deref(),
        Some("2023-11-14T22:13:21Z")
    );
    // Tail first, then since: the last line is older than a second.
    let tail = LogParams {
        tail_lines: Some(1),
        since_seconds: Some(1),
        ..LogParams::default()
    };
    assert!(read_all(&api, WEB, &tail).await.is_empty());
}

#[tokio::test]
async fn containers_and_instances_are_picked_like_a_real_cluster() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let pod = FakePod::new("demo", "multi", &["app", "side"])
        .label("app", "multi")
        .restarted("app");
    server.add_pod(pod);
    let app = ContainerLog::new("demo", "multi").container("app");
    server.set_container_log(&app.clone().previous(), "crashed\n");
    server.append_log(&app, "app line\n");
    server.append_log(
        &ContainerLog::new("demo", "multi").container("side"),
        "side line\n",
    );

    let err = open(&api, "multi", &LogParams::default())
        .await
        .err()
        .unwrap();
    assert_eq!(
        status_of(err),
        (
            400,
            "a container name must be specified for pod multi, choose one of: [app side]".into()
        )
    );
    let of = |container: &str, previous| LogParams {
        container: Some(container.into()),
        previous,
        ..LogParams::default()
    };
    assert_eq!(
        read_all(&api, "multi", &of("side", false)).await,
        ["side line"]
    );
    assert_eq!(
        read_all(&api, "multi", &of("app", false)).await,
        ["app line"]
    );
    assert_eq!(read_all(&api, "multi", &of("app", true)).await, ["crashed"]);
    let err = open(&api, "multi", &of("side", true)).await.err().unwrap();
    assert_eq!(
        status_of(err),
        (
            400,
            "previous terminated container \"side\" in pod \"multi\" not found".into()
        )
    );
    let err = open(&api, "multi", &of("nope", false)).await.err().unwrap();
    assert_eq!(status_of(err).0, 400);
    // Following a previous instance reads it and ends.
    let followed_previous = LogParams {
        follow: true,
        ..of("app", true)
    };
    assert_eq!(
        read_all(&api, "multi", &followed_previous).await,
        ["crashed"]
    );

    // Waiting with no earlier instance: 400 "waiting to start", which Flint retries.
    server.add_pod(FakePod::new("demo", "new", &["app"]).waiting("app", "ContainerCreating"));
    let err = open(&api, "new", &follow()).await.err().unwrap();
    assert_eq!(
        status_of(err).1,
        "container \"app\" in pod \"new\" is waiting to start: ContainerCreating"
    );
    // CrashLoopBackOff: the last instance is read, and a followed stream ends after it.
    let crash = read_all(&api, "crash-loop", &follow()).await;
    assert_eq!(
        texts(&crash),
        [1, 2, 3].map(|n| format!("fake previous log line {n} of demo/crash-loop"))
    );

    // Options the API server refuses (kube never sends both, so the request is built by hand).
    let client = server.client().await.unwrap();
    let request = http::Request::get(format!(
        "/api/v1/namespaces/demo/pods/{WEB}/log?sinceSeconds=5&sinceTime=2026-01-01T00:00:00Z"
    ))
    .body(Vec::new())
    .unwrap();
    let (code, message) = status_of(client.request_text(request).await.unwrap_err());
    assert_eq!(code, 422);
    assert!(
        message.starts_with(&format!("PodLogOptions \"{WEB}\" is invalid")),
        "{message}"
    );
}

/// A pod Table as Flint lists it.
async fn pod_table(server: &FakeApiServer) -> Value {
    let request = http::Request::get("/api/v1/namespaces/demo/pods")
        .header(
            http::header::ACCEPT,
            "application/json;as=Table;v=v1;g=meta.k8s.io",
        )
        .body(Vec::new())
        .unwrap();
    server
        .client()
        .await
        .unwrap()
        .request(request)
        .await
        .unwrap()
}

fn cell_of(table: &Value, pod: &str, column: &str) -> String {
    let index = table["columnDefinitions"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c["name"] == column)
        .unwrap();
    let row = table["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["object"]["metadata"]["name"] == pod)
        .unwrap();
    assert!(
        row.get("fakeLastRestart").is_none(),
        "internal marker leaked"
    );
    row["cells"][index].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_crashed_container_serves_its_old_instance_until_it_starts_again() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let web = ContainerLog::new("demo", WEB);
    server.set_log("demo", WEB, "first run\n");
    let mut lines = open(&api, WEB, &follow()).await.unwrap();
    next(&mut lines).await.unwrap().unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();

    // The container crashes: the stream ends, and until it starts again a plain request
    // (even a followed one) reads the stopped instance and ends.
    server.crash_container("demo", WEB, "nginx");
    assert!(next(&mut lines).await.is_none());
    let status = &api.get(WEB).await.unwrap().status.unwrap();
    let nginx = &status.container_statuses.as_ref().unwrap()[0];
    assert_eq!(nginx.restart_count, 0);
    let terminated = nginx.state.as_ref().unwrap().terminated.as_ref().unwrap();
    assert_eq!(terminated.exit_code, 1);
    assert_eq!(
        status.phase.as_deref(),
        Some("Running"),
        "Always restarts it"
    );
    assert_eq!(texts(&read_all(&api, WEB, &follow()).await), ["first run"]);
    assert_eq!(server.open_log_streams(), 0);
    assert_eq!(cell_of(&pod_table(&server).await, WEB, "Status"), "Error");

    // It starts: a new instance, the old one is the previous log.
    server.start_container("demo", WEB, "nginx");
    let pod = api.get(WEB).await.unwrap();
    let nginx = &pod.status.unwrap().container_statuses.unwrap()[0];
    assert_eq!(nginx.restart_count, 1);
    let last = nginx.last_state.as_ref().unwrap().terminated.as_ref();
    assert_eq!(last.unwrap().exit_code, 1);
    assert!(nginx.state.as_ref().unwrap().running.is_some());
    let previous = LogParams {
        previous: true,
        ..LogParams::default()
    };
    assert_eq!(read_all(&api, WEB, &previous).await, ["first run"]);
    assert!(read_all(&api, WEB, &LogParams::default()).await.is_empty());
    // Restarts is served with the age of the last restart; the stored row holds the count.
    let table = pod_table(&server).await;
    assert_eq!(cell_of(&table, WEB, "Restarts"), "1 (0s ago)");
    assert_eq!(cell_of(&table, WEB, "Status"), "Running");
    let pods_gvr = server.gvr("pods");
    let restarts = server.column(&pods_gvr, "Restarts").unwrap();
    let row = server.row(&pods_gvr, Some("demo"), WEB).unwrap();
    assert_eq!(row.cells()[restarts], "1");
    // The new instance is followed again.
    let mut lines = open(&api, WEB, &follow()).await.unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    server.append_log(&web, "second run\n");
    let line = next(&mut lines).await.unwrap().unwrap();
    assert_eq!(texts(&[line]), ["second run"]);
}

#[tokio::test]
async fn a_pending_pod_starts_as_the_same_pod() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let pending = server
        .pod("demo", WEB)
        .unwrap()
        .sibling("web-69c6f74b8b-new01")
        .waiting("nginx", "ContainerCreating");
    server.add_pod(pending.clone());
    let pod = api.get("web-69c6f74b8b-new01").await.unwrap();
    assert_eq!(pod.status.unwrap().phase.as_deref(), Some("Pending"));
    let err = open(&api, "web-69c6f74b8b-new01", &follow())
        .await
        .err()
        .unwrap();
    assert!(
        status_of(err)
            .1
            .contains("is waiting to start: ContainerCreating")
    );

    server.start_container("demo", "web-69c6f74b8b-new01", "nginx");
    let pod = api.get("web-69c6f74b8b-new01").await.unwrap();
    assert_eq!(pod.metadata.uid.as_deref(), Some(pending.uid()));
    let status = pod.status.unwrap();
    assert_eq!(status.phase.as_deref(), Some("Running"));
    assert_eq!(status.container_statuses.unwrap()[0].restart_count, 0);
    let mut lines = open(&api, "web-69c6f74b8b-new01", &follow()).await.unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    server.append_log(&ContainerLog::new("demo", "web-69c6f74b8b-new01"), "up\n");
    let line = next(&mut lines).await.unwrap().unwrap();
    assert_eq!(texts(&[line]), ["up"]);
}

#[tokio::test]
async fn a_deleted_pod_terminates_stays_readable_then_goes() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    server.set_log("demo", WEB, "serving\n");
    let mut lines = open(&api, WEB, &follow()).await.unwrap();
    next(&mut lines).await.unwrap().unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();

    server.delete_pod("demo", WEB);
    // The containers stopped: the stream ends while the pod still exists.
    assert!(next(&mut lines).await.is_none());
    let pod = api.get(WEB).await.unwrap();
    assert!(pod.metadata.deletion_timestamp.is_some());
    assert_eq!(pod.metadata.deletion_grace_period_seconds, Some(30));
    assert_eq!(pod.status.unwrap().phase.as_deref(), Some("Succeeded"));
    assert_eq!(texts(&read_all(&api, WEB, &follow()).await), ["serving"]);
    assert_eq!(server.open_log_streams(), 0);

    server.finish_delete("demo", WEB);
    assert!(api.get_opt(WEB).await.unwrap().is_none());
    let err = open(&api, WEB, &follow()).await.err().unwrap();
    assert_eq!(status_of(err).0, 404);

    // A forced deletion skips Terminating.
    let other = "web-69c6f74b8b-x4gss";
    let mut lines = open(&api, other, &follow()).await.unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    server.force_delete_pod("demo", other);
    while let Some(line) = next(&mut lines).await {
        line.unwrap();
    }
    assert!(api.get_opt(other).await.unwrap().is_none());
}

#[tokio::test]
async fn a_pod_being_deleted_keeps_logging_until_its_containers_stop() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    server.set_log("demo", WEB, "serving\n");
    let mut lines = open(&api, WEB, &follow()).await.unwrap();
    next(&mut lines).await.unwrap().unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();

    server.begin_delete_pod("demo", WEB);
    // Terminating, but still running: the stream stays open and gets the shutdown lines.
    let pod = api.get(WEB).await.unwrap();
    assert!(pod.metadata.deletion_timestamp.is_some());
    assert_eq!(pod.status.unwrap().phase.as_deref(), Some("Running"));
    assert_eq!(
        server.append_log(&ContainerLog::new("demo", WEB), "draining\n"),
        1
    );
    assert_eq!(
        texts(&[next(&mut lines).await.unwrap().unwrap()]),
        ["draining"]
    );

    server.finish_shutdown("demo", WEB);
    assert!(next(&mut lines).await.is_none());
    let pod = api.get(WEB).await.unwrap();
    assert_eq!(pod.status.unwrap().phase.as_deref(), Some("Succeeded"));
    server.finish_delete("demo", WEB);
}

#[test]
#[should_panic(expected = "is not being deleted; call begin_delete_pod first")]
fn finishing_a_shutdown_that_never_started_panics() {
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    server.finish_shutdown("demo", WEB);
}

/// Reads plain watch events (as kube's watcher gets them) until `count` arrived.
async fn events(lines: &mut Lines, count: usize) -> Vec<Value> {
    let mut out = Vec::new();
    while out.len() < count {
        let line = next(lines).await.expect("the watch is open").unwrap();
        out.push(serde_json::from_str(&line).unwrap());
    }
    out
}

/// A plain pod watch by label, as a workload log session opens it.
async fn watch_pods(server: &FakeApiServer, selector: &str) -> Lines {
    let watch = http::Request::get(format!(
        "/api/v1/namespaces/demo/pods?watch=1&labelSelector={selector}"
    ))
    .header(http::header::ACCEPT, "application/json")
    .body(Vec::new())
    .unwrap();
    let client = server.client().await.unwrap();
    client.request_stream(watch).await.unwrap().lines().boxed()
}

fn summary(event: &Value) -> (String, String, String) {
    let object = &event["object"];
    (
        event["type"].as_str().unwrap().to_owned(),
        object["metadata"]["uid"].as_str().unwrap().to_owned(),
        object["status"]["phase"].as_str().unwrap_or("").to_owned(),
    )
}

// The events are made at once, before the watch task runs (one thread): each must carry the
// object as it was when it was made, not as it is when it is sent.
#[tokio::test]
async fn watch_events_carry_the_object_as_of_their_change() {
    let server = FakeApiServer::start().await.unwrap();
    let mut watch = watch_pods(&server, "app%3Dfixture-db").await;
    let first = events(&mut watch, 1).await;
    let old = server.pod("demo", "fixture-db-0").unwrap();
    assert_eq!(first[0]["object"]["metadata"]["uid"], old.uid());

    let new = old.clone().sibling("fixture-db-0");
    server.replace_pod("demo", "fixture-db-0", new.clone());
    let seen: Vec<_> = events(&mut watch, 4).await.iter().map(summary).collect();
    let (old_uid, new_uid) = (old.uid().to_owned(), new.uid().to_owned());
    assert_eq!(
        seen,
        [
            ("MODIFIED".into(), old_uid.clone(), "Running".into()),
            ("MODIFIED".into(), old_uid.clone(), "Succeeded".into()),
            ("DELETED".into(), old_uid, "Succeeded".into()),
            ("ADDED".into(), new_uid, "Running".into()),
        ]
    );

    // A restart, then a forced deletion: every event has the full object of its moment.
    server.restart_container("demo", "fixture-db-0", "db");
    server.force_delete_pod("demo", "fixture-db-0");
    let seen = events(&mut watch, 3).await;
    let restarts: Vec<_> = seen
        .iter()
        .map(|e| e["object"]["status"]["containerStatuses"][0]["restartCount"].clone())
        .collect();
    assert_eq!(restarts, [0, 1, 1]);
    assert!(seen[0]["object"]["status"]["containerStatuses"][0]["state"]["terminated"].is_object());
    assert!(
        seen.iter()
            .all(|e| e["object"]["spec"]["containers"].is_array())
    );
    let versions: Vec<u64> = seen
        .iter()
        .map(|e| {
            e["object"]["metadata"]["resourceVersion"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
    assert_eq!(seen[2]["type"], "DELETED");
}

#[tokio::test]
async fn pods_come_and_go_with_watch_events_and_their_logs() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let mut watch = watch_pods(&server, "app%3Dweb").await;
    let initial = events(&mut watch, 2).await;
    assert!(initial.iter().all(|e| e["type"] == "ADDED"));

    // Scale up: a sibling of a web pod matches the same selector.
    let new01 = "web-69c6f74b8b-new01";
    let sibling = server.pod("demo", WEB).unwrap().sibling(new01);
    assert_eq!(server.add_pod(sibling.clone()), 1);
    let added = &events(&mut watch, 1).await[0];
    assert_eq!(added["type"], "ADDED");
    let object = &added["object"];
    assert_eq!(object["kind"], "Pod");
    assert_eq!(object["metadata"]["name"], new01);
    assert_eq!(object["metadata"]["uid"], sibling.uid());
    assert_eq!(
        object["metadata"]["ownerReferences"][0]["name"],
        "web-69c6f74b8b"
    );
    assert_eq!(object["status"]["containerStatuses"][0]["restartCount"], 0);
    // A new pod starts with an empty log, then shows what is written.
    assert!(
        read_all(&api, new01, &LogParams::default())
            .await
            .is_empty()
    );
    let mut new_log = open(&api, new01, &follow()).await.unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    server.append_log(&ContainerLog::new("demo", new01), "hello\n");
    let line = next(&mut new_log).await.unwrap().unwrap();
    assert_eq!(texts(&[line]), ["hello"]);

    // A rollout replaces a pod: it terminates, goes, and the new one comes.
    let replacement = server
        .pod("demo", WEB)
        .unwrap()
        .sibling("web-69c6f74b8b-new02");
    server.replace_pod("demo", "web-69c6f74b8b-x4gss", replacement);
    let replaced = events(&mut watch, 4).await;
    let types: Vec<&str> = replaced
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["MODIFIED", "MODIFIED", "DELETED", "ADDED"]);
    assert_eq!(
        replaced[2]["object"]["metadata"]["name"],
        "web-69c6f74b8b-x4gss"
    );

    // A StatefulSet pod comes back under its name as another pod.
    let db = server.pod("demo", "fixture-db-0").unwrap();
    let old_uid = db.uid().to_owned();
    server.replace_pod("demo", "fixture-db-0", db.sibling("fixture-db-0"));
    let new_uid = api.get("fixture-db-0").await.unwrap().metadata.uid.unwrap();
    assert_ne!(new_uid, old_uid);
    let rows = server.rows(&server.gvr("pods"));
    assert_eq!(
        rows.iter().filter(|r| r.name() == "fixture-db-0").count(),
        1
    );
}

#[tokio::test]
async fn one_pods_log_can_be_refused_while_the_others_stream() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let other = "web-69c6f74b8b-x4gss";
    server.inject(
        Fault::forbidden()
            .on(&Gvr::core("pods"))
            .verb(Verb::Get)
            .named(other)
            .subresource("log"),
    );
    let err = open(&api, other, &follow()).await.err().unwrap();
    assert_eq!(
        status_of(err),
        (
            403,
            format!(
                "pods \"{other}\" is forbidden: User \"flint-e2e\" cannot get resource \
                 \"pods/log\" in API group \"\" in the namespace \"demo\""
            )
        )
    );
    // The pod itself, and the other pod's log, are served.
    assert!(api.get(other).await.is_ok());
    assert_eq!(read_all(&api, WEB, &LogParams::default()).await.len(), 3);
    let requests = server.log_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].status, Some(403));
}

#[test]
#[should_panic(expected = "a container name must be specified for pod multi")]
fn helpers_refuse_an_ambiguous_container() {
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    server.add_pod(FakePod::new("demo", "multi", &["app", "side"]));
    server.append_log(&ContainerLog::new("demo", "multi"), "x\n");
}

#[tokio::test]
async fn every_started_instance_begins_with_an_empty_log() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let previous = LogParams {
        previous: true,
        ..LogParams::default()
    };
    // CrashLoopBackOff (a fixture whose logs nobody wrote): the started instance is new, and
    // its predecessor stays the previous log.
    server.start_container("demo", "crash-loop", "app");
    let pod = api.get("crash-loop").await.unwrap();
    assert_eq!(
        pod.status.unwrap().container_statuses.unwrap()[0].restart_count,
        6
    );
    assert!(
        read_all(&api, "crash-loop", &LogParams::default())
            .await
            .is_empty()
    );
    assert_eq!(
        read_all(&api, "crash-loop", &previous).await[0],
        "fake previous log line 1 of demo/crash-loop"
    );
    // A container that never ran (ImagePullBackOff, Pending) starts empty too.
    server.start_container("demo", "broken-image", "app");
    let pod = api.get("broken-image").await.unwrap();
    assert_eq!(pod.status.unwrap().phase.as_deref(), Some("Running"));
    assert!(
        read_all(&api, "broken-image", &LogParams::default())
            .await
            .is_empty()
    );
}

#[test]
#[should_panic(expected = "is already being deleted; call finish_delete")]
fn deleting_a_pod_twice_panics() {
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    server.delete_pod("demo", WEB);
    server.delete_pod("demo", WEB);
}

#[test]
#[should_panic(expected = "does not run, so it cannot stop")]
fn crashing_a_pending_container_panics() {
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    server.crash_container("demo", "broken-image", "app");
}

#[test]
#[should_panic(expected = "is not being deleted; call delete_pod first")]
fn finishing_a_deletion_that_never_started_panics() {
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    server.finish_delete("demo", WEB);
}

#[test]
#[should_panic(expected = "exists; use replace_pod")]
fn adding_an_existing_pod_panics() {
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    let web = server.pod("demo", WEB).unwrap();
    server.add_pod(web);
}
