// SPDX-License-Identifier: BUSL-1.1
//! Frozen-contract assertion oracles (ACC 2).
//!
//! These back the conformance scenarios and now delegate to
//! [`agent_contract_client`]'s **vendored** tables — the same compiled-in
//! baseline the operator renders `podFailurePolicy` from — so the e2e suite
//! and the control plane can never disagree about the contract. (The ACC 1.x
//! version loaded these from `contract/schemas/` at run time; the vendored
//! copies ARE those files, compiled in.)

use anyhow::{bail, Context, Result};

use agent_contract_client::{exit_codes, metrics, parse_manifest, Manifest};

/// The exit-code oracle: a Job's terminal exit must be a known code with the
/// expected `podFailurePolicy` intent.
pub struct ExitCodeTable {
    inner: exit_codes::Table,
}

impl ExitCodeTable {
    /// The vendored table (`exit_codes 1.0`).
    pub fn vendored() -> Self {
        Self {
            inner: exit_codes::Table::vendored(),
        }
    }

    pub fn version(&self) -> String {
        self.inner.version.to_string()
    }

    pub fn is_known(&self, code: i32) -> bool {
        self.inner.is_known(code)
    }

    pub fn name(&self, code: i32) -> Option<&str> {
        self.inner.name(code)
    }

    /// The frozen intent as a string (`complete`/`terminal`/`retriable`/
    /// `policy`/`infra`); unknown codes are `retriable` per the table's rule.
    pub fn intent(&self, code: i32) -> &'static str {
        match self.inner.intent(code) {
            exit_codes::Intent::Complete => "complete",
            exit_codes::Intent::Terminal => "terminal",
            exit_codes::Intent::Retriable => "retriable",
            exit_codes::Intent::Policy => "policy",
            exit_codes::Intent::Infra => "infra",
        }
    }
}

/// The metrics oracle: every `agent_*` series an agent emits must be a
/// registered name (schema 1.2), and reserved names must not be treated live.
pub struct MetricsRegistry {
    inner: metrics::Registry,
}

impl MetricsRegistry {
    /// The vendored registry (`metrics_schema 1.2`).
    pub fn vendored() -> Self {
        Self {
            inner: metrics::Registry::vendored(),
        }
    }

    pub fn version(&self) -> String {
        self.inner.version.to_string()
    }

    pub fn is_registered(&self, name: &str) -> bool {
        self.inner.is_registered(name)
    }

    /// Emitted-but-unregistered `agent_*` names — the drift findings. Names
    /// outside the `agent_` prefix are not ours to police.
    pub fn unregistered<'a>(&self, emitted: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        emitted
            .into_iter()
            .filter(|n| n.starts_with("agent_") && !self.inner.is_registered(n))
            .map(str::to_string)
            .collect()
    }
}

/// Parse + gate a live `--capabilities` manifest: it must identify as the
/// runtime generation this control plane manages (`runtime: "1"`). Everything
/// else in the manifest is informational at ACC 2 (`contract/SPEC.md` §3).
pub fn validate_manifest(json: &str) -> Result<Manifest> {
    let m = parse_manifest(json).context("capabilities manifest did not parse")?;
    if !m.is_runtime_1() {
        bail!(
            "agent did not identify as runtime generation 1 (a pre-rewrite agent?): \
             runtime={:?} version={:?}",
            m.runtime,
            m.version
        );
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_exit_codes_carry_the_frozen_intents() {
        let t = ExitCodeTable::vendored();
        assert_eq!(t.version(), "1.0");
        assert!(t.is_known(0) && t.is_known(7) && t.is_known(137));
        assert_eq!(t.intent(0), "complete");
        assert_eq!(t.intent(2), "terminal");
        assert_eq!(t.intent(7), "policy");
        assert_eq!(t.intent(99), "retriable");
        assert_eq!(t.name(5), Some("REFUSED"));
    }

    #[test]
    fn vendored_metrics_registry_flags_drift_only_in_our_prefix() {
        let r = MetricsRegistry::vendored();
        assert_eq!(r.version(), "1.2");
        assert!(r.is_registered("agent_inbox_pending"));
        let findings = r.unregistered(vec![
            "agent_inbox_pending",
            "agent_made_up_total",
            "process_cpu_seconds_total",
        ]);
        assert_eq!(findings, vec!["agent_made_up_total".to_string()]);
    }

    #[test]
    fn manifest_gate_requires_runtime_1() {
        let ok = r#"{"runtime":"1","version":"1.3.1"}"#;
        assert!(validate_manifest(ok).is_ok());
        let pre = r#"{"contract_version":"1.0","surfaces":{}}"#;
        assert!(validate_manifest(pre).is_err());
    }
}
