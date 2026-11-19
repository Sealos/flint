//! Flint's Kubernetes core: kubeconfig, connections, reads, and the runtime bridge to front ends.
//!
//! This crate must never depend on GPUI (ADR-0001). Front ends talk to the long-running core
//! through [`CoreHandle`] (ADR-0002); one-shot callers such as the MCP server use the functions
//! in [`cluster`] and [`read`] directly on their own tokio runtime.

pub mod bootstrap;
pub mod cluster;
pub mod env_path;
pub mod handle;
pub mod kubeconfig;
mod manager;
pub mod read;
pub mod settings;

pub use cluster::{ClusterId, ServerVersion};
pub use handle::{
    ClusterSummary, ClustersSnapshot, Command, ConnectionState, CoreConfig, CoreHandle, Event,
};
pub use settings::{Settings, WatchMode};
