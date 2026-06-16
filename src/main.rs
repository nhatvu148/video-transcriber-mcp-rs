use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::{
    ServiceExt,
    transport::{stdio, streamable_http_server::StreamableHttpService},
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_governor::{
    GovernorLayer, governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor,
};
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
use video_transcriber_mcp::credits;

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

/// Sweep any `transcriber-upload-*` directories left behind by a previous
/// process (SIGKILL, OOM, machine replacement, etc.). The normal case is
/// handled by `TempDir`'s Drop in the upload handler — this is the
/// belt-and-braces backstop. Runs once at HTTP-transport startup; only
/// matters when `/api/jobs/upload` is reachable.
fn sweep_stale_uploads() {
    let temp = std::env::temp_dir();
    let entries = match std::fs::read_dir(&temp) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut count = 0;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("transcriber-upload-") {
            continue;
        }
        // Best-effort size measurement for the log line. If we can't read it,
        // just skip the size — the cleanup is what matters.
        if let Ok(meta) = entry.metadata() {
            bytes = bytes.saturating_add(meta.len());
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            count += 1;
        }
    }
    if count > 0 {
        tracing::info!(
            "Cleaned up {} stale upload dir(s) (~{} MB) from a previous process",
            count,
            bytes / 1024 / 1024
        );
    }
}

/// Run the MCP server with Streamable HTTP transport (for remote access)
async fn run_http_transport(host: &str, port: u16) -> Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

    // Run once at startup. New uploads land in tempfile-managed dirs whose
    // Drop cleans them up automatically; this sweep covers prior processes
    // that died without unwinding.
    sweep_stale_uploads();

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
        credits: credits::new_store(),
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
    // `SmartIpKeyExtractor` reads the standard proxy headers (X-Forwarded-For,
    // X-Real-IP, Forwarded) and falls back to the connection's peer IP — which
    // is what we want behind Fly's edge proxy, where the connecting IP is
    // always Fly's internal loopback. With the default `PeerIpKeyExtractor`
    // every real-world user would share a single bucket (and the extractor
    // would also 500 because `axum::serve(...)` doesn't inject `ConnectInfo`
    // unless we ask).
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(1)
            .burst_size(20)
            .key_extractor(SmartIpKeyExtractor)
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

    // `into_make_service_with_connect_info::<SocketAddr>()` is required for
    // tower_governor's fallback peer-IP extraction (the SmartIp extractor
    // still wants a peer address if X-Forwarded-For is missing).
    use std::net::SocketAddr;
    axum::serve(
        tcp_listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}
