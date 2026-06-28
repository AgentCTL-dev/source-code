//! The `node-agent` binary (RFC 0008, Tier A — control bridge).
//!
//! One per node (a DaemonSet). It (1) periodically **discovers** local agent
//! management sockets on the stock-unix hostPath and logs what each advertises,
//! and (2) serves a small **HTTP API** the aggregated apiserver (RFC 0009) calls
//! to drive a management verb against a specific local agent:
//!
//! ```text
//! POST /v1/agents/{pod_uid}/{verb}          # verb ∈ drain|lame-duck|cancel|status
//! POST /v1/agents/{pod_uid}/a2a             # bridge a reference a2a.* JSON-RPC request
//! GET  /v1/agents/{pod_uid}/capabilities    # raw capabilities manifest (card projection)
//! GET  /healthz
//! ```
//!
//! The verb is executed by bridging to that pod's socket via the (blocking)
//! [`ManagementClient`], run on a blocking task. (apiserver↔node-agent mTLS is a
//! later hardening; in-cluster the node-agent is reached by pod IP + NetworkPolicy.)

use std::path::PathBuf;
use std::time::Duration;

use agentctl_node_agent::mgmt::URI_CAPABILITIES;
use agentctl_node_agent::{discover, metrics, DiscoveredAgent, Error, ManagementClient};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
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
        .route("/metrics", get(metrics_handler))
        .route("/v1/agents/{pod_uid}/a2a", post(a2a_handler))
        .route(
            "/v1/agents/{pod_uid}/capabilities",
            get(capabilities_handler),
        )
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
        client
            .call_tool(&verb, json!({}))
            .map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, Json(json!({ "ok": true, "result": value }))),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": e })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

/// Bridge a **reference** A2A JSON-RPC request to the local agent and relay its
/// response (the MVP bridging chain, agentctl RFC 0013). The body is the
/// reference request verbatim (`{jsonrpc,id,method,params}`) — the gateway has
/// already translated the spec slash-form (`message/send`, …) to the reference
/// name (`a2a.SendMessage`, …). We forward `method`/`params` over the socket and
/// wrap the outcome back into a JSON-RPC envelope carrying the original `id`.
///
/// An agent-level JSON-RPC error (e.g. `TASK_NOT_FOUND` −32001) is relayed with
/// its own code; only a bridge-level failure (no socket, connect/handshake/IO)
/// is reported as −32011.
async fn a2a_handler(
    State(root): State<PathBuf>,
    Path(pod_uid): Path<String>,
    Json(req): Json<Value>,
) -> Json<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let socket = root.join(&pod_uid).join("mgmt.sock");

    // ManagementClient is blocking std → run it off the async runtime.
    let bridged = tokio::task::spawn_blocking(move || -> Result<Value, (i64, String)> {
        if !socket.exists() {
            return Err((-32011, format!("no socket for pod {pod_uid}")));
        }
        let mut client = ManagementClient::connect(&socket).map_err(|e| (-32011, e.to_string()))?;
        client.initialize().map_err(|e| (-32011, e.to_string()))?;
        client.call(&method, params).map_err(|e| match e {
            // A genuine agent error: relay its code so the gateway can map it.
            Error::Rpc { code, message } => (code, message),
            // Transport/protocol/json: a bridge failure.
            other => (-32011, other.to_string()),
        })
    })
    .await;

    let response = match bridged {
        Ok(Ok(result)) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Ok(Err((code, message))) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
        Err(e) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32011, "message": e.to_string() }
        }),
    };
    Json(response)
}

/// Fetch the local agent's capabilities manifest as **raw JSON** (RFC 0005
/// §3.6). The contract `Manifest` is deserialize-only, so we return the wire
/// text parsed straight to a [`Value`] — the lossless passthrough the gateway
/// projects into an Agent Card (RFC 0013), with no re-serialization round-trip.
async fn capabilities_handler(
    State(root): State<PathBuf>,
    Path(pod_uid): Path<String>,
) -> (StatusCode, Json<Value>) {
    let socket = root.join(&pod_uid).join("mgmt.sock");
    if !socket.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no socket for pod {pod_uid}") })),
        );
    }

    let fetched = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let mut client = ManagementClient::connect(&socket).map_err(|e| e.to_string())?;
        client.initialize().map_err(|e| e.to_string())?;
        let text = client
            .read_resource_text(URI_CAPABILITIES)
            .map_err(|e| e.to_string())?;
        serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())
    })
    .await;

    match fetched {
        Ok(Ok(manifest)) => (StatusCode::OK, Json(manifest)),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": e })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

/// Scrape-proxy: read every local agent's metrics over the socket and re-expose
/// them as one Prometheus exposition (RFC 0010). Networkless agents stay
/// observable; Prometheus scrapes this node-agent endpoint.
async fn metrics_handler(
    State(root): State<PathBuf>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    let agents = discover(&root).unwrap_or_default();
    let mut collected: Vec<(String, String)> = Vec::new();
    for agent in agents {
        let socket = agent.socket.clone();
        let scraped = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut client = ManagementClient::connect(&socket).map_err(|e| e.to_string())?;
            client.initialize().map_err(|e| e.to_string())?;
            client
                .read_resource_text("agentd://metrics")
                .map_err(|e| e.to_string())
        })
        .await;
        match scraped {
            Ok(Ok(text)) => collected.push((agent.pod_uid, text)),
            Ok(Err(e)) => eprintln!("node-agent: scrape {} failed: {e}", agent.pod_uid),
            Err(e) => eprintln!("node-agent: scrape task panicked: {e}"),
        }
    }
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        metrics::merge(&collected),
    )
}

async fn discovery_loop(root: PathBuf, interval: Duration) {
    eprintln!(
        "node-agent: discovering management sockets under {}",
        root.display()
    );
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
