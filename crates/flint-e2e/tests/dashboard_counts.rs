//! UP-65: the dashboard's problem counts against the fake API server, with no view open. The
//! counts come from the telemetry feed the overview reads, so these tests read that feed.
//!
//! Not here, and why: the click-through to the filtered list and the tiles (the app's automation
//! state shows only the activity feed (UP-68), so `ui/dashboard_tests.rs` covers them), the usage tiles
//! and thresholds of UP-66 and UP-67 (they need a metrics server and a clock, #129), the diagram
//! of UP-62 to UP-64 and the cluster summary tool of UP-17 (not built, see docs/USER-PATHS.md).
//! A short StatefulSet is left out too: it counts only after a hold-down that lasts a minute.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use flint_core::dashboard::{ProblemCategory, count_pending_claims, count_recent_warnings};
use flint_core::metrics_trend::TrendStore;
use flint_core::telemetry::{
    Availability, Telemetry, TelemetrySnapshot, TelemetryTiming, start_for_tests,
};
use flint_core::{ProblemKind, ProblemSubject};
use flint_e2e::fake_api::clock::FIXTURE_NOW_SECONDS;
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, FakePod, Gvr};
use k8s_openapi::jiff::Timestamp;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);

/// A fixture time `minutes` before the fixtures' "now", which the server moves to its start.
fn ago(minutes: i64) -> String {
    Timestamp::from_second(FIXTURE_NOW_SECONDS - minutes * 60)
        .unwrap()
        .to_string()
}

async fn start(server: &FakeApiServer) -> Telemetry {
    let timing = TelemetryTiming {
        evaluate_every: Duration::from_millis(100),
        evaluate_gap: Duration::from_millis(10),
        metrics_every: Duration::from_millis(100),
        backoff_min: Duration::from_millis(20),
        backoff_max: Duration::from_millis(200),
        ..TelemetryTiming::default()
    };
    start_for_tests(
        &server.kubeconfig(None),
        CONTEXT,
        Arc::new(Mutex::new(TrendStore::new())),
        timing,
    )
    .await
    .unwrap()
}

/// Waits for a snapshot `check` accepts.
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

/// The counts the watched kinds give: failing pods, not-ready nodes, failed jobs and short
/// workloads, once the feed has read them all.
fn watched_counts(snapshot: &TelemetrySnapshot) -> Option<[u32; 4]> {
    if snapshot.availability != Availability::Checked {
        return None;
    }
    let mut all = [0; 4];
    for (slot, category) in all.iter_mut().zip([
        ProblemCategory::FailingPods,
        ProblemCategory::NotReadyNodes,
        ProblemCategory::FailedJobs,
        ProblemCategory::ShortWorkloads,
    ]) {
        *slot = snapshot.counts.get(category)?;
    }
    Some(all)
}

fn plus(base: [u32; 4], extra: [u32; 4]) -> [u32; 4] {
    std::array::from_fn(|i| base[i] + extra[i])
}

fn plus2(base: [u32; 2], extra: [u32; 2]) -> [u32; 2] {
    std::array::from_fn(|i| base[i] + extra[i])
}

/// The warning events of the last hour and the pending claims, as the polled reads give them.
fn polled_counts(feed: &Telemetry) -> Option<[u32; 2]> {
    let reads = feed.side_reads();
    let warnings = count_recent_warnings(reads.warning_events.current()?, Timestamp::now());
    let claims = count_pending_claims(reads.pending_claims.current()?);
    Some([warnings, claims])
}

/// What a fresh feed reports once every category has been read: the problems and counts of the
/// watched kinds, and the polled warnings and claims. The fake server's objects change silently,
/// so each reading is a new feed; the live following of pods is `telemetry.rs`.
async fn dashboard(server: &FakeApiServer) -> (Arc<TelemetrySnapshot>, [u32; 4], [u32; 2]) {
    let mut feed = start(server).await;
    let snapshot = until(&mut feed, "the first full read", |s| {
        watched_counts(s).is_some()
    })
    .await;
    let watched = watched_counts(&snapshot).expect("counted");
    let polled = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(polled) = polled_counts(&feed) {
                return polled;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the polled reads answer");
    feed.stop().await;
    (snapshot, watched, polled)
}

fn object(
    api_version: &str,
    kind: &str,
    namespace: Option<&str>,
    name: &str,
    rest: Value,
) -> Value {
    let mut metadata =
        json!({"name": name, "uid": format!("uid-{name}"), "creationTimestamp": ago(600)});
    if let Some(namespace) = namespace {
        metadata["namespace"] = json!(namespace);
    }
    let mut object = json!({"apiVersion": api_version, "kind": kind, "metadata": metadata});
    for (key, value) in rest.as_object().unwrap() {
        object[key] = value.clone();
    }
    object
}

/// A condition that changed two hours ago, past every grace period.
fn condition(condition_type: &str, status: &str, reason: &str) -> Value {
    json!({"type": condition_type, "status": status, "reason": reason,
           "lastTransitionTime": ago(120), "lastUpdateTime": ago(120)})
}

fn failing_pod(name: &str, reason: &str) -> FakePod {
    let pod = FakePod::new("demo", name, &["app"]);
    if reason == "CrashLoopBackOff" {
        pod.restarted("app")
            .restarted("app")
            .restarted("app")
            .restarted("app")
            .waiting("app", reason)
    } else {
        pod.waiting("app", reason)
    }
}

fn node(name: &str, ready: bool) -> Value {
    let (status, reason) = if ready {
        ("True", "KubeletReady")
    } else {
        ("False", "KubeletNotReady")
    };
    object(
        "v1",
        "Node",
        None,
        name,
        json!({"status": {"conditions": [condition("Ready", status, reason)]}}),
    )
}

fn job(name: &str, failed: bool) -> Value {
    let status = if failed { "True" } else { "False" };
    object(
        "batch/v1",
        "Job",
        Some("demo"),
        name,
        json!({"status": {"conditions": [condition("Failed", status, "BackoffLimitExceeded")]}}),
    )
}

fn deployment(name: &str, available: i64) -> Value {
    let status = if available >= 3 { "True" } else { "False" };
    object(
        "apps/v1",
        "Deployment",
        Some("demo"),
        name,
        json!({
        "spec": {"replicas": 3, "selector": {}, "template": {}},
        "status": {"replicas": 3, "updatedReplicas": 3, "availableReplicas": available,
            "conditions": [
                condition("Available", status, "MinimumReplicasUnavailable"),
                condition("Progressing", "True", "NewReplicaSetAvailable"),
            ]}}),
    )
}

fn event(name: &str, kind: &str, minutes: i64) -> Value {
    object(
        "v1",
        "Event",
        Some("demo"),
        name,
        json!({"type": kind, "reason": "BackOff", "message": "Back-off restarting",
               "involvedObject": {"kind": "Pod", "name": "crashing", "namespace": "demo"},
               "lastTimestamp": ago(minutes), "count": 1}),
    )
}

fn claim(name: &str, phase: &str) -> Value {
    object(
        "v1",
        "PersistentVolumeClaim",
        Some("demo"),
        name,
        json!({"status": {"phase": phase}}),
    )
}

/// What a problem names, as `kind/name`.
fn subject_name(subject: &ProblemSubject) -> String {
    match subject {
        ProblemSubject::Container { pod, .. } => format!("pod/{pod}"),
        ProblemSubject::Pod { name, .. } => format!("pod/{name}"),
        ProblemSubject::Node { name, .. } => format!("node/{name}"),
        ProblemSubject::Job { name, .. } => format!("job/{name}"),
        ProblemSubject::Workload { name, .. } => format!("workload/{name}"),
    }
}

fn named(snapshot: &TelemetrySnapshot, kind: ProblemKind) -> Vec<String> {
    let mut names: Vec<_> = snapshot
        .problems
        .iter()
        .filter(|p| p.kind() == kind)
        .map(|p| subject_name(&p.id.subject))
        .collect();
    names.sort();
    names
}

// covers UP-65: each count matches what the cluster holds and names exactly those objects, and
// goes back down once they are fixed or removed.
#[tokio::test]
async fn up_65_dashboard_counts_match_the_cluster() {
    let server = FakeApiServer::start().await.unwrap();
    let (_, base, polled_base) = dashboard(&server).await;

    let nodes = Gvr::core("nodes");
    let jobs = Gvr::new("batch", "v1", "jobs");
    let deployments = Gvr::new("apps", "v1", "deployments");
    let events = Gvr::core("events");
    let claims = Gvr::core("persistentvolumeclaims");

    // One problem of each category, and healthy twins that must not count.
    server.add_pod(failing_pod("crashing", "CrashLoopBackOff"));
    server.add_pod(failing_pod("crashing-too", "CrashLoopBackOff"));
    server.add_pod(FakePod::new("demo", "fine", &["app"]));
    server.set_object(&nodes, node("down", false));
    server.set_object(&nodes, node("up", true));
    server.set_object(&jobs, job("failed-job", true));
    server.set_object(&jobs, job("running-job", false));
    server.set_object(&deployments, deployment("short-deploy", 1));
    server.set_object(&deployments, deployment("whole-deploy", 3));
    server.set_object(&events, event("warned", "Warning", 5));
    server.set_object(&events, event("too-old", "Warning", 180));
    server.set_object(&events, event("normal", "Normal", 5));
    server.set_object(&claims, claim("waiting-claim", "Pending"));
    server.set_object(&claims, claim("bound-claim", "Bound"));

    // Failing pods, not-ready nodes, failed jobs and short workloads; then warnings and claims.
    let (snapshot, watched, polled) = dashboard(&server).await;
    assert_eq!(watched, plus(base, [2, 1, 1, 1]), "{snapshot:#?}");
    assert_eq!(polled, plus2(polled_base, [1, 1]));
    assert_eq!(named(&snapshot, ProblemKind::NodeNotReady), ["node/down"]);
    assert_eq!(named(&snapshot, ProblemKind::JobFailed), ["job/failed-job"]);
    assert_eq!(
        named(&snapshot, ProblemKind::WorkloadShort),
        ["workload/short-deploy"]
    );
    let crashing = named(&snapshot, ProblemKind::CrashLoop);
    assert!(
        crashing.contains(&"pod/crashing".to_owned()),
        "{crashing:?}"
    );
    assert!(
        crashing.contains(&"pod/crashing-too".to_owned()),
        "{crashing:?}"
    );
    assert!(!crashing.contains(&"pod/fine".to_owned()), "{crashing:?}");

    // Fixing or removing each takes every count back to the start.
    server.delete_pod("demo", "crashing");
    server.delete_pod("demo", "crashing-too");
    server.set_object(&nodes, node("down", true));
    server.remove_object(&jobs, Some("demo"), "failed-job");
    server.set_object(&deployments, deployment("short-deploy", 3));
    server.remove_object(&events, Some("demo"), "warned");
    server.remove_object(&claims, Some("demo"), "waiting-claim");
    let (snapshot, watched, polled) = dashboard(&server).await;
    assert_eq!(watched, base, "{snapshot:#?}");
    assert_eq!(polled, polled_base);
    assert!(server.writes().is_empty());
}
