//! The rule every client that is not behind the write gate enforces (ADR-0015): it sends only
//! `GET`s of paths known to read. The check runs before the request leaves the client, so a
//! write path that skips the gate fails instead of reaching the cluster.
//!
//! The paths are an allowlist, and anything else is refused. That matters for `GET`s too: `exec`,
//! `attach` and `portforward` are upgrades of a `GET`, and `proxy` can tunnel a write through
//! one. Flint makes no reads by `POST`, so none is allowed.

use http::{Method, Request};
use kube::client::Body;
use tower::BoxError;
use tower::filter::FilterLayer;

/// The client stack's layer that refuses what [`allows`] does not.
pub(crate) type ReadOnlyLayer = FilterLayer<fn(Request<Body>) -> Result<Request<Body>, BoxError>>;

/// A new [`ReadOnlyLayer`].
pub(crate) fn layer() -> ReadOnlyLayer {
    FilterLayer::new(check)
}

/// Why a read-only client did not send a request.
#[derive(Debug, thiserror::Error)]
#[error("refused a {method} request: this connection is read-only")]
struct Refused {
    method: Method,
}

fn check(request: Request<Body>) -> Result<Request<Body>, BoxError> {
    if allows(request.method(), request.uri().path()) {
        Ok(request)
    } else {
        Err(Box::new(Refused {
            method: request.method().clone(),
        }))
    }
}

/// Whether a read-only client may send `method` to `path`.
fn allows(method: &Method, path: &str) -> bool {
    if method != Method::GET {
        return false;
    }
    let Some(path) = path.strip_prefix('/') else {
        return false;
    };
    let Some(segments) = path.split('/').map(segment).collect::<Option<Vec<_>>>() else {
        return false;
    };
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    match segments.as_slice() {
        ["version"] | ["api"] | ["apis"] | ["api", _] | ["apis", _] | ["apis", _, _] => true,
        // The schemas the fixture recorder reads.
        ["openapi", "v3"] | ["openapi", "v3", "api", "v1"] | ["openapi", "v3", "apis", _, _] => {
            true
        }
        ["api", _, rest @ ..] => reads_core_objects(rest),
        ["apis", _, _, rest @ ..] => reads_group_objects(rest),
        _ => false,
    }
}

/// Whether the part of a core API path after its version lists or gets objects, or reads a
/// pod's log. Names may be anything, a pod named `proxy` included; a resource may not be a
/// tunnel (`/api/v1/proxy/...` is the old form of the `proxy` subresource).
fn reads_core_objects(rest: &[&str]) -> bool {
    match rest {
        ["namespaces", _, "pods", _, "log"] => true,
        [resource]
        | [resource, _]
        | ["namespaces", _, resource]
        | ["namespaces", _, resource, _] => !is_tunnel(resource),
        _ => false,
    }
}

/// Whether the part of a group's API path after its version lists or gets objects. A custom
/// resource may be named `exec` or `proxy`; a subresource, which this never allows, is what
/// would be a tunnel.
fn reads_group_objects(rest: &[&str]) -> bool {
    matches!(
        rest,
        [_] | [_, _] | ["namespaces", _, _] | ["namespaces", _, _, _]
    )
}

/// The subresources that open a way into the cluster, in any case.
fn is_tunnel(resource: &str) -> bool {
    ["exec", "attach", "portforward", "proxy"]
        .iter()
        .any(|t| resource.eq_ignore_ascii_case(t))
}

/// One path segment, percent-decoded, or `None` if it could mean anything else once a server or
/// proxy has normalized it: empty (a trailing or doubled slash), a dot segment, a bad escape, or
/// an escape that hides a separator or another escape.
fn segment(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    let unsafe_char = |c: char| matches!(c, '/' | '\\' | '%' | ';') || c.is_control();
    if decoded.is_empty() || decoded == "." || decoded == ".." || decoded.contains(unsafe_char) {
        return None;
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    const READS: &[&str] = &[
        "/version",
        "/api",
        "/apis",
        "/api/v1",
        "/apis/apps",
        "/apis/apps/v1",
        "/api/v1/pods",
        "/api/v1/nodes",
        "/api/v1/nodes/worker-1",
        "/api/v1/namespaces",
        "/api/v1/namespaces/demo",
        "/api/v1/namespaces/demo/pods",
        "/api/v1/namespaces/demo/pods/web",
        "/api/v1/namespaces/demo/pods/web/log",
        "/apis/apps/v1/deployments",
        "/apis/apps/v1/namespaces/demo/deployments",
        "/apis/apps/v1/namespaces/demo/deployments/web",
        "/apis/metrics.k8s.io/v1beta1/pods",
        "/apis/rbac.authorization.k8s.io/v1/clusterroles/system:node",
        "/apis/rbac.authorization.k8s.io/v1/clusterroles/system%3Anode",
        // Objects may be named like a tunnel: only a resource in that place is one.
        "/api/v1/namespaces/demo/pods/proxy",
        "/api/v1/namespaces/demo/pods/exec/log",
        "/api/v1/namespaces/proxy/pods/attach",
        "/api/v1/nodes/portforward",
        // Custom resources may be named like a tunnel.
        "/apis/example.com/v1/exec",
        "/apis/example.com/v1/attach/one",
        "/apis/example.com/v1/namespaces/demo/proxy",
        "/apis/example.com/v1/namespaces/demo/portforward/one",
        // The schemas the fixture recorder reads.
        "/openapi/v3",
        "/openapi/v3/api/v1",
        "/openapi/v3/apis/apps/v1",
    ];

    // covers UP-54: what the read paths ask for still goes through.
    #[test]
    fn reads_are_allowed() {
        for path in READS {
            assert!(allows(&Method::GET, path), "{path}");
        }
    }

    // covers UP-54: no other verb leaves a read-only client, on any path.
    #[test]
    fn every_other_verb_is_refused() {
        for method in [
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
            Method::CONNECT,
            Method::TRACE,
            Method::from_bytes(b"PROPFIND").unwrap(),
        ] {
            for path in READS {
                assert!(!allows(&method, path), "{method} {path}");
            }
        }
        for review in [
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        ] {
            assert!(!allows(&Method::POST, review), "{review}");
        }
    }

    // covers UP-54: every way into a pod, node or service is refused, however it is spelled.
    #[test]
    fn tunnels_are_refused_however_they_are_spelled() {
        let pod = "/api/v1/namespaces/demo/pods/web";
        for tunnel in ["exec", "attach", "portforward", "proxy"] {
            let encoded: String = tunnel.bytes().map(|b| format!("%{b:02x}")).collect();
            let upper: String = tunnel.bytes().map(|b| format!("%{b:02X}")).collect();
            for path in [
                format!("{pod}/{tunnel}"),
                format!("{pod}/{tunnel}/"),
                format!("{pod}//{tunnel}"),
                format!("{pod}/./{tunnel}"),
                format!("{pod}/x/../{tunnel}"),
                format!("{pod}/{}", tunnel.to_uppercase()),
                format!("{pod}/{encoded}"),
                format!("{pod}/{upper}"),
                format!("{pod}%2F{tunnel}"),
                format!("{pod}%2f{tunnel}"),
                format!("{pod}%252F{tunnel}"),
                format!("{pod};x/{tunnel}"),
                format!("{pod}/{tunnel};x"),
                format!("{pod}/{tunnel}/extra/path"),
                format!("/api/v1/nodes/worker-1/{tunnel}"),
                format!("/api/v1/nodes/worker-1/{tunnel}/metrics"),
                format!("/api/v1/namespaces/demo/services/web:80/{tunnel}"),
                format!("/api/v1/namespaces/demo/services/web:80/{tunnel}/admin"),
                format!("/api/v1/{tunnel}/namespaces/demo/pods/web"),
                format!("/api/v1/{tunnel}/nodes/worker-1"),
                format!("/api/v1/{tunnel}"),
                format!("/api/v1/{tunnel}/x"),
                format!("/api/v1/{encoded}/x"),
                format!("/api/v1/namespaces/demo/{tunnel}"),
                format!("/api/v1/namespaces/demo/{tunnel}/x"),
                format!("/apis/example.com/v1/namespaces/demo/widgets/w/{tunnel}"),
            ] {
                assert!(!allows(&Method::GET, &path), "{path}");
            }
        }
    }

    // covers UP-54: anything the allowlist does not name is refused.
    #[test]
    fn unknown_or_malformed_paths_are_refused() {
        for path in [
            "",
            "/",
            "api/v1/pods",
            "/healthz",
            "/metrics",
            "/logs/kube-apiserver.log",
            "/openapi",
            "/openapi/v2",
            "/openapi/v3/",
            "/openapi/v3/api",
            "/openapi/v3/api/v2",
            "/openapi/v3/apis/apps",
            "/openapi/v3/apis/apps/v1/extra",
            "/apis/example.com/v1/namespaces/demo/pods/web/log",
            "/apis/example.com/v1/widgets/one/status",
            "/api/",
            "/api/v1/",
            "/api/v1/pods/",
            "/api//v1/pods",
            "/api/v1/namespaces/demo/pods/web/log/",
            "/api/v1/namespaces/demo/pods/web/LOG",
            "/api/v1/namespaces/demo/pods/web/status",
            "/api/v1/namespaces/demo/pods/web/ephemeralcontainers",
            "/apis/apps/v1/namespaces/demo/deployments/web/scale",
            "/api/v1/namespaces/demo/secrets/x/log",
            "/api/v1/pods/..",
            "/api/v1/pods/%2e%2e",
            "/api/v1/pods/%2E",
            "/api/v1/pods/%",
            "/api/v1/pods/%2",
            "/api/v1/pods/%zz",
            "/api/v1/pods/%00",
            "/api/v1/pods/%ff",
            "/api/v1/pods/a%5Cb",
            "/api/v1/pods/a%25",
            "/other/v1/pods",
        ] {
            assert!(!allows(&Method::GET, path), "{path}");
        }
    }
}
