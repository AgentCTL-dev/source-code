// SPDX-License-Identifier: BUSL-1.1
//! Hermetic test of the management bridge against a mock server that replicates
//! the source-confirmed agent wire (NDJSON JSON-RPC, the `initialize` handshake,
//! `tools/list` with operator tools on a management origin, `resources/read
//! agent://capabilities` → the real golden fixture, `tools/call drain`).
//!
//! This exercises the full read path the MVP needs — fetch a manifest over the
//! wire and parse it with the typed contract client — without a cluster or a
//! running agent. (Live interop against the reference binary is a follow-up: it
//! needs a serve-stable reactive invocation with real intelligence/MCP.)

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

use agentctl_node_agent::ManagementClient;
use serde_json::{json, Value};

const CAPABILITIES_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contract/fixtures/capabilities/default.json"
);

/// One mock agent connection: reply to requests per the contract wire, ignore
/// notifications, return on EOF.
fn serve_one(stream: UnixStream, capabilities_json: String) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut line = String::new();
    while {
        line.clear();
        reader.read_line(&mut line).unwrap_or(0) > 0
    } {
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Notifications carry no id and get no reply.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}, "resources": {"subscribe": true}},
                "serverInfo": {"name": "agentd", "version": "2.5.0"}
            }),
            // A management-origin peer sees the operator tools too.
            "tools/list" => json!({"tools": [
                {"name": "status"},
                {"name": "subagent.spawn"},
                {"name": "drain"},
                {"name": "lame-duck"},
                {"name": "cancel"}
            ]}),
            "resources/read" => {
                let uri = msg
                    .pointer("/params/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                assert_eq!(uri, "agent://capabilities", "unexpected resource read");
                json!({"contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": capabilities_json,
                }]})
            }
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                assert_eq!(name, "drain");
                json!({"content": [{"type": "text", "text": "draining"}], "isError": false})
            }
            other => panic!("mock got unexpected method: {other}"),
        };
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let mut buf = serde_json::to_vec(&resp).unwrap();
        buf.push(b'\n');
        if writer.write_all(&buf).is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

#[test]
fn management_bridge_drives_the_wire() {
    let sock = std::env::temp_dir().join(format!("acc-mgmt-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind mock socket");

    let caps = std::fs::read_to_string(CAPABILITIES_FIXTURE).expect("read fixture");
    let server = {
        let caps = caps.clone();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            serve_one(stream, caps);
        })
    };

    // Drive the bridge.
    let mut client = ManagementClient::connect(&sock).expect("connect");
    client.initialize().expect("initialize");
    assert_eq!(client.server_name.as_deref(), Some("agentd"));
    assert_eq!(client.server_version.as_deref(), Some("2.5.0"));

    let tools = client.list_tools().expect("tools/list");
    assert!(tools.contains(&"drain".to_string()));
    assert!(tools.contains(&"cancel".to_string()));

    // Fetch the manifest over the wire and parse it with the typed client.
    let manifest = client.read_capabilities().expect("read capabilities");
    let v = manifest.negotiate().expect("negotiate");
    assert_eq!((v.major, v.minor), (1, 0));
    assert_eq!(manifest.version(), Some("2.5.0"));
    assert_eq!(
        manifest.surfaces.operator_tools,
        ["drain", "lame-duck", "pause", "resume", "cancel"]
    );
    assert!(!manifest.surfaces.management.is_served()); // the default fixture has it off

    // Call an operator tool.
    let drain = client.drain().expect("drain");
    assert_eq!(drain.pointer("/isError"), Some(&json!(false)));

    drop(client); // EOF → server returns
    server.join().expect("server thread");
    let _ = std::fs::remove_file(&sock);
}

/// One mock streaming connection: handshake, then answer `a2a.SendStreamingMessage`
/// with the contract-pinned multi-frame sequence (working → artifact-update echo →
/// completed/final), all carrying the request's id.
fn serve_stream(stream: UnixStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut line = String::new();
    while {
        line.clear();
        reader.read_line(&mut line).unwrap_or(0) > 0
    } {
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notification
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let frames: Vec<Value> = match method {
            "initialize" => vec![json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "serverInfo": {"name": "mock-agent", "version": "0.0.0"}
            })],
            "a2a.SendStreamingMessage" => {
                let input = msg
                    .pointer("/params/message/parts/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let tid = msg
                    .pointer("/params/message/messageId")
                    .and_then(Value::as_str)
                    .unwrap_or("task-1");
                vec![
                    json!({"kind": "status-update", "taskId": tid, "status": {"state": "working"}, "final": false}),
                    json!({"kind": "artifact-update", "taskId": tid, "artifact": {"artifactId": "art-1", "parts": [{"kind": "text", "text": format!("echo: {input}")}]}}),
                    json!({"kind": "status-update", "taskId": tid, "status": {"state": "completed"}, "final": true}),
                ]
            }
            other => panic!("mock got unexpected method: {other}"),
        };
        for result in frames {
            let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
            let mut buf = serde_json::to_vec(&resp).unwrap();
            buf.push(b'\n');
            if writer.write_all(&buf).is_err() {
                return;
            }
            let _ = writer.flush();
        }
    }
}

#[test]
fn streaming_bridge_collects_frames_until_final() {
    let sock = std::env::temp_dir().join(format!("acc-stream-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind mock socket");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        serve_stream(stream);
    });

    let mut client = ManagementClient::connect(&sock).expect("connect");
    client.initialize().expect("initialize");

    let params = json!({
        "message": {
            "role": "user",
            "parts": [{"kind": "text", "text": "hello"}],
            "messageId": "tid-7"
        }
    });
    let mut frames: Vec<Value> = Vec::new();
    client
        .stream("a2a.SendStreamingMessage", params, |frame| {
            frames.push(frame)
        })
        .expect("stream");

    // Exactly the three pinned frames, stopping after the final one.
    assert_eq!(frames.len(), 3, "should stop after the final frame");
    assert_eq!(frames[0]["kind"], "status-update");
    assert_eq!(frames[0]["status"]["state"], "working");
    assert_eq!(frames[0]["final"], false);
    assert_eq!(frames[1]["kind"], "artifact-update");
    assert_eq!(frames[1]["artifact"]["parts"][0]["text"], "echo: hello");
    assert_eq!(frames[2]["kind"], "status-update");
    assert_eq!(frames[2]["status"]["state"], "completed");
    assert_eq!(frames[2]["final"], true);
    // The taskId echoes the request's messageId.
    assert_eq!(frames[2]["taskId"], "tid-7");

    drop(client); // EOF → server returns
    server.join().expect("server thread");
    let _ = std::fs::remove_file(&sock);
}
