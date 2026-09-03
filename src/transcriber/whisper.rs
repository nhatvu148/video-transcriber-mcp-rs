use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::types::{Segment, WhisperModel};
use crate::utils::paths::get_models_dir;

pub struct WhisperTranscriber {
    models_dir: PathBuf,
}

impl Default for WhisperTranscriber {
    fn default() -> Self {
        Self::new()
    }
}

impl WhisperTranscriber {
    pub fn new() -> Self {
        let models_dir = get_models_dir();
        std::fs::create_dir_all(&models_dir).ok();

        Self { models_dir }
    }

    /// Transcribe an audio file. Routes to a remote whisper worker if
    /// `REMOTE_WHISPER_URL` is set; otherwise falls back to local
    /// whisper-rs (blocking, run on a tokio worker thread).
    pub async fn transcribe(
        &self,
        audio_path: &Path,
        model: WhisperModel,
        language: Option<&str>,
    ) -> Result<(String, Vec<Segment>)> {
        if let Some(url) = remote_whisper_url()
            && !url.trim().is_empty()
        {
            return transcribe_remote(&url, audio_path, model, language).await;
        }

        // Local fallback — the underlying whisper-rs API is blocking, so we
        // run it on a worker thread to avoid stalling the tokio scheduler.
        let audio_path = audio_path.to_path_buf();
        let models_dir = self.models_dir.clone();
        let language = language.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            transcribe_local(&models_dir, &audio_path, model, language.as_deref())
        })
        .await
        .context("transcribe task panicked")?
    }

    pub fn check_models_status(&self) -> String {
        let mut status = String::new();
        status.push_str("📦 Whisper Models:\n");

        if remote_whisper_url().is_some() {
            status.push_str("  (remote: REMOTE_WHISPER_URL is set — local models unused)\n");
        }

        for model in [
            WhisperModel::Tiny,
            WhisperModel::Base,
            WhisperModel::Small,
            WhisperModel::Medium,
            WhisperModel::Large,
        ] {
            let model_path = self.models_dir.join(model.model_filename());
            if model_path.exists() {
                let size = std::fs::metadata(&model_path)
                    .map(|m| format!("{:.1} MB", m.len() as f64 / 1_000_000.0))
                    .unwrap_or_else(|_| "unknown".to_string());
                status.push_str(&format!(
                    "  ✅ {:?}: {} ({})\n",
                    model,
                    model_path.display(),
                    size
                ));
            } else {
                status.push_str(&format!("  ❌ {:?}: not installed\n", model));
            }
        }

        status
    }
}

fn remote_whisper_url() -> Option<String> {
    std::env::var("REMOTE_WHISPER_URL").ok()
}

// ---------- remote whisper-worker path ----------

#[derive(Deserialize)]
struct RemoteResponse {
    transcript: String,
    segments: Vec<RemoteSegment>,
}

#[derive(Deserialize)]
struct RemoteSegment {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

/// How long to wait for the connection itself, as distinct from the work.
///
/// Modal cold-starts a GPU container on the first request after idling, and
/// that shows up here as a slow or dead connect rather than a slow response.
/// Short enough that a stall is retried while the caller still has budget.
const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Total attempts, including the first.
///
/// Three, not more: each retry re-uploads the whole audio file, so attempts are
/// not cheap, and a worker that has refused three connections is not having a
/// cold start any more.
const REMOTE_ATTEMPTS: u32 = 3;

/// Base backoff, multiplied by the attempt number (0.5s, then 1s).
///
/// Deliberately small. This is bridging a container cold start, not backing off
/// a rate limit, and the caller is holding a paid job open the whole time.
const REMOTE_BACKOFF: Duration = Duration::from_millis(500);

/// Whether a failed send is worth another attempt.
///
/// Connect, timeout and request errors all mean the worker never gave an
/// answer, so a retry has no side-effect to duplicate. Anything else — a
/// decode failure, a body error — is about the response we did get, and
/// repeating the upload would not change it.
fn is_retryable(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_request()
}

async fn transcribe_remote(
    url: &str,
    audio_path: &Path,
    model: WhisperModel,
    language: Option<&str>,
) -> Result<(String, Vec<Segment>)> {
    info!("🛰  Transcribing via remote Whisper ({}): {:?}", url, model);

    let bytes = tokio::fs::read(audio_path)
        .await
        .with_context(|| format!("Failed to read audio file: {}", audio_path.display()))?;

    let filename = audio_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.mp3")
        .to_string();

    let client = reqwest::Client::builder()
        // Whole-request budget: a long transcription legitimately takes minutes.
        .timeout(Duration::from_secs(600))
        // Getting the connection up is not the same as doing the work, and
        // without this a stalled connect can eat a large slice of the 600s
        // before anything is retried. On 2026-09-03 a job died after 46s with
        // `SendRequest: connection error: timed out` and no second attempt.
        .connect_timeout(REMOTE_CONNECT_TIMEOUT)
        .build()
        .context("Failed to build reqwest client")?;

    // The worker scales to zero, so a cold start is normal operation rather
    // than an exception, and the first connection into one can simply fail.
    // Retrying transport failures is what makes that survivable.
    let mut attempt = 1;
    let resp = loop {
        // Rebuilt per attempt: `multipart::Form` is consumed by `send`, so it
        // cannot be reused. The clone is the audio bytes again — real cost, but
        // only paid on a retry, and cheaper than losing a job the caller has
        // already been charged for.
        let part = reqwest::multipart::Part::bytes(bytes.clone())
            .file_name(filename.clone())
            .mime_str("audio/mpeg")
            .context("Failed to build multipart part")?;
        let form = reqwest::multipart::Form::new()
            .part("audio", part)
            .text("model", model.as_str().to_string())
            .text("language", language.unwrap_or("auto").to_string());

        match client.post(url).multipart(form).send().await {
            Ok(resp) => break resp,
            // Retry only what a retry can fix. A connect/timeout/request error
            // means we never got an answer, so trying again is free of
            // side-effects. An HTTP error status is a decision the worker
            // already made — repeating it would just re-upload the audio to be
            // refused again, which is why that case is handled below instead.
            Err(e) if attempt < REMOTE_ATTEMPTS && is_retryable(&e) => {
                let backoff = REMOTE_BACKOFF * attempt;
                warn!(
                    "🛰  Remote whisper attempt {attempt}/{REMOTE_ATTEMPTS} failed ({e}); \
                     retrying in {backoff:?}"
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Remote whisper POST failed after {attempt} attempt(s)")
                });
            }
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Remote whisper returned {}: {}", status, body);
    }

    let r: RemoteResponse = resp
        .json()
        .await
        .context("Failed to parse remote whisper response")?;

    info!(
        "🛰  Remote transcription complete: {} segments",
        r.segments.len()
    );

    let segments = r
        .segments
        .into_iter()
        .map(|s| Segment {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            text: s.text,
        })
        .collect();

    Ok((r.transcript, segments))
}

// ---------- local (whisper-rs) path ----------

fn transcribe_local(
    models_dir: &Path,
    audio_path: &Path,
    model: WhisperModel,
    language: Option<&str>,
) -> Result<(String, Vec<Segment>)> {
    info!("Loading Whisper model: {:?}", model);

    let model_path = get_model_path(models_dir, model)?;

    let ctx = WhisperContext::new_with_params(
        model_path.to_str().unwrap(),
        WhisperContextParameters::default(),
    )
    .context("Failed to load Whisper model")?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

    if let Some(lang) = language
        && lang != "auto"
    {
        params.set_language(Some(lang));
        params.set_translate(false);
    }

    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_n_threads(optimal_whisper_threads());

    // Anti-hallucination: over music/silence Whisper otherwise repeats a phrase
    // dozens of times. n_max_text_ctx(0) stops it feeding the prior window's
    // text back in (so a loop can't feed itself — the biggest single fix), and
    // suppress_nst drops non-speech tokens. Mirrors the Modal worker's
    // condition_on_previous_text=False. (Matches whisper-cli `-mc 0 -sns`.)
    params.set_n_max_text_ctx(0);
    params.set_suppress_nst(true);

    info!("Loading audio file...");
    let audio_data = load_audio_as_pcm(audio_path)?;

    info!("Transcribing... (this may take a few minutes)");
    let mut state = ctx
        .create_state()
        .context("Failed to create Whisper state")?;

    state
        .full(params, &audio_data[..])
        .context("Failed to transcribe audio")?;

    let num_segments = state.full_n_segments();

    let mut transcript = String::new();
    let mut segments = Vec::with_capacity(num_segments as usize);
    for i in 0..num_segments {
        let segment = state
            .get_segment(i)
            .context(format!("Failed to get segment {}", i))?;
        let text = segment
            .to_str_lossy()
            .context(format!("Failed to get text for segment {}", i))?
            .to_string();
        let start_ms = (segment.start_timestamp().max(0) as u64) * 10;
        let end_ms = (segment.end_timestamp().max(0) as u64) * 10;
        transcript.push_str(&text);
        transcript.push(' ');
        segments.push(Segment {
            start_ms,
            end_ms,
            text: text.trim().to_string(),
        });
    }

    Ok((transcript.trim().to_string(), segments))
}

fn get_model_path(models_dir: &Path, model: WhisperModel) -> Result<PathBuf> {
    let model_filename = model.model_filename();
    let model_path = models_dir.join(&model_filename);

    if !model_path.exists() {
        anyhow::bail!(
            "Whisper model not found: {}\n\n\
            Please download it using:\n\
              bash scripts/download-models.sh {}\n\n\
            Or download manually from:\n\
              https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            model_path.display(),
            model.as_str(),
            model_filename
        );
    }

    Ok(model_path)
}

fn load_audio_as_pcm(audio_path: &Path) -> Result<Vec<f32>> {
    info!("Converting audio to 16kHz mono PCM...");

    let output = std::process::Command::new("ffmpeg")
        .args([
            "-i",
            audio_path.to_str().unwrap(),
            "-ar",
            "16000",
            "-ac",
            "1",
            "-f",
            "f32le",
            "-",
        ])
        .output()
        .context("Failed to run ffmpeg")?;

    if !output.status.success() {
        anyhow::bail!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let bytes = output.stdout;
    // clippy (1.98+) suggests `as_chunks::<4>()`, which is nicer — it drops the
    // infallible `try_into().unwrap()` below. It stabilised in Rust 1.88 though,
    // and the README advertises 1.85+, so taking the suggestion would raise the
    // MSRV. Revisit when the floor moves to 1.88.
    //
    // `unknown_lints` is allowed alongside it because the lint does not exist
    // before 1.98: without it, naming the lint is itself a `-D warnings` error
    // on older toolchains, so CI and local would fail on opposite versions.
    #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|chunk| {
            let bytes: [u8; 4] = chunk.try_into().unwrap();
            f32::from_le_bytes(bytes)
        })
        .collect();

    info!("Loaded {} audio samples", samples.len());

    Ok(samples)
}

/// On Apple Silicon, Whisper is fastest using P-cores only — letting it
/// spill onto E-cores actively slows transcription due to thread scheduling
/// disparities. We probe `sysctl hw.perflevel0.physicalcpu` (P-core count)
/// on macOS and fall back to all logical cores elsewhere.
fn optimal_whisper_threads() -> i32 {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "hw.perflevel0.physicalcpu"])
            .output()
            && let Ok(s) = String::from_utf8(out.stdout)
            && let Ok(n) = s.trim().parse::<i32>()
            && n > 0
        {
            return n;
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4)
}

#[cfg(test)]
mod remote_retry_tests {
    use super::*;

    /// A refused connection is the shape a cold or absent worker presents, and
    /// it must be retried rather than failing the job on the first try. Port 1
    /// is refused immediately and deterministically, so this exercises the loop
    /// without depending on a network or a timer.
    #[tokio::test]
    async fn a_refused_connection_is_retried_to_the_limit() {
        let dir = std::env::temp_dir().join(format!("whisper-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("a.mp3");
        std::fs::write(&audio, b"not really an mp3").unwrap();

        let started = std::time::Instant::now();
        let err = transcribe_remote("http://127.0.0.1:1/", &audio, WhisperModel::Base, None)
            .await
            .expect_err("nothing is listening on port 1");
        let elapsed = started.elapsed();
        let msg = format!("{err:#}");

        // Deliberately the literal 3, not REMOTE_ATTEMPTS. Asserting against the
        // constant makes the test move with it, so dropping retries to 1 would
        // still "pass" — which is exactly what happened the first time this was
        // written.
        assert!(
            msg.contains("after 3 attempt(s)"),
            "the error must report all 3 attempts, got: {msg}"
        );
        // Two backoffs (500ms + 1000ms) must actually have been waited out. This
        // is the half that proves retries HAPPENED rather than that a number was
        // formatted into a string.
        assert!(
            elapsed >= std::time::Duration::from_millis(1400),
            "expected two backoffs to elapse, took only {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The classifier is what keeps a retry from duplicating work that already
    /// happened. A refused connect means the worker never answered, so trying
    /// again has nothing to duplicate.
    #[tokio::test]
    async fn a_refused_connect_classifies_as_retryable() {
        let e = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("nothing is listening on port 1");
        assert!(is_retryable(&e), "a refused connect must be retryable: {e}");
    }
}
