// SPDX-License-Identifier: BUSL-1.1
//! The durable, HA-capable Postgres claim store (agentctl RFC 0011 §3.2 / §10).
//!
//! [`PgClaimStore`] implements the same [`ClaimStore`] trait as the in-memory
//! store, with semantically identical behaviour — but the serializing point is no
//! longer a single in-process `Mutex` (a SPOF): it is a row in shared Postgres, so
//! the grant-one invariant holds across **>1 coordination replica** AND survives a
//! pod restart. Selected at startup by `COORDINATION_DATABASE_URL`/`DATABASE_URL`
//! (see `main.rs`); absent, the in-memory store stays the default.
//!
//! **The correctness invariant — grant-one is atomic across concurrent claimers
//! AND across replicas.** `claim` is a SINGLE conditional UPSERT
//! (`INSERT … ON CONFLICT (claim_key) DO UPDATE … WHERE …`). Postgres takes a
//! row-level lock on the conflicting row and re-evaluates the `WHERE` against the
//! latest committed version, so of N racers for the same `claim_key` **exactly
//! one** sees the predicate (`status='pending'` OR an expired `claimed`) hold and
//! is granted; the rest get no row back and read the live holder (contended) or
//! the acked tombstone (deduped). This is the same exactly-one-owner guarantee the
//! in-memory `Mutex` gives, now distributed.
//!
//! Wall-clock note: the in-memory store uses a monotonic `Instant` so a clock step
//! cannot resurrect a lease. Here lease expiry is a `TIMESTAMPTZ` compared to the
//! database server's `now()` — every replica defers to the SAME clock (the DB),
//! which is the property that actually matters for a shared serializing point.
//!
//! Threading: the [`ClaimStore`] trait is synchronous (so the MCP wire layer is
//! untouched), but tokio-postgres is async. We drive all DB I/O on a DEDICATED
//! multi-thread tokio runtime parked on its own OS thread, and bridge each sync
//! call by spawning the future there and blocking on its result. Because that
//! runtime is independent of the axum server runtime, blocking a server worker on
//! the result can never deadlock the DB future; on a multi-thread server worker we
//! additionally use `block_in_place` so the worker yields to other tasks while it
//! waits.

use std::future::Future;
use std::time::Duration;

use deadpool_postgres::{Manager, Pool};

use crate::store::{ClaimResult, ClaimStore, Stats, SubmitOutcome};

/// Pool size — mirrors the gateway/modelgateway stores.
const POOL_MAX_SIZE: usize = 8;
/// Worker threads on the dedicated DB runtime.
const DB_WORKER_THREADS: usize = 4;
/// Startup schema-readiness retries (the DB pod may start after us).
const SCHEMA_RETRIES: u32 = 30;
/// Delay between schema-readiness retries.
const SCHEMA_RETRY_DELAY: Duration = Duration::from_secs(2);

/// The atomic grant-one statement (the correctness invariant; see module docs).
///
/// `$1`=claim_key, `$2`=item, `$3`=holder, `$4`=ttl_ms. A returned `lease_id`
/// means GRANTED — `INSERT` (brand-new key, even if never submitted) or
/// `DO UPDATE` of a `pending`/expired-`claimed` row. No row means the conflicting
/// row is a live `claimed` (contended) or `acked` (deduped); the caller reads it.
/// The fresh, globally-unique, opaque `lease_id` is minted server-side from a
/// shared sequence (uniqueness) plus an `md5(random())` suffix (opacity).
const CLAIM_SQL: &str = "\
INSERT INTO work_items (claim_key, item, status, lease_id, holder, expires_at, created_at, updated_at)
VALUES (
    $1, $2, 'claimed',
    'lease-' || lpad(to_hex(nextval('work_items_lease_seq')), 8, '0') || '-' || substr(md5(random()::text || clock_timestamp()::text), 1, 16),
    $3,
    now() + ($4::bigint * interval '1 millisecond'),
    now(), now()
)
ON CONFLICT (claim_key) DO UPDATE SET
    item = EXCLUDED.item,
    status = 'claimed',
    lease_id = EXCLUDED.lease_id,
    holder = EXCLUDED.holder,
    expires_at = EXCLUDED.expires_at,
    updated_at = now()
WHERE work_items.status = 'pending'
   OR (work_items.status = 'claimed' AND work_items.expires_at < now())
RETURNING lease_id";

/// The persisted status of a `work_items` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowStatus {
    Pending,
    Claimed,
    Acked,
}

impl RowStatus {
    /// Parse the `status` text column; `None` for an unrecognised value.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "claimed" => Some(Self::Claimed),
            "acked" => Some(Self::Acked),
            _ => None,
        }
    }
}

/// PURE: map the conflicting row's state (read after a `claim` UPSERT granted
/// nothing) to the non-grant [`ClaimResult`]. An `acked` key is the dedupe
/// tombstone (never re-granted, `held_by:"<acked>"` on the wire); a live `claimed`
/// is contention (report the holder). A leftover `pending` (only reachable via a
/// concurrent transaction between the UPSERT and the read-back) is reported as
/// contended-with-unknown-holder — a benign "lost", never a false grant.
fn not_granted_result(status: RowStatus, holder: Option<String>) -> ClaimResult {
    match status {
        RowStatus::Acked => ClaimResult::Deduped,
        RowStatus::Claimed | RowStatus::Pending => ClaimResult::Contended { held_by: holder },
    }
}

/// PURE: map the conflicting row's status (read after a `submit` INSERT was a
/// no-op `ON CONFLICT DO NOTHING`) to the [`SubmitOutcome`]. Mirrors the in-memory
/// producer-side dedupe: an `acked` key is deduped; a `claimed` key is already
/// held; a `pending` key is already enqueued. (An expired `claimed` row reports
/// `AlreadyClaimed` here; the sweeper / next `claim` reclaims it.)
fn submit_conflict_outcome(status: RowStatus) -> SubmitOutcome {
    match status {
        RowStatus::Acked => SubmitOutcome::Deduped,
        RowStatus::Claimed => SubmitOutcome::AlreadyClaimed,
        RowStatus::Pending => SubmitOutcome::AlreadyPending,
    }
}

/// PURE: the `ack` idempotency decision, mirroring the in-memory store. A row was
/// settled (`updated`) ⇒ Ok. Otherwise the lease is gone: Ok only if this
/// `claim_key` is already an acked tombstone (idempotent re-ack, agentd RFC 0019
/// §3.5); else an unknown-lease error. We never fabricate done-state from a bare
/// ack of an unknown lease (that would let a stray call dedupe-block a future
/// claim).
fn ack_result(updated: bool, key_already_acked: bool, lease_id: &str) -> Result<(), String> {
    if updated || key_already_acked {
        Ok(())
    } else {
        Err(format!("unknown lease_id: {lease_id}"))
    }
}

/// The durable claim store backed by shared Postgres.
pub struct PgClaimStore {
    pool: Pool,
    /// Handle to the dedicated DB runtime (parked on its own OS thread).
    rt: tokio::runtime::Handle,
}

impl PgClaimStore {
    /// Build the pool, spawn the dedicated DB runtime, and run the schema
    /// migration (with startup retry — the DB pod may come up after us). Returns
    /// an `Err` only if the schema cannot be ensured after [`SCHEMA_RETRIES`].
    pub fn connect(database_url: &str) -> Result<Self, String> {
        let pool = build_pool(database_url)?;
        let rt = spawn_db_runtime();
        let store = Self { pool, rt };
        store.block(ensure_schema_with_retry(store.pool.clone()))?;
        Ok(store)
    }

    /// Drive `fut` to completion on the dedicated DB runtime and return its output,
    /// blocking the caller. Spawning on the dedicated runtime (not the caller's)
    /// means the future makes progress regardless of what the caller's runtime is
    /// doing, so blocking here cannot deadlock. On a multi-thread server worker we
    /// use `block_in_place` so the worker can run other tasks while it waits.
    fn block<F, T>(&self, fut: F) -> T
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<T>(1);
        self.rt.spawn(async move {
            // The receiver is only dropped if `block` is unwound (it is not), so a
            // send error is unreachable in practice; ignore it.
            let _ = tx.send(fut.await);
        });
        if on_multi_thread_runtime() {
            tokio::task::block_in_place(|| rx.recv().expect("pg db task dropped before sending"))
        } else {
            rx.recv().expect("pg db task dropped before sending")
        }
    }
}

impl std::fmt::Debug for PgClaimStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgClaimStore")
            .field("pool", &"<deadpool>")
            .finish()
    }
}

impl ClaimStore for PgClaimStore {
    fn submit(&self, item: &str, claim_key: Option<&str>) -> SubmitOutcome {
        // No claim_key ⇒ the item URI is its own identity key (the table is keyed
        // by claim_key; the item is the natural dedupe identity when none given).
        let key = claim_key.unwrap_or(item).to_string();
        let item = item.to_string();
        let pool = self.pool.clone();
        match self.block(async move { db_submit(&pool, &key, &item).await }) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(error = %e, "pg submit failed");
                // No error channel on the trait; report a non-enqueue so the
                // producer never sees a false success.
                SubmitOutcome::AlreadyPending
            }
        }
    }

    fn claim(&self, item: &str, ttl_ms: u64, claim_key: &str, holder: &str) -> ClaimResult {
        let key = claim_key.to_string();
        let item = item.to_string();
        let holder = holder.to_string();
        let pool = self.pool.clone();
        match self.block(async move { db_claim(&pool, &key, &item, ttl_ms, &holder).await }) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "pg claim failed");
                // Fail CLOSED: deny the grant. Contended-with-unknown-holder is the
                // safe outcome — the claimer treats it as Lost and retries. Never
                // return Granted on error (that would risk two owners).
                ClaimResult::Contended { held_by: None }
            }
        }
    }

    fn renew(&self, lease_id: &str, ttl_ms: u64) -> Result<(), String> {
        let lease_id = lease_id.to_string();
        let pool = self.pool.clone();
        self.block(async move { db_renew(&pool, &lease_id, ttl_ms).await })
    }

    fn ack(&self, lease_id: &str, claim_key: &str) -> Result<(), String> {
        let lease_id = lease_id.to_string();
        let claim_key = claim_key.to_string();
        let pool = self.pool.clone();
        self.block(async move { db_ack(&pool, &lease_id, &claim_key).await })
    }

    fn release(&self, lease_id: &str, _reason: &str) -> Result<(), String> {
        let lease_id = lease_id.to_string();
        let pool = self.pool.clone();
        self.block(async move { db_release(&pool, &lease_id).await })
    }

    fn sweep_expired(&self) -> usize {
        let pool = self.pool.clone();
        match self.block(async move { db_sweep(&pool).await }) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "pg sweep failed");
                0
            }
        }
    }

    fn stats(&self) -> Stats {
        let pool = self.pool.clone();
        match self.block(async move { db_stats(&pool).await }) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "pg stats failed");
                Stats {
                    pending: 0,
                    claimed: 0,
                    oldest_age_ms: 0,
                }
            }
        }
    }

    fn pending_items(&self) -> Vec<String> {
        let pool = self.pool.clone();
        match self.block(async move { db_pending_items(&pool).await }) {
            Ok(items) => items,
            Err(e) => {
                tracing::error!(error = %e, "pg pending_items failed");
                Vec::new()
            }
        }
    }
}

/// True when the current thread is a worker of a multi-thread tokio runtime — the
/// only context where `block_in_place` is valid (it panics on a current-thread
/// runtime). The coordination server runs on `#[tokio::main]` (multi-thread), so
/// in production every store call lands here.
fn on_multi_thread_runtime() -> bool {
    matches!(
        tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread)
    )
}

/// Spawn the dedicated multi-thread DB runtime on its own OS thread and return its
/// [`Handle`]. The thread owns the [`Runtime`] and parks forever, so the runtime
/// is never dropped inside an async context (which would panic) and lives for the
/// whole process. Only the cloneable, drop-safe `Handle` is handed back.
///
/// [`Handle`]: tokio::runtime::Handle
/// [`Runtime`]: tokio::runtime::Runtime
fn spawn_db_runtime() -> tokio::runtime::Handle {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("coordination-pg".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(DB_WORKER_THREADS)
                .enable_all()
                .thread_name("coordination-pg-worker")
                .build()
                .expect("build coordination postgres runtime");
            tx.send(rt.handle().clone())
                .expect("send coordination postgres runtime handle");
            rt.block_on(std::future::pending::<()>());
        })
        .expect("spawn coordination postgres runtime thread");
    rx.recv()
        .expect("receive coordination postgres runtime handle")
}

/// Build the deadpool pool from the DSN. `sslmode=disable` → [`tokio_postgres::NoTls`]
/// (plain in-cluster hop); any other mode → the rustls/ring connector in
/// [`crate::db_tls`] (encrypt-without-verify, like the gateway). Both pure Rust.
fn build_pool(database_url: &str) -> Result<Pool, String> {
    let cfg: tokio_postgres::Config = database_url
        .parse()
        .map_err(|e| format!("parse coordination database url: {e}"))?;
    let mgr = if cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
        Manager::new(cfg, tokio_postgres::NoTls)
    } else {
        Manager::new(cfg, crate::db_tls::make_connector())
    };
    Pool::builder(mgr)
        .max_size(POOL_MAX_SIZE)
        .build()
        .map_err(|e| format!("build coordination postgres pool: {e}"))
}

/// Create the schema if missing (idempotent; safe to run from every replica). The
/// `work_items` table is keyed by `claim_key` (the grant/dedupe identity); the
/// partial unique index on `lease_id` makes renew/ack/release lookups fast and
/// keeps lease ids unique; the shared sequence mints lease ids.
async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS work_items (
                claim_key  text        PRIMARY KEY,
                item       text        NOT NULL,
                status     text        NOT NULL DEFAULT 'pending',
                lease_id   text,
                holder     text,
                expires_at timestamptz,
                created_at timestamptz NOT NULL DEFAULT now(),
                updated_at timestamptz NOT NULL DEFAULT now()
            );
            CREATE SEQUENCE IF NOT EXISTS work_items_lease_seq;
            CREATE INDEX IF NOT EXISTS work_items_status_idx ON work_items (status);
            CREATE UNIQUE INDEX IF NOT EXISTS work_items_lease_id_idx
                ON work_items (lease_id) WHERE lease_id IS NOT NULL;",
        )
        .await
        .map_err(|e| e.to_string())
}

/// Run [`ensure_schema`] with startup retry (the DB pod may start after us).
async fn ensure_schema_with_retry(pool: Pool) -> Result<(), String> {
    for attempt in 1..=SCHEMA_RETRIES {
        match ensure_schema(&pool).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt == SCHEMA_RETRIES => {
                return Err(format!("postgres schema after {SCHEMA_RETRIES} tries: {e}"));
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "coordination: waiting for postgres…");
                tokio::time::sleep(SCHEMA_RETRY_DELAY).await;
            }
        }
    }
    Ok(())
}

/// `work.submit` → `INSERT … ON CONFLICT (claim_key) DO NOTHING` (dedupe). A
/// returned row means newly enqueued; a conflict is classified from the existing
/// row's status by [`submit_conflict_outcome`].
async fn db_submit(pool: &Pool, key: &str, item: &str) -> Result<SubmitOutcome, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let inserted = client
        .query_opt(
            "INSERT INTO work_items (claim_key, item, status, created_at, updated_at)
             VALUES ($1, $2, 'pending', now(), now())
             ON CONFLICT (claim_key) DO NOTHING
             RETURNING claim_key",
            &[&key, &item],
        )
        .await
        .map_err(|e| e.to_string())?;
    if inserted.is_some() {
        return Ok(SubmitOutcome::Enqueued);
    }
    // Conflict: the key already exists (rows are never deleted, so it is still
    // there). Classify by its current status.
    let row = client
        .query_one(
            "SELECT status FROM work_items WHERE claim_key = $1",
            &[&key],
        )
        .await
        .map_err(|e| e.to_string())?;
    let status = RowStatus::parse(row.get::<_, &str>(0))
        .ok_or_else(|| "work_items.status unrecognised".to_string())?;
    Ok(submit_conflict_outcome(status))
}

/// `work.claim` → the atomic grant-one UPSERT ([`CLAIM_SQL`]); on no grant, read
/// the conflicting row to report contended/deduped via [`not_granted_result`].
async fn db_claim(
    pool: &Pool,
    key: &str,
    item: &str,
    ttl_ms: u64,
    holder: &str,
) -> Result<ClaimResult, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let ttl = i64::try_from(ttl_ms).unwrap_or(i64::MAX);
    let granted = client
        .query_opt(CLAIM_SQL, &[&key, &item, &holder, &ttl])
        .await
        .map_err(|e| e.to_string())?;
    if let Some(r) = granted {
        let lease_id: String = r.get(0);
        return Ok(ClaimResult::Granted {
            lease_id,
            expires_in_ms: ttl_ms,
        });
    }
    let row = client
        .query_opt(
            "SELECT status, holder FROM work_items WHERE claim_key = $1",
            &[&key],
        )
        .await
        .map_err(|e| e.to_string())?;
    match row {
        Some(r) => {
            let status = RowStatus::parse(r.get::<_, &str>(0))
                .ok_or_else(|| "work_items.status unrecognised".to_string())?;
            let holder: Option<String> = r.get(1);
            Ok(not_granted_result(status, holder))
        }
        // The row vanished between the UPSERT and the read-back (rows are never
        // deleted, so this is unreachable); fail closed as contended-unknown.
        None => Ok(ClaimResult::Contended { held_by: None }),
    }
}

/// `work.renew` → extend a LIVE, owned lease. Never resurrects an expired lease
/// (the `expires_at > now()` guard). 0 rows ⇒ unknown/expired ⇒ Err.
async fn db_renew(pool: &Pool, lease_id: &str, ttl_ms: u64) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let ttl = i64::try_from(ttl_ms).unwrap_or(i64::MAX);
    let n = client
        .execute(
            "UPDATE work_items
                SET expires_at = now() + ($2::bigint * interval '1 millisecond'),
                    updated_at = now()
              WHERE lease_id = $1 AND status = 'claimed' AND expires_at > now()",
            &[&lease_id, &ttl],
        )
        .await
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err(format!("unknown or expired lease_id: {lease_id}"));
    }
    Ok(())
}

/// `work.ack` → settle the lease and record its `claim_key` as the acked tombstone
/// (status='acked'); idempotent via [`ack_result`].
async fn db_ack(pool: &Pool, lease_id: &str, claim_key: &str) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let updated = client
        .execute(
            "UPDATE work_items
                SET status = 'acked', lease_id = NULL, holder = NULL,
                    expires_at = NULL, updated_at = now()
              WHERE lease_id = $1",
            &[&lease_id],
        )
        .await
        .map_err(|e| e.to_string())?;
    let key_already_acked = if updated == 0 {
        client
            .query_opt(
                "SELECT 1 FROM work_items WHERE claim_key = $1 AND status = 'acked'",
                &[&claim_key],
            )
            .await
            .map_err(|e| e.to_string())?
            .is_some()
    } else {
        false
    };
    ack_result(updated > 0, key_already_acked, lease_id)
}

/// `work.release` → return a held item to `pending` (re-claimable; does NOT record
/// a tombstone). 0 rows ⇒ unknown lease ⇒ Err.
async fn db_release(pool: &Pool, lease_id: &str) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let n = client
        .execute(
            "UPDATE work_items
                SET status = 'pending', lease_id = NULL, holder = NULL,
                    expires_at = NULL, updated_at = now()
              WHERE lease_id = $1",
            &[&lease_id],
        )
        .await
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err(format!("unknown lease_id: {lease_id}"));
    }
    Ok(())
}

/// Move every expired `claimed` row back to `pending` (re-offer a dead claimer's
/// item). Lazy reclaim on `claim` already covers correctness between sweeps; this
/// keeps `stats`/`pending_items` truthful. Idempotent across concurrent replicas.
async fn db_sweep(pool: &Pool) -> Result<usize, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let n = client
        .execute(
            "UPDATE work_items
                SET status = 'pending', lease_id = NULL, holder = NULL,
                    expires_at = NULL, updated_at = now()
              WHERE status = 'claimed' AND expires_at <= now()",
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// `work.stats` → the P9 backlog snapshot: pending count, LIVE-claimed count, and
/// the age of the oldest pending item (a pending row's `updated_at` is the instant
/// it entered `pending`, so `now() - min(updated_at)` is its age).
async fn db_stats(pool: &Pool) -> Result<Stats, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_one(
            "SELECT
                count(*) FILTER (WHERE status = 'pending') AS pending,
                count(*) FILTER (WHERE status = 'claimed' AND expires_at > now()) AS claimed,
                COALESCE(
                    (EXTRACT(EPOCH FROM (now() - min(updated_at) FILTER (WHERE status = 'pending'))) * 1000)::bigint,
                    0
                ) AS oldest_age_ms
             FROM work_items",
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    let pending: i64 = row.get("pending");
    let claimed: i64 = row.get("claimed");
    let oldest: i64 = row.get("oldest_age_ms");
    Ok(Stats {
        pending: usize::try_from(pending).unwrap_or(0),
        claimed: usize::try_from(claimed).unwrap_or(0),
        oldest_age_ms: u64::try_from(oldest).unwrap_or(0),
    })
}

/// The current pending items (for the `work://pending` resource), oldest first.
async fn db_pending_items(pool: &Pool) -> Result<Vec<String>, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let rows = client
        .query(
            "SELECT item FROM work_items WHERE status = 'pending' ORDER BY updated_at ASC",
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pure, SQL-independent decision logic — exercised WITHOUT a database (the
    // live Postgres path is integration-verified on a cluster).

    #[test]
    fn row_status_parses_known_and_rejects_unknown() {
        assert_eq!(RowStatus::parse("pending"), Some(RowStatus::Pending));
        assert_eq!(RowStatus::parse("claimed"), Some(RowStatus::Claimed));
        assert_eq!(RowStatus::parse("acked"), Some(RowStatus::Acked));
        assert_eq!(RowStatus::parse("bogus"), None);
        assert_eq!(RowStatus::parse(""), None);
    }

    #[test]
    fn not_granted_acked_key_dedupes() {
        // An acked tombstone is never re-granted ⇒ Deduped (wire: held_by "<acked>").
        assert_eq!(
            not_granted_result(RowStatus::Acked, None),
            ClaimResult::Deduped
        );
        // Even if a holder column lingered, acked still dedupes.
        assert_eq!(
            not_granted_result(RowStatus::Acked, Some("h".into())),
            ClaimResult::Deduped
        );
    }

    #[test]
    fn not_granted_live_claim_is_contended_with_holder() {
        assert_eq!(
            not_granted_result(RowStatus::Claimed, Some("pod-7".into())),
            ClaimResult::Contended {
                held_by: Some("pod-7".into())
            }
        );
        // A leftover pending (concurrent-txn race) ⇒ contended-unknown, never a grant.
        assert_eq!(
            not_granted_result(RowStatus::Pending, None),
            ClaimResult::Contended { held_by: None }
        );
    }

    #[test]
    fn submit_conflict_maps_each_status() {
        assert_eq!(
            submit_conflict_outcome(RowStatus::Acked),
            SubmitOutcome::Deduped
        );
        assert_eq!(
            submit_conflict_outcome(RowStatus::Claimed),
            SubmitOutcome::AlreadyClaimed
        );
        assert_eq!(
            submit_conflict_outcome(RowStatus::Pending),
            SubmitOutcome::AlreadyPending
        );
    }

    #[test]
    fn ack_is_ok_when_updated_or_already_acked_else_err() {
        // A row was settled ⇒ Ok.
        assert!(ack_result(true, false, "lease-x").is_ok());
        // Lease gone but the key is already an acked tombstone ⇒ idempotent Ok.
        assert!(ack_result(false, true, "lease-x").is_ok());
        // Lease gone and key not done ⇒ unknown-lease error (no fabricated state).
        let err = ack_result(false, false, "lease-x").unwrap_err();
        assert!(err.contains("lease-x"), "error names the lease: {err}");
    }
}
