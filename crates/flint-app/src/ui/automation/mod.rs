//! The test-only automation channel (docs/TESTING.md, "Automation channel"): a Unix socket that
//! lets e2e tests and agents drive the running app. Compiled only with `--features automation`,
//! and only into debug builds; release builds contain none of it.
//!
//! - [`setup`]: the checks before the app starts (a private scratch `FLINT_CONFIG_DIR` that is
//!   not the real one, an explicit `KUBECONFIG`, a socket path the platform can bind).
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

#[cfg(target_os = "macos")]
mod activation;
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

/// Set to `1` by a test harness that holds the app's stdin open: the app quits when stdin reaches
/// end of file, so it never outlives a harness that was killed.
pub const EXIT_ON_STDIN_EOF_ENV: &str = "FLINT_AUTOMATION_EXIT_ON_STDIN_EOF";

/// How long a quit may take after stdin closes before the process exits anyway.
const QUIT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

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
        workspace: workspace.downgrade(),
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

    let lifecycle = Lifecycle::start(&places.config_dir);
    let closed = lifecycle.clone();
    let target_id = window.window_id();
    cx.on_window_closed(move |_, id| {
        if id == target_id {
            closed.record(Lifecycle::WINDOW_CLOSED);
        }
    })
    .detach();
    cx.on_app_quit(move |_| {
        lifecycle.record(Lifecycle::QUIT_HOOKS);
        socket_file.remove();
        async {}
    })
    .detach();
    if exit_on_stdin_eof(std::env::var_os(EXIT_ON_STDIN_EOF_ENV).as_deref()) {
        quit_on_stdin_eof(cx);
    }
    tracing::info!(path = %places.socket.display(), "automation channel listening");
}

/// A file in the scratch config directory recording, in order, the window closing and the quit
/// hooks running. The socket is gone by the time a test can read what happened during exit.
#[derive(Clone)]
struct Lifecycle(std::path::PathBuf);

impl Lifecycle {
    const FILE: &str = "automation-lifecycle.log";
    const WINDOW_CLOSED: &str = "window-closed";
    const QUIT_HOOKS: &str = "quit-hooks";

    /// Starts an empty record, so a relaunch on the same directory reads only its own events.
    fn start(config_dir: &std::path::Path) -> Self {
        let path = config_dir.join(Self::FILE);
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    fn record(&self, event: &str) {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.0);
        if let Err(e) = file.and_then(|mut file| writeln!(file, "{event}")) {
            tracing::warn!("could not record {event}: {e}");
        }
    }
}

fn exit_on_stdin_eof(flag: Option<&std::ffi::OsStr>) -> bool {
    flag.is_some_and(|v| v == "1")
}

/// Quits the app when stdin reaches end of file (the harness died or closed it), and exits the
/// process if quitting takes longer than [`QUIT_GRACE`].
fn quit_on_stdin_eof(cx: &mut App) {
    let (closed, on_close) = async_channel::bounded::<()>(1);
    let spawned = std::thread::Builder::new()
        .name("automation-stdin".into())
        .spawn(move || {
            use std::io::Read as _;
            let mut stdin = std::io::stdin().lock();
            let mut sink = [0u8; 256];
            loop {
                match stdin.read(&mut sink) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = closed.send_blocking(());
            std::thread::sleep(QUIT_GRACE);
            std::process::exit(0);
        });
    if let Err(e) = spawned {
        fail(&format!("could not watch stdin: {e}"));
    }
    cx.spawn(async move |cx| {
        if on_close.recv().await.is_ok() {
            tracing::info!("stdin closed: quitting");
            cx.update(|cx| cx.quit());
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    #[test]
    fn only_the_harness_flag_quits_on_stdin_eof() {
        use super::exit_on_stdin_eof;
        assert!(exit_on_stdin_eof(Some("1".as_ref())));
        assert!(!exit_on_stdin_eof(Some("0".as_ref())));
        assert!(!exit_on_stdin_eof(Some("".as_ref())));
        assert!(!exit_on_stdin_eof(None));
    }

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

    // The record shows the window closing before the quit hooks, and only for the target window.
    #[gpui_kit::test]
    async fn the_lifecycle_record_orders_close_before_quit(cx: &mut TestAppContext) {
        use super::Lifecycle;
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let places = Places {
            config_dir: dir.path().to_owned(),
            socket: dir.path().join("automation.sock"),
            screenshots: dir.path().to_owned(),
        };
        let (window, workspace) = (t.window, t.workspace.clone());
        cx.update(|cx| start(places, window, workspace, cx));
        let other = gpui_kit::VisualContext::window_handle(cx.add_empty_window());
        other
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        let record = dir.path().join(Lifecycle::FILE);
        assert!(!record.exists(), "another window closing is not recorded");

        window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| cx.shutdown());
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(record).unwrap(),
            format!("{}\n{}\n", Lifecycle::WINDOW_CLOSED, Lifecycle::QUIT_HOOKS)
        );
    }
}
