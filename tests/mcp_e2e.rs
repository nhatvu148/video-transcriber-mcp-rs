//! End-to-end tests that drive the real MCP server binary over the wire.
//!
//! These exist because the interesting failures in an rmcp upgrade are not
//! compile errors — they are serialization changes. A handler can typecheck
//! perfectly and still emit a `tools/list` payload no client can read. So
//! rather than calling `VideoTranscriberServer` in-process, every test here
//! spawns the actual binary and speaks JSON-RPC to it, asserting on the bytes
//! a real client would receive.
//!
//! Nothing here touches the network, a GPU, Postgres, or a whisper model:
//! only protocol-level tools are called, so the suite is safe to run in CI on
//! a bare runner.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Protocol version the extension and today's MCP clients negotiate.
const LEGACY_PROTOCOL: &str = "2025-06-18";
/// Draft version rmcp 3.x serves statelessly (SEP-2567).
const MODERN_PROTOCOL: &str = "2026-07-28";

/// Every tool the server is expected to advertise. Pinned deliberately: if a
/// dependency bump silently drops one, listing it here is what catches it.
const EXPECTED_TOOLS: &[&str] = &[
    "transcribe_video",
    "check_dependencies",
    "list_supported_sites",
    "list_transcripts",
    "search_transcripts",
    "get_latest_transcript",
    "delete_transcript",
    "cleanup_old_transcripts",
    "delete_all_transcripts",
];

/// A live server subprocess speaking JSON-RPC over stdio.
struct StdioServer {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl StdioServer {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_video-transcriber-mcp"))
            .args(["--transport", "stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Logs go to stderr; discard so they can't be mistaken for frames.
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the MCP server binary");
        let stdout = child.stdout.take().expect("child stdout");
        Self {
            child,
            reader: BufReader::new(stdout),
        }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().expect("child stdin");
        writeln!(stdin, "{msg}").expect("write to child stdin");
        stdin.flush().expect("flush child stdin");
    }

    /// Read frames until one carries `id`, so notifications or server-initiated
    /// messages interleaved on the stream don't desynchronize the test.
    fn recv(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for response id={id}"
            );
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .expect("read from child stdout");
            assert_ne!(n, 0, "server closed stdout while awaiting id={id}");
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("server emitted non-JSON frame {line:?}: {e}"));
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                return msg;
            }
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        let msg = self.recv(id);
        assert!(
            msg.get("error").is_none(),
            "{method} returned a JSON-RPC error: {msg}"
        );
        msg["result"].clone()
    }

    /// Perform the initialize handshake and return the server's result.
    fn handshake(&mut self, protocol_version: &str) -> Value {
        let result = self.request(
            1,
            "initialize",
            json!({
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": "e2e", "version": "1"},
            }),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        result
    }
}

impl Drop for StdioServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn initialize_negotiates_the_legacy_protocol_version() {
    let mut server = StdioServer::start();
    let result = server.handshake(LEGACY_PROTOCOL);

    assert_eq!(result["protocolVersion"], LEGACY_PROTOCOL);
    assert!(
        result["capabilities"]["tools"].is_object(),
        "server must advertise the tools capability: {result}"
    );
    assert!(
        result["instructions"]
            .as_str()
            .expect("instructions")
            .contains("whisper.cpp"),
        "instructions should describe the server: {result}"
    );
}

#[test]
fn initialize_negotiates_the_modern_protocol_version() {
    let mut server = StdioServer::start();
    let result = server.handshake(MODERN_PROTOCOL);
    assert_eq!(result["protocolVersion"], MODERN_PROTOCOL);
}

#[test]
fn lists_every_expected_tool_with_a_usable_schema() {
    let mut server = StdioServer::start();
    server.handshake(LEGACY_PROTOCOL);
    let result = server.request(2, "tools/list", json!({}));

    let tools = result["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(names, EXPECTED_TOOLS, "advertised tool set changed");

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{name} is missing a description"
        );
        // Clients reject a tool whose inputSchema isn't a JSON Schema object;
        // rmcp also strips unknown schema fields, so assert the shape survives.
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "{name} has a non-object inputSchema: {}",
            tool["inputSchema"]
        );
    }

    // `transcribe_video` is the one tool whose arguments actually matter.
    let transcribe = tools.iter().find(|t| t["name"] == "transcribe_video").unwrap();
    assert_eq!(
        transcribe["inputSchema"]["required"],
        json!(["url"]),
        "transcribe_video must still require `url`"
    );
    assert!(
        transcribe["inputSchema"]["properties"]["model"]["enum"]
            .as_array()
            .is_some_and(|e| e.contains(&json!("base"))),
        "the model enum should survive schema round-tripping"
    );
}

#[test]
fn calls_a_tool_and_returns_text_content() {
    let mut server = StdioServer::start();
    server.handshake(LEGACY_PROTOCOL);
    // `list_supported_sites` is pure static text: no deps, no I/O, no cost.
    let result = server.request(
        2,
        "tools/call",
        json!({"name": "list_supported_sites", "arguments": {}}),
    );

    assert_eq!(result["isError"], json!(false));
    let content = result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(
        content[0]["type"], "text",
        "rmcp renamed Content to ContentBlock; the wire tag must stay `text`"
    );
    assert!(
        content[0]["text"]
            .as_str()
            .expect("text")
            .contains("Supported Video Platforms"),
        "unexpected tool output: {}",
        content[0]
    );
}

#[test]
fn unknown_tools_are_rejected_as_errors() {
    let mut server = StdioServer::start();
    server.handshake(LEGACY_PROTOCOL);
    server.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "no_such_tool", "arguments": {}}
    }));
    let msg = server.recv(2);
    assert_eq!(
        msg["error"]["code"], -32601,
        "expected METHOD_NOT_FOUND: {msg}"
    );
}

#[test]
fn missing_required_arguments_do_not_crash_the_server() {
    let mut server = StdioServer::start();
    server.handshake(LEGACY_PROTOCOL);
    // `transcribe_video` requires `url`; omitting it must be a clean error…
    server.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "transcribe_video", "arguments": {}}
    }));
    let msg = server.recv(2);
    assert_eq!(
        msg["error"]["code"], -32602,
        "expected INVALID_PARAMS: {msg}"
    );

    // …and the session must still be alive afterwards.
    let result = server.request(3, "tools/list", json!({}));
    assert_eq!(result["tools"].as_array().unwrap().len(), EXPECTED_TOOLS.len());
}

/// rmcp 3.x adds SEP-2322 `resultType`, but only for peers that negotiated
/// the 2026-07-28 draft. Emitting it to an older client — or omitting it for
/// a modern one — is a spec violation that no type check would catch.
#[test]
fn result_type_is_emitted_only_for_modern_peers() {
    let mut legacy = StdioServer::start();
    legacy.handshake(LEGACY_PROTOCOL);
    let legacy_result = legacy.request(2, "tools/list", json!({}));
    assert!(
        legacy_result.get("resultType").is_none(),
        "resultType must be absent for {LEGACY_PROTOCOL} peers: {legacy_result}"
    );

    let mut modern = StdioServer::start();
    modern.handshake(MODERN_PROTOCOL);
    let modern_result = modern.request(2, "tools/list", json!({}));
    assert_eq!(
        modern_result["resultType"], "complete",
        "resultType must be `complete` for {MODERN_PROTOCOL} peers: {modern_result}"
    );
}
