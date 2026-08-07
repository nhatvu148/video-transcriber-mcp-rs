//! MCP server for video transcription — stdio or streamable HTTP.
//!
//! This binary is only the protocol surface over the transcription pipeline.
//! The product that was once served from here (REST API, accounts, credits,
//! Stripe, the AI study layer, x402 payments) now lives in a separate private
//! crate that depends on this one as a library. Keeping them apart means
//! `cargo install video-transcriber-mcp` builds a transcription server, not a
//! SaaS backend.
use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService},
    },
};
use tower_http::cors::{Any, CorsLayer};
use tracing::Level;

// Use the library rather than re-declaring the modules: `mod transcriber;`
// here alongside `pub mod transcriber;` in lib.rs compiles the whole tree
// twice and makes anything the binary doesn't call look dead.
use video_transcriber_mcp::mcp::VideoTranscriberServer;

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

    // Log to stderr so stdout stays clean for the stdio transport.
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(matches!(args.transport, Transport::Http))
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
    service.waiting().await?;

    Ok(())
}

/// Run the MCP server with Streamable HTTP transport (for remote access)
async fn run_http_transport(host: &str, port: u16) -> Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

    tracing::info!("Starting Streamable HTTP transport on {}:{}...", host, port);

    // The streamable-HTTP transport only answers for hosts on this list —
    // rmcp's DNS-rebinding protection, which defaults to loopback. That means a
    // deployed instance 403s requests carrying its own public hostname, so no
    // remote MCP client can reach `/mcp` until you name it.
    //
    // `MCP_ALLOWED_HOSTS` (comma-separated) is additive on top of the built-in
    // loopback defaults, so local dev and health checks keep working, and it is
    // opt-in with no wildcard. Starting from rmcp's own defaults rather than
    // re-declaring them means an rmcp upgrade carries through instead of
    // silently diverging.
    let mut mcp_config = StreamableHttpServerConfig::default();
    let configured: Vec<String> = std::env::var("MCP_ALLOWED_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .collect();
    if configured.is_empty() {
        tracing::info!(
            "MCP transport accepting default hosts only ({}) — set MCP_ALLOWED_HOSTS to serve remote clients",
            mcp_config.allowed_hosts.join(", ")
        );
    } else {
        tracing::info!("MCP transport also accepting Host: {}", configured.join(", "));
        mcp_config.allowed_hosts.extend(configured);
    }

    let mcp_service = StreamableHttpService::new(
        // Reachable by others, unlike the stdio transport.
        || Ok(VideoTranscriberServer::new().with_url_guard()),
        LocalSessionManager::default().into(),
        mcp_config,
    );

    // Permissive CORS — an HTTP MCP endpoint is commonly called from a browser
    // extension or web client. Tighten it in front of a public deployment.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let router = axum::Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(cors);

    let addr = format!("{}:{}", host, port);
    let tcp_listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("=================================================");
    tracing::info!("Server ready");
    tracing::info!("  MCP:  http://{}/mcp", addr);
    tracing::info!("=================================================");

    axum::serve(tcp_listener, router).await?;

    Ok(())
}
