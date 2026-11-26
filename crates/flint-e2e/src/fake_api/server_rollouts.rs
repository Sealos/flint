//! Rollouts of Deployments, StatefulSets and DaemonSets, standing in for the API server's
//! `metadata.generation` and the workload controllers' `status` ([`Options::rollouts`]). Tests
//! end a rollout that a write started with [`FakeApiServer::finish_rollout`] or
//! [`FakeApiServer::fail_rollout`].

// module-tests: ../../tests/fleet_actions.rs

use serde_json::json;

use super::*;

/// What a stored write to a Deployment, StatefulSet or DaemonSet does besides storing it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Rollouts {
    /// Nothing: the object is stored as written (the default).
    #[default]
    Untouched,
    /// A change to `spec` moves `metadata.generation` on, as on the API server, and the status
    /// stays until a test ends the rollout.
    Started,
    /// As [`Rollouts::Started`], and the status says the rollout finished at once.
    Finished,
}

const WORKLOADS: [&str; 3] = ["deployments", "statefulsets", "daemonsets"];

fn is_workload(gvr: &Gvr) -> bool {
    gvr.group == "apps" && WORKLOADS.contains(&gvr.resource.as_str())
}

/// Applies [`Options::rollouts`] to `object`, about to be stored over `before` (`None` for a
/// create).
pub(super) fn after_write(mode: Rollouts, gvr: &Gvr, before: Option<&Value>, object: &mut Value) {
    if mode == Rollouts::Untouched || !is_workload(gvr) {
        return;
    }
    let generation = before
        .and_then(|b| b.pointer("/metadata/generation"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let changed = before.is_none_or(|b| b.get("spec") != object.get("spec"));
    let generation = if changed { generation + 1 } else { generation };
    if let Some(meta) = object.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.insert("generation".into(), generation.into());
    }
    if mode == Rollouts::Finished {
        object["status"] = finished_status(gvr, object);
    }
}

fn generation_of(object: &Value) -> i64 {
    object
        .pointer("/metadata/generation")
        .and_then(Value::as_i64)
        .unwrap_or(1)
}

fn replicas_of(object: &Value) -> i64 {
    object
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1)
}

/// The status a controller reports once the object's latest spec has rolled out.
fn finished_status(gvr: &Gvr, object: &Value) -> Value {
    let generation = generation_of(object);
    let replicas = replicas_of(object);
    match gvr.resource.as_str() {
        "statefulsets" => json!({
            "observedGeneration": generation, "replicas": replicas, "readyReplicas": replicas,
            "availableReplicas": replicas, "currentReplicas": replicas, "updatedReplicas": replicas,
            "currentRevision": format!("rev-{generation}"),
            "updateRevision": format!("rev-{generation}"),
        }),
        "daemonsets" => json!({
            "observedGeneration": generation, "desiredNumberScheduled": 1,
            "currentNumberScheduled": 1, "numberMisscheduled": 0, "numberReady": 1,
            "updatedNumberScheduled": 1, "numberAvailable": 1,
        }),
        _ => json!({
            "observedGeneration": generation, "replicas": replicas,
            "updatedReplicas": replicas, "readyReplicas": replicas,
            "availableReplicas": replicas,
            "conditions": [{
                "type": "Progressing", "status": "True", "reason": "NewReplicaSetAvailable",
                "message": "ReplicaSet has successfully progressed.",
            }],
        }),
    }
}

/// The status of a Deployment whose controller gave up on its latest spec.
fn failed_status(object: &Value, message: &str) -> Value {
    json!({
        "observedGeneration": generation_of(object), "replicas": replicas_of(object),
        "updatedReplicas": 0, "availableReplicas": replicas_of(object),
        "conditions": [{
            "type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded",
            "message": message,
        }],
    })
}

impl FakeApiServer {
    /// Reports the rollout of the object's latest spec as finished, as its controller would,
    /// silently, as [`FakeApiServer::set_object`] does.
    pub fn finish_rollout(&self, gvr: &Gvr, namespace: Option<&str>, name: &str) {
        self.set_status(gvr, namespace, name, |object| finished_status(gvr, object));
    }

    /// Reports a Deployment's rollout as past its progress deadline, with `message` as the
    /// controller's words, silently.
    pub fn fail_rollout(&self, gvr: &Gvr, namespace: Option<&str>, name: &str, message: &str) {
        self.set_status(gvr, namespace, name, |object| {
            failed_status(object, message)
        });
    }

    fn set_status(
        &self,
        gvr: &Gvr,
        namespace: Option<&str>,
        name: &str,
        status: impl FnOnce(&Value) -> Value,
    ) {
        let mut object = self
            .object(gvr, namespace, name)
            .unwrap_or_else(|| panic!("no {gvr} \"{name}\" to roll out"));
        object["status"] = status(&object);
        self.set_object(gvr, object);
    }
}
