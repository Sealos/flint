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
    /// `ImagePullBackOff`, `CrashLoopBackOff`), not ready; without an earlier instance the pod
    /// is `Pending`. Its log answers 400 "is waiting to start" unless an earlier instance
    /// terminated. [`super::FakeApiServer::start_container`] starts it.
    pub fn waiting(mut self, container: &str, reason: &str) -> Self {
        let status = self.status_mut(container);
        status.insert("state".into(), json!({"waiting": {"reason": reason}}));
        status.insert("ready".into(), false.into());
        status.insert("started".into(), false.into());
        self.sync();
        self
    }

    /// The container exited with `exit_code` and stays terminated (as under `restartPolicy:
    /// Never` or `OnFailure`). Its log can still be read, but not followed.
    pub fn terminated(mut self, container: &str, exit_code: i32) -> Self {
        self.stop(container, exit_code, FIXTURE_NOW);
        self
    }

    /// The container crashed (exit code 1) and runs again: one more restart, its last instance
    /// in `lastState.terminated`, so its previous log can be read.
    pub fn restarted(mut self, container: &str) -> Self {
        self.stop(container, 1, FIXTURE_NOW);
        self.start(container, FIXTURE_NOW);
        self
    }

    /// The running container exits with `exit_code` at `now` (fixture time): `terminated`,
    /// holding the instance's container id, not ready. The restart count stays until it starts
    /// again. Panics if it does not run.
    pub(crate) fn stop(&mut self, container: &str, exit_code: i32, now: &str) {
        let pod = self.name().to_owned();
        let status = self.status_mut(container);
        assert!(
            status.get("state").and_then(|s| s.get("running")).is_some(),
            "container {container:?} of pod {pod} does not run, so it cannot stop"
        );
        let id = status.get("containerID").cloned().unwrap_or_default();
        let started = status
            .get("state")
            .and_then(|s| s.pointer("/running/startedAt"))
            .cloned()
            .unwrap_or_else(|| now.into());
        let reason = if exit_code == 0 { "Completed" } else { "Error" };
        status.insert(
            "state".into(),
            json!({"terminated": {"exitCode": exit_code, "reason": reason, "containerID": id,
                                  "startedAt": started, "finishedAt": now}}),
        );
        status.insert("ready".into(), false.into());
        status.insert("started".into(), false.into());
        self.sync();
    }

    /// The container runs from `now` (fixture time), always as a new instance with an empty
    /// log. One that terminated moves to `lastState.terminated` (its log becomes the previous
    /// one: returns true then); one in CrashLoopBackOff already has it there. Either way the
    /// restart count grows and the container id changes. One that waited for its first start
    /// (Pending) keeps its id and count. Panics if it already runs.
    pub(crate) fn start(&mut self, container: &str, now: &str) -> bool {
        let identity = format!("fake-pod/{}/{}", self.namespace(), self.uid());
        let pod = self.name().to_owned();
        let status = self.status_mut(container);
        let state = status.get("state").cloned().unwrap_or_default();
        assert!(
            state.get("running").is_none(),
            "container {container:?} of pod {pod} already runs"
        );
        let terminated = state.get("terminated").cloned();
        let backing_off = state.get("waiting").is_some()
            && status
                .get("lastState")
                .and_then(|l| l.get("terminated"))
                .is_some();
        let count = status
            .get("restartCount")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if let Some(terminated) = &terminated {
            status.insert("lastState".into(), json!({"terminated": terminated}));
        }
        if terminated.is_some() || backing_off {
            status.insert("restartCount".into(), (count + 1).into());
            status.insert(
                "containerID".into(),
                container_id(&identity, container, count + 1).into(),
            );
        }
        status.insert("state".into(), json!({"running": {"startedAt": now}}));
        status.insert("ready".into(), true.into());
        status.insert("started".into(), true.into());
        self.sync();
        terminated.is_some()
    }

    /// The pod is being deleted (`kubectl delete`): `deletionTimestamp` is set `grace` seconds
    /// after `now` (fixture time) and the pod shows as Terminating until its containers stop.
    pub(crate) fn mark_deleted(&mut self, now: &str, grace: i64) {
        let at = super::clock::parse(now).map_or(0, |(s, _)| s) + grace;
        let meta = self.metadata_mut();
        meta.insert(
            "deletionTimestamp".into(),
            super::clock::format(at, 0).into(),
        );
        meta.insert("deletionGracePeriodSeconds".into(), grace.into());
        self.sync();
    }

    /// Every running container exits 0 at `now` (it honoured SIGTERM). On a pod being
    /// deleted, the phase becomes terminal.
    pub(crate) fn stop_all(&mut self, now: &str) {
        let running: Vec<String> = self
            .statuses()
            .filter(|s| s.pointer("/state/running").is_some())
            .filter_map(|s| s["name"].as_str().map(str::to_owned))
            .collect();
        for container in running {
            self.stop(&container, 0, now);
        }
        self.sync();
    }

    fn statuses(&self) -> impl Iterator<Item = &Value> {
        self.0
            .pointer("/status/containerStatuses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    }

    /// The Ready condition and the phase follow the containers.
    fn sync(&mut self) {
        let ready = ready_counts(&self.0);
        let all = ready.0 == ready.1 && ready.1 > 0;
        let phase = phase(&self.0);
        let conditions = self
            .0
            .pointer_mut("/status/conditions")
            .and_then(Value::as_array_mut);
        for condition in conditions.into_iter().flatten() {
            if condition["type"] == "Ready" {
                condition["status"] = if all { "True" } else { "False" }.into();
            }
        }
        if let Some(status) = self.0.get_mut("status").and_then(Value::as_object_mut) {
            status.insert("phase".into(), phase.into());
        }
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

/// The phase the kubelet reports (`getPhase` in `pkg/kubelet/kubelet_pods.go`), for regular
/// containers: Pending while one waits for its first start, Running while one runs, and once
/// all stopped: terminal on a pod being deleted (Succeeded if all exited 0, else Failed),
/// Running under `restartPolicy: Always` (they restart), else by exit codes.
fn phase(pod: &Value) -> &'static str {
    let policy = pod
        .pointer("/spec/restartPolicy")
        .and_then(Value::as_str)
        .unwrap_or("Always");
    let statuses: Vec<&Value> = pod
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let total = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let (mut running, mut stopped, mut succeeded, mut waiting) = (0, 0, 0, 0);
    for status in &statuses {
        let state = &status["state"];
        if state.get("running").is_some() {
            running += 1;
        } else if let Some(terminated) = state.get("terminated") {
            stopped += 1;
            if terminated["exitCode"] == 0 {
                succeeded += 1;
            }
        } else if state.get("waiting").is_some() {
            if status.pointer("/lastState/terminated").is_some() {
                stopped += 1;
            } else {
                waiting += 1;
            }
        }
    }
    let unknown = total.saturating_sub(statuses.len());
    let deleting = pod.pointer("/metadata/deletionTimestamp").is_some();
    match () {
        () if waiting > 0 => "Pending",
        () if running > 0 && unknown == 0 => "Running",
        () if running == 0 && stopped > 0 && unknown == 0 => {
            if deleting {
                if stopped == succeeded {
                    "Succeeded"
                } else {
                    "Failed"
                }
            } else if policy == "Always" {
                "Running"
            } else if stopped == succeeded {
                "Succeeded"
            } else if policy == "Never" {
                "Failed"
            } else {
                "Running"
            }
        }
        () => "Pending",
    }
}

/// What the API server prints for a pod in its Table row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PodCells {
    /// Ready, Status and Restarts (the count), Node, IP and the `<none>` columns.
    pub cells: Vec<(&'static str, String)>,
    /// When a container last terminated (fixture time), if it restarted: the Restarts cell
    /// reads "<n> (<age> ago)" from it, rendered when served, as Age is.
    pub last_restart: Option<String>,
}

/// The Table cells the API server prints for a pod (`printPod` in
/// `pkg/printers/internalversion/printers.go`, v1.37), for regular containers. Age and the
/// age of the last restart are rendered when served.
pub(crate) fn table_cells(pod: &Value) -> PodCells {
    let statuses: Vec<&Value> = pod
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let total = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let phase = pod
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut reason = pod
        .pointer("/status/reason")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .unwrap_or(phase)
        .to_owned();
    let mut restarts = 0;
    let mut last_restart: Option<(i64, String)> = None;
    let mut ready = 0;
    let mut has_running = false;
    let mut error_reason = String::new();
    for status in statuses.iter().rev() {
        restarts += status["restartCount"].as_i64().unwrap_or(0);
        if let Some(finished) = status
            .pointer("/lastState/terminated/finishedAt")
            .and_then(Value::as_str)
            && let Some((at, _)) = super::clock::parse(finished)
            && last_restart.as_ref().is_none_or(|(last, _)| *last < at)
        {
            last_restart = Some((at, finished.to_owned()));
        }
        let state = &status["state"];
        if let Some(waiting) = state
            .pointer("/waiting/reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            reason = waiting.to_owned();
        } else if let Some(terminated) = state.get("terminated") {
            reason = match terminated["reason"].as_str().filter(|r| !r.is_empty()) {
                Some(r) => r.to_owned(),
                None => match terminated["signal"].as_i64().filter(|s| *s != 0) {
                    Some(signal) => format!("Signal:{signal}"),
                    None => format!("ExitCode:{}", terminated["exitCode"]),
                },
            };
            if terminated["exitCode"] != 0 {
                error_reason.clone_from(&reason);
            }
        } else if status["ready"] == true && state.get("running").is_some() {
            has_running = true;
            ready += 1;
        }
    }
    if reason == "Completed" {
        let pod_ready = pod
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|c| c["type"] == "Ready" && c["status"] == "True");
        if has_running && pod_ready {
            reason = "Running".to_owned();
        } else if !error_reason.is_empty() {
            reason = error_reason;
        } else if has_running {
            reason = "NotReady".to_owned();
        }
    }
    if pod.pointer("/metadata/deletionTimestamp").is_some()
        && !matches!(phase, "Succeeded" | "Failed")
    {
        reason = "Terminating".to_owned();
    }
    let last_restart = last_restart.filter(|_| restarts != 0).map(|(_, at)| at);
    let text = |pointer: &str| {
        pod.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("<none>")
            .to_owned()
    };
    PodCells {
        cells: vec![
            ("Ready", format!("{ready}/{total}")),
            ("Status", reason),
            ("Restarts", restarts.to_string()),
            ("IP", text("/status/podIP")),
            ("Node", text("/spec/nodeName")),
            ("Nominated Node", "<none>".to_owned()),
            ("Readiness Gates", "<none>".to_owned()),
        ],
        last_restart,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell<'a>(cells: &'a PodCells, name: &str) -> &'a str {
        &cells.cells.iter().find(|(n, _)| *n == name).unwrap().1
    }

    fn status(pod: &FakePod, index: usize) -> &Value {
        &pod.as_json()["status"]["containerStatuses"][index]
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
        assert_eq!(cells.last_restart, None);
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
        assert_eq!(status(&sibling, 0)["restartCount"], 0);
        // Recreated under the same name: another pod.
        assert_ne!(pod.clone().sibling("web-1").uid(), pod.uid());
    }

    #[test]
    fn a_crash_terminates_the_instance_and_a_start_begins_the_next() {
        let mut pod = FakePod::new("demo", "p", &["app"]);
        let first_id = status(&pod, 0)["containerID"].clone();
        pod.stop("app", 1, "2026-01-01T00:00:05Z");
        let crashed = status(&pod, 0);
        assert_eq!(crashed["state"]["terminated"]["containerID"], first_id);
        assert_eq!(crashed["state"]["terminated"]["exitCode"], 1);
        assert_eq!(crashed["restartCount"], 0);
        // Always: the pod stays Running while its container restarts.
        assert_eq!(pod.as_json()["status"]["phase"], "Running");
        let cells = table_cells(pod.as_json());
        assert_eq!(cell(&cells, "Status"), "Error");
        assert_eq!(cell(&cells, "Ready"), "0/1");

        assert!(pod.start("app", "2026-01-01T00:00:06Z"));
        let restarted = status(&pod, 0);
        assert_eq!(restarted["restartCount"], 1);
        assert_eq!(
            restarted["lastState"]["terminated"]["containerID"],
            first_id
        );
        assert_eq!(
            restarted["lastState"]["terminated"]["finishedAt"],
            "2026-01-01T00:00:05Z"
        );
        assert_ne!(restarted["containerID"], first_id);
        let cells = table_cells(pod.as_json());
        assert_eq!(cell(&cells, "Restarts"), "1");
        assert_eq!(cells.last_restart.as_deref(), Some("2026-01-01T00:00:05Z"));
        assert_eq!(cell(&cells, "Status"), "Running");
    }

    #[test]
    fn a_pending_container_starts_as_the_same_instance() {
        let mut pod = FakePod::new("demo", "p", &["app"]).waiting("app", "ContainerCreating");
        assert_eq!(pod.as_json()["status"]["phase"], "Pending");
        assert_eq!(
            cell(&table_cells(pod.as_json()), "Status"),
            "ContainerCreating"
        );
        let id = status(&pod, 0)["containerID"].clone();
        let uid = pod.uid().to_owned();
        assert!(!pod.start("app", "2026-01-01T00:00:01Z"));
        assert_eq!(status(&pod, 0)["restartCount"], 0);
        assert_eq!(status(&pod, 0)["containerID"], id);
        assert_eq!(pod.uid(), uid);
        assert_eq!(pod.as_json()["status"]["phase"], "Running");
        assert_eq!(pod.as_json()["status"]["conditions"][0]["status"], "True");
    }

    #[test]
    fn a_deleted_pod_terminates_then_reaches_a_terminal_phase() {
        let mut pod = FakePod::new("demo", "p", &["app"]);
        pod.mark_deleted("2026-01-01T00:00:00Z", 30);
        assert_eq!(
            pod.as_json()["metadata"]["deletionTimestamp"],
            "2026-01-01T00:00:30Z"
        );
        assert_eq!(cell(&table_cells(pod.as_json()), "Status"), "Terminating");
        pod.stop_all("2026-01-01T00:00:01Z");
        assert_eq!(pod.as_json()["status"]["phase"], "Succeeded");
        assert_eq!(cell(&table_cells(pod.as_json()), "Status"), "Completed");
        // Without a deletion, a stopped container under Always keeps the pod Running.
        let mut kept = FakePod::new("demo", "q", &["app"]);
        kept.stop_all("2026-01-01T00:00:01Z");
        assert_eq!(kept.as_json()["status"]["phase"], "Running");
    }

    #[test]
    fn statuses_follow_print_pod() {
        // A finished container beside a running, ready one: Running only if the pod is Ready.
        let mut pod = FakePod::new("demo", "p", &["app", "side"]);
        pod.stop("side", 0, FIXTURE_NOW);
        assert_eq!(cell(&table_cells(pod.as_json()), "Status"), "NotReady");
        let mut ready = pod.clone();
        ready.0["status"]["conditions"][0]["status"] = "True".into();
        assert_eq!(cell(&table_cells(ready.as_json()), "Status"), "Running");
        // A failed container wins over a completed one.
        let mut failed = FakePod::new("demo", "p", &["app", "side"]);
        failed.stop("side", 1, FIXTURE_NOW);
        failed.stop("app", 0, FIXTURE_NOW);
        assert_eq!(cell(&table_cells(failed.as_json()), "Status"), "Error");
        let mut no_reason = FakePod::new("demo", "p", &["app"]);
        no_reason.stop("app", 2, FIXTURE_NOW);
        no_reason.0["status"]["containerStatuses"][0]["state"]["terminated"]["reason"] = "".into();
        assert_eq!(
            cell(&table_cells(no_reason.as_json()), "Status"),
            "ExitCode:2"
        );
        let job = FakePod::new("demo", "j", &["job"]).terminated("job", 0);
        assert_eq!(cell(&table_cells(job.as_json()), "Status"), "Completed");
        let waiting =
            FakePod::new("demo", "w", &["app", "side"]).waiting("app", "ImagePullBackOff");
        let cells = table_cells(waiting.as_json());
        assert_eq!(
            (cell(&cells, "Ready"), cell(&cells, "Status")),
            ("1/2", "ImagePullBackOff")
        );
    }

    #[test]
    #[should_panic(expected = "has no status for container \"nope\"")]
    fn restarting_an_unknown_container_panics() {
        let _ = FakePod::new("demo", "p", &["app"]).restarted("nope");
    }

    #[test]
    #[should_panic(expected = "does not run, so it cannot stop")]
    fn crashing_a_waiting_container_panics() {
        let mut pod = FakePod::new("demo", "p", &["app"]).waiting("app", "ContainerCreating");
        pod.stop("app", 1, FIXTURE_NOW);
    }

    #[test]
    #[should_panic(expected = "already runs")]
    fn starting_a_running_container_panics() {
        FakePod::new("demo", "p", &["app"]).start("app", FIXTURE_NOW);
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
