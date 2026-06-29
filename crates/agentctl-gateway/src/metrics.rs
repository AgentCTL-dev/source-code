// SPDX-License-Identifier: BUSL-1.1
//! Prometheus `/metrics` exposition for the A2A gateway.
//!
//! Hand-rolled in the node-agent's style (RFC 0010): no client library, the body
//! is `text/plain; version=0.0.4`, each metric emits its `# HELP`/`# TYPE` once
//! followed by the sample. Counters live behind atomics in the shared app state.

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
    /// Requests that failed at the node-agent/upstream hop.
    upstream_errors: AtomicU64,
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

    /// An upstream (node-agent) hop failed.
    pub fn inc_upstream_error(&self) {
        self.upstream_errors.fetch_add(1, Ordering::Relaxed);
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
            "Requests that failed at the node-agent/upstream hop.",
            self.upstream_errors.load(Ordering::Relaxed),
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
        m.inc_rpc();
        m.inc_rpc();
        m.inc_stream();
        m.inc_card();
        m.inc_task();
        m.inc_upstream_error();
        let body = m.render();
        assert!(body.contains("# TYPE agentctl_gateway_rpc_requests_total counter"));
        assert!(body.contains("agentctl_gateway_rpc_requests_total 2"));
        assert!(body.contains("agentctl_gateway_stream_requests_total 1"));
        assert!(body.contains("agentctl_gateway_card_requests_total 1"));
        assert!(body.contains("agentctl_gateway_tasks_total 1"));
        assert!(body.contains("agentctl_gateway_upstream_errors_total 1"));
        assert!(body.contains("# TYPE process_start_time_seconds gauge"));
    }
}
