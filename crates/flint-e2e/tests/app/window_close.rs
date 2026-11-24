//! UP-42: Close Window from the list closes the real window.
//!
//! Close Window acts on the active window, and the app under test can start in the background
//! with none. This test asks the app to activate its window, waits until GPUI reports it active,
//! then sends the action once. Closing the last window is what ends the app; the app's lifecycle
//! record shows the window closed before the quit hooks ran, so a direct quit does not pass.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use flint_e2e::app::{App, AppOptions, Test::Equals};
use serde_json::Value;

use crate::support::*;

/// How long the app has to come to the front and close its window.
const CLOSE: Duration = Duration::from_secs(30);
/// How long one request may take to answer.
const ANSWER: Duration = Duration::from_secs(2);
/// The pause between looks at the app.
const POLL: Duration = Duration::from_millis(50);
/// What the app records while it closes the window and exits, in order.
const CLOSED_THEN_QUIT: &str = "window-closed\nquit-hooks\n";

/// The time left before `deadline`, capped at one answer. Fails once none is left, naming `doing`.
fn budget(app: &App, deadline: Instant, doing: &str) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        app.mark_failing();
        bail!(
            "still {doing} {CLOSE:?} after starting to close the window. App log:\n{}",
            app.log()
        );
    }
    Ok(left.min(ANSWER))
}

/// Brings the window to the front from inside the app, until GPUI reports it active: Close Window
/// acts on the active window, and does nothing without one.
fn activate(app: &App, deadline: Instant) -> Result<()> {
    let mut last = Value::Null;
    loop {
        let wait = budget(
            app,
            deadline,
            &format!("waiting for the window to become active (the app last said {last})"),
        )?;
        if let Ok(answer) = app.request_by("activate_window", Value::Null, wait) {
            if answer["active"] == true {
                return Ok(());
            }
            last = answer;
        }
        std::thread::sleep(POLL);
    }
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

/// Activates the window, sends Close Window once, and waits for the process to end with stdin
/// still open. Fails on any dialog that opens instead. Returns the exit code.
fn close_window_and_wait(app: &App) -> Result<i32> {
    let deadline = Instant::now() + CLOSE;
    activate(app, deadline)?;
    let wait = budget(app, deadline, "sending Close Window")?;
    // The app may end before it answers, so an error here is not a failure by itself.
    let answer = app.dispatch_within("flint::CloseWindow", Value::Null, wait);
    loop {
        // Before looking at the exit, so a close that came too late does not pass.
        let wait = budget(
            app,
            deadline,
            &format!("waiting for the app to exit (Close Window answered {answer:?})"),
        )?;
        if let Some(code) = exit_code(app.pid())? {
            return Ok(code);
        }
        // The app may end while it answers.
        if let Ok(state) = app.request_by("state", Value::Null, wait)
            && let Some(title) = state["dialog"]["title"].as_str()
        {
            app.mark_failing();
            bail!("Close Window opened the dialog {title:?} instead of closing the window");
        }
        std::thread::sleep(POLL);
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
    let record = std::fs::read_to_string(app.config_dir().join("automation-lifecycle.log"))
        .context("reading the app's lifecycle record")?;
    assert_eq!(
        record, CLOSED_THEN_QUIT,
        "the target window closed before the quit hooks ran"
    );

    // The teardown checks, on the exit already observed.
    app.quit()?;
    Ok(())
}
