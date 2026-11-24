//! Where the watch engine gets lists and watch streams from: the API server (Table format) or, in
//! tests, a fake.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{AsyncBufReadExt as _, FutureExt as _, StreamExt as _};
use kube::Client;
use kube::api::{Request, WatchParams};

use crate::cluster;
use crate::read::{KindInfo, ListQuery};
use crate::table::{self, FetchError, WireEvent, WireObject};

pub(crate) type EventStream = BoxStream<'static, Result<WireEvent, FetchError>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchRequest {
    /// Empty with `initial_events` for a streaming list.
    pub resource_version: String,
    /// Streaming list (`sendInitialEvents=true`, `resourceVersionMatch=NotOlderThan`).
    pub initial_events: bool,
    pub timeout_secs: u32,
}

pub(crate) trait Source: Send + Sync + 'static {
    /// One page of a Table list, across all namespaces (`None`) or in one.
    fn list(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        limit: Option<u32>,
        continue_token: Option<String>,
    ) -> BoxFuture<'static, Result<WireObject, FetchError>>;

    /// A Table watch, across all namespaces (`None`) or in one.
    fn watch(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        request: WatchRequest,
    ) -> BoxFuture<'static, Result<EventStream, FetchError>>;

    /// Whether to try streaming lists first.
    fn streaming_lists(&self) -> bool;

    /// The server rejected a streaming list: use list + watch from now on.
    fn disable_streaming_lists(&self);
}

/// The API server, through kube's raw request API with a Table Accept header.
pub(crate) struct KubeSource {
    client: Client,
    /// Names the server in errors.
    target: cluster::Target,
    streaming: AtomicBool,
}

/// Streaming lists are validated (and rejected when disabled) from 1.27; older servers ignore
/// the parameter, which would leave a list waiting for an end marker that never comes.
const STREAMING_LISTS_MIN_MINOR: u32 = 27;

impl KubeSource {
    pub fn new(
        client: Client,
        version: Option<&cluster::ServerVersion>,
        target: cluster::Target,
    ) -> Arc<Self> {
        let streaming = version
            .and_then(|v| server_minor(&v.git_version))
            .is_some_and(|minor| minor >= STREAMING_LISTS_MIN_MINOR);
        Arc::new(KubeSource {
            client,
            target,
            streaming: AtomicBool::new(streaming),
        })
    }
}

/// The minor version of a `v1.31.2-eks-…` style version string (major must be 1).
pub(crate) fn server_minor(git_version: &str) -> Option<u32> {
    let rest = git_version.trim_start_matches('v');
    let mut parts = rest.split('.');
    if parts.next()? != "1" {
        return None;
    }
    let minor: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    minor.parse().ok()
}

/// Parses one watch line. A bare `Status` (no event envelope) becomes an error.
pub(crate) fn parse_event_line(line: &str) -> Result<WireEvent, FetchError> {
    match serde_json::from_str::<WireEvent>(line) {
        Ok(event) => Ok(event),
        Err(e) => match serde_json::from_str::<WireObject>(line) {
            Ok(o) if o.is_status() => Err(FetchError::from_code(
                o.code.unwrap_or(0),
                o.status_message(),
            )),
            _ => Err(FetchError::Other(format!("unreadable watch event: {e}"))),
        },
    }
}

impl Source for KubeSource {
    fn list(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        limit: Option<u32>,
        continue_token: Option<String>,
    ) -> BoxFuture<'static, Result<WireObject, FetchError>> {
        let client = self.client.clone();
        let path = table::collection_path(kind, namespace);
        let query = ListQuery {
            limit,
            continue_token,
            ..ListQuery::default()
        };
        let target = self.target.clone();
        async move {
            let path = path.map_err(|error| FetchError::Invalid(error.to_string()))?;
            table::fetch_page(&client, &path, &query.to_params())
                .await
                .map_err(|e| e.into_fetch(&target))
        }
        .boxed()
    }

    fn watch(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        request: WatchRequest,
    ) -> BoxFuture<'static, Result<EventStream, FetchError>> {
        let client = self.client.clone();
        let target = self.target.clone();
        let path = table::collection_path(kind, namespace);
        async move {
            let path = path.map_err(|error| FetchError::Invalid(error.to_string()))?;
            let mut params = WatchParams::default().timeout(request.timeout_secs);
            if request.initial_events {
                params = params.initial_events();
            }
            let req = Request::new(path)
                .watch(&params, &request.resource_version)
                .map_err(|e| FetchError::Other(cluster::error_chain(&e)))?;
            let mut req = table::as_table_request(req)?;
            // As kube does for watches: Kubernetes 1.37 gzips watch streams one member per event,
            // which the client's decompression cannot read past the first member.
            req.headers_mut().insert(
                "accept-encoding",
                http::HeaderValue::from_static("identity"),
            );
            let body = client
                .request_stream(req)
                .await
                .map_err(|e| FetchError::from_kube(e, &target))?;
            let lines = Box::pin(body).lines();
            let events = lines.filter_map(move |line| {
                let result = match line {
                    Ok(l) if l.trim().is_empty() => None,
                    Ok(l) => Some(parse_event_line(&l)),
                    // Says "the connection to https://… dropped", not hyper's "error reading a
                    // body from connection".
                    Err(e) => Some(Err(FetchError::Other(
                        cluster::ClusterError::stream("watch stream", &e, &target)
                            .summary()
                            .to_string(),
                    ))),
                };
                std::future::ready(result)
            });
            Ok(events.boxed())
        }
        .boxed()
    }

    fn streaming_lists(&self) -> bool {
        self.streaming.load(Ordering::Relaxed)
    }

    fn disable_streaming_lists(&self) {
        self.streaming.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::EventType;

    #[test]
    fn minor_versions() {
        assert_eq!(server_minor("v1.37.0"), Some(37));
        assert_eq!(server_minor("v1.29.4-eks-a1b2"), Some(29));
        assert_eq!(server_minor("v1.30+"), Some(30));
        assert_eq!(server_minor("v2.0.0"), None);
        assert_eq!(server_minor("garbage"), None);
    }

    // covers UP-27
    #[test]
    fn bookmarks_are_detected_by_type_not_by_row() {
        let line = r#"{"type":"BOOKMARK","object":{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"801"},"columnDefinitions":null,"rows":[{"cells":["",null,null,null],"object":{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"801"}}}]}}"#;
        let event = parse_event_line(line).unwrap();
        assert_eq!(event.ty, EventType::Bookmark);
        assert_eq!(
            event.object.metadata.resource_version.as_deref(),
            Some("801")
        );
        assert!(!event.object.is_initial_events_end());
    }

    // covers UP-27
    #[test]
    fn a_bare_status_line_is_an_error() {
        let line = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Expired","code":410,"message":"gone"}"#;
        assert!(matches!(
            parse_event_line(line),
            Err(FetchError::Expired(_))
        ));
        assert!(matches!(
            parse_event_line("{not json"),
            Err(FetchError::Other(_))
        ));
    }
}
