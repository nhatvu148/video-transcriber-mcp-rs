#!/usr/bin/env bash
# Drive the real binary over both transports and assert what a consumer sees.
#
# `cargo test` covers the units. This covers the thing units can't: that the
# shipped binary actually speaks MCP, advertises the tools it claims, and
# applies the URL guard on HTTP but not on stdio. After the split that removed
# two thirds of this crate, "it compiles and the tests pass" is a weaker claim
# than it sounds — every one of these checks is something a published release
# would be judged on.
#
#   scripts/verify.sh
set -uo pipefail

BIN=${BIN:-./target/debug/video-transcriber-mcp}
PASS=0
FAIL=0

ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; PASS=$((PASS+1)); }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; FAIL=$((FAIL+1)); }
head_() { printf '\n\033[1m%s\033[0m\n' "$1"; }

[ -x "$BIN" ] || { echo "no binary at $BIN — run: cargo build"; exit 1; }

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"verify","version":"0"}}}'

# ----------------------------------------------------------------- stdio
head_ "stdio transport"

STDIO_OUT=$(printf '%s\n%s\n%s\n' \
  "$INIT" \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | "$BIN" 2>/dev/null)

grep -q '"serverInfo"' <<<"$STDIO_OUT" \
  && ok "initialize answers" || bad "initialize produced no serverInfo"

# The catalogue is the contract: an agent picks tools from this alone.
for tool in transcribe_video list_transcripts get_latest_transcript \
            search_transcripts delete_transcript cleanup_old_transcripts \
            delete_all_transcripts check_dependencies list_supported_sites; do
  grep -q "\"$tool\"" <<<"$STDIO_OUT" \
    && ok "tools/list advertises $tool" || bad "tools/list is missing $tool"
done

# This crate is the open-source server; a price belongs to a paid deployment.
grep -q 'COST:' <<<"$STDIO_OUT" \
  && bad "catalogue quotes a price — that belongs to a paid deployment" \
  || ok "catalogue quotes no price"

# --------------------------------------------------------------- guard off
head_ "URL guard is OFF on stdio (local use is not a threat model)"

LOCAL_CALL=$(printf '%s\n%s\n%s\n' \
  "$INIT" \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"transcribe_video","arguments":{"url":"http://127.0.0.1:9/nope.mp4"}}}' \
  | "$BIN" 2>/dev/null)

# It must fail (nothing is listening) but NOT because we refused the address —
# transcribing from your own machine is legitimate here.
grep -q 'not reachable' <<<"$LOCAL_CALL" \
  && bad "stdio refused a loopback URL — that breaks legitimate local use" \
  || ok "stdio does not refuse loopback"

# ------------------------------------------------------------------ http
head_ "HTTP transport"

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
"$BIN" --transport http --port "$PORT" >/dev/null 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT

for _ in $(seq 1 40); do
  curl -sf -o /dev/null -m 1 -X POST "http://127.0.0.1:$PORT/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -d "$INIT" && break
  sleep 0.25
done

SID=$(curl -s -D - -o /dev/null -X POST "http://127.0.0.1:$PORT/mcp" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' -d "$INIT" \
  | grep -i '^mcp-session-id:' | tr -d '\r' | awk '{print $2}')

[ -n "$SID" ] && ok "issues an Mcp-Session-Id" || bad "no session id returned"

# --------------------------------------------------------------- guard on
head_ "URL guard is ON over HTTP (reachable by others)"

for target in "http://127.0.0.1:8080/x.mp4" "http://169.254.169.254/latest/meta-data/"; do
  # Host as submitted, so the leak check below is about *this* target rather
  # than a hardcoded pattern that happens to match one of them.
  host=$(sed -E 's|^https?://||; s|[:/].*$||' <<<"$target")

  RESP=$(curl -s -X POST "http://127.0.0.1:$PORT/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "mcp-session-id: $SID" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"transcribe_video\",\"arguments\":{\"url\":\"$target\"}}}")

  grep -q 'not reachable' <<<"$RESP" \
    && ok "refuses $target" || bad "did NOT refuse $target"

  # Checked per target, not once after the loop: a refusal must not echo the
  # address back or name why it was refused, or it becomes a probe of what is
  # reachable from in here.
  if grep -qE "$host|private|loopback|link-local" <<<"$RESP"; then
    bad "refusal for $host leaks network detail"
  else
    ok "refusal for $host reveals nothing"
  fi
done

kill $SRV 2>/dev/null

# ---------------------------------------------------------------- verdict
head_ "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
