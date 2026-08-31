// SPDX-License-Identifier: BUSL-1.1
//! The credential-injection WITNESS (P5-3 e2e): a minimal streamable-HTTP MCP
//! server (plain HTTP, in-cluster fixture) whose single tool `auth.echo`
//! returns the `Authorization` header its call arrived with. Federated behind
//! the tenant mcpg with `auth.mode: oauth_impersonation`, the echoed value
//! proves WHICH credential the gateway injected upstream — the per-user token
//! minted by identity's `/v1/exchange`, not the caller's own bearer.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Default)]
struct Stats {
    calls: std::sync::atomic::AtomicU64,
}

#[tokio::main]
async fn main() {
    let stats = Arc::new(Stats::default());
    let router = Router::new()
        .route("/mcp", post(mcp))
        .route("/readyz", get(|| async { "ok" }))
        .with_state(stats);
    let addr: std::net::SocketAddr = "0.0.0.0:8080".parse().unwrap();
    eprintln!("mock-echo-mcp: serving http on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, router).await.expect("serve");
}

async fn mcp(State(stats): State<Arc<Stats>>, headers: HeaderMap, body: String) -> Response {
    let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    // A notification (no id) is acknowledged without a body.
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock-echo-mcp", "version": "0" },
        }),
        "tools/list" => json!({ "tools": [{
            "name": "auth.echo",
            "description": "Echo the Authorization header this call arrived with.",
            "inputSchema": { "type": "object", "properties": {} },
        }] }),
        "tools/call" => {
            let tool = req
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            if tool != "auth.echo" {
                let resp = json!({ "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32602, "message": format!("no tool {tool:?}") } });
                return mcp_ok(resp);
            }
            stats
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let auth = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>");
            json!({ "content": [{ "type": "text", "text": auth }] })
        }
        _ => {
            let resp = json!({ "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("no {method} here") } });
            return mcp_ok(resp);
        }
    };
    mcp_ok(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn mcp_ok(resp: Value) -> Response {
    (
        StatusCode::OK,
        [("mcp-session-id", "echo-session-1")],
        Json(resp),
    )
        .into_response()
}
