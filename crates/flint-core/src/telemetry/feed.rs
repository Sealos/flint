//! One kind's list and watch. Typed, because the classifier needs the objects' status, which the
//! Table watches behind lists do not carry.
//!
//! A kind is current once its list has loaded and a watch has opened. An expired resource version
//! or a dropped stream is routine and keeps the kind current while it recovers. A refused list or
//! watch, or an event that cannot be read, is a failure, reported at once and retried with
//! backoff.

use std::hash::{BuildHasher as _, RandomState};
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use kube::Client;
use kube::api::{Api, ListParams, WatchEvent, WatchParams};
use tokio::sync::Semaphore;

use super::objects::{MAX_OBJECTS_PER_KIND, Tracked};
use super::{CheckFailure, FailureCause, KindCheck, Shared, TelemetryTiming};
use crate::api_error::{ApiError, ApiErrorKind};
use crate::cluster::{ClusterError, SafeErrorText, Target};

/// Objects per page of a list.
const PAGE_SIZE: u32 = 500;

/// How long a watch must have run for its end to count as routine when it delivered nothing.
const STABLE_WATCH: Duration = Duration::from_secs(5);

/// Added to the server-side watch timeout before a silent stream counts as dropped.
const WATCH_GRACE: Duration = Duration::from_secs(30);

/// A delay that doubles up to a maximum, with up to 25% jitter.
#[derive(Debug)]
struct Backoff {
    next: Duration,
    min: Duration,
    max: Duration,
}

impl Backoff {
    fn new(min: Duration, max: Duration) -> Self {
        Self {
            next: min,
            min,
            max: max.max(min),
        }
    }

    fn next(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(self.max);
        let fraction = (RandomState::new().hash_one(0u8) % 1024) as f64 / 1024.0 * 0.25;
        base + base.mul_f64(fraction)
    }

    fn reset(&mut self) {
        self.next = self.min;
    }
}

/// How an attempt to list a kind ended, when it did not load.
enum ListEnd {
    Expired,
    TooLarge,
    Failed(CheckFailure),
}

/// How one watch ended.
enum WatchEnd {
    /// It closed or dropped: open another from the same version.
    Ended {
        healthy: bool,
    },
    /// Its resource version is too old: list again.
    Expired,
    TooLarge,
    Failed(CheckFailure),
}

/// What a kind's task needs.
#[derive(Clone)]
pub(super) struct KindFeed {
    pub(super) client: Client,
    pub(super) shared: Arc<Shared>,
    /// Shared by the kinds of a cluster.
    pub(super) limiter: Arc<Semaphore>,
    pub(super) target: Target,
    pub(super) timing: TelemetryTiming,
}

impl KindFeed {
    /// Lists and watches `K` until the task is stopped.
    pub(super) async fn run<K: Tracked>(self) {
        let api = Api::<K>::all(self.client.clone());
        let max = self.timing.backoff_max;
        let mut backoff = Backoff::new(self.timing.backoff_min, max);
        loop {
            let mut version = match self.list(&api).await {
                Ok((items, version)) => {
                    K::store(&mut self.shared.stores()).replace(items);
                    self.shared.dirty.notify_one();
                    version
                }
                Err(ListEnd::Expired) => {
                    tokio::time::sleep(backoff.next()).await;
                    continue;
                }
                Err(ListEnd::TooLarge) => {
                    self.invalidate::<K>(KindCheck::TooLarge);
                    tokio::time::sleep(max).await;
                    continue;
                }
                Err(ListEnd::Failed(failure)) => {
                    self.invalidate::<K>(KindCheck::Failed(failure));
                    tokio::time::sleep(backoff.next()).await;
                    continue;
                }
            };
            loop {
                match self.watch(&api, &mut version).await {
                    WatchEnd::Ended { healthy: true } => backoff.reset(),
                    // Opens but delivers nothing: only a fresh list can show the kind is current.
                    WatchEnd::Ended { healthy: false } => {
                        tokio::time::sleep(backoff.next()).await;
                        break;
                    }
                    WatchEnd::Expired => {
                        tokio::time::sleep(backoff.next()).await;
                        break;
                    }
                    WatchEnd::TooLarge => {
                        self.invalidate::<K>(KindCheck::TooLarge);
                        tokio::time::sleep(max).await;
                        break;
                    }
                    WatchEnd::Failed(failure) => {
                        self.invalidate::<K>(KindCheck::Failed(failure));
                        tokio::time::sleep(backoff.next()).await;
                        break;
                    }
                }
            }
        }
    }

    fn invalidate<K: Tracked>(&self, check: KindCheck) {
        K::store(&mut self.shared.stores()).invalidate(check);
        self.shared.dirty.notify_one();
    }

    /// Every object of `K`, trimmed, and the version to watch from.
    async fn list<K: Tracked>(&self, api: &Api<K>) -> Result<(Vec<K>, String), ListEnd> {
        let _slot = self.limiter.acquire().await.map_err(|_| {
            ListEnd::Failed(CheckFailure {
                cause: FailureCause::Failed,
                detail: SafeErrorText::reduced("the feed is stopping"),
            })
        })?;
        let mut items = Vec::new();
        let mut params = ListParams::default().limit(PAGE_SIZE);
        loop {
            let page = tokio::time::timeout(self.timing.list_timeout, api.list(&params))
                .await
                .map_err(|_| ListEnd::Failed(timed_out()))?
                .map_err(|err| self.list_end(&err))?;
            for mut item in page.items {
                item.trim();
                items.push(item);
            }
            if items.len() > MAX_OBJECTS_PER_KIND {
                return Err(ListEnd::TooLarge);
            }
            match page.metadata.continue_.filter(|token| !token.is_empty()) {
                Some(token) => params = params.continue_token(&token),
                None => {
                    let version = page.metadata.resource_version.unwrap_or_default();
                    return Ok((items, version));
                }
            }
        }
    }

    fn list_end(&self, err: &kube::Error) -> ListEnd {
        if is_expired(err) {
            ListEnd::Expired
        } else {
            ListEnd::Failed(failure_of(err, &self.target))
        }
    }

    /// One watch from `version`, applying its events and moving `version` along.
    async fn watch<K: Tracked>(&self, api: &Api<K>, version: &mut String) -> WatchEnd {
        // Asks for bookmarks by default, which keep `version` fresh on a quiet kind.
        let params = WatchParams::default().timeout(self.timing.watch_timeout_secs);
        let stream = match api.watch(&params, version).await {
            Ok(stream) => stream,
            Err(err) if is_expired(&err) => return WatchEnd::Expired,
            Err(err) => return WatchEnd::Failed(failure_of(&err, &self.target)),
        };
        K::store(&mut self.shared.stores()).check = KindCheck::Current;
        self.shared.dirty.notify_one();

        let silence = Duration::from_secs(u64::from(self.timing.watch_timeout_secs)) + WATCH_GRACE;
        let opened = Instant::now();
        let mut progressed = false;
        let mut stream = pin!(stream);
        loop {
            let event = match tokio::time::timeout(silence, stream.next()).await {
                Ok(Some(Ok(event))) => event,
                Ok(Some(Err(err))) if is_expired(&err) => return WatchEnd::Expired,
                // Resuming would meet the same event, and skipping it would hide a change.
                Ok(Some(Err(err @ kube::Error::SerdeError(_)))) => {
                    return WatchEnd::Failed(failure_of(&err, &self.target));
                }
                // A dropped, closed or silent stream.
                Ok(Some(Err(_)) | None) | Err(_) => {
                    return WatchEnd::Ended {
                        healthy: progressed || opened.elapsed() >= STABLE_WATCH,
                    };
                }
            };
            progressed = true;
            match event {
                WatchEvent::Added(item) | WatchEvent::Modified(item) => {
                    advance(version, item.meta().resource_version.as_deref());
                    let mut item = item;
                    item.trim();
                    if !K::store(&mut self.shared.stores()).upsert(item) {
                        return WatchEnd::TooLarge;
                    }
                }
                WatchEvent::Deleted(item) => {
                    advance(version, item.meta().resource_version.as_deref());
                    K::store(&mut self.shared.stores()).remove(&item);
                }
                WatchEvent::Bookmark(bookmark) => {
                    *version = bookmark.metadata.resource_version;
                    continue;
                }
                WatchEvent::Error(status) if status.code == 410 => return WatchEnd::Expired,
                WatchEvent::Error(status) => {
                    let err = kube::Error::Api(status);
                    return WatchEnd::Failed(failure_of(&err, &self.target));
                }
            }
            self.shared.dirty.notify_one();
        }
    }
}

fn advance(version: &mut String, next: Option<&str>) {
    if let Some(next) = next {
        next.clone_into(version);
    }
}

fn is_expired(err: &kube::Error) -> bool {
    ApiError::from_kube(err).is_some_and(|api| api.kind() == ApiErrorKind::Gone)
}

fn timed_out() -> CheckFailure {
    CheckFailure {
        cause: FailureCause::Failed,
        detail: SafeErrorText::reduced("timed out reading the cluster"),
    }
}

fn failure_of(err: &kube::Error, target: &Target) -> CheckFailure {
    let cause = match ApiError::from_kube(err).map(|api| api.kind()) {
        Some(ApiErrorKind::Unauthorized) => FailureCause::LoginFailed,
        Some(ApiErrorKind::Forbidden) => FailureCause::Forbidden,
        Some(ApiErrorKind::NotFound) => FailureCause::NotServed,
        _ if matches!(err, kube::Error::Auth(_)) => FailureCause::LoginFailed,
        _ => FailureCause::Failed,
    };
    CheckFailure {
        cause,
        detail: ClusterError::kube_ref(err).for_target(target).summary(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(code: u16) -> kube::Error {
        kube::Error::Api(Box::new(kube::core::Status {
            code,
            ..Default::default()
        }))
    }

    #[test]
    fn backoff_doubles_to_its_maximum_with_bounded_jitter_and_resets() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(4));
        let mut backoff = Backoff::new(min, max);
        for base in [1.0, 2.0, 4.0, 4.0] {
            let delay = backoff.next().as_secs_f64();
            assert!((base..=base * 1.25).contains(&delay), "{delay} from {base}");
        }
        backoff.reset();
        assert!(backoff.next() <= min.mul_f64(1.25));
    }

    #[test]
    fn failures_keep_their_cause() {
        let target = Target::default();
        for (code, cause) in [
            (401, FailureCause::LoginFailed),
            (403, FailureCause::Forbidden),
            (404, FailureCause::NotServed),
            (500, FailureCause::Failed),
        ] {
            assert_eq!(failure_of(&api_error(code), &target).cause, cause);
        }
    }

    #[test]
    fn only_a_gone_status_is_expired() {
        assert!(is_expired(&api_error(410)));
        assert!(!is_expired(&api_error(404)));
    }
}
