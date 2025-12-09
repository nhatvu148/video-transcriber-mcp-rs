# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2025-12-09

### Added
- **Streamable HTTP transport** for remote MCP server access (MCP protocol 2025-03-26)
  - New `--transport http` CLI option
  - Configurable `--host` and `--port` options
  - Single `/mcp` endpoint for all MCP communication
  - Session-based communication with SSE streaming support
- **CLI argument parsing** using clap for transport mode selection
- **Dual transport support**: stdio (default) and HTTP
- **Chrome extension example** for YouTube transcription
- **HTTP proxy** (Node.js) for Claude Code HTTP compatibility
- **axum** web framework (v0.8) for HTTP transport
- **Comprehensive documentation**:
  - `TESTING_HTTP.md` - HTTP testing guide
  - `WHEN_TO_USE_HTTP.md` - Transport comparison and use cases
  - `CLAUDE_CODE_HTTP_SETUP.md` - Claude Code HTTP setup
  - `CHROME_EXTENSION_VIABILITY.md` - Product strategy and market analysis
  - `PRODUCT_STRATEGY.md` - Business plan and competitive analysis
- **Test tools**:
  - Python test client (`test-mcp-client.py`)
  - Bash test script (`test-http-mcp.sh`)
  - Chrome extension (`chrome-extension-example/`)

### Changed
- Updated to support both stdio (local) and HTTP (remote) transport modes
- Added `transport-streamable-http-server` feature to rmcp (v0.10.0)
- Main entry point now accepts CLI arguments for transport selection
- Logging configuration adapts based on transport mode (ANSI colors for HTTP)

### Technical Details
- Uses rmcp v0.10.0 with Streamable HTTP transport
- Session-based architecture with LocalSessionManager
- SSE streaming for real-time responses
- Backward compatible (stdio is default)
- No breaking changes to existing stdio usage

## [0.1.2] - 2025-12-04

### Changed
- Use `env!("CARGO_PKG_VERSION")` macro for version strings (single source of truth)
- Install script now fetches latest version from GitHub API
- README badge now pulls version dynamically from crates.io

## [0.1.1] - 2025-12-04

### Changed
- Updated `rmcp` from 0.9.1 to 0.10.0
- Updated `whisper-rs` from 0.12 to 0.15.1
- Updated `thiserror` from 1.0 to 2.0
- Updated `tokio` from 1.41 to 1.48
- Updated `tempfile` from 3.13 to 3.23
- Updated `async-process` from 2.3 to 2.5

### Fixed
- Adapted to whisper-rs 0.15 API changes (`get_segment().to_str_lossy()`)

## [0.1.0] - 2025-11-26

### 🎉 First Stable Release

This release marks the first production-ready version of video-transcriber-mcp!

### Changed
- **BREAKING**: Migrated from manual JSON-RPC implementation to official `rmcp` SDK (v0.9.1)
- Renamed project from `video-transcriber-rs` to `video-transcriber-mcp` for clarity
- Server now uses `ServerHandler` trait for proper MCP integration
- Improved MCP protocol compliance and full compatibility with Claude Code

### Added
- Full support for MCP protocol version 2024-11-05
- Proper capabilities advertisement through official SDK
- Better error handling with structured ErrorData
- Comprehensive CHANGELOG documentation

### Fixed
- MCP capabilities now properly displayed in Claude Code
- Tools list correctly exposed to MCP clients (4 tools)
- Server initialization follows official MCP specification
- Switched from OpenSSL to rustls-tls for better cross-compilation support

### Features (Stable)
- ⚡ **High-performance transcription** using whisper.cpp (C++ with Rust bindings)
- 🌐 **1000+ video platforms** supported via yt-dlp
- 📁 **Local video files** transcription support
- 🛠️ **4 MCP tools**:
  - `transcribe_video`: Transcribe videos from URLs or local files
  - `check_dependencies`: Verify yt-dlp, ffmpeg, and whisper models
  - `list_supported_sites`: Show supported video platforms
  - `list_transcripts`: List previously transcribed videos
- 🎯 **Multiple Whisper models**: tiny, base, small, medium, large
- 🌍 **Multi-language support**: Auto-detect or specify language
- 📄 **Multiple output formats**: TXT, JSON, Markdown
- 🚀 **Comprehensive Taskfile** with automation tasks
- 📚 **Complete documentation** and examples
- 📦 **Standalone binary** - no Python or Node.js required

### Performance Characteristics
- Native binary with instant startup (<100ms)
- Lower memory footprint compared to Python implementations
- Binary size: 2.3MB (optimized release build)
- Performance depends on hardware and model choice
- Generally faster than Python-based Whisper implementations

### Documentation
- Complete README with installation and usage
- CLAUDE_SETUP.md for Claude Code integration
- FEATURE_PARITY.md comparing with TypeScript version
- Comprehensive Taskfile with examples
- API documentation and usage examples

## [0.1.0] - 2025-11-25 (Internal Development)

Initial development version with manual JSON-RPC implementation.

[0.2.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.2.0
[0.1.2]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.1.2
[0.1.1]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.1.1
[0.1.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.1.0
