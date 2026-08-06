//! Pay-per-call gating for the MCP endpoint (x402).
//!
//! # Why this exists rather than just `.layer(x402)`
//!
//! MCP puts every JSON-RPC method behind a single POST route. `x402-axum`
//! selects prices from `(headers, uri, base_url)` only — its `PriceTagSource`
//! never sees the body and, by contract, "must always return a non-empty
//! vector of price tags". Layering it directly on `/mcp` would therefore
//! charge for `initialize` and `tools/list`, which every client calls on
//! connect. An agent that cannot read the catalogue can never decide to buy
//! from it, so that breaks clients before they can pay.
//!
//! The established practice for paid MCP servers is *free discovery, paid
//! execution*: `initialize`, `tools/list`, `resources/list`, `prompts/list`
//! and notifications stay free; only `tools/call` for a priced tool is gated.
//!
//! # Design
//!
//! This module deliberately does **not** reimplement x402. It builds two
//! services from the same MCP service — one plain, one wrapped in the
//! `x402-axum` layer — and routes each request to whichever is appropriate
//! after peeking at the JSON-RPC body. All protocol handling (402 challenge
//! shape, facilitator verify/settle, payload conformance) stays inside the
//! maintained crate, where it belongs; the only judgement here is *which*
//! requests need to pay.
//!
//! Payment is not authorization. Settling a call buys that one call and
//! nothing else.
//!
//! # Not billing for failed work
//!
//! `settle_after_execution` skips settlement only when the response carries a
//! non-2xx status (`paygate.rs`: `is_client_error() || is_server_error()`),
//! but MCP reports tool failures as **HTTP 200** with `isError: true` in the
//! body. Left alone, a failed transcription settles and the caller pays for
//! nothing.
//!
//! Since x402 decides on status alone, [`McpFailureStatus`] sits *between* the
//! payment layer and the MCP service and re-stamps a failed tool result as
//! 502, which makes x402 skip settlement. The router then restores the
//! original status on the way out, so the client still sees the ordinary
//! JSON-RPC error it expects. The response body is untouched.
//!
//! # Not billing twice for one payment
//!
//! A client that retries an in-flight or completed call — a dropped
//! connection, an impatient agent — would otherwise run the work again and
//! settle again. [`NonceGuard`] keys execution on the payment nonce, so a
//! replayed payment is refused rather than charged twice.

use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::Request;
use tower::Service;

use std::sync::Arc;

use alloy_primitives::Address;
use x402_axum::facilitator_client::FacilitatorClient;
use x402_axum::{StaticPriceTags, X402LayerBuilder, X402Middleware};
// KnownNetworkEip155 is what puts `USDC::base_sepolia()` in scope.
use x402_chain_eip155::{KnownNetworkEip155, V1Eip155Exact};
use x402_types::networks::USDC;



/// Tools that cost money to serve, with their price in whole USD.
///
/// Only `transcribe_video` does real work — it spends GPU time on Modal and
/// tokens on OpenRouter. Everything else reads local state and is free, both
/// because it costs nothing to serve and because a server you cannot explore
/// without paying is one agents will skip.
///
/// Priced at the same rate as the smallest credit pack (25 credits for $5) so
/// the MCP path can't be used to undercut the Stripe packs.
pub const PRICED_TOOLS: &[(&str, &str)] = &[("transcribe_video", "0.20")];

/// Look up a tool's price, if it has one.
pub fn price_for_tool(tool: &str) -> Option<&'static str> {
    priced_tool_entry(tool).map(|(_, price)| price)
}

/// The `(tool, price)` entry for a priced tool. Both borrow from the table
/// rather than the caller's input, so they outlive the parsed request body.
fn priced_tool_entry(tool: &str) -> Option<(&'static str, &'static str)> {
    PRICED_TOOLS
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(name, price)| (*name, *price))
}

/// Decide whether a request body must pay before it runs.
///
/// Returns `(tool, price)` when the request is a `tools/call` for a priced
/// tool. Anything else — discovery, notifications, free tools, or a body we
/// can't parse — is free. Failing open to *free* is deliberate: an
/// unparseable request can't do any billable work either, and the MCP layer
/// below will reject it with a proper JSON-RPC error. Failing open to *paid*
/// would let a malformed body extract a 402 for work that never happens.
pub fn priced_tool_in_body(body: &[u8]) -> Option<(&'static str, &'static str)> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;

    // A JSON-RPC batch is an array, and `Value::get("method")` on an array is
    // None — so without this, a batch wrapping a priced call would read as
    // free. rmcp 3.1 rejects batches at the transport (415) so it is not
    // currently reachable, but a money path must not depend on a downstream
    // layer's present behaviour: MCP allowed batching until 2025-11-25, and
    // an rmcp change re-enabling it would silently open a bypass. Charge if
    // any element is priced.
    if let Some(batch) = json.as_array() {
        return batch
            .iter()
            .find_map(|entry| priced_tool_in_value(entry));
    }

    priced_tool_in_value(&json)
}

/// The single-message case, shared with each element of a batch.
fn priced_tool_in_value(json: &serde_json::Value) -> Option<(&'static str, &'static str)> {
    if json.get("method")?.as_str()? != "tools/call" {
        return None;
    }
    let tool = json.get("params")?.get("name")?.as_str()?;
    priced_tool_entry(tool)
}



/// Default price, matching the smallest credit pack's per-transcription rate.
pub const DEFAULT_PRICE_USD: &str = "0.20";
/// Public testnet facilitator.
pub const DEFAULT_FACILITATOR: &str = "https://facilitator.x402.rs";

/// The concrete layer type produced by [`layer_from_env`].
pub type PaidLayer =
    X402LayerBuilder<StaticPriceTags<x402_types::proto::v1::PriceTag>, Arc<FacilitatorClient>>;

/// Validated payment settings, or `None` when this deployment doesn't charge.
///
/// The single source of truth for "are we charging, and how much". Both the
/// payment layer and the tool catalogue derive from it, so the catalogue can't
/// advertise a price the gate doesn't actually enforce — which is exactly what
/// happened when each re-read the raw environment independently: an
/// unparseable `X402_PAY_TO` left payments off while the description still
/// claimed a price.
pub struct PaymentSettings {
    pub pay_to: Address,
    pub price: String,
    pub mainnet: bool,
    pub facilitator: String,
}

impl PaymentSettings {
    /// Human-readable network name, for the catalogue and logs.
    pub fn network(&self) -> &'static str {
        if self.mainnet { "base" } else { "base-sepolia" }
    }
}

/// Reads and *validates* payment settings. `None` means payments are off —
/// either unconfigured, or configured wrongly, which is logged and treated as
/// off rather than allowed to half-enable the feature.
pub fn payment_settings() -> Option<PaymentSettings> {
    let pay_to = std::env::var("X402_PAY_TO").ok().filter(|v| !v.trim().is_empty())?;
    let pay_to: Address = match pay_to.trim().parse() {
        Ok(address) => address,
        Err(e) => {
            tracing::error!("X402_PAY_TO is not a valid address ({e}) — MCP payments stay disabled");
            return None;
        }
    };
    let price = std::env::var("X402_PRICE_USD").unwrap_or_else(|_| DEFAULT_PRICE_USD.to_string());
    let mainnet = std::env::var("X402_NETWORK").map(|n| n.trim() == "base").unwrap_or(false);
    let facilitator =
        std::env::var("X402_FACILITATOR").unwrap_or_else(|_| DEFAULT_FACILITATOR.to_string());

    // Reject an unusable price here too, so the catalogue and the gate agree.
    let usdc = if mainnet { USDC::base() } else { USDC::base_sepolia() };
    if let Err(e) = usdc.parse(price.as_str()) {
        tracing::error!("X402_PRICE_USD `{price}` is invalid ({e}) — MCP payments stay disabled");
        return None;
    }

    Some(PaymentSettings { pay_to, price, mainnet, facilitator })
}

/// Builds the x402 layer from the environment, or `None` when payment is off.
///
/// Disabled unless `X402_PAY_TO` is set, so existing deployments are
/// unaffected until an operator opts in. Testnet is the default: reaching real
/// money takes an explicit `X402_NETWORK=base`, so a misconfiguration fails
/// toward play money rather than toward charging someone.
pub fn layer_from_env() -> Option<PaidLayer> {
    let settings = payment_settings()?;
    let usdc = if settings.mainnet { USDC::base() } else { USDC::base_sepolia() };
    // Already validated by payment_settings().
    let amount = usdc.parse(settings.price.as_str()).ok()?;

    tracing::info!(
        "MCP payments ON: ${} per priced tool call on {}, paid to {}",
        settings.price,
        if settings.mainnet { "Base MAINNET (real funds)" } else { "Base Sepolia (testnet)" },
        settings.pay_to
    );

    Some(
        X402Middleware::new(settings.facilitator.as_str())
            // Verify up front, settle after the work — but see the module-level
            // note: an MCP tool that fails still returns 200, so this does not
            // currently spare the caller for a failed job.
            .settle_after_execution()
            .with_price_tag(V1Eip155Exact::price_tag(settings.pay_to, amount)),
    )
}


// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

/// Remembers which payment nonces have already been admitted.
///
/// x402 nonces are single-use, so a nonce arriving twice means a retry of the
/// same logical call. Without this, a client whose connection dropped mid
/// transcription would run — and settle — the work a second time.
///
/// In-memory and per-process: a restart or a second machine forgets. That is
/// enough for a single-instance deployment and is stated plainly rather than
/// implied; a horizontally scaled deployment needs this in Postgres beside the
/// credit ledger.
#[derive(Clone, Default)]
pub struct NonceGuard {
    seen: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
}

/// How long a nonce is remembered. Comfortably longer than a transcription,
/// and bounded so the map can't grow without limit.
const NONCE_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

impl NonceGuard {
    /// Claims a nonce. `true` means it's new and the call may proceed;
    /// `false` means it's a replay.
    ///
    /// Claim-on-check rather than check-then-act, so two concurrent retries
    /// can't both pass — the same race the credit ledger guards against.
    pub fn claim(&self, nonce: &str) -> bool {
        let now = std::time::Instant::now();
        let mut seen = match self.seen.lock() {
            Ok(seen) => seen,
            // A poisoned lock must not become a free pass to double-charge.
            Err(poisoned) => poisoned.into_inner(),
        };
        seen.retain(|_, claimed_at| now.duration_since(*claimed_at) < NONCE_TTL);
        if seen.contains_key(nonce) {
            return false;
        }
        seen.insert(nonce.to_string(), now);
        true
    }
}

/// Pulls the payment nonce out of an `X-PAYMENT` header.
///
/// The header is base64 JSON; the nonce lives in the scheme-specific payload,
/// so this searches rather than assuming one shape — the field has moved
/// between protocol versions. `None` when there's no usable nonce, in which
/// case the call proceeds: refusing every payment we can't parse would break
/// paying clients, and x402 itself still rejects a genuinely replayed payment
/// on-chain.
pub fn payment_nonce(header: &str) -> Option<String> {
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(header.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(header.trim()))
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    find_nonce(&json)
}

fn find_nonce(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(nonce) = map.get("nonce").and_then(|n| n.as_str()) {
                return Some(nonce.to_string());
            }
            map.values().find_map(find_nonce)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_nonce),
        _ => None,
    }
}

/// Header used to carry a tool failure past the payment layer.
const TOOL_FAILED_HEADER: &str = "x-mcp-tool-failed";

/// Re-stamps an MCP tool failure as 502 so the payment layer above skips
/// settlement. The router restores the real status afterwards.
#[derive(Clone)]
pub struct McpFailureStatus<S> {
    inner: S,
}

impl<S> McpFailureStatus<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> Service<Request<Body>> for McpFailureStatus<S>
where
    S: Service<Request<Body>, Response = axum::response::Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        std::mem::swap(&mut inner, &mut self.inner);
        Box::pin(async move {
            let response = inner.call(request).await?;
            if !response.status().is_success() {
                return Ok(response); // already a failure; nothing to signal
            }

            // Only ever applied on the paid path, which is a single
            // request/response tool call — so buffering here doesn't stall a
            // long-lived stream.
            let (mut parts, body) = response.into_parts();
            let bytes = match axum::body::to_bytes(body, 8 * 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(_) => return Ok(axum::response::Response::from_parts(parts, Body::empty())),
            };

            if body_reports_tool_failure(&bytes) {
                let original = parts.status;
                parts.status = axum::http::StatusCode::BAD_GATEWAY;
                if let Ok(value) = axum::http::HeaderValue::from_str(original.as_str()) {
                    parts.headers.insert(TOOL_FAILED_HEADER, value);
                }
            }
            Ok(axum::response::Response::from_parts(parts, Body::from(bytes)))
        })
    }
}

/// Whether an MCP response reports a failed tool call.
///
/// Matches on the serialized body rather than parsing, because the same
/// payload arrives either as bare JSON or wrapped in SSE `data:` frames
/// depending on what the client negotiated. `isError` appears only in a tool
/// result, so the match is specific enough.
pub fn body_reports_tool_failure(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    text.contains("\"isError\":true") || text.contains("\"isError\": true")
}

/// Routes each MCP request to either the free or the paid service.
///
/// Both inner services are the same MCP service; `paid` is additionally
/// wrapped in the x402 layer.
#[derive(Clone)]
pub struct X402McpRouter<F, P> {
    free: F,
    paid: P,
    nonces: NonceGuard,
}

impl<F, P> X402McpRouter<F, P> {
    pub fn new(free: F, paid: P) -> Self {
        Self { free, paid, nonces: NonceGuard::default() }
    }
}

impl<F, P> Service<Request<Body>> for X402McpRouter<F, P>
where
    F: Service<Request<Body>, Response = axum::response::Response> + Clone + Send + 'static,
    F::Future: Send + 'static,
    F::Error: Send + 'static,
    P: Service<Request<Body>, Response = axum::response::Response, Error = F::Error>
        + Clone
        + Send
        + 'static,
    P::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = F::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Both branches must be ready, since either may be chosen.
        match (self.free.poll_ready(cx), self.paid.poll_ready(cx)) {
            (Poll::Ready(Ok(())), Poll::Ready(Ok(()))) => Poll::Ready(Ok(())),
            (Poll::Ready(Err(e)), _) | (_, Poll::Ready(Err(e))) => Poll::Ready(Err(e)),
            _ => Poll::Pending,
        }
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        // `self` may not be the clone that gets polled, so follow the tower
        // convention of moving the ready clone into the future.
        let mut free = self.free.clone();
        let mut paid = self.paid.clone();
        std::mem::swap(&mut free, &mut self.free);
        std::mem::swap(&mut paid, &mut self.paid);
        let nonces = self.nonces.clone();

        Box::pin(async move {
            // Only POST carries JSON-RPC. GET (SSE stream) and DELETE
            // (session teardown) are transport plumbing and always free.
            if request.method() != axum::http::Method::POST {
                return free.call(request).await;
            }

            let (parts, body) = request.into_parts();
            // Bounded so a huge body can't be used to exhaust memory here;
            // the MCP layer enforces its own limit below this one.
            let bytes = match axum::body::to_bytes(body, 2 * 1024 * 1024).await {
                Ok(bytes) => bytes,
                // Unreadable body: hand it down and let MCP produce the error.
                // Substituting an empty body would surface as a confusing
                // downstream parse error; say what actually happened.
                Err(_) => {
                    // JSON-RPC shaped, like every other error an MCP client
                    // sees from this endpoint.
                    return Ok(axum::response::IntoResponse::into_response((
                        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                        axum::Json(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": serde_json::Value::Null,
                            "error": {
                                "code": -32600,
                                "message": "request body too large for the MCP endpoint"
                            }
                        })),
                    )));
                }
            };

            let priced = priced_tool_in_body(&bytes);
            let request = Request::from_parts(parts, Body::from(bytes));

            match priced {
                Some((tool, price)) => {
                    tracing::debug!("x402: gating tools/call for `{tool}` (${price})");

                    // Refuse a replayed payment rather than doing — and
                    // settling — the same work twice.
                    if let Some(nonce) = request
                        .headers()
                        .get("x-payment")
                        .and_then(|v| v.to_str().ok())
                        .and_then(payment_nonce)
                        && !nonces.claim(&nonce)
                    {
                        tracing::warn!("x402: refusing replayed payment nonce for `{tool}`");
                        return Ok(axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": serde_json::Value::Null,
                                "error": {
                                    "code": -32600,
                                    "message": "this payment has already been used; \
                                                retry with a fresh payment"
                                }
                            })),
                        )));
                    }

                    let mut response = paid.call(request).await?;
                    // Undo the 502 that McpFailureStatus used to stop
                    // settlement, so the client sees the JSON-RPC error it
                    // expects rather than a transport failure.
                    if let Some(original) = response
                        .headers_mut()
                        .remove(TOOL_FAILED_HEADER)
                        .and_then(|v| v.to_str().ok().and_then(|s| s.parse::<u16>().ok()))
                        && let Ok(status) = axum::http::StatusCode::from_u16(original)
                    {
                        *response.status_mut() = status;
                    }
                    Ok(response)
                }
                None => free.call(request).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn transcribe_video_is_priced() {
        assert_eq!(price_for_tool("transcribe_video"), Some("0.20"));
    }

    #[test]
    fn read_only_tools_are_free() {
        for tool in [
            "check_dependencies",
            "list_supported_sites",
            "list_transcripts",
            "search_transcripts",
            "get_latest_transcript",
            "delete_transcript",
            "cleanup_old_transcripts",
            "delete_all_transcripts",
        ] {
            assert_eq!(price_for_tool(tool), None, "{tool} should be free");
        }
    }

    /// The whole reason this module exists: discovery must not cost anything,
    /// or a client can never learn what it would be paying for.
    #[test]
    fn discovery_methods_are_never_priced() {
        for method in [
            "initialize",
            "tools/list",
            "resources/list",
            "prompts/list",
            "ping",
            "notifications/initialized",
        ] {
            let request = body(json!({"jsonrpc": "2.0", "id": 1, "method": method}));
            assert_eq!(
                priced_tool_in_body(&request),
                None,
                "{method} must be free"
            );
        }
    }

    #[test]
    fn a_priced_tool_call_is_gated() {
        let request = body(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "transcribe_video", "arguments": {"url": "https://example.test/v"}}
        }));
        assert_eq!(
            priced_tool_in_body(&request),
            Some(("transcribe_video", "0.20"))
        );
    }

    #[test]
    fn a_free_tool_call_is_not_gated() {
        let request = body(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "list_transcripts", "arguments": {}}
        }));
        assert_eq!(priced_tool_in_body(&request), None);
    }

    /// Unparseable or odd bodies fail open to *free*. They can't do billable
    /// work, and charging for them would mean issuing a 402 for a request
    /// that was never going to run.
    #[test]
    fn malformed_bodies_are_free_not_paid() {
        for bad in [
            b"".as_slice(),
            b"not json",
            b"{",
            b"[]",
            b"null",
            br#"{"method": "tools/call"}"#,                       // no params
            br#"{"method": "tools/call", "params": {}}"#,         // no name
            br#"{"method": "tools/call", "params": {"name": 7}}"#, // name not a string
            br#"{"params": {"name": "transcribe_video"}}"#,       // no method
        ] {
            assert_eq!(
                priced_tool_in_body(bad),
                None,
                "malformed body must not be gated: {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// A tool we don't price must not be gated just because it's a tools/call.
    #[test]
    fn unknown_tools_are_not_gated() {
        let request = body(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "no_such_tool", "arguments": {}}
        }));
        assert_eq!(priced_tool_in_body(&request), None);
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use serde_json::json;

    /// A batch wrapping a priced call must be charged, not waved through.
    /// rmcp rejects batches today, so this guards against a future change
    /// re-opening the bypass rather than a live hole.
    #[test]
    fn a_batch_containing_a_priced_call_is_gated() {
        let batch = serde_json::to_vec(&json!([
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
             "params": {"name": "transcribe_video", "arguments": {}}}
        ]))
        .unwrap();
        assert_eq!(
            priced_tool_in_body(&batch),
            Some(("transcribe_video", "0.20"))
        );
    }

    #[test]
    fn a_batch_of_only_free_calls_is_free() {
        let batch = serde_json::to_vec(&json!([
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
             "params": {"name": "list_transcripts", "arguments": {}}}
        ]))
        .unwrap();
        assert_eq!(priced_tool_in_body(&batch), None);
    }

    #[test]
    fn an_empty_batch_is_free() {
        assert_eq!(priced_tool_in_body(b"[]"), None);
    }
}

#[cfg(test)]
mod gap_tests {
    use super::*;
    use base64::Engine as _;

    fn header_for(payload: serde_json::Value) -> String {
        base64::engine::general_purpose::STANDARD.encode(payload.to_string())
    }

    #[test]
    fn a_nonce_is_admitted_once_and_refused_after() {
        let guard = NonceGuard::default();
        assert!(guard.claim("abc"), "first use must be admitted");
        assert!(!guard.claim("abc"), "replay must be refused");
        assert!(guard.claim("def"), "a different nonce is unaffected");
    }

    /// Concurrent retries must not both slip through — the same race the
    /// credit ledger guards against, and the one that would double-charge.
    #[test]
    fn concurrent_claims_admit_exactly_one() {
        let guard = NonceGuard::default();
        let admitted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..32 {
                let guard = guard.clone();
                let admitted = admitted.clone();
                scope.spawn(move || {
                    if guard.claim("same-nonce") {
                        admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(
            admitted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one of 32 concurrent claims may be admitted"
        );
    }

    #[test]
    fn nonce_is_extracted_from_a_payment_header() {
        let header = header_for(serde_json::json!({
            "x402Version": 1,
            "payload": {"authorization": {"nonce": "0xdeadbeef", "from": "0xabc"}}
        }));
        assert_eq!(payment_nonce(&header).as_deref(), Some("0xdeadbeef"));
    }

    /// Unparseable headers yield no nonce, and the caller proceeds — refusing
    /// every payment we can't read would break paying clients, and x402 still
    /// rejects a genuinely replayed payment on-chain.
    #[test]
    fn unreadable_payment_headers_yield_no_nonce() {
        for bad in ["", "not-base64!!", &header_for(serde_json::json!({"no": "nonce"}))] {
            assert_eq!(payment_nonce(bad), None, "should not find a nonce in {bad:?}");
        }
    }

    #[test]
    fn tool_failure_is_detected_in_json_and_sse_framing() {
        let failed = br#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[]}}"#;
        assert!(body_reports_tool_failure(failed));

        let sse = b"event: message\ndata: {\"result\":{\"isError\": true}}\n\n";
        assert!(body_reports_tool_failure(sse), "SSE framing must be detected too");
    }

    #[test]
    fn successful_results_are_not_mistaken_for_failures() {
        let ok = br#"{"jsonrpc":"2.0","id":1,"result":{"isError":false,"content":[]}}"#;
        assert!(!body_reports_tool_failure(ok));
        assert!(!body_reports_tool_failure(br#"{"result":{"tools":[]}}"#));
    }
}
