// SPDX-License-Identifier: BUSL-1.1
//! Frozen-contract assertion oracles, loaded from `contract/schemas/*.json`.
//!
//! These back the conformance scenarios: the exit-code table (a Job's terminal exit
//! must be a known code with the expected intent), the metrics registry (every
//! `agent_*` series an agent emits must be a registered name), and the capabilities
//! manifest (a reactive agent's `agent://capabilities` must parse + negotiate via
//! [`agent_contract_client`] — the typed view of `manifest.schema.json`).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use agent_contract_client::{parse_manifest, Manifest};

/// The frozen exit-code table (`exit-codes.table.json`), indexed by code.
#[derive(Debug, Clone)]
pub struct ExitCodeTable {
    /// `exit_codes_version` (== `surfaces.exit_codes`).
    pub version: String,
    /// The raw `codes[]` entries.
    pub codes: Vec<ExitCode>,
}

/// One row of the exit-code table.
#[derive(Debug, Clone)]
pub struct ExitCode {
    /// The integer exit code (e.g. `0`, `7`, `137`).
    pub code: i64,
    /// The neutral name (e.g. `EXIT_OK`).
    pub name: String,
    /// The podFailurePolicy intent (`complete`/`terminal`/`retriable`/`policy`/`infra`).
    pub intent: String,
}

impl ExitCodeTable {
    /// Load + parse the exit-code table from `<dir>/exit-codes.table.json`.
    pub fn load(dir: &Path) -> Result<Self> {
        let v = read_json(&dir.join("exit-codes.table.json"))?;
        let version = v
            .get("exit_codes_version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let codes = v
            .get("codes")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        Some(ExitCode {
                            code: c.get("code")?.as_i64()?,
                            name: c.get("name")?.as_str()?.to_string(),
                            intent: c.get("intent")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ExitCodeTable { version, codes })
    }

    /// Look up a code's row.
    pub fn get(&self, code: i64) -> Option<&ExitCode> {
        self.codes.iter().find(|c| c.code == code)
    }

    /// Whether `code` is in the frozen table.
    pub fn is_known(&self, code: i64) -> bool {
        self.get(code).is_some()
    }

    /// The podFailurePolicy intent for `code` — an UNKNOWN code defaults to
    /// `retriable` (the contract rule: never a silent FailJob).
    pub fn intent(&self, code: i64) -> &str {
        self.get(code)
            .map(|c| c.intent.as_str())
            .unwrap_or("retriable")
    }
}

/// The metrics registry (`metrics.registry.json`): the set of registered neutral
/// `agent_*` series names, plus the schema version + neutral prefix.
#[derive(Debug, Clone)]
pub struct MetricsRegistry {
    /// `metrics_schema` (== `surfaces.metrics_schema`).
    pub version: String,
    /// The neutral metric-name prefix (`agent_`).
    pub prefix: String,
    /// Every registered metric name (the `metrics[].name` set).
    pub names: BTreeSet<String>,
}

impl MetricsRegistry {
    /// Load + parse the registry from `<dir>/metrics.registry.json`.
    pub fn load(dir: &Path) -> Result<Self> {
        let v = read_json(&dir.join("metrics.registry.json"))?;
        let version = v
            .get("metrics_schema")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let prefix = v
            .get("prefix")
            .and_then(|p| p.get("neutral"))
            .and_then(Value::as_str)
            .unwrap_or("agent_")
            .to_string();
        let names = v
            .get("metrics")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(MetricsRegistry {
            version,
            prefix,
            names,
        })
    }

    /// Whether `name` is a registered neutral series.
    pub fn is_registered(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Of `observed` series names, those carrying the neutral prefix that are NOT in
    /// the registry — the conformance violation set (additive minors tolerated, so
    /// only the prefixed-but-unknown names are a finding).
    pub fn unregistered<'a, I>(&self, observed: I) -> Vec<String>
    where
        I: IntoIterator<Item = &'a String>,
    {
        observed
            .into_iter()
            .filter(|n| n.starts_with(&self.prefix) && !self.is_registered(n))
            .cloned()
            .collect()
    }
}

/// Validate a capabilities manifest JSON against the contract: it must parse into the
/// typed [`Manifest`] (the load-bearing sum-type shapes of `manifest.schema.json`)
/// AND negotiate to the supported major. Returns the parsed manifest on success.
pub fn validate_manifest(json: &str) -> Result<Manifest> {
    let m = parse_manifest(json).context("parse capabilities manifest")?;
    m.negotiate().context("negotiate contract_version")?;
    Ok(m)
}

/// Read + parse a JSON file.
fn read_json(path: &Path) -> Result<Value> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {path:?}"))?;
    serde_json::from_str(&body).with_context(|| format!("parse {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_exit_code_defaults_retriable() {
        let t = ExitCodeTable {
            version: "1.0".into(),
            codes: vec![ExitCode {
                code: 0,
                name: "EXIT_OK".into(),
                intent: "complete".into(),
            }],
        };
        assert!(t.is_known(0));
        assert_eq!(t.intent(0), "complete");
        assert!(!t.is_known(99));
        assert_eq!(t.intent(99), "retriable");
    }

    #[test]
    fn registry_flags_only_prefixed_unknowns() {
        let mut names = BTreeSet::new();
        names.insert("agent_up".to_string());
        let reg = MetricsRegistry {
            version: "1.0".into(),
            prefix: "agent_".into(),
            names,
        };
        let observed = vec![
            "agent_up".to_string(),            // registered
            "agent_made_up_total".to_string(), // prefixed + unknown ⇒ finding
            "go_gc_seconds".to_string(),       // not prefixed ⇒ ignored
        ];
        assert_eq!(reg.unregistered(&observed), vec!["agent_made_up_total"]);
    }

    #[test]
    fn manifest_validation_round_trips() {
        // Contract 2.0: management is an mTLS https URL (vsock/unix retired), and
        // the typed client negotiates major 2.
        let json = r#"{
            "contract_version": "2.0",
            "surfaces": { "management": "https://0.0.0.0:8443", "metrics": false, "a2a": false }
        }"#;
        let m = validate_manifest(json).unwrap();
        assert_eq!(m.contract_version, "2.0");
    }
}
