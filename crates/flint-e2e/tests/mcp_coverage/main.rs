//! MCP scope, redaction and activity paths (UP-14 to UP-16), end to end: the MCP server over
//! stdio against the fake API server. No GUI and no cluster, so these run in plain `cargo test`
//! on Linux too. Every credential here is an obvious placeholder.
//!
//! Not covered here: the app's activity view (UP-16) and UP-93, which has no sending path yet.

mod activity;
mod boundary;
mod kubeconfig_files;
mod redaction;
mod scope;
mod support;
