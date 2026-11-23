//! UP-42: Close Window from the list closes the real window.
//!
//! Close Window acts on the active window, and the app under test can start in the background
//! with none. This test brings the app to the front through AppKit, then sends the action.
//! Closing the last window is what ends the app, so the process ending with stdin still open, and
//! without Quit ever being sent, shows that the window closed.

use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use flint_e2e::app::{App, AppOptions, Test::Equals};
use serde_json::Value;

use crate::support::*;

/// How long the app has to come to the front and close its window.
const CLOSE: Duration = Duration::from_secs(30);
/// How long one Close Window request may take to answer.
const ANSWER: Duration = Duration::from_secs(2);

/// Asks AppKit to activate `pid`. Uses no Apple Events, so it needs no automation permission.
const ACTIVATE: &str = r#"
ObjC.import("AppKit");
function run(argv) {
  const app = $.NSRunningApplication.runningApplicationWithProcessIdentifier(Number(argv[0]));
  if (!app.isNil()) app.activateWithOptions($.NSApplicationActivateIgnoringOtherApps);
}
"#;

fn activate(pid: u32) -> Result<()> {
    let output = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", ACTIVATE, &pid.to_string()])
        .output()
        .context("running osascript")?;
    if !output.status.success() {
        bail!(
            "activating the app failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// The exit code of child `pid` if it has ended. Leaves it unreaped, for `App::quit` to collect
/// and check.
fn exit_code(pid: u32) -> Result<Option<i32>> {
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

/// Activates the app and sends Close Window until the process ends, with stdin still open.
/// Activation is asynchronous and the action does nothing until a window is active, so each round
/// asks again; the osascript launch paces the rounds. Returns the exit code.
fn close_window_and_wait(app: &App) -> Result<i32> {
    let deadline = Instant::now() + CLOSE;
    let mut answer = None;
    loop {
        if let Some(code) = exit_code(app.pid())? {
            return Ok(code);
        }
        if Instant::now() > deadline {
            app.mark_failing();
            bail!(
                "the app was still running {CLOSE:?} after Close Window, with stdin open (last \
                 answer {answer:?}). App log:\n{}",
                app.log()
            );
        }
        activate(app.pid())?;
        // The app may end before it answers, so an error here is not a failure by itself.
        answer = Some(app.dispatch_within("flint::CloseWindow", Value::Null, ANSWER));
    }
}

#[test]
#[ignore = "opens a window: needs a macOS GUI session"]
fn up_42_close_window_from_the_list_closes_the_window() -> Result<()> {
    let two = TwoClusters::start()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    connect(&app, TwoClusters::A)?;
    open_live(&app, "pods", 1)?;
    app.wait_for("/focus", Equals("list".into()))?;
    assert_eq!(exit_code(app.pid())?, None, "the app is running");
    let socket = app.config_dir().join("automation.sock");
    assert!(socket.exists(), "the channel is open");

    assert_eq!(
        close_window_and_wait(&app)?,
        0,
        "closing the last window exits cleanly"
    );
    assert!(!socket.exists(), "the quit hooks ran");

    // The teardown checks, on the exit already observed.
    app.quit()?;
    Ok(())
}
