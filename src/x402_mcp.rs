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
//! # Status: routing only, not yet wired up
//!
//! The free/paid decision below is complete and tested. What is missing is
//! constructing the x402 layer itself from configuration and mounting this
//! router on `/mcp` in `main.rs` — see the PR for exactly what remains. Until
//! that lands nothing here is reachable, hence the `allow(dead_code)`.
#![allow(dead_code)]

use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::Request;
use tower::Service;



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
    if json.get("method")?.as_str()? != "tools/call" {
        return None;
    }
    let tool = json.get("params")?.get("name")?.as_str()?;
    priced_tool_entry(tool)
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
                Err(_) => return free.call(Request::from_parts(parts, Body::empty())).await,
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
