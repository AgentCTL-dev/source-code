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
    /// Digest-pinned OCI ref of mcpg's `credential-oauth-token-exchange`
    /// plugin (`AGENTCTL_MCPG_EXCHANGE_PLUGIN`). Absent ⇒ `auth.mode: obo`
    /// registry entries are DROPPED with a warning (fail closed; a federation
    /// without its per-user credential must not dial the upstream bare).
    pub exchange_plugin_oci: Option<String>,
    /// mcpg license posture for the BUSL plugin (`AGENTCTL_MCPG_NON_PRODUCTION`):
    /// the gateway's license gate refuses the plugin on the community tier
    /// without `license.non_production_use` (e2e/dev) or an entitling token
    /// (production — a commercial mcpg conversation, outside the chart).
    pub non_production_license: bool,
}

impl TenantMcpgConfig {
    pub fn from_env() -> Self {
        TenantMcpgConfig {
            image: std::env::var("AGENTCTL_TENANT_MCPG_IMAGE")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            exchange_plugin_oci: std::env::var("AGENTCTL_MCPG_EXCHANGE_PLUGIN")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            non_production_license: std::env::var("AGENTCTL_MCPG_NON_PRODUCTION")
                .is_ok_and(|v| v.trim() == "true"),
        }
    }
    pub fn enabled(&self) -> bool {
        self.image.is_some()
    }
}

/// Everything the OBO (per-user credential injection) lane needs, resolved by
/// the caller. `None` anywhere upstream ⇒ obo entries drop, proxy plane keeps
/// serving.
#[derive(Clone, Debug)]
pub struct OboWiring {
    /// Digest-pinned plugin OCI ref.
    pub plugin_oci: String,
    /// Identity's RFC 8693 endpoint — the STS trust anchor. Deliberately NOT
    /// per-federation (mcpg ignores a token URL in credential_config).
    pub exchange_url: String,
    /// `license.non_production_use` (the BUSL plugin's e2e/dev entitlement).
    pub non_production: bool,
}

/// The plugin id + the provider key the `cred://` URIs reference.
const EXCHANGE_PLUGIN_ID: &str = "dev.mcpg.credential.oauth-token-exchange";
const EXCHANGE_PROVIDER: &str = "agentctl";

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
    /// `auth.mode: obo` (P5-3): the audience `/v1/exchange` resolves the
    /// custody connection by — the per-user credential injected upstream.
    /// Defaults to the entry name (connection provider = registry entry).
    pub obo_audience: Option<String>,
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

/// The verified-tier caller-auth facts (P5-2): the identity provider's
/// published JWKS (inline — no fetch at gateway runtime), the issuer agents'
/// tokens carry, and THIS org's audience. `None` ⇒ the header-asserted tier
/// (bootstrap / identity unreachable).
#[derive(Clone, Debug)]
pub struct VerifiedTier {
    pub keys_json: String,
    pub issuer: String,
    pub audience: String,
}

/// The audience a tenant gateway binds its callers' tokens to.
pub fn gateway_audience(ns: &str) -> String {
    format!("mcpg:{ns}")
}

/// Render the tenant gateway's whole config document. Pure.
///
/// OBO entries (P5-3) render as `oauth_impersonation` federations: the
/// gateway hands the VERIFIED caller's bearer to the exchange plugin, which
/// redeems it at identity's `/v1/exchange` (RFC 8693) and injects the minted
/// per-user token as the upstream `Authorization`. They require BOTH the
/// wiring (plugin + exchange URL + license posture) and the verified tier —
/// impersonation from a header-asserted caller is refused by mcpg's engine,
/// so a downgraded gateway DROPS those entries rather than dialing bare.
pub fn render_config(
    entries: &[FederationEntry],
    tier: Option<&VerifiedTier>,
    obo: Option<&OboWiring>,
) -> String {
    let obo_active = obo.is_some() && tier.is_some();
    let mut any_obo = false;
    let federations: Vec<Value> = entries
        .iter()
        .filter(|e| {
            if e.obo_audience.is_some() && !obo_active {
                tracing::warn!(
                    entry = %e.name,
                    wired = obo.is_some(),
                    verified = tier.is_some(),
                    "obo entry dropped: needs the exchange plugin wiring AND the verified caller tier (impersonation never dials the upstream bare)"
                );
                return false;
            }
            true
        })
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
            if let Some(aud) = &e.obo_audience {
                any_obo = true;
                upstream["auth"] = json!({
                    "mode": "oauth_impersonation",
                    "credential": format!("cred://{EXCHANGE_PLUGIN_ID}/{EXCHANGE_PROVIDER}"),
                    // Per-federation overrides are audience/resource ONLY;
                    // the STS endpoint stays on the provider (trust anchor).
                    "credential_config": { "audience": aud },
                });
            }
            let trust = if tier.is_some() {
                "verified"
            } else {
                "header_asserted"
            };
            let mut fed = json!({
                "name": e.name,
                "governance": { "minimum_trust": trust },
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

    let floor = if tier.is_some() {
        "verified"
    } else {
        "header_asserted"
    };
    let mut doc = json!({
        "gateway": {
            "server": {
                "bind_address": "0.0.0.0:8787",
                "mcp_path": "/mcp",
                "health_path": "/health",
                // The header tier exists ONLY while the verified tier is
                // unavailable (bootstrap / identity blip): behind the
                // NetworkPolicy perimeter, callers self-assert; with jwks
                // active the header is ignored and unsigned callers refuse.
                "trust_subject_header": tier.is_none(),
            },
            "config_watch": { "enabled": true, "poll_interval_ms": 5000 },
        },
        "cluster": { "kind": "single_node" },
        "governance": {
            "policy": { "tool_access": { "default_minimum_trust": floor } },
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
    if let Some(t) = tier {
        // P5-2: callers present identity-minted EdDSA JWTs (audience-bound
        // to THIS org's gateway); unsigned or foreign-signed calls refuse.
        doc["governance"]["access"] = json!({
            "jwks": {
                "keys_json": t.keys_json,
                "issuer": t.issuer,
                "audience": t.audience,
            }
        });
    }
    if any_obo {
        // Emitted ONLY when an obo federation survived the gates above.
        let w = obo.expect("any_obo implies wiring");
        doc["plugins"] = json!([{
            "id": EXCHANGE_PLUGIN_ID,
            "class": "credential_issuer",
            "source": { "oci": w.plugin_oci },
            // The plugin declares NetworkOutbound (it dials the exchange);
            // an ungranted capability is a boot refusal.
            "granted_capabilities": ["network_outbound"],
            "config": {
                "providers": {
                    EXCHANGE_PROVIDER: {
                        "token_url": w.exchange_url,
                        "client_id": "mcpg-tenant-gateway",
                    }
                }
            },
        }]);
        if w.non_production {
            // The plugin is BUSL: the community license gate needs this (or
            // an entitling token) or the whole gateway refuses to boot.
            doc["license"] = json!({ "non_production_use": true });
        }
    }
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
                    // ndots:1 — external names (the plugin's ghcr OCI pull)
                    // resolve absolutely instead of walking cluster search
                    // domains, where a wildcard site domain captures them
                    // (the exact state-pod incident). In-cluster federation
                    // endpoints are rendered as absolute trailing-dot FQDNs
                    // already.
                    dns_config: Some(k8s_openapi::api::core::v1::PodDNSConfig {
                        options: Some(vec![k8s_openapi::api::core::v1::PodDNSConfigOption {
                            name: Some("ndots".into()),
                            value: Some("1".into()),
                        }]),
                        ..Default::default()
                    }),
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
                            // OCI plugin cache (P5-3): the exchange plugin
                            // pull needs a writable ~/.cache under the
                            // read-only rootfs. Ephemeral by design — the
                            // digest-pinned pull re-fills it on restart.
                            mount("cache", "/home/mcpg/.cache", false),
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
                        empty_dir("cache"),
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
        let obo_audience = svc.spec.auth.as_ref().and_then(|a| {
            (a.mode == "obo").then(|| a.audience.clone().unwrap_or_else(|| name.clone()))
        });
        entries.push(FederationEntry {
            name,
            endpoint,
            allow: svc.spec.allow.clone(),
            exclude: svc.spec.exclude.clone(),
            token_env,
            obo_audience,
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

    // The verified tier (P5-2): inline the identity provider's JWKS so the
    // gateway verifies caller tokens with NO runtime fetch. Unfetchable ⇒
    // render the header tier and warn — an identity blip must not take the
    // org's tool plane down, only downgrade its trust floor.
    let tier = match (&ctx.identity.url, &ctx.aauth.provider) {
        (Some(identity_url), Some(issuer)) => {
            let jwks_url = format!("{}/aauth-jwks.json", identity_url.trim_end_matches('/'));
            match ctx.identity_http.get(&jwks_url).send().await {
                Ok(resp) if resp.status().is_success() => match resp.text().await {
                    Ok(keys_json) if keys_json.contains("\"keys\"") => Some(VerifiedTier {
                        keys_json,
                        issuer: issuer.clone(),
                        audience: gateway_audience(ns),
                    }),
                    _ => {
                        tracing::warn!(
                            ns,
                            "identity JWKS unreadable; tenant gateway stays header-tier"
                        );
                        None
                    }
                },
                other => {
                    tracing::warn!(
                        ns,
                        ?other,
                        "identity JWKS fetch failed; tenant gateway stays header-tier"
                    );
                    None
                }
            }
        }
        _ => None,
    };

    // OBO wiring (P5-3): plugin ref from the chart, exchange URL from the
    // identity plane the operator already knows. Any gap ⇒ obo entries drop
    // (warned inside render_config); the proxy plane keeps serving.
    let obo = match (&ctx.tenant_mcpg.exchange_plugin_oci, &ctx.identity.url) {
        (Some(plugin), Some(identity_url)) => Some(OboWiring {
            plugin_oci: plugin.clone(),
            exchange_url: format!("{}/v1/exchange", identity_url.trim_end_matches('/')),
            non_production: ctx.tenant_mcpg.non_production_license,
        }),
        _ => None,
    };

    let pp = PatchParams::apply(FIELD_MANAGER).force();
    let cm = ConfigMap {
        metadata: meta(ns, org, owner),
        data: Some(BTreeMap::from([(
            "config.yaml".to_string(),
            render_config(&entries, tier.as_ref(), obo.as_ref()),
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
            obo_audience: None,
        }
    }

    fn test_tier() -> VerifiedTier {
        VerifiedTier {
            keys_json:
                r#"{"keys":[{"kty":"OKP","crv":"Ed25519","alg":"EdDSA","kid":"k1","x":"AA"}]}"#
                    .into(),
            issuer: "http://agentctl-identity.agentctl-system".into(),
            audience: gateway_audience("org-acme"),
        }
    }

    fn test_obo() -> OboWiring {
        OboWiring {
            plugin_oci: "ghcr.io/mcpg-dev/plugins/credential-oauth-token-exchange@sha256:b6b6"
                .into(),
            exchange_url:
                "http://agentctl-identity.agentctl-system.svc.cluster.local.:80/v1/exchange".into(),
            non_production: true,
        }
    }

    /// The rendered document is the validated proxy-only posture: zero
    /// plugins, header-asserted trust + subject header, hot reload, audit
    /// fail-closed to a writable path, per-entry prefix + narrowed filter,
    /// private/insecure upstream opt-ins.
    #[test]
    fn config_is_proxy_only_and_governed() {
        let yaml = render_config(
            &[
                entry("state", &["state.*"]),
                FederationEntry {
                    token_env: Some(federation_token_env("crm")),
                    ..entry("crm", &["search_*"])
                },
            ],
            None,
            None,
        );
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

    /// P5-2: with the verified tier, the jwks block is inline (issuer +
    /// audience bound to THIS org), the trust floors rise to `verified`, and
    /// the self-asserted subject header is OFF.
    #[test]
    fn verified_tier_raises_floors_and_binds_audience() {
        let tier = VerifiedTier {
            keys_json:
                r#"{"keys":[{"kty":"OKP","crv":"Ed25519","alg":"EdDSA","kid":"k1","x":"AA"}]}"#
                    .into(),
            issuer: "http://agentctl-identity.agentctl-system".into(),
            audience: gateway_audience("org-acme"),
        };
        let yaml = render_config(&[entry("state", &["state.*"])], Some(&tier), None);
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            doc["gateway"]["server"]["trust_subject_header"],
            serde_yaml::Value::Bool(false)
        );
        assert_eq!(
            doc["governance"]["policy"]["tool_access"]["default_minimum_trust"],
            "verified"
        );
        assert_eq!(
            doc["governance"]["access"]["jwks"]["audience"],
            "mcpg:org-acme"
        );
        assert!(doc["governance"]["access"]["jwks"]["keys_json"]
            .as_str()
            .unwrap()
            .contains("Ed25519"));
        let feds = doc["mcp"]["federations"].as_sequence().unwrap();
        assert_eq!(feds[0]["governance"]["minimum_trust"], "verified");
    }

    /// P5-3: an obo entry under the verified tier renders the impersonation
    /// federation + the plugin registration + the license posture — and the
    /// STS endpoint lives ONLY on the provider (credential_config is
    /// audience-only; mcpg ignores anything else there).
    #[test]
    fn obo_entry_renders_impersonation_plugin_and_license() {
        let mut e = entry("zendesk", &[]);
        e.obo_audience = Some("zendesk".into());
        let yaml = render_config(
            &[e, entry("state", &["state.*"])],
            Some(&test_tier()),
            Some(&test_obo()),
        );
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let feds = doc["mcp"]["federations"].as_sequence().unwrap();
        assert_eq!(feds.len(), 2);
        let auth = &feds[0]["upstream"]["auth"]; // input order: zendesk, state
        assert_eq!(auth["mode"], "oauth_impersonation");
        assert_eq!(
            auth["credential"],
            "cred://dev.mcpg.credential.oauth-token-exchange/agentctl"
        );
        assert_eq!(auth["credential_config"]["audience"], "zendesk");
        assert!(auth["credential_config"].get("redeem_token_url").is_none());
        let plugin = &doc["plugins"][0];
        assert_eq!(plugin["class"], "credential_issuer");
        assert_eq!(plugin["granted_capabilities"][0], "network_outbound");
        assert!(plugin["source"]["oci"]
            .as_str()
            .unwrap()
            .contains("@sha256:"));
        assert!(plugin["config"]["providers"]["agentctl"]["token_url"]
            .as_str()
            .unwrap()
            .ends_with("/v1/exchange"));
        assert_eq!(
            plugin["config"]["providers"]["agentctl"]["client_id"],
            "mcpg-tenant-gateway"
        );
        assert_eq!(
            doc["license"]["non_production_use"],
            serde_yaml::Value::Bool(true)
        );
        // The plain proxy entry is untouched.
        assert!(feds[1]["upstream"].get("auth").is_none());
    }

    /// OBO never dials bare: without the verified tier (or without wiring)
    /// the entry DROPS and no plugin/license blocks are emitted.
    #[test]
    fn obo_entry_drops_without_verified_tier_or_wiring() {
        let mut e = entry("zendesk", &[]);
        e.obo_audience = Some("zendesk".into());
        // Wired but header-tier: impersonation would be refused — drop.
        let yaml = render_config(
            &[e.clone(), entry("state", &["state.*"])],
            None,
            Some(&test_obo()),
        );
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(doc["mcp"]["federations"].as_sequence().unwrap().len(), 1);
        assert!(doc.get("plugins").is_none());
        assert!(doc.get("license").is_none());
        // Verified but unwired: same drop.
        let yaml = render_config(&[e, entry("state", &["state.*"])], Some(&test_tier()), None);
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(doc["mcp"]["federations"].as_sequence().unwrap().len(), 1);
        assert!(doc.get("plugins").is_none());
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
