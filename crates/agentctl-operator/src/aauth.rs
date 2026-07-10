// SPDX-License-Identifier: BUSL-1.1
//! AAuth identity provisioning (RFC 0023) — the operator as house-provisioner.
//!
//! For an `Agent` that opts in via `spec.identity.aauth`, the operator:
//!  1. ensures a per-Agent **durable Ed25519 key** Secret (`<name>-aauth-key`,
//!     the base64url-unpadded 32-byte seed — exactly the key-file format the
//!     reference agent reads),
//!  2. pre-registers the key's RFC 7638 thumbprint at the Agent Provider over
//!     the admin API (**allowlist enrollment** — the one secret-free path: the
//!     "credential" is a public-key hash sent over the operator's
//!     authenticated admin channel; nothing secret ever travels to the pod
//!     beyond the agent's own identity key),
//!  3. renders `--aauth-provider` + `--aauth-key-file` (see
//!     [`crate::render::inject_aauth`]),
//!  4. learns the enrolled identity back into `status.identity.aauth` and
//!     revokes it on deletion.
//!
//! Experimental tier: everything here is inert unless `AGENTCTL_AAUTH_PROVIDER`
//! (or a per-Agent `spec.identity.aauth.provider`) is configured. The admin
//! token is operator-side only and is never rendered into any pod.

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, ObjectMeta, PostParams};
use kube::Client;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};
use tracing::{debug, info, warn};

/// Secret key (filename) the seed is stored under — the agent's
/// `--aauth-key-file` basename.
pub const KEY_FILENAME: &str = "agent.key";
/// Annotation carrying the key's RFC 7638 thumbprint, so reconcile/cleanup
/// never re-derive it from the seed.
pub const JKT_ANNOTATION: &str = "agentctl.dev/aauth-jkt";
/// Registration label sent to the provider (`{ns}/{name}`) — the reverse index
/// for identity learning and orphan GC.
pub fn registration_label(ns: &str, name: &str) -> String {
    format!("{ns}/{name}")
}
/// The per-Agent key Secret name.
pub fn key_secret_name(workload: &str) -> String {
    format!("{workload}-aauth-key")
}

/// Operator-scoped AAuth wiring, read once at startup.
///
/// `AGENTCTL_AAUTH_PROVIDER` — the default Agent Provider issuer URL (a
/// per-Agent `spec.identity.aauth.provider` overrides it).
/// `AGENTCTL_AAUTH_ADMIN_TOKEN_FILE` — path to the mounted admin bearer token
/// (re-read per call so Secret rotation applies without a restart).
#[derive(Clone, Default)]
pub struct AauthConfig {
    pub provider: Option<String>,
    pub admin_token_file: Option<String>,
    http: Option<Arc<reqwest::Client>>,
}

impl std::fmt::Debug for AauthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AauthConfig")
            .field("provider", &self.provider)
            .field("admin_token_file", &self.admin_token_file)
            .finish()
    }
}

impl AauthConfig {
    pub fn from_env() -> Self {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let provider = get("AGENTCTL_AAUTH_PROVIDER").map(|p| p.trim_end_matches('/').to_string());
        let admin_token_file = get("AGENTCTL_AAUTH_ADMIN_TOKEN_FILE");
        let http = if provider.is_some() || admin_token_file.is_some() {
            match build_http_client() {
                Ok(c) => Some(Arc::new(c)),
                Err(e) => {
                    warn!(error = %e, "aauth: admin HTTP client unavailable; provisioning disabled");
                    None
                }
            }
        } else {
            None
        };
        AauthConfig {
            provider,
            admin_token_file,
            http,
        }
    }

    /// Resolve the provider for one Agent: spec override wins, else the
    /// operator default. `None` ⇒ the opt-in is unconfigured (admission also
    /// denies this; the controller degrades to `Validated=False`).
    ///
    /// The result is **absolutized** (an in-cluster `.svc.cluster.local` host
    /// gets a trailing dot) so neither the operator's admin dials nor the
    /// rendered `--aauth-provider` can be captured by an ndots search-domain
    /// wildcard — the same defense the MCP/modelgateway URLs already carry.
    /// A public provider (not `.svc.cluster.local`) passes through untouched.
    pub fn resolve_provider(&self, spec: &agent_api::AauthIdentity) -> Option<String> {
        spec.provider
            .as_ref()
            .map(|p| p.trim_end_matches('/').to_string())
            .or_else(|| self.provider.clone())
            .map(|p| absolutize_provider(&p))
    }

    /// Whether the admin channel is usable (client built + token file named).
    pub fn admin_ready(&self) -> bool {
        self.http.is_some() && self.admin_token_file.is_some()
    }

    fn bearer(&self) -> Option<String> {
        let path = self.admin_token_file.as_ref()?;
        match std::fs::read_to_string(path) {
            Ok(t) => {
                let t = t.trim().to_string();
                (!t.is_empty()).then_some(t)
            }
            Err(e) => {
                warn!(path = %path, error = %e, "aauth: admin token unreadable");
                None
            }
        }
    }

    /// `POST {provider}/admin/allowed-keys` — pre-register the durable key's
    /// thumbprint so the agent's keyless self-enrollment is authorized.
    /// Idempotent-defensive: 2xx and 409 (already registered/enrolled) are OK.
    pub async fn register_allowed_key(
        &self,
        provider: &str,
        jkt: &str,
        label: &str,
    ) -> Result<(), String> {
        let (http, token) = self.channel()?;
        let url = format!("{provider}/admin/allowed-keys");
        let body = serde_json::json!({ "jkt": jkt, "label": label, "ttl": 86_400 });
        let resp = http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 409 {
            debug!(jkt, label, "aauth: allowed-key registered");
            Ok(())
        } else {
            Err(format!("POST {url}: HTTP {status}"))
        }
    }

    /// `GET {provider}/admin/agents` — find the enrollment whose registration
    /// label matches, returning `(local, created_at?, agent_id?)`.
    pub async fn find_enrollment(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<Option<Enrollment>, String> {
        let (http, token) = self.channel()?;
        let url = format!("{provider}/admin/agents");
        let resp = http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GET {url}: HTTP {status}"));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| format!("GET {url}: {e}"))?;
        // Accept both a bare array and an {"agents":[…]} envelope.
        let list = v
            .as_array()
            .cloned()
            .or_else(|| v.get("agents").and_then(|a| a.as_array()).cloned())
            .unwrap_or_default();
        for rec in &list {
            let rec_label = rec.get("label").and_then(|l| l.as_str()).unwrap_or("");
            if rec_label != label {
                continue;
            }
            if rec
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("active")
                == "revoked"
            {
                continue;
            }
            let Some(local) = rec.get("local").and_then(|l| l.as_str()) else {
                continue;
            };
            return Ok(Some(Enrollment {
                local: local.to_string(),
                agent: rec
                    .get("agent")
                    .and_then(|a| a.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| agent_id(local, provider)),
                created_at: rec
                    .get("created_at")
                    .and_then(|c| c.as_str())
                    .map(str::to_string),
            }));
        }
        Ok(None)
    }

    /// `POST {provider}/admin/agents/{local}/revoke` — the provider refuses
    /// further token issuance; live tokens age out within their TTL.
    pub async fn revoke(&self, provider: &str, local: &str) -> Result<(), String> {
        let (http, token) = self.channel()?;
        let url = format!("{provider}/admin/agents/{local}/revoke");
        let resp = http
            .post(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        let status = resp.status();
        // 404 = already gone: revocation is idempotent from our side.
        if status.is_success() || status.as_u16() == 404 {
            info!(local, "aauth: identity revoked");
            Ok(())
        } else {
            Err(format!("POST {url}: HTTP {status}"))
        }
    }

    /// `DELETE {provider}/admin/allowed-keys/{jkt}` — withdraw a pending
    /// (never-consumed) registration at cleanup. Best-effort hygiene.
    pub async fn withdraw_allowed_key(&self, provider: &str, jkt: &str) -> Result<(), String> {
        let (http, token) = self.channel()?;
        let url = format!("{provider}/admin/allowed-keys/{jkt}");
        let resp = http
            .delete(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("DELETE {url}: {e}"))?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            Ok(())
        } else {
            Err(format!("DELETE {url}: HTTP {status}"))
        }
    }

    fn channel(&self) -> Result<(&reqwest::Client, String), String> {
        let http = self
            .http
            .as_deref()
            .ok_or_else(|| "aauth admin HTTP client not built".to_string())?;
        let token = self
            .bearer()
            .ok_or_else(|| "aauth admin token missing/unreadable".to_string())?;
        Ok((http, token))
    }
}

/// An enrollment record learned from the provider's admin API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub local: String,
    /// Full identifier (`aauth:local@domain`), constructed from the provider
    /// host when the record does not carry it explicitly.
    pub agent: String,
    pub created_at: Option<String>,
}

/// reqwest over rustls with the EXPLICIT ring provider + webpki roots — never
/// the (absent) process default and never aws-lc-rs (no C toolchain), matching
/// the gateway's client pattern. Plain-http providers (in-cluster dev shape)
/// bypass TLS entirely.
fn build_http_client() -> Result<reqwest::Client, String> {
    let provider = rustls::crypto::ring::default_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("rustls protocol versions: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build reqwest client: {e}"))
}

/// Absolutize an in-cluster provider URL's host (`…svc.cluster.local` →
/// `…svc.cluster.local.`) so DNS resolution is absolute; external hosts pass
/// through. Delegates to the renderer's shared helper so the two never drift.
fn absolutize_provider(provider: &str) -> String {
    crate::render::absolutize_endpoint(provider)
}

/// Construct the agent identifier from a `local` + the provider issuer URL
/// (identity domain = the issuer host, port stripped — server identifiers are
/// host-only).
pub fn agent_id(local: &str, provider: &str) -> String {
    let host = provider
        .split("://")
        .nth(1)
        .unwrap_or(provider)
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    format!("aauth:{local}@{host}")
}

// ---------------------------------------------------------------------------
// Key material (pure; unit-tested against the RFC 8037 A.3 vector)
// ---------------------------------------------------------------------------

/// base64url without padding (RFC 4648 §5) — the encoding of the key file, the
/// JWK `x`, and the thumbprint.
pub fn b64url_nopad(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// Decode base64url (padding-tolerant). Used to re-derive the thumbprint from
/// an existing key Secret that lost its annotation.
pub fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut acc: u32 = 0;
    let mut bits = 0u8;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue,
            b'\n' | b'\r' | b' ' => continue,
            _ => return Err(format!("invalid base64url byte {c:#x}")),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// Generate a fresh 32-byte Ed25519 seed.
pub fn generate_seed() -> Result<[u8; 32], String> {
    let mut seed = [0u8; 32];
    SystemRandom::new()
        .fill(&mut seed)
        .map_err(|_| "system RNG unavailable".to_string())?;
    Ok(seed)
}

/// The public-key `x` (base64url) of a seed.
pub fn public_jwk_x(seed: &[u8; 32]) -> Result<String, String> {
    let pair = Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| "invalid Ed25519 seed".to_string())?;
    Ok(b64url_nopad(pair.public_key().as_ref()))
}

/// RFC 7638 JWK thumbprint of the seed's public key: SHA-256 over the
/// canonical `{"crv":"Ed25519","kty":"OKP","x":"…"}` (keys lexicographic, no
/// whitespace), base64url-unpadded.
pub fn jwk_thumbprint(seed: &[u8; 32]) -> Result<String, String> {
    let x = public_jwk_x(seed)?;
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#);
    let hash = digest::digest(&digest::SHA256, canonical.as_bytes());
    Ok(b64url_nopad(hash.as_ref()))
}

/// The per-Agent key Secret body (pure): the seed-file the agent's
/// `--aauth-key-file` reads, owner-ref'd so GC reclaims it with the Agent, the
/// thumbprint annotated for reconcile/cleanup.
pub fn key_secret_body(
    workload: &str,
    ns: &str,
    seed_b64: &str,
    jkt: &str,
    owner: &OwnerReference,
) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(key_secret_name(workload)),
            namespace: Some(ns.to_string()),
            annotations: Some(BTreeMap::from([(
                JKT_ANNOTATION.to_string(),
                jkt.to_string(),
            )])),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/managed-by".to_string(),
                "agentctl".to_string(),
            )])),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        type_: Some("Opaque".to_string()),
        string_data: Some(BTreeMap::from([(
            KEY_FILENAME.to_string(),
            seed_b64.to_string(),
        )])),
        ..Default::default()
    }
}

/// Ensure the per-Agent durable-key Secret exists and return its thumbprint.
/// Existing Secret ⇒ read (annotation first, seed re-derivation as fallback);
/// absent ⇒ generate + create (a 409 race falls back to the winner's copy).
pub async fn ensure_key_secret(
    client: &Client,
    ns: &str,
    workload: &str,
    owner: &OwnerReference,
) -> Result<String, String> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let name = key_secret_name(workload);
    if let Some(existing) = secrets
        .get_opt(&name)
        .await
        .map_err(|e| format!("get Secret {ns}/{name}: {e}"))?
    {
        return jkt_of_secret(&existing).ok_or_else(|| {
            format!("Secret {ns}/{name} exists but holds no readable {KEY_FILENAME} seed")
        });
    }
    let seed = generate_seed()?;
    let jkt = jwk_thumbprint(&seed)?;
    let body = key_secret_body(workload, ns, &b64url_nopad(&seed), &jkt, owner);
    match secrets.create(&PostParams::default(), &body).await {
        Ok(_) => {
            info!(secret = %name, ns, "aauth: durable key provisioned");
            Ok(jkt)
        }
        // Lost a create race: use the winner's key.
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            let existing = secrets
                .get(&name)
                .await
                .map_err(|e| format!("get Secret {ns}/{name} after 409: {e}"))?;
            jkt_of_secret(&existing)
                .ok_or_else(|| format!("Secret {ns}/{name} raced but holds no readable seed"))
        }
        Err(e) => Err(format!("create Secret {ns}/{name}: {e}")),
    }
}

/// The thumbprint of a workload's existing key Secret, if any — used at
/// cleanup to withdraw a never-consumed allowlist registration.
pub async fn pending_jkt(client: &Client, ns: &str, workload: &str) -> Option<String> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret = secrets.get_opt(&key_secret_name(workload)).await.ok()??;
    jkt_of_secret(&secret)
}

/// The thumbprint of an existing key Secret: the annotation when present,
/// else re-derived from the stored seed.
pub fn jkt_of_secret(secret: &Secret) -> Option<String> {
    if let Some(jkt) = secret
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(JKT_ANNOTATION))
    {
        return Some(jkt.clone());
    }
    let raw = secret.data.as_ref()?.get(KEY_FILENAME)?;
    let seed_b64 = std::str::from_utf8(&raw.0).ok()?;
    let bytes = b64url_decode(seed_b64.trim()).ok()?;
    let seed: [u8; 32] = bytes.try_into().ok()?;
    jwk_thumbprint(&seed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_matches_rfc4648_vectors_unpadded() {
        assert_eq!(b64url_nopad(b""), "");
        assert_eq!(b64url_nopad(b"f"), "Zg");
        assert_eq!(b64url_nopad(b"fo"), "Zm8");
        assert_eq!(b64url_nopad(b"foo"), "Zm9v");
        assert_eq!(b64url_nopad(b"foob"), "Zm9vYg");
        assert_eq!(b64url_nopad(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url_nopad(b"foobar"), "Zm9vYmFy");
        // decode round-trips (incl. url-safe chars)
        assert_eq!(b64url_decode("Zm9vYmFy").unwrap(), b"foobar");
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(b64url_decode(&b64url_nopad(&all)).unwrap(), all);
    }

    #[test]
    fn thumbprint_matches_the_rfc8037_a3_vector() {
        // RFC 8037 A.1/A.3: d (seed), its public x, and the JWK thumbprint.
        let seed: [u8; 32] = b64url_decode("nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A")
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            public_jwk_x(&seed).unwrap(),
            "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
        );
        assert_eq!(
            jwk_thumbprint(&seed).unwrap(),
            "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k"
        );
    }

    #[test]
    fn key_secret_body_carries_seed_annotation_and_owner() {
        let owner = OwnerReference {
            api_version: "agentctl.dev/v1alpha1".into(),
            kind: "Agent".into(),
            name: "support".into(),
            uid: "uid-1".into(),
            controller: Some(true),
            ..Default::default()
        };
        let s = key_secret_body("support", "team-a", "c2VlZA", "JKT", &owner);
        assert_eq!(s.metadata.name.as_deref(), Some("support-aauth-key"));
        assert_eq!(
            s.string_data.as_ref().unwrap().get(KEY_FILENAME).unwrap(),
            "c2VlZA"
        );
        assert_eq!(
            s.metadata.annotations.as_ref().unwrap()[JKT_ANNOTATION],
            "JKT"
        );
        assert_eq!(
            s.metadata.owner_references.as_ref().unwrap()[0].name,
            "support"
        );
        // Fresh material derives a valid thumbprint end to end.
        let seed = generate_seed().unwrap();
        assert_eq!(jwk_thumbprint(&seed).unwrap().len(), 43); // 32 bytes → 43 b64url chars
    }

    #[test]
    fn agent_id_builds_from_provider_host() {
        assert_eq!(
            agent_id("k7q3p9n2", "https://ap.example.com"),
            "aauth:k7q3p9n2@ap.example.com"
        );
        // port + path + trailing-dot FQDN are stripped; host lowercased
        assert_eq!(
            agent_id(
                "x",
                "http://APD.agentctl-system.svc.cluster.local.:8420/base"
            ),
            "aauth:x@apd.agentctl-system.svc.cluster.local"
        );
    }

    #[test]
    fn resolve_provider_prefers_the_spec_override() {
        let cfg = AauthConfig {
            provider: Some("https://default.ap".into()),
            ..Default::default()
        };
        let none = agent_api::AauthIdentity::default();
        let over = agent_api::AauthIdentity {
            provider: Some("https://tenant.ap/".into()),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_provider(&none).as_deref(),
            Some("https://default.ap")
        );
        // trailing slash normalized off; a public host passes through absolutize
        assert_eq!(
            cfg.resolve_provider(&over).as_deref(),
            Some("https://tenant.ap")
        );
        let empty = AauthConfig::default();
        assert_eq!(empty.resolve_provider(&none), None);

        // An in-cluster provider is absolutized (trailing dot) so DNS resolution
        // cannot be captured by an ndots search-domain wildcard.
        let incluster = AauthConfig {
            provider: Some("http://apd.default.svc.cluster.local".into()),
            ..Default::default()
        };
        assert_eq!(
            incluster.resolve_provider(&none).as_deref(),
            Some("http://apd.default.svc.cluster.local.")
        );
    }
}
