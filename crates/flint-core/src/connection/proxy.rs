//! The proxy a connection goes through, if any.
//!
//! A cluster entry's `proxy-url` always applies. Without one, `HTTPS_PROXY` (or `https_proxy`)
//! does, unless `NO_PROXY` (or `no_proxy`) exempts the API server. Only `http://` proxies are
//! supported, through `CONNECT`: TLS to the API server runs inside the tunnel, so the proxy sees
//! neither the credentials nor the requests. Any other proxy is refused, never skipped.
//!
//! A proxy URL can hold a password. Only [`shown`], and [`route_identity`] with its one-way
//! digest, leave this module as text.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};

use http::{HeaderValue, Uri};
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::client::proxy::matcher::Matcher;

/// What [`shown`] says of a proxy URL it can't read, which may be a mistyped secret.
const UNREADABLE: &str = "an unreadable proxy URL";

/// Stands in for a cluster's proxy while kube loads its config, so kube neither parses the real
/// one nor an environment proxy that `NO_PROXY` may exempt. Flint then sets the proxy it chose;
/// were that ever skipped, this address leads nowhere.
pub(crate) const LOADER_PLACEHOLDER: &str = "http://proxy.invalid:1";

/// The proxy variables of an environment. Holds what may be a password, so it has no `Debug`.
#[derive(Clone, Default)]
pub(crate) struct ProxyEnvironment {
    pub(crate) https_proxy: Option<String>,
    pub(crate) no_proxy: Option<String>,
}

impl ProxyEnvironment {
    /// The variables of this process, in the order kube and kubectl read them.
    ///
    /// A value that is not UTF-8 stays set, lossily: as a proxy it then can't be read and is
    /// refused, and as `NO_PROXY` it exempts nothing. It never counts as unset.
    pub(crate) fn current() -> Self {
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

/// Rounds of the account digest. Grants are saved in `settings.toml`, so a guess at a proxy
/// password from one should cost real work.
const ACCOUNT_ROUNDS: u32 = 100_000;

/// `proxy` as pins and grants compare it: [`shown`], plus an opaque digest of its user info when
/// it has any, so another account at one address is another route. The digest follows a `#`,
/// which [`shown`] drops, and the credential itself is never kept.
pub(crate) fn route_identity(proxy: &str) -> String {
    let address = shown(proxy);
    let user_info = proxy
        .trim()
        .parse::<Uri>()
        .ok()
        .and_then(|uri| Some(uri.authority()?.as_str().rsplit_once('@')?.0.to_owned()));
    match user_info {
        Some(user_info) if address != UNREADABLE => {
            let mut digest = [0u8; 16];
            ring::pbkdf2::derive(
                ring::pbkdf2::PBKDF2_HMAC_SHA256,
                std::num::NonZeroU32::new(ACCOUNT_ROUNDS).expect("not zero"),
                format!("flint-proxy-account:{address}").as_bytes(),
                user_info.as_bytes(),
                &mut digest,
            );
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            format!("{address}#{hex}")
        }
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
    #[error("the proxy URL can't be read; use http://host:port. Flint did not connect without it")]
    Unreadable,
    #[error(
        "the proxy {shown} uses {scheme}, which Flint does not support; use an http:// proxy. \
         Flint did not connect without it"
    )]
    Unsupported { shown: String, scheme: String },
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

/// A proxy to tunnel through. Holds its credentials as a sensitive header value.
#[derive(Clone)]
pub(crate) struct ProxyTarget {
    uri: Uri,
    authorization: Option<HeaderValue>,
}

impl std::fmt::Debug for ProxyTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyTarget")
            .field("uri", &self.uri)
            .finish_non_exhaustive()
    }
}

impl ProxyTarget {
    /// The proxy at `proxy`, if Flint can connect through it.
    pub(crate) fn parse(proxy: &str) -> Result<Self, ProxyRefused> {
        let proxy = proxy.trim();
        if shown(proxy) == UNREADABLE {
            return Err(ProxyRefused::Unreadable);
        }
        let uri = proxy.parse::<Uri>().map_err(|_| ProxyRefused::Unreadable)?;
        let (Some(scheme), Some(_)) = (uri.scheme_str(), uri.host()) else {
            return Err(ProxyRefused::Unreadable);
        };
        if !scheme.eq_ignore_ascii_case("http") {
            return Err(ProxyRefused::Unsupported {
                shown: shown(proxy),
                scheme: scheme.to_ascii_lowercase(),
            });
        }
        // The address alone, so no connection error can quote the credentials.
        let uri = shown(proxy)
            .parse::<Uri>()
            .map_err(|_| ProxyRefused::Unreadable)?;
        let destination = Uri::from_static("https://api.invalid");
        let intercept = Matcher::builder()
            .all(proxy.to_owned())
            .build()
            .intercept(&destination)
            .ok_or(ProxyRefused::Unreadable)?;
        Ok(ProxyTarget {
            uri,
            authorization: intercept.basic_auth().cloned(),
        })
    }

    /// `connector`, reaching every destination through a `CONNECT` tunnel to this proxy.
    pub(crate) fn tunnel<C>(&self, connector: C) -> Tunnel<C> {
        let tunnel = Tunnel::new(self.uri.clone(), connector);
        match &self.authorization {
            Some(authorization) => tunnel.with_auth(authorization.clone()),
            None => tunnel,
        }
    }
}

// module-tests: proxy_tests.rs
#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
