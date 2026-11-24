//! The objects a feed holds: each kind cut down to what classification reads, in a store with a
//! hard limit. Server-supplied text and pod templates, which can hold credentials in environment
//! variables, are not kept.

use std::collections::HashMap;
use std::fmt::Debug;

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::{Job, JobStatus};
use k8s_openapi::api::core::v1::{
    Container, ContainerState, ContainerStatus, Node, NodeStatus, Pod, PodSpec, PodStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Resource;
use serde::de::DeserializeOwned;

use super::capacity::{ALLOCATABLE_KEYS, PodClaim, PodTally, PodTallySlot};
use super::{KindCheck, WatchedKind};

/// The most objects of one kind a feed holds.
pub const MAX_OBJECTS_PER_KIND: usize = 50_000;

/// An object's namespace (empty if cluster-scoped) and name.
type Key = (String, String);

fn key_of(meta: &ObjectMeta) -> Option<Key> {
    Some((
        meta.namespace.clone().unwrap_or_default(),
        meta.name.clone()?,
    ))
}

#[derive(Debug)]
struct StoreEntry<S> {
    at: usize,
    tally_slot: S,
}

/// The objects of one kind, in a slice as the classifier takes them.
#[derive(Debug)]
pub(super) struct KindStore<K: Tracked> {
    pub(super) check: KindCheck,
    items: Vec<K>,
    /// What the items add up to.
    totals: K::Totals,
    index: HashMap<Key, StoreEntry<K::TallySlot>>,
}

impl<K: Tracked> Default for KindStore<K> {
    fn default() -> Self {
        Self {
            check: KindCheck::Starting,
            items: Vec::new(),
            totals: K::Totals::default(),
            index: HashMap::new(),
        }
    }
}

impl<K: Tracked> KindStore<K> {
    /// The objects, if they can be relied on.
    pub(super) fn current(&self) -> Option<&[K]> {
        (self.check == KindCheck::Current).then_some(self.items.as_slice())
    }

    /// What the objects add up to, if they can be relied on.
    pub(super) fn current_totals(&self) -> Option<&K::Totals> {
        (self.check == KindCheck::Current).then_some(&self.totals)
    }

    /// Replaces the objects with a fresh list. The check is unchanged: a kind becomes current
    /// only when its watch opens.
    pub(super) fn replace(&mut self, items: Vec<(K, K::Extra)>) {
        self.items.clear();
        self.totals = K::Totals::default();
        self.index.clear();
        for (item, extra) in items {
            // A fresh list is already within the limit.
            let _ = self.upsert(item, extra);
        }
    }

    /// Adds or replaces an object. `false` if that would pass [`MAX_OBJECTS_PER_KIND`].
    pub(super) fn upsert(&mut self, item: K, extra: K::Extra) -> bool {
        let Some(key) = key_of(item.meta()) else {
            return true;
        };
        if let Some(entry) = self.index.get_mut(&key) {
            let tally_slot = item.count(extra, &mut self.totals);
            let old_slot = std::mem::replace(&mut entry.tally_slot, tally_slot);
            self.items[entry.at] = item;
            K::uncount(old_slot, &mut self.totals);
            return true;
        }
        if self.items.len() >= MAX_OBJECTS_PER_KIND {
            return false;
        }
        let tally_slot = item.count(extra, &mut self.totals);
        self.index.insert(
            key,
            StoreEntry {
                at: self.items.len(),
                tally_slot,
            },
        );
        self.items.push(item);
        true
    }

    pub(super) fn remove(&mut self, item: &K) {
        let Some(StoreEntry { at, tally_slot }) =
            key_of(item.meta()).and_then(|key| self.index.remove(&key))
        else {
            return;
        };
        self.items.swap_remove(at);
        K::uncount(tally_slot, &mut self.totals);
        if let Some(moved) = self.items.get(at).and_then(|m| key_of(m.meta())) {
            self.index
                .get_mut(&moved)
                .expect("moved item is indexed")
                .at = at;
        }
    }

    /// Records that the objects cannot be relied on, and lets them go.
    pub(super) fn invalidate(&mut self, check: KindCheck) {
        self.check = check;
        self.items = Vec::new();
        self.totals = K::Totals::default();
        self.index = HashMap::new();
    }
}

/// The objects of a feed.
#[derive(Debug, Default)]
pub(super) struct Stores {
    pub(super) pods: KindStore<Pod>,
    pub(super) nodes: KindStore<Node>,
    pub(super) jobs: KindStore<Job>,
    pub(super) deployments: KindStore<Deployment>,
    pub(super) stateful_sets: KindStore<StatefulSet>,
    pub(super) daemon_sets: KindStore<DaemonSet>,
    pub(super) replica_sets: KindStore<ReplicaSet>,
}

impl Stores {
    /// Each kind's state, in the order of [`WatchedKind::ALL`].
    pub(super) fn checks(&self) -> [(WatchedKind, KindCheck); WatchedKind::ALL.len()] {
        WatchedKind::ALL.map(|kind| {
            let check = match kind {
                WatchedKind::Pods => &self.pods.check,
                WatchedKind::Nodes => &self.nodes.check,
                WatchedKind::Jobs => &self.jobs.check,
                WatchedKind::Deployments => &self.deployments.check,
                WatchedKind::StatefulSets => &self.stateful_sets.check,
                WatchedKind::DaemonSets => &self.daemon_sets.check,
                WatchedKind::ReplicaSets => &self.replica_sets.check,
            };
            (kind, check.clone())
        })
    }
}

/// A kind the feed watches.
pub(super) trait Tracked:
    Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug + Send + Sync + 'static
{
    /// What trimming reads for [`Self::Totals`]. Handed to the store and not kept.
    type Extra: Clone + Debug + Send + Sync + 'static;

    /// What a store adds up over its objects.
    type Totals: Default + Debug + Send + Sync + 'static;

    /// What the index keeps to take this object back out of the totals.
    type TallySlot: Default + Debug + Send + Sync + 'static;

    /// This kind's store.
    fn store(stores: &mut Stores) -> &mut KindStore<Self>;

    /// Drops everything the feed does not read, and returns [`Self::Extra`].
    fn trim(&mut self) -> Self::Extra;

    /// Adds this object to `totals` and returns its tally slot.
    fn count(&self, _extra: Self::Extra, _totals: &mut Self::Totals) -> Self::TallySlot {
        Self::TallySlot::default()
    }

    /// Takes this object out of `totals` using the slot returned by [`Self::count`].
    fn uncount(_slot: Self::TallySlot, _totals: &mut Self::Totals) {}
}

fn trim_meta(meta: &mut ObjectMeta) {
    *meta = ObjectMeta {
        name: meta.name.take(),
        namespace: meta.namespace.take(),
        uid: meta.uid.take(),
        generation: meta.generation,
        creation_timestamp: meta.creation_timestamp.take(),
        deletion_timestamp: meta.deletion_timestamp.take(),
        owner_references: meta.owner_references.take(),
        ..ObjectMeta::default()
    };
}

/// What restart decisions read of a container.
fn trim_container(container: &mut Container) {
    *container = Container {
        name: std::mem::take(&mut container.name),
        restart_policy: container.restart_policy.take(),
        restart_policy_rules: container.restart_policy_rules.take(),
        ..Container::default()
    };
}

fn trim_state(state: &mut Option<ContainerState>) {
    let Some(state) = state else { return };
    if let Some(waiting) = &mut state.waiting {
        waiting.message = None;
    }
    if let Some(terminated) = &mut state.terminated {
        terminated.message = None;
        terminated.container_id = None;
    }
}

fn trim_container_status(status: &mut ContainerStatus) {
    let (mut state, mut last_state) = (status.state.take(), status.last_state.take());
    trim_state(&mut state);
    trim_state(&mut last_state);
    *status = ContainerStatus {
        name: std::mem::take(&mut status.name),
        restart_count: status.restart_count,
        state,
        last_state,
        ..ContainerStatus::default()
    };
}

impl Tracked for Pod {
    type Extra = Option<PodClaim>;
    type Totals = PodTally;
    type TallySlot = Option<PodTallySlot>;

    fn store(stores: &mut Stores) -> &mut KindStore<Self> {
        &mut stores.pods
    }

    fn trim(&mut self) -> Option<PodClaim> {
        let claim = PodClaim::of(self);
        trim_meta(&mut self.metadata);
        self.spec = self.spec.take().map(|mut spec| {
            spec.containers.iter_mut().for_each(trim_container);
            let mut init = spec.init_containers.take();
            init.iter_mut().flatten().for_each(trim_container);
            PodSpec {
                containers: spec.containers,
                init_containers: init,
                restart_policy: spec.restart_policy,
                ..PodSpec::default()
            }
        });
        self.status = self.status.take().map(|status| {
            let mut trimmed = PodStatus {
                phase: status.phase,
                container_statuses: status.container_statuses,
                init_container_statuses: status.init_container_statuses,
                ephemeral_container_statuses: status.ephemeral_container_statuses,
                ..PodStatus::default()
            };
            for list in [
                &mut trimmed.container_statuses,
                &mut trimmed.init_container_statuses,
                &mut trimmed.ephemeral_container_statuses,
            ] {
                list.iter_mut().flatten().for_each(trim_container_status);
            }
            trimmed
        });
        claim
    }

    fn count(&self, claim: Option<PodClaim>, totals: &mut PodTally) -> Option<PodTallySlot> {
        self.spec.as_ref()?;
        claim.map(|claim| totals.add(&claim))
    }

    fn uncount(slot: Option<PodTallySlot>, totals: &mut PodTally) {
        if let Some(slot) = slot {
            totals.remove(slot);
        }
    }
}

impl Tracked for Node {
    type Extra = ();
    type Totals = ();
    type TallySlot = ();

    fn store(stores: &mut Stores) -> &mut KindStore<Self> {
        &mut stores.nodes
    }

    fn trim(&mut self) {
        trim_meta(&mut self.metadata);
        self.spec = None;
        self.status = self.status.take().map(|status| {
            let mut conditions = status.conditions;
            for condition in conditions.iter_mut().flatten() {
                condition.message = None;
            }
            let allocatable = status.allocatable.map(|list| {
                list.into_iter()
                    .filter(|(name, _)| ALLOCATABLE_KEYS.contains(&name.as_str()))
                    .collect()
            });
            NodeStatus {
                conditions,
                allocatable,
                ..NodeStatus::default()
            }
        });
    }
}

impl Tracked for Job {
    type Extra = ();
    type Totals = ();
    type TallySlot = ();

    fn store(stores: &mut Stores) -> &mut KindStore<Self> {
        &mut stores.jobs
    }

    fn trim(&mut self) {
        trim_meta(&mut self.metadata);
        self.spec = None;
        self.status = self.status.take().map(|status| {
            let mut conditions = status.conditions;
            for condition in conditions.iter_mut().flatten() {
                condition.message = None;
            }
            JobStatus {
                conditions,
                ..JobStatus::default()
            }
        });
    }
}

impl Tracked for Deployment {
    type Extra = ();
    type Totals = ();
    type TallySlot = ();

    fn store(stores: &mut Stores) -> &mut KindStore<Self> {
        &mut stores.deployments
    }

    fn trim(&mut self) {
        trim_meta(&mut self.metadata);
        self.spec.template = Default::default();
        self.spec.selector = Default::default();
        if let Some(status) = &mut self.status {
            for condition in status.conditions.iter_mut().flatten() {
                condition.message = None;
            }
        }
    }
}

impl Tracked for StatefulSet {
    type Extra = ();
    type Totals = ();
    type TallySlot = ();

    fn store(stores: &mut Stores) -> &mut KindStore<Self> {
        &mut stores.stateful_sets
    }

    fn trim(&mut self) {
        trim_meta(&mut self.metadata);
        self.spec.template = Default::default();
        self.spec.selector = Default::default();
        self.spec.volume_claim_templates = None;
        if let Some(status) = &mut self.status {
            status.conditions = None;
        }
    }
}

impl Tracked for DaemonSet {
    type Extra = ();
    type Totals = ();
    type TallySlot = ();

    fn store(stores: &mut Stores) -> &mut KindStore<Self> {
        &mut stores.daemon_sets
    }

    fn trim(&mut self) {
        trim_meta(&mut self.metadata);
        self.spec.template = Default::default();
        self.spec.selector = Default::default();
        if let Some(status) = &mut self.status {
            status.conditions = None;
        }
    }
}

impl Tracked for ReplicaSet {
    type Extra = ();
    type Totals = ();
    type TallySlot = ();

    fn store(stores: &mut Stores) -> &mut KindStore<Self> {
        &mut stores.replica_sets
    }

    fn trim(&mut self) {
        trim_meta(&mut self.metadata);
        self.spec = Default::default();
        self.status = None;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn pod(name: &str) -> Pod {
        serde_json::from_value(json!({
            "metadata": {"name": name, "namespace": "demo", "uid": name,
                         "annotations": {"note": "secret-looking"}},
            "spec": {"restartPolicy": "Always", "containers": [{
                "name": "app", "image": "app:1",
                "env": [{"name": "TOKEN", "value": "hunter2"}],
                "restartPolicy": "Always"
            }]},
            "status": {"phase": "Running", "containerStatuses": [{
                "name": "app", "image": "app:1", "imageID": "x", "ready": false,
                "restartCount": 4,
                "state": {"waiting": {"reason": "CrashLoopBackOff", "message": "server text"}}
            }]}
        }))
        .unwrap()
    }

    #[test]
    fn a_trimmed_pod_keeps_what_classification_reads_and_no_template_values() {
        let mut trimmed = pod("a");
        let _ = trimmed.trim();
        let text = serde_json::to_string(&trimmed).unwrap();
        for gone in ["hunter2", "TOKEN", "secret-looking", "server text", "app:1"] {
            assert!(!text.contains(gone), "{gone} was kept: {text}");
        }
        let status = trimmed.status.as_ref().unwrap();
        let container = &status.container_statuses.as_ref().unwrap()[0];
        assert_eq!(container.restart_count, 4);
        let waiting = container.state.as_ref().unwrap().waiting.as_ref().unwrap();
        assert_eq!(waiting.reason.as_deref(), Some("CrashLoopBackOff"));
        let spec = trimmed.spec.as_ref().unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Always"));
        assert_eq!(spec.containers[0].restart_policy.as_deref(), Some("Always"));
    }

    #[test]
    fn the_store_replaces_removes_and_keeps_its_index() {
        let mut store = KindStore::<Pod>::default();
        store.replace(vec![(pod("a"), None), (pod("b"), None), (pod("c"), None)]);
        store.remove(&pod("a"));
        assert!(store.upsert(pod("b"), None));
        store.remove(&pod("c"));
        store.check = KindCheck::Current;
        let names: Vec<_> = store
            .current()
            .unwrap()
            .iter()
            .map(|p| p.metadata.name.as_deref().unwrap())
            .collect();
        assert_eq!(names, ["b"]);
    }

    #[test]
    fn the_store_refuses_a_new_object_past_the_limit_but_updates_a_held_one() {
        let mut store = KindStore::<ReplicaSet>::default();
        let set = |i: usize| ReplicaSet {
            metadata: ObjectMeta {
                name: Some(format!("rs-{i}")),
                namespace: Some("demo".into()),
                ..ObjectMeta::default()
            },
            ..ReplicaSet::default()
        };
        for i in 0..MAX_OBJECTS_PER_KIND {
            assert!(store.upsert(set(i), ()));
        }
        assert!(!store.upsert(set(MAX_OBJECTS_PER_KIND), ()));
        assert!(store.upsert(set(0), ()));
    }

    #[test]
    fn objects_held_are_not_current_until_the_check_says_so() {
        let mut store = KindStore::<Pod>::default();
        store.replace(vec![(pod("a"), None)]);
        assert!(store.current().is_none());
        store.check = KindCheck::Current;
        assert_eq!(store.current().map(<[Pod]>::len), Some(1));
        store.invalidate(KindCheck::TooLarge);
        assert!(store.current().is_none());
    }
}
