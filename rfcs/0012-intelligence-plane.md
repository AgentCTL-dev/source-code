# agentctl RFC 0012: Intelligence plane — the egress proxy, control-plane resilience, zero-secret dial & cost governance

**Status:** Proposed (agentctl intelligence track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the runtime/data path behind the **one endpoint a conformant agent dials for intelligence**: a governable, secret-free, resilient egress proxy fronting the `IntelligenceService`/ModelPool, kept out of the node-agent

> **Contract-first, not agent-first (P0).** A conformant agent reaches **exactly one
> intelligence endpoint** named by a single URI it is handed at provisioning time,
> dialed over the substrate (agentctl RFC 0002). This RFC owns what lives *behind*
> that endpoint. It is written against **the contract** — the intelligence transport
> (a list of endpoints, three transports, the agent's **manifest-advertised** in-binary
> dialect set (contract ask **P-dialects**; the reference impl ships two — `openai`,
> `anthropic` — but the count is its design choice, not a contract guarantee), native
> tool-calling + `usage`), the resilience machinery (ordered failover, per-endpoint
> health/breaker, all-down semantics, runtime hot-swap), the frozen token/health
> metrics, and the exit-code contract. **The reference implementation (agentd) is
> the first agent to implement these**; its branded contract surfaces
> (`AGENTD_INTELLIGENCE`, the `AGENTD_INTELLIGENCE_TOKEN[_n]`/`_FILE` env family, the
> `AGENTD_INTEL_*` failover/breaker env family, the `agentd_intel_*` / `agentd_tokens_total`
> metric series, `agentd://intelligence`, `--model-swap`, `--intelligence-names`) are
> **contract-normative-but-branded** — cited here as the reference spelling and
> flagged for neutralization under the P0 contract-extraction open question (agentctl
> RFC 0001 §9). A second-vendor conformant agent that speaks the same endpoint-list +
> `usage` + exit-code contract is managed by this plane unchanged.

> **The 0004 / 0012 split is load-bearing.** agentctl RFC 0004 owns the **schema** —
> the `IntelligenceService` (ModelPool) CRD: the ordered endpoint list, per-endpoint
> `models`/`dialect`/`backends`/`credentialRef`, `proxy.topology`, `swapPolicy`, the
> failover knobs, `fallbackDirect`, `placement.modelAware`. This RFC owns the
> **runtime/plane** — the egress **proxy data path** (deployment, in-pool load
> balancing, dialect translation, per-backend health re-export), the composition of
> the contract's **across-pool resilience** with the proxy's **within-pool**
> resilience, the **zero-secret-in-pod** rendering, **cost/token governance** and the
> `EXIT_BUDGET`/back-pressure contract ask (**P-cost**), and the per-pod/per-tenant
> **authz** the proxy enforces. Where this RFC names a CRD field, it is *citing* RFC
> 0004, not redefining it.

> **Justified on the real merits, not a manufactured contradiction.** The proxy is
> **not** here to "resolve" an RFC 0017↔0018 tension — the contract already hot-swaps
> a repointed endpoint with fresh breaker state and zero restart (agentd RFC 0018
> §5). The proxy earns its existence on three concrete jobs the agent contract
> deliberately refuses to do: **(1)** load-balance across interchangeable in-cluster
> model replicas (the agent never load-balances — sticky-primary only, agentd RFC
> 0018 §10); **(2)** hold the provider credential **off the agent pod** and terminate
> provider TLS; **(3)** translate the long tail of provider dialects to the two the
> agent ships, and be the single SSRF/egress chokepoint. Those are the merits; the
> rest of this RFC is their consequences.

---

## 1. Problem / Context

Every other plane agentctl owns reaches *into* a running agent (agentctl RFC 0002 §1).
Intelligence is the deliberate inversion: the agent dials *out*. The contract nails
the data-plane side to a deliberately thin primitive (agentd RFC 0006): the agentic
ReAct loop, running inside a subagent process, must reach **exactly one** LLM
endpoint named by `AGENTD_INTELLIGENCE`, dialed fresh per call (`Connection: close`),
and get back text + structured tool calls + a `usage{prompt_tokens,completion_tokens}`
record. That endpoint may sit behind a unix-socket sidecar, a direct `https://`, or a
`vsock:` channel to a host service (agentd RFC 0006 §2). The agent does not
load-balance, does not learn cluster topology, does not hold a price table, and holds
its provider credential only as an env/file secret behind the `resolve()` front door
(agentd RFC 0006 §6).

That thinness is correct, and it pushes a large, sharp body of work onto the control
plane. Behind the one endpoint the agent dials, agentctl must supply intelligence that
is:

- **Resilient** — a fleet of agent pods whose model is supplied by an in-cluster /
  host-side service must survive that service being rolled, moving address, flapping,
  or being swapped under a long-lived reactive daemon — *without* a fleet of crashing
  pods. The contract gives the agent ordered multi-endpoint failover, a per-endpoint
  circuit breaker, all-down back-pressure, and runtime hot-swap (agentd RFC 0018), but
  it is **agentctl** that owns *which* endpoints exist, *where* they are, and *when*
  they move (agentd RFC 0018 §1, §10 — "where is the model service is agentctl's").
- **Secret-free at the pod** — in a hostile multi-tenant cluster (the locked v1
  posture, brainstorm §0.6) a provider API key must **never** reach the agent pod. The
  pod is the untrusted blast surface; a key on it is a key one prompt-injection away
  from exfiltration. The contract makes this possible — the agent dials keyless and
  the credential is resolved elsewhere — but *elsewhere* has to be built.
- **Governable** — tokens cost money. The contract emits trustworthy token counts
  (`agentd_tokens_total{model,type}`, never an estimate, agentd RFC 0016 §4.3) and a
  per-run usage report, but it emits *tokens, not currency*, and it enforces only its
  *own* per-run budgets. Fleet-wide and per-tenant cost governance — rollups, soft
  throttles, hard budgets — is the control plane's, and it is the dimension the
  analysis found most over-claimed (brainstorm §6.2).

And it must do all of this while the data-plane pod may have **no cluster network at
all** (the kata-hybrid hardened tier, agentctl RFC 0002). The model channel is then
the **one egress the reasoning loop always blocks on** (agentctl RFC 0002 §1, §10
correction 1) — the *always-present* egress, distinct from the **optional** A2A
delegation-out dial (agentctl RFC 0013 §8), and in the lethal-trifecta model the
dangerous one. Concentrating, crediting, and governing that egress is the whole job of
this plane.

The shape of the answer is a **host/cluster-side egress proxy** — its own component,
**not** folded into the node-agent — fronted by the `IntelligenceService`/ModelPool
CRD (agentctl RFC 0004). The agent dials the proxy over the substrate, keyless; the
proxy holds the credential, load-balances within a pool, translates dialect, meters
tokens, and is the single egress chokepoint. The rest of this RFC specifies it.

---

## 2. Decision — the intelligence egress proxy plane (eight principles)

1. **Behind the one endpoint sits an egress proxy fronting the ModelPool.** The
   operator resolves an `Agent`'s bound `IntelligenceService` (agentctl RFC 0004 §4)
   into (a) the proxy's upstream config (backends + credential + dialect per pool) and
   (b) the agent's `AGENTD_INTELLIGENCE` list pointing at the **proxy** over the
   substrate, **keyless**. The agent's resilience machinery (agentd RFC 0018) operates
   over that list of *pools*; the proxy operates over the *backends* within each pool.

2. **The proxy is its own Deployment / sidecar, categorically OUT of the node-agent
   (D3).** A per-node proxy embedded in the node-agent would make the agentic loop's
   blocking LLM call a **per-node inference SPOF** and collapse the contract's
   inter-pool failover into one shared-fate hop. `proxy.topology` is `sidecar`
   (per-pod) or `node-local` (a separate Deployment) — **never** the node-agent
   (agentctl RFC 0008 §6). There MUST always be a path to keep the proxy off the hard
   inference critical path (`fallbackDirect`, §3.3).

3. **Two-level resilience, mapped onto the data/control split.** *Within* a pool
   (interchangeable backends serving the same model) the **proxy** load-balances, runs
   a per-backend breaker, and terminates provider TLS — because the agent contract
   *never* load-balances (agentd RFC 0018 §10). *Across* pools the **agent** fails over
   (sticky-primary, per-endpoint breaker, all-down back-pressure, hot-swap — agentd RFC
   0018). agentctl never re-implements the across-pool machinery; the proxy never
   leaks into it (§4).

4. **Zero-secret-in-pod is the default and the only hostile-tenant-legal topology.**
   The provider credential (`credentialRef`) is mounted into the **proxy**, never the
   agent pod; the agent's endpoint carries no key (agentd RFC 0006 §6, agentd RFC 0012
   §3.7, agentctl RFC 0004 §4.3). The `proxy.topology: none` direct-dial path — the
   only one that injects a key into the pod — is **rejected by admission on hostile
   tenancy** (agentctl RFC 0007, RFC 0004 §4.2) (§5).

5. **Endpoints are named, not indexed; dialects stay the agent's by default — and
   *which* dialects those are is read from the manifest, never assumed.** Each pool
   carries a **stable operator-assigned name** so the frozen `agentd_intel_endpoint_*`
   labels survive a list reorder (contract ask **P7**); model-aware placement keys off
   per-pool `models` (also P7) (§6). The proxy **passes through** any dialect the
   negotiated agent **advertises it supports in-binary** (read from the manifest —
   contract ask **P-dialects**; the reference impl ships exactly two, the `openai` and
   `anthropic` dialect values, but that two-adapter inventory is an agentd *design
   choice*, agentd RFC 0006 §3, **not** a contract guarantee) and **translates only the
   long tail** the agent does *not* advertise — no second tool-calling translation
   codebase for a dialect the agent already speaks (§6.4).

6. **Token metering is two-tier; `agentd_tokens_total` is the billing authority.**
   The frozen, never-estimated `agentd_tokens_total{model,type}` (agentd RFC 0016 §4.3)
   is the authoritative source for cost and quota; the proxy re-exports a
   **per-backend** `agentctl_intel_backend_*` series (agentctl's own namespace, RFC
   0010 §10.1) for the within-pool view that the agent's pod↔proxy metrics can no
   longer see (§4.5, §7.1).

7. **Cost governance is tiered: per-run (contract-enforced) → fleet best-effort
   (v1) → fleet hard (gated on P-cost + a shared store).** v1 ships per-run budgets
   (the contract limits box → `EXIT_BUDGET`) plus best-effort fleet/tenant throttling
   off the rollups (agentctl RFC 0010); **hard, fleet-wide budget enforcement** needs
   a clean budget-exhausted signal the proxy raises and the agent maps to
   `EXIT_BUDGET(7)` on `once` / readiness back-pressure on reactive (contract ask
   **P-cost**) **and** a shared accounting store — and is honestly demoted to a gated,
   post-v1 capability (§7).

8. **The model channel is the egress the reasoning loop always blocks on, and the
   proxy is where it is governed.** "Networkless pod" does not mean "no egress" — the
   model dial is a live egress leg even over vsock (agentctl RFC 0002 §10 correction 1).
   It is the *always-present* egress (the A2A delegation-out dial, agentctl RFC 0013 §8,
   is the other, optional one); the proxy is the single point where the model egress
   terminates TLS, enforces SSRF/allow-listing, applies per-tenant authz, and is metered
   — the honest realization of "vsock-everything" (§8).

### 2.1 What this RFC owns vs reuses (the boundary)

| Concern | Home | Note |
|---|---|---|
| `IntelligenceService`/ModelPool **schema** (endpoints, `credentialRef`, `proxy`, `swapPolicy`, `failover`, `fallbackDirect`, `placement.modelAware`) | **agentctl RFC 0004** | this RFC cites it; never redefines a field |
| The egress **proxy data path** (deploy, in-pool LB, dialect xlate, per-backend health, metering, authz) | **agentctl RFC 0012 (here)** | the *execution* of the RFC 0004 declaration |
| Across-pool **failover / breaker / all-down / hot-swap** | **the contract** (agentd RFC 0018) | agentctl supplies/moves the list; the agent runs the machine |
| Intelligence **wire / dialects / keyless dial / `usage`** | **the contract** (agentd RFC 0006) | the proxy speaks the agent-facing half of this wire |
| **Substrate reach** (how the pod's dial crosses to the proxy; vsock egress restriction) | **agentctl RFC 0002 / RFC 0015** | this RFC consumes the descriptor + the egress-restriction mandate |
| Proxy-**out**-of-node-agent invariant + per-node-SPOF argument | **agentctl RFC 0008 §6** | restated, not re-derived |
| Token **metering schema**, cost rollups, run-report capture | **agentctl RFC 0010** | this RFC consumes the rollups; owns *enforcement* |
| **Price table** + chosen token source (tokens → currency) | **agentctl RFC 0012 (here)** | RFC 0010 §9.2 defers it here (this plane owns the token source + enforcement); §7.1, §Open 2 |
| **Renderer** that compiles a pool into proxy config + pod env | **agentctl RFC 0006** | this RFC specifies *what* it renders |
| **Admission** rejecting `topology:none`/`fallbackDirect` on hostile tenancy, cross-ns refs | **agentctl RFC 0007** | this RFC specifies the rule; 0007 executes it |
| **Tenancy/PKI** trust model, the guest→host vsock egress restriction | **agentctl RFC 0015** | this RFC consumes; 0015 owns the threat model |

---

## 3. Placement — the proxy is its own component, out of the node-agent (D3)

> **Reconciliation with RFC 0001's component inventory.** RFC 0001 §1 fixes the stack
> for the **five control-plane components** (operator, node-agent, A2A gateway, CLI,
> KEDA scaler). The intel-proxy is a deliberate **sixth, data-PATH component** — it sits
> on the blocking LLM call path, terminates provider TLS, and translates dialect — and
> is therefore *not* control-plane. It is Rust (consistent with RFC 0001's all-Rust
> decision) and carries its own perf/HA posture (§3.3), but its existence and ownership
> are recorded here, not in RFC 0001's control-plane five.

### 3.1 Why not the node-agent (restated, binding)

The single most tempting consolidation — fold an intelligence proxy into the on-node
DaemonSet next to the management bridge and telemetry collector — is **categorically
wrong** and is excluded from both node-agent tiers (agentctl RFC 0008 §6, brainstorm
§6.2/D3). Three independent reasons, none recoverable by careful coding:

- **It is a per-node inference SPOF.** The agentic loop **blocks** on the model call
  (agentd RFC 0006 §7, RFC 0007) — reasoning cannot advance until the model answers.
  A per-node proxy crash therefore stalls reasoning on **every local pod on the
  node**, not just the node-agent's own work. That is the exact inverse of the
  bounce-safe invariant the node-agent's control tier is built to guarantee
  (agentctl RFC 0008 §3.3): management/telemetry tolerate a node-agent bounce;
  **inference does not**.
- **It collapses inter-pool failover into one shared-fate hop.** The contract's
  failover/breaker/all-down machinery (agentd RFC 0018) operates *across* pool
  endpoints. Terminating multiple pool endpoints on one per-node process makes that
  machinery shared-fate and blinds the frozen `agentd_intel_endpoint_*` metrics —
  they would measure pod↔proxy, not pod↔model, for *all* pools at once (§4.5).
- **It concentrates the dangerous egress in the most privileged host process.** The
  model channel is the irreducible, lethal-trifecta-relevant egress leg (agentctl RFC
  0002 §10 correction 1). Putting it in the same process that holds god-mode over
  every local pod (drain/cancel/inject) is precisely the privilege concentration the
  node-agent decomposition exists to forbid (agentctl RFC 0008 §2.1).

### 3.2 The two legal topologies (`proxy.topology`, RFC 0004 §4.2)

`proxy.topology` is a field on `IntelligenceService` (agentctl RFC 0004 owns it); this
RFC owns what each value *means* at runtime.

```
 (A) sidecar — per-pod, in-pod, unix:/run/intel.sock
   ┌──────────────────── Agent pod ────────────────────┐
   │  agent  ──unix:/run/intel.sock──▶  intel-proxy     │──TLS──▶ provider / in-cluster model svc
   │  (no key, dialect=pool's)          (holds cred)    │        (cluster network / internet)
   └────────────────────────────────────────────────────┘
   blast radius = ONE pod · weakest isolation (shares the pod netns ⇒ pod is network-attached, RFC 0002 §10)

 (B) node-local — separate Deployment, agent dials over the substrate
   ┌──── Agent pod (NETWORKLESS) ────┐         ┌──── intel-proxy Deployment (≥2 replicas + PDB) ────┐
   │ agent ──vsock|unix-hostPath──▶  │────────▶│ holds cred · in-pool LB · dialect xlate · meter    │──TLS──▶ provider
   └──────────────────────────────────┘         └─────────────────────────────────────────────────────┘
   blast radius = bounded by replicas · STRONGEST isolation (pod stays off the cluster network — §8)
```

| Topology | Provider cred lives in | Agent pod on the network? | Blast radius of a proxy crash | Hostile-tenant legal? |
|---|---|---|---|---|
| `sidecar` | the in-pod sidecar | **yes** (shared netns) | one pod | yes (cred off the *agent* container, but pod is network-attached — single-tenant-leaning) |
| `node-local` | the proxy Deployment | **no** (pod stays networkless) | bounded by proxy replicas | **yes** — the maximal-isolation path |
| `none` (direct dial) | the **agent pod** | yes | n/a | **no** (admission rejects, §5) |

**The honest isolation note (RFC 0002 §10).** A `sidecar` shares the pod's network
namespace, so a networked sidecar means the *pod* is network-attached even if the
agent container believes it is dialing only a unix socket — the agent is **not**
network-isolated in that topology. For the strongest posture (a truly networkless
pod), the proxy MUST be **off-pod** (`node-local`), reached over the substrate
(vsock / unix-hostPath), with its own netns carrying the egress. The kata-hybrid
hardened tier (agentctl RFC 0002) therefore pairs with `node-local`; `sidecar` is the
dev / single-tenant convenience.

### 3.3 The proxy is never a *hard* SPOF

The proxy is a component that can fail, and a single intelligence egress is exactly
the thing that must not become a fleet-wide stall. Three layers keep it soft:

1. **`fallbackDirect` (RFC 0004 §4.2)** appends a **non-proxy** endpoint to the
   agent's `AGENTD_INTELLIGENCE` list, so a proxy outage fails *over* (agentd RFC 0018
   §3.3) rather than down. That fallback carries a credential into the pod, so it is
   **single-tenancy only** — admission rejects `fallbackDirect: true` on a hostile
   class (agentctl RFC 0007).
2. **For hostile tenancy (no `fallbackDirect`)**, isolation is bought back with
   **proxy replicas**: `node-local` runs ≥2 replicas behind a Service with a PDB
   (agentctl RFC 0006 renders this); `sidecar`'s blast radius is already one pod (it
   shares that pod's fate regardless).
3. **A total proxy outage degrades through the contract, not into a crash-loop.**
   With every endpoint unreachable the agent enters the contract's documented
   all-endpoints-down semantics (agentd RFC 0018 §6): a `once` Job exits
   `EXIT_INTELLIGENCE(4)` and the scheduler retries; a `loop`/`reactive` daemon does
   **not** exit — it enters jittered all-down backoff and flips **not-ready**
   (back-pressure), resuming the instant a proxy replica returns. Liveness is **not**
   failed (a dead upstream is not a wedged supervisor). So even the worst case is a
   graceful, observable degradation, not a thundering herd of pod restarts.

---

## 4. Resilience — the contract's machinery at the control-plane level

This section is the heart of the plane: how agentctl makes the one endpoint resilient
by **composing** the proxy's within-pool resilience with the agent's across-pool
resilience, with a clean seam between them.

### 4.1 The two-level division (binding)

```
   Agent pod (keyless, dialect = pool's)            IntelligenceService (RFC 0004 §4)
   ┌──────────────────────────────────┐             ┌──────────────────────────────────┐
   │ AGENT / CONTRACT (agentd RFC 0018)│  ordered    │ endpoints[] (ORDER = failover)    │
   │  ACROSS pools:                    │◀──list of──▶│  [0] opus-primary  (pool)         │
   │   sticky-primary, lowest-idx-     │   POOLS     │  [1] opus-fallback (pool)         │
   │   healthy, per-endpoint breaker,  │  over the   │  …                                │
   │   all-down backoff, hot-swap      │  substrate  └───────────────┬──────────────────┘
   │   (NEVER load-balances — §10)     │  (keyless)                  │ each pool: backends[] + cred
   └───────────────┬───────────────────┘                            ▼
                   │ dials the active pool's PROXY endpoint
                   ▼
   ┌──────────────────────────────────────────────────────────────────────────────────┐
   │ PROXY / agentctl (this RFC)  —  WITHIN a pool:                                      │
   │   load-balance across interchangeable backends (the agent never does)              │
   │   per-BACKEND circuit breaker · TLS termination · dialect translation              │
   │   re-export per-backend health as agentctl_intel_backend_* (§4.5)                  │
   └──────────────────────────────────────────────────────────────────────────────────┘
```

| Axis | Who | Mechanism | Source of truth |
|---|---|---|---|
| **Across pools** (primary→fallback, different model svc/region) | **the agent** | ordered list, sticky-primary, per-endpoint breaker, all-down backoff | agentd RFC 0018 §3–§6 |
| **Within a pool** (interchangeable replicas, same model) | **the proxy** | LB (weighted/least-latency), per-backend breaker, TLS, dialect xlate | this RFC §4.4 |
| **Deciding** to swap/repoint (cost/latency policy) | **agentctl** | edit `IntelligenceService` → re-render → reload | this RFC §4.3 / RFC 0004 §4.5 |
| **Executing** a swap (quiesce→switch→resume) | **the agent** | turn-boundary hot-swap, `--model-swap` policy | agentd RFC 0018 §5 |

The seam is clean: the agent sees pools as opaque endpoints and never learns a backend
exists; the proxy sees backends and never learns there is a fallback *pool*. Neither
re-implements the other.

### 4.2 Across-pool: the agent runs it; agentctl supplies the list

The operator renders the resolved `IntelligenceService.endpoints[]` into the agent's
ordered `AGENTD_INTELLIGENCE` list (one entry per pool, in failover order) and the
RFC 0018 failover/breaker env (`AGENTD_INTEL_BREAKER_*`, `AGENTD_INTEL_ALLDOWN_BACKOFF`
— branded, flagged for neutralization, RFC 0004 §4.1). agentctl adds **nothing** to
the failover algorithm; it owns only *which pools exist and in what order*. A
single-pool `IntelligenceService` renders a single-element list — the contract's
resilience code paths are inert (agentd RFC 0018 §3.1) and the plane is exactly RFC
0006 behind a proxy.

### 4.3 Hot-swap is two independent layers (no agent restart, ever)

A backend move must never restart an agent. There are **two** distinct hot-swap
layers, and the cleaner one is invisible to the agent entirely:

- **Layer 1 — proxy-internal (within-pool), the agent sees nothing.** Repointing a
  backend address, swapping a `credentialRef`, or changing LB weights edits only the
  proxy's upstream config. The agent's endpoint (the proxy's substrate address) does
  **not** move, so `AGENTD_INTELLIGENCE` is unchanged and there is nothing to reload
  on the agent. The proxy reloads its own config (file-watch, the same Secret-mount
  rotation discipline as §5.3) and the next agent call transparently lands on the new
  backend. **This is the preferred backend-move path** — a credential rotation or a
  model-replica reschedule costs the agent zero turns.
- **Layer 2 — across-pool (the agent's quiesce→switch→resume).** Adding, removing, or
  reordering a *pool*, or changing the **model**, changes `AGENTD_INTELLIGENCE` /
  `model`. The operator re-renders the env (via a stable-named reloadable ConfigMap,
  agentctl RFC 0006) and signals a hot reload (agentd RFC 0017); the agent executes
  the contract's **quiesce → switch → resume** at a turn boundary per `swapPolicy`
  (agentd RFC 0018 §5): `finish-on-old` (default) lets the in-flight turn complete on
  the old config and the next turn use the new; `restart-turn` re-runs the current
  turn on the new model. A repointed pool starts with a **fresh** breaker record (no
  stale state to a new address). No restart, no dropped in-flight work.

The control-plane lever that *decides* a swap (small↔large for cost, region for
latency, a fleet-wide model migration) is agentctl's (§7, agentctl RFC 0004 §4.5);
the agent only executes the primitive (agentd RFC 0018 §5.3, "deciding to swap is
agentctl's").

### 4.4 Within-pool: what the proxy actually does

The proxy is the component agentd RFC 0006 always assumed existed for everything its
two in-binary adapters do not cover ("fewer adapters, thinner binary, push provider
quirks to the gateway," agentd RFC 0006 §2.4). It MUST:

- **Load-balance across `backends[]`** with weighted or least-latency selection
  (agentctl's choice; the agent contract forbids itself from doing this, agentd RFC
  0018 §10). Health-aware: a backend failing its per-backend breaker is removed from
  rotation.
- **Run a per-backend circuit breaker** (the within-pool analogue of the agent's
  per-endpoint breaker). `IntelligenceService.endpoints[].circuitBreaker` (RFC 0004
  §4.1) parameterizes it. A pool with **all** backends open presents the *pool* as
  unhealthy to the agent (connection refused / 503 on that endpoint), so the agent's
  across-pool failover engages — the two breakers compose.
- **Terminate provider TLS** and hold the credential (§5). The agent dials the proxy
  in plaintext over the substrate-local socket (the boundary is the transport, not
  TLS — agentd RFC 0012 §3.8); the proxy makes the real `https://` egress.
- **Translate dialect only for the long tail** (§6.4).
- **Propagate `traceparent`** received on the agent's call through to the backend, so
  one model call is a single connected trace; the proxy hop is its own span (agentctl
  RFC 0010 §8 owns the correlation model). The proxy MUST NOT mint or strip trace
  context.
- **Meter `usage`** per backend for the within-pool cost view (§7.1), and **never log
  or forward the credential** anywhere but the upstream `authorization`/`x-api-key`
  header (the same never-logged discipline as agentd RFC 0006 §6).

The proxy is **non-streaming agent-facing** (agentd RFC 0006 is `stream:false`): if a
backend streams, the proxy de-streams (buffers) the response and returns one body,
capped (the de-stream cap is an open question, §Open). The proxy holds **no
keep-alive state to the agent** — the agent dials fresh per call (agentd RFC 0006 §7).

### 4.5 Per-endpoint health is two-tier (the metrics correction)

Collapsing a pool to one proxy endpoint **changes what the frozen
`agentd_intel_endpoint_*` metrics mean** — they now measure the **pod↔proxy hop**, not
pod↔model (brainstorm §6.2). This is acceptable *only if* agentctl restores the lost
within-pool view on its own series. The binding result is a two-tier health story:

| Series | Owner | "endpoint"/key means | Measures |
|---|---|---|---|
| `agentd_intel_endpoint_up` / `_active` / `_latency_ms` / `agentd_intel_calls_total{result}` / `agentd_intel_all_down` (frozen, agentd RFC 0016 §4.3) | the **agent** | a **pool** (proxy endpoint), labeled by stable name (P7) or list index | the **pod↔proxy** hop and across-pool failover state |
| `agentctl_intel_backend_up` / `_latency_ms` / `agentctl_intel_backend_calls_total{result}` / `agentctl_intel_backend_breaker_opens_total` (this RFC, agentctl namespace, RFC 0010 §10.1) | the **proxy** | a `{pool, backend}` pair | the **proxy↔model** hop, in-pool LB + breaker state |

Dashboards stack the two: `agentd_intel_endpoint_active{endpoint="opus-primary"}` says
*which pool the pod prefers*; `agentctl_intel_backend_up{pool="opus-primary",backend="…"}`
says *which replicas within it are live*. The live human view stitches both —
`agentd://intelligence` (agentd RFC 0018 §4.4, the agent's pod↔proxy view) plus the
proxy's own `/healthz/backends` — into `kubectl agents describe <x>` (agentctl RFC
0008/0010 read it; agentctl RFC 0016 renders it). The semantics MUST be documented at
every read site: **in this plane, the agent's intel metrics are the proxy hop.**

### 4.6 Model discovery (optional, capability-negotiated)

The contract's optional `GET /v1/models` discovery (agentd RFC 0018 §5.4) surfaces
served models into the manifest `intelligence.models` — but it is **OpenAI-dialect
only**: the `anthropic` dialect has no list endpoint, so discovery yields nothing for
an anthropic-dialect pool (agentd RFC 0018 §5.4). So:

- **For an `openai`-dialect pool**, the proxy answers `/v1/models` with the **union (or
  per-backend set) of its backends'** models, so the agent's discovery sees the pool's
  true capability.
- **For an `anthropic`-dialect pool** (e.g. the §8.1 worked example), the agent never
  probes `/v1/models` and the proxy's endpoint is inert — the declared
  `endpoints[].models` (P7) is the **only** capability signal there, which strengthens
  the prefer-declared stance below.

In both cases agentctl **prefers the declared** `endpoints[].models` (RFC 0004, P7) for
placement (it is authoritative ops intent and needs no live probe) and uses live
discovery — *where the dialect supports it* — as a **cross-check / drift signal**
(declared-but-undiscovered ⇒ a `Degraded` condition, not a hard failure). Discovery
never blocks startup and degrades silently when a backend or dialect does not support
it (agentd RFC 0018 §5.4).

---

## 5. Zero-secret-in-pod

### 5.1 The load-bearing property

**The provider credential is resolved at the proxy and never enters the agent pod.**
This is the single most important security property of the plane in a hostile
multi-tenant cluster, and it rides three contract facts the agent already guarantees:

- secrets are env/file-only behind the `resolve()` front door and are **never read
  from the config file** (agentd RFC 0006 §6);
- the credential carrier (`Token`/`Secret`) is **structurally unserializable** — it
  has no `Serialize`, so it cannot reach the capabilities manifest, `Agent.status`, a
  log line, or a trace (agentd RFC 0006 §6 / RFC 0012 §3.7);
- on the keyless tier the agent dials a substrate-local socket with **no key in the
  URI and no token env** (agentd RFC 0018 §3.2 — "the list never carries a secret";
  agentctl RFC 0002 §10).

### 5.2 Where the credential lives, by topology

| `proxy.topology` | Credential mounted into | Agent pod env | Agent's `AGENTD_INTELLIGENCE` → | Hostile-tenant? |
|---|---|---|---|---|
| `sidecar` | the in-pod sidecar (mounts `credentialRef`) | **no** `AGENTD_INTELLIGENCE_TOKEN*` | `unix:/run/intel.sock` (substrate-local, keyless) | yes (cred off the agent container) |
| `node-local` | the proxy Deployment | **no** `AGENTD_INTELLIGENCE_TOKEN*` | the proxy endpoint over vsock/unix-hostPath, keyless | **yes** (the zero-secret path) |
| `none` (direct dial) | **the agent pod** (`AGENTD_INTELLIGENCE_TOKEN[_n]`/`_FILE`) | the provider key | the provider endpoint directly | **no** — admission rejects |

In the default proxy-fronted topologies the operator renders `AGENTD_INTELLIGENCE` to
the proxy endpoints and injects **no** token env into the agent pod; the
`credentialRef` `Secret` is mounted **only** into the proxy. Only `topology: none`
injects the contract's `AGENTD_INTELLIGENCE_TOKEN[_n]`/`_FILE` env into the pod — the
explicitly non-zero-secret, single-tenant-only path, rejected by admission on a
hostile class (agentctl RFC 0007, RFC 0004 §4.2/§4.3). The `credentialRef` value
appears **nowhere** in the `IntelligenceService` CRD, the manifest, `Agent.status`,
proxy logs, or traces.

### 5.3 Secret lifecycle

- **At rest:** a Kubernetes `Secret` (or an external-secrets-projected file). The
  proxy mounts it as a **file**, not an env var, so rotation is a file replacement the
  proxy picks up live — the same per-resolve / file-rotation posture the contract uses
  (agentd RFC 0006 §6: k8s Secret mounts / Vault Agent sidecars rotate by replacing
  the file). No proxy restart on rotation.
- **Rotation:** edit/replace the `Secret` → the proxy re-reads on the next request (or
  a debounced reload). **The agent is never involved** — provider key rotation never
  touches the agent pod, never triggers an agent reload, never drops in-flight work.
  This is a direct win of moving the key off the pod: the most frequent secret
  operation (key rotation) is invisible to the data plane.
- **Scoping (multi-tenant, binding):** the `credentialRef` is namespaced with the
  `IntelligenceService`. A pool MUST NOT reference a `Secret` in another namespace
  without an explicit cross-namespace grant (admission rejects it, agentctl RFC 0007),
  and the proxy MUST enforce a **`peer → Agent → allowed-pools`** authz map (§5.4) so
  one tenant's pod cannot dial another tenant's pool and spend its credential. A
  per-node shared proxy listener with unscoped cross-namespace refs is the exact
  credential/budget-theft hole the analysis flagged (brainstorm §6.2) and is forbidden
  by construction.

### 5.4 Per-pod / per-tenant authz at the proxy

A `node-local` proxy serves multiple pods (and, in a shared deployment, multiple
tenants). It MUST authenticate the dialing peer and authorize it against an
operator-maintained map:

```
   peer identity (substrate-attested) ──▶ Agent (namespace/name) ──▶ allowed IntelligenceService pools
```

- On the **kata-hybrid** substrate the peer is a per-VM uds / attested vsock peer
  (agentctl RFC 0002 §6, RFC 0008 discovery) — the proxy keys authz off that attested
  identity, not a self-asserted header.
- On the **stock-unix** substrate, `SO_PEERCRED` + the operator-assigned socket
  mapping (agentctl RFC 0002 §6) identify the pod.
- The map is rendered by the operator from the `Agent`↔`IntelligenceService` bindings;
  a dial to a pool the peer is not bound to is **refused** (and counted —
  `agentctl_intel_authz_denied_total{reason}`). This is the same chokepoint at which a
  per-tenant **budget** is checked once hard enforcement exists (§7.3). A `sidecar`
  proxy is single-pod and needs only the trivial "this pod, its pools" map.

This authz map and its trust roots are the security plane's (agentctl RFC 0015); this
RFC requires the proxy to be its enforcement point.

---

## 6. Model routing, pools & per-Agent binding

### 6.1 How an Agent selects a pool

Binding is owned by agentctl RFC 0004 §4.5; this RFC consumes the resolved result.
Resolution order (most specific wins): `Agent.spec.intelligence` (inline ordered
list) **>** `Agent.spec.intelligenceRef` **>** `AgentClass.spec.defaultIntelligenceRef`.
Intelligence is **replace, never merge** (the order *is* the failover policy; a partial
merge would silently reorder it — RFC 0004 §6.1). `Agent.spec.model` selects which of
the pool's models to request and overrides the pool default. The renderer (agentctl
RFC 0006) compiles the resolved pool into the proxy upstream config + the agent's
`AGENTD_INTELLIGENCE` list; a backend/credential move is a **one-object edit** of the
`IntelligenceService` (RFC 0004 §4.5).

### 6.2 Stable per-endpoint names (P7) — why metric labels survive reorder

The contract today labels endpoints by **list index** (`"0"`, `"1"`, …, agentd RFC
0018 §4.3). That index **shifts** when an operator inserts a new primary ahead of the
old one — a routine swap — so every `agentd_intel_endpoint_*` series silently
re-points and dashboards/alerts break. The fix is contract ask **P7**: stable,
operator-assigned endpoint **names** passed via the reference impl's
`--intelligence-names` (agentd RFC 0018 §11), so the label is `endpoint="opus-primary"`
and survives a reorder.

- agentctl always records `endpoints[].name` (RFC 0004 §4); the operator passes the
  names list when the negotiated contract supports P7, and **degrades to index labels**
  (with a documented dashboard-churn caveat) when it does not.
- The proxy's own `agentctl_intel_backend_*` series (§4.5) are likewise keyed by the
  stable `{pool, backend}` names, so the within-pool view is reorder-stable
  independent of P7.

### 6.3 Model-aware placement (P7)

With `placement.modelAware: true` (RFC 0004 §4.4) and per-pool `models` (P7), the
renderer routes an `Agent` whose `model` is `claude-opus-4` onto a node/pod whose
bound pool **serves** opus, adding the corresponding node/endpoint affinity. Without
P7, the operator assumes only the configured `model` and skips model-aware affinity
(the agent still works — it just dials its single configured model). This matters most
for the `node-local` topology where a proxy Deployment is pinned near specific model
backends (e.g. GPU-local vLLM): model-aware placement co-locates the agent with the
proxy that fronts its model.

### 6.4 Dialects stay the agent's; the proxy translates only the long tail

A naive design forces `DIALECT=openai` on every pool and re-implements tool-calling
translation in the proxy — which **orphans the agent's shipped adapters** and
duplicates the most error-prone code (native tool-calling round-trips) in a second
codebase (brainstorm §6.2). The principled split is **contract-driven, not keyed to a
specific binary's adapter inventory** (P0): the pass-through/translate boundary is read
from the agent's **declared dialect capability** in the manifest (contract ask
**P-dialects**, the ask boxed below), honoring agentd RFC 0006 §2.4:

- For **any dialect the negotiated agent advertises it supports in-binary**, the
  **dialect stays the agent's**. `endpoints[].dialect` (RFC 0004 §4.1) tells the agent
  which of *its* adapters to use; the proxy passes the wire through (it still terminates
  TLS, LBs, and meters, but does not re-encode the request/response). No second
  tool-calling translator. The reference impl advertises exactly the `openai` and
  `anthropic` dialect **values** (agentd RFC 0006 §4, `AGENTD_INTELLIGENCE_DIALECT`
  default `openai`) — but a second-vendor conformant agent that advertises one dialect,
  three, or a different pair is handled by construction, because the boundary is *what
  the manifest advertises*, not a hardcoded `{openai, anthropic}` set.
- For the **long tail the agent does NOT advertise** (Gemini, Vertex-native,
  Bedrock-native, or any provider whose native dialect is not in the agent's declared
  set — the providers agentd deliberately keeps *out* of its binary, agentd RFC 0006
  §2.4/§4), the proxy **is** the gateway agentd RFC 0006 always assumed: it translates
  the backend's native dialect into one of the dialects the agent **does** advertise, so
  the agent still speaks an adapter it ships.

> **Contract ask P-dialects (NEW).** The pass-through-vs-translate decision needs the
> agent to advertise its supported dialect set in the manifest (e.g.
> `intelligence_summary.dialects: ["openai","anthropic"]`, analogous to P7's per-endpoint
> `models[]`). The contract today carries `intelligence.transport` and
> `intelligence_summary.toolmode` (agentd RFC 0015 §5.2) but **no** dialect-capability
> list, so a second-vendor agent's set is undiscoverable and this plane's core routing
> would otherwise have to assume the reference impl's two adapters. Until P-dialects
> lands, the operator falls back to the AgentClass-pinned contract major's known default
> dialect set (the reference impl's `{openai, anthropic}`) and **flags any pool whose
> `dialect` is outside it for the translation path** — never silently assuming a
> second-vendor agent matches agentd. **Contract ask: P-dialects.**
- Any pool that asks the proxy to translate between the agent's advertised dialects, or
  into one outside that set, MUST be covered by a **versioned conformance suite** for the
  openai↔anthropic↔native tool-calling + `usage` round-trip (agentctl RFC 0001's
  conformance discipline). A translation the suite does not cover is not allowed in
  prod.

---

## 7. Cost & token governance

Cost is the dimension the analysis found most over-claimed (brainstorm §6.2), so this
section is deliberately honest about what v1 enforces versus observes. Governance is
**three tiers**, strongest-and-cheapest first.

### 7.1 Metering — pick one billing authority, key by `{model, type}` incl. cache tiers

agentd emits **tokens, never currency** (agentd RFC 0016 §4.3); cost = tokens × a
**price table this plane owns** — agentctl RFC 0010 builds the cost-rollup substrate
and **defers the price table + chosen-token-source ownership here** (RFC 0010 §9.2),
because this plane owns the chosen token source and cost enforcement. The price table
is keyed by `{model, type}` because input and output tokens price differently, **and**
must carry cache-read/cache-write tiers where a provider bills them (brainstorm §6.2).
Its storage/versioning/staleness is resolved in §Open 2.

There are two possible token sources; pick **one authority** to avoid double-counting:

| Source | Pros | Cons | Verdict |
|---|---|---|---|
| `agentd_tokens_total{model,type}` (frozen, agentd RFC 0016) | contract-frozen, **never estimated** (absence is `0`, not a guess), per-pod, already scraped (agentctl RFC 0010) | `type ∈ {in,out}` only — **no cache-tier breakdown** | **the billing authority** for fleet/tenant governance |
| proxy meter (`agentctl_intel_backend_tokens_total{pool,backend,model,type}`) | sees the provider `usage` object incl. **cache_read/cache_write**; per-backend attribution | agentctl's own code, not contract-frozen; risks divergence from the agent count | **enrichment only** — cache-tier ratio + per-backend attribution, **not** summed with the authority |

The recommendation is binding for v1: **`agentd_tokens_total` is the authority** for
cost/quota; the proxy meter supplies the cache-tier *ratio* and per-backend
attribution as an `agentctl_*` series, and dashboards apply the ratio to the
authoritative count rather than summing the two. (If, post-v1, cache pricing dominates
and the contract has not added cache types to `agentd_tokens_total`, revisit making
the proxy the authority — that is the §Open price-table question.)

### 7.2 Tier 1 — per-run budgets (already contract-enforced)

The cleanest, most direct cost lever already exists and needs no new primitive: the
**contract limits box** (`maxTokens`, `treeTokenBudget`, agentd RFC 0015 §5.2), set by
the operator via `AgentClass.defaults.limits` / `Agent.spec.limits` (agentctl RFC 0004
§3, RFC 0003), caps the worst-case spend of a single run/tree. When the budget is
exhausted the agent refuses (`agentd_refusals_total{reason="budget"}` /
`agentd_limit_exceeded_total{limit="tree_tokens"}`, agentd RFC 0016 §4.3) and on `once`
exits **`EXIT_BUDGET(7)`** (agentd RFC 0016 §5, policy: default `Count`, operator may
`FailJob` via `--budget-exit-code`). agentctl's job is to **set** these limits from
the class/pool policy and **alert** on the refusal/exit (agentctl RFC 0010 §7.4). This
is real, hard, contract-enforced cost governance — for the scope of *one run*.

### 7.3 Tier 2 (v1) — best-effort fleet/tenant throttling

What Tier 1 cannot cap is spend **across many runs/pods over time** — a per-tenant or
per-fleet daily budget. No single agent sees fleet-wide spend, and a per-node proxy
sees only *local* tokens while a fleet spends across nodes (brainstorm §6.2). v1 ships
**best-effort** governance, built entirely from levers agentctl already owns:

- **Observe:** the per-tenant/per-fleet rollup `agentctl:tokens:rate5m = sum by
  (namespace, tenant, model, type) (rate(agentd_tokens_total[5m]))` (agentctl RFC 0010
  §5.3) × the price table → a running cost gauge per tenant/fleet.
- **Throttle (control-loop, reactive):** as a soft budget is approached, agentctl
  applies coarse back-pressure with existing controls — lower the fleet's KEDA `max`
  / scale toward `min` (agentctl RFC 0011), `lame-duck` the lowest-priority pods
  **only when `lame-duck` is in the negotiated agent's `surfaces.operator_tools`** (the
  authoritative operator-tool list a build exposes, agentd RFC 0015 §5.2; the lever is
  gated on the surface, never assumed — a conformant agent may not advertise it), or
  **refuse new `Agent`s at admission** (agentctl RFC 0007) for the over-budget tenant.

This is honestly *best-effort*: it is reactive (lags the spend by the rollup window),
coarse (whole-fleet, not per-request), and not a hard cap. It is the right v1 trade —
no new contract primitive, no shared accounting store, no per-request hot-path cost.

### 7.4 Tier 3 (gated) — hard fleet-wide budget enforcement

Hard, fleet-wide enforcement ("this tenant may spend N tokens/day across all agents,
period") needs **two** things v1 does not have, and is therefore explicitly demoted to
a gated, post-v1 capability:

1. **A shared accounting store** the proxies read/write, so N proxies across nodes
   enforce **one** counter (a per-node-local count gives each pod N× the budget,
   brainstorm §6.2). This is a stateful component to run/HA/DR — it directly tensions
   the thin-node-agent / stateless-proxy posture, which is why it is gated, not
   default.
2. **A clean budget-exhausted signal — contract ask P-cost.** When the shared
   counter says a tenant is over budget, the proxy must refuse the agent's call with a
   signal the agent maps **distinctly** from auth-fatal (401/403 → `EXIT_INTELLIGENCE(4)`,
   non-failover) and from a transient failover (5xx). The clean mapping is:
   **`once` → `EXIT_BUDGET(7)`** — and to stop the scheduler pointlessly retrying a
   budget stop, agentctl compiles exit 7 to a **`FailJob`** rule in the rendered
   `podFailurePolicy` (agentctl RFC 0010 §7.4); **by the contract default `EXIT_BUDGET`
   is `Count` and IS retried** (it counts toward `backoffLimit`, agentd RFC 0016 exit
   table / brainstorm §3.2 "3,7 → Count"), so non-retry is agentctl's compiled policy,
   not inherent (consistent with §7.2's "default `Count`, operator may `FailJob`") — and
   **`loop`/`reactive` → readiness back-pressure** (the pod flips not-ready
   and stops claiming work, exactly like all-down back-pressure, agentd RFC 0018 §6 —
   it does **not** crash). Without P-cost, a proxy "budget refusal" is indistinguishable
   from a 5xx and would trigger spurious failover/retry, masking the budget stop.

Until P-cost + the store land, the answer to "hard fleet budget?" is Tier 2 plus the
hard *per-run* cap of Tier 1. The choice of whether v1 requires hard enforcement is a
human decision (brainstorm §17 Q8) recorded in §Open.

### 7.5 Per-tenant budgets/quotas

Per-tenant accounting keys off the `tenant` label on the rollups (agentctl RFC 0010
§5.3/§9.2) and, under Tier 3, off the proxy authz identity (§5.4 — the same chokepoint
that authorizes a dial counts its tokens). A first-class `Budget`/`Quota` CRD is **out
of v1** (a future object on the security/tenancy track, agentctl RFC 0015); v1 expresses
per-tenant soft budgets as operator-owned policy against the rollups.

---

## 8. Composition with vsock-everything — the egress the loop always blocks on

The distinctive vision — "vsock-everything, no cluster network" — is honest only with
one correction this plane embodies (agentctl RFC 0002 §10 correction 1, brainstorm
§1.1/§10): **"no network" never meant "no egress."** The agent's reasoning loop blocks
on a model call; that call is a live egress leg even when it travels over vsock to an
off-pod proxy, and in the lethal-trifecta threat model it is *the dangerous one*. The
isolation win is **not** "nothing to attack" — it is that the model channel is the
**single, concentrated, governable** egress, and this plane is where it is governed.

```
   kata-hybrid hardened tier — the maximal-isolation realization
   ┌──── Agent pod: NO cluster network ────┐        ┌── intel-proxy (node-local Deployment) ──┐
   │ agent ──vsock (provisioned port only)─┼───────▶│  authz (peer→Agent→pool, §5.4)          │
   │  keyless · dialect=pool's             │        │  hold cred · TLS · in-pool LB · meter   │──TLS──▶ provider
   └────────────────────────────────────────┘        │  SSRF / egress allow-list (RFC 0015)    │   (the ONE egress)
        ▲ guest→host vsock egress restricted to       └─────────────────────────────────────────┘
          the model dial + A2A dial only (RFC 0015)
```

The composition, made precise:

- The pod has **no cluster network**; its only outbound capability is a **vsock dial
  to the provisioned proxy port** (and the A2A dial). agentctl RFC 0015 owns the
  **guest→host vsock egress restriction** that limits even that vsock to the
  provisioned ports — so the egress is not just concentrated, it is *enumerated*.
- The proxy (off-pod, its own netns) is the **only** component that touches the
  cluster network / internet for intelligence. It is therefore the natural and only
  correct home for: the provider credential (§5), TLS termination (§4.4), the SSRF /
  metadata-endpoint / allow-list controls (agentd RFC 0012's SSRF posture, applied at
  the proxy because the pod can no longer reach anything to attack), per-tenant authz
  (§5.4), and metering (§7).
- The agent-side distillate firewall (the CaMeL-style reader/actor split, agentd RFC
  0012 §3.3) and the microVM kernel boundary (agentctl RFC 0002) are **orthogonal**
  isolation wins layered on top — the proxy governs *egress*, the kernel boundary
  governs *the host*, the firewall governs *what crosses back*. Market the combination,
  not "no network."

This is why the proxy is the keystone of the honest isolation story, and why it must be
off the agent pod (so the pod truly has no network) **and** off the node-agent (so the
egress is not concentrated in the god-mode host process, §3.1).

### 8.1 Worked strawman — one pool rendered into the data path

```yaml
# ── OPS: the ModelPool (schema owned by agentctl RFC 0004 §4; shown for context) ──
apiVersion: agents.x-k8s.io/v1alpha1
kind: IntelligenceService
metadata: { name: anthropic-pool, namespace: agents }
spec:
  endpoints:
    - name: opus-primary                 # STABLE name → metric label (P7), survives reorder
      models: [claude-opus-4]            # model-aware placement (P7)
      dialect: anthropic                 # the agent uses its shipped anthropic adapter; proxy passes through
      backends:                          # interchangeable replicas the PROXY load-balances across
        - { service: anthropic-gw.models.svc, port: 443, weight: 1 }
        - { service: anthropic-gw-2.models.svc, port: 443, weight: 1 }
      credentialRef: { secretRef: { name: anthropic-key, key: api-key } }   # mounted into the PROXY only
    - name: opus-fallback
      models: [claude-opus-4]
      dialect: anthropic
      backends: [{ service: bedrock-gw.models.svc, port: 443 }]   # bedrock-native → proxy translates to anthropic
      credentialRef: { secretRef: { name: bedrock-key, key: api-key } }
  failover: { breakerThreshold: 3, breakerCooldownSeconds: 5 }   # ACROSS-pool → agentd RFC 0018 (branded env)
  swapPolicy: finish-on-old
  proxy: { topology: node-local, image: registry.example.com/agentctl/intel-proxy@sha256:… }
  fallbackDirect: false                  # hostile-tenant: no in-pod key path (admission enforces)
  placement: { modelAware: true }
```

What the **operator renders** (agentctl RFC 0006), illustrating the runtime this RFC owns:

```jsonc
// (1) Agent pod env — KEYLESS, points at the proxy over the substrate, names for P7
{
  "AGENTD_INTELLIGENCE":       "vsock:<proxy-cid>:8081,vsock:<proxy-cid>:8082",  // pools in failover order
  "AGENTD_INTELLIGENCE_NAMES": "opus-primary,opus-fallback",                     // P7 stable names → metric labels
  "AGENTD_INTELLIGENCE_DIALECT": "anthropic",                                    // the agent's shipped adapter
  "AGENTD_MODEL_SWAP":         "finish-on-old"
  // NOTE: NO AGENTD_INTELLIGENCE_TOKEN* — the credential is NOT in this pod
}

// (2) intel-proxy upstream config (agentctl's component) — holds the cred, LBs within each pool
{
  "listen": "vsock:8081 (opus-primary), vsock:8082 (opus-fallback)",
  "authz":  "peer→Agent→{opus-primary,opus-fallback}",         // §5.4
  "pools": [
    { "name": "opus-primary", "dialect_out": "anthropic",
      "backends": [ {"url":"https://anthropic-gw.models.svc","weight":1},
                    {"url":"https://anthropic-gw-2.models.svc","weight":1} ],
      "credential_file": "/etc/intel/anthropic-key",            // mounted Secret; rotates by file replace (§5.3)
      "breaker": {"consecutiveFailures":3,"cooldownSeconds":5} },
    { "name": "opus-fallback", "dialect_in": "bedrock", "dialect_out": "anthropic",  // long-tail translate (§6.4)
      "backends": [ {"url":"https://bedrock-gw.models.svc"} ],
      "credential_file": "/etc/intel/bedrock-key" }
  ]
}
```

Resulting metric two-tier view (§4.5): `agentd_intel_endpoint_active{endpoint="opus-primary"}=1`
(the pod prefers the primary pool) and `agentctl_intel_backend_up{pool="opus-primary",backend="anthropic-gw"}=1`
(a specific replica within it is live). A credential rotation replaces
`/etc/intel/anthropic-key` and is invisible to the agent (§5.3, §4.3 layer 1).

---

## Non-goals

- **The `IntelligenceService`/ModelPool CRD schema.** Owned by agentctl RFC 0004
  (endpoints, `credentialRef`, `proxy`, `swapPolicy`, `failover`, `fallbackDirect`,
  `placement`). This RFC owns the runtime behind those fields, never their shape.
- **The across-pool failover/breaker/all-down/hot-swap algorithm.** Owned by the
  contract (agentd RFC 0018). agentctl supplies and moves the list; the agent runs the
  machine. agentctl never re-implements it.
- **The intelligence wire, dialects, keyless dial, `usage` shape, secrets front
  door.** The contract (agentd RFC 0006 / RFC 0012 §3.7). The proxy speaks the
  agent-facing half; it does not redefine the wire.
- **The metrics exposition, scrape-proxy, central SD, cost rollups, and run-report
  capture.** agentctl RFC 0010. This RFC *consumes* the rollups and owns cost
  **enforcement** (the levers + the P-cost ask) **and the price table + chosen token
  source** (deferred here by RFC 0010 §9.2 — §7.1, §Open 2).
- **The substrate descriptor, CID/uds discovery, and the guest→host vsock egress
  restriction.** agentctl RFC 0002 / RFC 0015. This RFC consumes the keyless dial and
  the egress-restriction mandate.
- **The node-agent.** The proxy is categorically out of it (agentctl RFC 0008 §6);
  this RFC restates the why, it does not own the node-agent.
- **The tenancy/PKI trust model and the proxy's authz trust roots.** agentctl RFC
  0015. This RFC requires the proxy to be the enforcement point; 0015 owns the model.
- **A `Budget`/`Quota` CRD and chargeback/billing integration.** Out of v1 (§7.5).
- **Provider/model procurement, a model catalog, or fine-tuning.** Not this plane.
- **Streaming intelligence end-to-end.** The contract is non-streaming
  (`stream:false`, agentd RFC 0006); the proxy de-streams a streaming backend. A
  streaming agent-facing wire is a follow-up (agentd RFC 0006 open item), not v1.

## Open questions

1. **Shared vs per-pod proxy listener (and the proxy's own resilience model).**
   `sidecar` (per-pod, one cred mount per pod) vs `node-local` (a shared Deployment
   serving many pods, one cred mount, harder authz). The default leans `node-local`
   for the networkless tier and `sidecar` for dev/single-tenant; confirm whether a
   *shared* node-local proxy serving multiple **tenants** is acceptable (it concentrates
   credentials and needs the §5.4 authz to be airtight) or whether hostile tenancy
   forces a per-tenant proxy Deployment. (Brainstorm §6.3.)
2. **Price-table freshness and ownership.** agentctl owns the price table (agentd RFC
   0016 §4.3); where does it live (a ConfigMap? a CRD? an external feed?), how is it
   versioned, and what is the staleness policy when a provider changes pricing
   mid-window? If cache-tier pricing comes to dominate and the contract has not added
   cache `type`s to `agentd_tokens_total`, does the proxy meter become the billing
   authority (§7.1)? (Brainstorm §6.3.)
3. **Streaming-to-buffered de-stream cap.** When the proxy de-streams a streaming
   backend (§4.4), what is the response buffer cap (the contract caps the body at 4
   MiB, agentd RFC 0006 §3) and what happens on overflow — truncate, error, or fail
   over? Pin this before a streaming-heavy backend is allowed in a pool.
4. **Hard fleet-wide budget enforcement in v1?** (Brainstorm §17 Q8.) Tier 3 (§7.4)
   needs **both** a shared accounting store (a stateful component to run/HA/DR) **and**
   the **P-cost** primitive. Is hard, fleet-wide enforcement a v1 requirement, or is
   the per-run hard cap (Tier 1) + best-effort fleet throttle (Tier 2) acceptable for
   v1? This gates whether the store and P-cost are on the critical path.
5. **Compose with KServe `InferenceService` rather than model backends directly?**
   (Brainstorm §12/§17 Q10, agentctl RFC 0004 OQ2.) A pool's `backends[]` could be a
   KServe `InferenceService` ref, reusing KServe's serving topology, autoscaling, and
   canarying instead of agentctl modelling backends. The tension is real: KServe owns
   *serving*, but the **zero-secret-in-pod / egress-proxy / dialect-translation**
   posture (§4–§6) and the contract's **across-pool failover** have no KServe
   equivalent. Leaning: keep the proxy + `IntelligenceService` shape and allow a
   `backends[].inferenceServiceRef` as an alternative to a raw `service`, so the two
   **compose** (KServe behind a backend) rather than compete. Confirm before GA.
6. **Per-endpoint credential env keying on the direct-dial path.** The `topology: none`
   path maps `credentialRef` onto the contract's index-keyed
   `AGENTD_INTELLIGENCE_TOKEN_<n>`; the contract flags that **name-keyed** credentials
   may be preferable once stable endpoint names (P7) land (agentd RFC 0018 §11,
   agentctl RFC 0004 OQ6). Align the direct-dial keying with whatever P7 freezes so
   there is one mental model across the proxy and direct-dial paths.
7. **Proxy dialect-translation conformance scope.** §6.4 requires a versioned
   conformance suite for any proxy-side dialect translation (tool-calling + `usage`
   round-trip). How broad must it be at v1 — only `bedrock→anthropic`/`gemini→openai`,
   or the full provider matrix? An untested translation is the highest-risk code in the
   plane.

## References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & Contract-as-Schema (P0): the contract-not-agentd
  framing this plane's surface-neutralization follows; the conformance discipline §6.4
  invokes; the proxy as a Rust **data-path** component — a deliberate addition beyond
  RFC 0001's five **control-plane** components (operator, node-agent, A2A gateway, CLI,
  KEDA scaler): it is a TLS-terminating, dialect-translating egress component on the
  blocking LLM call path, with its own perf/HA posture (§3.3), **not** a control-plane
  component (the aggregated APIServer, not this proxy, is the sanctioned Go escape hatch).
- **agentctl RFC 0002** — substrate & transport abstraction: the keyless model-dial as
  the **one egress exception** (§1), the §10 correction-1 "no network ≠ no egress" this
  plane embodies (§8), the per-tier reach the agent's `AGENTD_INTELLIGENCE` dials over.
- **agentctl RFC 0004** — AgentClass / IntelligenceService / MCPServerSet: **owns the
  schema** this plane executes — endpoints/`credentialRef`/`proxy`/`swapPolicy`/
  `failover`/`fallbackDirect`/`placement`, the zero-secret resolution table, the
  binding/merge rules (§5, §6.1).
- **agentctl RFC 0006** — operator reconcile: the renderer that compiles a resolved
  pool into proxy config + the agent's keyless env, the stable-named reloadable
  ConfigMap behind the across-pool hot-swap (§4.3 layer 2).
- **agentctl RFC 0007** — admission validation ladder: rejects `topology: none` /
  `fallbackDirect: true` on a hostile class and cross-namespace pool/secret refs
  (§3.3, §5.2/§5.3).
- **agentctl RFC 0008** — node-agent architecture: the **proxy-out-of-node-agent**
  invariant + the per-node-inference-SPOF argument this RFC restates (§3.1, agentctl
  RFC 0008 §6).
- **agentctl RFC 0010** — observability & telemetry bridge: the token rollups this
  plane meters against (the **price table + chosen token source are deferred here** by
  RFC 0010 §9.2 — owned in §7.1, not by 0010), the `agentctl_*` namespace the proxy's
  per-backend series live in (§4.5, §7.1), the trace-correlation model the proxy hop
  joins (§4.4), the exit-code → `podFailurePolicy` reading of `EXIT_BUDGET` (§7.2).
- **agentctl RFC 0011** — scaling plane: the KEDA `min`/`max` + `lame-duck` levers the
  best-effort cost throttle (§7.3) drives.
- **agentctl RFC 0015** — security & multi-tenancy: the hostile-tenancy mandate behind
  zero-secret-in-pod, the proxy authz trust roots (§5.4), the guest→host vsock egress
  restriction (§8), and the SSRF/egress controls the proxy enforces.

**Contract spec (the reference implementation, agentd RFCs)**

- **agentd RFC 0006** — intelligence transport & wire: the one-endpoint dial, the three
  transports, `parse_intelligence_uri`, the two in-binary dialects + native
  tool-calling + `usage`, the keyless local-gateway path, and the `resolve()` secrets
  front door — the agent-facing wire the proxy speaks (§4.4, §5, §6.4).
- **agentd RFC 0018** — intelligence transport resilience: the ordered `--intelligence`
  list, sticky-primary across-pool failover, per-endpoint breaker, all-down
  back-pressure (§3.3), quiesce→switch→resume hot-swap + `--model-swap` (§4.3), model
  discovery (§4.6), the frozen `agentd_intel_*` semantics + index-vs-name label open
  item (P7, §4.5/§6.2), `agentd://intelligence`, and "agentd never load-balances; where
  the model is, is agentctl's" (§4.1).
- **agentd RFC 0016** — telemetry & lifecycle contract: the frozen
  `agentd_tokens_total{model,type}` (the billing authority, §7.1), the never-estimate
  honesty, the refusal/limit counters, "tokens not currency," and the `EXIT_BUDGET(7)`
  / `EXIT_INTELLIGENCE(4)` exit codes (§7.2/§7.4).
- **agentd RFC 0012** — security posture: the `Secret`-has-no-`Serialize` invariant the
  zero-secret rule rides (§5.1), the SSRF posture the proxy enforces (§8), the
  transport-is-the-boundary trust model for the keyless dial, and the reader/actor
  distillate firewall orthogonal to the proxy's egress governance (§8).
- **agentd RFC 0017** — declarative config & hot reload: the reload trigger the
  across-pool swap (§4.3 layer 2) signals, with `intelligence`/`model` on the
  reloadable allowlist; the file-source secret-ref rotation discipline (§5.3).
- **agentd RFC 0007** — agentic loop: the loop **blocks on the model call** — the basis
  of the per-node-inference-SPOF argument (§3.1) and the per-run budget refusal (§7.2).

**Contract asks raised or cited by this RFC** (agentctl brainstorm §14): **P7**
(per-endpoint model arrays in the manifest + stable operator-assigned endpoint **names**
so metric labels survive list reorder — §4.5/§6.2/§6.3), **P-dialects** (NEW — the
agent advertises its supported dialect set in the manifest, e.g.
`intelligence_summary.dialects[]`, so the proxy's pass-through-vs-translate decision is
contract-driven instead of assuming the reference impl's two adapters — §6.4), and
**P-cost** (a clean budget-exhausted signal mapping to `EXIT_BUDGET(7)` on `once` /
readiness back-pressure on reactive, distinct from auth-fatal/failover — §7.4). The per-endpoint credential env
keying reconcile (index vs stable name) is the agentd RFC 0018 §11 open item (agentctl
RFC 0004 OQ6, §Open 6). The agentd-branded contract surfaces this plane resolves into
(`AGENTD_INTELLIGENCE`, `AGENTD_INTELLIGENCE_TOKEN[_n]`/`_FILE`, `AGENTD_INTELLIGENCE_NAMES`,
the `AGENTD_INTEL_*` failover/breaker family, `AGENTD_MODEL_SWAP`/`--model-swap`,
`--intelligence-names`, `agentd_intel_*`, `agentd_tokens_total`, `agentd://intelligence`)
are flagged for the **P0 contract-extraction** open question (agentctl RFC 0001 §9).

*Where this RFC and a contract spec disagree on the wire, the contract wins and this RFC
is corrected; where this RFC needs a primitive the contract does not expose (P7,
P-cost), it is a contract ask — never a leak of cluster logic into the agent.*
