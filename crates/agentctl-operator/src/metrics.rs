// SPDX-License-Identifier: BUSL-1.1
//! Operator self-observability: reconcile counters, a reconcile-duration
//! histogram, and a leadership gauge, rendered as a Prometheus text exposition.
//!
//! This mirrors the node-agent's metrics approach (RFC 0010): no external
//! metrics framework, just a small hand-rolled exposition served as
//! `text/plain; version=0.0.4`. The counters live in lock-free atomics so the
//! reconcile hot path and the `/metrics` scrape never contend.
//!
//! All series carry the `agentctl_operator_` prefix (the node-agent uses
//! `agentctl_node_agent_`), so a single Prometheus job can scrape every control
//! plane component without name collisions.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Upper bounds (seconds) for the reconcile-duration histogram. The Prometheus
/// default ladder, which spans the sub-millisecond apply through the multi-second
/// apiserver-stall tail a reconcile can hit.
const DURATION_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Shared, lock-free operator metrics. Cloned behind an [`std::sync::Arc`] into
/// the reconcile [`crate::controller::Ctx`], the leader-election loop, and the
/// health/metrics HTTP server.
pub struct Metrics {
    /// Total reconcile invocations (both `Agent` and `AgentFleet`).
    reconcile_total: AtomicU64,
    /// Reconcile invocations that returned an error.
    reconcile_errors: AtomicU64,
    /// Cumulative per-bucket counts: `bucket[i]` is the number of observations
    /// whose duration was `<= DURATION_BUCKETS[i]` seconds.
    duration_buckets: [AtomicU64; DURATION_BUCKETS.len()],
    /// Total observations (the histogram's `+Inf` bucket and `_count`).
    duration_count: AtomicU64,
    /// Sum of observed durations, in microseconds (rendered as `_sum` seconds).
    duration_sum_micros: AtomicU64,
    /// 1 when this replica currently holds the leader lease, else 0.
    leader: AtomicBool,
    /// True once the controller manager has started reconciling.
    manager_up: AtomicBool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// A fresh, zeroed metrics registry.
    pub fn new() -> Self {
        Self {
            reconcile_total: AtomicU64::new(0),
            reconcile_errors: AtomicU64::new(0),
            duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            duration_count: AtomicU64::new(0),
            duration_sum_micros: AtomicU64::new(0),
            leader: AtomicBool::new(false),
            manager_up: AtomicBool::new(false),
        }
    }

    /// Record one completed reconcile: bump the total (and the error counter when
    /// `is_err`), and observe its wall-clock `elapsed` into the duration histogram.
    pub fn record_reconcile(&self, elapsed: Duration, is_err: bool) {
        self.reconcile_total.fetch_add(1, Ordering::Relaxed);
        if is_err {
            self.reconcile_errors.fetch_add(1, Ordering::Relaxed);
        }
        self.observe_duration(elapsed);
    }

    /// Observe one duration into the histogram (cumulative buckets + sum + count).
    fn observe_duration(&self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        for (i, bound) in DURATION_BUCKETS.iter().enumerate() {
            if secs <= *bound {
                self.duration_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.duration_count.fetch_add(1, Ordering::Relaxed);
        self.duration_sum_micros
            .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    /// Set the leadership gauge (1 = this replica is the elected leader).
    pub fn set_leader(&self, leader: bool) {
        self.leader.store(leader, Ordering::Relaxed);
    }

    /// Whether this replica currently holds leadership.
    pub fn is_leader(&self) -> bool {
        self.leader.load(Ordering::Relaxed)
    }

    /// Mark the controller manager as started (gates `/readyz`).
    pub fn set_manager_up(&self, up: bool) {
        self.manager_up.store(up, Ordering::Relaxed);
    }

    /// Readiness: the manager is running AND this replica holds leadership. A
    /// standby (non-leader) replica is deliberately *not* ready, so a `Service`
    /// fronting the operator only routes to the active leader.
    pub fn is_ready(&self) -> bool {
        self.manager_up.load(Ordering::Relaxed) && self.is_leader()
    }

    /// Render the current state as a Prometheus text exposition
    /// (`text/plain; version=0.0.4`).
    pub fn render(&self) -> String {
        let total = self.reconcile_total.load(Ordering::Relaxed);
        let errors = self.reconcile_errors.load(Ordering::Relaxed);
        let count = self.duration_count.load(Ordering::Relaxed);
        let sum_secs = self.duration_sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let leader = u8::from(self.is_leader());

        let mut out = String::new();

        out.push_str("# HELP agentctl_operator_reconcile_total Total reconcile invocations.\n");
        out.push_str("# TYPE agentctl_operator_reconcile_total counter\n");
        out.push_str(&format!("agentctl_operator_reconcile_total {total}\n"));

        out.push_str(
            "# HELP agentctl_operator_reconcile_errors_total Reconcile invocations that errored.\n",
        );
        out.push_str("# TYPE agentctl_operator_reconcile_errors_total counter\n");
        out.push_str(&format!(
            "agentctl_operator_reconcile_errors_total {errors}\n"
        ));

        out.push_str(
            "# HELP agentctl_operator_reconcile_duration_seconds Reconcile wall-clock duration.\n",
        );
        out.push_str("# TYPE agentctl_operator_reconcile_duration_seconds histogram\n");
        for (i, bound) in DURATION_BUCKETS.iter().enumerate() {
            let c = self.duration_buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "agentctl_operator_reconcile_duration_seconds_bucket{{le=\"{bound}\"}} {c}\n"
            ));
        }
        out.push_str(&format!(
            "agentctl_operator_reconcile_duration_seconds_bucket{{le=\"+Inf\"}} {count}\n"
        ));
        out.push_str(&format!(
            "agentctl_operator_reconcile_duration_seconds_sum {sum_secs}\n"
        ));
        out.push_str(&format!(
            "agentctl_operator_reconcile_duration_seconds_count {count}\n"
        ));

        out.push_str(
            "# HELP agentctl_operator_leader 1 if this replica holds the leader lease, else 0.\n",
        );
        out.push_str("# TYPE agentctl_operator_leader gauge\n");
        out.push_str(&format!("agentctl_operator_leader {leader}\n"));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_gauge_start_zero() {
        let m = Metrics::new();
        let out = m.render();
        assert!(out.contains("agentctl_operator_reconcile_total 0"));
        assert!(out.contains("agentctl_operator_reconcile_errors_total 0"));
        assert!(out.contains("agentctl_operator_leader 0"));
        // exposition shape: HELP + TYPE present for every series.
        assert_eq!(out.matches("# TYPE ").count(), 4);
    }

    #[test]
    fn record_reconcile_bumps_total_errors_and_histogram() {
        let m = Metrics::new();
        m.record_reconcile(Duration::from_millis(3), false); // <= 0.005 bucket
        m.record_reconcile(Duration::from_millis(300), true); // error, ~0.3s
        let out = m.render();

        assert!(out.contains("agentctl_operator_reconcile_total 2"));
        assert!(out.contains("agentctl_operator_reconcile_errors_total 1"));
        // count == 2 (the +Inf bucket and _count line).
        assert!(out.contains("agentctl_operator_reconcile_duration_seconds_count 2"));
        assert!(out.contains("agentctl_operator_reconcile_duration_seconds_bucket{le=\"+Inf\"} 2"));
        // the 3ms sample lands in the smallest (le="0.005") bucket; the 300ms one
        // does not — so that bucket's cumulative count is 1.
        assert!(out.contains("agentctl_operator_reconcile_duration_seconds_bucket{le=\"0.005\"} 1"));
        // le="0.5" is cumulative over both samples → 2.
        assert!(out.contains("agentctl_operator_reconcile_duration_seconds_bucket{le=\"0.5\"} 2"));
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_monotonic() {
        let m = Metrics::new();
        for ms in [1u64, 20, 200, 2000] {
            m.observe_duration(Duration::from_millis(ms));
        }
        let counts: Vec<u64> = (0..DURATION_BUCKETS.len())
            .map(|i| m.duration_buckets[i].load(Ordering::Relaxed))
            .collect();
        // cumulative histograms are non-decreasing across ascending bounds.
        assert!(counts.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(m.duration_count.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn leader_gauge_and_readiness_track_state() {
        let m = Metrics::new();
        assert!(!m.is_ready()); // neither up nor leader
        m.set_manager_up(true);
        assert!(!m.is_ready()); // up but not leader → still not ready
        m.set_leader(true);
        assert!(m.is_ready()); // up AND leader → ready
        assert!(m.render().contains("agentctl_operator_leader 1"));
        m.set_leader(false);
        assert!(!m.is_ready());
    }
}
