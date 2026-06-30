// SPDX-License-Identifier: BUSL-1.1
//! Prometheus `/metrics` exposition for the coordination MCP server.
//!
//! Hand-rolled in the node-agent / gateway style (agentctl RFC 0010): no client
//! library, body is `text/plain; version=0.0.4`, each metric emits its
//! `# HELP`/`# TYPE` once then the sample. Counters live behind atomics; the
//! `pending`/`claimed` gauges are read live from the store at scrape time and
//! passed into [`Metrics::render`] (they are store truth, not a mirrored counter).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lifecycle counters for the coordination server.
#[derive(Debug)]
pub struct Metrics {
    /// Process start time (unix epoch seconds) — the standard `process_*` gauge.
    start_time_secs: f64,
    submitted: AtomicU64,
    claims_granted: AtomicU64,
    claims_contended: AtomicU64,
    claims_deduped: AtomicU64,
    renewed: AtomicU64,
    acked: AtomicU64,
    released: AtomicU64,
    expired: AtomicU64,
    auth_rejected: AtomicU64,
    attest_ok: AtomicU64,
    attest_reject: AtomicU64,
}

impl Metrics {
    /// Construct with the process start time captured now.
    pub fn new() -> Self {
        Self {
            start_time_secs: unix_now_secs(),
            submitted: AtomicU64::new(0),
            claims_granted: AtomicU64::new(0),
            claims_contended: AtomicU64::new(0),
            claims_deduped: AtomicU64::new(0),
            renewed: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            released: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            auth_rejected: AtomicU64::new(0),
            attest_ok: AtomicU64::new(0),
            attest_reject: AtomicU64::new(0),
        }
    }

    /// An item was enqueued via `work.submit`.
    pub fn inc_submitted(&self) {
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }
    /// A `work.claim` granted a lease.
    pub fn inc_claim_granted(&self) {
        self.claims_granted.fetch_add(1, Ordering::Relaxed);
    }
    /// A `work.claim` lost to a live holder.
    pub fn inc_claim_contended(&self) {
        self.claims_contended.fetch_add(1, Ordering::Relaxed);
    }
    /// A `work.claim` was deduped (key already acked).
    pub fn inc_claim_deduped(&self) {
        self.claims_deduped.fetch_add(1, Ordering::Relaxed);
    }
    /// A lease was renewed.
    pub fn inc_renewed(&self) {
        self.renewed.fetch_add(1, Ordering::Relaxed);
    }
    /// A lease was acked (terminal).
    pub fn inc_acked(&self) {
        self.acked.fetch_add(1, Ordering::Relaxed);
    }
    /// A lease was released back to pending.
    pub fn inc_released(&self) {
        self.released.fetch_add(1, Ordering::Relaxed);
    }
    /// `n` expired leases were swept back to pending.
    pub fn add_expired(&self, n: usize) {
        self.expired.fetch_add(n as u64, Ordering::Relaxed);
    }
    /// A request to a data endpoint was rejected (401) by the bearer-token gate.
    pub fn inc_auth_rejected(&self) {
        self.auth_rejected.fetch_add(1, Ordering::Relaxed);
    }
    /// A claim-lifecycle call's caller was successfully attested (source IP resolved
    /// to a pod identity) and allowed to proceed (RFC 0015 attested mode).
    pub fn inc_attest_ok(&self) {
        self.attest_ok.fetch_add(1, Ordering::Relaxed);
    }
    /// A claim-lifecycle call was rejected by attestation: the source IP could not
    /// be attested (fail closed) OR the attested caller is not the lease holder
    /// (a tenant tried to settle/steal another tenant's lease).
    pub fn inc_attest_reject(&self) {
        self.attest_reject.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the Prometheus exposition body. `pending`/`claimed` are the live
    /// store gauges read at scrape time (correct even at zero pods of the fleet).
    pub fn render(&self, pending: usize, claimed: usize) -> String {
        let mut out = String::new();
        gauge(
            &mut out,
            "process_start_time_seconds",
            "Start time of the process since unix epoch in seconds.",
            self.start_time_secs,
        );
        counter(
            &mut out,
            "agentctl_coordination_submitted_total",
            "Items enqueued via work.submit.",
            self.submitted.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_claims_granted_total",
            "work.claim calls that granted a lease.",
            self.claims_granted.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_claims_contended_total",
            "work.claim calls lost to a live holder.",
            self.claims_contended.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_claims_deduped_total",
            "work.claim calls deduped against an already-acked claim_key.",
            self.claims_deduped.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_renewed_total",
            "Leases renewed via work.renew.",
            self.renewed.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_acked_total",
            "Leases settled (terminal) via work.ack.",
            self.acked.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_released_total",
            "Leases returned to pending via work.release.",
            self.released.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_expired_total",
            "Leases swept back to pending after TTL expiry.",
            self.expired.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_auth_rejected_total",
            "Requests to data endpoints rejected (401) by the bearer-token gate.",
            self.auth_rejected.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_attest_ok_total",
            "Claim-lifecycle calls whose caller was attested and allowed (RFC 0015).",
            self.attest_ok.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "agentctl_coordination_attest_reject_total",
            "Claim-lifecycle calls rejected by attestation (unattestable caller or holder mismatch).",
            self.attest_reject.load(Ordering::Relaxed),
        );
        gauge(
            &mut out,
            "agentctl_coordination_pending",
            "Items enqueued and not yet claimed (the off-pod backlog, P9).",
            pending as f64,
        );
        gauge(
            &mut out,
            "agentctl_coordination_claimed",
            "Items currently held under a live lease.",
            claimed as f64,
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
    fn render_reflects_counters_and_gauges() {
        let m = Metrics::new();
        m.inc_submitted();
        m.inc_claim_granted();
        m.inc_claim_granted();
        m.inc_claim_contended();
        m.inc_claim_deduped();
        m.inc_renewed();
        m.inc_acked();
        m.inc_released();
        m.add_expired(3);
        m.inc_auth_rejected();
        m.inc_auth_rejected();
        m.inc_attest_ok();
        m.inc_attest_ok();
        m.inc_attest_ok();
        m.inc_attest_reject();
        let body = m.render(5, 2);

        assert!(body.contains("# TYPE agentctl_coordination_claims_granted_total counter"));
        assert!(body.contains("agentctl_coordination_submitted_total 1"));
        assert!(body.contains("agentctl_coordination_claims_granted_total 2"));
        assert!(body.contains("agentctl_coordination_claims_contended_total 1"));
        assert!(body.contains("agentctl_coordination_claims_deduped_total 1"));
        assert!(body.contains("agentctl_coordination_renewed_total 1"));
        assert!(body.contains("agentctl_coordination_acked_total 1"));
        assert!(body.contains("agentctl_coordination_released_total 1"));
        assert!(body.contains("agentctl_coordination_expired_total 3"));
        assert!(body.contains("# TYPE agentctl_coordination_auth_rejected_total counter"));
        assert!(body.contains("agentctl_coordination_auth_rejected_total 2"));
        assert!(body.contains("# TYPE agentctl_coordination_attest_ok_total counter"));
        assert!(body.contains("agentctl_coordination_attest_ok_total 3"));
        assert!(body.contains("# TYPE agentctl_coordination_attest_reject_total counter"));
        assert!(body.contains("agentctl_coordination_attest_reject_total 1"));
        assert!(body.contains("# TYPE agentctl_coordination_pending gauge"));
        assert!(body.contains("agentctl_coordination_pending 5"));
        assert!(body.contains("agentctl_coordination_claimed 2"));
        assert!(body.contains("# TYPE process_start_time_seconds gauge"));
    }
}
