use anyhow::{Context, Result};
use async_process::Command;
use std::path::PathBuf;
use tempfile::TempDir;
use tracing::{info, warn};

use super::types::VideoMetadata;

pub struct VideoDownloader {
    temp_dir: TempDir,
}

/// Resolves the cookie source for yt-dlp from the environment, returning the
/// flag pair to inject into yt-dlp commands.
///
/// Resolution order:
/// 1. `YT_DLP_COOKIES` — path to a Netscape-format cookies file
///    (`--cookies <file>`). Use this on headless/Linux setups where the
///    browser cookie DB isn't accessible (e.g. cookies exported via a QR
///    login flow).
/// 2. `YT_DLP_COOKIES_FROM_BROWSER` — browser name to read cookies from
///    (`--cookies-from-browser <name>`), piggybacking on a logged-in session.
///
/// Either source bypasses YouTube's "Sign in to confirm you're not a bot"
/// wall and unlocks age-restricted / members-only videos.
fn cookies_args() -> Option<[String; 2]> {
    resolve_cookies_args(
        std::env::var("YT_DLP_COOKIES").ok().as_deref(),
        std::env::var("YT_DLP_COOKIES_FROM_BROWSER").ok().as_deref(),
    )
}

/// Pure resolution of the cookie flag pair, factored out of `cookies_args` so
/// the precedence rules can be unit-tested without touching process-global env.
fn resolve_cookies_args(cookies_file: Option<&str>, browser: Option<&str>) -> Option<[String; 2]> {
    // Prefer an explicit cookies file — works on headless/Linux hosts.
    if let Some(trimmed) = cookies_file.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(["--cookies".to_string(), trimmed.to_string()]);
    }
    // Fall back to reading cookies straight from a local browser.
    let trimmed = browser.map(str::trim).filter(|s| !s.is_empty())?;
    Some(["--cookies-from-browser".to_string(), trimmed.to_string()])
}

impl Default for VideoDownloader {
    fn default() -> Self {
        Self::new()
    }
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
        let mut args: Vec<String> = vec!["--dump-json".to_string()];
        if let Some(c) = cookies_args() {
            info!("Using {} {}", c[0], c[1]);
            args.extend(c);
        }
        args.push(url.to_string());

        let output = Command::new("yt-dlp")
            .args(&args)
            .output()
            .await
            .context("Failed to run yt-dlp. Is it installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Surface a hint when bot-check fires and cookies aren't configured.
            if stderr.contains("Sign in to confirm") && cookies_args().is_none() {
                warn!(
                    "YouTube triggered bot detection. Authenticate with cookies: set YT_DLP_COOKIES=/path/to/cookies.txt (a Netscape-format cookies file, e.g. exported via QR login on headless/Linux), or YT_DLP_COOKIES_FROM_BROWSER=chrome (or brave/firefox/edge) in .env."
                );
            }
            anyhow::bail!("yt-dlp failed to fetch metadata: {}", stderr);
        }

        let json_str = String::from_utf8(output.stdout)?;
        let json: serde_json::Value = serde_json::from_str(&json_str)?;

        Ok(VideoMetadata {
            video_id: json["id"].as_str().unwrap_or("unknown").to_string(),
            title: json["title"].as_str().unwrap_or("Unknown").to_string(),
            channel: json["channel"]
                .as_str()
                .or_else(|| json["uploader"].as_str())
                .unwrap_or("Unknown")
                .to_string(),
            duration: json["duration"].as_u64().unwrap_or(0),
            upload_date: json["upload_date"].as_str().unwrap_or("").to_string(),
            platform: detect_platform(url, &json),
            url: url.to_string(),
        })
    }

    async fn download_audio(&self, url: &str) -> Result<PathBuf> {
        // Generate unique filename to avoid conflicts when downloading multiple videos
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output_template = self
            .temp_dir
            .path()
            .join(format!("video_{}.%(ext)s", unique_id));
        let expected_path = self
            .temp_dir
            .path()
            .join(format!("video_{}.mp3", unique_id));

        let mut args: Vec<String> = vec![
            "-x".to_string(), // Extract audio
            "--audio-format".to_string(),
            "mp3".to_string(),
            "-o".to_string(),
            output_template.to_string_lossy().to_string(),
        ];
        if let Some(c) = cookies_args() {
            args.extend(c);
        }
        args.push(url.to_string());

        let output = Command::new("yt-dlp")
            .args(&args)
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
        if !expected_path.exists() {
            anyhow::bail!(
                "Downloaded audio file not found at {}",
                expected_path.display()
            );
        }

        info!("✅ Downloaded audio to {}", expected_path.display());

        Ok(expected_path)
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
    json["extractor"].as_str().unwrap_or("Unknown").to_string()
}

#[cfg(test)]
mod tests {
    use super::resolve_cookies_args;

    #[test]
    fn cookies_file_takes_priority_over_browser() {
        let args = resolve_cookies_args(Some("/path/cookies.txt"), Some("chrome")).unwrap();
        assert_eq!(args, ["--cookies", "/path/cookies.txt"]);
    }

    #[test]
    fn falls_back_to_browser_when_no_file() {
        let args = resolve_cookies_args(None, Some("firefox")).unwrap();
        assert_eq!(args, ["--cookies-from-browser", "firefox"]);
    }

    #[test]
    fn blank_or_whitespace_values_are_ignored() {
        // Empty/whitespace file falls through to the browser...
        let args = resolve_cookies_args(Some("   "), Some("brave")).unwrap();
        assert_eq!(args, ["--cookies-from-browser", "brave"]);
        // ...and a trimmed file path is used verbatim.
        let args = resolve_cookies_args(Some("  /c.txt  "), None).unwrap();
        assert_eq!(args, ["--cookies", "/c.txt"]);
        // Nothing set → no cookie flags at all.
        assert!(resolve_cookies_args(None, None).is_none());
        assert!(resolve_cookies_args(Some(""), Some("  ")).is_none());
    }
}
