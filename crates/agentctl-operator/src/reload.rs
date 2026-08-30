// SPDX-License-Identifier: BUSL-1.1
//! # Reload-vs-restart classification (RFC 0033 §3, P2-5)
//!
//! Classify a config diff against the vendored RESTART_ONLY partition (ACC 2)
//! plus the operator's own special cases, so every rollout DECISION is
//! explicit and observable. Delivery today is ALWAYS a drain-first rolling
//! restart: the projection hash rides the pod-template annotation, agentd
//! drains on SIGTERM (28 s worst case < the 30 s grace), and in-flight runs
//! finish under pinned definitions — zero run loss. The live-reload delivery
//! is gated on upstream **U1, verified RED on 2026-08-30**: an armed
//! `config.watch` does NOT fire on a real kubelet ConfigMap symlink swap
//! (repro shared upstream). Every rendered daemon already carries
//! `lifecycle.watch_config: true` (itself restart-only), so fleets are born
//! ready for the day the watcher fires — the classifier's verdict then flips
//! reload-safe diffs to in-place ConfigMap updates with NO pod roll.
//!
//! Special case on top of the vendored table: `a2a.principals` is NOT in
//! agentd's RESTART_ONLY_PATHS but is reload-HOSTILE anyway (Resolver::build
//! resolves `bearer_ref` once at startup; upstream-verified) — classified
//! restart-only here.

use agent_contract_client::restart_only;
use serde_json::Value;

/// The classification of one config change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// Every changed path (dot notation; list-valued keys as one unit —
    /// layers replace lists, so a list change is atomic).
    pub changed: Vec<String>,
    /// The subset that CANNOT hot-reload (vendored table + special cases).
    pub restart_only: Vec<String>,
}

impl Classification {
    /// True when every changed path could hot-reload (delivery still rolls
    /// until U1 is green upstream — see module docs).
    pub fn reload_safe(&self) -> bool {
        !self.changed.is_empty() && self.restart_only.is_empty()
    }

    /// One-line summary for the reconcile Event.
    pub fn summary(&self) -> String {
        if self.changed.is_empty() {
            return "no config change".into();
        }
        if self.reload_safe() {
            format!(
                "reload-safe change ({}) — delivered as a drain-first rolling restart until \
                 upstream config-watch fires on kubelet swaps (U1)",
                self.changed.join(", ")
            )
        } else {
            format!("restart-required change ({})", self.restart_only.join(", "))
        }
    }
}

/// Paths reload-hostile beyond the vendored table.
fn special_case_restart_only(path: &str) -> bool {
    path == "a2a.principals" || path.starts_with("a2a.principals.")
}

/// Classify the diff between two INSTANCE-layer documents (the services
/// catalog layer participates too — catalog entries feed live MCP sessions,
/// which re-handshake on reload, so catalog paths classify as `services.*`).
pub fn classify(old: &Value, new: &Value) -> Classification {
    let mut changed = Vec::new();
    diff_paths(old, new, "", &mut changed);
    let entries = restart_only::paths();
    let restart: Vec<String> = changed
        .iter()
        .filter(|p| restart_only::is_restart_only(p, &entries) || special_case_restart_only(p))
        .cloned()
        .collect();
    Classification {
        changed,
        restart_only: restart,
    }
}

/// Collect dot-paths where the two documents differ. Objects recurse; every
/// other type (scalars AND lists — lists replace across layers) is one unit.
fn diff_paths(old: &Value, new: &Value, prefix: &str, out: &mut Vec<String>) {
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for k in keys {
                let path = if prefix.is_empty() {
                    k.to_string()
                } else {
                    format!("{prefix}.{k}")
                };
                match (a.get(k), b.get(k)) {
                    (Some(x), Some(y)) => diff_paths(x, y, &path, out),
                    (None, Some(_)) | (Some(_), None) => out.push(path),
                    (None, None) => unreachable!(),
                }
            }
        }
        _ if old != new => out.push(prefix.to_string()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn instruction_change_is_reload_safe_but_still_rolls() {
        let old = json!({ "config_version": "1", "agent": { "instruction": "a" } });
        let new = json!({ "config_version": "1", "agent": { "instruction": "b" } });
        let c = classify(&old, &new);
        assert_eq!(c.changed, vec!["agent.instruction"]);
        assert!(c.reload_safe());
        assert!(c.summary().contains("U1"), "{}", c.summary());
    }

    #[test]
    fn restart_only_paths_are_named() {
        let old = json!({ "lifecycle": { "run_until": "drained", "drain_timeout": "25s" } });
        let new = json!({ "lifecycle": { "run_until": "drained", "drain_timeout": "20s" } });
        let c = classify(&old, &new);
        assert_eq!(c.restart_only, vec!["lifecycle.drain_timeout"]);
        assert!(!c.reload_safe());
    }

    #[test]
    fn principals_are_special_cased_restart_only() {
        // NOT in agentd's own RESTART_ONLY_PATHS — but Resolver::build
        // resolves bearer_refs once at startup (upstream-verified), so the
        // classifier must treat them as restart-only regardless.
        let old =
            json!({ "a2a": { "principals": [{ "match": { "any": true }, "role": "operator" }] } });
        let new = json!({ "a2a": { "principals": [] } });
        let c = classify(&old, &new);
        assert_eq!(c.restart_only, vec!["a2a.principals"]);
    }

    #[test]
    fn added_and_removed_keys_count_and_lists_are_atomic() {
        let old = json!({ "mcp": { "servers": [{ "name": "a", "service": "a" }] } });
        let new = json!({
            "mcp": { "servers": [{ "name": "a", "service": "a" }, { "name": "b", "service": "b" }] },
            "intelligence": { "endpoints": "https://x/v1" },
        });
        let c = classify(&old, &new);
        assert!(c.changed.contains(&"mcp.servers".to_string()));
        assert!(c.changed.contains(&"intelligence".to_string()));
        // Both reloadable (mcp re-handshakes, intelligence hot-swaps).
        assert!(c.reload_safe());
    }
}
