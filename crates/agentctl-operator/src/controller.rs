// SPDX-License-Identifier: BUSL-1.1
//! The level-triggered reconcile loop (agentctl RFC 0006).
//!
//! A [`kube::runtime::Controller`] watches [`Agent`] objects and the workloads
//! they own, and drives the cluster toward the rendered desired state:
//!
//! 1. render the `Agent` to its workload via the pure [`render_agent`] core;
//! 2. server-side-apply that workload (it carries an owner reference, so GC
//!    reclaims it when the `Agent` is deleted — RFC 0003 §5);
//! 3. patch `Agent.status` with the conditions taxonomy (RFC 0003 §6.2) +
//!    `observedGeneration` + a curated contract projection.
//!
//! A [`RenderError`] is surfaced as a `Validated=False` condition rather than
//! failing the reconcile hard: a spec the renderer rejects is a user error, not
//! a transient one, so there is nothing to retry until the spec changes.
//!
//! The cluster-touching glue is kept thin; the decision-making lives in pure,
//! unit-testable helpers ([`ready_condition`], [`validated_failed_condition`],
//! [`rendered_kind`], [`requeue_after`], [`error_backoff`]).

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_api::{Agent, AgentFleet, AgentSpec, Condition, ContractStatus, MCPServerSet, Mode};
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use kube::api::{Patch, PatchParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event as KEvent, EventType, Recorder};
use kube::runtime::finalizer::{finalizer, Event};
use kube::{Api, Client, Resource, ResourceExt};
use tracing::{debug, info, warn};

use crate::metrics::Metrics;
use crate::{
    coordinator_name, fleet_selector_string, inject_api_token, inject_mcp_servers, inject_workflow,
    render_agent, render_coordinator, render_fleet, render_scaled_object, workflow_configmap_name,
    McpBinding, RenderConfig, RenderError, Rendered, DEFAULT_COORDINATION_URL,
    DEFAULT_SCALER_ADDRESS,
};

/// Finalizer key gating `Agent` deletion until cleanup runs (RFC 0006).
const FINALIZER: &str = "agentctl.dev/cleanup";
/// Field-manager identity for server-side apply of owned workloads.
const FIELD_MANAGER: &str = "agentctl-operator";
/// Steady-state resync cadence: re-reconcile even absent a watch event so the
/// status projection cannot drift silently.
const RESYNC: Duration = Duration::from_secs(300);
/// Backoff before retrying a failed reconcile (transient apiserver errors).
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Shared reconcile context: the cluster client every handler patches through,
/// plus the metrics registry the reconcile path records into.
#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub metrics: Arc<Metrics>,
    /// Publishes Kubernetes Events on reconcile outcomes (RFC 0010).
    pub recorder: Recorder,
    /// KEDA scaler wiring for claim-mode fleets (RFC 0011).
    pub scaler: ScalerConfig,
    /// Optional in-cluster bearer-token injection into rendered agent pods
    /// (chart `apiToken.enabled`).
    pub api_token: ApiTokenConfig,
    /// Operator-scoped render inputs (the ModelGateway URL agents dial keyless;
    /// env `AGENTCTL_MODELGATEWAY_URL`). Read once at startup
    /// ([`RenderConfig::from_env`]).
    pub render: RenderConfig,
    /// Workload PKI wiring (serving Certificates + per-ns CA distribution;
    /// envs `AGENTCTL_ISSUER_REF` / `AGENTCTL_CA_FILE`). Read once at startup
    /// ([`crate::pki::PkiConfig::from_env`]).
    pub pki: crate::pki::PkiConfig,
    /// Per-tenant-namespace agent NetworkPolicy reconciliation (env
    /// `NETWORK_POLICIES_ENABLED`; chart `networkPolicies.enabled`). Read once at
    /// startup ([`crate::netpol::NetPolConfig::from_env`]).
    pub netpol: crate::netpol::NetPolConfig,
}

/// Operator-side wiring for the optional in-cluster bearer-token gate (chart
/// `apiToken.enabled`). Read once at startup ([`ApiTokenConfig::from_env`]) and
/// carried on [`Ctx`]. When `enabled` (env `API_TOKEN_ENABLED`, default
/// `false`), the operator injects `AGENTCTL_API_TOKEN` (a `secretKeyRef` on the
/// chart-created `agentctl-api-token` Secret) into rendered agent pods so a
/// conformant agent can present it to the token-gated coordination server /
/// ModelGateway.
///
/// CROSS-NAMESPACE LIMITATION: a `secretKeyRef` resolves only within the pod's
/// own namespace, and the chart creates the token Secret in the control-plane
/// namespace ([`namespace`](Self::namespace), the operator's `POD_NAMESPACE`).
/// Injecting the ref into an agent in *another* namespace would produce a pod
/// that cannot start (the Secret is absent there). So injection is gated on the
/// agent being in the control-plane namespace ([`should_inject`](Self::should_inject));
/// for agents elsewhere the operator does NOT inject — the Secret must be
/// replicated into their namespace and wired by other means (documented in
/// docs/security.md). This keeps default + cross-namespace installs from breaking.
#[derive(Clone, Debug, Default)]
pub struct ApiTokenConfig {
    /// Inject the token into agent pods. `API_TOKEN_ENABLED`, default `false`.
    pub enabled: bool,
    /// Control-plane namespace the `agentctl-api-token` Secret lives in
    /// (the operator's `POD_NAMESPACE`). Injection only fires for agents here,
    /// since a `secretKeyRef` cannot cross namespaces.
    pub namespace: Option<String>,
}

impl ApiTokenConfig {
    /// Build from the operator environment. Disabled unless `API_TOKEN_ENABLED`
    /// is truthy; the control-plane namespace is the operator's `POD_NAMESPACE`.
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var("API_TOKEN_ENABLED")
                .map(|v| parse_bool(&v))
                .unwrap_or(false),
            namespace: non_empty_env("POD_NAMESPACE"),
        }
    }

    /// Whether to inject the token env into an agent rendered into `agent_ns`.
    /// True only when enabled AND the agent is in the control-plane namespace
    /// (where the `secretKeyRef` actually resolves).
    pub fn should_inject(&self, agent_ns: &str) -> bool {
        self.enabled && self.namespace.as_deref() == Some(agent_ns)
    }
}

/// Operator-side KEDA scaler wiring for claim-mode fleets (RFC 0011). Read once
/// from the environment at startup ([`ScalerConfig::from_env`]) and carried on
/// [`Ctx`]. The defaults point a stock install at the in-cluster scaler +
/// coordination Services; `enabled=false` (env `SCALER_ENABLED`) lets a non-KEDA
/// cluster turn ScaledObject emission off entirely so the Deployment reconcile is
/// untouched.
#[derive(Clone, Debug)]
pub struct ScalerConfig {
    /// Emit a `keda.sh/v1alpha1` ScaledObject per claim fleet. `SCALER_ENABLED`,
    /// default `true`.
    pub enabled: bool,
    /// gRPC address KEDA dials for the external trigger. `SCALER_ADDRESS`,
    /// default [`DEFAULT_SCALER_ADDRESS`].
    pub scaler_address: String,
    /// Coordination base URL the scaler reads the backlog from when a fleet does
    /// not set its own `spec.workSource`. `COORDINATION_URL`, default
    /// [`DEFAULT_COORDINATION_URL`].
    pub coordination_url: String,
}

impl Default for ScalerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scaler_address: DEFAULT_SCALER_ADDRESS.to_string(),
            coordination_url: DEFAULT_COORDINATION_URL.to_string(),
        }
    }
}

impl ScalerConfig {
    /// Build from the operator environment, falling back to the defaults.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            enabled: std::env::var("SCALER_ENABLED")
                .map(|v| parse_bool(&v))
                .unwrap_or(d.enabled),
            scaler_address: non_empty_env("SCALER_ADDRESS").unwrap_or(d.scaler_address),
            coordination_url: non_empty_env("COORDINATION_URL").unwrap_or(d.coordination_url),
        }
    }
}

/// Parse a boolean-ish env value; anything but the explicit false-set is true so
/// `SCALER_ENABLED` defaults to on even for unexpected values.
fn parse_bool(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off" | ""
    )
}

/// `std::env::var` filtered to a non-empty value (so `FOO=` falls back to default).
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Everything that can go wrong driving one reconcile. A [`RenderError`] is
/// deliberately *not* in the hot path here — it is converted to a condition —
/// but is kept as a variant so callers can construct it from the pure core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The apiserver rejected an apply/patch (typically transient).
    #[error("kube api error: {0}")]
    Kube(#[from] kube::Error),
    /// The workload could not be rendered from the spec.
    #[error("render error: {0}")]
    Render(#[from] RenderError),
    /// Building the status patch body failed (should be infallible in practice).
    #[error("status serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// A namespaced `Agent` reached us without a `.metadata.namespace`.
    #[error("Agent has no namespace")]
    MissingNamespace,
    /// A rendered workload carried no `.metadata.name` to apply against.
    #[error("rendered workload has no name")]
    MissingName,
    /// The finalizer machinery (add/remove, or a wrapped handler error) failed.
    #[error("finalizer error: {0}")]
    Finalizer(#[source] Box<kube::runtime::finalizer::Error<Error>>),
}

/// Reconcile one `Agent`, recording the reconcile-total/-errors counters and the
/// duration histogram around the actual work in [`reconcile_inner`]. A failed
/// reconcile (transient apiserver/finalizer error) emits a Warning Event.
#[tracing::instrument(skip_all, fields(agent = %agent.name_any()))]
pub async fn reconcile(agent: Arc<Agent>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let start = Instant::now();
    let result = reconcile_inner(agent.clone(), ctx.clone()).await;
    ctx.metrics
        .record_reconcile(start.elapsed(), result.is_err());
    if let Err(e) = &result {
        publish_event(
            &ctx,
            agent.as_ref(),
            EventType::Warning,
            "ReconcileError",
            "Reconcile",
            e.to_string(),
        )
        .await;
    }
    result
}

/// Publish a Kubernetes Event against `obj` (RFC 0010). Best-effort: a failed
/// publish is logged, never fatal to the reconcile. Generic over the CR type so
/// both `Agent` and `AgentFleet` share one path.
async fn publish_event<K>(
    ctx: &Ctx,
    obj: &K,
    type_: EventType,
    reason: &str,
    action: &str,
    note: String,
) where
    K: Resource<DynamicType = ()>,
{
    let reference = obj.object_ref(&());
    if let Err(e) = ctx
        .recorder
        .publish(
            &KEvent {
                type_,
                reason: reason.to_string(),
                note: Some(note),
                action: action.to_string(),
                secondary: None,
            },
            &reference,
        )
        .await
    {
        warn!(error = %e, reason, "failed to publish Kubernetes Event");
    }
}

/// Reconcile one `Agent`: wrap apply/cleanup in the deletion finalizer so the
/// owned workload is reclaimed in order before the object disappears.
async fn reconcile_inner(agent: Arc<Agent>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = agent.namespace().ok_or(Error::MissingNamespace)?;
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&agents, FINALIZER, agent, |event| async move {
        match event {
            Event::Apply(agent) => apply(agent, ctx, &ns).await,
            Event::Cleanup(agent) => Ok(cleanup(agent.as_ref())),
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

/// The `Apply` branch: render, server-side-apply the workload, patch status.
async fn apply(agent: Arc<Agent>, ctx: Arc<Ctx>, ns: &str) -> Result<Action, Error> {
    let name = agent.name_any();
    let observed = agent.metadata.generation;
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), ns);

    // Render + apply the workload, then derive the desired status. A RenderError
    // is a user error (invalid spec) → Validated=False, not a retried failure.
    let (condition, phase, contract) = match render_agent(&agent, &ctx.render) {
        Ok(mut rendered) => {
            let kind = rendered_kind(&rendered);
            // Optional in-cluster bearer-token injection (chart apiToken.enabled).
            // Only fires for agents in the control-plane namespace (a secretKeyRef
            // cannot cross namespaces — see ApiTokenConfig::should_inject).
            if ctx.api_token.should_inject(ns) {
                inject_api_token(&mut rendered);
            }
            // Bind MCP tool servers: resolve the agent's MCPServerSet refs and
            // render `--mcp <name>=<gateway>/s/<name>` so it dials the MCPGateway
            // keyless (RFC 0019). Best-effort — a dangling ref is logged, not
            // fatal (surfaces as a missing tool, not a failed reconcile).
            let bindings = resolve_mcp_bindings(&ctx.client, ns, &agent.spec).await;
            inject_mcp_servers(&mut rendered, &ctx.render.mcpgateway_url, &bindings);
            let owner = agent
                .controller_owner_ref(&())
                .expect("Agent CRs always carry name+uid on the live object");
            // Workflow (agentd v2 --mode workflow): materialize the graph
            // (inline → generated ConfigMap; ref → mount) + --workflow <file>.
            ensure_and_inject_workflow(&ctx.client, ns, &name, &agent.spec, &owner, &mut rendered)
                .await?;
            // Workload PKI (serving Certificate + per-ns CA ConfigMap), so the
            // pod's mounts resolve as it schedules.
            if ctx.pki.enabled() {
                crate::pki::ensure_workload_pki(&ctx.client, &ctx.pki, ns, &name, &owner).await?;
            }
            // Tenant network isolation (RFC 0015): ensure the agent NetworkPolicies
            // in THIS namespace, so a dynamically-created tenant namespace is
            // isolated without a chart re-render. No-op unless enabled.
            if ctx.netpol.active() {
                crate::netpol::ensure_agent_netpols(&ctx.client, &ctx.netpol, ns).await?;
            }
            apply_workload(&ctx.client, ns, &rendered).await?;
            info!(agent = %name, workload = kind, "applied workload");
            publish_event(
                ctx.as_ref(),
                agent.as_ref(),
                EventType::Normal,
                "Reconciled",
                "WorkloadApplied",
                format!("{kind} workload applied"),
            )
            .await;
            // Ready reflects the single pod's OBSERVED readiness (a reactive/loop
            // Deployment); a once/workflow Job keeps the applied condition (its
            // health is the exit-code disposition, not replica readiness).
            let condition = match workload_readiness(&ctx.client, ns, &name, kind, 1).await {
                Some((ready, desired)) => readiness_condition(observed, kind, ready, desired),
                None => ready_condition(observed, kind),
            };
            let phase: &str = if condition.status == "True" {
                "Ready"
            } else {
                "Progressing"
            };
            (
                condition,
                phase,
                ContractStatus {
                    mode: Some(mode_label(agent.spec.mode)),
                    ..Default::default()
                },
            )
        }
        Err(e) => {
            warn!(agent = %name, error = %e, "render rejected spec; marking Validated=False");
            publish_event(
                ctx.as_ref(),
                agent.as_ref(),
                EventType::Warning,
                "RenderFailed",
                "Validate",
                e.to_string(),
            )
            .await;
            (
                validated_failed_condition(&e.to_string()),
                "Invalid",
                ContractStatus::default(),
            )
        }
    };

    // DeepEqual guard (RFC 0006 §2.6): only write status if it actually changed,
    // so we don't churn the Agent (and re-trigger our own watch) every reconcile.
    let desired = desired_status(&condition, observed, phase, &contract)?;
    if status_changed(agent.status.as_ref(), &desired)? {
        let patch = serde_json::json!({ "status": desired });
        agents
            .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        debug!(agent = %name, phase, "patched status");
    } else {
        debug!(agent = %name, "status unchanged; skipped patch");
    }
    Ok(Action::requeue(requeue_after()))
}

/// The `Cleanup` branch: the workload is owner-referenced, so Kubernetes GC
/// reclaims it. We only log; deletion proceeds once the finalizer is removed.
fn cleanup(agent: &Agent) -> Action {
    info!(agent = %agent.name_any(), "agent deleted; owned workload reclaimed by GC");
    Action::await_change()
}

/// Server-side-apply the rendered workload into `ns` under our field manager.
/// Materialize an agent/fleet-template's workflow source (if any) and inject the
/// `--workflow <file>` mount into the rendered pod (RFC 0006 / agentd v2). For an
/// inline graph the operator server-side-applies a generated ConfigMap
/// (`<workload>-workflow`, owner-ref'd so GC reclaims it); a `configMapKeyRef` is
/// mounted directly. No-op when the spec carries no workflow. Errors bubble to
/// the reconcile (transient apiserver → retried).
async fn ensure_and_inject_workflow(
    client: &Client,
    ns: &str,
    workload: &str,
    spec: &AgentSpec,
    owner: &k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
    rendered: &mut Rendered,
) -> Result<(), Error> {
    let Some(wf) = &spec.workflow else {
        return Ok(());
    };
    // Inline → SSA a generated ConfigMap; ref → mount it directly.
    let (cm_name, key) = if let Some(inline) = &wf.inline {
        use k8s_openapi::api::core::v1::ConfigMap;
        let name = workflow_configmap_name(workload);
        let mut data = std::collections::BTreeMap::new();
        data.insert("workflow.json".to_string(), inline.clone());
        let cm = ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(ns.to_string()),
                owner_references: Some(vec![owner.clone()]),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
        cms.patch(
            &name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&cm),
        )
        .await?;
        (name, "workflow.json".to_string())
    } else if let Some(r) = &wf.config_map_key_ref {
        (r.name.clone(), r.key.clone())
    } else {
        // CEL guarantees exactly one is set; defensively no-op otherwise.
        return Ok(());
    };
    inject_workflow(rendered, &cm_name, &key);
    Ok(())
}

/// Resolve an agent/fleet-template's `mcpServerSetRefs` into the flat list of
/// bound MCP servers (name + trifecta tags) the renderer wires to the MCPGateway
/// (RFC 0019). Reads each referenced `MCPServerSet` in the workload's namespace.
/// Best-effort: a missing/dangling ref is logged and skipped (a config error
/// surfaces as a missing tool, never a failed reconcile). Duplicate server names
/// across sets collapse to the first seen (admission owns collision rejection).
async fn resolve_mcp_bindings(client: &Client, ns: &str, spec: &AgentSpec) -> Vec<McpBinding> {
    if spec.mcp_server_set_refs.is_empty() {
        return Vec::new();
    }
    let sets: Api<MCPServerSet> = Api::namespaced(client.clone(), ns);
    let mut out: Vec<McpBinding> = Vec::new();
    for r in &spec.mcp_server_set_refs {
        match sets.get(&r.name).await {
            Ok(set) => {
                for s in set.spec.servers {
                    if out.iter().any(|b| b.name == s.name) {
                        continue;
                    }
                    out.push(McpBinding {
                        name: s.name,
                        tags: s.tags,
                    });
                }
            }
            Err(e) => {
                warn!(%ns, set = %r.name, error = %e, "bound MCPServerSet not found; skipping");
            }
        }
    }
    out
}

async fn apply_workload(client: &Client, ns: &str, rendered: &Rendered) -> Result<(), Error> {
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    match rendered {
        Rendered::Job(job) => {
            let api: Api<Job> = Api::namespaced(client.clone(), ns);
            let name = job.metadata.name.clone().ok_or(Error::MissingName)?;
            api.patch(&name, &pp, &Patch::Apply(job.as_ref())).await?;
        }
        Rendered::CronJob(cj) => {
            let api: Api<CronJob> = Api::namespaced(client.clone(), ns);
            let name = cj.metadata.name.clone().ok_or(Error::MissingName)?;
            api.patch(&name, &pp, &Patch::Apply(cj.as_ref())).await?;
        }
        Rendered::Deployment(dep) => {
            let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
            let name = dep.metadata.name.clone().ok_or(Error::MissingName)?;
            api.patch(&name, &pp, &Patch::Apply(dep.as_ref())).await?;
        }
        Rendered::StatefulSet(sts) => {
            let api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
            let name = sts.metadata.name.clone().ok_or(Error::MissingName)?;
            api.patch(&name, &pp, &Patch::Apply(sts.as_ref())).await?;
        }
    }
    Ok(())
}

/// Build the desired `Agent.status` body (the inner object the merge patch wraps
/// under `"status"`). Kept separate from the write so it can be compared against
/// the live status for the DeepEqual guard.
fn desired_status(
    condition: &Condition,
    observed: Option<i64>,
    phase: &str,
    contract: &ContractStatus,
) -> Result<serde_json::Value, Error> {
    Ok(serde_json::json!({
        "conditions": [serde_json::to_value(condition)?],
        "observedGeneration": serde_json::to_value(observed)?,
        "phase": phase,
        "contract": serde_json::to_value(contract)?,
    }))
}

/// Whether the desired status differs from the live one. Compares as JSON so the
/// managed fields line up with their serialized (camelCase) form; `None`
/// (no status yet) always counts as changed. Generic over the status type so
/// both `Agent` and `AgentFleet` share one guard.
fn status_changed<S: serde::Serialize>(
    current: Option<&S>,
    desired: &serde_json::Value,
) -> Result<bool, Error> {
    let current = match current {
        Some(s) => serde_json::to_value(s)?,
        None => serde_json::Value::Null,
    };
    Ok(&current != desired)
}

/// The workload kind label a render produced, without applying it. Pure so the
/// "does the controller pick Job vs Deployment" decision is unit-testable.
pub fn rendered_kind(rendered: &Rendered) -> &'static str {
    match rendered {
        Rendered::Job(_) => "Job",
        Rendered::CronJob(_) => "CronJob",
        Rendered::Deployment(_) => "Deployment",
        Rendered::StatefulSet(_) => "StatefulSet",
    }
}

/// The applied-only condition, used for a Job (once/loop/schedule/workflow) whose
/// health is its exit-code disposition, not replica readiness. Long-lived workloads
/// use [`readiness_condition`] against observed readback instead.
pub fn ready_condition(observed_generation: Option<i64>, rendered_kind: &str) -> Condition {
    Condition {
        type_: "Ready".to_string(),
        status: "True".to_string(),
        reason: Some("WorkloadApplied".to_string()),
        message: Some(format!("{rendered_kind} workload applied")),
        observed_generation,
        last_transition_time: None,
    }
}

/// Read back the applied workload's OBSERVED readiness so `Ready` reflects reality
/// rather than "the object was server-side-applied" (RFC 0003 §6.2). The
/// `.owns(Deployment/StatefulSet)` watches re-trigger reconcile when the workload's
/// status changes, so this converges: applied → `0/N` (Unavailable) → `k/N`
/// (Progressing) → `N/N` (Ready). Returns `(ready, desired)` for the long-lived
/// kinds, or `None` for a Job (which keeps the applied condition). An unreadable
/// status is treated as `0` ready (fail-safe: a CrashLooping / PKI-misconfigured /
/// image-pull-failing workload never becomes Ready).
async fn workload_readiness(
    client: &Client,
    ns: &str,
    name: &str,
    kind: &str,
    desired_hint: u32,
) -> Option<(u32, u32)> {
    match kind {
        "Deployment" => {
            let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
            let (ready, desired) = match api.get_status(name).await {
                Ok(d) => {
                    let st = d.status.unwrap_or_default();
                    let ready = st.ready_replicas.unwrap_or(0).max(0) as u32;
                    // The Deployment's own observed desired (KEDA-driven for claim);
                    // fall back to the hint when status has no replicas yet.
                    let desired = st.replicas.map(|r| r.max(0) as u32).unwrap_or(desired_hint);
                    (ready, desired)
                }
                Err(_) => (0, desired_hint),
            };
            Some((ready, desired))
        }
        "StatefulSet" => {
            let api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
            let ready = match api.get_status(name).await {
                Ok(s) => s
                    .status
                    .unwrap_or_default()
                    .ready_replicas
                    .unwrap_or(0)
                    .max(0) as u32,
                Err(_) => 0,
            };
            Some((ready, desired_hint))
        }
        _ => None,
    }
}

/// The `Ready` condition derived from observed replica readiness.
pub fn readiness_condition(
    observed_generation: Option<i64>,
    kind: &str,
    ready: u32,
    desired: u32,
) -> Condition {
    let (status, reason, message): (&str, &str, String) = if desired == 0 {
        // A claim fleet scaled to zero is HEALTHY-but-idle (scale-from-zero on backlog).
        (
            "True",
            "ScaledToZero",
            "idle — scaled to zero (elastic-from-zero on work backlog)".to_string(),
        )
    } else if ready >= desired {
        ("True", "AllReplicasReady", format!("{ready}/{desired} replicas ready"))
    } else if ready > 0 {
        ("False", "Progressing", format!("{ready}/{desired} replicas ready"))
    } else {
        ("False", "Unavailable", format!("0/{desired} replicas ready ({kind})"))
    };
    Condition {
        type_: "Ready".to_string(),
        status: status.to_string(),
        reason: Some(reason.to_string()),
        message: Some(message),
        observed_generation,
        last_transition_time: None,
    }
}

/// The failure condition for a spec the renderer rejects (RFC 0003 §6.2).
pub fn validated_failed_condition(message: &str) -> Condition {
    Condition {
        type_: "Validated".to_string(),
        status: "False".to_string(),
        reason: Some("RenderFailed".to_string()),
        message: Some(message.to_string()),
        observed_generation: None,
        last_transition_time: None,
    }
}

/// Steady-state resync delay after a successful reconcile.
pub fn requeue_after() -> Duration {
    RESYNC
}

/// Backoff the [`error_policy`] applies after a failed reconcile.
pub fn error_backoff() -> Duration {
    ERROR_BACKOFF
}

/// Requeue with a short backoff on any reconcile error.
pub fn error_policy(_agent: Arc<Agent>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    warn!(error = %err, "reconcile failed; requeueing");
    Action::requeue(error_backoff())
}

// ---------------------------------------------------------------------------
// AgentFleet reconcile (scaling plane, RFC 0011)
// ---------------------------------------------------------------------------

/// Reconcile one `AgentFleet`, recording reconcile metrics around the work in
/// [`reconcile_fleet_inner`] (shared counters/histogram with the `Agent` loop).
#[tracing::instrument(skip_all, fields(fleet = %fleet.name_any()))]
pub async fn reconcile_fleet(fleet: Arc<AgentFleet>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let start = Instant::now();
    let result = reconcile_fleet_inner(fleet.clone(), ctx.clone()).await;
    ctx.metrics
        .record_reconcile(start.elapsed(), result.is_err());
    if let Err(e) = &result {
        publish_event(
            &ctx,
            fleet.as_ref(),
            EventType::Warning,
            "ReconcileError",
            "Reconcile",
            e.to_string(),
        )
        .await;
    }
    result
}

/// Reconcile one `AgentFleet`: render it to a Deployment (claim) or StatefulSet
/// (shard) and apply it, wrapped in the deletion finalizer (the workload is
/// owner-referenced, so GC reclaims it).
async fn reconcile_fleet_inner(fleet: Arc<AgentFleet>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = fleet.namespace().ok_or(Error::MissingNamespace)?;
    let fleets: Api<AgentFleet> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&fleets, FINALIZER, fleet, |event| async move {
        match event {
            Event::Apply(fleet) => apply_fleet(fleet, ctx, &ns).await,
            Event::Cleanup(fleet) => {
                info!(fleet = %fleet.name_any(), "fleet deleted; owned workload reclaimed by GC");
                Ok(Action::await_change())
            }
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

async fn apply_fleet(fleet: Arc<AgentFleet>, ctx: Arc<Ctx>, ns: &str) -> Result<Action, Error> {
    let name = fleet.name_any();
    let observed = fleet.metadata.generation;
    let fleets: Api<AgentFleet> = Api::namespaced(ctx.client.clone(), ns);

    // On a successful render we also surface the scale-subresource projection:
    // `.status.replicas` (the observed/desired replica count) and
    // `.status.selector` (the label-selector string matching the fleet pods), so
    // `kubectl get agentfleet` shows replicas and an HPA can read both back. On a
    // render error we leave them unset (the merge patch keeps the last value).
    // Observed readiness, populated inside the Ok arm from the workload readback.
    let mut ready_replicas: Option<u32> = None;
    let mut desired_replicas: Option<u32> = None;
    let (condition, replicas, selector, scaler_condition) = match render_fleet(&fleet, &ctx.render)
    {
        Ok(mut rendered) => {
            let kind = rendered_kind(&rendered);
            // Optional in-cluster bearer-token injection (chart apiToken.enabled);
            // gated to the control-plane namespace (secretKeyRef cannot cross
            // namespaces). Fleet pods are agents too, so inject before applying.
            if ctx.api_token.should_inject(ns) {
                inject_api_token(&mut rendered);
            }
            // Bind MCP tool servers for the fleet template (same resolution as a
            // singleton Agent; the fleet's template carries mcpServerSetRefs).
            let bindings = resolve_mcp_bindings(&ctx.client, ns, &fleet.spec.template).await;
            inject_mcp_servers(&mut rendered, &ctx.render.mcpgateway_url, &bindings);
            let owner = fleet
                .controller_owner_ref(&())
                .expect("AgentFleet CRs always carry name+uid on the live object");
            // Workflow for the fleet template (same as a singleton Agent).
            ensure_and_inject_workflow(
                &ctx.client,
                ns,
                &name,
                &fleet.spec.template,
                &owner,
                &mut rendered,
            )
            .await?;
            // Workload PKI (serving Certificate + per-ns CA ConfigMap), so fleet
            // pods' mounts resolve as they schedule.
            if ctx.pki.enabled() {
                crate::pki::ensure_workload_pki(&ctx.client, &ctx.pki, ns, &name, &owner).await?;
            }
            // Tenant network isolation (RFC 0015): same per-namespace agent
            // NetworkPolicies as the single-Agent path — a fleet's pods carry the
            // same `app.kubernetes.io/name: agent` label the policies select.
            if ctx.netpol.active() {
                crate::netpol::ensure_agent_netpols(&ctx.client, &ctx.netpol, ns).await?;
            }
            apply_workload(&ctx.client, ns, &rendered).await?;
            info!(fleet = %name, workload = kind, "applied fleet workload");
            publish_event(
                ctx.as_ref(),
                fleet.as_ref(),
                EventType::Normal,
                "Reconciled",
                "WorkloadApplied",
                format!("{kind} workload applied"),
            )
            .await;
            // The coordinator ("main agent", RFC 0022 §3) — a second owned workload
            // when spec.coordinator is set. `None` ⇒ headless worker pool (unchanged);
            // `Some(ready)` folds into the fleet readiness below (the fleet is Ready
            // only when BOTH the worker pool and the coordinator are ready).
            let coordinator_ready = reconcile_coordinator(ctx.as_ref(), ns, &fleet, &name).await?;
            // KEDA autoscaling for claim-mode fleets (RFC 0011). Gated +
            // best-effort: never hard-fails the Deployment reconcile (e.g. when
            // the KEDA CRDs are absent) — it only surfaces a condition.
            let scaler_condition = reconcile_scaled_object(ctx.as_ref(), ns, &fleet).await;
            // Ready reflects the fleet's OBSERVED replica readiness (Deployment /
            // StatefulSet), not merely "applied": an all-CrashLoop fleet is NOT Ready,
            // and a claim fleet scaled to zero is Ready-but-idle. Populates the
            // long-declared status.readyReplicas / desiredReplicas.
            let scale_target = fleet_replica_count(&fleet, &rendered);
            let worker_condition =
                match workload_readiness(&ctx.client, ns, &name, kind, scale_target).await {
                    Some((ready, desired)) => {
                        ready_replicas = Some(ready);
                        desired_replicas = Some(desired);
                        readiness_condition(observed, kind, ready, desired)
                    }
                    None => ready_condition(observed, kind),
                };
            // Fold the coordinator's readiness in: a not-ready coordinator holds the
            // whole fleet Progressing (the front door is down even if workers are up).
            let condition = match coordinator_ready {
                Some(false) => coordinator_progressing_condition(observed),
                _ => worker_condition,
            };
            (
                condition,
                Some(scale_target),
                Some(fleet_selector_string(&name)),
                scaler_condition,
            )
        }
        Err(e) => {
            warn!(fleet = %name, error = %e, "render rejected fleet spec; marking Validated=False");
            publish_event(
                ctx.as_ref(),
                fleet.as_ref(),
                EventType::Warning,
                "RenderFailed",
                "Validate",
                e.to_string(),
            )
            .await;
            (validated_failed_condition(&e.to_string()), None, None, None)
        }
    };

    let desired = desired_fleet_status(
        &condition,
        observed,
        replicas,
        selector.as_deref(),
        scaler_condition.as_ref(),
        ready_replicas,
        desired_replicas,
    )?;
    if status_changed(fleet.status.as_ref(), &desired)? {
        let patch = serde_json::json!({ "status": desired });
        // Best-effort: a failed status write is logged, never fatal — the next
        // resync re-projects it. The workload is already applied at this point.
        match fleets
            .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Ok(_) => debug!(fleet = %name, "patched fleet status"),
            Err(e) => {
                warn!(fleet = %name, error = %e, "failed to patch fleet status (best-effort)")
            }
        }
    } else {
        debug!(fleet = %name, "fleet status unchanged; skipped patch");
    }
    Ok(Action::requeue(requeue_after()))
}

/// Render + apply the fleet's **coordinator** workload (RFC 0022 §3) when one is
/// declared. Runs the SAME pre-apply pipeline as an agent (api-token, MCP tool
/// bindings from the coordinator template, workload PKI), owns it by the fleet, and
/// reads back its readiness. Returns `None` for a coordinatorless fleet, else
/// `Some(ready)` where `ready` is whether the coordinator Deployment has its
/// replicas up. A render error surfaces an event and reports `Some(false)`.
async fn reconcile_coordinator(
    ctx: &Ctx,
    ns: &str,
    fleet: &AgentFleet,
    fleet_name: &str,
) -> Result<Option<bool>, Error> {
    let Some(coord) = fleet.spec.coordinator.as_ref() else {
        return Ok(None);
    };
    let mut rendered = match render_coordinator(fleet, &ctx.render) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            warn!(fleet = %fleet_name, error = %e, "coordinator render rejected");
            publish_event(
                ctx,
                fleet,
                EventType::Warning,
                "RenderFailed",
                "Coordinator",
                format!("coordinator render rejected: {e}"),
            )
            .await;
            return Ok(Some(false));
        }
        None => return Ok(None),
    };
    let coord_name = coordinator_name(fleet_name);
    // Same secret-free wiring an agent pod gets: the in-cluster token (only in the
    // control-plane namespace), and the coordinator template's own MCP tool servers
    // (this is how `distribution: queue` reaches the coordination server — the
    // coordinator template references it like any tool).
    if ctx.api_token.should_inject(ns) {
        inject_api_token(&mut rendered);
    }
    let bindings = resolve_mcp_bindings(&ctx.client, ns, &coord.template).await;
    inject_mcp_servers(&mut rendered, &ctx.render.mcpgateway_url, &bindings);
    // The Certificate/CA are owned by the fleet (GC'd with it), and named for the
    // coordinator workload so the pod's serving-TLS mount resolves.
    let owner = fleet
        .controller_owner_ref(&())
        .expect("AgentFleet CRs always carry name+uid on the live object");
    if ctx.pki.enabled() {
        crate::pki::ensure_workload_pki(&ctx.client, &ctx.pki, ns, &coord_name, &owner).await?;
    }
    apply_workload(&ctx.client, ns, &rendered).await?;
    info!(fleet = %fleet_name, coordinator = %coord_name, "applied coordinator workload");
    // Readiness: the coordinator is a Deployment of `replicas` (default 1). Ready
    // when its ready replicas meet the desired count.
    let desired_hint = coord.replicas.unwrap_or(1).max(1);
    let ready = match workload_readiness(&ctx.client, ns, &coord_name, "Deployment", desired_hint)
        .await
    {
        Some((ready, desired)) => desired > 0 && ready >= desired,
        // No readback (unreadable status) — treat as applied; the next resync
        // re-reads. Never blocks the fleet on a transient read.
        None => true,
    };
    Ok(Some(ready))
}

/// The fleet-readiness condition when the coordinator is not yet up: the fleet is
/// Progressing (its front door is down) even if the worker pool is ready.
fn coordinator_progressing_condition(observed_generation: Option<i64>) -> Condition {
    Condition {
        type_: "Ready".to_string(),
        status: "False".to_string(),
        reason: Some("CoordinatorNotReady".to_string()),
        message: Some("fleet coordinator (main agent) is not yet ready".to_string()),
        observed_generation,
        last_transition_time: None,
    }
}

/// Best-effort: render + server-side-apply the KEDA `ScaledObject` for a
/// **claim-mode** fleet (RFC 0011), returning a `ScaledObject=False` condition to
/// surface on the fleet status when the apply failed (typically the KEDA CRDs are
/// not installed). Returns `None` — i.e. nothing to surface — when the scaler is
/// disabled, the fleet is shard mode (no ScaledObject), or the apply succeeded.
///
/// The workload Deployment is already applied by the time this runs, so a failure
/// here NEVER hard-fails the reconcile: KEDA being absent degrades to "no
/// autoscaling", not "no fleet".
async fn reconcile_scaled_object(ctx: &Ctx, ns: &str, fleet: &AgentFleet) -> Option<Condition> {
    if !ctx.scaler.enabled {
        return None;
    }
    // None for shard mode → no ScaledObject for fixed-partition fleets.
    let body = render_scaled_object(
        fleet,
        &ctx.scaler.scaler_address,
        &ctx.scaler.coordination_url,
    )?;
    let name = fleet.name_any();
    match apply_scaled_object(&ctx.client, ns, &name, &body).await {
        Ok(()) => {
            debug!(fleet = %name, "applied KEDA ScaledObject");
            None
        }
        Err(e) => {
            warn!(
                fleet = %name, error = %e,
                "failed to apply KEDA ScaledObject (best-effort; is KEDA installed?)"
            );
            publish_event(
                ctx,
                fleet,
                EventType::Warning,
                "ScaledObjectApplyFailed",
                "Autoscale",
                format!("KEDA ScaledObject apply failed (is KEDA installed?): {e}"),
            )
            .await;
            Some(scaled_object_failed_condition(&e.to_string()))
        }
    }
}

/// Server-side-apply the untyped KEDA `ScaledObject` (a [`DynamicObject`] body)
/// under our field manager. The operator holds **no KEDA crate dependency** — the
/// GVK is constructed by hand — so a cluster without the KEDA CRDs returns an
/// error here that the caller swallows (see [`reconcile_scaled_object`]).
async fn apply_scaled_object(
    client: &Client,
    ns: &str,
    name: &str,
    body: &serde_json::Value,
) -> Result<(), Error> {
    let gvk = GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject");
    let ar = ApiResource::from_gvk_with_plural(&gvk, "scaledobjects");
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    api.patch(name, &pp, &Patch::Apply(body)).await?;
    Ok(())
}

/// Condition surfaced on a fleet when the KEDA ScaledObject apply failed
/// (best-effort; typically the KEDA CRDs are not installed). The Deployment is
/// already applied, so this never blocks the workload — it only signals that
/// autoscaling is not wired.
pub fn scaled_object_failed_condition(message: &str) -> Condition {
    Condition {
        type_: "ScaledObject".to_string(),
        status: "False".to_string(),
        reason: Some("ScaledObjectApplyFailed".to_string()),
        message: Some(message.to_string()),
        observed_generation: None,
        last_transition_time: None,
    }
}

/// The replica count to surface on `.status.replicas` for the scale subresource.
///
/// - **shard mode**: the rendered StatefulSet's fixed partition count `N`.
/// - **claim mode**: `.spec.replicas` (the scale-subresource target an HPA/KEDA
///   drives), defaulting to 0 when unset (scaled-to-zero / deferred to KEDA).
///
/// Never reads or writes the rendered Deployment's `.spec.replicas` — that field
/// stays unset and KEDA-owned (the KEDA-safe invariant, RFC 0011).
fn fleet_replica_count(fleet: &AgentFleet, rendered: &Rendered) -> u32 {
    match rendered {
        Rendered::StatefulSet(sts) => sts
            .spec
            .as_ref()
            .and_then(|s| s.replicas)
            .map(|r| r.max(0) as u32)
            .unwrap_or(0),
        _ => fleet.spec.replicas.unwrap_or(0),
    }
}

/// The desired `AgentFleet.status` body. Carries the conditions + observed
/// generation, plus the scale-subresource projection (`replicas` + `selector`)
/// when a render succeeded; those are omitted on a render error so the merge
/// patch preserves the last-known values. An optional `extra` condition (e.g. the
/// KEDA ScaledObject apply outcome) is appended after the primary one.
#[allow(clippy::too_many_arguments)]
fn desired_fleet_status(
    condition: &Condition,
    observed: Option<i64>,
    replicas: Option<u32>,
    selector: Option<&str>,
    extra: Option<&Condition>,
    ready_replicas: Option<u32>,
    desired_replicas: Option<u32>,
) -> Result<serde_json::Value, Error> {
    let mut conditions = vec![serde_json::to_value(condition)?];
    // A best-effort outcome (e.g. the KEDA ScaledObject apply) is appended as a
    // second condition. On the next success it is simply not appended, and the
    // merge patch replaces the whole array — so it self-heals.
    if let Some(extra) = extra {
        conditions.push(serde_json::to_value(extra)?);
    }
    let mut status = serde_json::json!({
        "conditions": conditions,
        "observedGeneration": serde_json::to_value(observed)?,
    });
    if let Some(replicas) = replicas {
        status["replicas"] = serde_json::to_value(replicas)?;
    }
    if let Some(selector) = selector {
        status["selector"] = serde_json::to_value(selector)?;
    }
    // The long-declared, formerly-blank kubectl columns (Desired/Ready), now driven
    // by the observed workload readback.
    if let Some(ready) = ready_replicas {
        status["readyReplicas"] = serde_json::to_value(ready)?;
    }
    if let Some(desired) = desired_replicas {
        status["desiredReplicas"] = serde_json::to_value(desired)?;
    }
    Ok(status)
}

/// Requeue with a short backoff on any fleet reconcile error.
pub fn error_policy_fleet(_fleet: Arc<AgentFleet>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    warn!(error = %err, "fleet reconcile failed; requeueing");
    Action::requeue(error_backoff())
}

/// The agent's mode as its contract-neutral wire label (`once`/`loop`/…).
fn mode_label(mode: Mode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_api::{AgentSpec, Substrate};

    fn agent(mode: Mode) -> Agent {
        let mut a = Agent::new(
            "demo",
            AgentSpec {
                mode,
                image: Some("ghcr.io/example/agent@sha256:abc".into()),
                instruction: Some("do the thing".into()),
                ..Default::default()
            },
        );
        a.metadata.namespace = Some("agents".into());
        a.metadata.uid = Some("uid-1".into());
        a
    }

    #[test]
    fn ready_condition_is_true_with_reason() {
        let c = ready_condition(Some(7), "Job");
        assert_eq!(c.type_, "Ready");
        assert_eq!(c.status, "True");
        assert_eq!(c.reason.as_deref(), Some("WorkloadApplied"));
        assert_eq!(c.observed_generation, Some(7));
        assert!(c.message.unwrap().contains("Job"));
    }

    #[test]
    fn validated_failed_condition_is_false_and_carries_message() {
        let c = validated_failed_condition("image required");
        assert_eq!(c.type_, "Validated");
        assert_eq!(c.status, "False");
        assert_eq!(c.reason.as_deref(), Some("RenderFailed"));
        assert!(c.message.unwrap().contains("image required"));
    }

    #[test]
    fn backoffs_are_distinct_and_nonzero() {
        assert_eq!(requeue_after(), Duration::from_secs(300));
        assert_eq!(error_backoff(), Duration::from_secs(5));
        assert!(error_backoff() < requeue_after());
    }

    #[test]
    fn controller_picks_job_for_once() {
        let rendered = render_agent(&agent(Mode::Once), &RenderConfig::default()).unwrap();
        assert_eq!(rendered_kind(&rendered), "Job");
    }

    #[test]
    fn controller_picks_deployment_for_reactive() {
        let rendered = render_agent(&agent(Mode::Reactive), &RenderConfig::default()).unwrap();
        assert_eq!(rendered_kind(&rendered), "Deployment");
    }

    #[test]
    fn render_error_maps_to_validated_false() {
        let mut a = agent(Mode::Once);
        a.spec.image = None; // classless agent without an image is unrenderable
        let err = render_agent(&a, &RenderConfig::default()).unwrap_err();
        let c = validated_failed_condition(&err.to_string());
        assert_eq!(c.status, "False");
        assert_eq!(c.type_, "Validated");
    }

    #[test]
    fn desired_status_shapes_the_inner_status() {
        let contract = ContractStatus {
            mode: Some(mode_label(Mode::Loop)),
            ..Default::default()
        };
        let status = desired_status(
            &ready_condition(Some(3), "Deployment"),
            Some(3),
            "Ready",
            &contract,
        )
        .unwrap();
        assert_eq!(status["observedGeneration"], 3);
        assert_eq!(status["phase"], "Ready");
        assert_eq!(status["conditions"][0]["type"], "Ready");
        assert_eq!(status["conditions"][0]["status"], "True");
        assert_eq!(status["contract"]["mode"], "loop");
    }

    #[test]
    fn status_changed_is_a_deep_equal_guard() {
        let contract = ContractStatus {
            mode: Some("once".into()),
            ..Default::default()
        };
        let desired = desired_status(
            &ready_condition(Some(1), "Job"),
            Some(1),
            "Ready",
            &contract,
        )
        .unwrap();
        // no status yet → changed
        assert!(status_changed::<agent_api::AgentStatus>(None, &desired).unwrap());
        // an equivalent live status → unchanged (no needless patch / churn)
        let current: agent_api::AgentStatus = serde_json::from_value(desired.clone()).unwrap();
        assert!(!status_changed(Some(&current), &desired).unwrap());
        // a different phase → changed
        let draining = desired_status(
            &ready_condition(Some(1), "Job"),
            Some(1),
            "Draining",
            &contract,
        )
        .unwrap();
        assert!(status_changed(Some(&current), &draining).unwrap());
    }

    #[test]
    fn desired_fleet_status_carries_condition_and_generation() {
        let status = desired_fleet_status(
            &ready_condition(Some(2), "Deployment"),
            Some(2),
            Some(5),
            Some("agentctl.dev/agent=fleet,app.kubernetes.io/name=agent"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(status["observedGeneration"], 2);
        assert_eq!(status["conditions"][0]["type"], "Ready");
        assert_eq!(status["conditions"][0]["status"], "True");
        assert_eq!(status["replicas"], 5);
        assert_eq!(
            status["selector"],
            "agentctl.dev/agent=fleet,app.kubernetes.io/name=agent"
        );
        // a fleet status with a matching condition is unchanged (guard works for fleets too)
        let current: agent_api::AgentFleetStatus = serde_json::from_value(status.clone()).unwrap();
        assert!(!status_changed(Some(&current), &status).unwrap());
    }

    #[test]
    fn desired_fleet_status_omits_scale_projection_on_render_error() {
        // render-error path passes None/None → replicas + selector are not written,
        // so the merge patch preserves whatever the live status already holds.
        let status = desired_fleet_status(
            &validated_failed_condition("bad spec"),
            Some(1),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(status.get("replicas").is_none());
        assert!(status.get("selector").is_none());
    }

    #[test]
    fn fleet_replica_count_from_shard_statefulset_and_claim_spec() {
        use agent_api::{AgentFleet, AgentFleetSpec, AgentSpec, ScaleMode, Scaling};

        let template = AgentSpec {
            mode: Mode::Reactive,
            image: Some("ghcr.io/example/agent@sha256:abc".into()),
            ..Default::default()
        };

        // shard mode: replicas come from the rendered StatefulSet's partition count.
        let mut shard = AgentFleet::new(
            "fleet",
            AgentFleetSpec {
                template: template.clone(),
                scaling: Scaling {
                    mode: ScaleMode::Shard,
                    shards: Some(4),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        shard.metadata.namespace = Some("agents".into());
        shard.metadata.uid = Some("uid-shard".into());
        let rendered = render_fleet(&shard, &RenderConfig::default()).unwrap();
        assert!(matches!(rendered, Rendered::StatefulSet(_)));
        assert_eq!(fleet_replica_count(&shard, &rendered), 4);

        // claim mode: rendered Deployment omits replicas (KEDA-owned) → fall back
        // to .spec.replicas (the scale-subresource target).
        let mut claim = AgentFleet::new(
            "fleet",
            AgentFleetSpec {
                template,
                scaling: Scaling {
                    mode: ScaleMode::Claim,
                    ..Default::default()
                },
                replicas: Some(3),
                ..Default::default()
            },
        );
        claim.metadata.namespace = Some("agents".into());
        claim.metadata.uid = Some("uid-claim".into());
        let rendered = render_fleet(&claim, &RenderConfig::default()).unwrap();
        assert!(matches!(rendered, Rendered::Deployment(_)));
        // KEDA-safe: the rendered Deployment still carries no .spec.replicas.
        if let Rendered::Deployment(dep) = &rendered {
            assert!(dep.spec.as_ref().unwrap().replicas.is_none());
        }
        assert_eq!(fleet_replica_count(&claim, &rendered), 3);

        // claim mode with no spec.replicas → defaults to 0 (deferred to KEDA).
        claim.spec.replicas = None;
        let rendered = render_fleet(&claim, &RenderConfig::default()).unwrap();
        assert_eq!(fleet_replica_count(&claim, &rendered), 0);
    }

    #[test]
    fn fleet_selector_string_matches_rendered_pod_labels() {
        use agent_api::{AgentFleet, AgentFleetSpec, AgentSpec, ScaleMode, Scaling};
        use std::collections::BTreeMap;

        let mut fleet = AgentFleet::new(
            "myfleet",
            AgentFleetSpec {
                template: AgentSpec {
                    mode: Mode::Reactive,
                    image: Some("ghcr.io/example/agent@sha256:abc".into()),
                    ..Default::default()
                },
                scaling: Scaling {
                    mode: ScaleMode::Claim,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        fleet.metadata.namespace = Some("agents".into());
        fleet.metadata.uid = Some("uid-1".into());

        let selector = fleet_selector_string("myfleet");
        // sorted, equality-based form built from the SAME labels render.rs uses.
        assert_eq!(
            selector,
            "agentctl.dev/agent=myfleet,app.kubernetes.io/managed-by=agentctl,app.kubernetes.io/name=agent"
        );

        // every matchLabels entry on the rendered workload appears in the string.
        let rendered = render_fleet(&fleet, &RenderConfig::default()).unwrap();
        let Rendered::Deployment(dep) = &rendered else {
            panic!("claim fleet should render a Deployment");
        };
        let match_labels: BTreeMap<String, String> = dep
            .spec
            .as_ref()
            .unwrap()
            .selector
            .match_labels
            .clone()
            .unwrap();
        for (k, v) in &match_labels {
            assert!(selector.contains(&format!("{k}={v}")));
        }
    }

    #[test]
    fn scaler_config_defaults_point_at_in_cluster_services() {
        let d = ScalerConfig::default();
        assert!(d.enabled);
        assert_eq!(d.scaler_address, DEFAULT_SCALER_ADDRESS);
        assert_eq!(d.coordination_url, DEFAULT_COORDINATION_URL);
    }

    #[test]
    fn api_token_injection_is_gated_on_namespace_and_enabled() {
        // disabled → never inject, regardless of namespace match.
        let off = ApiTokenConfig {
            enabled: false,
            namespace: Some("agentctl-system".into()),
        };
        assert!(!off.should_inject("agentctl-system"));

        // enabled + agent in the control-plane namespace → inject.
        let on = ApiTokenConfig {
            enabled: true,
            namespace: Some("agentctl-system".into()),
        };
        assert!(on.should_inject("agentctl-system"));
        // enabled but agent in another namespace → do NOT inject (secretKeyRef
        // cannot cross namespaces; the Secret must be replicated there instead).
        assert!(!on.should_inject("team-a"));

        // enabled but the operator could not resolve its own namespace → no inject.
        let no_ns = ApiTokenConfig {
            enabled: true,
            namespace: None,
        };
        assert!(!no_ns.should_inject("agentctl-system"));

        // default is fully off.
        assert!(!ApiTokenConfig::default().enabled);
    }

    #[test]
    fn parse_bool_only_false_set_disables() {
        for off in ["0", "false", "FALSE", "no", "off", "", "  Off  "] {
            assert!(!parse_bool(off), "{off:?} should disable");
        }
        for on in ["1", "true", "TRUE", "yes", "on", "anything"] {
            assert!(parse_bool(on), "{on:?} should enable");
        }
    }

    #[test]
    fn scaled_object_failed_condition_is_false_with_reason() {
        let c = scaled_object_failed_condition("no KEDA CRDs");
        assert_eq!(c.type_, "ScaledObject");
        assert_eq!(c.status, "False");
        assert_eq!(c.reason.as_deref(), Some("ScaledObjectApplyFailed"));
        assert!(c.message.unwrap().contains("no KEDA CRDs"));
    }

    #[test]
    fn desired_fleet_status_appends_extra_condition() {
        let status = desired_fleet_status(
            &ready_condition(Some(1), "Deployment"),
            Some(1),
            Some(2),
            Some("agentctl.dev/agent=f"),
            Some(&scaled_object_failed_condition("boom")),
            Some(2),
            Some(2),
        )
        .unwrap();
        // primary condition first, the best-effort ScaledObject outcome second.
        assert_eq!(status["conditions"][0]["type"], "Ready");
        assert_eq!(status["conditions"][1]["type"], "ScaledObject");
        assert_eq!(status["conditions"][1]["status"], "False");
    }

    #[test]
    fn readiness_condition_reflects_replica_state() {
        // All ready → Ready=True.
        let c = readiness_condition(Some(1), "Deployment", 3, 3);
        assert_eq!((c.status.as_str(), c.reason.as_deref()), ("True", Some("AllReplicasReady")));
        // Partial → NOT Ready.
        let c = readiness_condition(Some(1), "Deployment", 1, 3);
        assert_eq!((c.status.as_str(), c.reason.as_deref()), ("False", Some("Progressing")));
        // Zero ready with a desired → Unavailable (the CrashLoop case that used to
        // falsely report Ready=WorkloadApplied).
        let c = readiness_condition(Some(1), "StatefulSet", 0, 3);
        assert_eq!((c.status.as_str(), c.reason.as_deref()), ("False", Some("Unavailable")));
        // Claim fleet scaled to zero → healthy-but-idle Ready.
        let c = readiness_condition(Some(1), "Deployment", 0, 0);
        assert_eq!((c.status.as_str(), c.reason.as_deref()), ("True", Some("ScaledToZero")));
    }

    #[test]
    fn desired_fleet_status_emits_readiness_columns() {
        let status = desired_fleet_status(
            &readiness_condition(Some(1), "Deployment", 2, 3),
            Some(1),
            Some(3),
            Some("sel"),
            None,
            Some(2),
            Some(3),
        )
        .unwrap();
        assert_eq!(status["readyReplicas"], 2);
        assert_eq!(status["desiredReplicas"], 3);
        assert_eq!(status["conditions"][0]["reason"], "Progressing");
    }

    #[test]
    fn unsupported_substrate_is_a_render_error_condition() {
        let mut a = agent(Mode::Once);
        a.spec.substrate = Some(Substrate::KataHybrid);
        let err = render_agent(&a, &RenderConfig::default()).unwrap_err();
        let c = validated_failed_condition(&err.to_string());
        assert!(c.message.unwrap().to_lowercase().contains("substrate"));
    }
}
