//! Reads cluster-wide `metrics.k8s.io` lists within a byte and an item limit.
//!
//! These lists do not page, so the limits apply while the answer comes in: the body stops being
//! read at [`MAX_BODY_BYTES`], and decoding stops at [`MAX_OBJECTS_PER_KIND`] items, keeping only
//! the parsed form of each.

use std::cell::Cell;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::pin;
use std::time::Duration;

use futures::io::{AsyncRead, AsyncReadExt as _};
use kube::Client;
use kube::api::ListParams;
use kube::core::Request;
use serde::Deserialize;
use serde::de::{self, DeserializeOwned, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess};

use super::RequestGate;
use super::objects::MAX_OBJECTS_PER_KIND;
use crate::api_error::{ApiError, ApiErrorKind};
use crate::cluster::{ClusterError, SafeErrorText};
use crate::metrics::{
    ContainerMetrics, MetricsError, MetricsList, MetricsResult, NodeMetrics, PodMetrics, Usage,
};

const API_PATH: &str = "/apis/metrics.k8s.io/v1beta1";

/// The longest one read may take, waiting for its turn included.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest answer read: room for [`MAX_OBJECTS_PER_KIND`] ordinary entries.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// What one bounded read gave.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Bounded<T> {
    Listed(MetricsResult<T>),
    /// The answer passed a limit, so it is not counted in part.
    TooLarge,
}

#[derive(Deserialize)]
struct WireMeta {
    name: Option<String>,
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    cpu: Option<String>,
    memory: Option<String>,
}

impl WireUsage {
    fn parse(&self) -> Option<Usage> {
        Some(Usage {
            cpu: self.cpu.as_deref()?.parse().ok()?,
            memory: self.memory.as_deref()?.parse().ok()?,
        })
    }
}

#[derive(Deserialize)]
struct WireContainer {
    name: String,
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WirePod {
    metadata: WireMeta,
    timestamp: Option<String>,
    window: Option<String>,
    #[serde(default)]
    containers: Vec<WireContainer>,
}

#[derive(Deserialize)]
struct WireNode {
    metadata: WireMeta,
    timestamp: Option<String>,
    window: Option<String>,
    usage: WireUsage,
}

/// `None` also for an empty `containers`: a missing sample, not a measured zero.
fn parse_pod(pod: WirePod) -> Option<PodMetrics> {
    if pod.containers.is_empty() {
        return None;
    }
    let containers = pod
        .containers
        .into_iter()
        .map(|container| {
            Some(ContainerMetrics {
                usage: container.usage.parse()?,
                name: container.name,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(PodMetrics {
        namespace: pod.metadata.namespace.filter(|n| !n.is_empty())?,
        name: pod.metadata.name.filter(|n| !n.is_empty())?,
        timestamp: pod.timestamp.and_then(|t| t.parse().ok()),
        window: pod.window.and_then(|w| w.parse().ok()),
        containers,
    })
}

fn parse_node(node: WireNode) -> Option<NodeMetrics> {
    Some(NodeMetrics {
        usage: node.usage.parse()?,
        name: node.metadata.name.filter(|n| !n.is_empty())?,
        timestamp: node.timestamp.and_then(|t| t.parse().ok()),
        window: node.window.and_then(|w| w.parse().ok()),
    })
}

/// Every pod's metrics, across all namespaces.
pub(super) async fn read_pods(client: &Client, requests: &RequestGate) -> Bounded<PodMetrics> {
    read(client, requests, "pods", parse_pod).await
}

/// Every node's metrics.
pub(super) async fn read_nodes(client: &Client, requests: &RequestGate) -> Bounded<NodeMetrics> {
    read(client, requests, "nodes", parse_node).await
}

async fn read<W, T>(
    client: &Client,
    requests: &RequestGate,
    resource: &str,
    parse: fn(W) -> Option<T>,
) -> Bounded<T>
where
    W: DeserializeOwned + 'static,
    T: Send + 'static,
{
    let fetch = fetch_body(client, resource);
    let body = match within(READ_TIMEOUT, requests.run(fetch)).await {
        Ok(Some(body)) => body,
        Ok(None) => return Bounded::TooLarge,
        Err(error) => return Bounded::Listed(Err(error)),
    };
    // Decoding a large answer is too long a job for an async worker.
    let decoded = tokio::task::spawn_blocking(move || decode(&body, MAX_OBJECTS_PER_KIND, parse));
    match decoded.await {
        Ok(read) => read,
        Err(_) => Bounded::Listed(Err(failed("could not read the metrics"))),
    }
}

async fn within<T>(
    limit: Duration,
    read: impl Future<Output = Result<T, MetricsError>>,
) -> Result<T, MetricsError> {
    tokio::time::timeout(limit, read)
        .await
        .unwrap_or_else(|_| Err(failed("timed out reading metrics")))
}

/// The body at `resource`, or `None` if it passes [`MAX_BODY_BYTES`].
async fn fetch_body(client: &Client, resource: &str) -> Result<Option<Vec<u8>>, MetricsError> {
    let request = Request::new(format!("{API_PATH}/{resource}"))
        .list(&ListParams::default())
        .map_err(|err| failed(&err.to_string()))?;
    let body = client
        .request_stream(request)
        .await
        .map_err(|err| classify(&err))?;
    read_capped(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| failed("the connection dropped while reading metrics"))
}

/// All of `reader`, or `None` as soon as it passes `cap` bytes.
async fn read_capped(reader: impl AsyncRead, cap: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut limited = pin!(reader.take(cap as u64 + 1));
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes).await?;
    Ok((bytes.len() <= cap).then_some(bytes))
}

fn classify(err: &kube::Error) -> MetricsError {
    match ApiError::from_kube(err).map(|api| api.kind()) {
        Some(ApiErrorKind::NotFound) => MetricsError::NotInstalled,
        Some(ApiErrorKind::Forbidden) => {
            MetricsError::Forbidden(ClusterError::kube_ref(err).summary())
        }
        _ => MetricsError::Failed(ClusterError::kube_ref(err).summary()),
    }
}

fn failed(text: &str) -> MetricsError {
    MetricsError::Failed(SafeErrorText::reduced(text))
}

/// Decodes a list one item at a time, so that an unreadable item loses only itself.
fn decode<W: DeserializeOwned, T>(
    body: &[u8],
    cap: usize,
    parse: fn(W) -> Option<T>,
) -> Bounded<T> {
    let over = Cell::new(false);
    let items = Items {
        cap,
        parse,
        over: &over,
        wire: PhantomData,
    };
    let mut reader = serde_json::Deserializer::from_slice(body);
    let read = de::Deserializer::deserialize_map(&mut reader, ListBody(Some(items)));
    match read {
        _ if over.get() => Bounded::TooLarge,
        Ok(list) if reader.end().is_ok() => Bounded::Listed(Ok(list)),
        Ok(_) | Err(_) => Bounded::Listed(Err(failed("could not read the metrics"))),
    }
}

struct ListBody<'a, W, T>(Option<Items<'a, W, T>>);

impl<'de, W: DeserializeOwned, T> de::Visitor<'de> for ListBody<'_, W, T> {
    type Value = MetricsList<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a metrics list")
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut list = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "items" {
                let items = self.0.take().ok_or(de::Error::duplicate_field("items"))?;
                list = Some(map.next_value_seed(items)?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        list.ok_or_else(|| de::Error::missing_field("items"))
    }
}

struct Items<'a, W, T> {
    cap: usize,
    parse: fn(W) -> Option<T>,
    /// Set when the list passed `cap`, which ends decoding with an error.
    over: &'a Cell<bool>,
    wire: PhantomData<W>,
}

impl<'de, W: DeserializeOwned, T> DeserializeSeed<'de> for Items<'_, W, T> {
    type Value = MetricsList<T>;

    fn deserialize<D: de::Deserializer<'de>>(self, reader: D) -> Result<Self::Value, D::Error> {
        reader.deserialize_seq(self)
    }
}

impl<'de, W: DeserializeOwned, T> de::Visitor<'de> for Items<'_, W, T> {
    type Value = MetricsList<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of metrics")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let (mut items, mut total) = (Vec::new(), 0usize);
        while let Some(item) = seq.next_element::<serde_json::Value>()? {
            total += 1;
            if total > self.cap {
                self.over.set(true);
                return Err(de::Error::custom("too many items"));
            }
            if let Some(parsed) = serde_json::from_value(item).ok().and_then(self.parse) {
                items.push(parsed);
            }
        }
        Ok(MetricsList {
            skipped: total - items.len(),
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use futures::io::Cursor;

    use super::*;

    const NODE: &str =
        r#"{"metadata":{"name":"a"},"usage":{"cpu":"100m","memory":"1Mi"},"window":"15s"}"#;
    const UNREADABLE: &str = r#"{"metadata":{"name":"b"},"usage":{"cpu":"nope"}}"#;

    fn nodes(body: &str, cap: usize) -> Bounded<NodeMetrics> {
        decode(body.as_bytes(), cap, parse_node)
    }

    #[test]
    fn an_unreadable_item_loses_only_itself() {
        let body = format!(r#"{{"kind":"NodeMetricsList","items":[{NODE},{UNREADABLE}]}}"#);
        let Bounded::Listed(Ok(list)) = nodes(&body, 10) else {
            panic!("the list should be read");
        };
        assert_eq!((list.items.len(), list.skipped), (1, 1));
        assert_eq!(list.items[0].name, "a");
    }

    #[test]
    fn a_list_past_the_item_limit_is_too_large() {
        let body = format!(r#"{{"items":[{NODE},{NODE},{NODE}]}}"#);
        assert_eq!(nodes(&body, 2), Bounded::TooLarge);
        assert!(matches!(nodes(&body, 3), Bounded::Listed(Ok(_))));
    }

    #[test]
    fn an_answer_that_is_not_a_list_is_a_failure() {
        for body in ["{}", r#"{"items":null}"#, "[]", r#"{"items":[]} trailing"#] {
            assert!(
                matches!(
                    nodes(body, 2),
                    Bounded::Listed(Err(MetricsError::Failed(_)))
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn a_pod_with_no_container_sample_is_skipped() {
        let body = r#"{"items":[{"metadata":{"name":"p","namespace":"n"},"containers":[]}]}"#;
        let Bounded::Listed(Ok(list)) = decode(body.as_bytes(), 2, parse_pod) else {
            panic!("the list should be read");
        };
        assert_eq!((list.items.len(), list.skipped), (0, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn the_token_file_limit_leaves_workers_free_and_an_open_gate_waits_for_nothing() {
        let limit = RequestGate::token_file_limit();
        let running = std::sync::Arc::new(tokio::sync::Barrier::new(
            super::super::CONCURRENT_TOKEN_FILE_REQUESTS,
        ));
        let mut held = tokio::task::JoinSet::new();
        for _ in 0..super::super::CONCURRENT_TOKEN_FILE_REQUESTS {
            let (limit, running) = (limit.clone(), running.clone());
            held.spawn(async move {
                limit
                    .run(async {
                        running.wait().await;
                        std::future::pending::<()>().await;
                    })
                    .await;
            });
        }
        tokio::time::sleep(Duration::from_millis(1)).await;

        let waited = tokio::time::timeout(Duration::from_secs(1), limit.run(async {})).await;
        assert!(waited.is_err(), "a request past the limit waits");
        RequestGate::default().run(async {}).await;
    }

    #[tokio::test]
    async fn reading_stops_at_the_byte_limit() {
        let read = |len: usize| read_capped(Cursor::new(vec![b'x'; len]), 8);
        assert_eq!(read(8).await.unwrap().map(|bytes| bytes.len()), Some(8));
        assert_eq!(read(9).await.unwrap(), None);
    }
}
