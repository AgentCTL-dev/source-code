//! # agentctl-operator
//!
//! The agentctl operator (agentctl RFC 0006). It pairs a **pure rendering core**
//! ([`render`]) — the deterministic mapping from an [`agent_api::Agent`] to its
//! Kubernetes workload (mode→workload, RFC 0003 §5; substrate wiring, RFC 0002)
//! — with the level-triggered [`controller`] that server-side-applies that
//! workload and patches `Agent.status`.

pub mod controller;
pub mod render;

pub use render::{render_agent, RenderError, Rendered};
