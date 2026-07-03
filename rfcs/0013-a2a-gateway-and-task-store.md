# agentctl RFC 0013: A2A gateway & task store

> ⚠️ **Superseded in part by [RFC 0021](0021-contract-2.0-network-substrate-pivot.md) (contract 2.0 — the network is the substrate).** **Amended.** The gateway forwards **direct to the agent pod** `/mcp` (no relay); the wire is contract-2.0 (bare PascalCase methods, `{"task"}` envelope, proto3-JSON shapes, SSE without a `final` flag). The durable store + gateway-owned methods are unchanged. See RFC 0021 §8.

**Status:** Proposed (agentctl A2A-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the public A2A surface and its durability. It owns the **replicated, stateless HTTP gateway** (the A2A PEP: TLS/auth/SSE/webhooks/rate-limit/version-pinning), the **shared durable task store** (task records, status history, the webhook registry, the `tasks/list` index, rate-limit state), the **Agent Card projection** for a single Agent, and **delegation-out**. It fronts the **node-pinned relay** that agentctl RFC 0008 owns (Tier B node-locality + live-stream lifecycle ownership).

> **P0 — the gateway bridges a *conformant agent's* A2A surface, never a specific binary.**
> The gateway re-envelopes A2A frames to and from whatever A2A version + address the agent
> advertises in `surfaces.a2a` (contract ask **P2**), maps the agent's run/Task handles, and
> projects the agent's capabilities manifest into an Agent Card. Where this RFC cites a
> concrete A2A wire surface it names the **reference implementation** (agentd RFC 0020) as
> *where the contract is presently written down*, not as a dependency. The agent-branded
> spellings it touches (`a2a.*`/`agent://run|subagent`, the `agent/…` `_meta` keys) are
> contract-normative-but-branded and flagged for neutralization (the P0 contract-extraction
> open question, agentctl RFC 0001 §9).

> **The agent is never on the network; the gateway carries everything network-shaped.** A
> conformant agent serves *real* A2A over the substrate (vsock/unix) and deliberately omits
> the heavy machinery — TLS, OAuth/OIDC/mTLS, SSE, webhooks, durable history, `tasks/list`,
> version negotiation (agentd RFC 0020 §1/§7). The gateway is a **dumb transport bridge + a
> policy-enforcement point (PEP)**, not a protocol translator (agentd RFC 0020 §1): it re-frames
> A2A JSON-RPC between HTTP/SSE and the substrate, enforces auth, and serves the durable surface
> from the store. The strongest posture this preserves: an agent pod with **no cluster network
> at all** — substrate-in for management + A2A, substrate-out for the model (agentd RFC 0020 §4).

> **The ownership seam with agentctl RFC 0008.** RFC 0008 owns **node-locality and the relay**:
> *why* the live A2A leg is on the agent's node, *that* the relay must own the live task stream
> end-to-end (the distillate is delivered **once**), the relay's privilege model (it terminates a
> small mTLS-locked inbound control listener only the gateway dials, post-PEP — RFC 0008 §4.1),
> and the relay's failure/upgrade envelope. **This RFC owns the gateway, the store schema + HA/DR,
> the webhook registry + its SSRF/encryption controls, `tasks/list`, rate-limit/quota state, A2A
> version negotiation + the wire-string commitment (P2), the routing decision (taskId → owner,
> live-vs-terminal) and the wire shape of the gateway→relay live-op hop** — both of which RFC 0008
> §4.1 explicitly defers here. Central Agent-Card *signing custody* is agentctl RFC 0014.

---

## 1. Problem / Context

A conformant agent is already **task-shaped**: a run is an A2A Task, the capabilities manifest is
an Agent Card, and the agent serves the live A2A core (`SendMessage`/`GetTask`/`CancelTask`/the
same-id streaming response) over the substrate it already speaks — JSON-RPC over vsock/unix, with
the existing thread-per-connection listener and codec (agentd RFC 0020 §2/§5, RFC 0004). What the
agent **does not** carry, by deliberate design, is everything that makes A2A a *network* protocol
(agentd RFC 0020 §1/§4/§7):

- an **HTTP server** and **SSE** framing;
- **enforced auth** — OAuth2 / OIDC / mTLS / API key — and TLS termination;
- **webhooks** (`tasks/pushNotificationConfig/*`) and their delivery;
- **durable task history** and `tasks/list` (the agent serves only **live** tasks from an
  ephemeral registry and delivers the final distillate **exactly once** — agentd RFC 0020 §4/§5,
  RFC 0009 §7/§8);
- **A2A version negotiation** and the public `/.well-known/agent-card.json` endpoint.

That is the gateway's entire job: add the heavy network machinery **around** the agent without
putting the agent on the network. agentd RFC 0020 §1 states the boundary the whole control-plane
track uses — *primitives in the agent, the network surface in the gateway* (agentd RFC 0014 §3):

> **The agent serves real A2A over the substrate; an on-node bridge re-envelopes HTTP↔substrate.**

Because the agent serves *real* A2A (not a bespoke surface), the bridge is a **dumb transport
bridge**, not a protocol translator — *if and only if* the agent and the gateway agree on the A2A
version (the unresolved **P2**, §3.4). Three forces shape this RFC:

1. **The live leg is physically node-pinned; the durable surface is not.** vsock/unix is
   point-to-point guest↔host, so the live task stream is pinned to the agent's node — that is the
   relay, agentctl RFC 0008's concern. But `tasks/list`, history, the webhook registry, and the
   public HTTP surface must aggregate **across pods and nodes** and **survive pod/node loss** —
   that is a **shared durable store** and a **replicated, stateless gateway**, this RFC's concern
   (brainstorm D4).

2. **The distillate is delivered once, so durability is a must-not-miss property, not a
   convenience.** The agent is stateless and distillate-only (agentd RFC 0011, RFC 0020 §6); the
   relay must own the live stream for the full lifecycle or the final artifact is lost — acute for
   `once`-mode agents that exit immediately on delivery. The store is the durable truth the relay
   writes into; the gateway reads it. The contract gap this rides is **P5** (terminal-distillate
   re-read / read-before-exit).

3. **Hostile multi-tenancy is a v1 requirement (brainstorm §0.6).** The gateway is an additional PEP
   alongside the management enforcement points (agentctl RFC 0009 §6.1) and it is the one that terminates
   **untrusted, off-cluster** A2A input. So tenant isolation, per-tenant secret scoping,
   SSRF-guarded webhook delivery, and central (not per-node) card signing are load-bearing, not
   hardening afterthoughts (brainstorm §7.2).

This RFC owns: the gateway (§3, §4), the Agent Card projection for a single Agent (§5), the
durable task store (§6), trace correlation as the trace root (§7), delegation-out (§8), and the
strawman config + flows (§9). It does **not** own: the relay's node-locality + live-stream
ownership + privilege model (agentctl RFC 0008); the descriptor/attestation/substrate (agentctl
RFC 0002); the `surfaces.a2a` `.spec`/`.status` field (agentctl RFC 0003); the management access
path's RBAC (agentctl RFC 0009); the trace-correlation *schema* (agentctl RFC 0010); fleet-level
Agent Card / mesh identity / **card-signing key custody** + `/.well-known` mesh publication
(agentctl RFC 0014); the cross-cutting trust model + internal PKI + the egress allow-list home
(agentctl RFC 0015); the CLI grammar (agentctl RFC 0016).

---

## 2. Decision — a replicated stateless gateway + a shared durable store + the node-pinned relay

The A2A surface is **three parts with three reliability classes**, two of which are this RFC's:

```
                         A2A clients (cluster peers · cross-vendor · cross-org)
                                            │ HTTPS / JSON-RPC 2.0 / SSE / webhooks
                                            ▼
                    ┌───────────────────────────────────────────────────────────┐
   THIS RFC ──────► │  A2A HTTP GATEWAY  (Deployment, REPLICATED, STATELESS,      │
                    │                     NOT node-pinned; per-tenant, §4.5)      │
                    │  PEP   : TLS · OAuth/OIDC/mTLS/API-key (card securitySchemes)│
                    │  bridge: HTTP/SSE ↔ substrate frames (DUMB, not translator) │
                    │  serves: tasks/list · pushNotificationConfig/* · ext. card  │  reads/writes
                    │          (durable/registry methods) FROM THE STORE          │◄────────────┐
                    │  routes: message/* · live tasks/get · tasks/cancel ·        │             │
                    │          tasks/resubscribe → the OWNING node's relay        │             │
                    └───────────────┬───────────────────────────────────────────┬┘             │
        live-op hop (gateway→relay, │ mTLS, post-PEP; routing+wire = THIS RFC §3.5)             │
        wire shape THIS RFC; the    │                                                            │
        relay terminates it = 0008) ▼                                                            ▼
                    ┌───────────────────────────────────┐               ┌──────────────────────────────┐
   RFC 0008 ──────► │  A2A RELAY (DaemonSet, NODE-PINNED)│  write status │  SHARED DURABLE STORE         │
                    │  owns the LIVE task stream end-to- │──────────────►│  (Postgres; sqlite dev — §6)  │
                    │  end (distillate ONCE — RFC 0008   │  + final ref  │  task · status_history ·      │
                    │  §4.2); opens its own A2A conn per │               │  push_config · delivery_outbox│
                    │  local agent over the substrate    │               │  · rate_state · ListTasks idx │
                    └───────────────┬───────────────────┘               └──────────────────────────────┘
       A2A conn (vsock/unix; point- │ PeerOrigin::Management; a2a feature (agentd RFC 0020 §2)
       to-point ⇒ relay on the node)▼
                    ┌───────────────────────────────────────────────────────────┐
                    │  conformant agent pod (reference impl: agent) — NO network │
                    │  serves a2a.* over the substrate; run/subagent = A2A Task   │
                    └───────────────────────────────────────────────────────────┘
```

**The principles (final for the A2A surface):**

1. **The gateway is replicated, stateless, and per-tenant.** It holds **no** durable state — all
   durability lives in the store. Any replica can serve any durable method; live methods route to
   the owning node's relay. Under hostile tenancy the default is **one gateway Deployment per
   tenant** (§4.5), each with a tenant-scoped store credential and tenant-scoped webhook secrets.

2. **The gateway is a dumb bridge + a PEP, not a protocol translator — version permitting.** It
   re-frames JSON-RPC between HTTP/SSE and the substrate (§3.1). It becomes a *translation* layer
   **only** to the extent the agent's advertised A2A version differs from the version the gateway
   serves outward — which is exactly the **P2** decision that must be made first (§3.4).

3. **The method surface splits live vs durable/registry (§3.2).** Live methods (`message/send`,
   `message/stream`, live `tasks/get`, `tasks/cancel`, `tasks/resubscribe`) route to the owning
   node's relay → agent. Durable/registry methods (`tasks/list`,
   `tasks/pushNotificationConfig/*`, the extended card, terminal `tasks/get`) are served by the
   gateway from the store. The agent serves only **live** tasks; the store is everything else.

4. **The shared store is the durable truth (§6).** Postgres (pluggable; sqlite dev fallback) holds
   the task record, status history, the webhook registry, the delivery outbox, the `tasks/list`
   index, and rate-limit/quota state. The relay (RFC 0008) writes; the gateway reads and writes
   registry/quota. **Reject** pure-per-node (fails `tasks/list` + durability + webhook survival)
   and a Task/etcd CRD (task/status churn is an etcd anti-pattern, and webhook creds do not belong
   in etcd) — brainstorm D4.

5. **The relay owns the live stream for the full lifecycle; the gateway never assumes a live
   client is attached (§6.4).** Because the distillate is delivered once (agentd RFC 0020 §5), the
   durable record must be written by the relay regardless of any SSE client — agentctl RFC 0008
   §4.2 makes the relay the must-not-miss consumer; this RFC defines what it writes and how the
   store recovers on owner loss (§6.5/§6.6).

6. **The gateway is a PEP under hostile tenancy (§4).** It authenticates per the Agent Card
   `securitySchemes`, treats the `tenant` as a **row-level authorization predicate** (not a
   descriptive label), shares rate-limit state across replicas, SSRF-guards and encrypts webhook
   delivery, and never holds the card-signing key (central signing, agentctl RFC 0014).

7. **Identity is descriptive past the PEP.** The gateway authenticates the client, then stamps the
   caller/tenant + `traceparent` into the frame's `_meta` (contract asks **P-meta**/**P-trace**);
   the relay carries it unchanged (RFC 0008 §4.4); the agent records but **never re-verifies** it
   (agentd RFC 0015 §6, RFC 0012 §3.8). Authority lives entirely in the gateway PEP.

---

## 3. The gateway — the public A2A surface

The gateway is an ordinary networked Rust service (the `kube-rs`/`hyper`/`tonic` stack, agentctl
RFC 0001), a replicated stateless Deployment fronted by a cluster A2A `Service`. It does five
things: terminate TLS + auth (§4), bridge frames (§3.1), route by method (§3.2/§3.5), make SSE
(§3.3), and serve durable methods from the store (§3.2/§6).

### 3.1 A dumb HTTP↔substrate bridge, not a protocol translator

For the **live** methods the gateway is a frame re-enveloper. The mapping is mechanical (agent
RFC 0020 §5):

The middle column names **the agent's advertised A2A method over the substrate** (read
from `surfaces.a2a`, version per P2) — *not* any binary's internal verbs. How a
conformant agent realizes a Task internally (the reference impl maps it to its
run/subagent machinery — `subagent.spawn`/`subagent.send`/`subagent.cancel`,
`agent://run|subagent/{handle}`) is the **agent's private concern, documented in the
contract** (agentd RFC 0020 §4/§5), and the gateway has no business naming it. A
second-vendor agent that realizes A2A without a "subagent" concept satisfies this table
unchanged:

| A2A method (served outward) | agent's advertised A2A method over the substrate (`surfaces.a2a`, P2) | Owner of the live leg |
|---|---|---|
| `message/send` (new) | the agent's A2A send (reference spelling `SendMessage`/`message/send`) — starts a Task | relay → agent |
| `message/stream` (new) | the agent's A2A streaming send — starts a Task + streams its same-id status | relay → agent |
| `message/send` / `message/stream` (existing `taskId`/`contextId`) | the agent's A2A multi-turn send on the existing Task (warm-session steering) | relay → agent |
| live `tasks/get` | the agent's A2A get-task (reference spelling `GetTask`) | relay → agent |
| `tasks/cancel` | the agent's A2A cancel-task (reference spelling `CancelTask`) | relay → agent |
| `tasks/resubscribe` | the agent's A2A subscribe-to-task (reference spelling `SubscribeToTask`) — re-subscribe same-id stream | relay → agent |

The agent emits Task **status** transitions (and the final artifact = the distillate) as
line-framed JSON-RPC notifications with the **same id** as the originating request
(`StreamResponse`), terminated by `statusUpdate.final == true` (agentd RFC 0020 §5). The relay
drains these; the gateway re-frames them into SSE (§3.3). The gateway adds **no** semantics it did
not receive — it re-envelopes bytes, stamps `_meta` (§2 principle 7), and enforces policy. The
**distillate-only invariant holds**: the agent streams *status*, never partial artifacts (agent
RFC 0009 §8), and the Agent Card advertises this honestly (§5).

### 3.2 The method-routing split — live vs durable/registry

The split is the whole reason the surface is HA without making the agent stateful (brainstorm §7.1):

| A2A method (v1.0) | Served by | Source of truth |
|---|---|---|
| `message/send`, `message/stream` | **relay → agent** (live) | the live run; status persisted to the store as it flows |
| `tasks/get` — **non-terminal** | **relay → agent** (live) | the agent's ephemeral registry |
| `tasks/get` — **terminal** | **gateway** (short-circuit) | the **store** (the agent no longer holds it — RFC 0020 §4) |
| `tasks/cancel`, `tasks/resubscribe` | **relay → agent** (live) | the live run |
| `tasks/list` | **gateway** | the **store** ListTasks index (cross-pod/cross-node) |
| `tasks/pushNotificationConfig/set\|get\|list\|delete` | **gateway** | the **store** webhook registry |
| `agent/getAuthenticatedExtendedCard` | **gateway** | the projected card (§5) + store-held extended fields |

The gateway decides live-vs-terminal by reading the task's **state** from the store: a `tasks/get`
for a task in a terminal state (`completed`/`failed`/`canceled`/`rejected`) is answered from the
store; a non-terminal one is proxied to the owning relay. This is also what makes a request that
lands on the **wrong gateway replica** correct without sticky routing: any replica reads the same
store; only live operations need the owning node's relay (§3.5).

### 3.3 SSE re-framing and resumability (contract ask P-seq)

The gateway turns the relay's substrate stream into a **text/event-stream** SSE response for
`message/stream` and `tasks/resubscribe`. Each substrate status frame becomes one SSE event; the
gateway **closes the SSE stream on `statusUpdate.final == true`** (agentd RFC 0020 §5), having
emitted the final artifact (the distillate) as the last event.

**Resumability** is where honesty matters. A2A clients reconnect a dropped SSE with
`Last-Event-ID`. To honor it the gateway needs a **monotonic per-frame sequence** on the agent's
streaming notifications — which the contract does **not** emit today (contract ask **P-seq**,
parallel to the events-ring seq, agentd RFC 0016 §7.2). Therefore:

- **Until P-seq lands**, the gateway scopes replay to **terminal-state reconstruction**: on
  `Last-Event-ID`, it re-sends the current task **status** + the final artifact (if terminal) from
  the store's `status_history` (§6.3), and re-attaches to the live tail for a non-terminal task —
  it does **not** claim gap-free mid-stream replay it cannot guarantee.
- **With P-seq**, the gateway sets the SSE `id:` to the frame seq, persists each status frame in
  `status_history` keyed by `(task_id, seq)`, and on reconnect replays from `Last-Event-ID + 1`
  (store-backed up to the live cursor, then live). A **re-drive epoch** (§6.5) is part of the
  cursor so a cursor from a previous task incarnation is **rejected**, not mis-resumed.

The replay source is always the **store** (any gateway replica can read it), so resumable SSE does
not require sticky routing to a particular gateway replica — only the live tail of a non-terminal
task is proxied from the owning relay.

### 3.4 A2A version pinning — resolve it FIRST (contract ask P2)

The red-team correction (brainstorm §7.2) is binding: **do not present "dumb pass-through" and "pin
v1.0 outward" as compatible without resolving the wire strings.** And the problem is a
**JSON-RPC binding spelling-conformance** gap, not merely a version bump:

- A2A's **JSON-RPC binding** uses **slash-form** method strings — `message/send`, `message/stream`,
  `tasks/get`, `tasks/cancel`, `tasks/resubscribe`, `tasks/list`, `tasks/pushNotificationConfig/*`,
  `agent/getAuthenticatedExtendedCard` — and has done so **since 0.2.x**. The **PascalCase**
  `SendMessage`/`GetTask`/`CancelTask`/`SubscribeToTask` names are the **gRPC/protobuf
  service-method binding**, *not* a JSON-RPC version.
- The reference implementation (agentd RFC 0020 §5) carries PascalCase method names inside a
  **JSON-RPC-over-vsock** envelope — which does **not** match A2A's JSON-RPC binding in **either**
  version. So committing the agent to slash-form JSON-RPC strings is a real **wire change** in the
  `a2a` feature, not a relabel — and even a "0.2.x" agent should already be emitting slash-form
  JSON-RPC. The contract also currently serves the card at the older `/.well-known/agent.json`,
  while current A2A serves `/.well-known/agent-card.json`.

**Decision: the gateway serves current A2A outward (the exact version RFC 0013 pins; the served card
is §5); the agent-facing leg speaks whatever `surfaces.a2a` advertises.** Two consequences, both gated on
**P2** — which must ask for the agent's **exact JSON-RPC method strings + conformance to A2A's
JSON-RPC binding (slash-form)**, decoupled from the `protocolVersion` bump:

- **If the agent's JSON-RPC method strings do not match the slash-form binding** (the reference
  impl's PascalCase-in-JSON-RPC today), the gateway **is** a method-string translation layer for
  the live leg — a bounded schema map between the slash-form names it serves and the names the agent
  dials — and this RFC budgets it as such: a translation table + conformance against both spellings.
  It is still a *transport* bridge for everything else.
- **If/when the agent commits to the slash-form JSON-RPC binding** (via P2), the live leg degrades
  to a true **pass-through** and the translation table is retired.

**Outward negotiation is per the A2A spec, not a custom header.** The gateway serves the **public**
surface to real, third-party A2A clients, so it negotiates the way conformant clients expect:
it advertises `protocolVersion` in the **served Agent Card** and selects transport per the card's
`preferredTransport`/`additionalInterfaces` (§5). The non-standard `A2A-Version` /
`VersionNotSupported` mechanism (agentd RFC 0020 §6) is confined to the **internal** gateway↔relay /
agent-facing leg, where it is fine; it is **not** the public negotiation mechanism. The served
version + the agent-advertised version are both surfaced in the Agent Card and in `agentctl_a2a_*`
metrics labels (agentctl RFC 0010 §10). **Contract ask: P2** — and until it lands, the gateway reads
the version + method strings from the install/`A2AGateway` config (§9.1), not from a guess.

### 3.5 Routing — picking a live pod and reaching the owning relay

The gateway is not node-pinned, so it must (a) for a **new** task, pick a ready agent pod and reach
that node's relay, and (b) for a **live op on an existing** task, resolve `taskId → owner` and reach
that owner's relay. Both legs use the **gateway→relay live-op hop** whose wire shape this RFC owns
(RFC 0008 §4.1 defers it here) over the relay's mTLS-locked inbound control listener (which RFC
0008 owns the existence + privilege of):

- **New task placement.** The A2A request path identifies the target Agent/AgentFleet. The gateway
  resolves it to a **ready replica** via the node-agent-published live snapshot transport (the
  `AgentInstance`/EndpointSlice carrying `{namespace, name, uid, node}`, agentctl RFC 0008 OQ (a) /
  RFC 0006 §12) and dials that node's relay. For an **AgentFleet**, replica selection rides the
  scaling/claim plane (agentctl RFC 0011) rather than naive round-robin — the precise placement
  policy is RFC 0011's; this RFC requires only that the chosen pod is **ready + attested** (a
  descriptor with `attestation.verified == true`, agentctl RFC 0002 §7) and that the relay on that
  node becomes the task's recorded `owner_node`/`owner_pod_uid` in the store (§6.3).
- **Live op on an existing task (the wrong-gateway case).** The gateway reads
  `owner_node`/`owner_pod_uid` from the store and dials that node's relay. `owner_*` are **caches,
  not truth** (§6.6): if the resolve is stale (pod rescheduled) or the owning relay no longer holds
  a currently-attested descriptor for that `uid`, the gateway **MUST** re-resolve and, failing
  that, answer from the store if terminal or return a transient A2A error (mapped to a retryable
  JSON-RPC error) — never dial a stale/wrong relay. This mirrors the destructive-verb
  re-resolve-or-reject rule the management path uses (agentctl RFC 0009 §5.3.2).
- **Terminal short-circuit (no relay needed).** A `tasks/get`/`tasks/list` for terminal work never
  touches a relay — it is served from the store, so it works **even when the owning pod is gone**
  (the durability property that motivated the shared store).

---

## 4. The PEP — authn / authz / tenancy / rate-limit

The gateway is an **additional PEP** of the platform — agentctl RFC 0009 establishes a single
management enforcement point reached two ways (operator → node-agent direct mTLS; humans → the
aggregated APIServer), and names this A2A gateway as the **other** PEP through which the
`subagent.send` steering primitive is reachable (agentctl RFC 0009 §6.1). The management PEP fronts
the management transport for the operator + humans; the A2A gateway fronts the A2A transport for
**off-cluster, untrusted** peers. The agent re-verifies nothing (agentd RFC 0012 §3.8), so the
gateway owns **100%** of authn/authz at the A2A access path — the same posture agentctl RFC 0009
establishes for management, applied to a hostile-input-facing surface.

### 4.1 Authentication per the Agent Card `securitySchemes`

A2A auth is declared in the Agent Card. The gateway authenticates every inbound request against the
card's `securitySchemes` (§5) and rejects an unauthenticated/unmatched request **before** any byte
reaches the relay (agentd RFC 0020 §6). Supported schemes:

| Scheme | Mechanism | Notes |
|---|---|---|
| `oauth2` / `openIdConnect` | bearer-token validation against the configured issuer (JWKS) | the cross-org / cross-vendor default; per-tenant issuer config (§4.5) |
| `mtls` | client-certificate validation against a configured trust bundle | east-west / intra-mesh; the mesh CA is agentctl RFC 0014/0015 |
| `apiKey` | a keyed credential checked against a tenant-scoped secret | simplest; for trusted first-party callers |
| `http` (bearer) | a static bearer credential | lowest-assurance; single-tenant/dev only |

The authenticated principal becomes the **caller identity** the gateway stamps into `_meta`
(P-meta, §2 principle 7). The scheme config is per-`A2AGateway` (§9.1); credentials are
Secret-referenced and never inlined.

### 4.2 Authorization — the tenant is a row-level predicate, not a label

The red-team correction (brainstorm §7.2) is normative: **treat the `tenant` column as an
authorization predicate (row-level security), not "descriptive."** Concretely:

- Every store query the gateway issues is **scoped to the authenticated principal's tenant** — a
  `tasks/list`, a `tasks/get`, a `tasks/pushNotificationConfig/list` returns **only** the tenant's
  rows. This is enforced at the query layer **and**, where the backend supports it, at the database
  with **row-level security** + a per-tenant DB role (§6.2), so a query bug cannot leak a
  neighbour's tasks or webhook configs.
- A live op on a task is authorized by matching the task's stored `tenant` to the caller's tenant
  before the gateway dials the owning relay — a cross-tenant `taskId` is a `404`, not a `403`
  (existence of another tenant's task is not disclosed).
- **`subagent.send` (warm-session steering) is the A2A-side reachability of the same steering
  primitive the management `attach` verb gates** (agentctl RFC 0009 §6.1.2). Multi-turn
  `message/send` against an existing `contextId`/`taskId` is steering. The gateway therefore gates
  multi-turn the same way the management PEP gates `attach`: a per-tenant policy that may **deny
  multi-turn/steering** while allowing fresh task submission. The structural form — omitting
  `subagent.send` from the agent surface entirely for a no-puppeting tenant — is the contract ask
  **P-attach-gate** (home: agentctl RFC 0015); until it lands, the gateway enforces the gate
  itself, and a "no-puppeting" guarantee that *only* the management PEP enforced would be
  incomplete without this one (agentctl RFC 0009 §6.1.2 names this gateway as the missing half).

### 4.3 Rate-limit / quota is shared state

A client load-balanced across **N** gateway replicas must not get **N×** the limit (brainstorm
D4). So rate-limit/quota counters are **shared state in the store** (`rate_state`, §6.3), not
per-replica memory:

- Per-tenant + per-method token-bucket / sliding-window counters live in the store; each replica
  reads-modifies-writes atomically (a single-row `UPDATE … RETURNING`, or a small Lua-on-Redis
  shard for high-QPS tenants — backend choice §6.2).
- Alternatively, **consistent ingress routing** (hash a tenant to a replica subset) bounds the
  fan-out, but the store-backed counter is the correct default because the gateway must stay
  replica-fungible for HA.
- Quota exhaustion returns the A2A-mapped rate-limit error before any relay dial; the event is
  audited and surfaced on `agentctl_a2a_requests_total{code}` (agentctl RFC 0010 §10).

### 4.4 Webhook delivery — SSRF-guarded, encrypted, at-least-once

`tasks/pushNotificationConfig/*` registers client-supplied callback URLs; the gateway POSTs task
status notifications to them. This is the platform's **one component that POSTs to client-supplied
URLs from inside the cluster**, so it inherits the strictest egress controls (brainstorm D4, §7.2):

- **SSRF controls.** Block the cloud metadata endpoint (`169.254.169.254`), link-local, loopback,
  and (by default) RFC 1918 ranges; resolve-then-pin the destination IP to defeat DNS rebinding;
  enforce a per-tenant **allow-list** of callback hosts. These are the **same** controls
  delegation-out uses (§8) — one egress-policy implementation, two callers (the egress allow-list
  home is agentctl RFC 0015).
- **Encrypted creds at rest.** Webhook auth tokens are **never** stored in plaintext — the store
  must not hold plaintext push tokens (the same reason etcd was rejected, brainstorm D4). They are
  envelope-encrypted (a per-tenant data key wrapped by a KMS/operator key, agentctl RFC 0015) and
  decrypted only in the delivery worker at POST time.
- **At-least-once, deduplicated.** Deliveries are rows in a `delivery_outbox` (§6.3) claimed by a
  delivery worker with a short lease, so N replicas do not double-fire; each carries a stable
  delivery id for client-side dedup, with bounded exponential-backoff retries and a dead-letter
  state. The delivery loop runs **in the gateway Deployment** (it already has network + the store
  credential); a dedicated delivery Deployment is an option for high-volume tenants (§9.1).
- **The final webhook is the durability backstop.** On owner loss (§6.5) the default is **FAIL the
  task + fire the final webhook** — so a client with a webhook learns of a lost task even if it was
  never streaming.

### 4.5 Per-tenant gateways + central card signing

Under hostile tenancy the **default is one gateway Deployment per tenant** (§9.1), each with:

- a **tenant-scoped store credential** (a DB role that can touch only that tenant's rows, §6.2) —
  so a compromised gateway cannot read another tenant's tasks or webhook creds;
- **tenant-scoped auth config** (its own issuer/trust bundle/API-key secret, §4.1);
- an **independent failure domain + rollout cadence** (it terminates untrusted TLS; its security
  review is independent of the privileged tiers — agentctl RFC 0008 §8.2).

A **shared multi-tenant gateway** (one Deployment, row-level isolation) is acceptable **only** for
single-tenant / trusted clusters, the same tier framing as the substrate (agentctl RFC 0002 §5).

**Card signing is central, never per-node/per-gateway.** A signed Agent Card (JWS) for cross-org
trust **MUST** be signed by a single cluster signing identity, **not** a per-node or per-gateway
key — a per-node key lets any compromised node forge cross-org cards (brainstorm §7.2). The
gateway **serves** the card and may attach a centrally-produced signature, but **never holds the
signing key**; key custody + the fleet/mesh `/.well-known` publication is agentctl RFC 0014.

---

## 5. The Agent Card projection

The Agent Card is the **capabilities manifest, re-serialized into A2A's schema** — one builder, no
second source of truth (agentd RFC 0020 §3, RFC 0015 §5.2). The gateway serves a **single Agent's**
card at `/.well-known/agent-card.json` (the v1.0 path; the v0.2.x `/.well-known/agent.json` is
served as an alias while P2 is unresolved). The **fleet-level** card, the cross-fleet mesh
identity, and the signed cross-org publication are agentctl RFC 0014.

The projection reads `surfaces.a2a` (contract ask **P2**) and the manifest, and is **truthful about
what the agent actually does** (brainstorm §7.2 — resolve before publishing any signed cross-org
card):

```jsonc
// GET /.well-known/agent-card.json  — projected from the manifest; per-Agent (fleet card = RFC 0014)
{
  "protocolVersion": "1.0",
  "name": "triage",
  "description": "<from the manifest>",
  "url": "https://a2a.acme.example/agents/triage",      // the tenant gateway Service (§4.5)
  "preferredTransport": "JSONRPC",
  "capabilities": {
    "streaming": true,                                  // STATUS-LEVEL ONLY — see note below
    "pushNotifications": true,                          // gateway-provided (§4.4), not the agent
    "stateTransitionHistory": true                      // gateway-provided from the store (§6.3)
  },
  "securitySchemes": { "oauth2": { /* per-tenant issuer, §4.1 */ } },
  "security": [ { "oauth2": [ "a2a.tasks" ] } ],
  "skills": [ /* projected from the manifest's advertised capabilities */ ],
  "defaultInputModes":  [ "text/plain", "application/json" ],
  "defaultOutputModes": [ "text/plain", "application/json" ],

  // x-agentctl: NON-STANDARD honesty annotations (not auth-bearing; descriptive)
  "x-agentctl": {
    "servedA2AVersion": "1.0",            // what the gateway serves outward (§3.4)
    "agentA2AVersion":  "0.2.x",          // what surfaces.a2a advertises (P2) — translation if != served
    "streamingSemantics": "status-only"   // distillate-only: status transitions + ONE final artifact
  }
}
```

Two honesty rules are normative (brainstorm §7.2):

- **`capabilities.streaming: true` means STATUS-level streaming only** — status transitions plus a
  single final artifact (the distillate), **not** incremental artifact streaming (agentd RFC 0009
  §8, RFC 0020 §6). The card MUST NOT imply token-by-token artifact streaming the agent does not do.
- **Interaction modes are declared truthfully.** The agent is fire-and-distill; multi-turn is
  `contextId` → a warm session (`subagent.send`, agentd RFC 0015 §4.5), and `input-required` /
  `auth-required` interaction states are supported only insofar as the warm-session model allows.
  `pushNotifications` and `stateTransitionHistory` are **gateway-provided** (the agent has neither),
  and the card advertises them as gateway capabilities, not agent capabilities. This MUST be
  resolved **before** any signed cross-org card is published (agentctl RFC 0014).

---

## 6. The durable task store (brainstorm D4)

### 6.1 Why a shared store — and the rejected shapes

Three contract requirements **cannot** be satisfied per-node (brainstorm D4):

- **`tasks/list` aggregates across many pods on many nodes** for an Agent/AgentFleet.
- **Durability must survive node/pod loss** — the agent is stateless, distillate-only (agentd RFC
  0020 §6).
- **The webhook registry must survive the task's node** and fire even if the owning pod
  reschedules.

| Alternative | Verdict |
|---|---|
| **Shared durable store + node-local live relay** (CHOSEN) | Satisfies all three; the live stream stays node-pinned (relay, RFC 0008); the durable record + registry + index + quota are shared. |
| Pure per-node store | **Rejected** — fails `tasks/list`, durability across reschedule, and webhook survival. |
| A `Task` CRD in etcd | **Rejected** — per-task + per-status-event churn is an etcd anti-pattern (watch storms, write-amplification, brainstorm §2.2/D4), and webhook creds **do not belong in etcd**. |
| An MCP backing service for the store | **Rejected** — reinvents a relational store behind an extra hop (brainstorm D4). |

### 6.2 Backend — Postgres (pluggable), sqlite dev fallback

The store is a **pluggable** backend behind a narrow repository trait (agentctl RFC 0001 stack):

- **Postgres** is the production default — relational integrity for the task/status/registry model,
  **row-level security** + per-tenant roles for the §4.2 tenant predicate, `SKIP LOCKED` for the
  delivery-outbox + lease sweeps, and mature HA/DR (the cluster's existing Postgres operator or a
  managed instance). HA/DR (replication, PITR, backup cadence) is an operational concern this RFC
  requires but delegates to the chosen Postgres deployment (and the broader DR story, agentctl RFC
  0017).
- **sqlite** is the **dev/single-tenant fallback** only — embedded, zero-ops, no row-level
  security; never under hostile tenancy. The same tier framing as the substrate (agentctl RFC
  0002).
- A **high-QPS rate-limit shard** (Redis/Valkey) is an *optional* swap-in for `rate_state` only
  (§4.3); the durable task/registry data always lives in the relational backend.

### 6.3 Schema (strawman)

```sql
-- TENANT as a ROW-LEVEL AUTHORIZATION PREDICATE (§4.2), enforced by RLS + per-tenant DB roles,
-- NOT a descriptive column. All CLIENT-QUERYABLE tables (`task`, `push_config`) carry `tenant`
-- directly. `status_history` and `delivery_outbox` are GATEWAY-INTERNAL (never queried by tenant
-- key directly) and inherit tenant TRANSITIVELY via their FK — `status_history.task_id → task.tenant`,
-- `delivery_outbox.push_id → push_config.tenant`; RLS is enforced on them via an FK-join policy
-- (a USING clause that joins to the parent's tenant), so a query bug cannot leak a neighbour's rows.
-- Webhook creds are ENVELOPE-ENCRYPTED at rest (§4.4).

CREATE TABLE task (
  task_id        text PRIMARY KEY,              -- A2A taskId
  context_id     text,                          -- A2A contextId (multi-turn / warm session)
  tenant         text NOT NULL,                 -- authz predicate (RLS)
  agent_ns       text NOT NULL,
  agent_name     text NOT NULL,
  agent_uid      text,                          -- the instance once placed (§3.5)
  owner_node     text,                          -- CACHE, not truth (§6.6)
  owner_pod_uid  text,                          -- CACHE, not truth (§6.6)
  state          text NOT NULL,                 -- submitted|working|input-required|completed|failed|canceled|rejected
  idempotency_key text,                         -- the run_id (agentd RFC 0011 §6): dedupes side effects WITHIN a
                                                --   single run only — it does NOT make whole-task RE-DRIVE safe
                                                --   (a re-drive starts a fresh run_id); re-drive safety is the
                                                --   per-fleet idempotency opt-in (§6.5), not this key
  redrive_epoch  int  NOT NULL DEFAULT 0,       -- bumped on re-drive; part of the SSE cursor (§3.3/§6.5)
  final_artifact_ref text,                      -- pointer to the distillate (object store / inline if small)
  lease_expires_at timestamptz,                 -- relay liveness lease (§6.6)
  created_at     timestamptz NOT NULL DEFAULT now(),
  updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX task_list_idx ON task (tenant, agent_ns, agent_name, state, updated_at DESC);  -- tasks/list

CREATE TABLE status_history (                   -- the stateTransitionHistory + SSE replay source (§3.3)
  task_id  text NOT NULL REFERENCES task(task_id),
  seq      bigint NOT NULL,                     -- monotonic per-frame seq (contract ask P-seq); else gateway-assigned coarse seq
  epoch    int    NOT NULL,                     -- = task.redrive_epoch at write time (stale-cursor rejection)
  state    text   NOT NULL,
  message  jsonb,                               -- the A2A status payload (final carries the distillate ref)
  final    boolean NOT NULL DEFAULT false,      -- statusUpdate.final (agentd RFC 0020 §5)
  at       timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (task_id, seq)
);

CREATE TABLE push_config (                      -- the webhook registry (tasks/pushNotificationConfig/*)
  id          text PRIMARY KEY,
  tenant      text NOT NULL,                    -- authz predicate (RLS)
  task_id     text,                             -- or context-scoped
  url         text NOT NULL,                    -- SSRF-validated at register + at delivery (§4.4)
  token_enc   bytea,                            -- ENVELOPE-ENCRYPTED auth token — never plaintext (§4.4)
  created_by  text NOT NULL,                    -- caller principal (descriptive)
  created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE delivery_outbox (                  -- at-least-once webhook delivery (§4.4)
  delivery_id  text PRIMARY KEY,                -- stable; client dedupes on it
  task_id      text NOT NULL,
  push_id      text NOT NULL REFERENCES push_config(id),
  attempt      int  NOT NULL DEFAULT 0,
  next_at      timestamptz NOT NULL DEFAULT now(),
  lease_owner  text,                            -- claimed via SELECT … FOR UPDATE SKIP LOCKED
  lease_expires_at timestamptz,
  state        text NOT NULL DEFAULT 'pending'  -- pending|delivered|dead-letter
);

CREATE TABLE rate_state (                       -- shared rate-limit/quota counters (§4.3)
  tenant   text NOT NULL,
  method   text NOT NULL,
  window   bigint NOT NULL,
  count    int  NOT NULL DEFAULT 0,
  PRIMARY KEY (tenant, method, window)
);
```

The relay (agentctl RFC 0008) **writes** `task` state transitions + `status_history` + the
`final_artifact_ref`, and keeps `lease_expires_at` fresh while it owns a live stream. The gateway
**reads** for the durable methods and **writes** `push_config` / `delivery_outbox` / `rate_state`.

### 6.4 The relay owns the live stream end-to-end; the store is the durable truth (P5)

agentctl RFC 0008 §4.2 establishes the must-not-miss property; this RFC defines the store contract
it satisfies. The relay drains the substrate task stream for the **full lifecycle independent of any
HTTP client** and writes each transition to `status_history`, culminating in the final transition
that carries the **distillate** (delivered **once**, agentd RFC 0020 §5). So:

- A `tasks/get` for a terminal task is answered from the store **even after the pod exits** — the
  durability the whole design exists for (acute for `once`-mode, §1).
- An SSE client that connects *after* completion still gets the final artifact (replay from
  `status_history`, §3.3).
- The store is the truth; the relay holds **no** durable state of its own (RFC 0008 §4.3).

This rides contract ask **P5** (a short post-terminal **linger** / read-before-exit / re-read of
the terminal distillate by run handle, agentd RFC 0016/0020) — the window in which the relay must
capture the once-delivered distillate. **Until P5**, the lost-window contract is §6.5.

### 6.5 Owner loss — FAILED by default, idempotent re-drive only by opt-in

If the relay misses the distillate (a relay crash at the instant of completion, or a pod loss
before terminal), the **default is FAIL the task + fire the final webhook** (§4.4) — never silent
re-execution (brainstorm D4, agentctl RFC 0008 §4.2):

- **Re-drive is opt-in and gated.** Re-running the whole task is safe **only** if the composition
  is idempotent. RFC 0011 §6 dedupes MCP-backing-service side effects by `run_id`
  (`task.idempotency_key`); it does **not** make whole-task re-execution safe for non-idempotent
  compositions. So re-drive is behind an explicit **per-fleet opt-in** that *asserts* idempotency.
- **The re-drive epoch.** A re-drive bumps `task.redrive_epoch` and starts a fresh
  `status_history` lineage at the new epoch. Any SSE cursor (`Last-Event-ID`, §3.3) or webhook
  carrying a prior epoch is **rejected/ignored**, so a stale stream is not silently stitched onto a
  re-driven run.
- **The re-drive *policy* is this RFC's; the relay's must-not-drop *requirement* is RFC 0008's.**
  The two compose: RFC 0008 guarantees the relay tries to own the stream end-to-end; this RFC
  defines what happens when it cannot.

### 6.6 Orphan reconciliation — the lease sweep

`owner_node`/`owner_pod_uid` are **caches, not truth** (brainstorm §7.2). A **lease-expiry sweep**
(a leader-elected store job, or any gateway replica holding a sweep lease) transitions tasks whose
`lease_expires_at` has passed and whose owning pod is gone (cross-checked against the node-agent
snapshot, RFC 0008 OQ (a)) from `working` to `failed`/`lost`, applying the §6.5 owner-loss policy
(FAIL + final webhook, or opt-in re-drive). The relay keeps its lease fresh while it owns a live
stream and surfaces owner loss promptly (RFC 0008 §4.2); the sweep is the backstop for the cases
the relay cannot self-report (it crashed).

---

## 7. Trace correlation — the gateway is the trace root (contract ask P-trace)

The gateway is the **trace root** for an inbound A2A flow (agentctl RFC 0010 §8.1). It is the one
place a cross-org request enters the platform, so:

- It **adopts the caller's `traceparent`** (W3C trace-context) if present, or **mints** one, and
  sets **`_meta.traceparent`** on the frame it forwards over the live-op hop to the relay → agent.
  The agent then **adopts-or-mints** and carries `trace_id` through every log line, event, the run
  report, every outbound MCP/LLM call, and the spawn payload (agentd RFC 0010 §3.6) — so a
  multi-pod, multi-hop flow (`A2A client → gateway → relay → agent → MCP backing service → delegated
  A2A peer`) is **one trace** with no agent change.
- **The missing primitive is P-trace.** Whether the agent **ingests** an inbound `traceparent` *on
  the A2A method surface* (a substrate frame) is unspecified — the contract defines ingest on the
  self-MCP request and via `AGENT_TRACEPARENT`, but not on the A2A surface the gateway uses
  (agentd RFC 0020 / RFC 0010 §3.6). Until **P-trace** lands, a gateway-rooted trace cannot be
  *claimed* — only the self-MCP/env-rooted one — and the gateway records the trace as a
  **control-plane span** that joins by `trace_id` regardless (agentctl RFC 0010 §8.3).
- The gateway emits `agentctl_a2a_requests_total{method,code}` /
  `agentctl_a2a_request_duration_seconds` on its own `/metrics` (agentctl RFC 0010 §10.1), scraped
  directly (it has network). Cross-pod correlation is `trace_id` + the span tree, **never**
  `agent_path` (agentctl RFC 0010 §8.2).

---

## 8. Delegation-out — the agent dials A2A out over the substrate (contract ask P-a2a-out)

agentd RFC 0020 §3 adds a **remote-A2A delegation backend** beside the local subagent: a
coordinator can spin up sub-work on a remote agent, and the agent becomes an A2A **client**. But the
agent has **no cluster network** — so, symmetrically to intelligence egress (the inverse of A2A
ingress, agentd RFC 0020 §4), the agent dials A2A **out over the substrate**, and a node-local
egress bridge carries it to the mesh:

```
   agent (no network) ──A2A out, over the substrate──►  node-local A2A egress bridge
        --a2a-peer <grammar>  (contract ask P-a2a-out)        │  mTLS (mesh identity, RFC 0014/0015)
                                                              │  SSRF / egress allow-list (= §4.4, RFC 0015)
                                                              ▼
                                            the target agent's A2A gateway (the mesh)
```

- **The flag schema + dial grammar is contract ask P-a2a-out** — `--a2a-peer` (agentd RFC 0020 §3)
  needs a concrete flag schema and an outbound-dial grammar (how the agent names a remote peer over
  the substrate). agentctl renders it from the Agent/AgentFleet spec but **cannot define the wire**;
  that is the contract's.
- **Delegation-out is egress, so it inherits the egress controls.** It applies the **same**
  SSRF/allow-list controls as webhook delivery (§4.4) — it is the lethal-trifecta exfil leg as much
  as the model channel is (agentctl RFC 0002 §10) — and is subject to a **per-tenant A2A egress
  allow-list** (home: agentctl RFC 0015). The outbound dial authenticates with the agent's **mesh
  identity** (mTLS client cert, agentctl RFC 0014/0015), not a per-node key.
- **Where the egress bridge lives is an open question (§ open questions).** It could ride the
  relay's existing A2A machinery (node-pinned, already substrate-adjacent) or a dedicated egress
  path — the same out-of-the-privileged-tier reasoning that keeps the intelligence proxy out of the
  node-agent (agentctl RFC 0008 §6) applies, so this RFC leans toward **not** folding untrusted
  outbound dialing into the privileged tiers, and records the decision as open pending RFC 0014/0015.

---

## 9. Strawman — the `A2AGateway` config + worked flows

### 9.1 `A2AGateway` (per-tenant, namespaced)

A2A exposure **per Agent** is `spec.surfaces.a2a` on the `Agent`/`AgentFleet` (agentctl RFC 0003
§3.3, gated on P2). The **gateway deployment + per-tenant policy** is a namespaced `A2AGateway`
config object (the A2A-milestone object brainstorm §2.1 deferred to here; *not* a per-Agent
`Task`/`Run` CRD, which RFC 0004 rejects). Whether this is a thin CRD or install-time config is an
open question; the **shape** is:

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: A2AGateway
metadata: { name: acme, namespace: tenant-acme }     # one per tenant under hostile tenancy (§4.5)
spec:
  servedA2AVersion: "1.0"                             # gateway serves v1.0 outward (§3.4); agent ver from surfaces.a2a (P2)
  service: { type: ClusterIP, tls: { secretRef: { name: acme-a2a-tls } } }
  replicas: 3                                         # stateless; HA via the cluster A2A Service
  auth:                                               # the card securitySchemes (§4.1)
    schemes:
      - { kind: oauth2, issuer: https://idp.acme.example, audience: a2a.acme }
  store:
    backend: postgres                                 # postgres | sqlite(dev only) (§6.2)
    credentialRef: { name: acme-a2a-store }           # a TENANT-SCOPED DB role (§4.5)
    rowLevelSecurity: true
  webhooks:                                           # push notifications (§4.4)
    delivery: in-gateway                              # in-gateway | dedicated
    egressAllowList: ["hooks.acme.example"]           # SSRF allow-list (egress home = RFC 0015)
    credentialEncryption: { kmsKeyRef: { name: acme-a2a-kms } }   # envelope-encrypt webhook tokens
  rateLimit: { perTenantQps: 50, perMethodQps: { "message/stream": 10 } }   # shared state (§4.3)
  steering: { allowMultiTurn: false }                # the A2A-side no-puppeting gate (§4.2; P-attach-gate)
  delegationOut:                                     # §8
    enabled: true
    egressAllowList: ["a2a.partner.example"]         # same egress controls as webhooks
status:
  conditions:
    - { type: Available,        status: "True",  reason: ReplicasReady }
    - { type: StoreReachable,   status: "True",  reason: PostgresConnected }
    - { type: A2AVersionPinned, status: "True",  reason: ServedV1AgentV02xTranslating }  # §3.4 (P2)
```

### 9.2 Flow — inbound `message/stream` (a new task)

```
1. A2A client → POST /agents/triage  (message/stream) at the tenant gateway Service (HTTPS).
2. Gateway PEP: authenticate per card securitySchemes (§4.1) → caller principal + tenant=acme.
   Authorize: tenant rate-limit OK (§4.3, shared state); fresh submission allowed (§4.2).
3. Gateway adopts/mints traceparent (§7); resolves triage → a READY+ATTESTED pod via the node-agent
   snapshot (§3.5; fleet placement → RFC 0011). Inserts task{state:submitted, tenant:acme}.
4. Gateway dials the OWNING node's relay (the live-op hop, mTLS, post-PEP), stamping _meta
   {caller, tenant, traceparent} (P-meta/P-trace). Relay starts the run on the agent over the
   substrate (RFC 0008); becomes owner_node/owner_pod_uid; keeps the lease fresh (§6.6).
5. Agent emits same-id StreamResponse status frames; relay drains them, writes status_history
   (+ final_artifact_ref on the final frame, delivered ONCE — P5), and forwards live to the gateway.
6. Gateway re-frames each into SSE; closes the SSE stream on statusUpdate.final==true (§3.3).
7. On a dropped SSE + Last-Event-ID: replay status from the store (terminal-state reconstruction
   today; gap-free from `seq` once P-seq lands), re-attach live if non-terminal (§3.3).
```

### 9.3 Flow — `tasks/get` on a terminal task, hitting the "wrong" gateway replica

```
1. A2A client → tasks/get{taskId} lands on gateway replica B (not the one that started it).
2. Replica B reads task{taskId} from the SHARED store (tenant-scoped, §4.2). state==completed.
3. Terminal → served entirely from the store (final_artifact_ref + status_history). NO relay dial,
   works even though the owning pod is gone (§3.2/§6.4). [The durability the design exists for.]
   (If it were non-terminal: resolve owner_node from the store; re-resolve-or-reject if stale (§3.5).)
```

### 9.4 Flow — webhook delivery on completion

```
1. Earlier: client registered tasks/pushNotificationConfig/set{url, token}. Gateway SSRF-validates
   url against the tenant allow-list, envelope-encrypts token, inserts push_config (§4.4).
2. On the task's terminal transition (relay write, §6.4), a delivery_outbox row is enqueued.
3. A gateway delivery worker claims it (SELECT … FOR UPDATE SKIP LOCKED), re-validates url (DNS-rebind
   pin), decrypts token, POSTs the status notification with a stable delivery_id (client dedupes).
4. Failure → bounded backoff via next_at; exhaustion → dead-letter. Owner-loss FAIL also fires the
   final webhook (§4.4/§6.5), so a webhook client learns of a lost task even if never streaming.
```

---

## Non-goals

- **The relay's node-locality, live-stream lifecycle ownership, and privilege model.** agentctl RFC
  0008 (Tier B). This RFC owns the gateway, the store, the routing *decision*, and the wire shape of
  the gateway→relay hop; RFC 0008 owns *that* the relay terminates that hop and *why* the live leg is
  node-pinned.
- **The substrate, the endpoint descriptor, and attestation.** agentctl RFC 0002. The gateway
  consumes "a ready, attested pod" via the node-agent snapshot; it does not discover or attest.
- **The `surfaces.a2a` `.spec`/`.status` field and mode→workload rendering.** agentctl RFC 0003.
  This RFC consumes the advertised A2A surface; it does not define the CRD field.
- **The management access path + RBAC.** agentctl RFC 0009. The A2A gateway is a *different* PEP with
  its own auth/durability; the management `attach` gate and the A2A multi-turn gate are siblings
  (§4.2), not the same enforcement point.
- **The scaling/claim plane + fleet replica placement.** agentctl RFC 0011. New-task placement to a
  ready replica for a fleet rides the claim/backlog plane; this RFC requires only ready+attested.
- **The intelligence egress proxy / ModelPool.** agentctl RFC 0012. Delegation-out (§8) is the A2A
  egress leg, distinct from the model egress leg, but shares the egress-control implementation.
- **Fleet-level Agent Card, mesh identity, card-signing key custody, and `/.well-known` mesh
  publication.** agentctl RFC 0014. This RFC projects a **single** Agent's card and never holds the
  signing key.
- **The cross-cutting trust model, internal PKI, the egress allow-list home, and the
  P-attach-gate home.** agentctl RFC 0015. This RFC enforces the A2A-PEP half and consumes the
  egress allow-list + the steering gate.
- **The CLI grammar (`kubectl agents tasks`, `kubectl agent card`) and the trace-correlation
  schema.** agentctl RFC 0016 / RFC 0010. The gateway is a cold/A2A data source the CLI reads.
- **An HTTP server in the agent, durable history in the agent, or any auth in the agent.** agent
  RFC 0020 §7 / RFC 0012 §3.8 — all of it is the gateway's, by construction.
- **Any data-plane internals.** The gateway bridges the contract A2A surface; it MUST NOT branch on
  one binary's flags, file layout, or `build_features` *values* — only on `surfaces.a2a` and the
  negotiated A2A/contract version (agentd RFC 0014 §6.2, P0).

---

## Open questions

(a) **A2A version commitment (P2) — pass-through vs translation.** The chosen posture serves v1.0
outward and translates the live leg if the agent advertises v0.2.x (§3.4). Confirm the exact wire
strings the reference impl's `a2a` feature registers and whether it commits to v1.0 (retiring the
translation table) or pins v0.2.x (keeping it). **Blocking: P2.**

(b) **Resumable SSE (P-seq).** Gap-free mid-stream replay needs a monotonic per-frame seq the agent
does not emit (§3.3). Until P-seq, replay is scoped to terminal-state reconstruction. Confirm the
seq is parallel to the events-ring seq and that the re-drive epoch (§6.5) is sufficient stale-cursor
rejection. **Blocking for resumable SSE: P-seq.**

(c) **The terminal-distillate window (P5).** The relay's must-not-miss property (§6.4, RFC 0008
§4.2) rides P5 (read-before-exit vs short post-terminal linger vs re-read-by-run-handle). Which
variant the contract adopts changes the relay's drain-on-upgrade story and the §6.5 lost-window
default. **Blocking for once-mode durability: P5.**

(d) **Descriptive caller/tenant `_meta` (P-meta) + traceparent ingest (P-trace).** The gateway
stamps both; the agent must accept `_meta` it records-but-never-verifies (P-meta) and ingest
`traceparent` on the A2A surface (P-trace) for a gateway-rooted trace to be claimable (§7). Confirm
both land on the A2A method surface, not only the self-MCP one.

(e) **Where the delegation-out egress bridge lives (§8).** Ride the node-pinned relay (substrate-
adjacent, but adds untrusted outbound dialing to a Tier B component) or a dedicated egress path
(more sprawl, cleaner failure domain — the same reasoning that keeps the intelligence proxy out of
the node-agent, RFC 0008 §6)? Resolve with agentctl RFC 0014/0015. Plus the `--a2a-peer` flag schema
+ dial grammar (**P-a2a-out**).

(f) **Store backend HA/DR ownership.** Postgres HA (replication, failover, PITR) and backup cadence
are required but delegated to the chosen deployment + agentctl RFC 0017. Confirm the cluster Postgres
operator vs a managed instance default, and whether the rate-limit shard (Redis/Valkey, §4.3) is in
v1 or deferred until a tenant's QPS forces it.

(g) **`A2AGateway` as a CRD vs install config (§9.1).** A thin namespaced CRD (declarative,
GitOps-native, per-tenant) vs install-time config (fewer API objects). RFC 0004 rejected a *per-Agent*
A2AGateway CRD; this is the *per-tenant gateway-deployment* object the A2A milestone owns. Settle the
shape and the `agents.x-k8s.io` vs `agentctl.dev` group string (inherits agentctl RFC 0003 §13.1).

(h) **East-west relay/gateway mutual auth + discovery, and the live-op hop wire shape (§3.5).** The
gateway→relay hop's mTLS identity model and the relay's inbound-listener wire shape (RFC 0008 §4.1
defers the shape here; RFC 0008 owns *that* it exists). Reconcile with the internal PKI (agentctl
RFC 0015) and the snapshot transport (RFC 0008 OQ (a)). Plus transport breadth: JSON-RPC only in v1,
or +gRPC/REST A2A bindings later?

(i) **Distillate artifact storage for `final_artifact_ref` (§6.3).** The once-delivered distillate
is the durability the whole design centers on, but the non-inline-artifact storage location (object
store vs inline) and the inline/object-store **size threshold** are unspecified. Settle the object
store (the cluster's existing object storage vs a managed bucket), the threshold, and its HA/DR
(distinct from the relational store's, OQ (f)) before once-mode durability is GA.

---

## References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — Stack & repo decision record: Rust for all five components, the
  `kube-rs`/`hyper`/`tonic` stack the gateway is built on, the generated A2A/contract client the
  gateway re-frames through, the P0 contract-as-schema anti-drift.
- **agentctl RFC 0002** — Substrate & transport abstraction: the endpoint descriptor + attestation
  (§3.5 requires ready+attested), the three tiers and the tenancy×substrate rule the per-tenant
  gateway/store framing (§4.5/§6.2) mirrors, the lethal-trifecta egress framing (§4.4/§8).
- **agentctl RFC 0003** — Agent & AgentFleet CRDs: `spec.surfaces.a2a` (§3.3, inert until **P2**),
  the `A2AUnsupported` condition, the curated `.status` the cold A2A reads complement.
- **agentctl RFC 0004** — AgentClass / IntelligenceService / MCPServerSet: rejects a per-Agent
  `A2AGateway`/`Task`/`Run` CRD and defers A2A to this RFC (§9.1); the egress-proxy plane (§8/§ NG).
- **agentctl RFC 0006** — Operator reconcile & capability model: the node-agent-published live
  snapshot transport (§3.5 placement / OQ) and the single-`.status`-writer discipline.
- **agentctl RFC 0008** — node-agent architecture (two tiers): **the relay** — node-locality, the
  live-stream lifecycle ownership (the distillate delivered once, §4.2), the relay's inbound control
  listener + privilege model (§4.1, whose routing + wire shape this RFC owns), descriptive `_meta`
  pass-through (§4.4), and the per-tier failure/upgrade envelope. The ownership seam is explicit:
  RFC 0008 owns node-locality + the relay; this RFC owns the gateway + store + protocol.
- **agentctl RFC 0009** — Management access path & RBAC: the management enforcement model this RFC's
  gateway is the **additional A2A-facing PEP** alongside (RFC 0009 §6.1, which names this gateway as
  the other reachability of `subagent.send`); the `attach`/`subagent.send` no-puppeting gate whose
  A2A-side half is this RFC's multi-turn gate (§4.2 / RFC 0009 §6.1.2); the re-resolve-or-reject
  routing rule (§3.5 / RFC 0009 §5.3.2).
- **agentctl RFC 0010** — Observability & telemetry bridge: the gateway as the **trace root** (§7 /
  RFC 0010 §8.1, **P-trace**), `trace_id` correlation (not `agent_path`), the `agentctl_a2a_*`
  control-plane series, and run-outcome capture (the relay's must-not-miss property is shared with
  Tier A capture, **P5**).
- **agentctl RFC 0011** — Scaling plane: fleet replica placement for new-task routing (§3.5) and the
  claim/backlog plane the gateway selects a ready pod through.
- **agentctl RFC 0012** — Intelligence plane: the model egress leg delegation-out (§8) is symmetric
  to and distinct from; the shared egress-control implementation.
- **agentctl RFC 0014** — Agent mesh identity: the **fleet-level** Agent Card, central card-signing
  key custody, the `/.well-known` mesh publication, and the agent's mesh identity for delegation-out
  (§5/§8) — this RFC projects a single Agent's card and never holds the signing key.
- **agentctl RFC 0015** — Security & multi-tenancy: the cross-cutting trust model, internal mTLS/PKI
  for the gateway↔relay hop, the **egress allow-list home** (webhooks §4.4 + delegation-out §8), the
  webhook-credential encryption keys, and the **P-attach-gate** home (§4.2).
- **agentctl RFC 0016** — CLI & kubectl-plugin grammar: `kubectl agents tasks` / `kubectl agent card`
  as cold/A2A reads off this RFC's store + projection.

**Contract spec (the reference implementation's current home — agentd RFCs)**

- **agentd RFC 0020 (the reference impl's contract spec)** — A2A over the substrate: A2A served over
  vsock/unix (§2), the gateway as a dumb HTTP↔substrate bridge + PEP (§1/§2), manifest = Agent Card
  (§3), run/subagent = Task + the `a2a.*` method mapping (§4/§5), status-level same-id streaming +
  `statusUpdate.final` + distillate-only (§5/§6), the delegation-out backend + `--a2a-peer` (§3,
  **P-a2a-out**), gateway-held `tasks/list`/history/push-notification (§4/§7), descriptive
  caller/tenant `_meta` (§5, **P-meta**), stateless-agent / stateful-gateway re-drive (§6).
- **agentd RFC 0015 (the reference impl's contract spec)** — management & control surface: the
  manifest = Agent Card source (§5.2), `surfaces.a2a` (the **P2** ask), the warm-session
  `subagent.send` that multi-turn / steering maps onto (§4.5), `PeerOrigin::Management` over the
  substrate listener (§3), the descriptive downward-API identity the agent never re-verifies (§6).
- **agentd RFC 0007 (the reference impl's contract spec)** — agentic loop & terminal status: the
  TerminalStatus → A2A Task-state mapping (§3.4 → `completed`/`failed`/`canceled`/`rejected`).
- **agentd RFC 0005/0009 (the reference impl's contract spec)** — self-MCP surface + subagent model:
  `agent://run|subagent/{handle}` (= a Task), the async-subagent machinery a `message/send` starts,
  `subagent.cancel` (= `tasks/cancel`), the distillate (= the final artifact), delivered once.
- **agentd RFC 0011 (the reference impl's contract spec)** — idempotency & exit codes:
  side-effect dedupe by `run_id` (= `task.idempotency_key`, §6.3/§6.5) — re-drive dedupes side
  effects, it does **not** make whole-task re-execution safe.
- **agentd RFC 0016 (the reference impl's contract spec)** — telemetry & lifecycle: the events-ring
  seq/`dropped` semantics the SSE seq (**P-seq**) parallels, the run-report / terminal-distillate
  read window (**P5**).
- **agentd RFC 0010 (the reference impl's contract spec)** — observability/health: W3C trace-context
  on by default, ingest on the self-MCP request + `AGENT_TRACEPARENT` but **not** the A2A surface
  (the **P-trace** gap, §7).
- **agentd RFC 0012 (the reference impl's contract spec)** — security posture: the
  transport-is-the-boundary / no-auth-in-the-agent model (§3.8) that forces all A2A authz into the
  gateway PEP (§4).
- **agentd RFC 0014 (the reference impl's contract spec)** — control-plane contract umbrella:
  primitives-not-policy (§3), `surfaces{}` as the single discovery point (§6.2), version negotiation
  (§6.3), graceful degradation (§8).

**Contract asks raised or cited by this RFC** (agentctl brainstorm §14): **P2** (`surfaces.a2a` =
served A2A version + address, or `false`; + a commitment to specific A2A wire-method strings — §3.4,
§5), **P5** (read-before-exit / short post-terminal linger / terminal-distillate re-read by run
handle — the relay's must-not-miss property + the §6.5 lost-window default), **P-seq** (monotonic
per-frame seq on the A2A streaming response for resumable SSE — §3.3), **P-meta** (descriptive
caller/tenant `_meta` the gateway stamps and the agent records but never re-verifies — §2/§7),
**P-trace** (traceparent ingest on the A2A method surface — §7), **P-a2a-out** (the `--a2a-peer`
flag schema + outbound A2A-over-substrate dial grammar — §8), **P-attach-gate** (the per-tool
Management-profile gate whose A2A-side analogue is the §4.2 multi-turn/steering gate).

*Where this RFC and a contract spec disagree on the wire, the contract wins and this RFC is
corrected; where this RFC identifies a missing or defective primitive (P2 wire strings, P5
distillate re-read, P-seq stream seq, P-meta/P-trace `_meta` ingest, P-a2a-out dial grammar), it
becomes a contract ask — never a leak of cluster logic into the agent, and never auth/HTTP/SSE/
webhooks/persistence pushed into a data-plane binary.*
