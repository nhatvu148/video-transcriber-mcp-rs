//! JWT signature-verification tests.
//!
//! These exist because of a production outage. The jsonwebtoken 9 -> 11 upgrade
//! compiled cleanly, passed every existing test, deployed successfully, booted
//! healthy, and correctly returned 401 for missing and malformed tokens — then
//! panicked on the first *real* token:
//!
//!     Could not automatically determine the process-level CryptoProvider
//!     from jsonwebtoken crate features.
//!
//! jsonwebtoken 11 made its crypto backend pluggable and its default features
//! enable neither provider. Nothing caught it because every test we had failed
//! *before* reaching the crypto layer: header parsing rejects garbage, and
//! malformed-token tests never get as far as verifying a signature.
//!
//! So the rule these tests encode is: a test that never verifies a genuine
//! signature does not test JWT verification. Each one below signs a token with
//! a real private key and verifies it against the matching public JWK, exactly
//! as `auth::verify_jwt` does — which is the code path that panicked.
//!
//! Keys live in tests/fixtures/ and are throwaway, generated for this suite.
//! They are test material only and must never be used anywhere else.

use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Mirrors the subset of `auth::UserClaims` the verifier actually checks.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Claims {
    sub: String,
    aud: String,
    exp: usize,
    email: Option<String>,
}

/// Supabase issues `aud: "authenticated"`, and the production validation in
/// `auth.rs` pins that value.
const AUDIENCE: &str = "authenticated";

const ES256_PRIVATE: &str = include_str!("fixtures/test-es256-private.pem");
const RS256_PRIVATE: &str = include_str!("fixtures/test-rs256-private.pem");

const EC_X: &str = "6TUusjoiKhF4G7xy_2eB3Ea1malyT9jAqaejjM2vUrQ";
const EC_Y: &str = "nWzJN2zEQ0JSAa4VvlaPSiDHjvlSYDIfdI-nBViKs2A";
const RSA_N: &str = "yNe2BfbpaukgC702E0mmBQ3Il3_xZcSUNmsJDCDJU_uuPlfccGgBlE1oyLHV1Qidu2raYVp-EcsRVuyXd1_io8W2E1IVG4fCceZXADb5Vm9SbOQFg_nlZZe6fIKfIt6RB3WMXVUaJfO7Kf6zPdUZzYFIgqKIFQ3AW-0EvHTu4_6kwxI7t7SpOv6ljQ1el5q6vf_d0DbfFN0pQDvQDcumQnobttBw3YqLkaMNUdKZNufi64nj7VNr7d8mD6hXZ2vI37X1mBlsZYT-lYtrouXpBJUAnYQrGcSaELA6Svr1553fdsuNcTm-Ov7Io8xVeWn3eALZXb-tUXggJL_kiNOiLw";
const RSA_E: &str = "AQAB";

fn far_future() -> usize {
    // Fixed, far-future timestamp so the suite can't start failing on a date.
    4_102_444_800 // 2100-01-01
}

fn claims() -> Claims {
    Claims {
        sub: "11111111-2222-3333-4444-555555555555".into(),
        aud: AUDIENCE.into(),
        exp: far_future(),
        email: Some("user@example.test".into()),
    }
}

/// The public half of the ES256 fixture, shaped like a Supabase JWKS entry.
fn es256_jwk() -> Jwk {
    serde_json::from_value(json!({
        "kty": "EC", "crv": "P-256", "alg": "ES256",
        "use": "sig", "kid": "test-es256", "x": EC_X, "y": EC_Y,
    }))
    .expect("ES256 JWK")
}

fn rs256_jwk() -> Jwk {
    serde_json::from_value(json!({
        "kty": "RSA", "alg": "RS256",
        "use": "sig", "kid": "test-rs256", "n": RSA_N, "e": RSA_E,
    }))
    .expect("RS256 JWK")
}

/// Validation configured exactly as `auth::verify_jwt` configures it.
fn production_validation(algorithm: Algorithm) -> Validation {
    let mut validation = Validation::new(algorithm);
    validation.set_audience(&[AUDIENCE]);
    validation
}

fn sign(algorithm: Algorithm, kid: &str, key: &EncodingKey, claims: &Claims) -> String {
    let mut header = Header::new(algorithm);
    header.kid = Some(kid.to_string());
    encode(&header, claims, key).expect("signing should succeed")
}

/// THE regression test. Before the `aws_lc_rs` feature was added this panicked
/// rather than failing, which is why the outage reached production.
#[test]
fn verifies_a_genuine_es256_token_the_way_supabase_signs_them() {
    let key = EncodingKey::from_ec_pem(ES256_PRIVATE.as_bytes()).expect("ES256 encoding key");
    let token = sign(Algorithm::ES256, "test-es256", &key, &claims());

    let decoding = DecodingKey::from_jwk(&es256_jwk()).expect("decoding key from JWK");
    let decoded = decode::<Claims>(&token, &decoding, &production_validation(Algorithm::ES256))
        .expect("a genuine ES256 token must verify");

    assert_eq!(decoded.claims, claims());
}

/// `algorithm_from_jwk` also maps RSA JWKs to RS256, so that branch needs the
/// provider just as much.
#[test]
fn verifies_a_genuine_rs256_token() {
    let key = EncodingKey::from_rsa_pem(RS256_PRIVATE.as_bytes()).expect("RS256 encoding key");
    let token = sign(Algorithm::RS256, "test-rs256", &key, &claims());

    let decoding = DecodingKey::from_jwk(&rs256_jwk()).expect("decoding key from JWK");
    let decoded = decode::<Claims>(&token, &decoding, &production_validation(Algorithm::RS256))
        .expect("a genuine RS256 token must verify");

    assert_eq!(decoded.claims, claims());
}

/// A tampered signature must be *rejected*, not panic — the distinction that
/// matters, since a panic takes the whole worker down instead of returning 401.
#[test]
fn rejects_a_tampered_signature_without_panicking() {
    let key = EncodingKey::from_ec_pem(ES256_PRIVATE.as_bytes()).expect("ES256 encoding key");
    let token = sign(Algorithm::ES256, "test-es256", &key, &claims());

    // Flip the last character of the signature segment.
    let (body, sig) = token.rsplit_once('.').expect("token has three segments");
    let last = sig.chars().last().unwrap();
    let flipped = if last == 'A' { 'B' } else { 'A' };
    let tampered = format!("{body}.{}{flipped}", &sig[..sig.len() - 1]);

    let decoding = DecodingKey::from_jwk(&es256_jwk()).expect("decoding key");
    let result = decode::<Claims>(&tampered, &decoding, &production_validation(Algorithm::ES256));
    assert!(result.is_err(), "a tampered signature must not verify");
}

/// A token signed by a *different* key must not verify — the case that
/// actually matters for auth, since anyone can mint their own ES256 token.
#[test]
fn rejects_a_token_signed_by_an_unknown_key() {
    // Sign with the RSA fixture, then try to verify against the EC JWK.
    let key = EncodingKey::from_rsa_pem(RS256_PRIVATE.as_bytes()).expect("RS256 encoding key");
    let token = sign(Algorithm::RS256, "test-rs256", &key, &claims());

    let decoding = DecodingKey::from_jwk(&es256_jwk()).expect("decoding key");
    let result = decode::<Claims>(&token, &decoding, &production_validation(Algorithm::ES256));
    assert!(result.is_err(), "a foreign-signed token must not verify");
}

#[test]
fn rejects_an_expired_token() {
    let mut expired = claims();
    expired.exp = 1_000_000_000; // 2001
    let key = EncodingKey::from_ec_pem(ES256_PRIVATE.as_bytes()).expect("ES256 encoding key");
    let token = sign(Algorithm::ES256, "test-es256", &key, &expired);

    let decoding = DecodingKey::from_jwk(&es256_jwk()).expect("decoding key");
    let result = decode::<Claims>(&token, &decoding, &production_validation(Algorithm::ES256));
    assert!(result.is_err(), "an expired token must not verify");
}

/// Production pins `aud: "authenticated"`; a token for a different audience
/// must be refused even though its signature is perfectly valid.
#[test]
fn rejects_a_token_for_the_wrong_audience() {
    let mut wrong = claims();
    wrong.aud = "some-other-service".into();
    let key = EncodingKey::from_ec_pem(ES256_PRIVATE.as_bytes()).expect("ES256 encoding key");
    let token = sign(Algorithm::ES256, "test-es256", &key, &wrong);

    let decoding = DecodingKey::from_jwk(&es256_jwk()).expect("decoding key");
    let result = decode::<Claims>(&token, &decoding, &production_validation(Algorithm::ES256));
    assert!(result.is_err(), "wrong-audience token must not verify");
}
