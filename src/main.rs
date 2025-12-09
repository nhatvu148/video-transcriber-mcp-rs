use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::{
    transport::{stdio, streamable_http_server::StreamableHttpService},
    ServiceExt,
};
use tracing::Level;

mod mcp;
mod transcriber;
mod utils;

use mcp::VideoTranscriberServer;

/// Transport mode for the MCP server
#[derive(Debug, Clone, ValueEnum)]
enum Transport {
    /// Standard I/O transport (default for local CLI usage)
    Stdio,
    /// Streamable HTTP transport (for remote access)
    Http,
}

/// High-performance video transcription MCP server using whisper.cpp
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Transport mode to use
    #[arg(short, long, value_enum, default_value = "stdio")]
    transport: Transport,

    /// Host address for HTTP transport
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port for HTTP transport
    #[arg(short, long, default_value = "8080")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging to stderr so stdout is clean for MCP (stdio mode)
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(matches!(args.transport, Transport::Http)) // Enable ANSI for HTTP mode
        .init();

    tracing::info!(
        "Video Transcriber MCP Server (Rust) - v{}",
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!("Powered by whisper.cpp - 6x faster than Python whisper!");

    match args.transport {
        Transport::Stdio => run_stdio_transport().await,
        Transport::Http => run_http_transport(&args.host, args.port).await,
    }
}

/// Run the MCP server with stdio transport (for local CLI usage)
async fn run_stdio_transport() -> Result<()> {
    tracing::info!("Starting stdio transport...");

    let server = VideoTranscriberServer::new();
    let service = server.serve(stdio()).await?;

    // Wait for shutdown
    service.waiting().await?;

    Ok(())
}

/// Run the MCP server with Streamable HTTP transport (for remote access)
async fn run_http_transport(host: &str, port: u16) -> Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

    tracing::info!("Starting Streamable HTTP transport on {}:{}...", host, port);

    // Create the Streamable HTTP service
    // Each session gets its own VideoTranscriberServer instance
    let service = StreamableHttpService::new(
        || Ok(VideoTranscriberServer::new()),
        LocalSessionManager::default().into(),
        Default::default(),
    );

    // Create axum router with the MCP endpoint
    let router = axum::Router::new().nest_service("/mcp", service);

    // Bind and serve
    let addr = format!("{}:{}", host, port);
    let tcp_listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("=================================================");
    tracing::info!("MCP Server ready at http://{}/mcp", addr);
    tracing::info!("=================================================");
    tracing::info!("");
    tracing::info!("Add to your MCP client configuration:");
    tracing::info!("  {{");
    tracing::info!("    \"mcpServers\": {{");
    tracing::info!("      \"video-transcriber-mcp\": {{");
    tracing::info!("        \"url\": \"http://{}/mcp\"", addr);
    tracing::info!("      }}");
    tracing::info!("    }}");
    tracing::info!("  }}");
    tracing::info!("");

    axum::serve(tcp_listener, router).await?;

    Ok(())
}
