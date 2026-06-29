// SPDX-License-Identifier: BUSL-1.1
//! Prometheus `/metrics` exposition for the external scaler.
//!
//! Hand-rolled in the node-agent / coordination style (agentctl RFC 0010): no
//! client library, body is `text/plain; version=0.0.4`, each metric emits its
//! `# HELP`/`# TYPE` once then the sample. `stats_reads_total` /
//! `stats_errors_total` are lifecycle counters; `last_backlog` is a gauge holding
//! the most recent `pending` the scaler observed from the coordination server.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lifecycle counters + last-backlog gauge for the scaler.
#[derive(Debug)]
pub struct Metrics {
    /// Process start time (unix epoch seconds) — the standard `process_*` gauge.
    start_time_secs: f64,
    /// Successful `work.stats` reads from the coordination server.
    stats_reads: AtomicU64,
    /// Failed `work.stats` reads (transport/decode/missing field). On these the
    /// scaler serves the LAST known IsActive value, never flapping to 0.
    stats_errors: AtomicU64,
    /// The most recent `pending` count observed (the off-pod backlog, P9).
    last_backlog: AtomicI64,
}

impl Metrics {
    /// Construct with the process start time captured now.
    pub fn new() -> Self {
        Self {
            start_time_secs: unix_now_secs(),
            stats_reads: AtomicU64::new(0),
            stats_errors: AtomicU64::new(0),
            last_backlog: AtomicI64::new(0),
        }
    }

    /// A `work.stats` read succeeded.
    pub fn inc_read(&self) {
        self.stats_reads.fetch_add(1, Ordering::Relaxed);
    }

    /// A `work.stats` read failed (the scaler fell back to the last known value).
    pub fn inc_error(&self) {
        self.stats_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record the latest observed backlog (pending count).
    pub fn set_backlog(&self, pending: i64) {
        self.last_backlog.store(pending, Ordering::Relaxed);
    }

    /// The last observed backlog (0 if none read yet) — the `GetMetrics` fallback
    /// on a coordination read failure.
    pub fn last_backlog(&self) -> i64 {
        self.last_backlog.load(Ordering::Relaxed)
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
            "agentctl_scaler_stats_reads_total",
            "Successful work.stats reads from the coordination server.",
            self.stats_reads.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_scaler_stats_errors_total",
            "Failed work.stats reads; the scaler served the last known IsActive value.",
            self.stats_errors.load(Ordering::Relaxed),
        );
        gauge(
            &mut out,
            "agentctl_scaler_last_backlog",
            "Most recent pending backlog count observed (the off-pod scale-from-zero signal, P9).",
            self.last_backlog.load(Ordering::Relaxed) as f64,
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
    fn render_reflects_counters_and_backlog_gauge() {
        let m = Metrics::new();
        m.inc_read();
        m.inc_read();
        m.inc_error();
        m.set_backlog(7);
        let body = m.render();

        assert!(body.contains("# TYPE agentctl_scaler_stats_reads_total counter"));
        assert!(body.contains("agentctl_scaler_stats_reads_total 2"));
        assert!(body.contains("agentctl_scaler_stats_errors_total 1"));
        assert!(body.contains("# TYPE agentctl_scaler_last_backlog gauge"));
        assert!(body.contains("agentctl_scaler_last_backlog 7"));
        assert!(body.contains("# TYPE process_start_time_seconds gauge"));
    }
}
