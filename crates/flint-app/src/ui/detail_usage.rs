//! A pod's or node's current use against its requests, limits or allocatable, from the cluster
//! feed's metrics poll, as the rings of the Usage section ([`super::ring`]). The pane makes no
//! request of its own.

use std::collections::BTreeMap;

use flint_core::metrics::{Amounts, Cpu, Memory, PodBudget, PodBudgetCheck, Share, Usage, share};
use flint_core::thresholds::{ThresholdMetric, Thresholds};
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use serde_json::Value;

use super::super::list_usage::{UNKNOWN_TEXT, UsageSource, UsageSubject, cpu_text, memory_text};
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
}

impl DetailUsage {
    /// Takes the poll's newest state. `true` if it changes what the pane shows.
    pub(super) fn set_source(&mut self, source: UsageSource) -> bool {
        if self.deleted {
            return false;
        }
        let changed = self.source != source;
        self.source = source;
        changed
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
    }

    /// Reads the budget from the fetched object. One that cannot be read stays unknown.
    pub(super) fn set_object(&mut self, subject: Option<UsageSubject>, object: &Value) {
        self.created = object
            .pointer("/metadata/creationTimestamp")
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<jiff::Timestamp>().ok())
            .map(jiff::Timestamp::as_second);
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
            None => None,
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
            UsageSource::Nodes(usage, _) => Some(match usage.get(name) {
                Some(&used) => {
                    let allocatable = match self.budget {
                        Some(Budget::Node(amounts)) => Some(amounts),
                        _ => None,
                    };
                    node_rings(used, allocatable, &self.thresholds)
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
        if request.is_some() || end_word == "limit" {
            lines.push(against(self.used, request, self.text, "request"));
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
    cpu_text(Cpu::from_nanos(nanos))
}

fn memory_text_of(bytes: u64) -> String {
    memory_text(Memory::from_bytes(bytes))
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

/// A node's rings run to allocatable.
///
/// TODO(#801): the tick at the total requests of the node's pods needs the per-node requested
/// totals of #798; until they are read the ring has no tick.
fn node_rings(used: Usage, allocatable: Option<Amounts>, thresholds: &Thresholds) -> UsagePanel {
    let (cpu, memory) = measures(used, thresholds);
    match allocatable {
        Some(allocatable) => UsagePanel::Rings {
            cpu: cpu.ring(None, allocatable.cpu.map(Cpu::nanos), "allocatable"),
            memory: memory.ring(None, allocatable.memory.map(Memory::bytes), "allocatable"),
        },
        None => UsagePanel::Rings {
            cpu: cpu.text_only("allocatable"),
            memory: memory.text_only("allocatable"),
        },
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
            ["200Mi used", "200% of 100Mi request", "50% of 400Mi limit"]
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
            ["200Mi used, against requests and limits unknown"]
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
        assert_eq!(cpu.lines, ["1000m used", "25% of 4000m allocatable"]);
        assert_eq!(memory.center, "50%");
        assert_eq!(memory.lines, ["512Mi used", "50% of 1024Mi allocatable"]);
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
        assert_eq!(memory.lines, ["20Mi used, against allocatable unknown"]);
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
}
