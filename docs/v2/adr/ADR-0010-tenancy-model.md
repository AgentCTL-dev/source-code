# ADR-0010 — tenancy is organization → group → user, above the agent

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0028, 0032, 0033; ARCHITECTURE §4

## Context

Requirements 7, 21, 26, 27: users belong to organizations/groups; several users
manage shared fleets; per-tenant registries; the whole thing must serve as a
multi-tenant backbone for managed offerings. agentd is deliberately not
multi-tenant ("the answer to a different caller needs a different surface is a
different process"); mcpg's hard tenancy is one gateway per tenant; Kubernetes'
tenancy primitive is the namespace.

## Decision

1. **Organization = the tenancy root**: an `Organization` CR maps to dedicated
   namespace(s) (`org-<slug>`), a ResourceQuota, a tenant mcpg gateway, a registry
   scope, and IdP claim-mapping rules (issuer + group globs → membership/roles).
2. **Groups are roles, not spaces**: named role bindings (viewer/operator/admin)
   optionally narrowed by label selectors over agents/fleets — how several users
   share a fleet. Enforced at the apiserver and gateway (and mirrored to K8s RBAC
   for direct kubectl users). "Org/group/user" is the generic frame, not a fixed
   org chart: the binding mechanism is **IdP claims/groups/scopes →
   `accessPolicies` → roles over label selectors** (RFC 0033 §Organization), so
   whatever structure the company centralizes in Okta/Auth0/Keycloak maps
   directly — engineering operates `team: engineering` agents, marketing cannot,
   platform-admins hold an empty selector (everything).
3. **Users are IdP subjects**: no local accounts; per-user artifacts are their
   supervisor, their credential connections, their registry scope, and their
   minted A2A principals.
4. **Agents are single-tenant by construction**: one org, one namespace, one
   catalog; per-user distinction *at* an agent is carried by principals/labels
   (attribution, quotas, addressed gates), never by giving one agent two tenants.
5. Isolation stack per org: namespace + NetworkPolicy + tenant gateway process +
   registry scope + quota + PKI SAN namespace.

## Consequences

- A managed-service builder maps their customers to Organizations 1:1 (req. 27).
- Cross-org sharing is an explicit registry export/import, never ambient.
- Namespace-per-org keeps counts sane at scale (thousands of orgs ⇒ phase-M
  virtual-cluster options noted in RFC 0035, deferred).

## Alternatives rejected

- Namespace-per-user: supervisor sprawl (quota objects, netpols × users) with no
  isolation win — user separation is identity-level, not workload-level.
- Cluster-per-org as the default: operationally heavy; kept as a *supported
  profile* under multi-cluster (RFC 0035) for regulated tenants.
- Agent-level multi-tenancy: contradicts agentd's design and our audit model.
