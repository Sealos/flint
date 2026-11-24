//! Which clusters are kept watched in the background, and what running their sign-in was agreed
//! to (ADR-0012).
//!
//! An entry in the settings names a context at its exact server. Agreement to run an exec plugin
//! is never saved: it is a seal held in memory for the running app, over the whole
//! definition the person confirmed. Any change to the context, its user or its cluster entry
//! breaks it, and after a restart the plugin does not run until it is confirmed again. The seal
//! also names the program the command resolved to when it was given, by path and by a digest of
//! its contents: once the command would run another file, or the same file changed, it is void.
//!
//! It is a per-cluster toggle, off by default. Turning it on starts from a [`PluginOffer`], which
//! the core makes for the definition the toggle showed. Approving it gives a [`PluginApproval`]
//! the core accepts once.

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kube::config::{AuthInfo, Kubeconfig};
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::{digest, hmac};
use serde::{Deserialize, Serialize};

use crate::credential::CredentialSource;

/// The most clusters that can be watched, one feed each: what a trend store holds.
pub const MAX_WATCHED_CLUSTERS: usize = crate::metrics_trend::MAX_CLUSTERS;

/// The refusal when [`MAX_WATCHED_CLUSTERS`] are already watched.
pub fn too_many_watched() -> String {
    format!("Flint can keep watching at most {MAX_WATCHED_CLUSTERS} clusters. Turn one off first.")
}

const SALT_BYTES: usize = 16;

/// A cluster kept watched in the background.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedCluster {
    pub context: String,
    /// The exact API server it was turned on for, with its proxy if it has one
    /// ([`crate::kubeconfig::ContextInfo::grant_server`]).
    pub server: String,
}

/// The entries of `watched_clusters` this version can read. One it can't is left out, and never
/// makes the settings file unreadable, which would also hide every cluster from the MCP server.
pub(crate) fn readable_watched<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<WatchedCluster>, D::Error> {
    Ok(watched_entries(toml::Value::deserialize(deserializer)?))
}

/// The readable entries of a `watched_clusters` value.
pub(crate) fn watched_entries(value: toml::Value) -> Vec<WatchedCluster> {
    let toml::Value::Array(entries) = value else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| entry.try_into().ok())
        .collect()
}

/// Agreement to run one exact definition, for as long as the app runs. Neither saved nor shown.
pub(crate) struct PluginConsent {
    salt: [u8; SALT_BYTES],
    seal: Vec<u8>,
    /// The program the definition's command ran when this was given.
    binary: Arc<PluginBinary>,
    /// Ended for good, by a confirmed change or by the person, whatever the files say later.
    void: Arc<AtomicBool>,
}

impl std::fmt::Debug for PluginConsent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConsent").finish_non_exhaustive()
    }
}

impl PluginConsent {
    /// A seal over `definition` and the program its command runs now, or `None` if the system
    /// has no randomness to give or that program can't be found and opened. The program's
    /// contents are read off the caller's thread: they are bound to the file as it is now, and a
    /// file that changes before they are read ends this.
    pub(crate) fn seal(definition: &PluginDefinition) -> Option<Self> {
        Self::seal_under(definition, std::env::var_os("PATH").as_deref())
    }

    /// [`Self::seal`] with `path_var` as the app's own `PATH`.
    fn seal_under(definition: &PluginDefinition, path_var: Option<&OsStr>) -> Option<Self> {
        let binary = PluginBinary::found(definition.program.resolve(path_var)?)?;
        let mut salt = [0u8; SALT_BYTES];
        SystemRandom::new().fill(&mut salt).ok()?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, &salt);
        let seal = hmac::sign(&key, &definition.canonical).as_ref().to_vec();
        let consent = PluginConsent {
            salt,
            seal,
            binary: Arc::new(binary),
            void: Arc::new(AtomicBool::new(false)),
        };
        consent.read_in_background();
        Some(consent)
    }

    /// Whether this is in force for exactly `definition`, whose command still runs the same
    /// program with the same contents, read in full if they may have changed. Blocks on the
    /// file, so it runs off the manager's loop: asked before each unattended start. Another
    /// program, a changed or missing one ends this for good: undoing that brings nothing back.
    pub(crate) fn verify(&self, definition: &PluginDefinition) -> bool {
        self.verify_under(definition, std::env::var_os("PATH").as_deref())
    }

    /// [`Self::verify`] with `path_var` as the app's own `PATH`.
    fn verify_under(&self, definition: &PluginDefinition, path_var: Option<&OsStr>) -> bool {
        if !self.sealed_over(definition) {
            return false;
        }
        let same = definition
            .program
            .resolve(path_var)
            .is_some_and(|path| self.binary.verify(&path));
        if !same {
            self.end();
        }
        same
    }

    /// What a toggle shows: whether this still looks in force for `definition`. Never reads the
    /// program's contents and looks at the file at most once per [`LOOK_INTERVAL`], so it may lag
    /// behind a change; a file that may have changed is read in the background, and ends this if
    /// it did. Grants nothing by itself: each start asks [`Self::verify`] first.
    pub(crate) fn holds(&self, definition: &PluginDefinition) -> bool {
        self.holds_at(
            definition,
            std::env::var_os("PATH").as_deref(),
            Instant::now(),
        )
    }

    /// [`Self::holds`] with `path_var` as the app's own `PATH`, at `now`.
    fn holds_at(
        &self,
        definition: &PluginDefinition,
        path_var: Option<&OsStr>,
        now: Instant,
    ) -> bool {
        if !self.sealed_over(definition) {
            return false;
        }
        match self
            .binary
            .look(now, || definition.program.resolve(path_var))
        {
            Look::Same => true,
            Look::Unsure => {
                self.read_in_background();
                true
            }
            Look::Changed => {
                self.end();
                false
            }
        }
    }

    /// Whether this is in force and was sealed over exactly `definition`.
    fn sealed_over(&self, definition: &PluginDefinition) -> bool {
        if self.is_void() {
            return false;
        }
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.salt);
        hmac::verify(&key, &definition.canonical, &self.seal).is_ok()
    }

    fn end(&self) {
        // The path and digest stay out of the log.
        tracing::debug!("background sign-in consent ended: the plugin program changed");
        self.void();
    }

    /// Reads the program on a thread of its own, unless that is already under way, and ends this
    /// if its contents are not the ones agreed to. If no thread can start, the next
    /// [`Self::verify`] reads them.
    fn read_in_background(&self) {
        if !self.binary.start_reading() {
            return;
        }
        let (binary, void) = (Arc::clone(&self.binary), Arc::clone(&self.void));
        let spawned = std::thread::Builder::new()
            .name("plugin-digest".to_owned())
            .spawn(move || {
                if !binary.verify(&binary.path) {
                    tracing::debug!("background sign-in consent ended: the plugin program changed");
                    void.store(true, Ordering::SeqCst);
                }
                binary.done_reading();
            });
        if spawned.is_err() {
            self.binary.done_reading();
        }
    }

    /// Ends this for good. Takes effect at once, from any thread.
    pub(crate) fn void(&self) {
        self.void.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_void(&self) -> bool {
        self.void.load(Ordering::SeqCst)
    }
}

/// How long what a toggle shows of a plugin program stands before the file is looked at again.
const LOOK_INTERVAL: Duration = Duration::from_secs(2);

/// What a look at a plugin program found, without reading its contents.
#[derive(Debug, PartialEq, Eq)]
enum Look {
    Same,
    /// The same file, which may have new contents: only reading them tells.
    Unsure,
    /// Another file, or none, or one whose agreed contents were never read and can't be now.
    Changed,
}

/// The program a plugin's command resolved to, and what its contents were agreed to be.
struct PluginBinary {
    path: PathBuf,
    /// The file when consent was given. The contents agreed to are the ones it had then.
    sealed: FileStamp,
    state: Mutex<BinaryState>,
}

struct BinaryState {
    /// The SHA-256 of the contents agreed to, once read.
    digest: Option<digest::Digest>,
    /// The file as it was when its contents last matched, so an unchanged one is not read again.
    checked: FileStamp,
    /// When a toggle last looked at the file and found it unchanged.
    looked: Option<Instant>,
    /// A background read is under way.
    reading: bool,
}

impl PluginBinary {
    /// The program at `path`, if it can be opened, bound to the file as it is now.
    fn found(path: PathBuf) -> Option<Self> {
        let file = std::fs::File::open(&path).ok()?;
        let sealed = FileStamp::of(&file.metadata().ok()?);
        Some(PluginBinary {
            path,
            state: Mutex::new(BinaryState {
                digest: None,
                checked: sealed.clone(),
                looked: None,
                reading: false,
            }),
            sealed,
        })
    }

    /// Whether `path` is this program with the contents agreed to, read in full unless the file
    /// is exactly as it was when they last matched.
    fn verify(&self, path: &Path) -> bool {
        if path != self.path {
            return false;
        }
        let Ok(now) = std::fs::metadata(path).map(|meta| FileStamp::of(&meta)) else {
            return false;
        };
        {
            let Ok(state) = self.state.lock() else {
                return false;
            };
            if state.digest.is_some() && state.checked == now {
                return true;
            }
        }
        // No lock is held while the file is read, so a toggle's look never waits for it.
        let Some((digest, stamp)) = digest_of(path) else {
            return false;
        };
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        match state.digest {
            Some(agreed) if agreed.as_ref() != digest.as_ref() => return false,
            Some(_) => {}
            // The first read: the contents are the agreed ones only if the file is as it was.
            None if stamp != self.sealed => return false,
            None => state.digest = Some(digest),
        }
        state.checked = stamp;
        true
    }

    /// Looks at the file `resolve` finds, at most once per [`LOOK_INTERVAL`], without reading it.
    fn look(&self, now: Instant, resolve: impl FnOnce() -> Option<PathBuf>) -> Look {
        let recent = self.state.lock().is_ok_and(|state| {
            state
                .looked
                .is_some_and(|at| now.saturating_duration_since(at) < LOOK_INTERVAL)
        });
        if recent {
            return Look::Same;
        }
        if resolve().as_deref() != Some(self.path.as_path()) {
            return Look::Changed;
        }
        let Ok(stamp) = std::fs::metadata(&self.path).map(|meta| FileStamp::of(&meta)) else {
            return Look::Changed;
        };
        let Ok(mut state) = self.state.lock() else {
            return Look::Changed;
        };
        if stamp == state.checked {
            state.looked = Some(now);
            Look::Same
        } else if state.digest.is_none() {
            // Changed before its contents were read: what was agreed to can't be known now.
            Look::Changed
        } else {
            state.looked = Some(now);
            Look::Unsure
        }
    }

    /// Marks a background read as under way, unless one already is.
    fn start_reading(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|mut state| !std::mem::replace(&mut state.reading, true))
    }

    fn done_reading(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.reading = false;
        }
    }
}

/// What tells a file's contents may have changed without reading them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    device: u64,
    inode: u64,
    changed: (i64, i64),
}

impl FileStamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        FileStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
            device: meta.dev(),
            inode: meta.ino(),
            changed: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// The SHA-256 of the file at `path` and its stamp, or `None` if it can't be read or changed
/// while it was.
fn digest_of(path: &Path) -> Option<(digest::Digest, FileStamp)> {
    let mut file = std::fs::File::open(path).ok()?;
    let before = FileStamp::of(&file.metadata().ok()?);
    let mut context = digest::Context::new(&digest::SHA256);
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    let after = FileStamp::of(&std::fs::metadata(path).ok()?);
    (after == before).then(|| (context.finish(), before))
}

/// An exec plugin's command as it is started: its program, and the `PATH` the plugin gets.
struct PluginProgram {
    command: String,
    /// The `PATH` its `env` sets, if it does.
    path_var: Option<OsString>,
    /// Its `dropEnv` removes `PATH`, after its `env` is applied, as when it is started.
    drops_path: bool,
}

impl PluginProgram {
    /// The absolute path of the file the command runs when the app's own `PATH` is `path_var`.
    /// A command with a slash is a path. Otherwise it is searched for as `execvp` does in the
    /// plugin's environment: the `PATH` in its `env` wins over the app's, and the first file
    /// this process may execute is the one. Without a `PATH`, `execvp` searches a default that
    /// differs between systems, so nothing is resolved.
    fn resolve(&self, path_var: Option<&OsStr>) -> Option<PathBuf> {
        let found = if self.command.contains('/') {
            Some(PathBuf::from(&self.command)).filter(|path| executable(path))
        } else {
            if self.drops_path {
                return None;
            }
            let search = self.path_var.as_deref().or(path_var)?;
            std::env::split_paths(search)
                .map(|dir| dir.join(&self.command))
                .find(|path| executable(path))
        };
        std::path::absolute(found?).ok()
    }
}

/// Whether `path` is a file this process may execute, as `execvp` decides when it searches:
/// one it may not is skipped for the next directory.
fn executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(name) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `name` is a NUL-terminated string that outlives the call.
    let allowed = unsafe { libc::access(name.as_ptr(), libc::X_OK) } == 0;
    allowed && std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// What identifies one offer. Random, so it can't be worked out from anything shown.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OfferTicket([u8; SALT_BYTES]);

impl OfferTicket {
    /// A new ticket, or `None` if the system has no randomness to give.
    pub(crate) fn new() -> Option<Self> {
        let mut ticket = [0u8; SALT_BYTES];
        SystemRandom::new().fill(&mut ticket).ok()?;
        Some(OfferTicket(ticket))
    }
}

/// A key for this run of the app, under which a view names the exact definition it shows.
pub(crate) struct ViewKey(hmac::Key);

impl ViewKey {
    /// A new key, or `None` if the system has no randomness to give.
    pub(crate) fn new() -> Option<Self> {
        let mut key = [0u8; SALT_BYTES];
        SystemRandom::new().fill(&mut key).ok()?;
        Some(ViewKey(hmac::Key::new(hmac::HMAC_SHA256, &key)))
    }

    /// What names `definition` in a view. Says nothing of it, and differs after any change.
    pub(crate) fn name(&self, definition: &PluginDefinition) -> u64 {
        let tag = hmac::sign(&self.0, &definition.canonical);
        let mut name = [0u8; 8];
        name.copy_from_slice(&tag.as_ref()[..8]);
        u64::from_le_bytes(name)
    }
}

/// One turn of a cluster's background sign-in toggle to on, for the definition it showed, from
/// [`CoreHandle::offer_keep_watching`](crate::CoreHandle::offer_keep_watching).
pub struct PluginOffer {
    pub(crate) context: String,
    pub(crate) ticket: OfferTicket,
}

impl std::fmt::Debug for PluginOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginOffer")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

impl PluginOffer {
    /// The person turned the toggle on. Call this only from that toggle's click handler.
    pub fn approve(self) -> PluginApproval {
        PluginApproval {
            context: self.context,
            ticket: self.ticket,
        }
    }
}

/// A confirmed [`PluginOffer`], for [`Command::ApproveKeepWatching`](crate::Command). The core
/// accepts it once, and only while the definition is the one the offer showed.
#[derive(Clone)]
pub struct PluginApproval {
    pub(crate) context: String,
    pub(crate) ticket: OfferTicket,
}

impl std::fmt::Debug for PluginApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginApproval")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

/// How a context signs in, for whether it can be watched in the background.
#[derive(Debug)]
pub(crate) enum SignIn {
    /// Runs nothing: a certificate, a token or a token file.
    Static,
    Plugin(PluginDefinition),
    /// An auth provider's command, which cannot be stopped once it runs: never in the background.
    Provider,
    /// The context or its user can't be resolved.
    Unknown,
}

/// A context's exec plugin definition.
pub(crate) struct PluginDefinition {
    /// What the confirmation names.
    pub(crate) command: String,
    /// What it runs.
    program: PluginProgram,
    /// The named context and its user and cluster entries, in a stable form. Holds credentials, so
    /// it never leaves this struct except through a seal.
    canonical: Vec<u8>,
}

impl std::fmt::Debug for PluginDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginDefinition")
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

/// How `context` in `kubeconfig` signs in.
pub(crate) fn sign_in_of(kubeconfig: &Kubeconfig, context: &str) -> SignIn {
    let Some(entry) = kubeconfig
        .contexts
        .iter()
        .find(|named| named.name == context)
        .and_then(|named| named.context.as_ref())
    else {
        return SignIn::Unknown;
    };
    let Some(user) = &entry.user else {
        return SignIn::Static;
    };
    let Some(auth) = kubeconfig
        .auth_infos
        .iter()
        .find(|named| &named.name == user)
        .and_then(|named| named.auth_info.as_ref())
    else {
        return SignIn::Unknown;
    };
    match CredentialSource::of(auth) {
        CredentialSource::AuthProvider => SignIn::Provider,
        CredentialSource::Plugin => {
            let cluster = kubeconfig
                .clusters
                .iter()
                .find(|named| named.name == entry.cluster);
            let Ok(mut value) = serde_json::to_value((context, entry, user, auth, cluster)) else {
                return SignIn::Unknown;
            };
            // Exec `env` items are maps, serialized in a different order at each load.
            value.sort_all_objects();
            match (shown_command(auth), plugin_program(auth)) {
                (Some(command), Some(program)) => SignIn::Plugin(PluginDefinition {
                    command,
                    program,
                    canonical: value.to_string().into_bytes(),
                }),
                _ => SignIn::Unknown,
            }
        }
        CredentialSource::Definition | CredentialSource::TokenFile(_) => SignIn::Static,
    }
}

/// The plugin's program as written, and its arguments with anything that looks secret hidden.
fn shown_command(auth: &AuthInfo) -> Option<String> {
    let exec = auth.exec.as_ref()?;
    let program = exec.command.as_deref()?.trim();
    if program.is_empty() {
        return None;
    }
    let mut parts = vec![program.to_owned()];
    let mut hide_next = false;
    for arg in exec.args.iter().flatten() {
        if std::mem::take(&mut hide_next) {
            parts.push("[hidden]".to_owned());
            continue;
        }
        match arg.split_once('=') {
            Some((name, _)) if secret_name(name) => parts.push(format!("{name}=[hidden]")),
            Some(_) => parts.push(arg.clone()),
            None => {
                hide_next = arg.starts_with('-') && secret_name(arg);
                parts.push(arg.clone());
            }
        }
    }
    Some(parts.join(" "))
}

/// The program as the plugin is started with it, and the `PATH` its `env` sets, if it does.
fn plugin_program(auth: &AuthInfo) -> Option<PluginProgram> {
    let exec = auth.exec.as_ref()?;
    let path_var = exec.env.iter().flatten().rev().find_map(|item| {
        (item.get("name").map(String::as_str) == Some("PATH"))
            .then(|| item.get("value").map(OsString::from))
            .flatten()
    });
    let drops_path = exec.drop_env.iter().flatten().any(|name| name == "PATH");
    Some(PluginProgram {
        command: exec.command.clone()?,
        path_var,
        drops_path,
    })
}

fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "credential", "key"]
        .iter()
        .any(|word| name.contains(word))
}

/// What a cluster's toggle says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepWatching {
    Off,
    Watching,
    /// Stopped after a failure, until the person acts.
    SignInNeeded,
    /// Its sign-in has no agreement for this definition, because it changed or the app restarted:
    /// nothing runs until it is confirmed again.
    NeedsConfirming,
    /// A kubeconfig file that can't be read could change its definition. It resumes by itself.
    Paused,
}

/// A cluster's background sign-in toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundSignIn {
    /// Its credential runs nothing, so there is no toggle.
    NotNeeded,
    /// Shown disabled: an auth provider, or a user Flint can't read.
    Unavailable,
    Off,
    On,
}

/// A cluster's toggles, and what turning them on would agree to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepWatchView {
    pub status: KeepWatching,
    pub sign_in: BackgroundSignIn,
    /// The command the background sign-in toggle names, when the credential is an exec plugin.
    pub command: Option<String>,
    /// Names the exact plugin definition on show. Turning the toggle on sends it back, so a click
    /// made on a definition that has changed since grants nothing.
    pub shown_sign_in: Option<u64>,
    /// The definition this view describes. Turning on a credential that runs nothing sends it
    /// back, so a request made on a definition that has changed since is refused. It is not
    /// consent to run a plugin, which takes a [`PluginApproval`].
    pub definition: Option<u64>,
    /// Why it cannot be turned on, if so.
    pub refusal: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(user: &str) -> String {
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: https://a.example\n\
             contexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\nusers:\n- name: u\n  user:\n{user}"
        )
    }

    fn config(user: &str) -> Kubeconfig {
        Kubeconfig::from_yaml(&text(user)).expect("a kubeconfig")
    }

    /// An exec user running `command`.
    fn plugin(command: &str) -> String {
        format!(
            "    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      command: {command}\n      args: [eks, get-token, --token, abc, --key=xyz]\n      env:\n      - name: AWS_PROFILE\n        value: prod\n"
        )
    }

    /// A directory holding executable fake plugin programs.
    struct Programs(tempfile::TempDir);

    impl Programs {
        fn new() -> Self {
            Programs(tempfile::tempdir().expect("a temp dir"))
        }

        /// Writes `dir/name` as an executable script printing `marker`, and returns its path.
        fn write(&self, dir: &str, name: &str, marker: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = self.0.path().join(dir);
            std::fs::create_dir_all(&dir).expect("a dir");
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho {marker}\n")).expect("a script");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("executable");
            path
        }

        fn dir(&self, dir: &str) -> PathBuf {
            self.0.path().join(dir)
        }
    }

    /// A user running `/…/bin/aws`, with that program in place.
    fn absolute_plugin(programs: &Programs) -> String {
        plugin(programs.write("bin", "aws", "a").to_str().expect("utf-8"))
    }

    fn definition(kubeconfig: &Kubeconfig) -> PluginDefinition {
        match sign_in_of(kubeconfig, "dev") {
            SignIn::Plugin(definition) => definition,
            other => panic!("not a plugin: {other:?}"),
        }
    }

    #[test]
    fn the_command_is_named_with_secret_arguments_hidden() {
        assert_eq!(
            definition(&config(&plugin("/usr/bin/aws"))).command,
            "/usr/bin/aws eks get-token --token [hidden] --key=[hidden]"
        );
    }

    /// Waits until `consent`'s background read of its program is over.
    fn settled(consent: PluginConsent) -> PluginConsent {
        wait_for(|| !consent.binary.state.lock().expect("the state").reading);
        consent
    }

    fn wait_for(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The definition of `kubeconfig` with its exec plugin's `dropEnv` set to `names`, which no file can set.
    fn dropping(kubeconfig: &mut Kubeconfig, names: &[&str]) -> PluginDefinition {
        let exec = kubeconfig.auth_infos[0]
            .auth_info
            .as_mut()
            .and_then(|auth| auth.exec.as_mut())
            .expect("an exec user");
        exec.drop_env = Some(names.iter().map(|name| (*name).to_owned()).collect());
        definition(kubeconfig)
    }

    #[test]
    fn consent_is_void_on_every_definition_change() {
        let programs = Programs::new();
        let user = absolute_plugin(&programs);
        let base = config(&user);
        let consent = settled(PluginConsent::seal(&definition(&base)).expect("a seal"));
        assert!(consent.verify(&definition(&base)));
        assert!(consent.holds(&definition(&base)));
        let original = text(&user);
        let changed = |from: &str, to: &str| {
            assert!(original.contains(from), "{from} not in the fixture");
            let edited = Kubeconfig::from_yaml(&original.replace(from, to)).expect("a kubeconfig");
            assert!(!consent.holds(&definition(&edited)), "{from} -> {to}");
            assert!(!consent.verify(&definition(&edited)), "{from} -> {to}");
        };
        let first = programs.dir("bin").join("aws");
        let other = programs.write("other", "aws", "a");
        changed(
            first.to_str().expect("utf-8"),
            other.to_str().expect("utf-8"),
        );
        changed("get-token", "get-other");
        changed("AWS_PROFILE", "AWS_OTHER");
        changed("value: prod", "value: staging");
        changed("https://a.example", "https://b.example");
        changed("user: u\n", "user: u\n    namespace: other\n");
        let renamed = original
            .replace("user: u\n", "user: v\n")
            .replace("- name: u\n", "- name: v\n");
        let renamed = Kubeconfig::from_yaml(&renamed).expect("a kubeconfig");
        assert!(!consent.holds(&definition(&renamed)), "another user");
    }

    #[test]
    fn a_seal_shows_nothing_and_a_broken_one_never_holds() {
        let programs = Programs::new();
        let kubeconfig = config(&absolute_plugin(&programs));
        let found = definition(&kubeconfig);
        let consent = settled(PluginConsent::seal(&found).expect("a seal"));
        assert_eq!(format!("{consent:?}"), "PluginConsent { .. }");
        let broken = PluginConsent {
            salt: consent.salt,
            seal: vec![0; consent.seal.len()],
            binary: Arc::clone(&consent.binary),
            void: Arc::new(AtomicBool::new(false)),
        };
        assert!(!broken.holds(&found));
        assert!(!broken.verify(&found));
    }

    #[test]
    fn a_saved_consent_field_is_ignored() {
        let entry: WatchedCluster = toml::from_str(
            "context = \"dev\"\nserver = \"https://a.example\"\n\
             [plugin_consent]\nsalt = \"00\"\nseal = \"00\"\n",
        )
        .expect("an entry");
        let saved = toml::to_string(&entry).expect("toml");
        assert!(!saved.contains("plugin_consent"), "{saved}");
    }

    #[test]
    fn sign_in_kinds() {
        assert!(matches!(
            sign_in_of(&config("    token: abc\n"), "dev"),
            SignIn::Static
        ));
        assert!(matches!(
            sign_in_of(
                &config("    auth-provider:\n      name: oidc\n      config: {}\n"),
                "dev"
            ),
            SignIn::Provider
        ));
        assert!(matches!(
            sign_in_of(&config(&plugin("/usr/bin/aws")), "missing"),
            SignIn::Unknown
        ));
    }

    #[test]
    fn consent_holds_for_the_same_program_found_on_path() {
        let programs = Programs::new();
        programs.write("bin", "aws", "a");
        let path = programs.dir("bin").into_os_string();
        let found = definition(&config(&plugin("aws")));
        let consent = settled(PluginConsent::seal_under(&found, Some(&path)).expect("a seal"));
        assert!(consent.verify_under(&found, Some(&path)));
        assert!(
            consent.verify_under(&found, Some(&path)),
            "again, from the cache"
        );
        assert!(consent.holds_at(&found, Some(&path), Instant::now()));
        assert!(!consent.is_void());
    }

    #[test]
    fn consent_is_void_once_path_finds_another_program() {
        let programs = Programs::new();
        programs.write("first", "aws", "a");
        // The same contents elsewhere is still another program.
        programs.write("second", "aws", "a");
        let found = definition(&config(&plugin("aws")));
        let first = programs.dir("first").into_os_string();
        let both =
            std::env::join_paths([programs.dir("second"), programs.dir("first")]).expect("a PATH");
        let consent = settled(PluginConsent::seal_under(&found, Some(&first)).expect("a seal"));
        assert!(!consent.holds_at(&found, Some(&both), Instant::now()));
        assert!(consent.is_void());
        assert!(
            !consent.verify_under(&found, Some(&first)),
            "going back brings nothing back"
        );
    }

    #[test]
    fn a_path_in_the_plugin_env_is_the_one_searched() {
        let programs = Programs::new();
        programs.write("own", "aws", "a");
        programs.write("app", "aws", "b");
        let user = plugin("aws").replace(
            "      - name: AWS_PROFILE",
            &format!(
                "      - name: PATH\n        value: {}\n      - name: AWS_PROFILE",
                programs.dir("own").display()
            ),
        );
        let found = definition(&config(&user));
        let app = programs.dir("app").into_os_string();
        let consent = settled(PluginConsent::seal_under(&found, Some(&app)).expect("a seal"));
        assert_eq!(consent.binary.path, programs.dir("own").join("aws"));
        assert!(consent.verify_under(&found, None));
    }

    #[test]
    fn a_dropped_path_resolves_nothing() {
        // The plugin starts without `PATH`, so `execvp` searches a system default, not the
        // `PATH` in its `env` or the app's.
        let programs = Programs::new();
        programs.write("own", "aws", "a");
        let user = plugin("aws").replace(
            "      - name: AWS_PROFILE",
            &format!(
                "      - name: PATH\n        value: {}\n      - name: AWS_PROFILE",
                programs.dir("own").display()
            ),
        );
        let path = programs.dir("own").into_os_string();
        let mut kubeconfig = config(&user);
        let found = definition(&kubeconfig);
        let consent = settled(PluginConsent::seal_under(&found, Some(&path)).expect("a seal"));
        let dropped = dropping(&mut kubeconfig, &["PATH"]);
        assert!(PluginConsent::seal_under(&dropped, Some(&path)).is_none());
        // The definition's text is the same, but its program no longer resolves.
        assert!(!consent.verify_under(&dropped, Some(&path)));
        assert!(consent.is_void());
        // Another dropped variable, or a command with a slash, still resolves.
        let mut kubeconfig = config(&plugin("aws"));
        let other = dropping(&mut kubeconfig, &["HOME"]);
        assert!(PluginConsent::seal_under(&other, Some(&path)).is_some());
        let mut kubeconfig = config(&absolute_plugin(&programs));
        let absolute = dropping(&mut kubeconfig, &["PATH"]);
        assert!(PluginConsent::seal_under(&absolute, None).is_some());
    }

    #[test]
    fn a_program_this_process_may_not_execute_is_skipped() {
        use std::os::unix::fs::PermissionsExt as _;
        let programs = Programs::new();
        // Executable by group and others only: `execvp` skips it when this process owns it.
        let earlier = programs.write("earlier", "aws", "a");
        std::fs::set_permissions(&earlier, std::fs::Permissions::from_mode(0o611))
            .expect("permissions");
        programs.write("later", "aws", "a");
        if executable(&earlier) {
            // Root may execute it: nothing to skip.
            return;
        }
        let found = definition(&config(&plugin("aws")));
        let both =
            std::env::join_paths([programs.dir("earlier"), programs.dir("later")]).expect("a PATH");
        let consent = settled(PluginConsent::seal_under(&found, Some(&both)).expect("a seal"));
        assert_eq!(consent.binary.path, programs.dir("later").join("aws"));
        let only = programs.dir("earlier").into_os_string();
        assert!(PluginConsent::seal_under(&found, Some(&only)).is_none());
    }

    #[test]
    fn consent_is_void_once_the_program_changes() {
        let programs = Programs::new();
        let found = definition(&config(&absolute_plugin(&programs)));
        let consent = settled(PluginConsent::seal(&found).expect("a seal"));
        let path = consent.binary.path.clone();
        // A new stamp with the same contents is read again and still holds.
        let file = std::fs::File::options()
            .append(true)
            .open(&path)
            .expect("open");
        file.set_modified(std::time::SystemTime::UNIX_EPOCH)
            .expect("set the time");
        drop(file);
        assert!(consent.verify(&found), "same contents");
        programs.write("bin", "aws", "b");
        assert!(!consent.verify(&found));
        assert!(consent.is_void());
        programs.write("bin", "aws", "a");
        assert!(
            !consent.verify(&found),
            "undoing the change brings nothing back"
        );
        assert!(!consent.holds(&found));
    }

    #[test]
    fn the_toggle_may_lag_but_a_start_never_does() {
        let programs = Programs::new();
        let found = definition(&config(&absolute_plugin(&programs)));
        let consent = settled(PluginConsent::seal(&found).expect("a seal"));
        let start = Instant::now();
        assert!(consent.holds_at(&found, None, start));
        programs.write("bin", "aws", "b");
        // Within the interval the toggle shows what it last saw, without looking.
        assert!(consent.holds_at(&found, None, start));
        assert!(!consent.is_void());
        // A start always reads the file.
        assert!(!consent.verify(&found));
        assert!(consent.is_void());
    }

    #[test]
    fn the_toggle_reads_a_changed_program_in_the_background() {
        let programs = Programs::new();
        let found = definition(&config(&absolute_plugin(&programs)));
        let consent = settled(PluginConsent::seal(&found).expect("a seal"));
        programs.write("bin", "aws", "b");
        // The file changed: the look does not read it, and the toggle lags until the read ends.
        assert!(consent.holds_at(&found, None, Instant::now()));
        wait_for(|| consent.is_void());
        assert!(!consent.holds(&found));
    }

    #[test]
    fn a_program_changed_before_it_was_read_is_never_agreed_to() {
        let programs = Programs::new();
        let path = programs.write("bin", "aws", "a");
        let binary = PluginBinary::found(path.clone()).expect("the program");
        programs.write("bin", "aws", "b");
        programs.write("bin", "aws", "a");
        assert_eq!(
            binary.look(Instant::now(), || Some(path.clone())),
            Look::Changed
        );
        assert!(!binary.verify(&path), "the contents agreed to are unknown");
    }

    #[test]
    fn consent_is_void_once_the_program_is_missing() {
        let programs = Programs::new();
        let found = definition(&config(&absolute_plugin(&programs)));
        let consent = settled(PluginConsent::seal(&found).expect("a seal"));
        std::fs::remove_file(&consent.binary.path).expect("removed");
        assert!(!consent.holds(&found));
        assert!(consent.is_void());
    }

    #[test]
    fn no_consent_is_given_for_a_program_that_is_not_there() {
        let programs = Programs::new();
        let missing = programs.dir("bin").join("aws");
        let found = definition(&config(&plugin(missing.to_str().expect("utf-8"))));
        assert!(PluginConsent::seal(&found).is_none());
        let empty = programs.dir("none").into_os_string();
        let searched = definition(&config(&plugin("aws")));
        assert!(PluginConsent::seal_under(&searched, Some(&empty)).is_none());
        assert!(PluginConsent::seal_under(&searched, None).is_none());
    }
}
