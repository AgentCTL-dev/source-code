// SPDX-License-Identifier: BUSL-1.1
//! Prometheus `/metrics` exposition for the aggregated apiserver.
//!
//! Hand-rolled: no client library, the body is
//! `text/plain; version=0.0.4`, each metric emits its `# HELP`/`# TYPE` once
//! followed by the sample. Counters live behind atomics in the shared app state.
//!
//! `/metrics` is served on the EXISTING `:6443` HTTPS surface (it does NOT open a
//! separate plaintext port), so it inherits the front-proxy mTLS requirement —
//! only a caller presenting a CA-signed client cert can scrape it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process + verb-forwarding counters for the aggregated apiserver.
#[derive(Debug)]
pub struct Metrics {
    /// Process start time (unix epoch seconds) — the standard `process_*` gauge.
    start_time_secs: f64,
    /// Connect-verb requests received (after the verb-name check).
    requests: AtomicU64,
    /// Verb requests authorized by the SubjectAccessReview.
    authorized: AtomicU64,
    /// Verb requests denied by the SubjectAccessReview.
    denied: AtomicU64,
    /// Verbs successfully forwarded to the agent pod (mTLS `POST /mcp` on :8443).
    forwarded: AtomicU64,
    /// Verb requests that errored (SAR failure or agent-pod forward failure).
    errors: AtomicU64,
}

impl Metrics {
    /// Construct with the process start time captured now.
    pub fn new() -> Self {
        Self {
            start_time_secs: unix_now_secs(),
            requests: AtomicU64::new(0),
            authorized: AtomicU64::new(0),
            denied: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    /// A connect-verb request was accepted for processing.
    pub fn inc_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// The SubjectAccessReview allowed the verb.
    pub fn inc_authorized(&self) {
        self.authorized.fetch_add(1, Ordering::Relaxed);
    }

    /// The SubjectAccessReview denied the verb.
    pub fn inc_denied(&self) {
        self.denied.fetch_add(1, Ordering::Relaxed);
    }

    /// The verb was forwarded to the agent pod.
    pub fn inc_forwarded(&self) {
        self.forwarded.fetch_add(1, Ordering::Relaxed);
    }

    /// The verb errored (SAR failure or agent-pod forward failure).
    pub fn inc_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
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
            "agentctl_apiserver_verb_requests_total",
            "Connect-verb requests received.",
            self.requests.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_apiserver_verb_authorized_total",
            "Connect-verb requests authorized by SubjectAccessReview.",
            self.authorized.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_apiserver_verb_denied_total",
            "Connect-verb requests denied by SubjectAccessReview.",
            self.denied.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_apiserver_verb_forwarded_total",
            "Connect-verbs forwarded to the agent pod.",
            self.forwarded.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_apiserver_verb_errors_total",
            "Connect-verb requests that errored (SAR or agent-pod forward).",
            self.errors.load(Ordering::Relaxed),
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
        m.inc_request();
        m.inc_request();
        m.inc_authorized();
        m.inc_forwarded();
        m.inc_denied();
        m.inc_error();
        let body = m.render();
        assert!(body.contains("# TYPE agentctl_apiserver_verb_requests_total counter"));
        assert!(body.contains("agentctl_apiserver_verb_requests_total 2"));
        assert!(body.contains("agentctl_apiserver_verb_authorized_total 1"));
        assert!(body.contains("agentctl_apiserver_verb_denied_total 1"));
        assert!(body.contains("agentctl_apiserver_verb_forwarded_total 1"));
        assert!(body.contains("agentctl_apiserver_verb_errors_total 1"));
        assert!(body.contains("# TYPE process_start_time_seconds gauge"));
    }
}
