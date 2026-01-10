use anyhow::{Context, Result};
use async_process::Command;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tracing::info;

pub struct AudioProcessor {
    temp_dir: TempDir,
}

impl Default for AudioProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioProcessor {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        Self { temp_dir }
    }

    pub async fn extract_audio(&self, video_path: &Path) -> Result<PathBuf> {
        info!("🎵 Extracting audio from video...");

        // Generate unique filename to avoid conflicts when processing multiple videos
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output_path = self
            .temp_dir
            .path()
            .join(format!("audio_{}.mp3", unique_id));

        let output = Command::new("ffmpeg")
            .args([
                "-i",
                video_path.to_str().unwrap(),
                "-vn", // No video
                "-acodec",
                "libmp3lame", // MP3 codec
                "-q:a",
                "2",  // Quality (2 is high quality)
                "-y", // Overwrite output file
                output_path.to_str().unwrap(),
            ])
            .output()
            .await
            .context("Failed to run ffmpeg. Is it installed?")?;

        if !output.status.success() {
            anyhow::bail!(
                "ffmpeg failed to extract audio: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        if !output_path.exists() {
            anyhow::bail!("Extracted audio file not found");
        }

        info!(
            "✅ Audio extracted successfully to {}",
            output_path.display()
        );

        Ok(output_path)
    }
}
