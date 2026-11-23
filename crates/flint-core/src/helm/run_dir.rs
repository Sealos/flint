//! Where a `helm` run keeps its private files, and which `helm` may be given them.
//!
//! A run's kubeconfig has to be a path: `helm` reads it more than once, and on macOS rereading
//! `/dev/fd/N` continues from the shared offset and finds nothing. So each run gets a `0700`
//! directory under an app-owned `0700` root, removed when the run ends. A run holds a lock on its
//! directory for as long as it lives, so the next start can remove what a crash left behind
//! without touching a run still going in another process.

// module-tests: ../helm_tests.rs

use std::ffi::OsString;
use std::fs::{DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Component, Path, PathBuf};

use super::HelmError;
use super::acl::{self, Reach};

/// The most symbolic links followed in one path, as the kernel limits them.
const MAX_LINKS: usize = 40;
const LOCK: &str = ".lock";
const RUN_PREFIX: &str = "run-";

/// The root of the run directories, under the app's config directory.
pub(crate) fn runs_root(config_dir: &Path) -> PathBuf {
    config_dir.join("helm-runs")
}

fn this_user() -> u32 {
    // SAFETY: takes nothing and cannot fail.
    unsafe { libc::geteuid() }
}

fn owned_by_us_or_root(meta: &Metadata) -> bool {
    meta.uid() == 0 || meta.uid() == this_user()
}

/// The group macOS gives its administrators, whose write access a default Homebrew install has.
pub(super) fn admin_group() -> Option<u32> {
    cfg!(target_os = "macos").then_some(80)
}

/// Who besides this account and root can change an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    Private,
    /// Root's sticky directory (`/tmp`): others can add entries but not replace a directory of
    /// ours, so it may only lead to a real directory.
    StickyRoot,
    AdminGroup,
    Refused,
}

/// An entry's [`Access`], for this user `me` and the administrators' group `admin`.
pub(super) fn classify(
    (uid, gid, mode): (u32, u32, u32),
    is_dir: bool,
    me: u32,
    admin: Option<u32>,
) -> Access {
    if uid != 0 && uid != me {
        Access::Refused
    } else if mode & 0o022 == 0 {
        Access::Private
    } else if is_dir && uid == 0 && mode & 0o1000 != 0 {
        Access::StickyRoot
    } else if mode & 0o002 == 0 && admin == Some(gid) {
        Access::AdminGroup
    } else {
        Access::Refused
    }
}

/// A path with every link resolved, and whether administrators could change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Trusted {
    pub(super) path: PathBuf,
    pub(super) admin_writable: bool,
}

fn parts(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

/// The real path of `path`, if nothing on the way to it, links included, can be changed by an
/// account other than this one, root and, when `admin` is given, that group.
pub(super) fn resolve_trusted(path: &Path, admin: Option<u32>) -> Option<Trusted> {
    if !path.is_absolute() {
        return None;
    }
    let access = |meta: &Metadata| {
        let mode = meta.permissions().mode();
        classify(
            (meta.uid(), meta.gid(), mode),
            meta.is_dir(),
            this_user(),
            admin,
        )
    };
    let mut at = PathBuf::from("/");
    let root = access(&std::fs::symlink_metadata(&at).ok()?);
    if root == Access::Refused || acl::lets_others(&at, Reach::Change) {
        return None;
    }
    let mut admin_writable = root == Access::AdminGroup;
    let mut in_sticky = root == Access::StickyRoot;
    let mut pending: Vec<OsString> = parts(path).into_iter().rev().collect();
    let mut links = 0;
    while let Some(part) = pending.pop() {
        if part == ".." {
            at.pop();
            // The directory reached was checked on the way in, but what it may hold was not.
            in_sticky = access(&std::fs::symlink_metadata(&at).ok()?) == Access::StickyRoot;
            continue;
        }
        let next = at.join(&part);
        let meta = std::fs::symlink_metadata(&next).ok()?;
        if (in_sticky && !meta.is_dir()) || acl::lets_others(&next, Reach::Change) {
            return None;
        }
        if meta.file_type().is_symlink() {
            links += 1;
            if links > MAX_LINKS || !owned_by_us_or_root(&meta) {
                return None;
            }
            let target = std::fs::read_link(&next).ok()?;
            if target.is_absolute() {
                at = PathBuf::from("/");
                in_sticky = root == Access::StickyRoot;
            }
            pending.extend(parts(&target).into_iter().rev());
            continue;
        }
        let entry = access(&meta);
        if entry == Access::Refused {
            return None;
        }
        admin_writable |= entry == Access::AdminGroup;
        in_sticky = entry == Access::StickyRoot;
        at = next;
    }
    Some(Trusted {
        path: at,
        admin_writable,
    })
}

fn flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: the descriptor is open for as long as `file` is borrowed.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// The app-owned root, made if needed. Refused unless it is a real directory of this account's
/// alone, under a parent no other account can change.
fn private_root(root: &Path) -> Result<PathBuf, HelmError> {
    let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
        return Err(HelmError::UnsafeRunDirectory);
    };
    std::fs::create_dir_all(parent).map_err(|_| HelmError::NotStarted)?;
    let parent = resolve_trusted(parent, None)
        .ok_or(HelmError::UnsafeRunDirectory)?
        .path;
    let root = parent.join(name);
    match DirBuilder::new().mode(0o700).create(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(HelmError::NotStarted),
    }
    let meta = std::fs::symlink_metadata(&root).map_err(|_| HelmError::NotStarted)?;
    if !meta.is_dir()
        || meta.uid() != this_user()
        || meta.permissions().mode() & 0o077 != 0
        || acl::lets_others(&root, Reach::Any)
    {
        return Err(HelmError::UnsafeRunDirectory);
    }
    Ok(root)
}

/// One run's private directory. Dropping it removes the directory, then releases its lock.
pub(super) struct RunDir {
    dir: tempfile::TempDir,
    lock: File,
}

impl RunDir {
    /// Blocks on the file system.
    pub(super) fn create(root: &Path) -> Result<Self, HelmError> {
        let root = private_root(root)?;
        // Shared while the directory is made and locked, so a reaper never sees it unlocked.
        let root_lock = open_lock(&root.join(LOCK), true).map_err(|_| HelmError::NotStarted)?;
        flock(&root_lock, libc::LOCK_SH).map_err(|_| HelmError::NotStarted)?;
        let dir = tempfile::Builder::new()
            .prefix(RUN_PREFIX)
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(&root)
            .map_err(|_| HelmError::NotStarted)?;
        if acl::lets_others(dir.path(), Reach::Any) {
            return Err(HelmError::UnsafeRunDirectory);
        }
        let lock = open_lock(&dir.path().join(LOCK), true).map_err(|_| HelmError::NotStarted)?;
        flock(&lock, libc::LOCK_EX | libc::LOCK_NB).map_err(|_| HelmError::NotStarted)?;
        Ok(Self { dir, lock })
    }

    pub(super) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Makes `command`'s process hold this run's lock too, until it and all it started exit.
    pub(super) fn hand_lock_to(&self, command: &mut tokio::process::Command) {
        let lock = self.lock.as_raw_fd();
        let inherit = move || {
            // SAFETY: async-signal-safe, on the child's own descriptor table. `lock` stays open
            // in the parent until after the spawn, since `self` is borrowed for it.
            if unsafe { libc::fcntl(lock, libc::F_SETFD, 0) } == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        // SAFETY: the closure only makes the call above, between fork and exec.
        unsafe { command.pre_exec(inherit) };
    }

    /// What a crash leaves: the directory stays, and this process's hold on the lock ends.
    #[cfg(test)]
    pub(super) fn crash(self) -> PathBuf {
        self.dir.keep()
    }
}

/// Removes the run directories under `root` that no live run holds. Blocks on the file system.
pub(crate) fn reap_stale(root: &Path) {
    if std::fs::symlink_metadata(root).is_err() {
        return;
    }
    let Ok(root) = private_root(root) else {
        return;
    };
    let Ok(root_lock) = open_lock(&root.join(LOCK), true) else {
        return;
    };
    if flock(&root_lock, libc::LOCK_EX).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let is_run = entry.file_name().to_string_lossy().starts_with(RUN_PREFIX)
            && entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_run {
            continue;
        }
        let path = entry.path();
        // Without a lock file the run died before taking it: creating one needs the root lock.
        let held = match open_lock(&path.join(LOCK), false) {
            Ok(lock) => match flock(&lock, libc::LOCK_EX | libc::LOCK_NB) {
                Ok(()) => Some(lock),
                Err(_) => continue,
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => continue,
        };
        let _ = std::fs::remove_dir_all(&path);
        drop(held);
    }
}
