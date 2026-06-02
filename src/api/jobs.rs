use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobRequest {
    pub url: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobResult {
    pub transcript: String,
    pub segments: Vec<Segment>,
    pub metadata: VideoMetadata,
    pub summary_md: String,
    pub mermaid_src: String,
    pub key_points: Vec<String>,
    pub model_used: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: Uuid,
    pub status: JobStatus,
    pub url: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn parse_model(s: Option<&str>) -> WhisperModel {
    s.and_then(|m| m.parse::<WhisperModel>().ok())
        .unwrap_or(WhisperModel::Base)
}
