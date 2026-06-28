//! agentctl A2A gateway (RFC 0013) — the public A2A HTTP/JSON-RPC surface.
//!
//! External A2A clients speak the spec slash-form over HTTP; the gateway:
//!   1. projects an **Agent Card** at
//!      `GET /agents/{ns}/{name}/.well-known/agent-card.json` from the agent's
//!      capabilities manifest (fetched through the node-agent), and
//!   2. bridges JSON-RPC calls at `POST /agents/{ns}/{name}` — translating the
//!      spec method (`message/send`, …) to the **reference** method
//!      (`a2a.SendMessage`, …) the agent dispatches, then forwarding to the
//!      node-agent on the agent's node. The `message/stream` method takes the
//!      streaming path: the node-agent's `…/a2a/stream` SSE byte-stream is piped
//!      straight back to the client as `text/event-stream` (transparent pipe;
//!      the gateway never parses the SSE frames), and
//!   3. serves a mesh discovery registry at `GET /agents` — the union of `Agent`
//!      and `AgentFleet` CRs across all namespaces, each with its Agent Card URL.
//!
//! Routing ({ns,name}→pod→node-agent) mirrors the apiserver's
//! `forward_to_node_agent` (RFC 0009). Hand-rolled in Rust (axum); agentctl is
//! Rust-only and depends on the contract wire, never on a specific agent (P0).

use std::net::SocketAddr;

use agent_api::{Agent, AgentFleet};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use deadpool_postgres::Pool;
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::{Api, Client};
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;

mod store;

/// Namespace the node-agent DaemonSet runs in (same as the apiserver assumes).
const NODE_AGENT_NS: &str = "agentctl-system";

#[derive(Clone)]
struct AppState {
    client: Client,
    pool: Pool,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let client = Client::try_default().await.expect("in-cluster kube client");

    // The durable task store (RFC 0013). Retry the schema — the DB pod may start
    // after us.
    let pool = build_pool();
    for attempt in 1..=30u32 {
        match store::ensure_schema(&pool).await {
            Ok(()) => break,
            Err(e) if attempt == 30 => panic!("postgres schema after 30 tries: {e}"),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "waiting for postgres…");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/agents", get(list_agents))
        .route(
            "/agents/{ns}/{name}/.well-known/agent-card.json",
            get(agent_card),
        )
        .route("/agents/{ns}/{name}", post(a2a_rpc))
        .with_state(AppState { client, pool });

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
///
/// Non-streaming methods (`message/send`, `tasks/get`, …) forward a single
/// JSON-RPC call and return the node-agent's response verbatim. `message/stream`
/// takes the streaming path: it forwards to the node-agent's `…/a2a/stream` and
/// pipes the resulting SSE byte-stream straight back to the client untouched.
async fn a2a_rpc(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    Json(mut req): Json<Value>,
) -> Response {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let spec = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // `tasks/list` is served by the GATEWAY from the durable store (the agent
    // serves only live tasks); it is not forwarded.
    if spec == "tasks/list" {
        return match store::list(&state.pool, &ns, &name).await {
            Ok(rows) => {
                let tasks: Vec<Value> = rows.iter().map(store::task_json).collect();
                Json(json!({ "jsonrpc": "2.0", "id": id, "result": { "tasks": tasks } }))
                    .into_response()
            }
            Err(e) => Json(rpc_error(id, -32603, &format!("store list: {e}"))).into_response(),
        };
    }

    // Push-notification config (RFC 0013) is gateway-owned: our agents are
    // networkless, so the gateway stores the webhook and delivers. Not forwarded.
    if let Some(op) = spec.strip_prefix("tasks/pushNotificationConfig/") {
        return push_config(&state.pool, &ns, &name, op, &req, id).await;
    }

    // Translate spec → reference; unknown method ⇒ -32601 (METHOD_NOT_FOUND).
    let streaming = spec == "message/stream";
    let reference = match translate_method(&spec) {
        Some(m) => m,
        None => {
            return Json(rpc_error(id, -32601, &format!("method not found: {spec}")))
                .into_response()
        }
    };

    // `tasks/get`: serve from the durable store first (survives the agent),
    // falling back to a live call.
    if spec == "tasks/get" {
        if let Some(tid) = req.pointer("/params/id").and_then(Value::as_str) {
            if let Ok(Some(row)) = store::get(&state.pool, &ns, &name, tid).await {
                return Json(
                    json!({ "jsonrpc": "2.0", "id": id, "result": store::task_json(&row) }),
                )
                .into_response();
            }
        }
    }

    // The input text to persist alongside a message/send result.
    let input = req
        .pointer("/params/message/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Rewrite the request method in place to the reference spelling.
    if let Some(obj) = req.as_object_mut() {
        obj.insert("method".to_string(), json!(reference));
    }

    let (uid, na_ip) = match resolve(&state.client, &ns, &name).await {
        Ok(loc) => loc,
        Err(e) => {
            tracing::warn!(%ns, agent = %name, error = %e, "rpc resolve failed");
            return Json(rpc_error(id, -32603, &e)).into_response();
        }
    };

    if streaming {
        // Streaming path: forward to the node-agent's SSE endpoint and pipe the
        // raw `text/event-stream` body straight through — do NOT parse the SSE
        // frames (transparent byte pipe; the node-agent already framed them).
        let url = format!("http://{na_ip}:8080/v1/agents/{uid}/a2a/stream");
        return match reqwest::Client::new().post(&url).json(&req).send().await {
            Ok(resp) => (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(resp.bytes_stream()),
            )
                .into_response(),
            Err(e) => Json(rpc_error(
                id,
                -32603,
                &format!("node-agent POST {url}: {e}"),
            ))
            .into_response(),
        };
    }

    let url = format!("http://{na_ip}:8080/v1/agents/{uid}/a2a");
    let body = match reqwest::Client::new().post(&url).json(&req).send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(b) => b,
            Err(e) => {
                return Json(rpc_error(id, -32603, &format!("decode node-agent: {e}")))
                    .into_response()
            }
        },
        Err(e) => {
            return Json(rpc_error(
                id,
                -32603,
                &format!("node-agent POST {url}: {e}"),
            ))
            .into_response()
        }
    };

    // Persist task state into the durable store.
    if spec == "message/send" {
        if let Some(result) = body.get("result") {
            let tid = result.get("id").and_then(Value::as_str).unwrap_or("task-1");
            let st = result
                .pointer("/status/state")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            let artifact = result
                .pointer("/artifacts/0/parts/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Err(e) = store::upsert(&state.pool, &ns, &name, tid, st, &input, artifact).await
            {
                tracing::warn!(error = %e, "store upsert failed");
            }
            // Deliver a push notification if a webhook is registered (RFC 0013).
            if let Ok(Some(url)) = store::push_get(&state.pool, &ns, &name, tid).await {
                deliver_push(url, result.clone());
            }
        }
    } else if spec == "tasks/cancel" {
        if let Some(tid) = body.pointer("/result/id").and_then(Value::as_str) {
            let _ = store::set_state(&state.pool, &ns, &name, tid, "canceled").await;
        }
    }

    Json(body).into_response()
}

/// Mesh discovery registry: the union of `Agent` and `AgentFleet` CRs across all
/// namespaces, each carrying its projected Agent Card URL. Contract-shaped — the
/// rows describe CR identity + mode, never any agent's internals (P0).
async fn list_agents(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let base_url = base_url(&headers);
    let mut rows: Vec<Value> = Vec::new();

    let agents: Api<Agent> = Api::all(state.client.clone());
    match agents.list(&ListParams::default()).await {
        Ok(list) => {
            for a in list {
                let ns = a.metadata.namespace.unwrap_or_default();
                let name = a.metadata.name.unwrap_or_default();
                // `spec.mode` is a required enum; project its lowercase wire form.
                let mode = serde_json::to_value(a.spec.mode)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned));
                rows.push(registry_row(
                    "Agent",
                    &ns,
                    &name,
                    mode.as_deref(),
                    &base_url,
                ));
            }
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("list agents: {e}") })),
            )
                .into_response()
        }
    }

    let fleets: Api<AgentFleet> = Api::all(state.client.clone());
    match fleets.list(&ListParams::default()).await {
        Ok(list) => {
            for f in list {
                let ns = f.metadata.namespace.unwrap_or_default();
                let name = f.metadata.name.unwrap_or_default();
                // `AgentFleet` has no top-level `spec.mode` (mode lives on the
                // per-replica template) ⇒ null.
                rows.push(registry_row("AgentFleet", &ns, &name, None, &base_url));
            }
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("list fleets: {e}") })),
            )
                .into_response()
        }
    }

    Json(json!({ "agents": rows })).into_response()
}

/// Serve the A2A `tasks/pushNotificationConfig/*` methods (set/get/list/delete)
/// from the gateway-owned store. The agent is networkless, so the gateway holds
/// the webhook config and performs delivery — these are never forwarded.
async fn push_config(
    pool: &Pool,
    ns: &str,
    name: &str,
    op: &str,
    req: &Value,
    id: Value,
) -> Response {
    let task_id = req
        .pointer("/params/taskId")
        .or_else(|| req.pointer("/params/id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url_param = req
        .pointer("/params/pushNotificationConfig/url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let outcome: Result<Value, String> = match op {
        "set" if task_id.is_empty() || url_param.is_empty() => {
            Err("set requires params.taskId and params.pushNotificationConfig.url".into())
        }
        "set" => store::push_set(pool, ns, name, &task_id, &url_param)
            .await
            .map(|_| json!({ "taskId": task_id, "pushNotificationConfig": { "url": url_param } })),
        "get" => store::push_get(pool, ns, name, &task_id)
            .await
            .map(|u| match u {
                Some(url) => json!({ "taskId": task_id, "pushNotificationConfig": { "url": url } }),
                None => Value::Null,
            }),
        "list" => store::push_list(pool, ns, name).await.map(|rows| {
            Value::Array(
                rows.into_iter()
                    .map(|(t, u)| json!({ "taskId": t, "pushNotificationConfig": { "url": u } }))
                    .collect(),
            )
        }),
        "delete" => store::push_delete(pool, ns, name, &task_id)
            .await
            .map(|_| Value::Null),
        other => Err(format!("unknown pushNotificationConfig op: {other}")),
    };

    match outcome {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err(e) => Json(rpc_error(id, -32602, &e)).into_response(),
    }
}

/// Fire-and-forget delivery of a task to a registered push webhook (RFC 0013).
fn deliver_push(url: String, task: Value) {
    tokio::spawn(async move {
        match reqwest::Client::new().post(&url).json(&task).send().await {
            Ok(r) => tracing::info!(%url, status = r.status().as_u16(), "push delivered"),
            Err(e) => tracing::warn!(%url, error = %e, "push delivery failed"),
        }
    });
}

// --- pure helpers (unit-tested) --------------------------------------------

/// Translate an A2A spec slash-form method to the reference method the agent
/// dispatches over the substrate. `None` ⇒ unsupported (→ JSON-RPC -32601).
fn translate_method(spec: &str) -> Option<&'static str> {
    match spec {
        "message/send" => Some("a2a.SendMessage"),
        "message/stream" => Some("a2a.SendStreamingMessage"),
        "tasks/get" => Some("a2a.GetTask"),
        "tasks/cancel" => Some("a2a.CancelTask"),
        _ => None,
    }
}

/// One mesh-registry row for a discovered CR (`Agent` / `AgentFleet`): identity,
/// the projected Agent Card URL, and the optional run mode (`None` ⇒ JSON null).
fn registry_row(kind: &str, ns: &str, name: &str, mode: Option<&str>, base_url: &str) -> Value {
    json!({
        "kind": kind,
        "namespace": ns,
        "name": name,
        "cardUrl": format!("{base_url}/agents/{ns}/{name}/.well-known/agent-card.json"),
        "mode": mode,
    })
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

/// Build the Postgres connection pool for the durable task store from
/// `DATABASE_URL` (e.g. `postgres://user:pw@host:5432/db?sslmode=disable`).
/// `NoTls` — the in-cluster hop is NetworkPolicy-scoped (TLS to the DB is later
/// hardening), and it keeps the build pure-Rust (no C toolchain).
fn build_pool() -> Pool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let cfg: tokio_postgres::Config = url.parse().expect("parse DATABASE_URL");
    let mgr = deadpool_postgres::Manager::new(cfg, tokio_postgres::NoTls);
    Pool::builder(mgr)
        .max_size(8)
        .build()
        .expect("build postgres pool")
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
        assert_eq!(
            translate_method("message/stream"),
            Some("a2a.SendStreamingMessage")
        );
        assert_eq!(translate_method("tasks/get"), Some("a2a.GetTask"));
        assert_eq!(translate_method("tasks/cancel"), Some("a2a.CancelTask"));
    }

    #[test]
    fn translate_method_rejects_unknown() {
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
    fn registry_row_builds_card_url_and_carries_mode() {
        let row = registry_row(
            "Agent",
            "team-a",
            "echo",
            Some("loop"),
            "https://gw.example",
        );
        assert_eq!(row["kind"], "Agent");
        assert_eq!(row["namespace"], "team-a");
        assert_eq!(row["name"], "echo");
        assert_eq!(
            row["cardUrl"],
            "https://gw.example/agents/team-a/echo/.well-known/agent-card.json"
        );
        assert_eq!(row["mode"], "loop");
    }

    #[test]
    fn registry_row_null_mode_serializes_to_json_null() {
        let row = registry_row("AgentFleet", "ns", "fleet-a", None, "http://h:8080");
        assert_eq!(row["kind"], "AgentFleet");
        assert_eq!(row["namespace"], "ns");
        assert_eq!(row["name"], "fleet-a");
        assert_eq!(
            row["cardUrl"],
            "http://h:8080/agents/ns/fleet-a/.well-known/agent-card.json"
        );
        assert_eq!(row["mode"], Value::Null);
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
