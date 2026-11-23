//! What a feed reads by polling rather than watching: Warning events, claims and pod usage.
//!
//! Each read reports its own state. A read that fails replaces what it held, so a list that can no
//! longer be read is never shown as if it were current.

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{Event, PersistentVolumeClaim};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Client;
use kube::api::{Api, ListParams};
use serde::de::DeserializeOwned;
use tokio::sync::{Semaphore, watch};

use super::feed::{PAGE_SIZE, failure_of, timed_out};
use super::objects::MAX_OBJECTS_PER_KIND;
use super::{CheckFailure, FailureCause, TelemetryTiming};
use crate::cluster::{SafeErrorText, Target};
use crate::metrics::{
    MetricsError, MetricsResult, NodeMetrics, PodMetrics, Usage, fetch_pod_metrics,
};

/// A list read apart from the watched kinds.
#[derive(Clone, Debug, PartialEq)]
pub enum SideRead<T> {
    /// No read has finished.
    Waiting,
    Current(Arc<[T]>),
    /// It holds more than [`MAX_OBJECTS_PER_KIND`] objects, so it is not counted in part.
    TooLarge,
    /// The last read failed.
    Failed(CheckFailure),
}

impl<T> Default for SideRead<T> {
    fn default() -> Self {
        Self::Waiting
    }
}

impl<T> SideRead<T> {
    /// The objects, only if the last read succeeded.
    pub fn current(&self) -> Option<&[T]> {
        match self {
            Self::Current(items) => Some(items),
            Self::Waiting | Self::TooLarge | Self::Failed(_) => None,
        }
    }
}

/// The newest usage by key, from `metrics.k8s.io`.
#[derive(Clone, Debug, PartialEq)]
pub enum UsageRead<K> {
    /// No poll has finished.
    Waiting,
    Current(Arc<BTreeMap<K, Usage>>),
    /// It holds more than [`MAX_OBJECTS_PER_KIND`] entries.
    TooLarge,
    /// The last poll failed. [`MetricsError::NotInstalled`] is the case for an install hint.
    Unavailable(MetricsError),
}

impl<K> Default for UsageRead<K> {
    fn default() -> Self {
        Self::Waiting
    }
}

impl<K: Ord> UsageRead<K> {
    /// The usage of `key`, if the last poll succeeded and reported it.
    pub fn get(&self, key: &K) -> Option<Usage> {
        match self {
            Self::Current(usage) => usage.get(key).copied(),
            Self::Waiting | Self::TooLarge | Self::Unavailable(_) => None,
        }
    }

    fn of<T>(result: &MetricsResult<T>, entry: impl Fn(&T) -> (K, Usage)) -> Self {
        match result {
            Ok(list) if list.items.len() > MAX_OBJECTS_PER_KIND => Self::TooLarge,
            Ok(list) => Self::Current(Arc::new(list.items.iter().map(entry).collect())),
            Err(error) => Self::Unavailable(error.clone()),
        }
    }
}

/// A pod's namespace and name.
pub type PodKey = (String, String);

/// Everything a feed reads besides the watched kinds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SideReads {
    /// Warning events, for [`DashboardInput`](crate::dashboard::DashboardInput).
    pub warning_events: SideRead<Event>,
    /// Claims of every phase, for [`DashboardInput`](crate::dashboard::DashboardInput).
    pub pending_claims: SideRead<PersistentVolumeClaim>,
    pub pod_usage: UsageRead<PodKey>,
    /// By node name.
    pub node_usage: UsageRead<String>,
}

impl SideReads {
    pub(super) fn node_usage_of(result: &MetricsResult<NodeMetrics>) -> UsageRead<String> {
        UsageRead::of(result, |node| (node.name.clone(), node.usage))
    }

    fn pod_usage_of(result: &MetricsResult<PodMetrics>) -> UsageRead<PodKey> {
        UsageRead::of(result, |pod| {
            ((pod.namespace.clone(), pod.name.clone()), pod.total())
        })
    }
}

fn trimmed_meta(meta: &mut ObjectMeta) -> ObjectMeta {
    ObjectMeta {
        name: meta.name.take(),
        namespace: meta.namespace.take(),
        uid: meta.uid.take(),
        creation_timestamp: meta.creation_timestamp.take(),
        deletion_timestamp: meta.deletion_timestamp.take(),
        ..ObjectMeta::default()
    }
}

fn trim_event(event: &mut Event) {
    event.metadata = trimmed_meta(&mut event.metadata);
}

fn trim_claim(claim: &mut PersistentVolumeClaim) {
    claim.metadata = trimmed_meta(&mut claim.metadata);
    claim.spec = None;
}

/// What the polls of one feed share.
#[derive(Clone)]
pub(super) struct SideFeed {
    pub(super) client: Client,
    pub(super) limiter: Arc<Semaphore>,
    pub(super) target: Target,
    pub(super) timing: TelemetryTiming,
    pub(super) reads: watch::Sender<SideReads>,
}

impl SideFeed {
    pub(super) async fn run_warning_events(self) {
        let api = Api::<Event>::all(self.client.clone());
        let params = ListParams::default()
            .fields("type=Warning")
            .limit(PAGE_SIZE);
        self.poll(api, params, trim_event, |reads| &mut reads.warning_events)
            .await;
    }

    pub(super) async fn run_pending_claims(self) {
        let api = Api::<PersistentVolumeClaim>::all(self.client.clone());
        let params = ListParams::default().limit(PAGE_SIZE);
        self.poll(api, params, trim_claim, |reads| &mut reads.pending_claims)
            .await;
    }

    pub(super) async fn run_pod_usage(self) {
        loop {
            let result = fetch_pod_metrics(&self.client, None).await;
            let next = SideReads::pod_usage_of(&result);
            publish(&self.reads, |reads| &mut reads.pod_usage, next);
            tokio::time::sleep(self.timing.metrics_every).await;
        }
    }

    async fn poll<K>(
        self,
        api: Api<K>,
        params: ListParams,
        trim: fn(&mut K),
        slot: fn(&mut SideReads) -> &mut SideRead<K>,
    ) where
        K: Clone + DeserializeOwned + std::fmt::Debug + PartialEq,
    {
        loop {
            let next = self.read(&api, params.clone(), trim).await;
            publish(&self.reads, slot, next);
            tokio::time::sleep(self.timing.lists_every).await;
        }
    }

    /// Every page of a list, trimmed. All or nothing: a list cut short is no answer.
    async fn read<K>(&self, api: &Api<K>, mut params: ListParams, trim: fn(&mut K)) -> SideRead<K>
    where
        K: Clone + DeserializeOwned + std::fmt::Debug,
    {
        let Ok(_slot) = self.limiter.acquire().await else {
            return SideRead::Failed(CheckFailure {
                cause: FailureCause::Failed,
                detail: SafeErrorText::reduced("the feed is stopping"),
            });
        };
        let mut items = Vec::new();
        loop {
            let page = match tokio::time::timeout(self.timing.list_timeout, api.list(&params)).await
            {
                Ok(Ok(page)) => page,
                Ok(Err(err)) => return SideRead::Failed(failure_of(&err, &self.target)),
                Err(_) => return SideRead::Failed(timed_out()),
            };
            for mut item in page.items {
                trim(&mut item);
                items.push(item);
            }
            if items.len() > MAX_OBJECTS_PER_KIND {
                return SideRead::TooLarge;
            }
            match page.metadata.continue_.filter(|token| !token.is_empty()) {
                Some(token) => params = params.continue_token(&token),
                None => return SideRead::Current(items.into()),
            }
        }
    }
}

/// Sets one read, waking listeners only if it changed.
fn publish<T: PartialEq>(
    reads: &watch::Sender<SideReads>,
    slot: fn(&mut SideReads) -> &mut T,
    next: T,
) {
    reads.send_if_modified(|current| {
        let held = slot(current);
        let changed = *held != next;
        *held = next;
        changed
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricsList;

    fn failure() -> CheckFailure {
        CheckFailure {
            cause: FailureCause::Forbidden,
            detail: SafeErrorText::reduced("forbidden"),
        }
    }

    #[test]
    fn only_a_successful_read_has_objects() {
        let current: SideRead<u8> = SideRead::Current(Arc::from([1, 2]));
        assert_eq!(current.current(), Some(&[1, 2][..]));
        assert_eq!(SideRead::<u8>::Waiting.current(), None);
        assert_eq!(SideRead::<u8>::TooLarge.current(), None);
        assert_eq!(SideRead::<u8>::Failed(failure()).current(), None);
    }

    #[test]
    fn a_failed_read_replaces_what_was_held() {
        let (reads, seen) = watch::channel(SideReads::default());
        let slot: fn(&mut SideReads) -> &mut SideRead<Event> = |r| &mut r.warning_events;
        publish(
            &reads,
            slot,
            SideRead::Current(Arc::from([Event::default()])),
        );
        assert!(seen.borrow().warning_events.current().is_some());
        publish(&reads, slot, SideRead::Failed(failure()));
        assert_eq!(seen.borrow().warning_events.current(), None);
    }

    #[test]
    fn usage_is_keyed_and_a_failed_poll_has_none() {
        let node = NodeMetrics {
            name: "a".into(),
            timestamp: None,
            window: None,
            usage: Usage::default(),
        };
        let listed = Ok(MetricsList {
            items: vec![node],
            skipped: 0,
        });
        let usage = SideReads::node_usage_of(&listed);
        assert!(usage.get(&"a".to_owned()).is_some());
        assert!(usage.get(&"b".to_owned()).is_none());
        let failed = SideReads::node_usage_of(&Err(MetricsError::NotInstalled));
        assert_eq!(failed, UsageRead::Unavailable(MetricsError::NotInstalled));
        assert!(failed.get(&"a".to_owned()).is_none());
    }

    #[test]
    fn trimming_keeps_what_the_dashboard_reads() {
        let mut claim = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("data".into()),
                annotations: Some(BTreeMap::from([("a".to_owned(), "b".to_owned())])),
                ..ObjectMeta::default()
            },
            spec: Some(Default::default()),
            status: Some(k8s_openapi::api::core::v1::PersistentVolumeClaimStatus {
                phase: Some("Pending".into()),
                ..Default::default()
            }),
        };
        trim_claim(&mut claim);
        assert_eq!(claim.metadata.name.as_deref(), Some("data"));
        assert!(claim.metadata.annotations.is_none() && claim.spec.is_none());
        assert_eq!(crate::dashboard::count_pending_claims([&claim]), 1);
    }
}
