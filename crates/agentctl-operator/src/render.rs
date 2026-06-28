//! Pure workload rendering: an [`Agent`]/[`AgentFleet`] → the Kubernetes
//! workload that runs it.
//!
//! This is the deterministic, side-effect-free core the reconcile loop (RFC
//! 0006) calls. Keeping it pure makes the mode→workload mapping (RFC 0003 §5),
//! the scaling regime (RFC 0011), and the substrate wiring (RFC 0002) all
//! unit-testable without a cluster.
//!
//! v1 implements the **stock-unix** substrate (the PRIMARY/dev tier, RFC 0002):
//! the agent serves its management socket on a per-pod hostPath subdir, reached
//! by the node-agent. The Kata-hybrid tier reuses this same shape with a
//! different volume source (RFC 0002 §4/§6.2) and is added later.

use std::collections::BTreeMap;

use agent_api::{Agent, AgentFleet, AgentSpec, Mode, ScaleMode, Substrate};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, StatefulSet, StatefulSetSpec};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvVar, EnvVarSource, HostPathVolumeSource, ObjectFieldSelector, PodSpec,
    PodTemplateSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};

/// API group/version these resources are owned by (agent-api `GROUP`).
const API_VERSION: &str = "agents.x-k8s.io/v1alpha1";

/// The node-agent-owned hostPath root for the stock-unix substrate (RFC 0002
/// §6.1). Each pod mounts only its own `<pod-uid>/` subdir.
const SOCKET_HOSTPATH_ROOT: &str = "/run/agentctl/sockets";
/// In-pod mount point where the agent binds its management socket.
const SOCKET_MOUNT: &str = "/run/agentd";
const SOCKET_VOLUME: &str = "agentctl-sockets";

/// What the renderer produced. Boxed to keep the enum small (clippy).
#[derive(Debug, Clone, PartialEq)]
pub enum Rendered {
    /// `once` mode → a batch Job.
    Job(Box<Job>),
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
pub fn render_agent(agent: &Agent) -> Result<Rendered, RenderError> {
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
    let pod = pod_template(&agent.spec, &image, &labels);

    match agent.spec.mode {
        Mode::Once | Mode::Schedule => {
            // `schedule` renders a CronJob whose jobTemplate is this Job; for v1
            // the renderer emits the Job and the CronJob wrap is layered later.
            Ok(Rendered::Job(Box::new(Job {
                metadata: meta,
                spec: Some(JobSpec {
                    template: pod,
                    backoff_limit: Some(0),
                    ..Default::default()
                }),
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
pub fn render_fleet(fleet: &AgentFleet) -> Result<Rendered, RenderError> {
    let name = fleet
        .metadata
        .name
        .clone()
        .ok_or(RenderError::MissingName)?;
    let spec = &fleet.spec.template;
    let image = spec.image.clone().ok_or(RenderError::MissingImage)?;
    require_stock_unix(spec.substrate)?;

    let labels = managed_labels(&name);
    let meta = owned_meta(
        &name,
        fleet.metadata.namespace.clone(),
        &labels,
        owner_ref("AgentFleet", &name, uid_of(&fleet.metadata.uid)),
    );
    let pod = pod_template(spec, &image, &labels);

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
) -> PodTemplateSpec {
    let restart_policy = match spec.mode {
        Mode::Once | Mode::Schedule => Some("Never".to_string()),
        // Deployments/StatefulSets require Always.
        Mode::Loop | Mode::Reactive => None,
    };

    let container = Container {
        name: "agent".to_string(),
        image: Some(image.to_string()),
        args: Some(agent_args(spec)),
        env: Some(downward_env()),
        volume_mounts: Some(vec![VolumeMount {
            name: SOCKET_VOLUME.to_string(),
            mount_path: SOCKET_MOUNT.to_string(),
            // Per RFC 0002 §6.1: the per-pod subdir is selected by the pod UID
            // via subPathExpr, so the path is unique WITHOUT the operator
            // knowing the UID at render time.
            sub_path_expr: Some("$(AGENTD_POD_UID)".to_string()),
            ..Default::default()
        }]),
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
            volumes: Some(vec![Volume {
                name: SOCKET_VOLUME.to_string(),
                host_path: Some(HostPathVolumeSource {
                    path: SOCKET_HOSTPATH_ROOT.to_string(),
                    type_: Some("DirectoryOrCreate".to_string()),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
    }
}

/// The downward-API instance-identity env (contract `env-convention`, agentd RFC
/// 0014 §6.4). Emitted with the reference-alias spelling (`AGENTD_*`) the
/// reference agent reads today; moves to the neutral `AGENT_*` set at the
/// de-branding GA cutover (contract `README` de-branding map).
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
        field("AGENTD_POD_NAME", "metadata.name"),
        field("AGENTD_POD_UID", "metadata.uid"),
        field("AGENTD_POD_NAMESPACE", "metadata.namespace"),
        field("AGENTD_NODE_NAME", "spec.nodeName"),
        // The management bind-address instruction (RFC 0002 §6.1): the agent
        // serves its self-MCP management profile on the per-pod hostPath socket.
        EnvVar {
            name: "AGENTD_SERVE_MCP".to_string(),
            value: Some(format!("unix:{SOCKET_MOUNT}/mgmt.sock")),
            ..Default::default()
        },
    ]
}

/// Container args derived from the spec (mode + instruction + subscriptions).
/// A later step renders the full config via a ConfigMap (RFC 0017); args keep
/// the v1 render self-contained and testable.
fn agent_args(spec: &AgentSpec) -> Vec<String> {
    let mut args = vec!["--mode".to_string(), mode_str(spec.mode).to_string()];
    if let Some(instruction) = &spec.instruction {
        args.push("--instruction".to_string());
        args.push(instruction.clone());
    }
    for sub in &spec.subscribe {
        args.push("--subscribe".to_string());
        args.push(sub.clone());
    }
    args
}

fn mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Once => "once",
        Mode::Loop => "loop",
        Mode::Reactive => "reactive",
        Mode::Schedule => "schedule",
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
            },
        );
        f.metadata.namespace = Some("agents".into());
        f.metadata.uid = Some("fleet-uid".into());
        f
    }

    fn container_of(pod: &PodTemplateSpec) -> &Container {
        &pod.spec.as_ref().unwrap().containers[0]
    }

    #[test]
    fn once_renders_a_job() {
        let r = render_agent(&agent(Mode::Once)).unwrap();
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
        let r = render_agent(&a).unwrap();
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
        assert!(c
            .args
            .as_ref()
            .unwrap()
            .windows(2)
            .any(|w| w == ["--subscribe".to_string(), "file:///data/inbox".to_string()]));
    }

    #[test]
    fn stock_unix_substrate_wiring() {
        let r = render_agent(&agent(Mode::Once)).unwrap();
        let Rendered::Job(job) = r else {
            unreachable!()
        };
        let pod = job.spec.unwrap().template;
        let podspec = pod.spec.as_ref().unwrap();

        let vol = &podspec.volumes.as_ref().unwrap()[0];
        assert_eq!(vol.name, "agentctl-sockets");
        assert_eq!(
            vol.host_path.as_ref().unwrap().path,
            "/run/agentctl/sockets"
        );

        let c = container_of(&pod);
        let mount = &c.volume_mounts.as_ref().unwrap()[0];
        assert_eq!(mount.sub_path_expr.as_deref(), Some("$(AGENTD_POD_UID)"));
        assert_eq!(mount.mount_path, "/run/agentd");

        let env = c.env.as_ref().unwrap();
        let uid = env.iter().find(|e| e.name == "AGENTD_POD_UID").unwrap();
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
        let serve = env.iter().find(|e| e.name == "AGENTD_SERVE_MCP").unwrap();
        assert_eq!(serve.value.as_deref(), Some("unix:/run/agentd/mgmt.sock"));
    }

    #[test]
    fn missing_image_is_an_error() {
        let mut a = agent(Mode::Once);
        a.spec.image = None;
        assert_eq!(render_agent(&a), Err(RenderError::MissingImage));
    }

    #[test]
    fn non_stock_substrate_not_yet_supported() {
        let mut a = agent(Mode::Once);
        a.spec.substrate = Some(Substrate::KataHybrid);
        assert_eq!(
            render_agent(&a),
            Err(RenderError::UnsupportedSubstrate(Substrate::KataHybrid))
        );
    }

    #[test]
    fn claim_fleet_renders_deployment_with_replicas_omitted() {
        let r = render_fleet(&fleet(ScaleMode::Claim, None)).unwrap();
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
        let r = render_fleet(&fleet(ScaleMode::Shard, Some(3))).unwrap();
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
            render_fleet(&fleet(ScaleMode::Shard, None)),
            Err(RenderError::MissingShards)
        );
    }
}
