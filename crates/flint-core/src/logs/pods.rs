//! The pods a workload session follows: which workload or selector the user asked for, how it
//! resolves to a label selector, and the pod watch that reports pods as they come and go (from
//! the API server, or a fake in tests).

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt as _, StreamExt as _};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::api::Api;
use kube::core::Selector;
use kube::runtime::watcher;

use super::source::{ClientLookup, LogError};
use crate::cluster::ClusterError;
use crate::read::KindId;

/// A workload whose pods are selected by its `spec.selector`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkloadKind {
    Deployment,
    StatefulSet,
    DaemonSet,
    ReplicaSet,
    Job,
}

impl WorkloadKind {
    /// The workload kind of a resource kind (`deployments.apps`, `jobs.batch`, …), if it is one.
    pub fn from_kind(kind: &KindId) -> Option<Self> {
        Some(match kind.as_str() {
            "deployments.apps" => WorkloadKind::Deployment,
            "statefulsets.apps" => WorkloadKind::StatefulSet,
            "daemonsets.apps" => WorkloadKind::DaemonSet,
            "replicasets.apps" => WorkloadKind::ReplicaSet,
            "jobs.batch" => WorkloadKind::Job,
            _ => return None,
        })
    }

    /// The kind as Kubernetes names it (`Deployment`).
    pub fn name(&self) -> &'static str {
        match self {
            WorkloadKind::Deployment => "Deployment",
            WorkloadKind::StatefulSet => "StatefulSet",
            WorkloadKind::DaemonSet => "DaemonSet",
            WorkloadKind::ReplicaSet => "ReplicaSet",
            WorkloadKind::Job => "Job",
        }
    }
}

/// Which pods a workload session follows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PodSet {
    /// The pods matching a workload's `spec.selector`, read once when the session starts (it is
    /// immutable for every kind here but ReplicaSet).
    Workload { kind: WorkloadKind, name: String },
    /// The pods matching a label selector, as `kubectl -l` takes it (`app=web,tier!=db`).
    Selector(String),
}

/// The pods of a workload in one namespace, merged into one session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkloadTarget {
    pub namespace: String,
    pub pods: PodSet,
    /// One container of each pod (pods without it are skipped), or `None` for every container
    /// of each pod: its regular containers and native sidecars (init containers with
    /// `restartPolicy: Always`). Ordinary init containers are read only when named.
    pub container: Option<String>,
}

/// What a session needs to know about a pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PodInfo {
    pub name: String,
    pub uid: String,
    /// Init containers first, in spec order, then regular containers.
    pub containers: Vec<ContainerInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerRole {
    /// Runs once before the regular containers.
    Init,
    /// An init container with `restartPolicy: Always`: runs beside the regular containers.
    Sidecar,
    Regular,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerInfo {
    pub name: String,
    pub role: ContainerRole,
    /// It has run at least once, so it has a log to read.
    pub started: bool,
    pub running: bool,
    pub restarts: i32,
    /// A previous, terminated instance exists.
    pub has_previous: bool,
}

impl PodInfo {
    pub fn from_pod(pod: &Pod) -> Self {
        let spec = pod.spec.as_ref();
        let status = pod.status.as_ref();
        let statuses = status.into_iter().flat_map(|s| {
            s.init_container_statuses
                .iter()
                .flatten()
                .chain(s.container_statuses.iter().flatten())
        });
        let statuses: Vec<_> = statuses.collect();
        let info = |name: &str, role| {
            let st = statuses.iter().find(|s| s.name == name);
            let state = st.and_then(|s| s.state.as_ref());
            let has_previous = st
                .and_then(|s| s.last_state.as_ref())
                .is_some_and(|l| l.terminated.is_some());
            let running = state.is_some_and(|s| s.running.is_some());
            ContainerInfo {
                name: name.to_owned(),
                role,
                started: running || state.is_some_and(|s| s.terminated.is_some()) || has_previous,
                running,
                restarts: st.map_or(0, |s| s.restart_count),
                has_previous,
            }
        };
        let init = spec
            .into_iter()
            .flat_map(|s| s.init_containers.iter().flatten())
            .map(|c| {
                let role = if c.restart_policy.as_deref() == Some("Always") {
                    ContainerRole::Sidecar
                } else {
                    ContainerRole::Init
                };
                info(&c.name, role)
            });
        let regular = spec
            .into_iter()
            .flat_map(|s| s.containers.iter())
            .map(|c| info(&c.name, ContainerRole::Regular));
        PodInfo {
            name: pod.metadata.name.clone().unwrap_or_default(),
            uid: pod.metadata.uid.clone().unwrap_or_default(),
            containers: init.chain(regular).collect(),
        }
    }

    /// The containers a session reads: the named one, or every regular container and sidecar.
    pub fn selected<'a>(
        &'a self,
        container: Option<&'a str>,
    ) -> impl Iterator<Item = &'a ContainerInfo> + 'a {
        self.containers.iter().filter(move |c| match container {
            Some(name) => c.name == name,
            None => c.role != ContainerRole::Init,
        })
    }
}

/// What the pod watch reports. Like kube's watcher: a (re)list is `Init`, the pods, then
/// `InitDone`; pods missing from it are gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PodEvent {
    Init,
    InitApply(PodInfo),
    InitDone,
    Apply(PodInfo),
    Delete(PodInfo),
}

pub(crate) type PodEvents = BoxStream<'static, Result<PodEvent, LogError>>;

pub(crate) trait PodSource: Send + Sync + 'static {
    /// The label selector of a workload's pods.
    fn selector(
        &self,
        namespace: &str,
        kind: WorkloadKind,
        name: &str,
    ) -> BoxFuture<'static, Result<String, LogError>>;

    /// Watches the pods matching `selector`. The stream starts with a list; it ends, or yields
    /// an error, when the watch broke, and the session opens a new one after a backoff.
    fn watch(
        &self,
        namespace: &str,
        selector: &str,
    ) -> BoxFuture<'static, Result<PodEvents, LogError>>;

    /// Checks the session's cluster is still the one it was opened on (see
    /// [`ClientLookup`]), without a request to the server. kube's watcher keeps its client across
    /// its own re-watches, so an open watch would otherwise never notice a context that was
    /// removed or re-pointed.
    fn check(&self) -> BoxFuture<'static, Result<(), LogError>>;
}

/// A label selector as the API takes it. An empty one would select every pod in the namespace,
/// which is never what a workload means, so it is refused.
pub(crate) fn selector_string(
    kind: WorkloadKind,
    name: &str,
    selector: Option<LabelSelector>,
) -> Result<String, LogError> {
    let no_selector = || LogError::fatal(format!("{} \"{name}\" has no pod selector", kind.name()));
    let selector = Selector::try_from(selector.ok_or_else(no_selector)?)
        .map_err(|e| LogError::fatal(format!("{} \"{name}\": {e}", kind.name())))?;
    let text = selector.to_string();
    if text.is_empty() {
        return Err(no_selector());
    }
    Ok(text)
}

/// The API server.
pub(crate) struct KubePodSource {
    client: ClientLookup,
}

impl KubePodSource {
    pub fn new(client: ClientLookup) -> Self {
        KubePodSource { client }
    }
}

async fn spec_selector<K, S>(
    client: kube::Client,
    namespace: &str,
    name: &str,
    selector: S,
) -> Result<Option<LabelSelector>, LogError>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
    K::DynamicType: Default,
    S: FnOnce(K) -> Option<LabelSelector>,
{
    let api: Api<K> = Api::namespaced(client, namespace);
    let object = api.get(name).await.map_err(LogError::from_kube)?;
    Ok(selector(object))
}

impl PodSource for KubePodSource {
    fn selector(
        &self,
        namespace: &str,
        kind: WorkloadKind,
        name: &str,
    ) -> BoxFuture<'static, Result<String, LogError>> {
        let client = self.client.clone();
        let (namespace, name) = (namespace.to_owned(), name.to_owned());
        async move {
            let client = client().await?;
            let (ns, n) = (namespace.as_str(), name.as_str());
            let selector = match kind {
                WorkloadKind::Deployment => {
                    spec_selector(client, ns, n, |d: Deployment| Some(d.spec.selector)).await
                }
                WorkloadKind::StatefulSet => {
                    spec_selector(client, ns, n, |d: StatefulSet| Some(d.spec.selector)).await
                }
                WorkloadKind::DaemonSet => {
                    spec_selector(client, ns, n, |d: DaemonSet| Some(d.spec.selector)).await
                }
                WorkloadKind::ReplicaSet => {
                    spec_selector(client, ns, n, |d: ReplicaSet| Some(d.spec.selector)).await
                }
                WorkloadKind::Job => {
                    spec_selector(client, ns, n, |j: Job| j.spec.and_then(|s| s.selector)).await
                }
            }?;
            selector_string(kind, &name, selector)
        }
        .boxed()
    }

    fn watch(
        &self,
        namespace: &str,
        selector: &str,
    ) -> BoxFuture<'static, Result<PodEvents, LogError>> {
        let client = self.client.clone();
        let (namespace, selector) = (namespace.to_owned(), selector.to_owned());
        async move {
            let client = client().await?;
            let api: Api<Pod> = Api::namespaced(client, &namespace);
            let events = watcher(api, watcher::Config::default().labels(&selector))
                .filter_map(|event| async move {
                    match event {
                        Ok(event) => Some(Ok(pod_event(event))),
                        Err(e) => watch_error(e).map(Err),
                    }
                })
                .boxed();
            Ok(events)
        }
        .boxed()
    }

    fn check(&self) -> BoxFuture<'static, Result<(), LogError>> {
        let client = self.client.clone();
        async move { client().await.map(drop) }.boxed()
    }
}

/// A pod of a workload, as a shell into the workload needs it (UP-41, UP-47).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadPod {
    pub name: String,
    /// Its `Ready` condition is true and it is not being deleted.
    pub ready: bool,
    /// Its app containers (not init containers or sidecars), in spec order.
    pub containers: Vec<String>,
    /// The container `kubectl exec` picks: the one the `kubectl.kubernetes.io/default-container`
    /// annotation names, if it is an app container, else the first.
    pub default_container: Option<String>,
}

impl WorkloadPod {
    pub fn from_pod(pod: &Pod) -> Self {
        let containers: Vec<String> = pod
            .spec
            .iter()
            .flat_map(|s| s.containers.iter().map(|c| c.name.clone()))
            .collect();
        let annotated = pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("kubectl.kubernetes.io/default-container"))
            .filter(|name| containers.contains(name))
            .cloned();
        let ready = pod.metadata.deletion_timestamp.is_none()
            && pod
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
        WorkloadPod {
            name: pod.metadata.name.clone().unwrap_or_default(),
            ready,
            default_container: annotated.or_else(|| containers.first().cloned()),
            containers,
        }
    }

    /// The pod a shell into a workload opens in: the first ready one with a container, by name,
    /// so asking again picks the same pod while it stays ready.
    pub fn first_ready(pods: &[WorkloadPod]) -> Option<&WorkloadPod> {
        pods.iter()
            .filter(|p| p.ready && p.default_container.is_some())
            .min_by(|a, b| a.name.cmp(&b.name))
    }
}

/// Lists the pods of a workload (or a label selector) in `namespace` once, sorted by name.
pub(crate) async fn list_workload_pods(
    client: kube::Client,
    namespace: &str,
    pods: &PodSet,
) -> Result<Vec<WorkloadPod>, LogError> {
    let selector = match pods {
        PodSet::Workload { kind, name } => {
            let lookup: ClientLookup = {
                let client = client.clone();
                std::sync::Arc::new(move || {
                    let client = client.clone();
                    async move { Ok(client) }.boxed()
                })
            };
            KubePodSource::new(lookup)
                .selector(namespace, *kind, name)
                .await?
        }
        PodSet::Selector(selector) => selector.clone(),
    };
    let api: Api<Pod> = Api::namespaced(client, namespace);
    let list = api
        .list(&kube::api::ListParams::default().labels(&selector))
        .await
        .map_err(LogError::from_kube)?;
    let mut pods: Vec<WorkloadPod> = list.items.iter().map(WorkloadPod::from_pod).collect();
    pods.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pods)
}

fn pod_event(event: watcher::Event<Pod>) -> PodEvent {
    match event {
        watcher::Event::Init => PodEvent::Init,
        watcher::Event::InitApply(pod) => PodEvent::InitApply(PodInfo::from_pod(&pod)),
        watcher::Event::InitDone => PodEvent::InitDone,
        watcher::Event::Apply(pod) => PodEvent::Apply(PodInfo::from_pod(&pod)),
        watcher::Event::Delete(pod) => PodEvent::Delete(PodInfo::from_pod(&pod)),
    }
}

/// The error a watch failure is reported as, or `None` for one kube's watcher recovers from by
/// itself (an expired resource version: it lists again).
fn watch_error(err: watcher::Error) -> Option<LogError> {
    match err {
        watcher::Error::InitialListFailed(e)
        | watcher::Error::WatchStartFailed(e)
        | watcher::Error::WatchFailed(e) => Some(LogError::from_kube(e)),
        watcher::Error::WatchError(status) if status.code == 410 => None,
        watcher::Error::WatchError(status) => Some(LogError::from_kube(kube::Error::Api(status))),
        watcher::Error::NoResourceVersion => Some(LogError::retry(
            ClusterError::Kube("the pod watch returned no resource version".into()).to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(value: serde_json::Value) -> Pod {
        serde_json::from_value(value).unwrap()
    }

    fn status(name: &str, state: serde_json::Value, last: serde_json::Value) -> serde_json::Value {
        json!({"name": name, "image": "i", "imageID": "", "ready": true, "restartCount": 2,
               "state": state, "lastState": last})
    }

    // covers UP-45
    #[test]
    fn reads_containers_sidecars_and_what_has_started() {
        let p = pod(json!({
            "metadata": {"name": "web-1", "uid": "u1"},
            "spec": {
                "initContainers": [{"name": "migrate"}, {"name": "proxy", "restartPolicy": "Always"}],
                "containers": [{"name": "app"}, {"name": "pending"}]
            },
            "status": {
                "initContainerStatuses": [
                    status("migrate", json!({"terminated": {"exitCode": 0}}), json!({})),
                    status("proxy", json!({"running": {}}), json!({})),
                ],
                "containerStatuses": [
                    status("app", json!({"waiting": {"reason": "CrashLoopBackOff"}}),
                           json!({"terminated": {"exitCode": 1}})),
                    status("pending", json!({"waiting": {"reason": "ContainerCreating"}}), json!({})),
                ]
            }
        }));
        let info = PodInfo::from_pod(&p);
        assert_eq!((info.name.as_str(), info.uid.as_str()), ("web-1", "u1"));
        let summary: Vec<_> = info
            .containers
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.role,
                    c.started,
                    c.running,
                    c.has_previous,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("migrate", ContainerRole::Init, true, false, false),
                ("proxy", ContainerRole::Sidecar, true, true, false),
                ("app", ContainerRole::Regular, true, false, true),
                ("pending", ContainerRole::Regular, false, false, false),
            ]
        );
        assert_eq!(info.containers[2].restarts, 2);

        let all: Vec<_> = info.selected(None).map(|c| c.name.as_str()).collect();
        assert_eq!(
            all,
            ["proxy", "app", "pending"],
            "init containers only when named"
        );
        let named: Vec<_> = info.selected(Some("migrate")).map(|c| &c.name).collect();
        assert_eq!(named, ["migrate"]);
        assert_eq!(info.selected(Some("missing")).count(), 0);

        // A pod without a status yet has nothing started.
        let new = PodInfo::from_pod(&pod(json!({"spec": {"containers": [{"name": "app"}]}})));
        assert!(!new.containers[0].started);
    }

    // covers UP-45
    #[test]
    fn workload_selectors_become_label_selectors() {
        let selector: LabelSelector = serde_json::from_value(json!({
            "matchLabels": {"app": "web"},
            "matchExpressions": [
                {"key": "tier", "operator": "In", "values": ["a", "b"]},
                {"key": "canary", "operator": "DoesNotExist"}
            ]
        }))
        .unwrap();
        assert_eq!(
            selector_string(WorkloadKind::Deployment, "web", Some(selector)).unwrap(),
            "app=web,tier in (a,b),!canary"
        );
        let empty = selector_string(WorkloadKind::Job, "j", Some(LabelSelector::default()));
        let empty = empty.unwrap_err();
        assert!(empty.is_fatal());
        assert_eq!(empty.message, "Job \"j\" has no pod selector");
        assert!(selector_string(WorkloadKind::Job, "j", None).is_err());
    }

    // covers UP-47: a shell into a workload opens in its first ready pod, in the container
    // kubectl would pick.
    #[test]
    fn a_workload_shell_picks_the_first_ready_pod() {
        let ready = |name: &str, ready: &str| {
            pod(json!({
                "metadata": {"name": name},
                "spec": {"initContainers": [{"name": "init"}],
                         "containers": [{"name": "app"}, {"name": "proxy"}]},
                "status": {"conditions": [{"type": "Ready", "status": ready}]}
            }))
        };
        let web_b = WorkloadPod::from_pod(&ready("web-b", "True"));
        assert_eq!(web_b.containers, ["app", "proxy"]);
        assert_eq!(web_b.default_container.as_deref(), Some("app"));
        assert!(web_b.ready);
        let web_a = WorkloadPod::from_pod(&ready("web-a", "False"));
        assert!(!web_a.ready);
        let web_c = WorkloadPod::from_pod(&ready("web-c", "True"));
        let pods = [web_c.clone(), web_a.clone(), web_b.clone()];
        assert_eq!(WorkloadPod::first_ready(&pods), Some(&web_b));
        assert_eq!(WorkloadPod::first_ready(&[web_a]), None);

        // A pod being deleted is not ready; the annotation names the default container.
        let leaving = pod(json!({
            "metadata": {"name": "web-0", "deletionTimestamp": "2024-05-01T12:00:00Z"},
            "spec": {"containers": [{"name": "app"}]},
            "status": {"conditions": [{"type": "Ready", "status": "True"}]}
        }));
        assert!(!WorkloadPod::from_pod(&leaving).ready);
        let annotated = pod(json!({
            "metadata": {"name": "a", "annotations": {
                "kubectl.kubernetes.io/default-container": "proxy"}},
            "spec": {"containers": [{"name": "app"}, {"name": "proxy"}]}
        }));
        let annotated = WorkloadPod::from_pod(&annotated);
        assert_eq!(annotated.default_container.as_deref(), Some("proxy"));
        assert!(!annotated.ready, "no Ready condition yet");
    }

    #[test]
    fn workload_kinds_come_from_resource_kinds() {
        assert_eq!(
            WorkloadKind::from_kind(&KindId::new("apps", "deployments")),
            Some(WorkloadKind::Deployment)
        );
        assert_eq!(
            WorkloadKind::from_kind(&KindId::new("batch", "jobs")),
            Some(WorkloadKind::Job)
        );
        assert_eq!(WorkloadKind::from_kind(&KindId::new("", "pods")), None);
        assert_eq!(WorkloadKind::StatefulSet.name(), "StatefulSet");
    }

    // covers UP-45
    #[test]
    fn watch_errors_say_whether_to_retry() {
        let status = |code: u16| {
            Box::new(kube::core::Status {
                code,
                message: "m".into(),
                reason: "R".into(),
                ..Default::default()
            })
        };
        assert_eq!(watch_error(watcher::Error::WatchError(status(410))), None);
        let forbidden = watch_error(watcher::Error::InitialListFailed(kube::Error::Api(status(
            403,
        ))))
        .unwrap();
        assert!(forbidden.is_fatal() && !forbidden.is_cluster());
        let flaky = watch_error(watcher::Error::WatchError(status(500))).unwrap();
        assert!(!flaky.is_fatal());
        assert!(
            !watch_error(watcher::Error::NoResourceVersion)
                .unwrap()
                .is_fatal()
        );
    }

    #[test]
    fn watcher_events_map_one_to_one() {
        let p = pod(json!({"metadata": {"name": "a", "uid": "1"}}));
        let info = PodInfo::from_pod(&p);
        assert_eq!(pod_event(watcher::Event::Init), PodEvent::Init);
        assert_eq!(pod_event(watcher::Event::InitDone), PodEvent::InitDone);
        assert_eq!(
            pod_event(watcher::Event::Apply(p.clone())),
            PodEvent::Apply(info.clone())
        );
        assert_eq!(
            pod_event(watcher::Event::InitApply(p.clone())),
            PodEvent::InitApply(info.clone())
        );
        assert_eq!(pod_event(watcher::Event::Delete(p)), PodEvent::Delete(info));
    }
}
