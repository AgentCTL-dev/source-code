// SPDX-License-Identifier: Apache-2.0
//! # agentctl.dev/v1alpha2 — the provisioning-v2 API family (RFC 0033)
//!
//! The v2 kinds: [`Agent`] (triggers over all ten agentd start kinds, class
//! chain, service grants), [`AgentTemplate`] (instantiable spec + params),
//! [`AgentClass`] (the scoped defaults bundle, RFC 0032), [`MCPService`] (the
//! capability registry entry), and [`Supervisor`] (RFC 0027 §2).
//! `Organization` is already v1alpha2-native in [`crate::org`].
//!
//! **Conversion** (P2-1b): v1alpha1 → v1alpha2 is LOSSLESS
//! ([`convert::agent_v1_to_v2`] — mode/subscribe/loop/schedule/workflow fold
//! into `shape` + `triggers[]` + `workflows[]`, with warnings for the
//! deprecated spellings); v1alpha2 → v1alpha1 is lossy for v2-only fields
//! (class/services/skills/store/peers/approval), which is why **v1alpha2 is
//! the storage version** — the down-conversion exists only for old readers
//! and warns about every dropped field.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::Condition;

/// Schema for free-form JSON fields (`serde_json::Value`): schemars 1.x emits
/// a `true` schema for `Value`, which the structural-CRD validator rejects —
/// emit an `x-kubernetes-preserve-unknown-fields` object instead.
fn preserve_arbitrary(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "x-kubernetes-preserve-unknown-fields": true
    })
}

fn preserve_arbitrary_vec(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "items": { "x-kubernetes-preserve-unknown-fields": true }
    })
}

// ===========================================================================
// Agent
// ===========================================================================

/// One logical agent, provisioning-v2 shape: an instruction + typed triggers
/// + scoped bindings, rendered to the workload `shape` dictates.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha2",
    kind = "Agent",
    namespaced,
    status = "AgentStatus",
    category = "agentctl",
    printcolumn = r#"{"name":"Shape","type":"string","jsonPath":".spec.shape"}"#,
    printcolumn = r#"{"name":"Class","type":"string","jsonPath":".spec.class","priority":1}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {
        // A job-shaped agent runs to completion; a daemon needs a wake
        // source: at least one trigger, or the exposed a2a surface.
        "rule": "self.shape != 'daemon' || (has(self.triggers) && self.triggers.size() > 0) || (has(self.expose) && has(self.expose.a2a) && self.expose.a2a)",
        "message": "a daemon needs a wake source: triggers[] or expose.a2a"
    },
    {
        "rule": "self.shape != 'cron' || has(self.schedule) || (has(self.triggers) && self.triggers.exists(t, has(t.schedule)))",
        "message": "cron shape needs spec.schedule or a schedule trigger"
    }
]))]
pub struct AgentSpec {
    /// The `AgentClass` this agent resolves defaults/floors through
    /// (RFC 0032 scope chain). Absent ⇒ the org's `default` class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// Org-unique @handle (DNS-1123; defaults to the CR name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// Human-facing display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    /// Runtime selection (agentd version/digest via class; explicit image
    /// wins — the v1 `spec.image` compatibility path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeSelector>,

    /// The rendered workload shape — ALWAYS explicit after defaulting
    /// (inference per RFC 0033 §2.2: any long-lived trigger ⇒ daemon;
    /// once/manual only ⇒ job; a sole schedule trigger ⇒ cron).
    #[serde(default)]
    pub shape: Shape,
    /// Cron expression for `shape: cron` (the external-CronJob path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,

    /// The instruction document (prose or agentd directive document).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<Instruction>,

    /// Typed triggers over agentd's ten start kinds (RFC 0033 §2.2); the
    /// renderer compiles each into a generated `main-<kind>` workflow.
    /// Bounded (CEL rule-cost budget needs a maxItems).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub triggers: Vec<Trigger>,

    /// Explicit workflow documents (compose WITH trigger sugar).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<WorkflowSource>,
    /// Skill bundle references (SkillSet names; P2-3 projects the folder).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<BundleRef>,

    /// Service GRANTS: `MCPService` references with per-agent narrowing
    /// (allow lists shrink the registry entry's ceiling, never widen).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceGrant>,
    /// Inline direct-dial MCP servers — the v1 compatibility spelling.
    /// DEPRECATED in v2: prefer an `MCPService` + `services[]` grant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<crate::McpServer>,

    /// Intelligence binding (pool + tier/model + budget).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence: Option<Intelligence>,
    /// Durable-state class (`managed` = the state service; `local` = PVC;
    /// `ephemeral` = emptyDir). Absent ⇒ class default (ephemeral).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<StoreSpec>,

    /// Workload identity + delegation posture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentitySpec>,
    /// Per-agent access policy (named principals, per-agent OIDC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<crate::Access>,

    /// What the gateway exposes for this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose: Option<Expose>,
    /// East-west peer wiring (same-org agent handles).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<PeerRef>,

    /// Lifecycle knobs (explicit run_until; declarative pause).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<LifecycleSpec>,
    /// Run/step/token ceilings (narrowing over class).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::Limits>,
    /// Dangerous-capability legs (trifecta gating at admission).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<crate::Capabilities>,
    /// Scheduling priority band.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    /// Approval / HITL policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,

    /// Substrate tier (v1 compatibility; stock-unix is the rendered tier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substrate: Option<crate::Substrate>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    #[default]
    Daemon,
    Job,
    Cron,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSelector {
    /// Runtime version tag (resolved to a digest via class policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Explicit image reference (wins over version resolution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Instruction {
    /// Inline instruction text (prose or an agentd directive document).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A ConfigMap key holding the instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_ref: Option<crate::ConfigMapKeyRef>,
}

/// One trigger — a typed union over agentd's ten start kinds. EXACTLY ONE
/// member is set (CEL-enforced); the renderer compiles it into a
/// `main-<kind>` start-node workflow.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {
        // Written as a sum of conditionals (not list+filter) to stay inside
        // the apiserver's CEL rule-cost budget; `loop` is CEL-reserved and
        // escapes as `__loop__`.
        "rule": "(has(self.once)?1:0) + (has(self.manual)?1:0) + (has(self.__loop__)?1:0) + (has(self.schedule)?1:0) + (has(self.webhook)?1:0) + (has(self.subscribe)?1:0) + (has(self.stream)?1:0) + (has(self.signal)?1:0) + (has(self.event)?1:0) + (has(self.a2aCommand)?1:0) == 1",
        "message": "exactly one trigger kind must be set"
    }
]))]
pub struct Trigger {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub once: Option<OnceTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<ManualTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "loop")]
    pub loop_: Option<LoopTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<SubscribeTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<SignalTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<EventTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_command: Option<A2aCommandTrigger>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct OnceTrigger {}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct ManualTrigger {}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoopTrigger {
    /// Cadence (`30s`, `5m`, …).
    pub interval: String,
    /// Optional stop condition (deadline duration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleTrigger {
    /// Cron expression (`0 7 * * 1-5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// Interval sugar (`1h`) — compiled to cron.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    /// Timezone (default UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    /// Where the schedule runs: `external` (CronJob — the default for a sole
    /// schedule trigger) or `internal` (an agentd schedule start in a daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ScheduleRuntime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleRuntime {
    External,
    Internal,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookTrigger {
    /// Listener path (`/zendesk`).
    pub path: String,
    /// `hmac` (default — the operator provisions the route secret) or
    /// `bearer`. agentd refuses unauthenticated routes on its non-loopback
    /// webhook listener, so there is no `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// `<burst>/<per>` arrival rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
    /// Idempotency-key header name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeTrigger {
    /// The granted `MCPService` the resource lives on (admission refuses a
    /// subscribe against an ungranted service).
    pub service: String,
    /// The MCP resource URI to watch.
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StreamTrigger {
    /// The declared stream name.
    pub stream: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SignalTrigger {
    /// Signal name (`SIGUSR1`, or an agentd-defined logical signal).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventTrigger {
    /// A runtime event from agentd's closed vocabulary. Named `name` (not the
    /// RFC's original `on`): YAML 1.1 parses a bare `on:` key as boolean
    /// `true`, which corrupts both the CRD schema and every user manifest
    /// (the GitHub-Actions trap). RFC 0033 §2.2 updated to match.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct A2aCommandTrigger {
    /// The typed command name registered on the A2A listener.
    pub command: String,
    /// JSON-schema for the command's DataPart payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "preserve_arbitrary")]
    pub schema: Option<serde_json::Value>,
    /// Principal roles that may invoke it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSource {
    /// A WorkflowSet bundle reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_ref: Option<String>,
    /// An inline dialect-3 workflow document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "preserve_arbitrary")]
    pub inline: Option<serde_json::Value>,
    /// A ConfigMap key holding a workflow document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_ref: Option<crate::ConfigMapKeyRef>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleRef {
    pub set_ref: String,
}

/// A grant against a registry `MCPService`: the agent may narrow the entry's
/// ceilings (allow ⊆ registry allow), never widen — the P2-2 resolver and
/// admission both enforce it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceGrant {
    /// The `MCPService` name (scope-chain resolved).
    pub name: String,
    /// Tool-name patterns this agent may call (narrows the entry's allow).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Intelligence {
    /// The `ModelPool` binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Model tier within the pool (`small` | `large` | pool-defined).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Explicit model id (wins over tier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Token budget (lifetime + windows; narrows the class ceiling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<BudgetWindow>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetWindow {
    /// `hour` | `day` | `week` | `month`.
    pub per: String,
    pub tokens: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StoreSpec {
    /// `managed` (state service) | `local` (PVC) | `ephemeral` (emptyDir).
    #[serde(default)]
    pub class: StoreClass,
    /// Retention window for `delete` (`720h`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<String>,
    /// `local` class only: the PVC size (default `1Gi`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StoreClass {
    #[default]
    Ephemeral,
    Local,
    Managed,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdentitySpec {
    /// AAuth workload identity (RFC 0028 §5; provider from class/operator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aauth: Option<crate::AauthIdentity>,
    /// Refuse autonomous runs that would need user grants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting_for_required: Option<bool>,
    /// The service subject autonomous runs act as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomous_as: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Expose {
    /// Route this agent at the gateway (`/orgs/<org>/agents/<handle>`).
    #[serde(default)]
    pub a2a: bool,
    /// Webhook routes exposed on the hooks host (RFC 0029 §5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<WebhookExposure>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookExposure {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PeerRef {
    /// Same-org agent handle.
    pub agent: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleSpec {
    /// `drained` (daemons) | `idle` (jobs); ALWAYS rendered explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_timeout: Option<String>,
    /// Declarative pause (parity with the `pause` verb).
    #[serde(default)]
    pub paused: bool,
    /// P6-5 scale-from-zero for webhook daemons: park to zero replicas after
    /// this many seconds without a delivery; the gateway's hooks proxy stamps
    /// activity and its stamp on a parked agent is the WAKE signal (senders
    /// retry on the 503 + Retry-After meanwhile). Unset = never park.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_park_seconds: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    /// `ask` (gate to the acting human) | `auto` | `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// HITL channels (`slack:#support-approvals`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hitl: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Negotiated-contract facts (v1 compatibility; carried by conversion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<crate::ContractStatus>,
    /// Workload identity facts (v1 compatibility; carried by conversion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<crate::IdentityStatus>,
    /// Content hash of the rendered configuration (diffable rollouts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered_hash: Option<String>,
    /// Per-bundle content hashes (workflows/skills/context).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bundles: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

// ===========================================================================
// AgentFleet
// ===========================================================================

/// A replicated fleet, v2 shape: the worker `template` is a v1alpha2
/// [`AgentSpec`]; scaling/work/coordinator carry over from v1alpha1
/// unchanged (their v2 evolution is the P6 fleets wave).
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha2",
    kind = "AgentFleet",
    namespaced,
    status = "crate::AgentFleetStatus",
    category = "agentctl",
    printcolumn = r#"{"name":"Scaling","type":"string","jsonPath":".spec.scaling.mode"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    scale(
        spec_replicas_path = ".spec.replicas",
        status_replicas_path = ".status.replicas",
        label_selector_path = ".status.selector"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct AgentFleetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The per-replica worker definition (v1alpha2 spec).
    pub template: AgentSpec,
    pub scaling: crate::Scaling,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<crate::Work>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<FleetCoordinator>,
    /// v2 partitioning (RFC 0034 §3): how members divide the work. Absent ⇒
    /// the v1 `scaling.mode` behavior stands unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partitioning: Option<Partitioning>,
    /// Per-fleet budget window (RFC 0034 §5, P6-6): breach PAUSES intake —
    /// the operator scales the pool to zero for the rest of the window and
    /// surfaces `BudgetExceeded`; the work fabric's redelivery makes the
    /// pause loss-free for leased items. v2-only (stash-preserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<FleetBudget>,
}

/// The enforceable window: `maxUnits` of metering `kind` per `windowSeconds`,
/// read from the platform's own metering aggregation (attributed to this
/// fleet's workload name).
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetBudget {
    /// The metering kind counted (e.g. `a2a_requests`; `tokens` once its
    /// source lands — the vocabulary is `agentctl-metering`'s).
    pub kind: String,
    pub max_units: i64,
    pub window_seconds: i64,
}

/// RFC 0034 §3 — the partitioning strategy family. v2-only (stash-preserved
/// across v1-mediated writes; dropped with a warning on down-convert).
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Partitioning {
    #[serde(default)]
    pub strategy: PartitionStrategy,
    /// Static-strategy details (`strategy: static`).
    #[serde(default, rename = "static", skip_serializing_if = "Option::is_none")]
    pub static_: Option<StaticStrategy>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PartitionStrategy {
    /// Fixed member set (StatefulSet): stable identities, per-member vars
    /// overlays, ordinal-0 singletons.
    #[default]
    Static,
    /// Owner + workers behind a fleet route (P6-3).
    Dispatcher,
    /// Members pull leases from the `work.*` fabric (P6-4).
    Workqueue,
}

/// `strategy: static` — per-member differentiation over ONE shared config:
/// members get the same document plus a tiny per-ordinal overlay carrying
/// only `vars:` (agentd folds `{{config.*}}` references anywhere, and an
/// `armed` workflow flag folds to a real bool — RFC 0034 §3.1 / ADR-0009).
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StaticStrategy {
    /// Per-member vars overlays, indexed by ordinal: `vars[0]` lands on
    /// member 0's config as `vars:` (referenced as `{{config.<key>}}`).
    /// Shorter than `replicas` ⇒ later members get only the defaults below.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "preserve_arbitrary_vec")]
    pub vars: Vec<serde_json::Value>,
    /// Fleet-wide var defaults every member receives (overridden per-member).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "preserve_arbitrary")]
    pub defaults: Option<serde_json::Value>,
    /// Workflows that must run on EXACTLY ONE member (ordinal 0): entries
    /// name a trigger KIND (`schedule`, `loop`, …) to single out every
    /// generated workflow of that kind, or a hand-authored workflow's name.
    /// Everyone else renders the workflow `armed: false` — loaded, inert.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub singletons: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetCoordinator {
    pub template: AgentSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,
    /// How the coordinator reaches the pool (mirrors v1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<crate::Distribution>,
}

// ===========================================================================
// AgentTemplate
// ===========================================================================

/// An instantiable agent spec + typed parameters (the control MCP's and the
/// CLI's `create agent --from-template` source).
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha2",
    kind = "AgentTemplate",
    namespaced,
    category = "agentctl",
    printcolumn = r#"{"name":"Params","type":"string","jsonPath":".spec.paramNames"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AgentTemplateSpec {
    /// The spec to instantiate; `{{params.X}}` holes fold in at create.
    pub template: AgentSpec,
    /// The ONLY holes an instantiation may fill, schema-validated.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamSpec>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSpec {
    /// `string` | `number` | `boolean` | `cron` | `duration`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "preserve_arbitrary")]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

// ===========================================================================
// AgentClass
// ===========================================================================

/// The scoped defaults bundle (RFC 0032 §3): settings fragments, service
/// grants, bundle refs, subagent templates, the supervisor profile, HITL
/// channels — and the SECURITY FLOORS lower scopes may only narrow.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha2",
    kind = "AgentClass",
    namespaced,
    category = "agentctl",
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AgentClassSpec {
    /// The parent class in the scope chain (system → org → group). Absent ⇒
    /// this is a chain root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Group scoping: this class applies to principals matching the selector
    /// (an `AgentClass` with a groupSelector IS the group scope).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_selector: Vec<String>,

    /// Content defaults (shadow-by-name downward).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<ClassDefaults>,
    /// Service grants agents of this class may draw on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceGrant>,
    /// Workflow/skill bundle refs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_sets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_sets: Vec<String>,

    /// SECURITY FLOORS — lower scopes narrow only (RFC 0032 §2); widening is
    /// an admission error naming the floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floors: Option<Floors>,

    /// The supervisor profile agents-of-users in this class get (RFC 0027).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<SupervisorProfile>,
    /// HITL notifier channels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hitl: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClassDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::Limits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<StoreSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence: Option<Intelligence>,
}

/// The narrow-only security envelope.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Floors {
    /// Trifecta legs an agent may at most hold (an unlisted leg is refused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<crate::Capabilities>,
    /// `closed` forbids any egress not named by a granted service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<String>,
    /// The budget ceiling (agents may only shrink it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Tool-name patterns agents may at most be allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Approval may not be weakened below this policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorProfile {
    /// The platform instruction layer (users override with PROSE ONLY).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    /// Services the supervisor itself is granted (the control MCP etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

// ===========================================================================
// MCPService
// ===========================================================================

/// A capability registry entry (RFC 0032 §3): an MCP/peer/http service with
/// tags (FLOORS), allow/exclude (ceilings), and the auth mode agents reach it
/// with. Valid at every scope, user included.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha2",
    kind = "MCPService",
    namespaced,
    status = "MCPServiceStatus",
    category = "agentctl",
    printcolumn = r#"{"name":"Kind","type":"string","jsonPath":".spec.kind"}"#,
    printcolumn = r#"{"name":"Endpoint","type":"string","jsonPath":".spec.endpoint"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MCPServiceSpec {
    /// `mcp` | `peer` | `http`. NOTE: agentd 1.3.1's services catalog accepts
    /// ONLY `mcp` (schema: "phase A") — the renderer projects `peer`/`http`
    /// entries by other means until upstream ships the other kinds (U5).
    #[serde(default)]
    pub kind: ServiceKind,
    /// The service endpoint (`https://…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// An in-cluster Deployment/Service this entry fronts (the operator
    /// resolves the endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_ref: Option<String>,

    /// Capability tags — UNCONDITIONAL FLOORS (agentd treats catalog tags the
    /// same way): `egress`, `secrets`, `exec`, `untrusted-content`, …
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Tool-name ceiling (consumers narrow, never widen).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,

    /// How agents authenticate: `service` (shared credential), `obo`
    /// (per-user token via the exchange, RFC 0030), `passthrough`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ServiceAuth>,
    /// Default arrival rate (`<burst>/<per>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
    /// Dial directly from the agent pod (RFC 0024 posture) instead of
    /// through the tenant mcpg.
    #[serde(default)]
    pub direct: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    #[default]
    Mcp,
    Peer,
    Http,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAuth {
    /// `service` | `obo` | `passthrough`.
    pub mode: String,
    /// RFC 8707 resource/audience for minted tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Secret holding the service credential (`mode: service`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret_ref: Option<crate::SecretKeyRef>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MCPServiceStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Agents currently granted this service (the tag-laundering guard
    /// consults it: an edit that would widen a live consumer is refused).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

// ===========================================================================
// Supervisor
// ===========================================================================

/// A user's supervisor agent (RFC 0027 §2): ensured on first authenticated
/// request (org policy `supervisors: auto`), rendered into an `Agent` with
/// the class's supervisor profile.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha2",
    kind = "Supervisor",
    namespaced,
    status = "SupervisorStatus",
    category = "agentctl",
    printcolumn = r#"{"name":"User","type":"string","jsonPath":".spec.user"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorSpec {
    /// The canonicalized IdP subject this supervisor serves.
    pub user: String,
    #[serde(default)]
    pub paused: bool,
    /// PROSE-ONLY user instruction layer (never config-defining directives —
    /// the renderer strips machinery; RFC 0027 §4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_override: Option<String>,
    /// Budget narrowing below the class ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_override: Option<Budget>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorStatus {
    /// The rendered Agent's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_conversation: Option<String>,
    /// The owner's identity-resolved groups, stamped by the GATEWAY at each
    /// introspection (P4-2). The control MCP evaluates org accessPolicies
    /// against these — the supervisor never asserts its owner's groups
    /// itself, and stale grants age out at the owner's next conversation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

// ===========================================================================
// Conversion (P2-1b): v1alpha1 ↔ v1alpha2
// ===========================================================================

pub mod convert {
    use super::*;

    /// v1alpha1 → v1alpha2, LOSSLESS: every v1 field has a v2 home. Returns
    /// the converted spec plus conversion WARNINGS (deprecated spellings the
    /// author should migrate).
    pub fn agent_v1_to_v2(v1: &crate::AgentSpec) -> (AgentSpec, Vec<String>) {
        let mut warnings = Vec::new();
        let mut triggers = Vec::new();
        let mut workflows = Vec::new();

        let shape = match v1.mode {
            crate::Mode::Once => {
                triggers.push(Trigger {
                    once: Some(OnceTrigger {}),
                    ..Default::default()
                });
                Shape::Job
            }
            crate::Mode::Loop => {
                triggers.push(Trigger {
                    loop_: Some(LoopTrigger {
                        interval: v1
                            .loop_
                            .as_ref()
                            .map(|l| l.interval.clone())
                            .unwrap_or_default(),
                        until: v1.loop_.as_ref().and_then(|l| l.deadline.clone()),
                    }),
                    ..Default::default()
                });
                Shape::Daemon
            }
            crate::Mode::Schedule => {
                triggers.push(Trigger {
                    schedule: Some(ScheduleTrigger {
                        cron: v1.schedule.as_ref().map(|s| s.cron.clone()),
                        every: None,
                        tz: None,
                        runtime: Some(ScheduleRuntime::External),
                    }),
                    ..Default::default()
                });
                Shape::Cron
            }
            crate::Mode::Reactive => {
                for uri in &v1.subscribe {
                    warnings.push(format!(
                        "spec.subscribe[{uri:?}]: converted to a subscribe trigger with an \
                         INFERRED service binding — declare triggers[].subscribe.service explicitly"
                    ));
                    triggers.push(Trigger {
                        subscribe: Some(SubscribeTrigger {
                            service: String::new(),
                            uri: uri.clone(),
                            debounce: None,
                            filter: None,
                        }),
                        ..Default::default()
                    });
                }
                Shape::Daemon
            }
            crate::Mode::Workflow => {
                warnings.push(
                    "mode: workflow is now spec.workflows[] on a daemon/job shape".to_string(),
                );
                Shape::Daemon
            }
        };
        if let Some(wf) = &v1.workflow {
            workflows.push(WorkflowSource {
                set_ref: None,
                inline: wf
                    .inline
                    .as_ref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .or_else(|| {
                        wf.inline
                            .as_ref()
                            .map(|raw| serde_json::Value::String(raw.clone()))
                    }),
                config_map_ref: wf.config_map_key_ref.clone(),
            });
        }
        if !v1.mcp_servers.is_empty() {
            warnings.push(
                "spec.mcpServers (inline endpoints) are deprecated in v1alpha2: register an \
                 MCPService and grant it via spec.services[]"
                    .to_string(),
            );
        }

        let spec = AgentSpec {
            class: None,
            handle: v1.handle.clone(),
            display_name: v1.display_name.clone(),
            runtime: v1.image.as_ref().map(|image| RuntimeSelector {
                version: None,
                image: Some(image.clone()),
            }),
            shape,
            schedule: v1.schedule.as_ref().map(|s| s.cron.clone()),
            instruction: v1.instruction.as_ref().map(|text| Instruction {
                text: Some(text.clone()),
                config_map_ref: None,
            }),
            triggers,
            workflows,
            skills: Vec::new(),
            services: Vec::new(),
            mcp_servers: v1.mcp_servers.clone(),
            intelligence: v1.model.as_ref().map(|m| Intelligence {
                pool: m.pool.clone(),
                tier: None,
                model: m.id.clone(),
                budget: v1.limits.as_ref().and_then(|l| {
                    l.lifetime_tokens.map(|t| Budget {
                        lifetime_tokens: Some(t as i64),
                        windows: Vec::new(),
                    })
                }),
            }),
            store: None,
            identity: v1.identity.as_ref().map(|i| IdentitySpec {
                aauth: i.aauth.clone(),
                acting_for_required: None,
                autonomous_as: None,
            }),
            access: v1.access.clone(),
            expose: v1.surfaces.as_ref().map(|s| Expose {
                a2a: s.a2a,
                webhooks: Vec::new(),
            }),
            peers: Vec::new(),
            lifecycle: None,
            limits: v1.limits.clone(),
            capabilities: v1.capabilities.clone(),
            priority: None,
            approval: None,
            substrate: v1.substrate,
        };
        (spec, warnings)
    }

    /// v1alpha1 fleet → v1alpha2 (template + coordinator template convert;
    /// scaling/work carry verbatim).
    pub fn fleet_v1_to_v2(v1: &crate::AgentFleetSpec) -> (AgentFleetSpec, Vec<String>) {
        let (template, mut warnings) = agent_v1_to_v2(&v1.template);
        let coordinator = v1.coordinator.as_ref().map(|c| {
            let (t, w) = agent_v1_to_v2(&c.template);
            warnings.extend(w.into_iter().map(|w| format!("coordinator.template: {w}")));
            FleetCoordinator {
                template: t,
                replicas: c.replicas,
                distribution: c.distribution,
            }
        });
        (
            AgentFleetSpec {
                handle: v1.handle.clone(),
                display_name: v1.display_name.clone(),
                template,
                scaling: v1.scaling.clone(),
                work: v1.work.clone(),
                replicas: v1.replicas,
                coordinator,
                // v1 has no partitioning/budget surface; the stash (or a
                // v2 write) is the only source.
                partitioning: None,
                budget: None,
            },
            warnings,
        )
    }

    /// v1alpha2 fleet → v1alpha1 (templates down-convert; lossy fields warn).
    pub fn fleet_v2_to_v1(v2: &AgentFleetSpec) -> (crate::AgentFleetSpec, Vec<String>) {
        let (template, mut warnings) = agent_v2_to_v1(&v2.template);
        if v2.partitioning.is_some() {
            warnings.push(
                "spec.partitioning is v1alpha2-only and is not represented in v1alpha1 \
                 (preserved via the conversion stash)"
                    .into(),
            );
        }
        if v2.budget.is_some() {
            warnings.push(
                "spec.budget is v1alpha2-only and is not represented in v1alpha1 \
                 (preserved via the conversion stash)"
                    .into(),
            );
        }
        let coordinator = v2.coordinator.as_ref().map(|c| {
            let (t, w) = agent_v2_to_v1(&c.template);
            warnings.extend(w.into_iter().map(|w| format!("coordinator.template: {w}")));
            crate::Coordinator {
                template: t,
                replicas: c.replicas,
                distribution: c.distribution,
            }
        });
        (
            crate::AgentFleetSpec {
                handle: v2.handle.clone(),
                display_name: v2.display_name.clone(),
                template,
                scaling: v2.scaling.clone(),
                work: v2.work.clone(),
                replicas: v2.replicas,
                coordinator,
            },
            warnings,
        )
    }

    /// v1alpha2 → v1alpha1, LOSSY for v2-only fields — every drop is a
    /// warning. Exists only for old readers; v1alpha2 is the storage version.
    pub fn agent_v2_to_v1(v2: &AgentSpec) -> (crate::AgentSpec, Vec<String>) {
        let mut warnings = Vec::new();
        let mut mode = match v2.shape {
            Shape::Job => crate::Mode::Once,
            Shape::Cron => crate::Mode::Schedule,
            Shape::Daemon => crate::Mode::Reactive,
        };
        let mut subscribe = Vec::new();
        let mut loop_ = None;
        let mut schedule = v2.schedule.as_ref().map(|cron| crate::Schedule {
            cron: cron.clone(),
            timezone: None,
        });
        for t in &v2.triggers {
            if let Some(l) = &t.loop_ {
                mode = crate::Mode::Loop;
                loop_ = Some(crate::LoopParams {
                    interval: l.interval.clone(),
                    deadline: l.until.clone(),
                });
            } else if let Some(s) = &t.schedule {
                mode = crate::Mode::Schedule;
                if schedule.is_none() {
                    schedule = s.cron.as_ref().map(|cron| crate::Schedule {
                        cron: cron.clone(),
                        timezone: s.tz.clone(),
                    });
                }
            } else if let Some(s) = &t.subscribe {
                subscribe.push(s.uri.clone());
            } else if t.webhook.is_some()
                || t.stream.is_some()
                || t.signal.is_some()
                || t.event.is_some()
                || t.a2a_command.is_some()
            {
                warnings.push(
                    "a webhook/stream/signal/event/a2aCommand trigger has no v1alpha1 \
                     representation and was dropped"
                        .to_string(),
                );
            }
        }
        for field in [
            ("class", v2.class.is_some()),
            ("services", !v2.services.is_empty()),
            ("skills", !v2.skills.is_empty()),
            ("store", v2.store.is_some()),
            ("peers", !v2.peers.is_empty()),
            ("approval", v2.approval.is_some()),
            ("priority", v2.priority.is_some()),
            ("lifecycle", v2.lifecycle.is_some()),
        ] {
            if field.1 {
                warnings.push(format!(
                    "spec.{} has no v1alpha1 representation and was dropped",
                    field.0
                ));
            }
        }

        let v1 = crate::AgentSpec {
            mode,
            handle: v2.handle.clone(),
            display_name: v2.display_name.clone(),
            image: v2.runtime.as_ref().and_then(|r| r.image.clone()),
            model: v2.intelligence.as_ref().map(|i| crate::ModelBinding {
                pool: i.pool.clone(),
                id: i.model.clone(),
            }),
            instruction: v2.instruction.as_ref().and_then(|i| i.text.clone()),
            mcp_servers: v2.mcp_servers.clone(),
            substrate: v2.substrate,
            surfaces: v2.expose.as_ref().map(|e| crate::DesiredSurfaces {
                a2a: e.a2a,
                ..Default::default()
            }),
            subscribe,
            loop_,
            schedule,
            workflow: v2.workflows.first().map(|w| crate::WorkflowSource {
                inline: w.inline.as_ref().map(|v| match v {
                    serde_json::Value::String(raw) => raw.clone(),
                    other => other.to_string(),
                }),
                config_map_key_ref: w.config_map_ref.clone(),
            }),
            limits: v2.limits.clone(),
            capabilities: v2.capabilities.clone(),
            access: v2.access.clone(),
            identity: v2.identity.as_ref().map(|i| crate::Identity {
                aauth: i.aauth.clone(),
            }),
        };
        (v1, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_spec_parses_the_rfc_example_shape() {
        let spec: AgentSpec = serde_yaml::from_str(
            r#"
class: default
handle: zendesk-triage
displayName: "Zendesk triage"
shape: daemon
instruction: { text: "You triage Zendesk tickets" }
triggers:
  - schedule: { cron: "0 7 * * 1-5", tz: UTC }
  - webhook: { path: /zendesk, auth: hmac, rate: "30/1m" }
  - subscribe: { service: drive, uri: "drive://tickets/inbox.xlsx", debounce: 500ms }
services:
  - name: zendesk
    allow: [ticket_read, ticket_reply]
intelligence: { pool: default, tier: small }
expose: { a2a: true }
lifecycle: { runUntil: drained, drainTimeout: 25s }
"#,
        )
        .unwrap();
        assert_eq!(spec.shape, Shape::Daemon);
        assert_eq!(spec.triggers.len(), 3);
        assert!(spec.triggers[0].schedule.is_some());
        assert_eq!(
            spec.triggers[2].subscribe.as_ref().unwrap().service,
            "drive"
        );
        assert_eq!(spec.services[0].allow, vec!["ticket_read", "ticket_reply"]);
        assert_eq!(
            spec.lifecycle.as_ref().unwrap().run_until.as_deref(),
            Some("drained")
        );
    }

    #[test]
    fn v1_to_v2_conversion_is_lossless_for_each_mode() {
        // reactive + subscribe + a2a surface → daemon with subscribe triggers
        // (warned: inferred service binding) + expose.a2a.
        let v1: crate::AgentSpec = serde_json::from_value(serde_json::json!({
            "mode": "reactive",
            "image": "agentd:1.3.1",
            "subscribe": ["mock://res/a"],
            "surfaces": { "a2a": true },
            "model": { "pool": "p", "id": "m" },
            "instruction": null,
        }))
        .unwrap();
        let (v2, warnings) = convert::agent_v1_to_v2(&v1);
        assert_eq!(v2.shape, Shape::Daemon);
        assert_eq!(v2.triggers.len(), 1);
        assert_eq!(
            v2.triggers[0].subscribe.as_ref().unwrap().uri,
            "mock://res/a"
        );
        assert!(v2.expose.as_ref().unwrap().a2a);
        assert_eq!(
            v2.runtime.as_ref().unwrap().image.as_deref(),
            Some("agentd:1.3.1")
        );
        assert_eq!(v2.intelligence.as_ref().unwrap().pool.as_deref(), Some("p"));
        assert!(warnings.iter().any(|w| w.contains("INFERRED service")));

        // schedule → cron shape with an external schedule trigger.
        let v1: crate::AgentSpec = serde_json::from_value(serde_json::json!({
            "mode": "schedule",
            "schedule": { "cron": "0 * * * *" },
            "instruction": "tick",
        }))
        .unwrap();
        let (v2, _) = convert::agent_v1_to_v2(&v1);
        assert_eq!(v2.shape, Shape::Cron);
        assert_eq!(
            v2.triggers[0].schedule.as_ref().unwrap().runtime,
            Some(ScheduleRuntime::External)
        );
        assert_eq!(v2.schedule.as_deref(), Some("0 * * * *"));
    }

    #[test]
    fn v1_v2_v1_roundtrip_preserves_the_v1_surface() {
        let v1: crate::AgentSpec = serde_json::from_value(serde_json::json!({
            "mode": "loop",
            "image": "agentd:1.3.1",
            "instruction": "work",
            "loop": { "interval": "30s", "deadline": "1h" },
            "handle": "worker",
            "capabilities": { "egress": true },
        }))
        .unwrap();
        let (v2, _) = convert::agent_v1_to_v2(&v1);
        let (back, warnings) = convert::agent_v2_to_v1(&v2);
        assert_eq!(back.mode, crate::Mode::Loop);
        assert_eq!(back.loop_.as_ref().unwrap().interval, "30s");
        assert_eq!(back.loop_.as_ref().unwrap().deadline.as_deref(), Some("1h"));
        assert_eq!(back.image.as_deref(), Some("agentd:1.3.1"));
        assert_eq!(back.handle.as_deref(), Some("worker"));
        assert_eq!(back.instruction.as_deref(), Some("work"));
        assert_eq!(back.capabilities.as_ref().unwrap().egress, Some(true));
        assert!(
            warnings.is_empty(),
            "a pure v1 surface drops nothing: {warnings:?}"
        );
    }

    #[test]
    fn v2_to_v1_names_every_dropped_field() {
        let v2 = AgentSpec {
            class: Some("gold".into()),
            services: vec![ServiceGrant {
                name: "zendesk".into(),
                allow: vec![],
            }],
            store: Some(StoreSpec {
                class: StoreClass::Managed,
                retention: None,
                size: None,
            }),
            triggers: vec![Trigger {
                webhook: Some(WebhookTrigger {
                    path: "/hook".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (_, warnings) = convert::agent_v2_to_v1(&v2);
        for needle in ["spec.class", "spec.services", "spec.store", "webhook"] {
            assert!(
                warnings.iter().any(|w| w.contains(needle)),
                "no warning names {needle}: {warnings:?}"
            );
        }
    }

    #[test]
    fn registry_kinds_parse() {
        let svc: MCPServiceSpec = serde_yaml::from_str(
            r#"
kind: mcp
endpoint: https://zendesk-mcp.tools:8443/mcp
tags: [egress]
allow: [ticket_read, ticket_reply, ticket_close]
auth: { mode: obo, audience: zendesk }
"#,
        )
        .unwrap();
        assert_eq!(svc.kind, ServiceKind::Mcp);
        assert_eq!(svc.tags, vec!["egress"]);
        assert_eq!(svc.auth.as_ref().unwrap().mode, "obo");

        let class: AgentClassSpec = serde_yaml::from_str(
            r#"
defaults:
  runtime: { version: "1.3.1" }
  priority: normal
floors:
  egress: closed
  budget: { windows: [{ per: day, tokens: 100000 }] }
  tools: ["ticket_*"]
services:
  - name: zendesk
supervisor:
  instruction: "You are the org supervisor."
"#,
        )
        .unwrap();
        assert_eq!(
            class.floors.as_ref().unwrap().egress.as_deref(),
            Some("closed")
        );
        assert_eq!(
            class
                .floors
                .as_ref()
                .unwrap()
                .budget
                .as_ref()
                .unwrap()
                .windows[0]
                .tokens,
            100000
        );

        let sup: SupervisorSpec = serde_yaml::from_str(
            r#"
user: "mock:alice"
instructionOverride: "Prefer terse answers."
"#,
        )
        .unwrap();
        assert_eq!(sup.user, "mock:alice");
        assert!(!sup.paused);
    }
}
