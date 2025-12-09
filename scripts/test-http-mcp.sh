#!/bin/bash

# Test script for Streamable HTTP MCP server
# Usage: ./test-http-mcp.sh [base_url]

BASE_URL="${1:-http://127.0.0.1:8080}"
MCP_ENDPOINT="${BASE_URL}/mcp"

echo "=================================================="
echo "Testing Video Transcriber MCP HTTP Server"
echo "=================================================="
echo "Endpoint: $MCP_ENDPOINT"
echo ""

# Test 1: Initialize
echo "Test 1: Initialize Session"
echo "----------------------------"
INIT_RESPONSE=$(curl -s "$MCP_ENDPOINT" \
  -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d @- <<'EOF'
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "2024-11-05",
    "capabilities": {},
    "clientInfo": {
      "name": "test-client",
      "version": "1.0.0"
    }
  }
}
EOF
)

echo "$INIT_RESPONSE" | jq . 2>/dev/null || echo "$INIT_RESPONSE"

# Extract session ID from headers if needed
echo ""
echo "----------------------------"
echo ""

# Test 2: List Tools
echo "Test 2: List Available Tools"
echo "----------------------------"
TOOLS_RESPONSE=$(curl -s "$MCP_ENDPOINT" \
  -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d @- <<'EOF'
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/list",
  "params": {}
}
EOF
)

echo "$TOOLS_RESPONSE" | jq . 2>/dev/null || echo "$TOOLS_RESPONSE"
echo ""
echo "----------------------------"
echo ""

# Test 3: Check Dependencies
echo "Test 3: Check Dependencies Tool"
echo "----------------------------"
DEPS_RESPONSE=$(curl -s "$MCP_ENDPOINT" \
  -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d @- <<'EOF'
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "check_dependencies",
    "arguments": {}
  }
}
EOF
)

echo "$DEPS_RESPONSE" | jq . 2>/dev/null || echo "$DEPS_RESPONSE"
echo ""
echo "----------------------------"
echo ""

# Test 4: List Supported Sites
echo "Test 4: List Supported Sites Tool"
echo "----------------------------"
SITES_RESPONSE=$(curl -s "$MCP_ENDPOINT" \
  -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d @- <<'EOF'
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "tools/call",
  "params": {
    "name": "list_supported_sites",
    "arguments": {}
  }
}
EOF
)

echo "$SITES_RESPONSE" | jq . 2>/dev/null || echo "$SITES_RESPONSE"
echo ""
echo "----------------------------"
echo ""

echo "=================================================="
echo "All tests completed!"
echo "=================================================="
