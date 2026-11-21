//! The list + watch loop for one kind (see `docs/spikes/table-watch.md`).
//!
//! 1. Initial sync: a streaming list (`sendInitialEvents`) when the server supports it, else a
//!    paginated Table list. Rows are collected aside and swapped in when complete.
//! 2. Watch from the sync's resource version, with bookmarks. A stream that ends normally is
//!    resumed from the last resource version; a 410 (which arrives as an `ERROR` event inside an
//!    HTTP 200) relists; other errors back off and resume.
//! 3. APIs that cannot watch (405, metrics.k8s.io) are listed again on an interval instead. A
//!    watch the user may not open (403) is polled the same way, and the watch is tried again
//!    after successful polls, less and less often.
//! 4. A 403 or 404 is retried a few times with a short backoff before it is believed: an API
//!    server that just restarted can refuse requests while its authorizer warms up, and a
//!    recreated CRD is briefly not served.
//! 5. When the user may not list a namespaced kind in all namespaces, it is listed namespace by
//!    namespace instead ([`Engine::run_namespaced`]): in the namespaces the user filters on,
//!    else the context's default namespace, else every namespace the user can list. Lens calls
//!    these "accessible namespaces".
//!
//! Changes are published as snapshots at most every `publish_interval`, and only while someone
//! listens.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use tokio::sync::{Notify, Semaphore, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

use crate::cluster::ClusterId;
use crate::read::{KindId, KindInfo};
use crate::redact;
use crate::store::{Feed, KindSnapshot, NamespaceLimit, Store, SyncState};
use crate::table::{Column, EventType, FetchError, ListFormat, unix_now};

use super::hub::Hub;
use super::source::{EventStream, Source, WatchRequest};

#[derive(Debug, Clone)]
pub(crate) struct EngineConfig {
    pub page_size: u32,
    /// Server-side watch timeout; the stream then ends and is resumed.
    pub watch_timeout_secs: u32,
    /// Minimum time between two snapshots of one kind.
    pub publish_interval: Duration,
    pub poll_interval: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Retry interval when the server refuses a list (403, 404) for good.
    pub denied_retry: Duration,
    /// How many 403s or 404s in a row are retried with `denied_backoff_min` before they are
    /// believed.
    pub transient_denials: u32,
    pub denied_backoff_min: Duration,
    /// After that, the first wait before the next try; it doubles up to `denied_retry`.
    pub denied_wait_min: Duration,
    /// Silence on a watch after which the connection counts as dead.
    pub watch_stall: Duration,
}

impl EngineConfig {
    /// The defaults with the retry timings of `timing`.
    pub fn with_timing(timing: &crate::handle::WatchTiming) -> Self {
        EngineConfig {
            backoff_min: timing.backoff_min,
            backoff_max: timing.backoff_max,
            denied_backoff_min: timing.denied_backoff_min,
            ..EngineConfig::default()
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        let timing = crate::handle::WatchTiming::default();
        EngineConfig {
            page_size: 500,
            watch_timeout_secs: 290,
            publish_interval: Duration::from_millis(100),
            poll_interval: Duration::from_secs(15),
            backoff_min: timing.backoff_min,
            backoff_max: timing.backoff_max,
            denied_retry: Duration::from_secs(300),
            transient_denials: 3,
            denied_backoff_min: timing.denied_backoff_min,
            denied_wait_min: Duration::from_secs(5),
            watch_stall: Duration::from_secs(150),
        }
    }
}

struct Backoff {
    next: Duration,
    min: Duration,
    max: Duration,
}

impl Backoff {
    fn new(config: &EngineConfig) -> Self {
        Self::between(config.backoff_min, config.backoff_max)
    }

    fn between(min: Duration, max: Duration) -> Self {
        Backoff {
            next: min,
            min,
            max: max.max(min),
        }
    }

    /// The next delay, doubling up to the maximum, with up to 25% jitter so many kinds do not
    /// reconnect in lockstep after a network drop.
    fn next(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(self.max);
        jittered(base)
    }

    fn reset(&mut self) {
        self.next = self.min;
    }
}

/// `delay` plus up to 25% jitter, so the kinds of a cluster do not retry in lockstep after a
/// network drop or an API-server restart. Every retry and poll delay goes through this.
fn jittered(delay: Duration) -> Duration {
    delay + delay.mul_f64(jitter_fraction(jitter_entropy()))
}

/// Fresh bits for one jitter: the clock, hashed with a per-call random key.
fn jitter_entropy() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    keyed(nanos)
}

/// `value` hashed with a new random key, so two kinds that read the same clock value still get
/// different jitter.
fn keyed(value: u128) -> u64 {
    use std::hash::BuildHasher as _;
    std::collections::hash_map::RandomState::new().hash_one(value)
}

/// A jitter between 0 and 25%, spread evenly whichever bits of `entropy` vary. (A raw clock
/// reading is not enough: macOS clocks tick in microseconds, so the low digits of the
/// nanoseconds are always zero there.)
fn jitter_fraction(entropy: u64) -> f64 {
    // splitmix64's finalizer: every input bit affects every output bit.
    let mut z = entropy.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    // `% 1000` keeps the value exact as an f64.
    (z % 1000) as f64 / 4000.0
}

enum StreamEnd {
    /// The server ended the stream (its timeout). Resume from the last resource version.
    Closed {
        events: usize,
    },
    /// The streaming list delivered every initial object.
    InitialEnd,
    /// 410: relist.
    Expired,
    Failed(FetchError),
}

/// The namespaces a namespace-limited list may use, shared by every kind of a cluster and kept
/// current by the cluster manager.
pub(crate) type Scope = watch::Receiver<Arc<[String]>>;

pub(crate) struct Engine {
    cluster: ClusterId,
    id: KindId,
    kind: KindInfo,
    /// `Some` for one lane of a namespace-limited list: everything is listed in that namespace.
    namespace: Option<String>,
    /// Where to list when all namespaces are refused. `None` for lanes and cluster-scoped kinds.
    scope: Option<Scope>,
    source: Arc<dyn Source>,
    hub: Arc<Hub>,
    epoch: u64,
    config: Arc<EngineConfig>,
    /// Limits concurrent initial lists per cluster.
    limiter: Arc<Semaphore>,
    store: Store,
    state: SyncState,
    feed: Feed,
    synced_once: bool,
    dirty: bool,
    /// `state` is synced but the hub has not been told: the next snapshot says so with the rows.
    synced_held: bool,
    last_publish: Option<Instant>,
    /// 403s and 404s of lists in a row; reset by a successful list.
    denials: u32,
    /// 403s and 404s of watches in a row; reset when a watch opens.
    watch_denials: u32,
    /// The wait after a refusal that was believed; grows up to `denied_retry`.
    denied_wait: Backoff,
    /// How long until a refused watch is tried again, while polling instead.
    watch_retry: Backoff,
    /// For a lane of a namespace-limited list: the merged list's hub, whose listeners decide
    /// whether the lane builds snapshots and skips the list queue.
    parent: Option<Arc<Hub>>,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cluster: ClusterId,
        kind: KindInfo,
        source: Arc<dyn Source>,
        hub: Arc<Hub>,
        epoch: u64,
        config: Arc<EngineConfig>,
        limiter: Arc<Semaphore>,
    ) -> Self {
        Engine {
            cluster,
            id: kind.id(),
            store: Store::new(kind.namespaced),
            kind,
            namespace: None,
            scope: None,
            source,
            hub,
            epoch,
            watch_retry: Backoff::between(config.poll_interval, config.denied_retry),
            denied_wait: Backoff::between(config.denied_wait_min, config.denied_retry),
            watch_denials: 0,
            parent: None,
            config,
            limiter,
            state: SyncState::Loading,
            feed: Feed::Watch,
            synced_once: false,
            dirty: true,
            synced_held: false,
            last_publish: None,
            denials: 0,
        }
    }

    /// Lets a namespaced kind fall back to listing namespace by namespace in `scope`.
    pub fn with_scope(mut self, scope: Option<Scope>) -> Self {
        if self.kind.namespaced {
            self.scope = scope;
        }
        self
    }

    /// Lists and watches only `namespace` (one lane of a namespace-limited list merged into
    /// `parent`).
    fn in_namespace(mut self, namespace: String, parent: Arc<Hub>) -> Self {
        self.namespace = Some(namespace);
        self.scope = None;
        self.parent = Some(parent);
        self
    }

    /// Who reads this kind's rows: the subscribers of the hub, or of the merged list for a
    /// lane (whose own hub always has the merger listening).
    fn audience(&self) -> usize {
        self.parent.as_ref().unwrap_or(&self.hub).listeners()
    }

    pub async fn run(mut self, feed: Feed) {
        match feed {
            Feed::Watch => self.run_watch().await,
            Feed::Poll => {
                self.run_poll(false).await;
            }
        }
    }

    fn set_feed(&mut self, feed: Feed) {
        if self.feed != feed {
            self.feed = feed;
            self.hub.set_feed(self.epoch, feed);
            self.dirty = true;
        }
    }

    // --- publishing ----------------------------------------------------------------------

    fn set_state(&mut self, state: SyncState) {
        if state == SyncState::Synced {
            self.denials = 0;
            self.denied_wait.reset();
        }
        if self.state != state {
            // Status displays read this even while nobody subscribes (no snapshot is built).
            self.hub.set_state(self.epoch, &state);
            self.state = state;
            self.dirty = true;
            self.synced_held = false;
        }
    }

    /// Marks the rows as current. When that is news to a listener, the hub learns it from the
    /// next snapshot, at the usual deadline, so a status display never reports live ahead of
    /// the rows and a view gets no extra update.
    fn set_synced(&mut self) {
        if self.state != SyncState::Synced && self.audience() > 0 {
            self.state = SyncState::Synced;
            self.dirty = true;
            self.synced_held = true;
        }
        self.set_state(SyncState::Synced);
    }

    /// Tells the hub about a synced state held for a snapshot that will not be sent.
    fn report_held_synced(&mut self) {
        if std::mem::take(&mut self.synced_held) {
            self.hub.set_state(self.epoch, &self.state);
        }
    }

    /// Reports a relist of rows that were synced. Only that is errorless: a relist during an
    /// outage keeps the error on show. A synced state still held for its snapshot was never
    /// reported, so the trouble the hub last heard of still stands.
    fn begin_relist(&mut self) {
        if !self.synced_once || self.state != SyncState::Synced {
            return;
        }
        if self.synced_held {
            let reported = self.hub.state();
            if matches!(
                reported,
                SyncState::Reconnecting { error: Some(_) }
                    | SyncState::RetryingRefusal { .. }
                    | SyncState::Failed { .. }
            ) {
                self.state = reported;
                self.synced_held = false;
                return;
            }
        }
        self.set_state(SyncState::Reconnecting { error: None });
    }

    /// When the next snapshot is due: never while nobody listens or nothing changed. A held
    /// synced state stays due, so it is reported even if the last listener has left.
    fn deadline(&self) -> Option<Instant> {
        if !self.dirty || (self.audience() == 0 && !self.synced_held) {
            return None;
        }
        Some(match self.last_publish {
            Some(t) => t + self.config.publish_interval,
            None => Instant::now(),
        })
    }

    /// Answers one-off reads of the rows ([`Hub::peek`]) with a snapshot nobody keeps.
    fn answer_peeks(&self) {
        if !self.hub.has_peeks() {
            return;
        }
        self.hub.answer_peeks(self.epoch, || {
            self.store.snapshot(KindSnapshot {
                cluster: self.cluster.clone(),
                kind: self.id.clone(),
                state: self.state.clone(),
                feed: self.feed,
                ..Default::default()
            })
        });
    }

    /// Sends a snapshot if someone listens; otherwise only a held state is recorded.
    fn publish(&mut self) {
        if self.send_snapshot() {
            self.synced_held = false;
        } else {
            self.report_held_synced();
        }
    }

    /// Builds and sends a snapshot. Returns whether the hub took one.
    fn send_snapshot(&mut self) -> bool {
        if self.audience() == 0 {
            return false;
        }
        let base = KindSnapshot {
            cluster: self.cluster.clone(),
            kind: self.id.clone(),
            state: self.state.clone(),
            feed: self.feed,
            ..Default::default()
        };
        let snapshot = self.store.snapshot(base);
        let sent = self.hub.publish(self.epoch, snapshot);
        self.dirty = false;
        self.last_publish = Some(Instant::now());
        sent
    }

    /// Awaits `fut` while publishing due snapshots. The deadline is checked first, so a burst
    /// of ready watch events cannot starve publishing.
    async fn wait<F: Future>(&mut self, fut: F) -> F::Output {
        let hub = self.hub.clone();
        tokio::pin!(fut);
        loop {
            let deadline = self.deadline();
            tokio::select! {
                biased;
                _ = sleep_until_opt(deadline) => self.publish(),
                _ = hub.wake.notified() => {
                    self.dirty = true;
                    self.answer_peeks();
                }
                out = &mut fut => return out,
            }
        }
    }

    async fn pause(&mut self, delay: Duration) {
        self.wait(sleep(delay)).await;
    }

    /// Waits for one of the cluster's list slots. A kind someone has open skips the queue, so
    /// the visible list does not wait behind the All-mode bulk after activation.
    async fn list_slot(&mut self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let limiter = self.limiter.clone();
        let hub = self.hub.clone();
        let acquire = limiter.acquire_owned();
        tokio::pin!(acquire);
        loop {
            if self.audience() > 0 {
                return None;
            }
            // This wait runs no publish deadline, and nobody is left to send a snapshot to.
            self.report_held_synced();
            tokio::select! {
                permit = &mut acquire => return permit.ok(),
                // A subscriber arrived: check again, and publish what was held back.
                _ = hub.wake.notified() => {
                    self.dirty = true;
                    self.publish();
                    self.answer_peeks();
                }
            }
        }
    }

    /// Whether the next 403 or 404 is to be believed: enough of them came in a row.
    fn denial_is_final(&self) -> bool {
        self.denials >= self.config.transient_denials
    }

    /// Shows the error and waits before the next attempt. A 403 or 404 is retried soon a few
    /// times (it can be transient), then less and less often, up to every `denied_retry`.
    async fn fail(&mut self, error: FetchError, backoff: &mut Backoff) {
        tracing::debug!(cluster = %self.cluster, kind = %self.id, namespace = ?self.namespace,
            error = %sync_error_text(error.message()), "watch error");
        let delay = match &error {
            FetchError::Denied(m) | FetchError::NotFound(m) => {
                let final_ = self.denial_is_final();
                self.denials += 1;
                if final_ {
                    self.set_state(SyncState::Failed {
                        error: sync_error_text(m),
                    });
                    self.denied_wait.next()
                } else {
                    self.set_state(SyncState::RetryingRefusal {
                        error: sync_error_text(m),
                    });
                    let n = self.denials.saturating_sub(1).min(8);
                    jittered(self.config.denied_backoff_min * 2u32.pow(n))
                }
            }
            other => {
                self.set_state(SyncState::Reconnecting {
                    error: Some(sync_error_text(&other.to_string())),
                });
                backoff.next()
            }
        };
        self.pause(delay).await;
    }

    /// A watch was refused (403/404), when opened or inside the stream. A few refusals in a
    /// row are retried soon; then the kind is polled, and the watch tried again now and then.
    /// Returns the resource version to watch from next, or `None` when the task should end.
    async fn watch_refused(&mut self, error: FetchError, retrying: bool) -> Option<Option<String>> {
        if !retrying && self.watch_denials < self.config.transient_denials {
            self.watch_denials += 1;
            self.set_state(SyncState::RetryingRefusal {
                error: sync_error_text(&error.to_string()),
            });
            let n = self.watch_denials.saturating_sub(1).min(8);
            self.pause(jittered(self.config.denied_backoff_min * 2u32.pow(n)))
                .await;
            return Some(None);
        }
        self.run_poll(true).await.map(Some)
    }

    // --- list + watch --------------------------------------------------------------------

    async fn run_watch(&mut self) {
        let mut backoff = Backoff::new(&self.config);
        // Grows across relists caused by 410s and resets only once a watch has proven healthy,
        // so a kind whose list outlasts the server's watch window cannot relist in a tight loop.
        let mut relist_backoff = Backoff::new(&self.config);
        let mut expiries = 0u32;
        // Set while trying a watch that was refused before: one refusal goes back to polling.
        let mut retrying_watch = false;
        'sync: loop {
            if expiries > 1 {
                let delay = relist_backoff.next();
                self.pause(delay).await;
            }
            self.begin_relist();
            let (mut rv, mut stream) = match self.initial_sync().await {
                Ok(synced) => synced,
                Err(FetchError::Expired(_)) => {
                    // A continue token expired mid-list.
                    expiries += 1;
                    continue 'sync;
                }
                Err(FetchError::Denied(reason))
                    if self.denial_is_final() && self.scope.is_some() =>
                {
                    // Not in all namespaces: list namespace by namespace until the
                    // all-namespaces list works again.
                    if !self.run_namespaced(reason).await {
                        return;
                    }
                    self.store = Store::new(self.kind.namespaced);
                    self.denials = 0;
                    // The merged rows go with the lanes; the full list loads afresh.
                    self.set_state(SyncState::Loading);
                    continue 'sync;
                }
                Err(e) => {
                    self.fail(e, &mut backoff).await;
                    continue 'sync;
                }
            };
            self.synced_once = true;
            self.set_synced();
            backoff.reset();
            loop {
                let mut events = match stream.take() {
                    Some(s) => s,
                    None => {
                        let fut = self.source.watch(
                            &self.kind,
                            self.namespace.as_deref(),
                            WatchRequest {
                                resource_version: rv.clone(),
                                initial_events: false,
                                timeout_secs: self.config.watch_timeout_secs,
                            },
                        );
                        match self.wait(fut).await {
                            Ok(s) => s,
                            Err(FetchError::Expired(_)) => {
                                expiries += 1;
                                continue 'sync;
                            }
                            Err(FetchError::MethodNotAllowed(_)) => {
                                // The API cannot watch at all: poll for good.
                                self.run_poll(false).await;
                                return;
                            }
                            Err(e @ (FetchError::Denied(_) | FetchError::NotFound(_))) => {
                                // Listing works but watching is refused: retry soon a few
                                // times, then poll and try the watch again now and then.
                                match self.watch_refused(e, retrying_watch).await {
                                    Some(Some(polled)) => {
                                        rv = polled;
                                        retrying_watch = true;
                                    }
                                    Some(None) => {}
                                    None => return,
                                }
                                continue;
                            }
                            Err(e) => {
                                self.fail(e, &mut backoff).await;
                                continue;
                            }
                        }
                    }
                };
                // The watch resumed from a known resource version: the rows are current again.
                retrying_watch = false;
                self.watch_denials = 0;
                self.set_feed(Feed::Watch);
                self.set_synced();
                let started = Instant::now();
                let end = self.consume(&mut events, &mut rv, false).await;
                let healthy = started.elapsed() >= Duration::from_secs(30);
                match end {
                    StreamEnd::Closed { events } => {
                        if healthy || events > 0 {
                            expiries = 0;
                            relist_backoff.reset();
                            backoff.reset();
                            self.watch_retry.reset();
                        } else if started.elapsed() < Duration::from_secs(1) {
                            // A server closing streams at once would otherwise spin.
                            let delay = backoff.next();
                            self.pause(delay).await;
                        }
                    }
                    StreamEnd::Expired => {
                        if healthy {
                            expiries = 0;
                            relist_backoff.reset();
                        }
                        expiries += 1;
                        continue 'sync;
                    }
                    StreamEnd::Failed(e @ (FetchError::Denied(_) | FetchError::NotFound(_))) => {
                        match self.watch_refused(e, false).await {
                            Some(Some(polled)) => {
                                rv = polled;
                                retrying_watch = true;
                            }
                            Some(None) => {}
                            None => return,
                        }
                    }
                    StreamEnd::Failed(e) => self.fail(e, &mut backoff).await,
                    StreamEnd::InitialEnd => {}
                }
            }
        }
    }

    /// Lists everything and returns the resource version to watch from, plus the stream to keep
    /// reading when the list was a streaming list.
    async fn initial_sync(&mut self) -> Result<(String, Option<EventStream>), FetchError> {
        let _permit = self.list_slot().await;
        if self.source.streaming_lists() {
            match self.streaming_list().await {
                Ok((rv, stream)) => return Ok((rv, Some(stream))),
                Err(FetchError::Invalid(m)) => {
                    // Only a rejection of the streaming parameters turns them off for the
                    // cluster; another 400 (one API's quirk) only skips them this time.
                    if is_streaming_rejection(&m) {
                        tracing::info!(cluster = %self.cluster, kind = %self.id,
                            error = %sync_error_text(&m),
                            "streaming lists rejected; using list and watch");
                        self.source.disable_streaming_lists();
                    }
                }
                // The watch verb may be refused where list is allowed.
                Err(FetchError::Denied(_) | FetchError::MethodNotAllowed(_)) => {}
                Err(e) => return Err(e),
            }
        }
        self.list_all().await.map(|rv| (rv, None))
    }

    async fn streaming_list(&mut self) -> Result<(String, EventStream), FetchError> {
        let fut = self.source.watch(
            &self.kind,
            self.namespace.as_deref(),
            WatchRequest {
                resource_version: String::new(),
                initial_events: true,
                timeout_secs: self.config.watch_timeout_secs,
            },
        );
        let mut stream = self.wait(fut).await?;
        self.store.begin_resync();
        let mut rv = String::new();
        let result = match self.consume(&mut stream, &mut rv, true).await {
            StreamEnd::InitialEnd => {
                self.store.finish_resync();
                self.dirty = true;
                if !self.store.has_layout() {
                    // An empty collection: no event carried the column definitions. One list
                    // of one row brings them, so even an empty list shows the right columns.
                    let fut =
                        self.source
                            .list(&self.kind, self.namespace.as_deref(), Some(1), None);
                    if let Ok(page) = self.wait(fut).await {
                        // Nothing is visible yet, so this never asks for a relist.
                        let _ = self.store.learn_columns(&page);
                    }
                }
                return Ok((rv, stream));
            }
            StreamEnd::Closed { .. } => Err(FetchError::Other(
                "the watch ended before the initial list was complete".into(),
            )),
            StreamEnd::Expired => Err(FetchError::Expired("expired".into())),
            StreamEnd::Failed(e) => Err(e),
        };
        self.store.abandon_resync();
        result
    }

    /// A paginated list. Returns its resource version.
    async fn list_all(&mut self) -> Result<String, FetchError> {
        self.store.begin_resync();
        let mut token = None;
        loop {
            let fut = self.source.list(
                &self.kind,
                self.namespace.as_deref(),
                Some(self.config.page_size),
                token.take(),
            );
            let page = match self.wait(fut).await {
                Ok(p) => p,
                Err(e) => {
                    self.store.abandon_resync();
                    return Err(e);
                }
            };
            let rv = page.metadata.resource_version.clone().unwrap_or_default();
            let next = page.metadata.continue_.clone().filter(|c| !c.is_empty());
            self.store.apply(page, false, unix_now());
            match next {
                Some(t) => token = Some(t),
                None => {
                    self.store.finish_resync();
                    self.dirty = true;
                    return Ok(rv);
                }
            }
        }
    }

    async fn consume(
        &mut self,
        stream: &mut EventStream,
        rv: &mut String,
        initial: bool,
    ) -> StreamEnd {
        // Servers send a bookmark about every minute on an idle watch. Much longer silence means
        // a dead connection (laptop sleep, network change); resuming from the last version is
        // cheap, so do not wait for the server's own timeout.
        let stall = self.config.watch_stall;
        let mut events = 0;
        loop {
            let item = match self.wait(tokio::time::timeout(stall, stream.next())).await {
                Err(_) => return StreamEnd::Failed(FetchError::Other("the watch stalled".into())),
                Ok(None) => return StreamEnd::Closed { events },
                Ok(Some(item)) => item,
            };
            let event = match item {
                Ok(e) => e,
                Err(FetchError::Expired(_)) => return StreamEnd::Expired,
                Err(e) => return StreamEnd::Failed(e),
            };
            events += 1;
            let event_rv = event
                .object
                .metadata
                .resource_version
                .clone()
                .filter(|v| !v.is_empty());
            match event.ty {
                EventType::Added | EventType::Modified | EventType::Deleted => {
                    if self.store.lacks_columns_for(&event.object) {
                        // Relisting brings the definitions with the rows.
                        return StreamEnd::Expired;
                    }
                    if let Some(v) = event_rv {
                        *rv = v;
                    }
                    let delete = event.ty == EventType::Deleted;
                    let applied = self.store.apply(event.object, delete, unix_now());
                    if applied.changed {
                        self.dirty = true;
                    }
                    if applied.relayout {
                        // New column definitions (an edited CRD): the stored rows' cells
                        // follow the old ones. Relist so every row matches.
                        return StreamEnd::Expired;
                    }
                }
                // Detected by type only: a bookmark's single row is fake.
                EventType::Bookmark => {
                    if self.store.learn_columns(&event.object) {
                        // New column definitions before any row event (an edited CRD's
                        // printer columns, first event of a resumed watch): relist.
                        return StreamEnd::Expired;
                    }
                    if let Some(v) = event_rv {
                        *rv = v;
                    }
                    if initial && event.object.is_initial_events_end() {
                        return StreamEnd::InitialEnd;
                    }
                }
                EventType::Error => {
                    let o = &event.object;
                    return match FetchError::from_code(o.code.unwrap_or(0), o.status_message()) {
                        FetchError::Expired(_) => StreamEnd::Expired,
                        e => StreamEnd::Failed(e),
                    };
                }
                EventType::Unknown => {}
            }
        }
    }

    // --- polling -------------------------------------------------------------------------

    /// Lists on an interval. With `retry_watch`, returns the resource version of a successful
    /// list once it is time to try watching again (the watch was refused, maybe only for now).
    async fn run_poll(&mut self, retry_watch: bool) -> Option<String> {
        self.set_feed(Feed::Poll);
        let mut backoff = Backoff::new(&self.config);
        let retry_at = retry_watch.then(|| Instant::now() + self.watch_retry.next());
        loop {
            let permit = self.list_slot().await;
            let result = self.list_all().await;
            drop(permit);
            match result {
                Ok(rv) => {
                    backoff.reset();
                    self.set_synced();
                    if retry_at.is_some_and(|at| Instant::now() >= at) {
                        return Some(rv);
                    }
                    self.pause(jittered(self.config.poll_interval)).await;
                }
                Err(e) => self.fail(e, &mut backoff).await,
            }
        }
    }

    // --- namespace by namespace ----------------------------------------------------------

    /// Lists and watches the kind in each namespace of the scope, one lane (an engine of its
    /// own) per namespace, and publishes the lanes' rows merged into one list. Follows the
    /// scope as it changes, and tries the all-namespaces list again now and then (the refusal
    /// may have been transient, or the user got more rights). Returns `true` when that list
    /// works again; lanes stop when this returns or the task stops.
    async fn run_namespaced(&mut self, reason: String) -> bool {
        let Some(mut scope) = self.scope.clone() else {
            return false;
        };
        tracing::info!(cluster = %self.cluster, kind = %self.id,
            "not allowed in all namespaces; listing namespace by namespace");
        let reason = sync_error_text(&reason);
        self.store = Store::new(true);
        let changed = Arc::new(Notify::new());
        let mut lanes: BTreeMap<String, Lane> = BTreeMap::new();
        let mut published: Option<Instant> = None;
        let mut recheck =
            Backoff::between(self.config.denied_wait_min * 6, self.config.denied_retry);
        let mut recheck_at = Instant::now() + recheck.next();
        loop {
            let wanted = scope.borrow_and_update().clone();
            lanes.retain(|ns, _| wanted.contains(ns));
            for ns in wanted.iter() {
                if !lanes.contains_key(ns) {
                    let lane = self.start_lane(ns.clone(), changed.clone());
                    lanes.insert(ns.clone(), lane);
                }
            }
            // Rows are merged only for subscribers; status displays need the state alone.
            let listening = self.hub.listeners() > 0;
            let merged = merge_lanes(&lanes, &reason, self.kind.namespaced, listening, None);
            let state = merged.state.clone();
            // For a subscriber, synced is reported by the snapshot that carries the merged rows.
            if !listening || state != SyncState::Synced {
                self.set_state(state.clone());
            }
            self.set_feed(merged.feed);
            if self.hub.has_peeks() {
                self.answer_namespaced_peeks(&lanes, &reason).await;
            }
            if listening {
                // At most one snapshot per interval, however many lanes change.
                if let Some(at) = published {
                    let due = at + self.config.publish_interval;
                    if Instant::now() < due {
                        sleep_until(due).await;
                        continue;
                    }
                }
                let snapshot = KindSnapshot {
                    cluster: self.cluster.clone(),
                    kind: self.id.clone(),
                    ..merged
                };
                self.hub.publish(self.epoch, snapshot);
                self.set_state(state);
                published = Some(Instant::now());
            }
            tokio::select! {
                _ = changed.notified() => {}
                _ = self.hub.wake.notified() => {
                    // A subscriber arrived: the lanes publish the rows they held back.
                    for lane in lanes.values() {
                        lane.hub.wake.notify_one();
                    }
                }
                res = scope.changed() => if res.is_err() {
                    // The cluster's watches are going away.
                    return false;
                },
                _ = sleep_until(recheck_at) => {
                    let fut = self.source.list(&self.kind, None, Some(1), None);
                    match fut.await {
                        Ok(_) => {
                            tracing::info!(cluster = %self.cluster, kind = %self.id,
                                "allowed in all namespaces again");
                            return true;
                        }
                        Err(_) => recheck_at = Instant::now() + recheck.next(),
                    }
                }
            }
        }
    }

    /// Answers one-off reads of the merged rows ([`Hub::peek`]). The lanes publish only while
    /// the merged list has subscribers, so each lane is asked for its current rows the same
    /// way, and the answers are merged. Nothing is published and the state is not touched: a
    /// read must not change what status displays show. While it waits for the lanes (at most
    /// [`LANE_PEEK_WAIT`]), this task follows no scope change and publishes nothing; the lanes
    /// keep watching meanwhile.
    async fn answer_namespaced_peeks(&self, lanes: &BTreeMap<String, Lane>, reason: &str) {
        let asked: Vec<(String, oneshot::Receiver<Arc<KindSnapshot>>)> = lanes
            .iter()
            .map(|(ns, lane)| (ns.clone(), lane.hub.peek()))
            .collect();
        let deadline = Instant::now() + LANE_PEEK_WAIT;
        let mut answers = HashMap::new();
        for (ns, rx) in asked {
            if let Ok(Ok(snapshot)) = tokio::time::timeout_at(deadline, rx).await {
                answers.insert(ns, snapshot);
            }
        }
        let merged = merge_lanes(lanes, reason, self.kind.namespaced, true, Some(&answers));
        self.hub.answer_peeks(self.epoch, || KindSnapshot {
            cluster: self.cluster.clone(),
            kind: self.id.clone(),
            ..merged
        });
    }

    fn start_lane(&self, namespace: String, changed: Arc<Notify>) -> Lane {
        let hub = Arc::new(super::hub::Hub::child(
            self.cluster.clone(),
            self.id.clone(),
            changed,
        ));
        let rx = hub.subscribe();
        let mut loading =
            KindSnapshot::empty(self.cluster.clone(), self.id.clone(), SyncState::Loading);
        loading.namespaced = true;
        let epoch = hub.take_over(loading);
        let engine = Engine::new(
            self.cluster.clone(),
            self.kind.clone(),
            self.source.clone(),
            hub.clone(),
            epoch,
            self.config.clone(),
            self.limiter.clone(),
        )
        .in_namespace(namespace, self.hub.clone());
        Lane {
            task: tokio::spawn(engine.run(Feed::Watch)),
            hub,
            rx,
        }
    }
}

/// One namespace of a namespace-limited list. Stops its task when dropped.
struct Lane {
    task: JoinHandle<()>,
    hub: Arc<Hub>,
    rx: watch::Receiver<Arc<KindSnapshot>>,
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The lanes' latest snapshots as one list, in namespace order (lanes are keyed by namespace and
/// each lane's rows are sorted by name, so concatenating keeps the snapshot order).
///
/// Lanes whose list was refused are left out and named in the limit. The state is the least
/// ready of the other lanes; with no lane left, the list failed, and says how to pick
/// namespaces.
/// How long a namespace-limited list waits for its lanes' rows to answer a one-off read.
const LANE_PEEK_WAIT: Duration = Duration::from_millis(250);

/// `answers`: the lanes' rows as they answered a one-off read, used instead of their published
/// snapshots (a lane missing from them counts by its state alone).
fn merge_lanes(
    lanes: &BTreeMap<String, Lane>,
    reason: &str,
    namespaced: bool,
    with_rows: bool,
    answers: Option<&HashMap<String, Arc<KindSnapshot>>>,
) -> KindSnapshot {
    struct View<'a> {
        ns: &'a String,
        /// The latest snapshot: rows and columns (set only `with_rows`).
        snap: Option<Arc<KindSnapshot>>,
        /// From the snapshot, or from the hub (current even while the lane publishes no
        /// snapshots) when rows are not merged.
        state: SyncState,
        feed: Feed,
        /// The revision of the lane's rows merged in.
        revision: u64,
    }
    let views: Vec<View> = lanes
        .iter()
        .map(|(ns, lane)| {
            // With rows, the state that came with them, so they agree; without, the hub's.
            let snap = match answers {
                Some(answers) => answers.get(ns).cloned(),
                None => with_rows.then(|| lane.rx.borrow().clone()),
            };
            let (state, feed) = match &snap {
                Some(s) => (s.state.clone(), s.feed),
                None => (lane.hub.state(), lane.hub.feed()),
            };
            let revision = match &snap {
                Some(s) => s.revision,
                None => lane.rx.borrow().revision,
            };
            View {
                ns,
                snap,
                state,
                feed,
                revision,
            }
        })
        .collect();
    let (denied, open): (Vec<&View>, Vec<&View>) = views
        .iter()
        .partition(|v| matches!(v.state, SyncState::Failed { .. }));
    // Columns from the first lane that knows them; a lane whose columns differ (mid-relist
    // after a CRD edit) waits until it matches, so cells always fit the columns.
    let snaps: Vec<&Arc<KindSnapshot>> = open.iter().filter_map(|v| v.snap.as_ref()).collect();
    let columns: Option<Arc<[Column]>> = snaps
        .iter()
        .find(|s| !s.columns.is_empty() && s.format == ListFormat::Table)
        .or_else(|| snaps.iter().find(|s| !s.columns.is_empty()))
        .map(|s| s.columns.clone());
    let mut rows = Vec::new();
    let mut format = ListFormat::default();
    let mut lagging = false;
    if let Some(columns) = &columns {
        for s in &snaps {
            if Arc::ptr_eq(&s.columns, columns) || *s.columns == **columns {
                rows.extend(s.rows.iter().cloned());
                format = s.format;
            } else if !s.rows.is_empty() {
                lagging = true;
            }
        }
    }
    let state = if open.is_empty() {
        SyncState::Failed {
            error: match denied.first() {
                Some(v) => match &v.state {
                    SyncState::Failed { error } => error.clone(),
                    _ => reason.to_owned(),
                },
                None => format!(
                    "{reason}. Pick the namespaces you may use in the namespace filter to list \
                     them one by one."
                ),
            },
        }
    } else if open
        .iter()
        .any(|v| v.state == SyncState::Loading || v.state == SyncState::Idle)
    {
        SyncState::Loading
    } else if let Some(state) = most_troubled(open.iter().map(|v| &v.state)) {
        state.clone()
    } else if lagging {
        SyncState::Reconnecting { error: None }
    } else {
        SyncState::Synced
    };
    let feed = if open.iter().any(|v| v.feed == Feed::Poll) {
        Feed::Poll
    } else {
        Feed::Watch
    };
    KindSnapshot {
        namespaced,
        columns: columns.unwrap_or_else(|| Arc::new([])),
        rows,
        state,
        format,
        feed,
        limited_to: Some(Arc::new(NamespaceLimit {
            listed: open.iter().map(|v| v.ns.clone()).collect(),
            denied: denied.iter().map(|v| v.ns.clone()).collect(),
            reason: reason.to_owned(),
            revisions: open.iter().map(|v| v.revision).collect(),
        })),
        ..Default::default()
    }
}

/// The state a merged list reports for lanes that are retrying: a connection error first, so a
/// refusal in one namespace cannot hide it, then a refusal, then a routine relist.
pub(super) fn most_troubled<'a>(
    states: impl Iterator<Item = &'a SyncState>,
) -> Option<&'a SyncState> {
    states
        .filter_map(|state| match state {
            SyncState::Reconnecting { error: Some(_) } => Some((0, state)),
            SyncState::RetryingRefusal { .. } => Some((1, state)),
            SyncState::Reconnecting { error: None } => Some((2, state)),
            _ => None,
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, state)| state)
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// The most characters of a server's error text a [`SyncState`] carries.
const MAX_SYNC_ERROR_CHARS: usize = 200;

/// What a [`SyncState`] may say of an error: the first line of the text with values hidden
/// ([`redact::hide_server_values`]), cut to [`MAX_SYNC_ERROR_CHARS`]. Values are hidden in the
/// whole text first, so the cut cannot split one. Decisions about an error read the
/// [`FetchError`], never this text.
fn sync_error_text(raw: &str) -> String {
    redact::first_line(&redact::hide_server_values(raw), MAX_SYNC_ERROR_CHARS)
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Whether a 400/422 is the server refusing streaming lists (feature off or unknown).
fn is_streaming_rejection(message: &str) -> bool {
    ["sendInitialEvents", "resourceVersionMatch", "WatchList"]
        .iter()
        .any(|m| message.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{ColumnRole, ColumnType};

    #[test]
    fn sync_error_text_is_one_clean_line() {
        let text = sync_error_text(
            "Forbidden: value \"abc123\" with Authorization: Bearer abcdefghijklmnop\rmore\nnext",
        );
        assert!(!text.contains("abc123"), "{text}");
        assert!(!text.contains("abcdefghijklmnop"), "{text}");
        assert!(!text.contains("next"), "{text}");
        assert!(!text.chars().any(char::is_control), "{text}");
        assert!(text.starts_with("Forbidden: value"), "{text}");
    }

    #[test]
    fn sync_error_text_is_capped_on_a_char_boundary() {
        for long in ["é".repeat(5000), "\"\"".repeat(500), "x ".repeat(5000)] {
            let text = sync_error_text(&long);
            assert!(text.chars().count() <= MAX_SYNC_ERROR_CHARS + 1, "{text}");
        }
    }

    #[test]
    fn sync_error_text_keeps_the_explanation_after_a_long_quoted_value() {
        let value = "v".repeat(500);
        let text = sync_error_text(&format!(
            "pods \"{value}\" is forbidden: missing permission"
        ));
        assert_eq!(text, "pods \"…\" is forbidden: missing permission");
    }

    #[test]
    fn sync_error_text_keeps_a_plain_message() {
        assert_eq!(sync_error_text("connection refused"), "connection refused");
    }

    fn lane(state: SyncState, rows: &[&str], columns: &Arc<[Column]>, ns: &str) -> Lane {
        let hub = super::super::hub::Hub::new("c".into(), KindId::pods());
        let rx = hub.subscribe();
        let snapshot = KindSnapshot {
            namespaced: true,
            columns: columns.clone(),
            rows: rows
                .iter()
                .map(|n| {
                    Arc::new(crate::table::Row {
                        uid: format!("{ns}/{n}").into(),
                        name: (*n).into(),
                        namespace: Some(ns.into()),
                        resource_version: "1".into(),
                        generation: None,
                        created: None,
                        labels: Box::new([]),
                        cells: Box::new([crate::table::Cell::Name]),
                    })
                })
                .collect(),
            state,
            format: ListFormat::Table,
            // A revision of its own per namespace, as its store would give.
            revision: u64::from(ns.as_bytes()[0]),
            ..Default::default()
        };
        hub.take_over(snapshot);
        Lane {
            task: tokio::spawn(async {}),
            hub: Arc::new(hub),
            rx,
        }
    }

    fn columns() -> Arc<[Column]> {
        Arc::new([Column {
            name: "Name".into(),
            ty: ColumnType::String,
            format: "name".into(),
            description: "".into(),
            priority: 0,
            role: ColumnRole::Name,
        }])
    }

    fn names(s: &KindSnapshot) -> Vec<&str> {
        s.rows.iter().map(|r| &*r.name).collect()
    }

    // covers UP-20, UP-21
    #[tokio::test]
    async fn lanes_merge_in_namespace_order_and_refused_ones_are_named() {
        let cols = columns();
        let mut lanes = BTreeMap::new();
        lanes.insert("b".to_owned(), lane(SyncState::Synced, &["y"], &cols, "b"));
        lanes.insert(
            "a".to_owned(),
            lane(SyncState::Synced, &["x", "z"], &cols, "a"),
        );
        lanes.insert(
            "c".to_owned(),
            lane(SyncState::Failed { error: "no".into() }, &[], &cols, "c"),
        );
        let merged = merge_lanes(&lanes, "forbidden", true, true, None);
        assert_eq!(names(&merged), ["x", "z", "y"]);
        assert_eq!(merged.state, SyncState::Synced);
        let limit = merged.limited_to.unwrap();
        assert_eq!(
            (limit.listed.clone(), limit.denied.clone()),
            (vec!["a".to_owned(), "b".to_owned()], vec!["c".to_owned()])
        );
        // Each listed namespace says which of its lists was merged in.
        let lane_revision = |ns: &str| lanes[ns].rx.borrow().revision;
        assert_eq!(limit.revisions, [lane_revision("a"), lane_revision("b")]);
        assert_eq!(limit.revision_of("b"), Some(lane_revision("b")));
        assert_eq!(limit.revision_of("c"), None, "denied");

        // The least ready lane decides the state.
        lanes.insert("d".to_owned(), lane(SyncState::Loading, &[], &cols, "d"));
        assert_eq!(
            merge_lanes(&lanes, "forbidden", true, true, None).state,
            SyncState::Loading
        );
    }

    // covers UP-21
    #[tokio::test]
    async fn without_lanes_the_list_fails_and_says_how_to_pick_namespaces() {
        let merged = merge_lanes(&BTreeMap::new(), "forbidden", true, true, None);
        let SyncState::Failed { error } = merged.state else {
            panic!("failed expected");
        };
        assert!(error.starts_with("forbidden") && error.contains("namespace filter"));
        assert!(merged.rows.is_empty());
    }

    #[test]
    fn backoff_doubles_to_its_maximum_and_resets() {
        let mut b = Backoff::between(Duration::from_secs(1), Duration::from_secs(4));
        let first = b.next();
        assert!(first >= Duration::from_secs(1) && first < Duration::from_millis(1250));
        b.next();
        let third = b.next();
        assert!(third >= Duration::from_secs(4) && third < Duration::from_secs(5));
        assert!(b.next() < Duration::from_secs(5), "capped");
        b.reset();
        assert!(b.next() < Duration::from_millis(1250));
    }

    #[test]
    fn jitter_varies_even_on_a_microsecond_clock() {
        // A microsecond clock: every nanosecond reading is a multiple of 1000.
        let fractions: Vec<f64> = (1..=200u64).map(|us| jitter_fraction(us * 1000)).collect();
        assert!(fractions.iter().all(|f| (0.0..0.25).contains(f)));
        let distinct: std::collections::BTreeSet<u64> =
            fractions.iter().map(|f| f.to_bits()).collect();
        assert!(distinct.len() > 150, "{} distinct", distinct.len());
        assert!(fractions.iter().any(|f| *f < 0.05) && fractions.iter().any(|f| *f > 0.2));
    }

    #[test]
    fn jittered_adds_up_to_a_quarter_and_varies() {
        let base = Duration::from_millis(500);
        let delays: std::collections::BTreeSet<Duration> =
            (0..50).map(|_| jittered(base)).collect();
        assert!(
            delays.iter().all(|d| *d >= base && *d < base.mul_f64(1.25)),
            "{delays:?}"
        );
        assert!(delays.len() > 1, "{delays:?}");
    }

    #[test]
    fn the_same_clock_value_gets_different_keys() {
        let keys: std::collections::BTreeSet<u64> = (0..20).map(|_| keyed(42)).collect();
        assert!(keys.len() > 1, "{keys:?}");
    }

    #[test]
    fn backoffs_in_the_same_instant_get_different_jitter() {
        let delays: std::collections::BTreeSet<Duration> = (0..20)
            .map(|_| Backoff::between(Duration::from_secs(1), Duration::from_secs(4)).next())
            .collect();
        assert!(delays.len() > 1, "{delays:?}");
    }

    #[test]
    fn timing_overrides_only_the_retry_waits() {
        let timing = crate::handle::WatchTiming {
            backoff_min: Duration::from_millis(10),
            backoff_max: Duration::from_millis(50),
            denied_backoff_min: Duration::from_millis(10),
        };
        let config = EngineConfig::with_timing(&timing);
        assert_eq!(config.backoff_min, Duration::from_millis(10));
        assert_eq!(config.denied_backoff_min, Duration::from_millis(10));
        assert_eq!(config.page_size, EngineConfig::default().page_size);
        let default = EngineConfig::with_timing(&crate::handle::WatchTiming::default());
        assert_eq!(default.backoff_max, EngineConfig::default().backoff_max);
    }

    #[test]
    fn only_streaming_parameter_errors_turn_streaming_lists_off() {
        assert!(is_streaming_rejection(
            "unknown parameter sendInitialEvents"
        ));
        assert!(is_streaming_rejection("resourceVersionMatch is forbidden"));
        assert!(!is_streaming_rejection("field selector is not supported"));
    }
}
