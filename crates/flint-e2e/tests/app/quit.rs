//! UP-42: the app's exit, observed on its own rather than through `App::quit`.
//!
//! `App::quit` sends Quit and later closes stdin, which also ends the app, so its success does
//! not show which of the two did it. These tests send Quit themselves and watch the process end
//! while the harness still holds stdin open.
//!
//! Not here: **Close Window** closes `cx.active_window()`, and the app under test runs in the
//! background with no active window, so the action does nothing (#263).

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use flint_e2e::app::{App, AppOptions};
use serde_json::{Value, json};

use crate::support::*;

/// How long Quit may take. The harness's own fallback never runs in this time: it starts only
/// when the test calls `App::quit` or drops the app.
const EXIT: Duration = Duration::from_secs(5);
/// The app saves pane sizes this long after the last change (`PANE_SAVE_DELAY`). An exit sooner
/// than this after the change can only have saved it from the quit hook.
const SAVE_DELAY: Duration = Duration::from_millis(400);
/// Launches to try for an exit inside [`SAVE_DELAY`], on a machine busy enough to miss it.
const ATTEMPTS: u32 = 8;

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

/// Sends Quit, which may end the app before it answers, and waits for the process to end with
/// stdin still open. Returns its exit code.
fn quit_and_wait(app: &App) -> Result<i32> {
    let sent = app.dispatch("flint::Quit", Value::Null);
    let deadline = Instant::now() + EXIT;
    loop {
        if let Some(code) = exit_code(app.pid())? {
            return Ok(code);
        }
        if Instant::now() > deadline {
            app.mark_failing();
            bail!(
                "the app was still running {EXIT:?} after Quit, with stdin open (Quit answered \
                 {sent:?}). App log:\n{}",
                app.log()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "opens a window: needs a macOS GUI session"]
fn up_42_quit_ends_the_app_with_stdin_open() -> Result<()> {
    let two = TwoClusters::start()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    connect(&app, TwoClusters::A)?;
    assert_eq!(exit_code(app.pid())?, None, "the app is running");
    let socket = app.config_dir().join("automation.sock");
    assert!(socket.exists(), "the channel is open");

    assert_eq!(quit_and_wait(&app)?, 0, "Quit exits cleanly");
    assert!(!socket.exists(), "the quit hooks ran");

    // The teardown checks, on the exit already observed.
    app.quit()?;
    Ok(())
}

#[test]
#[ignore = "opens a window: needs a macOS GUI session"]
fn up_42_quitting_saves_a_change_still_waiting_to_be_saved() -> Result<()> {
    let two = TwoClusters::start()?;
    let mut slowest = Duration::ZERO;
    for attempt in 0..ATTEMPTS {
        let height = 333 + attempt;
        let app = App::launch(AppOptions::new(two.kubeconfig()))?;
        let change = Instant::now();
        app.dispatch("workspace::SetDockHeight", json!({ "height": height }))?;
        let code = quit_and_wait(&app)?;
        let took = change.elapsed();
        assert_eq!(code, 0, "Quit exits cleanly");
        let scratch = app.quit()?;
        if took >= SAVE_DELAY {
            // The delayed save may have run, so this launch proves nothing.
            slowest = slowest.max(took);
            continue;
        }

        let app = App::launch(AppOptions::new(two.kubeconfig()).reuse(scratch))?;
        app.wait_for("/dock/height", Equals(height.into()))?;
        return Ok(());
    }
    bail!(
        "no exit within {SAVE_DELAY:?} of the change in {ATTEMPTS} launches (slowest {slowest:?})"
    )
}
