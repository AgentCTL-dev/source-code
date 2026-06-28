//! The durable token-usage meter (RFC 0012, the intelligence plane).
//!
//! Every `/v1/infer` call records the tokens it consumed against its
//! `(namespace, pool)` so the gateway can enforce the `ModelPool` budget
//! pre-request and report consumption. Backed by a shared Postgres so the
//! gateway stays a replicated, stateless front end — the meter lives in the
//! store, not the pod. Plain `NoTls` in-cluster (the hop is NetworkPolicy-scoped;
//! TLS to the DB is later hardening).
//!
//! These ops need a live Postgres; the pure budget/usage helpers live in
//! `main.rs` and are unit-tested there.

use deadpool_postgres::Pool;

/// Create the usage table if missing (idempotent; called with retry at startup).
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
            )",
        )
        .await
        .map_err(|e| e.to_string())
}

/// Record one inference's token consumption against `(ns, pool)`. `pool_ref` is
/// the Postgres connection pool; `pool` is the `ModelPool` name.
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

/// Total tokens consumed so far against `(ns, pool)` (0 when no rows). Used for
/// the pre-request budget check.
pub async fn pool_tokens(pool_ref: &Pool, ns: &str, pool: &str) -> Result<i64, String> {
    let client = pool_ref.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_one(
            "SELECT COALESCE(SUM(total_tokens), 0)::bigint FROM intelligence_usage
             WHERE namespace = $1 AND pool = $2",
            &[&ns, &pool],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(row.get(0))
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
