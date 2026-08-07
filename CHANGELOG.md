# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.10.0] - 2026-08-08

### Changed

- **BREAKING: the crate is now only the MCP server and transcription
  pipeline.** The public modules `api`, `auth`, `credits`, `llm` and
  `x402_mcp` are gone — roughly 5,600 lines of REST API, Supabase auth, credit
  ledger, Stripe and x402 payments that had accumulated here. They were a
  product built on this crate, not part of it, and now live in a separate
  private crate that depends on this one as a library.

  Library consumers importing those modules must move to the new crate. The
  binary, the MCP tool surface and the transcription behaviour are unchanged —
  all nine tools work exactly as before.

  **Dependencies drop from 544 to 274.** `cargo install video-transcriber-mcp`
  no longer builds Solana, Stripe, sqlx/Postgres or JWT support to get a
  transcription server.

- `search_transcripts`' chunking and embedding moved to a new `embeddings`
  module. It is MCP surface — it searches transcripts this server produced —
  so it stayed while the rest of the AI layer left.

- `transcribe_video`'s description is no longer built by the payment layer. A
  deployment that charges for the tool supplies its own note via
  `VideoTranscriberServer::with_tool_note`, so the open-source build quotes no
  price.

### Added

- **`url_guard`: caller-supplied URLs are checked before reaching yt-dlp.**
  Refuses loopback, RFC1918, link-local (cloud metadata), carrier-grade NAT and
  the IPv6 equivalents — including v4-mapped, 6to4, Teredo, NAT64 and
  IPv4-compatible forms, each of which can smuggle an internal v4 target past a
  naive check. DNS resolution is bounded at 5s.

  Applied by `--transport http` via `VideoTranscriberServer::with_url_guard`,
  and deliberately **not** on stdio: there the caller already owns the machine,
  so refusing `http://localhost:8000/clip.mp4` would remove a legitimate use and
  prevent no attack.

  It does not stop redirects or DNS rebinding — yt-dlp resolves and follows
  those itself. See the module docs for why pinning the resolved IP is neither
  available in yt-dlp nor sufficient, and what does close it.

- `task mcp:http` drives the HTTP transport and asserts the guard is on there
  and off on stdio. Wired into `task verify`.

### Removed

- No ARM64 Linux release binary. whisper.cpp's NEON path does not compile for
  that target (#13); the job previously ran with `continue-on-error`, so
  releases either lacked the artifact or shipped an unverified one. `install.sh`
  now says so and points at `cargo install`.

## [0.9.0] - 2026-08-06

### Changed

- **All dependencies to latest stable.** Headline: **rmcp 1.7 → 3.1**, plus
  jsonwebtoken 9 → 11, sqlx 0.8 → 0.9, sha2 0.10 → 0.11, hmac 0.12 → 0.13,
  clap 4.5 → 4.6, tokio 1.52 → 1.53 and the rest of the tree.
- **MCP protocol support advances with rmcp 3.x.** The server now negotiates
  the `2026-07-28` draft protocol version when a client offers it, emitting the
  SEP-2322 `resultType` field for such peers while omitting it for older ones.
  Existing clients on `2025-06-18` and earlier are unaffected.
- Internally, `Content` became `ContentBlock` (matching the MCP 2025-11-25
  `ContentBlock` union), paginated results gained `result_type`/`ttl_ms`/
  `cache_scope`, and `ServerHandler::call_tool` now returns `CallToolResponse`.
  No change to the tool surface — all nine tools behave as before.

### Fixed

- **JWT verification panicked instead of rejecting** when built against
  jsonwebtoken 11. That release makes the crypto backend pluggable and its
  default features select *neither* provider, so signature verification
  resolved to a factory that panics. Only reachable in deployments using
  Supabase auth (`SUPABASE_URL` set); the crate now selects `aws_lc_rs`
  explicitly.
- Postgres credit ledger builds against sqlx 0.9, whose `runtime-tokio-rustls`
  feature was split into separate runtime and TLS features.

### Added

- **Test coverage for the paths that dependency upgrades break.** MCP
  end-to-end suites drive the real binary over both stdio and streamable HTTP
  (handshake on both protocol versions, tool list and schemas, tool calls,
  error paths, sessions, SSE framing, CORS); a Postgres suite exercises the
  credit ledger against a real database including a concurrency test that
  parallel reserves cannot oversell; JWT tests sign and verify genuine ES256
  and RS256 tokens; and Stripe webhook signatures are checked against a vector
  generated independently of the `hmac` crate.
- **CI** (`.github/workflows/ci.yml`) running clippy and the full suite on
  Linux against a Postgres service container, plus macOS and Windows checks.

### Note for packagers

The minimum supported Rust version is now **1.94**, raised by sqlx 0.9.

## [0.8.0] - 2026-07-05

### Added

- **Library-wide semantic search** — a "second brain" over a caller's whole
  library:
  - `POST /api/library-ask` (auth-required): embeds the question, vector-searches
    the caller's own transcript chunks in Postgres (`pgvector`), and returns a
    RAG answer that cites the source videos. Supports **multi-turn** conversation
    (`messages[]`) and optional **current-note grounding** (`transcript` +
    `title`), so one endpoint answers both "about this video" and "across
    everything". Scoped to the caller's `user_id` (the service pool bypasses
    RLS, so the filter is explicit). Fair-use daily cap via
    `LIBRARY_ASK_DAILY_CAP` (default 50).
  - Transcripts are **chunked and embedded** at transcription time (OpenRouter
    embeddings, `openai/text-embedding-3-small` by default, overridable via
    `EMBEDDING_MODEL`) and returned on `JobResult.chunks[]` for the client to
    persist. Best-effort — an embedding failure never fails the transcription.
  - **`backfill` binary** (`cargo run --release --bin backfill`) embeds existing
    transcripts that predate the feature. Idempotent.
- **Local semantic search (MCP)** — a new **`search_transcripts`** MCP tool
  cosine-searches across every saved transcript and returns the most relevant
  passages (with source video + timestamp), giving an MCP client (Claude Code)
  retrieval it can't do itself. Saved transcripts now also include Whisper
  `segments` and, when `OPENROUTER_API_KEY` is set, embedded `chunks` — matching
  the REST `JobResult` shape. Embedding is **opt-in and best-effort**, so plain
  transcription stays 100% offline by default.
- **Chat with a video** — `POST /api/chat`: multi-turn Q&A grounded only in a
  supplied transcript.
- **Flashcards** — `POST /api/flashcards`: generates study question/answer cards
  from a transcript.
- **Per-takeaway timestamps** — `JobResult.key_point_times[]` (parallel to
  `key_points`) so clients can deep-link each takeaway to its moment in the video.
- **Supabase auth + accounts** — Supabase JWT verification and `GET /api/me`;
  credit identity is now account-based, with a one-time claim that migrates a
  legacy device balance into the signed-in account.
- **Postgres credits backend** — credits move from JSON-on-disk to Postgres
  (`DATABASE_URL`), auto-migrating any existing `credits.json` balances on first
  boot. Falls back to the JSON file when `DATABASE_URL` is unset.
- **Concurrency control** — `MAX_CONCURRENT_JOBS` (default 4) bounds simultaneous
  pipelines with a semaphore so a traffic spike queues instead of overwhelming
  the host; a background GC evicts finished jobs from the in-memory store.

### Changed

- **yt-dlp throttling resilience** — downloads now prefer the `android` /
  `web_safari` player clients (`--extractor-args`) plus fragment retries, which
  sidesteps YouTube's nsig "N bytes read… giving up" throttling. Overridable via
  `YT_DLP_PLAYER_CLIENT`. Applied to both metadata and audio fetches.
- **Server-side single-flight** — `POST /api/jobs` now returns the existing
  in-flight job for the same (identity, URL) instead of creating a duplicate,
  atomically under the jobs lock. Prevents double charges and two concurrent
  yt-dlp downloads racing each other.
- **Richer, sturdier Mermaid diagram prompts** — subgraphs, shape vocabulary,
  key-node emphasis, and a preference for top-down (`flowchart TD`) pipelines for
  screenshot-worthy framing; stricter node-label rules (no shape nesting, no
  stray special characters) to keep the generated diagram valid.

### Fixed

- LLM summarisation retries up to 2× on malformed JSON before propagating the
  error.
- Upload tempdirs are wiped after each job (no `/tmp` leak on the upload path).

## [0.7.0] - 2026-06-15

### Changed

- **`rmcp` 0.12 → 1.7** — the official MCP Rust SDK reached its first stable major. The MCP protocol wire format is unchanged (existing Claude Code / Claude Desktop clients connect seamlessly), but the construction API for `Tool` and `ServerInfo` did move behind `#[non_exhaustive]`:
  - `Tool { name, description, input_schema, … }` struct expressions → `Tool::new(name, description, input_schema)` builder. Drops a lot of boilerplate per tool.
  - `ServerInfo { …, ..Default::default() }` → mutate a `ServerInfo::default()` instance.
  - `PaginatedRequestParam` and `CallToolRequestParam` are now plural (`…Params`); the old type aliases are deprecated.
- **`reqwest` 0.12 → 0.13** — `rustls-tls` feature flag renamed to `rustls`; `form` is no longer enabled by default (added explicitly).
- **`whisper-rs` 0.15 → 0.16**, **`tower-http` 0.6 → 0.7** — no source-side breakage.
- Assorted patch bumps: `axum` → 0.8.9, `tokio` → 1.52, `tempfile` → 3.27, `tower` → 0.5.3, `uuid` → 1.23.

## [0.6.0] - 2026-06-14

### Added

- **Credits / metering system** (`src/credits.rs`): per-device credit balances gate transcription jobs, with simple JSON-on-volume persistence (path configurable via `CREDITS_DB_PATH`). `GET /api/balance` reports the caller's remaining balance.
- **Stripe Checkout integration** (`src/api/stripe.rs`, env-gated):
  - `POST /api/checkout` — create a Stripe Checkout session to top up credits
  - `POST /api/webhook/stripe` — Stripe webhook that credits a device on successful payment (signature-verified)
  - Configured via `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`, `CHECKOUT_SUCCESS_URL`, `CHECKOUT_CANCEL_URL`. Entirely disabled when the keys are unset.
- **`YT_DLP_COOKIES` env var**: point the downloader at a Netscape-format cookies file (`--cookies <file>`), taking priority over `YT_DLP_COOKIES_FROM_BROWSER`. Enables authentication on headless / Linux hosts that have no local browser cookie database (e.g. cookies exported via a QR-login flow). ([#3](https://github.com/nhatvu148/video-transcriber-mcp-rs/issues/3))
- **Job cancellation**: `DELETE /api/jobs/{id}` cancels an in-flight transcription job.
- **Per-IP rate limiting** on the `/api/*` surface via `tower_governor` (steady 1 req/s, burst 20), using `SmartIpKeyExtractor` so it reads the real client IP from proxy headers (`X-Forwarded-For` / `X-Real-IP` / `Forwarded`) behind an edge proxy.

### Fixed

- Panic when slicing the transcript preview at a non-char boundary for multi-byte UTF-8 languages (Vietnamese, Chinese, Arabic, …).
- LLM summarisation: raised `max_tokens` and added a length budget in the prompt to avoid truncated summaries.
- Rate-limit key extraction now uses `SmartIpKeyExtractor` + `ConnectInfo`, so every user no longer shares a single bucket behind Fly's edge proxy (and the extractor no longer 500s on a missing peer address).

## [0.5.0] - 2026-06-02

### Added

- **REST API on the HTTP transport** for client-friendly integration:
  - `POST /api/jobs` — submit a transcription job for any yt-dlp URL or local path
  - `GET /api/jobs/{id}` — poll job status and read the full result
  - `POST /api/jobs/upload` — multipart upload of a local audio/video file
- **LLM summarisation step** (`src/llm.rs`): the REST result includes an AI-generated Markdown summary, a Mermaid diagram, and key takeaways. Calls OpenRouter (`OPENROUTER_API_KEY` required); default model `anthropic/claude-haiku-4-5`, overridable via `LLM_MODEL`.
- **Whisper segment timestamps** exposed on `JobResult.segments[]` (each with `start_ms`, `end_ms`, `text`) — enables SRT/VTT export downstream.
- **`REMOTE_WHISPER_URL` env var**: when set, audio bytes are POSTed to a remote HTTP worker (e.g. a serverless GPU) instead of running whisper-rs locally. Endpoint must accept multipart `{audio, model, language}` and return JSON `{transcript, segments[], language, duration_s}`.
- **`YT_DLP_COOKIES_FROM_BROWSER` env var**: forwards `--cookies-from-browser <name>` to yt-dlp so the downloader piggybacks on your already-signed-in browser session. Unlocks age-restricted / members-only videos and bypasses YouTube's intermittent bot-check wall.
- **Metal GPU acceleration on macOS** via target-conditional `whisper-rs` feature — `cargo build --release` now uses Metal on Apple Silicon.
- **`Cargo.lock` is now committed** for reproducible Docker / CI builds.
- **CORS** layer on the HTTP transport so browser clients (extensions, web apps) can call the REST API directly.

### Changed

- `WhisperTranscriber::transcribe` is now `async`. The local whisper-rs path runs via `tokio::task::spawn_blocking` to avoid stalling the runtime; the remote path is naturally async.
- Per-P-core thread cap on Apple Silicon (`sysctl hw.perflevel0.physicalcpu`) for faster whisper-rs CPU inference.
- Removed a redundant ffmpeg re-encode in the URL download path — `yt-dlp` already produces mp3.

## [0.4.0] - 2026-01-10

### Added

- **Transcript management tools** for better organization and cleanup:
  - `get_latest_transcript`: Get the most recently created/modified transcript
  - `delete_transcript`: Delete specific transcript by video ID (removes all files: txt, json, md)
  - `cleanup_old_transcripts`: Delete transcripts older than specified number of days
  - `delete_all_transcripts`: Delete all transcripts with confirmation requirement

### Changed

- **list_transcripts improvements**:
  - Now sorts transcripts by modification time (newest first)
  - Added optional `limit` parameter to show only N most recent transcripts
  - Shows count summary (e.g., "showing 5 most recent out of 20 total")

### Fixed

- **Critical bug**: Fixed duplicate transcript content issue
  - Audio files and downloaded videos now use unique timestamp-based filenames
  - Prevents file collisions when processing multiple videos sequentially
  - Each transcription now gets completely independent audio processing
- Added debug logging for audio extraction and download paths

## [0.3.0] - 2026-01-06

### Changed

- Updated rmcp from 0.10.0 to 0.12.0
- Updated tokio from 1.48 to 1.49
- Updated tempfile from 3.23 to 3.24
- Updated whisper-rs from 0.15.1 to 0.15

### Fixed

- Added `meta` field to `ListToolsResult` for rmcp 0.12 compatibility

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

[0.7.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.7.0
[0.6.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.6.0
[0.5.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.5.0
[0.4.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.4.0
[0.3.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.3.0
[0.2.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.2.0
[0.1.2]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.1.2
[0.1.1]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.1.1
[0.1.0]: https://github.com/nhatvu148/video-transcriber-mcp-rs/releases/tag/v0.1.0
