//! Scratch directories for a Flint process: a private (0700) directory with a short path, so the
//! automation socket in it stays under macOS's 103-byte limit, holding the config directory, a
//! home directory and logs. Nothing here touches the user's own files.

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// The settings every scratch config starts from: a fixed `PATH`, so no login shell runs.
pub const BASE_SETTINGS: &str = "path = \"/usr/bin:/bin:/usr/sbin:/sbin\"\n";

/// A private scratch directory, removed when dropped.
#[derive(Debug)]
pub struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    /// A new directory under `/tmp` (short, unlike macOS's per-user `$TMPDIR`), mode 0700, with
    /// `config/` and `home/` inside.
    pub fn new() -> Result<Self> {
        let base = if Path::new("/tmp").is_dir() {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = tempfile::Builder::new()
            .prefix("flint-e2e-")
            .tempdir_in(&base)
            .with_context(|| format!("creating a scratch directory in {}", base.display()))?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
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

    /// Writes `settings.toml` in the config directory: [`BASE_SETTINGS`] unless `settings` sets
    /// `path` itself, then `settings`.
    pub fn write_settings(&self, settings: &str) -> Result<()> {
        write_settings(&self.config_dir(), settings)
    }
}

/// Writes `settings.toml` in `config_dir` (see [`Scratch::write_settings`]).
pub fn write_settings(config_dir: &Path, settings: &str) -> Result<()> {
    let sets_path = settings
        .lines()
        .any(|l| l.trim_start().starts_with("path ") || l.trim_start().starts_with("path="));
    let text = if sets_path {
        settings.to_owned()
    } else {
        format!("{BASE_SETTINGS}{settings}")
    };
    let file = config_dir.join("settings.toml");
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;

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
        scratch
            .write_settings("watch_mode = \"on_view\"\n")
            .unwrap();
        let text = std::fs::read_to_string(scratch.config_dir().join("settings.toml")).unwrap();
        assert!(text.starts_with(BASE_SETTINGS), "{text}");
        assert!(text.ends_with("watch_mode = \"on_view\"\n"), "{text}");
        scratch.write_settings("path = \"/bin\"\n").unwrap();
        let text = std::fs::read_to_string(scratch.config_dir().join("settings.toml")).unwrap();
        assert_eq!(text, "path = \"/bin\"\n");
    }
}
