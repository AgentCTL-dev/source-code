# RFC 0030 — the capability egress: per-org mcpg with identity injection

- **Status:** Proposed
- **Date:** 2026-08-30
- **Decisions:** ADR-0003, ADR-0004 · **Depends on:** RFC 0028 (exchange), 0032
  (registry → catalog), 0031 (tool surfaces it hosts)

## 1. Motivation

Agents need tools whose credentials are (a) per-user and per-moment (OBO), and
(b) governed per tenant — catalogs, quotas, audit, approval gates (req. 12, 17,
26). Direct dial cannot inject a *changing* credential or enforce an org
catalog; a static relay adds nothing (v1.3.0's lesson). The per-org **tenant
mcpg gateway** exists precisely for the delta: verified caller identity in,
fresh scoped credential out, policy in between.

## 2. Topology

One mcpg gateway per organization (`mcpg-<org>` in the org namespace),
provisioned by the operator via mcpg-operator CRs (`MCPGGateway` + `MCPGTenant`
+ per-workload `MCPGServer` where we host in-cluster servers). mcpg's own
guidance is followed: hard tenancy = one process per tenant; a shared gateway's
per-route chain isolation is explicitly unsupported upstream.

The **system mcpg** (RFC 0031) is separate: cluster-scoped foundational tools
(state/artifacts/control/work) on their own availability and latency budget —
an org-gateway outage never takes checkpointing down.

## 3. Caller identity (agent → gateway)

- agentd signs every MCP request (AAuth RFC 9421) under its enrolled identity;
  mcpg's AAuth identity plugin verifies against agentctl-identity's JWKS →
  `PluginIdentity {subject: agent:<org>/<name>, attributes: {org, class…}}`.
- `_meta agent/acting_for` + `agent/labels` (agentd propagates the run's
  principal) ride into policy/audit and into the credential key.
- Transport additionally mTLS inside the namespace; NetworkPolicies admit only
  org agents.

## 4. Credential injection (`cred://`)

Per upstream target, the federation/binding declares a credential mode from the
registry entry (`MCPService.spec.auth`):

| Mode | Mechanism |
|---|---|
| `service` | static/service secret (K8s Secret ref) injected by mcpg; no user context |
| `obo` | `cred://agentctl-identity/<audience>` → **our Apache-2.0 `CredentialIssuer` plugin** calls identity `/exchange` with (verified agent identity, `acting_for`, audience) → short-lived scoped token; host-side cache + proactive refresh; `connection_required` surfaces as an approval card (consent URL) |
| `passthrough` | forbidden by default (mcpg strips inbound auth headers at egress); enabled only for explicitly-marked same-trust targets |

We ship our own issuer plugin rather than relying on mcpg's BUSL
`oauth-token-exchange`/`oauth-id-jag` plugins: custody and policy live in
agentctl-identity anyway, the issuer is a thin client, and the chart then needs
no mcpg license token for the core path (mcpg BUSL plugins remain usable where
licensed).

## 5. Governance at the gateway

Rendered from the same registry entries that produce each agent's
`services.yaml` (one source of truth, enforced twice):

- **Catalog**: federations with prefixes + include/exclude filters; per-tool CEL
  visibility (`identity.attributes.org`, group claims) so `tools/list` shows a
  caller only its world; SSRF-guarded upstreams (`allow_private_backends: false`
  except cluster-internal allowlisted services).
- **Quotas**: mcpg quota policies per identity (agent) and per `acting_for`
  user; rate + concurrency + budget counters cluster-backed.
- **Gates**: approval policies on designated tools (first-use, destructive
  verbs) via mcpg `PendingApproval` + notifiers — complementing agentd's own
  `security.policies` (`ask`) on the agent side; org chooses which layer asks.
- **Audit**: signed audit chain per org; every record carries agent, user
  (`acting_for`), tool, upstream, decision, latency.

## 6. Fit with agentd's egress model

Each agent's rendered `services:` entry for a governed capability points at its
org gateway endpoint with `allow:` tool ceilings and tag floors; `security.
egress: closed` + NetworkPolicy make the gateway (plus model providers, peers,
system mcpg) the agent's entire reachable world. Where a registry entry is
marked `direct: true` (service-credential, latency-critical, no user context),
the agent dials the upstream directly — the catalog still renders tags/ceilings
and netpol; ADR-0004's burden of proof applies to moving anything behind the
gateway.

## 7. Failure and performance posture

- Exchange cache keeps OBO off the hot path (target: ≤1 extra RTT amortized;
  cold exchange budget ≤300 ms in-cluster).
- Gateway unavailability degrades *capability*, never *durability* (state is on
  the system tier) and never *conversation* (A2A path independent).
- Per-org HPA on mcpg; breaker/rate defaults come from the registry entry and
  are mirrored into agents' step-level `breaker:`/`rate:` guidance.

## 8. Alternatives considered

Sidecar injectors; identity-aware direct dial; one shared multi-tenant gateway —
all rejected in ADR-0004/0003 with reasons recorded there.

## 9. Open questions

1. Whether `hitl.*`/`sandbox.*` live on the tenant gateway (as designed) or get
   their own per-org deployment when noisy (PLAN watches p95s).
2. DLP/content-inspection transform plugins per org — post-v2.0 extension point
   (mcpg `Transform` class is ready for it).
3. Streaming tool results end-to-end (mcpg streaming backend → agentd tool
   result): agentd consumes final results only today — upstream conversation.
