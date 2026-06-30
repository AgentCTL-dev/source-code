// SPDX-License-Identifier: BUSL-1.1
//! Prometheus `/metrics` exposition for the ModelGateway.
//!
//! Hand-rolled in the node-agent's style (RFC 0010): no client library, the body
//! is `text/plain; version=0.0.4`, each metric emits its `# HELP`/`# TYPE` once
//! followed by the sample. Counters live behind atomics in the shared app state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process + proxy + token counters for the ModelGateway.
#[derive(Debug)]
pub struct Metrics {
    /// Process start time (unix epoch seconds) — the standard `process_*` gauge.
    start_time_secs: f64,
    /// `/v1/infer` requests received.
    infer_requests: AtomicU64,
    /// Inference requests that failed at the provider hop (proxy errors).
    infer_errors: AtomicU64,
    /// Requests rejected pre-flight because the pool budget was exhausted.
    budget_rejections: AtomicU64,
    /// Total provider tokens metered through the gateway.
    tokens: AtomicU64,
    /// Requests rejected (401) by the bearer-token access gate.
    auth_rejected: AtomicU64,
    /// Requests whose identity was attested from the source IP (RFC 0015).
    identity_attested: AtomicU64,
    /// Requests where the `X-Agent-Namespace` header disagreed with the
    /// attested namespace (a spoof attempt; the attested one is used).
    identity_spoof: AtomicU64,
    /// Requests whose identity was attested via the trusted node-agent forwarder
    /// (the real caller asserted in `X-Agent-Pod-Uid`).
    identity_forwarded: AtomicU64,
}

impl Metrics {
    /// Construct with the process start time captured now.
    pub fn new() -> Self {
        Self {
            start_time_secs: unix_now_secs(),
            infer_requests: AtomicU64::new(0),
            infer_errors: AtomicU64::new(0),
            budget_rejections: AtomicU64::new(0),
            tokens: AtomicU64::new(0),
            auth_rejected: AtomicU64::new(0),
            identity_attested: AtomicU64::new(0),
            identity_spoof: AtomicU64::new(0),
            identity_forwarded: AtomicU64::new(0),
        }
    }

    /// An `/v1/infer` request was accepted for processing.
    pub fn inc_request(&self) {
        self.infer_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// The provider hop failed (connect/non-2xx/decode).
    pub fn inc_error(&self) {
        self.infer_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// A request was rejected because the pool budget was exhausted.
    pub fn inc_budget_rejection(&self) {
        self.budget_rejections.fetch_add(1, Ordering::Relaxed);
    }

    /// A request was rejected (401) by the bearer-token access gate.
    pub fn inc_auth_rejected(&self) {
        self.auth_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// A request's identity was attested from its source IP.
    pub fn inc_identity_attested(&self) {
        self.identity_attested.fetch_add(1, Ordering::Relaxed);
    }

    /// A request's `X-Agent-Namespace` header disagreed with the attested
    /// namespace (a spoof attempt; the attested namespace is used regardless).
    pub fn inc_identity_spoof(&self) {
        self.identity_spoof.fetch_add(1, Ordering::Relaxed);
    }

    /// A request's identity was attested via the trusted node-agent forwarder
    /// (the real caller resolved from `X-Agent-Pod-Uid`).
    pub fn inc_identity_forwarded(&self) {
        self.identity_forwarded.fetch_add(1, Ordering::Relaxed);
    }

    /// Meter `tokens` provider tokens (negative/absent counts are clamped to 0).
    pub fn add_tokens(&self, tokens: i64) {
        self.tokens
            .fetch_add(tokens.max(0) as u64, Ordering::Relaxed);
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
            "agentctl_modelgateway_infer_requests_total",
            "Inference requests received on /v1/infer.",
            self.infer_requests.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_modelgateway_infer_errors_total",
            "Inference requests that failed at the provider hop.",
            self.infer_errors.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_modelgateway_budget_rejections_total",
            "Inference requests rejected because the pool budget was exhausted.",
            self.budget_rejections.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_modelgateway_tokens_total",
            "Total provider tokens metered through the gateway.",
            self.tokens.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_modelgateway_auth_rejected_total",
            "Requests rejected (401) by the bearer-token access gate.",
            self.auth_rejected.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_modelgateway_identity_attested_total",
            "Requests whose identity was attested from the source IP.",
            self.identity_attested.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_modelgateway_identity_spoof_total",
            "Requests where the X-Agent-Namespace header disagreed with the attested namespace.",
            self.identity_spoof.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_modelgateway_identity_forwarded_total",
            "Requests whose identity was attested via the trusted node-agent forwarder.",
            self.identity_forwarded.load(Ordering::Relaxed),
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

/// Emit one `counter` metric (HELP + TYPE + sample), node-agent style.
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
        m.inc_request();
        m.inc_request();
        m.inc_error();
        m.inc_budget_rejection();
        m.add_tokens(150);
        m.add_tokens(-5); // clamped to 0
        let body = m.render();
        assert!(body.contains("# TYPE agentctl_modelgateway_infer_requests_total counter"));
        assert!(body.contains("agentctl_modelgateway_infer_requests_total 2"));
        assert!(body.contains("agentctl_modelgateway_infer_errors_total 1"));
        assert!(body.contains("agentctl_modelgateway_budget_rejections_total 1"));
        assert!(body.contains("agentctl_modelgateway_tokens_total 150"));
        assert!(body.contains("# TYPE process_start_time_seconds gauge"));
    }

    #[test]
    fn render_reports_forwarded_identity_counter() {
        let m = Metrics::new();
        m.inc_identity_forwarded();
        m.inc_identity_forwarded();
        let body = m.render();
        assert!(body.contains("# TYPE agentctl_modelgateway_identity_forwarded_total counter"));
        assert!(body.contains("agentctl_modelgateway_identity_forwarded_total 2"));
    }
}
