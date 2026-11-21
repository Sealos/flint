//! One log session: a task that fills a [`LogBuffer`] from a pod's log stream, or from the
//! streams of every pod of a workload, and publishes coalesced [`LogSnapshot`]s (ADR-0002).
//!
//! What fills the buffer is a [`Feed`]: [`PodFeed`] drives one [`Stream`], and
//! [`WorkloadFeed`] a pod watch and one stream per pod container.
//! The session around it is the same for both: the buffer, pausing, and publishing.
//!
//! Lines are never sent one by one: the task publishes at most once per
//! [`SessionConfig::publish_every`], and only when something changed. A front end diffs two
//! snapshots by line id (see [`LogLines::end_id`]), so each snapshot works as a delta too.

use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use k8s_openapi::jiff::Timestamp;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::budget::LogBudget;
use super::buffer::{LogBuffer, LogLines};
use super::line::LogLine;
use super::pods::{PodSource, WorkloadTarget};
use super::source::LogSource;
use super::stream::{Backoff, Stream};
use super::workload::{PodSetStatus, WorkloadFeed};
use crate::redact;

/// Which container's logs to read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogTarget {
    pub namespace: String,
    pub pod: String,
    /// Required when the pod has more than one container; `None` lets the server pick the only
    /// one.
    pub container: Option<String>,
}

/// What a session reads: one pod's container, or the pods of a workload merged into one stream.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LogSubject {
    Pod(LogTarget),
    Workload(WorkloadTarget),
}

impl From<LogTarget> for LogSubject {
    fn from(target: LogTarget) -> Self {
        LogSubject::Pod(target)
    }
}

impl From<WorkloadTarget> for LogSubject {
    fn from(target: WorkloadTarget) -> Self {
        LogSubject::Workload(target)
    }
}

/// Where the logs start, in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogSince {
    /// The last this many seconds.
    Seconds(i64),
    /// From this time on.
    Time(Timestamp),
}

/// How a session reads logs. Changing any of it means opening a new session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogOptions {
    /// Keep the stream open and add lines as they are written.
    pub follow: bool,
    /// Start with at most this many of the newest lines (per container in a workload session).
    /// `None`, or more than the buffer holds, asks for as many as the buffer holds (shared out
    /// between the containers of a workload session).
    pub tail_lines: Option<i64>,
    pub since: Option<LogSince>,
    /// The previous (terminated) instance of the container. Never followed.
    pub previous: bool,
}

impl Default for LogOptions {
    /// Follow, starting with as many lines as the buffer holds.
    fn default() -> Self {
        LogOptions {
            follow: true,
            tail_lines: None,
            since: None,
            previous: false,
        }
    }
}

/// Why a container has no log to read yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerWait {
    /// The pod is not on a node yet.
    Unscheduled,
    /// The container has not started: the `reason` of its status, when that is a plain word
    /// (`ImagePullBackOff`, `ContainerCreating`).
    NotStarted(Option<String>),
}

/// The part of a log request that failed, when the error shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FailedHop {
    /// Not known, or the API server itself.
    #[default]
    Unknown,
    /// The API server answered, but could not reach the kubelet it proxies the log from.
    Node,
}

impl FailedHop {
    /// Reads the hop from an error's full `raw` text. The kubelet is recognised by its port or
    /// by the API server's `InternalError`; the failure itself must be in the words outside
    /// quoted values, so a quoted name cannot stand for one.
    pub(crate) fn of(raw: &str) -> Self {
        let lower = raw.to_lowercase();
        let names_kubelet = lower.starts_with("internalerror")
            || [":10250/", ":10250\""].iter().any(|p| lower.contains(p));
        let outside_quotes = redact::hide_server_values(raw).to_lowercase();
        if names_kubelet && NETWORK_FAILURES.iter().any(|w| outside_quotes.contains(w)) {
            FailedHop::Node
        } else {
            FailedHop::Unknown
        }
    }
}

/// Words of a failure to reach a server.
pub const NETWORK_FAILURES: &[&str] = &[
    "connection refused",
    "could not resolve",
    "tls handshake",
    "connection reset",
    "dropped",
    "unreachable",
    "timed out",
    "could not reach",
    "no route to host",
    "i/o timeout",
];

/// Where a session is.
///
/// In a workload session this is the state of the session as a whole (resolving the workload
/// and watching its pods); each stream's own state is in [`LogSnapshot::pods`]. A stream that
/// fails does not end the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogState {
    /// Opening the stream (or, for a workload, finding its pods) for the first time.
    Connecting,
    /// Reading (and, when following, waiting for new lines).
    Streaming,
    /// The stream (or a workload's pod watch) failed or ended while the container may log
    /// again; it is reopened after a backoff, without repeating lines already shown. A
    /// workload's streams keep running meanwhile. `error` is safe to show. `back_off`: the pod's
    /// status says the container is waiting in `CrashLoopBackOff`, so the wait is the kubelet's
    /// restart back-off rather than a broken connection.
    Reconnecting {
        error: Option<String>,
        /// Which part failed, read from the full error: `error` has its values hidden.
        hop: FailedHop,
        attempt: u32,
        back_off: bool,
    },
    /// The container has not started, so there is no log yet and nothing is wrong with the
    /// connection. Polled quietly until it starts.
    Waiting { wait: ContainerWait },
    /// Nothing more will come: all lines were read (not following, or the previous instance),
    /// the container finished, its pod was deleted, or an error retrying cannot fix. `error`
    /// is safe to show. `pod_deleted`: a single-pod session ended because its pod was deleted
    /// after everything it logged was read. That is an expected end, not a failure, and comes
    /// with no `error`.
    Ended {
        error: Option<String>,
        /// Which part failed, read from the full error: `error` has its values hidden.
        hop: FailedHop,
        pod_deleted: bool,
    },
}

/// How an ended session's `error` starts when the previous instance's log could not be read:
/// the kubelet answered with one line instead of the log ("unable to retrieve container logs
/// for …", "failed to try resolving symlinks in path …"), or the API server said the previous
/// terminated container was not found. The container may still be restarting, or may have been
/// removed (garbage-collected, or its node restarted). The server's words follow after `": "`.
pub const PREVIOUS_NOT_AVAILABLE: &str = "the previous instance's logs are not available";

impl LogState {
    /// The server's words, when the session ended because the previous instance's log is not
    /// available: see [`PREVIOUS_NOT_AVAILABLE`].
    pub fn previous_not_available(&self) -> Option<&str> {
        match self {
            LogState::Ended { error: Some(e), .. } => e
                .strip_prefix(PREVIOUS_NOT_AVAILABLE)
                .map(|rest| rest.trim_start_matches(": ")),
            _ => None,
        }
    }
}

/// What a front end shows of a session. Immutable and shared.
#[derive(Debug, Clone)]
pub struct LogSnapshot {
    pub lines: LogLines,
    pub state: LogState,
    /// Delivery is paused: `lines` stay as they were when the session was paused, while the
    /// buffer keeps filling.
    pub paused: bool,
    /// Lines that arrived after `lines` (while paused). They show on resume, unless evicted.
    pub held_back: u64,
    /// Grows with every snapshot of the session.
    pub version: u64,
    /// A workload session's pods and streams: how many are live, failed or capped. `None` for a
    /// pod session (and before a workload session first reports).
    pub pods: Option<Arc<PodSetStatus>>,
    /// Older lines were dropped to keep all log tabs within the shared memory budget
    /// (`log_total_mib`, [`super::LogBudget`]), not only by this session's own caps.
    pub trimmed: bool,
}

impl LogSnapshot {
    pub(crate) fn connecting() -> Self {
        LogSnapshot {
            lines: LogLines::default(),
            state: LogState::Connecting,
            paused: false,
            held_back: 0,
            version: 0,
            pods: None,
            trimmed: false,
        }
    }
}

/// Most streams a workload session reads at once. Containers beyond it wait in a queue and
/// start as others end; [`PodSetStatus::capped`] counts them.
pub const MAX_STREAMS: usize = 32;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionConfig {
    /// Lines the buffer keeps.
    pub limit: usize,
    /// Bytes the buffer's lines may take.
    pub budget: usize,
    /// At most one snapshot per this interval (ADR-0002's ~10 per second).
    pub publish_every: Duration,
    pub backoff: Backoff,
    /// Most streams a workload session reads at once.
    pub max_streams: usize,
}

impl SessionConfig {
    pub fn new(limit: usize, budget: usize) -> Self {
        SessionConfig {
            limit,
            budget,
            publish_every: Duration::from_millis(100),
            backoff: Backoff {
                first: Duration::from_secs(1),
                max: Duration::from_secs(30),
            },
            max_streams: MAX_STREAMS,
        }
    }

    /// The first request's `tail_lines` when `options` asks for `tail`: capped at the buffer.
    pub fn tail(&self, tail: Option<i64>) -> i64 {
        let limit = self.limit as i64;
        tail.map_or(limit, |t| t.clamp(0, limit))
    }
}

/// What fills a session's buffer.
pub(crate) trait Feed: Send {
    /// Makes progress on one outcome (a line, a state change, a pod event), writing to `out`.
    /// `Pending` when nothing is ready; the session publishes in between.
    fn poll(&mut self, cx: &mut Context<'_>, out: &mut Output) -> Poll<()>;
}

/// Builds the feed for `subject`.
pub(crate) fn feed(
    subject: LogSubject,
    options: LogOptions,
    config: &SessionConfig,
    logs: Arc<dyn LogSource>,
    pods: Arc<dyn PodSource>,
) -> Box<dyn Feed> {
    match subject {
        LogSubject::Pod(target) => Box::new(PodFeed::new(logs, target, options, config)),
        LogSubject::Workload(target) => {
            Box::new(WorkloadFeed::new(logs, pods, target, options, *config))
        }
    }
}

/// One pod's container.
pub(crate) struct PodFeed {
    stream: Stream,
}

impl PodFeed {
    pub fn new(
        source: Arc<dyn LogSource>,
        target: LogTarget,
        options: LogOptions,
        config: &SessionConfig,
    ) -> Self {
        let tail = config.tail(options.tail_lines);
        PodFeed {
            stream: Stream::new(source, target, options, tail, config.backoff, None),
        }
    }
}

impl Feed for PodFeed {
    fn poll(&mut self, cx: &mut Context<'_>, out: &mut Output) -> Poll<()> {
        let step = ready!(self.stream.poll_step(cx));
        self.stream.handle(step, &mut |line| out.append(line));
        out.set_state(self.stream.state());
        Poll::Ready(())
    }
}

/// The session's side of a feed: the buffer, pausing, and what to publish next.
pub(crate) struct Output {
    buffer: LogBuffer,
    state: LogState,
    pods: Option<Arc<PodSetStatus>>,
    /// The lines of the last snapshot published; pausing freezes these, so lines the front end
    /// never saw count as held back.
    published: LogLines,
    /// The lines shown while paused. Trimmed to the buffer's oldest line whenever the buffer
    /// drops a block, so a pause never keeps evicted blocks alive: a paused session stays within
    /// the byte budget like a running one (plus the frozen copy of the open tail block).
    frozen: Option<LogLines>,
    /// [`LogBuffer::dropped_blocks`] when `frozen` was last trimmed.
    frozen_dropped: u64,
    /// The shared budget made this session drop lines.
    trimmed: bool,
    version: u64,
    dirty: bool,
    /// A trim published its snapshot; the next tick still reports to the budget.
    report_due: bool,
}

impl Output {
    fn new(config: &SessionConfig) -> Self {
        Output {
            buffer: LogBuffer::with_budget(config.limit, config.budget),
            state: LogState::Connecting,
            pods: None,
            published: LogLines::default(),
            frozen: None,
            frozen_dropped: 0,
            trimmed: false,
            version: 0,
            dirty: true,
            report_due: false,
        }
    }

    pub fn append(&mut self, line: LogLine) {
        self.buffer.push(line);
        self.dirty = true;
        self.trim_frozen();
    }

    /// Gives memory back to the shared budget, if it asks: the oldest blocks, one at a time,
    /// for as long as the tab stays hidden and the plan stands. `None` when nothing was asked,
    /// else whether any line went. The lines are freed here, outside the budget's lock.
    fn trim_for(&mut self, budget: &LogBudget, id: u64) -> Option<bool> {
        let plan = budget.begin_trim(id)?;
        let mut detached = Vec::new();
        let buffer = &mut self.buffer;
        while let Some(lines) = budget.while_planned(id, plan, |cutoff| {
            let before = buffer.bytes();
            let lines = buffer.detach_oldest(plan.to)?;
            *cutoff = buffer.first_id();
            Some((lines, before - buffer.bytes()))
        }) {
            detached.push(lines);
        }
        if detached.is_empty() {
            return Some(false);
        }
        self.trimmed = true;
        self.dirty = true;
        self.trim_frozen();
        Some(true)
    }

    /// Keeps a paused view from holding blocks the buffer dropped.
    fn trim_frozen(&mut self) {
        if let Some(frozen) = &mut self.frozen
            && self.frozen_dropped != self.buffer.dropped_blocks()
        {
            *frozen = frozen.trimmed(self.buffer.first_id());
            self.frozen_dropped = self.buffer.dropped_blocks();
        }
    }

    pub fn set_state(&mut self, state: &LogState) {
        if self.state != *state {
            self.state = state.clone();
            self.dirty = true;
            // Nothing more will be appended: the open block can go back to the budget too.
            if matches!(state, LogState::Ended { .. }) {
                self.buffer.seal_tail();
            }
        }
    }

    pub fn set_pods(&mut self, pods: Arc<PodSetStatus>) {
        self.pods = Some(pods);
        self.dirty = true;
    }

    fn set_paused(&mut self, paused: bool) {
        if paused != self.frozen.is_some() {
            let first = self.buffer.first_id();
            self.frozen = paused.then(|| self.published.trimmed(first));
            self.frozen_dropped = self.buffer.dropped_blocks();
            self.dirty = true;
        }
    }

    fn publish(&mut self, tx: &watch::Sender<Arc<LogSnapshot>>) {
        self.version += 1;
        let lines = self.frozen.clone().unwrap_or_else(|| self.buffer.view());
        self.published = lines.clone();
        let end = self.buffer.last_id().map_or(0, |id| id + 1);
        tx.send_replace(Arc::new(LogSnapshot {
            held_back: end - lines.end_id(),
            lines,
            state: self.state.clone(),
            paused: self.frozen.is_some(),
            version: self.version,
            pods: self.pods.clone(),
            trimmed: self.trimmed,
        }));
        self.dirty = false;
    }
}

/// A session's place in the shared budget: registered while it runs.
pub(crate) struct Budgeted {
    budget: Arc<LogBudget>,
    id: u64,
    wake: Arc<tokio::sync::Notify>,
}

impl Budgeted {
    pub fn new(budget: Arc<LogBudget>, id: u64) -> Self {
        let wake = budget.register(id);
        Budgeted { budget, id, wake }
    }

    /// Where the session's trim cutoff is published, for its front end.
    pub fn cutoff(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.budget.cutoff_of(self.id)
    }
}

impl Drop for Budgeted {
    fn drop(&mut self) {
        self.budget.unregister(self.id);
    }
}

/// Runs a session until every receiver of `tx` is gone. `paused` pauses and resumes delivery.
/// With `budget`, the session reports its bytes when it publishes and drops its oldest lines
/// when the shared budget asks.
pub(crate) async fn run(
    mut feed: Box<dyn Feed>,
    config: SessionConfig,
    tx: watch::Sender<Arc<LogSnapshot>>,
    mut paused: watch::Receiver<bool>,
    budget: Option<Budgeted>,
) {
    let mut out = Output::new(&config);
    // `notify_one` keeps a permit while nobody waits, so a request made between two turns of
    // the loop is not lost.
    let wake = budget.as_ref().map(|b| b.wake.clone());
    let trim_asked = || async {
        match &wake {
            Some(wake) => wake.notified().await,
            None => std::future::pending().await,
        }
    };
    let mut tick = tokio::time::interval(config.publish_every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut pause_open = true;
    loop {
        tokio::select! {
            biased;
            () = tx.closed() => return,
            changed = paused.changed(), if pause_open => {
                if changed.is_err() {
                    // The session handle is gone; `tx.closed()` follows.
                    pause_open = false;
                    continue;
                }
                let paused = *paused.borrow_and_update();
                out.set_paused(paused);
                // Paused tabs are trimmed after idle ones: say so now, not with the next
                // snapshot.
                if let Some(b) = &budget {
                    b.budget.set_paused(b.id, paused);
                }
            }
            () = trim_asked() => {
                if let Some(b) = &budget
                    && let Some(dropped) = out.trim_for(&b.budget, b.id)
                {
                    b.budget.trimmed(b.id, out.buffer.bytes(), out.buffer.trimmable_bytes());
                    if dropped {
                        // At once, so a tab shown next has no lines that are gone. The next
                        // tick reports, and the budget plans again.
                        out.publish(&tx);
                        out.report_due = true;
                    }
                }
            }
            _ = tick.tick(), if out.dirty || out.report_due => {
                if out.dirty {
                    out.publish(&tx);
                }
                out.report_due = false;
                if let Some(b) = &budget {
                    b.budget.report(
                        b.id,
                        out.buffer.bytes(),
                        out.buffer.trimmable_bytes(),
                        out.frozen.is_some(),
                    );
                }
            }
            () = std::future::poll_fn(|cx| feed.poll(cx, &mut out)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::BLOCKS_PER_BUDGET;
    use crate::logs::source::{AfterEnd, LogError, LogRequest, RawLine, RawLines};
    use futures::future::BoxFuture;
    use futures::{FutureExt as _, StreamExt as _};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// One scripted outcome of `open`: an error, or lines then an end (EOF or an error).
    enum Script {
        Fail(LogError),
        Lines(Vec<String>, Option<LogError>),
        /// Lines, then the stream stays open.
        Hold(Vec<String>),
        /// Lines as the test sends them.
        Feed(futures::channel::mpsc::UnboundedReceiver<String>),
    }

    #[derive(Default)]
    struct Fake {
        scripts: Mutex<VecDeque<Script>>,
        requests: Mutex<Vec<LogRequest>>,
        after_end: Mutex<VecDeque<AfterEnd>>,
    }

    impl LogSource for Fake {
        fn open(&self, request: &LogRequest) -> BoxFuture<'static, Result<RawLines, LogError>> {
            self.requests.lock().unwrap().push(request.clone());
            let script = self.scripts.lock().unwrap().pop_front();
            let raw = |text: String| {
                Ok(RawLine {
                    text,
                    truncated: false,
                })
            };
            let result = match script {
                None => Err(LogError::fatal("no more scripts")),
                Some(Script::Fail(e)) => Err(e),
                Some(Script::Lines(lines, end)) => {
                    let mut items: Vec<_> = lines.into_iter().map(raw).collect();
                    items.extend(end.map(Err));
                    Ok(futures::stream::iter(items).boxed())
                }
                Some(Script::Feed(rx)) => Ok(rx.map(raw).boxed()),
                Some(Script::Hold(lines)) => {
                    let items: Vec<_> = lines.into_iter().map(raw).collect();
                    Ok(futures::stream::iter(items)
                        .chain(futures::stream::pending())
                        .boxed())
                }
            };
            async move { result }.boxed()
        }

        fn after_end(
            &self,
            _: &LogTarget,
            _: Option<&str>,
        ) -> BoxFuture<'static, Result<AfterEnd, LogError>> {
            let fate = self
                .after_end
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(AfterEnd::Finished);
            async move { Ok(fate) }.boxed()
        }
    }

    fn target() -> LogTarget {
        LogTarget {
            namespace: "ns".into(),
            pod: "p".into(),
            container: None,
        }
    }

    fn config(limit: usize) -> SessionConfig {
        SessionConfig {
            limit,
            budget: usize::MAX,
            publish_every: Duration::from_millis(100),
            backoff: Backoff {
                first: Duration::from_millis(500),
                max: Duration::from_secs(4),
            },
            max_streams: MAX_STREAMS,
        }
    }

    struct Running {
        rx: watch::Receiver<Arc<LogSnapshot>>,
        paused: watch::Sender<bool>,
        fake: Arc<Fake>,
    }

    /// Runs a pod session on `fake`.
    fn spawn(
        fake: Arc<Fake>,
        options: LogOptions,
        config: SessionConfig,
        tx: watch::Sender<Arc<LogSnapshot>>,
        paused: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let feed = PodFeed::new(fake, target(), options, &config);
        tokio::spawn(run(Box::new(feed), config, tx, paused, None))
    }

    fn start(scripts: Vec<Script>, options: LogOptions, limit: usize) -> Running {
        let fake = Arc::new(Fake::default());
        fake.scripts.lock().unwrap().extend(scripts);
        let (tx, rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (paused, paused_rx) = watch::channel(false);
        spawn(fake.clone(), options, config(limit), tx, paused_rx);
        Running { rx, paused, fake }
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
            result.expect("the session reached the expected state")
        }

        fn requests(&self) -> Vec<LogRequest> {
            self.fake.requests.lock().unwrap().clone()
        }
    }

    fn texts(snapshot: &LogSnapshot) -> Vec<String> {
        snapshot
            .lines
            .iter()
            .map(|(_, l)| l.text.to_string())
            .collect()
    }

    fn owned<const N: usize>(lines: [&str; N]) -> Vec<String> {
        lines.map(str::to_owned).into()
    }

    fn ended(s: &LogSnapshot) -> bool {
        matches!(s.state, LogState::Ended { .. })
    }

    const T1: &str = "2024-05-01T12:00:01.100Z";

    // covers UP-40, UP-42: open tabs share one memory budget; the least recently viewed gives
    // lines back first, the shown one keeps its own cap, and streams keep running (#254).
    #[tokio::test(start_paused = true)]
    async fn sessions_share_one_budget_least_recently_viewed_first() {
        const PER_SESSION: usize = 256 * 1024;
        // Two full tabs, and room for the open tail blocks the least recently viewed tab
        // cannot give back, so it alone covers the excess.
        const TOTAL: usize = 2 * PER_SESSION + 2 * (PER_SESSION / crate::logs::BLOCKS_PER_BUDGET);
        let budget = LogBudget::new(TOTAL);
        // Three tabs, opened in order; the third is shown.
        let mut feeds = Vec::new();
        let mut tabs = Vec::new();
        for id in 1..=3u64 {
            let (lines, rx) = futures::channel::mpsc::unbounded();
            let fake = Arc::new(Fake::default());
            fake.scripts.lock().unwrap().push_back(Script::Feed(rx));
            let (tx, snapshots) = watch::channel(Arc::new(LogSnapshot::connecting()));
            let (_paused, paused_rx) = watch::channel(false);
            let config = SessionConfig {
                budget: PER_SESSION,
                ..config(1_000_000)
            };
            let feed = PodFeed::new(fake, target(), LogOptions::default(), &config);
            let budgeted = Budgeted::new(budget.clone(), id);
            tokio::spawn(run(Box::new(feed), config, tx, paused_rx, Some(budgeted)));
            feeds.push(lines);
            tabs.push((snapshots, _paused));
        }
        // Tab 1 was viewed before tab 2; tab 3 is shown now.
        for id in [1, 2] {
            budget.set_shown(id, true);
            budget.set_shown(id, false);
        }
        budget.set_shown(3, true);

        // Each tab fills to its own cap in turn, the shown one last: 3 × 256 KiB of lines
        // against about 512 KiB for all, so only the last one's growth goes over.
        let line = "x".repeat(200);
        for feed in &feeds {
            for n in 0..3_000 {
                feed.unbounded_send(format!("{n} {line}")).unwrap();
                if n % 100 == 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let latest = |tab: &mut (watch::Receiver<Arc<LogSnapshot>>, watch::Sender<bool>)| {
            tab.0.borrow_and_update().clone()
        };
        let snaps: Vec<Arc<LogSnapshot>> = tabs.iter_mut().map(latest).collect();
        // What the tabs really hold, from their lines: within the budget, give or take an open
        // tail block per tab. A full tab sits a few blocks under its own cap, which evicts whole
        // blocks.
        let held = |s: &LogSnapshot| -> usize { s.lines.iter().map(|(_, l)| l.heap_bytes()).sum() };
        let block = PER_SESSION / crate::logs::BLOCKS_PER_BUDGET;
        let total: usize = snaps.iter().map(|s| held(s)).sum();
        assert!(
            total <= TOTAL + 3 * block,
            "{total} bytes held, budget {TOTAL}"
        );
        assert!(
            snaps[0].trimmed,
            "the least recently viewed tab gave lines back"
        );
        // Tab 1 gave back what was needed; tab 2, viewed more recently, nothing.
        assert!(!snaps[1].trimmed, "tab 2 was not needed");
        assert!(
            held(&snaps[1]) + 4 * block >= PER_SESSION,
            "tab 2 holds {} of {PER_SESSION}",
            held(&snaps[1])
        );
        assert!(!snaps[2].trimmed, "the shown tab keeps its own cap");
        assert!(
            held(&snaps[2]) + 4 * block >= PER_SESSION,
            "the shown tab is full: {} of {PER_SESSION}",
            held(&snaps[2])
        );
        assert!(
            snaps[0].lines.len() < snaps[1].lines.len(),
            "tab 1 ({}) was trimmed before tab 2 ({})",
            snaps[0].lines.len(),
            snaps[1].lines.len()
        );
        assert!(snaps[2].lines.len() >= snaps[1].lines.len());

        // A trimmed tab still streams.
        feeds[0]
            .unbounded_send("after the trim".to_owned())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tab1 = latest(&mut tabs[0]);
        assert_eq!(
            tab1.lines
                .iter()
                .last()
                .map(|(_, l)| l.text.to_string())
                .as_deref(),
            Some("after the trim")
        );
        assert!(tab1.trimmed);
    }

    // A trim that is the last change to a tab publishes once, and still reports to the budget.
    #[tokio::test(start_paused = true)]
    async fn a_trim_that_is_the_last_change_publishes_once() {
        let budget = LogBudget::new(1);
        let (lines, rx) = futures::channel::mpsc::unbounded();
        let fake = Arc::new(Fake::default());
        fake.scripts.lock().unwrap().push_back(Script::Feed(rx));
        let (tx, mut snapshots) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (_paused, paused_rx) = watch::channel(false);
        let config = SessionConfig {
            budget: 1 << 30,
            ..config(1_000_000)
        };
        let feed = PodFeed::new(fake, target(), LogOptions::default(), &config);
        let budgeted = Budgeted::new(budget.clone(), 1);
        let cutoff = budgeted.cutoff();
        tokio::spawn(run(Box::new(feed), config, tx, paused_rx, Some(budgeted)));
        for n in 0..crate::logs::CHUNK_LINES * 2 + 5 {
            lines.unbounded_send(format!("{n}")).unwrap();
        }
        let trimmed = snapshots.wait_for(|s| s.trimmed).await.unwrap().clone();
        assert_eq!(
            cutoff.load(std::sync::atomic::Ordering::Acquire),
            trimmed.lines.first_id()
        );
        let reports = budget.reports();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(snapshots.borrow().version, trimmed.version);
        assert_eq!(budget.reports(), reports + 1, "the next tick reports once");
    }

    fn output_with(lines: usize) -> Output {
        // A byte budget large enough that blocks seal by their line count.
        let mut out = Output::new(&SessionConfig::new(1_000_000, 1 << 30));
        for n in 0..lines {
            out.append(LogLine::parse(&format!("line {n}"), false));
        }
        out
    }

    // covers UP-42: a trim taken just before the tab is shown takes nothing from it.
    #[test]
    fn a_trim_taken_before_the_tab_is_shown_leaves_its_lines() {
        let budget = LogBudget::new(1);
        budget.register(1);
        let mut out = output_with(crate::logs::CHUNK_LINES * 3);
        let held = out.buffer.len();
        budget.report(1, out.buffer.bytes(), out.buffer.trimmable_bytes(), false);
        let plan = budget.begin_trim(1).expect("over the budget");
        budget.set_shown(1, true);
        assert_eq!(
            budget.while_planned(1, plan, |_| {
                out.buffer.detach_oldest(plan.to).map(|lines| (lines, 0))
            }),
            None
        );
        assert_eq!(out.buffer.len(), held);
        assert!(!out.trimmed);
    }

    // covers UP-42: a hidden tab gives its sealed blocks back, and says so.
    #[test]
    fn a_hidden_tab_gives_back_its_sealed_blocks() {
        let budget = LogBudget::new(1);
        budget.register(1);
        let mut out = output_with(crate::logs::CHUNK_LINES * 2 + 5);
        budget.report(1, out.buffer.bytes(), out.buffer.trimmable_bytes(), false);
        assert_eq!(out.trim_for(&budget, 1), Some(true));
        assert_eq!(out.buffer.len(), 5, "the open block stays");
        assert!(out.trimmed);
        assert_eq!(out.trim_for(&budget, 1), None, "asked once");
    }

    // covers UP-42: an ended stream never appends again, so its last block can go back too;
    // one still running keeps its open block.
    #[test]
    fn an_ended_stream_can_give_back_its_last_block() {
        let mut running = output_with(5);
        running.set_state(&LogState::Streaming);
        assert_eq!(running.buffer.trimmable_bytes(), 0);

        let mut ended = output_with(5);
        ended.set_state(&LogState::Ended {
            hop: Default::default(),
            error: None,
            pod_deleted: false,
        });
        assert_eq!(ended.buffer.trimmable_bytes(), ended.buffer.bytes());
        assert_eq!(ended.buffer.len(), 5, "sealing keeps every line");

        let budget = LogBudget::new(0);
        budget.register(1);
        budget.report(
            1,
            ended.buffer.bytes(),
            ended.buffer.trimmable_bytes(),
            false,
        );
        assert_eq!(ended.trim_for(&budget, 1), Some(true));
        assert!(ended.buffer.is_empty());
        assert!(ended.trimmed);
    }

    // covers UP-42: an ended tab that is shown keeps its whole log, however far over the budget.
    #[test]
    fn an_ended_tab_that_is_shown_keeps_its_log() {
        let mut ended = output_with(5);
        ended.set_state(&LogState::Ended {
            hop: Default::default(),
            error: None,
            pod_deleted: true,
        });
        let budget = LogBudget::new(0);
        budget.register(1);
        budget.set_shown(1, true);
        budget.report(
            1,
            ended.buffer.bytes(),
            ended.buffer.trimmable_bytes(),
            false,
        );
        assert_eq!(ended.trim_for(&budget, 1), None);
        assert_eq!(ended.buffer.len(), 5);
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn reads_everything_once_and_ends_when_not_following() {
        let options = LogOptions {
            follow: false,
            tail_lines: Some(500),
            ..LogOptions::default()
        };
        let lines = owned([
            "2024-05-01T12:00:00Z ERROR boom",
            "2024-05-01T12:00:01Z WARN slow",
        ]);
        let mut run = start(vec![Script::Lines(lines, None)], options, 100);
        let snapshot = run.until(ended).await;
        assert_eq!(texts(&snapshot), ["ERROR boom", "WARN slow"]);
        assert_eq!(
            snapshot.state,
            LogState::Ended {
                hop: Default::default(),
                error: None,
                pod_deleted: false
            }
        );
        assert!(snapshot.pods.is_none(), "a pod session has no pod set");
        let severities: Vec<_> = snapshot.lines.iter().map(|(_, l)| l.severity).collect();
        assert_eq!(
            severities,
            [
                super::super::Severity::Error,
                super::super::Severity::Warning
            ]
        );
        let requests = run.requests();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].follow);
        assert_eq!(requests[0].tail_lines, Some(100), "capped at the buffer");
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn many_lines_arrive_in_few_snapshots_and_stay_bounded() {
        let lines = (0..5000).map(|n| format!("{T1} line {n}")).collect();
        let mut run = start(vec![Script::Hold(lines)], LogOptions::default(), 1000);
        let mut versions = 0;
        let snapshot = loop {
            let s = run.until(|_| true).await;
            versions += 1;
            if s.lines.end_id() == 5000 {
                break s;
            }
            run.rx.changed().await.unwrap();
        };
        assert!(versions < 10, "coalesced into {versions} snapshots");
        assert_eq!(snapshot.lines.len(), 1000);
        assert_eq!(snapshot.lines.first_id(), 4000);
        assert_eq!(snapshot.state, LogState::Streaming);
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn reconnects_with_backoff_without_repeating_lines() {
        let first = owned([
            "2024-05-01T12:00:00.500Z a",
            "2024-05-01T12:00:01.100Z b",
            "2024-05-01T12:00:01.100Z c",
        ]);
        // From `since_time` rounded down to the second: a repeat of b and c, then new lines,
        // one of which shares their timestamp.
        let second = owned([
            "2024-05-01T12:00:01.000Z old",
            "2024-05-01T12:00:01.100Z b",
            "2024-05-01T12:00:01.100Z c",
            "2024-05-01T12:00:01.100Z d",
            "2024-05-01T12:00:02Z e",
        ]);
        let mut run = start(
            vec![
                Script::Lines(first, Some(LogError::retry("connection reset"))),
                Script::Fail(LogError::retry("still down")),
                Script::Hold(second),
            ],
            LogOptions::default(),
            100,
        );
        let broken = run
            .until(|s| matches!(s.state, LogState::Reconnecting { attempt: 2, .. }))
            .await;
        assert_eq!(
            broken.state,
            LogState::Reconnecting {
                hop: Default::default(),
                error: Some("still down".into()),
                attempt: 2,
                back_off: false,
            }
        );
        let snapshot = run.until(|s| s.lines.end_id() == 5).await;
        assert_eq!(texts(&snapshot), ["a", "b", "c", "d", "e"]);
        assert_eq!(snapshot.state, LogState::Streaming);
        let requests = run.requests();
        assert_eq!(requests.len(), 3);
        let resume = &requests[2];
        assert_eq!(
            resume.since,
            Some(LogSince::Time("2024-05-01T12:00:01.100Z".parse().unwrap()))
        );
        assert_eq!(resume.tail_lines, None);
        assert!(resume.follow);
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn a_tail_that_cut_a_timestamp_group_resumes_after_what_was_shown() {
        const T: &str = "2024-05-01T12:00:01.100Z";
        let mut run = start(
            vec![
                // tail_lines = 1 cut A and B, which share C's timestamp.
                Script::Lines(vec![format!("{T} C")], Some(LogError::retry("reset"))),
                Script::Hold(vec![
                    format!("{T} A"),
                    format!("{T} B"),
                    format!("{T} C"),
                    format!("{T} D"),
                    "2024-05-01T12:00:02Z E".to_owned(),
                ]),
            ],
            LogOptions {
                tail_lines: Some(1),
                ..LogOptions::default()
            },
            100,
        );
        let snapshot = run.until(|s| s.lines.len() == 3).await;
        assert_eq!(texts(&snapshot), ["C", "D", "E"]);
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn a_finished_container_is_drained_once_before_the_session_ends() {
        let mut run = start(
            vec![
                Script::Lines(owned(["2024-05-01T12:00:00Z run 1"]), None),
                // It restarted and finished again before the status check.
                Script::Lines(
                    owned(["2024-05-01T12:00:00Z run 1", "2024-05-01T12:00:03Z run 2"]),
                    None,
                ),
            ],
            LogOptions::default(),
            100,
        );
        run.fake
            .after_end
            .lock()
            .unwrap()
            .push_back(AfterEnd::Finished);
        let snapshot = run.until(ended).await;
        assert_eq!(
            snapshot.state,
            LogState::Ended {
                hop: Default::default(),
                error: None,
                pod_deleted: false
            }
        );
        assert_eq!(texts(&snapshot), ["run 1", "run 2"]);
        let requests = run.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].follow);
        assert!(!requests[1].follow, "the catch-up does not follow");
        assert!(requests[1].since.is_some());
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn pausing_freezes_what_was_last_published() {
        let (feed, lines) = futures::channel::mpsc::unbounded();
        let mut run = start(vec![Script::Feed(lines)], LogOptions::default(), 100);
        feed.unbounded_send("one".to_owned()).unwrap();
        feed.unbounded_send("two".to_owned()).unwrap();
        run.until(|s| s.lines.len() == 2).await;
        // "three" reaches the buffer but no snapshot before the pause.
        feed.unbounded_send("three".to_owned()).unwrap();
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        run.paused.send(true).unwrap();
        let paused = run.until(|s| s.paused).await;
        assert_eq!(texts(&paused), ["one", "two"]);
        assert_eq!(paused.held_back, 1);
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn a_followed_stream_that_ends_reconnects_only_while_the_container_can_log() {
        let mut run = start(
            vec![
                Script::Lines(owned(["2024-05-01T12:00:00Z run 1"]), None),
                Script::Lines(owned(["2024-05-01T12:00:05Z run 2"]), None),
            ],
            LogOptions::default(),
            100,
        );
        run.fake
            .after_end
            .lock()
            .unwrap()
            .extend([AfterEnd::Restarts, AfterEnd::Gone]);
        let snapshot = run.until(ended).await;
        assert_eq!(texts(&snapshot), ["run 1", "run 2"]);
        assert_eq!(
            snapshot.state,
            LogState::Ended {
                hop: Default::default(),
                error: None,
                pod_deleted: true,
            }
        );
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn fatal_errors_end_the_session_and_previous_is_never_followed() {
        let mut run = start(
            vec![Script::Fail(LogError::fatal(
                "NotFound: pods \"p\" not found",
            ))],
            LogOptions::default(),
            100,
        );
        let snapshot = run.until(ended).await;
        assert_eq!(
            snapshot.state,
            LogState::Ended {
                hop: Default::default(),
                error: Some("NotFound: pods \"p\" not found".into()),
                pod_deleted: false,
            }
        );

        let options = LogOptions {
            previous: true,
            ..LogOptions::default()
        };
        let mut run = start(
            vec![Script::Lines(owned(["last words"]), None)],
            options,
            100,
        );
        let snapshot = run.until(ended).await;
        assert_eq!(
            snapshot.state,
            LogState::Ended {
                hop: Default::default(),
                error: None,
                pod_deleted: false
            }
        );
        let request = &run.requests()[0];
        assert!(request.previous && !request.follow);
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn pausing_freezes_the_lines_and_counts_what_is_held_back() {
        let (feed, lines) = futures::channel::mpsc::unbounded();
        let mut run = start(vec![Script::Feed(lines)], LogOptions::default(), 100);
        feed.unbounded_send("one".to_owned()).unwrap();
        feed.unbounded_send("two".to_owned()).unwrap();
        run.until(|s| s.lines.len() == 2).await;
        run.paused.send(true).unwrap();
        run.until(|s| s.paused).await;
        feed.unbounded_send("three".to_owned()).unwrap();
        let paused = run.until(|s| s.held_back == 1).await;
        assert!(paused.paused);
        assert_eq!(texts(&paused), ["one", "two"]);
        run.paused.send(false).unwrap();
        let resumed = run.until(|s| !s.paused).await;
        assert_eq!(texts(&resumed), ["one", "two", "three"]);
        assert_eq!(resumed.held_back, 0);
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn a_long_pause_stays_within_the_byte_budget() {
        let (feed, lines) = futures::channel::mpsc::unbounded();
        let budget = 2 * 1024 * 1024;
        let fake = Arc::new(Fake::default());
        fake.scripts.lock().unwrap().push_back(Script::Feed(lines));
        let (tx, rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (paused, paused_rx) = watch::channel(false);
        let config = SessionConfig {
            budget,
            ..config(100_000)
        };
        spawn(fake.clone(), LogOptions::default(), config, tx, paused_rx);
        let mut run = Running { rx, paused, fake };
        let long = "z".repeat(32 * 1024);
        for _ in 0..40 {
            feed.unbounded_send(long.clone()).unwrap();
        }
        run.until(|s| s.lines.end_id() == 40).await;
        run.paused.send(true).unwrap();
        run.until(|s| s.paused).await;
        // Five budgets' worth arrives while paused.
        for _ in 0..(5 * budget / long.len()) {
            feed.unbounded_send(long.clone()).unwrap();
        }
        let held = run.until(|s| s.held_back > 250).await;
        // The frozen view lost its oldest lines with the buffer instead of keeping them alive.
        assert!(held.paused);
        assert!(held.lines.first_id() > 0);
        let frozen_bytes: usize = held.lines.iter().map(|(_, l)| l.heap_bytes()).sum();
        assert!(frozen_bytes <= budget, "{frozen_bytes}");
        assert!(
            held.lines.blocks() <= BLOCKS_PER_BUDGET + 1,
            "{}",
            held.lines.blocks()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_task_ends_when_the_session_is_dropped() {
        let fake = Arc::new(Fake::default());
        fake.scripts
            .lock()
            .unwrap()
            .push_back(Script::Hold(owned(["x"])));
        let (tx, rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (paused, paused_rx) = watch::channel(false);
        let task = spawn(fake, LogOptions::default(), config(10), tx, paused_rx);
        drop((rx, paused));
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("ended")
            .unwrap();
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn replayed_lines_do_not_reset_the_backoff() {
        let last = "2024-05-01T12:00:00Z crashed";
        let mut run = start(
            vec![
                Script::Lines(owned([last]), None),
                Script::Lines(owned([last]), None),
                Script::Lines(owned([last]), None),
            ],
            LogOptions::default(),
            100,
        );
        run.fake.after_end.lock().unwrap().extend([
            AfterEnd::Restarts,
            AfterEnd::Restarts,
            AfterEnd::Restarts,
        ]);
        let snapshot = run
            .until(|s| matches!(s.state, LogState::Reconnecting { attempt: 3, .. }))
            .await;
        assert_eq!(texts(&snapshot), ["crashed"]);
    }
}
