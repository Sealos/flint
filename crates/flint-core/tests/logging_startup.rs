//! A startup failure reaches the log file, in a process of its own: the subscriber is global.

use std::process::Command;

use flint_core::bootstrap;

const CHILD: &str = "FLINT_TEST_LOG_STARTUP_FAILURE";

// covers UP-01: a launch that fails before logging starts still leaves its reason in the file,
// and prints nothing more on stderr than the caller already did.
#[test]
fn a_startup_failure_is_written_to_the_log_file_only() {
    if std::env::var_os(CHILD).is_some() {
        bootstrap::log_startup_failure("flint: no config directory");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "a_startup_failure_is_written_to_the_log_file_only",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("HOME", scratch.path())
        .env("XDG_STATE_HOME", scratch.path().join("state"))
        .env_remove("FLINT_LOG")
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let log = if cfg!(target_os = "macos") {
        scratch.path().join("Library/Logs/Flint/flint.log")
    } else {
        scratch.path().join("state/flint/flint.log")
    };
    let text = std::fs::read_to_string(log).unwrap();
    assert!(text.contains("flint: no config directory"), "{text}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("no config directory"), "{stderr}");
}
