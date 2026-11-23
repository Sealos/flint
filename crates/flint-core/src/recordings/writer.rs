//! Runs a [`Store`] on its own thread, so disk work never lands on a caller's path.
//!
//! Everything a caller hands over goes through one bounded queue, in order, and no caller ever
//! waits on it. What the queue has no room for is lost and said so: as a marker in the same queue
//! when there is room for one, so the store learns of the loss at the place it happened.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::identity::ConnectionIdentity;
use super::scrub::may_open_block;
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
/// How many sources one marker names, and the longest name it keeps. Past either, the whole
/// recording is withheld instead.
const MAX_MARKER_SOURCES: usize = 32;
const MAX_SOURCE_NAME_BYTES: usize = 1024;
/// How often retention runs.
const RETENTION_EVERY: Duration = Duration::from_secs(60);

type StoreCall = Box<dyn FnOnce(&mut Store) + Send>;
type Stopped = oneshot::Sender<Result<(), FlushError>>;

enum Message {
    Append {
        id: RecordingId,
        current: ConnectionIdentity,
        batch: Batch,
        cost: u64,
    },
    /// A batch submitted at this point in the queue was lost.
    Lost(Loss),
    Call(StoreCall),
    /// Finish what is queued and stop, answering once the database is closed.
    Stop(Option<Stopped>),
}

/// What one lost batch held.
struct Loss {
    id: RecordingId,
    rows: u64,
    /// Pod and container of each lost line that may have opened a block.
    sources: BTreeSet<(String, String)>,
    /// Too many sources, or names too long, to keep: withhold the whole recording.
    seal: bool,
}

impl Loss {
    fn of(id: RecordingId, batch: &Batch) -> Self {
        let mut loss = Loss {
            id,
            rows: batch.rows(),
            sources: BTreeSet::new(),
            seal: false,
        };
        // A cut line may have lost an opening that nothing left in it shows.
        for line in batch
            .lines
            .iter()
            .filter(|line| line.truncated || may_open_block(&line.text))
        {
            if line.pod.len() > MAX_SOURCE_NAME_BYTES
                || line.container.len() > MAX_SOURCE_NAME_BYTES
                || loss.sources.len() >= MAX_MARKER_SOURCES
            {
                loss.seal = true;
                loss.sources.clear();
                break;
            }
            loss.sources
                .insert((line.pod.clone(), line.container.clone()));
        }
        loss
    }
}

/// The queue's accounting. Only atomics, so a caller never waits for the writer.
#[derive(Default)]
struct Shared {
    queued_bytes: AtomicU64,
    queued_batches: AtomicUsize,
    queued_calls: AtomicUsize,
    queued_markers: AtomicUsize,
    /// A loss had no room for a marker, so its place in the queue is unknown: every running
    /// recording is withheld. Being permanent, that holds wherever the loss really was.
    seal_all: AtomicBool,
    /// The rows of those losses.
    unnamed_rows: AtomicU64,
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
    #[error("{0} recorded rows could not be saved")]
    Unsaved(u64),
    #[error("the recordings writer stopped before finishing")]
    WriterStopped,
}

/// The running writer. Dropping it never waits: the thread finishes the queue by itself. Use
/// [`Self::shutdown`] to wait for that.
pub struct Recorder {
    tx: SyncSender<Message>,
    shared: Arc<Shared>,
}

impl Recorder {
    /// Opens the store in `dir` on a new thread, which then owns it.
    pub fn open(
        dir: PathBuf,
        retention: Retention,
    ) -> impl Future<Output = Result<Self, RecordingError>> + Send + 'static {
        let (tx, rx) = mpsc::sync_channel(QUEUE_SLOTS);
        let shared = Arc::new(Shared::default());
        let (ready, opened) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("flint-recordings".to_owned())
            .spawn({
                let shared = shared.clone();
                move || {
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
                    let (stopped, unsaved) = run(&mut store, &rx, &shared);
                    store.flush_dropped_counts();
                    drop(store);
                    let result = match unsaved {
                        0 => Ok(()),
                        rows => Err(FlushError::Unsaved(rows)),
                    };
                    match stopped {
                        Some(stopped) => {
                            let _ = stopped.send(result);
                        }
                        None if unsaved > 0 => {
                            tracing::warn!(unsaved, "recorded rows could not be saved");
                        }
                        None => {}
                    }
                }
            })
            .map(drop);
        async move {
            spawned?;
            opened
                .await
                .map_err(|_| std::io::Error::other("the recordings writer stopped"))??;
            Ok(Recorder { tx, shared })
        }
    }

    /// Queues rows for a recording. Never blocks and never touches the disk.
    pub fn submit(&self, id: RecordingId, current: &ConnectionIdentity, batch: Batch) -> Submitted {
        let cost = batch.queued_bytes() + BATCH_OVERHEAD;
        if !self.reserve_batch(cost) {
            self.note_lost(id, &batch);
            return Submitted::Dropped;
        }
        let message = Message::Append {
            id,
            current: current.clone(),
            batch,
            cost,
        };
        match self.tx.try_send(message) {
            Ok(()) => Submitted::Queued,
            Err(TrySendError::Full(message) | TrySendError::Disconnected(message)) => {
                self.shared.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
                release(&self.shared.queued_batches);
                if let Message::Append { batch, .. } = message {
                    self.note_lost(id, &batch);
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
            self.tx.try_send(message).map_err(|_| {
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
        let sent = self.tx.try_send(Message::Stop(Some(stopped)));
        async move {
            sent.map_err(|_| FlushError::WriterStopped)?;
            done.await.unwrap_or(Err(FlushError::WriterStopped))
        }
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

    /// Says a batch was lost: at this place in the queue, or for every running recording.
    fn note_lost(&self, id: RecordingId, batch: &Batch) {
        let shared = &self.shared;
        if reserve(&shared.queued_markers, MAX_QUEUED_MARKERS) {
            match self.tx.try_send(Message::Lost(Loss::of(id, batch))) {
                Ok(()) => return,
                Err(_) => release(&shared.queued_markers),
            }
        }
        shared
            .unnamed_rows
            .fetch_add(batch.rows(), Ordering::AcqRel);
        shared.seal_all.store(true, Ordering::Release);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Message::Stop(None));
    }
}

/// Runs until stopped: who to answer, and how many accepted rows could not be saved.
fn run(store: &mut Store, rx: &mpsc::Receiver<Message>, shared: &Shared) -> (Option<Stopped>, u64) {
    let mut last_retention = Instant::now();
    let mut unsaved = 0u64;
    loop {
        let wait = RETENTION_EVERY.saturating_sub(last_retention.elapsed());
        let message = rx.recv_timeout(wait);
        let settled = settle_unnamed(store, shared);
        match message {
            Ok(Message::Append {
                id,
                current,
                batch,
                cost,
            }) => {
                shared.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
                release(&shared.queued_batches);
                unsaved += append(store, id, &current, batch, settled);
            }
            Ok(Message::Lost(loss)) => {
                release(&shared.queued_markers);
                apply_loss(store, &loss);
            }
            Ok(Message::Call(call)) => {
                release(&shared.queued_calls);
                call(store);
            }
            Ok(Message::Stop(stopped)) => return (stopped, unsaved),
            Err(RecvTimeoutError::Disconnected) => return (None, unsaved),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if last_retention.elapsed() >= RETENTION_EVERY {
            last_retention = Instant::now();
            if let Err(error) = store.apply_retention(now_ms()) {
                tracing::warn!(%error, "recordings retention failed");
            }
        }
    }
}

/// Stores one batch; how many of its rows could not be saved. Until an unnamed loss is settled
/// nothing can say which lines are safe, so none are stored.
fn append(
    store: &mut Store,
    id: RecordingId,
    current: &ConnectionIdentity,
    batch: Batch,
    settled: bool,
) -> u64 {
    let rows = batch.rows();
    if !settled {
        store.count_dropped(id, rows);
        return rows;
    }
    let unsaved = match store.append_counted(id, current, batch) {
        Ok((_, None)) => 0,
        Ok((done, Some(error))) => {
            tracing::debug!(%error, "recording rows not stored");
            if error.is_storage() {
                rows.saturating_sub(done.stored + done.dropped)
            } else {
                0
            }
        }
        Err(error) => {
            tracing::debug!(%error, "recording rows not stored");
            if error.is_storage() { rows } else { 0 }
        }
    };
    if unsaved > 0 {
        store.count_dropped(id, unsaved);
    }
    unsaved
}

fn apply_loss(store: &mut Store, loss: &Loss) {
    for (pod, container) in &loss.sources {
        store.line_lost(loss.id, pod, container);
    }
    if loss.seal {
        store.seal_lines(loss.id);
    }
    if loss.rows > 0 {
        store.count_dropped(loss.id, loss.rows);
    }
}

/// Withholds every running recording if a loss could not be placed in the queue. False while
/// that could not be done.
fn settle_unnamed(store: &mut Store, shared: &Shared) -> bool {
    if !shared.seal_all.swap(false, Ordering::AcqRel) {
        return true;
    }
    let rows = shared.unnamed_rows.swap(0, Ordering::AcqRel);
    match store.seal_running(rows) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "cannot withhold running recordings");
            shared.unnamed_rows.fetch_add(rows, Ordering::AcqRel);
            shared.seal_all.store(true, Ordering::Release);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::{DATABASE_FILE, LogRow, RecordingTarget};
    use super::*;

    fn line(text: &str) -> LogRow {
        LogRow {
            at_ms: 1,
            pod: "p".into(),
            container: "c".into(),
            text: text.into(),
            truncated: false,
        }
    }

    fn batch(text: &str) -> Batch {
        Batch {
            lines: vec![line(text)],
            ..Batch::default()
        }
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

    #[tokio::test]
    async fn a_batch_is_written_off_the_callers_thread() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        assert_eq!(
            recorder.submit(id, &identity, batch("hello")),
            Submitted::Queued
        );
        let (stored, thread) = recorder
            .call(|store| {
                (
                    texts(store),
                    std::thread::current().name().map(str::to_owned),
                )
            })
            .await
            .unwrap();
        assert_eq!(stored, ["hello"]);
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
        let huge = "x".repeat(MAX_QUEUED_BYTES as usize + 1);
        assert_eq!(
            recorder.submit(id, &identity, batch(&huge)),
            Submitted::Dropped
        );
        recorder.call(|_| ()).await.unwrap();
        let info = recorder.call(move |s| s.info(id).unwrap()).await.unwrap();
        assert_eq!(info.dropped_rows, 1);
    }

    #[tokio::test]
    async fn lines_after_a_dropped_key_header_are_withheld_for_that_source() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let mut dropped = "x".repeat(MAX_QUEUED_BYTES as usize);
        dropped.push_str("-----BEGIN PRIVATE KEY-----");
        assert_eq!(
            recorder.submit(id, &identity, batch(&dropped)),
            Submitted::Dropped
        );
        let mut other = batch("from another pod");
        other.lines[0].pod = "q".into();
        recorder.submit(id, &identity, other);
        recorder.submit(id, &identity, batch("MIIEkeybody"));
        let stored = recorder.call(|store| texts(store)).await.unwrap();
        assert_eq!(stored, ["from another pod", "[REDACTED:private-key]"]);
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
    async fn an_older_key_end_does_not_clear_a_newer_lost_header() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let release = stalled(&recorder).await;
        recorder.submit(id, &identity, batch("-----BEGIN PRIVATE KEY-----"));
        recorder.submit(id, &identity, batch("-----END PRIVATE KEY-----"));
        let mut dropped = "x".repeat(MAX_QUEUED_BYTES as usize);
        dropped.push_str("-----BEGIN PRIVATE KEY-----");
        assert_eq!(
            recorder.submit(id, &identity, batch(&dropped)),
            Submitted::Dropped
        );
        recorder.submit(id, &identity, batch("MIIEnewerkeybody"));
        drop(release);
        let stored = recorder.call(|store| texts(store)).await.unwrap();
        assert!(stored.iter().all(|l| !l.contains("MIIEnewerkeybody")));
    }

    #[tokio::test]
    async fn a_dropped_cut_line_withholds_what_follows() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let mut dropped = batch(&"x".repeat(MAX_QUEUED_BYTES as usize + 1));
        dropped.lines[0].truncated = true;
        assert_eq!(recorder.submit(id, &identity, dropped), Submitted::Dropped);
        recorder.submit(id, &identity, batch("MIIEkeybody"));
        let stored = recorder.call(|store| texts(store)).await.unwrap();
        assert!(stored.iter().all(|l| !l.contains("MIIEkeybody")));
    }

    #[tokio::test]
    async fn a_loss_with_no_room_for_its_marker_withholds_running_recordings() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let release = stalled(&recorder).await;
        let huge = "x".repeat(MAX_QUEUED_BYTES as usize + 1);
        for _ in 0..=MAX_QUEUED_MARKERS {
            recorder.submit(id, &identity, batch(&huge));
        }
        assert!(recorder.shared.seal_all.load(Ordering::Acquire));
        recorder.submit(id, &identity, batch("MIIEkeybody"));
        drop(release);
        let stored = recorder.call(|store| texts(store)).await.unwrap();
        assert!(stored.iter().all(|l| !l.contains("MIIEkeybody")));
        let info = recorder.call(move |s| s.info(id).unwrap()).await.unwrap();
        assert!(info.dropped_rows > MAX_QUEUED_MARKERS as u64);
    }

    #[tokio::test]
    async fn a_stalled_writer_keeps_every_queue_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        let release = stalled(&recorder).await;
        for _ in 0..MAX_QUEUED_BATCHES {
            assert_eq!(
                recorder.submit(id, &identity, batch("x")),
                Submitted::Queued
            );
        }
        assert_eq!(
            recorder.submit(id, &identity, batch("x")),
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
    async fn a_final_batch_that_cannot_be_written_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        recorder
            .call(|store| store.set_busy_timeout(Duration::from_millis(1)))
            .await
            .unwrap();
        let blocker = rusqlite::Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        recorder.submit(id, &identity, batch("last"));
        assert_eq!(recorder.shutdown().await, Err(FlushError::Unsaved(1)));
        blocker.execute_batch("ROLLBACK").unwrap();
    }

    #[tokio::test]
    async fn the_queue_budget_is_released_after_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        recorder.submit(id, &identity, batch("hello"));
        recorder.call(|_| ()).await.unwrap();
        assert_eq!(recorder.shared.queued_bytes.load(Ordering::Acquire), 0);
        assert_eq!(recorder.shared.queued_batches.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn shutdown_finishes_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        recorder.submit(id, &identity, batch("last"));
        recorder.shutdown().await.unwrap();
        let store = Store::open(dir.path(), Retention::default()).unwrap();
        assert_eq!(texts(&store), ["last"]);
    }
}
