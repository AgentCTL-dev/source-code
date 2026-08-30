//! Pure composition of the agent config document (ACC 2).
//!
//! agentd ≥ the August-2026 v1.x line has **no execution-mode flags**: an agent
//! is one `config_version: "1"` document (triggers are workflow start nodes,
//! serving is `a2a.listen`, limits/budgets/store/lifecycle are config keys), and
//! the removed 1.x flags (`--mode`, `--shard`, `--subscribe`, `--serve-mcp`
//! certs, …) exit 2. This crate is the single builder that maps a v1alpha1
//! `AgentSpec` plus operator-resolved facts onto that document.
//!
//! Rules encoded here (see `contract/SPEC.md` §2 and RFC 0032 §4):
//! - the document is emitted as **JSON** (`agentd.json`) — agentd selects the
//!   parser by extension, and JSON needs no extra dependency and diffs cleanly;
//! - `lifecycle.run_until` is always **explicit** (`idle` for job shapes,
//!   `drained` for daemons) — never agentd's `auto`, which misclassifies
//!   webhook/stream-only instances (upstream ask U4);
//! - secrets appear only as `{{secret:NAME}}` references resolved from env vars
//!   the workload layer mounts as `secretKeyRef`s;
//! - `agent.name` is never written: it resolves from the `AGENT_POD_NAME`
//!   downward-API env at runtime, which is the durable-store identity fence
//!   (unique per replica by construction);
//! - daemons get an explicit file store (an emptyDir survives container
//!   restarts — the `ephemeral` store class; `managed` arrives with the state
//!   service, RFC 0031).

use agent_api::{AgentSpec, McpAuthMode, McpServer, Mode};
use serde_json::{json, Map, Value};

/// Where the operator mounts things inside the agent pod. The config document
/// references these paths, so builder and workload renderer must agree.
pub mod paths {
    /// The rendered config directory (a ConfigMap volume).
    pub const CONFIG_DIR: &str = "/etc/agentctl/config";
    /// The config document inside [`CONFIG_DIR`].
    pub const CONFIG_FILE: &str = "agentd.json";
    /// Serving keypair (cert-manager Secret volume).
    pub const TLS_CERT: &str = "/etc/agentctl/tls/tls.crt";
    pub const TLS_KEY: &str = "/etc/agentctl/tls/tls.key";
    /// The shared trust bundle (the `agentctl-ca` ConfigMap volume).
    pub const CA_BUNDLE: &str = "/etc/agentctl/ca/ca.crt";
    /// A user-supplied workflow document (ConfigMap volume), keyed file.
    pub const WORKFLOW_DIR: &str = "/etc/agentctl/workflow";
    /// The AAuth key Secret volume.
    pub const AAUTH_KEY: &str = "/etc/agentctl/aauth/agent.key";
    /// The daemon file store root (an emptyDir volume).
    pub const STATE_DIR: &str = "/var/lib/agentd/state";

    /// The full in-pod path of the config document.
    pub fn config_file() -> String {
        format!("{CONFIG_DIR}/{CONFIG_FILE}")
    }
}

/// The env-var name that carries the intelligence bearer into the pod; the
/// document references it as `{{secret:INTELLIGENCE_TOKEN}}`.
pub const INTELLIGENCE_TOKEN_ENV: &str = "INTELLIGENCE_TOKEN";

/// The A2A listener every rendered agent serves (mTLS via the PKI mounts).
pub const A2A_LISTEN: &str = "https://0.0.0.0:8443";
/// The probe/scrape listener (`/metrics`, `/healthz`, `/readyz`).
pub const METRICS_ADDR: &str = "0.0.0.0:9090";
/// `lifecycle.drain_timeout`; the workload layer must keep
/// `terminationGracePeriodSeconds` strictly above drain + abandon (28 s).
pub const DRAIN_TIMEOUT: &str = "25s";

/// An MCP server binding after the operator resolved its credential env name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedMcp {
    pub name: String,
    pub endpoint: String,
    /// Trifecta tags (rendered as the `{"*": [...]}` per-server tag map).
    pub tags: Vec<String>,
    /// Env var holding a static bearer (referenced as `{{secret:<env>}}`).
    pub token_env: Option<String>,
    /// Header carrying the token. `None` ⇒ `Authorization: Bearer <token>`.
    pub header: Option<String>,
}

impl ResolvedMcp {
    /// The conventional env-var name for a static-token MCP credential.
    pub fn token_env_for(server_name: &str) -> String {
        format!(
            "AGENT_MCP_{}_TOKEN",
            server_name.to_uppercase().replace(['-', '.'], "_")
        )
    }

    /// Map the CRD declaration onto a resolved binding. A `{{secret:…}}`
    /// reference is emitted ONLY when a Secret is actually declared to mount
    /// (`staticToken` + `tokenSecretRef`) — an unresolved reference is exit 2
    /// at agent startup, so the doc and the pod wiring must agree exactly.
    pub fn from_spec(s: &McpServer) -> ResolvedMcp {
        let has_mounted_token = s
            .auth
            .as_ref()
            .is_some_and(|a| a.mode == McpAuthMode::StaticToken && a.token_secret_ref.is_some());
        ResolvedMcp {
            name: s.name.clone(),
            endpoint: s.endpoint.clone(),
            tags: s.tags.clone(),
            token_env: has_mounted_token.then(|| Self::token_env_for(&s.name)),
            header: s.auth.as_ref().and_then(|a| a.header.clone()),
        }
    }
}

/// The intelligence binding after the operator resolved the `ModelPool`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedIntelligence {
    pub endpoint: String,
    pub model: Option<String>,
    /// `true` ⇒ the pool carries a credential mounted as
    /// [`INTELLIGENCE_TOKEN_ENV`]; the document references it, never holds it.
    pub has_token: bool,
}

/// An A2A peer wiring (coordinator → fleet endpoint, declared `spec.peers`, …).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub endpoint: String,
}

/// AAuth enrollment facts (RFC 0023 house-provisioning path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AauthInput {
    pub provider: String,
}

/// Everything the builder needs — pre-resolved, no I/O behind it.
#[derive(Clone, Debug, Default)]
pub struct ConfigInput {
    pub mode: Mode,
    pub instruction: Option<String>,
    /// Reactive-mode MCP resource URIs (`spec.subscribe`).
    pub subscribe: Vec<String>,
    /// Loop-mode cadence (`spec.loop`).
    pub loop_interval: Option<String>,
    pub loop_deadline: Option<String>,
    pub intelligence: Option<ResolvedIntelligence>,
    pub mcp: Vec<ResolvedMcp>,
    /// In-pod path of a user-supplied workflow document (`mode: workflow`, or
    /// a reactive daemon carrying a graph).
    pub workflow_file: Option<String>,
    pub limits: Option<agent_api::Limits>,
    pub peers: Vec<Peer>,
    pub aauth: Option<AauthInput>,
    /// `true` ⇒ the A2A listener is declared (the default posture; every
    /// rendered agent serves its control surface over mTLS).
    pub serve_a2a: bool,
    pub allow_trifecta: bool,
}

impl ConfigInput {
    /// The common path: map a CRD spec plus resolved facts.
    pub fn from_spec(
        spec: &AgentSpec,
        intelligence: Option<ResolvedIntelligence>,
        workflow_file: Option<String>,
        aauth_provider: Option<String>,
    ) -> ConfigInput {
        ConfigInput {
            mode: spec.mode,
            instruction: spec.instruction.clone(),
            subscribe: spec.subscribe.clone(),
            loop_interval: spec.loop_.as_ref().map(|l| l.interval.clone()),
            loop_deadline: spec.loop_.as_ref().and_then(|l| l.deadline.clone()),
            intelligence,
            mcp: spec
                .mcp_servers
                .iter()
                .map(ResolvedMcp::from_spec)
                .collect(),
            workflow_file,
            limits: spec.limits.clone(),
            peers: Vec::new(),
            aauth: aauth_provider.map(|provider| AauthInput { provider }),
            serve_a2a: true,
            allow_trifecta: false,
        }
    }
}

/// Composition failures — each names the exact spec defect.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// `mode: loop` without `spec.loop.interval`.
    MissingLoopInterval,
    /// `mode: workflow` without a workflow source.
    MissingWorkflow,
    /// A subscribe URI names no MCP server: no server name matches the URI
    /// scheme and more than one server is declared.
    AmbiguousSubscribe { uri: String },
    /// A subscribe URI with no MCP servers declared at all.
    NoServerForSubscribe { uri: String },
    /// once/loop/schedule need an instruction (mirrors the CRD CEL rule).
    MissingInstruction,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::MissingLoopInterval => write!(f, "mode 'loop' requires spec.loop.interval"),
            ConfigError::MissingWorkflow => write!(f, "mode 'workflow' requires spec.workflow"),
            ConfigError::AmbiguousSubscribe { uri } => write!(
                f,
                "subscribe uri {uri:?} matches no declared MCP server by scheme and several servers are declared; name the server as the uri scheme"
            ),
            ConfigError::NoServerForSubscribe { uri } => {
                write!(f, "subscribe uri {uri:?} needs a declared MCP server")
            }
            ConfigError::MissingInstruction => {
                write!(f, "this mode requires spec.instruction")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// The composed document.
#[derive(Clone, Debug)]
pub struct ConfigDoc {
    /// The `config_version: "1"` document as a JSON value.
    pub value: Value,
}

impl ConfigDoc {
    /// Pretty JSON — what lands in the ConfigMap under
    /// [`paths::CONFIG_FILE`] and what `--validate-config` runs over.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(&self.value).expect("infallible: Value → JSON");
        s.push('\n');
        s
    }

    /// Every distinct `{{secret:NAME}}` env name the document references.
    /// `--validate-config` requires each to RESOLVE from the environment
    /// (verified against the binary), so a validator must export a placeholder
    /// for each; the workload layer must mount a real `secretKeyRef` for each
    /// — this scan is how both stay complete.
    pub fn secret_refs(&self) -> Vec<String> {
        fn scan(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::String(s) => {
                    let mut rest = s.as_str();
                    while let Some(i) = rest.find("{{secret:") {
                        let tail = &rest[i + "{{secret:".len()..];
                        if let Some(j) = tail.find("}}") {
                            let name = tail[..j].to_string();
                            if !out.contains(&name) {
                                out.push(name);
                            }
                            rest = &tail[j + 2..];
                        } else {
                            break;
                        }
                    }
                }
                Value::Array(a) => a.iter().for_each(|v| scan(v, out)),
                Value::Object(m) => m.values().for_each(|v| scan(v, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        scan(&self.value, &mut out);
        out
    }

    /// A short stable hash of the rendered document — the pod-template
    /// annotation that turns config changes into rolling restarts (the safe
    /// interim delivery until upstream ask U1 lands; ADR-0007).
    pub fn hash(&self) -> String {
        // FNV-1a/64: no crypto need — this is a change detector, not a MAC.
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = OFFSET;
        for b in self.to_json().as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(PRIME);
        }
        format!("{h:016x}")
    }
}

/// Is this a daemon (Deployment/StatefulSet) or a job shape (Job/CronJob)?
/// The workload kind and `lifecycle.run_until` both derive from THIS answer —
/// never from agentd's `lifecycle.daemon` manifest hint (unreliable, U3/U4).
pub fn is_daemon(mode: Mode) -> bool {
    matches!(mode, Mode::Loop | Mode::Reactive)
}

/// Normalize an in-cluster Service endpoint to its ABSOLUTE (trailing-dot)
/// FQDN form so pod `ndots`/search-domain resolution can never leak the lookup
/// to an external wildcard domain (the documented cluster-DNS trap).
pub fn absolutize_endpoint(endpoint: &str) -> String {
    let Some(scheme_end) = endpoint.find("://") else {
        return endpoint.to_string();
    };
    let rest = &endpoint[scheme_end + 3..];
    let host_end = rest.find(['/', ':', '?']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.ends_with(".svc.cluster.local") {
        let mut out = String::with_capacity(endpoint.len() + 1);
        out.push_str(&endpoint[..scheme_end + 3]);
        out.push_str(host);
        out.push('.');
        out.push_str(&rest[host_end..]);
        out
    } else {
        endpoint.to_string()
    }
}

/// The one compose entrypoint operator AND admission share, so the document
/// the webhook validates is byte-identical to the one the reconcile mounts.
/// In-cluster endpoints (intelligence + MCP) are absolutized here.
pub fn compose_from_spec(
    spec: &AgentSpec,
    intelligence: Option<ResolvedIntelligence>,
    workflow_file: Option<String>,
    aauth_provider: Option<String>,
) -> Result<ConfigDoc, ConfigError> {
    let intelligence = intelligence.map(|mut i| {
        i.endpoint = absolutize_endpoint(&i.endpoint);
        i
    });
    let mut input = ConfigInput::from_spec(spec, intelligence, workflow_file, aauth_provider);
    for m in &mut input.mcp {
        m.endpoint = absolutize_endpoint(&m.endpoint);
    }
    build(&input)
}

/// Compose the document. Pure; the only failure modes are spec defects.
pub fn build(input: &ConfigInput) -> Result<ConfigDoc, ConfigError> {
    let mut doc = Map::new();
    doc.insert("config_version".into(), json!("1"));

    // --- agent -----------------------------------------------------------
    // The persona/instruction. For generated-workflow modes the task rides the
    // workflow's agent step; `agent.instruction` stays the standing persona.
    let mut agent = Map::new();
    match input.mode {
        Mode::Once | Mode::Schedule => {
            let instruction = input
                .instruction
                .clone()
                .ok_or(ConfigError::MissingInstruction)?;
            agent.insert("instruction".into(), json!(instruction));
        }
        Mode::Loop | Mode::Reactive | Mode::Workflow => {
            if let Some(instruction) = &input.instruction {
                agent.insert("instruction".into(), json!(instruction));
            }
        }
    }
    if !agent.is_empty() {
        doc.insert("agent".into(), Value::Object(agent));
    }

    // --- intelligence -----------------------------------------------------
    if let Some(intel) = &input.intelligence {
        let mut m = Map::new();
        m.insert("endpoints".into(), json!(intel.endpoint));
        if let Some(model) = &intel.model {
            m.insert("model".into(), json!(model));
        }
        if intel.has_token {
            m.insert(
                "token".into(),
                json!(format!("{{{{secret:{INTELLIGENCE_TOKEN_ENV}}}}}")),
            );
        }
        doc.insert("intelligence".into(), Value::Object(m));
    }

    // --- mcp --------------------------------------------------------------
    if !input.mcp.is_empty() {
        let servers: Vec<Value> = input.mcp.iter().map(mcp_server_entry).collect();
        doc.insert("mcp".into(), json!({ "servers": servers }));
    }

    // --- workflows --------------------------------------------------------
    let mut workflows: Vec<Value> = Vec::new();
    match input.mode {
        // The instruction sugar (`once → agent → finish`) is agentd's own; a
        // job document with an instruction and no machinery needs nothing here.
        Mode::Once | Mode::Schedule => {}
        Mode::Loop => {
            let interval = input
                .loop_interval
                .clone()
                .ok_or(ConfigError::MissingLoopInterval)?;
            let instruction = input
                .instruction
                .clone()
                .ok_or(ConfigError::MissingInstruction)?;
            workflows.push(json!({
                "name": "main",
                "steps": {
                    "tick": { "kind": "loop", "interval": interval },
                    "work": { "kind": "agent", "depends_on": ["tick"], "instruction": instruction },
                    "done": { "kind": "finish", "depends_on": ["work"] },
                }
            }));
        }
        Mode::Reactive => {
            if !input.subscribe.is_empty() {
                workflows.push(subscribe_workflow(input)?);
            }
        }
        Mode::Workflow => {
            let file = input
                .workflow_file
                .clone()
                .ok_or(ConfigError::MissingWorkflow)?;
            workflows.push(json!({ "file": file }));
        }
    }
    // A reactive daemon may ALSO carry a user workflow graph.
    if input.mode == Mode::Reactive {
        if let Some(file) = &input.workflow_file {
            workflows.push(json!({ "file": file }));
        }
    }
    if !workflows.is_empty() {
        doc.insert("workflows".into(), Value::Array(workflows));
    }

    // --- serving ----------------------------------------------------------
    if input.serve_a2a {
        doc.insert("a2a".into(), a2a_block(&input.peers));
    } else if !input.peers.is_empty() {
        doc.insert("a2a".into(), json!({ "peers": peer_entries(&input.peers) }));
    }

    // --- store + lifecycle ------------------------------------------------
    if is_daemon(input.mode) {
        doc.insert(
            "store".into(),
            json!({ "kind": "file", "file": { "path": paths::STATE_DIR } }),
        );
        doc.insert(
            "lifecycle".into(),
            json!({ "run_until": "drained", "drain_timeout": DRAIN_TIMEOUT }),
        );
    } else {
        doc.insert(
            "lifecycle".into(),
            json!({ "run_until": "idle", "drain_timeout": DRAIN_TIMEOUT }),
        );
    }

    // --- limits + budget --------------------------------------------------
    if let Some(l) = &input.limits {
        let mut run = Map::new();
        if let Some(v) = l.max_steps {
            run.insert("steps".into(), json!(v));
        }
        if let Some(v) = l.max_tokens {
            run.insert("tokens".into(), json!(v));
        }
        if let Some(d) = &input.loop_deadline {
            run.insert("deadline".into(), json!(d));
        }
        let mut limits = Map::new();
        if !run.is_empty() {
            limits.insert("run".into(), Value::Object(run));
        }
        if let Some(v) = l.max_depth {
            limits.insert("subagents".into(), json!({ "depth": v }));
        }
        if !limits.is_empty() {
            doc.insert("limits".into(), Value::Object(limits));
        }
        if let Some(v) = l.lifetime_tokens {
            // The lifetime budget lives under intelligence.budget — fold it
            // into the block, creating one if no pool was bound.
            let intel = doc
                .entry("intelligence")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = intel {
                m.insert("budget".into(), json!({ "lifetime_tokens": v }));
            }
        }
    } else if let Some(d) = &input.loop_deadline {
        doc.insert("limits".into(), json!({ "run": { "deadline": d } }));
    }

    // --- identity (AAuth) -------------------------------------------------
    // --- security ---------------------------------------------------------
    let mut security = Map::new();
    security.insert("tls_ca".into(), json!(paths::CA_BUNDLE));
    if let Some(aauth) = &input.aauth {
        security.insert(
            "aauth".into(),
            json!({ "provider": aauth.provider, "key_file": paths::AAUTH_KEY }),
        );
    }
    if input.allow_trifecta {
        security.insert("allow_trifecta".into(), json!(true));
    }
    doc.insert("security".into(), Value::Object(security));

    // --- observability ----------------------------------------------------
    doc.insert(
        "observability".into(),
        json!({ "metrics_addr": METRICS_ADDR }),
    );

    Ok(ConfigDoc {
        value: Value::Object(doc),
    })
}

fn a2a_block(peers: &[Peer]) -> Value {
    let mut a2a = Map::new();
    a2a.insert("listen".into(), json!(A2A_LISTEN));
    a2a.insert(
        "tls".into(),
        json!({
            "cert": paths::TLS_CERT,
            "key": paths::TLS_KEY,
            "client_ca": paths::CA_BUNDLE,
        }),
    );
    if !peers.is_empty() {
        a2a.insert("peers".into(), Value::Array(peer_entries(peers)));
    }
    Value::Object(a2a)
}

fn peer_entries(peers: &[Peer]) -> Vec<Value> {
    peers
        .iter()
        .map(|p| json!({ "name": p.name, "endpoint": p.endpoint }))
        .collect()
}

fn mcp_server_entry(m: &ResolvedMcp) -> Value {
    let mut entry = Map::new();
    entry.insert("name".into(), json!(m.name));
    entry.insert("endpoint".into(), json!(m.endpoint));
    if !m.tags.is_empty() {
        entry.insert("tags".into(), json!({ "*": m.tags }));
    }
    if let Some(env) = &m.token_env {
        let value = match &m.header {
            // A named header carries the raw token; the default carries the
            // conventional `Bearer` scheme on `Authorization`.
            Some(_) => format!("{{{{secret:{env}}}}}"),
            None => format!("Bearer {{{{secret:{env}}}}}"),
        };
        let header = m.header.clone().unwrap_or_else(|| "Authorization".into());
        entry.insert("headers".into(), json!({ header: value }));
    }
    Value::Object(entry)
}

/// Reactive mode: one workflow, one `subscribe` start per URI, a single agent
/// step satisfied by whichever start fired (sibling starts stay Skipped and
/// still satisfy dependents — the multi-start convention agentd documents).
fn subscribe_workflow(input: &ConfigInput) -> Result<Value, ConfigError> {
    let mut steps = Map::new();
    let mut start_ids = Vec::new();
    for (i, uri) in input.subscribe.iter().enumerate() {
        let server = server_for_uri(input, uri)?;
        let id = format!("s{i}");
        steps.insert(
            id.clone(),
            json!({ "kind": "subscribe", "server": server, "uri": uri }),
        );
        start_ids.push(Value::String(id));
    }
    let instruction = input
        .instruction
        .clone()
        .unwrap_or_else(|| "Handle the subscribed resource update.".to_string());
    steps.insert(
        "work".into(),
        json!({ "kind": "agent", "depends_on": start_ids, "instruction": instruction }),
    );
    steps.insert(
        "done".into(),
        json!({ "kind": "finish", "depends_on": ["work"] }),
    );
    Ok(json!({ "name": "watch", "steps": steps }))
}

/// Bind a subscribe URI to a declared MCP server: a server named like the URI
/// scheme wins; a single declared server is an unambiguous fallback.
fn server_for_uri(input: &ConfigInput, uri: &str) -> Result<String, ConfigError> {
    let scheme = uri.split("://").next().unwrap_or("");
    if let Some(m) = input.mcp.iter().find(|m| m.name == scheme) {
        return Ok(m.name.clone());
    }
    match input.mcp.len() {
        0 => Err(ConfigError::NoServerForSubscribe { uri: uri.into() }),
        1 => Ok(input.mcp[0].name.clone()),
        _ => Err(ConfigError::AmbiguousSubscribe { uri: uri.into() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_api::{Limits, LoopParams, McpAuth};

    fn base(mode: Mode) -> ConfigInput {
        ConfigInput {
            mode,
            instruction: Some("Do the thing.".into()),
            serve_a2a: true,
            ..Default::default()
        }
    }

    fn doc(input: &ConfigInput) -> Value {
        build(input).expect("build").value
    }

    #[test]
    fn once_is_a_job_document_with_sugar_instruction() {
        let d = doc(&base(Mode::Once));
        assert_eq!(d["config_version"], "1");
        assert_eq!(d["agent"]["instruction"], "Do the thing.");
        assert_eq!(d["lifecycle"]["run_until"], "idle");
        assert!(d.get("workflows").is_none(), "sugar main is agentd's own");
        assert!(d.get("store").is_none(), "one-shots write nothing");
    }

    #[test]
    fn schedule_document_is_job_shaped_cron_lives_in_the_cronjob() {
        let d = doc(&base(Mode::Schedule));
        assert_eq!(d["lifecycle"]["run_until"], "idle");
        assert!(d.get("workflows").is_none());
    }

    #[test]
    fn loop_mode_generates_the_main_loop_workflow() {
        let mut input = base(Mode::Loop);
        input.loop_interval = Some("5m".into());
        input.loop_deadline = Some("24h".into());
        let d = doc(&input);
        assert_eq!(d["workflows"][0]["name"], "main");
        assert_eq!(d["workflows"][0]["steps"]["tick"]["kind"], "loop");
        assert_eq!(d["workflows"][0]["steps"]["tick"]["interval"], "5m");
        assert_eq!(
            d["workflows"][0]["steps"]["work"]["instruction"],
            "Do the thing."
        );
        assert_eq!(d["lifecycle"]["run_until"], "drained");
        assert_eq!(d["store"]["kind"], "file");
        assert_eq!(d["limits"]["run"]["deadline"], "24h");
    }

    #[test]
    fn loop_without_interval_is_a_named_error() {
        let input = base(Mode::Loop);
        assert_eq!(build(&input).unwrap_err(), ConfigError::MissingLoopInterval);
    }

    #[test]
    fn reactive_subscribes_bind_servers_by_scheme_then_singleton() {
        let mut input = base(Mode::Reactive);
        input.mcp = vec![
            ResolvedMcp {
                name: "queue".into(),
                endpoint: "https://q.internal/mcp".into(),
                tags: vec![],
                token_env: None,
                header: None,
            },
            ResolvedMcp {
                name: "fs".into(),
                endpoint: "https://fs.internal/mcp".into(),
                tags: vec![],
                token_env: None,
                header: None,
            },
        ];
        input.subscribe = vec!["queue://inbox".into()];
        let d = doc(&input);
        let steps = &d["workflows"][0]["steps"];
        assert_eq!(steps["s0"]["kind"], "subscribe");
        assert_eq!(steps["s0"]["server"], "queue");
        assert_eq!(steps["s0"]["uri"], "queue://inbox");
        assert_eq!(steps["work"]["depends_on"][0], "s0");
        assert_eq!(d["lifecycle"]["run_until"], "drained");

        input.subscribe = vec!["drive://x".into()];
        assert!(matches!(
            build(&input).unwrap_err(),
            ConfigError::AmbiguousSubscribe { .. }
        ));

        input.mcp.truncate(1);
        let d = doc(&input);
        assert_eq!(d["workflows"][0]["steps"]["s0"]["server"], "queue");
    }

    #[test]
    fn workflow_mode_references_the_mounted_file() {
        let mut input = base(Mode::Workflow);
        input.workflow_file = Some("/etc/agentctl/workflow/workflow.json".into());
        let d = doc(&input);
        assert_eq!(
            d["workflows"][0]["file"],
            "/etc/agentctl/workflow/workflow.json"
        );
        assert_eq!(d["lifecycle"]["run_until"], "idle");
    }

    #[test]
    fn serving_is_a2a_config_with_the_pki_mount_paths() {
        let d = doc(&base(Mode::Once));
        assert_eq!(d["a2a"]["listen"], A2A_LISTEN);
        assert_eq!(d["a2a"]["tls"]["cert"], paths::TLS_CERT);
        assert_eq!(d["a2a"]["tls"]["key"], paths::TLS_KEY);
        assert_eq!(d["a2a"]["tls"]["client_ca"], paths::CA_BUNDLE);
        assert_eq!(d["security"]["tls_ca"], paths::CA_BUNDLE);
        assert_eq!(d["observability"]["metrics_addr"], METRICS_ADDR);
    }

    #[test]
    fn intelligence_uses_secret_references_never_values() {
        let mut input = base(Mode::Once);
        input.intelligence = Some(ResolvedIntelligence {
            endpoint: "https://llm.internal/v1".into(),
            model: Some("m1".into()),
            has_token: true,
        });
        let d = doc(&input);
        assert_eq!(d["intelligence"]["endpoints"], "https://llm.internal/v1");
        assert_eq!(d["intelligence"]["model"], "m1");
        assert_eq!(d["intelligence"]["token"], "{{secret:INTELLIGENCE_TOKEN}}");
    }

    #[test]
    fn mcp_static_token_renders_a_secret_reference_header() {
        let m = McpServer {
            name: "billing-api".into(),
            endpoint: "https://b.internal/mcp".into(),
            auth: Some(McpAuth {
                mode: McpAuthMode::StaticToken,
                token_secret_ref: Some(agent_api::SecretKeyRef {
                    name: "billing-token".into(),
                    key: "token".into(),
                }),
                header: None,
            }),
            tags: vec!["sensitive".into()],
        };
        let r = ResolvedMcp::from_spec(&m);
        assert_eq!(r.token_env.as_deref(), Some("AGENT_MCP_BILLING_API_TOKEN"));
        let mut input = base(Mode::Once);
        input.mcp = vec![r];
        let d = doc(&input);
        let entry = &d["mcp"]["servers"][0];
        assert_eq!(entry["name"], "billing-api");
        assert_eq!(entry["tags"]["*"][0], "sensitive");
        assert_eq!(
            entry["headers"]["Authorization"],
            "Bearer {{secret:AGENT_MCP_BILLING_API_TOKEN}}"
        );
    }

    #[test]
    fn limits_map_onto_run_and_budget_keys() {
        let mut input = base(Mode::Once);
        input.limits = Some(Limits {
            max_tokens: Some(20_000),
            max_depth: Some(2),
            max_steps: Some(50),
            lifetime_tokens: Some(1_000_000),
        });
        let d = doc(&input);
        assert_eq!(d["limits"]["run"]["steps"], 50);
        assert_eq!(d["limits"]["run"]["tokens"], 20_000);
        assert_eq!(d["limits"]["subagents"]["depth"], 2);
        assert_eq!(d["intelligence"]["budget"]["lifetime_tokens"], 1_000_000);
    }

    #[test]
    fn aauth_lands_under_security_with_the_key_mount() {
        let mut input = base(Mode::Once);
        input.aauth = Some(AauthInput {
            provider: "https://apd.internal".into(),
        });
        let d = doc(&input);
        assert_eq!(d["security"]["aauth"]["provider"], "https://apd.internal");
        assert_eq!(d["security"]["aauth"]["key_file"], paths::AAUTH_KEY);
    }

    #[test]
    fn hash_is_stable_and_change_sensitive() {
        let a = build(&base(Mode::Once)).unwrap();
        let b = build(&base(Mode::Once)).unwrap();
        assert_eq!(a.hash(), b.hash());
        let mut input = base(Mode::Once);
        input.instruction = Some("Different.".into());
        assert_ne!(a.hash(), build(&input).unwrap().hash());
    }

    #[test]
    fn loop_params_via_spec_mapping() {
        let spec = AgentSpec {
            mode: Mode::Loop,
            instruction: Some("tick".into()),
            loop_: Some(LoopParams {
                interval: "30s".into(),
                deadline: None,
            }),
            ..Default::default()
        };
        let input = ConfigInput::from_spec(&spec, None, None, None);
        let d = build(&input).unwrap().value;
        assert_eq!(d["workflows"][0]["steps"]["tick"]["interval"], "30s");
    }

    /// Ground truth: every document this builder emits must pass the real
    /// binary's own validation. Gated on AGENTD_BIN so `cargo test` stays
    /// hermetic; CI sets it to the pinned release binary.
    #[test]
    fn rendered_documents_validate_against_the_real_binary() {
        let Some(bin) = std::env::var_os("AGENTD_BIN") else {
            eprintln!("skipping: AGENTD_BIN not set");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let cases: Vec<ConfigInput> = vec![
            base(Mode::Once),
            {
                let mut i = base(Mode::Loop);
                i.loop_interval = Some("5m".into());
                i
            },
            {
                let mut i = base(Mode::Reactive);
                i.mcp = vec![ResolvedMcp {
                    name: "queue".into(),
                    endpoint: "http://127.0.0.1:8931/mcp".into(),
                    tags: vec![],
                    token_env: None,
                    header: None,
                }];
                i.subscribe = vec!["queue://inbox".into()];
                i
            },
        ];
        for (n, input) in cases.iter().enumerate() {
            // The document validates VERBATIM — the binary stats neither the
            // TLS/CA/store paths nor dials anything at --validate-config
            // (verified). Only two obligations exist: every `{{secret:…}}`
            // must resolve from env, and `file:` workflow refs must exist.
            let mut input = input.clone();
            input.intelligence = Some(ResolvedIntelligence {
                endpoint: "http://127.0.0.1:9999/v1".into(),
                model: Some("t".into()),
                has_token: true,
            });
            let d = build(&input).expect("build");
            let path = dir.path().join(format!("case{n}.json"));
            std::fs::write(&path, d.to_json()).unwrap();
            let mut cmd = std::process::Command::new(&bin);
            cmd.arg("-c")
                .arg(&path)
                .arg("--validate-config")
                .current_dir(dir.path());
            for name in d.secret_refs() {
                cmd.env(name, "validation-placeholder");
            }
            let out = cmd.output().expect("run agentd");
            assert!(
                out.status.success(),
                "case {n} refused by the binary:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn secret_refs_are_scanned_from_the_whole_document() {
        let mut input = base(Mode::Once);
        input.intelligence = Some(ResolvedIntelligence {
            endpoint: "https://llm/v1".into(),
            model: None,
            has_token: true,
        });
        input.mcp = vec![ResolvedMcp {
            name: "billing".into(),
            endpoint: "https://b/mcp".into(),
            tags: vec![],
            token_env: Some("AGENT_MCP_BILLING_TOKEN".into()),
            header: None,
        }];
        let d = build(&input).unwrap();
        let refs = d.secret_refs();
        assert!(refs.contains(&"INTELLIGENCE_TOKEN".to_string()));
        assert!(refs.contains(&"AGENT_MCP_BILLING_TOKEN".to_string()));
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn compose_from_spec_absolutizes_cluster_endpoints() {
        let spec = AgentSpec {
            mode: Mode::Once,
            instruction: Some("x".into()),
            mcp_servers: vec![McpServer {
                name: "fs".into(),
                endpoint: "https://fs.tenant.svc.cluster.local:8443/mcp".into(),
                auth: None,
                tags: vec![],
            }],
            ..Default::default()
        };
        let d = compose_from_spec(
            &spec,
            Some(ResolvedIntelligence {
                endpoint: "https://llm.ns.svc.cluster.local/v1".into(),
                model: None,
                has_token: false,
            }),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            d.value["intelligence"]["endpoints"],
            "https://llm.ns.svc.cluster.local./v1"
        );
        assert_eq!(
            d.value["mcp"]["servers"][0]["endpoint"],
            "https://fs.tenant.svc.cluster.local.:8443/mcp"
        );
    }
}
