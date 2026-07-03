// SPDX-License-Identifier: Apache-2.0
//! `mock-agent` — a minimal **conformant-agent stand-in** for dev / e2e /
//! conformance, at **contract 2.0**.
//!
//! It is NOT a real agent (no agentic loop, no intelligence): it serves the
//! contract **management + A2A profile** (the self-MCP, RFC 0005/0015/0021) over
//! **mTLS HTTPS `POST /mcp`** — `initialize`, `tools/list`, `resources/read
//! agent://capabilities|inventory|metrics`, `tools/call`, the bare A2A methods,
//! and the `a2a.*` admin verbs — plus a **plaintext `/readyz` + `/metrics`**
//! listener for probes/scrape. That is exactly the surface the agentctl APIServer
//! (management verbs) and A2A gateway drive, so it lets the keystone path be
//! exercised end-to-end without the real runtime — and demonstrates P0: agentctl
//! manages *any* conformant agent.
//!
//! **Contract 2.0 — the network is the substrate.** The agent *serves* mTLS HTTPS
//! and *dials nothing*. A caller that presents a client cert the TLS acceptor
//! verified against the pinned client CA (`--serve-client-ca`) is
//! `PeerOrigin::Management`; the listener **requires** a client cert, so every
//! request reaching `/mcp` is Management. The A2A methods are the bare PascalCase
//! spec-§9 names (the legacy `a2a.`-prefixed spelling is also accepted); streaming
//! is an SSE `text/event-stream` terminated by the terminal task state (no `final`
//! flag).
//!
//! CLI (the flags the operator renders — RFC 0021 §5): `--serve-mcp
//! https://0.0.0.0:8443 --serve-cert <p> --serve-key <p> --serve-client-ca <p>`;
//! `--tls-ca`/`--mode`/`--mcp`/… are accepted and ignored (the mock dials
//! nothing). The metrics/readiness listener address comes from `AGENT_METRICS_ADDR`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::{env, fs};

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rustls::server::WebPkiClientVerifier;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig};
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-11-25";

/// Mock Prometheus metrics (text exposition 0.0.4) served on the metrics listener
/// (contract 2.0: the pod is network-attached and scraped directly — no proxy).
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

/// The serving-TLS material the operator mounts (RFC 0021 §5).
struct Serve {
    addr: SocketAddr,
    cert: String,
    key: String,
    client_ca: String,
}

/// Read `--flag value` out of argv (the operator renders these; unknown flags and
/// their values are ignored, so `--mode`/`--tls-ca`/`--mcp`/… are tolerated).
fn arg_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Parse `https://host:port` (or a bare `host:port`) into a `SocketAddr`.
fn parse_serve_addr(v: &str) -> SocketAddr {
    let hostport = v.strip_prefix("https://").unwrap_or(v);
    hostport
        .parse()
        .unwrap_or_else(|e| panic!("--serve-mcp {v}: bad host:port: {e}"))
}

#[tokio::main]
async fn main() {
    // axum-server is built with `tls-rustls-no-provider`, so the process installs
    // the ring crypto provider (matches the control-plane components).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let args: Vec<String> = env::args().collect();
    let serve = Serve {
        addr: parse_serve_addr(
            &arg_val(&args, "--serve-mcp")
                .or_else(|| env::var("AGENT_SERVE_MCP").ok())
                .expect("--serve-mcp https://HOST:PORT (or AGENT_SERVE_MCP) required"),
        ),
        cert: arg_val(&args, "--serve-cert").expect("--serve-cert PATH required"),
        key: arg_val(&args, "--serve-key").expect("--serve-key PATH required"),
        client_ca: arg_val(&args, "--serve-client-ca").expect("--serve-client-ca PATH required"),
    };

    // Plaintext metrics + readiness listener (AGENT_METRICS_ADDR, /readyz).
    if let Ok(metrics_addr) = env::var("AGENT_METRICS_ADDR") {
        let addr: SocketAddr = metrics_addr
            .parse()
            .unwrap_or_else(|e| panic!("AGENT_METRICS_ADDR {metrics_addr}: {e}"));
        tokio::spawn(async move {
            let app = Router::new()
                .route("/healthz", get(|| async { "ok" }))
                .route("/readyz", get(|| async { "ok" }))
                .route("/metrics", get(|| async {
                    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], METRICS)
                }));
            let l = tokio::net::TcpListener::bind(addr)
                .await
                .unwrap_or_else(|e| panic!("bind metrics {addr}: {e}"));
            eprintln!("mock-agent: metrics/readiness on http://{addr}");
            axum::serve(l, app).await.expect("serve metrics");
        });
    }

    // mTLS self-MCP listener: present the serving cert AND REQUIRE a client cert
    // chained to the pinned client CA (a verified cert ⇒ Management).
    let tls = build_mtls_config(&serve.cert, &serve.key, &serve.client_ca)
        .unwrap_or_else(|e| panic!("build mock-agent TLS: {e}"));
    let mcp = Router::new().route("/mcp", post(handle_mcp));
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));
    eprintln!("mock-agent: serving mTLS self-MCP on https://{}/mcp", serve.addr);
    axum_server::bind_rustls(serve.addr, rustls_config)
        .serve(mcp.into_make_service())
        .await
        .expect("serve mTLS /mcp");
}

/// rustls server config: the mock's serving cert + a **required** client-cert
/// verifier chained to `client_ca` (mirrors `coordination::mtls::build_tls_config`).
fn build_mtls_config(cert: &str, key: &str, client_ca: &str) -> Result<ServerConfig, String> {
    let certs = load_certs(cert)?;
    let key = load_key(key)?;
    let ca = load_ca(client_ca)?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(ca))
        .build()
        .map_err(|e| format!("client verifier: {e}"))?;
    ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("server config: {e}"))
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut r = std::io::BufReader::new(fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?);
    rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read certs {path}: {e}"))
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>, String> {
    let mut r = std::io::BufReader::new(fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?);
    rustls_pemfile::private_key(&mut r)
        .map_err(|e| format!("read key {path}: {e}"))?
        .ok_or_else(|| format!("no private key in {path}"))
}

fn load_ca(path: &str) -> Result<RootCertStore, String> {
    let mut r = std::io::BufReader::new(fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?);
    let mut roots = RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut r) {
        roots
            .add(c.map_err(|e| format!("parse CA {path}: {e}"))?)
            .map_err(|e| format!("add CA {path}: {e}"))?;
    }
    if roots.is_empty() {
        return Err(format!("CA file {path} had no certs"));
    }
    Ok(roots)
}

/// `POST /mcp`: one JSON-RPC request (or a batch array) in, one JSON-RPC response
/// out — `application/json` for the unary methods, `text/event-stream` (SSE) for
/// `SendStreamingMessage`. Reaching here means the client cert verified, so the
/// caller is `Management`.
async fn handle_mcp(body: String) -> Response {
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return rpc_error(Value::Null, -32700, &format!("parse error: {e}")),
    };
    // Batch (array) → array of responses (no streaming in a batch).
    if let Some(arr) = parsed.as_array() {
        let out: Vec<Value> = arr.iter().filter_map(unary).collect();
        return Json(Value::Array(out)).into_response();
    }
    let id = parsed.get("id").cloned();
    let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
    // A2A streaming: one request → several same-id SSE frames terminated by the
    // terminal task state + stream close (contract 2.0: no `final` flag).
    if matches!(method, "SendStreamingMessage" | "a2a.SendStreamingMessage") {
        return sse_stream(id.unwrap_or(json!("task-1")), &parsed);
    }
    match unary(&parsed) {
        Some(resp) => Json(resp).into_response(),
        None => StatusCode::ACCEPTED.into_response(), // a notification (no id)
    }
}

/// Build the JSON-RPC response for one unary request, or `None` for a notification.
fn unary(msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?; // no id ⇒ notification, no reply
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    Some(match dispatch(method, msg) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}

fn dispatch(method: &str, msg: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {}, "resources": { "subscribe": true } },
            "serverInfo": { "name": "mock-agent", "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        // In contract 2.0 the operator admin family is the a2a.* JSON-RPC methods,
        // not MCP tools; tools/list carries only the read `status` tool.
        "tools/list" => Ok(json!({ "tools": [ { "name": "status" } ] })),
        "resources/read" => {
            let uri = msg.pointer("/params/uri").and_then(Value::as_str).unwrap_or("");
            let text = match uri {
                "agent://capabilities" => manifest().to_string(),
                "agent://inventory" => json!({ "agents": [], "warm_sessions": 0 }).to_string(),
                // Prometheus 0.0.4 text as an in-band MCP resource (also served on
                // the /metrics listener for direct scrape).
                "agent://metrics" => METRICS.to_string(),
                other => return Err((-32602, format!("unknown resource: {other}"))),
            };
            Ok(json!({ "contents": [{ "uri": uri, "mimeType": "application/json", "text": text }] }))
        }
        // A2A methods — contract 2.0 bare PascalCase (a2a.-prefixed accepted). A
        // served run IS a Task; this mock echoes the input back as the distillate.
        // SendMessage returns the SendMessageResponse oneof {"task": <Task>}.
        "SendMessage" | "a2a.SendMessage" => {
            let input = a2a_text(msg);
            let id = a2a_msg_id(msg);
            eprintln!("mock-agent: SendMessage");
            Ok(json!({ "task": task(&id, "TASK_STATE_COMPLETED", Some(&format!("echo: {input}"))) }))
        }
        "GetTask" | "a2a.GetTask" => {
            let id = a2a_id(msg);
            eprintln!("mock-agent: GetTask {id}");
            Ok(task(&id, "TASK_STATE_COMPLETED", Some("echo: (mock)")))
        }
        "CancelTask" | "a2a.CancelTask" => {
            let id = a2a_id(msg);
            eprintln!("mock-agent: CancelTask {id}");
            Ok(task(&id, "TASK_STATE_CANCELED", None))
        }
        "ListTasks" | "a2a.ListTasks" => Ok(json!({ "tasks": [] })),
        // Operator admin verbs (a2a.* extensions). The mock acks; it does not model
        // the full drain/pause lifecycle (agentd owns that).
        "a2a.Drain" => Ok(json!({ "draining": true, "in_flight": 0 })),
        "a2a.LameDuck" => Ok(json!({ "ready": false, "in_flight": 0 })),
        "a2a.Pause" => Ok(json!({ "paused": true, "affected": 0 })),
        "a2a.Resume" => Ok(json!({ "paused": false, "affected": 0 })),
        "a2a.Cancel" => Ok(json!({ "cancelling": true, "subtree_size": 0 })),
        "tools/call" => {
            let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
            eprintln!("mock-agent: tools/call {name}");
            Ok(json!({ "content": [{ "type": "text", "text": format!("{name}: ok (mock)") }], "isError": false }))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

/// SSE `text/event-stream` of same-id StreamResponse frames: statusUpdate(WORKING)
/// → artifactUpdate(echo) → statusUpdate(COMPLETED), then the stream closes.
/// Contract 2.0 carries no `final` flag — termination is the terminal task state.
fn sse_stream(id: Value, msg: &Value) -> Response {
    let input = a2a_text(msg);
    let tid = a2a_msg_id(msg);
    eprintln!("mock-agent: SendStreamingMessage (stream)");
    let frames = [
        json!({ "jsonrpc": "2.0", "id": id, "result": {
            "statusUpdate": { "taskId": tid, "status": { "state": "TASK_STATE_WORKING" } } } }),
        json!({ "jsonrpc": "2.0", "id": id, "result": {
            "artifactUpdate": { "taskId": tid, "artifact": {
                "artifactId": "art-1", "parts": [{ "text": format!("echo: {input}") }] },
                "lastChunk": true } } }),
        json!({ "jsonrpc": "2.0", "id": id, "result": {
            "statusUpdate": { "taskId": tid, "status": { "state": "TASK_STATE_COMPLETED" } } } }),
    ];
    let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn a2a_text(msg: &Value) -> String {
    msg.pointer("/params/message/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
fn a2a_msg_id(msg: &Value) -> String {
    msg.pointer("/params/message/messageId")
        .and_then(Value::as_str)
        .unwrap_or("task-1")
        .to_string()
}
fn a2a_id(msg: &Value) -> String {
    msg.pointer("/params/id")
        .and_then(Value::as_str)
        .unwrap_or("task-1")
        .to_string()
}

/// A2A Task in proto3-JSON (`TASK_STATE_*`, `{text}` Part oneof).
fn task(id: &str, state: &str, distillate: Option<&str>) -> Value {
    let mut t = json!({
        "id": id, "contextId": "ctx-1",
        "status": { "state": state }, "kind": "task"
    });
    if let Some(text) = distillate {
        t["artifacts"] = json!([{ "artifactId": "art-1", "parts": [{ "text": text }] }]);
    }
    t
}

/// A minimal but contract-2.0-valid capabilities manifest (agent-contract-client
/// parses this). Identity comes from the downward-API env the operator injects.
fn manifest() -> Value {
    let serve = env::var("AGENT_SERVE_MCP").unwrap_or_default();
    let id = |name: &str| env::var(name).ok().map(Value::String).unwrap_or(Value::Null);
    json!({
        "contract_version": "2.0",
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
            "metrics": env::var("AGENT_METRICS_ADDR").ok().map(Value::String).unwrap_or(Value::Bool(false)),
            "a2a": {
                "version": "1.0",
                "streaming": true,
                // Contract 2.0: bare PascalCase spec-§9 A2A method names.
                "methods": ["SendMessage", "SendStreamingMessage", "GetTask", "CancelTask", "ListTasks", "SubscribeToTask"]
            },
            "events": false,
            // Contract 2.0: operator tools are the a2a.* admin JSON-RPC methods.
            "operator_tools": ["a2a.Drain", "a2a.LameDuck", "a2a.Pause", "a2a.Resume", "a2a.Cancel"],
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

#[cfg(test)]
mod tests {
    use super::*;

    // The transport (mTLS axum-server) mirrors the proven coordination/mcpgateway
    // patterns and is validated end-to-end in the kind e2e (a pod, not a local
    // listener — this sandbox kills bound listeners). These tests cover the pure
    // dispatch + manifest logic, which is where a wire bug would actually live.

    #[test]
    fn manifest_is_contract_2_0_conformant() {
        // The REAL typed client (the conformance oracle) must parse + negotiate it.
        std::env::set_var("AGENT_POD_NAMESPACE", "agents");
        let json = manifest().to_string();
        let m = agent_contract_client::parse_manifest(&json).expect("manifest parses");
        let v = m.negotiate().expect("negotiates");
        assert_eq!(
            v,
            agent_contract_client::ContractVersion { major: 2, minor: 0 }
        );
        // Bare PascalCase A2A + a2a.* operator tools + no exec (contract 2.0).
        let a2a = m.surfaces.a2a.info().expect("a2a served");
        assert!(a2a.methods.iter().any(|x| x == "SendMessage"));
        assert!(!a2a.methods.iter().any(|x| x.starts_with("a2a.")));
        assert_eq!(m.surfaces.operator_tools.first().map(String::as_str), Some("a2a.Drain"));
        assert!(!m.exec_enabled);
    }

    #[test]
    fn dispatch_core_methods() {
        let init = dispatch("initialize", &json!({})).unwrap();
        assert_eq!(init["serverInfo"]["name"], "mock-agent");

        // SendMessage → SendMessageResponse oneof {"task": <Task>}, proto3 state.
        let msg = json!({"params":{"message":{"messageId":"t1","parts":[{"text":"hi"}]}}});
        let r = dispatch("SendMessage", &msg).unwrap();
        assert_eq!(r["task"]["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(r["task"]["artifacts"][0]["parts"][0]["text"], "echo: hi");

        // Bare and legacy a2a.-prefixed spellings both dispatch.
        assert!(dispatch("GetTask", &json!({"params":{"id":"x"}})).is_ok());
        assert!(dispatch("a2a.GetTask", &json!({"params":{"id":"x"}})).is_ok());

        // Operator admin verb.
        assert_eq!(dispatch("a2a.Drain", &json!({})).unwrap()["draining"], true);

        // Unknown method → METHOD_NOT_FOUND.
        assert_eq!(dispatch("nope", &json!({})).unwrap_err().0, -32601);
    }

    #[test]
    fn arg_and_addr_parsing() {
        let args: Vec<String> = ["--serve-mcp", "https://0.0.0.0:8443", "--mode", "reactive"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(arg_val(&args, "--serve-mcp").as_deref(), Some("https://0.0.0.0:8443"));
        assert_eq!(arg_val(&args, "--serve-cert"), None);
        assert_eq!(parse_serve_addr("https://0.0.0.0:8443").to_string(), "0.0.0.0:8443");
        assert_eq!(parse_serve_addr("127.0.0.1:9000").to_string(), "127.0.0.1:9000");
    }
}
