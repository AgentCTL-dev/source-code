// SPDX-License-Identifier: BUSL-1.1
//! The operator's health + metrics HTTP surface.
//!
//! A small plaintext axum server (the same stack the node-agent serves
//! `/healthz` + `/metrics` with, RFC 0008/0010 — no second HTTP/metrics stack):
//!
//! * `GET /healthz` — 200 while the process is alive (liveness). Served by every
//!   replica, leader or standby, so the kubelet never kills a healthy standby.
//! * `GET /readyz` — 200 once the manager is running AND this replica holds
//!   leadership; 503 otherwise (a standby is intentionally un-ready, so a
//!   fronting `Service` routes only to the active leader).
//! * `GET /metrics` — the Prometheus exposition from [`Metrics::render`], scraped
//!   by the operator `ServiceMonitor`.
//!
//! The port is configurable via `HEALTH_PORT` (or `METRICS_PORT`), default 8080.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::metrics::Metrics;

/// Default health/metrics port (matches the other components' `http` port).
pub const DEFAULT_PORT: u16 = 8080;

/// Resolve the bind port from `HEALTH_PORT`, then `METRICS_PORT`, else
/// [`DEFAULT_PORT`]. An unparseable value falls back to the default.
pub fn port_from_env() -> u16 {
    std::env::var("HEALTH_PORT")
        .or_else(|_| std::env::var("METRICS_PORT"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Build the health/metrics router over the shared [`Metrics`].
pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
        .with_state(metrics)
}

/// Bind `addr` and serve the health/metrics router until the process exits.
pub async fn serve(addr: SocketAddr, metrics: Arc<Metrics>) {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind health/metrics addr {addr}: {e}"));
    if let Err(e) = axum::serve(listener, router(metrics)).await {
        // The health server failing is unrecoverable: without it the kubelet
        // probes fail and the pod is restarted.
        panic!("health/metrics server: {e}");
    }
}

/// Liveness: 200 as long as the process can answer (every replica).
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: 200 once the manager is up AND this replica is the leader.
async fn readyz(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    if metrics.is_ready() {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "standby")
    }
}

/// Prometheus exposition (same content type the node-agent serves).
async fn metrics_handler(
    State(metrics): State<Arc<Metrics>>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        metrics.render(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_defaults_to_8080_without_env() {
        // No env set in this test process → the default.
        // (HEALTH_PORT / METRICS_PORT are not set in the test environment.)
        assert_eq!(DEFAULT_PORT, 8080);
        assert_eq!(port_from_env(), 8080);
    }

    #[tokio::test]
    async fn readyz_flips_with_leadership() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt; // oneshot

        let metrics = Arc::new(Metrics::new());
        let app = router(metrics.clone());

        // standby (not ready) → 503
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

        // leader + manager up → 200
        metrics.set_manager_up(true);
        metrics.set_leader(true);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_is_always_ok_and_metrics_render() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let metrics = Arc::new(Metrics::new());
        let app = router(metrics);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res.headers().get(header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "text/plain; version=0.0.4");
    }
}
