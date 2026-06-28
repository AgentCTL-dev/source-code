//! The durable A2A task store (RFC 0013 / brainstorm D4).
//!
//! The agent serves only *live* tasks; the gateway persists task records here so
//! `tasks/get` survives the agent and `tasks/list` returns history. Backed by a
//! shared Postgres (so the gateway stays a replicated, stateless front end —
//! state lives in the store, not the pod). Plain `NoTls` in-cluster (the hop is
//! NetworkPolicy-scoped; TLS to the DB is later hardening). Pure
//! [`task_json`] is unit-tested; the DB ops need a live Postgres.

use deadpool_postgres::Pool;
use serde_json::{json, Value};

/// A persisted task row.
pub struct TaskRow {
    pub id: String,
    pub state: String,
    pub artifact: String,
}

/// Create the tables if missing (idempotent; called with retry at startup).
pub async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS a2a_tasks (
                namespace  text        NOT NULL,
                agent      text        NOT NULL,
                id         text        NOT NULL,
                state      text        NOT NULL,
                input      text        NOT NULL DEFAULT '',
                artifact   text        NOT NULL DEFAULT '',
                created_at timestamptz NOT NULL DEFAULT now(),
                updated_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (namespace, agent, id)
            );
            CREATE TABLE IF NOT EXISTS a2a_push_configs (
                namespace  text        NOT NULL,
                agent      text        NOT NULL,
                task_id    text        NOT NULL,
                url        text        NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (namespace, agent, task_id)
            )",
        )
        .await
        .map_err(|e| e.to_string())
}

/// Insert or update a task record for `(ns, agent, id)`.
pub async fn upsert(
    pool: &Pool,
    ns: &str,
    agent: &str,
    id: &str,
    state: &str,
    input: &str,
    artifact: &str,
) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            "INSERT INTO a2a_tasks (namespace, agent, id, state, input, artifact)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (namespace, agent, id)
             DO UPDATE SET state = $4, artifact = $6, updated_at = now()",
            &[&ns, &agent, &id, &state, &input, &artifact],
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Fetch one task record, if present.
pub async fn get(pool: &Pool, ns: &str, agent: &str, id: &str) -> Result<Option<TaskRow>, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_opt(
            "SELECT id, state, artifact FROM a2a_tasks
             WHERE namespace = $1 AND agent = $2 AND id = $3",
            &[&ns, &agent, &id],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(row.map(|r| TaskRow {
        id: r.get(0),
        state: r.get(1),
        artifact: r.get(2),
    }))
}

/// All task records for an agent, newest first (the durable history).
pub async fn list(pool: &Pool, ns: &str, agent: &str) -> Result<Vec<TaskRow>, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let rows = client
        .query(
            "SELECT id, state, artifact FROM a2a_tasks
             WHERE namespace = $1 AND agent = $2 ORDER BY created_at DESC",
            &[&ns, &agent],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| TaskRow {
            id: r.get(0),
            state: r.get(1),
            artifact: r.get(2),
        })
        .collect())
}

/// Update a task's state (e.g. on cancel). Returns whether a row matched.
pub async fn set_state(
    pool: &Pool,
    ns: &str,
    agent: &str,
    id: &str,
    state: &str,
) -> Result<bool, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let n = client
        .execute(
            "UPDATE a2a_tasks SET state = $4, updated_at = now()
             WHERE namespace = $1 AND agent = $2 AND id = $3",
            &[&ns, &agent, &id, &state],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

/// Register (or replace) the push-notification webhook URL for a task.
pub async fn push_set(
    pool: &Pool,
    ns: &str,
    agent: &str,
    task_id: &str,
    url: &str,
) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            "INSERT INTO a2a_push_configs (namespace, agent, task_id, url)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (namespace, agent, task_id) DO UPDATE SET url = $4",
            &[&ns, &agent, &task_id, &url],
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The webhook URL registered for a task, if any.
pub async fn push_get(
    pool: &Pool,
    ns: &str,
    agent: &str,
    task_id: &str,
) -> Result<Option<String>, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_opt(
            "SELECT url FROM a2a_push_configs
             WHERE namespace = $1 AND agent = $2 AND task_id = $3",
            &[&ns, &agent, &task_id],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(row.map(|r| r.get(0)))
}

/// All push-notification configs registered for an agent.
pub async fn push_list(
    pool: &Pool,
    ns: &str,
    agent: &str,
) -> Result<Vec<(String, String)>, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let rows = client
        .query(
            "SELECT task_id, url FROM a2a_push_configs
             WHERE namespace = $1 AND agent = $2 ORDER BY created_at DESC",
            &[&ns, &agent],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// Remove a task's push-notification config. Returns whether a row matched.
pub async fn push_delete(
    pool: &Pool,
    ns: &str,
    agent: &str,
    task_id: &str,
) -> Result<bool, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let n = client
        .execute(
            "DELETE FROM a2a_push_configs
             WHERE namespace = $1 AND agent = $2 AND task_id = $3",
            &[&ns, &agent, &task_id],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

/// Render a stored row as an A2A Task object (the wire shape clients expect).
pub fn task_json(row: &TaskRow) -> Value {
    let artifacts = if row.artifact.is_empty() {
        json!([])
    } else {
        json!([{
            "artifactId": "art-1",
            "parts": [{ "kind": "text", "text": row.artifact }]
        }])
    };
    json!({
        "id": row.id,
        "contextId": "ctx-1",
        "status": { "state": row.state },
        "artifacts": artifacts,
        "kind": "task",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_json_shapes_a_task() {
        let row = TaskRow {
            id: "t-1".into(),
            state: "completed".into(),
            artifact: "echo: hi".into(),
        };
        let t = task_json(&row);
        assert_eq!(t["id"], "t-1");
        assert_eq!(t["kind"], "task");
        assert_eq!(t["status"]["state"], "completed");
        assert_eq!(t["artifacts"][0]["parts"][0]["text"], "echo: hi");
    }

    #[test]
    fn task_json_empty_artifact_is_empty_array() {
        let row = TaskRow {
            id: "t-2".into(),
            state: "canceled".into(),
            artifact: String::new(),
        };
        let t = task_json(&row);
        assert_eq!(t["status"]["state"], "canceled");
        assert_eq!(t["artifacts"], json!([]));
    }
}
