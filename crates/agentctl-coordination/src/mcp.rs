// SPDX-License-Identifier: BUSL-1.1
//! The MCP JSON-RPC 2.0 wire layer — the server side of the stable `work.*`
//! contract (the method names and `_meta` keys the conformant agent claim client
//! depends on).
//!
//! Methods: `initialize`, `tools/list`, `tools/call` (`{name, arguments, _meta}`),
//! `resources/list`, `resources/read`. A `tools/call` result returns BOTH
//! `structuredContent` AND a text `content[]` item carrying the SAME JSON — the
//! agent parses `structuredContent` preferred, text as fallback, so both must
//! agree.
//!
//! This module is pure (`Value` in, `Value` out) so the whole contract is
//! unit-testable without a socket; the axum layer in `main.rs` only adds HTTP.

use serde_json::{json, Value};

use crate::attest::{self, CallerIdentity, HolderCheck};
use crate::metrics::Metrics;
use crate::store::{is_holder_mismatch, ClaimResult, ClaimStore, SubmitOutcome};

/// The MCP protocol version this server advertises. Interop is negotiated by
/// capability rather than strict version matching.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// The countable backlog resource — the scale-from-zero signal an external
/// autoscaler reads. Observable even at zero pods because it lives on the server.
pub const PENDING_URI: &str = "work://pending";

/// The dead-letter queue resource — items redelivered past `max_attempts`, awaiting
/// an admin requeue/drop.
pub const DLQ_URI: &str = "dlq://items";

/// The stable `_meta` key carrying the item-derived dedupe key. Present on
/// `work.claim` and `work.ack`.
const META_CLAIM_KEY: &str = "agent/claim_key";

/// Dispatch one JSON-RPC message. `Some(response)` for a request; `None` for a
/// notification (no `id` / `notifications/*`) — the caller returns 202 with no
/// body. The store is the serializing point; this layer only translates wire ⇄
/// store and counts metrics.
///
/// `caller` is the source-IP-attested identity computed by the axum layer.
/// [`CallerIdentity::Disabled`] (attest mode off, the default) uses the
/// self-asserted `_meta` holder; otherwise the claim lifecycle is bound to and
/// verified against the attested holder.
pub fn handle_rpc(
    req: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
    caller: &CallerIdentity,
) -> Option<Value> {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Notifications carry no id and expect no response (e.g.
    // `notifications/initialized` after the handshake).
    if method.starts_with("notifications/") {
        return None;
    }
    // Notifications carry no id; by JSON-RPC, a message without an id expects no
    // response, so `?` short-circuits to None here.
    let id = req.get("id").cloned()?;
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let resp = match method {
        "initialize" => ok(id, initialize_result()),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tool_defs() })),
        "tools/call" => tools_call(id, &params, store, metrics, caller),
        "resources/list" => ok(id, json!({ "resources": resource_defs() })),
        "resources/read" => resources_read(id, &params, store),
        other => err(id, -32601, &format!("method not found: {other}")),
    };
    Some(resp)
}

/// The `initialize` result — advertises tools + resources (no subscribe; the
/// scaler polls `work.stats` / `work://pending`, the server pushes nothing).
fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {}, "resources": {} },
        "serverInfo": {
            "name": "agentctl-coordination",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Reference coordination MCP server. \
            Call work.claim before processing an item; work.renew at ttl/3; \
            work.ack on success (terminal, dedupes the claim_key); work.release on \
            wind-down. Read work://pending or call work.stats for the backlog.",
    })
}

/// `tools/call` → run the named tool, wrap its outcome in the dual
/// `content`/`structuredContent` result the agent parses.
fn tools_call(
    id: Value,
    params: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
    caller: &CallerIdentity,
) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let empty = Value::Null;
    let args = params.get("arguments").unwrap_or(&empty);
    let meta = params.get("_meta").unwrap_or(&empty);
    let (structured, is_error) = dispatch_tool(name, args, meta, store, metrics, caller);
    ok(id, tool_result(structured, is_error))
}

/// Wrap a tool's structured body into the MCP `CallToolResult`: the SAME JSON in
/// both `structuredContent` (preferred by the agent) and a text `content[]` item
/// (its fallback). `isError` is the tool-domain failure flag.
fn tool_result(structured: Value, is_error: bool) -> Value {
    let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": is_error,
    })
}

/// Route to the per-tool handler. Returns `(structured_body, is_error)`.
#[tracing::instrument(skip_all, fields(tool = %name))]
fn dispatch_tool(
    name: &str,
    args: &Value,
    meta: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
    caller: &CallerIdentity,
) -> (Value, bool) {
    match name {
        "work.claim" => tool_claim(args, meta, store, metrics, caller),
        "work.renew" => tool_renew(args, store, metrics, caller),
        "work.ack" => tool_ack(args, meta, store, metrics, caller),
        "work.release" => tool_release(args, store, metrics, caller),
        "work.submit" => tool_submit(args, store, metrics, caller),
        "work.stats" => tool_stats(store, caller),
        "work.result" => tool_work_result(args, store, caller),
        "work.deadletter" => tool_deadletter(args, store, caller),
        other => (json!({ "error": format!("unknown tool: {other}") }), true),
    }
}

/// Resolve the `expected_holder` predicate for a verifying lifecycle op
/// (ack/renew/release) from the attested caller, via the pure [`attest::holder_check`].
/// `Ok(None)` ⇒ unconstrained (attest off); `Ok(Some(h))` ⇒ constrain the store op
/// to holder `h`; `Err(reject)` ⇒ the caller is unattestable (fail closed) — the
/// returned tuple is the JSON-RPC reject body, already counted in `attest_reject`.
fn resolve_expected_holder(
    caller: &CallerIdentity,
    metrics: &Metrics,
    op: &str,
) -> Result<Option<String>, (Value, bool)> {
    match attest::holder_check(caller) {
        HolderCheck::Unconstrained => Ok(None),
        HolderCheck::MustMatch(h) => Ok(Some(h)),
        HolderCheck::Reject => {
            metrics.inc_attest_reject();
            tracing::warn!(
                op,
                "attest: source IP resolves to no pod; rejecting lifecycle call"
            );
            Err((
                json!({
                    "error": format!(
                        "{op}: cannot attest caller identity from source IP (attested mode, fail closed)"
                    )
                }),
                true,
            ))
        }
    }
}

/// Translate a verifying lifecycle op's store result into the wire tuple, counting
/// the attestation outcome. `constrained` is whether the op carried an attested
/// holder (so a success is an attest-ok and a holder mismatch an attest-reject).
fn finish_verify(
    result: Result<(), String>,
    constrained: bool,
    metrics: &Metrics,
    ok_body: Value,
) -> (Value, bool) {
    match result {
        Ok(()) => {
            if constrained {
                metrics.inc_attest_ok();
            }
            (ok_body, false)
        }
        Err(e) => {
            if constrained && is_holder_mismatch(&e) {
                metrics.inc_attest_reject();
                tracing::warn!(error = %e, "attest: holder mismatch on lifecycle call; rejecting");
            }
            (json!({ "ok": false, "error": e }), true)
        }
    }
}

/// `work.claim` — the atomic grant. arguments `{item, ttl_ms}`,
/// `_meta {agent/claim_key}`. Result: `{granted:true, lease_id, expires_in_ms}`
/// or `{granted:false, held_by}` (a normal "lost"/"deduped" outcome — NOT
/// `isError`, so the agent treats it as Lost, never a transport Error).
fn tool_claim(
    args: &Value,
    meta: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
    caller: &CallerIdentity,
) -> (Value, bool) {
    let item = args.get("item").and_then(Value::as_str);
    let ttl_ms = args.get("ttl_ms").and_then(Value::as_u64);
    let claim_key = meta.get(META_CLAIM_KEY).and_then(Value::as_str);
    let (item, ttl_ms, claim_key) = match (item, ttl_ms, claim_key) {
        (Some(i), Some(t), Some(k)) => (i, t, k),
        _ => {
            return (
                json!({ "error": "work.claim requires arguments.item, arguments.ttl_ms and _meta.\"agent/claim_key\"" }),
                true,
            )
        }
    };
    // Holder binding: in attested mode the lease HOLDER is the
    // source-IP-attested identity (authoritative — it overrides the self-asserted
    // `_meta` agent so a tenant cannot bill the lease to another). An unattestable
    // caller fails closed (the claim is rejected). Attest off ⇒ the self-asserted
    // holder, unchanged.
    let self_asserted = holder_of(meta);
    let holder = match attest::claim_holder(caller, &self_asserted) {
        Some(h) => h,
        None => {
            metrics.inc_attest_reject();
            tracing::warn!(
                item,
                "attest: source IP resolves to no pod; rejecting work.claim"
            );
            return (
                json!({ "error": "work.claim: cannot attest caller identity from source IP (attested mode, fail closed)" }),
                true,
            );
        }
    };
    if caller.is_attested() {
        metrics.inc_attest_ok();
    }
    match store.claim(item, ttl_ms, claim_key, &holder) {
        ClaimResult::Granted {
            lease_id,
            expires_in_ms,
        } => {
            metrics.inc_claim_granted();
            tracing::info!(item, holder = %holder, lease_id = %lease_id, "claim granted");
            (
                json!({ "granted": true, "lease_id": lease_id, "expires_in_ms": expires_in_ms }),
                false,
            )
        }
        ClaimResult::Contended { held_by } => {
            metrics.inc_claim_contended();
            (json!({ "granted": false, "held_by": held_by }), false)
        }
        ClaimResult::Deduped => {
            metrics.inc_claim_deduped();
            (json!({ "granted": false, "held_by": "<acked>" }), false)
        }
        ClaimResult::Deadlettered => {
            metrics.inc_claim_deadlettered();
            // A normal "lost" outcome (not isError): the claimer treats it as Lost
            // and stops — the item is poison, held out until an admin requeues it.
            (
                json!({ "granted": false, "held_by": "<deadletter>" }),
                false,
            )
        }
    }
}

/// `work.renew` — extend a live, owned lease. arguments `{lease_id, ttl_ms}`. In
/// attested mode the caller's attested identity MUST equal the lease holder (a
/// tenant cannot renew another tenant's lease); an unattestable caller fails closed.
fn tool_renew(
    args: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
    caller: &CallerIdentity,
) -> (Value, bool) {
    let lease_id = args.get("lease_id").and_then(Value::as_str);
    let ttl_ms = args.get("ttl_ms").and_then(Value::as_u64);
    let (lease_id, ttl_ms) = match (lease_id, ttl_ms) {
        (Some(l), Some(t)) => (l, t),
        _ => {
            return (
                json!({ "error": "work.renew requires arguments.lease_id and arguments.ttl_ms" }),
                true,
            )
        }
    };
    let expected = match resolve_expected_holder(caller, metrics, "work.renew") {
        Ok(e) => e,
        Err(reject) => return reject,
    };
    let result = store.renew(lease_id, ttl_ms, expected.as_deref());
    if result.is_ok() {
        metrics.inc_renewed();
    }
    finish_verify(
        result,
        expected.is_some(),
        metrics,
        json!({ "ok": true, "renewed": true, "expires_in_ms": ttl_ms }),
    )
}

/// `work.ack` — terminal settle + record the `claim_key` as done (dedupe set).
/// arguments `{lease_id, result?}`, `_meta {agent/claim_key}`. Idempotent. The
/// optional `result` (any JSON value) is recorded atomically with the
/// settle and returned by a later `work.result` — how a coordinator collects what a
/// worker produced.
fn tool_ack(
    args: &Value,
    meta: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
    caller: &CallerIdentity,
) -> (Value, bool) {
    let lease_id = args.get("lease_id").and_then(Value::as_str);
    let claim_key = meta.get(META_CLAIM_KEY).and_then(Value::as_str);
    let (lease_id, claim_key) = match (lease_id, claim_key) {
        (Some(l), Some(k)) => (l, k),
        _ => {
            return (
                json!({ "error": "work.ack requires arguments.lease_id and _meta.\"agent/claim_key\"" }),
                true,
            )
        }
    };
    // The result is serialized to a compact JSON string for the store (which is
    // value-agnostic). A bare string is stored as-is; any other JSON is stringified.
    let result_str = args.get("result").map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let expected = match resolve_expected_holder(caller, metrics, "work.ack") {
        Ok(e) => e,
        Err(reject) => return reject,
    };
    let outcome = store.ack(
        lease_id,
        claim_key,
        expected.as_deref(),
        result_str.as_deref(),
    );
    if outcome.is_ok() {
        metrics.inc_acked();
    }
    finish_verify(
        outcome,
        expected.is_some(),
        metrics,
        json!({ "ok": true, "acked": true }),
    )
}

/// `work.release` — return the item to pending (re-claimable). arguments
/// `{lease_id, reason}`. In attested mode the caller's attested identity MUST equal
/// the lease holder (a tenant cannot release another tenant's lease); an
/// unattestable caller fails closed.
fn tool_release(
    args: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
    caller: &CallerIdentity,
) -> (Value, bool) {
    let lease_id = match args.get("lease_id").and_then(Value::as_str) {
        Some(l) => l,
        None => {
            return (
                json!({ "error": "work.release requires arguments.lease_id" }),
                true,
            )
        }
    };
    let reason = args.get("reason").and_then(Value::as_str).unwrap_or("");
    let expected = match resolve_expected_holder(caller, metrics, "work.release") {
        Ok(e) => e,
        Err(reject) => return reject,
    };
    let result = store.release(lease_id, reason, expected.as_deref());
    if result.is_ok() {
        metrics.inc_released();
    }
    finish_verify(
        result,
        expected.is_some(),
        metrics,
        json!({ "ok": true, "released": true }),
    )
}

/// `work.submit` — enqueue an item into the backlog (producer side). arguments
/// `{item, claim_key?, max_attempts?}`. Skips if `claim_key` is already done or
/// dead-lettered. Returns the `work_id` (the effective `claim_key`) so a producer
/// can later correlate the outcome via `work.result`. `max_attempts`
/// bounds redelivery (from the fleet `work` policy); absent ⇒ unbounded.
///
/// Producers may be EXTERNAL (not pod-attestable), so submit is NOT hard-blocked by
/// attestation — it stays token-gated. We attest-if-resolvable only to LOG the
/// caller (observability under hostile multi-tenancy).
fn tool_submit(
    args: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
    caller: &CallerIdentity,
) -> (Value, bool) {
    log_attested_caller("work.submit", caller);
    let item = match args.get("item").and_then(Value::as_str) {
        Some(i) => i,
        None => {
            return (
                json!({ "error": "work.submit requires arguments.item" }),
                true,
            )
        }
    };
    let claim_key = args.get("claim_key").and_then(Value::as_str);
    let max_attempts = args
        .get("max_attempts")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    // The work_id a producer uses for a later work.result lookup is the effective
    // claim_key (the item URI when none is given — matches the store keying).
    let work_id = claim_key.unwrap_or(item).to_string();
    match store.submit(item, claim_key, max_attempts) {
        SubmitOutcome::Enqueued => {
            metrics.inc_submitted();
            (
                json!({ "submitted": true, "deduped": false, "work_id": work_id }),
                false,
            )
        }
        SubmitOutcome::Deduped => (
            json!({ "submitted": false, "deduped": true, "work_id": work_id }),
            false,
        ),
        SubmitOutcome::AlreadyPending => (
            json!({ "submitted": false, "deduped": false, "reason": "already_pending", "work_id": work_id }),
            false,
        ),
        SubmitOutcome::AlreadyClaimed => (
            json!({ "submitted": false, "deduped": false, "reason": "already_claimed", "work_id": work_id }),
            false,
        ),
        SubmitOutcome::Deadlettered => (
            json!({ "submitted": false, "deduped": false, "reason": "deadletter", "work_id": work_id }),
            false,
        ),
    }
}

/// `work.stats` — the off-pod backlog snapshot. The scaler reads this and may
/// not be pod-attestable, so it is NOT hard-blocked by attestation (token-gated);
/// we attest-if-resolvable only to LOG the caller.
fn tool_stats(store: &dyn ClaimStore, caller: &CallerIdentity) -> (Value, bool) {
    log_attested_caller("work.stats", caller);
    let s = store.stats();
    (
        json!({
            "pending": s.pending,
            "claimed": s.claimed,
            "oldest_age_ms": s.oldest_age_ms,
            "deadletter": s.deadletter,
        }),
        false,
    )
}

/// `work.result` — correlate a submitted work unit to its outcome.
/// arguments `{work_id}` (the effective `claim_key` returned by `work.submit`).
/// Result: `{state, result?}` where `state` ∈ pending|claimed|done|deadletter|unknown
/// and `result` is the JSON the worker recorded on `work.ack` (only for `done`).
/// Token-gated like submit/stats (a coordinator/producer may be external).
fn tool_work_result(
    args: &Value,
    store: &dyn ClaimStore,
    caller: &CallerIdentity,
) -> (Value, bool) {
    log_attested_caller("work.result", caller);
    let work_id = match args.get("work_id").and_then(Value::as_str) {
        Some(w) => w,
        None => {
            return (
                json!({ "error": "work.result requires arguments.work_id" }),
                true,
            )
        }
    };
    let status = store.result_of(work_id);
    // The stored result is a JSON string; re-parse it so the caller gets structured
    // JSON back (falling back to the raw string if it was a bare string).
    let result = status
        .result
        .map(|s| serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)));
    (
        json!({ "work_id": work_id, "state": status.state.as_str(), "result": result }),
        false,
    )
}

/// `work.deadletter` — inspect and manage the dead-letter queue.
/// arguments `{action, work_id?}`: `list` → `{items:[{work_id,item,attempts}]}`;
/// `requeue`/`drop` require `work_id` → `{ok, found}`. Token-gated (an operator/admin
/// tool, not a per-lease holder op).
fn tool_deadletter(args: &Value, store: &dyn ClaimStore, caller: &CallerIdentity) -> (Value, bool) {
    log_attested_caller("work.deadletter", caller);
    let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
    match action {
        "list" => {
            let items: Vec<Value> = store
                .dead_items()
                .into_iter()
                .map(|d| json!({ "work_id": d.claim_key, "item": d.item, "attempts": d.attempts }))
                .collect();
            (json!({ "items": items }), false)
        }
        "requeue" | "drop" => {
            let work_id = match args.get("work_id").and_then(Value::as_str) {
                Some(w) => w,
                None => {
                    return (
                        json!({ "error": format!("work.deadletter {action} requires arguments.work_id") }),
                        true,
                    )
                }
            };
            let found = if action == "requeue" {
                store.requeue_dead(work_id)
            } else {
                store.drop_dead(work_id)
            };
            (
                json!({ "ok": true, "action": action, "work_id": work_id, "found": found }),
                false,
            )
        }
        other => (
            json!({ "error": format!("work.deadletter: unknown action {other:?} (want list|requeue|drop)") }),
            true,
        ),
    }
}

/// Log the attested caller for a NON-blocking (token-gated) call (`work.submit` /
/// `work.stats`). Only logs when attest mode resolved an identity; never blocks.
fn log_attested_caller(op: &str, caller: &CallerIdentity) {
    if let CallerIdentity::Attested(id) = caller {
        tracing::debug!(op, attested = %id, "attest: producer/reader caller attested (not blocked)");
    }
}

/// `resources/read` — serves `work://pending` (the pending count + items, the
/// from-zero scale signal) and `dlq://items` (the dead-letter contents). The body
/// JSON rides the `text` field, which the agent's resource reader concatenates.
fn resources_read(id: Value, params: &Value, store: &dyn ClaimStore) -> Value {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (matched_uri, body) = if uri == PENDING_URI {
        let items = store.pending_items();
        (
            PENDING_URI,
            json!({ "pending": items.len(), "items": items }),
        )
    } else if uri == DLQ_URI {
        let items: Vec<Value> = store
            .dead_items()
            .into_iter()
            .map(|d| json!({ "work_id": d.claim_key, "item": d.item, "attempts": d.attempts }))
            .collect();
        (
            DLQ_URI,
            json!({ "deadletter": items.len(), "items": items }),
        )
    } else {
        return err(id, -32602, &format!("unknown resource uri: {uri}"));
    };
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    ok(
        id,
        json!({
            "contents": [{
                "uri": matched_uri,
                "mimeType": "application/json",
                "text": text,
            }],
        }),
    )
}

/// The advertised tools (`tools/list`). The agent validates a coordination server
/// by requiring BOTH `work.claim` and `work.ack` here.
fn tool_defs() -> Vec<Value> {
    vec![
        tool_def(
            "work.claim",
            "Atomically claim an item for a TTL; grants to exactly one of N racers.",
            json!({
                "type": "object",
                "properties": {
                    "item": { "type": "string", "description": "The item URI to claim." },
                    "ttl_ms": { "type": "integer", "minimum": 0, "description": "Requested lease TTL in ms (server is authoritative)." }
                },
                "required": ["item", "ttl_ms"]
            }),
        ),
        tool_def(
            "work.renew",
            "Extend a live, owned lease.",
            json!({
                "type": "object",
                "properties": {
                    "lease_id": { "type": "string" },
                    "ttl_ms": { "type": "integer", "minimum": 0 }
                },
                "required": ["lease_id", "ttl_ms"]
            }),
        ),
        tool_def(
            "work.ack",
            "Settle a lease (terminal), record its claim_key as done (dedupe), and optionally record a result.",
            json!({
                "type": "object",
                "properties": {
                    "lease_id": { "type": "string" },
                    "result": { "description": "Optional outcome (any JSON) retrievable via work.result." }
                },
                "required": ["lease_id"]
            }),
        ),
        tool_def(
            "work.release",
            "Return a held item to pending (re-claimable).",
            json!({
                "type": "object",
                "properties": {
                    "lease_id": { "type": "string" },
                    "reason": { "type": "string" }
                },
                "required": ["lease_id"]
            }),
        ),
        tool_def(
            "work.submit",
            "Enqueue an item into the backlog (skipped if done/deadletter). Returns a work_id for work.result correlation.",
            json!({
                "type": "object",
                "properties": {
                    "item": { "type": "string" },
                    "claim_key": { "type": "string" },
                    "max_attempts": { "type": "integer", "minimum": 1, "description": "Dead-letter after this many redeliveries; absent ⇒ unbounded." }
                },
                "required": ["item"]
            }),
        ),
        tool_def(
            "work.stats",
            "Backlog snapshot: { pending, claimed, oldest_age_ms, deadletter } (the P9 from-zero signal).",
            json!({ "type": "object", "properties": {} }),
        ),
        tool_def(
            "work.result",
            "Correlate a submitted work unit to its outcome: { state, result? } by work_id.",
            json!({
                "type": "object",
                "properties": { "work_id": { "type": "string" } },
                "required": ["work_id"]
            }),
        ),
        tool_def(
            "work.deadletter",
            "Inspect/manage the dead-letter queue: action list|requeue|drop (requeue/drop need work_id).",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "requeue", "drop"] },
                    "work_id": { "type": "string" }
                },
                "required": ["action"]
            }),
        ),
    ]
}

/// One `tools/list` entry.
fn tool_def(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

/// The advertised resources (`resources/list`).
fn resource_defs() -> Vec<Value> {
    vec![
        json!({
            "uri": PENDING_URI,
            "name": "pending",
            "description": "Pending (unclaimed) item count + list — the off-pod scale-from-zero signal (P9).",
            "mimeType": "application/json",
        }),
        json!({
            "uri": DLQ_URI,
            "name": "deadletter",
            "description": "Dead-lettered items (redelivered past max_attempts) awaiting requeue/drop.",
            "mimeType": "application/json",
        }),
    ]
}

/// Holder identity for `held_by`: prefer the stable `agent/instance`, then
/// `agent/run_id`; else `anonymous`.
fn holder_of(meta: &Value) -> String {
    for key in ["agent/instance", "agent/run_id"] {
        if let Some(s) = meta.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    "anonymous".to_string()
}

/// A JSON-RPC 2.0 success envelope.
fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC 2.0 error envelope, preserving the request id.
fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::InMemoryStore;

    fn ctx() -> (InMemoryStore, Metrics) {
        (InMemoryStore::new(4096), Metrics::new())
    }

    /// A `tools/call` with attestation DISABLED (back-compat) — the default for the
    /// existing behaviour tests.
    fn call(
        store: &dyn ClaimStore,
        metrics: &Metrics,
        id: i64,
        name: &str,
        args: Value,
        meta: Value,
    ) -> Value {
        call_as(
            store,
            metrics,
            id,
            name,
            args,
            meta,
            &CallerIdentity::Disabled,
        )
    }

    /// A `tools/call` with an explicit attested caller (attested-mode tests).
    fn call_as(
        store: &dyn ClaimStore,
        metrics: &Metrics,
        id: i64,
        name: &str,
        args: Value,
        meta: Value,
        caller: &CallerIdentity,
    ) -> Value {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args, "_meta": meta }
        });
        handle_rpc(&req, store, metrics, caller).expect("tools/call yields a response")
    }

    /// An attested caller whose holder string is `ns/agent`.
    fn attested(ns: &str, agent: &str) -> CallerIdentity {
        CallerIdentity::Attested(format!("{ns}/{agent}"))
    }

    // The JSON-RPC envelope + the EXACT work.claim result shape round-trip:
    // grant, contention, dedupe — structuredContent and the text fallback agree.
    #[test]
    fn claim_envelope_roundtrip_grant_contend_dedupe() {
        let (store, metrics) = ctx();

        // GRANT.
        let r = call(
            &store,
            &metrics,
            1,
            "work.claim",
            json!({ "item": "file:///x", "ttl_ms": 30_000 }),
            json!({ "agent/claim_key": "ck1", "agent/instance": "pod-1" }),
        );
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 1);
        let result = &r["result"];
        assert_eq!(result["isError"], false);
        let sc = &result["structuredContent"];
        assert_eq!(sc["granted"], true);
        let lease = sc["lease_id"].as_str().expect("lease_id").to_string();
        assert!(!lease.is_empty());
        assert_eq!(sc["expires_in_ms"], 30_000);
        // The text content[] carries the SAME json (the agent's fallback parse).
        let text = result["content"][0]["text"].as_str().expect("text content");
        let parsed: Value = serde_json::from_str(text).expect("text is json");
        assert_eq!(&parsed, sc);
        assert_eq!(result["content"][0]["type"], "text");

        // CONTENTION: same item still held ⇒ granted:false, held_by the holder,
        // and NOT isError (so the agent treats it as Lost, not Error).
        let r2 = call(
            &store,
            &metrics,
            2,
            "work.claim",
            json!({ "item": "file:///x", "ttl_ms": 30_000 }),
            json!({ "agent/claim_key": "ck1", "agent/instance": "pod-2" }),
        );
        let sc2 = &r2["result"]["structuredContent"];
        assert_eq!(sc2["granted"], false);
        assert_eq!(sc2["held_by"], "pod-1");
        assert_eq!(r2["result"]["isError"], false);

        // ack then re-claim ⇒ DEDUPE: granted:false, held_by "<acked>".
        let ack = call(
            &store,
            &metrics,
            3,
            "work.ack",
            json!({ "lease_id": lease }),
            json!({ "agent/claim_key": "ck1" }),
        );
        assert_eq!(ack["result"]["isError"], false);
        let r3 = call(
            &store,
            &metrics,
            4,
            "work.claim",
            json!({ "item": "file:///x", "ttl_ms": 30_000 }),
            json!({ "agent/claim_key": "ck1", "agent/instance": "pod-3" }),
        );
        let sc3 = &r3["result"]["structuredContent"];
        assert_eq!(sc3["granted"], false);
        assert_eq!(sc3["held_by"], "<acked>");
        assert_eq!(r3["result"]["isError"], false);
    }

    #[test]
    fn initialize_and_tools_list_advertise_work_tools() {
        let (store, metrics) = ctx();
        let init = handle_rpc(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            &store,
            &metrics,
            &CallerIdentity::Disabled,
        )
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(
            init["result"]["serverInfo"]["name"],
            "agentctl-coordination"
        );

        let tl = handle_rpc(
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            &store,
            &metrics,
            &CallerIdentity::Disabled,
        )
        .unwrap();
        let names: Vec<&str> = tl["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        // The agent's claim-server validation requires BOTH of these.
        assert!(names.contains(&"work.claim"));
        assert!(names.contains(&"work.ack"));
        for n in ["work.renew", "work.release", "work.submit", "work.stats"] {
            assert!(names.contains(&n), "missing tool {n}");
        }
    }

    #[test]
    fn resources_list_and_read_pending() {
        let (store, metrics) = ctx();
        store.submit("file:///a", Some("ka"), None);
        let rl = handle_rpc(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }),
            &store,
            &metrics,
            &CallerIdentity::Disabled,
        )
        .unwrap();
        assert_eq!(rl["result"]["resources"][0]["uri"], PENDING_URI);

        let rr = handle_rpc(
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": { "uri": PENDING_URI } }),
            &store,
            &metrics,
            &CallerIdentity::Disabled,
        )
        .unwrap();
        let text = rr["result"]["contents"][0]["text"].as_str().unwrap();
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["pending"], 1);
        assert!(v["items"].as_array().unwrap().contains(&json!("file:///a")));
    }

    #[test]
    fn submit_and_stats_tools_roundtrip() {
        let (store, metrics) = ctx();
        let s1 = call(
            &store,
            &metrics,
            1,
            "work.submit",
            json!({ "item": "p1", "claim_key": "kp1" }),
            Value::Null,
        );
        assert_eq!(s1["result"]["structuredContent"]["submitted"], true);
        // claim a different item, then stats sees 1 pending + 1 claimed.
        call(
            &store,
            &metrics,
            2,
            "work.claim",
            json!({ "item": "c1", "ttl_ms": 60_000 }),
            json!({ "agent/claim_key": "kc1" }),
        );
        let st = call(&store, &metrics, 3, "work.stats", json!({}), Value::Null);
        let sc = &st["result"]["structuredContent"];
        assert_eq!(sc["pending"], 1);
        assert_eq!(sc["claimed"], 1);
        assert!(sc["oldest_age_ms"].is_u64());
    }

    #[test]
    fn renew_ack_release_unknown_lease_sets_is_error() {
        let (store, metrics) = ctx();
        let renew = call(
            &store,
            &metrics,
            1,
            "work.renew",
            json!({ "lease_id": "nope", "ttl_ms": 1000 }),
            Value::Null,
        );
        assert_eq!(renew["result"]["isError"], true);
        let ack = call(
            &store,
            &metrics,
            2,
            "work.ack",
            json!({ "lease_id": "nope" }),
            json!({ "agent/claim_key": "fresh" }),
        );
        assert_eq!(ack["result"]["isError"], true);
        let rel = call(
            &store,
            &metrics,
            3,
            "work.release",
            json!({ "lease_id": "nope", "reason": "x" }),
            Value::Null,
        );
        assert_eq!(rel["result"]["isError"], true);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let (store, metrics) = ctx();
        let r = handle_rpc(
            &json!({ "jsonrpc": "2.0", "id": 9, "method": "bogus/thing" }),
            &store,
            &metrics,
            &CallerIdentity::Disabled,
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn notification_has_no_response() {
        let (store, metrics) = ctx();
        assert!(handle_rpc(
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            &store,
            &metrics,
            &CallerIdentity::Disabled,
        )
        .is_none());
        // A request missing its id is, by JSON-RPC, a notification ⇒ no response.
        assert!(handle_rpc(
            &json!({ "jsonrpc": "2.0", "method": "ping" }),
            &store,
            &metrics,
            &CallerIdentity::Disabled,
        )
        .is_none());
    }

    #[test]
    fn unknown_tool_is_error() {
        let (store, metrics) = ctx();
        let r = call(&store, &metrics, 1, "work.bogus", json!({}), Value::Null);
        assert_eq!(r["result"]["isError"], true);
    }

    // --- attested mode ----------------------------------------------------

    // In attested mode the lease HOLDER is the source-IP-attested identity, NOT the
    // self-asserted `_meta` agent: a tenant cannot bill/route the lease to another.
    #[test]
    fn attested_claim_binds_attested_holder_over_self_asserted() {
        let (store, metrics) = ctx();
        // The caller is attested as team-a/checkout but self-asserts a DIFFERENT
        // holder in `_meta`. The attested identity must win.
        let r = call_as(
            &store,
            &metrics,
            1,
            "work.claim",
            json!({ "item": "file:///x", "ttl_ms": 30_000 }),
            json!({ "agent/claim_key": "ck1", "agent/instance": "i-am-someone-else" }),
            &attested("team-a", "checkout"),
        );
        assert_eq!(r["result"]["structuredContent"]["granted"], true);
        // A contending claim sees the ATTESTED holder, not "i-am-someone-else".
        let r2 = call_as(
            &store,
            &metrics,
            2,
            "work.claim",
            json!({ "item": "file:///x", "ttl_ms": 30_000 }),
            json!({ "agent/claim_key": "ck1", "agent/instance": "other" }),
            &attested("team-b", "evil"),
        );
        assert_eq!(r2["result"]["structuredContent"]["granted"], false);
        assert_eq!(
            r2["result"]["structuredContent"]["held_by"],
            "team-a/checkout"
        );
    }

    // ack isolation: the holder may settle its own lease; a DIFFERENT attested
    // tenant cannot (mismatch ⇒ isError, lease untouched). caller==holder allows;
    // caller!=holder rejects.
    #[test]
    fn attested_ack_isolation_holder_allowed_other_rejected() {
        let (store, metrics) = ctx();
        let granted = call_as(
            &store,
            &metrics,
            1,
            "work.claim",
            json!({ "item": "file:///x", "ttl_ms": 30_000 }),
            json!({ "agent/claim_key": "ck1" }),
            &attested("team-a", "checkout"),
        );
        let lease = granted["result"]["structuredContent"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();

        // A DIFFERENT tenant cannot ack it (cannot settle/steal another's lease).
        let stolen = call_as(
            &store,
            &metrics,
            2,
            "work.ack",
            json!({ "lease_id": lease }),
            json!({ "agent/claim_key": "ck1" }),
            &attested("team-b", "evil"),
        );
        assert_eq!(stolen["result"]["isError"], true);

        // The lease is still held by team-a (the failed ack did not settle it):
        // a renew by the wrong tenant is also rejected...
        let bad_renew = call_as(
            &store,
            &metrics,
            3,
            "work.renew",
            json!({ "lease_id": lease, "ttl_ms": 1000 }),
            Value::Null,
            &attested("team-b", "evil"),
        );
        assert_eq!(bad_renew["result"]["isError"], true);

        // ...while the rightful holder renews and then acks successfully.
        let ok_renew = call_as(
            &store,
            &metrics,
            4,
            "work.renew",
            json!({ "lease_id": lease, "ttl_ms": 1000 }),
            Value::Null,
            &attested("team-a", "checkout"),
        );
        assert_eq!(ok_renew["result"]["isError"], false);
        let ok_ack = call_as(
            &store,
            &metrics,
            5,
            "work.ack",
            json!({ "lease_id": lease }),
            json!({ "agent/claim_key": "ck1" }),
            &attested("team-a", "checkout"),
        );
        assert_eq!(ok_ack["result"]["isError"], false);
        assert_eq!(ok_ack["result"]["structuredContent"]["acked"], true);
    }

    // release isolation: a different tenant cannot return another's lease to
    // pending; the rightful holder can.
    #[test]
    fn attested_release_isolation() {
        let (store, metrics) = ctx();
        let granted = call_as(
            &store,
            &metrics,
            1,
            "work.claim",
            json!({ "item": "file:///r", "ttl_ms": 30_000 }),
            json!({ "agent/claim_key": "ckr" }),
            &attested("team-a", "checkout"),
        );
        let lease = granted["result"]["structuredContent"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();
        let bad = call_as(
            &store,
            &metrics,
            2,
            "work.release",
            json!({ "lease_id": lease, "reason": "x" }),
            Value::Null,
            &attested("team-b", "evil"),
        );
        assert_eq!(bad["result"]["isError"], true);
        assert!(
            !store.pending_items().contains(&"file:///r".to_string()),
            "a rejected release must not return the item to pending"
        );
        let ok = call_as(
            &store,
            &metrics,
            3,
            "work.release",
            json!({ "lease_id": lease, "reason": "drain" }),
            Value::Null,
            &attested("team-a", "checkout"),
        );
        assert_eq!(ok["result"]["isError"], false);
        assert!(store.pending_items().contains(&"file:///r".to_string()));
    }

    // An UNATTESTABLE caller (source IP owns no pod) fails closed on EVERY
    // claim-lifecycle call (claim/ack/renew/release) in attested mode.
    #[test]
    fn attested_unresolved_caller_rejects_every_lifecycle_call() {
        let (store, metrics) = ctx();
        let unres = CallerIdentity::Unresolved;

        let claim = call_as(
            &store,
            &metrics,
            1,
            "work.claim",
            json!({ "item": "file:///x", "ttl_ms": 30_000 }),
            json!({ "agent/claim_key": "ck1" }),
            &unres,
        );
        assert_eq!(claim["result"]["isError"], true);
        // The claim was rejected ⇒ nothing is held.
        assert_eq!(store.stats().claimed, 0);

        for (id, name, args, meta) in [
            (
                2,
                "work.ack",
                json!({ "lease_id": "whatever" }),
                json!({ "agent/claim_key": "ck1" }),
            ),
            (
                3,
                "work.renew",
                json!({ "lease_id": "whatever", "ttl_ms": 1000 }),
                Value::Null,
            ),
            (
                4,
                "work.release",
                json!({ "lease_id": "whatever", "reason": "x" }),
                Value::Null,
            ),
        ] {
            let r = call_as(&store, &metrics, id, name, args, meta, &unres);
            assert_eq!(r["result"]["isError"], true, "{name} must fail closed");
        }
    }

    // work.submit / work.stats are NOT hard-blocked by attestation — an unresolved
    // caller still succeeds (producers/scaler may be external; token-gated only).
    #[test]
    fn attested_submit_and_stats_are_not_hard_blocked() {
        let (store, metrics) = ctx();
        let s = call_as(
            &store,
            &metrics,
            1,
            "work.submit",
            json!({ "item": "p1", "claim_key": "kp1" }),
            Value::Null,
            &CallerIdentity::Unresolved,
        );
        assert_eq!(s["result"]["isError"], false);
        assert_eq!(s["result"]["structuredContent"]["submitted"], true);

        let st = call_as(
            &store,
            &metrics,
            2,
            "work.stats",
            json!({}),
            Value::Null,
            &CallerIdentity::Unresolved,
        );
        assert_eq!(st["result"]["isError"], false);
        assert_eq!(st["result"]["structuredContent"]["pending"], 1);
    }
}
