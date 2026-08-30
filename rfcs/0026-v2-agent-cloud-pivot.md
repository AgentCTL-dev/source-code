# RFC 0026 — agentctl v2: the agent-cloud pivot (umbrella)

- **Status:** Proposed
- **Date:** 2026-08-30
- **Authority:** umbrella for the v2 track. The architectural narrative lives in
  [`docs/v2/ARCHITECTURE.md`](../docs/v2/ARCHITECTURE.md); decisions in
  [`docs/v2/adr/`](../docs/v2/adr/); this RFC fixes the scope, the contract
  re-baseline, and the supersession map over RFCs 0001–0025.

## 1. What changes and why

agentctl v1 was "the Kubernetes control plane for conformant agents": CRDs →
pods → flags, a management A2A gateway, fleet sharding, AAuth provisioning.
Two things moved underneath it:

1. **agentd was rewritten** (Aug 2026; AGPL relicense + version reset; now
   v1.3.1). One durable runtime, triggers as workflow start nodes, a directory-
   shaped layered config model, durable workflows/gates/streams, a services
   catalog with closed egress, webhooks, pressure/priority — and **no clustering,
   no served MCP, no execution modes**. Every flag the v1 operator renders exits 2.
2. **The mission grew** (the 32 v2 requirements): from "run conformant agents" to
   **an agent cloud** — per-user supervisor agents, a unified A2A front door,
   brokered identity with on-behalf-of token exchange, an mcpg-based governed
   capability plane, tenancy (org/group/user), registries, full lifecycle
   (backup/migrate), multi-cluster, and a backbone posture for managed offerings.

v2 is therefore a **re-founding, not a patch**: same repo, same Rust stack, same
CRD group — new component set, new contract baseline, new data planes.

## 2. Scope of the v2 track

In scope (each with its RFC):

| Area | RFC |
|---|---|
| Supervisors + control MCP | 0027 |
| Identity service (custody, OBO, AAuth provider, principal mint) | 0028 |
| A2A gateway v2: routing, principals, webhook exposure | 0029 |
| Capability egress: per-org mcpg + credential injection | 0030 |
| Foundational MCP services (state, artifacts, work, sandbox, HITL) | 0031 |
| Registry & config projection | 0032 |
| CRD API v1alpha2 + lifecycle verbs | 0033 |
| Fleets & scaling v2 | 0034 |
| Multi-cluster (hub/spoke) | 0035 (Draft) |

Out of scope for v2.0 (deferred; tracked in `docs/v2/PLAN.md` §Deferred):
an eval/testing service for agent changes, billing/metering export beyond raw
usage metrics, a first-party web console (the thin agentd-ui + CLI carry v2.0),
virtual-cluster tenancy, non-agentd runtimes behind the contract.

## 3. Contract re-baseline (ACC v2)

The vendored contract (`contract/`) is regenerated against agentd v1.3.1 wire
surfaces. Principle P0 (wire-only) is unchanged; the *artifact list* changes:

| ACC v1 artifact | ACC v2 replacement |
|---|---|
| `--capabilities` `surfaces{}` + `contract_version` negotiation | **Config schema** (`config-1.json`, `x-agentd-contract-version`) + **workflow schema** (`workflow-3.json`) + `--capabilities` (informational; missing surfaces tracked as upstream ask U3) |
| `management-profile.json` (served-MCP verbs) | **A2A profile**: methods, admin verbs, command ops, principals model |
| exit-codes table | unchanged in substance; extended with the **intent table** (`complete/terminal/retriable/policy/infra`) that the operator compiles into `podFailurePolicy` |
| metrics registry 1.0 | metrics **schema 1.2**, with the live/reserved split recorded (`agent_inbox_pending` live; `agent_pending_events` reserved-flat) |
| env convention | regenerated (`AGENT_*`/`AGENTD_*` families; `AGENT_INTELLIGENCE`, downward-API names) |
| — | **Store profile**: the checkpointer tool contract (`state.put/get/list/delete`, seq CAS, `_meta agent/idempotency_key = "<key>#<seq>"`) — normative for the state service (RFC 0031) |
| — | **Validation authority**: `agentd --validate-config` exit/output contract used by admission (RFC 0032) |

`agent-contract-client` is rebuilt around this list; conformance fixtures are
re-captured from the v1.3.1 binary; CI runs the renderer's output through the
real `--validate-config`.

## 4. Supersession map (RFCs 0001–0025)

| RFC | Disposition under v2 |
|---|---|
| 0001 stack/repo | **Stands** (reaffirmed by ADR-0012). |
| 0002 substrate/transport | **Stands as amended by 0021**; v2 adds no transports. |
| 0003 Agent/AgentFleet CRDs | **Superseded by 0033** (v1alpha2; conversion per 0005). |
| 0004 AgentClass/IntelligenceService/MCPServerSet | **Superseded by 0032/0033**: AgentClass returns (scoped defaults); MCPServerSet stays dead; ModelPool continues as the intelligence binding. |
| 0005 CRD versioning | **Stands** — its conversion machinery carries v1alpha1→v1alpha2. |
| 0006 operator reconcile | **Amended by 0033** (config projection replaces flag rendering; status via A2A `status`). |
| 0007 admission ladder | **Amended by 0032** (binary-validation step; policy floors from registry). |
| 0009 management path/RBAC | **Superseded by 0028/0029** (identity + gateway carry authn/z; no served-MCP management profile exists to gate). |
| 0010 observability bridge | **Amended** (metrics 1.2 facts; OTLP JSON; audit streams) — carried in ARCHITECTURE §12. |
| 0011 scaling plane | **Superseded by 0034.** |
| 0013 A2A gateway & task store | **Superseded by 0029** (multi-tenant routing, per-user principals, hooks). |
| 0014 mesh identity | **Superseded by 0028** (identity service absorbs PKI SAN scheme + AAuth provider role). |
| 0015 security & multi-tenancy | **Amended by ADR-0010 + ARCHITECTURE §11** (org/group/user model; capability-plane policy moves to mcpg + agentd policies). |
| 0016 CLI/kubectl plugin | **Stands, extended** (§3.8 verb families; conversation verbs via gateway). |
| 0017 release & lifecycle | **Stands, extended by 0033** (agent-level lifecycle verbs added). |
| 0018 codegen/conformance | **Stands** — re-pointed at the ACC v2 artifact list (§3). |
| 0020 instruction sourcing | **Amended by 0032** — instruction documents (agentd's RFC 0034 directive format) become a first-class input; live delivery rides config projection + reload/restart. |
| 0021 contract-2.0 pivot | **Historical**; v2 keeps its HTTPS-everywhere substrate conclusions. |
| 0022 fleet orchestration | **Superseded-in-part by 0034** (agent-side shard identity is gone upstream; the work-fabric and budget layers carry forward). |
| 0023–0025 AAuth track | **Amended by 0028**: agentd now ships enrollment (incl. federated via projected SA tokens) and outbound signing — the "blocking facts" list shrinks; identity absorbs the provider (APD) role; delegation-chain asks remain upstream. |

`rfcs/README.md` gains a v2 banner pointing here.

## 5. Compatibility and migration

- **CRDs**: v1alpha1 objects convert to v1alpha2 (webhook per RFC 0005); fields
  that rendered removed agentd flags (`mode`, shard hints) map to their v2
  equivalents (start-node workflows, fleet partitioning) or convert with warnings.
- **Running v1 fleets**: the v1 operator cannot manage agentd ≥ new-1.x at all
  (flags exit 2), so migration is: install v2 → convert CRs → re-render → rolling
  replace. Old July-era agentd images remain manageable only by the v1 operator;
  no mixed mode is attempted.
- **mock-agent / e2e**: the mock agent is retired where the real agentd (with
  `mock:` intelligence and the in-process LLM) can stand in — the e2e matrix runs
  against the genuine binary.

## 6. Open questions

1. Naming: keep `AgentFleet` vs rename `Fleet` in v1alpha2 (0033 leans keep).
2. Whether the system mcpg also fronts model providers later (LLM gateway) —
   explicitly out of v2.0 per ADR-0004; revisit with data.
3. How much of ACC v2 to publish as a public spec for third-party runtimes vs
   keeping it "agentd-shaped" — PLAN defers the public-spec pass to post-GA.
