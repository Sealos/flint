//! The agent activity log (ADR-0013, UP-16): one record per MCP tool call, so the user can see
//! what an agent read. `flint mcp` appends; the app watches the file and shows it per cluster.
//! A record says what was asked for and how much came back, never the returned content.
//!
//! The log is JSON lines in [`ACTIVITY_FILE`] in Flint's config directory. When an append would
//! take it past [`MAX_FILE_BYTES`], it is renamed to [`PREVIOUS_FILE`] (replacing the one before)
//! and a new file starts, so the log keeps at most two files' worth. Each record is written with
//! one append, so records from several `flint mcp` processes do not interleave. Lines that do not
//! parse (a newer format, a torn write) are skipped when reading.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The current log file, in the config directory.
pub const ACTIVITY_FILE: &str = "agent-activity.jsonl";
/// The file the log rotates into.
pub const PREVIOUS_FILE: &str = "agent-activity.1.jsonl";
/// The size at which the log rotates: about four thousand records.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Writers hold an exclusive lock on this file while they append (and rotate).
pub const LOCK_FILE: &str = "agent-activity.lock";
/// The longest a text field is recorded: a DNS name's length, plenty for any Kubernetes name.
pub const MAX_FIELD_BYTES: usize = 253;
/// The most records [`read`] returns, newest kept: more than the two files hold, so this only
/// bounds a log written with a larger cap.
pub const MAX_RECORDS: usize = 10_000;

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// It returned what was asked for (perhaps cut short).
    Ok,
    /// It named something the agent may not read: a cluster not exposed, or a namespace or kind
    /// outside the cluster's scope.
    Denied,
    /// It failed for another reason (not found, unreachable, invalid arguments).
    Failed,
}

/// One MCP tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRecord {
    /// When the call ended, in Unix milliseconds.
    pub at_ms: i64,
    /// The tool's name, as the agent called it.
    pub tool: String,
    /// The context the call named; none for a tool that names no cluster (`list_clusters`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    /// The API server the cluster was exposed with, when it is exposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// What the call named, as the agent wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// How many items came back (objects, events, log lines, kinds), when the tool counts them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<u64>,
    /// The size of the reply, in bytes.
    pub bytes: u64,
    /// The reply was cut short at a limit.
    #[serde(default)]
    pub truncated: bool,
    pub outcome: Outcome,
}

/// Appends `record` to the log in `config_dir`, rotating it first if it would grow past
/// [`MAX_FILE_BYTES`]. Creates the directory if needed. Text fields are cut at
/// [`MAX_FIELD_BYTES`]: they hold what the agent wrote, and a few huge ones must not flush the
/// log. Writers take [`LOCK_FILE`] for the whole check, rotation and write, so two of them never
/// rotate the same file twice.
pub fn append(config_dir: &Path, record: &ActivityRecord) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(&capped(record))?;
    line.push(b'\n');
    std::fs::create_dir_all(config_dir)?;
    let _lock = Lock::take(&config_dir.join(LOCK_FILE))?;
    let path = config_dir.join(ACTIVITY_FILE);
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    if size > 0 && size + line.len() as u64 > MAX_FILE_BYTES {
        std::fs::rename(&path, config_dir.join(PREVIOUS_FILE))?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(&line)
}

/// `record` with each text field cut at [`MAX_FIELD_BYTES`], saying how much was left out.
fn capped(record: &ActivityRecord) -> ActivityRecord {
    let cap = |text: &String| {
        if text.len() <= MAX_FIELD_BYTES {
            return text.clone();
        }
        let mut end = MAX_FIELD_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…(+{} bytes)", &text[..end], text.len() - end)
    };
    let cap_opt = |text: &Option<String>| text.as_ref().map(cap);
    ActivityRecord {
        tool: cap(&record.tool),
        cluster: cap_opt(&record.cluster),
        server: cap_opt(&record.server),
        kind: cap_opt(&record.kind),
        namespace: cap_opt(&record.namespace),
        name: cap_opt(&record.name),
        container: cap_opt(&record.container),
        ..record.clone()
    }
}

/// An exclusive `flock` on a file, released when dropped (with the descriptor).
struct Lock {
    /// Closing it releases the lock.
    _file: std::fs::File,
}

impl Lock {
    fn take(path: &Path) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd as _;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        // SAFETY: flock on a descriptor `file` owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Lock { _file: file })
    }
}

/// The records in `config_dir`, oldest first, at most [`MAX_RECORDS`] (the newest). A missing
/// log is empty.
pub fn read(config_dir: &Path) -> Vec<ActivityRecord> {
    let mut records: Vec<ActivityRecord> = [PREVIOUS_FILE, ACTIVITY_FILE]
        .iter()
        .filter_map(|file| std::fs::read(config_dir.join(file)).ok())
        .flat_map(|bytes| {
            bytes
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice(line).ok())
                .collect::<Vec<_>>()
        })
        .collect();
    let extra = records.len().saturating_sub(MAX_RECORDS);
    records.drain(..extra);
    records
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(n: u64) -> ActivityRecord {
        ActivityRecord {
            at_ms: 1_700_000_000_000 + n as i64,
            tool: "get_resource".into(),
            cluster: Some("prod".into()),
            server: Some("https://prod:6443".into()),
            kind: Some("pods".into()),
            namespace: Some("web".into()),
            name: Some(format!("web-{n}")),
            container: None,
            items: Some(1),
            bytes: 1200,
            truncated: false,
            outcome: Outcome::Ok,
        }
    }

    // covers UP-16: the record format is one JSON object per line, with what was named and how
    // much came back; empty fields are left out.
    #[test]
    fn records_are_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &record(1)).unwrap();
        let denied = ActivityRecord {
            tool: "list_resources".into(),
            kind: Some("secrets".into()),
            namespace: None,
            name: None,
            items: None,
            bytes: 80,
            outcome: Outcome::Denied,
            ..record(2)
        };
        append(dir.path(), &denied).unwrap();
        let text = std::fs::read_to_string(dir.path().join(ACTIVITY_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            r#"{"at_ms":1700000000001,"tool":"get_resource","cluster":"prod","server":"https://prod:6443","kind":"pods","namespace":"web","name":"web-1","items":1,"bytes":1200,"truncated":false,"outcome":"ok"}"#
        );
        assert_eq!(
            lines[1],
            r#"{"at_ms":1700000000002,"tool":"list_resources","cluster":"prod","server":"https://prod:6443","kind":"secrets","bytes":80,"truncated":false,"outcome":"denied"}"#
        );
        assert_eq!(read(dir.path()), [record(1), denied]);
    }

    // covers UP-16
    #[test]
    fn a_missing_log_is_empty_and_bad_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_empty());
        append(&dir.path().join("new"), &record(1)).unwrap();
        let path = dir.path().join("new").join(ACTIVITY_FILE);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"at_ms\": 1, \"torn\n");
        std::fs::write(&path, text).unwrap();
        append(&dir.path().join("new"), &record(2)).unwrap();
        assert_eq!(read(&dir.path().join("new")), [record(1), record(2)]);
    }

    // covers UP-16: the log rotates at its size cap and keeps the file before; reading spans
    // both, oldest first.
    #[test]
    fn the_log_rotates_at_its_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let line = serde_json::to_vec(&record(0)).unwrap().len() as u64 + 1;
        let total = MAX_FILE_BYTES / line * 3;
        for n in 0..total {
            append(dir.path(), &record(n)).unwrap();
        }
        let current = std::fs::metadata(dir.path().join(ACTIVITY_FILE))
            .unwrap()
            .len();
        let previous = std::fs::metadata(dir.path().join(PREVIOUS_FILE))
            .unwrap()
            .len();
        assert!(current <= MAX_FILE_BYTES && previous <= MAX_FILE_BYTES);
        let records = read(dir.path());
        // The oldest file was dropped at the second rotation; what is kept is the newest, in
        // order, with nothing missing in between.
        assert_eq!(records.last(), Some(&record(total - 1)));
        assert!(records.len() as u64 > MAX_FILE_BYTES / line && (records.len() as u64) < total);
        assert!(records.windows(2).all(|w| w[0].at_ms + 1 == w[1].at_ms));
    }

    // covers UP-16: an agent cannot flush the log with huge names: each field is cut, so the
    // records before stay.
    #[test]
    fn huge_fields_are_cut_and_do_not_flush_the_log() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..100 {
            append(dir.path(), &record(n)).unwrap();
        }
        let huge = ActivityRecord {
            name: Some("x".repeat(1_100_000)),
            namespace: Some("é".repeat(300)),
            ..record(100)
        };
        append(dir.path(), &huge).unwrap();
        append(dir.path(), &huge).unwrap();
        let records = read(dir.path());
        assert_eq!(records.len(), 102);
        let name = records[100].name.as_deref().unwrap();
        assert_eq!(name, format!("{}…(+1099747 bytes)", "x".repeat(253)));
        let namespace = records[100].namespace.as_deref().unwrap();
        assert!(namespace.starts_with(&"é".repeat(126)) && namespace.ends_with("…(+348 bytes)"));
    }

    // covers UP-16: writers in several processes (and threads) rotate in turn: no record is
    // lost but the ones the cap drops, and the kept ones are complete files' worth.
    #[test]
    fn concurrent_writers_rotate_without_losing_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let line = serde_json::to_vec(&record(0)).unwrap().len() as u64 + 1;
        let per_writer = MAX_FILE_BYTES / line;
        std::thread::scope(|s| {
            for w in 0..4u64 {
                let dir = dir.path();
                s.spawn(move || {
                    for n in 0..per_writer {
                        append(dir, &record(w * 1_000_000 + n)).unwrap();
                    }
                });
            }
        });
        // About four files' worth written, so two rotations at least: the previous file is
        // always a full one, never a near-empty one that replaced it.
        let previous = std::fs::metadata(dir.path().join(PREVIOUS_FILE))
            .unwrap()
            .len();
        assert!(previous > MAX_FILE_BYTES - 2 * line, "{previous}");
        let records = read(dir.path());
        let mut ids: Vec<i64> = records.iter().map(|r| r.at_ms).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), records.len(), "no record written twice");
    }

    #[test]
    fn reading_keeps_the_newest_records() {
        let dir = tempfile::tempdir().unwrap();
        let text: String = (0..MAX_RECORDS as u64 + 3)
            .map(|n| serde_json::to_string(&record(n)).unwrap() + "\n")
            .collect();
        std::fs::write(dir.path().join(ACTIVITY_FILE), text).unwrap();
        let records = read(dir.path());
        assert_eq!(records.len(), MAX_RECORDS);
        assert_eq!(records[0], record(3));
    }
}
