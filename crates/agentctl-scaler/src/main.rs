// SPDX-License-Identifier: BUSL-1.1
//! agentctl KEDA EXTERNAL gRPC scaler (agentctl RFC 0011 §5) — the off-pod,
//! scale-from-zero trigger for claim-mode `AgentFleet`s.
//!
//! At replica 0 there is no pod to scrape, so a Prometheus trigger reading the
//! per-pod `agent_pending_events` gauge sums to zero and can never wake the fleet
//! (RFC 0011 §5.1). The only thing that knows work is pending while no worker runs
//! is the **reference coordination MCP server** (`crates/agentctl-coordination`),
//! which exposes the off-pod backlog (`work.stats` → `pending`, contract ask P9).
//! This binary serves KEDA's `externalscaler.proto` over gRPC, reading that
//! backlog and mapping it onto the four RPCs (see [`scaler`]).
//!
//! Surfaces:
//!   * gRPC `ExternalScaler` on `GRPC_PORT` (default 9100) — what KEDA dials.
//!   * HTTP on `HEALTH_PORT` (default 8080): `GET /healthz`, `/readyz`, `/metrics`
//!     (`agentctl_scaler_{stats_reads_total,stats_errors_total,last_backlog}`).
//!
//! Plaintext is fine for v1 — the scaler sits behind the cluster network (RFC 0011
//! §3.4), exactly like the coordination server. Graceful shutdown on SIGTERM/SIGINT
//! drains both servers (matches the gateway). Hand-rolled in Rust; agentctl is
//! Rust-only.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

mod client;
mod metrics;
mod pb;
mod scaler;

use metrics::Metrics;
use pb::external_scaler_server::ExternalScalerServer;
use scaler::{Scaler, DEFAULT_STREAM_POLL_INTERVAL_MS};

/// Default gRPC port KEDA dials (`agentctl-scaler.agentctl-system:9100`, RFC 0011 §5.2).
const DEFAULT_GRPC_PORT: u16 = 9100;
/// Default HTTP port for `/healthz` + `/readyz` + `/metrics`.
const DEFAULT_HEALTH_PORT: u16 = 8080;

/// Shared HTTP-surface state.
#[derive(Clone)]
struct HttpState {
    metrics: Arc<Metrics>,
    /// Flipped true once both servers are bound and serving (drives `/readyz`).
    ready: Arc<AtomicBool>,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set — matches every other control-plane bin.
    agentctl_telemetry::init("agentctl-scaler");

    // ring crypto provider as the process default: no aws-lc-rs → no C toolchain.
    // Required so reqwest's rustls backend resolves a provider when building the
    // coordination-hop client (both the plaintext and the mTLS path).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let grpc_port = env_u16("GRPC_PORT", DEFAULT_GRPC_PORT);
    let health_port = env_u16("HEALTH_PORT", DEFAULT_HEALTH_PORT);
    let poll_ms = env_u64("STREAM_POLL_INTERVAL_MS", DEFAULT_STREAM_POLL_INTERVAL_MS);

    let metrics = Arc::new(Metrics::new());
    let ready = Arc::new(AtomicBool::new(false));

    // Coordination-hop client. Gated on COORDINATION_CLIENT_CERT_DIR: unset ⇒
    // today's plaintext-http client (optional bearer applied per-request); set ⇒ a
    // ring-backed mTLS client presenting the scaler's client cert and verifying the
    // coordination server cert against COORDINATION_CA (see src/client.rs). No
    // native-tls/openssl/aws-lc → no C toolchain.
    let client_mode = client::mode_from_env();
    if let client::ClientMode::Mtls { cert, ca, .. } = &client_mode {
        tracing::info!(
            cert = %cert.display(),
            ca = %ca.display(),
            "{} set: presenting client cert (mTLS) on work.stats requests",
            client::ENV_CLIENT_CERT_DIR,
        );
    }
    let http = client::build_client(&client_mode).expect("build coordination HTTP client");

    // Bearer token presented to the coordination server's gated work.stats endpoint
    // (agentctl RFC 0011 §3.4). Read once from AGENTCTL_API_TOKEN (set by the
    // operator/chart). Unset/empty ⇒ no header (back-compat with an open coordinator).
    let auth_token = std::env::var("AGENTCTL_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(Arc::new);
    if auth_token.is_some() {
        tracing::info!("AGENTCTL_API_TOKEN set: presenting bearer token on work.stats requests");
    }
    let svc = Scaler::new(
        http,
        metrics.clone(),
        Duration::from_millis(poll_ms.max(1)),
        auth_token,
    );

    // One shutdown signal fans out to both servers via a watch channel.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    // --- HTTP surface (health/readiness/metrics) -------------------------------
    let http_state = HttpState {
        metrics: metrics.clone(),
        ready: ready.clone(),
    };
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(serve_metrics))
        .with_state(http_state);
    let http_addr = SocketAddr::from(([0, 0, 0, 0], health_port));
    let http_listener = TcpListener::bind(http_addr)
        .await
        .unwrap_or_else(|e| panic!("bind health {http_addr}: {e}"));
    let http_server = {
        let mut rx = shutdown_rx.clone();
        axum::serve(http_listener, app).with_graceful_shutdown(async move {
            let _ = rx.changed().await;
        })
    };

    // --- gRPC ExternalScaler ---------------------------------------------------
    let grpc_addr = SocketAddr::from(([0, 0, 0, 0], grpc_port));
    let grpc_server = {
        let mut rx = shutdown_rx.clone();
        tonic::transport::Server::builder()
            .add_service(ExternalScalerServer::new(svc))
            .serve_with_shutdown(grpc_addr, async move {
                let _ = rx.changed().await;
            })
    };

    ready.store(true, Ordering::Release);
    tracing::info!(
        %grpc_addr,
        %http_addr,
        poll_ms,
        "agentctl scaler: serving KEDA externalscaler.proto (off-pod backlog, scale-from-zero)"
    );

    // Run both to completion; either erroring is fatal.
    let (grpc_res, http_res) = tokio::join!(grpc_server, http_server);
    grpc_res.expect("grpc serve");
    http_res.expect("http serve");
}

/// `GET /readyz` — 200 once both servers are bound and serving, else 503.
async fn readyz(State(state): State<HttpState>) -> StatusCode {
    if state.ready.load(Ordering::Acquire) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// `GET /metrics` — Prometheus exposition (node-agent text format).
async fn serve_metrics(
    State(state): State<HttpState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

/// Wait for SIGTERM/SIGINT, then resolve so both servers drain in-flight work.
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
    tracing::info!("shutting down: draining the gRPC + HTTP servers");
}

/// Parse a `u16` env var, falling back to `default`.
fn env_u16(key: &str, default: u16) -> u16 {
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
    fn ports_default_when_env_absent() {
        assert_eq!(env_u16("SCALER_NOPE_GRPC", DEFAULT_GRPC_PORT), 9100);
        assert_eq!(env_u16("SCALER_NOPE_HEALTH", DEFAULT_HEALTH_PORT), 8080);
        assert_eq!(
            env_u64("SCALER_NOPE_POLL", DEFAULT_STREAM_POLL_INTERVAL_MS),
            2_000
        );
    }
}
