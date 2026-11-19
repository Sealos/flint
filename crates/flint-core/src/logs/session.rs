//! One log session: a task that reads a pod's log stream into a [`LogBuffer`], reconnects with
//! backoff when the stream drops, and publishes coalesced [`LogSnapshot`]s (ADR-0002).
//!
//! Lines are never sent one by one: the task publishes at most once per
//! [`SessionConfig::publish_every`], and only when something changed. A front end diffs two
//! snapshots by line id (see [`LogLines::end_id`]), so each snapshot works as a delta too.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use k8s_openapi::jiff::Timestamp;
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, Sleep};

use super::buffer::{LogBuffer, LogLines};
use super::line::LogLine;
use super::source::{AfterEnd, LogError, LogRequest, LogSource, RawLine, RawLines};

/// Which container's logs to read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogTarget {
    pub namespace: String,
    pub pod: String,
    /// Required when the pod has more than one container; `None` lets the server pick the only
    /// one.
    pub container: Option<String>,
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
    /// Start with at most this many of the newest lines. `None`, or more than the buffer holds,
    /// asks for as many as the buffer holds.
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

/// Where a session is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogState {
    /// Opening the stream for the first time.
    Connecting,
    /// Reading (and, when following, waiting for new lines).
    Streaming,
    /// The stream failed or ended while the container may log again; it is reopened after a
    /// backoff, without repeating lines already shown. `error` is safe to show.
    Reconnecting { error: Option<String>, attempt: u32 },
    /// Nothing more will come: all lines were read (not following, or the previous instance),
    /// the container finished, or an error retrying cannot fix. `error` is safe to show.
    Ended { error: Option<String> },
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
}

impl LogSnapshot {
    pub(crate) fn connecting() -> Self {
        LogSnapshot {
            lines: LogLines::default(),
            state: LogState::Connecting,
            paused: false,
            held_back: 0,
            version: 0,
        }
    }
}

/// Reconnect delays: `first`, doubling up to `max`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Backoff {
    fn delay(&self, attempt: u32) -> Duration {
        self.first
            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1).min(16)))
            .min(self.max)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionConfig {
    /// Lines the buffer keeps.
    pub limit: usize,
    /// Bytes the buffer's lines may take.
    pub budget: usize,
    /// At most one snapshot per this interval (ADR-0002's ~10 per second).
    pub publish_every: Duration,
    pub backoff: Backoff,
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
        }
    }
}

/// Lines of the newest timestamp remembered for resuming. Enough to recognise where the shown
/// lines end in a replay; a longer run of lines sharing one timestamp is matched on its end.
const RESUME_GROUP: usize = 64;

/// After a reconnect from `since_time` (which the server rounds down to the second), skips lines
/// already shown. Lines older than the newest shown timestamp are skipped. Lines stamped exactly
/// like it are held until the lines shown with that timestamp (`shown`, the newest ones) have
/// been replayed in order: those and everything before them were seen or cut by the tail, and
/// everything after them is new. The first request may have cut that group (`tail_lines`), so
/// the overlap is found by content, not by count.
#[derive(Debug, Clone)]
struct Resume {
    last: Timestamp,
    shown: Vec<Arc<str>>,
    held: Vec<LogLine>,
}

enum Phase {
    Connect(BoxFuture<'static, Result<RawLines, LogError>>),
    Read(RawLines),
    CheckEnd(BoxFuture<'static, Result<AfterEnd, LogError>>),
    Wait(Pin<Box<Sleep>>),
    Done,
}

enum Step {
    Opened(Result<RawLines, LogError>),
    Line(Result<RawLine, LogError>),
    Eof,
    AfterEnd(Result<AfterEnd, LogError>),
    Retry,
}

impl Phase {
    /// Waits for the phase's next outcome. Cancel-safe: the futures live in the phase.
    async fn next(&mut self) -> Step {
        match self {
            Phase::Connect(open) => Step::Opened(open.await),
            Phase::Read(lines) => match lines.next().await {
                Some(line) => Step::Line(line),
                None => Step::Eof,
            },
            Phase::CheckEnd(check) => Step::AfterEnd(check.await),
            Phase::Wait(sleep) => {
                sleep.await;
                Step::Retry
            }
            Phase::Done => std::future::pending().await,
        }
    }
}

struct Session {
    source: Arc<dyn LogSource>,
    target: LogTarget,
    options: LogOptions,
    config: SessionConfig,
    buffer: LogBuffer,
    state: LogState,
    /// Failed attempts in a row, for the backoff. Reset when a line arrives.
    attempts: u32,
    /// The newest timestamp shown, and the texts of the newest lines shown with it (at most
    /// [`RESUME_GROUP`]).
    last_ts: Option<Timestamp>,
    last_group: Vec<Arc<str>>,
    resume: Option<Resume>,
    /// The container finished: one last request without following picks up anything it wrote
    /// after the followed stream ended (it may have restarted and finished again meanwhile).
    draining: bool,
    /// The lines of the last snapshot published; pausing freezes these, so lines the front end
    /// never saw count as held back.
    published: LogLines,
    /// The lines shown while paused. Trimmed to the buffer's oldest line whenever the buffer
    /// drops a block, so a pause never keeps evicted blocks alive: a paused session stays within
    /// the byte budget like a running one (plus the frozen copy of the open tail block).
    frozen: Option<LogLines>,
    /// [`LogBuffer::dropped_blocks`] when `frozen` was last trimmed.
    frozen_dropped: u64,
    version: u64,
    dirty: bool,
}

impl Session {
    /// The first request: as asked, with the tail capped at what the buffer holds.
    fn first_request(&self) -> LogRequest {
        let limit = self.config.limit as i64;
        LogRequest {
            target: self.target.clone(),
            follow: self.options.follow && !self.options.previous && !self.draining,
            tail_lines: Some(self.options.tail_lines.map_or(limit, |t| t.clamp(0, limit))),
            since: self.options.since,
            previous: self.options.previous,
        }
    }

    /// A reconnect: from the last timestamp seen, else as at first.
    fn resume_request(&mut self) -> LogRequest {
        let mut request = self.first_request();
        if let Some(last) = self.last_ts {
            request.since = Some(LogSince::Time(last));
            request.tail_lines = None;
            self.resume = Some(Resume {
                last,
                shown: self.last_group.clone(),
                held: Vec::new(),
            });
        }
        request
    }

    /// Appends a line unless it repeats one already shown after a reconnect. A new line resets
    /// the backoff; repeats do not, so a crash-looping container that only replays its last
    /// lines is polled less and less often.
    fn push(&mut self, raw: RawLine) {
        let line = LogLine::parse(&raw.text, raw.truncated);
        if let Some(resume) = &mut self.resume {
            match line.timestamp {
                Some(ts) if ts < resume.last => return,
                Some(ts) if ts == resume.last => {
                    resume.held.push(line);
                    let held = &resume.held;
                    if held.len() >= resume.shown.len()
                        && held[held.len() - resume.shown.len()..]
                            .iter()
                            .zip(&resume.shown)
                            .all(|(l, s)| l.text == *s)
                    {
                        // The shown lines are replayed: the rest is new.
                        self.resume = None;
                    }
                    return;
                }
                _ => {
                    // The timestamp moved on without the shown lines replayed in order (the log
                    // changed under us): keep the held lines that were not shown.
                    let resume = self.resume.take().expect("checked above");
                    for held in resume.held {
                        if !resume.shown.contains(&held.text) {
                            self.append(held);
                        }
                    }
                }
            }
        }
        self.append(line);
    }

    fn append(&mut self, line: LogLine) {
        if let Some(ts) = line.timestamp {
            if self.last_ts != Some(ts) {
                self.last_ts = Some(ts);
                self.last_group.clear();
            }
            if self.last_group.len() == RESUME_GROUP {
                self.last_group.remove(0);
            }
            self.last_group.push(line.text.clone());
        }
        self.buffer.push(line);
        self.attempts = 0;
        self.dirty = true;
        if let Some(frozen) = &mut self.frozen
            && self.frozen_dropped != self.buffer.dropped_blocks()
        {
            *frozen = frozen.trimmed(self.buffer.first_id());
            self.frozen_dropped = self.buffer.dropped_blocks();
        }
    }

    fn set_state(&mut self, state: LogState) {
        if self.state != state {
            self.state = state;
            self.dirty = true;
        }
    }

    /// Waits before reopening the stream.
    fn retry(&mut self, error: Option<LogError>) -> Phase {
        self.attempts += 1;
        let delay = self.config.backoff.delay(self.attempts);
        if let Some(e) = &error {
            tracing::debug!(pod = self.target.pod, error = %e.message, ?delay, "log stream retry");
        }
        self.set_state(LogState::Reconnecting {
            error: error.map(|e| e.message),
            attempt: self.attempts,
        });
        Phase::Wait(Box::pin(tokio::time::sleep(delay)))
    }

    fn end(&mut self, error: Option<String>) -> Phase {
        self.set_state(LogState::Ended { error });
        Phase::Done
    }

    /// Applies one outcome; returns the next phase, or `None` to stay in the current one.
    fn handle(&mut self, step: Step) -> Option<Phase> {
        let next = match step {
            Step::Line(Ok(raw)) => {
                self.push(raw);
                return None;
            }
            Step::Opened(Ok(lines)) => {
                self.set_state(LogState::Streaming);
                Phase::Read(lines)
            }
            // The last catch-up of a finished container: what was read is all there is.
            Step::Opened(Err(e)) if e.fatal && self.draining => self.end(None),
            Step::Opened(Err(e)) | Step::AfterEnd(Err(e)) if e.fatal => self.end(Some(e.message)),
            Step::Opened(Err(e)) | Step::AfterEnd(Err(e)) | Step::Line(Err(e)) => {
                self.retry(Some(e))
            }
            Step::Eof if !self.options.follow || self.options.previous || self.draining => {
                self.end(None)
            }
            Step::Eof => Phase::CheckEnd(self.source.after_end(&self.target)),
            Step::AfterEnd(Ok(AfterEnd::Restarts)) => self.retry(None),
            Step::AfterEnd(Ok(AfterEnd::Finished)) => {
                self.draining = true;
                let request = self.resume_request();
                Phase::Connect(self.source.open(&request))
            }
            Step::AfterEnd(Ok(AfterEnd::Gone)) => self.end(Some("the pod was deleted".to_owned())),
            Step::Retry => {
                let request = self.resume_request();
                Phase::Connect(self.source.open(&request))
            }
        };
        Some(next)
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
        }));
        self.dirty = false;
    }
}

/// Runs a session until every receiver of `tx` is gone. `paused` pauses and resumes delivery.
pub(crate) async fn run(
    source: Arc<dyn LogSource>,
    target: LogTarget,
    options: LogOptions,
    config: SessionConfig,
    tx: watch::Sender<Arc<LogSnapshot>>,
    mut paused: watch::Receiver<bool>,
) {
    let mut session = Session {
        buffer: LogBuffer::with_budget(config.limit, config.budget),
        source,
        target,
        options,
        config,
        state: LogState::Connecting,
        attempts: 0,
        last_ts: None,
        last_group: Vec::new(),
        resume: None,
        draining: false,
        published: LogLines::default(),
        frozen: None,
        frozen_dropped: 0,
        version: 0,
        dirty: true,
    };
    let first = session.first_request();
    let mut phase = Phase::Connect(session.source.open(&first));
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
                if paused != session.frozen.is_some() {
                    let first = session.buffer.first_id();
                    session.frozen = paused.then(|| session.published.trimmed(first));
                    session.frozen_dropped = session.buffer.dropped_blocks();
                    session.dirty = true;
                }
            }
            _ = tick.tick(), if session.dirty => session.publish(&tx),
            step = phase.next() => {
                if let Some(next) = session.handle(step) {
                    phase = next;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::BLOCKS_PER_BUDGET;
    use futures::FutureExt as _;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// One scripted outcome of `open`: an error, or lines then an end (EOF or an error).
    enum Script {
        Fail(LogError),
        Lines(Vec<&'static str>, Option<LogError>),
        /// Lines, then the stream stays open.
        Hold(Vec<&'static str>),
        /// Lines as the test sends them.
        Feed(futures::channel::mpsc::UnboundedReceiver<&'static str>),
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
            let raw = |t: &str| {
                Ok(RawLine {
                    text: t.to_owned(),
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

        fn after_end(&self, _: &LogTarget) -> BoxFuture<'static, Result<AfterEnd, LogError>> {
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
        }
    }

    struct Running {
        rx: watch::Receiver<Arc<LogSnapshot>>,
        paused: watch::Sender<bool>,
        fake: Arc<Fake>,
    }

    fn start(scripts: Vec<Script>, options: LogOptions, limit: usize) -> Running {
        let fake = Arc::new(Fake::default());
        fake.scripts.lock().unwrap().extend(scripts);
        let (tx, rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (paused, paused_rx) = watch::channel(false);
        tokio::spawn(run(
            fake.clone(),
            target(),
            options,
            config(limit),
            tx,
            paused_rx,
        ));
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

    fn leak(text: &str) -> &'static str {
        Box::leak(text.to_owned().into_boxed_str())
    }

    fn ended(s: &LogSnapshot) -> bool {
        matches!(s.state, LogState::Ended { .. })
    }

    const T1: &str = "2024-05-01T12:00:01.100Z";

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn reads_everything_once_and_ends_when_not_following() {
        let options = LogOptions {
            follow: false,
            tail_lines: Some(500),
            ..LogOptions::default()
        };
        let lines = vec![
            "2024-05-01T12:00:00Z ERROR boom",
            "2024-05-01T12:00:01Z WARN slow",
        ];
        let mut run = start(vec![Script::Lines(lines, None)], options, 100);
        let snapshot = run.until(ended).await;
        assert_eq!(texts(&snapshot), ["ERROR boom", "WARN slow"]);
        assert_eq!(snapshot.state, LogState::Ended { error: None });
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
        let lines: Vec<&'static str> = (0..5000)
            .map(|n| &*Box::leak(format!("{T1} line {n}").into_boxed_str()))
            .collect();
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
        let first = vec![
            "2024-05-01T12:00:00.500Z a",
            "2024-05-01T12:00:01.100Z b",
            "2024-05-01T12:00:01.100Z c",
        ];
        // From `since_time` rounded down to the second: a repeat of b and c, then new lines,
        // one of which shares their timestamp.
        let second = vec![
            "2024-05-01T12:00:01.000Z old",
            "2024-05-01T12:00:01.100Z b",
            "2024-05-01T12:00:01.100Z c",
            "2024-05-01T12:00:01.100Z d",
            "2024-05-01T12:00:02Z e",
        ];
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
                error: Some("still down".into()),
                attempt: 2
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
                Script::Lines(
                    vec![leak(&format!("{T} C"))],
                    Some(LogError::retry("reset")),
                ),
                Script::Hold(vec![
                    leak(&format!("{T} A")),
                    leak(&format!("{T} B")),
                    leak(&format!("{T} C")),
                    leak(&format!("{T} D")),
                    "2024-05-01T12:00:02Z E",
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
                Script::Lines(vec!["2024-05-01T12:00:00Z run 1"], None),
                // It restarted and finished again before the status check.
                Script::Lines(
                    vec!["2024-05-01T12:00:00Z run 1", "2024-05-01T12:00:03Z run 2"],
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
        assert_eq!(snapshot.state, LogState::Ended { error: None });
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
        feed.unbounded_send("one").unwrap();
        feed.unbounded_send("two").unwrap();
        run.until(|s| s.lines.len() == 2).await;
        // "three" reaches the buffer but no snapshot before the pause.
        feed.unbounded_send("three").unwrap();
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
                Script::Lines(vec!["2024-05-01T12:00:00Z run 1"], None),
                Script::Lines(vec!["2024-05-01T12:00:05Z run 2"], None),
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
                error: Some("the pod was deleted".into())
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
                error: Some("NotFound: pods \"p\" not found".into())
            }
        );

        let options = LogOptions {
            previous: true,
            ..LogOptions::default()
        };
        let mut run = start(vec![Script::Lines(vec!["last words"], None)], options, 100);
        let snapshot = run.until(ended).await;
        assert_eq!(snapshot.state, LogState::Ended { error: None });
        let request = &run.requests()[0];
        assert!(request.previous && !request.follow);
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn pausing_freezes_the_lines_and_counts_what_is_held_back() {
        let (feed, lines) = futures::channel::mpsc::unbounded();
        let mut run = start(vec![Script::Feed(lines)], LogOptions::default(), 100);
        feed.unbounded_send("one").unwrap();
        feed.unbounded_send("two").unwrap();
        run.until(|s| s.lines.len() == 2).await;
        run.paused.send(true).unwrap();
        run.until(|s| s.paused).await;
        feed.unbounded_send("three").unwrap();
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
        tokio::spawn(run(
            fake.clone(),
            target(),
            LogOptions::default(),
            config,
            tx,
            paused_rx,
        ));
        let mut run = Running { rx, paused, fake };
        let long: &'static str = Box::leak("z".repeat(32 * 1024).into_boxed_str());
        for _ in 0..40 {
            feed.unbounded_send(long).unwrap();
        }
        run.until(|s| s.lines.end_id() == 40).await;
        run.paused.send(true).unwrap();
        run.until(|s| s.paused).await;
        // Five budgets' worth arrives while paused.
        for _ in 0..(5 * budget / long.len()) {
            feed.unbounded_send(long).unwrap();
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
            .push_back(Script::Hold(vec!["x"]));
        let (tx, rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        let (paused, paused_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            fake,
            target(),
            LogOptions::default(),
            config(10),
            tx,
            paused_rx,
        ));
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
                Script::Lines(vec![last], None),
                Script::Lines(vec![last], None),
                Script::Lines(vec![last], None),
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

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let backoff = Backoff {
            first: Duration::from_secs(1),
            max: Duration::from_secs(30),
        };
        let delays: Vec<_> = (1..=7).map(|a| backoff.delay(a).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(backoff.delay(u32::MAX).as_secs(), 30);
    }
}
