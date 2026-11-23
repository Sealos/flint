//! How full a cluster is, and how much is in it, from the objects a feed already watches.
//!
//! A pod's requests and limits are reduced to a fixed-size [`PodLoad`] when it is trimmed, so a
//! large cluster holds a few numbers per pod and no resource lists. Amounts follow what the
//! scheduler reserves: containers and sidecars add up, an init container counts when it needs more
//! than that, pod-level resources replace the container total, and overhead is added.
//!
//! An amount that cannot be read is `None` and makes every total it belongs to `None`, so a
//! partial sum is never shown as the cluster's.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use k8s_openapi::api::core::v1::{Container, Node, Pod, PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use super::objects::Stores;
use crate::metrics::{Cpu, Memory};

/// The allocatable entries a trimmed node keeps.
pub(super) const ALLOCATABLE_KEYS: [&str; 3] = ["cpu", "memory", "pods"];

/// CPU in nanocores and memory in bytes, each `None` when it could not be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Demand {
    cpu: Option<u64>,
    memory: Option<u64>,
}

/// What one pod reserves, kept in place of its containers' resource lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PodLoad {
    requests: Demand,
    limits: Demand,
}

impl PodLoad {
    /// The load of `spec`.
    pub(super) fn of(spec: &PodSpec) -> Self {
        let cpu = |text: &str| text.parse::<Cpu>().ok().map(Cpu::nanos);
        let memory = |text: &str| text.parse::<Memory>().ok().map(Memory::bytes);
        Self {
            requests: Demand {
                cpu: effective(spec, "cpu", Side::Requests, &cpu),
                memory: effective(spec, "memory", Side::Requests, &memory),
            },
            limits: Demand {
                cpu: effective(spec, "cpu", Side::Limits, &cpu),
                memory: effective(spec, "memory", Side::Limits, &memory),
            },
        }
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

/// What `spec` reserves of `name`. Not setting it is zero; a value that does not parse is `None`.
fn effective(
    spec: &PodSpec,
    name: &str,
    side: Side,
    parse: &dyn Fn(&str) -> Option<u64>,
) -> Option<u64> {
    let read = |list: Option<&BTreeMap<String, Quantity>>| match list.and_then(|l| l.get(name)) {
        Some(quantity) => parse(&quantity.0),
        None => Some(0),
    };
    let of = |container: &Container| read(container.resources.as_ref().and_then(|r| side.of(r)));
    let pod_level = spec
        .resources
        .as_ref()
        .and_then(|r| side.of(r))
        .and_then(|list| list.get(name));
    let base = match pod_level {
        Some(quantity) => parse(&quantity.0)?,
        None => {
            let mut running = 0u64;
            for container in &spec.containers {
                running = running.saturating_add(of(container)?);
            }
            let (mut sidecars, mut peak) = (0u64, 0u64);
            for init in spec.init_containers.iter().flatten() {
                let amount = of(init)?;
                if init.restart_policy.as_deref() == Some("Always") {
                    sidecars = sidecars.saturating_add(amount);
                    peak = peak.max(sidecars);
                } else {
                    peak = peak.max(sidecars.saturating_add(amount));
                }
            }
            running.saturating_add(sidecars).max(peak)
        }
    };
    let overhead = read(spec.overhead.as_ref())?;
    // No limit means unbounded, and overhead does not turn that into a limit.
    Some(if side == Side::Limits && base == 0 {
        0
    } else {
        base.saturating_add(overhead)
    })
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

    fn add(&mut self, demand: Demand) {
        self.cpu = self
            .cpu
            .zip(demand.cpu)
            .map(|(a, b)| Cpu::from_nanos(a.nanos().saturating_add(b)));
        self.memory = self
            .memory
            .zip(demand.memory)
            .map(|(a, b)| Memory::from_bytes(a.bytes().saturating_add(b)));
    }

    fn add_reserved(&mut self, other: Self) {
        self.add(Demand {
            cpu: other.cpu.map(Cpu::nanos),
            memory: other.memory.map(Memory::bytes),
        });
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
            total.add_reserved(of(node));
        }
        total
    }
}

fn sum(mut values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.try_fold(0u64, |total, value| Some(total.saturating_add(value?)))
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

/// Whether a pod holds resources on a node: scheduled, and not finished.
fn holds_slot(pod: &Pod) -> Option<&str> {
    let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
    if matches!(phase, Some("Succeeded" | "Failed")) {
        return None;
    }
    pod.spec.as_ref()?.node_name.as_deref()
}

/// The capacity of the nodes held, or `None` if they cannot be relied on. Without current pods the
/// nodes still show what they offer, with what is reserved unknown.
pub(super) fn capacity_of(stores: &Stores) -> Option<ClusterCapacity> {
    let nodes = stores.nodes.current()?;
    let mut by_name: HashMap<&str, NodeCapacity> = HashMap::with_capacity(nodes.len());
    let pods_known = stores.pods.current().is_some();
    for node in nodes {
        let Some(name) = node.metadata.name.as_deref() else {
            continue;
        };
        let start = if pods_known {
            Reserved::ZERO
        } else {
            Reserved::default()
        };
        by_name.insert(
            name,
            NodeCapacity {
                name: name.to_owned(),
                allocatable: Allocatable::of(node),
                requests: start,
                limits: start,
                pods: pods_known.then_some(0),
            },
        );
    }
    for (pod, load) in stores.pods.current_with_extras().into_iter().flatten() {
        let Some(node) = holds_slot(pod).and_then(|name| by_name.get_mut(name)) else {
            continue;
        };
        node.requests.add(load.requests);
        node.limits.add(load.limits);
        node.pods = node.pods.map(|count| count + 1);
    }
    let mut rows: Vec<NodeCapacity> = by_name.into_values().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Some(ClusterCapacity { nodes: rows.into() })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::KindCheck;
    use super::super::objects::Tracked as _;
    use super::*;

    fn spec(value: Value) -> PodSpec {
        serde_json::from_value(value).unwrap()
    }

    fn container(cpu: &str, memory: &str) -> Value {
        json!({"name": "c", "resources": {
            "requests": {"cpu": cpu, "memory": memory},
            "limits": {"cpu": cpu, "memory": memory},
        }})
    }

    fn pod(name: &str, node: Option<&str>, phase: &str, cpu: &str) -> (Pod, PodLoad) {
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

    fn stores(nodes: Vec<(Node, ())>, pods: Vec<(Pod, PodLoad)>) -> Stores {
        let mut stores = Stores::default();
        stores.nodes.replace(nodes);
        stores.nodes.check = KindCheck::Current;
        stores.pods.replace(pods);
        stores.pods.check = KindCheck::Current;
        stores
    }

    #[test]
    fn containers_and_sidecars_add_up_and_a_larger_init_container_wins() {
        let load = PodLoad::of(&spec(json!({
            "containers": [container("100m", "1Mi"), container("200m", "1Mi")],
            "initContainers": [
                {"name": "side", "restartPolicy": "Always", "resources": {
                    "requests": {"cpu": "50m"}}},
                {"name": "init", "resources": {"requests": {"cpu": "1", "memory": "1Mi"}}},
            ],
        })));
        assert_eq!(load.requests.cpu, Some(1_050_000_000), "init plus sidecar");
        assert_eq!(load.requests.memory, Some(2 * 1024 * 1024));
    }

    #[test]
    fn pod_level_resources_replace_the_total_and_overhead_is_added() {
        let load = PodLoad::of(&spec(json!({
            "containers": [container("100m", "1Mi")],
            "resources": {"requests": {"cpu": "2"}},
            "overhead": {"cpu": "10m"},
        })));
        assert_eq!(load.requests.cpu, Some(2_010_000_000));
        assert_eq!(load.limits.cpu, Some(110_000_000));
    }

    #[test]
    fn overhead_does_not_invent_a_limit() {
        let load = PodLoad::of(&spec(json!({
            "containers": [{"name": "c"}],
            "overhead": {"cpu": "10m"},
        })));
        assert_eq!(load.requests.cpu, Some(10_000_000));
        assert_eq!(load.limits.cpu, Some(0));
    }

    #[test]
    fn a_request_that_cannot_be_read_is_unknown_not_zero() {
        let load = PodLoad::of(&spec(json!({"containers": [container("lots", "1Mi")]})));
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
        let (pod, load) = pod("a", Some("n"), "Running", "1");
        let spec = pod.spec.unwrap();
        assert_eq!(spec.node_name.as_deref(), Some("n"));
        assert!(spec.containers[0].resources.is_none());
        assert_eq!(load.requests.cpu, Some(1_000_000_000));
    }
}
