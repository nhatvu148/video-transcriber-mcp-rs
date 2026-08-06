#!/usr/bin/env bash
#
# Walks the x402 payment gate end to end and reports what passed.
#
# Everything that doesn't require a funded wallet runs automatically. The one
# step that does — an actual on-chain settlement — is explained at the end with
# the exact commands, because it needs testnet funds only you can obtain.
#
#   scripts/x402-check.sh
#
# Set X402_PAY_TO to your Solana receiving address to exercise the paid
# path. Without it the script still verifies that payments stay off, which is
# the behaviour every existing deployment depends on.

set -uo pipefail

PORT="${PORT:-8402}"
BASE="http://127.0.0.1:${PORT}"
BIN="./target/release/video-transcriber-mcp"
WORK="$(mktemp -d)"
PASS=0
FAIL=0

cleanup() {
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; FAIL=$((FAIL + 1)); }
note() { printf '    %s\n' "$1"; }
head2() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# --- prerequisites ---------------------------------------------------------

head2 "Prerequisites"
if [ ! -x "$BIN" ]; then
    echo "  Building release binary (first run takes a few minutes)..."
    cargo build --release --quiet || { echo "  build failed"; exit 1; }
fi
ok "server binary present"

if [ -n "${X402_PAY_TO:-}" ]; then
    ok "X402_PAY_TO is set (${X402_PAY_TO})"
    PAID_MODE=1
else
    note "X402_PAY_TO is not set — checking the payments-off path only."
    note "To exercise payments, see 'Getting a testnet wallet' at the end."
    PAID_MODE=0
fi

# --- start the server ------------------------------------------------------

# A throwaway credits file so this never touches a real ledger, and no
# DATABASE_URL so it can't reach production Postgres.
env -u DATABASE_URL CREDITS_DB_PATH="$WORK/credits.json" \
    "$BIN" --transport http --port "$PORT" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 40); do
    curl -s -o /dev/null --max-time 2 "$BASE/api/jobs" && break
    sleep 0.5
done

head2 "Server startup"
if grep -q "MCP payments ON" "$WORK/server.log"; then
    ok "payments enabled: $(grep -o 'MCP payments ON.*' "$WORK/server.log" | head -1)"
elif [ "$PAID_MODE" = "1" ]; then
    bad "X402_PAY_TO was set but payments did not enable"
    note "$(grep -i 'x402\|payment' "$WORK/server.log" | head -3)"
    note "Usually a malformed address — the server refuses to half-enable."
else
    ok "payments off (default)"
fi

# --- MCP session -----------------------------------------------------------

SID=$(curl -s -D- -X POST "$BASE/mcp" \
    -H 'content-type: application/json' \
    -H 'accept: application/json, text/event-stream' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"x402-check","version":"1"}}}' \
    2>/dev/null | tr -d '\r' | grep -i '^mcp-session-id' | cut -d' ' -f2)

curl -s -X POST "$BASE/mcp" -H 'content-type: application/json' \
    -H 'accept: application/json, text/event-stream' -H "mcp-session-id: $SID" \
    -d '{"jsonrpc":"2.0","method":"notifications/initialized"}' > /dev/null

head2 "Free access (must work with or without payments)"
[ -n "$SID" ] && ok "initialize opened a session" || bad "initialize failed"

TOOLS=$(curl -s -X POST "$BASE/mcp" -H 'content-type: application/json' \
    -H 'accept: application/json, text/event-stream' -H "mcp-session-id: $SID" \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}')
if grep -q '"transcribe_video"' <<< "$TOOLS"; then
    ok "tools/list returned the catalogue without payment"
else
    bad "tools/list did not return the catalogue"
fi

FREE_CALL=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/mcp" \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $SID" \
    -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_supported_sites","arguments":{}}}')
[ "$FREE_CALL" = "200" ] && ok "a free tool call succeeded (HTTP 200)" \
                         || bad "free tool call returned HTTP $FREE_CALL"

# --- the priced tool -------------------------------------------------------

head2 "Priced tool without payment"
PRICED=$(curl -s -w '\n%{http_code}' -X POST "$BASE/mcp" \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $SID" \
    -d '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"transcribe_video","arguments":{"url":"https://example.test/v.mp4"}}}')
PRICED_CODE=$(tail -1 <<< "$PRICED")
PRICED_BODY=$(sed '$d' <<< "$PRICED")

if [ "$PAID_MODE" = "1" ]; then
    if [ "$PRICED_CODE" = "402" ]; then
        ok "unpaid call was challenged with 402"
        if grep -q '"accepts"' <<< "$PRICED_BODY"; then
            ok "the challenge tells the client what to pay"
            note "$(python3 -c "
import json,sys
d=json.loads(sys.stdin.read())
a=d['accepts'][0]
print(f\"pay {a.get('amount')} of {a.get('asset','?')[:10]}… on {a.get('network')} to {a.get('payTo')}\")
" <<< "$PRICED_BODY" 2>/dev/null || echo 'could not parse accepts')"
        else
            bad "402 carried no 'accepts' — a client cannot act on it"
        fi
    else
        bad "expected 402, got HTTP $PRICED_CODE"
    fi
    # Catalogue should advertise the price so agents can decide before calling.
    grep -q 'COST:' <<< "$TOOLS" && ok "catalogue advertises the price" \
                                 || bad "catalogue does not advertise the price"
else
    if [ "$PRICED_CODE" != "402" ]; then
        ok "payments off: the priced tool was not challenged"
    else
        bad "payments are off but the call was challenged with 402"
    fi
    grep -q 'COST:' <<< "$TOOLS" && bad "catalogue advertises a price while payments are off" \
                                 || ok "catalogue advertises no price"
fi

# --- summary ---------------------------------------------------------------

head2 "Result"
printf '  %d passed, %d failed\n' "$PASS" "$FAIL"

if [ "$PAID_MODE" = "0" ]; then
    cat <<'GUIDE'

  ─────────────────────────────────────────────────────────────────────
  Getting a testnet wallet (nothing here costs real money)
  ─────────────────────────────────────────────────────────────────────

  You need two throwaway wallets: one to RECEIVE, one to PAY.

  1. Create them. With the Solana CLI installed:

         solana-keygen new -o payer.json      # keep this key local
         solana address -k payer.json

     Or use Phantom and switch the network to Devnet.

  2. Fund only the PAYER with devnet USDC:

         https://faucet.circle.com     → choose "Solana Devnet"

     $1 covers five calls at $0.20. The payer also needs a little devnet
     SOL for rent/fees:

         solana airdrop 1 <payer-address> --url devnet

  3. Re-run this script with the RECEIVER address:

         X402_PAY_TO=<your-solana-address> scripts/x402-check.sh

     Everything above will then run against the paid path.

  4. For a real on-chain settlement, a paying client is still needed —
     it has to sign the payment. See the "settlement" section of PR #18.
     Once it runs, confirm the transfer independently at:

         https://explorer.solana.com/address/<receiver>?cluster=devnet

  Nothing in this script moves funds, and it never asks for a private key.
GUIDE
fi

[ "$FAIL" -eq 0 ] || exit 1
GUIDE_EXIT=0
