//! One container's log stream inside a session: opens it, reads it, reconnects with backoff when
//! it drops (without repeating lines already shown), and decides when it is over.
//!
//! A pod session drives one stream; a workload session drives one per pod container. A stream
//! is a state machine polled by its session's task ([`Stream::poll_step`], then
//! [`Stream::handle`]), so a session with many streams still runs on one task and needs no
//! channels between them.

use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use k8s_openapi::jiff::Timestamp;
use tokio::time::Sleep;

use super::line::{LineSource, LogLine};
use super::session::{LogOptions, LogSince, LogState, LogTarget, PREVIOUS_NOT_AVAILABLE};
use super::source::{
    AfterEnd, CRASH_LOOP, ErrorKind, LogError, LogRequest, LogSource, RawLine, RawLines,
    is_previous_missing, is_unavailable_notice,
};

/// Reconnect delays: `first`, doubling up to `max`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Backoff {
    pub fn delay(&self, attempt: u32) -> Duration {
        self.first
            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1).min(16)))
            .min(self.max)
    }
}

/// Failed attempts in a row and the [`Backoff`] they wait by: what a stream and a workload's pod
/// watch share when they reopen after a failure. The owner decides what counts as progress and
/// calls [`Retry::reset`] then.
#[derive(Debug)]
pub(crate) struct Retry {
    attempts: u32,
    backoff: Backoff,
}

/// One failure counted by [`Retry::fail`]: the state to show and the wait before reopening.
pub(crate) struct Reconnect {
    /// [`LogState::Reconnecting`] with this attempt's number.
    pub state: LogState,
    pub delay: Duration,
    /// Sleeps for `delay`; the owner parks in it.
    pub sleep: Pin<Box<Sleep>>,
}

impl Retry {
    pub fn new(backoff: Backoff) -> Self {
        Retry {
            attempts: 0,
            backoff,
        }
    }

    /// Progress was made: the next failure is the first again.
    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    /// Counts a failure and returns how to wait for the next attempt. `back_off`: the container
    /// waits in `CrashLoopBackOff` (see [`LogState::Reconnecting`]).
    pub fn fail(&mut self, error: Option<&LogError>, back_off: bool) -> Reconnect {
        self.attempts += 1;
        let delay = self.backoff.delay(self.attempts);
        Reconnect {
            state: LogState::Reconnecting {
                error: error.map(|e| e.message.clone()),
                attempt: self.attempts,
                back_off,
            },
            delay,
            sleep: Box::pin(tokio::time::sleep(delay)),
        }
    }
}

/// How long a stream of a deleted pod may keep reading before it is dropped. The kubelet closes
/// the stream when the container stops, which normally comes first; this bounds a stream whose
/// node went away without closing it.
pub(crate) const GONE_GRACE: Duration = Duration::from_secs(10);

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
    shown: Vec<u64>,
    held: Vec<LogLine>,
}

/// A line text's fingerprint, what resuming compares. Keeping fingerprints instead of the texts
/// means a stream never keeps lines alive after the buffer evicted them: an ended stream that may
/// be revived holds [`RESUME_GROUP`] numbers, not up to that many 64 KiB lines, outside the
/// session's budget.
fn fingerprint(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

enum Phase {
    Connect(BoxFuture<'static, Result<RawLines, LogError>>),
    Read(RawLines),
    CheckEnd(BoxFuture<'static, Result<AfterEnd, LogError>>),
    Wait(Pin<Box<Sleep>>),
    Done,
}

/// The next outcome of a stream, from [`Stream::poll_step`].
pub(crate) enum Step {
    Opened(Result<RawLines, LogError>),
    Line(Result<RawLine, LogError>),
    Eof,
    AfterEnd(Result<AfterEnd, LogError>),
    Retry,
    /// The pod is gone and [`GONE_GRACE`] passed.
    Expired,
}

pub(crate) struct Stream {
    source: Arc<dyn LogSource>,
    target: LogTarget,
    options: LogOptions,
    /// `tail_lines` of the first request.
    tail: i64,
    /// Failed attempts in a row, for the backoff. Reset when a line arrives (and on a revive).
    retry: Retry,
    /// The pod set this stream belongs to (workload sessions).
    member: Option<Member>,
    phase: Phase,
    state: LogState,
    /// The state changed in the current [`Stream::handle`].
    changed: bool,
    /// The newest timestamp shown, and the fingerprints of the newest lines shown with it (at most
    /// [`RESUME_GROUP`]).
    last_ts: Option<Timestamp>,
    last_group: Vec<u64>,
    resume: Option<Resume>,
    /// The container finished: one last request without following picks up anything it wrote
    /// after the followed stream ended (it may have restarted and finished again meanwhile).
    draining: bool,
    /// The pod was deleted: finish the current read, then end instead of reconnecting.
    gone: bool,
    /// The pod is being deleted (single-pod sessions): the last catch-up read ends the stream
    /// with `pod_deleted`.
    deleted: bool,
    /// The pod was seen being deleted while its container still ran (single-pod sessions): if
    /// it is gone (404) by the next open, it was deleted.
    terminating: bool,
    /// An open succeeded once: the pod existed, so a later 404 means it was deleted.
    opened: bool,
    /// The current open has delivered no line yet: a first line that is the kubelet's "unable to
    /// retrieve container logs" is its answer, not the container's log.
    fresh: bool,
    /// The kubelet's "cannot read the log" answer, from the last read that got one instead of
    /// lines: the error a stream that then ends or waits reports. Cleared by a real line.
    notice: Option<String>,
    deadline: Option<Pin<Box<Sleep>>>,
    /// The stream ended on an error about the cluster rather than this container.
    cluster_error: bool,
}

/// What a stream of a workload session knows about its pod.
pub(crate) struct Member {
    /// What every line is tagged with.
    pub source: Arc<LineSource>,
    /// The pod's uid: a pod recreated under the same name is another pod, and its stream another
    /// stream.
    pub uid: String,
}

impl Stream {
    /// Starts opening the stream. `tail` is the first request's `tail_lines`.
    pub fn new(
        source: Arc<dyn LogSource>,
        target: LogTarget,
        options: LogOptions,
        tail: i64,
        backoff: Backoff,
        member: Option<Member>,
    ) -> Self {
        let mut stream = Stream {
            source,
            target,
            options,
            tail,
            retry: Retry::new(backoff),
            member,
            phase: Phase::Done,
            state: LogState::Connecting,
            changed: false,
            last_ts: None,
            last_group: Vec::new(),
            resume: None,
            draining: false,
            gone: false,
            deleted: false,
            terminating: false,
            opened: false,
            fresh: false,
            notice: None,
            deadline: None,
            cluster_error: false,
        };
        let first = stream.first_request();
        stream.phase = Phase::Connect(stream.source.open(&first));
        stream
    }

    pub fn state(&self) -> &LogState {
        &self.state
    }

    /// What the stream's lines are tagged with (workload sessions).
    pub fn tag(&self) -> Option<&Arc<LineSource>> {
        self.member.as_ref().map(|m| &m.source)
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// The stream ended on an error that is about the cluster (it left the kubeconfig, or the
    /// context now points at another server), not about this container.
    pub fn ended_on_cluster_error(&self) -> bool {
        self.cluster_error
    }

    /// The first request: as asked, with the tail as given.
    fn first_request(&self) -> LogRequest {
        LogRequest {
            target: self.target.clone(),
            uid: self.member.as_ref().map(|m| m.uid.clone()),
            follow: self.options.follow && !self.options.previous && !self.draining,
            tail_lines: Some(self.tail),
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

    /// Waits for the stream's next outcome. Cancel-safe: the futures live in the stream.
    pub fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<Step> {
        if let Some(deadline) = &mut self.deadline
            && deadline.as_mut().poll(cx).is_ready()
        {
            self.deadline = None;
            return Poll::Ready(Step::Expired);
        }
        match &mut self.phase {
            Phase::Connect(open) => open.poll_unpin(cx).map(Step::Opened),
            Phase::Read(lines) => lines
                .poll_next_unpin(cx)
                .map(|line| line.map_or(Step::Eof, Step::Line)),
            Phase::CheckEnd(check) => check.poll_unpin(cx).map(Step::AfterEnd),
            Phase::Wait(sleep) => sleep.as_mut().poll(cx).map(|()| Step::Retry),
            Phase::Done => Poll::Pending,
        }
    }

    /// Applies one outcome. New lines go to `out`, already tagged. Returns whether the state
    /// changed.
    pub fn handle(&mut self, step: Step, out: &mut dyn FnMut(LogLine)) -> bool {
        self.changed = false;
        let next = match step {
            // The kubelet could not read the log. For the previous instance that is the end.
            // For the current one it is an end of stream like any other, with the kubelet's
            // words as the error: not following, it ends; following, the pod's status decides
            // whether to wait for a restart or end (a finished container whose log is gone).
            Step::Line(Ok(raw)) if self.fresh && is_unavailable_notice(&raw.text) => {
                self.fresh = false;
                let notice = raw.text.trim_end().to_owned();
                if self.options.previous {
                    self.end(Some(format!("{PREVIOUS_NOT_AVAILABLE}: {notice}")))
                } else {
                    self.notice = Some(notice);
                    self.at_end()
                }
            }
            Step::Line(Ok(raw)) => {
                self.fresh = false;
                self.notice = None;
                self.push(raw, out);
                return false;
            }
            Step::Opened(Ok(lines)) => {
                self.opened = true;
                self.fresh = true;
                // A new read: an earlier read's "cannot read the log" no longer holds. If this
                // one gets it again, it is set again.
                self.notice = None;
                self.set_state(LogState::Streaming);
                Phase::Read(lines)
            }
            // Before anything else, even a drain or a deleted pod: the session must hear of it.
            Step::Opened(Err(e)) | Step::AfterEnd(Err(e)) if e.is_cluster() => {
                self.cluster_error = true;
                self.end(Some(e.message))
            }
            // The pod by this name is not the stream's pod any more: nothing of it is read.
            Step::Opened(Err(e)) if e.kind == ErrorKind::PodGone => self.end(None),
            // A deleted pod's stream ends at whatever comes next.
            Step::Expired
            | Step::Eof
            | Step::Opened(Err(_))
            | Step::Line(Err(_))
            | Step::AfterEnd(_)
            | Step::Retry
                if self.gone =>
            {
                self.end(None)
            }
            Step::Expired => return false,
            // A pod that was read before, or seen being deleted, and is now not found was
            // deleted: an ordinary end. A 404 on the very first open stays an error (a wrong
            // name, or a pod deleted before the session began).
            Step::Opened(Err(e))
                if e.kind == ErrorKind::NotFound
                    && self.member.is_none()
                    && (self.opened || self.terminating || self.deleted) =>
            {
                self.end_deleted()
            }
            // The last catch-up of a finished container: what was read is all there is.
            Step::Opened(Err(e)) if e.is_fatal() && self.draining => self.drained(),
            // No previous instance (its container was removed): said like the kubelet's answer.
            Step::Opened(Err(e)) if self.options.previous && is_previous_missing(&e.message) => {
                self.end(Some(format!("{PREVIOUS_NOT_AVAILABLE}: {}", e.message)))
            }
            Step::Opened(Err(e)) | Step::AfterEnd(Err(e)) if e.is_fatal() => {
                self.end(Some(e.message))
            }
            Step::Opened(Err(e)) | Step::AfterEnd(Err(e)) | Step::Line(Err(e)) => {
                self.retry(Some(e))
            }
            Step::Eof => self.at_end(),
            Step::AfterEnd(Ok(AfterEnd::Restarts)) => self.retry(self.notice_error()),
            Step::AfterEnd(Ok(AfterEnd::BackOff)) => self.wait(self.notice_error(), true),
            Step::AfterEnd(Ok(AfterEnd::Terminating)) => {
                self.terminating = self.member.is_none();
                self.retry(self.notice_error())
            }
            Step::AfterEnd(Ok(end @ (AfterEnd::Finished | AfterEnd::Deleting))) => {
                // Pods of a workload come and go; a deleted one ends its stream quietly.
                self.deleted = end == AfterEnd::Deleting && self.member.is_none();
                self.draining = true;
                let request = self.resume_request();
                Phase::Connect(self.source.open(&request))
            }
            // Pods of a workload come and go; that is no error.
            Step::AfterEnd(Ok(AfterEnd::Gone)) if self.member.is_some() => self.end(None),
            Step::AfterEnd(Ok(AfterEnd::Gone)) => self.end_deleted(),
            Step::Retry => {
                let request = self.resume_request();
                Phase::Connect(self.source.open(&request))
            }
        };
        self.phase = next;
        self.changed
    }

    /// The container is running again (the pod watch saw it restart): skip the rest of the
    /// backoff.
    pub fn nudge(&mut self) {
        if matches!(self.phase, Phase::Wait(_)) {
            self.phase = Phase::Wait(Box::pin(tokio::time::sleep(Duration::ZERO)));
        }
    }

    /// A stream that ended (on an error, or because its container finished) while its pod
    /// stays: the container restarted, so read on from where the stream stopped.
    pub fn revive(&mut self) {
        if !self.is_done() || self.gone {
            return;
        }
        self.draining = false;
        self.deleted = false;
        self.retry.reset();
        self.set_state(LogState::Connecting);
        let request = self.resume_request();
        self.phase = Phase::Connect(self.source.open(&request));
    }

    /// The pod was deleted. With `grace`, a stream that is reading finishes what the server still
    /// sends (the container's last lines), for at most [`GONE_GRACE`]; any other stream ends now.
    /// No grace when the pod was replaced by one of the same name: reading by name would read
    /// the new pod.
    pub fn pod_gone(&mut self, grace: bool) {
        if self.gone || self.is_done() {
            return;
        }
        self.gone = true;
        if grace && matches!(self.phase, Phase::Read(_)) {
            self.deadline = Some(Box::pin(tokio::time::sleep(GONE_GRACE)));
        } else {
            self.phase = self.end(None);
        }
    }

    /// The pod is back before its stream ended: it left the pod set and matches again (its
    /// labels changed back) within [`GONE_GRACE`]. The same pod (same uid) still runs, so the
    /// pending end is called off and the stream reads on as if the pod had never left.
    pub fn pod_back(&mut self) {
        if self.gone && !self.is_done() {
            self.gone = false;
            self.deadline = None;
        }
    }

    /// Appends a line unless it repeats one already shown after a reconnect. A new line resets
    /// the backoff; repeats do not, so a crash-looping container that only replays its last
    /// lines is polled less and less often.
    fn push(&mut self, raw: RawLine, out: &mut dyn FnMut(LogLine)) {
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
                            .all(|(l, s)| fingerprint(&l.text) == *s)
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
                        if !resume.shown.contains(&fingerprint(&held.text)) {
                            self.append(held, out);
                        }
                    }
                }
            }
        }
        self.append(line, out);
    }

    fn append(&mut self, mut line: LogLine, out: &mut dyn FnMut(LogLine)) {
        if let Some(ts) = line.timestamp {
            if self.last_ts != Some(ts) {
                self.last_ts = Some(ts);
                self.last_group.clear();
            }
            if self.last_group.len() == RESUME_GROUP {
                self.last_group.remove(0);
            }
            self.last_group.push(fingerprint(&line.text));
        }
        line.source = self.tag().cloned();
        self.retry.reset();
        out(line);
    }

    fn set_state(&mut self, state: LogState) {
        if self.state != state {
            self.state = state;
            self.changed = true;
        }
    }

    /// Waits before reopening the stream. An error that names `CrashLoopBackOff` is the
    /// kubelet's back-off.
    fn retry(&mut self, error: Option<LogError>) -> Phase {
        let back_off = error
            .as_ref()
            .is_some_and(|e| e.message.contains(CRASH_LOOP));
        self.wait(error, back_off)
    }

    /// Waits before reopening the stream; `back_off`: the container waits in `CrashLoopBackOff`.
    fn wait(&mut self, error: Option<LogError>, back_off: bool) -> Phase {
        let next = self.retry.fail(error.as_ref(), back_off);
        if let Some(e) = &error {
            let delay = next.delay;
            tracing::debug!(pod = self.target.pod, error = %e.message, ?delay, "log stream retry");
        }
        self.set_state(next.state);
        Phase::Wait(next.sleep)
    }

    /// Ends after the last catch-up read: quietly for a container that finished, with
    /// `pod_deleted` for a single-pod stream whose pod is being deleted.
    fn drained(&mut self) -> Phase {
        if self.deleted {
            self.end_deleted()
        } else {
            let notice = self.notice.take();
            self.end(notice)
        }
    }

    /// The read ended (or the kubelet could not serve it): a drain is over, a read that does not
    /// follow ends (with the kubelet's words, if that is what came), and a followed one asks the
    /// pod's status whether the container can log again.
    fn at_end(&mut self) -> Phase {
        if self.draining {
            self.drained()
        } else if !self.options.follow || self.options.previous {
            let notice = self.notice.take();
            self.end(notice)
        } else {
            let uid = self.member.as_ref().map(|m| m.uid.as_str());
            Phase::CheckEnd(self.source.after_end(&self.target, uid))
        }
    }

    /// The kubelet's answer as the error to wait with, if that is what the last read got.
    fn notice_error(&self) -> Option<LogError> {
        self.notice.clone().map(LogError::retry)
    }

    fn end(&mut self, error: Option<String>) -> Phase {
        self.finish(LogState::Ended {
            error,
            pod_deleted: false,
        })
    }

    /// Ends a single-pod stream because its pod was deleted: an expected end, not an error.
    fn end_deleted(&mut self) -> Phase {
        self.finish(LogState::Ended {
            error: None,
            pod_deleted: true,
        })
    }

    fn finish(&mut self, state: LogState) -> Phase {
        self.deadline = None;
        // Lines held for a resume are not kept past the end.
        self.resume = None;
        self.set_state(state);
        Phase::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    /// Serves the same lines at every open; they end (EOF) when `ends`, else stay open.
    struct Fixed {
        lines: Vec<String>,
        ends: bool,
        uids: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl LogSource for Fixed {
        fn open(&self, _: &LogRequest) -> BoxFuture<'static, Result<RawLines, LogError>> {
            let ends = self.ends;
            let lines: Vec<_> = self
                .lines
                .iter()
                .map(|t| {
                    Ok(RawLine {
                        text: t.clone(),
                        truncated: false,
                    })
                })
                .collect();
            let lines = stream::iter(lines);
            let lines = if ends {
                lines.boxed()
            } else {
                lines.chain(stream::pending()).boxed()
            };
            async move { Ok(lines) }.boxed()
        }

        fn after_end(
            &self,
            _: &LogTarget,
            uid: Option<&str>,
        ) -> BoxFuture<'static, Result<AfterEnd, LogError>> {
            self.uids.lock().unwrap().push(uid.map(str::to_owned));
            async { Ok(AfterEnd::Gone) }.boxed()
        }
    }

    fn stream(lines: &[&str], tag: Option<Arc<LineSource>>) -> Stream {
        stream_on(fixed(lines, false), tag)
    }

    fn fixed(lines: &[&str], ends: bool) -> Arc<Fixed> {
        Arc::new(Fixed {
            lines: lines.iter().map(|&t| t.to_owned()).collect(),
            ends,
            uids: Default::default(),
        })
    }

    fn stream_on(source: Arc<Fixed>, tag: Option<Arc<LineSource>>) -> Stream {
        let member = tag.map(|source| Member {
            source,
            uid: "u1".into(),
        });
        Stream::new(
            source,
            LogTarget {
                namespace: "ns".into(),
                pod: "p".into(),
                container: Some("c".into()),
            },
            LogOptions::default(),
            10,
            Backoff {
                first: Duration::from_secs(1),
                max: Duration::from_secs(30),
            },
            member,
        )
    }

    /// Drives `stream` until it has no outcome ready, collecting its lines.
    async fn drain(stream: &mut Stream) -> Vec<LogLine> {
        let mut out = Vec::new();
        loop {
            let step = futures::future::poll_fn(|cx| match stream.poll_step(cx) {
                Poll::Ready(step) => Poll::Ready(Some(step)),
                Poll::Pending => Poll::Ready(None),
            })
            .await;
            match step {
                Some(step) => {
                    stream.handle(step, &mut |l| out.push(l));
                }
                None => return out,
            }
        }
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

    fn attempt_of(state: &LogState) -> u32 {
        match state {
            LogState::Reconnecting { attempt, .. } => *attempt,
            other => panic!("not reconnecting: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_counts_attempts_and_waits_longer_up_to_the_cap() {
        let mut retry = Retry::new(Backoff {
            first: Duration::from_secs(1),
            max: Duration::from_secs(30),
        });
        let mut seen = Vec::new();
        for _ in 0..7 {
            let next = retry.fail(None, false);
            let left = next.sleep.deadline() - tokio::time::Instant::now();
            assert_eq!(left, next.delay, "parks for the delay");
            seen.push((attempt_of(&next.state), next.delay.as_secs()));
        }
        assert_eq!(
            seen,
            [(1, 1), (2, 2), (3, 4), (4, 8), (5, 16), (6, 30), (7, 30)]
        );
        retry.reset();
        assert_eq!(retry.fail(None, false).delay, Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_reports_the_error_and_the_back_off() {
        let mut retry = Retry::new(Backoff {
            first: Duration::from_millis(500),
            max: Duration::from_secs(4),
        });
        assert_eq!(
            retry.fail(Some(&LogError::retry("reset")), false).state,
            LogState::Reconnecting {
                error: Some("reset".into()),
                attempt: 1,
                back_off: false,
            }
        );
        assert_eq!(
            retry.fail(None, true).state,
            LogState::Reconnecting {
                error: None,
                attempt: 2,
                back_off: true,
            }
        );
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn lines_are_tagged_with_the_streams_source() {
        let tag = Arc::new(LineSource::new("web-1", "app"));
        let mut s = stream(&["one", "two"], Some(tag.clone()));
        let lines = drain(&mut s).await;
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.source.as_ref() == Some(&tag)));
        assert_eq!(s.state(), &LogState::Streaming);
        assert!(Arc::ptr_eq(s.tag().unwrap(), &tag));
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_deleted_pods_stream_reads_on_for_a_grace_period_then_ends() {
        let mut s = stream(&["last"], None);
        assert_eq!(drain(&mut s).await.len(), 1);
        s.pod_gone(true);
        assert!(!s.is_done(), "still reading the last lines");
        tokio::time::advance(GONE_GRACE + Duration::from_millis(1)).await;
        drain(&mut s).await;
        assert!(s.is_done());
        assert_eq!(
            s.state(),
            &LogState::Ended {
                error: None,
                pod_deleted: false
            }
        );
        assert!(!s.ended_on_cluster_error());
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_nudge_skips_the_rest_of_the_backoff() {
        let mut s = stream(&[], None);
        drain(&mut s).await;
        s.handle(Step::Line(Err(LogError::retry("reset"))), &mut |_| {});
        assert!(matches!(
            s.state(),
            LogState::Reconnecting { attempt: 1, .. }
        ));
        s.nudge();
        tokio::task::yield_now().await;
        drain(&mut s).await;
        assert_eq!(s.state(), &LogState::Streaming, "reopened without waiting");

        // A stream that is not waiting ends at once when its pod goes.
        let mut waiting = stream(&[], None);
        waiting.handle(Step::Opened(Err(LogError::retry("x"))), &mut |_| {});
        waiting.pod_gone(true);
        assert!(waiting.is_done());
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_workload_stream_ends_quietly_with_its_pod_and_revives_on_a_restart() {
        let source = fixed(&["2024-05-01T12:00:00Z last words"], true);
        let tag = Arc::new(LineSource::new("web-1", "app"));
        let mut s = stream_on(source.clone(), Some(tag));
        assert_eq!(drain(&mut s).await.len(), 1);
        // The pod lookup is by uid: a pod of the same name is another pod, so this one is gone.
        assert_eq!(*source.uids.lock().unwrap(), [Some("u1".to_owned())]);
        assert!(s.is_done());
        assert_eq!(
            s.state(),
            &LogState::Ended {
                error: None,
                pod_deleted: false
            },
            "no error"
        );

        // Restarted after all: it reads on from where it stopped, without repeating lines.
        s.revive();
        assert_eq!(s.state(), &LogState::Connecting);
        assert!(drain(&mut s).await.is_empty(), "the replay is skipped");
        assert!(s.is_done());

        // A pod session still says why it ended.
        let plain = fixed(&[], true);
        let mut single = stream_on(plain.clone(), None);
        drain(&mut single).await;
        assert_eq!(
            single.state(),
            &LogState::Ended {
                error: None,
                pod_deleted: true,
            }
        );
        assert_eq!(*plain.uids.lock().unwrap(), [None]);
    }

    /// A stream reading the previous instance (`previous`) or following the current one.
    fn previous_stream(lines: &[&str], previous: bool) -> Stream {
        let mut s = stream_on(fixed(lines, true), None);
        s.options.previous = previous;
        s
    }

    // covers UP-40: the kubelet's "unable to retrieve container logs" answer, sent as the log's
    // only line while a container restarts, ends a previous-instance session with a reason
    // instead of showing as line 1; for the current instance it is tried again.
    #[tokio::test(start_paused = true)]
    async fn the_kubelets_unavailable_answer_is_not_a_line() {
        let notice = "unable to retrieve container logs for containerd://c54f232e5b";
        let mut previous = previous_stream(&[notice], true);
        assert!(drain(&mut previous).await.is_empty());
        assert_eq!(
            previous.state().previous_not_available(),
            Some(notice),
            "{:?}",
            previous.state()
        );
        assert!(previous.is_done());

        // A removed container's log, in a read that does not follow: it ends, saying why.
        let mut once = previous_stream(&[notice], false);
        once.options.follow = false;
        assert!(drain(&mut once).await.is_empty());
        assert_eq!(
            once.state(),
            &LogState::Ended {
                error: Some(notice.into()),
                pod_deleted: false
            }
        );
        // The previous terminated container is gone: said the same way.
        let mut missing = previous_stream(&[], true);
        missing.handle(
            Step::Opened(Err(LogError::fatal(
                "BadRequest: previous terminated container \"c\" in pod \"p\" not found",
            ))),
            &mut |_| {},
        );
        assert!(missing.state().previous_not_available().is_some());

        // Only as the first line of a read: later, the same words are the container's.
        let mut logged = previous_stream(&["2024-05-01T12:00:00Z hello", notice], true);
        assert_eq!(drain(&mut logged).await.len(), 2);
        assert_eq!(logged.state().previous_not_available(), None);
    }

    /// Serves `lines` at every open and ends; the pod's fate after the end is `fate`.
    struct Fated {
        lines: Vec<String>,
        fate: AfterEnd,
    }

    impl LogSource for Fated {
        fn open(&self, _: &LogRequest) -> BoxFuture<'static, Result<RawLines, LogError>> {
            let lines: Vec<_> = self
                .lines
                .iter()
                .map(|t| {
                    Ok(RawLine {
                        text: t.clone(),
                        truncated: false,
                    })
                })
                .collect();
            async move { Ok(stream::iter(lines).boxed()) }.boxed()
        }

        fn after_end(
            &self,
            _: &LogTarget,
            _: Option<&str>,
        ) -> BoxFuture<'static, Result<AfterEnd, LogError>> {
            let fate = self.fate;
            async move { Ok(fate) }.boxed()
        }
    }

    // covers UP-40: following a container whose log the kubelet cannot read is decided by the
    // pod's status like any end of stream: a finished container (a completed Job pod whose
    // container was removed) ends with the kubelet's words instead of reconnecting forever; one
    // that restarts is waited for, saying why.
    #[tokio::test(start_paused = true)]
    async fn the_kubelets_answer_for_the_current_instance_goes_through_the_end_check() {
        let notice = "unable to retrieve container logs for containerd://c54f232e5b";
        let followed = |fate| {
            Stream::new(
                Arc::new(Fated {
                    lines: vec![notice.to_owned()],
                    fate,
                }),
                LogTarget {
                    namespace: "ns".into(),
                    pod: "p".into(),
                    container: Some("c".into()),
                },
                LogOptions::default(),
                10,
                Backoff {
                    first: Duration::from_secs(1),
                    max: Duration::from_secs(30),
                },
                None,
            )
        };
        let mut finished = followed(AfterEnd::Finished);
        assert!(drain(&mut finished).await.is_empty());
        assert_eq!(
            finished.state(),
            &LogState::Ended {
                error: Some(notice.into()),
                pod_deleted: false
            }
        );
        assert!(finished.is_done());

        let mut restarts = followed(AfterEnd::Restarts);
        assert!(drain(&mut restarts).await.is_empty());
        assert!(matches!(
            restarts.state(),
            LogState::Reconnecting { attempt: 1, error: Some(e), back_off: false } if e == notice
        ));
    }

    // covers UP-40: a reopen that succeeds drops the kubelet's earlier answer, so a later end or
    // wait does not report it.
    #[tokio::test(start_paused = true)]
    async fn a_successful_reopen_forgets_the_kubelets_answer() {
        let notice = "unable to retrieve container logs for containerd://c54f232e5b";
        let mut s = Stream::new(
            Arc::new(Fated {
                lines: vec![notice.to_owned()],
                fate: AfterEnd::Restarts,
            }),
            LogTarget {
                namespace: "ns".into(),
                pod: "p".into(),
                container: Some("c".into()),
            },
            LogOptions::default(),
            10,
            Backoff {
                first: Duration::from_secs(1),
                max: Duration::from_secs(30),
            },
            None,
        );
        drain(&mut s).await;
        assert!(matches!(
            s.state(),
            LogState::Reconnecting { error: Some(_), .. }
        ));
        // The container restarted: its log opens, holds nothing, and ends.
        s.handle(Step::Opened(Ok(stream::empty().boxed())), &mut |_| {});
        s.handle(Step::Eof, &mut |_| {});
        s.handle(Step::AfterEnd(Ok(AfterEnd::Restarts)), &mut |_| {});
        assert!(
            matches!(s.state(), LogState::Reconnecting { error: None, .. }),
            "{:?}",
            s.state()
        );
        // Finished: the last catch-up reads nothing more and ends without an error.
        s.handle(Step::Opened(Ok(stream::empty().boxed())), &mut |_| {});
        s.handle(Step::Eof, &mut |_| {});
        s.handle(Step::AfterEnd(Ok(AfterEnd::Finished)), &mut |_| {});
        s.handle(Step::Opened(Ok(stream::empty().boxed())), &mut |_| {});
        s.handle(Step::Eof, &mut |_| {});
        assert_eq!(
            s.state(),
            &LogState::Ended {
                error: None,
                pod_deleted: false
            }
        );
    }

    // covers UP-40: a container in CrashLoopBackOff is waited for as the kubelet's back-off.
    #[tokio::test(start_paused = true)]
    async fn a_crash_loop_waits_as_a_back_off() {
        let mut s = stream(&[], None);
        drain(&mut s).await;
        s.handle(Step::AfterEnd(Ok(AfterEnd::BackOff)), &mut |_| {});
        assert_eq!(
            s.state(),
            &LogState::Reconnecting {
                error: None,
                attempt: 1,
                back_off: true
            }
        );
        let mut starting = stream(&[], None);
        starting.handle(
            Step::Opened(Err(LogError::retry(
                "BadRequest: container \"app\" in pod \"p\" is waiting to start: CrashLoopBackOff",
            ))),
            &mut |_| {},
        );
        assert!(matches!(
            starting.state(),
            LogState::Reconnecting { back_off: true, .. }
        ));
        s.handle(Step::AfterEnd(Ok(AfterEnd::Restarts)), &mut |_| {});
        assert!(matches!(
            s.state(),
            LogState::Reconnecting {
                back_off: false,
                attempt: 2,
                ..
            }
        ));
    }

    /// The time left in a waiting stream's backoff.
    fn wait_left(s: &Stream) -> Duration {
        match &s.phase {
            Phase::Wait(sleep) => sleep.deadline() - tokio::time::Instant::now(),
            _ => panic!("not waiting"),
        }
    }

    fn attempt(s: &Stream) -> u32 {
        attempt_of(s.state())
    }

    // covers UP-40: the kubelet's "cannot read the log" answer is not progress, so a container
    // that keeps answering it is retried less and less often, like any other failure.
    #[tokio::test(start_paused = true)]
    async fn repeated_kubelet_answers_keep_backing_off() {
        let notice = "unable to retrieve container logs for containerd://c54f232e5b";
        let mut s = Stream::new(
            Arc::new(Fated {
                lines: vec![notice.to_owned()],
                fate: AfterEnd::Restarts,
            }),
            LogTarget {
                namespace: "ns".into(),
                pod: "p".into(),
                container: Some("c".into()),
            },
            LogOptions::default(),
            10,
            Backoff {
                first: Duration::from_secs(1),
                max: Duration::from_secs(30),
            },
            None,
        );
        drain(&mut s).await;
        assert_eq!((attempt(&s), wait_left(&s).as_secs()), (1, 1));
        for expected in [(2, 2), (3, 4)] {
            s.handle(Step::Retry, &mut |_| {});
            s.handle(Step::Opened(Ok(stream::pending().boxed())), &mut |_| {});
            let mut out = Vec::new();
            s.handle(
                Step::Line(Ok(RawLine {
                    text: notice.into(),
                    truncated: false,
                })),
                &mut |l| out.push(l),
            );
            assert!(out.is_empty(), "the answer is not shown as a line");
            s.handle(Step::AfterEnd(Ok(AfterEnd::Restarts)), &mut |_| {});
            assert_eq!((attempt(&s), wait_left(&s).as_secs()), expected);
            assert!(matches!(
                s.state(),
                LogState::Reconnecting { error: Some(e), .. } if e == notice
            ));
        }
    }

    // covers UP-45: a nudge skips the wait but keeps the count, so a container that fails again
    // right after restarting waits longer, not from the start.
    #[tokio::test(start_paused = true)]
    async fn a_nudge_keeps_the_attempt_count() {
        let mut s = stream(&[], None);
        s.handle(Step::Opened(Err(LogError::retry("down"))), &mut |_| {});
        s.handle(Step::Opened(Err(LogError::retry("down"))), &mut |_| {});
        assert_eq!((attempt(&s), wait_left(&s).as_secs()), (2, 2));
        s.nudge();
        assert_eq!(wait_left(&s), Duration::ZERO, "no wait left");
        s.handle(Step::Retry, &mut |_| {});
        s.handle(Step::Opened(Ok(stream::pending().boxed())), &mut |_| {});
        s.handle(Step::Line(Err(LogError::retry("reset"))), &mut |_| {});
        assert_eq!((attempt(&s), wait_left(&s).as_secs()), (3, 4));
    }

    // covers UP-45: failures in a row wait longer and longer; only a new line resets the count.
    #[tokio::test(start_paused = true)]
    async fn retries_back_off_until_a_line_arrives() {
        let mut s = stream(&[], None);
        let mut waits = Vec::new();
        for _ in 0..7 {
            s.handle(Step::Opened(Err(LogError::retry("down"))), &mut |_| {});
            waits.push((attempt(&s), wait_left(&s).as_secs()));
        }
        assert_eq!(
            waits,
            [(1, 1), (2, 2), (3, 4), (4, 8), (5, 16), (6, 30), (7, 30)]
        );

        // Opening again is not progress: the next failure counts on.
        s.handle(Step::Opened(Ok(stream::pending().boxed())), &mut |_| {});
        assert_eq!(s.state(), &LogState::Streaming);
        s.handle(Step::Line(Err(LogError::retry("reset"))), &mut |_| {});
        assert_eq!((attempt(&s), wait_left(&s).as_secs()), (8, 30));

        // A line is: the next failure is the first again.
        s.handle(Step::Opened(Ok(stream::pending().boxed())), &mut |_| {});
        let mut out = Vec::new();
        s.handle(
            Step::Line(Ok(RawLine {
                text: "news".into(),
                truncated: false,
            })),
            &mut |l| out.push(l),
        );
        assert_eq!(out.len(), 1);
        s.handle(Step::Line(Err(LogError::retry("reset"))), &mut |_| {});
        assert_eq!((attempt(&s), wait_left(&s).as_secs()), (1, 1));
        assert_eq!(
            s.state(),
            &LogState::Reconnecting {
                error: Some("reset".into()),
                attempt: 1,
                back_off: false,
            }
        );
    }

    // covers UP-45: a revived stream starts its backoff afresh.
    #[tokio::test(start_paused = true)]
    async fn a_revived_stream_backs_off_from_the_start() {
        let mut s = stream(&[], Some(Arc::new(LineSource::new("p", "c"))));
        s.handle(Step::Opened(Err(LogError::retry("down"))), &mut |_| {});
        s.handle(Step::Opened(Err(LogError::retry("down"))), &mut |_| {});
        assert_eq!(attempt(&s), 2);
        s.handle(
            Step::Opened(Err(LogError::fatal("gone for now"))),
            &mut |_| {},
        );
        assert!(s.is_done());
        s.revive();
        assert_eq!(s.state(), &LogState::Connecting);
        s.handle(Step::Opened(Err(LogError::retry("down"))), &mut |_| {});
        assert_eq!((attempt(&s), wait_left(&s).as_secs()), (1, 1));
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn a_pod_being_deleted_ends_its_session_with_a_reason_after_a_last_read() {
        let mut s = stream(&[], None);
        s.handle(Step::AfterEnd(Ok(AfterEnd::Deleting)), &mut |_| {});
        // The stopped container is read to its end once more before the session ends.
        assert!(!s.is_done() && matches!(s.phase, Phase::Connect(_)));
        s.handle(Step::Eof, &mut |_| {});
        let deleted = LogState::Ended {
            error: None,
            pod_deleted: true,
        };
        assert_eq!(s.state(), &deleted);

        // The pod went before that last read: the reason stays.
        let mut early = stream(&[], None);
        early.handle(Step::AfterEnd(Ok(AfterEnd::Deleting)), &mut |_| {});
        early.handle(Step::Opened(Err(LogError::fatal("not found"))), &mut |_| {});
        assert_eq!(early.state(), &deleted);

        // A workload's pod comes and goes: its stream ends quietly.
        let tag = Arc::new(LineSource::new("web-1", "app"));
        let mut member = stream(&[], Some(tag));
        member.handle(Step::AfterEnd(Ok(AfterEnd::Deleting)), &mut |_| {});
        member.handle(Step::Eof, &mut |_| {});
        assert_eq!(
            member.state(),
            &LogState::Ended {
                error: None,
                pod_deleted: false
            }
        );
    }

    // covers UP-40
    #[tokio::test(start_paused = true)]
    async fn a_pod_gone_before_a_reopen_was_deleted_not_missing() {
        let not_found = || {
            Step::Opened(Err(LogError {
                message: "NotFound: pods \"p\" not found".into(),
                kind: ErrorKind::NotFound,
            }))
        };
        let deleted = LogState::Ended {
            error: None,
            pod_deleted: true,
        };
        // Seen terminating while its container still ran, then gone by the reopen: the kubelet
        // ended the stream before the pod's status showed the container stopped.
        let mut s = stream(&[], None);
        s.handle(Step::AfterEnd(Ok(AfterEnd::Terminating)), &mut |_| {});
        assert!(matches!(s.state(), LogState::Reconnecting { .. }));
        s.handle(not_found(), &mut |_| {});
        assert_eq!(s.state(), &deleted);

        // Read once, then gone (deleted at once, between two reads).
        let mut read = stream(&[], None);
        read.handle(
            Step::Opened(Ok(futures::stream::pending().boxed())),
            &mut |_| {},
        );
        read.handle(Step::Line(Err(LogError::retry("EOF"))), &mut |_| {});
        read.handle(not_found(), &mut |_| {});
        assert_eq!(read.state(), &deleted);

        // Never found: the error stays.
        let mut never = stream(&[], None);
        never.handle(not_found(), &mut |_| {});
        assert!(matches!(
            never.state(),
            LogState::Ended {
                error: Some(_),
                pod_deleted: false
            }
        ));
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_cluster_error_ends_even_a_draining_stream_loudly() {
        let mut s = stream(&[], None);
        s.handle(Step::AfterEnd(Ok(AfterEnd::Finished)), &mut |_| {});
        let changed = s.handle(
            Step::Opened(Err(LogError::cluster("re-pointed"))),
            &mut |_| {},
        );
        assert!(changed);
        assert!(s.is_done() && s.ended_on_cluster_error());
        // A plain fatal error while draining is just the end.
        let mut d = stream(&[], None);
        d.handle(Step::AfterEnd(Ok(AfterEnd::Finished)), &mut |_| {});
        d.handle(Step::Opened(Err(LogError::fatal("gone"))), &mut |_| {});
        assert_eq!(
            d.state(),
            &LogState::Ended {
                error: None,
                pod_deleted: false
            }
        );
        assert!(!d.ended_on_cluster_error());
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_reopen_never_reads_a_pod_recreated_under_the_name() {
        let tag = Arc::new(LineSource::new("db-0", "db"));
        let mut s = stream(&[], Some(tag));
        // Every open (the first and each reconnect) names the pod's uid for the source to check.
        assert_eq!(s.first_request().uid.as_deref(), Some("u1"));
        assert_eq!(s.resume_request().uid.as_deref(), Some("u1"));
        assert_eq!(stream(&[], None).first_request().uid, None);
        // The source found another pod by that name: the stream ends without reading it.
        s.handle(Step::Line(Err(LogError::retry("reset"))), &mut |_| {});
        let changed = s.handle(
            Step::Opened(Err(LogError::pod_gone("replaced"))),
            &mut |_| {},
        );
        assert!(changed && s.is_done());
        assert_eq!(
            s.state(),
            &LogState::Ended {
                error: None,
                pod_deleted: false
            }
        );
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn an_ended_stream_keeps_no_line_text_alive() {
        let long = format!("2024-05-01T12:00:00Z {}", "x".repeat(1000));
        let mut s = stream_on(
            fixed(&[&long, &long], true),
            Some(Arc::new(LineSource::new("p", "c"))),
        );
        let lines = drain(&mut s).await;
        assert!(s.is_done());
        // Only the caller's copies hold the texts: resuming remembers fingerprints.
        assert!(lines.iter().all(|l| Arc::strong_count(&l.text) == 1));
        assert_eq!(s.last_group.len(), 2);
    }

    // covers UP-45
    #[tokio::test(start_paused = true)]
    async fn a_cluster_error_is_reported_as_such() {
        let mut s = stream(&[], None);
        s.handle(
            Step::Opened(Err(LogError::cluster("re-pointed"))),
            &mut |_| {},
        );
        assert!(s.is_done() && s.ended_on_cluster_error());
        assert_eq!(
            s.state(),
            &LogState::Ended {
                error: Some("re-pointed".into()),
                pod_deleted: false,
            }
        );
    }
}
