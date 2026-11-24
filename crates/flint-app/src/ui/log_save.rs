//! What keeps a log save safe and bounded: the file is replaced whole or not at all
//! ([`replace_file`]), only so many saves hold a snapshot at once ([`SaveSlots`]), and what a
//! save held is freed off the UI thread ([`OffThread`]).

use std::fs::{File, OpenOptions};
use std::io;
use std::ops::Deref;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use gpui_kit::{BackgroundExecutor, Global};

/// The most saves that hold a snapshot at once, from the confirmation to the end of the write.
/// Each can keep log blocks the live buffer has already evicted.
pub(super) const MAX_SAVES: usize = 2;

/// Tells the temporary files of one process apart.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Replaces the file at `path` with what `write` puts in the file it is given, or leaves it as
/// it was. `write` fills a private file next to the destination, which is synced and moved over
/// it in one step, so a failed or overlapping save never shows a partial file. A replaced file
/// keeps its mode, a new one is private, and a link is followed. Anything but a regular file is
/// refused.
pub(super) fn replace_file<T>(
    path: &Path,
    write: impl FnOnce(&File) -> io::Result<T>,
) -> io::Result<T> {
    let target = follow_link(path)?;
    let mode = match std::fs::metadata(&target) {
        Ok(found) if found.is_file() => Some(found.permissions().mode() & 0o777),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the destination is not a regular file",
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let (file, temp) = create_temp(dir)?;
    let value = write(&file)?;
    if let Some(mode) = mode {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp.0, &target)?;
    temp.keep();
    // The data is safe and in place; a folder that cannot be synced does not undo that.
    let _ = File::open(dir).and_then(|dir| dir.sync_all());
    Ok(value)
}

/// Where a link at `path` points, which must exist; any other path as it is.
fn follow_link(path: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(found) if found.file_type().is_symlink() => std::fs::canonicalize(path),
        _ => Ok(path.to_owned()),
    }
}

/// A new file only this user can read, under a name nothing else holds.
fn create_temp(dir: &Path) -> io::Result<(File, TempPath)> {
    loop {
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".flint-save-{}-{n}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((file, TempPath(path))),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

/// A temporary file, removed when dropped unless it was moved into place.
struct TempPath(PathBuf);

impl TempPath {
    /// It is the destination now: nothing to remove.
    fn keep(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// The saves under way in this app.
#[derive(Default)]
pub(super) struct SaveSlots(Arc<AtomicUsize>);

impl Global for SaveSlots {}

impl SaveSlots {
    /// A slot, or `None` while [`MAX_SAVES`] saves are under way.
    pub(super) fn take(&self) -> Option<SaveSlot> {
        let mut running = self.0.load(Ordering::SeqCst);
        while running < MAX_SAVES {
            match self
                .0
                .compare_exchange(running, running + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(SaveSlot(self.0.clone())),
                Err(now) => running = now,
            }
        }
        None
    }
}

/// One save under way, until dropped.
pub(super) struct SaveSlot(Arc<AtomicUsize>);

impl Drop for SaveSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Holds `T`, and frees it on the background executor: a snapshot the live buffer has moved on
/// from can be large, and however a save ends, freeing it must not stall the UI.
pub(super) struct OffThread<T: Send + 'static> {
    value: Option<T>,
    executor: BackgroundExecutor,
}

impl<T: Send + 'static> Drop for OffThread<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            self.executor.spawn(async move { drop(value) }).detach();
        }
    }
}

impl<T: Send + 'static> OffThread<T> {
    pub(super) fn new(value: T, executor: BackgroundExecutor) -> Self {
        OffThread {
            value: Some(value),
            executor,
        }
    }
}

impl<T: Send + 'static> Deref for OffThread<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value.as_ref().expect("held until dropped")
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicBool;

    use gpui_kit::TestAppContext;

    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_failed_write_keeps_the_old_file_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pod.log");
        std::fs::write(&path, "the old export").unwrap();

        let result = replace_file(&path, |mut file| {
            file.write_all(b"half of")?;
            Err::<(), _>(io::Error::other("disk full"))
        });

        assert_eq!(result.unwrap_err().to_string(), "disk full");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the old export");
        assert_eq!(names(dir.path()), ["pod.log"]);
    }

    #[test]
    fn overlapping_saves_do_not_mix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pod.log");

        replace_file(&path, |mut outer| {
            outer.write_all(b"first half, ")?;
            replace_file(&path, |mut inner| inner.write_all(b"the other save"))?;
            outer.write_all(b"second half")
        })
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first half, second half"
        );
        assert_eq!(names(dir.path()), ["pod.log"]);
    }

    #[test]
    fn a_new_file_is_private_and_a_replaced_one_keeps_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("new.log");
        replace_file(&new, |mut file| file.write_all(b"new")).unwrap();
        assert_eq!(mode(&new), 0o600);

        let old = dir.path().join("old.log");
        std::fs::write(&old, "old").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o640)).unwrap();
        replace_file(&old, |file| {
            // Private while it is written, whatever the old file allowed.
            assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
            (&*file).write_all(b"new")
        })
        .unwrap();
        assert_eq!(mode(&old), 0o640);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "new");
    }

    #[test]
    fn a_link_is_followed_and_stays_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.log");
        let link = dir.path().join("link.log");
        std::fs::write(&target, "old").unwrap();
        symlink(&target, &link).unwrap();

        replace_file(&link, |mut file| file.write_all(b"new")).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn only_a_regular_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let dangling = dir.path().join("dangling.log");
        symlink(dir.path().join("missing"), &dangling).unwrap();

        assert!(replace_file(&folder, |_| Ok(())).is_err());
        assert!(replace_file(&dangling, |_| Ok(())).is_err());
        assert_eq!(names(dir.path()), ["dangling.log", "folder"]);
    }

    #[test]
    fn saves_past_the_cap_are_refused_until_one_ends() {
        let slots = SaveSlots::default();
        let mut held: Vec<SaveSlot> = (0..MAX_SAVES).map(|_| slots.take().unwrap()).collect();
        assert!(slots.take().is_none());
        held.pop();
        assert!(slots.take().is_some());
    }

    struct Freed(Arc<AtomicBool>);

    impl Drop for Freed {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[gpui_kit::test]
    async fn what_a_save_held_is_freed_off_the_ui_thread(cx: &mut TestAppContext) {
        let freed = Arc::new(AtomicBool::new(false));
        let held = OffThread::new(Freed(freed.clone()), cx.executor());

        drop(held);

        assert!(!freed.load(Ordering::SeqCst), "not on this thread");
        cx.run_until_parked();
        assert!(freed.load(Ordering::SeqCst));
    }
}
