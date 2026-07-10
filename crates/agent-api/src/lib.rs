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
    category = "agentctl",
    printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".spec.mode"}"#,
    printcolumn = r#"{"name":"Pool","type":"string","jsonPath":".spec.model.pool","priority":1}"#,
    printcolumn = r#"{"name":"Model","type":"string","jsonPath":".spec.model.id","priority":1}"#,
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
    },
    {
        // once/loop/schedule drive a single instruction; reactive wakes on a
        // source and workflow drives a graph, so those two need no instruction.
        "rule": "self.mode == 'reactive' || self.mode == 'workflow' || has(self.instruction)",
        "message": "instruction is required for once/loop/schedule modes"
    },
    {
        // A reactive agent needs a wake source: subscriptions, a workflow graph,
        // or the a2a surface (an A2A-driven coordinator/agent has neither
        // subscribe nor workflow — inbound calls are its trigger).
        "rule": "self.mode != 'reactive' || (has(self.subscribe) && self.subscribe.size() > 0) || has(self.workflow) || (has(self.surfaces) && has(self.surfaces.a2a) && self.surfaces.a2a)",
        "message": "reactive mode requires subscribe, a workflow, or the a2a surface (a wake source)"
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

    /// The intelligence binding: `pool` names the `ModelPool` this agent draws
    /// keyless model access from (the admission-validated binding), and `id` is
    /// the model chosen within that pool (metadata/default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelBinding>,

    /// Inline instruction. Required for non-reactive modes (CEL/admission).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,

    /// The remote MCP tool servers this agent dials **directly** (inline; no
    /// broker). Each names its endpoint + auth (`aauth` = the agent signs
    /// itself, secret-free; `staticToken` = a bearer mounted onto the pod).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServer>,

    /// Substrate tier. Absent ⇒ `stock-unix`, which is the **only** tier the
    /// operator renders today. `kata-hybrid` / `sidecar-emptydir` are declared
    /// roadmap tiers (the locked hardening direction for hostile multi-tenancy);
    /// selecting one is rejected at render until implemented.
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

    /// The privileged capability grants — `exec` / `egress` / `secrets` — the
    /// admission gate evaluates **as a union** (the lethal trifecta). Grouped so
    /// the grants read as one reviewable block (mirrors k8s `securityContext`).
    /// **Declared intent, enforced at admission only**: the operator does not
    /// itself mount the `Secret`s, drive the egress `NetworkPolicy`, or pass
    /// `exec` to the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Capabilities>,

    /// Declarative per-agent access policy (authn/authz at the A2A gateway):
    /// `oidc` configures JWT verification and claim-based authorization for
    /// inbound A2A calls. Exposure itself is governed by `surfaces.a2a`.
    #[serde(rename = "access", default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,

    /// Portable agent identity (RFC 0023). `aauth` opts the agent into an
    /// AAuth identity the operator provisions and lifecycle-manages at an
    /// Agent Provider. **Experimental, default-off** — absent ⇒ rendering is
    /// byte-identical to today. Grouped so future identity systems slot
    /// beside `aauth` rather than flattening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
}

/// Portable-identity opt-ins for an agent (RFC 0023). One member today.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// AAuth identity (`aauth:local@domain`): the operator provisions a
    /// per-Agent Ed25519 key, pre-registers its thumbprint at the Agent
    /// Provider (allowlist enrollment), and the agent self-enrolls at startup
    /// — no secret beyond the agent's own key ever reaches the pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aauth: Option<AauthIdentity>,
}

/// The AAuth identity opt-in. An empty object is valid: the operator's
/// configured default provider applies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AauthIdentity {
    /// The Agent Provider issuer URL (`https://…`). Absent ⇒ the operator's
    /// configured default (`AGENTCTL_AAUTH_PROVIDER`). The admission webhook
    /// denies the opt-in when neither is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// RESERVED (AAuth Case C, user-scoped access via a Person Server). Not
    /// rendered in v1; accepted so specs written for the roadmap stay valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_server: Option<String>,
}

/// The intelligence binding for an `Agent`: which `ModelPool` supplies keyless
/// model access, and which model id to request within it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelBinding {
    /// The `ModelPool` (same namespace) this agent binds for model access. The
    /// admission webhook validates it (the pool exists / is permitted for this
    /// tenant). This is the real, load-bearing binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// The model id to request within the pool. Metadata/default — when unset the
    /// pool's own `defaultModel` applies. Surfaced as a printer column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// The privileged capability grants (the "lethal trifecta"), grouped because the
/// admission gate evaluates them as a union: `exec` && `egress` && a non-empty
/// `secrets` together trip the override gate. **Declared intent, enforced at
/// admission only** — the operator wires none of these downstream.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// The agent requests in-sandbox command execution. Declared intent; gated at
    /// admission (a privileged trifecta leg), never wired by the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<bool>,
    /// The agent requests outbound network egress. Declared intent; gated at
    /// admission (a trifecta leg), never wired by the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<bool>,
    /// The namespace-local `Secret` names the agent may read. Declared intent;
    /// each name is validated at admission (a trifecta leg); the operator does
    /// not itself mount them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<String>>,
}

/// Per-agent access policy for the A2A surface.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Access {
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
    /// Stock Kubernetes pods (dev / single-tenant). The only tier the operator
    /// renders today; the default when `substrate` is absent.
    StockUnix,
    /// Kata Containers isolation (hardened; the locked direction for hostile
    /// multi-tenant prod). **Roadmap — not yet rendered** (render rejects it).
    KataHybrid,
    /// Per-pod sidecar (most portable; weakest isolation). **Roadmap — not yet
    /// rendered** (render rejects it).
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
    /// Per-RUN token box (each once-run / each reaction). Renders `--max-tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u64>,
    /// Per-INSTANCE lifetime token budget — cumulative across every run/reaction
    /// of this instance (RFC 0025). Renders `--budget-tokens-lifetime`. On a
    /// bounded `once` run exhaustion folds into `EXIT_BUDGET(7)`; a
    /// `reactive`/`loop`/`schedule` daemon stops accepting new reactions and
    /// drains cleanly (exit 0 by default, operator-tunable via the agent's
    /// `--budget-exit-code`). Each instance (fleet member included) gets its own
    /// lifetime box — the harness is the only token-budget enforcement point now
    /// that the metering gateway is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_tokens: Option<u64>,
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
    /// Provisioned portable identity (RFC 0023), learned after the agent
    /// enrolls at its Agent Provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentityStatus>,
}

/// Provisioned portable identities, mirroring `spec.identity`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdentityStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aauth: Option<AauthIdentityStatus>,
}

/// The enrolled AAuth identity as learned from the Agent Provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AauthIdentityStatus {
    /// The stable agent identifier, e.g. `aauth:k7q3p9n2@ap.example`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The Agent Provider issuer URL the identity is enrolled at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// RFC 3339 time the operator first observed the enrollment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrolled_at: Option<String>,
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
    category = "agentctl",
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
    /// The fleet's shared **work fabric**: the work source (an MCP resource URI
    /// for claim/shard distribution) plus the redelivery/lease policy the
    /// operator advertises to the coordinator. Absent ⇒ no shared source +
    /// server-default redelivery/TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<Work>,
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
    /// wires it as a producer on the fleet `work.source`; workers claim
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

/// The fleet's shared work fabric: the work source plus the redelivery/lease
/// policy the operator advertises to the coordinator.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Work {
    /// The shared work source — an MCP resource URI (e.g. `queue://jobs`) for
    /// claim/shard distribution. The operator delivers it to the coordinator
    /// (as `AGENT_FLEET_WORKSOURCE`) and to the KEDA scaler's backlog probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Dead-letter an item after it has been redelivered this many times without
    /// a terminal `ack`. Absent ⇒ unbounded redelivery. A poison item is moved to
    /// the `deadletter` state (surfaced at `dlq://items`) instead of cycling
    /// forever. Delivered to the coordinator as `AGENT_FLEET_MAX_ATTEMPTS`, which
    /// a conformant coordinator stamps onto each `work.submit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,

    /// Default claim lease TTL as a Go-duration string (e.g. `"30s"`), matching
    /// `loop.interval` / `loop.deadline`. Absent ⇒ the server default. Delivered
    /// to the coordinator as `AGENT_FLEET_CLAIM_TTL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_ttl: Option<String>,
}

/// The scaling regime.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Scaling {
    pub mode: ScaleMode,
    /// Claim mode: minimum replicas (KEDA-elastic range). Unset ⇒ may scale to 0.
    #[serde(
        rename = "minReplicas",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub min_replicas: Option<u32>,
    /// Claim mode: maximum replicas.
    #[serde(
        rename = "maxReplicas",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_replicas: Option<u32>,
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
    pub metric: String,
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

/// A named model endpoint the intelligence plane dials **directly** (no broker).
/// The pool records the provider, its endpoint, and the allowed models; an
/// `Agent` binds it via `spec.model.pool` and the operator renders
/// `INTELLIGENCE=<endpoint>` straight into the pod. The preferred authentication
/// is the agent's own **AAuth** identity (`spec.identity.aauth`), so the pod
/// holds no provider secret. For a key-authenticated provider,
/// `credentialSecretRef` is mounted onto the agent as its `INTELLIGENCE_TOKEN`
/// env-secret (the agent then holds that key — there is no off-pod broker).
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha1",
    kind = "ModelPool",
    namespaced,
    status = "ModelPoolStatus",
    shortname = "mp",
    category = "agentctl",
    printcolumn = r#"{"name":"Provider","type":"string","jsonPath":".spec.provider"}"#,
    printcolumn = r#"{"name":"Endpoint","type":"string","jsonPath":".spec.endpoint","priority":1}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ModelPoolSpec {
    /// Provider id, e.g. `"mock"` | `"anthropic"` | `"openai"` (free string).
    pub provider: String,

    /// Provider base URL the agent dials directly, e.g.
    /// `https://api.anthropic.com` — rendered into the pod as `INTELLIGENCE`.
    pub endpoint: String,

    /// OPTIONAL provider API-key `Secret`. Present ⇒ the operator mounts it onto
    /// the agent as its `INTELLIGENCE_TOKEN` env-secret (the agent holds the
    /// key — there is no off-pod broker). Absent ⇒ the agent authenticates by
    /// its AAuth identity (the secret-free path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_secret_ref: Option<SecretKeyRef>,

    /// Allowed model ids for this pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,

    /// Default model id, used when a request does not pin one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

/// A reference to a specific key within a namespace-local `Secret`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    /// The `Secret` name (same namespace as the referencing resource).
    pub name: String,
    /// The key within the `Secret`'s data holding the credential.
    pub key: String,
}

/// `ModelPool.status` — health of the model binding.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelPoolStatus {
    /// Health conditions (`Ready`, …); shares the `conditions` + `Ready`-column
    /// idiom with `Agent`/`AgentFleet`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

// ===========================================================================
// MCP servers (inline on the Agent — no broker CRD)
// ===========================================================================

/// One remote MCP tool server the agent dials **directly** (Streamable HTTP).
/// Declared inline on `Agent.spec.mcpServers`; there is no gateway facade.
/// Preferred auth is the agent's own **AAuth** identity (`mode: aauth`, the
/// secret-free path); `staticToken` mounts a bearer onto the agent pod for a
/// server that only takes a key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    /// Server name — the agent's `--mcp <name>=<endpoint>` key. Unique per Agent.
    pub name: String,

    /// The remote MCP server URL (Streamable HTTP transport) the agent dials.
    pub endpoint: String,

    /// How the agent authenticates to the server (default: `none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuth>,

    /// Per-tool trifecta capability tags (the Rule-of-Two admission check).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// How the agent authenticates to a remote MCP server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpAuth {
    /// Auth mode: `none` | `staticToken` (a bearer mounted onto the agent) |
    /// `aauth` (the agent signs its own requests — secret-free).
    #[serde(default)]
    pub mode: McpAuthMode,

    /// The bearer for `staticToken` mode: a `Secret` key the operator mounts
    /// onto the agent, which attaches it upstream. (The agent holds the key —
    /// there is no off-pod broker.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret_ref: Option<SecretKeyRef>,

    /// Header name to carry the token (default `Authorization: Bearer <value>`).
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
    /// A `Secret`-backed bearer the operator mounts onto the agent.
    StaticToken,
    /// The **agent authenticates itself** (AAuth): the server verifies the
    /// agent's RFC 9421-signed requests against its Agent Provider — no
    /// credential exists to hold. Requires the Agent to carry `identity.aauth`
    /// and declare `capabilities.egress` (admission-enforced).
    Aauth,
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
    fn inline_mcp_server_roundtrips_with_static_token_and_aauth() {
        // staticToken (bearer mounted on the agent) round-trips.
        let s = McpServer {
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
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["auth"]["mode"], "staticToken");
        assert_eq!(json["auth"]["tokenSecretRef"]["name"], "gh-mcp-token");
        let back: McpServer = serde_json::from_value(json).unwrap();
        assert_eq!(back.auth.unwrap().mode, McpAuthMode::StaticToken);
        // aauth (agent signs itself) — no token.
        let a: McpServer = serde_json::from_value(serde_json::json!({
            "name": "secure", "endpoint": "https://mcp.secure/mcp",
            "auth": { "mode": "aauth" }
        }))
        .unwrap();
        assert_eq!(a.auth.unwrap().mode, McpAuthMode::Aauth);
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
                model: Some(ModelBinding {
                    pool: Some("gpt".into()),
                    id: Some("gpt-4o-mini".into()),
                }),
                capabilities: Some(Capabilities {
                    exec: Some(false),
                    egress: Some(true),
                    secrets: Some(vec!["db-creds".into()]),
                }),
                identity: Some(Identity {
                    aauth: Some(AauthIdentity {
                        provider: Some("https://ap.example.com".into()),
                        person_server: Some("https://ps.example.com".into()),
                    }),
                }),
                ..Default::default()
            },
        );
        let json = serde_json::to_value(&a).unwrap();
        // camelCase + kebab/lowercase enum renames land on the wire
        assert_eq!(json["spec"]["mode"], "reactive");
        assert_eq!(json["spec"]["substrate"], "kata-hybrid");
        assert_eq!(json["kind"], "Agent");
        // grouped bindings land under model{} / capabilities{} / identity{}
        assert_eq!(json["spec"]["model"]["pool"], "gpt");
        assert_eq!(json["spec"]["model"]["id"], "gpt-4o-mini");
        assert_eq!(json["spec"]["capabilities"]["egress"], true);
        assert_eq!(json["spec"]["capabilities"]["secrets"][0], "db-creds");
        assert_eq!(
            json["spec"]["identity"]["aauth"]["provider"],
            "https://ap.example.com"
        );
        assert_eq!(
            json["spec"]["identity"]["aauth"]["personServer"],
            "https://ps.example.com"
        );
        // round-trip back
        let back: Agent = serde_json::from_value(json).unwrap();
        assert_eq!(back.spec.mode, Mode::Reactive);
        assert_eq!(back.spec.substrate, Some(Substrate::KataHybrid));
        assert_eq!(back.spec.model.unwrap().pool.as_deref(), Some("gpt"));
        assert_eq!(back.spec.capabilities.unwrap().exec, Some(false));
        assert_eq!(
            back.spec
                .identity
                .unwrap()
                .aauth
                .unwrap()
                .person_server
                .as_deref(),
            Some("https://ps.example.com")
        );
    }

    #[test]
    fn identity_status_uses_camelcase_enrolled_at() {
        let s = AgentStatus {
            identity: Some(IdentityStatus {
                aauth: Some(AauthIdentityStatus {
                    agent: Some("aauth:k7q3p9n2@ap.example".into()),
                    provider: Some("https://ap.example.com".into()),
                    enrolled_at: Some("2026-07-09T12:00:00Z".into()),
                }),
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(
            json["identity"]["aauth"]["agent"],
            "aauth:k7q3p9n2@ap.example"
        );
        assert_eq!(
            json["identity"]["aauth"]["enrolledAt"],
            "2026-07-09T12:00:00Z"
        );
    }

    #[test]
    fn fleet_work_and_scaling_use_grouped_camelcase_keys() {
        let f = AgentFleet::new(
            "workers",
            AgentFleetSpec {
                template: AgentSpec {
                    mode: Mode::Reactive,
                    subscribe: vec!["queue://jobs".into()],
                    ..Default::default()
                },
                scaling: Scaling {
                    mode: ScaleMode::Claim,
                    min_replicas: Some(0),
                    max_replicas: Some(10),
                    target: Some(ScaleTarget {
                        metric: "pending_events".into(),
                        value: "5".into(),
                    }),
                    ..Default::default()
                },
                work: Some(Work {
                    source: Some("queue://jobs".into()),
                    max_attempts: Some(5),
                    claim_ttl: Some("30s".into()),
                }),
                ..Default::default()
            },
        );
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["spec"]["scaling"]["minReplicas"], 0);
        assert_eq!(json["spec"]["scaling"]["maxReplicas"], 10);
        assert_eq!(
            json["spec"]["scaling"]["target"]["metric"],
            "pending_events"
        );
        assert_eq!(json["spec"]["work"]["source"], "queue://jobs");
        assert_eq!(json["spec"]["work"]["maxAttempts"], 5);
        assert_eq!(json["spec"]["work"]["claimTtl"], "30s");
        let back: AgentFleet = serde_json::from_value(json).unwrap();
        assert_eq!(back.spec.scaling.min_replicas, Some(0));
        assert_eq!(back.spec.work.unwrap().claim_ttl.as_deref(), Some("30s"));
    }
}
