use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::transcriber::types::{Segment, VideoMetadata};

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
    /// Seconds into the video where each key point is primarily discussed,
    /// parallel to `key_points`. `f64` (not i64) so a stray float from the LLM
    /// doesn't fail the whole parse; `default` so its absence is harmless
    /// (older prompts / models simply omit it → non-clickable takeaways).
    #[serde(default)]
    pub key_point_times: Vec<f64>,
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
2. A Mermaid diagram (default to `flowchart TD` — top-down — for both narrative pipelines and hierarchies; use `flowchart LR` only for genuinely short flows of at most 3-4 nodes; `sequenceDiagram` only for explicit step-by-step interactions, `mindmap` only for purely associative content) that visualizes how the key concepts relate.
3. 3-7 single-sentence key takeaways.

Respond with ONLY a JSON object, no preamble, no explanation, no markdown fences. The exact shape is:
{\"summary_md\": \"...\", \"mermaid_src\": \"...\", \"key_points\": [\"...\", \"...\"], \"key_point_times\": [12, 84]}

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
- Choose the direction by shape, NOT by default. Use `flowchart TD` (top-down) for any linear/sequential process with more than 3 stages — a left-to-right (`LR`) chain of many stages renders as one unreadably wide row that shrinks to tiny nodes. Reserve `flowchart LR` for short flows (at most 3-4 nodes across). When in doubt, prefer `TD`.
- Group related nodes into `subgraph Name [Display Label] ... end` blocks. Aim for 2-4 subgraphs in any non-trivial diagram so the structure is scannable at a glance.
- Use shape variety to signal node type. Pick EXACTLY ONE shape per node — never combine or nest them (e.g. `[((label))]` is invalid and a parse error):
  ((label))    core concept / final outcome
  [[label]]    process / mechanism / subroutine
  {label}      decision / open question
  [/label/]    input / data source
  [label]      default / generic node
  Inside node labels, avoid the characters `(` `)` `[` `]` `{` `}` `|` unless they are part of a quoted string — they confuse the shape parser. If a label genuinely needs special characters, wrap the entire label text in double quotes (e.g. Node[\"text with (parens)\"]).
- Highlight the 1-3 MOST important nodes by appending EXACTLY two lines at the end of the diagram (after all node and edge declarations). Format:
    classDef key fill:#7C3AED,stroke:#5B21B6,color:#fff,stroke-width:2px
    class NodeA,NodeB key
  Strict syntax rules — getting these wrong breaks the diagram parser:
  * No semicolons at the end of either line.
  * The `class` line MUST end with the class name token (`key`). Omitting it (e.g. `class NodeA,NodeB`) is a parse error.
  * Use the exact word `key` as the class name — don't rename it.
  Use this sparingly — if everything is highlighted, nothing stands out.
- Add edge labels (`A -->|how A leads to B| B`) where the connection isn't obvious from the node names alone. Keep edge labels to 4 words or fewer — long labels collide with nearby nodes.
- Edges MUST connect two specific nodes (e.g. `NodeA --> NodeB`). NEVER point an edge at a subgraph name, and NEVER draw an edge from a node into the subgraph that contains it — Mermaid lays those out poorly and the labels overlap other nodes. To link groups, connect a representative node in one subgraph to a representative node in another.
- Write the diagram with raw characters: use `-->` (not `--&gt;`) and `&` (not `&amp;`). Do NOT HTML-escape any part of the diagram source.

Length budget (CRITICAL — exceeding this truncates the response):
- `summary_md`: aim for 400–900 words. A digestible study note, not a transcript rewrite. Prefer tight bullets over long paragraphs. Reserve headings only for genuinely distinct sections.
- `mermaid_src`: 8–15 nodes total (across all subgraphs). 25+ nodes is unreadable.
- `key_points`: 3–7 items, each one sentence.
- `key_point_times`: an array of integers, the SAME length and order as `key_points`. Each value is the start-time in seconds — the number in the square brackets at the start of the transcript line — nearest to where that takeaway is primarily discussed. Copy the bracketed number from the most relevant line. If the transcript has no bracketed times, return an empty array.
- TOTAL output must fit in roughly 4,000 words. If the source video is long-form, ruthlessly compress — capture the structure and key insights, not every example.";

pub async fn summarize_and_diagram(
    transcript: &str,
    segments: &[Segment],
    metadata: &VideoMetadata,
) -> Result<LlmResult> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY environment variable is required")?;
    let model =
        std::env::var("LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    // Feed a time-marked transcript so the model can cite where each takeaway
    // is discussed (for key_point_times). Falls back to plain text if there
    // are no segment timings.
    let source = timestamped_transcript(segments, transcript);

    let user_msg = format!(
        "Video title: {}\nChannel: {}\nPlatform: {}\nDuration: {}s\n\nThe transcript is split into lines, each prefixed with its start time in seconds in square brackets, e.g. `[123] ...`. Use those numbers for key_point_times.\n\n--- TRANSCRIPT ---\n{}\n--- END TRANSCRIPT ---\n\nGenerate the JSON now.",
        metadata.title, metadata.channel, metadata.platform, metadata.duration, source
    );

    // Retry loop: malformed-JSON responses come back ~1-2% of the time on
    // long transcripts. A fresh sampling pass (different RNG seed inside the
    // model) almost always returns valid JSON on the next attempt. Network
    // and API errors are NOT retried — only JSON parse errors, where retry
    // is meaningful.
    let mut last_parse_err: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_LLM_ATTEMPTS {
        match call_llm_once(&api_key, &model, &user_msg, source.len()).await {
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

/// Prefix each transcript line with its start time in seconds, e.g. `[123] ...`,
/// so the LLM can cite where each takeaway is discussed. Falls back to the plain
/// transcript when segment timings aren't available.
fn timestamped_transcript(segments: &[Segment], fallback: &str) -> String {
    if segments.is_empty() {
        return fallback.to_string();
    }
    let mut out = String::with_capacity(fallback.len() + segments.len() * 8);
    for seg in segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        let secs = seg.start_ms / 1000;
        out.push_str(&format!("[{secs}] {text}\n"));
    }
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
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

/// Answer a follow-up question about a video, grounded ONLY in its transcript.
/// Powers the "Chat with the video" feature. `history` is the prior turns as
/// (role, content) pairs, where role is "user" or "assistant". Returns the
/// assistant's answer text. Free feature — the per-video question cap is
/// enforced client-side; this endpoint just answers.
pub async fn chat_about_transcript(
    transcript: &str,
    title: &str,
    history: &[(String, String)],
    question: &str,
) -> Result<String> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY environment variable is required")?;
    let model =
        std::env::var("LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    // Bound the transcript we send so a very long video can't blow up token
    // cost. ~48k chars ≈ 12k tokens of context, plenty for grounded Q&A.
    let ctx = truncate_chars(transcript, 48_000);

    let system = format!(
        "You answer questions about one specific video, using ONLY the transcript below. \
Be concise, direct, and helpful. If the answer isn't covered in the transcript, say it \
isn't discussed in the video rather than guessing or using outside knowledge.\n\n\
Video title: {title}\n--- TRANSCRIPT ---\n{ctx}\n--- END TRANSCRIPT ---"
    );

    let mut messages: Vec<serde_json::Value> =
        vec![serde_json::json!({ "role": "system", "content": system })];
    for (role, content) in history {
        let r = if role == "assistant" { "assistant" } else { "user" };
        messages.push(serde_json::json!({ "role": r, "content": content }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": question }));

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1024,
        "messages": messages,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(OPENROUTER_URL)
        .bearer_auth(&api_key)
        .header("HTTP-Referer", "https://github.com/nhatvu148/video-transcriber-mcp-rs")
        .header("X-Title", "video-transcriber-mcp")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("OpenRouter chat request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let b = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenRouter returned {}: {}", status, b);
    }

    let api_resp: ChatResponse = resp
        .json()
        .await
        .context("Failed to parse OpenRouter chat response")?;
    if let Some(err) = api_resp.error {
        anyhow::bail!("OpenRouter error: {} ({:?})", err.message, err.code);
    }

    let answer = api_resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default();
    Ok(answer.trim().to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flashcard {
    pub question: String,
    pub answer: String,
}

/// Generate study flashcards (question/answer pairs) from a transcript. On-demand
/// feature (client calls it when the user opens Flashcards), so it's a separate
/// LLM call rather than part of the main summarize step.
pub async fn generate_flashcards(transcript: &str, title: &str) -> Result<Vec<Flashcard>> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY environment variable is required")?;
    let model =
        std::env::var("LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let ctx = truncate_chars(transcript, 48_000);

    let system = "You create study flashcards from a video transcript. \
Output ONLY a JSON array of objects, each exactly {\"question\": \"...\", \"answer\": \"...\"}. \
Produce 8-15 cards that test understanding of the key ideas (not trivia). \
Questions are clear and self-contained; answers are concise (1-3 sentences). \
Base everything strictly on the transcript. No preamble, no explanation, no markdown fences.";

    let user = format!(
        "Video title: {title}\n\n--- TRANSCRIPT ---\n{ctx}\n--- END TRANSCRIPT ---\n\nGenerate the flashcards JSON array now."
    );

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 2048,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(OPENROUTER_URL)
        .bearer_auth(&api_key)
        .header("HTTP-Referer", "https://github.com/nhatvu148/video-transcriber-mcp-rs")
        .header("X-Title", "video-transcriber-mcp")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("OpenRouter flashcards request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let b = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenRouter returned {}: {}", status, b);
    }

    let api_resp: ChatResponse = resp
        .json()
        .await
        .context("Failed to parse OpenRouter flashcards response")?;
    if let Some(err) = api_resp.error {
        anyhow::bail!("OpenRouter error: {} ({:?})", err.message, err.code);
    }
    let raw = api_resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default();

    let cleaned = strip_code_fences(&raw);
    let cards: Vec<Flashcard> =
        serde_json::from_str(cleaned).context("flashcards response was not valid JSON")?;
    Ok(cards)
}

/// Truncate to at most `max` chars on a char boundary, appending a marker.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n[...transcript truncated...]");
    out
}
