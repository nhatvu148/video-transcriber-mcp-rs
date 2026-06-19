use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::transcriber::types::VideoMetadata;

/// LLM JSON parsing is non-deterministic — Claude Haiku occasionally emits
/// a response that *almost* fits the schema but has a stray escape, missing
/// quote, or trailing comma. Single attempts fail ~1-2% of the time on long
/// transcripts; retrying with a fresh sampling pass almost always succeeds.
/// We retry up to MAX_LLM_ATTEMPTS-1 times before propagating the error.
const MAX_LLM_ATTEMPTS: usize = 3;

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
2. A Mermaid diagram (default to `flowchart LR` for narrative content or `flowchart TD` for hierarchies; use `sequenceDiagram` only for explicit step-by-step interactions, `mindmap` only for purely associative content) that visualizes how the key concepts relate.
3. 3-7 single-sentence key takeaways.

Respond with ONLY a JSON object, no preamble, no explanation, no markdown fences. The exact shape is:
{\"summary_md\": \"...\", \"mermaid_src\": \"...\", \"key_points\": [\"...\", \"...\"]}

Hard rules:
- `mermaid_src` must contain ONLY the diagram code (no ```mermaid fences, no surrounding text).
- `mermaid_src` must be syntactically valid Mermaid — no stray characters, no half-closed brackets.
- `summary_md` is free-form Markdown but must NOT contain a top-level title (the caller adds one).
- Use ASCII-safe node IDs in the diagram (alphanumeric + underscore); put any natural-language labels in the brackets.

Diagram quality bar — aim for \"screenshot-worthy enough that a reader would post it on Twitter as a takeaway from the video\":
- The diagram should communicate the video's central insight at a glance. Someone who hasn't watched it should be able to look at the diagram and grasp the main argument or framework in ~10 seconds.
- Favor strong structural choices (clear hierarchy, distinct phases, decision branches) over comprehensive coverage. A vivid diagram of 8 concepts beats an exhaustive one of 20.
- The subgraph names and node labels are read first — make them concrete and concept-loaded, not generic (prefer \"Building Intuition\" over \"Phase 1\", prefer \"Gradient Descent Step\" over \"Step 2\").

When generating a `flowchart`:
- Group related nodes into `subgraph Name [Display Label] ... end` blocks. Aim for 2-4 subgraphs in any non-trivial diagram so the structure is scannable at a glance.
- Use shape variety to signal node type:
  ((label))    core concept / final outcome
  [[label]]    process / mechanism / subroutine
  {label}      decision / open question
  [/label/]    input / data source
  [label]      default / generic node
- Highlight the 1-3 MOST important nodes by appending EXACTLY two lines at the end of the diagram (after all node and edge declarations). Format:
    classDef key fill:#7C3AED,stroke:#5B21B6,color:#fff,stroke-width:2px
    class NodeA,NodeB key
  Strict syntax rules — getting these wrong breaks the diagram parser:
  * No semicolons at the end of either line.
  * The `class` line MUST end with the class name token (`key`). Omitting it (e.g. `class NodeA,NodeB`) is a parse error.
  * Use the exact word `key` as the class name — don't rename it.
  Use this sparingly — if everything is highlighted, nothing stands out.
- Add edge labels (`A -->|how A leads to B| B`) where the connection isn't obvious from the node names alone.

Length budget (CRITICAL — exceeding this truncates the response):
- `summary_md`: aim for 400–900 words. A digestible study note, not a transcript rewrite. Prefer tight bullets over long paragraphs. Reserve headings only for genuinely distinct sections.
- `mermaid_src`: 8–15 nodes total (across all subgraphs). 25+ nodes is unreadable.
- `key_points`: 3–7 items, each one sentence.
- TOTAL output must fit in roughly 4,000 words. If the source video is long-form, ruthlessly compress — capture the structure and key insights, not every example.";

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

    // Retry loop: malformed-JSON responses come back ~1-2% of the time on
    // long transcripts. A fresh sampling pass (different RNG seed inside the
    // model) almost always returns valid JSON on the next attempt. Network
    // and API errors are NOT retried — only JSON parse errors, where retry
    // is meaningful.
    let mut last_parse_err: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_LLM_ATTEMPTS {
        match call_llm_once(&api_key, &model, &user_msg, transcript.len()).await {
            Ok(result) => {
                if attempt > 1 {
                    info!("LLM call succeeded on attempt {} of {}", attempt, MAX_LLM_ATTEMPTS);
                }
                return Ok(result);
            }
            Err(LlmError::ParseError(e)) if attempt < MAX_LLM_ATTEMPTS => {
                warn!(
                    "LLM attempt {}/{} returned malformed JSON; retrying. ({})",
                    attempt, MAX_LLM_ATTEMPTS, e
                );
                last_parse_err = Some(e);
                continue;
            }
            Err(LlmError::ParseError(e)) => {
                // Last attempt's parse failure — propagate
                return Err(e);
            }
            Err(LlmError::Other(e)) => {
                // Network / API / auth error — not worth retrying
                return Err(e);
            }
        }
    }
    Err(last_parse_err
        .unwrap_or_else(|| anyhow::anyhow!("LLM exhausted retries with no recorded error")))
}

/// Inner attempt — returns a typed error so the outer loop can distinguish
/// "JSON parse failure (retry me)" from "everything else (don't retry)".
enum LlmError {
    ParseError(anyhow::Error),
    Other(anyhow::Error),
}

async fn call_llm_once(
    api_key: &str,
    model: &str,
    user_msg: &str,
    transcript_len: usize,
) -> std::result::Result<LlmResult, LlmError> {
    let req = ChatRequest {
        model,
        // 16384 gives ~12k words of headroom — enough that even a verbose
        // long-form transcript won't truncate mid-JSON like 8192 sometimes
        // did. Claude Haiku 4.5 supports much more; this is a defensive
        // ceiling. Cost impact: ~$0.02 worst-case per call vs ~$0.01 before.
        max_tokens: 16384,
        messages: vec![
            ChatMessage {
                role: "system",
                content: SYSTEM_PROMPT,
            },
            ChatMessage {
                role: "user",
                content: user_msg,
            },
        ],
    };

    info!(
        "Calling OpenRouter ({} chars transcript, model={})",
        transcript_len, model
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
        .context("OpenRouter request failed")
        .map_err(LlmError::Other)?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(LlmError::Other(anyhow::anyhow!(
            "OpenRouter returned {}: {}",
            status,
            body
        )));
    }

    let api_resp: ChatResponse = resp
        .json()
        .await
        .context("Failed to parse OpenRouter response")
        .map_err(LlmError::Other)?;

    // OpenRouter sometimes returns 200 with an error body (e.g. credits exhausted).
    if let Some(err) = api_resp.error {
        return Err(LlmError::Other(anyhow::anyhow!(
            "OpenRouter error: {} ({:?})",
            err.message,
            err.code
        )));
    }

    let raw_text = api_resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("OpenRouter response had no choices")
        .map_err(LlmError::Other)?;

    let json_str = strip_code_fences(raw_text.trim());

    let result: LlmResult = serde_json::from_str(json_str)
        .with_context(|| {
            format!(
                "Failed to parse LLM JSON output. Raw response was:\n{}",
                raw_text
            )
        })
        .map_err(LlmError::ParseError)?;

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
