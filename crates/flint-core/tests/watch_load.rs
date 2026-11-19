//! Load measurement for the watch manager (Sealos/flint#8, UP-26). Ignored by default. Needs the
//! dev cluster (`FLINT_TEST_KUBECONFIG`) and a prepared namespace, named by
//! `FLINT_LOAD_NAMESPACE`, holding thousands of ConfigMaps and a Deployment `load` scaled to
//! about 200 pods. Run it in release mode with `--nocapture` to see the numbers:
//!
//! ```text
//! FLINT_TEST_KUBECONFIG=.dev/kubeconfig FLINT_LOAD_NAMESPACE=m1fix-load \
//!   cargo test --release -p flint-core --test watch_load -- --ignored --nocapture
//! ```
//!
//! It scales `load` to 0 and leaves it there; the caller deletes the namespace. Without
//! `FLINT_LOAD_NAMESPACE` it skips (and says so), so `--include-ignored` runs pass on a clean
//! dev cluster.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::{KindId, KindSnapshot, Settings, SyncState, cluster, kubeconfig};
use k8s_openapi::api::apps::v1::Deployment;
use kube::api::{Api, Patch, PatchParams};
use serde_json::json;

fn env_path(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{name} must be set"))
}

/// Resident set size of this process, in KiB.
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// User plus system CPU time of this process.
fn cpu_time() -> Duration {
    // SAFETY: getrusage only writes into the struct we pass.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

fn in_namespace(snap: &KindSnapshot, ns: &str) -> usize {
    snap.namespace_range(ns).len()
}

// covers UP-26
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "load measurement: needs the dev cluster and FLINT_LOAD_NAMESPACE"]
async fn measure_all_mode_during_scale_down() {
    let path = PathBuf::from(env_path("FLINT_TEST_KUBECONFIG"));
    let Some(ns) = std::env::var("FLINT_LOAD_NAMESPACE")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        eprintln!(
            "SKIPPED measure_all_mode_during_scale_down: set FLINT_LOAD_NAMESPACE to a namespace \
             holding about 3000 ConfigMaps and a Deployment `load` at 200 replicas (see the doc \
             comment of tests/watch_load.rs)"
        );
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let rss_idle = rss_kib();
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().to_owned(),
        settings: Settings::default(),
        settings_error: None,
        kubeconfig_base: vec![path.clone()],
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    })
    .unwrap();
    let events = core.events();
    let cluster = loop {
        if let Event::ClustersChanged(s) = events.recv().await.unwrap() {
            break s.clusters[0].info.name.clone();
        }
    };
    let cpu_start = cpu_time();
    let activated = Instant::now();
    core.send(Command::Activate(cluster.clone()));
    // Drain control events in the background, as the UI would.
    let drain = tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            if let Event::ConnectionChanged {
                state: ConnectionState::Failed(msg),
                ..
            } = e
            {
                panic!("{msg}");
            }
        }
    });

    // The pods view and the config map view are open; everything else is watched by All mode.
    let mut pods = core.subscribe(&cluster, &KindId::pods());
    let mut configmaps = core.subscribe(&cluster, &KindId::new("", "configmaps"));
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let p = pods.latest();
            let c = configmaps.latest();
            if p.state == SyncState::Synced
                && c.state == SyncState::Synced
                && in_namespace(&p, &ns) >= 200
                && in_namespace(&c, &ns) >= 3000
            {
                break;
            }
            tokio::select! {
                _ = pods.changed() => {}
                _ = configmaps.changed() => {}
            }
        }
    })
    .await
    .expect("initial lists");
    let sync_time = activated.elapsed();
    // Let every All-mode kind finish its initial list.
    let watched = loop {
        let w = core.watched_kinds(&cluster).await;
        if activated.elapsed() > Duration::from_secs(5) {
            break w;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let cpu_synced = cpu_time() - cpu_start;
    let rss_synced = rss_kib();
    let pod_rows = pods.latest().rows.len();
    let cm_rows = configmaps.latest().rows.len();
    drop(configmaps);

    // Count what the UI's pods view would see during a 200-pod scale-down.
    let notifications = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(rss_synced));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (peak.clone(), done.clone());
        tokio::spawn(async move {
            while !done.load(Ordering::SeqCst) {
                peak.fetch_max(rss_kib(), Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
    let loaded = kubeconfig::load(&[path]);
    let client = cluster::connect(&loaded.merged, &cluster).await.unwrap();
    let cpu_before = cpu_time();
    let started = Instant::now();
    Api::<Deployment>::namespaced(client, &ns)
        .patch_scale(
            "load",
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"replicas": 0}})),
        )
        .await
        .unwrap();
    let mut gone_at = None;
    let mut last_notification = Duration::ZERO;
    loop {
        let changed = tokio::time::timeout(Duration::from_secs(3), pods.changed()).await;
        match changed {
            Ok(true) => {
                notifications.fetch_add(1, Ordering::SeqCst);
                last_notification = started.elapsed();
                let snap = pods.latest();
                if gone_at.is_none() && in_namespace(&snap, &ns) == 0 {
                    gone_at = Some(started.elapsed());
                }
            }
            Ok(false) => panic!("core stopped"),
            // Three quiet seconds after the last pod left.
            Err(_) if gone_at.is_some() => break,
            Err(_) => assert!(
                started.elapsed() < Duration::from_secs(180),
                "scale-down stalled"
            ),
        }
    }
    let window = gone_at.unwrap();
    let cpu_scale = cpu_time() - cpu_before;
    done.store(true, Ordering::SeqCst);
    sampler.await.unwrap();
    let notified = notifications.load(Ordering::SeqCst);
    let bound = (last_notification.as_millis() as u64).div_ceil(100) + 1;

    eprintln!("--- watch load (mode All) ---");
    eprintln!("kinds watched: {}", watched.len());
    eprintln!("rows: pods {pod_rows}, configmaps {cm_rows}");
    eprintln!("initial sync of pods+configmaps: {sync_time:?}");
    eprintln!("RSS idle (before activation): {} MiB", rss_idle / 1024);
    eprintln!("RSS synced: {} MiB", rss_synced / 1024);
    eprintln!("CPU for activation and initial lists: {cpu_synced:?}");
    eprintln!("scale-down 200 -> 0: last pod gone after {window:?}");
    eprintln!(
        "  pods view notifications: {notified} over {last_notification:?} (one per 100 ms \
         allows {bound})"
    );
    eprintln!("  CPU during scale-down: {cpu_scale:?}");
    eprintln!(
        "  RSS peak during scale-down: {} MiB",
        peak.load(Ordering::SeqCst) / 1024
    );
    eprintln!("  RSS after: {} MiB", rss_kib() / 1024);
    assert!(
        notified <= bound,
        "notifications are coalesced: {notified} > {bound}"
    );
    drain.abort();
}
