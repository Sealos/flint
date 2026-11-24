//! UP-42: the app's exit, observed on its own rather than through `App::quit`.
//!
//! `App::quit` sends Quit and later closes stdin, which also ends the app, so its success does
//! not show which of the two did it. This test sends Quit itself and watches the process end
//! while the harness still holds stdin open.
//!
//! Close Window has its own test, in `window_close.rs`.
//!
//! With a port forward running, Quit asks first (#701): Cancel keeps the app and the forward,
//! confirming quits.
//!
//! Not here (#263): **saving on quit**: a relaunch reads back a value that is also saved shortly
//! after each change, so it does not show that quitting saved it.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::fake_api::FakePod;
use serde_json::{Value, json};

use crate::support::*;

/// How long the app may take to exit once the Quit request has returned. The harness's own
/// fallback never runs in this time: it starts only when the test calls `App::quit` or drops the
/// app.
const EXIT: Duration = Duration::from_secs(5);

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
fn up_42_quit_with_a_forward_running_asks_first() -> Result<()> {
    let two = TwoClusters::start()?;
    two.a.add_pod(FakePod::from_json(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "web", "namespace": "demo", "uid": "uid-web", "resourceVersion": "5001",
            "creationTimestamp": "2026-01-01T00:00:00Z", "labels": {},
        },
        "spec": {"containers": [{"name": "c", "image": "busybox:1.37.0",
                                 "ports": [{"containerPort": 8080}]}]},
        "status": {"phase": "Running"},
    })));
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    connect(&app, TwoClusters::A)?;
    open_live(&app, "pods", 1)?;
    select(&app, Some("demo"), "web")?;
    app.dispatch("detail::ForwardPort", Value::Null)?;
    wait_offered(&app, "port_forward::StartPortForward")?;
    app.dispatch("port_forward::StartPortForward", Value::Null)?;

    let text = open_dialog(&app, "flint::Quit")?;
    assert!(
        text.contains("stops 1 running port forward"),
        "the question names the forward: {text}"
    );
    dismiss(&app, "escape")?;
    assert_eq!(exit_code(app.pid())?, None, "Cancel keeps the app running");

    app.dispatch("flint::Quit", Value::Null)?;
    app.wait_for("/dialog/title", flint_e2e::app::Test::Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null).ok();
    let deadline = Instant::now() + EXIT;
    while exit_code(app.pid())?.is_none() {
        if Instant::now() > deadline {
            app.mark_failing();
            bail!(
                "the app was still running after Quit was confirmed. App log:\n{}",
                app.log()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    app.quit()?;
    Ok(())
}
