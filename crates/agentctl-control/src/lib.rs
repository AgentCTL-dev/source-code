// SPDX-License-Identifier: BUSL-1.1
//! # The control MCP surface (RFC 0027 §5, P4-1)
//!
//! `control.*` — the governed tool surface a SUPERVISOR manages its owner's
//! estate through. Design stance:
//!
//! * **Identity first**: every call arrives AAuth-signed (RFC 9421 `jwt`
//!   scheme); the axum layer verifies it against the fleet Agent Provider's
//!   JWKS and hands this module the token's operator-registered workload
//!   label. No header-asserted identity is ever trusted.
//! * **Namespace-scoped by construction**: every tool operates in the
//!   VERIFIED caller's own namespace — the tools take no namespace argument
//!   at all, so cross-tenant access is unrepresentable on the wire.
//! * **The API server stays the gate**: writes go through the ordinary
//!   admission chain (policy ladder, tag laundering, handle uniqueness,
//!   quota) as the control server's OWN service account — a supervisor
//!   cannot smuggle a spec admission would refuse.
//! * **Narrow create**: `control.agents.create` accepts instruction +
//!   shape sugar + class — never image, services, access, or budget. Those
//!   come from the class (registry-governed) or stay platform defaults.
//!   Destructive verbs (delete/pause) are P4-5 (approval gates) — absent
//!   here on purpose.
//!
//! The dispatcher is `Value` in / `Value` out (same wire discipline as the
//! coordination server): `initialize`, `tools/list`, `tools/call`, with the
//! dual `structuredContent` + text `content[]` result shape agentd parses.

use agent_api::v1alpha2 as v2;
use kube::api::{Api, ListParams, PostParams};
use kube::ResourceExt as _;
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Label stamped on every agent created through this surface — the audit
/// anchor tying the CR to the creating workload.
pub const CREATED_BY_LABEL: &str = "agentctl.dev/created-by";

/// The verified caller as the axum layer resolved it: the token's agent id
/// plus its operator-registered workload (namespace, name).
#[derive(Clone, Debug)]
pub struct Caller {
    pub agent: String,
    pub namespace: String,
    pub name: String,
}

/// Dispatch one JSON-RPC message. `Some(response)` for a request; `None` for
/// a notification. Every message reaching here was signature-verified.
pub async fn handle_rpc(req: &Value, client: &kube::Client, caller: &Caller) -> Option<Value> {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method.starts_with("notifications/") {
        return None;
    }
    let id = req.get("id").cloned()?;
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let resp = match method {
        "initialize" => ok(id, initialize_result()),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tool_defs() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let empty = Value::Null;
            let args = params.get("arguments").unwrap_or(&empty);
            let (structured, is_error) = dispatch_tool(name, args, client, caller).await;
            ok(id, tool_result(structured, is_error))
        }
        other => err(id, -32601, &format!("method not found: {other}")),
    };
    Some(resp)
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "agentctl-control", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "agentctl control surface. Every tool operates in YOUR \
            namespace only. control.agents.list to see the estate; .get/.status \
            for one agent; .resolve to map an @handle to its name; .create for a \
            new agent (instruction + optional schedule/once + optional class).",
    })
}

fn tool_result(structured: Value, is_error: bool) -> Value {
    let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": is_error,
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}
fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The five P4-1 tool schemas. Read-only tools are annotated so; `create` is
/// the one write, and it is non-destructive.
pub fn tool_defs() -> Vec<Value> {
    let name_arg = json!({ "type": "string", "minLength": 1, "maxLength": 253 });
    vec![
        json!({
            "name": "control.agents.list",
            "title": "List your agents",
            "description": "Every agent in your namespace: name, handle, shape, phase.",
            "annotations": { "readOnlyHint": true, "idempotentHint": true },
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "control.agents.get",
            "title": "Inspect one agent",
            "description": "The agent's declared surface: instruction, triggers, class, services, principals — plus its status.",
            "annotations": { "readOnlyHint": true, "idempotentHint": true },
            "inputSchema": { "type": "object", "required": ["name"],
                "properties": { "name": name_arg }, "additionalProperties": false },
        }),
        json!({
            "name": "control.agents.status",
            "title": "Agent status",
            "description": "Phase and conditions for one agent.",
            "annotations": { "readOnlyHint": true, "idempotentHint": true },
            "inputSchema": { "type": "object", "required": ["name"],
                "properties": { "name": name_arg }, "additionalProperties": false },
        }),
        json!({
            "name": "control.agents.resolve",
            "title": "Resolve an @handle",
            "description": "Map an org-unique @handle to the agent's resource name.",
            "annotations": { "readOnlyHint": true, "idempotentHint": true },
            "inputSchema": { "type": "object", "required": ["handle"],
                "properties": { "handle": name_arg }, "additionalProperties": false },
        }),
        json!({
            "name": "control.agents.create",
            "title": "Create an agent",
            "description": "Create a NEW agent in your namespace. Give it a DNS-safe \
                name and an instruction; optionally `once: true` (run to completion), \
                a `schedule` (cron or `every` like 1h), and a `class` from your org's \
                registry. Tools, budgets and identity come from the class — never \
                from this call. The platform's admission policies still apply.",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false },
            "inputSchema": { "type": "object", "required": ["name", "instruction"],
                "properties": {
                    "name": name_arg,
                    "instruction": { "type": "string", "minLength": 1, "maxLength": 16384 },
                    "once": { "type": "boolean" },
                    "schedule": { "type": "string", "maxLength": 128,
                        "description": "Cron (`0 7 * * 1-5`) or an interval (`1h`)." },
                    "class": name_arg,
                    "handle": name_arg,
                }, "additionalProperties": false },
        }),
    ]
}

/// Build the restricted Agent a `control.agents.create` call renders to.
/// Pure — the narrow-surface rule lives here and is unit-tested.
pub fn build_created_agent(
    caller: &Caller,
    name: &str,
    instruction: &str,
    once: bool,
    schedule: Option<&str>,
    class: Option<&str>,
    handle: Option<&str>,
) -> Result<v2::Agent, String> {
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.is_empty()
        || name.len() > 63
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err("name must be DNS-1123 (lowercase alphanumeric + '-', ≤63)".into());
    }
    if once && schedule.is_some() {
        return Err("once and schedule are mutually exclusive".into());
    }
    let mut triggers = Vec::new();
    if once {
        triggers.push(v2::Trigger {
            once: Some(v2::OnceTrigger::default()),
            ..Default::default()
        });
    }
    let mut is_cron = false;
    if let Some(s) = schedule {
        // Cron if it looks like one (5 fields), else interval sugar.
        let sched = if s.split_whitespace().count() >= 5 {
            is_cron = true;
            v2::ScheduleTrigger {
                cron: Some(s.to_string()),
                ..Default::default()
            }
        } else {
            v2::ScheduleTrigger {
                every: Some(s.to_string()),
                ..Default::default()
            }
        };
        triggers.push(v2::Trigger {
            schedule: Some(sched),
            ..Default::default()
        });
    }
    // Mirrors the compiler's inference (CLI create.rs): only-once ⇒ Job; a
    // sole CRON schedule ⇒ CronJob; anything else (incl. `every` sugar, which
    // cannot be a CronJob) ⇒ daemon.
    let shape = if once {
        v2::Shape::Job
    } else if is_cron {
        v2::Shape::Cron
    } else {
        v2::Shape::Daemon
    };
    let mut agent = v2::Agent::new(
        name,
        v2::AgentSpec {
            class: class.map(str::to_string),
            handle: handle.map(str::to_string),
            shape,
            instruction: Some(v2::Instruction {
                text: Some(instruction.to_string()),
                config_map_ref: None,
            }),
            triggers,
            ..Default::default()
        },
    );
    agent.metadata.namespace = Some(caller.namespace.clone());
    agent.metadata.labels = Some(
        [(
            CREATED_BY_LABEL.to_string(),
            // Label values are constrained; the workload NAME is DNS-safe.
            caller.name.clone(),
        )]
        .into(),
    );
    Ok(agent)
}

/// One list row: the estate summary a supervisor narrates from.
fn agent_row(a: &v2::Agent) -> Value {
    json!({
        "name": a.name_any(),
        "handle": a.spec.handle,
        "displayName": a.spec.display_name,
        "class": a.spec.class,
        "shape": a.spec.shape,
        "phase": a.status.as_ref().and_then(|s| s.phase.clone()),
    })
}

async fn dispatch_tool(
    name: &str,
    args: &Value,
    client: &kube::Client,
    caller: &Caller,
) -> (Value, bool) {
    let agents: Api<v2::Agent> = Api::namespaced(client.clone(), &caller.namespace);
    let fail = |msg: String| (json!({ "error": msg }), true);
    let arg = |key: &str| args.get(key).and_then(Value::as_str);

    match name {
        "control.agents.list" => match agents.list(&ListParams::default()).await {
            Ok(list) => {
                let rows: Vec<Value> = list.items.iter().map(agent_row).collect();
                (
                    json!({ "agents": rows, "namespace": caller.namespace }),
                    false,
                )
            }
            Err(e) => fail(format!("list agents: {e}")),
        },
        "control.agents.get" => {
            let Some(target) = arg("name") else {
                return fail("name is required".into());
            };
            match agents.get_opt(target).await {
                Ok(Some(a)) => {
                    let mut row = agent_row(&a);
                    if let Value::Object(m) = &mut row {
                        m.insert("instruction".into(), json!(a.spec.instruction));
                        m.insert("triggers".into(), json!(a.spec.triggers));
                        m.insert(
                            "services".into(),
                            json!(a
                                .spec
                                .services
                                .iter()
                                .map(|s| s.name.clone())
                                .collect::<Vec<_>>()),
                        );
                        m.insert(
                            "principals".into(),
                            json!(a.spec.access.as_ref().map(|x| x.principals.clone())),
                        );
                        m.insert("status".into(), json!(a.status));
                    }
                    (row, false)
                }
                Ok(None) => fail(format!("no agent {target:?} in your namespace")),
                Err(e) => fail(format!("get agent: {e}")),
            }
        }
        "control.agents.status" => {
            let Some(target) = arg("name") else {
                return fail("name is required".into());
            };
            match agents.get_opt(target).await {
                Ok(Some(a)) => (
                    json!({
                        "name": a.name_any(),
                        "phase": a.status.as_ref().and_then(|s| s.phase.clone()),
                        "conditions": a.status.as_ref().map(|s| s.conditions.clone()),
                    }),
                    false,
                ),
                Ok(None) => fail(format!("no agent {target:?} in your namespace")),
                Err(e) => fail(format!("get agent: {e}")),
            }
        }
        "control.agents.resolve" => {
            let Some(handle) = arg("handle") else {
                return fail("handle is required".into());
            };
            let handle = handle.trim_start_matches('@');
            match agents.list(&ListParams::default()).await {
                Ok(list) => match list
                    .items
                    .iter()
                    .find(|a| a.spec.handle.as_deref().unwrap_or(&a.name_any()) == handle)
                {
                    Some(a) => (json!({ "name": a.name_any(), "handle": handle }), false),
                    None => fail(format!("no agent holds the handle @{handle}")),
                },
                Err(e) => fail(format!("list agents: {e}")),
            }
        }
        "control.agents.create" => {
            let (Some(new_name), Some(instruction)) = (arg("name"), arg("instruction")) else {
                return fail("name and instruction are required".into());
            };
            let agent = match build_created_agent(
                caller,
                new_name,
                instruction,
                args.get("once").and_then(Value::as_bool).unwrap_or(false),
                arg("schedule"),
                arg("class"),
                arg("handle"),
            ) {
                Ok(a) => a,
                Err(e) => return fail(e),
            };
            // Plain create (not apply): a name collision is an error the
            // supervisor should hear about, never a silent overwrite.
            match agents.create(&PostParams::default(), &agent).await {
                Ok(a) => (
                    json!({ "created": a.name_any(), "namespace": caller.namespace }),
                    false,
                ),
                // Admission refusals carry the policy message — surface it
                // verbatim: it names floors/holders and is the useful part.
                Err(kube::Error::Api(e)) => fail(format!("refused: {}", e.message)),
                Err(e) => fail(format!("create agent: {e}")),
            }
        }
        other => fail(format!("unknown tool: {other}")),
    }
}

/// Tool-count sanity + the pure builder rules, pinned.
#[cfg(test)]
mod tests {
    use super::*;

    fn caller() -> Caller {
        Caller {
            agent: "aauth:abc@ap".into(),
            namespace: "org-acme".into(),
            name: "sup-mock-alice".into(),
        }
    }

    #[test]
    fn tool_surface_is_the_five_read_mostly_verbs() {
        let defs = tool_defs();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "control.agents.list",
                "control.agents.get",
                "control.agents.status",
                "control.agents.resolve",
                "control.agents.create",
            ]
        );
        // Exactly one write, and it is non-destructive (P4-5 owns delete).
        let writes: Vec<&Value> = defs
            .iter()
            .filter(|d| d["annotations"]["readOnlyHint"] != json!(true))
            .collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["annotations"]["destructiveHint"], json!(false));
    }

    #[test]
    fn created_agent_is_caller_scoped_and_narrow() {
        let a = build_created_agent(
            &caller(),
            "digest",
            "summarize the day",
            false,
            Some("0 7 * * 1-5"),
            Some("research"),
            Some("digest"),
        )
        .unwrap();
        assert_eq!(a.metadata.namespace.as_deref(), Some("org-acme"));
        assert_eq!(
            a.metadata.labels.as_ref().unwrap()[CREATED_BY_LABEL],
            "sup-mock-alice"
        );
        assert_eq!(a.spec.class.as_deref(), Some("research"));
        let t = &a.spec.triggers[0];
        assert_eq!(
            t.schedule.as_ref().unwrap().cron.as_deref(),
            Some("0 7 * * 1-5")
        );
        // The narrow surface: no image, no services, no access, no budget.
        assert!(a.spec.runtime.is_none());
        assert!(a.spec.services.is_empty());
        assert!(a.spec.access.is_none());
        assert!(a.spec.intelligence.is_none());

        // Interval sugar goes to `every`, not `cron`.
        let b = build_created_agent(&caller(), "tick", "x", false, Some("1h"), None, None).unwrap();
        assert_eq!(
            b.spec.triggers[0]
                .schedule
                .as_ref()
                .unwrap()
                .every
                .as_deref(),
            Some("1h")
        );

        // Guard rails.
        assert!(build_created_agent(&caller(), "Bad_Name", "x", false, None, None, None).is_err());
        assert!(build_created_agent(&caller(), "x", "i", true, Some("1h"), None, None).is_err());
    }
}
