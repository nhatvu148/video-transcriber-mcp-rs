use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::transcriber::types::{Segment, VideoMetadata, WhisperModel};

pub type JobStore = Arc<Mutex<HashMap<Uuid, Job>>>;

pub fn new_store() -> JobStore {
    Arc::new(Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Downloading,
    Transcribing,
    Summarizing,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobRequest {
    pub url: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

/// One embedded transcript passage (Phase 6 — library-wide semantic search).
/// Returned in `JobResult` so the client can write it into `transcript_chunks`
/// alongside the transcript row it owns.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptChunk {
    pub chunk_index: i32,
    pub content: String,
    /// Seconds into the video for the passage's first segment (citations).
    pub start_time: Option<f64>,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobResult {
    pub transcript: String,
    pub segments: Vec<Segment>,
    pub metadata: VideoMetadata,
    pub summary_md: String,
    pub mermaid_src: String,
    pub key_points: Vec<String>,
    /// Seconds into the video for each key point (parallel to `key_points`),
    /// so the client can make takeaways seek the player. Empty when unavailable.
    pub key_point_times: Vec<i64>,
    pub model_used: String,
    /// Embedded transcript passages for library-wide semantic search. Empty
    /// when embedding was unavailable (the note still saves; a backfill can
    /// re-embed it later).
    #[serde(default)]
    pub chunks: Vec<TranscriptChunk>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: Uuid,
    pub status: JobStatus,
    pub url: String,
    /// Device that owns this job — used to refund the credit on failure /
    /// cancellation. Not exposed to the client (different devices polling the
    /// same job_id shouldn't reveal each other's identities). Currently
    /// unread (the device_id is also threaded through the spawn closure), but
    /// kept so future admin / debug endpoints can attribute jobs.
    #[serde(skip)]
    #[allow(dead_code)]
    pub device_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Per-job cancellation signal. Cloned into the spawned pipeline task and
    /// fired by `DELETE /api/jobs/{id}`. Dropping the in-flight futures (Modal
    /// HTTP request, OpenRouter HTTP request) closes their TCP connections so
    /// the remote work stops too.
    #[serde(skip)]
    pub cancel: CancellationToken,
}

pub fn parse_model(s: Option<&str>) -> WhisperModel {
    s.and_then(|m| m.parse::<WhisperModel>().ok())
        .unwrap_or(WhisperModel::Base)
}
