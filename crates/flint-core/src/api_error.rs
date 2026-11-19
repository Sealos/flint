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

    /// `"<reason>: <message>"` from the server's `Status`, or `None` when it says nothing
    /// [`status_meaning`] would not say better: no `Status` body, an empty one, or a 401 (whose
    /// `Status` only repeats "Unauthorized").
    pub fn explanation(&self) -> Option<String> {
        if self.kind() == ApiErrorKind::Unauthorized || self.reason == UNPARSED_STATUS {
            return None;
        }
        match (self.reason.is_empty(), self.message.is_empty()) {
            (true, true) => None,
            (false, true) => Some(self.reason.to_owned()),
            (true, false) => Some(self.message.to_owned()),
            (false, false) => Some(format!("{}: {}", self.reason, self.message)),
        }
    }
}

/// kube's `reason` when the error body was not a Kubernetes `Status` (it then puts the body,
/// often a proxy's HTML page, in `message`).
const UNPARSED_STATUS: &str = "Failed to parse error data";

/// Names the API server in a message when its URL is not known.
const SOME_SERVER: &str = "the API server";

/// What an HTTP error status means, naming `server` (a URL already through
/// [`crate::redact::server_url`]): `https://host:6443 rejected the credentials (401
/// Unauthorized); log in again or renew the token`.
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
        ApiErrorKind::Unauthorized => {
            format!("{s} rejected the credentials ({status}); log in again or renew the token")
        }
        ApiErrorKind::Forbidden => {
            format!("{s} denied access ({status}); check your RBAC permissions")
        }
        ApiErrorKind::NotFound => format!("{s} has no such resource ({status})"),
        _ => match code {
            429 => format!("{s} is limiting requests ({status}); try again later"),
            500 => format!("{s} failed with an internal error ({status})"),
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
    /// The TLS handshake failed; the reason in plain words.
    Tls(String),
    /// The server (or something between) reset the connection.
    Reset,
    /// The connection closed before the response, or a streaming response, was complete.
    Dropped,
    /// No route to the server's network or host.
    Unreachable,
}

impl Transport {
    /// The problem as a sentence fragment naming `server` (a URL already through
    /// [`crate::redact::server_url`]), or "the API server" when it is not known.
    pub fn message(&self, server: Option<&str>) -> String {
        let s = server.unwrap_or(SOME_SERVER);
        match self {
            Transport::Refused => format!("connection refused by {s}"),
            Transport::Unresolved => match server.and_then(host_of) {
                Some(host) => format!("could not resolve host {host}"),
                None => "could not resolve the API server's host name".to_owned(),
            },
            Transport::Tls(reason) => format!("TLS handshake with {s} failed: {reason}"),
            Transport::Reset => format!("connection reset by {s}"),
            Transport::Dropped => format!("the connection to {s} dropped"),
            Transport::Unreachable => format!("{s} is unreachable (no route to host)"),
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
        return Some(Transport::Tls(tls_reason(tls)));
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

/// Why a TLS handshake failed, in words a user can act on.
fn tls_reason(err: &rustls::Error) -> String {
    use rustls::CertificateError as C;
    use rustls::Error as E;
    match err {
        E::InvalidCertificate(cert) => match cert {
            // A bad signature is what a CA of the same name gives: every kubeadm CA is called
            // "kubernetes", so a kubeconfig left from an earlier cluster ends up here.
            C::UnknownIssuer | C::BadSignature => {
                "the server's certificate is not signed by a trusted certificate \
                                 authority (check certificate-authority-data in kubeconfig)"
                    .to_owned()
            }
            C::Expired | C::ExpiredContext { .. } => {
                "the server's certificate has expired".to_owned()
            }
            C::NotValidYet | C::NotValidYetContext { .. } => {
                "the server's certificate is not valid yet (check the clock)".to_owned()
            }
            C::NotValidForName | C::NotValidForNameContext { .. } => {
                "the server's certificate is not valid for this host name".to_owned()
            }
            C::Revoked => "the server's certificate was revoked".to_owned(),
            C::BadEncoding => "the server's certificate is malformed".to_owned(),
            // Platform verifiers (macOS) report distrust as `Other`, with a status code.
            _ => "the server's certificate is not trusted".to_owned(),
        },
        E::InvalidMessage(_) | E::InappropriateMessage { .. } => {
            "the server did not answer in TLS (is it an http:// endpoint?)".to_owned()
        }
        E::AlertReceived(alert) => {
            format!("the server ended the handshake ({alert:?}); check the client certificate")
        }
        E::PeerIncompatible(_) => {
            "the server and Flint have no TLS version or cipher in common".to_owned()
        }
        E::NoCertificatesPresented => "the server sent no certificate".to_owned(),
        other => other.to_string(),
    }
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
        let server = Some("https://127.0.0.1:6443");
        for (problem, named, anonymous) in [
            (
                Transport::Refused,
                "connection refused by https://127.0.0.1:6443",
                "connection refused by the API server",
            ),
            (
                Transport::Unresolved,
                "could not resolve host 127.0.0.1",
                "could not resolve the API server's host name",
            ),
            (
                Transport::Tls("why".into()),
                "TLS handshake with https://127.0.0.1:6443 failed: why",
                "TLS handshake with the API server failed: why",
            ),
            (
                Transport::Reset,
                "connection reset by https://127.0.0.1:6443",
                "connection reset by the API server",
            ),
            (
                Transport::Dropped,
                "the connection to https://127.0.0.1:6443 dropped",
                "the connection to the API server dropped",
            ),
            (
                Transport::Unreachable,
                "https://127.0.0.1:6443 is unreachable (no route to host)",
                "the API server is unreachable (no route to host)",
            ),
        ] {
            assert_eq!(problem.message(server), named);
            assert_eq!(problem.message(None), anonymous);
        }
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
        assert_eq!(
            classify_transport(&tls),
            Some(Transport::Tls(
                "the server's certificate has expired".to_owned()
            ))
        );
    }

    #[test]
    fn tls_reasons_are_plain() {
        use rustls::CertificateError as C;
        for (err, reason) in [
            (
                rustls::Error::InvalidCertificate(C::NotValidForName),
                "the server's certificate is not valid for this host name",
            ),
            (
                rustls::Error::InvalidCertificate(C::NotValidYet),
                "the server's certificate is not valid yet (check the clock)",
            ),
            (
                rustls::Error::InvalidCertificate(C::Revoked),
                "the server's certificate was revoked",
            ),
            (
                rustls::Error::InvalidCertificate(C::ApplicationVerificationFailure),
                "the server's certificate is not trusted",
            ),
            (
                rustls::Error::AlertReceived(rustls::AlertDescription::BadCertificate),
                "the server ended the handshake (BadCertificate); check the client certificate",
            ),
            (
                rustls::Error::NoCertificatesPresented,
                "the server sent no certificate",
            ),
        ] {
            assert_eq!(tls_reason(&err), reason);
        }
    }
}
