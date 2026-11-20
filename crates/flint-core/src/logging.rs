//! Process diagnostics for the desktop app and the stdio MCP server.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use tracing_subscriber::filter::{EnvFilter, filter_fn};
use tracing_subscriber::prelude::*;

use crate::redact::redact_credentials;

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const BACKUPS: usize = 3;
/// How long a log write waits for another process's write to finish before dropping the event.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

pub(crate) fn init(filter: EnvFilter) {
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let file = log_path(
        cfg!(target_os = "macos"),
        std::env::home_dir().as_deref(),
        state_home.as_deref().map(Path::new),
    )
    .map(|path| FileLog::open(path, MAX_BYTES))
    .transpose();
    let (file, error) = match file {
        Ok(file) => (file, None),
        Err(error) => (None, Some(error)),
    };
    subscriber(filter, file, io::stderr).init();
    if let Some(error) = error {
        tracing::warn!(%error, "could not open Flint's log file; logging to stderr");
    }
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
    filter: EnvFilter,
    file: Option<FileLog>,
    stderr: impl Fn() -> W + Send + Sync + 'static,
) -> impl tracing::Subscriber + Send + Sync {
    let stderr = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(move || RedactingWriter(stderr()));
    let file = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .log_internal_errors(false)
            .with_writer(file)
            // Dependencies may trace whole API responses or protocol messages. Keep only
            // Flint's diagnostics in the report file and omit span fields from dependencies.
            .with_filter(filter_fn(|metadata| {
                metadata.is_event()
                    && matches!(
                        metadata.target().split("::").next(),
                        Some("flint" | "flint_app" | "flint_core" | "flint_mcp")
                    )
            }))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file)
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

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileLog {
    type Writer = RedactingWriter<&'a FileLog>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter(self)
    }
}

struct FileLog {
    path: PathBuf,
    lock_path: PathBuf,
    max_bytes: u64,
}

impl FileLog {
    fn open(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(path.parent().expect("log path has a parent directory"))?;
        let log = Self {
            lock_path: path.with_extension("log.lock"),
            path,
            max_bytes,
        };
        let _lock = log.lock()?;
        open_private(&log.path)?;
        Ok(log)
    }

    /// Takes the cross-process lock, giving up after [`LOCK_WAIT`]. Logging runs on whatever
    /// thread emits the event (the UI thread too), so a process stopped while holding the lock
    /// must never stall the others: the event is then dropped from the file (stderr keeps it).
    fn lock(&self) -> io::Result<File> {
        let file = open_private(&self.lock_path)?;
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            // SAFETY: the descriptor belongs to `file`. Closing it releases the lock.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(file);
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
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

impl Write for &FileLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let event = if bytes.len() as u64 > self.max_bytes {
            b"[log event omitted: exceeds file size limit]\n"
        } else {
            bytes
        };
        // The app and multiple MCP processes can log at once. Reopen the current file under
        // the same lock each time so no writer keeps appending to a rotated file.
        let _lock = self.lock()?;
        let mut file = open_private(&self.path)?;
        if file.metadata()?.len() + event.len() as u64 > self.max_bytes {
            self.rotate()?;
            file = open_private(&self.path)?;
        }
        file.write_all(event)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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

    #[test]
    fn rotation_keeps_the_current_file_and_three_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/flint.log");
        let first = FileLog::open(path.clone(), 8).unwrap();
        let second = FileLog::open(path.clone(), 8).unwrap();
        for (n, mut log) in [&first, &second].into_iter().cycle().take(10).enumerate() {
            log.write_all(format!("{n}!!\n").as_bytes()).unwrap();
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
        (&FileLog::open(path.clone(), 64).unwrap())
            .write_all(&[b'x'; 65])
            .unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "[log event omitted: exceeds file size limit]\n"
        );
    }

    #[test]
    fn concurrent_writes_keep_every_event_during_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let log = FileLog::open(dir.path().join("flint.log"), 8).unwrap();
        let events = b"0123456789abcdef";
        std::thread::scope(|scope| {
            for chunk in events.chunks(4) {
                let mut writer = &log;
                scope.spawn(move || {
                    for &event in chunk {
                        writer.write_all(&[event, b'\n']).unwrap();
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
    fn file_logs_use_the_env_filter_and_exclude_credentials_and_dependency_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flint.log");
        let log = FileLog::open(path.clone(), MAX_BYTES).unwrap();
        let subscriber = subscriber(EnvFilter::new("info"), Some(log), io::sink);
        tracing::subscriber::with_default(subscriber, || {
            let span =
                tracing::info_span!(target: "rmcp", "detail", raw = "dependency span payload");
            let _entered = span.enter();
            tracing::info!(target: "rmcp", "dependency event payload");
            tracing::debug!("filtered out");
            tracing::info!("auth failed with token=example-credential");
        });
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
        let log = FileLog::open(path.clone(), 1024).unwrap();
        let held = log.lock().unwrap();
        let started = std::time::Instant::now();
        let error = (&log).write_all(b"dropped\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(held);
        (&log).write_all(b"kept\n").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "kept\n");
    }
}
