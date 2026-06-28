// SPDX-License-Identifier: Apache-2.0
//! `mock-agent` — a minimal **conformant-agent stand-in** for dev / e2e /
//! conformance.
//!
//! It is NOT a real agent (no agentic loop, no intelligence): it just serves the
//! contract **management profile** (the self-MCP, RFC 0005/0015) as NDJSON
//! JSON-RPC over the substrate unix socket — `initialize`, `tools/list` (status +
//! the operator tools), `resources/read agent://capabilities|inventory`, and
//! `tools/call`. That is exactly the surface agentctl's node-agent bridge drives,
//! so it lets the keystone path be exercised end-to-end without the real runtime
//! — and demonstrates P0: agentctl manages *any* conformant agent.
//!
//! Bind address comes from the contract bind instruction
//! `AGENT_SERVE_MCP` (e.g. `unix:/run/agent/mgmt.sock`),
//! which the operator injects (RFC 0002 §6.1).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::{env, fs, thread};

use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-11-25";

/// Mock Prometheus metrics (text exposition 0.0.4) served as an MCP resource.
const METRICS: &str = "\
# HELP agent_pending_events Reactive events awaiting processing.
# TYPE agent_pending_events gauge
agent_pending_events 3
# HELP agent_tokens_total Tokens consumed by the agentic loop.
# TYPE agent_tokens_total counter
agent_tokens_total{direction=\"input\"} 1200
agent_tokens_total{direction=\"output\"} 340
# HELP agent_tool_calls_total MCP tool calls made.
# TYPE agent_tool_calls_total counter
agent_tool_calls_total 17
";

fn main() {
    let serve = env::var("AGENT_SERVE_MCP")
        .expect("AGENT_SERVE_MCP must be set (e.g. unix:/run/agent/mgmt.sock)");
    let path = serve.strip_prefix("unix:").unwrap_or(&serve).to_string();

    let _ = fs::remove_file(&path); // clear a stale socket from a prior pod
    let listener = UnixListener::bind(&path).unwrap_or_else(|e| panic!("bind {path}: {e}"));
    eprintln!("mock-agent: serving management profile on unix:{path}");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || serve_conn(stream));
            }
            Err(e) => eprintln!("mock-agent: accept error: {e}"),
        }
    }
}

/// One management connection: NDJSON JSON-RPC, requests get a reply, notifications
/// are ignored, the loop ends on EOF.
fn serve_conn(stream: UnixStream) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return; // peer hung up
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // a notification (e.g. notifications/initialized)
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        // A2A streaming (RFC 0020): one request → MULTIPLE same-id response frames
        // (working → artifact-update echo → completed/final), then resume reading.
        if method == "a2a.SendStreamingMessage" {
            eprintln!("mock-agent: a2a.SendStreamingMessage (stream)");
            let input = msg
                .pointer("/params/message/parts/0/text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let tid = msg
                .pointer("/params/message/messageId")
                .and_then(Value::as_str)
                .unwrap_or("task-1");
            let frames = [
                json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "kind": "status-update", "taskId": tid,
                    "status": { "state": "working" }, "final": false
                }}),
                json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "kind": "artifact-update", "taskId": tid,
                    "artifact": { "artifactId": "art-1", "parts": [
                        { "kind": "text", "text": format!("echo: {input}") }
                    ]}
                }}),
                json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "kind": "status-update", "taskId": tid,
                    "status": { "state": "completed" }, "final": true
                }}),
            ];
            for frame in &frames {
                if write_line(&mut writer, frame).is_err() {
                    return;
                }
            }
            continue;
        }
        let response = match dispatch(method, &msg) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        };
        if write_line(&mut writer, &response).is_err() {
            return;
        }
    }
}

fn dispatch(method: &str, msg: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {}, "resources": { "subscribe": true } },
            "serverInfo": { "name": "mock-agent", "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": [
            { "name": "status" },
            { "name": "drain" },
            { "name": "lame-duck" },
            { "name": "cancel" }
        ]})),
        "resources/read" => {
            let uri = msg
                .pointer("/params/uri")
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = match uri {
                "agent://capabilities" => manifest().to_string(),
                "agent://inventory" => json!({ "agents": [], "warm_sessions": 0 }).to_string(),
                // Prometheus 0.0.4 text exposed as an MCP resource (RFC 0010 / P4):
                // the node-agent reads this over the socket and re-exposes it, so a
                // networkless agent is still scrapeable.
                "agent://metrics" => METRICS.to_string(),
                other => {
                    return Err((-32602, format!("unknown resource: {other}")));
                }
            };
            Ok(
                json!({ "contents": [{ "uri": uri, "mimeType": "application/json", "text": text }] }),
            )
        }
        // A2A reference methods (RFC 0020 binding; the gateway translates the
        // spec slash-form message/send|tasks/get|tasks/cancel → these). A served
        // run IS a Task; this mock echoes the input back as the distillate.
        "a2a.SendMessage" => {
            let input = msg
                .pointer("/params/message/parts/0/text")
                .and_then(Value::as_str)
                .unwrap_or("");
            eprintln!("mock-agent: a2a.SendMessage");
            let id = msg
                .pointer("/params/message/messageId")
                .and_then(Value::as_str)
                .unwrap_or("task-1");
            Ok(json!({
                "id": id,
                "contextId": "ctx-1",
                "status": { "state": "completed" },
                "artifacts": [{
                    "artifactId": "art-1",
                    "parts": [{ "kind": "text", "text": format!("echo: {input}") }]
                }],
                "kind": "task"
            }))
        }
        "a2a.GetTask" => {
            let id = msg
                .pointer("/params/id")
                .and_then(Value::as_str)
                .unwrap_or("task-1");
            eprintln!("mock-agent: a2a.GetTask {id}");
            Ok(json!({
                "id": id,
                "contextId": "ctx-1",
                "status": { "state": "completed" },
                "artifacts": [{
                    "artifactId": "art-1",
                    "parts": [{ "kind": "text", "text": "echo: (mock)" }]
                }],
                "kind": "task"
            }))
        }
        "a2a.CancelTask" => {
            let id = msg
                .pointer("/params/id")
                .and_then(Value::as_str)
                .unwrap_or("task-1");
            eprintln!("mock-agent: a2a.CancelTask {id}");
            Ok(json!({
                "id": id,
                "contextId": "ctx-1",
                "status": { "state": "canceled" },
                "kind": "task"
            }))
        }
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            eprintln!("mock-agent: tools/call {name} (operator tool invoked)");
            Ok(
                json!({ "content": [{ "type": "text", "text": format!("{name}: ok (mock)") }], "isError": false }),
            )
        }
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

/// A minimal but contract-valid capabilities manifest (agent-contract-client
/// parses this). Identity comes from the downward-API env the operator injects.
fn manifest() -> Value {
    let serve = env::var("AGENT_SERVE_MCP").unwrap_or_default();
    let id = |name: &str| {
        env::var(name)
            .ok()
            .map(Value::String)
            .unwrap_or(Value::Null)
    };
    json!({
        "contract_version": "1.0",
        "agent_version": format!("mock-agent-{}", env!("CARGO_PKG_VERSION")),
        "build_features": [],
        "identity": {
            "run_id": "mock-run",
            "instance": id("AGENT_POD_NAME"),
            "namespace": id("AGENT_POD_NAMESPACE"),
            "node": id("AGENT_NODE_NAME"),
            "uid": id("AGENT_POD_UID")
        },
        "mode": "reactive",
        "model": null,
        "intelligence": { "endpoints": 0, "transport": null, "healthy": "unknown" },
        "limits": {},
        "mcp_servers": [],
        "a2a_peers": [],
        "exec_enabled": false,
        "allow_trifecta": false,
        "surfaces": {
            "management": if serve.is_empty() { Value::Bool(false) } else { Value::String(serve) },
            "metrics": false,
            "a2a": {
                "version": "1.0",
                "streaming": false,
                "methods": ["a2a.SendMessage", "a2a.SendStreamingMessage", "a2a.GetTask", "a2a.CancelTask"]
            },
            "events": false,
            "operator_tools": ["drain", "lame-duck", "cancel"],
            "metrics_schema": "1.0",
            "report_schema": "1.0",
            "exit_codes": "1.0",
            "config_validate": false,
            "config_schema": false,
            "hot_reload": false,
            "intelligence": true,
            "cluster": true,
            "standby": false,
            "shard": null
        }
    })
}

fn write_line(w: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(v)?;
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}
