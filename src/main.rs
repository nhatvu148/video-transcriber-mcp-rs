use anyhow::Result;
use rmcp::{transport::stdio, ServiceExt};
use tracing::Level;
use tracing_subscriber;

mod mcp;
mod transcriber;
mod utils;

use mcp::VideoTranscriberServer;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging to stderr so stdout is clean for MCP
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(false)
        .init();

    tracing::info!("🚀 Video Transcriber MCP Server (Rust) - v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("⚡ Powered by whisper.cpp - 6x faster than Python whisper!");

    // Create server
    let server = VideoTranscriberServer::new();

    // Run server with stdio transport
    let service = server.serve(stdio()).await?;

    // Wait for shutdown
    service.waiting().await?;

    Ok(())
}
