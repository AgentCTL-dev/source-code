# agentctl RFC 0009: Management access path & RBAC

**Status:** Proposed (agentctl management-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the access tier that fronts the node-agent's management API (agentctl RFC 0008). It owns *who* may call which lifecycle verb over the discovered, attested socket, and *how* operator and human identity reach the node-agent. agentctl RFC 0002 §11 stops at "a verified socket the authorized caller can reach"; this RFC defines the caller, the verb, and the authorization.

> **Split by caller — one path cannot serve both.** The **operator** (autonomous,
> high-volume, in-cluster, a single machine identity) and **humans** (`kubectl
> agent …`, needing per-verb RBAC, per-human identity, and per-human audit) have
> incompatible access requirements. The decision: the operator reaches the
> node-agent **directly over mTLS**; humans reach the management verbs through an
> **aggregated APIServer** (an `APIService`/extension-apiserver) so per-verb RBAC
> (`SubjectAccessReview`) and end-user identity **survive** to the node-agent.

> **Reachability == authority is the entire problem (agentd RFC 0015 §7).** A
> conformant agent has *no in-band auth*: whoever can reach the management
> transport may call `drain`/`lame-duck`/`cancel`/`subagent.send` (agentd RFC
> 0015 §3.4/§7). The node-agent holds an attested management connection to **every
> local pod** (agentctl RFC 0002/0008). So the node-agent is a single,
> all-tenants-reaching PEP, and agentctl owns **100% of authn/authz at the access
> path** — the agent re-verifies nothing. Two seemingly-obvious shortcuts (CRD
> subresources, `pods/proxy`) **both fail** to carry per-verb RBAC + identity to
> that PEP under hostile tenancy (§3).

> **The aggregated APIServer is the single sanctioned hybrid seam (agentctl RFC
> 0001 §6).** This RFC fires RFC 0001's **revisit trigger #3** explicitly: under
> the locked hostile-multi-tenancy decision the aggregated APIServer (or an
> equivalent that preserves identity + per-verb RBAC) is **REQUIRED for v1**, and
> a Go out-of-workspace `k8s.io/apiserver` component is permitted **for that
> component only**. The raw-`pods/proxy` shortcut is an **admin-only /
> single-tenant stopgap**, labeled as such throughout.

---

## 1. Problem / Context

agentctl turns lifecycle intent into action on a running conformant agent through
exactly one socket-adjacent component: the **node-agent** (agentctl RFC 0008),
which holds an *attested* management connection (agentctl RFC 0002 §7) to each
local agent pod and speaks the agent's self-MCP management profile (agentd RFC
0015 §3/§4). Two completely different callers need that same set of verbs:

1. **The operator** — leader-elected, autonomous, high-volume, in-cluster
   (agentctl RFC 0006). It reads the live capability snapshot every reconcile
   (agentd RFC 0015 §5.2–5.4 → agentctl RFC 0006 §4.2), drives drain choreography
   on finalizer/scale-down, and reflects status. It is **one machine identity**,
   it needs **low, predictable latency decoupled from human traffic**, and it
   authenticates with a client certificate, not a human credential.

2. **Humans** — `kubectl agent <name> drain|lame-duck|cancel|attach`, `kubectl
   agent <name> tree -w|logs -f|top -w` (the CLI grammar is **agentctl RFC 0016**,
   the human client). A human invocation needs three things the operator path
   does not: **per-verb RBAC** (alice may `drain` but not `attach`), **end-user
   identity** carried all the way to the PEP (so `kubectl auth can-i create
   agents/drain` is answerable), and **per-human audit** (who drained which pod,
   when, landed in the cluster audit log).

The verbs themselves are not invented here — they are the agent's **operator
profile** (agentd RFC 0015 §4), discovered from `surfaces.operator_tools` in the
manifest (agentd RFC 0015 §5.2) and driven only as advertised (graceful
degradation, agentd RFC 0014 §8):

| Verb (kubectl agent) | Underlying agent surface (agentd RFC 0015) | Shape |
|---|---|---|
| `drain` | `drain` tool (§4.1) — ≡ SIGTERM ≡ clean exit 0 | mutating |
| `lame-duck` | `lame-duck` tool (§4.2) — readiness flip, reversible | mutating |
| `pause` / `resume` | `pause`/`resume` (§4.3) — **flagged P-pause** (below) | mutating |
| `cancel <handle>` | `cancel` tool (§4.4) — wraps `subagent.cancel` | mutating |
| `attach` | **`subagent.send`** (§4.5) — live steering; **gated** (§6) | mutating (steer) |
| `logs -f` (live tail) | `agent://events` (agentd RFC 0016) | streaming read |
| `tree -w` (live) | `agent://inventory` (§5.3) | streaming read |
| `top -w` (live) | `agent://metrics` / `agent_*` (ask **P4** — to be defined in agentd RFC 0005/0015) | streaming read |
| `describe`/`get`/`results`/static `top`/`card` | `Agent.status` / persisted store / gateway | **cold** (no access-path hop) |

> **P-pause caveat (verified against the reference impl, brainstorm §0.6).** The
> reference implementation's `OPERATOR_TOOLS` is `["drain","lame-duck","cancel"]`
> today — `pause`/`resume` are specified by the contract (agentd RFC 0015 §4.3)
> but **not yet implemented** (contract ask **P-pause**). Because the access path
> renders its verb set from `surfaces.operator_tools`, not from a hardcoded list,
> a `pause` verb on a binary that does not advertise it is simply **not exposed**
> (the same capability-absence-not-error posture as everywhere else). This RFC
> specifies the access shape for the *whole* operator profile; which members are
> live is the manifest's call, per agent.

**The structural fact that organizes the whole design:** a conformant agent
exposes its management surface with **no auth** — the transport *is* the boundary
(agentd RFC 0015 §3.3/§7, agentd RFC 0012 §3.8). "Whoever can reach the
management transport may call the operator tools." The node-agent is precisely
the principal that can reach it — for *every* pod on its node, across *every*
tenant namespace. That makes the node-agent's management API a single
high-value, all-tenants PEP. Under the locked **hostile multi-tenancy** decision
(brainstorm §0.6), this is the load-bearing security seam of the management
plane: if a tenant can reach the node-agent's management API for a verb on a pod
that is not theirs, the isolation posture is void. So this RFC's job is to make
**per-verb authorization and end-user identity reach the node-agent**, and to
make the node-agent **refuse to be a confused deputy** (§5.3) — without putting
auth into the agent (which the contract forbids) and without duplicating
Kubernetes authz inside a privileged host component (which the brainstorm
rejects, D5).

This RFC owns: the caller split (§2), the correctness argument against the naive
shortcuts (§3), the hybrid-seam reconciliation (§4), the RBAC model (§5), and the
attach/inject gate (§6). It does **not** own: the node-agent's management API
*surface* and discovery loop (agentctl RFC 0008), the descriptor + attestation
(agentctl RFC 0002), the renderer / status projection (agentctl RFC 0006), the
admission-time `override-trifecta` SAR (agentctl RFC 0007 §3.3 — a distinct,
admission-only gate), the full multi-tenant trust model and the P-attach-gate
*home* (agentctl RFC 0015), or the CLI grammar + attach UX mechanics (agentctl
RFC 0016).

---

## 2. Decision — split by caller

Two paths, two trust models, one PEP. The autonomous machine path and the
identity-bearing human path are kept **physically distinct** all the way to the
node-agent, where they converge on the same attested socket but with different
authorization obligations.

### 2.1 The two paths

| | **Operator path** | **Human path** |
|---|---|---|
| Caller | the operator (one machine identity) | a human via `kubectl agent …` (agentctl RFC 0016) |
| Transport to node-agent | **direct mTLS** (operator client cert) | via the **aggregated APIServer** → mTLS to node-agent |
| Authn | the operator's SA / client cert | kube front-proxy / OIDC / SA token (standard delegation) |
| Authz | the operator is trusted for all it manages; **scoped by the node-agent per target** (§5.3) | **per-verb `SubjectAccessReview`** at the aggregated layer (§5.2) **+** node-agent per-target scoping |
| Identity at the PEP | the operator identity | **the end user**, forwarded through the chain |
| Audit | control-plane self-observability (agentctl RFC 0010 §10) | **per-human, in the kube audit log** |
| Traffic profile | autonomous, high-volume, latency-sensitive | interactive, low-volume, correctness-sensitive |
| Verbs | the full profile + the live reads (snapshot) | the full profile, **per-verb gated** + live reads |

The two paths exist because their requirements are genuinely irreconcilable on
one listener: a single TLS endpoint cannot simultaneously *require* an operator
client cert (the machine path) and *accept* an identity-forwarding,
certless-from-the-user front (the human path); and the operator's high-volume,
low-latency snapshot traffic must not share a fate or a queue with interactive
human streams. **(a) Operator → node-agent over mTLS** for autonomous/high-volume
traffic — this is the same connection the operator already uses to read the live
snapshot (agentctl RFC 0006 §4.2). **(b) Humans → the management verbs through an
aggregated APIServer** so per-verb RBAC and end-user identity survive to the
node-agent (D5).

### 2.2 Both paths, end to end

```
   HUMAN                                        ┌──────────────────────────────────────────────┐
   alice@corp  ── kubectl agent triage drain ──►│            kube-apiserver                      │
   (OIDC)                                        │  authn (front-proxy/OIDC/SA) · RBAC · AUDIT    │
                                                 └───────┬───────────────────────────┬──────────┘
                  APIService (AGGREGATED, §4.4)          │ proxied, IDENTITY-BEARING  │ watch / SSA
              management.agents.x-k8s.io/v1alpha1         │ (X-Remote-User: alice)     │ (Agent CRD + .status
                  (connect subresources only;            ▼                            │  in agents.x-k8s.io,
                   the Agent CRD stays in agents.x-k8s.io)                             │  served by kube-apiserver)
                                            ┌──────────────────────────────────┐      │
                                            │      AGGREGATED APISERVER          │      │
                                            │  (extension-apiserver; Go,         │      │
                                            │   out-of-workspace — RFC 0001 §6)  │      │
                                            │  1. recv authenticated user        │      │
                                            │  2. SubjectAccessReview:           │      │
                                            │     create agents/drain ns=acme    │◄──┐  │
                                            │  3. connect subresource handler    │   │  │ SAR/TokenReview
                                            │     drain·cancel·attach·log·tree·… │   └──┘ (delegated)
                                            └───────────────┬────────────────────┘      │
                                                            │ mTLS  { apiserver id,      │
                                                            │         fwd user=alice,    │
                                                            │         target=acme/triage }│
   OPERATOR                                                 │                             │
   (one SA, autonomous, high-volume)                        ▼                             ▼
   ── reconcile / drain / snapshot ── mTLS ──────►┌──────────────────────────────────────────────┐
      (operator client cert)                      │      node-agent  (DaemonSet, Tier A)            │
                                                  │      management API — agentctl RFC 0008         │
                                                  │  • exactly TWO known TLS clients:               │
                                                  │      (i) operator   (ii) aggregated APIServer   │
                                                  │  • per-target-namespace authz (DEFENSE IN DEPTH,│
                                                  │    never a confused deputy — §5.3)              │
                                                  │  • holds 1 attested conn per LOCAL pod          │
                                                  │    (descriptor + SO_PEERCRED/uds — RFC 0002 §7) │
                                                  └───────────────────────┬─────────────────────────┘
                                       reachability == authority          │ unix:PATH | vsock:PORT
                                       (agentd RFC 0015 §7)               ▼ PeerOrigin::Management
                                                  ┌──────────────────────────────────────────────┐
                                                  │  conformant agent pod (the reference impl: agent)│
                                                  │  self-MCP operator profile: drain · lame-duck ·  │
                                                  │  cancel · (pause/resume) · subagent.send · …     │
                                                  │  NO in-band auth — the transport is the boundary │
                                                  └──────────────────────────────────────────────┘

   COLD reads (kubectl agent get/describe/results/static top/card) bypass this entirely:
   plain CRD GET on Agent.status (RFC 0003/0006) · persisted run-report store (RFC 0010) ·
   A2A gateway (RFC 0013) — reusing kubeconfig auth, working even when the pod is gone.
```

### 2.3 Cold vs live — only the live/mutating verbs use the access path

The access path is for **live** state and **mutations**. Everything that can be
answered from already-persisted Kubernetes state takes the ordinary kube-apiserver
read path with kubeconfig auth (and so works when the pod is gone):

- **COLD** (`get`/`describe`/`results`/static `top`/`card`): a plain `GET` on
  `Agent.status` (the curated projection, agentctl RFC 0003 §6 / RFC 0006 §4.2),
  the persisted run-report store (agentctl RFC 0010), or the A2A gateway (agentctl
  RFC 0013 — a *third* path with its own auth/durability). These need **no**
  aggregated subresource and **no** node-agent hop; standard `get agents` RBAC
  governs them.
- **LIVE / mutating** (`drain`/`lame-duck`/`pause`/`resume`/`cancel`/`attach`,
  live `tree -w`/`logs -f`/`top -w`): these require reaching the *running* agent
  through the node-agent and are the subject of §3–§6.

This split matters for RBAC economy: a read-only auditor gets `get/list/watch
agents` and sees everything cold, with **zero** ability to reach a live socket.

---

## 3. Why NOT raw kube-apiserver pod-proxy (the correctness core)

The attractive shortcut — "model `drain`/`attach`/`cancel` as `Agent`
subresources under standard RBAC, and let the kube-apiserver's `pods`/`services`
proxy carry the bytes to the node-agent" — was red-teamed hard (brainstorm D5)
and is **not implementable as stated** under hostile tenancy. There are four
independent failures; any one is disqualifying, and they compound.

### 3.1 A CRD supports only `/status` and `/scale` subresources — not connect verbs

A CustomResourceDefinition exposes **exactly two** subresources: `/status` and
`/scale`. There is **no mechanism to register an arbitrary connect/streaming
subresource** (`pods/exec`-style) on a CRD. So `agents/drain`, `agents/attach`,
`agents/cancel` **cannot be CRD subresources**. An RBAC rule that names
`resources: ["agents/drain"]` will *parse and apply* — RBAC is opaque string
matching, it does not validate that a subresource exists — but **no request path
ever reaches it**: `kubectl`/`kube-apiserver` has no route that turns a
`create agents/drain` into anything. The result is **dead policy**: rules that
look like they gate the verb but gate nothing. This is the trap that makes
"per-verb RBAC on a CRD subresource" silently false (brainstorm §2.2/§8.2). The
*only* Kubernetes mechanism that can host arbitrary connect verbs on a custom
resource is an **aggregated/extension APIServer** (§4) — which is exactly why D5
lands there.

### 3.2 `pods/proxy` forwards **no end-user identity** to the backend

The kube-apiserver pod/service proxy is a *transport* proxy: it authenticates the
*caller to the apiserver*, then opens a connection to the backend and streams
bytes. It does **not** forward the end user's identity to the backend (there is no
`X-Remote-User` impersonation on the proxy path the way the aggregation layer
provides it). So a node-agent that wanted to answer "**can alice drain this
pod?**" with a `SubjectAccessReview` **cannot** — it never learns that the request
is alice's. Worse, a single node-agent TLS listener **cannot** simultaneously
*require* the operator's client certificate (the machine path, §2) and *accept*
the apiserver proxy's connection (which presents the apiserver's identity, not
alice's, and not the operator's). The identity needed for per-verb authz is
structurally absent on this path. The aggregation layer, by contrast, **does**
deliver the authenticated end user to the extension server via the standard
front-proxy delegation chain (§5.2) — that is its defining property.

### 3.3 `pods/proxy` is all-ports / all-paths — an all-tenants master key here

`pods/proxy` is coarse by design: a grant of `pods/proxy` (or `create
pods/proxy`) on a pod reaches **any port and any path** on that pod, with no
notion of a per-verb gate. Two consequences, both fatal under hostile tenancy:

1. **It bypasses the per-verb gate entirely.** There is no way to say "this
   principal may reach the `drain` verb but not the `attach` verb" through
   `pods/proxy` — it is reach-the-pod-or-not. The whole point of the access path
   (separating `drain` from `attach`, §6) is unexpressible.
2. **It is a multiplexed master key.** The node-agent terminates the management
   socket for **every pod on its node, across every tenant**. A principal granted
   `pods/proxy` on the *node-agent pod* (or on the DaemonSet's pods) thereby
   reaches the node-agent's management API for **all** of those pods — an
   all-tenants, all-namespaces key for destructive lifecycle ops (`drain`,
   `cancel`, live steering). Under hostile multi-tenancy this is precisely the
   capability the isolation posture exists to remove (brainstorm §10.1).

### 3.4 (Secondary) Streaming through generic `pods/proxy` is an engineering-risk seam, not a clean fit

This is the **weakest and non-load-bearing** of the four arguments — §3.1–§3.3 each
already independently disqualify `pods/proxy`, so this one only adds engineering
risk, not a correctness verdict. To be precise rather than overclaiming: the
kube-apiserver pod/service proxy **does** support connection upgrade (SPDY/WebSocket)
to an arbitrary backend port, so "streaming is impossible/unproven" would be too
strong. The real concern is **stream-lifetime correctness**: `logs -f` and `attach`
are long-lived, bidirectional, upgraded streams, and the kubelet's own
`pods/exec`/`pods/attach`/`pods/log` paths get clean half-close and lifetime handling
from *purpose-built* connect handlers, whereas the **generic** `pods/proxy` to a
*non-kubelet* backend (our node-agent) gives none of that purpose-built handling and
is a fragile shape to operate. The aggregated APIServer, by contrast, is built to own
connect subresources with proper upgrade handling (it is how `kubectl exec`-style
verbs are implemented for aggregated APIs) — it dials the backend, negotiates the
upgrade, and ties stream lifetimes to the client correctly. Even if this seam were
fully clean, §3.1–§3.3 still rule `pods/proxy` out under hostile tenancy.

### 3.5 Verdict: `pods/proxy` is an admin-only / single-tenant STOPGAP

Given §3.1–§3.4, **raw `pods/proxy` cannot be the v1 human management path under
hostile tenancy.** It survives only as an explicitly-labeled **stopgap** for
**single-tenant or admin-only clusters**, where (a) there is one trust domain so
"all-tenants master key" is vacuous, and (b) per-human attribution is a
nice-to-have, not a security requirement. Where it is used, agentctl MUST surface
that it is **coarse and unattributable**: a cluster running the `pods/proxy`
stopgap advertises (via a control-plane condition / install flag, agentctl RFC
0010 §10) that the management path provides **no per-verb RBAC and no per-human
audit**, so operators do not mistake it for the hardened path. It MUST be the
non-default; the default is §4.

---

## 4. Reconciling the aggregated APIServer with the all-Rust decision (RFC 0001 §6)

§3 forces the conclusion D5 reached: the human path that needs per-verb RBAC,
end-user identity, and clean streaming is an **aggregated APIServer** (an
`APIService` registered with the kube-apiserver's aggregation layer, served by an
extension-apiserver). This directly engages agentctl RFC 0001's all-Rust decision,
and the reconciliation must be explicit rather than implied.

### 4.1 The aggregated APIServer **is** the single sanctioned hybrid seam

agentctl RFC 0001 §6 records exactly one pre-approved deviation from all-Rust:

> *"An aggregated APIServer becomes v1-blocking … that component is materially
> harder in Rust (`k8s.io/apiserver` is Go-native). This does not reopen the whole
> decision — it activates the hybrid escape hatch for that component only … add
> **one** Go component (the aggregated APIServer) **out-of-workspace** (its own
> `go.mod` under `apiserver/`), keeping the other four Rust."* (agentctl RFC 0001
> §6, trigger #3 + "The single sanctioned hybrid escape hatch")

**This RFC fires trigger #3.** Under the locked hostile-multi-tenancy decision
(brainstorm §0.6), the aggregated APIServer (or an equivalent that preserves
identity + per-verb RBAC; §4.3) is **REQUIRED for v1** — not deferred. Therefore
the Go out-of-workspace `apiserver/` component (`k8s.io/apiserver` +
`apiserver-runtime`/`sample-apiserver` lineage) is **permitted and expected**,
and *only* that component. The other four components (operator, node-agent,
gateway/CLI faces, KEDA scaler) remain Rust in the one Cargo workspace; the
workspace layout already anticipates this ("The workspace can host an
out-of-workspace Go component … without reopening the Rust default for the other
four" — agentctl RFC 0001 §5).

### 4.2 Resolving brainstorm open question #4

Brainstorm §17 OQ #4 asked: *"Build the aggregated APIServer for the human path
in v1 (heavier, correct), or ship the coarse `pods/proxy` stopgap and defer?"*
Given the locked hostile tenancy:

> **Resolution.** The aggregated APIServer (or an equivalent preserving end-user
> identity **and** per-verb `SubjectAccessReview`) is **REQUIRED for v1**. The
> `pods/proxy` stopgap is acceptable **ONLY** for single-tenant / admin clusters
> (§3.5), as an install-time non-default, never under hostile tenancy. Per-verb
> RBAC and per-human audit therefore **exist at launch** for the multi-tenant
> product.

This is the answer the rest of the RFC builds on: §5's RBAC model assumes the
aggregated layer is present; §3.5's stopgap is the single-tenant fallback only.

### 4.3 The requirement is identity + per-verb RBAC, not a specific binary

The contract this RFC pins is **functional**, not "a Go process": the human path
MUST deliver, to the node-agent, (1) the authenticated **end-user identity** and
(2) a **per-verb authorization decision** that Kubernetes RBAC can express and
`kubectl auth can-i` can introspect, with the request **audited per human**. Two
admissible realizations:

| Option | What it is | v1 stance |
|---|---|---|
| **(i) Go aggregated APIServer, out-of-workspace** | `k8s.io/apiserver`-based extension-apiserver under `apiserver/` (its own `go.mod`); registers `agents/<verb>` connect subresources; native delegated authn/authz + audit. | **Chosen for v1.** The mature, low-risk realization; the sanctioned RFC 0001 §6 seam. |
| **(ii) Rust extension-apiserver** | A `kube-rs`/`hyper`-based implementation of the aggregation contract (delegated `TokenReview`/`SubjectAccessReview`, `APIService` serving, connect-verb upgrade). | **Permitted if/when feasible.** No mature Rust `k8s.io/apiserver` equivalent exists today (agentctl RFC 0001 §3 table marks this **High** cost); adopt only when it can pass the same conformance. Until then, (i). |
| **(iii) `pods/proxy` stopgap** | No extension server; raw apiserver pod proxy. | **Single-tenant / admin only** (§3.5). **Forbidden under hostile tenancy.** |

The dependency arrow stays P0-clean: the aggregated APIServer is an **agentctl**
component (it speaks the cluster's RBAC/identity machinery and the node-agent's
mTLS management API); it links **no** data-plane crate and execs **no** agent
binary. It is control-plane code, in whatever language the seam permits.

### 4.4 The CRD/aggregation split — where each resource is served (DECISION)

The aggregation layer routes **per GroupVersion, not per resource**: an `APIService`
for `v1alpha1.agents.x-k8s.io` delegates **every** `agents.x-k8s.io/v1alpha1`
request to the extension-apiserver. So the `Agent` CRD machinery (the CRD itself,
its `/status` subresource, CEL, the conversion webhook, the validating webhook) and
a set of custom connect subresources **cannot both live on the same GroupVersion** —
one of them has to move. This RFC makes the choice explicit rather than leaving it an
unstated (and as-drawn impossible) assumption:

> **Decision.** The `Agent`/`AgentFleet` resources and their `/status` subresource
> stay **CRDs in `agents.x-k8s.io/v1alpha1`, served by the kube-apiserver** —
> RFC 0003 (CRD + CEL), RFC 0005 (conversion webhook + `StorageVersionMigration`),
> and RFC 0007 (CEL/validating webhook on `agents`) are **unchanged**. The runtime
> **management connect verbs** live in a **separate, aggregation-owned GroupVersion,
> `management.agents.x-k8s.io/v1alpha1`**, served by the extension-apiserver (§4.1).
> That GroupVersion exposes a resource (`agents`) whose **only** purpose is to host
> the connect subresources `agents/drain`, `agents/attach`, … (§5.1); the
> extension-apiserver does **not** re-serve the `Agent` spec/status (those stay on the
> CRD path). Each connect handler resolves `{namespace,name}` against the CRD (a read
> to the kube-apiserver) and then dials the node-agent.

This is the only supported way to graft arbitrary connect verbs onto a custom resource
without moving the whole resource into the aggregated apiserver. The two admissible
shapes were:

| Option | Mechanism | Verdict |
|---|---|---|
| **(a) distinct management GroupVersion** (CHOSEN) | CRD stays in `agents.x-k8s.io`; verbs in `management.agents.x-k8s.io`, aggregated. | **Chosen.** RFC 0003/0005/0007 CRD machinery untouched; the cost is an ergonomic one (§4.4.1). |
| **(b) aggregate the whole `agents.x-k8s.io` GV** | The extension-apiserver owns and re-serves `Agent` + `.status` itself, plus the verbs. | **Rejected.** It would contradict RFC 0003 (CRD), RFC 0005 (conversion webhook + SVM), and RFC 0007 (CEL/validating webhook on `agents`) — re-implementing all of it inside a Go apiserver. Far larger blast radius for the same outcome. |

#### 4.4.1 The ergonomic cost (conceded)

Because the verbs key off `management.agents.x-k8s.io` while the cold reads key off
`agents.x-k8s.io`, a Role granting both needs **two `apiGroups` entries** (§5.4), and
`kubectl auth can-i create agents/drain` must name the group
(`agents/drain.management.agents.x-k8s.io`). A stock RBAC auditor still sees and gates
the verb (it is a first-class aggregated resource, not a dead CRD-subresource string,
§3.1) — the legibility property survives; only the single-group ergonomic does not.
This is the price of keeping the `Agent` CRD and its conversion/validation machinery
whole, and it is the right trade.

### 4.5 Availability posture (a v1-required component and a human-path SPOF)

The aggregated APIServer is elevated here from RFC 0001's *deferred/conditional*
status to a **hard v1 requirement** (§4.2), and it fronts destructive lifecycle
verbs — so its availability is a body concern, not just an open question:

- **It is a SPOF for the human path only.** A down extension-apiserver blocks
  `kubectl agent <verb>` (the human path), but the **operator path is unaffected** —
  the operator dials the node-agent over direct mTLS and never traverses the
  aggregation layer (§2.1). Autonomous reconcile, finalizer drain, and status
  projection keep working through a human-path outage.
- **HA posture.** Run **≥2 replicas** behind the `APIService`, with a **PDB**,
  `priorityClassName: system-cluster-critical`, and a leader-agnostic design (it
  holds no durable state — it authorizes and proxies). It may co-reside in/near the
  operator deployment, reusing the webhook HA pattern (agentctl RFC 0007 §4.3).
- **Aggregation-layer failure semantics.** When the extension-apiserver is
  unavailable, the `APIService` `Available` condition goes `False` and the **whole
  `management.agents.x-k8s.io/v1alpha1` GroupVersion becomes unavailable** — but
  because §4.4 keeps the `Agent` CRD on a *separate* GroupVersion served by the
  kube-apiserver, **cold reads (`get/list/watch agents`, `.status`) keep working**
  through the outage (this is a second reason the §4.4 split is the right shape).
- **CLI degradation contract.** On `APIService` unavailability the CLI surfaces a
  clear "management path (aggregated APIServer) is unavailable; cold reads still
  work; the operator's autonomous path is unaffected" message rather than an opaque
  error. OQ #7 tracks finalizing the exact `Available`-condition probe and message.

---

## 5. The RBAC model

The model turns "reachability == authority at the node-agent" into "**Kubernetes
RBAC at the aggregated layer**, defended in depth by the node-agent." Three parts:
the verbs as virtual subresources (§5.1), the `SubjectAccessReview` that makes
them real and identity-bearing (§5.2), and the node-agent's internal per-target
authz that prevents a confused deputy (§5.3).

### 5.1 Management verbs as connect subresources on the aggregated `agents` resource

The aggregated APIServer serves the **`management.agents.x-k8s.io` group** (the
distinct, aggregation-owned GroupVersion of §4.4 — *not* the CRD's
`agents.x-k8s.io`) and registers, **on its `agents` (and `agentfleets`) resource**, a
set of **connect subresources** — the thing a CRD cannot do (§3.1) but an
extension-apiserver can:

| Aggregated subresource | RBAC verb | Backs (agentd RFC 0015) | Kind |
|---|---|---|---|
| `agents/drain` | `create` | `drain` tool (§4.1) | mutating (connect) |
| `agents/lame-duck` | `create` | `lame-duck` tool (§4.2) | mutating (connect) |
| `agents/pause` | `create` | `pause`/`resume` (§4.3, **P-pause**) | mutating (connect) |
| `agents/cancel` | `create` | `cancel` tool (§4.4) | mutating (connect) |
| `agents/attach` | `create` | **`subagent.send`** (§4.5) — **gated, §6** | mutating (connect/stream) |
| `agents/log` | `get` | `agent://events` (agentd RFC 0016) | streaming read |
| `agents/inventory` | `get` | `agent://inventory` (§5.3) — backs `tree -w` | streaming read |
| `agents/metrics` | `get` | `agent://metrics` (ask **P4** — to be defined in agentd RFC 0005/0015; referenced by RFC 0019) — backs live `top` | streaming read |

The **verb-on-subresource** choice mirrors kubelet idiom precisely: **mutating /
write-stream** verbs use `create` (as `pods/exec`, `pods/attach` do); **read /
read-stream** verbs use `get` (as `pods/log` does). This is what makes the policy
*legible to existing tooling*: `kubectl auth can-i create agents/drain.management.agents.x-k8s.io -n acme`
returns the right answer (the group-qualified form, §4.4.1), and an auditor's
existing RBAC review catches an over-broad grant. Each verb is a **separate**
subresource so RBAC can grant them **independently** — the whole reason `attach`
(§6) can be withheld while `drain` is allowed.

This shares an *idea* with the "synthetic verb in a `SubjectAccessReview`" device
agentctl RFC 0007 §3.3 uses for the admission-time `override-trifecta` gate, but the
**mechanism differs**: 0007's `override-trifecta` is a synthetic **verb** on the
`agents` *resource*, string-matched **at admission** (no request path is required,
and it is never made callable); this RFC's verbs are runtime **connect subresources**
served by the aggregated apiserver (a real request path reaches them, §4.4). So this
RFC does **not** make `override-trifecta` callable and does **not** resolve RFC 0007
OQ #3 (whether `override-trifecta` should be a real or synthetic verb is unchanged) —
it only establishes that an aggregated apiserver exists, which *informs* that choice.

### 5.2 `SubjectAccessReview` at the aggregated layer — identity survives

The aggregation layer's defining property is the one §3.2 showed `pods/proxy`
lacks: the kube-apiserver **forwards the authenticated end user** to the
extension-apiserver via the front-proxy headers (`X-Remote-User` /
`X-Remote-Group`, authenticated by the requestheader CA), the standard delegated
authentication chain. So the aggregated APIServer **knows the request is alice's**
and performs, for the *specific verb on the specific namespaced object*:

```
SubjectAccessReview {
  user: "alice", groups: [...],                  // delegated from the kube front-proxy
  resourceAttributes: {
    group: "management.agents.x-k8s.io", resource: "agents",  // the aggregated GV (§4.4)
    subresource: "drain",                        // the per-verb gate (§5.1)
    namespace: "tenant-acme", name: "triage",    // the specific target object
    verb: "create" } }                           // create == connect/mutate (kubelet idiom)
```

The SAR is issued **against the kube-apiserver** (delegated authorization), so it
honors *all* of the cluster's RBAC — Roles, ClusterRoles, bindings — with no
parallel authz store. On `allowed:false`, the aggregated APIServer returns
`403 Forbidden` to `kubectl` **before** any byte reaches the node-agent. On
`allowed:true`, it forwards (§5.3). Crucially, because the request *is* a
first-class API request to the kube-apiserver's aggregation front, it is **audited
per human in the cluster audit log** automatically (RequestReceived/ResponseStarted
stages with `user`, `objectRef.subresource=drain`, `verb=create`) — satisfying the
per-human-audit requirement with no bespoke logging.

### 5.3 The node-agent's internal per-target authz (defense in depth — never a confused deputy)

The node-agent MUST NOT trust "a connection arrived on my management API" as
authorization to act on an arbitrary pod — that is exactly the confused-deputy
hazard a single all-tenants PEP invites. Defense in depth, **without** duplicating
Kubernetes authz wholesale (the explicitly-rejected alternative, brainstorm D5):

1. **Exactly two known TLS clients.** The node-agent's management API accepts mTLS
   from **only** (i) the operator identity and (ii) the aggregated APIServer
   identity (pinned client certs / SPIFFE IDs from the internal CA, agentctl RFC
   0015). Any other peer is refused at the TLS layer. This is what makes §3.2's
   "single listener can't do both" a non-issue: there is no certless proxy peer;
   both peers are authenticated machine identities.

2. **Every request carries an explicit target + (for the human path) the
   forwarded end user.** A management request names `{namespace, name/pod_uid,
   verb}`. The node-agent resolves the target to an **attested descriptor it
   already holds** (agentctl RFC 0002 §7 — the connection it proved owns
   `pod_uid`); it physically **cannot** act on a pod it has not discovered and
   attested on *this* node. Cross-node requests are not served (the descriptor is
   node-local) — they are the aggregated APIServer's routing problem, not a verb
   the node-agent can be tricked into.

   **The name→node→node-agent routing leg is a precondition, not a refinement.** A
   human targets `tenant-acme/triage` by *name*, so the aggregated APIServer MUST
   resolve name→node→node-agent **before** any mTLS dial — the walkthroughs (§7.1)
   assume this resolved. The **source of truth** is the node-agent-published live
   snapshot transport (the node-agent-owned watchable object — an
   `AgentInstance`/`EndpointSlice` carrying `{namespace, name, uid, node}` — that
   agentctl RFC 0008 open question (a) settles, deferred there from RFC 0006 §12):
   the aggregated APIServer reads that mapping. **Stale-mapping behavior is
   normative:** on a reschedule, the destructive-verb path MUST **re-resolve** and,
   if the target node-agent does not hold a *currently-attested* descriptor for that
   `uid`, **reject** (a fresh resolve or a `409/404`) rather than act on a stale or
   wrong node — never drain a wrong/stale node-agent. The transport choice itself is
   RFC 0008's; this RFC commits only to the source-of-truth and the
   re-resolve-or-reject rule, and OQ #4 tracks confirming the `AgentInstance` vs
   apiserver-watch realization.

3. **Per-target-namespace re-check (the anti-confused-deputy core).** For the
   **human path**, the node-agent requires the aggregated APIServer to have
   forwarded the end-user identity **and** to assert the per-verb SAR result for
   the *specific target* (§5.2). The node-agent **re-verifies the target's
   namespace matches** the authorized namespace before issuing the `tools/call`,
   so a routing or templating bug in the front cannot cause the node-agent to
   drain a pod in a namespace the user was never authorized for. The node-agent
   does **not** re-run the full RBAC (no `kubectl auth can-i` re-implementation in
   a privileged host process — D5); it scopes, pins, and re-checks the **target**,
   which is the narrow invariant that prevents the deputy from being confused.
   For the **operator path**, scoping is to the set of objects the operator
   manages on this node (the operator is trusted for its own reconcile, but is
   still bound to node-local attested descriptors per (2)).

4. **Audit at the PEP too.** The node-agent emits a management-action audit record
   per call — `{verb, target, caller, forwarded_user?}` — keyed to the contract's
   `mgmt.invoked`-class vocabulary (agent ask **P-audit** / **P-meta**; agentctl
   RFC 0015 audit vocabulary). This composes with the kube audit log (§5.2): the
   *front* records "alice asked to drain acme/triage"; the *PEP* records "the
   node-agent executed drain on the attested pod_uid". Two independent records of
   one destructive act.

The descriptive caller/tenant identity the node-agent forwards onto the agent's
`_meta` (agent ask **P-meta**) is **never re-verified by the agent** — exactly
like the downward-API identity (agentd RFC 0015 §6). Authority lives entirely in
agentctl's two PEPs; the agent only *records* who asked.

### 5.4 RBAC examples

The payoff is ordinary, reviewable RBAC. A tenant operator who may manage but not
puppet:

```yaml
# tenant-acme: may drain/lame-duck/cancel + observe their own agents; may NOT attach.
# NOTE the two apiGroups: runtime verbs are the AGGREGATED management group (§4.4);
# cold reads are the CRD group. This two-group split is the conceded ergonomic cost (§4.4.1).
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: { name: agent-operator, namespace: tenant-acme }
rules:
  - apiGroups: ["management.agents.x-k8s.io"]   # aggregated connect verbs (§4.4)
    resources: ["agents/drain", "agents/lame-duck", "agents/cancel", "agents/pause"]
    verbs:     ["create"]                       # mutating connect verbs (kubelet pods/exec idiom)
  - apiGroups: ["management.agents.x-k8s.io"]   # aggregated streaming reads (§4.4)
    resources: ["agents/log", "agents/inventory", "agents/metrics"]
    verbs:     ["get"]                          # streaming reads (kubelet pods/log idiom)
  - apiGroups: ["agents.x-k8s.io"]              # the CRD group — kube-apiserver (RFC 0003)
    resources: ["agents"]
    verbs:     ["get", "list", "watch"]         # COLD reads of .status (RFC 0003) — no node-agent hop
  # agents/attach is DELIBERATELY ABSENT — see §6 (the no-puppeting gate).
---
# A break-glass steerer (cluster-wide attach), held by few, every use audited (§5.2).
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: { name: agent-steerer }
rules:
  - apiGroups: ["management.agents.x-k8s.io"]   # aggregated connect verb (§4.4)
    resources: ["agents/attach"]
    verbs:     ["create"]
---
# Read-only auditor: sees everything COLD, can reach NO live socket.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: { name: agent-viewer }
rules:
  - apiGroups: ["agents.x-k8s.io"]
    resources: ["agents", "agentfleets"]
    verbs:     ["get", "list", "watch"]
```

`kubectl auth can-i create agents/drain.management.agents.x-k8s.io -n tenant-acme`
now returns a true answer; `kubectl auth can-i create
agents/attach.management.agents.x-k8s.io -n tenant-acme` returns `no` for the
`agent-operator` role — the per-verb separation that §6 depends on, expressed in
stock RBAC (group-qualified per §4.4.1).

---

## 6. attach / inject gating under hostile tenancy

`attach` is categorically different from every other verb, and hostile tenancy
makes the difference load-bearing. `drain`/`cancel`/`lame-duck`/observe are
*lifecycle and read* operations; **`attach` is live puppeting** — it is
`subagent.send` (agentd RFC 0015 §4.5), injecting free-text steering into a warm
session via `ctrl/inject`. The contract ask is exactly the brainstorm's
**P-attach-gate**: *a caller must be able to `drain`/`cancel`/observe a neighbour
under a shared PEP, but not steer it* (brainstorm §0.6 tenancy row (c), §10.2).

### 6.1 Two layers of gating, because RBAC alone is not enough here

**Layer 1 — per-verb RBAC at the aggregated layer (§5).** Because `agents/attach`
is its own subresource with its own `create` verb, RBAC already separates it from
`drain` in the human path (§5.4): the `agent-operator` role omits it; only
`agent-steerer` holds it. This is necessary and is the *primary* human-path
control. It is sufficient **only** if every path to the agent's steering primitive
passes through that gate.

**Layer 2 — a contract per-tool gate on the management transport
(P-attach-gate).** RBAC at the front is **not** sufficient by itself, for two
contract-level reasons (agentd RFC 0015 §10.2, brainstorm §10.2):

1. **`subagent.send` is a *work* tool, not an operator tool.** It is listed to
   **both** `Stdio` and `Management` peers (agentd RFC 0015 §3.4 / §4.5) — it is
   not gated by `PeerOrigin` the way `drain` is. So at the node-agent PEP, *any*
   principal that can reach the management transport can call it — i.e.
   reachability == steering, not just reachability == lifecycle. Under a
   multiplexed all-tenants PEP that is the precise capability we must be able to
   remove.
2. **The same primitive is reachable via a *different* PEP.** `subagent.send`
   warm-session steering is also reachable through A2A multi-turn at the gateway
   (agentctl RFC 0013), a PEP with its own policy. A "no-puppeting" guarantee that
   only the management front enforces is not a guarantee.

Therefore agentctl asks for **P-attach-gate**: a per-tool gate **within the
Management profile** so the agent can be built/configured to **omit `inject` /
`subagent.send` from the management transport without dropping
`drain`/`cancel`/observe** (agent ask P-attach-gate; the home of the multi-tenant
requirement is agentctl RFC 0015). With it, "no live-puppeting for tenant X" is
**structural** at the PEP — the node-agent, configured for a no-puppeting tenant,
serves a management surface on which steering is *not present*, so even a
front-side RBAC bug cannot reach it. This is the same capability-absence-not-error
shape the contract uses everywhere (agentd RFC 0015 §2.5).

### 6.2 The honest v1 posture (until P-attach-gate lands)

Until P-attach-gate is in the contract, this RFC is explicit about the residual
exposure rather than papering over it:

- The **human path** gate is real and enforced: `agents/attach` per-verb RBAC
  (§5.4) + the node-agent's per-target scoping (§5.3) mean a human without the
  `attach` grant cannot steer through the aggregated APIServer.
- The **residual** is the contract truth that *reachability of the management
  transport == ability to call `subagent.send`* (§6.1.1). agentctl's mitigation
  in the interim is operational, not structural: the node-agent's two-known-clients
  rule (§5.3.1) means only the operator and the aggregated APIServer can reach the
  management transport at all, and the aggregated APIServer **does** gate
  `agents/attach` per-verb. So the residual reduces to "trust the two PEPs," which
  is the multi-tenant trust assumption agentctl RFC 0015 owns. **With**
  P-attach-gate the guarantee becomes structural for tenants configured
  no-puppeting; this RFC records the dependency and defers the structural form to
  the contract.

### 6.3 Steering is `subagent.send`; the attach UX is scoped (and lives elsewhere)

This RFC governs the **authorization** of attach, not its UX. For completeness and
to bound scope:

- `attach` is **not a new agent tool** — it is `subagent.send` (agentd RFC 0015
  §4.5). agentctl adds nothing to the agent here; it *names and gates* the
  primitive.
- v1 `kubectl agent attach` is scoped to **one-shot `--send` + a read-only event
  tail** (both backed by today's contract). Interactive multi-viewer steering with
  steer-echo and session-target enumeration needs **new** agent primitives
  (P-inject, P-session; brainstorm §8.2) and is **out of scope** here — named so
  the gate is not mistaken for the UX.
- The single-writer attach **lease** (read-many / write-one, `--steal`,
  `--read-only`) lives in agentctl because the agent has no session/auth model;
  its mechanics are **agentctl RFC 0016** (CLI grammar) / **agentctl RFC 0008**
  (node-agent). This RFC fixes only that `attach` is its own RBAC verb (§5.1) and
  its own contract gate (§6.1).

---

## 7. Worked request walkthroughs

### 7.1 Human `kubectl agent triage drain -n tenant-acme` (allowed)

```
1. kubectl resolves the management.agents.x-k8s.io APIService (the aggregated GV, §4.4)
   → POSTs to agents/triage/drain (connect subresource). The Agent CRD itself stays in
   agents.x-k8s.io on the kube-apiserver — only the verb is aggregated.
2. kube-apiserver authenticates alice (OIDC), then proxies to the aggregated APIServer,
   forwarding identity via front-proxy headers (X-Remote-User: alice). [§5.2]
3. Aggregated APIServer issues SubjectAccessReview{user:alice, verb:create,
   group:management.agents.x-k8s.io, resource:agents, subresource:drain,
   ns:tenant-acme, name:triage} → allowed:true. [§5.2]
   (kube audit log records the per-human request automatically.)
4. Aggregated APIServer resolves triage → its node via the node-agent-published snapshot
   (re-resolve-or-reject on a stale mapping, §5.3.2), then opens mTLS to that node's
   node-agent, presenting {apiserver id, fwd user=alice, target=tenant-acme/triage}. [§2.2]
5. node-agent: client is the known aggregated-APIServer cert (✓, §5.3.1); resolves
   tenant-acme/triage → attested descriptor it holds (✓, §5.3.2); re-checks target ns ==
   authorized ns (✓, §5.3.3); emits mgmt-action audit (§5.3.4).
6. node-agent calls `drain` on the agent's self-MCP (PeerOrigin::Management). Agent runs
   the SIGTERM choreography → clean exit 0 (agentd RFC 0015 §4.1). Snapshot returned.
7. Stream/result flows back: node-agent → aggregated APIServer → kube-apiserver → kubectl.
```

### 7.2 Human `kubectl agent triage attach -n tenant-acme` by the `agent-operator` role (denied)

```
3'. SubjectAccessReview{... subresource:attach ...} → allowed:FALSE (role omits agents/attach, §5.4).
    Aggregated APIServer returns 403 Forbidden to kubectl. NO byte reaches the node-agent. [§6.1 L1]
    (With P-attach-gate, even a misconfigured front cannot reach steering: the node-agent's
     management surface for this tenant omits subagent.send entirely. [§6.1 L2])
```

### 7.3 Operator drain on finalizer / scale-down (no human in the loop)

```
1. Operator (leader) decides to drain triage (CR deletion finalizer, agentctl RFC 0006). [§2]
2. Operator → node-agent over direct mTLS (operator client cert). [§2.1]
3. node-agent: client is the known operator cert (✓, §5.3.1); target is a node-local
   attested descriptor the operator manages (✓, §5.3.2). Emits mgmt-action audit.
4. node-agent calls `drain`. (Identical agent-side effect as 7.1 — same tool, agentd RFC 0015 §4.1.)
   Latency is on the operator's own listener, decoupled from any human stream. [§2.1]
```

### 7.4 Single-tenant cluster, `pods/proxy` stopgap

```
Install selects the pods/proxy stopgap (no aggregated APIServer). [§3.5]
- kubectl agent drain → kube-apiserver pods/proxy → node-agent. NO end-user identity is
  forwarded (§3.2); NO per-verb gate (§3.3). RBAC is coarse `create pods/proxy` only.
- agentctl surfaces a control-plane condition: "management path = pods/proxy stopgap;
  no per-verb RBAC, no per-human audit; admin/single-tenant only." [§3.5]
- FORBIDDEN if any namespace is labeled hostile-tenant. [§4.2]
```

---

## 8. Non-goals

- **The node-agent's management API *surface* and discovery loop.** agentctl RFC
  0008. This RFC fronts that API with the access path + authz; it does not define
  the API's methods, the connection manager, or the Tier-A/Tier-B split.
- **The descriptor + attestation.** agentctl RFC 0002 (the `EndpointDescriptor`,
  `SO_PEERCRED`/per-VM-uds attestation). This RFC consumes "an attested, node-local
  socket" and authorizes *who may drive it*.
- **The CLI grammar and the attach UX/lease mechanics.** agentctl RFC 0016 (the
  human client — `kubectl agent[s]` faces, cold/live paths, `--steal`/`--read-only`
  lease, output contract). This RFC owns only attach's *authorization* (§6).
- **The full multi-tenant trust model, the two-PEP framing, internal mTLS / PKI,
  and the P-attach-gate home.** agentctl RFC 0015. This RFC enforces the
  management-PEP half; the A2A-gateway PEP and the cross-cutting trust model are
  there.
- **Defining the agent's operator tools or adding auth to the agent.** agentd RFC
  0015 owns the tools; the contract is deliberately auth-free (agentd RFC 0012
  §3.8). agentctl never asks the agent to authenticate — authority is the two PEPs.
- **The admission-time `override-trifecta` SAR.** agentctl RFC 0007 §3.3 — a
  distinct, *admission-only* synthetic-verb gate (it works by RBAC string match,
  needs no callable request path). This RFC's runtime verbs need the aggregated
  APIServer to be *callable* (§5.1); the two share the synthetic-verb idea, not the
  machinery.
- **Cold reads, run-report persistence, and the A2A path's auth.** Plain CRD reads
  on `Agent.status` (agentctl RFC 0003/0006), the persisted run-report store
  (agentctl RFC 0010), and the A2A gateway (agentctl RFC 0013) each carry their own
  auth and are out of the access-path scope (§2.3).
- **Control-plane self-observability / audit aggregation.** agentctl RFC 0010 §10
  (the node-agent SPOF alerting, the management-action audit sink). This RFC
  *emits* the PEP audit record (§5.3.4); where it lands is there.

---

## 9. Open questions

1. **Aggregated APIServer realization for v1: Go out-of-workspace vs a Rust
   extension-apiserver (§4.3).** Chosen: **(i) Go**, the sanctioned RFC 0001 §6
   seam. Confirm the team accepts the second toolchain for this one component, or
   commits to attempting **(ii) Rust** and accepting the **High**-cost risk
   (agentctl RFC 0001 §3 table). The functional contract (identity + per-verb
   SAR + audit) is fixed either way.
2. **Connect-verb idiom: `create` for all mutating verbs vs distinct verbs.**
   §5.1 uses `create` (kubelet `pods/exec` idiom) for every mutating subresource.
   Should destructive verbs (`drain`, `cancel`) instead use a *named* verb (e.g.
   a custom `drain` verb on `agents`) for finer audit/policy granularity, at the
   cost of departing from kubelet idiom and `kubectl auth can-i` familiarity?
3. **node-agent ↔ aggregated-APIServer assertion shape (§5.3).** How does the
   aggregated APIServer assert the forwarded user + per-verb SAR result to the
   node-agent — re-forwarded headers the node-agent trusts (because the client cert
   is the pinned aggregated-APIServer identity), or a signed short-lived token? The
   former is simpler; the latter is robust if the client-cert pin is ever
   weakened. Reconcile with the internal-PKI design (agentctl RFC 0015).
4. **Cross-node routing for the human path (source-of-truth realization).** §5.3.2
   now commits the **rule** in the body — the aggregated APIServer resolves
   name→node→node-agent from the node-agent-published watchable snapshot transport
   (agentctl RFC 0008 OQ (a)), and on a reschedule MUST **re-resolve or reject**
   rather than act on a stale node. This OQ tracks only the remaining realization
   detail: confirming that transport is an `AgentInstance`/`EndpointSlice` the
   node-agent owns (RFC 0008 OQ (a) / RFC 0006 §12) vs the aggregated APIServer
   watching the node→pod map directly, and the exact freshness window for the
   re-resolve check.
5. **P-attach-gate timeline (§6).** The structural no-puppeting guarantee depends
   on the contract ask. If P-attach-gate slips past v1, is the operational residual
   (§6.2 — two-known-clients + per-verb RBAC) an acceptable v1 posture for hostile
   tenants, or must a tenant requiring structural no-puppeting be refused
   `attach`-capable agents until the gate ships?
6. **`pods/proxy` stopgap surface (§3.5).** How is "this cluster runs the coarse
   stopgap" advertised so it cannot be silently mistaken for the hardened path — a
   cluster-scoped condition, an install-flag-gated capability the CLI reports in
   `kubectl agent` help, or both? And is the stopgap even worth shipping, or should
   single-tenant clusters simply run the aggregated APIServer too (it is correct
   there as well, just heavier)?
7. **APIService availability — finalize the probe/message.** §4.5 now commits the
   HA posture in the body (≥2 replicas, PDB, `system-cluster-critical`, the
   operator-path-unaffected property, and cold reads surviving via the §4.4 split).
   This OQ tracks only finalizing the exact `APIService` `Available`-condition probe
   and the CLI degradation message, reconciled with the webhook HA pattern (agentctl
   RFC 0007 §4.3) since both may co-reside in/near the operator.
8. **`apiGroup` strings (CRD group + management group).** Inherits agentctl RFC 0003
   §13 / RFC 0007 OQ #7 for the CRD base (`agents.x-k8s.io` vs `agentctl.dev`); the
   §4.4 aggregated management GroupVersion derives from it as
   `management.<base>` (e.g. `management.agents.x-k8s.io`), and the
   `agents/<verb>` subresource RBAC keys off that management group. Settle the base
   string and confirm the `management.<base>` derivation.

---

## 10. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo decision record: §6 the **single sanctioned
  hybrid escape hatch** (trigger #3, the Go out-of-workspace aggregated APIServer)
  this RFC fires (§4); §3 the "No mature Rust `k8s.io/apiserver` equivalent /
  **High** cost" row behind §4.3 option (ii); §5 the workspace that hosts the
  out-of-workspace `apiserver/`.
- **agentctl RFC 0002** — substrate & transport abstraction: §11 defers "the
  management access path / RBAC / aggregated APIServer … is agentctl RFC 0009" to
  this RFC; §7 the **attested** descriptor (`SO_PEERCRED`/per-VM-uds) the node-agent
  scopes each verb to (§5.3.2).
- **agentctl RFC 0003** — Agent & AgentFleet CRDs: the `Agent` kind the aggregated
  subresources hang off; §6 the curated `.status` the **cold** read path serves
  (§2.3); CEL invariants distinct from runtime authz.
- **agentctl RFC 0006** — operator reconcile & capability model: §4.2 the operator's
  **live snapshot** read (the operator-path mTLS traffic, §2.1); the single-`.status`-
  writer; §4 open item on the snapshot transport (cross-node routing, OQ #4).
- **agentctl RFC 0007** — admission validation ladder: §3.3 the admission-time
  **`override-trifecta`** SAR (string-match-only synthetic *verb on the resource*) —
  this RFC shares the synthetic-verb-in-a-SAR *idea* but differs in mechanism (its
  verbs are *callable connect subresources* on a distinct aggregated GroupVersion,
  §4.4/§5.1) and does **not** resolve RFC 0007 OQ #3 (only informs it); §6 explicitly
  defers runtime-verb RBAC here; §4.3 the webhook HA pattern (§4.5 / OQ #7).
- **agentctl RFC 0008** — node-agent architecture (two tiers): the **management API**
  this RFC fronts; the connection manager holding one attested conn per local pod;
  the discovery loop. This RFC is consistent with that API and adds the access/authz
  in front of it.
- **agentctl RFC 0010** — observability & telemetry bridge: §10 control-plane
  self-observability + the **management-action audit** sink (§5.3.4); the persisted
  run-report store the cold `results` read uses (§2.3).
- **agentctl RFC 0013** — A2A gateway & task store: the **additional** A2A-facing PEP (a
  different auth/durability path, RFC 0013 §4) and the *other* reachability of
  `subagent.send` steering that makes front-only attach gating insufficient (§6.1.2).
- **agentctl RFC 0015** — security & multi-tenancy: the two-PEP trust model, the
  internal mTLS/PKI for the node-agent's two known clients (§5.3.1), the
  **P-attach-gate** requirement home (§6), and the management-action audit
  vocabulary.
- **agentctl RFC 0016** — CLI & kubectl-plugin grammar (**the human client**): the
  `kubectl agent[s]` faces, the cold/live split (§2.3), and the attach UX + lease
  whose *authorization* (not mechanics) this RFC owns (§6.3).

**Contract spec (the reference implementation, agentd RFCs)**

- **agentd RFC 0015 (the reference impl's contract spec)** — management & control
  surface: §4 the operator tools (`drain`/`lame-duck`/`pause`/`resume`/`cancel`)
  this RFC authorizes; §4.5 **`attach` == `subagent.send`** (the gated verb, §6);
  §3.4 `PeerOrigin::Management` gating; §5.2 `surfaces.operator_tools` (the verb set
  is manifest-driven, P-pause); **§7 reachability == operator authority** (the
  premise of this whole RFC); §6 the descriptive downward-API identity the agent
  never re-verifies (the P-meta analogue, §5.3).
- **agentd RFC 0014 (the reference impl's contract spec)** — contract umbrella:
  primitives-not-policy (§3); §8 graceful degradation off `surfaces{}` (drive only
  advertised verbs).
- **agentd RFC 0016 (the reference impl's contract spec)** — telemetry & lifecycle
  contract: `agent://events` (live `logs -f`) backing the `agents/log` streaming-read
  subresource (§5.1). `agent://metrics` (live `top`) is **not yet a defined resource**
  — ask **P4** defines it in agentd RFC 0005/0015 (RFC 0019 references it); cited as an
  unbuilt primitive, not an RFC 0016 resource.
- **agentd RFC 0012 (the reference impl's contract spec)** — security posture: §3.8
  the transport-is-the-boundary, **no-auth-in-the-agent** model that forces all authz
  into agentctl's PEPs.
- **agentd RFC 0010 (the reference impl's contract spec)** — observability/health:
  liveness = supervisor heartbeat, independent of the management connection (why a
  bounced access path is a control gap, not a data-plane outage).
- **agentd RFC 0020 (the reference impl's contract spec)** — A2A over the substrate:
  the gateway-PEP reachability of warm-session steering (§6.1.2).

**Contract asks raised or cited by this RFC** (brainstorm §14): **P-attach-gate**
(per-tool gate within the Management profile so `inject`/`subagent.send` can be
omitted without dropping `drain`/`cancel`/observe — the structural no-puppeting
tier, §6); **P-meta** (descriptive caller/tenant `_meta` the node-agent forwards
and the agent records but never re-verifies, §5.3); **P-audit** (closed-vocabulary
`mgmt.invoked` management-action event, §5.3.4); **P-pause** (the unbuilt
`pause`/`resume` tools, §1); **P4** (`agent://metrics` for the live `top`
streaming-read subresource, §5.1).

*Where this RFC and a contract spec disagree on the wire, the contract wins and
this RFC is corrected; where this RFC needs a primitive the contract does not yet
expose (a per-tool management gate, a forwarded-caller `_meta`, a management-action
audit event), it is a contract ask — never a leak of cluster logic into the agent,
and never auth pushed into a data-plane binary.*
