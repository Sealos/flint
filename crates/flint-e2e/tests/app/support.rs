//! Helpers shared by the app e2e tests.

use std::fmt::Debug;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use flint_e2e::app::{App, AppOptions, WAIT};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{
    FakeApiServer, FakePod, Gvr, Options, RequestRecord, Verb, WriteRecord, write_kubeconfigs,
};
use flint_e2e::scratch::Scratch;
use serde_json::{Value, json};

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

/// What the connected cluster's feed lists on a timer, whatever view is open: the metrics behind
/// the usage columns and trends, claims, and Warning events. Each a path, and the one field
/// selector it carries, if any.
const FEED_POLLS: [(&str, Option<&str>); 4] = [
    ("/apis/metrics.k8s.io/v1beta1/nodes", None),
    ("/apis/metrics.k8s.io/v1beta1/pods", None),
    ("/api/v1/persistentvolumeclaims", None),
    ("/api/v1/events", Some("fieldSelector=type%3DWarning")),
];

/// Whether `request` is one of the feed's timed list GETs, which run on the read client while the
/// cluster is connected. A check that an action or a search sent nothing leaves only these out.
/// A list with another selector, such as one object's events, is not one of them.
pub fn is_feed_poll(request: &RequestRecord) -> bool {
    let param = |name: &str| {
        let prefix = format!("{name}=");
        request.query.split('&').find(|p| p.starts_with(&prefix))
    };
    request.method == http::Method::GET
        && request.verb == Verb::List
        && param("labelSelector").is_none()
        && FEED_POLLS
            .iter()
            .any(|&(path, fields)| request.path == path && param("fieldSelector") == fields)
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

/// Settings that protect a loopback cluster, so no write test depends on the default.
pub const PROTECTED: &str = "protect_local_clusters = true\n";

/// Launches the app on `cluster`, with its protection on, and waits for the connection.
pub fn launch_protected(cluster: &TestCluster) -> Result<App> {
    let app = App::launch(AppOptions::new(&cluster.kubeconfig).settings(PROTECTED))?;
    connect(&app, &cluster.context)?;
    assert_eq!(
        sidebar(&app.state()?, &cluster.context)["protection"],
        "protected"
    );
    Ok(app)
}

/// Answers the dialog of `action` (`UnlockCluster`, `TrustCluster`) and waits for `protection`.
pub fn lift_protection(app: &App, context: &str, action: &str, protection: &str) -> Result<()> {
    app.dispatch(&format!("workspace::{action}"), json!({"cluster": context}))?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": context, "protection": protection})),
    )?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    Ok(())
}

/// Selects `name` and waits until its object has loaded.
pub fn select(app: &App, namespace: Option<&str>, name: &str) -> Result<()> {
    app.select_row(namespace, name)?;
    app.wait_for("/detail/name", Equals(name.into()))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    Ok(())
}

/// Whether the registry offers `action` now.
pub fn offered(app: &App, action: &str) -> Result<bool> {
    let actions = app.actions()?;
    let entry = actions["actions"]
        .as_array()
        .and_then(|all| all.iter().find(|a| a["name"] == action))
        .with_context(|| format!("no {action}"))?;
    Ok(entry["enabled"] == true)
}

/// Waits until the registry offers `action`.
pub fn wait_offered(app: &App, action: &str) -> Result<()> {
    until(action, || offered(app, action).unwrap_or(false), |yes| *yes)?;
    Ok(())
}

/// Waits until the registry offers `action`, then opens its dialog: returns the dialog's text.
pub fn open_dialog(app: &App, action: &str) -> Result<String> {
    wait_offered(app, action)?;
    app.dispatch(action, Value::Null)?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    Ok(state["dialog"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// Closes the open dialog with `key` and waits until it is gone.
pub fn dismiss(app: &App, key: &str) -> Result<()> {
    app.press_key(key)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    Ok(())
}

/// Waits until `count` of the writes `read` returns were answered.
pub fn answered_by(count: usize, read: impl Fn() -> Vec<WriteRecord>) -> Result<Vec<WriteRecord>> {
    until("the write's answer", read, |writes| {
        writes.len() >= count && writes.iter().all(|w| w.status.is_some())
    })
}

/// Waits until `count` writes were answered.
pub fn answered(fake: &FakeApiServer, count: usize) -> Result<Vec<WriteRecord>> {
    answered_by(count, || fake.writes())
}

/// Adds a node called `name`, copied from the first one, with no pods on it.
pub fn add_node(fake: &FakeApiServer, name: &str, unschedulable: bool) -> Result<Gvr> {
    let nodes = fake.gvr("nodes");
    let first = fake.rows(&nodes)[0].name().to_owned();
    let mut node = fake.object(&nodes, None, &first).context("no node")?;
    node["metadata"]["name"] = name.into();
    node["metadata"]["uid"] = format!("uid-{name}").into();
    node["spec"]["unschedulable"] = unschedulable.into();
    fake.set_object(&nodes, node);
    Ok(nodes)
}

/// A running pod on `node`, controlled by an `owner` of that kind.
pub fn pod_on_node(name: &str, node: &str, owner: &str) -> FakePod {
    FakePod::from_json(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name, "namespace": "demo", "uid": format!("uid-{name}"),
            "resourceVersion": "5002",
            "creationTimestamp": "2026-01-01T00:00:00Z",
            "labels": {},
            "ownerReferences": [{"apiVersion": "apps/v1", "kind": owner, "name": "owner",
                                 "uid": "uid-owner", "controller": true}],
        },
        "spec": {"nodeName": node, "containers": [{"name": "c", "image": "busybox:1.37.0"}]},
        "status": {"phase": "Running", "containerStatuses": [{
            "name": "c", "ready": true, "restartCount": 0, "image": "busybox:1.37.0",
            "imageID": "", "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}},
        }]},
    }))
}
