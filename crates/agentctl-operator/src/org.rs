// SPDX-License-Identifier: BUSL-1.1
//! # Organization reconciliation (RFC 0033 §2.1, P1-3)
//!
//! The tenancy root: an [`Organization`] reconciles to its **managed
//! namespaces** (`org-<name>` + any `spec.namespaces.extra`, each labeled and
//! owner-referenced so deleting the org garbage-collects its spaces) and a
//! per-namespace **ResourceQuota** enforcing `spec.quotas.agents` as
//! `count/agents.agentctl.dev`. The metering ceilings (tokens/day, sandbox
//! CPU) are recorded on the spec for the billing plane and are NOT Kubernetes
//! quotas. Access-policy *resolution* is pure library code
//! ([`agent_api::org::access`]); *enforcement* wires up at the gateway /
//! apiserver in P1-8 — nothing here inspects principals.
//!
//! `namespaces.mode: unmanaged` reconciles no namespaces (the org then only
//! scopes identity/policy); an existing quota object in a previously-managed
//! namespace is left behind deliberately (the namespace's owner is the org —
//! flipping to unmanaged is a policy statement, not a teardown).

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_api::org::{org_namespace, NamespaceMode, Organization, OrganizationStatus};
use agent_api::Condition;
use k8s_openapi::api::core::v1::{Namespace, ResourceQuota, ResourceQuotaSpec};
use k8s_openapi::api::rbac::v1::{PolicyRule, Role as RbacRole, RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Resource, ResourceExt};
use tracing::{info, warn};

use crate::controller::{error_backoff, requeue_after, Ctx, Error};

const FIELD_MANAGER: &str = "agentctl-operator";
/// Label stamped on every managed namespace, carrying the owning org's name.
pub const ORG_LABEL: &str = "agentctl.dev/organization";
/// The managed per-namespace quota object.
pub const QUOTA_NAME: &str = "agentctl-org-quota";
/// The ResourceQuota key counting Agent CRs.
pub const AGENTS_COUNT_KEY: &str = "count/agents.agentctl.dev";

/// The namespaces this org manages, in apply order. Empty for unmanaged mode.
pub fn managed_namespaces(org: &Organization) -> Vec<String> {
    let spec_ns = org.spec.namespaces.clone().unwrap_or_default();
    if spec_ns.mode == NamespaceMode::Unmanaged {
        return Vec::new();
    }
    let mut out = vec![org_namespace(&org.name_any())];
    for extra in &spec_ns.extra {
        if !out.contains(extra) {
            out.push(extra.clone());
        }
    }
    out
}

/// The desired managed Namespace: labeled with the owning org and
/// owner-referenced to it (cluster→cluster ownership is legal, and it makes
/// `kubectl delete org` tear the tenant down through GC).
pub fn desired_namespace(name: &str, org_name: &str, owner: OwnerReference) -> Namespace {
    Namespace {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(BTreeMap::from([
                (ORG_LABEL.to_string(), org_name.to_string()),
                (
                    "app.kubernetes.io/managed-by".to_string(),
                    "agentctl-operator".to_string(),
                ),
            ])),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The desired per-namespace quota: `spec.quotas.agents` as a CR count.
/// `None` when the spec sets no agent ceiling (the caller then deletes any
/// previously-applied quota so lowering→removing a ceiling round-trips).
pub fn desired_quota(namespace: &str, agents: Option<i64>) -> Option<ResourceQuota> {
    let agents = agents?;
    Some(ResourceQuota {
        metadata: ObjectMeta {
            name: Some(QUOTA_NAME.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(ResourceQuotaSpec {
            hard: Some(BTreeMap::from([(
                AGENTS_COUNT_KEY.to_string(),
                Quantity(agents.to_string()),
            )])),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// The RBAC MIRROR (RFC 0033 §2.1): project the org's accessPolicies as
/// Kubernetes Roles + RoleBindings in each managed namespace so direct
/// `kubectl` users get the same role ladder. Two honest limits, both
/// documented at the CRD: (1) K8s RBAC has no label selectors — a
/// selector-scoped grant widens to the whole namespace here (precise
/// label scoping is enforced at the gateway); (2) only EXACT group names
/// mirror (a glob like `okta:eng-*` cannot be a K8s Group subject).
pub fn desired_org_roles(ns: &str) -> Vec<RbacRole> {
    let role = |name: &str, rules: Vec<PolicyRule>| RbacRole {
        metadata: ObjectMeta {
            name: Some(format!("agentctl-org-{name}")),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        rules: Some(rules),
    };
    let read = PolicyRule {
        api_groups: Some(vec!["agentctl.dev".into()]),
        resources: Some(vec![
            "agents".into(),
            "agents/status".into(),
            "agentfleets".into(),
            "agentfleets/status".into(),
            "modelpools".into(),
        ]),
        verbs: vec!["get".into(), "list".into(), "watch".into()],
        ..Default::default()
    };
    // The aggregated management verbs: POST /…/agents/<name>/<verb> is RBAC
    // resource `agents/<verb>`, verb `create`.
    let mgmt_verbs: Vec<String> = ["drain", "lame-duck", "pause", "resume", "cancel"]
        .iter()
        .flat_map(|v| [format!("agents/{v}"), format!("agentfleets/{v}")])
        .collect();
    let manage = PolicyRule {
        api_groups: Some(vec!["management.agentctl.dev".into()]),
        resources: Some(mgmt_verbs),
        verbs: vec!["create".into()],
        ..Default::default()
    };
    let full = PolicyRule {
        api_groups: Some(vec![
            "agentctl.dev".into(),
            "management.agentctl.dev".into(),
        ]),
        resources: Some(vec!["*".into()]),
        verbs: vec!["*".into()],
        ..Default::default()
    };
    vec![
        role("viewer", vec![read.clone()]),
        role("operator", vec![read, manage]),
        role("admin", vec![full]),
    ]
}

/// The mirror bindings: one per role, subjects = the union of EXACT group
/// names from policies granting that role (glob patterns are skipped — they
/// cannot name a K8s Group). Always all three objects, deterministically:
/// a role no policy grants gets an EMPTY subject list, which grants nothing
/// and prunes stale subjects on policy change (SSA replaces the object).
pub fn desired_role_bindings(
    ns: &str,
    policies: &[agent_api::org::AccessPolicy],
) -> Vec<RoleBinding> {
    use agent_api::org::Role;
    let groups_for = |role: Role| -> Vec<String> {
        let mut groups: Vec<String> = policies
            .iter()
            .filter(|p| p.role == role)
            .flat_map(|p| p.match_.groups.iter())
            .filter(|g| !g.contains('*'))
            .cloned()
            .collect();
        groups.sort();
        groups.dedup();
        groups
    };
    [
        (Role::Viewer, "viewer"),
        (Role::Operator, "operator"),
        (Role::Admin, "admin"),
    ]
    .into_iter()
    .map(|(role, name)| RoleBinding {
        metadata: ObjectMeta {
            name: Some(format!("agentctl-org-{name}")),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: Some("rbac.authorization.k8s.io".into()),
            kind: "Role".into(),
            name: format!("agentctl-org-{name}"),
        },
        subjects: Some(
            groups_for(role)
                .into_iter()
                .map(|g| Subject {
                    api_group: Some("rbac.authorization.k8s.io".into()),
                    kind: "Group".into(),
                    name: g,
                    ..Default::default()
                })
                .collect(),
        ),
    })
    .collect()
}

/// The Ready condition for a fully-applied org.
pub fn org_ready_condition(observed_generation: Option<i64>, namespaces: usize) -> Condition {
    Condition {
        type_: "Ready".to_string(),
        status: "True".to_string(),
        reason: Some("Provisioned".to_string()),
        message: Some(format!("{namespaces} managed namespace(s) reconciled")),
        observed_generation,
        last_transition_time: None,
    }
}

/// The seeded per-namespace `control` registry entry (P4-1): the platform
/// control MCP, reachable by AAuth-signed dials, tool ceiling `control.*`.
/// Pure — the endpoint comes from the operator env (chart `control.enabled`).
pub fn desired_control_service(ns: &str, endpoint: &str) -> agent_api::v1alpha2::MCPService {
    use agent_api::v1alpha2 as v2;
    let mut svc = v2::MCPService::new(
        "control",
        v2::MCPServiceSpec {
            endpoint: Some(endpoint.to_string()),
            allow: vec!["control.*".into()],
            ..Default::default()
        },
    );
    svc.metadata.namespace = Some(ns.to_string());
    svc
}

/// The seeded `supervisor` AgentClass (P4-3): the default profile every
/// managed namespace starts with — the platform persona + the control grant.
/// Orgs OWN it after seeding (create-if-absent, never overwritten).
pub fn desired_supervisor_class(ns: &str, with_control: bool) -> agent_api::v1alpha2::AgentClass {
    use agent_api::v1alpha2 as v2;
    let mut class = v2::AgentClass::new(
        crate::supervisor::SUPERVISOR_CLASS,
        v2::AgentClassSpec {
            supervisor: Some(v2::SupervisorProfile {
                instruction: Some(
                    "You are this user's supervisor: their standing assistant for the agent \
                     estate in this organization. Use your control tools to list, inspect, \
                     resolve and create agents when asked; report statuses plainly; never \
                     invent an agent that your tools do not show."
                        .into(),
                ),
                services: if with_control {
                    vec![v2::ServiceGrant {
                        name: "control".into(),
                        allow: vec!["control.*".into()],
                    }]
                } else {
                    Vec::new()
                },
                budget: None,
            }),
            ..Default::default()
        },
    );
    class.metadata.namespace = Some(ns.to_string());
    class
}

/// Seed the platform defaults into a managed namespace — CREATE-IF-ABSENT:
/// the org owns both objects after birth (a 409 is success, and the operator
/// never overwrites an org's edits).
async fn seed_namespace_defaults(ctx: &Ctx, ns: &str) -> Result<(), Error> {
    use agent_api::v1alpha2 as v2;
    let Ok(control_url) = std::env::var("AGENTCTL_CONTROL_URL") else {
        return Ok(());
    };
    let create_if_absent_svc: Api<v2::MCPService> = Api::namespaced(ctx.client.clone(), ns);
    match create_if_absent_svc
        .create(
            &Default::default(),
            &desired_control_service(ns, &control_url),
        )
        .await
    {
        Ok(_) => info!(namespace = %ns, "seeded control MCPService"),
        Err(kube::Error::Api(e)) if e.code == 409 => {}
        Err(e) => return Err(e.into()),
    }
    let classes: Api<v2::AgentClass> = Api::namespaced(ctx.client.clone(), ns);
    match classes
        .create(&Default::default(), &desired_supervisor_class(ns, true))
        .await
    {
        Ok(_) => info!(namespace = %ns, "seeded supervisor AgentClass"),
        Err(kube::Error::Api(e)) if e.code == 409 => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(org = %org.name_any()))]
pub async fn reconcile_org(org: Arc<Organization>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    // Deletion: nothing to unwind by hand — the managed namespaces carry an
    // ownerReference to this org, so GC cascades. No finalizer needed.
    if org.meta().deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let owner = org.controller_owner_ref(&()).ok_or(Error::MissingName)?;
    let org_name = org.name_any();
    let pp = PatchParams::apply(FIELD_MANAGER).force();

    let namespaces = managed_namespaces(&org);
    let ns_api: Api<Namespace> = Api::all(ctx.client.clone());
    for ns in &namespaces {
        let desired = desired_namespace(ns, &org_name, owner.clone());
        ns_api.patch(ns, &pp, &Patch::Apply(&desired)).await?;

        // RBAC mirror: the role ladder + group bindings for kubectl users.
        let roles_api: Api<RbacRole> = Api::namespaced(ctx.client.clone(), ns);
        for role in desired_org_roles(ns) {
            let name = role.metadata.name.clone().expect("named role");
            roles_api.patch(&name, &pp, &Patch::Apply(&role)).await?;
        }
        let bindings_api: Api<RoleBinding> = Api::namespaced(ctx.client.clone(), ns);
        for binding in desired_role_bindings(ns, &org.spec.access_policies) {
            let name = binding.metadata.name.clone().expect("named binding");
            bindings_api
                .patch(&name, &pp, &Patch::Apply(&binding))
                .await?;
        }

        // Platform defaults (control registry entry + supervisor class) —
        // seeded once, then org-owned.
        seed_namespace_defaults(&ctx, ns).await?;

        // The tenant capability plane (P5-1): this org's OWN mcpg gateway,
        // federating its registry. Catalog edits hot-reload via the rendered
        // ConfigMap; the requeue below keeps it converged.
        crate::tenant_mcpg::ensure_tenant_gateway(&ctx, ns, &org_name, &owner).await?;

        let quota_api: Api<ResourceQuota> = Api::namespaced(ctx.client.clone(), ns);
        match desired_quota(ns, org.spec.quotas.as_ref().and_then(|q| q.agents)) {
            Some(quota) => {
                quota_api
                    .patch(QUOTA_NAME, &pp, &Patch::Apply(&quota))
                    .await?;
            }
            None => {
                // A ceiling removed from the spec removes the quota object.
                match quota_api.delete(QUOTA_NAME, &Default::default()).await {
                    Ok(_) => info!(namespace = %ns, "removed org quota (no agent ceiling in spec)"),
                    Err(kube::Error::Api(e)) if e.code == 404 => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }

    let status = OrganizationStatus {
        phase: Some("Ready".to_string()),
        namespaces: namespaces.clone(),
        conditions: vec![org_ready_condition(org.meta().generation, namespaces.len())],
        observed_generation: org.meta().generation,
    };
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch_status(
        &org_name,
        &PatchParams::default(),
        &Patch::Merge(&serde_json::json!({ "status": status })),
    )
    .await?;

    Ok(Action::requeue(requeue_after()))
}

pub fn error_policy_org(_org: Arc<Organization>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    warn!(error = %err, "organization reconcile failed; requeueing");
    Action::requeue(error_backoff())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_api::org::OrganizationSpec;

    fn org(name: &str, yaml: &str) -> Organization {
        Organization::new(
            name,
            serde_yaml::from_str::<OrganizationSpec>(yaml).unwrap(),
        )
    }

    fn owner() -> OwnerReference {
        OwnerReference {
            api_version: "agentctl.dev/v1alpha2".into(),
            kind: "Organization".into(),
            name: "acme".into(),
            uid: "u-1".into(),
            controller: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn managed_namespaces_default_extra_and_unmanaged() {
        // Default (no namespaces block) → the conventional org-<name>.
        assert_eq!(managed_namespaces(&org("acme", "{}")), vec!["org-acme"]);
        // extra spaces append, deduplicated against the conventional one.
        assert_eq!(
            managed_namespaces(&org(
                "acme",
                "namespaces: { mode: managed, extra: [org-acme, acme-scratch] }"
            )),
            vec!["org-acme", "acme-scratch"]
        );
        // unmanaged → the operator touches no namespaces.
        assert!(managed_namespaces(&org("acme", "namespaces: { mode: unmanaged }")).is_empty());
    }

    #[test]
    fn namespace_is_labeled_and_owned() {
        let ns = desired_namespace("org-acme", "acme", owner());
        let labels = ns.metadata.labels.unwrap();
        assert_eq!(labels.get(ORG_LABEL).unwrap(), "acme");
        let owners = ns.metadata.owner_references.unwrap();
        assert_eq!(owners[0].kind, "Organization");
        assert_eq!(
            owners[0].controller,
            Some(true),
            "GC cascades on org delete"
        );
    }

    #[test]
    fn rbac_mirror_roles_ladder_and_exact_group_bindings() {
        let roles = desired_org_roles("org-acme");
        assert_eq!(roles.len(), 3);
        // operator = read + the aggregated management verbs as `create`.
        let operator = &roles[1];
        let rules = operator.rules.as_ref().unwrap();
        assert!(rules[1]
            .resources
            .as_ref()
            .unwrap()
            .contains(&"agents/drain".to_string()));
        assert_eq!(rules[1].verbs, vec!["create"]);

        let o = org(
            "acme",
            r#"
accessPolicies:
  - match: { groups: ["okta:eng-*"] }
    role: operator
    selector: { matchLabels: { team: engineering } }
  - match: { groups: ["okta:platform-admins", "okta:sre"] }
    role: admin
"#,
        );
        let bindings = desired_role_bindings("org-acme", &o.spec.access_policies);
        assert_eq!(
            bindings.len(),
            3,
            "always all three (prunes stale subjects)"
        );
        // The glob group cannot be a K8s Group subject → operator has none.
        assert!(bindings[1].subjects.as_ref().unwrap().is_empty());
        let admin_subjects: Vec<&str> = bindings[2]
            .subjects
            .as_ref()
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(admin_subjects, vec!["okta:platform-admins", "okta:sre"]);
    }

    #[test]
    fn quota_maps_agents_to_cr_count_and_absent_means_none() {
        let q = desired_quota("org-acme", Some(200)).unwrap();
        assert_eq!(q.metadata.name.as_deref(), Some(QUOTA_NAME));
        let hard = q.spec.unwrap().hard.unwrap();
        assert_eq!(hard.get(AGENTS_COUNT_KEY).unwrap().0, "200");
        assert!(desired_quota("org-acme", None).is_none());
    }
}
