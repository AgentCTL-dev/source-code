// SPDX-License-Identifier: BUSL-1.1
//! Envelope encryption for grants at rest (AES-256-GCM via ring). One data key
//! today (the chart-provided `IDENTITY_SEAL_KEY`); the KMS-wrapped key
//! hierarchy (per-org BYOK) slots behind this same seam later (RFC 0028 §10).
//!
//! Format: `v1.` + base64(nonce ‖ ciphertext‖tag). Nonces are 96-bit random —
//! at our mint rates collision probability is negligible, and every value is
//! independently sealed (no counter state to lose across replicas).

use base64::Engine as _;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SealError {
    #[error("sealed value is malformed")]
    Malformed,
    #[error("unseal failed (wrong key or corrupt value)")]
    Unseal,
    #[error("seal failed")]
    Seal,
}

/// The sealing key. Cloneable handle; the key material itself lives once.
pub struct Sealer {
    key: LessSafeKey,
    rng: SystemRandom,
}

impl Sealer {
    pub fn new(key: [u8; 32]) -> Sealer {
        let unbound = UnboundKey::new(&AES_256_GCM, &key).expect("32-byte AES-256-GCM key");
        Sealer {
            key: LessSafeKey::new(unbound),
            rng: SystemRandom::new(),
        }
    }

    /// Ephemeral key for dev/memory mode — grants sealed with it die with the
    /// process, which is exactly the posture memory-store mode advertises.
    pub fn ephemeral() -> Sealer {
        let rng = SystemRandom::new();
        let mut key = [0u8; 32];
        rng.fill(&mut key).expect("system rng");
        Sealer::new(key)
    }

    /// Seal a secret value; `aad` binds the ciphertext to its context (e.g.
    /// `"connection/<subject>/<provider>"`) so rows cannot be swapped.
    pub fn seal(&self, aad: &str, plaintext: &[u8]) -> Result<String, SealError> {
        let mut nonce = [0u8; NONCE_LEN];
        self.rng.fill(&mut nonce).map_err(|_| SealError::Seal)?;
        let mut buf = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad.as_bytes()),
                &mut buf,
            )
            .map_err(|_| SealError::Seal)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&buf);
        Ok(format!("v1.{}", B64.encode(out)))
    }

    /// Unseal; fails closed on any malformation or context mismatch.
    pub fn unseal(&self, aad: &str, sealed: &str) -> Result<Vec<u8>, SealError> {
        let b64 = sealed.strip_prefix("v1.").ok_or(SealError::Malformed)?;
        let raw = B64.decode(b64).map_err(|_| SealError::Malformed)?;
        if raw.len() < NONCE_LEN + AES_256_GCM.tag_len() {
            return Err(SealError::Malformed);
        }
        let (nonce, ct) = raw.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| SealError::Malformed)?;
        let mut buf = ct.to_vec();
        let plain = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad.as_bytes()),
                &mut buf,
            )
            .map_err(|_| SealError::Unseal)?;
        Ok(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip_and_context_binding() {
        let s = Sealer::new([7u8; 32]);
        let sealed = s
            .seal("connection/okta:alice/github", b"refresh-token-value")
            .unwrap();
        assert!(sealed.starts_with("v1."));
        assert_eq!(
            s.unseal("connection/okta:alice/github", &sealed).unwrap(),
            b"refresh-token-value"
        );
        // A different context must NOT unseal (row-swap defense).
        assert_eq!(
            s.unseal("connection/okta:mallory/github", &sealed),
            Err(SealError::Unseal)
        );
        // A different key must not unseal.
        let other = Sealer::new([8u8; 32]);
        assert_eq!(
            other.unseal("connection/okta:alice/github", &sealed),
            Err(SealError::Unseal)
        );
    }

    #[test]
    fn malformed_values_fail_closed() {
        let s = Sealer::new([7u8; 32]);
        assert_eq!(s.unseal("x", "garbage"), Err(SealError::Malformed));
        assert_eq!(s.unseal("x", "v1.!!!"), Err(SealError::Malformed));
        assert_eq!(s.unseal("x", "v1.AAAA"), Err(SealError::Malformed));
    }
}
