//! UP-26: the whole app with 10,000 pods and a burst of 200 deletes, on the fake API server.
//!
//! Nightly only (`macos-load` in `memory.yml`): the PR and master suite skips it by name. It
//! records its numbers and fails only on a gross regression: a round-trip over the live-list
//! latency bound in docs/TESTING.md. Memory is recorded, not gated. Set `FLINT_E2E_LOAD_REPORT`
//! to a path to keep the numbers as JSON.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use flint_e2e::app::{App, AppOptions, WAIT};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::Options;
use serde_json::{Value, json};

use crate::support::*;

const PODS: usize = 10_000;
const BURST: usize = 200;
const NAMESPACE: &str = "demo";

fn pod_name(index: usize) -> String {
    format!("load-{index:05}")
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Adds [`PODS`] rows to the pods table of the fixtures at `root`. In the fixtures rather than
/// through `FakeApiServer::add_pod`, whose scan of the rows makes 10,000 calls take minutes.
fn add_generated_pods(root: &Path) -> Result<()> {
    let path = root.join("tables/core/v1/pods.json");
    let mut table: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let rows = table["rows"].as_array_mut().context("the pods table")?;
    let mut template = rows
        .iter()
        .find(|r| r["object"]["metadata"]["namespace"] == NAMESPACE && r["cells"][2] == "Running")
        .context("a running pod to copy")?
        .clone();
    if let Some(metadata) = template["object"]["metadata"].as_object_mut() {
        metadata.remove("annotations");
    }
    rows.extend((0..PODS).map(|index| {
        let mut row = template.clone();
        row["cells"][0] = pod_name(index).into();
        row["object"]["metadata"]["name"] = pod_name(index).into();
        row["object"]["metadata"]["uid"] = format!("00000000-0000-4000-8000-{index:012}").into();
        row
    }));
    std::fs::write(path, table.to_string())?;
    Ok(())
}

/// The live-list latency bound of docs/TESTING.md: 2 s, 5 s under `CI`, or
/// `FLINT_TEST_LATENCY_MS`.
fn latency_bound() -> Duration {
    let ms = std::env::var("FLINT_TEST_LATENCY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if std::env::var_os("CI").is_some() {
            5000
        } else {
            2000
        });
    Duration::from_millis(ms)
}

/// The resident memory of the app this test launched, in KiB.
fn app_rss_kib(app: &App) -> Result<u64> {
    let out = std::process::Command::new("ps")
        .args(["-ax", "-o", "ppid=,rss=,comm="])
        .output()?;
    let parent = std::process::id().to_string();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let (ppid, rss) = (fields.next()?, fields.next()?);
            let command = fields.collect::<Vec<_>>().join(" ");
            (ppid == parent && Path::new(&command) == app.binary())
                .then(|| rss.parse().ok())
                .flatten()
        })
        .context("the app's process in ps")
}

/// The sample at `percent` of `sorted`, which is not empty.
fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    sorted[(sorted.len() - 1) * percent / 100]
}

// covers UP-26
#[test]
#[ignore = "app e2e load test: nightly, needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_26_large_list_stays_responsive() -> Result<()> {
    let fixtures = tempfile::tempdir()?;
    let options = Options::default();
    copy_tree(&options.fixtures, fixtures.path())?;
    add_generated_pods(fixtures.path())?;
    let cluster = TestCluster::fake(Options {
        fixtures: fixtures.path().to_owned(),
        ..options
    })?;
    let fake = cluster.fake_server().context("a fake cluster")?;
    let before = fake.rows(&fake.gvr("pods")).len();
    ensure!(before >= PODS, "only {before} pods were served");
    let after = before - BURST;

    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    let opening = Instant::now();
    app.open_view("pods")?;
    app.wait_for("/list/total", Equals(before.into()))?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    let opened_in = opening.elapsed();
    let rss_before = app_rss_kib(&app)?;

    // Every round-trip from the first delete until the list shows the new count.
    let burst_done = AtomicBool::new(false);
    let burst_started = Instant::now();
    let mut round_trips = Vec::new();
    let converged = std::thread::scope(|scope| -> Result<Value> {
        scope.spawn(|| {
            for index in 0..BURST {
                fake.force_delete_pod(NAMESPACE, &pod_name(index));
            }
            burst_done.store(true, Ordering::Release);
        });
        loop {
            let sent = Instant::now();
            let state = app.state()?;
            round_trips.push(sent.elapsed());
            if burst_done.load(Ordering::Acquire) && state["list"]["total"] == after {
                return Ok(state);
            }
            if burst_started.elapsed() > WAIT {
                bail!(
                    "the list did not converge to {after} rows within {WAIT:?}; last saw {}",
                    state["list"]["total"]
                );
            }
        }
    })?;
    let converged_in = burst_started.elapsed();
    let rss_after = app_rss_kib(&app)?;

    assert_eq!(converged["list"]["sync"], "live");
    let shown = row_names(&converged);
    ensure!(
        !shown.iter().any(|name| {
            name.strip_prefix("load-")
                .and_then(|n| n.parse::<usize>().ok())
                .is_some_and(|n| n < BURST)
        }),
        "a deleted pod is still listed"
    );

    round_trips.sort();
    let slowest = *round_trips.last().context("a round-trip")?;
    let bound = latency_bound();
    let report = json!({
        "pods": before,
        "deleted": BURST,
        "list_opened_ms": opened_in.as_millis() as u64,
        "converged_ms": converged_in.as_millis() as u64,
        "round_trips": round_trips.len(),
        "round_trip_p50_us": percentile(&round_trips, 50).as_micros() as u64,
        "round_trip_p99_us": percentile(&round_trips, 99).as_micros() as u64,
        "round_trip_max_us": slowest.as_micros() as u64,
        "round_trip_bound_ms": bound.as_millis() as u64,
        "rss_before_burst_kib": rss_before,
        "rss_after_burst_kib": rss_after,
    });
    eprintln!("UP-26 load: {report}");
    if let Some(path) = std::env::var_os("FLINT_E2E_LOAD_REPORT") {
        std::fs::write(path, format!("{report:#}\n"))?;
    }
    ensure!(
        slowest <= bound,
        "the slowest round-trip took {slowest:?}, over the {bound:?} bound"
    );
    app.quit()?;
    Ok(())
}
