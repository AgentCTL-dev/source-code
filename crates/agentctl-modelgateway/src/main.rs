// SPDX-License-Identifier: BUSL-1.1
//! agentctl ModelGateway (RFC 0012) — the intelligence plane's inference proxy.
//!
//! Conformant agents are **networkless and hold NO provider secrets** (P0). They
//! cannot reach a model provider on their own; instead their intelligence request
//! reaches this gateway carrying only their *identity* in headers (in production
//! the on-node bridge asserts these after attestation, RFC 0015; for now they are
//! passed in). The gateway:
//!   1. selects the agent's `ModelPool` (CRD, `agents.x-k8s.io/v1alpha1`),
//!   2. enforces the pool's token **budget** pre-request,
//!   3. **injects** the pool's provider credential (read from the referenced
//!      `Secret`) — the agent's own credential, if any, is NEVER used,
//!   4. forwards the request to the provider endpoint, and
//!   5. **meters** the tokens consumed into a durable Postgres store.
//!
//! Hand-rolled in Rust (axum); agentctl is Rust-only and depends on the
//! contract/wire, never on a specific agent or provider SDK (P0).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use agent_api::{ModelPool, ModelPoolSpec};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use deadpool_postgres::Pool;
use k8s_openapi::api::core::v1::Secret;
use kube::api::ListParams;
use kube::{Api, Client};
use serde_json::{json, Value};

mod db_tls;
mod metrics;
mod store;

/// Identity header: the requesting agent's namespace (required).
const H_NAMESPACE: &str = "X-Agent-Namespace";
/// Identity header: the requesting agent's name (optional; defaults to `unknown`).
const H_AGENT: &str = "X-Agent-Name";
/// Routing header: which `ModelPool` to use (optional; defaults to the first in ns).
const H_POOL: &str = "X-Model-Pool";

#[derive(Clone)]
struct AppState {
    client: Client,
    pool: Pool,
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set; otherwise byte-identical to before.
    agentctl_telemetry::init("agentctl-modelgateway");

    let client = Client::try_default().await.expect("in-cluster kube client");

    // The durable usage meter. Retry the schema — the DB pod may start after us.
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
        // `/metrics` rides the EXISTING plaintext :8080 (the chart's `http` port),
        // alongside /healthz — no new port; scraped scheme=http.
        .route("/metrics", get(serve_metrics))
        .route("/v1/infer", post(infer))
        .route("/v1/usage", get(usage))
        .with_state(AppState {
            client,
            pool,
            metrics: Arc::new(metrics::Metrics::new()),
        });

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    tracing::info!(%addr, "agentctl modelgateway serving the intelligence plane");
    // Graceful shutdown: on SIGTERM/SIGINT, stop accepting and drain in-flight
    // requests (hyper's `with_graceful_shutdown`).
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve");
}

// --- graceful shutdown -----------------------------------------------------

/// Wait for SIGTERM/SIGINT, then resolve so hyper drains in-flight requests.
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
    tracing::info!("shutting down: draining in-flight requests");
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

/// `POST /v1/infer` — the inference wire. The agent supplies only its identity in
/// headers and a provider-neutral body; the gateway selects its pool, enforces
/// the budget, injects the pool's credential, forwards to the provider, meters
/// the result, and returns the provider response (tagged with the pool for
/// traceability).
#[tracing::instrument(name = "modelgateway.infer", skip_all)]
async fn infer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    state.metrics.inc_request();
    // a. identity headers.
    let ns = match header_str(&headers, H_NAMESPACE) {
        Some(ns) => ns,
        None => return bad_request(&format!("{H_NAMESPACE} header required")),
    };
    let agent = header_str(&headers, H_AGENT).unwrap_or_else(|| "unknown".to_string());
    let want_pool = header_str(&headers, H_POOL);

    // b. select the ModelPool.
    let pools: Api<ModelPool> = Api::namespaced(state.client.clone(), &ns);
    let (pool_name, spec) = match select_pool(&pools, want_pool.as_deref()).await {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(&no_pool_msg(&ns, want_pool.as_deref()), &ns),
        Err(e) => return internal(&format!("select ModelPool: {e}")),
    };
    let budget = spec.budget.as_ref().map(|b| b.max_tokens);

    // c. budget — pre-request check.
    if budget.is_some() {
        match store::pool_tokens(&state.pool, &ns, &pool_name).await {
            Ok(used) if over_budget(used, budget) => {
                state.metrics.inc_budget_rejection();
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "error": "budget exceeded",
                        "namespace": ns,
                        "pool": pool_name,
                        "usedTokens": used,
                        "budget": budget,
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) => return internal(&format!("budget check: {e}")),
        }
    }

    // d. read the credential the gateway will inject (never the agent's own).
    let secrets: Api<Secret> = Api::namespaced(state.client.clone(), &ns);
    let secret_name = &spec.credential_secret_ref.name;
    let secret = match secrets.get_opt(secret_name).await {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(&format!("Secret {secret_name} not found"), &ns),
        Err(e) => return internal(&format!("get Secret {secret_name}: {e}")),
    };
    let key = match read_secret_key(&secret, &spec.credential_secret_ref.key) {
        Ok(k) => k,
        Err(e) => return internal(&e),
    };

    // e. inject the default model when the body pins none, then forward with the
    //    pool's credential injected as a bearer token.
    inject_model(&mut body, spec.default_model.as_deref());
    let url = format!("{}/v1/infer", spec.endpoint.trim_end_matches('/'));
    let resp = match reqwest::Client::new()
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            state.metrics.inc_error();
            return bad_gateway(&format!("provider POST {url}: {e}"));
        }
    };
    if !resp.status().is_success() {
        state.metrics.inc_error();
        let code = resp.status().as_u16();
        let detail = resp.text().await.unwrap_or_default();
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "provider error", "status": code, "detail": detail })),
        )
            .into_response();
    }
    let mut provider_body = match resp.json::<Value>().await {
        Ok(b) => b,
        Err(e) => {
            state.metrics.inc_error();
            return bad_gateway(&format!("decode provider response: {e}"));
        }
    };

    // f. meter the tokens, then return the provider body tagged with the pool.
    let total = provider_body
        .pointer("/usage/total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    state.metrics.add_tokens(total);
    if let Err(e) = store::record_usage(&state.pool, &ns, &pool_name, &agent, total).await {
        tracing::warn!(%ns, pool = %pool_name, error = %e, "record usage failed");
    }
    if let Some(obj) = provider_body.as_object_mut() {
        obj.insert(
            "x-agentctl-pool".to_string(),
            json!(format!("{ns}/{pool_name}")),
        );
    }
    Json(provider_body).into_response()
}

/// `GET /v1/usage?namespace=&pool=` — the consumption report for a pool.
async fn usage(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let ns = match params.get("namespace").filter(|s| !s.is_empty()) {
        Some(ns) => ns.clone(),
        None => return bad_request("namespace query parameter required"),
    };
    let want_pool = params.get("pool").filter(|s| !s.is_empty()).cloned();

    let pools: Api<ModelPool> = Api::namespaced(state.client.clone(), &ns);
    let (pool_name, spec) = match select_pool(&pools, want_pool.as_deref()).await {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(&no_pool_msg(&ns, want_pool.as_deref()), &ns),
        Err(e) => return internal(&format!("select ModelPool: {e}")),
    };
    let (used, requests) = match store::usage_report(&state.pool, &ns, &pool_name).await {
        Ok(v) => v,
        Err(e) => return internal(&format!("usage report: {e}")),
    };
    let budget = spec.budget.as_ref().map(|b| b.max_tokens);
    Json(usage_json(&ns, &pool_name, used, requests, budget)).into_response()
}

// --- kube glue (needs a cluster to run, not to compile/test) ---------------

/// Select the `ModelPool` for a request: by name when `want` is given (404 if
/// absent), else the first pool in the namespace. `Ok(None)` ⇒ no matching pool.
async fn select_pool(
    api: &Api<ModelPool>,
    want: Option<&str>,
) -> Result<Option<(String, ModelPoolSpec)>, String> {
    if let Some(name) = want {
        return match api.get_opt(name).await.map_err(|e| e.to_string())? {
            Some(mp) => {
                let resolved = mp.metadata.name.clone().unwrap_or_else(|| name.to_string());
                Ok(Some((resolved, mp.spec)))
            }
            None => Ok(None),
        };
    }
    let list = api
        .list(&ListParams::default())
        .await
        .map_err(|e| e.to_string())?;
    Ok(list.items.into_iter().next().map(|mp| {
        let name = mp.metadata.name.clone().unwrap_or_default();
        (name, mp.spec)
    }))
}

/// Read and UTF-8-decode the named key from a `Secret`. The typed kube client
/// already base64-decodes `data` into raw `ByteString` bytes, so this only maps
/// bytes → string (trimming trailing whitespace/newlines).
fn read_secret_key(secret: &Secret, key: &str) -> Result<String, String> {
    let bytes = secret
        .data
        .as_ref()
        .and_then(|d| d.get(key))
        .ok_or_else(|| format!("Secret has no key {key:?}"))?;
    let s = String::from_utf8(bytes.0.clone())
        .map_err(|e| format!("Secret key {key:?} is not valid UTF-8: {e}"))?;
    Ok(s.trim().to_string())
}

// --- pure helpers (unit-tested) --------------------------------------------

/// Whether `used` tokens have reached/exceeded the pool's `budget`. No budget
/// (`None`) ⇒ never over budget. The check is `>=` so the request that would
/// cross the line is the one rejected (pre-request, conservative).
fn over_budget(used: i64, budget: Option<i64>) -> bool {
    matches!(budget, Some(b) if used >= b)
}

/// Inject `default_model` into the request body when it pins no (non-empty)
/// model. No-op when there is no default, the body is not a JSON object, or the
/// body already carries a non-empty `model`. The agent stays provider-neutral;
/// the pool supplies the default.
fn inject_model(body: &mut Value, default_model: Option<&str>) {
    let Some(model) = default_model else { return };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let has_model = obj
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !has_model {
        obj.insert("model".to_string(), json!(model));
    }
}

/// Build the `GET /v1/usage` response body. `budget`/`remaining` are JSON `null`
/// when the pool has no budget; `remaining` is floored at 0 (never negative).
fn usage_json(ns: &str, pool: &str, used: i64, requests: i64, budget: Option<i64>) -> Value {
    json!({
        "namespace": ns,
        "pool": pool,
        "usedTokens": used,
        "requests": requests,
        "budget": budget,
        "remaining": budget.map(|b| (b - used).max(0)),
    })
}

/// The "no ModelPool" error message — distinguishes a missing named pool from an
/// empty namespace.
fn no_pool_msg(ns: &str, want: Option<&str>) -> String {
    match want {
        Some(name) => format!("ModelPool {name} not found in {ns}"),
        None => format!("no ModelPool in namespace {ns}"),
    }
}

// --- small response helpers ------------------------------------------------

/// Read a header value as an owned `String`, treating empty as absent.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

fn not_found(msg: &str, ns: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": msg, "namespace": ns })),
    )
        .into_response()
}

fn internal(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg })),
    )
        .into_response()
}

fn bad_gateway(msg: &str) -> Response {
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": msg }))).into_response()
}

/// Build the Postgres connection pool for the usage meter from `DATABASE_URL`
/// (e.g. `postgres://user:pw@host:5432/db?sslmode=disable`).
///
/// `sslmode=disable` (the default path) → [`tokio_postgres::NoTls`]: a plain
/// in-cluster hop, kept NetworkPolicy-scoped. Any other mode (`require`/`prefer`,
/// e.g. bundled `postgres.tls.enabled` or an external managed DSN) → a rustls/ring
/// connector ([`db_tls::make_connector`]) that encrypts the hop. Both stay
/// pure-Rust (no C toolchain).
fn build_pool() -> Pool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let cfg: tokio_postgres::Config = url.parse().expect("parse DATABASE_URL");
    let mgr = if cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
        deadpool_postgres::Manager::new(cfg, tokio_postgres::NoTls)
    } else {
        deadpool_postgres::Manager::new(cfg, db_tls::make_connector())
    };
    Pool::builder(mgr)
        .max_size(8)
        .build()
        .expect("build postgres pool")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_budget_none_is_never_over() {
        assert!(!over_budget(0, None));
        assert!(!over_budget(i64::MAX, None));
    }

    #[test]
    fn over_budget_is_inclusive_at_the_limit() {
        assert!(!over_budget(99, Some(100)));
        assert!(over_budget(100, Some(100)));
        assert!(over_budget(101, Some(100)));
    }

    #[test]
    fn over_budget_zero_budget_blocks_everything() {
        assert!(over_budget(0, Some(0)));
    }

    #[test]
    fn inject_model_fills_when_absent() {
        let mut body = json!({ "messages": [] });
        inject_model(&mut body, Some("claude-x"));
        assert_eq!(body["model"], "claude-x");
    }

    #[test]
    fn inject_model_fills_when_empty_string() {
        let mut body = json!({ "model": "", "messages": [] });
        inject_model(&mut body, Some("claude-x"));
        assert_eq!(body["model"], "claude-x");
    }

    #[test]
    fn inject_model_preserves_pinned_model() {
        let mut body = json!({ "model": "pinned", "messages": [] });
        inject_model(&mut body, Some("claude-x"));
        assert_eq!(body["model"], "pinned");
    }

    #[test]
    fn inject_model_noop_without_default() {
        let mut body = json!({ "messages": [] });
        inject_model(&mut body, None);
        assert_eq!(body.get("model"), None);
    }

    #[test]
    fn inject_model_noop_on_non_object() {
        let mut body = json!("not-an-object");
        inject_model(&mut body, Some("claude-x"));
        assert_eq!(body, json!("not-an-object"));
    }

    #[test]
    fn usage_json_with_budget_reports_remaining() {
        let v = usage_json("team-a", "default", 30, 3, Some(100));
        assert_eq!(v["namespace"], "team-a");
        assert_eq!(v["pool"], "default");
        assert_eq!(v["usedTokens"], 30);
        assert_eq!(v["requests"], 3);
        assert_eq!(v["budget"], 100);
        assert_eq!(v["remaining"], 70);
    }

    #[test]
    fn usage_json_remaining_floors_at_zero() {
        let v = usage_json("ns", "p", 150, 5, Some(100));
        assert_eq!(v["remaining"], 0);
    }

    #[test]
    fn usage_json_without_budget_is_null() {
        let v = usage_json("ns", "p", 42, 7, None);
        assert_eq!(v["budget"], Value::Null);
        assert_eq!(v["remaining"], Value::Null);
        assert_eq!(v["usedTokens"], 42);
        assert_eq!(v["requests"], 7);
    }

    #[test]
    fn no_pool_msg_distinguishes_named_from_empty() {
        assert_eq!(
            no_pool_msg("team-a", Some("gpt")),
            "ModelPool gpt not found in team-a"
        );
        assert_eq!(
            no_pool_msg("team-a", None),
            "no ModelPool in namespace team-a"
        );
    }

    #[test]
    fn read_secret_key_decodes_and_trims() {
        use k8s_openapi::ByteString;
        use std::collections::BTreeMap;
        let mut data = BTreeMap::new();
        data.insert("apiKey".to_string(), ByteString(b"sk-secret\n".to_vec()));
        let secret = Secret {
            data: Some(data),
            ..Default::default()
        };
        assert_eq!(read_secret_key(&secret, "apiKey").unwrap(), "sk-secret");
    }

    #[test]
    fn read_secret_key_missing_key_errors() {
        let secret = Secret::default();
        assert!(read_secret_key(&secret, "apiKey").is_err());
    }
}
