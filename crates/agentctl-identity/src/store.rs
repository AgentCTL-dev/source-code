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

/// An operator-registered enrollment allowlist entry (AAuth, RFC 0028 §5):
/// the agent key thumbprint the operator pre-registered, before the agent's
/// first signed dial.
#[derive(Debug, Clone)]
pub struct AllowedKey {
    /// RFC 7638 thumbprint (base64url).
    pub jkt: String,
    /// `<namespace>/<name>` (the operator's registration label).
    pub label: String,
    pub expires_unix: i64,
}

/// An enrolled AAuth agent.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AauthAgent {
    /// The `<local>` half of the agent id (admin revoke path key).
    pub local: String,
    /// Full id: `aauth:<local>@<domain>`.
    pub agent: String,
    pub jkt: String,
    pub label: String,
    /// `active` | `revoked`.
    pub status: String,
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

    // -- AAuth provider custody (RFC 0028 §5) -------------------------------
    /// Upsert by jkt (re-registration refreshes label/ttl).
    async fn put_allowed_key(&self, k: AllowedKey) -> Result<(), StoreError>;
    async fn find_allowed_key(&self, jkt: &str) -> Result<AllowedKey, StoreError>;
    /// Ok(true) deleted; Ok(false) was absent (admin DELETE is idempotent).
    async fn delete_allowed_key(&self, jkt: &str) -> Result<bool, StoreError>;
    /// Upsert by local (re-enrollment of the same key is a no-op refresh).
    async fn put_aauth_agent(&self, a: AauthAgent) -> Result<(), StoreError>;
    async fn find_aauth_agent_by_jkt(&self, jkt: &str) -> Result<AauthAgent, StoreError>;
    async fn list_aauth_agents(&self) -> Result<Vec<AauthAgent>, StoreError>;
    /// Ok(true) revoked; Ok(false) unknown local.
    async fn revoke_aauth_agent(&self, local: &str) -> Result<bool, StoreError>;
}

// ---------------------------------------------------------------------------
// memory backend
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MemoryStore {
    devices: Mutex<HashMap<String, DeviceSession>>,
    principals: Mutex<HashMap<String, PrincipalRecord>>, // key ns/agent/subject
    allowed_keys: Mutex<HashMap<String, AllowedKey>>,    // key jkt
    aauth_agents: Mutex<HashMap<String, AauthAgent>>,    // key local
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

    async fn put_allowed_key(&self, k: AllowedKey) -> Result<(), StoreError> {
        self.allowed_keys.lock().unwrap().insert(k.jkt.clone(), k);
        Ok(())
    }

    async fn find_allowed_key(&self, jkt: &str) -> Result<AllowedKey, StoreError> {
        self.allowed_keys
            .lock()
            .unwrap()
            .get(jkt)
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    async fn delete_allowed_key(&self, jkt: &str) -> Result<bool, StoreError> {
        Ok(self.allowed_keys.lock().unwrap().remove(jkt).is_some())
    }

    async fn put_aauth_agent(&self, a: AauthAgent) -> Result<(), StoreError> {
        self.aauth_agents.lock().unwrap().insert(a.local.clone(), a);
        Ok(())
    }

    async fn find_aauth_agent_by_jkt(&self, jkt: &str) -> Result<AauthAgent, StoreError> {
        self.aauth_agents
            .lock()
            .unwrap()
            .values()
            .find(|a| a.jkt == jkt)
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    async fn list_aauth_agents(&self) -> Result<Vec<AauthAgent>, StoreError> {
        Ok(self
            .aauth_agents
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect())
    }

    async fn revoke_aauth_agent(&self, local: &str) -> Result<bool, StoreError> {
        match self.aauth_agents.lock().unwrap().get_mut(local) {
            Some(a) => {
                a.status = "revoked".into();
                Ok(true)
            }
            None => Ok(false),
        }
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
CREATE TABLE IF NOT EXISTS identity_allowed_keys (
    jkt          TEXT PRIMARY KEY,
    label        TEXT NOT NULL,
    expires_unix BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_aauth_agents (
    local        TEXT PRIMARY KEY,
    agent        TEXT NOT NULL,
    jkt          TEXT NOT NULL,
    label        TEXT NOT NULL,
    status       TEXT NOT NULL,
    created_unix BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS identity_aauth_agents_jkt ON identity_aauth_agents (jkt);
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

    async fn put_allowed_key(&self, k: AllowedKey) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO identity_allowed_keys (jkt, label, expires_unix)
                 VALUES ($1,$2,$3)
                 ON CONFLICT (jkt) DO UPDATE SET label=$2, expires_unix=$3",
                &[&k.jkt, &k.label, &k.expires_unix],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(())
    }

    async fn find_allowed_key(&self, jkt: &str) -> Result<AllowedKey, StoreError> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT label, expires_unix FROM identity_allowed_keys WHERE jkt = $1",
                &[&jkt],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?
            .ok_or(StoreError::NotFound)?;
        Ok(AllowedKey {
            jkt: jkt.to_string(),
            label: row.get(0),
            expires_unix: row.get(1),
        })
    }

    async fn delete_allowed_key(&self, jkt: &str) -> Result<bool, StoreError> {
        let n = self
            .client()
            .await?
            .execute("DELETE FROM identity_allowed_keys WHERE jkt = $1", &[&jkt])
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(n > 0)
    }

    async fn put_aauth_agent(&self, a: AauthAgent) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO identity_aauth_agents (local, agent, jkt, label, status, created_unix)
                 VALUES ($1,$2,$3,$4,$5,$6)
                 ON CONFLICT (local) DO UPDATE SET agent=$2, jkt=$3, label=$4, status=$5",
                &[
                    &a.local,
                    &a.agent,
                    &a.jkt,
                    &a.label,
                    &a.status,
                    &a.created_unix,
                ],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(())
    }

    async fn find_aauth_agent_by_jkt(&self, jkt: &str) -> Result<AauthAgent, StoreError> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT local, agent, label, status, created_unix
                 FROM identity_aauth_agents WHERE jkt = $1",
                &[&jkt],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?
            .ok_or(StoreError::NotFound)?;
        Ok(AauthAgent {
            local: row.get(0),
            agent: row.get(1),
            jkt: jkt.to_string(),
            label: row.get(2),
            status: row.get(3),
            created_unix: row.get(4),
        })
    }

    async fn list_aauth_agents(&self) -> Result<Vec<AauthAgent>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT local, agent, jkt, label, status, created_unix FROM identity_aauth_agents",
                &[],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(rows
            .into_iter()
            .map(|row| AauthAgent {
                local: row.get(0),
                agent: row.get(1),
                jkt: row.get(2),
                label: row.get(3),
                status: row.get(4),
                created_unix: row.get(5),
            })
            .collect())
    }

    async fn revoke_aauth_agent(&self, local: &str) -> Result<bool, StoreError> {
        let n = self
            .client()
            .await?
            .execute(
                "UPDATE identity_aauth_agents SET status = 'revoked' WHERE local = $1",
                &[&local],
            )
            .await
            .map_err(|e| StoreError::Pg(format!("{e}")))?;
        Ok(n > 0)
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
