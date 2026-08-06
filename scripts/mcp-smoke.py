#!/usr/bin/env python3
"""
Drive the MCP server over stdio the way a real client does, and check every
tool answers.

The integration tests in tests/ cover this too, but they exercise the server
as a library consumer would. This drives the *built binary* — the artifact
people actually install — so it catches packaging and startup problems tests
can't see, and it's the fastest way to sanity-check a release build by hand.

    scripts/mcp-smoke.py [path-to-binary]

Exits non-zero if any tool fails to answer. Only read-only tools are called:
nothing here transcribes, deletes, or costs anything. `transcribe_video` is
covered separately by mcp-transcribe-check.py, which needs a model.
"""

import json
import queue
import subprocess
import sys
import threading
from pathlib import Path

BINARY = sys.argv[1] if len(sys.argv) > 1 else "./target/release/video-transcriber-mcp"
PROTOCOL = "2025-06-18"

# Every tool the server should advertise, with args that are safe to run.
# `check_dependencies` shells out to yt-dlp and ffmpeg so it can take ~20s —
# hence the generous per-request timeout below.
TOOL_CALLS = [
    ("check_dependencies", {}),
    ("list_supported_sites", {}),
    ("list_transcripts", {"limit": 3}),
    ("get_latest_transcript", {}),
    ("search_transcripts", {"query": "smoke test", "limit": 2}),
]

# Tools deliberately not called here, because they mutate or cost money.
# Listing them keeps this honest about what "smoke tested" covers.
NOT_CALLED = [
    "transcribe_video",  # see mcp-transcribe-check.py
    "delete_transcript",
    "cleanup_old_transcripts",
    "delete_all_transcripts",
]

REQUEST_TIMEOUT = 60  # seconds to wait for any single response


class Server:
    def __init__(self, binary):
        self.proc = subprocess.Popen(
            [binary, "--transport", "stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,  # logs go to stderr; keep them out of the way
            text=True,
            bufsize=1,
        )
        # Read on a thread so a hung server times out instead of blocking forever.
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

    def request(self, req_id, method, params):
        self.send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
        while True:
            try:
                line = self.responses.get(timeout=REQUEST_TIMEOUT)
            except queue.Empty:
                raise TimeoutError(f"no response to {method} within {REQUEST_TIMEOUT}s")
            msg = json.loads(line)
            if msg.get("id") == req_id:
                return msg

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            # A server that ignores SIGTERM must not turn a clean failure
            # report into a traceback — close() runs in a finally block, so an
            # exception here would replace the error we're trying to report.
            self.proc.kill()
            self.proc.wait()


def main():
    binary = Path(BINARY)
    if not binary.exists():
        print(f"✗ binary not found: {binary}\n  build it first: task build")
        return 1

    print(f"🔌 MCP stdio smoke test — {binary}")
    server = Server(str(binary))
    failures = []
    next_id = iter(range(1, 1000))

    try:
        # ---- handshake ----
        reply = server.request(
            next(next_id),
            "initialize",
            {
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "smoke", "version": "1"},
            },
        )
        if "error" in reply:
            print(f"  ✗ initialize: {reply['error']}")
            return 1
        negotiated = reply["result"]["protocolVersion"]
        print(f"  ✓ initialize            protocol={negotiated}")
        server.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

        # ---- tool listing ----
        reply = server.request(next(next_id), "tools/list", {})
        tools = [t["name"] for t in reply["result"]["tools"]]
        print(f"  ✓ tools/list            {len(tools)} tools")

        expected = {name for name, _ in TOOL_CALLS} | set(NOT_CALLED)
        missing = expected - set(tools)
        if missing:
            failures.append(f"tools missing from tools/list: {sorted(missing)}")
            print(f"  ✗ missing tools: {sorted(missing)}")

        # ---- call each read-only tool ----
        for name, args in TOOL_CALLS:
            try:
                reply = server.request(
                    next(next_id), "tools/call", {"name": name, "arguments": args}
                )
            except TimeoutError as e:
                failures.append(f"{name}: {e}")
                print(f"  ✗ {name:<22} TIMEOUT")
                continue

            if "error" in reply:
                failures.append(f"{name}: {reply['error']}")
                print(f"  ✗ {name:<22} error {reply['error']['code']}")
                continue

            result = reply["result"]
            text = result.get("content", [{}])[0].get("text", "")
            if not text:
                failures.append(f"{name}: returned no text content")
                print(f"  ✗ {name:<22} empty response")
                continue

            preview = " ".join(text.split())[:48]
            print(f"  ✓ {name:<22} {preview}")

        # ---- an unknown tool must be a clean error, not a crash ----
        reply = server.request(
            next(next_id), "tools/call", {"name": "no_such_tool", "arguments": {}}
        )
        if reply.get("error", {}).get("code") == -32601:
            print("  ✓ unknown tool rejected (-32601)")
        else:
            failures.append("unknown tool did not return METHOD_NOT_FOUND")
            print(f"  ✗ unknown tool: expected -32601, got {reply}")

        # ---- server must still be alive after all that ----
        reply = server.request(next(next_id), "tools/list", {})
        if len(reply["result"]["tools"]) == len(tools):
            print("  ✓ session still healthy")
        else:
            failures.append("tool list changed after error handling")

    except Exception as e:  # noqa: BLE001 - surface anything as a failure
        failures.append(str(e))
        print(f"  ✗ {e}")
    finally:
        server.close()

    print()
    if failures:
        print(f"❌ {len(failures)} failure(s):")
        for f in failures:
            print(f"   - {f}")
        return 1

    print(f"✅ MCP stdio smoke passed ({len(TOOL_CALLS)} tools called)")
    print(f"   not called (mutating or costly): {', '.join(NOT_CALLED)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
