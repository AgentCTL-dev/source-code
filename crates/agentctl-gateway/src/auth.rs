// SPDX-License-Identifier: BUSL-1.1
//! Optional bearer-token access gate for the A2A data surface.
//!
//! The token is read from `AGENTCTL_API_TOKEN` at startup:
//!   * **unset / empty** → the gate is OFF; every route is served without
//!     authentication (the in-cluster default).
//!   * **set** → the gate is ON: the A2A surface (`POST /agents/{ns}/{name}`, the
//!     registry `GET /agents`, the agent/fleet card endpoints) requires
//!     `Authorization: Bearer <AGENTCTL_API_TOKEN>` and returns `401` (no body)
//!     on a missing/mismatched header. The compare is **constant-time**
//!     ([`subtle::ConstantTimeEq`]) to avoid a token timing side-channel.
//!
//! The probes + Prometheus scrape AND the public JWKS ([`EXEMPT`]) are NEVER
//! gated: `GET /.well-known/jwks.json` is the verification key for signed Agent
//! Cards and must stay reachable without the token.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::metrics::Metrics;

/// Paths always exempt from the gate: liveness/readiness probes, Prometheus, and
/// the public JWKS (the Agent Card verification key — must stay public).
const EXEMPT: &[&str] = &["/healthz", "/readyz", "/metrics", "/.well-known/jwks.json"];

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
            tracing::info!("AGENTCTL_API_TOKEN set: bearer auth ENFORCED on the A2A surface");
        } else {
            tracing::info!("AGENTCTL_API_TOKEN unset: the A2A surface is UNAUTHENTICATED");
        }
        Self { token, metrics }
    }

    /// The coarse bearer-token check, evaluated inline by the A2A RPC handler for
    /// agents WITHOUT per-agent OIDC. Returns `true` when the gate is disabled
    /// (no token configured) or the request carries the matching bearer token.
    ///
    /// This is the same decision [`gate`] makes, exposed so the RPC handler can
    /// fall back to it once it has determined the agent has no `spec.access.oidc`
    /// (per-agent OIDC takes precedence — see [`gate`]).
    pub fn authorize(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = self.token.as_deref() else {
            return true;
        };
        authorized(headers.get(header::AUTHORIZATION), expected)
    }
}

/// axum middleware enforcing the bearer-token gate. No token configured → pass
/// through; exempt path (probes/metrics/JWKS) → pass through; otherwise require a
/// matching `Authorization: Bearer` header, returning `401` (no body) on failure.
///
/// The A2A JSON-RPC route — the gateway's only `POST` (`POST /agents/{ns}/{name}`)
/// — is ALWAYS deferred to its handler, which performs per-agent enforcement:
/// per-agent OIDC ([`crate::oidc`]) when `spec.access.oidc` is set (precedence),
/// otherwise this same coarse token via [`Auth::authorize`]. The registry
/// `GET /agents` and the card endpoints stay on this coarse gate (signed
/// discovery).
pub async fn gate(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    // Defer the A2A RPC route (the only POST) to its handler so per-agent OIDC can
    // take precedence over the coarse bearer token.
    if req.method() == Method::POST {
        return next.run(req).await;
    }
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

/// Whether `path` is one of the always-exempt probe/metrics/JWKS routes.
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
    fn exempt_paths_include_probes_metrics_and_public_jwks() {
        assert!(is_exempt("/healthz"));
        assert!(is_exempt("/readyz"));
        assert!(is_exempt("/metrics"));
        assert!(is_exempt("/.well-known/jwks.json"));
        // The A2A data surface is NOT exempt.
        assert!(!is_exempt("/agents"));
        assert!(!is_exempt("/agents/team-a/echo"));
        assert!(!is_exempt(
            "/agents/team-a/echo/.well-known/agent-card.json"
        ));
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

    fn auth_with(token: Option<&str>) -> Auth {
        Auth {
            token: token.map(|t| Arc::from(t.as_bytes().to_vec().into_boxed_slice())),
            metrics: Arc::new(Metrics::new()),
        }
    }

    #[test]
    fn authorize_passes_when_gate_disabled() {
        // No token configured → the inline fallback always allows.
        let auth = auth_with(None);
        assert!(auth.authorize(&HeaderMap::new()));
    }

    #[test]
    fn authorize_enforces_configured_token() {
        let auth = auth_with(Some("s3cr3t"));
        // Missing header → rejected.
        assert!(!auth.authorize(&HeaderMap::new()));
        // Matching bearer → allowed.
        let mut ok = HeaderMap::new();
        ok.insert(header::AUTHORIZATION, hv("Bearer s3cr3t"));
        assert!(auth.authorize(&ok));
        // Wrong token → rejected.
        let mut bad = HeaderMap::new();
        bad.insert(header::AUTHORIZATION, hv("Bearer nope"));
        assert!(!auth.authorize(&bad));
    }
}
