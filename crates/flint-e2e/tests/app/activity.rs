//! UP-16 and UP-93: the app beside a separate `flint mcp` process on one config directory.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::mcp::Mcp;
use serde_json::{Value, json};

use crate::support::*;

const CLUSTER: &str = TwoClusters::A;
const NAMESPACE: &str = "demo";
const POD: &str = "web-69c6f74b8b-qv4xv";
/// Only in what the API server returns: it must never be on disk.
const RETURNED: &str = "placeholder-returned-content";

/// Settings exposing [`CLUSTER`] at `server`, limited to [`NAMESPACE`].
fn exposing(server: &str) -> String {
    format!(
        "[[ai_exposed_clusters]]\ncontext = \"{CLUSTER}\"\nserver = \"{server}\"\n\
         namespaces = [\"{NAMESPACE}\"]\n"
    )
}

/// The rows of [`CLUSTER`]'s activity tab, newest first.
fn activity_rows(state: &Value) -> Vec<String> {
    state["dock"]["tabs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["kind"] == "agent_activity" && t["cluster"]["context"] == CLUSTER)
        .and_then(|t| t["activity"]["rows"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|row| row.as_str().map(str::to_owned))
        .collect()
}

/// Waits until the tab has `count` rows, the newest ending in `row`.
fn wait_for_row(app: &App, count: usize, row: &str) -> Result<()> {
    until(
        &format!("activity row {count}: {row}"),
        || app.state().map(|s| activity_rows(&s)).unwrap_or_default(),
        |rows| rows.len() == count && rows[0].ends_with(row),
    )?;
    Ok(())
}

/// A reply's size as the view shows it.
fn size(text: &str) -> String {
    match text.len() {
        bytes @ 0..1024 => format!("{bytes} B"),
        bytes => format!("{:.1} KB", bytes as f64 / 1024.),
    }
}

/// Every regular file below `dir` (the automation socket is not one).
fn files_below(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            found.extend(files_below(&path)?);
        } else if path.is_file() {
            found.push(path);
        }
    }
    Ok(found)
}

/// Waits until `running` turns false.
fn wait_gone(what: &str, running: impl Fn() -> bool) -> Result<()> {
    until(what, running, |still| !still)?;
    Ok(())
}

/// Where an MCP server started by [`logged_mcp`] keeps its files.
struct McpFiles {
    home: PathBuf,
    pid_file: PathBuf,
}

impl McpFiles {
    /// Waits for the server to finish, so its queued log lines are on disk, and returns every
    /// file in its home. Its rotating log must be among them, or a scan would prove nothing.
    fn after_exit(&self) -> Result<Vec<PathBuf>> {
        let pid: libc::pid_t = std::fs::read_to_string(&self.pid_file)?.trim().parse()?;
        // SAFETY: signal 0 only checks that the process exists.
        wait_gone("the MCP server to exit", || unsafe {
            libc::kill(pid, 0) == 0
        })?;
        let files = files_below(&self.home)?;
        let log = files.iter().find(|f| f.ends_with("flint.log"));
        let log = log.with_context(|| format!("no log file in {}", self.home.display()))?;
        anyhow::ensure!(std::fs::metadata(log)?.len() > 0, "the MCP log is empty");
        Ok(files)
    }
}

/// `flint mcp` logging at debug level into a home the test can read: [`Mcp`] keeps its own home
/// private and removes it. The script stays the server's parent, so dropping the [`Mcp`] kills
/// only the script and closes the server's stdin, and the server flushes its log as it exits.
fn logged_mcp(app: &App, two: &TwoClusters, name: &str) -> Result<(Mcp, McpFiles)> {
    let dir = two.scratch.path().join(name);
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let pid_file = dir.join("pid");
    let script = dir.join("mcp");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nHOME='{home}' XDG_STATE_HOME='{home}' \
             FLINT_LOG=warn,flint=debug,flint_app=debug,flint_core=debug,flint_mcp=debug \
             '{binary}' \"$@\" <&0 &\necho $! > '{pid}'\nwait\n",
            home = home.display(),
            binary = app.binary().canonicalize()?.display(),
            pid = pid_file.display(),
        ),
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
    let config_dir = app.config_dir();
    // A process forked by another test while the script was open for writing holds it open
    // until it execs, and until then running the script fails with ETXTBSY.
    for _ in 0..100 {
        match Mcp::flint(&script, two.kubeconfig(), &config_dir) {
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error)
                    == Some(libc::ETXTBSY) =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            started => return Ok((started?, McpFiles { home, pid_file })),
        }
    }
    let mcp = Mcp::flint(&script, two.kubeconfig(), &config_dir)?;
    Ok((mcp, McpFiles { home, pid_file }))
}

/// Whether the pod log tab displays `line`.
fn log_line_shown(state: &Value, line: &str) -> bool {
    state["dock"]["tabs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["kind"] == "pod_logs")
        .filter_map(|t| t["logs"]["last_lines"].as_array())
        .flatten()
        .any(|l| l["text"] == line)
}

// covers UP-16: the open tab lists each call of two separate processes as it lands, with its
// tool, target, size and outcome, and no returned content is stored.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_16_activity_view_follows_separate_processes() -> Result<()> {
    let two = TwoClusters::start()?;
    let log = format!("first {RETURNED}\nsecond {RETURNED}\n");
    two.a.set_log(NAMESPACE, POD, &log);
    let app = App::launch(AppOptions::new(two.kubeconfig()).settings(&exposing(&two.a.url())))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    app.wait_for("/clusters", Any(json!({"name": CLUSTER, "ai": "exposed"})))?;
    // The agent contacts the exposed cluster; the app never selects it.
    app.mark_selected(CLUSTER);
    app.dispatch("workspace::ShowAgentActivity", json!({"cluster": CLUSTER}))?;
    app.wait_for("/dock/tabs", Any(json!({"kind": "agent_activity"})))?;

    let (mut one, one_files) = logged_mcp(&app, &two, "mcp-one")?;
    let (mut two_mcp, two_files) = logged_mcp(&app, &two, "mcp-two")?;

    let args = json!({"cluster": CLUSTER, "namespace": NAMESPACE, "pod": POD, "tail_lines": 1});
    let reply = one.call("get_pod_logs", args)?;
    assert!(reply.text.contains(RETURNED), "{}", reply.text);
    let expected = format!(
        "get_pod_logs pods {NAMESPACE}/{POD} — 1 item, {}, cut short",
        size(&reply.text)
    );
    wait_for_row(&app, 1, &expected)?;

    let args = json!({"cluster": CLUSTER, "kind": "pods", "namespace": NAMESPACE});
    let reply = two_mcp.call("list_resources", args)?;
    assert!(!reply.is_error, "{}", reply.text);
    let listed: Value = serde_json::from_str(&reply.text)?;
    let items = listed["rows"].as_array().map_or(0, Vec::len);
    let expected = format!(
        "list_resources pods in {NAMESPACE} — {items} items, {}",
        size(&reply.text)
    );
    wait_for_row(&app, 2, &expected)?;

    let args = json!({"cluster": CLUSTER, "kind": "pods", "namespace": "kube-system"});
    let reply = one.call("list_resources", args)?;
    assert!(reply.is_error, "{}", reply.text);
    let expected = format!(
        "list_resources pods in kube-system — denied: outside what agents may read, {}",
        size(&reply.text)
    );
    wait_for_row(&app, 3, &expected)?;

    // A call on a cluster that is not exposed is not this cluster's row.
    let args = json!({"cluster": TwoClusters::B, "kind": "pods", "namespace": NAMESPACE});
    assert!(two_mcp.call("list_resources", args)?.is_error);

    let args =
        json!({"cluster": CLUSTER, "kind": "pods", "namespace": NAMESPACE, "name": "missing"});
    let reply = two_mcp.call("get_resource", args)?;
    assert!(reply.is_error, "{}", reply.text);
    let expected = format!(
        "get_resource pods {NAMESPACE}/missing — failed, {}",
        size(&reply.text)
    );
    wait_for_row(&app, 4, &expected)?;

    let reply = one.call("list_clusters", json!({}))?;
    assert!(!reply.is_error, "{}", reply.text);
    let expected = format!("list_clusters — — 1 item, {}", size(&reply.text));
    wait_for_row(&app, 5, &expected)?;

    drop((one, two_mcp));
    let mut written = one_files.after_exit()?;
    written.extend(two_files.after_exit()?);
    // The app's scratch directory holds its config, its home and its stderr log.
    let app_scratch = app.quit()?;
    written.extend(files_below(app_scratch.path())?);
    for path in written {
        let bytes = std::fs::read(&path)?;
        assert!(
            !String::from_utf8_lossy(&bytes).contains(RETURNED),
            "returned content in {}",
            path.display()
        );
    }
    Ok(())
}

// covers UP-93: browsing a cluster, reading logs and watching an agent's activity never start an
// AI command line tool found on the PATH.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_93_nothing_sent_without_action() -> Result<()> {
    let two = TwoClusters::start()?;
    let bin = two.scratch.path().join("bin");
    std::fs::create_dir(&bin)?;
    let ran = two.scratch.path().join("claude-ran");
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!("#!/bin/sh\necho run >> '{}'\n", ran.display()),
    )?;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755))?;
    // The detector works: running the stand-in leaves its mark.
    assert!(Command::new(&claude).status()?.success());
    assert!(ran.exists());
    std::fs::remove_file(&ran)?;

    let line = "a known log line";
    two.a.set_log(NAMESPACE, POD, &format!("{line}\n"));
    let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display());
    let settings = format!("path = \"{path}\"\n{}", exposing(&two.a.url()));
    let app = App::launch(
        AppOptions::new(two.kubeconfig())
            .settings(&settings)
            .env("PATH", &path),
    )?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    connect(&app, CLUSTER)?;
    open_live(&app, "pods", 1)?;
    app.select_row(Some(NAMESPACE), POD)?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    app.dispatch("detail::OpenLogs", Value::Null)?;
    app.wait_for("/dock/tabs", Any(json!({"kind": "pod_logs"})))?;
    until(
        "the known line in the log tab",
        || app.state().is_ok_and(|s| log_line_shown(&s, line)),
        |shown| *shown,
    )?;
    app.dispatch("workspace::ShowAgentActivity", json!({"cluster": CLUSTER}))?;
    app.wait_for("/dock/tabs", Any(json!({"kind": "agent_activity"})))?;

    let mut mcp = Mcp::flint(app.binary(), two.kubeconfig(), &app.config_dir())?;
    let args = json!({"cluster": CLUSTER, "namespace": NAMESPACE, "pod": POD});
    assert!(!mcp.call("get_pod_logs", args)?.is_error);
    until(
        "the call in the activity tab",
        || app.state().map(|s| activity_rows(&s)).unwrap_or_default(),
        |rows| !rows.is_empty(),
    )?;
    drop(mcp);

    // After the app and everything in its process group have gone, so a late start counts too.
    let group = libc::pid_t::try_from(app.pid())?;
    app.quit()?;
    // SAFETY: signal 0 only checks that a process of the group exists.
    wait_gone("the app's child processes to exit", || unsafe {
        libc::killpg(group, 0) == 0
    })?;
    assert!(!ran.exists(), "an AI tool ran without an action");
    Ok(())
}
