# agentctl RFC 0011: Scaling plane — claim/shard regimes, the KEDA external scaler & the reference coordination MCP server

**Status:** Proposed (agentctl scaling track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the elastic plane that turns one `AgentFleet` into N reactive workers without double-processing, scales them on a backlog signal (including from zero), and drains them down without dropping in-flight work

> **agentctl invents no claim protocol and runs no autoscaler in the data plane —
> it owns the *policy*, the contract owns the *primitives* (P0).** The cross-replica
> ownership rule (the work-claim lease, the shard predicate, the autoscaling-signal
> set, the drain-release step) is **frozen by the contract** (the reference
> implementation's agent RFC 0019, the `work.*` names in agent RFC 0015 §5.6, the
> metric set in agent RFC 0016 §4.3) and honoured by *any* conformant agent.
> agentctl supplies the Kubernetes-shaped halves the agent refuses to learn: the
> **KEDA external scaler** (`crates/scaler`), the **reference coordination MCP
> server** (the atomic-lease + dedupe backbone agentctl ships and operates), the
> **shard-resize controller**, and the **warm-pool** orchestration. Where this RFC
> names a concrete surface it cites the reference implementation (agent RFCs) as
> *where the contract is presently written down*, never as a dependency.

> **KEDA owns `.spec.replicas`; agentctl never fights the HPA.** For an elastic
> (claim) fleet the operator renders a workload that **omits** `.spec.replicas`
> (agentctl RFC 0003 §4.3, RFC 0006 §8.4) and the KEDA-generated HPA is the sole
> writer. For a (shard) fleet the replica count *is* `N` and is operator-owned;
> KEDA is paused for the resize window. There is **exactly one writer** of the
> replica field at any instant (§5.4).

> **The agent never learns it is in a fleet.** It claims one item, shards one key
> space, emits one instance's gauges, and drains itself on SIGTERM. Fan-out,
> aggregation, scale decisions, victim selection, and rebalance are **exclusively
> agentctl's** (agent RFC 0016 §7.3, agent RFC 0014 §6 non-goals). A
> `"cluster":false` agent degrades to a single, un-scaled singleton (§9).

---

## 1. Problem / Context

agentctl must horizontally scale **reactive** fleets — N workers all subscribed to
one MCP work source — and the moment there is a second replica, the contract's
own correctness guarantee turns against us:

- **Replicating the worker replicates the owner.** agent RFC 0008 §2.2 makes one
  instance an exactly-one-owner: every inbound
  `notifications/resources/updated{uri}` matches **exactly one** route, in order,
  no fan-out. That guarantee is **intra-instance**. Run two reactive Deployments
  on `file:///inbox/*.json` and the source server delivers the *same* notification
  to *both* connections — each routes it to its own owning route and processes it.
  R replicas = R processings = **R× duplicate side effects, not R× throughput**
  (agent RFC 0019 §1). A naive KEDA scale 1→10 is a correctness bug, not a win.

- **The pods are networkless.** Under the locked substrate decision (agentctl RFC
  0002) the production multi-tenant tier (Kata-hybrid) and the portable off-pod
  tier run the agent with **no pod IP and no shell**. The serializing point that
  resolves cross-replica contention is reached over the substrate — not a TCP dial
  — and the autoscaling signal cannot be a per-pod scrape at the source (§5, §6).

- **Scale-from-zero is the headline elastic property and the hardest one.** A
  reactive worker idles at near-zero CPU between events (agent RFC 0008 §3.1.3),
  so the natural floor is **zero pods**. But every per-replica gauge the agent
  emits — `agent_pending_events`, `agent_reaction_lag_ms` (agent RFC 0016 §4.3)
  — emits **nothing at replica 0** (no pod exists to emit it). The signal that
  there is *pending work when zero workers run* must come from **off the pods** —
  the work store itself (§5.3, contract ask **P9**).

- **Scale-down must not drop a held claim or in-flight work.** An HPA deleting a
  replica must let that pod **release its claims** and **bleed in-flight work**
  before exit, or the fleet loses an item until a lease TTL elapses (agent RFC
  0019 §6). The drain choreography and the workload kind interact (a StatefulSet
  scale-down deletes the highest ordinal; a Deployment can pick the least-loaded
  victim, agentctl RFC 0003 §4.2).

The contract already solves the **data-plane** half: a work-claim lease that makes
exactly-one-owner hold *across* instances (agent RFC 0019 §3), a static shard
predicate (§4), an honest autoscaling-signal surface (§5), and a drain that
releases claims (§6). **None of the Kubernetes half exists until agentctl builds
it**, and several of the surfaces the data plane assumes are unbuilt or
unreconciled. This RFC owns that half and names the gaps as contract asks.

### 1.1 What this RFC owns vs. reuses

| Concern | Owner | This RFC's role |
|---|---|---|
| `AgentFleet.spec` (`scaling.mode`, `target`, `work.*`, `drain`/`grace`) | **agentctl RFC 0003 §4** | consume; never re-define the schema |
| Mode→workload render; `ScaledObject` render; **KEDA-owns-replicas** field-ownership | **agentctl RFC 0006 §6/§8.4** | specify the **trigger detail** the operator renders; own the resize handoff |
| The claim *lifecycle* the agent honours; the shard predicate; the drain-release step | **agent RFC 0019** (contract) | reuse verbatim; build the **server** + scaler + resize controller around it |
| `work.*` tool names + `_meta` + `claim_key` | **agent RFC 0015 §5.6** (frozen) | the reference coordination server implements the **server side** of this |
| Frozen autoscaling-signal metric set; the **P10** name defect | **agent RFC 0016 §4.3 / agentctl RFC 0010 §5.4** | author triggers against the **frozen** names only; reconcile via P10 |
| The Tier A telemetry bridge / scrape-proxy / `agent://capacity` read | **agentctl RFC 0008 / RFC 0010** | consume for in-pod refinement + victim selection |
| Networkless **egress** (the agent's outbound MCP/claim connection) | **agentctl RFC 0002 / RFC 0012** | route the claim transport over the egress path (§3.4) |
| **KEDA external scaler** (`crates/scaler`, `tonic`) | **this RFC** (built per agentctl RFC 0001) | `externalscaler.proto` server reading the off-pod backlog |
| **Reference coordination MCP server** | **this RFC** | atomic lease + `claim_key` dedupe + off-pod backlog count (P9) |
| **Shard-resize controller** | **this RFC** | the drain-and-reassign rolling restart on an `N` change |
| **Warm-pool / standby** orchestration | **this RFC** | the floored sub-pool + assignment policy |

---

## 2. Decision

1. **`AgentFleet` renders to one of two topologies, selected by `scaling.mode`
   (agentctl RFC 0003 §4.2), because two scaling regimes cannot share one
   topology.** KEDA wanting to jiggle `.spec.replicas` continuously and `--shard
   K/N` requiring `N` to be a fleet-consistent immutable config (agent RFC 0019
   Decision 4) are irreconcilable in one object. So:
   - **`claim` (recommended default; the only elastic regime)** → a **Deployment**
     (fungible pods) + a KEDA `ScaledObject`. Cross-instance ownership is the
     work-claim lease alone (§3). KEDA owns `.spec.replicas`; the rendered
     Deployment omits it.
   - **`shard` (fixed partition; NOT KEDA-elastic)** → a **StatefulSet** (stable
     ordinal = `K`, `.spec.replicas == N`). `N` is the operator-chosen partition
     count `scaling.shards` (agentctl RFC 0003 §4.1) — a partition count, **not** a
     KEDA replica range; an `N`-change is a controller-driven **shard-resize**
     rolling restart (§4) with KEDA paused.

2. **agentctl ships and operates a reference coordination MCP server — the
   correctness backbone of claim mode.** It is the single serializing point that
   makes exactly-one-owner hold across replicas: an **atomic** `work.claim`,
   **transactional side-effect dedupe on `claim_key`** (not merely atomic claim),
   and the **off-pod backlog count** scale-from-zero needs (§3.2). It is a
   first-class stateful service agentctl runs (HA, backed up), and it is
   **pluggable** — a fleet may point at any server advertising the frozen `work.*`
   contract (agent RFC 0015 §5.6). It is the SERVER side of `work.*`; the agent is
   only ever a *caller* (agent RFC 0019 §2).

3. **The KEDA scaler is an EXTERNAL gRPC scaler (`crates/scaler`, `tonic`), not a
   Prometheus scaler — because the from-zero signal is off-pod and the per-pod
   gauges are unreadable at replica 0.** It implements KEDA's `externalscaler.proto`
   (`IsActive` / `StreamIsActive` / `GetMetricSpec` / `GetMetrics`) and reads the
   coordination server's backlog count (§5). At replica 0 there is no pod to scrape;
   the only thing that knows there is pending work is the work store. The external
   scaler's `IsActive` is what lights the first pod (§5.2).

4. **KEDA owns `.spec.replicas`; the renderer omits it (claim) or pauses KEDA
   (shard) — exactly one writer always.** This RFC specifies the `ScaledObject`'s
   *triggers*; the operator renders the object and never writes the replica field
   or the HPA directly (agentctl RFC 0006 §8.4). This is restated, not re-decided.

5. **Scale-down is the pod's SIGTERM drain — drain → bleed in-flight → release
   claims → exit 0 — never a CR finalizer.** KEDA deletes individual replica pods;
   a CR finalizer fires only on CR deletion (agentctl RFC 0006 §7.1). The pod's
   `terminationGracePeriodSeconds` (> `drain.timeoutSeconds`, the agentctl RFC 0003
   CEL invariant) + the agent's claim-release drain (agent RFC 0019 §6) do the
   work (§3.3).

6. **Correctness never depends on exactly-once delivery.** It rests on (a) a single
   serializing claim *or* a single owning shard, **and** (b) an item-derived
   idempotency key on every side effect (agent RFC 0019 §3.5/§8). When (a)
   momentarily fails — a lease TTL expiry, a rebalance seam, a mis-assignment — (b)
   holds the line. The reference coordination server MUST provide (b) transactionally
   (§3.2), or the system is at-least-once with no safety net (§7).

7. **Author autoscaling triggers against the FROZEN metric names only.** The frozen
   reactive set is `agent_pending_events` / `agent_reaction_lag_ms` /
   `agent_inflight_reactions` / `agent_subscriptions_active` (agent RFC 0016
   §4.3). The names agent RFC 0019 §5 used (`agent_reactive_backlog` /
   `agent_saturation` / …) are **not in the frozen schema** and are treated as
   not-real until **contract ask P10** reconciles the two sets (agentctl RFC 0010
   §5.4). The off-pod from-zero signal is neither set — it is the coordination
   server's count (P9).

8. **`AGENT_SHARD="K/N"` is unimplementable from one StatefulSet pod template — a
   contract defect, fixed by `--shard auto/N` (P3).** A pod template's env is
   identical across ordinals; the downward API exposes `metadata.name` (with the
   ordinal) but cannot express a computed composite `"3/8"` (agentctl RFC 0003
   §9.1). The fix is the agent deriving `K` from its ordinal; agentctl injects only
   `N` (§4.2).

These decisions are final for v1 of the scaling plane. Each defers to its owning
sibling RFC where it touches another plane, and **degrades gracefully** when a
surface or a contract ask is absent (§9).

---

## 3. claim-mode — the work-claim lease as the cross-replica owner

### 3.1 The regime

In claim mode every pod is shard `0/1` (no partitioning); cross-instance ownership
is the work-claim lease and nothing else (agent RFC 0019 §3). The flow per
reactive wake (agent RFC 0019 §3.4) is: route the item (intra-instance
exactly-one-owner) → **claim** it on the coordination server → process only on a
*granted* claim → **ack** on success / **release** on wind-down. Replicas are
fungible: any pod can process any item, and a re-claimed item runs identically
anywhere (agent RFC 0019 §6). That is precisely what lets the HPA treat the fleet
as interchangeable cattle and own `.spec.replicas` freely.

agentctl renders claim mode to a **Deployment** — not a StatefulSet — for two
reasons: a claim-mode pod needs **no ordinal identity** (the lease, not the
ordinal, is the owner), and a Deployment + `controller.kubernetes.io/pod-deletion-cost`
(driven from per-pod load) lets the HPA remove the **least-loaded** victim, which a
StatefulSet (highest-ordinal-first) cannot (agentctl RFC 0003 §4.2). The
pod-deletion-cost annotation is maintained by the operator from per-pod load read
through Tier A (`agent://capacity` / `agent_active_subagents`, agentctl RFC 0008
§3.2) — gated on **contract ask P4** (the `agent://capacity` schema is referenced
by agent RFC 0019 but undefined in agent RFC 0005/0015).

### 3.2 The reference coordination MCP server (the correctness backbone)

The claim convention is only as correct as the server behind it. agent is a
*participant*: it calls `work.claim`/`renew`/`ack`/`release` and supplies the
`claim_key`; it **assumes the server makes the claim atomic** (agent RFC 0019 §8
row 1) and **assumes the server dedupes the side effect on the key** (agent RFC
0019 §3.5). If the server cannot, two-owner is possible and only idempotency saves
correctness. So **agentctl ships a reference coordination MCP server** and operates
it as a first-class stateful service. It owns, exactly:

| Responsibility | Contract anchor | Notes |
|---|---|---|
| **Atomic `work.claim`** | agent RFC 0015 §5.6, agent RFC 0019 §3.3 | the single serializing point; grants to exactly one of N racers |
| **Lease lifecycle** (`renew`/`ack`/`release`, TTL expiry) | agent RFC 0019 §3.2 | a dead claimer's lease expires → item re-offered to the fleet |
| **Transactional dedupe on `claim_key`** | agent RFC 0019 §3.5/§10 | a redelivered-but-already-acked item is a server-side no-op; **the safety net for at-least-once** |
| **Off-pod backlog count** (`work.stats` / `work://pending`) | **contract ask P9** | the scale-from-zero signal the external scaler reads (§5.3) |
| **Both styles** (`tool` and `resource`) | agent RFC 0015 §5.6 | tool: serves the four `work.*` tools; resource: items carry a `lease` field + a compare-and-set |

> **Contract status (P12 / P9, made precise).** The `work.*` **tool names**,
> argument shapes, the `agent/*` `_meta` keys, and the item-derived `claim_key`
> are **already frozen** in agent RFC 0015 §5.6 (which resolved agent RFC 0019
> §12) — so the *claim* contract is settled and the reference server implements its
> server side directly. The residual asks are two: **(P9)** an off-pod **backlog
> count** surface (a `work.stats` tool or a countable `work://pending` resource) so
> a scaler can read "items pending" when **zero** pods exist; and the **`assign`**
> directed-assign tool for warm-pool push (§7), which agent RFC 0019 §7.2 and §12
> leave un-frozen. Until `assign` lands, directed-assign falls back to
> `subagent.spawn` (agent RFC 0005 §3.2). **Contract asks: P9, P12 (`assign`).**

The server is **pluggable**: `work.claim.server` (agentctl RFC 0003 §4.1) names a
declared MCP server; a fleet may BYO any server advertising the frozen `work.*`
contract (Redis-backed, Postgres-backed, the source server itself). agentctl's
reference server is the batteries-included default, not a lock-in. Its HA, sharding
(per-fleet vs cluster-shared), and backpressure are an open question (§10); its
durability matters because a coordination-server loss collapses the serializing
point for every fleet that depends on it.

**Claim TTL must be budget-aware, not a flat 120 s.** A lease shorter than the
realistic processing time (a slow LLM turn) expires mid-flight → the item is
redelivered → two replicas process concurrently → both write under the same
`claim_key` → the server dedupes to one effect (agent RFC 0019 §8 row 7). Correct,
but wasteful. agentctl sets `work.claim.ttlSeconds` from the fleet's
`limits.deadlineSeconds` / typical turn budget, and the agent renews at `ttl/3`
(agent RFC 0019 §3.6). The TTL is the *requested* value; the server is the
authority.

### 3.3 Scale-down: drain → bleed in-flight → release claims → exit 0

When KEDA scales a claim fleet down, the HPA deletes a replica **pod** (not the
CR). The pod's SIGTERM path runs the agent's drain choreography (agent RFC 0011
§4.2) extended by agent RFC 0019 §6's **step 1.5**:

```
KEDA/HPA scale-down  ──►  delete pod  ──►  SIGTERM:
  1.   DRAINING: disarm triggers; stop routing new items; flip not-ready
  1.5. work.release every CLAIMED-but-not-terminal item   ◄── hands work back NOW
       (best-effort, sub-budget min(2s, drain_timeout/4); lease TTL is the backstop)
  2.   bleed in-flight: wind down subagents at turn boundaries
         └─ each that reaches `completed` → work.ack; else → work.release
  3.   flush; exit 0   ◄── clean drain is 0, NOT 143 (agent RFC 0011 §5)
```

Two normative couplings agentctl renders to make this safe (agentctl RFC 0006
§7.1, agent RFC 0019 §8 row 10):

- **`podGraceSeconds` MUST exceed `drain.timeoutSeconds`** (the agentctl RFC 0003
  CEL invariant). A scale-down that SIGKILLs before drain completes leaks a held
  claim until its TTL — correct but slow.
- **A `PodDisruptionBudget`** bounds concurrent evictions so a single scale-down
  event does not SIGTERM the whole fleet at once and pile every release onto the
  coordination server in one burst (open question §10). The agentctl-rendered PDB
  is `maxUnavailable` tuned to the coordination server's release throughput.

This is the **pod** drain path; the **CR** drain path (the operator's
lame-duck→drain→finalizer choreography on `kubectl delete agentfleet`) is the
operator's and is owned by agentctl RFC 0006 §7. Do not conflate them.

### 3.4 The claim transport on a networkless pod (own the consequence)

The claim is the serializing **correctness** point, and a networkless pod (Kata /
off-pod tier) can reach the coordination MCP server only as an **outbound** MCP
client connection — which, on a pod with no NIC, must cross the substrate the same
way the irreducible intelligence/model egress leg does (agentctl RFC 0002, agentctl
RFC 0012; the brainstorm §5.2 flags this as the claim-transport-on-networkless-pod
problem). Two realizations, an operator picks one per substrate tier:

- **vsock→host egress proxy** (consistent with the intelligence egress chokepoint,
  agentctl RFC 0012): the agent dials the coordination server through the host-side
  egress path. This makes the on-node egress component a **per-node correctness
  dependency in the claim hot path** — own that consequence; it is the same
  reachability that the model channel already needs, so it adds no new isolation
  hole, but it is now on the *correctness* path, not just the inference path.
- **in-VM sidecar** the agent reaches over the loopback/emptyDir socket — keeps the
  claim path off the host but couples the coordination connection to the pod's
  lifecycle.

This is governed by the substrate reach abstraction (agentctl RFC 0002), not
re-decided here; this RFC only records that **claim is egress, and egress on a
networkless pod is not free**.

---

## 4. shard-mode — static partitioning & the resize controller

### 4.1 The regime

Shard mode statically partitions the key space so a fleet need not contend on every
item: an instance with shard `K` of `N` handles an item only if
`fnv1a64(shard_key(item)) % N == K` (agent RFC 0019 §4.1). The hash is a
hand-rolled **FNV-1a/64** — stable across versions, languages, and architectures
(it must produce the same partition on every conformant agent in the fleet,
regardless of vendor) — and the predicate runs **at routing intake, before claim
and before spawn**, so out-of-shard items are dropped at near-zero cost. `shard_key`
defaults to the resource URI (agentctl RFC 0003 §4.1 `work.shardKey`).

`N` is a deliberate, operator-chosen partition count (`scaling.shards`, agentctl
RFC 0003 §4.1), **not** a KEDA-elastic target — overloading the claim-mode
`scaling.min`/`scaling.max` range to mean `N` would be a category error, so shard
mode reads a dedicated field. Shard mode renders to a **StatefulSet** (stable
ordinal = `K`) with `.spec.replicas == N` operator-owned, and KEDA paused (§5.4). Sharding **composes
with claim**: shard narrows *which* items a replica considers, claim resolves
contention among replicas sharing a shard (transient overlap during a resize) — the
recommended belt-and-braces for work that must make progress despite a node loss
(agent RFC 0019 §4.1 bottom row). Shard-only (no claim) is the cheapest and is
correct as long as a dead shard's items may wait for the StatefulSet to reschedule
that ordinal — acceptable only for level-triggered, current-state work.

### 4.2 The `AGENT_SHARD` defect → `--shard auto/N` (P3)

agent RFC 0014 §6.4 / agent RFC 0019 §4.2 specify the agent reading
`AGENT_SHARD="K/N"` from env, with `K` = the StatefulSet ordinal and `N` =
`.spec.replicas`. **This is unimplementable from a single StatefulSet pod
template** (agentctl RFC 0003 §9.1): a pod template's `env` is **identical across
all ordinals** — every replica would receive the *same* `AGENT_SHARD` — and the
downward API can expose `metadata.name` (which *contains* the ordinal,
`<name>-<k>`) but **cannot express a computed composite** like `"3/8"`. So an
operator literally cannot inject a correct per-replica `K/N` via the template.

The fix is a contract primitive, not a leak into agentctl:

> **Contract ask P3 (a FIX).** `--shard auto/N`: the agent reads `N` from config
> and **derives `K` itself** from the ordinal in `AGENT_POD_NAME`. The operator
> then injects only `N` (uniform across the template, legal) and validates
> `0 ≤ K < N` at startup (agent RFC 0019 §4.4, exit 2). The operator renders the
> shard env by the negotiated contract (agentctl RFC 0006 §6.3): **P3 present →
> inject `N`, agent derives `K`**; **P3 absent → an initContainer shim** parses the
> ordinal from the downward-API pod name and writes `AGENT_SHARD="K/N"` to a shared
> `emptyDir` env file the agent sources — which **requires a non-scratch image**
> (a shell), defeating the scratch posture, so it is a stopgap only.

### 4.3 The shard-resize controller (rebalance hazards on an `N` change)

An `N`-change re-partitions the key space: items that hashed to shard 3-of-8 may now
belong to 5-of-12. agent holds shard identity as **immutable config** (agent RFC
0011 §4.1, no live reload), so a resize is a **rolling restart driven by agentctl**,
never an in-process migration (agent RFC 0019 §4.3, Decision 4). agentctl owns a
**shard-resize controller** (a sub-controller of the operator, agentctl RFC 0006)
that choreographs it:

```
shard-resize(fleet, N: 8 → 12):
  0. PAUSE KEDA            : pin the rendered ScaledObject minReplicaCount==maxReplicaCount==N
                            (a KEDA-level artifact, NOT the CRD's claim-only scaling.min/max),
                            or remove it — exactly one replica-field writer during the resize (§5.4)
  1. patch StatefulSet .spec.replicas 8 → 12  (== scaling.shards, operator-owned in shard mode)
  2. ROLL one pod at a time (NOT all at once):
       SIGTERM pod-k ──► drain (agent RFC 0019 §4.3):
         a. DRAINING: stop routing (old owns() predicate)
         b. work.release every held claim        ◄── frees in-flight items immediately
         c. wind down subagents at turn boundaries; exit 0
       start pod-k with the new N ──► new owns() predicate
  3. UNPAUSE / re-establish the ScaledObject (or leave pinned if shard is fixed)
```

**Rebalance hazards, and how each is covered (agent RFC 0019 §4.3 / §8 rows 8, 9,
14):**

| Hazard | Cover |
|---|---|
| **Two *live* shards own the same item** during the seam | The old pod's `owns()` stops routing (step 2a) **before** the new pod with the new `N` starts. One-pod-at-a-time bounds the overlap to one pod's restart. If claim is layered, the lease resolves any transient two-owner. |
| **An item owned by *neither* live shard** (old drained, new not ready) | Harmless for level-triggered work: read-after-subscribe reconcile (agent RFC 0008 §3.5) re-synthesizes a `Synthetic("possibly changed")` event when the now-owning shard comes up. |
| **A held claim not cleanly released** (forced kill) | The lease TTL is the backstop — the item redelivers when the lease expires. |
| **KEDA un-paused mid-resize** | Two replica-field writers → scale churn racing the roll. **The controller MUST pause KEDA first** (step 0) — this is the §5.4 one-writer rule applied to the resize. |
| **Shard-only fleet, a shard's pod down** | Items wait for the StatefulSet to reschedule that ordinal; acceptable only for level-triggered work, else enable claim (§4.1). |

The controller is **level-triggered and idempotent** (it re-derives the desired
roll state from `spec.scaling` + the StatefulSet's observed `currentReplicas`,
never from the triggering event), consistent with the operator discipline (agentctl
RFC 0006 §8). A resize-in-progress is surfaced as an `AgentFleet` condition (a
`Resizing` reason on the scaling projection, agentctl RFC 0003 §6.2) so GitOps and
`kubectl agents get` see it.

---

## 5. The KEDA external scaler (`crates/scaler`, `tonic`)

### 5.1 Why EXTERNAL, not a Prometheus scaler

KEDA ships a `prometheus` scaler that reads a PromQL query, and agentctl *does* feed
Prometheus (the Tier A scrape-proxy + recording rules, agentctl RFC 0010 §4/§5.3).
It is tempting to point a `prometheus` trigger at the
`agentctl:fleet_backlog = sum by (namespace, agent)(agent_pending_events)`
recording rule (agentctl RFC 0010 §5.3) and be done. **That breaks at the one place
elasticity matters most:**

- **At replica 0 there is no pod to scrape.** `agent_pending_events` is a
  **per-replica** gauge (agent RFC 0016 §4.3); with zero pods, the recording rule
  sums to *absent/zero*, and a Prometheus scaler reading zero **can never scale up
  from zero** — the fleet is wedged off. The signal that *work is pending while no
  worker runs* exists only **off the pods**, in the work store.
- **Networkless pods are not scraped at the source.** Even above zero, the per-pod
  metrics reach Prometheus only via the Tier A bridge (agentctl RFC 0010 §4) — an
  aggregate the **agent never computes** (agent RFC 0016 §7.3: fan-out is the
  subscriber's job). The cross-pod fleet view lives where the cross-pod
  serialization already lives: the coordination server.

So agentctl builds an **external** scaler — `crates/scaler`, a `tonic` gRPC service
implementing KEDA's `externalscaler.proto` (agentctl RFC 0001 §3, the
Rust/kube-rs-gaps closure) — that reads the coordination server's **off-pod backlog
count** (P9). This is the only signal correct at replica 0, and it is uniform across
all three substrate tiers (it never touches a pod).

### 5.2 The four RPCs

```
service ExternalScaler {            // crates/scaler, served on e.g. agentctl-scaler.agentctl-system:9100
  rpc IsActive(ScaledObjectRef) returns (IsActiveResponse);          // gate scale-from/to-zero
  rpc StreamIsActive(ScaledObjectRef) returns (stream IsActiveResponse); // external-PUSH: react fast
  rpc GetMetricSpec(ScaledObjectRef) returns (GetMetricSpecResponse);    // the target the HPA scales on
  rpc GetMetrics(GetMetricsRequest)  returns (GetMetricsResponse);       // the current value
}
```

| RPC | What `crates/scaler` returns | Source |
|---|---|---|
| **`IsActive`** | `active = (backlog > activationThreshold)` | coordination server backlog count (P9). **This is the scale-from-zero gate**: `active=false` → KEDA holds the fleet at 0; `active=true` → KEDA lights the first pod (to `max(1, minReplicaCount)`). |
| **`StreamIsActive`** | pushes `IsActiveResponse` on each backlog 0↔>0 transition | the **external-push** variant — the coordination server notifies the scaler (via a `work://pending` subscription) so a from-zero wake is sub-second, not bound to KEDA's poll interval. |
| **`GetMetricSpec`** | `metricName`, `targetValue = scaling.target.threshold` | from `AgentFleet.spec.scaling.target` (agentctl RFC 0003 §4.1) |
| **`GetMetrics`** | the current backlog depth | coordination server `work.stats` / count of `work://pending` (P9) |

KEDA's generated HPA then drives `.spec.replicas` toward
`ceil(backlog / threshold)`, floored/ceiled by `scaling.min`/`scaling.max`. The
`activationThreshold` (default 1) is distinct from the `threshold`: activation gates
the **0↔1** transition; the threshold governs the **1↔max** ratio. Both come from
the CRD (agentctl RFC 0003 §4.1).

> **Contract ask P9 (made concrete).** The reference coordination server exposes a
> `work.stats` tool returning `{ "pending": <int>, "claimed": <int>, "oldest_age_ms": <int> }`
> (and/or a countable `work://pending` resource). The scaler reads `pending` for
> `GetMetrics`/`IsActive` and `oldest_age_ms` as an optional latency-pressure
> trigger. Freezing this convention (parallel to the frozen `work.*`) is the ask —
> a BYO server must advertise it to support scale-from-zero. **Contract ask: P9.**

### 5.3 The signal — off-pod for from-zero, in-pod for refinement

agentctl uses **two** layered triggers on the claim-mode `ScaledObject`, with the
external trigger as the load-bearing one:

1. **External (primary, from-zero-capable).** `crates/scaler` reading the
   off-pod backlog (P9). Correct at every replica count **including 0**; substrate-
   blind. This is the trigger that makes `scaling.min: 0` work.
2. **Prometheus (optional, in-pod refinement, floor ≥ 1).** When the floor is ≥ 1
   (a warm pool, §7, or a no-from-zero fleet), a second `prometheus` trigger on the
   **frozen** `agent_pending_events` / `agent_reaction_lag_ms` recording rule
   (agentctl RFC 0010 §5.3) adds per-pod saturation awareness the off-pod count
   lacks. KEDA takes the **max** of the triggers' replica recommendations, so the
   refinement only ever scales *further up*, never below the off-pod floor.

### 5.4 KEDA owns `.spec.replicas` — exactly one writer (restated)

This RFC owns the *trigger detail*; the field-ownership rule is agentctl RFC 0003
§4.3 / RFC 0006 §8.4 and is **restated, not re-decided**:

- **claim mode:** the rendered Deployment **omits `.spec.replicas`** entirely — the
  operator's `fieldManager` never claims it, so the KEDA-generated HPA's writes are
  never reverted (SSA: omitted fields are unowned). The operator renders the
  `ScaledObject` (`minReplicaCount: scaling.min`, `maxReplicaCount: scaling.max`,
  the triggers above); KEDA generates the HPA.
- **shard mode:** `.spec.replicas == N` (`scaling.shards`) is operator-owned and
  KEDA is **paused** — the rendered `ScaledObject`'s
  `minReplicaCount == maxReplicaCount == N` pin (a KEDA-level artifact, distinct
  from the CRD's claim-only `scaling.min`/`scaling.max`), or the `ScaledObject`
  removed for the resize window (§4.3). One writer at any instant.

The operator never writes the HPA or the replica field directly. The
`lastScaleTime` / `currentMin`/`currentMax` projection into `AgentFleet.status`
(agentctl RFC 0003 §6.2) is read back from the HPA, not authored here.

---

## 6. The autoscaling-signal pipeline — frozen metrics → Tier A → KEDA

### 6.1 The pipeline

```
              ┌────────────────────── the fleet (N reactive workers, networkless) ──────────────────────┐
              │  pod-0 … pod-k   agent: claim over egress (§3.4) · emits per-pod gauges (agent RFC 0016)│
              └────────┬───────────────────────────────────────────────────────────┬─────────────────────┘
   off-pod backlog     │ work.claim/renew/ack/release (frozen, agent RFC 0015 §5.6) │ agent_pending_events,
   (correct at 0)      ▼                                                             │ reaction_lag_ms (FROZEN)
        ▲     ┌──────────────────────────────────────┐                              ▼ scraped via Tier A
        │     │ reference coordination MCP server      │   work.stats / work://pending   ┌──────────────────┐
        │     │ (agentctl ships+operates; PLUGGABLE)   │◄── (P9)                          │ node-agent Tier A │
        │     │  • atomic lease  • claim_key dedupe    │                                  │ scrape-proxy      │
        │     │  • off-pod backlog count (P9)          │                                  │ (agentctl RFC 0010)│
        │     └──────────────────┬─────────────────────┘                                  └────────┬─────────┘
        │  GetMetrics / IsActive │ StreamIsActive (push)                                            ▼ http_sd
        │     ┌──────────────────▼─────────────────────┐                                  ┌──────────────────┐
        └─────┤ crates/scaler  (tonic externalscaler)   │                                  │   PROMETHEUS     │
              │  IsActive · StreamIsActive ·            │◄── prometheus trigger (≥1 floor)─┤ agentctl:fleet_  │
              │  GetMetricSpec · GetMetrics             │     in-pod refinement (FROZEN)    │ backlog (RFC 0010)│
              └──────────────────┬─────────────────────┘                                  └──────────────────┘
                                 ▼ external trigger
              ┌──────────────────────────────────────────┐
              │ KEDA  →  generated HPA  OWNS .spec.replicas│  (agentctl RFC 0003 §4.3 / RFC 0006 §8.4)
              └──────────────────┬─────────────────────────┘
                                 ▼ scale
              Deployment (claim, replicas omitted)  |  StatefulSet (shard, replicas=N, KEDA paused)
```

The off-pod path (left) is the load-bearing one — correct at replica 0, substrate-
blind. The in-pod path (right) reuses the telemetry bridge agentctl RFC 0010 already
builds, and only ever refines *upward*.

### 6.2 Reconciling the P10 metric-name conflict

There are **two competing name sets** in the contract, and agentctl authors only
against the real one (agentctl RFC 0010 §5.4):

- **Frozen — author triggers against these** (agent RFC 0016 §4.3): `agent_pending_events`
  (gauge), `agent_reaction_lag_ms` (gauge), `agent_inflight_reactions` (gauge),
  `agent_subscriptions_active` (gauge).
- **NOT frozen — do NOT author against these** (agent RFC 0019 §5 names them and
  *falsely calls them frozen*): `agent_reactive_backlog`, `agent_saturation`,
  `agent_tokens_per_sec`, `agent_claims_lost_total`. They are **not in the frozen
  schema** (agent RFC 0016 §4.3).

> **Contract ask P10 (a FIX).** Reconcile the two sets into **one**, and — if
> `agent_saturation` (`in_flight / capacity`) is to be the HPA "utilization"
> trigger agent RFC 0019 §5.2 wants — **add it to the frozen `metrics_schema`**.
> Until P10 lands, the in-pod refinement trigger (§5.3) uses **only**
> `agent_pending_events` / `agent_reaction_lag_ms`; `saturation`/`backlog` are
> treated as not-real. The `scaling.target.signal` token in the CRD is deliberately
> **un-prefixed and contract-neutral** (`reactive_backlog`, *not*
> `agent_reactive_backlog`); the webhook maps the neutral token onto the negotiated
> `metrics_schema`'s actual (possibly `agent_`-prefixed) name, so the CRD never
> bakes in a vendor prefix and never hard-transcribes a name codegen should own
> (agentctl RFC 0003 §4.3, the brainstorm §11.2 transcription-hazard correction).
> **Contract ask: P10.**

This is the same anti-transcription discipline the whole track turns on: the off-pod
from-zero signal is the coordination server's count (a contract agentctl *defines*
in P9), and the in-pod refinement keys off the *negotiated* `metrics_schema`, never
a literal string in the scaler binary.

---

## 7. Warm-pool / standby — fast start, and the hazards spelled out

### 7.1 Standby vs scale-to-zero (the tension)

Scale-from-zero (§5) trades a **cold start** — one intelligence handshake + MCP
connects on the first event — for a zero idle cost. A **warm pool** trades a small
steady-state cost for **no cold start**: `--standby` workers hold the intelligence
session and MCP connections open and wait to be *assigned* work (agent RFC 0019
§7). The two are in direct tension: a warm floor `> 0` is incompatible with `min: 0`
on the *same* pods. agentctl resolves it by **regime, per fleet**:

- **Pure scale-from-zero:** `scaling.min: 0`, no warm pool. Accept the cold start.
- **Floored warm pool:** `scaling.min: W` (the warm-pool size) with the standby
  triggers; the external scaler's `IsActive` is irrelevant below `W`. The fleet
  never sleeps but never cold-starts within `W`.
- **Hybrid (recommended for spiky load):** a small **floored standby sub-pool**
  (`minReplicaCount: W` of `--standby` pods) **plus** a from-zero elastic pool above
  it. The standby pool absorbs the first `W` of a spike with no cold start while the
  elastic pool spins up behind it. This is the floored-`minReplicaCount`-sub-pool
  resolution the brainstorm §5.4 names.

### 7.2 How work reaches a standby member (reuse MCP — no new protocol)

Two mechanisms (agent RFC 0019 §7.2), an operator picks one:

1. **Claim-pull (preferred, symmetric with §3).** Standby members subscribe to a
   shared assignment resource (`work://pending`) on the coordination server; on its
   `updated`, every standby member races `work.claim` — exactly one wins, the rest
   return to standby. **No new code**: it is the claim convention with the warm pool
   as the contender set, and the lease still covers a winner that dies.
2. **Directed-assign (push).** agentctl, having chosen a specific warm member from
   `agent://capacity` (P4), hands it a unit of work directly. The canonical push
   tool is an **`assign` management tool that does not yet exist** in the frozen
   contract (agent RFC 0019 §7.2/§12) — **residual contract ask P12 (`assign`)** —
   so until it lands, directed-assign falls back to `subagent.spawn` over the
   management transport (agent RFC 0005 §3.2). The *choice of which member* is
   always agentctl policy (agent RFC 0014 §3); the agent only exposes the
   spawn/assign primitive and `agent://capacity` so agentctl sees who is free.

```jsonc
// agent://capacity (agent RFC 0005 resource, contract ask P4) — what agentctl reads to place work
{ "instance":"pod-abc", "shard":"3/8", "standby":true,
  "free_slots":4, "active_subagents":0, "intelligence":{"warm":true,"healthy":true},
  "max_total_subagents":64, "saturation":0.0 }
// NOTE: `saturation` here is the P4 `agent://capacity` RESOURCE field (a placement input, gated on
// the P4 capacity schema being defined), NOT the P10 `agent_saturation` METRIC (§6.2 / Decision 7,
// treated as not-frozen until P10). The capacity-resource field and the metrics-schema gauge are
// distinct surfaces; do not conflate them.
```

Standby is **not** session checkpoint/restore: a standby pod holds *open
connections*, never *prior work*, so it stays as stateless and fungible as any other
replica (agent RFC 0019 §7.3) — a scale-down or eviction loses nothing but a warm
connection.

### 7.3 Exactly-once + rebalance hazards (the invariant, restated)

Across **every** scaling action — scale-up, scale-down, from-zero wake, shard
resize, warm-pool assign — the invariant holds (agent RFC 0019 §8):

> Correctness **never** depends on exactly-once delivery. It depends on **(a)** a
> single serializing claim *or* a single owning shard, **and (b)** an item-derived
> idempotency key on every side effect. When (a) momentarily fails — a lease TTL
> expiry under a slow LLM turn, a rebalance seam where an item is briefly owned by
> two live shards, a warm-pool race, a `--shard` mis-assignment — **(b) holds the
> line**: both writers use the same `claim_key`, and the coordination server's
> transactional dedupe (§3.2) collapses them to one effect.

The hazards and their covers, consolidated:

| Scaling action | Hazard | Cover |
|---|---|---|
| **Scale-up (claim)** | a new pod joins the claim race mid-item | the loser gets `granted:false`, drops (`claims_lost`); only one processes |
| **Scale-down (claim)** | held claim lost on victim deletion | drain step 1.5 releases; lease TTL backstop; PDB bounds the burst |
| **From-zero wake** | the first event arrives before any pod | the coordination server holds the item; `IsActive`→true lights a pod; it claims on wake |
| **Shard resize** | two live shards own the same slice | one-pod roll + old `owns()` stops before new starts; claim resolves overlap (§4.3) |
| **Warm-pool assign** | two standby members race the same item | claim-pull serializes; directed-assign is agentctl-chosen (one target) |
| **Lease expiry mid-flight** | two replicas process one item | same `claim_key` → server dedupe → one effect; straggler's ack is a no-op |

The single thing agentctl MUST get right to make all of this safe is **(b) at the
coordination server** — which is exactly why the reference server's transactional
`claim_key` dedupe (§3.2) is non-negotiable, not "atomic claim" alone.

---

## 8. YAML strawman

### 8.1 claim mode → Deployment + external `ScaledObject` + coordination server

```yaml
# what the user applies (agentctl RFC 0003 §10.3)
apiVersion: agents.x-k8s.io/v1alpha1
kind: AgentFleet
metadata: { name: inbox-workers, namespace: agents }
spec:
  template:                                 # per-replica Agent spec (mode pinned reactive)
    mode: reactive
    image: registry.example.com/acme/agent@sha256:abcd…
    instruction: { configMapRef: { name: inbox-instruction } }
    config:      { configMapRef: { name: inbox-config } }
    intelligenceRef: { name: anthropic-pool }
    mcp: { serverSetRefs: [inbox-readers], servers: [ { name: ticketer, tags: [egress] } ] }
    substrate: { tier: kata-hybrid }         # networkless → claim rides egress (§3.4)
  scaling:
    mode: claim                             # elastic regime → Deployment + KEDA
    min: 0                                   # scale-from-zero (off-pod backlog, P9)
    max: 50
    target:
      signal: reactive_backlog              # contract-neutral token → negotiated metrics_schema (P10)
      threshold: 5                           # GetMetricSpec targetValue: replicas ≈ ceil(pending/5)
      activationThreshold: 1                 # IsActive gates the 0↔1 transition
  work:
    source: { mcp: inbox, uri: "file:///inbox/*.json" }
    claim:  { server: coord, style: tool, ttlSeconds: 30, key: item }   # budget-aware TTL (§3.2)
  drain: { timeoutSeconds: 45 }
  podGraceSeconds: 60                        # CEL: MUST exceed drain.timeoutSeconds (§3.3)
```

```yaml
# ── what the operator RENDERS (agentctl RFC 0006); this RFC owns the ScaledObject triggers ──
apiVersion: apps/v1
kind: Deployment
metadata: { name: inbox-workers, namespace: agents }
spec:
  # .spec.replicas OMITTED — KEDA's HPA is the sole writer (agentctl RFC 0006 §8.4)
  selector: { matchLabels: { agents.x-k8s.io/fleet: inbox-workers } }
  template:
    metadata:
      labels: { agents.x-k8s.io/fleet: inbox-workers, agents.x-k8s.io/managed: "true" }
      annotations: { controller.kubernetes.io/pod-deletion-cost: "0" }  # operator updates from Tier A load (§3.1)
    spec:
      terminationGracePeriodSeconds: 60      # == podGraceSeconds (> drain.timeoutSeconds)
      # … agent container, downward-API env, substrate wiring (agentctl RFC 0006 §6) …
---
apiVersion: keda.sh/v1alpha1
kind: ScaledObject
metadata: { name: inbox-workers, namespace: agents }
spec:
  scaleTargetRef: { name: inbox-workers }    # the Deployment
  minReplicaCount: 0                          # from scaling.min — scale-to/from-zero
  maxReplicaCount: 50
  cooldownPeriod: 120                         # hysteresis vs bursty backlog (agent RFC 0019 §5.2)
  triggers:
    - type: external-push                     # PRIMARY — off-pod, from-zero (§5)
      metadata:
        scalerAddress: agentctl-scaler.agentctl-system:9100   # crates/scaler (tonic)
        fleet: agents/inbox-workers           # ScaledObjectRef → coordination-server backlog (P9)
        threshold: "5"
        activationThreshold: "1"
    - type: prometheus                        # OPTIONAL refinement — in-pod, frozen names, floor ≥1 (§5.3)
      metadata:
        serverAddress: http://prometheus.monitoring:9090
        query: 'agentctl:fleet_backlog{namespace="agents",agent="inbox-workers"}'  # FROZEN agent_pending_events sum
        threshold: "5"
---
apiVersion: apps/v1                           # the reference coordination MCP server (agentctl ships+operates)
kind: Deployment
metadata: { name: coord, namespace: agents }  # PLUGGABLE: any server advertising frozen work.* + P9 backlog
spec: { replicas: 2 }                          # HA (open question §10); atomic lease + claim_key dedupe + work.stats
# A PodDisruptionBudget (maxUnavailable tuned to coord release throughput) is rendered for the fleet (§3.3).
```

### 8.2 shard mode → StatefulSet (KEDA paused) + the resize controller

```yaml
spec:
  scaling:
    mode: shard                             # fixed partition → StatefulSet, NOT KEDA-elastic
    shards: 8                                # N = partition count (FNV-1a/64 modulus) AND .spec.replicas (RFC 0003 §4.1)
    # NO scaling.min/max in shard mode — N is a partition count, not a KEDA range. The rendered
    # ScaledObject (if any) is pinned minReplicaCount==maxReplicaCount==N → KEDA paused (§5.4).
    target: { signal: reactive_backlog, threshold: 5 }   # inert while paused; used only if claim layered
  work:
    source:   { mcp: inbox, uri: "file:///inbox/*.json" }
    shardKey: item                           # FNV-1a/64 over this key (agent RFC 0019 §4.1)
    claim:    { server: coord, style: tool, ttlSeconds: 30 }   # OPTIONAL — layered for rebalance safety (§4.1)
  drain: { timeoutSeconds: 45 }
  podGraceSeconds: 60
# Rendered: a StatefulSet with .spec.replicas: 8 (operator-owned). Each pod gets AGENT_SHARD's N
# injected uniformly; K is derived by the agent from its ordinal via --shard auto/N (P3), or by an
# initContainer shim on a non-scratch image (§4.2). A shard-resize (N: 8→12) is the controller-driven
# drain-and-reassign roll with KEDA paused first (§4.3) — never an HPA action.
```

---

## 9. Graceful degradation

The scaling plane degrades cleanly when a surface or a contract ask is absent
(agent RFC 0014 §7, agentctl RFC 0006 §6.1):

- **`surfaces.cluster: false`** (a non-cluster build, agent RFC 0019 §9) → the
  agent is a single-instance worker; agentctl runs it as a `reactive` **singleton**
  (a Deployment `strategy: Recreate`, agentctl RFC 0003 §5.1), **never** as an
  `AgentFleet` — admission rejects an `AgentFleet` whose negotiated agent lacks
  `cluster` (agentctl RFC 0007).
- **`claim` advertised but not `standby`** → claim-mode elasticity works; the warm
  pool is unavailable (cold start only).
- **`work.claim.style` not in the negotiated agent's `surfaces.claim.styles`** → the
  webhook **rejects** the fleet at admission (agentctl RFC 0007). A fleet may select
  only a claim style the conformant agent advertises (the agent's manifest declares
  `claim: { styles: [...] }`, agent RFC 0015 §5.6 / RFC 0019 §9). This is the same
  read-it-from-`surfaces{}`-never-assume discipline that gates `min: 0` on the
  from-zero backlog surface — a mistyped `style` fails at admission, not at runtime.
- **`crates/scaler` (external scaler) unavailable** → KEDA gets no external metric;
  per KEDA semantics it **holds the last computed replica count** (it does not scale
  to zero on a missing metric) and the `StreamIsActive` push path drops to the poll
  fallback. To keep a busy **from-zero** fleet from being stranded at 0 with pending
  work while the scaler bounces, agentctl renders a configurable **non-zero floor
  fallback** (`activationFallbackReplicas`, default 1) so an `min: 0` fleet recovers
  to ≥1 if the scaler is unreachable past a grace window; the operator surfaces a
  `ScalerUnreachable` condition. The scaler's own HA/replication is §11 #9.
- **P9 absent** (no off-pod backlog count) → **no scale-from-zero**; the fleet
  floors at `min ≥ 1` and uses the in-pod Prometheus refinement (§5.3) only. The
  webhook rejects `scaling.min: 0` against an agent/coordination-server that cannot
  serve the backlog count (consistent with the agentctl RFC 0003 CEL that already
  gates `min: 0` to claim mode).
- **P3 absent** (no `--shard auto/N`) → shard mode falls back to the initContainer
  shim on a non-scratch image; on a scratch image, shard mode is unavailable and the
  webhook says so (agentctl RFC 0003 §9.1).
- **P10 unmet** → in-pod refinement uses only `agent_pending_events` /
  `agent_reaction_lag_ms`; `saturation`-based triggers are not authored.
- **`assign` absent** → directed-assign falls back to `subagent.spawn`; claim-pull
  warm-pool assignment is unaffected.
- **Coordination server unreachable at startup** → a `claim` route's agent exits
  **6 (EXIT_MCP)**, retriable (agent RFC 0019 §8 row 4); the operator surfaces a
  `CoordinationUnreachable` condition and the fleet does not silently double-process
  (no claim = no processing on a `claim` route, by construction).

---

## 10. Non-goals

- **No queue, broker, consensus, or autoscaler in the data plane.** The agent
  claims, shards, and emits signals; it ships no Redis/Raft/etcd client and no HPA
  logic (agent RFC 0019 §10). All of that is agentctl's.
- **No exactly-once delivery.** At-least-once + item-derived idempotency only (§7.3,
  agent RFC 0019 §3.5). A use case needing true exactly-once needs a transactional
  backing service; the reference coordination server *is* that service for its own
  side effects, but agentctl does not make arbitrary non-idempotent compositions
  exactly-once.
- **No live shard migration / consistent-hashing ring in v1.** Rebalance is a
  rolling restart (§4.3); a minimal-disruption consistent-hash assignment is a
  later agentctl placement refinement, not a v1 mechanism (agent RFC 0019 §10).
- **No `Task`/`Run` CRD and no per-item state in etcd.** Claim/lease state lives in
  the coordination server; fleet scaling state is the HPA's + the curated
  `AgentFleet.status` projection (agentctl RFC 0003 §6.2). Task/status-event churn
  in etcd is the anti-pattern this whole track avoids.
- **No `ScaledJob`/batch-fan-out regime in v1.** `AgentFleet` is **reactive** only
  (agentctl RFC 0003 §4.1 pins `template.mode == reactive`); a queue-driven `once`
  batch pattern (a KEDA `ScaledJob` over `mode: once` Jobs) is a recognised future
  shape but is **not** an `AgentFleet` and is deferred.
- **No coordination-server schema beyond the frozen `work.*` + the P9 backlog
  count.** Its internal storage, HA topology, and durability are an implementation
  matter (a reference impl this RFC ships), not a contract; a BYO server need only
  satisfy the frozen surface.
- **No intelligence-cost-aware scaling enforcement in v1.** Backlog *can* become a
  scale signal when a fleet hits its tree-token budget (agent RFC 0019 §8 row 12),
  but **hard** budget-back-pressure as a scale brake needs the `EXIT_BUDGET` /
  readiness-back-pressure primitive (contract ask **P-cost**, agentctl RFC 0012);
  v1 is best-effort.

---

## 11. Open questions

1. **Coordination server: per-fleet or cluster-shared, and its HA/sharding/
   backpressure shape.** A cluster-shared server is one thing to operate but a
   blast-radius concentration (its loss collapses the serializing point for every
   fleet); a per-fleet server isolates failure but multiplies ops. The transactional
   `claim_key` dedupe (§3.2) must survive a server failover (an RPO/RTO question the
   DR line, brainstorm §12, must answer).
2. **PDB sizing vs the coordination server's release throughput.** A scale-down that
   evicts many pods at once piles every `work.release` onto the server in a burst
   (§3.3). What `maxUnavailable` keeps the release rate under the server's capacity
   without making scale-down glacial?
3. **The KEDA↔operator handoff during shard-resize.** Pause via the rendered
   `ScaledObject`'s `minReplicaCount == maxReplicaCount == N` pin (a KEDA-level
   artifact, not the CRD), or remove the `ScaledObject` for the window? The pin keeps
   the object (cleaner GitOps) but risks an operator that re-reconciles the pin
   fighting itself; removal is unambiguous but churns the object (§4.3).
4. **Standby vs scale-to-zero default.** Should the *default* claim fleet be
   from-zero (cheapest) or a small floored warm pool (no cold start)? The hybrid
   (§7.1) is best for spiky load but is the most moving parts.
5. **Claim transport on networkless pods (§3.4): vsock→host egress proxy vs in-VM
   sidecar as the default.** The proxy makes the on-node egress a per-node
   correctness dependency in the claim hot path; the sidecar avoids that but couples
   the coordination connection to the pod. Which is the default per tier (an agentctl
   RFC 0002 / RFC 0012 cross-cut)?
6. **`scaling.target.signal` enum source.** Validated against the negotiated
   `metrics_schema` (agentctl RFC 0003 §13.5), but the contract's own metric-name set
   is unreconciled until P10. The webhook keys the allowed set to the AgentClass's
   contract major in the interim.
7. **From-zero scale-up latency.** `StreamIsActive` (external-push) makes the wake
   sub-second, but the *first pod's* cold start (intel + MCP handshake) is the real
   floor; is a minimum 1-pod warm reserve (§7) effectively mandatory for
   latency-sensitive fleets, making true `min: 0` a cost-only, not latency-only,
   choice?
8. **Multi-tenant coordination isolation.** Under hostile tenancy (brainstorm §0.6),
   a cluster-shared coordination server multiplexes every tenant's work; the
   `claim_key`/backlog surface must be tenant-scoped (row-level authorization), and
   the external scaler must not leak one tenant's backlog depth to another's
   `ScaledObject` (an agentctl RFC 0015 — security & multi-tenancy — cross-cut).
9. **`crates/scaler` HA/replication/degradation.** The external scaler is the SOLE
   from-zero signal path for a `min: 0` claim fleet (§5.3) — a control-plane SPOF for
   the headline elastic property — yet, unlike the coordination server (#1), its own
   availability is unspecified. What is its replication model (≥2 replicas behind the
   `scalerAddress` Service; leader-elected vs stateless-fungible read of the
   coordination backlog), and is the §9 `activationFallbackReplicas` floor the right
   stranded-at-zero backstop, or should KEDA's last-replicas hold suffice for fleets
   that tolerate a wake delay? (Parallel to the coordination-server blast-radius #1.)

---

## 12. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo: the **KEDA external scaler as `crates/scaler`
  in `tonic`** implementing `externalscaler.proto` (§3, the kube-rs-gaps closure
  this RFC builds against); the contract-as-schema anti-drift the neutral
  `scaling.target.signal` token relies on.
- **agentctl RFC 0002** — substrate & transport: the networkless reach the claim
  egress (§3.4) and the `agent://capacity` read ride; the exec-health probe (P1) the
  scaled pods use on networkless tiers.
- **agentctl RFC 0003** — Agent & AgentFleet CRDs: the `AgentFleet.spec` this RFC
  consumes (`scaling.mode` claim/shard §4.2, the claim-only `scaling.min`/`scaling.max`
  range and the shard-only `scaling.shards` partition count `N` §4.1, `scaling.target`
  §4.1, `work.*`, drain<grace, the KEDA-owns-replicas field-ownership §4.3, the
  `AGENT_SHARD` defect §9.1, the neutral `scaling.target.signal` §4.3, the status
  projection §6.2).
- **agentctl RFC 0006** — operator reconcile: renders the StatefulSet/Deployment +
  `ScaledObject` (this RFC owns the trigger detail), omits `.spec.replicas` (§8.4),
  the two drain paths (§7.1), the downward-API shard `N` injection (§6.3); host of
  the shard-resize controller.
- **agentctl RFC 0008** — node-agent (two tiers): the Tier A telemetry bridge / load
  read (`agent://capacity`, `agent_active_subagents`) for pod-deletion-cost and
  victim selection, and the lame-duck/drain victim path.
- **agentctl RFC 0010** — observability & telemetry bridge: the frozen
  autoscaling-signal set, the **P10** flag (§5.4), the `agentctl:fleet_backlog`
  recording rule the in-pod refinement trigger reads, the scrape topology.
- **agentctl RFC 0012** — intelligence plane: the egress proxy the claim transport
  on networkless pods shares (§3.4); the `EXIT_BUDGET` / back-pressure primitive
  (P-cost) for budget-aware scaling.
- **agentctl RFC 0007** — admission ladder: rejects an `AgentFleet` on a
  non-`cluster` agent, `min: 0` without a from-zero backlog surface, and a
  `work.claim.style` not in the negotiated `surfaces.claim.styles` (§9).
- **agentctl RFC 0015** — security & multi-tenancy: the
  home for multi-tenant coordination isolation — the tenant-scoped `claim_key`/backlog
  surface and the per-tenant scaler isolation (§11 #8).

**Contract spec (the reference implementation, agent RFCs)**

- **agent RFC 0019** — horizontal scaling: the work-claim lease (§3), the FNV-1a/64
  shard predicate (§4), the autoscaling-signal surface (§5), the drain-release
  **step 1.5** (§6), standby (§7), the edge-case/failure-semantics table (§8), the
  `cluster` manifest additions (§9) — the whole data-plane half this RFC builds the
  Kubernetes half around.
- **agent RFC 0015** — management & control surface: the **frozen `work.*` contract**
  (§5.6 — names, `_meta`, `claim_key`, styles), the operator tools
  (`drain`/`lame-duck`/`cancel`) for scale-down victim selection.
- **agent RFC 0016** — telemetry & lifecycle contract: the **frozen metrics schema**
  (§4.3, the four reactive gauges this RFC's triggers key off), the exit-code table,
  `agent_drains_total{phase}`.
- **agent RFC 0008** — execution modes & reactive routing: the intra-instance
  exactly-one-owner rule (§2.2) this RFC extends across instances; the reactive wake
  (§3.7) the claim/shard gates insert into; read-after-subscribe reconcile (§3.5).
- **agent RFC 0011** — cloud-native contract: the drain choreography (extended by
  RFC 0019 §6), the exit-code table (2/6 classify claim-server failures), RUN_ID
  idempotency (the claim key), statelessness (fungible replicas).
- **agent RFC 0005** — self-MCP & control: `agent://capacity`/`agent://metrics`
  (contract ask **P4**) for placement and the networkless metrics path;
  `subagent.spawn` as the directed-assign fallback.
- **agent RFC 0014** — control-plane umbrella: primitives-not-policy; the
  capabilities manifest (`surfaces.cluster`/`claim`/`shard`/`standby`); the
  downward-API env convention (and the `AGENT_SHARD` §6.4 defect, P3).

**Contract asks raised or cited by this RFC** (agentctl brainstorm §14): **P3**
(`--shard auto/N` — the `AGENT_SHARD` defect, a FIX), **P4** (`agent://capacity` /
`agent://metrics` schema, a FIX), **P9** (off-pod backlog count for scale-from-zero,
NEW), **P10** (autoscaling metric-name reconciliation, a FIX), **P12** (`work.*`
ownership — **resolved/frozen** in agent RFC 0015 §5.6; residual: the `assign`
directed-assign tool, NEW), **P1** (exec health verb for networkless scaled pods),
**P-cost** (budget back-pressure for budget-aware scaling).
