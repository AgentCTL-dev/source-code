// SPDX-License-Identifier: BUSL-1.1
//! The `agentctl-operator` binary: run the reconcile [`Controller`] (RFC 0006)
//! as a leader-elected, observable singleton.
//!
//! Watches `Agent`/`AgentFleet` objects and the workloads they own, reconciling
//! each via [`agentctl_operator::controller`]. On top of that this binary adds
//! operator HA + observability:
//!
//! * a **health/metrics** HTTP server ([`serve`]) on `HEALTH_PORT`/`METRICS_PORT`
//!   (default 8080): `/healthz`, `/readyz`, `/metrics` — served by every replica;
//! * **leader election** ([`lease`]) over a `coordination.k8s.io/v1` Lease named
//!   `agentctl-operator`: only the holder runs the controllers; standbys serve
//!   `/healthz` and report `/readyz` 503. Default `replicas: 1`, but safe at >1.
//!
//! Requires a cluster to run; it is compile-checked here without one.

use std::net::SocketAddr;
use std::sync::Arc;

use agent_api::{Agent, AgentFleet};
use agentctl_operator::controller::{
    error_policy, error_policy_fleet, reconcile, reconcile_fleet, Ctx, ScalerConfig,
};
use agentctl_operator::{lease, serve, Metrics};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::coordination::v1::Lease;
use kube::runtime::controller::Error as ControllerError;
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::{watcher, Controller};
use kube::{Api, Client};
use tracing::{debug, error, info};

#[tokio::main]
async fn main() -> Result<(), kube::Error> {
    // Honor RUST_LOG (e.g. `agentctl_operator=debug`); default to info. Adds an
    // OTLP exporter only when OTEL_EXPORTER_OTLP_ENDPOINT is set (else fmt-only).
    agentctl_telemetry::init("agentctl-operator");

    let client = Client::try_default().await?;
    let metrics = Arc::new(Metrics::new());

    // Health/metrics server: bind first and on EVERY replica (leader or standby)
    // so the kubelet liveness probe is answered before — and regardless of —
    // leadership. Mark the manager up now (participating in the election) so
    // /readyz flips to 200 for standbys too: gating readiness on leadership would
    // deadlock a RollingUpdate (the old leader holds the lease until it
    // terminates, but won't terminate until the new pod is Ready). Who actually
    // leads is observable via the agentctl_operator_leader gauge.
    let port = serve::port_from_env();
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    tokio::spawn(serve::serve(addr, metrics.clone()));
    metrics.set_manager_up(true);
    info!(%addr, "serving /healthz, /readyz, /metrics");

    // Leader election (RFC 0006 — operator HA). Identity is the pod name (downward
    // API); the lease lives in the operator's own namespace.
    let identity = std::env::var("POD_NAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "agentctl-operator".to_string());
    let namespace =
        std::env::var("POD_NAMESPACE").unwrap_or_else(|_| client.default_namespace().to_string());
    let leases: Api<Lease> = Api::namespaced(client.clone(), &namespace);

    info!(identity, namespace, "starting leader election");
    // Blocks until this replica wins the lease; spawns the renewer (which exits
    // the process if leadership is later lost, so two replicas never both lead).
    lease::run(
        leases,
        &identity,
        lease::LeaseConfig::default(),
        metrics.clone(),
    )
    .await;

    // Won the lease: run the controllers (set_leader is handled inside lease::run +
    // the renewer; manager_up was already set above so /readyz was 200 while standby).
    // Kubernetes Events recorder (RFC 0010): the operator already holds events
    // RBAC; `reporter.controller` is the controller name and `instance` the pod so
    // events are attributable per-replica.
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: "agentctl-operator".to_string(),
            instance: Some(identity.clone()),
        },
    );

    // KEDA scaler wiring for claim-mode fleets (RFC 0011), read from the operator
    // env (SCALER_ENABLED / SCALER_ADDRESS / COORDINATION_URL). Defaults point at
    // the in-cluster scaler + coordination Services; disable on a non-KEDA cluster.
    let scaler = ScalerConfig::from_env();
    info!(
        enabled = scaler.enabled,
        scaler_address = %scaler.scaler_address,
        coordination_url = %scaler.coordination_url,
        "KEDA scaler config"
    );

    let ctx = Arc::new(Ctx {
        client: client.clone(),
        metrics: metrics.clone(),
        recorder,
        scaler,
    });

    info!("starting agentctl-operator controllers (Agent + AgentFleet)");

    // Agent → Job/Deployment.
    let agent_ctrl = Controller::new(
        Api::<Agent>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(Api::<Job>::all(client.clone()), watcher::Config::default())
    .owns(
        Api::<Deployment>::all(client.clone()),
        watcher::Config::default(),
    )
    .shutdown_on_signal()
    .run(reconcile, error_policy, ctx.clone())
    .for_each(|res| async move {
        match res {
            Ok((obj, action)) => info!(kind = "Agent", ?obj, ?action, "reconciled"),
            // A queued reconcile for an object already gone from the store
            // (the post-delete / finalizer race) is benign — log it quietly.
            Err(e @ ControllerError::ObjectNotFound(_)) => {
                debug!(error = %e, "object gone before reconcile (post-delete race)")
            }
            Err(e) => error!(error = %e, "reconcile loop error"),
        }
    });

    // AgentFleet → Deployment (claim) / StatefulSet (shard).
    let fleet_ctrl = Controller::new(
        Api::<AgentFleet>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(
        Api::<Deployment>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(
        Api::<StatefulSet>::all(client.clone()),
        watcher::Config::default(),
    )
    .shutdown_on_signal()
    .run(reconcile_fleet, error_policy_fleet, ctx.clone())
    .for_each(|res| async move {
        match res {
            Ok((obj, action)) => info!(kind = "AgentFleet", ?obj, ?action, "reconciled"),
            Err(e @ ControllerError::ObjectNotFound(_)) => {
                debug!(error = %e, "object gone before reconcile (post-delete race)")
            }
            Err(e) => error!(error = %e, "reconcile loop error"),
        }
    });

    tokio::join!(agent_ctrl, fleet_ctrl);

    info!("agentctl-operator controllers stopped");
    Ok(())
}
