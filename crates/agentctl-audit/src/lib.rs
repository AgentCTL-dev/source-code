// SPDX-License-Identifier: Apache-2.0
//! # agentctl audit trail (`audit/v1`)
//!
//! One record vocabulary for the WHOLE control plane: the gateway (A2A
//! requests, hooks deliveries, approvals, gate transitions), identity
//! (exchange mints/refusals, consent), and — via their own emitters as they
//! land — apiserver/control verbs. Every record is attributed
//! `{org, namespace, workload, user?}` and correlates by `trail_id`
//! (propagated `x-agentctl-trail`, minted at the first hop) and `task_id`,
//! so ONE query answers "what happened, on whose behalf, end to end" for a
//! full OBO tool call.
//!
//! Design mirrors `agentctl-metering` deliberately: Apache-licensed public
//! contract, fire-and-forget durable PG sink off the request path, and a
//! filtered query surface served by the management API. Audit rows are
//! EVIDENCE, not flow control — a sink failure is logged, never surfaced.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The record schema tag.
pub const SCHEMA: &str = "audit/v1";

// -- actions (the closed core; components may add namespaced extras) ---------
pub const ACTION_A2A_REQUEST: &str = "a2a.request";
pub const ACTION_TASK_STATE: &str = "task.state";
pub const ACTION_GATE_NOTIFIED: &str = "gate.notified";
pub const ACTION_HOOK_DELIVERY: &str = "hook.delivery";
pub const ACTION_APPROVAL: &str = "approval.decision";
pub const ACTION_EXCHANGE: &str = "identity.exchange";
pub const ACTION_CONSENT: &str = "identity.consent";
pub const ACTION_CONNECTION_REVOKED: &str = "identity.connection_revoked";

// -- outcomes ----------------------------------------------------------------
pub const OUTCOME_OK: &str = "ok";
pub const OUTCOME_REFUSED: &str = "refused";
pub const OUTCOME_ERROR: &str = "error";

/// One audit record (the durable row).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Record {
    /// [`SCHEMA`].
    pub schema: String,
    /// Unix seconds.
    pub ts: i64,
    /// The emitting component (`gateway` | `identity` | `apiserver` | …).
    pub component: String,
    /// The owning organization (empty for unmanaged namespaces).
    pub org: String,
    pub namespace: String,
    /// The workload the action concerns (agent/fleet/supervisor name).
    pub workload: String,
    /// The acting human, when one is bound (org routes, OBO chains).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// One of the `ACTION_*` constants (or a namespaced extra).
    pub action: String,
    /// One of the `OUTCOME_*` constants.
    pub outcome: String,
    /// Cross-hop correlation: the trail id minted at the first hop and
    /// propagated as `x-agentctl-trail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_id: Option<String>,
    /// The A2A task the action belongs to, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Free-form refinements (`provider`, `method`, `path`, `state`, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dims: BTreeMap<String, String>,
}

impl Record {
    pub fn new(
        component: &str,
        org: impl Into<String>,
        namespace: impl Into<String>,
        workload: impl Into<String>,
        action: &str,
        outcome: &str,
    ) -> Record {
        Record {
            schema: SCHEMA.to_string(),
            ts: now_unix(),
            component: component.to_string(),
            org: org.into(),
            namespace: namespace.into(),
            workload: workload.into(),
            user: None,
            action: action.to_string(),
            outcome: outcome.to_string(),
            trail_id: None,
            task_id: None,
            dims: BTreeMap::new(),
        }
    }
    pub fn user(mut self, user: impl Into<String>) -> Record {
        self.user = Some(user.into());
        self
    }
    pub fn trail(mut self, trail: impl Into<String>) -> Record {
        let t = trail.into();
        if !t.is_empty() {
            self.trail_id = Some(t);
        }
        self
    }
    pub fn task(mut self, task: impl Into<String>) -> Record {
        self.task_id = Some(task.into());
        self
    }
    pub fn dim(mut self, k: &str, v: impl Into<String>) -> Record {
        self.dims.insert(k.to_string(), v.into());
        self
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Filters for the query surface. Every field is optional; unset = match all.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Query {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub org: Option<String>,
    pub user: Option<String>,
    pub action: Option<String>,
    pub trail_id: Option<String>,
    pub task_id: Option<String>,
    /// Row cap (default 500, hard max 5000 — evidence reads, not exports).
    pub limit: Option<i64>,
}

pub mod pg {
    use super::*;
    use deadpool_postgres::Pool;

    /// Idempotent DDL — call once at emitter/reader startup.
    pub async fn ensure_schema(pool: &Pool) -> Result<(), String> {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS audit_records (
                    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                    schema text NOT NULL,
                    ts bigint NOT NULL,
                    component text NOT NULL,
                    org text NOT NULL,
                    namespace text NOT NULL,
                    workload text NOT NULL,
                    usr text,
                    action text NOT NULL,
                    outcome text NOT NULL,
                    trail_id text,
                    task_id text,
                    dims jsonb NOT NULL DEFAULT '{}'::jsonb
                 );
                 CREATE INDEX IF NOT EXISTS audit_records_ts ON audit_records (ts);
                 CREATE INDEX IF NOT EXISTS audit_records_trail ON audit_records (trail_id);
                 CREATE INDEX IF NOT EXISTS audit_records_org_user
                     ON audit_records (org, usr, ts);",
            )
            .await
            .map_err(|e| e.to_string())
    }

    /// Insert one record. Callers SPAWN this (fire-and-forget) — an audit
    /// write failure is logged, never surfaced to the request path.
    pub async fn record(pool: &Pool, r: &Record) -> Result<(), String> {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        let dims = serde_json::to_value(&r.dims).unwrap_or_default();
        client
            .execute(
                "INSERT INTO audit_records
                    (schema, ts, component, org, namespace, workload, usr, action, outcome, trail_id, task_id, dims)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
                &[
                    &r.schema,
                    &r.ts,
                    &r.component,
                    &r.org,
                    &r.namespace,
                    &r.workload,
                    &r.user,
                    &r.action,
                    &r.outcome,
                    &r.trail_id,
                    &r.task_id,
                    &dims,
                ],
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The filtered trail read, newest first.
    pub async fn query(pool: &Pool, q: &Query) -> Result<Vec<Record>, String> {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        let mut sql = String::from(
            "SELECT schema, ts, component, org, namespace, workload, usr, action, outcome, trail_id, task_id, dims
             FROM audit_records WHERE 1=1",
        );
        let mut params: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> = Vec::new();
        let push = |sql: &mut String,
                    params: &mut Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>>,
                    clause: &str,
                    v: Box<dyn tokio_postgres::types::ToSql + Sync + Send>| {
            params.push(v);
            sql.push_str(&format!(" AND {} ${}", clause, params.len()));
        };
        if let Some(from) = q.from {
            push(&mut sql, &mut params, "ts >=", Box::new(from));
        }
        if let Some(to) = q.to {
            push(&mut sql, &mut params, "ts <=", Box::new(to));
        }
        if let Some(org) = &q.org {
            push(&mut sql, &mut params, "org =", Box::new(org.clone()));
        }
        if let Some(user) = &q.user {
            push(&mut sql, &mut params, "usr =", Box::new(user.clone()));
        }
        if let Some(action) = &q.action {
            push(&mut sql, &mut params, "action =", Box::new(action.clone()));
        }
        if let Some(trail) = &q.trail_id {
            push(&mut sql, &mut params, "trail_id =", Box::new(trail.clone()));
        }
        if let Some(task) = &q.task_id {
            push(&mut sql, &mut params, "task_id =", Box::new(task.clone()));
        }
        let limit = q.limit.unwrap_or(500).clamp(1, 5000);
        sql.push_str(&format!(" ORDER BY ts DESC, id DESC LIMIT {limit}"));
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
            .iter()
            .map(|b| b.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        let rows = client.query(&sql, &refs).await.map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|row| Record {
                schema: row.get(0),
                ts: row.get(1),
                component: row.get(2),
                org: row.get(3),
                namespace: row.get(4),
                workload: row.get(5),
                user: row.get(6),
                action: row.get(7),
                outcome: row.get(8),
                trail_id: row.get(9),
                task_id: row.get(10),
                dims: serde_json::from_value(row.get::<_, serde_json::Value>(11))
                    .unwrap_or_default(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_builder_attributes_and_correlates() {
        let r = Record::new(
            "gateway",
            "acme",
            "org-acme",
            "sup-erin",
            ACTION_A2A_REQUEST,
            OUTCOME_OK,
        )
        .user("mock:erin")
        .trail("tr-1")
        .task("task-9")
        .dim("method", "SendMessage");
        assert_eq!(r.schema, SCHEMA);
        assert_eq!(r.user.as_deref(), Some("mock:erin"));
        assert_eq!(r.trail_id.as_deref(), Some("tr-1"));
        assert_eq!(r.task_id.as_deref(), Some("task-9"));
        assert_eq!(r.dims["method"], "SendMessage");
        // An empty trail never stores as an empty string.
        let r2 =
            Record::new("identity", "", "ns", "wl", ACTION_EXCHANGE, OUTCOME_REFUSED).trail("");
        assert!(r2.trail_id.is_none());
    }
}
