// SPDX-License-Identifier: BUSL-1.1
//! Per-tenant-namespace agent NetworkPolicies, reconciled by the operator for
//! defence-in-depth tenant isolation.
//!
//! The chart ships the SAME four data-plane policies, but only for the
//! *statically* enumerated `networkPolicies.agentNamespaces` — so a tenant
//! namespace created after install (the normal case for a multi-tenant control
//! plane) gets NO isolation until someone re-renders the chart. To cover that
//! case, on every `Agent`/`AgentFleet` reconcile the operator ensures, in the
//! workload's OWN namespace, the four policies that make the tenancy boundary
//! real for agent pods (label `app.kubernetes.io/name: agent`):
//!
//! 1. **`agent-default-deny`** — deny all ingress AND egress by default;
//! 2. **`agent-allow-controlplane-and-dns`** — re-open egress ONLY to DNS and the
//!    control-plane GATEWAYS (ModelGateway / MCPGateway / A2A gateway /
//!    coordination), scoped by pod selector so a tenant agent cannot reach e.g.
//!    the admission webhook or another tenant;
//! 3. **`agent-ingress-controlplane-only`** — accept ingress only from the
//!    control-plane namespace (no cross-tenant pod-to-pod traffic);
//! 4. **`agent-aauth-internet-egress`** — for **identity-provisioned (AAuth)**
//!    agents only (selected by [`AAUTH_POD_LABEL`], RFC 0024): HTTPS to PUBLIC
//!    address space (private/link-local/CGNAT carved out), so direct signed
//!    dials work while lateral movement stays default-denied. Inert in a
//!    namespace with no AAuth agents.
//!
//! All four are server-side-applied (idempotent, drift-corrected) namespace
//! singletons carrying NO owner reference — they gate EVERY agent pod in the
//! namespace, so deleting one Agent must not tear the namespace's isolation down.
//! The bodies are byte-identical to the chart's, so where both the chart (a listed
//! namespace) and the operator manage a policy, SSA simply co-owns it without
//! conflict.
//!
//! Enforced only by a NetworkPolicy-capable CNI (Calico/Cilium); inert but
//! harmless on kindnet. Gated by [`NetPolConfig`] (`NETWORK_POLICIES_ENABLED`,
//! default off — matching the chart's `networkPolicies.enabled`).

use k8s_openapi::api::networking::v1::{
    IPBlock, NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer,
    NetworkPolicyPort, NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    LabelSelector, LabelSelectorRequirement, ObjectMeta,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use std::collections::BTreeMap;

/// Field-manager identity for the NetworkPolicy server-side applies (shared with
/// the operator's other SSA writers so co-ownership with the chart is clean).
const FIELD_MANAGER: &str = "agentctl-operator";

/// The pod label every rendered agent workload carries (see `render::managed_labels`);
/// the policies select on it so they cover ALL agent pods in the namespace.
const AGENT_POD_NAME: &str = "agent";

/// Well-known policy names — identical to the chart's data-plane set so the two
/// co-own the same objects rather than fighting.
const P_DEFAULT_DENY: &str = "agent-default-deny";
const P_ALLOW_CP_DNS: &str = "agent-allow-controlplane-and-dns";
const P_INGRESS_CP_ONLY: &str = "agent-ingress-controlplane-only";
const P_AAUTH_EGRESS: &str = "agent-aauth-internet-egress";

/// Pod label the renderer stamps on identity-provisioned (AAuth) agent pods —
/// the selector key of [`P_AAUTH_EGRESS`], so the internet-egress hole opens
/// for exactly those pods and no other agent.
pub const AAUTH_POD_LABEL: &str = "agentctl.dev/aauth";

/// The control-plane gateway app names an agent is permitted to egress to — and
/// ONLY these (a bare namespaceSelector would also expose the admission webhook /
/// apiserver to a tenant agent).
const GATEWAY_APP_NAMES: [&str; 4] = [
    "agentctl-modelgateway",
    "agentctl-mcpgateway",
    "agentctl-gateway",
    "agentctl-coordination",
];

/// Operator-side wiring for the per-namespace agent NetworkPolicies. Read once at
/// startup ([`NetPolConfig::from_env`]) and carried on the reconcile context.
/// `enabled=false` (the default) turns the whole ensure path off — a cluster whose
/// CNI does not enforce NetworkPolicy is untouched.
#[derive(Clone, Debug, Default)]
pub struct NetPolConfig {
    /// Reconcile the agent NetworkPolicies. `NETWORK_POLICIES_ENABLED`, default
    /// `false` (matches the chart's `networkPolicies.enabled`).
    pub enabled: bool,
    /// The control-plane namespace the gateway egress + control-plane ingress rules
    /// point at (the operator's `POD_NAMESPACE`). Required to build the egress
    /// allow; absent ⇒ the ensure path is skipped (fail closed on config, never
    /// render an over-broad policy).
    pub control_plane_ns: Option<String>,
}

impl NetPolConfig {
    /// Build from the operator environment. Enabled only when
    /// `NETWORK_POLICIES_ENABLED` is explicitly truthy; the control-plane namespace
    /// is the operator's `POD_NAMESPACE`.
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var("NETWORK_POLICIES_ENABLED")
                .map(|v| env_truthy(&v))
                .unwrap_or(false),
            control_plane_ns: std::env::var("POD_NAMESPACE")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }

    /// Whether the ensure path should run: enabled AND a control-plane namespace is
    /// known (needed to scope the egress allow). Warns-by-absence elsewhere.
    pub fn active(&self) -> bool {
        self.enabled && self.control_plane_ns.is_some()
    }
}

/// Strict truthy parse for an explicit opt-in flag (unlike the scaler's
/// default-on parse): only `1/true/yes/on` enable.
fn env_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Ensure the four agent NetworkPolicies in `agent_ns` (server-side apply;
/// idempotent, drift-corrected). No-op when [`NetPolConfig::active`] is false.
/// Errors bubble to the reconcile (transient apiserver errors → retried).
pub async fn ensure_agent_netpols(
    client: &Client,
    cfg: &NetPolConfig,
    agent_ns: &str,
) -> Result<(), kube::Error> {
    let Some(cp_ns) = cfg.control_plane_ns.as_deref() else {
        return Ok(());
    };
    let params = PatchParams::apply(FIELD_MANAGER).force();
    let api: Api<NetworkPolicy> = Api::namespaced(client.clone(), agent_ns);
    for policy in agent_network_policies(cp_ns) {
        // Every builder sets metadata.name, so the unwrap is total.
        let name = policy
            .metadata
            .name
            .clone()
            .expect("agent NetworkPolicy body always names itself");
        api.patch(&name, &params, &Patch::Apply(&policy)).await?;
    }
    Ok(())
}

/// The four agent NetworkPolicy bodies (pure) for an agent namespace, with the
/// egress/ingress rules pointed at the control-plane namespace `cp_ns`. The
/// aauth internet-egress policy is always rendered — it selects only pods the
/// renderer labels [`AAUTH_POD_LABEL`], so it is inert in a namespace with no
/// identity-provisioned agents.
pub fn agent_network_policies(cp_ns: &str) -> Vec<NetworkPolicy> {
    vec![
        default_deny(),
        allow_controlplane_and_dns(cp_ns),
        ingress_controlplane_only(cp_ns),
        aauth_internet_egress(),
    ]
}

/// `agent-default-deny`: deny all ingress AND egress for agent pods (the allow
/// policies below re-open only sanctioned flows).
fn default_deny() -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta(P_DEFAULT_DENY),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(agent_pod_selector()),
            policy_types: Some(vec!["Ingress".to_string(), "Egress".to_string()]),
            // No ingress/egress rules ⇒ deny-all.
            ..Default::default()
        }),
    }
}

/// `agent-allow-controlplane-and-dns`: egress ONLY to DNS and the control-plane
/// gateways (scoped by pod selector), nothing else — not the internet, not other
/// tenants, not other control-plane pods.
fn allow_controlplane_and_dns(cp_ns: &str) -> NetworkPolicy {
    let dns = NetworkPolicyEgressRule {
        to: Some(vec![NetworkPolicyPeer {
            // Any namespace (kube-dns lives in kube-system).
            namespace_selector: Some(LabelSelector::default()),
            ..Default::default()
        }]),
        ports: Some(vec![port("UDP", 53), port("TCP", 53)]),
    };
    let gateways = NetworkPolicyEgressRule {
        to: Some(vec![NetworkPolicyPeer {
            namespace_selector: Some(ns_name_selector(cp_ns)),
            pod_selector: Some(gateway_pod_selector()),
            ..Default::default()
        }]),
        // 443: the v2 HTTPS listeners (Model/MCP gateways) agents dial keyless;
        // 8080: coordination / legacy plaintext.
        ports: Some(vec![port("TCP", 443), port("TCP", 8080)]),
    };
    NetworkPolicy {
        metadata: meta(P_ALLOW_CP_DNS),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(agent_pod_selector()),
            policy_types: Some(vec!["Egress".to_string()]),
            egress: Some(vec![dns, gateways]),
            ..Default::default()
        }),
    }
}

/// `agent-aauth-internet-egress`: the RFC 0024 baseline egress tier for
/// identity-provisioned agents — HTTPS to **public** address space only
/// (`0.0.0.0/0`/`::/0` minus private/link-local/CGNAT ranges), selected by the
/// [`AAUTH_POD_LABEL`] the renderer stamps. Direct signed dials to remote
/// AAuth resources (and a public Agent Provider) work; lateral movement into
/// cluster/private space stays blocked by the default-deny. Vanilla
/// NetworkPolicy cannot express per-FQDN egress — this is the honest coarse
/// tier; DNS-aware CNIs (Cilium/Calico) can tighten it to the declared
/// endpoints (documented, not rendered).
fn aauth_internet_egress() -> NetworkPolicy {
    let v4 = NetworkPolicyPeer {
        ip_block: Some(IPBlock {
            cidr: "0.0.0.0/0".to_string(),
            except: Some(vec![
                "10.0.0.0/8".to_string(),
                "172.16.0.0/12".to_string(),
                "192.168.0.0/16".to_string(),
                "169.254.0.0/16".to_string(),
                "100.64.0.0/10".to_string(),
            ]),
        }),
        ..Default::default()
    };
    let v6 = NetworkPolicyPeer {
        ip_block: Some(IPBlock {
            cidr: "::/0".to_string(),
            except: Some(vec!["fc00::/7".to_string(), "fe80::/10".to_string()]),
        }),
        ..Default::default()
    };
    NetworkPolicy {
        metadata: meta(P_AAUTH_EGRESS),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(aauth_pod_selector()),
            policy_types: Some(vec!["Egress".to_string()]),
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![v4, v6]),
                ports: Some(vec![port("TCP", 443)]),
            }]),
            ..Default::default()
        }),
    }
}

/// `agent-ingress-controlplane-only`: accept ingress only from the control-plane
/// namespace (the apiserver + A2A gateway reach the agent's mTLS :8443); no
/// cross-tenant pod-to-pod traffic.
fn ingress_controlplane_only(cp_ns: &str) -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta(P_INGRESS_CP_ONLY),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(agent_pod_selector()),
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress: Some(vec![NetworkPolicyIngressRule {
                from: Some(vec![NetworkPolicyPeer {
                    namespace_selector: Some(ns_name_selector(cp_ns)),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        }),
    }
}

/// Object metadata for a policy: name + the operator's managed-by label.
fn meta(name: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        labels: Some(BTreeMap::from([(
            "app.kubernetes.io/managed-by".to_string(),
            "agentctl".to_string(),
        )])),
        ..Default::default()
    }
}

/// Selector matching only identity-provisioned (AAuth) agent pods.
fn aauth_pod_selector() -> LabelSelector {
    LabelSelector {
        match_labels: Some(BTreeMap::from([(
            AAUTH_POD_LABEL.to_string(),
            "true".to_string(),
        )])),
        ..Default::default()
    }
}

/// Selector matching every agent pod in the namespace.
fn agent_pod_selector() -> LabelSelector {
    LabelSelector {
        match_labels: Some(BTreeMap::from([(
            "app.kubernetes.io/name".to_string(),
            AGENT_POD_NAME.to_string(),
        )])),
        ..Default::default()
    }
}

/// Selector matching ONLY the control-plane gateway pods (an `In` set over the
/// app name), so egress cannot reach any other control-plane pod.
fn gateway_pod_selector() -> LabelSelector {
    LabelSelector {
        match_expressions: Some(vec![LabelSelectorRequirement {
            key: "app.kubernetes.io/name".to_string(),
            operator: "In".to_string(),
            values: Some(GATEWAY_APP_NAMES.iter().map(|s| s.to_string()).collect()),
        }]),
        ..Default::default()
    }
}

/// Selector matching a namespace by its immutable `kubernetes.io/metadata.name`
/// label (set by the apiserver on every namespace).
fn ns_name_selector(ns: &str) -> LabelSelector {
    LabelSelector {
        match_labels: Some(BTreeMap::from([(
            "kubernetes.io/metadata.name".to_string(),
            ns.to_string(),
        )])),
        ..Default::default()
    }
}

/// A single `protocol/port` allow.
fn port(protocol: &str, number: i32) -> NetworkPolicyPort {
    NetworkPolicyPort {
        protocol: Some(protocol.to_string()),
        port: Some(IntOrString::Int(number)),
        end_port: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_truthy_only_explicit_true() {
        for t in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert!(env_truthy(t), "{t:?} should be truthy");
        }
        for f in ["0", "false", "no", "off", "", "enabled", "2"] {
            assert!(!env_truthy(f), "{f:?} should be falsey");
        }
    }

    #[test]
    fn active_requires_enabled_and_a_control_plane_ns() {
        let mut cfg = NetPolConfig {
            enabled: true,
            control_plane_ns: None,
        };
        assert!(
            !cfg.active(),
            "no control-plane ns ⇒ inactive (fail closed)"
        );
        cfg.control_plane_ns = Some("agentctl-system".to_string());
        assert!(cfg.active());
        cfg.enabled = false;
        assert!(!cfg.active(), "disabled ⇒ inactive");
    }

    #[test]
    fn renders_exactly_four_named_policies() {
        let ps = agent_network_policies("agentctl-system");
        let names: Vec<&str> = ps
            .iter()
            .map(|p| p.metadata.name.as_deref().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                P_DEFAULT_DENY,
                P_ALLOW_CP_DNS,
                P_INGRESS_CP_ONLY,
                P_AAUTH_EGRESS
            ]
        );
    }

    #[test]
    fn default_deny_denies_both_directions_with_no_rules() {
        let spec = default_deny().spec.unwrap();
        let mut types = spec.policy_types.unwrap();
        types.sort();
        assert_eq!(types, vec!["Egress".to_string(), "Ingress".to_string()]);
        assert!(spec.ingress.is_none(), "no ingress rules ⇒ deny all in");
        assert!(spec.egress.is_none(), "no egress rules ⇒ deny all out");
        // Selects agent pods (not everything).
        assert_eq!(
            spec.pod_selector.unwrap().match_labels.unwrap()["app.kubernetes.io/name"],
            "agent"
        );
    }

    #[test]
    fn egress_allows_dns_and_only_the_gateways_scoped_to_cp_ns() {
        let spec = allow_controlplane_and_dns("agentctl-system").spec.unwrap();
        assert_eq!(spec.policy_types.unwrap(), vec!["Egress".to_string()]);
        let egress = spec.egress.unwrap();
        assert_eq!(egress.len(), 2, "one DNS rule + one gateway rule");

        // DNS rule: any namespace, ports 53/udp + 53/tcp.
        let dns = &egress[0];
        let dns_ports: Vec<(String, i32)> = dns
            .ports
            .as_ref()
            .unwrap()
            .iter()
            .map(|p| (p.protocol.clone().unwrap(), int_port(p)))
            .collect();
        assert!(dns_ports.contains(&("UDP".to_string(), 53)));
        assert!(dns_ports.contains(&("TCP".to_string(), 53)));

        // Gateway rule: scoped to the control-plane namespace AND the gateway pods,
        // ports 443 + 8080.
        let gw = &egress[1];
        let peer = &gw.to.as_ref().unwrap()[0];
        assert_eq!(
            peer.namespace_selector
                .as_ref()
                .unwrap()
                .match_labels
                .as_ref()
                .unwrap()["kubernetes.io/metadata.name"],
            "agentctl-system"
        );
        let req = &peer
            .pod_selector
            .as_ref()
            .unwrap()
            .match_expressions
            .as_ref()
            .unwrap()[0];
        assert_eq!(req.operator, "In");
        let vals = req.values.as_ref().unwrap();
        for g in GATEWAY_APP_NAMES {
            assert!(vals.contains(&g.to_string()), "gateway {g} must be allowed");
        }
        // The admission webhook / apiserver are NOT reachable.
        assert!(!vals.contains(&"agentctl-admission".to_string()));
        assert!(!vals.contains(&"agentctl-apiserver".to_string()));
        let gw_ports: Vec<i32> = gw.ports.as_ref().unwrap().iter().map(int_port).collect();
        assert!(gw_ports.contains(&443) && gw_ports.contains(&8080));
    }

    #[test]
    fn aauth_egress_opens_public_https_only_for_labeled_pods() {
        // Rendered as the fourth policy of the set.
        let all = agent_network_policies("agentctl-system");
        assert_eq!(all.len(), 4);
        let p = &all[3];
        assert_eq!(p.metadata.name.as_deref(), Some(P_AAUTH_EGRESS));

        let spec = p.spec.as_ref().unwrap();
        // Selects ONLY identity-provisioned pods — inert for every other agent.
        assert_eq!(
            spec.pod_selector
                .as_ref()
                .unwrap()
                .match_labels
                .as_ref()
                .unwrap()[AAUTH_POD_LABEL],
            "true"
        );
        assert_eq!(
            spec.policy_types.as_ref().unwrap(),
            &vec!["Egress".to_string()]
        );
        let rule = &spec.egress.as_ref().unwrap()[0];
        // 443 only — CEL already forbids plaintext direct endpoints.
        let ports: Vec<i32> = rule.ports.as_ref().unwrap().iter().map(int_port).collect();
        assert_eq!(ports, vec![443]);
        // Public space only: v4 + v6 blocks each carve out private/link-local
        // (and v4 CGNAT) ranges, so lateral movement stays default-denied.
        let peers = rule.to.as_ref().unwrap();
        let v4 = peers[0].ip_block.as_ref().unwrap();
        assert_eq!(v4.cidr, "0.0.0.0/0");
        let except = v4.except.as_ref().unwrap();
        for range in [
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "100.64.0.0/10",
        ] {
            assert!(except.contains(&range.to_string()), "missing {range}");
        }
        let v6 = peers[1].ip_block.as_ref().unwrap();
        assert_eq!(v6.cidr, "::/0");
        assert!(v6
            .except
            .as_ref()
            .unwrap()
            .contains(&"fc00::/7".to_string()));
    }

    #[test]
    fn ingress_only_from_control_plane_namespace() {
        let spec = ingress_controlplane_only("agentctl-system").spec.unwrap();
        assert_eq!(spec.policy_types.unwrap(), vec!["Ingress".to_string()]);
        let ingress = spec.ingress.unwrap();
        let peer = &ingress[0].from.as_ref().unwrap()[0];
        assert_eq!(
            peer.namespace_selector
                .as_ref()
                .unwrap()
                .match_labels
                .as_ref()
                .unwrap()["kubernetes.io/metadata.name"],
            "agentctl-system"
        );
        // No podSelector ⇒ any control-plane pod may reach the agent's mTLS surface.
        assert!(peer.pod_selector.is_none());
    }

    fn int_port(p: &NetworkPolicyPort) -> i32 {
        match p.port.as_ref().unwrap() {
            IntOrString::Int(n) => *n,
            IntOrString::String(s) => panic!("expected int port, got {s}"),
        }
    }
}
