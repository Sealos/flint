//! End-to-end test harness for Flint.
//!
//! - [`fake_api`]: a fake Kubernetes API server over HTTPS that replays fixtures recorded from
//!   the kind dev cluster, serves scripted watch streams and injects faults.
//! - [`record`]: records those fixtures (`scripts/record-fixtures.sh`).
//!
//! See `docs/TESTING.md`.

pub mod fake_api;
pub mod record;
