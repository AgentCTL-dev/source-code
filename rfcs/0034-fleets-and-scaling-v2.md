# RFC 0034 — fleets and scaling v2

- **Status:** Proposed
- **Date:** 2026-08-30
- **Supersedes:** RFC 0011; supersedes-in-part RFC 0022 (agent-side sharding
  premise removed upstream) · **Decisions:** ADR-0009
- **Depends on:** RFC 0031 (`work.*`), 0033 (AgentFleet), 0029 (fleet routes)

## 1. Motivation

agentd deleted clustering: no shards, claims, or standby pools — "a fleet
partitions upstream," and the deploy system owns which replica arms what. That
system is agentctl (req. 9). v1's RFC 0022 fabric (queue/DLQ, result flow,
fleet A2A, per-fleet budgets) survives; its shard-identity half is re-founded
here as **renderer-level partitioning**.

## 2. AgentFleet

```yaml
apiVersion: agentctl.dev/v1alpha2
kind: AgentFleet
metadata: { name: triage, namespace: org-acme }
spec:
  replicas: 3                      # or autoscale: {min, max, metric}
  template: { spec: <AgentSpec> }  # one agent spec for all members
  partitioning:
    strategy: static | dispatcher | workqueue
    static:
      vars: { partition: "{{ordinal}}" }     # rendered into each member's vars:
      singletons: [nightly-report]           # workflows armed on ordinal 0 only
    dispatcher:
      dispatcher: { overrides: {...} }       # the owner instance (max_runs: 1)
      workers:    { replicas: 3 }
    workqueue:
      queues: [tickets]                      # work.* queues (RFC 0031 §4)
      leaseTtl: 60s
  budget: { windows: [...] }                 # per-fleet ceiling (fabric-metered)
  rollout: { maxUnavailable: 1, canary: {partitions: [0]} }
status:
  members: [...]  partitionsHealthy: 3/3  queueDepth: …  phase: …
```

## 3. The three strategies (mechanics)

**static** — each member gets a unique `agent.name` (ordinal) and an overlay
`vars:` map; workflow documents reference `{{config.partition}}` in URIs/
filters. *Singletons*: the renderer arms listed workflows (`armed: true`) only
on ordinal 0 — agentd's own "replica 0 arms the schedule" doctrine, expressed
declaratively. **Resize is a guarded re-partition**: scale-out adds partitions
(no movement); scale-in or re-mapping drains affected members first (the v1
stop-the-world lesson: never re-map a live partition). Best for: partitioned
sources (queue-per-partition, per-region resources).

**dispatcher** — one owner member (subscription holder, `concurrency.max_runs:
1`) plus a worker pool; the operator wires peers + principals; the fleet's A2A
route (RFC 0029) targets the dispatcher. The dispatcher is a deliberate SPOF
(agentd's guidance) made cheap: managed store ⇒ restart = restore. Best for:
one upstream subscription, elastic workers.

**workqueue** — members are identical consumers of `work.*` leases; ownership
lives in the fabric (TTL lease; crashed member ⇒ lease expiry ⇒ redelivery).
At-least-once end-to-end: member workflows use item-id idempotency (composing
with agentd's derived keys); DLQ + replay via CLI. Best for: high-volume
heterogeneous work, scale-from-zero.

## 4. Autoscaling (`agentctl-scaler`)

Signals (the *live* set, per the v1.3.1 metrics analysis): `agent_inbox_pending`
(primary), `agent_turns_queued`, `agent_pressure_level` (shed = hard ceiling
reached — scale out, don't push), `work.*` queue depth (workqueue strategy),
webhook 429 rates at the gateway. Anti-signals: breaker-open counts (scaling
out multiplies probes against a down dependency — the scaler damps while
breakers are open); `agent_pending_events` and friends are reserved-flat
upstream and must not be targeted.

Scale-from-zero: webhook/a2a-triggered agents park their routes at the gateway;
first delivery wakes the workload (gateway holds with 503+Retry-After or queues
per route policy within a small budget); schedule-driven members never scale to
zero unless their singleton moves.

Vertical guidance: agentd's own knobs (`limits.max_runs` global 8 default,
per-workflow concurrency, `max_parallel_turns`, fan-out caps) are class-tunable
and documented as the first lever before replicas.

## 5. Budgets

Per-fleet budgets meter at the fabric/apiserver from agentd's usage surfaces
(harness-tracked lineage of RFC 0025): member `agent_tokens_total` +
`budget.*` events aggregate into fleet windows; breach → policy (alert, pause
intake, refuse scale-out). Per-member budgets stay in agentd config (its
governor enforces locally).

## 6. What carries over from RFC 0022

The work fabric (queue/DLQ, result flow), the fleet A2A endpoint concept, and
per-fleet budgets — re-implemented on the v2 substrate (fabric as `work.*`
tools, fleet route at the gateway). The per-item **ownership epoch** remains an
upstream/data-plane idea (noted there) — the TTL-lease + idempotency pairing is
the v2 answer.

## 7. Open questions

1. Cross-partition rebalancing telemetry (hot-partition detection) — observe
   first (dashboards), automate later.
2. Fleet-level `SubscribeToEvents` fan-in (unified live view) — dispatcher-only
   in v2.0 (RFC 0029 §9).
3. KEDA integration as an alternative scaler backend — deferred; native scaler
   first (Rust-only, and our signals need the damping logic).
