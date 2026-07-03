//! `m1-mcp` binary: serve the M1 toolchain MCP tools over stdio.
//!
//! stdout is the JSON-RPC channel, so all logging goes to **stderr**. Run with
//! `RUST_LOG=debug` for verbose tracing.

use anyhow::Result;
use m1_mcp::M1Server;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Logs to stderr only — stdout carries the MCP protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("starting m1-mcp {}", env!("CARGO_PKG_VERSION"));

    let service = M1Server::new().serve(stdio()).await?;
    let reason = service.waiting().await?;
    tracing::info!(?reason, "m1-mcp stopped");
    Ok(())
}
