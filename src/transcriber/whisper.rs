use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::types::WhisperModel;
use crate::utils::paths::get_models_dir;

pub struct WhisperTranscriber {
    models_dir: PathBuf,
}

impl WhisperTranscriber {
    pub fn new() -> Self {
        let models_dir = get_models_dir();
        std::fs::create_dir_all(&models_dir).ok();

        Self { models_dir }
    }

    pub fn transcribe(
        &self,
        audio_path: &Path,
        model: WhisperModel,
        language: Option<&str>,
    ) -> Result<String> {
        info!("Loading Whisper model: {:?}", model);

        let model_path = self.get_model_path(model)?;

        // Load Whisper context
        let ctx = WhisperContext::new_with_params(
            model_path.to_str().unwrap(),
            WhisperContextParameters::default(),
        )
        .context("Failed to load Whisper model")?;

        // Configure transcription parameters
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

        // Set language if specified
        if let Some(lang) = language {
            if lang != "auto" {
                params.set_language(Some(lang));
                params.set_translate(false);
            }
        }

        // Performance optimizations
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_n_threads(num_cpus::get() as i32);

        info!("Loading audio file...");
        let audio_data = self.load_audio_as_pcm(audio_path)?;

        info!("Transcribing... (this may take a few minutes)");
        let mut state = ctx.create_state().context("Failed to create Whisper state")?;

        state
            .full(params, &audio_data[..])
            .context("Failed to transcribe audio")?;

        // Extract transcript
        let num_segments = state.full_n_segments();

        let mut transcript = String::new();
        for i in 0..num_segments {
            let segment = state
                .get_segment(i)
                .context(format!("Failed to get segment {}", i))?;
            let text = segment
                .to_str_lossy()
                .context(format!("Failed to get text for segment {}", i))?;
            transcript.push_str(&text);
            transcript.push(' ');
        }

        Ok(transcript.trim().to_string())
    }

    fn get_model_path(&self, model: WhisperModel) -> Result<PathBuf> {
        let model_filename = model.model_filename();
        let model_path = self.models_dir.join(&model_filename);

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

    fn load_audio_as_pcm(&self, audio_path: &Path) -> Result<Vec<f32>> {
        // Use ffmpeg to convert audio to 16kHz mono PCM
        // whisper.cpp expects 16kHz sample rate
        info!("Converting audio to 16kHz mono PCM...");

        let output = std::process::Command::new("ffmpeg")
            .args(&[
                "-i",
                audio_path.to_str().unwrap(),
                "-ar",
                "16000", // 16kHz sample rate
                "-ac",
                "1", // mono
                "-f",
                "f32le", // 32-bit float PCM little-endian
                "-",
            ])
            .output()
            .context("Failed to run ffmpeg")?;

        if !output.status.success() {
            anyhow::bail!(
                "ffmpeg failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Convert bytes to f32 samples
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

    pub fn check_models_status(&self) -> String {
        let mut status = String::new();
        status.push_str("📦 Whisper Models:\n");

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
                status.push_str(&format!("  ✅ {:?}: {} ({})\n", model, model_path.display(), size));
            } else {
                status.push_str(&format!("  ❌ {:?}: not installed\n", model));
            }
        }

        status
    }
}

// We need to add num_cpus to Cargo.toml
// For now, let's implement a simple fallback
mod num_cpus {
    pub fn get() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    }
}
