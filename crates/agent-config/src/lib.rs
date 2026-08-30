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
    /// The instance-layer config document inside [`CONFIG_DIR`].
    pub const CONFIG_FILE: &str = "agentd.json";
    /// The catalog layer (RFC 0032 §4): the `services:` map. Layered FIRST
    /// (`-c services.json -c agentd.json`); folders adopt beside the LAST
    /// file (both live in [`CONFIG_DIR`], so the distinction is moot here).
    pub const SERVICES_FILE: &str = "services.json";
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
    /// Per-(user, agent) A2A principal bearers (the `<name>-principals`
    /// Secret volume; one file per subject, RFC 0028 §6).
    pub const PRINCIPALS_DIR: &str = "/etc/agentctl/principals";
    /// Outbound peer bearers (P4-7 @mention): per-peer copies of THE OWNER's
    /// principal bearer for each target, projected by the operator into the
    /// `<name>-peer-bearers` Secret — never another user's bearer.
    pub const PEER_BEARERS_DIR: &str = "/etc/agentctl/peer-bearers";

    /// The full in-pod path of the config document.
    pub fn config_file() -> String {
        format!("{CONFIG_DIR}/{CONFIG_FILE}")
    }

    /// The full in-pod path of the services catalog layer.
    pub fn services_file() -> String {
        format!("{CONFIG_DIR}/{SERVICES_FILE}")
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
    /// The consumer's NARROWED tool allow list (grant ∩ registry ceiling —
    /// resolved by the operator; empty = the catalog's full surface).
    pub allow: Vec<String>,
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
            allow: Vec::new(),
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub endpoint: String,
    /// Static bearer credential reference (`{{secret-file:…}}` template,
    /// resolved by agentd at dial time) — the @mention path carries the
    /// OWNER's principal bearer this way, so the callee attributes the hop
    /// to the human, never the calling pod. Rendered as
    /// `auth: {kind: static, token: …}` (the unified credential provider);
    /// header maps would be resolution-CHECKED by `--validate-config` and
    /// the file only exists in the workload pod.
    pub auth_bearer_ref: Option<String>,
    /// mTLS client cert/key paths presented on dial (the workload's own
    /// serving pair — the callee's `client_ca` admits the chart CA).
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
}

/// AAuth enrollment facts (RFC 0023 house-provisioning path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AauthInput {
    pub provider: String,
}

/// Where the agent's durable state lives (P3: the store classes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StoreSelector {
    /// The in-pod file store (ephemeral — an emptyDir; today's default).
    #[default]
    File,
    /// The managed state service: `store.kind: mcp` against a declared
    /// server, keys under `prefix` (the operator renders
    /// `orgs/<ns>/<agent>`; the AGENT_POD_NAME instance fence still applies
    /// inside the key).
    Mcp { server: String, prefix: String },
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
    /// Durable-state placement (P3 store classes).
    pub store: StoreSelector,
    /// Generated trigger workflows (P2-8: the v2 `triggers[]` compiler's
    /// output — full dialect-3 documents appended to `workflows`).
    pub generated_workflows: Vec<Value>,
    /// The `webhooks:` config block a webhook trigger requires (the config is
    /// REFUSED without `webhooks.listen` when a webhook start exists).
    pub webhooks_block: Option<Value>,
    /// The `streams:` declarations stream triggers consume.
    pub streams_block: Option<Value>,
    /// Named A2A principal subjects (`spec.access.principals`,
    /// `<provider>:<sub>`). Non-empty ⇒ `a2a.principals[]` is projected:
    /// one `user` rule per subject (bearer file under
    /// [`paths::PRINCIPALS_DIR`]) plus the control-plane `operator` rule —
    /// REQUIRED because any non-empty principals list switches off agentd's
    /// implicit loopback/management operator fallback.
    pub principal_subjects: Vec<String>,
    /// Typed-command grant patterns for the named principals (spec
    /// `access.grants`) — a command DataPart's `op` needs one; prose none.
    pub principal_grants: Vec<String>,
    /// Base-layer `vars:` (agentd folds `{{config.<key>}}` references
    /// anywhere in the document, type-preserving for whole-token
    /// substitution; unresolved references refuse startup). Fleet members
    /// override these via a per-member overlay layer.
    pub vars: Map<String, Value>,
    /// Singleton selectors (RFC 0034 §3.1): workflow entries these match — a
    /// trigger KIND (matching generated `main-<kind>-…` names) or an exact
    /// workflow/file-stem name — render `armed: "{{config.is_lead}}"`, so
    /// exactly the member whose vars say `is_lead: true` runs them.
    pub singleton_selectors: Vec<String>,
}

/// Does a singleton selector match a workflow entry name? Generated names
/// are `main-<kind>-<i>`; hand entries carry their own name/file stem.
pub fn singleton_matches(selector: &str, entry_name: &str) -> bool {
    entry_name == selector
        || entry_name == format!("main-{selector}")
        || entry_name.starts_with(&format!("main-{selector}-"))
}

/// The per-member overlay document (RFC 0034 §3.1): ONLY `vars:` — merged
/// over the shared layers by an extra `-c`, RFC 7396 key-by-key. `is_lead`
/// (ordinal 0) is what singleton-armed workflows fold on; `member` is the
/// ordinal for partition math in workflows/instructions.
pub fn member_overlay(
    ordinal: u32,
    defaults: Option<&Value>,
    member_vars: Option<&Value>,
) -> Value {
    let mut vars = Map::new();
    if let Some(Value::Object(d)) = defaults {
        for (k, v) in d {
            vars.insert(k.clone(), v.clone());
        }
    }
    if let Some(Value::Object(m)) = member_vars {
        for (k, v) in m {
            vars.insert(k.clone(), v.clone());
        }
    }
    vars.insert("member".into(), json!(ordinal.to_string()));
    vars.insert("is_lead".into(), json!(ordinal == 0));
    json!({ "vars": vars })
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
            store: StoreSelector::File,
            generated_workflows: Vec::new(),
            webhooks_block: None,
            streams_block: None,
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
            vars: Map::new(),
            singleton_selectors: Vec::new(),
        }
    }
}

pub mod v2;

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
    /// A subscribe trigger names a service with no resolved MCP binding —
    /// agentd would accept the config and the node would silently never fire.
    UnknownSubscribeServer { service: String },
    /// `shape: cron` (an external CronJob) needs CRON syntax; `every` cannot
    /// be expressed as a CronJob schedule.
    ExternalScheduleNeedsCron,
    /// A schedule trigger with neither `cron` nor `every`.
    ScheduleTriggerNeedsWhen,
    /// A trigger union with no member set (CEL-guarded; defense in depth).
    EmptyTrigger,
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
            ConfigError::UnknownSubscribeServer { service } => write!(
                f,
                "subscribe trigger names service {service:?} with no resolved MCP binding                  (grant an MCPService or declare it inline) — agentd would accept the config                  and the trigger would silently never fire"
            ),
            ConfigError::ExternalScheduleNeedsCron => write!(
                f,
                "shape: cron (an external CronJob) needs cron syntax on spec.schedule or the                  schedule trigger; `every` cannot be a CronJob schedule (use an internal                  schedule on a daemon)"
            ),
            ConfigError::ScheduleTriggerNeedsWhen => {
                write!(f, "a schedule trigger needs `cron` or `every`")
            }
            ConfigError::EmptyTrigger => write!(f, "a trigger must set exactly one kind"),
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
    /// The workload layer must mount a real `secretKeyRef` for each, and a
    /// validator exports a placeholder for each. NOTE (upstream-confirmed):
    /// `--validate-config` enforces resolution only for refs in HEADER maps —
    /// `intelligence.token` passes unresolved (defect raised upstream). The
    /// placeholder export stays correct under either behavior; missing-Secret
    /// detection therefore lives in admission's existence check, not here.
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

/// The rendered two-layer projection (RFC 0032 §4): the `services:` catalog
/// layer plus the instance layer, invoked as
/// `-c services.json -c agentd.json` (RFC 7396 merge; folders adopt beside
/// the LAST file). The catalog layer is ALWAYS emitted — an empty catalog
/// keeps argv/mounts uniform across every agent.
#[derive(Debug, Clone)]
pub struct Projection {
    pub services: ConfigDoc,
    pub instance: ConfigDoc,
}

impl Projection {
    /// Change-detector hash over BOTH layers (the pod-template annotation).
    pub fn hash(&self) -> String {
        ConfigDoc {
            value: json!({ "services": self.services.value, "instance": self.instance.value }),
        }
        .hash()
    }

    /// Every `{{secret:NAME}}` referenced by either layer, deduplicated.
    pub fn secret_refs(&self) -> Vec<String> {
        let mut out = self.services.secret_refs();
        for r in self.instance.secret_refs() {
            if !out.contains(&r) {
                out.push(r);
            }
        }
        out
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
) -> Result<Projection, ConfigError> {
    let intelligence = intelligence.map(|mut i| {
        i.endpoint = absolutize_endpoint(&i.endpoint);
        i
    });
    let mut input = ConfigInput::from_spec(spec, intelligence, workflow_file, aauth_provider);
    for m in &mut input.mcp {
        m.endpoint = absolutize_endpoint(&m.endpoint);
    }
    build_projection(&input)
}

/// Compose the two-layer projection: the services catalog (connection facts,
/// tags-as-floors, credential headers) + the instance layer whose
/// `mcp.servers[]` REFERENCE catalog entries (`service:` — restating
/// endpoint/auth in a consumer is refused by agentd, the reference-never-
/// restate law).
pub fn build_projection(input: &ConfigInput) -> Result<Projection, ConfigError> {
    Ok(Projection {
        services: services_layer(input),
        instance: build(input)?,
    })
}

/// The catalog layer: one `services:` entry per MCP binding. Kind is always
/// `mcp` — agentd 1.3.1's catalog accepts only that (schema "phase A"); the
/// registry's peer/http kinds project by other means until upstream U5.
fn services_layer(input: &ConfigInput) -> ConfigDoc {
    let mut services = Map::new();
    for m in &input.mcp {
        let mut entry = Map::new();
        entry.insert("kind".into(), json!("mcp"));
        entry.insert("endpoint".into(), json!(m.endpoint));
        if !m.tags.is_empty() {
            entry.insert("tags".into(), json!({ "*": m.tags }));
        }
        if let Some(env) = &m.token_env {
            let value = match &m.header {
                Some(_) => format!("{{{{secret:{env}}}}}"),
                None => format!("Bearer {{{{secret:{env}}}}}"),
            };
            let header = m.header.clone().unwrap_or_else(|| "Authorization".into());
            entry.insert("headers".into(), json!({ header: value }));
        }
        services.insert(m.name.clone(), Value::Object(entry));
    }
    ConfigDoc {
        value: json!({ "config_version": "1", "services": services }),
    }
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
    // Consumers REFERENCE the catalog (services layer) — never restate
    // endpoint/auth (agentd refuses restating; the catalog's tags/allow are
    // floors/ceilings the reference inherits).
    if !input.mcp.is_empty() {
        let servers: Vec<Value> = input
            .mcp
            .iter()
            .map(|m| {
                let mut e = Map::new();
                e.insert("name".into(), json!(m.name));
                e.insert("service".into(), json!(m.name));
                if !m.allow.is_empty() {
                    // The consumer's NARROWED tool surface (grant ∩ registry
                    // ceiling, resolved by the operator) — references may
                    // narrow the catalog, never widen it.
                    e.insert("allow".into(), json!(m.allow));
                }
                Value::Object(e)
            })
            .collect();
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
            workflows.push(workflow_file_entry(&file));
        }
    }
    // A reactive daemon may ALSO carry a user workflow graph.
    if input.mode == Mode::Reactive {
        if let Some(file) = &input.workflow_file {
            workflows.push(workflow_file_entry(file));
        }
    }
    workflows.extend(input.generated_workflows.iter().cloned());
    // Singleton arming (RFC 0034 §3.1): matched entries fold `armed` from the
    // member's own vars — every member LOADS the workflow, exactly the
    // `is_lead: true` member arms it. Whole-token substitution keeps the
    // folded value a real bool.
    if !input.singleton_selectors.is_empty() {
        for wf in &mut workflows {
            let name = wf
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if input
                .singleton_selectors
                .iter()
                .any(|sel| singleton_matches(sel, &name))
            {
                wf["armed"] = json!("{{config.is_lead}}");
            }
        }
    }
    if !workflows.is_empty() {
        doc.insert("workflows".into(), Value::Array(workflows));
    }
    // Base-layer vars. A fleet with singletons ALWAYS carries an `is_lead`
    // default (true — a solo daemon is its own lead) so the armed template
    // folds even before any member overlay lands.
    let mut vars = input.vars.clone();
    if !input.singleton_selectors.is_empty() {
        vars.entry("is_lead".to_string()).or_insert(json!(true));
    }
    if !vars.is_empty() {
        doc.insert("vars".into(), Value::Object(vars));
    }
    if let Some(w) = &input.webhooks_block {
        doc.insert("webhooks".into(), w.clone());
    }
    if let Some(st) = &input.streams_block {
        doc.insert("streams".into(), st.clone());
    }

    // --- serving ----------------------------------------------------------
    if input.serve_a2a {
        doc.insert(
            "a2a".into(),
            a2a_block(
                &input.peers,
                &input.principal_subjects,
                &input.principal_grants,
            ),
        );
    } else if !input.peers.is_empty() {
        doc.insert("a2a".into(), json!({ "peers": peer_entries(&input.peers) }));
    }

    // --- store + lifecycle ------------------------------------------------
    // The store is ALWAYS declared explicitly: even a one-shot run initializes
    // its state dir at boot, and the unset default lands on
    // $XDG_STATE_HOME/agentd/state — a read-only rootfs in our pods (observed:
    // exit 6 "store dir ...: Read-only file system"). The workload layer
    // mounts a writable emptyDir at the parent of STATE_DIR for every shape.
    match &input.store {
        StoreSelector::File => {
            doc.insert(
                "store".into(),
                json!({ "kind": "file", "file": { "path": paths::STATE_DIR } }),
            );
        }
        StoreSelector::Mcp { server, prefix } => {
            // The managed state service (checkpointer profile). The named
            // server MUST be a declared mcp.servers entry — agentd refuses an
            // undeclared name; the operator appends the synthetic binding.
            doc.insert(
                "store".into(),
                json!({ "kind": "mcp", "prefix": prefix, "mcp": { "server": server } }),
            );
        }
    }
    let run_until = if is_daemon(input.mode) {
        "drained"
    } else {
        "idle"
    };
    // watch_config is ON for every daemon: the key itself is RESTART-ONLY,
    // so an agent must be born watching to ever gain live reload; whether a
    // kubelet ConfigMap symlink swap actually fires the watcher is the U1
    // verification the reload classifier depends on (P2-5).
    let watch = is_daemon(input.mode);
    doc.insert(
        "lifecycle".into(),
        json!({ "run_until": run_until, "drain_timeout": DRAIN_TIMEOUT, "watch_config": watch }),
    );

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

fn a2a_block(peers: &[Peer], principal_subjects: &[String], principal_grants: &[String]) -> Value {
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
    // ALWAYS declared (RFC 0029 §4, "close the loopback-operator trap on all
    // agents"): even with no user subjects the list carries the control-plane
    // operator rule, and ANY non-empty list switches off agentd's implicit
    // loopback/management operator fallback — so an anonymous or loopback
    // caller is never silently the operator on any rendered agent.
    a2a.insert(
        "principals".into(),
        Value::Array(principal_entries(principal_subjects, principal_grants)),
    );
    Value::Object(a2a)
}

/// The control-plane client-cert identity (the chart's `agentctl-client`
/// Certificate CN). agentd's `san` matcher also checks the subject CN.
const OPERATOR_SAN: &str = "agentctl-control-plane";

/// The per-agent principals: one `user` rule per subject FIRST, then the
/// control-plane `operator` rule LAST. Order is load-bearing twice over:
/// agentd matches rules first-listed-first, and the gateway presents the
/// control-plane client cert on EVERY forwarded call — with the operator rule
/// first, an injected user bearer could never win. And the operator rule must
/// exist at all because any non-empty principals list disables agentd's
/// implicit management/loopback operator fallback (omitting it bricks the
/// management verbs). Each bearer is a `{{secret-file:…}}` TEMPLATE — never a
/// literal: agentd uses a `bearer_ref` without `{{` VERBATIM as the
/// credential, so a literal here would embed a secret in the ConfigMap.
fn principal_entries(subjects: &[String], grants: &[String]) -> Vec<Value> {
    let mut out = Vec::with_capacity(subjects.len() + 1);
    for subject in subjects {
        let bearer_ref = format!(
            "{{{{secret-file:{}/{}}}}}",
            paths::PRINCIPALS_DIR,
            principal_secret_key(subject)
        );
        debug_assert!(bearer_ref.contains("{{"), "bearer_ref must be a template");
        let mut entry = json!({
            "match": { "bearer_ref": bearer_ref },
            "role": "user",
            "labels": { "user": subject },
        });
        if !grants.is_empty() {
            entry["grants"] = json!(grants);
        }
        out.push(entry);
    }
    out.push(json!({
        "match": { "san": OPERATOR_SAN },
        "role": "operator",
    }));
    out
}

/// A subject's key inside the `<name>-principals` Secret (env/file-safe).
/// MUST stay byte-identical to `agentctl_identity::principals::
/// principal_secret_key` — the identity service names the key at mint time and
/// this builder references it in `bearer_ref`; a divergence produces a
/// dangling ref that `--validate-config` does NOT catch (startup exit 2 does).
pub fn principal_secret_key(subject: &str) -> String {
    let safe: String = subject
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("PRINCIPAL_{}", safe.to_uppercase())
}

/// A `{name, file}` workflow reference — agentd refuses a bare `{file}`
/// ("workflows[0] has no name", caught by the gated binary test). The entry
/// name is the file stem; the document keeps its own internal `name`.
fn workflow_file_entry(file: &str) -> Value {
    let stem = std::path::Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("workflow");
    json!({ "name": stem, "file": file })
}

fn peer_entries(peers: &[Peer]) -> Vec<Value> {
    peers
        .iter()
        .map(|p| {
            let mut e = Map::new();
            e.insert("name".into(), json!(p.name));
            e.insert("endpoint".into(), json!(p.endpoint));
            if let Some(token) = &p.auth_bearer_ref {
                e.insert("auth".into(), json!({ "kind": "static", "token": token }));
            }
            if let Some(cert) = &p.client_cert {
                e.insert("client_cert".into(), json!(cert));
            }
            if let Some(key) = &p.client_key {
                e.insert("client_key".into(), json!(key));
            }
            Value::Object(e)
        })
        .collect()
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
        // The store is explicit for EVERY shape (a defaulted store lands on
        // the read-only rootfs — observed exit 6 in-cluster).
        assert_eq!(d["store"]["kind"], "file");
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
                allow: Vec::new(),
            },
            ResolvedMcp {
                name: "fs".into(),
                endpoint: "https://fs.internal/mcp".into(),
                tags: vec![],
                token_env: None,
                header: None,
                allow: Vec::new(),
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

    /// The per-user principal projection (RFC 0028 §6): the exact bearer_ref
    /// template and the mandatory operator rule are contract surface — agentd
    /// uses a bearer_ref without `{{` VERBATIM as the credential, and any
    /// non-empty principals list disables the implicit loopback operator.
    #[test]
    fn principals_project_operator_rule_and_file_templates() {
        let mut input = ConfigInput {
            mode: Mode::Reactive,
            serve_a2a: true,
            ..Default::default()
        };
        input.principal_subjects = vec!["okta:alice".into(), "mock:bob-1".into()];
        let d = build(&input).unwrap().value;

        let principals = d["a2a"]["principals"].as_array().unwrap();
        assert_eq!(principals.len(), 3);
        // Subject rules FIRST (first match wins, and the gateway presents the
        // control-plane cert on every forwarded call — the injected user
        // bearer must outrank it): role REQUIRED by the schema; bearer_ref is
        // a secret-file TEMPLATE under the principals mount, never a literal.
        assert_eq!(
            principals[0]["match"]["bearer_ref"],
            "{{secret-file:/etc/agentctl/principals/PRINCIPAL_OKTA_ALICE}}"
        );
        assert_eq!(principals[0]["role"], "user");
        assert_eq!(principals[0]["labels"]["user"], "okta:alice");
        assert_eq!(
            principals[1]["match"]["bearer_ref"],
            "{{secret-file:/etc/agentctl/principals/PRINCIPAL_MOCK_BOB_1}}"
        );
        // The control-plane operator rule LAST (management verbs survive —
        // any non-empty list disables the implicit fallback).
        assert_eq!(principals[2]["match"]["san"], "agentctl-control-plane");
        assert_eq!(principals[2]["role"], "operator");

        // No subjects ⇒ STILL a principals list with the operator rule: the
        // loopback-operator trap is closed on every rendered agent (RFC 0029
        // §4), and the control plane keeps its management path via the cert.
        let bare = build(&ConfigInput {
            mode: Mode::Reactive,
            serve_a2a: true,
            ..Default::default()
        })
        .unwrap()
        .value;
        let bare_principals = bare["a2a"]["principals"].as_array().unwrap();
        assert_eq!(bare_principals.len(), 1);
        assert_eq!(bare_principals[0]["match"]["san"], "agentctl-control-plane");
        assert_eq!(bare_principals[0]["role"], "operator");
    }

    /// Byte-identical with `agentctl_identity::principals::principal_secret_key`
    /// (the identity service names the Secret key at mint; this crate
    /// references it) — pin the mapping so a drift is a test failure here.
    #[test]
    fn principal_secret_key_mapping_is_pinned() {
        assert_eq!(principal_secret_key("okta:alice"), "PRINCIPAL_OKTA_ALICE");
        assert_eq!(principal_secret_key("a.b-c_d"), "PRINCIPAL_A_B_C_D");
    }

    /// The managed store class (P3): `store.kind: mcp` against the declared
    /// `state` binding with the operator-computed prefix — the exact shape
    /// agentd's checkpointer profile dials.
    #[test]
    fn managed_store_renders_the_mcp_checkpointer() {
        let mut input = base(Mode::Reactive);
        input.store = StoreSelector::Mcp {
            server: "state".into(),
            prefix: "orgs/org-acme/triage".into(),
        };
        input.mcp = vec![ResolvedMcp {
            name: "state".into(),
            endpoint: "http://agentctl-state.agentctl-system.svc.cluster.local.:8787/mcp".into(),
            tags: vec![],
            token_env: None,
            header: None,
            allow: vec!["state.*".into()],
        }];
        let proj = build_projection(&input).unwrap();
        let store = &proj.instance.value["store"];
        assert_eq!(store["kind"], "mcp");
        assert_eq!(store["prefix"], "orgs/org-acme/triage");
        assert_eq!(store["mcp"]["server"], "state");
        // The narrowed allow rides the reference entry.
        assert_eq!(
            proj.instance.value["mcp"]["servers"][0]["allow"][0],
            "state.*"
        );
        // The catalog carries the connection fact.
        assert!(proj.services.value["services"]["state"]["endpoint"]
            .as_str()
            .unwrap()
            .contains(":8787/mcp"));
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
        // Connection facts (endpoint/tags/headers) live in the CATALOG layer
        // now; the instance entry only references (`service:`).
        let proj = build_projection(&input).unwrap();
        let cat = &proj.services.value["services"]["billing-api"];
        assert_eq!(cat["kind"], "mcp");
        assert_eq!(cat["tags"]["*"][0], "sensitive");
        assert_eq!(
            cat["headers"]["Authorization"],
            "Bearer {{secret:AGENT_MCP_BILLING_API_TOKEN}}"
        );
        // PINNED SPELLING (upstream-confirmed asymmetry): only HEADER-map
        // secret refs are enforced by --validate-config; the `auth.token`
        // spelling passes unresolved. The builder must therefore never emit
        // `auth:`/`token:` on a catalog entry — headers are the enforced form.
        assert!(cat.get("auth").is_none());
        assert!(cat.get("token").is_none());
        let entry = &proj.instance.value["mcp"]["servers"][0];
        assert_eq!(entry["name"], "billing-api");
        assert_eq!(entry["service"], "billing-api");
        // Reference-never-restate: no endpoint/headers on the consumer.
        assert!(entry.get("endpoint").is_none());
        assert!(entry.get("headers").is_none());
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

    /// P6-1: member overlays carry ONLY vars; ordinal 0 is the lead;
    /// per-member entries override fleet defaults key-by-key.
    #[test]
    fn member_overlays_are_vars_only_and_lead_is_zero() {
        let defaults = json!({ "region": "eu", "batch": 100 });
        let m1 = json!({ "region": "us" });
        let o0 = member_overlay(0, Some(&defaults), None);
        let o1 = member_overlay(1, Some(&defaults), Some(&m1));
        assert_eq!(o0["vars"]["is_lead"], json!(true));
        assert_eq!(o1["vars"]["is_lead"], json!(false));
        assert_eq!(o0["vars"]["member"], "0");
        assert_eq!(o1["vars"]["region"], "us");
        assert_eq!(o1["vars"]["batch"], 100);
        // Nothing but vars rides an overlay (RFC 7396 arrays replace
        // wholesale — an overlay must never carry workflows).
        assert_eq!(o1.as_object().unwrap().len(), 1);
    }

    /// P6-1: singleton selectors stamp `armed: {{config.is_lead}}` on the
    /// matched workflow entries (generated `main-<kind>-…` by kind, hand
    /// entries by name), and the base layer carries an `is_lead` default so
    /// a solo daemon still arms.
    #[test]
    fn singletons_arm_only_matched_workflows() {
        let mut input = base(Mode::Reactive);
        input.generated_workflows = vec![
            json!({ "name": "main-schedule-0", "version": 3, "steps": {} }),
            json!({ "name": "main-loop-1", "version": 3, "steps": {} }),
        ];
        input.singleton_selectors = vec!["schedule".into()];
        let d = build(&input).unwrap().value;
        let wfs = d["workflows"].as_array().unwrap();
        let by_name = |n: &str| wfs.iter().find(|w| w["name"] == n).unwrap().clone();
        assert_eq!(by_name("main-schedule-0")["armed"], "{{config.is_lead}}");
        assert!(by_name("main-loop-1").get("armed").is_none());
        assert_eq!(d["vars"]["is_lead"], json!(true));

        // Selector grammar.
        assert!(singleton_matches("schedule", "main-schedule-0"));
        assert!(singleton_matches("nightly-report", "nightly-report"));
        assert!(!singleton_matches("schedule", "main-loop-0"));
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
                    allow: Vec::new(),
                }];
                i.subscribe = vec!["queue://inbox".into()];
                i
            },
        ];
        // The P2-8 ten-kind daemon: every trigger compiles into this ONE
        // document — the strongest possible check that the generated start
        // nodes carry exactly the field names 1.3.1's strict validator wants.
        let ten_kind: agent_api::v1alpha2::AgentSpec = serde_yaml::from_str(
            r#"
shape: daemon
instruction: { text: "persona" }
expose: { a2a: true }
triggers:
  - once: {}
  - manual: {}
  - loop: { interval: 10m }
  - schedule: { cron: "0 7 * * 1-5" }
  - schedule: { every: 1h }
  - webhook: { path: /hooks/ci, methods: [POST], rate: "30/60s" }
  - subscribe: { service: queue, uri: "queue://inbox", debounce: 500ms }
  - stream: { stream: incidents, from: new }
  - signal: { name: "reply/42" }
  - event: { name: workflow.finished }
  - a2aCommand: { command: qa.verify }
"#,
        )
        .unwrap();
        let (ten_input, _) = v2::from_v2_spec(
            &ten_kind,
            Some(ResolvedIntelligence {
                endpoint: "http://127.0.0.1:9999/v1".into(),
                model: Some("t".into()),
                has_token: false,
            }),
            None,
            None,
            vec![ResolvedMcp {
                name: "queue".into(),
                endpoint: "http://127.0.0.1:8931/mcp".into(),
                tags: vec![],
                token_env: None,
                header: None,
                allow: Vec::new(),
            }],
        )
        .expect("ten-kind compile");
        let mut cases = cases;
        cases.push(ten_input);

        // The P4-7 supervisor shape: a daemon with a HAND-AUTHORED mention
        // workflow (a2a start + switch hop guard + foreach a2a.delegate +
        // template gather), owner-authenticated peers (static-auth
        // secret-file bearer + mTLS client pair), and a principal-gated a2a
        // surface — the exact document the supervisor controller renders.
        {
            let sup_spec: agent_api::v1alpha2::AgentSpec = serde_yaml::from_str(
                r#"
shape: daemon
instruction: { text: "supervisor persona" }
expose: { a2a: true }
access: { principals: ["mock:carol"] }
"#,
            )
            .unwrap();
            let wf_path = dir.path().join("mention-workflow.json");
            let mention = serde_json::json!({
                "name": "mention",
                "version": 3,
                "concurrency": { "max_runs": 1, "on_overflow": "queue", "scope": "workflow" },
                "steps": {
                    "start": { "kind": "a2a", "command": "mention" },
                    "guard": { "kind": "switch", "depends_on": ["start"],
                               "on": "{{steps.start.output.args.hops}}",
                               "cases": { "0": "stopped" }, "default": "fan" },
                    "stopped": { "kind": "finish", "depends_on": ["guard"], "status": "completed",
                                 "output": "mention hop ceiling reached — not fanning out" },
                    "fan": { "kind": "foreach", "depends_on": ["guard"],
                             "over": "{{steps.start.output.args.mentions}}",
                             "as": "item", "on_error": "continue",
                             "body": { "steps": { "ask": { "kind": "a2a.delegate", "peer": "{{item}}",
                                                "objective": "{{steps.start.output.args.text}}",
                                                "timeout": "60s" } } } },
                    "render": { "kind": "template", "depends_on": ["fan"],
                                "value": { "mentions": "{{steps.start.output.args.mentions}}",
                                           "answers": "{{steps.fan.output}}" } },
                    "done": { "kind": "finish", "depends_on": ["render"], "status": "completed",
                              "output": "{{steps.render.output}}" }
                }
            });
            std::fs::write(&wf_path, serde_json::to_string_pretty(&mention).unwrap()).unwrap();
            let (mut sup_input, _) = v2::from_v2_spec(
                &sup_spec,
                None,
                Some(wf_path.to_string_lossy().into_owned()),
                None,
                Vec::new(),
            )
            .expect("supervisor compile");
            sup_input.peers = vec![Peer {
                name: "helper".into(),
                endpoint: "https://helper.org-acme.svc.cluster.local.:8443".into(),
                auth_bearer_ref: Some("{{secret-file:/etc/agentctl/peer-bearers/helper}}".into()),
                client_cert: Some(paths::TLS_CERT.into()),
                client_key: Some(paths::TLS_KEY.into()),
            }];
            cases.push(sup_input);
        }

        // P6-1 static-fleet member: the shared daemon document (schedule
        // trigger compiled to a generated workflow, singleton-armed with the
        // `{{config.is_lead}}` template) PLUS the per-member overlay as a
        // THIRD `-c` — exactly the trio a fleet pod is invoked with. Proves
        // the binary folds `armed` to a real bool from the overlay's vars.
        {
            let fleet_template: agent_api::v1alpha2::AgentSpec = serde_yaml::from_str(
                r#"
shape: daemon
instruction: { text: "fleet worker persona for {{config.region}}" }
expose: { a2a: true }
triggers:
  - schedule: { cron: "0 3 * * *" }
  - loop: { interval: 10m }
"#,
            )
            .unwrap();
            let (mut member_input, _) = v2::from_v2_spec(
                &fleet_template,
                Some(ResolvedIntelligence {
                    endpoint: "http://127.0.0.1:9999/v1".into(),
                    model: Some("t".into()),
                    has_token: false,
                }),
                None,
                None,
                Vec::new(),
            )
            .expect("fleet member compile");
            member_input.singleton_selectors = vec!["schedule".into()];
            // Referenced vars MUST default in the base layer (agentd types
            // each file independently before the merge — a base-layer
            // {{config.*}} with no base-layer default refuses startup).
            member_input
                .vars
                .insert("region".into(), serde_json::json!("eu"));
            let proj = build_projection(&member_input).expect("build");
            let overlay = member_overlay(
                1,
                Some(&serde_json::json!({ "region": "eu" })),
                Some(&serde_json::json!({ "region": "us" })),
            );
            let svc = dir.path().join("member-services.json");
            let base = dir.path().join("member-agentd.json");
            let over = dir.path().join("member-1.json");
            std::fs::write(&svc, proj.services.to_json()).unwrap();
            std::fs::write(&base, proj.instance.to_json()).unwrap();
            std::fs::write(&over, serde_json::to_string_pretty(&overlay).unwrap()).unwrap();
            let mut cmd = std::process::Command::new(&bin);
            cmd.arg("-c")
                .arg(&svc)
                .arg("-c")
                .arg(&base)
                .arg("-c")
                .arg(&over)
                .arg("--validate-config")
                .current_dir(dir.path());
            for name in proj.secret_refs() {
                cmd.env(name, "validation-placeholder");
            }
            let out = cmd.output().expect("run agentd");
            assert!(
                out.status.success(),
                "static-member trio refused by the binary:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

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
            // The REAL invocation shape: catalog first, instance last.
            let proj = build_projection(&input).expect("build");
            let svc_path = dir.path().join(format!("case{n}-services.json"));
            let path = dir.path().join(format!("case{n}.json"));
            std::fs::write(&svc_path, proj.services.to_json()).unwrap();
            std::fs::write(&path, proj.instance.to_json()).unwrap();
            let mut cmd = std::process::Command::new(&bin);
            cmd.arg("-c")
                .arg(&svc_path)
                .arg("-c")
                .arg(&path)
                .arg("--validate-config")
                .current_dir(dir.path());
            for name in proj.secret_refs() {
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
            allow: Vec::new(),
        }];
        // Refs span BOTH layers: the intelligence token in the instance, the
        // MCP header token in the catalog — the projection unions them.
        let proj = build_projection(&input).unwrap();
        let refs = proj.secret_refs();
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
            d.instance.value["intelligence"]["endpoints"],
            "https://llm.ns.svc.cluster.local./v1"
        );
        // The connection fact lives in the CATALOG layer; the instance entry
        // only references it.
        assert_eq!(
            d.services.value["services"]["fs"]["endpoint"],
            "https://fs.tenant.svc.cluster.local.:8443/mcp"
        );
        assert_eq!(d.instance.value["mcp"]["servers"][0]["service"], "fs");
    }
}
