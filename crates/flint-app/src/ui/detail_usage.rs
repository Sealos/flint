//! A pod's or node's current use against its requests, limits or allocatable, from the cluster
//! feed's metrics poll. The pane makes no request of its own.

use std::collections::BTreeMap;

use flint_core::metrics::{Amounts, Cpu, Memory, PodBudget, PodBudgetCheck, Share, Usage, share};
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use serde_json::Value;

use super::super::list_usage::{
    NOT_INSTALLED_HINT, UNKNOWN_TEXT, UsageSource, UsageSubject, cpu_text, memory_text,
};

/// What the object's own definition gives to compare use with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Budget {
    Pod(PodBudgetCheck),
    Node(Amounts),
}

/// What the Usage section shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UsagePanel {
    Hint(&'static str),
    Lines { cpu: String, memory: String },
}

/// The usage of the object a detail pane shows.
#[derive(Debug, Default)]
pub(super) struct DetailUsage {
    source: UsageSource,
    budget: Option<Budget>,
}

impl DetailUsage {
    /// Takes the poll's newest state. `true` if it changes what the pane shows.
    pub(super) fn set_source(&mut self, source: UsageSource) -> bool {
        let changed = self.source != source;
        self.source = source;
        changed
    }

    /// Reads the budget from the fetched object. One that cannot be read stays unknown.
    pub(super) fn set_object(&mut self, subject: Option<UsageSubject>, object: &Value) {
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
            UsageSource::NotInstalled => Some(UsagePanel::Hint(NOT_INSTALLED_HINT)),
            UsageSource::Unknown => Some(unknown_lines()),
            UsageSource::Nodes(usage) => Some(match usage.get(name) {
                Some(&used) => {
                    let allocatable = match self.budget {
                        Some(Budget::Node(amounts)) => Some(amounts),
                        _ => None,
                    };
                    node_lines(used, allocatable)
                }
                None => unknown_lines(),
            }),
            UsageSource::Pods(usage) => {
                let key = (namespace?.to_owned(), name.to_owned());
                Some(match usage.get(&key) {
                    Some(sample) => {
                        let budget = match &self.budget {
                            Some(Budget::Pod(check)) => check.for_sample(sample),
                            _ => None,
                        };
                        pod_lines(sample.usage, budget)
                    }
                    None => unknown_lines(),
                })
            }
        }
    }
}

fn unknown_lines() -> UsagePanel {
    UsagePanel::Lines {
        cpu: UNKNOWN_TEXT.to_owned(),
        memory: UNKNOWN_TEXT.to_owned(),
    }
}

/// `50% of 250m request`, or what stands in for it.
fn against(used: u64, amount: Option<u64>, text: Option<String>, what: &str) -> String {
    match (share(used, amount), text) {
        (Share::Percent(percent), Some(text)) => format!("{percent}% of {text} {what}"),
        (Share::ZeroBudget, _) => format!("{what} is zero"),
        _ => format!("no {what}"),
    }
}

fn cpu_against(used: Cpu, amount: Option<Cpu>, what: &str) -> String {
    against(
        used.nanos(),
        amount.map(Cpu::nanos),
        amount.map(cpu_text),
        what,
    )
}

fn memory_against(used: Memory, amount: Option<Memory>, what: &str) -> String {
    against(
        used.bytes(),
        amount.map(Memory::bytes),
        amount.map(memory_text),
        what,
    )
}

fn pod_lines(used: Usage, budget: Option<PodBudget>) -> UsagePanel {
    let (cpu, memory) = match budget {
        Some(budget) => (
            format!(
                "{} used, {}, {}",
                cpu_text(used.cpu),
                cpu_against(used.cpu, budget.requests.cpu, "request"),
                cpu_against(used.cpu, budget.limits.cpu, "limit"),
            ),
            format!(
                "{} used, {}, {}",
                memory_text(used.memory),
                memory_against(used.memory, budget.requests.memory, "request"),
                memory_against(used.memory, budget.limits.memory, "limit"),
            ),
        ),
        // The sample covers other containers than the spec's: no percentage would be true.
        None => (
            format!(
                "{} used, against requests and limits {UNKNOWN_TEXT}",
                cpu_text(used.cpu)
            ),
            format!(
                "{} used, against requests and limits {UNKNOWN_TEXT}",
                memory_text(used.memory)
            ),
        ),
    };
    UsagePanel::Lines { cpu, memory }
}

fn node_lines(used: Usage, allocatable: Option<Amounts>) -> UsagePanel {
    let line = |text: String, compared: Option<String>| match compared {
        Some(compared) => format!("{text} used, {compared}"),
        None => format!("{text} used, against allocatable {UNKNOWN_TEXT}"),
    };
    UsagePanel::Lines {
        cpu: line(
            cpu_text(used.cpu),
            allocatable.map(|a| cpu_against(used.cpu, a.cpu, "allocatable")),
        ),
        memory: line(
            memory_text(used.memory),
            allocatable.map(|a| memory_against(used.memory, a.memory, "allocatable")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flint_core::metrics::{ContainerMetrics, PodMetrics};
    use serde_json::json;

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
            timestamp: None,
            window: None,
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
        UsageSource::Pods(Arc::new([(key, sample)].into_iter().collect()))
    }

    fn pod_object() -> Value {
        json!({"spec": {"containers": [{
            "name": "app",
            "resources": {
                "requests": {"cpu": "200m", "memory": "100Mi"},
                "limits": {"memory": "400Mi"}
            }
        }]}})
    }

    fn pod_detail(source: UsageSource) -> DetailUsage {
        let mut detail = DetailUsage::default();
        detail.set_object(Some(UsageSubject::Pods), &pod_object());
        detail.set_source(source);
        detail
    }

    fn lines(cpu: &str, memory: &str) -> Option<UsagePanel> {
        Some(UsagePanel::Lines {
            cpu: cpu.to_owned(),
            memory: memory.to_owned(),
        })
    }

    #[test]
    fn a_pod_shows_use_against_its_requests_and_limits() {
        let detail = pod_detail(pod_source(&["app"], usage(100, 200)));
        assert_eq!(
            detail.panel(Some("ns"), "web"),
            lines(
                "100m used, 50% of 200m request, no limit",
                "200Mi used, 200% of 100Mi request, 50% of 400Mi limit",
            )
        );
    }

    #[test]
    fn a_sample_of_other_containers_gets_no_percentage() {
        let detail = pod_detail(pod_source(&["migrate"], usage(100, 200)));
        assert_eq!(
            detail.panel(Some("ns"), "web"),
            lines(
                "100m used, against requests and limits unknown",
                "200Mi used, against requests and limits unknown",
            )
        );
    }

    #[test]
    fn a_pod_the_poll_did_not_report_is_unknown_not_zero() {
        let detail = pod_detail(pod_source(&["app"], usage(1, 1)));
        assert_eq!(
            detail.panel(Some("ns"), "other"),
            lines(UNKNOWN_TEXT, UNKNOWN_TEXT)
        );
    }

    #[test]
    fn a_node_shows_use_against_allocatable() {
        let source = UsageSource::Nodes(Arc::new(
            [("n1".to_owned(), usage(1000, 512))].into_iter().collect(),
        ));
        let mut detail = DetailUsage::default();
        detail.set_object(
            Some(UsageSubject::Nodes),
            &json!({"status": {"allocatable": {"cpu": "4", "memory": "1Gi"}}}),
        );
        detail.set_source(source);
        assert_eq!(
            detail.panel(None, "n1"),
            lines(
                "1000m used, 25% of 4000m allocatable",
                "512Mi used, 50% of 1024Mi allocatable",
            )
        );
        assert_eq!(detail.panel(None, "n2"), lines(UNKNOWN_TEXT, UNKNOWN_TEXT));
    }

    #[test]
    fn a_node_with_no_readable_allocatable_says_so() {
        let source = UsageSource::Nodes(Arc::new(
            [("n1".to_owned(), usage(10, 20))].into_iter().collect(),
        ));
        let mut detail = DetailUsage::default();
        detail.set_object(Some(UsageSubject::Nodes), &json!({"status": {}}));
        detail.set_source(source);
        assert_eq!(
            detail.panel(None, "n1"),
            lines(
                "10m used, against allocatable unknown",
                "20Mi used, against allocatable unknown",
            )
        );
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
    fn a_failed_poll_is_unknown() {
        let detail = pod_detail(UsageSource::Unknown);
        assert_eq!(
            detail.panel(Some("ns"), "web"),
            lines(UNKNOWN_TEXT, UNKNOWN_TEXT)
        );
    }
}
