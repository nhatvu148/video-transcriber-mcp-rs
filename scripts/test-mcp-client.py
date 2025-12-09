#!/usr/bin/env python3
"""
Simple test client for Streamable HTTP MCP server
Requires: requests
Install: pip install requests
"""

import requests
import json
import sys

class MCPClient:
    def __init__(self, base_url):
        self.base_url = base_url
        self.endpoint = f"{base_url}/mcp"
        self.session_id = None
        self.message_id = 0

    def _next_id(self):
        self.message_id += 1
        return self.message_id

    def _send_request(self, method, params=None):
        """Send a JSON-RPC request to the MCP server"""
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream"
        }

        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id

        payload = {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": method,
            "params": params or {}
        }

        print(f"\n📤 Sending: {method}")
        print(f"   Payload: {json.dumps(payload, indent=2)}")

        response = requests.post(self.endpoint, json=payload, headers=headers)

        # Extract session ID from response headers
        if "Mcp-Session-Id" in response.headers:
            self.session_id = response.headers["Mcp-Session-Id"]
            print(f"   Session ID: {self.session_id}")

        # Handle SSE response
        if response.headers.get("Content-Type") == "text/event-stream":
            # Parse SSE data
            lines = response.text.strip().split('\n')
            for line in lines:
                if line.startswith('data: '):
                    data = json.loads(line[6:])
                    print(f"📥 Response: {json.dumps(data, indent=2)}")
                    return data
        else:
            # Regular JSON response
            data = response.json()
            print(f"📥 Response: {json.dumps(data, indent=2)}")
            return data

    def _send_notification(self, method, params=None):
        """Send a JSON-RPC notification (no response expected)"""
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream"
        }

        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id

        payload = {
            "jsonrpc": "2.0",
            "method": method,
            "params": params or {}
        }

        print(f"\n📤 Sending notification: {method}")
        requests.post(self.endpoint, json=payload, headers=headers)

    def initialize(self):
        """Initialize the MCP session"""
        result = self._send_request("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "python-test-client",
                "version": "1.0.0"
            }
        })
        # Send initialized notification
        self._send_notification("notifications/initialized")
        return result

    def list_tools(self):
        """List available tools"""
        return self._send_request("tools/list")

    def call_tool(self, name, arguments=None):
        """Call a specific tool"""
        return self._send_request("tools/call", {
            "name": name,
            "arguments": arguments or {}
        })

def main():
    base_url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080"

    print("=" * 60)
    print("MCP Streamable HTTP Test Client")
    print("=" * 60)
    print(f"Server: {base_url}/mcp\n")

    client = MCPClient(base_url)

    try:
        # Test 1: Initialize
        print("\n🔧 Test 1: Initialize Session")
        print("-" * 60)
        client.initialize()

        # Test 2: List Tools
        print("\n🔧 Test 2: List Available Tools")
        print("-" * 60)
        tools_result = client.list_tools()
        if tools_result and "result" in tools_result and "tools" in tools_result["result"]:
            print(f"\n✅ Found {len(tools_result['result']['tools'])} tools:")
            for tool in tools_result["result"]["tools"]:
                print(f"   - {tool['name']}")

        # Test 3: Check Dependencies
        print("\n🔧 Test 3: Call check_dependencies Tool")
        print("-" * 60)
        client.call_tool("check_dependencies")

        # Test 4: List Supported Sites
        print("\n🔧 Test 4: Call list_supported_sites Tool")
        print("-" * 60)
        client.call_tool("list_supported_sites")

        print("\n" + "=" * 60)
        print("✅ All tests completed successfully!")
        print("=" * 60)

    except Exception as e:
        print(f"\n❌ Error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)

if __name__ == "__main__":
    main()
