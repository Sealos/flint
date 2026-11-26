//! Each pod's requests, limits, node and owner, from the pods a feed already watches, so that the
//! pod, node and workload lists can compare usage with them without a request of their own.
//!
//! A pod's [`PodBudgetCheck`] and node are read before the pod is trimmed. Its record is kept in
//! the store's index next to the capacity tally, shared behind an `Arc`, and the [`PodIndex`] a
//! snapshot carries is rebuilt from those records only after a pod came, changed or left. Node
//! names, namespaces and owners are interned, so pods on one node or of one ReplicaSet share them.
//!
//! The per-node totals of what pods request are in [`ClusterCapacity`](super::ClusterCapacity).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Resource;

use super::capacity::{PodClaim, PodTally, PodTallySlot};
use super::checked::identity;
use super::objects::{KindStore, Tracked};
use crate::metrics::PodBudgetCheck;

/// The object that controls another, from its controller owner reference.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PodOwner {
    /// The API group, empty for the core group: `apps` for a ReplicaSet, `batch` for a Job.
    pub group: Arc<str>,
    /// Such as `ReplicaSet`, `Job`, `StatefulSet` or `DaemonSet`.
    pub kind: Arc<str>,
    pub name: Arc<str>,
    /// The owner's uid, which its own list row carries as its uid.
    pub uid: Arc<str>,
}

/// What a list compares a pod's usage with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodRecord {
    pub namespace: Arc<str>,
    pub name: Box<str>,
    /// `metadata.creationTimestamp`, Unix seconds, to tell a usage sample of an earlier pod
    /// under the same name apart through [`PodSample::follows_creation`](crate::metrics::PodSample::follows_creation).
    pub created: Option<i64>,
    /// Its requests and limits, under the rules of the detail pane: compare a usage sample with
    /// them through [`PodBudgetCheck::for_sample`].
    pub budget: PodBudgetCheck,
    /// The node it is scheduled on, `None` while it is not.
    pub node: Option<Arc<str>>,
    /// Its controller, `None` for a pod nothing controls.
    pub owner: Option<PodOwner>,
    /// It has Succeeded or Failed, so it holds nothing on its node.
    pub finished: bool,
}

/// The objects that control ReplicaSets or Jobs, by the uid of the ReplicaSet or Job.
pub type Controllers = HashMap<Arc<str>, PodOwner>;

/// Every pod's [`PodRecord`], by the identity its list row carries as its uid: the pod's uid, or
/// `namespace/name` if it has none.
#[derive(Clone, Debug, Default)]
pub struct PodIndex {
    pods: Arc<HashMap<Arc<str>, Arc<PodRecord>>>,
    /// `None` while ReplicaSets cannot be relied on.
    replica_sets: Option<Arc<Controllers>>,
    /// `None` while Jobs cannot be relied on.
    jobs: Option<Arc<Controllers>>,
}

impl PartialEq for PodIndex {
    /// Indexes built from the same reads. Each part is rebuilt only when what it reads changed, so
    /// this needs no walk over the pods.
    fn eq(&self, other: &Self) -> bool {
        fn same<T>(a: Option<&Arc<T>>, b: Option<&Arc<T>>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
        }
        Arc::ptr_eq(&self.pods, &other.pods)
            && same(self.replica_sets.as_ref(), other.replica_sets.as_ref())
            && same(self.jobs.as_ref(), other.jobs.as_ref())
    }
}

impl Eq for PodIndex {}

impl PodIndex {
    /// The pod a list row with this uid shows.
    pub fn get(&self, uid: &str) -> Option<&PodRecord> {
        self.pods.get(uid).map(AsRef::as_ref)
    }

    /// Every pod, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &PodRecord> {
        self.pods.values().map(AsRef::as_ref)
    }

    pub fn len(&self) -> usize {
        self.pods.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pods.is_empty()
    }

    /// The workload `pod` belongs to: its owner's controller for a pod of a ReplicaSet (a
    /// Deployment) or a Job (a CronJob), else its owner. `None` for a pod nothing controls, or
    /// while the ReplicaSets or Jobs that would say cannot be read.
    pub fn workload_of<'a>(&'a self, pod: &'a PodRecord) -> Option<&'a PodOwner> {
        let owner = pod.owner.as_ref()?;
        let controllers = match (&*owner.group, &*owner.kind) {
            ("apps", "ReplicaSet") => self.replica_sets.as_deref()?,
            ("batch", "Job") => self.jobs.as_deref()?,
            _ => return Some(owner),
        };
        Some(controllers.get(&owner.uid).unwrap_or(owner))
    }

    /// The index a feed's stores give, `None` while pods cannot be relied on.
    pub(super) fn of(
        pods: &KindStore<Pod>,
        replica_sets: &KindStore<k8s_openapi::api::apps::v1::ReplicaSet>,
        jobs: &KindStore<k8s_openapi::api::batch::v1::Job>,
    ) -> Option<Self> {
        Some(Self {
            pods: pods.records()?,
            replica_sets: replica_sets.controllers(),
            jobs: jobs.controllers(),
        })
    }

    /// An index of `records` by uid, with `controllers` of both ReplicaSets and Jobs, for
    /// front-end tests.
    #[cfg(feature = "test-support")]
    pub fn for_tests(
        records: impl IntoIterator<Item = (&'static str, PodRecord)>,
        controllers: Controllers,
    ) -> Self {
        let controllers = Arc::new(controllers);
        Self {
            pods: Arc::new(
                records
                    .into_iter()
                    .map(|(uid, record)| (Arc::from(uid), Arc::new(record)))
                    .collect(),
            ),
            replica_sets: Some(Arc::clone(&controllers)),
            jobs: Some(controllers),
        }
    }
}

/// What a pod gives its store before it is trimmed. Handed over and not kept.
#[derive(Clone, Debug)]
pub(crate) struct PodRead {
    pub(super) claim: Option<PodClaim>,
    /// `None` for a pod without a spec, which gets no record.
    budget: Option<PodBudgetCheck>,
    node: Option<String>,
    finished: bool,
}

impl PodRead {
    /// Read before `pod` is trimmed.
    pub(super) fn of(pod: &Pod) -> Self {
        let spec = pod.spec.as_ref();
        let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
        Self {
            claim: PodClaim::of(pod),
            budget: spec.map(PodBudgetCheck::of),
            node: spec
                .and_then(|s| s.node_name.clone())
                .filter(|n| !n.is_empty()),
            finished: matches!(phase, Some("Succeeded" | "Failed")),
        }
    }
}

/// What a pod's index entry keeps to take it back out of the totals, and its record.
#[derive(Debug, Default)]
pub(super) struct PodSlot {
    tally: Option<PodTallySlot>,
    record: Option<(Arc<str>, Arc<PodRecord>)>,
}

impl PodSlot {
    /// Whether `other` publishes the same record, so that an update such as a new annotation
    /// keeps the index built.
    pub(super) fn publishes_as(&self, other: &Self) -> bool {
        self.record == other.record
    }
}

/// Strings many pods share, each held once. Strings no record holds any more are dropped when the
/// set has doubled since it was last pruned.
#[derive(Debug, Default)]
struct Names {
    set: HashSet<Arc<str>>,
    pruned_at: usize,
}

impl Names {
    const MIN_PRUNE: usize = 256;

    fn get(&mut self, name: &str) -> Arc<str> {
        if let Some(held) = self.set.get(name) {
            return Arc::clone(held);
        }
        if self.set.len() >= (2 * self.pruned_at).max(Self::MIN_PRUNE) {
            self.set.retain(|held| Arc::strong_count(held) > 1);
            self.pruned_at = self.set.len();
        }
        let held: Arc<str> = Arc::from(name);
        self.set.insert(Arc::clone(&held));
        held
    }
}

/// What a pod store adds up: per-node totals, and the records the index is built from.
#[derive(Debug, Default)]
pub(crate) struct PodTotals {
    pub(super) tally: PodTally,
    names: Names,
    /// The records as last built, until a pod comes, changes or leaves.
    built: OnceLock<Arc<HashMap<Arc<str>, Arc<PodRecord>>>>,
}

impl PodTotals {
    /// A pod came or left, or its record changed.
    pub(super) fn changed(&mut self) {
        self.built.take();
    }

    /// Counts `pod`, trimmed, with what was read before trimming.
    pub(super) fn count(&mut self, pod: &Pod, read: PodRead) -> PodSlot {
        let tally = read.claim.as_ref().map(|claim| self.tally.add(claim));
        let meta = pod.meta();
        let record = meta.name.as_deref().zip(read.budget).map(|(name, budget)| {
            let uid: Arc<str> = Arc::from(identity(meta));
            let record = PodRecord {
                namespace: self
                    .names
                    .get(meta.namespace.as_deref().unwrap_or_default()),
                name: name.into(),
                budget,
                node: read.node.as_deref().map(|node| self.names.get(node)),
                created: meta.creation_timestamp.as_ref().map(|t| t.0.as_second()),
                owner: controller_of(meta).map(|owner| owner_of(&mut self.names, owner)),
                finished: read.finished,
            };
            (uid, Arc::new(record))
        });
        PodSlot { tally, record }
    }

    pub(super) fn uncount(&mut self, slot: PodSlot) {
        if let Some(tally) = slot.tally {
            self.tally.remove(tally);
        }
    }
}

impl KindStore<Pod> {
    /// Every pod's record, if pods can be relied on. Built once per change of the pods.
    fn records(&self) -> Option<Arc<HashMap<Arc<str>, Arc<PodRecord>>>> {
        let totals = self.current_totals()?;
        let built = totals.built.get_or_init(|| {
            Arc::new(
                self.slots()
                    .filter_map(|slot| slot.record.clone())
                    .collect(),
            )
        });
        Some(Arc::clone(built))
    }
}

/// What a ReplicaSet or Job store keeps: who controls them, built once per change.
#[derive(Debug, Default)]
pub(crate) struct ControllerCache {
    names: Names,
    built: OnceLock<Arc<Controllers>>,
}

impl ControllerCache {
    pub(super) fn changed(&mut self) {
        self.built.take();
    }
}

/// A ReplicaSet's or Job's uid and controller, kept in its index entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ControllerSlot(Option<(Arc<str>, PodOwner)>);

impl ControllerSlot {
    pub(super) fn of(meta: &ObjectMeta, totals: &mut ControllerCache) -> Self {
        let uid = meta.uid.as_deref().filter(|uid| !uid.is_empty());
        Self(
            uid.zip(controller_of(meta))
                .map(|(uid, owner)| (totals.names.get(uid), owner_of(&mut totals.names, owner))),
        )
    }
}

impl<K: Tracked<Totals = ControllerCache, TallySlot = ControllerSlot>> KindStore<K> {
    /// The controller of each object held, by its uid, if they can be relied on.
    fn controllers(&self) -> Option<Arc<Controllers>> {
        let totals = self.current_totals()?;
        let built = totals
            .built
            .get_or_init(|| Arc::new(self.slots().filter_map(|slot| slot.0.clone()).collect()));
        Some(Arc::clone(built))
    }
}

fn owner_of(names: &mut Names, owner: &OwnerReference) -> PodOwner {
    let group = owner
        .api_version
        .rsplit_once('/')
        .map_or("", |(group, _)| group);
    PodOwner {
        group: names.get(group),
        kind: names.get(&owner.kind),
        name: names.get(&owner.name),
        uid: names.get(&owner.uid),
    }
}

/// The owner reference marked as an object's controller.
fn controller_of(meta: &ObjectMeta) -> Option<&OwnerReference> {
    meta.owner_references
        .as_ref()?
        .iter()
        .find(|owner| owner.controller == Some(true))
}

#[cfg(test)]
#[path = "pods_tests.rs"]
mod tests;
