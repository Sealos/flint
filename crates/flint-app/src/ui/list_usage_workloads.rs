//! A workload's CPU and memory as totals over its current pods, for the workload lists.
//!
//! A pod is counted when it has not finished and the metrics poll reported it (a sample that
//! follows its creation). Each counted pod adds its use, its requests (the ones it sets) and its
//! limits. A limit total exists only if every counted pod has one, so one pod without a limit
//! leaves the workload's bar to scale to its request. A pod whose sample does not cover the
//! containers its budget was read for (an init container still running) has no budget to add, so
//! the workload shows its use alone.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use flint_core::metrics::{Cpu, Memory, PodSample, Usage};
use flint_core::telemetry::{PodIndex, PodKey, PodOwner, PodRecord};

/// Which of a pod's owners a list's rows are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum OwnerChain {
    /// The pod's own controller: a ReplicaSet, a Job, a StatefulSet or a DaemonSet.
    Direct,
    /// A Deployment, through the ReplicaSet that controls the pod.
    ViaReplicaSets,
    /// A CronJob, through the Job that controls the pod.
    ViaJobs,
}

impl OwnerChain {
    /// Whether `index` can follow this chain. If not, a workload with no counted pod may have
    /// pods that could not be placed, so nothing can be said about it.
    pub(in crate::ui) fn is_known(self, index: &PodIndex) -> bool {
        match self {
            Self::Direct => true,
            Self::ViaReplicaSets => index.knows_replica_set_owners(),
            Self::ViaJobs => index.knows_job_owners(),
        }
    }
}

/// What one amount adds up to over the counted pods.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::ui) struct Total {
    pub used: u64,
    /// The sum of the requests the pods set, if any set one.
    pub request: Option<u64>,
    /// The sum of the limits, if every counted pod sets one.
    pub limit: Option<u64>,
}

/// The totals of one workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) struct Totals {
    pub cpu: Total,
    pub memory: Total,
    /// How many pods were counted. Never zero.
    pub pods: usize,
    /// Every counted pod's sample covers the containers its budget was read for, so the requests
    /// and limits are those of the same pods the use is.
    pub budgeted: bool,
}

impl Totals {
    pub(in crate::ui) fn usage(&self) -> Usage {
        Usage {
            cpu: Cpu::from_nanos(self.cpu.used),
            memory: Memory::from_bytes(self.memory.used),
        }
    }
}

/// A total being added up.
#[derive(Debug, Clone, Copy)]
struct Sum {
    used: u64,
    request: Option<u64>,
    /// `Some` while every pod added so far sets a limit.
    limit: Option<u64>,
}

impl Sum {
    const START: Self = Self {
        used: 0,
        request: None,
        limit: Some(0),
    };

    fn add(&mut self, used: u64, request: Option<u64>, limit: Option<u64>) {
        self.used = self.used.saturating_add(used);
        self.request = match (self.request, request) {
            (sum, None) => sum,
            (None, request) => request,
            (Some(sum), Some(request)) => Some(sum.saturating_add(request)),
        };
        self.limit = self
            .limit
            .zip(limit)
            .map(|(sum, limit)| sum.saturating_add(limit));
    }

    /// Counted without a budget: only its use is known, and no limit or request total is.
    fn add_unbudgeted(&mut self, used: u64) {
        self.add(used, None, None);
    }
}

/// A workload's running sums.
#[derive(Debug, Clone, Copy)]
struct Running {
    cpu: Sum,
    memory: Sum,
    pods: usize,
    /// A counted pod had no budget, so no request is a whole.
    unbudgeted: bool,
}

/// The totals of every workload that has a counted pod, by the uid of the owner: the key of its
/// row in the list.
pub(in crate::ui) fn totals_by_owner(
    index: &PodIndex,
    samples: &BTreeMap<PodKey, PodSample>,
    chain: OwnerChain,
) -> HashMap<Arc<str>, Totals> {
    totals(index, samples, chain, None)
}

/// The totals of the one workload with this `uid`, `None` if it has no counted pod.
pub(in crate::ui) fn totals_of(
    index: &PodIndex,
    samples: &BTreeMap<PodKey, PodSample>,
    chain: OwnerChain,
    uid: &str,
) -> Option<Totals> {
    totals(index, samples, chain, Some(uid)).remove(uid)
}

fn totals(
    index: &PodIndex,
    samples: &BTreeMap<PodKey, PodSample>,
    chain: OwnerChain,
    only: Option<&str>,
) -> HashMap<Arc<str>, Totals> {
    let mut running: HashMap<Arc<str>, Running> = HashMap::new();
    let mut key = PodKey::default();
    for pod in index.iter() {
        if pod.finished {
            continue;
        }
        let Some(owner) =
            owner(index, pod, chain).filter(|o| only.is_none_or(|uid| *o.uid == *uid))
        else {
            continue;
        };
        key.0.clear();
        key.0.push_str(&pod.namespace);
        key.1.clear();
        key.1.push_str(&pod.name);
        let Some(sample) = samples
            .get(&key)
            .filter(|sample| sample.follows_creation(pod.created))
        else {
            continue;
        };
        let entry = running.entry(Arc::clone(&owner.uid)).or_insert(Running {
            cpu: Sum::START,
            memory: Sum::START,
            pods: 0,
            unbudgeted: false,
        });
        entry.pods += 1;
        let (cpu, memory) = (sample.usage.cpu.nanos(), sample.usage.memory.bytes());
        match pod.budget.for_sample(sample) {
            Some(budget) => {
                entry.cpu.add(
                    cpu,
                    budget.requests.cpu.map(Cpu::nanos),
                    budget.limits.cpu.map(Cpu::nanos),
                );
                entry.memory.add(
                    memory,
                    budget.requests.memory.map(Memory::bytes),
                    budget.limits.memory.map(Memory::bytes),
                );
            }
            None => {
                entry.unbudgeted = true;
                entry.cpu.add_unbudgeted(cpu);
                entry.memory.add_unbudgeted(memory);
            }
        }
    }
    running
        .into_iter()
        .map(|(uid, run)| {
            let total = |sum: Sum| Total {
                used: sum.used,
                // A request that leaves some pods out is not a total, so it is not shown.
                request: sum.request.filter(|_| !run.unbudgeted),
                limit: sum.limit.filter(|_| !run.unbudgeted),
            };
            (
                uid,
                Totals {
                    cpu: total(run.cpu),
                    memory: total(run.memory),
                    pods: run.pods,
                    budgeted: !run.unbudgeted,
                },
            )
        })
        .collect()
}

fn owner<'a>(index: &'a PodIndex, pod: &'a PodRecord, chain: OwnerChain) -> Option<&'a PodOwner> {
    match chain {
        OwnerChain::Direct => pod.owner.as_ref(),
        OwnerChain::ViaReplicaSets | OwnerChain::ViaJobs => index.workload_of(pod),
    }
}

#[cfg(test)]
mod tests {
    use flint_core::metrics::{ContainerMetrics, PodBudgetCheck, PodMetrics};
    use flint_core::telemetry::Controllers;
    use serde_json::json;

    use super::*;

    const SAMPLED_AT: i64 = 1_000;
    const WINDOW: i64 = 15;
    const CREATED: i64 = SAMPLED_AT - WINDOW;

    fn owner(kind: &str, group: &str, name: &str, uid: &str) -> PodOwner {
        PodOwner {
            group: group.into(),
            kind: kind.into(),
            name: name.into(),
            uid: uid.into(),
        }
    }

    fn record(
        name: &str,
        owner: Option<PodOwner>,
        containers: serde_json::Value,
        finished: bool,
    ) -> PodRecord {
        let spec = serde_json::from_value(json!({ "containers": containers })).unwrap();
        PodRecord {
            namespace: "ns".into(),
            name: name.into(),
            created: Some(CREATED),
            budget: PodBudgetCheck::of(&spec),
            node: None,
            owner,
            finished,
        }
    }

    fn sample(container: &str, millis: u64, mib: u64) -> PodSample {
        PodMetrics {
            namespace: "ns".into(),
            name: "pod".into(),
            timestamp: Some(jiff::Timestamp::from_second(SAMPLED_AT).unwrap()),
            window: Some(jiff::SignedDuration::from_secs(WINDOW)),
            containers: vec![ContainerMetrics {
                name: container.into(),
                usage: Usage {
                    cpu: Cpu::from_nanos(millis * 1_000_000),
                    memory: Memory::from_bytes(mib * 1024 * 1024),
                },
            }],
        }
        .sample()
    }

    fn key(name: &str) -> PodKey {
        ("ns".to_owned(), name.to_owned())
    }

    fn app(requests: serde_json::Value, limits: serde_json::Value) -> serde_json::Value {
        json!([{"name": "app", "resources": {"requests": requests, "limits": limits}}])
    }

    /// Pods `(uid, name, owner)` all asking `requests` and `limits`, using 10m and 20Mi each.
    fn index(
        pods: Vec<(&'static str, &str, Option<PodOwner>, serde_json::Value)>,
        controllers: Controllers,
    ) -> (PodIndex, BTreeMap<PodKey, PodSample>) {
        let samples = pods
            .iter()
            .map(|(_, name, ..)| (key(name), sample("app", 10, 20)))
            .collect();
        let records = pods
            .into_iter()
            .map(|(uid, name, owner, containers)| (uid, record(name, owner, containers, false)))
            .collect::<Vec<_>>();
        (PodIndex::for_tests(records, controllers), samples)
    }

    fn limited() -> serde_json::Value {
        app(
            json!({"cpu": "100m", "memory": "50Mi"}),
            json!({"cpu": "200m", "memory": "100Mi"}),
        )
    }

    #[test]
    fn a_deployment_counts_the_pods_of_its_old_and_new_replica_sets() {
        let deployment = owner("Deployment", "apps", "web", "dep");
        let old = owner("ReplicaSet", "apps", "web-1", "rs-old");
        let new = owner("ReplicaSet", "apps", "web-2", "rs-new");
        let controllers: Controllers = [
            ("rs-old".into(), deployment.clone()),
            ("rs-new".into(), deployment),
        ]
        .into_iter()
        .collect();
        let (index, samples) = index(
            vec![
                ("u1", "a", Some(old.clone()), limited()),
                ("u2", "b", Some(new.clone()), limited()),
                ("u3", "c", Some(new), limited()),
            ],
            controllers,
        );

        let by_deployment = totals_by_owner(&index, &samples, OwnerChain::ViaReplicaSets);
        let total = by_deployment["dep"];
        assert_eq!(total.pods, 3);
        assert_eq!(total.cpu.used, 30_000_000);
        assert_eq!(total.cpu.request, Some(300_000_000));
        assert_eq!(total.cpu.limit, Some(600_000_000));
        assert_eq!(total.memory.limit, Some(300 * 1024 * 1024));
        assert_eq!(by_deployment.len(), 1);

        // A ReplicaSet's own list counts its pods directly.
        let by_set = totals_by_owner(&index, &samples, OwnerChain::Direct);
        assert_eq!(by_set["rs-old"].pods, 1);
        assert_eq!(by_set["rs-new"].pods, 2);
    }

    #[test]
    fn a_cron_job_counts_the_pods_of_its_jobs_and_a_stateful_set_its_own() {
        let cron = owner("CronJob", "batch", "nightly", "cron");
        let job = owner("Job", "batch", "nightly-1", "job");
        let sts = owner("StatefulSet", "apps", "db", "sts");
        let controllers: Controllers = [("job".into(), cron)].into_iter().collect();
        let (index, samples) = index(
            vec![
                ("u1", "a", Some(job), limited()),
                ("u2", "b", Some(sts), limited()),
            ],
            controllers,
        );
        let totals = totals_by_owner(&index, &samples, OwnerChain::ViaJobs);
        assert_eq!(totals["cron"].pods, 1);
        assert_eq!(totals["sts"].pods, 1);
        assert!(!totals.contains_key("job"));
        assert!(totals_by_owner(&index, &samples, OwnerChain::Direct).contains_key("job"));
    }

    #[test]
    fn one_pod_without_a_limit_leaves_the_workload_with_no_limit_total() {
        let sts = owner("StatefulSet", "apps", "db", "sts");
        let unlimited = app(json!({"cpu": "100m"}), json!({}));
        let (index, samples) = index(
            vec![
                ("u1", "a", Some(sts.clone()), limited()),
                ("u2", "b", Some(sts), unlimited),
            ],
            Controllers::new(),
        );
        let total = totals_by_owner(&index, &samples, OwnerChain::Direct)["sts"];
        assert_eq!(total.cpu.limit, None);
        assert_eq!(total.memory.limit, None);
        // The requests the pods set still add up: the second sets no memory request.
        assert_eq!(total.cpu.request, Some(200_000_000));
        assert_eq!(total.memory.request, Some(50 * 1024 * 1024));
    }

    /// A sample of two containers, `app` and `sidecar`, using nothing.
    fn two_container_sample() -> PodSample {
        PodMetrics {
            namespace: "ns".into(),
            name: "a".into(),
            timestamp: Some(jiff::Timestamp::from_second(SAMPLED_AT).unwrap()),
            window: Some(jiff::SignedDuration::from_secs(WINDOW)),
            containers: ["app", "sidecar"]
                .map(|name| ContainerMetrics {
                    name: name.into(),
                    usage: Usage::default(),
                })
                .to_vec(),
        }
        .sample()
    }

    #[test]
    fn a_container_without_a_limit_leaves_its_pod_without_one() {
        let sts = owner("StatefulSet", "apps", "db", "sts");
        let two = json!([
            {"name": "app", "resources": {"limits": {"cpu": "200m"}}},
            {"name": "sidecar"}
        ]);
        let record = record("a", Some(sts), two, false);
        let index = PodIndex::for_tests([("u1", record)], Controllers::new());
        let samples = [(key("a"), two_container_sample())].into_iter().collect();
        let total = totals_by_owner(&index, &samples, OwnerChain::Direct)["sts"];
        assert!(total.budgeted);
        assert_eq!(total.cpu.limit, None);
    }

    #[test]
    fn finished_pods_and_pods_without_a_reading_are_not_counted() {
        let sts = owner("StatefulSet", "apps", "db", "sts");
        let job = owner("Job", "batch", "j", "job");
        let records = vec![
            ("u1", record("a", Some(sts.clone()), limited(), false)),
            ("u2", record("b", Some(sts.clone()), limited(), true)),
            ("u3", record("c", Some(sts), limited(), false)),
            ("u4", record("d", Some(job), limited(), false)),
        ];
        let index = PodIndex::for_tests(records, Controllers::new());
        // `c` has no sample, and `d`'s window opened before the pod existed.
        let before_creation = PodMetrics {
            namespace: "ns".into(),
            name: "d".into(),
            timestamp: Some(jiff::Timestamp::from_second(SAMPLED_AT).unwrap()),
            window: Some(jiff::SignedDuration::from_secs(WINDOW * 100)),
            containers: vec![ContainerMetrics {
                name: "app".into(),
                usage: Usage::default(),
            }],
        }
        .sample();
        let samples = [
            (key("a"), sample("app", 10, 20)),
            (key("b"), sample("app", 10, 20)),
            (key("d"), before_creation),
        ]
        .into_iter()
        .collect();
        let totals = totals_by_owner(&index, &samples, OwnerChain::Direct);
        assert_eq!(totals["sts"].pods, 1);
        assert_eq!(totals["sts"].cpu.used, 10_000_000);
        // A workload with no counted pod has no entry: its row shows no value.
        assert!(!totals.contains_key("job"));
    }

    #[test]
    fn a_sample_of_other_containers_adds_its_use_but_no_request_or_limit() {
        let sts = owner("StatefulSet", "apps", "db", "sts");
        let (index, mut samples) = index(
            vec![
                ("u1", "a", Some(sts.clone()), limited()),
                ("u2", "b", Some(sts), limited()),
            ],
            Controllers::new(),
        );
        samples.insert(key("b"), sample("migrate", 10, 20));
        let total = totals_by_owner(&index, &samples, OwnerChain::Direct)["sts"];
        assert!(!total.budgeted);
        assert_eq!((total.pods, total.cpu.used), (2, 20_000_000));
        assert_eq!((total.cpu.request, total.cpu.limit), (None, None));
    }

    #[test]
    fn a_chain_through_owners_the_index_cannot_read_is_not_known() {
        let unread = PodIndex::default();
        assert!(OwnerChain::Direct.is_known(&unread));
        assert!(!OwnerChain::ViaReplicaSets.is_known(&unread));
        assert!(!OwnerChain::ViaJobs.is_known(&unread));
        let read = PodIndex::for_tests([], Controllers::new());
        assert!(OwnerChain::ViaReplicaSets.is_known(&read));
        assert!(OwnerChain::ViaJobs.is_known(&read));
    }

    #[test]
    fn one_workload_alone_is_the_same_as_its_entry_among_all() {
        let sts = owner("StatefulSet", "apps", "db", "sts");
        let other = owner("StatefulSet", "apps", "cache", "other");
        let (index, samples) = index(
            vec![
                ("u1", "a", Some(sts.clone()), limited()),
                ("u2", "b", Some(sts), limited()),
                ("u3", "c", Some(other), limited()),
            ],
            Controllers::new(),
        );
        let all = totals_by_owner(&index, &samples, OwnerChain::Direct);
        assert_eq!(
            totals_of(&index, &samples, OwnerChain::Direct, "sts"),
            Some(all["sts"])
        );
        assert_eq!(all["sts"].pods, 2);
        assert_eq!(
            totals_of(&index, &samples, OwnerChain::Direct, "none"),
            None
        );
    }

    #[test]
    fn pods_nothing_controls_belong_to_no_workload() {
        let (index, samples) = index(vec![("u1", "a", None, limited())], Controllers::new());
        assert!(totals_by_owner(&index, &samples, OwnerChain::ViaReplicaSets).is_empty());
    }
}
