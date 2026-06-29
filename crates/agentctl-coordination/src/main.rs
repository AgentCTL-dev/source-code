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

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use tokio::net::TcpListener;

mod mcp;
mod metrics;
mod store;

use metrics::Metrics;
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
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set — matches every other control-plane bin.
    agentctl_telemetry::init("agentctl-coordination");

    let dedupe_cap = env_usize("COORDINATION_DEDUPE_CAP", DEFAULT_DEDUPE_CAP);
    let sweep_ms = env_u64("COORDINATION_SWEEP_INTERVAL_MS", DEFAULT_SWEEP_INTERVAL_MS);

    let store: Arc<dyn ClaimStore> = Arc::new(InMemoryStore::new(dedupe_cap));
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

    let state = AppState {
        store,
        metrics,
        ready,
    };

    let app = Router::new()
        .route("/", post(rpc))
        .route("/mcp", post(rpc))
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
}
