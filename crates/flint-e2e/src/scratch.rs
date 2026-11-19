//! Scratch directories for a Flint process: a private (0700) directory with a short path, so the
//! automation socket in it stays under macOS's 103-byte limit, holding the config directory, a
//! home directory and logs. Nothing here touches the user's own files.
//!
//! A test process killed before it cleans up leaves its directory behind; the next
//! [`Scratch::new`] removes directories whose owning process is gone.

use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// The settings every scratch config starts from: a fixed `PATH`, so no login shell runs.
pub const BASE_SETTINGS: &str = "path = \"/usr/bin:/bin:/usr/sbin:/sbin\"\n";

/// Scratch directories are named `<PREFIX>XXXXXX`.
const PREFIX: &str = "flint-e2e-";
/// The file in a scratch directory naming the process that made it.
const OWNER: &str = "owner.pid";

/// A private scratch directory, removed when dropped.
#[derive(Debug)]
pub struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    /// A new directory under `/tmp` (short, unlike macOS's per-user `$TMPDIR`), mode 0700, with
    /// `config/` and `home/` inside. Removes scratch directories left by dead test processes.
    pub fn new() -> Result<Self> {
        let base = base_dir();
        sweep_stale(&base);
        let dir = tempfile::Builder::new()
            .prefix(PREFIX)
            .tempdir_in(&base)
            .with_context(|| format!("creating a scratch directory in {}", base.display()))?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        std::fs::write(dir.path().join(OWNER), std::process::id().to_string())?;
        let scratch = Scratch { dir };
        for sub in ["config", "home"] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(scratch.path().join(sub))?;
        }
        Ok(scratch)
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The scratch `FLINT_CONFIG_DIR`.
    pub fn config_dir(&self) -> PathBuf {
        self.path().join("config")
    }

    /// The scratch `HOME`, so nothing falls back to the user's `~/.kube`.
    pub fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    /// Writes `settings.toml` in the config directory (see [`write_settings`]).
    pub fn write_settings(&self, settings: &str) -> Result<()> {
        write_settings(&self.config_dir(), settings)
    }
}

fn base_dir() -> PathBuf {
    if Path::new("/tmp").is_dir() {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    }
}

/// Removes this user's scratch directories in `base` whose owning process has exited.
fn sweep_stale(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    for entry in entries.flatten() {
        let path = entry.path();
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PREFIX));
        let mine = path
            .symlink_metadata()
            .is_ok_and(|m| m.is_dir() && m.uid() == me);
        if !named || !mine {
            continue;
        }
        let owner = std::fs::read_to_string(path.join(OWNER))
            .ok()
            .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok());
        if owner.is_some_and(|pid| !alive(pid)) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Writes `settings.toml` in `config_dir`, atomically (a temporary file renamed into place, so
/// a watching app never reads half a file): [`BASE_SETTINGS`] then `settings`, unless
/// `settings` sets the top-level `path` itself. Malformed `settings` are written as given, after
/// the base.
pub fn write_settings(config_dir: &Path, settings: &str) -> Result<()> {
    let text = if sets_path(settings) {
        settings.to_owned()
    } else {
        format!("{BASE_SETTINGS}{settings}")
    };
    let file = config_dir.join("settings.toml");
    let temp = config_dir.join(".settings.toml.tmp");
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &file).with_context(|| format!("writing {}", file.display()))
}

/// Whether TOML text sets the top-level `path` key.
fn sets_path(settings: &str) -> bool {
    settings
        .parse::<toml::Table>()
        .is_ok_and(|table| table.contains_key("path"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt as _;

    use super::*;

    #[test]
    fn scratch_is_private_and_short() {
        let scratch = Scratch::new().unwrap();
        let mode = std::fs::metadata(scratch.path()).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let socket = std::fs::canonicalize(scratch.config_dir())
            .unwrap()
            .join("automation.sock");
        assert!(socket.as_os_str().as_bytes().len() <= 103, "{socket:?}");
        assert!(scratch.home().is_dir());
    }

    #[test]
    fn settings_get_a_fixed_path_unless_they_set_one() {
        let scratch = Scratch::new().unwrap();
        let read = || std::fs::read_to_string(scratch.config_dir().join("settings.toml")).unwrap();
        scratch
            .write_settings("watch_mode = \"on_view\"\n")
            .unwrap();
        assert_eq!(read(), format!("{BASE_SETTINGS}watch_mode = \"on_view\"\n"));
        scratch.write_settings("path = \"/bin\"\n").unwrap();
        assert_eq!(read(), "path = \"/bin\"\n");
        // A `path` inside a table is not the top-level key.
        let nested = "[[ai_exposed_clusters]]\ncontext = \"a\"\npath = \"x\"\n";
        scratch.write_settings(nested).unwrap();
        assert_eq!(read(), format!("{BASE_SETTINGS}{nested}"));
        scratch.write_settings("pathological = 1\n").unwrap();
        assert!(read().starts_with(BASE_SETTINGS));
    }

    #[test]
    fn directories_of_dead_processes_are_swept() {
        let base = tempfile::tempdir().unwrap();
        let dead = base.path().join(format!("{PREFIX}dead"));
        let live = base.path().join(format!("{PREFIX}live"));
        let other = base.path().join("unrelated");
        for dir in [&dead, &live, &other] {
            std::fs::create_dir(dir).unwrap();
        }
        // A pid far above any real one.
        std::fs::write(dead.join(OWNER), "99999999").unwrap();
        std::fs::write(live.join(OWNER), std::process::id().to_string()).unwrap();
        std::fs::write(other.join(OWNER), "99999999").unwrap();
        sweep_stale(base.path());
        assert!(!dead.exists());
        assert!(live.exists());
        assert!(other.exists());
    }
}
