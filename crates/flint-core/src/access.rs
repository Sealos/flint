//! What the cluster's RBAC says about an action, for showing it: a `SelfSubjectAccessReview` for a
//! verb on a resource in a namespace, cached per cluster.
//!
//! An answer is display only. It never lets anything through: the write gate and the API server
//! still decide every request. [`Access::Unknown`] (the check failed, timed out, or could not be
//! asked) must leave an action enabled, so the server answers it.
//!
//! A review is a `POST`, which read-only clients never send (`read_only`), so it takes
//! the gated client and runs only where the gate allows changes. On a protected cluster every
//! answer is [`Access::Unknown`].
//!
//! Answers are kept per connection: a new credential starts with none. Traffic is bounded by the
//! cache, by one running check per key, and by a limit on the checks running at a time.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    SubjectAccessReviewStatus,
};
use kube::Api;
use kube::api::PostParams;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::time::Instant;

use crate::credential::{CredentialId, WriteCredential};
use crate::handle::CoreHandle;

/// How long an answer from the server is kept.
pub const ACCESS_FRESH_FOR: Duration = Duration::from_secs(60);

/// How long [`Access::Unknown`] is kept, so a failing check is not repeated at once.
const UNKNOWN_FOR: Duration = Duration::from_secs(10);

/// How long one check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// The most keys kept across clusters. Past it the cache starts again.
const MAX_ENTRIES: usize = 512;

/// The most checks running at a time.
const MAX_RUNNING: usize = 4;

/// One question for the API server's authorizer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccessCheck {
    /// `None` for a cluster-scoped resource.
    pub namespace: Option<String>,
    pub group: String,
    pub resource: String,
    pub subresource: Option<String>,
    /// The object asked about. A Role can limit a verb with `resourceNames`, so a request for a
    /// named object is asked by that name; `None` asks about the resource as a whole.
    pub name: Option<String>,
    pub verb: String,
}

/// What the authorizer answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Allowed,
    Forbidden,
    /// No usable answer: treat the action as enabled.
    Unknown,
}

/// A cluster's answers, for one connection's credential.
struct ClusterAnswers {
    credential: CredentialId,
    slots: HashMap<AccessCheck, Slot>,
}

/// One key's answer. Its lock is held while it is asked, so one check runs per key.
type Slot = Arc<AsyncMutex<Option<(Instant, Access)>>>;

/// The answers of every cluster.
pub(crate) struct AccessCache {
    clusters: Mutex<HashMap<String, ClusterAnswers>>,
    running: Semaphore,
}

impl Default for AccessCache {
    fn default() -> Self {
        AccessCache {
            clusters: Mutex::default(),
            running: Semaphore::new(MAX_RUNNING),
        }
    }
}

impl AccessCache {
    /// The slot of `check`. A credential other than the one the cluster's answers came from drops
    /// them first.
    fn slot(&self, cluster: &str, credential: &WriteCredential, check: &AccessCheck) -> Slot {
        let id = credential.id();
        let mut clusters = self.clusters.lock().unwrap_or_else(PoisonError::into_inner);
        if clusters
            .get(cluster)
            .is_some_and(|answers| !answers.credential.is(&id))
        {
            clusters.remove(cluster);
        }
        let known = clusters
            .get(cluster)
            .is_some_and(|answers| answers.slots.contains_key(check));
        if !known && clusters.values().map(|a| a.slots.len()).sum::<usize>() >= MAX_ENTRIES {
            clusters.clear();
        }
        clusters
            .entry(cluster.to_owned())
            .or_insert_with(|| ClusterAnswers {
                credential: id,
                slots: HashMap::new(),
            })
            .slots
            .entry(check.clone())
            .or_default()
            .clone()
    }

    /// The answer to `check`: a fresh one kept, or what `ask` says now.
    async fn answer(
        &self,
        cluster: &str,
        credential: &WriteCredential,
        check: &AccessCheck,
        ask: impl Future<Output = Access>,
    ) -> Access {
        let slot = self.slot(cluster, credential, check);
        let mut held = slot.lock().await;
        if let Some((at, access)) = *held
            && at.elapsed() < kept_for(access)
        {
            return access;
        }
        let access = match self.running.acquire().await {
            Ok(_permit) => tokio::time::timeout(CHECK_TIMEOUT, ask)
                .await
                .unwrap_or(Access::Unknown),
            Err(_) => Access::Unknown,
        };
        *held = Some((Instant::now(), access));
        access
    }
}

fn kept_for(access: Access) -> Duration {
    match access {
        Access::Unknown => UNKNOWN_FOR,
        Access::Allowed | Access::Forbidden => ACCESS_FRESH_FOR,
    }
}

/// What a review's status means. RBAC says no with `allowed: false` alone, so that is a refusal;
/// an authorizer that failed along the way may be why, so that is not known.
fn access_of(status: Option<&SubjectAccessReviewStatus>) -> Access {
    let Some(status) = status else {
        return Access::Unknown;
    };
    if status.allowed {
        return Access::Allowed;
    }
    let failed = status
        .evaluation_error
        .as_deref()
        .is_some_and(|e| !e.is_empty());
    if failed && status.denied != Some(true) {
        return Access::Unknown;
    }
    Access::Forbidden
}

fn review_of(check: &AccessCheck) -> SelfSubjectAccessReview {
    SelfSubjectAccessReview {
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                namespace: check.namespace.clone(),
                group: Some(check.group.clone()),
                resource: Some(check.resource.clone()),
                subresource: check.subresource.clone(),
                name: check.name.clone(),
                verb: Some(check.verb.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn ask(client: kube::Client, check: &AccessCheck) -> Access {
    let api: Api<SelfSubjectAccessReview> = Api::all(client);
    match api.create(&PostParams::default(), &review_of(check)).await {
        Ok(review) => access_of(review.status.as_ref()),
        Err(_) => Access::Unknown,
    }
}

impl CoreHandle {
    /// What RBAC says about `check` on `cluster` at `server`, for showing an action. Never
    /// permission to write. Anything but a clear answer is [`Access::Unknown`]. Never blocks;
    /// the future works on any executor, and dropping it stops the check.
    pub fn check_access(
        &self,
        cluster: &str,
        server: Option<String>,
        check: AccessCheck,
    ) -> impl Future<Output = Access> + Send + 'static {
        let client = self.bound_write_client(cluster, Some(server));
        let cache = self.access_cache();
        let cluster = cluster.to_owned();
        let run = self.on_runtime(async move {
            let Ok((client, _pin, credential)) = client.await else {
                return Access::Unknown;
            };
            // Its provider's lease ended: this connection authenticates nothing more.
            if credential.spent() {
                return Access::Unknown;
            }
            let asked = ask(client.into_client(), &check);
            cache.answer(&cluster, &credential, &check, asked).await
        });
        async move { run.await.unwrap_or(Access::Unknown) }
    }

    /// [`Self::check_access`] for each of `checks`, each with its answer.
    pub fn check_access_all(
        &self,
        cluster: &str,
        server: Option<String>,
        checks: Vec<AccessCheck>,
    ) -> impl Future<Output = Vec<(AccessCheck, Access)>> + Send + 'static {
        let asked: Vec<_> = checks
            .iter()
            .map(|check| self.check_access(cluster, server.clone(), check.clone()))
            .collect();
        async move {
            let answers = futures::future::join_all(asked).await;
            checks.into_iter().zip(answers).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::credential::CredentialSource;

    fn credential() -> WriteCredential {
        WriteCredential::new(Vec::new(), CredentialSource::Plugin, None, None)
    }

    fn check(verb: &str) -> AccessCheck {
        AccessCheck {
            namespace: Some("demo".into()),
            group: "apps".into(),
            resource: "deployments".into(),
            subresource: None,
            name: None,
            verb: verb.into(),
        }
    }

    async fn counted(
        cache: &AccessCache,
        cluster: &str,
        credential: &WriteCredential,
        check: &AccessCheck,
        asks: &AtomicUsize,
        answer: Access,
    ) -> Access {
        cache
            .answer(cluster, credential, check, async {
                asks.fetch_add(1, Ordering::SeqCst);
                answer
            })
            .await
    }

    #[test]
    fn a_status_means_what_the_authorizer_said() {
        let status = |allowed, denied, error: Option<&str>| SubjectAccessReviewStatus {
            allowed,
            denied,
            evaluation_error: error.map(str::to_owned),
            reason: None,
        };
        assert_eq!(access_of(None), Access::Unknown);
        assert_eq!(access_of(Some(&status(true, None, None))), Access::Allowed);
        // RBAC's refusal: no opinion from any authorizer.
        assert_eq!(
            access_of(Some(&status(false, None, None))),
            Access::Forbidden
        );
        assert_eq!(
            access_of(Some(&status(false, Some(true), Some("webhook failed")))),
            Access::Forbidden
        );
        assert_eq!(
            access_of(Some(&status(false, None, Some("webhook failed")))),
            Access::Unknown
        );
    }

    #[test]
    fn a_named_check_is_reviewed_by_its_name() {
        let named = AccessCheck {
            name: Some("web".into()),
            ..check("update")
        };
        let review = review_of(&named);
        let attributes = review.spec.resource_attributes.unwrap();
        assert_eq!(attributes.name.as_deref(), Some("web"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_has_its_own_answer() {
        let cache = AccessCache::default();
        let (id, asks) = (credential(), AtomicUsize::new(0));
        let named = AccessCheck {
            name: Some("web".into()),
            ..check("update")
        };
        counted(&cache, "a", &id, &check("update"), &asks, Access::Forbidden).await;
        let access = counted(&cache, "a", &id, &named, &asks, Access::Allowed).await;
        assert_eq!(access, Access::Allowed);
        assert_eq!(asks.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_is_kept_per_key_until_it_expires() {
        let cache = AccessCache::default();
        let (id, asks) = (credential(), AtomicUsize::new(0));
        let get = check("get");
        for _ in 0..3 {
            let access = counted(&cache, "a", &id, &get, &asks, Access::Forbidden).await;
            assert_eq!(access, Access::Forbidden);
        }
        assert_eq!(asks.load(Ordering::SeqCst), 1);

        counted(&cache, "a", &id, &check("delete"), &asks, Access::Allowed).await;
        counted(&cache, "b", &id, &get, &asks, Access::Allowed).await;
        assert_eq!(asks.load(Ordering::SeqCst), 3, "per verb and per cluster");

        tokio::time::advance(ACCESS_FRESH_FOR).await;
        counted(&cache, "a", &id, &get, &asks, Access::Allowed).await;
        assert_eq!(asks.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_answer_is_retried_sooner() {
        let cache = AccessCache::default();
        let (id, asks) = (credential(), AtomicUsize::new(0));
        let get = check("get");
        counted(&cache, "a", &id, &get, &asks, Access::Unknown).await;
        counted(&cache, "a", &id, &get, &asks, Access::Allowed).await;
        assert_eq!(asks.load(Ordering::SeqCst), 1);
        tokio::time::advance(UNKNOWN_FOR).await;
        let access = counted(&cache, "a", &id, &get, &asks, Access::Allowed).await;
        assert_eq!(access, Access::Allowed);
        assert_eq!(asks.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_credential_starts_with_no_answers() {
        let cache = AccessCache::default();
        let asks = AtomicUsize::new(0);
        let get = check("get");
        counted(&cache, "a", &credential(), &get, &asks, Access::Forbidden).await;
        let access = counted(&cache, "a", &credential(), &get, &asks, Access::Allowed).await;
        assert_eq!(access, Access::Allowed);
        assert_eq!(asks.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_that_hangs_is_unknown() {
        let cache = AccessCache::default();
        let access = cache
            .answer(
                "a",
                &credential(),
                &check("get"),
                std::future::pending::<Access>(),
            )
            .await;
        assert_eq!(access, Access::Unknown);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cache_stays_bounded() {
        let cache = AccessCache::default();
        let (id, asks) = (credential(), AtomicUsize::new(0));
        for i in 0..=MAX_ENTRIES {
            let key = check(&format!("verb-{i}"));
            counted(&cache, "a", &id, &key, &asks, Access::Allowed).await;
        }
        let kept: usize = cache
            .clusters
            .lock()
            .unwrap()
            .values()
            .map(|a| a.slots.len())
            .sum();
        assert!(kept <= MAX_ENTRIES, "{kept}");
    }
}
