#!/usr/bin/env node
/**
 * Stdio-to-HTTP Proxy for MCP
 *
 * This proxy allows Claude Code (stdio) to connect to an HTTP MCP server.
 *
 * Usage:
 *   node http-proxy.js http://localhost:8080/mcp
 *
 * In .mcp.json:
 * {
 *   "mcpServers": {
 *     "video-transcriber-http": {
 *       "command": "node",
 *       "args": ["http-proxy.js", "http://localhost:8080/mcp"],
 *       "enabled": true
 *     }
 *   }
 * }
 */

const http = require('http');
const https = require('https');
const readline = require('readline');

const MCP_URL = process.argv[2] || 'http://localhost:8080/mcp';
let sessionId = null;

// Parse URL
const url = new URL(MCP_URL);
const client = url.protocol === 'https:' ? https : http;

// Read from stdin (Claude Code sends requests here)
const rl = readline.createInterface({
  input: process.stdin,
  output: process.stdout,
  terminal: false
});

// Send request to HTTP MCP server
async function sendRequest(jsonrpcRequest) {
  return new Promise((resolve, reject) => {
    const headers = {
      'Content-Type': 'application/json',
      'Accept': 'application/json, text/event-stream'
    };

    if (sessionId) {
      headers['Mcp-Session-Id'] = sessionId;
    }

    const data = JSON.stringify(jsonrpcRequest);

    const options = {
      method: 'POST',
      headers: {
        ...headers,
        'Content-Length': Buffer.byteLength(data)
      }
    };

    const req = client.request(MCP_URL, options, (res) => {
      // Extract session ID
      const sid = res.headers['mcp-session-id'];
      if (sid) {
        sessionId = sid;
      }

      let responseData = '';

      res.on('data', (chunk) => {
        responseData += chunk.toString();
      });

      res.on('end', () => {
        // Handle SSE response
        if (res.headers['content-type']?.includes('text/event-stream')) {
          // Parse SSE
          const lines = responseData.split('\n');
          for (const line of lines) {
            if (line.startsWith('data: ')) {
              const jsonData = line.substring(6);
              resolve(jsonData);
              return;
            }
          }
        }
        // Handle regular JSON
        resolve(responseData);
      });
    });

    req.on('error', (error) => {
      reject(error);
    });

    req.write(data);
    req.end();
  });
}

// Send notification (no response expected)
async function sendNotification(jsonrpcNotification) {
  const headers = {
    'Content-Type': 'application/json',
    'Accept': 'application/json, text/event-stream'
  };

  if (sessionId) {
    headers['Mcp-Session-Id'] = sessionId;
  }

  const data = JSON.stringify(jsonrpcNotification);

  const options = {
    method: 'POST',
    headers: {
      ...headers,
      'Content-Length': Buffer.byteLength(data)
    }
  };

  return new Promise((resolve) => {
    const req = client.request(MCP_URL, options, (res) => {
      res.on('data', () => {}); // Consume data
      res.on('end', () => resolve());
    });

    req.on('error', () => resolve()); // Ignore errors for notifications

    req.write(data);
    req.end();
  });
}

// Handle incoming messages from Claude Code
rl.on('line', async (line) => {
  try {
    const message = JSON.parse(line);

    // Check if it's a notification (no id field)
    const isNotification = !message.hasOwnProperty('id');

    if (isNotification) {
      // Send notification (no response)
      await sendNotification(message);
    } else {
      // Send request and get response
      const response = await sendRequest(message);

      // Write response to stdout (Claude Code reads from here)
      console.log(response);
    }
  } catch (error) {
    // Send error response
    const errorResponse = {
      jsonrpc: '2.0',
      id: null,
      error: {
        code: -32603,
        message: error.message
      }
    };
    console.log(JSON.stringify(errorResponse));
  }
});

// Handle errors
process.on('uncaughtException', (error) => {
  console.error(JSON.stringify({
    jsonrpc: '2.0',
    id: null,
    error: {
      code: -32603,
      message: error.message
    }
  }));
  process.exit(1);
});

// Log to stderr (won't interfere with JSON-RPC)
console.error(`[http-proxy] Connecting to ${MCP_URL}`);
console.error('[http-proxy] Ready for JSON-RPC messages on stdin');
