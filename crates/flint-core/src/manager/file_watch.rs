//! The watches on the kubeconfig files and `settings.toml`. Paths are scanned and watches are
//! changed on one dedicated thread, since any of those calls can hang on a stuck mount and the
//! manager's loop answers the write gate. A stuck scan holds up only later scans.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};
use std::time::Duration;

use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use super::{Changed, Internal};
use crate::{activity, settings};

/// Changes to watched files often come in bursts (editor save, `kind` rewriting the file).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
/// How many symlinks in a row are followed to find what to watch, as the OS limits them too.
const MAX_LINK_HOPS: usize = 40;
/// How soon watches that could not be set up are tried again.
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// Scans in a row that may each find the paths moved again before waiting for `RETRY_AFTER`.
const MAX_SETTLE_PASSES: usize = 4;

/// The files to watch.
#[derive(Clone)]
pub(super) struct WatchScope {
    pub(super) kubeconfig: Vec<PathBuf>,
    pub(super) config_dir: PathBuf,
}

/// The newest scope asked for, so requests made during a stuck scan do not pile up.
#[derive(Default)]
struct Requests {
    next: Option<WatchScope>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    requests: Mutex<Requests>,
    wake: Condvar,
}

/// Handle to the watching thread, which stops when this is dropped (or, if stuck, when its call
/// returns).
pub(super) struct FileWatcher {
    shared: Arc<Shared>,
}

impl FileWatcher {
    /// Starts watching on its own thread. Must be called on the runtime.
    pub(super) fn start(internal: mpsc::UnboundedSender<Internal>) -> Option<Self> {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Changed>();
        tokio::spawn(debounce(raw_rx, internal.clone()));
        let classes = Arc::new(Classes::default());
        let in_open = classes.clone();
        Self::spawn(Worker {
            open: Box::new(move || open_notify(in_open.clone(), raw_tx.clone())),
            scan: Box::new(scan_scope),
            // Everything may have changed before the new watches started.
            caught_up: Box::new(move || {
                let _ = internal.send(Internal::FilesChanged(Changed {
                    kubeconfig: true,
                    settings: true,
                    activity: true,
                    rearm: false,
                }));
            }),
            classes,
            watch: None,
            registered: HashSet::new(),
            retry_after: RETRY_AFTER,
        })
    }

    fn spawn<W: DirWatch + 'static>(worker: Worker<W>) -> Option<Self> {
        let shared = Arc::new(Shared::default());
        let in_thread = shared.clone();
        match std::thread::Builder::new()
            .name("flint-file-watch".into())
            .spawn(move || worker.run(&in_thread))
        {
            Ok(_) => Some(FileWatcher { shared }),
            Err(e) => {
                tracing::warn!(error = %e, "cannot watch kubeconfig and settings files");
                None
            }
        }
    }

    /// Asks for the watches to match `scope`. Never waits on the disk.
    pub(super) fn request(&self, scope: WatchScope) {
        lock(&self.shared.requests).next = Some(scope);
        self.shared.wake.notify_one();
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        lock(&self.shared.requests).closed = true;
        self.shared.wake.notify_one();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The directory watches the worker changes; a seam for tests to refuse one.
trait DirWatch: Send {
    fn watch(&mut self, dir: &Path) -> notify::Result<()>;
    fn unwatch(&mut self, dir: &Path);
}

impl DirWatch for RecommendedWatcher {
    fn watch(&mut self, dir: &Path) -> notify::Result<()> {
        Watcher::watch(self, dir, RecursiveMode::NonRecursive)
    }

    fn unwatch(&mut self, dir: &Path) {
        // A directory that is gone has no watch left to remove.
        let _ = Watcher::unwatch(self, dir);
    }
}

/// What the event callback matches against, shared with the worker.
#[derive(Default)]
struct Classes {
    current: RwLock<Arc<Watched>>,
    /// Watched directories reported removed or renamed: their watch no longer follows the path,
    /// even if a new directory already stands there.
    lost: Mutex<HashSet<PathBuf>>,
}

impl Classes {
    fn current(&self) -> Arc<Watched> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn publish(&self, watched: Watched) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(watched);
    }

    fn on_event(&self, event: &notify::Event) -> Option<Changed> {
        if matches!(event.kind, EventKind::Access(_)) {
            return None;
        }
        let watched = self.current();
        let dir_left = matches!(
            event.kind,
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        if dir_left {
            let gone = event.paths.iter().filter(|p| watched.dirs.contains(*p));
            lock(&self.lost).extend(gone.cloned());
        }
        let changed = classify(&event.paths, dir_left, &watched);
        (changed.kubeconfig || changed.settings || changed.activity).then_some(changed)
    }
}

fn open_notify(
    classes: Arc<Classes>,
    raw: mpsc::UnboundedSender<Changed>,
) -> Option<RecommendedWatcher> {
    let opened = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if let Some(changed) = classes.on_event(&event) {
            let _ = raw.send(changed);
        }
    });
    opened
        .map_err(|e| tracing::warn!(error = %e, "cannot watch kubeconfig and settings files"))
        .ok()
}

/// What to watch for `scope`, as the disk has it now.
fn scan_scope(scope: &WatchScope) -> Watched {
    // Create the config directory, so a settings.toml written there later (by hand or by
    // another Flint) is noticed. Failing is fine: the app's own writes create it too.
    let _ = std::fs::create_dir_all(&scope.config_dir);
    Watched::for_files(&scope.kubeconfig, &scope.config_dir)
}

type Scan = Box<dyn Fn(&WatchScope) -> Watched + Send>;

struct Worker<W> {
    open: Box<dyn FnMut() -> Option<W> + Send>,
    scan: Scan,
    /// Called after watches changed, so what was written before they started is read.
    caught_up: Box<dyn Fn() + Send>,
    classes: Arc<Classes>,
    watch: Option<W>,
    /// The directories with a working watch.
    registered: HashSet<PathBuf>,
    retry_after: Duration,
}

impl<W: DirWatch> Worker<W> {
    fn run(mut self, shared: &Shared) {
        let mut scope: Option<WatchScope> = None;
        let mut retry = false;
        loop {
            let mut requests = lock(&shared.requests);
            loop {
                if requests.closed {
                    return;
                }
                if let Some(next) = requests.next.take() {
                    scope = Some(next);
                    break;
                }
                if retry {
                    let (guard, waited) = shared
                        .wake
                        .wait_timeout(requests, self.retry_after)
                        .unwrap_or_else(PoisonError::into_inner);
                    requests = guard;
                    if waited.timed_out() {
                        break;
                    }
                } else {
                    requests = shared
                        .wake
                        .wait(requests)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
            drop(requests);
            let Some(scope) = &scope else { continue };
            let (changed, settled) = self.settle(scope);
            retry = !settled;
            if changed {
                (self.caught_up)();
            }
        }
    }

    /// Scans and applies until a scan finds what is already watched, so a link moved while the
    /// watches were changing is followed too: from then on a watch sees any further move.
    /// Returns whether anything changed and whether it settled.
    fn settle(&mut self, scope: &WatchScope) -> (bool, bool) {
        let mut changed = false;
        for _ in 0..MAX_SETTLE_PASSES {
            let lost: Vec<PathBuf> = lock(&self.classes.lost).drain().collect();
            let plan = (self.scan)(scope);
            if self.watch.is_none() {
                self.watch = (self.open)();
            }
            let Some(watch) = self.watch.as_mut() else {
                return (changed, false);
            };
            for dir in &lost {
                if self.registered.remove(dir) {
                    watch.unwatch(dir);
                    changed = true;
                }
            }
            if plan.dirs == self.registered && plan == *self.classes.current() {
                return (changed, true);
            }
            if !self.apply(plan) {
                return (changed, false);
            }
            changed = true;
        }
        (changed, false)
    }

    /// Adds the new watches before removing the old, reacting to both meanwhile. Whether
    /// anything changed: with no progress, only time (a freed quota) can help.
    fn apply(&mut self, plan: Watched) -> bool {
        let Some(watch) = self.watch.as_mut() else {
            return false;
        };
        let before = self.classes.current();
        let mut progress = plan != *before;
        self.classes.publish(before.merged(&plan));
        for dir in plan.dirs.difference(&self.registered.clone()) {
            match watch.watch(dir) {
                Ok(()) => {
                    self.registered.insert(dir.clone());
                    progress = true;
                }
                Err(e) => tracing::debug!(dir = %dir.display(), error = %e, "not watching dir"),
            }
        }
        let stale: Vec<PathBuf> = self.registered.difference(&plan.dirs).cloned().collect();
        for dir in stale {
            watch.unwatch(&dir);
            self.registered.remove(&dir);
            progress = true;
        }
        self.classes.publish(plan);
        progress
    }
}

/// `dir` resolved (symlinks, `..`), or as given if it cannot be.
fn resolved(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// The directories to watch so `files` are noticed, each once by its resolved name (inotify
/// merges a directory's aliases into one watch, and removing one alias's watch would remove
/// the other's), and the missing directories whose creation means the watches are redone. A
/// directory that does not exist yet cannot be watched: its nearest existing ancestor is.
pub(super) fn watch_plan<'a>(
    files: impl Iterator<Item = &'a PathBuf>,
) -> (HashSet<PathBuf>, HashSet<PathBuf>) {
    let dirs: HashSet<&Path> = files.filter_map(|f| f.parent()).collect();
    let mut watch_dirs = HashSet::new();
    let mut pending = HashSet::new();
    for dir in dirs {
        match missing_dirs(dir) {
            Some((ancestor, missing)) => {
                watch_dirs.insert(resolved(&ancestor));
                pending.extend(missing);
            }
            None => {
                watch_dirs.insert(resolved(dir));
            }
        }
    }
    (watch_dirs, pending)
}

/// The paths a file watcher reacts to.
#[derive(Clone, Default, PartialEq)]
pub(super) struct Watched {
    /// The directories watched.
    pub(super) dirs: HashSet<PathBuf>,
    /// Missing directories whose creation means the watches are redone.
    pub(super) pending: HashSet<PathBuf>,
    pub(super) kube: HashSet<PathBuf>,
    pub(super) settings: HashSet<PathBuf>,
    pub(super) activity: HashSet<PathBuf>,
}

impl Watched {
    /// What to watch for `kubeconfig` files and the settings in `config_dir`, as the disk has
    /// them now.
    pub(super) fn for_files(kubeconfig: &[PathBuf], config_dir: &Path) -> Self {
        let kube: HashSet<PathBuf> = kubeconfig.iter().flat_map(|p| watch_names(p)).collect();
        let settings_path = config_dir.join(settings::SETTINGS_FILE);
        let settings: HashSet<PathBuf> = watch_names(&settings_path).into_iter().collect();
        // Beside settings.toml, so it needs no directory of its own.
        let activity = [activity::ACTIVITY_FILE, activity::PREVIOUS_FILE]
            .iter()
            .flat_map(|f| watch_names(&config_dir.join(f)))
            .collect();
        let (dirs, pending) = watch_plan(kube.iter().chain(&settings));
        Watched {
            dirs,
            pending,
            kube,
            settings,
            activity,
        }
    }

    /// Reacts to everything either reacts to, while watches move from one to the other.
    fn merged(&self, other: &Watched) -> Watched {
        let union = |a: &HashSet<PathBuf>, b: &HashSet<PathBuf>| a.union(b).cloned().collect();
        Watched {
            dirs: union(&self.dirs, &other.dirs),
            pending: union(&self.pending, &other.pending),
            kube: union(&self.kube, &other.kube),
            settings: union(&self.settings, &other.settings),
            activity: union(&self.activity, &other.activity),
        }
    }
}

/// What an event on `paths` changed. A missing directory that appeared (`pending`) may already
/// hold kubeconfig files or `settings.toml` (a symlink's target directory restored with the file
/// in it), so both are read again, besides redoing the watches. So does a watched directory
/// that was removed or renamed (`dir_left`), even if it is back already: its watch is not.
pub(super) fn classify(paths: &[PathBuf], dir_left: bool, watched: &Watched) -> Changed {
    let rearm = paths
        .iter()
        .any(|p| watched.pending.contains(p) || (dir_left && watched.dirs.contains(p)));
    let any = |set: &HashSet<PathBuf>| rearm || paths.iter().any(|p| set.contains(p));
    Changed {
        kubeconfig: any(&watched.kube),
        settings: any(&watched.settings),
        activity: any(&watched.activity),
        rearm,
    }
}

/// For a directory that does not exist: its nearest existing ancestor, and every directory
/// from there down to it that is missing (as given and with the ancestor resolved, since
/// events may name either), whose creation means the watches should be redone. `None` when
/// the directory exists.
pub(super) fn missing_dirs(dir: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    if dir.is_dir() {
        return None;
    }
    let ancestor = dir.ancestors().skip(1).find(|a| a.is_dir())?.to_path_buf();
    let resolved = std::fs::canonicalize(&ancestor).ok();
    let mut missing = Vec::new();
    for path in dir.ancestors().take_while(|a| *a != ancestor) {
        missing.push(path.to_path_buf());
        if let (Some(resolved), Ok(rest)) = (&resolved, path.strip_prefix(&ancestor)) {
            missing.push(resolved.join(rest));
        }
    }
    Some((ancestor, missing))
}

/// The paths a file may be reported under: as given, resolved (if it exists), and with its
/// directory resolved (if only the directory exists yet).
pub(super) fn watch_names(path: &Path) -> Vec<PathBuf> {
    let mut names = vec![path.to_path_buf()];
    if let Ok(target) = std::fs::canonicalize(path) {
        names.push(target);
    }
    // A symlink whose target is missing cannot be canonicalized: name each target along the
    // chain as its link says it (relative to the link's directory), and with that directory
    // resolved, so restoring a target or moving a later link is noticed.
    let mut at = path.to_path_buf();
    for _ in 0..MAX_LINK_HOPS {
        let Ok(link) = std::fs::read_link(&at) else {
            break;
        };
        let target = match at.parent() {
            Some(dir) if link.is_relative() => dir.join(link),
            _ => link,
        };
        if names.contains(&target) {
            break;
        }
        if let (Some(dir), Some(name)) = (target.parent(), target.file_name())
            && let Ok(dir) = std::fs::canonicalize(dir)
        {
            names.push(dir.join(name));
        }
        names.push(target.clone());
        at = target;
    }
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
        && let Ok(dir) = std::fs::canonicalize(dir)
    {
        names.push(dir.join(name));
    }
    names
}

/// Ends when the watcher is dropped (its sender closes) or the manager stops.
async fn debounce(mut raw: mpsc::UnboundedReceiver<Changed>, out: mpsc::UnboundedSender<Internal>) {
    while let Some(first) = raw.recv().await {
        tokio::time::sleep(RELOAD_DEBOUNCE).await;
        let mut changed = first;
        while let Ok(more) = raw.try_recv() {
            changed.kubeconfig |= more.kubeconfig;
            changed.settings |= more.settings;
            changed.activity |= more.activity;
            changed.rearm |= more.rearm;
        }
        if out.send(Internal::FilesChanged(changed)).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    #[derive(Clone, Default)]
    struct FakeWatch {
        registered: Arc<Mutex<HashSet<PathBuf>>>,
        refused: Arc<Mutex<HashSet<PathBuf>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl DirWatch for FakeWatch {
        fn watch(&mut self, dir: &Path) -> notify::Result<()> {
            if lock(&self.refused).contains(dir) {
                return Err(notify::Error::generic("no watches left"));
            }
            lock(&self.calls).push(format!("watch {}", dir.display()));
            lock(&self.registered).insert(dir.to_path_buf());
            Ok(())
        }

        fn unwatch(&mut self, dir: &Path) {
            lock(&self.calls).push(format!("unwatch {}", dir.display()));
            lock(&self.registered).remove(dir);
        }
    }

    fn plan(dirs: &[&str]) -> Watched {
        Watched {
            dirs: dirs.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    fn scope() -> WatchScope {
        WatchScope {
            kubeconfig: Vec::new(),
            config_dir: PathBuf::from("/cfg"),
        }
    }

    struct Harness {
        watcher: FileWatcher,
        fake: FakeWatch,
        classes: Arc<Classes>,
        caught_up: std_mpsc::Receiver<HashSet<PathBuf>>,
    }

    /// A worker whose scans come from `scan`; each catch-up reports what was registered by then.
    fn harness(scan: Scan) -> Harness {
        let fake = FakeWatch::default();
        let classes = Arc::new(Classes::default());
        let (tx, caught_up) = std_mpsc::channel();
        let (opened, seen) = (fake.clone(), fake.registered.clone());
        let watcher = FileWatcher::spawn(Worker {
            open: Box::new(move || Some(opened.clone())),
            scan,
            caught_up: Box::new(move || {
                let _ = tx.send(lock(&seen).clone());
            }),
            classes: classes.clone(),
            watch: None,
            registered: HashSet::new(),
            retry_after: Duration::from_millis(20),
        })
        .unwrap();
        Harness {
            watcher,
            fake,
            classes,
            caught_up,
        }
    }

    fn fixed(current: &Arc<Mutex<Watched>>) -> Scan {
        let current = current.clone();
        Box::new(move |_| lock(&current).clone())
    }

    #[test]
    fn a_stuck_scan_does_not_hold_up_requests_and_the_newest_wins() {
        let (entered_tx, entered) = std_mpsc::channel();
        let (release, released) = std_mpsc::channel::<()>();
        let released = Mutex::new(released);
        let scanned = Arc::new(Mutex::new(Vec::new()));
        let in_scan = scanned.clone();
        let h = harness(Box::new(move |scope| {
            lock(&in_scan).push(scope.config_dir.clone());
            let _ = entered_tx.send(scope.config_dir.clone());
            let _ = lock(&released).recv();
            plan(&["/cfg"])
        }));
        h.watcher.request(scope());
        entered.recv_timeout(WAIT).unwrap();

        // The scan is stuck: asking again returns at once, however often.
        let start = std::time::Instant::now();
        for name in ["/one", "/two", "/three"] {
            h.watcher.request(WatchScope {
                kubeconfig: Vec::new(),
                config_dir: PathBuf::from(name),
            });
        }
        assert!(start.elapsed() < Duration::from_secs(1));

        // The first scope is scanned again to confirm it settled, then the newest follows.
        drop(release);
        while entered.recv_timeout(WAIT).unwrap() != Path::new("/three") {}
        let scanned = lock(&scanned).clone();
        assert!(!scanned.contains(&PathBuf::from("/one")), "{scanned:?}");
        assert!(!scanned.contains(&PathBuf::from("/two")), "{scanned:?}");
    }

    #[test]
    fn the_read_is_asked_for_only_after_the_new_watch_started() {
        let current = Arc::new(Mutex::new(plan(&["/a"])));
        let h = harness(fixed(&current));
        h.watcher.request(scope());
        let seen = h.caught_up.recv_timeout(WAIT).unwrap();
        assert!(seen.contains(Path::new("/a")), "{seen:?}");

        *lock(&current) = plan(&["/b"]);
        h.watcher.request(scope());
        let seen = h.caught_up.recv_timeout(WAIT).unwrap();
        assert_eq!(seen, HashSet::from([PathBuf::from("/b")]));
        let calls = lock(&h.fake.calls).clone();
        let (added, removed) = (
            calls.iter().position(|c| c == "watch /b").unwrap(),
            calls.iter().position(|c| c == "unwatch /a").unwrap(),
        );
        assert!(added < removed, "{calls:?}");
    }

    #[test]
    fn an_unchanged_plan_touches_no_watch_and_asks_for_no_read() {
        let current = Arc::new(Mutex::new(plan(&["/a"])));
        let h = harness(fixed(&current));
        h.watcher.request(scope());
        h.caught_up.recv_timeout(WAIT).unwrap();
        let before = lock(&h.fake.calls).clone();

        h.watcher.request(scope());
        assert!(
            h.caught_up
                .recv_timeout(Duration::from_millis(150))
                .is_err()
        );
        assert_eq!(*lock(&h.fake.calls), before);
    }

    #[test]
    fn a_watch_that_failed_is_tried_again_and_then_read() {
        let current = Arc::new(Mutex::new(plan(&["/a", "/b"])));
        let h = harness(fixed(&current));
        lock(&h.fake.refused).insert(PathBuf::from("/b"));
        h.watcher.request(scope());
        let seen = h.caught_up.recv_timeout(WAIT).unwrap();
        assert_eq!(seen, HashSet::from([PathBuf::from("/a")]));

        // The quota frees up; nothing else happens.
        lock(&h.fake.refused).clear();
        let seen = h.caught_up.recv_timeout(WAIT).unwrap();
        assert!(seen.contains(Path::new("/b")), "{seen:?}");
    }

    #[test]
    fn a_link_moved_during_the_change_is_followed() {
        let scans = Arc::new(Mutex::new(0));
        let in_scan = scans.clone();
        let h = harness(Box::new(move |_| {
            let mut scans = lock(&in_scan);
            *scans += 1;
            // The first plan is out of date by the time it is applied.
            if *scans == 1 {
                plan(&["/moved-away"])
            } else {
                plan(&["/final"])
            }
        }));
        h.watcher.request(scope());
        let seen = h.caught_up.recv_timeout(WAIT).unwrap();
        assert_eq!(seen, HashSet::from([PathBuf::from("/final")]));
    }

    #[test]
    fn a_folder_recreated_before_its_event_is_watched_again() {
        let current = Arc::new(Mutex::new(plan(&["/a"])));
        let h = harness(fixed(&current));
        h.watcher.request(scope());
        h.caught_up.recv_timeout(WAIT).unwrap();

        // Removed and already back, so the plan alone shows no difference.
        let event = notify::Event::new(EventKind::Remove(notify::event::RemoveKind::Folder))
            .add_path(PathBuf::from("/a"));
        let changed = h.classes.on_event(&event);
        assert!(changed.is_none_or(|c| c.rearm));
        assert!(lock(&h.classes.lost).contains(Path::new("/a")));
        h.watcher.request(scope());
        h.caught_up.recv_timeout(WAIT).unwrap();
        let calls = lock(&h.fake.calls).clone();
        assert_eq!(calls, ["watch /a", "unwatch /a", "watch /a"]);
    }

    #[test]
    fn a_removed_folder_rearms_even_if_it_exists_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let watched = Watched {
            dirs: HashSet::from([path.clone()]),
            kube: HashSet::from([path.join("config")]),
            ..Default::default()
        };
        assert!(classify(std::slice::from_ref(&path), true, &watched).rearm);
        assert!(!classify(std::slice::from_ref(&path), false, &watched).rearm);
    }

    #[test]
    fn every_link_of_a_longer_cycle_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&c, &b).unwrap();
        std::os::unix::fs::symlink(&a, &c).unwrap();
        let names = watch_names(&a);
        assert!(names.contains(&b) && names.contains(&c), "{names:?}");
    }
}
