//! Log session tests against the dev cluster, through the public `CoreHandle` API. Ignored by
//! default; run with `--include-ignored` and `FLINT_TEST_KUBECONFIG` pointing at the throwaway
//! kind cluster (`scripts/dev-cluster.sh`). They use that file alone, create their pods in a
//! namespace prefixed `logs-test-`, and delete it again (also when the test fails).

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flint_core::handle::{Command, ConnectionState, Event};
use flint_core::{
    Freshness, LogOptions, LogSearch, LogSession, LogSnapshot, LogState, LogSubject, LogTarget,
    PodSet, SearchQuery, Settings, Severity, WorkloadKind, WorkloadPod, WorkloadTarget,
};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::Client;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;

mod support;

use support::{
    Core, client, init_logging, name_of, test_kubeconfig, two_context_kubeconfig, unique_name,
};

fn start(kubeconfig: PathBuf, settings: Settings) -> (tempfile::TempDir, Core) {
    init_logging();
    let dir = tempfile::tempdir().unwrap();
    let core = Core::start(kubeconfig, dir.path().join("config"), settings);
    (dir, core)
}

impl Core {
    async fn activate(&self, name: &str) {
        self.handle.send(Command::Activate(name.to_owned()));
        self.wait_for(60, |e| match e {
            Event::ConnectionChanged {
                cluster,
                state: ConnectionState::Connected(_),
            } if cluster == name => Some(()),
            _ => None,
        })
        .await;
    }
}

/// The first snapshot of `session` for which `pred` holds.
async fn until(
    session: &mut LogSession,
    secs: u64,
    pred: impl Fn(&LogSnapshot) -> bool,
) -> Arc<LogSnapshot> {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let snapshot = session.latest();
            if pred(&snapshot) {
                return snapshot;
            }
            assert!(session.changed().await, "core running");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("log condition not reached: {:?}", session.peek().state))
}

// Tests `support::unique_name`, here so it runs once rather than in every binary using support.
#[test]
fn namespace_names_are_unique_and_valid() {
    let names: BTreeSet<String> = (0..1000).map(|_| unique_name("logs-test")).collect();
    assert_eq!(names.len(), 1000);
    let pid = format!("-{}-", std::process::id());
    assert!(
        names
            .iter()
            .all(|n| n.starts_with("logs-test-") && n.contains(&pid))
    );
    // A namespace name is a DNS label: at most 63 characters, even with every part at its
    // largest.
    for prefix in ["logs-test", "exec-test"] {
        let longest = name_of(prefix, u64::MAX, u32::MAX, u32::MAX);
        assert!(longest.len() <= 63, "{longest}");
    }
}

/// Runs `body` in a fresh namespace and deletes the namespace afterwards, also when it panics.
async fn in_namespace<F, Fut>(body: F)
where
    F: FnOnce(Client, String) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let kube = client(&test_kubeconfig()).await;
    let ns = unique_name("logs-test");
    let namespaces: Api<Namespace> = Api::all(kube.clone());
    namespaces
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({"metadata": {"name": ns}})).unwrap(),
        )
        .await
        .unwrap();
    let result = tokio::spawn(body(kube.clone(), ns.clone())).await;
    let _ = namespaces.delete(&ns, &DeleteParams::default()).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn create_pod(kube: &Client, ns: &str, pod: serde_json::Value) -> Api<Pod> {
    let pods: Api<Pod> = Api::namespaced(kube.clone(), ns);
    pods.create(
        &PostParams::default(),
        &serde_json::from_value(pod).unwrap(),
    )
    .await
    .unwrap();
    pods
}

async fn wait_pod(pods: &Api<Pod>, name: &str, pred: impl Fn(&Pod) -> bool) {
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            if pods.get_opt(name).await.unwrap().is_some_and(|p| pred(&p)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("pod {name} never got ready"));
}

fn running(pod: &Pod) -> bool {
    pod.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running")
}

fn container(name: &str, script: &str) -> serde_json::Value {
    json!({
        "name": name,
        "image": "nginx:1.27",
        "imagePullPolicy": "IfNotPresent",
        "command": ["sh", "-c", script],
    })
}

/// Prints 3000 lines at once, then a few lines a second forever, every tenth an error and
/// every tenth (offset by five) a warning.
const CHATTY: &str = r#"i=0; while [ $i -lt 3000 ]; do echo "boot $i"; i=$((i+1)); done
i=0; while true; do
  case $((i % 10)) in 0) echo "ERROR tick $i";; 5) echo "WARN tick $i";; *) echo "info tick $i";; esac
  i=$((i+1)); sleep 0.05
done"#;

// covers UP-40, UP-42, UP-43
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn follows_a_chatty_pod_within_the_buffer_and_across_cluster_switches() {
    let kc_dir = tempfile::tempdir().unwrap();
    let (path, first, second) = two_context_kubeconfig(kc_dir.path(), None);
    in_namespace(move |kube, ns| async move {
        let pods = create_pod(
            &kube,
            &ns,
            json!({
                "metadata": {"name": "chatty"},
                "spec": {"containers": [container("app", CHATTY)], "terminationGracePeriodSeconds": 0}
            }),
        )
        .await;
        wait_pod(&pods, "chatty", running).await;

        let settings = Settings {
            log_buffer_lines: 1000,
            ..Settings::default()
        };
        let (_dir, core) = start(path, settings);
        core.wait_for(10, |e| matches!(e, Event::ClustersChanged(_)).then_some(()))
            .await;
        core.activate(&first).await;

        let target = LogTarget {
            namespace: ns.clone(),
            pod: "chatty".into(),
            container: None,
        };
        let mut session = core
            .handle
            .open_logs(&first, target.clone(), LogOptions::default());
        // The tail asks for no more than the buffer holds (1000 of the 3000+ lines); as ticks
        // follow, the oldest lines are evicted and the buffer stays at 1000.
        let snapshot = until(&mut session, 60, |s| {
            s.lines.end_id() > 1020 && s.state == LogState::Streaming
        })
        .await;
        assert_eq!(snapshot.lines.len(), 1000);
        assert!(snapshot.lines.first_id() > 0, "evicted the oldest");
        assert!(snapshot.lines.iter().all(|(_, l)| l.timestamp.is_some()));
        let error = snapshot
            .lines
            .iter()
            .find(|(_, l)| l.text.starts_with("ERROR tick"))
            .expect("an error line");
        assert_eq!(error.1.severity, Severity::Error);
        let warn = snapshot
            .lines
            .iter()
            .find(|(_, l)| l.text.starts_with("WARN tick"))
            .expect("a warning line");
        assert_eq!(warn.1.severity, Severity::Warning);

        // Search keeps matching new lines while following.
        let search = core
            .handle
            .search_logs(LogSearch::new(SearchQuery::new("error tick")), snapshot.lines.clone())
            .await;
        let before = search.count();
        assert!(before >= 1);

        // Another cluster becomes active; the session keeps streaming.
        core.activate(&second).await;
        let end = snapshot.lines.end_id();
        let later = until(&mut session, 30, |s| s.lines.end_id() >= end + 40).await;
        assert_eq!(later.state, LogState::Streaming);
        assert_eq!(later.lines.len(), 1000);
        let mut search = search;
        search.update(&later.lines);
        assert!(search.count() > before, "{} > {before}", search.count());

        // Tail without following: exactly the newest lines, then the end.
        let tail = LogOptions {
            follow: false,
            tail_lines: Some(5),
            ..LogOptions::default()
        };
        let mut short = core.handle.open_logs(&first, target, tail);
        let done = until(&mut short, 30, |s| matches!(s.state, LogState::Ended { .. })).await;
        assert_eq!(done.state, LogState::Ended { hop: Default::default(), error: None, pod_deleted: false });
        assert_eq!(done.lines.len(), 5);
        assert!(done.lines.iter().all(|(_, l)| l.text.contains("tick")));
    })
    .await;
}

// covers UP-40
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn picks_a_container_and_reads_the_previous_instance() {
    let path = test_kubeconfig();
    in_namespace(move |kube, ns| async move {
        let pods = create_pod(
            &kube,
            &ns,
            json!({
                "metadata": {"name": "twins"},
                "spec": {
                    "containers": [
                        container("app", "echo \"app run $(date +%s%N)\"; sleep 2; exit 1"),
                        container("side", "echo side-hello; sleep 3600"),
                    ],
                    "terminationGracePeriodSeconds": 0
                }
            }),
        )
        .await;
        wait_pod(&pods, "twins", |p| {
            p.status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .and_then(|c| c.iter().find(|c| c.name == "app"))
                .is_some_and(|c| c.restart_count >= 1)
        })
        .await;

        let (_dir, core) = start(path, Settings::default());
        let name = core
            .wait_for(10, |e| match e {
                Event::ClustersChanged(s) => Some(s.clusters[0].info.name.clone()),
                _ => None,
            })
            .await;
        core.activate(&name).await;
        let target = |container: Option<&str>| LogTarget {
            namespace: ns.clone(),
            pod: "twins".into(),
            container: container.map(str::to_owned),
        };
        let ended = |s: &LogSnapshot| matches!(s.state, LogState::Ended { .. });

        let previous = LogOptions {
            previous: true,
            ..LogOptions::default()
        };
        let mut session = core.handle.open_logs(&name, target(Some("app")), previous);
        let done = until(&mut session, 30, ended).await;
        assert_eq!(
            done.state,
            LogState::Ended {
                hop: Default::default(),
                error: None,
                pod_deleted: false
            }
        );
        assert_eq!(done.lines.len(), 1);
        assert!(
            done.lines
                .iter()
                .all(|(_, l)| l.text.starts_with("app run"))
        );

        let once = LogOptions {
            follow: false,
            ..LogOptions::default()
        };
        let mut side = core
            .handle
            .open_logs(&name, target(Some("side")), once.clone());
        let done = until(&mut side, 30, ended).await;
        let texts: Vec<_> = done.lines.iter().map(|(_, l)| l.text.to_string()).collect();
        assert_eq!(texts, ["side-hello"]);

        // Two containers and none picked: the server's reason, and no retrying.
        let mut unpicked = core.handle.open_logs(&name, target(None), once);
        let done = until(&mut unpicked, 30, ended).await;
        let LogState::Ended {
            error: Some(error), ..
        } = &done.state
        else {
            panic!("expected an error, got {:?}", done.state);
        };
        assert!(error.contains("container"), "{error}");
    })
    .await;
}

/// The pods (by name) that have lines in `snapshot`.
fn pods_with_lines(snapshot: &LogSnapshot) -> BTreeSet<String> {
    snapshot
        .lines
        .iter()
        .filter_map(|(_, l)| l.source.as_ref().map(|s| s.pod().to_owned()))
        .collect()
}

// covers UP-45
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn follows_a_deployments_pods_as_they_scale_and_get_replaced() {
    let path = test_kubeconfig();
    in_namespace(move |kube, ns| async move {
        let deployments: Api<Deployment> = Api::namespaced(kube.clone(), &ns);
        // Each pod prints its own name, so the lines tell the pods apart by content too.
        let printer =
            r#"i=0; while true; do echo "$HOSTNAME line $i"; i=$((i+1)); sleep 0.2; done"#;
        deployments
            .create(
                &PostParams::default(),
                &serde_json::from_value(json!({
                    "metadata": {"name": "printer"},
                    "spec": {
                        "replicas": 2,
                        "selector": {"matchLabels": {"app": "printer"}},
                        "template": {
                            "metadata": {"labels": {"app": "printer"}},
                            "spec": {
                                "containers": [container("app", printer)],
                                "terminationGracePeriodSeconds": 0
                            }
                        }
                    }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

        let (_dir, core) = start(path, Settings::default());
        let name = core
            .wait_for(10, |e| match e {
                Event::ClustersChanged(s) => Some(s.clusters[0].info.name.clone()),
                _ => None,
            })
            .await;
        core.activate(&name).await;
        let target = WorkloadTarget {
            namespace: ns.clone(),
            pods: PodSet::Workload {
                kind: WorkloadKind::Deployment,
                name: "printer".into(),
            },
            container: None,
        };
        let mut session = core
            .handle
            .open_logs(&name, target.clone(), LogOptions::default());
        assert_eq!(session.subject(), &LogSubject::Workload(target));
        assert!(session.target().is_none());

        // Both replicas, merged, each line tagged with its pod and container.
        let two = until(&mut session, 180, |s| pods_with_lines(s).len() == 2).await;
        assert_eq!(two.state, LogState::Streaming);
        let status = two.pods.clone().expect("pod set status");
        assert_eq!(status.selector.as_deref(), Some("app=printer"));
        for (_, line) in two.lines.iter() {
            let source = line.source.as_ref().expect("tagged");
            assert_eq!(source.container(), "app");
            assert!(
                line.text.starts_with(&format!("{} line", source.pod())),
                "{} came from {}",
                line.text,
                source.label()
            );
            assert!(
                line.display(false)
                    .starts_with(&format!("[{}] ", source.label()))
            );
        }

        // A shell into the workload finds its pods (UP-47): both ready, the same one picked
        // each time.
        let listed = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let pods = core
                    .handle
                    .workload_pods(
                        &name,
                        &ns,
                        PodSet::Workload {
                            kind: WorkloadKind::Deployment,
                            name: "printer".into(),
                        },
                        Freshness::Current,
                    )
                    .await
                    .unwrap();
                if pods.len() == 2 && pods.iter().all(|p| p.ready) {
                    return pods;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .expect("both pods get ready");
        assert!(listed.iter().all(|p| p.containers == ["app"]));
        let first = WorkloadPod::first_ready(&listed).unwrap();
        assert_eq!(first.name, listed[0].name, "sorted by name");
        let missing = core
            .handle
            .workload_pods(
                &name,
                &ns,
                PodSet::Workload {
                    kind: WorkloadKind::Deployment,
                    name: "nope".into(),
                },
                Freshness::Current,
            )
            .await;
        assert!(missing.is_err(), "{missing:?}");
        let by_selector = core
            .handle
            .workload_pods(
                &name,
                &ns,
                PodSet::Selector("app=printer".into()),
                Freshness::Cached,
            )
            .await
            .unwrap();
        assert_eq!(by_selector.len(), 2);

        // Scale to 3: the third pod's lines appear in the same session.
        deployments
            .patch(
                "printer",
                &PatchParams::default(),
                &Patch::Merge(json!({"spec": {"replicas": 3}})),
            )
            .await
            .unwrap();
        let three = until(&mut session, 180, |s| pods_with_lines(s).len() == 3).await;
        let status = three.pods.clone().expect("pod set status");
        assert_eq!(status.pods, 3, "{status:?}");
        assert_eq!(status.failed(), 0, "{status:?}");

        // Delete one pod: its stream goes, and its replacement is followed.
        let before = pods_with_lines(&three);
        let victim = before.iter().next().unwrap().clone();
        let pods: Api<Pod> = Api::namespaced(kube.clone(), &ns);
        pods.delete(&victim, &DeleteParams::default())
            .await
            .unwrap();
        let replaced = until(&mut session, 180, |s| {
            let status = s.pods.clone().unwrap_or_default();
            pods_with_lines(s).difference(&before).count() >= 1
                && status.live() == 3
                && status.streams.iter().all(|st| st.source.pod() != victim)
        })
        .await;
        assert_eq!(replaced.state, LogState::Streaming);
        let status = replaced.pods.clone().unwrap();
        assert_eq!((status.pods, status.capped), (3, 0), "{status:?}");
    })
    .await;
}
