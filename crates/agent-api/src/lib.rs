// SPDX-License-Identifier: Apache-2.0
//! # agent-api
//!
//! The agentctl custom-resource types — `Agent` and `AgentFleet` — per agentctl
//! RFC 0003. These are **contract-shaped**: a CR describes contract-level intent
//! (mode, surfaces to expose, intelligence/MCP bindings, substrate), not any
//! agent's internals (principle P0). `agent` (the reference agent binary; repo
//! `agentd-dev`) is the reference implementation; these types never reference it.
//!
//! Generated as kube-rs [`kube::CustomResource`]s. CRD YAML is produced via
//! [`kube::CustomResourceExt::crd`] (see the `agentctl-crdgen` path / tests).
//!
//! Versioning per RFC 0005: a single served version (`v1alpha1`) at a time, with
//! conversion handled out of band — the CRD `apiVersion` clock is decoupled from
//! the agent `contract_version` clock.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Group for all agentctl CRDs. Provisional (RFC 0003 open question): the final
/// string (`agents.x-k8s.io` vs `agentctl.dev`) is undecided; the shapes here
/// are group-string-independent.
pub const GROUP: &str = "agents.x-k8s.io";

// ===========================================================================
// Agent
// ===========================================================================

/// One logical agent: an instruction + bindings rendered to a workload whose
/// shape follows `mode` (RFC 0003 §5: once→Job, schedule→CronJob,
/// loop/reactive→Deployment).
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agents.x-k8s.io",
    version = "v1alpha1",
    kind = "Agent",
    namespaced,
    status = "AgentStatus",
    shortname = "agent",
    shortname = "agents",
    printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".spec.mode"}"#,
    printcolumn = r#"{"name":"Model","type":"string","jsonPath":".spec.model","priority":1}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "self.mode != 'schedule' || has(self.schedule)",
    "message": "schedule is required when mode is 'schedule'"
}]))]
pub struct AgentSpec {
    /// The run shape. Determines the rendered workload kind.
    pub mode: Mode,

    /// The conformant-agent image to run. **Required iff `classRef` is unset;
    /// forbidden when `classRef` is set** (enforced by CEL/admission, RFC 0003
    /// §7 / RFC 0007). A classless `Agent` names its own image here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Reference to an `AgentClass` (RFC 0004) supplying the ops profile +
    /// image. Mutually exclusive with `image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_ref: Option<LocalRef>,

    /// The agent's declared model id (metadata; the real binding is via
    /// intelligence). Surfaced as a printer column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Inline instruction. Required for non-reactive modes (CEL/admission).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,

    /// Reference to an `IntelligenceService`/ModelPool (RFC 0004/0012).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence_ref: Option<LocalRef>,

    /// Reusable MCP server bundles (RFC 0004 `MCPServerSet`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_server_set_refs: Vec<LocalRef>,

    /// Substrate selection (RFC 0002). Absent ⇒ inherited from the `AgentClass`
    /// / cluster default (hostile tenancy forces `kata-hybrid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substrate: Option<Substrate>,

    /// Which control-plane surfaces to expose (RFC 0003 §3). The operator drives
    /// only what the agent actually advertises in its manifest (graceful
    /// degradation), intersected with this desired set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surfaces: Option<DesiredSurfaces>,

    /// Reactive-mode subscriptions (MCP resource URIs). Required for `reactive`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscribe: Vec<String>,
    /// Loop-mode cadence. Used only for `loop`.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "loop")]
    pub loop_: Option<LoopParams>,
    /// Schedule-mode cron. Used only for `schedule`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,

    /// Resolved limits/budgets (override the agent defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,

    /// **Declared capability** (RFC 0007): the agent requests in-sandbox command
    /// execution. The admission webhook gates this — it is a privileged
    /// capability and one leg of the lethal trifecta, never granted implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<bool>,

    /// **Declared capability** (RFC 0007): the agent requests outbound network
    /// egress. The admission webhook gates this — combined with `exec` and
    /// `secrets` it forms the lethal trifecta and triggers the override gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<bool>,

    /// **Declared capability** (RFC 0007): the names of namespace-local `Secret`s
    /// the agent may read. The admission webhook validates each requested name
    /// against policy; access to untrusted private data is a trifecta leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<String>>,

    /// **Declared capability** (RFC 0007): the `ModelPool` (RFC 0012) this agent
    /// binds for model access. The admission webhook validates the binding (the
    /// pool exists / is permitted for this tenant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_pool: Option<String>,
}

/// The run shape (RFC 0003 §5 / agentd RFC 0008).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Run to a terminal status, then exit (→ Job).
    #[default]
    Once,
    /// Re-enter on a cadence until a bound (→ Deployment).
    Loop,
    /// Idle, wake on subscribed MCP resource changes (→ Deployment).
    Reactive,
    /// Internal cron (→ CronJob). Production cron prefers an external scheduler.
    Schedule,
}

/// Substrate tier (RFC 0002). Names are the canonical tier ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Substrate {
    /// unix-socket-over-hostPath → host DaemonSet (dev / single-tenant).
    StockUnix,
    /// vsock on Kata hybrid (hardened; default for hostile multi-tenant prod).
    KataHybrid,
    /// per-pod sidecar over emptyDir (most portable; weakest isolation).
    SidecarEmptydir,
}

/// Which control-plane surfaces an `Agent` wants exposed (RFC 0003 §3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesiredSurfaces {
    #[serde(default)]
    pub management: bool,
    #[serde(default)]
    pub metrics: bool,
    #[serde(default)]
    pub a2a: bool,
}

/// Loop-mode cadence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoopParams {
    /// Re-entry interval, e.g. `"5m"`.
    pub interval: String,
    /// Optional wall-clock bound, e.g. `"24h"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
}

/// Schedule-mode cron (contract cron is UTC-only; `timezone` is advisory).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub cron: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Resolved limits/budgets (subset; additive).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_token_budget: Option<u64>,
}

/// A name-only reference to a sibling resource in the same namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct LocalRef {
    pub name: String,
}

/// `Agent.status` — a curated projection of the live capabilities manifest +
/// health (RFC 0003 §6), never a raw manifest dump.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    /// The conditions taxonomy (RFC 0003 §6.2): `Validated`, `Rendered`,
    /// `Ready`, `Draining`, `Degraded`, plus advisory `TrifectaUnionObserved`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// The `.metadata.generation` this status reflects (hot-loop guard, RFC 0006).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// A coarse human-facing phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// The curated contract projection from the live manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<ContractStatus>,
}

/// Curated facts projected from the agent's live capabilities manifest.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContractStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The operator tools the live agent advertises (read, never assumed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_tools: Vec<String>,
    /// Served-surface summary (true ⇒ advertised + served).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served: Option<ServedSurfaces>,
}

/// Which surfaces the live agent actually serves.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServedSurfaces {
    #[serde(default)]
    pub management: bool,
    #[serde(default)]
    pub metrics: bool,
    #[serde(default)]
    pub a2a: bool,
    #[serde(default)]
    pub events: bool,
}

/// A status condition (RFC 0003 §6.2). Mirrors `metav1.Condition` shape.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    /// `"True"` | `"False"` | `"Unknown"`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// RFC 3339 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

// ===========================================================================
// AgentFleet
// ===========================================================================

/// A replicated, autoscaled set of agents (RFC 0003 §4 / RFC 0011). Renders to a
/// StatefulSet (shard mode) or Deployment (claim mode); **KEDA owns
/// `.spec.replicas`**, so the rendered workload omits it.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agents.x-k8s.io",
    version = "v1alpha1",
    kind = "AgentFleet",
    namespaced,
    status = "AgentFleetStatus",
    shortname = "afleet",
    shortname = "afleets",
    printcolumn = r#"{"name":"Scaling","type":"string","jsonPath":".spec.scaling.mode"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "self.scaling.mode != 'shard' || has(self.scaling.shards)",
    "message": "shards is required when scaling.mode is 'shard'"
}]))]
pub struct AgentFleetSpec {
    /// The per-replica agent definition.
    pub template: AgentSpec,
    /// The scaling regime.
    pub scaling: Scaling,
    /// The shared work source (an MCP resource URI) for claim/shard distribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_source: Option<String>,
}

/// The scaling regime (RFC 0011).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Scaling {
    pub mode: ScaleMode,
    /// Claim mode: minimum replicas (KEDA-elastic range). Unset ⇒ may scale to 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u32>,
    /// Claim mode: maximum replicas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    /// Shard mode: the fixed partition count `N` (the FNV-1a/64 modulus and the
    /// StatefulSet replica count). A partition count, **not** a KEDA range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shards: Option<u32>,
    /// Claim mode: the autoscaling signal (a contract-neutral metric token the
    /// operator maps onto the negotiated metrics schema).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ScaleTarget>,
}

/// Claim (elastic, KEDA) vs shard (fixed partition) scaling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScaleMode {
    /// Cross-replica work-claim/lease; elastic; KEDA owns replicas.
    #[default]
    Claim,
    /// Static `--shard K/N` partitioning; fixed at `shards`.
    Shard,
}

/// An autoscaling target signal.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScaleTarget {
    /// A contract-neutral metric token (mapped onto the negotiated metrics
    /// schema by the operator — never the branded literal).
    pub signal: String,
    /// The per-replica target value KEDA scales toward.
    pub value: String,
}

/// `AgentFleet.status`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentFleetStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scale_time: Option<String>,
}

// ===========================================================================
// ModelPool
// ===========================================================================

/// A pool of model access (RFC 0012, the intelligence plane). Agents are
/// **networkless and hold NO provider secrets**; the control plane supplies
/// model access through a credential-injecting, metering, budget-enforcing
/// proxy (the `ModelGateway`) configured by this CRD. The pool names the
/// provider, its endpoint, the `Secret` holding the provider API key, the
/// allowed models, and an optional total token budget.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agents.x-k8s.io",
    version = "v1alpha1",
    kind = "ModelPool",
    namespaced,
    status = "ModelPoolStatus",
    shortname = "mp",
    printcolumn = r#"{"name":"Provider","type":"string","jsonPath":".spec.provider"}"#,
    printcolumn = r#"{"name":"Endpoint","type":"string","jsonPath":".spec.endpoint","priority":1}"#,
    printcolumn = r#"{"name":"Budget","type":"integer","jsonPath":".spec.budget.maxTokens"}"#,
    printcolumn = r#"{"name":"Used","type":"integer","jsonPath":".status.usedTokens"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {
        "rule": "!has(self.budget) || self.budget.maxTokens > 0",
        "message": "budget.maxTokens must be > 0"
    },
    {
        "rule": "self.credentialSecretRef.name != ''",
        "message": "credentialSecretRef.name is required"
    }
]))]
pub struct ModelPoolSpec {
    /// Provider id, e.g. `"mock"` | `"anthropic"` | `"openai"` (free string).
    pub provider: String,

    /// Provider base URL, e.g. `http://mock-provider.default:8080`.
    pub endpoint: String,

    /// Reference to the `Secret` holding the provider API key. The gateway
    /// injects it; the agent never sees it.
    pub credential_secret_ref: SecretKeyRef,

    /// Allowed model ids for this pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,

    /// Default model id, used when a request does not pin one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    /// Total token budget for the pool (enforced by the gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

/// A reference to a specific key within a namespace-local `Secret`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    /// The `Secret` name (same namespace as the `ModelPool`).
    pub name: String,
    /// The key within the `Secret`'s data holding the provider API key.
    pub key: String,
}

/// A total token budget for a `ModelPool`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    /// Maximum total tokens the pool may consume.
    pub max_tokens: i64,
}

/// `ModelPool.status` — running meter against the pool's budget.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelPoolStatus {
    /// Total tokens consumed against the pool so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_tokens: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn agent_crd_generates() {
        let crd = Agent::crd();
        assert_eq!(crd.spec.group, "agents.x-k8s.io");
        assert_eq!(crd.spec.names.kind, "Agent");
        assert!(crd
            .spec
            .names
            .short_names
            .as_ref()
            .unwrap()
            .contains(&"agent".to_string()));
        assert!(crd.spec.versions.iter().any(|v| v.name == "v1alpha1"));
        // status subresource is enabled
        let v = &crd.spec.versions[0];
        assert!(v
            .subresources
            .as_ref()
            .and_then(|s| s.status.as_ref())
            .is_some());
        // printer columns wired
        assert!(v
            .additional_printer_columns
            .as_ref()
            .unwrap()
            .iter()
            .any(|c| c.name == "Mode"));
    }

    #[test]
    fn agentfleet_crd_generates() {
        let crd = AgentFleet::crd();
        assert_eq!(crd.spec.names.kind, "AgentFleet");
        assert!(crd.spec.versions.iter().any(|v| v.name == "v1alpha1"));
    }

    #[test]
    fn modelpool_crd_generates() {
        let crd = ModelPool::crd();
        assert_eq!(crd.spec.group, "agents.x-k8s.io");
        assert_eq!(crd.spec.names.kind, "ModelPool");
        assert!(crd
            .spec
            .names
            .short_names
            .as_ref()
            .unwrap()
            .contains(&"mp".to_string()));
        assert!(crd.spec.versions.iter().any(|v| v.name == "v1alpha1"));
        // status subresource is enabled
        let v = &crd.spec.versions[0];
        assert!(v
            .subresources
            .as_ref()
            .and_then(|s| s.status.as_ref())
            .is_some());
    }

    #[test]
    fn agent_roundtrips_camelcase() {
        let a = Agent::new(
            "demo",
            AgentSpec {
                mode: Mode::Reactive,
                image: Some("ghcr.io/example/agent@sha256:abc".into()),
                subscribe: vec!["file:///data/inbox".into()],
                surfaces: Some(DesiredSurfaces {
                    management: true,
                    metrics: true,
                    a2a: false,
                }),
                substrate: Some(Substrate::KataHybrid),
                ..Default::default()
            },
        );
        let json = serde_json::to_value(&a).unwrap();
        // camelCase + kebab/lowercase enum renames land on the wire
        assert_eq!(json["spec"]["mode"], "reactive");
        assert_eq!(json["spec"]["substrate"], "kata-hybrid");
        assert_eq!(json["kind"], "Agent");
        // round-trip back
        let back: Agent = serde_json::from_value(json).unwrap();
        assert_eq!(back.spec.mode, Mode::Reactive);
        assert_eq!(back.spec.substrate, Some(Substrate::KataHybrid));
    }
}
