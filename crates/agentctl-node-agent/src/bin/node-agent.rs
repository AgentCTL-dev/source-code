//! The `node-agent` binary (RFC 0008, Tier A — control bridge).
//!
//! One per node (a DaemonSet). It (1) periodically **discovers** local agent
//! management sockets on the stock-unix hostPath and logs what each advertises,
//! and (2) serves a small **HTTP API** the aggregated apiserver (RFC 0009) calls
//! to drive a management verb against a specific local agent:
//!
//! ```text
//! POST /v1/agents/{pod_uid}/{verb}   # verb ∈ drain|lame-duck|cancel|status
//! GET  /healthz
//! ```
//!
//! The verb is executed by bridging to that pod's socket via the (blocking)
//! [`ManagementClient`], run on a blocking task. (apiserver↔node-agent mTLS is a
//! later hardening; in-cluster the node-agent is reached by pod IP + NetworkPolicy.)

use std::path::PathBuf;
use std::time::Duration;

use agentctl_node_agent::{discover, DiscoveredAgent, ManagementClient};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[tokio::main]
async fn main() {
    let root: PathBuf = std::env::var("AGENTCTL_SOCKET_ROOT")
        .unwrap_or_else(|_| "/run/agentctl/sockets".to_string())
        .into();
    let interval = Duration::from_secs(
        std::env::var("AGENTCTL_DISCOVERY_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30),
    );
    let bind = std::env::var("AGENTCTL_NODE_AGENT_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());

    // Background: periodic discovery + capability logging.
    tokio::spawn(discovery_loop(root.clone(), interval));

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/agents/{pod_uid}/{verb}", post(verb_handler))
        .with_state(root);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| panic!("bind {bind}: {e}"));
    eprintln!("node-agent: HTTP API on {bind}");
    axum::serve(listener, app).await.expect("serve");
}

/// Execute a management verb against the local agent identified by `pod_uid`.
async fn verb_handler(
    State(root): State<PathBuf>,
    Path((pod_uid, verb)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    if !matches!(verb.as_str(), "drain" | "lame-duck" | "cancel" | "status") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": format!("unknown verb: {verb}") })),
        );
    }
    let socket = root.join(&pod_uid).join("mgmt.sock");
    if !socket.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no socket for pod {pod_uid}") })),
        );
    }

    // ManagementClient is blocking std → run it off the async runtime.
    let result = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let mut client = ManagementClient::connect(&socket).map_err(|e| e.to_string())?;
        client.initialize().map_err(|e| e.to_string())?;
        client.call_tool(&verb, json!({})).map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, Json(json!({ "ok": true, "result": value }))),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "ok": false, "error": e }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

async fn discovery_loop(root: PathBuf, interval: Duration) {
    eprintln!("node-agent: discovering management sockets under {}", root.display());
    loop {
        match discover(&root) {
            Ok(agents) => {
                eprintln!("node-agent: discovered {} agent socket(s)", agents.len());
                for agent in &agents {
                    match probe(agent) {
                        Ok(line) => eprintln!("  + {line}"),
                        Err(e) => eprintln!("  ! pod {} probe failed: {e}", agent.pod_uid),
                    }
                }
            }
            Err(e) => eprintln!("node-agent: discovery error: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}

fn probe(agent: &DiscoveredAgent) -> Result<String, Box<dyn std::error::Error>> {
    let mut client = ManagementClient::connect(&agent.socket)?;
    client.initialize()?;
    let manifest = client.read_capabilities()?;
    Ok(format!(
        "pod {} -> server={} contract={} mode={:?}",
        agent.pod_uid,
        client.server_name.as_deref().unwrap_or("?"),
        manifest.contract_version,
        manifest.mode.as_deref().unwrap_or("?"),
    ))
}
