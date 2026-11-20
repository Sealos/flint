//! A workload session's feed: resolves the workload to a label selector, watches the matching
//! pods, and runs one [`Stream`] per pod container, starting and stopping streams as pods come,
//! start, restart and go. Every stream writes into the session's one buffer, so the line limit
//! and byte budget hold for the session as a whole.
//!
//! **Ordering.** Lines interleave in arrival order: each stream's lines stay in order, and
//! lines of different streams are appended as they are read. There is no global sort by
//! timestamp. A reorder window would delay every line by the window, and a line that arrives
//! later than the window (a slow node, a reconnect that replays from `since_time`) would still
//! land out of order, while line ids must be assigned once and never change. The initial tails of
//! several pods therefore show as one block per stream. Timestamps are always requested, so
//! each line still shows when it was written.
//!
//! **Limits.** At most [`SessionConfig::max_streams`] streams read at once; further containers
//! wait in a queue (first come, first served) and start as others end. Without a `tail_lines`,
//! the buffer's line limit is shared out between the containers found when the session starts.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use tokio::time::Sleep;

use super::line::LineSource;
use super::pods::{ContainerInfo, PodEvent, PodEvents, PodInfo, PodSet, PodSource, WorkloadTarget};
use super::session::{Feed, LogOptions, LogState, LogTarget, Output, SessionConfig};
use super::source::{LogError, LogSource};
use super::stream::{Member, Stream};

/// What a workload session reads: its pods and the state of each stream. Rebuilt only when
/// something in it changes, not for every line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PodSetStatus {
    /// The label selector the pods are matched by, once known.
    pub selector: Option<String>,
    /// Pods matching it.
    pub pods: usize,
    /// One entry per stream, ordered by pod name and container (spec order). Streams of pods
    /// that are gone are dropped once they end.
    pub streams: Vec<StreamStatus>,
    /// Selected containers that have not started yet (following only).
    pub waiting: usize,
    /// Containers not read because [`PodSetStatus::max_streams`] streams are already running.
    /// They start as other streams end.
    pub capped: usize,
    /// Most streams read at once.
    pub max_streams: usize,
}

/// One pod container's stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStatus {
    pub source: Arc<LineSource>,
    /// `Streaming` while reading, `Reconnecting` while waiting to reopen (the container may be
    /// restarting), `Ended` when done (with an error when it failed).
    pub state: LogState,
}

impl PodSetStatus {
    /// Streams reading now.
    pub fn live(&self) -> usize {
        self.count(|s| matches!(s, LogState::Streaming))
    }

    /// Streams opening or waiting to reopen.
    pub fn connecting(&self) -> usize {
        self.count(|s| matches!(s, LogState::Connecting | LogState::Reconnecting { .. }))
    }

    /// Streams that ended on an error.
    pub fn failed(&self) -> usize {
        self.count(|s| matches!(s, LogState::Ended { error: Some(_), .. }))
    }

    /// Streams that ended without one (the container finished, or everything was read).
    pub fn ended(&self) -> usize {
        self.count(|s| matches!(s, LogState::Ended { error: None, .. }))
    }

    fn count(&self, pred: impl Fn(&LogState) -> bool) -> usize {
        self.streams.iter().filter(|s| pred(&s.state)).count()
    }
}

enum Watch {
    Resolve(BoxFuture<'static, Result<String, LogError>>),
    Open(BoxFuture<'static, Result<PodEvents, LogError>>),
    Events(PodEvents),
    Wait(Pin<Box<Sleep>>),
    Done,
}

enum WatchStep {
    Resolved(Result<String, LogError>),
    Opened(Result<PodEvents, LogError>),
    Event(Option<Result<PodEvent, LogError>>),
    Retry,
}

impl Watch {
    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<WatchStep> {
        match self {
            Watch::Resolve(f) => f.poll_unpin(cx).map(WatchStep::Resolved),
            Watch::Open(f) => f.poll_unpin(cx).map(WatchStep::Opened),
            Watch::Events(events) => events.poll_next_unpin(cx).map(WatchStep::Event),
            Watch::Wait(sleep) => sleep.as_mut().poll(cx).map(|()| WatchStep::Retry),
            Watch::Done => Poll::Pending,
        }
    }
}

/// A pod container, by pod uid so a pod recreated under the same name (a StatefulSet's) gets a
/// new stream.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    pod: String,
    uid: String,
    container: String,
}

struct Entry {
    key: Key,
    stream: Stream,
}

/// Most outcomes taken from one ready stream in a row.
const STEP_BATCH: usize = 256;

/// How often a following session checks its context still points at the server it was opened
/// on, while the pod watch stays open (see [`PodSource::check`]).
pub(crate) const RECHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) struct WorkloadFeed {
    logs: Arc<dyn LogSource>,
    pods: Arc<dyn PodSource>,
    target: WorkloadTarget,
    options: LogOptions,
    config: SessionConfig,
    watch: Watch,
    selector: Option<String>,
    /// Failed watch attempts in a row, for the backoff. Reset by a complete list.
    attempts: u32,
    state: LogState,
    /// The pods of a (re)list in progress, between `Init` and `InitDone`.
    listing: Option<Vec<PodInfo>>,
    /// The first list is complete.
    synced: bool,
    /// `tail_lines` of each stream's first request; set by the first list.
    tail: i64,
    known: BTreeMap<String, PodInfo>,
    streams: Vec<Entry>,
    queue: VecDeque<Key>,
    /// Where the next round-robin poll of the streams starts, so a chatty stream cannot starve
    /// the others.
    cursor: usize,
    status_dirty: bool,
    /// When to check the context next, and the check in flight.
    recheck: Pin<Box<Sleep>>,
    checking: Option<BoxFuture<'static, Result<(), LogError>>>,
}

impl WorkloadFeed {
    pub fn new(
        logs: Arc<dyn LogSource>,
        pods: Arc<dyn PodSource>,
        target: WorkloadTarget,
        options: LogOptions,
        config: SessionConfig,
    ) -> Self {
        let mut feed = WorkloadFeed {
            logs,
            pods,
            target,
            options,
            config,
            watch: Watch::Done,
            selector: None,
            attempts: 0,
            state: LogState::Connecting,
            listing: None,
            synced: false,
            tail: config.tail(None),
            known: BTreeMap::new(),
            streams: Vec::new(),
            queue: VecDeque::new(),
            cursor: 0,
            status_dirty: true,
            recheck: Box::pin(tokio::time::sleep(RECHECK_EVERY)),
            checking: None,
        };
        match &feed.target.pods {
            PodSet::Selector(selector) if selector.trim().is_empty() => {
                feed.fail("an empty label selector would follow every pod in the namespace".into());
            }
            PodSet::Selector(selector) => {
                feed.selector = Some(selector.trim().to_owned());
                feed.watch = feed.open_watch();
            }
            PodSet::Workload { kind, name } => {
                feed.watch =
                    Watch::Resolve(feed.pods.selector(&feed.target.namespace, *kind, name));
            }
        }
        feed
    }

    /// Following new lines (and so new pods).
    fn following(&self) -> bool {
        self.options.follow && !self.options.previous
    }

    /// Whether `container` has a log this session can read.
    fn readable(&self, container: &ContainerInfo) -> bool {
        if self.options.previous {
            container.has_previous
        } else {
            container.started
        }
    }

    fn open_watch(&self) -> Watch {
        let selector = self.selector.as_deref().unwrap_or_default();
        Watch::Open(self.pods.watch(&self.target.namespace, selector))
    }

    fn live(&self) -> usize {
        self.streams.iter().filter(|e| !e.stream.is_done()).count()
    }

    fn handle_watch(&mut self, step: WatchStep) {
        match step {
            WatchStep::Resolved(Ok(selector)) => {
                self.selector = Some(selector);
                self.status_dirty = true;
                self.watch = self.open_watch();
            }
            WatchStep::Opened(Ok(events)) => self.watch = Watch::Events(events),
            WatchStep::Resolved(Err(e))
            | WatchStep::Opened(Err(e))
            | WatchStep::Event(Some(Err(e))) => {
                if e.is_fatal() {
                    self.fail(e.message);
                } else {
                    self.retry(Some(e));
                }
            }
            // kube's watcher never ends; a fake or a future one might.
            WatchStep::Event(None) => self.retry(None),
            WatchStep::Event(Some(Ok(event))) => self.apply_event(event),
            WatchStep::Retry => {
                self.watch = match (&self.selector, &self.target.pods) {
                    (Some(_), _) => self.open_watch(),
                    (None, PodSet::Workload { kind, name }) => {
                        Watch::Resolve(self.pods.selector(&self.target.namespace, *kind, name))
                    }
                    (None, PodSet::Selector(_)) => unreachable!("a selector is known at once"),
                };
            }
        }
    }

    /// The pod watch broke: reopen it after a backoff. Streams keep running meanwhile.
    fn retry(&mut self, error: Option<LogError>) {
        self.attempts += 1;
        let delay = self.config.backoff.delay(self.attempts);
        if let Some(e) = &error {
            tracing::debug!(error = %e.message, ?delay, "pod watch retry");
        }
        self.state = LogState::Reconnecting {
            error: error.map(|e| e.message),
            attempt: self.attempts,
        };
        self.watch = Watch::Wait(Box::pin(tokio::time::sleep(delay)));
    }

    /// Ends the session: an error retrying cannot fix (the workload does not exist, access is
    /// denied, the context now points elsewhere). Every stream stops; the lines stay.
    fn fail(&mut self, error: String) {
        self.state = LogState::Ended {
            error: Some(error),
            pod_deleted: false,
        };
        self.watch = Watch::Done;
        self.streams.clear();
        self.queue.clear();
        self.known.clear();
        self.status_dirty = true;
    }

    fn apply_event(&mut self, event: PodEvent) {
        match event {
            PodEvent::Init => self.listing = Some(Vec::new()),
            PodEvent::InitApply(pod) => match &mut self.listing {
                Some(listing) => listing.push(pod),
                None => self.apply_pod(pod),
            },
            PodEvent::InitDone => self.listed(),
            PodEvent::Apply(pod) if !self.synced => {
                self.listing.get_or_insert_with(Vec::new).push(pod);
            }
            PodEvent::Apply(pod) => self.apply_pod(pod),
            PodEvent::Delete(pod) => {
                if self.known.get(&pod.name).is_some_and(|p| p.uid == pod.uid) {
                    self.remove_pod(&pod.name, true);
                }
            }
        }
    }

    /// A (re)list is complete: pods missing from it are gone, the others are (re)applied.
    fn listed(&mut self) {
        let mut listed = self.listing.take().unwrap_or_default();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        let names: HashSet<&str> = listed.iter().map(|p| p.name.as_str()).collect();
        let gone: Vec<String> = self
            .known
            .keys()
            .filter(|name| !names.contains(name.as_str()))
            .cloned()
            .collect();
        for name in gone {
            self.remove_pod(&name, true);
        }
        if !self.synced {
            // Share the buffer out between the containers there are now (also those beyond the
            // stream cap, which read later), so together they ask for no more than it holds.
            let readable = listed
                .iter()
                .flat_map(|p| p.selected(self.target.container.as_deref()))
                .filter(|c| self.readable(c))
                .count()
                .max(1);
            self.tail = match self.options.tail_lines {
                Some(_) => self.config.tail(self.options.tail_lines),
                None => (self.config.tail(None) / readable as i64).max(1),
            };
            self.synced = true;
        }
        for pod in listed {
            self.apply_pod(pod);
        }
        self.attempts = 0;
        self.state = LogState::Streaming;
        self.status_dirty = true;
        if !self.following() {
            // One list is all a session that does not follow reads.
            self.watch = Watch::Done;
        }
    }

    fn apply_pod(&mut self, pod: PodInfo) {
        if self.known.get(&pod.name).is_some_and(|p| p.uid != pod.uid) {
            // Replaced under the same name: the old pod is long gone, and reading on by name
            // would read the new one.
            self.remove_pod(&pod.name, false);
        }
        let before = self.known.insert(pod.name.clone(), pod.clone());
        let container = self.target.container.clone();
        let following = self.following();
        for c in pod.selected(container.as_deref()) {
            let key = Key {
                pod: pod.name.clone(),
                uid: pod.uid.clone(),
                container: c.name.clone(),
            };
            if let Some(entry) = self.streams.iter_mut().find(|e| e.key == key) {
                // Still draining after the pod left the pod set, and now it matches again: keep
                // following it. (A gone stream that ended was dropped at once, so a pod that
                // comes back later gets a new stream below.)
                entry.stream.pod_back();
                let was = before
                    .as_ref()
                    .and_then(|p| p.containers.iter().find(|o| o.name == c.name));
                let restarted = was.is_some_and(|w| w.restarts != c.restarts);
                if !c.running || !following {
                    continue;
                }
                if !entry.stream.is_done() {
                    if restarted || was.is_none_or(|w| !w.running) {
                        // Restarted: reopen now instead of at the end of the backoff.
                        entry.stream.nudge();
                    }
                } else if restarted {
                    // It ended (failed, or finished) and the container ran again: read on, as
                    // soon as there is room.
                    self.admit(key);
                }
                continue;
            }
            if self.readable(c) {
                self.admit(key);
            }
        }
        self.status_dirty = true;
    }

    /// Starts reading `key`: a new stream, or an ended one revived. At the cap, it waits in the
    /// queue instead; the one queue serves both, so a revival is never lost or over the cap.
    fn admit(&mut self, key: Key) {
        if self.queue.contains(&key) {
            return;
        }
        if self.live() >= self.config.max_streams {
            self.queue.push_back(key);
            self.status_dirty = true;
            return;
        }
        if let Some(entry) = self.streams.iter_mut().find(|e| e.key == key) {
            entry.stream.revive();
            self.status_dirty = true;
            return;
        }
        let target = LogTarget {
            namespace: self.target.namespace.clone(),
            pod: key.pod.clone(),
            container: Some(key.container.clone()),
        };
        let member = Member {
            source: Arc::new(LineSource::new(&key.pod, &key.container)),
            uid: key.uid.clone(),
        };
        let stream = Stream::new(
            self.logs.clone(),
            target,
            self.options.clone(),
            self.tail,
            self.config.backoff,
            Some(member),
        );
        self.streams.push(Entry { key, stream });
        self.status_dirty = true;
    }

    /// The pod is gone. With `grace`, its streams may finish reading what the server still
    /// sends (see [`Stream::pod_gone`]).
    fn remove_pod(&mut self, name: &str, grace: bool) {
        self.known.remove(name);
        self.queue.retain(|k| k.pod != name);
        for entry in self.streams.iter_mut().filter(|e| e.key.pod == name) {
            entry.stream.pod_gone(grace);
        }
        self.after_stream_end();
    }

    /// Drops ended streams of pods that are gone, and starts queued containers in their place.
    fn after_stream_end(&mut self) {
        let known = &self.known;
        self.streams.retain(|e| {
            !e.stream.is_done() || known.get(&e.key.pod).is_some_and(|p| p.uid == e.key.uid)
        });
        while self.live() < self.config.max_streams {
            let Some(key) = self.queue.pop_front() else {
                break;
            };
            self.admit(key);
        }
        self.status_dirty = true;
    }

    /// A session that does not follow ends once every stream did.
    fn check_end(&mut self) {
        if !self.following()
            && self.synced
            && self.queue.is_empty()
            && self.streams.iter().all(|e| e.stream.is_done())
            && !matches!(self.state, LogState::Ended { .. })
        {
            self.state = LogState::Ended {
                error: None,
                pod_deleted: false,
            };
            self.status_dirty = true;
        }
    }

    fn status(&self) -> PodSetStatus {
        let mut streams: Vec<(usize, StreamStatus)> = self
            .streams
            .iter()
            .map(|e| {
                let order = self
                    .known
                    .get(&e.key.pod)
                    .and_then(|p| p.containers.iter().position(|c| c.name == e.key.container))
                    .unwrap_or(usize::MAX);
                let status = StreamStatus {
                    source: e
                        .stream
                        .tag()
                        .cloned()
                        .expect("workload streams are tagged"),
                    state: e.stream.state().clone(),
                };
                (order, status)
            })
            .collect();
        streams.sort_by(|(oa, a), (ob, b)| a.source.pod().cmp(b.source.pod()).then(oa.cmp(ob)));
        let waiting = if self.following() {
            self.known
                .values()
                .flat_map(|p| {
                    p.selected(self.target.container.as_deref())
                        .map(move |c| (p, c))
                })
                .filter(|(p, c)| {
                    !self.readable(c)
                        && !self
                            .streams
                            .iter()
                            .any(|e| e.key.uid == p.uid && e.key.container == c.name)
                })
                .count()
        } else {
            0
        };
        PodSetStatus {
            selector: self.selector.clone(),
            pods: self.known.len(),
            streams: streams.into_iter().map(|(_, s)| s).collect(),
            waiting,
            capped: self.queue.len(),
            max_streams: self.config.max_streams,
        }
    }

    fn finish(&mut self, out: &mut Output) {
        self.check_end();
        out.set_state(&self.state);
        if self.status_dirty {
            self.status_dirty = false;
            out.set_pods(Arc::new(self.status()));
        }
    }
}

impl WorkloadFeed {
    /// Checks the context every [`RECHECK_EVERY`] while the pod watch is open. Ready with the
    /// error that ends the session, or `None` when a check finished without one.
    fn poll_recheck(&mut self, cx: &mut Context<'_>) -> Poll<Option<String>> {
        if !matches!(self.watch, Watch::Events(_)) {
            return Poll::Pending;
        }
        if self.checking.is_none() {
            std::task::ready!(self.recheck.as_mut().poll(cx));
            let next = tokio::time::Instant::now() + RECHECK_EVERY;
            self.recheck.as_mut().reset(next);
            self.checking = Some(self.pods.check());
        }
        let checking = self.checking.as_mut().expect("set above");
        let result = std::task::ready!(checking.poll_unpin(cx));
        self.checking = None;
        // Only a cluster error is about the session; a cluster that is reconnecting is the
        // streams' and the watch's business.
        Poll::Ready(result.err().filter(LogError::is_cluster).map(|e| e.message))
    }
}

impl Feed for WorkloadFeed {
    fn poll(&mut self, cx: &mut Context<'_>, out: &mut Output) -> Poll<()> {
        if let Poll::Ready(error) = self.poll_recheck(cx) {
            if let Some(error) = error {
                self.fail(error);
            }
            self.finish(out);
            return Poll::Ready(());
        }
        if let Poll::Ready(step) = self.watch.poll_step(cx) {
            self.handle_watch(step);
            self.finish(out);
            return Poll::Ready(());
        }
        let count = self.streams.len();
        for i in 0..count {
            let at = (self.cursor + i) % count;
            let stream = &mut self.streams[at].stream;
            let Poll::Ready(step) = stream.poll_step(cx) else {
                continue;
            };
            // Take what this stream has ready (up to a batch) before polling the others again,
            // so a chatty stream costs one pass over the idle ones per batch, not per line.
            let mut changed = stream.handle(step, &mut |line| out.append(line));
            for _ in 1..STEP_BATCH {
                if changed {
                    break;
                }
                let Poll::Ready(step) = stream.poll_step(cx) else {
                    break;
                };
                changed = stream.handle(step, &mut |line| out.append(line));
            }
            let cluster_error = match stream.state() {
                LogState::Ended { error, .. } if stream.ended_on_cluster_error() => error.clone(),
                _ => None,
            };
            let done = stream.is_done();
            self.cursor = (at + 1) % count;
            self.status_dirty |= changed;
            if let Some(error) = cluster_error {
                self.fail(error);
            } else if done {
                self.after_stream_end();
            }
            self.finish(out);
            return Poll::Ready(());
        }
        if self.status_dirty {
            // Something changed without a step (the first report): let the session publish it.
            self.finish(out);
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::session::{LogSnapshot, run};
    use crate::logs::source::{AfterEnd, LogRequest, RawLine, RawLines};
    use crate::logs::stream::{Backoff, GONE_GRACE};
    use crate::logs::{LogLine, WorkloadKind};
    use futures::channel::mpsc;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::watch;

    /// Pod watches the test drives: each `watch` call takes the next channel.
    #[derive(Default)]
    struct FakePods {
        selector: Mutex<Option<Result<String, LogError>>>,
        watches: Mutex<VecDeque<mpsc::UnboundedReceiver<Result<PodEvent, LogError>>>>,
        selectors: Mutex<Vec<String>>,
        /// What `check` answers (`None`: fine), and how often it was asked.
        check: Mutex<Option<LogError>>,
        checks: Mutex<u32>,
    }

    impl PodSource for FakePods {
        fn selector(
            &self,
            _: &str,
            _: WorkloadKind,
            _: &str,
        ) -> BoxFuture<'static, Result<String, LogError>> {
            let result = self
                .selector
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok("app=web".into()));
            async move { result }.boxed()
        }

        fn watch(
            &self,
            _: &str,
            selector: &str,
        ) -> BoxFuture<'static, Result<PodEvents, LogError>> {
            self.selectors.lock().unwrap().push(selector.to_owned());
            let rx = self.watches.lock().unwrap().pop_front();
            async move {
                match rx {
                    Some(rx) => Ok(rx.boxed()),
                    None => Ok(futures::stream::pending().boxed()),
                }
            }
            .boxed()
        }

        fn check(&self) -> BoxFuture<'static, Result<(), LogError>> {
            let result = self.check.lock().unwrap().clone().map_or(Ok(()), Err);
            *self.checks.lock().unwrap() += 1;
            async move { result }.boxed()
        }
    }

    /// Log streams the test feeds by `pod/container`; each open replaces the previous feed.
    #[derive(Default)]
    struct FakeLogs {
        feeds: Mutex<HashMap<String, mpsc::UnboundedSender<Result<RawLine, LogError>>>>,
        requests: Mutex<Vec<LogRequest>>,
        /// Opening these fails with the error.
        fail: Mutex<HashMap<String, LogError>>,
        /// Closed by the test: the container finished, so a catch-up read finds nothing more.
        closed: Mutex<Vec<String>>,
    }

    impl FakeLogs {
        fn send(&self, label: &str, text: &str) {
            let feeds = self.feeds.lock().unwrap();
            let feed = feeds
                .get(label)
                .unwrap_or_else(|| panic!("{label} is not open"));
            feed.unbounded_send(Ok(RawLine {
                text: text.to_owned(),
                truncated: false,
            }))
            .unwrap();
        }

        fn close(&self, label: &str) {
            self.feeds.lock().unwrap().remove(label);
            self.closed.lock().unwrap().push(label.to_owned());
        }

        fn opened(&self) -> Vec<String> {
            let requests = self.requests.lock().unwrap();
            requests
                .iter()
                .map(|r| {
                    format!(
                        "{}/{}",
                        r.target.pod,
                        r.target.container.as_deref().unwrap()
                    )
                })
                .collect()
        }
    }

    impl LogSource for FakeLogs {
        fn open(&self, request: &LogRequest) -> BoxFuture<'static, Result<RawLines, LogError>> {
            self.requests.lock().unwrap().push(request.clone());
            let label = format!(
                "{}/{}",
                request.target.pod,
                request.target.container.as_deref().unwrap()
            );
            let closed = self.closed.lock().unwrap().contains(&label);
            let result = match self.fail.lock().unwrap().get(&label) {
                Some(e) => Err(e.clone()),
                None if closed => Ok(futures::stream::empty().boxed()),
                None => {
                    let (tx, rx) = mpsc::unbounded();
                    self.feeds.lock().unwrap().insert(label, tx);
                    Ok(rx.boxed())
                }
            };
            async move { result }.boxed()
        }

        fn after_end(
            &self,
            _: &LogTarget,
            _: Option<&str>,
        ) -> BoxFuture<'static, Result<AfterEnd, LogError>> {
            async { Ok(AfterEnd::Finished) }.boxed()
        }
    }

    fn pod(name: &str, uid: &str, containers: &[(&str, bool)]) -> PodInfo {
        PodInfo {
            name: name.into(),
            uid: uid.into(),
            containers: containers
                .iter()
                .map(|(c, running)| ContainerInfo {
                    name: (*c).into(),
                    role: super::super::pods::ContainerRole::Regular,
                    started: *running,
                    running: *running,
                    restarts: 0,
                    has_previous: false,
                })
                .collect(),
        }
    }

    struct Running {
        rx: watch::Receiver<Arc<LogSnapshot>>,
        _paused: watch::Sender<bool>,
        pods: Arc<FakePods>,
        logs: Arc<FakeLogs>,
        watch: mpsc::UnboundedSender<Result<PodEvent, LogError>>,
    }

    fn config(limit: usize, max_streams: usize) -> SessionConfig {
        SessionConfig {
            limit,
            budget: usize::MAX,
            publish_every: Duration::from_millis(100),
            backoff: Backoff {
                first: Duration::from_millis(500),
                max: Duration::from_secs(4),
            },
            max_streams,
        }
    }

    fn target(pods: PodSet, container: Option<&str>) -> WorkloadTarget {
        WorkloadTarget {
            namespace: "ns".into(),
            pods,
            container: container.map(str::to_owned),
        }
    }

    fn deployment() -> PodSet {
        PodSet::Workload {
            kind: WorkloadKind::Deployment,
            name: "web".into(),
        }
    }

    fn start_with(target: WorkloadTarget, options: LogOptions, config: SessionConfig) -> Running {
        let pods = Arc::new(FakePods::default());
        let logs = Arc::new(FakeLogs::default());
        let (watch, events) = mpsc::unbounded();
        pods.watches.lock().unwrap().push_back(events);
        let (tx, rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (paused, paused_rx) = watch::channel(false);
        let feed = WorkloadFeed::new(logs.clone(), pods.clone(), target, options, config);
        tokio::spawn(run(Box::new(feed), config, tx, paused_rx));
        Running {
            rx,
            _paused: paused,
            pods,
            logs,
            watch,
        }
    }

    fn start(max_streams: usize) -> Running {
        start_with(
            target(deployment(), None),
            LogOptions::default(),
            config(1000, max_streams),
        )
    }

    impl Running {
        async fn until(&mut self, pred: impl Fn(&LogSnapshot) -> bool) -> Arc<LogSnapshot> {
            let result = tokio::time::timeout(Duration::from_secs(600), async {
                loop {
                    let snapshot = self.rx.borrow_and_update().clone();
                    if pred(&snapshot) {
                        return snapshot;
                    }
                    self.rx.changed().await.expect("session running");
                }
            })
            .await;
            result.unwrap_or_else(|_| {
                let s = self.rx.borrow();
                panic!("not reached: {:?} {:?}", s.state, s.pods)
            })
        }

        fn event(&self, event: PodEvent) {
            self.watch.unbounded_send(Ok(event)).unwrap();
        }

        fn list(&self, pods: Vec<PodInfo>) {
            self.event(PodEvent::Init);
            for p in pods {
                self.event(PodEvent::InitApply(p));
            }
            self.event(PodEvent::InitDone);
        }

        /// Waits until `label`'s stream is open.
        async fn opened(&mut self, label: &str) {
            let label = label.to_owned();
            let logs = self.logs.clone();
            self.until(move |_| logs.feeds.lock().unwrap().contains_key(&label))
                .await;
        }
    }

    fn texts(snapshot: &LogSnapshot) -> Vec<String> {
        snapshot.lines.iter().map(|(_, l)| shown(l)).collect()
    }

    fn shown(line: &LogLine) -> String {
        line.display(false)
    }

    fn pods_of(s: &LogSnapshot) -> Arc<PodSetStatus> {
        s.pods.clone().unwrap_or_default()
    }

    fn labels(status: &PodSetStatus) -> Vec<&str> {
        status.streams.iter().map(|s| s.source.label()).collect()
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn merges_every_pod_container_and_follows_pods_as_they_come_and_go() {
        let mut run = start(32);
        run.list(vec![
            pod("web-a", "a", &[("app", true), ("proxy", true)]),
            pod("web-b", "b", &[("app", true), ("proxy", true)]),
        ]);
        for label in ["web-a/app", "web-a/proxy", "web-b/app", "web-b/proxy"] {
            run.opened(label).await;
        }
        run.logs.send("web-a/app", "hello from a");
        run.logs.send("web-b/proxy", "hello from b");
        let s = run.until(|s| s.lines.len() == 2).await;
        let mut lines = texts(&s);
        lines.sort();
        assert_eq!(
            lines,
            ["[web-a/app] hello from a", "[web-b/proxy] hello from b"]
        );
        assert_eq!(s.state, LogState::Streaming);
        assert!(s.pods.is_some(), "a workload session reports its pods");
        let status = pods_of(&s);
        assert_eq!(status.selector.as_deref(), Some("app=web"));
        assert_eq!(status.pods, 2);
        assert_eq!(status.live(), 4);
        assert_eq!(
            labels(&status),
            ["web-a/app", "web-a/proxy", "web-b/app", "web-b/proxy"]
        );
        assert_eq!(*run.pods.selectors.lock().unwrap(), ["app=web"]);

        // Scale up: a new pod appears, not started yet, then running.
        run.event(PodEvent::Apply(pod(
            "web-c",
            "c",
            &[("app", false), ("proxy", false)],
        )));
        let s = run.until(|s| pods_of(s).pods == 3).await;
        assert_eq!(pods_of(&s).waiting, 2);
        run.event(PodEvent::Apply(pod(
            "web-c",
            "c",
            &[("app", true), ("proxy", true)],
        )));
        run.opened("web-c/app").await;
        run.logs.send("web-c/app", "hello from c");
        let s = run.until(|s| s.lines.len() == 3).await;
        assert_eq!(texts(&s)[2], "[web-c/app] hello from c");
        assert_eq!(pods_of(&s).waiting, 0);
        assert_eq!(pods_of(&s).live(), 6);

        // Delete a pod: its streams end and drop out, the rest go on.
        run.logs.close("web-a/app");
        run.logs.close("web-a/proxy");
        run.event(PodEvent::Delete(pod("web-a", "a", &[])));
        let s = run.until(|s| pods_of(s).streams.len() == 4).await;
        assert_eq!(pods_of(&s).pods, 2);
        assert!(!labels(&pods_of(&s)).iter().any(|l| l.starts_with("web-a")));
        assert_eq!(s.state, LogState::Streaming);

        // Its replacement is followed.
        run.event(PodEvent::Apply(pod("web-d", "d", &[("app", true)])));
        run.opened("web-d/app").await;
        run.logs.send("web-d/app", "hello from d");
        let s = run.until(|s| s.lines.len() == 4).await;
        assert_eq!(texts(&s)[3], "[web-d/app] hello from d");
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_pod_recreated_under_its_name_gets_a_new_stream() {
        let mut run = start(32);
        run.list(vec![pod("db-0", "old", &[("db", true)])]);
        run.opened("db-0/db").await;
        // The StatefulSet replaced it: same name, new uid. The old stream ends (the fake's feed
        // closes when the new one opens) and only the new one is left.
        run.event(PodEvent::Apply(pod("db-0", "new", &[("db", true)])));
        let logs = run.logs.clone();
        run.until(move |_| logs.opened().len() == 2).await;
        let s = run
            .until(|s| pods_of(s).streams.len() == 1 && pods_of(s).live() == 1)
            .await;
        assert_eq!(pods_of(&s).pods, 1);
        run.logs.send("db-0/db", "new instance");
        let s = run.until(|s| s.lines.len() == 1).await;
        assert_eq!(texts(&s), ["[db-0/db] new instance"]);
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn streams_beyond_the_cap_wait_their_turn() {
        let mut run = start(2);
        run.list(vec![
            pod("web-a", "a", &[("app", true)]),
            pod("web-b", "b", &[("app", true)]),
            pod("web-c", "c", &[("app", true)]),
        ]);
        let s = run.until(|s| pods_of(s).capped == 1).await;
        let status = pods_of(&s);
        assert_eq!(
            (status.pods, status.streams.len(), status.max_streams),
            (3, 2, 2)
        );
        assert_eq!(labels(&status), ["web-a/app", "web-b/app"]);

        // A stream ends for good (the fake's containers finish): the queued one starts.
        run.opened("web-a/app").await;
        run.logs.close("web-a/app");
        let s = run.until(|s| pods_of(s).capped == 0).await;
        assert!(labels(&pods_of(&s)).contains(&"web-c/app"));
        assert_eq!(pods_of(&s).ended(), 1);
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_failing_stream_does_not_end_the_session() {
        let mut run = start(32);
        run.logs.fail.lock().unwrap().insert(
            "web-b/app".into(),
            LogError::fatal("Forbidden: pods \"web-b\" is forbidden"),
        );
        run.list(vec![
            pod("web-a", "a", &[("app", true)]),
            pod("web-b", "b", &[("app", true)]),
        ]);
        run.opened("web-a/app").await;
        let s = run.until(|s| pods_of(s).failed() == 1).await;
        assert_eq!(s.state, LogState::Streaming);
        let status = pods_of(&s);
        assert_eq!((status.live(), status.failed()), (1, 1));
        assert_eq!(
            status.streams[1].state,
            LogState::Ended {
                error: Some("Forbidden: pods \"web-b\" is forbidden".into()),
                pod_deleted: false,
            }
        );
        run.logs.send("web-a/app", "still here");
        run.until(|s| s.lines.len() == 1).await;
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_broken_pod_watch_reconnects_and_relists_while_streams_go_on() {
        let mut run = start(32);
        let (second, events) = mpsc::unbounded();
        run.pods.watches.lock().unwrap().push_back(events);
        run.list(vec![
            pod("web-a", "a", &[("app", true)]),
            pod("web-b", "b", &[("app", true)]),
        ]);
        run.opened("web-a/app").await;
        run.opened("web-b/app").await;
        run.watch
            .unbounded_send(Err(LogError::retry("watch reset")))
            .unwrap();
        let s = run
            .until(|s| matches!(s.state, LogState::Reconnecting { .. }))
            .await;
        assert_eq!(
            s.state,
            LogState::Reconnecting {
                error: Some("watch reset".into()),
                attempt: 1
            }
        );
        run.logs.send("web-a/app", "during the outage");
        run.until(|s| s.lines.len() == 1).await;

        // web-b went away during the outage: the re-list drops it (its stream after the grace
        // period, since the fake never closes it).
        second.unbounded_send(Ok(PodEvent::Init)).unwrap();
        second
            .unbounded_send(Ok(PodEvent::InitApply(pod("web-a", "a", &[("app", true)]))))
            .unwrap();
        second.unbounded_send(Ok(PodEvent::InitDone)).unwrap();
        let s = run
            .until(|s| s.state == LogState::Streaming && pods_of(s).streams.len() == 1)
            .await;
        assert_eq!(pods_of(&s).pods, 1);
        assert_eq!(labels(&pods_of(&s)), ["web-a/app"]);
        assert_eq!(
            run.logs.opened(),
            ["web-a/app", "web-b/app"],
            "web-a kept its stream"
        );
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn errors_retrying_cannot_fix_end_the_session() {
        let pods = Arc::new(FakePods::default());
        *pods.selector.lock().unwrap() = Some(Err(LogError::fatal(
            "NotFound: deployments.apps \"web\" not found",
        )));
        let (tx, mut rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (_paused, paused_rx) = watch::channel(false);
        let cfg = config(100, 32);
        let feed = WorkloadFeed::new(
            Arc::new(FakeLogs::default()),
            pods,
            target(deployment(), None),
            LogOptions::default(),
            cfg,
        );
        tokio::spawn(run(Box::new(feed), cfg, tx, paused_rx));
        let s = loop {
            let s = rx.borrow_and_update().clone();
            if matches!(s.state, LogState::Ended { .. }) {
                break s;
            }
            rx.changed().await.unwrap();
        };
        assert_eq!(
            s.state,
            LogState::Ended {
                error: Some("NotFound: deployments.apps \"web\" not found".into()),
                pod_deleted: false,
            }
        );

        // An empty selector is refused without asking the server.
        let mut run = start_with(
            target(PodSet::Selector("  ".into()), None),
            LogOptions::default(),
            config(100, 32),
        );
        let s = run
            .until(|s| matches!(s.state, LogState::Ended { .. }))
            .await;
        assert!(matches!(&s.state, LogState::Ended { error: Some(e), .. } if e.contains("empty")));
        assert!(run.pods.selectors.lock().unwrap().is_empty());
    }

    // covers UP-45, UP-06
    #[tokio::test(start_paused = true)]
    async fn an_open_pod_watch_still_notices_a_re_pointed_context() {
        let mut run = start(32);
        run.list(vec![pod("web-a", "a", &[("app", true)])]);
        run.opened("web-a/app").await;
        // Checks pass while the context is unchanged; a cluster that is only reconnecting does
        // not end anything.
        // A passing check publishes nothing, so wait on the clock rather than on snapshots.
        let checks = |run: &Running| *run.pods.checks.lock().unwrap();
        while checks(&run) < 2 {
            tokio::time::sleep(RECHECK_EVERY).await;
        }
        *run.pods.check.lock().unwrap() = Some(LogError::retry("connecting to \"dev\""));
        while checks(&run) < 3 {
            tokio::time::sleep(RECHECK_EVERY).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(run.rx.borrow().state, LogState::Streaming);

        // Re-pointed while the old server still answers: the watch never errors, the check ends
        // the session within one interval.
        *run.pods.check.lock().unwrap() = Some(LogError::cluster(
            "\"dev\" now points to a different API server",
        ));
        let s = run
            .until(|s| matches!(s.state, LogState::Ended { .. }))
            .await;
        assert_eq!(
            s.state,
            LogState::Ended {
                error: Some("\"dev\" now points to a different API server".into()),
                pod_deleted: false,
            }
        );
        assert!(pods_of(&s).streams.is_empty());
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_re_pointed_context_ends_the_whole_session() {
        let mut run = start(32);
        run.logs.fail.lock().unwrap().insert(
            "web-b/app".into(),
            LogError::cluster("\"dev\" now points to a different API server"),
        );
        run.list(vec![
            pod("web-a", "a", &[("app", true)]),
            pod("web-b", "b", &[("app", true)]),
        ]);
        let s = run
            .until(|s| matches!(s.state, LogState::Ended { .. }))
            .await;
        assert_eq!(
            s.state,
            LogState::Ended {
                error: Some("\"dev\" now points to a different API server".into()),
                pod_deleted: false,
            }
        );
        assert_eq!(pods_of(&s).streams.len(), 0, "every stream stopped");
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn without_following_it_reads_each_container_once_then_ends() {
        let options = LogOptions {
            follow: false,
            ..LogOptions::default()
        };
        let mut run = start_with(
            target(PodSet::Selector("app=web".into()), Some("app")),
            options,
            config(100, 32),
        );
        run.list(vec![
            pod("web-a", "a", &[("app", true), ("proxy", true)]),
            pod("web-b", "b", &[("app", true)]),
            pod("web-c", "c", &[("app", false)]),
        ]);
        run.opened("web-a/app").await;
        run.opened("web-b/app").await;
        run.logs.send("web-a/app", "a");
        run.logs.close("web-a/app");
        run.logs.send("web-b/app", "b");
        run.logs.close("web-b/app");
        let s = run
            .until(|s| matches!(s.state, LogState::Ended { .. }))
            .await;
        assert_eq!(
            s.state,
            LogState::Ended {
                error: None,
                pod_deleted: false
            }
        );
        let mut lines = texts(&s);
        lines.sort();
        assert_eq!(lines, ["[web-a/app] a", "[web-b/app] b"]);
        let requests = run.logs.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "only the named container, only started ones"
        );
        assert!(requests.iter().all(|r| !r.follow));
        // The buffer is shared out between the two containers.
        assert!(requests.iter().all(|r| r.tail_lines == Some(50)));
        assert_eq!(*run.pods.selectors.lock().unwrap(), ["app=web"]);
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn an_explicit_tail_applies_to_each_container() {
        let options = LogOptions {
            tail_lines: Some(10),
            ..LogOptions::default()
        };
        let mut run = start_with(target(deployment(), None), options, config(100, 32));
        run.list(vec![pod("web-a", "a", &[("app", true), ("proxy", true)])]);
        run.opened("web-a/proxy").await;
        let requests = run.logs.requests.lock().unwrap().clone();
        assert!(
            requests
                .iter()
                .all(|r| r.tail_lines == Some(10) && r.follow)
        );
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn the_buffer_is_shared_out_between_capped_containers_too() {
        let mut run = start_with(
            target(deployment(), None),
            LogOptions::default(),
            config(100, 2),
        );
        run.list(vec![
            pod("web-a", "a", &[("app", true), ("proxy", true)]),
            pod("web-b", "b", &[("app", true), ("proxy", true)]),
        ]);
        let s = run.until(|s| pods_of(s).capped == 2).await;
        assert_eq!(pods_of(&s).streams.len(), 2);
        let requests = run.logs.requests.lock().unwrap().clone();
        assert!(
            requests.iter().all(|r| r.tail_lines == Some(25)),
            "{requests:?}"
        );
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_restart_at_the_cap_waits_in_the_queue_for_a_slot() {
        let mut run = start(1);
        run.logs.fail.lock().unwrap().insert(
            "web-a/app".into(),
            LogError::fatal("BadRequest: container \"app\" is terminated"),
        );
        run.list(vec![
            pod("web-a", "a", &[("app", true)]),
            pod("web-b", "b", &[("app", true)]),
        ]);
        // web-a failed, which let web-b take the one slot.
        run.opened("web-b/app").await;
        run.until(|s| pods_of(s).failed() == 1 && pods_of(s).live() == 1)
            .await;

        // web-a restarts while the slot is taken: it queues instead of going over the cap.
        run.logs.fail.lock().unwrap().clear();
        let mut restarted = pod("web-a", "a", &[("app", true)]);
        restarted.containers[0].restarts = 1;
        run.event(PodEvent::Apply(restarted));
        let s = run.until(|s| pods_of(s).capped == 1).await;
        assert_eq!(pods_of(&s).live(), 1);
        assert_eq!(run.logs.opened(), ["web-a/app", "web-b/app"]);

        // web-b finishes: web-a reads on in its slot.
        run.logs.close("web-b/app");
        run.opened("web-a/app").await;
        let s = run
            .until(|s| pods_of(s).capped == 0 && pods_of(s).live() == 1)
            .await;
        assert_eq!(pods_of(&s).failed(), 0);
        run.logs.send("web-a/app", "back");
        let s = run.until(|s| s.lines.len() == 1).await;
        assert_eq!(texts(&s), ["[web-a/app] back"]);
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_pod_that_matches_again_while_draining_is_still_followed() {
        let mut run = start(32);
        run.list(vec![pod("web-a", "a", &[("app", true)])]);
        run.opened("web-a/app").await;
        // Its labels change: the watch reports it gone, and its stream drains. Before the grace
        // period is over the labels change back: the same pod (same uid) matches again.
        run.event(PodEvent::Delete(pod("web-a", "a", &[])));
        run.until(|s| pods_of(s).pods == 0).await;
        run.event(PodEvent::Apply(pod("web-a", "a", &[("app", true)])));
        run.until(|s| pods_of(s).pods == 1).await;

        // Past the grace period the stream still reads, on the same request.
        tokio::time::sleep(GONE_GRACE + Duration::from_secs(1)).await;
        let s = run.rx.borrow().clone();
        assert_eq!(labels(&pods_of(&s)), ["web-a/app"]);
        assert_eq!(pods_of(&s).live(), 1, "{:?}", pods_of(&s));
        run.logs.send("web-a/app", "still matching");
        let s = run.until(|s| s.lines.len() == 1).await;
        assert_eq!(texts(&s), ["[web-a/app] still matching"]);
        assert_eq!(run.logs.opened(), ["web-a/app"]);
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_failed_stream_reads_on_when_its_container_restarts() {
        let mut run = start(32);
        run.logs.fail.lock().unwrap().insert(
            "web-a/app".into(),
            LogError::fatal("BadRequest: container \"app\" is terminated"),
        );
        run.list(vec![pod("web-a", "a", &[("app", true)])]);
        run.until(|s| pods_of(s).failed() == 1).await;

        // The same status again revives nothing; a restart does.
        run.logs.fail.lock().unwrap().clear();
        run.event(PodEvent::Apply(pod("web-a", "a", &[("app", true)])));
        let mut restarted = pod("web-a", "a", &[("app", true)]);
        restarted.containers[0].restarts = 1;
        run.event(PodEvent::Apply(restarted));
        run.opened("web-a/app").await;
        let s = run.until(|s| pods_of(s).live() == 1).await;
        assert_eq!(pods_of(&s).failed(), 0);
        assert_eq!(run.logs.opened(), ["web-a/app", "web-a/app"]);
        run.logs.send("web-a/app", "back");
        run.until(|s| s.lines.len() == 1).await;
    }
}
