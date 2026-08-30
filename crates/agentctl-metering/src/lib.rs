// SPDX-License-Identifier: Apache-2.0
//! # Billing-ready metering (RFC 0035, P7-4)
//!
//! The VERSIONED usage-event vocabulary, its durable Postgres sink, and the
//! period aggregation the export/invoice pipeline reads. Design stance:
//!
//! * **The vocabulary is a public contract** (`metering/v1`): every event is
//!   attributed `{org, namespace, workload, user?}` plus free-form `dims`,
//!   and a QUANTITY in a named UNIT — an invoice is computable from the
//!   export alone, with no reference back to internal state.
//! * **Emit is fire-and-forget**: metering must never sit on a request path.
//!   Emitters spawn the insert and drop the handle; Prometheus counters
//!   (kept by each component) remain the low-latency operational signal —
//!   the durable rows are the BILLING signal.
//! * **Sources land incrementally**: the vocabulary defines more kinds than
//!   currently emit (tokens-by-tier waits on agentd usage export; sandbox
//!   CPU-seconds on P5-5). A kind with no emitter simply has no rows — the
//!   schema does not change when a source arrives.
//!
//! Current emitters: the GATEWAY (every A2A request at the traffic
//! chokepoint: `a2a_requests`, `supervisor_conversations` — user-attributed
//! on org routes) — with the aggregation served by the apiserver's
//! management API (`/metering/export`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The event schema tag. Bump ONLY with an RFC; readers reject unknown tags.
pub const SCHEMA: &str = "metering/v1";

/// Event kinds with emitters today.
pub const KIND_A2A_REQUESTS: &str = "a2a_requests";
pub const KIND_SUPERVISOR_CONVERSATIONS: &str = "supervisor_conversations";
/// Defined, source pending (see module docs).
pub const KIND_TOKENS: &str = "tokens";
pub const KIND_AGENT_SECONDS: &str = "agent_seconds";
pub const KIND_TOOL_CALLS: &str = "tool_calls";
pub const KIND_WORK_ITEMS: &str = "work_items";
pub const KIND_SANDBOX_CPU_SECONDS: &str = "sandbox_cpu_seconds";
pub const KIND_STATE_BYTES: &str = "state_bytes";
pub const KIND_GATE_EVENTS: &str = "gate_events";

/// One usage event (the durable row).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    /// [`SCHEMA`].
    pub schema: String,
    /// Unix seconds.
    pub ts: i64,
    /// The owning organization (empty for unmanaged namespaces).
    pub org: String,
    pub namespace: String,
    /// The workload (agent/fleet/supervisor name) the usage attributes to.
    pub workload: String,
    /// The acting human, when one is bound (org routes, OBO chains).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// One of the `KIND_*` constants.
    pub kind: String,
    pub quantity: i64,
    /// `requests` | `conversations` | `tokens` | `seconds` | `calls` |
    /// `items` | `bytes` | `events`.
    pub unit: String,
    /// Free-form attribution refinements (`tier`, `service`, `method`, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dims: BTreeMap<String, String>,
}

impl Event {
    pub fn new(
        org: impl Into<String>,
        namespace: impl Into<String>,
        workload: impl Into<String>,
        kind: &str,
        quantity: i64,
        unit: &str,
    ) -> Event {
        Event {
            schema: SCHEMA.to_string(),
            ts: now_unix(),
            org: org.into(),
            namespace: namespace.into(),
            workload: workload.into(),
            user: None,
            kind: kind.to_string(),
            quantity,
            unit: unit.to_string(),
            dims: BTreeMap::new(),
        }
    }
    pub fn user(mut self, user: impl Into<String>) -> Event {
        self.user = Some(user.into());
        self
    }
    pub fn dim(mut self, k: &str, v: impl Into<String>) -> Event {
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

/// One aggregated export row: the invoice's line-item input.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AggRow {
    pub org: String,
    pub namespace: String,
    pub workload: String,
    pub kind: String,
    pub unit: String,
    pub total: i64,
    pub events: i64,
}

/// Render export rows as CSV (header + RFC4180-quoted cells).
pub fn to_csv(rows: &[AggRow]) -> String {
    let mut out = String::from("org,namespace,workload,kind,unit,total,events\n");
    let cell = |s: &str| {
        if s.contains([',', '"', '\n']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            cell(&r.org),
            cell(&r.namespace),
            cell(&r.workload),
            cell(&r.kind),
            cell(&r.unit),
            r.total,
            r.events
        ));
    }
    out
}

/// The durable sink + aggregation over the shared Postgres.
pub mod pg {
    use super::*;
    use deadpool_postgres::Pool;

    /// Idempotent DDL — call once at emitter/exporter startup.
    pub async fn ensure_schema(pool: &Pool) -> Result<(), String> {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS metering_events (
                    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                    schema text NOT NULL,
                    ts bigint NOT NULL,
                    org text NOT NULL,
                    namespace text NOT NULL,
                    workload text NOT NULL,
                    usr text,
                    kind text NOT NULL,
                    quantity bigint NOT NULL,
                    unit text NOT NULL,
                    dims jsonb NOT NULL DEFAULT '{}'::jsonb
                 );
                 CREATE INDEX IF NOT EXISTS metering_events_ts ON metering_events (ts);
                 CREATE INDEX IF NOT EXISTS metering_events_org_kind
                     ON metering_events (org, kind, ts);",
            )
            .await
            .map_err(|e| e.to_string())
    }

    /// Insert one event. Callers SPAWN this (fire-and-forget) — a metering
    /// failure is logged, never surfaced to the request path.
    pub async fn record(pool: &Pool, ev: &Event) -> Result<(), String> {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        client
            .execute(
                "INSERT INTO metering_events
                    (schema, ts, org, namespace, workload, usr, kind, quantity, unit, dims)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
                &[
                    &ev.schema,
                    &ev.ts,
                    &ev.org,
                    &ev.namespace,
                    &ev.workload,
                    &ev.user,
                    &ev.kind,
                    &ev.quantity,
                    &ev.unit,
                    &serde_json::to_value(&ev.dims).unwrap_or(serde_json::json!({})),
                ],
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Aggregate `[from, to)` grouped by the attribution tuple — the export.
    pub async fn export(pool: &Pool, from: i64, to: i64) -> Result<Vec<AggRow>, String> {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        let rows = client
            .query(
                "SELECT org, namespace, workload, kind, unit,
                        COALESCE(SUM(quantity), 0)::bigint AS total,
                        COUNT(*)::bigint AS events
                 FROM metering_events
                 WHERE ts >= $1 AND ts < $2
                 GROUP BY org, namespace, workload, kind, unit
                 ORDER BY org, namespace, workload, kind",
                &[&from, &to],
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| AggRow {
                org: r.get(0),
                namespace: r.get(1),
                workload: r.get(2),
                kind: r.get(3),
                unit: r.get(4),
                total: r.get(5),
                events: r.get(6),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vocabulary round-trips and stays attributed; CSV quotes what it
    /// must. An invoice line is computable from the row alone.
    #[test]
    fn events_are_attributed_and_csv_is_sane() {
        let ev = Event::new(
            "acme",
            "org-acme",
            "sup-alice",
            KIND_SUPERVISOR_CONVERSATIONS,
            1,
            "conversations",
        )
        .user("okta:alice")
        .dim("method", "SendMessage");
        assert_eq!(ev.schema, SCHEMA);
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);

        let rows = vec![
            AggRow {
                org: "acme".into(),
                namespace: "org-acme".into(),
                workload: "sup-alice".into(),
                kind: KIND_SUPERVISOR_CONVERSATIONS.into(),
                unit: "conversations".into(),
                total: 42,
                events: 42,
            },
            AggRow {
                org: "we,ird\"co".into(),
                namespace: "n".into(),
                workload: "w".into(),
                kind: KIND_A2A_REQUESTS.into(),
                unit: "requests".into(),
                total: 7,
                events: 7,
            },
        ];
        let csv = to_csv(&rows);
        assert!(csv.starts_with("org,namespace,workload,kind,unit,total,events\n"));
        assert!(
            csv.contains("acme,org-acme,sup-alice,supervisor_conversations,conversations,42,42")
        );
        assert!(csv.contains("\"we,ird\"\"co\""));
        // Invoice math from the export alone: sum per org.
        let acme_total: i64 = rows
            .iter()
            .filter(|r| r.org == "acme")
            .map(|r| r.total)
            .sum();
        assert_eq!(acme_total, 42);
    }
}
