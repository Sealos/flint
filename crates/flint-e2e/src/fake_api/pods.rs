//! Pods that tests create, replace and restart ([`super::FakeApiServer::add_pod`] and friends):
//! full `Pod` objects with owner references, labels, specs and container statuses, and the
//! Table cells the API server prints for them.
//!
//! Timestamps here are in fixture time, as the server stores objects (see [`super::clock`]).

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use super::clock::FIXTURE_NOW;
use crate::record::normalize::stable_uid;

/// The node of the recorded kind cluster, where new pods are scheduled.
const NODE: &str = "flint-dev-control-plane";

/// Makes every pod built in this process distinct, so a pod recreated under the same name (a
/// StatefulSet's) is another pod with another uid, as on a real cluster.
static SERIAL: AtomicU64 = AtomicU64::new(0);

fn next_serial() -> u64 {
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// A pod object for [`super::FakeApiServer::add_pod`] and
/// [`super::FakeApiServer::replace_pod`].
///
/// [`FakePod::new`] builds one from scratch; [`super::FakeApiServer::pod`] returns a current one,
/// and [`FakePod::sibling`] turns it into a fresh pod of the same workload (labels, owner, spec).
/// Every container starts running with no restarts.
#[derive(Debug, Clone, PartialEq)]
pub struct FakePod(Value);

impl FakePod {
    /// A running pod with these containers (busybox images), scheduled on the kind node, with
    /// `restartPolicy: Always` and no labels or owner.
    pub fn new(namespace: &str, name: &str, containers: &[&str]) -> Self {
        let spec_containers: Vec<Value> = containers
            .iter()
            .map(|c| json!({"name": c, "image": "busybox:1.37.0", "imagePullPolicy": "IfNotPresent"}))
            .collect();
        let mut pod = FakePod(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name, "namespace": namespace, "labels": {}},
            "spec": {
                "containers": spec_containers,
                "nodeName": NODE,
                "restartPolicy": "Always",
            },
            "status": {},
        }));
        pod.fresh();
        pod
    }

    /// Wraps a `Pod` object as the API server returns it.
    pub fn from_json(object: Value) -> Self {
        FakePod(object)
    }

    /// Another pod of the same workload, named `name`: the labels, owner references and spec
    /// stay; the uid, creation time and status are new, every container running with no
    /// restarts. The same name gives a pod recreated under its name (another uid).
    pub fn sibling(mut self, name: &str) -> Self {
        if let Some(meta) = self.0.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.insert("name".into(), name.into());
            for key in ["deletionTimestamp", "deletionGracePeriodSeconds"] {
                meta.remove(key);
            }
        }
        self.fresh();
        self
    }

    /// A new uid, creation time and status: every container running, ready, with no restarts.
    fn fresh(&mut self) {
        let identity = format!(
            "fake-pod/{}/{}/{}",
            self.namespace(),
            self.name(),
            next_serial()
        );
        let meta = self.metadata_mut();
        meta.insert("uid".into(), stable_uid(&identity).into());
        meta.insert("creationTimestamp".into(), FIXTURE_NOW.into());
        meta.remove("resourceVersion");
        let statuses: Vec<Value> = self
            .container_names()
            .iter()
            .map(|c| {
                json!({
                    "name": c,
                    "containerID": container_id(&identity, c, 0),
                    "image": self.image_of(c),
                    "ready": true,
                    "started": true,
                    "restartCount": 0,
                    "lastState": {},
                    "state": {"running": {"startedAt": FIXTURE_NOW}},
                })
            })
            .collect();
        let ready = !statuses.is_empty();
        self.0["status"] = json!({
            "phase": "Running",
            "conditions": [
                {"type": "Ready", "status": if ready { "True" } else { "False" }},
                {"type": "PodScheduled", "status": "True"},
            ],
            "containerStatuses": statuses,
            "hostIP": "172.18.0.2",
            "podIP": "10.244.0.200",
            "startTime": FIXTURE_NOW,
        });
    }

    /// Sets a label (workloads select their pods by label).
    pub fn label(mut self, key: &str, value: &str) -> Self {
        let meta = self.metadata_mut();
        let labels = meta.entry("labels").or_insert_with(|| json!({}));
        if let Some(labels) = labels.as_object_mut() {
            labels.insert(key.into(), value.into());
        }
        self
    }

    /// Sets the controller owner reference (`ReplicaSet`, `StatefulSet`, `DaemonSet`, `Job`),
    /// replacing any other.
    pub fn owner(mut self, kind: &str, name: &str) -> Self {
        let api_version = match kind {
            "Job" => "batch/v1",
            "ReplicaSet" | "StatefulSet" | "DaemonSet" => "apps/v1",
            _ => "v1",
        };
        let namespace = self.namespace().to_owned();
        let uid = stable_uid(&format!("fake-owner/{namespace}/{kind}/{name}"));
        self.metadata_mut().insert(
            "ownerReferences".into(),
            json!([{"apiVersion": api_version, "kind": kind, "name": name, "uid": uid,
                    "controller": true, "blockOwnerDeletion": true}]),
        );
        self
    }

    /// The container has not started yet: `waiting` with `reason` (`ContainerCreating`,
    /// `ImagePullBackOff`, `CrashLoopBackOff`), not ready. Its log answers 400 "is waiting to
    /// start" unless an earlier instance terminated.
    pub fn waiting(mut self, container: &str, reason: &str) -> Self {
        let status = self.status_mut(container);
        status.insert("state".into(), json!({"waiting": {"reason": reason}}));
        status.insert("ready".into(), false.into());
        status.insert("started".into(), false.into());
        self.sync_ready();
        self
    }

    /// The container exited with `exit_code` and stays terminated (as under `restartPolicy:
    /// Never` or `OnFailure`). Its log can still be read, but not followed.
    pub fn terminated(mut self, container: &str, exit_code: i32) -> Self {
        let status = self.status_mut(container);
        let id = status.get("containerID").cloned().unwrap_or_default();
        let reason = if exit_code == 0 { "Completed" } else { "Error" };
        status.insert(
            "state".into(),
            json!({"terminated": {"exitCode": exit_code, "reason": reason, "containerID": id,
                                  "startedAt": FIXTURE_NOW, "finishedAt": FIXTURE_NOW}}),
        );
        status.insert("ready".into(), false.into());
        status.insert("started".into(), false.into());
        self.sync_ready();
        self
    }

    /// The container crashed (exit code 1) and runs again: one more restart, its last instance
    /// in `lastState.terminated`, so its previous log can be read.
    pub fn restarted(mut self, container: &str) -> Self {
        self.restart(container, FIXTURE_NOW);
        self
    }

    /// See [`FakePod::restarted`], at `now` (fixture time).
    pub(crate) fn restart(&mut self, container: &str, now: &str) {
        let identity = format!("fake-pod/{}/{}", self.namespace(), self.uid());
        let status = self.status_mut(container);
        let count = status
            .get("restartCount")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        let old_id = status.get("containerID").cloned().unwrap_or_default();
        let started = status
            .get("state")
            .and_then(|s| s.pointer("/running/startedAt"))
            .cloned()
            .unwrap_or_else(|| now.into());
        status.insert(
            "lastState".into(),
            json!({"terminated": {"exitCode": 1, "reason": "Error", "containerID": old_id,
                                  "startedAt": started, "finishedAt": now}}),
        );
        status.insert("state".into(), json!({"running": {"startedAt": now}}));
        status.insert("restartCount".into(), count.into());
        status.insert(
            "containerID".into(),
            container_id(&identity, container, count).into(),
        );
        status.insert("ready".into(), true.into());
        status.insert("started".into(), true.into());
        self.sync_ready();
    }

    /// Sets the creation time and every container's start time (fixture time).
    pub(crate) fn created_at(&mut self, now: &str) {
        self.metadata_mut()
            .insert("creationTimestamp".into(), now.into());
        if let Some(status) = self.0.get_mut("status").and_then(Value::as_object_mut) {
            status.insert("startTime".into(), now.into());
        }
        let statuses = self
            .0
            .pointer_mut("/status/containerStatuses")
            .and_then(Value::as_array_mut);
        for status in statuses.into_iter().flatten() {
            if let Some(running) = status.pointer_mut("/state/running") {
                running["startedAt"] = now.into();
            }
        }
    }

    pub fn name(&self) -> &str {
        self.meta_str("name")
    }

    pub fn namespace(&self) -> &str {
        self.meta_str("namespace")
    }

    pub fn uid(&self) -> &str {
        self.meta_str("uid")
    }

    /// Regular container names, in spec order.
    pub fn container_names(&self) -> Vec<String> {
        self.0
            .pointer("/spec/containers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    pub fn as_json(&self) -> &Value {
        &self.0
    }

    pub fn into_json(self) -> Value {
        self.0
    }

    fn meta_str(&self, key: &str) -> &str {
        self.0
            .pointer(&format!("/metadata/{key}"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    fn metadata_mut(&mut self) -> &mut Map<String, Value> {
        if !self.0["metadata"].is_object() {
            self.0["metadata"] = json!({});
        }
        self.0["metadata"]
            .as_object_mut()
            .expect("metadata is an object")
    }

    fn image_of(&self, container: &str) -> Value {
        self.0
            .pointer("/spec/containers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|c| c["name"] == container)
            .and_then(|c| c.get("image").cloned())
            .unwrap_or_default()
    }

    /// The status of a container; panics if the pod has none (a typo should fail the test).
    fn status_mut(&mut self, container: &str) -> &mut Map<String, Value> {
        let name = self.name().to_owned();
        self.0
            .pointer_mut("/status/containerStatuses")
            .and_then(Value::as_array_mut)
            .and_then(|s| s.iter_mut().find(|s| s["name"] == container))
            .and_then(Value::as_object_mut)
            .unwrap_or_else(|| panic!("pod {name} has no status for container {container:?}"))
    }

    /// The Ready condition follows the containers.
    fn sync_ready(&mut self) {
        let ready = ready_counts(&self.0);
        let all = ready.0 == ready.1 && ready.1 > 0;
        let conditions = self
            .0
            .pointer_mut("/status/conditions")
            .and_then(Value::as_array_mut);
        for condition in conditions.into_iter().flatten() {
            if condition["type"] == "Ready" {
                condition["status"] = if all { "True" } else { "False" }.into();
            }
        }
    }
}

fn container_id(identity: &str, container: &str, instance: i64) -> String {
    let uid = stable_uid(&format!("{identity}/{container}/{instance}")).replace('-', "");
    format!("containerd://{uid}")
}

/// Ready and total regular containers.
fn ready_counts(pod: &Value) -> (usize, usize) {
    let total = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let ready = pod
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s["ready"] == true)
        .count();
    (ready, total)
}

/// The Table cells the API server prints for a pod (`printPod` in
/// `pkg/printers/internalversion/printers.go`), for regular containers: Ready, Status and
/// Restarts, plus Node, IP and the `<none>` columns. Age is rendered when served.
pub(crate) fn table_cells(pod: &Value) -> Vec<(&'static str, String)> {
    let statuses: Vec<&Value> = pod
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let (ready, total) = ready_counts(pod);
    let mut reason = pod
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or("Pending")
        .to_owned();
    let mut restarts = 0;
    let mut restarted = false;
    let mut running = false;
    for status in statuses.iter().rev() {
        restarts += status["restartCount"].as_i64().unwrap_or(0);
        restarted |= status.pointer("/lastState/terminated").is_some();
        if let Some(waiting) = status
            .pointer("/state/waiting/reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            reason = waiting.to_owned();
        } else if let Some(terminated) = status.pointer("/state/terminated") {
            reason = terminated["reason"].as_str().map_or_else(
                || format!("ExitCode:{}", terminated["exitCode"]),
                str::to_owned,
            );
        } else if status["ready"] == true && status.pointer("/state/running").is_some() {
            running = true;
        }
    }
    if reason == "Completed" && running {
        reason = "Running".to_owned();
    }
    if pod.pointer("/metadata/deletionTimestamp").is_some() {
        reason = "Terminating".to_owned();
    }
    // The server prints "<n> (<age> ago)" from the last restart; the fake prints it at the
    // moment of the restart.
    let restarts = if restarts != 0 && restarted {
        format!("{restarts} (0s ago)")
    } else {
        restarts.to_string()
    };
    let text = |pointer: &str| {
        pod.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("<none>")
            .to_owned()
    };
    vec![
        ("Ready", format!("{ready}/{total}")),
        ("Status", reason),
        ("Restarts", restarts),
        ("IP", text("/status/podIP")),
        ("Node", text("/spec/nodeName")),
        ("Nominated Node", "<none>".to_owned()),
        ("Readiness Gates", "<none>".to_owned()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell<'a>(cells: &'a [(&str, String)], name: &str) -> &'a str {
        &cells.iter().find(|(n, _)| *n == name).unwrap().1
    }

    #[test]
    fn new_pods_run_every_container() {
        let pod = FakePod::new("demo", "multi", &["app", "sidecar"])
            .label("app", "multi")
            .owner("ReplicaSet", "multi-abc");
        assert_eq!(pod.container_names(), ["app", "sidecar"]);
        assert_eq!((pod.namespace(), pod.name()), ("demo", "multi"));
        let json = pod.as_json();
        assert_eq!(json["metadata"]["labels"]["app"], "multi");
        assert_eq!(json["metadata"]["ownerReferences"][0]["kind"], "ReplicaSet");
        assert_eq!(
            json["metadata"]["ownerReferences"][0]["apiVersion"],
            "apps/v1"
        );
        let statuses = json["status"]["containerStatuses"].as_array().unwrap();
        assert!(statuses.iter().all(|s| s["state"]["running"].is_object()));
        assert_ne!(statuses[0]["containerID"], statuses[1]["containerID"]);
        let cells = table_cells(json);
        assert_eq!(cell(&cells, "Ready"), "2/2");
        assert_eq!(cell(&cells, "Status"), "Running");
        assert_eq!(cell(&cells, "Restarts"), "0");
        assert_eq!(cell(&cells, "Node"), NODE);
    }

    #[test]
    fn siblings_keep_the_workload_and_get_a_new_identity() {
        let pod = FakePod::new("demo", "web-1", &["nginx"])
            .label("app", "web")
            .owner("ReplicaSet", "web-abc")
            .restarted("nginx");
        let sibling = pod.clone().sibling("web-2");
        assert_eq!(sibling.name(), "web-2");
        assert_ne!(sibling.uid(), pod.uid());
        assert_eq!(sibling.as_json()["metadata"]["labels"]["app"], "web");
        assert_eq!(
            sibling.as_json()["metadata"]["ownerReferences"],
            pod.as_json()["metadata"]["ownerReferences"]
        );
        assert_eq!(
            sibling.as_json()["status"]["containerStatuses"][0]["restartCount"],
            0
        );
        // Recreated under the same name: another pod.
        assert_ne!(pod.clone().sibling("web-1").uid(), pod.uid());
    }

    #[test]
    fn restarts_keep_the_last_instance() {
        let mut pod = FakePod::new("demo", "p", &["app"]);
        let first_id = pod.as_json()["status"]["containerStatuses"][0]["containerID"].clone();
        pod.restart("app", "2026-01-01T00:00:05Z");
        let status = &pod.as_json()["status"]["containerStatuses"][0];
        assert_eq!(status["restartCount"], 1);
        assert_eq!(status["lastState"]["terminated"]["containerID"], first_id);
        assert_eq!(status["lastState"]["terminated"]["exitCode"], 1);
        assert_ne!(status["containerID"], first_id);
        assert_eq!(
            status["state"]["running"]["startedAt"],
            "2026-01-01T00:00:05Z"
        );
        let cells = table_cells(pod.as_json());
        assert_eq!(cell(&cells, "Restarts"), "1 (0s ago)");
        assert_eq!(cell(&cells, "Status"), "Running");
    }

    #[test]
    fn waiting_and_terminated_containers_show_their_reason() {
        let pod = FakePod::new("demo", "p", &["app", "side"]).waiting("app", "ContainerCreating");
        let cells = table_cells(pod.as_json());
        assert_eq!(cell(&cells, "Ready"), "1/2");
        assert_eq!(cell(&cells, "Status"), "ContainerCreating");
        assert_eq!(pod.as_json()["status"]["conditions"][0]["status"], "False");
        let done = FakePod::new("demo", "j", &["job"]).terminated("job", 0);
        assert_eq!(cell(&table_cells(done.as_json()), "Status"), "Completed");
        let failed = FakePod::new("demo", "j", &["job"]).terminated("job", 2);
        assert_eq!(cell(&table_cells(failed.as_json()), "Status"), "Error");
    }

    #[test]
    #[should_panic(expected = "has no status for container \"nope\"")]
    fn restarting_an_unknown_container_panics() {
        let _ = FakePod::new("demo", "p", &["app"]).restarted("nope");
    }

    #[test]
    fn creation_times_follow_the_server() {
        let mut pod = FakePod::new("demo", "p", &["app"]);
        pod.created_at("2026-01-01T00:01:00Z");
        let json = pod.as_json();
        assert_eq!(
            json["metadata"]["creationTimestamp"],
            "2026-01-01T00:01:00Z"
        );
        assert_eq!(
            json["status"]["containerStatuses"][0]["state"]["running"]["startedAt"],
            "2026-01-01T00:01:00Z"
        );
        let wrapped = FakePod::from_json(pod.clone().into_json());
        assert_eq!(wrapped, pod);
    }
}
