// SPDX-License-Identifier: BUSL-1.1
//! The `agentctl-operator` binary: run the reconcile [`Controller`] as a
//! leader-elected, observable singleton.
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

use agent_api::v1alpha2::{Agent, AgentFleet};
use agent_api::Organization;
use agentctl_operator::controller::{
    error_policy, error_policy_fleet, reconcile, reconcile_fleet, ApiTokenConfig, Ctx, ScalerConfig,
};
use agentctl_operator::org::{error_policy_org, reconcile_org};
use agentctl_operator::supervisor::{error_policy_supervisor, reconcile_supervisor};
use agentctl_operator::{lease, serve, Metrics};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Namespace;
use kube::runtime::controller::Error as ControllerError;
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::{watcher, Controller};
use kube::{Api, Client};
use tracing::{debug, error, info, warn};

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

    // Leader election for operator HA. Identity is the pod name (downward
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
    // Kubernetes Events recorder: the operator already holds events RBAC;
    // `reporter.controller` is the controller name and `instance` the pod so
    // events are attributable per-replica.
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: "agentctl-operator".to_string(),
            instance: Some(identity.clone()),
        },
    );

    // KEDA scaler wiring for claim-mode fleets, read from the operator
    // env (SCALER_ENABLED / SCALER_ADDRESS / COORDINATION_URL). Defaults point at
    // the in-cluster scaler + coordination Services; disable on a non-KEDA cluster.
    let scaler = ScalerConfig::from_env();
    info!(
        enabled = scaler.enabled,
        scaler_address = %scaler.scaler_address,
        coordination_url = %scaler.coordination_url,
        "KEDA scaler config"
    );

    // Optional in-cluster bearer-token injection (chart apiToken.enabled), read
    // from API_TOKEN_ENABLED + POD_NAMESPACE. When on, the operator injects
    // AGENTCTL_API_TOKEN (a secretKeyRef on agentctl-api-token) into rendered
    // agent pods — but ONLY for agents in the control-plane namespace, since a
    // secretKeyRef cannot cross namespaces. Default off (no injection).
    let api_token = ApiTokenConfig::from_env();
    info!(
        enabled = api_token.enabled,
        namespace = ?api_token.namespace,
        "API token injection config"
    );

    let render = agentctl_operator::RenderConfig::from_env();
    info!(gateway = %render.gateway_url, "render config (agents dial intelligence + MCP directly; no gateways)");
    let pki = agentctl_operator::pki::PkiConfig::from_env();
    info!(
        issuer = ?pki.issuer,
        ca_loaded = pki.ca_pem.is_some(),
        enabled = pki.enabled(),
        "workload PKI config"
    );
    let netpol = agentctl_operator::netpol::NetPolConfig::from_env();
    info!(
        enabled = netpol.enabled,
        control_plane_ns = ?netpol.control_plane_ns,
        active = netpol.active(),
        "agent NetworkPolicy config"
    );
    if netpol.enabled && !netpol.active() {
        warn!(
            "NETWORK_POLICIES_ENABLED set but POD_NAMESPACE is unset/empty: agent \
             NetworkPolicies will NOT be reconciled (cannot scope gateway egress \
             without the control-plane namespace). Set POD_NAMESPACE (downward API \
             metadata.namespace)."
        );
    }
    let aauth = agentctl_operator::aauth::AauthConfig::from_env();
    info!(
        provider = ?aauth.provider,
        admin_ready = aauth.admin_ready(),
        "aauth house-provisioning config"
    );
    if aauth.provider.is_some() && !aauth.admin_ready() {
        warn!(
            "AGENTCTL_AAUTH_PROVIDER set but the admin channel is unusable (set \
             AGENTCTL_AAUTH_ADMIN_TOKEN_FILE to the mounted apd admin token): \
             identity.aauth Agents will be held Validated=False."
        );
    }
    let identity = agentctl_operator::identity::IdentityConfig::from_env();
    info!(
        url = ?identity.url,
        admin_token = identity.admin_token.is_some(),
        "identity-service (principal minting) config"
    );
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        metrics: metrics.clone(),
        recorder,
        scaler,
        api_token,
        render,
        pki,
        netpol,
        aauth,
        identity,
        identity_http: agentctl_operator::identity::http_client(),
        tenant_mcpg: agentctl_operator::tenant_mcpg::TenantMcpgConfig::from_env(),
    });

    info!("starting agentctl-operator controllers (Agent + AgentFleet + Organization)");

    // Agent → Job/Deployment.
    let agent_ctrl = Controller::new(
        Api::<Agent>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(Api::<Job>::all(client.clone()), watcher::Config::default())
    .owns(
        Api::<CronJob>::all(client.clone()),
        watcher::Config::default(),
    )
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

    // Organization → managed namespaces + quotas (tenancy root, RFC 0033 §2.1).
    // The CRD may lag the operator on upgraded clusters, so a missing
    // organizations.agentctl.dev must not take the whole binary down: probe
    // for it and run the tenancy controller only when present.
    let orgs_api = Api::<Organization>::all(client.clone());
    let org_crd_present = orgs_api
        .list(&Default::default())
        .await
        .map(|_| true)
        .unwrap_or_else(|e| {
            warn!(error = %e, "Organization CRD not listable — tenancy controller disabled (install deploy/crds/organization.yaml)");
            false
        });
    let org_ctrl = async {
        if !org_crd_present {
            return;
        }
        Controller::new(orgs_api, watcher::Config::default())
            .owns(
                Api::<Namespace>::all(client.clone()),
                watcher::Config::default(),
            )
            .shutdown_on_signal()
            .run(reconcile_org, error_policy_org, ctx.clone())
            .for_each(|res| async move {
                match res {
                    Ok((obj, action)) => info!(kind = "Organization", ?obj, ?action, "reconciled"),
                    Err(e @ ControllerError::ObjectNotFound(_)) => {
                        debug!(error = %e, "object gone before reconcile (post-delete race)")
                    }
                    Err(e) => error!(error = %e, "reconcile loop error"),
                }
            })
            .await
    };

    // Supervisor → its rendered Agent (RFC 0027 §2). Same CRD-presence gate
    // as the tenancy controller: an upgraded cluster without the CRD keeps a
    // working operator.
    let sup_api = Api::<agent_api::v1alpha2::Supervisor>::all(client.clone());
    let sup_crd_present = sup_api
        .list(&Default::default())
        .await
        .map(|_| true)
        .unwrap_or_else(|e| {
            warn!(error = %e, "Supervisor CRD not listable — supervisor controller disabled (install deploy/crds/supervisor.yaml)");
            false
        });
    let sup_ctrl = async {
        if !sup_crd_present {
            return;
        }
        Controller::new(sup_api, watcher::Config::default())
            .owns(
                Api::<agent_api::v1alpha2::Agent>::all(client.clone()),
                watcher::Config::default(),
            )
            .shutdown_on_signal()
            .run(reconcile_supervisor, error_policy_supervisor, ctx.clone())
            .for_each(|res| async move {
                match res {
                    Ok((obj, action)) => info!(kind = "Supervisor", ?obj, ?action, "reconciled"),
                    Err(e @ ControllerError::ObjectNotFound(_)) => {
                        debug!(error = %e, "object gone before reconcile (post-delete race)")
                    }
                    Err(e) => error!(error = %e, "reconcile loop error"),
                }
            })
            .await
    };

    tokio::join!(agent_ctrl, fleet_ctrl, org_ctrl, sup_ctrl);

    info!("agentctl-operator controllers stopped");
    Ok(())
}
