//! Helpers shared by the app e2e tests.

use std::fmt::Debug;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use flint_e2e::app::{App, WAIT};
use flint_e2e::fake_api::{FakeApiServer, Options, write_kubeconfigs};
use flint_e2e::scratch::Scratch;
use serde_json::Value;

pub use flint_e2e::app::Test::{self, Any, Contains, Equals, Exists};

/// The exit code of child `pid` if it has ended. Leaves it unreaped, for `App::quit` to collect
/// and check.
pub fn exit_code(pid: u32) -> Result<Option<i32>> {
    let id = libc::id_t::from(pid);
    // SAFETY: an all-zero siginfo_t is valid, and waitid only writes to it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` outlives the call; WNOWAIT leaves the child for the harness to reap.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("waitid on the app");
    }
    // SAFETY: waitid filled it in; with WNOHANG and no exit, si_pid stays 0.
    let (ended, status) = unsafe { (info.si_pid(), info.si_status()) };
    Ok((ended != 0).then_some(status))
}

/// Selects `cluster` and waits until it is connected and its kinds are discovered.
pub fn connect(app: &App, cluster: &str) -> Result<Value> {
    app.select_cluster(cluster)?;
    app.wait_for("/active_cluster", Equals(cluster.into()))?;
    app.wait_for(
        "/clusters",
        Any(serde_json::json!({"name": cluster, "connection": "connected"})),
    )?;
    // Discovery done: the navigation lists the kinds, and views can open.
    Ok(app.wait_for("/nav/status", Equals("ready".into()))?)
}

/// Opens `kind`'s list and waits until it is live with at least `rows` rows shown.
pub fn open_live(app: &App, kind: &str, rows: usize) -> Result<Value> {
    app.open_view(kind)?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    Ok(app.wait_for("/list/rows", Test::at_least(rows as u32))?)
}

/// The names of the rows shown.
pub fn row_names(state: &Value) -> Vec<String> {
    state["list"]["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The sidebar's context names, in order.
pub fn cluster_names(state: &Value) -> Vec<String> {
    state["clusters"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter_map(|c| c["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// One cluster's entry in the sidebar.
pub fn sidebar<'a>(state: &'a Value, name: &str) -> &'a Value {
    state["clusters"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["name"] == name))
        .unwrap_or(&Value::Null)
}

/// Two fake servers (contexts `kind-fake-a` and `kind-fake-b`) in one kubeconfig in `scratch`.
pub struct TwoClusters {
    pub a: FakeApiServer,
    pub b: FakeApiServer,
    pub scratch: Scratch,
}

impl TwoClusters {
    pub const A: &str = "kind-fake-a";
    pub const B: &str = "kind-fake-b";

    pub fn start() -> Result<TwoClusters> {
        let a = FakeApiServer::start_blocking(Options::default())?;
        let b = FakeApiServer::start_blocking(Options::default())?;
        let scratch = Scratch::new()?;
        write_kubeconfigs(
            &scratch.path().join("kubeconfig"),
            &[(&a, Self::A, None), (&b, Self::B, None)],
        )?;
        Ok(TwoClusters { a, b, scratch })
    }

    pub fn kubeconfig(&self) -> std::path::PathBuf {
        self.scratch.path().join("kubeconfig")
    }
}

/// Writes a kubeconfig file by hand.
pub fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text)?;
    Ok(())
}

/// Polls `observe` until `done` holds on what it returns (for things outside the app's state,
/// such as the fake server's open watches), for up to [`WAIT`]. A timeout names `what` and the
/// last value seen.
pub fn until<T: Debug>(
    what: &str,
    mut observe: impl FnMut() -> T,
    done: impl Fn(&T) -> bool,
) -> Result<T> {
    let deadline = Instant::now() + WAIT;
    loop {
        let seen = observe();
        if done(&seen) {
            return Ok(seen);
        }
        if Instant::now() > deadline {
            bail!("timed out after {WAIT:?} waiting for {what}; last saw {seen:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A local port nothing listens on: one the OS just handed out, released again.
pub fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free local port")
}
