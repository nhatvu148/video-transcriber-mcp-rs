#!/usr/bin/env python3
"""
End-to-end check of the one tool that does the real work: transcribe a clip of
known speech and assert the words come back.

`transcribe_video` is the headline feature and the only tool with no automated
coverage — the MCP e2e suites deliberately call cheap metadata tools so they can
run without a model. That leaves the whole local pipeline (file input → ffmpeg
audio extraction → whisper.cpp inference → txt/json/md output) unverified, which
matters because whisper-rs wraps a bundled C++ build that can compile fine and
still produce garbage.

    scripts/mcp-transcribe-check.py [path-to-binary]

Needs the tiny model (task download:tiny) and ffmpeg. Skips with exit 0 if the
model is absent so it can be wired into a broader task without becoming a
tripwire on a fresh checkout — but prints loudly, because a silently skipped
check is worse than no check.
"""

import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

BINARY = sys.argv[1] if len(sys.argv) > 1 else "./target/release/video-transcriber-mcp"
MODEL = Path.home() / ".cache/video-transcriber-mcp/models/ggml-tiny.bin"

SPOKEN = "The quick brown fox jumps over the lazy dog. This is a test of the video transcriber."
# Assert on content words rather than the exact string: whisper's casing and
# punctuation drift between builds, and going red over a comma would train
# everyone to ignore this check. These still fail loudly on silence or garbage.
MUST_CONTAIN = ["brown fox", "lazy dog", "video transcriber"]

TRANSCRIBE_TIMEOUT = 300


class Server:
    def __init__(self, binary):
        self.proc = subprocess.Popen(
            [binary, "--transport", "stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        self.responses = queue.Queue()
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        for line in self.proc.stdout:
            line = line.strip()
            if line:
                self.responses.put(line)

    def send(self, payload):
        self.proc.stdin.write(json.dumps(payload) + "\n")
        self.proc.stdin.flush()

    def request(self, req_id, method, params, timeout):
        self.send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
        while True:
            try:
                line = self.responses.get(timeout=timeout)
            except queue.Empty:
                raise TimeoutError(f"no response to {method} within {timeout}s")
            msg = json.loads(line)
            if msg.get("id") == req_id:
                return msg

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        self.proc.terminate()
        self.proc.wait(timeout=10)


def make_clip(workdir):
    """Produce a short audio clip of known speech.

    Prefers a committed fixture so the check is deterministic and portable;
    falls back to macOS `say` for local runs where no fixture exists.
    """
    fixture = Path(__file__).parent.parent / "tests/fixtures/speech-sample.m4a"
    clip = workdir / "clip.m4a"

    if fixture.exists():
        shutil.copy(fixture, clip)
        print(f"  using fixture {fixture.name}")
        return clip

    if not shutil.which("say"):
        print("  ✗ no fixture and no `say` available to synthesize one")
        print("    (on Linux, commit tests/fixtures/speech-sample.m4a)")
        return None

    aiff = workdir / "speech.aiff"
    subprocess.run(["say", "-o", str(aiff), SPOKEN], check=True)
    subprocess.run(
        ["ffmpeg", "-y", "-i", str(aiff), "-vn", "-ac", "1", "-ar", "16000",
         "-c:a", "aac", str(clip)],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    print("  synthesized clip with `say`")
    return clip


def main():
    binary = Path(BINARY)
    if not binary.exists():
        print(f"✗ binary not found: {binary}\n  build it first: task build")
        return 1

    if not MODEL.exists():
        print("⏭  SKIPPED — tiny model not found")
        print(f"   expected at {MODEL}")
        print("   get it with: task download:tiny")
        return 0

    if not shutil.which("ffmpeg"):
        print("⏭  SKIPPED — ffmpeg not installed (brew install ffmpeg)")
        return 0

    print(f"🎤 transcription end-to-end check — {binary}")

    with tempfile.TemporaryDirectory(prefix="transcribe-check-") as tmp:
        workdir = Path(tmp)
        clip = make_clip(workdir)
        if clip is None:
            return 1

        outdir = workdir / "out"
        outdir.mkdir()
        server = Server(str(binary))
        try:
            server.request(
                1, "initialize",
                {"protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": {"name": "transcribe-check", "version": "1"}},
                timeout=30,
            )
            server.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

            print("  transcribing (tiny model)…")
            reply = server.request(
                2, "tools/call",
                {"name": "transcribe_video",
                 "arguments": {"url": str(clip), "model": "tiny",
                               "language": "en", "output_dir": str(outdir)}},
                timeout=TRANSCRIBE_TIMEOUT,
            )
            if "error" in reply:
                print(f"  ✗ transcribe_video failed: {reply['error']}")
                return 1
        finally:
            server.close()

        # The tool reports success in its text; the real check is the artifacts.
        produced = sorted(p.name for p in outdir.iterdir())
        for suffix in (".txt", ".json", ".md"):
            if not any(n.endswith(suffix) for n in produced):
                print(f"  ✗ no {suffix} output produced (got: {produced})")
                return 1
        print(f"  ✓ wrote {len(produced)} files: {', '.join(produced)}")

        txt = next(p for p in outdir.iterdir() if p.suffix == ".txt")
        transcript = txt.read_text().strip()
        if not transcript:
            print("  ✗ transcript file is empty")
            return 1

        print(f"  transcript: {transcript[:100]}")
        lowered = transcript.lower()
        missing = [phrase for phrase in MUST_CONTAIN if phrase not in lowered]
        if missing:
            print(f"  ✗ transcript is missing expected phrases: {missing}")
            print(f"    spoken:     {SPOKEN}")
            print(f"    transcribed:{transcript}")
            return 1

        print(f"  ✓ all {len(MUST_CONTAIN)} expected phrases present")

    print("\n✅ transcription end-to-end passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
