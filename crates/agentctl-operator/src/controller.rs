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

use agent_api::{Agent, AgentFleet, Condition, ContractStatus, Mode};
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::Job;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event};
use kube::{Api, Client, ResourceExt};
use tracing::{debug, info, warn};

use crate::metrics::Metrics;
use crate::{render_agent, render_fleet, RenderError, Rendered};

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
/// duration histogram around the actual work in [`reconcile_inner`].
pub async fn reconcile(agent: Arc<Agent>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let start = Instant::now();
    let result = reconcile_inner(agent, ctx.clone()).await;
    ctx.metrics
        .record_reconcile(start.elapsed(), result.is_err());
    result
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
    let (condition, phase, contract) = match render_agent(&agent) {
        Ok(rendered) => {
            let kind = rendered_kind(&rendered);
            apply_workload(&ctx.client, ns, &rendered).await?;
            info!(agent = %name, workload = kind, "applied workload");
            (
                ready_condition(observed, kind),
                "Ready",
                ContractStatus {
                    mode: Some(mode_label(agent.spec.mode)),
                    ..Default::default()
                },
            )
        }
        Err(e) => {
            warn!(agent = %name, error = %e, "render rejected spec; marking Validated=False");
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
async fn apply_workload(client: &Client, ns: &str, rendered: &Rendered) -> Result<(), Error> {
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    match rendered {
        Rendered::Job(job) => {
            let api: Api<Job> = Api::namespaced(client.clone(), ns);
            let name = job.metadata.name.clone().ok_or(Error::MissingName)?;
            api.patch(&name, &pp, &Patch::Apply(job.as_ref())).await?;
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
        Rendered::Deployment(_) => "Deployment",
        Rendered::StatefulSet(_) => "StatefulSet",
    }
}

/// The success condition: the workload was applied for this generation.
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
pub async fn reconcile_fleet(fleet: Arc<AgentFleet>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let start = Instant::now();
    let result = reconcile_fleet_inner(fleet, ctx.clone()).await;
    ctx.metrics
        .record_reconcile(start.elapsed(), result.is_err());
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

    let condition = match render_fleet(&fleet) {
        Ok(rendered) => {
            let kind = rendered_kind(&rendered);
            apply_workload(&ctx.client, ns, &rendered).await?;
            info!(fleet = %name, workload = kind, "applied fleet workload");
            ready_condition(observed, kind)
        }
        Err(e) => {
            warn!(fleet = %name, error = %e, "render rejected fleet spec; marking Validated=False");
            validated_failed_condition(&e.to_string())
        }
    };

    let desired = desired_fleet_status(&condition, observed)?;
    if status_changed(fleet.status.as_ref(), &desired)? {
        let patch = serde_json::json!({ "status": desired });
        fleets
            .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        debug!(fleet = %name, "patched fleet status");
    } else {
        debug!(fleet = %name, "fleet status unchanged; skipped patch");
    }
    Ok(Action::requeue(requeue_after()))
}

/// The desired `AgentFleet.status` body (replica counts are owned by KEDA / the
/// workload and projected in a later step; v1 carries the conditions + observed).
fn desired_fleet_status(
    condition: &Condition,
    observed: Option<i64>,
) -> Result<serde_json::Value, Error> {
    Ok(serde_json::json!({
        "conditions": [serde_json::to_value(condition)?],
        "observedGeneration": serde_json::to_value(observed)?,
    }))
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
        let rendered = render_agent(&agent(Mode::Once)).unwrap();
        assert_eq!(rendered_kind(&rendered), "Job");
    }

    #[test]
    fn controller_picks_deployment_for_reactive() {
        let rendered = render_agent(&agent(Mode::Reactive)).unwrap();
        assert_eq!(rendered_kind(&rendered), "Deployment");
    }

    #[test]
    fn render_error_maps_to_validated_false() {
        let mut a = agent(Mode::Once);
        a.spec.image = None; // classless agent without an image is unrenderable
        let err = render_agent(&a).unwrap_err();
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
        let status =
            desired_fleet_status(&ready_condition(Some(2), "Deployment"), Some(2)).unwrap();
        assert_eq!(status["observedGeneration"], 2);
        assert_eq!(status["conditions"][0]["type"], "Ready");
        assert_eq!(status["conditions"][0]["status"], "True");
        // a fleet status with a matching condition is unchanged (guard works for fleets too)
        let current: agent_api::AgentFleetStatus = serde_json::from_value(status.clone()).unwrap();
        assert!(!status_changed(Some(&current), &status).unwrap());
    }

    #[test]
    fn unsupported_substrate_is_a_render_error_condition() {
        let mut a = agent(Mode::Once);
        a.spec.substrate = Some(Substrate::KataHybrid);
        let err = render_agent(&a).unwrap_err();
        let c = validated_failed_condition(&err.to_string());
        assert!(c.message.unwrap().to_lowercase().contains("substrate"));
    }
}
