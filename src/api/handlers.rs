use axum::{
    Json,
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use uuid::Uuid;

use crate::api::jobs::{Job, JobRequest, JobResult, JobStatus, JobStore, parse_model};
use crate::auth::{AuthUser, JwksCache};
use crate::credits::{self, CreditStore, is_valid_device_id};
use crate::llm::summarize_and_diagram;
use crate::transcriber::{TranscriberEngine, TranscriptionOptions};
use crate::utils::paths::get_default_output_dir;
use axum::extract::FromRef;

#[derive(Clone)]
pub struct AppState {
    pub jobs: JobStore,
    pub engine: Arc<Mutex<TranscriberEngine>>,
    pub credits: CreditStore,
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

const DEVICE_ID_HEADER: &str = "x-device-id";

/// Public wrapper so the Stripe checkout handler resolves the same identity
/// (authenticated account preferred, else device id) as the job handlers —
/// ensuring a purchase credits the account a signed-in user is actually using.
pub(crate) async fn resolve_identity_pub(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, Json<Value>)> {
    resolve_identity(state, headers).await
}

/// Extract + validate the device id from request headers. Returns the id on
/// success or an HTTP-ready error tuple on failure.
fn require_device_id(headers: &HeaderMap) -> Result<String, (StatusCode, Json<Value>)> {
    let raw = headers
        .get(DEVICE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string());
    let id = match raw {
        Some(s) if !s.is_empty() => s,
        _ => {
            return Err(bad_request(
                "missing required header: X-Device-Id (client must generate and persist a UUID)",
            ));
        }
    };
    if !is_valid_device_id(&id) {
        return Err(bad_request(
            "invalid X-Device-Id: must be alphanumeric + dashes, ≤128 chars",
        ));
    }
    Ok(id)
}

/// Resolve the ledger identity for a request, preferring the authenticated
/// account over the legacy device id.
///
/// - **Valid `Authorization: Bearer …`** → `user:<sub>` account key. This is
///   the path the signed-in web app takes.
/// - **Authorization present but invalid/expired** → 401. We never silently
///   downgrade to the device path when a token was supplied — that would let a
///   client paper over a broken session and quietly spend device credits.
/// - **No Authorization header** → legacy `X-Device-Id` path (the extension
///   and any pre-auth client). Returns 400 if the device header is missing.
///
/// This dual path is what lets the web (authed) and extension (not yet authed)
/// share one backend during the auth rollout.
async fn resolve_identity(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, Json<Value>)> {
    if let Some(auth_value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let token = crate::auth::extract_bearer_token(auth_value).ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "malformed Authorization header" })),
            )
        })?;
        return match crate::auth::verify_jwt(token, &state.jwks).await {
            Ok(claims) => Ok(credits::account_key(&claims.sub)),
            Err(e) => {
                tracing::debug!("auth token rejected: {:#}", e);
                Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "invalid or expired token" })),
                ))
            }
        };
    }
    require_device_id(headers)
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

pub async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<JobRequest>,
) -> (StatusCode, Json<Value>) {
    let device_id = match resolve_identity(&state, &headers).await {
        Ok(id) => id,
        Err(e) => return e,
    };

    // Reserve a credit upfront. Refunded later if the pipeline ends in
    // Failed or Cancelled. Atomic — concurrent requests can't both pass at
    // balance=1.
    if credits::reserve(&state.credits, &device_id).await.is_err() {
        return payment_required(0);
    }

    let job_id = Uuid::new_v4();
    let now = now_unix();
    let cancel = CancellationToken::new();

    let job = Job {
        id: job_id,
        status: JobStatus::Queued,
        url: req.url.clone(),
        device_id: device_id.clone(),
        created_at: now,
        updated_at: now,
        result: None,
        error: None,
        cancel: cancel.clone(),
    };

    {
        let mut store = state.jobs.lock().await;
        store.insert(job_id, job);
    }

    info!("Created job {} for url {}", job_id, req.url);

    let store = state.jobs.clone();
    let engine = state.engine.clone();
    let credit_store = state.credits.clone();
    tokio::spawn(async move {
        run_with_cancel(job_id, req, engine, store, credit_store, device_id, cancel).await
    });

    (StatusCode::ACCEPTED, Json(json!({ "job_id": job_id })))
}

/// GET /api/balance — returns the caller's credit balance. Uses the
/// authenticated account when signed in, else the legacy device id.
/// Initialises to FREE_TIER_CREDITS for never-before-seen identities.
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
    // Move `saved_tempdir` into the spawned task. The TempDir's Drop runs
    // when the task ends (success, failure, panic, cancellation) — at which
    // point the uploaded file and its parent directory are removed from
    // /tmp. Without the move, the TempDir would drop here at the end of
    // `upload_job`, deleting the file before the pipeline reads it.
    tokio::spawn(async move {
        let _upload_guard = saved_tempdir;
        run_with_cancel(job_id, req, engine, store, credit_store, device_id, cancel).await
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
) {
    tokio::select! {
        _ = cancel.cancelled() => {
            info!("Job {} cancelled by client", job_id);
            mark_cancelled(&store, job_id).await;
            // Refund the credit we reserved at create_job time.
            credits::refund(&credit_store, &device_id).await;
        }
        _ = run_pipeline(job_id, req, engine, store.clone(), credit_store.clone(), device_id.clone()) => {
            // run_pipeline set Complete (kept the reservation) or Failed
            // (refunded inside).
        }
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

    // The existing engine handles download → audio extraction → whisper as one call.
    // Status flips to Transcribing right before the whisper step starts inside engine.
    update_status(&store, job_id, JobStatus::Transcribing).await;
    let transcription = {
        let eng = engine.lock().await;
        eng.transcribe(options).await
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
    let llm = match summarize_and_diagram(&transcription.transcript, &transcription.metadata).await
    {
        Ok(l) => l,
        Err(e) => {
            error!("LLM step failed for job {}: {:#}", job_id, e);
            mark_failed(&store, job_id, format!("{:#}", e)).await;
            credits::refund(&credit_store, &device_id).await;
            return;
        }
    };

    let result = JobResult {
        transcript: transcription.transcript.clone(),
        segments: transcription.segments.clone(),
        metadata: transcription.metadata.clone(),
        summary_md: llm.summary_md,
        mermaid_src: llm.mermaid_src,
        key_points: llm.key_points,
        model_used: transcription.model_used.as_str().to_string(),
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
