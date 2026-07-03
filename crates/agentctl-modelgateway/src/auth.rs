// SPDX-License-Identifier: BUSL-1.1
//! Optional bearer-token access gate for the data endpoints.
//!
//! The token is read from `AGENTCTL_API_TOKEN` at startup:
//!   * **unset / empty** → the gate is OFF; every route is served without
//!     authentication (the in-cluster default).
//!   * **set** → the gate is ON: data routes require
//!     `Authorization: Bearer <AGENTCTL_API_TOKEN>` and return `401` (no body) on
//!     a missing/mismatched header. The compare is **constant-time**
//!     ([`subtle::ConstantTimeEq`]) to avoid a token timing side-channel.
//!
//! The probes + Prometheus scrape ([`EXEMPT`]) are NEVER gated. This is an
//! additional access gate in front of the existing `X-Agent-*` identity logic —
//! it does not replace it.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::metrics::Metrics;

/// Paths always exempt from the gate: liveness/readiness probes + Prometheus.
const EXEMPT: &[&str] = &["/healthz", "/readyz", "/metrics"];

/// The access-gate state threaded into the middleware: the expected token (when
/// configured) and the metrics surface for the rejection counter.
#[derive(Clone)]
pub struct Auth {
    /// `Some` when `AGENTCTL_API_TOKEN` is set & non-empty → enforce. `None` →
    /// the gate is disabled.
    token: Option<Arc<[u8]>>,
    metrics: Arc<Metrics>,
}

impl Auth {
    /// Build the gate from the environment, logging whether it is enforced.
    pub fn from_env(metrics: Arc<Metrics>) -> Self {
        let token = std::env::var("AGENTCTL_API_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(|t| Arc::from(t.into_bytes().into_boxed_slice()));
        if token.is_some() {
            tracing::info!("AGENTCTL_API_TOKEN set: bearer auth ENFORCED on data endpoints");
        } else {
            tracing::info!("AGENTCTL_API_TOKEN unset: data endpoints are UNAUTHENTICATED");
        }
        Self { token, metrics }
    }
}

/// axum middleware enforcing the bearer-token gate. No token configured → pass
/// through; exempt path → pass through; otherwise require a matching
/// `Authorization: Bearer` header, returning `401` (no body) on failure.
pub async fn gate(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    let Some(expected) = auth.token.as_deref() else {
        return next.run(req).await;
    };
    if is_exempt(req.uri().path()) {
        return next.run(req).await;
    }
    if authorized(req.headers().get(header::AUTHORIZATION), expected) {
        next.run(req).await
    } else {
        auth.metrics.inc_auth_rejected();
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Whether `path` is one of the always-exempt probe/metrics routes.
fn is_exempt(path: &str) -> bool {
    EXEMPT.contains(&path)
}

/// Constant-time check of an `Authorization: Bearer <token>` header against the
/// `expected` token bytes. Missing header, non-ASCII value, or a non-`Bearer`
/// scheme all fail closed.
fn authorized(header: Option<&HeaderValue>, expected: &[u8]) -> bool {
    let Some(token) = header
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    token.as_bytes().ct_eq(expected).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn exempt_paths_are_the_probes_and_metrics() {
        assert!(is_exempt("/healthz"));
        assert!(is_exempt("/readyz"));
        assert!(is_exempt("/metrics"));
        assert!(!is_exempt("/v1/infer"));
        assert!(!is_exempt("/v1/usage"));
    }

    #[test]
    fn authorized_matches_exact_bearer_token() {
        assert!(authorized(Some(&hv("Bearer s3cr3t")), b"s3cr3t"));
    }

    #[test]
    fn authorized_rejects_mismatch_missing_and_malformed() {
        assert!(!authorized(Some(&hv("Bearer nope")), b"s3cr3t"));
        assert!(!authorized(None, b"s3cr3t"));
        assert!(!authorized(Some(&hv("s3cr3t")), b"s3cr3t")); // no scheme
        assert!(!authorized(Some(&hv("Basic s3cr3t")), b"s3cr3t"));
        assert!(!authorized(Some(&hv("Bearer ")), b"s3cr3t"));
        assert!(!authorized(Some(&hv("Bearer s3cr3t")), b"s3cr3t-longer"));
    }
}
