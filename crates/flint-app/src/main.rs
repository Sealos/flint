//! The `flint` binary: the desktop app, or `flint mcp` for the read-only MCP server.

mod ui;

use std::process::ExitCode;

use flint_core::bootstrap;

const USAGE: &str = "\
usage: flint                start the app
       flint mcp            serve the read-only MCP server on stdio (see flint mcp --help)
       flint --version      print the version";

/// Help for `flint mcp`. `exe` is this binary's absolute path, for the example.
fn mcp_usage(exe: &str) -> String {
    format!(
        "\
usage: flint mcp            serve the read-only MCP server on stdio
       flint mcp --help     print this help
       flint mcp --version  print the version

Register `flint mcp` as a stdio MCP server in your AI client, for example:

    claude mcp add flint -- {exe} mcp

It only sees the clusters marked Expose to AI in Flint. Logs go to stderr; set FLINT_LOG
(for example FLINT_LOG=debug) for more."
    )
}

/// This binary's absolute path, or `flint` if it cannot be found.
fn current_exe() -> String {
    std::env::current_exe().map_or_else(|_| "flint".to_owned(), |p| p.display().to_string())
}

fn version() -> String {
    format!("flint {}", env!("CARGO_PKG_VERSION"))
}

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
        // Checked before the server starts: on stdio, `--help` would otherwise be served as a
        // session that ends at once.
        Some("mcp") => match args.next().as_deref() {
            None => Ok(Mode::Mcp),
            Some("-h" | "--help" | "help") => Err((mcp_usage(&current_exe()), 0)),
            Some("-V" | "--version" | "version") => Err((version(), 0)),
            Some(other) => Err((
                format!("flint mcp: unknown argument \"{other}\" (try flint mcp --help)"),
                2,
            )),
        },
        Some("-h" | "--help" | "help") => Err((USAGE.to_owned(), 0)),
        Some("-V" | "--version" | "version") => Err((version(), 0)),
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

    if mode == Mode::Mcp {
        // SAFETY: nothing has started a thread yet.
        return unsafe { flint_mcp::serve_stdio_main("flint") };
    }

    // SAFETY: nothing has started a thread yet. This must stay ahead of anything that might:
    // it changes the process environment (PATH), which is only sound while single-threaded.
    let boot = match unsafe { bootstrap::bootstrap() } {
        Ok(boot) => boot,
        Err(e) => {
            eprintln!("flint: {e}");
            return ExitCode::FAILURE;
        }
    };
    bootstrap::init_logging();
    boot.log_summary();
    ui::run(boot);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{Mode, parse_args, version};

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
    fn mcp_help_prints_usage_instead_of_serving() {
        for help in ["-h", "--help", "help"] {
            let (text, code) = parse(&["mcp", help]).unwrap_err();
            assert_eq!(code, 0);
            assert!(text.starts_with("usage: flint mcp"), "{text}");
            assert!(text.contains("Register `flint mcp`"), "{text}");
            assert!(text.contains("FLINT_LOG"), "{text}");
            // The example names this binary by its absolute path.
            let exe = std::env::current_exe().unwrap();
            assert!(
                text.contains(&format!("claude mcp add flint -- {} mcp", exe.display())),
                "{text}"
            );
        }
        assert_eq!(parse(&["mcp", "--version"]), Err((version(), 0)));
        assert!(
            parse(&["--help"])
                .unwrap_err()
                .0
                .contains("flint mcp --help")
        );
        let (text, code) = parse(&["mcp", "--bogus"]).unwrap_err();
        assert_eq!(code, 2);
        assert!(text.contains("--bogus"), "{text}");
    }

    #[test]
    fn version_prints_the_crate_version() {
        let (text, code) = parse(&["--version"]).unwrap_err();
        assert_eq!(code, 0);
        assert_eq!(text, format!("flint {}", env!("CARGO_PKG_VERSION")));
    }
}
