//! Video transcription over the Model Context Protocol.
//!
//! Two things live here and nothing else: the transcription pipeline
//! (yt-dlp → ffmpeg → whisper.cpp) and an MCP server exposing it over stdio or
//! streamable HTTP.
//!
//! The product built on top of this — REST API, accounts, credits, payments,
//! the AI study layer — is deliberately *not* here. It lives in a separate
//! private crate that depends on this one, so installing this gets a
//! transcription server rather than somebody's SaaS backend.
pub mod embeddings;
pub mod mcp;
pub mod transcriber;
pub mod url_guard;
pub mod utils;

pub use transcriber::types::Segment;
pub use transcriber::{TranscriberEngine, TranscriptionOptions, WhisperModel};
