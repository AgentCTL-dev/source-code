// SPDX-License-Identifier: BUSL-1.1
//! Prometheus `/metrics` exposition for the admission webhook.
//!
//! Hand-rolled in the node-agent's style (RFC 0010): no client library, the body
//! is `text/plain; version=0.0.4`, and each metric emits its `# HELP`/`# TYPE`
//! once followed by the sample. Counters live behind atomics in the shared app
//! state so handlers can bump them lock-free; [`Metrics::render`] snapshots them
//! on each scrape.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process + service counters for the admission webhook.
#[derive(Debug)]
pub struct Metrics {
    /// Process start time (unix epoch seconds) — the standard `process_*` gauge.
    start_time_secs: f64,
    /// AdmissionReviews handled.
    reviews: AtomicU64,
    /// Reviews that admitted (allowed).
    admit: AtomicU64,
    /// Reviews that were denied.
    deny: AtomicU64,
}

impl Metrics {
    /// Construct with the process start time captured now.
    pub fn new() -> Self {
        Self {
            start_time_secs: unix_now_secs(),
            reviews: AtomicU64::new(0),
            admit: AtomicU64::new(0),
            deny: AtomicU64::new(0),
        }
    }

    /// Record one AdmissionReview verdict (`allowed` ⇒ admit, else deny).
    pub fn record(&self, allowed: bool) {
        self.reviews.fetch_add(1, Ordering::Relaxed);
        let bucket = if allowed { &self.admit } else { &self.deny };
        bucket.fetch_add(1, Ordering::Relaxed);
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
            "agentctl_admission_reviews_total",
            "AdmissionReviews handled by the webhook.",
            self.reviews.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_admission_admit_total",
            "AdmissionReviews that admitted (allowed).",
            self.admit.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_admission_deny_total",
            "AdmissionReviews that were denied.",
            self.deny.load(Ordering::Relaxed),
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
    fn render_reflects_recorded_verdicts() {
        let m = Metrics::new();
        m.record(true);
        m.record(false);
        m.record(true);
        let body = m.render();
        assert!(body.contains("# TYPE agentctl_admission_reviews_total counter"));
        assert!(body.contains("agentctl_admission_reviews_total 3"));
        assert!(body.contains("agentctl_admission_admit_total 2"));
        assert!(body.contains("agentctl_admission_deny_total 1"));
        assert!(body.contains("# TYPE process_start_time_seconds gauge"));
    }
}
