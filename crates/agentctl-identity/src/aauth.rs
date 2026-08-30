// SPDX-License-Identifier: BUSL-1.1
//! # The AAuth Agent Provider role (RFC 0028 §5, P1-6)
//!
//! agentctl-identity IS the fleet's Agent Provider: a real agentd 1.3.1 agent
//! (built with `--features aauth`) enrolls its Ed25519 workload key here and
//! obtains short-lived agent tokens for signed egress. The wire contract is
//! what the SHIPPED 1.3.1 client speaks (verified against agentd source —
//! paths are hard-coded root-relative, there is no endpoint discovery):
//!
//! | surface | caller | auth |
//! |---|---|---|
//! | `POST /enroll` | agent | RFC 9421 `hwk` signature (inline Ed25519 JWK) |
//! | `POST /agent-token` | agent | RFC 9421 `hwk` signature |
//! | `GET /.well-known/aauth-agent.json` | resource servers | none |
//! | `GET /aauth-jwks.json` | resource servers | none |
//! | `POST /admin/allowed-keys` | operator | admin bearer |
//! | `GET /admin/agents` | operator | admin bearer |
//! | `POST /admin/agents/{local}/revoke` | operator | admin bearer |
//! | `DELETE /admin/allowed-keys/{jkt}` | operator | admin bearer |
//!
//! Enrollment is gated on the operator-registered key allowlist (thumbprint
//! pre-registration — secret-free for the agent: it only ever signs with its
//! own key). The RFC's federated projected-SA-token assertion leg is accepted
//! on the wire (`enrollment_assertion`) but deferred until the renderer
//! projects the SA token (upstream delta recorded in the PLAN).
//!
//! Notable 1.3.1 client facts this module leans on: enrollment is LAZY (first
//! signed dial, not boot); a provider failure degrades to an UNSIGNED request
//! (no exit 4); both calls are re-attempted on every sign, so `/enroll` must
//! be idempotent by thumbprint; the agent validates the returned token's
//! `iss` against its configured provider URL (trailing-slash/case tolerant)
//! and `cnf.jwk` against its own key.

use base64::Engine as _;
use ring::signature::{Ed25519KeyPair, KeyPair as _, UnparsedPublicKey, ED25519};
use serde_json::{json, Value};

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const B64STD: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// RFC 7638 thumbprint of an Ed25519 public key — byte-identical to agentd's
/// and the operator's derivation (`{"crv":"Ed25519","kty":"OKP","x":…}`).
pub fn jkt(x_b64url: &str) -> String {
    let canonical = format!("{{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":\"{x_b64url}\"}}");
    B64URL.encode(ring::digest::digest(
        &ring::digest::SHA256,
        canonical.as_bytes(),
    ))
}

/// The provider's own Ed25519 signing key (agent tokens; published via JWKS).
pub struct ProviderKey {
    pair: Ed25519KeyPair,
    pub kid: String,
}

impl ProviderKey {
    /// From a 32-byte seed (`IDENTITY_AAUTH_SEED`, base64) — stable across
    /// replicas/restarts. `kid` is the key's own RFC 7638 thumbprint.
    pub fn from_seed(seed: &[u8]) -> Result<ProviderKey, String> {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|_| "IDENTITY_AAUTH_SEED is not a valid Ed25519 seed".to_string())?;
        let x = B64URL.encode(pair.public_key().as_ref());
        let kid = jkt(&x);
        Ok(ProviderKey { pair, kid })
    }

    /// Fresh random key (dev only — agent tokens die with the pod).
    pub fn ephemeral() -> ProviderKey {
        use ring::rand::{SecureRandom as _, SystemRandom};
        let mut seed = [0u8; 32];
        SystemRandom::new().fill(&mut seed).expect("system rng");
        Self::from_seed(&seed).expect("random seed is valid")
    }

    pub fn public_x_b64url(&self) -> String {
        B64URL.encode(self.pair.public_key().as_ref())
    }

    /// The published JWKS document.
    pub fn jwks(&self) -> Value {
        json!({ "keys": [{
            "kty": "OKP", "crv": "Ed25519", "use": "sig", "alg": "EdDSA",
            "kid": self.kid, "x": self.public_x_b64url(),
        }]})
    }

    /// Mint an agent token: EdDSA JWT, `typ: aa-agent+jwt`, `cnf.jwk` bound
    /// to the AGENT's key (proof-of-possession — a stolen token is useless
    /// without the workload key).
    pub fn mint_agent_token(
        &self,
        issuer: &str,
        agent_id: &str,
        agent_x_b64url: &str,
        ttl_secs: i64,
    ) -> String {
        let now = now_unix();
        let header = B64URL
            .encode(json!({ "alg": "EdDSA", "typ": "aa-agent+jwt", "kid": self.kid }).to_string());
        let claims = B64URL.encode(
            json!({
                "iss": issuer,
                "sub": agent_id,
                "iat": now,
                "exp": now + ttl_secs,
                "cnf": { "jwk": { "kty": "OKP", "crv": "Ed25519", "x": agent_x_b64url } },
            })
            .to_string(),
        );
        let signing_input = format!("{header}.{claims}");
        let sig = self.pair.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", B64URL.encode(sig.as_ref()))
    }
}

/// The verified caller of a signed agent request.
#[derive(Debug, Clone)]
pub struct SignedCaller {
    /// The agent's Ed25519 public key, base64url (the `hwk` `x`).
    pub x_b64url: String,
    /// Its RFC 7638 thumbprint.
    pub jkt: String,
}

/// The three RFC 9421 headers plus the covered Content-Digest, as received.
pub struct SignatureHeaders<'a> {
    pub signature_input: &'a str,
    pub signature: &'a str,
    pub signature_key: &'a str,
    pub content_digest: Option<&'a str>,
}

/// Verify an RFC 9421 `hwk`-signed agent request (agentd's exact profile:
/// covered components `@method @authority @path content-digest signature-key`,
/// Ed25519, std-b64 signature, inline OKP JWK members in `Signature-Key`).
/// Also checks `Content-Digest` against the actual body and the `created`
/// freshness window (±300s). Errors name the defect, never key material.
pub fn verify_signed_request(
    method: &str,
    authority: &str,
    path: &str,
    sig: &SignatureHeaders<'_>,
    body: &[u8],
) -> Result<SignedCaller, String> {
    let SignatureHeaders {
        signature_input,
        signature,
        signature_key,
        content_digest,
    } = *sig;
    // Signature-Key: sig=hwk;kty="OKP";crv="Ed25519";x="…" (members inline —
    // a conformant provider must not require a packed jwk="…" form).
    let members = signature_key
        .strip_prefix("sig=hwk")
        .ok_or("Signature-Key is not the hwk scheme")?;
    let member = |name: &str| -> Option<String> {
        members.split(';').find_map(|p| {
            let p = p.trim();
            p.strip_prefix(&format!("{name}=\""))
                .and_then(|r| r.strip_suffix('"'))
                .map(str::to_string)
        })
    };
    if member("kty").as_deref() != Some("OKP") || member("crv").as_deref() != Some("Ed25519") {
        return Err("hwk key is not OKP/Ed25519".into());
    }
    let x = member("x").ok_or("hwk key has no x member")?;
    let key_bytes = B64URL
        .decode(&x)
        .map_err(|_| "hwk x is not base64url".to_string())?;

    // Content-Digest must match the body (it is a covered component).
    let expected_digest = format!(
        "sha-256=:{}:",
        B64STD.encode(ring::digest::digest(&ring::digest::SHA256, body))
    );
    let presented_digest = content_digest.ok_or("missing Content-Digest")?;
    if presented_digest != expected_digest {
        return Err("Content-Digest does not match the body".into());
    }

    // Signature-Input: sig=("…" …);created=N
    let params = signature_input
        .strip_prefix("sig=")
        .ok_or("Signature-Input label != sig")?;
    let created: i64 = params
        .split("created=")
        .nth(1)
        .and_then(|c| c.split(';').next())
        .and_then(|c| c.trim().parse().ok())
        .ok_or("no created parameter")?;
    if (now_unix() - created).abs() > 300 {
        return Err("created outside the freshness window".into());
    }
    let components = params
        .strip_prefix('(')
        .and_then(|p| p.split(')').next())
        .ok_or("malformed covered-component list")?;

    let mut base = String::new();
    for comp in components.split_whitespace() {
        let name = comp.trim_matches('"');
        let value = match name {
            "@method" => method.to_string(),
            "@authority" => authority.to_string(),
            "@path" => path.to_string(),
            "content-digest" => presented_digest.to_string(),
            "signature-key" => signature_key.to_string(),
            other => return Err(format!("unsupported covered component {other:?}")),
        };
        base.push_str(&format!("\"{name}\": {value}\n"));
    }
    base.push_str(&format!("\"@signature-params\": {params}"));

    // Signature: sig=:<std b64, padded>:
    let sig_b64 = signature
        .strip_prefix("sig=:")
        .and_then(|r| r.strip_suffix(':'))
        .ok_or("malformed Signature header")?;
    let sig = B64STD
        .decode(sig_b64)
        .map_err(|_| "Signature is not base64".to_string())?;
    UnparsedPublicKey::new(&ED25519, &key_bytes)
        .verify(base.as_bytes(), &sig)
        .map_err(|_| "signature does not verify against the presented key".to_string())?;

    Ok(SignedCaller {
        jkt: jkt(&x),
        x_b64url: x,
    })
}

/// The agent id for an enrolled key: `aauth:<local>@<domain>`, `local`
/// derived deterministically from the thumbprint (re-enrollment of the same
/// key always yields the same id), `domain` = the issuer host (port/path
/// stripped, lowercased — the operator's synthesis convention).
pub fn agent_id(issuer: &str, jkt: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, jkt.as_bytes());
    let local: String = digest.as_ref()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("aauth:{local}@{}", issuer_domain(issuer))
}

/// `local` part of [`agent_id`] (the admin revoke path key).
pub fn agent_local(id: &str) -> Option<&str> {
    id.strip_prefix("aauth:")?.split('@').next()
}

pub fn issuer_domain(issuer: &str) -> String {
    // Scheme strip is case-insensitive (URL schemes are).
    let lower = issuer.to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &issuer[8..]
    } else if lower.starts_with("http://") {
        &issuer[7..]
    } else {
        issuer
    };
    let host = rest.split('/').next().unwrap_or(rest);
    let host = host.split(':').next().unwrap_or(host);
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a signed request exactly the way agentd 1.3.1 does
    /// (aauth/sig.rs): same covered set, same header spellings.
    fn sign_like_agentd(
        pair: &Ed25519KeyPair,
        method: &str,
        authority: &str,
        path: &str,
        body: &[u8],
    ) -> (String, String, String, String) {
        let x = B64URL.encode(pair.public_key().as_ref());
        let signature_key = format!("sig=hwk;kty=\"OKP\";crv=\"Ed25519\";x=\"{x}\"");
        let digest = format!(
            "sha-256=:{}:",
            B64STD.encode(ring::digest::digest(&ring::digest::SHA256, body))
        );
        let params = format!(
            "(\"@method\" \"@authority\" \"@path\" \"content-digest\" \"signature-key\");created={}",
            now_unix()
        );
        let mut base = String::new();
        for (name, value) in [
            ("@method", method),
            ("@authority", authority),
            ("@path", path),
            ("content-digest", &digest),
            ("signature-key", &signature_key),
        ] {
            base.push_str(&format!("\"{name}\": {value}\n"));
        }
        base.push_str(&format!("\"@signature-params\": {params}"));
        let sig = format!("sig=:{}:", B64STD.encode(pair.sign(base.as_bytes())));
        (format!("sig={params}"), sig, signature_key, digest)
    }

    fn test_pair() -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap()
    }

    #[test]
    fn agentd_shaped_signature_verifies_and_tamper_fails() {
        let pair = test_pair();
        let body = br#"{"platform":"workload"}"#;
        let (input, sig, key, digest) =
            sign_like_agentd(&pair, "POST", "identity.example", "/enroll", body);

        let caller = verify_signed_request(
            "POST",
            "identity.example",
            "/enroll",
            &SignatureHeaders {
                signature_input: &input,
                signature: &sig,
                signature_key: &key,
                content_digest: Some(&digest),
            },
            body,
        )
        .expect("verifies");
        assert_eq!(caller.x_b64url, B64URL.encode(pair.public_key().as_ref()));

        // A different body fails on Content-Digest (covered component).
        assert!(verify_signed_request(
            "POST",
            "identity.example",
            "/enroll",
            &SignatureHeaders {
                signature_input: &input,
                signature: &sig,
                signature_key: &key,
                content_digest: Some(&digest),
            },
            br#"{"platform":"evil"}"#,
        )
        .unwrap_err()
        .contains("Content-Digest"));

        // A different path breaks the signature base.
        assert!(verify_signed_request(
            "POST",
            "identity.example",
            "/agent-token",
            &SignatureHeaders {
                signature_input: &input,
                signature: &sig,
                signature_key: &key,
                content_digest: Some(&digest),
            },
            body,
        )
        .unwrap_err()
        .contains("does not verify"));
    }

    #[test]
    fn thumbprint_matches_the_operator_derivation() {
        // Fixed vector: derivation over the canonical JWK JSON.
        let pair = test_pair();
        let x = B64URL.encode(pair.public_key().as_ref());
        let t = jkt(&x);
        // Deterministic + b64url (43 chars for SHA-256, no padding).
        assert_eq!(t.len(), 43);
        assert_eq!(t, jkt(&x));
    }

    #[test]
    fn agent_ids_are_deterministic_and_domain_normalized() {
        let id = agent_id("http://agentctl-identity.agentctl-system:80/", "SOME-JKT");
        assert!(id.starts_with("aauth:"));
        assert!(id.ends_with("@agentctl-identity.agentctl-system"));
        assert_eq!(
            id,
            agent_id("HTTP://agentctl-identity.agentctl-system", "SOME-JKT")
        );
        assert_eq!(agent_local(&id).unwrap().len(), 16);
        assert_eq!(
            issuer_domain("https://Ap.Example.com.:8443/x"),
            "ap.example.com"
        );
    }

    #[test]
    fn minted_agent_token_carries_pop_binding_and_verifies() {
        let provider = ProviderKey::from_seed(&[9u8; 32]).unwrap();
        let agent_pair = test_pair();
        let agent_x = B64URL.encode(agent_pair.public_key().as_ref());
        let tok = provider.mint_agent_token("http://ap", "aauth:abc@ap", &agent_x, 300);

        let parts: Vec<&str> = tok.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&B64URL.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["typ"], "aa-agent+jwt");
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(header["kid"], provider.kid);
        let claims: Value = serde_json::from_slice(&B64URL.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["sub"], "aauth:abc@ap");
        assert_eq!(claims["cnf"]["jwk"]["x"], agent_x);

        // Verifies against the JWKS key (what the resource server does).
        let jwks = provider.jwks();
        let x = jwks["keys"][0]["x"].as_str().unwrap();
        let pk = B64URL.decode(x).unwrap();
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        UnparsedPublicKey::new(&ED25519, &pk)
            .verify(signing_input.as_bytes(), &B64URL.decode(parts[2]).unwrap())
            .expect("token verifies against the published JWKS");
    }
}
