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

    let req = EmbeddingRequest { model, input: texts };
    let client = reqwest::Client::new();
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
