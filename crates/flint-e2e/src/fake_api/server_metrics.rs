//! What metrics.k8s.io reports: tests set a pod's or node's usage, or queue samples that each
//! read of it takes in turn. Samples change the Table cells and the plain `PodMetrics` or
//! `NodeMetrics` object. The API has no resource versions or watches, so nothing else moves.

use serde_json::json;

use super::*;

const GROUP: &str = "metrics.k8s.io";
const VERSION: &str = "v1beta1";
const WINDOW: &str = "10s";

/// Samples still to be served, by target.
pub(super) type Sequences = HashMap<MetricsTarget, VecDeque<MetricsSample>>;

/// What a metric is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MetricsTarget {
    Pod { namespace: String, name: String },
    Node { name: String },
}

impl MetricsTarget {
    pub fn pod(namespace: &str, name: &str) -> Self {
        MetricsTarget::Pod {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
        }
    }

    pub fn node(name: &str) -> Self {
        MetricsTarget::Node {
            name: name.to_owned(),
        }
    }

    fn gvr(&self) -> Gvr {
        let resource = match self {
            MetricsTarget::Pod { .. } => "pods",
            MetricsTarget::Node { .. } => "nodes",
        };
        Gvr::new(GROUP, VERSION, resource)
    }

    fn namespace(&self) -> Option<&str> {
        match self {
            MetricsTarget::Pod { namespace, .. } => Some(namespace),
            MetricsTarget::Node { .. } => None,
        }
    }

    fn name(&self) -> &str {
        match self {
            MetricsTarget::Pod { name, .. } | MetricsTarget::Node { name } => name,
        }
    }

    /// Whether a list or get on `route` reads this target. A get names its exact scope; a list
    /// without a namespace reads all of them.
    fn read_by(&self, route: &Route) -> bool {
        let in_scope = match route.name() {
            Some(name) => name == self.name() && route.namespace() == self.namespace(),
            None => route
                .namespace()
                .is_none_or(|ns| self.namespace() == Some(ns)),
        };
        route.gvr() == Some(&self.gvr()) && in_scope
    }
}

/// One reading, in Kubernetes quantities (`"250m"`, `"64Mi"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSample {
    pub cpu: String,
    pub memory: String,
}

impl MetricsSample {
    pub fn new(cpu: &str, memory: &str) -> Self {
        MetricsSample {
            cpu: cpu.to_owned(),
            memory: memory.to_owned(),
        }
    }
}

pub(super) fn is_metrics(gvr: &Gvr) -> bool {
    gvr.group == GROUP
}

/// Aggregated discovery of `/apis` without metrics.k8s.io.
pub(super) fn without_metrics(discovery: &Value) -> Value {
    let mut discovery = discovery.clone();
    if let Some(items) = discovery.get_mut("items").and_then(Value::as_array_mut) {
        items
            .retain(|group| group.pointer("/metadata/name").and_then(Value::as_str) != Some(GROUP));
    }
    discovery
}

/// For a read of metrics that passed validation: each target it returns takes its next queued
/// sample, and the last one stays once the queue is empty. The caller holds `state` until it has
/// built the response, so overlapping reads each serve their own sample.
pub(super) fn advance(state: &mut State, inner: &Inner, route: &Route, query: &Query, table: bool) {
    if !route.gvr().is_some_and(is_metrics) {
        return;
    }
    let mut sequences = lock(&inner.metrics);
    for (target, samples) in sequences.iter_mut() {
        let returned = target.read_by(route)
            && state.metrics_selected(
                &target.gvr(),
                target.namespace(),
                target.name(),
                query,
                table,
            );
        if returned && let Some(sample) = samples.pop_front() {
            store(state, &inner.clock, target, &sample).unwrap_or_else(|e| panic!("{e}"));
        }
    }
}

/// Writes a sample to the target's row and object, making them if there are none.
fn store(
    state: &mut State,
    clock: &Clock,
    target: &MetricsTarget,
    sample: &MetricsSample,
) -> Result<(), String> {
    let gvr = target.gvr();
    let (namespace, name) = (target.namespace(), target.name());
    let cells = [
        ("cpu", sample.cpu.as_str()),
        ("memory", sample.memory.as_str()),
        ("Window", WINDOW),
    ];
    let columns = state.columns(&gvr);
    let index = |column: &str| {
        columns
            .as_array()
            .and_then(|all| all.iter().position(|c| c["name"] == column))
            .ok_or_else(|| format!("{gvr} has no column {column:?}"))
    };
    let row = match state.row(&gvr, namespace, name) {
        Some(row) => row
            .with_cell(index("cpu")?, sample.cpu.as_str())
            .with_cell(index("memory")?, sample.memory.as_str())
            .with_cell(index("Window")?, WINDOW),
        None => state.new_row(&gvr, namespace, name, &cells)?,
    };
    let object = state.metrics_object(
        &gvr,
        &row,
        (&sample.cpu, &sample.memory),
        WINDOW,
        &clock.fixture_now(),
    );
    state.put_metrics(&gvr, row, object);
    Ok(())
}

/// The `APIResourceList` of `metrics.k8s.io/v1beta1`, which clients read to tell a missing API
/// from a missing namespace. `None` if the server serves none of it.
pub(super) fn api_resources(inner: &Inner) -> Option<Value> {
    let resources: Vec<Value> = inner
        .fixtures
        .resources
        .iter()
        .filter(|r| {
            is_metrics(&r.gvr) && r.gvr.version == VERSION && !inner.options.hidden.contains(&r.gvr)
        })
        .map(|r| {
            json!({
                "name": r.gvr.resource,
                "singularName": "",
                "namespaced": r.namespaced,
                "kind": r.kind,
                "verbs": r.verbs,
            })
        })
        .collect();
    (!inner.options.no_metrics && !resources.is_empty()).then(|| {
        json!({
            "kind": "APIResourceList",
            "apiVersion": "v1",
            "groupVersion": format!("{GROUP}/{VERSION}"),
            "resources": resources,
        })
    })
}

impl FakeApiServer {
    fn require_metrics(&self) {
        assert!(
            !self.inner.options.no_metrics,
            "this fake API server was started with Options::no_metrics"
        );
    }

    /// Sets what metrics.k8s.io reports for `target` from now on, dropping any queued samples.
    /// A target without metrics yet gets them.
    pub fn set_metrics(&self, target: &MetricsTarget, cpu: &str, memory: &str) {
        self.require_metrics();
        let mut state = lock(&self.inner.state);
        lock(&self.inner.metrics).remove(target);
        store(
            &mut state,
            &self.inner.clock,
            target,
            &MetricsSample::new(cpu, memory),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Queues samples for `target`, replacing those queued before: each list or get that reads
    /// it serves the next one, and the last one stays.
    pub fn script_metrics(&self, target: &MetricsTarget, samples: Vec<MetricsSample>) {
        self.require_metrics();
        let _state = lock(&self.inner.state);
        lock(&self.inner.metrics).insert(target.clone(), samples.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "application/json;as=Table;v=v1;g=meta.k8s.io, application/json";

    async fn fetch(client: &kube::Client, path: &str, accept: &str) -> Result<Value, kube::Error> {
        let request = http::Request::get(path)
            .header(http::header::ACCEPT, accept)
            .body(Vec::new())
            .unwrap();
        client.request::<Value>(request).await
    }

    /// The `(cpu, memory)` cells of the row called `name`.
    fn usage(table: &Value, name: &str) -> (String, String) {
        let column = |wanted: &str| {
            table["columnDefinitions"]
                .as_array()
                .unwrap()
                .iter()
                .position(|c| c["name"] == wanted)
                .unwrap()
        };
        let row = table["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["object"]["metadata"]["name"] == name)
            .unwrap();
        let cell = |wanted: &str| row["cells"][column(wanted)].as_str().unwrap().to_owned();
        (cell("cpu"), cell("memory"))
    }

    fn pair(cpu: &str, memory: &str) -> (String, String) {
        (cpu.to_owned(), memory.to_owned())
    }

    #[tokio::test]
    async fn set_metrics_changes_the_table_and_the_object_of_a_pod_and_a_node() {
        let server = FakeApiServer::start().await.unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let version = lock(&server.inner.state).resource_version();
        server.set_metrics(&MetricsTarget::pod("demo", "fixture-db-0"), "250m", "64Mi");
        server.set_metrics(&MetricsTarget::pod("demo", "brand-new"), "1m", "2Mi");
        server.set_metrics(&MetricsTarget::node("flint-dev-control-plane"), "2", "4Gi");
        let pods = "/apis/metrics.k8s.io/v1beta1/pods";
        let table = fetch(&client, pods, TABLE).await.unwrap();
        assert_eq!(usage(&table, "fixture-db-0"), pair("250m", "64Mi"));
        assert_eq!(usage(&table, "brand-new"), pair("1m", "2Mi"));
        let plain = fetch(
            &client,
            "/apis/metrics.k8s.io/v1beta1/namespaces/demo/pods/fixture-db-0",
            "application/json",
        )
        .await
        .unwrap();
        assert_eq!(plain["kind"], "PodMetrics");
        assert_eq!(plain["containers"][0]["usage"]["cpu"], "250m");
        let node = fetch(
            &client,
            "/apis/metrics.k8s.io/v1beta1/nodes/flint-dev-control-plane",
            "application/json",
        )
        .await
        .unwrap();
        assert_eq!(node["kind"], "NodeMetrics");
        assert_eq!(node["usage"]["memory"], "4Gi");
        assert_eq!(
            lock(&server.inner.state).resource_version(),
            version,
            "metrics do not move resource versions, or watches of other kinds would expire"
        );
    }

    #[tokio::test]
    async fn queued_samples_are_served_one_per_read_and_the_last_stays() {
        let server = FakeApiServer::start().await.unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let db = MetricsTarget::pod("demo", "fixture-db-0");
        server.script_metrics(
            &db,
            vec![
                MetricsSample::new("1m", "1Mi"),
                MetricsSample::new("2m", "2Mi"),
            ],
        );
        let pods = "/apis/metrics.k8s.io/v1beta1/namespaces/demo/pods";
        for (cpu, memory) in [("1m", "1Mi"), ("2m", "2Mi"), ("2m", "2Mi")] {
            let table = fetch(&client, pods, TABLE).await.unwrap();
            assert_eq!(usage(&table, "fixture-db-0"), pair(cpu, memory));
        }
        // A read of something else takes nothing from the queue.
        server.script_metrics(&db, vec![MetricsSample::new("3m", "3Mi")]);
        fetch(&client, "/apis/metrics.k8s.io/v1beta1/nodes", TABLE)
            .await
            .unwrap();
        fetch(
            &client,
            "/apis/metrics.k8s.io/v1beta1/namespaces/other/pods",
            TABLE,
        )
        .await
        .unwrap();
        assert_eq!(lock(&server.inner.metrics)[&db].len(), 1);
        // Setting a value drops what was queued.
        server.set_metrics(&db, "9m", "9Mi");
        let table = fetch(&client, pods, TABLE).await.unwrap();
        assert_eq!(usage(&table, "fixture-db-0"), pair("9m", "9Mi"));
    }

    #[tokio::test]
    async fn only_a_valid_read_takes_a_sample_and_only_for_what_it_returns() {
        let server = FakeApiServer::start().await.unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let db = MetricsTarget::pod("demo", "fixture-db-0");
        let other = MetricsTarget::pod("demo", "scripted-other");
        let queued = |target: &MetricsTarget| lock(&server.inner.metrics)[target].len();
        for target in [&db, &other] {
            server.set_metrics(target, "1m", "1Mi");
            server.script_metrics(
                target,
                vec![
                    MetricsSample::new("2m", "2Mi"),
                    MetricsSample::new("3m", "3Mi"),
                ],
            );
        }
        let pods = "/apis/metrics.k8s.io/v1beta1/namespaces/demo/pods";

        let filtered = format!("{pods}?fieldSelector=metadata.name%3Dfixture-db-0");
        let table = fetch(&client, &filtered, TABLE).await.unwrap();
        assert_eq!(usage(&table, "fixture-db-0"), pair("2m", "2Mi"));
        assert_eq!((queued(&db), queued(&other)), (1, 2));

        // Refused by the Table's field check, and a get of a name it does not serve.
        let refused = format!("{pods}?fieldSelector=status.unsupported%3Dx");
        fetch(&client, &refused, TABLE).await.unwrap_err();
        let cluster_wide = "/apis/metrics.k8s.io/v1beta1/pods/fixture-db-0";
        fetch(&client, cluster_wide, "application/json")
            .await
            .unwrap_err();
        assert_eq!((queued(&db), queued(&other)), (1, 2));

        let plain = format!("{pods}?fieldSelector=metadata.name%3Dscripted-other");
        let list = fetch(&client, &plain, "application/json").await.unwrap();
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        assert_eq!(list["items"][0]["containers"][0]["usage"]["cpu"], "2m");
        assert_eq!((queued(&db), queued(&other)), (1, 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_reads_each_serve_their_own_sample() {
        const READS: usize = 48;
        let server = FakeApiServer::start().await.unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let db = MetricsTarget::pod("demo", "fixture-db-0");
        let samples = (0..READS)
            .map(|i| MetricsSample::new(&format!("{i}m"), "1Mi"))
            .collect();
        server.script_metrics(&db, samples);
        let path = "/apis/metrics.k8s.io/v1beta1/namespaces/demo/pods/fixture-db-0";
        let reads: Vec<_> = (0..READS)
            .map(|_| {
                let client = client.clone();
                tokio::spawn(async move { fetch(&client, path, "application/json").await })
            })
            .collect();
        let mut served = std::collections::BTreeSet::new();
        for read in reads {
            let body = read.await.unwrap().unwrap();
            served.insert(
                body["containers"][0]["usage"]["cpu"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        assert_eq!(served.len(), READS, "a sample was served twice: {served:?}");
    }

    #[tokio::test]
    async fn an_updated_sample_reports_one_window_in_both_formats() {
        let server = FakeApiServer::start().await.unwrap();
        let client = server.unrestricted_client().await.unwrap();
        server.set_metrics(&MetricsTarget::pod("demo", "fixture-db-0"), "5m", "5Mi");
        let path = "/apis/metrics.k8s.io/v1beta1/namespaces/demo/pods/fixture-db-0";
        let table = fetch(&client, path, TABLE).await.unwrap();
        let window = table["columnDefinitions"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c["name"] == "Window")
            .unwrap();
        let plain = fetch(&client, path, "application/json").await.unwrap();
        assert_eq!(table["rows"][0]["cells"][window], WINDOW);
        assert_eq!(plain["window"], WINDOW);
    }

    #[tokio::test]
    async fn no_metrics_drops_the_group_from_discovery_and_its_paths() {
        let server = FakeApiServer::start_with(Options {
            no_metrics: true,
            ..Options::default()
        })
        .await
        .unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let groups = client.list_api_groups_aggregated().await.unwrap();
        let names: Vec<_> = groups
            .items
            .iter()
            .filter_map(|g| g.metadata.as_ref()?.name.as_deref())
            .collect();
        assert!(
            names.contains(&"apps") && !names.contains(&GROUP),
            "{names:?}"
        );
        let err = fetch(&client, "/apis/metrics.k8s.io/v1beta1/pods", TABLE)
            .await
            .unwrap_err();
        assert!(matches!(err, kube::Error::Api(status) if status.code == 404));
        fetch(&client, "/api/v1/pods", TABLE).await.unwrap();
    }

    #[tokio::test]
    async fn a_plain_list_gives_recorded_rows_their_usage() {
        let server = FakeApiServer::start().await.unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let nodes = fetch(
            &client,
            "/apis/metrics.k8s.io/v1beta1/nodes",
            "application/json",
        )
        .await
        .unwrap();
        let node = &nodes["items"][0];
        assert_eq!(nodes["kind"], "NodeMetricsList");
        assert_eq!(node["metadata"]["name"], "flint-dev-control-plane");
        assert_eq!(node["usage"]["cpu"], "146036024n");
        assert_eq!(node["usage"]["memory"], "1146616Ki");
        assert_eq!(node["window"], "10.021s");
        assert!(node["timestamp"].as_str().unwrap().ends_with('Z'));

        let pods = fetch(
            &client,
            "/apis/metrics.k8s.io/v1beta1/namespaces/demo/pods",
            "application/json",
        )
        .await
        .unwrap();
        let db = pods["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["metadata"]["name"] == "fixture-db-0")
            .unwrap();
        assert_eq!(db["metadata"]["namespace"], "demo");
        assert!(db["containers"][0]["name"].is_string());
        assert_eq!(db["containers"][0]["usage"]["cpu"], "0");
        assert_eq!(db["containers"][0]["usage"]["memory"], "552Ki");
    }

    #[tokio::test]
    async fn the_group_version_lists_its_resources_unless_metrics_are_off() {
        let path = "/apis/metrics.k8s.io/v1beta1";
        let server = FakeApiServer::start().await.unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let api = client
            .list_api_group_resources("metrics.k8s.io/v1beta1")
            .await
            .unwrap();
        let names: Vec<_> = api.resources.iter().map(|r| r.name.as_str()).collect();
        assert!(
            names.contains(&"pods") && names.contains(&"nodes"),
            "{names:?}"
        );

        let hidden = FakeApiServer::start_with(Options {
            hidden: vec![Gvr::new(GROUP, VERSION, "pods")],
            ..Options::default()
        })
        .await
        .unwrap();
        let client = hidden.unrestricted_client().await.unwrap();
        let api = client
            .list_api_group_resources("metrics.k8s.io/v1beta1")
            .await
            .unwrap();
        assert!(api.resources.iter().all(|r| r.name != "pods"));

        let off = FakeApiServer::start_with(Options {
            no_metrics: true,
            ..Options::default()
        })
        .await
        .unwrap();
        let client = off.unrestricted_client().await.unwrap();
        let err = fetch(&client, path, "application/json").await.unwrap_err();
        assert!(matches!(err, kube::Error::Api(status) if status.code == 404));
    }

    #[tokio::test]
    #[should_panic(expected = "no_metrics")]
    async fn setting_metrics_on_a_server_without_them_is_a_test_bug() {
        let server = FakeApiServer::start_with(Options {
            no_metrics: true,
            ..Options::default()
        })
        .await
        .unwrap();
        server.set_metrics(&MetricsTarget::node("n"), "1", "1Mi");
    }
}
