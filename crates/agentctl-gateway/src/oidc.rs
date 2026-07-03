// SPDX-License-Identifier: BUSL-1.1
//! Native per-agent OIDC/JWT enforcement for the A2A gateway.
//!
//! When an `Agent` (or `AgentFleet`) sets `spec.access.oidc`
//! ([`agent_api::OidcAccess`]), inbound A2A JSON-RPC calls to that agent must
//! carry a `Authorization: Bearer <JWT>` that this module validates **for that
//! specific agent**:
//!
//!   * **JWKS** is discovered from `oidc.jwks_uri`, else from the issuer's
//!     `…/.well-known/openid-configuration` → `jwks_uri` → the JWKS document. The
//!     key set is cached per issuer with a TTL ([`JWKS_TTL`]); a `kid` miss inside
//!     a fresh cache forces one refresh (handles key rotation).
//!   * **Verification** ([`jsonwebtoken`], ring-backed — no openssl/aws-lc) checks
//!     the signature against the matching JWK (RSA or EC/ES256), `iss ==
//!     oidc.issuer`, `aud` intersecting `oidc.audiences`, and `exp`/`nbf` with a
//!     60s leeway.
//!   * **Authorization** then enforces every `required_claims` entry: the caller's
//!     claim (array ⇒ contains; scalar ⇒ equals) must match one of `any_of`.
//!
//! AuthN failures map to **401**, authZ failures to **403**; the response body
//! never leaks token detail (the reason is logged server-side only). On success
//! the verified [`Identity`] (`sub`/`email`/`groups`) is returned so the caller
//! can optionally forward it to the agent (`forward_identity`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_api::{ClaimRequirement, OidcAccess};
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde_json::Value;
use tokio::sync::Mutex;

/// How long a fetched JWKS stays cached per issuer before a refresh.
const JWKS_TTL: Duration = Duration::from_secs(300);

/// Clock-skew tolerance (seconds) applied to `exp`/`nbf` during verification.
const LEEWAY_SECS: u64 = 60;

/// The outcome of a failed authn/authz decision, carrying the HTTP status intent.
/// The contained reason is for server-side logs only — never the response body.
#[derive(Debug)]
pub enum AuthError {
    /// Authentication failed (missing/invalid/expired token, bad signature,
    /// wrong issuer/audience, unknown key) → **401**.
    Unauthorized(String),
    /// Authentication succeeded but a `required_claims` rule was not satisfied →
    /// **403**.
    Forbidden(String),
}

/// A verified caller identity, projected from the validated JWT claims. Forwarded
/// to the agent as `X-Auth-*` headers when `forward_identity` is enabled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// The `sub` claim (subject) — always present (empty string if absent).
    pub sub: String,
    /// The `email` claim, when present.
    pub email: Option<String>,
    /// The `groups` claim (array of strings, or a single string), when present.
    pub groups: Vec<String>,
}

impl Identity {
    /// Inject this verified identity as `X-Auth-*` headers onto the forwarded
    /// request to the agent. Only the `email`/`groups` headers that
    /// have values are added; `X-Auth-Subject` is always set. Client-supplied
    /// `X-Auth-*` headers are NOT propagated (we build a fresh request), so the
    /// agent can trust these to be gateway-verified.
    pub fn inject(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb.header("X-Auth-Subject", self.sub.as_str());
        if let Some(email) = &self.email {
            rb = rb.header("X-Auth-Email", email.as_str());
        }
        if !self.groups.is_empty() {
            rb = rb.header("X-Auth-Groups", self.groups.join(","));
        }
        rb
    }
}

/// One cached JWKS for an issuer, with the time it was fetched (for TTL).
struct Cached {
    keys: JwkSet,
    fetched: Instant,
}

/// Per-agent OIDC verifier with a per-issuer JWKS cache. Cheap to clone-share via
/// an [`Arc`]; the cache + HTTP client are shared.
pub struct Verifier {
    /// HTTP client for JWKS discovery/fetch (public-CA roots, rustls/ring).
    http: reqwest::Client,
    /// JWKS cache keyed by issuer URL.
    cache: Mutex<HashMap<String, Cached>>,
    /// Cache TTL (overridable in tests).
    ttl: Duration,
}

impl Verifier {
    /// Build a verifier with the production JWKS HTTP client (public CA roots).
    pub fn new() -> Self {
        Self::with_client(jwks_http_client())
    }

    /// Build a verifier with a caller-supplied HTTP client (used by tests to point
    /// at a local JWKS server over plain HTTP).
    pub fn with_client(http: reqwest::Client) -> Self {
        Self {
            http,
            cache: Mutex::new(HashMap::new()),
            ttl: JWKS_TTL,
        }
    }

    /// Verify `token` for the agent's `oidc` policy, returning the caller identity
    /// on success. AuthN problems → [`AuthError::Unauthorized`]; an unsatisfied
    /// `required_claims` rule → [`AuthError::Forbidden`].
    pub async fn verify(&self, oidc: &OidcAccess, token: &str) -> Result<Identity, AuthError> {
        let header = decode_header(token)
            .map_err(|e| AuthError::Unauthorized(format!("decode token header: {e}")))?;
        let jwk = self.jwk_for_kid(oidc, header.kid.as_deref()).await?;
        let alg = jwk_algorithm(&jwk)
            .ok_or_else(|| AuthError::Unauthorized("unsupported JWK key type".into()))?;
        let key = DecodingKey::from_jwk(&jwk)
            .map_err(|e| AuthError::Unauthorized(format!("build decoding key: {e}")))?;

        let validation = build_validation(oidc, alg);
        let data = decode::<Value>(token, &key, &validation)
            .map_err(|e| AuthError::Unauthorized(format!("jwt verify: {e}")))?;

        enforce_claims(&data.claims, oidc.required_claims.as_deref())?;
        Ok(extract_identity(&data.claims))
    }

    /// Resolve the JWK matching `kid` for the issuer, consulting the cache first
    /// and force-refreshing once on a miss (key rotation) or TTL expiry.
    async fn jwk_for_kid(&self, oidc: &OidcAccess, kid: Option<&str>) -> Result<Jwk, AuthError> {
        if let Some(jwk) = self.cached_lookup(&oidc.issuer, kid).await {
            return Ok(jwk);
        }
        let set = self
            .fetch_jwks(oidc)
            .await
            .map_err(AuthError::Unauthorized)?;
        let found = find_key(&set, kid).cloned();
        self.store(&oidc.issuer, set).await;
        found.ok_or_else(|| AuthError::Unauthorized("no JWK matches the token kid".into()))
    }

    /// Look up a fresh cached key for `(issuer, kid)`. Returns `None` if the issuer
    /// isn't cached, the entry is past its TTL, or no key matches `kid` (→ caller
    /// refreshes).
    async fn cached_lookup(&self, issuer: &str, kid: Option<&str>) -> Option<Jwk> {
        let cache = self.cache.lock().await;
        let entry = cache.get(issuer)?;
        if entry.fetched.elapsed() > self.ttl {
            return None;
        }
        find_key(&entry.keys, kid).cloned()
    }

    /// Store/replace the cached JWKS for an issuer, stamping it now.
    async fn store(&self, issuer: &str, keys: JwkSet) {
        self.cache.lock().await.insert(
            issuer.to_string(),
            Cached {
                keys,
                fetched: Instant::now(),
            },
        );
    }

    /// Fetch the issuer's JWKS, discovering the `jwks_uri` when not pinned.
    async fn fetch_jwks(&self, oidc: &OidcAccess) -> Result<JwkSet, String> {
        let uri = self.resolve_jwks_uri(oidc).await?;
        let set = self
            .http
            .get(&uri)
            .send()
            .await
            .map_err(|e| format!("GET JWKS {uri}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("JWKS status {uri}: {e}"))?
            .json::<JwkSet>()
            .await
            .map_err(|e| format!("decode JWKS {uri}: {e}"))?;
        Ok(set)
    }

    /// The JWKS URI: `oidc.jwks_uri` when set, else the issuer's OIDC discovery
    /// document's `jwks_uri`.
    async fn resolve_jwks_uri(&self, oidc: &OidcAccess) -> Result<String, String> {
        if let Some(uri) = &oidc.jwks_uri {
            return Ok(uri.clone());
        }
        let disc = format!(
            "{}/.well-known/openid-configuration",
            oidc.issuer.trim_end_matches('/')
        );
        let doc = self
            .http
            .get(&disc)
            .send()
            .await
            .map_err(|e| format!("GET discovery {disc}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("discovery status {disc}: {e}"))?
            .json::<Value>()
            .await
            .map_err(|e| format!("decode discovery {disc}: {e}"))?;
        doc.get("jwks_uri")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("discovery doc {disc} missing jwks_uri"))
    }
}

impl Default for Verifier {
    fn default() -> Self {
        Self::new()
    }
}

// --- pure helpers (unit-tested) --------------------------------------------

/// Find the JWK matching `kid`. With no `kid` in the token header, a single-key
/// set is unambiguous and selected; otherwise the lookup fails (ambiguous).
fn find_key<'a>(set: &'a JwkSet, kid: Option<&str>) -> Option<&'a Jwk> {
    match kid {
        Some(k) => set
            .keys
            .iter()
            .find(|j| j.common.key_id.as_deref() == Some(k)),
        None if set.keys.len() == 1 => set.keys.first(),
        None => None,
    }
}

/// The verification [`Algorithm`] implied by a JWK: its declared `alg` (honored
/// for RSA variants), else the family default (RSA→RS256, P-256→ES256,
/// P-384→ES384). Pinning the algorithm to the key — not the attacker-controlled
/// token header — blocks `alg`-confusion (e.g. an `HS256` forgery against an RSA
/// public key).
fn jwk_algorithm(jwk: &Jwk) -> Option<Algorithm> {
    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Some(match jwk.common.key_algorithm {
            Some(KeyAlgorithm::RS384) => Algorithm::RS384,
            Some(KeyAlgorithm::RS512) => Algorithm::RS512,
            Some(KeyAlgorithm::PS256) => Algorithm::PS256,
            Some(KeyAlgorithm::PS384) => Algorithm::PS384,
            Some(KeyAlgorithm::PS512) => Algorithm::PS512,
            _ => Algorithm::RS256,
        }),
        AlgorithmParameters::EllipticCurve(ec) => match ec.curve {
            EllipticCurve::P256 => Some(Algorithm::ES256),
            EllipticCurve::P384 => Some(Algorithm::ES384),
            // P-521 is unsupported by ring; Ed25519 arrives as OctetKeyPair.
            _ => None,
        },
        AlgorithmParameters::OctetKeyPair(okp) => match okp.curve {
            EllipticCurve::Ed25519 => Some(Algorithm::EdDSA),
            _ => None,
        },
        // Symmetric (HMAC) keys are never valid for an OIDC issuer's public JWKS.
        AlgorithmParameters::OctetKey(_) => None,
    }
}

/// Build the [`Validation`] from the agent's `oidc` policy: pin the algorithm,
/// require + match `iss`, require + match `aud` only when audiences are
/// configured, validate `exp`/`nbf` with the standard leeway.
fn build_validation(oidc: &OidcAccess, alg: Algorithm) -> Validation {
    let mut v = Validation::new(alg);
    v.leeway = LEEWAY_SECS;
    v.validate_exp = true;
    v.validate_nbf = true;
    v.set_issuer(&[oidc.issuer.as_str()]);

    let mut required: std::collections::HashSet<String> =
        ["exp", "iss"].iter().map(|s| s.to_string()).collect();
    if oidc.audiences.is_empty() {
        // No audience policy ⇒ don't gate on `aud`.
        v.validate_aud = false;
    } else {
        v.validate_aud = true;
        v.set_audience(&oidc.audiences);
        // Require the claim so an `aud`-less token can't slip past the policy.
        required.insert("aud".to_string());
    }
    v.required_spec_claims = required;
    v
}

/// Enforce every `required_claims` rule (logical AND). Each rule passes when the
/// caller's claim satisfies it ([`claim_satisfied`]); the first miss → 403.
///
/// `pub(crate)` so the trusted-proxy path can reuse the SAME authZ logic against a
/// caller identity asserted by the front proxy (no JWT) — see
/// [`crate::trusted_proxy::identity_claims`].
pub(crate) fn enforce_claims(
    claims: &Value,
    required: Option<&[ClaimRequirement]>,
) -> Result<(), AuthError> {
    let Some(rules) = required else {
        return Ok(());
    };
    for rule in rules {
        if !claim_satisfied(claims, rule) {
            return Err(AuthError::Forbidden(format!(
                "required claim `{}` not satisfied",
                rule.claim
            )));
        }
    }
    Ok(())
}

/// Whether the caller's claim satisfies one rule: an array claim must *contain*
/// one of `any_of`; a scalar (string/number/bool) claim must *equal* one of
/// `any_of` (compared by string form). An empty `any_of` matches nothing
/// (fail-closed: a misconfigured rule denies rather than waves traffic through).
fn claim_satisfied(claims: &Value, rule: &ClaimRequirement) -> bool {
    if rule.any_of.is_empty() {
        return false;
    }
    let Some(value) = claims.get(&rule.claim) else {
        return false;
    };
    match value {
        Value::Array(items) => items.iter().any(|item| scalar_in(item, &rule.any_of)),
        other => scalar_in(other, &rule.any_of),
    }
}

/// Whether a scalar JSON value equals one of the allowed strings (numbers/bools
/// are compared by their string form; arrays/objects/null never match).
fn scalar_in(value: &Value, any_of: &[String]) -> bool {
    let candidate = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return false,
    };
    any_of.iter().any(|a| a == &candidate)
}

/// Project the caller [`Identity`] from validated claims: `sub` (empty if
/// absent), `email` (when a string), `groups` (a string array, or a single
/// string lifted into a one-element list).
fn extract_identity(claims: &Value) -> Identity {
    let sub = claims
        .get("sub")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let groups = match claims.get("groups") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    };
    Identity { sub, email, groups }
}

/// Build the HTTPS client used for JWKS discovery/fetch: rustls with the **ring**
/// provider and the bundled Mozilla (`webpki-roots`) trust anchors — public OIDC
/// issuers are reached over the open internet, so we trust public CAs (NOT the
/// internal control-plane CA used by the mTLS hop that dials agents directly).
/// Pure Rust, no C toolchain.
pub fn jwks_http_client() -> reqwest::Client {
    try_build_client().expect("build JWKS https client")
}

fn try_build_client() -> Result<reqwest::Client, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = rustls::crypto::ring::default_provider();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls protocol versions: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .build()
        .map_err(|e| format!("build JWKS reqwest client: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    // ===== generated test key material (RSA-A, RSA-B for forgery, EC P-256) =====
    const RSA_A_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCEjqnt+5IrThK5\n1N/CYXTseQxlHFGCfIMU60geD4TLKJ1EZC+Fzit3I1WB3LhCKBh4vAKrYuZd2adA\n8VMA0RB0PvTQ6RqYCwn5prGQRpmcHv1Hm7AZm3S2pNLqNaSWzuL78xlIQiXKxlk2\nqXHjnbDr+3He/6RbS43tzhi0W1r7cmR5NjQm0D7cBg9jgY6Ms0jxsFr4CL3OukJZ\nkLdbavVFPx9Iz7epiTGXRzoyjPy614nJUrB3OntHpxc2rn/r4r8/zcZholZoRD2f\njv3v/ivGxKf5A0a0St9g+Xt9eN+uacPN1dpdSrGsDQwbuAl+zpo2YJBu7C2j3V57\n8y51Q0yHAgMBAAECggEAAYkXiIAJXZgOGSSmoj1NGe99sxl6C5K+qqTean66hpHw\ntG9GqlGiPkM6BS2V3X+nvOpMoPNIgN4kalkBTE6frAEOBpzUp93k3oVNpEK1Gn0K\nE7ocIZ0j485n+mTnBFo0Kz/8fdJKVsgnwL2Dag6/ExS6vnRj+9drDE3+tO7OpdVV\nS7bQPNqafuztofcFIFWWMY92JewFkykvUkeP2Huaf4bZ4DInVg56x6WMGK/b/5LZ\nNgOLiHjr0zS1STuu6IqKwDcLkYDNr6rdoqzXvzfPe6u9QUtWdizpZusESfvCgzE/\nHJGtmUebrwh3AIo+vb2WKxEABfd00uHAAk0bG8wfjQKBgQC4szblEu797uPWbY90\nqvpgLSCSaC41y0Ox0jqfR9OGkTbfzw7O8CC0PVABQ+saBavtu2hkZeAqPRcU0gOh\nZMXiGrDqLDM+os8OSzGM0WDVx9kWgOVDC5ZJ5+h6aaaCz4uwt4neyHLYBLI48I6I\nyi3GEiCO/D0Zm662IW/0zyM6wwKBgQC3un7KjZxYLyPx9Rt9vtyIKckKBTPH90rK\nCQibAxXWS7dLNLpENNVodTnLw+CoKvcTwTnAOz0aieev6JV2+/kwhKsuMOzAuWm6\n3YFTMxvkIwoOAGAwrB7HjNIZwAIUYtrTIp6nzGnRrn+gFFEkigdtk93YxNupMLw2\nBX8Yv0gi7QKBgDrflU3rfRagQSumfKW5ollpyQoh/yjSg994nYsMABbSzuUEQToh\nPKt3J7tfhN8kk6sRo7Ls7klIc8UFNHcLgjASRfY+5I7AorNxsHesfetm6oHL0EhQ\ntzUToPz0FEl6EpLfziifSEwnIxAXTbe4imKqgIpTSL6S61vOyLsGE7q1AoGATXKW\nA/hR0XJ9qn7yCb2s5NEIZ+rtevupUSUhtYZFbEIaj984LYw/8XqI1HZLe1gxMuie\n2YOfLFK5kZNvfeqVjng+WIhTJKKECTtaSqIevbpvgJtz8NB9YQzhe+1Ocx2AtMPB\nMWafrL3sGqS117s/ildsivXgyp86l2MVwm7Pj7kCgYBJyXt6AoQ+UsoAzWQbZOsP\n84sAYN8RkY4k0Iir4iVPxcD/cZlQjB9t88lehG5P2yJZKYehLGuPwgib2HhvWOHJ\njycUoHRXvBi2lHZhqz8FEG+FnYFTnyXt9WcKvXSAzK2IYviK9S7TbRESYAZM9ok1\nREbV2nCgDfq8ly/t3mNKaw==\n-----END PRIVATE KEY-----\n";
    const RSA_A_N: &str = "hI6p7fuSK04SudTfwmF07HkMZRxRgnyDFOtIHg-EyyidRGQvhc4rdyNVgdy4QigYeLwCq2LmXdmnQPFTANEQdD700OkamAsJ-aaxkEaZnB79R5uwGZt0tqTS6jWkls7i-_MZSEIlysZZNqlx452w6_tx3v-kW0uN7c4YtFta-3JkeTY0JtA-3AYPY4GOjLNI8bBa-Ai9zrpCWZC3W2r1RT8fSM-3qYkxl0c6Moz8uteJyVKwdzp7R6cXNq5_6-K_P83GYaJWaEQ9n4797_4rxsSn-QNGtErfYPl7fXjfrmnDzdXaXUqxrA0MG7gJfs6aNmCQbuwto91ee_MudUNMhw";
    const RSA_A_E: &str = "AQAB";
    const RSA_B_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDAiMcsU2tJeHcw\nyRcBZ5qJEDYAmHO2eiBNO8w319cHswnaADnlxgkHOi3AjTRjgBsiKR/l9XtD/apn\n0oR9dVChAi/HDpNrpWgmlwcyguGm4fX/x5poDTRBST14KsOY6x+5/rzbnenG/sNc\nxNSM4S4fTSbt3SR43JjKBQ9KqxDFnmFIPf3Qk6pUc9zaeBVvTeDLBlq2Ff/JzXbM\ngQfcnVgx+VUnHF2yn9GHyhp3lGbyaGlfvaZynK6eLR2cC/sjBmkA6hXfi8cuzYsX\nofZ0NzJapJvUpW6X+yqDHRt3yrQgFoNghpSQnPFNbUNTdQokn6txhInLukflNUbJ\n1nzNEUhPAgMBAAECggEADnH0p5G2qfN81c8wh61zPbdWpeLKQ7WT+Nd0sffirTQ0\nmAOOVHvwL3eg+SJe/NwerQhy2Tj6v5Ynk9SKljMYEoxscz3Xt6rYTpTkOFjzfybS\n4xbhsc7TzdYl438p3648WiMPnlaRtJlmpO4rmEpIwJZ0RkJiOyMp33ZTuGFvR7RB\n/MdeCh5bC17a6HumaGI44v++TmTKJsB7VKbS4asVXWg02pd7PsbCjr9kG3rwwTXX\nX0B4xmJ7vgzdf2W6MoW4gUxeNSjsN36RF8Xb91O6ViBc0TMjsUqQVUo3l7h2cZRi\nIANf90uCjciL+kvrTMbPN8POPUbQbt+Q9HE2wMyCIQKBgQDhp61O6mFC6Qn5zbqJ\nHyo1135ERCNFVNJVuxmT7pEyKjyJKUgXNR/47L9VaFxaFHiakVC7/5yzOLNc5ixJ\nNd50/ayK/NF/9igZf6gzad0o6WFknbxljfKFRvi69tuA5U1Qp87hWf3bbYCJJP8v\nrRb0DBMonJv9ULQnorgR4J11MQKBgQDabOYhT6iU1XlIjrdBBr97Z7TLhHphI3ep\noCgCxMMnGRjii70q1KebF1/MWur7oDWl07ErsXaukuVkpauMOAdcCTwBneRTl0Ig\naGQx4CS9l6ZMAsvVNnu6Hs0flS87CX11wnrJM6EFCuywCYAG5MJaETOhlVFUVRcT\nYsaTOIE1fwKBgQCy13bm1aGSKyop3qBZbubAV3MOXcZqe4hcQ/ZIpUpULN9fgeVN\n51/YpKIb6aNQDWtsbYFEDpk9/dFB7nbo6xXNOQPX//l2ZjxvwRoo7V1HwHfdC5q2\nDiNI9+/IFj/vz0xQgT7Yob8teoLlrvnE6nUHpM5GYKDMynqN80vZd2Cz8QKBgQCT\n6l0pv8UdDTd14FfPLF+tlTxE+jDZ6WfWsgOGZHL33jIQ8Kqo/5uFFp4kSImK3yKV\narc3LJV/gTDhKKP0b9jkBcjiG2eNCAia47a+Y9jdn33ZSad5eszs7IDiW2fBphqV\nDZ+S82iefphsWfKeOHo4/h8l1HVgE8NtuF1bQ0+UxwKBgGowCFolJCWlaYnOXPd/\nhw4+1OJqzUtZFeXt8vRIGBRo1RgrQoEk+3g+2NsqHqorDtOLqDQWeP0fsxiPuDym\nvzr3val8gxPUPWgF02PFULZOghRG3syFwqt0BIiZvnV9OFTYp/ZvKucOUQa9Pn+/\nh2T1r5bndup9ZZVTSRanW1bC\n-----END PRIVATE KEY-----\n";
    const EC_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgLWafjHTaNr0o9J5i\nWMJlfhqd4hRduWKk4xtv4roOPtmhRANCAAR4zlanQqatq3+2F7aSbra8wdVAG2/1\nb6ybyLA2K1XB8yQNe3320Q+XZTaEGbf4xzICrA6hxLtM9O0z9VQbbgg+\n-----END PRIVATE KEY-----\n";
    const EC_X: &str = "eM5Wp0Kmrat_the2km62vMHVQBtv9W-sm8iwNitVwfM";
    const EC_Y: &str = "JA17ffbRD5dlNoQZt_jHMgKsDqHEu0z07TP1VBtuCD4";

    const ISSUER: &str = "https://issuer.test";
    const AUDIENCE: &str = "a2a-gateway";
    const KID_RSA: &str = "rsa-a";
    const KID_EC: &str = "ec-1";

    /// An RSA JWKS (kid=rsa-a) built from the RSA-A public components.
    fn rsa_jwks() -> JwkSet {
        serde_json::from_value(json!({
            "keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256",
                "kid": KID_RSA, "n": RSA_A_N, "e": RSA_A_E,
            }]
        }))
        .unwrap()
    }

    /// An EC P-256 JWKS (kid=ec-1) built from the EC public components.
    fn ec_jwks() -> JwkSet {
        serde_json::from_value(json!({
            "keys": [{
                "kty": "EC", "use": "sig", "crv": "P-256",
                "kid": KID_EC, "x": EC_X, "y": EC_Y,
            }]
        }))
        .unwrap()
    }

    fn now() -> u64 {
        jsonwebtoken::get_current_timestamp()
    }

    /// A reqwest client for tests. We reuse the production [`jwks_http_client`]
    /// builder, which hands reqwest a fully-built ring `ClientConfig`
    /// (`use_preconfigured_tls`) and so needs NO process-default crypto provider
    /// (unlike a bare `reqwest::Client::new()`, which panics with "No provider set"
    /// because the gateway's reqwest pulls no default provider). Plain-HTTP
    /// requests to the local test server work unchanged.
    fn test_client() -> reqwest::Client {
        jwks_http_client()
    }

    /// Sign `claims` with `pem`/`alg`, stamping the given `kid` in the header.
    fn sign(pem: &str, kid: &str, alg: Algorithm, claims: &Value) -> String {
        let mut header = Header::new(alg);
        header.kid = Some(kid.to_string());
        let key = match alg {
            Algorithm::ES256 => EncodingKey::from_ec_pem(pem.as_bytes()).unwrap(),
            _ => EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        };
        encode(&header, claims, &key).unwrap()
    }

    /// A baseline OIDC policy (issuer + audience, no claim rules).
    fn policy() -> OidcAccess {
        OidcAccess {
            issuer: ISSUER.to_string(),
            audiences: vec![AUDIENCE.to_string()],
            jwks_uri: Some(format!("{ISSUER}/jwks.json")),
            required_claims: None,
            forward_identity: None,
        }
    }

    /// A verifier with the RSA JWKS preloaded for [`ISSUER`] (no network).
    async fn rsa_verifier() -> Verifier {
        let v = Verifier::with_client(test_client());
        v.store(ISSUER, rsa_jwks()).await;
        v
    }

    fn standard_claims() -> Value {
        json!({
            "sub": "user-123",
            "iss": ISSUER,
            "aud": AUDIENCE,
            "exp": now() + 3600,
            "nbf": now() - 5,
            "email": "user@corp.example",
            "groups": ["eng", "oncall"],
        })
    }

    #[tokio::test]
    async fn valid_token_with_aud_and_claims_is_allowed() {
        let v = rsa_verifier().await;
        let token = sign(
            RSA_A_PRIV_PEM,
            KID_RSA,
            Algorithm::RS256,
            &standard_claims(),
        );
        let id = v.verify(&policy(), &token).await.expect("allow");
        assert_eq!(id.sub, "user-123");
        assert_eq!(id.email.as_deref(), Some("user@corp.example"));
        assert_eq!(id.groups, vec!["eng".to_string(), "oncall".to_string()]);
    }

    #[tokio::test]
    async fn valid_es256_token_is_allowed() {
        let v = Verifier::with_client(test_client());
        v.store(ISSUER, ec_jwks()).await;
        let token = sign(EC_PRIV_PEM, KID_EC, Algorithm::ES256, &standard_claims());
        let id = v.verify(&policy(), &token).await.expect("allow ES256");
        assert_eq!(id.sub, "user-123");
    }

    #[tokio::test]
    async fn required_claims_satisfied_is_allowed() {
        let v = rsa_verifier().await;
        let mut oidc = policy();
        oidc.required_claims = Some(vec![ClaimRequirement {
            claim: "groups".to_string(),
            any_of: vec!["oncall".to_string(), "admins".to_string()],
        }]);
        let token = sign(
            RSA_A_PRIV_PEM,
            KID_RSA,
            Algorithm::RS256,
            &standard_claims(),
        );
        assert!(v.verify(&oidc, &token).await.is_ok());
    }

    #[tokio::test]
    async fn bad_signature_is_unauthorized() {
        // JWKS advertises RSA-A's key, but the token is signed by RSA-B → 401.
        let v = rsa_verifier().await;
        let token = sign(
            RSA_B_PRIV_PEM,
            KID_RSA,
            Algorithm::RS256,
            &standard_claims(),
        );
        assert!(matches!(
            v.verify(&policy(), &token).await,
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn wrong_issuer_is_unauthorized() {
        let v = rsa_verifier().await;
        let mut claims = standard_claims();
        claims["iss"] = json!("https://evil.test");
        let token = sign(RSA_A_PRIV_PEM, KID_RSA, Algorithm::RS256, &claims);
        assert!(matches!(
            v.verify(&policy(), &token).await,
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn wrong_audience_is_unauthorized() {
        let v = rsa_verifier().await;
        let mut claims = standard_claims();
        claims["aud"] = json!("some-other-service");
        let token = sign(RSA_A_PRIV_PEM, KID_RSA, Algorithm::RS256, &claims);
        assert!(matches!(
            v.verify(&policy(), &token).await,
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn expired_token_is_unauthorized() {
        let v = rsa_verifier().await;
        let mut claims = standard_claims();
        // Beyond the 60s leeway.
        claims["exp"] = json!(now() - 120);
        let token = sign(RSA_A_PRIV_PEM, KID_RSA, Algorithm::RS256, &claims);
        assert!(matches!(
            v.verify(&policy(), &token).await,
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn missing_required_group_is_forbidden() {
        let v = rsa_verifier().await;
        let mut oidc = policy();
        oidc.required_claims = Some(vec![ClaimRequirement {
            claim: "groups".to_string(),
            any_of: vec!["admins".to_string()],
        }]);
        // Caller is in eng/oncall, NOT admins → authN ok, authZ denied → 403.
        let token = sign(
            RSA_A_PRIV_PEM,
            KID_RSA,
            Algorithm::RS256,
            &standard_claims(),
        );
        assert!(matches!(
            v.verify(&oidc, &token).await,
            Err(AuthError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn unknown_kid_is_unauthorized_after_refresh_attempt() {
        let v = rsa_verifier().await;
        // Token kid not in the (preloaded) set; the refresh hits the bogus jwks_uri
        // and fails → 401 (never a panic / fall-through).
        let token = sign(
            RSA_A_PRIV_PEM,
            "rotated-away",
            Algorithm::RS256,
            &standard_claims(),
        );
        assert!(matches!(
            v.verify(&policy(), &token).await,
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[test]
    fn forward_identity_injects_x_auth_headers() {
        let id = Identity {
            sub: "user-123".to_string(),
            email: Some("user@corp.example".to_string()),
            groups: vec!["eng".to_string(), "oncall".to_string()],
        };
        let req = id
            .inject(test_client().post("http://node-agent.local/v1"))
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(h.get("X-Auth-Subject").unwrap(), "user-123");
        assert_eq!(h.get("X-Auth-Email").unwrap(), "user@corp.example");
        assert_eq!(h.get("X-Auth-Groups").unwrap(), "eng,oncall");
    }

    #[test]
    fn inject_omits_absent_email_and_groups() {
        let id = Identity {
            sub: "svc-1".to_string(),
            email: None,
            groups: vec![],
        };
        let req = id
            .inject(test_client().post("http://node-agent.local/v1"))
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(h.get("X-Auth-Subject").unwrap(), "svc-1");
        assert!(h.get("X-Auth-Email").is_none());
        assert!(h.get("X-Auth-Groups").is_none());
    }

    #[test]
    fn claim_satisfied_array_contains_and_scalar_equals() {
        let claims = json!({
            "groups": ["eng", "oncall"],
            "email": "user@corp.example",
            "level": 7,
        });
        // Array contains.
        assert!(claim_satisfied(
            &claims,
            &ClaimRequirement {
                claim: "groups".to_string(),
                any_of: vec!["oncall".to_string()],
            }
        ));
        // Array contains none.
        assert!(!claim_satisfied(
            &claims,
            &ClaimRequirement {
                claim: "groups".to_string(),
                any_of: vec!["admins".to_string()],
            }
        ));
        // Scalar string equals.
        assert!(claim_satisfied(
            &claims,
            &ClaimRequirement {
                claim: "email".to_string(),
                any_of: vec!["user@corp.example".to_string()],
            }
        ));
        // Scalar number equals (by string form).
        assert!(claim_satisfied(
            &claims,
            &ClaimRequirement {
                claim: "level".to_string(),
                any_of: vec!["7".to_string()],
            }
        ));
        // Absent claim never matches.
        assert!(!claim_satisfied(
            &claims,
            &ClaimRequirement {
                claim: "missing".to_string(),
                any_of: vec!["x".to_string()],
            }
        ));
        // Empty any_of fails closed.
        assert!(!claim_satisfied(
            &claims,
            &ClaimRequirement {
                claim: "groups".to_string(),
                any_of: vec![],
            }
        ));
    }

    #[test]
    fn extract_identity_handles_string_groups_and_absent_fields() {
        let id = extract_identity(&json!({ "sub": "s", "groups": "solo" }));
        assert_eq!(id.sub, "s");
        assert_eq!(id.email, None);
        assert_eq!(id.groups, vec!["solo".to_string()]);

        let empty = extract_identity(&json!({}));
        assert_eq!(empty.sub, "");
        assert!(empty.groups.is_empty());
    }

    #[test]
    fn jwk_algorithm_pins_family_default() {
        assert_eq!(jwk_algorithm(&rsa_jwks().keys[0]), Some(Algorithm::RS256));
        assert_eq!(jwk_algorithm(&ec_jwks().keys[0]), Some(Algorithm::ES256));
    }

    #[tokio::test]
    async fn discovery_and_jwks_fetch_over_http_then_caches() {
        // Spin up a local HTTP server serving the OIDC discovery doc + JWKS, point
        // the policy's issuer at it (no jwks_uri → forces discovery), and verify a
        // token end-to-end. A second call is served from the cache.
        use axum::routing::get;
        use axum::{Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let jwks_hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let disc_base = base.clone();
        let hits = jwks_hits.clone();
        let app =
            Router::new()
                .route(
                    "/.well-known/openid-configuration",
                    get(move || {
                        let base = disc_base.clone();
                        async move {
                            Json(json!({ "issuer": base, "jwks_uri": format!("{base}/keys") }))
                        }
                    }),
                )
                .route(
                    "/keys",
                    get(move || {
                        hits.fetch_add(1, Ordering::SeqCst);
                        async move { Json(serde_json::to_value(rsa_jwks()).unwrap()) }
                    }),
                );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let oidc = OidcAccess {
            issuer: base.clone(),
            audiences: vec![AUDIENCE.to_string()],
            jwks_uri: None, // force discovery
            required_claims: None,
            forward_identity: None,
        };
        let mut claims = standard_claims();
        claims["iss"] = json!(base);
        let token = sign(RSA_A_PRIV_PEM, KID_RSA, Algorithm::RS256, &claims);

        let v = Verifier::with_client(test_client());
        let id = v.verify(&oidc, &token).await.expect("allow via discovery");
        assert_eq!(id.sub, "user-123");
        // Second verify is served from cache — no extra JWKS fetch.
        v.verify(&oidc, &token).await.expect("allow from cache");
        assert_eq!(jwks_hits.load(Ordering::SeqCst), 1);
    }
}
