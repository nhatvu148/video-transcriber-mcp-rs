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
//! # Failed work, and why settlement happens first
//!
//! Settlement runs **before** execution. A payment is a signed Solana
//! transaction whose blockhash dies after ~60-90s, while x402 advertises
//! `maxTimeoutSeconds: 300` — the protocol promises more time than the chain
//! allows. Settling afterwards therefore only worked for jobs shorter than the
//! blockhash window: measured against a live facilitator, a 6s clip settled
//! and a 582s video failed `transaction_simulation` and was transcribed for
//! free, with nothing distinguishing the two. Long lectures are the product,
//! so settle-after was unusable.
//!
//! That means a failed job has already been charged, and the older mechanism —
//! withholding settlement — is no longer available. [`McpFailureStatus`]
//! instead sits *between* the payment layer and the MCP service, detects the
//! failure, and records a credit for the payer.
//!
//! Detection matters because MCP reports tool failures as **HTTP 200** with
//! `isError: true` in the body, so status alone never revealed them. "Failed"
//! covers both shapes MCP uses: an in-band `result.isError`, and a top-level
//! JSON-RPC `error` — which is what this server's handlers actually return.
//! The 502 re-stamp is retained and the router restores the original status on
//! the way out, so the client still sees the ordinary JSON-RPC error it
//! expects. The response body is untouched.
//!
//! **The recorded credit is not yet spendable** — see [`compensate`]. The debt
//! is tracked; the caller is not yet made whole.
//!
//! # Side effect of enabling payments
//!
//! With `X402_PAY_TO` set, every POST to `/mcp` is buffered in memory (up to
//! 2 MB) so the JSON-RPC method can be read before routing — including free
//! calls, which previously streamed straight through. Requests above that cap
//! are refused with a JSON-RPC 413. Leaving payments off keeps the original
//! zero-copy path.
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

use x402_chain_solana::chain::Address;
use x402_axum::facilitator_client::FacilitatorClient;
use x402_axum::{StaticPriceTags, X402LayerBuilder, X402Middleware};
// KnownNetworkSolana is what puts `USDC::solana_devnet()` in scope.
use x402_chain_solana::{KnownNetworkSolana, V2SolanaExact};
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
///
/// x402 **v2**. v2 identifies the network by CAIP-2 chain id
/// (`solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1` for devnet) rather than v1's
/// `"solana-devnet"` name. Both halves of the payment path must agree on the
/// version, so this moves in lockstep with the client in `x402-mcp-proxy` and
/// with the `schemes` entry of whichever facilitator settles for us.
pub type PaidLayer =
    X402LayerBuilder<StaticPriceTags<x402_types::proto::v2::PriceTag>, Arc<FacilitatorClient>>;

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
        if self.mainnet { "solana" } else { "solana-devnet" }
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
    let mainnet = std::env::var("X402_NETWORK")
        .map(|n| matches!(n.trim(), "solana" | "mainnet"))
        .unwrap_or(false);
    let facilitator =
        std::env::var("X402_FACILITATOR").unwrap_or_else(|_| DEFAULT_FACILITATOR.to_string());

    // Reject an unusable price here too, so the catalogue and the gate agree.
    let usdc = if mainnet { USDC::solana() } else { USDC::solana_devnet() };
    if let Err(e) = usdc.parse(price.as_str()) {
        tracing::error!("X402_PRICE_USD `{price}` is invalid ({e}) — MCP payments stay disabled");
        return None;
    }

    Some(PaymentSettings { pay_to, price, mainnet, facilitator })
}

/// Builds the x402 layer from the environment, or `None` when payment is off.
///
/// Disabled unless `X402_PAY_TO` is set, so existing deployments are
/// unaffected until an operator opts in. Devnet is the default: reaching real
/// money takes an explicit `X402_NETWORK=solana` (or `mainnet`), so a
/// misconfiguration fails toward play money rather than toward charging
/// someone. The value `base` predates the move off EVM and is not recognised —
/// it would silently leave the deployment on devnet.
pub fn layer_from_env() -> Option<PaidLayer> {
    let settings = payment_settings()?;
    let usdc = if settings.mainnet { USDC::solana() } else { USDC::solana_devnet() };
    // Already validated by payment_settings().
    let amount = usdc.parse(settings.price.as_str()).ok()?;

    tracing::info!(
        "MCP payments ON: ${} per priced tool call on {}, paid to {}",
        settings.price,
        if settings.mainnet { "Solana MAINNET (real funds)" } else { "Solana devnet (test funds)" },
        settings.pay_to
    );

    Some(
        X402Middleware::new(settings.facilitator.as_str())
            // Settle *before* the work. A payment is a signed Solana
            // transaction, and its blockhash dies after ~60-90s — while x402
            // advertises `maxTimeoutSeconds: 300`, promising more time than the
            // chain allows. Settling afterwards therefore worked only for jobs
            // shorter than the blockhash window: a 6s clip settled, a YouTube
            // lecture failed `transaction_simulation` and was transcribed for
            // free, with nothing distinguishing the two. Since transcription is
            // slow by nature and long videos are the point of this product,
            // settle-after was unusable.
            //
            // In exchange, a failed job has already been charged.
            // [`McpFailureStatus`] records a credit for the payer — a ledger
            // write rather than an on-chain refund, so it has no timing
            // constraint. Note that credit is **not yet spendable** (see
            // [`compensate`]): the debt is recorded, the caller is not yet made
            // whole.
            .settle_before_execution()
            .with_price_tag(V2SolanaExact::price_tag(settings.pay_to, amount)),
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

    /// Claims a nonce and returns a guard that releases it on drop unless
    /// committed.
    ///
    /// Explicit release after the call can't run if the future is dropped —
    /// and a client disconnecting mid-transcription is exactly the scenario
    /// this exists for. Tying release to `Drop` makes cancellation safe:
    /// nothing settled, so nothing is held.
    pub fn claim_scoped(&self, nonce: &str) -> Option<NonceClaim> {
        self.claim(nonce).then(|| NonceClaim {
            guard: self.clone(),
            nonce: nonce.to_string(),
            committed: false,
        })
    }

    /// Gives a nonce back.
    ///
    /// The claim is taken *before* the payment layer runs, so concurrent
    /// retries can't both execute. But a payment that then fails verification
    /// — bad signature, insufficient funds, a flaky facilitator — never ran
    /// and never settled, and the correct client behaviour is to retry the
    /// same signed payload. Holding the nonce would answer that legitimate
    /// retry with a spurious "already used" for half an hour. So the claim is
    /// kept only when the work actually executed and settled.
    pub fn release(&self, nonce: &str) {
        let mut seen = match self.seen.lock() {
            Ok(seen) => seen,
            Err(poisoned) => poisoned.into_inner(),
        };
        seen.remove(nonce);
    }
}

/// A claimed nonce, released on drop unless [`NonceClaim::commit`] is called.
///
/// Held for the duration of a paid call. Committed only when the work
/// executed *and* settled; every other outcome — payment failure, tool
/// failure, transport error, or the future being cancelled — releases it, so
/// a client can retry the same signed payload.
pub struct NonceClaim {
    guard: NonceGuard,
    nonce: String,
    committed: bool,
}

impl NonceClaim {
    /// Keep the nonce: the work ran and the payment settled.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for NonceClaim {
    fn drop(&mut self) {
        if !self.committed {
            self.guard.release(&self.nonce);
        }
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

/// Largest response body inspected for a tool failure. Generous, because
/// transcripts with word-level timestamps are large and losing one would be
/// worse than mis-settling it.
const MAX_INSPECTABLE_RESPONSE: usize = 64 * 1024 * 1024;

/// Compensates the caller when a paid tool call fails.
///
/// Payment now settles *before* execution (see [`layer_from_env`]), so a failed
/// job cannot be un-charged by withholding settlement — the money has already
/// moved. Refunding on-chain would mean this server holding a funded wallet and
/// making payouts, which is a whole second payment system.
///
/// Instead the caller is granted one credit against [`crate::credits`], keyed by
/// the paying wallet. A ledger write has no blockhash window, costs no fees, and
/// reuses the same balance the Stripe path already spends from.
///
/// The failure *detection* is unchanged and still structural
/// ([`body_reports_tool_failure`]) — only the consequence moved, from "skip
/// settlement" to "grant a credit". The 502 re-stamp is kept because the router
/// still uses it to restore the caller's real status, and because it keeps the
/// response shape identical for clients that were built against the old
/// behaviour.
#[derive(Clone)]
pub struct McpFailureStatus<S> {
    inner: S,
    credits: Option<Arc<crate::credits::CreditStore>>,
}

impl<S> McpFailureStatus<S> {
    /// `credits: None` disables compensation — the failure is still detected
    /// and reported, but nothing is granted. Used where no ledger exists.
    pub fn new(inner: S, credits: Option<Arc<crate::credits::CreditStore>>) -> Self {
        Self { inner, credits }
    }
}

/// Ledger key for a paying wallet.
///
/// The credit ledger is keyed by an opaque identity string and already carries
/// two kinds (`user:<uuid>` accounts, raw device ids). An x402 caller has no
/// account, but it does have a wallet, which is stable across calls and costs
/// nothing to establish — so it makes a third kind.
pub fn wallet_key(payer: &str) -> String {
    format!("wallet:{payer}")
}

/// The paying wallet, from the settlement the payment layer attached.
///
/// With `settle_before_execution` the middleware injects
/// `Option<SettleResponse>` into the request extensions, and the settle
/// response carries `payer`. Reading it here avoids deserializing the signed
/// Solana transaction out of the `X-PAYMENT` header, which would drag
/// `solana-sdk` into a server that otherwise never touches Solana.
pub fn payer_from_extensions(request: &Request<Body>) -> Option<String> {
    let settled = request
        .extensions()
        .get::<Option<x402_types::proto::SettleResponse>>()?
        .as_ref()?;
    settled
        .0
        .get("payer")
        .and_then(|p| p.as_str())
        .map(str::to_owned)
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
        // Read before the request is consumed; the settlement extension is
        // gone by the time the response comes back.
        let payer = payer_from_extensions(&request);
        let credits = self.credits.clone();
        Box::pin(async move {
            let response = inner.call(request).await?;
            if !response.status().is_success() {
                // Transport-level failure — a 500 from the MCP service, a
                // timeout, a panic mapped to 5xx. Under settle-after this
                // returned early because the non-2xx status was itself what
                // made x402 withhold settlement, so no action was needed.
                // Settling first removed that lever: the payer has already
                // been charged, and undelivered work is undelivered whether
                // the failure was reported in-band or by status code.
                compensate(credits.as_deref(), payer.as_deref()).await;
                return Ok(response);
            }

            // A response too big to inspect must still reach the caller:
            // discarding a completed transcript to protect a billing decision
            // is a far worse trade than occasionally mis-billing one we
            // couldn't check. This branch is a 2xx carrying a large body —
            // work that succeeded — so the charge stands and nothing is owed.
            let too_large = response
                .headers()
                .get(axum::http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok())
                .is_some_and(|len| len > MAX_INSPECTABLE_RESPONSE);
            if too_large {
                tracing::warn!(
                    "x402: response too large to inspect; delivering it and letting the charge stand"
                );
                return Ok(response);
            }

            // Only ever applied on the paid path, which is a single
            // request/response tool call — so buffering here doesn't stall a
            // long-lived stream.
            let (mut parts, body) = response.into_parts();
            let bytes = match axum::body::to_bytes(body, MAX_INSPECTABLE_RESPONSE).await {
                Ok(bytes) => bytes,
                // Only reachable for a chunked response with no
                // Content-Length that outgrows the cap mid-stream. The body
                // is already consumed and unrecoverable, so this is the one
                // case where content is genuinely lost. Under settle-after,
                // reporting 502 was enough to stop the charge; settling first
                // means the payer has paid for something they will never
                // receive, so the debt must be recorded explicitly.
                Err(_) => {
                    compensate(credits.as_deref(), payer.as_deref()).await;
                    parts.status = axum::http::StatusCode::BAD_GATEWAY;
                    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                    return Ok(axum::response::Response::from_parts(
                        parts,
                        Body::from(
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": serde_json::Value::Null,
                                "error": {
                                    "code": -32603,
                                    "message": "response exceeded the inspectable size limit"
                                }
                            })
                            .to_string(),
                        ),
                    ));
                }
            };

            if body_reports_tool_failure(&bytes) {
                compensate(credits.as_deref(), payer.as_deref()).await;
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

/// Records one credit against the payer of a call whose work failed.
///
/// # This is not yet a refund
///
/// The credit is written, but **nothing can currently spend it**.
/// `api::handlers::resolve_identity` requires a valid Supabase JWT and returns
/// a `user:<sub>` key; falling back to a non-account identity was deliberately
/// removed as a security hole. A `wallet:` key therefore has no redemption
/// path, so this is correct bookkeeping and an unredeemable balance — not a
/// caller made whole.
///
/// It is written anyway because the debt is real and the payer is only
/// identifiable at this moment: recording it now means a redemption path added
/// later can honour balances accrued in the meantime, whereas dropping it
/// loses them permanently.
///
/// Closing the gap needs a way for a Solana payer to prove wallet ownership —
/// signing a challenge to link `wallet:<pubkey>` to an account, or to spend
/// directly. That is a product decision, tracked in `X402_HANDOFF.md` §8.
///
/// Deliberately best-effort and never fatal: the caller already has a failed
/// tool call, and turning a bookkeeping problem into a second failure helps
/// nobody. Every path that can't record logs loudly instead, because silently
/// keeping money for undelivered work is the one outcome worth being noisy
/// about.
async fn compensate(credits: Option<&crate::credits::CreditStore>, payer: Option<&str>) {
    match (credits, payer) {
        (Some(store), Some(payer)) => {
            let key = wallet_key(payer);
            let balance = crate::credits::add(store, &key, 1).await;
            // warn, not info: money was taken for work not delivered, and the
            // credit standing in for it cannot yet be spent.
            tracing::warn!(
                "x402: paid tool call failed — recorded 1 credit for {key} (balance {balance}). \
                 NOTE: wallet-keyed credits have no redemption path yet, so the payer is owed \
                 but not yet refunded"
            );
        }
        (Some(_), None) => tracing::error!(
            "x402: paid tool call failed but the settlement carried no payer — \
             cannot compensate; the caller has been charged for nothing"
        ),
        (None, _) => tracing::error!(
            "x402: paid tool call failed and no credit ledger is wired — \
             cannot compensate; the caller has been charged for nothing"
        ),
    }
}

/// Whether an MCP response reports a failed tool call.
///
/// Reads the `result.isError` field structurally. An earlier version matched
/// the raw bytes for `"isError":true`, which would misread a *successful*
/// transcript that happened to contain that text as a failure — and then deny
/// settlement for work that was genuinely delivered. Billing decisions
/// shouldn't hinge on what a video says.
///
/// Handles both framings, since the same payload arrives as bare JSON or in
/// SSE `data:` frames depending on what the client negotiated.
pub fn body_reports_tool_failure(body: &[u8]) -> bool {
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) {
        return reports_tool_error(&json);
    }

    String::from_utf8_lossy(body).lines().any(|line| {
        line.strip_prefix("data:")
            .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload.trim()).ok())
            .as_ref()
            .is_some_and(reports_tool_error)
    })
}

/// Whether a JSON-RPC response reports a failed call.
///
/// Two shapes, because MCP servers use both and this one uses the second:
///
/// - `result.isError == true` — the tool ran and reported failure in-band.
/// - a top-level `error` object — the handler returned `Err(ErrorData)`.
///
/// The second is what `dispatch_tool` actually produces. A failed
/// transcription comes back as:
///
/// ```text
/// {"jsonrpc":"2.0","id":2,"error":{"code":-32603,
///  "message":"Transcription failed: Video file not found: ..."}}
/// ```
///
/// Checking only `isError` meant this never fired for this server, and every
/// failed transcription settled. Both shapes are handled now.
fn reports_tool_error(json: &serde_json::Value) -> bool {
    if json.get("error").is_some_and(|e| !e.is_null()) {
        return true;
    }
    json.get("result")
        .and_then(|result| result.get("isError"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
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
                Some((tool, _table_price)) => {
                    // Deliberately not logging the table's price: billing uses
                    // X402_PRICE_USD, so a hardcoded figure here would drift
                    // from what is actually charged.
                    tracing::debug!("x402: gating tools/call for `{tool}`");

                    // Refuse a replayed payment rather than doing — and
                    // settling — the same work twice.
                    let presented_nonce = request
                        .headers()
                        .get("x-payment")
                        .and_then(|v| v.to_str().ok())
                        .and_then(payment_nonce);
                    // Held across the call; released on drop unless the work
                    // settles, so cancellation can't strand it.
                    let claim = presented_nonce
                        .as_deref()
                        .and_then(|nonce| nonces.claim_scoped(nonce));
                    if presented_nonce.is_some() && claim.is_none() {
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

                    // `?` is safe now: dropping `claim` releases the nonce.
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
                        // Executed but the tool failed. Under settle-before the
                        // caller HAS paid — `McpFailureStatus` compensates with
                        // a credit rather than by withholding settlement.
                        //
                        // Leaving `claim` to drop still releases the nonce, and
                        // that is still safe: replaying this payment cannot buy
                        // a second execution, because settlement now runs first
                        // and the transaction is already spent on-chain, so the
                        // facilitator rejects it before the tool is reached. A
                        // genuine retry needs a fresh payment either way.
                    } else if response.status().is_success()
                        && let Some(claim) = claim
                    {
                        // The only outcome that consumes the nonce: ran and
                        // settled. Everything else releases on drop.
                        claim.commit();
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

#[cfg(test)]
mod review_fix_tests {
    use super::*;

    /// A payment that fails verification never ran and never settled, so the
    /// client's correct move is to retry the same signed payload. Holding the
    /// nonce would answer that with a spurious "already used".
    #[test]
    fn a_released_nonce_can_be_claimed_again() {
        let guard = NonceGuard::default();
        assert!(guard.claim("n1"));
        assert!(!guard.claim("n1"), "still held while in flight");
        guard.release("n1");
        assert!(guard.claim("n1"), "retry after a failed payment must be admitted");
    }

    #[test]
    fn releasing_an_unknown_nonce_is_harmless() {
        let guard = NonceGuard::default();
        guard.release("never-claimed");
        assert!(guard.claim("never-claimed"));
    }

    /// The bug the structural check exists to prevent: a *successful*
    /// transcript that happens to quote `"isError":true` must not be read as
    /// a failure and denied settlement for work actually delivered.
    #[test]
    fn transcript_text_quoting_is_error_is_not_a_failure() {
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "isError": false,
                "content": [{"type": "text",
                             "text": "the JSON looked like {\"isError\":true} in the video"}]
            }
        }))
        .unwrap();
        assert!(
            !body_reports_tool_failure(&body),
            "a delivered transcript must not be denied settlement over its contents"
        );
    }

    #[test]
    fn a_real_failure_is_still_detected_in_both_framings() {
        let json = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "result": {"isError": true, "content": []}
        }))
        .unwrap();
        assert!(body_reports_tool_failure(&json));

        let sse = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"isError\":true}}\n\n";
        assert!(body_reports_tool_failure(sse));
    }

    /// `isError` nested somewhere other than the tool result must not count.
    #[test]
    fn is_error_outside_the_result_is_ignored() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[]},"meta":{"isError":true}}"#;
        assert!(!body_reports_tool_failure(body));
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    /// A dropped claim must free the nonce. This is the cancellation path: a
    /// client disconnecting mid-transcription settles nothing, so holding the
    /// nonce would reject their retry for the full TTL.
    #[test]
    fn dropping_an_uncommitted_claim_releases_the_nonce() {
        let guard = NonceGuard::default();
        {
            let claim = guard.claim_scoped("n1").expect("first claim");
            assert!(guard.claim_scoped("n1").is_none(), "held while in scope");
            drop(claim);
        }
        assert!(
            guard.claim_scoped("n1").is_some(),
            "a cancelled call must not strand the nonce"
        );
    }

    /// Committing keeps it — the work ran and settled, so a replay is a
    /// genuine double-charge attempt.
    #[test]
    fn a_committed_claim_keeps_the_nonce() {
        let guard = NonceGuard::default();
        guard.claim_scoped("n2").expect("first claim").commit();
        assert!(
            guard.claim_scoped("n2").is_none(),
            "a settled payment must not be replayable"
        );
    }

    /// Panicking mid-call still releases, since Drop runs while unwinding.
    #[test]
    fn a_claim_dropped_during_unwind_releases() {
        let guard = NonceGuard::default();
        let result = std::panic::catch_unwind({
            let guard = guard.clone();
            move || {
                let _claim = guard.claim_scoped("n3").expect("first claim");
                panic!("simulated failure mid-call");
            }
        });
        assert!(result.is_err());
        assert!(guard.claim_scoped("n3").is_some(), "unwind must release too");
    }
}

#[cfg(test)]
mod real_failure_tests {
    use super::*;

    /// Captured from a running server: `transcribe_video` against a missing
    /// file. Verbatim, including the SSE framing — earlier tests used
    /// hand-written JSON matching an assumed shape, which is why the
    /// detection could pass its tests and still never fire in production.
    const REAL_FAILURE_SSE: &[u8] = b"data: \nid: 0/0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32603,\"message\":\"Transcription failed: Video file not found: /definitely/not/a/real/file.mp4\"}}\nid: 1/0\n\n";

    #[test]
    fn the_servers_actual_failure_response_is_detected() {
        assert!(
            body_reports_tool_failure(REAL_FAILURE_SSE),
            "a real failed transcription must not settle"
        );
    }

    /// The same shape unframed, as a client negotiating plain JSON receives it.
    #[test]
    fn a_top_level_jsonrpc_error_is_a_failure() {
        let body = br#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"Transcription failed"}}"#;
        assert!(body_reports_tool_failure(body));
    }

    /// Captured from the same server: a successful call. Must still settle.
    #[test]
    fn a_real_successful_response_still_settles() {
        let success = b"data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Video transcribed successfully!\"}],\"isError\":false}}\n\n";
        assert!(
            !body_reports_tool_failure(success),
            "delivered work must not be denied settlement"
        );
    }

    /// An explicit null error must not read as a failure.
    #[test]
    fn wallet_keys_are_namespaced_away_from_accounts_and_devices() {
        // The ledger is shared with `user:<uuid>` and raw device ids, so a
        // wallet must not be able to collide with either.
        let k = wallet_key("8Pnjr4698LvRn7563BUkpJcXCK7im6yK26cBS8kiqjjK");
        assert_eq!(k, "wallet:8Pnjr4698LvRn7563BUkpJcXCK7im6yK26cBS8kiqjjK");
        assert!(!k.starts_with("user:"));
        assert!(k.contains(':'), "must stay namespaced");
    }

    fn request_with(ext: Option<Option<x402_types::proto::SettleResponse>>) -> Request<Body> {
        let mut r = Request::new(Body::empty());
        if let Some(v) = ext {
            r.extensions_mut().insert(v);
        }
        r
    }

    #[test]
    fn payer_is_read_from_a_completed_settlement() {
        let settled = x402_types::proto::SettleResponse(serde_json::json!({
            "success": true,
            "payer": "8Pnjr4698LvRn7563BUkpJcXCK7im6yK26cBS8kiqjjK",
            "network": "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
        }));
        assert_eq!(
            payer_from_extensions(&request_with(Some(Some(settled)))).as_deref(),
            Some("8Pnjr4698LvRn7563BUkpJcXCK7im6yK26cBS8kiqjjK")
        );
    }

    #[test]
    fn no_settlement_means_no_payer() {
        // `None` is what settle_after_execution injects, and a free call has no
        // extension at all. Neither may be mistaken for a payer.
        assert!(payer_from_extensions(&request_with(Some(None))).is_none());
        assert!(payer_from_extensions(&request_with(None)).is_none());
    }

    #[test]
    fn a_settlement_without_a_payer_field_yields_none() {
        let settled = x402_types::proto::SettleResponse(serde_json::json!({"success": true}));
        assert!(payer_from_extensions(&request_with(Some(Some(settled)))).is_none());
    }

    /// Inner service that always answers with a fixed status.
    #[derive(Clone)]
    struct FixedResponse(axum::http::StatusCode);

    impl Service<Request<Body>> for FixedResponse {
        type Response = axum::response::Response;
        type Error = std::convert::Infallible;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
        >;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: Request<Body>) -> Self::Future {
            let status = self.0;
            Box::pin(async move {
                Ok(axum::response::Response::builder()
                    .status(status)
                    .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#))
                    .unwrap())
            })
        }
    }

    async fn balance_after(status: axum::http::StatusCode, payer: &str) -> (i32, i32) {
        let dir = std::env::temp_dir().join(format!("x402-exit-{}", status.as_u16()));
        let _ = std::fs::create_dir_all(&dir);
        let store = Arc::new(crate::credits::test_store(dir.join("credits.json")));
        let key = wallet_key(payer);
        let before = crate::credits::balance(&store, &key).await;

        let mut svc = McpFailureStatus::new(FixedResponse(status), Some(store.clone()));
        let mut req = Request::new(Body::empty());
        req.extensions_mut()
            .insert(Some(x402_types::proto::SettleResponse(serde_json::json!({
                "success": true, "payer": payer,
            }))));
        let _ = svc.call(req).await.expect("service call");

        (before, crate::credits::balance(&store, &key).await)
    }

    #[tokio::test]
    async fn a_transport_level_failure_after_payment_is_compensated() {
        // Settle-before means a 5xx from the MCP service is undelivered paid
        // work exactly like an in-band tool error. This exit returned early
        // under settle-after, when the status itself withheld settlement.
        let (before, after) =
            balance_after(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "PayerFor500").await;
        assert_eq!(after, before + 1, "a 5xx after payment must record the debt");
    }

    #[tokio::test]
    async fn a_client_error_after_payment_is_also_compensated() {
        let (before, after) =
            balance_after(axum::http::StatusCode::BAD_REQUEST, "PayerFor400").await;
        assert_eq!(after, before + 1, "any non-2xx after payment owes the caller");
    }

    #[tokio::test]
    async fn a_successful_call_owes_nothing() {
        // The control: work delivered, charge stands, no credit.
        let (before, after) = balance_after(axum::http::StatusCode::OK, "PayerForOk").await;
        assert_eq!(after, before, "a delivered result must not be compensated");
    }

    #[tokio::test]
    async fn compensation_is_best_effort_and_never_panics() {
        // Every un-compensatable combination must degrade to a log, not a
        // second failure on top of the one the caller already has.
        compensate(None, None).await;
        compensate(None, Some("8Pnjr4698LvRn7563BUkpJcXCK7im6yK26cBS8kiqjjK")).await;
    }

    #[tokio::test]
    async fn a_failed_call_records_exactly_one_credit_for_the_payer() {
        // Deliberately NOT `new_store()`: without DATABASE_URL that resolves to
        // ./credits.json, the real ledger, and this test was silently topping
        // up a production balance on every run.
        let dir = std::env::temp_dir().join("x402-compensation-test");
        let _ = std::fs::create_dir_all(&dir);
        let store = crate::credits::test_store(dir.join("credits.json"));
        let payer = "TestPayerForCompensation11111111111111111111";
        let key = wallet_key(payer);
        let before = crate::credits::balance(&store, &key).await;

        compensate(Some(&store), Some(payer)).await;

        assert_eq!(
            crate::credits::balance(&store, &key).await,
            before + 1,
            "a failed paid call must record the debt, even though it is not yet spendable"
        );
    }

    #[test]
    fn a_null_error_field_is_not_a_failure() {
        let body = br#"{"jsonrpc":"2.0","id":2,"error":null,"result":{"content":[]}}"#;
        assert!(!body_reports_tool_failure(body));
    }
}
