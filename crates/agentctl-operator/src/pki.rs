// SPDX-License-Identifier: BUSL-1.1
//! Workload PKI: the operator-ensured serving identity + trust distribution
//! behind the v2 render (contract 2.0 — identity is the boundary).
//!
//! For every reconciled `Agent`/`AgentFleet` the operator ensures, in the
//! workload's namespace:
//!
//! 1. a **cert-manager `Certificate`** minting the workload's serving identity
//!    into the Secret the render mounts ([`crate::serving_secret_name`]) —
//!    SANs cover the Service name AND the per-pod DNS form
//!    (`*.<ns>.pod.cluster.local`) so per-pod management dials verify;
//! 2. the **`agentctl-ca` ConfigMap** carrying the cluster CA *public*
//!    certificate (read once from the operator's own mounted CA file) — the
//!    render mounts it as the agent's client-CA + outbound trust anchor.
//!
//! Both are server-side-applied (idempotent, drift-corrected) and owned by the
//! CR where ownership is sound: the `Certificate` is owner-ref'd to its
//! Agent/Fleet (GC reclaims it); the ConfigMap is namespace-shared and carries
//! NO owner (many workloads reference it — deleting one agent must not tear
//! down the namespace's trust anchor).

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::Client;
use std::collections::BTreeMap;

use crate::render::{serving_secret_name, CA_CONFIGMAP, CA_KEY};

/// Field-manager identity for the PKI server-side applies.
const FIELD_MANAGER: &str = "agentctl-operator";

/// Operator-side PKI wiring, read once at startup ([`PkiConfig::from_env`]) and
/// carried on the reconcile context. `enabled=false` (no issuer configured)
/// turns the whole ensure path off — a dev cluster without cert-manager still
/// reconciles workloads (they will Pending on the missing Secret until PKI is
/// configured, which is the honest signal).
#[derive(Clone, Debug, Default)]
pub struct PkiConfig {
    /// cert-manager issuer the serving Certificates reference
    /// (`AGENTCTL_ISSUER_REF`, `ClusterIssuer/<name>` or `Issuer/<name>`).
    /// `None` ⇒ PKI ensure disabled.
    pub issuer: Option<IssuerRef>,
    /// The cluster CA **public certificate** PEM, read at startup from
    /// `AGENTCTL_CA_FILE` (the operator's own CA mount). Distributed to every
    /// agent namespace as the [`CA_CONFIGMAP`] ConfigMap.
    pub ca_pem: Option<String>,
}

/// A cert-manager issuer reference (`kind` ∈ {`ClusterIssuer`, `Issuer`}).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuerRef {
    pub kind: String,
    pub name: String,
}

impl PkiConfig {
    /// Build from the operator environment: `AGENTCTL_ISSUER_REF`
    /// (`Kind/name`; e.g. `ClusterIssuer/agentctl-ca`) + `AGENTCTL_CA_FILE`
    /// (path to the mounted CA public cert). Either absent ⇒ that half is off.
    pub fn from_env() -> Self {
        let issuer = std::env::var("AGENTCTL_ISSUER_REF")
            .ok()
            .and_then(|v| parse_issuer_ref(v.trim()));
        let ca_pem = std::env::var("AGENTCTL_CA_FILE")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .and_then(|p| match std::fs::read_to_string(p.trim()) {
                Ok(pem) if pem.contains("BEGIN CERTIFICATE") => Some(pem),
                _ => None,
            });
        PkiConfig { issuer, ca_pem }
    }

    /// Whether the ensure path runs at all.
    pub fn enabled(&self) -> bool {
        self.issuer.is_some() || self.ca_pem.is_some()
    }
}

/// Parse `Kind/name` into an [`IssuerRef`]; only the two cert-manager issuer
/// kinds are accepted (anything else is a misconfiguration → `None`, logged by
/// the caller at startup).
pub fn parse_issuer_ref(v: &str) -> Option<IssuerRef> {
    let (kind, name) = v.split_once('/')?;
    if name.is_empty() || !matches!(kind, "ClusterIssuer" | "Issuer") {
        return None;
    }
    Some(IssuerRef {
        kind: kind.to_string(),
        name: name.to_string(),
    })
}

/// The cert-manager `Certificate` for a workload's serving identity (pure —
/// unit-testable): secret [`serving_secret_name`], SANs for the workload's
/// Service name + the pod-DNS wildcard, owner-ref'd to the CR so GC reclaims
/// it with the Agent/Fleet.
pub fn certificate_body(
    workload: &str,
    ns: &str,
    issuer: &IssuerRef,
    owner: &OwnerReference,
) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "cert-manager.io/v1",
        "kind": "Certificate",
        "metadata": {
            "name": serving_secret_name(workload),
            "namespace": ns,
            "ownerReferences": [{
                "apiVersion": owner.api_version,
                "kind": owner.kind,
                "name": owner.name,
                "uid": owner.uid,
                "controller": true,
                "blockOwnerDeletion": true,
            }],
        },
        "spec": {
            "secretName": serving_secret_name(workload),
            "duration": "2160h",     // 90d
            "renewBefore": "720h",   // renew at T-30d; agentd reloads in place
            "privateKey": { "algorithm": "ECDSA", "size": 256 },
            "usages": ["server auth"],
            "dnsNames": [
                // The workload's (headless) Service name forms.
                format!("{workload}.{ns}.svc"),
                format!("{workload}.{ns}.svc.cluster.local"),
                // Per-pod dial form (CoreDNS `pods insecure`, kubeadm default):
                // <ip-dashed>.<ns>.pod.cluster.local — how the control plane
                // addresses ONE replica (drain/pause THIS pod).
                format!("*.{ns}.pod.cluster.local"),
            ],
            "issuerRef": { "kind": issuer.kind, "name": issuer.name },
        },
    })
}

/// The per-namespace CA ConfigMap body (pure). Deliberately UN-owned: it is
/// namespace-shared trust, not a per-workload child.
pub fn ca_configmap_body(ns: &str, ca_pem: &str) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(CA_CONFIGMAP.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/managed-by".to_string(),
                "agentctl".to_string(),
            )])),
            ..Default::default()
        },
        data: Some(BTreeMap::from([(CA_KEY.to_string(), ca_pem.to_string())])),
        ..Default::default()
    }
}

/// Ensure the workload's PKI in `ns`: SSA the CA ConfigMap (when the operator
/// has a CA) and the serving `Certificate` (when an issuer is configured).
/// Idempotent; errors bubble to the reconcile (transient → retried).
pub async fn ensure_workload_pki(
    client: &Client,
    pki: &PkiConfig,
    ns: &str,
    workload: &str,
    owner: &OwnerReference,
) -> Result<(), kube::Error> {
    let params = PatchParams::apply(FIELD_MANAGER).force();

    if let Some(ca_pem) = &pki.ca_pem {
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
        let body = ca_configmap_body(ns, ca_pem);
        cms.patch(CA_CONFIGMAP, &params, &Patch::Apply(&body))
            .await?;
    }

    if let Some(issuer) = &pki.issuer {
        // cert-manager is not a compiled-in dependency: apply the Certificate
        // as a DynamicObject (same discipline as the KEDA ScaledObject).
        let gvk = GroupVersionKind::gvk("cert-manager.io", "v1", "Certificate");
        let ar = ApiResource::from_gvk(&gvk);
        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
        let body = certificate_body(workload, ns, issuer, owner);
        api.patch(
            &serving_secret_name(workload),
            &params,
            &Patch::Apply(&body),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> OwnerReference {
        OwnerReference {
            api_version: "agents.x-k8s.io/v1alpha1".into(),
            kind: "Agent".into(),
            name: "demo".into(),
            uid: "uid-1".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    #[test]
    fn issuer_ref_parses_the_two_kinds_only() {
        assert_eq!(
            parse_issuer_ref("ClusterIssuer/agentctl-ca"),
            Some(IssuerRef {
                kind: "ClusterIssuer".into(),
                name: "agentctl-ca".into()
            })
        );
        assert_eq!(
            parse_issuer_ref("Issuer/ns-local"),
            Some(IssuerRef {
                kind: "Issuer".into(),
                name: "ns-local".into()
            })
        );
        assert_eq!(parse_issuer_ref("ClusterIssuer/"), None);
        assert_eq!(parse_issuer_ref("Certificate/x"), None);
        assert_eq!(parse_issuer_ref("no-slash"), None);
    }

    #[test]
    fn certificate_covers_service_and_pod_dns_and_is_owned() {
        let issuer = IssuerRef {
            kind: "ClusterIssuer".into(),
            name: "agentctl-ca".into(),
        };
        let c = certificate_body("demo", "agents", &issuer, &owner());
        assert_eq!(c["apiVersion"], "cert-manager.io/v1");
        assert_eq!(c["metadata"]["name"], "demo-serving-tls");
        assert_eq!(c["spec"]["secretName"], "demo-serving-tls");
        let sans = c["spec"]["dnsNames"].as_array().unwrap();
        assert!(sans.contains(&serde_json::json!("demo.agents.svc")));
        assert!(sans.contains(&serde_json::json!("demo.agents.svc.cluster.local")));
        // The per-pod dial form the mTLS management client verifies against.
        assert!(sans.contains(&serde_json::json!("*.agents.pod.cluster.local")));
        assert_eq!(c["spec"]["issuerRef"]["kind"], "ClusterIssuer");
        assert_eq!(c["spec"]["issuerRef"]["name"], "agentctl-ca");
        // GC: owned by the CR.
        assert_eq!(c["metadata"]["ownerReferences"][0]["kind"], "Agent");
        assert_eq!(c["metadata"]["ownerReferences"][0]["uid"], "uid-1");
        // Rotation cadence: renew well before expiry (agentd reloads in place).
        assert_eq!(c["spec"]["duration"], "2160h");
        assert_eq!(c["spec"]["renewBefore"], "720h");
    }

    #[test]
    fn ca_configmap_is_namespace_shared_and_unowned() {
        let cm = ca_configmap_body("agents", "-----BEGIN CERTIFICATE-----\nx\n");
        assert_eq!(cm.metadata.name.as_deref(), Some(CA_CONFIGMAP));
        assert_eq!(cm.metadata.namespace.as_deref(), Some("agents"));
        assert!(cm.metadata.owner_references.is_none(), "must stay unowned");
        assert!(cm.data.unwrap()[CA_KEY].contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn pki_config_disabled_without_env() {
        // No AGENTCTL_ISSUER_REF / AGENTCTL_CA_FILE in a unit-test env.
        let pki = PkiConfig::default();
        assert!(!pki.enabled());
    }
}
