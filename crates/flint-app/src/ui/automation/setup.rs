//! Where the channel may live, checked before the app starts: a scratch config directory that is
//! not the user's real one and that only the user can write, an explicit `KUBECONFIG` (or, without
//! one, a scratch `HOME` with no `.kube/config`, so the app never falls back to the user's
//! `~/.kube/config`), a socket path macOS can bind, and a screenshots directory. Any failure stops
//! the app with a reason, so a test harness fails fast.

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

/// The socket's file name in the config directory.
pub const SOCKET_NAME: &str = "automation.sock";

/// macOS's `sockaddr_un.sun_path` holds 104 bytes, the terminating NUL included.
pub const MAX_SOCKET_PATH: usize = 103;

/// Overrides where screenshots go: an absolute, existing directory the harness owns.
pub const SCREENSHOTS_ENV: &str = "FLINT_AUTOMATION_SCREENSHOTS";

/// The environment the checks read, passed in so tests need not change the process's.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub config_dir: Option<OsString>,
    pub home: Option<PathBuf>,
    /// The account's own home directory (from the user database, not `HOME`).
    pub account_home: Option<PathBuf>,
    /// `XDG_CONFIG_HOME`, which places the real config directory on Linux.
    pub xdg_config_home: Option<PathBuf>,
    pub kubeconfig: Option<OsString>,
    pub screenshots: Option<OsString>,
}

impl Env {
    pub fn from_process() -> Self {
        Env {
            config_dir: std::env::var_os("FLINT_CONFIG_DIR"),
            home: std::env::home_dir(),
            account_home: account_home(),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            kubeconfig: std::env::var_os("KUBECONFIG"),
            screenshots: std::env::var_os(SCREENSHOTS_ENV),
        }
    }
}

/// Where the channel lives, once checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    /// The scratch config directory, canonical.
    pub config_dir: PathBuf,
    pub socket: PathBuf,
    pub screenshots: PathBuf,
}

/// Checks the environment and prepares the directories (mode 0700 when created).
pub fn prepare(env: &Env) -> Result<Places, String> {
    let raw = env.config_dir.as_ref().filter(|d| !d.is_empty()).ok_or(
        "FLINT_CONFIG_DIR is not set; the automation channel needs a scratch config directory",
    )?;
    if env.kubeconfig.as_ref().is_none_or(|k| k.is_empty()) {
        check_scratch_home(env)?;
    }
    let dir = PathBuf::from(raw);
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    let config_dir =
        fs::canonicalize(&dir).map_err(|e| format!("could not resolve {}: {e}", dir.display()))?;
    if let Ok(real) =
        flint_core::settings::default_config_dir(env.xdg_config_home.clone(), env.home.clone())
    {
        let real = fs::canonicalize(&real).unwrap_or(real);
        if real == config_dir {
            return Err(format!(
                "FLINT_CONFIG_DIR is the real config directory ({}); use a scratch one",
                config_dir.display()
            ));
        }
    }
    check_private(&config_dir)?;
    let socket = config_dir.join(SOCKET_NAME);
    let length = socket.as_os_str().as_bytes().len();
    if length > MAX_SOCKET_PATH {
        return Err(format!(
            "the socket path {} is {length} bytes; macOS allows {MAX_SOCKET_PATH}. Use a shorter \
             FLINT_CONFIG_DIR",
            socket.display()
        ));
    }
    let screenshots = match env.screenshots.as_ref().filter(|s| !s.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if !dir.is_absolute() || !dir.is_dir() {
                return Err(format!(
                    "{SCREENSHOTS_ENV} must be an absolute, existing directory: {}",
                    dir.display()
                ));
            }
            check_owned(&dir)?;
            dir
        }
        None => {
            let dir = config_dir.join("screenshots");
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
            dir
        }
    };
    Ok(Places {
        config_dir,
        socket,
        screenshots,
    })
}

/// Without `KUBECONFIG` the app reads `$HOME/.kube/config`. That is allowed only when `HOME` is
/// a scratch directory (not the account's home) with no `.kube/config` in it, so a test can
/// launch "with no kubeconfig" without the app ever reading the user's own.
fn check_scratch_home(env: &Env) -> Result<(), String> {
    const REFUSED: &str = "KUBECONFIG is not set; the automation channel never falls back to \
                           ~/.kube/config";
    let (Some(home), Some(account)) = (&env.home, &env.account_home) else {
        return Err(REFUSED.to_owned());
    };
    let canonical = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    if canonical(home) == canonical(account) {
        return Err(format!("{REFUSED} (HOME is the account's home directory)"));
    }
    let fallback = home.join(".kube").join("config");
    if fallback.symlink_metadata().is_ok() {
        return Err(format!(
            "{REFUSED}, and {} exists; set KUBECONFIG explicitly",
            fallback.display()
        ));
    }
    Ok(())
}

/// The account's home directory from the user database, which `HOME` cannot change.
fn account_home() -> Option<PathBuf> {
    use std::ffi::{CStr, OsStr};
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `pwd` and `buf`, both ours and live for the call; the
    // strings it points `pwd` at live in `buf`, which outlives their use below.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    // SAFETY: `pw_dir` is a NUL-terminated string inside `buf`.
    let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
    Some(PathBuf::from(OsStr::from_bytes(dir.to_bytes())))
}

/// The directory belongs to this user and nobody else may write to it.
fn check_private(dir: &Path) -> Result<(), String> {
    let meta = check_owned(dir)?;
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is writable by other users (mode {:o}); the channel needs a private directory",
            dir.display(),
            meta.mode() & 0o777
        ));
    }
    Ok(())
}

fn check_owned(dir: &Path) -> Result<fs::Metadata, String> {
    let meta = fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        return Err(format!(
            "{} is not owned by the current user",
            dir.display()
        ));
    }
    Ok(meta)
}

/// Checks a screenshot name: a plain file name, no directories, no `..`, no hidden files. A
/// `.png` extension is added when missing.
pub fn screenshot_file(name: &str) -> Result<String, String> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        return Err(format!(
            "screenshot name {name:?} must be a plain file name: letters, digits, '-', '_' and \
             '.', no directories"
        ));
    }
    Ok(if name.ends_with(".png") {
        name.to_owned()
    } else {
        format!("{name}.png")
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn env(config: &Path, home: &Path) -> Env {
        Env {
            config_dir: Some(config.as_os_str().to_owned()),
            home: Some(home.to_owned()),
            account_home: Some("/Users/someone".into()),
            xdg_config_home: None,
            kubeconfig: Some("/tmp/kubeconfig".into()),
            screenshots: None,
        }
    }

    #[test]
    fn prepares_a_private_scratch_directory() {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let config = scratch.path().join("cfg");
        let places = prepare(&env(&config, home.path())).unwrap();
        assert_eq!(places.config_dir, fs::canonicalize(&config).unwrap());
        assert_eq!(places.socket, places.config_dir.join(SOCKET_NAME));
        assert_eq!(places.screenshots, places.config_dir.join("screenshots"));
        let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn refuses_the_real_config_directory_and_missing_variables() {
        let home = tempfile::tempdir().unwrap();
        let real =
            flint_core::settings::default_config_dir(None, Some(home.path().to_owned())).unwrap();
        fs::create_dir_all(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let error = prepare(&env(&real, home.path())).unwrap_err();
        assert!(error.contains("real config directory"), "{error}");

        let scratch = tempfile::tempdir().unwrap();
        let mut no_config = env(scratch.path(), home.path());
        no_config.config_dir = None;
        assert!(
            prepare(&no_config)
                .unwrap_err()
                .contains("FLINT_CONFIG_DIR")
        );
        let mut no_kubeconfig = env(scratch.path(), home.path());
        no_kubeconfig.kubeconfig = None;
        no_kubeconfig.account_home = Some(home.path().to_owned());
        assert!(prepare(&no_kubeconfig).unwrap_err().contains("KUBECONFIG"));
        no_kubeconfig.account_home = None;
        assert!(prepare(&no_kubeconfig).unwrap_err().contains("KUBECONFIG"));
    }

    // Without KUBECONFIG, a scratch HOME with nothing in .kube is allowed; one with a
    // .kube/config is not, since the app would read it.
    #[test]
    fn without_kubeconfig_home_must_be_a_scratch_one() {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let mut no_kubeconfig = env(scratch.path(), home.path());
        no_kubeconfig.kubeconfig = None;
        assert!(prepare(&no_kubeconfig).is_ok());
        fs::create_dir_all(home.path().join(".kube")).unwrap();
        fs::write(home.path().join(".kube/config"), "").unwrap();
        let error = prepare(&no_kubeconfig).unwrap_err();
        assert!(error.contains(".kube/config exists"), "{error}");
    }

    #[test]
    fn the_account_home_comes_from_the_user_database() {
        assert!(account_home().is_some_and(|h| h.is_absolute()));
    }

    #[test]
    fn refuses_a_directory_others_can_write() {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let error = prepare(&env(scratch.path(), home.path())).unwrap_err();
        assert!(error.contains("writable by other users"), "{error}");
    }

    #[test]
    fn refuses_a_socket_path_macos_cannot_bind() {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let deep = scratch.path().join("x".repeat(110));
        let error = prepare(&env(&deep, home.path())).unwrap_err();
        assert!(error.contains("macOS allows 103"), "{error}");
    }

    #[test]
    fn screenshots_go_where_the_harness_says() {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let shots = tempfile::tempdir().unwrap();
        let mut with_dir = env(scratch.path(), home.path());
        with_dir.screenshots = Some(shots.path().as_os_str().to_owned());
        assert_eq!(prepare(&with_dir).unwrap().screenshots, shots.path());
        with_dir.screenshots = Some("relative/dir".into());
        assert!(prepare(&with_dir).unwrap_err().contains(SCREENSHOTS_ENV));
    }

    #[test]
    fn screenshot_names_are_plain_file_names() {
        assert_eq!(screenshot_file("pods-dark").unwrap(), "pods-dark.png");
        assert_eq!(screenshot_file("a.png").unwrap(), "a.png");
        for bad in ["", "../x", "/tmp/x", "a/b", ".hidden", "a..b", "a b"] {
            assert!(screenshot_file(bad).is_err(), "{bad}");
        }
    }
}
