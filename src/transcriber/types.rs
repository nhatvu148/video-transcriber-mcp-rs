use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum WhisperModel {
    Tiny,
    Base,
    Small,
    Medium,
    Large,
}

impl FromStr for WhisperModel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "tiny" => Ok(WhisperModel::Tiny),
            "base" => Ok(WhisperModel::Base),
            "small" => Ok(WhisperModel::Small),
            "medium" => Ok(WhisperModel::Medium),
            "large" => Ok(WhisperModel::Large),
            _ => Err(anyhow::anyhow!("Invalid whisper model: {}", s)),
        }
    }
}

impl WhisperModel {
    pub fn as_str(&self) -> &str {
        match self {
            WhisperModel::Tiny => "tiny",
            WhisperModel::Base => "base",
            WhisperModel::Small => "small",
            WhisperModel::Medium => "medium",
            WhisperModel::Large => "large",
        }
    }

    pub fn model_filename(&self) -> String {
        format!("ggml-{}.bin", self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptionOptions {
    pub url: String,
    pub output_dir: String,
    pub model: WhisperModel,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoMetadata {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub duration: u64,
    pub upload_date: String,
    pub platform: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct OutputFiles {
    pub txt: String,
    pub json: String,
    pub md: String,
}

#[derive(Debug, Clone)]
pub struct TranscriptionResult {
    #[allow(dead_code)]
    pub success: bool,
    pub files: OutputFiles,
    pub metadata: VideoMetadata,
    #[allow(dead_code)]
    pub transcript: String,
    pub transcript_preview: String,
    pub word_count: usize,
    pub model_used: WhisperModel,
}
