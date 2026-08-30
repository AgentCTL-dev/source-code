// SPDX-License-Identifier: BUSL-1.1
//! Per-(user, agent) A2A principal minting (RFC 0028 §6). The mint returns
//! the bearer secret EXACTLY ONCE (the caller — the operator — projects it
//! into a Kubernetes Secret and the agent's `a2a.principals[]`); custody keeps
//! only the SHA-256 hash, so verification is a hash lookup and a database
//! leak yields no usable bearer.

use base64::Engine as _;
use ring::rand::{SecureRandom, SystemRandom};

use crate::store::{bearer_hash, PrincipalRecord, Store, StoreError};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A mint request (operator-authenticated).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MintRequest {
    pub org: String,
    pub namespace: String,
    pub agent: String,
    /// The user subject (`<provider>:<sub>`).
    pub subject: String,
}

/// The mint response — the ONLY time the secret exists outside the caller.
#[derive(Debug, serde::Serialize)]
pub struct MintResponse {
    /// `pat-…` bearer, 32 random bytes base64url. Project it; do not store it.
    pub bearer: String,
    /// The Kubernetes Secret name the operator projects it under (convention:
    /// one Secret per agent holding one key per subject).
    pub secret_name: String,
    /// The env-style key within that Secret for this subject.
    pub secret_key: String,
}

/// The conventional per-agent principal Secret name.
pub fn principal_secret_name(agent: &str) -> String {
    format!("{agent}-principals")
}

/// A subject's key inside the principal Secret (DNS/env-safe).
pub fn principal_secret_key(subject: &str) -> String {
    let safe: String = subject
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("PRINCIPAL_{}", safe.to_uppercase())
}

/// Mint (or re-mint) the principal for (namespace, agent, subject).
pub async fn mint(store: &dyn Store, req: MintRequest) -> Result<MintResponse, StoreError> {
    let rng = SystemRandom::new();
    let mut raw = [0u8; 32];
    rng.fill(&mut raw).expect("system rng");
    let bearer = format!("pat-{}", B64.encode(raw));
    store
        .put_principal(PrincipalRecord {
            org: req.org,
            namespace: req.namespace,
            agent: req.agent.clone(),
            subject: req.subject.clone(),
            bearer_hash: bearer_hash(&bearer),
            created_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        })
        .await?;
    Ok(MintResponse {
        secret_name: principal_secret_name(&req.agent),
        secret_key: principal_secret_key(&req.subject),
        bearer,
    })
}

/// Verify a presented bearer → its principal record (the gateway's fast path).
pub async fn verify(store: &dyn Store, bearer: &str) -> Result<PrincipalRecord, StoreError> {
    store.find_principal_by_hash(&bearer_hash(bearer)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    #[tokio::test]
    async fn mint_returns_secret_once_and_verify_finds_it() {
        let store = MemoryStore::default();
        let resp = mint(
            &store,
            MintRequest {
                org: "acme".into(),
                namespace: "org-acme".into(),
                agent: "triage".into(),
                subject: "okta:alice".into(),
            },
        )
        .await
        .unwrap();
        assert!(resp.bearer.starts_with("pat-"));
        assert_eq!(resp.secret_name, "triage-principals");
        assert_eq!(resp.secret_key, "PRINCIPAL_OKTA_ALICE");

        let found = verify(&store, &resp.bearer).await.unwrap();
        assert_eq!(found.subject, "okta:alice");
        assert_eq!(found.org, "acme");
        assert!(verify(&store, "pat-not-a-real-bearer").await.is_err());
    }

    #[tokio::test]
    async fn remint_rotates_the_bearer() {
        let store = MemoryStore::default();
        let req = MintRequest {
            org: "acme".into(),
            namespace: "org-acme".into(),
            agent: "triage".into(),
            subject: "okta:alice".into(),
        };
        let first = mint(&store, req.clone()).await.unwrap();
        let second = mint(&store, req).await.unwrap();
        assert_ne!(first.bearer, second.bearer);
        assert!(
            verify(&store, &first.bearer).await.is_err(),
            "old bearer dead"
        );
        assert!(verify(&store, &second.bearer).await.is_ok());
    }
}
