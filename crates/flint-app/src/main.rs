//! The `flint` binary: the desktop app, or `flint mcp` for the read-only MCP server.

mod ui;

use std::process::ExitCode;

use flint_core::bootstrap::{self, Bootstrap};
use flint_core::env_path::PathSource;

enum Mode {
    App,
    Mcp,
}

fn main() -> ExitCode {
    // Only argument parsing may run before `bootstrap`: it must not start threads.
    let mode = match std::env::args().nth(1).as_deref() {
        None => Mode::App,
        Some("mcp") => Mode::Mcp,
        Some("-h" | "--help" | "help") => {
            println!(
                "usage: flint          start the app\n       flint mcp      serve the read-only MCP server on stdio"
            );
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("flint: unknown command \"{other}\" (try --help)");
            return ExitCode::from(2);
        }
    };

    // SAFETY: nothing has started a thread yet. This must stay ahead of anything that might:
    // it changes the process environment (PATH), which is only sound while single-threaded.
    let boot = match unsafe { bootstrap::bootstrap() } {
        Ok(boot) => boot,
        Err(e) => {
            eprintln!("flint: {e}");
            return ExitCode::FAILURE;
        }
    };
    init_logging();
    log_bootstrap(&boot);

    match mode {
        Mode::App => {
            ui::run(boot);
            ExitCode::SUCCESS
        }
        Mode::Mcp => match flint_mcp::run_stdio(&boot) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("mcp server failed: {e:#}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Logs go to stderr: in `flint mcp`, stdout carries the protocol.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("FLINT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

fn log_bootstrap(boot: &Bootstrap) {
    match &boot.path.source {
        PathSource::Settings => tracing::info!("PATH from settings.toml"),
        PathSource::LoginShell => tracing::info!("PATH from login shell"),
        PathSource::Fallback(reason) => {
            tracing::warn!(reason, "PATH from fallback (login shell not used)")
        }
    }
    tracing::debug!(path = %boot.path.value, "resolved PATH");
    tracing::info!(config_dir = %boot.config_dir.display(), "config directory");
    if let Some(e) = &boot.settings_error {
        tracing::warn!("using default settings: {e}");
    }
}
