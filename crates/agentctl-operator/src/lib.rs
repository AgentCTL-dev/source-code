// SPDX-License-Identifier: BUSL-1.1
//! # agentctl-operator
//!
//! The agentctl operator. It pairs a **pure rendering core** ([`render`]) — the
//! deterministic mapping from an [`agent_api::Agent`] to its Kubernetes workload
//! (mode→workload and substrate wiring) — with the level-triggered [`controller`]
//! that server-side-applies that workload and patches `Agent.status`.

pub mod aauth;
pub mod controller;
pub mod lease;
pub mod metrics;
pub mod netpol;
pub mod pki;
pub mod render;
pub mod serve;

pub use metrics::Metrics;
pub use render::{
    coordinator_name, fleet_selector_string, inject_aauth, inject_api_token, inject_mcp_servers,
    inject_workflow, render_agent, render_coordinator, render_fleet, render_scaled_object,
    serving_secret_name, workflow_configmap_name, McpBinding, RenderConfig, RenderError, Rendered,
    API_TOKEN_ENV, API_TOKEN_SECRET, CA_CONFIGMAP, CA_KEY, DEFAULT_COORDINATION_URL,
    DEFAULT_GATEWAY_URL, DEFAULT_MCPGATEWAY_URL, DEFAULT_MODELGATEWAY_URL, DEFAULT_SCALER_ADDRESS,
};
