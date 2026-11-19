//! Flint's read-only MCP server (ADR-0009), served over stdio by `flint mcp`.
//!
//! The read functions live in `flint-core`; this crate holds only the tool layer and the policy:
//! opt-in clusters, redaction, and output bounds. It must never depend on GPUI.

pub mod policy;
mod server;

pub use server::FlintMcp;

use flint_core::bootstrap::Bootstrap;
use rmcp::ServiceExt;

/// Serves MCP over stdin/stdout until the client disconnects. Starts its own tokio runtime, so
/// call it after [`flint_core::bootstrap::bootstrap`] and from a plain (non-async) context.
/// stdout carries only the protocol; log to stderr.
pub fn run_stdio(bootstrap: &Bootstrap) -> anyhow::Result<()> {
    // A mostly idle stdio server: two workers are plenty.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("flint-mcp")
        .enable_all()
        .build()?;
    let server = FlintMcp::new(
        bootstrap.config_dir.clone(),
        bootstrap.kubeconfig_base.clone(),
        bootstrap.home.clone(),
    );
    let result = runtime.block_on(async move {
        let service = server.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        anyhow::Ok(())
    });
    // stdin reads cannot be cancelled; do not wait for them on the way out, on success or error.
    runtime.shutdown_background();
    result
}
