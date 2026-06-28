// SPDX-License-Identifier: BUSL-1.1
//! Agent Card signing (JWS / EdDSA) + JWKS publication (RFC 0013).
//!
//! The gateway signs every projected Agent Card with an Ed25519 key so A2A
//! clients can verify card authenticity, and publishes the public key as a JWKS
//! at `/.well-known/jwks.json`. The signature follows the A2A `AgentCardSignature`
//! shape: a JWS-style `{protected, signature}` over `protected . payload`, where
//! `payload` is base64url of the card serialized *without* its `signatures` field.
//!
//! Pure Rust (`ed25519-dalek`, `base64`) — no C toolchain (keep the image
//! dependency-free of a C compiler; agentctl P0 keep-it-pure).

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use serde_json::{json, Value};

/// The published key id, surfaced in both the JWS `protected` header and the JWKS.
pub const KID: &str = "agentctl-gateway-key-1";

/// Wraps the gateway's Ed25519 signing key.
pub struct Signer {
    key: SigningKey,
}

impl Signer {
    /// Load the signing key from `GATEWAY_SIGNING_SEED` — base64 of exactly 32
    /// bytes, accepting URL-safe-no-pad or standard alphabets.
    pub fn from_env() -> Result<Self, String> {
        let raw = std::env::var("GATEWAY_SIGNING_SEED")
            .map_err(|_| "GATEWAY_SIGNING_SEED must be set".to_string())?;
        let bytes = decode_b64(raw.trim())
            .ok_or_else(|| "GATEWAY_SIGNING_SEED must be valid base64".to_string())?;
        let seed: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| "GATEWAY_SIGNING_SEED must decode to exactly 32 bytes".to_string())?;
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// The Ed25519 verifying (public) key.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Sign `card` in place, attaching an A2A `signatures` array (JWS-style,
    /// EdDSA). Any pre-existing `signatures` field is removed first so the signed
    /// payload is exactly the unsigned card.
    pub fn sign_card(&self, card: &mut Value) {
        if let Some(obj) = card.as_object_mut() {
            obj.remove("signatures");
        }
        let payload_b64 = URL_SAFE_NO_PAD.encode(card.to_string());
        let protected_b64 =
            URL_SAFE_NO_PAD.encode(json!({ "alg": "EdDSA", "kid": KID }).to_string());
        let signing_input = format!("{protected_b64}.{payload_b64}");
        let sig = self.key.sign(signing_input.as_bytes());
        let signature_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        if let Some(obj) = card.as_object_mut() {
            obj.insert(
                "signatures".to_string(),
                json!([{ "protected": protected_b64, "signature": signature_b64 }]),
            );
        }
    }

    /// The JWKS document advertising the public key (for card verification).
    pub fn jwks(&self) -> Value {
        let x = URL_SAFE_NO_PAD.encode(self.verifying_key().to_bytes());
        json!({
            "keys": [{
                "kty": "OKP",
                "crv": "Ed25519",
                "x": x,
                "kid": KID,
                "alg": "EdDSA",
                "use": "sig",
            }]
        })
    }
}

/// Decode base64 trying URL-safe-no-pad first, then standard (padded).
fn decode_b64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(s)
        .ok()
        .or_else(|| STANDARD.decode(s).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signer over a fixed seed — deterministic, env-free (tests must never
    /// depend on `GATEWAY_SIGNING_SEED`).
    fn fixed_signer() -> Signer {
        Signer {
            key: SigningKey::from_bytes(&[7u8; 32]),
        }
    }

    /// Reconstruct the signing input from a signed card exactly as a verifier
    /// would: `protected . base64url(card-without-signatures)`.
    fn signing_input_of(card: &Value, protected_b64: &str) -> String {
        let mut unsigned = card.clone();
        unsigned.as_object_mut().unwrap().remove("signatures");
        let payload_b64 = URL_SAFE_NO_PAD.encode(unsigned.to_string());
        format!("{protected_b64}.{payload_b64}")
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let signer = fixed_signer();
        let mut card = json!({ "name": "ns/echo", "version": "1.2.3" });
        signer.sign_card(&mut card);

        let sigs = card["signatures"].as_array().expect("signatures array");
        assert_eq!(sigs.len(), 1);
        let protected_b64 = sigs[0]["protected"].as_str().unwrap();
        let signature_b64 = sigs[0]["signature"].as_str().unwrap();

        let signing_input = signing_input_of(&card, protected_b64);
        let sig_bytes = URL_SAFE_NO_PAD.decode(signature_b64).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        signer
            .verifying_key()
            .verify_strict(signing_input.as_bytes(), &sig)
            .expect("signature verifies");
    }

    #[test]
    fn tampered_card_fails_verification() {
        let signer = fixed_signer();
        let mut card = json!({ "name": "ns/echo", "version": "1.2.3" });
        signer.sign_card(&mut card);
        let protected_b64 = card["signatures"][0]["protected"]
            .as_str()
            .unwrap()
            .to_string();
        let signature_b64 = card["signatures"][0]["signature"]
            .as_str()
            .unwrap()
            .to_string();

        // Mutate the card body after signing.
        card["version"] = json!("9.9.9");

        let signing_input = signing_input_of(&card, &protected_b64);
        let sig_bytes = URL_SAFE_NO_PAD.decode(&signature_b64).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        assert!(signer
            .verifying_key()
            .verify_strict(signing_input.as_bytes(), &sig)
            .is_err());
    }

    #[test]
    fn sign_card_replaces_any_existing_signatures() {
        let signer = fixed_signer();
        let mut card = json!({ "name": "ns/echo", "signatures": ["stale"] });
        signer.sign_card(&mut card);
        let sigs = card["signatures"].as_array().unwrap();
        assert_eq!(sigs.len(), 1);
        assert!(sigs[0]["protected"].is_string());
        assert!(sigs[0]["signature"].is_string());
    }

    #[test]
    fn jwks_publishes_kid_and_x() {
        let signer = fixed_signer();
        let jwks = signer.jwks();
        let key = &jwks["keys"][0];
        assert_eq!(key["kid"], KID);
        assert_eq!(key["kty"], "OKP");
        assert_eq!(key["crv"], "Ed25519");
        assert_eq!(key["alg"], "EdDSA");
        assert_eq!(key["use"], "sig");
        let x = key["x"].as_str().expect("x present");
        assert!(!x.is_empty());
        // `x` decodes to the 32-byte public key.
        assert_eq!(URL_SAFE_NO_PAD.decode(x).unwrap().len(), 32);
    }

    #[test]
    fn decode_b64_accepts_both_alphabets() {
        let raw = [9u8; 32];
        let url = URL_SAFE_NO_PAD.encode(raw);
        let std = STANDARD.encode(raw);
        assert_eq!(decode_b64(&url).unwrap(), raw);
        assert_eq!(decode_b64(&std).unwrap(), raw);
        assert!(decode_b64("!!! not base64 !!!").is_none());
    }
}
