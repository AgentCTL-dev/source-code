// SPDX-License-Identifier: BUSL-1.1
//! agentctl reference coordination MCP server (agentctl RFC 0011 §3.2) — the
//! claim-mode **correctness backbone**: the single serializing point that makes
//! exactly-one-owner hold across replicas.
//!
//! It serves the FROZEN `work.*` contract (agentd RFC 0015 §5.6) over MCP
//! JSON-RPC — the server side of what the conformant agent
//! (`agentd crates/agentd/src/cluster/claim.rs`) calls: an atomic `work.claim`,
//! the lease lifecycle (`renew`/`ack`/`release` + TTL expiry), transactional
//! dedupe on `claim_key`, and the off-pod backlog count (`work.stats` /
//! `work://pending`, contract ask P9) the future KEDA external scaler reads to
//! scale a fleet **from zero**.
//!
//! Surface:
//!   * `POST /` and `POST /mcp` — MCP JSON-RPC 2.0.
//!   * `GET /healthz` — liveness (always 200 while serving).
//!   * `GET /readyz`  — 200 once the lease-sweep loop is up.
//!   * `GET /metrics` — Prometheus exposition (agentctl RFC 0010 text format).
//!
//! Plain HTTP is fine for v1 — it sits behind the cluster network / egress proxy
//! (agentctl RFC 0011 §3.4). Hand-rolled in Rust (axum); agentctl is Rust-only.
//!
//! **Open question (agentctl RFC 0011 §3.2 / §10):** HA, durability, and
//! per-fleet vs cluster-shared sharding of this single replica. The store sits
//! behind the [`store::ClaimStore`] trait so a Redis/Postgres backend slots in
//! without touching the wire layer; v1 ships the in-memory store and documents
//! that a coordination loss collapses the serializing point for dependent fleets.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use tokio::net::TcpListener;

mod db_tls;
mod mcp;
mod metrics;
mod pg_store;
mod store;

use metrics::Metrics;
use pg_store::PgClaimStore;
use store::{ClaimStore, InMemoryStore};

/// FIFO bound on the dedupe (done) set — see `store::DoneSet`. Override with
/// `COORDINATION_DEDUPE_CAP`.
const DEFAULT_DEDUPE_CAP: usize = 100_000;
/// How often the background task sweeps expired leases back to pending. Override
/// with `COORDINATION_SWEEP_INTERVAL_MS`.
const DEFAULT_SWEEP_INTERVAL_MS: u64 = 250;

/// Shared handler state. `store` is the single serializing point (`Arc<dyn …>` so
/// a durable backend can replace it without a wire change).
#[derive(Clone)]
struct AppState {
    store: Arc<dyn ClaimStore>,
    metrics: Arc<Metrics>,
    /// Flipped true once the sweep loop is running (drives `/readyz`).
    ready: Arc<AtomicBool>,
    /// Optional bearer token gating the data endpoints (`POST /`, `POST /mcp`).
    /// `Some` ⇒ enforce `Authorization: Bearer <token>`; `None` (env unset/empty)
    /// ⇒ no auth (back-compat). Read once from `AGENTCTL_API_TOKEN` at startup.
    auth_token: Option<Arc<String>>,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set — matches every other control-plane bin.
    agentctl_telemetry::init("agentctl-coordination");

    let dedupe_cap = env_usize("COORDINATION_DEDUPE_CAP", DEFAULT_DEDUPE_CAP);
    let sweep_ms = env_u64("COORDINATION_SWEEP_INTERVAL_MS", DEFAULT_SWEEP_INTERVAL_MS);

    // BACKEND SELECTION (agentctl RFC 0011 §3.2 / §10): a durable, HA-capable
    // Postgres store when COORDINATION_DATABASE_URL (or DATABASE_URL) is set —
    // the serializing point becomes a shared DB row, so grant-one holds across
    // >1 replica and survives a restart. Absent, the in-memory store is the
    // (single-replica, non-durable) default. Both sit behind the SAME ClaimStore
    // trait, so the MCP wire layer is untouched either way.
    let store: Arc<dyn ClaimStore> = match coordination_database_url() {
        Some(url) => {
            tracing::info!(
                "coordination backend: Postgres (durable, HA-capable across replicas) \
                 via COORDINATION_DATABASE_URL/DATABASE_URL"
            );
            match PgClaimStore::connect(&url) {
                Ok(s) => Arc::new(s),
                Err(e) => panic!("coordination Postgres backend: {e}"),
            }
        }
        None => {
            tracing::info!(
                "coordination backend: in-memory (single-replica, non-durable default) — \
                 set COORDINATION_DATABASE_URL/DATABASE_URL for the durable Postgres backend"
            );
            Arc::new(InMemoryStore::new(dedupe_cap))
        }
    };
    let metrics = Arc::new(Metrics::new());
    let ready = Arc::new(AtomicBool::new(false));

    // Background sweeper: return expired leases to pending so a dead claimer's
    // item is re-offered to the fleet (agentd RFC 0019 §3.2). Marks ready once up.
    {
        let store = store.clone();
        let metrics = metrics.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            ready.store(true, Ordering::Release);
            let mut tick = tokio::time::interval(Duration::from_millis(sweep_ms.max(1)));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let n = store.sweep_expired();
                if n > 0 {
                    metrics.add_expired(n);
                    tracing::debug!(swept = n, "expired leases returned to pending");
                }
            }
        });
    }

    // Bearer-token gate (agentctl RFC 0011 §3.4 hardening): read AGENTCTL_API_TOKEN
    // once. Unset/empty ⇒ no auth (back-compat); set ⇒ enforce on the data routes.
    let auth_token = std::env::var("AGENTCTL_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(Arc::new);
    if auth_token.is_some() {
        tracing::info!(
            "AGENTCTL_API_TOKEN set: bearer-token gate enforced on POST / and POST /mcp"
        );
    } else {
        tracing::info!(
            "AGENTCTL_API_TOKEN unset: data endpoints are unauthenticated (back-compat)"
        );
    }

    let state = AppState {
        store,
        metrics,
        ready,
        auth_token,
    };

    // The data routes carry the bearer gate via `route_layer` so it runs ONLY for
    // `POST /` and `POST /mcp`; the probe/metrics routes added afterwards are always
    // exempt (never require the token).
    let app = Router::new()
        .route("/", post(rpc))
        .route("/mcp", post(rpc))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_gate))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(serve_metrics))
        .with_state(state);

    let port = port_from_env();
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    tracing::info!(%addr, dedupe_cap, sweep_ms, "agentctl coordination MCP server: serving the work.* claim surface");
    // Graceful shutdown on SIGTERM/SIGINT — drain in-flight requests (matches the
    // gateway). A SIGTERM is the normal pod-stop signal.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve");
}

/// Bearer-token gate for the data endpoints. When `state.auth_token` is `None`
/// (env unset/empty) every request passes through unchanged (back-compat). When
/// `Some`, the request MUST carry `Authorization: Bearer <token>` with a token
/// that matches in constant time; otherwise it is rejected with a bare 401 (no
/// body, no detail leak) and counted in `agentctl_coordination_auth_rejected_total`.
/// This layer wraps only `POST /` and `POST /mcp` — the probes/metrics are exempt.
async fn auth_gate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = &state.auth_token else {
        // No token configured ⇒ auth disabled.
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let ok = presented.is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        state.metrics.inc_auth_rejected();
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Constant-time byte-slice equality — avoids the early-exit timing side-channel of
/// `==` when comparing the secret token. Unequal lengths are not equal (length is
/// not itself the secret here); equal-length inputs are compared in full every time.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `POST /` and `POST /mcp` — one MCP JSON-RPC message (or a batch array). A
/// notification yields no body (202 Accepted).
async fn rpc(State(state): State<AppState>, Json(req): Json<Value>) -> Response {
    if let Some(batch) = req.as_array() {
        let mut out = Vec::new();
        for item in batch {
            if let Some(resp) = mcp::handle_rpc(item, state.store.as_ref(), &state.metrics) {
                out.push(resp);
            }
        }
        if out.is_empty() {
            StatusCode::ACCEPTED.into_response()
        } else {
            Json(Value::Array(out)).into_response()
        }
    } else {
        match mcp::handle_rpc(&req, state.store.as_ref(), &state.metrics) {
            Some(resp) => Json(resp).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        }
    }
}

/// `GET /readyz` — 200 once the sweep loop is running, else 503.
async fn readyz(State(state): State<AppState>) -> StatusCode {
    if state.ready.load(Ordering::Acquire) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// `GET /metrics` — Prometheus exposition. The `pending`/`claimed` gauges are read
/// live from the store at scrape time.
async fn serve_metrics(
    State(state): State<AppState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    let s = state.store.stats();
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(s.pending, s.claimed),
    )
}

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

/// The HTTP port: `PORT`, then `HTTP_PORT`, else 8080.
fn port_from_env() -> u16 {
    for key in ["PORT", "HTTP_PORT"] {
        if let Ok(v) = std::env::var(key) {
            if let Ok(p) = v.parse::<u16>() {
                return p;
            }
        }
    }
    8080
}

/// The coordination Postgres DSN, if configured. Prefers the coordination-specific
/// `COORDINATION_DATABASE_URL`, then the shared `DATABASE_URL`. An unset OR empty
/// value selects the in-memory backend (back-compat default).
fn coordination_database_url() -> Option<String> {
    for key in ["COORDINATION_DATABASE_URL", "DATABASE_URL"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Parse a `usize` env var, falling back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Parse a `u64` env var, falling back to `default`.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_defaults_to_8080() {
        // No env override in the test harness ⇒ default.
        assert_eq!(port_from_env(), 8080);
    }

    #[test]
    fn env_helpers_fall_back_on_absent_or_garbage() {
        assert_eq!(env_usize("COORDINATION_NOPE_USIZE", 7), 7);
        assert_eq!(env_u64("COORDINATION_NOPE_U64", 9), 9);
    }

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"s3cret-token", b"s3cret-token"));
        assert!(!constant_time_eq(b"s3cret-token", b"s3cret-toker"));
        assert!(!constant_time_eq(b"s3cret-token", b"s3cret")); // length mismatch
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
