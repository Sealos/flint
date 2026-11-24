//! Loading and merging kubeconfig files with kubectl semantics.
//!
//! Sources: the `KUBECONFIG` list if set, else `~/.kube/config`, then extra paths from settings.
//! Files are merged in order and the first file to define a name wins (`Kubeconfig::merge`).
//! Every context becomes a Flint cluster, and Flint records which file each one came from.

pub(crate) mod definition;

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use kube::config::{Kubeconfig, KubeconfigError, NamedContext};
use tokio::sync::{Mutex, OwnedMutexGuard, oneshot};
use tokio::time::Instant;

pub use crate::recheck::{ConfirmedDefinitions, JudgedRead, ReadOutcome};
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
    /// The proxy requests go through, from the cluster entry's `proxy-url` or the environment,
    /// so the server's address alone does not say where they lead. Scheme, host and port, plus
    /// after a `#` an opaque digest of its account if it has one, so pins and grants tell
    /// accounts apart: never its credentials. Show it through [`crate::redact::server_url`].
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

    /// What a grant on this context is pinned to: its server and the proxy it is reached through.
    pub fn grant_server(&self) -> Option<String> {
        let server = self.server.as_deref()?;
        Some(
            crate::connection::proxy::grant_endpoint(server, self.proxy_url.as_deref())
                .into_owned(),
        )
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
/// exist, and whether it can be read now. When the stamps of two loads are equal, so are the
/// results, which lets callers that load often (the MCP server) skip re-parsing. Losing read
/// access changes neither the time nor the size, so it is checked on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamps(Vec<(PathBuf, Option<ModifiedAndSize>, SourceAccess)>);

/// A file's modification time and size.
type ModifiedAndSize = (std::time::SystemTime, u64);

impl FileStamps {
    pub fn of(paths: &[PathBuf]) -> Self {
        FileStamps(
            paths
                .iter()
                .map(|p| (p.clone(), file_stamp(p), SourceAccess::of(p)))
                .collect(),
        )
    }
}

/// Whether a kubeconfig path can be read at this moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceAccess {
    Absent,
    /// A symbolic link on the way to it points at nothing.
    Dangling,
    /// Not a regular file, so it is not opened: opening one can block or have effects.
    Special,
    Readable,
    Failed(std::io::ErrorKind),
}

impl SourceAccess {
    fn of(path: &Path) -> Self {
        use std::io::{ErrorKind, Read};
        if std::fs::metadata(path).is_ok_and(|meta| !meta.is_file()) {
            return SourceAccess::Special;
        }
        match open_nonblocking(path) {
            Ok(mut file) => match file.read(&mut [0; 1]) {
                Ok(_) => SourceAccess::Readable,
                Err(e) => SourceAccess::Failed(e.kind()),
            },
            // Asked after the open failed, so a link that appeared before that failure is seen.
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                if dangling_link(path) {
                    SourceAccess::Dangling
                } else {
                    SourceAccess::Absent
                }
            }
            Err(e) => SourceAccess::Failed(e.kind()),
        }
    }
}

/// Whether `path`, or a directory on the way to it, is a symbolic link whose target can't be
/// reached. Its target may only be unmounted for now, so the file is unknown rather than absent.
fn dangling_link(path: &Path) -> bool {
    path.ancestors().any(|part| {
        std::fs::symlink_metadata(part).is_ok_and(|meta| meta.file_type().is_symlink())
            && std::fs::metadata(part).is_err()
    })
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
        let mut config = match read_config(path) {
            Ok(config) => config,
            Err(e) => {
                errors.push((path.clone(), e));
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
    let environment = crate::connection::proxy::ProxyEnvironment::current();
    // Once per distinct proxy URL in a load, since each one with an account reads the route key.
    let mut identities: HashMap<String, String> = HashMap::new();
    for info in &mut contexts {
        let entry = info.cluster.as_ref().and_then(|cluster| {
            merged
                .clusters
                .iter()
                .find(|c| &c.name == cluster)
                .and_then(|c| c.cluster.as_ref())
        });
        info.server = entry.and_then(|c| c.server.clone());
        info.proxy_url = crate::connection::proxy::chosen(
            entry.and_then(|c| c.proxy_url.as_deref()),
            info.server.as_deref(),
            &environment,
        )
        .map(|proxy| {
            identities
                .entry(proxy.to_owned())
                .or_insert_with(|| crate::connection::proxy::route_identity(proxy))
                .clone()
        });
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

/// [`load`] for a process that judges definitions across reads: a path behind a symbolic link
/// with no target counts as failed rather than absent, since the target may only be unmounted.
/// A link that breaks after this check changes the input files, so the read that saw it is not
/// consistent ([`FileRead::consistent`]).
fn load_counting_dangling_links(paths: &[PathBuf]) -> LoadedKubeconfig {
    let (dangling, present): (Vec<PathBuf>, Vec<PathBuf>) =
        paths.iter().cloned().partition(|p| dangling_link(p));
    let mut loaded = load(&present);
    loaded.paths = paths.to_vec();
    loaded.errors.extend(
        dangling
            .into_iter()
            .map(|path| (path, "its symbolic link has no target".to_owned())),
    );
    loaded
}

/// Parses the file at `path`, as [`Kubeconfig::read_from`] does, from one handle. A path that is
/// already a FIFO or a device is refused without opening it, since opening one can wake a writer
/// or set off the device. Its kind is checked again on the handle the bytes come from, as the
/// path can become one between the two steps. Regular files and the null device are read
/// (`KUBECONFIG=/dev/null` is the usual way to say "no kubeconfig").
fn read_config(path: &Path) -> Result<Kubeconfig, String> {
    let failed = |e: std::io::Error| KubeconfigError::ReadConfig(e, path.into()).to_string();
    if std::fs::metadata(path).is_ok_and(|meta| !readable_kind(&meta)) {
        return Err("not a regular file".to_owned());
    }
    let file = open_nonblocking(path).map_err(failed)?;
    read_opened(path, file)
}

/// [`read_config`] from `file`, just opened at `path`.
fn read_opened(path: &Path, mut file: std::fs::File) -> Result<Kubeconfig, String> {
    use std::io::Read;
    let failed = |e: std::io::Error| KubeconfigError::ReadConfig(e, path.into()).to_string();
    let meta = file.metadata().map_err(failed)?;
    if !readable_kind(&meta) {
        return Err("not a regular file".to_owned());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(failed)?;
    let text = decode_config(bytes).map_err(failed)?;
    let mut config = Kubeconfig::from_yaml(&text).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        resolve_file_references(&mut config, dir);
    }
    Ok(config)
}

/// The text of a kubeconfig file, in the encodings its byte order mark names.
fn decode_config(bytes: Vec<u8>) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind};
    let utf16 = |body: &[u8], unit: fn([u8; 2]) -> u16, name: &str| {
        let invalid = || Error::new(ErrorKind::InvalidData, format!("Invalid {name}"));
        let (pairs, rest) = body.as_chunks::<2>();
        if !rest.is_empty() {
            return Err(invalid());
        }
        let units: Vec<u16> = pairs.iter().map(|pair| unit(*pair)).collect();
        String::from_utf16(&units).map_err(|_| invalid())
    };
    match bytes.as_slice() {
        [0xFF, 0xFE, body @ ..] => utf16(body, u16::from_le_bytes, "UTF-16 LE"),
        [0xFE, 0xFF, body @ ..] => utf16(body, u16::from_be_bytes, "UTF-16 BE"),
        [0xEF, 0xBB, 0xBF, body @ ..] => String::from_utf8(body.to_vec())
            .map_err(|_| Error::new(ErrorKind::InvalidData, "Invalid UTF-8 BOM")),
        _ => String::from_utf8(bytes)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "Invalid UTF-8")),
    }
}

/// Makes the files `config` refers to absolute, relative to `dir`, the kubeconfig's directory. An
/// exec command counts as a file only if it has a path separator: a bare name is a `PATH` lookup.
fn resolve_file_references(config: &mut Kubeconfig, dir: &Path) {
    let resolve = |file: &mut Option<String>| {
        if let Some(name) = file.as_deref()
            && Path::new(name).is_relative()
            && let Some(absolute) = dir.join(name).to_str()
        {
            *file = Some(absolute.to_owned());
        }
    };
    for cluster in config
        .clusters
        .iter_mut()
        .filter_map(|c| c.cluster.as_mut())
    {
        resolve(&mut cluster.certificate_authority);
    }
    for user in config
        .auth_infos
        .iter_mut()
        .filter_map(|u| u.auth_info.as_mut())
    {
        resolve(&mut user.client_certificate);
        resolve(&mut user.client_key);
        resolve(&mut user.token_file);
        if let Some(exec) = &mut user.exec
            && exec
                .command
                .as_ref()
                .is_some_and(|command| command.contains(std::path::MAIN_SEPARATOR))
        {
            resolve(&mut exec.command);
        }
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

/// Whether a file of this kind may be opened and read as a kubeconfig.
fn readable_kind(meta: &std::fs::Metadata) -> bool {
    meta.is_file() || is_null_device(meta)
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

/// The last number given to a read ([`FileRead::order`]).
static READ_ORDER: AtomicU64 = AtomicU64::new(0);

/// Opens `path` for reading without waiting: opening a FIFO would otherwise wait for a writer.
fn open_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::File::options();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    options.open(path)
}

/// One input file as it stood at a moment, to tell whether it changed during a read.
///
/// A regular file is compared by identity, times, size and every byte. What this cannot see is a
/// file changed and put back byte for byte between two checks within one tick of the
/// filesystem's clock, which leaves the same content when the read ends.
#[derive(PartialEq, Eq)]
enum InputFile {
    Absent,
    /// A symbolic link on the way to it points at nothing.
    Dangling,
    /// It could not be opened or read, for this reason.
    Failed(std::io::ErrorKind),
    /// Not a regular file, so its content is not read: opening one can block.
    Special(FileIdentity),
    Regular(FileIdentity, Vec<u8>),
    /// The path no longer showed this file once its bytes were read, so they may be a version
    /// that is already gone.
    Changing,
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
        use std::io::ErrorKind;
        // Known from the path alone: opening a FIFO or a device can have effects of its own.
        if let Ok(meta) = std::fs::metadata(path)
            && !meta.is_file()
        {
            return InputFile::Special(FileIdentity::of(&meta));
        }
        match open_nonblocking(path) {
            Ok(file) => Self::from_open(path, file),
            // Asked after the open failed, so a link that appeared before that failure is seen.
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                if dangling_link(path) {
                    InputFile::Dangling
                } else {
                    InputFile::Absent
                }
            }
            Err(e) => InputFile::Failed(e.kind()),
        }
    }

    /// What `file`, just opened at `path`, holds.
    fn from_open(path: &Path, mut file: std::fs::File) -> Self {
        use std::io::Read;
        let meta = match file.metadata() {
            Ok(meta) => meta,
            Err(e) => return InputFile::Failed(e.kind()),
        };
        let identity = FileIdentity::of(&meta);
        if !meta.is_file() {
            return InputFile::Special(identity);
        }
        let mut bytes = Vec::new();
        if let Err(e) = file.read_to_end(&mut bytes) {
            return InputFile::Failed(e.kind());
        }
        // The handle keeps a replaced file readable, so only the path can say it is gone.
        match std::fs::metadata(path) {
            Ok(now) if FileIdentity::of(&now) == identity => InputFile::Regular(identity, bytes),
            _ => InputFile::Changing,
        }
    }

    fn all(paths: &[PathBuf]) -> Vec<Self> {
        paths.iter().map(|path| Self::of(path)).collect()
    }

    /// Whether the path still showed this file when its reading ended.
    fn held_still(&self) -> bool {
        !matches!(self, InputFile::Changing)
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
    /// How many files, from the first, held still through the read. The first file to define an
    /// entry wins, so what these give is exact even if a file after them changed.
    pub held_still: usize,
    /// Where this read started among all reads: a higher number started later. The reader's lock
    /// makes reads of one list of files run one after another, but their results can still
    /// arrive in another order.
    pub order: u64,
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
            held_still: 0,
            order: 0,
        }
    }

    /// Counts `previous` files that are not in this read as failed too. Until a finished read of
    /// the new list shows otherwise, what they defined is not known to be gone.
    pub fn keep_undecided<'a>(&mut self, previous: impl Iterator<Item = &'a PathBuf>) {
        for path in previous {
            let listed = self.loaded.paths.contains(path);
            if !listed && !self.loaded.errors.iter().any(|(p, _)| p == path) {
                let reason = "waiting for a finished read of the new file list".to_owned();
                self.loaded.errors.push((path.clone(), reason));
            }
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
        let order = READ_ORDER.fetch_add(1, Ordering::SeqCst) + 1;
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
        let after = InputFile::all(paths);
        let held_still = before
            .iter()
            .zip(&after)
            .take_while(|(before, after)| before == after && after.held_still())
            .count();
        FileRead {
            loaded,
            exist,
            blank,
            consistent: held_still == paths.len(),
            held_still,
            order,
        }
    }
}

/// A read of the kubeconfig files for [`ConfirmedDefinitions::judge`], by a process that reads
/// them itself.
#[derive(Debug)]
pub struct KubeconfigRead(pub(crate) FileRead);

impl KubeconfigRead {
    /// Reads `paths`, again while they change under the read, for a few seconds at most. This
    /// blocks, so hold no lock across it.
    pub fn settled(paths: &[PathBuf]) -> Self {
        let deadline = std::time::Instant::now() + READ_TIMEOUT;
        KubeconfigRead(FileRead::settled(
            paths,
            load_counting_dangling_links,
            deadline,
        ))
    }

    /// What was read, to adjust before it is judged.
    pub fn loaded_mut(&mut self) -> &mut LoadedKubeconfig {
        &mut self.0.loaded
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

    /// When a read starting now has to be done by.
    pub(crate) fn deadline(&self) -> Instant {
        Instant::now() + self.timeout
    }

    /// [`Self::read_once`], with a read already running counted as not finishing.
    #[cfg(test)]
    pub(crate) async fn read(
        &self,
        paths: &[PathBuf],
        on_late: impl FnOnce(FileRead) + Clone + Send + 'static,
    ) -> Option<FileRead> {
        self.read_once(paths, on_late).await.ok().flatten()
    }

    /// The files as they are now, from one pass, or `None` if the read did not finish in time. An
    /// earlier read of the same files still running is [`ReadBusy`]. A read that finishes after
    /// its caller gave up hands its result to `on_late`. The result may be
    /// [inconsistent](FileRead::consistent).
    pub(crate) async fn read_once(
        &self,
        paths: &[PathBuf],
        on_late: impl FnOnce(FileRead) + Clone + Send + 'static,
    ) -> Result<Option<FileRead>, ReadBusy> {
        let turn = self.turn(paths).try_lock_owned().map_err(|_| ReadBusy)?;
        let deadline = self.deadline();
        let Some(answer) = self.start(paths, turn, None, on_late.clone()) else {
            return Ok(None);
        };
        Ok(answer_by(answer, deadline, on_late).await)
    }

    /// [`Self::read_once`], reading again until `by` (at most the timeout) while the files change
    /// under it. An earlier read of the same files still running is [`ReadBusy`], told apart from
    /// a timeout in the same step that tries to start this one.
    pub(crate) async fn read_settled(
        &self,
        paths: &[PathBuf],
        by: Option<Instant>,
        on_late: impl FnOnce(FileRead) + Clone + Send + 'static,
    ) -> Result<Option<FileRead>, ReadBusy> {
        let turn = self.turn(paths).try_lock_owned().map_err(|_| ReadBusy)?;
        let limit = self.deadline();
        let deadline = by.map_or(limit, |by| by.min(limit));
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
                let mut read = match settle_by {
                    Some(deadline) => FileRead::settled(&owned, load, deadline.into_std()),
                    None => FileRead::of(&owned, load),
                };
                // Nobody is waiting on this thread any more, so one more pass costs the caller
                // nothing and can turn a late result into one that shows the files as they are.
                if settle_by.is_some() && !read.consistent && reply.is_closed() {
                    read = FileRead::of(&owned, load);
                }
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

    // covers UP-54: a file replaced or removed after it was opened and before its bytes were read
    // is not taken as what the path holds.
    #[cfg(unix)]
    #[test]
    fn a_file_changed_while_it_is_read_is_not_a_settled_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a", &dev_at("https://old"));
        let quiet = open_nonblocking(&path).unwrap();
        assert!(InputFile::from_open(&path, quiet).held_still());

        let replaced = open_nonblocking(&path).unwrap();
        let next = write(dir.path(), "next", &dev_at("https://new"));
        std::fs::rename(&next, &path).unwrap();
        assert!(!InputFile::from_open(&path, replaced).held_still());

        let removed = open_nonblocking(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!InputFile::from_open(&path, removed).held_still());
    }

    // covers UP-06: the step that opens and parses a file refuses a FIFO itself, so a path that
    // became one after any earlier check cannot block the read.
    #[cfg(unix)]
    #[test]
    fn parsing_a_path_that_is_now_a_fifo_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = make_fifo(dir.path(), "fifo");
        let (done, finished) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = done.send(read_config(&path).map(drop));
        });
        let answer = finished.recv_timeout(Duration::from_secs(2));
        if answer.is_err() {
            // Lets a blocked thread end. Not waiting: with no reader left this fails at once.
            use std::os::unix::fs::OpenOptionsExt;
            drop(
                std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo),
            );
        }
        assert_eq!(
            answer.expect("the parse did not block"),
            Err("not a regular file".to_owned())
        );
    }

    // covers UP-06: a FIFO already at the path is refused without being opened, so a writer
    // waiting on it keeps waiting.
    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_path_is_not_opened() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = make_fifo(dir.path(), "fifo");
        let (opened, woke) = std::sync::mpsc::channel();
        let path = fifo.clone();
        let writer = std::thread::spawn(move || {
            let file = std::fs::OpenOptions::new().write(true).open(&path);
            let _ = opened.send(());
            drop(file);
        });
        std::thread::sleep(Duration::from_millis(50));

        let parsed = read_config(&fifo).map(drop);
        let input = InputFile::of(&fifo);
        let loaded = load(std::slice::from_ref(&fifo));
        let stayed = woke.recv_timeout(Duration::from_millis(100));

        let release = open_nonblocking(&fifo).unwrap();
        writer.join().unwrap();
        drop(release);
        assert!(stayed.is_err(), "the waiting writer was woken");
        assert_eq!(parsed, Err("not a regular file".to_owned()));
        assert!(matches!(input, InputFile::Special(_)));
        assert_eq!(loaded.errors.len(), 1);
    }

    // covers UP-06: a path replaced by a FIFO after the check before opening is still refused,
    // from the handle the bytes would come from.
    #[cfg(unix)]
    #[test]
    fn a_handle_that_is_a_fifo_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = make_fifo(dir.path(), "fifo");
        let handle = open_nonblocking(&fifo).unwrap();
        assert_eq!(
            read_opened(&fifo, handle).map(drop),
            Err("not a regular file".to_owned())
        );
    }

    // covers UP-06: the null device stays an empty kubeconfig.
    #[cfg(unix)]
    #[test]
    fn the_null_device_still_reads_as_empty() {
        let null = Path::new("/dev/null");
        assert!(read_config(null).is_ok());
        assert!(load(&[null.to_path_buf()]).errors.is_empty());
    }

    // covers UP-06: parsing from an open file gives what kube's own reader gives: every encoding,
    // several documents, and file references resolved against the kubeconfig's directory.
    #[test]
    fn parsing_from_the_handle_matches_the_library_reader() {
        let dir = tempfile::tempdir().unwrap();
        let body = "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    \
                    server: https://one\n    certificate-authority: certs/ca.pem\nusers:\n\
                    - name: u\n  user:\n    client-certificate: certs/c.pem\n    \
                    client-key: /abs/k.pem\n    tokenFile: token\n    exec:\n      \
                    apiVersion: client.authentication.k8s.io/v1\n      command: bin/login\n\
                    - name: bare\n  user:\n    exec:\n      \
                    apiVersion: client.authentication.k8s.io/v1\n      command: aws\n\
                    contexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n---\n\
                    apiVersion: v1\nkind: Config\nclusters:\n- name: second\n  cluster:\n    \
                    server: https://two\n    certificate-authority: other.pem\n";
        let utf16 = |bom: [u8; 2], unit: fn(u16) -> [u8; 2]| {
            let units = body.encode_utf16().flat_map(unit);
            bom.into_iter().chain(units).collect::<Vec<u8>>()
        };
        let encodings: [(&str, Vec<u8>); 5] = [
            ("plain", body.as_bytes().to_vec()),
            ("bom", [&[0xEF, 0xBB, 0xBF], body.as_bytes()].concat()),
            ("le", utf16([0xFF, 0xFE], u16::to_le_bytes)),
            ("be", utf16([0xFE, 0xFF], u16::to_be_bytes)),
            ("empty", Vec::new()),
        ];
        for (name, bytes) in encodings {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let ours = read_config(&path).unwrap();
            let theirs = Kubeconfig::read_from(&path).unwrap();
            assert_eq!(format!("{ours:?}"), format!("{theirs:?}"), "{name}");
        }
    }

    // covers UP-06: a UTF-16 file cut off halfway through a character is a failed file.
    #[test]
    fn a_truncated_utf16_file_is_a_failed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut");
        std::fs::write(&path, [0xFF, 0xFE, b'a']).unwrap();
        let loaded = load(std::slice::from_ref(&path));
        assert_eq!(loaded.errors.len(), 1);
    }

    // covers UP-06: a read that hangs gives up in time, no second read of the same files starts
    // while it is stuck, and its late end is reported so the caller can read again.
    #[tokio::test]
    async fn a_hung_read_times_out_and_runs_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let held = write(dir.path(), "held", &config(&[], &[]));
        let hold = LoadHold::on(&held);
        let paths = vec![held];
        let reader = KubeconfigReader::new(held_load, Duration::from_millis(50));
        let (late, mut late_rx) = tokio::sync::mpsc::unbounded_channel();

        let first = late.clone();
        let read = reader
            .read_once(&paths, move |_| {
                let _ = first.send(());
            })
            .await;
        assert!(matches!(read, Ok(None)), "timed out");
        let second = late.clone();
        let read = reader
            .read_once(&paths, move |_| {
                let _ = second.send(());
            })
            .await;
        assert!(matches!(read, Err(ReadBusy)), "refused without a new wait");

        // Another list of files is not held up by the stuck one. On a slow machine its result
        // can come after the timeout, so it is taken from either route.
        let other = vec![write(dir.path(), "other", &config(&[], &[]))];
        let (other_late, mut other_rx) = tokio::sync::mpsc::unbounded_channel();
        let answer = reader
            .read_once(&other, move |read| {
                let _ = other_late.send(read);
            })
            .await
            .expect("not held up by the stuck list");
        let finished = match answer {
            Some(read) => Some(read),
            None => other_rx.recv().await,
        };
        assert!(finished.expect("it finished").loaded.errors.is_empty());

        hold.release();
        // Bounds a hang only: the release has already let the read finish.
        tokio::time::timeout(Duration::from_secs(5), late_rx.recv())
            .await
            .expect("the late read reports")
            .unwrap();
        // Every sender is gone once the read's thread is done, so this sees any second report.
        drop(late);
        let end = tokio::time::timeout(Duration::from_secs(5), late_rx.recv())
            .await
            .expect("the read's thread ends");
        assert_eq!(end, None, "only the one read reported");
        assert_eq!(hold.loads(), 1, "only the one read ran");
    }

    /// Keeps [`held_load`] of one path from finishing until the test lets it go, as a stalled
    /// mount would. A FIFO can't do this reliably: on macOS a writer that closes before the
    /// reader has entered `read` can leave that reader blocked with no end of file.
    #[derive(Default)]
    struct LoadHold {
        state: StdMutex<LoadHoldState>,
        changed: std::sync::Condvar,
    }

    #[derive(Default)]
    struct LoadHoldState {
        released: bool,
        loads: usize,
    }

    static LOAD_HOLDS: StdMutex<Vec<(PathBuf, Arc<LoadHold>)>> = StdMutex::new(Vec::new());

    impl LoadHold {
        fn on(path: &Path) -> Arc<Self> {
            let hold = Arc::new(Self::default());
            let entry = (path.to_path_buf(), hold.clone());
            LOAD_HOLDS.lock().unwrap().push(entry);
            hold
        }

        fn wait(&self) {
            let mut state = self.state.lock().unwrap();
            state.loads += 1;
            while !state.released {
                state = self.changed.wait(state).unwrap();
            }
        }

        fn release(&self) {
            self.state.lock().unwrap().released = true;
            self.changed.notify_all();
        }

        fn loads(&self) -> usize {
            self.state.lock().unwrap().loads
        }
    }

    /// Loads `paths`, first waiting on the [`LoadHold`] of any of them.
    fn held_load(paths: &[PathBuf]) -> LoadedKubeconfig {
        let hold = LOAD_HOLDS
            .lock()
            .unwrap()
            .iter()
            .find(|(path, _)| paths.contains(path))
            .map(|(_, hold)| hold.clone());
        if let Some(hold) = hold {
            hold.wait();
        }
        load(paths)
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

    // covers UP-54: losing read access changes the stamps, though not the time or the size.
    #[cfg(unix)]
    #[test]
    fn file_stamps_change_when_a_file_stops_being_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        std::fs::write(&path, "a").unwrap();
        let paths = vec![path.clone()];
        let readable = FileStamps::of(&paths);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            return; // Running as a user that permissions don't apply to.
        }
        assert_ne!(readable, FileStamps::of(&paths));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(readable, FileStamps::of(&paths));
    }

    // covers UP-54: a link to a target that is gone, directly or through a directory, is a file
    // that failed to load and not an absent one, and its stamps differ from an absent file's.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_counts_as_a_failed_file() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("unmounted");
        let (link, linked_dir) = (dir.path().join("link"), dir.path().join("linked"));
        let through_dir = linked_dir.join("config");
        let absent = vec![link.clone()];
        let before = FileStamps::of(&absent);
        std::os::unix::fs::symlink(&gone, &link).unwrap();
        std::os::unix::fs::symlink(&gone, &linked_dir).unwrap();
        assert_ne!(before, FileStamps::of(&absent));

        let shown = write(
            dir.path(),
            "b",
            &config(&[("dev", "c1", "u1")], &[("c1", "https://one")]),
        );
        let paths = [link.clone(), through_dir.clone(), shown];
        let loaded = load_counting_dangling_links(&paths);
        let failed: Vec<_> = loaded.errors.iter().map(|(p, _)| p).collect();
        assert_eq!(failed, [&link, &through_dir]);
        assert_eq!(loaded.paths, paths, "failed files keep their rank");
        assert!(loaded.context("dev").is_some());
    }

    // covers UP-54: a link with no target that appears while the files are read makes the read
    // inconsistent, though the path was absent when the read began.
    #[cfg(unix)]
    #[test]
    fn a_link_breaking_during_a_read_is_not_consistent() {
        thread_local! {
            static LINK: std::cell::RefCell<Option<(PathBuf, PathBuf)>> =
                const { std::cell::RefCell::new(None) };
        }
        fn load_then_link(paths: &[PathBuf]) -> LoadedKubeconfig {
            let loaded = load_counting_dangling_links(paths);
            LINK.with(|link| {
                let (target, path) = link.borrow_mut().take().unwrap();
                std::os::unix::fs::symlink(target, path).unwrap();
            });
            loaded
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        LINK.with(|link| *link.borrow_mut() = Some((dir.path().join("gone"), path.clone())));
        assert!(!FileRead::of(&[path], load_then_link).consistent);
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
