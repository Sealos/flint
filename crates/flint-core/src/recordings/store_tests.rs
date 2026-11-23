use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use super::identity::ConnectionIdentity;
use super::store::*;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn identity() -> ConnectionIdentity {
    ConnectionIdentity::for_tests(1)
}

#[test]
fn a_deleted_recordings_id_is_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    store.delete(id).unwrap();
    assert_ne!(store.start(&identity(), "ns", &pod("b"), 1).unwrap(), id);
}

#[test]
fn a_recording_left_running_is_ended_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    store
        .append(
            id,
            &identity(),
            lines(vec![line(1, "-----BEGIN PRIVATE KEY-----")]),
        )
        .unwrap();
    drop(store);
    let mut store = open(dir.path());
    assert!(matches!(
        store.append(id, &identity(), lines(vec![line(2, "MIIEsecretbody")])),
        Err(RecordingError::Ended)
    ));
    assert_ne!(store.start(&identity(), "ns", &pod("a"), 3).unwrap(), id);
}

#[test]
fn starred_data_near_the_budget_does_not_wipe_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let budget = 2 * 1024 * 1024;
    let retention = Retention {
        max_total_bytes: budget,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let starred = running(&mut store, "kept");
    store.set_starred(starred, true).unwrap();
    let other = store.start(&identity(), "ns", &pod("other"), 1).unwrap();
    // One row to a page: the starred rows alone take the file past its 90% mark.
    let text = "s".repeat(3000);
    for at in 0..410 {
        store
            .append(starred, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    assert!(store.used_bytes().unwrap() > (budget - budget / 8) / 10 * 9);
    for at in 410..520 {
        store
            .append(other, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    assert!(count(&store, "log_lines", other) > 0);
    assert_eq!(
        store
            .connection()
            .query_row(
                "SELECT MAX(at_ms) FROM log_lines WHERE recording_id = ?1",
                [other.0],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        519
    );
}

#[test]
fn the_database_and_its_log_stay_within_the_total_budget() {
    let dir = tempfile::tempdir().unwrap();
    let budget = 2 * 1024 * 1024;
    let retention = Retention {
        max_total_bytes: budget,
        max_age: Duration::from_millis(1),
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let id = running(&mut store, "a");
    let text = "w".repeat(1000);
    for at in 0..3000 {
        store
            .append(id, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    store.apply_retention(10_000).unwrap();
    let size = |name: &str| std::fs::metadata(dir.path().join(name)).map_or(0, |m| m.len());
    let wal = format!("{DATABASE_FILE}-wal");
    assert!(size(DATABASE_FILE) + size(&wal) <= budget);
}

#[test]
fn a_recordings_pages_stay_near_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let limit = 256 * 1024;
    let retention = Retention {
        max_recording_bytes: limit,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let empty = store.used_bytes().unwrap();
    let id = running(&mut store, "a");
    let text = "m".repeat(1000);
    for at in 0..1000 {
        store
            .append(id, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    assert!(store.used_bytes().unwrap() - empty <= limit + 16 * 1024);
}

#[test]
fn a_key_header_seen_while_paused_still_withholds_its_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    store.pause(id, GapReason::Disconnected, 1).unwrap();
    let header = lines(vec![line(2, "-----BEGIN PRIVATE KEY-----")]);
    assert!(store.append(id, &identity(), header).is_err());
    store.resume(id, 3).unwrap();
    store
        .append(id, &identity(), lines(vec![line(4, "MIIEsecretbody")]))
        .unwrap();
    assert_eq!(stored_lines(&store, id), ["[REDACTED:private-key]"]);
}

#[test]
fn a_refused_connection_does_not_end_an_open_key_block() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let header = lines(vec![line(1, "-----BEGIN PRIVATE KEY-----")]);
    store.append(id, &identity(), header).unwrap();
    let moved = ConnectionIdentity::for_tests(2);
    assert!(store.append(id, &moved, lines(vec![line(2, "x")])).is_err());
    store
        .append(id, &identity(), lines(vec![line(3, "MIIEsecretbody")]))
        .unwrap();
    assert!(
        stored_lines(&store, id)
            .iter()
            .all(|l| !l.contains("MIIEsecretbody"))
    );
}

fn pod(name: &str) -> RecordingTarget {
    RecordingTarget::Pod { name: name.into() }
}

fn line(at_ms: i64, text: &str) -> LogRow {
    LogRow {
        at_ms,
        pod: "p".into(),
        container: "c".into(),
        text: text.into(),
        truncated: false,
    }
}

fn lines(rows: Vec<LogRow>) -> Batch {
    Batch {
        lines: rows,
        ..Batch::default()
    }
}

fn open(dir: &Path) -> Store {
    Store::open(dir, Retention::default()).unwrap()
}

fn running(store: &mut Store, name: &str) -> RecordingId {
    store.set_enabled(&identity(), "dev", true, 0).unwrap();
    store.start(&identity(), "ns", &pod(name), 0).unwrap()
}

fn count(store: &Store, table: &str, id: RecordingId) -> i64 {
    store
        .connection()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE recording_id = ?1"),
            [id.0],
            |row| row.get(0),
        )
        .unwrap()
}

fn stored_lines(store: &Store, id: RecordingId) -> Vec<String> {
    store
        .connection()
        .prepare("SELECT text FROM log_lines WHERE recording_id = ?1 ORDER BY at_ms, id")
        .unwrap()
        .query_map([id.0], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn recording_is_off_until_turned_on_for_that_connection() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    assert!(!store.is_enabled(&identity()).unwrap());
    assert!(matches!(
        store.start(&identity(), "ns", &pod("a"), 0),
        Err(RecordingError::Disabled)
    ));
    store.set_enabled(&identity(), "dev", true, 0).unwrap();
    assert!(store.is_enabled(&identity()).unwrap());
    assert!(!store.is_enabled(&ConnectionIdentity::for_tests(2)).unwrap());
}

#[test]
fn a_re_pointed_context_never_appends_to_the_old_recording() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let moved = ConnectionIdentity::for_tests(2);
    assert!(matches!(
        store.append(id, &moved, lines(vec![line(1, "other cluster")])),
        Err(RecordingError::ConnectionChanged)
    ));
    assert_eq!(count(&store, "log_lines", id), 0);
    assert!(matches!(
        store.start(&moved, "ns", &pod("a"), 1),
        Err(RecordingError::Disabled)
    ));
    store.set_enabled(&moved, "dev", true, 1).unwrap();
    assert_ne!(store.start(&moved, "ns", &pod("a"), 1).unwrap(), id);
}

#[test]
fn starting_a_running_target_returns_its_recording() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    assert_eq!(store.start(&identity(), "ns", &pod("a"), 5).unwrap(), id);
    store.stop(id, 6).unwrap();
    assert_ne!(store.start(&identity(), "ns", &pod("a"), 7).unwrap(), id);
}

#[test]
fn turning_a_cluster_off_ends_its_recordings() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    store.pause(id, GapReason::Disconnected, 1).unwrap();
    store.set_enabled(&identity(), "dev", false, 2).unwrap();
    let info = store.info(id).unwrap();
    assert_eq!(info.ended_ms, Some(2));
    assert!(!info.paused);
    store.set_enabled(&identity(), "dev", true, 3).unwrap();
    assert!(matches!(
        store.append(id, &identity(), lines(vec![line(4, "x")])),
        Err(RecordingError::Ended)
    ));
}

#[test]
fn logs_events_and_samples_land_in_typed_tables() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let batch = Batch {
        lines: vec![line(1, "hello")],
        events: vec![EventRow {
            at_ms: 2,
            kind: EventKind::Warning,
            reason: "BackOff".into(),
            object_kind: "Pod".into(),
            object_name: "p".into(),
            message: "restarting".into(),
            count: 3,
        }],
        samples: vec![SampleRow {
            at_ms: 3,
            pod: "p".into(),
            cpu_millicores: 250,
            memory_bytes: 1024,
        }],
    };
    let appended = store.append(id, &identity(), batch).unwrap();
    assert_eq!(appended.stored, 3);
    let (kind, n): (String, i64) = store
        .connection()
        .query_row("SELECT kind, count FROM events", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((kind.as_str(), n), ("warning", 3));
    let cpu: i64 = store
        .connection()
        .query_row("SELECT cpu_millicores FROM metric_samples", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(cpu, 250);
}

#[test]
fn no_column_holds_json() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let sql: Vec<String> = store
        .connection()
        .prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!sql.is_empty());
    assert!(sql.iter().all(|s| !s.to_lowercase().contains("json")));
}

#[test]
fn credentials_are_replaced_before_storage() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let batch = Batch {
        lines: vec![line(1, "Authorization: Bearer abcdefghijklmnop0123")],
        events: vec![EventRow {
            at_ms: 2,
            kind: EventKind::Normal,
            reason: "Pulled".into(),
            object_kind: "Pod".into(),
            object_name: "p".into(),
            message: "failed with password=hunter2secret".into(),
            count: 1,
        }],
        samples: vec![],
    };
    store.append(id, &identity(), batch).unwrap();
    drop(store);
    let mut raw = Vec::new();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        raw.extend(std::fs::read(entry.unwrap().path()).unwrap());
    }
    let find = |needle: &[u8]| raw.windows(needle.len()).any(|w| w == needle);
    assert!(find(b"[REDACTED:"));
    assert!(!find(b"abcdefghijklmnop0123"));
    assert!(!find(b"hunter2secret"));
}

#[test]
fn a_key_logged_over_several_appends_is_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    for text in [
        "-----BEGIN PRIVATE KEY-----",
        "MIIEsecretbody",
        "-----END PRIVATE KEY-----",
        "after",
    ] {
        store
            .append(id, &identity(), lines(vec![line(1, text)]))
            .unwrap();
    }
    let stored = stored_lines(&store, id);
    assert!(stored.iter().all(|l| !l.contains("MIIEsecretbody")));
    assert_eq!(stored.last().unwrap(), "after");
}

#[test]
fn the_database_and_its_side_files_are_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    store
        .append(id, &identity(), lines(vec![line(1, "x")]))
        .unwrap();
    let mut seen = 0;
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let meta = entry.unwrap().metadata().unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        seen += 1;
    }
    assert!(seen >= 2, "expected the database and its WAL");
}

#[test]
fn loose_permissions_are_tightened_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DATABASE_FILE);
    drop(open(dir.path()));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    drop(open(dir.path()));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_linked_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::write(&elsewhere, b"").unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::os::unix::fs::symlink(&elsewhere, data.join(DATABASE_FILE)).unwrap();
    assert!(matches!(
        Store::open(&data, Retention::default()),
        Err(RecordingError::NotAFile)
    ));
}

#[test]
fn a_database_of_another_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store
        .connection()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    drop(store);
    assert!(matches!(
        Store::open(dir.path(), Retention::default()),
        Err(RecordingError::UnknownSchema(99))
    ));
}

#[test]
fn the_recording_budget_evicts_its_oldest_rows() {
    let dir = tempfile::tempdir().unwrap();
    let retention = Retention {
        max_recording_bytes: 32 * 1024,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let id = running(&mut store, "a");
    let text = "x".repeat(1024);
    for at in 0..100 {
        store
            .append(id, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    let info = store.info(id).unwrap();
    assert!(info.bytes <= 32 * 1024, "{} bytes", info.bytes);
    let newest: i64 = store
        .connection()
        .query_row("SELECT MAX(at_ms) FROM log_lines", [], |r| r.get(0))
        .unwrap();
    let oldest: i64 = store
        .connection()
        .query_row("SELECT MIN(at_ms) FROM log_lines", [], |r| r.get(0))
        .unwrap();
    assert_eq!(newest, 99);
    assert!(oldest > 0);
}

#[test]
fn a_starred_recording_keeps_its_rows_and_drops_new_ones() {
    let dir = tempfile::tempdir().unwrap();
    let retention = Retention {
        max_recording_bytes: 32 * 1024,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let id = running(&mut store, "a");
    store.set_starred(id, true).unwrap();
    let text = "x".repeat(1024);
    for at in 0..100 {
        store
            .append(id, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    let info = store.info(id).unwrap();
    assert!(info.bytes <= 32 * 1024);
    assert!(info.dropped_rows > 0);
    let oldest: i64 = store
        .connection()
        .query_row("SELECT MIN(at_ms) FROM log_lines", [], |r| r.get(0))
        .unwrap();
    assert_eq!(oldest, 0);
}

#[test]
fn the_total_budget_holds_and_takes_the_oldest_recording_first() {
    let dir = tempfile::tempdir().unwrap();
    let budget = 2 * 1024 * 1024;
    let retention = Retention {
        max_total_bytes: budget,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    store.set_enabled(&identity(), "dev", true, 0).unwrap();
    let old = store.start(&identity(), "ns", &pod("old"), 1).unwrap();
    let new = store.start(&identity(), "ns", &pod("new"), 2).unwrap();
    let text = "y".repeat(4096);
    for at in 0..300 {
        store
            .append(old, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    let before = count(&store, "log_lines", old);
    for at in 300..600 {
        store
            .append(new, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    assert!(store.used_bytes().unwrap() <= budget);
    assert!(count(&store, "log_lines", old) < before);
    assert_eq!(
        store
            .connection()
            .query_row(
                "SELECT MAX(at_ms) FROM log_lines WHERE recording_id = ?1",
                [new.0],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        599
    );
    let file = std::fs::metadata(dir.path().join(DATABASE_FILE))
        .unwrap()
        .len();
    assert!(file <= budget, "file is {file} bytes");
}

#[test]
fn starred_rows_survive_the_total_budget() {
    let dir = tempfile::tempdir().unwrap();
    let budget = 1024 * 1024;
    let retention = Retention {
        max_total_bytes: budget,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let starred = running(&mut store, "kept");
    let other = store.start(&identity(), "ns", &pod("other"), 1).unwrap();
    store.set_starred(starred, true).unwrap();
    let text = "z".repeat(4096);
    store
        .append(starred, &identity(), lines(vec![line(0, &text)]))
        .unwrap();
    for at in 1..500 {
        store
            .append(other, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    assert_eq!(count(&store, "log_lines", starred), 1);
    assert!(store.used_bytes().unwrap() <= budget);
}

#[test]
fn retention_removes_old_rows_but_not_starred_ones() {
    let dir = tempfile::tempdir().unwrap();
    let retention = Retention {
        max_age: Duration::from_millis(DAY_MS as u64),
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let plain = running(&mut store, "plain");
    let starred = store.start(&identity(), "ns", &pod("starred"), 0).unwrap();
    store.set_starred(starred, true).unwrap();
    for id in [plain, starred] {
        store
            .append(
                id,
                &identity(),
                lines(vec![line(0, "old"), line(2 * DAY_MS, "new")]),
            )
            .unwrap();
    }
    let before = store.info(plain).unwrap().bytes;
    store.apply_retention(2 * DAY_MS + 1).unwrap();
    assert_eq!(stored_lines(&store, plain), ["new"]);
    assert_eq!(stored_lines(&store, starred), ["old", "new"]);
    assert!(store.info(plain).unwrap().bytes < before);
}

#[test]
fn retention_removes_an_ended_recording_left_empty() {
    let dir = tempfile::tempdir().unwrap();
    let retention = Retention {
        max_age: Duration::from_millis(DAY_MS as u64),
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let id = running(&mut store, "a");
    store
        .append(id, &identity(), lines(vec![line(0, "old")]))
        .unwrap();
    store.stop(id, 1).unwrap();
    store.apply_retention(3 * DAY_MS).unwrap();
    assert!(matches!(
        store.info(id),
        Err(RecordingError::UnknownRecording)
    ));
}

#[test]
fn a_paused_recording_refuses_rows_and_marks_the_gap() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    store.pause(id, GapReason::SignInNeeded, 10).unwrap();
    store.pause(id, GapReason::SignInNeeded, 11).unwrap();
    assert!(store.info(id).unwrap().paused);
    assert!(matches!(
        store.append(id, &identity(), lines(vec![line(12, "x")])),
        Err(RecordingError::Paused)
    ));
    store.resume(id, 20).unwrap();
    store
        .append(id, &identity(), lines(vec![line(21, "x")]))
        .unwrap();
    let gap: (i64, i64, String) = store
        .connection()
        .query_row("SELECT from_ms, to_ms, reason FROM gaps", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(gap, (10, 20, "sign_in_needed".to_owned()));
}

#[test]
fn an_overlong_line_is_cut_at_a_character_and_marked() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let text = "é".repeat(crate::logs::MAX_LINE_BYTES);
    store
        .append(id, &identity(), lines(vec![line(1, &text)]))
        .unwrap();
    let (stored, cut): (String, bool) = store
        .connection()
        .query_row("SELECT text, truncated FROM log_lines", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(stored.len() <= crate::logs::MAX_LINE_BYTES);
    assert!(cut);
}

#[test]
fn deleting_a_recording_removes_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    store
        .append(id, &identity(), lines(vec![line(1, "x")]))
        .unwrap();
    store.delete(id).unwrap();
    assert_eq!(count(&store, "log_lines", id), 0);
    assert!(matches!(
        store.info(id),
        Err(RecordingError::UnknownRecording)
    ));
}
