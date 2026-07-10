use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use sqlx::Row;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::api::jobs::{
    Job, JobRequest, JobResult, JobStatus, JobStore, TranscriptChunk, parse_model,
};
use crate::auth::{AuthUser, JwksCache};
use crate::credits::{self, CreditStore, is_valid_device_id};
use crate::llm::{
    answer_from_library, chat_about_transcript, chunk_segments, embed, generate_flashcards,
    summarize_and_diagram,
};
use crate::transcriber::types::{Segment, VideoMetadata};
use crate::transcriber::{TranscriberEngine, TranscriptionOptions};
use crate::utils::paths::get_default_output_dir;
use axum::extract::FromRef;

#[derive(Clone)]
pub struct AppState {
    pub jobs: JobStore,
    pub engine: Arc<Mutex<TranscriberEngine>>,
    pub credits: CreditStore,
    /// Bounds how many transcription pipelines run concurrently on this
    /// machine. Excess jobs stay `Queued` and wait for a permit instead of
    /// piling up `yt-dlp`/`ffmpeg` processes until the box runs out of
    /// CPU/RAM. Size via `MAX_CONCURRENT_JOBS` (default 4).
    pub pipeline_permits: Arc<Semaphore>,
    /// Cached Supabase JWKS for verifying incoming auth tokens. Cloned cheaply
    /// (Arc) on every request. `None` only when SUPABASE_URL isn't set, in
    /// which case any auth-requiring endpoint will 401.
    pub jwks: Arc<JwksCache>,
}

/// Lets axum's `AuthUser` extractor pull the shared `JwksCache` out of the
/// application state without coupling the extractor to the rest of AppState.
impl FromRef<AppState> for Arc<JwksCache> {
    fn from_ref(state: &AppState) -> Self {
        state.jwks.clone()
    }
}

/// GET /api/me — returns the current authenticated user's identity, or 401
/// if the request lacks a valid Supabase token. Used by the frontend to
/// confirm sign-in succeeded and surface the email in the UI.
pub async fn get_me(AuthUser(claims): AuthUser) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "user_id": claims.sub,
            "email": claims.email,
        })),
    )
}

#[derive(serde::Deserialize)]
pub struct ChatMsg {
    pub role: String,
    pub content: String,
}

#[derive(serde::Deserialize)]
pub struct ChatBody {
    pub transcript: String,
    #[serde(default)]
    pub title: String,
    /// Prior conversation turns (role: "user" | "assistant").
    #[serde(default)]
    pub messages: Vec<ChatMsg>,
    pub question: String,
}

/// POST /api/chat — answer a question about a video, grounded in its transcript.
/// Auth-required so it's tied to a signed-in account. Free feature: the
/// per-video question cap is enforced client-side, so no credit is charged.
pub async fn chat(
    AuthUser(_claims): AuthUser,
    Json(body): Json<ChatBody>,
) -> (StatusCode, Json<Value>) {
    let q = body.question.trim();
    if q.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "empty question" })));
    }
    if body.transcript.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing transcript" })),
        );
    }
    let history: Vec<(String, String)> = body
        .messages
        .into_iter()
        .map(|m| (m.role, m.content))
        .collect();
    match chat_about_transcript(&body.transcript, &body.title, &history, q).await {
        Ok(answer) => (StatusCode::OK, Json(json!({ "answer": answer }))),
        Err(e) => {
            error!("chat failed: {e:#}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "chat failed, try again" })),
            )
        }
    }
}

#[derive(serde::Deserialize)]
pub struct LibraryAskBody {
    pub question: String,
    /// Prior conversation turns (role: "user" | "assistant") for multi-turn.
    #[serde(default)]
    pub messages: Vec<ChatMsg>,
    /// The note currently open, if any — its full transcript is always kept in
    /// context so "about this video" questions get full detail on top of the
    /// library RAG. Empty/absent when asking purely across the library.
    #[serde(default)]
    pub transcript: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

/// One retrieved passage + its source video (Phase 6 vector search). Built via
/// manual row extraction — the engine's sqlx has no `macros` feature (no
/// `FromRow` derive), matching credits.rs.
struct LibraryHit {
    content: String,
    start_time: Option<f64>,
    // Selected as ::text so this works regardless of the sqlx `uuid` feature.
    transcript_id: String,
    title: Option<String>,
    url: Option<String>,
}

/// Format an embedding as a pgvector text literal: `[0.1,0.2,...]`.
fn to_pgvector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// POST /api/library-ask — answer a question across ALL of the caller's notes.
/// Embeds the question, vector-searches their `transcript_chunks` (scoped to
/// their user_id — the service pool bypasses RLS, so this filter is the
/// security boundary), then RAGs an answer with citations. Free; auth-required.
pub async fn library_ask(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<LibraryAskBody>,
) -> (StatusCode, Json<Value>) {
    let question = body.question.trim();
    if question.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "empty question" })),
        );
    }
    let Some(pool) = state.credits.pool() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "library search needs a database backend" })),
        );
    };

    // 0) Fair-use daily cap (free feature). Atomic upsert-and-count. Fail-open:
    // if the usage table errors (e.g. migration not yet run), don't block.
    let cap: i32 = std::env::var("LIBRARY_ASK_DAILY_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(50);
    let used: i32 = sqlx::query_scalar(
        "INSERT INTO public.library_ask_usage (user_id, day, count) \
         VALUES ($1::uuid, current_date, 1) \
         ON CONFLICT (user_id, day) \
         DO UPDATE SET count = library_ask_usage.count + 1 \
         RETURNING count",
    )
    .bind(&claims.sub)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    if used > cap {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": format!("Daily limit reached ({cap} questions/day). Resets tomorrow.")
            })),
        );
    }

    // 1) Embed the question.
    let query_vec = match embed(vec![question.to_string()]).await {
        Ok(mut v) if !v.is_empty() => v.remove(0),
        _ => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "embedding failed, try again" })),
            );
        }
    };
    let vec_literal = to_pgvector_literal(&query_vec);

    // 2) User-scoped vector search (cosine distance).
    const K: i64 = 8;
    let raw = match sqlx::query(
        "SELECT c.content, c.start_time, c.transcript_id::text AS transcript_id, \
                t.title, t.url \
         FROM public.transcript_chunks c \
         JOIN public.transcripts t ON t.id = c.transcript_id \
         WHERE c.user_id = $1::uuid AND c.embedding IS NOT NULL \
         ORDER BY c.embedding <=> $2::vector \
         LIMIT $3",
    )
    .bind(&claims.sub)
    .bind(&vec_literal)
    .bind(K)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("library-ask vector search failed: {e:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "search failed" })),
            );
        }
    };
    let rows: Vec<LibraryHit> = raw
        .iter()
        .map(|r| LibraryHit {
            content: r.get("content"),
            start_time: r.get("start_time"),
            transcript_id: r.get("transcript_id"),
            title: r.get("title"),
            url: r.get("url"),
        })
        .collect();

    // 3) Build context: the currently-open note (if any) is ALWAYS included in
    // full, then the retrieved library passages. No scope toggle — one answer
    // grounded in both.
    let mut context = String::new();
    if let Some(t) = body
        .transcript
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let title = body.title.as_deref().unwrap_or("the current video");
        // Bound token cost on very long transcripts (~48k chars ≈ 12k tokens).
        let truncated: String = t.chars().take(48_000).collect();
        context.push_str(&format!(
            "[Current video: {title}]\n{truncated}\n\n"
        ));
    }
    for hit in &rows {
        let title = hit.title.as_deref().unwrap_or("Untitled");
        let ts = hit
            .start_time
            .map(|t| format!(" @ {}s", t.round() as i64))
            .unwrap_or_default();
        context.push_str(&format!("[From: {title}{ts}]\n{}\n\n", hit.content));
    }
    if context.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "answer": "I couldn't find anything in your library about that yet. \
                    Transcribe a few more videos, or try rephrasing.",
                "sources": []
            })),
        );
    }

    let history: Vec<(String, String)> = body
        .messages
        .iter()
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect();
    let answer = match answer_from_library(question, &context, &history).await {
        Ok(a) => a,
        Err(e) => {
            error!("library-ask RAG failed: {e:#}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "answer failed, try again" })),
            );
        }
    };

    // Distinct source videos for the UI cards (first occurrence wins).
    let mut seen = std::collections::HashSet::new();
    let mut sources = Vec::new();
    for hit in &rows {
        if seen.insert(hit.transcript_id.clone()) {
            sources.push(json!({
                "transcript_id": hit.transcript_id,
                "title": hit.title,
                "url": hit.url,
                "start_time": hit.start_time,
            }));
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "answer": answer, "sources": sources })),
    )
}

#[derive(serde::Deserialize)]
pub struct FlashcardsBody {
    pub transcript: String,
    #[serde(default)]
    pub title: String,
}

/// POST /api/flashcards — generate study flashcards from a transcript.
/// Auth-required; free (generated on demand when the user opens Flashcards).
pub async fn flashcards(
    AuthUser(_claims): AuthUser,
    Json(body): Json<FlashcardsBody>,
) -> (StatusCode, Json<Value>) {
    if body.transcript.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing transcript" })),
        );
    }
    match generate_flashcards(&body.transcript, &body.title).await {
        Ok(cards) => (StatusCode::OK, Json(json!({ "flashcards": cards }))),
        Err(e) => {
            error!("flashcards failed: {e:#}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "flashcards failed, try again" })),
            )
        }
    }
}

const DEVICE_ID_HEADER: &str = "x-device-id";

/// Public wrapper so the Stripe checkout handler resolves the same identity
/// (authenticated account — JWT required) as the job handlers, ensuring a
/// purchase credits the account the signed-in user is actually using.
pub(crate) async fn resolve_identity_pub(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, Json<Value>)> {
    resolve_identity(state, headers).await
}

/// Resolve the ledger identity for a request. **Requires a valid Supabase
/// JWT** — the anonymous `X-Device-Id` fallback was removed (2026-07) because
/// it let anyone run paid transcriptions and spend credits with nothing but a
/// self-generated device id and no account. Transcription is a paid,
/// account-gated action, so identity here is always an authenticated account.
///
/// - **Valid `Authorization: Bearer …`** → `user:<sub>` account key. This is
///   the path every signed-in client (web + extension) takes.
/// - **Missing / malformed / invalid / expired token** → 401. We never fall
///   back to a device identity — that fallback was the security hole.
///
/// Device ids still exist, but ONLY as a migration source: `POST
/// /api/auth/claim` reads `X-Device-Id` directly (alongside a required JWT) to
/// fold any legacy anonymous balance into the account on first sign-in.
async fn resolve_identity(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, Json<Value>)> {
    let auth_value = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing Authorization header" })),
            )
        })?;
    let token = crate::auth::extract_bearer_token(auth_value).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "malformed Authorization header" })),
        )
    })?;
    match crate::auth::verify_jwt(token, &state.jwks).await {
        Ok(claims) => Ok(credits::account_key(&claims.sub)),
        Err(e) => {
            tracing::debug!("auth token rejected: {:#}", e);
            Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid or expired token" })),
            ))
        }
    }
}

fn payment_required(balance: i32) -> (StatusCode, Json<Value>) {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(json!({
            "error": "out of credits",
            "balance": balance,
            "checkout_endpoint": "/api/checkout",
        })),
    )
}

/// One client-produced transcript segment (from in-browser Whisper).
#[derive(Debug, serde::Deserialize)]
pub struct ClientSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct FromTranscriptBody {
    pub transcript: String,
    #[serde(default)]
    pub segments: Vec<ClientSegment>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub platform: Option<String>,
    /// Source URL (for URL Free mode) — so the note dedupes by URL and its
    /// timestamps deep-link to the video. Empty for file uploads.
    #[serde(default)]
    pub url: Option<String>,
}

/// Free-mode notes handoff. The browser already transcribed (Whisper WASM), so
/// this runs ONLY the LLM summary+diagram step and **charges 0 credits** (we
/// never call `credits::reserve`). Auth is REQUIRED so the fair-use daily cap
/// can bite — it's a free LLM call, the one abuse vector. Synchronous: there's
/// no slow work, so we return the `JobResult` directly instead of job/poll.
///
/// Route is `POST /api/from-transcript` — NOT under `/jobs`, which would collide
/// with `/jobs/{id}` (that only allows GET/DELETE → 405).
pub async fn from_transcript(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<FromTranscriptBody>,
) -> (StatusCode, Json<Value>) {
    // Cap the payload — this is a free LLM call, so bound how much work one
    // request can trigger. ~200k chars is several hours of speech.
    const MAX_TRANSCRIPT_CHARS: usize = 200_000;

    let transcript = body.transcript.trim();
    if transcript.is_empty() || body.segments.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "empty transcript" })),
        );
    }
    if transcript.len() > MAX_TRANSCRIPT_CHARS {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "transcript too large" })),
        );
    }

    // Fair-use daily cap (free feature). Atomic upsert-and-count. FAIL-CLOSED:
    // this is a free, uncapped LLM+diagram call if the cap can't be enforced, so
    // a DB error (or a missing usage table) refuses the request rather than
    // waving it through. Requires the `from_transcript_usage` migration to be
    // deployed — without it this endpoint returns 503. (Contrast `library_ask`,
    // which fails open because one stray extra question is cheap; this isn't.)
    if let Some(pool) = state.credits.pool() {
        let cap: i32 = std::env::var("FROM_TRANSCRIPT_DAILY_CAP")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(20);
        let used: i32 = match sqlx::query_scalar(
            "INSERT INTO public.from_transcript_usage (user_id, day, count) \
             VALUES ($1::uuid, current_date, 1) \
             ON CONFLICT (user_id, day) \
             DO UPDATE SET count = from_transcript_usage.count + 1 \
             RETURNING count",
        )
        .bind(&claims.sub)
        .fetch_one(pool)
        .await
        {
            Ok(n) => n,
            Err(e) => {
                error!("from_transcript cap check failed for {}: {e:#}", claims.sub);
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "error": "Free-mode notes are temporarily unavailable — please try again shortly, or use Fast mode."
                    })),
                );
            }
        };
        if used > cap {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": format!("Daily limit reached ({cap} free notes/day). Resets tomorrow.")
                })),
            );
        }
    }

    // Client segments → engine `Segment`.
    let segments: Vec<Segment> = body
        .segments
        .iter()
        .map(|s| Segment {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            text: s.text.clone(),
        })
        .collect();

    // No video source — synthesize metadata from what the client sent.
    let duration = body
        .duration
        .unwrap_or_else(|| segments.last().map(|s| s.end_ms / 1000).unwrap_or(0));
    let metadata = VideoMetadata {
        video_id: String::new(),
        title: body
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "Local transcription".to_string()),
        channel: String::new(),
        duration,
        upload_date: String::new(),
        platform: body
            .platform
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "upload".to_string()),
        url: body
            .url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_default(),
    };

    // The reused pipeline tail (same calls as `run_pipeline`, minus transcribe).
    let llm = match summarize_and_diagram(transcript, &segments, &metadata).await {
        Ok(l) => l,
        Err(e) => {
            error!("from_transcript LLM step failed: {:#}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("{e:#}") })),
            );
        }
    };

    // Best-effort embeddings for library search — a failure won't fail the note.
    let chunks = build_transcript_chunks(&segments).await;

    let result = JobResult {
        transcript: transcript.to_string(),
        segments,
        metadata,
        summary_md: llm.summary_md,
        mermaid_src: llm.mermaid_src,
        key_points: llm.key_points,
        key_point_times: llm
            .key_point_times
            .iter()
            .map(|t| t.max(0.0).round() as i64)
            .collect(),
        model_used: "browser-whisper".to_string(),
        chunks,
    };

    (
        StatusCode::OK,
        Json(serde_json::to_value(result).unwrap_or_else(|_| json!({}))),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct FetchAudioBody {
    pub url: String,
}

/// Percent-encode a string so it's safe (and Unicode-preserving) in an HTTP
/// header value — titles are often non-ASCII (e.g. Vietnamese). The client
/// decodes with `decodeURIComponent`.
fn header_pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// URL Free mode. Download a video's audio via yt-dlp and return it downsampled
/// to 16kHz mono (tiny egress) so the BROWSER can run Whisper locally — browsers
/// can't run yt-dlp themselves. 0 credits; auth required; duration-capped to
/// bound egress + in-browser transcribe time. Metadata rides in headers so the
/// browser can post it to /api/from-transcript for notes.
pub async fn fetch_audio(
    State(state): State<AppState>,
    AuthUser(_claims): AuthUser,
    Json(body): Json<FetchAudioBody>,
) -> Response {
    // Cap the audio length: bounds egress AND how long the browser grinds on it.
    const MAX_DURATION_SECS: u64 = 60 * 60;

    let url = body.url.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a valid http(s) URL is required" })),
        )
            .into_response();
    }

    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{e}") })),
            )
                .into_response();
        }
    };
    let dir = tmp.path().to_string_lossy().to_string();

    let (metadata, bytes) = {
        let eng = state.engine.lock().await;
        // Reject oversized videos BEFORE the expensive download — a cheap
        // --dump-json probe first, so a 2-hour video doesn't waste bandwidth+CPU.
        match eng.probe_metadata(&url).await {
            Ok(meta) if meta.duration > MAX_DURATION_SECS => {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({
                        "error": "Video is too long for Free mode (max 60 min). Use Fast mode."
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) => {
                error!("fetch_audio probe failed for {url}: {e:#}");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("Couldn't read video info: {e:#}") })),
                )
                    .into_response();
            }
        }
        match eng.fetch_audio_16k(&url, &dir).await {
            Ok(r) => r,
            Err(e) => {
                error!("fetch_audio failed for {url}: {e:#}");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("Couldn't fetch audio: {e:#}") })),
                )
                    .into_response();
            }
        }
    };

    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/mpeg")
        .header("X-Whisgram-Title", header_pct(&metadata.title))
        .header("X-Whisgram-Duration", metadata.duration.to_string())
        .header("X-Whisgram-Platform", header_pct(&metadata.platform))
        .header("X-Whisgram-Url", header_pct(&metadata.url))
        .header(
            "Access-Control-Expose-Headers",
            "X-Whisgram-Title, X-Whisgram-Duration, X-Whisgram-Platform, X-Whisgram-Url",
        )
        .body(Body::from(bytes))
    {
        Ok(r) => r,
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "failed to build audio response" })),
        )
            .into_response(),
    }
}

pub async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<JobRequest>,
) -> (StatusCode, Json<Value>) {
    let device_id = match resolve_identity(&state, &headers).await {
        Ok(id) => id,
        Err(e) => return e,
    };

    let job_id = Uuid::new_v4();
    let now = now_unix();
    let cancel = CancellationToken::new();

    // Atomic dedup + slot claim. Under a single jobs-lock we either return an
    // existing in-flight job for this (identity, url) or insert this one to
    // claim the slot — so two concurrent requests (the client's cross-tab race)
    // can't both create a job. Prevents the double charge AND two concurrent
    // yt-dlp downloads racing YouTube's throttling.
    {
        let mut store = state.jobs.lock().await;
        if let Some(existing) = store.values().find(|j| {
            j.device_id == device_id
                && j.url == req.url
                && matches!(
                    j.status,
                    JobStatus::Queued
                        | JobStatus::Downloading
                        | JobStatus::Transcribing
                        | JobStatus::Summarizing
                )
        }) {
            let id = existing.id;
            info!("Reusing in-flight job {} for url {} (server dedup)", id, req.url);
            return (StatusCode::ACCEPTED, Json(json!({ "job_id": id })));
        }
        store.insert(
            job_id,
            Job {
                id: job_id,
                status: JobStatus::Queued,
                url: req.url.clone(),
                device_id: device_id.clone(),
                created_at: now,
                updated_at: now,
                metadata: None,
                result: None,
                error: None,
                cancel: cancel.clone(),
            },
        );
    }

    // Reserve a credit; roll back the just-claimed job if the caller is out of
    // credits. Reserve is atomic — concurrent requests can't both pass at
    // balance=1. Refunded later if the pipeline ends in Failed or Cancelled.
    if credits::reserve(&state.credits, &device_id).await.is_err() {
        state.jobs.lock().await.remove(&job_id);
        return payment_required(0);
    }

    info!("Created job {} for url {}", job_id, req.url);

    let store = state.jobs.clone();
    let engine = state.engine.clone();
    let credit_store = state.credits.clone();
    let permits = state.pipeline_permits.clone();
    tokio::spawn(async move {
        run_with_cancel(
            job_id,
            req,
            engine,
            store,
            credit_store,
            device_id,
            cancel,
            permits,
        )
        .await
    });

    (StatusCode::ACCEPTED, Json(json!({ "job_id": job_id })))
}

/// GET /api/balance — returns the caller's credit balance for their
/// authenticated account (JWT required; 401 otherwise). Initialises to
/// FREE_TIER_CREDITS the first time an account is seen.
pub async fn get_balance(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    let id = match resolve_identity(&state, &headers).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    let bal = credits::balance(&state.credits, &id).await;
    (StatusCode::OK, Json(json!({ "balance": bal })))
}

/// POST /api/auth/claim — one-time account bootstrap on first sign-in.
///
/// Requires a valid JWT (via the `AuthUser` extractor). The client optionally
/// passes its legacy `X-Device-Id` so we can migrate any anonymous balance
/// into the freshly-signed-in account. Returns the resulting balance plus a
/// human-readable note about what happened (migrated vs seeded).
pub async fn claim_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    AuthUser(claims): AuthUser,
) -> (StatusCode, Json<Value>) {
    // Device id is optional here — a brand-new user on a fresh browser won't
    // have one, and that's fine (they just get the free tier).
    let device_id = headers
        .get(DEVICE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && is_valid_device_id(s));

    let outcome = credits::claim_account(&state.credits, &claims.sub, device_id).await;
    let (balance, note) = match outcome {
        credits::ClaimOutcome::AlreadyClaimed { balance } => {
            (balance, "already claimed".to_string())
        }
        credits::ClaimOutcome::Migrated { from_device, balance } => (
            balance,
            format!("migrated {from_device} credits from this device"),
        ),
        credits::ClaimOutcome::Seeded { balance } => {
            (balance, format!("welcome — {balance} free credits to start"))
        }
    };
    (
        StatusCode::OK,
        Json(json!({ "balance": balance, "note": note })),
    )
}

pub async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Job>, StatusCode> {
    let store = state.jobs.lock().await;
    store
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Best-effort cancellation. Idempotent: hitting cancel on a completed,
/// failed, or already-cancelled job is fine (returns the current status).
/// Calling .cancel() on a token whose select! arm has already resolved is a
/// no-op, so there's no risk of clobbering a Complete result.
pub async fn cancel_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, StatusCode> {
    let store = state.jobs.lock().await;
    let job = store.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    job.cancel.cancel();
    info!("Cancel signalled for job {} (current status: {:?})", id, job.status);
    Ok(Json(json!({ "ok": true, "status": job.status })))
}

pub async fn upload_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> (StatusCode, Json<Value>) {
    let device_id = match resolve_identity(&state, &headers).await {
        Ok(id) => id,
        Err(e) => return e,
    };

    // Reserve credit BEFORE accepting the upload — refusing late wastes the
    // user's upload bandwidth, but we don't want to commit Modal cost before
    // the gate check. This is the right place.
    if credits::reserve(&state.credits, &device_id).await.is_err() {
        return payment_required(0);
    }

    let cancel = CancellationToken::new();
    let mut saved_path: Option<PathBuf> = None;
    // RAII guard around the upload's tempdir. When this is dropped — at the
    // end of the spawned pipeline task — the tempdir + file are wiped. This
    // is what prevents `/tmp/transcriber-upload-*` from accumulating across
    // jobs. Held in the outer scope so the early-exit error paths drop it
    // promptly too.
    let mut saved_tempdir: Option<TempDir> = None;
    let mut original_filename: Option<String> = None;
    let mut model_str: Option<String> = None;
    let mut language: Option<String> = None;

    // Stream each field. The "file" field gets streamed to disk so we don't
    // hold a multi-GB upload in RAM.
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return bad_request(&format!("multipart error: {}", e));
            }
        };

        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                let raw_name = field
                    .file_name()
                    .unwrap_or("upload.bin")
                    .to_string();
                let safe_name = sanitize_filename(&raw_name);

                // Use a tempfile::TempDir so the directory + file are wiped
                // automatically when the spawned pipeline task ends. Prefix
                // is intentional (the boot-time sweep in main.rs looks for
                // `transcriber-upload-*` to clean up stragglers from
                // SIGKILL'd previous processes).
                let tempdir = match tempfile::Builder::new()
                    .prefix("transcriber-upload-")
                    .tempdir()
                {
                    Ok(t) => t,
                    Err(e) => return server_error(&format!("tempdir: {}", e)),
                };
                let path = tempdir.path().join(&safe_name);

                let mut file = match tokio::fs::File::create(&path).await {
                    Ok(f) => f,
                    Err(e) => return server_error(&format!("file create: {}", e)),
                };

                let mut field = field;
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if let Err(e) = file.write_all(&chunk).await {
                                return server_error(&format!("write: {}", e));
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            return bad_request(&format!("read chunk: {}", e));
                        }
                    }
                }
                if let Err(e) = file.flush().await {
                    return server_error(&format!("flush: {}", e));
                }

                original_filename = Some(raw_name);
                saved_path = Some(path);
                saved_tempdir = Some(tempdir);
            }
            "model" => model_str = field.text().await.ok(),
            "language" => language = field.text().await.ok(),
            _ => {
                // Drain unknown fields so the parser stays happy.
                let _ = field.bytes().await;
            }
        }
    }

    let path = match saved_path {
        Some(p) => p,
        None => return bad_request("missing 'file' field"),
    };
    let url = path.to_string_lossy().to_string();

    let job_id = Uuid::new_v4();
    let now = now_unix();
    let job = Job {
        id: job_id,
        status: JobStatus::Queued,
        url: url.clone(),
        device_id: device_id.clone(),
        created_at: now,
        updated_at: now,
        metadata: None,
        result: None,
        error: None,
        cancel: cancel.clone(),
    };

    {
        let mut store = state.jobs.lock().await;
        store.insert(job_id, job);
    }

    info!(
        "Created upload job {} for file {} ({})",
        job_id,
        original_filename.as_deref().unwrap_or("?"),
        url
    );

    let req = JobRequest {
        url,
        model: model_str,
        language,
    };
    let store = state.jobs.clone();
    let engine = state.engine.clone();
    let credit_store = state.credits.clone();
    let permits = state.pipeline_permits.clone();
    // Move `saved_tempdir` into the spawned task. The TempDir's Drop runs
    // when the task ends (success, failure, panic, cancellation) — at which
    // point the uploaded file and its parent directory are removed from
    // /tmp. Without the move, the TempDir would drop here at the end of
    // `upload_job`, deleting the file before the pipeline reads it.
    tokio::spawn(async move {
        let _upload_guard = saved_tempdir;
        run_with_cancel(
            job_id,
            req,
            engine,
            store,
            credit_store,
            device_id,
            cancel,
            permits,
        )
        .await
    });

    (StatusCode::ACCEPTED, Json(json!({ "job_id": job_id })))
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c => c,
        })
        .collect();
    if cleaned.trim().is_empty() {
        "upload.bin".to_string()
    } else {
        cleaned.chars().take(200).collect()
    }
}

fn bad_request(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

fn server_error(msg: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg })),
    )
}

/// Wrap the pipeline in a select! against the cancellation token. When the
/// token fires (via `DELETE /api/jobs/{id}`), the `run_pipeline` future is
/// dropped at its current `.await` — which closes any in-flight `reqwest`
/// connection (Modal whisper / OpenRouter LLM), saving the bulk of the cost.
/// One caveat: `spawn_blocking` for local whisper-rs can't be cancelled
/// cleanly, so a local-whisper job that's mid-transcription will finish its
/// compute before we mark the job cancelled. The status flip still happens,
/// so the client correctly sees Cancelled rather than Complete.
async fn run_with_cancel(
    job_id: Uuid,
    req: JobRequest,
    engine: Arc<Mutex<TranscriberEngine>>,
    store: JobStore,
    credit_store: CreditStore,
    device_id: String,
    cancel: CancellationToken,
    permits: Arc<Semaphore>,
) {
    tokio::select! {
        _ = cancel.cancelled() => {
            info!("Job {} cancelled by client", job_id);
            mark_cancelled(&store, job_id).await;
            // Refund the credit we reserved at create_job time.
            credits::refund(&credit_store, &device_id).await;
        }
        _ = async {
            // Wait for a concurrency slot. Under load, jobs queue here (staying
            // Queued) instead of spawning unbounded yt-dlp/ffmpeg and starving
            // the machine. Still cancellable via the branch above. The permit
            // is held for the whole pipeline and released on drop.
            if let Ok(_permit) = permits.acquire_owned().await {
                run_pipeline(job_id, req, engine, store.clone(), credit_store.clone(), device_id.clone()).await;
                // run_pipeline set Complete (kept the reservation) or Failed
                // (refunded inside).
            }
        } => {}
    }
}

async fn run_pipeline(
    job_id: Uuid,
    req: JobRequest,
    engine: Arc<Mutex<TranscriberEngine>>,
    store: JobStore,
    credit_store: CreditStore,
    device_id: String,
) {
    let model = parse_model(req.model.as_deref());
    let options = TranscriptionOptions {
        url: req.url.clone(),
        output_dir: get_default_output_dir().to_string_lossy().to_string(),
        model,
        language: req.language.clone(),
    };

    update_status(&store, job_id, JobStatus::Downloading).await;

    // Surface the video title mid-flight. The engine sends resolved metadata
    // down this channel the instant yt-dlp's `--dump-json` probe returns —
    // before the slow audio download + Whisper — and this task drops it onto the
    // Job so the next client poll can label the working view with the real title
    // instead of a generic "Transcribing…". Best-effort: if the job fails before
    // metadata resolves, nothing is sent and the task simply exits.
    let (metadata_tx, mut metadata_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let store = store.clone();
        tokio::spawn(async move {
            if let Some(metadata) = metadata_rx.recv().await {
                let mut store = store.lock().await;
                if let Some(job) = store.get_mut(&job_id) {
                    // Don't clobber a job that already reached a terminal state
                    // while we were waiting on the probe.
                    if job.result.is_none() && job.error.is_none() {
                        job.metadata = Some(metadata);
                        job.updated_at = now_unix();
                    }
                }
            }
        });
    }

    // The existing engine handles download → audio extraction → whisper as one call.
    // Status flips to Transcribing right before the whisper step starts inside engine.
    update_status(&store, job_id, JobStatus::Transcribing).await;
    let transcription = {
        let eng = engine.lock().await;
        eng.transcribe_reporting(options, Some(metadata_tx)).await
    };

    let transcription = match transcription {
        Ok(t) => t,
        Err(e) => {
            error!("Transcription failed for job {}: {:#}", job_id, e);
            mark_failed(&store, job_id, format!("{:#}", e)).await;
            credits::refund(&credit_store, &device_id).await;
            return;
        }
    };

    update_status(&store, job_id, JobStatus::Summarizing).await;
    let llm = match summarize_and_diagram(
        &transcription.transcript,
        &transcription.segments,
        &transcription.metadata,
    )
    .await
    {
        Ok(l) => l,
        Err(e) => {
            error!("LLM step failed for job {}: {:#}", job_id, e);
            mark_failed(&store, job_id, format!("{:#}", e)).await;
            credits::refund(&credit_store, &device_id).await;
            return;
        }
    };

    // Chunk + embed the transcript for library-wide semantic search (Phase 6).
    // Best-effort: an embedding failure must NOT fail an otherwise-good job.
    let chunks = build_transcript_chunks(&transcription.segments).await;

    let result = JobResult {
        transcript: transcription.transcript.clone(),
        segments: transcription.segments.clone(),
        metadata: transcription.metadata.clone(),
        summary_md: llm.summary_md,
        mermaid_src: llm.mermaid_src,
        key_points: llm.key_points,
        key_point_times: llm
            .key_point_times
            .iter()
            .map(|t| t.max(0.0).round() as i64)
            .collect(),
        model_used: transcription.model_used.as_str().to_string(),
        chunks,
    };

    {
        let mut store = store.lock().await;
        if let Some(job) = store.get_mut(&job_id) {
            job.status = JobStatus::Complete;
            job.result = Some(result);
            job.updated_at = now_unix();
        }
    }
    info!("Job {} complete", job_id);
}

async fn update_status(store: &JobStore, job_id: Uuid, status: JobStatus) {
    let mut store = store.lock().await;
    if let Some(job) = store.get_mut(&job_id) {
        job.status = status;
        job.updated_at = now_unix();
    }
}

async fn mark_failed(store: &JobStore, job_id: Uuid, error: String) {
    let mut store = store.lock().await;
    if let Some(job) = store.get_mut(&job_id) {
        // Don't overwrite a terminal status if the job was already cancelled
        // (e.g. cancel arrived just as the pipeline was returning an error).
        if !matches!(
            job.status,
            JobStatus::Complete | JobStatus::Failed | JobStatus::Cancelled
        ) {
            job.status = JobStatus::Failed;
            job.error = Some(error);
            job.updated_at = now_unix();
        }
    }
}

async fn mark_cancelled(store: &JobStore, job_id: Uuid) {
    let mut store = store.lock().await;
    if let Some(job) = store.get_mut(&job_id) {
        // Only flip to Cancelled if the job is still in-flight — otherwise we'd
        // clobber a Complete result that landed in the race window between the
        // pipeline finishing and the cancel arriving.
        if !matches!(
            job.status,
            JobStatus::Complete | JobStatus::Failed | JobStatus::Cancelled
        ) {
            job.status = JobStatus::Cancelled;
            job.updated_at = now_unix();
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Chunk + embed a transcript for library-wide semantic search. Best-effort:
/// on any embedding failure (or a count mismatch) it logs and returns empty, so
/// the note still saves — a backfill can re-embed it later.
async fn build_transcript_chunks(segments: &[Segment]) -> Vec<TranscriptChunk> {
    let pieces = chunk_segments(segments);
    if pieces.is_empty() {
        return Vec::new();
    }
    let texts: Vec<String> = pieces.iter().map(|p| p.content.clone()).collect();
    match embed(texts).await {
        Ok(vectors) if vectors.len() == pieces.len() => pieces
            .into_iter()
            .zip(vectors)
            .enumerate()
            .map(|(i, (p, embedding))| TranscriptChunk {
                chunk_index: i as i32,
                content: p.content,
                start_time: p.start_time,
                embedding,
            })
            .collect(),
        Ok(vectors) => {
            warn!(
                "Embedding count mismatch ({} chunks, {} vectors) — note saved without chunks",
                pieces.len(),
                vectors.len()
            );
            Vec::new()
        }
        Err(e) => {
            warn!("Chunk embedding failed (note saved, not yet searchable): {e:#}");
            Vec::new()
        }
    }
}

/// Periodically evict terminal (Complete/Failed/Cancelled) jobs older than a
/// TTL from the in-memory store, so long-lived instances don't leak memory as
/// finished jobs accumulate. In-flight jobs are always kept; recently-finished
/// ones stay long enough for late polls and client resumes.
pub fn spawn_job_gc(jobs: JobStore) {
    const TTL_SECS: i64 = 3600; // keep finished jobs ~1h for late polls/resumes
    const INTERVAL_SECS: u64 = 600; // sweep every 10 min
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(INTERVAL_SECS));
        loop {
            ticker.tick().await;
            let now = now_unix();
            let mut store = jobs.lock().await;
            let before = store.len();
            store.retain(|_, j| {
                matches!(
                    j.status,
                    JobStatus::Queued
                        | JobStatus::Downloading
                        | JobStatus::Transcribing
                        | JobStatus::Summarizing
                ) || now.saturating_sub(j.updated_at) < TTL_SECS
            });
            let removed = before - store.len();
            if removed > 0 {
                info!(
                    "Job GC: evicted {} stale job(s), {} remain",
                    removed,
                    store.len()
                );
            }
        }
    });
}
