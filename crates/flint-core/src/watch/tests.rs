//! Engine and watch manager tests against a scripted fake API server.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::source::{EventStream, Source, WatchRequest};
use super::*;
use crate::discovery::tests::kind;
use crate::read::KindInfo;
use crate::store::RefusalCause;
use crate::table::tests::pod_table;
use crate::table::{FetchError, WireEvent, WireObject};

type Push = mpsc::UnboundedSender<Result<WireEvent, FetchError>>;

/// What the next watch call of a kind returns.
enum WatchScript {
    /// A stream fed by the test through the sender handed out by [`Fake::stream_sender`].
    Live,
    Fail(FetchError),
}

#[derive(Default)]
struct FakeState {
    /// List responses per plural (or `plural/namespace` for one namespace); the last one
    /// repeats.
    lists: HashMap<String, VecDeque<Value>>,
    /// Keys (as for `lists`) whose lists fail with this error, before any scripted page.
    list_errors: HashMap<String, VecDeque<FetchError>>,
    watches: HashMap<String, VecDeque<WatchScript>>,
    senders: HashMap<String, Vec<Push>>,
    list_calls: HashMap<String, usize>,
    /// How long each list takes to answer.
    list_delay: Duration,
    watch_requests: HashMap<String, Vec<WatchRequest>>,
}

#[derive(Default)]
struct Fake {
    state: Mutex<FakeState>,
    streaming: AtomicBool,
    /// Watch streams currently open (dropped streams decrement it).
    open_streams: Arc<AtomicUsize>,
}

struct OpenGuard(Arc<AtomicUsize>);
impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Fake::default())
    }
    fn list(&self, plural: &str, page: Value) {
        let mut s = self.state.lock().unwrap();
        s.lists.entry(plural.into()).or_default().push_back(page);
    }
    /// The next list of `key` fails with `error`.
    fn list_error(&self, key: &str, error: FetchError) {
        let mut s = self.state.lock().unwrap();
        s.list_errors
            .entry(key.into())
            .or_default()
            .push_back(error);
    }
    fn script(&self, plural: &str, script: WatchScript) {
        let mut s = self.state.lock().unwrap();
        s.watches
            .entry(plural.into())
            .or_default()
            .push_back(script);
    }
    fn list_calls(&self, plural: &str) -> usize {
        *self
            .state
            .lock()
            .unwrap()
            .list_calls
            .get(plural)
            .unwrap_or(&0)
    }
    fn watch_requests(&self, plural: &str) -> Vec<WatchRequest> {
        self.state
            .lock()
            .unwrap()
            .watch_requests
            .get(plural)
            .cloned()
            .unwrap_or_default()
    }
    /// The sender of the newest live stream of `plural`.
    fn sender(&self, plural: &str) -> Option<Push> {
        self.state
            .lock()
            .unwrap()
            .senders
            .get(plural)
            .and_then(|v| v.last().cloned())
    }
    fn open_streams(&self) -> usize {
        self.open_streams.load(Ordering::SeqCst)
    }
}

impl Source for Fake {
    fn list(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        _limit: Option<u32>,
        _token: Option<String>,
    ) -> BoxFuture<'static, Result<WireObject, FetchError>> {
        let key = key(kind, namespace);
        let mut s = self.state.lock().unwrap();
        *s.list_calls.entry(key.clone()).or_default() += 1;
        if let Some(error) = s.list_errors.get_mut(&key).and_then(VecDeque::pop_front) {
            let delay = s.list_delay;
            return async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                Err(error)
            }
            .boxed();
        }
        let queue = s.lists.entry(key).or_default();
        let page = if queue.len() > 1 {
            queue.pop_front()
        } else {
            queue.front().cloned()
        }
        .unwrap_or_else(|| json!({"kind":"Table","metadata":{"resourceVersion":"1"},"columnDefinitions":[],"rows":[]}));
        let parsed: WireObject = serde_json::from_value(page).unwrap();
        let delay = s.list_delay;
        async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Ok(parsed)
        }
        .boxed()
    }

    fn watch(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        request: WatchRequest,
    ) -> BoxFuture<'static, Result<EventStream, FetchError>> {
        let mut s = self.state.lock().unwrap();
        let plural = key(kind, namespace);
        s.watch_requests
            .entry(plural.clone())
            .or_default()
            .push(request);
        let script = s
            .watches
            .get_mut(&plural)
            .and_then(VecDeque::pop_front)
            .unwrap_or(WatchScript::Live);
        let result = match script {
            WatchScript::Fail(e) => Err(e),
            WatchScript::Live => {
                let (tx, rx) = mpsc::unbounded_channel();
                s.senders.entry(plural).or_default().push(tx);
                self.open_streams.fetch_add(1, Ordering::SeqCst);
                let guard = OpenGuard(self.open_streams.clone());
                let stream = futures::stream::unfold((rx, guard), |(mut rx, guard)| async move {
                    rx.recv().await.map(|item| (item, (rx, guard)))
                });
                Ok(stream.boxed())
            }
        };
        async move { result }.boxed()
    }

    fn streaming_lists(&self) -> bool {
        self.streaming.load(Ordering::SeqCst)
    }

    fn disable_streaming_lists(&self) {
        self.streaming.store(false, Ordering::SeqCst);
    }
}

/// How the fake keys a kind's lists and watches: `plural`, or `plural/namespace`.
fn key(kind: &KindInfo, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("{}/{ns}", kind.plural()),
        None => kind.plural().to_owned(),
    }
}

fn event(ty: &str, object: Value) -> Result<WireEvent, FetchError> {
    Ok(serde_json::from_value(json!({"type": ty, "object": object})).unwrap())
}

fn pod(uid: &str, name: &str, status: &str, rv: &str, columns: bool) -> Value {
    pod_table(&[(uid, "demo", name, status)], columns, rv)
}

const LW: &[&str] = &["get", "list", "watch"];

fn pods() -> KindInfo {
    kind("", "Pod", "pods", true, LW)
}

fn config() -> Arc<EngineConfig> {
    Arc::new(EngineConfig {
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(50),
        poll_interval: Duration::from_millis(500),
        ..EngineConfig::default()
    })
}

/// Starts an engine for `kind` with a listener on its hub.
fn start_engine(
    fake: &Arc<Fake>,
    kind: KindInfo,
    feed: Feed,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Receiver<Arc<KindSnapshot>>,
) {
    let (task, rx, _) = start_engine_with(fake, kind, feed, None);
    (task, rx)
}

/// [`start_engine`] with a namespace scope for namespace-limited lists, returning the hub too.
fn start_engine_with(
    fake: &Arc<Fake>,
    kind: KindInfo,
    feed: Feed,
    scope: Option<engine::Scope>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Receiver<Arc<KindSnapshot>>,
    Arc<Hub>,
) {
    start_engine_configured(fake, kind, feed, scope, config())
}

/// [`start_engine_with`] with other timings.
fn start_engine_configured(
    fake: &Arc<Fake>,
    kind: KindInfo,
    feed: Feed,
    scope: Option<engine::Scope>,
    config: Arc<EngineConfig>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Receiver<Arc<KindSnapshot>>,
    Arc<Hub>,
) {
    let hub = Arc::new(Hub::new("c".into(), kind.id()));
    let rx = hub.subscribe();
    let epoch = hub.take_over(KindSnapshot::default());
    let engine = engine::Engine::new(
        "c".into(),
        kind,
        fake.clone(),
        hub.clone(),
        epoch,
        config,
        Arc::new(tokio::sync::Semaphore::new(4)),
    )
    .with_scope(scope);
    (tokio::spawn(engine.run(feed)), rx, hub)
}

fn forbidden() -> FetchError {
    FetchError::Denied("pods is forbidden: User \"dev\" cannot list resource \"pods\"".into())
}

/// Waits (in paused time) up to 30 s until the newest snapshot satisfies `pred`.
async fn until(
    rx: &mut tokio::sync::watch::Receiver<Arc<KindSnapshot>>,
    pred: impl Fn(&KindSnapshot) -> bool,
) -> Arc<KindSnapshot> {
    until_within(rx, Duration::from_secs(30), pred).await
}

/// [`until`] with another limit.
async fn until_within(
    rx: &mut tokio::sync::watch::Receiver<Arc<KindSnapshot>>,
    limit: Duration,
    pred: impl Fn(&KindSnapshot) -> bool,
) -> Arc<KindSnapshot> {
    tokio::time::timeout(limit, async {
        loop {
            let snap = rx.borrow_and_update().clone();
            if pred(&snap) {
                return snap;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("condition reached")
}

fn names(snap: &KindSnapshot) -> Vec<String> {
    snap.rows.iter().map(|r| r.name.to_string()).collect()
}

// covers UP-20
#[tokio::test(start_paused = true)]
async fn lists_then_applies_watch_events_and_bookmarks() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    let snap = until(&mut rx, |s| s.state == SyncState::Synced).await;
    assert_eq!(names(&snap), ["a"]);
    assert_eq!(fake.watch_requests("pods")[0].resource_version, "10");

    let tx = fake.sender("pods").unwrap();
    // Only the first watch event carries column definitions.
    tx.send(event("ADDED", pod("u2", "b", "Pending", "11", true)))
        .unwrap();
    tx.send(event("MODIFIED", pod("u2", "b", "Running", "12", false)))
        .unwrap();
    tx.send(event("DELETED", pod("u1", "a", "Running", "13", false)))
        .unwrap();
    // A bookmark's fake row is never a row.
    tx.send(event(
        "BOOKMARK",
        json!({"kind":"Table","metadata":{"resourceVersion":"20"},"columnDefinitions":null,
               "rows":[{"cells":["","0/0","","0","<unknown>",""],"object":{"metadata":{"resourceVersion":"20"}}}]}),
    ))
    .unwrap();
    let snap = until(&mut rx, |s| names(s) == ["b"]).await;
    assert_eq!(snap.rows[0].cell_text(3, 0), "Running");
    // The stream ends (server timeout): the watch resumes from the bookmark's version.
    drop(tx);
    fake.state.lock().unwrap().senders.clear();
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.watch_requests("pods").len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fake.watch_requests("pods")[1].resource_version, "20");
    assert_eq!(fake.list_calls("pods"), 1, "no relist after a normal close");
    task.abort();
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn expired_error_event_inside_a_200_relists() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "old", "Running")], true, "10"),
    );
    fake.list(
        "pods",
        pod_table(&[("u2", "demo", "new", "Running")], true, "50"),
    );
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    let tx = fake.sender("pods").unwrap();
    tx.send(event(
        "ERROR",
        json!({"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure",
               "message":"too old resource version: 1 (773)","reason":"Expired","code":410}),
    ))
    .unwrap();
    let snap = until(&mut rx, |s| {
        names(s) == ["new"] && s.state == SyncState::Synced
    })
    .await;
    assert_eq!(snap.rows.len(), 1);
    assert_eq!(fake.list_calls("pods"), 2);
    let watches = fake.watch_requests("pods");
    assert_eq!(
        watches.last().unwrap().resource_version,
        "50",
        "watch from the new list"
    );
    task.abort();
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_relist_reports_synced_only_with_its_rows() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "old", "Running")], true, "10"),
    );
    fake.list(
        "pods",
        pod_table(&[("u2", "demo", "new", "Running")], true, "50"),
    );
    let (task, mut rx, hub) = start_engine_with(&fake, pods(), Feed::Watch, None);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    // Past the publish interval, so the reconnecting state is published just before the list
    // and the finished list falls inside the next interval.
    tokio::time::sleep(Duration::from_millis(200)).await;
    fake.sender("pods")
        .unwrap()
        .send(event(
            "ERROR",
            json!({"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure",
                   "message":"too old resource version: 1 (773)","reason":"Expired","code":410}),
        ))
        .unwrap();
    until(&mut rx, |s| {
        matches!(s.state, SyncState::Reconnecting { .. })
    })
    .await;
    let reconnecting = tokio::time::Instant::now();
    // Each step lets the paused clock advance. The state is read first: rows read after it
    // are at least as new as the snapshot that reported it.
    for _ in 0..1000 {
        if hub.state() == SyncState::Synced {
            assert_eq!(
                names(&hub.latest()),
                ["new"],
                "synced was reported before the rows"
            );
            assert_eq!(fake.list_calls("pods"), 2);
            assert!(
                reconnecting.elapsed() >= config().publish_interval,
                "two snapshots inside one publish interval"
            );
            task.abort();
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the relist was never reported as synced");
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_relist_reports_synced_after_its_last_listener_leaves() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "old", "Running")], true, "10"),
    );
    fake.list(
        "pods",
        pod_table(&[("u2", "demo", "new", "Running")], true, "50"),
    );
    let (task, mut rx, hub) = start_engine_with(&fake, pods(), Feed::Watch, None);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    fake.sender("pods")
        .unwrap()
        .send(event(
            "ERROR",
            json!({"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure",
                   "message":"too old resource version: 1 (773)","reason":"Expired","code":410}),
        ))
        .unwrap();
    until(&mut rx, |s| {
        matches!(s.state, SyncState::Reconnecting { .. })
    })
    .await;
    // The list is done and its synced state waits for the next snapshot.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(fake.list_calls("pods"), 2);
    assert!(matches!(hub.state(), SyncState::Reconnecting { .. }));
    drop(rx);
    tokio::time::sleep(config().publish_interval).await;
    assert_eq!(hub.state(), SyncState::Synced);
    assert!(
        matches!(hub.latest().state, SyncState::Reconnecting { .. }),
        "no snapshot is built for nobody"
    );
    task.abort();
}

// covers UP-21, UP-27
#[tokio::test(start_paused = true)]
async fn a_merged_relist_reports_synced_only_with_its_rows() {
    let fake = Fake::new();
    for _ in 0..10 {
        fake.list_error("pods", forbidden());
    }
    fake.list(
        "pods/a",
        pod_table(&[("u1", "a", "old", "Running")], true, "10"),
    );
    fake.list(
        "pods/a",
        pod_table(&[("u2", "a", "new", "Running")], true, "50"),
    );
    let (_scope_tx, scope) =
        tokio::sync::watch::channel::<Arc<[String]>>(Arc::from(vec!["a".to_owned()]));
    let (task, mut rx, hub) = start_engine_with(&fake, pods(), Feed::Watch, Some(scope));
    until(&mut rx, |s| {
        s.state == SyncState::Synced && names(s) == ["old"]
    })
    .await;
    // Halfway through the publish interval, with a relist that ends halfway through a later
    // one: the merged list has just published "reconnecting" when the lane is synced again.
    tokio::time::sleep(Duration::from_millis(50)).await;
    fake.state.lock().unwrap().list_delay = Duration::from_millis(200);
    fake.sender("pods/a")
        .unwrap()
        .send(event(
            "ERROR",
            json!({"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure",
                   "message":"too old resource version: 1 (773)","reason":"Expired","code":410}),
        ))
        .unwrap();
    let mut reconnected = false;
    for _ in 0..1000 {
        let state = hub.state();
        reconnected |= state != SyncState::Synced;
        let latest = hub.latest();
        if names(&latest) == ["new"] && latest.state == SyncState::Synced {
            assert_eq!(state, SyncState::Synced);
            assert!(reconnected, "the merged list never showed the reconnect");
            task.abort();
            return;
        }
        assert!(
            !(reconnected && state == SyncState::Synced),
            "synced was reported before the merged rows"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the merged relist was never published");
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn repeated_410s_relist_with_growing_backoff() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    for _ in 0..50 {
        fake.script(
            "pods",
            WatchScript::Fail(FetchError::Expired("too old".into())),
        );
    }
    let (task, _rx) = start_engine(&fake, pods(), Feed::Watch);
    // backoff 10 ms doubling to 50 ms (test config): 2 s allows a few dozen relists at most,
    // where an unthrottled loop would run thousands.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let lists = fake.list_calls("pods");
    assert!((3..=60).contains(&lists), "{lists} relists in 2 s");
    task.abort();
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn errors_back_off_and_show_reconnecting() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    fake.script("pods", WatchScript::Live);
    fake.script(
        "pods",
        WatchScript::Fail(FetchError::Other("connection refused".into())),
    );
    fake.script(
        "pods",
        WatchScript::Fail(FetchError::Other("connection refused".into())),
    );
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    // Past the publish interval, so the reconnect is not coalesced away.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let tx = fake.sender("pods").unwrap();
    tx.send(Err(FetchError::Other("stream broke".into())))
        .unwrap();
    let snap = until(&mut rx, |s| {
        matches!(s.state, SyncState::Reconnecting { .. })
    })
    .await;
    assert_eq!(
        names(&snap),
        ["a"],
        "last known rows stay, marked as reconnecting"
    );
    // After two refused attempts the watch is back, from the same version: no relist.
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    let watches = fake.watch_requests("pods");
    assert_eq!(watches.len(), 4);
    assert!(watches.iter().all(|w| w.resource_version == "10"));
    assert_eq!(fake.list_calls("pods"), 1);
    task.abort();
}

/// A state as the tests below compare it, without its text.
fn shape(state: &SyncState) -> &'static str {
    match state {
        SyncState::Synced => "synced",
        SyncState::Reconnecting { error: None } => "relisting",
        SyncState::Reconnecting { error: Some(_) } => "error",
        SyncState::Failed { .. } => "failed",
        _ => "other",
    }
}

/// The hub's states, sampled every millisecond without repeats, until it is synced again.
/// Every synced sample must already have `rows` published.
async fn states_until_synced_again(hub: &Hub, rows: &[&str]) -> Vec<&'static str> {
    let mut seen = vec![shape(&hub.state())];
    for _ in 0..3000 {
        tokio::time::sleep(Duration::from_millis(1)).await;
        // The state is read first: rows read after it are at least as new.
        let state = hub.state();
        let now = shape(&state);
        if seen.last() != Some(&now) {
            seen.push(now);
            if state == SyncState::Synced {
                assert_eq!(
                    names(&hub.latest()),
                    rows,
                    "synced before its rows: {seen:?}"
                );
                return seen;
            }
        }
    }
    panic!("never synced again: {seen:?}");
}

fn expired_event() -> Result<WireEvent, FetchError> {
    event(
        "ERROR",
        json!({"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure",
               "message":"too old resource version: 1 (773)","reason":"Expired","code":410}),
    )
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn an_outage_keeps_its_error_across_relists() {
    let fake = Fake::new();
    fake.list("pods", pod("u1", "old", "Running", "10", true));
    fake.list("pods", pod("u2", "new", "Running", "50", true));
    let (task, mut rx, hub) = start_engine_with(&fake, pods(), Feed::Watch, None);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for _ in 0..3 {
        fake.list_error("pods", FetchError::Other("connection refused".into()));
    }
    fake.state.lock().unwrap().list_delay = Duration::from_millis(5);
    fake.sender("pods").unwrap().send(expired_event()).unwrap();
    assert_eq!(
        states_until_synced_again(&hub, &["new"]).await,
        ["synced", "relisting", "error", "synced"]
    );
    task.abort();
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_held_synced_state_does_not_clear_the_outage_error() {
    let fake = Fake::new();
    fake.list("pods", pod("u1", "old", "Running", "10", true));
    fake.list("pods", pod("u2", "new", "Running", "50", true));
    fake.script("pods", WatchScript::Live);
    // The list after the outage works, but its watch expires before the snapshot is due.
    fake.script(
        "pods",
        WatchScript::Fail(FetchError::Expired("too old".into())),
    );
    let (task, mut rx, hub) = start_engine_with(&fake, pods(), Feed::Watch, None);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for _ in 0..2 {
        fake.list_error("pods", FetchError::Other("connection refused".into()));
    }
    fake.state.lock().unwrap().list_delay = Duration::from_millis(5);
    fake.sender("pods").unwrap().send(expired_event()).unwrap();
    assert_eq!(
        states_until_synced_again(&hub, &["new"]).await,
        ["synced", "relisting", "error", "synced"]
    );
    task.abort();
}

/// Waits (in paused time) until `key` has been listed `calls` times.
async fn until_listed(fake: &Fake, key: &str, calls: usize) {
    for _ in 0..20_000 {
        if fake.list_calls(key) >= calls {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("{key} was not listed {calls} times");
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_relist_backoff_does_not_publish_a_held_synced_state() {
    let fake = Fake::new();
    fake.list("pods", pod("u1", "old", "Running", "10", true));
    fake.list("pods", pod("u2", "new", "Running", "50", true));
    fake.list("pods", pod("u3", "newer", "Running", "60", true));
    fake.script("pods", WatchScript::Live);
    // The list after the outage works, but its watch expires before the snapshot is due.
    fake.script(
        "pods",
        WatchScript::Fail(FetchError::Expired("too old".into())),
    );
    // Every backoff is longer than the publish interval, so the snapshot falls due during it.
    let timing = Arc::new(EngineConfig {
        backoff_min: Duration::from_millis(200),
        backoff_max: Duration::from_millis(200),
        ..EngineConfig::default()
    });
    let (task, mut rx, hub) = start_engine_configured(&fake, pods(), Feed::Watch, None, timing);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    fake.list_error("pods", FetchError::Other("connection refused".into()));
    fake.state.lock().unwrap().list_delay = Duration::from_millis(50);
    fake.sender("pods").unwrap().send(expired_event()).unwrap();
    until_listed(&fake, "pods", 3).await;
    assert_eq!(shape(&hub.state()), "error");
    // A snapshot now makes the one for the list in flight wait out the publish interval.
    let _peek = hub.peek();
    assert_eq!(
        states_until_synced_again(&hub, &["newer"]).await,
        ["error", "synced"]
    );
    task.abort();
}

// covers UP-21, UP-27
#[tokio::test(start_paused = true)]
async fn a_lane_recovering_from_a_refusal_keeps_its_rows_through_a_410() {
    let fake = Fake::new();
    for _ in 0..10 {
        fake.list_error("pods", forbidden());
    }
    for _ in 0..4 {
        fake.list_error("pods/a", forbidden());
    }
    fake.list(
        "pods/a",
        pod_table(&[("u1", "a", "fresh", "Running")], true, "10"),
    );
    fake.list(
        "pods/a",
        pod_table(&[("u2", "a", "newer", "Running")], true, "20"),
    );
    fake.script(
        "pods/a",
        WatchScript::Fail(FetchError::Expired("too old".into())),
    );
    let (_scope_tx, scope) =
        tokio::sync::watch::channel::<Arc<[String]>>(Arc::from(vec!["a".to_owned()]));
    let (task, mut rx, hub) = start_engine_with(&fake, pods(), Feed::Watch, Some(scope));
    until(&mut rx, |s| {
        matches!(s.state, SyncState::Failed { .. })
            && s.limited_to.as_ref().is_some_and(|l| l.denied == ["a"])
    })
    .await;
    fake.state.lock().unwrap().list_delay = Duration::from_millis(50);
    until_listed(&fake, "pods/a", 5).await;
    // The relist after the 410 takes long enough to be seen.
    fake.state.lock().unwrap().list_delay = Duration::from_millis(300);
    // A snapshot now makes the lane's recovery snapshot wait out the publish interval.
    let _peek = hub.peek();
    let snap = until(&mut rx, |s| {
        s.state == (SyncState::Reconnecting { error: None }) && names(s) == ["fresh"]
    })
    .await;
    let limit = snap.limited_to.as_ref().unwrap();
    assert_eq!(limit.listed, ["a"]);
    assert!(limit.denied.is_empty(), "{:?}", limit.denied);
    until(&mut rx, |s| {
        s.state == SyncState::Synced && names(s) == ["newer"]
    })
    .await;
    task.abort();
}

/// A server message with a quoted value, a token and a second line.
const NOISY_MESSAGE: &str =
    "denied value \"quoted-value\" Authorization: Bearer abcdefghijklmnop\nsecond line";

fn assert_clean(text: &str) {
    assert!(!text.contains("quoted-value"), "{text}");
    assert!(!text.contains("abcdefghijklmnop"), "{text}");
    assert!(!text.contains("second line"), "{text}");
    assert!(!text.chars().any(char::is_control), "{text}");
    assert!(text.contains("denied value"), "{text}");
}

fn reconnecting_error(snap: &KindSnapshot) -> Option<&str> {
    match &snap.state {
        SyncState::Reconnecting { error } => error.as_deref(),
        SyncState::RetryingRefusal { error, .. } => Some(error),
        _ => None,
    }
}

fn refusal(text: &str) -> SyncState {
    SyncState::RetryingRefusal {
        error: text.into(),
        cause: Some(RefusalCause::Denied),
    }
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_refused_list_carries_its_cause_from_retry_to_failure() {
    let cases: [(fn() -> FetchError, RefusalCause); 2] = [
        (forbidden, RefusalCause::Denied),
        (
            || FetchError::NotFound("not found".into()),
            RefusalCause::NotFound,
        ),
    ];
    for (error, expected) in cases {
        let fake = Fake::new();
        for _ in 0..10 {
            fake.list_error("pods", error());
        }
        let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
        let snap = until(&mut rx, |s| {
            !matches!(s.state, SyncState::Idle | SyncState::Loading)
        })
        .await;
        let SyncState::RetryingRefusal { cause, .. } = &snap.state else {
            panic!("a refusal being retried: {:?}", snap.state);
        };
        assert_eq!(*cause, Some(expected));
        let snap = until(&mut rx, |s| {
            assert!(
                !matches!(s.state, SyncState::Reconnecting { .. }),
                "a refusal never reports a broken connection: {:?}",
                s.state
            );
            matches!(s.state, SyncState::Failed { .. })
        })
        .await;
        let SyncState::Failed { cause, .. } = &snap.state else {
            unreachable!()
        };
        assert_eq!(*cause, Some(expected));
        task.abort();
    }
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_refused_list_being_retried_is_typed_and_clean() {
    let fake = Fake::new();
    for _ in 0..10 {
        fake.list_error("pods", FetchError::Denied(NOISY_MESSAGE.into()));
    }
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    let snap = until(&mut rx, |s| {
        !matches!(s.state, SyncState::Idle | SyncState::Loading)
    })
    .await;
    let SyncState::RetryingRefusal { error, .. } = &snap.state else {
        panic!("a refusal being retried: {:?}", snap.state);
    };
    assert_clean(error);
    until(&mut rx, |s| matches!(s.state, SyncState::Failed { .. })).await;
    task.abort();
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_refused_watch_being_retried_is_typed_and_keeps_its_rows() {
    for error in [forbidden(), FetchError::NotFound("not found".into())] {
        let fake = Fake::new();
        fake.list("pods", pod("u1", "a", "Running", "10", true));
        fake.script("pods", WatchScript::Live);
        fake.script("pods", WatchScript::Fail(error));
        let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
        until(&mut rx, |s| s.state == SyncState::Synced).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(fake.state.lock().unwrap().senders.remove("pods"));
        let snap = until(&mut rx, |s| s.state != SyncState::Synced).await;
        assert!(
            matches!(snap.state, SyncState::RetryingRefusal { .. }),
            "{:?}",
            snap.state
        );
        assert_eq!(names(&snap), ["a"]);
        task.abort();
    }
}

#[test]
fn a_merged_list_reports_a_connection_error_over_a_refusal() {
    let relist = SyncState::Reconnecting { error: None };
    let down = SyncState::Reconnecting {
        error: Some("connection refused".into()),
    };
    let refused = refusal("forbidden");
    let pick = |states: &[&SyncState]| engine::most_troubled(states.iter().copied()).cloned();
    assert_eq!(pick(&[&refused, &relist, &down]), Some(down.clone()));
    assert_eq!(pick(&[&relist, &refused]), Some(refused.clone()));
    assert_eq!(pick(&[&SyncState::Synced, &relist]), Some(relist));
    assert_eq!(pick(&[&SyncState::Synced]), None);
}

#[tokio::test]
async fn stored_rows_include_a_kind_retrying_a_refusal() {
    let registry = HubRegistry::default();
    let hub = registry.get_or_create("c", &KindId::pods());
    let _rx = hub.subscribe();
    hub.take_over(KindSnapshot::empty(
        "c".into(),
        KindId::pods(),
        refusal("forbidden"),
    ));
    let stored = registry.stored("c", Duration::from_millis(10)).await;
    assert_eq!(stored.len(), 1);
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_relist_during_a_refusal_keeps_the_refusal() {
    let fake = Fake::new();
    fake.list("pods", pod("u1", "old", "Running", "10", true));
    fake.list("pods", pod("u2", "new", "Running", "50", true));
    let (task, mut rx, hub) = start_engine_with(&fake, pods(), Feed::Watch, None);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for _ in 0..2 {
        fake.list_error("pods", forbidden());
    }
    fake.state.lock().unwrap().list_delay = Duration::from_millis(5);
    fake.sender("pods").unwrap().send(expired_event()).unwrap();
    assert_eq!(
        states_until_synced_again(&hub, &["new"]).await,
        ["synced", "relisting", "other", "synced"]
    );
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn an_error_event_in_a_stream_shows_clean_text() {
    for (code, reason) in [
        (500, "InternalError"),
        (403, "Forbidden"),
        (404, "NotFound"),
    ] {
        let fake = Fake::new();
        fake.list(
            "pods",
            pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
        );
        let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
        until(&mut rx, |s| s.state == SyncState::Synced).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        fake.sender("pods")
            .unwrap()
            .send(event(
                "ERROR",
                json!({"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure",
                       "message":NOISY_MESSAGE,"reason":reason,"code":code}),
            ))
            .unwrap();
        let snap = until(&mut rx, |s| reconnecting_error(s).is_some()).await;
        assert_clean(reconnecting_error(&snap).unwrap());
        task.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_list_shows_clean_text() {
    let fake = Fake::new();
    for _ in 0..10 {
        fake.list_error("pods", FetchError::Denied(NOISY_MESSAGE.into()));
    }
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    let snap = until(&mut rx, |s| reconnecting_error(s).is_some()).await;
    assert_clean(reconnecting_error(&snap).unwrap());
    let snap = until(&mut rx, |s| matches!(s.state, SyncState::Failed { .. })).await;
    let SyncState::Failed { error, .. } = &snap.state else {
        unreachable!()
    };
    assert_clean(error);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_namespace_limited_list_shows_clean_text() {
    let fake = Fake::new();
    for _ in 0..10 {
        fake.list_error("pods", FetchError::Denied(NOISY_MESSAGE.into()));
    }
    let (_scope_tx, scope) = tokio::sync::watch::channel::<Arc<[String]>>(Arc::from(Vec::new()));
    let (task, mut rx, _) = start_engine_with(&fake, pods(), Feed::Watch, Some(scope));
    let snap = until(&mut rx, |s| {
        matches!(s.state, SyncState::Failed { .. }) && s.limited_to.is_some()
    })
    .await;
    let SyncState::Failed { error, .. } = &snap.state else {
        unreachable!()
    };
    assert_clean(error);
    assert!(error.contains("namespace filter"), "{error}");
    assert_clean(&snap.limited_to.as_ref().unwrap().reason);
    task.abort();
}

/// A 403 `Status` whose URL has its host past the length of a shown error.
fn long_url_status() -> kube::Error {
    let password = "placeholder-password-".repeat(20);
    kube::Error::Api(Box::new(kube::core::Status {
        code: 403,
        reason: "Forbidden".into(),
        message: format!("forbidden: https://placeholder:{password}@api.example.test/ refused"),
        ..Default::default()
    }))
}

#[tokio::test(start_paused = true)]
async fn an_api_status_is_hidden_from_its_full_text() {
    let target = crate::cluster::Target {
        server: None,
        has_exec_plugin: false,
        exec_command: None,
    };
    let fake = Fake::new();
    for _ in 0..10 {
        fake.list_error("pods", FetchError::from_kube(long_url_status(), &target));
    }
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    let snap = until(&mut rx, |s| reconnecting_error(s).is_some()).await;
    let text = reconnecting_error(&snap).unwrap();
    assert!(!text.contains("placeholder-password"), "{text}");
    assert!(text.contains("refused"), "{text}");
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_very_long_list_error_is_cut() {
    let fake = Fake::new();
    // Enough failures in a row that one outlasts the publish interval.
    for _ in 0..50 {
        fake.list_error("pods", FetchError::Other("é".repeat(100_000)));
    }
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    let snap = until(&mut rx, |s| reconnecting_error(s).is_some()).await;
    assert!(reconnecting_error(&snap).unwrap().chars().count() <= 201);
    task.abort();
}

// covers UP-28
#[tokio::test(start_paused = true)]
async fn apis_that_cannot_watch_are_polled() {
    let fake = Fake::new();
    let metrics = kind(
        "metrics.k8s.io",
        "PodMetrics",
        "pods",
        true,
        &["get", "list"],
    );
    fake.list(
        "pods",
        json!({"kind":"PodMetricsList","metadata":{},"items":[{"metadata":{"name":"p","namespace":"demo"}}]}),
    );
    let (task, mut rx) = start_engine(&fake, metrics, Feed::Poll);
    let snap = until(&mut rx, |s| s.state == SyncState::Synced).await;
    assert_eq!(snap.feed, Feed::Poll);
    assert_eq!(snap.format, crate::table::ListFormat::Fallback);
    assert_eq!(names(&snap), ["p"]);
    // Two more polls: 500 ms each, plus up to 25% jitter.
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    assert!(fake.list_calls("pods") >= 3, "listed again on an interval");
    assert!(fake.watch_requests("pods").is_empty(), "never watched");
    task.abort();

    // A kind discovery calls watchable whose watch answers 405 falls back to polling.
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    fake.script(
        "pods",
        WatchScript::Fail(FetchError::MethodNotAllowed("405".into())),
    );
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    until(&mut rx, |s| {
        s.feed == Feed::Poll && s.state == SyncState::Synced
    })
    .await;
    task.abort();
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_transient_403_or_404_is_retried_soon_instead_of_failing() {
    let fake = Fake::new();
    // An API server that just restarted: its authorizer refuses, then a recreated CRD is
    // briefly not served, then the list works.
    fake.list_error("pods", forbidden());
    fake.list_error("pods", FetchError::NotFound("not found".into()));
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    let started = tokio::time::Instant::now();
    let mut seen_failed = false;
    let snap = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snap = rx.borrow_and_update().clone();
            seen_failed |= matches!(snap.state, SyncState::Failed { .. });
            if snap.state == SyncState::Synced {
                return snap;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(!seen_failed, "a transient refusal is shown as reconnecting");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(names(&snap), ["a"]);
    assert_eq!(fake.list_calls("pods"), 3);
    task.abort();

    // A refusal that persists is believed after a few tries, and then retried slowly.
    let fake = Fake::new();
    for _ in 0..5 {
        fake.list_error("pods", forbidden());
    }
    let nodes = kind("", "Node", "nodes", false, LW);
    fake.list_error("nodes", forbidden());
    for _ in 0..5 {
        fake.list_error("nodes", forbidden());
    }
    let (task, mut rx) = start_engine(&fake, nodes, Feed::Watch);
    until(&mut rx, |s| matches!(s.state, SyncState::Failed { .. })).await;
    let calls = fake.list_calls("nodes");
    assert_eq!(calls, 4, "three quick retries, then failed");
    // Then less and less often: 5 s, 10 s, 20 s, 40 s… up to denied_retry.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(fake.list_calls("nodes"), calls, "not at once");
    tokio::time::sleep(Duration::from_secs(76)).await;
    let later = fake.list_calls("nodes");
    assert!((calls + 3..=calls + 5).contains(&later), "{later}");
    task.abort();
}

// covers UP-27, UP-28
#[tokio::test(start_paused = true)]
async fn a_refused_watch_is_polled_reported_as_polled_and_tried_again() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    for _ in 0..4 {
        fake.script("pods", WatchScript::Fail(forbidden()));
    }
    // The second retry finds the watch allowed again.
    fake.script("pods", WatchScript::Fail(forbidden()));
    fake.script("pods", WatchScript::Live);
    let (task, mut rx, hub) = start_engine_with(&fake, pods(), Feed::Watch, None);
    until(&mut rx, |s| {
        s.feed == Feed::Poll && s.state == SyncState::Synced
    })
    .await;
    assert_eq!(hub.feed(), Feed::Poll, "watched_kinds reports the poll");
    let refused = fake.watch_requests("pods").len();
    assert_eq!(refused, 4, "three quick retries, then polling");
    // Polls run every poll_interval; the watch is tried again after a successful poll, at a
    // growing interval, and taken back once it works.
    until(&mut rx, |s| {
        s.feed == Feed::Watch && s.state == SyncState::Synced
    })
    .await;
    assert_eq!(hub.feed(), Feed::Watch);
    assert_eq!(fake.watch_requests("pods").len(), 6);
    assert!(fake.list_calls("pods") >= 3, "polled meanwhile");
    task.abort();
}

// covers UP-22
#[tokio::test(start_paused = true)]
async fn new_columns_in_a_bookmark_relist_instead_of_relabelling_old_cells() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    let mut edited = pod_table(&[("u1", "demo", "a", "Running")], true, "30");
    edited["columnDefinitions"][1]["name"] = "Ready Now".into();
    fake.list("pods", edited);
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    let before = until(&mut rx, |s| s.state == SyncState::Synced).await;
    let tx = fake.sender("pods").unwrap();
    // A resumed watch whose first event is a bookmark with the edited CRD's columns.
    let mut bookmark = pod_table(&[], true, "20");
    bookmark["columnDefinitions"][1]["name"] = "Ready Now".into();
    tx.send(event("BOOKMARK", bookmark)).unwrap();
    let after = until(&mut rx, |s| {
        s.state == SyncState::Synced && s.columns.iter().any(|c| &*c.name == "Ready Now")
    })
    .await;
    assert_eq!(fake.list_calls("pods"), 2, "relisted");
    assert!(!Arc::ptr_eq(&before.columns, &after.columns));
    for row in &after.rows {
        assert_eq!(row.cells.len(), after.columns.len());
    }
    task.abort();
}

// covers UP-20, UP-21
#[tokio::test(start_paused = true)]
async fn a_namespace_limited_user_gets_the_namespaces_they_may_list_merged() {
    let fake = Fake::new();
    for _ in 0..10 {
        fake.list_error("pods", forbidden());
    }
    fake.list(
        "pods/b",
        pod_table(&[("u2", "b", "y", "Running")], true, "20"),
    );
    fake.list(
        "pods/a",
        pod_table(&[("u1", "a", "x", "Running")], true, "10"),
    );
    for _ in 0..10 {
        fake.list_error("pods/secret", forbidden());
    }
    fake.list(
        "pods/c",
        pod_table(&[("u3", "c", "z", "Pending")], true, "30"),
    );
    let (scope_tx, scope) = tokio::sync::watch::channel::<Arc<[String]>>(Arc::from(Vec::new()));
    let (task, mut rx, _) = start_engine_with(&fake, pods(), Feed::Watch, Some(scope));

    // Nothing to fall back to yet: say how to get a list.
    let snap = until(&mut rx, |s| matches!(s.state, SyncState::Failed { .. })).await;
    let SyncState::Failed { error, .. } = &snap.state else {
        unreachable!()
    };
    assert!(error.contains("namespace filter"), "{error}");
    assert!(snap.limited_to.is_some());

    // The scope arrives (the filter, the default namespace, or the listable namespaces).
    scope_tx.send_replace(Arc::from(vec![
        "b".to_owned(),
        "a".to_owned(),
        "secret".to_owned(),
    ]));
    let snap = until(&mut rx, |s| {
        s.state == SyncState::Synced && s.limited_to.as_ref().is_some_and(|l| !l.denied.is_empty())
    })
    .await;
    assert_eq!(names(&snap), ["x", "y"], "merged in namespace order");
    let limit = snap.limited_to.as_ref().unwrap();
    assert_eq!(limit.listed, ["a", "b"]);
    assert_eq!(limit.denied, ["secret"]);
    assert!(limit.reason.contains("forbidden"));
    assert_eq!(snap.columns.len(), 7);
    assert!(fake.watch_requests("pods/a").len() == 1);

    // Live updates arrive per namespace.
    fake.sender("pods/b")
        .unwrap()
        .send(event(
            "ADDED",
            pod_table(&[("u4", "b", "y2", "Running")], true, "21"),
        ))
        .unwrap();
    let snap = until(&mut rx, |s| s.rows.len() == 3).await;
    assert_eq!(names(&snap), ["x", "y", "y2"]);

    // The scope changes: lanes follow, and the removed namespace's rows go.
    scope_tx.send_replace(Arc::from(vec!["c".to_owned()]));
    let snap = until(&mut rx, |s| {
        s.state == SyncState::Synced && names(s) == ["z"]
    })
    .await;
    assert_eq!(snap.limited_to.as_ref().unwrap().listed, ["c"]);
    assert_eq!(fake.open_streams(), 1, "the lanes of a and b stopped");
    task.abort();
}

// covers UP-31: a one-off read of a namespace-limited list nobody subscribes to gets every
// lane's current rows, and leaves the list's state (what status displays show) as it was.
#[tokio::test(start_paused = true)]
async fn a_peek_of_a_namespace_limited_list_reads_every_lane() {
    let fake = Fake::new();
    for _ in 0..10 {
        fake.list_error("pods", forbidden());
    }
    fake.list(
        "pods/a",
        pod_table(&[("u1", "a", "x", "Running")], true, "10"),
    );
    fake.list(
        "pods/b",
        pod_table(&[("u2", "b", "y", "Running")], true, "20"),
    );
    let (_scope_tx, scope) = tokio::sync::watch::channel::<Arc<[String]>>(Arc::from(vec![
        "a".to_owned(),
        "b".to_owned(),
    ]));
    let hub = Arc::new(Hub::new("c".into(), pods().id()));
    let epoch = hub.take_over(KindSnapshot::default());
    let engine = engine::Engine::new(
        "c".into(),
        pods(),
        fake.clone(),
        hub.clone(),
        epoch,
        config(),
        Arc::new(tokio::sync::Semaphore::new(4)),
    )
    .with_scope(Some(scope));
    let task = tokio::spawn(engine.run(Feed::Watch));
    tokio::time::timeout(Duration::from_secs(30), async {
        while hub.state() != SyncState::Synced {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both namespaces list");
    assert_eq!(hub.listeners(), 0);

    let answer = tokio::time::timeout(Duration::from_secs(1), hub.peek())
        .await
        .expect("answered")
        .expect("answered");
    assert_eq!(names(&answer), ["x", "y"], "both namespaces' rows");
    assert_eq!(answer.state, SyncState::Synced);
    assert_eq!(hub.state(), SyncState::Synced);
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert_eq!(
        hub.state(),
        SyncState::Synced,
        "the read left the state alone"
    );
    assert!(hub.latest().rows.is_empty(), "nothing published");
    task.abort();
}

// covers UP-27
#[tokio::test(start_paused = true)]
async fn a_namespace_limited_list_goes_back_to_all_namespaces_when_allowed_again() {
    let fake = Fake::new();
    // Refused long enough to be believed (an authorizer that took its time after a restart).
    for _ in 0..4 {
        fake.list_error("pods", forbidden());
    }
    fake.list(
        "pods",
        pod_table(
            &[("u1", "a", "x", "Running"), ("u2", "b", "y", "Running")],
            true,
            "10",
        ),
    );
    fake.list(
        "pods/a",
        pod_table(&[("u1", "a", "x", "Running")], true, "5"),
    );
    let (_scope_tx, scope) =
        tokio::sync::watch::channel::<Arc<[String]>>(Arc::from(vec!["a".to_owned()]));
    let (task, mut rx, _) = start_engine_with(&fake, pods(), Feed::Watch, Some(scope));
    let snap = until(&mut rx, |s| {
        s.state == SyncState::Synced && s.limited_to.is_some()
    })
    .await;
    assert_eq!(names(&snap), ["x"]);
    // The all-namespaces list is tried again after a while (30 s plus up to 25% jitter), and
    // taken back once it works.
    let snap = until_within(&mut rx, Duration::from_secs(45), |s| {
        s.state == SyncState::Synced && s.limited_to.is_none()
    })
    .await;
    assert_eq!(names(&snap), ["x", "y"]);
    assert_eq!(fake.open_streams(), 1, "the lane stopped");
    task.abort();
}

// covers UP-20
#[tokio::test(start_paused = true)]
async fn streaming_list_is_used_when_supported() {
    let fake = Fake::new();
    fake.streaming.store(true, Ordering::SeqCst);
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.sender("pods").is_none() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let tx = fake.sender("pods").unwrap();
    tx.send(event("ADDED", pod("u1", "a", "Running", "5", true)))
        .unwrap();
    tx.send(event("ADDED", pod("u2", "b", "Running", "6", false)))
        .unwrap();
    // Nothing is shown before the end marker.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rx.borrow().rows.is_empty());
    tx.send(event(
        "BOOKMARK",
        json!({"kind":"Table","metadata":{"resourceVersion":"7"},"columnDefinitions":null,
               "rows":[{"cells":[],"object":{"metadata":{"resourceVersion":"7",
               "annotations":{"k8s.io/initial-events-end":"true"}}}}]}),
    ))
    .unwrap();
    let snap = until(&mut rx, |s| s.state == SyncState::Synced).await;
    assert_eq!(names(&snap), ["a", "b"]);
    assert_eq!(fake.list_calls("pods"), 0);
    assert!(fake.watch_requests("pods")[0].initial_events);
    // The same stream keeps delivering changes.
    tx.send(event("DELETED", pod("u1", "a", "Running", "8", false)))
        .unwrap();
    until(&mut rx, |s| names(s) == ["b"]).await;
    task.abort();

    // A server that rejects streaming lists: list + watch, and not asked again.
    let fake = Fake::new();
    fake.streaming.store(true, Ordering::SeqCst);
    fake.script(
        "pods",
        WatchScript::Fail(FetchError::Invalid("sendInitialEvents is forbidden".into())),
    );
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    assert!(!fake.streaming_lists());
    assert_eq!(fake.list_calls("pods"), 1);
    task.abort();
}

// covers UP-22
#[tokio::test(start_paused = true)]
async fn an_empty_streaming_list_still_learns_its_columns() {
    let fake = Fake::new();
    fake.streaming.store(true, Ordering::SeqCst);
    // The probe list: no rows, but column definitions.
    fake.list("pods", pod_table(&[], true, "7"));
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.sender("pods").is_none() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let tx = fake.sender("pods").unwrap();
    // The first (and only) initial event is the end bookmark, without columns.
    tx.send(event(
        "BOOKMARK",
        json!({"kind":"Table","metadata":{"resourceVersion":"7"},"columnDefinitions":null,
               "rows":[{"cells":[],"object":{"metadata":{"resourceVersion":"7",
               "annotations":{"k8s.io/initial-events-end":"true"}}}}]}),
    ))
    .unwrap();
    let snap = until(&mut rx, |s| s.state == SyncState::Synced).await;
    assert_eq!(snap.columns.len(), 7, "server columns, not the fallback");
    // A later object arrives without definitions and still maps onto the columns.
    tx.send(event("ADDED", pod("u1", "a", "Running", "8", false)))
        .unwrap();
    let snap = until(&mut rx, |s| s.rows.len() == 1).await;
    assert_eq!(snap.rows[0].cell_text(3, 0), "Running");
    task.abort();
}

// covers UP-26
#[tokio::test(start_paused = true)]
async fn bursts_are_coalesced_to_one_snapshot_per_interval() {
    let fake = Fake::new();
    fake.list("pods", pod_table(&[], true, "1"));
    let (task, mut rx) = start_engine(&fake, pods(), Feed::Watch);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    let tx = fake.sender("pods").unwrap();
    rx.borrow_and_update();

    // A reader that wakes for every change notification, like the UI task.
    let counter = Arc::new(AtomicUsize::new(0));
    let reader = {
        let counter = counter.clone();
        let mut rx = rx.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                rx.borrow_and_update();
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })
    };
    // 1000 events over one second (a 200-pod scale-down is ~330).
    let n = 1000;
    let span = Duration::from_secs(1);
    let started = tokio::time::Instant::now();
    for i in 0..n {
        let uid = format!("u{i}");
        let name = format!("p{i:04}");
        tx.send(event(
            "ADDED",
            pod(&uid, &name, "Running", &format!("{}", i + 2), false),
        ))
        .unwrap();
        tokio::time::sleep(span / n).await;
    }
    let elapsed = started.elapsed();
    until(&mut rx, |s| s.rows.len() == n as usize).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The last snapshot may land up to one interval after the last event.
    let bound = (elapsed.as_millis() as u64).div_ceil(100) + 1;
    let notified = counter.load(Ordering::SeqCst) as u64;
    assert!(
        notified <= bound,
        "{notified} UI notifications for {n} events in {elapsed:?} (bound {bound})"
    );
    assert!(
        notified >= 5,
        "updates kept flowing during the burst: {notified}"
    );
    reader.abort();
    task.abort();
}

// covers UP-26
#[tokio::test(start_paused = true)]
async fn nothing_is_built_while_nobody_listens() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    let hub = Arc::new(Hub::new("c".into(), pods().id()));
    let epoch = hub.take_over(KindSnapshot::default());
    let engine = engine::Engine::new(
        "c".into(),
        pods(),
        fake.clone(),
        hub.clone(),
        epoch,
        config(),
        Arc::new(tokio::sync::Semaphore::new(4)),
    );
    let initial = hub.latest().version;
    let task = tokio::spawn(engine.run(Feed::Watch));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        hub.latest().version,
        initial,
        "only the manager's initial snapshot"
    );
    // A subscriber arrives: the held-back state is published at once.
    let mut rx = hub.subscribe();
    hub.wake.notify_one();
    let snap = until(&mut rx, |s| s.state == SyncState::Synced).await;
    assert_eq!(names(&snap), ["a"]);
    task.abort();
}

// covers UP-31: the resource switcher reads a watch's rows from memory. The watch nobody
// subscribes to builds them from its store when woken; nothing is listed or watched again.
#[tokio::test(start_paused = true)]
async fn stored_rows_are_read_without_another_request() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(
            &[
                ("u1", "demo", "a", "Running"),
                ("u2", "demo", "b", "Running"),
            ],
            true,
            "10",
        ),
    );
    let registry = HubRegistry::default();
    let hub = registry.get_or_create("c", &pods().id());
    let epoch = hub.take_over(KindSnapshot::default());
    let engine = engine::Engine::new(
        "c".into(),
        pods(),
        fake.clone(),
        hub.clone(),
        epoch,
        config(),
        Arc::new(tokio::sync::Semaphore::new(4)),
    );
    let task = tokio::spawn(engine.run(Feed::Watch));
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry.states("c") != [(pods().id(), SyncState::Synced)] {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the watch lists");
    assert_eq!(hub.latest().rows.len(), 0, "nothing published while unread");
    let (lists, watches) = (fake.list_calls("pods"), fake.watch_requests("pods").len());

    let stored = registry.stored("c", Duration::from_secs(1)).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(names(&stored[0]), ["a", "b"]);
    // Read again: built anew (a new version), but the same rows (the same revision).
    let again = registry.stored("c", Duration::from_secs(1)).await;
    assert_ne!(again[0].version, stored[0].version);
    assert_eq!(again[0].revision, stored[0].revision);
    assert!(stored[0].revision > 0);
    assert_eq!(fake.list_calls("pods"), lists, "no list");
    assert_eq!(fake.watch_requests("pods").len(), watches, "no watch");
    assert_eq!(hub.listeners(), 0, "the call let go of the hub");
    // Nothing stays built in the hub: it still holds no rows, as before the read.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        hub.latest().rows.is_empty(),
        "the rows were left in the hub"
    );
    task.abort();
}

// ---- watch manager ----------------------------------------------------------------------

fn kinds() -> Vec<KindInfo> {
    vec![
        kind("", "Pod", "pods", true, LW),
        kind("", "ConfigMap", "configmaps", true, LW),
        kind("", "Binding", "bindings", true, &["create"]),
        kind("events.k8s.io", "Event", "events", true, LW),
        kind(
            "metrics.k8s.io",
            "PodMetrics",
            "pods",
            true,
            &["get", "list"],
        ),
        kind(
            "apiextensions.k8s.io",
            "CustomResourceDefinition",
            "customresourcedefinitions",
            false,
            LW,
        ),
    ]
}

fn watches(fake: &Arc<Fake>, hubs: &HubRegistry) -> ClusterWatches {
    let mut w = ClusterWatches::new("c".into(), hubs.clone(), config());
    w.set_source(Some(fake.clone()));
    w.set_kinds(&kinds());
    w
}

fn running(w: &ClusterWatches) -> Vec<(String, Feed)> {
    let mut v: Vec<(String, Feed)> = w
        .running()
        .into_iter()
        .map(|(k, f)| (k.to_string(), f))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn subs(ids: &[KindId]) -> HashSet<KindId> {
    ids.iter().cloned().collect()
}

// covers UP-25
#[tokio::test]
async fn watch_mode_transitions() {
    let fake = Fake::new();
    let hubs = HubRegistry::default();
    let mut w = watches(&fake, &hubs);
    let none = subs(&[]);
    let crds = (
        "customresourcedefinitions.apiextensions.k8s.io".to_owned(),
        Feed::Watch,
    );

    w.reconcile(true, WatchMode::All, &none);
    assert_eq!(
        running(&w),
        [
            ("configmaps".to_owned(), Feed::Watch),
            crds.clone(),
            ("pods".to_owned(), Feed::Watch),
        ],
        "All: every listable+watchable kind except duplicates; metrics are not watched"
    );

    w.reconcile(true, WatchMode::OnView, &none);
    assert_eq!(
        running(&w),
        std::slice::from_ref(&crds),
        "On view: only the CRD list for discovery"
    );

    let metrics = KindId::new("metrics.k8s.io", "pods");
    let open = subs(&[KindId::pods(), metrics.clone()]);
    w.reconcile(true, WatchMode::OnView, &open);
    assert_eq!(
        running(&w),
        [
            crds.clone(),
            ("pods".to_owned(), Feed::Watch),
            ("pods.metrics.k8s.io".to_owned(), Feed::Poll)
        ]
    );

    w.reconcile(true, WatchMode::All, &open);
    assert_eq!(
        running(&w).len(),
        4,
        "subscribed metrics stay polled in All mode"
    );

    // Closing the last view of a kind in On view mode stops it and drops its rows.
    w.reconcile(
        true,
        WatchMode::OnView,
        &subs(std::slice::from_ref(&metrics)),
    );
    assert!(!w.running().contains_key(&KindId::pods()));
    let pods_hub = hubs.get_or_create("c", &KindId::pods());
    assert_eq!(pods_hub.latest().state, SyncState::Idle);
    assert!(pods_hub.latest().rows.is_empty());

    // A subscription to a kind the cluster does not serve says so.
    let missing = KindId::new("example.com", "widgets");
    let (_hub, rx) = hubs.subscribe("c", &missing);
    w.reconcile(
        true,
        WatchMode::OnView,
        &subs(std::slice::from_ref(&missing)),
    );
    assert_eq!(rx.borrow().state, SyncState::Unavailable);
}

// covers UP-24
#[tokio::test]
async fn deactivating_stops_every_watch_and_reactivating_relists() {
    let fake = Fake::new();
    fake.list(
        "pods",
        pod_table(&[("u1", "demo", "a", "Running")], true, "10"),
    );
    let hubs = HubRegistry::default();
    let mut w = watches(&fake, &hubs);
    let (_hub, mut rx) = hubs.subscribe("c", &KindId::pods());
    let open = subs(&[KindId::pods()]);
    w.reconcile(true, WatchMode::All, &open);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.open_streams() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("pods, configmaps and CRDs are watched");

    w.reconcile(false, WatchMode::All, &open);
    assert!(w.running().is_empty());
    let snap = until(&mut rx, |s| s.state == SyncState::Idle).await;
    assert!(
        snap.rows.is_empty(),
        "no stale rows from an inactive cluster"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.open_streams() > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("every watch stream closed");

    w.reconcile(true, WatchMode::All, &open);
    until(&mut rx, |s| s.state == SyncState::Synced).await;
    assert_eq!(fake.list_calls("pods"), 2, "a fresh list on reactivation");

    // A changed connection (new client) also restarts everything.
    w.set_source(None);
    assert!(w.running().is_empty());
}
