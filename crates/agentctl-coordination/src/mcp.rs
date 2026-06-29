// SPDX-License-Identifier: BUSL-1.1
//! The MCP JSON-RPC 2.0 wire layer — the server side of the FROZEN `work.*`
//! contract (agentd RFC 0015 §5.6, the names + `_meta` keys; the conformant agent
//! is `agentd crates/agentd/src/cluster/claim.rs`).
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

use crate::metrics::Metrics;
use crate::store::{ClaimResult, ClaimStore, SubmitOutcome};

/// The MCP protocol version this server advertises (matches the agentd self-server
/// target, agentd RFC 0004/0005; interop is by capability, not strict version).
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// The countable backlog resource — the scale-from-zero signal (P9) the future
/// KEDA external scaler reads. Known at ZERO pods because it lives on the server.
pub const PENDING_URI: &str = "work://pending";

/// The frozen `_meta` key carrying the item-derived dedupe key (agentd RFC 0015
/// §5.6). Present on `work.claim` and `work.ack`.
const META_CLAIM_KEY: &str = "agent/claim_key";

/// Dispatch one JSON-RPC message. `Some(response)` for a request; `None` for a
/// notification (no `id` / `notifications/*`) — the caller returns 202 with no
/// body. The store is the serializing point; this layer only translates wire ⇄
/// store and counts metrics.
pub fn handle_rpc(req: &Value, store: &dyn ClaimStore, metrics: &Metrics) -> Option<Value> {
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
        "tools/call" => tools_call(id, &params, store, metrics),
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
        "instructions": "Reference coordination MCP server (agentctl RFC 0011 §3.2). \
            Call work.claim before processing an item; work.renew at ttl/3; \
            work.ack on success (terminal, dedupes the claim_key); work.release on \
            wind-down. Read work://pending or call work.stats for the backlog.",
    })
}

/// `tools/call` → run the named tool, wrap its outcome in the dual
/// `content`/`structuredContent` result the agent parses.
fn tools_call(id: Value, params: &Value, store: &dyn ClaimStore, metrics: &Metrics) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let empty = Value::Null;
    let args = params.get("arguments").unwrap_or(&empty);
    let meta = params.get("_meta").unwrap_or(&empty);
    let (structured, is_error) = dispatch_tool(name, args, meta, store, metrics);
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
) -> (Value, bool) {
    match name {
        "work.claim" => tool_claim(args, meta, store, metrics),
        "work.renew" => tool_renew(args, store, metrics),
        "work.ack" => tool_ack(args, meta, store, metrics),
        "work.release" => tool_release(args, store, metrics),
        "work.submit" => tool_submit(args, store, metrics),
        "work.stats" => tool_stats(store),
        other => (json!({ "error": format!("unknown tool: {other}") }), true),
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
    let holder = holder_of(meta);
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
    }
}

/// `work.renew` — extend a live, owned lease. arguments `{lease_id, ttl_ms}`.
fn tool_renew(args: &Value, store: &dyn ClaimStore, metrics: &Metrics) -> (Value, bool) {
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
    match store.renew(lease_id, ttl_ms) {
        Ok(()) => {
            metrics.inc_renewed();
            (
                json!({ "ok": true, "renewed": true, "expires_in_ms": ttl_ms }),
                false,
            )
        }
        Err(e) => (json!({ "ok": false, "error": e }), true),
    }
}

/// `work.ack` — terminal settle + record the `claim_key` as done (dedupe set).
/// arguments `{lease_id}`, `_meta {agent/claim_key}`. Idempotent.
fn tool_ack(
    args: &Value,
    meta: &Value,
    store: &dyn ClaimStore,
    metrics: &Metrics,
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
    match store.ack(lease_id, claim_key) {
        Ok(()) => {
            metrics.inc_acked();
            (json!({ "ok": true, "acked": true }), false)
        }
        Err(e) => (json!({ "ok": false, "error": e }), true),
    }
}

/// `work.release` — return the item to pending (re-claimable). arguments
/// `{lease_id, reason}`.
fn tool_release(args: &Value, store: &dyn ClaimStore, metrics: &Metrics) -> (Value, bool) {
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
    match store.release(lease_id, reason) {
        Ok(()) => {
            metrics.inc_released();
            (json!({ "ok": true, "released": true }), false)
        }
        Err(e) => (json!({ "ok": false, "error": e }), true),
    }
}

/// `work.submit` — enqueue an item into the backlog (producer side). arguments
/// `{item, claim_key?}`. Skips if `claim_key` is already done (dedupe).
fn tool_submit(args: &Value, store: &dyn ClaimStore, metrics: &Metrics) -> (Value, bool) {
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
    match store.submit(item, claim_key) {
        SubmitOutcome::Enqueued => {
            metrics.inc_submitted();
            (json!({ "submitted": true, "deduped": false }), false)
        }
        SubmitOutcome::Deduped => (json!({ "submitted": false, "deduped": true }), false),
        SubmitOutcome::AlreadyPending => (
            json!({ "submitted": false, "deduped": false, "reason": "already_pending" }),
            false,
        ),
        SubmitOutcome::AlreadyClaimed => (
            json!({ "submitted": false, "deduped": false, "reason": "already_claimed" }),
            false,
        ),
    }
}

/// `work.stats` — the off-pod backlog snapshot (P9).
fn tool_stats(store: &dyn ClaimStore) -> (Value, bool) {
    let s = store.stats();
    (
        json!({ "pending": s.pending, "claimed": s.claimed, "oldest_age_ms": s.oldest_age_ms }),
        false,
    )
}

/// `resources/read` — only `work://pending` is served: the pending count + items,
/// the from-zero scale signal. The body JSON rides the `text` field (which the
/// agent's resource reader concatenates).
fn resources_read(id: Value, params: &Value, store: &dyn ClaimStore) -> Value {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if uri == PENDING_URI {
        let items = store.pending_items();
        let body = json!({ "pending": items.len(), "items": items });
        let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        ok(
            id,
            json!({
                "contents": [{
                    "uri": PENDING_URI,
                    "mimeType": "application/json",
                    "text": text,
                }],
            }),
        )
    } else {
        err(id, -32602, &format!("unknown resource uri: {uri}"))
    }
}

/// The advertised tools (`tools/list`). The agent validates a coordination server
/// by requiring BOTH `work.claim` and `work.ack` here (agentd RFC 0019 §3.6).
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
            "Settle a lease (terminal) and record its claim_key as done (dedupe).",
            json!({
                "type": "object",
                "properties": { "lease_id": { "type": "string" } },
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
            "Enqueue an item into the backlog (skipped if its claim_key is done).",
            json!({
                "type": "object",
                "properties": {
                    "item": { "type": "string" },
                    "claim_key": { "type": "string" }
                },
                "required": ["item"]
            }),
        ),
        tool_def(
            "work.stats",
            "Backlog snapshot: { pending, claimed, oldest_age_ms } (the P9 from-zero signal).",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

/// One `tools/list` entry.
fn tool_def(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

/// The advertised resources (`resources/list`).
fn resource_defs() -> Vec<Value> {
    vec![json!({
        "uri": PENDING_URI,
        "name": "pending",
        "description": "Pending (unclaimed) item count + list — the off-pod scale-from-zero signal (P9).",
        "mimeType": "application/json",
    })]
}

/// Holder identity for `held_by`: prefer the frozen `agent/instance`, then
/// `agent/run_id` (agentd RFC 0015 §5.6); else `anonymous`.
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

    fn call(
        store: &dyn ClaimStore,
        metrics: &Metrics,
        id: i64,
        name: &str,
        args: Value,
        meta: Value,
    ) -> Value {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args, "_meta": meta }
        });
        handle_rpc(&req, store, metrics).expect("tools/call yields a response")
    }

    // (7) The JSON-RPC envelope + the EXACT work.claim result shape round-trip:
    //     grant, contention, dedupe — structuredContent and the text fallback agree.
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
        store.submit("file:///a", Some("ka"));
        let rl = handle_rpc(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }),
            &store,
            &metrics,
        )
        .unwrap();
        assert_eq!(rl["result"]["resources"][0]["uri"], PENDING_URI);

        let rr = handle_rpc(
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": { "uri": PENDING_URI } }),
            &store,
            &metrics,
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
        )
        .is_none());
        // A request missing its id is, by JSON-RPC, a notification ⇒ no response.
        assert!(handle_rpc(
            &json!({ "jsonrpc": "2.0", "method": "ping" }),
            &store,
            &metrics,
        )
        .is_none());
    }

    #[test]
    fn unknown_tool_is_error() {
        let (store, metrics) = ctx();
        let r = call(&store, &metrics, 1, "work.bogus", json!({}), Value::Null);
        assert_eq!(r["result"]["isError"], true);
    }
}
