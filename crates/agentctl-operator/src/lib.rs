// SPDX-License-Identifier: BUSL-1.1
//! # agentctl-operator
//!
//! The agentctl operator (agentctl RFC 0006). It pairs a **pure rendering core**
//! ([`render`]) — the deterministic mapping from an [`agent_api::Agent`] to its
//! Kubernetes workload (mode→workload, RFC 0003 §5; substrate wiring, RFC 0002)
//! — with the level-triggered [`controller`] that server-side-applies that
//! workload and patches `Agent.status`.

pub mod controller;
pub mod lease;
pub mod metrics;
pub mod render;
pub mod serve;

pub use metrics::Metrics;
pub use render::{
    fleet_selector_string, inject_api_token, render_agent, render_fleet, render_scaled_object,
    serving_secret_name, RenderConfig, RenderError, Rendered, API_TOKEN_ENV, API_TOKEN_SECRET,
    CA_CONFIGMAP, CA_KEY, DEFAULT_COORDINATION_URL, DEFAULT_MODELGATEWAY_URL,
    DEFAULT_SCALER_ADDRESS,
};
