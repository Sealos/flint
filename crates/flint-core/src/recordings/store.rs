//! `recordings.sqlite`: what is recorded, under which budget, and for how long.
// module-tests: store_tests.rs

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::ops::Range;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension as _, TransactionBehavior, params,
};

use super::identity::ConnectionIdentity;
use super::schema::{self, ROW_TABLES};
use super::scrub::{Scrubber, scrub};

/// The database, in Flint's data directory.
pub const DATABASE_FILE: &str = "recordings.sqlite";

const OWNER_ONLY: u32 = 0o600;

/// What a row costs beyond its text, for the per-recording budget.
const ROW_OVERHEAD: u64 = 64;
/// The longest stored line or event message. Longer text is cut after it is redacted.
const MAX_TEXT_BYTES: usize = crate::logs::MAX_LINE_BYTES;
/// The longest stored name or reason.
const MAX_NAME_BYTES: usize = 1024;
/// The most one eviction transaction frees, and how many rows one retention transaction deletes:
/// small commits let the write-ahead log checkpoint instead of growing.
const MAX_EVICT_TX_BYTES: u64 = 1024 * 1024;
const RETENTION_CHUNK: i64 = 1024;
/// The least one eviction pass frees.
const MIN_EVICT_BYTES: u64 = 64 * 1024;
/// The most of the total budget kept for the write-ahead log.
const MAX_WAL_BYTES: u64 = 16 * 1024 * 1024;
/// The smallest budgets a store runs with: the schema itself has to fit.
const MIN_TOTAL_BYTES: u64 = 1024 * 1024;
const MIN_RECORDING_BYTES: u64 = 16 * 1024;
/// What a gap counts for against its recording's budget, and the most one recording keeps. Past
/// that its oldest closed gaps go. At most half a recording's budget goes to gaps.
const GAP_COST: u64 = ROW_OVERHEAD;
const MAX_GAPS: u64 = 1024;
/// How long SQLite waits for a lock, and how many times a write tries before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_ATTEMPTS: u32 = 3;
/// How many recordings can have a dropped-row count waiting to be saved.
const MAX_DEFERRED_COUNTS: usize = 4096;

/// How long recorded data is kept, and how much of it. Starred recordings are exempt from age and
/// are never evicted: once they hold the whole budget, new rows are dropped and counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub max_age: Duration,
    pub max_recording_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            max_age: Duration::from_secs(7 * 24 * 60 * 60),
            max_recording_bytes: 500 * 1000 * 1000,
            max_total_bytes: 2000 * 1000 * 1000,
        }
    }
}

/// A recording's row id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordingId(pub i64);

/// What a recording follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingTarget {
    Workload { kind: String, name: String },
    Pod { name: String },
    Selector { selector: String },
}

impl RecordingTarget {
    fn columns(&self) -> (&'static str, &str, &str) {
        match self {
            RecordingTarget::Workload { kind, name } => ("workload", kind, name),
            RecordingTarget::Pod { name } => ("pod", "", name),
            RecordingTarget::Selector { selector } => ("selector", "", selector),
        }
    }
}

/// A log line to record. Holds raw text, so it has no `Debug`.
pub struct LogRow {
    pub at_ms: i64,
    pub pod: String,
    pub container: String,
    pub text: String,
    pub truncated: bool,
}

/// A Kubernetes event's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Normal,
    Warning,
}

impl EventKind {
    fn column(self) -> &'static str {
        match self {
            EventKind::Normal => "normal",
            EventKind::Warning => "warning",
        }
    }
}

/// A Kubernetes event to record. Holds raw text, so it has no `Debug`.
pub struct EventRow {
    pub at_ms: i64,
    pub kind: EventKind,
    pub reason: String,
    pub object_kind: String,
    pub object_name: String,
    pub message: String,
    pub count: i64,
}

/// One pod's CPU and memory at a moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleRow {
    pub at_ms: i64,
    pub pod: String,
    pub cpu_millicores: i64,
    pub memory_bytes: i64,
}

/// Rows to add to one recording.
#[derive(Default)]
pub struct Batch {
    pub lines: Vec<LogRow>,
    pub events: Vec<EventRow>,
    pub samples: Vec<SampleRow>,
}

impl Batch {
    pub(super) fn rows(&self) -> u64 {
        (self.lines.len() + self.events.len() + self.samples.len()) as u64
    }

    /// Roughly what this batch holds in memory while queued.
    pub(super) fn queued_bytes(&self) -> u64 {
        let lines: usize = self
            .lines
            .iter()
            .map(|l| l.pod.len() + l.container.len() + l.text.len())
            .sum();
        let events: usize = self
            .events
            .iter()
            .map(|e| e.reason.len() + e.object_kind.len() + e.object_name.len() + e.message.len())
            .sum();
        let samples: usize = self.samples.iter().map(|s| s.pod.len()).sum();
        (lines + events + samples) as u64 + self.rows() * ROW_OVERHEAD
    }
}

/// A run of one batch's rows, written in one transaction.
#[derive(Clone, Copy, Default)]
struct Part<'a> {
    lines: &'a [LogRow],
    events: &'a [EventRow],
    samples: &'a [SampleRow],
}

impl Part<'_> {
    fn rows(&self) -> u64 {
        (self.lines.len() + self.events.len() + self.samples.len()) as u64
    }
}

/// `batch` in runs that each cost at most `max`, or one row if that alone costs more.
fn parts(batch: &Batch, max: u64) -> Vec<Part<'_>> {
    let lines = runs(&batch.lines, max, |l| {
        stored_cost(l.pod.len() + l.container.len() + l.text.len())
    });
    let events = runs(&batch.events, max, |e| {
        let names = e.reason.len() + e.object_kind.len() + e.object_name.len();
        stored_cost(names + e.message.len())
    });
    let samples = runs(&batch.samples, max, |s| stored_cost(s.pod.len()));
    let lines = lines.into_iter().map(|r| Part {
        lines: &batch.lines[r],
        ..Part::default()
    });
    let events = events.into_iter().map(|r| Part {
        events: &batch.events[r],
        ..Part::default()
    });
    let samples = samples.into_iter().map(|r| Part {
        samples: &batch.samples[r],
        ..Part::default()
    });
    lines.chain(events).chain(samples).collect()
}

fn runs<T>(items: &[T], max: u64, cost: impl Fn(&T) -> u64) -> Vec<Range<usize>> {
    let (mut out, mut start, mut sum) = (Vec::new(), 0, 0u64);
    for (at, item) in items.iter().enumerate() {
        let item = cost(item);
        if at > start && sum + item > max {
            out.push(start..at);
            (start, sum) = (at, 0);
        }
        sum += item;
    }
    if start < items.len() {
        out.push(start..items.len());
    }
    out
}

/// Why a recording has a hole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapReason {
    SignInNeeded,
    Disconnected,
}

impl GapReason {
    fn column(self) -> &'static str {
        match self {
            GapReason::SignInNeeded => "sign_in_needed",
            GapReason::Disconnected => "disconnected",
        }
    }
}

/// How an append went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Appended {
    pub stored: u64,
    /// Rows the budget had no room for.
    pub dropped: u64,
}

/// A recording's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingInfo {
    pub id: RecordingId,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub starred: bool,
    pub paused: bool,
    pub bytes: u64,
    pub dropped_rows: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    #[error("recording is off for this cluster")]
    Disabled,
    #[error("this cluster's connection changed since the recording started")]
    ConnectionChanged,
    #[error("no such recording")]
    UnknownRecording,
    #[error("the recording has ended")]
    Ended,
    #[error("the recording is paused")]
    Paused,
    #[error("the recordings database has unknown schema version {0}")]
    UnknownSchema(i64),
    #[error("the recordings database is not a regular file")]
    NotAFile,
    #[error("recordings database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("recordings file error: {0}")]
    Io(#[from] std::io::Error),
}

impl RecordingError {
    /// The database failed, as opposed to refusing the rows.
    pub(super) fn is_storage(&self) -> bool {
        matches!(self, RecordingError::Sqlite(_) | RecordingError::Io(_))
    }
}

/// What a stored row counts for against the per-recording budget: its text plus what pages and
/// indexes add, rounded up so a recording's real size stays within its budget.
fn stored_cost(text_bytes: usize) -> u64 {
    let text = text_bytes as u64;
    ROW_OVERHEAD + text + text / 3
}

/// The part of the total budget kept for the write-ahead log.
fn wal_reserve(total: u64) -> u64 {
    (total / 8).min(MAX_WAL_BYTES)
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Another connection holds the lock: trying again can work.
fn is_busy(error: &RecordingError) -> bool {
    matches!(
        error,
        RecordingError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
            if matches!(e.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn is_full(error: &RecordingError) -> bool {
    matches!(
        error,
        RecordingError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
            if e.code == ErrorCode::DiskFull
    )
}

/// The recordings database. Use it from one thread: [`super::Recorder`] gives it one.
pub struct Store {
    conn: Connection,
    retention: Retention,
    scrubber: Scrubber,
    page_size: u64,
    /// The total budget less the write-ahead log's part: what the main file may use.
    file_budget: u64,
    wal_path: PathBuf,
    /// The write-ahead log's part of the total budget.
    wal_limit: u64,
    busy_timeout: Duration,
    /// Dropped rows not yet saved on their recordings, because the database took no writes.
    deferred_drops: BTreeMap<i64, u64>,
}

impl Store {
    /// Opens or creates [`DATABASE_FILE`] in `dir`, owner-only, and brings it within `retention`'s
    /// total budget.
    pub fn open(dir: &Path, retention: Retention) -> Result<Self, RecordingError> {
        std::fs::create_dir_all(dir)?;
        // SQLite's no-follow flag refuses a link anywhere in the path, and a data directory may
        // legitimately sit under one (`/var` on macOS). The file itself still may not be a link.
        let path = std::fs::canonicalize(dir)?.join(DATABASE_FILE);
        prepare_files(&path)?;
        let mut conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Must come before the first table: it decides how the file gives pages back.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema::ensure(&mut conn)?;
        // Only by path: closing any descriptor of the open database would drop SQLite's locks.
        check_files(&path)?;
        let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
        let retention = Retention {
            max_total_bytes: retention.max_total_bytes.max(MIN_TOTAL_BYTES),
            max_recording_bytes: retention.max_recording_bytes.max(MIN_RECORDING_BYTES),
            ..retention
        };
        let reserve = wal_reserve(retention.max_total_bytes);
        conn.pragma_update(None, "journal_size_limit", reserve as i64)?;
        conn.pragma_update(
            None,
            "wal_autocheckpoint",
            (reserve / page_size as u64 / 2).max(16) as i64,
        )?;
        let mut store = Store {
            conn,
            retention,
            scrubber: Scrubber::default(),
            page_size: page_size as u64,
            file_budget: retention.max_total_bytes - reserve,
            wal_path: PathBuf::from(format!("{}-wal", path.display())),
            wal_limit: reserve,
            busy_timeout: BUSY_TIMEOUT,
            deferred_drops: BTreeMap::new(),
        };
        store.end_running(now_ms())?;
        while store.used_bytes()? > store.file_budget {
            let before = store.used_bytes()?;
            store.make_room()?;
            if store.used_bytes()? >= before {
                break;
            }
        }
        let max_pages = i64::try_from(store.file_budget / store.page_size)
            .unwrap_or(i64::MAX)
            .min(i64::from(u32::MAX - 1));
        store
            .conn
            .pragma_update_and_check(None, "max_page_count", max_pages, |row| {
                row.get::<_, i64>(0)
            })?;
        Ok(store)
    }

    /// Ends what a previous run left running. Its streams are gone, and so is what the scrubber
    /// knew about them, so a stream must not continue the same recording mid-block.
    fn end_running(&mut self, now_ms: i64) -> Result<(), RecordingError> {
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE gaps SET to_ms = ?1 WHERE to_ms IS NULL", [now_ms])?;
        tx.execute(
            "UPDATE recordings SET ended_ms = MAX(started_ms, ?1) WHERE ended_ms IS NULL",
            [now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Turns recording on or off for one exact connection. Off ends its running recordings.
    pub fn set_enabled(
        &mut self,
        identity: &ConnectionIdentity,
        context: &str,
        enabled: bool,
        now_ms: i64,
    ) -> Result<(), RecordingError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO clusters (identity, context, enabled) VALUES (?1, ?2, ?3)
             ON CONFLICT (identity) DO UPDATE SET context = ?2, enabled = ?3",
            params![identity.as_bytes(), context, enabled],
        )?;
        if !enabled {
            let filter = "SELECT r.id FROM recordings r JOIN clusters c ON c.id = r.cluster_id
                          WHERE c.identity = ?2 AND r.ended_ms IS NULL";
            tx.execute(
                &format!(
                    "UPDATE gaps SET to_ms = ?1 WHERE to_ms IS NULL AND recording_id IN ({filter})"
                ),
                params![now_ms, identity.as_bytes()],
            )?;
            tx.execute(
                &format!("UPDATE recordings SET ended_ms = ?1 WHERE id IN ({filter})"),
                params![now_ms, identity.as_bytes()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether recording is on for this connection. An identity never turned on is off.
    pub fn is_enabled(&self, identity: &ConnectionIdentity) -> Result<bool, RecordingError> {
        Ok(self
            .conn
            .query_row(
                "SELECT enabled FROM clusters WHERE identity = ?1",
                [identity.as_bytes()],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// The running recording of `target` on this connection, started now if there is none.
    pub fn start(
        &mut self,
        identity: &ConnectionIdentity,
        namespace: &str,
        target: &RecordingTarget,
        now_ms: i64,
    ) -> Result<RecordingId, RecordingError> {
        let cluster: Option<(i64, bool)> = self
            .conn
            .query_row(
                "SELECT id, enabled FROM clusters WHERE identity = ?1",
                [identity.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((cluster, true)) = cluster else {
            return Err(RecordingError::Disabled);
        };
        let (kind, workload_kind, name) = target.columns();
        let id = self.conn.query_row(
            "INSERT INTO recordings (cluster_id, namespace, target_kind, workload_kind, target, started_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (cluster_id, namespace, target_kind, workload_kind, target)
                 WHERE ended_ms IS NULL
             DO UPDATE SET target = excluded.target
             RETURNING id",
            params![cluster, namespace, kind, workload_kind, name, now_ms],
            |row| row.get(0),
        )?;
        Ok(RecordingId(id))
    }

    /// Ends a recording. It keeps its rows until retention removes them.
    pub fn stop(&mut self, id: RecordingId, now_ms: i64) -> Result<(), RecordingError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE gaps SET to_ms = ?2 WHERE recording_id = ?1 AND to_ms IS NULL",
            params![id.0, now_ms],
        )?;
        let changed = tx.execute(
            "UPDATE recordings SET ended_ms = ?2 WHERE id = ?1 AND ended_ms IS NULL",
            params![id.0, now_ms],
        )?;
        tx.commit()?;
        self.scrubber.forget(id.0);
        if changed == 0 {
            self.info(id)?;
        }
        Ok(())
    }

    /// Opens a gap: appends are refused until [`Self::resume`]. A second pause changes nothing.
    pub fn pause(
        &mut self,
        id: RecordingId,
        reason: GapReason,
        now_ms: i64,
    ) -> Result<(), RecordingError> {
        if self.info(id)?.ended_ms.is_some() {
            return Err(RecordingError::Ended);
        }
        let keep = (self.retention.max_recording_bytes / (2 * GAP_COST)).min(MAX_GAPS) as i64;
        let tx = self.conn.transaction()?;
        let added = tx.execute(
            "INSERT INTO gaps (recording_id, from_ms, reason) VALUES (?1, ?2, ?3)
             ON CONFLICT (recording_id) WHERE to_ms IS NULL DO NOTHING",
            params![id.0, now_ms, reason.column()],
        )?;
        if added > 0 {
            let removed = tx.execute(
                "DELETE FROM gaps WHERE id IN
                 (SELECT id FROM gaps WHERE recording_id = ?1 AND to_ms IS NOT NULL
                  ORDER BY from_ms, id
                  LIMIT MAX((SELECT COUNT(*) FROM gaps WHERE recording_id = ?1) - ?2, 0))",
                params![id.0, keep],
            )?;
            let charge = (added as i64 - removed as i64) * GAP_COST as i64;
            tx.execute(
                "UPDATE recordings SET bytes = MAX(bytes + ?2, 0) WHERE id = ?1",
                params![id.0, charge],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Closes the open gap, if any.
    pub fn resume(&mut self, id: RecordingId, now_ms: i64) -> Result<(), RecordingError> {
        self.conn.execute(
            "UPDATE gaps SET to_ms = ?2 WHERE recording_id = ?1 AND to_ms IS NULL",
            params![id.0, now_ms],
        )?;
        Ok(())
    }

    /// Stars a recording, keeping it past retention, or unstars it.
    pub fn set_starred(&mut self, id: RecordingId, starred: bool) -> Result<(), RecordingError> {
        let changed = self.conn.execute(
            "UPDATE recordings SET starred = ?2 WHERE id = ?1",
            params![id.0, starred],
        )?;
        if changed == 0 {
            return Err(RecordingError::UnknownRecording);
        }
        Ok(())
    }

    /// Deletes a recording and its rows.
    pub fn delete(&mut self, id: RecordingId) -> Result<(), RecordingError> {
        self.conn
            .execute("DELETE FROM recordings WHERE id = ?1", [id.0])?;
        self.scrubber.forget(id.0);
        self.deferred_drops.remove(&id.0);
        self.vacuum()
    }

    pub fn info(&self, id: RecordingId) -> Result<RecordingInfo, RecordingError> {
        let deferred = self.deferred_drops.get(&id.0).copied().unwrap_or(0);
        self.conn
            .query_row(
                "SELECT started_ms, ended_ms, starred, bytes, dropped_rows,
                        EXISTS (SELECT 1 FROM gaps g WHERE g.recording_id = r.id AND g.to_ms IS NULL)
                 FROM recordings r WHERE id = ?1",
                [id.0],
                |row| {
                    Ok(RecordingInfo {
                        id,
                        started_ms: row.get(0)?,
                        ended_ms: row.get(1)?,
                        starred: row.get(2)?,
                        bytes: row.get::<_, i64>(3)? as u64,
                        dropped_rows: row.get::<_, i64>(4)? as u64 + deferred,
                        paused: row.get(5)?,
                    })
                },
            )
            .optional()?
            .ok_or(RecordingError::UnknownRecording)
    }

    /// Adds `batch` to a recording, for the connection the caller is on now. Refused unless that
    /// is the recording's own connection, recording is on for it, and the recording is running.
    /// Text is redacted first. The recording's oldest rows make room for new ones, except in a
    /// starred recording, where what does not fit is dropped and counted.
    pub fn append(
        &mut self,
        id: RecordingId,
        current: &ConnectionIdentity,
        batch: Batch,
    ) -> Result<Appended, RecordingError> {
        match self.append_counted(id, current, batch)? {
            (appended, None) => Ok(appended),
            (_, Some(error)) => Err(error),
        }
    }

    /// [`Self::append`], which also says how far it got when a later part of the batch failed.
    pub(super) fn append_counted(
        &mut self,
        id: RecordingId,
        current: &ConnectionIdentity,
        batch: Batch,
    ) -> Result<(Appended, Option<RecordingError>), RecordingError> {
        if let Err(refused) = self.check(id, current) {
            match refused {
                // Not stored, but a key block opened here runs on into the lines after resuming.
                RecordingError::Paused => drop(self.scrubbed(id, batch)),
                RecordingError::Ended | RecordingError::UnknownRecording => {
                    self.scrubber.forget(id.0);
                }
                // Not stored either, and its lines still count for what comes after them.
                ref error if error.is_storage() => drop(self.scrubbed(id, batch)),
                // Another connection's rows say nothing about this recording's open blocks.
                _ => {}
            }
            return Err(refused);
        }
        self.flush_dropped_counts();
        let clean = self.scrubbed(id, batch);
        let mut total = Appended::default();
        let mut failure = None;
        // Small transactions, so the write-ahead log can be checked and checkpointed between them.
        for part in parts(&clean, (self.wal_limit / 4).max(1)) {
            if !self.wal_has_room() {
                self.count_dropped(id, part.rows());
                total.dropped += part.rows();
                continue;
            }
            match self.write_part(id, current, part) {
                Ok(done) => {
                    total.stored += done.stored;
                    total.dropped += done.dropped;
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        // The rows are stored: a failure here is not a failure of this append.
        if self.wal_has_room()
            && self.used_bytes().is_ok_and(|used| used > self.file_budget)
            && let Err(error) = self.make_room()
        {
            tracing::warn!(%error, "cannot make room in the recordings database");
        }
        Ok((total, failure))
    }

    /// Writes one part, making room if the file is full and trying again while another connection
    /// holds the lock.
    fn write_part(
        &mut self,
        id: RecordingId,
        current: &ConnectionIdentity,
        part: Part<'_>,
    ) -> Result<Appended, RecordingError> {
        let mut attempt = 1;
        loop {
            let result = match self.write(id, current, part) {
                Err(e) if is_full(&e) => {
                    self.make_room()?;
                    match self.write(id, current, part) {
                        Err(e) if is_full(&e) => {
                            self.count_dropped(id, part.rows());
                            Ok(Appended {
                                stored: 0,
                                dropped: part.rows(),
                            })
                        }
                        other => other,
                    }
                }
                other => other,
            };
            match result {
                Err(e) if is_busy(&e) && attempt < WRITE_ATTEMPTS => attempt += 1,
                other => return other,
            }
        }
    }

    /// Whether the write-ahead log has room for more. Past its limit it is checkpointed without
    /// waiting; while a reader keeps that from reclaiming it, writes are suspended.
    fn wal_has_room(&mut self) -> bool {
        if self.wal_bytes() <= self.wal_limit {
            return true;
        }
        if self.conn.busy_timeout(Duration::ZERO).is_err() {
            return false;
        }
        let blocked = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            });
        if let Err(error) = self.conn.busy_timeout(self.busy_timeout) {
            tracing::warn!(%error, "cannot restore the recordings lock timeout");
        }
        matches!(blocked, Ok(0)) && self.wal_bytes() <= self.wal_limit
    }

    fn wal_bytes(&self) -> u64 {
        std::fs::metadata(&self.wal_path).map_or(0, |meta| meta.len())
    }

    /// Adds to a recording's count of rows that were never stored, now or once the database takes
    /// writes again.
    pub(super) fn count_dropped(&mut self, id: RecordingId, rows: u64) {
        if !self.wal_has_room() || self.save_dropped(id.0, rows).is_err() {
            self.defer_dropped(id.0, rows);
        }
    }

    fn save_dropped(&self, id: i64, rows: u64) -> Result<(), RecordingError> {
        self.conn.execute(
            "UPDATE recordings SET dropped_rows = dropped_rows + ?2 WHERE id = ?1",
            params![id, rows as i64],
        )?;
        Ok(())
    }

    fn defer_dropped(&mut self, id: i64, rows: u64) {
        if self.deferred_drops.len() >= MAX_DEFERRED_COUNTS
            && !self.deferred_drops.contains_key(&id)
        {
            tracing::warn!(rows, "cannot keep count of dropped recording rows");
            return;
        }
        *self.deferred_drops.entry(id).or_default() += rows;
    }

    /// Saves the dropped-row counts that had to wait, if the database takes writes.
    pub(super) fn flush_dropped_counts(&mut self) {
        if self.deferred_drops.is_empty() || !self.wal_has_room() {
            return;
        }
        for (id, rows) in std::mem::take(&mut self.deferred_drops) {
            if self.save_dropped(id, rows).is_err() {
                self.defer_dropped(id, rows);
            }
        }
    }

    /// A loss could not be placed in the queue: withholds every running recording until it ends,
    /// and counts `rows` on each, since any of them may be the one that lost them.
    pub(super) fn seal_running(&mut self, rows: u64) -> Result<(), RecordingError> {
        let ids: Vec<i64> = self
            .conn
            .prepare("SELECT id FROM recordings WHERE ended_ms IS NULL")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for id in ids {
            self.scrubber.seal(id);
            if rows > 0 {
                self.count_dropped(RecordingId(id), rows);
            }
        }
        Ok(())
    }

    /// A line of this source was lost before reaching the store and may have opened a block.
    pub(super) fn line_lost(&mut self, id: RecordingId, pod: &str, container: &str) {
        self.scrubber.lost_line(id.0, pod, container);
    }

    /// Too many sources lost such a line to name them: withhold all of `id` until it ends.
    pub(super) fn seal_lines(&mut self, id: RecordingId) {
        self.scrubber.seal(id.0);
    }

    /// Removes what is past its age, then whatever is over the total budget.
    pub fn apply_retention(&mut self, now_ms: i64) -> Result<(), RecordingError> {
        if !self.wal_has_room() {
            return Ok(());
        }
        let age = i64::try_from(self.retention.max_age.as_millis()).unwrap_or(i64::MAX);
        let cutoff = now_ms.saturating_sub(age);
        let ids: Vec<i64> = self
            .conn
            .prepare("SELECT id FROM recordings WHERE starred = 0")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for id in ids {
            for table in ROW_TABLES {
                while self.wal_has_room()
                    && self.expire_chunk(table, id, cutoff)? == RETENTION_CHUNK
                {}
            }
            while self.wal_has_room() && self.expire_gaps(id, cutoff)? == RETENTION_CHUNK {}
        }
        self.conn.execute(
            "DELETE FROM recordings WHERE starred = 0 AND ended_ms IS NOT NULL AND ended_ms < ?1
             AND NOT EXISTS (SELECT 1 FROM log_lines l WHERE l.recording_id = recordings.id)
             AND NOT EXISTS (SELECT 1 FROM events e WHERE e.recording_id = recordings.id)
             AND NOT EXISTS (SELECT 1 FROM metric_samples m WHERE m.recording_id = recordings.id)",
            [cutoff],
        )?;
        self.vacuum()?;
        if self.used_bytes()? > self.file_budget {
            self.make_room()?;
        }
        Ok(())
    }

    /// Deletes up to [`RETENTION_CHUNK`] expired rows in one transaction; how many it deleted.
    fn expire_chunk(&mut self, table: &str, id: i64, cutoff: i64) -> Result<i64, RecordingError> {
        let tx = self.conn.transaction()?;
        let (mut deleted, mut freed) = (0i64, 0i64);
        {
            let mut delete = tx.prepare_cached(&format!(
                "DELETE FROM {table} WHERE id IN
                 (SELECT id FROM {table} WHERE recording_id = ?1 AND at_ms < ?2
                  ORDER BY at_ms, id LIMIT ?3)
                 RETURNING bytes"
            ))?;
            let mut rows = delete.query(params![id, cutoff, RETENTION_CHUNK])?;
            while let Some(row) = rows.next()? {
                deleted += 1;
                freed += row.get::<_, i64>(0)?;
            }
        }
        tx.execute(
            "UPDATE recordings SET bytes = MAX(bytes - ?2, 0) WHERE id = ?1",
            params![id, freed],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Deletes up to [`RETENTION_CHUNK`] closed gaps that ended before `cutoff`; how many.
    fn expire_gaps(&mut self, id: i64, cutoff: i64) -> Result<i64, RecordingError> {
        let tx = self.conn.transaction()?;
        let removed = tx.execute(
            "DELETE FROM gaps WHERE id IN
             (SELECT id FROM gaps WHERE recording_id = ?1 AND to_ms IS NOT NULL AND to_ms < ?2
              ORDER BY from_ms, id LIMIT ?3)",
            params![id, cutoff, RETENTION_CHUNK],
        )? as i64;
        tx.execute(
            "UPDATE recordings SET bytes = MAX(bytes - ?2, 0) WHERE id = ?1",
            params![id, removed * GAP_COST as i64],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// The bytes of the file in use, free pages aside.
    pub fn used_bytes(&self) -> Result<u64, RecordingError> {
        let pages: i64 = self
            .conn
            .pragma_query_value(None, "page_count", |row| row.get(0))?;
        let free: i64 = self
            .conn
            .pragma_query_value(None, "freelist_count", |row| row.get(0))?;
        Ok((pages - free).max(0) as u64 * self.page_size)
    }

    fn check(&self, id: RecordingId, current: &ConnectionIdentity) -> Result<(), RecordingError> {
        check_on(&self.conn, id, current).map(|_| ())
    }

    fn scrubbed(&mut self, id: RecordingId, batch: Batch) -> Batch {
        let Batch {
            lines,
            events,
            samples,
        } = batch;
        let lines = lines
            .into_iter()
            .map(|line| {
                let text = if line.truncated {
                    self.scrubber
                        .cut_line(id.0, &line.pod, &line.container, &line.text)
                } else {
                    self.scrubber
                        .line(id.0, &line.pod, &line.container, &line.text)
                };
                let (text, cut) = clipped(text, MAX_TEXT_BYTES);
                LogRow {
                    at_ms: line.at_ms,
                    pod: clipped(line.pod, MAX_NAME_BYTES).0,
                    container: clipped(line.container, MAX_NAME_BYTES).0,
                    text,
                    truncated: line.truncated || cut,
                }
            })
            .collect();
        let events = events
            .into_iter()
            .map(|event| EventRow {
                at_ms: event.at_ms,
                kind: event.kind,
                reason: clipped(scrub(&event.reason), MAX_NAME_BYTES).0,
                object_kind: clipped(event.object_kind, MAX_NAME_BYTES).0,
                object_name: clipped(event.object_name, MAX_NAME_BYTES).0,
                message: clipped(scrub(&event.message), MAX_TEXT_BYTES).0,
                count: event.count,
            })
            .collect();
        let samples = samples
            .into_iter()
            .map(|sample| SampleRow {
                pod: clipped(sample.pod, MAX_NAME_BYTES).0,
                ..sample
            })
            .collect();
        Batch {
            lines,
            events,
            samples,
        }
    }

    fn write(
        &mut self,
        id: RecordingId,
        current: &ConnectionIdentity,
        batch: Part<'_>,
    ) -> Result<Appended, RecordingError> {
        let budget = self.retention.max_recording_bytes;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (starred, before) = check_on(&tx, id, current)?;
        // A starred recording keeps what it has, so its budget limits what comes in.
        let mut room = if starred {
            budget.saturating_sub(before)
        } else {
            budget
        };
        let mut added = 0u64;
        let mut outcome = Appended::default();
        let mut admit = |cost: u64| {
            if cost > room {
                outcome.dropped += 1;
                return false;
            }
            if starred {
                room -= cost;
            }
            added += cost;
            outcome.stored += 1;
            true
        };
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO log_lines (recording_id, at_ms, pod, container, text, truncated, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for l in batch.lines {
                let cost = stored_cost(l.pod.len() + l.container.len() + l.text.len());
                if admit(cost) {
                    insert.execute(params![
                        id.0,
                        l.at_ms,
                        l.pod,
                        l.container,
                        l.text,
                        l.truncated,
                        cost as i64
                    ])?;
                }
            }
            let mut insert = tx.prepare_cached(
                "INSERT INTO events
                 (recording_id, at_ms, kind, reason, object_kind, object_name, message, count, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for e in batch.events {
                let text = e.reason.len() + e.object_kind.len() + e.object_name.len();
                let cost = stored_cost(text + e.message.len());
                if admit(cost) {
                    insert.execute(params![
                        id.0,
                        e.at_ms,
                        e.kind.column(),
                        e.reason,
                        e.object_kind,
                        e.object_name,
                        e.message,
                        e.count,
                        cost as i64
                    ])?;
                }
            }
            let mut insert = tx.prepare_cached(
                "INSERT INTO metric_samples
                 (recording_id, at_ms, pod, cpu_millicores, memory_bytes, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for s in batch.samples {
                let cost = stored_cost(s.pod.len());
                if admit(cost) {
                    insert.execute(params![
                        id.0,
                        s.at_ms,
                        s.pod,
                        s.cpu_millicores,
                        s.memory_bytes,
                        cost as i64
                    ])?;
                }
            }
        }
        let mut total = before + added;
        if !starred && total > budget {
            total = total.saturating_sub(evict_oldest(&tx, id.0, total - budget)?);
        }
        tx.execute(
            "UPDATE recordings SET bytes = ?2, dropped_rows = dropped_rows + ?3 WHERE id = ?1",
            params![id.0, total as i64, outcome.dropped as i64],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Evicts the oldest rows of the oldest unstarred recordings, toward 90% of the file budget.
    /// One pass frees at most a tenth of the budget: starred data can keep the file above that
    /// mark for good, and must not cost the unstarred recordings everything they hold.
    fn make_room(&mut self) -> Result<(), RecordingError> {
        let target = self.file_budget / 10 * 9;
        let used = self.used_bytes()?;
        if used <= target {
            return Ok(());
        }
        let mut left = (used - target).clamp(MIN_EVICT_BYTES, self.file_budget / 10);
        while left > 0 && self.wal_has_room() {
            let oldest: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM recordings WHERE starred = 0 AND bytes > 0
                     ORDER BY started_ms, id LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(id) = oldest else {
                break;
            };
            let tx = self.conn.transaction()?;
            let freed = evict_oldest(&tx, id, left.min(MAX_EVICT_TX_BYTES))?;
            // Nothing left to free means the count was off: it must not pick this one again.
            tx.execute(
                "UPDATE recordings SET bytes = CASE WHEN ?2 = 0 THEN 0 ELSE MAX(bytes - ?2, 0) END
                 WHERE id = ?1",
                params![id, freed as i64],
            )?;
            tx.commit()?;
            left = left.saturating_sub(freed);
        }
        self.vacuum()
    }

    /// Gives free pages back to the file system.
    fn vacuum(&mut self) -> Result<(), RecordingError> {
        let mut statement = self.conn.prepare("PRAGMA incremental_vacuum")?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn set_busy_timeout(&mut self, timeout: Duration) {
        self.busy_timeout = timeout;
        self.conn.busy_timeout(timeout).unwrap();
    }

    #[cfg(test)]
    pub(super) fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// Whether `id` may take rows from `current` now: its starred flag and bytes if so.
fn check_on(
    conn: &Connection,
    id: RecordingId,
    current: &ConnectionIdentity,
) -> Result<(bool, u64), RecordingError> {
    type Row = (Vec<u8>, bool, Option<i64>, bool, i64, bool);
    let row: Option<Row> = conn
        .query_row(
            "SELECT c.identity, c.enabled, r.ended_ms, r.starred, r.bytes,
                    EXISTS (SELECT 1 FROM gaps g WHERE g.recording_id = r.id AND g.to_ms IS NULL)
             FROM recordings r JOIN clusters c ON c.id = r.cluster_id WHERE r.id = ?1",
            [id.0],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((identity, enabled, ended, starred, bytes, paused)) = row else {
        return Err(RecordingError::UnknownRecording);
    };
    if identity != current.as_bytes() {
        return Err(RecordingError::ConnectionChanged);
    }
    if !enabled {
        return Err(RecordingError::Disabled);
    }
    if ended.is_some() {
        return Err(RecordingError::Ended);
    }
    if paused {
        return Err(RecordingError::Paused);
    }
    Ok((starred, bytes as u64))
}

/// The next thing to evict: when, its table's place, its row and what it frees.
type Evictable = (i64, usize, i64, u64);

/// Deletes a recording's oldest rows and closed gaps, one at a time across all tables, until
/// `want` bytes are gone or it has none left, and returns the bytes freed. The caller updates the
/// recording's byte count.
fn evict_oldest(conn: &Connection, recording: i64, want: u64) -> Result<u64, RecordingError> {
    let mut freed = 0u64;
    while freed < want {
        let mut oldest: Option<Evictable> = None;
        for (index, table) in ROW_TABLES.iter().enumerate() {
            let row = conn
                .prepare_cached(&format!(
                    "SELECT at_ms, id, bytes FROM {table} WHERE recording_id = ?1
                     ORDER BY at_ms, id LIMIT 1"
                ))?
                .query_row([recording], |row| {
                    Ok((
                        row.get(0)?,
                        index,
                        row.get(1)?,
                        row.get::<_, i64>(2)? as u64,
                    ))
                })
                .optional()?;
            oldest = earliest(oldest, row);
        }
        let gap = conn
            .prepare_cached(
                "SELECT from_ms, id FROM gaps WHERE recording_id = ?1 AND to_ms IS NOT NULL
                 ORDER BY from_ms, id LIMIT 1",
            )?
            .query_row([recording], |row| {
                Ok((row.get(0)?, ROW_TABLES.len(), row.get(1)?, GAP_COST))
            })
            .optional()?;
        let Some((_, index, id, bytes)) = earliest(oldest, gap) else {
            break;
        };
        let table = ROW_TABLES.get(index).copied().unwrap_or("gaps");
        conn.prepare_cached(&format!("DELETE FROM {table} WHERE id = ?1"))?
            .execute([id])?;
        freed += bytes;
    }
    Ok(freed)
}

fn earliest(a: Option<Evictable>, b: Option<Evictable>) -> Option<Evictable> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if (b.0, b.1) < (a.0, a.1) { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// `text` cut to at most `max` bytes at a character boundary, and whether it was cut.
fn clipped(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

/// Makes the database and its side files owner-only before SQLite opens them, creating the
/// database if needed. SQLite gives new side files the database's mode. Never call it once SQLite
/// has the file open: use [`check_files`].
fn prepare_files(path: &Path) -> Result<(), RecordingError> {
    check_files(path)?;
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(OWNER_ONLY)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    match created {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Keeps the database and its side files owner-only, by path alone. A link or anything else that
/// is not a regular file is refused.
fn check_files(path: &Path) -> Result<(), RecordingError> {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let file = PathBuf::from(name);
        match std::fs::symlink_metadata(&file) {
            Ok(meta) if !meta.file_type().is_file() => return Err(RecordingError::NotAFile),
            Ok(meta) if meta.permissions().mode() & 0o777 != OWNER_ONLY => {
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(OWNER_ONLY))?;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
