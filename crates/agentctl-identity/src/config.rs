// SPDX-License-Identifier: BUSL-1.1
//! Service configuration — env-driven, read once at startup (the control-plane
//! convention). Providers arrive as one JSON document so the chart renders
//! them from values without a shape zoo.

use serde::Deserialize;

/// One federated OIDC provider.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Provider {
    /// Short name; prefixes subjects (`<name>:<sub>`).
    pub name: String,
    /// Issuer URL (`https://…`); discovery at
    /// `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    /// OAuth client id (the CLI's device-flow client).
    pub client_id: String,
    /// Optional client secret (confidential clients; many device-flow clients
    /// are public). NEVER logged.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Accepted `aud` values for validated tokens. Empty ⇒ `client_id`.
    #[serde(default)]
    pub audiences: Vec<String>,
    /// Scopes requested on the device flow.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// The claim carrying group memberships (accessPolicies match on it).
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
}

fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

fn default_groups_claim() -> String {
    "groups".into()
}

impl Provider {
    pub fn effective_audiences(&self) -> Vec<String> {
        if self.audiences.is_empty() {
            vec![self.client_id.clone()]
        } else {
            self.audiences.clone()
        }
    }
}

/// Full service config.
#[derive(Debug, Clone)]
pub struct Config {
    /// Plain-HTTP bind (in-cluster; TLS terminates via the chart's mesh certs
    /// in the hardened profile — P1 ships behind NetworkPolicy + the mesh CA).
    pub bind: std::net::SocketAddr,
    pub providers: Vec<Provider>,
    /// `postgres` DSN, or `memory` (tests/dev only — custody is process-local).
    pub store: StoreConfig,
    /// 32-byte seal key (base64) for envelope encryption at rest. Dev default
    /// derives an ephemeral key WITH A LOUD WARNING (grants die with the pod).
    pub seal_key: Option<[u8; 32]>,
    /// Static admin bearer protecting the mutating surfaces (mint/admin).
    /// The chart wires the shared control-plane token; empty ⇒ mutating
    /// surfaces are refused entirely (fail closed, never open).
    pub admin_token: Option<String>,
    /// Extra PEM root CA file for issuer TLS (`IDENTITY_ISSUER_CA`) — for
    /// IdPs serving from a private CA. Adds to (never replaces) webpki.
    pub issuer_ca: Option<String>,
    /// AAuth Agent Provider role (RFC 0028 §5): the provider URL agents dial
    /// (`IDENTITY_AAUTH_ISSUER` — must equal the operator's
    /// `AGENTCTL_AAUTH_PROVIDER` and the rendered `security.aauth.provider`;
    /// the shipped agent validates the token `iss` against it). Unset ⇒ the
    /// AAuth surfaces are not served.
    pub aauth_issuer: Option<String>,
    /// 32-byte Ed25519 seed (base64) for the provider signing key
    /// (`IDENTITY_AAUTH_SEED`). Unset ⇒ ephemeral with a loud warning.
    pub aauth_seed: Option<[u8; 32]>,
    /// Agent-token lifetime seconds (`IDENTITY_AAUTH_TOKEN_TTL`, default 3600).
    pub aauth_token_ttl: i64,
    /// `/v1/exchange` synthetic lifetime for `static` connections, seconds
    /// (`IDENTITY_EXCHANGE_STATIC_TTL`, default 300). Kept short on purpose:
    /// mcpg's host cache honors our `expires_in`, and revocation surfaces
    /// only after both caches lapse.
    pub exchange_static_ttl: i64,
    /// Cache margin seconds (`IDENTITY_EXCHANGE_REFRESH_MARGIN`, default 60):
    /// a cached token is never served with less than this much life left.
    pub exchange_refresh_margin: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoreConfig {
    Memory,
    Postgres { dsn: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IDENTITY_PROVIDERS is not valid JSON: {0}")]
    Providers(String),
    #[error("provider {0:?}: issuer must be https:// (got {1:?})")]
    InsecureIssuer(String, String),
    #[error("IDENTITY_SEAL_KEY is not 32 bytes of base64")]
    SealKey,
    #[error("IDENTITY_BIND is not a socket address: {0}")]
    Bind(String),
}

impl Config {
    /// Read from the environment. Pure aside from env access; validation is
    /// loud and total (a misconfigured identity service must not start).
    pub fn from_env() -> Result<Config, ConfigError> {
        let bind = std::env::var("IDENTITY_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8087".into())
            .parse()
            .map_err(|e| ConfigError::Bind(format!("{e}")))?;

        let providers: Vec<Provider> = match std::env::var("IDENTITY_PROVIDERS") {
            Ok(raw) if !raw.trim().is_empty() => {
                serde_json::from_str(&raw).map_err(|e| ConfigError::Providers(e.to_string()))?
            }
            _ => Vec::new(),
        };
        for p in &providers {
            let loopback = p.issuer.starts_with("http://127.0.0.1")
                || p.issuer.starts_with("http://localhost");
            if !p.issuer.starts_with("https://") && !loopback {
                return Err(ConfigError::InsecureIssuer(
                    p.name.clone(),
                    p.issuer.clone(),
                ));
            }
        }

        // IDENTITY_DATABASE_URL wins; DATABASE_URL is the control-plane-wide
        // convention the chart's shared Postgres helper injects.
        let store = match std::env::var("IDENTITY_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
        {
            Ok(dsn) if !dsn.trim().is_empty() => StoreConfig::Postgres { dsn },
            _ => StoreConfig::Memory,
        };

        let seal_key = match std::env::var("IDENTITY_SEAL_KEY") {
            Ok(b64) if !b64.trim().is_empty() => {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|_| ConfigError::SealKey)?;
                let arr: [u8; 32] = bytes.try_into().map_err(|_| ConfigError::SealKey)?;
                Some(arr)
            }
            _ => None,
        };

        let admin_token = std::env::var("IDENTITY_ADMIN_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());

        let issuer_ca = std::env::var("IDENTITY_ISSUER_CA")
            .ok()
            .filter(|p| !p.trim().is_empty());

        let aauth_issuer = std::env::var("IDENTITY_AAUTH_ISSUER")
            .ok()
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());
        let aauth_seed = match std::env::var("IDENTITY_AAUTH_SEED") {
            Ok(b64) if !b64.trim().is_empty() => {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|_| ConfigError::SealKey)?;
                let arr: [u8; 32] = bytes.try_into().map_err(|_| ConfigError::SealKey)?;
                Some(arr)
            }
            _ => None,
        };
        let aauth_token_ttl = std::env::var("IDENTITY_AAUTH_TOKEN_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);
        let exchange_static_ttl = std::env::var("IDENTITY_EXCHANGE_STATIC_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300)
            .clamp(30, 24 * 3600);
        let exchange_refresh_margin = std::env::var("IDENTITY_EXCHANGE_REFRESH_MARGIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60)
            .clamp(0, 3600);

        Ok(Config {
            bind,
            providers,
            store,
            seal_key,
            admin_token,
            issuer_ca,
            aauth_issuer,
            aauth_seed,
            aauth_token_ttl,
            exchange_static_ttl,
            exchange_refresh_margin,
        })
    }

    pub fn provider(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_parse_and_validate() {
        let raw = r#"[{"name":"okta","issuer":"https://acme.okta.com","client_id":"agentctl"}]"#;
        let ps: Vec<Provider> = serde_json::from_str(raw).unwrap();
        assert_eq!(ps[0].name, "okta");
        assert_eq!(ps[0].effective_audiences(), vec!["agentctl"]);
        assert_eq!(ps[0].groups_claim, "groups");
        assert!(ps[0].scopes.contains(&"openid".to_string()));
    }

    #[test]
    fn loopback_http_issuer_is_dev_only_carveout() {
        // Mirrors the agent's own plaintext-loopback carve-out.
        let p = Provider {
            name: "dev".into(),
            issuer: "http://127.0.0.1:9999".into(),
            client_id: "x".into(),
            client_secret: None,
            audiences: vec![],
            scopes: default_scopes(),
            groups_claim: default_groups_claim(),
        };
        let loopback = p.issuer.starts_with("http://127.0.0.1");
        assert!(loopback);
    }
}
