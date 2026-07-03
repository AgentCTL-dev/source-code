// SPDX-License-Identifier: BUSL-1.1
//! The durable token-usage meter (RFC 0012, the intelligence plane).
//!
//! Every `/v1/infer` call records the tokens it consumed against its
//! `(namespace, pool)` so the gateway can enforce the `ModelPool` budget
//! pre-request and report consumption. Backed by a shared Postgres so the
//! gateway stays a replicated, stateless front end — the meter lives in the
//! store, not the pod. Plain `NoTls` in-cluster (the hop is NetworkPolicy-scoped;
//! TLS to the DB is later hardening).
//!
//! **Budget enforcement is atomic (no check-then-act race).** A naive "read the
//! SUM, then charge after the call" lets N concurrent requests all pass the
//! pre-check at the same stale total and overshoot the cap without bound — the
//! common case is a whole fleet sharing one pool. Instead a request first
//! *reserves* a conservative upper-bound estimate against the budget under a
//! per-pool advisory lock ([`reserve`]): the reservation is admitted ONLY if
//! `committed + outstanding-reserved + estimate <= budget`, so concurrent
//! reservers serialize and the cap holds. After the provider responds the
//! reservation is *reconciled* to the true token count ([`commit_reservation`]) or
//! released on error ([`release_reservation`]). A crashed in-flight request leaks a
//! reservation row, but it is excluded from the budget after [`RESERVATION_TTL_SECS`]
//! (self-healing) and swept opportunistically — so a leak can never permanently
//! shrink a pool's budget.
//!
//! These ops need a live Postgres; the pure budget/usage helpers live in
//! `main.rs` and are unit-tested there.

use deadpool_postgres::Pool;

/// How long a reservation counts against the budget before it is treated as leaked
/// (a crashed gateway between reserve and reconcile) and excluded. Generous enough
/// to cover any real provider call; short enough that a leak self-heals promptly.
pub const RESERVATION_TTL_SECS: i64 = 300;

/// Create the usage + reservation tables if missing (idempotent; called with retry
/// at startup). `intelligence_usage` is the committed ledger (audit + report + the
/// committed-spend SUM); `intelligence_reservation` holds outstanding in-flight
/// reservations that also count against the budget until reconciled.
pub async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS intelligence_usage (
                namespace    text        NOT NULL,
                pool         text        NOT NULL,
                agent        text        NOT NULL,
                total_tokens bigint      NOT NULL,
                created_at   timestamptz NOT NULL DEFAULT now()
            );
            CREATE INDEX IF NOT EXISTS intelligence_usage_ns_pool_idx
                ON intelligence_usage (namespace, pool);
            CREATE TABLE IF NOT EXISTS intelligence_reservation (
                id           bigserial   PRIMARY KEY,
                namespace    text        NOT NULL,
                pool         text        NOT NULL,
                agent        text        NOT NULL,
                est_tokens   bigint      NOT NULL,
                created_at   timestamptz NOT NULL DEFAULT now()
            );
            CREATE INDEX IF NOT EXISTS intelligence_reservation_ns_pool_idx
                ON intelligence_reservation (namespace, pool);",
        )
        .await
        .map_err(|e| e.to_string())
}

/// Atomically reserve `est` tokens against the `(ns, pool)` budget. Returns
/// `Ok(Some(reservation_id))` when admitted, `Ok(None)` when the reservation would
/// exceed `budget` (the caller returns 429). This is the race-free replacement for
/// a bare pre-request SUM check.
///
/// Correctness: an `xact` advisory lock keyed by `(ns, pool)` serializes all
/// reservers for the pool, so the read of `committed + outstanding-reserved` and
/// the conditional insert are one atomic unit — concurrent reservers see each
/// other's committed reservations and cannot collectively exceed the budget.
/// Reservations older than [`RESERVATION_TTL_SECS`] are excluded (leaked by a
/// crashed request) and swept opportunistically inside the same transaction.
pub async fn reserve(
    pool_ref: &Pool,
    ns: &str,
    pool: &str,
    agent: &str,
    est: i64,
    budget: i64,
) -> Result<Option<i64>, String> {
    let mut client = pool_ref.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(|e| e.to_string())?;
    // Serialize this pool for the duration of the transaction. The lock auto-releases
    // on commit/rollback; a bigint key derived from (ns/pool) keeps pools independent.
    let lock_key = format!("{ns}/{pool}");
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        &[&lock_key],
    )
    .await
    .map_err(|e| e.to_string())?;
    // Opportunistic sweep of leaked reservations (bounded to this pool) so the table
    // and the reserved-sum stay truthful without a separate background task.
    tx.execute(
        "DELETE FROM intelligence_reservation
          WHERE namespace = $1 AND pool = $2
            AND created_at < now() - ($3::bigint * interval '1 second')",
        &[&ns, &pool, &RESERVATION_TTL_SECS],
    )
    .await
    .map_err(|e| e.to_string())?;
    // committed + still-outstanding reserved, under the lock (fresh snapshot).
    let committed: i64 = tx
        .query_one(
            "SELECT COALESCE(SUM(total_tokens), 0)::bigint FROM intelligence_usage
             WHERE namespace = $1 AND pool = $2",
            &[&ns, &pool],
        )
        .await
        .map_err(|e| e.to_string())?
        .get(0);
    let reserved: i64 = tx
        .query_one(
            "SELECT COALESCE(SUM(est_tokens), 0)::bigint FROM intelligence_reservation
             WHERE namespace = $1 AND pool = $2",
            &[&ns, &pool],
        )
        .await
        .map_err(|e| e.to_string())?
        .get(0);
    if committed.saturating_add(reserved).saturating_add(est) > budget {
        // Would exceed the cap — do NOT insert; let the lock release on rollback.
        tx.rollback().await.map_err(|e| e.to_string())?;
        return Ok(None);
    }
    let id: i64 = tx
        .query_one(
            "INSERT INTO intelligence_reservation (namespace, pool, agent, est_tokens)
             VALUES ($1, $2, $3, $4) RETURNING id",
            &[&ns, &pool, &agent, &est],
        )
        .await
        .map_err(|e| e.to_string())?
        .get(0);
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(Some(id))
}

/// Reconcile a reservation to the ACTUAL consumption: book `actual` tokens into the
/// committed ledger and drop the reservation, atomically. Called after the provider
/// responds. Because `actual <= est` for a well-formed estimate, the committed total
/// never exceeds the budget the reservation was admitted under.
pub async fn commit_reservation(
    pool_ref: &Pool,
    ns: &str,
    pool: &str,
    agent: &str,
    reservation_id: i64,
    actual: i64,
) -> Result<(), String> {
    let mut client = pool_ref.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO intelligence_usage (namespace, pool, agent, total_tokens)
         VALUES ($1, $2, $3, $4)",
        &[&ns, &pool, &agent, &actual],
    )
    .await
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM intelligence_reservation WHERE id = $1",
        &[&reservation_id],
    )
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// Release a reservation WITHOUT booking spend (the provider call failed / consumed
/// nothing). Frees the reserved headroom for other requests.
pub async fn release_reservation(pool_ref: &Pool, reservation_id: i64) -> Result<(), String> {
    let client = pool_ref.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            "DELETE FROM intelligence_reservation WHERE id = $1",
            &[&reservation_id],
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Record one inference's token consumption against `(ns, pool)`. `pool_ref` is
/// the Postgres connection pool; `pool` is the `ModelPool` name. Used on the
/// no-budget path (an uncapped pool needs no reservation, only the audit ledger).
pub async fn record_usage(
    pool_ref: &Pool,
    ns: &str,
    pool: &str,
    agent: &str,
    tokens: i64,
) -> Result<(), String> {
    let client = pool_ref.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            "INSERT INTO intelligence_usage (namespace, pool, agent, total_tokens)
             VALUES ($1, $2, $3, $4)",
            &[&ns, &pool, &agent, &tokens],
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `(total_tokens, request_count)` consumed against `(ns, pool)` — the usage
/// report (both 0 when no rows).
pub async fn usage_report(pool_ref: &Pool, ns: &str, pool: &str) -> Result<(i64, i64), String> {
    let client = pool_ref.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_one(
            "SELECT COALESCE(SUM(total_tokens), 0)::bigint, COUNT(*)::bigint
             FROM intelligence_usage WHERE namespace = $1 AND pool = $2",
            &[&ns, &pool],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok((row.get(0), row.get(1)))
}
