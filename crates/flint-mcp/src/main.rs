//! `flint-mcp`: the read-only MCP server on stdio, without the desktop app.
//!
//! It runs the same [`flint_mcp::serve_stdio_main`] as `flint mcp`, but links no GPUI, so it builds
//! on Linux. CI runs the MCP end-to-end tests against kind with it. It is not shipped: users run
//! `flint mcp`.

use std::process::ExitCode;

const USAGE: &str = "\
usage: flint-mcp [--verbose]  serve the read-only MCP server on stdio
       flint-mcp --version    print the version

--verbose logs info as well as warnings. FLINT_LOG (or RUST_LOG) sets the filter
exactly, for example FLINT_LOG=debug.

Same server as `flint mcp`, without the desktop app. Not shipped; register `flint mcp` in AI clients.";

/// `Ok(verbose)` to serve; `Err` holds what to print and the exit code. Program name excluded.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<bool, (String, u8)> {
    let (verbose, rest): (Vec<String>, Vec<String>) =
        args.into_iter().partition(|a| a == "--verbose");
    match rest.first().map(String::as_str) {
        None => Ok(!verbose.is_empty()),
        Some("-h" | "--help" | "help") => Err((USAGE.to_owned(), 0)),
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
    // Only argument parsing may run before the server's bootstrap: it must not start threads.
    let verbose = match parse_args(std::env::args().skip(1)) {
        Ok(verbose) => verbose,
        Err((text, 0)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err((text, code)) => {
            eprintln!("{text}");
            return ExitCode::from(code);
        }
    };
    // SAFETY: nothing has started a thread yet.
    unsafe { flint_mcp::serve_stdio_main("flint-mcp", verbose) }
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn parse(args: &[&str]) -> Result<bool, (String, u8)> {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn no_arguments_serves() {
        assert_eq!(parse(&[]), Ok(false));
    }

    #[test]
    fn verbose_serves_with_more_logging() {
        assert_eq!(parse(&["--verbose"]), Ok(true));
        assert_eq!(parse(&["-v"]).unwrap_err().1, 2);
        assert!(parse(&["--help"]).unwrap_err().0.contains("--verbose"));
        assert_eq!(parse(&["--verbose", "bogus"]).unwrap_err().1, 2);
    }

    #[test]
    fn help_and_version_exit_zero_and_unknown_arguments_exit_two() {
        let (help, code) = parse(&["--help"]).unwrap_err();
        assert_eq!(code, 0);
        assert!(help.contains("register `flint mcp`"), "{help}");
        let (text, code) = parse(&["--version"]).unwrap_err();
        assert_eq!(code, 0);
        assert_eq!(text, format!("flint-mcp {}", env!("CARGO_PKG_VERSION")));
        // `flint mcp` takes the subcommand; this binary does not.
        assert_eq!(parse(&["mcp"]).unwrap_err().1, 2);
    }
}
