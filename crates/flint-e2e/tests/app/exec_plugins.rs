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

/// Answers with a credential once the `answer` file exists, and until then waits. Each run
/// records its process id and starts a child that waits the same way and records its own, with
/// its output detached so it cannot hold the plugin's pipe open. Both end by themselves once the
/// scratch directory is gone (or after five minutes).
const STUB: &str = r#"#!/bin/sh
dir=$(dirname "$0")
wait_for_answer() {
  waited=0
  while [ ! -e "$dir/answer" ]; do
    [ -d "$dir" ] && [ "$waited" -lt 300 ] || exit 1
    sleep 1
    waited=$((waited + 1))
  done
}
if [ "$1" = child ]; then
  wait_for_answer
  exit 0
fi
echo $$ >> "$dir/starts"
"$0" child </dev/null >/dev/null 2>&1 &
echo $! >> "$dir/children"
wait_for_answer
printf '%s' '{"apiVersion":"client.authentication.k8s.io/v1beta1","kind":"ExecCredential","status":{"token":"e2e-stub-token"}}'
"#;

/// A stub plugin's directory. `fake-a` runs the program at [`Plugin::path`], `fake-b` has no
/// plugin.
struct Plugin {
    dir: PathBuf,
}

impl Plugin {
    /// Sets `two`'s `kind-fake-a` to authenticate with the plugin, which is not installed yet.
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
            .context("kind-fake-a's user")?;
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

    /// The process id of the child each run started.
    fn children(&self) -> Vec<i32> {
        read_pids(&self.dir.join("children"))
    }
}

/// Kills whatever is left of the stub, also when a test fails. A recorded id counts only while
/// it still runs this stub's file, since the system may have given it to another process.
impl Drop for Plugin {
    fn drop(&mut self) {
        let program = self.path().display().to_string();
        for pid in self.starts().into_iter().chain(self.children()) {
            if runs(pid, &program) {
                // SAFETY: kill takes plain integers, and `pid` was just seen running the stub.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
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

/// Whether process `pid` has `program` on its command line.
fn runs(pid: i32, program: &str) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|ps| String::from_utf8_lossy(&ps.stdout).contains(program))
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
// other cluster stays connected and still works, and Retry connects once the plugin is installed.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_04_missing_exec_plugin_is_named_and_retries() -> Result<()> {
    let two = TwoClusters::start()?;
    let plugin = Plugin::configure(&two)?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;

    connect(&app, TwoClusters::B)?;
    open_live(&app, "pods", 17)?;

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

    assert_eq!(sidebar(&state, TwoClusters::B)["connection"], "connected");

    // `fake-a` stays selected: selecting it again would start an attempt of its own.
    plugin.install()?;
    plugin.answer()?;
    retry_connects(&app)?;
    assert_eq!(plugin.starts().len(), 1, "one run, from the retry");
    assert!(!two.a.requests().is_empty());

    connect(&app, TwoClusters::B)?;
    open_live(&app, "pods", 17)?;
    Ok(())
}

// covers UP-04: switching away from a cluster whose plugin hangs ends the attempt and kills the
// plugin with the child it started, and the other cluster connects and lists. Selected again, the
// plugin runs anew, fails at the connect timeout while the UI answers, and is killed the same way;
// Retry runs it once more.
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
    let abandoned = starts[0];
    let children = until("its child to start", || plugin.children(), |c| c.len() == 1)?;
    let abandoned_child = children[0];
    assert!(exists(abandoned), "the plugin is waiting");
    assert!(exists(abandoned_child), "its child is waiting");

    // A credential that runs a program does not go on in the background.
    connect(&app, TwoClusters::B)?;
    open_live(&app, "pods", 17)?;
    let state = app.state()?;
    assert_ne!(sidebar(&state, TwoClusters::A)["connection"], "connecting");
    until(
        "the plugin to be killed",
        || exists(abandoned),
        |alive| !alive,
    )?;
    until(
        "its child to be killed",
        || exists(abandoned_child),
        |alive| !alive,
    )?;
    assert!(two.a.requests().is_empty(), "{:?}", two.a.requests());

    // Back on the hanging cluster: a new attempt, which stays selected until it runs out.
    let started = Instant::now();
    app.select_cluster(TwoClusters::A)?;
    let starts = until(
        "the plugin to run again",
        || plugin.starts(),
        |s| s.len() == 2,
    )?;
    let hung = starts[1];
    let children = until("its new child", || plugin.children(), |c| c.len() == 2)?;
    let child = children[1];
    assert!(app.actions()?["actions"].as_array().is_some());
    let left = HANG_REPORTED.saturating_sub(started.elapsed());
    let state = app.wait_for_within("/main/view", Equals("failed".into()), left)?;
    assert!(two.a.requests().is_empty(), "{:?}", two.a.requests());
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
    until("its child to be killed", || exists(child), |alive| !alive)?;
    assert_eq!(
        plugin.starts(),
        [abandoned, hung],
        "no further run without a retry"
    );

    plugin.answer()?;
    retry_connects(&app)?;
    assert_eq!(plugin.starts().len(), 3, "Retry ran the plugin again");
    assert!(!two.a.requests().is_empty());
    Ok(())
}
