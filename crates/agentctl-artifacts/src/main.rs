// SPDX-License-Identifier: BUSL-1.1
//! # agentctl artifacts façade (P3-3, RFC 0030 `artifacts.*`)
//!
//! The `artifacts.*` MCP backend: `put`/`get`/`list` over an S3-compatible
//! content store (MinIO in-cluster; any S3 in production). Unlike the small
//! seq-CAS `state.*` checkpoints, artifacts are opaque blobs — model outputs,
//! rendered documents, intermediate files — so they belong in object storage,
//! not the state Postgres.
//!
//! Two invariants make it multi-tenant-safe:
//!
//! - **Org fence.** Every object is keyed under the CALLER's org prefix
//!   (`<org>/…`), derived from the host-asserted `x-mcpg-subject-id` (the same
//!   identity seam `state.*` fences on). The caller supplies only a RELATIVE
//!   key; it can never name another org's keyspace, and path traversal is
//!   rejected. No identity ⇒ refuse (fail closed).
//! - **Org quota.** A per-org byte cap (`ARTIFACTS_ORG_QUOTA_BYTES`) checked
//!   before each write — one tenant cannot exhaust the shared store.
//!
//! Registered as an org `MCPService`, it federates through the tenant gateway
//! like the sandbox cell; the direct MCP surface here is what that gateway
//! proxies (and what the e2e drives).

mod s3;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};

use s3::S3;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Largest single object a caller may put (server-side ceiling).
const MAX_OBJECT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    s3: Arc<S3>,
    /// Per-org byte cap; 0 disables the quota check.
    quota_bytes: u64,
    auth_token: Option<Arc<String>>,
    metrics: Arc<Metrics>,
}

#[derive(Default)]
struct Metrics {
    puts: AtomicU64,
    gets: AtomicU64,
    lists: AtomicU64,
    quota_refusals: AtomicU64,
    failures: AtomicU64,
}

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| d.to_string())
}

#[tokio::main]
async fn main() {
    agentctl_telemetry::init("agentctl-artifacts");

    let s3 = S3 {
        http: reqwest::Client::new(),
        endpoint: env_or(
            "ARTIFACTS_S3_ENDPOINT",
            "http://agentctl-artifacts-minio:9000",
        )
        .trim_end_matches('/')
        .to_string(),
        bucket: env_or("ARTIFACTS_S3_BUCKET", "artifacts"),
        region: env_or("ARTIFACTS_S3_REGION", "us-east-1"),
        access: env_or("AWS_ACCESS_KEY_ID", ""),
        secret: env_or("AWS_SECRET_ACCESS_KEY", ""),
    };

    let state = AppState {
        s3: Arc::new(s3),
        quota_bytes: env_or("ARTIFACTS_ORG_QUOTA_BYTES", "104857600")
            .parse()
            .unwrap_or(104_857_600),
        auth_token: std::env::var("AGENTCTL_API_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(Arc::new),
        metrics: Arc::new(Metrics::default()),
    };

    // Ensure the bucket exists (MinIO may still be coming up — retry in the
    // background, forever, so serving is not blocked and a slow MinIO start
    // never leaves us permanently bucketless).
    {
        let s3 = state.s3.clone();
        tokio::spawn(async move {
            // Ensure the bucket at startup AND periodically thereafter: a MinIO
            // restart on an emptyDir (the dev default) loses the bucket, so a
            // periodic idempotent re-ensure self-heals it (a bucket we own
            // returns 409 = success). Backs off fast until first ready, then
            // re-checks every 30s.
            let mut ready = false;
            let mut attempts = 0u64;
            loop {
                match s3.ensure_bucket().await {
                    Ok(()) => {
                        if !ready {
                            tracing::info!("artifacts bucket ready");
                            ready = true;
                        }
                    }
                    Err(e) => {
                        attempts += 1;
                        if attempts % 10 == 1 {
                            tracing::warn!(error = %e, attempts, "bucket not ready; retrying");
                        }
                        ready = false;
                    }
                }
                let backoff = if ready { 30 } else { 3 };
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            }
        });
    }

    let app = Router::new()
        .route("/", post(rpc))
        .route("/mcp", post(rpc))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route("/metrics", get(serve_metrics))
        .with_state(state);
    let addr: std::net::SocketAddr = "0.0.0.0:8080".parse().unwrap();
    tracing::info!(%addr, "agentctl artifacts serving artifacts.*");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("serve");
}

async fn serve_metrics(State(st): State<AppState>) -> impl IntoResponse {
    let m = &st.metrics;
    (
        [("content-type", "text/plain; version=0.0.4")],
        format!(
            "# TYPE agentctl_artifacts_puts_total counter\nagentctl_artifacts_puts_total {}\n\
             # TYPE agentctl_artifacts_gets_total counter\nagentctl_artifacts_gets_total {}\n\
             # TYPE agentctl_artifacts_lists_total counter\nagentctl_artifacts_lists_total {}\n\
             # TYPE agentctl_artifacts_quota_refusals_total counter\nagentctl_artifacts_quota_refusals_total {}\n\
             # TYPE agentctl_artifacts_failures_total counter\nagentctl_artifacts_failures_total {}\n",
            m.puts.load(Ordering::Relaxed),
            m.gets.load(Ordering::Relaxed),
            m.lists.load(Ordering::Relaxed),
            m.quota_refusals.load(Ordering::Relaxed),
            m.failures.load(Ordering::Relaxed),
        ),
    )
}

/// The caller's org prefix, from the host-asserted subject. `orgs/<ns>/<agent>`
/// ⇒ `orgs/<ns>` (the org owns the keyspace + quota; agents in it share both).
fn org_prefix(headers: &HeaderMap) -> Option<String> {
    let subject = headers
        .get("x-mcpg-subject-id")
        .and_then(|v| v.to_str().ok())?
        .trim_matches('/');
    if subject.is_empty() {
        return None;
    }
    let segs: Vec<&str> = subject.split('/').filter(|s| !s.is_empty()).collect();
    Some(match segs.as_slice() {
        [a, b, ..] => format!("{a}/{b}"),
        _ => subject.to_string(),
    })
}

/// A relative object/prefix component: safe-charset (`[A-Za-z0-9/_.-]`), no
/// traversal. The safe-charset rule keeps S3 SigV4 signing exact (the path/query
/// need no percent-encoding — see `s3`); traversal segments are refused.
fn valid_relative(rel: &str) -> bool {
    !rel.split('/').any(|seg| seg == ".." || seg == ".")
        && rel
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'.' | b'-'))
}

/// Reject traversal / absolute / unsafe keys; return the object path `<org>/<key>`.
fn object_path(org: &str, key: &str) -> Result<String, Value> {
    let key = key.trim_start_matches('/');
    if key.is_empty() || !valid_relative(key) {
        return Err(json!({ "code": "invalid_key",
            "message": "key must be a non-empty relative [A-Za-z0-9/_.-] path with no `.`/`..` segments" }));
    }
    Ok(format!("{org}/{key}"))
}

async fn rpc(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> axum::response::Response {
    if let Some(expected) = &st.auth_token {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        if presented != expected.as_str() {
            return (StatusCode::UNAUTHORIZED, "bearer required").into_response();
        }
    }
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    if method.starts_with("notifications/") {
        return StatusCode::ACCEPTED.into_response();
    }
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let resp = match method {
        "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "agentctl-artifacts", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "artifacts.put/get/list store opaque blobs in your org's content store. Keys are relative to your org; a per-org byte quota applies.",
        } }),
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tool_specs() } }),
        "tools/call" => {
            let name = params
                .pointer("/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let (body, is_error) = match call_tool(&st, &headers, name, &args).await {
                Ok(v) => (v, false),
                Err(e) => {
                    st.metrics.failures.fetch_add(1, Ordering::Relaxed);
                    (json!({ "error": e }), true)
                }
            };
            let text = serde_json::to_string(&body).unwrap_or_default();
            json!({ "jsonrpc": "2.0", "id": id, "result": {
                "content": [{ "type": "text", "text": text }],
                "structuredContent": body,
                "isError": is_error,
            } })
        }
        other => json!({ "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("method not found: {other}") } }),
    };
    Json(resp).into_response()
}

fn tool_specs() -> Value {
    json!([
        {
            "name": "artifacts.put",
            "description": "Store a blob under a key in your org's content store (base64 content). Refused if it would exceed the org quota.",
            "inputSchema": { "type": "object", "required": ["key", "content_base64"], "properties": {
                "key": { "type": "string", "minLength": 1, "maxLength": 1024 },
                "content_base64": { "type": "string" },
                "content_type": { "type": "string" }
            } }
        },
        {
            "name": "artifacts.get",
            "description": "Fetch a blob by key (base64 content). structuredContent is null when the key is absent.",
            "inputSchema": { "type": "object", "required": ["key"], "properties": {
                "key": { "type": "string", "minLength": 1, "maxLength": 1024 }
            } }
        },
        {
            "name": "artifacts.list",
            "description": "List objects (key, size, last_modified) under an optional relative prefix in your org.",
            "inputSchema": { "type": "object", "properties": {
                "prefix": { "type": "string", "maxLength": 1024 }
            } }
        }
    ])
}

async fn call_tool(
    st: &AppState,
    headers: &HeaderMap,
    tool: &str,
    args: &Value,
) -> Result<Value, Value> {
    let org = org_prefix(headers).ok_or_else(|| {
        json!({ "code": "no_identity",
            "message": "no caller identity (x-mcpg-subject-id) — refusing (fail closed)" })
    })?;
    match tool {
        "artifacts.put" => put_obj(st, &org, args).await,
        "artifacts.get" => get_obj(st, &org, args).await,
        "artifacts.list" => list_obj(st, &org, args).await,
        other => Err(json!({ "code": "no_tool", "message": format!("no tool {other:?}") })),
    }
}

fn store_err(context: &str, e: impl std::fmt::Display) -> Value {
    json!({ "code": "store_error", "message": format!("{context}: {e}") })
}

/// Sum the bytes an org already holds (for the quota gate; paginates).
async fn org_usage(st: &AppState, org: &str) -> Result<u64, Value> {
    let objs = st
        .s3
        .list_all(&format!("{org}/"))
        .await
        .map_err(|e| store_err("list for quota", e))?;
    Ok(objs.iter().map(|o| o.size).sum())
}

async fn put_obj(st: &AppState, org: &str, args: &Value) -> Result<Value, Value> {
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| json!({ "code": "bad_args", "message": "key required" }))?;
    let path = object_path(org, key)?;
    let b64 = args
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| json!({ "code": "bad_args", "message": "content_base64 required" }))?;
    let bytes = B64
        .decode(b64)
        .map_err(|e| json!({ "code": "bad_args", "message": format!("content_base64: {e}") }))?;
    if bytes.len() > MAX_OBJECT_BYTES {
        return Err(json!({ "code": "too_large",
            "message": format!("object {} bytes exceeds the {MAX_OBJECT_BYTES}-byte ceiling", bytes.len()) }));
    }

    // Quota: refuse a write that would push the org over its cap. A put that
    // OVERWRITES an existing key still counts the old bytes here (conservative);
    // the cap is a guard rail, not an exact accountant.
    if st.quota_bytes > 0 {
        let used = org_usage(st, org).await?;
        if used + bytes.len() as u64 > st.quota_bytes {
            st.metrics.quota_refusals.fetch_add(1, Ordering::Relaxed);
            return Err(json!({ "code": "quota_exceeded", "message": format!(
                "org quota exceeded: {used} + {} > {} bytes", bytes.len(), st.quota_bytes) }));
        }
    }

    let size = bytes.len() as u64;
    let content_type = args.get("content_type").and_then(Value::as_str);
    st.s3
        .put(&path, bytes, content_type)
        .await
        .map_err(|e| store_err("put_object", e))?;
    st.metrics.puts.fetch_add(1, Ordering::Relaxed);
    Ok(json!({ "key": key, "size": size }))
}

async fn get_obj(st: &AppState, org: &str, args: &Value) -> Result<Value, Value> {
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| json!({ "code": "bad_args", "message": "key required" }))?;
    let path = object_path(org, key)?;
    match st
        .s3
        .get(&path)
        .await
        .map_err(|e| store_err("get_object", e))?
    {
        None => Ok(Value::Null),
        Some(bytes) => {
            st.metrics.gets.fetch_add(1, Ordering::Relaxed);
            Ok(json!({
                "key": key,
                "size": bytes.len(),
                "content_base64": B64.encode(&bytes),
            }))
        }
    }
}

async fn list_obj(st: &AppState, org: &str, args: &Value) -> Result<Value, Value> {
    let rel = args
        .get("prefix")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start_matches('/');
    if !rel.is_empty() && !valid_relative(rel) {
        return Err(json!({ "code": "invalid_key",
            "message": "prefix must be a relative [A-Za-z0-9/_.-] path" }));
    }
    let full_prefix = format!("{org}/{rel}");
    let objs = st
        .s3
        .list_all(&full_prefix)
        .await
        .map_err(|e| store_err("list", e))?;
    // Strip the org prefix so callers see their OWN relative keyspace.
    let strip = format!("{org}/");
    let items: Vec<Value> = objs
        .into_iter()
        .map(|o| {
            let key = o.key.strip_prefix(&strip).unwrap_or(&o.key).to_string();
            json!({ "key": key, "size": o.size, "last_modified": o.last_modified })
        })
        .collect();
    st.metrics.lists.fetch_add(1, Ordering::Relaxed);
    Ok(json!({ "items": items }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn hdrs(subject: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-mcpg-subject-id", HeaderValue::from_str(subject).unwrap());
        h
    }

    #[test]
    fn org_prefix_takes_the_first_two_subject_segments() {
        assert_eq!(
            org_prefix(&hdrs("orgs/org-acme/triage")).as_deref(),
            Some("orgs/org-acme")
        );
        assert_eq!(
            org_prefix(&hdrs("orgs/org-acme")).as_deref(),
            Some("orgs/org-acme")
        );
        assert_eq!(org_prefix(&HeaderMap::new()), None);
        assert_eq!(org_prefix(&hdrs("///")), None);
    }

    #[test]
    fn object_path_blocks_traversal_and_absolute_keys() {
        assert_eq!(object_path("orgs/o", "a/b.txt").unwrap(), "orgs/o/a/b.txt");
        // Leading slash is stripped, not treated as absolute.
        assert_eq!(object_path("orgs/o", "/x").unwrap(), "orgs/o/x");
        assert!(object_path("orgs/o", "../escape").is_err());
        assert!(object_path("orgs/o", "a/../../b").is_err());
        assert!(object_path("orgs/o", "").is_err());
    }
}
