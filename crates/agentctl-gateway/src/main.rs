// SPDX-License-Identifier: BUSL-1.1
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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use agent_api::{Agent, AgentFleet};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use deadpool_postgres::Pool;
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::{Api, Client};
use serde_json::{json, Value};

mod auth;
mod db_tls;
mod metrics;
mod na_client;
mod oidc;
mod signing;
mod store;
mod trusted_proxy;

/// Namespace the node-agent DaemonSet runs in (same as the apiserver assumes).
const NODE_AGENT_NS: &str = "agentctl-system";

#[derive(Clone)]
struct AppState {
    client: Client,
    pool: Pool,
    signer: Arc<signing::Signer>,
    /// mTLS client for the node-agent control hop (RFC 0015). Built once.
    na: reqwest::Client,
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
    /// Per-agent OIDC/JWT verifier (RFC: native A2A authn/authz). Holds the
    /// per-issuer JWKS cache; built once.
    oidc: Arc<oidc::Verifier>,
    /// The coarse bearer-token gate, threaded in so the A2A RPC handler can apply
    /// it inline for agents WITHOUT per-agent OIDC (the gate middleware defers the
    /// POST RPC route — see [`auth::gate`]).
    auth: auth::Auth,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set; otherwise byte-identical to before.
    agentctl_telemetry::init("agentctl-gateway");
    // ring crypto provider as the process default (RFC 0015): no aws-lc-rs → no
    // C toolchain. Required so reqwest's rustls backend (federation/push) and the
    // node-agent mTLS client both resolve a provider.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let client = Client::try_default().await.expect("in-cluster kube client");

    // The Agent Card signing key (RFC 0013) — required at startup.
    let signer = Arc::new(signing::Signer::from_env().expect("GATEWAY_SIGNING_SEED"));

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

    // Shared metrics surface (also feeds the access gate's rejection counter).
    let metrics = Arc::new(metrics::Metrics::new());
    // Cloned for the trusted-proxy mTLS middleware (the original moves into state).
    let mw_metrics = metrics.clone();
    // Optional bearer-token access gate (AGENTCTL_API_TOKEN). Unset → no-op; set
    // → enforced on the A2A surface, with /healthz /readyz /metrics AND the public
    // JWKS (/.well-known/jwks.json) exempt. The middleware short-circuits the
    // exempt paths, so it can wrap the whole router.
    let gate = auth::Auth::from_env(metrics.clone());

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // `/metrics` rides the EXISTING plaintext :8080 (the chart's `http` port),
        // alongside /healthz — no new port; scraped scheme=http.
        .route("/metrics", get(serve_metrics))
        .route("/.well-known/jwks.json", get(jwks))
        .route("/agents", get(list_agents))
        .route(
            "/agents/{ns}/{name}/.well-known/agent-card.json",
            get(agent_card),
        )
        .route(
            "/fleets/{ns}/{name}/.well-known/agent-card.json",
            get(fleet_card),
        )
        .route("/agents/{ns}/{name}", post(a2a_rpc))
        .layer(axum::middleware::from_fn_with_state(
            gate.clone(),
            auth::gate,
        ))
        .with_state(AppState {
            client,
            pool,
            signer,
            na: na_client::node_agent_client(),
            metrics,
            // Per-agent OIDC verifier (public-CA JWKS HTTP client, ring-backed).
            oidc: Arc::new(oidc::Verifier::new()),
            // Same coarse gate the middleware uses; the RPC handler falls back to
            // it for non-OIDC agents.
            auth: gate,
        });

    // TRUSTED-PROXY mode (front-proxy trust over mTLS). OFF by default — when off
    // this whole block is skipped and the plaintext listener path below is
    // byte-identical to before.
    let tp = Arc::new(trusted_proxy::Config::from_env());

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));

    // Graceful shutdown: on SIGTERM/SIGINT, stop accepting and drain in-flight
    // requests (hyper's `with_graceful_shutdown`). In-flight SSE streams
    // (`message/stream`) are short-lived — our agents complete synchronously, so
    // the node-agent emits its terminal frame and the passthrough body ends,
    // letting the connection close cleanly within the drain.
    if !tp.enabled {
        tracing::info!(%addr, "agentctl gateway serving the A2A HTTP surface");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .expect("serve");
        return;
    }

    // Enabled: serve a SECOND mTLS listener (front-proxy trust) concurrently with
    // the existing plaintext one — mirroring the node-agent's dual listener.
    let tls_addr: SocketAddr = tp
        .tls_addr
        .parse()
        .unwrap_or_else(|e| panic!("parse AGENTCTL_GATEWAY_TLS_ADDR {}: {e}", tp.tls_addr));
    let server_config = trusted_proxy::build_tls_config(&tp.tls_dir, &tp.ca_path)
        .expect("build trusted-proxy mTLS server config");
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
    let acceptor = trusted_proxy::PeerCertAcceptor::new(rustls_config);

    // The mTLS router enforces the allow-list + extracts the asserted identity
    // (a verified TRUSTED caller); the plaintext router STRIPS the asserted
    // identity headers (anti-spoof). Both share the same routes + access gate.
    let mtls_ctx = trusted_proxy::MtlsCtx {
        cfg: tp.clone(),
        metrics: mw_metrics,
    };
    let mtls_app = app
        .clone()
        .layer(axum::middleware::from_fn_with_state(
            mtls_ctx,
            trusted_proxy::mtls_decision,
        ))
        .into_make_service();
    let plaintext_app = app.layer(axum::middleware::from_fn_with_state(
        tp.clone(),
        trusted_proxy::strip_plaintext,
    ));

    tracing::info!(
        %addr, %tls_addr, ca = %tp.ca_path.display(), allowed = ?tp.allowed_names,
        "trusted-proxy ENABLED: plaintext :8080 (identity headers stripped) + mTLS front-proxy listener"
    );
    // The mTLS listener runs as a background task; the plaintext listener keeps the
    // existing graceful-shutdown behaviour in the foreground. On SIGTERM the
    // foreground drains and returns, and the process exits (dropping the task).
    tokio::spawn(async move {
        axum_server::bind(tls_addr)
            .acceptor(acceptor)
            .serve(mtls_app)
            .await
            .expect("serve trusted-proxy mTLS");
    });
    axum::serve(listener, plaintext_app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve");
}

// --- graceful shutdown -----------------------------------------------------

/// Wait for SIGTERM/SIGINT, then resolve so hyper drains in-flight requests
/// (including any in-flight SSE passthroughs).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down: draining in-flight requests and SSE streams");
}

/// `GET /metrics` — the Prometheus exposition (node-agent text format).
async fn serve_metrics(
    State(state): State<AppState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

// --- handlers --------------------------------------------------------------

/// Publish the JWKS that verifies signed Agent Cards (RFC 0013).
async fn jwks(State(state): State<AppState>) -> Json<Value> {
    Json(state.signer.jwks())
}

/// Project a **signed** A2A Agent Card from the agent's capabilities manifest,
/// fetched from the node-agent on the agent's node. `kind` (when `Some`) is
/// attached as `x-agentctl-kind` — used to mark fleet cards. This is the shared
/// path behind both [`agent_card`] and [`fleet_card`] (a fleet's pods are
/// labelled the same way an agent's are, so [`resolve`] works for both).
async fn build_signed_card(
    state: &AppState,
    ns: &str,
    name: &str,
    base_url: &str,
    kind: Option<&str>,
) -> Result<Value, String> {
    let (uid, na_ip) = resolve(&state.client, ns, name).await?;
    // mTLS control hop (RFC 0015): https on :8443, client-cert required.
    let url = format!("https://{na_ip}:8443/v1/agents/{uid}/capabilities");
    let manifest = state
        .na
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("node-agent GET {url}: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("decode capabilities: {e}"))?;
    let mut card = project_card(&manifest, ns, name, base_url);
    if let Some(k) = kind {
        card["x-agentctl-kind"] = json!(k);
    }
    state.signer.sign_card(&mut card);
    Ok(card)
}

/// Project the signed A2A Agent Card for an `Agent` CR.
async fn agent_card(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    state.metrics.inc_card();
    let base_url = base_url(&headers);
    match build_signed_card(&state, &ns, &name, &base_url, None).await {
        Ok(card) => (StatusCode::OK, Json(card)),
        Err(e) => {
            state.metrics.inc_upstream_error();
            tracing::warn!(%ns, agent = %name, error = %e, "card build failed");
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e })))
        }
    }
}

/// Project the signed A2A Agent Card for an `AgentFleet` CR (marked
/// `x-agentctl-kind: AgentFleet`).
async fn fleet_card(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    state.metrics.inc_card();
    let base_url = base_url(&headers);
    match build_signed_card(&state, &ns, &name, &base_url, Some("AgentFleet")).await {
        Ok(card) => (StatusCode::OK, Json(card)),
        Err(e) => {
            state.metrics.inc_upstream_error();
            tracing::warn!(%ns, fleet = %name, error = %e, "fleet card build failed");
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e })))
        }
    }
}

/// Bridge a spec-form A2A JSON-RPC request to the agent's reference method.
///
/// Non-streaming methods (`message/send`, `tasks/get`, …) forward a single
/// JSON-RPC call and return the node-agent's response verbatim. `message/stream`
/// takes the streaming path: it forwards to the node-agent's `…/a2a/stream` and
/// pipes the resulting SSE byte-stream straight back to the client untouched.
#[tracing::instrument(skip_all, fields(ns = %ns, agent = %name))]
async fn a2a_rpc(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    trusted_proxy::TrustedDecision(decision): trusted_proxy::TrustedDecision,
    headers: HeaderMap,
    Json(mut req): Json<Value>,
) -> Response {
    state.metrics.inc_rpc();
    let id = req.get("id").cloned().unwrap_or(Value::Null);

    // Per-agent access enforcement, BEFORE any method handling. Precedence:
    //   (1) a verified trusted-proxy identity (mTLS listener) — trusted, enforce
    //       any requiredClaims, forward identity;
    //   (2) per-agent OIDC (spec.access.oidc) — validate the JWT;
    //   (3) the coarse bearer gate.
    // On success with identity forwarding, the verified caller identity is sent to
    // the agent as X-Auth-* headers.
    let (identity, forward_identity) =
        match enforce_access(&state, &ns, &name, &headers, &decision).await {
            Ok(v) => v,
            Err(resp) => return resp,
        };
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

    // `tasks/resubscribe` is served by the GATEWAY: a one-shot SSE resume of the
    // stored task. NOTE: live resume of an in-flight stream is future work — our
    // agents complete synchronously, so the stored task is already terminal and a
    // single replayed frame is the whole stream.
    if spec == "tasks/resubscribe" {
        let tid = req
            .pointer("/params/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return match store::get(&state.pool, &ns, &name, &tid).await {
            Ok(Some(row)) => (
                [(header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {}\n\n", store::task_json(&row)),
            )
                .into_response(),
            Ok(None) => {
                Json(rpc_error(id, -32001, &format!("task not found: {tid}"))).into_response()
            }
            Err(e) => Json(rpc_error(id, -32603, &format!("store get: {e}"))).into_response(),
        };
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
            state.metrics.inc_upstream_error();
            tracing::warn!(%ns, agent = %name, error = %e, "rpc resolve failed");
            return Json(rpc_error(id, -32603, &e)).into_response();
        }
    };

    if streaming {
        // Streaming path: forward to the node-agent's SSE endpoint and pipe the
        // raw `text/event-stream` body straight through — do NOT parse the SSE
        // frames (transparent byte pipe; the node-agent already framed them).
        state.metrics.inc_stream();
        let url = format!("https://{na_ip}:8443/v1/agents/{uid}/a2a/stream");
        let forwarded = forward_request(&state, &url, &req, &identity, forward_identity);
        return match forwarded.send().await {
            Ok(resp) => (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(resp.bytes_stream()),
            )
                .into_response(),
            Err(e) => {
                state.metrics.inc_upstream_error();
                Json(rpc_error(
                    id,
                    -32603,
                    &format!("node-agent POST {url}: {e}"),
                ))
                .into_response()
            }
        };
    }

    let url = format!("https://{na_ip}:8443/v1/agents/{uid}/a2a");
    let forwarded = forward_request(&state, &url, &req, &identity, forward_identity);
    let body = match forwarded.send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(b) => b,
            Err(e) => {
                state.metrics.inc_upstream_error();
                return Json(rpc_error(id, -32603, &format!("decode node-agent: {e}")))
                    .into_response();
            }
        },
        Err(e) => {
            state.metrics.inc_upstream_error();
            return Json(rpc_error(
                id,
                -32603,
                &format!("node-agent POST {url}: {e}"),
            ))
            .into_response();
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
            } else {
                state.metrics.inc_task();
            }
            // Deliver a push notification if a webhook is registered (RFC 0013).
            if let Ok(Some((url, token))) = store::push_get(&state.pool, &ns, &name, tid).await {
                deliver_push(url, token, result.clone());
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
async fn list_agents(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
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
                let mut row = registry_row("Agent", &ns, &name, mode.as_deref(), &base_url);
                row["origin"] = json!("local");
                rows.push(row);
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
                let mut row = registry_row("AgentFleet", &ns, &name, None, &base_url);
                row["origin"] = json!("local");
                rows.push(row);
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

    // `?local=…` ⇒ return ONLY local rows. This is the endpoint peers call when
    // federating, so it must NOT fan out again (no infinite recursion).
    if params.contains_key("local") {
        return Json(json!({ "agents": rows })).into_response();
    }

    // Federation: merge each peer gateway's local rows, tagging the peer origin.
    // A peer fetch error is logged and skipped — never fail the whole registry.
    let peers = federation_peers(&std::env::var("FEDERATION_PEERS").unwrap_or_default());
    for peer in peers {
        let url = format!("{peer}/agents?local=1");
        match reqwest::Client::new().get(&url).send().await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(body) => {
                    if let Some(arr) = body.get("agents").and_then(Value::as_array) {
                        for r in arr {
                            let mut r = r.clone();
                            r["origin"] = json!(peer);
                            rows.push(r);
                        }
                    }
                }
                Err(e) => tracing::warn!(%peer, error = %e, "decode peer agents; skipping"),
            },
            Err(e) => tracing::warn!(%peer, error = %e, "fetch peer agents; skipping"),
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
    let token_param = req
        .pointer("/params/pushNotificationConfig/token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let outcome: Result<Value, String> = match op {
        "set" if task_id.is_empty() || url_param.is_empty() => {
            Err("set requires params.taskId and params.pushNotificationConfig.url".into())
        }
        "set" => store::push_set(pool, ns, name, &task_id, &url_param, &token_param)
            .await
            .map(|_| {
                json!({ "taskId": task_id, "pushNotificationConfig": push_cfg(&url_param, &token_param) })
            }),
        "get" => store::push_get(pool, ns, name, &task_id)
            .await
            .map(|u| match u {
                Some((url, token)) => {
                    json!({ "taskId": task_id, "pushNotificationConfig": push_cfg(&url, &token) })
                }
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
/// Retries up to 3 attempts (200ms backoff) until a 2xx; a non-empty `token` is
/// sent as `Authorization: Bearer <token>`.
fn deliver_push(url: String, token: String, task: Value) {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut last = String::from("no attempt");
        for attempt in 1..=3u32 {
            let mut rb = client.post(&url).json(&task);
            if !token.is_empty() {
                rb = rb.bearer_auth(&token);
            }
            match rb.send().await {
                Ok(r) if r.status().is_success() => {
                    let status = r.status().as_u16();
                    tracing::info!(%url, status, attempt, "push delivered");
                    return;
                }
                Ok(r) => last = format!("status {}", r.status().as_u16()),
                Err(e) => last = e.to_string(),
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        tracing::warn!(%url, error = %last, "push delivery failed after 3 attempts");
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

/// Build the `pushNotificationConfig` object echoed back to clients: always the
/// `url`, plus `token` only when one is set (don't leak an empty token field).
fn push_cfg(url: &str, token: &str) -> Value {
    let mut cfg = json!({ "url": url });
    if !token.is_empty() {
        cfg["token"] = json!(token);
    }
    cfg
}

/// Parse the comma-separated `FEDERATION_PEERS` env value into clean gateway
/// base URLs (trimmed; empties dropped). Pure — unit-tested.
fn federation_peers(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
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
/// read from the neutral `agent_version` key.
fn project_card(manifest: &Value, ns: &str, name: &str, base_url: &str) -> Value {
    let version = manifest
        .get("agent_version")
        .and_then(Value::as_str)
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
///
/// `sslmode=disable` (the default path) → [`tokio_postgres::NoTls`]: a plain
/// in-cluster hop, kept NetworkPolicy-scoped. `sslmode=require`/`prefer` (e.g.
/// bundled `postgres.tls.enabled` or an external managed DSN) → a rustls/ring
/// connector ([`db_tls::make_connector`]) that encrypts the hop without verifying
/// the cert. `sslmode=verify-full` (or `DB_TLS_VERIFY=full`) with a mounted CA
/// bundle → a CA-pinning connector ([`db_tls::make_verifying_connector`]) that
/// verifies the chain and server name. All paths stay pure-Rust (no C toolchain).
fn build_pool() -> Pool {
    let raw = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let (url, verify_full) = db_tls::resolve_tls(&raw);
    let cfg: tokio_postgres::Config = url.parse().expect("parse DATABASE_URL");
    let mgr = if cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
        deadpool_postgres::Manager::new(cfg, tokio_postgres::NoTls)
    } else if verify_full {
        let ca = db_tls::ca_file_path();
        match db_tls::make_verifying_connector(&ca) {
            Ok(connector) => {
                tracing::info!(ca = %ca.display(), "postgres TLS: verify-full (CA pinning)");
                deadpool_postgres::Manager::new(cfg, connector)
            }
            Err(err) => {
                tracing::warn!(
                    ca = %ca.display(),
                    error = %err,
                    "postgres TLS: verify-full requested but CA load failed; \
                     falling back to encrypt-without-verify"
                );
                deadpool_postgres::Manager::new(cfg, db_tls::make_connector())
            }
        }
    } else {
        deadpool_postgres::Manager::new(cfg, db_tls::make_connector())
    };
    Pool::builder(mgr)
        .max_size(8)
        .build()
        .expect("build postgres pool")
}

// --- per-agent access enforcement (OIDC) -----------------------------------

/// Enforce the per-agent access policy for an inbound A2A RPC, BEFORE method
/// handling. Returns `(identity, forward_identity)` on success — `identity` is
/// `Some` for a trusted-proxy or OIDC caller (so the caller can forward it). On any
/// failure it returns the terminal [`Response`] to send (401 authN / 403 authZ /
/// 502 lookup).
///
/// Precedence:
///   1. a verified trusted-proxy identity (`decision == Trusted`, mTLS listener):
///      authN is satisfied; if the agent declares `spec.access.oidc.requiredClaims`
///      they are enforced against the asserted identity (403 on miss); the identity
///      is forwarded to the agent.
///   2. `spec.access.oidc` set: a bearer JWT is required + validated for THIS agent.
///   3. otherwise the coarse bearer gate the middleware enforces is applied inline.
async fn enforce_access(
    state: &AppState,
    ns: &str,
    name: &str,
    headers: &HeaderMap,
    decision: &trusted_proxy::Decision,
) -> Result<(Option<oidc::Identity>, bool), Response> {
    let access = match read_access(&state.client, ns, name).await {
        Ok(a) => a,
        Err(e) => {
            // A hard error reading the CR (not a clean NotFound) → fail closed.
            state.metrics.inc_upstream_error();
            tracing::warn!(%ns, agent = %name, error = %e, "read access policy failed");
            return Err((StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response());
        }
    };

    // (1) Verified trusted-proxy identity (mTLS listener). The front proxy already
    // performed edge authN; we only apply authZ (requiredClaims) and forward the
    // asserted identity to the agent.
    if let trusted_proxy::Decision::Trusted(identity) = decision {
        if let Some(rules) = access
            .as_ref()
            .and_then(|a| a.oidc.as_ref())
            .and_then(|o| o.required_claims.as_deref())
        {
            let claims = trusted_proxy::identity_claims(identity);
            if oidc::enforce_claims(&claims, Some(rules)).is_err() {
                state.metrics.inc_trusted_proxy_rejected();
                tracing::warn!(%ns, agent = %name, sub = %identity.sub, "trusted-proxy authZ denied: requiredClaims unsatisfied");
                return Err(StatusCode::FORBIDDEN.into_response());
            }
        }
        state.metrics.inc_trusted_proxy_accepted();
        return Ok((Some(identity.clone()), true));
    }

    let Some(oidc_cfg) = access.as_ref().and_then(|a| a.oidc.as_ref()) else {
        // No per-agent OIDC → fall back to the coarse bearer gate.
        if state.auth.authorize(headers) {
            return Ok((None, false));
        }
        state.metrics.inc_auth_rejected();
        return Err(StatusCode::UNAUTHORIZED.into_response());
    };

    // OIDC agent: require + validate a bearer JWT scoped to THIS agent.
    let Some(token) = bearer_token(headers) else {
        state.metrics.inc_oidc_deny();
        return Err(StatusCode::UNAUTHORIZED.into_response());
    };
    match state.oidc.verify(oidc_cfg, token).await {
        Ok(identity) => {
            state.metrics.inc_oidc_allow();
            Ok((Some(identity), oidc_cfg.forward_identity.unwrap_or(false)))
        }
        // No token detail leaks to the client (body is the bare status); the
        // reason is logged server-side only.
        Err(oidc::AuthError::Unauthorized(reason)) => {
            state.metrics.inc_oidc_deny();
            tracing::warn!(%ns, agent = %name, reason = %reason, "oidc authN denied");
            Err(StatusCode::UNAUTHORIZED.into_response())
        }
        Err(oidc::AuthError::Forbidden(reason)) => {
            state.metrics.inc_oidc_deny();
            tracing::warn!(%ns, agent = %name, reason = %reason, "oidc authZ denied");
            Err(StatusCode::FORBIDDEN.into_response())
        }
    }
}

/// Read `spec.access` for an `Agent`, falling back to an `AgentFleet`'s
/// `spec.template.access`. A clean 404 on both kinds ⇒ `Ok(None)` (no policy; the
/// later [`resolve`] surfaces "no running pod"); a transport/permission error ⇒
/// `Err` so the caller fails closed.
async fn read_access(
    client: &Client,
    ns: &str,
    name: &str,
) -> Result<Option<agent_api::Access>, String> {
    let agents: Api<Agent> = Api::namespaced(client.clone(), ns);
    match agents.get_opt(name).await {
        Ok(Some(a)) => return Ok(a.spec.access),
        Ok(None) => {}
        Err(e) => return Err(format!("get Agent {ns}/{name}: {e}")),
    }
    let fleets: Api<AgentFleet> = Api::namespaced(client.clone(), ns);
    match fleets.get_opt(name).await {
        Ok(Some(f)) => Ok(f.spec.template.access),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("get AgentFleet {ns}/{name}: {e}")),
    }
}

/// Extract `<JWT>` from an `Authorization: Bearer <JWT>` header (non-empty).
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
}

/// Build the forwarded node-agent request, injecting the verified caller identity
/// as `X-Auth-*` headers when `forward_identity` is enabled for an OIDC agent.
fn forward_request(
    state: &AppState,
    url: &str,
    req: &Value,
    identity: &Option<oidc::Identity>,
    forward_identity: bool,
) -> reqwest::RequestBuilder {
    let rb = state.na.post(url).json(req);
    match (forward_identity, identity) {
        (true, Some(id)) => id.inject(rb),
        _ => rb,
    }
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
    fn push_cfg_includes_token_only_when_set() {
        let with = push_cfg("https://hook", "s3cr3t");
        assert_eq!(with["url"], "https://hook");
        assert_eq!(with["token"], "s3cr3t");

        let without = push_cfg("https://hook", "");
        assert_eq!(without["url"], "https://hook");
        assert_eq!(without.get("token"), None);
    }

    #[test]
    fn federation_peers_splits_trims_and_drops_empties() {
        assert_eq!(federation_peers(""), Vec::<String>::new());
        assert_eq!(federation_peers("   "), Vec::<String>::new());
        assert_eq!(federation_peers(",,"), Vec::<String>::new());
        assert_eq!(
            federation_peers("http://a , http://b ,, http://c "),
            vec![
                "http://a".to_string(),
                "http://b".to_string(),
                "http://c".to_string()
            ]
        );
        assert_eq!(
            federation_peers("http://only"),
            vec!["http://only".to_string()]
        );
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
