//! A pod's or node's current use against its requests, limits or allocatable, from the cluster
//! feed's metrics poll, as the rings of the Usage section ([`super::ring`]). The pane makes no
//! request of its own.

use std::collections::BTreeMap;

use flint_core::metrics::{Amounts, Cpu, Memory, PodBudget, PodBudgetCheck, Share, Usage, share};
use flint_core::thresholds::{ThresholdMetric, Thresholds};
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use serde_json::Value;

use super::super::list_usage::{
    UNKNOWN_TEXT, UsageSource, UsageSubject, cpu_amount, memory_amount,
};
use super::super::list_usage_workloads::Totals;
use super::ring::RingSpec;

/// What the object's own definition gives to compare use with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Budget {
    Pod(PodBudgetCheck),
    Node(Amounts),
}

/// What the Usage section shows.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum UsagePanel {
    Hint(&'static str),
    Rings { cpu: UsageRing, memory: UsageRing },
}

/// One metric of the Usage section: a ring, or only text when there is nothing to compare with.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct UsageRing {
    pub label: &'static str,
    pub spec: Option<RingSpec>,
    /// What the middle of the ring says: the share of the limit, request or allocatable. Without
    /// a ring, the amount in use, or `unknown`.
    pub center: String,
    /// The exact amounts and shares, one per line.
    pub lines: Vec<String>,
}

/// The usage of the object a detail pane shows.
#[derive(Debug, Default)]
pub(super) struct DetailUsage {
    source: UsageSource,
    budget: Option<Budget>,
    /// The warning thresholds of the object's cluster, from Settings.
    thresholds: Thresholds,
    /// The object's `creationTimestamp`, Unix seconds: a sample from before it is another's.
    created: Option<i64>,
    /// The object was deleted, so a reading under its name is now another object's.
    deleted: bool,
    /// A workload's uid, which names its pods' owner.
    uid: Option<Box<str>>,
    /// A workload's totals over its pods, worked out when the poll or the object changes rather
    /// than on every frame.
    workload: WorkloadReading,
}

/// What the pods of the workload the pane shows add up to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum WorkloadReading {
    /// Not worked out: the pane shows no workload, or has no uid for it yet.
    #[default]
    Unread,
    /// No pod is running and reporting.
    Idle,
    Totals(Totals),
}

impl DetailUsage {
    /// Takes the poll's newest state. `true` if it changes what the pane shows.
    pub(super) fn set_source(&mut self, source: UsageSource) -> bool {
        if self.deleted {
            return false;
        }
        let changed = self.source != source;
        self.source = source;
        // Only when the readings changed: adding up a workload's pods is not free.
        if changed {
            self.read_workload();
        }
        changed
    }

    /// Adds up the workload's pods, if the pane shows one.
    fn read_workload(&mut self) {
        self.workload = match (&self.source, &self.uid) {
            (UsageSource::Workloads(..), Some(uid)) => match self.source.workload_totals(uid) {
                Some(totals) => WorkloadReading::Totals(totals),
                None => WorkloadReading::Idle,
            },
            _ => WorkloadReading::Unread,
        };
    }

    /// Takes the thresholds in force for the object's cluster. `true` if they change the pane.
    pub(super) fn set_thresholds(&mut self, thresholds: Thresholds) -> bool {
        let changed = self.thresholds != thresholds;
        self.thresholds = thresholds;
        changed
    }

    /// The object was deleted: its usage is unknown from now on.
    pub(super) fn mark_deleted(&mut self) {
        self.deleted = true;
        self.source = UsageSource::Unknown;
        self.read_workload();
    }

    /// Reads the budget from the fetched object. One that cannot be read stays unknown.
    pub(super) fn set_object(&mut self, subject: Option<UsageSubject>, object: &Value) {
        self.created = object
            .pointer("/metadata/creationTimestamp")
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<jiff::Timestamp>().ok())
            .map(jiff::Timestamp::as_second);
        self.uid = object
            .pointer("/metadata/uid")
            .and_then(Value::as_str)
            .map(Into::into);
        self.read_workload();
        self.budget = match subject {
            Some(UsageSubject::Pods) => object
                .get("spec")
                .cloned()
                .and_then(|spec| serde_json::from_value::<PodSpec>(spec).ok())
                .map(|spec| Budget::Pod(PodBudgetCheck::of(&spec))),
            Some(UsageSubject::Nodes) => object
                .pointer("/status/allocatable")
                .cloned()
                .and_then(|list| serde_json::from_value::<BTreeMap<String, Quantity>>(list).ok())
                .map(|list| Budget::Node(Amounts::from_quantities(&list))),
            Some(UsageSubject::Workloads(_)) | None => None,
        };
    }

    /// What to show for the object, or `None` while there is nothing to say.
    pub(super) fn panel(&self, namespace: Option<&str>, name: &str) -> Option<UsagePanel> {
        match &self.source {
            UsageSource::Hidden => None,
            UsageSource::NoFeed | UsageSource::NotInstalled => {
                self.source.hint().map(UsagePanel::Hint)
            }
            UsageSource::Unknown => Some(unknown_rings()),
            UsageSource::Workloads(..) => Some(match self.workload {
                WorkloadReading::Totals(totals) => workload_rings(totals, &self.thresholds),
                WorkloadReading::Idle => idle_rings(),
                WorkloadReading::Unread => unknown_rings(),
            }),
            UsageSource::Nodes(usage, capacity) => Some(match usage.get(name) {
                Some(&used) => {
                    let allocatable = match self.budget {
                        Some(Budget::Node(amounts)) => Some(amounts),
                        _ => None,
                    };
                    // What the node's pods request, once the feed has read the pods.
                    let requests = capacity
                        .as_ref()
                        .and_then(|capacity| capacity.node(name))
                        .map(|node| node.requests)
                        .unwrap_or_default();
                    node_rings(
                        used,
                        allocatable,
                        Amounts {
                            cpu: requests.cpu,
                            memory: requests.memory,
                        },
                        &self.thresholds,
                    )
                }
                None => unknown_rings(),
            }),
            UsageSource::Pods(usage, _) => {
                let key = (namespace?.to_owned(), name.to_owned());
                let sample = usage
                    .get(&key)
                    .filter(|sample| sample.follows_creation(self.created));
                Some(match sample {
                    Some(sample) => {
                        let budget = match &self.budget {
                            Some(Budget::Pod(check)) => check.for_sample(sample),
                            _ => None,
                        };
                        pod_rings(sample.usage, budget, &self.thresholds)
                    }
                    None => unknown_rings(),
                })
            }
        }
    }
}

/// A workload with no pod running and reporting.
fn idle_rings() -> UsagePanel {
    let idle = |label| UsageRing {
        label,
        spec: None,
        center: "none".to_owned(),
        lines: vec!["no pod is running and reporting".to_owned()],
    };
    UsagePanel::Rings {
        cpu: idle("CPU"),
        memory: idle("Memory"),
    }
}

fn unknown_rings() -> UsagePanel {
    let unknown = |label| UsageRing {
        label,
        spec: None,
        center: UNKNOWN_TEXT.to_owned(),
        lines: Vec::new(),
    };
    UsagePanel::Rings {
        cpu: unknown("CPU"),
        memory: unknown("Memory"),
    }
}

/// `50% of 250m request`, or what stands in for it.
fn against(used: u64, amount: Option<u64>, text: &dyn Fn(u64) -> String, what: &str) -> String {
    match (share(used, amount), amount) {
        (Share::Percent(percent), Some(amount)) => format!("{percent}% of {} {what}", text(amount)),
        (Share::ZeroBudget, _) => format!("{what} is zero"),
        _ => format!("no {what}"),
    }
}

/// One metric's use, and how to read and write its amounts.
struct Measure<'a> {
    label: &'static str,
    metric: ThresholdMetric,
    thresholds: &'a Thresholds,
    used: u64,
    text: &'a dyn Fn(u64) -> String,
}

impl Measure<'_> {
    /// Use that has nothing to be compared with: the amount only, saying why there is no ring.
    fn text_only(&self, against: &str) -> UsageRing {
        let used = (self.text)(self.used);
        UsageRing {
            label: self.label,
            spec: None,
            center: used.clone(),
            lines: vec![format!("{used} used, against {against} {UNKNOWN_TEXT}")],
        }
    }

    /// The ring of use against `request` and `end`, the limit of a pod or allocatable of a node
    /// (`end_word` names it).
    fn ring(&self, request: Option<u64>, end: Option<u64>, end_word: &str) -> UsageRing {
        let spec = RingSpec::of(self.used, request, end, self.metric, self.thresholds);
        let circle = end
            .filter(|&end| end > 0)
            .or(request.filter(|&request| request > 0));
        let center = match (spec, share(self.used, circle)) {
            (Some(_), Share::Percent(percent)) => format!("{percent}%"),
            _ => (self.text)(self.used),
        };
        let mut lines = vec![format!("{} used", (self.text)(self.used))];
        match request {
            // A node's tick is what its pods ask for, which it does not itself.
            Some(request) if end_word == "allocatable" => {
                lines.push(format!("{} requested by pods", (self.text)(request)));
            }
            _ if request.is_some() || end_word == "limit" => {
                lines.push(against(self.used, request, self.text, "request"));
            }
            _ => {}
        }
        lines.push(against(self.used, end, self.text, end_word));
        UsageRing {
            label: self.label,
            spec,
            center,
            lines,
        }
    }
}

fn cpu_text_of(nanos: u64) -> String {
    cpu_amount(nanos)
}

fn memory_text_of(bytes: u64) -> String {
    memory_amount(bytes)
}

fn measures<'a>(used: Usage, thresholds: &'a Thresholds) -> (Measure<'a>, Measure<'a>) {
    (
        Measure {
            label: "CPU",
            metric: ThresholdMetric::CpuPercent,
            thresholds,
            used: used.cpu.nanos(),
            text: &cpu_text_of,
        },
        Measure {
            label: "Memory",
            metric: ThresholdMetric::MemoryPercent,
            thresholds,
            used: used.memory.bytes(),
            text: &memory_text_of,
        },
    )
}

fn pod_rings(used: Usage, budget: Option<PodBudget>, thresholds: &Thresholds) -> UsagePanel {
    let (cpu, memory) = measures(used, thresholds);
    match budget {
        Some(budget) => UsagePanel::Rings {
            cpu: cpu.ring(
                budget.requests.cpu.map(Cpu::nanos),
                budget.limits.cpu.map(Cpu::nanos),
                "limit",
            ),
            memory: memory.ring(
                budget.requests.memory.map(Memory::bytes),
                budget.limits.memory.map(Memory::bytes),
                "limit",
            ),
        },
        // The sample covers other containers than the spec's: no percentage would be true.
        None => UsagePanel::Rings {
            cpu: cpu.text_only("requests and limits"),
            memory: memory.text_only("requests and limits"),
        },
    }
}

/// A workload's rings, over the totals of its pods: the same rules as a pod's, with its use,
/// requests and limits summed (a limit total exists only if every pod sets one).
fn workload_rings(totals: Totals, thresholds: &Thresholds) -> UsagePanel {
    let (cpu, memory) = measures(totals.usage(), thresholds);
    if !totals.budgeted {
        return UsagePanel::Rings {
            cpu: cpu.text_only("requests and limits"),
            memory: memory.text_only("requests and limits"),
        };
    }
    UsagePanel::Rings {
        cpu: cpu.ring(totals.cpu.request, totals.cpu.limit, "limit"),
        memory: memory.ring(totals.memory.request, totals.memory.limit, "limit"),
    }
}

/// A node's rings run to allocatable, with a tick at what its pods request.
fn node_rings(
    used: Usage,
    allocatable: Option<Amounts>,
    requests: Amounts,
    thresholds: &Thresholds,
) -> UsagePanel {
    let (cpu, memory) = measures(used, thresholds);
    // A ring needs positive allocatable to be the circle: without it, what pods request would
    // stand in for the capacity, and a node would look full that has no known size.
    let ring =
        |measure: &Measure, requests: Option<u64>, allocatable: Option<u64>| match allocatable
            .filter(|&amount| amount > 0)
        {
            Some(_) => measure.ring(requests, allocatable, "allocatable"),
            None => measure.text_only("allocatable"),
        };
    let allocatable = allocatable.unwrap_or_default();
    UsagePanel::Rings {
        cpu: ring(
            &cpu,
            requests.cpu.map(Cpu::nanos),
            allocatable.cpu.map(Cpu::nanos),
        ),
        memory: ring(
            &memory,
            requests.memory.map(Memory::bytes),
            allocatable.memory.map(Memory::bytes),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flint_core::metrics::{ContainerMetrics, PodMetrics};
    use serde_json::json;

    use super::super::super::list_usage::{NO_FEED_HINT, NOT_INSTALLED_HINT};
    use super::super::ring::Tone;
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn usage(millis: u64, mib: u64) -> Usage {
        Usage {
            cpu: Cpu::from_nanos(millis * 1_000_000),
            memory: Memory::from_bytes(mib * MIB),
        }
    }

    fn pod_source(containers: &[&str], used: Usage) -> UsageSource {
        let sample = PodMetrics {
            namespace: "ns".into(),
            name: "web".into(),
            timestamp: Some(jiff::Timestamp::from_second(1_000).unwrap()),
            window: Some(jiff::SignedDuration::from_secs(15)),
            containers: containers
                .iter()
                .enumerate()
                .map(|(i, name)| ContainerMetrics {
                    name: (*name).to_owned(),
                    usage: if i == 0 { used } else { Usage::default() },
                })
                .collect(),
        }
        .sample();
        let key = ("ns".to_owned(), "web".to_owned());
        UsageSource::Pods(Arc::new([(key, sample)].into_iter().collect()), None)
    }

    fn pod_object() -> Value {
        json!({
            "metadata": {"creationTimestamp": "1970-01-01T00:01:40Z"},
            "spec": {"containers": [{
                "name": "app",
                "resources": {
                    "requests": {"cpu": "200m", "memory": "100Mi"},
                    "limits": {"memory": "400Mi"}
                }
            }]}
        })
    }

    fn pod_detail(source: UsageSource) -> DetailUsage {
        let mut detail = DetailUsage::default();
        detail.set_object(Some(UsageSubject::Pods), &pod_object());
        detail.set_source(source);
        detail
    }

    fn rings(panel: Option<UsagePanel>) -> (UsageRing, UsageRing) {
        match panel {
            Some(UsagePanel::Rings { cpu, memory }) => (cpu, memory),
            other => panic!("expected rings, got {other:?}"),
        }
    }

    fn unknown_ring(ring: &UsageRing) -> bool {
        ring.spec.is_none() && ring.center == UNKNOWN_TEXT && ring.lines.is_empty()
    }

    #[test]
    fn a_pod_shows_use_against_its_requests_and_limits() {
        let detail = pod_detail(pod_source(&["app"], usage(100, 200)));
        let (cpu, memory) = rings(detail.panel(Some("ns"), "web"));
        // No CPU limit: the request ends the circle.
        assert_eq!(cpu.center, "50%");
        assert_eq!(cpu.lines, ["100m used", "50% of 200m request", "no limit"]);
        let spec = cpu.spec.unwrap();
        assert_eq!((spec.fill, spec.tick, spec.tone), (0.5, None, Tone::Normal));
        // Memory limit 400Mi, request 100Mi: the circle is the limit, the tick the request.
        assert_eq!(memory.center, "50%");
        assert_eq!(
            memory.lines,
            [
                "200 MiB used",
                "200% of 100 MiB request",
                "50% of 400 MiB limit"
            ]
        );
        let spec = memory.spec.unwrap();
        assert_eq!(
            (spec.fill, spec.tick, spec.tone),
            (0.5, Some(0.25), Tone::Normal)
        );
    }

    #[test]
    fn use_past_the_request_with_no_limit_is_red() {
        let detail = pod_detail(pod_source(&["app"], usage(300, 200)));
        let (cpu, _) = rings(detail.panel(Some("ns"), "web"));
        assert_eq!(cpu.spec.unwrap().tone, Tone::Over);
        assert_eq!(cpu.center, "150%");
    }

    #[test]
    fn the_warning_colour_follows_the_settings_thresholds() {
        let mut detail = pod_detail(pod_source(&["app"], usage(100, 200)));
        let mut overrides = flint_core::thresholds::ThresholdOverrides::default();
        overrides
            .set(ThresholdMetric::CpuPercent, Some(40))
            .unwrap();
        assert!(detail.set_thresholds(Thresholds::default().merged(&overrides)));
        assert!(!detail.set_thresholds(Thresholds::default().merged(&overrides)));
        let (cpu, memory) = rings(detail.panel(Some("ns"), "web"));
        assert_eq!(cpu.spec.unwrap().tone, Tone::Warning);
        assert_eq!(memory.spec.unwrap().tone, Tone::Normal);
    }

    #[test]
    fn a_pod_with_neither_request_nor_limit_has_a_value_and_no_ring() {
        let mut detail = DetailUsage::default();
        detail.set_object(
            Some(UsageSubject::Pods),
            &json!({
                "metadata": {"creationTimestamp": "1970-01-01T00:01:40Z"},
                "spec": {"containers": [{"name": "app"}]}
            }),
        );
        detail.set_source(pod_source(&["app"], usage(100, 200)));
        let (cpu, memory) = rings(detail.panel(Some("ns"), "web"));
        assert_eq!(cpu.spec, None);
        assert_eq!(cpu.center, "100m");
        assert_eq!(cpu.lines, ["100m used", "no request", "no limit"]);
        assert_eq!(memory.spec, None);
    }

    #[test]
    fn a_sample_of_other_containers_gets_no_ring() {
        let detail = pod_detail(pod_source(&["migrate"], usage(100, 200)));
        let (cpu, memory) = rings(detail.panel(Some("ns"), "web"));
        assert_eq!(cpu.spec, None);
        assert_eq!(
            cpu.lines,
            ["100m used, against requests and limits unknown"]
        );
        assert_eq!(
            memory.lines,
            ["200 MiB used, against requests and limits unknown"]
        );
    }

    #[test]
    fn a_pod_the_poll_did_not_report_is_unknown_not_zero() {
        let detail = pod_detail(pod_source(&["app"], usage(1, 1)));
        let (cpu, memory) = rings(detail.panel(Some("ns"), "other"));
        assert!(unknown_ring(&cpu) && unknown_ring(&memory));
    }

    fn node_detail(source: UsageSource, object: &Value) -> DetailUsage {
        let mut detail = DetailUsage::default();
        detail.set_object(Some(UsageSubject::Nodes), object);
        detail.set_source(source);
        detail
    }

    #[test]
    fn a_node_shows_use_against_allocatable() {
        let source = UsageSource::Nodes(
            Arc::new([("n1".to_owned(), usage(1000, 512))].into_iter().collect()),
            None,
        );
        let detail = node_detail(
            source,
            &json!({"status": {"allocatable": {"cpu": "4", "memory": "1Gi"}}}),
        );
        let (cpu, memory) = rings(detail.panel(None, "n1"));
        assert_eq!(cpu.center, "25%");
        assert_eq!(
            cpu.lines,
            ["1.00 cores used", "25% of 4.00 cores allocatable"]
        );
        assert_eq!(memory.center, "50%");
        assert_eq!(memory.lines, ["512 MiB used", "50% of 1.0 GiB allocatable"]);
        let (cpu, memory) = rings(detail.panel(None, "n2"));
        assert!(unknown_ring(&cpu) && unknown_ring(&memory));
    }

    #[test]
    fn a_node_with_no_readable_allocatable_says_so() {
        let source = UsageSource::Nodes(
            Arc::new([("n1".to_owned(), usage(10, 20))].into_iter().collect()),
            None,
        );
        let detail = node_detail(source, &json!({"status": {}}));
        let (cpu, memory) = rings(detail.panel(None, "n1"));
        assert_eq!(cpu.spec, None);
        assert_eq!(cpu.lines, ["10m used, against allocatable unknown"]);
        assert_eq!(memory.lines, ["20 MiB used, against allocatable unknown"]);
    }

    #[test]
    fn with_no_metrics_server_the_pane_shows_the_hint() {
        let mut detail = DetailUsage::default();
        assert_eq!(detail.panel(Some("ns"), "web"), None);
        assert!(detail.set_source(UsageSource::NotInstalled));
        assert!(!detail.set_source(UsageSource::NotInstalled));
        assert_eq!(
            detail.panel(Some("ns"), "web"),
            Some(UsagePanel::Hint(NOT_INSTALLED_HINT))
        );
    }

    #[test]
    fn with_no_feed_the_pane_says_when_usage_appears() {
        let mut detail = DetailUsage::default();
        detail.set_source(UsageSource::NoFeed);
        assert_eq!(
            detail.panel(Some("ns"), "web"),
            Some(UsagePanel::Hint(NO_FEED_HINT))
        );
    }

    #[test]
    fn a_sample_from_before_the_pod_was_created_is_unknown() {
        let mut detail = pod_detail(pod_source(&["app"], usage(100, 200)));
        detail.set_object(
            Some(UsageSubject::Pods),
            &json!({"metadata": {"creationTimestamp": "1970-01-01T00:16:40Z"}}),
        );
        let (cpu, memory) = rings(detail.panel(Some("ns"), "web"));
        assert!(unknown_ring(&cpu) && unknown_ring(&memory));
    }

    #[test]
    fn a_deleted_pod_takes_no_later_readings() {
        let mut detail = pod_detail(UsageSource::Unknown);
        detail.mark_deleted();
        assert!(!detail.set_source(pod_source(&["app"], usage(100, 200))));
        let (cpu, memory) = rings(detail.panel(Some("ns"), "web"));
        assert!(unknown_ring(&cpu) && unknown_ring(&memory));
    }

    #[test]
    fn a_failed_poll_is_unknown() {
        let detail = pod_detail(UsageSource::Unknown);
        let (cpu, memory) = rings(detail.panel(Some("ns"), "web"));
        assert!(unknown_ring(&cpu) && unknown_ring(&memory));
    }

    #[test]
    fn a_node_ring_has_a_tick_at_what_its_pods_request() {
        use flint_core::telemetry::{Allocatable, ClusterCapacity, NodeCapacity, Reserved};
        let capacity = ClusterCapacity {
            nodes: Arc::new([NodeCapacity {
                name: "n1".into(),
                allocatable: Allocatable::default(),
                requests: Reserved {
                    cpu: Some(Cpu::from_nanos(2_000_000_000)),
                    memory: None,
                },
                limits: Reserved::default(),
                pods: None,
            }]),
        };
        let source = UsageSource::Nodes(
            Arc::new([("n1".to_owned(), usage(1000, 512))].into_iter().collect()),
            Some(capacity),
        );
        let detail = node_detail(
            source,
            &json!({"status": {"allocatable": {"cpu": "4", "memory": "1Gi"}}}),
        );
        let (cpu, memory) = rings(detail.panel(None, "n1"));
        let spec = cpu.spec.unwrap();
        assert_eq!((spec.fill, spec.tick), (0.25, Some(0.5)));
        assert_eq!(
            cpu.lines,
            [
                "1.00 cores used",
                "2.00 cores requested by pods",
                "25% of 4.00 cores allocatable"
            ]
        );
        // No memory request is known: no tick.
        assert_eq!(memory.spec.unwrap().tick, None);
    }

    fn workload_detail(pods: &[(&'static str, &str)], object_uid: &str) -> DetailUsage {
        use flint_core::telemetry::{Controllers, PodIndex, PodOwner, PodRecord};

        use super::super::super::list_usage_workloads::OwnerChain;
        let owner = PodOwner {
            group: "apps".into(),
            kind: "StatefulSet".into(),
            name: "db".into(),
            uid: "sts".into(),
        };
        let spec: PodSpec = serde_json::from_value(json!({"containers": [{
            "name": "app",
            "resources": {
                "requests": {"cpu": "100m", "memory": "50Mi"},
                "limits": {"cpu": "200m", "memory": "100Mi"}
            }
        }]}))
        .unwrap();
        let records: Vec<_> = pods
            .iter()
            .map(|&(uid, name)| {
                let record = PodRecord {
                    namespace: "ns".into(),
                    name: name.into(),
                    created: Some(985),
                    budget: PodBudgetCheck::of(&spec),
                    node: None,
                    owner: Some(owner.clone()),
                    finished: false,
                };
                (uid, record)
            })
            .collect();
        let samples = pods
            .iter()
            .map(|&(_, name)| {
                let sample = PodMetrics {
                    namespace: "ns".into(),
                    name: name.into(),
                    timestamp: Some(jiff::Timestamp::from_second(1_000).unwrap()),
                    window: Some(jiff::SignedDuration::from_secs(15)),
                    containers: vec![ContainerMetrics {
                        name: "app".into(),
                        usage: usage(10, 20),
                    }],
                }
                .sample();
                (("ns".to_owned(), name.to_owned()), sample)
            })
            .collect();
        let source = UsageSource::workloads(
            Arc::new(samples),
            PodIndex::for_tests(records, Controllers::new()),
            OwnerChain::Direct,
        );
        let mut detail = DetailUsage::default();
        detail.set_object(
            Some(UsageSubject::Workloads(OwnerChain::Direct)),
            &json!({"metadata": {"uid": object_uid}}),
        );
        detail.set_source(source);
        detail
    }

    #[test]
    fn a_node_with_no_allocatable_for_a_metric_gets_no_ring_for_it() {
        use flint_core::telemetry::{Allocatable, ClusterCapacity, NodeCapacity, Reserved};
        let capacity = ClusterCapacity {
            nodes: Arc::new([NodeCapacity {
                name: "n1".into(),
                allocatable: Allocatable::default(),
                requests: Reserved {
                    cpu: Some(Cpu::from_nanos(500_000_000)),
                    memory: None,
                },
                limits: Reserved::default(),
                pods: None,
            }]),
        };
        let source = UsageSource::Nodes(
            Arc::new([("n1".to_owned(), usage(1000, 512))].into_iter().collect()),
            Some(capacity),
        );
        // Memory is allocatable; CPU is missing, and the node's requests must not stand in.
        let detail = node_detail(
            source,
            &json!({"status": {"allocatable": {"memory": "1Gi"}}}),
        );
        let (cpu, memory) = rings(detail.panel(None, "n1"));
        assert_eq!(cpu.spec, None);
        assert_eq!(cpu.center, "1.00 cores");
        assert!(memory.spec.is_some());
    }

    #[test]
    fn a_workload_shows_rings_over_the_totals_of_its_pods() {
        let detail = workload_detail(&[("u1", "a"), ("u2", "b")], "sts");
        let (cpu, memory) = rings(detail.panel(Some("ns"), "db"));
        // 20m used of a 400m limit, with 200m requested.
        assert_eq!(cpu.center, "5%");
        let spec = cpu.spec.unwrap();
        assert_eq!(
            (spec.fill, spec.tick, spec.tone),
            (0.05, Some(0.5), Tone::Normal)
        );
        assert_eq!(
            cpu.lines,
            ["20m used", "10% of 200m request", "5% of 400m limit"]
        );
        // 40Mi used of a 200Mi limit.
        assert_eq!(memory.center, "20%");
    }

    #[test]
    fn a_workload_with_no_pod_reporting_says_so_and_is_not_unknown() {
        let detail = workload_detail(&[("u1", "a")], "another");
        let (cpu, _) = rings(detail.panel(Some("ns"), "other"));
        assert_eq!(cpu.spec, None);
        assert_eq!(cpu.center, "none");
        assert!(!unknown_ring(&cpu));
    }
}
