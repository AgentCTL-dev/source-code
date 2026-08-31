// SPDX-License-Identifier: BUSL-1.1
//! Pure workload rendering: an [`Agent`]/[`AgentFleet`] → the Kubernetes
//! workload that runs it.
//!
//! **ACC 2: the agent is a config document, the pod is its shell.** The
//! rewritten reference agent has no execution-mode flags — everything the v1
//! renderer said in argv (`--mode`, `--serve-mcp`, `--mcp`, `--shard`, …) now
//! lives in the `config_version: "1"` document that `agent-config` composes and
//! the controller ships as a ConfigMap. What remains here is the *workload*
//! half: shape (Job/CronJob/Deployment/StatefulSet), volumes and env-secret
//! mounts the document's `{{secret:…}}` references resolve against, probes,
//! drain-aware termination grace, the `podFailurePolicy` compiled from the
//! vendored exit-code intents, and the config-hash annotation that turns a
//! document change into a rolling restart (ADR-0007's safe interim delivery).
//!
//! Invocation is exactly `agentd -c /etc/agentctl/config/agentd.json`; the
//! only other argv is nothing — even the metrics listener rides the document.

use std::collections::BTreeMap;

use agent_api::{Agent, AgentFleet, Mode, ScaleMode, Substrate};
use agent_config::paths;
use agent_contract_client::exit_codes::{Intent, Table};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, StatefulSet, StatefulSetSpec};
use k8s_openapi::api::batch::v1::{
    CronJob, CronJobSpec, Job, JobSpec, JobTemplateSpec, PodFailurePolicy,
    PodFailurePolicyOnExitCodesRequirement, PodFailurePolicyOnPodConditionsPattern,
    PodFailurePolicyRule,
};
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HTTPGetAction, ObjectFieldSelector, PodSecurityContext, PodSpec, PodTemplateSpec,
    Probe, SeccompProfile, SecretKeySelector, SecretVolumeSource, SecurityContext, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

/// API group/version these resources are owned by (agent-api `GROUP`).
const API_VERSION: &str = "agentctl.dev/v1alpha1";

/// In-pod mount of the workload's own serving identity — the cert-manager
/// `Certificate` Secret ([`serving_secret_name`], keys `tls.crt`/`tls.key`).
/// The document's `a2a.tls` points here. NOTE (upstream ask U2): the rewritten
/// agent reads A2A TLS material once at listener start — a cert-manager
/// renewal therefore requires a rolling restart, which the controller drives.
const TLS_MOUNT: &str = "/etc/agentctl/tls";
const TLS_VOLUME: &str = "agentctl-serving-tls";

/// In-pod mount of the cluster CA **public certificate** (ConfigMap
/// [`CA_CONFIGMAP`], key `ca.crt`). Doubles as the A2A `client_ca` (who may
/// call me) and the outbound trust anchor (`security.tls_ca`).
const CA_MOUNT: &str = "/etc/agentctl/ca";
const CA_VOLUME: &str = "agentctl-ca";
/// The per-namespace ConfigMap carrying the cluster CA cert (public material).
pub const CA_CONFIGMAP: &str = "agentctl-ca";
/// Key within [`CA_CONFIGMAP`] (and the mounted filename) holding the CA PEM.
pub const CA_KEY: &str = "ca.crt";

/// The rendered config document's ConfigMap volume.
const CONFIG_VOLUME: &str = "agentctl-config";
/// The daemon file-store volume (an emptyDir at [`paths::STATE_DIR`]'s parent).
const STATE_VOLUME: &str = "agentd-state";
const STATE_MOUNT: &str = "/var/lib/agentd";

/// The HTTPS port every rendered agent serves its A2A surface on
/// (`a2a.listen: https://0.0.0.0:8443` in the document).
pub const SERVE_PORT: i32 = 8443;
/// The probe/scrape listener port (`observability.metrics_addr`).
pub const METRICS_PORT: i32 = 9090;

/// `terminationGracePeriodSeconds`: strictly above the agent's worst-case
/// drain (`drain_timeout` 25 s + abandon 3 s = 28 s, per the vendored
/// contract), so a clean SIGTERM drain always exits 0 — never a kernel 143.
pub const TERMINATION_GRACE_SECONDS: i64 = 30;

/// Pod-template annotation carrying the config document's hash: a document
/// change rolls the pods (the safe delivery until upstream reload lands, U1).
pub const CONFIG_HASH_ANNOTATION: &str = "agentctl.dev/config-hash";

/// Pod-template annotation carrying a shard fleet's applied partition count.
/// The guarded-resize state machine reads it back from the LIVE StatefulSet as
/// its durable "applied N" memory (it survives the quiesce-to-zero, which
/// `spec.replicas` does not) — the role the removed `--shard auto/N` argv
/// used to play.
pub const SHARDS_ANNOTATION: &str = "agentctl.dev/shards";

/// The serving-identity Secret name for a workload (cert-manager
/// `Certificate.spec.secretName`; created by the operator, mounted at
/// [`TLS_MOUNT`]).
pub fn serving_secret_name(workload: &str) -> String {
    format!("{workload}-serving-tls")
}

/// The rendered-config ConfigMap name for a workload.
pub fn config_configmap_name(workload: &str) -> String {
    format!("{workload}-config")
}

/// The generated-ConfigMap name for an inline workflow on a workload.
pub fn workflow_configmap_name(workload: &str) -> String {
    format!("{workload}-workflow")
}

/// Operator-scoped render inputs that do not live on the CR. Built once by the
/// controller from its environment; a test passes a literal.
#[derive(Debug, Clone)]
pub struct RenderConfig {
    /// The A2A gateway base URL a coordinator's worker peer
    /// (`a2a.peers[]` in its document) is rendered against for
    /// `distribution: a2a`.
    pub gateway_url: String,
    /// Operator-configured **default agent image**; `spec.image` overrides.
    pub default_agent_image: Option<String>,
}

/// Default in-cluster A2A gateway URL (chart Service, control-plane namespace).
pub const DEFAULT_GATEWAY_URL: &str =
    "http://agentctl-gateway.agentctl-system.svc.cluster.local.:8080";

impl Default for RenderConfig {
    fn default() -> Self {
        RenderConfig {
            gateway_url: DEFAULT_GATEWAY_URL.to_string(),
            default_agent_image: None,
        }
    }
}

impl RenderConfig {
    /// Build from the operator environment (`AGENTCTL_GATEWAY_URL`,
    /// `AGENTCTL_DEFAULT_AGENT_IMAGE`), falling back to in-cluster defaults.
    pub fn from_env() -> Self {
        let d = Self::default();
        let env = |k: &str, dflt: String| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or(dflt)
        };
        RenderConfig {
            gateway_url: env("AGENTCTL_GATEWAY_URL", d.gateway_url),
            default_agent_image: std::env::var("AGENTCTL_DEFAULT_AGENT_IMAGE")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        }
    }
}

/// Resolve the container image: explicit `spec.image` wins; else the
/// operator's default; else an error.
fn resolve_image(image: &Option<String>, cfg: &RenderConfig) -> Result<String, RenderError> {
    image
        .clone()
        .or_else(|| cfg.default_agent_image.clone())
        .ok_or(RenderError::MissingImage)
}

/// An env-secret the pod mounts so a `{{secret:<env>}}` reference in the
/// document resolves (an MCP bearer, the intelligence token, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEnv {
    pub env: String,
    pub secret: String,
    pub key: String,
}

/// A mounted workflow document (ConfigMap key file the document references).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMount {
    pub config_map: String,
    pub key: String,
}

impl WorkflowMount {
    /// The in-pod file path the config document's `workflows: [{file: …}]`
    /// entry must reference.
    pub fn file_path(&self) -> String {
        format!("{}/{}", paths::WORKFLOW_DIR, self.key)
    }
}

/// Everything the pod shell needs beyond the CR: the document identity and the
/// mounts/envs its `{{secret:…}}` references and `file:` entries resolve
/// against. Composed by the controller alongside the `agent-config` document —
/// the two MUST agree (same secret env names, same file paths), which is why
/// both derive from the same resolved facts.
#[derive(Debug, Clone, Default)]
pub struct PodWiring {
    /// `ConfigDoc::hash()` — stamps [`CONFIG_HASH_ANNOTATION`].
    pub config_hash: String,
    /// Mount for `{{secret:INTELLIGENCE_TOKEN}}` (the bound ModelPool's key).
    pub intelligence_token: Option<agent_api::SecretKeyRef>,
    /// Mounts for `{{secret:AGENT_MCP_*_TOKEN}}` references.
    pub mcp_tokens: Vec<SecretEnv>,
    /// The workflow ConfigMap the document's `file:` entry references.
    pub workflow: Option<WorkflowMount>,
    /// Mount the AAuth key Secret (`<workload>-aauth-key`) at
    /// [`paths::AAUTH_KEY`] (the document's `security.aauth.key_file`).
    pub aauth_key: bool,
    /// Inject the in-cluster `AGENTCTL_API_TOKEN` bearer (chart apiToken).
    pub api_token: bool,
    /// Mount the `<workload>-principals` Secret at [`paths::PRINCIPALS_DIR`]
    /// (per-user A2A bearers the document's `a2a.principals[].bearer_ref`
    /// templates reference; RFC 0028 §6). The operator lands the Secret BEFORE
    /// the config references it — a dangling ref is a startup exit 2 that
    /// `--validate-config` does not catch.
    pub principals: bool,
    /// Mount the `<workload>-peer-bearers` Secret at
    /// [`paths::PEER_BEARERS_DIR`] (P4-7: the OWNER's bearer per dialable
    /// peer, referenced by the peers' header `secret-file` templates).
    pub peer_bearers: bool,
    /// Extra plain env (fleet work-fabric coordinates for a coordinator).
    pub extra_env: Vec<(String, String)>,
    /// Static-fleet member overlays (RFC 0034 §3.1): appends a third config
    /// layer `-c <CONFIG_DIR>/member-$(AGENT_POD_INDEX).json` (the per-member
    /// `vars:` overlay in the SAME shared ConfigMap) and the downward-API
    /// `AGENT_POD_INDEX` env (the StatefulSet pod-index label).
    pub member_overlays: bool,
    /// Declare the `hooks` container port (:9494) — any webhook trigger
    /// binds agentd's listener on the pod network for the gateway's hooks
    /// proxy (P7-1; NetworkPolicy admits only the control plane).
    pub hooks_port: bool,
    /// Mount the `<workload>-hooks` Secret at [`paths::HOOKS_SECRETS_DIR`]
    /// (per-route HMAC/bearer values the webhook auth blocks reference).
    pub hooks_secrets: bool,
    /// `store.class: local` (P3-4): the agentd file store on a durable PVC
    /// instead of an emptyDir — the render becomes a single-replica
    /// StatefulSet whose volumeClaimTemplate provides the state dir (survives
    /// pod reschedule; node-local durability between ephemeral and managed).
    /// `Some(size)` ⇒ local; the pod template drops the state emptyDir.
    pub local_store_size: Option<String>,
}

/// In-pod mount of the AAuth key Secret.
const AAUTH_MOUNT: &str = "/etc/agentctl/aauth";
const AAUTH_VOLUME: &str = "agentctl-aauth-key";
const PRINCIPALS_VOLUME: &str = "agentctl-principals";

/// The per-agent principals Secret (one key per subject; identity mints, the
/// operator projects).
pub fn principals_secret_name(workload: &str) -> String {
    format!("{workload}-principals")
}

const PEER_BEARERS_VOLUME: &str = "agentctl-peer-bearers";

/// The per-agent OUTBOUND peer-bearer Secret (P4-7; one key per peer handle).
pub fn peer_bearers_secret_name(workload: &str) -> String {
    format!("{workload}-peer-bearers")
}

const HOOKS_VOLUME: &str = "agentctl-hooks";

/// Delivery-activity stamp (P6-5): shared with the gateway via agent-api.
pub use agent_api::LAST_DELIVERY_ANNOTATION;

/// The per-agent webhook-route Secret (P7-1; `hmac-<i>`/`bearer-<i>` keys).
pub fn hooks_secret_name(workload: &str) -> String {
    format!("{workload}-hooks")
}

/// Writable scratch over the read-only root filesystem.
const TMP_MOUNT: &str = "/tmp";
const TMP_VOLUME: &str = "tmp";

/// Secret holding the optional in-cluster bearer token (chart `apiToken.enabled`).
pub const API_TOKEN_SECRET: &str = "agentctl-api-token";
/// Env var (and Secret key) the gated services read the bearer token from.
pub const API_TOKEN_ENV: &str = "AGENTCTL_API_TOKEN";

/// What the renderer produced. Boxed to keep the enum small (clippy).
#[derive(Debug, Clone, PartialEq)]
pub enum Rendered {
    /// `once`/`workflow` mode → a batch Job.
    Job(Box<Job>),
    /// `schedule` mode → a CronJob firing the Job on its cron.
    CronJob(Box<CronJob>),
    /// `loop`/`reactive` Agent, or a claim-mode AgentFleet → a Deployment.
    Deployment(Box<Deployment>),
    /// A shard-mode AgentFleet → a StatefulSet (stable per-replica identity —
    /// `AGENT_POD_NAME` is the store fence; partition semantics live upstream
    /// of the agent per ADR-0009).
    StatefulSet(Box<StatefulSet>),
}

/// Why rendering could not proceed (surfaced as `Validated=False`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The resource has no `.metadata.name`.
    MissingName,
    /// No image: neither `spec.image` nor the operator default is set.
    MissingImage,
    /// A shard-mode fleet did not set `scaling.shards`.
    MissingShards,
    /// A substrate this renderer does not yet implement.
    UnsupportedSubstrate(Substrate),
    /// `mode: schedule` without `spec.schedule` (CEL also enforces this).
    MissingSchedule,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::MissingName => write!(f, "resource has no metadata.name"),
            RenderError::MissingImage => write!(
                f,
                "image is required: set spec.image, or configure the operator's \
                 default agent image (operator.defaultAgentImage / AGENTCTL_DEFAULT_AGENT_IMAGE)"
            ),
            RenderError::MissingShards => write!(
                f,
                "shard-mode fleet requires scaling.shards (the partition count N)"
            ),
            RenderError::UnsupportedSubstrate(s) => {
                write!(f, "substrate {s:?} not implemented by this renderer")
            }
            RenderError::MissingSchedule => {
                write!(f, "mode 'schedule' requires spec.schedule.cron")
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// Render an `Agent` to its workload (shape mapping — the document itself is
/// the controller's `agent-config` output, referenced by hash in `wiring`).
pub fn render_agent(
    agent: &Agent,
    cfg: &RenderConfig,
    wiring: &PodWiring,
) -> Result<Rendered, RenderError> {
    let name = agent
        .metadata
        .name
        .clone()
        .ok_or(RenderError::MissingName)?;
    let image = resolve_image(&agent.spec.image, cfg)?;
    require_stock_unix(agent.spec.substrate)?;

    let labels = managed_labels(&name);
    let meta = owned_meta(
        &name,
        agent.metadata.namespace.clone(),
        &labels,
        owner_ref("Agent", &name, uid_of(&agent.metadata.uid)),
    );
    let template = pod_template(&name, &image, agent.spec.mode, &labels, wiring);

    Ok(match agent.spec.mode {
        Mode::Once | Mode::Workflow => Rendered::Job(Box::new(Job {
            metadata: meta,
            spec: Some(job_spec(template)),
            ..Default::default()
        })),
        Mode::Schedule => {
            let schedule = agent
                .spec
                .schedule
                .as_ref()
                .ok_or(RenderError::MissingSchedule)?;
            Rendered::CronJob(Box::new(CronJob {
                metadata: meta,
                spec: CronJobSpec {
                    schedule: schedule.cron.clone(),
                    time_zone: schedule.timezone.clone(),
                    concurrency_policy: Some("Forbid".to_string()),
                    job_template: JobTemplateSpec {
                        metadata: Some(ObjectMeta {
                            labels: Some(labels.clone()),
                            ..Default::default()
                        }),
                        spec: Some(job_spec(template)),
                    },
                    ..Default::default()
                },
                ..Default::default()
            }))
        }
        Mode::Loop | Mode::Reactive => match &wiring.local_store_size {
            // `local` store (P3-4): a single-replica StatefulSet whose
            // volumeClaimTemplate provides the durable state dir; the state
            // survives pod reschedule (unlike ephemeral's emptyDir).
            Some(size) => Rendered::StatefulSet(Box::new(StatefulSet {
                metadata: meta,
                spec: Some(StatefulSetSpec {
                    replicas: Some(1),
                    service_name: Some(name.clone()),
                    selector: label_selector(&labels),
                    template,
                    volume_claim_templates: Some(vec![state_pvc_template(size)]),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            None => Rendered::Deployment(Box::new(Deployment {
                metadata: meta,
                spec: Some(DeploymentSpec {
                    replicas: Some(1),
                    selector: label_selector(&labels),
                    template,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        },
    })
}

/// The durable state PVC template for a `local`-store agent (P3-4): named
/// [`STATE_VOLUME`] so the pod's existing state MOUNT binds to it; RWO, the
/// default StorageClass, the requested size.
fn state_pvc_template(size: &str) -> k8s_openapi::api::core::v1::PersistentVolumeClaim {
    use k8s_openapi::api::core::v1::{
        PersistentVolumeClaim, PersistentVolumeClaimSpec, VolumeResourceRequirements,
    };
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(STATE_VOLUME.to_string()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(std::collections::BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(size.to_string()),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The Job spec around a template: no in-cluster retries beyond the policy
/// (`backoffLimit: 0` keeps the run-or-not decision with `podFailurePolicy` +
/// the CR owner), and the exit-code intents compiled per the vendored table.
fn job_spec(template: PodTemplateSpec) -> JobSpec {
    JobSpec {
        backoff_limit: Some(0),
        pod_failure_policy: Some(pod_failure_policy()),
        template,
        ..Default::default()
    }
}

/// Compile the vendored exit-code intent table into a `podFailurePolicy`:
/// `terminal` codes fail the Job outright (a retry never helps), `retriable`
/// and `policy` codes count against `backoffLimit`, and a SIGTERM'd pod that
/// was disrupted (node drain, preemption) is counted rather than failed —
/// exactly the division agentd's own manifests document, derived here from the
/// contract instead of hand-copied.
fn pod_failure_policy() -> PodFailurePolicy {
    let table = Table::vendored();
    let on_codes = |codes: Vec<i32>, action: &str| PodFailurePolicyRule {
        action: action.to_string(),
        on_exit_codes: Some(PodFailurePolicyOnExitCodesRequirement {
            container_name: Some("agent".to_string()),
            operator: "In".to_string(),
            values: codes,
        }),
        on_pod_conditions: None,
    };
    let mut retriable = table.codes_with_intent(Intent::Retriable);
    retriable.extend(table.codes_with_intent(Intent::Policy));
    retriable.sort_unstable();
    PodFailurePolicy {
        rules: vec![
            on_codes(table.codes_with_intent(Intent::Terminal), "FailJob"),
            on_codes(retriable, "Count"),
            PodFailurePolicyRule {
                action: "Count".to_string(),
                on_exit_codes: None,
                on_pod_conditions: Some(vec![PodFailurePolicyOnPodConditionsPattern {
                    status: Some("True".to_string()),
                    type_: "DisruptionTarget".to_string(),
                }]),
            },
        ],
    }
}

/// Render an `AgentFleet`'s worker workload. Claim mode → a Deployment whose
/// replicas KEDA owns (`replicas: None`); shard mode → a StatefulSet with the
/// fixed partition count. Per-member identity is `AGENT_POD_NAME` (the store
/// fence); there is NO agent-side shard flag any more — partition semantics
/// live in the fleet's own workflows/config (ADR-0009, RFC 0034).
pub fn render_fleet(
    fleet: &AgentFleet,
    cfg: &RenderConfig,
    wiring: &PodWiring,
    static_replicas: Option<u32>,
) -> Result<Rendered, RenderError> {
    let name = fleet
        .metadata
        .name
        .clone()
        .ok_or(RenderError::MissingName)?;
    let mut template_spec = fleet.spec.template.clone();
    // Fleet workers are long-lived consumers regardless of what the template
    // says (the v1 coercion, unchanged): the document the controller composed
    // for the fleet already reflects Reactive.
    template_spec.mode = Mode::Reactive;
    let image = resolve_image(&template_spec.image, cfg)?;
    require_stock_unix(template_spec.substrate)?;

    let labels = managed_labels(&name);
    let meta = owned_meta(
        &name,
        fleet.metadata.namespace.clone(),
        &labels,
        owner_ref("AgentFleet", &name, uid_of(&fleet.metadata.uid)),
    );
    let template = pod_template(&name, &image, Mode::Reactive, &labels, wiring);

    // v2 static strategy (RFC 0034 §3.1): a fixed member set — StatefulSet
    // for stable ordinals (the member-overlay key + the store fence), sized
    // by spec.replicas, NO shards annotation (no modulus, no guarded
    // resize: vars overlays are per-ordinal, and scale-down drops the
    // highest ordinals with their overlays).
    if let Some(members) = static_replicas {
        return Ok(Rendered::StatefulSet(Box::new(StatefulSet {
            metadata: meta,
            spec: Some(StatefulSetSpec {
                replicas: Some(members as i32),
                service_name: Some(name.clone()),
                selector: label_selector(&labels),
                template,
                ..Default::default()
            }),
            ..Default::default()
        })));
    }

    Ok(match fleet.spec.scaling.mode {
        ScaleMode::Claim => Rendered::Deployment(Box::new(Deployment {
            metadata: meta,
            spec: Some(DeploymentSpec {
                // KEDA owns replicas: leave unset so SSA never fights it.
                replicas: None,
                selector: label_selector(&labels),
                template,
                ..Default::default()
            }),
            ..Default::default()
        })),
        ScaleMode::Shard => {
            let shards = fleet
                .spec
                .scaling
                .shards
                .ok_or(RenderError::MissingShards)?;
            let mut template = template;
            template
                .metadata
                .get_or_insert_with(Default::default)
                .annotations
                .get_or_insert_with(Default::default)
                .insert(SHARDS_ANNOTATION.to_string(), shards.to_string());
            Rendered::StatefulSet(Box::new(StatefulSet {
                metadata: meta,
                spec: Some(StatefulSetSpec {
                    replicas: Some(shards as i32),
                    service_name: Some(name.clone()),
                    selector: label_selector(&labels),
                    template,
                    ..Default::default()
                }),
                ..Default::default()
            }))
        }
    })
}

/// Label distinguishing a fleet's coordinator pods from its workers.
pub const FLEET_ROLE_LABEL: &str = "agentctl.dev/fleet-role";
/// Label carrying the owning fleet's name on auxiliary workloads.
pub const FLEET_LABEL: &str = "agentctl.dev/fleet";

/// The coordinator Deployment name for a fleet.
pub fn coordinator_name(fleet: &str) -> String {
    format!("{fleet}-coordinator")
}

/// Render a fleet's coordinator ("main agent") Deployment, if declared. Its
/// document (with `a2a.peers` for `distribution: a2a`) is composed by the
/// controller; the queue-distribution work-fabric coordinates ride
/// `wiring.extra_env`.
pub fn render_coordinator(
    fleet: &AgentFleet,
    cfg: &RenderConfig,
    wiring: &PodWiring,
) -> Result<Option<Rendered>, RenderError> {
    let Some(coord) = &fleet.spec.coordinator else {
        return Ok(None);
    };
    let fleet_name = fleet
        .metadata
        .name
        .clone()
        .ok_or(RenderError::MissingName)?;
    let name = coordinator_name(&fleet_name);
    let mut spec = coord.template.clone();
    spec.mode = Mode::Reactive; // long-lived front door, like v1
    let image = resolve_image(&spec.image, cfg)?;
    require_stock_unix(spec.substrate)?;

    let mut labels = managed_labels(&name);
    labels.insert(FLEET_ROLE_LABEL.to_string(), "coordinator".to_string());
    labels.insert(FLEET_LABEL.to_string(), fleet_name.clone());
    let meta = owned_meta(
        &name,
        fleet.metadata.namespace.clone(),
        &labels,
        owner_ref("AgentFleet", &fleet_name, uid_of(&fleet.metadata.uid)),
    );
    let template = pod_template(&name, &image, Mode::Reactive, &labels, wiring);
    let replicas = coord.replicas.unwrap_or(1).max(1) as i32;

    Ok(Some(Rendered::Deployment(Box::new(Deployment {
        metadata: meta,
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            selector: label_selector(&labels),
            template,
            ..Default::default()
        }),
        ..Default::default()
    }))))
}

/// The A2A worker-pool peer endpoint a coordinator's document declares for
/// `distribution: a2a` (RFC 0022's fleet front door via the gateway).
pub fn fleet_peer_endpoint(cfg: &RenderConfig, ns: &str, fleet: &str) -> String {
    // The WORKERS tier (P6-3): the coordinator's downstream dial must skip
    // the fleet front door (which would route it back onto itself).
    format!(
        "{}/fleets/{}/{}/workers",
        cfg.gateway_url.trim_end_matches('/'),
        ns,
        fleet
    )
}

// ---------------------------------------------------------------------------
// KEDA ScaledObject (claim-mode fleets) — unchanged mechanics from v1
// ---------------------------------------------------------------------------

/// Default scaler gRPC address for the KEDA external trigger.
pub const DEFAULT_SCALER_ADDRESS: &str = "agentctl-scaler.agentctl-system:9100";
/// Default in-cluster coordination-server base URL (claim backlog source).
pub const DEFAULT_COORDINATION_URL: &str = "http://agentctl-coordination.agentctl-system/";
/// Fallback per-replica backlog target.
const DEFAULT_SCALE_THRESHOLD: &str = "5";

/// Render the KEDA `ScaledObject` autoscaling a claim-mode fleet, or `None`
/// for shard mode. Untyped body (no hard KEDA CRD dependency).
pub fn render_scaled_object(
    fleet: &AgentFleet,
    scaler_address: &str,
    coordination_url: &str,
) -> Option<serde_json::Value> {
    if fleet.spec.scaling.mode != ScaleMode::Claim {
        return None;
    }
    let name = fleet.metadata.name.clone()?;
    let scaling = &fleet.spec.scaling;
    let min = scaling.min_replicas.unwrap_or(0);
    // The contract-neutral signal token (P6-5 scaler v2): `backlog` (the
    // coordination work fabric — the default) or `inbox_pending` (the sum of
    // agent_inbox_pending over the fleet's own member pods, per the metrics
    // registry's scaler_guidance.primary).
    let metric = scaling
        .target
        .as_ref()
        .map(|t| t.metric.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "backlog".to_string());
    let threshold = scaling
        .target
        .as_ref()
        .map(|t| t.value.clone())
        .unwrap_or_else(|| DEFAULT_SCALE_THRESHOLD.to_string());
    let coordination_url = fleet
        .spec
        .work
        .as_ref()
        .and_then(|w| w.source.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| coordination_url.to_string());

    let mut spec = serde_json::json!({
        "scaleTargetRef": { "name": name },
        "minReplicaCount": min,
        "triggers": [{
            "type": "external",
            "metadata": {
                "scalerAddress": scaler_address,
                "coordinationUrl": coordination_url,
                "metric": metric,
                // inbox_pending source: the scaler scrapes the fleet's own
                // member pods (label-selected) on the probes port.
                "namespace": fleet.metadata.namespace.clone().unwrap_or_default(),
                "selector": format!("agentctl.dev/agent={name}"),
                "threshold": threshold,
                "activationThreshold": "1",
            }
        }]
    });
    if let Some(max) = scaling.max_replicas {
        spec["maxReplicaCount"] = serde_json::json!(max);
    }

    let mut metadata = serde_json::json!({
        "name": name,
        "labels": managed_labels(&name),
        "ownerReferences": [{
            "apiVersion": API_VERSION,
            "kind": "AgentFleet",
            "name": name,
            "uid": uid_of(&fleet.metadata.uid),
            "controller": true,
            "blockOwnerDeletion": true,
        }],
    });
    if let Some(ns) = &fleet.metadata.namespace {
        metadata["namespace"] = serde_json::json!(ns);
    }

    Some(serde_json::json!({
        "apiVersion": "keda.sh/v1alpha1",
        "kind": "ScaledObject",
        "metadata": metadata,
        "spec": spec,
    }))
}

// ---------------------------------------------------------------------------
// The pod shell
// ---------------------------------------------------------------------------

fn require_stock_unix(substrate: Option<Substrate>) -> Result<(), RenderError> {
    match substrate.unwrap_or(Substrate::StockUnix) {
        Substrate::StockUnix => Ok(()),
        other => Err(RenderError::UnsupportedSubstrate(other)),
    }
}

fn managed_labels(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "agentctl".to_string(),
        ),
        ("app.kubernetes.io/name".to_string(), "agent".to_string()),
        ("agentctl.dev/agent".to_string(), name.to_string()),
    ])
}

fn label_selector(labels: &BTreeMap<String, String>) -> LabelSelector {
    LabelSelector {
        match_labels: Some(labels.clone()),
        ..Default::default()
    }
}

/// The label-selector STRING matching a fleet's pods (scale subresource).
pub fn fleet_selector_string(name: &str) -> String {
    selector_string(&managed_labels(name))
}

fn selector_string(labels: &BTreeMap<String, String>) -> String {
    labels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn owned_meta(
    name: &str,
    namespace: Option<String>,
    labels: &BTreeMap<String, String>,
    owner: OwnerReference,
) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace,
        labels: Some(labels.clone()),
        owner_references: Some(vec![owner]),
        ..Default::default()
    }
}

fn uid_of(uid: &Option<String>) -> String {
    uid.clone().unwrap_or_default()
}

fn owner_ref(kind: &str, name: &str, uid: String) -> OwnerReference {
    OwnerReference {
        api_version: API_VERSION.to_string(),
        kind: kind.to_string(),
        name: name.to_string(),
        uid,
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Trailing-dot FQDN normalization — lives in `agent-config` now (the compose
/// path applies it); re-exported for the AAuth admin client's own dials.
pub(crate) use agent_config::absolutize_endpoint;

/// Kubernetes downward-API identity env (the ACC env convention):
/// `AGENT_POD_NAME` is load-bearing — it becomes `agent.name`, the durable
/// store's per-replica identity fence.
fn downward_env() -> Vec<EnvVar> {
    let field = |name: &str, path: &str| EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                field_path: path.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    vec![
        field("AGENT_POD_NAME", "metadata.name"),
        field("AGENT_POD_UID", "metadata.uid"),
        field("AGENT_POD_NAMESPACE", "metadata.namespace"),
        field("AGENT_NODE_NAME", "spec.nodeName"),
    ]
}

fn secret_env(name: &str, secret: &str, key: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: secret.to_string(),
                key: key.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn container_security_context() -> SecurityContext {
    SecurityContext {
        run_as_non_root: Some(true),
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn pod_security_context() -> PodSecurityContext {
    PodSecurityContext {
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn http_probe(path: &str, period: i32, failures: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.to_string()),
            port: IntOrString::Int(METRICS_PORT),
            ..Default::default()
        }),
        period_seconds: Some(period),
        failure_threshold: Some(failures),
        ..Default::default()
    }
}

/// The one pod template every shape shares. The container runs
/// `agentd -c <config file>`; everything else is mounts/env the document's
/// references resolve against.
fn pod_template(
    workload: &str,
    image: &str,
    mode: Mode,
    labels: &BTreeMap<String, String>,
    wiring: &PodWiring,
) -> PodTemplateSpec {
    let daemon = agent_config::is_daemon(mode);

    let mut volumes = vec![
        Volume {
            name: CONFIG_VOLUME.to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: config_configmap_name(workload),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: TLS_VOLUME.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(serving_secret_name(workload)),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: CA_VOLUME.to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: CA_CONFIGMAP.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: TMP_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
    ];
    let mut mounts = vec![
        mount(CONFIG_VOLUME, paths::CONFIG_DIR, true),
        mount(TLS_VOLUME, TLS_MOUNT, true),
        mount(CA_VOLUME, CA_MOUNT, true),
        mount(TMP_VOLUME, TMP_MOUNT, false),
    ];

    // Writable state for EVERY shape: the document always declares the file
    // store (agentd initializes the state dir even on one-shots, and the
    // XDG-defaulted path is the read-only rootfs). `local` (P3-4) provides
    // the SAME mount from a StatefulSet volumeClaimTemplate instead of an
    // emptyDir — so omit the emptyDir volume there (the mount stays).
    if wiring.local_store_size.is_none() {
        volumes.push(Volume {
            name: STATE_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        });
    }
    mounts.push(mount(STATE_VOLUME, STATE_MOUNT, false));
    if let Some(wf) = &wiring.workflow {
        volumes.push(Volume {
            name: "agentctl-workflow".to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: wf.config_map.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount("agentctl-workflow", paths::WORKFLOW_DIR, true));
    }
    if wiring.aauth_key {
        volumes.push(Volume {
            name: AAUTH_VOLUME.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(format!("{workload}-aauth-key")),
                default_mode: Some(0o444),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount(AAUTH_VOLUME, AAUTH_MOUNT, true));
    }
    if wiring.principals {
        volumes.push(Volume {
            name: PRINCIPALS_VOLUME.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(principals_secret_name(workload)),
                // 0444 like the AAuth key mount: the container runs nonroot
                // (65532) with no fsGroup chown of these files, so group-only
                // modes are unreadable (observed: startup exit 2 EACCES).
                default_mode: Some(0o444),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount(
            PRINCIPALS_VOLUME,
            agent_config::paths::PRINCIPALS_DIR,
            true,
        ));
    }
    if wiring.peer_bearers {
        volumes.push(Volume {
            name: PEER_BEARERS_VOLUME.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(peer_bearers_secret_name(workload)),
                // 0444 for the same nonroot-read reason as the principals mount.
                default_mode: Some(0o444),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount(
            PEER_BEARERS_VOLUME,
            agent_config::paths::PEER_BEARERS_DIR,
            true,
        ));
    }
    if wiring.hooks_secrets {
        volumes.push(Volume {
            name: HOOKS_VOLUME.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(hooks_secret_name(workload)),
                default_mode: Some(0o444),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount(
            HOOKS_VOLUME,
            agent_config::paths::HOOKS_SECRETS_DIR,
            true,
        ));
    }

    let mut env = downward_env();
    if wiring.member_overlays {
        // The StatefulSet pod-index label (stable per member) — the ordinal
        // the member-overlay argv layer keys off.
        env.push(EnvVar {
            name: "AGENT_POD_INDEX".to_string(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    field_path: "metadata.labels['apps.kubernetes.io/pod-index']".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(r) = &wiring.intelligence_token {
        env.push(secret_env(
            agent_config::INTELLIGENCE_TOKEN_ENV,
            &r.name,
            &r.key,
        ));
    }
    for t in &wiring.mcp_tokens {
        env.push(secret_env(&t.env, &t.secret, &t.key));
    }
    if wiring.api_token {
        env.push(secret_env(API_TOKEN_ENV, API_TOKEN_SECRET, API_TOKEN_ENV));
    }
    for (k, v) in &wiring.extra_env {
        env.push(EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            ..Default::default()
        });
    }

    let mut args = vec![
        "-c".to_string(),
        paths::services_file(),
        "-c".to_string(),
        paths::config_file(),
    ];
    if wiring.member_overlays {
        // Member overlay LAST (RFC 7396: later layers win key-by-key). The
        // kubelet expands $(AGENT_POD_INDEX) from the env below.
        args.push("-c".to_string());
        args.push(format!(
            "{}/member-$(AGENT_POD_INDEX).json",
            paths::CONFIG_DIR
        ));
    }
    let container = Container {
        name: "agent".to_string(),
        image: Some(image.to_string()),
        // Catalog layer first, instance last (RFC 7396 layering; folders —
        // when the projection emits them — adopt beside the LAST file).
        args: Some(args),
        env: Some(env),
        ports: Some({
            let mut ports = vec![
                ContainerPort {
                    name: Some("a2a".to_string()),
                    container_port: SERVE_PORT,
                    ..Default::default()
                },
                ContainerPort {
                    name: Some("probes".to_string()),
                    container_port: METRICS_PORT,
                    ..Default::default()
                },
            ];
            if wiring.hooks_port {
                // agentd's webhook listener (pod-network bind; the gateway
                // hooks proxy is the only sanctioned external path).
                ports.push(ContainerPort {
                    name: Some("hooks".to_string()),
                    container_port: 9494,
                    ..Default::default()
                });
            }
            ports
        }),
        volume_mounts: Some(mounts),
        // Liveness = the supervisor reactor heartbeat (an idle reactive agent
        // is healthy; a stuck subagent must NOT fail pod liveness); readiness
        // flips on drain/lame-duck/intel-down — both per the agent's own
        // documented probe semantics.
        liveness_probe: Some(http_probe("/healthz", 10, 3)),
        readiness_probe: Some(http_probe("/readyz", 5, 2)),
        security_context: Some(container_security_context()),
        ..Default::default()
    };

    let mut annotations = BTreeMap::new();
    if !wiring.config_hash.is_empty() {
        annotations.insert(
            CONFIG_HASH_ANNOTATION.to_string(),
            wiring.config_hash.clone(),
        );
    }

    PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            annotations: (!annotations.is_empty()).then_some(annotations),
            ..Default::default()
        }),
        spec: Some(PodSpec {
            containers: vec![container],
            volumes: Some(volumes),
            restart_policy: (!daemon).then(|| "Never".to_string()),
            automount_service_account_token: Some(false),
            // agentd is its own pid-1 with a subreaper; sharing the pid
            // namespace keeps the orphan story intact under sidecars.
            share_process_namespace: Some(true),
            security_context: Some(pod_security_context()),
            termination_grace_period_seconds: Some(TERMINATION_GRACE_SECONDS),
            ..Default::default()
        }),
    }
}

fn mount(name: &str, path: &str, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        read_only: Some(read_only),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_api::{
        AgentFleetSpec, AgentSpec, Coordinator, LoopParams, ModelBinding, Scaling, Schedule,
        SecretKeyRef,
    };

    fn agent(mode: Mode) -> Agent {
        let mut a = Agent::new(
            "demo",
            AgentSpec {
                mode,
                image: Some("agentd:test".to_string()),
                instruction: Some("Do the thing.".to_string()),
                schedule: (mode == Mode::Schedule).then(|| Schedule {
                    cron: "0 * * * *".to_string(),
                    timezone: None,
                }),
                loop_: (mode == Mode::Loop).then(|| LoopParams {
                    interval: "5m".to_string(),
                    deadline: None,
                }),
                model: Some(ModelBinding {
                    pool: Some("pool".to_string()),
                    id: Some("m1".to_string()),
                }),
                ..Default::default()
            },
        );
        a.metadata.namespace = Some("tenant".to_string());
        a.metadata.uid = Some("uid-1".to_string());
        a
    }

    fn fleet(mode: ScaleMode, shards: Option<u32>) -> AgentFleet {
        let mut f = AgentFleet::new(
            "pool",
            AgentFleetSpec {
                template: agent(Mode::Reactive).spec,
                scaling: Scaling {
                    mode,
                    shards,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        f.metadata.namespace = Some("tenant".to_string());
        f.metadata.uid = Some("uid-2".to_string());
        f
    }

    fn cfg() -> RenderConfig {
        RenderConfig::default()
    }

    fn wiring() -> PodWiring {
        PodWiring {
            config_hash: "abc123".to_string(),
            ..Default::default()
        }
    }

    fn pod_of(r: &Rendered) -> &PodTemplateSpec {
        match r {
            Rendered::Job(j) => &j.spec.as_ref().unwrap().template,
            Rendered::CronJob(c) => &c.spec.job_template.spec.as_ref().unwrap().template,
            Rendered::Deployment(d) => &d.spec.as_ref().unwrap().template,
            Rendered::StatefulSet(s) => &s.spec.as_ref().unwrap().template,
        }
    }

    fn container_of(pod: &PodTemplateSpec) -> &Container {
        &pod.spec.as_ref().unwrap().containers[0]
    }

    #[test]
    fn the_only_argv_is_the_config_file() {
        for mode in [Mode::Once, Mode::Loop, Mode::Reactive, Mode::Schedule] {
            let r = render_agent(&agent(mode), &cfg(), &wiring()).unwrap();
            let c = container_of(pod_of(&r));
            assert_eq!(
                c.args.as_deref(),
                Some(
                    &[
                        "-c".to_string(),
                        "/etc/agentctl/config/services.json".to_string(),
                        "-c".to_string(),
                        "/etc/agentctl/config/agentd.json".to_string()
                    ][..]
                ),
                "mode {mode:?}: only the two config layers, no flags"
            );
        }
    }

    #[test]
    fn mode_to_workload_mapping_is_stable() {
        assert!(matches!(
            render_agent(&agent(Mode::Once), &cfg(), &wiring()).unwrap(),
            Rendered::Job(_)
        ));
        assert!(matches!(
            render_agent(&agent(Mode::Workflow), &cfg(), &wiring()).unwrap(),
            Rendered::Job(_)
        ));
        assert!(matches!(
            render_agent(&agent(Mode::Schedule), &cfg(), &wiring()).unwrap(),
            Rendered::CronJob(_)
        ));
        for m in [Mode::Loop, Mode::Reactive] {
            assert!(matches!(
                render_agent(&agent(m), &cfg(), &wiring()).unwrap(),
                Rendered::Deployment(_)
            ));
        }
    }

    #[test]
    fn jobs_compile_the_exit_code_intents_into_pod_failure_policy() {
        let r = render_agent(&agent(Mode::Once), &cfg(), &wiring()).unwrap();
        let Rendered::Job(job) = r else { panic!("job") };
        let policy = job
            .spec
            .as_ref()
            .unwrap()
            .pod_failure_policy
            .as_ref()
            .unwrap();
        // terminal ⇒ FailJob on exactly the contract's terminal codes (2, 5).
        assert_eq!(policy.rules[0].action, "FailJob");
        assert_eq!(
            policy.rules[0].on_exit_codes.as_ref().unwrap().values,
            vec![2, 5]
        );
        // retriable + policy ⇒ Count.
        assert_eq!(policy.rules[1].action, "Count");
        assert_eq!(
            policy.rules[1].on_exit_codes.as_ref().unwrap().values,
            vec![1, 3, 4, 6, 7, 124]
        );
        // disruption ⇒ Count (never FailJob a node drain).
        assert_eq!(
            policy.rules[2].on_pod_conditions.as_ref().unwrap()[0].type_,
            "DisruptionTarget"
        );
        assert_eq!(job.spec.as_ref().unwrap().backoff_limit, Some(0));
    }

    #[test]
    fn config_volume_and_hash_annotation_are_wired() {
        let r = render_agent(&agent(Mode::Reactive), &cfg(), &wiring()).unwrap();
        let pod = pod_of(&r);
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        assert!(vols.iter().any(
            |v| v.name == CONFIG_VOLUME && v.config_map.as_ref().unwrap().name == "demo-config"
        ));
        assert_eq!(
            pod.metadata
                .as_ref()
                .unwrap()
                .annotations
                .as_ref()
                .unwrap()
                .get(CONFIG_HASH_ANNOTATION)
                .map(String::as_str),
            Some("abc123")
        );
    }

    #[test]
    fn every_shape_gets_a_writable_state_volume() {
        for mode in [Mode::Reactive, Mode::Once, Mode::Schedule] {
            let r = render_agent(&agent(mode), &cfg(), &wiring()).unwrap();
            let vols: Vec<String> = pod_of(&r)
                .spec
                .as_ref()
                .unwrap()
                .volumes
                .as_ref()
                .unwrap()
                .iter()
                .map(|v| v.name.clone())
                .collect();
            assert!(
                vols.contains(&STATE_VOLUME.to_string()),
                "{mode:?}: the explicit file store needs its emptyDir"
            );
        }
    }

    #[test]
    fn drain_aware_grace_and_both_probes() {
        let r = render_agent(&agent(Mode::Reactive), &cfg(), &wiring()).unwrap();
        let pod = pod_of(&r);
        assert_eq!(
            pod.spec.as_ref().unwrap().termination_grace_period_seconds,
            Some(TERMINATION_GRACE_SECONDS)
        );
        let c = container_of(pod);
        let live = c.liveness_probe.as_ref().unwrap();
        assert_eq!(
            live.http_get.as_ref().unwrap().path.as_deref(),
            Some("/healthz")
        );
        let ready = c.readiness_probe.as_ref().unwrap();
        assert_eq!(
            ready.http_get.as_ref().unwrap().path.as_deref(),
            Some("/readyz")
        );
    }

    #[test]
    fn secret_envs_resolve_the_documents_references() {
        let mut w = wiring();
        w.intelligence_token = Some(SecretKeyRef {
            name: "pool-cred".to_string(),
            key: "token".to_string(),
        });
        w.mcp_tokens = vec![SecretEnv {
            env: "AGENT_MCP_BILLING_TOKEN".to_string(),
            secret: "billing".to_string(),
            key: "token".to_string(),
        }];
        w.api_token = true;
        let r = render_agent(&agent(Mode::Once), &cfg(), &w).unwrap();
        let env = container_of(pod_of(&r)).env.as_ref().unwrap();
        let get = |n: &str| env.iter().find(|e| e.name == n).unwrap();
        assert_eq!(
            get("INTELLIGENCE_TOKEN")
                .value_from
                .as_ref()
                .unwrap()
                .secret_key_ref
                .as_ref()
                .unwrap()
                .name,
            "pool-cred"
        );
        assert!(env.iter().any(|e| e.name == "AGENT_MCP_BILLING_TOKEN"));
        assert!(env.iter().any(|e| e.name == API_TOKEN_ENV));
        // Downward identity is always present — the store fence.
        assert!(env.iter().any(|e| e.name == "AGENT_POD_NAME"));
    }

    #[test]
    fn workflow_and_aauth_mounts_follow_the_wiring() {
        let mut w = wiring();
        w.workflow = Some(WorkflowMount {
            config_map: "demo-workflow".to_string(),
            key: "workflow.json".to_string(),
        });
        w.aauth_key = true;
        assert_eq!(
            w.workflow.as_ref().unwrap().file_path(),
            "/etc/agentctl/workflow/workflow.json"
        );
        let r = render_agent(&agent(Mode::Workflow), &cfg(), &w).unwrap();
        let vols = pod_of(&r).spec.as_ref().unwrap().volumes.as_ref().unwrap();
        assert!(vols.iter().any(|v| v.name == "agentctl-workflow"));
        assert!(vols.iter().any(|v| v.name == AAUTH_VOLUME));
    }

    #[test]
    fn schedule_renders_cron_with_forbid() {
        let r = render_agent(&agent(Mode::Schedule), &cfg(), &wiring()).unwrap();
        let Rendered::CronJob(cj) = r else {
            panic!("cronjob")
        };
        let spec = &cj.spec;
        assert_eq!(spec.schedule, "0 * * * *");
        assert_eq!(spec.concurrency_policy.as_deref(), Some("Forbid"));
        assert!(spec
            .job_template
            .spec
            .as_ref()
            .unwrap()
            .pod_failure_policy
            .is_some());
    }

    #[test]
    fn static_fleet_renders_member_addressed_statefulset() {
        let mut w = wiring();
        w.member_overlays = true;
        let r = render_fleet(&fleet(ScaleMode::Shard, Some(3)), &cfg(), &w, Some(3)).unwrap();
        let Rendered::StatefulSet(sts) = r else {
            panic!("static strategy must render a StatefulSet");
        };
        let spec = sts.spec.as_ref().unwrap();
        assert_eq!(spec.replicas, Some(3));
        let tmpl = &spec.template;
        // NO shards annotation: static members have no modulus to guard.
        assert!(tmpl
            .metadata
            .as_ref()
            .and_then(|m| m.annotations.as_ref())
            .map(|a| !a.contains_key(SHARDS_ANNOTATION))
            .unwrap_or(true));
        let c = &tmpl.spec.as_ref().unwrap().containers[0];
        let args = c.args.as_ref().unwrap();
        // The member overlay is the THIRD -c, keyed by the pod index env.
        assert_eq!(args.len(), 6);
        assert!(args[5].contains("member-$(AGENT_POD_INDEX).json"));
        assert!(c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| e.name == "AGENT_POD_INDEX"
                && e.value_from
                    .as_ref()
                    .and_then(|v| v.field_ref.as_ref())
                    .map(|f| f.field_path.contains("pod-index"))
                    .unwrap_or(false)));
    }

    #[test]
    fn claim_fleet_leaves_replicas_to_keda_and_carries_no_shard_flag() {
        let r = render_fleet(&fleet(ScaleMode::Claim, None), &cfg(), &wiring(), None).unwrap();
        let Rendered::Deployment(d) = r else {
            panic!("deployment")
        };
        assert_eq!(d.spec.as_ref().unwrap().replicas, None);
        let args = d
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0]
            .args
            .as_ref()
            .unwrap()
            .join(" ");
        assert!(
            !args.contains("--shard"),
            "agent-side sharding is gone upstream"
        );
    }

    #[test]
    fn shard_fleet_renders_statefulset_with_n_replicas_no_agent_side_identity() {
        let r = render_fleet(&fleet(ScaleMode::Shard, Some(3)), &cfg(), &wiring(), None).unwrap();
        let Rendered::StatefulSet(s) = r else {
            panic!("statefulset")
        };
        assert_eq!(s.spec.as_ref().unwrap().replicas, Some(3));
        let args = s
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0]
            .args
            .as_ref()
            .unwrap();
        assert_eq!(
            args,
            &[
                "-c",
                "/etc/agentctl/config/services.json",
                "-c",
                "/etc/agentctl/config/agentd.json"
            ]
        );
    }

    #[test]
    fn shard_fleet_without_shards_is_refused() {
        assert_eq!(
            render_fleet(&fleet(ScaleMode::Shard, None), &cfg(), &wiring(), None).unwrap_err(),
            RenderError::MissingShards
        );
    }

    #[test]
    fn coordinator_renders_with_role_labels_and_fleet_env() {
        let mut f = fleet(ScaleMode::Claim, None);
        f.spec.coordinator = Some(Coordinator {
            template: agent(Mode::Reactive).spec,
            replicas: None,
            distribution: None,
        });
        let mut w = wiring();
        w.extra_env = vec![(
            "AGENT_FLEET_WORKSOURCE".to_string(),
            "queue://jobs".to_string(),
        )];
        let r = render_coordinator(&f, &cfg(), &w).unwrap().unwrap();
        let Rendered::Deployment(d) = r else {
            panic!("deployment")
        };
        let labels = d.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels.get(FLEET_ROLE_LABEL).map(String::as_str),
            Some("coordinator")
        );
        let env = d
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0]
            .env
            .as_ref()
            .unwrap();
        assert!(env
            .iter()
            .any(|e| e.name == "AGENT_FLEET_WORKSOURCE"
                && e.value.as_deref() == Some("queue://jobs")));
    }

    #[test]
    fn fleet_peer_endpoint_shape() {
        assert_eq!(
            fleet_peer_endpoint(&cfg(), "tenant", "pool"),
            format!("{DEFAULT_GATEWAY_URL}/fleets/tenant/pool/workers")
        );
    }

    #[test]
    fn scaled_object_only_for_claim_mode() {
        assert!(render_scaled_object(
            &fleet(ScaleMode::Shard, Some(3)),
            DEFAULT_SCALER_ADDRESS,
            DEFAULT_COORDINATION_URL
        )
        .is_none());
        let so = render_scaled_object(
            &fleet(ScaleMode::Claim, None),
            DEFAULT_SCALER_ADDRESS,
            DEFAULT_COORDINATION_URL,
        )
        .unwrap();
        assert_eq!(so["spec"]["scaleTargetRef"]["name"], "pool");
    }

    #[test]
    fn hardened_pod_shell_survives() {
        let r = render_agent(&agent(Mode::Once), &cfg(), &wiring()).unwrap();
        let pod = pod_of(&r).spec.as_ref().unwrap();
        assert_eq!(pod.automount_service_account_token, Some(false));
        assert_eq!(pod.share_process_namespace, Some(true));
        let sc = container_of(pod_of(&r)).security_context.as_ref().unwrap();
        assert_eq!(sc.read_only_root_filesystem, Some(true));
        assert_eq!(sc.run_as_non_root, Some(true));
    }

    #[test]
    fn absolutize_appends_trailing_dot_to_cluster_fqdns() {
        assert_eq!(
            absolutize_endpoint("https://svc.ns.svc.cluster.local:8443/mcp"),
            "https://svc.ns.svc.cluster.local.:8443/mcp"
        );
        assert_eq!(
            absolutize_endpoint("https://api.example.com/v1"),
            "https://api.example.com/v1"
        );
    }
}
