//! App e2e: the real Flint app, built with `--features automation`, driven over its automation
//! socket through one test per user path (docs/USER-PATHS.md), against the fake API server by
//! default.
//!
//! macOS only, and ignored by default: they open a window, so they need a GUI session. Run them
//! with (see docs/TESTING.md, "Running app e2e"):
//!
//! ```sh
//! FLINT_AUTOMATION_SCREENSHOTS="$PWD/target/e2e-screenshots" \
//!   cargo test -p flint-e2e --test app -- --ignored
//! ```
#![cfg(target_os = "macos")]

mod exec_plugins;
mod harness;
mod m0;
mod m1;
mod m2;
mod m2_shell;
mod m3;
mod m3_writes;
mod support;
