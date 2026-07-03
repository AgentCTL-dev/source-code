// SPDX-License-Identifier: BUSL-1.1
//! Pure workload rendering: an [`Agent`]/[`AgentFleet`] → the Kubernetes
//! workload that runs it.
//!
//! This is the deterministic, side-effect-free core the reconcile loop (RFC
//! 0006) calls. Keeping it pure makes the mode→workload mapping (RFC 0003 §5),
//! the scaling regime (RFC 0011), and the serve wiring all unit-testable
//! without a cluster.
//!
//! **contract_version 2.0 (agentd v2 HTTPS-everywhere pivot): the network is
//! the substrate; identity is the boundary.** Every rendered pod SERVES its
//! management/A2A surface over mTLS-gated HTTPS (`--serve-mcp
//! https://0.0.0.0:8443`) with a cert-manager-issued serving identity, trusts
//! the cluster CA for callers (`--serve-client-ca`) and for its own keyless
//! outbound dials (`--tls-ca`, `AGENT_INTELLIGENCE=https://<modelgateway>`),
//! and exposes `/readyz` on a separate metrics listener. No hostPath, no
//! unix sockets, no pod-held credential: the ONLY key material in the pod is
//! its OWN serving identity (cert-manager Secret, rotated live by the agent).

use std::collections::BTreeMap;

use agent_api::{Agent, AgentFleet, AgentSpec, Distribution, Mode, ScaleMode, Substrate};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, StatefulSet, StatefulSetSpec};
use k8s_openapi::api::batch::v1::{CronJob, CronJobSpec, Job, JobSpec, JobTemplateSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HTTPGetAction, ObjectFieldSelector, PodSecurityContext, PodSpec, PodTemplateSpec,
    Probe, SeccompProfile, SecretKeySelector, SecretVolumeSource, SecurityContext, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

/// API group/version these resources are owned by (agent-api `GROUP`).
const API_VERSION: &str = "agents.x-k8s.io/v1alpha1";

/// In-pod mount of the workload's own serving identity — the cert-manager
/// `Certificate` Secret ([`serving_secret_name`], keys `tls.crt`/`tls.key`).
/// The agent re-reads these paths in place on rotation (agentd ≥2.1 live
/// acceptor), so a cert-manager renewal never restarts the pod.
const TLS_MOUNT: &str = "/etc/agentctl/tls";
const TLS_VOLUME: &str = "agentctl-serving-tls";

/// In-pod mount of the cluster CA **public certificate** (ConfigMap
/// [`CA_CONFIGMAP`], key `ca.crt`, ensured per agent namespace by the
/// operator). Doubles as the agent's client-CA (who may call me = holders of
/// agentctl-CA client certs → `Management`) and its outbound trust anchor
/// (`--tls-ca` — the gateways' serving certs chain to the same CA).
const CA_MOUNT: &str = "/etc/agentctl/ca";
const CA_VOLUME: &str = "agentctl-ca";
/// The per-namespace ConfigMap carrying the cluster CA cert (public material).
pub const CA_CONFIGMAP: &str = "agentctl-ca";
/// Key within [`CA_CONFIGMAP`] (and the mounted filename) holding the CA PEM.
pub const CA_KEY: &str = "ca.crt";

/// The HTTPS port every rendered agent serves its self-MCP/A2A surface on.
pub const SERVE_PORT: i32 = 8443;
/// The metrics/readiness listener port (`AGENT_METRICS_ADDR`, `/readyz`).
pub const METRICS_PORT: i32 = 9090;

/// The serving-identity Secret name for a workload (cert-manager
/// `Certificate.spec.secretName`; created by the operator, mounted at
/// [`TLS_MOUNT`]).
pub fn serving_secret_name(workload: &str) -> String {
    format!("{workload}-serving-tls")
}

/// Operator-scoped render inputs that do not live on the CR: where the model +
/// MCP gateways are. Built once by the controller from its environment; a test
/// passes a literal.
#[derive(Debug, Clone)]
pub struct RenderConfig {
    /// The ModelGateway base URL rendered into `AGENT_INTELLIGENCE` (keyless
    /// dial; identity = source-IP attestation at the gateway). MUST be an
    /// `https://` URL whose cert chains to the cluster CA, and SHOULD be an
    /// absolute (trailing-dot) FQDN so no DNS search list can capture it.
    pub modelgateway_url: String,
    /// The MCPGateway base URL each bound MCP server is rendered against
    /// (`--mcp <name>=<url>/s/<name>`, RFC 0019). Same constraints as the
    /// ModelGateway URL; the agent dials it keyless (identity = source IP).
    pub mcpgateway_url: String,
    /// The A2A gateway base URL a coordinator's `--a2a-peer worker=…/fleets/<ns>/<name>`
    /// is rendered against for `distribution: a2a` (RFC 0022 §5/§6). Unused for the
    /// default `queue` distribution.
    pub gateway_url: String,
}

/// Default in-cluster ModelGateway URL (chart Service, control-plane
/// namespace; absolute FQDN — trailing dot — so ndots search never rewrites it).
pub const DEFAULT_MODELGATEWAY_URL: &str =
    "https://agentctl-modelgateway.agentctl-system.svc.cluster.local.";
/// Default in-cluster MCPGateway URL (chart Service, control-plane namespace).
pub const DEFAULT_MCPGATEWAY_URL: &str =
    "https://agentctl-mcpgateway.agentctl-system.svc.cluster.local.";
/// Default in-cluster A2A gateway URL (chart Service, control-plane namespace).
pub const DEFAULT_GATEWAY_URL: &str =
    "http://agentctl-gateway.agentctl-system.svc.cluster.local.:8080";

impl Default for RenderConfig {
    fn default() -> Self {
        RenderConfig {
            modelgateway_url: DEFAULT_MODELGATEWAY_URL.to_string(),
            mcpgateway_url: DEFAULT_MCPGATEWAY_URL.to_string(),
            gateway_url: DEFAULT_GATEWAY_URL.to_string(),
        }
    }
}

impl RenderConfig {
    /// Build from the operator environment (`AGENTCTL_MODELGATEWAY_URL`,
    /// `AGENTCTL_MCPGATEWAY_URL`, `AGENTCTL_GATEWAY_URL`), falling back to the
    /// in-cluster defaults.
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
            modelgateway_url: env("AGENTCTL_MODELGATEWAY_URL", d.modelgateway_url),
            mcpgateway_url: env("AGENTCTL_MCPGATEWAY_URL", d.mcpgateway_url),
            gateway_url: env("AGENTCTL_GATEWAY_URL", d.gateway_url),
        }
    }
}

/// A resolved MCP server binding for an agent — the server name (which is both
/// the gateway facade path segment and the agent's `--mcp` server name) and its
/// operator-declared trifecta tags. Produced by the controller (which reads the
/// `MCPServerSet`s the agent binds) and rendered by [`inject_mcp_servers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpBinding {
    pub name: String,
    pub tags: Vec<String>,
}

/// In-pod mount of the workflow graph (agentd v2 `--mode workflow`). The
/// operator mounts a ConfigMap holding the graph JSON and passes the file path
/// as `--workflow`.
const WORKFLOW_MOUNT: &str = "/etc/agentctl/workflow";
const WORKFLOW_VOLUME: &str = "agentctl-workflow";
/// The generated-ConfigMap name for an inline workflow on a workload.
pub fn workflow_configmap_name(workload: &str) -> String {
    format!("{workload}-workflow")
}

/// Mount the workflow ConfigMap (key `key` at [`WORKFLOW_MOUNT`]) and pass
/// `--workflow <mount>/<key>` to the agent (agentd v2). Idempotent. `configmap`
/// is either the operator-generated `<workload>-workflow` (inline source) or the
/// user's `configMapKeyRef.name`.
pub fn inject_workflow(rendered: &mut Rendered, configmap: &str, key: &str) {
    let pod = match rendered {
        Rendered::Job(job) => job.spec.as_mut().map(|s| &mut s.template),
        Rendered::CronJob(cj) => cj
            .spec
            .job_template
            .spec
            .as_mut()
            .map(|js| &mut js.template),
        Rendered::Deployment(dep) => dep.spec.as_mut().map(|s| &mut s.template),
        Rendered::StatefulSet(sts) => sts.spec.as_mut().map(|s| &mut s.template),
    };
    let Some(pod) = pod else { return };
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    // Volume (idempotent).
    let volumes = spec.volumes.get_or_insert_with(Vec::new);
    if !volumes.iter().any(|v| v.name == WORKFLOW_VOLUME) {
        volumes.push(Volume {
            name: WORKFLOW_VOLUME.to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: configmap.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    let Some(container) = spec.containers.first_mut() else {
        return;
    };
    let mounts = container.volume_mounts.get_or_insert_with(Vec::new);
    if !mounts.iter().any(|m| m.name == WORKFLOW_VOLUME) {
        mounts.push(VolumeMount {
            name: WORKFLOW_VOLUME.to_string(),
            mount_path: WORKFLOW_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
    }
    let args = container.args.get_or_insert_with(Vec::new);
    if !args.iter().any(|a| a == "--workflow") {
        args.push("--workflow".to_string());
        args.push(format!("{WORKFLOW_MOUNT}/{key}"));
    }
}

/// Append the agent's bound MCP servers to the rendered container args, each
/// pointing at the MCPGateway facade (`--mcp <name>=<gateway>/s/<name>`) with
/// its trifecta tags (`--mcp-tags <name>=<comma-list>`). The agent dials the
/// gateway keyless (trusting the cluster CA via the already-rendered `--tls-ca`);
/// the gateway attests it, scopes it to these servers, and injects each server's
/// credential off-pod (RFC 0019). Idempotent per server name.
pub fn inject_mcp_servers(rendered: &mut Rendered, gateway_url: &str, servers: &[McpBinding]) {
    if servers.is_empty() {
        return;
    }
    let pod = match rendered {
        Rendered::Job(job) => job.spec.as_mut().map(|s| &mut s.template),
        Rendered::CronJob(cj) => cj
            .spec
            .job_template
            .spec
            .as_mut()
            .map(|js| &mut js.template),
        Rendered::Deployment(dep) => dep.spec.as_mut().map(|s| &mut s.template),
        Rendered::StatefulSet(sts) => sts.spec.as_mut().map(|s| &mut s.template),
    };
    let Some(pod) = pod else { return };
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    let Some(container) = spec.containers.first_mut() else {
        return;
    };
    let args = container.args.get_or_insert_with(Vec::new);
    let base = gateway_url.trim_end_matches('/');
    for s in servers {
        let mcp_val = format!("{}={}/s/{}", s.name, base, s.name);
        // Idempotent: never render the same server twice.
        if args.iter().any(|a| a == &mcp_val) {
            continue;
        }
        args.push("--mcp".to_string());
        args.push(mcp_val);
        if !s.tags.is_empty() {
            args.push("--mcp-tags".to_string());
            args.push(format!("{}={}", s.name, s.tags.join(",")));
        }
    }
}

/// Writable scratch dir mounted over the read-only root filesystem. With
/// `readOnlyRootFilesystem: true` (see `container_security_context`) the
/// container cannot write to `/`, so the agent's temp scratch needs an explicit
/// writable `emptyDir` here. The management socket dir (`SOCKET_MOUNT`) is a
/// separate mounted volume and stays writable on its own.
const TMP_MOUNT: &str = "/tmp";
const TMP_VOLUME: &str = "tmp";

/// Secret holding the optional in-cluster bearer token (chart `apiToken.enabled`),
/// created by the chart in the control-plane namespace. Both the Secret name and
/// its single key are `AGENTCTL_API_TOKEN`.
pub const API_TOKEN_SECRET: &str = "agentctl-api-token";
/// Env var (and Secret key) the gated services read the bearer token from.
pub const API_TOKEN_ENV: &str = "AGENTCTL_API_TOKEN";

/// What the renderer produced. Boxed to keep the enum small (clippy).
#[derive(Debug, Clone, PartialEq)]
pub enum Rendered {
    /// `once`/`workflow` mode → a batch Job.
    Job(Box<Job>),
    /// `schedule` mode → a CronJob firing the Job on its cron (RFC 0003 §5).
    CronJob(Box<CronJob>),
    /// `loop`/`reactive` Agent, or a claim-mode AgentFleet → a Deployment.
    Deployment(Box<Deployment>),
    /// A shard-mode AgentFleet → a StatefulSet (stable shard identity, RFC 0011).
    StatefulSet(Box<StatefulSet>),
}

/// Why rendering could not proceed (caller surfaces these as a `Validated=False`
/// condition rather than crashing the reconcile loop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The resource has no `.metadata.name`.
    MissingName,
    /// No image to run: a classless `Agent`/fleet template must set `image` (a
    /// classRef is resolved upstream, before rendering — RFC 0004).
    MissingImage,
    /// A shard-mode fleet did not set `scaling.shards` (the partition count `N`).
    MissingShards,
    /// A substrate this renderer does not yet implement.
    UnsupportedSubstrate(Substrate),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::MissingName => write!(f, "resource has no metadata.name"),
            RenderError::MissingImage => {
                write!(f, "image is required (resolve classRef first)")
            }
            RenderError::MissingShards => {
                write!(
                    f,
                    "shard-mode fleet requires scaling.shards (the partition count N)"
                )
            }
            RenderError::UnsupportedSubstrate(s) => {
                write!(f, "substrate {s:?} not implemented by this renderer")
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// Render an `Agent` to its workload (mode→workload, RFC 0003 §5).
pub fn render_agent(agent: &Agent, cfg: &RenderConfig) -> Result<Rendered, RenderError> {
    let name = agent
        .metadata
        .name
        .clone()
        .ok_or(RenderError::MissingName)?;
    let image = agent.spec.image.clone().ok_or(RenderError::MissingImage)?;
    require_stock_unix(agent.spec.substrate)?;

    let labels = managed_labels(&name);
    let meta = owned_meta(
        &name,
        agent.metadata.namespace.clone(),
        &labels,
        owner_ref("Agent", &name, uid_of(&agent.metadata.uid)),
    );
    let pod = pod_template(&agent.spec, &image, &labels, &name, cfg);

    match agent.spec.mode {
        // `workflow` is a supervised one-shot like `once` (→ Job): drive the graph
        // to a terminal status, then exit. (`reactive` + a workflow stays a
        // Deployment — the daemon arm below.)
        // `workflow` is a supervised one-shot like `once` (→ Job).
        Mode::Once | Mode::Workflow => Ok(Rendered::Job(Box::new(Job {
            metadata: meta,
            spec: Some(JobSpec {
                template: pod,
                backoff_limit: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        }))),
        // `schedule` → a real CronJob firing the Job on its cron. Previously this
        // rendered a plain one-shot Job, so a scheduled agent ran exactly ONCE and
        // never on cadence. CEL guarantees `spec.schedule` is set for this mode.
        Mode::Schedule => {
            let sched = agent.spec.schedule.clone().unwrap_or_default();
            Ok(Rendered::CronJob(Box::new(CronJob {
                metadata: meta,
                spec: CronJobSpec {
                    schedule: sched.cron,
                    time_zone: sched.timezone,
                    // Don't stack runs if one overruns its interval.
                    concurrency_policy: Some("Forbid".to_string()),
                    job_template: JobTemplateSpec {
                        spec: Some(JobSpec {
                            template: pod,
                            backoff_limit: Some(0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })))
        }
        Mode::Loop | Mode::Reactive => Ok(Rendered::Deployment(Box::new(Deployment {
            metadata: meta,
            spec: Some(DeploymentSpec {
                // A singleton Agent runs one replica. An AgentFleet omits replicas
                // entirely in claim mode (KEDA owns it) — see `render_fleet`.
                replicas: Some(1),
                selector: label_selector(&labels),
                template: pod,
                ..Default::default()
            }),
            ..Default::default()
        }))),
    }
}

/// Render an `AgentFleet` to its workload (scaling regime, RFC 0011): claim mode
/// → a Deployment with **`replicas` omitted** (KEDA's HPA owns it); shard mode →
/// a StatefulSet whose replica count is the fixed partition count `N`.
pub fn render_fleet(fleet: &AgentFleet, cfg: &RenderConfig) -> Result<Rendered, RenderError> {
    let name = fleet
        .metadata
        .name
        .clone()
        .ok_or(RenderError::MissingName)?;
    // A fleet member is a long-lived worker — claim members poll the work queue,
    // shard members own a partition — so the effective mode MUST be reactive. The
    // per-replica `template.mode` defaults to `once` (and once/loop/schedule/workflow
    // all run to exit), which would CrashLoop under the Deployment/StatefulSet. Pin
    // it here so a fleet with an unset or mismatched template mode still runs.
    let mut template = fleet.spec.template.clone();
    template.mode = Mode::Reactive;
    let spec = &template;
    let image = spec.image.clone().ok_or(RenderError::MissingImage)?;
    require_stock_unix(spec.substrate)?;

    let labels = managed_labels(&name);
    let meta = owned_meta(
        &name,
        fleet.metadata.namespace.clone(),
        &labels,
        owner_ref("AgentFleet", &name, uid_of(&fleet.metadata.uid)),
    );
    let mut pod = pod_template(spec, &image, &labels, &name, cfg);

    match fleet.spec.scaling.mode {
        ScaleMode::Claim => Ok(Rendered::Deployment(Box::new(Deployment {
            metadata: meta,
            spec: Some(DeploymentSpec {
                // replicas OMITTED: KEDA's HPA is the sole owner (RFC 0011).
                replicas: None,
                selector: label_selector(&labels),
                template: pod,
                ..Default::default()
            }),
            ..Default::default()
        }))),
        ScaleMode::Shard => {
            let shards = fleet
                .spec
                .scaling
                .shards
                .ok_or(RenderError::MissingShards)?;
            // Shard identity (RFC 0003 §9.1 P3): inject only `N` (identical across
            // every StatefulSet pod, as a shared template requires); the agent
            // derives its own `K` from the ordinal in `AGENT_POD_NAME`
            // (`<sts>-<ordinal>`). `--shard auto/N` is the contract spelling of that.
            inject_shard_identity(&mut pod, shards);
            Ok(Rendered::StatefulSet(Box::new(StatefulSet {
                metadata: meta,
                spec: Some(StatefulSetSpec {
                    // shard mode: replicas = N (the partition count), NOT KEDA-owned.
                    replicas: Some(shards as i32),
                    // headless Service for stable per-shard network identity.
                    service_name: Some(name.clone()),
                    selector: label_selector(&labels),
                    template: pod,
                    ..Default::default()
                }),
                ..Default::default()
            })))
        }
    }
}

/// Inject `--shard auto/N` into a shard StatefulSet pod's agent container (RFC 0003
/// §9.1 P3). Only `N` is templated — it is identical across all ordinals, which a
/// shared pod template requires; the agent self-derives its `K` from the ordinal in
/// `AGENT_POD_NAME`. Idempotent (no-op if a `--shard` flag is already present).
fn inject_shard_identity(pod: &mut PodTemplateSpec, shards: u32) {
    let Some(container) = pod.spec.as_mut().and_then(|s| s.containers.first_mut()) else {
        return;
    };
    let args = container.args.get_or_insert_with(Vec::new);
    if args.iter().any(|a| a == "--shard") {
        return;
    }
    args.push("--shard".to_string());
    args.push(format!("auto/{shards}"));
}

/// The label distinguishing a fleet's coordinator workload from its worker pool
/// (RFC 0022 §5). Worker pods keep only the existing `agentctl.dev/agent=<fleet>`
/// label; the coordinator carries its own name label PLUS these two.
pub const FLEET_ROLE_LABEL: &str = "agentctl.dev/fleet-role";
/// The label tying a coordinator back to its fleet (for cross-member discovery).
pub const FLEET_LABEL: &str = "agentctl.dev/fleet";

/// The coordinator workload's name for a fleet: `<fleet>-coordinator`. Distinct
/// from the fleet name so the worker Deployment's `agentctl.dev/agent=<fleet>`
/// selector never captures coordinator pods (and vice-versa), and so the gateway
/// can address the coordinator by `agentctl.dev/agent=<fleet>-coordinator`.
pub fn coordinator_name(fleet: &str) -> String {
    format!("{fleet}-coordinator")
}

/// Render an `AgentFleet`'s **coordinator** ("main agent", RFC 0022 §3) when one is
/// declared; `None` for a coordinatorless fleet. The coordinator is a long-lived
/// reactive front door rendered as its own Deployment `<fleet>-coordinator`,
/// labeled `fleet-role: coordinator`, replicas `coordinator.replicas` (default 1),
/// wired to fan work out per `distribution` (queue ⇒ a work-source env hint; a2a ⇒
/// an `--a2a-peer worker=…/fleets/<ns>/<name>` through the gateway PEP).
pub fn render_coordinator(
    fleet: &AgentFleet,
    cfg: &RenderConfig,
) -> Option<Result<Rendered, RenderError>> {
    let coord = fleet.spec.coordinator.as_ref()?;
    Some(render_coordinator_inner(fleet, coord, cfg))
}

fn render_coordinator_inner(
    fleet: &AgentFleet,
    coord: &agent_api::Coordinator,
    cfg: &RenderConfig,
) -> Result<Rendered, RenderError> {
    let fleet_name = fleet
        .metadata
        .name
        .clone()
        .ok_or(RenderError::MissingName)?;
    let name = coordinator_name(&fleet_name);
    // The coordinator is a long-lived A2A front door: coerce to reactive so a
    // run-to-exit mode does not CrashLoop under the Deployment (admission already
    // forbids `once`). A workflow-driving coordinator is a future extension.
    let mut template = coord.template.clone();
    template.mode = Mode::Reactive;
    let spec = &template;
    let image = spec.image.clone().ok_or(RenderError::MissingImage)?;
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
    let mut pod = pod_template(spec, &image, &labels, &name, cfg);
    apply_distribution(
        &mut pod,
        coord.distribution.unwrap_or_default(),
        fleet,
        cfg,
        &fleet_name,
    );

    let replicas = coord.replicas.unwrap_or(1).max(1) as i32;
    Ok(Rendered::Deployment(Box::new(Deployment {
        metadata: meta,
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            selector: label_selector(&labels),
            template: pod,
            ..Default::default()
        }),
        ..Default::default()
    })))
}

/// Wire the coordinator's work fan-out into its pod (RFC 0022 §5). `queue` (the
/// default): inject `AGENT_FLEET_WORKSOURCE` (the fleet `workSource`) as a config
/// hint so a conformant coordinator knows where to `work.submit`/`work.result`.
/// `a2a`: append `--a2a-peer worker=<gateway>/fleets/<ns>/<fleet>` so the
/// coordinator's `a2a.delegate` reaches the worker pool through the gateway PEP.
fn apply_distribution(
    pod: &mut PodTemplateSpec,
    distribution: Distribution,
    fleet: &AgentFleet,
    cfg: &RenderConfig,
    fleet_name: &str,
) {
    let Some(container) = pod.spec.as_mut().and_then(|s| s.containers.first_mut()) else {
        return;
    };
    match distribution {
        Distribution::Queue => {
            if let Some(ws) = fleet.spec.work_source.as_deref() {
                container.env.get_or_insert_with(Vec::new).push(EnvVar {
                    name: "AGENT_FLEET_WORKSOURCE".to_string(),
                    value: Some(ws.to_string()),
                    ..Default::default()
                });
            }
        }
        Distribution::A2a => {
            let ns = fleet.metadata.namespace.as_deref().unwrap_or("default");
            let peer = format!(
                "{}/fleets/{}/{}",
                cfg.gateway_url.trim_end_matches('/'),
                ns,
                fleet_name
            );
            let args = container.args.get_or_insert_with(Vec::new);
            args.push("--a2a-peer".to_string());
            args.push(format!("worker={peer}"));
        }
    }
}

/// Inject the optional in-cluster bearer token (`AGENTCTL_API_TOKEN`, `valueFrom`
/// a `secretKeyRef` on [`API_TOKEN_SECRET`]) into the rendered agent pod's first
/// container env, so a conformant agent can present it to the token-gated
/// coordination server / ModelGateway (chart `apiToken.enabled`). Idempotent: a
/// no-op if the env var is already set (e.g. a user `extraEnv`).
///
/// LIMITATION (documented, not silently broken): a `secretKeyRef` resolves only
/// within the pod's OWN namespace. The chart creates [`API_TOKEN_SECRET`] in the
/// control-plane namespace, so this injection only yields a *resolvable* ref for
/// agents in that namespace. The caller therefore gates injection on the agent
/// being in the control-plane namespace (see
/// `controller::ApiTokenConfig::should_inject`); agents in other namespaces need
/// the Secret replicated there before the operator should inject it.
pub fn inject_api_token(rendered: &mut Rendered) {
    let pod = match rendered {
        Rendered::Job(job) => job.spec.as_mut().map(|s| &mut s.template),
        Rendered::CronJob(cj) => cj
            .spec
            .job_template
            .spec
            .as_mut()
            .map(|js| &mut js.template),
        Rendered::Deployment(dep) => dep.spec.as_mut().map(|s| &mut s.template),
        Rendered::StatefulSet(sts) => sts.spec.as_mut().map(|s| &mut s.template),
    };
    let Some(pod) = pod else { return };
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    let Some(container) = spec.containers.first_mut() else {
        return;
    };
    let env = container.env.get_or_insert_with(Vec::new);
    // Idempotent: never duplicate (or shadow) an existing AGENTCTL_API_TOKEN.
    if env.iter().any(|e| e.name == API_TOKEN_ENV) {
        return;
    }
    env.push(EnvVar {
        name: API_TOKEN_ENV.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: API_TOKEN_SECRET.to_string(),
                key: API_TOKEN_ENV.to_string(),
                optional: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    });
}

/// Default in-cluster address of the agentctl KEDA external scaler (gRPC). The
/// operator overrides this from `SCALER_ADDRESS`; KEDA dials it for the external
/// trigger (RFC 0011).
pub const DEFAULT_SCALER_ADDRESS: &str = "agentctl-scaler.agentctl-system:9100";
/// Default in-cluster coordination-server base URL the scaler reads the claim
/// backlog (`work.stats`) from. Overridden from `COORDINATION_URL`, or per-fleet
/// by `spec.workSource` when set.
pub const DEFAULT_COORDINATION_URL: &str = "http://agentctl-coordination.agentctl-system/";
/// Fallback per-replica backlog target KEDA scales toward when a claim fleet does
/// not set `scaling.target.value`.
const DEFAULT_SCALE_THRESHOLD: &str = "5";

/// Render the KEDA `ScaledObject` that autoscales a **claim-mode** fleet's
/// Deployment off the coordination backlog (RFC 0011), or `None` for shard mode
/// (a StatefulSet with a fixed partition count `N` is NOT KEDA-driven, so the
/// caller emits no ScaledObject for it).
///
/// Built as an untyped [`serde_json::Value`] (a kube `DynamicObject` body) so the
/// operator carries **no hard dependency on the KEDA CRD types**: a cluster
/// without KEDA simply never has this object applied (the controller gates on a
/// config flag and applies it best-effort).
///
/// The KEDA-safe invariant holds: this object — not the rendered Deployment —
/// owns the replica count (`scaleTargetRef` → the Deployment, whose
/// `.spec.replicas` stays unset; see [`render_fleet`]).
///
/// - `scaler_address` — gRPC address KEDA dials for the external trigger
///   (operator `SCALER_ADDRESS`, default [`DEFAULT_SCALER_ADDRESS`]).
/// - `coordination_url` — coordination base URL the scaler reads the backlog
///   from; the fleet's own `spec.workSource` wins when set, else this operator
///   default (`COORDINATION_URL`, default [`DEFAULT_COORDINATION_URL`]).
pub fn render_scaled_object(
    fleet: &AgentFleet,
    scaler_address: &str,
    coordination_url: &str,
) -> Option<serde_json::Value> {
    // Shard mode is a fixed partition count — no KEDA autoscaling.
    if fleet.spec.scaling.mode != ScaleMode::Claim {
        return None;
    }
    let name = fleet.metadata.name.clone()?;
    let scaling = &fleet.spec.scaling;

    // minReplicaCount defaults to 0 (scale-to-zero); maxReplicaCount is emitted
    // only when set (else KEDA's own default applies).
    let min = scaling.min.unwrap_or(0);
    // threshold: the per-replica backlog target (scaling.target.value, default 5).
    let threshold = scaling
        .target
        .as_ref()
        .map(|t| t.value.clone())
        .unwrap_or_else(|| DEFAULT_SCALE_THRESHOLD.to_string());
    // coordinationUrl: the fleet's own work source wins; else the operator default.
    let coordination_url = fleet
        .spec
        .work_source
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| coordination_url.to_string());

    let mut spec = serde_json::json!({
        // scaleTargetRef.name = the rendered Deployment (same name as the fleet).
        "scaleTargetRef": { "name": name },
        "minReplicaCount": min,
        "triggers": [{
            "type": "external",
            "metadata": {
                "scalerAddress": scaler_address,
                "coordinationUrl": coordination_url,
                "threshold": threshold,
                "activationThreshold": "1",
            }
        }]
    });
    if let Some(max) = scaling.max {
        spec["maxReplicaCount"] = serde_json::json!(max);
    }

    let mut metadata = serde_json::json!({
        "name": name,
        "labels": managed_labels(&name),
        // ownerRef → the AgentFleet so GC reclaims the ScaledObject with the fleet.
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

fn require_stock_unix(substrate: Option<Substrate>) -> Result<(), RenderError> {
    match substrate.unwrap_or(Substrate::StockUnix) {
        Substrate::StockUnix => Ok(()),
        // Kata-hybrid swaps the volume source only; sidecar adds a sibling
        // container. Both reuse the rest of this shape (RFC 0002) — not yet wired.
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

/// The label-selector STRING matching a fleet's pods, for the scale
/// subresource's `labelSelectorPath` (`.status.selector`). Built from the SAME
/// [`managed_labels`] the rendered workload's `.spec.selector.matchLabels` and
/// pod template carry, so an HPA reading `.status.selector` resolves exactly the
/// operator-managed pods. Formatted as comma-separated `key=value` pairs in the
/// `BTreeMap`'s sorted key order, so the string is deterministic.
pub fn fleet_selector_string(name: &str) -> String {
    selector_string(&managed_labels(name))
}

/// Serialize a `matchLabels` map to the equality-based label-selector string
/// form Kubernetes uses (`k1=v1,k2=v2`, keys sorted).
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
    // uid may be empty before the apiserver assigns it; that's fine for a
    // dry-run render and is populated on the live object.
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

fn pod_template(
    spec: &AgentSpec,
    image: &str,
    labels: &BTreeMap<String, String>,
    workload: &str,
    cfg: &RenderConfig,
) -> PodTemplateSpec {
    let restart_policy = match spec.mode {
        Mode::Once | Mode::Schedule | Mode::Workflow => Some("Never".to_string()),
        // Deployments/StatefulSets require Always.
        Mode::Loop | Mode::Reactive => None,
    };

    let mut env = downward_env();
    // Keyless intelligence dial: the ModelGateway holds the provider credential
    // and attests the caller by source IP — NO token env is ever rendered.
    env.push(EnvVar {
        name: "AGENT_INTELLIGENCE".to_string(),
        value: Some(cfg.modelgateway_url.clone()),
        ..Default::default()
    });
    // Metrics + readiness listener (`/readyz`), probed below and scraped directly
    // (the pod is network-attached; there is no scrape proxy).
    env.push(EnvVar {
        name: "AGENT_METRICS_ADDR".to_string(),
        value: Some(format!("0.0.0.0:{METRICS_PORT}")),
        ..Default::default()
    });

    let volume_mounts = vec![
        // The workload's OWN serving identity (cert-manager Secret; tls.crt/tls.key).
        // Read-only; the agent re-reads it in place on rotation (agentd ≥2.1).
        VolumeMount {
            name: TLS_VOLUME.to_string(),
            mount_path: TLS_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        // The cluster CA public cert (client-CA + outbound trust anchor).
        VolumeMount {
            name: CA_VOLUME.to_string(),
            mount_path: CA_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        // Writable scratch: `readOnlyRootFilesystem` makes `/` read-only, so
        // give the agent an explicit writable `/tmp`.
        VolumeMount {
            name: TMP_VOLUME.to_string(),
            mount_path: TMP_MOUNT.to_string(),
            ..Default::default()
        },
    ];
    let volumes = vec![
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
        // Backs the writable `/tmp` mount above.
        Volume {
            name: TMP_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
    ];

    let mut args = agent_args(spec);
    args.extend(serve_args());

    let container = Container {
        name: "agent".to_string(),
        image: Some(image.to_string()),
        args: Some(args),
        env: Some(env),
        ports: Some(vec![
            ContainerPort {
                name: Some("mcp".to_string()),
                container_port: SERVE_PORT,
                ..Default::default()
            },
            ContainerPort {
                name: Some("metrics".to_string()),
                container_port: METRICS_PORT,
                ..Default::default()
            },
        ]),
        // Readiness = the contract's `/readyz` on the metrics listener (drain /
        // lame-duck / all-endpoints-down flip it, so ready == accepting work).
        readiness_probe: Some(Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/readyz".to_string()),
                port: IntOrString::Int(METRICS_PORT),
                ..Default::default()
            }),
            ..Default::default()
        }),
        // Confine the tenant container (hostile multi-tenancy P0).
        security_context: Some(container_security_context()),
        volume_mounts: Some(volume_mounts),
        ..Default::default()
    };

    PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            ..Default::default()
        }),
        spec: Some(PodSpec {
            containers: vec![container],
            restart_policy,
            // Pod-level hardening (hostile multi-tenancy P0).
            security_context: Some(pod_security_context()),
            // The pod holds NO borrowed credential — and no ambient one either:
            // never auto-mount the namespace default ServiceAccount token.
            automount_service_account_token: Some(false),
            // Share the pod PID namespace so the pod's infra (pause) container is
            // PID 1 and the agent is NOT. A conformant agent (e.g. agentd) forks a
            // worker subagent guarded by an orphan check (`getppid() == 1` ⇒ the
            // supervisor died, bail): with the agent running as PID 1 (scratch
            // image, agent as ENTRYPOINT) that check misfires — the worker's parent
            // IS pid 1 — so EVERY run aborts before doing any work. Sharing the PID
            // namespace gives the agent a non-1 pid so the guard is correct.
            share_process_namespace: Some(true),
            volumes: Some(volumes),
            ..Default::default()
        }),
    }
}

/// The HTTPS serve + trust args every rendered agent gets (contract 2.0): serve
/// the self-MCP/A2A surface mTLS-gated on [`SERVE_PORT`], trust cluster-CA
/// client certs (`Management` = the control plane), and trust the same CA for
/// outbound dials (the gateways).
fn serve_args() -> Vec<String> {
    vec![
        "--serve-mcp".to_string(),
        format!("https://0.0.0.0:{SERVE_PORT}"),
        "--serve-cert".to_string(),
        format!("{TLS_MOUNT}/tls.crt"),
        "--serve-key".to_string(),
        format!("{TLS_MOUNT}/tls.key"),
        "--serve-client-ca".to_string(),
        format!("{CA_MOUNT}/{CA_KEY}"),
        "--tls-ca".to_string(),
        format!("{CA_MOUNT}/{CA_KEY}"),
    ]
}

/// Container-level confinement for the tenant agent (hostile multi-tenancy P0):
/// **nonroot enforced**, no privilege escalation, all Linux capabilities
/// dropped, read-only root filesystem (writable paths are explicit volumes —
/// `/tmp`). With no hostPath socket to bind (the v2 pivot removed it), the
/// reference image's native `USER 65532` runs unchanged and the whole render
/// satisfies the `restricted` Pod Security Standard.
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

/// Pod-level confinement: pin the seccomp profile to the runtime default so the
/// kernel syscall surface is filtered for every container in the pod.
fn pod_security_context() -> PodSecurityContext {
    PodSecurityContext {
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The downward-API instance-identity env (contract `env-convention`, RFC
/// 0014 §6.4). Emitted with the `AGENT_*` spelling the reference agent reads
/// (contract `env-convention` / README map). The serve instruction is NOT env
/// anymore — it is the `--serve-mcp https://…` argv ([`serve_args`]).
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

/// Container args derived from the spec (mode + instruction + model +
/// subscriptions). A later step renders the full config via a ConfigMap (RFC
/// 0017); args keep the render self-contained and testable.
fn agent_args(spec: &AgentSpec) -> Vec<String> {
    let mut args = vec!["--mode".to_string(), mode_str(spec.mode).to_string()];
    if let Some(instruction) = &spec.instruction {
        args.push("--instruction".to_string());
        args.push(instruction.clone());
    }
    if let Some(model) = &spec.model {
        args.push("--model".to_string());
        args.push(model.clone());
    }
    for sub in &spec.subscribe {
        args.push("--subscribe".to_string());
        args.push(sub.clone());
    }
    // Deliver the declared bounding box to the agent (RFC 0003 §4.1). Without this
    // the operator silently dropped `spec.limits`, so subagent-tree/step/token caps
    // set on the CR never reached agentd (which consumes these flags). `max_tokens`
    // / `max_depth` / `max_steps` map to agentd flags; `tree_token_budget` has no
    // agentd flag yet (hardcoded Caps default — an agentd P-cap follow-up), so it is
    // deliberately not emitted rather than passed to an unknown flag.
    if let Some(limits) = &spec.limits {
        if let Some(v) = limits.max_tokens {
            args.push("--max-tokens".to_string());
            args.push(v.to_string());
        }
        if let Some(v) = limits.max_depth {
            args.push("--max-depth".to_string());
            args.push(v.to_string());
        }
        if let Some(v) = limits.max_steps {
            args.push("--max-steps".to_string());
            args.push(v.to_string());
        }
    }
    args
}

fn mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Once => "once",
        Mode::Loop => "loop",
        Mode::Reactive => "reactive",
        Mode::Schedule => "schedule",
        Mode::Workflow => "workflow",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_api::{AgentFleetSpec, DesiredSurfaces, Scaling};

    fn agent(mode: Mode) -> Agent {
        let mut a = Agent::new(
            "demo",
            AgentSpec {
                mode,
                image: Some("ghcr.io/example/agent@sha256:abc".into()),
                instruction: Some("do the thing".into()),
                surfaces: Some(DesiredSurfaces {
                    management: true,
                    metrics: false,
                    a2a: false,
                }),
                ..Default::default()
            },
        );
        a.metadata.namespace = Some("agents".into());
        a.metadata.uid = Some("uid-1".into());
        a
    }

    fn fleet(mode: ScaleMode, shards: Option<u32>) -> AgentFleet {
        let mut f = AgentFleet::new(
            "workers",
            AgentFleetSpec {
                template: AgentSpec {
                    mode: Mode::Reactive,
                    image: Some("ghcr.io/example/agent@sha256:abc".into()),
                    subscribe: vec!["queue://jobs".into()],
                    ..Default::default()
                },
                scaling: Scaling {
                    mode,
                    shards,
                    max: if mode == ScaleMode::Claim {
                        Some(10)
                    } else {
                        None
                    },
                    ..Default::default()
                },
                work_source: Some("queue://jobs".into()),
                replicas: None,
                ..Default::default()
            },
        );
        f.metadata.namespace = Some("agents".into());
        f.metadata.uid = Some("fleet-uid".into());
        f
    }

    fn container_of(pod: &PodTemplateSpec) -> &Container {
        &pod.spec.as_ref().unwrap().containers[0]
    }

    fn cfg() -> RenderConfig {
        RenderConfig::default()
    }

    fn has_arg_pair(c: &Container, k: &str, v: &str) -> bool {
        c.args
            .as_ref()
            .unwrap()
            .windows(2)
            .any(|w| w == [k.to_string(), v.to_string()])
    }

    #[test]
    fn once_renders_a_job() {
        let r = render_agent(&agent(Mode::Once), &cfg()).unwrap();
        let Rendered::Job(job) = r else {
            panic!("expected a Job")
        };
        assert_eq!(job.metadata.name.as_deref(), Some("demo"));
        assert_eq!(job.metadata.namespace.as_deref(), Some("agents"));
        let spec = job.spec.unwrap();
        assert_eq!(spec.backoff_limit, Some(0));
        let pod = spec.template;
        assert_eq!(
            pod.spec.as_ref().unwrap().restart_policy.as_deref(),
            Some("Never")
        );
        let c = container_of(&pod);
        assert_eq!(c.image.as_deref(), Some("ghcr.io/example/agent@sha256:abc"));
        let owners = job.metadata.owner_references.unwrap();
        assert_eq!(owners[0].kind, "Agent");
        assert_eq!(owners[0].controller, Some(true));
    }

    #[test]
    fn reactive_renders_a_singleton_deployment() {
        let mut a = agent(Mode::Reactive);
        a.spec.subscribe = vec!["file:///data/inbox".into()];
        let r = render_agent(&a, &cfg()).unwrap();
        let Rendered::Deployment(dep) = r else {
            panic!("expected a Deployment")
        };
        let spec = dep.spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(
            spec.selector
                .match_labels
                .as_ref()
                .unwrap()
                .get("agentctl.dev/agent")
                .map(String::as_str),
            Some("demo")
        );
        let c = container_of(&spec.template);
        assert!(has_arg_pair(c, "--subscribe", "file:///data/inbox"));
    }

    #[test]
    fn serve_wiring_v2() {
        // Every rendered pod SERVES mTLS-gated HTTPS (contract 2.0): the serve
        // argv, its own serving-identity Secret mount, the cluster-CA ConfigMap
        // mount (client-CA + outbound trust), ports, and the /readyz probe.
        let r = render_agent(&agent(Mode::Once), &cfg()).unwrap();
        let Rendered::Job(job) = r else {
            unreachable!()
        };
        let pod = job.spec.unwrap().template;
        let podspec = pod.spec.as_ref().unwrap();
        let c = container_of(&pod);

        // Serve + trust argv.
        assert!(has_arg_pair(c, "--serve-mcp", "https://0.0.0.0:8443"));
        assert!(has_arg_pair(c, "--serve-cert", "/etc/agentctl/tls/tls.crt"));
        assert!(has_arg_pair(c, "--serve-key", "/etc/agentctl/tls/tls.key"));
        assert!(has_arg_pair(
            c,
            "--serve-client-ca",
            "/etc/agentctl/ca/ca.crt"
        ));
        assert!(has_arg_pair(c, "--tls-ca", "/etc/agentctl/ca/ca.crt"));

        // The workload's OWN serving identity, mounted read-only.
        let mounts = c.volume_mounts.as_ref().unwrap();
        let tls = mounts
            .iter()
            .find(|m| m.name == TLS_VOLUME)
            .expect("serving-tls mounted");
        assert_eq!(tls.mount_path, "/etc/agentctl/tls");
        assert_eq!(tls.read_only, Some(true));
        let volumes = podspec.volumes.as_ref().unwrap();
        let tls_vol = volumes.iter().find(|v| v.name == TLS_VOLUME).unwrap();
        assert_eq!(
            tls_vol.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("demo-serving-tls")
        );

        // The cluster CA (public), mounted read-only from the per-ns ConfigMap.
        let ca = mounts
            .iter()
            .find(|m| m.name == CA_VOLUME)
            .expect("ca mounted");
        assert_eq!(ca.mount_path, "/etc/agentctl/ca");
        assert_eq!(ca.read_only, Some(true));
        let ca_vol = volumes.iter().find(|v| v.name == CA_VOLUME).unwrap();
        assert_eq!(ca_vol.config_map.as_ref().unwrap().name, CA_CONFIGMAP);

        // NO sockets, NO hostPath anywhere (restricted-PSS-clean).
        assert!(volumes.iter().all(|v| v.host_path.is_none()));
        assert!(!c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| e.name == "AGENT_SERVE_MCP"));

        // Ports + the /readyz readiness probe on the metrics listener.
        let ports = c.ports.as_ref().unwrap();
        assert!(ports.iter().any(|p| p.container_port == SERVE_PORT));
        assert!(ports.iter().any(|p| p.container_port == METRICS_PORT));
        let probe = c.readiness_probe.as_ref().unwrap();
        let get = probe.http_get.as_ref().unwrap();
        assert_eq!(get.path.as_deref(), Some("/readyz"));
        assert_eq!(get.port, IntOrString::Int(METRICS_PORT));

        // Downward-API identity env intact.
        let env = c.env.as_ref().unwrap();
        let uid = env.iter().find(|e| e.name == "AGENT_POD_UID").unwrap();
        assert_eq!(
            uid.value_from
                .as_ref()
                .unwrap()
                .field_ref
                .as_ref()
                .unwrap()
                .field_path,
            "metadata.uid"
        );
    }

    #[test]
    fn intelligence_env_is_keyless_and_from_config() {
        let custom = RenderConfig {
            modelgateway_url: "https://mgw.cp.svc.cluster.local.".into(),
            ..RenderConfig::default()
        };
        let r = render_agent(&agent(Mode::Reactive), &custom).unwrap();
        let Rendered::Deployment(dep) = r else {
            unreachable!()
        };
        let c = container_of(&dep.spec.as_ref().unwrap().template).clone();
        let env = c.env.as_ref().unwrap();
        let intel = env
            .iter()
            .find(|e| e.name == "AGENT_INTELLIGENCE")
            .expect("AGENT_INTELLIGENCE rendered");
        assert_eq!(
            intel.value.as_deref(),
            Some("https://mgw.cp.svc.cluster.local.")
        );
        // Keyless: NO intelligence token env of any spelling.
        assert!(!env.iter().any(|e| e.name.contains("INTELLIGENCE_TOKEN")));
        // Metrics listener env for the /readyz probe + direct scrape.
        let metrics = env.iter().find(|e| e.name == "AGENT_METRICS_ADDR").unwrap();
        assert_eq!(metrics.value.as_deref(), Some("0.0.0.0:9090"));
    }

    #[test]
    fn rendered_pod_is_confined() {
        // Hardening must apply to every rendered workload; exercise the Job path
        // (all kinds share `pod_template`).
        let r = render_agent(&agent(Mode::Once), &cfg()).unwrap();
        let Rendered::Job(job) = r else {
            unreachable!()
        };
        let pod = job.spec.unwrap().template;
        let podspec = pod.spec.as_ref().unwrap();

        // Pod-level: seccomp pinned; no ambient SA credential; PID ns shared
        // (the agentd orphan-guard, see pod_template).
        let psc = podspec
            .security_context
            .as_ref()
            .expect("pod securityContext present");
        assert_eq!(
            psc.seccomp_profile.as_ref().unwrap().type_,
            "RuntimeDefault"
        );
        assert_eq!(podspec.automount_service_account_token, Some(false));
        assert_eq!(podspec.share_process_namespace, Some(true));

        // Container-level: NONROOT (restricted PSS — no hostPath socket to bind
        // anymore), no priv-esc, drop ALL caps, read-only root fs.
        let c = container_of(&pod);
        let sc = c
            .security_context
            .as_ref()
            .expect("container securityContext present");
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.run_as_user, None);
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(sc.read_only_root_filesystem, Some(true));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop.as_deref(),
            Some(["ALL".to_string()].as_slice())
        );

        // Writable /tmp emptyDir backs the read-only root filesystem.
        let mounts = c.volume_mounts.as_ref().unwrap();
        let tmp_mount = mounts
            .iter()
            .find(|m| m.mount_path == "/tmp")
            .expect("/tmp mount present");
        assert_eq!(tmp_mount.name, "tmp");
        assert_ne!(tmp_mount.read_only, Some(true));
        let volumes = podspec.volumes.as_ref().unwrap();
        let tmp_vol = volumes
            .iter()
            .find(|v| v.name == "tmp")
            .expect("tmp volume present");
        assert!(tmp_vol.empty_dir.is_some(), "tmp volume is an emptyDir");
    }

    #[test]
    fn inject_api_token_adds_secret_key_ref_env() {
        let mut r = render_agent(&agent(Mode::Reactive), &cfg()).unwrap();
        inject_api_token(&mut r);
        let Rendered::Deployment(dep) = &r else {
            unreachable!()
        };
        let c = container_of(&dep.spec.as_ref().unwrap().template);
        let token = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == API_TOKEN_ENV)
            .expect("AGENTCTL_API_TOKEN env injected");
        let sel = token
            .value_from
            .as_ref()
            .unwrap()
            .secret_key_ref
            .as_ref()
            .unwrap();
        assert_eq!(sel.name, API_TOKEN_SECRET);
        assert_eq!(sel.key, API_TOKEN_ENV);
        // The downward-API identity env is preserved alongside the injected token.
        assert!(c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| e.name == "AGENT_POD_UID"));
    }

    #[test]
    fn inject_mcp_servers_renders_gateway_urls_and_tags() {
        let mut r = render_agent(&agent(Mode::Reactive), &cfg()).unwrap();
        let servers = vec![
            McpBinding {
                name: "github".into(),
                tags: vec!["untrusted_input".into(), "egress".into()],
            },
            McpBinding {
                name: "fs".into(),
                tags: vec![],
            },
        ];
        inject_mcp_servers(&mut r, "https://mcpgw.cp.svc.cluster.local.", &servers);
        inject_mcp_servers(&mut r, "https://mcpgw.cp.svc.cluster.local.", &servers); // idempotent
        let Rendered::Deployment(dep) = &r else {
            unreachable!()
        };
        let c = container_of(&dep.spec.as_ref().unwrap().template);
        // github: --mcp github=<gw>/s/github + --mcp-tags github=untrusted_input,egress
        assert!(has_arg_pair(
            c,
            "--mcp",
            "github=https://mcpgw.cp.svc.cluster.local./s/github"
        ));
        assert!(has_arg_pair(
            c,
            "--mcp-tags",
            "github=untrusted_input,egress"
        ));
        // fs: dial rendered, but no tags flag (empty tags).
        assert!(has_arg_pair(
            c,
            "--mcp",
            "fs=https://mcpgw.cp.svc.cluster.local./s/fs"
        ));
        assert!(!c
            .args
            .as_ref()
            .unwrap()
            .iter()
            .any(|a| a.starts_with("fs=") && a.contains("=,")));
        // Idempotent: exactly one --mcp entry for github.
        let n = c
            .args
            .as_ref()
            .unwrap()
            .iter()
            .filter(|a| a.as_str() == "github=https://mcpgw.cp.svc.cluster.local./s/github")
            .count();
        assert_eq!(n, 1);
        // The agent trusts the gateway via the already-rendered --tls-ca.
        assert!(has_arg_pair(c, "--tls-ca", "/etc/agentctl/ca/ca.crt"));
    }

    #[test]
    fn workflow_mode_renders_a_job_with_workflow_mount() {
        let mut a = agent(Mode::Once);
        a.spec.mode = Mode::Workflow;
        a.spec.workflow = Some(agent_api::WorkflowSource {
            inline: Some(r#"{"nodes":[]}"#.into()),
            config_map_key_ref: None,
        });
        let mut r = render_agent(&a, &cfg()).unwrap();
        // workflow is a supervised one-shot → a Job.
        let Rendered::Job(job) = &r else {
            panic!("workflow mode must render a Job")
        };
        let c = container_of(&job.spec.as_ref().unwrap().template);
        assert!(has_arg_pair(c, "--mode", "workflow"));

        // The controller injects the mount (inline → the generated ConfigMap).
        inject_workflow(&mut r, &workflow_configmap_name("demo"), "workflow.json");
        inject_workflow(&mut r, &workflow_configmap_name("demo"), "workflow.json"); // idempotent
        let Rendered::Job(job) = &r else {
            unreachable!()
        };
        let pod = &job.spec.as_ref().unwrap().template;
        let c = container_of(pod);
        assert!(has_arg_pair(
            c,
            "--workflow",
            "/etc/agentctl/workflow/workflow.json"
        ));
        let mounts = c.volume_mounts.as_ref().unwrap();
        let wf = mounts
            .iter()
            .find(|m| m.mount_path == "/etc/agentctl/workflow")
            .expect("workflow mount present");
        assert_eq!(wf.read_only, Some(true));
        let vol = pod
            .spec
            .as_ref()
            .unwrap()
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "agentctl-workflow")
            .expect("workflow volume present");
        assert_eq!(vol.config_map.as_ref().unwrap().name, "demo-workflow");
        // Idempotent: exactly one --workflow flag.
        let n = c
            .args
            .as_ref()
            .unwrap()
            .iter()
            .filter(|a| a.as_str() == "--workflow")
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn inject_api_token_is_idempotent() {
        let mut r = render_agent(&agent(Mode::Once), &cfg()).unwrap();
        inject_api_token(&mut r);
        inject_api_token(&mut r);
        let Rendered::Job(job) = &r else {
            unreachable!()
        };
        let c = container_of(&job.spec.as_ref().unwrap().template);
        let n = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .filter(|e| e.name == API_TOKEN_ENV)
            .count();
        assert_eq!(n, 1, "token env must not be duplicated");
    }

    #[test]
    fn missing_image_is_an_error() {
        let mut a = agent(Mode::Once);
        a.spec.image = None;
        assert_eq!(render_agent(&a, &cfg()), Err(RenderError::MissingImage));
    }

    #[test]
    fn non_stock_substrate_not_yet_supported() {
        let mut a = agent(Mode::Once);
        a.spec.substrate = Some(Substrate::KataHybrid);
        assert_eq!(
            render_agent(&a, &cfg()),
            Err(RenderError::UnsupportedSubstrate(Substrate::KataHybrid))
        );
    }

    #[test]
    fn claim_fleet_renders_deployment_with_replicas_omitted() {
        let r = render_fleet(&fleet(ScaleMode::Claim, None), &cfg()).unwrap();
        let Rendered::Deployment(dep) = r else {
            panic!("expected a Deployment")
        };
        let spec = dep.spec.unwrap();
        // KEDA owns replicas → omitted from the rendered workload.
        assert_eq!(spec.replicas, None);
        assert_eq!(dep.metadata.owner_references.unwrap()[0].kind, "AgentFleet");
    }

    #[test]
    fn shard_fleet_renders_statefulset_with_n_replicas() {
        let r = render_fleet(&fleet(ScaleMode::Shard, Some(3)), &cfg()).unwrap();
        let Rendered::StatefulSet(sts) = r else {
            panic!("expected a StatefulSet")
        };
        let spec = sts.spec.unwrap();
        assert_eq!(spec.replicas, Some(3)); // replicas = N (partition count)
        assert_eq!(spec.service_name.as_deref(), Some("workers"));
    }

    #[test]
    fn shard_fleet_without_shards_is_an_error() {
        assert_eq!(
            render_fleet(&fleet(ScaleMode::Shard, None), &cfg()),
            Err(RenderError::MissingShards)
        );
    }

    #[test]
    fn schedule_mode_renders_a_cronjob() {
        // mode:schedule must render a CronJob firing on its cron — not a one-shot Job
        // (which would run exactly once, never on cadence).
        let mut a = agent(Mode::Schedule);
        a.spec.schedule = Some(agent_api::Schedule {
            cron: "*/5 * * * *".into(),
            timezone: Some("UTC".into()),
        });
        let r = render_agent(&a, &cfg()).unwrap();
        let Rendered::CronJob(cj) = r else {
            panic!("mode:schedule must render a CronJob, got {r:?}");
        };
        let spec = cj.spec;
        assert_eq!(spec.schedule, "*/5 * * * *");
        assert_eq!(spec.time_zone.as_deref(), Some("UTC"));
        assert_eq!(spec.concurrency_policy.as_deref(), Some("Forbid"));
        // The jobTemplate carries the agent pod.
        let pod = spec.job_template.spec.unwrap().template;
        assert!(has_arg_pair(container_of(&pod), "--mode", "schedule"));
    }

    #[test]
    fn fleet_template_mode_is_coerced_to_reactive() {
        // A fleet whose template carries the DEFAULT `once` mode (or any non-reactive
        // mode) must render a long-lived reactive member — else the Deployment pods
        // exit and CrashLoop. Both regimes go through the coercion.
        for mode in [ScaleMode::Claim, ScaleMode::Shard] {
            let mut f = fleet(mode, Some(2));
            f.spec.template.mode = Mode::Once; // the CrashLoop-inducing default
            let r = render_fleet(&f, &cfg()).unwrap();
            let pod = match &r {
                Rendered::Deployment(d) => d.spec.as_ref().unwrap().template.clone(),
                Rendered::StatefulSet(s) => s.spec.as_ref().unwrap().template.clone(),
                _ => panic!("fleet renders a Deployment or StatefulSet"),
            };
            assert!(
                has_arg_pair(container_of(&pod), "--mode", "reactive"),
                "fleet {mode:?} member must run --mode reactive"
            );
        }
    }

    #[test]
    fn agent_args_render_declared_limits() {
        // The bounding box (spec.limits) must reach agentd; the operator used to
        // drop it silently.
        let mut a = agent(Mode::Reactive);
        a.spec.limits = Some(agent_api::Limits {
            max_tokens: Some(500_000),
            max_depth: Some(3),
            max_steps: Some(40),
            tree_token_budget: Some(2_000_000), // no agentd flag yet → not emitted
        });
        let r = render_agent(&a, &cfg()).unwrap();
        let Rendered::Deployment(dep) = r else {
            unreachable!()
        };
        let c = container_of(&dep.spec.as_ref().unwrap().template).clone();
        assert!(has_arg_pair(&c, "--max-tokens", "500000"));
        assert!(has_arg_pair(&c, "--max-depth", "3"));
        assert!(has_arg_pair(&c, "--max-steps", "40"));
        // tree_token_budget has no agentd flag; it must NOT be passed to an unknown one.
        assert!(!c
            .args
            .as_ref()
            .unwrap()
            .iter()
            .any(|a| a == "--tree-token-budget"));
    }

    #[test]
    fn claim_fleet_renders_a_scaled_object() {
        let f = fleet(ScaleMode::Claim, None);
        let so = render_scaled_object(&f, DEFAULT_SCALER_ADDRESS, DEFAULT_COORDINATION_URL)
            .expect("claim mode produces a ScaledObject");

        assert_eq!(so["apiVersion"], "keda.sh/v1alpha1");
        assert_eq!(so["kind"], "ScaledObject");
        assert_eq!(so["metadata"]["name"], "workers");
        assert_eq!(so["metadata"]["namespace"], "agents");
        // Owns the Deployment of the same name; ownerRef back to the AgentFleet.
        assert_eq!(so["spec"]["scaleTargetRef"]["name"], "workers");
        let owner = &so["metadata"]["ownerReferences"][0];
        assert_eq!(owner["kind"], "AgentFleet");
        assert_eq!(owner["name"], "workers");
        assert_eq!(owner["uid"], "fleet-uid");
        assert_eq!(owner["controller"], true);

        // min defaults to 0 (scale-to-zero); max comes from scaling.max (10 here).
        assert_eq!(so["spec"]["minReplicaCount"], 0);
        assert_eq!(so["spec"]["maxReplicaCount"], 10);

        // External trigger → the scaler, carrying the coordination + threshold knobs.
        let trigger = &so["spec"]["triggers"][0];
        assert_eq!(trigger["type"], "external");
        let md = &trigger["metadata"];
        assert_eq!(md["scalerAddress"], DEFAULT_SCALER_ADDRESS);
        // the fleet helper sets workSource = "queue://jobs", which wins over the
        // operator COORDINATION_URL default.
        assert_eq!(md["coordinationUrl"], "queue://jobs");
        // no scaling.target set → default threshold "5".
        assert_eq!(md["threshold"], "5");
        assert_eq!(md["activationThreshold"], "1");
    }

    #[test]
    fn scaled_object_falls_back_to_default_coordination_url() {
        // No per-fleet workSource → the operator COORDINATION_URL default is used.
        let mut f = fleet(ScaleMode::Claim, None);
        f.spec.work_source = None;
        let so =
            render_scaled_object(&f, DEFAULT_SCALER_ADDRESS, DEFAULT_COORDINATION_URL).unwrap();
        assert_eq!(
            so["spec"]["triggers"][0]["metadata"]["coordinationUrl"],
            DEFAULT_COORDINATION_URL
        );
    }

    #[test]
    fn shard_fleet_renders_no_scaled_object() {
        // Shard mode is a fixed StatefulSet partition count — never KEDA-driven.
        let f = fleet(ScaleMode::Shard, Some(3));
        assert!(
            render_scaled_object(&f, DEFAULT_SCALER_ADDRESS, DEFAULT_COORDINATION_URL).is_none()
        );
    }

    #[test]
    fn scaled_object_honors_target_value_and_work_source() {
        let mut f = fleet(ScaleMode::Claim, None);
        f.spec.scaling.target = Some(agent_api::ScaleTarget {
            signal: "backlog".into(),
            value: "12".into(),
        });
        f.spec.work_source = Some("http://my-coordination.custom.svc/".into());

        let so = render_scaled_object(&f, "scaler:9100", DEFAULT_COORDINATION_URL).unwrap();
        let md = &so["spec"]["triggers"][0]["metadata"];
        // scaling.target.value wins over the default threshold.
        assert_eq!(md["threshold"], "12");
        // the fleet's own workSource wins over the operator coordination default.
        assert_eq!(md["coordinationUrl"], "http://my-coordination.custom.svc/");
        assert_eq!(md["scalerAddress"], "scaler:9100");
    }

    // ── RFC 0022: coordinator ("main agent") rendering ──────────────────────

    fn coord_template() -> AgentSpec {
        AgentSpec {
            mode: Mode::Reactive,
            image: Some("ghcr.io/example/coordinator@sha256:def".into()),
            instruction: Some("decompose and delegate".into()),
            ..Default::default()
        }
    }

    #[test]
    fn shard_fleet_injects_shard_identity() {
        // RFC 0003 §9.1 P3: a shard StatefulSet pod carries `--shard auto/N` (N only,
        // identical across ordinals); the agent derives K from AGENT_POD_NAME.
        let f = fleet(ScaleMode::Shard, Some(4));
        let r = render_fleet(&f, &cfg()).unwrap();
        let Rendered::StatefulSet(sts) = r else {
            panic!("shard mode renders a StatefulSet");
        };
        let pod = sts.spec.unwrap().template;
        assert!(has_arg_pair(container_of(&pod), "--shard", "auto/4"));
    }

    #[test]
    fn claim_fleet_has_no_shard_flag() {
        let f = fleet(ScaleMode::Claim, None);
        let r = render_fleet(&f, &cfg()).unwrap();
        let Rendered::Deployment(dep) = r else {
            unreachable!()
        };
        let c = container_of(&dep.spec.as_ref().unwrap().template);
        assert!(!c.args.as_ref().unwrap().iter().any(|a| a == "--shard"));
    }

    #[test]
    fn coordinatorless_fleet_renders_no_coordinator() {
        let f = fleet(ScaleMode::Claim, None);
        assert!(render_coordinator(&f, &cfg()).is_none());
    }

    #[test]
    fn coordinator_renders_a_named_labeled_deployment() {
        let mut f = fleet(ScaleMode::Claim, None);
        f.spec.coordinator = Some(agent_api::Coordinator {
            template: coord_template(),
            replicas: Some(2),
            distribution: None, // default queue
        });
        let r = render_coordinator(&f, &cfg())
            .expect("coordinator present")
            .unwrap();
        let Rendered::Deployment(dep) = r else {
            panic!("coordinator renders a Deployment, got {r:?}");
        };
        // Named `<fleet>-coordinator`, distinct from the worker Deployment name.
        assert_eq!(dep.metadata.name.as_deref(), Some("workers-coordinator"));
        let spec = dep.spec.as_ref().unwrap();
        assert_eq!(spec.replicas, Some(2));
        // Pod labels carry the fleet-role + fleet labels AND the coordinator's own
        // agent label (so the worker `agent=workers` selector never grabs it).
        let labels = spec
            .template
            .metadata
            .as_ref()
            .unwrap()
            .labels
            .as_ref()
            .unwrap();
        assert_eq!(labels[FLEET_ROLE_LABEL], "coordinator");
        assert_eq!(labels[FLEET_LABEL], "workers");
        assert_eq!(labels["agentctl.dev/agent"], "workers-coordinator");
        // Long-lived: coerced to reactive so it does not CrashLoop under a Deployment.
        assert!(has_arg_pair(
            container_of(&spec.template),
            "--mode",
            "reactive"
        ));
        // Owned by the fleet (GC'd with it).
        let owner = &dep.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!(owner.kind, "AgentFleet");
        assert_eq!(owner.name, "workers");
    }

    #[test]
    fn coordinator_replicas_default_to_one() {
        let mut f = fleet(ScaleMode::Claim, None);
        f.spec.coordinator = Some(agent_api::Coordinator {
            template: coord_template(),
            replicas: None,
            distribution: None,
        });
        let r = render_coordinator(&f, &cfg()).unwrap().unwrap();
        let Rendered::Deployment(dep) = r else {
            unreachable!()
        };
        assert_eq!(
            dep.spec.unwrap().replicas,
            Some(1),
            "singleton main agent by default"
        );
    }

    #[test]
    fn coordinator_queue_distribution_injects_worksource_env() {
        let mut f = fleet(ScaleMode::Claim, None);
        f.spec.work_source = Some("https://coord.svc/mcp".into());
        f.spec.coordinator = Some(agent_api::Coordinator {
            template: coord_template(),
            replicas: None,
            distribution: Some(agent_api::Distribution::Queue),
        });
        let r = render_coordinator(&f, &cfg()).unwrap().unwrap();
        let Rendered::Deployment(dep) = r else {
            unreachable!()
        };
        let c = container_of(&dep.spec.as_ref().unwrap().template);
        let env = c.env.as_ref().unwrap();
        let ws = env
            .iter()
            .find(|e| e.name == "AGENT_FLEET_WORKSOURCE")
            .expect("worksource env");
        assert_eq!(ws.value.as_deref(), Some("https://coord.svc/mcp"));
        // Queue mode does NOT add an --a2a-peer.
        assert!(!c.args.as_ref().unwrap().iter().any(|a| a == "--a2a-peer"));
    }

    #[test]
    fn coordinator_a2a_distribution_injects_worker_peer() {
        let mut f = fleet(ScaleMode::Claim, None);
        f.spec.coordinator = Some(agent_api::Coordinator {
            template: coord_template(),
            replicas: None,
            distribution: Some(agent_api::Distribution::A2a),
        });
        let mut c = cfg();
        c.gateway_url = "http://gw.svc:8080".into();
        let r = render_coordinator(&f, &c).unwrap().unwrap();
        let Rendered::Deployment(dep) = r else {
            unreachable!()
        };
        let container = container_of(&dep.spec.as_ref().unwrap().template);
        // `--a2a-peer worker=<gateway>/fleets/<ns>/<fleet>`.
        assert!(has_arg_pair(
            container,
            "--a2a-peer",
            "worker=http://gw.svc:8080/fleets/agents/workers"
        ));
    }
}
