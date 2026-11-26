use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::batch::v1::Job;
use serde_json::{Value, json};

use super::super::KindCheck;
use super::super::objects::Stores;
use super::*;
use crate::metrics::{Cpu, Memory, PodBudget, PodMetrics};

fn pod(name: &str, node: Option<&str>, phase: &str, spec: Value, owner: Option<Value>) -> Pod {
    let mut spec = spec;
    if let Some(node) = node {
        spec["nodeName"] = json!(node);
    }
    serde_json::from_value(json!({
        "metadata": {
            "name": name, "namespace": "demo", "uid": format!("uid-{name}"),
            "ownerReferences": owner.into_iter().collect::<Vec<_>>(),
        },
        "spec": spec,
        "status": {"phase": phase},
    }))
    .unwrap()
}

fn owner(kind: &str, name: &str) -> Value {
    let api_version = match kind {
        "Job" | "CronJob" => "batch/v1",
        _ => "apps/v1",
    };
    json!({"apiVersion": api_version, "kind": kind, "name": name, "uid": format!("uid-{name}"),
           "controller": true})
}

fn container(name: &str, requests: Value, limits: Value) -> Value {
    json!({"name": name, "resources": {"requests": requests, "limits": limits}})
}

fn current(pods: Vec<Pod>) -> Stores {
    let mut stores = Stores::default();
    stores.pods.replace(pods.into_iter().map(trimmed).collect());
    stores.pods.check = KindCheck::Current;
    stores.replica_sets.check = KindCheck::Current;
    stores.jobs.check = KindCheck::Current;
    stores
}

fn trimmed(mut pod: Pod) -> (Pod, PodRead) {
    let read = pod.trim();
    (pod, read)
}

fn index(stores: &Stores) -> PodIndex {
    PodIndex::of(&stores.pods, &stores.replica_sets, &stores.jobs).unwrap()
}

/// The budget a usage sample of exactly `containers` is compared with.
fn budget(record: &PodRecord, containers: &[&str]) -> Option<PodBudget> {
    let metrics = PodMetrics {
        namespace: "demo".into(),
        name: record.name.to_string(),
        timestamp: None,
        window: None,
        containers: containers
            .iter()
            .map(|name| crate::metrics::ContainerMetrics {
                name: (*name).into(),
                usage: crate::metrics::Usage::default(),
            })
            .collect(),
    };
    record.budget.for_sample(&metrics.sample())
}

fn millis(count: u64) -> Option<Cpu> {
    Some(Cpu::from_nanos(count * 1_000_000))
}

fn mebibytes(count: u64) -> Option<Memory> {
    Some(Memory::from_bytes(count * 1024 * 1024))
}

#[test]
fn a_container_without_a_limit_leaves_the_pod_without_that_limit_total() {
    let spec = json!({"containers": [
        container("a", json!({"cpu": "100m", "memory": "64Mi"}), json!({"cpu": "200m", "memory": "128Mi"})),
        container("b", json!({"cpu": "50m"}), json!({"memory": "64Mi"})),
    ]});
    let stores = current(vec![pod("web", Some("n1"), "Running", spec, None)]);
    let index = index(&stores);
    let record = index.get("uid-web").unwrap();
    let budget = budget(record, &["a", "b"]).unwrap();
    assert_eq!(budget.requests.cpu, millis(150));
    assert_eq!(budget.requests.memory, mebibytes(64));
    assert_eq!(budget.limits.cpu, None, "b sets no CPU limit");
    assert_eq!(budget.limits.memory, mebibytes(192));
    assert_eq!(record.node.as_deref(), Some("n1"));
    assert_eq!(&*record.namespace, "demo");
    assert!(!record.finished);
}

#[test]
fn pod_level_resources_replace_the_container_totals() {
    let spec = json!({
        "containers": [container("a", json!({"cpu": "100m"}), json!({"cpu": "100m"}))],
        "resources": {"requests": {"cpu": "1"}, "limits": {"cpu": "2", "memory": "1Gi"}},
    });
    let stores = current(vec![pod("web", Some("n1"), "Running", spec, None)]);
    let index = index(&stores);
    let budget = budget(index.get("uid-web").unwrap(), &["a"]).unwrap();
    assert_eq!(budget.requests.cpu, millis(1000));
    assert_eq!(budget.limits.cpu, millis(2000));
    assert_eq!(budget.limits.memory, mebibytes(1024));
}

#[test]
fn a_sample_of_other_containers_gets_no_budget() {
    let spec = json!({"containers": [container("a", json!({"cpu": "1"}), json!({}))]});
    let stores = current(vec![pod("web", None, "Pending", spec, None)]);
    let index = index(&stores);
    let record = index.get("uid-web").unwrap();
    assert_eq!(budget(record, &["init"]), None);
    assert_eq!(record.node, None, "not scheduled yet");
}

#[test]
fn records_follow_a_pod_that_moves_finishes_or_is_deleted() {
    let spec = || json!({"containers": [container("a", json!({"cpu": "1"}), json!({}))]});
    let mut stores = current(vec![
        pod("one", Some("n1"), "Running", spec(), None),
        pod("two", Some("n1"), "Running", spec(), None),
    ]);
    let before = index(&stores);
    assert_eq!(before.len(), 2);

    let (moved, read) = trimmed(pod("one", Some("n2"), "Running", spec(), None));
    assert!(stores.pods.upsert(moved, read));
    let after = index(&stores);
    assert_eq!(after.get("uid-one").unwrap().node.as_deref(), Some("n2"));
    assert_ne!(before, after, "a change builds a new index");
    assert_eq!(before.get("uid-one").unwrap().node.as_deref(), Some("n1"));

    let (done, read) = trimmed(pod("two", Some("n1"), "Succeeded", spec(), None));
    assert!(stores.pods.upsert(done, read));
    assert!(index(&stores).get("uid-two").unwrap().finished);

    stores
        .pods
        .remove(&pod("two", Some("n1"), "Succeeded", spec(), None));
    let left = index(&stores);
    assert!(left.get("uid-two").is_none());
    assert_eq!(left.len(), 1);
}

#[test]
fn an_unchanged_store_gives_the_same_index_without_rebuilding() {
    let spec = json!({"containers": [container("a", json!({}), json!({}))]});
    let stores = current(vec![pod("web", Some("n1"), "Running", spec, None)]);
    let (first, second) = (index(&stores), index(&stores));
    assert!(Arc::ptr_eq(&first.pods, &second.pods));
    assert_eq!(first, second);
}

#[test]
fn an_update_that_changes_no_record_keeps_the_index() {
    let spec = || json!({"containers": [container("a", json!({"cpu": "1"}), json!({}))]});
    let mut stores = current(vec![pod("web", Some("n1"), "Running", spec(), None)]);
    let first = index(&stores);
    let mut annotated = pod("web", Some("n1"), "Running", spec(), None);
    annotated.metadata.annotations = Some([("note".to_owned(), "x".to_owned())].into());
    let (annotated, read) = trimmed(annotated);
    assert!(stores.pods.upsert(annotated, read));
    assert_eq!(first, index(&stores), "nothing a list reads changed");

    let (resized, read) = trimmed(pod(
        "web",
        Some("n1"),
        "Running",
        json!({"containers": [container("a", json!({"cpu": "2"}), json!({}))]}),
        None,
    ));
    assert!(stores.pods.upsert(resized, read));
    assert_ne!(first, index(&stores));
}

#[test]
fn a_record_keeps_the_creation_time_to_match_samples_with() {
    let mut pod = pod("web", None, "Pending", json!({"containers": []}), None);
    pod.metadata.creation_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
        jiff::Timestamp::from_second(1_800_000_000).unwrap(),
    ));
    let stores = current(vec![pod]);
    assert_eq!(
        index(&stores).get("uid-web").unwrap().created,
        Some(1_800_000_000)
    );
}

#[test]
fn a_custom_kind_named_like_a_built_in_one_is_its_own_workload() {
    let custom = json!({"apiVersion": "example.com/v1", "kind": "ReplicaSet", "name": "mine",
                        "uid": "uid-mine", "controller": true});
    let mut stores = current(vec![pod(
        "p",
        None,
        "Running",
        json!({"containers": []}),
        Some(custom),
    )]);
    stores.replica_sets.invalidate(KindCheck::TooLarge);
    let index = PodIndex::of(&stores.pods, &stores.replica_sets, &stores.jobs).unwrap();
    let workload = index.workload_of(index.get("uid-p").unwrap()).unwrap();
    assert_eq!((&*workload.group, &*workload.name), ("example.com", "mine"));
}

#[test]
fn without_current_pods_there_is_no_index() {
    let mut stores = current(Vec::new());
    stores.pods.invalidate(KindCheck::TooLarge);
    assert!(PodIndex::of(&stores.pods, &stores.replica_sets, &stores.jobs).is_none());
}

#[test]
fn a_pod_without_a_uid_is_known_by_its_namespaced_name() {
    let mut pod = pod("web", None, "Pending", json!({"containers": []}), None);
    pod.metadata.uid = None;
    let stores = current(vec![pod]);
    assert!(index(&stores).get("demo/web").is_some());
}

fn replica_set(name: &str, deployment: &str) -> (ReplicaSet, ()) {
    let mut set: ReplicaSet = serde_json::from_value(json!({
        "metadata": {"name": name, "namespace": "demo", "uid": format!("uid-{name}"),
                     "ownerReferences": [owner("Deployment", deployment)]},
    }))
    .unwrap();
    set.trim();
    (set, ())
}

#[test]
fn pods_resolve_to_their_workload_through_replica_sets_and_jobs() {
    let spec = || json!({"containers": []});
    let mut stores = current(vec![
        pod(
            "old",
            None,
            "Running",
            spec(),
            Some(owner("ReplicaSet", "web-1")),
        ),
        pod(
            "new",
            None,
            "Running",
            spec(),
            Some(owner("ReplicaSet", "web-2")),
        ),
        pod(
            "db-0",
            None,
            "Running",
            spec(),
            Some(owner("StatefulSet", "db")),
        ),
        pod(
            "run",
            None,
            "Running",
            spec(),
            Some(owner("Job", "nightly-1")),
        ),
        pod("loose", None, "Running", spec(), None),
        pod(
            "orphan",
            None,
            "Running",
            spec(),
            Some(owner("ReplicaSet", "gone")),
        ),
    ]);
    stores.replica_sets.replace(vec![
        replica_set("web-1", "web"),
        replica_set("web-2", "web"),
    ]);
    let mut job: Job = serde_json::from_value(json!({
        "metadata": {"name": "nightly-1", "namespace": "demo", "uid": "uid-nightly-1",
                     "ownerReferences": [owner("CronJob", "nightly")]},
    }))
    .unwrap();
    job.trim();
    stores.jobs.replace(vec![(job, ())]);

    let index = index(&stores);
    let workload = |uid: &str| {
        let record = index.get(uid).unwrap();
        index
            .workload_of(record)
            .map(|w| (w.kind.to_string(), w.name.to_string()))
    };
    let deployment = Some(("Deployment".to_owned(), "web".to_owned()));
    assert_eq!(
        workload("uid-old"),
        deployment,
        "both ReplicaSets of a rollout"
    );
    assert_eq!(workload("uid-new"), deployment);
    assert_eq!(
        workload("uid-db-0"),
        Some(("StatefulSet".into(), "db".into()))
    );
    assert_eq!(
        workload("uid-run"),
        Some(("CronJob".into(), "nightly".into()))
    );
    assert_eq!(workload("uid-loose"), None);
    assert_eq!(
        workload("uid-orphan"),
        Some(("ReplicaSet".into(), "gone".into()))
    );
    assert_eq!(
        index
            .get("uid-old")
            .unwrap()
            .owner
            .as_ref()
            .unwrap()
            .name
            .as_ref(),
        "web-1"
    );

    stores.replica_sets.invalidate(KindCheck::TooLarge);
    let index = super::PodIndex::of(&stores.pods, &stores.replica_sets, &stores.jobs).unwrap();
    let old = index.get("uid-old").unwrap();
    assert_eq!(
        index.workload_of(old),
        None,
        "unknown while ReplicaSets cannot be read"
    );
}

#[test]
fn a_replica_set_change_rebuilds_the_controllers() {
    let mut stores = current(Vec::new());
    stores
        .replica_sets
        .replace(vec![replica_set("web-1", "web")]);
    let first = index(&stores);
    let (set, ()) = replica_set("web-1", "other");
    assert!(stores.replica_sets.upsert(set, ()));
    let second = index(&stores);
    assert_ne!(first, second);
    let owner = &second.replica_sets.as_ref().unwrap()["uid-web-1"];
    assert_eq!(&*owner.name, "other");

    let (same, ()) = replica_set("web-1", "other");
    assert!(stores.replica_sets.upsert(same, ()));
    assert_eq!(
        second,
        index(&stores),
        "a status change keeps the controllers"
    );
}

#[test]
fn pods_share_node_names_namespaces_and_owners() {
    let spec = || json!({"containers": []});
    let pods = (0..100)
        .map(|i| {
            pod(
                &format!("p{i}"),
                Some("node-a"),
                "Running",
                spec(),
                Some(owner("ReplicaSet", "web-1")),
            )
        })
        .collect();
    let stores = current(pods);
    let index = index(&stores);
    let records: Vec<&PodRecord> = index.iter().collect();
    for record in &records[1..] {
        assert!(Arc::ptr_eq(
            record.node.as_ref().unwrap(),
            records[0].node.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(&record.namespace, &records[0].namespace));
        let (a, b) = (
            record.owner.as_ref().unwrap(),
            records[0].owner.as_ref().unwrap(),
        );
        assert!(Arc::ptr_eq(&a.uid, &b.uid));
    }
}

#[test]
fn names_no_record_holds_are_dropped_as_the_set_grows() {
    let mut names = Names::default();
    let kept = names.get("kept");
    for i in 0..(Names::MIN_PRUNE * 4) {
        let _ = names.get(&format!("gone-{i}"));
    }
    assert!(names.set.len() < Names::MIN_PRUNE * 2);
    assert!(Arc::ptr_eq(&names.get("kept"), &kept));
}
