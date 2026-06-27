//! Pure workload rendering: an [`Agent`] → the Kubernetes workload that runs it.
//!
//! This is the deterministic, side-effect-free core the reconcile loop (RFC
//! 0006) calls. Keeping it pure makes the mode→workload mapping (RFC 0003 §5)
//! and the substrate wiring (RFC 0002) unit-testable without a cluster.
//!
//! v1 implements the **stock-unix** substrate (the PRIMARY/dev tier, RFC 0002):
//! the agent serves its management socket on a per-pod hostPath subdir, reached
//! by the node-agent. The Kata-hybrid tier reuses this same shape with a
//! different volume source (RFC 0002 §4/§6.2) and is added later.

use std::collections::BTreeMap;

use agent_api::{Agent, Mode, Substrate};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvVar, EnvVarSource, HostPathVolumeSource, ObjectFieldSelector, PodSpec,
    PodTemplateSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};

/// API group/version these resources are owned by (agent-api `GROUP`).
const API_VERSION: &str = "agents.x-k8s.io/v1alpha1";
const KIND: &str = "Agent";

/// The node-agent-owned hostPath root for the stock-unix substrate (RFC 0002
/// §6.1). Each pod mounts only its own `<pod-uid>/` subdir.
const SOCKET_HOSTPATH_ROOT: &str = "/run/agentctl/sockets";
/// In-pod mount point where the agent binds its management socket.
const SOCKET_MOUNT: &str = "/run/agentd";
const SOCKET_VOLUME: &str = "agentctl-sockets";

/// What the renderer produced. Boxed to keep the enum small (clippy).
#[derive(Debug, Clone, PartialEq)]
pub enum Rendered {
    /// `once` mode.
    Job(Box<Job>),
    /// `loop` / `reactive` mode.
    Deployment(Box<Deployment>),
}

/// Why rendering could not proceed (caller surfaces these as a `Validated=False`
/// condition rather than crashing the reconcile loop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The `Agent` has no `.metadata.name`.
    MissingName,
    /// No image to run: a classless `Agent` must set `.spec.image` (a classRef
    /// is resolved upstream, before rendering — RFC 0004).
    MissingImage,
    /// A substrate this renderer does not yet implement.
    UnsupportedSubstrate(Substrate),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::MissingName => write!(f, "Agent has no metadata.name"),
            RenderError::MissingImage => {
                write!(f, "Agent.spec.image is required (resolve classRef first)")
            }
            RenderError::UnsupportedSubstrate(s) => {
                write!(f, "substrate {s:?} not implemented by this renderer")
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// Render an `Agent` to its workload.
pub fn render_agent(agent: &Agent) -> Result<Rendered, RenderError> {
    let name = agent.metadata.name.clone().ok_or(RenderError::MissingName)?;
    let image = agent.spec.image.clone().ok_or(RenderError::MissingImage)?;

    let substrate = agent.spec.substrate.unwrap_or(Substrate::StockUnix);
    if substrate != Substrate::StockUnix {
        // Kata-hybrid swaps the volume source only; sidecar adds a sibling
        // container. Both reuse the rest of this shape (RFC 0002) — not yet wired.
        return Err(RenderError::UnsupportedSubstrate(substrate));
    }

    let labels = managed_labels(&name);
    let owner = owner_ref(agent)?;
    let meta = ObjectMeta {
        name: Some(name.clone()),
        namespace: agent.metadata.namespace.clone(),
        labels: Some(labels.clone()),
        owner_references: Some(vec![owner]),
        ..Default::default()
    };

    let pod = pod_template(agent, &image, &labels);

    match agent.spec.mode {
        Mode::Once | Mode::Schedule => {
            // `schedule` renders a CronJob whose jobTemplate is this Job; for v1
            // the renderer emits the Job and the CronJob wrap is layered later.
            let job = Job {
                metadata: meta,
                spec: Some(JobSpec {
                    template: pod,
                    backoff_limit: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            };
            Ok(Rendered::Job(Box::new(job)))
        }
        Mode::Loop | Mode::Reactive => {
            let dep = Deployment {
                metadata: meta,
                spec: Some(DeploymentSpec {
                    // replicas intentionally 1 for a singleton Agent. An
                    // AgentFleet omits replicas entirely (KEDA owns it, RFC 0011).
                    replicas: Some(1),
                    selector: LabelSelector {
                        match_labels: Some(labels.clone()),
                        ..Default::default()
                    },
                    template: pod,
                    ..Default::default()
                }),
                ..Default::default()
            };
            Ok(Rendered::Deployment(Box::new(dep)))
        }
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

fn owner_ref(agent: &Agent) -> Result<OwnerReference, RenderError> {
    let name = agent.metadata.name.clone().ok_or(RenderError::MissingName)?;
    Ok(OwnerReference {
        api_version: API_VERSION.to_string(),
        kind: KIND.to_string(),
        name,
        // uid may be empty before the apiserver assigns it; that's fine for a
        // dry-run render and is populated on the live object.
        uid: agent.metadata.uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    })
}

fn pod_template(
    agent: &Agent,
    image: &str,
    labels: &BTreeMap<String, String>,
) -> PodTemplateSpec {
    let restart_policy = match agent.spec.mode {
        Mode::Once | Mode::Schedule => Some("Never".to_string()),
        // Deployments require Always.
        Mode::Loop | Mode::Reactive => None,
    };

    let container = Container {
        name: "agent".to_string(),
        image: Some(image.to_string()),
        args: Some(agent_args(agent)),
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
fn agent_args(agent: &Agent) -> Vec<String> {
    let mut args = vec!["--mode".to_string(), mode_str(agent.spec.mode).to_string()];
    if let Some(instruction) = &agent.spec.instruction {
        args.push("--instruction".to_string());
        args.push(instruction.clone());
    }
    for sub in &agent.spec.subscribe {
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
    use agent_api::{AgentSpec, DesiredSurfaces};

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
        // owner ref wired for GC
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
        // selector matches the managed labels
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
        // subscriptions flow into args
        assert!(c.args.as_ref().unwrap().windows(2).any(|w| w
            == ["--subscribe".to_string(), "file:///data/inbox".to_string()]));
    }

    #[test]
    fn stock_unix_substrate_wiring() {
        let r = render_agent(&agent(Mode::Once)).unwrap();
        let Rendered::Job(job) = r else { unreachable!() };
        let pod = job.spec.unwrap().template;
        let podspec = pod.spec.as_ref().unwrap();

        // hostPath socket volume present
        let vol = &podspec.volumes.as_ref().unwrap()[0];
        assert_eq!(vol.name, "agentctl-sockets");
        assert_eq!(
            vol.host_path.as_ref().unwrap().path,
            "/run/agentctl/sockets"
        );

        let c = container_of(&pod);
        // per-pod subdir via subPathExpr (no UID known at render time)
        let mount = &c.volume_mounts.as_ref().unwrap()[0];
        assert_eq!(mount.sub_path_expr.as_deref(), Some("$(AGENTD_POD_UID)"));
        assert_eq!(mount.mount_path, "/run/agentd");

        // downward-API identity env + the management bind instruction
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
}
