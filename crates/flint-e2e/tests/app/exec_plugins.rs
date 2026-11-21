//! UP-04 with exec auth plugins (docs/USER-PATHS.md): one that is missing and one that never
//! answers, each next to a healthy cluster, and Retry on both.
//!
//! The plugins are small shell stubs in the test's scratch directory. The hanging one takes the
//! real `CONNECT_TIMEOUT` to fail, so its test lasts a little over a minute.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions, WAIT};
use flint_e2e::fake_api::kubeconfig;
use kube::config::{ExecConfig, Kubeconfig};
use serde_json::{Value, json};

use crate::support::*;

/// The plugin's file name, the only part of its path the app may show.
const PLUGIN: &str = "flint-e2e-stub-plugin";

/// How long the hanging plugin's attempt may take to be reported: `CONNECT_TIMEOUT` and a margin.
const HANG_REPORTED: Duration = Duration::from_secs(90);

/// Answers with a credential once the `answer` file exists, and until then waits. It records
/// each run's process id, and ends by itself once the scratch directory is gone (or after five
/// minutes), so a failed test leaves nothing running.
const STUB: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo $$ >> "$dir/starts"
waited=0
while [ ! -e "$dir/answer" ]; do
  [ -d "$dir" ] && [ "$waited" -lt 300 ] || exit 1
  sleep 1
  waited=$((waited + 1))
done
printf '%s' '{"apiVersion":"client.authentication.k8s.io/v1beta1","kind":"ExecCredential","status":{"token":"e2e-stub-token"}}'
"#;

/// A stub plugin's directory. `fake-a` runs the program at [`Plugin::path`], `fake-b` has no
/// plugin.
struct Plugin {
    dir: PathBuf,
}

impl Plugin {
    /// Sets `two`'s `fake-a` to authenticate with the plugin, which is not installed yet.
    fn configure(two: &TwoClusters) -> Result<Plugin> {
        let dir = two.scratch.path().join("plugin");
        std::fs::create_dir(&dir)?;
        let plugin = Plugin { dir };
        let path = two.kubeconfig();
        let mut config = Kubeconfig::read_from(&path)?;
        let user = config
            .auth_infos
            .iter_mut()
            .find(|user| user.name == TwoClusters::A)
            .and_then(|user| user.auth_info.as_mut())
            .context("fake-a's user")?;
        user.exec = Some(serde_json::from_value::<ExecConfig>(json!({
            "apiVersion": "client.authentication.k8s.io/v1beta1",
            "command": plugin.path(),
            "interactiveMode": "Never",
        }))?);
        kubeconfig::write(&path, &config)?;
        Ok(plugin)
    }

    fn path(&self) -> PathBuf {
        self.dir.join(PLUGIN)
    }

    /// Puts the stub in place in one step, so the app never runs a half-written file.
    fn install(&self) -> Result<()> {
        let partial = self.dir.join("partial");
        std::fs::write(&partial, STUB)?;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&partial, self.path())?;
        Ok(())
    }

    /// From now on the stub answers with a credential.
    fn answer(&self) -> Result<()> {
        std::fs::write(self.dir.join("answer"), "")?;
        Ok(())
    }

    /// The process id of each run so far.
    fn starts(&self) -> Vec<i32> {
        read_pids(&self.dir.join("starts"))
    }
}

fn read_pids(path: &Path) -> Vec<i32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Whether a process with this id exists (a zombie not reaped yet counts).
fn exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks the process; nothing is sent.
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn main_error(state: &Value) -> &str {
    state["main"]["error"].as_str().unwrap_or_default()
}

/// Retries the shown cluster and waits until it is connected and listing pods.
fn retry_connects(app: &App) -> Result<()> {
    app.dispatch("workspace::RetryConnection", Value::Null)?;
    app.wait_for("/main/view", Equals("connected".into()))?;
    app.wait_for("/nav/status", Equals("ready".into()))?;
    open_live(app, "pods", 17)?;
    Ok(())
}

// covers UP-04: a missing plugin is named in plain words, without its directory, at once; the
// other cluster works, and Retry connects once the plugin is installed.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_04_missing_exec_plugin_is_named_and_retries() -> Result<()> {
    let two = TwoClusters::start()?;
    let plugin = Plugin::configure(&two)?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;

    let started = Instant::now();
    app.select_cluster(TwoClusters::A)?;
    let state = app.wait_for("/main/view", Equals("failed".into()))?;
    assert!(started.elapsed() < WAIT, "{:?}", started.elapsed());
    let error = main_error(&state);
    assert!(
        error.starts_with(&format!("exec auth plugin `{PLUGIN}` not found on PATH")),
        "{error}"
    );
    assert!(error.contains("settings.toml"), "{error}");
    assert!(
        !error.contains(&plugin.dir.display().to_string()),
        "the plugin's directory is hidden: {error}"
    );
    assert!(
        two.a.requests().is_empty(),
        "no credential, so no request: {:?}",
        two.a.requests()
    );

    connect(&app, TwoClusters::B)?;
    open_live(&app, "pods", 17)?;

    app.select_cluster(TwoClusters::A)?;
    app.wait_for("/main/view", Equals("failed".into()))?;
    plugin.install()?;
    plugin.answer()?;
    retry_connects(&app)?;
    assert_eq!(plugin.starts().len(), 1, "one run, from the retry");
    assert!(!two.a.requests().is_empty());
    Ok(())
}

// covers UP-04: a plugin that never answers fails at the connect timeout and is killed; the other
// cluster connects and the UI answers meanwhile, and Retry runs the plugin again.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_04_hanging_exec_plugin_times_out_and_retries() -> Result<()> {
    let two = TwoClusters::start()?;
    let plugin = Plugin::configure(&two)?;
    plugin.install()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;

    app.select_cluster(TwoClusters::A)?;
    app.wait_for("/main/view", Equals("connecting".into()))?;
    let starts = until("the plugin to start", || plugin.starts(), |s| s.len() == 1)?;
    let hung = starts[0];

    // The other cluster connects and lists while the plugin hangs, and the UI keeps answering.
    connect(&app, TwoClusters::B)?;
    open_live(&app, "pods", 17)?;
    assert!(app.actions()?["actions"].as_array().is_some());
    let state = app.state()?;
    assert_eq!(sidebar(&state, TwoClusters::A)["connection"], "connecting");
    assert!(exists(hung), "the plugin is still waiting");
    assert!(two.a.requests().is_empty(), "{:?}", two.a.requests());

    // Back on the hanging cluster: the same attempt runs out.
    app.select_cluster(TwoClusters::A)?;
    let state = app.wait_for_within("/main/view", Equals("failed".into()), HANG_REPORTED)?;
    let error = main_error(&state);
    assert!(error.contains("1m0s"), "{error}");
    assert!(
        error.contains(&format!("exec plugin `{PLUGIN}`")),
        "{error}"
    );
    assert!(
        !error.contains(&plugin.dir.display().to_string()),
        "the plugin's directory is hidden: {error}"
    );
    until("the plugin to be killed", || exists(hung), |alive| !alive)?;
    assert_eq!(plugin.starts(), [hung], "no second run without a retry");

    plugin.answer()?;
    retry_connects(&app)?;
    assert_eq!(plugin.starts().len(), 2, "Retry ran the plugin again");
    assert!(!two.a.requests().is_empty());
    Ok(())
}
