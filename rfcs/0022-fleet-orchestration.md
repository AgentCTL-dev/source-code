# agentctl RFC 0022: Fleet orchestration — coordinator/worker topology, the work fabric, and the fleet as an addressable endpoint

**Status:** Proposed (agentctl plane track; extends 0003/0011/0013/0014; amends 0012)
**Author:** Andrii Tsok
**Date:** 2026-07-03
**Part of:** the agentctl control plane — turns `AgentFleet` from a bag of fungible replicas into a **distributed agent system**: a "main agent" (coordinator) that fans work out to an elastic worker pool over a result-bearing work fabric, addressable from outside as a **single A2A endpoint**, with per-fleet budget isolation.

> **Contract-first, not agent-first (P0).** This RFC adds control-plane *orchestration*
> around conformant agents; it does not add a dependency on any one agent. Every new
> mechanism is expressed in terms the published Agent Control Contract already defines —
> the `work.*` coordination MCP surface (RFC 0011), A2A over the substrate (RFC 0013),
> and the downward-API env convention. Where a concrete flag or wire shape is named, it
> is *where the contract is presently written down* (agentd 2.x), never a dependency. A
> second conformant agent that claims work and serves A2A is orchestrated unchanged.

---

## 1. Summary

Today an `AgentFleet` is **homogeneous**: one `template: AgentSpec`, rendered to one
Deployment (claim mode) or StatefulSet (shard mode) of interchangeable replicas
(RFC 0003 §4, `render_fleet`). That models a *worker pool* but nothing else. The three
things a real "agent distributed system on top of Kubernetes" needs are missing:

1. **Heterogeneous topology.** No way to say "a **main** agent that plans and delegates,
   plus N **worker** agents that execute." Subagents exist only *inside* one pod
   (agentd's in-process `subagent.spawn`); there is no control-plane coordinator→workers
   shape.
2. **A result-bearing work fabric.** The `work.*` queue distributes items but has **no
   result/correlation channel** (a `submit` returns no id; `ack` carries no output) and
   **no dead-letter** (a poison item redelivers forever). A coordinator cannot collect
   what its workers produced, and a bad item never stops.
3. **A drivable fleet endpoint.** The A2A gateway serves a fleet **card** at
   `/fleets/{ns}/{name}` but **no RPC route** — a caller that reads the card and POSTs to
   the advertised URL gets a 404. Fleets are discoverable but not addressable, and there
   is no load-balancing or task affinity across members.

Plus two isolation gaps this RFC closes while it is in the CRD: **per-fleet model budget**
(today a `ModelPool` budget is shared across every fleet that uses the pool, so one fleet
starves the rest) and **shard identity injection** (a shard StatefulSet gives each pod
`N` replicas but no way to learn its `K/N` — RFC 0003 §9.1's frozen defect).

This RFC specifies the **additive `AgentFleet` CRD shape** and the mechanisms behind it.
It is **strictly additive to `agents.x-k8s.io/v1alpha1`** (RFC 0005 §2.3): every new field
is optional with a safe default, the existing homogeneous fleet is unchanged, and no
conversion webhook or version bump is required. **§4 (the CRD) is the review artifact.**

---

## 2. Design principles

- **Additive, single-version.** New optional fields on `AgentFleetSpec`; `template` and
  `scaling` keep their exact current meaning (the worker pool). A fleet with none of the
  new fields renders byte-identically to today. (RFC 0005 §2.1 rule 2.)
- **Reuse the two primitives, invent no third.** Coordinator→worker fan-out rides the
  existing `work.*` claim queue (load-balanced, exactly-one-owner, KEDA-observable). The
  fleet's external face rides the existing A2A gateway. We *extend* both; we do not add a
  parallel bus.
- **The gateway is the only policy enforcement point (PEP).** All *external* A2A into a
  fleet goes through the gateway's `enforce_access` (OIDC/claims/trusted-proxy), exactly
  as for a single agent. *Internal* coordinator↔worker traffic rides the queue, whose
  holder-attestation (RFC 0015) is the internal boundary — so members never dial each
  other directly and the direct-mesh attestation problem (RFC 0014 §4) does not arise for
  intra-fleet work.
- **The coordinator is a normal conformant agent.** It is not a new binary or a
  privileged role — it is an `AgentSpec` like any other, distinguished only by a label and
  by the operator wiring it as a work *producer* (and A2A front door) instead of a
  *consumer*. Any agent that can `work.submit` + collect `work.result` can be a
  coordinator.
- **Correctness before elasticity.** The work fabric's exactly-once semantics
  (RFC 0011, the `claim_key`-keyed store) are preserved; result and dead-letter writes
  fold into the *same* atomic transitions (`ack`, `sweep`, lazy-reclaim), never a second
  round-trip that would reopen the two-owner race.

---

## 3. Topology — coordinator + workers

```
                 ┌───────────────────────── AgentFleet "research" ─────────────────────────┐
   A2A caller    │                                                                          │
  (external) ───▶│  gateway  POST /fleets/{ns}/research   ──▶  COORDINATOR pod (main agent) │
                 │  (PEP: OIDC/claims)                          │  plans, work.submit ×K     │
                 │                                              ▼                            │
                 │                                   ┌──────── work fabric ────────┐         │
                 │                                   │ coordination server (RFC11) │         │
                 │                                   │  submit → claim → ack+result│         │
                 │                                   └───┬─────────┬─────────┬─────┘         │
                 │        WORKER pool (elastic, KEDA)    ▼         ▼         ▼               │
                 │                                     w-0       w-1       w-2  … (claim)     │
                 │  coordinator collects work.result(work_id) ◀──┴─────────┴─────           │
                 └──────────────────────────────────────────────────────────────────────────┘
```

- **Worker pool** = today's `template` + `scaling` (claim or shard). Unchanged. Elastic
  under KEDA off the queue backlog (RFC 0011).
- **Coordinator** = the new optional `coordinator` block: a small (default 1-replica)
  Deployment of a *main agent* that receives the fleet's external A2A request, decomposes
  it, `work.submit`s subtasks, collects results, and returns the aggregate. It carries the
  label `agentctl.dev/fleet-role: coordinator`; workers carry `…: worker`.
- **A load-balanced fleet with no main agent** is just a fleet with no `coordinator`: the
  gateway round-robins external A2A directly across the worker replicas (§6).

This maps the user's three asks onto existing machinery: *"main agent + subagents"* =
coordinator + worker pool; *"load balancing"* = gateway LB across workers (§6) and
exactly-one-owner claim distribution; *"clustering"* = the worker pool + shard mode (§8).

---

## 4. The CRD — additive `AgentFleet` shape (review artifact)

New fields on `AgentFleetSpec`, plus four new supporting types. Nothing existing changes
type or meaning. Rust (the source of truth in `crates/agent-api/src/lib.rs`), then a YAML
example.

```rust
pub struct AgentFleetSpec {
    // ── existing, unchanged ────────────────────────────────────────────────
    pub template: AgentSpec,            // the WORKER agent definition
    pub scaling: Scaling,               // worker scaling regime (claim | shard)
    pub work_source: Option<String>,    // shared work source (coordination MCP URI)
    pub replicas: Option<u32>,          // worker count (scale subresource target)

    // ── NEW, all optional & additive ───────────────────────────────────────

    /// The fleet's "main agent". When set, the operator renders an additional
    /// single-role Deployment (label `agentctl.dev/fleet-role: coordinator`) and
    /// wires it as the fleet's A2A front door + work producer. Absent ⇒ a
    /// headless worker pool (today's behaviour), load-balanced directly by the
    /// gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<Coordinator>,

    /// Per-fleet model budget, enforced by the ModelGateway IN ADDITION to the
    /// ModelPool budget (RFC 0012). Isolates one fleet's spend from another's
    /// even when they share a pool. Absent ⇒ only the pool cap applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<FleetBudget>,

    /// Work-fabric policy for this fleet's items: dead-letter threshold and the
    /// default lease TTL. Absent ⇒ unbounded redelivery + server-default TTL
    /// (today's behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_policy: Option<WorkPolicy>,
}

/// The fleet's coordinator ("main agent").
pub struct Coordinator {
    /// The coordinator agent definition — a normal AgentSpec (its own image,
    /// instruction, mode, model, MCP tools). Typically `mode: reactive` (a
    /// long-lived planner that accepts A2A) or `mode: workflow`.
    pub template: AgentSpec,

    /// Coordinator replica count. Default 1 (a singleton main agent). >1 is
    /// allowed for HA but the replicas are peers, not shards — they must
    /// coordinate via the work fabric like any other producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,

    /// How the coordinator reaches the workers. `queue` (default): the operator
    /// wires the coordinator as a producer on the fleet `workSource`; workers
    /// claim (load-balanced, elastic). `a2a`: the operator injects an
    /// `--a2a-peer worker=<gateway>/fleets/<ns>/<name>` so the coordinator
    /// delegates point-to-point through the gateway PEP (RFC 0013).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<Distribution>,
}

#[serde(rename_all = "lowercase")]
pub enum Distribution {
    #[default]
    Queue, // fan-out over the work.* claim queue (elastic, load-balanced)
    A2a,   // point-to-point delegation through the gateway
}

/// Per-fleet model budget (RFC 0012, the intelligence plane).
pub struct FleetBudget {
    /// Total tokens this fleet may consume against its ModelPool, across all
    /// members. Enforced by the ModelGateway reservation path (RFC 0012) keyed
    /// by (namespace, pool, fleet), in addition to the pool-wide cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
}

/// Work-fabric policy for the fleet's items (RFC 0011).
pub struct WorkPolicy {
    /// Dead-letter an item after it has been redelivered this many times without
    /// a terminal `ack`. Absent ⇒ unbounded redelivery (today). A poison item is
    /// moved to the `deadletter` state (surfaced at `dlq://items`) instead of
    /// cycling forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,

    /// Default lease TTL (ms) the operator advertises to workers for this fleet's
    /// claims. Absent ⇒ the agent/server default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_ttl_ms: Option<u64>,
}
```

**CEL / validation deltas (additive only):**
- No change tightens an existing rule. The existing `scaling.mode != 'shard' ||
  has(scaling.shards)` still applies to the *worker* scaling.
- New rule constraining only the new field: `!has(self.coordinator) ||
  self.coordinator.template.mode != 'once'` — a coordinator must be long-lived (a `once`
  coordinator would exit and never serve the front door). Rejects only *new-shaped*
  objects, so it is additive.
- `budget.maxTokens`, `workPolicy.maxAttempts`, `coordinator.replicas` are `>= 0` /`>= 1`
  bounded via schemars, mirroring existing numeric fields.

**Example — a research fleet with a planner and an elastic worker pool:**

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: AgentFleet
metadata: { name: research, namespace: agents }
spec:
  # The elastic worker pool (unchanged shape).
  template:
    image: ghcr.io/agentd-dev/agentd:2.0.0
    instruction: "Execute one research subtask from the queue; ack with a result."
    modelPool: shared-sonnet
  scaling: { mode: claim, min: 0, max: 20, target: { signal: backlog, value: "5" } }
  workSource: "https://agentctl-coordination.agentctl-system.svc.cluster.local.:8080/mcp"

  # NEW — the main agent.
  coordinator:
    replicas: 1
    distribution: queue
    template:
      image: ghcr.io/agentd-dev/agentd:2.0.0
      mode: reactive
      instruction: "Decompose the incoming brief into subtasks, submit them, aggregate results."
      modelPool: shared-opus
      surfaces: { a2a: true }

  # NEW — isolate this fleet's spend from other fleets sharing the pools.
  budget: { maxTokens: 5000000 }

  # NEW — stop poison items; give claims a 2-minute lease.
  workPolicy: { maxAttempts: 5, claimTtlMs: 120000 }
```

Removing every `# NEW` block yields exactly today's homogeneous claim fleet — the proof
of additivity.

---

## 5. Rendering — from one workload to a role set

`render_fleet` today returns a single `Rendered` (one Deployment/StatefulSet). It becomes
role-aware **without breaking the single-role path**:

- **Signature.** `render_fleet` returns `Vec<Rendered>` (was one). A fleet with no
  `coordinator` returns a one-element vec — the exact workload it renders today. The
  controller's `apply_fleet` loops: applies each `Rendered`, and aggregates readiness
  across the set for status.
- **Labels gain a role dimension.** `managed_labels` adds `agentctl.dev/fleet-role`
  (`worker` | `coordinator`). Worker pods keep `agentctl.dev/agent=<fleet>` (so the queue,
  the scale selector, and the gateway still find them); the role label disambiguates
  members. The `scale` subresource + `ScaledObject` continue to target the **worker**
  selector only (`agentctl.dev/fleet-role=worker`), so `kubectl scale` and KEDA drive the
  pool, never the coordinator.
- **Coordinator workload.** Rendered from `coordinator.template` as its own Deployment
  named `<fleet>-coordinator`, `replicas` default 1, role label `coordinator`. It is a
  normal agent pod (serves mTLS :8443, mounts serving-TLS + CA, keyless intelligence).
  Operator wiring by `distribution`:
  - `queue`: inject the fleet `workSource` as the coordinator's work producer endpoint
    (it `work.submit`s and polls `work.result`), and `workPolicy.claimTtlMs` as the
    advertised TTL.
  - `a2a`: inject `--a2a-peer worker=<gateway>/fleets/<ns>/<fleet>` so the coordinator's
    `a2a.delegate` reaches the worker pool through the gateway's fleet endpoint (§6).
- **Mode coercion** stays for workers (`Reactive`, so a `once` default does not CrashLoop
  a Deployment) but the **coordinator keeps its declared mode** (`reactive`/`workflow`) —
  the CEL rule forbids `once`.

The single-container / single-image assumptions in the post-render mutators
(`inject_mcp_servers`, `inject_workflow`, `inject_api_token`) are already per-`Rendered`;
looping over the vec reuses them unchanged.

---

## 6. The fleet as an addressable A2A endpoint (#4/#6)

Add the missing RPC route and member selection to the gateway.

- **New route** `POST /fleets/{ns}/{name}` → a fleet variant of `a2a_rpc`. It runs the
  **same `enforce_access`** (reading `spec.template.access`, already supported) before any
  member dial — the fleet endpoint is a PEP exactly like the per-agent endpoint. The
  `/fleets/.../agent-card.json` route already exists; this closes the "card advertises a
  URL nothing serves" hole.
- **Member selection** replaces `resolve()`'s "first Running pod" for fleets:
  - If a **coordinator** exists → route `message/send` / `message/stream` to the
    coordinator pod (`fleet-role=coordinator`). The main agent is the front door; it fans
    out internally over the fabric.
  - Else → **load-balance** across Running worker pods (`fleet-role=worker`): round-robin
    (a per-fleet atomic counter) with readiness gating. This is the "load-balanced fleet"
    with no main agent.
- **Task affinity (#6).** Live task operations must reach the member holding the task. Add
  an `owner_pod` column to `a2a_tasks` (nullable, additive), recorded on `message/send`
  with the member's pod IP. `tasks/cancel`, non-terminal `tasks/get`,
  `message/stream` resume, and `tasks/resubscribe` route back to `owner_pod`; if that pod
  is gone, terminal state is served from the store and live ops return a clean
  `-32001`/re-dispatch. `tasks/list` already aggregates by `(ns, agent)`, so a fleet's
  tasks (written under the fleet name) aggregate for free.
- **Card.** The fleet card is projected from the coordinator's manifest when present
  (else a worker's), keeping the existing sign/JWKS path.
- **Preserved constraints:** the shared mTLS member client (Management origin, chain-only
  verify), `X-Auth-*` identity forwarding, the trusted-proxy dual-listener (the new route
  is added before layering so it inherits both listeners), and the SSRF webhook guard —
  all unchanged, applied per member.

---

## 7. The work fabric — result/correlation + dead-letter (#8/#16/#46)

Extend the `work.*` surface and the `ClaimStore`; fold every write into the existing
atomic transitions (the single in-memory `Mutex`; the Postgres single-statement UPSERT /
row lock) so exactly-once is preserved.

**Correlation + result channel.**
- `work.submit` returns a **`work_id`** (the `claim_key` — already the item's stable
  identity and the store PK), so a producer can later ask about the outcome.
- `work.ack` gains an **optional `result`** argument (a JSON value), recorded *in the same
  settle transaction* that flips the item terminal — in-memory into the done entry, in
  Postgres a new `result jsonb` column on the (never-deleted) `acked` tombstone row.
- New **`work.result`** verb: given a `work_id`, returns `{state: done|pending|claimed|
  deadletter|unknown, result?}`. This is how a coordinator collects what workers produced
  without a side channel.

**Dead-letter.**
- New store columns `attempts int` + per-item `max_attempts` (from the fleet
  `workPolicy.maxAttempts`, carried on `submit`). The redelivery points —
  `sweep_expired` and the lazy-reclaim branch of `claim` — increment `attempts` and, when
  it reaches `max_attempts`, move the item to a new **`deadletter`** state *in the same
  UPDATE* that would otherwise return it to `pending` (never a separate read-then-write).
- New **`dlq://items`** resource (mirrors `work://pending`) and a **`work.deadletter`**
  admin verb to list/requeue/drop. Unbounded redelivery remains the default when
  `maxAttempts` is unset (back-compat).

Both backends implement identical semantics (the RFC-0011/#34 invariant that in-memory and
Postgres never diverge). Attestation holder-gating extends to the new verbs touching a
held lease.

---

## 8. Shard mode fixes (#7/#18/#22/#47)

- **Shard identity injection.** The operator injects `--shard auto/N` (N = `scaling.shards`)
  into the shard StatefulSet's pod args, and the agent derives its `K` from the ordinal in
  `AGENT_POD_NAME` (`<sts>-<ordinal>`). This lands RFC 0003 §9.1's P3 fix: only `N` is
  templated (identical across pods, as a StatefulSet requires); `K` is self-derived. Purely
  additive to `agent_args`.
- **Guarded resize.** Changing `N` is choreographed by the operator, not a bare
  `.spec.replicas` patch: a `Resizing` status condition, one-ordinal-at-a-time roll, and
  (where KEDA is not the owner) drain-before-reassign, so the §4.3 hazards (two live shards
  owning the same slice; orphaned items) are covered. This is the one piece that may **phase
  after** the CRD + coordinator/fabric work, since it needs an agent-side drain
  acknowledgement; flagged explicitly, not silently deferred.

---

## 9. Per-fleet budget isolation (#20)

`FleetBudget.maxTokens` is enforced by the ModelGateway's existing reservation path
(RFC 0012, the reserve→reconcile cap) with the accounting scope widened from
`(namespace, pool)` to `(namespace, pool, fleet)`. The gateway already resolves the
caller's fleet (a fleet pod's identity *is* the fleet name); it reads the fleet's
`spec.budget.maxTokens` and admits a request only if **both** the pool cap and the fleet
cap have headroom. Absent `budget`, only the pool cap applies (unchanged). This makes the
just-landed atomic reservation the natural enforcement point — no new mechanism, only a
second scope.

---

## 10. Compatibility, versioning, phasing

- **Additive, no version bump.** All CRD changes are optional fields on the single served
  `v1alpha1` (RFC 0005 §2.3). `template`/`scaling` are untouched. A pre-0022 `AgentFleet`
  round-trips and renders identically. New store columns use `ADD COLUMN IF NOT EXISTS` /
  new optional map entries; new `work.*` verbs and `dlq://` are additive to the frozen-by-
  convention MCP surface (a client that does not know them is unaffected).
- **Phasing (each independently shippable, correctness-first):**
  1. **CRD types + admission** (this shape) — inert until the operator/gateway read them.
  2. **Work fabric** (result/correlation + dead-letter) — usable by any producer/worker
     immediately, fleet or not.
  3. **Coordinator rendering** (`render_fleet → Vec<Rendered>`, role labels, wiring).
  4. **Fleet A2A endpoint + LB + task affinity** (gateway route + `owner_pod`).
  5. **Per-fleet budget** (ModelGateway scope widening).
  6. **Shard identity + guarded resize** (needs agent-side drain ack; may trail).
- **Rejected alternatives:**
  - *A general `roles: Vec<FleetRole>` list* (arbitrary N roles). More powerful, but it
    demotes `template` from "the fleet" to "one role", tightening the existing required
    field, and it over-serves the stated use cases (main + workers). Kept as a future
    generalization; `coordinator` is the additive 80%.
  - *A new coordinator binary / privileged role.* Rejected — the coordinator is a normal
    conformant agent; privilege stays in RBAC + the gateway PEP, not a bespoke component.
  - *Direct coordinator↔worker A2A mesh as the default.* Rejected as the default — it
    reopens RFC 0014's direct-mesh attestation. The queue (holder-attested) is the default
    internal fan-out; `distribution: a2a` (through the gateway PEP) is the opt-in.
  - *A second message bus for results.* Rejected — the result rides the existing `ack`
    transition and a `work.result` read, so it inherits the store's exactly-once guarantees
    for free.

---

## 11. What this unlocks

With 0022 an `AgentFleet` is a **programmable distributed agent system**: a planner that
decomposes a brief and fans it to an elastic, budgeted, self-healing (dead-lettering)
worker pool, reachable as one authenticated A2A endpoint, load-balanced, with per-fleet
cost isolation — all on the primitives already built (claim queue, gateway PEP, reservation
budget), all additive to the CRD. It is the orchestration layer that makes "give agents a
purpose and let Kubernetes run them at scale" a first-class control-plane capability rather
than a pattern the user has to assemble by hand.
