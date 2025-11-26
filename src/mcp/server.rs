use anyhow::Result;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use tracing::{error, info};

use super::types::{McpRequest, McpResponse, McpTool};
use crate::transcriber::{TranscriberEngine, TranscriptionOptions, WhisperModel};
use crate::utils::paths::get_default_output_dir;

pub struct McpServer {
    transcriber: TranscriberEngine,
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            transcriber: TranscriberEngine::new(),
        }
    }

    pub async fn run(&self) -> Result<()> {
        info!("📡 MCP Server listening on stdio...");

        let stdin = io::stdin();
        let mut stdout = io::stdout();

        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            match self.handle_request(&line).await {
                Ok(response) => {
                    let response_json = serde_json::to_string(&response)?;
                    writeln!(stdout, "{}", response_json)?;
                    stdout.flush()?;
                }
                Err(e) => {
                    error!("Error handling request: {}", e);
                    let error_response = McpResponse::error(
                        None,
                        -32603,
                        format!("Internal error: {}", e),
                    );
                    let response_json = serde_json::to_string(&error_response)?;
                    writeln!(stdout, "{}", response_json)?;
                    stdout.flush()?;
                }
            }
        }

        Ok(())
    }

    async fn handle_request(&self, request_json: &str) -> Result<McpResponse> {
        let request: McpRequest = serde_json::from_str(request_json)?;

        match request.method.as_str() {
            "initialize" => self.handle_initialize(request.id),
            "tools/list" => self.handle_list_tools(request.id),
            "tools/call" => self.handle_call_tool(request.id, request.params).await,
            "resources/list" => self.handle_list_resources(request.id),
            "resources/read" => self.handle_read_resource(request.id, request.params),
            _ => Ok(McpResponse::error(
                request.id,
                -32601,
                format!("Method not found: {}", request.method),
            )),
        }
    }

    fn handle_initialize(&self, id: Option<Value>) -> Result<McpResponse> {
        let result = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "serverInfo": {
                "name": "video-transcriber-mcp",
                "version": "0.1.0"
            }
        });

        Ok(McpResponse::success(id, result))
    }

    fn handle_list_tools(&self, id: Option<Value>) -> Result<McpResponse> {
        let tools = vec![
            McpTool {
                name: "transcribe_video".to_string(),
                description: "Transcribe videos from 1000+ platforms (YouTube, Vimeo, TikTok, Twitter, etc.) or local video files using whisper.cpp (4-10x faster than Python whisper!). Downloads/extracts audio and generates transcript in TXT, JSON, and Markdown formats.".to_string(),
                input_schema: json!({
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
                }),
            },
            McpTool {
                name: "check_dependencies".to_string(),
                description: "Check if all required dependencies (yt-dlp, ffmpeg, whisper models) are installed".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            McpTool {
                name: "list_supported_sites".to_string(),
                description: "List all video platforms supported by yt-dlp (1000+ sites including YouTube, Vimeo, TikTok, Twitter, Facebook, Instagram, educational platforms, and more)".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            McpTool {
                name: "list_transcripts".to_string(),
                description: "List all available transcripts in the output directory".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "output_dir": {
                            "type": "string",
                            "description": format!("Optional output directory path. Defaults to {}", get_default_output_dir().display())
                        }
                    }
                }),
            },
        ];

        let result = json!({ "tools": tools });
        Ok(McpResponse::success(id, result))
    }

    async fn handle_call_tool(&self, id: Option<Value>, params: Option<Value>) -> Result<McpResponse> {
        let params = params.ok_or_else(|| anyhow::anyhow!("Missing params"))?;
        let name = params["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing tool name"))?;

        match name {
            "transcribe_video" => self.handle_transcribe_video(id, &params["arguments"]).await,
            "check_dependencies" => self.handle_check_dependencies(id),
            "list_supported_sites" => self.handle_list_supported_sites(id),
            "list_transcripts" => self.handle_list_transcripts(id, &params["arguments"]),
            _ => Ok(McpResponse::error(
                id,
                -32602,
                format!("Unknown tool: {}", name),
            )),
        }
    }

    async fn handle_transcribe_video(&self, id: Option<Value>, args: &Value) -> Result<McpResponse> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing 'url' parameter"))?;

        let output_dir = args["output_dir"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| get_default_output_dir().to_string_lossy().to_string());

        let model = args["model"]
            .as_str()
            .and_then(|s| s.parse::<WhisperModel>().ok())
            .unwrap_or(WhisperModel::Base);

        let language = args["language"]
            .as_str()
            .map(|s| s.to_string());

        let options = TranscriptionOptions {
            url: url.to_string(),
            output_dir,
            model,
            language,
        };

        info!("🎬 Starting transcription for: {}", url);

        match self.transcriber.transcribe(options).await {
            Ok(result) => {
                let result_json = json!({
                    "content": [{
                        "type": "text",
                        "text": format!(
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
                        )
                    }]
                });
                Ok(McpResponse::success(id, result_json))
            }
            Err(e) => {
                error!("Transcription failed: {}", e);
                Ok(McpResponse::error(
                    id,
                    -32000,
                    format!("Transcription failed: {}", e),
                ))
            }
        }
    }

    fn handle_check_dependencies(&self, id: Option<Value>) -> Result<McpResponse> {
        match self.transcriber.check_dependencies() {
            Ok(status) => {
                let result = json!({
                    "content": [{
                        "type": "text",
                        "text": format!("✅ Dependency Check:\n\n{}", status)
                    }]
                });
                Ok(McpResponse::success(id, result))
            }
            Err(e) => Ok(McpResponse::error(
                id,
                -32000,
                format!("Dependency check failed: {}", e),
            )),
        }
    }

    fn handle_list_resources(&self, id: Option<Value>) -> Result<McpResponse> {
        use std::fs;

        let output_dir = get_default_output_dir();

        if !output_dir.exists() {
            let result = json!({ "resources": [] });
            return Ok(McpResponse::success(id, result));
        }

        let mut resources = Vec::new();

        if let Ok(entries) = fs::read_dir(&output_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let filename = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");

                if filename.ends_with(".txt") || filename.ends_with(".md") || filename.ends_with(".json") {
                    if let Ok(metadata) = fs::metadata(&path) {
                        let mime_type = if filename.ends_with(".json") {
                            "application/json"
                        } else if filename.ends_with(".md") {
                            "text/markdown"
                        } else {
                            "text/plain"
                        };

                        let size_kb = metadata.len() as f64 / 1024.0;
                        let modified = metadata.modified()
                            .ok()
                            .and_then(|t| {
                                use std::time::SystemTime;
                                let duration = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
                                Some(duration.as_secs())
                            })
                            .unwrap_or(0);

                        resources.push(json!({
                            "uri": format!("file://{}", path.display()),
                            "name": filename,
                            "description": format!("Transcript file ({:.2} KB, modified {})",
                                size_kb,
                                format_timestamp(modified)
                            ),
                            "mimeType": mime_type
                        }));
                    }
                }
            }
        }

        let result = json!({ "resources": resources });
        Ok(McpResponse::success(id, result))
    }

    fn handle_read_resource(&self, id: Option<Value>, params: Option<Value>) -> Result<McpResponse> {
        use std::fs;

        let params = params.ok_or_else(|| anyhow::anyhow!("Missing params"))?;
        let uri = params["uri"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing uri parameter"))?;

        // Remove file:// prefix
        let file_path = uri.strip_prefix("file://").unwrap_or(uri);

        match fs::read_to_string(file_path) {
            Ok(content) => {
                let mime_type = if file_path.ends_with(".json") {
                    "application/json"
                } else if file_path.ends_with(".md") {
                    "text/markdown"
                } else {
                    "text/plain"
                };

                let result = json!({
                    "contents": [{
                        "uri": uri,
                        "mimeType": mime_type,
                        "text": content
                    }]
                });
                Ok(McpResponse::success(id, result))
            }
            Err(e) => Ok(McpResponse::error(
                id,
                -32000,
                format!("Failed to read file: {}", e),
            )),
        }
    }

    fn handle_list_supported_sites(&self, id: Option<Value>) -> Result<McpResponse> {
        let result = json!({
            "content": [{
                "type": "text",
                "text": "📺 Supported Video Platforms (1000+ total)\n\n\
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
                You can transcribe videos from any of these platforms!"
            }]
        });
        Ok(McpResponse::success(id, result))
    }

    fn handle_list_transcripts(&self, id: Option<Value>, args: &Value) -> Result<McpResponse> {
        use std::collections::HashMap;
        use std::fs;

        let output_dir = args["output_dir"]
            .as_str()
            .map(|s| PathBuf::from(s))
            .unwrap_or_else(|| get_default_output_dir());

        if !output_dir.exists() {
            let result = json!({
                "content": [{
                    "type": "text",
                    "text": format!("📂 No transcripts directory found at: {}\n\nTranscribe your first video to create it!", output_dir.display())
                }]
            });
            return Ok(McpResponse::success(id, result));
        }

        let mut video_groups: HashMap<String, Vec<String>> = HashMap::new();

        if let Ok(entries) = fs::read_dir(&output_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let filename = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");

                if filename.ends_with(".txt") || filename.ends_with(".md") {
                    let video_id = filename.split('-').next().unwrap_or("unknown").to_string();
                    video_groups.entry(video_id).or_insert_with(Vec::new).push(filename.to_string());
                }
            }
        }

        if video_groups.is_empty() {
            let result = json!({
                "content": [{
                    "type": "text",
                    "text": format!("📂 No transcripts found in {}\n\nTranscribe a video to get started!", output_dir.display())
                }]
            });
            return Ok(McpResponse::success(id, result));
        }

        let mut list_items = Vec::new();
        for (i, (video_id, files)) in video_groups.iter().enumerate() {
            let main_file = files.iter()
                .find(|f| f.ends_with(".txt"))
                .unwrap_or(&files[0]);

            let full_path = output_dir.join(main_file);

            if let Ok(metadata) = fs::metadata(&full_path) {
                let size_kb = metadata.len() as f64 / 1024.0;
                let modified = metadata.modified()
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

                let extensions: Vec<&str> = files.iter()
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

        let result = json!({
            "content": [{
                "type": "text",
                "text": format!("📚 Available transcripts ({} videos):\n\n{}\n\n💡 Tip: You can read any transcript by asking me to read the file path shown above.",
                    video_groups.len(),
                    list_items.join("\n\n"))
            }]
        });
        Ok(McpResponse::success(id, result))
    }
}

fn format_timestamp(timestamp: u64) -> String {
    use chrono::{DateTime, Utc, TimeZone};
    let dt: DateTime<Utc> = Utc.timestamp_opt(timestamp as i64, 0).unwrap();
    dt.format("%Y-%m-%d").to_string()
}
