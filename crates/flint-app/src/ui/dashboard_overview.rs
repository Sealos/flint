//! The overview's detail: individual problems, capacity left per resource and node, and what the
//! cluster holds. Plain view models, built off the UI thread from one telemetry snapshot.
//!
//! Anything that could not be read says so. Usage without metrics-server is unavailable rather
//! than zero, and requests do not depend on it.

use std::cmp::Reverse;
use std::collections::HashSet;

use flint_core::metrics::{Share, Usage, share};
use flint_core::problems::{Problem, ProblemId, ProblemSeverity, ProblemSubject, WorkloadKind};
use flint_core::telemetry::{ClusterCapacity, Inventory, NodeCapacity, UsageRead, VersionCheck};

use super::super::registry::{OpenObject, OpenView};
use super::{NO_VALUE, cleaned, format_cpu, format_memory};

/// The most individual problems listed.
pub const MAX_PROBLEM_ROWS: usize = 20;
/// The most nodes in the allocation table.
pub const MAX_NODE_ROWS: usize = 50;
/// What a requested amount shows when it could not be read.
pub const UNKNOWN: &str = "unknown";
/// What usage shows when metrics do not cover it. Never zero.
pub const USAGE_UNAVAILABLE: &str = "unavailable";
/// What the problem list says before anything could be read.
pub const PROBLEMS_UNKNOWN: &str = "Individual problems cannot be checked yet.";
/// What the problem list says when every kind was read and nothing is wrong.
pub const NO_PROBLEMS: &str = "No problems found.";
/// What the problem list says when nothing is wrong in the kinds that could be read.
pub const NO_PROBLEMS_SO_FAR: &str = "No problems in what could be checked.";
/// What the capacity section says while nodes cannot be read.
pub const CAPACITY_UNKNOWN: &str = "Capacity cannot be checked yet.";

/// The active cluster a row was built for, so that a click from an old frame cannot open an
/// object on another cluster or API server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenScope {
    pub cluster: String,
    pub server: Option<String>,
}

/// The ranked list of individual problems.
#[derive(Clone, Debug, PartialEq)]
pub enum ProblemsPanel {
    /// Nothing could be read.
    Unknown,
    Rows {
        /// Most severe first, at most [`MAX_PROBLEM_ROWS`], out of `total`.
        rows: Vec<ProblemRow>,
        total: usize,
        /// Problems left out because they may still settle by themselves.
        settling: usize,
        /// Some problem kinds could not be read.
        incomplete: bool,
    },
}

/// One problem and the object it opens.
#[derive(Clone, Debug, PartialEq)]
pub struct ProblemRow {
    pub severity: ProblemSeverity,
    pub summary: &'static str,
    /// `Pod shop/web-0 (app)`.
    pub object: String,
    pub open: OpenObject,
}

impl ProblemRow {
    /// The row in one line, for accessibility and tests.
    pub fn label(&self) -> String {
        let severity = match self.severity {
            ProblemSeverity::Critical => "Critical",
            ProblemSeverity::Warning => "Warning",
        };
        format!("{severity}: {} on {}", self.summary, self.object)
    }
}

/// The kind as `OpenObject` takes it, the kind as shown, and the object's place.
fn subject_parts(subject: &ProblemSubject) -> (&'static str, &'static str, &str, &str) {
    match subject {
        ProblemSubject::Container { namespace, pod, .. } => ("pods", "Pod", namespace, pod),
        ProblemSubject::Pod {
            namespace, name, ..
        } => ("pods", "Pod", namespace, name),
        ProblemSubject::Node { name, .. } => ("nodes", "Node", "", name),
        ProblemSubject::Job {
            namespace, name, ..
        } => ("jobs", "Job", namespace, name),
        ProblemSubject::Workload {
            kind,
            namespace,
            name,
            ..
        } => {
            let (open, shown) = match kind {
                WorkloadKind::Deployment => ("deployments", "Deployment"),
                WorkloadKind::StatefulSet => ("statefulsets", "StatefulSet"),
                WorkloadKind::DaemonSet => ("daemonsets", "DaemonSet"),
            };
            (open, shown, namespace, name)
        }
    }
}

fn problem_row(problem: &Problem, scope: &OpenScope) -> ProblemRow {
    let (kind, shown, namespace, name) = subject_parts(&problem.id.subject);
    let place = if namespace.is_empty() {
        cleaned(name)
    } else {
        format!("{}/{}", cleaned(namespace), cleaned(name))
    };
    let object = match &problem.id.subject {
        ProblemSubject::Container { container, .. } => {
            format!("{shown} {place} ({})", cleaned(container))
        }
        _ => format!("{shown} {place}"),
    };
    ProblemRow {
        severity: problem.severity(),
        summary: problem.summary(),
        object,
        open: OpenObject {
            kind: kind.to_owned(),
            namespace: (!namespace.is_empty()).then(|| namespace.to_owned()),
            name: name.to_owned(),
            cluster: Some(scope.cluster.clone()),
            server: scope.server.clone(),
        },
    }
}

/// The problems to list: most severe, then newest, then by name. Those still in their hold-down
/// are not counted by the tiles, so they are not listed either.
pub fn problems_panel(
    problems: &[Problem],
    settling: &[ProblemId],
    readable: bool,
    incomplete: bool,
    scope: &OpenScope,
) -> ProblemsPanel {
    if !readable {
        return ProblemsPanel::Unknown;
    }
    let held: HashSet<&ProblemId> = settling.iter().collect();
    let mut listed: Vec<&Problem> = problems
        .iter()
        .filter(|problem| !held.contains(&problem.id))
        .collect();
    let total = listed.len();
    listed.sort_by_cached_key(|problem| {
        let (kind, _, namespace, name) = subject_parts(&problem.id.subject);
        (
            Reverse(problem.severity()),
            Reverse(problem.since),
            kind,
            namespace.to_owned(),
            name.to_owned(),
            problem.id.kind,
        )
    });
    ProblemsPanel::Rows {
        rows: listed
            .into_iter()
            .take(MAX_PROBLEM_ROWS)
            .map(|problem| problem_row(problem, scope))
            .collect(),
        total,
        settling: problems.len() - total,
        incomplete,
    }
}

/// How much of one resource is left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityCard {
    pub label: &'static str,
    pub left: String,
    pub allocatable: String,
    pub requested: String,
    /// How much of the bar is requested.
    pub requested_percent: Option<u32>,
    /// `None` for a resource with no live usage, such as pod slots.
    pub used: Option<String>,
    /// `None` for a resource with no limits, such as pod slots.
    pub limits: Option<String>,
}

impl CapacityCard {
    /// The card in one line, for accessibility and tests.
    pub fn description(&self) -> String {
        let mut text = format!(
            "{}: {} left of {}, requested {}",
            self.label, self.left, self.allocatable, self.requested
        );
        if let Some(used) = &self.used {
            text.push_str(&format!(", used {used}"));
        }
        if let Some(limits) = &self.limits {
            text.push_str(&format!(", limits {limits}"));
        }
        text
    }
}

/// One row of the allocation table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    pub name: String,
    pub cpu_requested: String,
    pub memory_requested: String,
    pub pods: String,
    pub cpu_used: String,
    pub memory_used: String,
}

impl NodeRow {
    /// The row in one line, for accessibility and tests.
    pub fn label(&self) -> String {
        format!(
            "{}: CPU requested {}, memory requested {}, pods {}, CPU used {}, memory used {}",
            self.name,
            self.cpu_requested,
            self.memory_requested,
            self.pods,
            self.cpu_used,
            self.memory_used
        )
    }
}

/// The capacity section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapacityPanel {
    /// Nodes could not be read.
    Unknown,
    Known {
        cards: Box<[CapacityCard; 3]>,
        /// Most allocated first, at most [`MAX_NODE_ROWS`], out of `total_nodes`.
        rows: Vec<NodeRow>,
        total_nodes: usize,
        /// Why the cluster's usage is unavailable though some nodes report it.
        usage_note: Option<String>,
    },
}

fn percent(part: u64, whole: Option<u64>) -> Option<u32> {
    match share(part, whole) {
        Share::Percent(percent) => Some(percent),
        Share::NotSet | Share::ZeroBudget => None,
    }
}

fn amount(value: Option<u64>, whole: Option<u64>, format: fn(u64) -> String) -> String {
    match value {
        None => UNKNOWN.to_owned(),
        Some(value) => match percent(value, whole) {
            Some(percent) => format!("{} ({percent}%)", format(value)),
            None => format(value),
        },
    }
}

fn used(value: Option<u64>, whole: Option<u64>, format: fn(u64) -> String) -> String {
    value.map_or_else(
        || USAGE_UNAVAILABLE.to_owned(),
        |value| amount(Some(value), whole, format),
    )
}

fn count(value: u64) -> String {
    value.to_string()
}

fn card(
    label: &'static str,
    allocatable: Option<u64>,
    requested: Option<u64>,
    format: fn(u64) -> String,
) -> CapacityCard {
    let left = allocatable
        .zip(requested)
        .map(|(whole, part)| whole.saturating_sub(part));
    CapacityCard {
        label,
        left: left.map_or_else(|| UNKNOWN.to_owned(), format),
        allocatable: allocatable.map_or_else(|| UNKNOWN.to_owned(), format),
        requested: amount(requested, allocatable, format),
        requested_percent: requested.and_then(|part| percent(part, allocatable)),
        used: None,
        limits: None,
    }
}

/// The highest share of a node that is requested, to rank it. `None` if no share is known.
fn allocated_rank(node: &NodeCapacity) -> Option<u32> {
    let cpu = node.requests.cpu.and_then(|part| {
        percent(
            part.nanos(),
            node.allocatable.cpu.map(|whole| whole.nanos()),
        )
    });
    let memory = node.requests.memory.and_then(|part| {
        percent(
            part.bytes(),
            node.allocatable.memory.map(|whole| whole.bytes()),
        )
    });
    let pods = node
        .pods
        .and_then(|part| percent(part, node.allocatable.pods));
    [cpu, memory, pods].into_iter().flatten().max()
}

fn node_row(node: &NodeCapacity, usage: Option<Usage>) -> NodeRow {
    let cpu_whole = node.allocatable.cpu.map(|cpu| cpu.nanos());
    let memory_whole = node.allocatable.memory.map(|memory| memory.bytes());
    let part = |value: Option<u64>| value.map_or_else(|| UNKNOWN.to_owned(), count);
    NodeRow {
        name: cleaned(&node.name),
        cpu_requested: amount(node.requests.cpu.map(|c| c.nanos()), cpu_whole, format_cpu),
        memory_requested: amount(
            node.requests.memory.map(|m| m.bytes()),
            memory_whole,
            format_memory,
        ),
        pods: format!("{} / {}", part(node.pods), part(node.allocatable.pods)),
        cpu_used: used(usage.map(|u| u.cpu.nanos()), cpu_whole, format_cpu),
        memory_used: used(usage.map(|u| u.memory.bytes()), memory_whole, format_memory),
    }
}

/// The capacity of `capacity`, with live usage where `usage` covers every node counted.
pub fn capacity_panel(
    capacity: Option<&ClusterCapacity>,
    usage: &UsageRead<String>,
) -> CapacityPanel {
    let Some(capacity) = capacity else {
        return CapacityPanel::Unknown;
    };
    let nodes = &capacity.nodes;
    let per_node: Vec<Option<Usage>> = nodes.iter().map(|n| usage.get(&n.name)).collect();
    let missing = per_node.iter().filter(|u| u.is_none()).count();
    let total_usage = (missing == 0 && !nodes.is_empty()).then(|| {
        per_node.iter().flatten().fold((0u64, 0u64), |(c, m), u| {
            (
                c.saturating_add(u.cpu.nanos()),
                m.saturating_add(u.memory.bytes()),
            )
        })
    });
    let usage_note = (matches!(usage, UsageRead::Current(_)) && missing > 0).then(|| {
        format!(
            "Usage is missing for {missing} of {} nodes, so the cluster total is unavailable.",
            nodes.len()
        )
    });

    let (allocatable, requests, limits) = (
        capacity.allocatable(),
        capacity.requests(),
        capacity.limits(),
    );
    let cpu_whole = allocatable.cpu.map(|cpu| cpu.nanos());
    let memory_whole = allocatable.memory.map(|memory| memory.bytes());
    let cpu = CapacityCard {
        used: Some(used(total_usage.map(|t| t.0), cpu_whole, format_cpu)),
        limits: Some(amount(limits.cpu.map(|c| c.nanos()), cpu_whole, format_cpu)),
        ..card(
            "CPU",
            cpu_whole,
            requests.cpu.map(|c| c.nanos()),
            format_cpu,
        )
    };
    let memory = CapacityCard {
        used: Some(used(total_usage.map(|t| t.1), memory_whole, format_memory)),
        limits: Some(amount(
            limits.memory.map(|m| m.bytes()),
            memory_whole,
            format_memory,
        )),
        ..card(
            "Memory",
            memory_whole,
            requests.memory.map(|m| m.bytes()),
            format_memory,
        )
    };
    let pods = card("Pod slots", allocatable.pods, capacity.pods(), count);

    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by_cached_key(|&at| (Reverse(allocated_rank(&nodes[at])), nodes[at].name.clone()));
    let rows = order
        .into_iter()
        .take(MAX_NODE_ROWS)
        .map(|at| node_row(&nodes[at], per_node[at]))
        .collect();
    CapacityPanel::Known {
        cards: Box::new([cpu, memory, pods]),
        rows,
        total_nodes: nodes.len(),
        usage_note,
    }
}

/// One clickable count of what the cluster holds.
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryCount {
    pub label: &'static str,
    /// `None` when its kind could not be read: it then opens nothing.
    pub count: Option<usize>,
    pub open: OpenView,
}

impl InventoryCount {
    /// The number as shown.
    pub fn value(&self) -> String {
        self.count
            .map_or_else(|| NO_VALUE.to_owned(), |count| count.to_string())
    }

    /// The count in one line, for accessibility and tests.
    pub fn description(&self) -> String {
        match self.count {
            Some(count) => format!("{}: {count}", self.label),
            None => format!("{}: {UNKNOWN}", self.label),
        }
    }
}

/// What is in the cluster.
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryPanel {
    pub version: String,
    pub counts: Vec<InventoryCount>,
}

/// The inventory of one snapshot.
pub fn inventory_panel(
    inventory: &Inventory,
    version: &VersionCheck,
    scope: &OpenScope,
) -> InventoryPanel {
    let entry = |label, kind: &str, count| InventoryCount {
        label,
        count,
        open: OpenView {
            kind: kind.to_owned(),
            cluster: Some(scope.cluster.clone()),
            server: scope.server.clone(),
        },
    };
    InventoryPanel {
        version: match version {
            VersionCheck::Waiting => "Checking…".to_owned(),
            VersionCheck::Known(version) => cleaned(version),
            VersionCheck::Unknown => "Unknown".to_owned(),
        },
        counts: vec![
            entry("Nodes", "nodes", inventory.nodes),
            entry("Pods", "pods", inventory.pods),
            entry("Deployments", "deployments", inventory.deployments),
            entry("StatefulSets", "statefulsets", inventory.stateful_sets),
            entry("DaemonSets", "daemonsets", inventory.daemon_sets),
            entry("Jobs", "jobs", inventory.jobs),
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use flint_core::metrics::{Cpu, Memory, MetricsError};
    use flint_core::problems::ProblemKind;
    use flint_core::telemetry::{Allocatable, Reserved};
    use jiff::Timestamp;

    use super::*;

    const CORE: u64 = 1_000_000_000;
    const GIB: u64 = 1 << 30;

    fn scope() -> OpenScope {
        OpenScope {
            cluster: "one".into(),
            server: Some("https://one".into()),
        }
    }

    fn problem(kind: ProblemKind, subject: ProblemSubject, since: Option<i64>) -> Problem {
        Problem {
            id: ProblemId {
                cluster: Arc::from("one"),
                kind,
                subject,
            },
            owner: None,
            owner_known: true,
            reason: None,
            since: since.map(|s| Timestamp::from_second(s).unwrap()),
        }
    }

    fn job(name: &str, since: i64) -> Problem {
        problem(
            ProblemKind::JobFailed,
            ProblemSubject::Job {
                namespace: "shop".into(),
                name: name.into(),
                uid: None,
            },
            Some(since),
        )
    }

    fn node_problem(name: &str) -> Problem {
        problem(
            ProblemKind::NodeNotReady,
            ProblemSubject::Node {
                name: name.into(),
                uid: None,
            },
            Some(1),
        )
    }

    fn rows(panel: &ProblemsPanel) -> &[ProblemRow] {
        match panel {
            ProblemsPanel::Rows { rows, .. } => rows,
            ProblemsPanel::Unknown => panic!("unknown"),
        }
    }

    #[test]
    fn problems_rank_by_severity_then_newest_and_open_their_exact_object() {
        let problems = [job("old", 10), job("new", 20), node_problem("node-1")];
        let panel = problems_panel(&problems, &[], true, false, &scope());
        let rows = rows(&panel);
        assert_eq!(rows[0].object, "Node node-1");
        assert_eq!(rows[1].object, "Job shop/new");
        assert_eq!(rows[2].object, "Job shop/old");
        assert_eq!(
            rows[1].open,
            OpenObject {
                kind: "jobs".into(),
                namespace: Some("shop".into()),
                name: "new".into(),
                cluster: Some("one".into()),
                server: Some("https://one".into()),
            }
        );
        assert_eq!(rows[0].open.namespace, None);
    }

    #[test]
    fn a_container_problem_opens_its_pod_and_names_the_container() {
        let crash = problem(
            ProblemKind::CrashLoop,
            ProblemSubject::Container {
                namespace: "shop".into(),
                pod: "web-0".into(),
                pod_uid: None,
                container: "app".into(),
            },
            None,
        );
        let panel = problems_panel(&[crash], &[], true, false, &scope());
        let row = &rows(&panel)[0];
        assert_eq!(row.object, "Pod shop/web-0 (app)");
        assert_eq!(row.open.kind, "pods");
        assert_eq!(row.open.name, "web-0");
    }

    #[test]
    fn a_problem_still_settling_is_counted_apart_and_not_listed() {
        let problems = [job("a", 1), job("b", 2)];
        let settling = [problems[0].id.clone()];
        let panel = problems_panel(&problems, &settling, true, false, &scope());
        let ProblemsPanel::Rows {
            rows,
            total,
            settling,
            ..
        } = panel
        else {
            panic!("unknown");
        };
        assert_eq!((rows.len(), total, settling), (1, 1, 1));
        assert_eq!(rows[0].open.name, "b");
    }

    #[test]
    fn the_list_is_bounded_and_keeps_the_total() {
        let problems: Vec<Problem> = (0..MAX_PROBLEM_ROWS as i64 + 5)
            .map(|i| job(&format!("job-{i}"), i))
            .collect();
        let panel = problems_panel(&problems, &[], true, false, &scope());
        let ProblemsPanel::Rows { rows, total, .. } = panel else {
            panic!("unknown");
        };
        assert_eq!(rows.len(), MAX_PROBLEM_ROWS);
        assert_eq!(total, MAX_PROBLEM_ROWS + 5);
    }

    #[test]
    fn an_unreadable_cluster_has_no_problem_list_rather_than_an_empty_one() {
        assert_eq!(
            problems_panel(&[], &[], false, true, &scope()),
            ProblemsPanel::Unknown
        );
    }

    fn node(name: &str, cpu: u64, requested_cores: u64, pods: u64) -> NodeCapacity {
        NodeCapacity {
            name: name.into(),
            allocatable: Allocatable {
                cpu: Some(Cpu::from_nanos(cpu * CORE)),
                memory: Some(Memory::from_bytes(8 * GIB)),
                pods: Some(100),
            },
            requests: Reserved {
                cpu: Some(Cpu::from_nanos(requested_cores * CORE)),
                memory: Some(Memory::from_bytes(GIB)),
            },
            limits: Reserved {
                cpu: Some(Cpu::from_nanos(requested_cores * CORE)),
                memory: Some(Memory::from_bytes(GIB)),
            },
            pods: Some(pods),
        }
    }

    fn capacity(nodes: Vec<NodeCapacity>) -> ClusterCapacity {
        ClusterCapacity {
            nodes: nodes.into(),
        }
    }

    fn usage(entries: &[(&str, u64)]) -> UsageRead<String> {
        let map: BTreeMap<String, Usage> = entries
            .iter()
            .map(|(name, cores)| {
                (
                    (*name).to_owned(),
                    Usage {
                        cpu: Cpu::from_nanos(cores * CORE),
                        memory: Memory::from_bytes(GIB),
                    },
                )
            })
            .collect();
        UsageRead::Current(Arc::new(map))
    }

    fn known(panel: CapacityPanel) -> ([CapacityCard; 3], Vec<NodeRow>, usize, Option<String>) {
        match panel {
            CapacityPanel::Known {
                cards,
                rows,
                total_nodes,
                usage_note,
            } => (*cards, rows, total_nodes, usage_note),
            CapacityPanel::Unknown => panic!("unknown"),
        }
    }

    #[test]
    fn requests_render_without_metrics_server_and_usage_is_unavailable_not_zero() {
        let capacity = capacity(vec![node("a", 4, 1, 10)]);
        let read = UsageRead::Unavailable(MetricsError::NotInstalled);
        let (cards, rows, _, note) = known(capacity_panel(Some(&capacity), &read));
        assert_eq!(cards[0].left, "3.00 cores");
        assert_eq!(cards[0].requested, "1.00 cores (25%)");
        assert_eq!(cards[0].used.as_deref(), Some(USAGE_UNAVAILABLE));
        assert_eq!(cards[1].used.as_deref(), Some(USAGE_UNAVAILABLE));
        assert_eq!(rows[0].cpu_requested, "1.00 cores (25%)");
        assert_eq!(rows[0].cpu_used, USAGE_UNAVAILABLE);
        assert_eq!(rows[0].memory_used, USAGE_UNAVAILABLE);
        assert_eq!(note, None, "the install hint covers a missing metrics API");
    }

    #[test]
    fn a_node_without_metrics_makes_the_total_unavailable_and_keeps_the_others() {
        let capacity = capacity(vec![node("a", 4, 1, 1), node("b", 4, 1, 1)]);
        let (cards, rows, _, note) = known(capacity_panel(Some(&capacity), &usage(&[("a", 2)])));
        assert_eq!(cards[0].used.as_deref(), Some(USAGE_UNAVAILABLE));
        assert_eq!(rows[0].cpu_used, "2.00 cores (50%)");
        assert_eq!(rows[1].cpu_used, USAGE_UNAVAILABLE);
        assert!(note.unwrap().contains("1 of 2"));
    }

    #[test]
    fn usage_totals_every_node_when_all_report() {
        let capacity = capacity(vec![node("a", 4, 1, 1), node("b", 4, 1, 1)]);
        let read = usage(&[("a", 1), ("b", 3)]);
        let (cards, ..) = known(capacity_panel(Some(&capacity), &read));
        assert_eq!(cards[0].used.as_deref(), Some("4.00 cores (50%)"));
    }

    #[test]
    fn nodes_sort_by_their_most_allocated_resource_with_unknown_last() {
        let mut unknown = node("a-unknown", 4, 0, 0);
        unknown.requests = Reserved::default();
        unknown.pods = None;
        let capacity = capacity(vec![
            unknown,
            node("b-low", 4, 1, 1),
            node("c-high", 4, 3, 1),
            node("d-pods", 4, 1, 90),
        ]);
        let (_, rows, ..) = known(capacity_panel(Some(&capacity), &UsageRead::Waiting));
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["d-pods", "c-high", "b-low", "a-unknown"]);
        assert_eq!(rows[3].cpu_requested, UNKNOWN);
        assert_eq!(rows[3].pods, "unknown / 100");
    }

    #[test]
    fn unknown_requests_leave_what_is_left_unknown() {
        let mut one = node("a", 4, 1, 1);
        one.requests.cpu = None;
        let capacity = capacity(vec![one]);
        let (cards, ..) = known(capacity_panel(Some(&capacity), &UsageRead::Waiting));
        assert_eq!(cards[0].left, UNKNOWN);
        assert_eq!(cards[0].requested, UNKNOWN);
        assert_eq!(cards[0].requested_percent, None);
        assert_eq!(cards[0].allocatable, "4.00 cores");
    }

    #[test]
    fn the_table_is_bounded_and_keeps_the_node_count() {
        let nodes = (0..MAX_NODE_ROWS + 3)
            .map(|i| node(&format!("n-{i}"), 4, 1, 1))
            .collect();
        let capacity = capacity(nodes);
        let (_, rows, total, _) = known(capacity_panel(Some(&capacity), &UsageRead::Waiting));
        assert_eq!(rows.len(), MAX_NODE_ROWS);
        assert_eq!(total, MAX_NODE_ROWS + 3);
    }

    #[test]
    fn no_readable_nodes_is_unknown_capacity() {
        assert_eq!(
            capacity_panel(None, &UsageRead::Waiting),
            CapacityPanel::Unknown
        );
    }

    #[test]
    fn inventory_counts_open_their_kind_here_and_unknown_is_not_zero() {
        let inventory = Inventory {
            nodes: Some(3),
            pods: None,
            ..Inventory::default()
        };
        let panel = inventory_panel(&inventory, &VersionCheck::Known("v1.31.2".into()), &scope());
        assert_eq!(panel.version, "v1.31.2");
        assert_eq!(panel.counts[0].description(), "Nodes: 3");
        assert_eq!(panel.counts[0].open.kind, "nodes");
        assert_eq!(panel.counts[0].open.cluster.as_deref(), Some("one"));
        assert_eq!(panel.counts[1].value(), NO_VALUE);
        assert_eq!(panel.counts[1].description(), "Pods: unknown");
    }

    #[test]
    fn a_version_that_failed_reads_as_unknown() {
        let panel = inventory_panel(&Inventory::default(), &VersionCheck::Unknown, &scope());
        assert_eq!(panel.version, "Unknown");
    }
}
