# ADR-0009 — fleet coordination lives upstream of the agent

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0034; supersedes the agent-side premise of RFC 0022; ARCHITECTURE §10

## Context

agentctl v1's fleet orchestration (RFC 0022) leaned on agentd-side shard identity
and work-claim flags. The agentd rewrite **removed clustering entirely** — every
shard/claim flag now exits 2 — with an explicit rationale: exactly-one-owner must
live where it can be arbitrated (the source of the work, or a shared store), and
"replica 0 arms the schedule" is one line of deployment config. agentd documents
three sanctioned fleet shapes: partition at the source, dispatcher + A2A workers,
and queue-owned leases called from workflow steps. Store identity (`agent.name` +
seq-CAS) is a split-brain fence, not a distributor.

## Decision

1. **agentctl is the partitioner.** `AgentFleet.spec.partitioning.strategy` selects
   one of the three sanctioned shapes; the operator implements it:
   - `static` — per-replica config overlays (`vars: {partition}`), incl. the
     "only replica 0 arms the schedule" rule; resizes are guarded re-partitions
     (drain-then-move).
   - `dispatcher` — one owner agent (`max_runs: 1`) + a worker pool, peers wired.
   - `workqueue` — the coordination work fabric (`work.*` MCP tools with TTL
     leases) owns ownership; agents run `lease → work → ack` workflows.
2. **agentctl-coordination is re-founded** as the work-fabric tool surface on the
   system mcpg (its Postgres queue/DLQ machinery survives; its agent-side shard
   assumptions do not).
3. Store identity discipline is enforced by the renderer: `agent.name` unique per
   replica (downward API), file stores never shared, `managed` store for anything
   that must survive rescheduling.

## Consequences

- RFC 0022's fabric and budget work carries forward; its shard-identity half is
  formally superseded.
- Semantics are at-least-once end-to-end; the design leans on agentd's derived
  idempotency keys and the fabric's item-id dedup, documented per strategy.
- Scheduling singletons ("nightly report across a 3-replica fleet") are a
  *renderer* concern (static strategy) — simple, inspectable, testable.

## Alternatives rejected

- Re-adding sharding to the agent (fork/patches): upstream deleted it on the
  merits; ADR-0001 forbids forks.
- A leader-election sidecar per fleet: reintroduces agent-adjacent coordination
  state that the store/queue already arbitrates better.
