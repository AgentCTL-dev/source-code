// SPDX-License-Identifier: BUSL-1.1
//! # agentctl-identity — the identity plane (RFC 0028)
//!
//! Four duties, one service:
//! 1. **Federation** — external OIDC IdPs (Auth0/Keycloak/Okta/generic) are the
//!    only source of human identity: discovery, the device flow for the CLI,
//!    JWKS validation for every other plane.
//! 2. **Custody** — long-lived grants (refresh tokens, later agent keys) exist
//!    ONLY here, envelope-encrypted at rest; agents hold their own workload
//!    identity and nothing else.
//! 3. **Exchange** — audience-scoped short-lived tokens minted per call
//!    (RFC 8693 where the IdP supports it; connection-based otherwise). P1
//!    ships the seam; P5 wires the mcpg credential issuer onto it.
//! 4. **Principal minting** — per-(user, agent) A2A bearer secrets the
//!    operator projects into `a2a.principals[]` and the gateway injects, so
//!    addressed gates and per-user quotas work natively at the agent.
//!
//! P1 scope implemented here: providers + discovery + JWKS validation, the
//! device flow, principal mint/verify, and the sealed store. The service is
//! deliberately the smallest crown jewel: strictest NetworkPolicy, no value
//! ever logged, every secret column sealed.

pub mod aauth;
pub mod config;
pub mod exchange;
pub mod http;
pub mod oidc;
pub mod principals;
pub mod seal;
pub mod store;

pub use config::Config;

/// Resolved identity facts the rest of the control plane consumes — the output
/// of `/v1/introspect` and the input to access-policy resolution (RFC 0033).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Identity {
    /// Stable subject (`<provider>:<sub>`), the user key everywhere.
    pub subject: String,
    /// The provider that vouched for it.
    pub provider: String,
    /// Raw `sub` at the provider.
    pub sub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Group claims as the IdP presented them (accessPolicies match on these).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Token scopes (may narrow a session below the policy ceiling).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Expiry (unix seconds) of the presented token.
    pub exp: i64,
}
