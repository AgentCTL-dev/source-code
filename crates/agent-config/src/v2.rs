// SPDX-License-Identifier: Apache-2.0
//! # The v1alpha2 trigger compiler (RFC 0033 §2.2, P2-8)
//!
//! Compile `spec.triggers[]` — the typed union over agentd's TEN start kinds
//! — into generated dialect-3 workflows (`main-<kind>`: `start → work →
//! done`, the same expansion agentd's own instruction sugar performs), plus
//! the config-document prerequisites each kind demands. Every field name
//! below is pinned against agentd 1.3.1's `KINDS` table (strict field
//! checking: an unknown field is a hard validation error), with the traps the
//! source audit surfaced:
//!
//! - `subscribe` takes **`server` + `uri`** (`server` names an `mcp.servers`
//!   entry — NOT `service`), and **`debounce_ms`** (not `debounce`); an
//!   unknown server is NOT caught by `--validate-config` (the node silently
//!   never fires), so THIS compiler refuses it.
//! - `webhook` REQUIRES `webhooks.listen` in the config (refused otherwise);
//!   we bind loopback (`http://127.0.0.1:9494`) — reachable via
//!   port-forward and the gateway's hooks proxy, and exempt from the
//!   public-bind TLS/auth demands. `methods` must be an ARRAY (a scalar
//!   silently means "any method"); `respond: sync` is the only respond form
//!   with an effect (the documented object form is parsed and ignored).
//! - `stream` wants a `streams:` declaration — an undeclared stream is
//!   another silent-never-fires (validator checks only observability
//!   streams), so the compiler declares it.
//! - `schedule.cron` needs the `cron` build feature and silently never fires
//!   without it (passes validation!); `every`/`at` are core. External
//!   schedules (CronJob) REQUIRE `cron` syntax; an `every`-only schedule
//!   compiles to an in-daemon start instead.
//! - `workflows[].version: 3` is mandatory; multi-start convention: one run
//!   fires exactly ONE start, siblings mark Skipped (which satisfies
//!   dependents) — we still emit one workflow per trigger so each carries
//!   its own instruction and a stable `{{steps.start.output…}}` reference.

use serde_json::{json, Map, Value};

use crate::{ConfigError, ConfigInput, Mode, ResolvedIntelligence, ResolvedMcp};
use agent_api::v1alpha2 as api;

/// The in-pod webhook listener (P7-1): bound on the POD network so the
/// gateway's hooks proxy — the one external door — can deliver. agentd
/// refuses plaintext off loopback, so the listener serves the agent's own
/// operator-issued certs (the same unconditional TLS mount the a2a surface
/// uses); per-route HMAC/bearer (below) is the authenticator, per RFC 0029
/// §5's role split (gateway: rate/size/audit; agentd: TLS + auth).
pub const WEBHOOK_LISTEN: &str = "https://0.0.0.0:9494";

/// What a v2 spec renders as (the workload kind decision).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderShape {
    Daemon,
    Job,
    /// External CronJob with this cron expression.
    Cron(String),
}

/// Compile a full v2 spec into the shared [`ConfigInput`] + the render shape.
/// `mcp` carries the operator-resolved MCP bindings (inline `mcpServers` +
/// resolved `services[]` grants) — subscribe triggers must name one of them.
pub fn from_v2_spec(
    spec: &api::AgentSpec,
    intelligence: Option<ResolvedIntelligence>,
    workflow_file: Option<String>,
    aauth_provider: Option<String>,
    mcp: Vec<ResolvedMcp>,
) -> Result<(ConfigInput, RenderShape), ConfigError> {
    from_v2_spec_with_store(
        spec,
        intelligence,
        workflow_file,
        aauth_provider,
        mcp,
        crate::StoreSelector::File,
        None,
    )
}

/// [`from_v2_spec`] with the operator-resolved store placement (`class:
/// managed` → the state-service selector the operator computes; the compiler
/// itself cannot know the org/agent prefix) and the run-retention bound.
pub fn from_v2_spec_with_store(
    spec: &api::AgentSpec,
    intelligence: Option<ResolvedIntelligence>,
    workflow_file: Option<String>,
    aauth_provider: Option<String>,
    mcp: Vec<ResolvedMcp>,
    store: crate::StoreSelector,
    run_retention_keep_last: Option<u32>,
) -> Result<(ConfigInput, RenderShape), ConfigError> {
    let shape = derive_shape(spec)?;
    let persona = spec.instruction.as_ref().and_then(|i| i.text.clone());

    // The render-mode drives the SHARED bits (store, lifecycle.run_until,
    // a2a serving) — the trigger workflows below are the actual wake sources.
    let mode = match &shape {
        RenderShape::Daemon => Mode::Reactive,
        RenderShape::Job => Mode::Once,
        RenderShape::Cron(_) => Mode::Schedule,
    };

    let mut generated = Vec::new();
    let mut webhooks_block = None;
    let mut streams = Map::new();
    let mcp_names: Vec<&str> = mcp.iter().map(|m| m.name.as_str()).collect();

    for (i, t) in spec.triggers.iter().enumerate() {
        // once/manual on a JOB shape need no workflow at all (agentd's own
        // instruction sugar covers the once run; workflow.run covers manual
        // via default_start). On a DAEMON they get explicit workflows.
        if let Some(compiled) = compile_trigger(
            t,
            i,
            &persona,
            &shape,
            &mcp_names,
            &mut webhooks_block,
            &mut streams,
        )? {
            generated.push(compiled);
        }
    }

    let instruction = match mode {
        // Job/cron shapes ride the instruction sugar (`agent.instruction` +
        // run_until idle); daemons keep the persona standing.
        Mode::Once | Mode::Schedule => {
            if generated.is_empty() {
                Some(persona.clone().ok_or(ConfigError::MissingInstruction)?)
            } else {
                persona.clone()
            }
        }
        _ => persona.clone(),
    };

    let input = ConfigInput {
        mode,
        instruction,
        subscribe: Vec::new(), // v2 subscribe rides the compiled workflows
        loop_interval: None,
        loop_deadline: None,
        intelligence,
        mcp,
        workflow_file,
        limits: spec.limits.clone(),
        peers: Vec::new(),
        aauth: aauth_provider.map(|provider| crate::AauthInput { provider }),
        serve_a2a: spec.expose.as_ref().map(|e| e.a2a).unwrap_or(true),
        allow_trifecta: false,
        store,
        run_retention_keep_last,
        generated_workflows: generated,
        webhooks_block,
        streams_block: (!streams.is_empty()).then_some(Value::Object(streams)),
        principal_subjects: spec
            .access
            .as_ref()
            .map(|a| a.principals.clone())
            .unwrap_or_default(),
        principal_grants: spec
            .access
            .as_ref()
            .map(|a| a.grants.clone())
            .unwrap_or_default(),
        approval_policy: spec.approval.as_ref().and_then(|a| a.policy.clone()),
        vars: Map::new(),
        singleton_selectors: Vec::new(),
    };
    Ok((input, shape))
}

/// The shape decision (RFC 0033 §2.2): explicit `spec.shape` wins; a
/// job-shaped spec must not carry long-lived triggers; `shape: cron` needs a
/// CRON expression (spec.schedule or a cron schedule trigger) — an
/// `every`-only schedule cannot be a CronJob.
fn derive_shape(spec: &api::AgentSpec) -> Result<RenderShape, ConfigError> {
    let only_short = !spec.triggers.is_empty()
        && spec
            .triggers
            .iter()
            .all(|t| t.once.is_some() || t.manual.is_some());
    match spec.shape {
        api::Shape::Job => Ok(RenderShape::Job),
        api::Shape::Cron => {
            let cron = spec
                .schedule
                .clone()
                .or_else(|| {
                    spec.triggers
                        .iter()
                        .find_map(|t| t.schedule.as_ref().and_then(|s| s.cron.clone()))
                })
                .ok_or(ConfigError::ExternalScheduleNeedsCron)?;
            Ok(RenderShape::Cron(cron))
        }
        // The daemon default with ONLY once/manual triggers renders a Job —
        // the one inference the schema default forces on us (a defaulted
        // `daemon` is indistinguishable from an explicit one).
        api::Shape::Daemon if only_short => Ok(RenderShape::Job),
        api::Shape::Daemon => Ok(RenderShape::Daemon),
    }
}

/// One trigger → one generated workflow (or none for job-sugar cases).
#[allow(clippy::too_many_arguments)]
fn compile_trigger(
    t: &api::Trigger,
    index: usize,
    persona: &Option<String>,
    shape: &RenderShape,
    mcp_names: &[&str],
    webhooks_block: &mut Option<Value>,
    streams: &mut Map<String, Value>,
) -> Result<Option<Value>, ConfigError> {
    let instruction = persona.clone().ok_or(ConfigError::MissingInstruction)?;
    let wrap = |name: String, start: Value| {
        json!({
            "name": name,
            "version": 3,
            "steps": {
                "start": start,
                "work": { "kind": "agent", "depends_on": ["start"], "instruction": instruction },
                "done": { "kind": "finish", "depends_on": ["work"], "status": "completed",
                          "output": "{{steps.work.output}}" },
            }
        })
    };

    let wf = if t.once.is_some() {
        match shape {
            // A once trigger on a job shape IS the instruction sugar.
            RenderShape::Job | RenderShape::Cron(_) => return Ok(None),
            RenderShape::Daemon => wrap(format!("main-once-{index}"), json!({ "kind": "once" })),
        }
    } else if t.manual.is_some() {
        // `workflow.run` prefers the manual start (default_start), so the
        // workflow exists even on job shapes.
        wrap(format!("main-manual-{index}"), json!({ "kind": "manual" }))
    } else if let Some(l) = &t.loop_ {
        let mut start = Map::new();
        start.insert("kind".into(), json!("loop"));
        start.insert("interval".into(), json!(l.interval));
        if let Some(u) = &l.until {
            start.insert("until".into(), json!(u));
        }
        wrap(format!("main-loop-{index}"), Value::Object(start))
    } else if let Some(sch) = &t.schedule {
        match shape {
            // External schedule = the CronJob fires the pod; no workflow.
            RenderShape::Cron(_) => return Ok(None),
            _ => {
                let mut start = Map::new();
                start.insert("kind".into(), json!("schedule"));
                match (&sch.cron, &sch.every) {
                    (Some(c), _) => {
                        start.insert("cron".into(), json!(c));
                        if let Some(tz) = &sch.tz {
                            start.insert("tz".into(), json!(tz));
                        }
                    }
                    (None, Some(e)) => {
                        start.insert("every".into(), json!(e));
                    }
                    (None, None) => return Err(ConfigError::ScheduleTriggerNeedsWhen),
                }
                wrap(format!("main-schedule-{index}"), Value::Object(start))
            }
        }
    } else if let Some(w) = &t.webhook {
        // The listener block is the config prerequisite (refused absent).
        *webhooks_block = Some(json!({
            "listen": WEBHOOK_LISTEN,
            "tls": { "cert": crate::paths::TLS_CERT, "key": crate::paths::TLS_KEY },
        }));
        let mut start = Map::new();
        start.insert("kind".into(), json!("webhook"));
        start.insert("path".into(), json!(w.path));
        // Per-route auth (P7-1): the operator provisions the secret value
        // into the `<name>-hooks` Secret mounted at HOOKS_SECRETS_DIR; the
        // ref resolves at agentd startup (a dangling mount is exit 2).
        match w.auth.as_deref() {
            // SECURE BY DEFAULT: an off-loopback listener refuses
            // unauthenticated routes at agentd, so unset auth means HMAC
            // (the operator provisions the secret; `agentctl expose webhook
            // --show-secret` hands the sender its half).
            Some("hmac") | None => {
                start.insert(
                    "auth".into(),
                    // GitHub-convention signature shape, pinned explicitly
                    // (header + prefix defaults differ across senders):
                    // X-Signature: sha256=<hex HMAC-SHA256(secret, body)>.
                    json!({ "hmac": {
                        "algo": "sha256",
                        "header": "X-Signature",
                        "prefix": "sha256=",
                        "secret": format!(
                            "{{{{secret-file:{}/hmac-{index}}}}}",
                            crate::paths::HOOKS_SECRETS_DIR
                        ),
                    } }),
                );
            }
            Some("bearer") => {
                start.insert(
                    "auth".into(),
                    json!({ "bearer": format!(
                        "{{{{secret-file:{}/bearer-{index}}}}}",
                        crate::paths::HOOKS_SECRETS_DIR
                    ) }),
                );
            }
            Some(other) => {
                // Including "none": agentd refuses unauthenticated routes on
                // a non-loopback listener outright — failing here beats a
                // crash-looping pod.
                return Err(ConfigError::UnknownWebhookAuth {
                    auth: other.to_string(),
                });
            }
        }
        if !w.methods.is_empty() {
            // MUST be an array — a scalar silently means "any method".
            start.insert("methods".into(), json!(w.methods));
        }
        if let Some(r) = &w.rate {
            start.insert("rate".into(), json!(r));
        }
        if let Some(i) = &w.idempotency {
            start.insert("idempotency".into(), json!(i));
        }
        wrap(format!("main-webhook-{index}"), Value::Object(start))
    } else if let Some(sub) = &t.subscribe {
        // The server MUST be a resolved MCP binding: an unknown server passes
        // --validate-config and then silently never fires (audited upstream)
        // — this compiler is the only gate.
        if !mcp_names.contains(&sub.service.as_str()) {
            return Err(ConfigError::UnknownSubscribeServer {
                service: sub.service.clone(),
            });
        }
        let mut start = Map::new();
        start.insert("kind".into(), json!("subscribe"));
        start.insert("server".into(), json!(sub.service));
        start.insert("uri".into(), json!(sub.uri));
        if let Some(d) = &sub.debounce {
            // The v2 API says `debounce: 500ms`; the step field is
            // `debounce_ms` (integer). Parse the duration suffix.
            if let Some(ms) = parse_ms(d) {
                start.insert("debounce_ms".into(), json!(ms));
            }
        }
        if let Some(f) = &sub.filter {
            start.insert("filter".into(), json!(f));
        }
        wrap(format!("main-subscribe-{index}"), Value::Object(start))
    } else if let Some(st) = &t.stream {
        // Declare the stream — undeclared = silent-never-fires.
        streams
            .entry(st.stream.clone())
            .or_insert_with(|| json!({ "retention": { "max_events": 10_000, "max_age": "72h" } }));
        let mut start = Map::new();
        start.insert("kind".into(), json!("stream"));
        start.insert("stream".into(), json!(st.stream));
        if let Some(subj) = &st.subject {
            start.insert("subject".into(), json!(subj));
        }
        if let Some(f) = &st.from {
            start.insert("from".into(), json!(f));
        }
        if let Some(r) = &st.rate {
            start.insert("rate".into(), json!(r));
        }
        wrap(format!("main-stream-{index}"), Value::Object(start))
    } else if let Some(sig) = &t.signal {
        let mut start = Map::new();
        start.insert("kind".into(), json!("signal"));
        start.insert("name".into(), json!(sig.name));
        if let Some(f) = &sig.filter {
            start.insert("filter".into(), json!(f));
        }
        wrap(format!("main-signal-{index}"), Value::Object(start))
    } else if let Some(ev) = &t.event {
        let mut start = Map::new();
        start.insert("kind".into(), json!("event"));
        start.insert("on".into(), json!(ev.name));
        if let Some(f) = &ev.filter {
            start.insert("filter".into(), json!(f));
        }
        wrap(format!("main-event-{index}"), Value::Object(start))
    } else if let Some(cmd) = &t.a2a_command {
        let mut start = Map::new();
        start.insert("kind".into(), json!("a2a"));
        start.insert("command".into(), json!(cmd.command));
        if !cmd.roles.is_empty() {
            start.insert("roles".into(), json!(cmd.roles));
        }
        if let Some(schema) = &cmd.schema {
            start.insert("schema".into(), schema.clone());
        }
        wrap(format!("main-a2a-{index}"), Value::Object(start))
    } else {
        // CEL guarantees exactly-one; an empty union here is a spec defect.
        return Err(ConfigError::EmptyTrigger);
    };
    Ok(Some(wf))
}

/// `500ms` / `2s` / `1m` → milliseconds.
fn parse_ms(d: &str) -> Option<u64> {
    let d = d.trim();
    if let Some(n) = d.strip_suffix("ms") {
        return n.trim().parse().ok();
    }
    if let Some(n) = d.strip_suffix('s') {
        return n.trim().parse::<u64>().ok().map(|v| v * 1000);
    }
    if let Some(n) = d.strip_suffix('m') {
        return n.trim().parse::<u64>().ok().map(|v| v * 60_000);
    }
    d.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(yaml: &str) -> api::AgentSpec {
        serde_yaml::from_str(yaml).unwrap()
    }
    fn mcp(name: &str) -> ResolvedMcp {
        ResolvedMcp {
            name: name.into(),
            endpoint: "http://127.0.0.1:8931/mcp".into(),
            tags: vec![],
            token_env: None,
            header: None,
            allow: Vec::new(),
            static_headers: Default::default(),
        }
    }

    /// P7-1: an authenticated webhook trigger emits the auth block with the
    /// signature shape PINNED — `algo` is parsed-and-IGNORED by agentd 1.3.1
    /// (verified upstream: sha512 configs validate and still verify sha256),
    /// so emitting anything but explicit sha256 would be a silent
    /// cryptographic-downgrade trap. Hex, GitHub-style prefix, secret-file ref.
    #[test]
    fn webhook_auth_emits_pinned_hmac_shape() {
        let s = spec(
            r#"
shape: daemon
instruction: { text: "persona" }
triggers:
  - webhook: { path: /zendesk-events, auth: hmac, methods: [POST] }
  - webhook: { path: /ci, auth: bearer }
"#,
        );
        let (input, _) = from_v2_spec(&s, None, None, None, vec![]).unwrap();
        let wf = &input.generated_workflows;
        let auth0 = &wf[0]["steps"]["start"]["auth"];
        assert_eq!(auth0["hmac"]["algo"], "sha256");
        assert_eq!(auth0["hmac"]["header"], "X-Signature");
        assert_eq!(auth0["hmac"]["prefix"], "sha256=");
        assert_eq!(
            auth0["hmac"]["secret"],
            "{{secret-file:/etc/agentctl/hooks/hmac-0}}"
        );
        let auth1 = &wf[1]["steps"]["start"]["auth"];
        assert_eq!(
            auth1["bearer"],
            "{{secret-file:/etc/agentctl/hooks/bearer-1}}"
        );
        // Unknown auth refuses at compile (never silently unauthenticated).
        let bad = spec(
            r#"
shape: daemon
instruction: { text: "persona" }
triggers:
  - webhook: { path: /x, auth: sha512-hmac }
"#,
        );
        assert!(from_v2_spec(&bad, None, None, None, vec![]).is_err());
    }

    #[test]
    fn all_ten_kinds_compile_with_pinned_field_names() {
        let s = spec(
            r#"
shape: daemon
instruction: { text: "persona" }
triggers:
  - once: {}
  - manual: {}
  - loop: { interval: 10m, until: 2h }
  - schedule: { cron: "0 7 * * 1-5", tz: UTC }
  - schedule: { every: 1h }
  - webhook: { path: /hooks/ci, methods: [POST], rate: "30/60s" }
  - subscribe: { service: queue, uri: "queue://inbox", debounce: 500ms }
  - stream: { stream: incidents, subject: "sev1.*", from: new }
  - signal: { name: "reply/42" }
  - event: { name: workflow.finished }
  - a2aCommand: { command: qa.verify, roles: [operator] }
"#,
        );
        let (input, shape) = from_v2_spec(&s, None, None, None, vec![mcp("queue")]).unwrap();
        assert_eq!(shape, RenderShape::Daemon);
        let wf = &input.generated_workflows;
        assert_eq!(wf.len(), 11);
        for w in wf {
            assert_eq!(w["version"], 3, "version: 3 is mandatory");
            assert_eq!(w["steps"]["work"]["kind"], "agent");
            assert_eq!(w["steps"]["done"]["kind"], "finish");
        }
        // The trap fields, pinned:
        let by_kind = |k: &str| {
            wf.iter()
                .find(|w| w["steps"]["start"]["kind"] == k)
                .unwrap_or_else(|| panic!("no {k} workflow"))["steps"]["start"]
                .clone()
        };
        assert_eq!(by_kind("loop")["interval"], "10m");
        assert_eq!(
            by_kind("subscribe")["server"],
            "queue",
            "server, NOT service"
        );
        assert_eq!(
            by_kind("subscribe")["debounce_ms"],
            500,
            "debounce_ms, NOT debounce"
        );
        assert!(
            by_kind("webhook")["methods"].is_array(),
            "methods must be an array"
        );
        assert_eq!(by_kind("event")["on"], "workflow.finished");
        assert_eq!(by_kind("a2a")["command"], "qa.verify");
        // Prerequisites wired:
        assert_eq!(
            input.webhooks_block.as_ref().unwrap()["listen"],
            WEBHOOK_LISTEN
        );
        assert_eq!(
            input.webhooks_block.as_ref().unwrap()["tls"]["cert"],
            crate::paths::TLS_CERT,
            "off-loopback binds MUST serve TLS (agentd refuses plaintext)"
        );
        assert!(
            input.streams_block.as_ref().unwrap()["incidents"]["retention"]["max_events"]
                .is_number()
        );
    }

    #[test]
    fn unknown_subscribe_server_is_refused_here_not_silently_dead() {
        let s = spec(
            "shape: daemon\ninstruction: { text: p }\ntriggers: [{ subscribe: { service: ghost, uri: \"g://x\" } }]",
        );
        let err = from_v2_spec(&s, None, None, None, vec![]).unwrap_err();
        assert!(matches!(err, ConfigError::UnknownSubscribeServer { .. }));
    }

    #[test]
    fn shape_rules() {
        // Sole once/manual on the defaulted daemon → Job (inference).
        let s = spec("instruction: { text: p }\ntriggers: [{ once: {} }]");
        let (_, shape) = from_v2_spec(&s, None, None, None, vec![]).unwrap();
        assert_eq!(shape, RenderShape::Job);

        // cron shape takes the expression from the schedule trigger.
        let s = spec(
            "shape: cron\ninstruction: { text: p }\ntriggers: [{ schedule: { cron: \"0 * * * *\" } }]",
        );
        let (input, shape) = from_v2_spec(&s, None, None, None, vec![]).unwrap();
        assert_eq!(shape, RenderShape::Cron("0 * * * *".into()));
        // External schedule: the CronJob fires the pod; NO generated workflow.
        assert!(input.generated_workflows.is_empty());
        assert_eq!(input.instruction.as_deref(), Some("p"));

        // cron shape with an every-only schedule is refused (CronJob needs cron).
        let s =
            spec("shape: cron\ninstruction: { text: p }\ntriggers: [{ schedule: { every: 1h } }]");
        assert!(matches!(
            from_v2_spec(&s, None, None, None, vec![]).unwrap_err(),
            ConfigError::ExternalScheduleNeedsCron
        ));
    }

    #[test]
    fn a2a_only_daemon_compiles_with_no_workflows() {
        // Inbound A2A as the sole wake source: valid daemon, no triggers.
        let s = spec("shape: daemon\ninstruction: { text: p }\nexpose: { a2a: true }");
        let (input, shape) = from_v2_spec(&s, None, None, None, vec![]).unwrap();
        assert_eq!(shape, RenderShape::Daemon);
        assert!(input.generated_workflows.is_empty());
        assert!(input.serve_a2a);
    }
}
