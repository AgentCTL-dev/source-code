// SPDX-License-Identifier: BUSL-1.1
//! OIDC federation: discovery, JWKS-backed token validation, and the device
//! flow (the CLI's login). Discovery documents and JWKS are cached per
//! provider with a refresh-on-unknown-kid fallback; every outbound dial is
//! issuer-pinned (we fetch only URLs the issuer's own discovery document
//! names, and the document must come from the configured issuer).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::Provider;
use crate::Identity;

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("provider {0:?} is not configured")]
    UnknownProvider(String),
    #[error("discovery: {0}")]
    Discovery(String),
    #[error("device flow: {0}")]
    Device(String),
    #[error("authorization pending")]
    AuthorizationPending,
    #[error("slow down")]
    SlowDown,
    #[error("token invalid: {0}")]
    Invalid(String),
}

/// The subset of the discovery document we use.
#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
    pub token_endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Jwks {
    keys: Vec<serde_json::Value>,
}

/// Per-provider cached state.
struct ProviderState {
    discovery: Option<Discovery>,
    /// kid → decoding key (+alg), rebuilt on refresh.
    keys: HashMap<String, (DecodingKey, Algorithm)>,
    fetched_unix: i64,
}

/// The federation client: validation + device flow over cached discovery.
pub struct Federation {
    http: reqwest::Client,
    providers: Vec<Provider>,
    state: RwLock<HashMap<String, Arc<RwLock<ProviderState>>>>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Build the shared outbound client (webpki roots, rustls/ring, no proxies).
pub fn outbound_client() -> reqwest::Client {
    outbound_client_with_extra_roots(&[]).expect("webpki roots always parse")
}

/// [`outbound_client`] plus additional PEM root certificates — for IdPs
/// serving from a private CA (an in-cluster Keycloak, the e2e mock issuer).
/// The extra roots ADD to webpki; public issuers keep working.
pub fn outbound_client_with_extra_roots(extra_pem: &[u8]) -> Result<reqwest::Client, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if !extra_pem.is_empty() {
        let mut added = 0usize;
        use rustls_pki_types::pem::PemObject as _;
        for cert in rustls_pki_types::CertificateDer::pem_slice_iter(extra_pem) {
            let cert = cert.map_err(|e| format!("issuer CA: bad PEM: {e:?}"))?;
            roots
                .add(cert)
                .map_err(|e| format!("issuer CA: rejected certificate: {e}"))?;
            added += 1;
        }
        if added == 0 {
            return Err("issuer CA: no certificates found in PEM".into());
        }
    }
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("build outbound client: {e}"))
}

impl Federation {
    pub fn new(http: reqwest::Client, providers: Vec<Provider>) -> Federation {
        Federation {
            http,
            providers,
            state: RwLock::new(HashMap::new()),
        }
    }

    /// Provider names, for the CLI's login picker (names only — no secrets).
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.iter().map(|p| p.name.clone()).collect()
    }

    pub fn provider(&self, name: &str) -> Result<&Provider, OidcError> {
        self.providers
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| OidcError::UnknownProvider(name.to_string()))
    }

    async fn provider_state(&self, name: &str) -> Arc<RwLock<ProviderState>> {
        let mut map = self.state.write().await;
        map.entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(RwLock::new(ProviderState {
                    discovery: None,
                    keys: HashMap::new(),
                    fetched_unix: 0,
                }))
            })
            .clone()
    }

    /// Fetch (or return cached) discovery for a provider. Issuer-pinned: the
    /// document's `issuer` must equal the configured issuer or we refuse it.
    pub async fn discovery(&self, name: &str) -> Result<Discovery, OidcError> {
        let p = self.provider(name)?.clone();
        let state = self.provider_state(name).await;
        if let Some(d) = state.read().await.discovery.clone() {
            return Ok(d);
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            p.issuer.trim_end_matches('/')
        );
        let d: Discovery = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| OidcError::Discovery(format!("GET {url}: {e}")))?
            .error_for_status()
            .map_err(|e| OidcError::Discovery(format!("{e}")))?
            .json()
            .await
            .map_err(|e| OidcError::Discovery(format!("parse: {e}")))?;
        if d.issuer.trim_end_matches('/') != p.issuer.trim_end_matches('/') {
            return Err(OidcError::Discovery(format!(
                "issuer mismatch: document says {:?}, configured {:?} — refusing (host-poisoning guard)",
                d.issuer, p.issuer
            )));
        }
        state.write().await.discovery = Some(d.clone());
        Ok(d)
    }

    /// (Re)load the JWKS for a provider.
    async fn refresh_jwks(&self, name: &str) -> Result<(), OidcError> {
        let d = self.discovery(name).await?;
        let jwks: Jwks = self
            .http
            .get(&d.jwks_uri)
            .send()
            .await
            .map_err(|e| OidcError::Discovery(format!("jwks: {e}")))?
            .error_for_status()
            .map_err(|e| OidcError::Discovery(format!("jwks: {e}")))?
            .json()
            .await
            .map_err(|e| OidcError::Discovery(format!("jwks parse: {e}")))?;
        let mut keys = HashMap::new();
        for k in jwks.keys {
            let kid = k.get("kid").and_then(|v| v.as_str()).unwrap_or_default();
            let kty = k.get("kty").and_then(|v| v.as_str()).unwrap_or_default();
            let alg = match k.get("alg").and_then(|v| v.as_str()) {
                Some("RS256") | None if kty == "RSA" => Algorithm::RS256,
                Some("RS384") => Algorithm::RS384,
                Some("RS512") => Algorithm::RS512,
                Some("ES256") => Algorithm::ES256,
                Some("EdDSA") => Algorithm::EdDSA,
                _ => continue,
            };
            let key = match kty {
                "RSA" => {
                    let n = k.get("n").and_then(|v| v.as_str()).unwrap_or_default();
                    let e = k.get("e").and_then(|v| v.as_str()).unwrap_or_default();
                    match DecodingKey::from_rsa_components(n, e) {
                        Ok(dk) => dk,
                        Err(_) => continue,
                    }
                }
                _ => continue, // EC/OKP keys land with the AAuth provider work
            };
            if !kid.is_empty() {
                keys.insert(kid.to_string(), (key, alg));
            }
        }
        let state = self.provider_state(name).await;
        let mut s = state.write().await;
        s.keys = keys;
        s.fetched_unix = now_unix();
        Ok(())
    }

    /// Validate a bearer JWT against a provider's JWKS + issuer/audience/exp.
    /// Unknown `kid` triggers ONE JWKS refresh (rotation) before failing.
    pub async fn validate(&self, name: &str, token: &str) -> Result<Identity, OidcError> {
        let p = self.provider(name)?.clone();
        let header =
            decode_header(token).map_err(|e| OidcError::Invalid(format!("header: {e}")))?;
        let kid = header.kid.unwrap_or_default();

        let state = self.provider_state(name).await;
        let need_refresh = { !state.read().await.keys.contains_key(&kid) };
        if need_refresh {
            self.refresh_jwks(name).await?;
        }
        let s = state.read().await;
        let (key, alg) = s
            .keys
            .get(&kid)
            .ok_or_else(|| OidcError::Invalid(format!("unknown kid {kid:?}")))?;

        let mut validation = Validation::new(*alg);
        validation.set_issuer(&[p.issuer.trim_end_matches('/')]);
        validation.set_audience(&p.effective_audiences());
        let data = decode::<serde_json::Value>(token, key, &validation)
            .map_err(|e| OidcError::Invalid(format!("{e}")))?;
        let claims = data.claims;

        let sub = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .ok_or_else(|| OidcError::Invalid("no sub".into()))?
            .to_string();
        let groups = claims
            .get(&p.groups_claim)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|g| g.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let scopes = claims
            .get("scope")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        Ok(Identity {
            subject: format!("{}:{}", p.name, sub),
            provider: p.name.clone(),
            sub,
            email: claims
                .get("email")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            groups,
            scopes,
            exp: claims.get("exp").and_then(|v| v.as_i64()).unwrap_or(0),
        })
    }

    /// Try every configured provider (first that validates wins) — the
    /// gateway's introspection path when the caller doesn't name one.
    pub async fn validate_any(&self, token: &str) -> Result<Identity, OidcError> {
        let mut last = OidcError::Invalid("no providers configured".into());
        for p in &self.providers {
            match self.validate(&p.name, token).await {
                Ok(id) => return Ok(id),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    // -- device flow --------------------------------------------------------

    /// RFC 8628 §3.1–3.2: start a device authorization.
    pub async fn device_start(&self, name: &str) -> Result<DeviceStart, OidcError> {
        let p = self.provider(name)?.clone();
        let d = self.discovery(name).await?;
        let endpoint = d
            .device_authorization_endpoint
            .ok_or_else(|| OidcError::Device("provider offers no device flow".into()))?;
        let mut form = vec![
            ("client_id", p.client_id.clone()),
            ("scope", p.scopes.join(" ")),
        ];
        if let Some(secret) = &p.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        #[derive(Deserialize)]
        struct Resp {
            device_code: String,
            user_code: String,
            verification_uri: String,
            #[serde(default)]
            verification_uri_complete: Option<String>,
            expires_in: u64,
            #[serde(default = "default_interval")]
            interval: u64,
        }
        fn default_interval() -> u64 {
            5
        }
        let r: Resp = self
            .http
            .post(&endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| OidcError::Device(format!("{e}")))?
            .error_for_status()
            .map_err(|e| OidcError::Device(format!("{e}")))?
            .json()
            .await
            .map_err(|e| OidcError::Device(format!("parse: {e}")))?;
        Ok(DeviceStart {
            device_code: r.device_code,
            user_code: r.user_code,
            verification_uri: r.verification_uri_complete.unwrap_or(r.verification_uri),
            expires_in: r.expires_in,
            interval: r.interval,
        })
    }

    /// RFC 8628 §3.4–3.5: poll the token endpoint once.
    pub async fn device_poll(
        &self,
        name: &str,
        device_code: &str,
    ) -> Result<DeviceTokens, OidcError> {
        let p = self.provider(name)?.clone();
        let d = self.discovery(name).await?;
        let mut form = vec![
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            ),
            ("device_code", device_code.to_string()),
            ("client_id", p.client_id.clone()),
        ];
        if let Some(secret) = &p.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let resp = self
            .http
            .post(&d.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| OidcError::Device(format!("{e}")))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| OidcError::Device(format!("parse: {e}")))?;
        if status.is_success() {
            return Ok(DeviceTokens {
                access_token: body
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                id_token: body
                    .get("id_token")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                refresh_token: body
                    .get("refresh_token")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                expires_in: body.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(0),
            });
        }
        match body.get("error").and_then(|v| v.as_str()) {
            Some("authorization_pending") => Err(OidcError::AuthorizationPending),
            Some("slow_down") => Err(OidcError::SlowDown),
            Some(other) => Err(OidcError::Device(other.to_string())),
            None => Err(OidcError::Device(format!("HTTP {status}"))),
        }
    }
}

/// What `/v1/device/start` returns to the CLI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceStart {
    #[serde(skip)]
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// A completed device grant. The refresh token NEVER leaves this service —
/// it is sealed into custody; callers get the access/id tokens only.
#[derive(Debug, Clone)]
pub struct DeviceTokens {
    pub access_token: String,
    pub id_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}
