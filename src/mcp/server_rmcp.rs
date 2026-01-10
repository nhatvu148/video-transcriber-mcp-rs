use anyhow::Result;
use rmcp::{
    ServerHandler,
    model::*,
    service::{RequestContext, RoleServer},
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

impl Default for VideoTranscriberServer {
    fn default() -> Self {
        Self::new()
    }
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
                    description: Some("List all available transcripts in the output directory, sorted by modification time (newest first)".into()),
                    input_schema: Arc::new(
                        serde_json::from_value(json!({
                            "type": "object",
                            "properties": {
                                "output_dir": {
                                    "type": "string",
                                    "description": format!("Optional output directory path. Defaults to {}", get_default_output_dir().display())
                                },
                                "limit": {
                                    "type": "number",
                                    "description": "Optional limit on number of transcripts to return (newest first). If not specified, returns all transcripts."
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
                Tool {
                    name: "get_latest_transcript".into(),
                    title: None,
                    description: Some("Get the path and details of the most recently created/modified transcript. Useful to avoid accidentally reading old transcripts.".into()),
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
                Tool {
                    name: "delete_transcript".into(),
                    title: None,
                    description: Some("Delete a specific transcript by video ID. This removes all associated files (txt, json, md).".into()),
                    input_schema: Arc::new(
                        serde_json::from_value(json!({
                            "type": "object",
                            "properties": {
                                "video_id": {
                                    "type": "string",
                                    "description": "The video ID of the transcript to delete (e.g., 'dQw4w9WgXcQ')"
                                },
                                "output_dir": {
                                    "type": "string",
                                    "description": format!("Optional output directory path. Defaults to {}", get_default_output_dir().display())
                                }
                            },
                            "required": ["video_id"]
                        }))
                        .unwrap(),
                    ),
                    output_schema: None,
                    annotations: None,
                    icons: None,
                    meta: None,
                },
                Tool {
                    name: "cleanup_old_transcripts".into(),
                    title: None,
                    description: Some("Delete transcripts older than a specified number of days. Helps manage disk space.".into()),
                    input_schema: Arc::new(
                        serde_json::from_value(json!({
                            "type": "object",
                            "properties": {
                                "days": {
                                    "type": "number",
                                    "description": "Delete transcripts older than this many days (e.g., 30 for month-old transcripts)"
                                },
                                "output_dir": {
                                    "type": "string",
                                    "description": format!("Optional output directory path. Defaults to {}", get_default_output_dir().display())
                                }
                            },
                            "required": ["days"]
                        }))
                        .unwrap(),
                    ),
                    output_schema: None,
                    annotations: None,
                    icons: None,
                    meta: None,
                },
                Tool {
                    name: "delete_all_transcripts".into(),
                    title: None,
                    description: Some("Delete ALL transcripts in the output directory. Use with caution - this cannot be undone!".into()),
                    input_schema: Arc::new(
                        serde_json::from_value(json!({
                            "type": "object",
                            "properties": {
                                "output_dir": {
                                    "type": "string",
                                    "description": format!("Optional output directory path. Defaults to {}", get_default_output_dir().display())
                                },
                                "confirm": {
                                    "type": "boolean",
                                    "description": "Must be set to true to confirm deletion of all transcripts"
                                }
                            },
                            "required": ["confirm"]
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
            meta: None,
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
                    ErrorData::new(
                        ErrorCode::INVALID_PARAMS,
                        "Missing arguments".to_string(),
                        None,
                    )
                })?;

                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ErrorData::new(
                            ErrorCode::INVALID_PARAMS,
                            "Missing 'url' parameter".to_string(),
                            None,
                        )
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

                let limit = request
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("limit"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);

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
                                .or_default()
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

                // Collect video data with timestamps for sorting
                let mut video_data: Vec<(String, Vec<String>, u64, PathBuf)> = Vec::new();

                for (video_id, files) in video_groups.iter() {
                    let main_file = files
                        .iter()
                        .find(|f| f.ends_with(".txt"))
                        .unwrap_or(&files[0]);

                    let full_path = output_dir.join(main_file);

                    if let Ok(metadata) = fs::metadata(&full_path) {
                        let modified = metadata
                            .modified()
                            .ok()
                            .and_then(|t| {
                                use std::time::SystemTime;
                                let duration = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
                                Some(duration.as_secs())
                            })
                            .unwrap_or(0);

                        video_data.push((video_id.clone(), files.clone(), modified, full_path));
                    }
                }

                // Sort by modification time (newest first)
                video_data.sort_by(|a, b| b.2.cmp(&a.2));

                // Apply limit if specified
                let videos_to_show = if let Some(lim) = limit {
                    &video_data[..video_data.len().min(lim)]
                } else {
                    &video_data[..]
                };

                let mut list_items = Vec::new();
                for (i, (video_id, files, modified, full_path)) in videos_to_show.iter().enumerate()
                {
                    let main_file = files
                        .iter()
                        .find(|f| f.ends_with(".txt"))
                        .unwrap_or(&files[0]);

                    if let Ok(metadata) = fs::metadata(full_path) {
                        let size_kb = metadata.len() as f64 / 1024.0;

                        let title = main_file
                            .replace(&format!("{}-", video_id), "")
                            .replace(".txt", "")
                            .replace(".md", "")
                            .replace(".json", "")
                            .replace("-", " ");

                        let extensions: Vec<&str> = files
                            .iter()
                            .filter_map(|f| f.split('.').next_back())
                            .collect();

                        list_items.push(format!(
                            "{}. **{}**\n   Video ID: {}\n   Files: {} ({})\n   Size: {:.2} KB\n   Modified: {}\n   Path: {}",
                            i + 1,
                            title,
                            video_id,
                            files.len(),
                            extensions.join(", "),
                            size_kb,
                            format_timestamp(*modified),
                            full_path.display()
                        ));
                    }
                }

                let total_count = video_data.len();
                let showing_count = videos_to_show.len();

                let summary = if showing_count < total_count {
                    format!(
                        "showing {} most recent out of {} total",
                        showing_count, total_count
                    )
                } else {
                    format!("{} videos", total_count)
                };

                let text = format!(
                    "📚 Available transcripts ({}):\n\n{}\n\n💡 Tip: You can read any transcript by asking me to read the file path shown above.",
                    summary,
                    list_items.join("\n\n")
                );

                Ok(CallToolResult::success(vec![Content::text(text)]))
            }

            "get_latest_transcript" => {
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
                                .or_default()
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

                // Find the most recent transcript
                let mut latest_transcript: Option<(String, Vec<String>, u64, PathBuf)> = None;

                for (video_id, files) in video_groups.iter() {
                    let main_file = files
                        .iter()
                        .find(|f| f.ends_with(".txt"))
                        .unwrap_or(&files[0]);

                    let full_path = output_dir.join(main_file);

                    if let Ok(metadata) = fs::metadata(&full_path) {
                        let modified = metadata
                            .modified()
                            .ok()
                            .and_then(|t| {
                                use std::time::SystemTime;
                                let duration = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
                                Some(duration.as_secs())
                            })
                            .unwrap_or(0);

                        if let Some((_, _, latest_time, _)) = &latest_transcript {
                            if modified > *latest_time {
                                latest_transcript =
                                    Some((video_id.clone(), files.clone(), modified, full_path));
                            }
                        } else {
                            latest_transcript =
                                Some((video_id.clone(), files.clone(), modified, full_path));
                        }
                    }
                }

                if let Some((video_id, files, modified, full_path)) = latest_transcript {
                    let main_file = files
                        .iter()
                        .find(|f| f.ends_with(".txt"))
                        .unwrap_or(&files[0]);

                    if let Ok(metadata) = fs::metadata(&full_path) {
                        let size_kb = metadata.len() as f64 / 1024.0;

                        let title = main_file
                            .replace(&format!("{}-", video_id), "")
                            .replace(".txt", "")
                            .replace(".md", "")
                            .replace(".json", "")
                            .replace("-", " ");

                        let extensions: Vec<&str> = files
                            .iter()
                            .filter_map(|f| f.split('.').next_back())
                            .collect();

                        // Find paths for all file types
                        let txt_path = output_dir.join(
                            files
                                .iter()
                                .find(|f| f.ends_with(".txt"))
                                .unwrap_or(&files[0]),
                        );
                        let md_path = files
                            .iter()
                            .find(|f| f.ends_with(".md"))
                            .map(|f| output_dir.join(f));
                        let json_path = files
                            .iter()
                            .find(|f| f.ends_with(".json"))
                            .map(|f| output_dir.join(f));

                        let mut file_paths = format!("- Text: {}", txt_path.display());
                        if let Some(md) = md_path {
                            file_paths.push_str(&format!("\n- Markdown: {}", md.display()));
                        }
                        if let Some(json) = json_path {
                            file_paths.push_str(&format!("\n- JSON: {}", json.display()));
                        }

                        let text = format!(
                            "📄 **Latest Transcript:**\n\n\
                            **Title:** {}\n\
                            **Video ID:** {}\n\
                            **Modified:** {}\n\
                            **Size:** {:.2} KB\n\
                            **Files:** {} ({})\n\n\
                            **File Paths:**\n{}\n\n\
                            💡 Tip: Use the text file path above to read or summarize this transcript.",
                            title,
                            video_id,
                            format_timestamp(modified),
                            size_kb,
                            files.len(),
                            extensions.join(", "),
                            file_paths
                        );

                        Ok(CallToolResult::success(vec![Content::text(text)]))
                    } else {
                        Err(ErrorData::new(
                            ErrorCode::INTERNAL_ERROR,
                            "Failed to read latest transcript metadata".to_string(),
                            None,
                        ))
                    }
                } else {
                    let text = "📂 No transcripts found.".to_string();
                    Ok(CallToolResult::success(vec![Content::text(text)]))
                }
            }

            "delete_transcript" => {
                use std::fs;
                use std::path::PathBuf;

                let args = request.arguments.as_ref().ok_or_else(|| {
                    ErrorData::new(
                        ErrorCode::INVALID_PARAMS,
                        "Missing arguments".to_string(),
                        None,
                    )
                })?;

                let video_id = args
                    .get("video_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ErrorData::new(
                            ErrorCode::INVALID_PARAMS,
                            "Missing 'video_id' parameter".to_string(),
                            None,
                        )
                    })?;

                let output_dir = args
                    .get("output_dir")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .unwrap_or_else(get_default_output_dir);

                if !output_dir.exists() {
                    let text = "📂 No transcripts directory found.".to_string();
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }

                // Find all files matching the video_id
                let mut deleted_files = Vec::new();
                if let Ok(entries) = fs::read_dir(&output_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

                        if filename.starts_with(&format!("{}-", video_id))
                            && fs::remove_file(&path).is_ok()
                        {
                            deleted_files.push(path.display().to_string());
                        }
                    }
                }

                if deleted_files.is_empty() {
                    let text = format!("⚠️ No transcripts found for video ID: {}", video_id);
                    Ok(CallToolResult::success(vec![Content::text(text)]))
                } else {
                    let text = format!(
                        "🗑️ Deleted {} file(s) for video ID '{}':\n\n{}",
                        deleted_files.len(),
                        video_id,
                        deleted_files
                            .iter()
                            .map(|f| format!("- {}", f))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    Ok(CallToolResult::success(vec![Content::text(text)]))
                }
            }

            "cleanup_old_transcripts" => {
                use std::fs;
                use std::path::PathBuf;
                use std::time::{Duration, SystemTime};

                let args = request.arguments.as_ref().ok_or_else(|| {
                    ErrorData::new(
                        ErrorCode::INVALID_PARAMS,
                        "Missing arguments".to_string(),
                        None,
                    )
                })?;

                let days = args.get("days").and_then(|v| v.as_u64()).ok_or_else(|| {
                    ErrorData::new(
                        ErrorCode::INVALID_PARAMS,
                        "Missing or invalid 'days' parameter".to_string(),
                        None,
                    )
                })?;

                let output_dir = args
                    .get("output_dir")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .unwrap_or_else(get_default_output_dir);

                if !output_dir.exists() {
                    let text = "📂 No transcripts directory found.".to_string();
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }

                let cutoff_time = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
                let mut deleted_files = Vec::new();

                if let Ok(entries) = fs::read_dir(&output_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();

                        if let Ok(metadata) = fs::metadata(&path)
                            && let Ok(modified) = metadata.modified()
                            && modified < cutoff_time
                            && fs::remove_file(&path).is_ok()
                        {
                            deleted_files.push(path.display().to_string());
                        }
                    }
                }

                if deleted_files.is_empty() {
                    let text = format!("✅ No transcripts older than {} days found.", days);
                    Ok(CallToolResult::success(vec![Content::text(text)]))
                } else {
                    let text = format!(
                        "🗑️ Deleted {} file(s) older than {} days:\n\n{}",
                        deleted_files.len(),
                        days,
                        deleted_files
                            .iter()
                            .map(|f| format!("- {}", f))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    Ok(CallToolResult::success(vec![Content::text(text)]))
                }
            }

            "delete_all_transcripts" => {
                use std::fs;
                use std::path::PathBuf;

                let args = request.arguments.as_ref().ok_or_else(|| {
                    ErrorData::new(
                        ErrorCode::INVALID_PARAMS,
                        "Missing arguments".to_string(),
                        None,
                    )
                })?;

                let confirm = args
                    .get("confirm")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !confirm {
                    let text = "⚠️ Deletion not confirmed. Set 'confirm' to true to delete all transcripts.".to_string();
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }

                let output_dir = args
                    .get("output_dir")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .unwrap_or_else(get_default_output_dir);

                if !output_dir.exists() {
                    let text = "📂 No transcripts directory found.".to_string();
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }

                let mut deleted_count = 0;
                if let Ok(entries) = fs::read_dir(&output_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_file() && fs::remove_file(&path).is_ok() {
                            deleted_count += 1;
                        }
                    }
                }

                if deleted_count == 0 {
                    let text = "📂 No transcripts found to delete.".to_string();
                    Ok(CallToolResult::success(vec![Content::text(text)]))
                } else {
                    let text = format!(
                        "🗑️ Deleted ALL transcripts: {} file(s) removed from {}",
                        deleted_count,
                        output_dir.display()
                    );
                    Ok(CallToolResult::success(vec![Content::text(text)]))
                }
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
