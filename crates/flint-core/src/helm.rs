//! Read-only Helm releases through the `helm` CLI (ADR-0004): the list, one release's details,
//! history, values and manifest.
//!
//! `helm` is another program acting with a cluster's credentials, so each run is held to the
//! connection Flint admitted:
//!
//! - It gets a kubeconfig of its own, written for this run from the definition the connection was
//!   built from, holding that one context and only the credential the connection uses. The
//!   person's files are never given to it, so editing them cannot move a run to another server
//!   or user.
//! - The binary and every folder on the way to it must be safe from other accounts, and the
//!   run's files stay in a private directory.
//! - Certificate files go in as the bytes the connection was built from, and a file that no longer
//!   holds them refuses the run.
//! - `helm` never starts a program to sign in. An exec plugin is run by Flint before the run, as a
//!   kept cluster's is (ADR-0012): only under the person's background sign-in consent for exactly
//!   this definition, unattended, with no stdin, and killed if that consent ends. `helm` gets the
//!   token or certificate it printed, never the plugin. Without that consent, and for an auth
//!   provider, which can't be stopped once it runs, the run is refused.
//! - The arguments are a list, with no shell, and the environment is only what is set here: no
//!   `PATH`, and a home, Helm directories and working directory that exist for this run alone.
//!
//! Only commands that read are built here ([`HelmQuery`]). What `helm` prints on failure may quote
//! a cluster's data, so it is only classified and never shown or logged.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use kube::config::{Kubeconfig, NamedAuthInfo, NamedCluster, NamedContext};
use secrecy::SecretString;
use serde::{Deserialize, Deserializer};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::connection::policy::CredentialAccess;
use crate::connection::proxy::{self, ProxyEnvironment};
use crate::credential::{CredentialSource, WriteCredential};
use crate::keep_watching::{PluginConsent, SignIn, sign_in_of};
use crate::manager::KeptGrants;
use crate::read::{is_dns1123_label, is_dns1123_subdomain};
use crate::recheck::ContextPin;
use crate::redact;

mod acl;
mod mask;
mod run_dir;

use acl::Reach;
use mask::HiddenValues;
pub use mask::{MASK, mask_manifest, mask_values};
use run_dir::{RunDir, Trusted, admin_group, resolve_trusted};
pub(crate) use run_dir::{reap_stale, runs_root};

/// How long one `helm` command may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a running query checks that its connection still holds.
pub(crate) const RECHECK_EVERY: Duration = Duration::from_millis(250);
/// The most of `helm`'s output kept: a release's manifest is stored in a Secret, so about 1 MiB
/// compressed, which this leaves room for once expanded.
const STDOUT_CAP: usize = 32 * 1024 * 1024;
/// The most of `helm`'s error output read, which is only classified.
const STDERR_CAP: usize = 64 * 1024;
/// The most releases listed, and revisions of one release.
pub const MAX_ROWS: usize = 1000;
/// Helm's longest release name.
const MAX_RELEASE_NAME: usize = 53;
/// The most of a script's first line read, as much as the kernel does.
const MAX_SHEBANG: u64 = 512;

/// Why Helm data could not be shown. The text is safe to show: none of it comes from `helm`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HelmError {
    #[error(
        "helm was not found. Install it from https://helm.sh/docs/intro/install/, or set `helm` in settings.toml."
    )]
    NotInstalled,
    #[error("The helm set in settings.toml is not an executable file at an absolute path.")]
    BadConfiguredPath,
    #[error("helm is in a folder other accounts can change, so Flint did not run it.")]
    UntrustedLocation,
    #[error("Flint's folder for helm can be changed by other accounts, so helm did not run.")]
    UnsafeRunDirectory,
    #[error(
        "This cluster signs in with a command. To use Helm, turn on Keep Watching for it and let \
         Flint run its sign-in command in the background."
    )]
    CredentialRunsCommand,
    #[error("Helm can't use this cluster's SOCKS proxy yet, so it did not run.")]
    SocksProxy,
    #[error("This cluster's proxy can't be used, so Helm did not run.")]
    ProxyRefused,
    #[error(
        "Helm is not available for clusters that sign in with an auth provider, or with a command \
         beside another credential."
    )]
    SignInNotSupported,
    #[error("This cluster's sign-in command failed, so helm did not run.")]
    SignInFailed,
    #[error("This cluster is not connected.")]
    NotConnected,
    #[error("This cluster's kubeconfig changed. Reopen Helm releases.")]
    Changed,
    #[error("This cluster's kubeconfig can't be confirmed right now, so Helm did not run.")]
    Unsettled,
    #[error("A certificate or key file of this cluster changed. Reconnect to use Helm.")]
    CredentialFileChanged,
    #[error("This cluster's token file path is not absolute, so Helm did not run.")]
    RelativeTokenFile,
    #[error("That is not a valid release name or namespace.")]
    InvalidRelease,
    #[error("The release was not found.")]
    NotFound,
    #[error("This cluster's credentials may not read Helm releases.")]
    Forbidden,
    #[error("helm could not reach the cluster.")]
    Unreachable,
    #[error("helm did not answer in time.")]
    TimedOut,
    #[error("helm printed more than Flint will read.")]
    OutputTooLarge,
    #[error("helm's output could not be read. Check that it is Helm 3 or 4.")]
    BadOutput,
    #[error("helm could not be started.")]
    NotStarted,
    #[error("helm failed (exit code {0}).")]
    Failed(i32),
    #[error("The core is not running.")]
    Stopped,
}

/// The definition a Helm view opened on. Runs are refused once the context no longer has it,
/// even if a later edit puts the same values back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelmPin {
    pub(crate) pin: ContextPin,
    /// [`crate::recheck::ContextHistory::definition_revision`] when pinned.
    pub(crate) revision: u64,
}

/// One release, by a name and namespace safe to pass as arguments.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReleaseRef {
    name: String,
    namespace: String,
}

impl ReleaseRef {
    pub fn new(name: &str, namespace: &str) -> Result<Self, HelmError> {
        if name.len() > MAX_RELEASE_NAME
            || !is_dns1123_subdomain(name)
            || !is_dns1123_label(namespace)
        {
            return Err(HelmError::InvalidRelease);
        }
        Ok(Self {
            name: name.to_owned(),
            namespace: namespace.to_owned(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

/// What can be asked of `helm`. Every variant only reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelmQuery {
    List,
    Details(ReleaseRef),
    History(ReleaseRef),
    Values(ReleaseRef),
    Manifest(ReleaseRef),
}

impl HelmQuery {
    /// The arguments for this query, before the kubeconfig and context.
    fn args(&self) -> Vec<String> {
        let max = format!("--max={MAX_ROWS}");
        let on = |release: &ReleaseRef| {
            (
                release.name.clone(),
                format!("--namespace={}", release.namespace),
            )
        };
        let owned = |parts: &[&str]| parts.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        match self {
            HelmQuery::List => owned(&[
                "list",
                "--all-namespaces",
                // Helm 3 lists only deployed and failed releases unless asked: an install or
                // upgrade in progress, or one that died midway, would otherwise be missing.
                "--deployed",
                "--failed",
                "--pending",
                &max,
                "--output=json",
            ]),
            HelmQuery::Details(release) => {
                let (name, namespace) = on(release);
                owned(&["status", &name, &namespace, "--output=json"])
            }
            HelmQuery::History(release) => {
                let (name, namespace) = on(release);
                owned(&["history", &name, &namespace, &max, "--output=json"])
            }
            HelmQuery::Values(release) => {
                let (name, namespace) = on(release);
                owned(&["get", "values", &name, &namespace, "--output=json"])
            }
            HelmQuery::Manifest(release) => {
                let (name, namespace) = on(release);
                owned(&["get", "manifest", &name, &namespace])
            }
        }
    }
}

/// A release in the list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HelmRelease {
    pub name: String,
    pub namespace: String,
    #[serde(deserialize_with = "number_or_text")]
    pub revision: u64,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub chart: String,
}

/// One revision of a release.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HelmRevision {
    #[serde(deserialize_with = "number_or_text")]
    pub revision: u64,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub chart: String,
    #[serde(default)]
    pub description: String,
}

/// A release's details. Deliberately not its notes or stored values: charts often print
/// passwords in their notes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelmDetails {
    pub name: String,
    pub namespace: String,
    pub revision: u64,
    pub status: String,
    pub description: String,
    pub first_deployed: String,
    pub last_deployed: String,
    pub chart: String,
    pub chart_version: String,
    pub app_version: String,
}

/// What a query returned, with secrets already masked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelmOutput {
    Releases {
        releases: Arc<[HelmRelease]>,
        /// There may be more than [`MAX_ROWS`].
        truncated: bool,
    },
    Details(Arc<HelmDetails>),
    History(Arc<[HelmRevision]>),
    /// Values or a manifest, as YAML.
    Text(Arc<str>),
}

/// Accepts `3` or `"3"`: Helm prints a list's revision as text and a history's as a number.
fn number_or_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Number(u64),
        Text(String),
    }
    match Number::deserialize(deserializer)? {
        Number::Number(n) => Ok(n),
        Number::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
}

/// What an exec plugin runs under for `helm`, held as a kept cluster's plugin is (ADR-0012).
#[derive(Clone)]
pub(crate) struct PluginLaunch {
    /// The background sign-in consent, which held over exactly this definition when the target
    /// was made.
    pub(crate) consent: Arc<PluginConsent>,
    /// Asked right before the plugin starts, off the async threads: the kubeconfig files as they
    /// are now must still define what was agreed to, with the same program contents, and a
    /// confirmed change voids the consent ([`PluginConsent::verify`]).
    pub(crate) check: crate::cluster::SendCheck,
    /// The keep-watching grants: turning the sign-in off or quitting kills the plugin, and a quit
    /// waits for it.
    pub(crate) grants: KeptGrants,
}

/// Everything one `helm` run needs, taken in one step with checking the connection.
#[derive(Clone)]
pub(crate) struct HelmTarget {
    context: String,
    /// Only this context, its cluster and its user.
    kubeconfig: Kubeconfig,
    credential: WriteCredential,
    /// For an exec plugin: what it runs under. `None` once the plugin's output replaced it.
    plugin: Option<PluginLaunch>,
    configured_helm: Option<PathBuf>,
    /// Where the run's private directory goes ([`runs_root`]).
    runs_root: PathBuf,
    /// The proxy variables `helm` runs with. They may hold a password, so this never has `Debug`
    /// and is only ever handed to the child's environment.
    proxy: Vec<(&'static str, String)>,
}

impl HelmTarget {
    /// The target for `context` of `merged`, the definition `pin` was taken on. An exec plugin is
    /// accepted only with `plugin`'s consent holding over exactly this definition. Refuses an auth
    /// provider, and a user entry naming a program beside another credential: client-go's order
    /// is not kube's, so `helm` could still run it.
    pub(crate) fn new(
        merged: &Kubeconfig,
        context: &str,
        pin: ContextPin,
        credential: WriteCredential,
        plugin: Option<PluginLaunch>,
        configured_helm: Option<PathBuf>,
        runs_root: PathBuf,
    ) -> Result<Self, HelmError> {
        Self::new_in(
            merged,
            context,
            pin,
            credential,
            plugin,
            configured_helm,
            runs_root,
            &ProxyEnvironment::current(),
        )
    }

    /// [`Self::new`] under the proxy variables `environment` holds.
    #[allow(clippy::too_many_arguments)]
    fn new_in(
        merged: &Kubeconfig,
        context: &str,
        pin: ContextPin,
        credential: WriteCredential,
        plugin: Option<PluginLaunch>,
        configured_helm: Option<PathBuf>,
        runs_root: PathBuf,
        environment: &ProxyEnvironment,
    ) -> Result<Self, HelmError> {
        let named = merged
            .contexts
            .iter()
            .find(|c| c.name == context)
            .ok_or(HelmError::Changed)?;
        let entry = named.context.as_ref().ok_or(HelmError::Changed)?;
        let cluster = merged
            .clusters
            .iter()
            .find(|c| c.name == entry.cluster)
            .ok_or(HelmError::Changed)?;
        let server = cluster.cluster.as_ref().and_then(|c| c.server.as_deref());
        if server.is_none() || server != pin.server() {
            return Err(HelmError::Changed);
        }
        let user = match &entry.user {
            Some(name) => Some(
                merged
                    .auth_infos
                    .iter()
                    .find(|u| &u.name == name)
                    .ok_or(HelmError::Changed)?,
            ),
            None => None,
        };
        let auth = user.and_then(|u| u.auth_info.as_ref());
        let runs_plugin = auth.map(CredentialSource::of) == Some(CredentialSource::Plugin);
        if runs_plugin != credential.runs_a_command()
            || (!runs_plugin && auth.is_some_and(|a| a.exec.is_some() || a.auth_provider.is_some()))
        {
            return Err(HelmError::SignInNotSupported);
        }
        // The proxy Flint's own connection takes, so `helm` reaches the same server the same way.
        let mut proxy = Vec::new();
        if let Some(chosen) = proxy::chosen(
            cluster
                .cluster
                .as_ref()
                .and_then(|c| c.proxy_url.as_deref()),
            server,
            environment,
        ) {
            // Only a proxy Flint itself would connect through, and never a SOCKS one.
            let target = proxy::ProxyTarget::parse(chosen).map_err(|_| HelmError::ProxyRefused)?;
            if target.is_socks() {
                return Err(HelmError::SocksProxy);
            }
            proxy.push(("HTTPS_PROXY", chosen.to_owned()));
        }
        if let Some(no_proxy) = &environment.no_proxy {
            proxy.push(("NO_PROXY", no_proxy.clone()));
        }
        let kubeconfig = Kubeconfig {
            api_version: Some("v1".to_owned()),
            kind: Some("Config".to_owned()),
            clusters: vec![cluster.clone()],
            auth_infos: user.cloned().into_iter().collect(),
            contexts: vec![NamedContext::clone(named)],
            current_context: Some(context.to_owned()),
            ..Kubeconfig::default()
        };
        let plugin = if runs_plugin {
            let launch = plugin.ok_or(HelmError::CredentialRunsCommand)?;
            match sign_in_of(&kubeconfig, context) {
                SignIn::Plugin(definition) if launch.consent.holds(&definition) => Some(launch),
                _ => return Err(HelmError::CredentialRunsCommand),
            }
        } else {
            None
        };
        Ok(Self {
            context: context.to_owned(),
            kubeconfig,
            credential,
            plugin,
            configured_helm,
            runs_root,
            proxy,
        })
    }

    /// The kubeconfig `helm` gets, with certificate and key files replaced by the bytes the
    /// connection was built from. Blocks on the file system. Refused while an exec plugin or auth
    /// provider is still in it: `helm` would run it.
    fn pinned_kubeconfig(&self) -> Result<Vec<u8>, HelmError> {
        let config = self.pinned_config()?;
        let names_program = config
            .auth_infos
            .iter()
            .filter_map(|user| user.auth_info.as_ref())
            .any(|auth| auth.exec.is_some() || auth.auth_provider.is_some());
        if names_program {
            return Err(HelmError::SignInNotSupported);
        }
        serde_json::to_vec(&config).map_err(|_| HelmError::NotStarted)
    }

    /// [`Self::kubeconfig`] with its files replaced by the bytes the connection was built from.
    fn pinned_config(&self) -> Result<Kubeconfig, HelmError> {
        let mut config = self.kubeconfig.clone();
        for cluster in &mut config.clusters {
            self.inline_authority(cluster)?;
        }
        for user in &mut config.auth_infos {
            self.inline_identity(user)?;
        }
        Ok(config)
    }

    /// This target with its exec plugin's credential in place of the plugin, so `helm` starts no
    /// program. The plugin runs as a kept cluster's does: unattended, with no stdin, and only after
    /// [`PluginLaunch::check`] passes right before it starts. It is counted and held with the
    /// keep-watching grants, so turning the sign-in off or quitting kills it, and dropping the
    /// future does too. A target with no plugin is returned as it is.
    async fn signed_in(&self) -> Result<Self, HelmError> {
        let Some(PluginLaunch {
            consent,
            check,
            grants,
        }) = self.plugin.clone()
        else {
            return Ok(self.clone());
        };
        let SignIn::Plugin(definition) = sign_in_of(&self.kubeconfig, &self.context) else {
            return Err(HelmError::Changed);
        };
        let pinned = {
            let target = self.clone();
            tokio::task::spawn_blocking(move || target.pinned_config())
                .await
                .map_err(|_| HelmError::NotStarted)??
        };
        let access = CredentialAccess::consented(Arc::new(|| {}), Some(check), grants.runs());
        let _held = grants.hold_access(&self.context, &access);
        // Why it failed: the consent ended, or the plugin did.
        let failed = || {
            if consent.holds(&definition) {
                HelmError::SignInFailed
            } else {
                HelmError::CredentialRunsCommand
            }
        };
        let prepared = tokio::time::timeout(
            RUN_TIMEOUT,
            crate::cluster::prepare(&pinned, &self.context, &access),
        )
        .await
        .map_err(|_| HelmError::TimedOut)?
        .map_err(|_| failed())?;
        let token = match &prepared.token {
            Some(refreshing) => {
                let header = refreshing.header().await.map_err(|_| failed())?;
                let token = header
                    .to_str()
                    .ok()
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .filter(|token| !token.is_empty())
                    .ok_or(HelmError::SignInFailed)?;
                Some(SecretString::from(token.to_owned()))
            }
            None => prepared.config.auth_info.token.clone(),
        };
        if !consent.holds(&definition) {
            return Err(HelmError::CredentialRunsCommand);
        }
        let mut signed = self.clone();
        let auth = signed
            .kubeconfig
            .auth_infos
            .iter_mut()
            .find_map(|user| user.auth_info.as_mut())
            .ok_or(HelmError::Changed)?;
        auth.exec = None;
        auth.token = token;
        if prepared.replaced_certificate {
            let resolved = &prepared.config.auth_info;
            auth.client_certificate = None;
            auth.client_key = None;
            auth.client_certificate_data = resolved.client_certificate_data.clone();
            auth.client_key_data = resolved.client_key_data.clone();
        }
        signed.plugin = None;
        Ok(signed)
    }

    fn held(&self, path: &str) -> Result<String, HelmError> {
        let bytes = self
            .credential
            .held_file_bytes(Path::new(path))
            .ok_or(HelmError::CredentialFileChanged)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    fn inline_authority(&self, cluster: &mut NamedCluster) -> Result<(), HelmError> {
        let Some(entry) = cluster.cluster.as_mut() else {
            return Ok(());
        };
        if let Some(path) = entry.certificate_authority.take()
            && entry.certificate_authority_data.is_none()
        {
            entry.certificate_authority_data = Some(self.held(&path)?);
        }
        Ok(())
    }

    fn inline_identity(&self, user: &mut NamedAuthInfo) -> Result<(), HelmError> {
        let Some(auth) = user.auth_info.as_mut() else {
            return Ok(());
        };
        if let Some(path) = auth.client_certificate.take()
            && auth.client_certificate_data.is_none()
        {
            auth.client_certificate_data = Some(self.held(&path)?);
        }
        if let Some(path) = auth.client_key.take()
            && auth.client_key_data.is_none()
        {
            auth.client_key_data = Some(SecretString::from(self.held(&path)?));
        }
        // kube and client-go order these differently, so only the one kube uses goes in.
        let basic = auth.username.is_some() && auth.password.is_some();
        let token = if basic {
            None
        } else if auth.token.is_some() {
            auth.token.take()
        } else if let CredentialSource::TokenFile(path) = CredentialSource::of(auth) {
            Some(self.token_from_file(&path)?)
        } else {
            None
        };
        if !basic {
            auth.username = None;
            auth.password = None;
        }
        auth.token = token;
        auth.token_file = None;
        Ok(())
    }

    /// The token the connection was admitted with. The file is not read: it may hold another
    /// identity by now.
    fn token_from_file(&self, path: &Path) -> Result<SecretString, HelmError> {
        if !path.is_absolute() {
            return Err(HelmError::RelativeTokenFile);
        }
        self.credential
            .admitted_token(path)
            .ok_or(HelmError::CredentialFileChanged)
    }
}

/// Whether `path` is a file someone may execute.
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// What follows `#!` on the first line of `path`, if it is a script.
fn shebang(path: &Path) -> std::io::Result<Option<String>> {
    use std::io::Read as _;
    let mut start = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_SHEBANG)
        .read_to_end(&mut start)?;
    let Some(line) = start.strip_prefix(b"#!") else {
        return Ok(None);
    };
    let line = line.split(|byte| *byte == b'\n').next().unwrap_or_default();
    Ok(Some(String::from_utf8_lossy(line).into_owned()))
}

/// Whether what runs `path` is `path` itself, or an interpreter only this account and root can
/// change. `env` is refused: what it starts is chosen when it runs.
fn interpreter_is_safe(path: &Path) -> bool {
    let line = match shebang(path) {
        Ok(None) => return true,
        Ok(Some(line)) => line,
        Err(_) => return false,
    };
    line.split_whitespace()
        .next()
        .and_then(|interpreter| resolve_trusted(Path::new(interpreter), None))
        .is_some_and(|real| {
            is_executable(&real.path)
                && real.path.file_name() != Some(OsStr::new("env"))
                && matches!(shebang(&real.path), Ok(None))
        })
}

/// The real file to run for `candidate`, if only this account, root and the `admin` group can
/// change it or what leads to it, and a script's interpreter is safe too.
fn trusted_helm(candidate: &Path, admin: Option<u32>) -> Result<Trusted, HelmError> {
    resolve_trusted(candidate, admin)
        .filter(|real| is_executable(&real.path) && interpreter_is_safe(&real.path))
        .ok_or(HelmError::UntrustedLocation)
}

/// The `helm` to run: the one set in the settings, or the first on `path`. Only absolute paths
/// count, so that the working directory never decides what runs. A first match in an unsafe
/// place is refused rather than skipped, so what runs is never a surprise.
fn locate_helm(
    configured: Option<&Path>,
    path: Option<&OsStr>,
    admin: Option<u32>,
) -> Result<Trusted, HelmError> {
    if let Some(configured) = configured {
        if !configured.is_absolute() || !is_executable(configured) {
            return Err(HelmError::BadConfiguredPath);
        }
        return trusted_helm(configured, admin);
    }
    let found = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("helm"))
        .find(|candidate| is_executable(candidate))
        .ok_or(HelmError::NotInstalled)?;
    trusted_helm(&found, admin)
}

/// What a Helm view says of a `helm` other administrators could replace.
fn admin_writable_warning(path: &Path) -> String {
    format!(
        "Other administrators on this Mac could replace {}, and it would receive this cluster's \
         credentials.",
        path.display()
    )
}

/// The warning for the `helm` that `target` would run now, if it needs one.
pub(crate) async fn warning(target: &HelmTarget) -> Option<String> {
    let configured = target.configured_helm.clone();
    let path = std::env::var_os("PATH");
    let found = tokio::task::spawn_blocking(move || {
        locate_helm(configured.as_deref(), path.as_deref(), admin_group())
    })
    .await
    .ok()?
    .ok()?;
    found
        .admin_writable
        .then(|| admin_writable_warning(&found.path))
}

/// Kills the process group of a child not reaped yet, so what `helm` started ends with it.
struct GroupGuard(Option<libc::pid_t>);

impl GroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            // SAFETY: only a call with a group id. The leader is unreaped, so the id is its own.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}

/// Reads `reader` to its end, failing past `cap` bytes.
async fn read_capped(
    reader: &mut (impl AsyncRead + Unpin),
    cap: usize,
) -> Result<Vec<u8>, HelmError> {
    let mut out = Vec::new();
    let read = reader
        .take(cap as u64 + 1)
        .read_to_end(&mut out)
        .await
        .map_err(|_| HelmError::NotStarted)?;
    if read > cap {
        return Err(HelmError::OutputTooLarge);
    }
    Ok(out)
}

/// A failed run's error output, as one of the errors Flint words itself.
fn classify_failure(code: i32, stderr: &[u8]) -> HelmError {
    let text = String::from_utf8_lossy(stderr).to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if has(&["forbidden", "unauthorized"]) {
        HelmError::Forbidden
    } else if has(&["release: not found", "release not found", "not found"]) {
        HelmError::NotFound
    } else if has(&[
        "connection refused",
        "no such host",
        "i/o timeout",
        "unreachable",
        "tls handshake",
        "kubernetes cluster unreachable",
    ]) {
        HelmError::Unreachable
    } else {
        HelmError::Failed(code)
    }
}

/// Writes the run's private kubeconfig. The directory is the owner's alone, and the file is
/// created new, so nothing else can have opened it.
fn write_private(dir: &Path, bytes: &[u8]) -> Result<PathBuf, HelmError> {
    use std::io::Write as _;
    let path = dir.join("kubeconfig");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|_| HelmError::NotStarted)?;
    if acl::lets_others(&path, Reach::Any) {
        return Err(HelmError::UnsafeRunDirectory);
    }
    file.write_all(bytes).map_err(|_| HelmError::NotStarted)?;
    Ok(path)
}

/// The whole environment of a `helm` run, under the run's private directory `dir`.
fn run_environment(
    dir: &Path,
    kubeconfig: &Path,
    proxy: &[(&'static str, String)],
) -> Vec<(&'static str, OsString)> {
    let under = |name: &str| dir.join(name).into_os_string();
    let proxy = proxy.iter().map(|(name, value)| (*name, value.into()));
    let mut environment = vec![
        ("KUBECONFIG", kubeconfig.as_os_str().to_owned()),
        ("HOME", dir.as_os_str().to_owned()),
        ("HELM_CACHE_HOME", under("cache")),
        ("HELM_CONFIG_HOME", under("config")),
        ("HELM_DATA_HOME", under("data")),
        ("HELM_DRIVER", "secret".into()),
    ];
    environment.extend(proxy);
    environment
}

/// `query`'s arguments, for exactly `revision` of its release.
fn at_revision(query: &HelmQuery, revision: u64) -> Vec<String> {
    let mut args = query.args();
    args.push(format!("--revision={revision}"));
    args
}

/// The revision `helm status` is about.
fn status_revision(out: &[u8]) -> Result<u64, HelmError> {
    let status: StatusOutput = serde_json::from_slice(out).map_err(|_| HelmError::BadOutput)?;
    match status.version {
        0 => Err(HelmError::BadOutput),
        revision => Ok(revision),
    }
}

/// Runs `query` with the `helm` at `binary`, held to `target`. Details and a manifest are read
/// with the values of the same revision, so that what Values hides is hidden in them too.
async fn run_with(
    binary: &Path,
    target: &HelmTarget,
    query: &HelmQuery,
    admin: Option<u32>,
) -> Result<HelmOutput, HelmError> {
    let off_thread = async |out: Vec<u8>, hidden: HiddenValues| {
        let query = query.clone();
        tokio::task::spawn_blocking(move || parse(&query, &out, &hidden))
            .await
            .map_err(|_| HelmError::BadOutput)?
    };
    let (HelmQuery::Details(release) | HelmQuery::Manifest(release)) = query else {
        let out = run_helm(binary, target, query.args(), admin).await?;
        return off_thread(out, HiddenValues::default()).await;
    };
    let status_args = HelmQuery::Details(release.clone()).args();
    let status = run_helm(binary, target, status_args, admin).await?;
    let (status, revision) = tokio::task::spawn_blocking(move || {
        let revision = status_revision(&status)?;
        Ok::<_, HelmError>((status, revision))
    })
    .await
    .map_err(|_| HelmError::BadOutput)??;
    let values_args = at_revision(&HelmQuery::Values(release.clone()), revision);
    let values = run_helm(binary, target, values_args, admin).await?;
    let hidden = tokio::task::spawn_blocking(move || HiddenValues::from_output(&values))
        .await
        .map_err(|_| HelmError::BadOutput)??;
    let out = match query {
        HelmQuery::Manifest(_) => {
            run_helm(binary, target, at_revision(query, revision), admin).await?
        }
        _ => status,
    };
    off_thread(out, hidden).await
}

/// One `helm` command, to its output. The binary is checked again here, last before it starts
/// and before any credential is written for it.
async fn run_helm(
    binary: &Path,
    target: &HelmTarget,
    args: Vec<String>,
    admin: Option<u32>,
) -> Result<Vec<u8>, HelmError> {
    let proxy = target.proxy.clone();
    let (binary, dir, kubeconfig) = {
        let target = target.clone();
        let binary = binary.to_owned();
        tokio::task::spawn_blocking(move || {
            let binary = trusted_helm(&binary, admin)?.path;
            let bytes = target.pinned_kubeconfig()?;
            // Private to this user, and removed when the run ends, however it ends.
            let dir = RunDir::create(&target.runs_root)?;
            let path = write_private(dir.path(), &bytes)?;
            Ok::<_, HelmError>((binary, dir, path))
        })
        .await
        .map_err(|_| HelmError::NotStarted)??
    };
    let mut command = tokio::process::Command::new(&binary);
    command
        .args(args)
        .arg(format!("--kube-context={}", target.context))
        .arg(format!("--kubeconfig={}", kubeconfig.display()))
        .env_clear()
        .envs(run_environment(dir.path(), &kubeconfig, &proxy))
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    dir.hand_lock_to(&mut command);
    let mut child = command.spawn().map_err(|_| HelmError::NotStarted)?;
    let mut group = GroupGuard(child.id().map(|id| id as libc::pid_t));
    let mut stdout = child.stdout.take().ok_or(HelmError::NotStarted)?;
    let mut stderr = child.stderr.take().ok_or(HelmError::NotStarted)?;
    let finished = async {
        // Both at once, or a child filling the pipe not being read would stall. The child is
        // reaped last, so the guard's group id stays its own until then.
        let (out, err) = tokio::try_join!(
            read_capped(&mut stdout, STDOUT_CAP),
            read_capped(&mut stderr, STDERR_CAP),
        )?;
        let status = child.wait().await.map_err(|_| HelmError::NotStarted)?;
        Ok::<_, HelmError>((out, err, status))
    };
    let (out, err, status) = tokio::time::timeout(RUN_TIMEOUT, finished)
        .await
        .map_err(|_| HelmError::TimedOut)??;
    group.disarm();
    drop(dir);
    if !status.success() {
        return Err(classify_failure(status.code().unwrap_or(-1), &err));
    }
    Ok(out)
}

/// Runs `query` on `target`, with the `helm` the settings name or the first on this process's
/// `PATH`.
/// An exec plugin is run first, once for all of the query's commands ([`HelmTarget::signed_in`]).
pub(crate) async fn run(target: &HelmTarget, query: &HelmQuery) -> Result<HelmOutput, HelmError> {
    let configured = target.configured_helm.clone();
    let path = std::env::var_os("PATH");
    let admin = admin_group();
    let found = tokio::task::spawn_blocking(move || {
        locate_helm(configured.as_deref(), path.as_deref(), admin)
    })
    .await
    .map_err(|_| HelmError::NotStarted)??;
    let target = target.signed_in().await?;
    run_with(&found.path, &target, query, admin).await
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatusInfo {
    status: String,
    description: String,
    first_deployed: String,
    last_deployed: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChartMetadata {
    name: String,
    version: String,
    #[serde(rename = "appVersion")]
    app_version: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatusChart {
    metadata: ChartMetadata,
}

/// Only what [`HelmDetails`] shows of `helm status`: the rest of it, notes and stored values
/// included, is never read into memory as fields.
#[derive(Deserialize)]
struct StatusOutput {
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    version: u64,
    #[serde(default)]
    info: StatusInfo,
    #[serde(default)]
    chart: StatusChart,
}

/// The most of a failure reason shown, after redaction.
const REASON_CHARS: usize = 300;
/// Follows a failure reason that redaction changed.
pub const REASON_MASKED: &str = "(some values masked)";

/// Stands for a failed revision's reason where that revision's values are not loaded.
pub const REASON_IN_DETAILS: &str = "Failed. Details show the reason for the current revision.";

/// A release status in words. A failed one gives Helm's stored reason ([`failure_reason`]) only
/// with its revision's values (`hidden`) to scrub against, and [`REASON_IN_DETAILS`] without
/// them; any other status gets fixed words.
fn status_text(status: &str, description: &str, hidden: Option<&HiddenValues>) -> String {
    let fixed = match status {
        "deployed" => "Deployed.",
        "superseded" => "Replaced by a later revision.",
        "failed" => {
            return hidden.map_or_else(
                || REASON_IN_DETAILS.to_owned(),
                |hidden| failure_reason(description, hidden),
            );
        }
        "uninstalled" => "Uninstalled.",
        "uninstalling" => "Uninstalling.",
        "pending-install" => "Install in progress.",
        "pending-upgrade" => "Upgrade in progress.",
        "pending-rollback" => "Rollback in progress.",
        _ => "No description.",
    };
    fixed.to_owned()
}

/// Helm's stored reason for a failed release, on one line and held to the rules for a server's
/// error text ([`redact::hide_server_values`]): credentials, bearer values and every quoted value
/// are hidden, then the revision's own values (`hidden`), and only then is the line
/// cut. A reason redaction changed ends with [`REASON_MASKED`].
fn failure_reason(description: &str, hidden: &HiddenValues) -> String {
    let shown = hidden.scrub_text(&redact::hide_server_values(description));
    let line = redact::first_line(&shown, REASON_CHARS);
    if line.is_empty() {
        "Failed. Helm stored no reason.".to_owned()
    } else if shown == description {
        format!("Failed: {line}")
    } else {
        format!("Failed: {line} {REASON_MASKED}")
    }
}

/// `query`'s output as data, masked. `hidden` is what a manifest or details must not show. A list
/// or history spans revisions with no one set of values to scrub against, so neither keeps the
/// free-form app version, and history shows no failure reason.
fn parse(query: &HelmQuery, out: &[u8], hidden: &HiddenValues) -> Result<HelmOutput, HelmError> {
    fn json<'a, T: Deserialize<'a>>(out: &'a [u8]) -> Result<T, HelmError> {
        serde_json::from_slice(out).map_err(|_| HelmError::BadOutput)
    }
    match query {
        HelmQuery::List => {
            // An empty list prints `[]`, and some versions print nothing.
            let releases: Vec<HelmRelease> = if out.iter().all(u8::is_ascii_whitespace) {
                Vec::new()
            } else {
                json(out)?
            };
            Ok(HelmOutput::Releases {
                truncated: releases.len() >= MAX_ROWS,
                releases: releases.into(),
            })
        }
        HelmQuery::Details(release) => {
            let status: StatusOutput = json(out)?;
            Ok(HelmOutput::Details(Arc::new(HelmDetails {
                name: status.name,
                namespace: if status.namespace.is_empty() {
                    release.namespace.clone()
                } else {
                    status.namespace
                },
                revision: status.version,
                description: status_text(
                    &status.info.status,
                    &status.info.description,
                    Some(hidden),
                ),
                status: status.info.status,
                first_deployed: status.info.first_deployed,
                last_deployed: status.info.last_deployed,
                chart: hidden.scrub_text(&status.chart.metadata.name),
                chart_version: hidden.scrub_text(&status.chart.metadata.version),
                app_version: hidden.scrub_text(&status.chart.metadata.app_version),
            })))
        }
        HelmQuery::History(_) => {
            let mut revisions: Vec<HelmRevision> = json(out)?;
            for revision in &mut revisions {
                revision.description = status_text(&revision.status, &revision.description, None);
            }
            // Newest first.
            revisions.sort_by_key(|r| std::cmp::Reverse(r.revision));
            Ok(HelmOutput::History(revisions.into()))
        }
        HelmQuery::Values(_) => {
            let values: serde_json::Value = json(out)?;
            Ok(HelmOutput::Text(mask_values(values)?.into()))
        }
        HelmQuery::Manifest(_) => {
            let text = std::str::from_utf8(out).map_err(|_| HelmError::BadOutput)?;
            Ok(HelmOutput::Text(hidden.mask_manifest(text).into()))
        }
    }
}

#[cfg(test)]
#[path = "helm_tests.rs"]
mod tests;
