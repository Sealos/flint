//! Runs a [`Store`] on its own thread, so disk work never lands on a caller's path.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::identity::ConnectionIdentity;
use super::scrub::may_open_block;
use super::store::{Batch, RecordingError, RecordingId, Retention, Store, now_ms};

/// How much batch data may wait for the writer. Past it, new batches are dropped and counted.
const MAX_QUEUED_BYTES: u64 = 16 * 1024 * 1024;
/// What a queued batch counts for beyond its rows, so empty batches cannot queue without limit.
const BATCH_OVERHEAD: u64 = 128;
/// How many sources of one recording are remembered as having lost a block's first line. Past
/// it the whole recording is withheld.
const MAX_MISSED_SOURCES: usize = 256;
/// How often retention runs.
const RETENTION_EVERY: Duration = Duration::from_secs(60);

type StoreCall = Box<dyn FnOnce(&mut Store) + Send>;

enum Message {
    Append {
        id: RecordingId,
        current: ConnectionIdentity,
        batch: Batch,
        cost: u64,
    },
    Call(StoreCall),
    /// Finish what is queued and stop, answering once the database is closed.
    Stop(Option<oneshot::Sender<()>>),
}

/// What was lost for one recording while the queue was full.
#[derive(Default)]
struct Missed {
    rows: u64,
    /// Pod and container of each lost line that may have opened a block.
    sources: BTreeSet<(String, String)>,
    too_many_sources: bool,
}

#[derive(Default)]
struct Shared {
    queued_bytes: AtomicU64,
    missed: Mutex<BTreeMap<RecordingId, Missed>>,
}

impl Shared {
    fn missed(&self) -> std::sync::MutexGuard<'_, BTreeMap<RecordingId, Missed>> {
        self.missed.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Whether a batch was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    Queued,
    /// The queue was full. The rows are counted on the recording.
    Dropped,
}

/// The writer thread has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the recordings writer has stopped")]
pub struct RecorderClosed;

/// The running writer. Dropping it never waits: the thread finishes the queue by itself. Use
/// [`Self::shutdown`] to wait for that.
pub struct Recorder {
    tx: Sender<Message>,
    shared: Arc<Shared>,
}

impl Recorder {
    /// Opens the store in `dir` on a new thread, which then owns it.
    pub fn open(
        dir: PathBuf,
        retention: Retention,
    ) -> impl Future<Output = Result<Self, RecordingError>> + Send + 'static {
        let (tx, rx) = mpsc::channel();
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
                    let stopped = run(&mut store, &rx, &shared);
                    drop(store);
                    if let Some(stopped) = stopped {
                        let _ = stopped.send(());
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
        let queued = self.shared.queued_bytes.fetch_add(cost, Ordering::AcqRel);
        if queued + cost > MAX_QUEUED_BYTES {
            self.shared.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
            self.note_missed(id, &batch);
            return Submitted::Dropped;
        }
        let message = Message::Append {
            id,
            current: current.clone(),
            batch,
            cost,
        };
        match self.tx.send(message) {
            Ok(()) => Submitted::Queued,
            Err(_) => Submitted::Dropped,
        }
    }

    /// Runs `call` on the writer thread, after everything queued before it.
    pub fn call<R: Send + 'static>(
        &self,
        call: impl FnOnce(&mut Store) -> R + Send + 'static,
    ) -> impl Future<Output = Result<R, RecorderClosed>> + Send + 'static {
        let (reply, answer) = oneshot::channel();
        let sent = self.tx.send(Message::Call(Box::new(move |store| {
            let _ = reply.send(call(store));
        })));
        async move {
            sent.map_err(|_| RecorderClosed)?;
            answer.await.map_err(|_| RecorderClosed)
        }
    }

    /// Finishes the queue and closes the database.
    pub fn shutdown(self) -> impl Future<Output = ()> + Send + 'static {
        let (stopped, done) = oneshot::channel();
        let _ = self.tx.send(Message::Stop(Some(stopped)));
        async move {
            let _ = done.await;
        }
    }

    fn note_missed(&self, id: RecordingId, batch: &Batch) {
        let opening: Vec<_> = batch
            .lines
            .iter()
            .filter(|line| may_open_block(&line.text))
            .collect();
        let mut missed = self.shared.missed();
        let entry = missed.entry(id).or_default();
        entry.rows += batch.rows();
        for line in opening {
            if entry.sources.len() >= MAX_MISSED_SOURCES {
                entry.too_many_sources = true;
                break;
            }
            entry
                .sources
                .insert((line.pod.clone(), line.container.clone()));
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.tx.send(Message::Stop(None));
    }
}

fn run(
    store: &mut Store,
    rx: &mpsc::Receiver<Message>,
    shared: &Shared,
) -> Option<oneshot::Sender<()>> {
    let mut last_retention = Instant::now();
    loop {
        let wait = RETENTION_EVERY.saturating_sub(last_retention.elapsed());
        let message = rx.recv_timeout(wait);
        settle_missed(store, shared);
        match message {
            Ok(Message::Append {
                id,
                current,
                batch,
                cost,
            }) => {
                shared.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
                let rows = batch.rows();
                if let Err(error) = store.append(id, &current, batch) {
                    tracing::debug!(%error, "recording rows not stored");
                    if error.is_storage() {
                        shared.missed().entry(id).or_default().rows += rows;
                    }
                }
            }
            Ok(Message::Call(call)) => call(store),
            Ok(Message::Stop(stopped)) => return stopped,
            Err(RecvTimeoutError::Disconnected) => return None,
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

/// Records what was lost, before any later batch of the same recording is scrubbed. A count that
/// cannot be saved stays for the next round.
fn settle_missed(store: &mut Store, shared: &Shared) {
    let missed = std::mem::take(&mut *shared.missed());
    for (id, missed) in missed {
        for (pod, container) in &missed.sources {
            store.line_lost(id, pod, container);
        }
        if missed.too_many_sources {
            store.seal_lines(id);
        }
        if missed.rows > 0
            && let Err(error) = store.count_dropped(id, missed.rows)
        {
            tracing::debug!(%error, "cannot count dropped recording rows");
            shared.missed().entry(id).or_default().rows += missed.rows;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::{LogRow, RecordingTarget};
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

    #[tokio::test]
    async fn the_queue_budget_is_released_after_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        recorder.submit(id, &identity, batch("hello"));
        recorder.call(|_| ()).await.unwrap();
        assert_eq!(recorder.shared.queued_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn shutdown_finishes_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, id, identity) = started(dir.path()).await;
        recorder.submit(id, &identity, batch("last"));
        recorder.shutdown().await;
        let store = Store::open(dir.path(), Retention::default()).unwrap();
        assert_eq!(texts(&store), ["last"]);
    }
}
