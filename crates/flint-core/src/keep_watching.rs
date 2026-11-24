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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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
    binary: PluginBinary,
    /// Ended for good, by a confirmed change or by the person, whatever the files say later.
    void: AtomicBool,
}

impl std::fmt::Debug for PluginConsent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConsent").finish_non_exhaustive()
    }
}

impl PluginConsent {
    /// A seal over `definition` and the program its command runs now, or `None` if the system
    /// has no randomness to give or that program can't be found and read.
    pub(crate) fn seal(definition: &PluginDefinition) -> Option<Self> {
        Self::seal_under(definition, std::env::var_os("PATH").as_deref())
    }

    /// [`Self::seal`] with `path_var` as the app's own `PATH`.
    fn seal_under(definition: &PluginDefinition, path_var: Option<&OsStr>) -> Option<Self> {
        let binary = PluginBinary::read(definition.program.resolve(path_var)?)?;
        let mut salt = [0u8; SALT_BYTES];
        SystemRandom::new().fill(&mut salt).ok()?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, &salt);
        let seal = hmac::sign(&key, &definition.canonical).as_ref().to_vec();
        Some(PluginConsent {
            salt,
            seal,
            binary,
            void: AtomicBool::new(false),
        })
    }

    /// Whether this is still in force and was sealed over exactly `definition`, whose command
    /// still runs the same program. Asked before each unattended start. Another program, a
    /// changed or missing one ends this for good: undoing that brings nothing back.
    pub(crate) fn holds(&self, definition: &PluginDefinition) -> bool {
        self.holds_under(definition, std::env::var_os("PATH").as_deref())
    }

    /// [`Self::holds`] with `path_var` as the app's own `PATH`.
    fn holds_under(&self, definition: &PluginDefinition, path_var: Option<&OsStr>) -> bool {
        if self.is_void() {
            return false;
        }
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.salt);
        if hmac::verify(&key, &definition.canonical, &self.seal).is_err() {
            return false;
        }
        let same = definition
            .program
            .resolve(path_var)
            .is_some_and(|path| self.binary.matches(&path));
        if !same {
            // The path and digest stay out of the log.
            tracing::debug!("background sign-in consent ended: the plugin program changed");
            self.void();
        }
        same
    }

    /// Ends this for good. Takes effect at once, from any thread.
    pub(crate) fn void(&self) {
        self.void.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_void(&self) -> bool {
        self.void.load(Ordering::SeqCst)
    }
}

/// The program a plugin's command resolved to, and a digest of its contents.
struct PluginBinary {
    path: PathBuf,
    digest: digest::Digest,
    /// The file as it was when its contents last matched, so an unchanged one is not read again.
    checked: Mutex<Option<FileStamp>>,
}

impl PluginBinary {
    fn read(path: PathBuf) -> Option<Self> {
        let (digest, stamp) = digest_of(&path)?;
        Some(PluginBinary {
            path,
            digest,
            checked: Mutex::new(Some(stamp)),
        })
    }

    /// Whether `path` is this program, with the same contents.
    fn matches(&self, path: &Path) -> bool {
        if path != self.path {
            return false;
        }
        let Ok(now) = std::fs::metadata(path).map(|meta| FileStamp::of(&meta)) else {
            return false;
        };
        let Ok(mut checked) = self.checked.lock() else {
            return false;
        };
        if checked.as_ref() == Some(&now) {
            return true;
        }
        match digest_of(path) {
            Some((digest, stamp)) if digest.as_ref() == self.digest.as_ref() => {
                *checked = Some(stamp);
                true
            }
            _ => false,
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

/// An exec plugin's command as it is started: its program, and the `PATH` its `env` sets.
struct PluginProgram {
    command: String,
    path_var: Option<OsString>,
}

impl PluginProgram {
    /// The absolute path of the file the command runs when the app's own `PATH` is `path_var`.
    /// A command with a slash is a path; otherwise the `PATH` in its `env` wins over the app's,
    /// as when it is started.
    fn resolve(&self, path_var: Option<&OsStr>) -> Option<PathBuf> {
        let found = if self.command.contains('/') {
            Some(PathBuf::from(&self.command)).filter(|path| executable(path))
        } else {
            let search = self.path_var.as_deref().or(path_var)?;
            std::env::split_paths(search)
                .map(|dir| dir.join(&self.command))
                .find(|path| executable(path))
        };
        std::path::absolute(found?).ok()
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
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
    Some(PluginProgram {
        command: exec.command.clone()?,
        path_var,
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

    #[test]
    fn consent_is_void_on_every_definition_change() {
        let programs = Programs::new();
        let user = absolute_plugin(&programs);
        let base = config(&user);
        let consent = PluginConsent::seal(&definition(&base)).expect("a seal");
        assert!(consent.holds(&definition(&base)));
        let original = text(&user);
        let changed = |from: &str, to: &str| {
            assert!(original.contains(from), "{from} not in the fixture");
            let edited = Kubeconfig::from_yaml(&original.replace(from, to)).expect("a kubeconfig");
            assert!(!consent.holds(&definition(&edited)), "{from} -> {to}");
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
        let consent = PluginConsent::seal(&found).expect("a seal");
        assert_eq!(format!("{consent:?}"), "PluginConsent { .. }");
        let broken = PluginConsent {
            salt: consent.salt,
            seal: vec![0; consent.seal.len()],
            binary: PluginBinary::read(consent.binary.path.clone()).expect("the program"),
            void: AtomicBool::new(false),
        };
        assert!(!broken.holds(&found));
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
        let consent = PluginConsent::seal_under(&found, Some(&path)).expect("a seal");
        assert!(consent.holds_under(&found, Some(&path)));
        assert!(
            consent.holds_under(&found, Some(&path)),
            "again, from the cache"
        );
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
        let consent = PluginConsent::seal_under(&found, Some(&first)).expect("a seal");
        assert!(!consent.holds_under(&found, Some(&both)));
        assert!(consent.is_void());
        assert!(
            !consent.holds_under(&found, Some(&first)),
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
        let consent = PluginConsent::seal_under(&found, Some(&app)).expect("a seal");
        assert_eq!(consent.binary.path, programs.dir("own").join("aws"));
        assert!(consent.holds_under(&found, None));
    }

    #[test]
    fn consent_is_void_once_the_program_changes() {
        let programs = Programs::new();
        let found = definition(&config(&absolute_plugin(&programs)));
        let consent = PluginConsent::seal(&found).expect("a seal");
        let path = consent.binary.path.clone();
        // A new stamp with the same contents is read again and still holds.
        let file = std::fs::File::options()
            .append(true)
            .open(&path)
            .expect("open");
        file.set_modified(std::time::SystemTime::UNIX_EPOCH)
            .expect("set the time");
        drop(file);
        assert!(consent.holds(&found), "same contents");
        programs.write("bin", "aws", "b");
        assert!(!consent.holds(&found));
        assert!(consent.is_void());
        programs.write("bin", "aws", "a");
        assert!(
            !consent.holds(&found),
            "undoing the change brings nothing back"
        );
    }

    #[test]
    fn consent_is_void_once_the_program_is_missing() {
        let programs = Programs::new();
        let found = definition(&config(&absolute_plugin(&programs)));
        let consent = PluginConsent::seal(&found).expect("a seal");
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
