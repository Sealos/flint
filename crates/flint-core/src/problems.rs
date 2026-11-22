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
//! no restart count, reason or time. A notifier that keeps the ids of the last pass can tell:
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

use std::sync::Arc;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ContainerStateTerminated, ContainerStatus, Node, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use k8s_openapi::jiff::{SignedDuration, Timestamp};

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
}

impl ProblemKind {
    /// How urgent this kind of problem is.
    pub const fn severity(self) -> ProblemSeverity {
        match self {
            Self::CrashLoop | Self::NodeNotReady => ProblemSeverity::Critical,
            Self::OomKilled | Self::JobFailed => ProblemSeverity::Warning,
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
        }
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

/// The object a problem concerns. UIDs are optional only because the API types make them so.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProblemSubject {
    /// One container of a pod, including init and sidecar containers.
    Container {
        namespace: String,
        pod: String,
        pod_uid: Option<String>,
        container: String,
    },
    /// A node.
    Node { name: String, uid: Option<String> },
    /// A Job.
    Job {
        namespace: String,
        name: String,
        uid: Option<String>,
    },
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
    /// The known reason code, if the object gave one.
    pub reason: Option<ProblemReason>,
    /// When this was last known to start, if the object says.
    ///
    /// For a crash loop or OOM kill, the latest termination, so it moves on each restart. For a
    /// node or Job, the last transition of its condition. Never part of the identity.
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
        }
    }
}

/// Classifies objects of one cluster at one moment.
#[derive(Clone, Debug)]
pub struct Classifier {
    cluster: Arc<str>,
    now: Timestamp,
}

impl Classifier {
    /// A classifier for `cluster`, which is only copied into results, judging ages against `now`.
    pub fn new(cluster: impl Into<Arc<str>>, now: Timestamp) -> Self {
        Self {
            cluster: cluster.into(),
            now,
        }
    }

    /// The problems of every pod, in order.
    pub fn pods<'a>(&self, pods: impl IntoIterator<Item = &'a Pod>) -> Vec<Problem> {
        let mut out = Vec::new();
        for pod in pods {
            self.pod(pod, &mut out);
        }
        out
    }

    /// Appends the problems of one pod, at most one per container.
    ///
    /// A deleting or succeeded pod has none. A failed pod will not restart, so it can only report
    /// an OOM kill that ended its final run.
    pub fn pod(&self, pod: &Pod, out: &mut Vec<Problem>) {
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
        let init = status.init_container_statuses.iter().flatten();
        for container in init.chain(status.container_statuses.iter().flatten()) {
            let Some((kind, reason, since)) = self.container(container, finished) else {
                continue;
            };
            let subject = ProblemSubject::Container {
                namespace: meta.namespace.clone().unwrap_or_default(),
                pod: name.clone(),
                pod_uid: meta.uid.clone(),
                container: container.name.clone(),
            };
            out.push(self.problem(kind, subject, reason, since));
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
        Some(self.problem(ProblemKind::NodeNotReady, subject, reason, since))
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
        Some(self.problem(ProblemKind::JobFailed, subject, reason, since))
    }

    /// A crash loop takes precedence over an OOM kill, so one failure is not counted twice.
    fn container(
        &self,
        status: &ContainerStatus,
        pod_finished: bool,
    ) -> Option<(ProblemKind, Option<ProblemReason>, Option<Timestamp>)> {
        let current = status.state.as_ref().and_then(|s| s.terminated.as_ref());
        let previous = status
            .last_state
            .as_ref()
            .and_then(|s| s.terminated.as_ref());
        let latest = current.or(previous);
        if !pod_finished && self.is_crash_looping(status, current, previous) {
            let reason = if latest.is_some_and(is_oom) {
                ProblemReason::OomKilled
            } else {
                ProblemReason::CrashLoopBackOff
            };
            let since = latest.and_then(|t| time(&t.finished_at));
            return Some((ProblemKind::CrashLoop, Some(reason), since));
        }
        // The last state of a finished pod is an earlier attempt, not what ended it.
        let last_run = if pod_finished { current } else { latest };
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
            // Between back-offs: still looping until this run outlasts the stable period.
            return time(&running.started_at)
                .is_some_and(|started| self.within(started, CRASH_LOOP_STABLE_AFTER))
                && previous.is_some_and(|t| ran_for(t).is_some_and(|d| !is_stable(d)));
        }
        // Just exited, before the kubelet reports the back-off.
        current.is_some_and(|t| t.exit_code != 0 && ran_for(t).is_some_and(|d| !is_stable(d)))
    }

    fn problem(
        &self,
        kind: ProblemKind,
        subject: ProblemSubject,
        reason: Option<ProblemReason>,
        since: Option<Timestamp>,
    ) -> Problem {
        Problem {
            id: ProblemId {
                cluster: Arc::clone(&self.cluster),
                kind,
                subject,
            },
            reason,
            since,
        }
    }

    /// Whether `then` is less than `window` ago. A time ahead of `now` (clock skew) counts.
    fn within(&self, then: Timestamp, window: SignedDuration) -> bool {
        self.now.duration_since(then) < window
    }
}

/// The Kubernetes termination reason for an OOM kill.
const OOM_KILLED: &str = "OOMKilled";

fn is_oom(terminated: &ContainerStateTerminated) -> bool {
    terminated.reason.as_deref() == Some(OOM_KILLED)
}

fn is_stable(ran: SignedDuration) -> bool {
    ran >= CRASH_LOOP_STABLE_AFTER
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
