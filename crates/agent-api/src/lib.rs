// SPDX-License-Identifier: Apache-2.0
//! # agent-api
//!
//! The agentctl custom-resource types — `Agent` and `AgentFleet`. These are
//! **contract-shaped**: a CR describes contract-level intent (mode, surfaces to
//! expose, intelligence/MCP bindings, substrate), not any agent's internals. The
//! reference agent binary is one implementation; these types never reference it.
//!
//! Generated as kube-rs [`kube::CustomResource`]s. CRD YAML is produced via
//! [`kube::CustomResourceExt::crd`].
//!
//! A single version (`v1alpha1`) is served at a time, with conversion handled out
//! of band — the CRD `apiVersion` clock is decoupled from the agent
//! `contract_version` clock.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Group for all agentctl CRDs.
pub const GROUP: &str = "agentctl.dev";

// ===========================================================================
// Agent
// ===========================================================================

/// One logical agent: an instruction + bindings rendered to a workload whose
/// shape follows `mode` (once→Job, schedule→CronJob, loop/reactive→Deployment).
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
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
#[schemars(extend("x-kubernetes-validations" = [
    {
        "rule": "self.mode != 'schedule' || has(self.schedule)",
        "message": "schedule is required when mode is 'schedule'"
    },
    {
        "rule": "self.mode != 'workflow' || has(self.workflow)",
        "message": "workflow is required when mode is 'workflow'"
    }
]))]
pub struct AgentSpec {
    /// The run shape. Determines the rendered workload kind.
    pub mode: Mode,

    /// The conformant-agent image to run. When omitted, the operator falls back
    /// to its configured **default agent image** (`operator.defaultAgentImage` /
    /// `AGENTCTL_DEFAULT_AGENT_IMAGE`); an explicit value here always overrides
    /// that default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// The agent's declared model id (metadata; the real binding is `modelPool`).
    /// Surfaced as a printer column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Inline instruction. Required for non-reactive modes (CEL/admission).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,

    /// The reusable MCP tool-server bundles this agent binds, by `MCPServerSet`
    /// name (same namespace). The MCPGateway scopes the agent to exactly these
    /// servers and injects each one's credential off-pod.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<String>,

    /// Substrate selection. Absent ⇒ the cluster default (hostile tenancy forces
    /// `kata-hybrid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substrate: Option<Substrate>,

    /// Which control-plane surfaces to expose. The operator drives only what the
    /// agent actually advertises in its manifest (graceful degradation),
    /// intersected with this desired set.
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

    /// The workflow graph to drive. Required when `mode: workflow`; also valid
    /// alongside `mode: reactive` (a suspend/resume daemon graph). Source is
    /// inline JSON or a ConfigMap key; the operator materializes it to a mounted
    /// file passed as `--workflow <path>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowSource>,

    /// Resolved limits/budgets (override the agent defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,

    /// **Declared capability**: the agent requests in-sandbox command execution.
    /// The admission webhook gates this — it is a privileged capability and one
    /// leg of the lethal trifecta, never granted implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<bool>,

    /// **Declared capability**: the agent requests outbound network egress. The
    /// admission webhook gates this — combined with `exec` and `secrets` it forms
    /// the lethal trifecta and triggers the override gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<bool>,

    /// **Declared capability**: the names of namespace-local `Secret`s the agent
    /// may read. The admission webhook validates each requested name against
    /// policy; access to untrusted private data is a trifecta leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<String>>,

    /// **Declared capability**: the `ModelPool` this agent binds for model access.
    /// The admission webhook validates the binding (the pool exists / is permitted
    /// for this tenant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_pool: Option<String>,

    /// Declarative per-agent access policy (authn/authz at the A2A gateway).
    /// `public` is documentation-only; `oidc` configures JWT verification and
    /// claim-based authorization for inbound A2A calls.
    #[serde(rename = "access", default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
}

/// Per-agent access policy for the A2A surface.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    /// Whether this agent is served publicly via the A2A gateway. Documentation-
    /// only (no enforcement yet); records intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    /// OIDC/JWT authentication + authorization for inbound A2A calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OidcAccess>,
}

/// OIDC/JWT verification + claim-based authorization config for the A2A gateway.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OidcAccess {
    /// OIDC issuer URL. JWKS is auto-discovered from
    /// `issuer/.well-known/openid-configuration` unless `jwks_uri` is set.
    pub issuer: String,
    /// Accepted `aud` (audience) claims.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audiences: Vec<String>,
    /// Explicit JWKS URI override (skips OIDC discovery when set).
    #[serde(rename = "jwksUri", default, skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    /// Authorization: ALL listed claim requirements must hold for the caller.
    #[serde(
        rename = "requiredClaims",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub required_claims: Option<Vec<ClaimRequirement>>,
    /// Inject the caller's `sub`/`email`/`groups` identity to the agent.
    #[serde(
        rename = "forwardIdentity",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub forward_identity: Option<bool>,
}

/// A single claim-based authorization requirement.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequirement {
    /// The claim name, e.g. `"groups"` or `"email"`.
    pub claim: String,
    /// The caller's claim (array contains OR scalar equals) must be one of these.
    #[serde(rename = "anyOf", default, skip_serializing_if = "Vec::is_empty")]
    pub any_of: Vec<String>,
}

/// The run shape.
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
    /// Drive a declarative **workflow** graph instead of a single instruction
    /// (`--mode workflow --workflow <file>`). Supervised like `once` (→ Job): same
    /// exit-code table, the result carries the workflow outcome. Requires
    /// `spec.workflow`. A `reactive` daemon may ALSO carry a workflow (a
    /// suspend/resume graph) — that is `mode: reactive` + `spec.workflow`, not
    /// this mode.
    Workflow,
}

/// Substrate tier. Names are the canonical tier ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Substrate {
    /// Stock Kubernetes pods (dev / single-tenant).
    StockUnix,
    /// Kata Containers isolation (hardened; default for hostile multi-tenant prod).
    KataHybrid,
    /// Per-pod sidecar (most portable; weakest isolation).
    SidecarEmptydir,
}

/// Which control-plane surfaces an `Agent` wants exposed.
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

/// Where an agent's workflow graph comes from. Exactly one of `inline` /
/// `configMapKeyRef` is set (CEL). The operator materializes it to a file mounted
/// into the pod and passed as `--workflow`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "has(self.inline) != has(self.configMapKeyRef)",
    "message": "workflow needs exactly one of inline or configMapKeyRef"
}]))]
pub struct WorkflowSource {
    /// The workflow graph as an inline JSON string. The operator renders it into
    /// a generated ConfigMap and mounts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
    /// A ConfigMap key holding the workflow JSON (mounted directly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_key_ref: Option<ConfigMapKeyRef>,
}

/// A reference to a specific key within a namespace-local `ConfigMap`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapKeyRef {
    /// The `ConfigMap` name (same namespace as the `Agent`).
    pub name: String,
    /// The key within the `ConfigMap`'s data holding the workflow JSON.
    pub key: String,
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
}

/// `Agent.status` — a curated projection of the live capabilities manifest +
/// health, never a raw manifest dump.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    /// The conditions taxonomy: `Validated`, `Rendered`, `Ready`, `Draining`,
    /// `Degraded`, plus advisory `TrifectaUnionObserved`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// The `.metadata.generation` this status reflects (hot-loop guard).
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

/// A status condition. Mirrors `metav1.Condition` shape.
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
    /// An ISO 8601 / UTC timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

// ===========================================================================
// AgentFleet
// ===========================================================================

/// A replicated, autoscaled set of agents. Renders to a StatefulSet (shard mode)
/// or Deployment (claim mode); **KEDA owns `.spec.replicas`**, so the rendered
/// workload omits it.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha1",
    kind = "AgentFleet",
    namespaced,
    status = "AgentFleetStatus",
    shortname = "afleet",
    shortname = "afleets",
    printcolumn = r#"{"name":"Scaling","type":"string","jsonPath":".spec.scaling.mode"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    scale(
        spec_replicas_path = ".spec.replicas",
        status_replicas_path = ".status.replicas",
        label_selector_path = ".status.selector"
    )
)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {
        "rule": "self.scaling.mode != 'shard' || has(self.scaling.shards)",
        "message": "shards is required when scaling.mode is 'shard'"
    },
    {
        // A coordinator ("main agent") is a long-lived front door — a `once`
        // coordinator would exit and never serve.
        "rule": "!has(self.coordinator) || self.coordinator.template.mode != 'once'",
        "message": "coordinator.template.mode must not be 'once' (the coordinator must be long-lived)"
    }
]))]
pub struct AgentFleetSpec {
    /// The per-replica **worker** agent definition.
    pub template: AgentSpec,
    /// The **worker** scaling regime.
    pub scaling: Scaling,
    /// The shared work source (an MCP resource URI) for claim/shard distribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_source: Option<String>,
    /// Claim-mode **worker** replica count, the target of the `scale` subresource
    /// so `kubectl scale agentfleet` and an HPA can drive it. **KEDA owns this in
    /// steady state**; the rendered workload omits it. Optional so claim mode may
    /// scale to 0 / defer to KEDA when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,

    /// The fleet's **coordinator** ("main agent"). When set, the operator renders
    /// an additional single-role Deployment (label
    /// `agentctl.dev/fleet-role: coordinator`) and wires it as the fleet's A2A
    /// front door + work producer. Absent ⇒ a headless worker pool, load-balanced
    /// directly by the A2A gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<Coordinator>,

    /// Per-fleet model budget, enforced by the ModelGateway IN ADDITION to the
    /// `ModelPool` budget. Isolates one fleet's spend from another's even when
    /// they share a pool. Absent ⇒ only the pool cap applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<FleetBudget>,

    /// Work-fabric policy for this fleet's items: dead-letter threshold and the
    /// default lease TTL. Absent ⇒ unbounded redelivery + server-default TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_policy: Option<WorkPolicy>,
}

/// The fleet's coordinator ("main agent"). A normal conformant agent,
/// distinguished only by its role label and by the operator wiring it as a work
/// **producer** + A2A front door rather than a **consumer**.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Coordinator {
    /// The coordinator agent definition — a normal `AgentSpec` (its own image,
    /// instruction, mode, model, MCP tools). Typically `mode: reactive` (a
    /// long-lived planner that accepts A2A) or `mode: workflow`. Never `once`
    /// (CEL-enforced on the fleet).
    pub template: AgentSpec,

    /// Coordinator replica count. Default 1 (a singleton main agent). `>1` is
    /// allowed for HA, but the replicas are peers (not shards): they must
    /// coordinate through the work fabric like any other producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,

    /// How the coordinator reaches the workers. `queue` (default): the operator
    /// wires it as a producer on the fleet `workSource`; workers claim
    /// (load-balanced, elastic). `a2a`: the operator injects an
    /// `--a2a-peer worker=<gateway>/fleets/<ns>/<name>` so it delegates
    /// point-to-point through the gateway PEP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<Distribution>,
}

/// How a coordinator fans work out to its workers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Distribution {
    /// Fan-out over the `work.*` claim queue (elastic, load-balanced,
    /// exactly-one-owner, holder-attested internally). The default.
    #[default]
    Queue,
    /// Point-to-point delegation through the gateway PEP (`a2a.delegate`).
    A2a,
}

/// Per-fleet model budget (the intelligence plane).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetBudget {
    /// Total tokens this fleet may consume against its `ModelPool`, across all
    /// members. Enforced by the ModelGateway reservation path keyed by
    /// `(namespace, pool, fleet)`, IN ADDITION to the pool-wide cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
}

/// Work-fabric policy for a fleet's items.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkPolicy {
    /// Dead-letter an item after it has been redelivered this many times without
    /// a terminal `ack`. Absent ⇒ unbounded redelivery. A poison item is moved to
    /// the `deadletter` state (surfaced at `dlq://items`) instead of cycling
    /// forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,

    /// Default lease TTL (ms) the operator advertises to workers for this fleet's
    /// claims. Absent ⇒ the agent/server default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_ttl_ms: Option<u64>,
}

/// The scaling regime.
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
    /// Current replica count surfaced through the `scale` subresource
    /// (`statusReplicasPath = .status.replicas`). Populated by the operator from
    /// the rendered workload's observed replicas; HPA reads this back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,
    /// Serialized label selector for the `scale` subresource
    /// (`labelSelectorPath = .status.selector`); an HPA uses it to find the
    /// pods it scales. Populated by the operator with the workload's selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scale_time: Option<String>,
}

// ===========================================================================
// ModelPool
// ===========================================================================

/// A pool of model access (the intelligence plane). Agents hold **NO provider
/// secrets**; they dial the control plane keyless, and the control plane supplies
/// model access through a credential-injecting, metering, budget-enforcing proxy
/// (the `ModelGateway`) configured by this CRD. The pool names the
/// provider, its endpoint, the `Secret` holding the provider API key, the
/// allowed models, and an optional total token budget.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
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

// ===========================================================================
// MCPServerSet
// ===========================================================================

/// A reusable, namespaced bundle of MCP tool servers an `Agent`/`AgentFleet`
/// binds via `spec.mcpServers`. Agents hold **NO tool-server
/// credentials**: they never hold a tool server's token. The
/// control plane brokers every remote MCP server through a credential-injecting,
/// attesting, policy-enforcing proxy (the `MCPGateway`, the tool-plane analog of
/// the `ModelGateway`) configured by this CRD. Each server names its remote
/// endpoint, its auth (a `Secret`-backed token held at the gateway, never the
/// pod), its per-tool trifecta tags, and an optional call budget.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha1",
    kind = "MCPServerSet",
    namespaced,
    status = "MCPServerSetStatus",
    shortname = "mcpset",
    printcolumn = r#"{"name":"Servers","type":"integer","jsonPath":".status.serverCount"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {
        "rule": "self.servers.all(s, s.name != '')",
        "message": "every server needs a non-empty name"
    },
    {
        "rule": "self.servers.all(s, s.endpoint.startsWith('https://') || s.endpoint.startsWith('http://'))",
        "message": "each server endpoint must be an http(s):// URL"
    }
]))]
pub struct MCPServerSetSpec {
    /// The MCP tool servers this set bundles.
    pub servers: Vec<McpServer>,
}

/// One remote MCP tool server, brokered by the MCPGateway.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    /// Server name — the key an `Agent` references and the gateway facade path
    /// segment (`/s/<name>`). Unique within the resolved `Agent` union.
    pub name: String,

    /// The remote MCP server URL (Streamable HTTP transport). The AGENT never
    /// dials this — it dials the gateway, which dials this.
    pub endpoint: String,

    /// How the gateway authenticates to the remote server. The credential lives
    /// in a `Secret` the gateway reads; it is NEVER placed on the agent pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuth>,

    /// Per-tool trifecta capability tags the operator declares for the
    /// Rule-of-Two check. A bare list is shorthand for the `*` glob.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Optional per-server call budget (enforced by the gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

/// How the MCPGateway authenticates to a remote MCP server. The `staticToken`
/// bearer (a long-lived credential held off-pod at the gateway) is currently
/// supported; OAuth client-credentials / EMA tiers extend this enum.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpAuth {
    /// Auth mode. `none` (unauthenticated server) | `staticToken` (a bearer the
    /// gateway attaches upstream).
    #[serde(default)]
    pub mode: McpAuthMode,

    /// The bearer credential for `staticToken` mode: a `Secret` key the gateway
    /// reads and attaches as `Authorization: Bearer <value>` on the upstream
    /// hop. Required iff `mode == staticToken`. NEVER placed on the agent pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret_ref: Option<SecretKeyRef>,

    /// Header name to carry the token (default `Authorization` as
    /// `Bearer <value>`). A custom header (e.g. `X-API-Key`) sends the raw value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

/// The MCP-server auth mode enum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum McpAuthMode {
    /// The remote server needs no credential.
    #[default]
    None,
    /// A `Secret`-backed bearer the gateway attaches upstream (off-pod).
    StaticToken,
}

/// `MCPServerSet.status` — resolution + per-server broker health.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MCPServerSetStatus {
    /// Number of servers in the set (printer column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_count: Option<i64>,
    /// The union of trifecta tags across all servers (informational; the
    /// Agent-level union is the gate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tag_union: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn agent_crd_generates() {
        let crd = Agent::crd();
        assert_eq!(crd.spec.group, "agentctl.dev");
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
        assert_eq!(crd.spec.group, "agentctl.dev");
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
    fn mcpserverset_crd_generates() {
        let crd = MCPServerSet::crd();
        assert_eq!(crd.spec.group, "agentctl.dev");
        assert_eq!(crd.spec.names.kind, "MCPServerSet");
        assert!(crd
            .spec
            .names
            .short_names
            .as_ref()
            .unwrap()
            .contains(&"mcpset".to_string()));
        let v = &crd.spec.versions[0];
        assert!(v
            .subresources
            .as_ref()
            .and_then(|s| s.status.as_ref())
            .is_some());
    }

    #[test]
    fn mcpserverset_roundtrips_with_static_token_auth() {
        // A server with a Secret-backed bearer + tags parses back to the same spec.
        let spec = MCPServerSetSpec {
            servers: vec![McpServer {
                name: "github".into(),
                endpoint: "https://mcp.example.com/mcp".into(),
                auth: Some(McpAuth {
                    mode: McpAuthMode::StaticToken,
                    token_secret_ref: Some(SecretKeyRef {
                        name: "gh-mcp-token".into(),
                        key: "token".into(),
                    }),
                    header: None,
                }),
                tags: vec!["untrusted_input".into(), "egress".into()],
                budget: Some(Budget { max_tokens: 1000 }),
            }],
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["servers"][0]["auth"]["mode"], "staticToken");
        assert_eq!(
            json["servers"][0]["auth"]["tokenSecretRef"]["name"],
            "gh-mcp-token"
        );
        let back: MCPServerSetSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.servers[0].name, "github");
        assert_eq!(
            back.servers[0].auth.as_ref().unwrap().mode,
            McpAuthMode::StaticToken
        );
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
