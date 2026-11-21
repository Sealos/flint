//! The bodies of the resource actions (UP-53): delete, scale, rollout restart, cordon, uncordon
//! and drain.
//!
//! Every request first asks the write gate ([`ActionGate::client`]) and never keeps the client,
//! so a drain stops at its next request once its cluster is locked. Each action names the object
//! the user reviewed by its uid, and where the API allows it its `resourceVersion`: a conflict
//! comes back as [`ActionError::Conflict`], for the review to open again. Nothing here retries.

use std::collections::BTreeSet;
use std::future::Future;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, EvictParams, ListParams, Patch, PatchParams,
    Preconditions,
};
use kube::{Client, ResourceExt as _};
use serde_json::json;

use crate::cluster::ClusterError;
use crate::protection::WriteRefusal;

/// Who changed the object, as the API server records it.
const FIELD_MANAGER: &str = "flint";

/// The annotation `kubectl rollout restart` sets on a workload's pod template.
const RESTARTED_AT: &str = "kubectl.kubernetes.io/restartedAt";

/// The annotation the kubelet puts on a static pod's mirror, which the API cannot evict.
const MIRROR_POD: &str = "kubernetes.io/config.mirror";

/// The write gate, asked before each request of one action.
pub(crate) trait ActionGate: Send {
    /// A client for the next request, if the gate allows it now. Never keep it.
    fn client(&mut self) -> impl Future<Output = Result<Client, ActionError>> + Send;
}

/// The object an action was reviewed for.
#[derive(Debug, Clone)]
pub(crate) struct ActionObject {
    pub(crate) resource: ApiResource,
    pub(crate) namespace: Option<String>,
    pub(crate) name: String,
    pub(crate) uid: String,
    pub(crate) resource_version: Option<String>,
}

impl ActionObject {
    fn api(&self, client: Client) -> Api<DynamicObject> {
        match &self.namespace {
            Some(namespace) => Api::namespaced_with(client, namespace, &self.resource),
            None => Api::all_with(client, &self.resource),
        }
    }
}

/// Why an action did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionError {
    /// The write gate refused it.
    Refused(WriteRefusal),
    /// The object changed since it was reviewed: review it again.
    Conflict(String),
    /// The object is gone.
    Gone(String),
    /// This kind has no such action.
    Unsupported(String),
    Failed(String),
}

impl ActionError {
    fn of(error: kube::Error) -> Self {
        let code = api_code(&error);
        let message = ClusterError::kube(error).summary().to_string();
        match code {
            Some(409) => Self::Conflict(message),
            Some(404) => Self::Gone(message),
            _ => Self::Failed(message),
        }
    }
}

fn api_code(error: &kube::Error) -> Option<u16> {
    match error {
        kube::Error::Api(status) => Some(status.code),
        _ => None,
    }
}

fn patch_params() -> PatchParams {
    PatchParams {
        field_manager: Some(FIELD_MANAGER.to_owned()),
        ..Default::default()
    }
}

fn uid_precondition(uid: &str) -> Preconditions {
    Preconditions {
        uid: Some(uid.to_owned()),
        resource_version: None,
    }
}

/// Deletes `object`, if it is still the one with its uid.
pub(crate) async fn delete(
    gate: &mut impl ActionGate,
    object: &ActionObject,
) -> Result<(), ActionError> {
    let client = gate.client().await?;
    let params = DeleteParams {
        preconditions: Some(uid_precondition(&object.uid)),
        ..Default::default()
    };
    object
        .api(client)
        .delete(&object.name, &params)
        .await
        .map(drop)
        .map_err(ActionError::of)
}

/// Sets `object`'s replicas through its `scale` subresource.
pub(crate) async fn scale(
    gate: &mut impl ActionGate,
    object: &ActionObject,
    replicas: u32,
) -> Result<(), ActionError> {
    let client = gate.client().await?;
    let mut metadata = json!({ "uid": object.uid });
    if let Some(version) = &object.resource_version {
        metadata["resourceVersion"] = json!(version);
    }
    let patch = json!({ "metadata": metadata, "spec": { "replicas": replicas } });
    object
        .api(client)
        .patch_scale(&object.name, &patch_params(), &Patch::Merge(&patch))
        .await
        .map(drop)
        .map_err(ActionError::of)
}

/// Restarts a Deployment's, DaemonSet's or StatefulSet's pods, as `kubectl rollout restart`.
pub(crate) async fn rollout_restart(
    gate: &mut impl ActionGate,
    object: &ActionObject,
) -> Result<(), ActionError> {
    let resource = &object.resource;
    if resource.group != "apps"
        || !matches!(
            resource.plural.as_str(),
            "deployments" | "daemonsets" | "statefulsets"
        )
    {
        return Err(ActionError::Unsupported(format!(
            "{} cannot be restarted",
            resource.plural
        )));
    }
    let client = gate.client().await?;
    let now = jiff::Timestamp::now().to_string();
    let patch = json!({
        "metadata": { "uid": object.uid },
        "spec": { "template": { "metadata": { "annotations": { RESTARTED_AT: now } } } },
    });
    object
        .api(client)
        .patch(&object.name, &patch_params(), &Patch::Merge(&patch))
        .await
        .map(drop)
        .map_err(ActionError::of)
}

/// Marks a node unschedulable, or schedulable again.
pub(crate) async fn set_cordoned(
    gate: &mut impl ActionGate,
    node: &ActionObject,
    cordoned: bool,
) -> Result<(), ActionError> {
    let client = gate.client().await?;
    // By uid only: a node's resourceVersion moves with every status update.
    let patch = json!({
        "metadata": { "uid": node.uid },
        "spec": { "unschedulable": cordoned },
    });
    node.api(client)
        .patch(&node.name, &patch_params(), &Patch::Merge(&patch))
        .await
        .map(drop)
        .map_err(ActionError::of)
}

/// What a drain may evict beyond what `kubectl drain` does by default, and how long it waits.
#[derive(Debug, Clone)]
pub(crate) struct DrainOptions {
    /// Evict pods with `emptyDir` volumes, whose data is lost.
    pub(crate) delete_emptydir_data: bool,
    /// Evict pods no controller will recreate.
    pub(crate) force: bool,
    /// How long to wait for evicted pods to terminate.
    pub(crate) wait: Duration,
    pub(crate) poll: Duration,
}

impl Default for DrainOptions {
    fn default() -> Self {
        Self {
            delete_emptydir_data: false,
            force: false,
            wait: Duration::from_secs(60),
            poll: Duration::from_secs(1),
        }
    }
}

/// A pod, by namespace and name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PodName {
    pub(crate) namespace: String,
    pub(crate) name: String,
}

/// Why a drain left a pod on the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlockedReason {
    /// A PodDisruptionBudget does not allow it now.
    DisruptionBudget,
    /// No controller would recreate it.
    Unmanaged,
    /// It has `emptyDir` data that would be lost.
    LocalStorage,
    /// It was replaced since it was listed.
    Changed,
    Failed(String),
}

/// What a drain did, however it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DrainReport {
    pub(crate) cordoned: bool,
    /// Evictions the API server accepted.
    pub(crate) evicted: Vec<PodName>,
    /// Pods left on the node, and why.
    pub(crate) blocked: Vec<(PodName, BlockedReason)>,
    /// DaemonSet and mirror pods, which a drain leaves.
    pub(crate) skipped: Vec<PodName>,
    /// Pods not tried, because the drain stopped first.
    pub(crate) remaining: Vec<PodName>,
    /// Evicted pods still terminating when the wait ended.
    pub(crate) terminating: Vec<PodName>,
    /// Why the drain stopped early.
    pub(crate) stopped: Option<ActionError>,
}

/// What a drain does with a pod.
#[derive(Debug, PartialEq, Eq)]
enum PodPlan {
    Evict,
    Skip,
    Block(BlockedReason),
}

/// As `kubectl drain --ignore-daemonsets`, with its two opt-ins.
fn plan(pod: &Pod, options: &DrainOptions) -> PodPlan {
    if pod.annotations().contains_key(MIRROR_POD) {
        return PodPlan::Skip;
    }
    let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
    if matches!(phase, Some("Succeeded" | "Failed")) {
        return PodPlan::Evict;
    }
    let controller = pod
        .owner_references()
        .iter()
        .find(|owner| owner.controller == Some(true));
    if controller.is_some_and(|owner| owner.kind == "DaemonSet") {
        return PodPlan::Skip;
    }
    if controller.is_none() && !options.force {
        return PodPlan::Block(BlockedReason::Unmanaged);
    }
    let has_empty_dir = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .is_some_and(|volumes| volumes.iter().any(|v| v.empty_dir.is_some()));
    if has_empty_dir && !options.delete_emptydir_data {
        return PodPlan::Block(BlockedReason::LocalStorage);
    }
    PodPlan::Evict
}

fn pod_name(pod: &Pod) -> PodName {
    PodName {
        namespace: pod.namespace().unwrap_or_default(),
        name: pod.name_any(),
    }
}

/// Cordons `node` and evicts its pods through the Eviction API, one try each. Ends with what was
/// done, also when the gate closes on the way.
pub(crate) async fn drain(
    gate: &mut impl ActionGate,
    node: &ActionObject,
    options: &DrainOptions,
) -> DrainReport {
    let mut report = DrainReport::default();
    if let Err(error) = set_cordoned(gate, node, true).await {
        report.stopped = Some(error);
        return report;
    }
    report.cordoned = true;

    let client = match gate.client().await {
        Ok(client) => client,
        Err(error) => {
            report.stopped = Some(error);
            return report;
        }
    };
    let listed = Api::<Pod>::all(client)
        .list(&ListParams::default().fields(&format!("spec.nodeName={}", node.name)))
        .await;
    let pods = match listed {
        Ok(list) => list.items,
        Err(error) => {
            report.stopped = Some(ActionError::of(error));
            return report;
        }
    };

    let mut to_evict = Vec::new();
    for pod in pods {
        match plan(&pod, options) {
            PodPlan::Evict => to_evict.push(pod),
            PodPlan::Skip => report.skipped.push(pod_name(&pod)),
            PodPlan::Block(reason) => report.blocked.push((pod_name(&pod), reason)),
        }
    }

    let mut accepted = Vec::new();
    let mut pods = to_evict.into_iter();
    while let Some(pod) = pods.next() {
        let name = pod_name(&pod);
        let client = match gate.client().await {
            Ok(client) => client,
            Err(error) => {
                report.remaining.push(name);
                report.remaining.extend(pods.map(|p| pod_name(&p)));
                report.stopped = Some(error);
                return report;
            }
        };
        match evict(client, &pod).await {
            Ok(()) => {
                report.evicted.push(name);
                accepted.push(pod);
            }
            // Gone already: nothing left to do for it.
            Err(error) if api_code(&error) == Some(404) => {}
            Err(error) => {
                let reason = match api_code(&error) {
                    Some(429) => BlockedReason::DisruptionBudget,
                    Some(409) => BlockedReason::Changed,
                    _ => BlockedReason::Failed(ClusterError::kube(error).summary().to_string()),
                };
                report.blocked.push((name, reason));
            }
        }
    }
    let (terminating, stopped) = wait_until_gone(gate, accepted, options).await;
    report.terminating = terminating;
    report.stopped = stopped;
    report
}

async fn evict(client: Client, pod: &Pod) -> Result<(), kube::Error> {
    let namespace = pod.namespace().unwrap_or_default();
    let params = EvictParams {
        delete_options: Some(DeleteParams {
            preconditions: pod.uid().map(|uid| uid_precondition(&uid)),
            ..Default::default()
        }),
        ..Default::default()
    };
    Api::<Pod>::namespaced(client, &namespace)
        .evict(&pod.name_any(), &params)
        .await
        .map(drop)
}

/// Waits for `pods` to go, or for their name to be someone else's. Returns those still there,
/// and the refusal if the gate closed meanwhile.
async fn wait_until_gone(
    gate: &mut impl ActionGate,
    pods: Vec<Pod>,
    options: &DrainOptions,
) -> (Vec<PodName>, Option<ActionError>) {
    let names = |pods: &[Pod]| {
        let names: BTreeSet<PodName> = pods.iter().map(pod_name).collect();
        names.into_iter().collect::<Vec<_>>()
    };
    let deadline = Instant::now() + options.wait;
    let mut left: Vec<Pod> = pods;
    loop {
        if left.is_empty() {
            return (Vec::new(), None);
        }
        let client = match gate.client().await {
            Ok(client) => client,
            Err(error) => return (names(&left), Some(error)),
        };
        let mut still = Vec::new();
        for pod in left {
            let api = Api::<Pod>::namespaced(client.clone(), &pod.namespace().unwrap_or_default());
            match api.get_opt(&pod.name_any()).await {
                Ok(Some(now)) if now.uid() == pod.uid() => still.push(pod),
                Ok(_) => {}
                // Not known to be gone.
                Err(_) => still.push(pod),
            }
        }
        left = still;
        if left.is_empty() || Instant::now() >= deadline {
            return (names(&left), None);
        }
        tokio::time::sleep(options.poll).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use http::{Request, Response};
    use kube::client::Body;
    use serde_json::Value;

    use super::*;

    /// A request the fake API server saw.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        path: String,
        body: Value,
    }

    type Reply = Arc<dyn Fn(&Seen) -> (u16, Value) + Send + Sync>;

    /// A client answered by `reply`, recording what it was sent.
    fn fake(reply: Reply) -> (Client, Arc<Mutex<Vec<Seen>>>) {
        let log: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let seen = log.clone();
        let service = tower::service_fn(move |request: Request<Body>| {
            let (seen, reply) = (seen.clone(), reply.clone());
            async move {
                let (parts, body) = request.into_parts();
                let bytes = body.collect_bytes().await.unwrap();
                let entry = Seen {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_owned(),
                    body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                };
                seen.lock().unwrap().push(entry.clone());
                let (code, value) = reply(&entry);
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&value).unwrap()))
                        .unwrap(),
                )
            }
        });
        (Client::new(service, "default"), log)
    }

    /// A gate the test closes with its flag.
    struct TestGate {
        client: Client,
        closed: Arc<AtomicBool>,
    }

    impl ActionGate for TestGate {
        async fn client(&mut self) -> Result<Client, ActionError> {
            if self.closed.load(Ordering::SeqCst) {
                Err(ActionError::Refused(refusal()))
            } else {
                Ok(self.client.clone())
            }
        }
    }

    fn writes(client: Client, closed: Arc<AtomicBool>) -> TestGate {
        TestGate { client, closed }
    }

    fn refusal() -> WriteRefusal {
        WriteRefusal::Protected {
            cluster: "dev".into(),
        }
    }

    fn resource(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
        ApiResource {
            group: group.into(),
            version: version.into(),
            api_version: if group.is_empty() {
                version.into()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.into(),
            plural: plural.into(),
        }
    }

    fn deployment() -> ActionObject {
        ActionObject {
            resource: resource("apps", "v1", "Deployment", "deployments"),
            namespace: Some("demo".into()),
            name: "web".into(),
            uid: "uid-web".into(),
            resource_version: Some("42".into()),
        }
    }

    fn node() -> ActionObject {
        ActionObject {
            resource: resource("", "v1", "Node", "nodes"),
            namespace: None,
            name: "worker".into(),
            uid: "uid-node".into(),
            resource_version: Some("7".into()),
        }
    }

    fn ok(_: &Seen) -> (u16, Value) {
        (
            200,
            json!({"apiVersion": "v1", "kind": "Status", "status": "Success",
                     "metadata": {"name": "x"}}),
        )
    }

    fn status(code: u16, reason: &str) -> Value {
        json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "code": code, "reason": reason, "message": reason,
        })
    }

    fn open() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    // covers UP-53: delete names the reviewed object by uid.
    #[tokio::test]
    async fn delete_sends_the_uid() {
        let (client, log) = fake(Arc::new(ok));
        delete(&mut writes(client, open()), &deployment())
            .await
            .unwrap();
        let seen = log.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "DELETE");
        assert_eq!(
            seen[0].path,
            "/apis/apps/v1/namespaces/demo/deployments/web"
        );
        assert_eq!(seen[0].body["preconditions"]["uid"], "uid-web");
    }

    // covers UP-52, UP-53: a conflict is said once, for a new review: nothing retries.
    #[tokio::test]
    async fn a_conflict_comes_back_without_a_retry() {
        let (client, log) = fake(Arc::new(|_| (409, status(409, "Conflict"))));
        let mut gated = writes(client, open());
        for result in [
            delete(&mut gated, &deployment()).await,
            scale(&mut gated, &deployment(), 3).await,
            rollout_restart(&mut gated, &deployment()).await,
            set_cordoned(&mut gated, &node(), true).await,
        ] {
            assert!(
                matches!(result, Err(ActionError::Conflict(_))),
                "{result:?}"
            );
        }
        assert_eq!(log.lock().unwrap().len(), 4);
    }

    // covers UP-54: with the gate closed, no action sends anything.
    #[tokio::test]
    async fn a_closed_gate_sends_nothing() {
        let (client, log) = fake(Arc::new(ok));
        let mut gated = writes(client, Arc::new(AtomicBool::new(true)));
        let refused = Err(ActionError::Refused(refusal()));
        assert_eq!(delete(&mut gated, &deployment()).await, refused);
        assert_eq!(scale(&mut gated, &deployment(), 1).await, refused);
        assert_eq!(rollout_restart(&mut gated, &deployment()).await, refused);
        assert_eq!(set_cordoned(&mut gated, &node(), true).await, refused);
        let report = drain(&mut gated, &node(), &DrainOptions::default()).await;
        assert!(!report.cordoned);
        assert_eq!(report.stopped, Some(ActionError::Refused(refusal())));
        assert!(log.lock().unwrap().is_empty());
    }

    // covers UP-53
    #[tokio::test]
    async fn scale_patches_the_scale_subresource_of_the_reviewed_version() {
        let reply: Reply = Arc::new(|_| {
            (
                200,
                json!({"apiVersion": "autoscaling/v1", "kind": "Scale",
                         "metadata": {"name": "web"}, "spec": {"replicas": 3}}),
            )
        });
        let (client, log) = fake(reply);
        scale(&mut writes(client, open()), &deployment(), 3)
            .await
            .unwrap();
        let seen = log.lock().unwrap();
        assert_eq!(seen[0].method, "PATCH");
        assert_eq!(
            seen[0].path,
            "/apis/apps/v1/namespaces/demo/deployments/web/scale"
        );
        assert_eq!(seen[0].body["spec"]["replicas"], 3);
        assert_eq!(seen[0].body["metadata"]["uid"], "uid-web");
        assert_eq!(seen[0].body["metadata"]["resourceVersion"], "42");
    }

    // covers UP-53
    #[tokio::test]
    async fn rollout_restart_annotates_the_pod_template() {
        let reply: Reply = Arc::new(|_| {
            (
                200,
                json!({"apiVersion": "apps/v1", "kind": "Deployment",
                         "metadata": {"name": "web"}}),
            )
        });
        let (client, log) = fake(reply);
        let mut gated = writes(client, open());
        rollout_restart(&mut gated, &deployment()).await.unwrap();
        {
            let seen = log.lock().unwrap();
            assert_eq!(
                seen[0].path,
                "/apis/apps/v1/namespaces/demo/deployments/web"
            );
            assert_eq!(seen[0].body["metadata"]["uid"], "uid-web");
            assert!(
                seen[0].body["spec"]["template"]["metadata"]["annotations"][RESTARTED_AT]
                    .is_string()
            );
        }
        let pod = ActionObject {
            resource: resource("", "v1", "Pod", "pods"),
            ..deployment()
        };
        assert!(matches!(
            rollout_restart(&mut gated, &pod).await,
            Err(ActionError::Unsupported(_))
        ));
        assert_eq!(log.lock().unwrap().len(), 1, "nothing sent for a pod");
    }

    // covers UP-53
    #[tokio::test]
    async fn cordon_and_uncordon_set_the_node_by_uid() {
        let reply: Reply = Arc::new(|_| {
            (
                200,
                json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "worker"}}),
            )
        });
        let (client, log) = fake(reply);
        let mut gated = writes(client, open());
        set_cordoned(&mut gated, &node(), true).await.unwrap();
        set_cordoned(&mut gated, &node(), false).await.unwrap();
        let seen = log.lock().unwrap();
        assert_eq!(seen[0].path, "/api/v1/nodes/worker");
        assert_eq!(seen[0].body["spec"]["unschedulable"], true);
        assert_eq!(seen[1].body["spec"]["unschedulable"], false);
        assert_eq!(seen[0].body["metadata"]["uid"], "uid-node");
    }

    fn pod(name: &str, owner: Option<&str>, extra: Value) -> Value {
        let mut value = json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "demo", "uid": format!("uid-{name}")},
            "spec": {"containers": [{"name": "c"}]},
            "status": {"phase": "Running"},
        });
        if let Some(kind) = owner {
            value["metadata"]["ownerReferences"] = json!([{
                "apiVersion": "apps/v1", "kind": kind, "name": "owner", "uid": "o",
                "controller": true,
            }]);
        }
        json_merge(&mut value, extra);
        value
    }

    fn json_merge(into: &mut Value, from: Value) {
        match (into, from) {
            (Value::Object(into), Value::Object(from)) => {
                for (key, value) in from {
                    json_merge(into.entry(key).or_insert(Value::Null), value);
                }
            }
            (into, from) => *into = from,
        }
    }

    fn pod_list(pods: Vec<Value>) -> Value {
        json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": pods})
    }

    fn named(name: &str) -> PodName {
        PodName {
            namespace: "demo".into(),
            name: name.into(),
        }
    }

    fn evicted_pod(path: &str) -> Option<&str> {
        path.strip_suffix("/eviction")?.rsplit('/').next()
    }

    fn quick() -> DrainOptions {
        DrainOptions {
            wait: Duration::from_millis(30),
            poll: Duration::from_millis(5),
            ..Default::default()
        }
    }

    /// Answers a drain: the node's patch, `pods`, evictions by `evict`, and every pod gone.
    fn drain_server(
        pods: Vec<Value>,
        evict: impl Fn(&str) -> (u16, Value) + Send + Sync + 'static,
    ) -> Reply {
        Arc::new(move |seen| match seen.method.as_str() {
            "PATCH" => (
                200,
                json!({"apiVersion": "v1", "kind": "Node",
                                    "metadata": {"name": "worker"}}),
            ),
            "POST" => evict(evicted_pod(&seen.path).expect("an eviction")),
            _ if seen.path == "/api/v1/pods" => (200, pod_list(pods.clone())),
            _ => (404, status(404, "NotFound")),
        })
    }

    fn accepted() -> (u16, Value) {
        (
            201,
            json!({"apiVersion": "v1", "kind": "Status", "status": "Success"}),
        )
    }

    // covers UP-53: a drain cordons first, evicts what `kubectl drain` evicts through the
    // Eviction API by uid, and says what it left and why.
    #[tokio::test]
    async fn drain_cordons_then_evicts_and_reports_the_rest() {
        let pods = vec![
            pod("web", Some("ReplicaSet"), json!({})),
            pod("agent", Some("DaemonSet"), json!({})),
            pod(
                "static",
                None,
                json!({"metadata": {"annotations": {MIRROR_POD: "x"}}}),
            ),
            pod("bare", None, json!({})),
            pod(
                "cache",
                Some("ReplicaSet"),
                json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}]}}),
            ),
            pod("done", None, json!({"status": {"phase": "Succeeded"}})),
        ];
        let (client, log) = fake(drain_server(pods, |_| accepted()));
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;

        assert!(report.cordoned);
        assert_eq!(report.evicted, [named("web"), named("done")]);
        assert_eq!(report.skipped, [named("agent"), named("static")]);
        assert_eq!(
            report.blocked,
            [
                (named("bare"), BlockedReason::Unmanaged),
                (named("cache"), BlockedReason::LocalStorage),
            ]
        );
        assert!(report.remaining.is_empty());
        assert!(report.terminating.is_empty());
        assert_eq!(report.stopped, None);

        let seen = log.lock().unwrap();
        assert_eq!(seen[0].method, "PATCH", "cordoned first");
        let evictions: Vec<&Seen> = seen.iter().filter(|s| s.method == "POST").collect();
        assert_eq!(evictions.len(), 2);
        assert_eq!(
            evictions[0].path,
            "/api/v1/namespaces/demo/pods/web/eviction"
        );
        assert_eq!(
            evictions[0].body["deleteOptions"]["preconditions"]["uid"],
            "uid-web"
        );
    }

    // covers UP-53: the opt-ins evict what a drain otherwise leaves.
    #[tokio::test]
    async fn drain_options_evict_unmanaged_and_empty_dir_pods() {
        let pods = vec![
            pod("bare", None, json!({})),
            pod(
                "cache",
                Some("ReplicaSet"),
                json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}]}}),
            ),
        ];
        let (client, _log) = fake(drain_server(pods, |_| accepted()));
        let options = DrainOptions {
            force: true,
            delete_emptydir_data: true,
            ..quick()
        };
        let report = drain(&mut writes(client, open()), &node(), &options).await;
        assert_eq!(report.evicted, [named("bare"), named("cache")]);
        assert!(report.blocked.is_empty());
    }

    // covers UP-53: a pod a PodDisruptionBudget protects is reported after one try, and the
    // drain goes on with the others.
    #[tokio::test]
    async fn a_disruption_budget_is_reported_and_not_retried() {
        let pods = vec![
            pod("guarded", Some("ReplicaSet"), json!({})),
            pod("web", Some("ReplicaSet"), json!({})),
        ];
        let (client, log) = fake(drain_server(pods, |name| {
            if name == "guarded" {
                (429, status(429, "TooManyRequests"))
            } else {
                accepted()
            }
        }));
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(
            report.blocked,
            [(named("guarded"), BlockedReason::DisruptionBudget)]
        );
        assert_eq!(report.evicted, [named("web")]);
        let tries = log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.path.ends_with("/guarded/eviction"))
            .count();
        assert_eq!(tries, 1);
    }

    // covers UP-54: the gate closing during a drain stops the next eviction, and the report
    // says what was done and what is left.
    #[tokio::test]
    async fn a_closed_gate_stops_the_next_eviction() {
        let pods = vec![
            pod("one", Some("ReplicaSet"), json!({})),
            pod("two", Some("ReplicaSet"), json!({})),
            pod("three", Some("ReplicaSet"), json!({})),
        ];
        let closed = open();
        let closing = closed.clone();
        let (client, log) = fake(drain_server(pods, move |_| {
            closing.store(true, Ordering::SeqCst);
            accepted()
        }));
        let report = drain(&mut writes(client, closed), &node(), &quick()).await;
        assert!(report.cordoned);
        assert_eq!(report.evicted, [named("one")]);
        assert_eq!(report.remaining, [named("two"), named("three")]);
        assert_eq!(report.stopped, Some(ActionError::Refused(refusal())));
        let evictions = log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.method == "POST")
            .count();
        assert_eq!(evictions, 1);
    }

    // covers UP-54: the gate closing while evicted pods terminate ends the wait, saying so.
    #[tokio::test]
    async fn a_closed_gate_ends_the_wait_for_terminating_pods() {
        let pods = vec![pod("one", Some("ReplicaSet"), json!({}))];
        let closed = open();
        let closing = closed.clone();
        let (client, _log) = fake(drain_server(pods, move |_| {
            closing.store(true, Ordering::SeqCst);
            accepted()
        }));
        let report = drain(&mut writes(client, closed), &node(), &quick()).await;
        assert_eq!(report.evicted, [named("one")]);
        assert_eq!(report.terminating, [named("one")]);
        assert_eq!(report.stopped, Some(ActionError::Refused(refusal())));
    }

    // covers UP-53: a pod replaced since it was listed is reported, and one already gone is not.
    #[tokio::test]
    async fn changed_and_gone_pods_during_a_drain() {
        let pods = vec![
            pod("moved", Some("ReplicaSet"), json!({})),
            pod("gone", Some("ReplicaSet"), json!({})),
        ];
        let (client, _log) = fake(drain_server(pods, |name| match name {
            "moved" => (409, status(409, "Conflict")),
            _ => (404, status(404, "NotFound")),
        }));
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(report.blocked, [(named("moved"), BlockedReason::Changed)]);
        assert!(report.evicted.is_empty());
    }

    // covers UP-53: evicted pods still there when the wait ends are reported as terminating; a
    // new pod under the same name is not the one evicted.
    #[tokio::test]
    async fn the_report_names_pods_still_terminating() {
        let listed = vec![
            pod("slow", Some("ReplicaSet"), json!({})),
            pod("replaced", Some("ReplicaSet"), json!({})),
        ];
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let reply: Reply = Arc::new(move |seen| match seen.method.as_str() {
            "PATCH" => (
                200,
                json!({"apiVersion": "v1", "kind": "Node",
                                    "metadata": {"name": "worker"}}),
            ),
            "POST" => accepted(),
            _ if seen.path == "/api/v1/pods" => (200, pod_list(listed.clone())),
            _ if seen.path.ends_with("/slow") => {
                counted.fetch_add(1, Ordering::SeqCst);
                (200, pod("slow", Some("ReplicaSet"), json!({})))
            }
            _ => (
                200,
                pod(
                    "replaced",
                    Some("ReplicaSet"),
                    json!({"metadata": {"uid": "a-new-uid"}}),
                ),
            ),
        });
        let (client, _log) = fake(reply);
        let report = drain(&mut writes(client, open()), &node(), &quick()).await;
        assert_eq!(report.terminating, [named("slow")]);
        assert!(reads.load(Ordering::SeqCst) > 1, "it polled");
    }
}
