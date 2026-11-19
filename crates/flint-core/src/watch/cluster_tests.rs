//! Engine tests against the dev cluster. Ignored by default; run with `--include-ignored` and
//! `FLINT_TEST_KUBECONFIG` pointing at the throwaway kind cluster (`scripts/dev-cluster.sh`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;

use super::source::{EventStream, KubeSource, Source, WatchRequest};
use super::*;
use crate::read::{self, KindInfo};
use crate::table::{EventType, FetchError, WireObject};
use crate::{cluster, kubeconfig};

async fn dev_source() -> (Arc<KubeSource>, Vec<KindInfo>) {
    let path = std::env::var_os("FLINT_TEST_KUBECONFIG")
        .filter(|p| !p.is_empty())
        .expect("FLINT_TEST_KUBECONFIG must point at the dev cluster's kubeconfig");
    let loaded = kubeconfig::load(&[path.into()]);
    let context = &loaded.contexts.first().expect("a context").name;
    let (client, version) =
        cluster::connect_and_check(&loaded.merged, context, Duration::from_secs(30))
            .await
            .unwrap();
    let kinds = read::discover_kinds(&client).await.unwrap().kinds;
    (
        KubeSource::new(client, Some(&version), cluster::Target::default()),
        kinds,
    )
}

fn find(kinds: &[KindInfo], group: &str, plural: &str) -> KindInfo {
    kinds
        .iter()
        .find(|k| k.group() == group && k.plural() == plural)
        .cloned()
        .unwrap()
}

// covers UP-27
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn an_old_resource_version_is_an_expired_event_inside_a_200() {
    let (source, kinds) = dev_source().await;
    let pods = find(&kinds, "", "pods");
    let mut stream = source
        .watch(
            &pods,
            None,
            WatchRequest {
                resource_version: "1".into(),
                initial_events: false,
                timeout_secs: 10,
            },
        )
        .await
        .expect("HTTP 200");
    let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.ty, EventType::Error);
    assert_eq!(first.object.code, Some(410));
}

/// The real API server, except that the first plain watch starts from an expired version.
struct ExpireFirst {
    inner: Arc<KubeSource>,
    expired: AtomicBool,
    lists: AtomicUsize,
}

impl Source for ExpireFirst {
    fn list(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        limit: Option<u32>,
        token: Option<String>,
    ) -> BoxFuture<'static, Result<WireObject, FetchError>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list(kind, namespace, limit, token)
    }
    fn watch(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        mut request: WatchRequest,
    ) -> BoxFuture<'static, Result<EventStream, FetchError>> {
        if !request.initial_events && !self.expired.swap(true, Ordering::SeqCst) {
            request.resource_version = "1".into();
        }
        self.inner.watch(kind, namespace, request)
    }
    fn streaming_lists(&self) -> bool {
        false
    }
    fn disable_streaming_lists(&self) {}
}

// covers UP-27
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn engine_relists_after_410_and_stays_live() {
    let (inner, kinds) = dev_source().await;
    let source = Arc::new(ExpireFirst {
        inner,
        expired: AtomicBool::new(false),
        lists: AtomicUsize::new(0),
    });
    let pods = find(&kinds, "", "pods");
    let hub = Arc::new(Hub::new("dev".into(), pods.id()));
    let mut rx = hub.subscribe();
    let epoch = hub.take_over(KindSnapshot::default());
    let engine = engine::Engine::new(
        "dev".into(),
        pods,
        source.clone(),
        hub,
        epoch,
        Arc::new(EngineConfig::default()),
        Arc::new(tokio::sync::Semaphore::new(2)),
    );
    let task = tokio::spawn(engine.run(Feed::Watch));
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            {
                let snap = rx.borrow_and_update();
                if snap.state == SyncState::Synced
                    && !snap.rows.is_empty()
                    && source.lists.load(Ordering::SeqCst) >= 2
                {
                    break;
                }
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("relisted after the 410 and synced again");
    task.abort();
}

// covers UP-20
#[tokio::test]
#[ignore = "needs the dev cluster: set FLINT_TEST_KUBECONFIG and pass --include-ignored"]
async fn streaming_list_with_tables_on_the_dev_cluster() {
    let (source, kinds) = dev_source().await;
    assert!(source.streaming_lists(), "the dev cluster is recent enough");
    let pods = find(&kinds, "", "pods");
    let hub = Arc::new(Hub::new("dev".into(), pods.id()));
    let mut rx = hub.subscribe();
    let epoch = hub.take_over(KindSnapshot::default());
    let engine = engine::Engine::new(
        "dev".into(),
        pods,
        source.clone(),
        hub,
        epoch,
        Arc::new(EngineConfig::default()),
        Arc::new(tokio::sync::Semaphore::new(2)),
    );
    let task = tokio::spawn(engine.run(Feed::Watch));
    let snap = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snap = rx.borrow_and_update().clone();
            if snap.state == SyncState::Synced {
                return snap;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        source.streaming_lists(),
        "the server accepted the streaming list"
    );
    assert!(
        snap.rows
            .iter()
            .any(|r| r.namespace.as_deref() == Some("kube-system"))
    );
    assert_eq!(&*snap.columns[0].name, "Name");
    task.abort();
}
