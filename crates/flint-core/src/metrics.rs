//! Current CPU and memory from `metrics.k8s.io/v1beta1` (metrics-server, ADR-0005).
//!
//! - [`Cpu`] and [`Memory`] are parsed Kubernetes quantities.
//! - [`fetch_pod_metrics`] and [`fetch_node_metrics`] list metrics through a client the caller
//!   already has, and tell "not installed" from "forbidden" from "failed" ([`MetricsError`]).
//! - [`MetricsPoll`] repeats a fetch while it is held, and stops when dropped.
//! - [`Usage::against`] compares usage with the [`PodBudget`] from [`PodMetrics::budget_in`], or
//!   with a node's allocatable [`Amounts`].

use std::collections::BTreeMap;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use k8s_openapi::api::core::v1::{Container, PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::Client;
use kube::api::ListParams;
use kube::core::Request;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::api_error::{ApiError, ApiErrorKind};
use crate::api_path;
use crate::cluster::{ClusterError, SafeErrorText};

/// How long a [`MetricsPoll`] from [`poll_pod_metrics`] or [`poll_node_metrics`] waits between
/// the end of one fetch and the start of the next.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// The longest one fetch may take, so that a stuck server cannot hold a poll forever.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

const GROUP_VERSION: &str = "metrics.k8s.io/v1beta1";
const API_PATH: &str = "/apis/metrics.k8s.io/v1beta1";

// ---------------------------------------------------------------------------------------------
// Quantities

/// A string that is not a non-negative Kubernetes quantity this type can hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a valid quantity")]
pub struct InvalidQuantity;

/// An amount of CPU, in nanocores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cpu(u64);

impl Cpu {
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub const fn nanos(self) -> u64 {
        self.0
    }

    /// Whole millicores, rounded up as `kubectl top` shows any non-zero use.
    pub const fn millis(self) -> u64 {
        self.0.div_ceil(1_000_000)
    }
}

impl FromStr for Cpu {
    type Err = InvalidQuantity;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        parse_quantity(text, 9).map(Self)
    }
}

/// An amount of memory, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Memory(u64);

impl Memory {
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }
}

impl FromStr for Memory {
    type Err = InvalidQuantity;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        parse_quantity(text, 0).map(Self)
    }
}

/// The largest decimal exponent accepted in a quantity, either way.
const MAX_EXPONENT: i32 = 40;

/// `text` in units of `10^-unit_exponent`, rounded up as Kubernetes rounds a value too precise
/// for its unit. Accepts decimal (`n u m k M G T P E`) and binary (`Ki` to `Ei`) suffixes and
/// decimal exponents (`1e3`, `1E-3`).
fn parse_quantity(text: &str, unit_exponent: i32) -> Result<u64, InvalidQuantity> {
    let unsigned = text.strip_prefix('+').unwrap_or(text);
    let end = unsigned
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(unsigned.len());
    let (number, suffix) = unsigned.split_at(end);
    let (shift, exponent) = parse_suffix(suffix).ok_or(InvalidQuantity)?;

    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if (whole.is_empty() && fraction.is_empty()) || fraction.contains('.') {
        return Err(InvalidQuantity);
    }
    let mut mantissa: u128 = 0;
    for digit in whole.bytes().chain(fraction.bytes()) {
        mantissa = mantissa
            .checked_mul(10)
            .and_then(|m| m.checked_add(u128::from(digit - b'0')))
            .ok_or(InvalidQuantity)?;
    }
    let fraction_digits = i32::try_from(fraction.len()).map_err(|_| InvalidQuantity)?;

    let scaled = mantissa.checked_shl(shift).ok_or(InvalidQuantity)?;
    if shift > 0 && scaled >> shift != mantissa {
        return Err(InvalidQuantity);
    }
    let power = exponent + unit_exponent - fraction_digits;
    let value = if power >= 0 {
        10u128
            .checked_pow(power.unsigned_abs())
            .and_then(|p| scaled.checked_mul(p))
            .ok_or(InvalidQuantity)?
    } else {
        match 10u128.checked_pow(power.unsigned_abs()) {
            Some(divisor) => scaled.div_ceil(divisor),
            // More than any u128: all that is left of a non-zero value is the round up.
            None => u128::from(scaled > 0),
        }
    };
    u64::try_from(value).map_err(|_| InvalidQuantity)
}

/// A quantity suffix as a binary shift and a decimal exponent.
fn parse_suffix(suffix: &str) -> Option<(u32, i32)> {
    Some(match suffix {
        "" => (0, 0),
        "n" => (0, -9),
        "u" => (0, -6),
        "m" => (0, -3),
        "k" => (0, 3),
        "M" => (0, 6),
        "G" => (0, 9),
        "T" => (0, 12),
        "P" => (0, 15),
        "E" => (0, 18),
        "Ki" => (10, 0),
        "Mi" => (20, 0),
        "Gi" => (30, 0),
        "Ti" => (40, 0),
        "Pi" => (50, 0),
        "Ei" => (60, 0),
        _ => {
            let exponent = suffix.strip_prefix(['e', 'E'])?.parse::<i32>().ok()?;
            ((-MAX_EXPONENT..=MAX_EXPONENT).contains(&exponent)).then_some((0, exponent))?
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Models

/// CPU and memory in use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub cpu: Cpu,
    pub memory: Memory,
}

impl Usage {
    /// This usage as shares of `budget`.
    pub fn against(&self, budget: &Amounts) -> UsageShares {
        UsageShares {
            cpu: share(self.cpu.nanos(), budget.cpu.map(Cpu::nanos)),
            memory: share(self.memory.bytes(), budget.memory.map(Memory::bytes)),
        }
    }
}

/// One container's use, from a [`PodMetrics`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerMetrics {
    pub name: String,
    pub usage: Usage,
}

/// A pod's `metrics.k8s.io` PodMetrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodMetrics {
    pub namespace: String,
    pub name: String,
    /// When the sample was taken, if the server said in a form that parses.
    pub timestamp: Option<Timestamp>,
    /// The window the sample covers, if the server said in a form that parses.
    pub window: Option<SignedDuration>,
    pub containers: Vec<ContainerMetrics>,
}

impl PodMetrics {
    /// The pod's total across its containers.
    pub fn total(&self) -> Usage {
        self.containers
            .iter()
            .fold(Usage::default(), |total, container| Usage {
                cpu: Cpu(total.cpu.0.saturating_add(container.usage.cpu.0)),
                memory: Memory(total.memory.0.saturating_add(container.usage.memory.0)),
            })
    }

    /// The budget to compare [`Self::total`] with, or `None` if this sample does not cover
    /// exactly the containers that run alongside each other. That is the case while an init
    /// container runs or before every container reports, when any percentage would compare the
    /// use of some containers with the budget of others.
    pub fn budget_in(&self, spec: &PodSpec) -> Option<PodBudget> {
        let mut sampled: Vec<&str> = self.containers.iter().map(|c| c.name.as_str()).collect();
        let mut expected: Vec<&str> = long_running(spec).map(|c| c.name.as_str()).collect();
        sampled.sort_unstable();
        expected.sort_unstable();
        (!sampled.is_empty() && sampled == expected).then(|| PodBudget::of_pod(spec))
    }

    /// This sample reduced to what is kept per pod between polls.
    pub fn sample(&self) -> PodSample {
        PodSample {
            usage: self.total(),
            containers: container_digest(self.containers.iter().map(|c| c.name.as_str())),
            window_start: self.window_start(),
        }
    }

    /// When the sample's window opened, in Unix seconds, if the server gave a usable window.
    fn window_start(&self) -> Option<i64> {
        let window = self.window.filter(|window| window.is_positive())?;
        Some(self.timestamp?.checked_sub(window).ok()?.as_second())
    }
}

/// A pod's total use, a digest of the containers it covers and when its window opened: a fixed
/// size, whatever the pod's containers are called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PodSample {
    pub usage: Usage,
    containers: u64,
    window_start: Option<i64>,
}

impl PodSample {
    /// Whether this sample can belong to a pod created at `created` (Unix seconds). The metrics
    /// API sends no uid, so a pod recreated under the same name is told apart by its window,
    /// which cannot open before the pod exists. Without both times nothing is proved.
    pub fn follows_creation(&self, created: Option<i64>) -> bool {
        matches!((self.window_start, created), (Some(start), Some(created)) if start >= created)
    }
}

/// A pod spec's budget and the containers it applies to, kept in place of the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PodBudgetCheck {
    budget: PodBudget,
    containers: u64,
}

impl PodBudgetCheck {
    pub fn of(spec: &PodSpec) -> Self {
        Self {
            budget: PodBudget::of_pod(spec),
            containers: container_digest(long_running(spec).map(|c| c.name.as_str())),
        }
    }

    /// The budget to compare `sample` with, under the rule of [`PodMetrics::budget_in`].
    pub fn for_sample(&self, sample: &PodSample) -> Option<PodBudget> {
        (self.containers == sample.containers).then_some(self.budget)
    }
}

/// FNV-1a over the sorted names, each ended by a byte no container name holds.
fn container_digest<'a>(names: impl Iterator<Item = &'a str>) -> u64 {
    let mut names: Vec<&str> = names.collect();
    names.sort_unstable();
    names
        .iter()
        .flat_map(|name| name.bytes().chain([0xff]))
        .fold(0xcbf2_9ce4_8422_2325, |hash: u64, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// A node's `metrics.k8s.io` NodeMetrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeMetrics {
    pub name: String,
    /// When the sample was taken, if the server said in a form that parses.
    pub timestamp: Option<Timestamp>,
    /// The window the sample covers, if the server said in a form that parses.
    pub window: Option<SignedDuration>,
    pub usage: Usage,
}

/// One successful list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsList<T> {
    pub items: Vec<T>,
    /// Items left out because they could not be read or, for a pod, hold no container sample.
    pub skipped: usize,
}

/// Why a fetch returned no metrics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetricsError {
    /// The cluster does not serve `metrics.k8s.io`: hide the columns and show a hint.
    #[error("metrics-server is not installed")]
    NotInstalled,
    /// The API is there, but this identity may not read it.
    #[error("{0}")]
    Forbidden(SafeErrorText),
    /// Anything else: a server error, a timeout, a dropped connection, an unreadable answer.
    #[error("{0}")]
    Failed(SafeErrorText),
}

/// What one fetch gave.
pub type MetricsResult<T> = Result<MetricsList<T>, MetricsError>;

// ---------------------------------------------------------------------------------------------
// Fetching

#[derive(Deserialize)]
struct WireList {
    /// Decoded one by one, so that one unreadable item cannot lose the others.
    items: Vec<serde_json::Value>,
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

impl WirePod {
    /// `None` also for an empty `containers`, which metrics-server returns when it could not
    /// calculate usage: that is a missing sample, not a measured zero.
    fn parse(self) -> Option<PodMetrics> {
        if self.containers.is_empty() {
            return None;
        }
        let containers = self
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
            namespace: self.metadata.namespace.filter(|n| !n.is_empty())?,
            name: self.metadata.name.filter(|n| !n.is_empty())?,
            timestamp: self.timestamp.and_then(|t| t.parse().ok()),
            window: self.window.and_then(|w| w.parse().ok()),
            containers,
        })
    }
}

impl WireNode {
    fn parse(self) -> Option<NodeMetrics> {
        Some(NodeMetrics {
            usage: self.usage.parse()?,
            name: self.metadata.name.filter(|n| !n.is_empty())?,
            timestamp: self.timestamp.and_then(|t| t.parse().ok()),
            window: self.window.and_then(|w| w.parse().ok()),
        })
    }
}

/// Lists PodMetrics across all namespaces (`None`) or in one, with a single `GET`.
pub async fn fetch_pod_metrics(
    client: &Client,
    namespace: Option<&str>,
) -> MetricsResult<PodMetrics> {
    fetch_pod_metrics_within(client, namespace, FETCH_TIMEOUT).await
}

/// [`fetch_pod_metrics`] with `limit` over all its requests.
async fn fetch_pod_metrics_within(
    client: &Client,
    namespace: Option<&str>,
    limit: Duration,
) -> MetricsResult<PodMetrics> {
    within(limit, read_pod_metrics(client, namespace)).await
}

async fn read_pod_metrics(client: &Client, namespace: Option<&str>) -> MetricsResult<PodMetrics> {
    api_path::validate_names(namespace, None).map_err(|error| failed(&error.to_string()))?;
    let path = match namespace {
        None => format!("{API_PATH}/pods"),
        Some(namespace) => format!("{API_PATH}/namespaces/{namespace}/pods"),
    };
    match list(client, path).await {
        Ok(items) => Ok(collect(items, WirePod::parse)),
        Err(err) if namespace.is_some() && is_not_found(&err) => {
            Err(namespaced_not_found(client, &err).await)
        }
        Err(err) => Err(classify(&err)),
    }
}

/// Lists NodeMetrics with a single `GET`.
pub async fn fetch_node_metrics(client: &Client) -> MetricsResult<NodeMetrics> {
    within(FETCH_TIMEOUT, async {
        match list(client, format!("{API_PATH}/nodes")).await {
            Ok(items) => Ok(collect(items, WireNode::parse)),
            Err(err) => Err(classify(&err)),
        }
    })
    .await
}

/// `fetch`'s result, or a failure once `limit` has passed.
async fn within<T>(
    limit: Duration,
    fetch: impl Future<Output = MetricsResult<T>>,
) -> MetricsResult<T> {
    tokio::time::timeout(limit, fetch)
        .await
        .unwrap_or_else(|_| Err(failed("timed out reading metrics")))
}

/// Why a list failed: from the server, or Flint's own limit on it.
enum ListError {
    Kube(kube::Error),
    Other(MetricsError),
}

/// The items at `path`. metrics.k8s.io does not page, so this asks for everything at once.
async fn list(client: &Client, path: String) -> Result<Vec<serde_json::Value>, ListError> {
    let request = Request::new(path)
        .list(&ListParams::default())
        .map_err(|e| ListError::Other(failed(&e.to_string())))?;
    match client.request::<WireList>(request).await {
        Ok(list) => Ok(list.items),
        Err(err) => Err(ListError::Kube(err)),
    }
}

fn collect<W: DeserializeOwned, T>(
    items: Vec<serde_json::Value>,
    parse: impl Fn(W) -> Option<T>,
) -> MetricsList<T> {
    let total = items.len();
    let items: Vec<T> = items
        .into_iter()
        .filter_map(|item| parse(serde_json::from_value(item).ok()?))
        .collect();
    MetricsList {
        skipped: total - items.len(),
        items,
    }
}

fn is_not_found(err: &ListError) -> bool {
    matches!(err, ListError::Kube(e)
        if ApiError::from_kube(e).is_some_and(|api| api.kind() == ApiErrorKind::NotFound))
}

fn classify(err: &ListError) -> MetricsError {
    match err {
        ListError::Other(err) => err.clone(),
        ListError::Kube(err) => classify_kube(err),
    }
}

fn classify_kube(err: &kube::Error) -> MetricsError {
    match ApiError::from_kube(err).map(|api| api.kind()) {
        Some(ApiErrorKind::NotFound) => MetricsError::NotInstalled,
        Some(ApiErrorKind::Forbidden) => {
            MetricsError::Forbidden(ClusterError::kube_ref(err).summary())
        }
        _ => MetricsError::Failed(ClusterError::kube_ref(err).summary()),
    }
}

/// A 404 for one namespace's pods can mean the namespace is gone while the API is fine, so the
/// API's own resource list decides whether metrics are missing.
async fn namespaced_not_found(client: &Client, err: &ListError) -> MetricsError {
    match client.list_api_group_resources(GROUP_VERSION).await {
        Ok(api) if api.resources.iter().any(|r| r.name == "pods") => match err {
            ListError::Kube(e) => MetricsError::Failed(ClusterError::kube_ref(e).summary()),
            ListError::Other(e) => e.clone(),
        },
        Ok(_) => MetricsError::NotInstalled,
        Err(lookup_err) => classify_kube(&lookup_err),
    }
}

fn failed(text: &str) -> MetricsError {
    MetricsError::Failed(SafeErrorText::reduced(text))
}

// ---------------------------------------------------------------------------------------------
// Polling

/// A fetch repeating in the background for as long as this value is held. Dropping it cancels
/// the fetch in flight and stops the task. The next fetch starts only after the last one ended
/// and the interval passed, so fetches never overlap.
#[derive(Debug)]
pub struct MetricsPoll<R> {
    latest: watch::Receiver<Option<Arc<R>>>,
    task: AbortHandle,
}

impl<R: Send + Sync + 'static> MetricsPoll<R> {
    /// Fetches at once, then `interval` after each fetch ends. Must run inside a tokio runtime.
    pub fn start<F, Fut>(interval: Duration, mut fetch: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
    {
        let (sender, latest) = watch::channel(None);
        let task = tokio::spawn(async move {
            loop {
                let result = fetch().await;
                if sender.send(Some(Arc::new(result))).is_err() {
                    return;
                }
                tokio::time::sleep(interval).await;
            }
        })
        .abort_handle();
        Self { latest, task }
    }

    /// The most recent result, if a fetch has finished.
    pub fn latest(&self) -> Option<Arc<R>> {
        self.latest.borrow().clone()
    }

    /// Waits for a result newer than the last one seen. `false` if the poll has ended.
    pub async fn changed(&mut self) -> bool {
        self.latest.changed().await.is_ok()
    }
}

impl<R> Drop for MetricsPoll<R> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Polls PodMetrics every [`POLL_INTERVAL`] while the returned value is held.
pub fn poll_pod_metrics(
    client: Client,
    namespace: Option<String>,
) -> MetricsPoll<MetricsResult<PodMetrics>> {
    MetricsPoll::start(POLL_INTERVAL, move || {
        let (client, namespace) = (client.clone(), namespace.clone());
        async move { fetch_pod_metrics(&client, namespace.as_deref()).await }
    })
}

/// Polls NodeMetrics every [`POLL_INTERVAL`] while the returned value is held.
pub fn poll_node_metrics(client: Client) -> MetricsPoll<MetricsResult<NodeMetrics>> {
    MetricsPoll::start(POLL_INTERVAL, move || {
        let client = client.clone();
        async move { fetch_node_metrics(&client).await }
    })
}

// ---------------------------------------------------------------------------------------------
// Usage against requests and limits

/// CPU and memory amounts, each possibly not set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Amounts {
    pub cpu: Option<Cpu>,
    pub memory: Option<Memory>,
}

impl Amounts {
    /// The `cpu` and `memory` of a Kubernetes resource list, such as a node's `allocatable`. A
    /// value that does not parse counts as not set.
    pub fn from_quantities(quantities: &BTreeMap<String, Quantity>) -> Self {
        Self {
            cpu: quantities.get("cpu").and_then(|q| q.0.parse().ok()),
            memory: quantities.get("memory").and_then(|q| q.0.parse().ok()),
        }
    }

    fn from_requirements(resources: Option<&ResourceRequirements>) -> (Self, Self) {
        let read = |list: Option<&BTreeMap<String, Quantity>>| {
            list.map(Self::from_quantities).unwrap_or_default()
        };
        (
            read(resources.and_then(|r| r.requests.as_ref())),
            read(resources.and_then(|r| r.limits.as_ref())),
        )
    }
}

/// What a pod or container asked for and may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PodBudget {
    pub requests: Amounts,
    pub limits: Amounts,
}

impl PodBudget {
    /// One container's requests and limits. A zero limit counts as not set, because the kubelet
    /// applies no cap for it.
    pub fn of_container(container: &Container) -> Self {
        let (requests, limits) = Amounts::from_requirements(container.resources.as_ref());
        let limits = Amounts {
            cpu: limits.cpu.filter(|cpu| cpu.nanos() > 0),
            memory: limits.memory.filter(|memory| memory.bytes() > 0),
        };
        Self { requests, limits }
    }

    /// The budget of the containers that run alongside each other, to compare with a pod's
    /// current use: its containers and restartable (sidecar) init containers. Other init
    /// containers have finished by then, and pod overhead is not in container metrics.
    ///
    /// Requests add up over the containers that set one. A limit total exists only if every
    /// container has that limit, since one without is unbounded. Pod-level resources, when set,
    /// replace the total for that resource.
    ///
    /// Private because it only fits a sample of those same containers: see
    /// [`PodMetrics::budget_in`].
    fn of_pod(spec: &PodSpec) -> Self {
        let budgets: Vec<Self> = long_running(spec).map(Self::of_container).collect();
        let (pod_requests, pod_limits) = Amounts::from_requirements(spec.resources.as_ref());
        Self {
            requests: Amounts {
                cpu: pod_requests.cpu.or_else(|| {
                    sum_set(budgets.iter().map(|b| b.requests.cpu.map(Cpu::nanos))).map(Cpu)
                }),
                memory: pod_requests.memory.or_else(|| {
                    sum_set(budgets.iter().map(|b| b.requests.memory.map(Memory::bytes)))
                        .map(Memory)
                }),
            },
            limits: Amounts {
                cpu: pod_limits.cpu.or_else(|| {
                    sum_all(budgets.iter().map(|b| b.limits.cpu.map(Cpu::nanos))).map(Cpu)
                }),
                memory: pod_limits.memory.or_else(|| {
                    sum_all(budgets.iter().map(|b| b.limits.memory.map(Memory::bytes))).map(Memory)
                }),
            },
        }
    }
}

/// The containers that run alongside each other once a pod has started: its containers and
/// restartable (sidecar) init containers.
fn long_running(spec: &PodSpec) -> impl Iterator<Item = &Container> {
    let sidecars = spec
        .init_containers
        .iter()
        .flatten()
        .filter(|c| c.restart_policy.as_deref() == Some("Always"));
    spec.containers.iter().chain(sidecars)
}

/// The sum of the values that are set, or `None` if none is.
fn sum_set(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.flatten().reduce(u64::saturating_add)
}

/// The sum of the values, or `None` if there are none or any is not set.
fn sum_all(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.reduce(|a, b| Some(a?.saturating_add(b?))).flatten()
}

/// Use as a share of a request, limit or capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Share {
    /// There is nothing to compare with.
    NotSet,
    /// The amount is explicitly zero, so no percentage exists.
    ZeroBudget,
    /// Rounded to the nearest percent. Above 100 when use exceeds the amount.
    Percent(u32),
}

/// CPU and memory [`Share`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageShares {
    pub cpu: Share,
    pub memory: Share,
}

/// `used` as a share of `budget`.
pub fn share(used: u64, budget: Option<u64>) -> Share {
    match budget {
        None => Share::NotSet,
        Some(0) => Share::ZeroBudget,
        Some(budget) => {
            let (used, budget) = (u128::from(used), u128::from(budget));
            let percent = (used * 200 + budget) / (budget * 2);
            Share::Percent(u32::try_from(percent).unwrap_or(u32::MAX))
        }
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
