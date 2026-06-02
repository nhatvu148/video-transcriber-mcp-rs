use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::transcriber::types::VideoMetadata;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEFAULT_MODEL: &str = "anthropic/claude-haiku-4-5";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResult {
    pub summary_md: String,
    pub mermaid_src: String,
    pub key_points: Vec<String>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    error: Option<OpenRouterError>,
}

#[derive(Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: String,
}

#[derive(Deserialize, Debug)]
struct OpenRouterError {
    message: String,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

const SYSTEM_PROMPT: &str = "You are a study-note generator for technical learners (CS/ML students, engineers, researchers).

Given the transcript of an educational video, produce three things:
1. A clear, well-structured Markdown summary with headings, bullet lists, and code blocks or LaTeX formulas where relevant. Aim for the kind of note a serious learner would keep in their Obsidian vault.
2. A Mermaid diagram in `flowchart TD`, `sequenceDiagram`, `classDiagram`, or `mindmap` syntax (pick whichever best illustrates the content) that visualizes the key concepts and how they relate.
3. 3-7 single-sentence key takeaways.

Respond with ONLY a JSON object, no preamble, no explanation, no markdown fences. The exact shape is:
{\"summary_md\": \"...\", \"mermaid_src\": \"...\", \"key_points\": [\"...\", \"...\"]}

Hard rules:
- `mermaid_src` must contain ONLY the diagram code (no ```mermaid fences, no surrounding text).
- `mermaid_src` must be syntactically valid Mermaid — no stray characters, no half-closed brackets.
- `summary_md` is free-form Markdown but must NOT contain a top-level title (the caller adds one).
- Use ASCII-safe node IDs in the diagram (alphanumeric + underscore); put any natural-language labels in the brackets.";

pub async fn summarize_and_diagram(
    transcript: &str,
    metadata: &VideoMetadata,
) -> Result<LlmResult> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY environment variable is required")?;
    let model =
        std::env::var("LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    let user_msg = format!(
        "Video title: {}\nChannel: {}\nPlatform: {}\nDuration: {}s\n\n--- TRANSCRIPT ---\n{}\n--- END TRANSCRIPT ---\n\nGenerate the JSON now.",
        metadata.title, metadata.channel, metadata.platform, metadata.duration, transcript
    );

    let req = ChatRequest {
        model: &model,
        max_tokens: 8192,
        messages: vec![
            ChatMessage {
                role: "system",
                content: SYSTEM_PROMPT,
            },
            ChatMessage {
                role: "user",
                content: &user_msg,
            },
        ],
    };

    info!(
        "Calling OpenRouter ({} chars transcript, model={})",
        transcript.len(),
        model
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(OPENROUTER_URL)
        .bearer_auth(api_key)
        // Optional but recommended by OpenRouter for ranking/analytics.
        .header("HTTP-Referer", "https://github.com/nhatvu148/video-transcriber-mcp-rs")
        .header("X-Title", "video-transcriber-mcp")
        .header("content-type", "application/json")
        .json(&req)
        .send()
        .await
        .context("OpenRouter request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenRouter returned {}: {}", status, body);
    }

    let api_resp: ChatResponse = resp
        .json()
        .await
        .context("Failed to parse OpenRouter response")?;

    // OpenRouter sometimes returns 200 with an error body (e.g. credits exhausted).
    if let Some(err) = api_resp.error {
        anyhow::bail!("OpenRouter error: {} ({:?})", err.message, err.code);
    }

    let raw_text = api_resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("OpenRouter response had no choices")?;

    let json_str = strip_code_fences(raw_text.trim());

    let result: LlmResult = serde_json::from_str(json_str).with_context(|| {
        format!(
            "Failed to parse LLM JSON output. Raw response was:\n{}",
            raw_text
        )
    })?;

    info!(
        "LLM call complete: {} key points, {} char summary, {} char mermaid",
        result.key_points.len(),
        result.summary_md.len(),
        result.mermaid_src.len()
    );

    Ok(result)
}

fn strip_code_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .map(str::trim_start)
        .unwrap_or(s);
    s.strip_suffix("```").map(str::trim_end).unwrap_or(s)
}
