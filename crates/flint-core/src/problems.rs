//! One definition of what counts as a problem, shared by lists, dashboards and notifications.
//!
//! Classification is pure and synchronous: typed Kubernetes objects and a caller-supplied clock
//! go in, typed [`Problem`]s come out. Nothing here touches the network, and each object is
//! looked at once, so a pass over a large list is linear and allocates only for what it reports.
//!
//! # Identity
//!
//! [`ProblemId`] names the cluster, the kind of problem and the exact object (by UID, so a
//! recreated object is a different one). It holds nothing that changes while a problem lasts:
//! no restart count, reason, owner or time. A notifier that keeps the ids of the last pass can tell:
//!
//! - **new**: the id was not in the last pass and has not been seen before;
//! - **already reported**: the id was in the last pass;
//! - **cleared**: the id was in the last pass and is not in this one;
//! - **came back**: the id is in this pass, not in the last, but was seen earlier.
//!
//! A crash-looping container alternates between running and back-off, so the rules below keep
//! reporting it through the short runs. Otherwise it would clear and come back on every restart.
//!
//! # Text
//!
//! [`Problem::summary`] is Flint's own wording and returns `&'static str`, so it cannot carry a
//! server-supplied message. Reasons are read only by matching known Kubernetes values
//! ([`ProblemReason`]); any other value is dropped.

// module-tests: problems_tests.rs

use std::collections::HashMap;
use std::sync::Arc;

use k8s_openapi::api::apps::v1::{
    DaemonSet, Deployment, ReplicaSet, StatefulSet, StatefulSetSpec, StatefulSetStatus,
};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ContainerStateTerminated, ContainerStatus, Node, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference, Time};
use k8s_openapi::jiff::{SignedDuration, Timestamp};

use crate::thresholds::ThresholdMetric;

/// Restarts a container needs before a back-off counts as a crash loop.
///
/// The kubelet reports `CrashLoopBackOff` from the first failed restart, which also happens to
/// containers that fail once while a dependency comes up. With back-off delays of 10, 20 and
/// 40 seconds, three restarts mean the container has kept failing for over a minute.
pub const CRASH_LOOP_MIN_RESTARTS: i32 = 3;

/// How long a container must run to count as recovered from a crash loop.
///
/// Matches the kubelet, which resets a container's back-off after ten minutes of running. A
/// shorter run keeps the problem open between restarts; a longer previous run means the latest
/// exit was a one-off, however many restarts have added up over the container's life.
pub const CRASH_LOOP_STABLE_AFTER: SignedDuration = SignedDuration::from_secs(10 * 60);

/// The longest gap between one exit and the next start that still counts as the same crash loop.
///
/// The kubelet's restart back-off tops out at five minutes. The extra minute covers its sync
/// delay and a routine image check, so a loop at full back-off stays one problem, while a start
/// after a long outage such as an image pull failure does not continue an old loop.
pub const CRASH_LOOP_MAX_RESTART_GAP: SignedDuration = SignedDuration::from_secs(6 * 60);

/// How long after an OOM kill it is still reported.
///
/// An OOM kill is an event, but the container status remembers it until the next restart, which
/// may be weeks away. One hour keeps it visible without alarming about old history.
pub const OOM_RECENT_WINDOW: SignedDuration = SignedDuration::from_secs(60 * 60);

/// How long a node must stay not ready before it is reported.
///
/// Longer than the control plane's node-monitor grace period (under a minute by default) and
/// than a routine kubelet restart, so a brief blip does not raise an alarm.
pub const NODE_NOT_READY_GRACE: SignedDuration = SignedDuration::from_secs(2 * 60);

/// How long after registering a node may be not ready without being reported.
///
/// A joining node is not ready until its network plugin and kubelet finish starting, which can
/// take several minutes on a cloud instance.
pub const NODE_JOIN_GRACE: SignedDuration = SignedDuration::from_secs(10 * 60);

/// How long after creation a pod's image pull may fail without being reported.
///
/// The kubelet retries after 10, 20 and 40 seconds, so a brief registry error clears inside this
/// window. Every retried failure is held back alike, so a problem does not flap between them,
/// including a missing image that may still be loaded onto the node. An invalid image name can
/// never succeed and is reported at once.
pub const IMAGE_PULL_GRACE: SignedDuration = SignedDuration::from_secs(60);

/// How long after creation a workload may be short of replicas without being reported.
///
/// Covers a first start, including pulling images and passing readiness probes. StatefulSet and
/// DaemonSet status carries no timestamps, so their age is the only clock available.
pub const WORKLOAD_START_GRACE: SignedDuration = SignedDuration::from_secs(5 * 60);

/// How long a settled Deployment must report `Available=False` before it is reported.
///
/// Long enough for a replacement pod to start after a pod or node is lost.
pub const DEPLOYMENT_UNAVAILABLE_GRACE: SignedDuration = SignedDuration::from_secs(2 * 60);

/// How long a caller should keep seeing a StatefulSet or DaemonSet shortfall before alerting.
///
/// Their status has no timestamps and cannot separate a stuck pod from the last pod of a rollout
/// still starting, so a single pass cannot apply a grace. See [`Problem::hold_down`].
pub const WORKLOAD_HOLD_DOWN: SignedDuration = SignedDuration::from_secs(2 * 60);

/// What kind of problem an object has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProblemKind {
    /// A container keeps failing and being restarted.
    CrashLoop,
    /// A container was recently killed for exceeding its memory limit, and is not crash-looping.
    OomKilled,
    /// A node has been not ready for longer than its grace periods.
    NodeNotReady,
    /// A Job has finished as failed.
    JobFailed,
    /// A container cannot start because its image cannot be pulled.
    ImagePull,
    /// A workload has settled with fewer ready replicas than it wants.
    WorkloadShort,
    /// A pod has stayed Pending past its threshold, with no more specific problem.
    PodPending,
}

impl ProblemKind {
    /// How urgent this kind of problem is.
    pub const fn severity(self) -> ProblemSeverity {
        match self {
            Self::CrashLoop | Self::NodeNotReady | Self::ImagePull => ProblemSeverity::Critical,
            Self::OomKilled | Self::JobFailed | Self::WorkloadShort | Self::PodPending => {
                ProblemSeverity::Warning
            }
        }
    }
}

/// How urgent a problem is. Orders from least to most urgent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProblemSeverity {
    /// Something failed, but the workload or cluster may still be serving.
    Warning,
    /// Something is down or keeps failing right now.
    Critical,
}

/// A reason code from the known Kubernetes values. Anything else is not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProblemReason {
    /// The kubelet is backing off restarts of a failing container.
    CrashLoopBackOff,
    /// The container exceeded its memory limit.
    OomKilled,
    /// The kubelet reports the node as not ready.
    KubeletNotReady,
    /// The control plane stopped hearing from the node.
    NodeStatusUnknown,
    /// The Job used up its retries.
    BackoffLimitExceeded,
    /// The Job ran past its active deadline.
    DeadlineExceeded,
    /// A pod failure policy failed the Job.
    PodFailurePolicy,
    /// Some indexes of an indexed Job failed.
    FailedIndexes,
    /// An indexed Job passed its limit of failed indexes.
    MaxFailedIndexesExceeded,
    /// The latest image pull failed.
    ErrImagePull,
    /// The kubelet is backing off retries of a failing image pull.
    ImagePullBackOff,
    /// The container's image name cannot be parsed.
    InvalidImageName,
    /// The image is not on the node and the pull policy forbids pulling it.
    ErrImageNeverPull,
    /// The image registry could not be reached.
    RegistryUnavailable,
    /// The image failed signature validation.
    SignatureValidationFailed,
    /// A Deployment rollout stopped making progress within its deadline.
    ProgressDeadlineExceeded,
    /// A Deployment has fewer available replicas than its update strategy requires.
    MinimumReplicasUnavailable,
}

impl ProblemReason {
    /// The Kubernetes spelling of this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CrashLoopBackOff => "CrashLoopBackOff",
            Self::OomKilled => OOM_KILLED,
            Self::KubeletNotReady => "KubeletNotReady",
            Self::NodeStatusUnknown => "NodeStatusUnknown",
            Self::BackoffLimitExceeded => "BackoffLimitExceeded",
            Self::DeadlineExceeded => "DeadlineExceeded",
            Self::PodFailurePolicy => "PodFailurePolicy",
            Self::FailedIndexes => "FailedIndexes",
            Self::MaxFailedIndexesExceeded => "MaxFailedIndexesExceeded",
            Self::ErrImagePull => "ErrImagePull",
            Self::ImagePullBackOff => "ImagePullBackOff",
            Self::InvalidImageName => "InvalidImageName",
            Self::ErrImageNeverPull => "ErrImageNeverPull",
            Self::RegistryUnavailable => "RegistryUnavailable",
            Self::SignatureValidationFailed => "SignatureValidationFailed",
            Self::ProgressDeadlineExceeded => PROGRESS_DEADLINE_EXCEEDED,
            Self::MinimumReplicasUnavailable => "MinimumReplicasUnavailable",
        }
    }

    fn of_image_pull(reason: Option<&str>) -> Option<Self> {
        match reason? {
            "ErrImagePull" => Some(Self::ErrImagePull),
            "ImagePullBackOff" => Some(Self::ImagePullBackOff),
            "InvalidImageName" => Some(Self::InvalidImageName),
            "ErrImageNeverPull" => Some(Self::ErrImageNeverPull),
            "RegistryUnavailable" => Some(Self::RegistryUnavailable),
            "SignatureValidationFailed" => Some(Self::SignatureValidationFailed),
            _ => None,
        }
    }

    fn of_deployment_available(reason: Option<&str>) -> Option<Self> {
        (reason? == "MinimumReplicasUnavailable").then_some(Self::MinimumReplicasUnavailable)
    }

    fn of_node(reason: Option<&str>) -> Option<Self> {
        match reason? {
            "KubeletNotReady" => Some(Self::KubeletNotReady),
            "NodeStatusUnknown" => Some(Self::NodeStatusUnknown),
            _ => None,
        }
    }

    fn of_job(reason: Option<&str>) -> Option<Self> {
        match reason? {
            "BackoffLimitExceeded" => Some(Self::BackoffLimitExceeded),
            "DeadlineExceeded" => Some(Self::DeadlineExceeded),
            "PodFailurePolicy" => Some(Self::PodFailurePolicy),
            "FailedIndexes" => Some(Self::FailedIndexes),
            "MaxFailedIndexesExceeded" => Some(Self::MaxFailedIndexesExceeded),
            _ => None,
        }
    }
}

/// A kind of workload that keeps a number of replicas running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WorkloadKind {
    Deployment,
    StatefulSet,
    DaemonSet,
}

/// The object a problem concerns. UIDs are optional only because the API types make them so.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProblemSubject {
    /// One container of a pod, including init, sidecar and ephemeral containers.
    Container {
        namespace: String,
        pod: String,
        pod_uid: Option<String>,
        container: String,
    },
    /// A pod as a whole.
    Pod {
        namespace: String,
        name: String,
        uid: Option<String>,
    },
    /// A node.
    Node { name: String, uid: Option<String> },
    /// A Job.
    Job {
        namespace: String,
        name: String,
        uid: Option<String>,
    },
    /// A Deployment, StatefulSet or DaemonSet.
    Workload {
        kind: WorkloadKind,
        namespace: String,
        name: String,
        uid: Option<String>,
    },
}

/// The controller a problem belongs to, for grouping.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProblemOwner {
    /// The API group, empty for the core group.
    pub group: String,
    pub kind: String,
    pub name: String,
}

impl ProblemOwner {
    /// The controller of an object, seen through a ReplicaSet to its Deployment.
    ///
    /// A pod names only its ReplicaSet, which changes on each rollout. The Deployment is named
    /// only when `replica_set` is that ReplicaSet's metadata, matched by name, UID and namespace,
    /// and carries a controller owner reference of kind Deployment: an orphaned ReplicaSet keeps
    /// its name and pod template hash, so those prove nothing. Without verified metadata the
    /// ReplicaSet is the owner.
    pub fn of_meta(meta: &ObjectMeta, replica_set: Option<&ObjectMeta>) -> Option<Self> {
        let owner = controller_of(meta)?;
        let group = api_group(&owner.api_version);
        if group == "apps" && owner.kind == "ReplicaSet" {
            let deployment = replica_set
                .filter(|rs| {
                    rs.name.as_deref() == Some(owner.name.as_str())
                        && rs.uid.as_deref() == Some(owner.uid.as_str())
                        && rs.namespace == meta.namespace
                })
                .and_then(deployment_of);
            if let Some(deployment) = deployment {
                return Some(Self::workload(WorkloadKind::Deployment, deployment));
            }
        }
        Some(Self {
            group: group.to_owned(),
            kind: owner.kind.clone(),
            name: owner.name.clone(),
        })
    }

    /// A Deployment, StatefulSet or DaemonSet as an owner.
    pub fn workload(kind: WorkloadKind, name: &str) -> Self {
        let kind = match kind {
            WorkloadKind::Deployment => "Deployment",
            WorkloadKind::StatefulSet => "StatefulSet",
            WorkloadKind::DaemonSet => "DaemonSet",
        };
        Self {
            group: "apps".to_owned(),
            kind: kind.to_owned(),
            name: name.to_owned(),
        }
    }
}

/// The outcome of reading a cluster's ReplicaSets, for [`ReplicaSetIndex::from_read`].
#[derive(Clone, Copy, Debug)]
pub enum ReplicaSetRead<'a> {
    /// Every ReplicaSet the pods being classified could belong to.
    Complete(&'a [ReplicaSet]),
    /// Some ReplicaSets, such as a list that failed part way through its pages. A ReplicaSet
    /// missing from these says nothing about the pod that names it, so they are not used.
    Partial(&'a [ReplicaSet]),
    /// The read failed.
    Failed,
}

/// ReplicaSet metadata for resolving pods to their Deployments, or the fact that there is none.
///
/// Build one with [`Self::from_read`], or [`Self::unavailable`] when the ReplicaSets could not be
/// read. There is no default, so a caller has to say which it is. A pod's owner is known only if a
/// complete read holds its ReplicaSet; see [`Problem::owner_known`].
///
/// Looked up by the UID in the pod's owner reference, within the pod's namespace, so a ReplicaSet
/// recreated under the same name or one in another namespace is never mistaken for the owner.
/// ReplicaSets without a UID cannot be matched and are left out.
#[derive(Clone, Debug)]
pub struct ReplicaSetIndex<'a> {
    /// `None` when the ReplicaSets could not be read.
    by_uid: Option<HashMap<&'a str, &'a ObjectMeta>>,
}

/// Test fixtures hold bare metadata. Production code goes through [`ReplicaSetIndex::from_read`],
/// so that collecting a partial list cannot state that it is complete.
#[cfg(test)]
impl<'a> FromIterator<&'a ObjectMeta> for ReplicaSetIndex<'a> {
    fn from_iter<I: IntoIterator<Item = &'a ObjectMeta>>(replica_sets: I) -> Self {
        Self::complete(replica_sets)
    }
}

impl<'a> ReplicaSetIndex<'a> {
    /// The index for the outcome of a read. Only a complete read can resolve an owner: a partial
    /// or failed one is [`Self::unavailable`], never an empty set of owners.
    pub fn from_read(read: ReplicaSetRead<'a>) -> Self {
        match read {
            ReplicaSetRead::Complete(replica_sets) => {
                Self::complete(replica_sets.iter().map(|replica_set| &replica_set.metadata))
            }
            ReplicaSetRead::Partial(_) | ReplicaSetRead::Failed => Self::unavailable(),
        }
    }

    fn complete(replica_sets: impl IntoIterator<Item = &'a ObjectMeta>) -> Self {
        let by_uid = replica_sets
            .into_iter()
            .filter_map(|meta| Some((meta.uid.as_deref()?, meta)))
            .collect();
        Self {
            by_uid: Some(by_uid),
        }
    }
}

impl ReplicaSetIndex<'_> {
    /// The ReplicaSets could not be read in this pass.
    pub fn unavailable() -> Self {
        Self { by_uid: None }
    }

    /// The owner of `pod`, and whether it is known.
    fn resolve(&self, pod: &ObjectMeta) -> (Option<ProblemOwner>, bool) {
        let Some(controller) = controller_of(pod) else {
            return (None, true);
        };
        if controller.kind != "ReplicaSet" || api_group(&controller.api_version) != "apps" {
            return (ProblemOwner::of_meta(pod, None), true);
        }
        let replica_set = self
            .by_uid
            .as_ref()
            .and_then(|read| read.get(controller.uid.as_str()).copied())
            .filter(|rs| {
                rs.namespace == pod.namespace && rs.name.as_deref() == Some(&controller.name)
            });
        (
            ProblemOwner::of_meta(pod, replica_set),
            replica_set.is_some(),
        )
    }
}

/// The stable identity of a problem; see the [module docs](self#identity).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProblemId {
    /// The caller-supplied cluster. Shared, so a large pass does not copy it per problem.
    pub cluster: Arc<str>,
    pub kind: ProblemKind,
    pub subject: ProblemSubject,
}

/// One current problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub id: ProblemId,
    /// The workload to group this under. Never part of the identity.
    ///
    /// For a container, its pod's controller, resolved to the Deployment when the ReplicaSet's
    /// metadata was supplied. For a Job, its controller, such as a CronJob. For a Deployment,
    /// StatefulSet or DaemonSet, itself. `None` for a node or an object with no controller.
    pub owner: Option<ProblemOwner>,
    /// Whether `owner` is the real controller. False for a pod whose ReplicaSet is not in a
    /// complete read: `owner` then names that ReplicaSet, though a Deployment may control it, and
    /// must not be used as an identity. The alert engine treats the kind as unchecked.
    pub owner_known: bool,
    /// The known reason code, if the object gave one.
    pub reason: Option<ProblemReason>,
    /// When this was last known to start, if the object says.
    ///
    /// For a crash loop or OOM kill, the latest termination, so it moves on each restart. For a
    /// node, Job or Deployment, the last transition of its condition. For a stuck Pending pod,
    /// its creation. Image pull failures and
    /// StatefulSet or DaemonSet shortfalls have no time in their status, so this is `None`.
    /// Never part of the identity.
    pub since: Option<Timestamp>,
}

impl Problem {
    /// The kind of problem.
    pub fn kind(&self) -> ProblemKind {
        self.id.kind
    }

    /// How urgent the problem is.
    pub fn severity(&self) -> ProblemSeverity {
        self.id.kind.severity()
    }

    /// How long this id must stay present across passes before it is worth alerting on, when the
    /// object's status has no time to judge that from. The caller tracks when it first saw the id
    /// and must apply this: a DaemonSet placing a pod on a new node looks the same as one whose
    /// pod admission rejects, and only the hold-down tells them apart.
    pub fn hold_down(&self) -> Option<SignedDuration> {
        matches!(
            self.id.subject,
            ProblemSubject::Workload {
                kind: WorkloadKind::StatefulSet | WorkloadKind::DaemonSet,
                ..
            }
        )
        .then_some(WORKLOAD_HOLD_DOWN)
    }

    /// Flint's description of the problem. Never contains server-supplied text.
    pub fn summary(&self) -> &'static str {
        use ProblemReason as R;
        match (self.id.kind, self.reason) {
            (ProblemKind::CrashLoop, Some(R::OomKilled)) => {
                "Container keeps restarting after running out of memory"
            }
            (ProblemKind::CrashLoop, _) => "Container keeps crashing and restarting",
            (ProblemKind::OomKilled, _) => {
                "Container was recently killed for using too much memory"
            }
            (ProblemKind::NodeNotReady, Some(R::NodeStatusUnknown)) => {
                "Node has stopped reporting its status"
            }
            (ProblemKind::NodeNotReady, _) => "Node is not ready",
            (ProblemKind::JobFailed, Some(R::BackoffLimitExceeded)) => {
                "Job failed after using up its retries"
            }
            (ProblemKind::JobFailed, Some(R::DeadlineExceeded)) => {
                "Job failed by running past its deadline"
            }
            (ProblemKind::JobFailed, Some(R::PodFailurePolicy)) => {
                "Job failed under its pod failure policy"
            }
            (ProblemKind::JobFailed, Some(R::FailedIndexes | R::MaxFailedIndexesExceeded)) => {
                "Indexed Job failed on some of its indexes"
            }
            (ProblemKind::JobFailed, _) => "Job failed",
            (ProblemKind::ImagePull, Some(R::InvalidImageName)) => {
                "Container image name is invalid"
            }
            (ProblemKind::ImagePull, Some(R::ErrImageNeverPull)) => {
                "Container image is missing and its pull policy forbids pulling it"
            }
            (ProblemKind::ImagePull, Some(R::SignatureValidationFailed)) => {
                "Container image failed signature validation"
            }
            (ProblemKind::ImagePull, _) => "Container image cannot be pulled",
            (ProblemKind::WorkloadShort, Some(R::ProgressDeadlineExceeded)) => {
                "Rollout stopped before reaching its desired replicas"
            }
            (ProblemKind::WorkloadShort, _) => "Workload has fewer ready replicas than desired",
            (ProblemKind::PodPending, _) => "Pod has been Pending longer than its threshold",
        }
    }
}

/// Classifies objects of one cluster at one moment.
#[derive(Clone, Debug)]
pub struct Classifier {
    cluster: Arc<str>,
    now: Timestamp,
    pending_after: SignedDuration,
}

impl Classifier {
    /// A classifier for `cluster`, which is only copied into results, judging ages against `now`.
    /// A pod counts as stuck Pending after the default threshold.
    pub fn new(cluster: impl Into<Arc<str>>, now: Timestamp) -> Self {
        let seconds = ThresholdMetric::PendingPodSeconds.default_value();
        Self {
            cluster: cluster.into(),
            now,
            pending_after: SignedDuration::from_secs(seconds.into()),
        }
    }

    /// This classifier with a pod counting as stuck once Pending for longer than `seconds`.
    #[must_use]
    pub fn with_pending_after(mut self, seconds: u32) -> Self {
        self.pending_after = SignedDuration::from_secs(seconds.into());
        self
    }

    /// The problems of every pod during a ReplicaSet outage. Test-only, so production code has to
    /// choose an index.
    #[cfg(test)]
    pub(crate) fn pods<'a>(&self, pods: impl IntoIterator<Item = &'a Pod>) -> Vec<Problem> {
        self.pods_with_replica_sets(pods, &ReplicaSetIndex::unavailable())
    }

    /// The problems of every pod, in order, resolving owners through `replica_sets`.
    pub fn pods_with_replica_sets<'a>(
        &self,
        pods: impl IntoIterator<Item = &'a Pod>,
        replica_sets: &ReplicaSetIndex<'_>,
    ) -> Vec<Problem> {
        let mut out = Vec::new();
        for pod in pods {
            self.pod_with_replica_sets(pod, replica_sets, &mut out);
        }
        out
    }

    /// Appends the problems of one pod, at most one per container, resolving its owner through
    /// `replica_sets`.
    ///
    /// A deleting or succeeded pod has none. A failed pod will not restart, so it can only report
    /// an OOM kill that ended its final run. The same holds for ephemeral containers, which are
    /// never restarted, so a debug container with a mistyped image does not mark its pod.
    pub fn pod_with_replica_sets(
        &self,
        pod: &Pod,
        replica_sets: &ReplicaSetIndex<'_>,
        out: &mut Vec<Problem>,
    ) {
        let meta = &pod.metadata;
        let (Some(name), Some(status), None) = (&meta.name, &pod.status, &meta.deletion_timestamp)
        else {
            return;
        };
        let phase = status.phase.as_deref();
        if phase == Some("Succeeded") {
            return;
        }
        let finished = phase == Some("Failed");
        let found_before = out.len();
        let pull_grace = meta
            .creation_timestamp
            .as_ref()
            .is_some_and(|created| self.within(created.0, IMAGE_PULL_GRACE));
        let init = status.init_container_statuses.iter().flatten();
        let restartable = init
            .chain(status.container_statuses.iter().flatten())
            .map(|container| (container, finished));
        let ephemeral = status
            .ephemeral_container_statuses
            .iter()
            .flatten()
            .map(|container| (container, true));
        for (container, final_run) in restartable.chain(ephemeral) {
            let Some((kind, reason, since)) =
                self.container(container, pod.spec.as_ref(), final_run, pull_grace)
            else {
                continue;
            };
            let subject = ProblemSubject::Container {
                namespace: meta.namespace.clone().unwrap_or_default(),
                pod: name.clone(),
                pod_uid: meta.uid.clone(),
                container: container.name.clone(),
            };
            let (owner, owner_known) = replica_sets.resolve(meta);
            let problem = self.problem(kind, subject, owner, reason, since);
            out.push(Problem {
                owner_known,
                ..problem
            });
        }
        // A container's own problem says more, and one failure is not counted twice.
        if phase == Some("Pending") && out.len() == found_before {
            let created = meta.creation_timestamp.as_ref().map(|created| created.0);
            if let Some(since) =
                created.filter(|c| self.now.duration_since(*c) > self.pending_after)
            {
                let subject = ProblemSubject::Pod {
                    namespace: meta.namespace.clone().unwrap_or_default(),
                    name: name.clone(),
                    uid: meta.uid.clone(),
                };
                let (owner, owner_known) = replica_sets.resolve(meta);
                let problem =
                    self.problem(ProblemKind::PodPending, subject, owner, None, Some(since));
                out.push(Problem {
                    owner_known,
                    ..problem
                });
            }
        }
    }

    /// The problem of one node, if it has been not ready past both grace periods.
    ///
    /// A node with no `Ready` condition counts as not ready once it is past joining.
    pub fn node(&self, node: &Node) -> Option<Problem> {
        let meta = &node.metadata;
        let name = meta.name.as_ref()?;
        if meta.deletion_timestamp.is_some() {
            return None;
        }
        let conditions = node.status.as_ref()?.conditions.iter().flatten();
        let ready = conditions.into_iter().find(|c| c.type_ == "Ready");
        if ready.is_some_and(|c| c.status == "True") {
            return None;
        }
        if meta
            .creation_timestamp
            .as_ref()
            .is_some_and(|created| self.within(created.0, NODE_JOIN_GRACE))
        {
            return None;
        }
        let since = ready.and_then(|c| time(&c.last_transition_time));
        if since.is_some_and(|since| self.within(since, NODE_NOT_READY_GRACE)) {
            return None;
        }
        let subject = ProblemSubject::Node {
            name: name.clone(),
            uid: meta.uid.clone(),
        };
        let reason = ProblemReason::of_node(ready.and_then(|c| c.reason.as_deref()));
        Some(self.problem(ProblemKind::NodeNotReady, subject, None, reason, since))
    }

    /// The problem of one Job, if it has finished as failed.
    ///
    /// Only the terminal `Failed` condition counts: `FailureTarget` means the Job is still
    /// stopping its pods.
    pub fn job(&self, job: &Job) -> Option<Problem> {
        let meta = &job.metadata;
        let name = meta.name.as_ref()?;
        if meta.deletion_timestamp.is_some() {
            return None;
        }
        let conditions = job.status.as_ref()?.conditions.as_ref()?;
        let failed = conditions
            .iter()
            .find(|c| c.type_ == "Failed" && c.status == "True")?;
        let subject = ProblemSubject::Job {
            namespace: meta.namespace.clone().unwrap_or_default(),
            name: name.clone(),
            uid: meta.uid.clone(),
        };
        let reason = ProblemReason::of_job(failed.reason.as_deref());
        let since = time(&failed.last_transition_time);
        let owner = ProblemOwner::of_meta(meta, None);
        Some(self.problem(ProblemKind::JobFailed, subject, owner, reason, since))
    }

    /// The problem of one Deployment, if it has settled short of its replicas.
    ///
    /// A paused Deployment is left alone. While a rollout or scale-up is moving, the controller's
    /// own progress deadline decides, and a rollout past it is reported until its new replicas
    /// have replaced the old ones, even while old pods keep every replica available. Once
    /// settled, `Available=False` must outlast [`DEPLOYMENT_UNAVAILABLE_GRACE`]; a shortfall
    /// within the update strategy's `maxUnavailable` keeps `Available=True` and is not reported.
    pub fn deployment(&self, deployment: &Deployment) -> Option<Problem> {
        let meta = &deployment.metadata;
        let spec = &deployment.spec;
        let status = deployment.status.as_ref()?;
        let desired = spec.replicas.unwrap_or(DEFAULT_REPLICAS);
        if spec.paused == Some(true)
            || !self.may_be_short(meta, desired, status.observed_generation)
        {
            return None;
        }
        let available = status.available_replicas.unwrap_or(0);
        let updated = status.updated_replicas.unwrap_or(0);
        let caught_up =
            available >= desired && updated >= desired && status.replicas.unwrap_or(0) <= updated;
        let conditions = status.conditions.as_deref().unwrap_or_default();
        let condition = |type_: &str| {
            conditions
                .iter()
                .find(|c| c.type_.as_deref() == Some(type_))
        };
        let progressing = condition("Progressing");
        if let Some(stalled) = progressing.filter(|c| {
            !caught_up
                && c.status.as_deref() == Some("False")
                && c.reason.as_deref() == Some(PROGRESS_DEADLINE_EXCEEDED)
        }) {
            let since = time(&stalled.last_transition_time);
            let reason = Some(ProblemReason::ProgressDeadlineExceeded);
            return self.workload(WorkloadKind::Deployment, meta, reason, since);
        }
        if available >= desired {
            return None;
        }
        let moving = match progressing {
            // `False` is a stop, such as a quota refusing new pods, so it has settled too.
            Some(c) => {
                c.status.as_deref() != Some("False")
                    && c.reason.as_deref() != Some("NewReplicaSetAvailable")
            }
            // With the progress deadline disabled there is no condition to read.
            None => updated < desired,
        };
        if moving {
            return None;
        }
        let unavailable =
            condition("Available").filter(|c| c.status.as_deref() == Some("False"))?;
        let since = time(&unavailable.last_transition_time);
        if since.is_some_and(|since| self.within(since, DEPLOYMENT_UNAVAILABLE_GRACE)) {
            return None;
        }
        let reason = ProblemReason::of_deployment_available(unavailable.reason.as_deref());
        self.workload(WorkloadKind::Deployment, meta, reason, since)
    }

    /// The problem of one StatefulSet, if it has settled short of ready replicas.
    ///
    /// Readiness is compared rather than availability, which older API servers omit.
    ///
    /// A missing pod counts unless an ordered scale-up is waiting for an existing pod to become
    /// ready. With every existing pod ready the controller creates the next one at once, so a
    /// gap that lasts means it cannot, as under a pod quota. [`Problem::hold_down`] covers the
    /// moment between two creations.
    pub fn stateful_set(&self, set: &StatefulSet) -> Option<Problem> {
        let meta = &set.metadata;
        let spec = &set.spec;
        let status = set.status.as_ref()?;
        let desired = spec.replicas.unwrap_or(DEFAULT_REPLICAS);
        let ready = status.ready_replicas.unwrap_or(0);
        let ordered = spec.pod_management_policy.as_deref() != Some(PARALLEL);
        if !self.may_be_short(meta, desired, status.observed_generation)
            || ready >= desired
            || (ordered && status.replicas < desired && ready < status.replicas)
            || is_rolling_out(spec, status, desired)
        {
            return None;
        }
        self.workload(WorkloadKind::StatefulSet, meta, None, None)
    }

    /// The problem of one DaemonSet, if it has settled short of ready pods.
    ///
    /// A rolling update is not reported. Neither is a set whose pods lag the template while some
    /// nodes have none, since `OnDelete` or a rollout may be the cause.
    ///
    /// A missing pod is reported when every existing pod is on the current revision. A normal
    /// placement on a new node has the same status as one admission keeps rejecting, so callers
    /// must apply [`Problem::hold_down`] before alerting on it.
    pub fn daemon_set(&self, set: &DaemonSet) -> Option<Problem> {
        let meta = &set.metadata;
        let status = set.status.as_ref()?;
        let desired = status.desired_number_scheduled;
        let current = status.current_number_scheduled;
        let updated = status.updated_number_scheduled.unwrap_or(0);
        let on_delete = set
            .spec
            .update_strategy
            .as_ref()
            .is_some_and(|strategy| strategy.type_.as_deref() == Some(ON_DELETE));
        if !self.may_be_short(meta, desired, status.observed_generation)
            || status.number_ready >= desired
            || (current < desired && updated < current)
            || (current >= desired && !on_delete && updated < desired)
        {
            return None;
        }
        self.workload(WorkloadKind::DaemonSet, meta, None, None)
    }

    /// Whether a workload's status can be judged at all: it is not deleting, wants replicas, is
    /// past its first start, and its controller has seen the latest spec.
    fn may_be_short(&self, meta: &ObjectMeta, desired: i32, observed: Option<i64>) -> bool {
        let starting = meta
            .creation_timestamp
            .as_ref()
            .is_some_and(|created| self.within(created.0, WORKLOAD_START_GRACE));
        let current = meta
            .generation
            .is_none_or(|generation| observed.is_some_and(|seen| seen >= generation));
        desired > 0 && meta.deletion_timestamp.is_none() && !starting && current
    }

    fn workload(
        &self,
        kind: WorkloadKind,
        meta: &ObjectMeta,
        reason: Option<ProblemReason>,
        since: Option<Timestamp>,
    ) -> Option<Problem> {
        let name = meta.name.clone()?;
        let owner = Some(ProblemOwner::workload(kind, &name));
        let subject = ProblemSubject::Workload {
            kind,
            namespace: meta.namespace.clone().unwrap_or_default(),
            name,
            uid: meta.uid.clone(),
        };
        Some(self.problem(ProblemKind::WorkloadShort, subject, owner, reason, since))
    }

    /// An image pull failure is the container's current state, so it comes before an OOM kill
    /// from an earlier run. A crash loop takes precedence over an OOM kill, so one failure is not
    /// counted twice.
    ///
    /// `final_run` means the container will not run again, so only its current state counts.
    /// `pull_grace` holds back image pull failures that are still likely to clear.
    fn container(
        &self,
        status: &ContainerStatus,
        spec: Option<&PodSpec>,
        final_run: bool,
        pull_grace: bool,
    ) -> Option<(ProblemKind, Option<ProblemReason>, Option<Timestamp>)> {
        if !final_run {
            let waiting = status.state.as_ref().and_then(|s| s.waiting.as_ref());
            let pull = waiting.and_then(|w| ProblemReason::of_image_pull(w.reason.as_deref()));
            if let Some(reason) =
                pull.filter(|r| !pull_grace || *r == ProblemReason::InvalidImageName)
            {
                return Some((ProblemKind::ImagePull, Some(reason), None));
            }
        }
        let current = status.state.as_ref().and_then(|s| s.terminated.as_ref());
        let previous = status
            .last_state
            .as_ref()
            .and_then(|s| s.terminated.as_ref());
        let latest = current.or(previous);
        if !final_run && self.is_crash_looping(status, spec, current, previous) {
            let reason = if latest.is_some_and(is_oom) {
                ProblemReason::OomKilled
            } else {
                ProblemReason::CrashLoopBackOff
            };
            let since = latest.and_then(|t| time(&t.finished_at));
            return Some((ProblemKind::CrashLoop, Some(reason), since));
        }
        // The last state of a final run is an earlier attempt, not what ended it.
        let last_run = if final_run { current } else { latest };
        let oom = last_run.filter(|t| is_oom(t))?;
        let since = time(&oom.finished_at).filter(|t| self.within(*t, OOM_RECENT_WINDOW))?;
        Some((
            ProblemKind::OomKilled,
            Some(ProblemReason::OomKilled),
            Some(since),
        ))
    }

    fn is_crash_looping(
        &self,
        status: &ContainerStatus,
        spec: Option<&PodSpec>,
        current: Option<&ContainerStateTerminated>,
        previous: Option<&ContainerStateTerminated>,
    ) -> bool {
        if status.restart_count < CRASH_LOOP_MIN_RESTARTS {
            return false;
        }
        let Some(state) = &status.state else {
            return false;
        };
        if let Some(waiting) = &state.waiting {
            // The kubelet's own verdict, unless the run before it was long enough to be healthy.
            return waiting.reason.as_deref() == Some("CrashLoopBackOff")
                && !previous.is_some_and(|t| ran_for(t).is_some_and(is_stable));
        }
        if let Some(running) = &state.running {
            // Between back-offs: still looping until this run outlasts the stable period, as
            // long as it follows directly from a short run.
            let Some(started) = time(&running.started_at) else {
                return false;
            };
            return self.within(started, CRASH_LOOP_STABLE_AFTER)
                && previous.is_some_and(|t| is_short_run_before(t, started));
        }
        // Just exited, before the kubelet reports the back-off. Only a loop if it follows another
        // short run, so a single failure is not one, and if it will restart.
        current.is_some_and(|t| {
            t.exit_code != 0
                && ran_for(t).is_some_and(|d| !is_stable(d))
                && time(&t.started_at).is_some_and(|started| {
                    previous.is_some_and(|p| is_short_run_before(p, started))
                })
                && spec.is_some_and(|spec| restarts_on_failure(spec, &status.name, t.exit_code))
        })
    }

    fn problem(
        &self,
        kind: ProblemKind,
        subject: ProblemSubject,
        owner: Option<ProblemOwner>,
        reason: Option<ProblemReason>,
        since: Option<Timestamp>,
    ) -> Problem {
        Problem {
            id: ProblemId {
                cluster: Arc::clone(&self.cluster),
                kind,
                subject,
            },
            owner,
            owner_known: true,
            reason,
            since,
        }
    }

    /// Whether `then` is less than `window` ago. A time ahead of `now` (clock skew) counts.
    fn within(&self, then: Timestamp, window: SignedDuration) -> bool {
        self.now.duration_since(then) < window
    }
}

/// The API group of an `apiVersion`, empty for the core group.
fn api_group(api_version: &str) -> &str {
    api_version.rsplit_once('/').map_or("", |(group, _)| group)
}

/// The owner reference marked as an object's controller.
fn controller_of(meta: &ObjectMeta) -> Option<&OwnerReference> {
    meta.owner_references
        .as_ref()?
        .iter()
        .find(|owner| owner.controller == Some(true))
}

/// The name of the Deployment controlling a ReplicaSet, if one does.
fn deployment_of(replica_set: &ObjectMeta) -> Option<&str> {
    controller_of(replica_set)
        .filter(|o| o.kind == "Deployment" && api_group(&o.api_version) == "apps")
        .map(|o| o.name.as_str())
}

/// The Kubernetes termination reason for an OOM kill.
const OOM_KILLED: &str = "OOMKilled";

/// The Deployment condition reason for a rollout that stopped making progress.
const PROGRESS_DEADLINE_EXCEEDED: &str = "ProgressDeadlineExceeded";

/// The update strategy under which pods change only when something else deletes them.
const ON_DELETE: &str = "OnDelete";

/// The StatefulSet pod management policy that creates pods without waiting for earlier ones.
const PARALLEL: &str = "Parallel";

/// The API's default when a Deployment or StatefulSet does not set `replicas`.
const DEFAULT_REPLICAS: i32 = 1;

/// Whether a StatefulSet is still moving replicas to its update revision.
///
/// Without a partition, the controller makes the revisions equal only once every replica is
/// updated and ready, so differing revisions mean the rollout is still running. `OnDelete` never
/// moves replicas itself, and a partition holds the lower ordinals back for good, so its
/// revisions can differ indefinitely and only the replicas it will update are counted.
fn is_rolling_out(spec: &StatefulSetSpec, status: &StatefulSetStatus, desired: i32) -> bool {
    let strategy = spec.update_strategy.as_ref();
    if strategy.is_some_and(|s| s.type_.as_deref() == Some(ON_DELETE))
        || status.current_revision == status.update_revision
    {
        return false;
    }
    let partition = strategy
        .and_then(|s| s.rolling_update.as_ref())
        .and_then(|rolling| rolling.partition)
        .unwrap_or(0);
    partition <= 0 || status.updated_replicas.unwrap_or(0) < desired.saturating_sub(partition)
}

fn is_oom(terminated: &ContainerStateTerminated) -> bool {
    terminated.reason.as_deref() == Some(OOM_KILLED)
}

fn is_stable(ran: SignedDuration) -> bool {
    ran >= CRASH_LOOP_STABLE_AFTER
}

/// Whether `previous` was a short run and a start at `started` is the next attempt after it, not
/// a later recovery.
fn is_short_run_before(previous: &ContainerStateTerminated, started: Timestamp) -> bool {
    ran_for(previous).is_some_and(|ran| !is_stable(ran))
        && time(&previous.finished_at)
            .is_some_and(|finished| started.duration_since(finished) <= CRASH_LOOP_MAX_RESTART_GAP)
}

/// Whether the kubelet will restart container `name` after it exits with non-zero `exit_code`.
///
/// Rules apply in order and the first match decides; with none, the container's policy applies,
/// then the pod's, which defaults to `Always`. Anything unrecognised counts as no restart, so a
/// value from a newer API cannot raise a false alarm. Container names are unique within a pod,
/// and the search runs only for a container that has just failed.
fn restarts_on_failure(spec: &PodSpec, name: &str, exit_code: i32) -> bool {
    let init = spec.init_containers.iter().flatten();
    let Some(container) = spec.containers.iter().chain(init).find(|c| c.name == name) else {
        return false;
    };
    for rule in container.restart_policy_rules.iter().flatten() {
        let Some(codes) = &rule.exit_codes else {
            return false;
        };
        let listed = codes.values.iter().flatten().any(|code| *code == exit_code);
        let matched = match codes.operator.as_str() {
            "In" => listed,
            "NotIn" => !listed,
            _ => return false,
        };
        if matched {
            return matches!(rule.action.as_str(), "Restart" | "RestartAllContainers");
        }
    }
    let policy = container
        .restart_policy
        .as_deref()
        .or(spec.restart_policy.as_deref())
        .unwrap_or("Always");
    matches!(policy, "Always" | "OnFailure")
}

/// How long a finished run lasted, when both ends are known.
fn ran_for(terminated: &ContainerStateTerminated) -> Option<SignedDuration> {
    let started = time(&terminated.started_at)?;
    let finished = time(&terminated.finished_at)?;
    Some(finished.duration_since(started))
}

fn time(time: &Option<Time>) -> Option<Timestamp> {
    time.as_ref().map(|t| t.0)
}

#[cfg(test)]
#[path = "problems_tests.rs"]
mod tests;
