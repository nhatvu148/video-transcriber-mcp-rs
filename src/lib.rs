pub mod api;
pub mod auth;
pub mod credits;
pub mod llm;
pub mod mcp;
pub mod transcriber;
pub mod utils;
// Shared by the binary (which mounts the router) and the MCP server module
// (which advertises the price), so both derive from one validated source.
pub mod x402_mcp;

pub use transcriber::types::Segment;
pub use transcriber::{TranscriberEngine, TranscriptionOptions, WhisperModel};
