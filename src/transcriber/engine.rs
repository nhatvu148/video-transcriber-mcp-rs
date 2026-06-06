use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;

use super::audio::AudioProcessor;
use super::downloader::VideoDownloader;
use super::types::{
    OutputFiles, TranscriptionOptions, TranscriptionResult, VideoMetadata, WhisperModel,
};
use super::whisper::WhisperTranscriber;

pub struct TranscriberEngine {
    whisper: WhisperTranscriber,
    downloader: VideoDownloader,
    audio_processor: AudioProcessor,
}

impl Default for TranscriberEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriberEngine {
    pub fn new() -> Self {
        Self {
            whisper: WhisperTranscriber::new(),
            downloader: VideoDownloader::new(),
            audio_processor: AudioProcessor::new(),
        }
    }

    pub async fn transcribe(&self, options: TranscriptionOptions) -> Result<TranscriptionResult> {
        info!("🎬 Starting transcription for: {}", options.url);

        // Create output directory
        std::fs::create_dir_all(&options.output_dir)
            .context("Failed to create output directory")?;

        // Determine if URL or local file
        let is_local = !options.url.starts_with("http://") && !options.url.starts_with("https://");

        let (metadata, audio_path) = if is_local {
            info!("📂 Processing local video file");
            let audio_path = self.process_local_video(&options.url).await?;
            let metadata = self.get_local_metadata(&options.url)?;
            (metadata, audio_path)
        } else {
            info!("🌐 Downloading video from URL");
            // yt-dlp already extracts audio to mp3 (-x --audio-format mp3),
            // so the returned path IS the audio. No need to re-run ffmpeg here;
            // whisper.rs converts to 16kHz mono PCM in one shot.
            let (metadata, audio_path) = self.downloader.download(&options.url).await?;
            (metadata, audio_path)
        };

        info!(
            "🎤 Transcribing audio with Whisper ({:?} model)...",
            options.model
        );
        let (transcript, segments) = self
            .whisper
            .transcribe(&audio_path, options.model, options.language.as_deref())
            .await?;

        // Save output files
        let files =
            self.save_outputs(&metadata, &transcript, &options.output_dir, options.model)?;

        // Calculate stats
        let word_count = transcript.split_whitespace().count();
        let transcript_preview = if transcript.len() > 500 {
            // Walk back from byte 500 to the nearest char boundary. Languages
            // with multi-byte UTF-8 sequences (Vietnamese, Chinese, Arabic…)
            // will land mid-character at a raw byte 500 and panic the slice.
            let mut end = 500;
            while !transcript.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &transcript[..end])
        } else {
            transcript.clone()
        };

        info!(
            "✅ Transcription complete! ({} segments)",
            segments.len()
        );

        Ok(TranscriptionResult {
            success: true,
            files,
            metadata,
            transcript,
            segments,
            transcript_preview,
            word_count,
            model_used: options.model,
        })
    }

    async fn process_local_video(&self, path: &str) -> Result<PathBuf> {
        let video_path = PathBuf::from(path);
        if !video_path.exists() {
            anyhow::bail!("Video file not found: {}", path);
        }

        self.audio_processor.extract_audio(&video_path).await
    }

    fn get_local_metadata(&self, path: &str) -> Result<VideoMetadata> {
        let path = Path::new(path);
        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(VideoMetadata {
            video_id: filename.clone(),
            title: filename,
            channel: "Local File".to_string(),
            duration: 0, // We could get this from ffprobe
            upload_date: String::new(),
            platform: "Local File".to_string(),
            url: path.to_string_lossy().to_string(),
        })
    }

    fn save_outputs(
        &self,
        metadata: &VideoMetadata,
        transcript: &str,
        output_dir: &str,
        model: WhisperModel,
    ) -> Result<OutputFiles> {
        let safe_filename = sanitize_filename(&format!("{}-{}", metadata.video_id, metadata.title));

        let txt_path = Path::new(output_dir).join(format!("{}.txt", safe_filename));
        let json_path = Path::new(output_dir).join(format!("{}.json", safe_filename));
        let md_path = Path::new(output_dir).join(format!("{}.md", safe_filename));

        // Save TXT
        std::fs::write(&txt_path, transcript)?;

        // Save JSON
        let json_output = serde_json::json!({
            "metadata": metadata,
            "transcript": transcript,
            "model": model.as_str(),
        });
        std::fs::write(&json_path, serde_json::to_string_pretty(&json_output)?)?;

        // Save Markdown
        let md_content = format!(
            "# {}\n\n\
            **Video:** {}\n\
            **Platform:** {}\n\
            **Channel:** {}\n\
            **Video ID:** {}\n\
            **Duration:** {}s\n\
            **Published:** {}\n\n\
            ---\n\n\
            ## Transcript\n\n\
            {}\n\n\
            ---\n\n\
            *Transcribed using whisper.cpp (Rust) - Model: {}*\n",
            metadata.title,
            metadata.url,
            metadata.platform,
            metadata.channel,
            metadata.video_id,
            metadata.duration,
            metadata.upload_date,
            transcript,
            model.as_str()
        );
        std::fs::write(&md_path, md_content)?;

        Ok(OutputFiles {
            txt: txt_path.to_string_lossy().to_string(),
            json: json_path.to_string_lossy().to_string(),
            md: md_path.to_string_lossy().to_string(),
        })
    }

    pub fn check_dependencies(&self) -> Result<String> {
        let mut status = String::new();

        // Check yt-dlp
        match std::process::Command::new("yt-dlp")
            .arg("--version")
            .output()
        {
            Ok(_) => status.push_str("✅ yt-dlp: installed\n"),
            Err(_) => status.push_str("❌ yt-dlp: NOT installed\n"),
        }

        // Check ffmpeg
        match std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
        {
            Ok(_) => status.push_str("✅ ffmpeg: installed\n"),
            Err(_) => status.push_str("❌ ffmpeg: NOT installed\n"),
        }

        // Check whisper models
        status.push_str(&self.whisper.check_models_status());

        Ok(status)
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            _ => c,
        })
        .collect::<String>()
        .chars()
        .take(150)
        .collect()
}
