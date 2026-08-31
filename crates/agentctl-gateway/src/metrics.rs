// SPDX-License-Identifier: BUSL-1.1
//! Prometheus `/metrics` exposition for the A2A gateway.
//!
//! Hand-rolled without a client library: the body is `text/plain; version=0.0.4`,
//! each metric emits its `# HELP`/`# TYPE` once followed by the sample. Counters
//! live behind atomics in the shared app state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process + request/task counters for the A2A gateway.
#[derive(Debug)]
pub struct Metrics {
    /// Process start time (unix epoch seconds) — the standard `process_*` gauge.
    start_time_secs: f64,
    /// A2A JSON-RPC requests received on `POST /agents/{ns}/{name}`.
    rpc_requests: AtomicU64,
    /// `message/stream` requests routed down the SSE passthrough.
    stream_requests: AtomicU64,
    /// Agent / fleet card projections served.
    card_requests: AtomicU64,
    /// Tasks persisted to the durable store (`message/send`).
    tasks: AtomicU64,
    /// Requests that failed at the upstream hop — dialing the agent pod's
    /// mTLS `/mcp` directly at `https://<pod-ip>:8443`.
    upstream_errors: AtomicU64,
    /// Requests rejected (401) by the bearer-token access gate.
    auth_rejected: AtomicU64,
    /// Owner approvals on destructive requests (P4-5): granted vs refused
    /// (wrong user / unknown nonce) — the "gates" observability (P7-2).
    approvals_allowed: AtomicU64,
    approvals_refused: AtomicU64,
    /// Hooks-ingress deliveries (P7-1): forwarded to the agent's webhook
    /// listener; refused at a gateway gate (exposure/method/rate/size);
    /// or dropped because no ready pod could be reached (503 + Retry-After).
    hooks_forwarded: AtomicU64,
    hooks_refused: AtomicU64,
    hooks_unreachable: AtomicU64,
    /// Per-agent OIDC requests allowed (valid JWT + claims) on the A2A surface.
    oidc_allow: AtomicU64,
    /// Per-agent OIDC requests denied (authN 401 or authZ 403) on the A2A surface.
    oidc_deny: AtomicU64,
    /// Trusted-proxy requests accepted on the verified mTLS listener (allow-listed
    /// peer + asserted identity + any requiredClaims satisfied).
    trusted_proxy_accepted: AtomicU64,
    /// Trusted-proxy requests rejected (peer-cert name not allow-listed, or the
    /// agent's requiredClaims unsatisfied by the asserted identity) → 403.
    trusted_proxy_rejected: AtomicU64,
}

impl Metrics {
    /// Construct with the process start time captured now.
    pub fn new() -> Self {
        Self {
            start_time_secs: unix_now_secs(),
            rpc_requests: AtomicU64::new(0),
            stream_requests: AtomicU64::new(0),
            card_requests: AtomicU64::new(0),
            tasks: AtomicU64::new(0),
            upstream_errors: AtomicU64::new(0),
            auth_rejected: AtomicU64::new(0),
            approvals_allowed: AtomicU64::new(0),
            approvals_refused: AtomicU64::new(0),
            hooks_forwarded: AtomicU64::new(0),
            hooks_refused: AtomicU64::new(0),
            hooks_unreachable: AtomicU64::new(0),
            oidc_allow: AtomicU64::new(0),
            oidc_deny: AtomicU64::new(0),
            trusted_proxy_accepted: AtomicU64::new(0),
            trusted_proxy_rejected: AtomicU64::new(0),
        }
    }

    /// An A2A JSON-RPC request was received.
    pub fn inc_rpc(&self) {
        self.rpc_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// A `message/stream` request entered the SSE passthrough.
    pub fn inc_stream(&self) {
        self.stream_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// An Agent / fleet card projection was served.
    pub fn inc_card(&self) {
        self.card_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// A task was persisted to the durable store.
    pub fn inc_task(&self) {
        self.tasks.fetch_add(1, Ordering::Relaxed);
    }

    /// An upstream hop to an agent pod's mTLS `/mcp` failed.
    pub fn inc_upstream_error(&self) {
        self.upstream_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// An owner-approval outcome on a destructive request (granted or refused).
    pub fn inc_approval(&self, allowed: bool) {
        if allowed {
            self.approvals_allowed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.approvals_refused.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn inc_auth_rejected(&self) {
        self.auth_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// A hooks delivery was forwarded to the agent's webhook listener.
    pub fn inc_hooks_forwarded(&self) {
        self.hooks_forwarded.fetch_add(1, Ordering::Relaxed);
    }

    /// A hooks delivery was refused at a gateway gate.
    pub fn inc_hooks_refused(&self) {
        self.hooks_refused.fetch_add(1, Ordering::Relaxed);
    }

    /// A hooks delivery found no reachable replica (503 + Retry-After).
    pub fn inc_hooks_unreachable(&self) {
        self.hooks_unreachable.fetch_add(1, Ordering::Relaxed);
    }

    /// A per-agent OIDC request was allowed (valid JWT + satisfied claims).
    pub fn inc_oidc_allow(&self) {
        self.oidc_allow.fetch_add(1, Ordering::Relaxed);
    }

    /// A per-agent OIDC request was denied (authN 401 or authZ 403).
    pub fn inc_oidc_deny(&self) {
        self.oidc_deny.fetch_add(1, Ordering::Relaxed);
    }

    /// A trusted-proxy request was accepted on the verified mTLS listener.
    pub fn inc_trusted_proxy_accepted(&self) {
        self.trusted_proxy_accepted.fetch_add(1, Ordering::Relaxed);
    }

    /// A trusted-proxy request was rejected (name not allow-listed, or
    /// requiredClaims unsatisfied) → 403.
    pub fn inc_trusted_proxy_rejected(&self) {
        self.trusted_proxy_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the Prometheus exposition body.
    pub fn render(&self) -> String {
        let mut out = String::new();
        gauge(
            &mut out,
            "process_start_time_seconds",
            "Start time of the process since unix epoch in seconds.",
            self.start_time_secs,
        );
        counter(
            &mut out,
            "agentctl_gateway_rpc_requests_total",
            "A2A JSON-RPC requests received.",
            self.rpc_requests.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_stream_requests_total",
            "message/stream requests routed down the SSE passthrough.",
            self.stream_requests.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_card_requests_total",
            "Agent/fleet card projections served.",
            self.card_requests.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_tasks_total",
            "Tasks persisted to the durable store.",
            self.tasks.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_upstream_errors_total",
            "Requests that failed at the agent/upstream hop.",
            self.upstream_errors.load(Ordering::Relaxed),
        );
        // Owner-approval outcomes (P4-5 gates; labeled, so hand-rendered).
        out.push_str(
            "# HELP agentctl_gateway_approvals_total Owner-approval outcomes on destructive requests.\n# TYPE agentctl_gateway_approvals_total counter\n",
        );
        out.push_str(&format!(
            "agentctl_gateway_approvals_total{{outcome=\"allowed\"}} {}\n",
            self.approvals_allowed.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "agentctl_gateway_approvals_total{{outcome=\"refused\"}} {}\n",
            self.approvals_refused.load(Ordering::Relaxed)
        ));
        counter(
            &mut out,
            "agentctl_gateway_auth_rejected_total",
            "Requests rejected (401) by the bearer-token access gate.",
            self.auth_rejected.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_hooks_forwarded_total",
            "Hooks-ingress deliveries forwarded to agent webhook listeners.",
            self.hooks_forwarded.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_hooks_refused_total",
            "Hooks-ingress deliveries refused at a gateway gate (exposure/method/rate/size).",
            self.hooks_refused.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_hooks_unreachable_total",
            "Hooks-ingress deliveries that found no reachable replica.",
            self.hooks_unreachable.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_oidc_allow_total",
            "Per-agent OIDC requests allowed (valid JWT + satisfied claims).",
            self.oidc_allow.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_oidc_deny_total",
            "Per-agent OIDC requests denied (authN 401 or authZ 403).",
            self.oidc_deny.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_trusted_proxy_accepted_total",
            "Trusted-proxy requests accepted on the verified mTLS listener.",
            self.trusted_proxy_accepted.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_gateway_trusted_proxy_rejected_total",
            "Trusted-proxy requests rejected (name not allow-listed or requiredClaims unsatisfied).",
            self.trusted_proxy_rejected.load(Ordering::Relaxed),
        );
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Seconds since the unix epoch, now (0.0 if the clock is before the epoch).
fn unix_now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Emit one `counter` metric (HELP + TYPE + sample).
fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
    ));
}

/// Emit one `gauge` metric (HELP + TYPE + sample).
fn gauge(out: &mut String, name: &str, help: &str, value: f64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_reflects_recorded_counters() {
        let m = Metrics::new();
        m.inc_rpc();
        m.inc_rpc();
        m.inc_stream();
        m.inc_card();
        m.inc_task();
        m.inc_upstream_error();
        m.inc_approval(true);
        m.inc_approval(false);
        m.inc_approval(false);
        let body = m.render();
        assert!(body.contains("# TYPE agentctl_gateway_rpc_requests_total counter"));
        assert!(body.contains("agentctl_gateway_rpc_requests_total 2"));
        assert!(body.contains("agentctl_gateway_stream_requests_total 1"));
        assert!(body.contains("agentctl_gateway_card_requests_total 1"));
        assert!(body.contains("agentctl_gateway_tasks_total 1"));
        assert!(body.contains("agentctl_gateway_upstream_errors_total 1"));
        assert!(body.contains("# TYPE agentctl_gateway_approvals_total counter"));
        assert!(body.contains("agentctl_gateway_approvals_total{outcome=\"allowed\"} 1"));
        assert!(body.contains("agentctl_gateway_approvals_total{outcome=\"refused\"} 2"));
        assert!(body.contains("# TYPE process_start_time_seconds gauge"));
    }
}
