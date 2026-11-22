//! Loading and merging kubeconfig files with kubectl semantics.
//!
//! Sources: the `KUBECONFIG` list if set, else `~/.kube/config`, then extra paths from settings.
//! Files are merged in order and the first file to define a name wins (`Kubeconfig::merge`).
//! Every context becomes a Flint cluster, and Flint records which file each one came from.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use kube::config::{Kubeconfig, NamedContext};
use tokio::sync::{Mutex, OwnedMutexGuard, oneshot};
use tokio::time::Instant;

use crate::settings::{Settings, expand_tilde};

/// One kubeconfig context, as Flint shows it in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextInfo {
    /// The context name. This is the cluster's identity in Flint.
    pub name: String,
    /// The kubeconfig `cluster` entry the context points at.
    pub cluster: Option<String>,
    /// The kubeconfig `user` entry the context points at.
    pub user: Option<String>,
    pub namespace: Option<String>,
    /// The API server URL, if the referenced cluster entry exists.
    pub server: Option<String>,
    /// The cluster entry's `proxy-url`, if it has one: requests go through it, so the server's
    /// address alone does not say where they lead.
    pub proxy_url: Option<String>,
    /// The file whose definition of this context won the merge.
    pub source: PathBuf,
}

impl ContextInfo {
    /// The host (and port, if any) of the API server URL, for compact display.
    pub fn server_host(&self) -> Option<&str> {
        let server = self.server.as_deref()?;
        let rest = server.split_once("://").map_or(server, |(_, r)| r);
        Some(rest.split('/').next().unwrap_or(rest))
    }
}

/// The result of loading every kubeconfig source.
#[derive(Debug, Clone, Default)]
pub struct LoadedKubeconfig {
    /// The merged config used to build clients.
    pub merged: Kubeconfig,
    /// Contexts in merge order.
    pub contexts: Vec<ContextInfo>,
    /// Every path Flint looked at, existing or not. These are the files to watch, and the list
    /// given to `helm` as `KUBECONFIG`.
    pub paths: Vec<PathBuf>,
    /// Files that exist but could not be read or parsed. They are skipped, like kubectl's
    /// behavior for a missing file, but reported so the UI can show them.
    pub errors: Vec<(PathBuf, String)>,
    /// The context names each file that loaded defines, in file order, including the ones an
    /// earlier file shadows.
    pub defines: Vec<(PathBuf, Vec<String>)>,
    /// The file whose entry won each cluster name: a context and its cluster can come from
    /// different files, and the server shown is the cluster entry's.
    pub cluster_sources: HashMap<String, PathBuf>,
    /// The same for each user entry, which can come from yet another file.
    pub user_sources: HashMap<String, PathBuf>,
    /// The files that parsed to nothing at all: no document, or only comments and whitespace.
    pub blank: Vec<PathBuf>,
}

impl LoadedKubeconfig {
    pub fn context(&self, name: &str) -> Option<&ContextInfo> {
        self.contexts.iter().find(|c| c.name == name)
    }
}

/// The ordered list of kubeconfig files, following kubectl: [`base_paths`] then `extra` paths
/// from settings. Duplicates are dropped, keeping the first occurrence.
pub fn kubeconfig_paths(
    env_kubeconfig: Option<&OsStr>,
    home: Option<&Path>,
    extra: &[PathBuf],
) -> Vec<PathBuf> {
    append_paths(&base_paths(env_kubeconfig, home), extra)
}

/// The files kubectl would read. `env_kubeconfig` is the value of `KUBECONFIG`: if set and
/// non-empty, its colon-separated entries are used and `~/.kube/config` is not. Empty entries are
/// ignored. The value is split as raw bytes, so a path that is not UTF-8 is still used rather than
/// falling back to `~/.kube/config`.
pub fn base_paths(env_kubeconfig: Option<&OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    match env_kubeconfig.filter(|s| !s.is_empty()) {
        Some(list) => std::env::split_paths(list)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| expand_tilde(&p, home))
            .collect(),
        None => home
            .map(|home| home.join(".kube").join("config"))
            .into_iter()
            .collect(),
    }
}

/// `base` followed by `extra`, without duplicates (the first occurrence is kept).
pub fn append_paths(base: &[PathBuf], extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    base.iter()
        .chain(extra)
        .filter(|p| seen.insert((*p).clone()))
        .cloned()
        .collect()
}

/// `base` plus the settings' `kubeconfig_paths`: the files to load for these settings.
pub fn paths_for_settings(
    base: &[PathBuf],
    settings: &Settings,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    append_paths(base, &settings.expanded_kubeconfig_paths(home))
}

/// [`base_paths`] for this process: `KUBECONFIG` or `~/.kube/config`.
pub fn default_base_paths() -> Vec<PathBuf> {
    base_paths(
        std::env::var_os("KUBECONFIG").as_deref(),
        std::env::home_dir().as_deref(),
    )
}

/// A one-line summary of a kubeconfig load error: its first non-empty line, at most 200
/// characters. Parse errors span several lines and can quote the file, which may hold
/// credentials, so callers show this and keep the full text out of agent output.
pub fn error_summary(error: &str) -> String {
    crate::redact::first_line(error, 200)
}

/// What [`load`] depends on: each file's modification time and size, or `None` if it does not
/// exist. When the stamps of two loads are equal, so are the
/// results, which lets callers that load often (the MCP server) skip re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamps(Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>);

impl FileStamps {
    pub fn of(paths: &[PathBuf]) -> Self {
        FileStamps(paths.iter().map(|p| (p.clone(), file_stamp(p))).collect())
    }
}

/// A file's modification time and size, or `None` if it can't be checked.
fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Reads and merges `paths` in order. Missing files are skipped silently; unreadable or invalid
/// files are skipped and listed in `errors`.
///
/// Names are resolved first-wins, across files and within one file, as kubectl documents for
/// merging. Each file's `kind` and `apiVersion` are dropped before merging: they are legacy
/// fields kubectl does not check, and a mismatch would otherwise reject a valid file.
pub fn load(paths: &[PathBuf]) -> LoadedKubeconfig {
    let mut merged = Kubeconfig::default();
    let mut contexts = Vec::new();
    let mut errors = Vec::new();
    let mut defines = Vec::new();
    let mut cluster_sources = HashMap::new();
    let mut user_sources = HashMap::new();
    let mut blank = Vec::new();
    for path in paths {
        // A path that can't be checked may hold a file: it counts as one that failed to load.
        match path.try_exists() {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => continue,
            Err(e) => {
                errors.push((path.clone(), e.to_string()));
                continue;
            }
        }
        // Opening a FIFO or a device can block for as long as nothing writes to it.
        if !readable_kind(path) {
            errors.push((path.clone(), "not a regular file".to_owned()));
            continue;
        }
        let mut config = match Kubeconfig::read_from(path) {
            Ok(config) => config,
            Err(e) => {
                errors.push((path.clone(), e.to_string()));
                continue;
            }
        };
        // From this parse, not a second read: the file may have content again by now.
        if parsed_to_nothing(&config) {
            blank.push(path.clone());
        }
        config.kind = None;
        config.api_version = None;
        dedupe_by_name(&mut config.clusters, |c| &c.name);
        dedupe_by_name(&mut config.auth_infos, |u| &u.name);
        dedupe_by_name(&mut config.contexts, |c| &c.name);
        defines.push((
            path.clone(),
            config.contexts.iter().map(|c| c.name.clone()).collect(),
        ));
        for cluster in &config.clusters {
            cluster_sources
                .entry(cluster.name.clone())
                .or_insert_with(|| path.clone());
        }
        for user in &config.auth_infos {
            user_sources
                .entry(user.name.clone())
                .or_insert_with(|| path.clone());
        }
        let known: HashSet<String> = merged.contexts.iter().map(|c| c.name.clone()).collect();
        let new_contexts: Vec<NamedContext> = config
            .contexts
            .iter()
            .filter(|c| !known.contains(&c.name))
            .cloned()
            .collect();
        merged = match merged.merge(config) {
            Ok(m) => m,
            Err(e) => {
                // Not expected now that kind and apiVersion are cleared, but `merge` may grow new
                // checks. It consumed the accumulated config, so report the file and rebuild from
                // the remaining sources.
                errors.push((path.clone(), e.to_string()));
                return load_without(paths, path, errors);
            }
        };
        for named in new_contexts {
            contexts.push(ContextInfo {
                cluster: named.context.as_ref().map(|c| c.cluster.clone()),
                user: named.context.as_ref().and_then(|c| c.user.clone()),
                namespace: named.context.as_ref().and_then(|c| c.namespace.clone()),
                server: None,
                proxy_url: None,
                name: named.name,
                source: path.clone(),
            });
        }
    }
    for info in &mut contexts {
        let entry = info.cluster.as_ref().and_then(|cluster| {
            merged
                .clusters
                .iter()
                .find(|c| &c.name == cluster)
                .and_then(|c| c.cluster.as_ref())
        });
        info.server = entry.and_then(|c| c.server.clone());
        info.proxy_url = entry.and_then(|c| c.proxy_url.clone());
    }
    LoadedKubeconfig {
        merged,
        contexts,
        paths: paths.to_vec(),
        errors,
        defines,
        cluster_sources,
        user_sources,
        blank,
    }
}

/// Whether a parse found nothing in its file, not even a `kind`.
fn parsed_to_nothing(config: &Kubeconfig) -> bool {
    config.kind.is_none()
        && config.api_version.is_none()
        && config.preferences.is_none()
        && config.clusters.is_empty()
        && config.auth_infos.is_empty()
        && config.contexts.is_empty()
        && config.current_context.is_none()
        && config.extensions.is_none()
        && config.other.is_empty()
}

/// Whether `path` is a kind of file a read returns from: a regular file, or the null device
/// (`KUBECONFIG=/dev/null` is the usual way to say "no kubeconfig"). A path that can't be
/// inspected is left for the read to report.
fn readable_kind(path: &Path) -> bool {
    std::fs::metadata(path).map_or(true, |meta| meta.is_file() || is_null_device(&meta))
}

#[cfg(unix)]
fn is_null_device(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    meta.file_type().is_char_device()
        && std::fs::metadata("/dev/null").is_ok_and(|null| null.rdev() == meta.rdev())
}

#[cfg(not(unix))]
fn is_null_device(_meta: &std::fs::Metadata) -> bool {
    false
}

/// How long a read of the kubeconfig files may hold up the cluster manager.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The pause before reading files again that changed during a read.
const SETTLE_PAUSE: Duration = Duration::from_millis(20);

/// One input file as it stood at a moment, to tell whether it changed during a read.
///
/// A regular file is compared by identity, times, size and every byte. What this cannot see is a
/// file changed and put back byte for byte between two checks within one tick of the
/// filesystem's clock, which leaves the same content when the read ends.
#[derive(PartialEq, Eq)]
enum InputFile {
    Absent,
    /// It could not be opened or read, for this reason.
    Failed(std::io::ErrorKind),
    /// Not a regular file, so its content is not read: opening one can block.
    Special(FileIdentity),
    Regular(FileIdentity, Vec<u8>),
}

/// What the file system says of an open file.
#[derive(PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    /// Device, inode and change time, which a writer can't set back.
    #[cfg(unix)]
    node: (u64, u64, i64, i64),
}

impl FileIdentity {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            node: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        }
    }
}

impl InputFile {
    fn of(path: &Path) -> Self {
        use std::io::{ErrorKind, Read};
        let mut options = std::fs::File::options();
        options.read(true);
        // Opening a FIFO for reading would otherwise wait for a writer.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                return InputFile::Absent;
            }
            Err(e) => return InputFile::Failed(e.kind()),
        };
        let meta = match file.metadata() {
            Ok(meta) => meta,
            Err(e) => return InputFile::Failed(e.kind()),
        };
        let identity = FileIdentity::of(&meta);
        if !meta.is_file() {
            return InputFile::Special(identity);
        }
        let mut bytes = Vec::new();
        match file.read_to_end(&mut bytes) {
            Ok(_) => InputFile::Regular(identity, bytes),
            Err(e) => InputFile::Failed(e.kind()),
        }
    }

    fn all(paths: &[PathBuf]) -> Vec<Self> {
        paths.iter().map(|path| Self::of(path)).collect()
    }
}

/// A file whose last parse found nothing in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlankFile {
    pub path: PathBuf,
    /// Its modification time and size from before that parse: another stamp later means it was
    /// written again.
    pub stamp: Option<(std::time::SystemTime, u64)>,
}

/// One finished read of the kubeconfig files.
#[derive(Debug)]
pub(crate) struct FileRead {
    pub loaded: LoadedKubeconfig,
    /// Each path, and whether it exists.
    pub exist: Vec<(PathBuf, bool)>,
    /// The files that parsed to nothing ([`LoadedKubeconfig::blank`]). Whether that is a failed
    /// load depends on what they defined before
    /// ([`crate::recheck::ContextHistory::fail_emptied`]).
    pub blank: Vec<BlankFile>,
    /// Whether every input file was the same when the read ended as when it began. If not, the
    /// merged result may mix versions that never existed together.
    pub consistent: bool,
}

impl FileRead {
    /// What a read that did not finish says: every file failed, so nothing it would have shown
    /// is taken as a change.
    pub fn stalled(paths: &[PathBuf]) -> Self {
        let failed = |p: &PathBuf| (p.clone(), "reading the file timed out".to_owned());
        FileRead {
            loaded: LoadedKubeconfig {
                paths: paths.to_vec(),
                errors: paths.iter().map(failed).collect(),
                ..LoadedKubeconfig::default()
            },
            exist: paths.iter().map(|p| (p.clone(), true)).collect(),
            blank: Vec::new(),
            consistent: false,
        }
    }

    /// Reads until a read is [consistent](Self::consistent) or `deadline` is near, and returns
    /// the last one.
    fn settled(
        paths: &[PathBuf],
        load: fn(&[PathBuf]) -> LoadedKubeconfig,
        deadline: std::time::Instant,
    ) -> Self {
        loop {
            let read = Self::of(paths, load);
            if read.consistent || std::time::Instant::now() + SETTLE_PAUSE >= deadline {
                return read;
            }
            std::thread::sleep(SETTLE_PAUSE);
        }
    }

    fn of(paths: &[PathBuf], load: fn(&[PathBuf]) -> LoadedKubeconfig) -> Self {
        let before = InputFile::all(paths);
        let stamps: HashMap<&PathBuf, _> = paths.iter().map(|p| (p, file_stamp(p))).collect();
        let loaded = load(paths);
        let exist = loaded
            .paths
            .iter()
            .map(|p| (p.clone(), p.exists()))
            .collect();
        let blank = loaded
            .blank
            .iter()
            .map(|path| BlankFile {
                path: path.clone(),
                stamp: stamps.get(path).copied().flatten(),
            })
            .collect();
        let consistent = before == InputFile::all(paths);
        FileRead {
            loaded,
            exist,
            blank,
            consistent,
        }
    }
}

/// Reads the kubeconfig files off the caller's task, for at most a fixed time.
///
/// A blocked read can't be cancelled, so it runs on its own thread (a runtime waits for its
/// blocking tasks when it shuts down) and at most one runs for a given list of files, tracked
/// until its thread exits. A read of another list may start beside it, so that dropping a stuck
/// path from the list recovers.
#[derive(Clone)]
pub(crate) struct KubeconfigReader {
    load: fn(&[PathBuf]) -> LoadedKubeconfig,
    timeout: Duration,
    /// One lock per list of files, held by the thread reading them.
    turns: Arc<StdMutex<ReadTurns>>,
}

/// A read of the same files is already running, so no new one started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadBusy;

/// The lock of each list of files.
type ReadTurns = HashMap<Vec<PathBuf>, Arc<Mutex<()>>>;

impl Default for KubeconfigReader {
    fn default() -> Self {
        Self::new(load, READ_TIMEOUT)
    }
}

impl KubeconfigReader {
    pub(crate) fn new(load: fn(&[PathBuf]) -> LoadedKubeconfig, timeout: Duration) -> Self {
        KubeconfigReader {
            load,
            timeout,
            turns: Arc::default(),
        }
    }

    /// The lock for `paths`. Forgets the locks nothing holds or waits for.
    fn turn(&self, paths: &[PathBuf]) -> Arc<Mutex<()>> {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        turns.retain(|_, turn| Arc::strong_count(turn) > 1);
        turns.entry(paths.to_vec()).or_default().clone()
    }

    /// Whether a read of `paths` is still running.
    pub(crate) fn busy(&self, paths: &[PathBuf]) -> bool {
        self.turn(paths).try_lock().is_err()
    }

    /// The files as they are now, from one pass, or `None` if the read did not finish in time or
    /// an earlier read of the same files still hasn't. A read that finishes after its caller gave
    /// up hands its result to `on_late`. The result may be [inconsistent](FileRead::consistent).
    pub(crate) async fn read(
        &self,
        paths: &[PathBuf],
        on_late: impl FnOnce(FileRead) + Clone + Send + 'static,
    ) -> Option<FileRead> {
        let turn = self.turn(paths).try_lock_owned().ok()?;
        let deadline = Instant::now() + self.timeout;
        let answer = self.start(paths, turn, None, on_late.clone())?;
        answer_by(answer, deadline, on_late).await
    }

    /// [`Self::read`], reading again within the timeout while the files change under it. An
    /// earlier read of the same files still running is [`ReadBusy`], told apart from a timeout
    /// in the same step that tries to start this one.
    pub(crate) async fn read_settled(
        &self,
        paths: &[PathBuf],
        on_late: impl FnOnce(FileRead) + Clone + Send + 'static,
    ) -> Result<Option<FileRead>, ReadBusy> {
        let turn = self.turn(paths).try_lock_owned().map_err(|_| ReadBusy)?;
        let deadline = Instant::now() + self.timeout;
        let Some(answer) = self.start(paths, turn, Some(deadline), on_late.clone()) else {
            return Ok(None);
        };
        Ok(answer_by(answer, deadline, on_late).await)
    }

    /// The files as they are after this call: a read already running is waited out, not used,
    /// and a new one starts after it. `None` if that does not finish within the timeout, the
    /// wait included. Files that change during the read are read again within that time; if they
    /// never hold still the result is [inconsistent](FileRead::consistent). A read that finishes
    /// after that hands its result to `on_late`.
    pub(crate) async fn read_fresh(
        &self,
        paths: &[PathBuf],
        on_late: impl FnOnce(FileRead) + Clone + Send + 'static,
    ) -> Option<FileRead> {
        let deadline = Instant::now() + self.timeout;
        let turn = tokio::time::timeout_at(deadline, self.turn(paths).lock_owned())
            .await
            .ok()?;
        let answer = self.start(paths, turn, Some(deadline), on_late.clone())?;
        answer_by(answer, deadline, on_late).await
    }

    /// Starts a read on its own thread, which holds `turn` until the files are read. With
    /// `settle_by` it reads again until they hold still or that time is near.
    fn start(
        &self,
        paths: &[PathBuf],
        turn: OwnedMutexGuard<()>,
        settle_by: Option<Instant>,
        on_late: impl FnOnce(FileRead) + Send + 'static,
    ) -> Option<oneshot::Receiver<FileRead>> {
        let (reply, answer) = oneshot::channel();
        let (load, owned) = (self.load, paths.to_vec());
        let thread = std::thread::Builder::new()
            .name("kubeconfig-read".to_owned())
            .spawn(move || {
                let read = match settle_by {
                    Some(deadline) => FileRead::settled(&owned, load, deadline.into_std()),
                    None => FileRead::of(&owned, load),
                };
                drop(turn);
                if let Err(read) = reply.send(read) {
                    on_late(read);
                }
            });
        if let Err(e) = thread {
            tracing::warn!(error = %e, "could not start a kubeconfig read");
            return None;
        }
        Some(answer)
    }
}

/// Waits for `answer` until `deadline`. A result that comes after goes to `on_late`.
async fn answer_by(
    mut answer: oneshot::Receiver<FileRead>,
    deadline: Instant,
    on_late: impl FnOnce(FileRead),
) -> Option<FileRead> {
    match tokio::time::timeout_at(deadline, &mut answer).await {
        Ok(read) => read.ok(),
        Err(_) => {
            give_up(&mut answer, on_late);
            None
        }
    }
}

/// Stops waiting on `answer`. From here the reader's send fails and it runs its own `on_late`;
/// a result it sent before that is handed to this `on_late`, so exactly one of them runs.
fn give_up(answer: &mut oneshot::Receiver<FileRead>, on_late: impl FnOnce(FileRead)) {
    answer.close();
    if let Ok(read) = answer.try_recv() {
        on_late(read);
    }
}

/// Keeps the first entry for each name.
fn dedupe_by_name<T>(items: &mut Vec<T>, name: impl Fn(&T) -> &String) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(name(item).clone()));
}

/// Reloads with one file left out, after it failed to merge.
fn load_without(
    paths: &[PathBuf],
    bad: &Path,
    mut errors: Vec<(PathBuf, String)>,
) -> LoadedKubeconfig {
    let remaining: Vec<PathBuf> = paths.iter().filter(|p| *p != bad).cloned().collect();
    let mut loaded = load(&remaining);
    errors.append(&mut loaded.errors);
    loaded.errors = errors;
    loaded.paths = paths.to_vec();
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn config(contexts: &[(&str, &str, &str)], clusters: &[(&str, &str)]) -> String {
        let mut s = String::from("apiVersion: v1\nkind: Config\nclusters:\n");
        for (name, server) in clusters {
            s += &format!("- name: {name}\n  cluster:\n    server: {server}\n");
        }
        s += "users:\n";
        for (_, _, user) in contexts {
            s += &format!("- name: {user}\n  user:\n    token: t\n");
        }
        s += "contexts:\n";
        for (name, cluster, user) in contexts {
            s += &format!("- name: {name}\n  context:\n    cluster: {cluster}\n    user: {user}\n");
        }
        s
    }

    // covers UP-06: a FIFO is reported at once instead of blocking the load, the files after it
    // still load, and the null device still counts as an empty file.
    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_regular_is_a_failed_file() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = make_fifo(dir.path(), "fifo");
        let shown = write(
            dir.path(),
            "b",
            &config(&[("dev", "c1", "u1")], &[("c1", "https://one")]),
        );
        let loaded = load(&[fifo.clone(), PathBuf::from("/dev/null"), shown]);
        let failed: Vec<_> = loaded.errors.iter().map(|(p, _)| p).collect();
        assert_eq!(failed, [&fifo]);
        assert!(loaded.context("dev").is_some());
    }

    #[cfg(unix)]
    fn make_fifo(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let path = dir.join(name);
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        path
    }

    /// Reads every path whatever its kind, so a FIFO blocks as a stalled mount would.
    #[cfg(unix)]
    fn blocking_load(paths: &[PathBuf]) -> LoadedKubeconfig {
        for path in paths {
            let _ = std::fs::read(path);
        }
        load(paths)
    }

    // covers UP-06: a read that hangs gives up in time, no second read of the same files starts
    // while it is stuck, and its late end is reported so the caller can read again.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hung_read_times_out_and_runs_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = make_fifo(dir.path(), "fifo");
        let paths = vec![fifo.clone()];
        let reader = KubeconfigReader::new(blocking_load, Duration::from_millis(50));
        let (late, mut late_rx) = tokio::sync::mpsc::unbounded_channel();

        let first = late.clone();
        let read = reader
            .read(&paths, move |_| {
                let _ = first.send(());
            })
            .await;
        assert!(read.is_none(), "timed out");
        let started = std::time::Instant::now();
        let second = late.clone();
        let read = reader
            .read(&paths, move |_| {
                let _ = second.send(());
            })
            .await;
        assert!(read.is_none(), "still running");
        assert!(started.elapsed() < Duration::from_millis(50), "no new wait");

        // Another list of files is not held up by the stuck one.
        let other = vec![write(dir.path(), "other", &config(&[], &[]))];
        assert!(reader.read(&other, |_| {}).await.is_some());

        // A writer that opens and closes the FIFO ends the blocked read.
        drop(std::fs::OpenOptions::new().write(true).open(&fifo).unwrap());
        tokio::time::timeout(Duration::from_secs(5), late_rx.recv())
            .await
            .expect("the late read reports")
            .unwrap();
        assert_eq!(late_rx.try_recv().ok(), None, "only the one read ran");
    }

    // covers UP-06: a result sent just as its caller timed out still reaches `on_late`.
    #[test]
    fn giving_up_keeps_a_result_already_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "config", &config(&[], &[]));
        let (reply, mut answer) = oneshot::channel();
        assert!(reply.send(FileRead::of(&[path], load)).is_ok());
        let (late, late_rx) = std::sync::mpsc::channel();
        give_up(&mut answer, move |_| late.send(()).unwrap());
        assert!(late_rx.try_recv().is_ok(), "handed to on_late");
    }

    // covers UP-06: after the caller gave up, the reader's send fails, so it runs `on_late`.
    #[test]
    fn a_read_ending_after_the_caller_gave_up_gets_its_result_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "config", &config(&[], &[]));
        let (reply, mut answer) = oneshot::channel();
        give_up(&mut answer, |_| panic!("nothing was sent"));
        assert!(reply.send(FileRead::of(&[path], load)).is_err());
    }

    // covers UP-06: a file counts as blank when its parse found nothing. One that says it has no
    // contexts, or supplies only a cluster entry, does not.
    #[test]
    fn a_blank_file_is_one_that_supplied_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let empty = write(dir.path(), "empty", "");
        let spaces = write(dir.path(), "spaces", " \n\t\n");
        let no_entries = write(dir.path(), "none", &config(&[], &[]));
        let cluster_only = write(
            dir.path(),
            "cluster",
            &config(&[], &[("c1", "https://one")]),
        );
        let read = FileRead::of(
            &[
                empty.clone(),
                spaces.clone(),
                no_entries.clone(),
                cluster_only,
            ],
            load,
        );
        let blank: Vec<_> = read.blank.iter().map(|b| b.path.clone()).collect();
        assert_eq!(blank, [empty, spaces]);
        assert!(read.loaded.errors.is_empty(), "the load itself is clean");
    }

    /// Parses the first file, then puts content back in it, as a truncating writer does.
    fn load_then_restore(paths: &[PathBuf]) -> LoadedKubeconfig {
        let loaded = load(paths);
        std::fs::write(
            &paths[0],
            config(&[("dev", "c1", "u1")], &[("c1", "https://one")]),
        )
        .unwrap();
        loaded
    }

    // covers UP-06: what the parse saw decides, though the file has content again by now.
    #[test]
    fn a_file_restored_right_after_its_parse_still_counts_as_blank() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "first", "");
        let read = FileRead::of(std::slice::from_ref(&path), load_then_restore);
        assert_eq!(read.blank.len(), 1);
        assert_eq!(read.blank[0].path, path);
    }

    /// Parses the files, then applies once what is waiting beside each: `<name>.next` becomes its
    /// content, keeping its modification time, and `<name>.gone` removes it.
    fn load_then_change(paths: &[PathBuf]) -> LoadedKubeconfig {
        let loaded = load(paths);
        for path in paths {
            let next = path.with_extension("next");
            if let Ok(body) = std::fs::read(&next) {
                std::fs::remove_file(&next).unwrap();
                let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                std::fs::write(path, body).unwrap();
                if let Some(modified) = modified {
                    let file = std::fs::File::options().write(true).open(path).unwrap();
                    file.set_modified(modified).unwrap();
                }
            }
            let gone = path.with_extension("gone");
            if gone.exists() {
                std::fs::remove_file(&gone).unwrap();
                std::fs::remove_file(path).unwrap();
            }
        }
        loaded
    }

    fn dev_at(server: &str) -> String {
        config(&[("dev", "c1", "u1")], &[("c1", server)])
    }

    async fn fresh_server(paths: &[PathBuf]) -> Option<String> {
        let reader = KubeconfigReader::new(load_then_change, Duration::from_secs(5));
        let read = reader.read_fresh(paths, |_| {}).await.expect("settled");
        read.loaded.context("dev").and_then(|c| c.server.clone())
    }

    // covers UP-54: a rewrite after the parse is seen though it keeps the size and the time.
    #[tokio::test]
    async fn a_same_size_rewrite_during_the_read_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a", &dev_at("https://old"));
        write(dir.path(), "a.next", &dev_at("https://new"));
        let server = fresh_server(std::slice::from_ref(&path)).await;
        assert_eq!(server.as_deref(), Some("https://new"));
    }

    // covers UP-54: a file that appears during the read can define the target.
    #[tokio::test]
    async fn a_file_appearing_during_the_read_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let later = write(dir.path(), "a", &dev_at("https://old"));
        write(dir.path(), "b.next", &dev_at("https://new"));
        let server = fresh_server(&[dir.path().join("b"), later]).await;
        assert_eq!(server.as_deref(), Some("https://new"));
    }

    // covers UP-54: a file that disappears during the read no longer defines the target.
    #[tokio::test]
    async fn a_file_disappearing_during_the_read_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let first = write(dir.path(), "b", &dev_at("https://old"));
        let later = write(dir.path(), "a", &dev_at("https://new"));
        write(dir.path(), "b.gone", "");
        let server = fresh_server(&[first, later]).await;
        assert_eq!(server.as_deref(), Some("https://new"));
    }

    #[cfg(unix)]
    static COUNTED_LOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[cfg(unix)]
    fn counted_blocking_load(paths: &[PathBuf]) -> LoadedKubeconfig {
        COUNTED_LOADS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        blocking_load(paths)
    }

    // covers UP-06: going back to a list of files whose read is still stuck starts no second
    // thread for it, however many other lists were read in between.
    #[cfg(unix)]
    #[tokio::test]
    async fn switching_lists_keeps_track_of_a_stuck_read() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = make_fifo(dir.path(), "fifo");
        let stuck = vec![fifo.clone()];
        let other = vec![write(dir.path(), "other", &config(&[], &[]))];
        let reader = KubeconfigReader::new(counted_blocking_load, Duration::from_millis(30));
        assert!(reader.read(&stuck, |_| {}).await.is_none());
        for _ in 0..3 {
            assert!(reader.read(&other, |_| {}).await.is_some());
            assert!(reader.read(&stuck, |_| {}).await.is_none());
        }
        let loads = COUNTED_LOADS.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(loads, 4, "one read of the stuck list, three of the other");
        drop(std::fs::OpenOptions::new().write(true).open(&fifo).unwrap());
    }

    // covers UP-06: a path that can't be checked is reported as failed, and the files after it
    // still load. A path that is really absent is skipped.
    #[cfg(unix)]
    #[test]
    fn a_path_that_cannot_be_checked_is_a_failed_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let hidden = write(locked.as_path(), "a", &config(&[], &[]));
        let shown = write(
            dir.path(),
            "b",
            &config(&[("dev", "c1", "u1")], &[("c1", "https://one")]),
        );
        let absent = dir.path().join("absent");
        let under_file = shown.join("nested");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = std::fs::metadata(&hidden).is_err();
        let loaded = load(&[hidden.clone(), absent, under_file, shown]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        if !denied {
            return; // Running as a user that permissions don't apply to.
        }
        let failed: Vec<_> = loaded.errors.iter().map(|(p, _)| p).collect();
        assert_eq!(failed, [&hidden]);
        assert!(loaded.context("dev").is_some());
    }

    // covers UP-06: what each file defines is kept, shadowed names included, so a removal
    // can be told apart from a file that failed to load.
    #[test]
    fn each_loaded_file_lists_what_it_defines() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(
            dir.path(),
            "a",
            &config(&[("main", "c", "u")], &[("c", "https://a")]),
        );
        let b = write(
            dir.path(),
            "b",
            &config(
                &[("main", "c", "u"), ("other", "c", "u")],
                &[("c", "https://b")],
            ),
        );
        let broken = write(dir.path(), "broken", "contexts: [");
        let loaded = load(&[a.clone(), broken, b.clone()]);
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(
            loaded.defines,
            vec![
                (a, vec!["main".to_owned()]),
                (b, vec!["main".to_owned(), "other".to_owned()]),
            ]
        );
    }

    // covers UP-01, UP-02
    #[test]
    fn kubeconfig_env_replaces_default() {
        let home = Path::new("/home/me");
        assert_eq!(
            kubeconfig_paths(Some(OsStr::new("/a:/b::~/c:/a")), Some(home), &[]),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/home/me/c")
            ]
        );
        assert_eq!(
            kubeconfig_paths(None, Some(home), &[PathBuf::from("/extra")]),
            vec![
                PathBuf::from("/home/me/.kube/config"),
                PathBuf::from("/extra")
            ]
        );
        assert_eq!(
            kubeconfig_paths(Some(OsStr::new("")), Some(home), &[]),
            vec![PathBuf::from("/home/me/.kube/config")]
        );
    }

    // covers UP-01
    #[test]
    fn merges_files_first_wins_and_tracks_sources() {
        let dir = tempfile::tempdir().unwrap();
        let first = write(
            dir.path(),
            "first",
            &config(
                &[("dev", "dev-cluster", "dev-user"), ("shared", "c1", "u1")],
                &[
                    ("dev-cluster", "https://dev.example:6443"),
                    ("c1", "https://first.example"),
                ],
            ),
        );
        let second = write(
            dir.path(),
            "second",
            &config(
                &[("shared", "c1", "u1"), ("prod", "c1", "u2")],
                // `c1` is redefined here; the first file's server must win.
                &[("c1", "https://second.example")],
            ),
        );
        let missing = dir.path().join("missing");
        let loaded = load(&[first.clone(), missing, second.clone()]);

        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let names: Vec<&str> = loaded.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["dev", "shared", "prod"]);

        let shared = loaded.context("shared").unwrap();
        assert_eq!(shared.source, first);
        assert_eq!(shared.server.as_deref(), Some("https://first.example"));

        let prod = loaded.context("prod").unwrap();
        assert_eq!(prod.source, second);
        assert_eq!(prod.user.as_deref(), Some("u2"));
        assert_eq!(prod.server.as_deref(), Some("https://first.example"));
        assert_eq!(loaded.cluster_sources["c1"], first, "the entry that won");

        let dev = loaded.context("dev").unwrap();
        assert_eq!(dev.server_host(), Some("dev.example:6443"));
        assert_eq!(loaded.merged.contexts.len(), 3);
        assert_eq!(loaded.merged.clusters.len(), 2);
    }

    // covers UP-02
    #[test]
    fn non_utf8_kubeconfig_env_is_used_not_ignored() {
        use std::os::unix::ffi::OsStrExt;
        let raw = OsStr::from_bytes(b"/tmp/\xffscratch:/b");
        let paths = kubeconfig_paths(Some(raw), Some(Path::new("/home/me")), &[]);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].as_os_str().as_bytes(), b"/tmp/\xffscratch");
        assert!(!paths.contains(&PathBuf::from("/home/me/.kube/config")));
    }

    // covers UP-54: a cluster entry's `proxy-url` is kept, so a loopback server behind a proxy
    // is not taken for a cluster on this machine.
    #[test]
    fn the_proxy_url_of_a_cluster_entry_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "proxied",
            "apiVersion: v1\nkind: Config\nclusters:\n- name: p\n  cluster:\n    server: https://127.0.0.1:6443\n    proxy-url: socks5://bastion:1080\n- name: d\n  cluster:\n    server: https://127.0.0.1:7443\nusers:\n- name: u\n  user:\n    token: t\ncontexts:\n- name: prod\n  context:\n    cluster: p\n    user: u\n- name: kind\n  context:\n    cluster: d\n    user: u\n",
        );
        let loaded = load(&[path]);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let prod = loaded.context("prod").unwrap();
        assert_eq!(prod.proxy_url.as_deref(), Some("socks5://bastion:1080"));
        assert_eq!(loaded.context("kind").unwrap().proxy_url, None);
    }

    // covers UP-01
    #[test]
    fn duplicate_names_within_a_file_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "dups",
            "apiVersion: v1\nkind: Config\nclusters:\n- name: a\n  cluster:\n    server: https://a\n- name: b\n  cluster:\n    server: https://b\n- name: a\n  cluster:\n    server: https://a2\nusers:\n- name: u\n  user:\n    token: t\ncontexts:\n- name: dev\n  context:\n    cluster: a\n    user: u\n- name: dev\n  context:\n    cluster: b\n    user: u\n",
        );
        let loaded = load(&[path]);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.contexts.len(), 1);
        assert_eq!(loaded.merged.contexts.len(), 1);
        assert_eq!(loaded.merged.clusters.len(), 2);
        let dev = loaded.context("dev").unwrap();
        assert_eq!(dev.server.as_deref(), Some("https://a"));
    }

    // covers UP-02
    #[test]
    fn kind_and_api_version_mismatch_does_not_drop_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let weird = write(
            dir.path(),
            "weird",
            "apiVersion: v2\nkind: NotConfig\nclusters:\n- name: w\n  cluster:\n    server: https://w\nusers:\n- name: wu\n  user:\n    token: t\ncontexts:\n- name: weird\n  context:\n    cluster: w\n    user: wu\n",
        );
        let good = write(
            dir.path(),
            "good",
            &config(&[("ok", "c", "u")], &[("c", "https://ok")]),
        );
        for order in [[weird.clone(), good.clone()], [good.clone(), weird.clone()]] {
            let loaded = load(&order);
            assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
            let mut names: Vec<&str> = loaded.contexts.iter().map(|c| c.name.as_str()).collect();
            names.sort();
            assert_eq!(names, ["ok", "weird"]);
        }
    }

    // covers UP-02
    #[test]
    fn merge_error_drops_only_the_failing_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(
            dir.path(),
            "a",
            &config(&[("a", "c", "u")], &[("c", "https://a")]),
        );
        let b = write(
            dir.path(),
            "b",
            &config(&[("b", "c", "u")], &[("c", "https://b")]),
        );
        let loaded = load_without(
            &[a.clone(), b.clone()],
            &b,
            vec![(b.clone(), "boom".into())],
        );
        assert_eq!(loaded.contexts.len(), 1);
        assert_eq!(loaded.contexts[0].name, "a");
        assert_eq!(loaded.errors, vec![(b.clone(), "boom".to_owned())]);
        assert_eq!(loaded.paths, vec![a, b]);
    }

    // covers UP-12
    #[test]
    fn file_stamps_change_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        let paths = vec![path.clone()];
        let missing = FileStamps::of(&paths);
        std::fs::write(&path, "a").unwrap();
        let one = FileStamps::of(&paths);
        assert_ne!(missing, one);
        assert_eq!(one, FileStamps::of(&paths));
        std::fs::write(&path, "abc").unwrap();
        assert_ne!(one, FileStamps::of(&paths));
    }

    // covers UP-02
    #[test]
    fn error_summary_is_one_line_without_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(
            dir.path(),
            "bad",
            "apiVersion: v1\nusers:\n- name: u\n  user:\n    token: SECRETTOKEN [oops\n  - x: [\n",
        );
        let loaded = load(&[bad]);
        let full = &loaded.errors[0].1;
        let summary = error_summary(full);
        assert!(!summary.contains('\n'));
        assert!(!summary.is_empty());
        assert!(!summary.contains("SECRETTOKEN"), "{summary}");
        assert_eq!(error_summary("\n  first\nsecond"), "first");
        assert_eq!(error_summary(&"x".repeat(300)).chars().count(), 201);
    }

    // covers UP-02
    #[test]
    fn invalid_file_is_reported_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(dir.path(), "bad", "this: [is not: valid");
        let good = write(
            dir.path(),
            "good",
            &config(&[("ok", "c", "u")], &[("c", "https://ok")]),
        );
        let loaded = load(&[bad.clone(), good]);
        assert_eq!(loaded.contexts.len(), 1);
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].0, bad);
    }

    // covers UP-01
    #[test]
    fn context_with_missing_cluster_has_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "c", &config(&[("orphan", "nowhere", "u")], &[]));
        let loaded = load(&[path]);
        let orphan = loaded.context("orphan").unwrap();
        assert_eq!(orphan.server, None);
        assert_eq!(orphan.server_host(), None);
    }
}
