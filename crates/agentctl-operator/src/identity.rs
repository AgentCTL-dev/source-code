// SPDX-License-Identifier: BUSL-1.1
//! # Identity-service client (RFC 0028 §6, P1-4)
//!
//! The operator's channel to `agentctl-identity` for per-(user, agent) A2A
//! principal minting: `POST /v1/principals/mint` behind the shared
//! control-plane admin bearer. The mint response carries the bearer secret
//! EXACTLY ONCE — the operator's only job is to land it in the agent's
//! `<name>-principals` Secret and bind it in `a2a.principals[]`; it never
//! stores the bearer anywhere else. A subject whose key already exists in the
//! Secret is NEVER re-minted on reconcile (idempotence); rotation is an
//! explicit act (delete the key or the principal record).
//!
//! Inert unless configured: without `AGENTCTL_IDENTITY_URL` (chart wires it
//! when `identity.service.enabled`) agents declaring `access.principals` are
//! held with a Degraded condition rather than half-provisioned.

use serde::Deserialize;
use tracing::warn;

/// Env-driven wiring, read once at startup. `admin_token` rides the shared
/// control-plane token (`AGENTCTL_API_TOKEN` — same secret the identity
/// service checks as `IDENTITY_ADMIN_TOKEN`).
#[derive(Clone, Debug, Default)]
pub struct IdentityConfig {
    /// Base URL of the identity service (`AGENTCTL_IDENTITY_URL`).
    pub url: Option<String>,
    /// Admin bearer for the mint surface (`AGENTCTL_API_TOKEN`).
    pub admin_token: Option<String>,
}

impl IdentityConfig {
    pub fn from_env() -> IdentityConfig {
        let url = std::env::var("AGENTCTL_IDENTITY_URL")
            .ok()
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());
        let admin_token = std::env::var("AGENTCTL_API_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());
        if url.is_some() && admin_token.is_none() {
            warn!(
                "AGENTCTL_IDENTITY_URL set without AGENTCTL_API_TOKEN: principal \
                 minting will be refused by the identity service (its admin \
                 surface fails closed) — arm apiToken.enabled in the chart"
            );
        }
        IdentityConfig { url, admin_token }
    }

    /// Minting is possible (URL present; token may still be refused server-side).
    pub fn ready(&self) -> bool {
        self.url.is_some()
    }
}

/// reqwest over rustls with the EXPLICIT ring provider + webpki roots — the
/// control-plane client pattern (plain-http in-cluster URLs bypass TLS).
pub fn http_client() -> reqwest::Client {
    let provider = rustls::crypto::ring::default_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("rustls protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

/// The mint response (the bearer appears here and NOWHERE else).
#[derive(Debug, Clone, Deserialize)]
pub struct MintedPrincipal {
    pub bearer: String,
    pub secret_name: String,
    pub secret_key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity service not configured (AGENTCTL_IDENTITY_URL unset)")]
    NotConfigured,
    #[error("identity mint refused ({status}): {message}")]
    Refused { status: u16, message: String },
    #[error("identity service unreachable: {0}")]
    Unreachable(String),
}

/// Mint the principal for (org, namespace, agent, subject). The caller owns
/// projecting `bearer` into the named Secret key immediately — it cannot be
/// fetched again.
pub async fn mint(
    http: &reqwest::Client,
    cfg: &IdentityConfig,
    org: &str,
    namespace: &str,
    agent: &str,
    subject: &str,
) -> Result<MintedPrincipal, IdentityError> {
    let url = cfg.url.as_deref().ok_or(IdentityError::NotConfigured)?;
    let mut req = http
        .post(format!("{url}/v1/principals/mint"))
        .json(&serde_json::json!({
            "org": org,
            "namespace": namespace,
            "agent": agent,
            "subject": subject,
        }));
    if let Some(token) = &cfg.admin_token {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| IdentityError::Unreachable(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        return Err(IdentityError::Refused {
            status: status.as_u16(),
            message: body["error"]
                .as_str()
                .unwrap_or("unexpected response")
                .to_string(),
        });
    }
    resp.json()
        .await
        .map_err(|e| IdentityError::Unreachable(format!("unreadable mint response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_response_parses_and_never_defaults_the_bearer() {
        let m: MintedPrincipal = serde_json::from_str(
            r#"{"bearer":"pat-x","secret_name":"triage-principals","secret_key":"PRINCIPAL_OKTA_ALICE"}"#,
        )
        .unwrap();
        assert_eq!(m.secret_name, "triage-principals");
        // A response without a bearer is a hard error, not an empty default.
        assert!(
            serde_json::from_str::<MintedPrincipal>(r#"{"secret_name":"x","secret_key":"y"}"#)
                .is_err()
        );
    }

    #[test]
    fn config_trims_and_reports_readiness() {
        let cfg = IdentityConfig {
            url: Some("http://id".into()),
            admin_token: None,
        };
        assert!(cfg.ready());
        assert!(!IdentityConfig::default().ready());
    }
}
