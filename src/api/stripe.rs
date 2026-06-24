//! Stripe Checkout + webhook handlers. Env-gated — if `STRIPE_SECRET_KEY` is
//! unset the endpoints return 503 so the engine can run standalone for
//! anyone forking it without payments.
//!
//! Pricing is per-device credit packs (one-time payments, not subscriptions):
//! every successful `checkout.session.completed` event adds the configured
//! credits to the device that initiated the session, identified by the
//! `client_reference_id` Stripe forwards from the original Checkout request.
//!
//! Webhook signature verification follows Stripe's documented HMAC-SHA256
//! scheme: `Stripe-Signature` is `t=<timestamp>,v1=<hex_hmac>,…`; we compute
//! HMAC of `<timestamp>.<raw_body>` with `STRIPE_WEBHOOK_SECRET` and
//! constant-time-compare against the supplied `v1`. The raw request body is
//! required (parsed JSON would re-serialize differently), so the handler
//! extracts `Bytes` rather than `Json<T>`.

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use tracing::{error, info, warn};

use crate::api::handlers::AppState;
use crate::credits;

/// Pack options the client may request. Mapped to Stripe Price IDs via env
/// vars (e.g. `STRIPE_PRICE_25` → the price for the 25-credit pack).
const PACKS: &[(&str, i32)] = &[("25", 25), ("100", 100), ("500", 500)];

#[derive(Deserialize)]
pub struct CheckoutRequest {
    /// One of "25" / "100" / "500" (see `PACKS`).
    pub pack: String,
}

/// POST /api/checkout
/// Body: { "pack": "25" }
/// Headers: X-Device-Id
/// Returns: { "checkout_url": "https://checkout.stripe.com/..." }
pub async fn create_checkout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CheckoutRequest>,
) -> (StatusCode, Json<Value>) {
    // Resolve the ledger identity (account key when signed in, else device id)
    // so the purchase credits whoever is actually making it.
    let identity = match super::handlers::resolve_identity_pub(&state, &headers).await {
        Ok(id) => id,
        Err(e) => return e,
    };

    let pack_credits = match PACKS.iter().find(|(k, _)| *k == req.pack) {
        Some((_, c)) => *c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "unsupported pack",
                    "valid_packs": PACKS.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
                })),
            );
        }
    };

    let secret = match std::env::var("STRIPE_SECRET_KEY") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            warn!("/api/checkout hit but STRIPE_SECRET_KEY is unset");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "checkout not configured on this server" })),
            );
        }
    };

    let price_env = format!("STRIPE_PRICE_{}", req.pack);
    let price_id = match std::env::var(&price_env) {
        Ok(s) if s.starts_with("price_") => s,
        _ => {
            error!("checkout requested pack {} but {} is unset/invalid", req.pack, price_env);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": format!("pack {} not configured", req.pack) })),
            );
        }
    };

    let success_url = std::env::var("CHECKOUT_SUCCESS_URL")
        .unwrap_or_else(|_| "https://whisgram.nvnv.app/?checkout=success".to_string());
    let cancel_url = std::env::var("CHECKOUT_CANCEL_URL")
        .unwrap_or_else(|_| "https://whisgram.nvnv.app/?checkout=cancel".to_string());

    // Stripe API uses form-encoding, not JSON.
    let form_params: Vec<(&str, String)> = vec![
        ("mode", "payment".to_string()),
        ("success_url", success_url),
        ("cancel_url", cancel_url),
        ("client_reference_id", identity.clone()),
        // Metadata is echoed back on the webhook — belt-and-braces in case
        // client_reference_id is ever stripped by a future Stripe change.
        // `identity` holds an account key (`user:…`) for signed-in users or a
        // raw device id for legacy clients; the webhook credits it verbatim.
        ("metadata[identity]", identity),
        ("metadata[pack]", req.pack.clone()),
        ("metadata[credits]", pack_credits.to_string()),
        ("line_items[0][price]", price_id),
        ("line_items[0][quantity]", "1".to_string()),
    ];

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.stripe.com/v1/checkout/sessions")
        .bearer_auth(&secret)
        .form(&form_params)
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            error!("Stripe checkout API call failed: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "stripe API unreachable" })),
            );
        }
    };

    let status = resp.status();
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            error!("Stripe checkout response malformed: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "stripe API returned non-JSON" })),
            );
        }
    };

    if !status.is_success() {
        error!("Stripe checkout returned {}: {}", status, body);
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "stripe API rejected request",
                "stripe_error": body.get("error"),
            })),
        );
    }

    let url = match body.get("url").and_then(|v| v.as_str()) {
        Some(u) => u.to_string(),
        None => {
            error!("Stripe checkout response missing url: {}", body);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "stripe response missing url" })),
            );
        }
    };

    info!(
        "Created Stripe Checkout session for pack {} ({} credits)",
        req.pack, pack_credits
    );
    (StatusCode::OK, Json(json!({ "checkout_url": url })))
}

/// POST /api/webhook/stripe
/// Stripe forwards events here (configured at https://dashboard.stripe.com).
/// We listen for `checkout.session.completed` and credit the device.
pub async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let secret = match std::env::var("STRIPE_WEBHOOK_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            warn!("/api/webhook/stripe hit but STRIPE_WEBHOOK_SECRET is unset");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "webhook not configured" })),
            );
        }
    };

    let sig_header = match headers.get("stripe-signature").and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing stripe-signature header" })),
            );
        }
    };

    if !verify_signature(secret.as_bytes(), &body, sig_header) {
        warn!("Stripe webhook signature verification failed");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid signature" })),
        );
    }

    // Parse the event payload now that we trust the signature.
    let event: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            error!("Stripe webhook body wasn't JSON: {}", e);
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "bad json" })));
        }
    };

    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // We only act on the one event type. Stripe will retry until we 2xx, so we
    // also 2xx other events to acknowledge receipt.
    if event_type != "checkout.session.completed" {
        return (StatusCode::OK, Json(json!({ "ignored": event_type })));
    }

    let session = event.pointer("/data/object");
    let metadata = session
        .and_then(|s| s.get("metadata"))
        .and_then(|m| m.as_object());

    // Identity to credit. Prefer the new `identity` metadata key; fall back to
    // the legacy `device_id` key (for any sessions created before this rename)
    // and finally `client_reference_id`.
    let identity = metadata
        .and_then(|m| m.get("identity"))
        .and_then(|v| v.as_str())
        .or_else(|| metadata.and_then(|m| m.get("device_id")).and_then(|v| v.as_str()))
        .map(str::to_string)
        .or_else(|| {
            session
                .and_then(|s| s.get("client_reference_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });

    let credits_str = metadata
        .and_then(|m| m.get("credits"))
        .and_then(|v| v.as_str());

    let (identity, credit_amount) = match (identity, credits_str.and_then(|s| s.parse::<i32>().ok())) {
        (Some(d), Some(c)) if c > 0 => (d, c),
        _ => {
            error!(
                "checkout.session.completed missing identity / credits in metadata: {}",
                event
            );
            // Acknowledge so Stripe stops retrying — we can't recover this one
            // automatically, but a 200 prevents the event from clogging the
            // retry queue.
            return (
                StatusCode::OK,
                Json(json!({ "warning": "session lacked metadata" })),
            );
        }
    };

    let new_balance = credits::add(&state.credits, &identity, credit_amount).await;
    info!(
        "Stripe webhook credited {} with {} credits (new balance: {})",
        identity, credit_amount, new_balance
    );
    (StatusCode::OK, Json(json!({ "ok": true, "balance": new_balance })))
}

/// Verify a Stripe-Signature header against the raw request body and a
/// webhook signing secret. Header format: `t=<unix_ts>,v1=<hex_hmac>,…`.
/// We accept the request if any `v1` value matches our computed HMAC.
fn verify_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    let parts: HashMap<&str, &str> = header
        .split(',')
        .filter_map(|kv| {
            let mut it = kv.splitn(2, '=');
            Some((it.next()?, it.next()?))
        })
        .collect();

    let timestamp = match parts.get("t") {
        Some(t) => *t,
        None => return false,
    };

    let signed_payload = format!("{}.", timestamp);
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(signed_payload.as_bytes());
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    // Stripe sends the signatures as hex. We may have multiple `v1=`
    // entries (rotating secrets); accept if any match.
    for (k, v) in header.split(',').filter_map(|kv| {
        let mut it = kv.splitn(2, '=');
        Some((it.next()?, it.next()?))
    }) {
        if k != "v1" {
            continue;
        }
        let Ok(provided) = decode_hex(v) else { continue };
        if provided.len() == expected.len() && constant_time_eq(&provided, &expected) {
            return true;
        }
    }
    false
}

fn decode_hex(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
