//! The test-only automation channel (docs/TESTING.md, "Automation channel"): a Unix socket that
//! lets e2e tests and agents drive the running app. Compiled only with `--features automation`,
//! and only into debug builds; release builds contain none of it.
//!
//! - [`setup`]: the checks before the app starts (a private scratch `FLINT_CONFIG_DIR` that is
//!   not the real one, an explicit `KUBECONFIG`, a socket path macOS can bind).
//! - [`socket`]: the socket at `FLINT_CONFIG_DIR/automation.sock` (mode 0600).
//! - [`protocol`]: newline-delimited JSON requests and responses.
//! - [`commands`]: runs each command through registered actions and keystrokes, never the core.
//! - [`predicate`] and [`keys`]: `wait_for` tests and `type_text` keystrokes.
//!
//! If the channel cannot start, the app exits with [`EXIT_CODE`], so a test harness fails fast
//! instead of waiting for a socket that never comes.

#[cfg(all(feature = "automation", not(debug_assertions)))]
compile_error!(
    "the automation channel is for test builds only: build it without --release, or drop \
     --features automation"
);

mod commands;
mod keys;
mod predicate;
mod protocol;
mod setup;
mod socket;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use gpui_kit::*;

use super::workspace::Workspace;
use commands::Target;
pub use setup::Places;

/// The exit code when the channel cannot start.
pub const EXIT_CODE: i32 = 3;

/// The most requests in flight at once; more get a `failed` response at once.
const MAX_IN_FLIGHT: usize = 16;

/// Checks the environment before anything starts; exits the app with a reason if the channel
/// could not run safely.
pub fn prepare() -> Places {
    match setup::prepare(&setup::Env::from_process()) {
        Ok(places) => places,
        Err(e) => fail(&e),
    }
}

fn fail(reason: &str) -> ! {
    tracing::error!("automation channel cannot start: {reason}");
    eprintln!("flint: automation channel cannot start: {reason}");
    std::process::exit(EXIT_CODE);
}

/// Starts the channel for `window`, at `places`. Exits the app if the socket cannot be bound.
pub fn start(places: Places, window: AnyWindowHandle, workspace: Entity<Workspace>, cx: &mut App) {
    let (listener, socket_file) = match socket::bind(&places.socket) {
        Ok(bound) => bound,
        Err(e) => fail(&format!("{}: {e}", places.socket.display())),
    };

    // Socket threads hand each line to the UI thread and wait for its answer.
    let (requests, incoming) = async_channel::unbounded::<(String, mpsc::SyncSender<String>)>();
    let in_flight = Arc::new(AtomicUsize::new(0));
    socket::serve(listener, move |line| {
        let failed = |message: &str| {
            protocol::response(
                protocol::request_id(line),
                Err(protocol::Error::new(protocol::ErrorCode::Failed, message)),
            )
        };
        if in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
            in_flight.fetch_sub(1, Ordering::SeqCst);
            return failed("too many requests in flight; wait for some to finish");
        }
        let (reply, answer) = mpsc::sync_channel(1);
        let response = if requests.send_blocking((line.to_owned(), reply)).is_err() {
            failed("the app is shutting down")
        } else {
            answer
                .recv()
                .unwrap_or_else(|_| failed("the app is shutting down"))
        };
        in_flight.fetch_sub(1, Ordering::SeqCst);
        response
    });

    let target = Target {
        window,
        workspace,
        screenshots: places.screenshots.clone(),
    };
    cx.spawn(async move |cx| {
        while let Ok((line, reply)) = incoming.recv().await {
            let target = target.clone();
            // Each request runs on its own, so a long `wait_for` does not hold up others.
            cx.spawn(async move |cx| {
                let response = commands::handle_line(&line, &target, cx).await;
                let _ = reply.send(response);
            })
            .detach();
        }
    })
    .detach();

    cx.on_app_quit(move |_| {
        socket_file.remove();
        async {}
    })
    .detach();
    tracing::info!(path = %places.socket.display(), "automation channel listening");
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use super::{Places, start};
    use crate::ui::support_tests::open_workspace;

    // The socket is there while the app runs, and gone when it quits.
    #[gpui_kit::test]
    async fn the_socket_goes_when_the_app_quits(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let places = Places {
            config_dir: dir.path().to_owned(),
            socket: dir.path().join("automation.sock"),
            screenshots: dir.path().to_owned(),
        };
        let socket = places.socket.clone();
        let (window, workspace) = (t.window, t.workspace.clone());
        cx.update(|cx| start(places, window, workspace, cx));
        assert!(socket.exists());
        cx.update(|cx| cx.shutdown());
        cx.run_until_parked();
        assert!(!socket.exists());
    }
}
