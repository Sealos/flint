//! Flint's read-only MCP server (ADR-0009), served over stdio by `flint mcp`.
//!
//! The read functions live in `flint-core`; this crate holds only the tool layer and the policy:
//! opt-in clusters, redaction, and output bounds. It must never depend on GPUI.

pub mod policy;
mod server;

pub use server::FlintMcp;

use std::process::ExitCode;

use flint_core::bootstrap::{self, Bootstrap};
use rmcp::ServiceExt;

/// The whole MCP server process, after argument parsing: bootstrap, logging to stderr, then
/// [`run_stdio`] until the client disconnects. `flint mcp` and the `flint-mcp` binary both run
/// this, so they behave the same. `program` prefixes a startup error; `verbose` (`--verbose`)
/// logs info as well as warnings, unless `FLINT_LOG` or `RUST_LOG` says otherwise.
///
/// # Safety
///
/// Like [`bootstrap::bootstrap`]: call it while the process is single-threaded, before anything
/// starts a thread.
pub unsafe fn serve_stdio_main(program: &str, verbose: bool) -> ExitCode {
    // SAFETY: forwarded to the caller.
    let boot = match unsafe { bootstrap::bootstrap() } {
        Ok(boot) => boot,
        Err(e) => {
            eprintln!("{program}: {e}");
            return ExitCode::FAILURE;
        }
    };
    bootstrap::init_logging(verbose);
    boot.log_summary();
    match run_stdio(&boot) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("mcp server failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

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
