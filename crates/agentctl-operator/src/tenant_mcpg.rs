// SPDX-License-Identifier: BUSL-1.1
//! # Tenant mcpg gateway (RFC 0026/0034 capability plane, P5-1)
//!
//! Every managed org namespace gets ITS OWN mcpg gateway: a governance proxy
//! federating the org's registered `MCPService` entries — imported tools are
//! re-served under a per-service prefix, narrowed by the entry's allow/exclude
//! lists, audited, and reachable in-cluster at `agentctl-mcpg.<ns>` — the
//! governed alternative to direct dials, per org, isolated by namespace.
//!
//! Deliberate posture (validated against the blessed 0.1.0-beta.24 source):
//! * **Proxy-only, zero plugins** — federation is in-gateway; the OCI plugin
//!   lane stays out of the boot path entirely.
//! * **Header-asserted callers** (`gateway.server.trust_subject_header`) +
//!   NetworkPolicy perimeter — the plugin-free trust tier; JWKS-verified
//!   per-agent tokens are the P5-2 upgrade.
//! * **Hot catalog** — `gateway.config_watch` (SHA-256 polling, built for
//!   ConfigMap symlink swaps): the operator re-renders the ConfigMap when the
//!   org's registry changes and the gateway reloads WITHOUT a restart
//!   (unchanged federations keep their satellites).
//! * The platform `control` entry is NEVER federated: the control surface is
//!   AAuth-verified first-party — re-proxying it would launder the identity
//!   the whole P4 plane hangs on.

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HTTPGetAction, PodSpec, PodTemplateSpec, Probe, SeccompProfile,
    SecretKeySelector, SecurityContext, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, Patch, PatchParams};
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::controller::{Ctx, Error};

const FIELD_MANAGER: &str = "agentctl-operator";
/// The in-namespace name every tenant gateway object carries.
pub const TENANT_GATEWAY_NAME: &str = "agentctl-mcpg";
/// The registry entry name the platform reserves for the control MCP.
const CONTROL_ENTRY: &str = "control";

/// Operator wiring: the blessed mcpg image (digest-pinned by the chart).
/// Absent ⇒ the tenant-gateway plane is off.
#[derive(Clone, Debug, Default)]
pub struct TenantMcpgConfig {
    pub image: Option<String>,
}

impl TenantMcpgConfig {
    pub fn from_env() -> Self {
        TenantMcpgConfig {
            image: std::env::var("AGENTCTL_TENANT_MCPG_IMAGE")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        }
    }
    pub fn enabled(&self) -> bool {
        self.image.is_some()
    }
}

/// A registry entry eligible for federation, reduced to what the config
/// needs. Pure input to [`render_config`].
#[derive(Clone, Debug)]
pub struct FederationEntry {
    pub name: String,
    pub endpoint: String,
    pub allow: Vec<String>,
    pub exclude: Vec<String>,
    /// The mounted env var carrying the upstream service token, when the
    /// entry declares `auth.tokenSecretRef` (mode `service`).
    pub token_env: Option<String>,
}

/// The env-var name a federation's service token rides in on.
pub fn federation_token_env(entry: &str) -> String {
    format!(
        "FEDERATION_{}_TOKEN",
        entry.to_uppercase().replace(['-', '.'], "_")
    )
}

/// mcpg's federation filter grammar is MINIMAL: exact, `*`, or one trailing
/// `prefix*`. Registry allow patterns like `state.*` already fit; anything
/// with an INNER wildcard cannot be expressed — dropped with a warning
/// (narrowing stays sound: dropping an allow pattern only hides tools).
fn filter_patterns(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter(|p| {
            let inner_star = p[..p.len().saturating_sub(1)].contains('*');
            if inner_star && p.as_str() != "*" {
                tracing::warn!(pattern = %p, "allow pattern has an inner wildcard mcpg's filter grammar cannot express; omitted from the federation filter");
            }
            !inner_star || p.as_str() == "*"
        })
        .cloned()
        .collect()
}

/// Render the tenant gateway's whole config document. Pure.
pub fn render_config(entries: &[FederationEntry]) -> String {
    let federations: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut upstream = json!({
                "url": e.endpoint,
                "transport": "streamable_http",
                "protocol_version": "auto",
                "upstream_safety": {
                    // In-cluster Services: private IPs, plaintext inside the
                    // NetworkPolicy'd mesh.
                    "allow_private_backends": true,
                    "allow_insecure_http": e.endpoint.starts_with("http://"),
                },
            });
            if let Some(env) = &e.token_env {
                upstream["auth"] = json!({
                    "mode": "service_token",
                    "token": format!("${{env.{env}}}"),
                });
            }
            let mut fed = json!({
                "name": e.name,
                "governance": { "minimum_trust": "header_asserted" },
                "upstream": upstream,
                "import": { "tools": true },
                // The prefix is REQUIRED with >1 federation and is the
                // namespace the org's agents see: `<entry>.<tool>`.
                "naming": { "tool_prefix": format!("{}.", e.name) },
                "cache": { "capability_ttl_secs": 300 },
            });
            let include = filter_patterns(&e.allow);
            let exclude = filter_patterns(&e.exclude);
            if !include.is_empty() || !exclude.is_empty() {
                let mut filter = serde_json::Map::new();
                if !include.is_empty() {
                    filter.insert("include_tools".into(), json!(include));
                }
                if !exclude.is_empty() {
                    filter.insert("exclude_tools".into(), json!(exclude));
                }
                fed["filter"] = Value::Object(filter);
            }
            fed
        })
        .collect();

    let doc = json!({
        "gateway": {
            "server": {
                "bind_address": "0.0.0.0:8787",
                "mcp_path": "/mcp",
                "health_path": "/health",
                // Header-asserted callers (x-mcpg-subject-id) — safe ONLY
                // behind the NetworkPolicy perimeter; P5-2 upgrades this to
                // JWKS-verified per-agent tokens.
                "trust_subject_header": true,
            },
            "config_watch": { "enabled": true, "poll_interval_ms": 5000 },
        },
        "cluster": { "kind": "single_node" },
        "governance": {
            "policy": { "tool_access": { "default_minimum_trust": "header_asserted" } },
            "audit": {
                "enabled": true,
                "required": true,
                "sinks": [{
                    "kind": "dev.mcpg.builtin.audit.local-file",
                    "config": { "path": "/var/log/mcpg/audit.log" },
                }],
            },
        },
        "mcp": { "federations": federations },
        "observability": {
            "logs": { "level": "info", "sinks": [{ "kind": "stderr", "config": { "format": "json" } }] },
        },
    });
    // YAML for operator legibility (`kubectl get cm -o yaml` reads well);
    // mcpg parses YAML as a superset of this JSON-shaped tree.
    serde_yaml::to_string(&doc).expect("static config tree serializes")
}

fn labels(org: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/name".to_string(),
            TENANT_GATEWAY_NAME.to_string(),
        ),
        (
            "app.kubernetes.io/managed-by".to_string(),
            "agentctl-operator".to_string(),
        ),
        (crate::org::ORG_LABEL.to_string(), org.to_string()),
    ])
}

fn meta(ns: &str, org: &str, owner: &OwnerReference) -> ObjectMeta {
    ObjectMeta {
        name: Some(TENANT_GATEWAY_NAME.to_string()),
        namespace: Some(ns.to_string()),
        labels: Some(labels(org)),
        owner_references: Some(vec![owner.clone()]),
        ..Default::default()
    }
}

/// The tenant gateway Deployment. Pure.
pub fn desired_deployment(
    ns: &str,
    org: &str,
    image: &str,
    entries: &[FederationEntry],
    token_refs: &[(String, agent_api::SecretKeyRef)],
    owner: &OwnerReference,
) -> Deployment {
    let mut env: Vec<EnvVar> = Vec::new();
    for (entry, r) in token_refs {
        env.push(EnvVar {
            name: federation_token_env(entry),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: r.name.clone(),
                    key: r.key.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    // The config hash rides the pod template so a FULL image/em-secret change
    // still rolls; ordinary catalog edits hot-reload via config_watch without
    // touching the pods.
    let config_hash = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        for e in entries {
            e.name.hash(&mut h);
            e.token_env.hash(&mut h);
        }
        format!("{:x}", h.finish())
    };
    let lbls = labels(org);
    Deployment {
        metadata: meta(ns, org, owner),
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    "app.kubernetes.io/name".to_string(),
                    TENANT_GATEWAY_NAME.to_string(),
                )])),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(lbls),
                    annotations: Some(BTreeMap::from([(
                        "agentctl.dev/federation-env-hash".to_string(),
                        config_hash,
                    )])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
                        run_as_non_root: Some(true),
                        // The blessed image's `mcpg` user, numerically (the
                        // kubelet refuses symbolic users).
                        run_as_user: Some(10001),
                        run_as_group: Some(999),
                        fs_group: Some(999),
                        seccomp_profile: Some(SeccompProfile {
                            type_: "RuntimeDefault".to_string(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    containers: vec![Container {
                        name: "mcpg".to_string(),
                        image: Some(image.to_string()),
                        image_pull_policy: Some("IfNotPresent".to_string()),
                        env: Some(env),
                        ports: Some(vec![ContainerPort {
                            name: Some("http".to_string()),
                            container_port: 8787,
                            ..Default::default()
                        }]),
                        readiness_probe: Some(probe(5, 5)),
                        liveness_probe: Some(probe(20, 10)),
                        security_context: Some(SecurityContext {
                            allow_privilege_escalation: Some(false),
                            read_only_root_filesystem: Some(true),
                            capabilities: Some(k8s_openapi::api::core::v1::Capabilities {
                                drop: Some(vec!["ALL".to_string()]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        volume_mounts: Some(vec![
                            mount("config", "/etc/mcpg", true),
                            mount("audit", "/var/log/mcpg", false),
                            mount("tmp", "/tmp", false),
                        ]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        Volume {
                            name: "config".to_string(),
                            config_map: Some(ConfigMapVolumeSource {
                                name: TENANT_GATEWAY_NAME.to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        empty_dir("audit"),
                        empty_dir("tmp"),
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn probe(initial: i32, period: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/health".to_string()),
            port: IntOrString::String("http".to_string()),
            ..Default::default()
        }),
        initial_delay_seconds: Some(initial),
        period_seconds: Some(period),
        ..Default::default()
    }
}

fn mount(name: &str, path: &str, ro: bool) -> VolumeMount {
    VolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        read_only: Some(ro),
        ..Default::default()
    }
}

fn empty_dir(name: &str) -> Volume {
    Volume {
        name: name.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    }
}

/// Reduce the org's registry to federation inputs: `kind: mcp` entries with
/// an endpoint, the platform `control` entry excluded.
pub fn eligible_entries(
    services: &[agent_api::v1alpha2::MCPService],
) -> (Vec<FederationEntry>, Vec<(String, agent_api::SecretKeyRef)>) {
    let mut entries = Vec::new();
    let mut token_refs = Vec::new();
    for svc in services {
        let name = match &svc.metadata.name {
            Some(n) if n != CONTROL_ENTRY => n.clone(),
            _ => continue,
        };
        let Some(endpoint) = svc.spec.endpoint.clone() else {
            continue;
        };
        let token_env = svc.spec.auth.as_ref().and_then(|a| {
            a.token_secret_ref.as_ref().map(|r| {
                token_refs.push((name.clone(), r.clone()));
                federation_token_env(&name)
            })
        });
        entries.push(FederationEntry {
            name,
            endpoint,
            allow: svc.spec.allow.clone(),
            exclude: svc.spec.exclude.clone(),
            token_env,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    (entries, token_refs)
}

/// Ensure the tenant gateway trio in a managed namespace (SSA; owner = the
/// Organization, so teardown cascades). No-op when the plane is off.
pub async fn ensure_tenant_gateway(
    ctx: &Ctx,
    ns: &str,
    org: &str,
    owner: &OwnerReference,
) -> Result<(), Error> {
    let Some(image) = ctx.tenant_mcpg.image.clone() else {
        return Ok(());
    };
    let services: Api<agent_api::v1alpha2::MCPService> = Api::namespaced(ctx.client.clone(), ns);
    let list = services.list(&Default::default()).await?;
    let (entries, token_refs) = eligible_entries(&list.items);

    let pp = PatchParams::apply(FIELD_MANAGER).force();
    let cm = ConfigMap {
        metadata: meta(ns, org, owner),
        data: Some(BTreeMap::from([(
            "config.yaml".to_string(),
            render_config(&entries),
        )])),
        ..Default::default()
    };
    let cms: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), ns);
    cms.patch(TENANT_GATEWAY_NAME, &pp, &Patch::Apply(&cm))
        .await?;

    let deploy = desired_deployment(ns, org, &image, &entries, &token_refs, owner);
    let deploys: Api<Deployment> = Api::namespaced(ctx.client.clone(), ns);
    deploys
        .patch(TENANT_GATEWAY_NAME, &pp, &Patch::Apply(&deploy))
        .await?;

    let svc = Service {
        metadata: meta(ns, org, owner),
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(
                "app.kubernetes.io/name".to_string(),
                TENANT_GATEWAY_NAME.to_string(),
            )])),
            ports: Some(vec![ServicePort {
                name: Some("http".to_string()),
                port: 8787,
                target_port: Some(IntOrString::String("http".to_string())),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let svcs: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    svcs.patch(TENANT_GATEWAY_NAME, &pp, &Patch::Apply(&svc))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, allow: &[&str]) -> FederationEntry {
        FederationEntry {
            name: name.into(),
            endpoint: format!("http://{name}.svc:8080/mcp"),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            exclude: Vec::new(),
            token_env: None,
        }
    }

    /// The rendered document is the validated proxy-only posture: zero
    /// plugins, header-asserted trust + subject header, hot reload, audit
    /// fail-closed to a writable path, per-entry prefix + narrowed filter,
    /// private/insecure upstream opt-ins.
    #[test]
    fn config_is_proxy_only_and_governed() {
        let yaml = render_config(&[
            entry("state", &["state.*"]),
            FederationEntry {
                token_env: Some(federation_token_env("crm")),
                ..entry("crm", &["search_*"])
            },
        ]);
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(
            doc.get("plugins").is_none(),
            "the plugin lane must stay out"
        );
        assert_eq!(
            doc["gateway"]["server"]["trust_subject_header"],
            serde_yaml::Value::Bool(true)
        );
        assert_eq!(
            doc["gateway"]["config_watch"]["enabled"],
            serde_yaml::Value::Bool(true)
        );
        assert_eq!(
            doc["governance"]["audit"]["sinks"][0]["config"]["path"],
            serde_yaml::Value::String("/var/log/mcpg/audit.log".into())
        );
        let feds = doc["mcp"]["federations"].as_sequence().unwrap();
        assert_eq!(feds.len(), 2);
        // Sorted inputs; each entry namespaced by its own prefix.
        assert_eq!(feds[0]["naming"]["tool_prefix"], "state.");
        assert_eq!(feds[0]["filter"]["include_tools"][0], "state.*");
        assert_eq!(
            feds[0]["upstream"]["upstream_safety"]["allow_insecure_http"],
            serde_yaml::Value::Bool(true)
        );
        // The tokened entry rides the env-templated service token.
        assert_eq!(
            feds[1]["upstream"]["auth"]["token"],
            "${env.FEDERATION_CRM_TOKEN}"
        );
    }

    /// The platform `control` entry NEVER federates (identity laundering);
    /// endpointless entries are skipped; inner-wildcard patterns drop.
    #[test]
    fn control_is_excluded_and_grammar_is_respected() {
        use agent_api::v1alpha2 as v2;
        let mk = |name: &str, endpoint: Option<&str>| {
            let mut s = v2::MCPService::new(
                name,
                v2::MCPServiceSpec {
                    endpoint: endpoint.map(str::to_string),
                    allow: vec!["a.*".into(), "x*y".into()],
                    ..Default::default()
                },
            );
            s.metadata.namespace = Some("org-acme".into());
            s
        };
        let (entries, refs) = eligible_entries(&[
            mk("control", Some("https://control:8443/mcp")),
            mk("tools", Some("http://tools:8080/mcp")),
            mk("draft", None),
        ]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "tools");
        assert!(refs.is_empty());
        assert_eq!(filter_patterns(&entries[0].allow), vec!["a.*".to_string()]);
    }
}
