//! The test-only automation channel (docs/TESTING.md, "Automation channel"): a Unix socket that
//! lets e2e tests and agents drive the running app. Compiled only with `--features automation`;
//! release builds contain none of it.
//!
//! - [`socket`]: the socket at `FLINT_CONFIG_DIR/automation.sock` (mode 0600), refused without
//!   `FLINT_CONFIG_DIR`.
//! - [`protocol`]: newline-delimited JSON requests and responses.
//! - [`commands`]: runs each command through registered actions and keystrokes, never the core.
//! - [`predicate`] and [`keys`]: `wait_for` tests and `type_text` keystrokes.

mod commands;
mod keys;
mod predicate;
mod protocol;
mod socket;

use std::sync::mpsc;

use gpui_kit::*;

use super::workspace::Workspace;
use commands::Target;

/// Starts the channel for `window`, unless `FLINT_CONFIG_DIR` is unset or the socket cannot be
/// bound; either is logged and the app runs on without it.
pub fn start(window: AnyWindowHandle, workspace: Entity<Workspace>, cx: &mut App) {
    let config_dir = std::env::var_os("FLINT_CONFIG_DIR");
    let path = match socket::socket_path(config_dir) {
        Ok(path) => path,
        Err(e) => {
            tracing::error!("automation channel not started: {e}");
            return;
        }
    };
    let listener = match socket::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(path = %path.display(), "automation channel not started: {e}");
            return;
        }
    };

    // Socket threads hand each line to the UI thread and wait for its answer.
    let (requests, incoming) = async_channel::unbounded::<(String, mpsc::SyncSender<String>)>();
    socket::serve(listener, move |line| {
        let (reply, answer) = mpsc::sync_channel(1);
        if requests.send_blocking((line.to_owned(), reply)).is_err() {
            return protocol::response(
                serde_json::Value::Null,
                Err(protocol::Error::new(
                    protocol::ErrorCode::Failed,
                    "the app is shutting down",
                )),
            );
        }
        answer.recv().unwrap_or_default()
    });

    let target = Target {
        window,
        workspace,
        config_dir: path.parent().map(ToOwned::to_owned).unwrap_or_default(),
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

    let socket_file = path.clone();
    cx.on_app_quit(move |_| {
        let _ = std::fs::remove_file(&socket_file);
        async {}
    })
    .detach();
    tracing::info!(path = %path.display(), "automation channel listening");
}
