// SPDX-License-Identifier: BUSL-1.1
//! Durable custody: device-flow sessions, minted principals, and (P5) user
//! connections. One trait, two backends — Postgres (production) and memory
//! (tests/dev; custody dies with the process and says so at startup).
//!
//! Secret discipline: principal bearers are stored as **SHA-256 hashes** (a
//! verify is a hash compare; the secret is returned exactly once at mint);
//! sealed columns (connections) go through [`crate::seal::Sealer`] before this
//! layer ever sees them — the store holds opaque strings either way.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("postgres: {0}")]
    Pg(String),
    #[error("not found")]
    NotFound,
}

/// A device-flow session parked between `/device/start` and `/device/poll`.
#[derive(Debug, Clone)]
pub struct DeviceSession {
    pub handle: String,
    pub provider: String,
    pub device_code: String,
    pub interval_secs: u64,
    pub expires_unix: i64,
}

/// A minted A2A principal (user × agent), stored hash-only.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PrincipalRecord {
    pub org: String,
    pub namespace: String,
    pub agent: String,
    pub subject: String,
    /// SHA-256 (base64url) of the bearer secret.
    #[serde(skip)]
    pub bearer_hash: String,
    pub created_unix: i64,
}

/// SHA-256 → base64url, the bearer-hash convention.
pub fn bearer_hash(secret: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, secret.as_bytes());
    B64.encode(digest.as_ref())
}

/// The custody surface. Small on purpose; every method is total.
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    async fn put_device_session(&self, s: DeviceSession) -> Result<(), StoreError>;
    async fn take_device_session(&self, handle: &str) -> Result<DeviceSession, StoreError>;
    /// Upsert by (namespace, agent, subject); a re-mint replaces the hash.
    async fn put_principal(&self, p: PrincipalRecord) -> Result<(), StoreError>;
    async fn find_principal_by_hash(&self, hash: &str) -> Result<PrincipalRecord, StoreError>;
    async fn list_principals(
        &self,
        namespace: &str,
        agent: &str,
    ) -> Result<Vec<PrincipalRecord>, StoreError>;
}

// ---------------------------------------------------------------------------
// memory backend
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MemoryStore {
    devices: Mutex<HashMap<String, DeviceSession>>,
    principals: Mutex<HashMap<String, PrincipalRecord>>, // key ns/agent/subject
}

#[async_trait::async_trait]
impl Store for MemoryStore {
    async fn put_device_session(&self, s: DeviceSession) -> Result<(), StoreError> {
        self.devices.lock().unwrap().insert(s.handle.clone(), s);
        Ok(())
    }

    async fn take_device_session(&self, handle: &str) -> Result<DeviceSession, StoreError> {
        self.devices
            .lock()
            .unwrap()
            .get(handle)
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    async fn put_principal(&self, p: PrincipalRecord) -> Result<(), StoreError> {
        let key = format!("{}/{}/{}", p.namespace, p.agent, p.subject);
        self.principals.lock().unwrap().insert(key, p);
        Ok(())
    }

    async fn find_principal_by_hash(&self, hash: &str) -> Result<PrincipalRecord, StoreError> {
        self.principals
            .lock()
            .unwrap()
            .values()
            .find(|p| p.bearer_hash == hash)
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    async fn list_principals(
        &self,
        namespace: &str,
        agent: &str,
    ) -> Result<Vec<PrincipalRecord>, StoreError> {
        Ok(self
            .principals
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.namespace == namespace && p.agent == agent)
            .cloned()
            .collect())
    }
}

// ---------------------------------------------------------------------------
// postgres backend
// ---------------------------------------------------------------------------

pub struct PgStore {
    pool: deadpool_postgres::Pool,
}

/// Schema, applied idempotently at startup (the control-plane migration
/// convention until a dedicated migration story lands with P2's chart work).
const MIGRATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS identity_device_sessions (
    handle        TEXT PRIMARY KEY,
    provider      TEXT NOT NULL,
    device_code   TEXT NOT NULL,
    interval_secs BIGINT NOT NULL,
    expires_unix  BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_principals (
    namespace    TEXT NOT NULL,
    agent        TEXT NOT NULL,
    subject      TEXT NOT NULL,
    org          TEXT NOT NULL,
    bearer_hash  TEXT NOT NULL,
    created_unix BIGINT NOT NULL,
    PRIMARY KEY (namespace, agent, subject)
);
CREATE INDEX IF NOT EXISTS identity_principals_hash ON identity_principals (bearer_hash);
"#;

impl PgStore {
    pub async fn connect(dsn: &str) -> Result<PgStore, StoreError> {
        let cfg: tokio_postgres::Config =
            dsn.parse().map_err(|e| StoreError::Pg(format!("{e}")))?;
        let mgr = deadpool_postgres::Manager::from_config(
            cfg,
            tokio_postgres::NoTls,
            deadpool_postgres::ManagerConfig {
                recycling_method: deadpool_postgres::RecyclingMethod::Fast,
            },
        );
        let pool = deadpool_postgres::Pool::builder(mgr)
            .max_size(8)
            .build()
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        let client = pool
            .get()
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        client
            .batch_execute(MIGRATIONS)
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(PgStore { pool })
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, StoreError> {
        self.pool
            .get()
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))
    }
}

#[async_trait::async_trait]
impl Store for PgStore {
    async fn put_device_session(&self, s: DeviceSession) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO identity_device_sessions (handle, provider, device_code, interval_secs, expires_unix)
                 VALUES ($1,$2,$3,$4,$5)
                 ON CONFLICT (handle) DO UPDATE SET device_code=$3, interval_secs=$4, expires_unix=$5",
                &[&s.handle, &s.provider, &s.device_code, &(s.interval_secs as i64), &s.expires_unix],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(())
    }

    async fn take_device_session(&self, handle: &str) -> Result<DeviceSession, StoreError> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT provider, device_code, interval_secs, expires_unix
                 FROM identity_device_sessions WHERE handle = $1",
                &[&handle],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?
            .ok_or(StoreError::NotFound)?;
        Ok(DeviceSession {
            handle: handle.to_string(),
            provider: row.get(0),
            device_code: row.get(1),
            interval_secs: row.get::<_, i64>(2) as u64,
            expires_unix: row.get(3),
        })
    }

    async fn put_principal(&self, p: PrincipalRecord) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO identity_principals (namespace, agent, subject, org, bearer_hash, created_unix)
                 VALUES ($1,$2,$3,$4,$5,$6)
                 ON CONFLICT (namespace, agent, subject)
                 DO UPDATE SET org=$4, bearer_hash=$5, created_unix=$6",
                &[&p.namespace, &p.agent, &p.subject, &p.org, &p.bearer_hash, &p.created_unix],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(())
    }

    async fn find_principal_by_hash(&self, hash: &str) -> Result<PrincipalRecord, StoreError> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT namespace, agent, subject, org, created_unix
                 FROM identity_principals WHERE bearer_hash = $1",
                &[&hash],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?
            .ok_or(StoreError::NotFound)?;
        Ok(PrincipalRecord {
            namespace: row.get(0),
            agent: row.get(1),
            subject: row.get(2),
            org: row.get(3),
            bearer_hash: hash.to_string(),
            created_unix: row.get(4),
        })
    }

    async fn list_principals(
        &self,
        namespace: &str,
        agent: &str,
    ) -> Result<Vec<PrincipalRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT subject, org, bearer_hash, created_unix
                 FROM identity_principals WHERE namespace = $1 AND agent = $2",
                &[&namespace, &agent],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(rows
            .into_iter()
            .map(|row| PrincipalRecord {
                namespace: namespace.to_string(),
                agent: agent.to_string(),
                subject: row.get(0),
                org: row.get(1),
                bearer_hash: row.get(2),
                created_unix: row.get(3),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_principal_roundtrip_by_hash_only() {
        let s = MemoryStore::default();
        let secret = "pat-abc123";
        s.put_principal(PrincipalRecord {
            org: "acme".into(),
            namespace: "org-acme".into(),
            agent: "triage".into(),
            subject: "okta:alice".into(),
            bearer_hash: bearer_hash(secret),
            created_unix: 1,
        })
        .await
        .unwrap();
        let found = s
            .find_principal_by_hash(&bearer_hash(secret))
            .await
            .unwrap();
        assert_eq!(found.subject, "okta:alice");
        assert!(s
            .find_principal_by_hash(&bearer_hash("wrong"))
            .await
            .is_err());
        // A re-mint replaces the hash (old bearer stops verifying).
        s.put_principal(PrincipalRecord {
            org: "acme".into(),
            namespace: "org-acme".into(),
            agent: "triage".into(),
            subject: "okta:alice".into(),
            bearer_hash: bearer_hash("pat-rotated"),
            created_unix: 2,
        })
        .await
        .unwrap();
        assert!(s
            .find_principal_by_hash(&bearer_hash(secret))
            .await
            .is_err());
        assert!(s
            .find_principal_by_hash(&bearer_hash("pat-rotated"))
            .await
            .is_ok());
    }
}
