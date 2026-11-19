//! `flint mcp` serves the MCP server. Its behavior is tested in `flint-mcp` (tests/mcp_stdio.rs,
//! through the GPUI-free `flint-mcp` binary); this checks only that the app's subcommand reaches
//! the same server. No cluster: the kubeconfig names 127.0.0.1:1, which refuses connections.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

// covers UP-09
#[test]
fn flint_mcp_answers_on_stdio() {
    let scratch = tempfile::tempdir().unwrap();
    let kubeconfig = scratch.path().join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: https://127.0.0.1:1\nusers:\n- name: u\n  user:\n    token: unused\ncontexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n",
    )
    .unwrap();
    let config_dir = scratch.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    // A fixed PATH, so no login shell runs.
    std::fs::write(
        config_dir.join("settings.toml"),
        "path = \"/usr/bin:/bin\"\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_flint"))
        .arg("mcp")
        .env("KUBECONFIG", &kubeconfig)
        .env("FLINT_CONFIG_DIR", &config_dir)
        .env("FLINT_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut exchange = |msg: Value| -> Option<Value> {
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        msg.get("id")?;
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON-RPC ({e}): {line}")))
    };

    let init = exchange(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "flint-test", "version": "0"}
        }}),
    )
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "flint", "{init}");
    exchange(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let tools = exchange(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert!(
        tools.iter().any(|t| t["name"] == "list_clusters"),
        "{tools:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
}
