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
    fleet_selector_string, render_agent, render_fleet, render_scaled_object, RenderError, Rendered,
    DEFAULT_COORDINATION_URL, DEFAULT_SCALER_ADDRESS,
};
