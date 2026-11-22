//! Parses API server request paths and query strings.

use super::fixtures::Gvr;

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `/version`
    Version,
    /// `/api` (core group discovery)
    DiscoveryCore,
    /// `/apis` (named group discovery)
    DiscoveryGroups,
    /// `/openapi/v3`
    OpenApiIndex,
    /// `/openapi/v3/<path>`
    OpenApiDocument(String),
    /// `/apis/metrics.k8s.io/v1beta1`, the one per-group discovery Flint reads.
    MetricsDiscovery,
    /// A collection: list or watch.
    Collection { gvr: Gvr, namespace: Option<String> },
    /// One object.
    Object {
        gvr: Gvr,
        namespace: Option<String>,
        name: String,
    },
    /// A subresource of one object (`/log`, `/status`, ...).
    Subresource {
        gvr: Gvr,
        namespace: Option<String>,
        name: String,
        subresource: String,
    },
    /// Anything else (legacy per-group discovery, unknown paths).
    Other,
}

/// What a request does, for faults and the request log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verb {
    Version,
    Discovery,
    OpenApi,
    List,
    Watch,
    Get,
    /// Writes and anything else.
    Other,
}

impl Route {
    pub fn parse(path: &str) -> Route {
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        match segments.as_slice() {
            ["version"] => Route::Version,
            ["api"] => Route::DiscoveryCore,
            ["apis"] => Route::DiscoveryGroups,
            ["openapi", "v3"] => Route::OpenApiIndex,
            ["openapi", "v3", rest @ ..] if !rest.is_empty() => {
                Route::OpenApiDocument(rest.join("/"))
            }
            ["apis", "metrics.k8s.io", "v1beta1"] => Route::MetricsDiscovery,
            ["api", version, rest @ ..] => resource_route("", version, rest),
            ["apis", group, version, rest @ ..] => resource_route(group, version, rest),
            _ => Route::Other,
        }
    }

    /// The resource a request is about, if any.
    pub fn gvr(&self) -> Option<&Gvr> {
        match self {
            Route::Collection { gvr, .. }
            | Route::Object { gvr, .. }
            | Route::Subresource { gvr, .. } => Some(gvr),
            _ => None,
        }
    }

    pub fn namespace(&self) -> Option<&str> {
        match self {
            Route::Collection { namespace, .. }
            | Route::Object { namespace, .. }
            | Route::Subresource { namespace, .. } => namespace.as_deref(),
            _ => None,
        }
    }

    /// The object a request names, if any.
    pub fn name(&self) -> Option<&str> {
        match self {
            Route::Object { name, .. } | Route::Subresource { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The subresource a request is about (`log`), if any.
    pub fn subresource(&self) -> Option<&str> {
        match self {
            Route::Subresource { subresource, .. } => Some(subresource),
            _ => None,
        }
    }

    pub fn verb(&self, method: &http::Method, query: &Query) -> Verb {
        if method != http::Method::GET {
            return Verb::Other;
        }
        match self {
            Route::Version => Verb::Version,
            Route::DiscoveryCore | Route::DiscoveryGroups | Route::MetricsDiscovery => {
                Verb::Discovery
            }
            Route::OpenApiIndex | Route::OpenApiDocument(_) => Verb::OpenApi,
            Route::Collection { .. } if query.watch => Verb::Watch,
            Route::Collection { .. } => Verb::List,
            Route::Object { .. } | Route::Subresource { .. } => Verb::Get,
            Route::Other => Verb::Other,
        }
    }
}

fn resource_route(group: &str, version: &str, rest: &[&str]) -> Route {
    let gvr = |resource: &str| Gvr::new(group, version, resource);
    let ns = |n: &str| Some(n.to_owned());
    match rest {
        ["namespaces", namespace, resource] => Route::Collection {
            gvr: gvr(resource),
            namespace: ns(namespace),
        },
        ["namespaces", namespace, resource, name] => Route::Object {
            gvr: gvr(resource),
            namespace: ns(namespace),
            name: (*name).to_owned(),
        },
        ["namespaces", namespace, resource, name, sub] => Route::Subresource {
            gvr: gvr(resource),
            namespace: ns(namespace),
            name: (*name).to_owned(),
            subresource: (*sub).to_owned(),
        },
        [resource] if !resource.is_empty() => Route::Collection {
            gvr: gvr(resource),
            namespace: None,
        },
        [resource, name] => Route::Object {
            gvr: gvr(resource),
            namespace: None,
            name: (*name).to_owned(),
        },
        [resource, name, sub] => Route::Subresource {
            gvr: gvr(resource),
            namespace: None,
            name: (*name).to_owned(),
            subresource: (*sub).to_owned(),
        },
        _ => Route::Other,
    }
}

/// The query parameters the fake server understands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub watch: bool,
    pub resource_version: Option<String>,
    pub send_initial_events: bool,
    pub allow_watch_bookmarks: bool,
    pub timeout_seconds: Option<u64>,
    pub limit: Option<usize>,
    pub continue_token: Option<String>,
    pub field_selector: Option<String>,
    pub label_selector: Option<String>,
    pub resource_version_match: Option<String>,
    /// `tailLines` of a log request.
    pub tail_lines: Option<i64>,
    /// `container` of a log request.
    pub container: Option<String>,
    /// `follow` of a log request.
    pub follow: bool,
    /// `dryRun=All` of a write.
    pub dry_run: bool,
    /// `fieldManager` of a write.
    pub field_manager: Option<String>,
    /// `previous` of a log request.
    pub previous: bool,
    /// `timestamps` of a log request.
    pub timestamps: bool,
    /// `sinceSeconds` of a log request.
    pub since_seconds: Option<i64>,
    /// `sinceTime` of a log request, as sent.
    pub since_time: Option<String>,
    /// `limitBytes` of a log request.
    pub limit_bytes: Option<i64>,
    /// The `command` parameters of an exec request, in order: the executable and its arguments.
    pub command: Vec<String>,
    /// `stdin` of an exec request.
    pub stdin: bool,
    /// `stdout` of an exec request.
    pub stdout: bool,
    /// `stderr` of an exec request.
    pub stderr: bool,
    /// `tty` of an exec request.
    pub tty: bool,
}

impl Query {
    pub fn parse(query: Option<&str>) -> Query {
        let mut q = Query::default();
        for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = percent_decode(value);
            let truthy = value == "true" || value == "1";
            match key {
                "watch" => q.watch = truthy,
                "resourceVersion" => q.resource_version = Some(value),
                "sendInitialEvents" => q.send_initial_events = truthy,
                "allowWatchBookmarks" => q.allow_watch_bookmarks = truthy,
                "timeoutSeconds" => q.timeout_seconds = value.parse().ok(),
                "limit" => q.limit = value.parse().ok().filter(|l| *l > 0),
                "continue" => q.continue_token = Some(value).filter(|v| !v.is_empty()),
                "fieldSelector" => q.field_selector = Some(value).filter(|v| !v.is_empty()),
                "labelSelector" => q.label_selector = Some(value).filter(|v| !v.is_empty()),
                "resourceVersionMatch" => q.resource_version_match = Some(value),
                "tailLines" => q.tail_lines = value.parse().ok(),
                "container" => q.container = Some(value).filter(|v| !v.is_empty()),
                "follow" => q.follow = truthy,
                "dryRun" => q.dry_run = value == "All",
                "fieldManager" => q.field_manager = Some(value),
                "previous" => q.previous = truthy,
                "timestamps" => q.timestamps = truthy,
                "sinceSeconds" => q.since_seconds = value.parse().ok(),
                "sinceTime" => q.since_time = Some(value).filter(|v| !v.is_empty()),
                "limitBytes" => q.limit_bytes = value.parse().ok(),
                "command" => q.command.push(value),
                "stdin" => q.stdin = truthy,
                "stdout" => q.stdout = truthy,
                "stderr" => q.stderr = truthy,
                "tty" => q.tty = truthy,
                _ => {}
            }
        }
        q
    }
}

/// Decodes `%XX` escapes and `+` (as a space).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = |b: Option<&u8>| b.and_then(|b| (*b as char).to_digit(16));
                match (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(gvr: Gvr, namespace: Option<&str>) -> Route {
        Route::Collection {
            gvr,
            namespace: namespace.map(str::to_owned),
        }
    }

    #[test]
    fn parses_discovery_and_resource_paths() {
        assert_eq!(Route::parse("/version"), Route::Version);
        assert_eq!(Route::parse("/api"), Route::DiscoveryCore);
        assert_eq!(Route::parse("/apis/"), Route::DiscoveryGroups);
        assert_eq!(Route::parse("/openapi/v3"), Route::OpenApiIndex);
        assert_eq!(
            Route::parse("/openapi/v3/apis/apps/v1"),
            Route::OpenApiDocument("apis/apps/v1".into())
        );
        assert_eq!(
            Route::parse("/api/v1/pods"),
            collection(Gvr::core("pods"), None)
        );
        assert_eq!(
            Route::parse("/apis/apps/v1/namespaces/demo/deployments"),
            collection(Gvr::new("apps", "v1", "deployments"), Some("demo"))
        );
        assert_eq!(
            Route::parse("/api/v1/namespaces/demo"),
            Route::Object {
                gvr: Gvr::core("namespaces"),
                namespace: None,
                name: "demo".into()
            }
        );
        let log = Route::parse("/api/v1/namespaces/demo/pods/web-1/log");
        assert_eq!(
            log,
            Route::Subresource {
                gvr: Gvr::core("pods"),
                namespace: Some("demo".into()),
                name: "web-1".into(),
                subresource: "log".into()
            }
        );
        assert_eq!(log.namespace(), Some("demo"));
        assert_eq!(log.name(), Some("web-1"));
        assert_eq!(log.subresource(), Some("log"));
        assert_eq!(Route::parse("/api/v1/pods").name(), None);
        assert_eq!(Route::parse("/api/v1/nodes/n").subresource(), None);
        assert_eq!(log.gvr(), Some(&Gvr::core("pods")));
        assert_eq!(Route::parse("/apis/apps/v1"), Route::Other);
        assert_eq!(Route::parse("/healthz"), Route::Other);
    }

    #[test]
    fn classifies_verbs() {
        let get = http::Method::GET;
        let list = Query::default();
        let watch = Query::parse(Some("watch=1"));
        let pods = Route::parse("/api/v1/pods");
        assert_eq!(pods.verb(&get, &list), Verb::List);
        assert_eq!(pods.verb(&get, &watch), Verb::Watch);
        assert_eq!(pods.verb(&http::Method::POST, &list), Verb::Other);
        assert_eq!(Route::parse("/api/v1/nodes/n").verb(&get, &list), Verb::Get);
        assert_eq!(Route::parse("/apis").verb(&get, &list), Verb::Discovery);
        assert_eq!(Route::parse("/openapi/v3").verb(&get, &list), Verb::OpenApi);
        assert_eq!(Route::Version.verb(&get, &list), Verb::Version);
    }

    #[test]
    fn parses_queries() {
        let q = Query::parse(Some(
            "watch=true&resourceVersion=12&allowWatchBookmarks=true&sendInitialEvents=true\
             &resourceVersionMatch=NotOlderThan&timeoutSeconds=290&limit=500&continue=&\
             fieldSelector=involvedObject.uid%3Dabc%2Cfoo%21%3Dbar&labelSelector=app+in",
        ));
        assert!(q.watch && q.allow_watch_bookmarks && q.send_initial_events);
        assert_eq!(q.resource_version.as_deref(), Some("12"));
        assert_eq!(q.timeout_seconds, Some(290));
        assert_eq!(q.limit, Some(500));
        assert_eq!(q.continue_token, None);
        assert_eq!(
            q.field_selector.as_deref(),
            Some("involvedObject.uid=abc,foo!=bar")
        );
        assert_eq!(q.label_selector.as_deref(), Some("app in"));
        assert_eq!(q.resource_version_match.as_deref(), Some("NotOlderThan"));
        assert_eq!(Query::parse(Some("tailLines=5")).tail_lines, Some(5));
        let log = Query::parse(Some(
            "container=app&follow=true&previous=true&timestamps=true&tailLines=-1\
             &sinceSeconds=30&sinceTime=2026-01-01T00%3A00%3A00Z&limitBytes=10",
        ));
        assert!(log.follow && log.previous && log.timestamps);
        assert_eq!(log.container.as_deref(), Some("app"));
        assert_eq!(log.tail_lines, Some(-1));
        assert_eq!(log.since_seconds, Some(30));
        assert_eq!(log.since_time.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(log.limit_bytes, Some(10));
        let exec = Query::parse(Some(
            "command=sh&command=-c&command=echo+%22a+b%22&stdin=true&stdout=true&tty=true",
        ));
        assert_eq!(exec.command, ["sh", "-c", "echo \"a b\""]);
        assert!(exec.stdin && exec.stdout && exec.tty && !exec.stderr);
        assert_eq!(Query::parse(None), Query::default());
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
