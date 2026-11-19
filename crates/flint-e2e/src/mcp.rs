//! [`Mcp`]: Flint's read-only MCP server over stdio (JSON-RPC, one message per line), as an AI
//! client runs it.
//!
//! [`Mcp::start`] runs the GPUI-free `flint-mcp` binary, so MCP e2e tests run on Linux too.
//! [`Mcp::flint`] runs `flint mcp` from an app binary, for tests that check the app and the
//! server agree.
//!
//! Either way the server gets exactly the given kubeconfig (never an empty one, which would fall
//! back to `~/.kube/config`), the given config directory (with a `settings.toml` setting a fixed
//! `path` when there is none, so no login shell runs), and a scratch `HOME`. Inherited
//! `KUBECONFIG` and `FLINT_*` variables are dropped. The server exits when its stdin closes, so
//! it never outlives the test.

use std::ffi::OsStr;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::binaries;
use crate::scratch::{self, Scratch};

/// How long one request may take.
const ANSWER: Duration = Duration::from_secs(60);

/// A running MCP server, initialized. Dropping it kills the server.
pub struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
    /// The scratch `HOME`.
    _home: Scratch,
}

impl Mcp {
    /// Starts `flint-mcp` (see [`binaries::mcp`]) on `kubeconfig` (a file or a `KUBECONFIG`
    /// list) and the scratch `config_dir`, and initializes the session.
    pub fn start(kubeconfig: impl AsRef<OsStr>, config_dir: &Path) -> Result<Mcp> {
        Mcp::spawn(&binaries::mcp()?, &[], kubeconfig.as_ref(), config_dir)
    }

    /// Starts `<flint> mcp`, the subcommand users register with their AI client.
    pub fn flint(flint: &Path, kubeconfig: impl AsRef<OsStr>, config_dir: &Path) -> Result<Mcp> {
        Mcp::spawn(flint, &["mcp"], kubeconfig.as_ref(), config_dir)
    }

    fn spawn(binary: &Path, args: &[&str], kubeconfig: &OsStr, config_dir: &Path) -> Result<Mcp> {
        if std::env::split_paths(kubeconfig).all(|p| p.as_os_str().is_empty()) {
            bail!("the kubeconfig names no file; the server would read ~/.kube/config");
        }
        if !config_dir.join("settings.toml").exists() {
            scratch::write_settings(config_dir, "")?;
        }
        let home = Scratch::new()?;
        let mut command = Command::new(binary);
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy();
            if key == "KUBECONFIG" || key.starts_with("FLINT_") {
                command.env_remove(&*key);
            }
        }
        let mut child = command
            .args(args)
            .env("KUBECONFIG", kubeconfig)
            .env("FLINT_CONFIG_DIR", config_dir)
            .env("HOME", home.home())
            .env("FLINT_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting {}", binary.display()))?;
        let stdin = child.stdin.take().context("the server's stdin")?;
        let stdout = child.stdout.take().context("the server's stdout")?;
        // Read on a thread, so a silent or dead server fails the test instead of hanging it.
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut mcp = Mcp {
            child,
            stdin,
            lines,
            next_id: 0,
            _home: home,
        };
        let init = mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "flint-e2e", "version": "0"}
            }),
        )?;
        if init["serverInfo"]["name"] != "flint" {
            bail!("not Flint's MCP server: {init}");
        }
        mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(mcp)
    }

    /// Sends a JSON-RPC request and returns its `result`. A JSON-RPC error is an `Err`.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let line = match self.lines.recv_timeout(ANSWER) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("{method}: no answer within {ANSWER:?}")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!(
                        "{method}: the server closed stdout ({:?})",
                        self.child.try_wait()
                    )
                }
            };
            let message: Value = serde_json::from_str(&line)
                .with_context(|| format!("stdout must carry only JSON-RPC: {line}"))?;
            if message["id"] != json!(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!("{method} failed: {error}");
            }
            return Ok(message["result"].clone());
        }
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        writeln!(self.stdin, "{message}")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// The tools' names, sorted.
    pub fn tools(&mut self) -> Result<Vec<String>> {
        let tools = self.request("tools/list", json!({}))?;
        let mut names: Vec<String> = tools["tools"]
            .as_array()
            .context("tools/list has no tools")?
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Calls a tool and returns whether it failed, and its text.
    pub fn call(&mut self, tool: &str, args: Value) -> Result<ToolResult> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": args}))?;
        Ok(ToolResult {
            is_error: result["isError"] == json!(true),
            text: result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// Calls a tool that must succeed and returns its JSON.
    pub fn call_json(&mut self, tool: &str, args: Value) -> Result<Value> {
        let result = self.call(tool, args.clone())?;
        if result.is_error {
            bail!("{tool} {args} failed: {}", result.text);
        }
        serde_json::from_str(&result.text)
            .with_context(|| format!("{tool} returned no JSON: {}", result.text))
    }
}

/// What a tool call returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub is_error: bool,
    pub text: String,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    // The server itself is exercised by tests/mcp.rs; this checks only that a missing binary is
    // an error, not a hang.
    #[test]
    fn an_empty_kubeconfig_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for empty in ["", ":", "::"] {
            let error = super::Mcp::flint(&dir.path().join("nope"), empty, dir.path())
                .err()
                .unwrap();
            assert!(error.to_string().contains("names no file"), "{error}");
        }
    }

    #[test]
    fn a_missing_binary_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = super::Mcp::flint(&dir.path().join("nope"), "/nonexistent", dir.path())
            .err()
            .unwrap();
        assert!(error.to_string().contains("starting"), "{error}");
    }
}
