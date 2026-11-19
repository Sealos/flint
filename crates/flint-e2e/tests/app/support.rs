//! Helpers shared by the app e2e tests.

use std::path::Path;

use anyhow::Result;
use flint_e2e::app::{App, Test, WAIT};
use flint_e2e::fake_api::{FakeApiServer, Options, write_kubeconfigs};
use flint_e2e::scratch::Scratch;
use serde_json::Value;

pub use flint_e2e::app::Test::{Any, AtLeast, Equals, Exists};

/// Selects `cluster` and waits until it is connected and its kinds are discovered.
pub fn connect(app: &App, cluster: &str) -> Result<Value> {
    app.select_cluster(cluster)?;
    app.wait_for("/active_cluster", Equals(cluster.into()), WAIT)?;
    app.wait_for(
        "/clusters",
        Any(serde_json::json!({"name": cluster, "connection": "connected"})),
        WAIT,
    )?;
    // Discovery done: the navigation lists the kinds, and views can open.
    Ok(app.wait_for("/nav/status", Equals("ready".into()), WAIT)?)
}

/// Opens `kind`'s list and waits until it is live with at least `rows` rows shown.
pub fn open_live(app: &App, kind: &str, rows: usize) -> Result<Value> {
    app.open_view(kind)?;
    app.wait_for("/list/sync", Equals("live".into()), WAIT)?;
    Ok(app.wait_for("/list/rows", AtLeast(rows as f64), WAIT)?)
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

/// Two fake servers (contexts `fake-a` and `fake-b`) in one kubeconfig in `scratch`.
pub struct TwoClusters {
    pub a: FakeApiServer,
    pub b: FakeApiServer,
    pub scratch: Scratch,
}

impl TwoClusters {
    pub const A: &str = "fake-a";
    pub const B: &str = "fake-b";

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

/// Waits for the value at `pointer` to pass `test`, with the default wait.
pub fn wait(app: &App, pointer: &str, test: Test) -> Result<Value> {
    Ok(app.wait_for(pointer, test, WAIT)?)
}
