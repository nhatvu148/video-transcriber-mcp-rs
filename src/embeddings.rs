//! Text embeddings, used by the `search_transcripts` MCP tool.
//!
//! Lives here rather than with the rest of the AI layer because
//! `search_transcripts` is part of the MCP surface: it searches transcripts
//! this server produced and stored. Everything else that calls an LLM —
//! summaries, diagrams, chat, flashcards — is product and lives in the private
//! backend crate.
//!
//! Degrades rather than fails: without `OPENROUTER_API_KEY` the tool reports
//! that it cannot embed instead of erroring the session.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::transcriber::types::Segment;

const OPENROUTER_EMBEDDINGS_URL: &str = "https://openrouter.ai/api/v1/embeddings";

const DEFAULT_EMBEDDING_MODEL: &str = "openai/text-embedding-3-small";

#[derive(Serialize)]
struct EmbeddingRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

pub async fn embed(texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY environment variable is required")?;
    let model = std::env::var("EMBEDDING_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EMBEDDING_MODEL.to_string());

    let req = EmbeddingRequest {
        model,
        input: texts,
    };
    // reqwest has NO default request timeout, so a stalled connection waits
    // forever. Callers treat embedding failure as best-effort and carry on, but
    // a hang gives them nothing to carry on from — it just blocks. That is not
    // hypothetical: an untimed OpenRouter client in the same pipeline hung a
    // paid job for eight hours on 2026-08-10.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("build embeddings HTTP client")?;
    let resp = client
        .post(OPENROUTER_EMBEDDINGS_URL)
        .bearer_auth(&api_key)
        .header(
            "HTTP-Referer",
            "https://github.com/nhatvu148/video-transcriber-mcp-rs",
        )
        .header("X-Title", "video-transcriber-mcp")
        .header("content-type", "application/json")
        .json(&req)
        .send()
        .await
        .context("OpenRouter embeddings request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenRouter embeddings returned {}: {}", status, body);
    }

    let mut parsed: EmbeddingResponse = resp
        .json()
        .await
        .context("Failed to parse embeddings response")?;
    // OpenAI returns in input order, but sort by index defensively.
    parsed.data.sort_by_key(|d| d.index);
    Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

/// A transcript passage + its embedding — the unit of semantic search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedChunk {
    pub chunk_index: i32,
    pub content: String,
    pub start_time: Option<f64>,
    pub embedding: Vec<f32>,
}

/// A transcript passage ready to embed.
pub struct ChunkText {
    pub content: String,
    /// Seconds into the video for the passage's first segment.
    pub start_time: Option<f64>,
}

/// Split transcript segments into ~500-token passages (~2000 chars) for
/// embedding, preserving each passage's start time for citations. Groups whole
/// segments so a passage never splits mid-sentence.
pub fn chunk_segments(segments: &[Segment]) -> Vec<ChunkText> {
    const MAX_CHARS: usize = 2000; // ~500 tokens at ~4 chars/token
    let mut chunks = Vec::new();
    let mut buf = String::new();
    let mut start: Option<f64> = None;
    for seg in segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        if start.is_none() {
            start = Some(seg.start_ms as f64 / 1000.0);
        }
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(text);
        if buf.len() >= MAX_CHARS {
            chunks.push(ChunkText {
                content: std::mem::take(&mut buf),
                start_time: start.take(),
            });
        }
    }
    if !buf.trim().is_empty() {
        chunks.push(ChunkText {
            content: buf,
            start_time: start,
        });
    }
    chunks
}

/// Chunk a transcript and embed every passage. Shared by the REST pipeline and
/// the local MCP save path so both write the same searchable format.
pub async fn embed_chunks(segments: &[Segment]) -> Result<Vec<EmbeddedChunk>> {
    let pieces = chunk_segments(segments);
    if pieces.is_empty() {
        return Ok(Vec::new());
    }
    let texts: Vec<String> = pieces.iter().map(|p| p.content.clone()).collect();
    let vectors = embed(texts).await?;
    if vectors.len() != pieces.len() {
        anyhow::bail!(
            "embedding count mismatch ({} chunks, {} vectors)",
            pieces.len(),
            vectors.len()
        );
    }
    Ok(pieces
        .into_iter()
        .zip(vectors)
        .enumerate()
        .map(|(i, (p, embedding))| EmbeddedChunk {
            chunk_index: i as i32,
            content: p.content,
            start_time: p.start_time,
            embedding,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_ms: u64, text: &str) -> Segment {
        Segment {
            start_ms,
            end_ms: start_ms + 1000,
            text: text.to_string(),
        }
    }

    #[test]
    fn no_segments_produce_no_chunks() {
        assert!(chunk_segments(&[]).is_empty());
    }

    #[test]
    fn blank_segments_are_skipped_and_do_not_start_a_chunk() {
        let chunks = chunk_segments(&[seg(0, "   "), seg(1000, ""), seg(2000, "hello")]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "hello");
        // The start time comes from the first non-blank segment, not segment 0.
        assert_eq!(chunks[0].start_time, Some(2.0));
    }

    #[test]
    fn short_segments_are_joined_into_a_single_chunk() {
        let chunks = chunk_segments(&[seg(0, "hello"), seg(1000, "world")]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "hello world");
        assert_eq!(chunks[0].start_time, Some(0.0));
    }

    #[test]
    fn a_chunk_flushes_once_it_reaches_the_char_budget() {
        // Each segment is 1000 chars, so the first two alone (2000 chars)
        // already hit MAX_CHARS and should flush as their own chunk.
        let long_a = "a".repeat(1000);
        let long_b = "b".repeat(1000);
        let chunks = chunk_segments(&[seg(0, &long_a), seg(5000, &long_b), seg(9000, "tail")]);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].content.starts_with('a') && chunks[0].content.contains('b'));
        assert_eq!(chunks[0].start_time, Some(0.0));
        assert_eq!(chunks[1].content, "tail");
        assert_eq!(chunks[1].start_time, Some(9.0));
    }
}
