//! Runs a [`Store`] on its own thread, so disk work never lands on a caller's path.
//!
//! Everything a caller hands over goes through one bounded lock-free queue, and no caller ever
//! waits on it. What the queue has no room for is lost: its log sources are closed right away, so
//! they store no more text, and a marker in the queue carries the count. Safety does not depend
//! on that marker. Each row has its place in its stream, so a row that overtakes a lost one is
//! itself refused.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};
use std::thread::Thread;
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;
use tokio::sync::oneshot;

use super::identity::ConnectionIdentity;
use super::scrub::LogSource;
use super::store::{Batch, RecordingError, RecordingId, Retention, Store, now_ms};

/// How much batch data may wait for the writer. Past it, new batches are dropped and counted.
const MAX_QUEUED_BYTES: u64 = 16 * 1024 * 1024;
/// What a queued batch counts for beyond its rows, so empty batches cannot queue without limit.
const BATCH_OVERHEAD: u64 = 128;
/// How many batches, calls and loss markers may wait. Each has its own share of the queue, so a
/// flood of batches cannot take the room a marker needs.
const MAX_QUEUED_BATCHES: usize = 4096;
const MAX_QUEUED_CALLS: usize = 256;
const MAX_QUEUED_MARKERS: usize = 256;
/// One stop from [`Recorder::shutdown`] and one from dropping it.
const STOP_SLOTS: usize = 2;
const QUEUE_SLOTS: usize = MAX_QUEUED_BATCHES + MAX_QUEUED_CALLS + MAX_QUEUED_MARKERS + STOP_SLOTS;
/// How often retention runs.
const RETENTION_EVERY: Duration = Duration::from_secs(60);

type StoreCall = Box<dyn FnOnce(&mut Store) + Send>;
type Stopped = oneshot::Sender<Result<(), FlushError>>;

struct Queued {
    id: RecordingId,
    current: ConnectionIdentity,
    batch: Batch,
    cost: u64,
}

enum Message {
    Append(Box<Queued>),
    /// A batch submitted about here was lost.
    Lost(Box<Loss>),
    Call(StoreCall),
    /// Finish what is queued and stop, answering once the database is closed.
    Stop(Option<Stopped>),
}

/// What one lost batch held.
struct Loss {
    id: RecordingId,
    rows: u64,
    /// The log sources it had rows of, each already closed.
    sources: Vec<LogSource>,
}

impl Loss {
    /// Closes each source at its first lost row.
    fn of(id: RecordingId, batch: &Batch) -> Self {
        let mut sources: Vec<LogSource> = Vec::new();
        for line in &batch.lines {
            line.source.close_at(line.seq);
            line.source.count_lost(1);
            sources.push(line.source.clone());
        }
        sources.sort_unstable_by_key(LogSource::key);
        sources.dedup_by_key(|source| source.key());
        Loss {
            id,
            rows: batch.rows(),
            sources,
        }
    }
}

/// The queue and its accounting. Only atomics, so a caller never waits for the writer.
struct Shared {
    queue: ArrayQueue<Message>,
    queued_bytes: AtomicU64,
    queued_batches: AtomicUsize,
    queued_calls: AtomicUsize,
    queued_markers: AtomicUsize,
    /// Rows of losses that had no room for a marker. Which recording they were of is not known,
    /// so they are counted on every running one. Their sources are closed already.
    unplaced_rows: AtomicU64,
    /// The writer has stopped taking messages.
    closed: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Shared {
            queue: ArrayQueue::new(QUEUE_SLOTS),
            queued_bytes: AtomicU64::new(0),
            queued_batches: AtomicUsize::new(0),
            queued_calls: AtomicUsize::new(0),
            queued_markers: AtomicUsize::new(0),
            unplaced_rows: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }

    /// Drops what is queued, which answers any waiting caller with an error.
    fn drain(&self) {
        while self.queue.pop().is_some() {}
    }
}

/// Marks the writer stopped however its thread ends.
struct Exit(Arc<Shared>);

impl Drop for Exit {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        self.0.drain();
    }
}

/// Takes one of `max` slots.
fn reserve(slots: &AtomicUsize, max: usize) -> bool {
    let mut taken = slots.load(Ordering::Acquire);
    while taken < max {
        match slots.compare_exchange_weak(taken, taken + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(now) => taken = now,
        }
    }
    false
}

fn release(slots: &AtomicUsize) {
    slots.fetch_sub(1, Ordering::AcqRel);
}

/// Whether a batch was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    Queued,
    /// The queue was full. The rows are counted on the recording.
    Dropped,
}

/// Why a call did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecorderError {
    #[error("the recordings writer has stopped")]
    Closed,
    #[error("the recordings writer is busy")]
    Busy,
}

/// The queue was not fully saved when the writer stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FlushError {
    #[error(
        "{rows} recorded rows could not be saved, and {uncounted} dropped rows were not counted"
    )]
    Unsaved {
        /// Accepted rows that are not in the database.
        rows: u64,
        /// Dropped rows missing from their recording's count.
        uncounted: u64,
    },
    #[error("the recordings writer stopped before finishing")]
    WriterStopped,
}

/// The running writer. Dropping it never waits: the thread finishes the queue by itself. Use
/// [`Self::shutdown`] to wait for that.
pub struct Recorder {
    shared: Arc<Shared>,
    writer: Thread,
}

impl Recorder {
    /// Opens the store in `dir` on a new thread, which then owns it.
    pub fn open(
        dir: PathBuf,
        retention: Retention,
    ) -> impl Future<Output = Result<Self, RecordingError>> + Send + 'static {
        let shared = Arc::new(Shared::new());
        let (ready, opened) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("flint-recordings".to_owned())
            .spawn({
                let shared = shared.clone();
                move || {
                    let _exit = Exit(shared.clone());
                    let mut store = match Store::open(&dir, retention) {
                        Ok(store) => {
                            let _ = ready.send(Ok(()));
                            store
                        }
                        Err(error) => {
                            let _ = ready.send(Err(error));
                            return;
                        }
                    };
                    let (stopped, rows) = run(&mut store, &shared);
                    settle_unplaced(&mut store, &shared);
                    store.flush_dropped_counts();
                    let uncounted =
                        store.uncounted_drops() + shared.unplaced_rows.load(Ordering::Acquire);
                    drop(store);
                    let result = match (rows, uncounted) {
                        (0, 0) => Ok(()),
                        (rows, uncounted) => Err(FlushError::Unsaved { rows, uncounted }),
                    };
                    match stopped {
                        Some(stopped) => {
                            let _ = stopped.send(result);
                        }
                        None if result.is_err() => {
                            tracing::warn!(rows, uncounted, "recorded rows could not be saved");
                        }
                        None => {}
                    }
                }
            })
            .map(|handle| handle.thread().clone());
        async move {
            let writer = spawned?;
            opened
                .await
                .map_err(|_| std::io::Error::other("the recordings writer stopped"))??;
            Ok(Recorder { shared, writer })
        }
    }

    /// Queues rows for a recording. Never blocks and never touches the disk.
    pub fn submit(&self, id: RecordingId, current: &ConnectionIdentity, batch: Batch) -> Submitted {
        let cost = batch.queued_bytes() + BATCH_OVERHEAD;
        if !self.reserve_batch(cost) {
            self.note_lost(id, &batch);
            return Submitted::Dropped;
        }
        let message = Message::Append(Box::new(Queued {
            id,
            current: current.clone(),
            batch,
            cost,
        }));
        match self.push(message) {
            Ok(()) => Submitted::Queued,
            Err(message) => {
                self.shared.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
                release(&self.shared.queued_batches);
                if let Message::Append(queued) = message {
                    self.note_lost(id, &queued.batch);
                }
                Submitted::Dropped
            }
        }
    }

    /// Runs `call` on the writer thread, after everything queued before it. Refused, without
    /// waiting, while too many calls are queued.
    pub fn call<R: Send + 'static>(
        &self,
        call: impl FnOnce(&mut Store) -> R + Send + 'static,
    ) -> impl Future<Output = Result<R, RecorderError>> + Send + 'static {
        let (reply, answer) = oneshot::channel();
        let sent = if reserve(&self.shared.queued_calls, MAX_QUEUED_CALLS) {
            let message = Message::Call(Box::new(move |store| {
                let _ = reply.send(call(store));
            }));
            self.push(message).map_err(|_| {
                release(&self.shared.queued_calls);
                RecorderError::Closed
            })
        } else {
            Err(RecorderError::Busy)
        };
        async move {
            sent?;
            answer.await.map_err(|_| RecorderError::Closed)
        }
    }

    /// Finishes the queue and closes the database. Reports rows it accepted but could not save.
    pub fn shutdown(self) -> impl Future<Output = Result<(), FlushError>> + Send + 'static {
        let (stopped, done) = oneshot::channel();
        let sent = self.push(Message::Stop(Some(stopped))).map_err(drop);
        async move {
            sent.map_err(|()| FlushError::WriterStopped)?;
            done.await.unwrap_or(Err(FlushError::WriterStopped))
        }
    }

    /// Queues `message` and wakes the writer, without taking a lock.
    fn push(&self, message: Message) -> Result<(), Message> {
        let shared = &self.shared;
        if shared.closed.load(Ordering::SeqCst) {
            return Err(message);
        }
        shared.queue.push(message)?;
        fence(Ordering::SeqCst);
        if shared.closed.load(Ordering::SeqCst) {
            shared.drain();
        }
        self.writer.unpark();
        Ok(())
    }

    fn reserve_batch(&self, cost: u64) -> bool {
        let shared = &self.shared;
        if !reserve(&shared.queued_batches, MAX_QUEUED_BATCHES) {
            return false;
        }
        let queued = shared.queued_bytes.fetch_add(cost, Ordering::AcqRel);
        if queued + cost > MAX_QUEUED_BYTES {
            shared.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
            release(&shared.queued_batches);
            return false;
        }
        true
    }

    /// Closes the batch's sources, then says how many rows were lost.
    fn note_lost(&self, id: RecordingId, batch: &Batch) {
        let shared = &self.shared;
        let loss = Box::new(Loss::of(id, batch));
        if reserve(&shared.queued_markers, MAX_QUEUED_MARKERS) {
            if self.push(Message::Lost(loss)).is_ok() {
                return;
            }
            release(&shared.queued_markers);
        }
        shared
            .unplaced_rows
            .fetch_add(batch.rows(), Ordering::AcqRel);
        self.writer.unpark();
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.push(Message::Stop(None));
    }
}

/// Runs until stopped: who to answer, and how many accepted rows could not be saved.
fn run(store: &mut Store, shared: &Arc<Shared>) -> (Option<Stopped>, u64) {
    let mut last_retention = Instant::now();
    let mut unsaved = 0u64;
    loop {
        settle_unplaced(store, shared);
        if last_retention.elapsed() >= RETENTION_EVERY {
            last_retention = Instant::now();
            if let Err(error) = store.apply_retention(now_ms()) {
                tracing::warn!(%error, "recordings retention failed");
            }
        }
        let Some(message) = shared.queue.pop() else {
            // Only this thread is left: the open was abandoned before a recorder existed.
            if Arc::strong_count(shared) <= 2 && shared.queue.is_empty() {
                return (None, unsaved);
            }
            std::thread::park_timeout(RETENTION_EVERY.saturating_sub(last_retention.elapsed()));
            continue;
        };
        match message {
            Message::Append(queued) => {
                let Queued {
                    id,
                    current,
                    batch,
                    cost,
                } = *queued;
                shared.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
                release(&shared.queued_batches);
                unsaved += append(store, id, &current, batch);
            }
            Message::Lost(loss) => {
                release(&shared.queued_markers);
                store.record_loss(loss.id, loss.rows, &loss.sources);
            }
            Message::Call(call) => {
                release(&shared.queued_calls);
                call(store);
            }
            Message::Stop(stopped) => return (stopped, unsaved),
        }
    }
}

/// Stores one batch; how many of its rows could not be saved.
fn append(store: &mut Store, id: RecordingId, current: &ConnectionIdentity, batch: Batch) -> u64 {
    let rows = batch.rows();
    match store.append_counted(id, current, batch) {
        Ok((done, failure)) => {
            if let Some(error) = failure {
                tracing::debug!(%error, "recording rows not stored");
            }
            done.unsaved
        }
        Err(error) => {
            tracing::debug!(%error, "recording rows not stored");
            if error.is_storage() {
                store.count_dropped(id, rows);
                rows
            } else {
                0
            }
        }
    }
}

/// Counts the rows of losses that had no room for a marker.
fn settle_unplaced(store: &mut Store, shared: &Shared) {
    let rows = shared.unplaced_rows.swap(0, Ordering::AcqRel);
    if rows > 0
        && let Err(error) = store.count_on_running(rows)
    {
        tracing::warn!(%error, "cannot count dropped recording rows");
        shared.unplaced_rows.fetch_add(rows, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::{DATABASE_FILE, RecordingTarget};
    use super::*;

    fn batch(source: &LogSource, text: &str) -> Batch {
        Batch {
            lines: vec![source.row(1, text.into(), false)],
            ..Batch::default()
        }
    }

    fn source() -> LogSource {
        LogSource::new("p", "c")
    }

    /// A line too big for the queue, ending in `tail`.
    fn oversized(source: &LogSource, tail: &str) -> Batch {
        let mut text = "x".repeat(MAX_QUEUED_BYTES as usize);
        text.push_str(tail);
        batch(source, &text)
    }

    async fn started(dir: &std::path::Path) -> (Recorder, RecordingId, ConnectionIdentity) {
        let identity = ConnectionIdentity::for_tests(1);
        let recorder = Recorder::open(dir.to_owned(), Retention::default())
            .await
            .unwrap();
        let on = identity.clone();
        let id = recorder
            .call(move |store| {
                store.set_enabled(&on, "dev", true, 0).unwrap();
                let target = RecordingTarget::Pod { name: "p".into() };
                store.start(&on, "ns", &target, 0).unwrap()
            })
            .await
            .unwrap();
        (recorder, id, identity)
    }

    fn texts(store: &Store) -> Vec<String> {
        store
            .connection()
            .prepare("SELECT text FROM log_lines ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    async fn stored(recorder: &Recorder) -> Vec<String> {
        recorder.call(|store| texts(store)).await.unwrap()
    }

    /// Holds the writer until the returned sender is dropped.
    async fn stalled(recorder: &Recorder) -> std::sync::mpsc::Sender<()> {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, running) = oneshot::channel();
        drop(recorder.call(move |_| {
            let _ = entered.send(());
            let _ = wait.recv();
        }));
        running.await.unwrap();
        release
    }

    #[tokio::test]
    async fn a_batch_is_written_off_the_callers_thread() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let s = source();
        assert_eq!(
            recorder.submit(id, &identity, batch(&s, "hello")),
            Submitted::Queued
        );
        let (lines, thread) = recorder
            .call(|store| {
                (
                    texts(store),
                    std::thread::current().name().map(str::to_owned),
                )
            })
            .await
            .unwrap();
        assert_eq!(lines, ["hello"]);
        assert_eq!(thread.as_deref(), Some("flint-recordings"));
    }

    #[tokio::test]
    async fn a_store_that_cannot_open_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(Recorder::open(file, Retention::default()).await.is_err());
    }

    #[tokio::test]
    async fn a_batch_over_the_queue_budget_is_dropped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        assert_eq!(
            recorder.submit(id, &identity, oversized(&source(), "x")),
            Submitted::Dropped
        );
        recorder.call(|_| ()).await.unwrap();
        let info = recorder.call(move |s| s.info(id).unwrap()).await.unwrap();
        assert_eq!(info.dropped_rows, 1);
    }

    #[tokio::test]
    async fn a_dropped_row_stops_only_its_source_until_it_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let (lossy, other) = (source(), LogSource::new("q", "c"));
        assert_eq!(
            recorder.submit(id, &identity, oversized(&lossy, "")),
            Submitted::Dropped
        );
        recorder.submit(id, &identity, batch(&other, "from another pod"));
        recorder.submit(id, &identity, batch(&lossy, "SECRETBODY"));
        recorder.submit(id, &identity, batch(&source(), "after a restart"));
        let lines = stored(&recorder).await;
        assert!(lines.iter().all(|l| !l.contains("SECRETBODY")));
        assert!(lines.iter().any(|l| l == "from another pod"));
        assert!(lines.iter().any(|l| l == "after a restart"));
        assert!(lines.iter().any(|l| l.contains("2 lines not recorded")));
    }

    #[tokio::test]
    async fn a_token_value_after_a_dropped_key_row_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let s = source();
        let key = oversized(&s, " \"token\":");
        assert_eq!(recorder.submit(id, &identity, key), Submitted::Dropped);
        recorder.submit(id, &identity, batch(&s, "\"SECRETVALUE\""));
        assert!(
            stored(&recorder)
                .await
                .iter()
                .all(|l| !l.contains("SECRETVALUE"))
        );
    }

    #[tokio::test]
    async fn a_body_racing_a_dropped_opener_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let s = source();
        // Made in stream order, but the body reaches the queue before the opener's loss does.
        let opener = oversized(&s, "-----BEGIN PRIVATE KEY-----");
        let body = batch(&s, "SECRETBODY");
        recorder.submit(id, &identity, body);
        assert_eq!(recorder.submit(id, &identity, opener), Submitted::Dropped);
        assert!(
            stored(&recorder)
                .await
                .iter()
                .all(|l| !l.contains("SECRETBODY"))
        );
    }

    #[tokio::test]
    async fn concurrent_bodies_never_pass_a_dropped_opener() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let recorder = Arc::new(recorder);
        let s = source();
        let opener = oversized(&s, "-----BEGIN PRIVATE KEY-----");
        let bodies: Vec<_> = (0..64).map(|_| batch(&s, "SECRETBODY")).collect();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let racing = {
            let (recorder, identity, barrier) =
                (recorder.clone(), identity.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for body in bodies {
                    recorder.submit(id, &identity, body);
                }
            })
        };
        barrier.wait();
        recorder.submit(id, &identity, opener);
        racing.join().unwrap();
        assert!(
            stored(&recorder)
                .await
                .iter()
                .all(|l| !l.contains("SECRETBODY"))
        );
    }

    #[tokio::test]
    async fn a_dropped_certificate_opener_withholds_its_body() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let s = source();
        let opener = oversized(&s, "-----BEGIN CERTIFICATE-----");
        assert_eq!(recorder.submit(id, &identity, opener), Submitted::Dropped);
        recorder.submit(id, &identity, batch(&s, "SECRETBODY"));
        assert!(
            stored(&recorder)
                .await
                .iter()
                .all(|l| !l.contains("SECRETBODY"))
        );
    }

    #[tokio::test]
    async fn an_older_key_end_does_not_clear_a_newer_loss() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let s = source();
        let release = stalled(&recorder).await;
        recorder.submit(id, &identity, batch(&s, "-----BEGIN PRIVATE KEY-----"));
        recorder.submit(id, &identity, batch(&s, "-----END PRIVATE KEY-----"));
        let dropped = oversized(&s, "-----BEGIN PRIVATE KEY-----");
        assert_eq!(recorder.submit(id, &identity, dropped), Submitted::Dropped);
        recorder.submit(id, &identity, batch(&s, "MIIEnewerkeybody"));
        drop(release);
        assert!(
            stored(&recorder)
                .await
                .iter()
                .all(|l| !l.contains("MIIEnewerkeybody"))
        );
    }

    #[tokio::test]
    async fn marker_saturation_neither_releases_nor_closes_another_source() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let (lossy, other) = (source(), LogSource::new("q", "c"));
        let release = stalled(&recorder).await;
        for _ in 0..=MAX_QUEUED_MARKERS {
            recorder.submit(id, &identity, oversized(&lossy, ""));
        }
        assert!(recorder.shared.unplaced_rows.load(Ordering::Acquire) > 0);
        recorder.submit(id, &identity, batch(&lossy, "SECRETBODY"));
        recorder.submit(id, &identity, batch(&other, "still recorded"));
        drop(release);
        let lines = stored(&recorder).await;
        assert!(lines.iter().all(|l| !l.contains("SECRETBODY")));
        assert!(lines.iter().any(|l| l == "still recorded"));
        let info = recorder.call(move |s| s.info(id).unwrap()).await.unwrap();
        assert!(info.dropped_rows > MAX_QUEUED_MARKERS as u64);
    }

    #[tokio::test]
    async fn a_stalled_writer_keeps_every_queue_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let s = source();
        let release = stalled(&recorder).await;
        for _ in 0..MAX_QUEUED_BATCHES {
            assert_eq!(
                recorder.submit(id, &identity, batch(&s, "x")),
                Submitted::Queued
            );
        }
        assert_eq!(
            recorder.submit(id, &identity, batch(&s, "x")),
            Submitted::Dropped
        );
        let calls: Vec<_> = (0..MAX_QUEUED_CALLS)
            .map(|_| recorder.call(|_| ()))
            .collect();
        assert_eq!(recorder.call(|_| ()).await, Err(RecorderError::Busy));
        drop(release);
        for call in calls {
            call.await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_queue_budget_charges_reserved_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let empty_but_huge = Batch {
            lines: Vec::with_capacity(MAX_QUEUED_BYTES as usize / 16),
            ..Batch::default()
        };
        assert_eq!(
            recorder.submit(id, &identity, empty_but_huge),
            Submitted::Dropped
        );
    }

    #[tokio::test]
    async fn a_final_batch_that_cannot_be_written_is_reported_with_its_count() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        recorder
            .call(|store| store.set_busy_timeout(Duration::from_millis(1)))
            .await
            .unwrap();
        let blocker = rusqlite::Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        recorder.submit(id, &identity, batch(&source(), "last"));
        assert_eq!(
            recorder.shutdown().await,
            Err(FlushError::Unsaved {
                rows: 1,
                uncounted: 1
            })
        );
        blocker.execute_batch("ROLLBACK").unwrap();
    }

    #[tokio::test]
    async fn the_queue_budget_is_released_after_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        recorder.submit(id, &identity, batch(&source(), "hello"));
        recorder.call(|_| ()).await.unwrap();
        assert_eq!(recorder.shared.queued_bytes.load(Ordering::Acquire), 0);
        assert_eq!(recorder.shared.queued_batches.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn shutdown_finishes_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        recorder.submit(id, &identity, batch(&source(), "last"));
        recorder.shutdown().await.unwrap();
        let store = Store::open(dir.path(), Retention::default()).unwrap();
        assert_eq!(texts(&store), ["last"]);
    }
}
