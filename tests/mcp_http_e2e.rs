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
    ///
    /// Reserving a port and then handing it to the child leaves a window in
    /// which another test's child binds it first. The failure that causes is
    /// worse than a timeout: our child exits with "Address already in use",
    /// but the port still *answers* — the other test's server is on it — so a
    /// readiness poll that only asks "does this port respond?" succeeds, and
    /// the test proceeds to talk to a server configured for a different test.
    /// That surfaced as unrelated assertion failures in roughly 1 run in 5.
    ///
    /// So readiness is judged on our own child still being alive, and a lost
    /// race retries with a fresh port instead of continuing against a stranger.
    async fn start_with_env(env: &[(&str, &str)]) -> Self {
        const ATTEMPTS: usize = 5;

        for attempt in 1..=ATTEMPTS {
            let port = TcpListener::bind("127.0.0.1:0")
                .expect("reserve a port")
                .local_addr()
                .expect("local addr")
                .port();

            let mut child = Command::new(env!("CARGO_BIN_EXE_video-transcriber-mcp"))
                .args(["--transport", "http", "--host", "127.0.0.1"])
                .args(["--port", &port.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // Force the file-backed credit store even if the developer has
                // a real DATABASE_URL exported: these tests must never touch a
                // production ledger.
                .env_remove("DATABASE_URL")
                .envs(env.iter().copied())
                .spawn()
                .expect("failed to spawn the MCP server binary");

            let base = format!("http://127.0.0.1:{port}");
            if Self::await_ready(&mut child, &base).await {
                return Self { child, base };
            }

            let _ = child.kill();
            let _ = child.wait();
            assert!(
                attempt < ATTEMPTS,
                "server never became ready in {ATTEMPTS} attempts (port contention?)"
            );
        }
        unreachable!("loop either returns or asserts on the last attempt")
    }

    /// True once *our* child is serving. Returns false if it exited — which is
    /// what losing the port race looks like — so the caller can retry rather
    /// than bind onto whatever else happens to be listening.
    async fn await_ready(child: &mut Child, base: &str) -> bool {
        let client = reqwest::Client::new();
        for _ in 0..100 {
            // Checked first: if the child is gone, a responding port is
            // somebody else's server, and treating that as ready is exactly
            // the bug this guards against.
            if let Ok(Some(_status)) = child.try_wait() {
                return false;
            }
            let responded = client
                .post(format!("{base}/mcp"))
                .header("content-type", "application/json")
                .header("accept", ACCEPT)
                .body("{}")
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok();
            if responded {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
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
