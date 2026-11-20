//! Process diagnostics for the desktop app and the stdio MCP server.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

use tracing_subscriber::filter::{EnvFilter, FilterExt as _, filter_fn};
use tracing_subscriber::prelude::*;

use crate::redact::redact_credentials;

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const BACKUPS: usize = 3;
/// How long a log write waits for another process's write to finish before dropping the event.
const LOCK_WAIT: Duration = Duration::from_millis(50);
/// Events waiting for the writer thread. When it falls behind (a stalled disk, a held lock),
/// further events are dropped from the file instead of blocking the thread that emitted them.
const QUEUE: usize = 1024;
/// How long [`flush`] waits for queued events to reach the file.
const FLUSH_WAIT: Duration = Duration::from_millis(500);

static FILE_QUEUE: OnceLock<SyncSender<Message>> = OnceLock::new();

/// Sets up the global subscriber: stderr filtered by `stderr_filter`, and Flint's events to the
/// rotating log file filtered by `file_filter`.
pub(crate) fn init(stderr_filter: EnvFilter, file_filter: EnvFilter) {
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let file = log_path(
        cfg!(target_os = "macos"),
        std::env::home_dir().as_deref(),
        state_home.as_deref().map(Path::new),
    )
    .map(|path| FileLog::spawn(path, MAX_BYTES))
    .transpose();
    let (file, error) = match file {
        Ok(file) => (file, None),
        Err(error) => (None, Some(error)),
    };
    if let Some(file) = &file {
        let _ = FILE_QUEUE.set(file.queue.clone());
    }
    subscriber(stderr_filter, file_filter, file, io::stderr).init();
    if let Some(error) = error {
        tracing::warn!(%error, "could not open Flint's log file; logging to stderr");
    }
}

/// Waits briefly for queued events to reach the log file. Call it before the process exits.
pub(crate) fn flush() {
    if let Some(queue) = FILE_QUEUE.get() {
        flush_queue(queue);
    }
}

fn flush_queue(queue: &SyncSender<Message>) {
    let deadline = Instant::now() + FLUSH_WAIT;
    let (done, wait) = mpsc::sync_channel(1);
    let mut marker = Message::Flush(done);
    // A full queue may still drain within the budget: keep offering the marker until then.
    loop {
        match queue.try_send(marker) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(back)) if Instant::now() < deadline => {
                marker = back;
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return,
        }
    }
    let _ = wait.recv_timeout(deadline.saturating_duration_since(Instant::now()));
}

fn log_path(macos: bool, home: Option<&Path>, state_home: Option<&Path>) -> Option<PathBuf> {
    if macos {
        return home.map(|home| home.join("Library/Logs/Flint/flint.log"));
    }
    // XDG treats empty and relative paths as unset.
    state_home
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(".local/state")))
        .map(|state_home| state_home.join("flint/flint.log"))
}

fn subscriber<W: Write>(
    stderr_filter: EnvFilter,
    file_filter: EnvFilter,
    file: Option<FileLog>,
    stderr: impl Fn() -> W + Send + Sync + 'static,
) -> impl tracing::Subscriber + Send + Sync {
    let stderr = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(move || RedactingWriter(stderr()))
        .with_filter(stderr_filter);
    let file = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .log_internal_errors(false)
            .with_writer(file)
            // Dependencies may trace whole API responses or protocol messages. Keep only
            // Flint's diagnostics in the report file and omit span fields from dependencies.
            .with_filter(file_filter.and(filter_fn(|metadata| {
                metadata.is_event()
                    && matches!(
                        metadata.target().split("::").next(),
                        Some("flint" | "flint_app" | "flint_core" | "flint_mcp")
                    )
            })))
    });
    tracing_subscriber::registry().with(stderr).with(file)
}

struct RedactingWriter<W>(W);

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // The fmt layer hands its writer each formatted event in a single write_all.
        let text = String::from_utf8_lossy(bytes);
        self.0
            .write_all(redact_credentials(&text).text.as_bytes())?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

enum Message {
    Event(Vec<u8>),
    Flush(SyncSender<()>),
}

/// The file layer's writer: hands each formatted event to the writer thread, so no file or lock
/// work runs on the thread that emitted it (the UI thread, an MCP runtime worker).
struct FileLog {
    queue: SyncSender<Message>,
}

impl FileLog {
    fn spawn(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        Self::spawn_writer(FileWriter::open(path, max_bytes, LOCK_WAIT)?)
    }

    fn spawn_writer(mut writer: FileWriter) -> io::Result<Self> {
        let (queue, events) = mpsc::sync_channel(QUEUE);
        std::thread::Builder::new()
            .name("flint-log".into())
            .spawn(move || {
                for message in events {
                    match message {
                        // Redact here, off the emitting thread. A failed write drops the event;
                        // stderr keeps it.
                        Message::Event(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            let _ = writer.write(redact_credentials(&text).text.as_bytes());
                        }
                        Message::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self { queue })
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileLog {
    type Writer = &'a FileLog;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

impl Write for &FileLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // A full queue drops the event rather than wait for the writer thread.
        let _ = self.queue.try_send(Message::Event(bytes.to_vec()));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Appends events to the log file on the writer thread. The app and several MCP processes can
/// share the file: each event is written under a cross-process `flock`, and the file is reopened
/// each time so no writer keeps appending to a rotated file.
struct FileWriter {
    path: PathBuf,
    lock_path: PathBuf,
    lock: File,
    max_bytes: u64,
    lock_wait: Duration,
}

impl FileWriter {
    /// Creates the directory and checks the files can be opened. Takes no lock: opening with
    /// `O_APPEND` is safe while another process writes.
    fn open(path: PathBuf, max_bytes: u64, lock_wait: Duration) -> io::Result<Self> {
        fs::create_dir_all(path.parent().expect("log path has a parent directory"))?;
        let lock_path = path.with_extension("log.lock");
        let lock = open_private(&lock_path)?;
        open_private(&path)?;
        Ok(Self {
            path,
            lock_path,
            lock,
            max_bytes,
            lock_wait,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let event = if bytes.len() as u64 > self.max_bytes {
            b"[log event omitted: exceeds file size limit]\n"
        } else {
            bytes
        };
        // The directory may have been removed and recreated (by this or another process): lock
        // the lock file that is at the path now, so every writer shares one.
        if !is_same_file(&self.lock, &self.lock_path)? {
            fs::create_dir_all(self.path.parent().expect("log path has a parent directory"))?;
            self.lock = open_private(&self.lock_path)?;
        }
        let _lock = self.lock()?;
        let mut file = open_private(&self.path)?;
        if file.metadata()?.len() + event.len() as u64 > self.max_bytes {
            self.rotate()?;
            file = open_private(&self.path)?;
        }
        file.write_all(event)
    }

    /// Takes the cross-process lock, giving up after `lock_wait`: a process stopped while
    /// holding it must not stop this one from logging for good, only drop events meanwhile.
    fn lock(&self) -> io::Result<Locked<'_>> {
        let deadline = Instant::now() + self.lock_wait;
        loop {
            // SAFETY: the descriptor belongs to `self.lock`, which outlives the guard.
            if unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Locked(&self.lock));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                _ => return Err(error),
            }
        }
    }

    fn rotate(&self) -> io::Result<()> {
        let backup = |index| self.path.with_extension(format!("log.{index}"));
        for index in (1..BACKUPS).rev() {
            match fs::rename(backup(index), backup(index + 1)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        fs::rename(&self.path, backup(1))
    }
}

/// Whether `path` still names the open `file` (false when it is gone or replaced).
fn is_same_file(file: &File, path: &Path) -> io::Result<bool> {
    let open = file.metadata()?;
    match fs::symlink_metadata(path) {
        Ok(current) => Ok(open.dev() == current.dev() && open.ino() == current.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Holds the `flock` until dropped.
struct Locked<'a>(&'a File);

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: the descriptor belongs to the borrowed file.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn open_private(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("Flint's log path is not a regular file"));
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        file.set_permissions(Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_library_logs_even_with_xdg_state_home() {
        assert_eq!(
            log_path(
                true,
                Some(Path::new("/Users/test")),
                Some(Path::new("/xdg"))
            ),
            Some(PathBuf::from("/Users/test/Library/Logs/Flint/flint.log"))
        );
        assert_eq!(log_path(true, None, Some(Path::new("/xdg"))), None);
    }

    #[test]
    fn linux_uses_absolute_xdg_state_home_or_the_home_default() {
        assert_eq!(
            log_path(false, None, Some(Path::new("/xdg"))),
            Some(PathBuf::from("/xdg/flint/flint.log"))
        );
        for state_home in [None, Some(Path::new("")), Some(Path::new("relative"))] {
            assert_eq!(
                log_path(false, Some(Path::new("/home/test")), state_home),
                Some(PathBuf::from("/home/test/.local/state/flint/flint.log"))
            );
        }
        assert_eq!(log_path(false, None, None), None);
    }

    /// A writer that waits long enough for the lock that contention in tests never drops events.
    fn writer(path: &Path, max_bytes: u64) -> FileWriter {
        FileWriter::open(path.to_path_buf(), max_bytes, Duration::from_secs(10)).unwrap()
    }

    #[test]
    fn rotation_keeps_the_current_file_and_three_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/flint.log");
        // Two writers on one file, as the app and an MCP process would be.
        let mut writers = [writer(&path, 8), writer(&path, 8)];
        for n in 0..10 {
            writers[n % 2].write(format!("{n}!!\n").as_bytes()).unwrap();
        }
        let read = |path| fs::read_to_string(path).unwrap();
        assert_eq!(read(path.clone()), "8!!\n9!!\n");
        for (index, expected) in [(1, "6!!\n7!!\n"), (2, "4!!\n5!!\n"), (3, "2!!\n3!!\n")] {
            let backup = path.with_extension(format!("log.{index}"));
            assert_eq!(
                fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(read(backup), expected);
        }
        assert!(!path.with_extension("log.4").exists());
    }

    #[test]
    fn an_oversized_event_is_replaced_and_cannot_overflow_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flint.log");
        writer(&path, 64).write(&[b'x'; 65]).unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "[log event omitted: exceeds file size limit]\n"
        );
    }

    #[test]
    fn concurrent_writers_keep_every_event_during_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flint.log");
        let events = b"0123456789abcdef";
        std::thread::scope(|scope| {
            for chunk in events.chunks(4) {
                let mut writer = writer(&path, 8);
                scope.spawn(move || {
                    for &event in chunk {
                        writer.write(&[event, b'\n']).unwrap();
                    }
                });
            }
        });
        let mut recorded = Vec::new();
        for file in ["flint.log", "flint.log.1", "flint.log.2", "flint.log.3"] {
            let bytes = fs::read(dir.path().join(file)).unwrap();
            assert_eq!(bytes.len(), 8);
            for &[event, newline] in bytes.as_chunks::<2>().0 {
                assert_eq!(newline, b'\n');
                recorded.push(event);
            }
        }
        recorded.sort_unstable();
        assert_eq!(recorded, events);
    }

    #[test]
    fn a_removed_log_directory_is_recreated_and_its_lock_shared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/flint.log");
        let mut first = writer(&path, 1024);
        let mut second = writer(&path, 1024);
        fs::remove_dir_all(dir.path().join("logs")).unwrap();
        first.write(b"after\n").unwrap();
        second.write(b"again\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "after\nagain\n");
        // Both now lock the recreated lock file: one holding it makes the other wait.
        let held = first.lock().unwrap();
        second.lock_wait = Duration::ZERO;
        assert_eq!(
            second.write(b"dropped\n").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(held);
    }

    #[test]
    fn file_logs_use_their_filter_and_exclude_credentials_and_dependency_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flint.log");
        let log = FileLog::spawn(path.clone(), MAX_BYTES).unwrap();
        let queue = log.queue.clone();
        let subscriber = subscriber(
            EnvFilter::new("warn"),
            EnvFilter::new("info"),
            Some(log),
            io::sink,
        );
        tracing::subscriber::with_default(subscriber, || {
            let span =
                tracing::info_span!(target: "rmcp", "detail", raw = "dependency span payload");
            let _entered = span.enter();
            tracing::info!(target: "rmcp", "dependency event payload");
            tracing::debug!("filtered out");
            tracing::info!("auth failed with token=example-credential");
        });
        flush_queue(&queue);
        let log = fs::read_to_string(path).unwrap();
        assert!(
            log.contains("auth failed with token=[REDACTED:token]"),
            "{log}"
        );
        assert!(!log.contains("example-credential"), "{log}");
        assert!(!log.contains("dependency"), "{log}");
        assert!(!log.contains("filtered out"), "{log}");
    }

    #[test]
    fn a_held_lock_drops_the_event_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flint.log");
        let other = writer(&path, 1024);
        let mut writer = FileWriter::open(path.clone(), 1024, LOCK_WAIT).unwrap();
        let held = other.lock().unwrap();
        let started = Instant::now();
        let error = writer.write(b"dropped\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
        writer.write(b"kept\n").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "kept\n");
    }

    #[test]
    fn a_stalled_writer_never_blocks_the_emitting_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flint.log");
        let other = writer(&path, 1024);
        let held = other.lock().unwrap();
        // The writer thread waits for the held lock on every event; the queue fills up.
        let mut log = &FileLog::spawn_writer(writer(&path, 1024)).unwrap();
        let started = Instant::now();
        for _ in 0..QUEUE * 2 {
            log.write_all(b"event\n").unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(held);
    }
}
