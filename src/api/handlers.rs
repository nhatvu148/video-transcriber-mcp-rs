use axum::{
    Json,
    extract::{Multipart, Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use uuid::Uuid;

use crate::api::jobs::{Job, JobRequest, JobResult, JobStatus, JobStore, parse_model};
use crate::llm::summarize_and_diagram;
use crate::transcriber::{TranscriberEngine, TranscriptionOptions};
use crate::utils::paths::get_default_output_dir;

#[derive(Clone)]
pub struct AppState {
    pub jobs: JobStore,
    pub engine: Arc<Mutex<TranscriberEngine>>,
}

pub async fn create_job(
    State(state): State<AppState>,
    Json(req): Json<JobRequest>,
) -> (StatusCode, Json<Value>) {
    let job_id = Uuid::new_v4();
    let now = now_unix();
    let cancel = CancellationToken::new();

    let job = Job {
        id: job_id,
        status: JobStatus::Queued,
        url: req.url.clone(),
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
    tokio::spawn(async move { run_with_cancel(job_id, req, engine, store, cancel).await });

    (StatusCode::ACCEPTED, Json(json!({ "job_id": job_id })))
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
    mut multipart: Multipart,
) -> (StatusCode, Json<Value>) {
    let cancel = CancellationToken::new();
    let mut saved_path: Option<PathBuf> = None;
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

                // /tmp/transcriber-uploads/<uuid>/<original-filename>
                // The uuid-scoped directory avoids collisions; keeping the
                // original filename inside gives the engine a clean
                // file_stem to use as the transcript title.
                let dir = std::env::temp_dir()
                    .join("transcriber-uploads")
                    .join(Uuid::new_v4().to_string());
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    return server_error(&format!("mkdir failed: {}", e));
                }
                let path = dir.join(&safe_name);

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
    tokio::spawn(async move { run_with_cancel(job_id, req, engine, store, cancel).await });

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
    cancel: CancellationToken,
) {
    tokio::select! {
        _ = cancel.cancelled() => {
            info!("Job {} cancelled by client", job_id);
            mark_cancelled(&store, job_id).await;
        }
        _ = run_pipeline(job_id, req, engine, store.clone()) => {
            // run_pipeline already set Complete or Failed on the job.
        }
    }
}

async fn run_pipeline(
    job_id: Uuid,
    req: JobRequest,
    engine: Arc<Mutex<TranscriberEngine>>,
    store: JobStore,
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
