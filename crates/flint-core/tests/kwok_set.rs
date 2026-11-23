//! Memory and CPU of a running set on local kwok clusters (Sealos/flint#447). Ignored by default.
//! `scripts/kwok-set.sh` creates the clusters; the method and results are in
//! `docs/spikes/kwok-set-measurements.md`:
//!
//! ```text
//! FLINT_KWOK_KUBECONFIG=<scratch>/kwok-set/kubeconfig FLINT_KWOK_MEMBERS=5 FLINT_KWOK_PODS=10000 \
//!   cargo test --release -p flint-core --test kwok_set -- --ignored --nocapture
//! ```
//!
//! It refuses any kubeconfig outside a `kwok-set` path or that is the default one, any context
//! `kwok-set.sh` did not name, and any server that is not the loopback endpoint kwokctl records
//! for that cluster. Churn labels fixture pods through `kubectl` in child processes, so its cost
//! is not counted as Flint's.

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::{Duration, Instant};

use flint_core::handle::{Command, CoreConfig, CoreHandle, MemberStatus, Subscription};
use flint_core::telemetry::KindCheck;
use flint_core::{KindId, KindSnapshot, Settings, SyncState, WatchMode, kubeconfig};

const FIXTURE_LABEL: &str = "kwok.x-k8s.io/kwokctl-scale";
const CHURN_LABEL: &str = "flint-churn";
const CHURN_BATCH: usize = 100;
const IDLE_WINDOW: Duration = Duration::from_secs(30);
const WAIT: Duration = Duration::from_secs(300);

fn env_number(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .map_or(default, |v| v.parse().unwrap_or_else(|_| panic!("{name}")))
}

/// `FLINT_KWOK_WATCH_MODE`: `all`, the default and the only mode that runs the telemetry feeds
/// without an overview, or `on-view`.
fn watch_mode() -> WatchMode {
    match std::env::var("FLINT_KWOK_WATCH_MODE").as_deref() {
        Err(_) | Ok("all") => WatchMode::All,
        Ok("on-view") => WatchMode::OnView,
        Ok(other) => panic!("FLINT_KWOK_WATCH_MODE: {other}"),
    }
}

/// The scratch kubeconfig, refused unless it is plainly not a real one.
fn scratch_kubeconfig() -> Option<PathBuf> {
    let raw = std::env::var_os("FLINT_KWOK_KUBECONFIG")?;
    let path = std::fs::canonicalize(raw).expect("the scratch kubeconfig exists");
    assert!(
        path.to_string_lossy().contains("kwok-set"),
        "the scratch kubeconfig path must contain kwok-set"
    );
    if let Some(home) = std::env::var_os("HOME") {
        let default = Path::new(&home).join(".kube/config");
        assert!(
            !same_file(&path, &default),
            "refusing the default kubeconfig"
        );
    }
    Some(path)
}

/// Whether both paths are one file, through symlinks and hard links. Reads metadata only. A
/// missing `other` is a different file; any other failure refuses rather than guesses.
fn same_file(path: &Path, other: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let path = std::fs::metadata(path).expect("the scratch kubeconfig is readable");
    match std::fs::metadata(other) {
        Ok(other) => path.dev() == other.dev() && path.ino() == other.ino(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("cannot compare with the default kubeconfig: {error}"),
    }
}

#[test]
fn a_hard_link_is_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let link = dir.path().join("link");
    let different = dir.path().join("different");
    std::fs::write(&original, "a").unwrap();
    std::fs::write(&different, "a").unwrap();
    std::fs::hard_link(&original, &link).unwrap();
    assert!(same_file(&link, &original));
    assert!(!same_file(&different, &original));
    assert!(!same_file(&original, &dir.path().join("missing")));
}

fn is_kwok_context(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("kwok-flint-kwok-") else {
        return false;
    };
    let Some((id, member)) = rest.rsplit_once('-') else {
        return false;
    };
    !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_digit())
        && matches!(member, "1" | "2" | "3" | "4" | "5")
}

/// Whether `server` is a bare `http(s)://127.0.0.1:<port>` URL, with nothing else in it.
fn is_loopback_server(server: &str) -> bool {
    let Ok(uri) = server.parse::<http::Uri>() else {
        return false;
    };
    let Some(authority) = uri.authority() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && !authority.as_str().contains('@')
        && authority.host() == "127.0.0.1"
        && authority.port_u16().is_some()
        && uri.path() == "/"
        && uri.query().is_none()
}

/// The API server kwokctl itself records for `cluster`. Its output holds credentials, so neither
/// the output nor a parse error is ever printed.
fn kwokctl_server(cluster: &str) -> String {
    let out = Process::new("kwokctl")
        .env_remove("KUBECONFIG")
        .args(["--name", cluster, "get", "kubeconfig"])
        .stderr(std::process::Stdio::null())
        .output()
        .expect("kwokctl starts");
    assert!(out.status.success(), "kwokctl does not know {cluster}");
    let config = std::str::from_utf8(&out.stdout)
        .ok()
        .and_then(|yaml| kube::config::Kubeconfig::from_yaml(yaml).ok())
        .expect("kwokctl prints a kubeconfig");
    let [entry] = config.clusters.as_slice() else {
        panic!("kwokctl names one cluster for {cluster}");
    };
    entry
        .cluster
        .as_ref()
        .and_then(|c| c.server.clone())
        .expect("kwokctl names a server")
}

#[test]
fn only_a_plain_loopback_url_is_a_loopback_server() {
    for local in ["https://127.0.0.1:32766", "http://127.0.0.1:8080/"] {
        assert!(is_loopback_server(local), "{local}");
    }
    for refused in [
        "https://127.0.0.1:1@remote.example:6443",
        "https://user@127.0.0.1:6443",
        "https://127.0.0.1.remote.example:6443",
        "https://127.0.0.1",
        "https://127.0.0.1:6443/proxy",
        "https://127.0.0.1:6443/?target=remote",
        "ftp://127.0.0.1:6443",
        "https://remote.example:6443",
        "not a url",
    ] {
        assert!(!is_loopback_server(refused), "{refused}");
    }
}

/// The first `count` contexts, all checked to be local kwok clusters.
fn member_contexts(path: &Path, count: usize) -> Vec<String> {
    let loaded = kubeconfig::load(&[path.to_owned()]);
    let mut names = Vec::new();
    for named in &loaded.merged.contexts {
        assert!(
            is_kwok_context(&named.name),
            "refusing context {}",
            named.name
        );
        let cluster = named
            .context
            .as_ref()
            .map(|c| c.cluster.as_str())
            .expect("a context names a cluster");
        let entry = loaded
            .merged
            .clusters
            .iter()
            .find(|c| c.name == cluster)
            .and_then(|c| c.cluster.as_ref())
            .expect("a context's cluster exists");
        let server = entry.server.as_deref().expect("a cluster has a server");
        assert!(
            is_loopback_server(server),
            "refusing a server that is not on loopback"
        );
        assert!(entry.proxy_url.is_none(), "refusing a proxied server");
        let kwok_name = named.name.strip_prefix("kwok-").expect("a kwok context");
        assert!(
            kwokctl_server(kwok_name) == server,
            "refusing a server kwokctl does not record for {kwok_name}"
        );
        names.push(named.name.clone());
    }
    names.sort();
    assert!(names.len() >= count, "{} contexts found", names.len());
    names.truncate(count);
    names
}

fn rss_mib() -> f64 {
    let out = Process::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    let kib: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    kib / 1024.0
}

/// Physical footprint in MiB, which unlike RSS counts compressed pages. macOS only.
#[cfg(target_os = "macos")]
fn footprint_mib() -> Option<f64> {
    unsafe extern "C" {
        fn proc_pid_rusage(pid: libc::c_int, flavor: libc::c_int, buffer: *mut u64) -> libc::c_int;
    }
    // `rusage_info_v0`: a 16-byte uuid, then u64 fields; `ri_phys_footprint` is the eighth.
    const RUSAGE_INFO_V0: libc::c_int = 0;
    const PHYS_FOOTPRINT: usize = 9;
    let mut info = [0u64; 12];
    // SAFETY: the buffer is larger than `rusage_info_v0` and aligned for its u64 fields.
    let status = unsafe { proc_pid_rusage(libc::getpid(), RUSAGE_INFO_V0, info.as_mut_ptr()) };
    (status == 0).then(|| info[PHYS_FOOTPRINT] as f64 / (1024.0 * 1024.0))
}

#[cfg(not(target_os = "macos"))]
fn footprint_mib() -> Option<f64> {
    None
}

fn footprint_text(value: Option<f64>) -> String {
    value.map_or_else(|| "not available".to_owned(), |v| format!("{v:.0} MiB"))
}

fn usage() -> libc::rusage {
    // SAFETY: getrusage only writes into the struct we pass.
    unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    }
}

fn cpu_time() -> Duration {
    let usage = usage();
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// The process's peak RSS so far.
fn peak_rss_mib() -> f64 {
    let peak = usage().ru_maxrss as f64;
    if cfg!(target_os = "macos") {
        peak / (1024.0 * 1024.0)
    } else {
        peak / 1024.0
    }
}

fn fixture_rows(snapshot: &KindSnapshot) -> usize {
    snapshot
        .rows
        .iter()
        .filter(|r| r.labels.iter().any(|(k, _)| &**k == FIXTURE_LABEL))
        .count()
}

fn labelled(snapshot: &KindSnapshot, value: &str) -> usize {
    snapshot
        .rows
        .iter()
        .filter(|r| {
            r.labels
                .iter()
                .any(|(k, v)| &**k == CHURN_LABEL && &**v == value)
        })
        .count()
}

/// Waits until `done` holds for the subscription's latest snapshot.
async fn until(subscription: &mut Subscription, done: impl Fn(&KindSnapshot) -> bool) {
    tokio::time::timeout(WAIT, async {
        loop {
            if done(&subscription.latest()) {
                return;
            }
            assert!(subscription.changed().await, "core stopped");
        }
    })
    .await
    .expect("the member reached the state in time");
}

/// Waits until every member's feed has loaded and is watching all of its kinds.
async fn until_feeds_current(core: &CoreHandle, contexts: &[String]) {
    let current = |context: &String| {
        core.telemetry(context).is_some_and(|feed| {
            feed.snapshot
                .kinds
                .iter()
                .all(|(_, check)| *check == KindCheck::Current)
        })
    };
    let mut updates = core.telemetry_updates();
    tokio::time::timeout(WAIT, async {
        while !contexts.iter().all(current) {
            assert!(updates.changed().await, "core stopped");
        }
    })
    .await
    .expect("every feed became current in time");
}

/// Labels `churn` fixture pods of one member with `value`, from child processes.
fn start_churn(
    path: &Path,
    context: &str,
    snapshot: &KindSnapshot,
    churn: usize,
    value: &str,
) -> Vec<std::process::Child> {
    let names: Vec<&str> = snapshot
        .rows
        .iter()
        .filter(|r| r.labels.iter().any(|(k, _)| &**k == FIXTURE_LABEL))
        .take(churn)
        .map(|r| &*r.name)
        .collect();
    assert_eq!(names.len(), churn, "not enough fixture pods to churn");
    names
        .chunks(CHURN_BATCH)
        .map(|batch| {
            Process::new("kubectl")
                .env("KUBECONFIG", path)
                .arg("--kubeconfig")
                .arg(path)
                .args(["--context", context, "--namespace", "default"])
                .args(["label", "--overwrite", "pods"])
                .args(batch)
                .arg(format!("{CHURN_LABEL}={value}"))
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("kubectl starts")
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement: needs local kwok clusters from scripts/kwok-set.sh"]
async fn measure_running_set() {
    let Some(path) = scratch_kubeconfig() else {
        eprintln!("SKIPPED measure_running_set: set FLINT_KWOK_KUBECONFIG");
        return;
    };
    let member_count = env_number("FLINT_KWOK_MEMBERS", 5);
    let pods = env_number("FLINT_KWOK_PODS", 10_000);
    let churn = env_number("FLINT_KWOK_CHURN", 1000);
    let mode = watch_mode();
    let contexts = member_contexts(&path, member_count);
    let dir = tempfile::tempdir().unwrap();

    let rss_idle = rss_mib();
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().join("config"),
        settings: Settings {
            watch_mode: mode,
            ..Settings::default()
        },
        settings_error: None,
        kubeconfig_base: vec![path.clone()],
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    })
    .unwrap();
    let events = core.events();
    let drain = tokio::spawn(async move { while events.recv().await.is_ok() {} });

    let cpu_start = cpu_time();
    let started = Instant::now();
    let mut lists: Vec<Subscription> = contexts
        .iter()
        .map(|c| core.subscribe(c, &KindId::pods()))
        .collect();
    core.send(Command::Activate(contexts[0].clone()));
    // A set holds at least two clusters, so the one-cluster baseline is the active cluster alone.
    if member_count > 1 {
        core.send(Command::SetRunningSet(contexts.clone()));
    }
    for list in &mut lists {
        until(list, |s| {
            s.state == SyncState::Synced && fixture_rows(s) >= pods
        })
        .await;
        assert_eq!(
            fixture_rows(&list.latest()),
            pods,
            "the fixture holds exactly FLINT_KWOK_PODS pods"
        );
    }
    let lists_synced_after = started.elapsed();
    if mode == WatchMode::All {
        until_feeds_current(&core, &contexts).await;
    }
    let synced_after = started.elapsed();
    let cpu_sync = cpu_time() - cpu_start;
    if member_count > 1 {
        let reports = core.running_set().await;
        assert_eq!(reports.len(), member_count, "every member is in the set");
        assert!(
            reports.iter().all(|r| r.status == MemberStatus::Live),
            "every member is live"
        );
    }
    let rss_synced = rss_mib();
    let footprint_synced = footprint_mib();

    let cpu_before_idle = cpu_time();
    tokio::time::sleep(IDLE_WINDOW).await;
    let cpu_idle = cpu_time() - cpu_before_idle;
    let rss_settled = rss_mib();
    let footprint_settled = footprint_mib();

    let value = format!("c{}", std::process::id());
    let cpu_before_churn = cpu_time();
    let churn_started = Instant::now();
    let mut children = Vec::new();
    for (context, list) in contexts.iter().zip(&mut lists) {
        children.extend(start_churn(&path, context, &list.latest(), churn, &value));
    }
    // The cutoff is the pod lists. The feed's separate pod watch exposes no label or revision to
    // wait on, so its remaining work can fall after this CPU sample.
    for list in &mut lists {
        until(list, |s| labelled(s, &value) >= churn).await;
    }
    let churn_seen = churn_started.elapsed();
    let cpu_churn = cpu_time() - cpu_before_churn;
    for mut child in children {
        assert!(child.wait().unwrap().success(), "kubectl label failed");
    }
    let rss_after_churn = rss_mib();
    let footprint_after_churn = footprint_mib();
    let rows: usize = lists.iter_mut().map(|l| l.latest().rows.len()).sum();

    eprintln!("--- kwok running set ---");
    eprintln!("members: {member_count}");
    eprintln!("fixture pods per member: {pods}; rows held: {rows}");
    eprintln!("watch mode: {mode:?}");
    eprintln!("pod lists synced after: {lists_synced_after:.2?}");
    eprintln!("all members synced after: {synced_after:.2?}");
    eprintln!("CPU for activation and initial lists: {cpu_sync:.2?}");
    eprintln!("RSS before start: {rss_idle:.0} MiB");
    eprintln!("RSS when synced: {rss_synced:.0} MiB");
    eprintln!("RSS after {IDLE_WINDOW:?} idle: {rss_settled:.0} MiB");
    eprintln!("CPU over {IDLE_WINDOW:?} idle: {cpu_idle:.2?}");
    eprintln!(
        "churn: {} label updates seen by the pod lists after {churn_seen:.2?}",
        churn * member_count
    );
    eprintln!("CPU until the pod lists saw the churn: {cpu_churn:.2?}");
    eprintln!("RSS after churn: {rss_after_churn:.0} MiB");
    eprintln!("peak RSS of the process: {:.0} MiB", peak_rss_mib());
    eprintln!(
        "footprint when synced: {}",
        footprint_text(footprint_synced)
    );
    eprintln!(
        "footprint after idle: {}",
        footprint_text(footprint_settled)
    );
    eprintln!(
        "footprint after churn: {}",
        footprint_text(footprint_after_churn)
    );
    drain.abort();
}
