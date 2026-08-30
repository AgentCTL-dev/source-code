// SPDX-License-Identifier: Apache-2.0
//! # Organization — the tenancy root (RFC 0033 §2.1)
//!
//! A cluster-scoped CR that anchors a tenant: managed namespaces, quotas, the
//! IdP binding (providers + claim mappings), and **access policies** mapping
//! IdP claims/groups to roles over label selectors. The policy document is
//! resolved per authenticated principal by [`access::resolve`] and consumed by
//! every enforcement point (gateway, apiserver verbs, RBAC mirror) so
//! team-scoped access is stated once and enforced everywhere.
//!
//! Ships at `v1alpha2`: the type is new (no v1alpha1 ever served), so it lands
//! directly on the version the rest of the family migrates to in P2-1.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::Condition;

/// The conventional managed namespace for an organization.
pub fn org_namespace(org: &str) -> String {
    format!("org-{org}")
}

/// One tenant: namespaces, quotas, IdP binding, and access policy.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agentctl.dev",
    version = "v1alpha2",
    kind = "Organization",
    status = "OrganizationStatus",
    shortname = "org",
    shortname = "orgs",
    category = "agentctl",
    printcolumn = r#"{"name":"Display","type":"string","jsonPath":".spec.displayName"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Namespaces","type":"string","jsonPath":".status.namespaces"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationSpec {
    /// Human-facing name (listings, dashboards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Namespace management. Default: managed (`org-<name>` is created and
    /// owned by the Organization — deleting the org deletes its namespaces).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespaces: Option<OrgNamespaces>,
    /// Tenant ceilings. `agents` is enforced as a ResourceQuota on the managed
    /// namespaces; the metering ceilings (tokens/day, sandbox CPU) are recorded
    /// for the billing plane (RFC 0026 §12) and enforced there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quotas: Option<OrgQuotas>,
    /// IdP binding: which issuers authenticate this org's members and how
    /// their claims map to user/groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<OrgIdentity>,
    /// IdP claims/groups → roles over label selectors. Evaluated top-down;
    /// EVERY matching rule grants (a principal holds the union of its grants).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_policies: Vec<AccessPolicy>,
    /// Supervisor provisioning for members: auto (default) | manual | disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisors: Option<SupervisorsMode>,
    /// Registry scoping (the org's defaults class; resolved in P2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_scope: Option<RegistryScope>,
    /// Optional vanity hosts for the tenant gateway endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_hosts: Option<GatewayHosts>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrgNamespaces {
    /// managed (default): the operator creates/owns `org-<name>` (+ `extra`).
    /// unmanaged: namespaces are the platform team's problem; the org only
    /// scopes identity/policy.
    #[serde(default)]
    pub mode: NamespaceMode,
    /// Additional managed namespaces beyond the conventional `org-<name>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NamespaceMode {
    #[default]
    Managed,
    Unmanaged,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrgQuotas {
    /// Ceiling on Agent objects per managed namespace (ResourceQuota
    /// `count/agents.agentctl.dev`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<i64>,
    /// Metering ceiling: model tokens per day (billing plane, not a K8s quota).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_day: Option<i64>,
    /// Metering ceiling: sandbox CPU seconds (billing plane, not a K8s quota).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_cpu_seconds: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrgIdentity {
    /// Issuers whose tokens authenticate members of this org. Matched against
    /// the identity service's configured providers by issuer URL.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<OrgIdentityProvider>,
    /// Claim names carrying the user id and group list (defaults: sub/groups).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_mappings: Option<ClaimMappings>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrgIdentityProvider {
    /// Issuer URL (must be one the identity service federates).
    pub issuer: String,
    /// Optional reference to a client registration (Secret name) for
    /// org-specific client credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimMappings {
    /// Claim carrying the stable user id. Default `sub`.
    #[serde(default = "default_user_claim")]
    pub user: String,
    /// Claim carrying group memberships. Default `groups`.
    #[serde(default = "default_groups_claim")]
    pub groups: String,
}

impl Default for ClaimMappings {
    fn default() -> Self {
        ClaimMappings {
            user: default_user_claim(),
            groups: default_groups_claim(),
        }
    }
}

fn default_user_claim() -> String {
    "sub".into()
}
fn default_groups_claim() -> String {
    "groups".into()
}

/// One policy rule: who ([`PolicyMatch`]) gets which [`Role`] over which
/// agents ([`selector`](AccessPolicy::selector)).
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccessPolicy {
    #[serde(rename = "match")]
    pub match_: PolicyMatch,
    pub role: Role,
    /// Label selector over agents this grant covers. An empty/absent selector
    /// covers everything in the org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<PolicySelector>,
}

/// Principal matcher. Rules with BOTH groups and claims require both to hold;
/// within `groups`, ANY listed pattern matching ANY held group suffices.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyMatch {
    /// Group patterns; `*` is a wildcard (e.g. `okta:eng-*`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Exact claim requirements (claim → required value; a list-valued claim
    /// matches when it contains the value).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub claims: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicySelector {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub match_labels: BTreeMap<String, String>,
}

/// Roles, weakest → strongest. `viewer` reads specs/status; `operator` adds
/// converse + lifecycle verbs; `admin` adds CRUD, registry writes, org
/// settings. Token scopes may narrow a session BELOW the policy ceiling,
/// never above it.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Viewer,
    Operator,
    Admin,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SupervisorsMode {
    #[default]
    Auto,
    Manual,
    Disabled,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegistryScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_ref: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHosts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationStatus {
    /// Coarse phase: `Provisioning` | `Ready` | `Degraded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Managed namespaces the operator has reconciled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

// ===========================================================================
// Access resolution — the pure policy engine every enforcement point calls.
// ===========================================================================

pub mod access {
    use super::{AccessPolicy, Role};
    use std::collections::BTreeMap;

    /// One resolved grant: a role over a label scope. `match_labels: None`
    /// means the grant covers everything in the org.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Grant {
        pub role: Role,
        pub match_labels: Option<BTreeMap<String, String>>,
    }

    /// The authenticated principal's identity facts, as the identity service
    /// resolves them (provider-prefixed groups, flattened claims).
    #[derive(Debug, Clone, Default)]
    pub struct PrincipalFacts {
        pub groups: Vec<String>,
        pub claims: BTreeMap<String, serde_json::Value>,
    }

    /// Resolve the full grant set for a principal: every matching rule grants
    /// (union semantics — policies broaden, they never veto each other).
    pub fn resolve(facts: &PrincipalFacts, policies: &[AccessPolicy]) -> Vec<Grant> {
        policies
            .iter()
            .filter(|p| matches(facts, p))
            .map(|p| Grant {
                role: p.role,
                match_labels: p
                    .selector
                    .as_ref()
                    .filter(|s| !s.match_labels.is_empty())
                    .map(|s| s.match_labels.clone()),
            })
            .collect()
    }

    /// Does any grant authorize `role` over an object with `labels`?
    /// A stronger role satisfies a weaker requirement (admin ⊇ operator ⊇
    /// viewer); a scoped grant applies only when every selector label matches.
    pub fn permits(grants: &[Grant], role: Role, labels: &BTreeMap<String, String>) -> bool {
        grants.iter().any(|g| {
            g.role >= role
                && match &g.match_labels {
                    None => true,
                    Some(sel) => sel.iter().all(|(k, v)| labels.get(k) == Some(v)),
                }
        })
    }

    fn matches(facts: &PrincipalFacts, policy: &AccessPolicy) -> bool {
        let m = &policy.match_;
        // A rule matching nothing at all grants nothing (a footgun guard: an
        // empty match would otherwise hand its role to every principal).
        if m.groups.is_empty() && m.claims.is_empty() {
            return false;
        }
        let groups_ok = m.groups.is_empty()
            || m.groups
                .iter()
                .any(|pat| facts.groups.iter().any(|g| glob_match(pat, g)));
        let claims_ok = m.claims.iter().all(|(claim, required)| {
            facts.claims.get(claim).is_some_and(|held| match held {
                serde_json::Value::String(s) => s == required,
                serde_json::Value::Array(items) => items
                    .iter()
                    .any(|i| i.as_str().is_some_and(|s| s == required)),
                // Number/bool claims match their canonical JSON spelling.
                other => {
                    let held = other.to_string();
                    held == *required
                }
            })
        });
        groups_ok && claims_ok
    }

    /// `*`-wildcard match (no other metacharacters). Linear-time two-pointer
    /// scan with backtracking to the last star — no regex, no pathological
    /// inputs.
    pub fn glob_match(pattern: &str, value: &str) -> bool {
        let (p, v): (Vec<char>, Vec<char>) = (pattern.chars().collect(), value.chars().collect());
        let (mut pi, mut vi) = (0usize, 0usize);
        let (mut star, mut mark) = (None::<usize>, 0usize);
        while vi < v.len() {
            if pi < p.len() && (p[pi] == v[vi]) {
                pi += 1;
                vi += 1;
            } else if pi < p.len() && p[pi] == '*' {
                star = Some(pi);
                mark = vi;
                pi += 1;
            } else if let Some(s) = star {
                pi = s + 1;
                mark += 1;
                vi = mark;
            } else {
                return false;
            }
        }
        while pi < p.len() && p[pi] == '*' {
            pi += 1;
        }
        pi == p.len()
    }
}

#[cfg(test)]
mod tests {
    use super::access::{glob_match, permits, resolve, Grant, PrincipalFacts};
    use super::*;
    use serde_json::json;

    fn policy(json: serde_json::Value) -> AccessPolicy {
        serde_json::from_value(json).unwrap()
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn org_namespace_is_conventional() {
        assert_eq!(org_namespace("acme"), "org-acme");
    }

    #[test]
    fn spec_parses_the_rfc_example() {
        let spec: OrganizationSpec = serde_yaml::from_str(
            r#"
displayName: "Acme Corp"
namespaces: { mode: managed }
quotas: { agents: 200 }
identity:
  providers: [{ issuer: https://acme.okta.com }]
  claimMappings: { user: sub, groups: groups }
accessPolicies:
  - match: { groups: ["okta:eng-*"] }
    role: operator
    selector: { matchLabels: { team: engineering } }
  - match: { claims: { dept: marketing } }
    role: viewer
    selector: { matchLabels: { team: marketing } }
  - match: { groups: ["okta:platform-admins"] }
    role: admin
    selector: {}
supervisors: auto
"#,
        )
        .unwrap();
        assert_eq!(spec.display_name.as_deref(), Some("Acme Corp"));
        assert_eq!(spec.namespaces.unwrap().mode, NamespaceMode::Managed);
        assert_eq!(spec.quotas.unwrap().agents, Some(200));
        assert_eq!(spec.access_policies.len(), 3);
        assert_eq!(spec.access_policies[2].role, Role::Admin);
        assert_eq!(spec.supervisors, Some(SupervisorsMode::Auto));
    }

    #[test]
    fn glob_matches_prefix_suffix_and_literal() {
        assert!(glob_match("okta:eng-*", "okta:eng-platform"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(glob_match("okta:platform-admins", "okta:platform-admins"));
        assert!(!glob_match("okta:eng-*", "okta:marketing"));
        assert!(!glob_match("okta:eng-*", "auth0:eng-x"));
        assert!(!glob_match("a*c", "ab"));
    }

    /// The RFC's worked example: engineering operates engineering, marketing
    /// views marketing, admins see all.
    #[test]
    fn rfc_worked_example_resolves_correctly() {
        let policies = vec![
            policy(json!({
                "match": { "groups": ["okta:eng-*"] },
                "role": "operator",
                "selector": { "matchLabels": { "team": "engineering" } },
            })),
            policy(json!({
                "match": { "claims": { "dept": "marketing" } },
                "role": "viewer",
                "selector": { "matchLabels": { "team": "marketing" } },
            })),
            policy(json!({
                "match": { "groups": ["okta:platform-admins"] },
                "role": "admin",
            })),
        ];

        let eng = PrincipalFacts {
            groups: vec!["okta:eng-platform".into()],
            ..Default::default()
        };
        let eng_grants = resolve(&eng, &policies);
        assert_eq!(eng_grants.len(), 1);
        // Engineering operates engineering agents…
        assert!(permits(
            &eng_grants,
            Role::Operator,
            &labels(&[("team", "engineering")])
        ));
        // …reads them too (operator ⊇ viewer)…
        assert!(permits(
            &eng_grants,
            Role::Viewer,
            &labels(&[("team", "engineering")])
        ));
        // …but marketing agents are refused, even for viewing.
        assert!(!permits(
            &eng_grants,
            Role::Viewer,
            &labels(&[("team", "marketing")])
        ));

        let marketing = PrincipalFacts {
            claims: [("dept".to_string(), json!("marketing"))].into(),
            ..Default::default()
        };
        let mk_grants = resolve(&marketing, &policies);
        // Marketing views marketing but cannot operate it.
        assert!(permits(
            &mk_grants,
            Role::Viewer,
            &labels(&[("team", "marketing")])
        ));
        assert!(!permits(
            &mk_grants,
            Role::Operator,
            &labels(&[("team", "marketing")])
        ));

        let admin = PrincipalFacts {
            groups: vec!["okta:platform-admins".into()],
            ..Default::default()
        };
        let admin_grants = resolve(&admin, &policies);
        // Admin sees and operates everything, labeled or not.
        assert!(permits(&admin_grants, Role::Admin, &labels(&[])));
        assert!(permits(
            &admin_grants,
            Role::Operator,
            &labels(&[("team", "marketing")])
        ));
    }

    #[test]
    fn union_semantics_and_list_claims() {
        let policies = vec![
            policy(json!({
                "match": { "groups": ["eng"] },
                "role": "viewer",
                "selector": { "matchLabels": { "team": "a" } },
            })),
            policy(json!({
                "match": { "claims": { "roles": "sre" } },
                "role": "operator",
                "selector": { "matchLabels": { "team": "b" } },
            })),
        ];
        // The list-valued claim matches by containment.
        let facts = PrincipalFacts {
            groups: vec!["eng".into()],
            claims: [("roles".to_string(), json!(["dev", "sre"]))].into(),
        };
        let grants = resolve(&facts, &policies);
        assert_eq!(grants.len(), 2, "both rules grant; union, no veto");
        assert!(permits(&grants, Role::Viewer, &labels(&[("team", "a")])));
        assert!(permits(&grants, Role::Operator, &labels(&[("team", "b")])));
        assert!(!permits(&grants, Role::Operator, &labels(&[("team", "a")])));
    }

    #[test]
    fn empty_match_grants_nothing_and_both_legs_must_hold() {
        let empty = policy(json!({ "match": {}, "role": "admin" }));
        let anyone = PrincipalFacts {
            groups: vec!["whoever".into()],
            ..Default::default()
        };
        assert!(
            resolve(&anyone, &[empty]).is_empty(),
            "an empty matcher must never hand out admin"
        );

        // groups AND claims in one rule: both must hold.
        let both = policy(json!({
            "match": { "groups": ["eng"], "claims": { "dept": "platform" } },
            "role": "operator",
        }));
        let only_group = PrincipalFacts {
            groups: vec!["eng".into()],
            ..Default::default()
        };
        assert!(resolve(&only_group, std::slice::from_ref(&both)).is_empty());
        let full = PrincipalFacts {
            groups: vec!["eng".into()],
            claims: [("dept".to_string(), json!("platform"))].into(),
        };
        assert_eq!(resolve(&full, &[both]).len(), 1);
    }

    #[test]
    fn scoped_grant_requires_every_selector_label() {
        let grants = vec![Grant {
            role: Role::Operator,
            match_labels: Some(labels(&[("team", "eng"), ("tier", "prod")])),
        }];
        assert!(permits(
            &grants,
            Role::Operator,
            &labels(&[("team", "eng"), ("tier", "prod"), ("extra", "x")])
        ));
        // One selector label missing on the object → refused.
        assert!(!permits(
            &grants,
            Role::Operator,
            &labels(&[("team", "eng")])
        ));
    }
}
