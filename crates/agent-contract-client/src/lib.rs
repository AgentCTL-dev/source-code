//! # agent-contract-client — typed access to the ACC 2 contract
//!
//! The August-2026 agent rewrite removed the ACC 1.x negotiation anchor: the
//! capabilities manifest no longer carries `contract_version` or a `surfaces{}`
//! block, and it under-reports several planes (see `contract/SPEC.md` §3).
//! ACC 2 therefore negotiates on **four independent clocks**, each owned by its
//! own artifact, and treats `--capabilities` as *informational*:
//!
//! 1. the config schema's `x-agentd-contract-version` (config-document contract),
//! 2. the workflow schema `$id` dialect (`workflow-3`),
//! 3. the exit-code table (`exit_codes`),
//! 4. the metrics registry (`metrics_schema`).
//!
//! The vendored copies under `contract/schemas/` are compiled in ([`clocks`],
//! [`exit_codes`], [`metrics`], [`restart_only`]) so every control-plane
//! component branches on one baseline; conformance re-captures them from the
//! pinned binary and diffs. Parsing stays additive-tolerant everywhere: no
//! `deny_unknown_fields`, `Option` for anything upstream might drop.

use serde::Deserialize;
use std::fmt;

/// A `major.minor` clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    /// Parse `"1.0"`-style strings.
    pub fn parse(s: &str) -> Option<Version> {
        let (maj, min) = s.split_once('.')?;
        Some(Version {
            major: maj.parse().ok()?,
            minor: min.parse().ok()?,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Contract skew: a clock the client cannot manage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkewError {
    /// A clock string was absent or unparsable.
    Malformed { clock: &'static str, found: String },
    /// A major this build does not understand.
    UnsupportedMajor {
        clock: &'static str,
        found: Version,
        supported: u32,
    },
}

impl fmt::Display for SkewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkewError::Malformed { clock, found } => {
                write!(f, "{clock}: malformed version {found:?} (want \"major.minor\")")
            }
            SkewError::UnsupportedMajor {
                clock,
                found,
                supported,
            } => write!(
                f,
                "{clock}: major {found} is not supported (this agentctl speaks major {supported}) — refuse to manage this agent version"
            ),
        }
    }
}

impl std::error::Error for SkewError {}

/// The four negotiation clocks (ACC 2, `contract/SPEC.md` §3).
pub mod clocks {
    use super::{SkewError, Version};

    /// `x-agentd-contract-version` major this build renders documents for.
    pub const CONFIG_CONTRACT_MAJOR: u32 = 1;
    /// The workflow dialect this build authors/validates.
    pub const WORKFLOW_DIALECT: u32 = 3;
    /// The exit-code table major compiled into `podFailurePolicy` rules.
    pub const EXIT_CODES_MAJOR: u32 = 1;
    /// The metrics schema major dashboards/scalers are built against.
    pub const METRICS_SCHEMA_MAJOR: u32 = 1;

    /// Read + gate the config-contract clock from a served/vendored config
    /// schema document (the JSON of `agentd --config-schema`).
    pub fn negotiate_config_schema(schema: &serde_json::Value) -> Result<Version, SkewError> {
        let found = schema
            .get("x-agentd-contract-version")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let v = Version::parse(found).ok_or(SkewError::Malformed {
            clock: "x-agentd-contract-version",
            found: found.to_string(),
        })?;
        if v.major != CONFIG_CONTRACT_MAJOR {
            return Err(SkewError::UnsupportedMajor {
                clock: "x-agentd-contract-version",
                found: v,
                supported: CONFIG_CONTRACT_MAJOR,
            });
        }
        Ok(v)
    }

    /// Gate the workflow dialect from a workflow schema `$id`
    /// (`https://agentd.dev/schema/workflow-<dialect>.json`).
    pub fn negotiate_workflow_schema(schema: &serde_json::Value) -> Result<u32, SkewError> {
        let id = schema
            .get("$id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let dialect = id
            .rsplit_once("workflow-")
            .and_then(|(_, tail)| tail.strip_suffix(".json"))
            .and_then(|d| d.parse::<u32>().ok())
            .ok_or(SkewError::Malformed {
                clock: "workflow-schema $id",
                found: id.to_string(),
            })?;
        if dialect != WORKFLOW_DIALECT {
            return Err(SkewError::UnsupportedMajor {
                clock: "workflow-dialect",
                found: Version {
                    major: dialect,
                    minor: 0,
                },
                supported: WORKFLOW_DIALECT,
            });
        }
        Ok(dialect)
    }

    /// The vendored config schema (captured from the pinned binary).
    pub fn vendored_config_schema() -> serde_json::Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contract/schemas/config.schema.json"
        )))
        .expect("vendored config schema parses")
    }

    /// The vendored workflow schema (captured from the pinned binary).
    pub fn vendored_workflow_schema() -> serde_json::Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contract/schemas/workflow.schema.json"
        )))
        .expect("vendored workflow schema parses")
    }
}

/// The exit-code table + `podFailurePolicy` intents (vendored, `exit_codes 1.0`).
pub mod exit_codes {
    use super::Version;
    use serde::Deserialize;

    /// The five intents a control plane compiles exit codes into.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Intent {
        Complete,
        Terminal,
        Retriable,
        Policy,
        Infra,
    }

    impl Intent {
        fn parse(s: &str) -> Option<Intent> {
            Some(match s {
                "complete" => Intent::Complete,
                "terminal" => Intent::Terminal,
                "retriable" => Intent::Retriable,
                "policy" => Intent::Policy,
                "infra" => Intent::Infra,
                _ => return None,
            })
        }
    }

    #[derive(Debug, Deserialize)]
    struct RawCode {
        code: i32,
        name: String,
        intent: String,
    }

    #[derive(Debug, Deserialize)]
    struct RawTable {
        exit_codes: String,
        codes: Vec<RawCode>,
    }

    /// The parsed table.
    #[derive(Debug)]
    pub struct Table {
        pub version: Version,
        codes: Vec<(i32, String, Intent)>,
    }

    impl Table {
        /// The vendored table, compiled in. Panics only if the vendored file is
        /// corrupt (a build-time defect, caught by tests).
        pub fn vendored() -> Table {
            let raw: RawTable = serde_json::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../contract/schemas/exit-codes.table.json"
            )))
            .expect("vendored exit-code table parses");
            Table {
                version: Version::parse(&raw.exit_codes).expect("exit_codes version"),
                codes: raw
                    .codes
                    .into_iter()
                    .map(|c| {
                        let i = Intent::parse(&c.intent)
                            .unwrap_or_else(|| panic!("unknown intent {:?}", c.intent));
                        (c.code, c.name, i)
                    })
                    .collect(),
            }
        }

        pub fn is_known(&self, code: i32) -> bool {
            self.codes.iter().any(|(c, _, _)| *c == code)
        }

        pub fn name(&self, code: i32) -> Option<&str> {
            self.codes
                .iter()
                .find(|(c, _, _)| *c == code)
                .map(|(_, n, _)| n.as_str())
        }

        /// The frozen mapping; an unknown code is `retriable` (the table's own
        /// conservative rule).
        pub fn intent(&self, code: i32) -> Intent {
            self.codes
                .iter()
                .find(|(c, _, _)| *c == code)
                .map(|(_, _, i)| *i)
                .unwrap_or(Intent::Retriable)
        }

        /// The codes carrying one intent — the input to a `podFailurePolicy`
        /// rule (`FailJob` over `terminal`, `Count` over `retriable`+`policy`).
        pub fn codes_with_intent(&self, intent: Intent) -> Vec<i32> {
            self.codes
                .iter()
                .filter(|(_, _, i)| *i == intent)
                .map(|(c, _, _)| *c)
                .collect()
        }
    }
}

/// The metrics registry (vendored, `metrics_schema 1.2`).
pub mod metrics {
    use super::Version;
    use serde::Deserialize;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Status {
        /// Written at runtime — safe to alert/scale on.
        Live,
        /// Rendered but flat/never written — MUST NOT be targeted.
        Reserved,
        /// Incremented in the child process — a supervisor scrape under-reports.
        ChildLocal,
    }

    #[derive(Debug, Deserialize)]
    struct RawMetric {
        name: String,
        status: String,
    }

    #[derive(Debug, Deserialize)]
    struct RawRegistry {
        metrics_schema: String,
        metrics: Vec<RawMetric>,
        scaler_guidance: RawGuidance,
    }

    #[derive(Debug, Deserialize)]
    struct RawGuidance {
        primary: String,
    }

    #[derive(Debug)]
    pub struct Registry {
        pub version: Version,
        pub scaler_primary: String,
        metrics: Vec<(String, Status)>,
    }

    impl Registry {
        pub fn vendored() -> Registry {
            let raw: RawRegistry = serde_json::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../contract/schemas/metrics.registry.json"
            )))
            .expect("vendored metrics registry parses");
            Registry {
                version: Version::parse(&raw.metrics_schema).expect("metrics_schema version"),
                scaler_primary: raw.scaler_guidance.primary,
                metrics: raw
                    .metrics
                    .into_iter()
                    .map(|m| {
                        let s = match m.status.as_str() {
                            "live" => Status::Live,
                            "reserved" => Status::Reserved,
                            "child_local" => Status::ChildLocal,
                            other => panic!("unknown metric status {other:?}"),
                        };
                        (m.name, s)
                    })
                    .collect(),
            }
        }

        pub fn status(&self, name: &str) -> Option<Status> {
            let direct = self
                .metrics
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| *s);
            if direct.is_some() {
                return direct;
            }
            // Histogram/summary expositions emit `<base>_sum/_count/_bucket`
            // series; the registry lists the BASE name once.
            for suffix in ["_sum", "_count", "_bucket"] {
                if let Some(base) = name.strip_suffix(suffix) {
                    if let Some(s) = self
                        .metrics
                        .iter()
                        .find(|(n, _)| n == base)
                        .map(|(_, s)| *s)
                    {
                        return Some(s);
                    }
                }
            }
            None
        }

        pub fn is_registered(&self, name: &str) -> bool {
            self.status(name).is_some()
        }

        /// A metric a scaler/alert may target.
        pub fn is_live(&self, name: &str) -> bool {
            self.status(name) == Some(Status::Live)
        }
    }
}

/// The reload/restart config partition (vendored `restart-only.json`).
pub mod restart_only {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Raw {
        restart_only_paths: Vec<String>,
    }

    /// The vendored path list.
    pub fn paths() -> Vec<String> {
        let raw: Raw = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contract/schemas/restart-only.json"
        )))
        .expect("vendored restart-only list parses");
        raw.restart_only_paths
    }

    /// Does a changed dotted path fall in the restart-only partition?
    /// Subtree semantics both ways: entry `security` covers a change at
    /// `security.policies`; a change replacing the whole `a2a` object covers
    /// the entry `a2a.tls`.
    pub fn is_restart_only(changed_path: &str, entries: &[String]) -> bool {
        entries.iter().any(|e| {
            e == changed_path
                || changed_path.starts_with(&format!("{e}."))
                || e.starts_with(&format!("{changed_path}."))
        })
    }
}

// ---------------------------------------------------------------------------
// The capabilities manifest (informational at ACC 2)
// ---------------------------------------------------------------------------

/// One workflow as the manifest reports it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WorkflowInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub inputs_schema: bool,
    /// KNOWN GAP (U3): `webhook` and `stream` starts are missing here —
    /// an empty list does not mean "no trigger".
    #[serde(default)]
    pub start_kinds: Vec<String>,
}

/// The `a2a` block when a listener is configured.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct A2aInfo {
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub bearer: bool,
    /// The admin verbs this build serves (`a2a.drain`, …).
    #[serde(default)]
    pub admin: Vec<String>,
    /// Built-in command ops. KNOWN GAP (U3): omits workflow-declared commands.
    #[serde(default)]
    pub command_ops: Vec<String>,
}

/// `agent` block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub instruction: bool,
    #[serde(default)]
    pub preflight: Option<String>,
}

/// `intelligence` block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct IntelligenceInfo {
    #[serde(default)]
    pub endpoints: u32,
    #[serde(default)]
    pub model: Option<String>,
}

/// `lifecycle` block. `daemon` is documented-unreliable (U4): never the
/// workload-shape oracle — the renderer decides shape from the spec.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LifecycleInfo {
    #[serde(default)]
    pub daemon: bool,
    #[serde(default)]
    pub run_until: Option<String>,
}

/// The v1.3.x `--capabilities` document. Additive-tolerant throughout; use it
/// to *identify* an agent and enumerate its configured inventories — never to
/// decide workload shape or trigger presence (`contract/SPEC.md` §3).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Manifest {
    /// The config-document generation (`"1"`).
    #[serde(default)]
    pub runtime: Option<String>,
    /// The agent semver (e.g. `"1.3.1"`).
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub agent: AgentInfo,
    #[serde(default)]
    pub intelligence: IntelligenceInfo,
    #[serde(default)]
    pub internal_tools: Vec<String>,
    #[serde(default)]
    pub workflows: Vec<WorkflowInfo>,
    #[serde(default)]
    pub a2a: Option<A2aInfo>,
    #[serde(default)]
    pub lifecycle: LifecycleInfo,
    /// Untyped remainder (interface, store, skills, …) for forward-compat
    /// readers that need a peek without a schema commitment.
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

impl Manifest {
    /// A manifest that identifies as the runtime generation this client
    /// understands. (`None` runtime ⇒ pre-rewrite agent ⇒ unmanageable.)
    pub fn is_runtime_1(&self) -> bool {
        self.runtime.as_deref() == Some("1")
    }

    pub fn workflow(&self, name: &str) -> Option<&WorkflowInfo> {
        self.workflows.iter().find(|w| w.name == name)
    }

    pub fn admin_verbs(&self) -> &[String] {
        self.a2a.as_ref().map(|a| a.admin.as_slice()).unwrap_or(&[])
    }
}

/// Parse a `--capabilities` document.
pub fn parse_manifest(json: &str) -> serde_json::Result<Manifest> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn version_parses_and_displays() {
        let v = Version::parse("1.2").unwrap();
        assert_eq!((v.major, v.minor), (1, 2));
        assert_eq!(v.to_string(), "1.2");
        assert!(Version::parse("nope").is_none());
        assert!(Version::parse("3").is_none());
    }

    #[test]
    fn config_schema_clock_negotiates_and_refuses() {
        let ok = serde_json::json!({ "x-agentd-contract-version": "1.0" });
        assert_eq!(
            clocks::negotiate_config_schema(&ok).unwrap(),
            Version { major: 1, minor: 0 }
        );
        let newer_minor = serde_json::json!({ "x-agentd-contract-version": "1.7" });
        assert!(clocks::negotiate_config_schema(&newer_minor).is_ok());
        let major = serde_json::json!({ "x-agentd-contract-version": "2.0" });
        assert!(matches!(
            clocks::negotiate_config_schema(&major),
            Err(SkewError::UnsupportedMajor { .. })
        ));
        let absent = serde_json::json!({});
        assert!(matches!(
            clocks::negotiate_config_schema(&absent),
            Err(SkewError::Malformed { .. })
        ));
    }

    #[test]
    fn vendored_schemas_carry_the_supported_clocks() {
        let cfg = clocks::vendored_config_schema();
        let v = clocks::negotiate_config_schema(&cfg).expect("vendored config clock");
        assert_eq!(v.major, clocks::CONFIG_CONTRACT_MAJOR);
        let wf = clocks::vendored_workflow_schema();
        assert_eq!(
            clocks::negotiate_workflow_schema(&wf).expect("vendored workflow clock"),
            clocks::WORKFLOW_DIALECT
        );
    }

    #[test]
    fn exit_code_table_matches_the_frozen_mapping() {
        use exit_codes::Intent::*;
        let t = exit_codes::Table::vendored();
        assert_eq!(t.version, Version { major: 1, minor: 0 });
        for (code, intent) in [
            (0, Complete),
            (2, Terminal),
            (5, Terminal),
            (3, Policy),
            (7, Policy),
            (124, Policy),
            (1, Retriable),
            (4, Retriable),
            (6, Retriable),
            (137, Infra),
            (143, Infra),
        ] {
            assert_eq!(t.intent(code), intent, "code {code}");
            assert!(t.is_known(code));
        }
        // The conservative default for a future additive code.
        assert_eq!(t.intent(99), Retriable);
        assert!(!t.is_known(99));
        assert_eq!(t.codes_with_intent(Terminal), vec![2, 5]);
    }

    #[test]
    fn metrics_registry_separates_live_from_reserved() {
        let r = metrics::Registry::vendored();
        assert_eq!(r.version, Version { major: 1, minor: 2 });
        assert_eq!(r.scaler_primary, "agent_inbox_pending");
        assert!(r.is_live("agent_inbox_pending"));
        assert!(r.is_registered("agent_pending_events"));
        assert!(
            !r.is_live("agent_pending_events"),
            "reserved-flat: never target"
        );
        assert_eq!(
            r.status("agent_loop_steps_total"),
            Some(metrics::Status::ChildLocal)
        );
        assert!(!r.is_registered("agent_made_up_total"));
    }

    #[test]
    fn restart_only_matches_subtrees_both_ways() {
        let entries = restart_only::paths();
        assert!(entries.iter().any(|e| e == "security"));
        assert!(restart_only::is_restart_only("security.policies", &entries));
        assert!(restart_only::is_restart_only("store.kind", &entries));
        assert!(
            restart_only::is_restart_only("a2a", &entries),
            "whole-object replace covers a2a.tls"
        );
        assert!(
            !restart_only::is_restart_only("a2a.principals", &entries),
            "principals hot-reload"
        );
        assert!(!restart_only::is_restart_only(
            "intelligence.endpoints",
            &entries
        ));
        assert!(!restart_only::is_restart_only("mcp.servers", &entries));
    }
}
