//! agentctl A2A gateway (RFC 0013) — the public A2A HTTP/JSON-RPC surface.
//!
//! External A2A clients speak the spec slash-form over HTTP; the gateway:
//!   1. projects an **Agent Card** at
//!      `GET /agents/{ns}/{name}/.well-known/agent-card.json` from the agent's
//!      capabilities manifest (fetched through the node-agent), and
//!   2. bridges JSON-RPC calls at `POST /agents/{ns}/{name}` — translating the
//!      spec method (`message/send`, …) to the **reference** method
//!      (`a2a.SendMessage`, …) the agent dispatches, then forwarding to the
//!      node-agent on the agent's node.
//!
//! Routing ({ns,name}→pod→node-agent) mirrors the apiserver's
//! `forward_to_node_agent` (RFC 0009). Hand-rolled in Rust (axum); agentctl is
//! Rust-only and depends on the contract wire, never on a specific agent (P0).

use std::net::SocketAddr;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::{Api, Client};
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;

/// Namespace the node-agent DaemonSet runs in (same as the apiserver assumes).
const NODE_AGENT_NS: &str = "agentctl-system";

#[derive(Clone)]
struct AppState {
    client: Client,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let client = Client::try_default().await.expect("in-cluster kube client");

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/agents/{ns}/{name}/.well-known/agent-card.json",
            get(agent_card),
        )
        .route("/agents/{ns}/{name}", post(a2a_rpc))
        .with_state(AppState { client });

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    tracing::info!(%addr, "agentctl gateway serving the A2A HTTP surface");
    axum::serve(listener, app).await.expect("serve");
}

// --- handlers --------------------------------------------------------------

/// Project the A2A Agent Card from the agent's capabilities manifest, fetched
/// from the node-agent on the agent's node.
async fn agent_card(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    let base_url = base_url(&headers);
    let (uid, na_ip) = match resolve(&state.client, &ns, &name).await {
        Ok(loc) => loc,
        Err(e) => {
            tracing::warn!(%ns, agent = %name, error = %e, "card resolve failed");
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e })));
        }
    };

    let url = format!("http://{na_ip}:8080/v1/agents/{uid}/capabilities");
    let manifest = match reqwest::Client::new().get(&url).send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(m) => m,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("decode capabilities: {e}") })),
                )
            }
        },
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("node-agent GET {url}: {e}") })),
            )
        }
    };

    (
        StatusCode::OK,
        Json(project_card(&manifest, &ns, &name, &base_url)),
    )
}

/// Bridge a spec-form A2A JSON-RPC request to the agent's reference method.
async fn a2a_rpc(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    Json(mut req): Json<Value>,
) -> Json<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);

    // Translate spec → reference; unknown method ⇒ -32601 (METHOD_NOT_FOUND).
    let spec = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let reference = match translate_method(spec) {
        Some(m) => m,
        None => return Json(rpc_error(id, -32601, &format!("method not found: {spec}"))),
    };

    // Rewrite the request method in place to the reference spelling.
    if let Some(obj) = req.as_object_mut() {
        obj.insert("method".to_string(), json!(reference));
    }

    let (uid, na_ip) = match resolve(&state.client, &ns, &name).await {
        Ok(loc) => loc,
        Err(e) => {
            tracing::warn!(%ns, agent = %name, error = %e, "rpc resolve failed");
            return Json(rpc_error(id, -32603, &e));
        }
    };

    let url = format!("http://{na_ip}:8080/v1/agents/{uid}/a2a");
    match reqwest::Client::new().post(&url).json(&req).send().await {
        Ok(resp) => match resp.json::<Value>().await {
            // Return the node-agent's JSON-RPC response verbatim.
            Ok(body) => Json(body),
            Err(e) => Json(rpc_error(id, -32603, &format!("decode node-agent: {e}"))),
        },
        Err(e) => Json(rpc_error(
            id,
            -32603,
            &format!("node-agent POST {url}: {e}"),
        )),
    }
}

// --- pure helpers (unit-tested) --------------------------------------------

/// Translate an A2A spec slash-form method to the reference method the agent
/// dispatches over the substrate. `None` ⇒ unsupported (→ JSON-RPC -32601).
fn translate_method(spec: &str) -> Option<&'static str> {
    match spec {
        "message/send" => Some("a2a.SendMessage"),
        "tasks/get" => Some("a2a.GetTask"),
        "tasks/cancel" => Some("a2a.CancelTask"),
        _ => None,
    }
}

/// Project a minimal A2A Agent Card from a capabilities manifest. The version is
/// read from the neutral `agent_version`, falling back to the reference alias
/// `agentd_version` (de-branding, P0).
fn project_card(manifest: &Value, ns: &str, name: &str, base_url: &str) -> Value {
    let version = manifest
        .get("agent_version")
        .and_then(Value::as_str)
        .or_else(|| manifest.get("agentd_version").and_then(Value::as_str))
        .unwrap_or("unknown");
    json!({
        "protocolVersion": "1.0",
        "name": format!("{ns}/{name}"),
        "url": format!("{base_url}/agents/{ns}/{name}"),
        "version": version,
        "capabilities": { "streaming": false },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": []
    })
}

/// A JSON-RPC 2.0 error envelope, preserving the request id.
fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The externally reachable base URL, from the request `Host` header.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080");
    format!("http://{host}")
}

// --- routing (kube; needs a cluster to run, not to compile/test) -----------

/// Resolve `{ns,name}` → `(pod_uid, node_agent_ip)`, exactly as the apiserver's
/// `forward_to_node_agent`: the agent's Running pod (labelled
/// `agentctl.dev/agent=<name>`) gives the uid + node; the Running node-agent on
/// that node gives the IP to reach.
async fn resolve(client: &Client, ns: &str, name: &str) -> Result<(String, String), String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("agentctl.dev/agent={name}"));
    let pod = pods
        .list(&lp)
        .await
        .map_err(|e| format!("list agent pods: {e}"))?
        .items
        .into_iter()
        .find(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .ok_or_else(|| format!("no running pod for agent {ns}/{name}"))?;
    let pod_uid = pod.metadata.uid.ok_or("agent pod has no uid")?;
    let node = pod
        .spec
        .and_then(|s| s.node_name)
        .ok_or("agent pod has no nodeName")?;

    let na: Api<Pod> = Api::namespaced(client.clone(), NODE_AGENT_NS);
    let na_lp = ListParams::default()
        .labels("app.kubernetes.io/name=agentctl-node-agent")
        .fields(&format!("spec.nodeName={node}"));
    let na_ip = na
        .list(&na_lp)
        .await
        .map_err(|e| format!("list node-agents: {e}"))?
        .items
        .into_iter()
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .find_map(|p| p.status.and_then(|s| s.pod_ip))
        .ok_or_else(|| format!("no running node-agent on node {node}"))?;

    Ok((pod_uid, na_ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_method_maps_the_mvp_set() {
        assert_eq!(translate_method("message/send"), Some("a2a.SendMessage"));
        assert_eq!(translate_method("tasks/get"), Some("a2a.GetTask"));
        assert_eq!(translate_method("tasks/cancel"), Some("a2a.CancelTask"));
    }

    #[test]
    fn translate_method_rejects_unknown() {
        assert_eq!(translate_method("message/stream"), None);
        assert_eq!(translate_method("tasks/list"), None);
        assert_eq!(translate_method(""), None);
        assert_eq!(translate_method("a2a.SendMessage"), None);
    }

    #[test]
    fn project_card_reads_neutral_version_and_builds_url() {
        let manifest = json!({ "agent_version": "1.2.3", "contract_version": "1.0" });
        let card = project_card(&manifest, "team-a", "echo", "https://gw.example");

        assert_eq!(card["protocolVersion"], "1.0");
        assert_eq!(card["name"], "team-a/echo");
        assert_eq!(card["url"], "https://gw.example/agents/team-a/echo");
        assert_eq!(card["version"], "1.2.3");
        assert_eq!(card["capabilities"]["streaming"], false);
        assert_eq!(card["defaultInputModes"], json!(["text/plain"]));
        assert_eq!(card["defaultOutputModes"], json!(["text/plain"]));
        assert_eq!(card["skills"], json!([]));
    }

    #[test]
    fn project_card_falls_back_to_branded_version_alias() {
        let manifest = json!({ "agentd_version": "mock-agent-0.1.0" });
        let card = project_card(&manifest, "ns", "a", "http://h:8080");
        assert_eq!(card["version"], "mock-agent-0.1.0");
    }

    #[test]
    fn project_card_prefers_neutral_over_alias() {
        let manifest = json!({ "agent_version": "9.9", "agentd_version": "old" });
        let card = project_card(&manifest, "ns", "a", "http://h");
        assert_eq!(card["version"], "9.9");
    }

    #[test]
    fn project_card_defaults_version_when_absent() {
        let card = project_card(&json!({}), "ns", "a", "http://h");
        assert_eq!(card["version"], "unknown");
    }

    #[test]
    fn rpc_error_preserves_id_and_shape() {
        let e = rpc_error(json!(7), -32601, "method not found: foo/bar");
        assert_eq!(e["jsonrpc"], "2.0");
        assert_eq!(e["id"], 7);
        assert_eq!(e["error"]["code"], -32601);
        assert_eq!(e["error"]["message"], "method not found: foo/bar");
    }
}
