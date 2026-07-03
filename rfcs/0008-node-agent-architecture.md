# agentctl RFC 0008: node-agent architecture (two tiers)

> ⚠️ **Superseded in part by [RFC 0021](0021-contract-2.0-network-substrate-pivot.md) (contract 2.0 — the network is the substrate).** **Retired in full.** The node-agent crate, DaemonSet, its Certificate, and its RBAC are deleted; every function it performed is re-homed to a network-native path (RFC 0021 §10). This RFC is retained only as the historical v1 design.

**Status:** Proposed (agentctl foundational track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the on-node keystone every other plane reaches the data plane *through*: it implements the substrate descriptor + discovery + attestation (agentctl RFC 0002), serves the operator the live capability/status snapshot it reconciles against (agentctl RFC 0006), and is the host-side anchor of the management, telemetry, and A2A paths

> **P0 — the node-agent bridges to a *conformant agent's contract surfaces*, never to a
> specific binary.** Everything the node-agent does — hold a management connection, read
> the capabilities manifest / inventory / status, tail the events ring, scrape metrics,
> relay A2A — it does over the **published control contract** (the self-MCP management
> profile, the `agent://` resource scheme, the frozen metrics surface, A2A-over-the-
> substrate). It learns *what* a local agent can do from that agent's `surfaces{}` block
> and **drives only what is advertised** (agentd RFC 0014 §6.2/§8). Where this RFC cites a
> concrete surface it names the **reference implementation** (agentd RFCs 0014–0020) as
> *where the contract is presently written down*, not as a dependency. The agent-branded
> spellings it touches (`agent://…`, `AGENT_*`, the `agent_` metric prefix) are
> contract-normative-but-branded and flagged for neutralization (the P0 contract-extraction
> open question, agentctl RFC 0001 §9 / RFC 0002 open question (a)).

> **One node-agent is a *process set*, not a process.** The on-node keystone is split into
> **two tiers with different reliability classes** (brainstorm D3): a **control + telemetry**
> tier whose crash is a control gap with *zero data-plane impact*, and an **A2A data-path**
> tier that owns a live, must-not-miss task stream and therefore needs its own availability
> envelope. Folding them — plus an intelligence proxy — into one DaemonSet (as `ideas.md
> §3.3` does) couples four incompatible failure domains, dictates one rollout cadence at the
> blast radius of the most critical role, and concentrates every node-local secret and
> god-mode-over-every-local-pod into the one process that also parses untrusted external A2A
> input. This RFC makes the keystone-safety claim **true by construction** for the tier it is
> claimed of, and gives the dangerous tier the isolation it needs.

---

## 1. Problem / Context

agentctl RFC 0002 established the one abstraction every plane reaches an agent through — the
**endpoint descriptor**, a *discovered, per-agent, attested socket dial string* — and was
explicit that "discovery, tier selection, and attestation are confined to the node-agent;
the operator, gateway, telemetry, and CLI consume descriptors and are substrate-blind"
(RFC 0002 §2 principle 1). agentctl RFC 0006 then built its entire LIVE capability path on a
node-agent-published **observed snapshot** ("the node-agent is the only socket-adjacent
component … the operator cannot speak the unix/vsock management profile to a pod", RFC 0006
§1/§4.2). Both RFCs deferred the *implementation* of discovery, attestation, the long-lived
connection, and the snapshot transport to **this RFC**.

This is that RFC. The node-agent is the **host-side keystone**: one process set per node,
**socket-adjacent to every local agent**, that turns "the operator/CLI/gateway/telemetry
want to reach agent X" into "open agent X's attested socket and speak the contract." It is
the single component that:

- **is on the agent's node** — because the substrate transports are point-to-point
  guest↔host (vsock) or filesystem-local (unix hostPath), so reaching a local agent's socket
  is physically a node-local act (agentd RFC 0015 §3.3, agentctl RFC 0002 §4);
- **holds privilege the rest of agentctl deliberately lacks** — CRI-socket access for
  discovery (node-root in the threat model, RFC 0002 §6), a hostPath mount onto the
  per-pod socket tree (RFC 0002 §6.1), and — for the A2A path — node-local secrets and a
  store credential;
- **is the management peer** — over the discovered socket it connects as
  `PeerOrigin::Management` (agentd RFC 0015 §3.3/§3.4), which is precisely the reachability
  that *grants the operator profile* (the operator tools the agent advertises in
  `surfaces.operator_tools` — `drain`/`lame-duck`/`cancel` in the reference impl today;
  `pause`/`resume` are contract-specified but **not yet implemented**, contract ask
  **P-pause** — plus the `agent://capabilities`/`inventory`/`status`/`events` resources).
  Reachability **is** authority (agentd RFC 0015 §7): the node-agent is therefore the
  highest-value trust principal in the data path.

Three forces shape every decision here, and they are in tension:

1. **Node-locality is forced; one-process-ness is not.** vsock/unix pins the host side of
   every agent connection to the agent's node. But the agent's listener is **blocking,
   thread-per-connection** (agentd RFC 0015 §3.2: "one reader thread per accepted
   connection"), so *independent host processes can each open their own connection* to each
   local agent. Node-locality is a placement constraint, not a single-binary constraint. The
   monolith conflates the two.

2. **The keystone-safety claim is only conditionally true.** The load-bearing safety claim —
   "the node-agent is *not* in the data path; a crash = zero data-plane impact" — is **true
   for management + telemetry** (agentd RFC 0015 §8: a dropped management connection is not a
   liveness signal; reconnect is a clean re-read) and **false for A2A** (the relay owns the
   only live ingress/egress for node-local agent-to-agent work, and the agent delivers the
   final distillate *exactly once* — RFC 0020 §5). One claim cannot cover both.

3. **Concentration is a security liability, not a convenience.** A monolith holds every
   node-local secret and god-mode (in-band-authless drain/cancel/steer over every namespace's
   local pods) *and* terminates untrusted external A2A frames in the same address space. One
   panic parsing one malformed frame takes down management + telemetry for the whole node;
   one memory-safety bug in the untrusted-input parser is adjacent to every secret on the
   node.

The resolution, stated once: **two tiers split by reliability class over one shared
node-locality constraint — a bounce-safe control+telemetry tier and an HA A2A data-path tier
— with the intelligence proxy kept out of both, discovery (not allocation) as the privileged
keystone, and a small mTLS API whose access *policy* is agentctl RFC 0009's and whose *shape*
and internal authz *chokepoint* are this RFC's.**

This RFC owns: the two-tier decomposition and what runs where; the bounce-safe invariant and
its honest scope; the A2A relay's node-locality and lifecycle ownership (not the
gateway/store, which are RFC 0013); why the intelligence proxy is excluded; the discovery /
connection-manager / attestation implementation of the RFC 0002 abstractions; the node-agent
API surface and its per-target-namespace authz hook; and the per-tier failure-mode, blast-
radius, and upgrade story. It does **not** own: the descriptor schema, the substrate tiers,
or the attestation *threat model* (RFC 0002); the operator reconcile loop or the snapshot
*consumer* (RFC 0006); the management access *policy*, per-verb RBAC, end-user identity, or
the aggregated APIServer (RFC 0009); the telemetry *schemas*, the scrape relabeling, or the
run-outcome store (RFC 0010); the A2A HTTP/SSE/auth/webhook machinery, the durable task-store
schema, or A2A version negotiation (RFC 0013); the intelligence proxy / ModelPool (RFC 0012).

---

## 2. Decision — two tiers, not one monolith and not four micro-DaemonSets

The node-agent is **two tiers**, deployed as **three workloads**: a Tier A DaemonSet, a Tier
B node-pinned relay DaemonSet, and a Tier B replicated stateless gateway Deployment. The
split is **by reliability class**, over the one constraint all of them share — node-locality
for the socket-adjacent parts.

### 2.1 The three arguments for the split (and against both extremes)

**Argument 1 — the agent's per-connection listener makes the split free.** The temptation to
fold everything into one process is "vsock is point-to-point, so it must be one on-node
thing." That conflates *node-locality* with *single-process*. The agent serves a **blocking,
thread-per-connection** listener (agentd RFC 0015 §3.2); each accepted connection gets its
own reader thread and is independent of every other. So **Tier A and Tier B can each open
their own connection to each local agent** at no cost to the agent — `serve_peer(…,
PeerOrigin::Management, …)` runs once per connection regardless of how many host processes
connect. Node-locality says *be on the node*; it does not say *be one process*. (The node-
agent holds **one** long-lived management connection per local agent per tier; fan-in stays
trivial — agentd RFC 0015 §3.2.) The single fact that demolishes the monolith is that the
data plane does not care how many host processes connect to it.

**Argument 2 — incompatible reliability classes cannot share a rollout cadence or a blast
radius.** The four roles a monolith would fuse have *opposite* requirements:

| Role | Failure on crash | Rollout cadence | Privilege | Faces untrusted input |
|---|---|---|---|---|
| Management bridge | **control gap only** (data plane untouched, §3) | roll freely, per node | CRI-root + socket | no (peer is the operator/apiserver) |
| Telemetry collector | observability gap only (§3) | roll freely, per node | socket | no |
| A2A relay | **lost live tasks** unless drained (§4) | careful, drain-aware | socket + store cred | **yes** (frames originate off-cluster) |
| A2A HTTP gateway (PEP) | dropped A2A ingress | HA, surge | store-read + TLS | **yes** (TLS-terminating, public) |

A single DaemonSet's rollout cadence is dictated by **the union of all roles' churn at the
blast radius of the most critical role**, and one OOM — or one panic parsing one malformed
external A2A frame — takes down management *and* telemetry for the whole node. The split lets
each role roll on its own clock at its own blast radius.

**Argument 3 — concentration of secrets and authority is a liability.** A monolith holds, in
one address space: every node-local secret the A2A path needs (webhook tokens, the durable-
store credential), **god-mode over every local pod** (drain / cancel / steer with no in-band
auth — reachability *is* authority, agentd RFC 0015 §7), the descriptor table (which *is* the
cross-tenant access-control table, RFC 0002 §6.2), *and* the parser for **untrusted external
A2A input**. Splitting the untrusted-input-facing A2A PEP (Tier B gateway) from the
privileged bridge (Tier A) gives the untrusted parser its own, *less*-privileged failure
domain, and keeps the all-tenants management keystone away from off-cluster bytes. Privilege
then **descends** across the tiers (§2.3) instead of pooling in one principal.

**Why not four micro-DaemonSets** (management / telemetry / relay / gateway each its own):
it maximizes deployment sprawl, **multiplies the discovery + attestation burden** (each
would need its own CRI-root access and its own per-connection attestation), and re-pools the
exact CRI-root privilege the split is meant to concentrate-then-minimize. Discovery is the
expensive, privileged, shared act; it should be owned **once** (Tier A) and its product (the
attested descriptor, RFC 0002 §3) consumed by the others. Two tiers is the seam that makes
the safety invariant true *and* keeps CRI-root in one place.

### 2.2 The principles (final for the node-agent surface)

1. **Two tiers, one node-locality constraint.** Tier A = control + telemetry (§3); Tier B =
   the A2A data path (§4). The split is by reliability class; the shared constraint is
   on-node for the socket-adjacent parts. Each tier opens its **own** connection per local
   agent (Argument 1); **discovery + attestation is owned once by Tier A** and its attested
   descriptors are consumed by the others (§5).

2. **Tier A is bounce-safe; the invariant is scoped to Tier A.** A Tier A crash/restart is a
   **control + observability gap on one node with zero data-plane impact** — the agent keeps
   running (liveness = the supervisor heartbeat, agentd RFC 0010 §3.7), and reconnect is a
   **clean re-read** (agentd RFC 0015 §8: no per-connection durable state). This is written
   down (§3.3) as scoped to *this* tier and explicitly **not** claimed of Tier B.

3. **Tier B owns the live A2A task stream end-to-end and writes to the shared store.** The
   distillate is delivered **once** (agentd RFC 0020 §5 / RFC 0009 §7); the relay must own
   the stream for the **full lifecycle independent of any HTTP client** or results are lost
   (§4.2). The relay writes durable status to the shared store; a **separate, replicated,
   stateless HTTP gateway** serves the public surface from that store. **This RFC owns
   node-locality + the relay; the gateway/store schema, auth, webhooks, and A2A versioning
   are agentctl RFC 0013.**

4. **The intelligence proxy is OUT of both tiers.** An embedded proxy would make the agentic
   loop's LLM call a per-node SPOF — a proxy crash stalls reasoning on *every* local pod
   (§4.3 of agentd RFC 0007: the loop blocks on the model call). It lives in the
   `IntelligenceService` / ModelPool plane (agentctl RFC 0004 / RFC 0012), never here (§6).

5. **Discovery, not allocation.** The node-agent **discovers** each agent's host-reachable
   socket from an apiserver watch scoped to `spec.nodeName == self` + the CRI; it does
   **not** allocate vsock CIDs in v1 (RFC 0002 §5/§6). CRI-socket access is **node-root** in
   the threat model.

6. **A small mTLS API; policy is RFC 0009, surface + chokepoint are here.** The node-agent
   exposes an authenticated (mTLS) API whose verbs map onto the contract's management
   profile — each verb rendered from `surfaces.operator_tools`, not a hardcoded set (§7). *Who* may call which verb, end-user identity, and per-verb RBAC are
   agentctl RFC 0009 (D5). This RFC owns the **API shape** and the **per-target-namespace
   internal authz hook** the policy plugs into — the structural guarantee that one tenant's
   reach does not become an all-tenants master key.

7. **Privilege descends; it never pools.** CRI-root in Tier A; hostPath-socket + store-cred +
   one mTLS-locked inbound control listener (the gateway→relay live-op hop, §4.1) in the Tier B
   relay; store-read + TLS only in the Tier B gateway (§2.3). No process holds more authority
   than its role requires; the relay terminates only an *internal*, post-PEP control connection,
   never off-cluster bytes (the gateway is the PEP).

These seven are final for the node-agent surface. Each defers to its owning sibling RFC where
it touches another plane (noted inline).

### 2.3 Topology — the two tiers and the data plane

```
                          CLUSTER (not node-pinned)
   operator (RFC 0006)        aggregated APIServer / pods/proxy stopgap (RFC 0009)
        │ mTLS (autonomous)        │ (human path: per-verb RBAC + end-user identity)
        │                         │                     A2A cluster Service (RFC 0013)
        ▼                         ▼                              │  HTTPS / SSE
 ┌───────────────────────────────────────────┐         ┌────────┴───────────────────┐
 │  TIER A  node-agent  (DaemonSet, 1/node)   │         │ TIER B  A2A HTTP GATEWAY   │
 │  PRIVILEGE: CRI-root + hostPath socket     │         │ (Deployment, REPLICATED,   │
 │  ┌─────────────────────────────────────┐  │         │  STATELESS, NOT node-pinned)│
 │  │ discovery  : watch nodeName==self    │  │         │  PEP: TLS, OAuth/OIDC/mTLS, │
 │  │   + CRI → attested EndpointDescriptor│──┼───┐     │  SSE, webhooks, ListTasks   │
 │  │   (RFC 0002 §3/§6/§7) — SHARED       │  │   │     │  reads the shared store     │
 │  ├─────────────────────────────────────┤  │   │     │  (RFC 0013 owns this box)   │
 │  │ conn mgr   : 1 mgmt conn/local agent │  │   │     └────────┬───────────────────┘
 │  │   PeerOrigin::Management (RFC 0015)  │  │   │              │ durable task record,
 │  │   caches manifest/inventory/status   │  │   │              │ status history, webhook
 │  │ mgmt API   : §7 (mTLS; NetPol→reach) │  │   │              ▼ registry, ListTasks idx
 │  │ telemetry  : metrics scrape-proxy +  │  │   │     ┌────────────────────────────┐
 │  │   events ring tail + run-outcome cap │  │   │     │  SHARED DURABLE STORE       │
 │  └─────────────────────────────────────┘  │   │     │  (RFC 0013 owns schema/HA)  │
 └──────────────────┬────────────────────────┘   │     └────────────────────────────┘
       descriptor   │ mgmt conn (per-connection           ▲ write status / read terminal
       (consumed)   │  attestation, RFC 0002 §7)          │
 ┌──────────────────┼─────────────────────────────┐      │
 │  TIER B  A2A RELAY (DaemonSet, NODE-PINNED)     │      │
 │  PRIVILEGE: hostPath socket + store cred +      │──────┘ owns the LIVE task stream
 │   mTLS-locked inbound control listener (←gw,    │        end-to-end (distillate is
 │   live-op hop, post-PEP — §4.1; routing=RFC0013)│        delivered ONCE — §4.2)
 │  consumes Tier A descriptors; opens its OWN A2A │
 │  conn/local agent; drains the distillate (once) │
 └──────────────────┬─────────────────────────────┘
        ▲ live-op proxy (message/stream, tasks/resubscribe) from the wrong gateway (§4.1)
   mgmt conn │      │ A2A conn (vsock/unix; point-to-point ⇒ relay MUST be on this node)
 ┌───────────▼──────▼───────────────────────────────────────────────────────────────┐
 │  DATA PLANE on this node — conformant agent pods (ANY contract-conformant agent)   │
 │  agent pod: serves self-MCP mgmt (RFC 0015 §3) + A2A (RFC 0020) + metrics/events   │
 │             over a discovered socket; runs with NO cluster network (vsock tiers)   │
 └───────────────────────────────────────────────────────────────────────────────────┘
```

Reading the diagram: **Tier A** is the privileged keystone (CRI-root discovery, the
management bridge, the telemetry collector) and the only producer of attested descriptors.
**Tier B relay** is node-pinned but *less* privileged — it consumes Tier A's descriptors,
opens its own A2A connection per local agent (Argument 1), drains the live task stream, and
writes to the shared store. **Tier B gateway** is not node-pinned at all — it is a replicated
stateless Deployment fronted by the cluster A2A Service, reading the store. Everything inside
the store/gateway boxes is **agentctl RFC 0013's**; this RFC owns the relay's node-locality
and lifecycle ownership and the Tier A boxes.

---

## 3. Tier A — control + telemetry DaemonSet

Tier A is the host-side bridge for **management** and **telemetry**: the part of the
node-agent whose crash is a control gap and nothing more. It is a privileged DaemonSet
(`hostNetwork: false`, CRI access, the hostPath socket mount), one replica per node.

```
Tier A node-agent Pod (DaemonSet, privileged for discovery)
  ├── discovery     : apiserver watch (spec.nodeName == self) + CRI sandbox introspection
  │                    → attested EndpointDescriptor per local agent (RFC 0002 §3/§6/§7)
  │                    → published for Tier B + the operator to consume (SHARED, §5)
  ├── conn manager  : 1 long-lived MCP management conn per local agent pod, over the
  │                    discovered unix socket (all tiers dial unix — RFC 0002 §3);
  │                    PeerOrigin::Management ⇒ the operator profile (RFC 0015 §3.4);
  │                    caches manifest / inventory / status; edge-re-reads on `updated`
  ├── mgmt API      : the §7 mTLS API (operator + aggregated-APIServer callers);
  │                    mTLS-authenticated; a NetworkPolicy restricts reachability to
  │                    the operator/apiserver pods (IP-layer, not identity — RFC 0002 §10)
  └── telemetry     : /metrics scrape-proxy (RFC 0010) + agent://events ring tail
                       + run-outcome capture (subscribe at pod-up, read on terminal)
```

### 3.1 The management bridge

Over the discovered, attested socket (RFC 0002 §3) the connection manager holds **one**
long-lived management connection per local agent and connects as `PeerOrigin::Management`
(agentd RFC 0015 §3.3) — the structural gate that lists the operator tools and operator
resources to this peer and to no spawned subagent (agentd RFC 0015 §3.4). The bridge:

- **caches the live read model.** It reads `agent://capabilities` (re-read on hot-reload /
  model-swap via the `updated` notification), `agent://inventory` (the live subagent tree),
  and `agent://status` (identity + lifecycle flags) — agentd RFC 0015 §5.2–5.4 — and
  publishes the **observed snapshot** correlated to the pod by `identity.uid` that agentctl
  RFC 0006 §4.2 consumes for the LIVE capability path. **The node-agent never writes
  `Agent.status`** — the operator is the single writer (RFC 0006 §2.6); the node-agent owns
  the *snapshot*, the operator owns the *projection*.
- **invokes the operator tools on request** through the §7 API: each verb maps onto a
  contract operator tool (agentd RFC 0015 §4), and `attach` is `subagent.send` into a warm
  session (agentd RFC 0015 §4.5) — *not* a new tool. The reference impl's
  `surfaces.operator_tools` is `drain`/`lame-duck`/`cancel` today; `pause`/`resume` are
  contract-specified (agentd RFC 0015 §4.3) but **not yet implemented** (contract ask
  **P-pause**, brainstorm §0.6) — so they are not a guaranteed 1:1 operator-tool set. The
  bridge renders its verb set from `surfaces.operator_tools`, not a hardcoded list, and
  drives **only** the verbs the agent advertises (graceful degradation, agentd RFC 0015
  §5.2/§8); a verb the agent omits (including `pause`/`resume` on a binary that lacks them)
  is surfaced as unsupported, never invoked — capability-absence, not error.
- **does not conflate the lifecycle verbs.** `drain` ≡ SIGTERM ≡ a clean exit `0` (agent
  RFC 0015 §4.1) — it *exits* the agent; `lame-duck` is the stay-resident NotReady primitive
  (§4.2). The node-agent exposes both and never substitutes one for the other; the per-kind
  drain *choreography* (stop the recreating controller before draining) is the operator's
  finalizer logic (RFC 0006 §7.2), not the bridge's.

### 3.2 The telemetry collector

Tier A is the **single networked bridge** for telemetry on a networkless pod (the LLM/model
egress leg aside, RFC 0002 §10). It owns three responsibilities; the *schemas and relabeling*
are agentctl RFC 0010's, the *on-node collection* is this RFC's:

- **metrics scrape-proxy.** Re-export each local agent's metrics as byte-identical Prometheus
  text (`GET /proxy/<uid>/metrics` → `agent://metrics` / the `/metrics` surface, agentd RFC
  0016). Whether `agent://metrics` is reachable over the management socket at all is the
  unresolved contract conflict **P4** (RFC 0002 open question (h)); on a networkless pod there
  is no TCP fallback, so the collector is *blocked* on P4 there.
- **events ring tail.** Tail `agent://events` for live `kubectl agent logs -f` / event
  follow, using the ring's cursor (`?after=<seq>`, the `dropped` counter, oldest/newest seq —
  agentd RFC 0016 §7.2). The ring is **lossy by design**; the collector surfaces `dropped > 0`
  rather than pretending completeness. **Bulk** event capture is *not* this path — it is the
  ordinary container-stderr → node log agent → Loki path, which already works on networkless
  pods (CRI captures stderr independent of pod networking); the ring is **live-tail only**
  (agentctl RFC 0010 §6.1).
- **run-outcome capture.** For `once` / Job pods the run report (`agent://run/{run_id}` /
  `--report-file`) **vanishes with the pod**. The collector subscribes at pod-up and reads on
  the terminal transition *while the process is still alive*, then hands the report to the
  durable run-outcome store (agentctl RFC 0010) that backs `kubectl agents results`. This is a
  real Tier A responsibility, not an afterthought, and it needs the **read-before-exit
  guarantee / short post-terminal linger** the contract does not yet make (**P5**, agentd RFC
  0016) — without it, a once-mode result is lost if the collector blinks at the wrong instant.

### 3.3 The bounce-safe invariant (scoped to Tier A)

> **Invariant.** A Tier A crash, restart, or rolling upgrade costs a **control +
> observability gap on one node** and has **zero data-plane impact**. It is claimed of Tier A
> **only** — never of Tier B (§4).

This is true by construction, and each clause has a contract citation:

- **The agent keeps running.** Liveness is the supervisor heartbeat / health-file (agentd RFC
  0010 §3.7), independent of any management connection. A pod whose node-agent bounced is
  still alive; only its control + telemetry *reach* gapped.
- **A dropped management connection is not a liveness signal** (agentd RFC 0015 §8). The
  kubelet probe is the contract's exec-health verb on a networkless pod (the **P1** ask, RFC
  0002 §8) or an HTTP probe on a networked one — never a dial to the node-agent.
- **Reconnect is a clean re-read.** agent holds **no per-connection durable state** (agent
  RFC 0015 §8 / RFC 0011 §7); on reconnect the bridge re-`initialize`s, re-attests (RFC 0002
  §7 — attestation is part of the re-read), and re-subscribes, correlating to the same
  instance by `identity.uid`.
- **Lifecycle still works without the bridge.** When the management `drain` tool is
  unreachable, `drain` is still reachable as **plain SIGTERM** (pod delete) — `drain` ≡
  SIGTERM ≡ exit 0 (agentd RFC 0015 §4.1). So the operator's finalizer drain (RFC 0006 §7.2)
  degrades to the pod's SIGTERM path, not to a stuck object.

**The honest scope.** A Tier A bounce *does* gap: live `kubectl agent` interaction, the
`Agent.status` LIVE projection (the operator sets `Ready=False`/`ManagementUnreachable` and
tolerates it without erroring — RFC 0006 §4.2/§8.1), metrics scraping, event tailing, and
**run-outcome capture during the gap** (the P5 window). None of these is a data-plane outage;
all self-heal on reconnect. The invariant is "no data-plane impact," **not** "no impact."

### 3.4 Blast-radius fallback respects workload semantics

When the management path is unavailable and the operator falls back to a Kubernetes-level
action, the fallback must respect the workload kind (brainstorm §4.2): "scale down / delete a
pod" on a StatefulSet deletes the **highest ordinal** or recreates the pod — it does **not**
selectively drain a middle ordinal. Tier A therefore exposes both `drain` (delete-to-drain,
for non-recreating kinds) and `lame-duck` (the contract primitive: selective, stay-resident
NotReady — *not* a node-level `cordon`, which is a different k8s concept) so the operator
(RFC 0006 §7) can pick the kind-appropriate fallback; the node-agent never substitutes a
destructive delete for a selective lame-duck.

---

## 4. Tier B — the A2A data path

Tier B is the part of the node-agent the bounce-safe invariant **cannot** cover, and it is
structured to own that fact honestly. It is two workloads:

- a **node-pinned relay DaemonSet** — this RFC's concern: node-locality + the live task
  stream + writing durable status to the shared store;
- a **replicated, stateless HTTP gateway Deployment** — agentctl RFC 0013's concern: the
  PEP (TLS, OAuth/OIDC/mTLS, SSE, webhooks, `ListTasks`), reading the shared store.

> **The ownership seam with RFC 0013.** This RFC owns **node-locality and the relay**: *why*
> the live A2A leg is on the agent's node, *that* the relay must own the stream end-to-end,
> and the relay's failure/upgrade envelope. agentctl RFC 0013 owns the **gateway and the
> store**: the durable task-store schema and HA/DR, the webhook registry, A2A version
> negotiation and the wire-string commitment (the **P2** ask, `surfaces.a2a`), `ListTasks`,
> rate-limit/quota state, central Agent-Card signing, and the SSRF/encryption controls on
> webhook delivery. Where this RFC names a store concern it is to justify the relay's
> behaviour, not to define the store.

### 4.1 Why the live A2A leg is node-pinned

A2A is served by the agent over vsock/unix (agentd RFC 0020 §2), which is point-to-point
guest↔host. The **live** task stream — `message/send`, `message/stream`, live `tasks/get`,
`tasks/cancel`, `tasks/resubscribe` — is therefore physically pinned to the node where the
agent pod runs. So the relay is on-node, by the same physics that pins Tier A. The
**durable/registry** methods (`tasks/list`, `tasks/pushNotificationConfig/*`, the extended
card) are *not* node-pinned — they are served by the replicated gateway from the shared store
(agentctl RFC 0013 owns that split). A request landing on the "wrong" gateway answers
terminal tasks from the store and proxies live operations to the owning node's relay
(resolution by `taskId` → owning pod/node via the store) — the routing detail is RFC 0013's;
the relevant fact here is that **the live leg is unavoidably on the agent's node**.

> **The relay's inbound surface (and what it does to the privilege model).** Because the
> replicated, NOT-node-pinned gateway must reach the *specific* owning node's relay to serve a
> live `message/stream`/`tasks/resubscribe`, the relay is **not** "hostPath socket + store-cred
> only" — it also **terminates an internal control connection**: a small, mTLS-locked inbound
> listener that only the gateway dials, post-PEP (an unauthenticated/over-quota/forbidden A2A
> client never reaches it — §4.4). This is a second internal-network surface (with its own
> mTLS identity and a NetworkPolicy restricting reachability to the gateway pods, IP-layer per
> RFC 0002 §10), and it is still *less* privileged than Tier A (no CRI-root). The **routing**
> (gateway→relay resolution, the listener's wire shape, its failure/blast-radius envelope) is
> **deferred to agentctl RFC 0013**; this RFC records only that the relay's privilege model
> includes this inbound control hop, so the §2.1-Argument-3 "keep the keystone away from
> off-cluster bytes" claim stays honest (the relay terminates an *internal*, post-PEP control
> connection, never off-cluster bytes directly).

### 4.2 The relay owns the live task stream for the full lifecycle (or results are lost)

This is the load-bearing correction the monolith hides. The agent delivers the final
distillate as a status notification **exactly once** and serves only **live** tasks from its
ephemeral registry (agentd RFC 0020 §4/§5 / RFC 0009 §7/§8 — the distillate-only invariant).
For `once`-mode agents this is acute: the agent **exits immediately** after delivering. So:

> **The relay MUST drain the vsock task stream for the full lifecycle, independent of any HTTP
> client.** It is not an "ephemeral live bridge" that exists only while an SSE client is
> attached — it is a **durable, must-not-miss consumer**: if it is not draining at the moment
> of completion, the final artifact is gone.

Two contract-level consequences this RFC records and RFC 0013 must build on:

- **It needs the terminal-distillate re-read primitive (P5).** The contract must guarantee a
  short **post-terminal linger** (or a re-read-by-run-handle after the fact) so a relay that
  blinks at completion can still recover the distillate (the agent ask **P5**, agentd RFC
  0016/0020). Until P5, the relay-loss window must be defined as **FAILED + idempotent
  re-drive**, gated behind an explicit per-fleet opt-in that asserts the composition is
  idempotent — re-drive is *not* free (RFC 0011 §6 dedupes side effects by `run_id`; it does
  not make whole-task re-execution safe). The default on owner loss is **FAIL the task + fire
  the final webhook**, not silent re-execution. (The re-drive policy itself is RFC 0013's; the
  *requirement* that the relay never silently drop a distillate is this RFC's.)
- **Orphan reconciliation is lease-based.** `owner_node` / `owner_pod_uid` in the store are
  **caches, not truth**; a lease-expiry sweep transitions `working` tasks whose owning pod is
  gone to `failed`/`lost` (the sweep lives with the store, RFC 0013). The relay's job is to
  keep its lease fresh while it owns a live stream and to surface owner loss promptly.

### 4.3 The relay writes durable status; the gateway is HA and stateless

The relay writes task status + the final artifact reference to the **shared durable store**
(agentctl RFC 0013) and holds **no** durable state of its own beyond the in-flight live
streams it owns — so a relay restart loses only the live streams it was draining (mitigated
by P5 / the FAILED+re-drive contract above), never the durable record. The **gateway** is a
separate, replicated, stateless Deployment: it reads the store, terminates TLS and auth, makes
SSE, and is fronted by the cluster A2A Service. Because it is stateless it scales and rolls
like any Deployment, with its **own PDB / surge / rollout cadence / security review** —
decoupled entirely from the relay's drain-aware cadence and from Tier A's free cadence. This
is what makes the A2A *surface* HA while the untrusted-input parser (the gateway PEP) has its
own, least-privileged failure domain (§2.1 Argument 3).

### 4.4 What the relay passes to the agent (descriptive, never re-verified)

The gateway is the PEP; an unauthenticated / over-quota / forbidden client never reaches the
relay (agentd RFC 0020 §6). The relay passes the authenticated caller / tenant identity to the
agent as **descriptive `_meta`** — the same posture as the downward-API identity (agentd RFC
0015 §6): the agent labels/scopes by it but **never re-verifies** it (the gateway already did,
agentd RFC 0012 §3.8). This needs the descriptive caller/tenant `_meta` convention the
contract does not yet define (the **P-meta** ask). The relay does **not** hold the card-
signing key (central signing, RFC 0013) and scopes its store credential to least privilege —
a per-node relay must not be able to forge cross-org cards or read other nodes' rows.

---

## 5. Discovery, not allocation — the implementation of the RFC 0002 abstractions

The node-agent **implements** the endpoint descriptor, discovery, and attestation that
agentctl RFC 0002 *specified*. This section is the implementation seam; the descriptor schema
(RFC 0002 §3), the substrate tiers (§4), the tenancy×substrate rule (§5), and the attestation
*threat model* (§7) are RFC 0002's and are not restated.

- **Discovery is keyed by pod UID, from an apiserver watch scoped to `spec.nodeName == self`
  plus the CRI.** Tier A watches only its own node's pods (the node-agent never enumerates the
  whole cluster) and resolves `pod UID → sandbox → host-reachable socket` via the **CRI API**
  (`PodSandboxStatus` / container annotations), **never** by parsing runtime-internal files
  (e.g. Kata's `persist.json`), which are version-volatile (RFC 0002 §6). On the stock-unix
  tier the socket is found in the per-pod hostPath subdir (RFC 0002 §6.1); on Kata-hybrid it
  is the per-VM uds the CRI reports (RFC 0002 §6.2). There is **no CID allocation** in v1
  (RFC 0002 §4.4/§5).
- **CRI-socket access is node-root in the threat model.** It is concentrated in **Tier A
  only** (principle 7), behind a dedicated ServiceAccount with audit; it is not pooled into
  Tier B or the gateway (§2.1, against four micro-DaemonSets).
- **Discovery is owned once; connections are per-tier.** Tier A produces the **attested
  EndpointDescriptor** (RFC 0002 §3) and publishes it for the operator and Tier B to consume.
  Each tier then opens its **own** connection per local agent (Argument 1) and performs the
  **per-connection attestation** RFC 0002 §7 mandates (e.g. `SO_PEERCRED` → cgroup → pod-UID
  on stock-unix) against the descriptor's expected `pod_uid` — attestation is a property of
  *each* connection, not a one-time path blessing, so Tier B re-attests on its own A2A
  connection rather than trusting a path Tier A attested on a different stream. A descriptor
  with `attestation.verified == false` is withheld from every management-capable consumer and
  surfaced as `Ready=False` with reason `AttestationFailed` (RFC 0002 §3/§7, RFC 0003 §6.2
  conditions taxonomy). There is no unattested path under hostile tenancy.
- **GC tracks the pod.** When a pod UID leaves the watch + CRI, Tier A closes its
  connections, prunes the per-pod socket subdir (RFC 0002 §6.1), and retracts the descriptor;
  Tier B drops its connection on descriptor retraction.

The residual open item is the **connection-manager scale envelope on dense nodes** (one
management connection + one A2A connection per local agent, per tier) and the precise CRI
fields + minimum privilege for the Kata per-VM-uds mapping (RFC 0002 open question (c)) — §10.

---

## 6. The intelligence proxy stays OUT of the node-agent

An embedded intelligence proxy is the one role that is **categorically wrong** in the
node-agent, and it is excluded from *both* tiers:

- **It would be a per-node inference SPOF.** The agentic loop **blocks on the LLM call**
  (agentd RFC 0007 / RFC 0006) — reasoning cannot proceed until the model responds. An
  in-node-agent proxy crash therefore **stalls reasoning on every local pod on the node**,
  not merely the node-agent's own work. This is the exact opposite of the bounce-safe
  invariant Tier A is built to guarantee (§3.3): management/telemetry tolerate a node-agent
  bounce; *inference does not*. Co-locating them would poison the safe tier with the unsafe
  one's failure mode.
- **It would make RFC 0018 inter-pool failover shared-fate.** The contract's resilience
  machinery (failover / breaker / all-down / hot-swap, agentd RFC 0018) operates *across*
  pool endpoints; terminating multiple pool endpoints on one per-node process collapses that
  into a single shared-fate hop and blinds the frozen `agent_intel_endpoint_*` metrics (they
  would measure pod↔proxy, not pod↔model).
- **The egress leg is the dangerous one.** The model channel is the irreducible egress leg in
  the lethal-trifecta model (RFC 0002 §10 correction 1); concentrating it in the same
  privileged host process that holds god-mode over every local pod is precisely the
  concentration §2.1 Argument 3 forbids.

**Where it lives instead:** the intelligence egress proxy is fronted by the
`IntelligenceService` / ModelPool CRD and runs as a **per-pod TLS-terminating sidecar**
(`unix:/run/intel.sock`) or a **separate node-local Deployment** — agentctl RFC 0004 (the
CRD) and agentctl RFC 0012 (the plane). There MUST always remain a fallback endpoint in the
agent's RFC 0018 endpoint list that does **not** traverse the proxy, so the proxy is never a
hard inference SPOF (agentctl RFC 0012). The node-agent's only relationship to intelligence
is that it does **not** carry it.

---

## 7. The node-agent API surface

The node-agent exposes a small, authenticated (mTLS) API. **This RFC owns the API *shape* and
the internal authz *chokepoint*; agentctl RFC 0009 owns the access *policy*** (who may call
which verb, end-user identity, per-verb RBAC, the aggregated-APIServer human path, the
`pods/proxy` admin stopgap). The two are deliberately separated: the surface must exist and be
shaped correctly before any policy can be expressed against it.

### 7.1 Shape — verbs that map onto the management profile

The API is a small gRPC service (tonic, agentctl RFC 0001 stack) served by **Tier A** on a
host-local listener, mTLS-authenticated, with a NetworkPolicy restricting reachability to
the operator and apiserver pods (§7.4).
Every call carries an `AgentRef = {namespace, name, uid}`; the node-agent resolves `uid →
attested descriptor → connection` (§5) and refuses any verb the agent does not advertise in
`surfaces.operator_tools` (graceful degradation):

```jsonc
// node-agent management API — mTLS; verbs map onto the contract's management profile.
service NodeAgent {
  // LIVE capability path (RFC 0006 §4.2): the observed snapshot the operator projects.
  rpc Snapshot(AgentRef)         returns (ObservedSnapshot);   // agent://capabilities|inventory|status
  // Operator tools (RFC 0015 §4) — verb ∈ surfaces.operator_tools or REFUSED before connect.
  rpc Invoke(InvokeRequest)      returns (InvokeResult);       // drain|lame-duck|cancel (+ pause|resume once P-pause lands)
  rpc Steer(SteerRequest)        returns (SteerResult);        // = subagent.send (attach, RFC 0015 §4.5)
  // Live tails for `kubectl agent logs -f` / `tree -w` (RFC 0010 / RFC 0016 ring cursor).
  rpc Tail(TailRequest)          returns (stream Event);       // agent://events ?after=<seq>
  rpc Watch(AgentRef)            returns (stream Inventory);   // agent://inventory `updated`
  // Telemetry (RFC 0010) — byte-identical Prom text; blocked on P4 for networkless pods.
  rpc ScrapeMetrics(AgentRef)    returns (PromText);           // agent://metrics | /metrics
  rpc Results(ResultsRef)        returns (RunReport);          // captured run-outcome (RFC 0010, P5)
}

message InvokeRequest {
  AgentRef target = 1;
  string   verb   = 2;            // any member of surfaces.operator_tools — "drain"|"lame-duck"|"cancel"
                                  //   today; "pause"|"resume" only once advertised (P-pause)
  bytes    args   = 3;            // the operator tool's inputSchema payload (RFC 0015 §4)
  CallerPrincipal caller = 4;     // DESCRIPTIVE identity, stamped as _meta (P-meta), never re-verified
}
```

The mapping is mechanical and total: `Invoke{verb}` → the operator tool `verb` (agentd RFC
0015 §4); `Steer` → `subagent.send` (§4.5); `Snapshot`/`Watch`/`Tail` → the `agent://`
resource reads/subscriptions (§5); `ScrapeMetrics`/`Results` → the telemetry surfaces (agent
RFC 0016). The node-agent invents **no new lifecycle verb** — it is a faithful, attested
conduit to the contract's primitives, exactly as agentd RFC 0014 §3 demands ("agent exposes
primitives; agentctl owns policy").

### 7.2 The per-target-namespace internal authz hook (the chokepoint, not the policy)

Because the node-agent **multiplexes every namespace's pods on one node**, an unguarded API
would be an *all-tenants master key* for destructive ops (the D5 hazard). So **every** RPC
passes through one chokepoint **before** it touches an agent connection:

```
authorize(caller_principal, target.namespace, verb) -> Allow | Deny     // BEFORE connect/invoke
```

- The **chokepoint is this RFC's** (it exists, it is unconditional, it runs before any
  side-effecting call, and it is keyed by the **target pod's namespace** so reach to one
  namespace never implies reach to another).
- The **policy behind it is agentctl RFC 0009's** — a `SubjectAccessReview`-style check that
  carries the end-user identity from the aggregated APIServer, or the operator's single mTLS
  identity for the autonomous path. The node-agent does **not** re-implement Kubernetes authz;
  it provides the enforcement *point* and delegates the decision.
- The **default with no RFC 0009 policy loaded is deny-all-except-the-operator-SA** (the
  autonomous reconcile path), so a node-agent shipped before RFC 0009 lands is not an open
  master key. The human path is closed until the policy plane exists.

### 7.3 Caller identity is descriptive; audit is in-band

The authenticated caller is passed to the agent as **descriptive `_meta`** (`CallerPrincipal`,
§7.1) — never re-verified by the agent (agentd RFC 0015 §6, RFC 0012 §3.8). The node-agent
additionally emits a **management-action audit record** on every `Invoke`/`Steer`
independent of the lossy events ring — the closed-vocabulary `mgmt.invoked{tool, caller?}`
event the contract does not yet define (the **P-audit** ask). For interactive `Steer`
(attach), the durable audit record per inject is the only complete record of steering, because
the events ring is lossy and the inject event is itself a pending contract primitive
(P-inject); the *interactive multi-viewer* attach UX is scoped by agentctl RFC 0016, not here.

### 7.4 Transport, not policy

Identity is enforced by **mTLS** + the §7.2 chokepoint; a **NetworkPolicy** additionally
restricts *reachability* of the listener to the operator/apiserver pods (IP-layer, not
identity — RFC 0002 §10 correction 2: NetworkPolicy selects by pod/namespace/IP, never by
caller identity; with `hostNetwork: false` the Tier A pod IP would otherwise be reachable by
any pod — brainstorm §4.2). Two client identities reach it (split by caller, D5 / RFC 0009):
the **operator** (a single mTLS client identity, for autonomous/high-volume traffic, on a
distinct listener whose latency is decoupled from the data path) and the **aggregated
APIServer** (the human path that preserves per-verb RBAC and end-user identity). Note the
streaming seam: the human path arrives over Kubernetes' aggregation streaming model
(SPDY/WebSocket connect-subresource upgrades), and this §7 backend is **gRPC bidi**
(`Tail`/`Steer`); bridging the k8s streaming upgrade to the gRPC backend is **not asserted
clean here** — the bridge mechanism is agentctl RFC 0009's (D5/RFC 0009 flagged generic
proxying as the unproven seam). Raw `pods/proxy` is the single-tenant/admin-only stopgap
(coarse, unattributable — RFC 0009). **All of this is access policy; the node-agent owns the
listener and the chokepoint, RFC 0009 owns who gets through and the streaming bridge.**

---

## 8. Failure modes, blast radius & per-tier upgrade

### 8.1 Failure-mode / blast-radius matrix

| Component | Crash impact | Blast radius | Self-heal | Data-plane impact |
|---|---|---|---|---|
| **Tier A** (control + telemetry) | control + observability gap on one node; LIVE status → `ManagementUnreachable`; metrics/event/run-capture gap (P5 window) | **one node's control reach** | reconnect = clean re-read (RFC 0015 §8); drain degrades to SIGTERM (pod delete) | **none** (§3.3 invariant) |
| **Tier B relay** | node-local **live** A2A streams dropped; in-flight distillates at risk (delivered once) | **one node's live A2A** | lease-expiry sweep → `failed/lost`; P5 re-read or FAILED + idempotent re-drive | live-task loss possible — **NOT bounce-safe** (§4.2) |
| **Tier B gateway** | dropped A2A ingress on the failed replica | **fraction of A2A ingress** (replicated) | another replica serves; store is durable | none (stateless; durable record survives) |
| **the agent itself** | the data plane — supervisor + kill ladder + cgroup bounds (agentd RFC 0003) | one pod | kubelet restart on liveness fail | this *is* the data plane |

The matrix is the whole argument for the split in one table: the bounce-safe column is **true
only for Tier A**, and the relay's must-not-miss column is exactly the property the monolith
would have hidden behind Tier A's safety claim.

### 8.2 Per-tier upgrade story

- **Tier A rolls freely, per node.** A DaemonSet `RollingUpdate` with bounded
  `maxUnavailable` and a PDB; each node's control reach gaps for the restart window and
  self-heals on reconnect (§3.3). No drain of the data plane is required — the agents do not
  notice. Tier A's version-skew envelope is against (a) the operator (the §7 API + the RFC
  0006 snapshot shape) and (b) the agent contract (`contract_version` negotiation, agentd RFC
  0014 §6.3) — both additive-within-major.
- **The Tier B relay rolls drain-aware.** Because it owns live streams (§4.2), a relay
  rollout must either drain in-flight live tasks to terminal/persisted state first or accept a
  re-baseline window governed by the P5/FAILED-re-drive contract. It has its **own** PDB and a
  more conservative cadence than Tier A — this is the cadence-decoupling the split exists to
  enable (§2.1 Argument 2). It is node-pinned, so it rolls per node like Tier A but on its own
  clock.
- **The Tier B gateway rolls like any stateless Deployment** — surge + PDB, behind the
  cluster A2A Service; the store is the durable truth, so a replica restart is invisible to
  durable tasks. Its security review (it terminates untrusted TLS) is independent of the
  privileged tiers'.
- **The tiers version independently.** There is no requirement that A and B upgrade together;
  the only shared contract between them is the **attested EndpointDescriptor** (RFC 0002 §3),
  which is additive-by-design. agentctl's own multi-component skew matrix (operator /
  node-agent tiers / gateway / CLI) is the broader release concern of agentctl RFC 0017; this
  RFC fixes only that the three node-agent workloads have **independent** cadences.

---

## 9. Non-goals (these live in other planes, in RFC 0013, or in the agent)

- **The descriptor schema, the substrate tiers, the tenancy×substrate rule, and the
  attestation threat model.** agentctl RFC 0002. This RFC *implements* discovery /
  attestation / the connection manager against those definitions; it does not redefine them.
- **The operator reconcile loop and the snapshot *consumer*.** agentctl RFC 0006. The
  node-agent *produces* the observed snapshot and is the single non-writer of `Agent.status`;
  the operator projects it.
- **The management access *policy*** — per-verb RBAC, end-user identity, the aggregated
  APIServer, the `pods/proxy` stopgap, the authz decision behind the §7.2 hook. agentctl RFC
  0009. This RFC owns the API *surface* and the chokepoint, not the policy.
- **The telemetry *schemas*** — the metrics relabeling/`http_sd` discovery, the run-outcome
  store, the trace-correlation model, exit-code → `podFailurePolicy`. agentctl RFC 0010. This
  RFC owns Tier A as the on-node *collector*, not the schemas.
- **The A2A gateway and the durable task store** — the store schema + HA/DR, the webhook
  registry + SSRF/encryption controls, `ListTasks`, rate-limit/quota state, central Agent-Card
  signing, A2A version negotiation and the wire-string commitment (P2), the `/.well-known`
  endpoint. agentctl RFC 0013. This RFC owns the relay's node-locality + lifecycle ownership
  only.
- **The intelligence proxy / ModelPool.** agentctl RFC 0004 / RFC 0012. Excluded from the
  node-agent by design (§6).
- **vsock CID allocation.** Out of v1 (agentctl RFC 0002 §4.4/§5). The node-agent discovers a
  per-VM uds; it does not allocate CIDs. Multi-pod-per-VM port allocation is recorded as a
  future descriptor extension (RFC 0002 open question (f)).
- **The interactive multi-viewer attach UX, the CLI grammar, and the single-writer lease.**
  agentctl RFC 0016. This RFC exposes `Steer` (= `subagent.send`) and the per-inject audit
  record; the multiplexing/lease policy is the CLI plane's.
- **Any data-plane internals.** The node-agent drives the contract; it MUST NOT branch on one
  binary's flags, file layout, or `build_features` *values* — only on `surfaces{}` and the
  negotiated `contract_version` (agentd RFC 0014 §6.2, P0).

---

## 10. Open questions

(a) **Live-status snapshot transport — the deferred-from-RFC-0006-§12 decision.** Does the
node-agent publish the observed snapshot (§3.1) as a **node-agent-owned watchable object**
(an `AgentInstance` / EndpointSlice the operator `.watches()`, giving a clean typed reconcile
edge and resolving the "do not `Owns(Pod)`" problem, RFC 0006 §8.2) — or does the operator
**query the §7 `Snapshot` RPC** on a label-driven pod edge (fewer objects, but a runtime
dependency on the node-agent for the edge)? RFC 0006 §12 leans the watchable object; this RFC
holds the decision because the *producer* is the node-agent. Settling it fixes whether the
node-agent owns a CRD/object lifecycle. **Recommendation: the watchable object**, scoped so a
Tier A bounce retracts it cleanly (so a missing object reads as `ManagementUnreachable`, not
stale-Ready).

(b) **Do Tier A and Tier B share or each open their own connection?** §2.1 Argument 1 +
principle 1 decide **each opens its own** (thread-per-connection makes it free), with
discovery/attestation owned once by Tier A. The residual is the **connection-manager scale
envelope on dense nodes** — one management + one A2A connection per local agent, per tier —
and whether a very dense node warrants a shared multiplexed connection with per-tier logical
channels. Out of v1 unless density forces it.

(c) **Kata per-VM-uds discovery API + minimum privilege (shared with RFC 0002 open question
(c)).** Exactly which CRI fields carry the hybrid-vsock uds path across Firecracker and
Cloud-Hypervisor, and the minimum host privilege Tier A needs to open it — without parsing
runtime-internal files. This bounds how much of the "node-root" privilege is truly required.

(d) **The P5 contract shape the relay depends on.** Read-before-exit guarantee vs a short
post-terminal linger vs re-read-by-run-handle (agentd RFC 0016/0020). The relay's
must-not-miss property (§4.2) and Tier A's run-outcome capture (§3.2) **both** ride P5; until
it lands, both degrade to a defined lost-window contract (FAILED + idempotent re-drive for
A2A; best-effort capture for run reports). Which P5 variant the contract adopts changes the
relay's drain-on-upgrade story (§8.2).

(e) **Does the relay re-attest independently, or trust Tier A's descriptor attestation?**
§5 decides **per-connection attestation** (the relay re-attests its own A2A stream, defense in
depth). Confirm this holds for the Kata `kata-sandbox-uds` method (where attestation is
structural, RFC 0002 §7) without a redundant cost, and define the failure mode if Tier A's
descriptor and the relay's `SO_PEERCRED` check disagree (hard-fail, withhold the descriptor).

(f) **Tier A networked-pod metrics vs networkless (P4).** On a networked stock-unix pod the
metrics `dial` is TCP (`:9090`); on a networkless pod it must traverse the management socket,
which the contract does not yet guarantee (P4, RFC 0002 open question (h)). Does Tier A's
collector require P4 before the HARDENED tier ships, or scope networkless metrics to a later
phase? (Leaning: P4 is a hard dependency of the HARDENED tier's observability.)

---

## 11. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — Stack & repo decision record: Rust for all five components, the
  `kube-rs` runtime, `tonic` for the §7 gRPC API, the generated `agent-contract-client` the
  bridge deserializes manifests/inventory/status through, the P0 Contract-as-Schema anti-drift.
- **agentctl RFC 0002** — Substrate & transport abstraction: the endpoint descriptor (§3),
  discovery (§6), pod→socket attestation (§7), the three tiers (§4) and the
  tenancy×substrate rule (§5), the exec-health probe ask (P1), the `agent://metrics`-over-
  socket conflict (P4 / open question (h)) — the abstractions this RFC implements.
- **agentctl RFC 0003** — Agent & AgentFleet CRD schema + status contract: the `Ready`
  (reasons `ManagementUnreachable`/`AttestationFailed`)/`Degraded` conditions the
  node-agent's snapshot feeds, the `agents.x-k8s.io/managed` label, the curated status
  projection.
- **agentctl RFC 0004** — AgentClass / IntelligenceService / MCPServerSet: the
  `IntelligenceService`/ModelPool the egress proxy is kept *out* of the node-agent for (§6).
- **agentctl RFC 0006** — Operator reconcile & manifest-driven capability model: the LIVE
  capability path that consumes the node-agent's observed snapshot (§4.2), the single-writer
  `.status` discipline, the live-status-transport open question (§12) this RFC's open question
  (a) resolves, the per-kind finalizer drain the §3.4 fallbacks serve.
- **agentctl RFC 0007** — Admission validation ladder: validates the CRs whose rendered pods
  the node-agent then reaches; shares the capability cache with the operator.
- **agentctl RFC 0009** — Management access path & RBAC: the access *policy* behind the §7.2
  chokepoint — the aggregated-APIServer human path, operator mTLS, per-verb RBAC + end-user
  identity, the `pods/proxy` stopgap (D5). This RFC owns the API surface; RFC 0009 owns who
  gets through.
- **agentctl RFC 0010** — Observability & telemetry bridge: the schemas/relabeling/`http_sd`
  discovery, the run-outcome store, and exit-code → `podFailurePolicy` that Tier A (the
  on-node collector, §3.2) feeds; the stderr→Loki bulk-event path the ring tail complements.
- **agentctl RFC 0013** — A2A gateway & task store: the durable store schema + HA/DR, the
  webhook registry, `ListTasks`, A2A version negotiation + the wire-string commitment (P2),
  central Agent-Card signing, rate-limit/quota state. This RFC owns node-locality + the relay
  (Tier B); RFC 0013 owns the gateway + store.
- **agentctl RFC 0016** — CLI & kubectl-plugin grammar: the interactive attach UX, the
  single-writer lease, and the cold/live data paths that drive the §7 API; the `Steer`
  multiplexing policy.
- **agentctl RFC 0017** — Release & lifecycle engineering: agentctl's own multi-component
  upgrade + skew matrix that the §8.2 per-tier cadences slot into.

**Contract spec (the reference implementation's current home — agentd RFCs)**

- **agentd RFC 0014** — control-plane contract umbrella: the data/control-plane split
  ("agent exposes primitives; agentctl owns policy", §3), `surfaces{}` as the single
  discovery point (§6.2), contract versioning/negotiation (§6.3), the downward-API env family
  (§6.4), graceful degradation (§8).
- **agentd RFC 0015** — management & control surface: `--serve-mcp` transports and the
  thread-per-connection listener (§3 — the basis of Argument 1), `PeerOrigin::Management`
  (§3.3/§3.4), the operator tools `drain`/`lame-duck`/`pause`/`resume`/`cancel` (§4) and
  `attach`=`subagent.send` (§4.5), the manifest/inventory/status resources (§5.2–5.4),
  reachability == operator authority (§7), reconnect = a clean re-read with no per-connection
  state (§8 — the basis of the §3.3 invariant), the downward-API identity (§6).
- **agentd RFC 0010** — observability, health & telemetry: liveness = the supervisor
  heartbeat / `--health-file` independent of any management connection (§3.7), the `agent_*`
  metric convention Tier A scrapes.
- **agentd RFC 0016** — telemetry & lifecycle contract: the `agent://events` lossy ring +
  cursor/`dropped` semantics Tier A tails (§7.2), the run-report object + `--report-file` /
  `agent://run/{run_id}` the run-outcome capture reads, the frozen metrics surface.
- **agentd RFC 0020** — A2A over the substrate: A2A served over vsock/unix (§2), the
  distillate delivered once / live-only registry (§4/§5), the gateway-as-PEP and node-pinned
  topology (§5), descriptive caller/tenant `_meta` (§5), stateless-agent / stateful-gateway
  (§6) — the basis of Tier B.
- **agentd RFC 0006/0007** — intelligence transport and the agentic loop: the loop blocks on
  the model call (the basis of the §6 per-node-inference-SPOF argument).
- **agentd RFC 0011** — idempotency & exit-code contract: no per-connection durable state and
  side-effect dedupe by `run_id` (§6/§7) — the basis of the §3.3 bounce-safe re-read invariant
  and the §4.2 FAILED + idempotent-re-drive contract (re-drive dedupes side effects by
  `run_id`; it does not make whole-task re-execution safe).
- **agentd RFC 0012** — security posture: the transport-is-the-boundary trust model (§3.8) the
  node-agent inherits as the management/A2A PEP-adjacent principal; the reader/actor
  distillate firewall.
- **agentd RFC 0018** — intelligence resilience: failover / breaker / all-down / hot-swap
  *across* pool endpoints and the frozen `agent_intel_endpoint_*` metrics — the basis of the
  §6 argument that an embedded per-node intelligence proxy collapses inter-pool failover into
  a shared-fate hop.

**Contract asks raised or cited by this RFC** (agentctl brainstorm §14): **P1** (exec-health
verb — the kubelet probe that replaces a dial to the node-agent, §3.3), **P2** (`surfaces.a2a`
+ A2A wire-string commitment — the relay's surface, §4, owned by RFC 0013), **P4** (define
`agent://metrics` over the management socket — networkless telemetry, §3.2 / open question
(f)), **P5** (read-before-exit / terminal-distillate re-read — the relay's must-not-miss
property and run-outcome capture, §3.2/§4.2/open question (d)), **P-meta** (descriptive
caller/tenant `_meta` — §4.4/§7.3), **P-audit** (closed-vocabulary `mgmt.invoked` management-
action event — §7.3), **P-pause** (the unbuilt `pause`/`resume` operator tools the §3.1/§7.1
verb set renders only when advertised — brainstorm §0.6), **P-inject** (frozen `InjectEvent`
shape + an inject event in the closed vocabulary — the §7.3 per-inject steering audit; the
interactive multi-viewer attach UX that consumes it is owned by agentctl RFC 0016).

*Where this RFC and a contract spec disagree on the wire, the contract wins and this RFC is
corrected; where this RFC identifies a missing or defective primitive (P5 distillate
re-read, P-meta, P-audit, P4 in-socket metrics), it becomes a contract ask — never a leak of
cluster logic into the agent.*
