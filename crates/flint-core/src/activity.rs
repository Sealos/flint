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
/// [`MAX_FILE_BYTES`]. Creates the directory if needed.
pub fn append(config_dir: &Path, record: &ActivityRecord) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    std::fs::create_dir_all(config_dir)?;
    let path = config_dir.join(ACTIVITY_FILE);
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    if size > 0 && size + line.len() as u64 > MAX_FILE_BYTES {
        // Another process may have rotated it a moment ago; then there is nothing to move.
        match std::fs::rename(&path, config_dir.join(PREVIOUS_FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(&line)
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
