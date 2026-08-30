# ADR-0002 — A2A is the primary protocol for conversation and control

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0029, ARCHITECTURE §8

## Context

agentd's only conversation/control surface is its A2A listener: messages, tasks,
SSE streams, typed command DataParts (extensible per workflow via `a2a` starts with
schemas), admin verbs (drain/pause/resume/cancel), gate answers, and per-principal
roles/quotas/labels. There is no served MCP and no bespoke REST on the agent.
Users need one uniform way to reach *any* agent (req. 22), and agents need a
spec-conformant way to reach each other (req. 5).

## Decision

1. **A2A is the wire for every conversation**: user↔supervisor, user↔agent,
   agent↔agent, and operator control verbs. agentctl never invents a parallel
   chat/control protocol.
2. `agentctl-gateway` is the single north–south A2A endpoint; east–west (same org)
   is direct agentd↔agentd A2A over operator-issued mTLS with operator-rendered
   `a2a.peers`/principals; cross-org/cluster traffic goes gateway↔gateway.
3. Inter-agent contracts prefer **typed A2A commands** (`command:` + listener-side
   `schema:`) over prose objectives, so fleet interfaces are schema-checked.
4. The management API (apiserver) remains REST/JSON for CRUD — A2A is for talking
   to *agents*, not for tenancy administration.

## Consequences

- One protocol to secure, observe, and rate-limit at the gateway; agent cards give
  discovery for free.
- Per-user experience (addressed gates, quotas, attribution) requires per-user
  principals at the agent — hence identity-minted principal bearers (ADR-0005,
  RFC 0029).
- A2A conformance is a release gate: we test through the same a2a-rs types agentd
  uses (their oracle pattern).

## Alternatives rejected

- Custom REST/gRPC control API on agents — agentd doesn't serve one; wrapping A2A
  in another protocol loses tasks/streams/gates semantics.
- Routing *all* east–west through the gateway — an unnecessary hop and SPOF inside
  an org's namespace; the gateway stays the boundary crossing, not the LAN.
