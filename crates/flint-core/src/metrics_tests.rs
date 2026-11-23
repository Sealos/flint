//! Tests of [`super`]: pure parsing and shares, a local stub API server for the fetches, and
//! paused time for the poll. Nothing leaves the machine.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use k8s_openapi::api::core::v1::{Container, PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::config::Kubeconfig;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::*;
use crate::cluster;

fn cpu(text: &str) -> u64 {
    text.parse::<Cpu>().unwrap().nanos()
}

fn memory(text: &str) -> u64 {
    text.parse::<Memory>().unwrap().bytes()
}

#[test]
fn cpu_quantities_parse_to_nanocores() {
    for (text, nanos) in [
        ("0", 0),
        ("1", 1_000_000_000),
        ("250m", 250_000_000),
        ("1.5", 1_500_000_000),
        (".5", 500_000_000),
        ("5.", 5_000_000_000),
        ("+2", 2_000_000_000),
        ("12345n", 12_345),
        ("7u", 7_000),
        ("1e-3", 1_000_000),
        ("1E3", 1_000_000_000_000),
        ("1k", 1_000_000_000_000),
    ] {
        assert_eq!(cpu(text), nanos, "{text}");
    }
}

#[test]
fn memory_quantities_parse_to_bytes() {
    for (text, bytes) in [
        ("0", 0),
        ("1024", 1024),
        ("1Ki", 1024),
        ("1.5Ki", 1536),
        ("128Mi", 134_217_728),
        ("2Gi", 2_147_483_648),
        ("1Ti", 1 << 40),
        ("1Pi", 1 << 50),
        ("1Ei", 1 << 60),
        ("1k", 1000),
        ("1M", 1_000_000),
        ("1G", 1_000_000_000),
        ("1T", 1_000_000_000_000),
        ("1P", 1_000_000_000_000_000),
        ("1E", 1_000_000_000_000_000_000),
        ("1e3", 1000),
        ("128974848000m", 128_974_848),
    ] {
        assert_eq!(memory(text), bytes, "{text}");
    }
}

#[test]
fn too_precise_values_round_up() {
    assert_eq!(memory("1m"), 1);
    assert_eq!(memory("1500m"), 2);
    assert_eq!(memory("1e-39"), 1);
    assert_eq!(memory("0e-39"), 0);
    assert_eq!(cpu("0.1n"), 1);
    assert_eq!(Cpu::from_nanos(1).millis(), 1);
    assert_eq!(Cpu::from_nanos(2_000_000).millis(), 2);
    assert_eq!(Cpu::from_nanos(0).millis(), 0);
}

#[test]
fn bad_quantities_are_refused() {
    for text in [
        "",
        ".",
        "-1",
        "1x",
        "1 Gi",
        "1.2.3",
        "Gi",
        "1Gib",
        "1e",
        "1e999",
        "e3",
        "NaN",
        // Above u64.
        "99999999999Ei",
        "340282366920938463463374607431768211456",
    ] {
        assert_eq!(text.parse::<Memory>(), Err(InvalidQuantity), "{text:?}");
    }
    // Fits as bytes, not as nanocores.
    assert!("99999999999".parse::<Memory>().is_ok());
    assert_eq!("99999999999".parse::<Cpu>(), Err(InvalidQuantity));
}

#[test]
fn share_covers_missing_zero_and_over() {
    assert_eq!(share(5, None), Share::NotSet);
    assert_eq!(share(0, None), Share::NotSet);
    assert_eq!(share(5, Some(0)), Share::ZeroBudget);
    assert_eq!(share(0, Some(0)), Share::ZeroBudget);
    assert_eq!(share(0, Some(10)), Share::Percent(0));
    assert_eq!(share(5, Some(10)), Share::Percent(50));
    assert_eq!(share(10, Some(10)), Share::Percent(100));
    assert_eq!(share(25, Some(10)), Share::Percent(250));
    // Nearest percent.
    assert_eq!(share(1, Some(3)), Share::Percent(33));
    assert_eq!(share(2, Some(3)), Share::Percent(67));
    assert_eq!(share(1, Some(200)), Share::Percent(1));
    assert_eq!(share(1, Some(201)), Share::Percent(0));
    // Neither overflows.
    assert_eq!(share(u64::MAX, Some(1)), Share::Percent(u32::MAX));
    assert_eq!(share(u64::MAX, Some(u64::MAX)), Share::Percent(100));
}

fn quantities(cpu: Option<&str>, memory: Option<&str>) -> Option<BTreeMap<String, Quantity>> {
    let list: BTreeMap<_, _> = [("cpu", cpu), ("memory", memory)]
        .into_iter()
        .filter_map(|(k, v)| Some((k.to_owned(), Quantity(v?.to_owned()))))
        .collect();
    (!list.is_empty()).then_some(list)
}

fn resources(
    requests: (Option<&str>, Option<&str>),
    limits: (Option<&str>, Option<&str>),
) -> ResourceRequirements {
    ResourceRequirements {
        requests: quantities(requests.0, requests.1),
        limits: quantities(limits.0, limits.1),
        ..Default::default()
    }
}

fn container(
    requests: (Option<&str>, Option<&str>),
    limits: (Option<&str>, Option<&str>),
) -> Container {
    Container {
        name: "c".to_owned(),
        resources: Some(resources(requests, limits)),
        ..Default::default()
    }
}

#[test]
fn a_pod_budget_adds_requests_and_needs_every_limit() {
    let spec = PodSpec {
        containers: vec![
            container((Some("100m"), Some("1Mi")), (Some("1"), None)),
            container((None, Some("1Mi")), (Some("1"), Some("4Mi"))),
        ],
        ..Default::default()
    };
    let budget = PodBudget::of_pod(&spec);
    assert_eq!(budget.requests.cpu, Some(Cpu::from_nanos(100_000_000)));
    assert_eq!(budget.requests.memory, Some(Memory::from_bytes(2 << 20)));
    assert_eq!(budget.limits.cpu, Some(Cpu::from_nanos(2_000_000_000)));
    // The first container has no memory limit, so the pod has none.
    assert_eq!(budget.limits.memory, None);
}

#[test]
fn a_pod_budget_counts_sidecars_but_not_finished_init_containers() {
    let mut sidecar = container((Some("100m"), None), (Some("1"), None));
    sidecar.restart_policy = Some("Always".to_owned());
    let spec = PodSpec {
        containers: vec![container((Some("200m"), None), (Some("1"), None))],
        init_containers: Some(vec![
            sidecar,
            container((Some("4"), None), (Some("8"), None)),
        ]),
        ..Default::default()
    };
    let budget = PodBudget::of_pod(&spec);
    assert_eq!(budget.requests.cpu, Some(Cpu::from_nanos(300_000_000)));
    assert_eq!(budget.limits.cpu, Some(Cpu::from_nanos(2_000_000_000)));
}

#[test]
fn pod_level_resources_replace_the_total_per_resource() {
    let spec = PodSpec {
        containers: vec![container(
            (Some("100m"), Some("1Mi")),
            (Some("1"), Some("2Mi")),
        )],
        resources: Some(resources((Some("500m"), None), (None, Some("8Mi")))),
        ..Default::default()
    };
    let budget = PodBudget::of_pod(&spec);
    assert_eq!(budget.requests.cpu, Some(Cpu::from_nanos(500_000_000)));
    assert_eq!(budget.requests.memory, Some(Memory::from_bytes(1 << 20)));
    assert_eq!(budget.limits.cpu, Some(Cpu::from_nanos(1_000_000_000)));
    assert_eq!(budget.limits.memory, Some(Memory::from_bytes(8 << 20)));
}

#[test]
fn a_pod_with_nothing_set_has_an_empty_budget() {
    let spec = PodSpec {
        containers: vec![Container::default()],
        ..Default::default()
    };
    assert_eq!(PodBudget::of_pod(&spec), PodBudget::default());
    assert_eq!(PodBudget::of_pod(&PodSpec::default()), PodBudget::default());
}

#[test]
fn an_unreadable_amount_counts_as_not_set() {
    let amounts = Amounts::from_quantities(&quantities(Some("lots"), Some("1Ki")).unwrap());
    assert_eq!(amounts.cpu, None);
    assert_eq!(amounts.memory, Some(Memory::from_bytes(1024)));
}

#[test]
fn usage_against_a_budget_and_pod_total() {
    let pod = PodMetrics {
        namespace: "demo".to_owned(),
        name: "web".to_owned(),
        timestamp: None,
        window: None,
        containers: ["100m", "150m"]
            .into_iter()
            .map(|c| ContainerMetrics {
                name: c.to_owned(),
                usage: Usage {
                    cpu: c.parse().unwrap(),
                    memory: "1Mi".parse().unwrap(),
                },
            })
            .collect(),
    };
    let total = pod.total();
    assert_eq!(total.cpu.millis(), 250);
    assert_eq!(total.memory.bytes(), 2 << 20);
    let shares = total.against(&Amounts {
        cpu: Some("500m".parse().unwrap()),
        memory: None,
    });
    assert_eq!(shares.cpu, Share::Percent(50));
    assert_eq!(shares.memory, Share::NotSet);
}

// ---------------------------------------------------------------------------------------------
// Fetching, against a local stub

type Route = Box<dyn Fn(&str) -> (u16, String) + Send + Sync>;

/// A local API server answering by path, recording each request as `"METHOD /path"`.
async fn stub(route: Route) -> (Client, Arc<Mutex<Vec<String>>>) {
    stub_after(Duration::ZERO, route).await
}

/// [`stub`], taking `delay` to answer each request.
async fn stub_after(delay: Duration, route: Route) -> (Client, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let route = Arc::new(route);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (log, route) = (log.clone(), route.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head);
                let mut line = head.lines().next().unwrap_or("").split(' ');
                let (method, target) = (line.next().unwrap_or(""), line.next().unwrap_or(""));
                let path = target.split('?').next().unwrap_or("");
                log.lock().unwrap().push(format!("{method} {path}"));
                tokio::time::sleep(delay).await;
                let (code, body) = route(path);
                let response = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    let kubeconfig = Kubeconfig::from_yaml(&format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {url}\nusers:\n- name: u\n  user:\n    token: t\ncontexts:\n- name: ctx\n  context:\n    cluster: c\n    user: u\n"
    ))
    .unwrap();
    // The read-only client the app uses, so these also prove its path rule allows the fetches.
    let client = cluster::connect(&kubeconfig, "ctx").await.unwrap();
    (client, seen)
}

fn status(code: u16, reason: &str, message: &str) -> (u16, String) {
    (
        code,
        json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
               "reason": reason, "message": message, "code": code})
        .to_string(),
    )
}

fn pod_list() -> String {
    json!({
        "kind": "PodMetricsList",
        "apiVersion": GROUP_VERSION,
        "metadata": {},
        "items": [
            {
                "metadata": {"name": "web", "namespace": "demo"},
                "timestamp": "2026-11-22T10:00:00Z",
                "window": "15.008s",
                "containers": [
                    {"name": "app", "usage": {"cpu": "12345678n", "memory": "1024Ki"}},
                    {"name": "proxy", "usage": {"cpu": "1m", "memory": "1Mi"}}
                ]
            },
            {
                "metadata": {"name": "bad", "namespace": "demo"},
                "containers": [{"name": "app", "usage": {"cpu": "lots", "memory": "1Mi"}}]
            },
            {
                "metadata": {"name": "partial", "namespace": "demo"},
                "containers": [{"name": "app", "usage": {"cpu": "1m"}}]
            },
            {
                "metadata": {"name": "odd-times", "namespace": "demo"},
                "timestamp": "yesterday",
                "window": "a while",
                "containers": [{"name": "app", "usage": {"cpu": "1m", "memory": "1Mi"}}]
            }
        ]
    })
    .to_string()
}

#[tokio::test]
async fn pod_metrics_are_read_with_one_get_and_bad_items_are_counted() {
    let (client, seen) = stub(Box::new(|_| (200, pod_list()))).await;
    let list = fetch_pod_metrics(&client, None).await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        ["GET /apis/metrics.k8s.io/v1beta1/pods"]
    );
    assert_eq!(list.skipped, 2);
    assert_eq!(list.items.len(), 2);
    let web = &list.items[0];
    assert_eq!((web.namespace.as_str(), web.name.as_str()), ("demo", "web"));
    assert_eq!(web.timestamp, Some("2026-11-22T10:00:00Z".parse().unwrap()));
    assert_eq!(web.window, Some(SignedDuration::from_millis(15_008)));
    assert_eq!(web.containers[0].name, "app");
    assert_eq!(web.containers[0].usage.cpu.nanos(), 12_345_678);
    assert_eq!(web.containers[1].usage.memory.bytes(), 1 << 20);
    let odd = &list.items[1];
    assert_eq!((odd.timestamp, odd.window), (None, None));
}

#[tokio::test]
async fn a_go_style_window_parses() {
    assert_eq!(
        "1m0.5s".parse::<SignedDuration>().ok(),
        Some(SignedDuration::from_millis(60_500))
    );
}

#[tokio::test]
async fn namespaced_pod_metrics_use_the_namespace_path() {
    let (client, seen) = stub(Box::new(|_| (200, pod_list()))).await;
    fetch_pod_metrics(&client, Some("demo")).await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        ["GET /apis/metrics.k8s.io/v1beta1/namespaces/demo/pods"]
    );
}

#[tokio::test]
async fn a_namespace_that_could_change_the_path_is_never_sent() {
    let (client, seen) = stub(Box::new(|_| (200, pod_list()))).await;
    for bad in ["", "a/b", "..", "a?b=c", "A", "-a", "a-", &"a".repeat(64)] {
        let err = fetch_pod_metrics(&client, Some(bad)).await.unwrap_err();
        assert!(matches!(err, MetricsError::Failed(_)), "{bad}");
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn node_metrics_are_read_with_one_get() {
    let body = json!({
        "kind": "NodeMetricsList",
        "items": [
            {"metadata": {"name": "worker-1"}, "timestamp": "2026-11-22T10:00:00Z",
             "window": "20s", "usage": {"cpu": "2", "memory": "4Gi"}},
            {"metadata": {"name": "worker-2"}, "usage": {"cpu": "x", "memory": "4Gi"}}
        ]
    })
    .to_string();
    let (client, seen) = stub(Box::new(move |_| (200, body.clone()))).await;
    let list = fetch_node_metrics(&client).await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        ["GET /apis/metrics.k8s.io/v1beta1/nodes"]
    );
    assert_eq!(list.skipped, 1);
    assert_eq!(list.items[0].name, "worker-1");
    assert_eq!(list.items[0].usage.cpu.nanos(), 2_000_000_000);
    assert_eq!(list.items[0].usage.memory.bytes(), 4 << 30);
    assert_eq!(list.items[0].window, Some(SignedDuration::from_secs(20)));
}

#[tokio::test]
async fn a_404_means_not_installed() {
    let (client, _) = stub(Box::new(|_| {
        status(
            404,
            "NotFound",
            "the server could not find the requested resource",
        )
    }))
    .await;
    assert_eq!(
        fetch_pod_metrics(&client, None).await,
        Err(MetricsError::NotInstalled)
    );
    assert_eq!(
        fetch_node_metrics(&client).await,
        Err(MetricsError::NotInstalled)
    );
    assert_eq!(
        fetch_pod_metrics(&client, Some("demo")).await,
        Err(MetricsError::NotInstalled)
    );
}

#[tokio::test]
async fn a_404_for_a_missing_namespace_is_not_not_installed() {
    let (client, seen) = stub(Box::new(|path| {
        if path == API_PATH {
            let body = json!({
                "kind": "APIResourceList",
                "groupVersion": GROUP_VERSION,
                "resources": [{"name": "pods", "singularName": "", "namespaced": true,
                               "kind": "PodMetrics", "verbs": ["get", "list"]}]
            });
            (200, body.to_string())
        } else {
            status(404, "NotFound", "namespaces \"gone\" not found")
        }
    }))
    .await;
    let err = fetch_pod_metrics(&client, Some("gone")).await.unwrap_err();
    assert!(matches!(err, MetricsError::Failed(_)), "{err:?}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|r| r.starts_with("GET ")), "{seen:?}");
}

#[tokio::test]
async fn a_group_without_pods_means_not_installed() {
    let (client, _) = stub(Box::new(|path| {
        if path == API_PATH {
            let body = json!({"kind": "APIResourceList", "groupVersion": GROUP_VERSION,
                              "resources": []});
            (200, body.to_string())
        } else {
            status(404, "NotFound", "not found")
        }
    }))
    .await;
    assert_eq!(
        fetch_pod_metrics(&client, Some("demo")).await,
        Err(MetricsError::NotInstalled)
    );
}

#[tokio::test]
async fn a_403_is_forbidden_with_safe_text() {
    let (client, _) = stub(Box::new(|_| {
        status(
            403,
            "Forbidden",
            "pods.metrics.k8s.io is forbidden: User \"secret-user\" cannot list",
        )
    }))
    .await;
    let err = fetch_pod_metrics(&client, None).await.unwrap_err();
    let MetricsError::Forbidden(text) = &err else {
        panic!("{err:?}");
    };
    assert!(text.as_str().contains("forbidden"), "{text}");
    assert!(!text.as_str().contains("secret-user"), "{text}");
}

#[tokio::test]
async fn other_answers_are_failed() {
    for (code, body) in [
        (500, status(500, "InternalError", "boom").1),
        (401, status(401, "Unauthorized", "Unauthorized").1),
        (200, "<html>a proxy page</html>".to_owned()),
        (200, json!({"kind": "Status"}).to_string()),
    ] {
        let (client, _) = stub(Box::new(move |_| (code, body.clone()))).await;
        let err = fetch_node_metrics(&client).await.unwrap_err();
        assert!(matches!(err, MetricsError::Failed(_)), "{code}: {err:?}");
    }
}

#[tokio::test]
async fn an_unreachable_server_is_failed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let kubeconfig = Kubeconfig::from_yaml(&format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {url}\nusers:\n- name: u\n  user:\n    token: t\ncontexts:\n- name: ctx\n  context:\n    cluster: c\n    user: u\n"
    ))
    .unwrap();
    let client = cluster::connect(&kubeconfig, "ctx").await.unwrap();
    let err = fetch_node_metrics(&client).await.unwrap_err();
    assert!(matches!(err, MetricsError::Failed(_)), "{err:?}");
}

// ---------------------------------------------------------------------------------------------
// Polling

#[tokio::test(start_paused = true)]
async fn a_poll_fetches_at_once_then_after_each_interval() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut poll = MetricsPoll::start(POLL_INTERVAL, move || {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move { n }
    });
    assert!(poll.changed().await);
    assert_eq!(poll.latest().as_deref(), Some(&1));
    let before = tokio::time::Instant::now();
    assert!(poll.changed().await);
    assert_eq!(before.elapsed(), POLL_INTERVAL);
    assert_eq!(poll.latest().as_deref(), Some(&2));
}

#[tokio::test(start_paused = true)]
async fn slow_fetches_never_overlap() {
    let running = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let (r, m, d) = (running.clone(), most.clone(), done.clone());
    let poll = MetricsPoll::start(Duration::from_secs(1), move || {
        let (r, m, d) = (r.clone(), m.clone(), d.clone());
        async move {
            let now = r.fetch_add(1, Ordering::SeqCst) + 1;
            m.fetch_max(now, Ordering::SeqCst);
            // Much longer than the interval.
            tokio::time::sleep(Duration::from_secs(10)).await;
            r.fetch_sub(1, Ordering::SeqCst);
            d.fetch_add(1, Ordering::SeqCst);
        }
    });
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(most.load(Ordering::SeqCst), 1);
    // One every 11 s: 10 s of fetch, then the 1 s wait.
    assert_eq!(done.load(Ordering::SeqCst), 5);
    drop(poll);
}

#[tokio::test(start_paused = true)]
async fn dropping_a_poll_stops_it_and_cancels_the_fetch_in_flight() {
    struct Flag(Arc<AtomicUsize>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let cancelled = Arc::new(AtomicUsize::new(0));
    let (c, f) = (calls.clone(), cancelled.clone());
    let poll = MetricsPoll::start(Duration::from_secs(1), move || {
        c.fetch_add(1, Ordering::SeqCst);
        let flag = Flag(f.clone());
        async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(poll);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_pod_and_node_polls_deliver_a_first_result() {
    let (client, seen) = stub(Box::new(|path| {
        if path.ends_with("/nodes") {
            (200, json!({"items": []}).to_string())
        } else {
            (200, pod_list())
        }
    }))
    .await;
    let mut pods = poll_pod_metrics(client.clone(), Some("demo".to_owned()));
    assert!(pods.changed().await);
    assert_eq!(
        pods.latest()
            .unwrap()
            .as_ref()
            .as_ref()
            .unwrap()
            .items
            .len(),
        2
    );
    let mut nodes = poll_node_metrics(client);
    assert!(nodes.changed().await);
    assert!(
        nodes
            .latest()
            .unwrap()
            .as_ref()
            .as_ref()
            .unwrap()
            .items
            .is_empty()
    );
    assert!(seen.lock().unwrap().iter().all(|r| r.starts_with("GET ")));
}

// ---------------------------------------------------------------------------------------------
// One sample against one budget, and items that cannot be read

fn named(name: &str, limits: (Option<&str>, Option<&str>)) -> Container {
    Container {
        name: name.to_owned(),
        ..container((None, None), limits)
    }
}

fn sample(names: &[&str]) -> PodMetrics {
    PodMetrics {
        namespace: "demo".to_owned(),
        name: "web".to_owned(),
        timestamp: None,
        window: None,
        containers: names
            .iter()
            .map(|name| ContainerMetrics {
                name: (*name).to_owned(),
                usage: Usage {
                    cpu: "500m".parse().unwrap(),
                    memory: "1Mi".parse().unwrap(),
                },
            })
            .collect(),
    }
}

#[test]
fn a_running_init_container_has_no_percentage() {
    let spec = PodSpec {
        containers: vec![named("app", (Some("100m"), None))],
        init_containers: Some(vec![named("migrate", (Some("1"), None))]),
        ..Default::default()
    };
    assert_eq!(sample(&["migrate"]).budget_in(&spec), None);
    assert_eq!(sample(&["app", "migrate"]).budget_in(&spec), None);
    assert_eq!(
        sample(&["app"]).budget_in(&spec),
        Some(PodBudget::of_pod(&spec))
    );
}

#[test]
fn a_budget_needs_a_sample_of_exactly_the_long_running_containers() {
    let mut sidecar = named("proxy", (Some("1"), None));
    sidecar.restart_policy = Some("Always".to_owned());
    let spec = PodSpec {
        containers: vec![named("app", (Some("1"), None))],
        init_containers: Some(vec![sidecar]),
        ..Default::default()
    };
    // A sidecar that started before the app, an empty sample, and a repeated name.
    for names in [&["proxy"][..], &[], &["app", "app"], &["app", "other"]] {
        assert_eq!(sample(names).budget_in(&spec), None, "{names:?}");
    }
    assert_eq!(
        sample(&["proxy", "app"]).budget_in(&spec),
        Some(PodBudget::of_pod(&spec))
    );
}

#[test]
fn a_kept_sample_applies_the_same_budget_rule() {
    let mut sidecar = named("proxy", (Some("1"), None));
    sidecar.restart_policy = Some("Always".to_owned());
    let spec = PodSpec {
        containers: vec![named("app", (Some("1"), None))],
        init_containers: Some(vec![sidecar]),
        ..Default::default()
    };
    let check = PodBudgetCheck::of(&spec);
    for names in [&["proxy"][..], &[], &["app", "app"], &["app", "other"]] {
        assert_eq!(check.for_sample(&sample(names).sample()), None, "{names:?}");
    }
    let whole = sample(&["proxy", "app"]);
    assert_eq!(whole.sample().usage, whole.total());
    assert_eq!(
        check.for_sample(&whole.sample()),
        Some(PodBudget::of_pod(&spec))
    );
}

#[tokio::test]
async fn one_unreadable_item_keeps_the_others() {
    let body = json!({"items": [
        {"metadata": {"name": "good", "namespace": "demo"},
         "containers": [{"name": "app", "usage": {"cpu": "1m", "memory": "1Mi"}}]},
        {"metadata": {"name": "no-usage", "namespace": "demo"},
         "containers": [{"name": "app"}]},
        {"metadata": {"name": "number", "namespace": "demo"},
         "containers": [{"name": "app", "usage": {"cpu": 1, "memory": "1Mi"}}]},
        "not an object"
    ]})
    .to_string();
    let (client, _) = stub(Box::new(move |_| (200, body.clone()))).await;
    let list = fetch_pod_metrics(&client, None).await.unwrap();
    assert_eq!(list.skipped, 3);
    assert_eq!(list.items.len(), 1);
}

#[tokio::test]
async fn an_empty_sample_is_missing_not_zero() {
    let body = json!({"items": [
        {"metadata": {"name": "empty", "namespace": "demo"}, "containers": []},
        {"metadata": {"name": "absent", "namespace": "demo"}}
    ]})
    .to_string();
    let (client, _) = stub(Box::new(move |_| (200, body.clone()))).await;
    let list = fetch_pod_metrics(&client, None).await.unwrap();
    assert_eq!((list.items.len(), list.skipped), (0, 2));
}

#[tokio::test]
async fn one_deadline_covers_the_list_and_the_discovery_after_it() {
    let (client, seen) = stub_after(
        Duration::from_millis(300),
        Box::new(|_| status(404, "NotFound", "not found")),
    )
    .await;
    let result = fetch_pod_metrics_within(&client, Some("demo"), Duration::from_millis(450)).await;
    assert_eq!(result, Err(failed("timed out reading metrics")));
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn a_zero_container_limit_is_unset_but_a_zero_request_stays() {
    let budget =
        PodBudget::of_container(&container((Some("0"), Some("0")), (Some("0"), Some("0Mi"))));
    assert_eq!(budget.limits, Amounts::default());
    assert_eq!(budget.requests.cpu, Some(Cpu::from_nanos(0)));
    assert_eq!(budget.requests.memory, Some(Memory::from_bytes(0)));
    // A zero limit on one container leaves the pod without a total.
    let spec = PodSpec {
        containers: vec![
            named("a", (None, Some("0"))),
            named("b", (None, Some("1Mi"))),
        ],
        ..Default::default()
    };
    assert_eq!(PodBudget::of_pod(&spec).limits.memory, None);
    // Allocatable is not a limit: zero stays zero.
    let allocatable = Amounts::from_quantities(&quantities(Some("0"), Some("0")).unwrap());
    assert_eq!(allocatable.memory, Some(Memory::from_bytes(0)));
}
