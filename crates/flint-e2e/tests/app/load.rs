//! UP-26: the whole app with 10,000 pods and a burst of 200 deletes, on the fake API server.
//!
//! Nightly only (`macos-load` in `memory.yml`): the PR and master suite skips it by name. It
//! records its numbers and fails only on a gross regression: a round-trip over the live-list
//! latency bound in docs/TESTING.md. Memory is recorded, not gated. Set `FLINT_E2E_LOAD_REPORT`
//! to a path to keep the numbers as JSON, written for a failed burst too.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// How often the app's memory is read during the burst.
const RSS_SAMPLE_EVERY: Duration = Duration::from_millis(50);

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

/// The resident memory of process `pid`, in KiB.
fn rss_kib(pid: u32) -> Result<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .with_context(|| format!("the resident memory of process {pid}"))
}

/// The sample at `percent` of `sorted` in microseconds, if there is one.
fn percentile_us(sorted: &[Duration], percent: usize) -> Option<u64> {
    let last = sorted.len().checked_sub(1)?;
    Some(sorted[last * percent / 100].as_micros() as u64)
}

/// The app's state, or an error once `limit` has passed. On its own connection because
/// [`App::state`] may wait a minute, longer than the time this test has left.
fn state_within(app: &App, limit: Duration) -> Result<Value> {
    let deadline = Instant::now() + limit;
    let mut stream = UnixStream::connect(app.config_dir().join("automation.sock"))?;
    stream.set_write_timeout(Some(limit))?;
    stream.write_all(b"{\"id\":0,\"cmd\":\"state\",\"args\":null}\n")?;
    let mut answer = Vec::new();
    let mut chunk = [0; 64 * 1024];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .with_context(|| format!("no answer to state within {limit:?}"))?;
        stream.set_read_timeout(Some(left))?;
        let read = stream
            .read(&mut chunk)
            .with_context(|| format!("no answer to state within {limit:?}"))?;
        ensure!(read > 0, "the app closed the connection during state");
        answer.extend_from_slice(&chunk[..read]);
        if chunk[..read].contains(&b'\n') {
            break;
        }
    }
    let mut response: Value = serde_json::from_slice(&answer)?;
    ensure!(response["ok"] == true, "state: {}", response["error"]);
    Ok(response["result"].take())
}

/// Polls the app until the burst is over and the list shows `after` rows, within [`WAIT`] of
/// `started`, adding every round-trip to `round_trips`.
fn await_convergence(
    app: &App,
    after: usize,
    burst_done: &AtomicBool,
    started: Instant,
    round_trips: &mut Vec<Duration>,
) -> Result<Value> {
    let mut last_seen = (Value::Null, Value::Null);
    loop {
        let Some(left) = WAIT
            .checked_sub(started.elapsed())
            .filter(|left| !left.is_zero())
        else {
            bail!(
                "the list did not converge to {after} rows within {WAIT:?}; last saw total {}, \
                 shown {}",
                last_seen.0,
                last_seen.1
            );
        };
        let sent = Instant::now();
        let state = state_within(app, left);
        round_trips.push(sent.elapsed());
        let state = state?;
        let list = &state["list"];
        last_seen = (list["total"].clone(), list["shown"].clone());
        // A state that arrives after the deadline is too late, whatever it says.
        if started.elapsed() < WAIT
            && burst_done.load(Ordering::Acquire)
            && list["total"] == after
            && list["shown"] == after
        {
            return Ok(state);
        }
    }
}

/// What the converged list must show once the deleted pods are gone.
fn check_converged(state: &Value) -> Result<()> {
    ensure!(
        state["list"]["sync"] == "live",
        "the list is {} after the burst",
        state["list"]["sync"]
    );
    let deleted_listed = row_names(state).iter().any(|name| {
        name.strip_prefix("load-")
            .and_then(|n| n.parse::<usize>().ok())
            .is_some_and(|n| n < BURST)
    });
    ensure!(!deleted_listed, "a deleted pod is still listed");
    Ok(())
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
    let pid = app.pid();
    let rss_before = rss_kib(pid)?;

    // Every round-trip from the first delete until the list shows the new count.
    let burst_done = AtomicBool::new(false);
    let sampling_done = AtomicBool::new(false);
    let rss_peak = AtomicU64::new(rss_before);
    let rss_samples = AtomicU64::new(0);
    let burst_started = Instant::now();
    let mut round_trips = Vec::new();
    let outcome = std::thread::scope(|scope| -> Result<Value> {
        scope.spawn(|| {
            for index in 0..BURST {
                fake.force_delete_pod(NAMESPACE, &pod_name(index));
            }
            burst_done.store(true, Ordering::Release);
        });
        scope.spawn(|| {
            while !sampling_done.load(Ordering::Acquire) {
                if let Ok(rss) = rss_kib(pid) {
                    rss_peak.fetch_max(rss, Ordering::Relaxed);
                    rss_samples.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(RSS_SAMPLE_EVERY);
            }
        });
        let outcome = await_convergence(&app, after, &burst_done, burst_started, &mut round_trips);
        sampling_done.store(true, Ordering::Release);
        outcome
    });
    let converged_in = burst_started.elapsed();
    let rss_after = rss_kib(pid).ok();
    let rss_peak = rss_after.map_or(rss_peak.load(Ordering::Relaxed), |rss| {
        rss.max(rss_peak.load(Ordering::Relaxed))
    });

    round_trips.sort();
    let slowest = round_trips.last().copied();
    let bound = latency_bound();
    let verdict = outcome
        .and_then(|state| check_converged(&state))
        .and_then(|()| {
            let slowest = slowest.context("a round-trip")?;
            ensure!(
                slowest <= bound,
                "the slowest round-trip took {slowest:?}, over the {bound:?} bound"
            );
            Ok(())
        });
    // The first line only: an app error goes on with the whole state and log.
    let failure = verdict
        .as_ref()
        .err()
        .and_then(|e| format!("{e:#}").lines().next().map(str::to_owned));
    let report = json!({
        "pods": before,
        "deleted": BURST,
        "failure": failure,
        "list_opened_ms": opened_in.as_millis() as u64,
        "converged_ms": converged_in.as_millis() as u64,
        "round_trips": round_trips.len(),
        "round_trip_p50_us": percentile_us(&round_trips, 50),
        "round_trip_p99_us": percentile_us(&round_trips, 99),
        "round_trip_max_us": slowest.map(|d| d.as_micros() as u64),
        "round_trip_bound_ms": bound.as_millis() as u64,
        "rss_before_burst_kib": rss_before,
        "rss_peak_during_burst_kib": rss_peak,
        "rss_samples_during_burst": rss_samples.load(Ordering::Relaxed),
        "rss_after_burst_kib": rss_after,
    });
    eprintln!("UP-26 load: {report}");
    let written = std::env::var_os("FLINT_E2E_LOAD_REPORT")
        .map_or(Ok(()), |path| std::fs::write(path, format!("{report:#}\n")));
    if let Err(error) = verdict {
        if let Err(write_error) = written {
            eprintln!("UP-26 load: the report was not written: {write_error}");
        }
        app.mark_failing();
        return Err(error);
    }
    written?;
    app.quit()?;
    Ok(())
}
