use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::{
    ServiceExt,
    transport::{stdio, streamable_http_server::StreamableHttpService},
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use tower_http::cors::{Any, CorsLayer};
use tracing::Level;

mod api;
mod llm;
mod mcp;
mod transcriber;
mod utils;

use api::AppState;
use mcp::VideoTranscriberServer;
use transcriber::TranscriberEngine;

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

    // MCP service (per-session VideoTranscriberServer)
    let mcp_service = StreamableHttpService::new(
        || Ok(VideoTranscriberServer::new()),
        LocalSessionManager::default().into(),
        Default::default(),
    );

    // REST API state shared across all jobs
    let app_state = AppState {
        jobs: api::new_store(),
        engine: Arc::new(Mutex::new(TranscriberEngine::new())),
    };
    let api_router = api::router(app_state);

    // Permissive CORS for local dev — clients are typically browser-based.
    // Tighten in production deployments.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Per-IP rate limit on the /api/* surface. Tuned to accommodate the
    // web/extension's job-polling pattern (~24 req/min while a job runs)
    // while blocking abusive bursts. Pairs with Modal + OpenRouter spending
    // caps for defence-in-depth: this throttles request frequency, the
    // dashboards cap aggregate cost.
    //
    //   - per_second: steady-state allowance
    //   - burst_size: initial allowance before throttling kicks in
    //
    // A misbehaving client hitting POST /api/jobs at full speed gets ~20
    // requests through immediately, then 1 per second thereafter — bounded
    // and visible in logs.
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(1)
            .burst_size(20)
            .finish()
            .expect("failed to build rate limit config"),
    );
    let governor_layer = GovernorLayer::new(governor_conf);

    let router = axum::Router::new()
        .nest("/api", api_router.layer(governor_layer))
        .nest_service("/mcp", mcp_service)
        .layer(cors);

    let addr = format!("{}:{}", host, port);
    let tcp_listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("=================================================");
    tracing::info!("Server ready");
    tracing::info!("  MCP:  http://{}/mcp", addr);
    tracing::info!("  REST: http://{}/api/jobs", addr);
    tracing::info!("=================================================");

    axum::serve(tcp_listener, router).await?;

    Ok(())
}
