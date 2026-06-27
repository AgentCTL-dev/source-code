//! The `agentctl-operator` binary: run the reconcile [`Controller`] (RFC 0006).
//!
//! Watches `Agent` objects and the `Job`/`Deployment` workloads they own, and
//! reconciles each via [`agentctl_operator::controller`]. Requires a cluster to
//! run; it is compile-checked here without one.

use std::sync::Arc;

use agent_api::Agent;
use agentctl_operator::controller::{error_policy, reconcile, Ctx};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
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

    let agents = Api::<Agent>::all(client.clone());
    let jobs = Api::<Job>::all(client.clone());
    let deployments = Api::<Deployment>::all(client.clone());

    info!("starting agentctl-operator controller");
    Controller::new(agents, watcher::Config::default())
        .owns(jobs, watcher::Config::default())
        .owns(deployments, watcher::Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, Arc::new(Ctx { client }))
        .for_each(|res| async move {
            match res {
                Ok((obj, action)) => info!(?obj, ?action, "reconciled"),
                // A queued reconcile for an object already gone from the store
                // (the post-delete / finalizer race) is benign — log it quietly.
                Err(e @ ControllerError::ObjectNotFound(_)) => {
                    debug!(error = %e, "object gone before reconcile (post-delete race)")
                }
                Err(e) => error!(error = %e, "reconcile loop error"),
            }
        })
        .await;

    info!("agentctl-operator controller stopped");
    Ok(())
}
