//! The recordings database's tables. Every value has a typed column.
// module-tests: store_tests.rs

use rusqlite::Connection;

use super::store::RecordingError;

/// `PRAGMA user_version` of this schema.
pub(super) const VERSION: i64 = 1;

/// The tables holding a recording's rows, each with `id`, `recording_id`, `at_ms` and `bytes`.
pub(super) const ROW_TABLES: [&str; 3] = ["log_lines", "events", "metric_samples"];

const TABLES: &str = "
CREATE TABLE clusters (
    id INTEGER PRIMARY KEY,
    identity BLOB NOT NULL UNIQUE CHECK (length(identity) = 32),
    context TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1))
) STRICT;

CREATE TABLE recordings (
    id INTEGER PRIMARY KEY AUTOINCREMENT, -- never reused: a stale stream's id matches nothing

    cluster_id INTEGER NOT NULL REFERENCES clusters (id) ON DELETE CASCADE,
    namespace TEXT NOT NULL,
    target_kind TEXT NOT NULL CHECK (target_kind IN ('workload', 'pod', 'selector')),
    workload_kind TEXT NOT NULL DEFAULT '',
    target TEXT NOT NULL,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER,
    starred INTEGER NOT NULL DEFAULT 0 CHECK (starred IN (0, 1)),
    bytes INTEGER NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    dropped_rows INTEGER NOT NULL DEFAULT 0 CHECK (dropped_rows >= 0)
) STRICT;
CREATE UNIQUE INDEX recordings_running
    ON recordings (cluster_id, namespace, target_kind, workload_kind, target)
    WHERE ended_ms IS NULL;

CREATE TABLE log_lines (
    id INTEGER PRIMARY KEY,
    recording_id INTEGER NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
    at_ms INTEGER NOT NULL,
    pod TEXT NOT NULL,
    container TEXT NOT NULL,
    text TEXT NOT NULL,
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    bytes INTEGER NOT NULL
) STRICT;
CREATE INDEX log_lines_by_time ON log_lines (recording_id, at_ms);

CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    recording_id INTEGER NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
    at_ms INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('normal', 'warning')),
    reason TEXT NOT NULL,
    object_kind TEXT NOT NULL,
    object_name TEXT NOT NULL,
    message TEXT NOT NULL,
    count INTEGER NOT NULL,
    bytes INTEGER NOT NULL
) STRICT;
CREATE INDEX events_by_time ON events (recording_id, at_ms);

CREATE TABLE metric_samples (
    id INTEGER PRIMARY KEY,
    recording_id INTEGER NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
    at_ms INTEGER NOT NULL,
    pod TEXT NOT NULL,
    cpu_millicores INTEGER NOT NULL,
    memory_bytes INTEGER NOT NULL,
    bytes INTEGER NOT NULL
) STRICT;
CREATE INDEX metric_samples_by_time ON metric_samples (recording_id, at_ms);

CREATE TABLE gaps (
    id INTEGER PRIMARY KEY,
    recording_id INTEGER NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
    from_ms INTEGER NOT NULL,
    to_ms INTEGER,
    reason TEXT NOT NULL CHECK (reason IN ('sign_in_needed', 'disconnected'))
) STRICT;
CREATE UNIQUE INDEX gaps_open ON gaps (recording_id) WHERE to_ms IS NULL;
CREATE INDEX gaps_by_time ON gaps (recording_id, from_ms);
";

/// Creates the tables in a new database. A database of any other version is refused and left as
/// it is.
pub(super) fn ensure(conn: &mut Connection) -> Result<(), RecordingError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        VERSION => Ok(()),
        0 => {
            let tx = conn.transaction()?;
            tx.execute_batch(TABLES)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
            Ok(())
        }
        found => Err(RecordingError::UnknownSchema(found)),
    }
}
