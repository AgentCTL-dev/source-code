# agentctl — Architecture Brainstorm

> **Status:** Pre-RFC alignment artifact. This is the document the team tunes
> against BEFORE writing agentctl RFCs. It is deliberately opinionated and
> deliberately long. Where it disagrees with `ideas.md`, this document is the
> newer thinking and `ideas.md` should be revised (see §13). Where it disagrees
> with an agentd RFC on the *wire*, the agentd RFC wins and we file a primitive
> ask (§14).
>
> **Inputs synthesized here:** the agent control-plane contract track (RFCs
> 0014–0020) and the upstream RFCs they extend (0005/0006/0010/0011/0012), the
> MCP 2025-11-25 and A2A v1.0 specs, ten per-dimension designs each with an
> adversarial red-team, and four cross-cutting analyses (vsock feasibility,
> Go-vs-Rust, node-agent SPOF, completeness). The red-team findings are
> integrated inline, not appended — several of them change load-bearing
> decisions, and burying them would defeat the purpose of this document.

---

## 0. How to read this document

- **§1 Thesis + Load-Bearing Decisions** is the part the human must align on
  first. Five decisions gate everything downstream; each has a recommendation
  and a confidence level.
- **§2–§12** walk every element of agentctl. Each section has: the recommended
  design (with strawman snippets), the key alternatives and trade-offs, the
  agent contract touchpoints, the NEW agent primitives the design needs, and
  the open questions. Red-team findings are folded into the recommendation as
  "**Red-team correction:**" callouts.
- **§13** consolidates what the analysis CONFIRMS / REVISES / ADDS versus
  `ideas.md`.
- **§14** is the consolidated agentd RFC ask list (the cross-repo critical path).
- **§15–§17** are the proposed agentctl RFC track, the phased roadmap with an
  explicit MVP cut line, and the top open questions needing a human decision.

---

## 0.6 Decisions locked (2026-06-27)

The human aligned on the four highest-leverage forks. Three of these **override
the analysis's own recommendation** — that is allowed, but each creates a new
precondition or tension recorded here. The RFC track (§15) and roadmap (§16) are
re-cut against these.

**P0 — agentctl depends on the CONTRACT, never on a specific agent (locked
2026-06-27).** The data plane is *any* agent that conforms to the contract — the
capabilities manifest, the management MCP profile, the frozen metrics/exit-code
contract, the config schema, A2A-over-the-substrate, and the downward-API env
convention. **agent is the reference / first implementation, not a dependency.**
Future agents from other vendors may implement the same contract and agentctl
must manage them unchanged. Consequences: (a) the anti-drift mechanism is
**conformance to a published, language-neutral contract + a behavioral
conformance suite**, not a shared Rust type; (b) agentctl codegens its client
from the **contract schemas**, never from a data-plane binary's source; (c) every
RFC is written against "a conformant agent" / "the contract," naming agent only
as the reference implementation in examples. **Open question P0 raises:** the
contract is presently authored inside the agent repo (agentd RFCs 0014–0020). It
should be **extracted into a neutral "Agent Control Contract" spec** (own home +
published JSON Schemas) so neither side owns the other — recommended; see agentctl
RFC 0001/0018.

| Fork | Decision | Vs. analysis | Consequence / new precondition |
|---|---|---|---|
| **Substrate (D1)** | **Stock primary + Kata hardened.** unix-socket-over-hostPath is the PRIMARY substrate; vsock-on-Kata-hybrid is the hardened tier; they converge on "open a discovered socket." | **Matches** | None new — but see the tenancy interaction below: hostile tenancy forces Kata to be *mandatory for tenant workloads*, so "stock primary" is the **dev / single-tenant / bring-up** path, not the multi-tenant production path. |
| **Stack (D2)** | **Rust for all five components (kube-rs), one control-plane ecosystem.** | **Overrides** (analysis said Go) | The **"shared wire crate with agent" rationale is void by P0** — importing a data-plane crate is the forbidden dependency. Rust now stands on **team / single-control-plane-ecosystem fit**, not coupling to agent. **Two hard preconditions:** (1) **CC — the contract is published as language-neutral, machine-readable schemas** (manifest + config JSON Schema, the frozen metrics schema, the management MCP profile, the `a2a.*` method set); agentctl **codegens its own typed client** from these and never links a data-plane crate. Today the reference impl builds the manifest `json!`→`Value` (deliberately, for secret-safety) and `--config-schema` is unbuilt — so CC is real work, but it is **contract work, not agent-coupling**, and it is what makes the contract portable to other agents. (2) The team **owns the kube-rs gaps**: webhook **cert rotation**, thinner **conversion-webhook** tooling, the **KEDA external scaler in tonic/gRPC**, a **Krew/kubectl plugin in Rust**. None blockers; all real work controller-runtime would give free. |
| **Tenancy** | **Hostile multi-tenancy in v1.** Untrusted tenants share the cluster from day one. | **Overrides** (analysis said trusted-tenant for MVP) | **Elevates the whole security plane (§10) into v1** and forces: (a) **Kata hybrid-vsock is mandatory** for any tenant workload (the microVM kernel boundary is the only real isolation — confirmed in D1); the stock-unix tier is then dev/single-tenant only. (b) **Attested pod-UID→socket mapping** must be solved *first* — on hostPath, the node-agent must prove which pod owns `/run/agent/<pod>.sock` and prevent socket-squatting (`SO_PEERCRED`/UID-mapping, or rely on the per-VM uds that Kata isolates naturally). (c) **Per-tool attach/inject gating** (agent ask **P-attach-gate**) so a tenant can `drain`/`cancel`/observe but not *puppet* a neighbour. (d) **Per-tenant A2A gateways + secret isolation.** |
| **v1 scope** | **All planes in v1:** Observe + Scale + A2A gateway/mesh + Intelligence ops/cost. | **Overrides** (analysis proposed a thin "manage what exists" MVP) | v1 is **the full platform, not an MVP**. The cross-repo critical path (§14) becomes the gating risk: v1 now depends on **~12 agent primitives/fixes** (P2, P3, P4, P5, P7, P9, P10, P12, P-meta, P-seq, P-trace, P-cost, P-attach-gate). **agent repo work must lead agentctl work** for each plane. Several asks are *defect fixes* (P3, P4, P10, P12), not features. |

**Two derived requirements this combination forces (new, not in the original §14):**

- **CC (Contract-as-Schema)** — the contract published as language-neutral,
  machine-readable schemas (manifest + config JSON Schema, the frozen metrics
  schema, the management MCP profile, the `a2a.*` methods) that agentctl and *any*
  conformant agent codegen/validate against. Replaces the earlier "agent-wire
  crate" framing, which would have coupled agentctl to agent (forbidden by P0).
  The single most important **contract** precondition for the Rust decision, and
  the thing that makes the contract portable to other agents.
- **AT (attested pod→socket identity)** — promoted from a deferred open item to a **v1-blocking** design problem by the hostile-tenancy decision. On the stock tier this needs a peer-credential attestation scheme; on Kata it is mostly free (per-VM uds).

**One unresolved tension to settle before the substrate RFC (agentctl-0002):** "stock
primary" + "hostile multi-tenancy" partly conflict — true isolation needs the
microVM boundary. The forced resolution is: **Kata is the default for multi-tenant
production; the stock-unix tier is the dev / single-tenant / conformance path.**
Confirm this framing or relax one of the two.

**Verified against agent source (2026-06-27) — refines the AW ask:**

- agent ships **only a TypeScript SDK** (`sdk/typescript/`); crates are just
  `agent` + `agent-conformance`. There is **no Rust wire crate** to import.
- The manifest builder (`crates/agent/src/capabilities.rs::manifest`) returns
  `serde_json::Value` via `json!` **by deliberate design, not laziness**: the
  `Secret` newtype "has no `Serialize`, so it cannot reach this builder" (source
  comment, RFC 0012 §3.7). So **AW is not a trivial `#[derive(Serialize)]`** —
  the wire crate must expose *consumer-facing* types (what agentctl reads) while
  preserving agent's invariant that secrets are structurally unserializable.
  Cleanest shape under P0: the contract publishes **language-neutral JSON
  Schemas** (not a Rust crate agentctl imports); agentctl **generates its own**
  typed client from them, and the reference agent maps its internal structs onto
  the same schema (with `Secret` structurally absent). This keeps the contract
  portable to other agents and is the **single most important contract
  precondition** (CC) for the Rust decision.
- `OPERATOR_TOOLS = ["drain", "lame-duck", "cancel"]` — **`pause`/`resume` are
  NOT implemented** (deferred: need a `ctrl/pause` control message + loop
  turn-boundary suspension). `ideas.md §3.1` and the operator MCP profile assume
  them ⇒ add **P-pause** to §14 (new agent ask) or drop pause/resume from v1.
- `CONTRACT_VERSION = "1.0"`; agent refuses an instance whose *major* it does
  not understand (RFC 0014 §6.3). The negotiation spine is real and shipped.

---

## 1. Thesis and the load-bearing decisions

### 1.1 Thesis

`agent` is the **data plane**: a single static ~1.1 MB Rust binary that runs the
bounded agentic loop, MCP-native, no async runtime, no HTTP server, no cluster
coupling. It exposes *primitives* over its existing transports and refuses to
learn Kubernetes. `agentctl` is the **control plane**: everything
Kubernetes-shaped — CRDs, operator, admission, node-agent bridge, A2A gateway,
scaling, observability, security policy, CLI. The asymmetry —
**agent exposes primitives; agentctl owns policy** — is the entire architecture
(RFC 0014 §3). The rule we never break: if agentctl needs something, it asks for
a *primitive* in an agentd RFC; the cluster-facing translation lives here.

That contract is real and largely frozen. But the cross-cutting analyses surface
a sharper, more uncomfortable truth that the rest of this document is organized
around:

> **The distinctive part of the vision — "vsock-everything, no cluster network"
> — is a premium isolation tier that runs only on microVM substrates, not the
> default substrate, and several of the proposals quietly build the whole system
> on capabilities agent does not yet expose.** The honest design is a tiered
> substrate with a portable default, a node-agent split into a bounce-safe
> control tier and an HA data-path tier, and a small, prioritized set of agent
> primitive asks that gate specific milestones.

### 1.2 The five load-bearing decisions

| # | Decision | Recommendation | Confidence |
|---|---|---|---|
| **D1** | vsock-everything as the substrate | **Demote to an optional hardened tier.** Make transport a pluggable abstraction; ship a **unix-socket-over-hostPath-to-host-DaemonSet** tier as the PRIMARY/MVP substrate (runs on stock runc/containerd), and **vsock-on-Kata-hybrid (Firecracker/Cloud-Hypervisor)** as the premium isolation tier. The two converge in the node-agent ("open a discovered socket"). | High |
| **D2** | Go vs Rust | **DECISION (2026-06-27): Rust, all five, one ecosystem with agent** — overriding the analysis below, which recommended Go. The override is sound *only with* the two preconditions in §0.6 (agent ships `agent-wire` + `--config-schema`; team owns the kube-rs gaps). The Go analysis is retained below as the record of what Rust must compensate for. | (overridden) |
| **D3** | node-agent decomposition | **Split into two tiers, not one and not four.** Tier A (control + telemetry) is bounce-safe and can roll freely. Tier B (A2A data path + a minimal node-pinned vsock relay + a separate replicated HTTP gateway) is HA with its own cadence. Keep the intelligence proxy OUT of both. | High |
| **D4** | A2A task-store location | **Shared durable store + node-local live bridge.** A shared store (Postgres or pluggable) holds task records, status history, the webhook registry, and the ListTasks index; the per-node relay holds only the ephemeral live vsock stream. Reject pure-per-node (fails ListTasks + durability) and CRD/etcd (churn anti-pattern). | Medium-High |
| **D5** | operator ↔ node-agent path | **Split by caller, but fix the identity hole.** Operator → node-agent over mTLS for autonomous/high-volume traffic; humans reach management verbs through an **aggregated APIServer** (not raw `pods/proxy`) so per-verb RBAC and end-user identity survive. Raw `pods/proxy` is a single-tenant/admin-only stopgap. | Medium |

The rest of this section argues each. The component sections (§2–§12) depend on
these being settled.

---

### D1 — vsock-everything is a tier, not the substrate

**Recommendation: demote vsock-everything from THE load-bearing default to an
OPTIONAL HARDENED PROFILE; make transport pluggable; ship a portable default.**

The vsock-feasibility analysis is unambiguous and changes the strategy:

- **vsock guest↔host requires a real VM boundary.** A stock runc/containerd
  container shares the host kernel, gets no guest CID, and has no vhost-vsock
  device. The only same-kernel vsock is `VMADDR_CID_LOCAL=1` loopback, which is
  kernel-global and useless as a per-pod bridge. So vsock-everything presupposes
  a microVM runtime (Kata-QEMU/Firecracker/Cloud-Hypervisor, or KubeVirt).
- **It is unavailable on the dominant managed tiers.** GKE Sandbox is gVisor
  (no guest↔host AF_VSOCK at all — runsc is a userspace syscall interceptor, not
  a VM); EKS/Fargate does not expose vsock; only AKS Pod Sandboxing or
  self-managed node pools with Kata installed qualify. Making vsock the **MVP
  day-one substrate** (as the roadmap in `ideas.md §6` does) stakes adoption on a
  substrate a minority of clusters have.
- **The contract already makes the alternative clean.** RFC 0015 §3.2/§3.4 set
  `PeerOrigin::Management` identically for unix and vsock, with identical NDJSON
  framing, and `surfaces.management` reports `false | "vsock:PORT" | "unix:PATH"`.
  Crucially, on **hybrid-vsock** the host side is *already a per-VM Unix-domain
  socket* (`uds_path`), not a host CID — so in BOTH the Kata-hybrid tier and the
  stock-runc tier, the node-agent's job is identical: open a unix socket at a
  discovered hostPath. Only QEMU real-vhost-vsock requires actual AF_VSOCK on the
  host (and inherits the host-global CID allocation problem, which is `ideas.md`'s
  hardest open item).

**Recommended tiering (one control-plane object, substrate-specific realization):**

1. **PRIMARY / MVP — unix-socket on a hostPath, bridged to the host DaemonSet.**
   Runs on any stock runc/containerd cluster permitting hostPath. agent serves
   `--serve-mcp unix:/run/agent/<pod>.sock` on a shared volume; the host
   DaemonSet reaches it. Bridge is off-pod (agent holds no secrets, the
   DaemonSet is the only networked component), and there is **no CID-allocation
   problem** — it is a filesystem path with filesystem ACLs as access control,
   the exact unix trust model RFC 0012 §3.8 already documents.
2. **HARDENED — vsock on Kata hybrid-vsock (Firecracker / Cloud-Hypervisor).**
   Real VM kernel isolation; same node-agent "open discovered socket" code path
   (per-VM uds). Explicitly **reject QEMU real-vhost-vsock** for v1 (host-global
   CID allocation/collision is intractable; Linux 7.0 netns local-mode still
   cannot reach the host).
3. **MOST-PORTABLE — per-pod sidecar over an emptyDir unix socket** where
   hostPath is forbidden (Autopilot/Fargate/PSS-restricted). Weakest isolation:
   **note honestly that pod containers share the netns, so a networked sidecar
   means agent is NOT network-isolated** — only off-pod bridge variants remove
   reachability.

**Two honesty corrections to carry forward:**

- **"No network = nothing to exfiltrate" is false.** The intelligence/model
  channel is an irreducible egress leg and, in the lethal-trifecta model, the
  dangerous one; guest→host vsock is itself a live egress channel. The real
  isolation win is the **microVM kernel boundary** (orthogonal to vsock) plus
  the agent-side reader/actor distillate firewall (RFC 0012 §3.3). Market that,
  not "nothing to attack."
- **Probes break on any networkless pod.** kubelet httpGet/tcpSocket/grpc probes
  dial INTO the pod network; a networkless pod has nothing to dial. Only exec
  probes traverse CRI. agent ships `--health-file` (RFC 0010 §3.7) as the
  default exec surface, but a scratch image has no shell/`cat` to read it. This
  needs an agent primitive (§14, P1) and is a blocking dependency for the
  isolation tiers — not a vsock detail.

**Alternatives considered:** keep vsock as the only substrate (rejected:
undeployable on most clusters); pure NetworkPolicy default-deny to fake "no
network" (rejected: NetworkPolicy is IP-layer and CNI-dependent, it does not
remove the netns and does nothing against vsock-to-host — true no-network is a
microVM property).

---

### D2 — Go for all five components

**Recommendation: build the operator, node-agent, A2A gateway, CLI/kubectl
plugin, and KEDA scaler all in Go, one monorepo.** This is a decisive call, not
a coin-flip.

The single strongest pro-Rust argument is "a shared typed wire crate makes
agentctl and agent unable to drift." The Go-vs-Rust analysis **verified this is
illusory against agent's actual source**:

- `crates/agent/src/capabilities.rs` builds the manifest with the `serde_json::json!`
  macro into an untyped `Value` — there is **no `derive(Serialize)` struct to
  share**, no `schemars`, no `schemas/` dir. The only machine-readable schema the
  contract even promises (`agent --config-schema`, RFC 0017) is **not
  implemented yet**.
- The contract is runtime-negotiated and **additive-by-minor by design**
  (RFC 0014 §6.3): agentctl must refuse only on unknown major and *tolerate
  unknown additive fields/tools/metrics*. A shared Rust crate with
  `deny_unknown_fields` would break that; making it lenient dissolves the
  "can't drift" guarantee. Drift is handled by **version negotiation + graceful
  degradation + behavioral conformance**, which is language-independent.
- The most decision-critical block, `surfaces{}`, is JSON sum types
  (`bool|string`, `bool|object`) that defeat typed codegen in *any* language and
  need hand-written unmarshalers regardless.
- agent's own blessed anti-drift pattern is the **black-box conformance suite**
  (`crates/agent-conformance`, "never linking the agent library"), and the only
  cross-language artifact it ships is a **TypeScript** SDK, not Rust.

Per-component scorecard: operator — **Go decisive** (controller-runtime is the
reference impl; kube-rs is CNCF Sandbox); admission webhook — **Go decisive**
(controller-runtime ships a webhook server with automatic cert rotation; kube-rs
hand-wires it); node-agent vsock bridge — **Go clear** (`mdlayher/vsock` is
mature, used by Firecracker tooling; goroutine-per-conn maps onto agent's own
thread-per-connection serving); A2A gateway PEP — **Go clear** (mature
server-side OAuth2/OIDC); KEDA scaler — **tie** (pick Go for consistency); CLI —
**Go slight** (client-go/cli-runtime + Krew idiom). There is no component Rust
wins.

**Revisit triggers (record these so we do not relitigate):** flip to all-Rust
only if ALL of (a) the team already has production kube-rs + tokio fluency, (b)
agent commits to and ships a published `agent-wire` crate
(`json!`→`derive(Serialize)`+schemars, `--config-schema` implemented), and (c)
Agent/AgentFleet CRDs are committed single-version-forever. Choose hybrid (Rust
node-agent only) only if (b) holds AND frame-handling becomes the dominant
complexity. The unstated input that could move this is **team kube-rs fluency** —
name it explicitly before locking.

---

### D3 — Split the node-agent into a control tier and a data-path tier

**Recommendation: two tiers, not one monolith and not four micro-DaemonSets.**

`ideas.md §3.3` folds management bridge + A2A gateway + telemetry collector
(+ a proposed intelligence proxy) into one "three thin roles" DaemonSet. Three
separate red-teams independently flagged this as a serious blast-radius and
SPOF error:

- The keystone safety claim — "the bridge is NOT in the data path; node-agent
  crash = zero data-plane impact" — is **true only for management + telemetry**
  (RFC 0015 §8: a dropped management connection is not a liveness signal,
  reconnect is a clean re-read). It is **false for A2A** (the gateway is the only
  network ingress/egress for agent-to-agent work; a crash drops all node-local
  A2A) and **false for an embedded intelligence proxy** (the agentic loop blocks
  on the LLM call, so a proxy crash stalls reasoning on every pod on the node).
- A single DaemonSet couples four incompatible reliability classes; one OOM or a
  panic parsing one malformed external A2A frame takes down management + model +
  telemetry for the whole node. The same DaemonSet rollout cadence is dictated by
  the union of all roles' churn at the blast radius of the most critical role.
- It concentrates **every node-local secret** (provider tokens, webhook tokens,
  any card-signing key, store creds) plus **god-mode over every local pod**
  (drain/cancel/inject with no in-band auth) into the one process that also
  terminates untrusted external A2A input.

**Recommended decomposition (vsock node-locality forces on-node, NOT
one-process — agent's listener is thread-per-connection, so independent host
processes can each open their own vsock connection to each local pod):**

- **Tier A — control + telemetry DaemonSet.** Management bridge + telemetry
  collector. Privileged for socket/Kata discovery; bounce-safe; rolls freely
  (agent survives, RFC 0015 §8 guarantees clean re-read on reconnect); a crash
  is a *control gap only*. The "crash = zero data-plane impact" invariant is
  written down as scoped to THIS tier.
- **Tier B — A2A data path.** A minimal node-pinned vsock relay that owns the
  task stream for the full lifecycle and writes status to the shared store
  (D4), PLUS a **separate, replicated, stateless HTTP gateway** fronted by the
  cluster A2A Service, reading the shared store, with its own PDB / surge /
  security review / rollout cadence. This makes the A2A surface HA and gives the
  untrusted-input parser its own (less privileged) failure domain.
- **Intelligence proxy: keep it OUT of both tiers** (see D-adjacent, §6). Prefer
  a per-pod TLS-terminating sidecar (RFC 0006 `unix:/run/intel.sock`) or a
  separate node-local Deployment; always keep a fallback endpoint in agent's
  RFC 0018 list that does NOT traverse the proxy, so the proxy is never a hard
  inference SPOF.

**Alternatives:** one fat DaemonSet (rejected: blast radius/SPOF/cadence
coupling above); four separate DaemonSets (rejected: maximizes sprawl and
multiplies the socket/CID discovery burden). Two tiers is the seam that makes the
safety invariant *true by construction* for the tier it is claimed of.

---

### D4 — Shared durable store + node-local live bridge for A2A tasks

**Recommendation: a shared backing store (Postgres, pluggable, embedded-sqlite
dev fallback) for the durable task record + status history + webhook registry +
ListTasks index; the per-node relay holds only the ephemeral live vsock stream.**

Three contract requirements force a shared store and cannot be satisfied
per-node: (a) `ListTasks` for an AgentFleet aggregates across many pods on many
nodes; (b) durability must survive node/pod loss (agent is stateless,
distillate-only — RFC 0020 §6); (c) the webhook registry must survive the task's
node and fire even if the owning pod reschedules. The live A2A stream is
physically pinned to the node where the agent pod runs (vsock is point-to-point),
so the relay is on-node; `taskId` resolves (via the store) to its owning
pod/node, and a request landing on the "wrong" gateway answers terminal tasks
from the store and proxies live ops to the owner.

**Red-team corrections to bake in from day one:**

- **The relay must own the vsock task stream for the full lifecycle independent
  of any HTTP client.** agent delivers the distillate **exactly once** as a
  status notification and serves only LIVE tasks; if the relay is not draining at
  completion, the final artifact is gone — worst for `once`-mode agents that exit
  immediately. This makes the "ephemeral relay" actually a durable, must-not-miss
  consumer. We need an agent primitive to **re-read a terminal distillate by run
  handle after a short post-terminal linger** (§14, P5), or define the contract
  as FAILED + idempotent re-drive.
- **Re-drive is not free.** RFC 0011 §6 idempotency dedupes MCP-backing-service
  side effects by `run_id`; it does NOT make whole-task re-execution safe for
  non-idempotent compositions. Default on owner loss = **FAIL the task + final
  webhook**; gate re-drive behind an explicit per-fleet opt-in that asserts the
  composition is idempotent.
- **Encrypt webhook creds + apply SSRF controls to webhook delivery.** The store
  must not hold plaintext push tokens (the same reason etcd was rejected), and
  webhook delivery POSTs to client-supplied URLs from the one networked
  component — apply metadata-endpoint blocking / allow-listing exactly as for
  delegation-out.
- **Rate-limit/quota state must be shared too**, or a client load-balanced
  across N gateways gets N× the limit. Either a shared counter store or
  consistent ingress routing.

**Alternatives:** pure per-node store (rejected: fails ListTasks + durability +
webhook survival); a Task CRD in etcd (rejected: task/status-event churn is an
etcd anti-pattern and creds do not belong in etcd); an MCP backing service for
the store (rejected: reinvents a relational store behind an extra hop).

---

### D5 — Operator ↔ node-agent: split by caller, fix the identity hole

**Recommendation: operator → node-agent over mTLS for autonomous/high-volume
traffic; humans reach management verbs through an aggregated APIServer that
preserves per-verb RBAC and end-user identity. Raw `pods/proxy` is a
single-tenant/admin-only stopgap.**

The proposals' attractive idea — model `drain`/`attach`/`cancel` as
Agent subresources governed by standard RBAC, with the node-agent re-checking via
SubjectAccessReview — was red-teamed hard and is **not implementable as stated on
a CRD**:

- A CRD supports exactly two subresources (`/status`, `/scale`). `pods/exec`-style
  connect subresources require an **extension/aggregated API server**; you cannot
  register arbitrary connect verbs on a CRD. RBAC rules naming `agents/drain` will
  *apply* (RBAC is string-matching) but no request path reaches them — dead policy.
- The kube-apiserver pod/service proxy **does not forward end-user identity** to
  the backend, so a node-agent SubjectAccessReview "can alice drain" is
  unanswerable; and a single shared listener cannot both require operator client
  certs and accept the certless apiserver proxy.
- `pods/proxy` is also coarse: it grants reach to ANY port/path on the target pod,
  so it bypasses the per-verb gate and (because the node-agent multiplexes every
  namespace's pods) becomes an all-tenants master key for destructive ops.
- Bidirectional streaming (`logs -f`, `attach`) through generic `pods/proxy` to a
  non-kubelet backend is unproven.

**Therefore:** the human path that needs per-verb RBAC, identity, and clean
streaming is the **aggregated APIServer** (it dials the backend with proper
upgrade handling and receives the authenticated user via the standard delegation
chain). The autonomous operator path is a single mTLS client identity, on a
distinct listener, whose latency is decoupled from data-path throughput. v1 may
ship raw `pods/proxy` ONLY for single-tenant/admin clusters, with an explicit
note that it is coarse and unattributable. The node-agent must additionally
enforce per-target-namespace authorization internally regardless of path.

**Alternatives:** node-agent does its own RBAC (rejected: duplicates Kubernetes
authz in a privileged host component, no `kubectl auth can-i`); CLI → node-agent
direct via NodePort/LB (rejected: new auth model, bypasses audit, exposes a
privileged host bridge on the network — the thing the isolation posture exists to
remove).

---

## 2. CRDs / API model

### 2.1 Recommended design

Ship a small CRD set with a clear ops/dev ownership seam, but resolve the
versioning and validation correctness errors the red-team caught.

**CRDs (group `agentctl.io`):**

- **`Agent`** (namespaced) — one logical agent. Renders by `mode` to
  Job/CronJob/Deployment/StatefulSet. Carries instruction, config (rendered to a
  ConfigMap and validated), intelligence binding, MCP servers + trifecta tags,
  limits, drain/grace, surfaces to expose, security gates
  (`enableExec`/`allowTrifecta`/`attachPolicy`), and a rich `status` mirroring a
  curated projection of the manifest + health + conditions.
- **`AgentFleet`** (namespaced) — reactive scale-out. Embeds an `Agent` template
  forced to `mode: reactive`, plus the RFC 0019 claim/shard/standby + autoscaling
  policy. Renders to StatefulSet (shard mode) or Deployment (claim mode) + a KEDA
  ScaledObject.
- **`AgentClass`** (cluster-scoped, StorageClass-shaped) — the ops bundle: image,
  `contractVersionRange`, `requiredBuildFeatures`, substrate
  (`runtimeClassName`, networkIsolation tier), default limits/drain/grace,
  default intelligence binding. This is where the substrate (D1) and
  contract-version policy live once.
- **`IntelligenceService` / `ModelPool`** (namespaced) — the RFC 0018 ordered
  endpoint list as an ops-owned, referenced object so a backend move is a
  one-object edit (see §6).
- **`MCPServerSet`** (namespaced, optional) — reusable MCP-server bundles + tags;
  an Agent may inline servers OR reference sets (refs+inline ADD, mirroring
  RFC 0017's `--mcp` deviation).

Defer **`A2AGateway`** to the A2A milestone (it is blocked on a `surfaces.a2a`
primitive anyway — §14, P2). Reject `ClaimService`/`AgentMesh` as CRDs for v1
(coordination is BYO/referenced MCP; federation is post-v1).

```yaml
apiVersion: agentctl.io/v1alpha1
kind: Agent
metadata: { name: triage, namespace: agents }
spec:
  classRef: standard-substrate        # AgentClass; spec fields below override class defaults
  mode: reactive                      # once | loop | reactive | schedule  (CEL enum, immutable)
  instruction: { configMapRef: { name: triage-instruction } }  # XOR inline
  model: claude-opus-4
  intelligenceRef: { name: anthropic-pool }
  mcp:
    serverSetRefs: [core-tools]
    servers: []                       # inline additions
  subscribe: ["fs:file:///watch/inbox"]   # required iff reactive (CEL)
  limits: { maxSteps: 200 }           # sparse override of class/manifest limits
  drain: { timeoutSeconds: 25 }
  podGraceSeconds: 30                  # CEL: drain.timeoutSeconds < podGraceSeconds
  surfaces:
    management: { enabled: true }     # address assigned by node-agent per substrate, NOT hardcoded
    metrics:    { enabled: true }
    events:     { enabled: true }
  security: { allowTrifecta: false, enableExec: false }
  attachPolicy: deny                  # opt-in per agent
status:
  observedGeneration: 4
  phase: Running
  contract: { version: "1.0", agentVersion: "1.0.0", compatible: true }
  buildFeatures: [serve-mcp, vsock, metrics, events, hot-reload]
  manifest:                           # CURATED projection (not the churny full doc)
    mode: reactive
    surfacesAdvertised: { management: "unix:/run/agent/mgmt.sock", metrics: ":9090", events: true }
  identity: { runId: "01J...", instance: triage-abc, uid: f3c1..., node: node-3 }
  health: { ready: true, draining: false, paused: false, lameDuck: false }
  conditions:
    - { type: ContractCompatible, status: "True", reason: MajorMatch }
    - { type: ConfigValidated,    status: "True", reason: SchemaValidated }
    - { type: ManagementReachable,status: "True", reason: SocketConnected }
    - { type: Ready,              status: "True", reason: SubscriptionsReconciled }
```

### 2.2 Red-team corrections (these change the design)

1. **CRD versioning: do NOT promise a `v1alpha1 → v1beta1 → v1` additive ladder
   under `conversion: None`.** That is a Kubernetes correctness error — with
   `conversion: None` the apiserver serves the stored bytes for every served
   version, and structural-schema pruning silently drops fields not in the
   requested version's schema. **Commit to a single served version per major and
   bump only with a conversion webhook + `StorageVersionMigration`.** Ship
   `v1alpha1` single-served, absorb churn additively *within* `v1alpha1`, and
   keep the **agent `contract_version` negotiation in `AgentClass`/`status` —
   decoupled from the CRD `apiVersion`** (they version independently).
2. **"CEL, no webhook" is false advertising — a validating webhook (or operator
   pre-render) is mandatory.** At least three checks cannot be CEL: cross-object
   trifecta union, MCP server-name collision across `serverSetRefs`+inline, and
   `agent --validate-config`. Be honest that the webhook is a first-class
   component from day one. Reserve CEL for cheap single-object invariants
   (`mode↔subscribe`, `drain<grace`, `claim XOR shard`, immutability).
3. **Trifecta is an Agent-level decision, not per-MCPServerSet.** Two
   individually-"safe" sets can compose the full lethal trifecta on one Agent.
   Compute and gate the union at the Agent (inline + all referenced sets vs
   `allowTrifecta`) in the webhook/pre-render. **More importantly:** agent
   already enforces Rule-of-Two *per subagent at the spawn chokepoint* over each
   child's narrowed grant (RFC 0012 §3.2), and the canonical SAFE pattern is a
   reader/actor split across three servers. A naive Agent-level union *refuses
   the safe pattern* and trains operators to flip `allowTrifecta` routinely.
   **Make the admission trifecta check advisory/observational, not blocking;
   gate the `allowTrifecta` override (elevated RBAC + audit) instead.**
4. **`mode` immutability rationale is "the workload Kind changes by mode"**
   (Job vs Deployment vs StatefulSet — you cannot mutate one into another), NOT
   RFC 0017's restart-only partition (restart-only ≠ recreate-only).
5. **Do not ship `surfaces.a2a` or a hardcoded vsock address in `v1alpha1`.**
   `surfaces.a2a` is not in the frozen manifest (§14, P2); the management/metrics
   addresses are substrate-assigned per-pod by the node-agent (CID allocation is
   unsolved), not literals in the CR.
6. **Keep churny telemetry OUT of `status`.** `inventory.active`,
   `tokensIn/Out`, `configGeneration`, fleet `backlog` in `status` cause etcd
   write-amplification and watch storms. Mirror only stable/structural facts +
   conditions; serve counts/tokens via metrics and on-demand `kubectl agent
   describe`. Replace `inUseBy: int` refcounts with finalizer logic that
   enumerates actual referrers.

### 2.3 Mode → workload (resolve the double-processing + double-schedule traps)

| mode | workload | notes |
|---|---|---|
| `once` | Job (`restartPolicy: Never`) | + mechanically compiled `podFailurePolicy` |
| `schedule` | **CronJob of `mode=once` Jobs** | Pick ONE clock: k8s CronJob → agent `once` per fire. Do NOT also run agent's internal cron (RFC 0008 makes them mutually exclusive). |
| `loop` | Deployment (replicas:1) or Job-with-deadline | give `loop` an explicit render target |
| `reactive` singleton | Deployment `strategy: Recreate` (or single-replica StatefulSet) | **Red-team: default RollingUpdate maxSurge briefly runs two reactive pods on the same source → double-processing (RFC 0019).** Use Recreate/at-most-one, or require `disposition: claim`. |
| `AgentFleet` | StatefulSet (shard) / Deployment (claim) + KEDA | see §5 |

### 2.4 agent contract touchpoints
RFC 0014 §5/§6 (manifest spine, `surfaces{}`, contract_version, downward-API
env); RFC 0015 §5.2–5.4 (manifest/inventory/status projected into status);
RFC 0017 §3 (ConfigFile render target, reloadable-vs-restart partition);
RFC 0018 (intelligence binding); RFC 0019 §3/§6 (claim/shard/standby);
RFC 0011 §5 / RFC 0016 §5 (exit-code → podFailurePolicy; drain<grace);
RFC 0012 §3.1–3.2 (trifecta tags, allow-trifecta, exec).

### 2.5 New agent primitives needed
`surfaces.a2a` manifest key (P2); per-endpoint model arrays in the manifest (P7);
ratification of the downward-API env family incl. the `AGENT_SHARD` defect (P3);
the exec-probe health verb (P1).

### 2.6 Open questions
Embedded `Agent` template inside `AgentFleet` vs `AgentFleet` owning real `Agent`
objects (lean embedded; lose per-replica `Agent.status` — synthesize via
node-agent); curated-projection vs full-manifest in status (lean curated);
contract_version negotiation home if two AgentClasses pin different ranges;
deep-merge vs replace semantics for sparse `limits`/`surfaces` overrides.

---

## 3. Operator + admission

### 3.1 Recommended design

One leader-elected operator binary, shared manager, two controllers
(`AgentReconciler`, `AgentFleetReconciler`) sharing informer caches. Strictly
level-triggered and idempotent: derive desired state from `spec` + observed
cluster + the agent manifest, never from the triggering event; write `.status`
only on a `DeepEqual` change; guard against status hot-loops.

The load-bearing constraint: **the operator cannot speak vsock/unix-to-pod** —
only the node-agent is socket-adjacent. So capability knowledge enters via two
distinct paths kept separate:

1. **STATIC, pre-pod (rendering + admission):** resolve `agent --capabilities`
   and `agent --config-schema` from the **image digest** via a `CapabilityProbe`
   one-shot Job run once per unseen digest, cached. **Red-team correction:** cache
   by `(digest + cargo feature-set)` and store ONLY digest-stable facts
   (`build_features`, `contract_version`, config-schema, the *build-gated* surface
   key set). The per-CR surface *values* (management address, mode, model,
   limits) are config-driven — derive them from the rendered config, not the
   probe.
2. **LIVE, runtime (status reflection):** the node-agent reads
   `agent://capabilities`/`status`/`inventory` and publishes an observed
   snapshot. Keep the operator single-writer on `.status`.

### 3.2 Red-team corrections (these are load-bearing bugs)

- **Live-status edge trigger:** the operator does NOT `Owns(Pod)` — a Pod's
  controller ownerRef points to its ReplicaSet/Job/StatefulSet, not the Agent CR.
  Wake reconcile by **labeling managed pods and enqueuing via
  `EnqueueRequestsFromMapFunc` on the label**, or introduce a watchable
  `AgentInstance`/EndpointSlice the node-agent owns. Do NOT rely on owner-ref
  traversal.
- **Do not put `GenerationChangedPredicate` as a controller-wide filter.** It
  suppresses the pod-annotation edge trigger (annotations don't bump generation)
  AND delays finalizer/deletion handling (deletionTimestamp doesn't bump
  generation). Scope it to the `For(&Agent{})` primary source only; guard
  status hot-loops with `DeepEqual`-before-patch.
- **ConfigMap naming vs hot-reload are mutually exclusive as drawn.** A
  content-hashed immutable ConfigMap name is never re-projected into a running
  pod, so inotify/SIGHUP hot-reload can't fire. **Partition the rendered config
  into two ConfigMaps:** a *stable-named* one for the reloadable partition
  (mutated in place → inotify/SIGHUP works) and a content-hashed immutable one
  for the restart-only partition (remount-via-template-change is the intended
  rollout).
- **Webhook fail-closed must not block the operator's own finalizer writes.**
  A `failurePolicy: Fail` webhook on `agents` intercepts the operator's
  finalizer add/remove (UPDATE on `agents`), so a webhook outage strands every
  Agent in `Terminating`. Exempt the operator ServiceAccount via a `matchCondition`
  and never let finalizer removal depend on the webhook.
- **Per-pod drain on scale-down is the POD's SIGTERM path, not a CR finalizer.**
  A CR finalizer fires only on CR deletion; KEDA deleting individual replica pods
  must rely on the pod's `terminationGracePeriodSeconds` + SIGTERM drain
  (RFC 0019 §6 claim-release). Document the two distinct drain paths.
- **podFailurePolicy: an unmatched exit code COUNTS toward backoffLimit and IS
  retried.** There is no "alert-only" action. If 137/143 must not retry, add
  explicit `FailJob` rules (and pair 137 with `onPodConditions: DisruptionTarget`
  since eviction/OOM exit-code matching is unreliable). The intent map:
  `2,5 → FailJob`; `1,4,6 → Count`; `3,7 → Count` (+`--budget-exit-code` remap);
  `124 → Count`; `137/143 → explicit handling + alert`.

### 3.3 Admission validation ladder

CEL/OpenAPI structural (in-apiserver) → in-webhook JSON-Schema check against the
cached `--config-schema` for the image's contract major → **(NOT a fail-closed
binary exec on tenant images)** → an `agent --validate-config` **init-container
running the EXACT target image** as the authoritative ground truth (exit 2 →
CrashLoop fast-fail).

**Red-team corrections:** do NOT shell tenant-specified `agent` images
synchronously inside the webhook (image pull on the apply critical path; running
tenant binaries in the control plane). The operator is Go and cannot link the
Rust crate. Validate against the JSON Schema in the webhook (fetched once per
AgentClass image), and use the init-container for ground truth. Also: admission
cannot see the runtime env layer (downward-API identity, mounted secrets,
`AGENT_SHARD` from the ordinal), so it validates the *file+flag* layers only and
explicitly defers env/identity/shard/secret coherence to the runtime
`ConfigValidated` condition. Both `--config-schema` and `--validate-config` are
unbuilt agent primitives today (§14, P6).

### 3.4 Strawman reconcile flow

```
1. Get Agent; not found → return (GC handled deletion).
2. DeletionTimestamp set → handleFinalizer() → return.
3. Ensure finalizer (patch metadata only; webhook exempts operator SA).
4. digest := resolveImageDigest(spec.image)
5. capFacts := capabilityCache.GetOrProbe(digest, featureSet)   // digest-stable facts only
6. cfgReloadable, cfgRestartOnly := renderConfig(spec)          // two ConfigMaps
7. desired := renderWorkload(spec.mode, spec, capFacts, cfg)    // §2.3 table
   serverSideApply(desired, fieldManager="agentctl", ownerRef=agent)
   // KEDA-managed workloads: OMIT .spec.replicas (HPA owns it)
8. observed := nodeAgentClient.Snapshot(agent)                  // tolerate unreachable
9. newStatus := projectStatus(capFacts, observed, workloadStatus)
   if !DeepEqual(agent.status, newStatus): patchStatus(newStatus)
10. return RequeueAfter: jittered 5–10m   // long backstop; edge trigger does the work
```

Every rendered child (Jobs/CronJobs/Deployments/StatefulSets/ConfigMaps/ScaledObjects,
the capability cache, the probe Jobs) carries the `agent.agentctl.io/managed`
label so the label-restricted informer cache sees them (or `Owns()`/SSA drift
detection silently breaks). Leader election via `coordination.k8s.io` Lease;
webhook HA = ≥3 replicas + PDB + `system-cluster-critical` + cert-manager.

### 3.5 agent touchpoints / primitives / open questions
Touchpoints: RFC 0014 §6.2/§6.3/§6.4; RFC 0017 §4 (validate-config/config-schema);
RFC 0016 §5 (exit codes). Primitives: `--config-schema` + `--validate-config`
(P6); `--config-only` validation mode that tolerates absent env (P6);
ordinal-derived shard (P3); exec health verb (P1). Open: live-status transport
shape (pod-label map-func vs `AgentInstance` object); capability-cache GC/TTL;
webhook contract-major skew policy.

---

## 4. node-agent

### 4.1 Recommended design

Per D3, **two tiers**. This section details the **control + telemetry tier**
(Tier A); the A2A data-path tier (Tier B) is in §5/§7; intelligence is in §6.

Tier A is the host-side bridge: it discovers `pod → sandbox → socket endpoint`
(NOT allocates — Kata already assigns the guest CID for its own kata-agent
channel), holds one long-lived MCP management connection per local agent pod
(over unix-hostPath in the PRIMARY tier, vsock in the hardened tier), caches the
manifest/status/inventory (edge-triggered re-read on `updated`), and serves a
management+telemetry API to the operator (mTLS) and humans (aggregated APIServer
per D5).

```
Tier A node-agent Pod (DaemonSet, control-tier, privileged for discovery)
  ├── discovery     : apiserver watch (spec.nodeName=self) + CRI + sandbox introspection
  ├── conn manager  : 1 long-lived MCP conn per local agent pod (unix-hostPath | vsock)
  │     └── caches manifest/status/inventory; fans out events/inventory streams
  ├── mgmt API      : mTLS (operator) + aggregated-APIServer-backed (humans)
  └── telemetry     : /metrics re-export + agent://events tail → stdout/Loki
```

**Discovery, not allocation** (dissolves `ideas.md §7`'s hardest open item for
the common case): in the unix-hostPath PRIMARY tier the path is operator-assigned
(no CID); in the Kata-hybrid tier the host side is a per-VM `uds_path` and the
problem reduces to `pod-UID → Kata sandbox → uds_path` mapping. **Red-team
correction:** prefer CRI-API discovery (`PodSandboxStatus`/container annotations)
over parsing Kata's internal `/run/vc/.../persist.json` (version-volatile, breaks
on upgrades). Treat CRI-socket access as node-root in the threat model.

**The safety invariant, scoped honestly:** a Tier A crash/restart costs an
observability+control gap on ONE node and **zero data-plane impact** — agent
keeps running (liveness is the supervisor heartbeat, RFC 0010 §3.7, independent of
any management connection), reconnect is a clean re-read (RFC 0015 §8, no
per-connection durable state), and lifecycle still works because `drain` is
reachable via plain SIGTERM (pod delete) when the management drain tool is
unavailable. **The management `drain` tool == SIGTERM == exit 0** (RFC 0015 §4.1)
— it is NOT a "drain-without-delete"; `lame-duck` is the stay-resident NotReady
primitive (RFC 0015 §4.2). Do not conflate them.

### 4.2 Red-team corrections

- **Blast-radius fallback must respect workload semantics.** For a StatefulSet
  fleet, "scale down / delete pod" deletes the highest ordinal or recreates the
  pod — it does not selectively drain a middle ordinal. Specify per-kind
  fallbacks (lame-duck/cordon for selective; deletion-to-drain only where the
  workload won't recreate).
- **Run-report collection is a real responsibility, not an afterthought.** For
  `once`/Job pods the report (`--report-file` / `agent://run/{run_id}`) vanishes
  with the pod; Tier A must capture it live (subscribe at pod-up, read on
  terminal transition) and persist it for `kubectl agents results` (§9). Needs the
  read-before-exit guarantee (§14, P5/P8).
- **NetworkPolicy lock-down is mandatory:** the Tier A management API must be
  reachable only by the apiserver/operator identities; with `hostNetwork:false`
  the pod IP is otherwise reachable by any pod.

### 4.3 Alternatives
Operator-as-sole-proxy (rejected: makes interactive ops depend on operator
liveness); CLI→node-agent direct via NodePort/LB (rejected: new auth model,
bypasses RBAC/audit); per-pod sidecar instead of host DaemonSet (kept as the
non-Kata/restricted-cluster fallback tier, accepting weaker isolation).

### 4.4 agent touchpoints / primitives / open questions
Touchpoints: RFC 0015 §3 (serve-mcp transports, trust domain, PeerOrigin),
§4 (operator tools), §5 (manifest/inventory/status), §8 (reconnect = clean
re-read); RFC 0016 (events ring cursor/dropped, metrics re-export); RFC 0019
(capacity for placement). Primitives: exec health verb (P1); a run-report
read-before-exit guarantee/linger (P5/P8); confirmation that A2A shares the
management vsock listener or gets its own `surfaces.a2a` address (P2). Open:
`pod-UID → sandbox → uds_path` mapping API + minimum privilege; whether Tier A
and Tier B share or each open their own vsock connection; connection-manager
scale envelope on dense nodes.

---

## 5. Scaling plane

### 5.1 Recommended design

Split `AgentFleet` into two scaling regimes via `scaling.mode`, because KEDA
wanting to jiggle `.spec.replicas` continuously and `--shard K/N` requiring N to
be fleet-consistent immutable config (RFC 0019 Decision 4) are irreconcilable in
one topology:

- **`mode: claim` (RECOMMENDED DEFAULT, the only elastic regime).** Every pod is
  shard `0/1`; cross-instance ownership is the work-claim lease alone. KEDA owns
  `.spec.replicas` freely; new pods join the claim race; scale-down pods
  drain+release (step 1.5). Render to a Deployment (fungible) — **reconsider vs
  StatefulSet:** claim-mode genuinely needs no ordinal, and a Deployment +
  `controller.kubernetes.io/pod-deletion-cost` (from per-pod
  `agent_active_subagents`) gives graceful victim selection that a StatefulSet
  (highest-ordinal-first) cannot.
- **`mode: shard` (fixed partition, NOT KEDA-elastic).** N is a deliberate
  operator-chosen partition count; an N-change is an operator-driven
  `shard-resize` rolling restart (drain→release→restart with new K/N), KEDA
  paused/handed-off for that fleet. Render to StatefulSet. Optionally layer claim
  on top for the rebalance seam.

```yaml
spec:
  scaling:
    mode: claim                       # claim | shard  (CEL: exactly one)
    min: 0
    max: 50
    target: { signal: pending_queue_depth, threshold: 5, activationThreshold: 1 }
  work:
    source: { mcp: inbox, uri: "file:///inbox/*.json" }
    claim:  { server: coord, style: tool, ttlSeconds: <budget-aware>, key: item-derived }
    coordinationServer: { managed: true }     # ship a reference atomic-lease server
  drain: { timeoutSeconds: 45 }
  gracePeriodSeconds: 60              # MUST be > drain.timeoutSeconds (CEL)
# Rendered StatefulSet/Deployment: .spec.replicas OMITTED — KEDA's HPA owns it (no fight)
```

### 5.2 Red-team corrections (several are blocking)

- **Scale-from-zero cannot read agent's per-pod metric.** `agent_pending_events`
  is per-replica; at replica=0 no pod emits it. The from-zero/primary signal must
  come from the **coordination server's pending-queue depth**, read by an
  agentctl KEDA external scaler (gRPC, `external-push`/`StreamIsActive`). This
  primitive (an off-pod backlog count) is **unspecified on both sides today** —
  freeze a convention (a `work.stats` tool or countable `work://pending`) before
  shipping claim-mode-with-scale-from-zero (§14, P9).
- **Metric-name drift must be reconciled.** RFC 0016 freezes
  `agent_pending_events`/`agent_reaction_lag_ms`; RFC 0019 §5 uses
  `agent_reactive_backlog`/`agent_saturation`/`agent_tokens_per_sec`/`agent_claims_lost_total`
  and falsely calls them frozen. Author KEDA triggers against the **actually
  frozen** names; treat saturation/backlog as not-real until the umbrella
  reconciles them into ONE set (§14, P10).
- **`AGENT_SHARD=K/N` is unimplementable from one StatefulSet pod template.**
  The template env is identical across ordinals and the downward API cannot
  express a computed composite. This is a **defect in the frozen RFC 0014 §6.4 /
  RFC 0019 §4.2 convention.** Require an agent primitive: `--shard auto/N`
  deriving K from `AGENT_POD_NAME`'s ordinal (P3). Interim: an initContainer
  shim (needs non-scratch).
- **`agent://capacity` and `agent://metrics` are referenced by RFC 0019 but
  defined in neither RFC 0005 nor 0015.** Both need frozen schemas before the
  node-agent vsock refinement path (saturation trigger + victim selection) works
  (§14, P4).
- **Claim transport on a networkless pod is undefined.** Claim is the
  serializing correctness point, but a networkless pod can reach the coordination
  MCP server only via vsock→node-agent proxy or an in-VM sidecar. Pick one
  (vsock→Tier A proxy makes Tier A a per-node correctness dependency in the hot
  path — own that consequence) (§14, P11).
- **Claim TTL must be budget-aware, not a flat 120s,** and the reference
  coordination server must provide **transactional side-effect dedupe on
  `claim_key`**, not merely atomic claim — non-idempotent side effects are unsafe
  otherwise (RFC 0019 §10).
- **Ship the reference coordination MCP server** (the `work.*` names are frozen
  by agentctl, atomicity is the whole correctness story) — but sequence it AFTER
  the umbrella freezes `work.*` + `assign` ownership (RFC 0015 vs 0019, §14, P12),
  not before.

### 5.3 Probes / exit codes
Render **exec** probes for networkless pods (httpGet/grpc cannot reach them);
the probe invokes the agent health verb (P1), not a fabricated `--health-probe`.
Compile `podFailurePolicy` per §3.2 (explicit 137/143 handling). For
`once`/`schedule` fleets, use a KEDA `ScaledJob`.

### 5.4 Open questions
KEDA-vs-operator `.spec.replicas` handoff during shard-resize (pause ScaledObject?
min==max pin?); standby/warm-pool vs scale-to-zero tension (floored
`minReplicaCount` sub-pool?); coordination-server HA/sharding/backpressure and
whether it is per-fleet or cluster-shared; PDB to bound concurrent-eviction claim
loss.

---

## 6. Intelligence plane

### 6.1 Recommended design

A **host-side egress proxy** fronted by an `IntelligenceService`/`ModelPool` CRD,
with a strict two-level division mapping onto the agent/agentctl split:

- **Inside a pool = the proxy's job.** A pool is a set of interchangeable
  backends serving the same model(s). The proxy load-balances across them, holds
  the upstream credential, terminates TLS, translates dialect, runs a per-backend
  circuit breaker. This is deliberate: RFC 0018 §10 says agent never
  load-balances (sticky-primary only), so *something* must.
- **Across pools = agent's job (RFC 0018).** The operator hands each Agent an
  ordered list of pool endpoints → `AGENT_INTELLIGENCE=...,...`; agent's RFC 0018
  failover/breaker/all-down machinery operates across pools.

`ModelPool` keeps provider secrets off the agent pod (keyless dial in the vsock
tier; the proxy injects the upstream key), presents a uniform openai-compatible
face (agent runs `DIALECT=openai` for all pools; the proxy translates), and is
the egress SSRF chokepoint.

### 6.2 Red-team corrections (this dimension was the most over-claimed)

- **DO NOT co-locate the proxy in the node-agent, and do not make it the per-Agent
  failover SPOF.** Both pool endpoints terminating on one per-node process makes
  RFC 0018 inter-pool failover shared-fate, and the agentic loop blocks on the
  LLM call so a proxy crash stalls inference on every local pod. **Prefer a
  per-pod sidecar** (`unix:/run/intel.sock`) or a separate node-local Deployment,
  and **always keep a fallback endpoint that does NOT traverse the proxy.**
- **The "resolves an RFC 0017/0018 contradiction" framing is manufactured** —
  RFC 0017 §5.1 explicitly defers endpoint resilience to RFC 0018, which already
  hot-swaps a repointed endpoint with fresh breaker state and zero restart. Do
  not justify the proxy on a fake contradiction; justify it on the real merits
  (LB across in-cluster replicas, secrets off-pod, dialect translation).
- **"Hard budget enforcement host-side" is false for multi-pod Agents/Fleets** (a
  per-node proxy sees only local tokens; a fleet spends across nodes) and needs
  an unbuilt `EXIT_BUDGET`/back-pressure agent signal (P-cost). **Demote budget
  to best-effort observability + control-loop throttling**, or build a shared
  accounting store (contradicting the thin node-agent).
- **Collapsing each pool to one stable vsock port blinds the frozen RFC 0016
  intel-health metrics** (`agent_intel_endpoint_*` now measure the hop to the
  proxy, not the model). Require the proxy to **re-export per-backend health on
  its own series**, and document that agent's intel metrics in this tier mean
  "pod↔proxy hop."
- **Cost model must be keyed by `{model,type}`** (input vs output pricing differ)
  and account for cache-read/cache-write tiers; pick ONE authoritative token
  source (proxy metering vs `agent_tokens_total`).
- **Don't force `DIALECT=openai` for all pools blindly** — it orphans agent's
  shipped anthropic adapter and re-implements tool-calling translation in a
  second codebase. Either keep per-pool dialect on agent, or own a versioned
  conformance suite for openai↔anthropic round-trips.
- **Per-pod authorization + multi-tenant isolation are unsolved:** a
  per-node-shared listener with cross-namespace secret refs lets a pod spend
  another tenant's credential/budget. Define a `CID→Agent→allowed-pools` authz
  map the proxy enforces, and forbid cross-namespace pool/secret refs without an
  explicit grant.

### 6.3 agent touchpoints / primitives / open questions
Touchpoints: RFC 0006 (transport/wire, keyless dial, traceparent); RFC 0018
(failover/breaker/all-down/hot-swap, `agent://intelligence`, intel metrics);
RFC 0017 (model reloadable, endpoint restart-only, file secret refs); RFC 0016
(token/cost metrics, exit 4 vs budget). Primitives: clean budget-exhausted signal
→ `EXIT_BUDGET`/back-pressure; per-endpoint model arrays in the manifest (P7);
stable endpoint NAMES (`--intelligence-names`) so metric labels survive reorder
(P7); reconcile per-endpoint token env keying (P3). Open: shared vs per-pod proxy
listener; price-table freshness/ownership; streaming-to-buffered de-stream cap.

---

## 7. A2A gateway

### 7.1 Recommended design

Per D3/D4: a **minimal node-pinned vsock relay** (Tier B, owns the live task
stream, writes to the shared store) + a **separate replicated stateless HTTP
gateway** (the PEP: TLS/auth/SSE/webhooks/rate-limit, reads the shared store,
fronted by the cluster A2A Service). The gateway is a transport bridge + PEP, not
a protocol translator: agent serves the live core methods over vsock; the
gateway serves the durable/registry methods (`tasks/list`,
`tasks/pushNotificationConfig/*`, extended card) from the store.

Method split: `message/send`, `message/stream`, `tasks/get` (live),
`tasks/cancel`, `tasks/resubscribe` → relay→agent over vsock (terminal
`tasks/get` short-circuits to the store); `tasks/list`,
`tasks/pushNotificationConfig/*`, `agent/getAuthenticatedExtendedCard` →
gateway-served from the store.

### 7.2 Red-team corrections

- **Resolve the A2A version FIRST, do not present "dumb pass-through" and "pin
  v1.0 outward" as compatible.** RFC 0020 cites 0.2.x method names
  (`/.well-known/agent.json`, PascalCase `a2a.SendMessage`); A2A v1.0 renamed
  several. If agent serves the older names and the gateway serves v1.0, the
  gateway IS a version-translation layer — budget it as such (schema mapping +
  conformance against both versions) or pin agent's served version and refuse
  others. Confirm the exact wire strings agent's `a2a` feature registers (§14, P2).
- **Durability depends on the relay never missing the live distillate** (agent
  delivers it once, once-mode exits immediately). Add the terminal-distillate
  re-read primitive (§14, P5) or define the lost-window contract as FAILED +
  idempotent re-drive (with the re-drive caveats from D4).
- **Security: do NOT replicate node-local secrets cluster-wide.** A shared store
  fronted by per-node relays must scope each node's DB credential to least
  privilege; encrypt webhook tokens at rest; sign Agent Cards **centrally** (not
  with a per-node JWS key on every node — that lets any node compromise forge
  cross-org cards); apply SSRF/egress controls to webhook delivery; treat the
  `tenant` column as an authorization predicate (row-level security), not
  "descriptive."
- **Honesty in the Agent Card:** advertise `capabilities.streaming` as
  STATUS-level only (distillate-only, no incremental artifacts); declare
  `input-required`/`auth-required` interaction support truthfully (agent is
  fire-and-distill; multi-turn is via `contextId`→warm session) — resolve this
  BEFORE publishing a signed cross-org card.
- **Reconciliation for orphaned tasks:** a lease-expiry sweep transitions
  `working` tasks whose owner pod is gone to `failed/lost`; `owner_node`/`owner_pod_uid`
  are caches, not truth.
- **Resumable SSE (`Last-Event-ID`) needs a monotonic per-frame seq agent does
  not emit** (§14, P-seq) and a re-drive epoch so a stale cursor is rejected, not
  mis-resumed. Until then, scope replay to terminal-state reconstruction.

### 7.3 agent touchpoints / primitives / open questions
Touchpoints: RFC 0020 §2/§4/§5; RFC 0015 §5.2 (manifest=card source); RFC 0007
§3.4 (TerminalStatus→TaskState); RFC 0009 (distillate-only); RFC 0011 §6
(RUN_ID idempotency); RFC 0012 §3.8 (gateway = PEP). Primitives: `surfaces.a2a`
(P2); `--a2a-peer` delegation-out flag schema + outbound dial grammar (P-a2a-out);
descriptive caller/tenant `_meta` convention (P-meta); commit to v1.0 wire
strings (P2); monotonic per-frame seq (P-seq). Open: east-west relay mutual auth
+ discovery; card-signing key custody; durable-store tech/HA/DR; transport
breadth (JSON-RPC only vs +gRPC/REST).

---

## 8. CLI + kubectl plugin

### 8.1 Recommended design

One binary, three faces by argv[0]: `agentctl` (standalone), `kubectl-agent`
(singular = one instance), `kubectl-agents` (plural = fleet/list). **Red-team
correction:** ship BOTH `kubectl-agent` and `kubectl-agents` as installed names
(symlinks to one binary) with two Krew manifests — kubectl resolves plugins by
binary name per top-level token, so one binary/one name cannot serve both verbs.

Two data paths: COLD/read (`get`/`describe`/`results`/static `top`) from
`Agent.status` (works when the pod is gone, reuses kubeconfig auth); LIVE
(`tree -w`/`logs -f`/`attach`/`drain`/`lame-duck`/`pause`/`resume`/`cancel`/live
`top`) streaming through D5's path to the node-agent.

```
# fleet
kubectl agents get [-o wide] [-l sel] [-w]
kubectl agents top | results | tasks
# instance
kubectl agent <name> describe | tree [-w] | logs [-f] | top [-w]
kubectl agent <name> drain | lame-duck | pause | resume | cancel <handle>
kubectl agent <name> attach [<handle>] [--read-only] [--steal] [--send "text"]
kubectl agent <name> card
```

### 8.2 Red-team corrections

- **RBAC subresource model is not implementable on a CRD** (D5) — drop the claim
  that `agents/drain` etc. are apiserver-enforced via CRD subresources; use the
  aggregated APIServer for the human path or be explicit that `pods/proxy` is
  coarse/admin-only.
- **`attach` is NOT contract-complete today.** It is `subagent.send` (RFC 0015
  §4.5), but multi-viewer steer-echo, a deterministic free-text inject shape, and
  session-target enumeration all need NEW agent primitives (§14, P-inject,
  P-session). Scope v1 attach to one-shot `--send` + read-only event tail
  (backed today); gate interactive multi-viewer steering behind a contract minor
  bump.
- **`attach` target resolution is unsound as drawn:** `subagent.send` injects
  into a warm CHILD session, addressed by `session_id` off
  `agent://session/{session_id}`, not handle `"0"`/`agent://subagent/{handle}`.
  Resolve handle-vs-session keying before freezing.
- **`results` does not work when the pod is gone unless something persisted the
  report first** (D4/§9). Scope "works when pod is gone" to status reads;
  `results` reads the persisted store.
- **`card`/`tasks` are a THIRD path** (gateway, different auth/durability) — do
  not imply uniform cold/live semantics.

### 8.3 attach multiplexing
Read-many / write-one advisory single-writer lease per `(pod, session)`;
`--steal` transfers (audited), `--read-only` joins as viewer. The lease lives in
agentctl (agent has no session/auth model). The viewer "sees every steer"
guarantee is bounded by the lossy events ring + the missing inject event — emit a
durable audit record on each inject independent of the ring.

### 8.4 Touchpoints / primitives / open questions
Touchpoints: RFC 0015 §5 (manifest/inventory/status), §4 (operator tools),
§4.5 (attach=subagent.send); RFC 0016 §7 (events cursor); RFC 0005 (resources).
Primitives: frozen InjectEvent shape (P-inject); session enumeration +
`steerable` flag (P-session); an inject event in the closed vocabulary (P-inject);
exec health verb (P1). Open: single-writer lease location (in-memory vs Lease CR);
streaming-upgrade through the chosen transport; Krew two-manifest acceptance.

---

## 9. Observability plane

### 9.1 Recommended design

agentctl invents zero telemetry; it transports, relabels, aggregates, and authors
k8s policy against the frozen RFC 0016/0010 contracts. The node-agent (Tier A) is
the single networked bridge for networkless pods.

- **Metrics:** node-agent as a per-pod relabeling scrape PROXY
  (`GET /proxy/<uid>/metrics` → `resources/read("agent://metrics")` →
  byte-identical Prometheus text). Discovery via an operator-central Prometheus
  `http_sd` endpoint (one target per agent pod, `__address__` = node-local
  node-agent). Relabel SD-meta → bounded labels; `metricRelabelings` labeldrop the
  forbidden cardinality keys (`run_id`/`agent_id`/`agent_path`/`call_id`/`uri`).
- **Events → logs:** **use the normal container-stderr → node log agent → Loki
  path** (it already works on networkless pods — CRI captures stderr regardless
  of pod networking). Reserve `agent://events` (lossy ring) for live tail only.
- **Run outcomes:** Tier A subscribes `agent://run/{run_id}` at pod-up, reads on
  the terminal transition while the process is alive, persists to a durable store
  + a curated `Agent.status.lastRun`; `--report-file` emptyDir is the backstop.
- **Exit codes → podFailurePolicy:** mechanical compile (§3.2), gated on
  `surfaces.exit_codes` major.
- **Traces:** the A2A gateway is the trace root (sets `_meta.traceparent` on the
  vsock frame); agent carries `trace_id` through logs/events/report/MCP calls.
  Cross-pod correlation by `trace_id` + span tree, **NOT** `agent_path` prefix
  (agent_path resets to `0` per pod — it is only valid within one pod's tree).

### 9.2 Red-team corrections

- **The "stderr assumes network" premise is FALSE** — container stdout/stderr is
  collected locally by the kubelet/CRI independent of pod networking. So routing
  the *bulk* event stream through the lossy vsock ring is redundant with a path
  that already works and is strictly worse (drops security/limit lines under
  load). Bulk logs = stderr→Loki; ring = live tail only.
- **`agent://metrics` is not in the authoritative resource lists** (RFC 0005/0015
  say no; RFC 0019 says yes) — the whole scrape-proxy is blocked until this is
  reconciled and the body/mimeType pinned (§14, P4). For a networkless pod the
  HTTP `/metrics` addr is unreachable, so vsock is the only path and it is
  currently undefined.
- **`honorLabels: true` is inverted** — it makes scraped pod labels win and
  discards SD identity, a cross-tenant spoofing hole. Use `honorLabels: false`.
- **Do not stamp `model` (a per-series label agent already emits) or `metrics_schema`
  as per-target labels** — collides/churns; multi-endpoint pods serve multiple
  models. Use a `build_info{}`-style series for version hints.
- **Cost rule must split input/output** (key price by `{model,type}`).
- **Per-pod labels on once/Job metrics are a cardinality blowup** — use run
  reports for per-run cost, aggregate metrics for fleets.
- **A2A trace-context ingest is unspecified in RFC 0020/0010 §3.6** — file the
  ingest-on-A2A-frame primitive (P-trace) before claiming gateway-rooted traces.
- **Histogram buckets are unspecified and the histogram name set conflicts
  between RFC 0010 §3.8 (otel-only) and RFC 0016 §4.3** — reconcile + freeze
  buckets before authoring quantile/SLO dashboards (§14, P-hist).

### 9.3 Touchpoints / primitives / open questions
Touchpoints: RFC 0016 §4–7; RFC 0010 §3.2/§3.6/§3.7/§3.8; RFC 0015; RFC 0019.
Primitives: define `agent://metrics` (byte-identical Prom text) + `agent://capacity`
(P4); exec health verb (P1); freeze histogram buckets (P-hist); run-report
read-before-exit/linger (P5/P8); stable endpoint names (P7); A2A trace ingest
(P-trace). Open: per-pod `up` vs vsock-broken disambiguation; run-report history
store (object store vs bounded CRs); SD durability decoupled from operator
liveness; sub-scrape-interval Jobs.

---

## 10. Security and multi-tenancy

### 10.1 Recommended design

The model collapses onto one fact: **vsock/unix reachability == full operator
authority** (RFC 0015 §3.4/§7: the operator profile is gated by `PeerOrigin`,
which is reachability, not a credential). So agentctl owns 100% of authn/authz at
**two PEPs**: the node-agent management bridge and the A2A gateway. agent
re-verifies nothing (RFC 0012 §3.8: the transport is the boundary).

- **Multi-tenancy is a substrate decision, not a NetworkPolicy decision.** Real
  vhost-vsock (QEMU) has a host-global CID space — any host process can dial any
  guest port. **Mandate hybrid-vsock (Firecracker/Cloud-Hypervisor) for
  multi-tenant clusters** so tenancy reduces to filesystem ACLs on the per-VM uds.
  The node-agent's `pod-UID → sandbox → uds` map IS the access-control table.
- **Management authz at the apiserver (D5), not in the node-agent.**
- **Keep every secret out of the agent pod** (keyless intel dial in the vsock
  tier; A2A creds in the gateway). Where a token must be in-pod, it is an
  env/`_FILE` ref to a mounted Secret, never the config file.
- **Rule-of-Two:** surface operator-declared MCP tags in the CRD; gate the
  `allowTrifecta` override behind elevated RBAC + audit (NOT a blocking
  Agent-level union that refuses the safe reader/actor split — §2.2). agent
  enforces the actual per-spawn check.
- **Supply chain:** cosign-verify signed images; pin the wire client to
  `contract_version` and refuse unknown majors.

### 10.2 Red-team corrections

- **`attach`/`inject` is not independently RBAC-enforceable** — `subagent.send`
  is a *work* tool listed to both Stdio and Management peers, and the same
  primitive is reachable via A2A multi-turn through the gateway (a different PEP
  with different policy). If a "no live-puppeting" tier is required for hostile
  tenants, it needs an agent per-tool gate within the Management profile
  (P-attach-gate); otherwise document that reachability = steering.
- **Egress is not closed by "no network":** the model channel is an irreducible
  egress leg; guest→host vsock is a live egress channel (NetworkPolicy governs IP,
  not AF_VSOCK). Add host-side guest→host vsock egress restriction (only
  provisioned ports dialable) to the threat model; the real injection defense is
  the agent reader/actor + distillate firewall.
- **The node-agent is a single high-value trust principal** (CRI socket =
  node-root; holds all node-local secrets; god-mode over local pods). Split the
  untrusted-input-facing A2A PEP from the privileged bridge (D3); model the CRI
  socket as node-root with a dedicated SA + audit; do not market it as
  unprivileged.
- **`agent --validate-config` in the webhook is version-skewed** — run it via
  the EXACT target image (init-container), not a webhook-bundled binary.
- **The MCP servers (not separate Pods) are the real ASI01 surface** — Kyverno
  `verifyImages` cannot verify them; bake them into the verified agent image and
  wire the `mcp.tool.description_changed` rug-pull warn into alerting.

### 10.3 Touchpoints / primitives / open questions
Touchpoints: RFC 0012 §2/§3.1–3.8; RFC 0015 §3.3/§3.4/§6/§7; RFC 0014 §6.3;
RFC 0020 §6; RFC 0017 (secret-key rejection); RFC 0019 (shard immutability).
Primitives: descriptive caller-principal `_meta` + a management-action audit
event (P-meta/P-audit); optional per-tool inject gate (P-attach-gate); per-route
trifecta override (RFC 0012 §6 open item). Open: operator-mediated vs
apiserver-proxy-direct (D5); hybrid-vsock uds mapping + privilege; durable
task-store encryption/per-tenant keys; revocation/kill-switch for compromised
attach grants.

---

## 11. Stack / repo / codegen

### 11.1 Recommended design

Go monorepo (D2). Layout: `cmd/{agentctl,kubectl-agent,kubectl-agents,operator,node-agent}`,
`api/v1alpha1`, `internal/{controller,admission,gateway,bridge,telemetry,scaler,cli}`,
`pkg/agent` (public, `go get`-able client mirroring agent's TS-SDK precedent),
`contracts/` (pinned source-of-truth + generators), `deploy/{helm,kustomize}`,
`test/{conformance,e2e}`.

**Codegen as three tiers** + a black-box conformance suite that drives a real
agent binary over the unix-socket transport (the contract-clean dev fallback):

- Tier 1: `agent --config-schema` → vendor pinned → generate config structs.
- Tier 2: manifest/inventory/status/capacity/operator-tool I/O — hand-author JSON
  Schemas until agent ships schema emitters; conformance is the enforcement.
- Tier 3: metrics + exit codes — checked-in registry → generated constants,
  asserted present by scraping a live `/metrics`.

Pin codegen by `(agent SHA + cargo feature-set + contract major.minor)` — NOT
SHA alone (manifest/surfaces/metrics are `cfg!`-conditional).

### 11.2 Red-team corrections

- **`agent_reactive_backlog` does not exist in the frozen schema** — derive the
  Tier-3 registry by scraping a real agent, not by hand-transcribing names
  (this exact transcription error is the failure codegen is meant to prevent).
- **Be lenient AND typed:** Go `encoding/json` is lenient by default; add an
  explicit additive-drift report (capabilities seen but not driven) since
  conformance only catches *regressive* drift, not additive.
- **`surfaces` unions need hand-written unmarshalers** — scope codegen honestly
  to flat fields.
- **`--config-schema` and `--validate-config` are unbuilt** (P6) — both Tier-1
  codegen and admission are blocked on them; mark explicitly.
- **Dev loop must exercise the risky path:** add a kind+Kata lane on every PR for
  the bridge/transport code, not only the unix fast-loop (which never exercises
  the guest↔host crossing). CID allocation + host-side listener is the project's
  actual hard part.
- **Supply-chain the codegen input:** the binary fetched to emit schemas must be
  signed/hash-pinned.

### 11.3 Touchpoints / primitives / open questions
Touchpoints: RFC 0015 §5.2; RFC 0014 §6.3; RFC 0017 (config-schema/validate-config);
RFC 0016 (metrics/exit codes); RFC 0020 (A2A types + projection); `crates/agent-conformance`.
Primitives: schema emitter for ALL surfaces (P6); implement `--config-schema`
(P6); a published golden-fixture corpus (P-golden); confirm every `surfaces` key
addition bumps contract minor (P-bump); (only if Rust ever chosen) an
`agent-wire` crate. Open: canonical pinning unit; build-vs-download agent in CI;
one binary with argv0 switch vs separate; CRD conversion strategy (§2.2).

---

## 12. Cross-cutting completeness gaps (named, not yet designed)

The completeness analysis surfaced whole concerns no dimension owns. These need
RFCs or explicit deferral; flagging them now prevents late rework:

- **agent build/version upgrade choreography** (canary, claim-safe rolling
  restart, version-skew window, contract-major migration, rollback) — the single
  most repeated dangerous operation; currently undesigned.
- **GitOps fit** (ArgoCD/Flux): status-churn drift, SSA three-way fight on
  `.spec.replicas`, child pruning, custom health checks — several proposed
  patterns actively fight GitOps; settle before freezing the status contract.
- **Control-plane self-observability + SLOs** (operator/node-agent/gateway/
  coordination/webhook metrics; node-agent SPOF alerting; management-action audit).
- **Disaster recovery / backup** for the gateway task store, CRDs, coordination
  lease state, capability cache (RPO/RTO, restore ordering).
- **agentctl's own multi-component upgrade + skew matrix** (operator/node-agent/
  gateway/CLI/CRD storage migration).
- **Unified per-tenant cost/budget/quota governance** with an enforceable
  kill-switch + ResourceQuota + chargeback (needs the `EXIT_BUDGET` primitive).
- **A2A ingress rate-limiting / abuse / financial-DoS protection** coupled to
  autoscaling backpressure.
- **Ecosystem positioning** vs kagent/kaito/Dapr Agents/KServe/llm-d/Numaflow +
  integrate-vs-reinvent (esp. ModelPool vs KServe model serving).
- **Air-gapped operation** (image/artifact mirroring; in-cluster-only
  intelligence with model discovery disabled; offline Krew/Helm/OCI).
- **E2E + chaos testing** (node-agent crash mid-stream, operator failover
  mid-reconcile, lease-seam two-owner, model all-down, reschedule mid-task).
- **Secret/PKI lifecycle** (internal mTLS CA rotation across all components +
  External Secrets/Vault) — cert-manager is scoped only to the webhook today.
- **Data residency / retention / compliance** (task-store region pinning,
  model-routing region affinity, retention/erasure).

---

## 13. Where `ideas.md` changes — CONFIRMS / REVISES / ADDS

### 13.1 CONFIRMS (the vision holds)

- **The thesis** (agent owns primitives, agentctl owns policy; no k8s in agent;
  ask for a primitive rather than leak cluster logic).
- **Two workload CRDs at the core** (`Agent` + `AgentFleet`), manifest-driven
  reconcile, graceful degradation on `surfaces{}`.
- **Go for the stack** (`ideas.md §5`) — confirmed decisively (D2).
- **node-agent as the host-side keystone** that bridges management/telemetry/A2A
  over vsock; the management bridge IS bounce-safe.
- **A2A served by agent over vsock; the gateway is a transport bridge + PEP**,
  not a protocol translator (modulo the version-translation caveat).
- **`kubectl agent attach` as the novel UX** (steering = `subagent.send`).
- **Contract is generated/negotiated, pinned to `contract_version`.**
- **The vsock-everything posture is the most distinctive isolation story** — kept
  as the premium tier.

### 13.2 REVISES (the analysis changes the call)

- **vsock-everything: default → optional hardened tier.** Add a unix-socket
  PRIMARY/MVP substrate; vsock requires Kata-hybrid; reject QEMU real-vhost-vsock;
  drop "no network = nothing to attack" framing (D1).
- **node-agent: one DaemonSet → two tiers** (control/telemetry bounce-safe;
  A2A relay + replicated HTTP gateway HA); intelligence proxy NOT in the
  node-agent (D3).
- **A2A task store: "per-node vs shared" open question → shared durable store +
  node-local relay**, with the durability/re-drive/security corrections (D4).
- **operator↔node-agent path: the `ideas.md §7` open question → split by caller,
  with the aggregated-APIServer fix** for identity/per-verb RBAC (D5).
- **CRD versioning: implied additive-version ladder → single served version +
  conversion webhook + SVM**; contract_version decoupled from `apiVersion` (§2.2).
- **Admission: "validating webhook runs `agent --validate-config`" → ladder**
  (CEL → cached config-schema → init-container ground truth on the exact image);
  do not exec tenant images in the webhook; both subcommands are unbuilt (§3.3).
- **Scaling: "StatefulSet (shards) + KEDA" → two regimes** (claim = elastic
  default; shard = fixed, operator-resized); KEDA owns replicas (operator must
  omit them); scale-from-zero reads coordination-server depth, not a pod metric
  (§5).
- **MVP roadmap: vsock day-one → unix-socket day-one** (vsock is a later additive
  tier on the same code path) (§15).
- **`kubectl agent results` "works when pod is gone"** only via a persisted store
  + live capture, not `--report-file`-on-emptyDir alone (§9).
- **Trifecta admission: blocking Agent-level union → advisory; gate the override**
  (agent enforces per-spawn) (§2.2/§10).

### 13.3 ADDS (concerns `ideas.md` omits)

- **AgentClass / IntelligenceService(ModelPool) / MCPServerSet** CRDs (ops/dev
  seam, substrate + contract policy in one place).
- **The intelligence plane as a first-class plane** (proxy + ModelPool + cost
  governance) — `ideas.md` only mentions "wire the vsock model service."
- **A reference coordination MCP server** (correctness backbone) + the claim
  transport-on-networkless-pod problem.
- **The whole §12 completeness list** (upgrade choreography, GitOps,
  self-observability, DR, cost governance, A2A ingress protection, ecosystem
  positioning, air-gap, chaos testing, PKI lifecycle, data residency).
- **An explicit substrate compatibility matrix** and the exec-probe-on-scratch
  problem.
- **The consolidated agentd RFC ask list with milestone gating** (§14).

---

## 14. New agentd RFC asks (the cross-repo critical path)

These are primitives agentctl needs that agent does not yet expose. Each notes
which milestone it gates and whether it is a NEW ask or a CONTRACT DEFECT/FIX.
Sequence agentctl work behind these.

| ID | Ask | Type | Gates |
|---|---|---|---|
| **P1** | Exec-probe health verb (e.g. `agent --check-health PATH` that reads its own `--health-file` and exits 0/non-zero) — kubelet HTTP probes can't reach networkless pods and scratch has no shell to read the file | NEW | the entire isolation tier (every milestone on a networkless substrate) |
| **P3** | `--shard auto/N` deriving K from `AGENT_POD_NAME` ordinal — `AGENT_SHARD="K/N"` is unimplementable from one StatefulSet template; this is a defect in RFC 0014 §6.4 / RFC 0019 §4.2 | FIX | scale (shard mode) |
| **P4** | Define `agent://metrics` (byte-identical Prom 0.0.4 text, pinned mimeType) and `agent://capacity` (frozen schema) in RFC 0005/0015 — referenced by RFC 0019 but undefined | FIX | observe, scale (networkless metrics + victim selection) |
| **P6** | Implement `agent --config-schema` (RFC 0017, not yet built) and a `--validate-config --config-only` mode tolerating absent env | FIX | config milestone + admission + Tier-1 codegen |
| **P10** | Reconcile the autoscaling metric names: RFC 0016 frozen (`agent_pending_events`/`agent_reaction_lag_ms`) vs RFC 0019 (`agent_reactive_backlog`/`agent_saturation`/...) into ONE set; add `agent_saturation` to the frozen schema | FIX | scale (KEDA triggers) |
| **P9** | An off-pod backlog signal for scale-from-zero (a `work.stats` tool or countable `work://pending` on the coordination server) | NEW | scale (scale-from-zero) |
| **P12** | Umbrella decision: single canonical owner + frozen schema for `work.*` + `assign` + `agent/claim_key` `_meta` (RFC 0015 vs 0019) | FIX | scale (coordination) |
| **P2** | `surfaces.a2a` manifest key (served A2A version + address, or false) — RFC 0020 references it but RFC 0015 §5.2 doesn't list it; commit agent's `a2a` feature to specific wire method strings | NEW | A2A gateway |
| **P5** | Read-before-exit guarantee (or short post-terminal linger / read-ack) for `agent://run/{run_id}` AND a way to re-read a terminal distillate by run handle — so a networkless once-mode result is not lost if the relay/collector blinks | NEW | A2A durability + run-report capture |
| **P-meta** | Descriptive caller/tenant `_meta` convention agent echoes into its audit/event log (never re-verified, like downward-API identity) | NEW | A2A identity + management audit |
| **P-audit** | A closed-vocabulary management-action event (`mgmt.invoked{tool,caller?}`) | NEW | security audit |
| **P-inject** | Frozen `InjectEvent` shape for free-text steering + an inject event in the closed vocabulary (so multi-viewer attach echoes a steer) | NEW | interactive attach |
| **P-session** | Warm-session/handle enumeration with a `steerable` flag (status exposes only `warm_sessions` count today) | NEW | interactive attach |
| **P7** | Per-endpoint model arrays in the manifest + stable operator-assigned endpoint NAMES (`--intelligence-names`) so metric labels survive list reorder | NEW | model-aware placement + intel dashboards |
| **P-cost** | A clean budget-exhausted signal mapping to `EXIT_BUDGET(7)` on once / readiness back-pressure on reactive (distinct from auth-fatal / failover) | NEW | cost governance enforcement |
| **P-seq** | Monotonic per-frame seq on A2A StreamResponse notifications (parallel to the events-ring seq) | NEW | resumable A2A SSE |
| **P-trace** | Define traceparent ingest on the A2A method surface (RFC 0020 / RFC 0010 §3.6) | NEW | gateway-rooted traces |
| **P-hist** | Reconcile the histogram name set (RFC 0010 §3.8 otel-only vs RFC 0016 §4.3) and freeze `*_duration_ms` bucket boundaries | FIX | portable SLO dashboards |
| **P-a2a-out** | Concrete `--a2a-peer` flag schema + outbound A2A-over-vsock dial grammar (delegation-out) | NEW | A2A delegation-out |
| **P-attach-gate** | (Conditional) a per-tool gate within the Management profile to omit `inject` without dropping drain/cancel/observe | NEW | hostile-tenant "no-puppeting" tier |
| **P-pause** | Implement the contract-specified `pause`/`resume` operator tools (`ctrl/pause` control message + loop turn-boundary suspension); `OPERATOR_TOOLS` is `[drain, lame-duck, cancel]` today (§0.6) | NEW | full operator profile (agentctl RFC 0008/0009) |
| **P3b** | Confirm every `surfaces{}` key addition bumps contract MINOR; publish a versioned golden-fixture corpus of `--capabilities` outputs per feature-set | NEW | negotiation reliability + codegen |
| **P-dialects** | The agent advertises its supported in-binary intelligence **dialect set** in the manifest (e.g. `intelligence_summary.dialects[]`, analogous to P7's per-endpoint `models[]`) so the intelligence proxy's pass-through-vs-translate boundary is contract-driven, not keyed to the reference impl's two-adapter (`openai`/`anthropic`) inventory | NEW | intelligence plane (agentctl RFC 0012) |

**Blocking subset for the MVP:** P1 (if MVP runs on a networkless substrate),
P4 (observe), P6 (config + admission). Everything else gates later milestones.

---

## 15. Proposed agentctl RFC track

Numbered, each with scope. Ordered roughly by dependency; foundational ones first.

1. **agentctl-0001 — Stack & repo decision record.** Go for all five
   components; monorepo layout; Rust/hybrid revisit triggers; the `pkg/agent`
   client + black-box conformance anti-drift strategy. (D2)
2. **agentctl-0002 — Substrate & transport abstraction.** Tiered substrate
   (unix-hostPath PRIMARY, Kata-hybrid-vsock HARDENED, emptyDir-sidecar PORTABLE);
   the pluggable endpoint descriptor; the substrate compatibility matrix; reject
   QEMU real-vsock for v1. (D1)
3. **agentctl-0003 — Agent & AgentFleet CRD schema + status contract.** Mode→
   workload rendering; the conditions taxonomy; curated status projection;
   CEL invariants; the double-processing/double-schedule fixes. (§2)
4. **agentctl-0004 — AgentClass, IntelligenceService/ModelPool, MCPServerSet.**
   ops/dev decoupling CRDs; contract_version negotiation home; substrate
   selection. (§2/§6)
5. **agentctl-0005 — CRD versioning & conversion policy.** Single-served-version
   + conversion webhook + SVM; alpha→beta→GA graduation; decoupled from agent
   contract_version. (§2.2)
6. **agentctl-0006 — Operator reconcile & manifest-driven capability model.**
   The two-path static/live capability model; the CapabilityProbe cache; the
   edge-trigger + status-hot-loop discipline; the reconcile correctness fixes. (§3)
7. **agentctl-0007 — Admission validation ladder.** CEL → cached config-schema →
   init-container ground truth; webhook HA/cert/failurePolicy + operator-SA
   exemption; trifecta-override gating. (§3.3/§10)
8. **agentctl-0008 — node-agent architecture (two tiers).** Control/telemetry
   tier + the data-plane-out-of-path invariant (scoped); discovery-not-allocation;
   failure modes/blast radius. (§4/D3)
9. **agentctl-0009 — Management access path & RBAC.** Aggregated-APIServer human
   path + operator mTLS; per-verb RBAC + identity; node-agent internal authz; the
   `pods/proxy` stopgap caveat. (D5)
10. **agentctl-0010 — Observability & telemetry bridge.** Scrape-proxy + central
    SD; stderr→Loki for bulk events; run-outcome capture; exit-code→podFailurePolicy;
    trace correlation; control-plane self-observability. (§9/§12)
11. **agentctl-0011 — Scaling plane.** claim-mode vs shard-mode; KEDA external
    scaler; the reference coordination MCP server; shard-resize controller;
    standby/warm-pool. (§5)
12. **agentctl-0012 — Intelligence plane.** ModelPool + (out-of-node-agent) egress
    proxy; zero-secret-in-pod; cost/token governance (best-effort + enforcement
    path); model-aware placement. (§6)
13. **agentctl-0013 — A2A gateway & task store.** Relay (Tier B) + replicated HTTP
    gateway; shared durable store schema; method routing split; durability/re-drive;
    A2A version pinning; delegation-out. (§7/D4)
14. **agentctl-0014 — Agent mesh identity.** Fleet-level Agent Card projection;
    central card signing; `/.well-known` publication; in-cluster registry
    (federation deferred). (§7)
15. **agentctl-0015 — Security & multi-tenancy.** Trust/PEP model;
    hybrid-vsock multi-tenancy mandate; secret/PKI lifecycle; supply chain;
    guest→host egress restriction; threat model. (§10/§12)
16. **agentctl-0016 — CLI & kubectl-plugin grammar.** Three-faces packaging; cold/
    live paths; output contract; contract negotiation + degradation; attach UX
    (scoped). (§8)
17. **agentctl-0017 — Release & lifecycle engineering.** agent build rolling
    upgrade; agentctl multi-component upgrade/skew matrix; DR/backup; GitOps fit;
    air-gap. (§12)
18. **agentctl-0018 — Codegen & contract conformance.** Three-tier source-of-truth;
    `pkg/agent` generation; SHA+feature pinning; conformance suite; E2E/chaos test
    strategy. (§11)

---

## 16. Phased build roadmap with an explicit MVP cut line

The ordering is dependency-driven. The MVP cut line is drawn to be **runnable
against shipped agent primitives on a stock cluster** — i.e. it does NOT require
the isolation tier or any unbuilt primitive except those marked blocking.

**Phase 0 — Foundations (pre-MVP).** RFCs 0001/0002/0003/0006. Go monorepo;
`pkg/agent` client + conformance against a real agent over **unix socket**;
`Agent` CRD (`once`/`reactive`) + operator rendering on the **unix-hostPath
PRIMARY substrate**; the control-tier node-agent bridge.

**━━━━━━━━━━ MVP CUT LINE ━━━━━━━━━━**

**MVP — "manage what exists, portably."** `kubectl agent
get/describe/tree/logs/drain/lame-duck/cancel` over the unix-socket bridge; the
`Agent` CRD + operator for `once`/`reactive`; the management access path
(operator mTLS; `pods/proxy` admin stopgap acceptable). Runnable day one on stock
runc/containerd against shipped agent. **Blocking agent asks: none for unix
substrate; P1 only if MVP must run networkless.**

**Phase 1 — Observe.** RFC 0010. Scrape-proxy + central SD; stderr→Loki; `top`;
run-outcome capture + `results`; dashboards/alerts off the frozen metrics.
**Blocking: P4** (define `agent://metrics`/`agent://capacity`); **P5** for
robust once-mode results.

**Phase 2 — Config & reconfigure.** RFC 0007. ConfigMap-driven config (two-CM
partition); admission ladder; hot reload. **Blocking: P6** (`--config-schema` +
`--validate-config`).

**Phase 3 — Scale.** RFC 0011. claim-mode AgentFleet + KEDA external scaler + the
reference coordination MCP server; shard-mode + resize controller.
**Blocking: P3** (auto/N), **P9** (scale-from-zero signal), **P10** (metric
names), **P12** (work.* ownership).

**Phase 4 — Hardened substrate.** RFC 0002 hardened tier. vsock-on-Kata-hybrid;
the same node-agent code path, swapped dial-string; CID/uds discovery; exec
probes. **Blocking: P1** (exec health verb).

**Phase 5 — A2A gateway & mesh.** RFCs 0013/0014. Relay + replicated gateway +
shared store; Agent Card; auth/webhooks. **Blocking: P2** (`surfaces.a2a` +
wire-string commitment), **P5** (distillate durability), **P-meta**, **P-seq**
(for resumable SSE), **P-trace**.

**Phase 6 — Intelligence ops & cost.** RFC 0012. Multi-endpoint/health/hot-swap
wiring; ModelPool proxy (out-of-node-agent); cost governance. **Blocking: P7**
(per-endpoint models/names), **P-cost** (budget enforcement).

**Phase 7 — Hardening & lifecycle (continuous).** RFCs 0015/0017/0018. Security/
multi-tenancy; release/upgrade/DR/GitOps/air-gap; chaos testing; PKI lifecycle.

---

## 17. Top open questions (need a human decision to align before RFCs)

1. **Substrate target (D1).** Does the deployment environment guarantee a microVM
   runtime (Kata)? If agentctl must run on stock EKS/GKE/AKS or restricted tiers
   (Autopilot/Fargate/gVisor), the unix-socket/sidecar tier is the PRIMARY path
   and the "strongest isolation" claim is a premium tier — confirm this is
   acceptable positioning.
2. **Team kube-rs fluency (D2).** The Go verdict holds for any team WITHOUT deep
   kube-rs fluency (the common case). Does the team have it? If yes, and agent
   commits to a wire crate, the decision is worth a second look — but only then.
3. **node-agent split + A2A HA (D3/D4).** Accept the two-tier split and the
   shared durable task store as a v1 dependency (a stateful component to operate,
   back up, and DR)? Or accept weaker A2A durability/availability to avoid the
   store in v1?
4. **Management access path (D5).** Build the aggregated APIServer for the human
   path in v1 (heavier, correct), or ship the coarse `pods/proxy` stopgap for
   single-tenant and defer the APIServer? This gates whether per-verb RBAC and
   per-human audit exist at launch.
5. **Multi-tenancy posture (§10).** Is hostile multi-tenancy a v1 requirement? If
   yes, hybrid-vsock is mandatory, the pod-UID→uds mapping must be solved first,
   and `attach`/inject reachability needs the per-tool gate (P-attach-gate). If
   v1 is single-tenant/trusted-tenant, much of §10 simplifies.
6. **Coordination server (§5).** Ship a reference atomic-lease coordination MCP
   server (correctness backbone, but a stateful service to run/HA/DR), or BYO and
   accept that correctness rests on whatever the operator provides?
7. **Cross-repo sequencing (§14).** Are the agent teams willing to take the
   blocking primitive asks (P1/P3/P4/P6 first, then P2/P5/P9/P10/P12) on the
   timeline the roadmap implies? Several are contract FIXES (defects), not new
   features — confirm they land before the dependent agentctl milestones.
8. **Cost enforcement (§6/§12).** Is hard, fleet-wide budget enforcement a v1
   requirement (needs `EXIT_BUDGET` + a shared accounting store), or is
   best-effort metering + control-loop throttling acceptable for v1?
9. **A2A version (§7).** Which A2A version(s) does the gateway serve, and will
   agent's `a2a` feature commit to those exact wire strings (true pass-through)
   or must the gateway carry a version-translation layer?
10. **Ecosystem stance (§12).** Build a net-new operator, or compose with the
    existing agentic-on-k8s ecosystem (e.g. ModelPool backend = KServe
    InferenceService; reuse KEDA as-is)? This shapes scope and positioning.

---

*This brainstorm consumes the agent `rfcs/0014`–`0020` contracts and the
upstream RFCs they extend. Where this document and an agentd RFC disagree on the
wire, the RFC wins and this document is corrected; where this document identifies
a missing or defective primitive, it becomes an agentd RFC ask (§14), never a
leak of cluster logic into agent.*
