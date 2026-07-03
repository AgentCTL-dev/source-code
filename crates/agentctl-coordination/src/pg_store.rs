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

use crate::store::{
    holder_mismatch_error, ClaimResult, ClaimStore, DeadItem, Stats, SubmitOutcome, WorkState,
    WorkStatus,
};

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
/// `$1`=claim_key, `$2`=item, `$3`=holder, `$4`=ttl_ms. A GRANT increments
/// `attempts` (this delivery). The reclaim clause requires an expired `claimed`
/// row to still be UNDER its `max_attempts` budget — a poison row past budget is
/// left for the sweeper (or the read-back) to dead-letter, never re-granted.
const CLAIM_SQL: &str = "\
INSERT INTO work_items (claim_key, item, status, lease_id, holder, expires_at, attempts, created_at, updated_at)
VALUES (
    $1, $2, 'claimed',
    'lease-' || lpad(to_hex(nextval('work_items_lease_seq')), 8, '0') || '-' || substr(md5(random()::text || clock_timestamp()::text), 1, 16),
    $3,
    now() + ($4::bigint * interval '1 millisecond'),
    1,
    now(), now()
)
ON CONFLICT (claim_key) DO UPDATE SET
    item = EXCLUDED.item,
    status = 'claimed',
    lease_id = EXCLUDED.lease_id,
    holder = EXCLUDED.holder,
    expires_at = EXCLUDED.expires_at,
    attempts = work_items.attempts + 1,
    updated_at = now()
WHERE work_items.status = 'pending'
   OR (work_items.status = 'claimed' AND work_items.expires_at < now()
       AND (work_items.max_attempts IS NULL OR work_items.attempts < work_items.max_attempts))
RETURNING lease_id";

/// The status → `pending`/`deadletter` transition for a redelivery (sweep / release):
/// a row past its `max_attempts` budget is dead-lettered, else returned to pending.
/// Shared by `db_sweep` and `db_release` so both agree with the in-memory store.
const REDELIVER_STATUS: &str =
    "CASE WHEN work_items.max_attempts IS NOT NULL AND work_items.attempts >= work_items.max_attempts \
     THEN 'deadletter' ELSE 'pending' END";

/// The persisted status of a `work_items` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowStatus {
    Pending,
    Claimed,
    Acked,
    Deadletter,
}

impl RowStatus {
    /// Parse the `status` text column; `None` for an unrecognised value.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "claimed" => Some(Self::Claimed),
            "acked" => Some(Self::Acked),
            "deadletter" => Some(Self::Deadletter),
            _ => None,
        }
    }
}

/// PURE: map the conflicting row's state (read after a `claim` UPSERT granted
/// nothing) to the non-grant [`ClaimResult`]. An `acked` key is the dedupe
/// tombstone (never re-granted, `held_by:"<acked>"`); a `deadletter` row — or an
/// expired `claimed` row already past its `max_attempts` budget (which the reclaim
/// clause deliberately left ungranted) — is [`ClaimResult::Deadlettered`]; any other
/// live/under-budget `claimed` is contention. A leftover `pending` (a concurrent-txn
/// race) is contended-with-unknown-holder — a benign "lost", never a false grant.
fn not_granted_result(
    status: RowStatus,
    holder: Option<String>,
    expired: bool,
    past_budget: bool,
) -> ClaimResult {
    match status {
        RowStatus::Acked => ClaimResult::Deduped,
        RowStatus::Deadletter => ClaimResult::Deadlettered,
        RowStatus::Claimed if expired && past_budget => ClaimResult::Deadlettered,
        RowStatus::Claimed | RowStatus::Pending => ClaimResult::Contended { held_by: holder },
    }
}

/// PURE: map the conflicting row's status (read after a `submit` INSERT was a
/// no-op `ON CONFLICT DO NOTHING`) to the [`SubmitOutcome`]. Mirrors the in-memory
/// producer-side dedupe: an `acked` key is deduped; a `deadletter` key is held out;
/// a `claimed` key is already held; a `pending` key is already enqueued. (An expired
/// `claimed` row reports `AlreadyClaimed` here; the sweeper / next `claim` reclaims it.)
fn submit_conflict_outcome(status: RowStatus) -> SubmitOutcome {
    match status {
        RowStatus::Acked => SubmitOutcome::Deduped,
        RowStatus::Deadletter => SubmitOutcome::Deadlettered,
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
    fn submit(
        &self,
        item: &str,
        claim_key: Option<&str>,
        max_attempts: Option<u32>,
    ) -> SubmitOutcome {
        // No claim_key ⇒ the item URI is its own identity key (the table is keyed
        // by claim_key; the item is the natural dedupe identity when none given).
        let key = claim_key.unwrap_or(item).to_string();
        let item = item.to_string();
        let max = max_attempts.map(|m| m as i32);
        let pool = self.pool.clone();
        match self.block(async move { db_submit(&pool, &key, &item, max).await }) {
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

    fn renew(
        &self,
        lease_id: &str,
        ttl_ms: u64,
        expected_holder: Option<&str>,
    ) -> Result<(), String> {
        let lease_id = lease_id.to_string();
        let expected = expected_holder.map(str::to_string);
        let pool = self.pool.clone();
        self.block(async move { db_renew(&pool, &lease_id, ttl_ms, expected.as_deref()).await })
    }

    fn ack(
        &self,
        lease_id: &str,
        claim_key: &str,
        expected_holder: Option<&str>,
        result: Option<&str>,
    ) -> Result<(), String> {
        let lease_id = lease_id.to_string();
        let claim_key = claim_key.to_string();
        let expected = expected_holder.map(str::to_string);
        let result = result.map(str::to_string);
        let pool = self.pool.clone();
        self.block(async move {
            db_ack(&pool, &lease_id, &claim_key, expected.as_deref(), result.as_deref()).await
        })
    }

    fn release(
        &self,
        lease_id: &str,
        _reason: &str,
        expected_holder: Option<&str>,
    ) -> Result<(), String> {
        let lease_id = lease_id.to_string();
        let expected = expected_holder.map(str::to_string);
        let pool = self.pool.clone();
        self.block(async move { db_release(&pool, &lease_id, expected.as_deref()).await })
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
                    deadletter: 0,
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

    fn result_of(&self, claim_key: &str) -> WorkStatus {
        let key = claim_key.to_string();
        let pool = self.pool.clone();
        match self.block(async move { db_result_of(&pool, &key).await }) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "pg result_of failed");
                // Fail safe: report Unknown rather than fabricate a state.
                WorkStatus {
                    state: WorkState::Unknown,
                    result: None,
                }
            }
        }
    }

    fn dead_items(&self) -> Vec<DeadItem> {
        let pool = self.pool.clone();
        match self.block(async move { db_dead_items(&pool).await }) {
            Ok(items) => items,
            Err(e) => {
                tracing::error!(error = %e, "pg dead_items failed");
                Vec::new()
            }
        }
    }

    fn requeue_dead(&self, claim_key: &str) -> bool {
        let key = claim_key.to_string();
        let pool = self.pool.clone();
        self.block(async move { db_requeue_dead(&pool, &key).await })
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg requeue_dead failed");
                false
            })
    }

    fn drop_dead(&self, claim_key: &str) -> bool {
        let key = claim_key.to_string();
        let pool = self.pool.clone();
        self.block(async move { db_drop_dead(&pool, &key).await })
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg drop_dead failed");
                false
            })
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
                claim_key    text        PRIMARY KEY,
                item         text        NOT NULL,
                status       text        NOT NULL DEFAULT 'pending',
                lease_id     text,
                holder       text,
                expires_at   timestamptz,
                created_at   timestamptz NOT NULL DEFAULT now(),
                updated_at   timestamptz NOT NULL DEFAULT now()
            );
            CREATE SEQUENCE IF NOT EXISTS work_items_lease_seq;
            CREATE INDEX IF NOT EXISTS work_items_status_idx ON work_items (status);
            CREATE UNIQUE INDEX IF NOT EXISTS work_items_lease_id_idx
                ON work_items (lease_id) WHERE lease_id IS NOT NULL;
            -- RFC 0022 §7: redelivery accounting + the ack result. Additive, so an
            -- existing table upgrades in place.
            ALTER TABLE work_items ADD COLUMN IF NOT EXISTS attempts     int NOT NULL DEFAULT 0;
            ALTER TABLE work_items ADD COLUMN IF NOT EXISTS max_attempts int;
            ALTER TABLE work_items ADD COLUMN IF NOT EXISTS result       text;",
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
async fn db_submit(
    pool: &Pool,
    key: &str,
    item: &str,
    max_attempts: Option<i32>,
) -> Result<SubmitOutcome, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let inserted = client
        .query_opt(
            "INSERT INTO work_items (claim_key, item, status, attempts, max_attempts, created_at, updated_at)
             VALUES ($1, $2, 'pending', 0, $3, now(), now())
             ON CONFLICT (claim_key) DO NOTHING
             RETURNING claim_key",
            &[&key, &item, &max_attempts],
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
            "SELECT status, holder, (expires_at IS NOT NULL AND expires_at < now()) AS expired,
                    (max_attempts IS NOT NULL AND attempts >= max_attempts) AS past_budget
               FROM work_items WHERE claim_key = $1",
            &[&key],
        )
        .await
        .map_err(|e| e.to_string())?;
    match row {
        Some(r) => {
            let status = RowStatus::parse(r.get::<_, &str>(0))
                .ok_or_else(|| "work_items.status unrecognised".to_string())?;
            let holder: Option<String> = r.get(1);
            let expired: bool = r.get(2);
            let past_budget: bool = r.get(3);
            Ok(not_granted_result(status, holder, expired, past_budget))
        }
        // The row vanished between the UPSERT and the read-back (rows are never
        // deleted, so this is unreachable); fail closed as contended-unknown.
        None => Ok(ClaimResult::Contended { held_by: None }),
    }
}

/// `work.renew` → extend a LIVE, owned lease. Never resurrects an expired lease
/// (the `expires_at > now()` guard). The `($3::text IS NULL OR holder = $3)`
/// predicate is the attested-identity gate (RFC 0015): `NULL` ⇒ unconstrained
/// (attest off); otherwise the UPDATE matches ONLY when the recorded holder equals
/// the attested caller — atomic, so a tenant cannot renew another's lease. 0 rows
/// ⇒ unknown/expired, or (when gated) a holder mismatch — distinguished by a
/// read-back so the wire layer can report a 403-style reject.
async fn db_renew(
    pool: &Pool,
    lease_id: &str,
    ttl_ms: u64,
    expected_holder: Option<&str>,
) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let ttl = i64::try_from(ttl_ms).unwrap_or(i64::MAX);
    let n = client
        .execute(
            "UPDATE work_items
                SET expires_at = now() + ($2::bigint * interval '1 millisecond'),
                    updated_at = now()
              WHERE lease_id = $1 AND status = 'claimed' AND expires_at > now()
                AND ($3::text IS NULL OR holder = $3)",
            &[&lease_id, &ttl, &expected_holder],
        )
        .await
        .map_err(|e| e.to_string())?;
    if n > 0 {
        return Ok(());
    }
    // Gated + a LIVE lease with this id still exists ⇒ the predicate rejected a
    // wrong holder (mismatch), not an unknown/expired lease.
    if expected_holder.is_some() && lease_is_live(&client, lease_id).await? {
        return Err(holder_mismatch_error(lease_id));
    }
    Err(format!("unknown or expired lease_id: {lease_id}"))
}

/// `work.ack` → settle the lease and record its `claim_key` as the acked tombstone
/// (status='acked'); idempotent via [`ack_result`]. The
/// `($3::text IS NULL OR holder = $3)` predicate is the attested-identity gate: a
/// tenant may settle ONLY its own lease. Idempotent re-ack (key already acked) is
/// honoured BEFORE the mismatch check, so a redelivered ack of an already-settled
/// item is a no-op regardless of who sends it.
async fn db_ack(
    pool: &Pool,
    lease_id: &str,
    claim_key: &str,
    expected_holder: Option<&str>,
    result: Option<&str>,
) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let updated = client
        .execute(
            "UPDATE work_items
                SET status = 'acked', lease_id = NULL, holder = NULL,
                    expires_at = NULL, result = $3, updated_at = now()
              WHERE lease_id = $1 AND ($2::text IS NULL OR holder = $2)",
            &[&lease_id, &expected_holder, &result],
        )
        .await
        .map_err(|e| e.to_string())?;
    if updated > 0 {
        return Ok(());
    }
    // Not settled: idempotent re-ack (the key is already an acked tombstone) wins
    // first — harmless no-op for any caller. A late `result` on the re-ack is
    // recorded only when none was stored (mirrors the in-memory store).
    let key_already_acked = client
        .query_opt(
            "SELECT 1 FROM work_items WHERE claim_key = $1 AND status = 'acked'",
            &[&claim_key],
        )
        .await
        .map_err(|e| e.to_string())?
        .is_some();
    if key_already_acked {
        if result.is_some() {
            client
                .execute(
                    "UPDATE work_items SET result = $2, updated_at = now()
                      WHERE claim_key = $1 AND status = 'acked' AND result IS NULL",
                    &[&claim_key, &result],
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    // Gated + a LIVE lease with this id still exists ⇒ a wrong-holder mismatch.
    if expected_holder.is_some() && lease_is_live(&client, lease_id).await? {
        return Err(holder_mismatch_error(lease_id));
    }
    ack_result(false, false, lease_id)
}

/// `work.release` → return a held item to `pending` (re-claimable; does NOT record
/// a tombstone). The `($2::text IS NULL OR holder = $2)` predicate is the
/// attested-identity gate: a tenant may release ONLY its own lease. 0 rows ⇒
/// unknown lease, or (when gated) a holder mismatch.
async fn db_release(
    pool: &Pool,
    lease_id: &str,
    expected_holder: Option<&str>,
) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let sql = format!(
        "UPDATE work_items
            SET status = {REDELIVER_STATUS}, lease_id = NULL, holder = NULL,
                expires_at = NULL, updated_at = now()
          WHERE lease_id = $1 AND ($2::text IS NULL OR holder = $2)"
    );
    let n = client
        .execute(sql.as_str(), &[&lease_id, &expected_holder])
        .await
        .map_err(|e| e.to_string())?;
    if n > 0 {
        return Ok(());
    }
    if expected_holder.is_some() && lease_is_live(&client, lease_id).await? {
        return Err(holder_mismatch_error(lease_id));
    }
    Err(format!("unknown lease_id: {lease_id}"))
}

/// Whether a row with this `lease_id` is still a LIVE claimed lease (a `lease_id`
/// is non-NULL only while claimed; `ack`/`release`/`sweep` clear it). Used ONLY to
/// classify a gated lifecycle op that matched 0 rows: a still-live lease means the
/// holder predicate rejected a wrong holder (mismatch) rather than the lease being
/// unknown/expired. This read does not affect the mutation's atomicity — the wrong
/// holder has already failed to mutate.
async fn lease_is_live(client: &deadpool_postgres::Client, lease_id: &str) -> Result<bool, String> {
    Ok(client
        .query_opt(
            "SELECT 1 FROM work_items
              WHERE lease_id = $1 AND status = 'claimed' AND expires_at > now()",
            &[&lease_id],
        )
        .await
        .map_err(|e| e.to_string())?
        .is_some())
}

/// Move every expired `claimed` row back to `pending` (re-offer a dead claimer's
/// item). Lazy reclaim on `claim` already covers correctness between sweeps; this
/// keeps `stats`/`pending_items` truthful. Idempotent across concurrent replicas.
async fn db_sweep(pool: &Pool) -> Result<usize, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    // Expired rows return to `pending`, EXCEPT those past their `max_attempts`
    // budget, which are dead-lettered (RFC 0022 §7) — the same decision the
    // in-memory `retire_or_requeue` makes.
    let sql = format!(
        "UPDATE work_items
            SET status = {REDELIVER_STATUS}, lease_id = NULL, holder = NULL,
                expires_at = NULL, updated_at = now()
          WHERE status = 'claimed' AND expires_at <= now()"
    );
    let n = client
        .execute(sql.as_str(), &[])
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
                count(*) FILTER (WHERE status = 'deadletter') AS deadletter,
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
    let deadletter: i64 = row.get("deadletter");
    let oldest: i64 = row.get("oldest_age_ms");
    Ok(Stats {
        pending: usize::try_from(pending).unwrap_or(0),
        claimed: usize::try_from(claimed).unwrap_or(0),
        oldest_age_ms: u64::try_from(oldest).unwrap_or(0),
        deadletter: usize::try_from(deadletter).unwrap_or(0),
    })
}

/// `work.result` → look up a unit's state (+ its acked `result`) by `claim_key`.
/// Mirrors the in-memory precedence: acked → deadletter → live-claimed →
/// (pending | expired-claimed) → unknown.
async fn db_result_of(pool: &Pool, key: &str) -> Result<WorkStatus, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_opt(
            "SELECT status, result, (expires_at IS NOT NULL AND expires_at > now()) AS live
               FROM work_items WHERE claim_key = $1",
            &[&key],
        )
        .await
        .map_err(|e| e.to_string())?;
    let Some(r) = row else {
        return Ok(WorkStatus {
            state: WorkState::Unknown,
            result: None,
        });
    };
    let status = RowStatus::parse(r.get::<_, &str>(0))
        .ok_or_else(|| "work_items.status unrecognised".to_string())?;
    let result: Option<String> = r.get(1);
    let live: bool = r.get(2);
    let state = match status {
        RowStatus::Acked => WorkState::Done,
        RowStatus::Deadletter => WorkState::Deadletter,
        // A live lease is Claimed; an expired-but-unswept lease is about to be
        // re-offered, so it reads as Pending (matches the in-memory store).
        RowStatus::Claimed if live => WorkState::Claimed,
        RowStatus::Claimed | RowStatus::Pending => WorkState::Pending,
    };
    Ok(WorkStatus {
        state,
        // Only a terminal ack carries a result.
        result: if state == WorkState::Done { result } else { None },
    })
}

/// The dead-lettered items (for `dlq://items` / `work.deadletter list`), by key.
async fn db_dead_items(pool: &Pool) -> Result<Vec<DeadItem>, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let rows = client
        .query(
            "SELECT claim_key, item, attempts FROM work_items
              WHERE status = 'deadletter' ORDER BY claim_key ASC",
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|r| DeadItem {
            claim_key: r.get::<_, String>(0),
            item: r.get::<_, String>(1),
            attempts: u32::try_from(r.get::<_, i32>(2)).unwrap_or(0),
        })
        .collect())
}

/// Re-offer a dead-lettered item to `pending` with a FRESH attempt budget
/// (`work.deadletter requeue`). Returns whether a DLQ row matched.
async fn db_requeue_dead(pool: &Pool, key: &str) -> Result<bool, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let n = client
        .execute(
            "UPDATE work_items
                SET status = 'pending', attempts = 0, lease_id = NULL, holder = NULL,
                    expires_at = NULL, updated_at = now()
              WHERE claim_key = $1 AND status = 'deadletter'",
            &[&key],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

/// Discard a dead-lettered item permanently, tombstoning it as `acked` so it is
/// never re-granted on a re-submit (`work.deadletter drop`). Returns whether a DLQ
/// row matched.
async fn db_drop_dead(pool: &Pool, key: &str) -> Result<bool, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let n = client
        .execute(
            "UPDATE work_items
                SET status = 'acked', result = NULL, lease_id = NULL, holder = NULL,
                    expires_at = NULL, updated_at = now()
              WHERE claim_key = $1 AND status = 'deadletter'",
            &[&key],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
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
        assert_eq!(RowStatus::parse("deadletter"), Some(RowStatus::Deadletter));
        assert_eq!(RowStatus::parse("bogus"), None);
        assert_eq!(RowStatus::parse(""), None);
    }

    #[test]
    fn not_granted_acked_key_dedupes() {
        // An acked tombstone is never re-granted ⇒ Deduped (wire: held_by "<acked>").
        assert_eq!(
            not_granted_result(RowStatus::Acked, None, false, false),
            ClaimResult::Deduped
        );
        // Even if a holder column lingered, acked still dedupes.
        assert_eq!(
            not_granted_result(RowStatus::Acked, Some("h".into()), false, false),
            ClaimResult::Deduped
        );
    }

    #[test]
    fn not_granted_live_claim_is_contended_with_holder() {
        assert_eq!(
            not_granted_result(RowStatus::Claimed, Some("pod-7".into()), false, false),
            ClaimResult::Contended {
                held_by: Some("pod-7".into())
            }
        );
        // A leftover pending (concurrent-txn race) ⇒ contended-unknown, never a grant.
        assert_eq!(
            not_granted_result(RowStatus::Pending, None, false, false),
            ClaimResult::Contended { held_by: None }
        );
    }

    #[test]
    fn not_granted_deadletter_and_expired_past_budget_are_deadlettered() {
        // A row already in the DLQ ⇒ Deadlettered.
        assert_eq!(
            not_granted_result(RowStatus::Deadletter, None, false, false),
            ClaimResult::Deadlettered
        );
        // An expired claimed row past its attempt budget (the reclaim clause left it
        // ungranted) ⇒ Deadlettered — the sweeper will formalize the DLQ move.
        assert_eq!(
            not_granted_result(RowStatus::Claimed, Some("pod-7".into()), true, true),
            ClaimResult::Deadlettered
        );
        // Expired but still UNDER budget ⇒ ordinary contention (will be reclaimed).
        assert_eq!(
            not_granted_result(RowStatus::Claimed, Some("pod-7".into()), true, false),
            ClaimResult::Contended {
                held_by: Some("pod-7".into())
            }
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
        assert_eq!(
            submit_conflict_outcome(RowStatus::Deadletter),
            SubmitOutcome::Deadlettered
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
