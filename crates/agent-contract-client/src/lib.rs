// SPDX-License-Identifier: Apache-2.0
//! # agent-contract-client
//!
//! The typed client for the **Agent Control Contract (ACC)** — the
//! language-neutral contract that agentctl consumes and that *any* conformant
//! agent implements (see `contract/`).
//!
//! **Design principle:** agentctl depends on the *contract*, never on a specific
//! agent. `agentd` is the reference implementation only. This crate therefore
//! models the contract's wire shapes — it does not import any agent's types.
//!
//! Most of the manifest is plain serde. The load-bearing exceptions are the
//! `surfaces{}` **sum types** that codegen cannot derive; these carry
//! hand-written [`Deserialize`] impls here:
//!
//! | field | shape | type |
//! |---|---|---|
//! | `surfaces.management` / `surfaces.metrics` | `false \| string` | [`SurfaceAddr`] |
//! | `surfaces.a2a` | `false \| object` | [`A2aSurface`] |
//! | `surfaces.claim` | `bool \| object` | [`ClaimSurface`] |
//! | `surfaces.shard` | `string \| null` | `Option<String>` |
//! | `intelligence.healthy` | `"unknown" \| bool` | [`Health`] |
//!
//! One invariant from the contract's version-negotiation rules is enforced
//! structurally:
//!
//! * **Additive tolerance** — every struct ignores unknown fields (no
//!   `deny_unknown_fields`), so a newer agent that adds manifest keys, surface
//!   keys, or operator tools still parses. A consumer refuses only an unknown
//!   **major** (see [`Manifest::negotiate`]).

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

/// The contract major version this client understands. A manifest whose
/// `contract_version` major differs is refused ([`Manifest::negotiate`]); a
/// differing minor is tolerated (additive-by-minor).
///
/// Under contract major 1 the reference agent serves exclusively over HTTP:
/// MCP servers are remote `https://` endpoints, the A2A methods use the bare
/// PascalCase binding, and the serving surface is mTLS HTTPS. The `surfaces{}`
/// sum types below are transport-agnostic, so they remain valid regardless of
/// transport; the major version is the compatibility gate.
pub const SUPPORTED_MAJOR: u32 = 1;

// ---------------------------------------------------------------------------
// The capabilities manifest
// ---------------------------------------------------------------------------

/// The capabilities manifest — the discovery spine of the contract. Emitted by
/// `--capabilities` (one-shot) and the `agent://capabilities` resource (live).
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// The contract version, `major.minor` (e.g. `"1.0"`). The only key a
    /// consumer must understand before anything else; gate on it via
    /// [`Manifest::negotiate`].
    pub contract_version: String,

    /// The agent-version key the reference agent emits (descriptive metadata;
    /// resolve via [`Manifest::version`]).
    #[serde(default)]
    pub agent_version: Option<String>,

    /// Compiled-in build features. **OPAQUE / agent-defined — never branch on a
    /// value here.** Capability discovery keys exclusively off [`Surfaces`].
    /// Useful only as diagnostic metadata and as a cache discriminator.
    #[serde(default)]
    pub build_features: Vec<String>,

    /// Downward-API instance identity (all fields optional / null when run
    /// outside a cluster).
    #[serde(default)]
    pub identity: Identity,

    /// The configured run shape (`once` / `loop` / `reactive` / `schedule`).
    #[serde(default)]
    pub mode: Option<String>,
    /// Operator-declared model id (metadata, never a secret).
    #[serde(default)]
    pub model: Option<String>,

    /// Intelligence binding (structural only — transport + endpoint count +
    /// reachability; never a URL or credential).
    #[serde(default)]
    pub intelligence: Intelligence,

    /// Resolved limits/budgets.
    #[serde(default)]
    pub limits: Limits,

    /// Declared MCP servers and their capability tags.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,

    /// Whether the gated `exec` capability is enabled.
    #[serde(default)]
    pub exec_enabled: bool,
    /// Whether the lethal-trifecta override is permitted for this instance.
    #[serde(default)]
    pub allow_trifecta: bool,

    /// **The single discovery point**: which control-plane surfaces this build/
    /// config actually serves. Drive only what is declared (graceful
    /// degradation).
    pub surfaces: Surfaces,
}

impl Manifest {
    /// Resolve and validate the contract version. Returns the parsed version on
    /// success; errors only on a malformed string or an **unsupported major**
    /// (the one breaking condition — additive minors are accepted).
    pub fn negotiate(&self) -> Result<ContractVersion, NegotiationError> {
        let v = ContractVersion::parse(&self.contract_version)?;
        if v.major != SUPPORTED_MAJOR {
            return Err(NegotiationError::UnsupportedMajor {
                found: v.major,
                supported: SUPPORTED_MAJOR,
            });
        }
        Ok(v)
    }

    /// The agent version, from the neutral `agent_version` key.
    pub fn version(&self) -> Option<&str> {
        self.agent_version.as_deref()
    }
}

/// Downward-API instance identity (contract `env-convention`). All optional —
/// descriptive, not load-bearing.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Identity {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub uid: Option<String>,
}

/// Structural intelligence binding (no URLs, no credentials).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Intelligence {
    #[serde(default)]
    pub endpoints: u32,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub healthy: Health,
}

/// Resolved limits/budgets. Additive-tolerant.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Limits {
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    #[serde(default)]
    pub drain_timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_children: Option<u64>,
    #[serde(default)]
    pub max_depth: Option<u64>,
    #[serde(default)]
    pub max_steps: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_total_subagents: Option<u64>,
    #[serde(default)]
    pub tree_token_budget: Option<u64>,
}

/// A declared MCP server and its capability/trifecta tags.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServer {
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// surfaces{} — the discovery block (with the sum-type fields)
// ---------------------------------------------------------------------------

/// The `surfaces{}` block: the single point where an agent advertises which
/// control-plane surfaces it serves. A key absent/off ⇒ that surface is unbuilt
/// ⇒ the consumer degrades gracefully.
#[derive(Debug, Clone, Deserialize)]
pub struct Surfaces {
    /// Management transport address, or off.
    #[serde(default = "SurfaceAddr::off")]
    pub management: SurfaceAddr,
    /// Prometheus `/metrics` scrape address, or off.
    #[serde(default = "SurfaceAddr::off")]
    pub metrics: SurfaceAddr,
    /// A2A surface (methods/streaming/version), or off.
    #[serde(default = "A2aSurface::off")]
    pub a2a: A2aSurface,
    /// Work-claim styles, if the claim surface is served.
    #[serde(default)]
    pub claim: Option<ClaimSurface>,
    /// Workflow-execution surface (dialect + checkpoint/resume), if served.
    /// Omitted-when-absent. Gate resumable-workflow behaviour on
    /// [`WorkflowSurface::resumable`] (`dialect >= 2 && checkpoint`).
    #[serde(default)]
    pub workflow: Option<WorkflowSurface>,
    /// AAuth identity surface, if the agent carries a portable AAuth identity
    /// (RFC 0023). Omitted-when-absent (a stock/unconfigured build). Carries
    /// the draft marker, the bound provider, and the resolved identity —
    /// never key or token material.
    #[serde(default)]
    pub aauth: Option<AauthSurface>,
    /// Shard identity `"K/N"`, or `null`.
    #[serde(default)]
    pub shard: Option<String>,

    #[serde(default)]
    pub events: bool,
    #[serde(default)]
    pub hot_reload: bool,
    #[serde(default)]
    pub config_validate: bool,
    #[serde(default)]
    pub config_schema: bool,
    #[serde(default)]
    pub intelligence: bool,
    #[serde(default)]
    pub cluster: bool,
    #[serde(default)]
    pub standby: bool,

    /// Sub-schema versions surfaced for independent negotiation.
    #[serde(default)]
    pub metrics_schema: Option<String>,
    #[serde(default)]
    pub report_schema: Option<String>,
    #[serde(default)]
    pub exit_codes: Option<String>,

    /// The operator tools actually served (read this — never hardcode the set).
    #[serde(default)]
    pub operator_tools: Vec<String>,
}

/// `false | string` — a served surface address, or off. (`true` is rejected.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceAddr {
    /// The surface is not served.
    Off,
    /// The surface is served at this address (e.g. the mTLS HTTPS management
    /// address `"https://0.0.0.0:8443"` or the metrics scrape address
    /// `"127.0.0.1:9090"`).
    At(String),
}

impl SurfaceAddr {
    fn off() -> Self {
        SurfaceAddr::Off
    }
    /// The address if served, else `None`.
    pub fn addr(&self) -> Option<&str> {
        match self {
            SurfaceAddr::At(s) => Some(s),
            SurfaceAddr::Off => None,
        }
    }
    /// Whether the surface is served.
    pub fn is_served(&self) -> bool {
        matches!(self, SurfaceAddr::At(_))
    }
}

impl<'de> Deserialize<'de> for SurfaceAddr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(false) => Ok(SurfaceAddr::Off),
            serde_json::Value::String(s) => Ok(SurfaceAddr::At(s)),
            other => Err(D::Error::custom(format!(
                "surface address must be `false` or a string, got {other}"
            ))),
        }
    }
}

/// `false | object` — the A2A surface descriptor, or off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum A2aSurface {
    /// A2A is not served.
    Off,
    /// A2A is served with this descriptor.
    On(A2aInfo),
}

/// The served-A2A descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct A2aInfo {
    pub version: String,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub methods: Vec<String>,
}

impl A2aSurface {
    fn off() -> Self {
        A2aSurface::Off
    }
    /// The descriptor if served, else `None`.
    pub fn info(&self) -> Option<&A2aInfo> {
        match self {
            A2aSurface::On(i) => Some(i),
            A2aSurface::Off => None,
        }
    }
    /// Whether A2A is served.
    pub fn is_served(&self) -> bool {
        matches!(self, A2aSurface::On(_))
    }
}

impl<'de> Deserialize<'de> for A2aSurface {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Bool(false) => Ok(A2aSurface::Off),
            serde_json::Value::Object(_) => serde_json::from_value(v)
                .map(A2aSurface::On)
                .map_err(D::Error::custom),
            other => Err(D::Error::custom(format!(
                "surfaces.a2a must be `false` or an object, got {other}"
            ))),
        }
    }
}

/// `bool | object` — the work-claim surface. Per the contract this is
/// omitted-when-absent rather than `false`, but both spellings are accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimSurface {
    /// Claiming is not served.
    Off,
    /// Claiming is served with these styles (e.g. `["tool", "resource"]`).
    On { styles: Vec<String> },
}

impl ClaimSurface {
    /// The claim styles if served, else `None`.
    pub fn styles(&self) -> Option<&[String]> {
        match self {
            ClaimSurface::On { styles } => Some(styles),
            ClaimSurface::Off => None,
        }
    }
}

impl<'de> Deserialize<'de> for ClaimSurface {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Styles {
            #[serde(default)]
            styles: Vec<String>,
        }
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(b) => Ok(if b {
                ClaimSurface::On { styles: Vec::new() }
            } else {
                ClaimSurface::Off
            }),
            v @ serde_json::Value::Object(_) => {
                let s: Styles = serde_json::from_value(v).map_err(D::Error::custom)?;
                Ok(ClaimSurface::On { styles: s.styles })
            }
            other => Err(D::Error::custom(format!(
                "surfaces.claim must be a bool or an object, got {other}"
            ))),
        }
    }
}

/// `bool | object` — the workflow-execution surface. Per the contract this is
/// omitted-when-absent (a build without a workflow engine leaves the key out);
/// a bare `true` is accepted as an object with `dialect == 1` and no checkpoint.
/// The resumable-workflow capability (checkpoint/resume + the INPUT_REQUIRED
/// gate + `--workflow-resume`) is advertised by `dialect >= 2 && checkpoint`;
/// read it via [`WorkflowSurface::resumable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowSurface {
    /// A workflow engine is not served.
    Off,
    /// A workflow engine is served with this dialect + checkpoint capability.
    On {
        /// Workflow-graph dialect version (>= 2 introduces checkpoint/resume).
        dialect: u32,
        /// Whether checkpoint/resume (INPUT_REQUIRED gate + gate-reply) is served.
        checkpoint: bool,
    },
}

impl WorkflowSurface {
    /// Whether a workflow engine is served at all.
    pub fn is_served(&self) -> bool {
        matches!(self, WorkflowSurface::On { .. })
    }
    /// Whether the RESUMABLE workflow surface is served: `dialect >= 2` AND
    /// `checkpoint`. This is the single gate a consumer keys the
    /// checkpoint/resume + INPUT_REQUIRED-gate behaviour off.
    pub fn resumable(&self) -> bool {
        matches!(self, WorkflowSurface::On { dialect, checkpoint } if *dialect >= 2 && *checkpoint)
    }
}

impl<'de> Deserialize<'de> for WorkflowSurface {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wf {
            #[serde(default = "one")]
            dialect: u32,
            #[serde(default)]
            checkpoint: bool,
        }
        fn one() -> u32 {
            1
        }
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(false) => Ok(WorkflowSurface::Off),
            serde_json::Value::Bool(true) => Ok(WorkflowSurface::On {
                dialect: 1,
                checkpoint: false,
            }),
            v @ serde_json::Value::Object(_) => {
                let w: Wf = serde_json::from_value(v).map_err(D::Error::custom)?;
                Ok(WorkflowSurface::On {
                    dialect: w.dialect,
                    checkpoint: w.checkpoint,
                })
            }
            other => Err(D::Error::custom(format!(
                "surfaces.workflow must be a bool or an object, got {other}"
            ))),
        }
    }
}

/// `surfaces.aauth` — the portable-identity surface (RFC 0023): `false`/omitted
/// (a stock or unconfigured build) or an object `{draft, provider, agent}`.
/// `agent` is `null` until the instance has enrolled + fetched its first token
/// (`aauth.ready`). The surface NEVER carries key or token material. While
/// `draft` is true the underlying protocol tracks unreleased IETF drafts —
/// consumers treat the capability as experimental.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AauthSurface {
    /// No AAuth identity is configured/built.
    Off,
    /// An AAuth identity is configured.
    On {
        /// The protocol is a draft; treat as experimental while true.
        draft: bool,
        /// The Agent Provider issuer URL this instance is bound to.
        provider: String,
        /// The resolved identity (`aauth:local@domain`), or `None` pre-enroll.
        agent: Option<String>,
    },
}

impl AauthSurface {
    /// Whether an AAuth identity is configured at all.
    pub fn is_served(&self) -> bool {
        matches!(self, AauthSurface::On { .. })
    }
    /// The resolved identity, once the instance has enrolled.
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            AauthSurface::On {
                agent: Some(id), ..
            } => Some(id),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for AauthSurface {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Aa {
            #[serde(default)]
            draft: bool,
            provider: String,
            #[serde(default)]
            agent: Option<String>,
        }
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(false) => Ok(AauthSurface::Off),
            v @ serde_json::Value::Object(_) => {
                let a: Aa = serde_json::from_value(v).map_err(D::Error::custom)?;
                Ok(AauthSurface::On {
                    draft: a.draft,
                    provider: a.provider,
                    agent: a.agent,
                })
            }
            other => Err(D::Error::custom(format!(
                "surfaces.aauth must be `false` or an object, got {other}"
            ))),
        }
    }
}

/// `"unknown" | bool` — intelligence reachability, or unknown pre-connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Health {
    /// Reachability not yet known (e.g. a pre-connect `--capabilities` probe).
    #[default]
    Unknown,
    /// Last-known reachability.
    Known(bool),
}

impl<'de> Deserialize<'de> for Health {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::String(s) if s == "unknown" => Ok(Health::Unknown),
            serde_json::Value::Bool(b) => Ok(Health::Known(b)),
            other => Err(D::Error::custom(format!(
                "intelligence.healthy must be `\"unknown\"` or a bool, got {other}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Version negotiation
// ---------------------------------------------------------------------------

/// A parsed `major.minor` contract version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractVersion {
    pub major: u32,
    pub minor: u32,
}

impl ContractVersion {
    /// Parse a `"major.minor"` string.
    pub fn parse(s: &str) -> Result<Self, NegotiationError> {
        let (maj, min) = s
            .split_once('.')
            .ok_or_else(|| NegotiationError::Malformed(s.to_string()))?;
        let major = maj
            .parse()
            .map_err(|_| NegotiationError::Malformed(s.to_string()))?;
        let minor = min
            .parse()
            .map_err(|_| NegotiationError::Malformed(s.to_string()))?;
        Ok(ContractVersion { major, minor })
    }
}

impl std::fmt::Display for ContractVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Why a manifest's contract version could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiationError {
    /// `contract_version` was not a `major.minor` string.
    Malformed(String),
    /// The major version is one this client does not understand.
    UnsupportedMajor { found: u32, supported: u32 },
}

impl std::fmt::Display for NegotiationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NegotiationError::Malformed(s) => {
                write!(
                    f,
                    "malformed contract_version: {s:?} (want \"major.minor\")"
                )
            }
            NegotiationError::UnsupportedMajor { found, supported } => write!(
                f,
                "unsupported contract major {found} (this client speaks major {supported})"
            ),
        }
    }
}

impl std::error::Error for NegotiationError {}

/// Parse a capabilities manifest from JSON.
pub fn parse_manifest(json: &str) -> serde_json::Result<Manifest> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn contract_version_parses() {
        assert_eq!(
            ContractVersion::parse("1.0").unwrap(),
            ContractVersion { major: 1, minor: 0 }
        );
        assert_eq!(
            ContractVersion::parse("2.7").unwrap(),
            ContractVersion { major: 2, minor: 7 }
        );
        assert!(ContractVersion::parse("1").is_err());
        assert!(ContractVersion::parse("x.y").is_err());
    }

    #[test]
    fn health_sum_type() {
        assert_eq!(
            serde_json::from_str::<Health>("\"unknown\"").unwrap(),
            Health::Unknown
        );
        assert_eq!(
            serde_json::from_str::<Health>("true").unwrap(),
            Health::Known(true)
        );
        assert!(serde_json::from_str::<Health>("\"maybe\"").is_err());
    }

    #[test]
    fn surface_addr_rejects_true() {
        assert_eq!(
            serde_json::from_str::<SurfaceAddr>("false").unwrap(),
            SurfaceAddr::Off
        );
        assert_eq!(
            serde_json::from_str::<SurfaceAddr>("\"https://0.0.0.0:8443\"").unwrap(),
            SurfaceAddr::At("https://0.0.0.0:8443".into())
        );
        assert!(serde_json::from_str::<SurfaceAddr>("true").is_err());
    }

    #[test]
    fn workflow_surface_detects_the_resumable_capability() {
        use WorkflowSurface as W;
        // Absent ⇒ Off ⇒ not resumable (via the Surfaces default).
        let s: Surfaces = serde_json::from_str("{}").unwrap();
        assert!(s.workflow.is_none());

        // A base dialect-1 engine: served, but NOT resumable.
        let base: W = serde_json::from_str(r#"{"dialect":1}"#).unwrap();
        assert!(base.is_served());
        assert!(!base.resumable());

        // dialect >= 2 alone is not enough — checkpoint must also be true.
        let no_ckpt: W = serde_json::from_str(r#"{"dialect":2}"#).unwrap();
        assert!(!no_ckpt.resumable(), "dialect>=2 without checkpoint");

        // The full resumable surface.
        let full: W = serde_json::from_str(r#"{"dialect":2,"checkpoint":true}"#).unwrap();
        assert!(full.resumable());
        assert_eq!(
            full,
            W::On {
                dialect: 2,
                checkpoint: true
            }
        );

        // `false` ⇒ Off; bare `true` ⇒ served-but-dialect-1 (not resumable).
        assert_eq!(serde_json::from_str::<W>("false").unwrap(), W::Off);
        assert!(!serde_json::from_str::<W>("true").unwrap().resumable());

        // Parsed off a full manifest surfaces block.
        let m = parse_manifest(
            r#"{"contract_version":"1.0",
                "surfaces":{"workflow":{"dialect":2,"checkpoint":true}}}"#,
        )
        .unwrap();
        assert!(m.surfaces.workflow.as_ref().unwrap().resumable());
    }

    #[test]
    fn aauth_surface_exposes_identity_never_material() {
        use AauthSurface as A;
        // Absent ⇒ None (a stock/unconfigured build leaves the key out).
        let s: Surfaces = serde_json::from_str("{}").unwrap();
        assert!(s.aauth.is_none());

        // Configured but not yet enrolled: agent is null.
        let pre: A =
            serde_json::from_str(r#"{"draft":true,"provider":"https://ap.example"}"#).unwrap();
        assert!(pre.is_served());
        assert_eq!(pre.agent_id(), None);

        // Enrolled: the resolved identity is readable.
        let m = parse_manifest(
            r#"{"contract_version":"1.0",
                "surfaces":{"aauth":{"draft":true,
                                     "provider":"https://ap.example",
                                     "agent":"aauth:k7q3p9n2@ap.example"}}}"#,
        )
        .unwrap();
        let a = m.surfaces.aauth.as_ref().unwrap();
        assert_eq!(a.agent_id(), Some("aauth:k7q3p9n2@ap.example"));
        assert_eq!(
            a,
            &A::On {
                draft: true,
                provider: "https://ap.example".into(),
                agent: Some("aauth:k7q3p9n2@ap.example".into()),
            }
        );

        // `false` ⇒ Off; a missing provider on the object form is an error
        // (the surface is meaningless without its issuer).
        assert_eq!(serde_json::from_str::<A>("false").unwrap(), A::Off);
        assert!(serde_json::from_str::<A>(r#"{"draft":true}"#).is_err());
    }
}
