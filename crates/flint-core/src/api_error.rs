//! Classification of what a request to the API server ran into, shared by the request paths in
//! the core, and how it is said in plain words (the same in the app and the MCP server).
//!
//! - An API status (`ApiError`): sorted by `ApiErrorKind`; a status that explains itself is
//!   shown as `"<reason>: <message>"` (`ApiError::explanation`), one that does not (a proxy's
//!   HTML page, an empty body, a bare 401) by what its code means ([`status_meaning`]).
//! - A failure below HTTP ([`Transport`]): kube reports it as a chain of client errors
//!   (`ServiceError: client error (Connect): tcp connect error: Connection refused (os error
//!   61)`); [`classify_transport`] finds the cause and [`Transport::message`] says it, naming the
//!   API server when it is known: `connection refused by https://127.0.0.1:6443`.

use std::error::Error as StdError;
use std::io::ErrorKind;

/// The API server's status for a kube error. Transport and client errors have no API status.
pub(crate) struct ApiError<'a> {
    pub code: u16,
    pub reason: &'a str,
    pub message: &'a str,
}

impl<'a> ApiError<'a> {
    pub fn from_kube(err: &'a kube::Error) -> Option<Self> {
        let kube::Error::Api(status) = err else {
            return None;
        };
        Some(Self {
            code: status.code,
            reason: &status.reason,
            message: &status.message,
        })
    }

    pub fn kind(&self) -> ApiErrorKind {
        ApiErrorKind::from_code(self.code)
    }

    /// `"<reason>: <message>"` from the server's `Status` (just the message when it already
    /// starts with the reason, as RBAC's "forbidden: User …" does), or `None` when it says
    /// nothing [`status_meaning`] would not say better: no `Status` body, an empty one, or a 401
    /// (whose `Status` only repeats "Unauthorized"). A body that is not a `Status` is never
    /// shown: the bare HTTP code says more ([`status_meaning`]). Launching an exec reads one
    /// known kubelet phrase itself ([`ApiError::kubelet_refusal`]).
    pub fn explanation(&self) -> Option<String> {
        if self.kind() == ApiErrorKind::Unauthorized || self.reason == UNPARSED_STATUS {
            return None;
        }
        let (reason, message) = (self.reason, self.message);
        let repeats_reason = message
            .get(..reason.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(reason));
        match (reason.is_empty(), message.is_empty()) {
            (true, true) => None,
            (false, true) => Some(reason.to_owned()),
            (true, false) => Some(message.to_owned()),
            (false, false) if repeats_reason => Some(message.to_owned()),
            (false, false) => Some(format!("{reason}: {message}")),
        }
    }

    /// The kubelet's refusal of an exec in a container that is not running, sent before the
    /// upgrade as a plain-text 500 ([`kubelet_reason`]), as the server's own reason. Only for
    /// launching an exec: anywhere else, and with any other status, that body stays a bare HTTP
    /// code, which tells more (a 502 from a proxy, say).
    pub(crate) fn kubelet_refusal(&self) -> Option<String> {
        if self.code != 500 || self.reason != UNPARSED_STATUS {
            return None;
        }
        kubelet_reason(self.message).map(str::to_owned)
    }
}

/// kube's `reason` when the error body was not a Kubernetes `Status` (it then puts the body,
/// often a proxy's HTML page, in `message`).
const UNPARSED_STATUS: &str = "Failed to parse error data";

/// The longest container name Kubernetes accepts (a DNS-1123 label).
const MAX_CONTAINER_NAME: usize = 63;

/// `body`, a plain-text error body, when it is exactly one of the kubelet's short refusals:
/// `container not found ("<name>")`, which it answers an exec in a container that is not
/// running with, before any upgrade (`GetExec`, `pkg/kubelet/kubelet_pods.go`). The name must
/// be a valid container name. Anything else is `None`: such a body can come from any proxy on
/// the way and hold anything, so none of its text is shown.
fn kubelet_reason(body: &str) -> Option<&str> {
    let body = body.trim();
    let name = body
        .strip_prefix("container not found (\"")?
        .strip_suffix("\")")?;
    let valid = (1..=MAX_CONTAINER_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    valid.then_some(body)
}

/// Names the API server in a message when its URL is not known.
const SOME_SERVER: &str = "the API server";

/// What an HTTP error status means, naming `server` (a URL already through
/// [`crate::redact::server_url`]): `https://host:6443 rejected the credentials (401
/// Unauthorized); log in again or renew the credentials`.
pub fn status_meaning(code: u16, server: Option<&str>) -> String {
    let s = server.unwrap_or(SOME_SERVER);
    let status = match http::StatusCode::from_u16(code)
        .ok()
        .and_then(|c| c.canonical_reason())
    {
        Some(reason) => format!("{code} {reason}"),
        None => format!("HTTP {code}"),
    };
    match ApiErrorKind::from_code(code) {
        ApiErrorKind::Unauthorized => format!(
            "{s} rejected the credentials ({status}); log in again or renew the credentials"
        ),
        ApiErrorKind::Forbidden => {
            format!("{s} denied access ({status}); check your RBAC permissions")
        }
        ApiErrorKind::NotFound => {
            format!("{s} answered {status}; is the server URL (and its path) right?")
        }
        _ => match code {
            429 => format!("{s} is limiting requests ({status}); try again later"),
            500 => format!(
                "{s} failed with an internal error ({status}); try again, or check the API \
                 server's logs"
            ),
            502 => format!("a proxy in front of {s} could not reach it ({status})"),
            503 => format!("{s} is unavailable ({status}); it may be starting or overloaded"),
            504 => format!("a proxy in front of {s} timed out waiting for it ({status})"),
            _ => format!("{s} answered {status}"),
        },
    }
}

/// What went wrong on the way to the API server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// Nothing listens on the server's port.
    Refused,
    /// The server's host name does not resolve.
    Unresolved,
    /// The TLS handshake failed.
    Tls(Tls),
    /// The server (or something between) reset the connection.
    Reset,
    /// The connection closed before the response, or a streaming response, was complete.
    Dropped,
    /// No route to the server's network or host.
    Unreachable,
    /// The proxy in front of the server could not be used.
    Proxy(ProxyFailure),
}

/// Why a tunnel through the proxy was not made. Only Flint's own words: never the proxy's URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyFailure {
    /// Nothing answered at the proxy's address.
    Unreachable,
    /// The proxy asked for credentials or refused them (HTTP 407, a SOCKS5 sign-in).
    AuthRequired,
    /// The proxy answered, but would not open the tunnel.
    Denied,
    /// The proxy closed the connection or sent something unreadable.
    Broken,
}

impl Transport {
    /// The problem as a sentence naming `server` (a URL already through
    /// [`crate::redact::server_url`]), or "the API server" when it is not known, with a hint
    /// at what to check.
    pub fn message(&self, server: Option<&str>) -> String {
        let s = server.unwrap_or(SOME_SERVER);
        let host = server.and_then(host_of);
        match self {
            Transport::Refused => format!(
                "connection refused by {s}; is the API server running, and is the port right?"
            ),
            Transport::Unresolved => match (&host, server) {
                (Some(host), Some(server)) => format!(
                    "could not resolve host {host} ({server}); check the server address, VPN or \
                     DNS"
                ),
                _ => "could not resolve the API server's host name; check the server address, \
                      VPN or DNS"
                    .to_owned(),
            },
            Transport::Tls(Tls::Failed) => format!("TLS handshake with {s} failed"),
            Transport::Tls(tls) => {
                format!(
                    "TLS handshake with {s} failed: {}",
                    tls.reason(host.as_deref())
                )
            }
            Transport::Reset => format!(
                "connection reset by {s}; a proxy, firewall or wrong port may be in the way"
            ),
            Transport::Dropped => format!("the connection to {s} dropped"),
            Transport::Unreachable => format!("{s} is unreachable (no route to host)"),
            Transport::Proxy(ProxyFailure::Unreachable) => format!(
                "could not reach the proxy for {s}; check its address. Flint did not connect \
                 without it"
            ),
            Transport::Proxy(ProxyFailure::AuthRequired) => format!(
                "the proxy for {s} asks for credentials or refused them; check the ones in its \
                 proxy URL"
            ),
            Transport::Proxy(ProxyFailure::Denied) => {
                format!("the proxy refused to open a tunnel to {s}")
            }
            Transport::Proxy(ProxyFailure::Broken) => {
                format!("the proxy for {s} closed the tunnel")
            }
        }
    }
}

/// Why a TLS handshake failed. Only Flint's own words: nothing from the peer or the platform
/// verifier is quoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tls {
    /// Not signed by the CA kubeconfig trusts (or by one of the same name).
    UntrustedCa,
    Expired,
    NotValidYet,
    /// Not issued for the host name connected to.
    WrongHost,
    Revoked,
    Malformed,
    /// Rejected for another reason (a platform verifier's own check).
    NotTrusted,
    /// The server answered, but not in TLS.
    NotTls,
    /// The server sent a certificate alert: it did not accept the client certificate.
    ClientCertificateRejected,
    /// Another alert, as lowercase words (`handshake failure`).
    Alert(String),
    NoCommonVersion,
    NoCertificate,
    /// Anything else.
    Failed,
}

impl Tls {
    /// The reason in plain words; `host` names the server's host for a name mismatch.
    fn reason(&self, host: Option<&str>) -> String {
        match self {
            Tls::UntrustedCa => "the server's certificate is not signed by a trusted CA (check \
                                 the cluster's certificate-authority in kubeconfig)"
                .to_owned(),
            Tls::Expired => "the server's certificate has expired".to_owned(),
            Tls::NotValidYet => {
                "the server's certificate is not valid yet (check the clock)".to_owned()
            }
            Tls::WrongHost => format!(
                "the server's certificate is not valid for {} (check the server address or \
                 tls-server-name)",
                host.unwrap_or("this host name")
            ),
            Tls::Revoked => "the server's certificate was revoked".to_owned(),
            Tls::Malformed => "the server's certificate is malformed".to_owned(),
            Tls::NotTrusted => "the server's certificate is not trusted".to_owned(),
            Tls::NotTls => {
                "the server did not answer in TLS (is it an http:// endpoint?)".to_owned()
            }
            Tls::ClientCertificateRejected => "the server rejected the client certificate \
                                               (check client-certificate in kubeconfig)"
                .to_owned(),
            Tls::Alert(words) => format!("the server ended the handshake ({words})"),
            Tls::NoCommonVersion => {
                "the server and Flint have no TLS version or cipher in common".to_owned()
            }
            Tls::NoCertificate => "the server sent no certificate".to_owned(),
            Tls::Failed => "the handshake failed".to_owned(),
        }
    }
}

/// The host part of a shown server URL.
fn host_of(server: &str) -> Option<String> {
    let uri = server.parse::<http::Uri>().ok()?;
    uri.host().filter(|h| !h.is_empty()).map(str::to_owned)
}

/// The transport problem in `err`'s chain, if any: the failures with no API status.
pub fn classify_transport(err: &(dyn StdError + 'static)) -> Option<Transport> {
    causes(err).find_map(classify_one)
}

fn classify_one(e: &(dyn StdError + 'static)) -> Option<Transport> {
    if let Some(tls) = e.downcast_ref::<rustls::Error>() {
        return Some(Transport::Tls(tls_problem(tls)));
    }
    // A `socks5://` proxy is given an address, so the name is looked up first, here.
    if e.is::<crate::connection::proxy::UnresolvedName>() {
        return Some(Transport::Unresolved);
    }
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        return match io.kind() {
            ErrorKind::ConnectionRefused => Some(Transport::Refused),
            ErrorKind::ConnectionReset => Some(Transport::Reset),
            ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof => Some(Transport::Dropped),
            ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => {
                Some(Transport::Unreachable)
            }
            // A wrapper: the cause is further down.
            _ => None,
        };
    }
    // hyper-util and hyper do not expose these as types or kinds; the messages are pinned by
    // the tests below, which build the errors from real connections.
    match e.to_string().as_str() {
        "dns error" => Some(Transport::Unresolved),
        "connection closed before message completed" => Some(Transport::Dropped),
        "tunnel error: failed to create underlying connection" => {
            Some(Transport::Proxy(ProxyFailure::Unreachable))
        }
        "tunnel error: proxy authorization required" => {
            Some(Transport::Proxy(ProxyFailure::AuthRequired))
        }
        "tunnel error: unsuccessful" => Some(Transport::Proxy(ProxyFailure::Denied)),
        "tunnel error: unexpected end of file"
        | "tunnel error: io error establishing tunnel"
        | "tunnel error: proxy response headers too long" => {
            Some(Transport::Proxy(ProxyFailure::Broken))
        }
        "SOCKS error: failed to create underlying connection" => {
            Some(Transport::Proxy(ProxyFailure::Unreachable))
        }
        "SOCKS error: credentials not accepted"
        | "SOCKS error: server does not support user/pass authentication" => {
            Some(Transport::Proxy(ProxyFailure::AuthRequired))
        }
        "SOCKS error: general server failure"
        | "SOCKS error: connection not allowed"
        | "SOCKS error: network unreachable"
        | "SOCKS error: host unreachable"
        | "SOCKS error: connection refused"
        | "SOCKS error: ttl expired"
        | "SOCKS error: command not supported"
        | "SOCKS error: address type not supported" => Some(Transport::Proxy(ProxyFailure::Denied)),
        "SOCKS error: io error during SOCKS handshake"
        | "SOCKS error: failed parsing server response"
        | "SOCKS error: server implements authentication incorrectly" => {
            Some(Transport::Proxy(ProxyFailure::Broken))
        }
        _ => None,
    }
}

/// `err` and its causes. A custom `io::Error` reports its inner error's source as its own,
/// skipping the inner error itself (where rustls and kube errors sit), so it is unwrapped here.
pub(crate) fn causes<'a>(
    err: &'a (dyn StdError + 'static),
) -> impl Iterator<Item = &'a (dyn StdError + 'static)> {
    std::iter::successors(Some(err), |&e: &&'a (dyn StdError + 'static)| {
        match e
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
        {
            Some(inner) => Some(inner as &(dyn StdError + 'static)),
            None => e.source(),
        }
    })
}

/// What a rustls error means for the user.
fn tls_problem(err: &rustls::Error) -> Tls {
    use rustls::AlertDescription as A;
    use rustls::CertificateError as C;
    use rustls::Error as E;
    match err {
        E::InvalidCertificate(cert) => match cert {
            // A bad signature is what a CA of the same name gives: every kubeadm CA is called
            // "kubernetes", so a kubeconfig left from an earlier cluster ends up here.
            C::UnknownIssuer | C::BadSignature => Tls::UntrustedCa,
            C::Expired | C::ExpiredContext { .. } => Tls::Expired,
            C::NotValidYet | C::NotValidYetContext { .. } => Tls::NotValidYet,
            C::NotValidForName | C::NotValidForNameContext { .. } => Tls::WrongHost,
            C::Revoked => Tls::Revoked,
            C::BadEncoding => Tls::Malformed,
            // Platform verifiers (macOS) report distrust as `Other`, with a status code.
            _ => Tls::NotTrusted,
        },
        E::InvalidMessage(_) | E::InappropriateMessage { .. } => Tls::NotTls,
        E::AlertReceived(
            A::BadCertificate
            | A::CertificateRequired
            | A::UnknownCA
            | A::CertificateExpired
            | A::CertificateRevoked
            | A::CertificateUnknown
            | A::UnsupportedCertificate
            | A::AccessDenied,
        ) => Tls::ClientCertificateRejected,
        E::AlertReceived(alert) => Tls::Alert(words(&format!("{alert:?}"))),
        E::PeerIncompatible(_) => Tls::NoCommonVersion,
        E::NoCertificatesPresented => Tls::NoCertificate,
        _ => Tls::Failed,
    }
}

/// `HandshakeFailure` as `handshake failure`.
fn words(camel: &str) -> String {
    let mut out = String::new();
    for c in camel.chars() {
        if c.is_ascii_uppercase() && !out.is_empty() {
            out.push(' ');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiErrorKind {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    MethodNotAllowed,
    Gone,
    UnprocessableEntity,
    Other,
}

impl ApiErrorKind {
    /// Also classifies status objects carried in a successful list or watch response.
    pub fn from_code(code: u16) -> Self {
        match code {
            400 => Self::BadRequest,
            401 => Self::Unauthorized,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            405 => Self::MethodNotAllowed,
            410 => Self::Gone,
            422 => Self::UnprocessableEntity,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
#[path = "api_error_tests.rs"]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_api_errors_and_keeps_the_server_details() {
        for (code, kind) in [
            (400, ApiErrorKind::BadRequest),
            (401, ApiErrorKind::Unauthorized),
            (403, ApiErrorKind::Forbidden),
            (404, ApiErrorKind::NotFound),
            (405, ApiErrorKind::MethodNotAllowed),
            (410, ApiErrorKind::Gone),
            (422, ApiErrorKind::UnprocessableEntity),
            (500, ApiErrorKind::Other),
        ] {
            let err = kube::Error::Api(Box::new(kube::core::Status {
                code,
                reason: "Reason".into(),
                message: "server details".into(),
                ..Default::default()
            }));
            let api = ApiError::from_kube(&err).unwrap();
            assert_eq!(api.kind(), kind);
            assert_eq!(api.code, code);
            assert_eq!(api.reason, "Reason");
            assert_eq!(api.message, "server details");
        }
    }

    #[test]
    fn messages_name_the_server_or_say_the_api_server() {
        let server = Some("https://api.example.test:6443");
        for (problem, named, anonymous) in [
            (
                Transport::Refused,
                "connection refused by https://api.example.test:6443; is the API server running, \
                 and is the port right?",
                "connection refused by the API server; is the API server running, and is the \
                 port right?",
            ),
            (
                Transport::Unresolved,
                "could not resolve host api.example.test (https://api.example.test:6443); check \
                 the server address, VPN or DNS",
                "could not resolve the API server's host name; check the server address, VPN or \
                 DNS",
            ),
            (
                Transport::Tls(Tls::WrongHost),
                "TLS handshake with https://api.example.test:6443 failed: the server's \
                 certificate is not valid for api.example.test (check the server address or \
                 tls-server-name)",
                "TLS handshake with the API server failed: the server's certificate is not valid \
                 for this host name (check the server address or tls-server-name)",
            ),
            (
                Transport::Tls(Tls::Failed),
                "TLS handshake with https://api.example.test:6443 failed",
                "TLS handshake with the API server failed",
            ),
            (
                Transport::Reset,
                "connection reset by https://api.example.test:6443; a proxy, firewall or wrong \
                 port may be in the way",
                "connection reset by the API server; a proxy, firewall or wrong port may be in \
                 the way",
            ),
            (
                Transport::Dropped,
                "the connection to https://api.example.test:6443 dropped",
                "the connection to the API server dropped",
            ),
            (
                Transport::Unreachable,
                "https://api.example.test:6443 is unreachable (no route to host)",
                "the API server is unreachable (no route to host)",
            ),
        ] {
            assert_eq!(problem.message(server), named);
            assert_eq!(problem.message(None), anonymous);
        }
    }

    #[test]
    fn explanations_keep_the_server_words_once() {
        let api = |code, reason, message| ApiError {
            code,
            reason,
            message,
        };
        assert_eq!(
            api(
                403,
                "Forbidden",
                "forbidden: User \"jane\" cannot get path \"/version\""
            )
            .explanation()
            .as_deref(),
            Some("forbidden: User \"jane\" cannot get path \"/version\"")
        );
        assert_eq!(
            api(404, "NotFound", "pods \"x\" not found")
                .explanation()
                .as_deref(),
            Some("NotFound: pods \"x\" not found")
        );
        assert_eq!(api(401, "Unauthorized", "Unauthorized").explanation(), None);
        assert_eq!(api(502, UNPARSED_STATUS, "<html>").explanation(), None);
        // Not even the kubelet's phrase: outside an exec launch, the HTTP code says more.
        assert_eq!(
            api(500, UNPARSED_STATUS, "container not found (\"job\")").explanation(),
            None
        );
        assert_eq!(api(500, "", "").explanation(), None);
        assert_eq!(api(409, "", "x").explanation().as_deref(), Some("x"));
    }

    #[test]
    fn only_a_strict_kubelet_phrase_is_read_from_a_plain_body() {
        let name63 = "a".repeat(MAX_CONTAINER_NAME);
        let ok = format!("container not found (\"{name63}\")");
        assert_eq!(kubelet_reason(&ok), Some(ok.as_str()));
        assert_eq!(
            kubelet_reason(" container not found (\"web-1\")\n"),
            Some("container not found (\"web-1\")")
        );
        let name64 = "a".repeat(MAX_CONTAINER_NAME + 1);
        for body in [
            "",
            "container not found",
            "container not found (\"\")",
            &format!("container not found (\"{name64}\")"),
            "container not found (\"Job\")",
            "container not found (\"-job\")",
            "container not found (\"job-\")",
            "container not found (\"job\") token=s3cr3t",
            "token=s3cr3t container not found (\"job\")",
            "container not found (\"a\"), password=s3cr3t (\"b\")",
            "container not found (\"<script>\")",
            "container not found (\"j\u{f6}b\")",
            "<html>container not found (\"job\")</html>",
        ] {
            assert_eq!(kubelet_reason(body), None, "{body:?}");
            assert_eq!(
                ApiError {
                    code: 500,
                    reason: UNPARSED_STATUS,
                    message: body,
                }
                .kubelet_refusal(),
                None,
                "{body:?}"
            );
        }
    }

    #[test]
    fn the_kubelet_refusal_is_only_a_plain_500() {
        let phrase = "container not found (\"job\")";
        let api = |code, reason| ApiError {
            code,
            reason,
            message: phrase,
        };
        assert_eq!(
            api(500, UNPARSED_STATUS).kubelet_refusal().as_deref(),
            Some(phrase)
        );
        // Another status keeps its HTTP diagnostic: a 401 (signing in), a 404, a proxy's 502.
        for code in [401, 404, 502, 503] {
            assert_eq!(api(code, UNPARSED_STATUS).kubelet_refusal(), None, "{code}");
        }
        // A parsed `Status` is the server's own words already ([`ApiError::explanation`]).
        assert_eq!(api(500, "InternalError").kubelet_refusal(), None);
    }

    #[test]
    fn io_kinds_map_to_problems() {
        for (kind, problem) in [
            (ErrorKind::ConnectionRefused, Some(Transport::Refused)),
            (ErrorKind::ConnectionReset, Some(Transport::Reset)),
            (ErrorKind::UnexpectedEof, Some(Transport::Dropped)),
            (ErrorKind::BrokenPipe, Some(Transport::Dropped)),
            (ErrorKind::HostUnreachable, Some(Transport::Unreachable)),
            (ErrorKind::NetworkUnreachable, Some(Transport::Unreachable)),
            (ErrorKind::PermissionDenied, None),
        ] {
            assert_eq!(
                classify_transport(&std::io::Error::from(kind)),
                problem,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn nested_io_errors_are_unwrapped() {
        // As hyper-rustls wraps a reset during the handshake.
        let inner = std::io::Error::from(ErrorKind::ConnectionReset);
        let outer = std::io::Error::other(inner);
        assert_eq!(classify_transport(&outer), Some(Transport::Reset));
        let tls = std::io::Error::other(std::io::Error::new(
            ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::Expired),
        ));
        assert_eq!(classify_transport(&tls), Some(Transport::Tls(Tls::Expired)));
    }

    #[test]
    fn tls_errors_map_to_problems_without_quoting_the_peer() {
        use rustls::AlertDescription as A;
        use rustls::CertificateError as C;
        for (err, problem) in [
            (
                rustls::Error::InvalidCertificate(C::NotValidForName),
                Tls::WrongHost,
            ),
            (
                rustls::Error::InvalidCertificate(C::NotValidYet),
                Tls::NotValidYet,
            ),
            (rustls::Error::InvalidCertificate(C::Revoked), Tls::Revoked),
            (
                rustls::Error::InvalidCertificate(C::ApplicationVerificationFailure),
                Tls::NotTrusted,
            ),
            (
                rustls::Error::AlertReceived(A::BadCertificate),
                Tls::ClientCertificateRejected,
            ),
            (
                rustls::Error::AlertReceived(A::CertificateRequired),
                Tls::ClientCertificateRejected,
            ),
            (
                rustls::Error::AlertReceived(A::HandshakeFailure),
                Tls::Alert("handshake failure".into()),
            ),
            (rustls::Error::NoCertificatesPresented, Tls::NoCertificate),
            // Free text from rustls or a verifier is never shown.
            (
                rustls::Error::General("peer said \"s3cr3t\"".into()),
                Tls::Failed,
            ),
        ] {
            assert_eq!(tls_problem(&err), problem);
        }
        assert_eq!(
            Transport::Tls(Tls::ClientCertificateRejected).message(None),
            "TLS handshake with the API server failed: the server rejected the client \
             certificate (check client-certificate in kubeconfig)"
        );
    }
}
