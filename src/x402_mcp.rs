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
//! # Known gap: settlement on tool failure
//!
//! `settle_after_execution` skips settlement only when the response carries a
//! non-2xx status (`paygate.rs`: `is_client_error() || is_server_error()`).
//! MCP reports tool failures as **HTTP 200** with `isError: true` in the
//! JSON-RPC body, so a failed transcription still settles and the caller is
//! billed for work that produced nothing. Closing this needs the response
//! body inspected before settlement, which the layer exposes no hook for
//! today. Tracked with the idempotency work.

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

/// Routes each MCP request to either the free or the paid service.
///
/// Both inner services are the same MCP service; `paid` is additionally
/// wrapped in the x402 layer.
#[derive(Clone)]
pub struct X402McpRouter<F, P> {
    free: F,
    paid: P,
}

impl<F, P> X402McpRouter<F, P> {
    pub fn new(free: F, paid: P) -> Self {
        Self { free, paid }
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
                    paid.call(request).await
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
