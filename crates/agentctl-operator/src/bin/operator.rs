//! The `agentctl-operator` binary: run the reconcile [`Controller`] (RFC 0006).
//!
//! Watches `Agent` objects and the `Job`/`Deployment` workloads they own, and
//! reconciles each via [`agentctl_operator::controller`]. Requires a cluster to
//! run; it is compile-checked here without one.

use std::sync::Arc;

use agent_api::{Agent, AgentFleet};
use agentctl_operator::controller::{
    error_policy, error_policy_fleet, reconcile, reconcile_fleet, Ctx,
};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::Job;
use kube::runtime::controller::Error as ControllerError;
use kube::runtime::{watcher, Controller};
use kube::{Api, Client};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), kube::Error> {
    // Honor RUST_LOG (e.g. `agentctl_operator=debug`); default to info.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let client = Client::try_default().await?;
    let ctx = Arc::new(Ctx {
        client: client.clone(),
    });

    info!("starting agentctl-operator controllers (Agent + AgentFleet)");

    // Agent → Job/Deployment.
    let agent_ctrl = Controller::new(Api::<Agent>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Job>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Deployment>::all(client.clone()), watcher::Config::default())
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
    let fleet_ctrl =
        Controller::new(Api::<AgentFleet>::all(client.clone()), watcher::Config::default())
            .owns(Api::<Deployment>::all(client.clone()), watcher::Config::default())
            .owns(Api::<StatefulSet>::all(client.clone()), watcher::Config::default())
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
