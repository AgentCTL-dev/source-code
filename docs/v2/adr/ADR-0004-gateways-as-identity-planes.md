# ADR-0004 — gateways return, as identity data planes

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0028, 0029, 0030; supersedes-in-part the v1.3.0 direct-dial note in rfcs/README.md

## Context

agentctl v1.3.0 removed the ModelGateway and MCPGateway: they were static
protocol/credential relays whose job (mount a key, forward bytes) was better done
by direct dial. That reasoning still holds — and is unchanged for **intelligence**
(agents keep dialing model providers directly via ModelPool).

The v2 requirements add something a mounted key cannot do: **per-user, per-moment
credentials.** A durable agent acting for a human across days needs each MCP call
to carry a *fresh, audience-scoped token for that human* (OBO), without the agent
ever holding a refresh token. That is inherently a data-plane function between
agent and capability — plus governance (catalog, quotas, audit) that must bind to
the verified caller.

## Decision

1. **Direct dial stays the rule for intelligence** and for MCP services that need
   only a service credential and no per-user identity.
2. **The capability path gains a gateway when identity must be injected or policy
   enforced per caller**: the per-org mcpg tenant gateway, whose credential
   issuers call `agentctl-identity` for RFC 8693/ID-JAG exchanges per request
   (RFC 0030). The A2A front door (`agentctl-gateway`) is the analogous identity
   plane for conversations (per-user principals — RFC 0029).
3. These gateways are justified **only** by what they add (identity, governance,
   audit); any future proposal to route traffic through them "for convenience"
   inherits the v1.3.0 burden of proof.

## Consequences

- Two hops on governed tool calls; none on model calls. Latency budget documented
  per path; mcpg's credential cache keeps exchange off the hot path.
- The old RFCs' "gateways removed" note stands for what it removed; rfcs/README
  gains a banner distinguishing *credential-static* gateways (gone) from
  *identity-injecting* gateways (new).

## Alternatives rejected

- Agents hold user refresh tokens (mounted): violates custody (a compromised agent
  = permanent account takeover), rotation, and least-privilege; unacceptable.
- Sidecar token injectors per agent pod: N sidecars ≈ the gateway's job with worse
  cache locality, no shared audit, and per-pod secret custody.
- Identity-aware direct dial (agent asks identity for each token itself): puts
  bearer tokens inside the (least-trusted) agent process and bypasses catalog
  governance; kept only as an explicitly-flagged escape hatch for self-hosted
  advanced users.
