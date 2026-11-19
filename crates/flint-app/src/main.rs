//! The `flint` binary: the desktop app, or `flint mcp` for the read-only MCP server.

mod ui;

use std::process::ExitCode;

use flint_core::bootstrap::{self, Bootstrap};
use flint_core::env_path::PathSource;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    App,
    Mcp,
}

/// What to run, from the command line (program name excluded). `Err` holds what to print and
/// the exit code.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Mode, (String, u8)> {
    // Finder (LaunchServices) can add a process serial number, `-psn_0_12345`, when it starts
    // an app bundle; it is not an argument for us.
    let mut args = args.into_iter().filter(|a| !a.starts_with("-psn_"));
    match args.next().as_deref() {
        None => Ok(Mode::App),
        Some("mcp") => Ok(Mode::Mcp),
        Some("-h" | "--help" | "help") => Err((
            "usage: flint            start the app\n       flint mcp        serve the read-only MCP server on stdio\n       flint --version  print the version"
                .to_owned(),
            0,
        )),
        Some("-V" | "--version" | "version") => {
            Err((format!("flint {}", env!("CARGO_PKG_VERSION")), 0))
        }
        Some(other) => Err((
            format!("flint: unknown command \"{other}\" (try --help)"),
            2,
        )),
    }
}

fn main() -> ExitCode {
    // Only argument parsing may run before `bootstrap`: it must not start threads.
    let mode = match parse_args(std::env::args().skip(1)) {
        Ok(mode) => mode,
        Err((text, 0)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err((text, code)) => {
            eprintln!("{text}");
            return ExitCode::from(code);
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

#[cfg(test)]
mod tests {
    use super::{Mode, parse_args};

    fn parse(args: &[&str]) -> Result<Mode, (String, u8)> {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn finder_process_serial_numbers_are_ignored() {
        assert_eq!(parse(&[]), Ok(Mode::App));
        assert_eq!(parse(&["-psn_0_1234567"]), Ok(Mode::App));
        assert_eq!(parse(&["-psn_0_1", "mcp"]), Ok(Mode::Mcp));
        assert_eq!(parse(&["mcp"]), Ok(Mode::Mcp));
        assert_eq!(parse(&["--help"]).unwrap_err().1, 0);
        assert_eq!(parse(&["bogus"]).unwrap_err().1, 2);
    }

    #[test]
    fn version_prints_the_crate_version() {
        let (text, code) = parse(&["--version"]).unwrap_err();
        assert_eq!(code, 0);
        assert_eq!(text, format!("flint {}", env!("CARGO_PKG_VERSION")));
    }
}
