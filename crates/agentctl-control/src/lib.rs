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

/// Who a call ultimately acts for (P4-2 binding check). Resolved SERVER-SIDE
/// from the Supervisor CR named like the calling workload — the wire carries
/// no acting-for claim to trust or forge. A workload without a Supervisor CR
/// acts as itself (service context — its authority came from the class grant).
#[derive(Clone, Debug, Default)]
pub struct Acting {
    /// The bound owner (`Supervisor.spec.user`), when the caller IS a
    /// supervisor.
    pub user: Option<String>,
    /// The owner's identity-resolved groups — stamped onto the Supervisor's
    /// status by the GATEWAY at introspection time, never self-asserted.
    pub groups: Vec<String>,
}

/// The role a tool demands, per the org role doc: `viewer` reads
/// specs/status; `admin` adds CRUD — so every read tool is viewer-tier and
/// `create` is admin-tier. (`operator`'s converse/lifecycle verbs live on
/// the A2A surface, not here.)
pub fn required_role(tool: &str) -> agent_api::org::Role {
    match tool {
        // Estate CRUD is admin-tier; a bounded, self-terminating child task
        // is operator-tier (the converse/lifecycle band). Delete is
        // admin-tier AND approval-gated on top (P4-5).
        "control.agents.create" | "control.agents.delete" => agent_api::org::Role::Admin,
        "control.subagents.create" => agent_api::org::Role::Operator,
        _ => agent_api::org::Role::Viewer,
    }
}

/// The label a spawned child carries naming its parent workload — both the
/// audit trail and the DEPTH CEILING: a workload carrying it cannot spawn
/// again (depth 1 across instances; agentd's own in-process depth limit does
/// not cross pod boundaries).
pub const PARENT_LABEL: &str = "agentctl.dev/parent";

// P4-5 approval markers: the shared grammar lives in [`agent_api::approval`]
// (the gateway writes the approved marker; this server writes pending and
// executes on a verified approval).
pub use agent_api::approval::{
    approval_marker, parse_approval, APPROVAL_TTL_SECS, APPROVED_DELETE_ANNOTATION,
    PENDING_DELETE_ANNOTATION,
};

/// Build a governed CHILD agent (P4-6). Pure. Caps enforced here:
/// run-to-completion only (`once`), the parent's lifetime-token budget is a
/// CEILING the child request narrows (a parent with a budget can never mint
/// a child above it), owner-referenced to the parent (GC cascades), and a
/// child of a child is refused before any API call.
pub fn build_subagent(
    caller: &Caller,
    parent: &v2::Agent,
    name: &str,
    instruction: &str,
    budget_tokens: Option<i64>,
    class: Option<&str>,
) -> Result<v2::Agent, String> {
    if parent
        .metadata
        .labels
        .as_ref()
        .is_some_and(|l| l.contains_key(PARENT_LABEL))
    {
        return Err("depth ceiling: a spawned subagent cannot spawn its own".into());
    }
    let parent_budget = parent
        .spec
        .intelligence
        .as_ref()
        .and_then(|i| i.budget.as_ref())
        .and_then(|b| b.lifetime_tokens);
    let effective_budget = match (budget_tokens, parent_budget) {
        (Some(req), Some(cap)) => Some(req.min(cap)),
        (Some(req), None) => Some(req),
        (None, cap) => cap, // inherit the ceiling — never unbounded past the parent
    };
    let mut child = build_created_agent(caller, name, instruction, true, None, class, None)?;
    if let Some(tokens) = effective_budget {
        child.spec.intelligence = Some(v2::Intelligence {
            budget: Some(v2::Budget {
                lifetime_tokens: Some(tokens),
                windows: Vec::new(),
            }),
            ..Default::default()
        });
    }
    child
        .metadata
        .labels
        .get_or_insert_with(Default::default)
        .insert(PARENT_LABEL.into(), caller.name.clone());
    child.metadata.owner_references = Some(vec![
        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "agentctl.dev/v1alpha2".into(),
            kind: "Agent".into(),
            name: parent.name_any(),
            uid: parent.metadata.uid.clone().unwrap_or_default(),
            controller: Some(false),
            block_owner_deletion: Some(true),
        },
    ]);
    Ok(child)
}

/// Evaluate the acting USER against the org's accessPolicies. `None` = the
/// org is ungoverned (no policies — P1-8 convention: no scoping) or the
/// caller is a service workload; both pass. A governed org refuses a bound
/// user whose grants do not reach `required` — including the user nothing
/// matches (zero grants).
pub fn user_permits(
    acting: &Acting,
    policies: &[agent_api::org::AccessPolicy],
    required: agent_api::org::Role,
) -> bool {
    use agent_api::org::access;
    let Some(user) = &acting.user else {
        return true; // service context
    };
    if policies.is_empty() {
        return true; // ungoverned org
    }
    let facts = access::PrincipalFacts {
        groups: acting.groups.clone(),
        claims: [("sub".to_string(), serde_json::json!(user))].into(),
    };
    let grants = access::resolve(&facts, policies);
    // Org-wide check (empty label scope): label-scoped grants deliberately
    // do not authorize control-surface writes.
    access::permits(&grants, required, &std::collections::BTreeMap::new())
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
            // P4-2: bind the acting user (Supervisor CR lookup — server-side,
            // unforgeable) and enforce the org's accessPolicies BEFORE any
            // tool logic runs. Every call is attributed in the audit log.
            let acting = resolve_acting(client, caller).await;
            tracing::info!(
                workload = %caller.name,
                ns = %caller.namespace,
                acting_for = acting.user.as_deref().unwrap_or("(service)"),
                tool = name,
                "control tool call"
            );
            let policies = org_policies(client, &caller.namespace).await;
            if !user_permits(&acting, &policies, required_role(name)) {
                let denied = json!({
                    "error": format!(
                        "{} requires the {:?} role in this organization, and {} does not hold it",
                        name,
                        required_role(name),
                        acting.user.as_deref().unwrap_or("the acting user"),
                    )
                });
                return Some(ok(id, tool_result(denied, true)));
            }
            let (structured, is_error) = dispatch_tool(name, args, client, caller, &acting).await;
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
            "name": "control.subagents.create",
            "title": "Spawn a subagent",
            "description": "Spawn a GOVERNED child task: a run-to-completion agent \
                owned by YOU (it is garbage-collected with you). Your own lifetime \
                token budget is the ceiling — a child can only narrow it — and a \
                spawned child cannot spawn again. Give it a DNS-safe name and an \
                instruction; optionally a budgetTokens cap and a class.",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false },
            "inputSchema": { "type": "object", "required": ["name", "instruction"],
                "properties": {
                    "name": name_arg,
                    "instruction": { "type": "string", "minLength": 1, "maxLength": 16384 },
                    "budgetTokens": { "type": "integer", "minimum": 1 },
                    "class": name_arg,
                }, "additionalProperties": false },
        }),
        json!({
            "name": "control.agents.delete",
            "title": "Delete an agent (owner-approved)",
            "description": "Delete one agent — DESTRUCTIVE and approval-gated: the \
                first call returns a pending approval your HUMAN must confirm out of \
                band (they run `agentctl approve <org> <nonce>`, or open the approval \
                link); call again after they approve and the delete executes. You \
                cannot approve on their behalf — relay the nonce and wait.",
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false },
            "inputSchema": { "type": "object", "required": ["name"],
                "properties": { "name": name_arg }, "additionalProperties": false },
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

/// The P4-2 binding check: a caller whose workload name matches a Supervisor
/// CR in its own namespace acts FOR that CR's user, with the gateway-stamped
/// owner groups. Lookup failures degrade to service context (logged) — the
/// alternative, refusing every call on a transient apiserver blip, would
/// take the whole estate surface down.
async fn resolve_acting(client: &kube::Client, caller: &Caller) -> Acting {
    let sups: Api<v2::Supervisor> = Api::namespaced(client.clone(), &caller.namespace);
    match sups.get_opt(&caller.name).await {
        Ok(Some(sup)) => Acting {
            user: Some(sup.spec.user.clone()),
            groups: sup
                .status
                .as_ref()
                .map(|s| s.owner_groups.clone())
                .unwrap_or_default(),
        },
        Ok(None) => Acting::default(),
        Err(e) => {
            tracing::warn!(error = %e, workload = %caller.name, "supervisor binding lookup failed; treating as service context");
            Acting::default()
        }
    }
}

/// The org's accessPolicies for a managed namespace (label
/// `agentctl.dev/organization` → cluster-scoped Organization). Absent org or
/// lookup failure ⇒ empty (ungoverned) — the same posture as an org that set
/// no policies.
async fn org_policies(client: &kube::Client, namespace: &str) -> Vec<agent_api::org::AccessPolicy> {
    use k8s_openapi::api::core::v1::Namespace;
    let ns_api: Api<Namespace> = Api::all(client.clone());
    let org_name = match ns_api.get_opt(namespace).await {
        Ok(Some(ns)) => ns
            .metadata
            .labels
            .and_then(|l| l.get("agentctl.dev/organization").cloned()),
        _ => None,
    };
    let Some(org_name) = org_name else {
        return Vec::new();
    };
    let orgs: Api<agent_api::Organization> = Api::all(client.clone());
    match orgs.get_opt(&org_name).await {
        Ok(Some(org)) => org.spec.access_policies,
        _ => Vec::new(),
    }
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
    acting: &Acting,
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
        "control.agents.delete" => {
            let Some(target) = arg("name") else {
                return fail("name is required".into());
            };
            // Destructive verbs require a bound HUMAN (P4-5): a service
            // workload has nobody to ask, so it cannot delete at all.
            let Some(user) = acting.user.clone() else {
                return fail(
                    "delete requires a supervisor bound to a human owner (service \
                     workloads cannot delete)"
                        .into(),
                );
            };
            let agent = match agents.get_opt(target).await {
                Ok(Some(a)) => a,
                Ok(None) => return fail(format!("no agent {target:?} in your namespace")),
                Err(e) => return fail(format!("get agent: {e}")),
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let annotations = agent.metadata.annotations.clone().unwrap_or_default();
            // Approved by the owner (via the gateway) and fresh → execute.
            if let Some((nonce, approver, exp)) = annotations
                .get(APPROVED_DELETE_ANNOTATION)
                .and_then(|v| parse_approval(v))
            {
                if approver == user && exp > now {
                    return match agents.delete(target, &Default::default()).await {
                        Ok(_) => {
                            tracing::info!(
                                target,
                                requested_by = %user,
                                approved_nonce = %nonce,
                                "approved delete executed"
                            );
                            (json!({ "deleted": target, "approvedBy": approver }), false)
                        }
                        Err(e) => fail(format!("delete: {e}")),
                    };
                }
            }
            // Standing pending request → remind, don't re-mint (the nonce the
            // human was told stays valid for the window).
            if let Some((nonce, requester, exp)) = annotations
                .get(PENDING_DELETE_ANNOTATION)
                .and_then(|v| parse_approval(v))
            {
                if requester == user && exp > now {
                    return (
                        json!({
                            "pending": nonce,
                            "expiresInSeconds": exp - now,
                            "message": format!(
                                "waiting for your owner's approval — ask them to run \
                                 `agentctl approve` with nonce {nonce} (or approve in \
                                 the console), then call this tool again"
                            ),
                        }),
                        false,
                    );
                }
            }
            // Mint a fresh pending approval on the target.
            let nonce = {
                use ring::rand::SecureRandom as _;
                let mut b = [0u8; 8];
                let _ = ring::rand::SystemRandom::new().fill(&mut b);
                b.iter().map(|x| format!("{x:02x}")).collect::<String>()
            };
            let marker = approval_marker(&nonce, &user, now + APPROVAL_TTL_SECS);
            let patch = json!({ "metadata": { "annotations": {
                PENDING_DELETE_ANNOTATION: marker,
                APPROVED_DELETE_ANNOTATION: null,
            } } });
            if let Err(e) = agents
                .patch(
                    target,
                    &kube::api::PatchParams::default(),
                    &kube::api::Patch::Merge(&patch),
                )
                .await
            {
                return fail(format!("record pending delete: {e}"));
            }
            tracing::info!(target, requested_by = %user, nonce = %nonce, "delete pending owner approval");
            (
                json!({
                    "pending": nonce,
                    "expiresInSeconds": APPROVAL_TTL_SECS,
                    "message": format!(
                        "deletion of {target:?} needs YOUR OWNER's approval: tell them \
                         the approval code {nonce}; once they approve, call this tool \
                         again"
                    ),
                }),
                false,
            )
        }
        "control.subagents.create" => {
            let (Some(new_name), Some(instruction)) = (arg("name"), arg("instruction")) else {
                return fail("name and instruction are required".into());
            };
            // Only a rendered agent can spawn — the parent CR anchors the
            // ownership, the budget ceiling, and the depth check.
            let parent = match agents.get_opt(&caller.name).await {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return fail(format!(
                        "no Agent {:?} in your namespace — only rendered agents spawn subagents",
                        caller.name
                    ))
                }
                Err(e) => return fail(format!("parent lookup: {e}")),
            };
            let mut child = match build_subagent(
                caller,
                &parent,
                new_name,
                instruction,
                args.get("budgetTokens").and_then(Value::as_i64),
                arg("class"),
            ) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            if let Some(user) = &acting.user {
                child
                    .metadata
                    .annotations
                    .get_or_insert_with(Default::default)
                    .insert("agentctl.dev/created-for".into(), user.clone());
            }
            match agents.create(&PostParams::default(), &child).await {
                Ok(a) => (
                    json!({ "created": a.name_any(), "owner": caller.name,
                            "budgetTokens": a.spec.intelligence.as_ref()
                                .and_then(|i| i.budget.as_ref())
                                .and_then(|b| b.lifetime_tokens) }),
                    false,
                ),
                Err(kube::Error::Api(e)) => fail(format!("refused: {}", e.message)),
                Err(e) => fail(format!("create subagent: {e}")),
            }
        }
        "control.agents.create" => {
            let (Some(new_name), Some(instruction)) = (arg("name"), arg("instruction")) else {
                return fail("name and instruction are required".into());
            };
            let mut agent = match build_created_agent(
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
            // Attribution (P4-2): the acting USER rides an annotation beside
            // the workload's created-by label.
            if let Some(user) = &acting.user {
                agent
                    .metadata
                    .annotations
                    .get_or_insert_with(Default::default)
                    .insert("agentctl.dev/created-for".into(), user.clone());
            }
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
    fn tool_surface_is_the_seven_governed_verbs() {
        let defs = tool_defs();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "control.agents.list",
                "control.agents.get",
                "control.agents.status",
                "control.agents.resolve",
                "control.subagents.create",
                "control.agents.delete",
                "control.agents.create",
            ]
        );
        // Exactly ONE destructive verb, and it is the approval-gated delete.
        let destructive: Vec<&Value> = defs
            .iter()
            .filter(|d| d["annotations"]["destructiveHint"] == json!(true))
            .collect();
        assert_eq!(destructive.len(), 1);
        assert_eq!(destructive[0]["name"], json!("control.agents.delete"));
    }

    /// P4-6: the child inherits the parent as a budget CEILING, is owned by
    /// it, carries the parent label, and depth stops at one.
    #[test]
    fn subagent_caps_are_enforced() {
        let mut parent = v2::Agent::new(
            "sup-mock-alice",
            v2::AgentSpec {
                intelligence: Some(v2::Intelligence {
                    budget: Some(v2::Budget {
                        lifetime_tokens: Some(50_000),
                        windows: Vec::new(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        parent.metadata.uid = Some("uid-1".into());
        let c = caller();

        // Request above the ceiling → clamped to it.
        let child =
            build_subagent(&c, &parent, "child", "do one thing", Some(999_999), None).unwrap();
        assert_eq!(
            child
                .spec
                .intelligence
                .unwrap()
                .budget
                .unwrap()
                .lifetime_tokens,
            Some(50_000)
        );

        // No request → the ceiling is inherited, never unbounded.
        let child = build_subagent(&c, &parent, "child", "x", None, None).unwrap();
        assert_eq!(
            child
                .spec
                .intelligence
                .unwrap()
                .budget
                .unwrap()
                .lifetime_tokens,
            Some(50_000)
        );

        // Owned + labelled + run-to-completion.
        let child = build_subagent(&c, &parent, "child", "x", Some(10), None).unwrap();
        assert_eq!(child.spec.shape, v2::Shape::Job);
        assert!(child.spec.triggers[0].once.is_some());
        let owner = &child.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!(
            (owner.kind.as_str(), owner.uid.as_str()),
            ("Agent", "uid-1")
        );
        assert_eq!(
            child.metadata.labels.as_ref().unwrap()[PARENT_LABEL],
            "sup-mock-alice"
        );

        // A child cannot spawn (depth ceiling).
        let mut spawned = parent.clone();
        spawned
            .metadata
            .labels
            .get_or_insert_with(Default::default)
            .insert(PARENT_LABEL.into(), "sup-mock-alice".into());
        assert!(build_subagent(&c, &spawned, "grandchild", "x", None, None).is_err());
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

    /// The P4-2 DoD: a supervisor bound to a VIEWER cannot create; the same
    /// binding with an admin grant can; reads stay viewer-tier; service
    /// context and ungoverned orgs pass untouched.
    #[test]
    fn viewer_bound_supervisor_cannot_create() {
        use agent_api::org::{AccessPolicy, Role};
        let policies: Vec<AccessPolicy> = serde_yaml::from_str(
            "- match: { claims: { sub: \"mock:carol\" } }\n  role: viewer\n\
             - match: { groups: [\"okta:platform-*\"] }\n  role: admin\n",
        )
        .unwrap();
        let carol = Acting {
            user: Some("mock:carol".into()),
            groups: vec![],
        };
        assert!(!user_permits(
            &carol,
            &policies,
            required_role("control.agents.create")
        ));
        assert!(user_permits(
            &carol,
            &policies,
            required_role("control.agents.list")
        ));

        // A gateway-stamped admin group flips create on — groups are the
        // GATEWAY's word, never the supervisor's.
        let carol_admin = Acting {
            user: Some("mock:carol".into()),
            groups: vec!["okta:platform-admins".into()],
        };
        assert!(user_permits(
            &carol_admin,
            &policies,
            required_role("control.agents.create")
        ));

        // A user NO policy matches has zero grants in a governed org: even
        // reads refuse (the org opted into governance).
        let stranger = Acting {
            user: Some("mock:mallory".into()),
            groups: vec![],
        };
        assert!(!user_permits(&stranger, &policies, Role::Viewer));

        // Ungoverned org / service workload: the P4-1 posture stands.
        assert!(user_permits(&carol, &[], Role::Admin));
        assert!(user_permits(&Acting::default(), &policies, Role::Admin));
    }
}
