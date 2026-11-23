//! The proxy a connection goes through, if any.
//!
//! A cluster entry's `proxy-url` always applies. Without one, `HTTPS_PROXY` (or `https_proxy`)
//! does, unless `NO_PROXY` (or `no_proxy`) exempts the API server. Only `http://` proxies are
//! supported, through `CONNECT`: TLS to the API server runs inside the tunnel, so the proxy sees
//! neither the credentials nor the requests. Any other proxy is refused, never skipped.
//!
//! A proxy URL can hold a password. Only [`shown`] leaves this module as text.

use std::borrow::Cow;

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
    pub(crate) fn current() -> Self {
        let first = |names: [&str; 2]| {
            names
                .iter()
                .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
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
        if uri.host().is_none() {
            return false;
        }
        let port = uri.port_u16().unwrap_or(default_port);
        let mut hosts = Vec::new();
        for entry in no_proxy.split(',').map(str::trim) {
            if entry == "*" {
                return true;
            }
            let (host, only_port) = no_proxy_entry(entry);
            if only_port.is_none_or(|only| only == port) {
                hosts.push(host);
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
    let (scheme, host) = (scheme.to_ascii_lowercase(), host.to_ascii_lowercase());
    match uri.port_u16() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

/// The changes that keep this process's proxy credentials from an exec plugin it starts: each
/// proxy variable holding user info, with its address alone, or `None` to unset one that can't
/// be read. Variables without user info are left as they are.
pub(crate) fn plugin_environment() -> Vec<(&'static str, Option<String>)> {
    const VARIABLES: [&str; 6] = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    VARIABLES
        .into_iter()
        .filter_map(|name| {
            let value = std::env::var_os(name)?;
            if !value.as_encoded_bytes().contains(&b'@') {
                return None;
            }
            let address = value.to_str().map(shown).filter(|s| s != UNREADABLE);
            Some((name, address))
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
}

/// Refuses a tunnel to `server` unless verified TLS keeps its credentials from the proxy.
pub(crate) fn check_route(server: &Uri, skips_verify: bool) -> Result<(), ProxyRefused> {
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
