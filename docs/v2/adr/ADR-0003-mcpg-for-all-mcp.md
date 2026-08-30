# ADR-0003 — every MCP surface agentctl ships is built on mcpg

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0030, 0031, ARCHITECTURE §3.5

## Context

agentctl needs several MCP surfaces: agent state (agentd's `store.kind: mcp`
checkpointer profile), artifacts, the control/provisioning tools, the work fabric,
sandboxed execution, and HITL. mcpg (`~/mcpg-dev`) is a governed MCP gateway —
declarative tool bindings (http/sql/pipeline backends), a 21-class plugin system
(Apache-2.0 SDK), identity chains (OIDC/JWT/mTLS/SPIFFE/**AAuth RFC 9421
verification**), CEL policy + tool gates, per-caller credential issuers
(`cred://`, incl. RFC 8693 token exchange and ID-JAG), quotas, signed audit,
federation of upstream MCP servers with SSRF guards, approval/HITL machinery,
content-addressed blob store, its own operator + Helm charts. Protocol revisions
2025-11-25 and 2026-07-28 — matching what agentd's rmcp client speaks.

## Decision

1. **We never hand-write an MCP server.** Every MCP surface is an mcpg deployment:
   declarative bindings where possible, small Apache-2.0 `declare_plugin!` cdylibs
   (or static-linked entities) where code is needed (state CAS façade, sandbox
   backend, identity credential issuer, catalog provider).
2. Two deployment tiers: a cluster **system gateway** (state, artifacts, control,
   work fabric) and a **per-org tenant gateway** (federated registry MCPs, sandbox,
   HITL, per-user credentials) — matching mcpg's hard-tenancy model (one process
   per tenant; per-route chain dispatch is explicitly unsupported).
3. agentctl-operator provisions gateways through **mcpg-operator CRs**
   (`MCPGGateway`/`MCPGTenant`/`MCPGServer`); the mcpg-operator chart is a Helm
   dependency.
4. Plugin builds are **pinned in lockstep** with the gateway version (mcpg is
   pre-1.0; the plugin ABI breaks in place), enforced in CI.

## Consequences

- Enormous leverage: auth, policy, quotas, audit, HITL, federation, SSRF guards
  come from mcpg instead of being rebuilt five times.
- mcpg closes agentd's inbound-AAuth gap: agentd signs (RFC 9421) → mcpg verifies.
- New obligations: the MCP *tool façades* over mcpg's internal Store/ContentStore
  traits are ours to build (RFC 0031); BUSL-licensed plugins we rely on
  (oauth-token-exchange, id-jag, SPIFFE workload) need license provisioning in the
  chart — or our Apache CredentialIssuer that delegates to agentctl-identity
  (RFC 0030 chooses the latter).
- Version skew: two pre-1.0 upstreams (agentd, mcpg) — the contract fixtures and
  e2e matrix carry both.

## Alternatives rejected

- rmcp-based bespoke servers: five times the auth/audit/tenancy work, none of the
  governance; contradicts requirement 18.
- One shared mcpg for everything: per-tenant chain isolation is unsupported on a
  shared process; blast radius and noisy-neighbor risks.
