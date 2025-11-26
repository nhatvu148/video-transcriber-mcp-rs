use anyhow::Result;
use rmcp::{
    model::*,
    service::{RequestContext, RoleServer},
    ServerHandler,
};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use crate::transcriber::{TranscriberEngine, TranscriptionOptions, WhisperModel};
use crate::utils::paths::get_default_output_dir;

#[derive(Clone)]
pub struct VideoTranscriberServer {
    transcriber: Arc<Mutex<TranscriberEngine>>,
}

impl VideoTranscriberServer {
    pub fn new() -> Self {
        Self {
            transcriber: Arc::new(Mutex::new(TranscriberEngine::new())),
        }
    }
}

impl ServerHandler for VideoTranscriberServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "High-performance video transcription server using whisper.cpp.\n\
                 Transcribes videos from 1000+ platforms or local files - 6x faster than Python whisper!"
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![
                Tool {
                    name: "transcribe_video".into(),
                    title: None,
                    description: Some("Transcribe videos from 1000+ platforms (YouTube, Vimeo, TikTok, Twitter, etc.) or local video files using whisper.cpp (4-10x faster than Python whisper!). Downloads/extracts audio and generates transcript in TXT, JSON, and Markdown formats.".into()),
                    input_schema: Arc::new(
                        serde_json::from_value(json!({
                            "type": "object",
                            "properties": {
                                "url": {
                                    "type": "string",
                                    "description": "Video URL from any supported platform OR absolute/relative path to a local video file (mp4, avi, mov, mkv, etc.)"
                                },
                                "output_dir": {
                                    "type": "string",
                                    "description": format!("Optional output directory path. Defaults to {}", get_default_output_dir().display())
                                },
                                "model": {
                                    "type": "string",
                                    "enum": ["tiny", "base", "small", "medium", "large"],
                                    "description": "Whisper model to use. Larger models are more accurate but slower. Default: 'base'"
                                },
                                "language": {
                                    "type": "string",
                                    "description": "Language code (ISO 639-1: en, es, fr, de, etc.) or 'auto' for automatic detection. Default: 'auto'"
                                }
                            },
                            "required": ["url"]
                        }))
                        .unwrap(),
                    ),
                    output_schema: None,
                    annotations: None,
                    icons: None,
                    meta: None,
                },
                Tool {
                    name: "check_dependencies".into(),
                    title: None,
                    description: Some("Check if all required dependencies (yt-dlp, ffmpeg, whisper models) are installed".into()),
                    input_schema: Arc::new(
                        serde_json::from_value(json!({
                            "type": "object",
                            "properties": {}
                        }))
                        .unwrap(),
                    ),
                    output_schema: None,
                    annotations: None,
                    icons: None,
                    meta: None,
                },
                Tool {
                    name: "list_supported_sites".into(),
                    title: None,
                    description: Some("List all video platforms supported by yt-dlp (1000+ sites including YouTube, Vimeo, TikTok, Twitter, Facebook, Instagram, educational platforms, and more)".into()),
                    input_schema: Arc::new(
                        serde_json::from_value(json!({
                            "type": "object",
                            "properties": {}
                        }))
                        .unwrap(),
                    ),
                    output_schema: None,
                    annotations: None,
                    icons: None,
                    meta: None,
                },
                Tool {
                    name: "list_transcripts".into(),
                    title: None,
                    description: Some("List all available transcripts in the output directory".into()),
                    input_schema: Arc::new(
                        serde_json::from_value(json!({
                            "type": "object",
                            "properties": {
                                "output_dir": {
                                    "type": "string",
                                    "description": format!("Optional output directory path. Defaults to {}", get_default_output_dir().display())
                                }
                            }
                        }))
                        .unwrap(),
                    ),
                    output_schema: None,
                    annotations: None,
                    icons: None,
                    meta: None,
                },
            ],
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "transcribe_video" => {
                let args = request.arguments.as_ref().ok_or_else(|| {
                    ErrorData::new(ErrorCode::INVALID_PARAMS, "Missing arguments".to_string(), None)
                })?;

                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ErrorData::new(ErrorCode::INVALID_PARAMS, "Missing 'url' parameter".to_string(), None)
                    })?
                    .to_string();

                let output_dir = args
                    .get("output_dir")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| get_default_output_dir().to_string_lossy().to_string());

                let model = args
                    .get("model")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<WhisperModel>().ok())
                    .unwrap_or(WhisperModel::Base);

                let language = args
                    .get("language")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let options = TranscriptionOptions {
                    url,
                    output_dir,
                    model,
                    language,
                };

                info!("🎬 Starting transcription...");

                let transcriber = self.transcriber.lock().await;
                match transcriber.transcribe(options).await {
                    Ok(result) => {
                        let text = format!(
                            "✅ Video transcribed successfully!\n\n\
                            **Video Details:**\n\
                            - Title: {}\n\
                            - Platform: {}\n\
                            - Duration: {}s\n\n\
                            **Transcription Settings:**\n\
                            - Model: {:?}\n\
                            - Engine: whisper.cpp (Rust)\n\n\
                            **Output Files:**\n\
                            - Text: {}\n\
                            - JSON: {}\n\
                            - Markdown: {}\n\n\
                            **Transcript Preview:**\n\
                            {}\n\n\
                            **Full transcript has {} words.**",
                            result.metadata.title,
                            result.metadata.platform,
                            result.metadata.duration,
                            result.model_used,
                            result.files.txt,
                            result.files.json,
                            result.files.md,
                            result.transcript_preview,
                            result.word_count
                        );

                        Ok(CallToolResult::success(vec![Content::text(text)]))
                    }
                    Err(e) => Err(ErrorData::new(
                        ErrorCode::INTERNAL_ERROR,
                        format!("Transcription failed: {}", e),
                        None,
                    )),
                }
            }

            "check_dependencies" => {
                let transcriber = self.transcriber.lock().await;
                match transcriber.check_dependencies() {
                    Ok(status) => {
                        let text = format!("✅ Dependency Check:\n\n{}", status);
                        Ok(CallToolResult::success(vec![Content::text(text)]))
                    }
                    Err(e) => Err(ErrorData::new(
                        ErrorCode::INTERNAL_ERROR,
                        format!("Dependency check failed: {}", e),
                        None,
                    )),
                }
            }

            "list_supported_sites" => {
                let text = "📺 Supported Video Platforms (1000+ total)\n\n\
                    **Popular platforms include:**\n\
                    - YouTube\n\
                    - Vimeo\n\
                    - TikTok\n\
                    - Twitter/X\n\
                    - Facebook\n\
                    - Instagram\n\
                    - Twitch\n\
                    - Dailymotion\n\
                    - Reddit\n\
                    - LinkedIn\n\
                    - Many educational and conference platforms\n\n\
                    **Total: 1000+ supported extractors**\n\n\
                    You can transcribe videos from any of these platforms!";

                Ok(CallToolResult::success(vec![Content::text(text)]))
            }

            "list_transcripts" => {
                use std::collections::HashMap;
                use std::fs;
                use std::path::PathBuf;

                let output_dir = request
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("output_dir"))
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .unwrap_or_else(get_default_output_dir);

                if !output_dir.exists() {
                    let text = format!(
                        "📂 No transcripts directory found at: {}\n\nTranscribe your first video to create it!",
                        output_dir.display()
                    );
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }

                let mut video_groups: HashMap<String, Vec<String>> = HashMap::new();

                if let Ok(entries) = fs::read_dir(&output_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

                        if filename.ends_with(".txt") || filename.ends_with(".md") {
                            let video_id =
                                filename.split('-').next().unwrap_or("unknown").to_string();
                            video_groups
                                .entry(video_id)
                                .or_insert_with(Vec::new)
                                .push(filename.to_string());
                        }
                    }
                }

                if video_groups.is_empty() {
                    let text = format!(
                        "📂 No transcripts found in {}\n\nTranscribe a video to get started!",
                        output_dir.display()
                    );
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }

                let mut list_items = Vec::new();
                for (i, (video_id, files)) in video_groups.iter().enumerate() {
                    let main_file = files
                        .iter()
                        .find(|f| f.ends_with(".txt"))
                        .unwrap_or(&files[0]);

                    let full_path = output_dir.join(main_file);

                    if let Ok(metadata) = fs::metadata(&full_path) {
                        let size_kb = metadata.len() as f64 / 1024.0;
                        let modified = metadata
                            .modified()
                            .ok()
                            .and_then(|t| {
                                use std::time::SystemTime;
                                let duration = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
                                Some(duration.as_secs())
                            })
                            .unwrap_or(0);

                        let title = main_file
                            .replace(&format!("{}-", video_id), "")
                            .replace(".txt", "")
                            .replace(".md", "")
                            .replace(".json", "")
                            .replace("-", " ");

                        let extensions: Vec<&str> = files
                            .iter()
                            .filter_map(|f| f.split('.').last())
                            .collect();

                        list_items.push(format!(
                            "{}. **{}**\n   Video ID: {}\n   Files: {} ({})\n   Size: {:.2} KB\n   Modified: {}\n   Path: {}",
                            i + 1,
                            title,
                            video_id,
                            files.len(),
                            extensions.join(", "),
                            size_kb,
                            format_timestamp(modified),
                            full_path.display()
                        ));
                    }
                }

                let text = format!(
                    "📚 Available transcripts ({} videos):\n\n{}\n\n💡 Tip: You can read any transcript by asking me to read the file path shown above.",
                    video_groups.len(),
                    list_items.join("\n\n")
                );

                Ok(CallToolResult::success(vec![Content::text(text)]))
            }

            _ => Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("Unknown tool: {}", request.name),
                None,
            )),
        }
    }
}

fn format_timestamp(timestamp: u64) -> String {
    use chrono::{DateTime, TimeZone, Utc};
    let dt: DateTime<Utc> = Utc.timestamp_opt(timestamp as i64, 0).unwrap();
    dt.format("%Y-%m-%d").to_string()
}
