// SPDX-License-Identifier: BUSL-1.1
//! # Identity-service client (RFC 0029 §3, P1-5)
//!
//! The gateway's inbound-authn channel for the org route family
//! (`/orgs/<org>/…`): a presented bearer is introspected at the identity
//! service (`POST /v1/introspect`, admin-gated), yielding the federated
//! subject + groups. Deliberately a SEPARATE env pair from the coarse
//! data-plane gate: `AGENTCTL_API_TOKEN` arms that gate, so the identity
//! admin channel rides `AGENTCTL_IDENTITY_TOKEN` — enabling org-route authn
//! must not silently start gating the legacy routes.

use serde::Deserialize;

#[derive(Clone, Debug, Default)]
pub struct IdentityConfig {
    /// Base URL (`AGENTCTL_IDENTITY_URL`); unset ⇒ org routes refuse 503.
    pub url: Option<String>,
    /// Admin bearer for `/v1/introspect` (`AGENTCTL_IDENTITY_TOKEN`).
    pub admin_token: Option<String>,
}

impl IdentityConfig {
    pub fn from_env() -> IdentityConfig {
        IdentityConfig {
            url: std::env::var("AGENTCTL_IDENTITY_URL")
                .ok()
                .map(|u| u.trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty()),
            admin_token: std::env::var("AGENTCTL_IDENTITY_TOKEN")
                .ok()
                .filter(|t| !t.trim().is_empty()),
        }
    }

    pub fn ready(&self) -> bool {
        self.url.is_some()
    }
}

/// The identity the service resolved for an inbound bearer.
#[derive(Debug, Clone, Deserialize)]
pub struct OrgUser {
    /// Provider-prefixed stable subject (`okta:alice`) — the SAME string the
    /// operator minted the per-agent principal under.
    pub subject: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
}

/// Introspect a presented bearer. `Ok(None)` = the service answered and the
/// token is NOT active (a clean 401 for the caller); `Err` = the channel
/// itself failed (a 502/503 for the caller — never fail open).
pub async fn introspect(
    http: &reqwest::Client,
    cfg: &IdentityConfig,
    token: &str,
) -> Result<Option<OrgUser>, String> {
    let url = cfg
        .url
        .as_deref()
        .ok_or_else(|| "identity service not configured (AGENTCTL_IDENTITY_URL)".to_string())?;
    let mut req = http
        .post(format!("{url}/v1/introspect"))
        .json(&serde_json::json!({ "token": token }));
    if let Some(t) = &cfg.admin_token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("identity introspect: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("identity introspect: unreadable response: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "identity introspect refused ({status}): {}",
            body["error"].as_str().unwrap_or("unexpected response")
        ));
    }
    if body["active"] != serde_json::Value::Bool(true) {
        return Ok(None);
    }
    serde_json::from_value::<OrgUser>(body["identity"].clone())
        .map(Some)
        .map_err(|e| format!("identity introspect: malformed identity: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_user_parses_the_introspection_identity() {
        let u: OrgUser = serde_json::from_value(serde_json::json!({
            "subject": "mock:alice", "provider": "mock", "sub": "alice",
            "email": "alice@example.test", "groups": ["eng"], "scopes": ["openid"], "exp": 1
        }))
        .unwrap();
        assert_eq!(u.subject, "mock:alice");
        assert_eq!(u.groups, vec!["eng"]);
    }

    #[test]
    fn unready_config_refuses_rather_than_failing_open() {
        assert!(!IdentityConfig::default().ready());
    }
}
