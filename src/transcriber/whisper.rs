use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::info;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::types::{Segment, WhisperModel};
use crate::utils::paths::get_models_dir;

pub struct WhisperTranscriber {
    models_dir: PathBuf,
}

impl Default for WhisperTranscriber {
    fn default() -> Self {
        Self::new()
    }
}

impl WhisperTranscriber {
    pub fn new() -> Self {
        let models_dir = get_models_dir();
        std::fs::create_dir_all(&models_dir).ok();

        Self { models_dir }
    }

    /// Transcribe an audio file. Routes to a remote whisper worker if
    /// `REMOTE_WHISPER_URL` is set; otherwise falls back to local
    /// whisper-rs (blocking, run on a tokio worker thread).
    pub async fn transcribe(
        &self,
        audio_path: &Path,
        model: WhisperModel,
        language: Option<&str>,
    ) -> Result<(String, Vec<Segment>)> {
        if let Some(url) = remote_whisper_url()
            && !url.trim().is_empty()
        {
            return transcribe_remote(&url, audio_path, model, language).await;
        }

        // Local fallback — the underlying whisper-rs API is blocking, so we
        // run it on a worker thread to avoid stalling the tokio scheduler.
        let audio_path = audio_path.to_path_buf();
        let models_dir = self.models_dir.clone();
        let language = language.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            transcribe_local(&models_dir, &audio_path, model, language.as_deref())
        })
        .await
        .context("transcribe task panicked")?
    }

    pub fn check_models_status(&self) -> String {
        let mut status = String::new();
        status.push_str("📦 Whisper Models:\n");

        if remote_whisper_url().is_some() {
            status.push_str(
                "  (remote: REMOTE_WHISPER_URL is set — local models unused)\n",
            );
        }

        for model in [
            WhisperModel::Tiny,
            WhisperModel::Base,
            WhisperModel::Small,
            WhisperModel::Medium,
            WhisperModel::Large,
        ] {
            let model_path = self.models_dir.join(model.model_filename());
            if model_path.exists() {
                let size = std::fs::metadata(&model_path)
                    .map(|m| format!("{:.1} MB", m.len() as f64 / 1_000_000.0))
                    .unwrap_or_else(|_| "unknown".to_string());
                status.push_str(&format!(
                    "  ✅ {:?}: {} ({})\n",
                    model,
                    model_path.display(),
                    size
                ));
            } else {
                status.push_str(&format!("  ❌ {:?}: not installed\n", model));
            }
        }

        status
    }
}

fn remote_whisper_url() -> Option<String> {
    std::env::var("REMOTE_WHISPER_URL").ok()
}

// ---------- remote whisper-worker path ----------

#[derive(Deserialize)]
struct RemoteResponse {
    transcript: String,
    segments: Vec<RemoteSegment>,
}

#[derive(Deserialize)]
struct RemoteSegment {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

async fn transcribe_remote(
    url: &str,
    audio_path: &Path,
    model: WhisperModel,
    language: Option<&str>,
) -> Result<(String, Vec<Segment>)> {
    info!(
        "🛰  Transcribing via remote Whisper ({}): {:?}",
        url, model
    );

    let bytes = tokio::fs::read(audio_path)
        .await
        .with_context(|| format!("Failed to read audio file: {}", audio_path.display()))?;

    let filename = audio_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.mp3")
        .to_string();

    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(filename)
        .mime_str("audio/mpeg")
        .context("Failed to build multipart part")?;

    let form = reqwest::multipart::Form::new()
        .part("audio", part)
        .text("model", model.as_str().to_string())
        .text("language", language.unwrap_or("auto").to_string());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .context("Failed to build reqwest client")?;

    let resp = client
        .post(url)
        .multipart(form)
        .send()
        .await
        .context("Remote whisper POST failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Remote whisper returned {}: {}", status, body);
    }

    let r: RemoteResponse = resp
        .json()
        .await
        .context("Failed to parse remote whisper response")?;

    info!(
        "🛰  Remote transcription complete: {} segments",
        r.segments.len()
    );

    let segments = r
        .segments
        .into_iter()
        .map(|s| Segment {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            text: s.text,
        })
        .collect();

    Ok((r.transcript, segments))
}

// ---------- local (whisper-rs) path ----------

fn transcribe_local(
    models_dir: &Path,
    audio_path: &Path,
    model: WhisperModel,
    language: Option<&str>,
) -> Result<(String, Vec<Segment>)> {
    info!("Loading Whisper model: {:?}", model);

    let model_path = get_model_path(models_dir, model)?;

    let ctx = WhisperContext::new_with_params(
        model_path.to_str().unwrap(),
        WhisperContextParameters::default(),
    )
    .context("Failed to load Whisper model")?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

    if let Some(lang) = language
        && lang != "auto"
    {
        params.set_language(Some(lang));
        params.set_translate(false);
    }

    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_n_threads(optimal_whisper_threads());

    info!("Loading audio file...");
    let audio_data = load_audio_as_pcm(audio_path)?;

    info!("Transcribing... (this may take a few minutes)");
    let mut state = ctx
        .create_state()
        .context("Failed to create Whisper state")?;

    state
        .full(params, &audio_data[..])
        .context("Failed to transcribe audio")?;

    let num_segments = state.full_n_segments();

    let mut transcript = String::new();
    let mut segments = Vec::with_capacity(num_segments as usize);
    for i in 0..num_segments {
        let segment = state
            .get_segment(i)
            .context(format!("Failed to get segment {}", i))?;
        let text = segment
            .to_str_lossy()
            .context(format!("Failed to get text for segment {}", i))?
            .to_string();
        let start_ms = (segment.start_timestamp().max(0) as u64) * 10;
        let end_ms = (segment.end_timestamp().max(0) as u64) * 10;
        transcript.push_str(&text);
        transcript.push(' ');
        segments.push(Segment {
            start_ms,
            end_ms,
            text: text.trim().to_string(),
        });
    }

    Ok((transcript.trim().to_string(), segments))
}

fn get_model_path(models_dir: &Path, model: WhisperModel) -> Result<PathBuf> {
    let model_filename = model.model_filename();
    let model_path = models_dir.join(&model_filename);

    if !model_path.exists() {
        anyhow::bail!(
            "Whisper model not found: {}\n\n\
            Please download it using:\n\
              bash scripts/download-models.sh {}\n\n\
            Or download manually from:\n\
              https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            model_path.display(),
            model.as_str(),
            model_filename
        );
    }

    Ok(model_path)
}

fn load_audio_as_pcm(audio_path: &Path) -> Result<Vec<f32>> {
    info!("Converting audio to 16kHz mono PCM...");

    let output = std::process::Command::new("ffmpeg")
        .args([
            "-i",
            audio_path.to_str().unwrap(),
            "-ar",
            "16000",
            "-ac",
            "1",
            "-f",
            "f32le",
            "-",
        ])
        .output()
        .context("Failed to run ffmpeg")?;

    if !output.status.success() {
        anyhow::bail!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let bytes = output.stdout;
    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|chunk| {
            let bytes: [u8; 4] = chunk.try_into().unwrap();
            f32::from_le_bytes(bytes)
        })
        .collect();

    info!("Loaded {} audio samples", samples.len());

    Ok(samples)
}

/// On Apple Silicon, Whisper is fastest using P-cores only — letting it
/// spill onto E-cores actively slows transcription due to thread scheduling
/// disparities. We probe `sysctl hw.perflevel0.physicalcpu` (P-core count)
/// on macOS and fall back to all logical cores elsewhere.
fn optimal_whisper_threads() -> i32 {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "hw.perflevel0.physicalcpu"])
            .output()
            && let Ok(s) = String::from_utf8(out.stdout)
            && let Ok(n) = s.trim().parse::<i32>()
            && n > 0
        {
            return n;
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4)
}
