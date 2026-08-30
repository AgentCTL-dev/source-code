// SPDX-License-Identifier: BUSL-1.1
//! # agentctl-operator
//!
//! The agentctl operator. It pairs a **pure rendering core** ([`render`]) — the
//! deterministic mapping from an [`agent_api::Agent`] to its Kubernetes workload
//! (mode→workload and substrate wiring) — with the level-triggered [`controller`]
//! that server-side-applies that workload and patches `Agent.status`.

pub mod aauth;
pub mod controller;
pub mod identity;
pub mod lease;
pub mod metrics;
pub mod netpol;
pub mod org;
pub mod pki;
pub mod reload;
pub mod render;
pub mod serve;
pub mod supervisor;
pub mod tenant_mcpg;

pub use metrics::Metrics;
pub use render::{
    config_configmap_name, coordinator_name, fleet_peer_endpoint, fleet_selector_string,
    render_agent, render_coordinator, render_fleet, render_scaled_object, serving_secret_name,
    workflow_configmap_name, PodWiring, RenderConfig, RenderError, Rendered, SecretEnv,
    WorkflowMount, API_TOKEN_ENV, API_TOKEN_SECRET, CA_CONFIGMAP, CA_KEY, DEFAULT_COORDINATION_URL,
    DEFAULT_GATEWAY_URL, DEFAULT_SCALER_ADDRESS,
};
