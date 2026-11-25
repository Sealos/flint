//! How full a cluster is, and how much is in it, from the objects a feed already watches.
//!
//! A pod's requests and limits are added to its node's totals when it enters the store. Its index
//! entry keeps a [`PodTallySlot`] so the store can subtract them later. Pods asking for
//! the same amounts share one table entry, so a large cluster adds no resource lists, loads or node
//! names per pod. Amounts follow what the scheduler reserves: containers and sidecars add up, an
//! init container counts when it needs more than that, pod-level resources replace the container
//! total, overhead is added, and during an in-place resize the larger of what is asked for and what
//! the node still holds counts.
//!
//! An amount that cannot be read or held is `None` and makes every total it belongs to `None`, so
//! a partial sum is never shown as the cluster's.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{
    Container, ContainerStatus, Node, Pod, PodSpec, PodStatus, ResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use super::objects::Stores;
use crate::metrics::{Cpu, Memory};

/// The allocatable entries a trimmed node keeps.
pub(super) const ALLOCATABLE_KEYS: [&str; 3] = ["cpu", "memory", "pods"];

/// CPU in nanocores and memory in bytes, each `None` when it could not be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Demand {
    cpu: Option<u64>,
    memory: Option<u64>,
}

/// What one pod reserves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct PodLoad {
    requests: Demand,
    limits: Demand,
}

impl PodLoad {
    /// The load of a pod with `spec` and, if reported, `status`.
    fn of(spec: &PodSpec, status: Option<&PodStatus>) -> Self {
        let cpu = |text: &str| text.parse::<Cpu>().ok().map(Cpu::nanos);
        let memory = |text: &str| text.parse::<Memory>().ok().map(Memory::bytes);
        Self {
            requests: Demand {
                cpu: effective(spec, status, "cpu", Side::Requests, &cpu),
                memory: effective(spec, status, "memory", Side::Requests, &memory),
            },
            limits: Demand {
                cpu: effective(spec, status, "cpu", Side::Limits, &cpu),
                memory: effective(spec, status, "memory", Side::Limits, &memory),
            },
        }
    }
}

/// A pod's share of a node, read before trimming and not kept.
#[derive(Clone, Debug)]
pub(super) struct PodClaim {
    node: String,
    load: PodLoad,
}

impl PodClaim {
    /// `None` for a pod that holds nothing: not scheduled, or finished.
    pub(super) fn of(pod: &Pod) -> Option<Self> {
        let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
        if matches!(phase, Some("Succeeded" | "Failed")) {
            return None;
        }
        let spec = pod.spec.as_ref()?;
        Some(Self {
            node: spec.node_name.clone()?,
            load: PodLoad::of(spec, pod.status.as_ref()),
        })
    }
}

/// Values in use, each under a small id that stays valid until its last holder releases it.
#[derive(Debug)]
struct Interned<T> {
    ids: HashMap<T, u32>,
    /// The value and how many hold it. A slot nobody holds is listed in `free`.
    slots: Vec<(T, u32)>,
    free: Vec<u32>,
}

impl<T> Default for Interned<T> {
    fn default() -> Self {
        Self {
            ids: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

impl<T: Clone + Eq + Hash> Interned<T> {
    fn acquire(&mut self, value: &T) -> u32 {
        if let Some(&id) = self.ids.get(value) {
            self.slots[id as usize].1 += 1;
            return id;
        }
        let id = match self.free.pop() {
            Some(id) => {
                self.slots[id as usize] = (value.clone(), 1);
                id
            }
            None => {
                self.slots.push((value.clone(), 1));
                // At most one slot per held pod, far below `u32::MAX`.
                (self.slots.len() - 1) as u32
            }
        };
        self.ids.insert(value.clone(), id);
        id
    }

    fn release(&mut self, id: u32) {
        let (value, holders) = &mut self.slots[id as usize];
        *holders -= 1;
        if *holders == 0 {
            self.ids.remove(value);
            self.free.push(id);
        }
    }

    fn get(&self, id: u32) -> &T {
        &self.slots[id as usize].0
    }
}

/// A sum that can be taken apart again: wide enough not to overflow, counting unreadable values.
#[derive(Clone, Copy, Debug, Default)]
struct Amount {
    total: u128,
    unknown: u32,
}

impl Amount {
    fn add(&mut self, value: Option<u64>) {
        match value {
            Some(value) => self.total += u128::from(value),
            None => self.unknown += 1,
        }
    }

    fn subtract(&mut self, value: Option<u64>) {
        match value {
            Some(value) => self.total -= u128::from(value),
            None => self.unknown -= 1,
        }
    }

    /// The sum, if every value was read and it fits.
    fn value(self) -> Option<u64> {
        (self.unknown == 0)
            .then(|| u64::try_from(self.total).ok())
            .flatten()
    }
}

/// CPU and memory totals of one side of a node's pods.
#[derive(Clone, Copy, Debug, Default)]
struct DemandSum {
    cpu: Amount,
    memory: Amount,
}

impl DemandSum {
    fn add(&mut self, demand: Demand) {
        self.cpu.add(demand.cpu);
        self.memory.add(demand.memory);
    }

    fn subtract(&mut self, demand: Demand) {
        self.cpu.subtract(demand.cpu);
        self.memory.subtract(demand.memory);
    }

    fn reserved(self) -> Reserved {
        Reserved {
            cpu: self.cpu.value().map(Cpu::from_nanos),
            memory: self.memory.value().map(Memory::from_bytes),
        }
    }
}

/// What the pods on one node add up to.
#[derive(Clone, Copy, Debug, Default)]
struct NodeTotals {
    pods: u64,
    requests: DemandSum,
    limits: DemandSum,
}

/// The interned node and load for one counted pod.
#[derive(Debug)]
pub(super) struct PodTallySlot {
    node: u32,
    load: u32,
}

/// Per-node totals of the pods held, kept up to date as pods come, change and go.
#[derive(Debug, Default)]
pub(super) struct PodTally {
    nodes: Interned<String>,
    /// At the ids of `nodes`. A released id's totals are back at zero.
    totals: Vec<NodeTotals>,
    loads: Interned<PodLoad>,
}

impl PodTally {
    /// Counts `claim` and returns the slot to give back to [`Self::remove`].
    pub(super) fn add(&mut self, claim: &PodClaim) -> PodTallySlot {
        let node = self.nodes.acquire(&claim.node);
        let load = self.loads.acquire(&claim.load);
        if self.totals.len() <= node as usize {
            self.totals.resize(node as usize + 1, NodeTotals::default());
        }
        let totals = &mut self.totals[node as usize];
        totals.pods += 1;
        totals.requests.add(claim.load.requests);
        totals.limits.add(claim.load.limits);
        PodTallySlot { node, load }
    }

    /// Takes back what [`Self::add`] counted for `slot`.
    pub(super) fn remove(&mut self, PodTallySlot { node, load }: PodTallySlot) {
        let amounts = *self.loads.get(load);
        let totals = &mut self.totals[node as usize];
        totals.pods -= 1;
        totals.requests.subtract(amounts.requests);
        totals.limits.subtract(amounts.limits);
        self.loads.release(load);
        self.nodes.release(node);
    }

    fn on(&self, node: &str) -> NodeTotals {
        self.nodes
            .ids
            .get(node)
            .map(|&id| self.totals[id as usize])
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Requests,
    Limits,
}

impl Side {
    fn of(self, resources: &ResourceRequirements) -> Option<&BTreeMap<String, Quantity>> {
        match self {
            Self::Requests => resources.requests.as_ref(),
            Self::Limits => resources.limits.as_ref(),
        }
    }
}

/// Whether the node has refused the pod's resize, so what it asks for will not be granted.
fn resize_infeasible(status: Option<&PodStatus>) -> bool {
    status
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.type_ == "PodResizePending" && c.reason.as_deref() == Some("Infeasible"))
}

/// The status of `container` if it reports the resources it is running with.
fn running_status<'a>(
    container: &Container,
    statuses: Option<&'a Vec<ContainerStatus>>,
) -> Option<(&'a ContainerStatus, &'a ResourceRequirements)> {
    let found = statuses?.iter().find(|s| s.name == container.name)?;
    Some((found, found.resources.as_ref()?))
}

/// What a pod reserves of `name`, as the scheduler counts it during a resize. Not setting it is
/// zero; a value that does not parse, or a sum too large to hold, is `None`.
fn effective(
    spec: &PodSpec,
    status: Option<&PodStatus>,
    name: &str,
    side: Side,
    parse: &dyn Fn(&str) -> Option<u64>,
) -> Option<u64> {
    let read = |list: Option<&BTreeMap<String, Quantity>>| match list.and_then(|l| l.get(name)) {
        Some(quantity) => parse(&quantity.0),
        None => Some(0),
    };
    let infeasible = resize_infeasible(status);
    // What is asked for against what the node holds: the larger, or only the held if refused.
    let settle = |wanted: Option<u64>, held: u64| {
        if infeasible {
            Some(held)
        } else {
            wanted.map(|wanted| wanted.max(held))
        }
    };
    let declared = |c: &Container| read(c.resources.as_ref().and_then(|r| side.of(r)));
    let of = |container: &Container, statuses: Option<&Vec<ContainerStatus>>| {
        let Some((found, running)) = running_status(container, statuses) else {
            return declared(container);
        };
        let mut held = read(side.of(running))?;
        if side == Side::Requests {
            held = held.max(read(found.allocated_resources.as_ref())?);
        }
        settle(declared(container), held)
    };
    let pod_level = spec
        .resources
        .as_ref()
        .and_then(|r| side.of(r))
        .and_then(|list| list.get(name));
    let base = match pod_level {
        Some(quantity) => {
            let wanted = parse(&quantity.0);
            let running = status
                .and_then(|s| s.resources.as_ref())
                .and_then(|r| side.of(r))
                .and_then(|list| list.get(name));
            match running {
                Some(running) => settle(wanted, parse(&running.0)?)?,
                None => wanted?,
            }
        }
        None => {
            let main = status.and_then(|s| s.container_statuses.as_ref());
            let init = status.and_then(|s| s.init_container_statuses.as_ref());
            let mut running = 0u64;
            for container in &spec.containers {
                running = running.checked_add(of(container, main)?)?;
            }
            let (mut sidecars, mut peak) = (0u64, 0u64);
            for container in spec.init_containers.iter().flatten() {
                if container.restart_policy.as_deref() == Some("Always") {
                    sidecars = sidecars.checked_add(of(container, init)?)?;
                    peak = peak.max(sidecars);
                } else {
                    // Only sidecars can be resized in place.
                    peak = peak.max(sidecars.checked_add(declared(container)?)?);
                }
            }
            running.checked_add(sidecars)?.max(peak)
        }
    };
    let overhead = read(spec.overhead.as_ref())?;
    // No limit means unbounded, and overhead does not turn that into a limit.
    if side == Side::Limits && base == 0 {
        Some(0)
    } else {
        base.checked_add(overhead)
    }
}

/// What a node offers to pods. Each is `None` when the node does not report it readably.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allocatable {
    pub cpu: Option<Cpu>,
    pub memory: Option<Memory>,
    pub pods: Option<u64>,
}

impl Allocatable {
    fn of(node: &Node) -> Self {
        let list = node.status.as_ref().and_then(|s| s.allocatable.as_ref());
        let get = |name: &str| list.and_then(|l| l.get(name)).map(|q| q.0.as_str());
        Self {
            cpu: get("cpu").and_then(|text| text.parse().ok()),
            memory: get("memory").and_then(|text| text.parse().ok()),
            // A plain count, which a quantity may still write as `1k`.
            pods: get("pods")
                .and_then(|text| text.parse::<Memory>().ok())
                .map(Memory::bytes),
        }
    }
}

/// CPU and memory reserved by pods. Each is `None` when it is not known for every pod counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reserved {
    pub cpu: Option<Cpu>,
    pub memory: Option<Memory>,
}

impl Reserved {
    const ZERO: Self = Self {
        cpu: Some(Cpu::from_nanos(0)),
        memory: Some(Memory::from_bytes(0)),
    };

    /// Adds `other`. A sum too large to hold is unknown rather than a wrong number.
    fn add(&mut self, other: Self) {
        self.cpu = self
            .cpu
            .zip(other.cpu)
            .and_then(|(a, b)| a.nanos().checked_add(b.nanos()))
            .map(Cpu::from_nanos);
        self.memory = self
            .memory
            .zip(other.memory)
            .and_then(|(a, b)| a.bytes().checked_add(b.bytes()))
            .map(Memory::from_bytes);
    }
}

/// One node's capacity and what its pods reserve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCapacity {
    pub name: String,
    pub allocatable: Allocatable,
    pub requests: Reserved,
    pub limits: Reserved,
    /// Pods holding a slot on this node, `None` while pods cannot be read.
    pub pods: Option<u64>,
}

/// Capacity per node. A cluster with no readable nodes has no value of this type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClusterCapacity {
    pub nodes: Arc<[NodeCapacity]>,
}

impl ClusterCapacity {
    /// The node called `name`: what it offers, and what the pods scheduled on it request and
    /// limit, as `kubectl describe node` shows under "Allocated resources".
    pub fn node(&self, name: &str) -> Option<&NodeCapacity> {
        self.nodes
            .binary_search_by(|node| node.name.as_str().cmp(name))
            .ok()
            .map(|at| &self.nodes[at])
    }

    /// What every node offers, each `None` unless all of them report it.
    pub fn allocatable(&self) -> Allocatable {
        let nodes = &self.nodes;
        Allocatable {
            cpu: sum(nodes.iter().map(|n| n.allocatable.cpu.map(Cpu::nanos))).map(Cpu::from_nanos),
            memory: sum(nodes
                .iter()
                .map(|n| n.allocatable.memory.map(Memory::bytes)))
            .map(Memory::from_bytes),
            pods: sum(nodes.iter().map(|n| n.allocatable.pods)),
        }
    }

    /// What pods request across the nodes.
    pub fn requests(&self) -> Reserved {
        self.total(|node| node.requests)
    }

    /// The limits pods set across the nodes.
    pub fn limits(&self) -> Reserved {
        self.total(|node| node.limits)
    }

    /// Pods holding a slot, `None` while pods cannot be read.
    pub fn pods(&self) -> Option<u64> {
        sum(self.nodes.iter().map(|node| node.pods))
    }

    fn total(&self, of: impl Fn(&NodeCapacity) -> Reserved) -> Reserved {
        let mut total = Reserved::ZERO;
        for node in self.nodes.iter() {
            total.add(of(node));
        }
        total
    }
}

fn sum(mut values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.try_fold(0u64, |total, value| total.checked_add(value?))
}

/// How many objects of each kind the feed holds, `None` for a kind it cannot rely on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    pub nodes: Option<usize>,
    pub pods: Option<usize>,
    pub deployments: Option<usize>,
    pub stateful_sets: Option<usize>,
    pub daemon_sets: Option<usize>,
    pub jobs: Option<usize>,
}

impl Inventory {
    pub(super) fn of(stores: &Stores) -> Self {
        Self {
            nodes: stores.nodes.current().map(<[_]>::len),
            pods: stores.pods.current().map(<[_]>::len),
            deployments: stores.deployments.current().map(<[_]>::len),
            stateful_sets: stores.stateful_sets.current().map(<[_]>::len),
            daemon_sets: stores.daemon_sets.current().map(<[_]>::len),
            jobs: stores.jobs.current().map(<[_]>::len),
        }
    }
}

/// The capacity of the nodes held, or `None` if they cannot be relied on. Without current pods the
/// nodes still show what they offer, with what is reserved unknown.
pub(super) fn capacity_of(stores: &Stores) -> Option<ClusterCapacity> {
    let nodes = stores.nodes.current()?;
    let tally = stores.pods.current_totals().map(|totals| &totals.tally);
    let mut rows: Vec<NodeCapacity> = nodes
        .iter()
        .filter_map(|node| {
            let name = node.metadata.name.as_deref()?;
            let totals = tally.map(|tally| tally.on(name));
            Some(NodeCapacity {
                name: name.to_owned(),
                allocatable: Allocatable::of(node),
                requests: totals.map_or_else(Reserved::default, |t| t.requests.reserved()),
                limits: totals.map_or_else(Reserved::default, |t| t.limits.reserved()),
                pods: totals.map(|t| t.pods),
            })
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Some(ClusterCapacity { nodes: rows.into() })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::KindCheck;
    use super::super::objects::Tracked;
    use super::*;

    fn spec(value: Value) -> PodSpec {
        serde_json::from_value(value).unwrap()
    }

    fn load_of(value: Value) -> PodLoad {
        PodLoad::of(&spec(value), None)
    }

    fn container(cpu: &str, memory: &str) -> Value {
        json!({"name": "c", "resources": {
            "requests": {"cpu": cpu, "memory": memory},
            "limits": {"cpu": cpu, "memory": memory},
        }})
    }

    fn pod(
        name: &str,
        node: Option<&str>,
        phase: &str,
        cpu: &str,
    ) -> (Pod, <Pod as Tracked>::Extra) {
        let mut pod: Pod = serde_json::from_value(json!({
            "metadata": {"name": name, "namespace": "demo"},
            "spec": {"nodeName": node, "containers": [container(cpu, "1Gi")]},
            "status": {"phase": phase},
        }))
        .unwrap();
        let load = pod.trim();
        (pod, load)
    }

    fn node(name: &str, cpu: &str) -> (Node, ()) {
        let mut node: Node = serde_json::from_value(json!({
            "metadata": {"name": name},
            "status": {"allocatable": {
                "cpu": cpu, "memory": "4Gi", "pods": "110", "hugepages-2Mi": "0"
            }},
        }))
        .unwrap();
        node.trim();
        (node, ())
    }

    fn stores(nodes: Vec<(Node, ())>, pods: Vec<(Pod, <Pod as Tracked>::Extra)>) -> Stores {
        let mut stores = Stores::default();
        stores.nodes.replace(nodes);
        stores.nodes.check = KindCheck::Current;
        stores.pods.replace(pods);
        stores.pods.check = KindCheck::Current;
        stores
    }

    /// A pod asking for `wanted` CPU while the node still runs it with `running`.
    fn resizing(
        wanted: &str,
        running: &str,
        reason: Option<&str>,
    ) -> (Pod, <Pod as Tracked>::Extra) {
        let conditions: Vec<Value> = reason
            .map(|reason| json!({"type": "PodResizePending", "status": "True", "reason": reason}))
            .into_iter()
            .collect();
        let mut pod: Pod = serde_json::from_value(json!({
            "metadata": {"name": "web", "namespace": "demo"},
            "spec": {"nodeName": "a", "containers": [container(wanted, "1Gi")]},
            "status": {"phase": "Running", "conditions": conditions, "containerStatuses": [{
                "name": "c", "image": "i", "imageID": "x", "ready": true, "restartCount": 0,
                "allocatedResources": {"cpu": running, "memory": "1Gi"},
                "resources": {
                    "requests": {"cpu": running, "memory": "1Gi"},
                    "limits": {"cpu": running, "memory": "1Gi"},
                },
            }]},
        }))
        .unwrap();
        let extra = pod.trim();
        (pod, extra)
    }

    fn cores(count: u64) -> Option<Cpu> {
        Some(Cpu::from_nanos(count * 1_000_000_000))
    }

    #[test]
    fn a_resize_down_holds_capacity_until_the_node_releases_it() {
        let stores = stores(vec![node("a", "8")], vec![resizing("1", "4", None)]);
        let capacity = capacity_of(&stores).unwrap();
        assert_eq!(capacity.requests().cpu, cores(4));
        assert_eq!(capacity.limits().cpu, cores(4));
    }

    #[test]
    fn a_resize_up_reserves_the_larger_amount_at_once() {
        let stores = stores(vec![node("a", "8")], vec![resizing("4", "1", None)]);
        assert_eq!(capacity_of(&stores).unwrap().requests().cpu, cores(4));
    }

    #[test]
    fn an_infeasible_resize_reserves_only_what_is_running() {
        let stores = stores(
            vec![node("a", "8")],
            vec![resizing("64", "2", Some("Infeasible"))],
        );
        assert_eq!(capacity_of(&stores).unwrap().requests().cpu, cores(2));
    }

    #[test]
    fn a_total_too_large_to_hold_is_unknown() {
        let pods = (0..20)
            .map(|i| {
                let mut pod: Pod = serde_json::from_value(json!({
                    "metadata": {"name": format!("p{i}"), "namespace": "demo"},
                    "spec": {"nodeName": "a", "containers": [container("1", "1Ei")]},
                    "status": {"phase": "Running"},
                }))
                .unwrap();
                let extra = pod.trim();
                (pod, extra)
            })
            .collect();
        let capacity = capacity_of(&stores(vec![node("a", "4")], pods)).unwrap();
        assert_eq!(capacity.requests().memory, None);
        assert_eq!(capacity.limits().memory, None);
        assert!(capacity.requests().cpu.is_some());
    }

    #[test]
    fn a_trimmed_pod_keeps_no_node_name() {
        let (pod, _) = pod("a", Some("a-rather-long-node-name"), "Running", "1");
        assert_eq!(pod.spec.unwrap().node_name, None);
    }

    #[test]
    fn containers_and_sidecars_add_up_and_a_larger_init_container_wins() {
        let load = load_of(json!({
            "containers": [container("100m", "1Mi"), container("200m", "1Mi")],
            "initContainers": [
                {"name": "side", "restartPolicy": "Always", "resources": {
                    "requests": {"cpu": "50m"}}},
                {"name": "init", "resources": {"requests": {"cpu": "1", "memory": "1Mi"}}},
            ],
        }));
        assert_eq!(load.requests.cpu, Some(1_050_000_000), "init plus sidecar");
        assert_eq!(load.requests.memory, Some(2 * 1024 * 1024));
    }

    #[test]
    fn pod_level_resources_replace_the_total_and_overhead_is_added() {
        let load = load_of(json!({
            "containers": [container("100m", "1Mi")],
            "resources": {"requests": {"cpu": "2"}},
            "overhead": {"cpu": "10m"},
        }));
        assert_eq!(load.requests.cpu, Some(2_010_000_000));
        assert_eq!(load.limits.cpu, Some(110_000_000));
    }

    #[test]
    fn overhead_does_not_invent_a_limit() {
        let load = load_of(json!({
            "containers": [{"name": "c"}],
            "overhead": {"cpu": "10m"},
        }));
        assert_eq!(load.requests.cpu, Some(10_000_000));
        assert_eq!(load.limits.cpu, Some(0));
    }

    #[test]
    fn a_request_that_cannot_be_read_is_unknown_not_zero() {
        let load = load_of(json!({"containers": [container("lots", "1Mi")]}));
        assert_eq!(load.requests.cpu, None);
        assert_eq!(load.requests.memory, Some(1024 * 1024));
    }

    #[test]
    fn only_scheduled_unfinished_pods_count_on_their_node() {
        let stores = stores(
            vec![node("a", "4"), node("b", "2")],
            vec![
                pod("run", Some("a"), "Running", "1"),
                pod("done", Some("a"), "Succeeded", "1"),
                pod("failed", Some("a"), "Failed", "1"),
                pod("waiting", None, "Pending", "1"),
                pod("gone", Some("elsewhere"), "Running", "1"),
            ],
        );
        let capacity = capacity_of(&stores).unwrap();
        let a = &capacity.nodes[0];
        assert_eq!(a.name, "a");
        assert_eq!(a.pods, Some(1));
        assert_eq!(a.requests.cpu, Some(Cpu::from_nanos(1_000_000_000)));
        assert_eq!(capacity.nodes[1].pods, Some(0));
        assert_eq!(capacity.pods(), Some(1));
        assert_eq!(
            capacity.allocatable().cpu,
            Some(Cpu::from_nanos(6_000_000_000))
        );
        assert_eq!(capacity.allocatable().pods, Some(220));
    }

    #[test]
    fn one_unreadable_request_makes_its_node_and_the_cluster_unknown() {
        let stores = stores(
            vec![node("a", "4"), node("b", "2")],
            vec![
                pod("bad", Some("a"), "Running", "lots"),
                pod("ok", Some("b"), "Running", "1"),
            ],
        );
        let capacity = capacity_of(&stores).unwrap();
        assert_eq!(capacity.nodes[0].requests.cpu, None);
        assert!(capacity.nodes[1].requests.cpu.is_some());
        assert_eq!(capacity.requests().cpu, None);
        assert!(capacity.requests().memory.is_some());
    }

    #[test]
    fn without_pods_nodes_show_what_they_offer_and_nothing_reserved() {
        let mut stores = stores(vec![node("a", "4")], Vec::new());
        stores.pods.invalidate(KindCheck::TooLarge);
        let capacity = capacity_of(&stores).unwrap();
        assert!(capacity.allocatable().cpu.is_some());
        assert_eq!(capacity.requests(), Reserved::default());
        assert_eq!(capacity.pods(), None);
    }

    #[test]
    fn without_nodes_there_is_no_capacity() {
        let mut stores = stores(vec![node("a", "4")], Vec::new());
        stores.nodes.invalidate(KindCheck::Starting);
        assert_eq!(capacity_of(&stores), None);
        assert_eq!(Inventory::of(&stores).nodes, None);
        assert_eq!(Inventory::of(&stores).pods, Some(0));
    }

    #[test]
    fn a_trimmed_node_keeps_only_the_allocatable_it_reports() {
        let (node, ()) = node("a", "4");
        let list = node.status.unwrap().allocatable.unwrap();
        let mut keys: Vec<&str> = list.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["cpu", "memory", "pods"]);
    }

    #[test]
    fn a_trimmed_pod_holds_no_resource_lists() {
        let (pod, claim) = pod("a", Some("n"), "Running", "1");
        let spec = pod.spec.unwrap();
        assert!(spec.containers[0].resources.is_none());
        assert_eq!(claim.claim.unwrap().load.requests.cpu, Some(1_000_000_000));
    }

    #[test]
    fn counting_pods_leaves_deadlines_unset_and_removal_keeps_the_moved_pod_counted() {
        let (first, first_claim) = pod("first", Some("a"), "Running", "1");
        let (last, last_claim) = pod("last", Some("a"), "Running", "2");
        let mut stores = stores(
            vec![node("a", "8")],
            vec![(first.clone(), first_claim), (last.clone(), last_claim)],
        );
        for pod in stores.pods.current().unwrap() {
            assert_eq!(pod.spec.as_ref().unwrap().active_deadline_seconds, None);
        }

        stores.pods.remove(&first);
        assert_eq!(capacity_of(&stores).unwrap().requests().cpu, cores(2));
        stores.pods.remove(&last);
        assert_eq!(capacity_of(&stores).unwrap().requests().cpu, cores(0));
    }

    #[test]
    fn totals_follow_a_pod_that_changes_moves_or_leaves() {
        let mut stores = stores(
            vec![node("a", "8"), node("b", "8")],
            vec![
                pod("one", Some("a"), "Running", "1"),
                pod("two", Some("a"), "Running", "2"),
            ],
        );
        let requested =
            |stores: &Stores, at: usize| capacity_of(stores).unwrap().nodes[at].requests.cpu;
        assert_eq!(requested(&stores, 0), cores(3));

        let (changed, claim) = pod("one", Some("a"), "Running", "4");
        assert!(stores.pods.upsert(changed, claim));
        assert_eq!(requested(&stores, 0), cores(6));

        let (moved, claim) = pod("two", Some("b"), "Running", "2");
        assert!(stores.pods.upsert(moved, claim));
        assert_eq!(requested(&stores, 0), cores(4));
        assert_eq!(requested(&stores, 1), cores(2));

        let (finished, claim) = pod("one", Some("a"), "Succeeded", "4");
        assert!(stores.pods.upsert(finished, claim));
        assert_eq!(requested(&stores, 0), cores(0));

        let (gone, _) = pod("two", Some("b"), "Running", "2");
        stores.pods.remove(&gone);
        let capacity = capacity_of(&stores).unwrap();
        assert_eq!(capacity.requests().cpu, cores(0));
        assert_eq!(capacity.pods(), Some(0));
    }

    #[test]
    fn an_unreadable_request_stops_counting_once_its_pod_leaves() {
        let mut stores = stores(
            vec![node("a", "8")],
            vec![pod("bad", Some("a"), "Running", "lots")],
        );
        assert_eq!(capacity_of(&stores).unwrap().requests().cpu, None);
        let (gone, _) = pod("bad", Some("a"), "Running", "lots");
        stores.pods.remove(&gone);
        assert_eq!(capacity_of(&stores).unwrap().requests().cpu, cores(0));
    }

    #[test]
    fn pods_with_the_same_load_share_one_entry() {
        let pods = (0..1000)
            .map(|i| pod(&format!("p{i}"), Some("a"), "Running", "1"))
            .collect();
        let stores = stores(vec![node("a", "8")], pods);
        let tally = &stores.pods.current_totals().unwrap().tally;
        assert_eq!(tally.loads.ids.len(), 1);
        assert_eq!(tally.nodes.ids.len(), 1);
    }
}
