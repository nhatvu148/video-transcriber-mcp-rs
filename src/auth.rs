//! Supabase Auth integration.
//!
//! Whisgram delegates identity to Supabase: the web app and Chrome extension
//! both sign users in via Google OAuth or email magic link through Supabase
//! Auth, then attach the resulting access token as `Authorization: Bearer …`
//! on every API call. This module verifies those tokens server-side.
//!
//! ## Verification approach (asymmetric / JWKS)
//!
//! Modern Supabase projects sign JWTs with ECC P-256 (ES256). We verify by
//! fetching the project's public JWKS at startup and matching each incoming
//! token's `kid` (key ID) header against the published keys.
//!
//! This is more involved than HS256 with a shared secret, but it has two
//! big wins:
//!   1. **No secret to leak.** The verification key is public; the signing
//!      key never leaves Supabase. Compromising our Fly machine doesn't
//!      let an attacker mint tokens for our users.
//!   2. **Key rotation is transparent.** When Supabase rotates the signing
//!      key, the JWKS endpoint returns both old and new keys; clients keep
//!      working until old tokens expire. Our cache refreshes hourly to pick
//!      up new keys.

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// The subset of Supabase JWT claims we actually consume. Supabase puts a
/// lot more in there (role, app_metadata, user_metadata, aal, amr, session_id…)
/// but we only need the user's identity for credit operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserClaims {
    /// Supabase user UUID. Stable across email changes — use this as the
    /// primary identity key when keying credits to a user.
    pub sub: String,
    /// User's email, normalized lowercase by Supabase. Optional because
    /// some sign-in methods (phone-only, anonymous) don't carry an email.
    /// Whisgram only enables email-based methods, so this will be present
    /// in practice.
    #[serde(default)]
    pub email: Option<String>,
    /// Token expiry (Unix seconds). Validated by `jsonwebtoken`; we carry
    /// it through for logging/debug.
    pub exp: i64,
    /// Issued-at time (Unix seconds).
    #[serde(default)]
    pub iat: Option<i64>,
    /// Audience — Supabase sets this to `"authenticated"` for signed-in users.
    /// Validated by `jsonwebtoken` against an expected value.
    #[serde(default)]
    pub aud: Option<String>,
    /// Role — usually `"authenticated"`.
    #[serde(default)]
    pub role: Option<String>,
}

/// Cached JWKS keyset with a soft TTL. We refresh from Supabase hourly OR
/// on-demand if we see a `kid` we don't recognize (e.g., right after a
/// Supabase-side key rotation, before our scheduled refetch).
pub struct JwksCache {
    jwks_url: String,
    state: RwLock<CachedKeys>,
}

struct CachedKeys {
    keyset: Option<JwkSet>,
    fetched_at: Option<Instant>,
}

const JWKS_TTL: Duration = Duration::from_secs(3600);

impl JwksCache {
    /// Create a new cache pointed at the Supabase project's JWKS endpoint.
    /// Doesn't fetch immediately — first verify() call will populate.
    pub fn new(supabase_url: &str) -> Arc<Self> {
        Arc::new(Self {
            jwks_url: format!(
                "{}/auth/v1/.well-known/jwks.json",
                supabase_url.trim_end_matches('/')
            ),
            state: RwLock::new(CachedKeys {
                keyset: None,
                fetched_at: None,
            }),
        })
    }

    /// Force-refresh the cached JWKS from Supabase. Returns the freshly
    /// fetched keyset.
    async fn refresh(&self) -> Result<JwkSet> {
        let resp = reqwest::get(&self.jwks_url)
            .await
            .context("fetching JWKS from Supabase")?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "Supabase JWKS endpoint returned {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }
        let keyset: JwkSet = resp
            .json()
            .await
            .context("parsing JWKS response as JSON")?;
        let mut state = self.state.write().await;
        state.keyset = Some(keyset.clone());
        state.fetched_at = Some(Instant::now());
        Ok(keyset)
    }

    /// Return the current cached JWKS, refreshing if stale or absent.
    async fn current(&self) -> Result<JwkSet> {
        {
            let state = self.state.read().await;
            if let (Some(ks), Some(at)) = (&state.keyset, &state.fetched_at) {
                if at.elapsed() < JWKS_TTL {
                    return Ok(ks.clone());
                }
            }
        }
        self.refresh().await
    }
}

/// Verify a Supabase-issued JWT against the cached JWKS and return its claims.
///
/// Returns `Err` if:
/// - The token is malformed or missing a `kid` header
/// - The matching public key isn't in our cache (even after a forced refresh)
/// - The signature is invalid
/// - The token has expired
/// - The audience is not `"authenticated"`
pub async fn verify_jwt(token: &str, cache: &JwksCache) -> Result<UserClaims> {
    let header = decode_header(token).context("decoding JWT header")?;
    let kid = header
        .kid
        .ok_or_else(|| anyhow!("JWT header has no `kid` — refusing to verify"))?;

    // Look up the key by kid. If we don't find it on first try, refresh JWKS
    // once (Supabase may have rotated keys) and try again. Hard fail if still
    // not found — issuing-side and verifying-side are out of sync.
    let try_verify = |jwks: &JwkSet| -> Result<Option<UserClaims>> {
        let Some(jwk) = jwks.find(&kid) else {
            return Ok(None);
        };
        let key = DecodingKey::from_jwk(jwk).context("converting JWK to DecodingKey")?;
        let algorithm = algorithm_from_jwk(jwk)?;
        let mut validation = Validation::new(algorithm);
        validation.set_audience(&["authenticated"]);
        let token_data = decode::<UserClaims>(token, &key, &validation)
            .context("JWT signature/claims verification failed")?;
        Ok(Some(token_data.claims))
    };

    let jwks = cache.current().await?;
    if let Some(claims) = try_verify(&jwks)? {
        return Ok(claims);
    }
    // kid not in current cache → maybe Supabase rotated keys. Force refresh.
    let jwks = cache.refresh().await?;
    if let Some(claims) = try_verify(&jwks)? {
        return Ok(claims);
    }
    Err(anyhow!(
        "JWT references kid `{}` not present in Supabase JWKS",
        kid
    ))
}

/// Map a JWK's algorithm field to `jsonwebtoken::Algorithm`. Supabase uses
/// ES256 (ECC P-256) currently; we leave the door open for RS256/EdDSA in
/// case they switch later.
fn algorithm_from_jwk(jwk: &jsonwebtoken::jwk::Jwk) -> Result<Algorithm> {
    use jsonwebtoken::jwk::AlgorithmParameters as Alg;
    Ok(match &jwk.algorithm {
        Alg::EllipticCurve(_) => Algorithm::ES256,
        Alg::RSA(_) => Algorithm::RS256,
        Alg::OctetKeyPair(_) => Algorithm::EdDSA,
        _ => return Err(anyhow!("Unsupported JWK algorithm in Supabase JWKS")),
    })
}

/// Strip the `Bearer ` prefix from an `Authorization` header value. Returns
/// the token portion if the header is well-formed, otherwise `None`. Kept
/// here so the parsing rules live next to the verification logic.
pub fn extract_bearer_token(auth_header: &str) -> Option<&str> {
    auth_header.strip_prefix("Bearer ").map(|s| s.trim())
}

// ---------- axum extractor ----------

use axum::{
    extract::{FromRef, FromRequestParts},
    http::{request::Parts, StatusCode},
    Json,
};

/// Extracted, verified user identity. Handlers that need to know who's
/// calling take this as a parameter; if the request lacks a valid token,
/// axum returns 401 before the handler runs.
#[derive(Debug, Clone)]
pub struct AuthUser(pub UserClaims);

/// Generic implementation: any app state that exposes our `Arc<JwksCache>`
/// (via `FromRef`) can be the source state for the extractor. This keeps
/// the extractor decoupled from the concrete `AppState` definition in
/// `api/handlers.rs`.
impl<S> FromRequestParts<S> for AuthUser
where
    Arc<JwksCache>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| unauthorized("missing Authorization header"))?;
        let token = extract_bearer_token(auth_header)
            .ok_or_else(|| unauthorized("malformed Authorization header"))?;
        let cache: Arc<JwksCache> = Arc::<JwksCache>::from_ref(state);
        let claims = verify_jwt(token, &cache).await.map_err(|e| {
            tracing::debug!("JWT verification failed: {:#}", e);
            unauthorized("invalid or expired token")
        })?;
        Ok(AuthUser(claims))
    }
}

fn unauthorized(message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": message })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_token() {
        assert_eq!(extract_bearer_token("Bearer abc.def.ghi"), Some("abc.def.ghi"));
        assert_eq!(extract_bearer_token("bearer abc.def.ghi"), None);
        assert_eq!(extract_bearer_token("Token abc"), None);
    }

    #[test]
    fn jwks_cache_uses_supabase_url() {
        let cache = JwksCache::new("https://abc.supabase.co");
        assert_eq!(
            cache.jwks_url,
            "https://abc.supabase.co/auth/v1/.well-known/jwks.json"
        );
        // Trailing slashes are normalized so we don't end up with `//`.
        let cache = JwksCache::new("https://abc.supabase.co/");
        assert_eq!(
            cache.jwks_url,
            "https://abc.supabase.co/auth/v1/.well-known/jwks.json"
        );
    }

    #[test]
    fn rejects_malformed_token() {
        // No "Bearer " prefix isn't auth.rs's concern — this just ensures
        // jsonwebtoken's header decode rejects obviously-bad input.
        assert!(decode_header("not-a-jwt").is_err());
    }
}
