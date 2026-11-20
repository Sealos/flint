//! M2 user paths: shells in the dock (UP-41), and log and shell sessions that outlive
//! navigation (UP-42, UP-24), against the fake API server's exec WebSocket, plus a smoke test of
//! the shell on the kind dev cluster.
//!
//! The log paths are in `m2.rs`. Not here, and why:
//!
//! - **Resizing the window** (UP-41): the automation channel cannot resize the window. The dock's
//!   height (`workspace::SetDockHeight`) and the side panes' widths (`workspace::SetPaneWidth`)
//!   resize the terminal through the same layout, and are tested.
//! - **Colours, wide characters, full-screen programs, the wheel and Cmd-C** (UP-41): the terminal
//!   model's and the view's tests (`flint-core`'s `terminal.rs`, `ui/terminal_tests.rs`). The
//!   state carries the screen's text, not its colours.
//! - **A huge clipboard** (UP-41): `ui/terminal_tests.rs`; filling the clipboard with megabytes
//!   here proves nothing more.
//! - **A context re-pointed at another API server** (UP-41): `ui/dock_tests.rs`.
//! - **Close Window** (UP-42: Cmd-W away from the dock closes the window): it closes
//!   `cx.active_window()`, and the app under test runs in the background, so it has no active
//!   window and the action does nothing. `ui/workspace_tests.rs` covers its routing; #174
//!   tracks the app test, with an exit observed apart from `App::quit`.
//! - **Saving the dock's height on quit** (UP-42): the height is saved when it changes, so the
//!   relaunch in `up_42_sessions_outlive_navigation` proves it is read back, not that quitting
//!   saves it.
//!
//! The paste test goes through the system clipboard, as a user's copy and paste does (the app's
//! own Copy, then Paste). It puts back the plain text the clipboard held before; anything else it
//! held (an image, rich text) is lost.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::{
    ContainerLog, ExecEnd, ExecImage, FakeApiServer, FakeExec, FakePod, Fault, Gvr, Options,
    TtySize,
};
use serde_json::{Value, json};

use crate::support::*;

/// The fixtures' web pod: nginx, which has bash.
const WEB: &str = "web-69c6f74b8b-qv4xv";
/// The fixtures' podinfo pod: only `sh`, as user `app`.
const PODINFO: &str = "podinfo-c6497595d-b8chj";
/// A pod the tests add, with three containers.
const MULTI: &str = "multi-1";

/// The index of tab `id` in the state's dock.
fn tab_index(state: &Value, id: u64) -> Result<usize> {
    state["dock"]["tabs"]
        .as_array()
        .and_then(|tabs| tabs.iter().position(|t| t["id"] == id))
        .with_context(|| format!("no tab {id} in {}", state["dock"]))
}

/// The tab at `index`.
fn tab_at(state: &Value, index: usize) -> &Value {
    &state["dock"]["tabs"][index]
}

/// The ids of the dock's tabs, in order.
fn tab_ids(state: &Value) -> Vec<u64> {
    state["dock"]["tabs"]
        .as_array()
        .map(|tabs| tabs.iter().filter_map(|t| t["id"].as_u64()).collect())
        .unwrap_or_default()
}

/// A shell tab's screen rows.
fn screen(state: &Value, index: usize) -> Vec<String> {
    tab_at(state, index)["shell"]["screen"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The pointer to a field of the tab at `index`.
fn at(index: usize, field: &str) -> String {
    format!("/dock/tabs/{index}/{field}")
}

/// Waits until the shell tab at `index` shows a row that reads exactly `row`.
fn wait_for_row(app: &App, index: usize, row: &str) -> Result<Value> {
    Ok(app.wait_for(&at(index, "shell/screen"), Contains(row.into()))?)
}

/// [`wait_for_row`] with its own timeout.
fn wait_for_row_within(app: &App, index: usize, row: &str, within: Duration) -> Result<Value> {
    Ok(app.wait_for_within(&at(index, "shell/screen"), Contains(row.into()), within)?)
}

/// Waits until the newest exec session's stdin holds `bytes`.
fn wait_for_stdin(exec: &FakeExec, bytes: &[u8]) -> Result<Vec<u8>> {
    until(
        &format!("stdin to hold {:?}", String::from_utf8_lossy(bytes)),
        || exec.stdin(),
        |stdin| stdin.windows(bytes.len()).any(|w| w == bytes),
    )
}

/// The newest exec session once there are at least `count`.
fn exec_number(fake: &FakeApiServer, count: usize) -> Result<FakeExec> {
    let sessions = until(
        &format!("{count} exec session(s)"),
        || fake.exec_sessions(),
        |s| s.len() >= count,
    )?;
    Ok(sessions[count - 1].clone())
}

/// Waits until the session got a resize to `size`.
fn wait_for_resize(exec: &FakeExec, size: TtySize) -> Result<()> {
    until(
        &format!("a resize to {size:?}"),
        || exec.resizes(),
        |r| r.contains(&size),
    )?;
    Ok(())
}

/// The commands of every exec request so far.
fn exec_commands(fake: &FakeApiServer) -> Vec<Vec<String>> {
    fake.exec_requests()
        .iter()
        .map(|r| r.parsed_query().command)
        .collect()
}

/// Types a command and presses Return (which reaches the pod as `\r`).
fn run(app: &App, command: &str) -> Result<()> {
    app.type_text(command)?;
    app.press_key("enter")?;
    Ok(())
}

/// The shell tab at `index`'s size, as the state says.
fn size(state: &Value, index: usize) -> TtySize {
    let shell = &tab_at(state, index)["shell"];
    let n = |f: &str| u16::try_from(shell[f].as_u64().unwrap_or(0)).unwrap_or(0);
    TtySize {
        width: n("cols"),
        height: n("rows"),
    }
}

/// Chooses the entry `label` of the open menu with the keyboard, as a user does (Down skips
/// disabled entries), and waits for the menu to close.
fn choose(app: &App, label: &str) -> Result<()> {
    let state = app.wait_for("/menu/entries", Any(json!({"label": label})))?;
    let entries = state["menu"]["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let index = entries
        .iter()
        .position(|e| e["label"] == label)
        .context("the entry")?;
    for _ in entries[..=index].iter().filter(|e| e["enabled"] != false) {
        app.press_key("down")?;
    }
    app.press_key("enter")?;
    app.wait_for("/menu", Equals(Value::Null))?;
    Ok(())
}

/// The id of the row of pod `name` in the open list.
fn row_id(state: &Value, name: &str) -> Result<String> {
    state["list"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == name))
        .and_then(|r| r["id"].as_str())
        .map(str::to_owned)
        .with_context(|| format!("no row {name}"))
}

/// Opens a new tab with `open` and waits for it: returns its id and index.
fn new_tab(app: &App, before: &[u64], open: impl FnOnce() -> Result<()>) -> Result<(u64, usize)> {
    open()?;
    let state = app.wait_for("/dock/tabs", Test::at_least(before.len() as u32 + 1))?;
    let id = tab_ids(&state)
        .into_iter()
        .find(|id| !before.contains(id))
        .context("a new tab")?;
    Ok((id, tab_index(&state, id)?))
}

/// The system clipboard's plain text, put back when dropped (anything else it held, such as an
/// image, is not restored): the paste tests copy and paste through
/// it, as a user does.
struct Clipboard(Vec<u8>);

impl Clipboard {
    fn save() -> Result<Clipboard> {
        Ok(Clipboard(Command::new("pbpaste").output()?.stdout))
    }

    fn text() -> Result<String> {
        Ok(String::from_utf8(Command::new("pbpaste").output()?.stdout)?)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        let restored = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(&self.0)?;
                }
                child.wait()
            });
        if let Err(e) = restored {
            eprintln!("could not restore the clipboard: {e}");
        }
    }
}

/// Whether the registered action `name` can be dispatched now.
fn available(app: &App, name: &str) -> Result<bool> {
    let actions = app.actions()?;
    Ok(actions["actions"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["name"] == name))
        .is_some_and(|a| a["available"] == true && a["enabled"] == true))
}

/// Prints `echo one` and `echo two` on two rows of the shell tab at `index`, selects them
/// (`terminal::SelectText`, as a drag does) and copies them with the app's Copy.
fn copy_two_lines(app: &App, index: usize) -> Result<()> {
    run(app, "echo 'echo one'; echo 'echo two'")?;
    let state = wait_for_row(app, index, "echo two")?;
    let rows = screen(&state, index);
    let row = rows
        .iter()
        .rposition(|r| r == "echo one")
        .context("the first line")?;
    if rows.get(row + 1).map(String::as_str) != Some("echo two") {
        bail!("the lines are not together: {rows:?}");
    }
    let row = u16::try_from(row)?;
    app.dispatch(
        "terminal::SelectText",
        json!({"start_row": row, "start_col": 0, "end_row": row + 1, "end_col": 7}),
    )?;
    // The selection reaches the view with the next screen: Copy is available from then on.
    until(
        "Copy to be available",
        || available(app, "terminal::CopyText"),
        |a| matches!(a, Ok(true)),
    )??;
    app.dispatch("terminal::CopyText", Value::Null)?;
    until("the copied lines on the clipboard", Clipboard::text, |t| {
        t.as_deref().is_ok_and(|t| t == "echo one\necho two")
    })??;
    Ok(())
}

// covers UP-41
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_41_shell_into_a_container() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    let state = open_live(&app, "pods", 17)?;
    let row = row_id(&state, WEB)?;

    // The row's menu offers Shell once the pod has loaded.
    app.right_click(&row)?;
    app.wait_for("/detail/name", Equals(WEB.into()))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    app.wait_for(
        "/menu/entries",
        Any(json!({"label": "Shell", "enabled": true})),
    )?;
    // The exec answers late, so the tab is seen connecting first.
    fake.inject(
        Fault::delay(Duration::from_secs(2))
            .on(&Gvr::core("pods"))
            .subresource("exec")
            .times(1),
    );
    let (shell, i) = new_tab(&app, &[], || choose(&app, "Shell"))?;
    let state = app.wait_for(&at(i, "status/state"), Equals("connecting".into()))?;
    assert_eq!(tab_at(&state, i)["kind"], "shell");
    assert_eq!(tab_at(&state, i)["status_text"], "Connecting…");
    let state = app.wait_for(&at(i, "status/state"), Equals("running".into()))?;
    let tab = tab_at(&state, i);
    assert_eq!(
        tab["status"],
        json!({"state": "running", "command": "bash"})
    );
    assert_eq!(tab["status_text"], "Running bash");
    assert_eq!(tab["title"], format!("Shell: {WEB} · nginx"));
    // The tab names its cluster.
    assert_eq!(tab["cluster"]["context"], cluster.context.as_str());
    assert_eq!(tab["cluster"]["chip"], cluster.context.as_str());
    assert_eq!(tab["shell"]["live"], true);
    assert_eq!(state["dock"]["visible"], true);
    let prompt = format!("root@{WEB}:/#");
    wait_for_row(&app, i, &prompt)?;
    app.wait_for("/focus", Equals("shell".into()))?;
    let exec = fake.last_exec().context("an exec session")?;
    assert_eq!(exec.command(), ["bash"]);
    assert_eq!(exec.container(), "nginx");
    assert!(exec.tty());
    assert_eq!(exec_commands(fake), [["bash"]]);

    // Typing reaches the pod, Return as `\r`, and the echo and output show.
    run(&app, "echo hi")?;
    wait_for_stdin(&exec, b"echo hi\r")?;
    wait_for_row(&app, i, &format!("{prompt} echo hi"))?;
    let state = wait_for_row(&app, i, "hi")?;
    // Flint sized the TTY as the tab is drawn, once, before anything was typed.
    let first = size(&state, i);
    assert_eq!(exec.resizes(), [first]);
    app.screenshot_themes("up41-shell-bash")?;

    // A taller dock is more rows: exactly one resize, and `stty size` says so.
    let height = state["dock"]["height"]
        .as_f64()
        .context("the dock's height")?;
    app.dispatch(
        "workspace::SetDockHeight",
        json!({"height": height + 120.0}),
    )?;
    let state = app.wait_for(&at(i, "shell/rows"), Test::at_least(first.height + 1))?;
    let taller = size(&state, i);
    assert_eq!(taller.width, first.width, "only the rows change");
    // kube sends stdin and resizes on separate channels: type only once the resize is in.
    wait_for_resize(&exec, taller)?;
    run(&app, "stty size")?;
    wait_for_row(&app, i, &format!("{} {}", taller.height, taller.width))?;
    assert_eq!(exec.resizes(), [first, taller]);

    // Narrower side panes are more columns. The window can't be resized over the automation
    // channel; this is the same layout path. The dock spans the list and the detail pane. When
    // the window is too narrow for every pane, the detail pane gives way first, then the
    // navigation, then the sidebar; so in a 1024-point window a narrower sidebar alone only
    // gives its room back to the squeezed navigation, and the dock keeps its width. Both side
    // panes at their narrowest widen the dock in any window over 950 points (sidebar 140,
    // navigation 150, detail 300, list 360).
    for (pane, width) in [("navigation", 150.0), ("clusters", 140.0)] {
        app.dispatch(
            "workspace::SetPaneWidth",
            json!({"pane": pane, "width": width}),
        )?;
    }
    // Then a taller dock, last: the grid it lays out has the new columns too, so once the
    // state shows the new rows it shows the final columns (the tab tells the pod a size once
    // it holds still, so the two widths may or may not reach it as separate resizes).
    app.dispatch(
        "workspace::SetDockHeight",
        json!({"height": height + 180.0}),
    )?;
    let state = app.wait_for(&at(i, "shell/rows"), Test::at_least(taller.height + 1))?;
    let resized = size(&state, i);
    if resized.width <= taller.width {
        let shot = app.request("screenshot", json!({"name": "up41-shell-narrow-window"}))?;
        let pixels = shot["width"].as_f64().unwrap_or(0.0);
        // A point is at least a pixel: a window this many pixels wide is at most as many
        // points.
        if pixels > 950.0 {
            bail!(
                "narrower side panes left the shell at {} columns (was {}) in a window \
                 {pixels} pixels wide",
                resized.width,
                taller.width
            );
        }
        eprintln!(
            "the window is {pixels} pixels wide, too narrow for the side panes to widen the \
             dock: only the rows are checked"
        );
    }
    wait_for_resize(&exec, resized)?;
    run(&app, "stty size")?;
    wait_for_row(&app, i, &format!("{} {}", resized.height, resized.width))?;
    let resizes = exec.resizes();
    assert_eq!(resizes[..2], [first, taller]);
    assert_eq!(resizes.last(), Some(&resized));
    assert_eq!(
        resizes.iter().filter(|r| **r == resized).count(),
        1,
        "the final size once: {resizes:?}"
    );

    // bash turned bracketed paste on: two lines copied from the screen go in bracketed, with no
    // question, and run only on Return.
    let clipboard = Clipboard::save()?;
    copy_two_lines(&app, i)?;
    app.dispatch("terminal::PasteText", Value::Null)?;
    wait_for_stdin(&exec, b"\x1b[200~echo one\necho two\x1b[201~")?;
    assert_eq!(app.state()?["dialog"], Value::Null, "no question for bash");
    app.press_key("enter")?;
    wait_for_row(&app, i, "one")?;
    wait_for_row(&app, i, "two")?;
    drop(clipboard);

    // Ctrl-C stops a running program: `yes` floods until it comes, then bash says 130.
    run(&app, "yes flint")?;
    wait_for_row(&app, i, "flint")?;
    app.press_key("ctrl-c")?;
    wait_for_stdin(&exec, b"yes flint\r\x03")?;
    wait_for_row(&app, i, &prompt)?;
    run(&app, "echo $?")?;
    wait_for_row(&app, i, "130")?;

    // A log tab after the shell, so Restart Shell has a tab to open before.
    let before = tab_ids(&app.state()?);
    let (logs, _) = new_tab(&app, &before, || {
        app.dispatch("detail::OpenLogs", Value::Null)?;
        Ok(())
    })?;
    app.dispatch("dock::ActivateTab", json!({"tab": shell}))?;
    app.wait_for("/dock/active", Equals(shell.into()))?;
    app.wait_for("/focus", Equals("shell".into()))?;

    // `exit 3`: the tab says so, keeps the screen, and typing reaches nothing.
    run(&app, "echo bye")?;
    wait_for_row(&app, i, "bye")?;
    run(&app, "exit 3")?;
    let state = app.wait_for(&at(i, "status/state"), Equals("exited".into()))?;
    let tab = tab_at(&state, i);
    assert_eq!(tab["status"], json!({"state": "exited", "code": 3}));
    assert_eq!(tab["status_text"], "Exited (code 3)");
    assert_eq!(tab["shell"]["live"], false);
    assert_eq!(tab["shell"]["exit_code"], 3);
    let rows = screen(&state, i);
    for kept in ["130", "bye", &format!("{prompt} exit 3"), "exit"] {
        assert!(rows.iter().any(|r| r == kept), "{kept} kept: {rows:?}");
    }
    assert_eq!(
        until("the exec to end", || exec.end(), Option::is_some)?,
        Some(ExecEnd::Exited(3))
    );
    until("no open exec", || fake.open_exec_sessions(), |n| *n == 0)?;
    app.type_text("echo late")?;
    app.screenshot_themes("up41-shell-exited")?;

    // Restart Shell: a new shell in a tab right after the ended one, which stays.
    let state = app.state()?;
    let before = tab_ids(&state);
    assert_eq!(before, [shell, logs]);
    let (restarted, j) = new_tab(&app, &before, || {
        app.dispatch("dock::RestartShell", Value::Null)?;
        Ok(())
    })?;
    assert_eq!(j, i + 1, "beside the ended tab");
    let state = app.wait_for(&at(j, "status/state"), Equals("running".into()))?;
    assert_eq!(tab_ids(&state), [shell, restarted, logs]);
    assert_eq!(
        tab_at(&state, j)["cluster"]["chip"],
        cluster.context.as_str()
    );
    assert_eq!(
        tab_at(&state, i)["status"],
        json!({"state": "exited", "code": 3})
    );
    let second = exec_number(fake, 2)?;
    assert_eq!(second.command(), ["bash"]);
    // The ended tab keeps its output beside the new one.
    let rows = screen(&state, i);
    for kept in ["130", "bye", &format!("{prompt} exit 3"), "exit"] {
        assert!(rows.iter().any(|r| r == kept), "{kept} kept: {rows:?}");
    }
    assert!(!rows.iter().any(|r| r.contains("late")), "{rows:?}");
    // The text typed into the ended tab went nowhere: the app handled the keys before the
    // restart (commands are handled in order). The ended session was already closed, so the
    // check that can fail is that the restarted shell never got them either.
    wait_for_row(&app, j, &prompt)?;
    assert!(
        !String::from_utf8_lossy(&second.stdin()).contains("late"),
        "{:?}",
        String::from_utf8_lossy(&second.stdin())
    );
    assert!(!String::from_utf8_lossy(&exec.stdin()).contains("late"));

    // Closing the tabs ends their sessions.
    for id in [restarted, shell, logs] {
        app.dispatch("dock::CloseTab", json!({"tab": id}))?;
    }
    app.wait_for("/dock/tabs", Equals(json!([])))?;
    until("no open exec", || fake.open_exec_sessions(), |n| *n == 0)?;
    until(
        "no open log stream",
        || fake.open_log_streams(),
        |n| *n == 0,
    )?;
    assert_eq!(fake.exec_requests().len(), 2, "one request per shell");
    Ok(())
}

// covers UP-41
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_41_sh_fallback_container_picker_and_paste() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    // Three containers: `app` (the default, with bash), `sidecar` (busybox, only sh) and
    // `warmup`, which has not started.
    fake.add_pod(
        FakePod::new("demo", MULTI, &["app", "sidecar", "warmup"])
            .waiting("warmup", "ContainerCreating"),
    );
    fake.set_exec_image("demo", MULTI, Some("app"), ExecImage::with_bash());
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    open_live(&app, "pods", 18)?;

    // Shift-Cmd-E on podinfo, which has no bash: bash fails to start, then sh runs.
    app.select_row(Some("demo"), PODINFO)?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    let (sh, i) = new_tab(&app, &[], || {
        app.press_key("secondary-shift-e")?;
        Ok(())
    })?;
    // The tab reads "running bash" from the moment bash's exec connects until the container
    // says bash is missing; wait for sh rather than for the first "running".
    let state = app.wait_for(
        &at(i, "status"),
        Equals(json!({"state": "running", "command": "/bin/sh"})),
    )?;
    assert_eq!(
        tab_at(&state, i)["title"],
        format!("Shell: {PODINFO} · podinfo")
    );
    wait_for_row(&app, i, "~ $")?;
    assert_eq!(exec_commands(fake), [["bash"], ["/bin/sh"]]);
    let sessions = fake.exec_sessions();
    assert!(
        matches!(sessions[0].end(), Some(ExecEnd::Failed(_))),
        "{:?}",
        sessions[0].end()
    );
    let exec = sessions[1].clone();
    assert_eq!(exec.container(), "podinfo");
    run(&app, "echo from-sh")?;
    wait_for_row(&app, i, "from-sh")?;
    // busybox asks where the cursor is with every prompt, so stdin holds the answers too.
    wait_for_stdin(&exec, b"echo from-sh\r")?;
    app.screenshot_themes("up41-shell-sh")?;

    // sh did not ask for bracketed paste: two lines would each run as they arrive, so the app
    // asks first, then sends them as typed lines (`\r`).
    let clipboard = Clipboard::save()?;
    copy_two_lines(&app, i)?;
    app.dispatch("terminal::PasteText", Value::Null)?;
    let state = app.wait_for("/dialog/title", Equals("Paste 2 lines?".into()))?;
    assert!(
        state["dialog"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("did not ask for bracketed paste")),
        "{}",
        state["dialog"]
    );
    assert_eq!(state["dialog"]["buttons"], json!(["Paste", "Cancel"]));
    assert!(
        !String::from_utf8_lossy(&exec.stdin()).contains("echo one\r"),
        "nothing sent before the answer"
    );
    app.screenshot_themes("up41-shell-paste-question")?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    wait_for_stdin(&exec, b"echo one\recho two")?;
    assert!(!String::from_utf8_lossy(&exec.stdin()).contains("\x1b[200~"));
    wait_for_row(&app, i, "one")?;
    // Focus goes back to the shell once the question has closed.
    app.wait_for("/focus", Equals("shell".into()))?;
    app.press_key("enter")?;
    wait_for_row(&app, i, "two")?;
    drop(clipboard);

    // A pod with several containers: the row menu's Shell picks one, the default marked and
    // one not running disabled.
    let state = app.state()?;
    app.right_click(&row_id(&state, MULTI)?)?;
    app.wait_for("/detail/name", Equals(MULTI.into()))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    let state = app.wait_for(
        "/menu/entries",
        Any(json!({"label": "Shell", "enabled": true})),
    )?;
    let entry = state["menu"]["entries"]
        .as_array()
        .and_then(|e| e.iter().find(|e| e["label"] == "Shell"))
        .context("Shell")?;
    assert_eq!(
        entry["submenu"],
        json!([
            {"label": "app (default)", "enabled": true},
            {"label": "sidecar", "enabled": true},
            {"label": "warmup (not running)", "enabled": false},
        ])
    );
    app.screenshot("up41-shell-container-picker")?;
    let before = tab_ids(&app.state()?);
    // A submenu entry performs `detail::OpenContainerShell` for its container. Down, Right and
    // Return did not reach the submenu over the automation channel, so the test performs the
    // entry's action itself.
    app.press_key("escape")?;
    app.wait_for("/menu", Equals(Value::Null))?;
    let (sidecar, j) = new_tab(&app, &before, || {
        app.dispatch(
            "detail::OpenContainerShell",
            json!({"container": "sidecar"}),
        )?;
        Ok(())
    })?;
    // As above: bash runs briefly before the container says it is missing.
    let state = app.wait_for(
        &at(j, "status"),
        Equals(json!({"state": "running", "command": "/bin/sh"})),
    )?;
    assert_eq!(
        tab_at(&state, j)["title"],
        format!("Shell: {MULTI} · sidecar")
    );
    wait_for_row(&app, j, "/ #")?;
    let requests = fake.exec_requests();
    let picked: Vec<(Option<String>, Vec<String>)> = requests[2..]
        .iter()
        .map(|r| (r.parsed_query().container, r.parsed_query().command))
        .collect();
    let sidecar_ = Some("sidecar".to_owned());
    assert_eq!(
        picked,
        [
            (sidecar_.clone(), vec!["bash".to_owned()]),
            (sidecar_, vec!["/bin/sh".to_owned()]),
        ]
    );

    // Shift-Cmd-E opens the default container at once, with no picker.
    let before = tab_ids(&app.state()?);
    let (default, k) = new_tab(&app, &before, || {
        app.press_key("secondary-shift-e")?;
        Ok(())
    })?;
    let state = app.wait_for(&at(k, "status/state"), Equals("running".into()))?;
    assert_eq!(tab_at(&state, k)["title"], format!("Shell: {MULTI} · app"));
    assert_eq!(state["menu"], Value::Null);
    wait_for_row(&app, k, &format!("root@{MULTI}:/#"))?;
    assert_eq!(fake.last_exec().context("an exec")?.container(), "app");

    for id in [sh, sidecar, default] {
        app.dispatch("dock::CloseTab", json!({"tab": id}))?;
    }
    app.wait_for("/dock/tabs", Equals(json!([])))?;
    until("no open exec", || fake.open_exec_sessions(), |n| *n == 0)?;
    Ok(())
}

/// Opens a shell on [`WEB`] (selected) and waits until it runs: its tab id and index.
fn open_web_shell(app: &App) -> Result<(u64, usize)> {
    let before = tab_ids(&app.state()?);
    let (id, i) = new_tab(app, &before, || {
        app.dispatch("detail::OpenShell", Value::Null)?;
        Ok(())
    })?;
    app.wait_for(&at(i, "status/state"), Equals("running".into()))?;
    wait_for_row(app, i, &format!("root@{WEB}:/#"))?;
    Ok((id, i))
}

/// Restarts the active (ended) shell tab at `index`, and waits until the new tab beside it
/// fails: returns its id and the state then. Exactly one more exec request was made.
fn restart_fails(app: &App, fake: &FakeApiServer, index: usize, why: &str) -> Result<(u64, Value)> {
    let requests = fake.exec_requests().len();
    let before = tab_ids(&app.state()?);
    let (id, j) = new_tab(app, &before, || {
        app.dispatch("dock::RestartShell", Value::Null)?;
        Ok(())
    })?;
    assert_eq!(j, index + 1, "beside the ended tab");
    let state = app.wait_for(&at(j, "status/state"), Equals("failed".into()))?;
    let tab = tab_at(&state, j);
    assert_eq!(tab["status"], json!({"state": "failed", "reason": why}));
    assert_eq!(tab["status_text"], "Failed");
    assert_eq!(tab["shell"]["live"], false);
    // Not a missing shell: sh is not tried.
    assert_eq!(fake.exec_requests().len(), requests + 1);
    Ok((id, state))
}

// covers UP-41
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_41_shells_fail_in_plain_words() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    open_live(&app, "pods", 17)?;
    app.select_row(Some("demo"), WEB)?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;

    // The connection breaks: the tab fails, says why, and keeps its screen.
    let (_, i) = open_web_shell(&app)?;
    run(&app, "echo before-the-break")?;
    wait_for_row(&app, i, "before-the-break")?;
    assert_eq!(fake.drop_exec_sessions(), 1);
    let state = app.wait_for(&at(i, "status/state"), Equals("failed".into()))?;
    let tab = tab_at(&state, i);
    assert_eq!(
        tab["status"],
        json!({"state": "failed", "reason": "The connection to the shell broke."})
    );
    assert_eq!(tab["status_text"], "Failed");
    assert!(screen(&state, i).iter().any(|r| r == "before-the-break"));
    app.screenshot_themes("up41-shell-connection-broke")?;

    let pods_exec = || {
        Fault::forbidden()
            .on(&Gvr::core("pods"))
            .subresource("exec")
    };
    let upgrade = "The connection to the cluster can't carry a shell (a proxy may be blocking \
                   WebSocket upgrades).";
    let permission = "You don't have permission to open a shell in this pod.";

    // A proxy answers the upgrade with a plain 200.
    fake.inject(Fault::refuse_upgrade().times(1));
    let (_, state) = restart_fails(&app, fake, i, upgrade)?;
    app.screenshot_themes("up41-shell-upgrade-refused")?;
    let i = tab_index(&state, state["dock"]["active"].as_u64().context("active")?)?;

    // A proxy strips the upgrade's headers: the API server's 400.
    fake.inject(Fault::strip_upgrade().times(1));
    let (_, state) = restart_fails(&app, fake, i, upgrade)?;
    let i = tab_index(&state, state["dock"]["active"].as_u64().context("active")?)?;

    // RBAC refuses `get` on pods/exec, then `create` (the WebSocket upgrade check).
    fake.inject(pods_exec().times(1));
    let (_, state) = restart_fails(&app, fake, i, permission)?;
    app.screenshot_themes("up41-shell-forbidden")?;
    let i = tab_index(&state, state["dock"]["active"].as_u64().context("active")?)?;
    fake.inject(
        Fault::forbidden_as("create")
            .on(&Gvr::core("pods"))
            .subresource("exec")
            .times(1),
    );
    let (_, state) = restart_fails(&app, fake, i, permission)?;
    let i = tab_index(&state, state["dock"]["active"].as_u64().context("active")?)?;

    // With the faults used up, Restart Shell gets a shell again.
    let before = tab_ids(&state);
    let (_, j) = new_tab(&app, &before, || {
        app.dispatch("dock::RestartShell", Value::Null)?;
        Ok(())
    })?;
    assert_eq!(j, i + 1);
    app.wait_for(&at(j, "status/state"), Equals("running".into()))?;
    wait_for_row(&app, j, &format!("root@{WEB}:/#"))?;
    assert_eq!(fake.open_exec_sessions(), 1);
    let state = app.state()?;
    assert_eq!(
        tab_ids(&state).len(),
        6,
        "every failed tab stays until closed"
    );
    for id in tab_ids(&state) {
        app.dispatch("dock::CloseTab", json!({"tab": id}))?;
    }
    app.wait_for("/dock/tabs", Equals(json!([])))?;
    until("no open exec", || fake.open_exec_sessions(), |n| *n == 0)?;
    Ok(())
}

/// Waits until the log tab at `index` shows a line reading `text`.
fn wait_for_line(app: &App, index: usize, text: &str) -> Result<Value> {
    Ok(app.wait_for(&at(index, "logs/last_lines"), Any(json!({ "text": text })))?)
}

// covers UP-42, UP-24
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_42_sessions_outlive_navigation() -> Result<()> {
    let two = TwoClusters::start()?;
    let (a, b) = (&two.a, &two.b);
    let web = ContainerLog::new("demo", WEB);
    let mut app = App::launch(AppOptions::new(two.kubeconfig()))?;
    connect(&app, TwoClusters::A)?;
    open_live(&app, "pods", 17)?;
    app.select_row(Some("demo"), WEB)?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;

    // A log tab and a shell tab on fake-a.
    let (logs, l) = new_tab(&app, &[], || {
        app.dispatch("detail::OpenLogs", Value::Null)?;
        Ok(())
    })?;
    app.wait_for(&at(l, "status/state"), Equals("streaming".into()))?;
    until("fake-a's log stream", || a.open_log_streams(), |n| *n == 1)?;
    let (shell, s) = open_web_shell(&app)?;
    let exec = a.last_exec().context("fake-a's exec")?;
    let state = app.state()?;
    for i in [l, s] {
        assert_eq!(tab_at(&state, i)["cluster"]["context"], TwoClusters::A);
        assert_eq!(tab_at(&state, i)["cluster"]["chip"], TwoClusters::A);
    }

    // Another view on the same cluster: both keep running, and their output still arrives.
    app.open_view("deployments.apps")?;
    app.wait_for("/list/kind", Equals("deployments.apps".into()))?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    assert_eq!(a.append_log(&web, "after the view change\n"), 1);
    wait_for_line(&app, l, "after the view change")?;
    assert!(exec.print("\r\nprinted after the view change\r\n"));
    wait_for_row(&app, s, "printed after the view change")?;

    // Toggle Dock (the action Cmd-J performs) hides the dock and shows it again, with its tabs
    // and sessions intact.
    app.dispatch("dock::ToggleDock", Value::Null)?;
    let state = app.wait_for("/dock/visible", Equals(false.into()))?;
    assert_eq!(tab_ids(&state), [logs, shell]);
    assert_eq!((a.open_log_streams(), a.open_exec_sessions()), (1, 1));
    app.dispatch("dock::ToggleDock", Value::Null)?;
    let state = app.wait_for("/dock/visible", Equals(true.into()))?;
    assert_eq!(tab_ids(&state), [logs, shell]);

    // Over to fake-b, with a log tab of its own: fake-a's sessions keep running there.
    connect(&app, TwoClusters::B)?;
    open_live(&app, "pods", 17)?;
    app.select_row(Some("demo"), WEB)?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    let (b_logs, bl) = new_tab(&app, &[logs, shell], || {
        app.dispatch("detail::OpenLogs", Value::Null)?;
        Ok(())
    })?;
    let state = app.wait_for(&at(bl, "status/state"), Equals("streaming".into()))?;
    assert_eq!(tab_at(&state, bl)["cluster"]["context"], TwoClusters::B);
    assert_eq!(tab_at(&state, bl)["cluster"]["chip"], TwoClusters::B);
    for i in [l, s] {
        assert_eq!(tab_at(&state, i)["cluster"]["chip"], TwoClusters::A);
    }
    until("fake-b's log stream", || b.open_log_streams(), |n| *n == 1)?;
    assert_eq!(a.append_log(&web, "after the cluster switch\n"), 1);
    wait_for_line(&app, l, "after the cluster switch")?;
    assert!(exec.print("\r\nprinted after the cluster switch\r\n"));
    let state = wait_for_row(&app, s, "printed after the cluster switch")?;
    assert_eq!(tab_at(&state, l)["status"]["state"], "streaming");
    assert_eq!(tab_at(&state, s)["status"]["state"], "running");
    // Output kept arriving on the streams opened first: no new log request, no new exec.
    assert_eq!(a.log_requests().len(), 1);
    assert_eq!(a.exec_requests().len(), 1);
    assert_eq!(b.exec_requests().len(), 0);
    app.screenshot_themes("up42-sessions-across-clusters")?;

    // The dock's height is kept.
    app.dispatch("workspace::SetDockHeight", json!({"height": 333.0}))?;
    app.wait_for("/dock/height", Equals(333.into()))?;

    // Closing the tabs ends their sessions: Cmd-W with the dock focused, Cmd-W with focus in the
    // list (it closes the active tab there too, not the window: #177), and Close Dock Tab.
    app.dispatch("dock::ActivateTab", json!({"tab": shell}))?;
    app.wait_for("/focus", Equals("shell".into()))?;
    app.press_key("secondary-w")?;
    let state = app.wait_for("/dock/tabs", Test::at_most(2))?;
    assert_eq!(tab_ids(&state), [logs, b_logs]);
    until(
        "fake-a's exec to close",
        || a.open_exec_sessions(),
        |n| *n == 0,
    )?;
    assert_eq!(a.open_log_streams(), 1, "the log tab still streams");
    app.dispatch("dock::ActivateTab", json!({"tab": b_logs}))?;
    app.wait_for("/dock/active", Equals(b_logs.into()))?;
    app.dispatch("resource_list::FocusFilter", Value::Null)?;
    app.wait_for("/focus", Equals("list_filter".into()))?;
    app.press_key("secondary-w")?;
    let state = app.wait_for("/dock/tabs", Test::at_most(1))?;
    assert_eq!(
        tab_ids(&state),
        [logs],
        "the window stays, with the other tab"
    );
    until(
        "fake-b's log stream to close",
        || b.open_log_streams(),
        |n| *n == 0,
    )?;
    app.wait_for("/dock/active", Equals(logs.into()))?;
    app.dispatch("dock::CloseActiveTab", Value::Null)?;
    let state = app.wait_for("/dock/tabs", Equals(json!([])))?;
    assert_eq!(state["dock"]["visible"], false);
    until(
        "fake-a's log stream to close",
        || a.open_log_streams(),
        |n| *n == 0,
    )?;
    assert_eq!((a.log_requests().len(), b.log_requests().len()), (1, 1));

    // The height survives a relaunch on the same config directory.
    // Quit checks that the app exited cleanly (`App::quit`).
    let scratch = app.quit()?;
    app = App::launch(AppOptions::new(two.kubeconfig()).reuse(scratch))?;
    let state = app.wait_for("/dock/height", Equals(333.into()))?;
    assert_eq!(state["dock"]["tabs"], json!([]));
    assert_eq!(state["dock"]["visible"], false);

    Ok(())
}

/// A labelled namespace with two pods on the kind dev cluster, deleted when dropped: `shell-bash`
/// (nginx, which has bash) and `shell-sh` (busybox, only sh).
struct KindPods {
    runtime: tokio::runtime::Runtime,
    client: kube::Client,
    namespace: String,
}

/// The label on the kind test's namespaces, so a run that was killed before deleting its
/// namespace leaves nothing behind for long: the next run deletes it.
const KIND_LABEL: (&str, &str) = ("flint-e2e", "shell");

impl KindPods {
    fn create(kubeconfig: &std::path::Path) -> Result<KindPods> {
        use k8s_openapi::api::core::v1::{Namespace, Pod};
        use kube::api::{Api, DeleteParams, ListParams, PostParams};
        use kube::runtime::wait::{await_condition, conditions::is_pod_running};

        let runtime = tokio::runtime::Runtime::new()?;
        let namespace = format!("flint-e2e-shell-{}", std::process::id());
        let client = runtime.block_on(async {
            let config = kube::Config::from_custom_kubeconfig(
                kube::config::Kubeconfig::read_from(kubeconfig)?,
                &kube::config::KubeConfigOptions::default(),
            )
            .await?;
            anyhow::Ok(kube::Client::try_from(config)?)
        })?;
        let namespaces: Api<Namespace> = Api::all(client.clone());
        let label = format!("{}={}", KIND_LABEL.0, KIND_LABEL.1);
        runtime.block_on(async {
            // Leftovers of runs that were killed.
            for stale in namespaces
                .list(&ListParams::default().labels(&label))
                .await?
            {
                let Some(name) = stale.metadata.name else {
                    continue;
                };
                eprintln!("deleting namespace {name}, left by an earlier run");
                namespaces.delete(&name, &DeleteParams::default()).await?;
            }
            let ns = serde_json::from_value(json!({"metadata": {
                "name": namespace,
                "labels": {KIND_LABEL.0: KIND_LABEL.1},
            }}))?;
            namespaces.create(&PostParams::default(), &ns).await?;
            anyhow::Ok(())
        })?;
        // The namespace exists: from here on, dropping the guard deletes it.
        let pods = KindPods {
            runtime,
            client: client.clone(),
            namespace: namespace.clone(),
        };
        pods.runtime.block_on(async {
            let api: Api<Pod> = Api::namespaced(client, &namespace);
            for (name, image, command) in [
                ("shell-bash", "nginx:1.27", None),
                ("shell-sh", "busybox:1.37.0", Some(["sleep", "3600"])),
            ] {
                let mut container = json!({
                    "name": "main",
                    "image": image,
                    "imagePullPolicy": "IfNotPresent",
                });
                if let Some(command) = command {
                    container["command"] = json!(command);
                }
                let pod = serde_json::from_value(json!({
                    "metadata": {"name": name},
                    "spec": {"containers": [container], "terminationGracePeriodSeconds": 0},
                }))?;
                api.create(&PostParams::default(), &pod).await?;
            }
            for name in ["shell-bash", "shell-sh"] {
                tokio::time::timeout(
                    Duration::from_secs(120),
                    await_condition(api.clone(), name, is_pod_running()),
                )
                .await
                .with_context(|| format!("{name} did not start"))??;
            }
            anyhow::Ok(())
        })?;
        Ok(pods)
    }
}

impl Drop for KindPods {
    fn drop(&mut self) {
        use k8s_openapi::api::core::v1::Namespace;
        use kube::api::{Api, DeleteParams};

        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        if let Err(e) = self
            .runtime
            .block_on(namespaces.delete(&self.namespace, &DeleteParams::default()))
        {
            eprintln!("could not delete namespace {}: {e}", self.namespace);
        }
    }
}

// covers UP-41
#[test]
#[ignore = "app e2e on kind: needs macOS with a GUI session and FLINT_TEST_KUBECONFIG"]
fn up_41_shell_on_kind() -> Result<()> {
    let Some(kubeconfig) = std::env::var_os("FLINT_TEST_KUBECONFIG") else {
        eprintln!("skipped: FLINT_TEST_KUBECONFIG is not set (see docs/TESTING.md)");
        return Ok(());
    };
    // Refuses a context that is not a local cluster, and gives the app only that context.
    let cluster = TestCluster::kind(std::path::Path::new(&kubeconfig))?;
    let pods = KindPods::create(&cluster.kubeconfig)?;
    let ns = pods.namespace.as_str();
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    app.open_view("pods")?;
    app.wait_for("/list/sync", Equals("live".into()))?;
    app.wait_for(
        "/list/rows",
        Any(json!({"namespace": ns, "name": "shell-sh"})),
    )?;

    // bash: a prompt, typing echoed, the size, and an exit code.
    app.select_row(Some(ns), "shell-bash")?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    let (bash, i) = new_tab(&app, &[], || {
        app.dispatch("detail::OpenShell", Value::Null)?;
        Ok(())
    })?;
    let state = app.wait_for(&at(i, "status/state"), Equals("running".into()))?;
    assert_eq!(tab_at(&state, i)["status"]["command"], "bash");
    assert_eq!(
        tab_at(&state, i)["cluster"]["context"],
        cluster.context.as_str()
    );
    app.wait_for(&at(i, "shell/cursor/col"), Test::at_least(1))?;
    run(&app, "echo hi-from-kind")?;
    let state = wait_for_row(&app, i, "hi-from-kind")?;
    let size = size(&state, i);
    // kube sends the first resize and stdin on separate channels, and kind has no fake to say
    // when the resize arrived: ask again until the TTY has the tab's size.
    let expected = format!("{} {}", size.height, size.width);
    let mut sized = false;
    for _ in 0..5 {
        run(&app, "stty size")?;
        if wait_for_row_within(&app, i, &expected, Duration::from_secs(2)).is_ok() {
            sized = true;
            break;
        }
    }
    if !sized {
        bail!(
            "stty size never said {expected}: {:?}",
            screen(&app.state()?, i)
        );
    }
    app.screenshot_themes("up41-shell-kind")?;
    run(&app, "exit 3")?;
    let state = app.wait_for(&at(i, "status/state"), Equals("exited".into()))?;
    assert_eq!(
        tab_at(&state, i)["status"],
        json!({"state": "exited", "code": 3})
    );
    assert!(screen(&state, i).iter().any(|r| r == "hi-from-kind"));

    // busybox has no bash: sh runs instead.
    app.select_row(Some(ns), "shell-sh")?;
    app.wait_for("/detail/name", Equals("shell-sh".into()))?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    let (sh, j) = new_tab(&app, &[bash], || {
        app.dispatch("detail::OpenShell", Value::Null)?;
        Ok(())
    })?;
    // On a real cluster the tab may say `running bash` until bash's launch error arrives.
    app.wait_for(&at(j, "status/command"), Equals("/bin/sh".into()))?;
    run(&app, "echo from-sh")?;
    wait_for_row(&app, j, "from-sh")?;

    for id in [bash, sh] {
        app.dispatch("dock::CloseTab", json!({"tab": id}))?;
    }
    app.wait_for("/dock/tabs", Equals(json!([])))?;
    drop(app);
    drop(pods);
    Ok(())
}
