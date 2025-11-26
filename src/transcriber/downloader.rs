use anyhow::{Context, Result};
use async_process::Command;
use std::path::PathBuf;
use tempfile::TempDir;
use tracing::info;

use super::types::VideoMetadata;

pub struct VideoDownloader {
    temp_dir: TempDir,
}

impl VideoDownloader {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        Self { temp_dir }
    }

    pub async fn download(&self, url: &str) -> Result<(VideoMetadata, PathBuf)> {
        info!("📥 Fetching video metadata...");
        let metadata = self.fetch_metadata(url).await?;

        info!("📺 Detected platform: {}", metadata.platform);
        info!("🎬 Title: {}", metadata.title);

        info!("⬇️  Downloading video (audio only)...");
        let video_path = self.download_audio(url).await?;

        Ok((metadata, video_path))
    }

    async fn fetch_metadata(&self, url: &str) -> Result<VideoMetadata> {
        let output = Command::new("yt-dlp")
            .args(&["--dump-json", url])
            .output()
            .await
            .context("Failed to run yt-dlp. Is it installed?")?;

        if !output.status.success() {
            anyhow::bail!(
                "yt-dlp failed to fetch metadata: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let json_str = String::from_utf8(output.stdout)?;
        let json: serde_json::Value = serde_json::from_str(&json_str)?;

        Ok(VideoMetadata {
            video_id: json["id"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            title: json["title"]
                .as_str()
                .unwrap_or("Unknown")
                .to_string(),
            channel: json["channel"]
                .as_str()
                .or_else(|| json["uploader"].as_str())
                .unwrap_or("Unknown")
                .to_string(),
            duration: json["duration"].as_u64().unwrap_or(0),
            upload_date: json["upload_date"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            platform: detect_platform(url, &json),
            url: url.to_string(),
        })
    }

    async fn download_audio(&self, url: &str) -> Result<PathBuf> {
        let output_template = self.temp_dir.path().join("video.%(ext)s");

        let output = Command::new("yt-dlp")
            .args(&[
                "-x",                        // Extract audio
                "--audio-format", "mp3",     // Convert to mp3
                "-o", output_template.to_str().unwrap(),
                url,
            ])
            .output()
            .await
            .context("Failed to run yt-dlp")?;

        if !output.status.success() {
            anyhow::bail!(
                "yt-dlp failed to download video: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Find the downloaded file
        let audio_path = self.temp_dir.path().join("video.mp3");
        if !audio_path.exists() {
            anyhow::bail!("Downloaded audio file not found");
        }

        Ok(audio_path)
    }
}

fn detect_platform(url: &str, json: &serde_json::Value) -> String {
    // Try to detect from URL first
    let url_lower = url.to_lowercase();

    if url_lower.contains("youtube.com") || url_lower.contains("youtu.be") {
        return "YouTube".to_string();
    } else if url_lower.contains("vimeo.com") {
        return "Vimeo".to_string();
    } else if url_lower.contains("tiktok.com") {
        return "TikTok".to_string();
    } else if url_lower.contains("twitter.com") || url_lower.contains("x.com") {
        return "Twitter/X".to_string();
    } else if url_lower.contains("facebook.com") || url_lower.contains("fb.watch") {
        return "Facebook".to_string();
    } else if url_lower.contains("instagram.com") {
        return "Instagram".to_string();
    } else if url_lower.contains("twitch.tv") {
        return "Twitch".to_string();
    }

    // Fallback to extractor from metadata
    json["extractor"]
        .as_str()
        .unwrap_or("Unknown")
        .to_string()
}
