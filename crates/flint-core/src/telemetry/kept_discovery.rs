//! Discovery for a feed on a kept-watching connection, which has no slot to hold it.
//!
//! It runs inside the feed, on the feed's client, so it ends with the feed. It reuses the token
//! that connection holds and never runs an exec plugin: while the token is due it finds nothing,
//! and every kind stays unchecked. Each round asks only whether the resources problems are read
//! from are served, and what it found is kept as a [`KeptCoverage`] of fixed size.
//!
//! A round sends at most five requests, one at a time, and every feed takes
//! its turns from one [`DiscoveryPace`]. Every request and every publication asks the connection's
//! grant first, so ending the grant stops discovery without waiting for the manager.

use std::sync::Arc;
use std::time::Duration;

use kube::core::discovery::v2::{
    ACCEPT_AGGREGATED_DISCOVERY_V2, APIGroupDiscovery, APIGroupDiscoveryList,
};
use ring::digest::{self, Digest};
use tokio::sync::{Mutex, watch};
use tokio::time::Instant;

use super::RequestGate;
use crate::auth_plugin::HeldTokenOnly;
use crate::cluster;
use crate::connection::policy::{CredentialGrant, CredentialLease};
use crate::problems::ProblemKind;
use crate::read::DISCOVERY_REQUEST_TIMEOUT;

const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);
const REVALIDATE_MIN: Duration = Duration::from_secs(30);
const REVALIDATE_MAX: Duration = Duration::from_secs(300);
/// How long a partial round's answer counts, from when the round started.
const COVERAGE_LIFETIME: Duration = Duration::from_secs(55);
/// The longest one round may take, waiting for its turns included.
const ROUND_TIMEOUT: Duration = Duration::from_secs(25);
/// The shortest time between two discovery requests of any feeds.
const REQUEST_GAP: Duration = Duration::from_millis(50);
/// Two aggregated requests, then one per group on a server without aggregated discovery.
#[cfg(test)]
const MAX_REQUESTS_PER_ROUND: usize = 2 + GROUPS.len();

const PODS: u8 = 1 << 0;
const NODES: u8 = 1 << 1;
const REPLICA_SETS: u8 = 1 << 2;
const DEPLOYMENTS: u8 = 1 << 3;
const STATEFUL_SETS: u8 = 1 << 4;
const DAEMON_SETS: u8 = 1 << 5;
const JOBS: u8 = 1 << 6;

/// A group the feed reads, at the version it reads, with the resources problems come from.
struct Group {
    name: &'static str,
    resources: &'static [(&'static str, u8)],
}

const GROUPS: [Group; 3] = [
    Group {
        name: "",
        resources: &[("pods", PODS), ("nodes", NODES)],
    },
    Group {
        name: "apps",
        resources: &[
            ("replicasets", REPLICA_SETS),
            ("deployments", DEPLOYMENTS),
            ("statefulsets", STATEFUL_SETS),
            ("daemonsets", DAEMON_SETS),
        ],
    },
    Group {
        name: "batch",
        resources: &[("jobs", JOBS)],
    },
];

/// The version of every group in [`GROUPS`] that the feed reads.
const VERSION: &str = "v1";

/// The resources `kind` is read from. A pod's owner is read through its ReplicaSet.
const fn needs(kind: ProblemKind) -> u8 {
    match kind {
        ProblemKind::CrashLoop
        | ProblemKind::OomKilled
        | ProblemKind::ImagePull
        | ProblemKind::PodPending => PODS | REPLICA_SETS,
        ProblemKind::NodeNotReady => NODES,
        ProblemKind::JobFailed => JOBS,
        ProblemKind::WorkloadShort => DEPLOYMENTS | STATEFUL_SETS | DAEMON_SETS,
    }
}

/// Which resources a kept connection last confirmed, and until when that counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeptCoverage {
    served: u8,
    until: Option<Instant>,
}

impl KeptCoverage {
    /// Vouches for nothing.
    pub(crate) const NONE: Self = Self {
        served: 0,
        until: None,
    };

    /// Whether discovery vouches for `kind` now.
    pub fn vouches_for(&self, kind: ProblemKind) -> bool {
        let needed = needs(kind);
        self.until.is_some_and(|until| Instant::now() < until) && self.served & needed == needed
    }

    /// The kinds of problem discovery cannot vouch for now.
    pub fn unchecked(&self) -> Vec<ProblemKind> {
        ProblemKind::ALL
            .into_iter()
            .filter(|kind| !self.vouches_for(*kind))
            .collect()
    }

    fn complete(served: u8) -> bool {
        ProblemKind::ALL
            .iter()
            .all(|kind| served & needs(*kind) == needs(*kind))
    }
}

/// Spaces the discovery requests of every kept feed.
#[derive(Debug)]
pub(crate) struct DiscoveryPace {
    next: Mutex<Option<Instant>>,
    gap: Duration,
}

impl Default for DiscoveryPace {
    fn default() -> Self {
        Self {
            next: Mutex::new(None),
            gap: REQUEST_GAP,
        }
    }
}

impl DiscoveryPace {
    /// Waits for the next free turn.
    async fn turn(&self) {
        let at = {
            let mut next = self.next.lock().await;
            let now = Instant::now();
            let at = next.map_or(now, |next| next.max(now));
            *next = Some(at + self.gap);
            at
        };
        tokio::time::sleep_until(at).await;
    }
}

/// A fingerprint and resource bits for each endpoint, kept only for the life of the feed.
#[derive(Default)]
struct DiscoveryCache {
    core: CachedDiscovery,
    apis: CachedDiscovery,
}

#[derive(Default)]
struct CachedDiscovery(Option<(Digest, Option<u8>)>);

enum AggregatedError {
    Unsupported,
    Unavailable,
}

impl CachedDiscovery {
    /// Returns the served bits (or no aggregated core groups) and whether the body was unchanged.
    fn read(
        &mut self,
        body: &str,
        decode: impl FnOnce(&str) -> Result<Option<u8>, serde_json::Error>,
    ) -> Result<(Option<u8>, bool), serde_json::Error> {
        let fingerprint = digest::digest(&digest::SHA256, body.as_bytes());
        if let Some((last, served)) = &self.0
            && last.as_ref() == fingerprint.as_ref()
        {
            return Ok((*served, true));
        }
        let served = decode(body)?;
        self.0 = Some((fingerprint, served));
        Ok((served, false))
    }
}

/// What a kept feed needs to run discovery.
pub(crate) struct KeptDiscovery {
    pub(crate) client: kube::Client,
    pub(crate) requests: RequestGate,
    /// The kept connection's grant, which every request and publication asks.
    pub(crate) grant: CredentialGrant,
    /// Shared by every kept feed, so a launch with many watched clusters is no burst.
    pub(crate) pace: Arc<DiscoveryPace>,
}

impl KeptDiscovery {
    /// Keeps `found` current until the feed ends. What could not be read vouches for nothing.
    pub(super) async fn follow(
        self,
        found: watch::Sender<KeptCoverage>,
        version: watch::Sender<u64>,
    ) {
        let publish = |coverage: KeptCoverage| {
            let before = found.send_replace(coverage);
            if before.served != coverage.served
                || before.until.is_some() != coverage.until.is_some()
            {
                version.send_modify(|count| *count += 1);
            }
        };
        let mut wait = RETRY_MIN;
        let mut revalidate: Option<Duration> = None;
        let mut cache = DiscoveryCache::default();
        loop {
            let lease = self.grant.lease();
            let started = Instant::now();
            let held = HeldTokenOnly::default();
            let client = granted(
                cluster::reusing_held_token(self.client.clone(), held.clone()),
                &lease,
            );
            let served = tokio::select! {
                biased;
                () = lease.revoked() => None,
                served = tokio::time::timeout(ROUND_TIMEOUT, self.served(&client, &mut cache)) => {
                    served.ok().flatten()
                }
            };
            let served = served.filter(|_| lease.holds());
            let expires = held.expires_at();
            let finished = Instant::now();
            let (pause, until) = match served {
                Some((bits, unchanged)) if KeptCoverage::complete(bits) => {
                    wait = RETRY_MIN;
                    let pause = revalidate
                        .filter(|_| unchanged)
                        .map_or(REVALIDATE_MIN, |last| (last * 2).min(REVALIDATE_MAX));
                    // Keep the original cadence as a held token approaches expiry.
                    let pause = expires.map_or(pause, |at| {
                        let remaining = at.saturating_duration_since(finished + COVERAGE_LIFETIME);
                        pause.min(remaining.max(REVALIDATE_MIN))
                    });
                    revalidate = Some(pause);
                    (pause, finished + pause + ROUND_TIMEOUT)
                }
                _ => {
                    revalidate = None;
                    let pause = wait;
                    wait = wait.saturating_mul(2).min(RETRY_MAX);
                    (pause, started + COVERAGE_LIFETIME)
                }
            };
            publish(
                served.map_or(KeptCoverage::NONE, |(served, _)| KeptCoverage {
                    served,
                    until: Some(expires.map_or(until, |at| until.min(at))),
                }),
            );
            if tokio::time::timeout(pause, lease.revoked()).await.is_ok() {
                publish(KeptCoverage::NONE);
                revalidate = None;
                cache = DiscoveryCache::default();
                tokio::time::sleep(wait).await;
            }
        }
    }

    /// One request, on its turn and within its limit. `None` if it took too long.
    async fn ask<T>(
        &self,
        request: impl Future<Output = Result<T, kube::Error>>,
    ) -> Option<Result<T, kube::Error>> {
        self.pace.turn().await;
        self.requests
            .run(tokio::time::timeout(DISCOVERY_REQUEST_TIMEOUT, request))
            .await
            .ok()
    }

    /// The resources served now and whether both aggregated responses were unchanged.
    async fn served(
        &self,
        client: &kube::Client,
        cache: &mut DiscoveryCache,
    ) -> Option<(u8, bool)> {
        match self.served_aggregated(client, cache).await {
            Ok(coverage) => Some(coverage),
            Err(AggregatedError::Unsupported) => {
                cache.apis = CachedDiscovery::default();
                Some((self.served_by_group(client).await, false))
            }
            Err(AggregatedError::Unavailable) => None,
        }
    }

    /// An empty core list or an unsupported response calls for per-group discovery.
    async fn served_aggregated(
        &self,
        client: &kube::Client,
        cache: &mut DiscoveryCache,
    ) -> Result<(u8, bool), AggregatedError> {
        let (core, core_unchanged) = self
            .aggregated(client, "/api", &mut cache.core, served_in_core)
            .await?;
        let (apis, apis_unchanged) = self
            .aggregated(client, "/apis", &mut cache.apis, |list| {
                Some(served_in_apis(list))
            })
            .await?;
        Ok((core | apis, core_unchanged && apis_unchanged))
    }

    async fn aggregated(
        &self,
        client: &kube::Client,
        path: &str,
        cache: &mut CachedDiscovery,
        project: impl FnOnce(&APIGroupDiscoveryList) -> Option<u8>,
    ) -> Result<(u8, bool), AggregatedError> {
        let request = http::Request::get(path)
            .header(http::header::ACCEPT, ACCEPT_AGGREGATED_DISCOVERY_V2)
            .body(Vec::new())
            .expect("valid aggregated discovery request");
        let answer = self
            .ask(async {
                let body = client.request_text(request).await?;
                cache
                    .read(&body, |body| {
                        serde_json::from_str(body).map(|list| project(&list))
                    })
                    .map_err(kube::Error::SerdeError)
            })
            .await;
        match answer {
            Some(Ok((Some(served), unchanged))) => Ok((served, unchanged)),
            Some(Ok((None, _))) => Err(AggregatedError::Unsupported),
            Some(Err(error)) if not_aggregated(&error) => Err(AggregatedError::Unsupported),
            _ => Err(AggregatedError::Unavailable),
        }
    }

    /// For a server without aggregated discovery: one request per group problems are read from,
    /// however many groups it has. A group that cannot be read vouches for none of its resources.
    async fn served_by_group(&self, client: &kube::Client) -> u8 {
        let mut served = 0;
        for group in &GROUPS {
            let answer = if group.name.is_empty() {
                self.ask(client.list_core_api_resources(VERSION)).await
            } else {
                let version = format!("{}/{VERSION}", group.name);
                self.ask(client.list_api_group_resources(&version)).await
            };
            if let Some(Ok(list)) = answer {
                for (name, bit) in group.resources {
                    if list.resources.iter().any(|found| found.name == *name) {
                        served |= bit;
                    }
                }
            }
        }
        served
    }
}

/// `client`, whose every request first asks `lease`.
#[expect(
    clippy::disallowed_methods,
    reason = "wraps a client build_client made, refusing requests once its grant has ended"
)]
fn granted(client: kube::Client, lease: &CredentialLease) -> kube::Client {
    let namespace = client.default_namespace().to_owned();
    let lease = lease.clone();
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let (client, lease) = (client.clone(), lease.clone());
        async move {
            if !lease.holds() {
                return Err(kube::Error::Service("the grant has ended".into()));
            }
            client.send(request).await
        }
    });
    kube::Client::new(service, namespace)
}

/// Whether a request failed because the server has no aggregated discovery.
fn not_aggregated(error: &kube::Error) -> bool {
    match error {
        kube::Error::Api(status) => matches!(status.code, 404 | 406),
        kube::Error::SerdeError(_) => true,
        _ => false,
    }
}

fn served_in_core(core: &APIGroupDiscoveryList) -> Option<u8> {
    core.items
        .first()
        .map(|listed| served_of(listed, &GROUPS[0]))
}

fn served_in_apis(apis: &APIGroupDiscoveryList) -> u8 {
    let mut served = 0;
    for group in &GROUPS[1..] {
        if let Some(listed) = apis.items.iter().find(|listed| {
            listed.metadata.as_ref().and_then(|m| m.name.as_deref()) == Some(group.name)
        }) {
            served |= served_of(listed, group);
        }
    }
    served
}

fn served_of(listed: &APIGroupDiscovery, group: &Group) -> u8 {
    let Some(version) = listed
        .versions
        .iter()
        .find(|version| version.version.as_deref() == Some(VERSION))
        .filter(|version| version.freshness.as_deref().is_none_or(|f| f == "Current"))
    else {
        return 0;
    };
    group
        .resources
        .iter()
        .filter(|(name, _)| {
            version
                .resources
                .iter()
                .any(|found| found.resource.as_deref() == Some(name))
        })
        .fold(0, |served, (_, bit)| served | bit)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use http::{Request, Response};
    use kube::client::Body;
    use serde_json::{Value, json};

    use super::*;
    use crate::connection::policy::{CredentialAccess, PluginRuns};
    use crate::credential::CredentialSource;

    const AGGREGATED: &str = "as=APIGroupDiscoveryList";

    fn group(name: &str, resources: &[&str]) -> Value {
        let resources: Vec<Value> = resources.iter().map(|r| json!({"resource": r})).collect();
        json!({"metadata": {"name": name},
               "versions": [{"version": "v1", "resources": resources, "freshness": "Current"}]})
    }

    fn core() -> Value {
        json!({"items": [group("", &["pods", "nodes"])]})
    }

    fn apis(batch: &[&str]) -> Value {
        json!({"items": [
            group("apps", &["replicasets", "deployments", "statefulsets", "daemonsets"]),
            group("batch", batch),
        ]})
    }

    /// A path whose answer waits, and what tells it to go on.
    type Held = (String, Arc<tokio::sync::Notify>);

    /// Answers by path, and records each request.
    #[derive(Clone, Default)]
    struct Fake {
        seen: Arc<StdMutex<Vec<(String, Instant)>>>,
        answers: Arc<StdMutex<std::collections::HashMap<String, (u16, Value)>>>,
        /// Holds answers to this path until told to go on.
        hold: Arc<StdMutex<Option<Held>>>,
    }

    impl Fake {
        fn aggregated(batch: &[&str]) -> Self {
            let fake = Self::default();
            fake.answer("/api", 200, core());
            fake.answer("/apis", 200, apis(batch));
            fake
        }

        fn answer(&self, path: &str, status: u16, body: Value) {
            self.answers
                .lock()
                .unwrap()
                .insert(path.to_owned(), (status, body));
        }

        fn paths(&self) -> Vec<String> {
            let seen = self.seen.lock().unwrap();
            seen.iter().map(|(path, _)| path.clone()).collect()
        }

        #[expect(
            clippy::disallowed_methods,
            reason = "a client on an in-memory service, which reaches no server"
        )]
        fn client(&self) -> kube::Client {
            let fake = self.clone();
            let service = tower::service_fn(move |request: Request<Body>| {
                let fake = fake.clone();
                async move {
                    let path = request.uri().path().to_owned();
                    let accept = request
                        .headers()
                        .get(http::header::ACCEPT)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    fake.seen
                        .lock()
                        .unwrap()
                        .push((path.clone(), Instant::now()));
                    let held = fake.hold.lock().unwrap().clone();
                    if let Some((_, go)) = held.filter(|(at, _)| *at == path) {
                        go.notified().await;
                    }
                    let found = fake.answers.lock().unwrap().get(&path).cloned();
                    let plain =
                        matches!(path.as_str(), "/api" | "/apis") && !accept.contains(AGGREGATED);
                    let (status, body) = found.filter(|_| !plain).unwrap_or_else(|| {
                        let gone = json!({"kind": "Status", "apiVersion": "v1", "code": 404,
                                          "status": "Failure", "message": "not found",
                                          "reason": "NotFound"});
                        (404, gone)
                    });
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(status)
                            .body(Body::from(body.to_string().into_bytes()))
                            .unwrap(),
                    )
                }
            });
            kube::Client::new(service, "default")
        }
    }

    fn access() -> CredentialAccess {
        CredentialAccess::consented(Arc::new(|| {}), None, PluginRuns::default())
    }

    struct Running {
        found: watch::Receiver<KeptCoverage>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn follow(fake: &Fake, access: &CredentialAccess, pace: &Arc<DiscoveryPace>) -> Running {
        let discovery = KeptDiscovery {
            client: fake.client(),
            requests: RequestGate::default(),
            grant: access.grant(CredentialSource::Plugin),
            pace: Arc::clone(pace),
        };
        let (sender, found) = watch::channel(KeptCoverage::NONE);
        let (version, _) = watch::channel(0);
        let task = tokio::spawn(discovery.follow(sender, version));
        Running { found, task }
    }

    async fn until(found: &watch::Receiver<KeptCoverage>, done: impl Fn(&KeptCoverage) -> bool) {
        for _ in 0..10_000 {
            if done(&found.borrow()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never reached: {:?}", *found.borrow());
    }

    #[tokio::test(start_paused = true)]
    async fn a_grant_ended_during_a_request_stops_the_next_and_publishes_nothing() {
        let fake = Fake::aggregated(&["jobs"]);
        let go = Arc::new(tokio::sync::Notify::new());
        *fake.hold.lock().unwrap() = Some(("/api".to_owned(), Arc::clone(&go)));
        let access = access();
        let running = follow(&fake, &access, &Arc::default());
        while fake.paths().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        access.revoke();
        go.notify_waiters();
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            fake.paths(),
            ["/api"],
            "nothing follows the end of the grant"
        );
        assert_eq!(
            running.found.borrow().unchecked().len(),
            ProblemKind::ALL.len()
        );
    }

    // An exec token with no expiry never reaches the held-token check: the grant answers for it.
    #[tokio::test(start_paused = true)]
    async fn ending_the_grant_withdraws_coverage_already_published() {
        let fake = Fake::aggregated(&["jobs"]);
        let access = access();
        let running = follow(&fake, &access, &Arc::default());
        until(&running.found, |c| c.unchecked().is_empty()).await;
        let sent = fake.paths().len();
        access.revoke();
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(
            running.found.borrow().unchecked().len(),
            ProblemKind::ALL.len()
        );
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(fake.paths().len(), sent, "no request after the grant ended");
    }

    #[tokio::test(start_paused = true)]
    async fn coverage_lapses_by_itself_and_is_revalidated() {
        let fake = Fake::aggregated(&["jobs"]);
        let access = access();
        let running = follow(&fake, &access, &Arc::default());
        until(&running.found, |c| c.unchecked().is_empty()).await;
        let first = *running.found.borrow();
        assert!(first.until.is_some());
        tokio::time::sleep(COVERAGE_LIFETIME + Duration::from_secs(1)).await;
        assert_eq!(first.unchecked().len(), ProblemKind::ALL.len(), "it lapsed");
        assert!(fake.paths().len() > 2, "it was read again");
        assert!(running.found.borrow().unchecked().is_empty());
    }

    #[test]
    fn unchanged_aggregated_body_is_not_decoded_again() {
        let mut cache = CachedDiscovery::default();
        let decode =
            |body: &str| serde_json::from_str(body).map(|list| Some(served_in_apis(&list)));
        let original = apis(&["jobs"]).to_string();
        let changed = apis(&["cronjobs"]).to_string();
        let expected = REPLICA_SETS | DEPLOYMENTS | STATEFUL_SETS | DAEMON_SETS;
        assert_eq!(
            cache.read(&original, decode).unwrap(),
            (Some(expected | JOBS), false)
        );
        assert_eq!(
            cache
                .read(&original, |_| panic!("unchanged body was decoded"))
                .unwrap(),
            (Some(expected | JOBS), true)
        );
        assert_eq!(
            cache.read(&changed, decode).unwrap(),
            (Some(expected), false)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_rounds_back_off_and_a_changed_body_resets_the_interval() {
        let fake = Fake::aggregated(&["jobs"]);
        let mut running = follow(&fake, &access(), &Arc::default());
        running.found.changed().await.unwrap();

        for (seconds, change) in [
            (30, false),
            (60, false),
            (120, false),
            (240, false),
            (300, false),
            (300, true),
            (30, false),
        ] {
            let found = &mut running.found;
            let early = Duration::from_secs(seconds - 1);
            assert!(tokio::time::timeout(early, found.changed()).await.is_err());
            assert!(
                found.borrow().unchecked().is_empty(),
                "coverage lapsed between rounds"
            );
            if change {
                // Change the document without changing consumer coverage.
                let mut changed = apis(&["jobs"]);
                changed["items"]
                    .as_array_mut()
                    .unwrap()
                    .push(group("metrics.k8s.io", &["pods"]));
                fake.answer("/apis", 200, changed);
            }
            tokio::time::timeout(Duration::from_secs(2), found.changed())
                .await
                .expect("round ran at the expected interval")
                .unwrap();
            assert!(found.borrow().unchecked().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_no_longer_held_withdraws_coverage_when_it_is_next_read() {
        let fake = Fake::aggregated(&["jobs"]);
        let access = access();
        let running = follow(&fake, &access, &Arc::default());
        until(&running.found, |c| c.unchecked().is_empty()).await;
        fake.answer("/api", 500, json!({}));
        until(&running.found, |c| {
            c.unchecked().len() == ProblemKind::ALL.len()
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_that_disappears_makes_only_its_kinds_unchecked() {
        let fake = Fake::aggregated(&["jobs"]);
        let access = access();
        let running = follow(&fake, &access, &Arc::default());
        until(&running.found, |c| c.unchecked().is_empty()).await;
        fake.answer("/apis", 200, apis(&["cronjobs"]));
        until(&running.found, |c| !c.vouches_for(ProblemKind::JobFailed)).await;
        assert_eq!(running.found.borrow().unchecked(), [ProblemKind::JobFailed]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_without_aggregated_discovery_gets_only_the_required_groups() {
        let fake = Fake::default();
        // A legacy server with many groups, none of which is ever asked for.
        let names: Vec<Value> = (0..100)
            .map(|at| json!({"name": format!("g{at}.example")}))
            .collect();
        fake.answer(
            "/apis",
            200,
            json!({"kind": "APIGroupList", "groups": names}),
        );
        fake.answer("/api/v1", 200, json!({"groupVersion": "v1", "resources": [
            {"name": "pods", "singularName": "pod", "namespaced": true, "kind": "Pod", "verbs": []},
            {"name": "nodes", "singularName": "node", "namespaced": false, "kind": "Node",
             "verbs": []},
        ]}));
        fake.answer("/apis/apps/v1", 500, json!({}));
        fake.answer("/apis/batch/v1", 200, json!({"groupVersion": "batch/v1", "resources": [
            {"name": "jobs", "singularName": "job", "namespaced": true, "kind": "Job", "verbs": []},
        ]}));
        let access = access();
        let running = follow(&fake, &access, &Arc::default());
        until(&running.found, |c| c.vouches_for(ProblemKind::JobFailed)).await;
        let round: Vec<String> = fake.paths().into_iter().take(4).collect();
        assert_eq!(
            round,
            ["/api", "/api/v1", "/apis/apps/v1", "/apis/batch/v1"]
        );
        assert!(round.len() <= MAX_REQUESTS_PER_ROUND);
        assert!(
            !running
                .found
                .borrow()
                .vouches_for(ProblemKind::WorkloadShort)
        );
        assert!(!running.found.borrow().vouches_for(ProblemKind::CrashLoop));
        assert!(
            running
                .found
                .borrow()
                .vouches_for(ProblemKind::NodeNotReady)
        );
        assert!(fake.paths().iter().all(|path| !path.contains("example")));
    }

    #[tokio::test(start_paused = true)]
    async fn requests_of_every_feed_are_spaced() {
        let pace = Arc::new(DiscoveryPace::default());
        let fakes: Vec<Fake> = (0..32).map(|_| Fake::aggregated(&["jobs"])).collect();
        let access = access();
        let running: Vec<Running> = fakes.iter().map(|f| follow(f, &access, &pace)).collect();
        for feed in &running {
            until(&feed.found, |c| c.unchecked().is_empty()).await;
        }
        let mut times: Vec<Instant> = fakes
            .iter()
            .flat_map(|fake| {
                let seen = fake.seen.lock().unwrap();
                seen.iter().map(|(_, at)| *at).collect::<Vec<_>>()
            })
            .collect();
        times.sort();
        assert!(times.len() >= 64);
        for pair in times.windows(2) {
            assert!(pair[1] - pair[0] >= REQUEST_GAP, "{:?}", pair[1] - pair[0]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_grant_ended_while_waiting_for_a_turn_sends_nothing() {
        let pace = Arc::new(DiscoveryPace {
            next: Mutex::new(Some(Instant::now() + Duration::from_secs(10))),
            gap: REQUEST_GAP,
        });
        let fake = Fake::aggregated(&["jobs"]);
        let access = access();
        let running = follow(&fake, &access, &pace);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(fake.paths().is_empty(), "it is still waiting for its turn");
        access.revoke();
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(fake.paths().is_empty(), "{:?}", fake.paths());
        assert_eq!(
            running.found.borrow().unchecked().len(),
            ProblemKind::ALL.len()
        );
    }

    fn list(body: Value) -> APIGroupDiscoveryList {
        serde_json::from_value(body).unwrap()
    }

    // However many groups a server reports, only those problems are read from count.
    #[test]
    fn only_the_groups_problems_are_read_from_count() {
        let all = PODS | NODES | REPLICA_SETS | DEPLOYMENTS | STATEFUL_SETS | DAEMON_SETS | JOBS;
        let core = served_in_core(&list(core())).unwrap();
        let mut reported = apis(&["jobs"]);
        let items = reported["items"].as_array_mut().unwrap();
        items.extend((0..10_000).map(|at| group(&format!("g{at}.example"), &["things"])));
        let mut metrics = group("metrics.k8s.io", &["pods"]);
        metrics["versions"][0]["freshness"] = "Stale".into();
        items.push(metrics);
        assert_eq!(core | served_in_apis(&list(reported.clone())), all);

        let batch = reported["items"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|group| group["metadata"]["name"] == "batch")
            .unwrap();
        batch["versions"][0]["freshness"] = "Stale".into();
        assert_eq!(core | served_in_apis(&list(reported)), all & !JOBS);

        let renamed = json!({"items": [group("APPS", &["deployments"])]});
        assert_eq!(served_in_apis(&list(renamed)) & DEPLOYMENTS, 0);
    }

    #[test]
    fn coverage_is_a_small_fixed_size_value() {
        assert!(size_of::<KeptCoverage>() <= 32);
        assert_eq!(MAX_REQUESTS_PER_ROUND, 5);
    }
}
