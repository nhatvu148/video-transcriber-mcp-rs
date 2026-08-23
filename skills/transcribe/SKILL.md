---
name: transcribe
description: Transcribe a video (YouTube, Vimeo, TikTok, X/Twitter, Twitch, Facebook, 1000+ yt-dlp sites, or a local video file) with whisper.cpp and answer questions about what was said. Also searches across every transcript already saved — use it for "what did that video say about X" and "which of my videos mentioned Y".
argument-hint: "<video-url-or-path> [question]"
allowed-tools: Read, Bash, mcp__video-transcriber
homepage: https://github.com/nhatvu148/video-transcriber-mcp-rs
license: MIT OR Apache-2.0
---

Transcribe the given video and answer the user's question about it — or summarize it if no question was asked.

Transcription is local and offline: whisper.cpp runs on this machine, and nothing but the video download leaves it.

## Pick the right tool

The `video-transcriber` MCP server is auto-registered by this plugin. Route by what the user actually wants:

| Situation | Tool |
| :--- | :--- |
| A URL or file path, needs transcribing | `transcribe_video` |
| "What did that video say about X" across past videos | `search_transcripts` |
| "The one I just did" / no URL given | `get_latest_transcript` |
| "What have I transcribed?" | `list_transcripts` |
| Something failed and the cause is unclear | `check_dependencies` |
| "Does it support <site>?" | `list_supported_sites` |

**Do not re-transcribe.** Before calling `transcribe_video` on a URL, check `list_transcripts` — transcription is minutes of compute, and the transcript may already be on disk. Transcripts are saved to `~/Downloads/video-transcripts/` by default.

## Transcribing

`transcribe_video` takes `url` (required), plus `model`, `language`, and `output_dir`.

**Model choice matters more than anything else here:**

| Model | Use when |
| :--- | :--- |
| `tiny` / `base` | Default. Clear speech, English, you want an answer now. `base` is the default. |
| `small` / `medium` | Accented speech, domain jargon, multiple speakers, or the `base` pass came back garbled. |
| `large` | Accuracy actually matters — quotes, names, numbers you'll act on. Slowest by a wide margin. |

Set `language` to an ISO 639-1 code (`en`, `vi`, `es`, `ja`) when you know it. `auto` is the default and it does misdetect on short or music-heavy clips — an explicit code is both faster and more accurate.

The response carries the transcript inline, truncated for long videos, along with the paths to the saved `.txt`, `.json`, and `.md`. **When the transcript is truncated and the question needs the whole thing, `Read` the `.txt` path** rather than transcribing again.

Cite timestamps as `M:SS` when the user asks about a specific moment — the `.json` file holds per-segment start times.

## Searching across transcripts

`search_transcripts` is semantic, not keyword — it embeds the query and returns the most relevant passages, each with its source video and timestamp, across everything saved. Reach for it whenever the question spans more than one video, or when the user can't remember which video said something.

It only covers transcripts that were saved **with embeddings**, which requires `OPENROUTER_API_KEY` to have been set at transcription time. If it returns nothing and the user expected hits, that's the likely reason — say so rather than reporting "not found". Transcripts saved before the key was configured need re-transcribing to become searchable.

## Prerequisites and failure modes

The server is a native binary; it needs three things on PATH:

- **`video-transcriber-mcp`** — `brew install nhatvu148/tap/video-transcriber-mcp` (pulls in the rest), or `cargo install video-transcriber-mcp`.
- **`yt-dlp`** — required for platform URLs. Local files don't need it.
- **`ffmpeg`** — required for local files and for audio extraction.

Whisper models download on first use into `~/.cache/video-transcriber-mcp/models/`. The first run with a given model is slow for that reason and not because anything is wrong — `large` is several GB.

Run `check_dependencies` when a transcription fails for an unclear reason; it reports all three plus which models are present.

Two failures worth naming precisely rather than passing through raw:

- **Age-restricted or login-gated video** — yt-dlp needs cookies. Tell the user to set `YT_DLP_COOKIES_FROM_BROWSER` to their browser (`firefox`, `chrome`, `brave`), or `YT_DLP_COOKIES` to a `cookies.txt` path.
- **Empty transcript on a video that clearly has speech** — usually a wrong `language`, not a broken transcription. Retry with an explicit code before escalating the model.

## Housekeeping

`cleanup_old_transcripts` (by age in days), `delete_transcript` (by video ID), and `delete_all_transcripts` exist for disk management. These destroy files. Confirm with the user before calling any of them, and never call `delete_all_transcripts` on your own initiative.
