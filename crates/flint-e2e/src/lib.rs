//! End-to-end test harness for Flint.
//!
//! - [`fake_api`]: a fake Kubernetes API server over HTTPS that replays fixtures recorded from
//!   the kind dev cluster, serves scripted watch streams and injects faults.
//! - [`record`]: records those fixtures (`scripts/record-fixtures.sh`).
//! - [`app`]: the real app, started with the test-only automation channel and driven over its
//!   socket.
//! - [`teardown`]: the checks every app test makes when the app goes away (UP-90, UP-91).
//! - [`mcp`]: the read-only MCP server over stdio.
//! - [`backend`]: the cluster a test talks to, the fake API server or the kind dev cluster.
//! - [`scratch`] and [`binaries`]: scratch directories, and the binaries the harness runs.
//!
//! See `docs/TESTING.md`.

pub mod app;
pub mod backend;
pub mod binaries;
pub mod fake_api;
pub mod mcp;
pub mod record;
pub mod scratch;
pub mod teardown;
