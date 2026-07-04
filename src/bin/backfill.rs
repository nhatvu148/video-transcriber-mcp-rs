//! One-time backfill: embed EXISTING transcripts into `transcript_chunks` so
//! Phase 6 library search covers notes created before embedding existed. New
//! transcriptions self-index; this catches everything already in the table.
//!
//! Idempotent — only touches transcripts that have no chunks yet, so it's safe
//! to re-run (e.g. after adding more notes, or if a run was interrupted).
//!
//! Run once, with your engine's env:
//!   DATABASE_URL='postgres://…' OPENROUTER_API_KEY='sk-…' \
//!     cargo run --release --bin backfill

use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use video_transcriber_mcp::Segment;
use video_transcriber_mcp::llm::{chunk_segments, embed};

/// The subset of the saved `data` (JobResult) JSONB we need.
#[derive(Deserialize)]
struct TranscriptData {
    #[serde(default)]
    segments: Vec<Segment>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let db_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL is required (point it at your Supabase Postgres)")?;
    std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY is required (for embeddings)")?;

    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&db_url)
        .await
        .context("connecting to Postgres")?;

    let rows = sqlx::query(
        "SELECT t.id::text AS id, t.user_id::text AS user_id, t.title, t.data \
         FROM public.transcripts t \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM public.transcript_chunks c WHERE c.transcript_id = t.id \
         ) \
         ORDER BY t.created_at",
    )
    .fetch_all(&pool)
    .await
    .context("querying transcripts without chunks")?;

    println!("Backfilling {} transcript(s) without embeddings…", rows.len());
    let (mut embedded, mut skipped) = (0usize, 0usize);

    for row in rows {
        let id: String = row.get("id");
        let user_id: String = row.get("user_id");
        let title: Option<String> = row.get("title");
        let data: serde_json::Value = row.get("data");
        let label = title.as_deref().unwrap_or("(untitled)");

        let parsed: TranscriptData = match serde_json::from_value(data) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("  skip {id}: unreadable data ({e})");
                skipped += 1;
                continue;
            }
        };
        let pieces = chunk_segments(&parsed.segments);
        if pieces.is_empty() {
            eprintln!("  skip {id}: no segments — {label}");
            skipped += 1;
            continue;
        }
        let texts: Vec<String> = pieces.iter().map(|p| p.content.clone()).collect();
        let vectors = match embed(texts).await {
            Ok(v) if v.len() == pieces.len() => v,
            Ok(v) => {
                eprintln!(
                    "  skip {id}: embed count mismatch ({} vs {})",
                    v.len(),
                    pieces.len()
                );
                skipped += 1;
                continue;
            }
            Err(e) => {
                eprintln!("  skip {id}: embed failed ({e:#})");
                skipped += 1;
                continue;
            }
        };

        let mut ok = true;
        for (i, (piece, vec)) in pieces.iter().zip(vectors).enumerate() {
            let res = sqlx::query(
                "INSERT INTO public.transcript_chunks \
                 (transcript_id, user_id, chunk_index, content, start_time, embedding) \
                 VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6::vector)",
            )
            .bind(&id)
            .bind(&user_id)
            .bind(i as i32)
            .bind(&piece.content)
            .bind(piece.start_time)
            .bind(vector_literal(&vec))
            .execute(&pool)
            .await;
            if let Err(e) = res {
                eprintln!("  chunk insert failed for {id}: {e}");
                ok = false;
                break;
            }
        }
        if ok {
            embedded += 1;
            println!("  ✓ {}… {} chunk(s) — {label}", &id[..8], pieces.len());
        } else {
            skipped += 1;
        }
    }

    println!("Done. Embedded {embedded}, skipped {skipped}.");
    Ok(())
}

/// Format an embedding as a pgvector text literal: `[0.1,0.2,…]`.
fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}
