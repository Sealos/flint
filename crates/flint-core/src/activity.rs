//! The agent activity log (ADR-0013, UP-16): one record per MCP tool call, so the user can see
//! what an agent read. `flint mcp` appends; the app watches the file and shows it per cluster.
//! A record says what was asked for and how much came back, never the returned content.
//!
//! The log is JSON lines in [`ACTIVITY_FILE`] in Flint's config directory. When an append would
//! take it past [`MAX_FILE_BYTES`], it is renamed to [`PREVIOUS_FILE`] (replacing the one before)
//! and a new file starts, so the log keeps at most two files' worth. Each record is written with
//! one append, so records from several `flint mcp` processes do not interleave. Lines that do not
//! parse (a newer format, a torn write) are skipped when reading.
//!
//! Logging is best-effort: a writer waits at most [`LOCK_WAIT`] for the lock, then skips its
//! record, so a stalled writer cannot hold up other `flint mcp` processes' replies.
//!
//! Server keys are HMAC-SHA256 digests under a random key in [`KEY_FILE`] ([`ServerKeys`]): a
//! kubeconfig URL can hold a password or token, and a copy of the log must not let anyone check
//! guesses at it.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The current log file, in the config directory.
pub const ACTIVITY_FILE: &str = "agent-activity.jsonl";
/// The file the log rotates into.
pub const PREVIOUS_FILE: &str = "agent-activity.1.jsonl";
/// The size at which the log rotates: about four thousand records.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Writers hold an exclusive lock on this file while they append (and rotate).
pub const LOCK_FILE: &str = "agent-activity.lock";
/// The longest a text field is recorded: a DNS name's length, plenty for any Kubernetes name.
pub const MAX_FIELD_BYTES: usize = 253;
/// The key of the server keys ([`ServerKeys`]): 32 random bytes, the owner's only, made on first
/// use.
pub const KEY_FILE: &str = "activity.key";
/// The most probed clusters a record lists by key; the rest are counted in
/// [`ActivityRecord::probed_more`]. At about 6 KB a record, a few hundred `list_clusters` calls fit
/// in the log before they push out earlier activity.
pub const MAX_PROBED: usize = 64;
/// The longest a writer waits for [`LOCK_FILE`] before it skips its record.
pub const LOCK_WAIT: Duration = Duration::from_secs(1);
/// The most records [`read`] returns, newest kept: more than the two files hold, so this only
/// bounds a log written with a larger cap.
pub const MAX_RECORDS: usize = 10_000;

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// It returned what was asked for (perhaps cut short).
    Ok,
    /// It named something the agent may not read: a cluster not exposed, or a namespace or kind
    /// outside the cluster's scope.
    Denied,
    /// It failed for another reason (not found, unreachable, invalid arguments).
    Failed,
}

/// One MCP tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRecord {
    /// When the call ended, in Unix milliseconds.
    pub at_ms: i64,
    /// The tool's name, as the agent called it.
    pub tool: String,
    /// The context the call named; none for a tool that names no cluster (`list_clusters`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    /// The API server the cluster was exposed with, when it is exposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// What the call named, as the agent wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// How many items came back (objects, events, log lines, kinds), when the tool counts them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<u64>,
    /// The size of the reply, in bytes.
    pub bytes: u64,
    /// The reply was cut short at a limit.
    #[serde(default)]
    pub truncated: bool,
    pub outcome: Outcome,
    /// [`cluster_key`] of the full `cluster`, set by [`append`]: what the app matches a context
    /// by, since `cluster` may be cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_key: Option<String>,
    /// [`ServerKeys::server_key`] of the server the cluster is exposed with, as the kubeconfig
    /// has it ([`append_call`]). None when the key file could not be read or made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_key: Option<String>,
    /// The clusters a call read about without naming one: those `list_clusters` probed, at most
    /// [`MAX_PROBED`]. Their activity tabs list the call too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probed: Vec<Probed>,
    /// How many more clusters it probed than `probed` lists.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub probed_more: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What a call reached, as the kubeconfig has it, for [`append_call`] to key: the server of the
/// cluster it named, and the contexts (with their servers) it probed. Never written as is: a
/// server URL can hold credentials.
#[derive(Debug, Clone, Default)]
pub struct Reached {
    /// The server of the cluster the call named, when it is exposed.
    pub server: Option<String>,
    /// The contexts the call probed, each with its server.
    pub probed: Vec<(String, Option<String>)>,
}

/// A cluster a call read about without naming it, by its keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probed {
    /// [`cluster_key`] of the context.
    pub cluster_key: String,
    /// [`ServerKeys::server_key`] of the server it is exposed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_key: Option<String>,
}

impl Probed {
    /// The keys of `context` at `server`.
    pub fn new(keys: &ServerKeys, context: &str, server: Option<&str>) -> Self {
        Probed {
            cluster_key: cluster_key(context),
            server_key: server.and_then(|s| keys.server_key(s)),
        }
    }
}

/// The server a view of the log is for, to compare with the keys in its records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedServer {
    /// The context has no server.
    NoServer,
    /// It has a server, but no key to digest it with: no record can be proven to be about it.
    Unknown,
    /// It has a server with this [`ServerKeys::server_key`].
    Key(String),
}

/// How far a record that is about a context can be tied to the server a view is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attribution {
    /// Recorded for this context, but nothing proves it reached this server: the record has no
    /// usable server key, or the view has none to compare it with.
    Unverified,
    /// The server keys are equal, or neither side has a server.
    Verified,
}

impl ActivityRecord {
    /// Whether the call is about the cluster with key `cluster` at `server`: it named that
    /// context, or probed it. See [`ActivityRecord::attribution`].
    pub fn is_about(&self, cluster: &str, server: &ExpectedServer) -> bool {
        self.attribution(cluster, server).is_some()
    }

    /// How the call is tied to the cluster with key `cluster` at `server`, or none when it is not
    /// about it. A context re-pointed at another server is another cluster, so two current keys
    /// that differ never match. A key that cannot be compared (missing, or in the format of a
    /// process started before an upgrade) leaves the call listed, as [`Attribution::Unverified`].
    pub fn attribution(&self, cluster: &str, server: &ExpectedServer) -> Option<Attribution> {
        let at = |cluster_key: Option<&str>, server_key: Option<&str>| {
            if cluster_key != Some(cluster) {
                return None;
            }
            match (server, server_key.filter(|k| is_current_server_key(k))) {
                (ExpectedServer::Key(expected), Some(key)) => {
                    (key == expected).then_some(Attribution::Verified)
                }
                (ExpectedServer::NoServer, None) if server_key.is_none() => {
                    Some(Attribution::Verified)
                }
                _ => Some(Attribution::Unverified),
            }
        };
        let named = at(self.cluster_key.as_deref(), self.server_key.as_deref());
        let probed = self
            .probed
            .iter()
            .filter_map(|p| at(Some(&p.cluster_key), p.server_key.as_deref()));
        named.into_iter().chain(probed).max()
    }
}

/// Whether `key` has the format of [`ServerKeys::server_key`]: 128 bits in lowercase hex. The
/// unkeyed digest older versions wrote is 64 bits.
fn is_current_server_key(key: &str) -> bool {
    key.len() == 32 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A stable key for a context name (FNV-1a, 64 bits, in hex): the same in every Flint version.
/// Context names are not secret (the record holds them), so this one is not keyed.
pub fn cluster_key(context: &str) -> String {
    fnv1a(context.as_bytes())
}

/// The keyed digest the log keys servers by, with the key in a config directory's [`KEY_FILE`].
/// Cheap to clone. The default has no key, so no server keys.
#[derive(Clone, Default)]
pub struct ServerKeys {
    /// None when the key file could not be read or made: then there are no server keys.
    key: Option<ring::hmac::Key>,
}

impl std::fmt::Debug for ServerKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerKeys")
            .field("key", &self.key.as_ref().map(|_| "…"))
            .finish()
    }
}

impl ServerKeys {
    /// The keys of the log in `config_dir`, making its [`KEY_FILE`] if there is none. Several
    /// processes making it at once agree on one: each writes a file of its own and links it into
    /// place, which fails if another got there first. A key file that cannot be read or made is
    /// logged, and there are no server keys.
    pub fn open(config_dir: &Path) -> Self {
        match load_or_make_key(config_dir) {
            Ok(secret) => ServerKeys::with_secret(&secret),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read or make the agent activity log's key");
                ServerKeys { key: None }
            }
        }
    }

    /// Keys under `secret` (tests, and [`ServerKeys::open`]).
    pub fn with_secret(secret: &[u8]) -> Self {
        ServerKeys {
            key: Some(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret)),
        }
    }

    /// What a view of the context at `server` compares records with.
    pub fn expected(&self, server: Option<&str>) -> ExpectedServer {
        match server {
            None => ExpectedServer::NoServer,
            Some(url) => self
                .server_key(url)
                .map_or(ExpectedServer::Unknown, ExpectedServer::Key),
        }
    }

    /// The key of an API server URL: of the endpoint the cluster pin compares
    /// ([`crate::settings::server_identity`]), so two servers have one key only when a pin would
    /// take them for the same. Pass the URL as the kubeconfig has it, not the shown one, which
    /// can hide distinct servers behind one placeholder. HMAC-SHA256, cut to 128 bits, in hex;
    /// none without a key.
    pub fn server_key(&self, url: &str) -> Option<String> {
        let key = self.key.as_ref()?;
        let tag = ring::hmac::sign(key, crate::settings::server_identity(url).as_bytes());
        Some(
            tag.as_ref()[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        )
    }
}

/// The 32 bytes in `config_dir`'s [`KEY_FILE`], made first if it is missing.
fn load_or_make_key(config_dir: &Path) -> std::io::Result<[u8; 32]> {
    use ring::rand::SecureRandom as _;
    let path = config_dir.join(KEY_FILE);
    match read_key(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        read => return read,
    }
    std::fs::create_dir_all(config_dir)?;
    let rng = ring::rand::SystemRandom::new();
    let no_random = |_| std::io::Error::other("no random bytes from the system");
    let mut secret = [0u8; 32];
    rng.fill(&mut secret).map_err(no_random)?;
    // A random name: a draft a crashed process left behind can never be taken for this one.
    let mut name = [0u8; 16];
    rng.fill(&mut name).map_err(no_random)?;
    let draft = config_dir.join(format!(".{KEY_FILE}.{:032x}", u128::from_ne_bytes(name)));
    if link_key(&draft, &path, &secret)? {
        Ok(secret)
    } else {
        // Another process made it first: use theirs.
        read_key(&path)
    }
}

/// Writes `secret` whole to a new file at `draft` and links it to `path`: a reader never sees part
/// of a key, and a key already there is never replaced (then this returns false). Removes `draft`
/// only once this call has created it, so a file already at that name stays and is an error.
fn link_key(draft: &Path, path: &Path, secret: &[u8; 32]) -> std::io::Result<bool> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(OWNER_ONLY)
        .open(draft)?;
    let linked = file
        .write_all(secret)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::hard_link(draft, path));
    let _ = std::fs::remove_file(draft);
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// A key file: exactly 32 bytes, made the owner's only if it is not.
fn read_key(path: &Path) -> std::io::Result<[u8; 32]> {
    let bytes = std::fs::read(path)?;
    let secret = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{KEY_FILE} is not 32 bytes"),
        )
    })?;
    owner_only(path)?;
    Ok(secret)
}

/// Appends the record of a call, with the keys of the servers it `reached` under the log's
/// [`ServerKeys`] (best-effort: none when the key file cannot be read or made).
pub fn append_call(
    config_dir: &Path,
    record: &ActivityRecord,
    reached: &Reached,
) -> std::io::Result<()> {
    let keys = ServerKeys::open(config_dir);
    let record = ActivityRecord {
        server_key: reached.server.as_deref().and_then(|s| keys.server_key(s)),
        probed: reached
            .probed
            .iter()
            .map(|(context, server)| Probed::new(&keys, context, server.as_deref()))
            .collect(),
        ..record.clone()
    };
    append(config_dir, &record)
}

fn fnv1a(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// Appends `record` to the log in `config_dir`, rotating it first if it would grow past
/// [`MAX_FILE_BYTES`]. Creates the directory if needed. Text fields are cut at
/// [`MAX_FIELD_BYTES`]: they hold what the agent wrote, and a few huge ones must not flush the
/// log. Writers take [`LOCK_FILE`] for the whole check, rotation and write, so two of them never
/// rotate the same file twice. A writer that cannot take it within [`LOCK_WAIT`] gives up with
/// [`std::io::ErrorKind::TimedOut`], and the record is not written.
pub fn append(config_dir: &Path, record: &ActivityRecord) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(&capped(record))?;
    line.push(b'\n');
    std::fs::create_dir_all(config_dir)?;
    let _lock = Lock::take(&config_dir.join(LOCK_FILE), LOCK_WAIT)?;
    let path = config_dir.join(ACTIVITY_FILE);
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    if size > 0 && size + line.len() as u64 > MAX_FILE_BYTES {
        std::fs::rename(&path, config_dir.join(PREVIOUS_FILE))?;
        owner_only(&config_dir.join(PREVIOUS_FILE))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(OWNER_ONLY)
        .open(&path)?;
    // A log written before (or by hand) may be readable by others: it names clusters and
    // objects. Rotation keeps the mode.
    owner_only(&path)?;
    // A write cut off before its newline (a crash, a full disk) would join this record to it,
    // losing both: end it first. Its fragment is then one line that does not parse.
    if ends_unterminated(&mut file)? {
        line.insert(0, b'\n');
    }
    file.write_all(&line)
}

/// Read and write for the owner only.
const OWNER_ONLY: u32 = 0o600;

/// Sets `path` to [`OWNER_ONLY`] unless it already is.
fn owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o777 != OWNER_ONLY {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNER_ONLY))?;
    }
    Ok(())
}

/// Whether `file` is not empty and its last byte is not a newline.
fn ends_unterminated(file: &mut std::fs::File) -> std::io::Result<bool> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// `record` with each text field cut at [`MAX_FIELD_BYTES`], saying how much was left out, and
/// at most [`MAX_PROBED`] probed clusters, counting the rest.
fn capped(record: &ActivityRecord) -> ActivityRecord {
    let cap = |text: &String| {
        if text.len() <= MAX_FIELD_BYTES {
            return text.clone();
        }
        let mut end = MAX_FIELD_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…(+{} bytes)", &text[..end], text.len() - end)
    };
    let cap_opt = |text: &Option<String>| text.as_ref().map(cap);
    ActivityRecord {
        cluster_key: record
            .cluster_key
            .clone()
            .or_else(|| record.cluster.as_deref().map(cluster_key)),
        probed: record.probed.iter().take(MAX_PROBED).cloned().collect(),
        probed_more: record.probed_more + record.probed.len().saturating_sub(MAX_PROBED) as u64,
        tool: cap(&record.tool),
        cluster: cap_opt(&record.cluster),
        server: cap_opt(&record.server),
        kind: cap_opt(&record.kind),
        namespace: cap_opt(&record.namespace),
        name: cap_opt(&record.name),
        container: cap_opt(&record.container),
        ..record.clone()
    }
}

/// An exclusive `flock` on a file, released when dropped (with the descriptor).
struct Lock {
    /// Closing it releases the lock.
    _file: std::fs::File,
}

impl Lock {
    /// Takes the lock on `path` (created owner-only), trying for up to `wait`.
    fn take(path: &Path, wait: Duration) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd as _;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(OWNER_ONLY)
            .open(path)?;
        owner_only(path)?;
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: flock on a descriptor `file` owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Lock { _file: file });
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {}
                _ => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "another writer held {LOCK_FILE} for over {} ms",
                        wait.as_millis()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// The records in `config_dir`, oldest first, at most [`MAX_RECORDS`] (the newest). A missing
/// log is empty.
///
/// Both files are read from one rotation: if a writer rotated the log between reading the
/// previous file and the current one (the previous file is then another file), both are read
/// again, a few times at most. Readers take no lock, so the app never holds up a writer.
pub fn read(config_dir: &Path) -> Vec<ActivityRecord> {
    read_with(config_dir, || {})
}

/// How many times [`read`] reads the files while writers keep rotating them under it.
const READ_TRIES: usize = 5;

/// [`read`], calling `between` after reading the previous file (for tests).
fn read_with(config_dir: &Path, mut between: impl FnMut()) -> Vec<ActivityRecord> {
    let previous = config_dir.join(PREVIOUS_FILE);
    let mut files = Vec::new();
    for _ in 0..READ_TRIES {
        let (old, id) = read_file(&previous);
        between();
        let current = std::fs::read(config_dir.join(ACTIVITY_FILE)).unwrap_or_default();
        let rotated = file_id(&previous) != id;
        files = vec![old, current];
        if !rotated {
            break;
        }
    }
    let mut records: Vec<ActivityRecord> = files
        .iter()
        .flat_map(|bytes| {
            bytes
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice::<ActivityRecord>(line).ok())
                .map(keyed)
                .collect::<Vec<_>>()
        })
        .collect();
    let extra = records.len().saturating_sub(MAX_RECORDS);
    records.drain(..extra);
    records
}

/// The file at `path` and its identity, read from one open file; empty and none when missing.
fn read_file(path: &Path) -> (Vec<u8>, Option<(u64, u64)>) {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    let Ok(mut file) = std::fs::File::open(path) else {
        return (Vec::new(), None);
    };
    let id = file.metadata().ok().map(|m| (m.dev(), m.ino()));
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        bytes.clear();
    }
    (bytes, id)
}

/// The device and inode of the file at `path`: a rotation puts another file there.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// `record` with its cluster key, computed from its context when it has none. Its server key is
/// never guessed from the shown server, which can drop the query or hide the server behind a
/// placeholder: a record written without the key file has none, and its tab lists it as
/// [`Attribution::Unverified`].
fn keyed(mut record: ActivityRecord) -> ActivityRecord {
    if record.cluster_key.is_none() {
        record.cluster_key = record.cluster.as_deref().map(cluster_key);
    }
    record
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Server keys under a fixed key, for records written by hand.
    fn server_key(url: &str) -> String {
        ServerKeys::with_secret(&[7; 32]).server_key(url).unwrap()
    }

    fn at(url: &str) -> ExpectedServer {
        ExpectedServer::Key(server_key(url))
    }

    fn record(n: u64) -> ActivityRecord {
        ActivityRecord {
            at_ms: 1_700_000_000_000 + n as i64,
            tool: "get_resource".into(),
            cluster: Some("prod".into()),
            server: Some("https://prod:6443".into()),
            kind: Some("pods".into()),
            namespace: Some("web".into()),
            name: Some(format!("web-{n}")),
            container: None,
            items: Some(1),
            bytes: 1200,
            truncated: false,
            outcome: Outcome::Ok,
            cluster_key: Some(cluster_key("prod")),
            server_key: Some(server_key("https://prod:6443")),
            probed: Vec::new(),
            probed_more: 0,
        }
    }

    // covers UP-16: the record format is one JSON object per line, with what was named and how
    // much came back; empty fields are left out.
    #[test]
    fn records_are_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &record(1)).unwrap();
        let denied = ActivityRecord {
            tool: "list_resources".into(),
            kind: Some("secrets".into()),
            namespace: None,
            name: None,
            items: None,
            bytes: 80,
            outcome: Outcome::Denied,
            ..record(2)
        };
        append(dir.path(), &denied).unwrap();
        let text = std::fs::read_to_string(dir.path().join(ACTIVITY_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let keys = format!(
            r#""cluster_key":"{}","server_key":"{}""#,
            cluster_key("prod"),
            server_key("https://prod:6443")
        );
        assert_eq!(
            lines[0],
            format!(
                r#"{{"at_ms":1700000000001,"tool":"get_resource","cluster":"prod","server":"https://prod:6443","kind":"pods","namespace":"web","name":"web-1","items":1,"bytes":1200,"truncated":false,"outcome":"ok",{keys}}}"#
            )
        );
        assert_eq!(
            lines[1],
            format!(
                r#"{{"at_ms":1700000000002,"tool":"list_resources","cluster":"prod","server":"https://prod:6443","kind":"secrets","bytes":80,"truncated":false,"outcome":"denied",{keys}}}"#
            )
        );
        assert_eq!(read(dir.path()), [record(1), denied]);
    }

    // covers UP-16
    #[test]
    fn a_missing_log_is_empty_and_bad_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_empty());
        append(&dir.path().join("new"), &record(1)).unwrap();
        let path = dir.path().join("new").join(ACTIVITY_FILE);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"at_ms\": 1, \"torn\n");
        std::fs::write(&path, text).unwrap();
        append(&dir.path().join("new"), &record(2)).unwrap();
        assert_eq!(read(&dir.path().join("new")), [record(1), record(2)]);
    }

    // covers UP-16: the log rotates at its size cap and keeps the file before; reading spans
    // both, oldest first.
    #[test]
    fn the_log_rotates_at_its_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let line = serde_json::to_vec(&record(0)).unwrap().len() as u64 + 1;
        let total = MAX_FILE_BYTES / line * 3;
        for n in 0..total {
            append(dir.path(), &record(n)).unwrap();
        }
        let current = std::fs::metadata(dir.path().join(ACTIVITY_FILE))
            .unwrap()
            .len();
        let previous = std::fs::metadata(dir.path().join(PREVIOUS_FILE))
            .unwrap()
            .len();
        assert!(current <= MAX_FILE_BYTES && previous <= MAX_FILE_BYTES);
        let records = read(dir.path());
        // The oldest file was dropped at the second rotation; what is kept is the newest, in
        // order, with nothing missing in between.
        assert_eq!(records.last(), Some(&record(total - 1)));
        assert!(records.len() as u64 > MAX_FILE_BYTES / line && (records.len() as u64) < total);
        assert!(records.windows(2).all(|w| w[0].at_ms + 1 == w[1].at_ms));
    }

    // covers UP-16: an agent cannot flush the log with huge names: each field is cut, so the
    // records before stay.
    #[test]
    fn huge_fields_are_cut_and_do_not_flush_the_log() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..100 {
            append(dir.path(), &record(n)).unwrap();
        }
        let huge = ActivityRecord {
            name: Some("x".repeat(1_100_000)),
            namespace: Some("é".repeat(300)),
            ..record(100)
        };
        append(dir.path(), &huge).unwrap();
        append(dir.path(), &huge).unwrap();
        let records = read(dir.path());
        assert_eq!(records.len(), 102);
        let name = records[100].name.as_deref().unwrap();
        assert_eq!(name, format!("{}…(+1099747 bytes)", "x".repeat(253)));
        let namespace = records[100].namespace.as_deref().unwrap();
        assert!(namespace.starts_with(&"é".repeat(126)) && namespace.ends_with("…(+348 bytes)"));
    }

    /// [`append`], tried again when it gives up on the lock, which a busy machine can make it do:
    /// nothing was written then, and skipping the record would leave the previous file short.
    /// Any other error fails the test, and so does a lock that never comes free.
    fn append_retrying(dir: &Path, record: &ActivityRecord) {
        for _ in 0..60 {
            match append(dir, record) {
                Ok(()) => return,
                // The lock's own timeout, which has no OS error: an OS timeout may come from a
                // rotation or a write that already changed the log.
                Err(e)
                    if e.kind() == std::io::ErrorKind::TimedOut && e.raw_os_error().is_none() => {}
                Err(e) => panic!("append failed: {e}"),
            }
        }
        panic!("the activity log's lock never came free");
    }

    // covers UP-16: writers in several processes (and threads) rotate in turn: no record is
    // lost but the ones the cap drops, and the kept ones are complete files' worth.
    #[test]
    fn concurrent_writers_rotate_without_losing_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let line = serde_json::to_vec(&record(0)).unwrap().len() as u64 + 1;
        let per_writer = MAX_FILE_BYTES / line;
        std::thread::scope(|s| {
            for w in 0..4u64 {
                let dir = dir.path();
                s.spawn(move || {
                    for n in 0..per_writer {
                        append_retrying(dir, &record(w * 1_000_000 + n));
                    }
                });
            }
        });
        // About four files' worth written, so two rotations at least: the previous file is
        // always a full one, never a near-empty one that replaced it.
        let previous = std::fs::metadata(dir.path().join(PREVIOUS_FILE))
            .unwrap()
            .len();
        assert!(previous > MAX_FILE_BYTES - 2 * line, "{previous}");
        let records = read(dir.path());
        let mut ids: Vec<i64> = records.iter().map(|r| r.at_ms).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), records.len(), "no record written twice");
    }

    // covers UP-16: the log is the owner's only (it names clusters and objects), including a log
    // written before with another mode, and the file it rotates into.
    #[test]
    fn the_log_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACTIVITY_FILE);
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        append(dir.path(), &record(1)).unwrap();
        assert_eq!(mode(&path), 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        append(dir.path(), &record(2)).unwrap();
        assert_eq!(mode(&path), 0o600);
        let big = "x".repeat(MAX_FILE_BYTES as usize);
        std::fs::write(&path, format!("{big}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        append(dir.path(), &record(3)).unwrap();
        assert_eq!(mode(&dir.path().join(PREVIOUS_FILE)), 0o600);
        assert_eq!(mode(&path), 0o600);
    }

    // covers UP-16: a record cut off before its newline costs only itself: the next one starts
    // on a line of its own.
    #[test]
    fn an_unterminated_line_does_not_swallow_the_next_record() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &record(1)).unwrap();
        let path = dir.path().join(ACTIVITY_FILE);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(r#"{"at_ms":17000"#);
        std::fs::write(&path, text).unwrap();
        append(dir.path(), &record(2)).unwrap();
        assert_eq!(read(dir.path()), [record(1), record(2)]);
    }

    // covers UP-16: a cut context or server still matches: the keys are of the full names.
    #[test]
    fn keys_are_of_the_full_names() {
        let dir = tempfile::tempdir().unwrap();
        let long = "c".repeat(400);
        let server = format!("https://{}:6443/", "s".repeat(300));
        let mut wide = record(1);
        wide.cluster = Some(long.clone());
        wide.server = Some(server.clone());
        wide.cluster_key = None;
        wide.server_key = None;
        let reached = Reached {
            server: Some(server.clone()),
            probed: Vec::new(),
        };
        append_call(dir.path(), &wide, &reached).unwrap();
        let read = read(dir.path());
        assert_ne!(read[0].cluster.as_deref(), Some(long.as_str()));
        assert_eq!(read[0].cluster_key, Some(cluster_key(&long)));
        let keys = ServerKeys::open(dir.path());
        assert_eq!(
            read[0].server_key,
            keys.server_key(server.trim_end_matches('/'))
        );
        assert_ne!(cluster_key("a"), cluster_key("b"));
        // FNV-1a's published values: the keys never change between Flint versions.
        assert_eq!(cluster_key(""), "cbf29ce484222325");
        assert_eq!(cluster_key("a"), "af63dc4c8601ec8c");
    }

    // covers UP-16: two servers a pin tells apart have two keys, though both show as the same
    // placeholder (an `@` in the path); a trailing slash makes no other server.
    #[test]
    fn server_keys_are_of_the_endpoint_the_pin_compares() {
        let one = "https://proxy.example.com/k8s/@one";
        let two = "https://proxy.example.com/k8s/@two";
        assert_eq!(
            crate::redact::server_url(one),
            crate::redact::server_url(two)
        );
        assert_ne!(server_key(one), server_key(two));
        assert_eq!(server_key(one), server_key(&format!("{one}/")));
        let pin = crate::settings::ClusterPin::new("prod", one);
        assert!(pin.matches("prod", &format!("{one}/")) && !pin.matches("prod", two));

        // Recorded with the shown server and the key of the real one: only that server's tab
        // finds it.
        let dir = tempfile::tempdir().unwrap();
        let at_one = ActivityRecord {
            server: Some(crate::redact::server_url(one)),
            server_key: Some(server_key(one)),
            ..record(1)
        };
        append(dir.path(), &at_one).unwrap();
        let read = read(dir.path());
        let prod = cluster_key("prod");
        assert!(read[0].is_about(&prod, &at(one)));
        assert!(!read[0].is_about(&prod, &at(two)));
        assert!(read[0].is_about(&prod, &ExpectedServer::NoServer));
        assert!(!read[0].is_about(&cluster_key("staging"), &ExpectedServer::NoServer));
    }

    // covers UP-16: a line without keys (written before them, or without a usable key file)
    // still reads, with its cluster key computed from its context. Its server key is never
    // guessed from the shown server, which can drop the query or be a placeholder: it matches
    // its tab by context alone.
    #[test]
    fn older_lines_get_their_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(ACTIVITY_FILE),
            "{\"at_ms\":1,\"tool\":\"get_resource\",\"cluster\":\"prod\",\"server\":\"https://prod:6443\",\"bytes\":9,\"outcome\":\"ok\"}\n\
             {\"at_ms\":2,\"tool\":\"get_resource\",\"cluster\":\"prod\",\"server\":\"<server URL not shown>\",\"bytes\":9,\"outcome\":\"ok\"}\n",
        )
        .unwrap();
        let read = read(dir.path());
        assert_eq!(read.len(), 2);
        for line in &read {
            assert_eq!(line.cluster_key, Some(cluster_key("prod")));
            assert_eq!(line.server_key, None);
            assert!(line.probed.is_empty() && line.probed_more == 0);
            assert_eq!(
                line.attribution(&cluster_key("prod"), &at("https://a")),
                Some(Attribution::Unverified)
            );
            assert!(!line.is_about(&cluster_key("dev"), &ExpectedServer::NoServer));
        }
    }

    // covers UP-16: server keys are keyed digests: a credential in a server URL cannot be checked
    // against a copy of the log. The key is the config directory's own, the owner's only, made
    // once and kept.
    #[test]
    fn server_keys_are_keyed_by_the_config_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let url = "https://admin:hunter2@prod.example.com:6443";
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let key = |dir: &Path| ServerKeys::open(dir).server_key(url).unwrap();
        assert_ne!(key(one.path()), key(two.path()));
        assert_eq!(key(one.path()), key(one.path()));
        let path = one.path().join(KEY_FILE);
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!((meta.len(), meta.permissions().mode() & 0o777), (32, 0o600));
        // Not the unkeyed digest of the URL, nor of its endpoint.
        assert_ne!(key(one.path()), fnv1a(url.as_bytes()));
        assert_eq!(key(one.path()).len(), 32);

        let reached = Reached {
            server: Some(url.into()),
            probed: vec![("dev".into(), Some(url.into()))],
        };
        append_call(one.path(), &record(1), &reached).unwrap();
        let read = read(one.path());
        assert_eq!(read[0].server_key, Some(key(one.path())));
        assert_eq!(read[0].probed[0].server_key, Some(key(one.path())));
        let log = std::fs::read_to_string(one.path().join(ACTIVITY_FILE)).unwrap();
        assert!(!log.contains("hunter2") && !log.contains(&fnv1a(url.as_bytes())));
    }

    // covers UP-16: processes that make the key at once agree on one.
    #[test]
    fn concurrent_first_uses_agree_on_one_key() {
        let dir = tempfile::tempdir().unwrap();
        let keys: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        ServerKeys::open(dir.path())
                            .server_key("https://prod:6443")
                            .unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(keys.windows(2).all(|w| w[0] == w[1]), "{keys:?}");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [KEY_FILE], "no drafts left behind");
    }

    // covers UP-16: a draft a crashed process left behind neither stops the key being made nor
    // is removed by a call that did not create it.
    #[test]
    fn a_leftover_key_draft_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let leftover = dir
            .path()
            .join(format!(".{KEY_FILE}.{}.0", std::process::id()));
        std::fs::write(&leftover, b"left by a crash").unwrap();
        let key = dir.path().join(KEY_FILE);

        let error = link_key(&leftover, &key, &[1; 32]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&leftover).unwrap(), b"left by a crash");
        assert!(!key.exists());

        assert!(
            ServerKeys::open(dir.path())
                .server_key("https://a")
                .is_some()
        );
        assert_eq!(std::fs::read(&key).unwrap().len(), 32);
        assert_eq!(std::fs::read(&leftover).unwrap(), b"left by a crash");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    // covers UP-16: without a usable key file a call is still recorded, without a server key;
    // its tab then finds it by context alone.
    #[test]
    fn without_a_key_file_records_have_no_server_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KEY_FILE), b"short").unwrap();
        assert_eq!(ServerKeys::open(dir.path()).server_key("https://a"), None);
        let reached = Reached {
            server: Some("https://prod:6443".into()),
            probed: Vec::new(),
        };
        let call = ActivityRecord {
            server_key: None,
            ..record(1)
        };
        append_call(dir.path(), &call, &reached).unwrap();
        let read = read(dir.path());
        assert_eq!(read, [call]);
        assert_eq!(
            read[0].attribution(&cluster_key("prod"), &at("https://prod:6443")),
            Some(Attribution::Unverified)
        );
        assert_eq!(std::fs::read(dir.path().join(KEY_FILE)).unwrap(), b"short");
    }

    // covers UP-16: a list_clusters call over a huge kubeconfig is still one modest record: the
    // probed clusters past MAX_PROBED are only counted.
    #[test]
    fn a_huge_probe_list_stays_far_below_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let reached = Reached {
            server: None,
            probed: (0..20_000)
                .map(|n| {
                    (
                        format!("context-{n}"),
                        Some(format!("https://api-{n}:6443")),
                    )
                })
                .collect(),
        };
        let listed = ActivityRecord {
            tool: "list_clusters".into(),
            cluster: None,
            server: None,
            cluster_key: None,
            server_key: None,
            ..record(1)
        };
        append_call(dir.path(), &listed, &reached).unwrap();
        append_call(dir.path(), &listed, &reached).unwrap();
        let size = std::fs::metadata(dir.path().join(ACTIVITY_FILE))
            .unwrap()
            .len();
        assert!(size < MAX_FILE_BYTES / 16, "{size}");
        let read = read(dir.path());
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].probed.len(), MAX_PROBED);
        assert_eq!(read[0].probed_more, 20_000 - MAX_PROBED as u64);
        assert_eq!(read[0].probed[0].cluster_key, cluster_key("context-0"));
    }

    // covers UP-16: a call that names no cluster but read about some (list_clusters) is about
    // each cluster it probed, at the server it probed.
    #[test]
    fn a_call_is_about_the_clusters_it_probed() {
        let dir = tempfile::tempdir().unwrap();
        let listed = ActivityRecord {
            tool: "list_clusters".into(),
            cluster: None,
            server: None,
            cluster_key: None,
            server_key: None,
            probed: vec![
                Probed::new(
                    &ServerKeys::with_secret(&[7; 32]),
                    "prod",
                    Some("https://prod:6443"),
                ),
                Probed::new(
                    &ServerKeys::with_secret(&[7; 32]),
                    "dev",
                    Some("https://dev:6443"),
                ),
            ],
            ..record(1)
        };
        append(dir.path(), &listed).unwrap();
        let text = std::fs::read_to_string(dir.path().join(ACTIVITY_FILE)).unwrap();
        assert!(text.contains(&format!(
            r#""probed":[{{"cluster_key":"{}","server_key":"{}"}},"#,
            cluster_key("prod"),
            server_key("https://prod:6443")
        )));
        let read = read(dir.path());
        assert_eq!(read, [listed]);
        let about =
            |context: &str, server: &str| read[0].is_about(&cluster_key(context), &at(server));
        assert!(about("prod", "https://prod:6443/") && about("dev", "https://dev:6443"));
        assert!(!about("prod", "https://new-prod:6443") && !about("staging", "https://prod:6443"));
    }

    // covers UP-16: a writer that holds the lock too long costs other writers one record each,
    // not their reply: they give up after LOCK_WAIT. The lock file is the owner's only.
    #[test]
    fn a_held_lock_skips_the_record_after_a_bounded_wait() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &record(1)).unwrap();
        let lock = dir.path().join(LOCK_FILE);
        let mode = std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();

        let held = Lock::take(&lock, Duration::ZERO).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let config = dir.path().to_owned();
        std::thread::spawn(move || {
            let started = Instant::now();
            let _ = tx.send((append(&config, &record(2)), started.elapsed()));
        });
        let (written, took) = rx
            .recv_timeout(LOCK_WAIT * 5)
            .expect("the writer gives up instead of waiting for the lock");
        assert_eq!(written.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(took >= LOCK_WAIT, "{took:?}");
        drop(held);
        assert_eq!(read(dir.path()), [record(1)]);
        let mode = std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        append(dir.path(), &record(3)).unwrap();
        assert_eq!(read(dir.path()), [record(1), record(3)]);
    }

    // covers UP-16: a key in the format of a process started before an upgrade is neither matched
    // to a server nor hidden, on a named call and on a probe.
    #[test]
    fn an_old_format_server_key_is_listed_unverified() {
        let prod = cluster_key("prod");
        let old = fnv1a(b"https://prod:6443");
        assert_eq!(old.len(), 16);
        let named = ActivityRecord {
            server_key: Some(old.clone()),
            ..record(1)
        };
        let probed = ActivityRecord {
            cluster_key: None,
            cluster: None,
            server_key: None,
            probed: vec![Probed {
                cluster_key: prod.clone(),
                server_key: Some(old),
            }],
            ..record(2)
        };
        for call in [&named, &probed] {
            for server in [
                at("https://prod:6443"),
                ExpectedServer::Unknown,
                ExpectedServer::NoServer,
            ] {
                assert_eq!(
                    call.attribution(&prod, &server),
                    Some(Attribution::Unverified)
                );
            }
        }
    }

    // covers UP-16: without a key to compare with, a call is never presented as the server's,
    // though its own key is current; a known server with a key still separates servers.
    #[test]
    fn a_view_without_a_key_verifies_nothing() {
        let prod = cluster_key("prod");
        let no_key = ServerKeys::default();
        assert_eq!(
            no_key.expected(Some("https://prod:6443")),
            ExpectedServer::Unknown
        );
        assert_eq!(no_key.expected(None), ExpectedServer::NoServer);
        assert_eq!(
            record(1).attribution(&prod, &ExpectedServer::Unknown),
            Some(Attribution::Unverified)
        );
        assert_eq!(
            record(1).attribution(&prod, &at("https://prod:6443")),
            Some(Attribution::Verified)
        );
        assert_eq!(
            record(1).attribution(&prod, &at("https://other:6443")),
            None
        );
    }

    // covers UP-16: a rotation while the log is read does not show expired records or hide the
    // file just rotated: both files are read again.
    #[test]
    fn a_rotation_while_reading_reads_both_files_again() {
        let dir = tempfile::tempdir().unwrap();
        let lines = |range: std::ops::Range<u64>| -> String {
            range
                .map(|n| serde_json::to_string(&record(n)).unwrap() + "\n")
                .collect()
        };
        let previous = dir.path().join(PREVIOUS_FILE);
        let current = dir.path().join(ACTIVITY_FILE);
        std::fs::write(&previous, lines(0..3)).unwrap();
        std::fs::write(&current, lines(3..6)).unwrap();
        let mut rotations = 0;
        let read = read_with(dir.path(), || {
            if rotations == 0 {
                std::fs::rename(&current, &previous).unwrap();
                std::fs::write(&current, lines(6..7)).unwrap();
            }
            rotations += 1;
        });
        assert_eq!(read, (3..7).map(record).collect::<Vec<_>>());
        assert_eq!(rotations, 2);
    }

    #[test]
    fn reading_keeps_the_newest_records() {
        let dir = tempfile::tempdir().unwrap();
        let text: String = (0..MAX_RECORDS as u64 + 3)
            .map(|n| serde_json::to_string(&record(n)).unwrap() + "\n")
            .collect();
        std::fs::write(dir.path().join(ACTIVITY_FILE), text).unwrap();
        let records = read(dir.path());
        assert_eq!(records.len(), MAX_RECORDS);
        assert_eq!(records[0], record(3));
    }
}
