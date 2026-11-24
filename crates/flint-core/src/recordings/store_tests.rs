use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use super::identity::ConnectionIdentity;
use super::scrub::LogSource;
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
    let stored = stored_lines(&store, id);
    assert_eq!(stored.len(), 1);
    assert!(stored[0].starts_with("[REDACTED:cut-off] 2 lines"));
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

/// Whether another process sees a lock on SQLite's shared-lock bytes of `path`.
fn another_process_sees_a_lock(path: &str) -> bool {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    // SAFETY: `flock` is plain data, and `F_GETLK` only fills it in.
    unsafe {
        let mut lock: libc::flock = std::mem::zeroed();
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 0x4000_0002;
        lock.l_len = 510;
        assert_eq!(libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock), 0);
        let unlocked: libc::c_short = libc::F_UNLCK as _;
        lock.l_type != unlocked
    }
}

#[test]
fn opening_keeps_sqlite_s_lock_on_the_database() {
    const PROBE: &str = "FLINT_RECORDINGS_LOCK_PROBE";
    if let Ok(path) = std::env::var(PROBE) {
        assert!(another_process_sees_a_lock(&path));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "recordings::store_tests::opening_keeps_sqlite_s_lock_on_the_database",
        ])
        .env(PROBE, dir.path().join(DATABASE_FILE))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    drop(store);
    assert!(status.success());
}

fn cut(at_ms: i64, text: &str) -> LogRow {
    STREAM.with(|s| s.borrow().row(at_ms, text.into(), true))
}

fn append_each(store: &mut Store, id: RecordingId, rows: Vec<LogRow>) {
    for row in rows {
        store.append(id, &identity(), lines(vec![row])).unwrap();
    }
}

#[test]
fn a_cut_line_that_lost_a_key_header_withholds_the_key_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let rows = vec![cut(1, "noise -----BEGIN PRIV"), line(2, "MIIEsecretbody")];
    append_each(&mut store, id, rows);
    assert!(
        stored_lines(&store, id)
            .iter()
            .all(|l| !l.contains("MIIEsecretbody"))
    );
}

#[test]
fn a_cut_line_with_no_visible_header_withholds_what_follows() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let rows = vec![cut(1, "plain prefix"), line(2, "MIIEsecretbody")];
    append_each(&mut store, id, rows);
    assert!(
        stored_lines(&store, id)
            .iter()
            .all(|l| !l.contains("MIIEsecretbody"))
    );
}

#[test]
fn a_token_key_cut_before_its_value_does_not_store_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let rows = vec![cut(1, r#"{"token":"#), line(2, r#""tokenvaluesecret"}"#)];
    append_each(&mut store, id, rows);
    assert!(
        stored_lines(&store, id)
            .iter()
            .all(|l| !l.contains("tokenvaluesecret"))
    );
}

#[test]
fn a_value_cut_in_the_middle_is_not_stored() {
    for text in [
        r#"{"password":"my partial secret"#,
        r#"{"token":"partialsecretvalue"#,
        "connect https://admin:partialsecret",
        "auth eyJhbGciOiJIUzI1NiJ9.partialsecret",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let id = running(&mut store, "a");
        append_each(&mut store, id, vec![cut(1, text)]);
        let stored = stored_lines(&store, id).join("\n");
        assert!(!stored.contains("partial"), "stored {stored:?}");
    }
}

#[test]
fn a_full_line_longer_than_the_limit_is_scrubbed_before_it_is_clipped() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let pad = "x ".repeat(crate::logs::MAX_LINE_BYTES / 2 - 8);
    let rows = vec![
        line(1, &format!("{pad}-----BEGIN PRIVATE KEY-----")),
        line(2, "MIIEsecretbody"),
    ];
    append_each(&mut store, id, rows);
    assert!(
        stored_lines(&store, id)
            .iter()
            .all(|l| !l.contains("MIIEsecretbody"))
    );
}

#[test]
fn a_cookie_array_does_not_end_the_quarantine_of_a_cut_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let rows = vec![
        cut(1, "noise"),
        line(2, r#""cookie": ["#),
        line(3, "]"),
        line(4, "MIIEsecretbody"),
    ];
    append_each(&mut store, id, rows);
    assert!(
        stored_lines(&store, id)
            .iter()
            .all(|l| !l.contains("MIIEsecretbody"))
    );
}

#[test]
fn a_write_is_retried_while_another_connection_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    store.set_busy_timeout(Duration::from_millis(100));
    let path = dir.path().join(DATABASE_FILE);
    let (locked, wait) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let blocker = rusqlite::Connection::open(path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        blocker.execute_batch("ROLLBACK").unwrap();
    });
    wait.recv().unwrap();
    let appended = store
        .append(id, &identity(), lines(vec![line(1, "kept")]))
        .unwrap();
    holder.join().unwrap();
    assert_eq!(appended.stored, 1);
    assert_eq!(stored_lines(&store, id), ["kept"]);
}

#[test]
fn sqlite_is_built_without_extension_loading() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let options: Vec<String> = store
        .connection()
        .prepare("PRAGMA compile_options")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(options.iter().any(|o| o == "OMIT_LOAD_EXTENSION"));
}

#[test]
fn a_second_line_does_not_evict_itself_under_a_small_budget() {
    let dir = tempfile::tempdir().unwrap();
    let retention = Retention {
        max_recording_bytes: 16 * 1024,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let id = running(&mut store, "a");
    let (first, second) = ("a".repeat(8 * 1024), "b".repeat(8 * 1024));
    append_each(&mut store, id, vec![line(1, &first), line(2, &second)]);
    assert_eq!(stored_lines(&store, id), [second]);
}

#[test]
fn closed_gaps_are_charged_capped_and_expire() {
    let dir = tempfile::tempdir().unwrap();
    let retention = Retention {
        max_age: Duration::from_millis(DAY_MS as u64),
        max_recording_bytes: 16 * 1024,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let id = running(&mut store, "a");
    for at in 0..2000 {
        store.pause(id, GapReason::Disconnected, at).unwrap();
        store.resume(id, at).unwrap();
    }
    let gaps = |store: &Store| -> i64 {
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM gaps", [], |r| r.get(0))
            .unwrap()
    };
    assert!(gaps(&store) < 2000);
    assert!(store.info(id).unwrap().bytes > 0);
    assert!(store.info(id).unwrap().bytes <= 16 * 1024);
    store.apply_retention(3 * DAY_MS).unwrap();
    assert_eq!(gaps(&store), 0);
    assert_eq!(store.info(id).unwrap().bytes, 0);
}

#[test]
fn a_pinned_reader_cannot_grow_the_log_past_its_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let budget = 2 * 1024 * 1024;
    let retention = Retention {
        max_total_bytes: budget,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let id = running(&mut store, "a");
    let text = "w".repeat(1000);
    store
        .append(id, &identity(), lines(vec![line(0, &text)]))
        .unwrap();
    let reader = rusqlite::Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM log_lines", [], |r| r.get(0))
        .unwrap();
    for at in 1..3000 {
        store
            .append(id, &identity(), lines(vec![line(at, &text)]))
            .unwrap();
    }
    let wal = dir.path().join(format!("{DATABASE_FILE}-wal"));
    let size = std::fs::metadata(&wal).unwrap().len();
    assert!(size <= budget / 8 + 128 * 1024, "log is {size} bytes");
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    let before = store.info(id).unwrap().dropped_rows;
    assert!(before > 0);
    restart_stream();
    let appended = store
        .append(id, &identity(), lines(vec![line(4000, "after")]))
        .unwrap();
    assert_eq!(appended.stored, 1);
}

fn pod(name: &str) -> RecordingTarget {
    RecordingTarget::Pod { name: name.into() }
}

thread_local! {
    /// The log stream [`line`] and [`cut`] belong to.
    static STREAM: std::cell::RefCell<LogSource> = std::cell::RefCell::new(LogSource::new("p", "c"));
}

/// The stream really restarted.
fn restart_stream() {
    STREAM.with(|s| *s.borrow_mut() = LogSource::new("p", "c"));
}

fn line(at_ms: i64, text: &str) -> LogRow {
    STREAM.with(|s| s.borrow().row(at_ms, text.into(), false))
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
    restart_stream();
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
    restart_stream();
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
fn an_overlong_line_is_stored_only_as_a_cut_marker() {
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
    assert!(stored.starts_with("[REDACTED:cut-off] 1 lines"));
    assert!(!stored.contains('é'));
    assert!(cut);
}

#[test]
fn an_overlong_event_message_is_stored_only_as_a_cut_marker() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let batch = Batch {
        events: vec![EventRow {
            at_ms: 1,
            kind: EventKind::Warning,
            reason: "Failed".into(),
            object_kind: "Pod".into(),
            object_name: "p".into(),
            message: format!("token 'SECRET{}", "x".repeat(crate::logs::MAX_LINE_BYTES)),
            count: 1,
        }],
        ..Batch::default()
    };
    store.append(id, &identity(), batch).unwrap();
    let message: String = store
        .connection()
        .query_row("SELECT message FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(message, "[REDACTED:cut-off]");
}

#[test]
fn a_forged_pem_footer_after_a_cut_does_not_resume_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let rows = vec![
        cut(1, "noise"),
        line(2, "-----BEGIN PRIVATE KEY-----"),
        line(3, "-----END PRIVATE KEY-----"),
        line(4, "SECRETBODY"),
    ];
    append_each(&mut store, id, rows);
    let stored = stored_lines(&store, id);
    assert_eq!(stored.len(), 1);
    assert!(stored[0].contains("4 lines not recorded"));
}

#[test]
fn a_600_line_pem_after_a_cut_stores_none_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let mut rows = vec![cut(1, "noise")];
    rows.extend((0..600).map(|at| line(at + 2, &format!("SECRETBODY{at}"))));
    append_each(&mut store, id, rows);
    let stored = stored_lines(&store, id);
    assert_eq!(stored.len(), 1);
    assert!(stored[0].contains("601 lines not recorded"));
    assert_eq!(store.info(id).unwrap().dropped_rows, 601);
}

#[test]
fn a_cut_record_stores_no_part_of_a_quoted_value() {
    for text in [
        "--token 'opaque,continued",
        r#"{"token":"opaque,continued"#,
        r#"{"a":"opaque"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let id = running(&mut store, "a");
        append_each(&mut store, id, vec![cut(1, text)]);
        let stored = stored_lines(&store, id).join("\n");
        assert!(!stored.contains("opaque"), "stored {stored:?}");
    }
}

#[test]
fn a_token_value_on_the_row_after_its_key_is_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let id = running(&mut store, "a");
    let rows = vec![line(1, r#""token":"#), line(2, r#""SECRETVALUE""#)];
    append_each(&mut store, id, rows);
    assert!(
        stored_lines(&store, id)
            .iter()
            .all(|l| !l.contains("SECRETVALUE"))
    );
}

#[test]
fn a_starred_recording_stores_nothing_of_a_stream_after_its_budget_drop() {
    let dir = tempfile::tempdir().unwrap();
    let retention = Retention {
        max_recording_bytes: 16 * 1024,
        ..Retention::default()
    };
    let mut store = Store::open(dir.path(), retention).unwrap();
    let id = running(&mut store, "a");
    store.set_starred(id, true).unwrap();
    let big = "x".repeat(32 * 1024);
    let batch = lines(vec![line(1, &big), line(2, "SECRETBODY")]);
    store.append(id, &identity(), batch).unwrap();
    append_each(&mut store, id, vec![line(3, "SECRETBODY")]);
    assert!(
        stored_lines(&store, id)
            .iter()
            .all(|l| !l.contains("SECRETBODY"))
    );
    assert_eq!(store.info(id).unwrap().dropped_rows, 3);
}

/// A store whose write-ahead log a reader keeps from being reclaimed, already past its limit.
fn pinned(dir: &Path) -> (Store, RecordingId, rusqlite::Connection) {
    let retention = Retention {
        max_total_bytes: 1024 * 1024,
        ..Retention::default()
    };
    let mut store = Store::open(dir, retention).unwrap();
    let id = running(&mut store, "a");
    append_each(&mut store, id, vec![line(0, "first")]);
    let reader = rusqlite::Connection::open(dir.join(DATABASE_FILE)).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM log_lines", [], |r| r.get(0))
        .unwrap();
    let text = "w".repeat(1000);
    for at in 1..1000 {
        let _ = store.append(id, &identity(), lines(vec![line(at, &text)]));
    }
    (store, id, reader)
}

#[test]
fn every_write_respects_a_pinned_log() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, id, reader) = pinned(dir.path());
    let wal = dir.path().join(format!("{DATABASE_FILE}-wal"));
    let before = std::fs::metadata(&wal).unwrap().len();
    for _ in 0..200 {
        assert!(matches!(
            store.set_starred(id, true),
            Err(RecordingError::LogFull)
        ));
    }
    assert!(matches!(store.resume(id, 1), Err(RecordingError::LogFull)));
    assert!(matches!(store.stop(id, 1), Err(RecordingError::LogFull)));
    assert!(matches!(store.delete(id), Err(RecordingError::LogFull)));
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), before);
    drop(reader);
    store.set_starred(id, true).unwrap();
}

#[test]
fn side_files_keep_a_path_that_is_not_utf8() {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    let raw = std::ffi::OsString::from_vec(b"/tmp/rec\xff/recordings.sqlite".to_vec());
    let wal = side_file(Path::new(&raw), "-wal");
    assert_eq!(
        wal.as_os_str().as_bytes(),
        b"/tmp/rec\xff/recordings.sqlite-wal"
    );
}

#[test]
fn first_opens_wait_for_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let held = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = dir.path().to_owned();
    let opening = std::thread::spawn(move || {
        Store::open(&path, Retention::default()).unwrap();
    });
    std::thread::sleep(Duration::from_millis(50));
    assert!(!dir.path().join(DATABASE_FILE).exists());
    drop(held);
    opening.join().unwrap();
    assert!(dir.path().join(DATABASE_FILE).exists());
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
