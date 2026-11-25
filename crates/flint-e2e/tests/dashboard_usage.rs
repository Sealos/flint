//! UP-66: the dashboard's usage tiles against the fake API server: what nodes use against what
//! they offer, what pods request and limit, per node and in total, and the tiles without
//! metrics-server. The feed is the one the overview reads.
//!
//! Not here: the sparkline growing over the hour. A step is 15 s and the trend store reads the
//! real clock, so a test of it would have to wait for real steps (a clock control is missing,
//! #129); `metrics_trend.rs` covers the ring buffer and `ui/dashboard_tests.rs` the tiles.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use flint_core::metrics::MetricsError;
use flint_core::metrics_trend::{TrendResource, TrendScope, TrendStore};
use flint_core::telemetry::{
    MetricsCheck, Telemetry, TelemetrySnapshot, TelemetryTiming, UsageRead, start_for_tests,
};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, FakePod, MetricsTarget, Options};
use k8s_openapi::jiff::Timestamp;
use serde_json::json;

const WAIT: Duration = Duration::from_secs(30);
const NODE: &str = "flint-dev-control-plane";
const MIB: u64 = 1024 * 1024;

async fn start(server: &FakeApiServer, store: &Arc<Mutex<TrendStore>>) -> Telemetry {
    let timing = TelemetryTiming {
        evaluate_every: Duration::from_millis(100),
        evaluate_gap: Duration::from_millis(10),
        metrics_every: Duration::from_millis(100),
        backoff_min: Duration::from_millis(20),
        backoff_max: Duration::from_millis(200),
        ..TelemetryTiming::default()
    };
    start_for_tests(&server.kubeconfig(None), CONTEXT, Arc::clone(store), timing)
        .await
        .unwrap()
}

async fn until(
    feed: &mut Telemetry,
    what: &str,
    check: impl Fn(&TelemetrySnapshot) -> bool,
) -> Arc<TelemetrySnapshot> {
    let waited = tokio::time::timeout(WAIT, async {
        loop {
            let latest = feed.latest();
            if check(&latest) {
                return latest;
            }
            assert!(feed.changed().await, "the feed ended");
        }
    })
    .await;
    waited.unwrap_or_else(|_| panic!("timed out on {what}: {:#?}", feed.latest()))
}

/// The newest value of a series of the store, if it has one.
fn newest(
    store: &Arc<Mutex<TrendStore>>,
    scope: TrendScope<'_>,
    resource: TrendResource,
    now: Timestamp,
) -> Option<u64> {
    store
        .lock()
        .unwrap()
        .series(CONTEXT, scope, resource, now)?
        .last()
        .flatten()
}

/// Waits until the newest value of a series is `value`, and returns the time it was read at.
async fn series_reaches(
    feed: &Telemetry,
    store: &Arc<Mutex<TrendStore>>,
    scope: TrendScope<'_>,
    resource: TrendResource,
    value: u64,
) -> Timestamp {
    tokio::time::timeout(WAIT, async {
        loop {
            let now = Timestamp::now();
            if newest(store, scope, resource, now) == Some(value) {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the series never reached {value}: {:#?}", feed.latest()))
}

fn pod_with_resources(name: &str) -> FakePod {
    FakePod::from_json(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": "demo"},
        "spec": {
            "nodeName": NODE,
            "containers": [{
                "name": "app",
                "image": "busybox:1.37.0",
                "resources": {
                    "requests": {"cpu": "250m", "memory": "64Mi"},
                    "limits": {"cpu": "500m", "memory": "128Mi"},
                },
            }],
        },
        "status": {"phase": "Running"},
    }))
}

// covers UP-66: usage against allocatable and requests and limits against capacity, per node and
// in total.
#[tokio::test]
async fn up_66_usage_tiles_show_use_against_capacity() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_metrics(&MetricsTarget::node(NODE), "750m", "1Gi");
    let store = Arc::new(Mutex::new(TrendStore::new()));
    let mut feed = start(&server, &store).await;

    let base = until(&mut feed, "node usage and capacity", |s| {
        matches!(s.metrics, MetricsCheck::Current { nodes } if nodes > 0)
            && s.capacity.as_ref().is_some_and(|c| c.pods().is_some())
    })
    .await;
    // The total is the sum over the nodes, and the one node's own series is the same.
    let at = series_reaches(
        &feed,
        &store,
        TrendScope::Total,
        TrendResource::Cpu,
        750_000_000,
    )
    .await;
    assert_eq!(
        newest(&store, TrendScope::Node(NODE), TrendResource::Memory, at),
        Some(1024 * MIB)
    );

    // Capacity: what the node offers, and what pods reserve.
    let capacity = base.capacity.as_ref().unwrap();
    let node = capacity
        .nodes
        .iter()
        .find(|n| n.name == NODE)
        .expect("the node");
    assert!(node.allocatable.cpu.is_some() && node.allocatable.memory.is_some());
    assert_eq!(capacity.allocatable().cpu, node.allocatable.cpu);
    let requests = capacity.requests();
    let limits = capacity.limits();
    let (cpu, memory) = (
        requests.cpu.unwrap().nanos(),
        requests.memory.unwrap().bytes(),
    );
    let (limit_cpu, limit_memory) = (limits.cpu.unwrap().nanos(), limits.memory.unwrap().bytes());

    // A pod that reserves resources moves the requests and limits, in total and on its node.
    server.add_pod(pod_with_resources("reserving"));
    let after = until(&mut feed, "the reserving pod", |s| {
        s.capacity
            .as_ref()
            .is_some_and(|c| c.requests().cpu.is_some_and(|now| now.nanos() > cpu))
    })
    .await;
    let capacity = after.capacity.as_ref().unwrap();
    let (requests, limits) = (capacity.requests(), capacity.limits());
    assert_eq!(requests.cpu.unwrap().nanos(), cpu + 250_000_000);
    assert_eq!(requests.memory.unwrap().bytes(), memory + 64 * MIB);
    assert_eq!(limits.cpu.unwrap().nanos(), limit_cpu + 500_000_000);
    assert_eq!(limits.memory.unwrap().bytes(), limit_memory + 128 * MIB);
    let on_node = capacity.nodes.iter().find(|n| n.name == NODE).unwrap();
    assert_eq!(on_node.requests, requests, "one node holds every pod");

    // A new reading moves the used figure.
    server.set_metrics(&MetricsTarget::node(NODE), "900m", "2Gi");
    let waited = series_reaches(
        &feed,
        &store,
        TrendScope::Total,
        TrendResource::Cpu,
        900_000_000,
    )
    .await;
    assert_eq!(
        newest(&store, TrendScope::Total, TrendResource::Memory, waited),
        Some(2048 * MIB)
    );

    feed.stop().await;
    assert!(server.writes().is_empty());
}

// covers UP-66: without metrics-server there is no usage, only the reason, while capacity
// (requests and limits against what nodes offer) still shows.
#[tokio::test]
async fn up_66_without_metrics_server_the_tiles_have_no_usage() {
    let server = FakeApiServer::start_with(Options {
        no_metrics: true,
        ..Options::default()
    })
    .await
    .unwrap();
    let store = Arc::new(Mutex::new(TrendStore::new()));
    let mut feed = start(&server, &store).await;
    let snapshot = until(&mut feed, "the metrics answer and capacity", |s| {
        s.metrics != MetricsCheck::Waiting && s.capacity.is_some()
    })
    .await;
    assert_eq!(
        snapshot.metrics,
        MetricsCheck::Unavailable(MetricsError::NotInstalled)
    );
    assert_eq!(
        newest(&store, TrendScope::Total, TrendResource::Cpu, snapshot.at),
        None
    );
    let capacity = snapshot.capacity.as_ref().unwrap();
    assert!(capacity.allocatable().cpu.is_some());
    assert!(capacity.requests().cpu.is_some());
    feed.stop().await;
}

// covers UP-61: the readings the pod and node lists show come from metrics-server, and follow it.
#[tokio::test]
async fn up_61_list_usage_follows_metrics_server() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_metrics(&MetricsTarget::node(NODE), "250m", "512Mi");
    server.set_metrics(&MetricsTarget::pod("demo", "crash-loop"), "30m", "20Mi");
    let store = Arc::new(Mutex::new(TrendStore::new()));
    let feed = start(&server, &store).await;

    let node_usage = |feed: &Telemetry| feed.side_reads().node_usage.get(&NODE.to_owned());
    let pod_usage = |feed: &Telemetry| match &feed.side_reads().pod_usage {
        UsageRead::Current(values) => values
            .iter()
            .find(|(key, _)| format!("{key:?}").contains("crash-loop"))
            .map(|(_, sample)| sample.usage),
        _ => None,
    };
    let (node, pod) = tokio::time::timeout(WAIT, async {
        loop {
            if let (Some(node), Some(pod)) = (node_usage(&feed), pod_usage(&feed)) {
                return (node, pod);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("both lists have readings");
    assert_eq!(node.cpu.millis(), 250);
    assert_eq!(node.memory.bytes(), 512 * MIB);
    assert_eq!(pod.cpu.millis(), 30);
    assert_eq!(pod.memory.bytes(), 20 * MIB);

    // Each poll brings the newest reading.
    server.set_metrics(&MetricsTarget::node(NODE), "400m", "768Mi");
    tokio::time::timeout(WAIT, async {
        while node_usage(&feed).map(|u| u.cpu.millis()) != Some(400) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the new reading arrives");
    feed.stop().await;
}

// covers UP-61: without metrics-server the lists have no readings and are told why.
#[tokio::test]
async fn up_61_list_usage_without_metrics_server_says_not_installed() {
    let server = FakeApiServer::start_with(Options {
        no_metrics: true,
        ..Options::default()
    })
    .await
    .unwrap();
    let store = Arc::new(Mutex::new(TrendStore::new()));
    let feed = start(&server, &store).await;
    tokio::time::timeout(WAIT, async {
        loop {
            let reads = feed.side_reads();
            let missing = MetricsError::NotInstalled;
            if reads.node_usage == UsageRead::Unavailable(missing.clone())
                && reads.pod_usage == UsageRead::Unavailable(missing)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("both lists learn metrics-server is missing");
    feed.stop().await;
}
