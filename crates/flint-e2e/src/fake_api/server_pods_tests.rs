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

#[tokio::test]
async fn restarting_a_container_ends_its_stream_and_keeps_its_log_as_previous() {
    let server = FakeApiServer::start().await.unwrap();
    let api = pods(&server).await;
    let web = ContainerLog::new("demo", WEB);
    server.set_log("demo", WEB, "first run\n");
    let mut lines = open(&api, WEB, &follow()).await.unwrap();
    next(&mut lines).await.unwrap().unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();

    server.restart_container("demo", WEB, "nginx");
    assert!(next(&mut lines).await.is_none());
    let pod = api.get(WEB).await.unwrap();
    let status = &pod.status.unwrap().container_statuses.unwrap()[0];
    assert_eq!(status.restart_count, 1);
    let last = status.last_state.as_ref().unwrap().terminated.as_ref();
    assert_eq!(last.unwrap().exit_code, 1);
    assert!(status.state.as_ref().unwrap().running.is_some());
    let previous = LogParams {
        previous: true,
        ..LogParams::default()
    };
    assert_eq!(read_all(&api, WEB, &previous).await, ["first run"]);
    assert!(read_all(&api, WEB, &LogParams::default()).await.is_empty());
    // The Table row follows: one restart.
    let pods_gvr = server.gvr("pods");
    let restarts = server.column(&pods_gvr, "Restarts").unwrap();
    let row = server.row(&pods_gvr, Some("demo"), WEB).unwrap();
    assert_eq!(row.cells()[restarts], "1 (0s ago)");
    // The new instance is followed again.
    let mut lines = open(&api, WEB, &follow()).await.unwrap();
    server.wait_for_log_streams(1, WAIT).await.unwrap();
    server.append_log(&web, "second run\n");
    let line = next(&mut lines).await.unwrap().unwrap();
    assert_eq!(texts(&[line]), ["second run"]);
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

#[tokio::test]
async fn pods_come_and_go_with_watch_events_and_their_logs() {
    let server = FakeApiServer::start().await.unwrap();
    let client = server.client().await.unwrap();
    let api = pods(&server).await;
    // The pod watch a workload log session opens: plain objects, by label.
    let watch = http::Request::get("/api/v1/namespaces/demo/pods?watch=1&labelSelector=app%3Dweb")
        .header(http::header::ACCEPT, "application/json")
        .body(Vec::new())
        .unwrap();
    let mut watch: Lines = client.request_stream(watch).await.unwrap().lines().boxed();
    let initial = events(&mut watch, 2).await;
    assert!(initial.iter().all(|e| e["type"] == "ADDED"));

    // Scale up: a sibling of a web pod matches the same selector.
    let sibling = server
        .pod("demo", WEB)
        .unwrap()
        .sibling("web-69c6f74b8b-new01");
    assert_eq!(server.add_pod(sibling.clone()), 1);
    let added = &events(&mut watch, 1).await[0];
    assert_eq!(added["type"], "ADDED");
    let object = &added["object"];
    assert_eq!(object["kind"], "Pod");
    assert_eq!(object["metadata"]["name"], "web-69c6f74b8b-new01");
    assert_eq!(object["metadata"]["uid"], sibling.uid());
    assert_eq!(
        object["metadata"]["ownerReferences"][0]["name"],
        "web-69c6f74b8b"
    );
    assert_eq!(object["status"]["containerStatuses"][0]["restartCount"], 0);
    // A new pod starts with an empty log, then shows what is written.
    let new01 = "web-69c6f74b8b-new01";
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

    // A rollout replaces a pod: DELETED, then ADDED.
    let replacement = server
        .pod("demo", WEB)
        .unwrap()
        .sibling("web-69c6f74b8b-new02");
    server.replace_pod("demo", "web-69c6f74b8b-x4gss", replacement);
    let replaced = events(&mut watch, 2).await;
    assert_eq!(
        (replaced[0]["type"].as_str(), replaced[1]["type"].as_str()),
        (Some("DELETED"), Some("ADDED"))
    );
    assert_eq!(
        replaced[0]["object"]["metadata"]["name"],
        "web-69c6f74b8b-x4gss"
    );

    // Deleting a pod ends its log stream, then the watch hears of it.
    server.delete_pod("demo", new01);
    assert!(next(&mut new_log).await.is_none());
    let deleted = &events(&mut watch, 1).await[0];
    assert_eq!(deleted["type"], "DELETED");
    let err = open(&api, new01, &follow()).await.err().unwrap();
    assert_eq!(status_of(err).0, 404);
    assert!(api.get_opt(new01).await.unwrap().is_none());

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

#[test]
#[should_panic(expected = "exists; use replace_pod")]
fn adding_an_existing_pod_panics() {
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    let web = server.pod("demo", WEB).unwrap();
    server.add_pod(web);
}
