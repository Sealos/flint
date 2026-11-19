//! Finds or builds the binaries the harness runs: `flint` with `--features automation` for
//! [`crate::app::App`], and `flint-mcp` for [`crate::mcp::Mcp`].
//!
//! An environment variable can name a binary built elsewhere (CI builds once, then runs the
//! tests): `FLINT_E2E_APP_BIN` for the app, `FLINT_E2E_MCP_BIN` for the MCP server. Otherwise the
//! binary is built with cargo, once per test process, into the workspace's target directory.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

/// Names a prebuilt `flint` (built with `--features automation`, without `--release`).
pub const APP_BIN_ENV: &str = "FLINT_E2E_APP_BIN";
/// Names a prebuilt `flint-mcp`.
pub const MCP_BIN_ENV: &str = "FLINT_E2E_MCP_BIN";

/// `flint` with the automation channel.
pub fn app() -> Result<PathBuf> {
    static APP: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    locate(
        &APP,
        APP_BIN_ENV,
        &[
            "-p",
            "flint-app",
            "--features",
            "automation",
            "--bin",
            "flint",
        ],
        "flint",
    )
}

/// The GPUI-free MCP server, `flint-mcp` (the same server as `flint mcp`).
pub fn mcp() -> Result<PathBuf> {
    static MCP: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    locate(
        &MCP,
        MCP_BIN_ENV,
        &["-p", "flint-mcp", "--bin", "flint-mcp"],
        "flint-mcp",
    )
}

fn locate(
    cell: &OnceLock<Result<PathBuf, String>>,
    env: &str,
    args: &[&str],
    name: &str,
) -> Result<PathBuf> {
    cell.get_or_init(|| {
        if let Some(path) = std::env::var_os(env).filter(|v| !v.is_empty()) {
            let path = PathBuf::from(path);
            return if path.is_file() {
                Ok(path)
            } else {
                Err(format!(
                    "{env} names {}, which is not a file",
                    path.display()
                ))
            };
        }
        build(args, name).map_err(|e| format!("{e:#}"))
    })
    .clone()
    .map_err(anyhow::Error::msg)
}

/// Runs `cargo build <args>` in the workspace and returns the path of the executable `name`.
fn build(args: &[&str], name: &str) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .arg("build")
        .args(args)
        .args(["--locked", "--message-format=json-render-diagnostics"])
        .current_dir(workspace_root())
        .output()
        .context("running cargo build")?;
    if !output.status.success() {
        bail!(
            "cargo build {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    executable(&String::from_utf8_lossy(&output.stdout), name)
        .with_context(|| format!("cargo build {} built no {name}", args.join(" ")))
}

/// The executable named `name` among cargo's JSON messages.
fn executable(messages: &str, name: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
        .next_back()
}

fn workspace_root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_executable_in_cargo_messages() {
        let messages = r#"{"reason":"compiler-artifact","target":{"name":"flint-core"},"executable":null}
not json
{"reason":"compiler-artifact","target":{"name":"flint"},"executable":"/t/debug/flint"}
{"reason":"build-finished","success":true}"#;
        assert_eq!(
            executable(messages, "flint"),
            Some(PathBuf::from("/t/debug/flint"))
        );
        assert_eq!(executable(messages, "flint-mcp"), None);
    }

    #[test]
    fn the_workspace_root_has_the_workspace_manifest() {
        let manifest = std::fs::read_to_string(workspace_root().join("Cargo.toml")).unwrap();
        assert!(manifest.contains("[workspace]"));
    }
}
