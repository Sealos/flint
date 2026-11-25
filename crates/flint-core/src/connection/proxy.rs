//! The proxy a connection goes through, if any.
//!
//! A cluster entry's `proxy-url` always applies. Without one, the `[proxy]` of `settings.toml`
//! does ([`use_setting`]), else `HTTPS_PROXY` (or `https_proxy`), unless the no-proxy list that
//! goes with it exempts the API server. `http://` proxies are used through `CONNECT`, and
//! `socks5://` (names resolved here) and `socks5h://` (names resolved by the proxy) through a
//! SOCKS5 tunnel. Either way TLS to the API server runs inside the tunnel, so the proxy sees
//! neither the credentials nor the requests. Any other proxy is refused, never skipped, and so is
//! every route that would take the setting while `settings.toml` could not be read at startup.
//!
//! A proxy URL can hold a password. Only [`shown`], and [`route_identity`] with its one-way
//! digest, leave this module as text.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use http::{HeaderValue, Uri};
use hyper_util::client::legacy::connect::proxy::{SocksV5, Tunnel};
use hyper_util::client::proxy::matcher::Matcher;
use ring::hmac;
use ring::rand::SystemRandom;

use crate::settings::ProxySetting;

/// What [`shown`] says of a proxy URL it can't read, which may be a mistyped secret.
const UNREADABLE: &str = "an unreadable proxy URL";

/// Stands in for a cluster's proxy while kube loads its config, so kube neither parses the real
/// one nor an environment proxy that `NO_PROXY` may exempt. Flint then sets the proxy it chose;
/// were that ever skipped, this address leads nowhere.
pub(crate) const LOADER_PLACEHOLDER: &str = "http://proxy.invalid:1";

/// Stands in for the proxy of a route that would take the setting while `settings.toml` could
/// not be read: every use of it is refused ([`ProxyRefused::SettingsUnreadable`]).
const SETTINGS_UNREADABLE: &str = "flint-settings-unreadable://settings.toml";

/// What [`shown`] says of [`SETTINGS_UNREADABLE`].
const SETTINGS_UNREADABLE_SHOWN: &str = "the proxy setting of an unreadable settings.toml";

/// The port of a SOCKS proxy URL that names none, as curl and Go use.
const SOCKS_DEFAULT_PORT: u16 = 1080;

/// The proxy setting this process uses.
#[derive(Clone)]
enum InUse {
    /// The `[proxy]` of a `settings.toml` that was read, or the defaults when there was none.
    Setting(ProxySetting),
    /// `settings.toml` exists but could not be read, so the setting is unknown.
    Unreadable,
}

/// The proxy setting this process uses: from `settings.toml` at startup, then whatever
/// [`replace_setting`] put in its place, with the number of times it was replaced. Kubeconfig
/// loads record that number ([`generation`]), so a context loaded under one setting is never
/// connected under another: the manager holds it until a load under the new one replaces it.
static SETTING: RwLock<Option<(u64, InUse)>> = RwLock::new(None);

fn setting_in_use() -> std::sync::RwLockReadGuard<'static, Option<(u64, InUse)>> {
    SETTING.read().unwrap_or_else(|e| e.into_inner())
}

/// Uses `setting` for cluster connections from now on. The first call wins; later ones are
/// ignored, since contexts loaded under the first are pinned to its route. A change made while
/// Flint runs goes through [`replace_setting`].
pub(crate) fn use_setting(setting: &ProxySetting) {
    use_in_process(InUse::Setting(setting.clone()));
}

/// Refuses, from now on, every cluster connection that would take the proxy setting: those
/// without a `proxy-url` of their own. For a `settings.toml` that exists but can't be read, whose
/// proxy may have been meant for every cluster. The first call wins, as for [`use_setting`].
pub(crate) fn use_unreadable_setting() {
    use_in_process(InUse::Unreadable);
}

fn use_in_process(setting: InUse) {
    let mut slot = SETTING.write().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        tracing::debug!("a proxy setting is already in use; the new one applies after a restart");
        return;
    }
    *slot = Some((0, setting));
}

/// Why [`replace_setting`] did not replace the setting. Its text holds no credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ReplaceRefused {
    #[error(
        "settings.toml could not be read when Flint started, so a new proxy setting applies \
         only after Flint restarts"
    )]
    UnreadableAtStartup,
}

/// Puts `setting` in place of the one in use, for connections made from now on, and returns the
/// new [`generation`]. Contexts loaded under the old one keep their route until a kubeconfig load
/// under the new one replaces them. Refused while `settings.toml` was unreadable at startup: the
/// proxy every cluster would have taken then stays unknown until a restart.
pub(crate) fn replace_setting(setting: &ProxySetting) -> Result<u64, ReplaceRefused> {
    let mut slot = SETTING.write().unwrap_or_else(|e| e.into_inner());
    let next = match &*slot {
        Some((_, InUse::Unreadable)) => return Err(ReplaceRefused::UnreadableAtStartup),
        Some((generation, InUse::Setting(_))) => generation + 1,
        None => 1,
    };
    *slot = Some((next, InUse::Setting(setting.clone())));
    Ok(next)
}

/// How many times the setting in use was replaced while Flint runs ([`replace_setting`]).
pub(crate) fn generation() -> u64 {
    setting_in_use()
        .as_ref()
        .map_or(0, |(generation, _)| *generation)
}

/// `reason`, why `settings.toml` can't be read, saying what that means for the proxy when the
/// file was unreadable at startup. Holds nothing of the file.
pub(crate) fn settings_error_with_proxy_note(reason: String) -> String {
    with_proxy_note(reason, setting_unreadable_at_startup())
}

/// Whether `settings.toml` could not be read at startup, so its proxy setting is unknown to this
/// process ([`use_unreadable_setting`]) however the file reads now.
pub(crate) fn setting_unreadable_at_startup() -> bool {
    matches!(&*setting_in_use(), Some((_, InUse::Unreadable)))
}

/// What a `settings.toml` that reads again says while [`setting_unreadable_at_startup`]: its
/// proxy setting applies only after a restart, so it still counts as not applied.
pub(crate) const RESTART_TO_APPLY: &str = "settings.toml can be read again, but its proxy \
     setting applies only after Flint restarts. Until then clusters without a proxy-url of their \
     own do not connect, and settings.toml is treated as unreadable: nothing is trusted, exposed \
     to AI or watched in the background";

/// `reason`, with the proxy note when the setting in use is `unreadable`. The note follows the
/// reason's first line: a TOML parse error goes on with lines that quote the file, and what is
/// shown of an error is its first line.
pub(crate) fn with_proxy_note(reason: String, unreadable: bool) -> String {
    if !unreadable {
        return reason;
    }
    let reason = reason
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    format!(
        "{reason}. Its proxy setting could not be applied, so clusters without a proxy-url of \
         their own do not connect until settings.toml can be read and Flint restarts"
    )
}

/// The proxy variables of an environment. Holds what may be a password, so it has no `Debug`.
#[derive(Clone, Default)]
pub(crate) struct ProxyEnvironment {
    pub(crate) https_proxy: Option<String>,
    pub(crate) no_proxy: Option<String>,
}

impl ProxyEnvironment {
    /// The proxy of this process: the variables of its environment under the setting it uses
    /// ([`use_setting`]).
    pub(crate) fn current() -> Self {
        Self::current_with_generation().1
    }

    /// [`Self::current`], with the [`generation`] of the setting it was read under, both read in
    /// one step.
    pub(crate) fn current_with_generation() -> (u64, Self) {
        let in_use = setting_in_use();
        let environment = match &*in_use {
            Some((_, InUse::Setting(setting))) => Self::of_process().under(Some(setting)),
            None => Self::of_process(),
            Some((_, InUse::Unreadable)) => Self::unreadable_setting(),
        };
        (in_use.as_ref().map_or(0, |(g, _)| *g), environment)
    }

    /// The environment while `settings.toml` can't be read: every route without a `proxy-url`
    /// of its own takes a proxy that is always refused. Nothing is exempt, since the setting's
    /// no-proxy list is unknown too.
    fn unreadable_setting() -> Self {
        ProxyEnvironment {
            https_proxy: Some(SETTINGS_UNREADABLE.to_owned()),
            no_proxy: None,
        }
    }

    /// The variables of this process, in the order kube and kubectl read them.
    ///
    /// A value that is not UTF-8 stays set, lossily: as a proxy it then can't be read and is
    /// refused, and as `NO_PROXY` it exempts nothing. It never counts as unset.
    fn of_process() -> Self {
        let first = |names: [&str; 2]| {
            names.iter().find_map(|name| {
                let value = std::env::var_os(name)?;
                let value = value.to_string_lossy();
                (!value.trim().is_empty()).then(|| value.into_owned())
            })
        };
        ProxyEnvironment {
            https_proxy: first(["HTTPS_PROXY", "https_proxy"]),
            no_proxy: first(["NO_PROXY", "no_proxy"]),
        }
    }

    /// This environment with `setting` over it. A setting's URL replaces the environment's proxy
    /// together with its `NO_PROXY`: only the setting's own list exempts servers from it. A
    /// setting with a no-proxy list and no URL replaces only `NO_PROXY`.
    pub(crate) fn under(self, setting: Option<&ProxySetting>) -> Self {
        let Some(setting) = setting else {
            return self;
        };
        let no_proxy = setting.no_proxy_list();
        match setting.url.as_deref().filter(|u| !u.trim().is_empty()) {
            Some(url) => ProxyEnvironment {
                https_proxy: Some(url.to_owned()),
                no_proxy,
            },
            None => ProxyEnvironment {
                no_proxy: no_proxy.or(self.no_proxy),
                ..self
            },
        }
    }

    /// Whether `NO_PROXY` exempts `server`. A server it can't read is not exempt.
    fn exempts(&self, server: &str) -> bool {
        let Some(no_proxy) = &self.no_proxy else {
            return false;
        };
        let Ok(uri) = server.parse::<Uri>() else {
            return false;
        };
        let default_port = match uri.scheme_str() {
            Some("https") => 443,
            Some("http") => 80,
            _ => return false,
        };
        let Some(server_host) = uri.host().filter(|_| authority_is_valid(&uri)) else {
            return false;
        };
        let server_host = server_host.to_ascii_lowercase();
        let port = uri.port_u16().unwrap_or(default_port);
        let mut hosts = Vec::new();
        for entry in no_proxy.split(',').map(str::trim) {
            if entry == "*" {
                return true;
            }
            let (host, only_port) = no_proxy_entry(entry);
            if only_port.is_some_and(|only| only != port) {
                continue;
            }
            match host.strip_prefix("*.") {
                Some(suffix) => {
                    let suffix = format!(".{}", suffix.to_ascii_lowercase());
                    if server_host.len() > suffix.len() && server_host.ends_with(&suffix) {
                        return true;
                    }
                }
                None => hosts.push(host),
            }
        }
        Matcher::builder()
            .all(LOADER_PLACEHOLDER.to_owned())
            .no(hosts.join(","))
            .build()
            .intercept(&uri)
            .is_none()
    }
}

/// Whether `uri`'s host and port are exactly a DNS name or IP address and a port. `http::Uri`
/// accepts more (`[::1]junk:3128`, port `65536`) and then dials something else than was written.
fn authority_is_valid(uri: &Uri) -> bool {
    let Some(authority) = uri.authority() else {
        return false;
    };
    let host_port = authority.as_str().rsplit('@').next().unwrap_or_default();
    let port = if let Some(rest) = host_port.strip_prefix('[') {
        let Some((address, after)) = rest.split_once(']') else {
            return false;
        };
        if address.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        match after.strip_prefix(':') {
            Some(port) => Some(port),
            None if after.is_empty() => None,
            None => return false,
        }
    } else {
        let (host, port) = match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        };
        let name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
        if host.is_empty() || !host.chars().all(name_char) {
            return false;
        }
        port
    };
    port.is_none_or(|p| p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok())
}

/// A `NO_PROXY` entry's host, and the one port it is limited to. An entry whose port can't be
/// read stays whole, so it matches no host rather than every port.
fn no_proxy_entry(entry: &str) -> (&str, Option<u16>) {
    if let Some((host, after)) = entry.strip_prefix('[').and_then(|e| e.split_once(']')) {
        return match after.strip_prefix(':').map(str::parse) {
            None if after.is_empty() => (host, None),
            Some(Ok(port)) => (host, Some(port)),
            _ => (entry, None),
        };
    }
    match entry.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (host, Some(port)),
            Err(_) => (entry, None),
        },
        _ => (entry, None),
    }
}

/// The proxy URL that applies to a cluster entry with `entry` as its `proxy-url`, at `server`.
pub(crate) fn chosen<'a>(
    entry: Option<&'a str>,
    server: Option<&str>,
    environment: &'a ProxyEnvironment,
) -> Option<&'a str> {
    if let Some(entry) = entry.filter(|e| !e.trim().is_empty()) {
        return Some(entry);
    }
    let proxy = environment.https_proxy.as_deref()?;
    (!server.is_some_and(|s| environment.exempts(s))).then_some(proxy)
}

/// `proxy` as it may be shown, logged and saved: scheme, host and port, with no credentials.
pub(crate) fn shown(proxy: &str) -> String {
    if proxy.trim() == SETTINGS_UNREADABLE {
        return SETTINGS_UNREADABLE_SHOWN.to_owned();
    }
    // The same reading of the authority as a server URL: a bad port or a stray `@` means what
    // parsed as the host may be part of a credential.
    if crate::redact::plain_server_url(proxy) == crate::redact::HIDDEN_SERVER_URL {
        return UNREADABLE.to_owned();
    }
    let Ok(uri) = proxy.trim().parse::<Uri>() else {
        return UNREADABLE.to_owned();
    };
    let (Some(scheme), Some(host)) = (uri.scheme_str(), uri.host()) else {
        return UNREADABLE.to_owned();
    };
    if !authority_is_valid(&uri) {
        return UNREADABLE.to_owned();
    }
    let (scheme, host) = (scheme.to_ascii_lowercase(), host.to_ascii_lowercase());
    match uri.port_u16() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

/// The private key of account digests, in the config directory.
const ROUTE_KEY_FILE: &str = "proxy-route.key";

/// Marks an identity made without [`ROUTE_KEY_FILE`]: it holds for this process only.
const UNKEYED: &str = "#unkeyed-";

/// The config directory whose route key this process uses. Both the app and `flint mcp` set it
/// at startup, so a grant made in one holds in the other.
static ROUTE_KEY_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Uses the route key in `config_dir` from now on. The first call wins.
pub(crate) fn use_route_key_in(config_dir: &Path) {
    let _ = ROUTE_KEY_DIR.set(config_dir.to_owned());
}

/// The key in `config_dir`, made if it is missing. None if it can't be read or made.
fn open_route_key(config_dir: &Path) -> Option<hmac::Key> {
    match crate::activity::load_or_make_key(config_dir, ROUTE_KEY_FILE) {
        Ok(secret) => Some(hmac::Key::new(hmac::HMAC_SHA256, &secret)),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read or make the proxy route key");
            None
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Makes [`route_key`] fail on this thread, as an unreadable key file does.
    static ROUTE_KEY_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// This process's route key, or None if it can't be had. Only test builds, which never run
/// [`crate::bootstrap::bootstrap`], have a fixed one, and only while no directory is set.
fn route_key() -> Option<hmac::Key> {
    #[cfg(test)]
    if ROUTE_KEY_FAILS.get() {
        return None;
    }
    match ROUTE_KEY_DIR.get() {
        Some(dir) => open_route_key(dir),
        #[cfg(any(test, feature = "test-support"))]
        None => Some(hmac::Key::new(hmac::HMAC_SHA256, &[7; 32])),
        #[cfg(not(any(test, feature = "test-support")))]
        None => None,
    }
}

/// Refuses a proxy with an account while the route key can't be had: its route could not be told
/// from another account's, so nothing is sent through it.
pub(crate) fn check_account_key(proxy: &str) -> Result<(), ProxyRefused> {
    check_account_key_with(proxy, route_key)
}

/// [`check_account_key`] under the key `key` gives, asked for only when `proxy` has an account.
fn check_account_key_with(
    proxy: &str,
    key: impl FnOnce() -> Option<hmac::Key>,
) -> Result<(), ProxyRefused> {
    if account_of(proxy).is_some() && key().is_none() {
        return Err(ProxyRefused::NoRouteKey);
    }
    Ok(())
}

/// The user info of `proxy`, if it has any.
fn account_of(proxy: &str) -> Option<String> {
    let uri = proxy.trim().parse::<Uri>().ok()?;
    Some(uri.authority()?.as_str().rsplit_once('@')?.0.to_owned())
}

/// The first 128 bits of the keyed digest of an account at `address`, in hex.
fn account_digest(key: &hmac::Key, address: &str, user_info: &str) -> String {
    let message = format!("{address}\0{user_info}");
    hmac::sign(key, message.as_bytes()).as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An identity for an account whose route key is unavailable: it tells accounts apart in this
/// process, is the same across its reloads, and matches nothing saved or any other process.
fn unkeyed_identity(address: &str, user_info: &str) -> String {
    static PROCESS_KEY: OnceLock<Option<hmac::Key>> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let key = PROCESS_KEY
        .get_or_init(|| hmac::Key::generate(hmac::HMAC_SHA256, &SystemRandom::new()).ok());
    match key {
        Some(key) => format!(
            "{address}{UNKEYED}{}",
            account_digest(key, address, user_info)
        ),
        // No random bytes: a new identity each time, so it can never compare equal.
        None => format!("{address}{UNKEYED}{}", NEXT.fetch_add(1, Ordering::Relaxed)),
    }
}

/// Whether `identity`, from [`route_identity`], can hold a saved or granted route.
pub(crate) fn is_stable_identity(identity: &str) -> bool {
    !identity.contains(UNKEYED)
}

/// `proxy` as pins and grants compare it: [`shown`], plus an opaque keyed digest of its user info
/// when it has any, so another account at one address is another route. The digest follows a
/// `#`, which [`shown`] drops, and the credential itself is never kept.
pub(crate) fn route_identity(proxy: &str) -> String {
    route_identity_with(proxy, route_key)
}

/// [`route_identity`] under the key `key` gives, asked for only when `proxy` has an account.
fn route_identity_with(proxy: &str, key: impl FnOnce() -> Option<hmac::Key>) -> String {
    let address = shown(proxy);
    match account_of(proxy) {
        Some(user_info) if address != UNREADABLE => match key() {
            Some(key) => format!("{address}#{}", account_digest(&key, &address, &user_info)),
            None => unkeyed_identity(&address, &user_info),
        },
        _ => address,
    }
}

/// The changes that keep proxy credentials from an exec plugin, given the variables it would
/// inherit and the ones set or removed for it (`None`): each proxy variable, in any case, whose
/// final value holds user info, with its address alone, or `None` to unset one that can't be
/// read. Variables without user info are left as they are.
pub(crate) fn plugin_environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    explicit: &[(OsString, Option<OsString>)],
) -> Vec<(OsString, Option<String>)> {
    let is_proxy = |name: &OsStr| {
        let name = name.to_string_lossy().to_ascii_lowercase();
        name.ends_with("_proxy") && name != "no_proxy"
    };
    let kept = inherited
        .into_iter()
        .filter(|(name, _)| !explicit.iter().any(|(set, _)| set == name));
    let set = explicit
        .iter()
        .filter_map(|(name, value)| Some((name.clone(), value.clone()?)));
    kept.chain(set)
        .filter(|(name, value)| is_proxy(name) && value.as_encoded_bytes().contains(&b'@'))
        .map(|(name, value)| {
            let address = value.to_str().map(shown).filter(|s| s != UNREADABLE);
            (name, address)
        })
        .collect()
}

/// A proxy variable for an exec plugin: set to the value, or removed (`None`).
pub(crate) type PluginVariable = (&'static str, Option<String>);

/// The proxy variables an exec plugin starts with under the proxy setting this process uses,
/// each to set (`Some`) or remove (`None`), so a plugin reaches its sign-in service the way
/// Flint's own connections go when they take the setting. Applied before the kubeconfig's own
/// `env`, which wins, and before [`plugin_environment`], which still keeps any credentials of
/// the URL from the plugin. Empty without a setting.
///
/// While `settings.toml` can't be read, only a cluster with a `proxy-url` of its own (`own`)
/// connects, and its plugin is given that proxy, with no exemptions, as its connection is.
pub(crate) fn plugin_setting_variables(own: Option<&str>) -> Vec<PluginVariable> {
    let in_use = setting_in_use();
    plugin_variables_under(in_use.as_ref().map(|(_, in_use)| in_use), own)
}

/// [`plugin_setting_variables`] under the setting `in_use`.
fn plugin_variables_under(in_use: Option<&InUse>, own: Option<&str>) -> Vec<PluginVariable> {
    match in_use {
        Some(InUse::Setting(setting)) => setting_variables(setting),
        None => Vec::new(),
        Some(InUse::Unreadable) => {
            let Some(own) = own.map(str::trim).filter(|u| !u.is_empty()) else {
                return Vec::new();
            };
            setting_variables(&ProxySetting {
                url: Some(own.to_owned()),
                no_proxy: Vec::new(),
            })
        }
    }
}

/// [`plugin_setting_variables`] for `setting`. Both spellings of each variable are set, since
/// tools differ in which they read first. A SOCKS URL goes in `ALL_PROXY` too, which curl reads.
fn setting_variables(setting: &ProxySetting) -> Vec<(&'static str, Option<String>)> {
    let mut variables = Vec::new();
    let url = setting
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let no_proxy = setting.no_proxy_list();
    if let Some(url) = url {
        for name in ["HTTPS_PROXY", "https_proxy"] {
            variables.push((name, Some(url.to_owned())));
        }
        if scheme_of(url).is_some_and(|s| SocksResolution::of_scheme(&s).is_some()) {
            for name in ["ALL_PROXY", "all_proxy"] {
                variables.push((name, Some(url.to_owned())));
            }
        }
    }
    // A URL takes its own no-proxy list, or none: the environment's belongs to its own proxy.
    if url.is_some() || no_proxy.is_some() {
        for name in ["NO_PROXY", "no_proxy"] {
            variables.push((name, no_proxy.clone()));
        }
    }
    variables
}

/// The scheme of `proxy`, in lowercase, if it can be read.
fn scheme_of(proxy: &str) -> Option<String> {
    let uri = proxy.trim().parse::<Uri>().ok()?;
    Some(uri.scheme_str()?.to_ascii_lowercase())
}

/// What a grant (trust, an unlock, AI exposure, keep watching) is pinned to: the server, and the
/// proxy shown for it. A grant made for one route does not hold for another.
pub fn grant_endpoint<'a>(server: &'a str, proxy: Option<&str>) -> Cow<'a, str> {
    match proxy {
        Some(proxy) => Cow::Owned(format!("{} via {proxy}", server.trim_end_matches('/'))),
        None => Cow::Borrowed(server),
    }
}

/// Why a connection was not made through its proxy. Its text holds no credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProxyRefused {
    #[error(
        "the proxy URL can't be read; use http://, socks5:// or socks5h://host:port. Flint did \
         not connect without it"
    )]
    Unreadable,
    #[error(
        "the proxy {shown} uses {scheme}, which Flint does not support; use an http://, \
         socks5:// or socks5h:// proxy. Flint did not connect without it"
    )]
    Unsupported { shown: String, scheme: String },
    #[error(
        "settings.toml could not be read when Flint started, so its proxy setting is unknown \
         and this cluster has no proxy-url of its own; fix settings.toml and restart Flint. \
         Flint did not connect"
    )]
    SettingsUnreadable,
    #[error(
        "the proxy's user name or password is longer than SOCKS5 allows (255 bytes). Flint did \
         not connect without it"
    )]
    SocksCredentialTooLong,
    #[error(
        "the API server uses plain HTTP, so a proxy could read its credentials; use https. \
         Flint did not connect"
    )]
    PlainHttp,
    #[error(
        "the API server's certificate is not verified (insecure-skip-tls-verify), so a proxy \
         could pose as it; remove that setting. Flint did not connect"
    )]
    UnverifiedTls,
    #[error("the API server's address can't be read. Flint did not connect")]
    UnreadableServer,
    #[error(
        "Flint can't read or create its proxy key file, which a proxy with a user name needs. \
         Check the permissions of Flint's config folder. Flint did not connect"
    )]
    NoRouteKey,
}

/// Refuses a tunnel to `server` unless its address is exact and verified TLS keeps its
/// credentials from the proxy.
pub(crate) fn check_route(server: &str, skips_verify: bool) -> Result<(), ProxyRefused> {
    let server = server
        .trim()
        .parse::<Uri>()
        .map_err(|_| ProxyRefused::UnreadableServer)?;
    if !authority_is_valid(&server) {
        return Err(ProxyRefused::UnreadableServer);
    }
    if !server
        .scheme_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
    {
        return Err(ProxyRefused::PlainHttp);
    }
    if skips_verify {
        return Err(ProxyRefused::UnverifiedTls);
    }
    Ok(())
}

/// Where a SOCKS5 proxy's destination names are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocksResolution {
    /// `socks5://`: here, and the proxy is given an address.
    Local,
    /// `socks5h://`: by the proxy, which is given the name.
    Proxy,
}

impl SocksResolution {
    /// The resolution `scheme` (in lowercase) stands for, or None if it is not SOCKS5.
    fn of_scheme(scheme: &str) -> Option<Self> {
        match scheme {
            "socks5" => Some(SocksResolution::Local),
            "socks5h" => Some(SocksResolution::Proxy),
            _ => None,
        }
    }
}

/// How a proxy is spoken to, with its credentials. Never `Debug`: they would show.
#[derive(Clone)]
enum Kind {
    /// HTTP `CONNECT`, with a `Proxy-Authorization` header value marked sensitive.
    Connect { authorization: Option<HeaderValue> },
    /// SOCKS5, with a user name and password.
    Socks {
        resolution: SocksResolution,
        account: Option<(String, String)>,
    },
}

/// A proxy to tunnel through, with its credentials.
#[derive(Clone)]
pub(crate) struct ProxyTarget {
    /// The proxy's address alone, so no connection error can quote the credentials.
    uri: Uri,
    kind: Kind,
}

impl std::fmt::Debug for ProxyTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyTarget")
            .field("uri", &self.uri)
            .finish_non_exhaustive()
    }
}

/// A connector reaching every destination through a proxy ([`ProxyTarget::tunnel`]).
pub(crate) enum ProxyConnector<C> {
    Connect(Tunnel<C>),
    Socks(SocksConnector<C>),
}

/// A SOCKS5 tunnel. For `socks5://` the destination's name is resolved here, without blocking
/// an async worker, and the proxy is given the address.
#[derive(Clone)]
pub(crate) struct SocksConnector<C> {
    socks: SocksV5<C>,
    resolution: SocksResolution,
}

/// The API server's name could not be resolved for a `socks5://` proxy, which is given an
/// address. Says nothing of the proxy.
#[derive(Debug, thiserror::Error)]
#[error("the API server's name can't be resolved")]
pub(crate) struct UnresolvedName;

/// Why a SOCKS5 tunnel was not opened: `E` is the SOCKS connector's own error.
#[derive(Debug)]
pub(crate) enum SocksConnectError<E> {
    /// `socks5://` could not resolve the destination's name.
    Unresolved(UnresolvedName),
    Socks(E),
}

impl<E: std::fmt::Display> std::fmt::Display for SocksConnectError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocksConnectError::Unresolved(e) => e.fmt(f),
            SocksConnectError::Socks(e) => e.fmt(f),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for SocksConnectError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SocksConnectError::Unresolved(e) => Some(e),
            SocksConnectError::Socks(e) => Some(e),
        }
    }
}

/// The error of the SOCKS5 connector over `C`.
type SocksErrorOf<C> = <SocksV5<C> as tower::Service<Uri>>::Error;

/// `dst` with its host replaced by the first address its name resolves to, looked up without
/// blocking the async worker. An address is kept as it is.
async fn resolved(dst: Uri) -> Result<Uri, UnresolvedName> {
    let host = dst.host().ok_or(UnresolvedName)?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(dst);
    }
    let port = dst.port_u16().unwrap_or(443);
    let address = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| UnresolvedName)?
        .next()
        .ok_or(UnresolvedName)?;
    let mut parts = dst.into_parts();
    parts.authority = Some(address.to_string().parse().map_err(|_| UnresolvedName)?);
    Uri::from_parts(parts).map_err(|_| UnresolvedName)
}

impl<C> tower::Service<Uri> for SocksConnector<C>
where
    C: tower::Service<Uri> + Clone + Send + 'static,
    C::Future: Send + 'static,
    C::Response: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    C::Error: Send + 'static,
{
    type Response = C::Response;
    type Error = SocksConnectError<SocksErrorOf<C>>;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.socks.poll_ready(cx).map_err(SocksConnectError::Socks)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        if self.resolution == SocksResolution::Proxy {
            let connecting = self.socks.call(dst);
            return Box::pin(async move { connecting.await.map_err(SocksConnectError::Socks) });
        }
        // The connector made ready goes with the call; a clone stays for the next one.
        let clone = self.socks.clone();
        let mut socks = std::mem::replace(&mut self.socks, clone);
        Box::pin(async move {
            let dst = resolved(dst).await.map_err(SocksConnectError::Unresolved)?;
            socks.call(dst).await.map_err(SocksConnectError::Socks)
        })
    }
}

impl ProxyTarget {
    /// The proxy at `proxy`, if Flint can connect through it.
    pub(crate) fn parse(proxy: &str) -> Result<Self, ProxyRefused> {
        let proxy = proxy.trim();
        if proxy == SETTINGS_UNREADABLE {
            return Err(ProxyRefused::SettingsUnreadable);
        }
        let address = shown(proxy);
        if address == UNREADABLE {
            return Err(ProxyRefused::Unreadable);
        }
        let uri = proxy.parse::<Uri>().map_err(|_| ProxyRefused::Unreadable)?;
        let (Some(scheme), Some(_)) = (uri.scheme_str(), uri.host()) else {
            return Err(ProxyRefused::Unreadable);
        };
        let scheme = scheme.to_ascii_lowercase();
        let socks = SocksResolution::of_scheme(&scheme);
        if scheme != "http" && socks.is_none() {
            return Err(ProxyRefused::Unsupported {
                shown: address,
                scheme,
            });
        }
        // A SOCKS proxy's address names its port: the connector under the tunnel would take 80
        // for a scheme it does not know.
        let address = match (socks, uri.port_u16()) {
            (Some(_), None) => format!("{address}:{SOCKS_DEFAULT_PORT}"),
            _ => address,
        };
        let address = address
            .parse::<Uri>()
            .map_err(|_| ProxyRefused::Unreadable)?;
        // The matcher knows SOCKS schemes in lowercase only.
        let (_, rest) = proxy.split_once("://").ok_or(ProxyRefused::Unreadable)?;
        let destination = Uri::from_static("https://api.invalid");
        let intercept = Matcher::builder()
            .all(format!("{scheme}://{rest}"))
            .build()
            .intercept(&destination)
            .ok_or(ProxyRefused::Unreadable)?;
        let kind = match socks {
            None => Kind::Connect {
                authorization: intercept.basic_auth().cloned(),
            },
            Some(resolution) => {
                let account = intercept
                    .raw_auth()
                    .map(|(user, password)| (user.to_owned(), password.to_owned()));
                if account
                    .as_ref()
                    .is_some_and(|(user, password)| user.len() > 255 || password.len() > 255)
                {
                    return Err(ProxyRefused::SocksCredentialTooLong);
                }
                Kind::Socks {
                    resolution,
                    account,
                }
            }
        };
        Ok(ProxyTarget { uri: address, kind })
    }

    /// Whether this is a SOCKS5 proxy.
    pub(crate) fn is_socks(&self) -> bool {
        matches!(self.kind, Kind::Socks { .. })
    }

    /// `connector`, reaching every destination through a tunnel to this proxy: `CONNECT` for an
    /// HTTP proxy, SOCKS5 for a SOCKS one.
    pub(crate) fn tunnel<C>(&self, connector: C) -> ProxyConnector<C> {
        match &self.kind {
            Kind::Connect { authorization } => {
                let tunnel = Tunnel::new(self.uri.clone(), connector);
                ProxyConnector::Connect(match authorization {
                    Some(authorization) => tunnel.with_auth(authorization.clone()),
                    None => tunnel,
                })
            }
            Kind::Socks {
                resolution,
                account,
            } => {
                // For `socks5://`, `SocksConnector` resolves the name first, so the proxy is
                // given an address; for `socks5h://`, the name.
                let socks = SocksV5::new(self.uri.clone(), connector);
                let socks = match account {
                    Some((user, password)) => socks.with_auth(user.clone(), password.clone()),
                    None => socks,
                };
                ProxyConnector::Socks(SocksConnector {
                    socks,
                    resolution: *resolution,
                })
            }
        }
    }
}

// module-tests: proxy_tests.rs
#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
