use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService},
    },
};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tower_governor::{
    GovernorLayer, governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::Level;

mod api;
mod auth;
mod llm;
mod mcp;
mod transcriber;
mod utils;

use api::AppState;
use mcp::VideoTranscriberServer;
use transcriber::TranscriberEngine;
use video_transcriber_mcp::{credits, x402_mcp};

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

    // The streamable-HTTP transport only answers for hosts on this list —
    // rmcp's DNS-rebinding protection, which defaults to loopback. That means
    // a deployed instance 403s requests carrying its own public hostname, so
    // no remote MCP client can reach `/mcp` (issue #14).
    //
    // `MCP_ALLOWED_HOSTS` (comma-separated) names the hostnames this
    // deployment should answer for. Additive on top of loopback so local dev
    // and health checks keep working, and opt-in with no wildcard — the
    // rebinding protection stays on unless an operator says which hosts to
    // expect.
    // Start from rmcp's own defaults and extend them, rather than re-declaring
    // the loopback list here — that way an rmcp upgrade that changes the
    // built-in defaults carries through instead of silently diverging.
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

    // MCP service (per-session VideoTranscriberServer)
    let mcp_service = StreamableHttpService::new(
        || Ok(VideoTranscriberServer::new()),
        LocalSessionManager::default().into(),
        mcp_config,
    );

    // Wrap in the x402 pay-per-call router when payments are configured.
    // Both arms become a Router so the two service types unify.
    // rmcp responds with BoxBody; axum and the x402 layer want axum::body::Body.
    let mcp_service = tower::Layer::layer(
        &tower_http::map_response_body::MapResponseBodyLayer::new(axum::body::Body::new),
        mcp_service,
    );
    let mcp_router: axum::Router = match x402_mcp::layer_from_env() {
        Some(layer) => {
            // McpFailureStatus sits *under* the payment layer so a failed
            // tool call reads as non-2xx there and settlement is skipped.
            let paid = tower::Layer::layer(
                &layer,
                x402_mcp::McpFailureStatus::new(mcp_service.clone()),
            );
            axum::Router::new()
                .fallback_service(x402_mcp::X402McpRouter::new(mcp_service, paid))
        }
        None => {
            tracing::info!("MCP payments OFF — set X402_PAY_TO to charge for priced tools");
            axum::Router::new().fallback_service(mcp_service)
        }
    };

    // Supabase JWKS cache for verifying user auth tokens. Falls back to a
    // placeholder URL if SUPABASE_URL isn't configured — the cache will
    // simply fail to fetch and every auth-requiring endpoint will 401,
    // which is the correct behavior for a misconfigured deployment.
    let supabase_url = std::env::var("SUPABASE_URL").unwrap_or_else(|_| {
        tracing::warn!(
            "SUPABASE_URL is not set — /api/me and other auth-required endpoints will reject all requests"
        );
        "https://invalid.supabase.invalid".to_string()
    });
    let jwks = auth::JwksCache::new(&supabase_url);

    // Cap concurrent transcription pipelines so a traffic spike queues instead
    // of overwhelming the machine — each job spawns yt-dlp + ffmpeg locally, so
    // unbounded concurrency exhausts CPU/RAM. Excess jobs stay Queued until a
    // slot frees. Tune via MAX_CONCURRENT_JOBS (default 4).
    let max_concurrent = std::env::var("MAX_CONCURRENT_JOBS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4);
    tracing::info!("Max concurrent transcription jobs: {max_concurrent}");

    // REST API state shared across all jobs
    let app_state = AppState {
        jobs: api::new_store(),
        engine: Arc::new(Mutex::new(TranscriberEngine::new())),
        credits: credits::new_store().await,
        jwks,
        pipeline_permits: Arc::new(Semaphore::new(max_concurrent)),
    };

    // Evict old finished jobs from the in-memory store so long-lived instances
    // don't leak memory as completed jobs accumulate.
    api::handlers::spawn_job_gc(app_state.jobs.clone());

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
        .nest_service("/mcp", mcp_router)
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
