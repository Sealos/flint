//! `flint-mcp`: the read-only MCP server on stdio, without the desktop app.
//!
//! It serves the same [`flint_mcp::run_stdio`] as `flint mcp`, but links no GPUI, so it builds on
//! Linux. CI runs the MCP end-to-end tests against kind with it. Users get `flint mcp`.

use std::process::ExitCode;

use flint_core::bootstrap::{self, Bootstrap};
use flint_core::env_path::PathSource;

/// `Ok` to serve; `Err` holds what to print and the exit code. Program name excluded.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<(), (String, u8)> {
    match args.into_iter().next().as_deref() {
        None => Ok(()),
        Some("-h" | "--help" | "help") => Err((
            "usage: flint-mcp            serve the read-only MCP server on stdio\n       flint-mcp --version  print the version"
                .to_owned(),
            0,
        )),
        Some("-V" | "--version" | "version") => {
            Err((format!("flint-mcp {}", env!("CARGO_PKG_VERSION")), 0))
        }
        Some(other) => Err((
            format!("flint-mcp: unknown argument \"{other}\" (try --help)"),
            2,
        )),
    }
}

fn main() -> ExitCode {
    // Only argument parsing may run before `bootstrap`: it must not start threads.
    match parse_args(std::env::args().skip(1)) {
        Ok(()) => {}
        Err((text, 0)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err((text, code)) => {
            eprintln!("{text}");
            return ExitCode::from(code);
        }
    }

    // SAFETY: nothing has started a thread yet. This must stay ahead of anything that might:
    // it changes the process environment (PATH), which is only sound while single-threaded.
    let boot = match unsafe { bootstrap::bootstrap() } {
        Ok(boot) => boot,
        Err(e) => {
            eprintln!("flint-mcp: {e}");
            return ExitCode::FAILURE;
        }
    };
    init_logging();
    log_bootstrap(&boot);

    match flint_mcp::run_stdio(&boot) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("mcp server failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Logs go to stderr: stdout carries the protocol.
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

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn parse(args: &[&str]) -> Result<(), (String, u8)> {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn no_arguments_serves() {
        assert_eq!(parse(&[]), Ok(()));
    }

    #[test]
    fn help_and_version_exit_zero_and_unknown_arguments_exit_two() {
        assert_eq!(parse(&["--help"]).unwrap_err().1, 0);
        let (text, code) = parse(&["--version"]).unwrap_err();
        assert_eq!(code, 0);
        assert_eq!(text, format!("flint-mcp {}", env!("CARGO_PKG_VERSION")));
        // `flint mcp` takes the subcommand; this binary does not.
        assert_eq!(parse(&["mcp"]).unwrap_err().1, 2);
    }
}
