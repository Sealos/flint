//! The `flint` binary: the desktop app, or `flint mcp` for the read-only MCP server.

mod ui;

use std::process::ExitCode;

use flint_core::bootstrap;

/// Help for `flint`: commands, options and the environment variables Flint reads. The
/// development aids are listed apart, so nobody mistakes them for settings.
const USAGE: &str = "\
Flint, a native Kubernetes desktop manager.

usage: flint [--verbose]        start the app
       flint mcp [--verbose]    serve the read-only MCP server on stdio (see flint mcp --help)
       flint --help             print this help
       flint --version          print the version

options:
  --verbose          log info as well as warnings and errors

environment:
  KUBECONFIG         kubeconfig files to load, separated by colons (default ~/.kube/config)
  FLINT_CONFIG_DIR   where settings.toml and state.toml live. Default on macOS:
                       ~/Library/Application Support/Flint
                     on Linux: $XDG_CONFIG_HOME/flint, else ~/.config/flint
  FLINT_LOG          log filter, for example debug or flint_core=debug; overrides --verbose.
                     RUST_LOG is read when FLINT_LOG is unset, empty or invalid
  XDG_STATE_HOME     parent of flint/flint.log on Linux (default ~/.local/state).
                     On macOS the log file is ~/Library/Logs/Flint/flint.log
  SHELL              login shell Flint asks for your PATH, unless settings.toml sets path
  PATH               used, plus the usual tool directories, when that login shell fails

development and testing only:
  FLINT_CLUSTER      activate this kubeconfig context at startup, as a click would
  FLINT_VIEW         with FLINT_CLUSTER, open a view: kind, kind/name or namespace/kind/name,
                     with #yaml for the YAML tab or #logs for a pod's logs
  FLINT_APPEARANCE   light or dark, over the appearance setting";

/// Help for `flint mcp`. `exe` is this binary's absolute path, for the example.
fn mcp_usage(exe: &str) -> String {
    format!(
        "\
usage: flint mcp [--verbose]  serve the read-only MCP server on stdio
       flint mcp --help       print this help
       flint mcp --version    print the version

Register `flint mcp` as a stdio MCP server in your AI client, for example:

    claude mcp add flint -- {exe} mcp

It only sees the clusters marked Expose to AI in Flint. Diagnostics go to stderr and Flint
events also go to the app's log file (see flint --help). Warnings and errors are logged by
default. Use --verbose for info, or set FLINT_LOG (for example FLINT_LOG=debug)."
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

/// What to run, and whether to log info as well as warnings (`--verbose`).
#[derive(Debug, PartialEq, Eq)]
struct Run {
    mode: Mode,
    verbose: bool,
}

/// What to run, from the command line (program name excluded). `Err` holds what to print and
/// the exit code. `--verbose` may come anywhere: `flint --verbose mcp` and `flint mcp --verbose`
/// match. There is no `-v`: `flint -v` meaning the version elsewhere, it would start the app.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Run, (String, u8)> {
    // Finder (LaunchServices) can add a process serial number, `-psn_0_12345`, when it starts
    // an app bundle; it is not an argument for us.
    let (flags, rest): (Vec<String>, Vec<String>) = args
        .into_iter()
        .filter(|a| !a.starts_with("-psn_"))
        .partition(|a| a == "--verbose");
    let verbose = !flags.is_empty();
    let run = |mode| Ok(Run { mode, verbose });
    match rest.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => run(Mode::App),
        // Checked before the server starts: on stdio, `--help` would otherwise be served as a
        // session that ends at once.
        ["mcp"] => run(Mode::Mcp),
        ["mcp", "-h" | "--help" | "help", ..] => Err((mcp_usage(&current_exe()), 0)),
        ["mcp", "-V" | "--version" | "version", ..] => Err((version(), 0)),
        ["mcp", other, ..] => Err((
            format!("flint mcp: unknown argument \"{other}\" (try flint mcp --help)"),
            2,
        )),
        ["-h" | "--help" | "help", ..] => Err((USAGE.to_owned(), 0)),
        ["-V" | "--version" | "version", ..] => Err((version(), 0)),
        [other, ..] => Err((
            format!("flint: unknown command \"{other}\" (try --help)"),
            2,
        )),
    }
}

fn main() -> ExitCode {
    // Only argument parsing may run before `bootstrap`: it must not start threads.
    let Run { mode, verbose } = match parse_args(std::env::args().skip(1)) {
        Ok(run) => run,
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
        return unsafe { flint_mcp::serve_stdio_main("flint", verbose) };
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
    bootstrap::init_logging(verbose);
    boot.log_summary();
    ui::run(boot);
    bootstrap::flush_logs();
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{Mode, Run, parse_args, version};

    fn parse(args: &[&str]) -> Result<Run, (String, u8)> {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    fn quiet(mode: Mode) -> Result<Run, (String, u8)> {
        Ok(Run {
            mode,
            verbose: false,
        })
    }

    fn verbose(mode: Mode) -> Result<Run, (String, u8)> {
        Ok(Run {
            mode,
            verbose: true,
        })
    }

    #[test]
    fn finder_process_serial_numbers_are_ignored() {
        assert_eq!(parse(&[]), quiet(Mode::App));
        assert_eq!(parse(&["-psn_0_1234567"]), quiet(Mode::App));
        assert_eq!(parse(&["-psn_0_1", "mcp"]), quiet(Mode::Mcp));
        assert_eq!(parse(&["mcp"]), quiet(Mode::Mcp));
        assert_eq!(parse(&["--help"]).unwrap_err().1, 0);
        assert_eq!(parse(&["bogus"]).unwrap_err().1, 2);
    }

    #[test]
    fn verbose_turns_on_info_logs_for_the_app_and_the_server() {
        assert_eq!(parse(&["--verbose"]), verbose(Mode::App));
        assert_eq!(parse(&["mcp", "--verbose"]), verbose(Mode::Mcp));
        assert_eq!(parse(&["--verbose", "mcp"]), verbose(Mode::Mcp));
        assert_eq!(parse(&["--verbose", "bogus"]).unwrap_err().1, 2);
        assert_eq!(parse(&["mcp", "--verbose", "bogus"]).unwrap_err().1, 2);
        // `-v` is no flag here: it must not start the app or a server that waits on stdin.
        assert_eq!(parse(&["-v"]).unwrap_err().1, 2);
        assert_eq!(parse(&["mcp", "-v"]).unwrap_err().1, 2);
    }

    #[test]
    fn help_lists_commands_options_and_environment_variables() {
        for help in ["-h", "--help", "help"] {
            let (text, code) = parse(&[help]).unwrap_err();
            assert_eq!(code, 0);
            for want in [
                "flint mcp",
                "flint --version",
                "--verbose",
                "KUBECONFIG",
                "FLINT_CONFIG_DIR",
                "FLINT_LOG",
                "RUST_LOG",
                "XDG_STATE_HOME",
                "~/Library/Logs/Flint/flint.log",
                "SHELL",
                "PATH",
                "FLINT_CLUSTER",
                "FLINT_VIEW",
                "FLINT_APPEARANCE",
            ] {
                assert!(text.contains(want), "{want} missing from:\n{text}");
            }
            // The development aids come after their heading, apart from the real settings.
            let dev = text.find("development and testing only:").unwrap();
            for aid in ["FLINT_CLUSTER", "FLINT_VIEW", "FLINT_APPEARANCE"] {
                assert!(
                    text.find(aid).unwrap() > dev,
                    "{aid} is not marked development-only"
                );
            }
            for setting in [
                "KUBECONFIG",
                "FLINT_CONFIG_DIR",
                "FLINT_LOG",
                "XDG_STATE_HOME",
                "SHELL",
                "  PATH ",
            ] {
                assert!(
                    text.find(setting).unwrap() < dev,
                    "{setting} is under development"
                );
            }
        }
    }

    #[test]
    fn mcp_help_prints_usage_instead_of_serving() {
        for help in ["-h", "--help", "help"] {
            let (text, code) = parse(&["mcp", help]).unwrap_err();
            assert_eq!(code, 0);
            assert!(text.starts_with("usage: flint mcp"), "{text}");
            assert!(text.contains("Register `flint mcp`"), "{text}");
            assert!(text.contains("FLINT_LOG"), "{text}");
            assert!(text.contains("--verbose"), "{text}");
            // The example names this binary by its absolute path.
            let exe = std::env::current_exe().unwrap();
            assert!(
                text.contains(&format!("claude mcp add flint -- {} mcp", exe.display())),
                "{text}"
            );
        }
        assert_eq!(parse(&["mcp", "--help", "extra"]).unwrap_err().1, 0);
        assert_eq!(parse(&["--verbose", "mcp", "--help"]).unwrap_err().1, 0);
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
