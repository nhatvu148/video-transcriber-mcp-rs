//! End-to-end tests for the streamable-HTTP transport — the path Fly serves
//! and the browser extension actually talks to.
//!
//! The stdio suite (`mcp_e2e.rs`) covers handler behaviour; this one covers
//! everything stdio can't: session establishment via the `Mcp-Session-Id`
//! header, SSE framing, CORS preflight, and the REST surface mounted
//! alongside `/mcp`. Those are transport concerns that an rmcp upgrade can
//! break without touching a single handler.
//!
//! No external services are required: without `DATABASE_URL` the credit store
//! falls back to its on-disk JSON mode, and no test here calls a tool that
//! spends credits or transcribes anything.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const PROTOCOL: &str = "2025-06-18";
/// Clients must accept both, since the server picks SSE or JSON per request.
const ACCEPT: &str = "application/json, text/event-stream";

/// A live server subprocess serving streamable HTTP on a private port.
struct HttpServer {
    child: Child,
    base: String,
}

impl HttpServer {
    async fn start() -> Self {
        Self::start_with_env(&[]).await
    }

    /// Same, but with extra environment variables set on the child — used to
    /// exercise `MCP_ALLOWED_HOSTS`.
    async fn start_with_env(env: &[(&str, &str)]) -> Self {
        // Ask the OS for a free port, then hand it to the child. There is a
        // small race between releasing and rebinding, but it keeps tests
        // independent so they can run in parallel.
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("reserve a port")
            .local_addr()
            .expect("local addr")
            .port();

        let child = Command::new(env!("CARGO_BIN_EXE_video-transcriber-mcp"))
            .args(["--transport", "http", "--host", "127.0.0.1"])
            .args(["--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Force the file-backed credit store even if the developer has a
            // real DATABASE_URL exported: these tests must never touch a
            // production ledger.
            .env_remove("DATABASE_URL")
            .envs(env.iter().copied())
            .spawn()
            .expect("failed to spawn the MCP server binary");

        let server = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
        };
        server.wait_until_ready().await;
        server
    }

    /// Poll until the MCP endpoint answers, so tests never race startup.
    async fn wait_until_ready(&self) {
        let client = reqwest::Client::new();
        for _ in 0..100 {
            let responded = client
                .post(format!("{}/mcp", self.base))
                .header("content-type", "application/json")
                .header("accept", ACCEPT)
                .body("{}")
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok();
            if responded {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("server did not become ready within 10s");
    }

    /// Initialize a session and return `(session_id, initialize_result)`.
    async fn open_session(&self) -> (String, Value) {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json")
            .header("accept", ACCEPT)
            .body(
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": PROTOCOL,
                        "capabilities": {},
                        "clientInfo": {"name": "e2e", "version": "1"},
                    }
                })
                .to_string(),
            )
            .send()
            .await
            .expect("initialize request");

        assert!(
            response.status().is_success(),
            "initialize failed: {}",
            response.status()
        );
        let session = response
            .headers()
            .get("mcp-session-id")
            .expect("server must return an Mcp-Session-Id header")
            .to_str()
            .expect("session id is valid ASCII")
            .to_string();
        assert!(!session.is_empty(), "session id must not be empty");

        let result = extract_result(&response.text().await.expect("initialize body"), 1);

        // Complete the handshake so the session leaves the initializing state.
        client
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json")
            .header("accept", ACCEPT)
            .header("mcp-session-id", &session)
            .body(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())
            .send()
            .await
            .expect("initialized notification");

        (session, result)
    }

    async fn request(&self, session: &str, id: u64, method: &str, params: Value) -> Value {
        let body = reqwest::Client::new()
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json")
            .header("accept", ACCEPT)
            .header("mcp-session-id", session)
            .body(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{method} request failed: {e}"))
            .text()
            .await
            .expect("response body");
        extract_result(&body, id)
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pull the JSON-RPC result for `id` out of a response body, which may be
/// either a bare JSON object or an SSE stream of `data:` frames.
fn extract_result(body: &str, id: u64) -> Value {
    for line in body.lines() {
        let line = line.trim();
        let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if !payload.starts_with('{') {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if msg.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        assert!(
            msg.get("error").is_none(),
            "request id={id} returned an error: {msg}"
        );
        return msg["result"].clone();
    }
    panic!("no JSON-RPC result for id={id} in body: {body}");
}

#[tokio::test]
async fn establishes_a_session_and_lists_tools_over_http() {
    let server = HttpServer::start().await;
    let (session, init) = server.open_session().await;

    assert_eq!(init["protocolVersion"], PROTOCOL);
    assert!(init["capabilities"]["tools"].is_object());

    let result = server.request(&session, 2, "tools/list", json!({})).await;
    let tools = result["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 9, "advertised tool count changed: {result}");
    assert!(
        tools.iter().any(|t| t["name"] == "transcribe_video"),
        "transcribe_video missing from HTTP tools/list"
    );
}

#[tokio::test]
async fn calls_a_tool_over_http() {
    let server = HttpServer::start().await;
    let (session, _) = server.open_session().await;

    let result = server
        .request(
            &session,
            2,
            "tools/call",
            json!({"name": "list_supported_sites", "arguments": {}}),
        )
        .await;

    assert_eq!(result["isError"], json!(false));
    assert_eq!(result["content"][0]["type"], "text");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("Supported Video Platforms")
    );
}

#[tokio::test]
async fn rejects_requests_carrying_an_unknown_session_id() {
    let server = HttpServer::start().await;
    server.open_session().await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base))
        .header("content-type", "application/json")
        .header("accept", ACCEPT)
        .header("mcp-session-id", "00000000-0000-0000-0000-000000000000")
        .body(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}).to_string())
        .send()
        .await
        .expect("request with a bogus session");

    assert!(
        response.status().is_client_error(),
        "an unknown session id must be refused, got {}",
        response.status()
    );
}

/// The extension is browser code, so a broken preflight silently breaks the
/// whole product while every server-side test still passes.
#[tokio::test]
async fn serves_cors_headers_for_browser_clients() {
    let server = HttpServer::start().await;

    let response = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{}/api/jobs", server.base))
        .header("origin", "https://example.test")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type")
        .send()
        .await
        .expect("CORS preflight");

    assert!(
        response.status().is_success(),
        "preflight failed: {}",
        response.status()
    );
    assert!(
        response
            .headers()
            .contains_key("access-control-allow-origin"),
        "preflight response is missing access-control-allow-origin"
    );
}

/// `/mcp` is nested next to the REST router; a routing regression in an axum
/// or tower upgrade would take out one without touching the other.
#[tokio::test]
async fn mounts_the_rest_api_alongside_the_mcp_endpoint() {
    let server = HttpServer::start().await;

    // GET on a POST-only route: 405 proves the route exists and is wired,
    // without creating a job or spending credits.
    let response = reqwest::Client::new()
        .get(format!("{}/api/jobs", server.base))
        .send()
        .await
        .expect("GET /api/jobs");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED,
        "expected /api/jobs to exist as a POST-only route"
    );
}

/// rmcp refuses requests whose `Host` header isn't on its allowlist — DNS
/// rebinding protection that defaults to loopback only. A deployed instance
/// therefore 403s its own public hostname, which is what kept remote MCP
/// clients out of the Fly deployment (issue #14).
#[tokio::test]
async fn rejects_a_foreign_host_header_by_default() {
    let server = HttpServer::start().await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base))
        .header("content-type", "application/json")
        .header("accept", ACCEPT)
        // Reaches the server over loopback, but claims to be someone else —
        // exactly the shape of a DNS-rebinding attempt.
        .header("host", "evil.example.test")
        .body(initialize_body())
        .send()
        .await
        .expect("request with a foreign Host");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an unlisted Host must be refused"
    );
}

/// …and naming the host in MCP_ALLOWED_HOSTS lets it through, which is what
/// makes a deployed `/mcp` reachable.
#[tokio::test]
async fn accepts_a_host_named_in_mcp_allowed_hosts() {
    let server =
        HttpServer::start_with_env(&[("MCP_ALLOWED_HOSTS", "mcp.example.test,other.example.test")])
            .await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base))
        .header("content-type", "application/json")
        .header("accept", ACCEPT)
        .header("host", "mcp.example.test")
        .body(initialize_body())
        .send()
        .await
        .expect("request with an allowed Host");

    assert!(
        response.status().is_success(),
        "a configured Host must be accepted, got {}",
        response.status()
    );
    assert!(
        response.headers().contains_key("mcp-session-id"),
        "a successful initialize must still open a session"
    );
}

/// The allowlist is additive: configuring public hostnames must not lock out
/// loopback, or local development and health checks break.
#[tokio::test]
async fn still_accepts_loopback_when_hosts_are_configured() {
    let server =
        HttpServer::start_with_env(&[("MCP_ALLOWED_HOSTS", "mcp.example.test")]).await;

    // open_session() talks to 127.0.0.1 with the default Host header.
    let (session, init) = server.open_session().await;
    assert!(!session.is_empty());
    assert_eq!(init["protocolVersion"], PROTOCOL);
}

/// An unlisted host stays refused even once others are configured — the
/// protection must narrow, never widen to "anything goes".
#[tokio::test]
async fn configuring_hosts_does_not_allow_every_host() {
    let server =
        HttpServer::start_with_env(&[("MCP_ALLOWED_HOSTS", "mcp.example.test")]).await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base))
        .header("content-type", "application/json")
        .header("accept", ACCEPT)
        .header("host", "evil.example.test")
        .body(initialize_body())
        .send()
        .await
        .expect("request with an unlisted Host");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::FORBIDDEN,
        "hosts outside the configured list must still be refused"
    );
}

fn initialize_body() -> String {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "e2e", "version": "1"},
        }
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// x402 pay-per-call gating
//
// These need no wallet: they assert *which* requests are challenged, which is
// the decision this server owns. Whether a presented payment actually settles
// is the facilitator's job and is covered by x402-axum's own tests.
// ---------------------------------------------------------------------------

/// A Solana address used only to switch payments on in tests. Funds are never
/// moved — no request here presents a payment. (Solana's system program, which
/// is a real, valid pubkey nobody can spend from.)
const TEST_PAY_TO: &str = "11111111111111111111111111111111";

fn paid_server_env() -> Vec<(&'static str, &'static str)> {
    vec![("X402_PAY_TO", TEST_PAY_TO)]
}

/// Discovery must stay free even with payments on. This is the whole reason
/// the routing layer exists: a blanket x402 layer would 402 the handshake, and
/// an agent that can't read the catalogue can never decide to buy from it.
#[tokio::test]
async fn discovery_is_free_when_payments_are_enabled() {
    let server = HttpServer::start_with_env(&paid_server_env()).await;

    // initialize must succeed and open a session…
    let (session, init) = server.open_session().await;
    assert_eq!(init["protocolVersion"], PROTOCOL);

    // …and tools/list must return the catalogue, not a 402.
    let result = server.request(&session, 2, "tools/list", json!({})).await;
    assert_eq!(
        result["tools"].as_array().expect("tools").len(),
        9,
        "tools/list must be free and complete when payments are on"
    );
}

/// Free tools stay callable with payments on — only the expensive one is gated.
#[tokio::test]
async fn free_tools_are_callable_when_payments_are_enabled() {
    let server = HttpServer::start_with_env(&paid_server_env()).await;
    let (session, _) = server.open_session().await;

    let result = server
        .request(
            &session,
            2,
            "tools/call",
            json!({"name": "list_supported_sites", "arguments": {}}),
        )
        .await;
    assert_eq!(result["isError"], json!(false));
}

/// The point of the feature: calling the priced tool without payment must be
/// challenged with 402 — and specifically 402, not 400. Clients treat 402 as
/// "retry with payment"; 400 is terminal and would strand a paying agent.
#[tokio::test]
async fn priced_tool_is_challenged_with_402_when_unpaid() {
    let server = HttpServer::start_with_env(&paid_server_env()).await;
    let (session, _) = server.open_session().await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base))
        .header("content-type", "application/json")
        .header("accept", ACCEPT)
        .header("mcp-session-id", &session)
        .body(
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "transcribe_video",
                           "arguments": {"url": "https://example.test/video.mp4"}}
            })
            .to_string(),
        )
        .send()
        .await
        .expect("unpaid call to a priced tool");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::PAYMENT_REQUIRED,
        "an unpaid priced tool call must return 402, got {}",
        response.status()
    );

    // The challenge has to tell the client what to pay, or it can't retry.
    let body = response.text().await.expect("402 body");
    let challenge: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("402 body must be JSON ({e}): {body}"));
    let accepts = challenge["accepts"]
        .as_array()
        .unwrap_or_else(|| panic!("402 must carry an `accepts` array: {challenge}"));
    assert!(!accepts.is_empty(), "`accepts` must not be empty");
    assert_eq!(
        accepts[0]["payTo"].as_str(),
        Some(TEST_PAY_TO),
        "the challenge must name our receiving address"
    );
}

/// With payments off (the default), the priced tool must NOT be challenged —
/// otherwise enabling the feature would be impossible to opt out of, and every
/// existing deployment would break.
#[tokio::test]
async fn priced_tool_is_not_challenged_when_payments_are_disabled() {
    let server = HttpServer::start().await; // no X402_PAY_TO
    let (session, _) = server.open_session().await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base))
        .header("content-type", "application/json")
        .header("accept", ACCEPT)
        .header("mcp-session-id", &session)
        .body(
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "transcribe_video", "arguments": {}}
            })
            .to_string(),
        )
        .send()
        .await
        .expect("call with payments disabled");

    assert_ne!(
        response.status(),
        reqwest::StatusCode::PAYMENT_REQUIRED,
        "payments are off; nothing should be challenged"
    );
}

/// Agents pick tools partly on cost, so a priced tool must say so in the
/// catalogue — otherwise the first a caller knows of the charge is a 402.
#[tokio::test]
async fn priced_tool_advertises_its_price_in_the_catalogue() {
    let server = HttpServer::start_with_env(&paid_server_env()).await;
    let (session, _) = server.open_session().await;

    let result = server.request(&session, 2, "tools/list", json!({})).await;
    let tools = result["tools"].as_array().expect("tools");
    let transcribe = tools
        .iter()
        .find(|t| t["name"] == "transcribe_video")
        .expect("transcribe_video");
    let description = transcribe["description"].as_str().expect("description");

    assert!(
        description.contains("0.20") && description.contains("402"),
        "priced tool must advertise cost and the 402 flow: {description}"
    );

    // Free tools must not claim a price.
    let free = tools
        .iter()
        .find(|t| t["name"] == "list_transcripts")
        .expect("list_transcripts");
    assert!(
        !free["description"].as_str().unwrap().contains("COST"),
        "free tools must not advertise a price"
    );
}

/// …and with payments off, nothing mentions cost at all.
#[tokio::test]
async fn no_price_is_advertised_when_payments_are_disabled() {
    let server = HttpServer::start().await;
    let (session, _) = server.open_session().await;

    let result = server.request(&session, 2, "tools/list", json!({})).await;
    let transcribe = result["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == "transcribe_video")
        .expect("transcribe_video");

    assert!(
        !transcribe["description"].as_str().unwrap().contains("COST"),
        "must not advertise a price when payments are off"
    );
}
