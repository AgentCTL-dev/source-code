# agentctl RFC 0003: Agent & AgentFleet — the CRD schema & status contract

**Status:** Proposed (agentctl foundational track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the declarative API surface (the CRDs the operator reconciles and mirrors live state into)

> **Contract-first, not agent-first (P0).** An `Agent`/`AgentFleet` describes
> **contract-level intent** — an instruction, a mode, an intelligence binding, a
> set of MCP servers, the surfaces to expose — for **any agent that conforms to
> the Agent Control Contract** (the capabilities manifest, the management MCP
> profile, the frozen metrics/exit-code table, the config schema, the
> downward-API env convention). The CRD MUST NOT encode internals of any one
> data-plane binary. Where this RFC needs a concrete field shape, it cites the
> **reference implementation's** contract spec (agentd RFCs 0014–0020); those are
> *where the contract is presently written down*, not a dependency. A second
> vendor's conformant agent is driven by the same CRD unchanged.

> **KEDA owns `.spec.replicas`.** For any autoscaled object the operator renders
> a workload that **omits** `.spec.replicas` and lets the HPA created by KEDA own
> it (§4, §5). The operator never fights the autoscaler over the replica field.

---

## 1. Problem / Context

agentctl is the Kubernetes control plane for a fleet of conformant agents
(agentctl RFC 0001). Its load-bearing user-facing artifact is the **thing a user
`kubectl apply`s**: a declarative object that names a logical agent and lets the
operator (agentctl RFC 0006) turn it into a running, observable, manageable
workload. That object is the subject of this RFC.

The object must do two jobs at once, and they pull in opposite directions:

1. **Map a logical agent to a rendered workload.** A user says "I want a reactive
   triage agent on the hardened substrate, watching this inbox, with these MCP
   tools, on this model pool" — and the operator must compile that into a Pod
   template, a Job/CronJob/Deployment/StatefulSet (chosen by execution mode), a
   rendered config object, the downward-API env the contract expects, the
   substrate wiring (agentctl RFC 0002), and (for fleets) a KEDA `ScaledObject`.

2. **Mirror live state back.** The same object's `.status` must reflect what the
   running instance(s) actually *are* — the contract version they speak, the
   surfaces they advertise, their health and lifecycle — so `kubectl agents get`,
   GitOps health, and the reconcile loop have one canonical read model.

The tension this RFC resolves: the object must stay **agent-implementation-neutral**
while being **precise enough to render a real workload and validate a real
config**. The resolution is to make the spec a projection of the *contract*
(modes, surfaces, manifest-shaped bindings) rather than a projection of any
binary's flags, and to make the status a **curated projection** of the live
capabilities manifest — never a raw manifest dump (which would couple the CRD to
an implementation's exact document and create etcd write-amplification, agentctl
brainstorm §2.2).

This RFC owns the **shape** of `Agent` and `AgentFleet` (`.spec`, `.status`, the
CEL invariants on the CRD, the mode→workload rendering table, the
additionalPrinterColumns) and the **versioning posture** of the CRD group. It
does **not** own: the reconcile loop and capability cache (agentctl RFC 0006),
the admission webhook implementation (agentctl RFC 0007), the substrate descriptor
(agentctl RFC 0002), the scaling controller / coordination server (agentctl RFC
0011), the deferred ops/dev CRDs (agentctl RFC 0004), or the conversion-webhook
machinery (agentctl RFC 0005). It references and reuses each.

---

## 2. Decision — six points

1. **Two CRDs now: `Agent` and `AgentFleet`.** `Agent` is one logical agent that
   renders, by execution mode, to a Job / CronJob / Deployment / StatefulSet.
   `AgentFleet` is an elastic, autoscaled set of reactive workers sharing one work
   source, with a claim/lease-or-shard ownership policy and an autoscaling target.
   Both are **namespaced**. These are the only CRDs this RFC defines.

2. **Three further CRDs are named but deferred to agentctl RFC 0004.** `AgentClass`
   (cluster-scoped, StorageClass-shaped: the ops bundle — image/contract-version
   range, substrate defaults, default limits/drain/grace, default intelligence
   binding), `IntelligenceService`/`ModelPool` (the ordered intelligence-endpoint
   list as a referenced object), and `MCPServerSet` (reusable MCP-server bundles +
   tags) carry the ops/dev decoupling seam. This RFC reserves the **reference
   fields** that point at them (`classRef`, `intelligenceRef`, `mcp.serverSetRefs`)
   so an `Agent` is forward-compatible, but every such field has an **inline
   equivalent**, so `Agent`/`AgentFleet` are fully usable in v1 *without* RFC 0004.

3. **Group `agents.x-k8s.io`, version `v1alpha1`.** The group noun is deliberately
   **vendor-neutral** ("an agent that conforms to the contract," not "an agentctl
   object") to match P0; the SIG-style `x-k8s.io` suffix signals the intent to
   land these as a neutral, upstreamable API rather than a single vendor's. The
   fallback if an upstream home is not pursued is `agentctl.dev`. **The group
   string is an open question** (§Open questions), but the *shape* below is
   independent of which string wins. Single served version `v1alpha1` (§8).

4. **The spec is contract-shaped, the status is a curated projection.** Every
   `.spec` field maps to a **contract concept** (mode, instruction/config,
   intelligence binding, MCP servers + trifecta/capability tags, limits, surfaces,
   substrate) — never to a private flag of one binary. `.status` mirrors only
   **stable, structural** facts distilled from the live capabilities manifest +
   inventory + health (§6), plus a conditions taxonomy. Churny per-run telemetry
   (token counts, backlog, active-subagent counts) stays in metrics and on-demand
   `kubectl agents describe`, **never** in `.status`.

5. **CEL guards single-object invariants; an admission webhook is mandatory for
   the rest.** The CRD carries `x-kubernetes-validations` CEL for everything
   expressible per-object (mode↔work-source coherence, drain<grace, claim XOR
   shard, mode immutability, instruction XOR ref). Cross-object and
   schema-semantic checks (trifecta-tag union, MCP-server name collision,
   config-schema validation) **cannot** be CEL and require the webhook (agentctl
   RFC 0007). "CEL, no webhook" is false advertising (§7).

6. **One served version + conversion webhook + StorageVersionMigration; CRD
   version is decoupled from the contract version.** Never a silent additive
   `apiVersion` ladder under `conversion: None` (it prunes unknown fields on
   round-trip — §8). The agent's `contract_version` negotiation lives in `.status`
   and the `AgentClass`, and versions **independently** of the CRD `apiVersion`
   (agentctl RFC 0005).

---

## 3. The `Agent` resource — `.spec`

An `Agent` is one logical agent. Its `.spec` is a flat-ish projection of the
contract: each block corresponds to a contract surface the operator renders into
a Pod template + a config object + substrate wiring.

### 3.1 Field reference

| Field | Type | Req | Contract anchor | Notes |
|---|---|---|---|---|
| `classRef` | `{name}` | cond. | — (agentctl RFC 0004) | `AgentClass`; spec fields below **override** class defaults. Reserved; inline works without it. **Mutually exclusive with `image`** (CEL): when set, the class owns the image + contract pin (RFC 0004 §3.4). |
| `image` | string (digest/tag) | cond. | — | the conformant agent image to run. **Required iff no `classRef`; forbidden when `classRef` is set** (CEL `has(classRef) != has(image)`). A fully-inline (classless) `Agent` names its own image here, so it renders without any RFC 0004 object. Resolve to a digest before probing (RFC 0006 §5.3). |
| `imagePullPolicy` / `imagePullSecrets` | enum / `[]{name}` | no | — | standard pod image-pull controls for the inline-image case; ignored when `classRef` supplies them. |
| `mode` | enum `once\|loop\|reactive\|schedule` | **yes** | RFC 0008 §2 (modes) | **immutable** (CEL) — the workload *Kind* is a function of mode (§5). |
| `instruction` | `{inline?\|configMapRef?}` | cond. | RFC 0008 (mode↔required field) | XOR (CEL). Required for `once`/`loop`/`schedule`; optional for `reactive` (templated per event). |
| `config` | `{inline?\|configMapRef?}` | no | RFC 0017 §3 (config file) | the **declarative config file** (structural only — MCP servers, subscriptions, limits, model/intel params). Never secrets. Rendered to a ConfigMap (agentctl RFC 0006 partitions reloadable vs restart-only). |
| `intelligence` / `intelligenceRef` | object / `{name}` | no | RFC 0018 (multi-endpoint) | inline ordered endpoint list **or** a ref to `IntelligenceService`/`ModelPool` (RFC 0004). Operator resolves to the contract's `--intelligence a,b,…` + per-endpoint token env (RFC 0014 §6.4). |
| `model` | string | no | manifest `model` (RFC 0015 §5.2) | configured model id; may be supplied by the pool instead. |
| `mcp` | `{serverSetRefs[]?, servers[]?}` | no | RFC 0017 §3.3, RFC 0012 §3.1 | inline servers and/or referenced `MCPServerSet`s. Refs+inline **ADD** (mirrors the contract's `--mcp` deviation). Each server carries per-tool **glob `tags`** — the canonical shape is a map of *tool-name-glob → legs* (`{ "*": [untrusted_input], "read_*": […] }`, first-match/longest-glob-wins, agentd RFC 0012 §3.1); a bare list `tags: [legs]` is accepted **shorthand** for `{ "*": [legs] }`. The richer map form is defined in agentctl RFC 0004 §5; legs are `untrusted_input\|sensitive\|egress`. |
| `subscribe` | `[]string` | cond. | RFC 0008 (reactive routing) | the work source(s) — MCP resource URIs. **Required iff `reactive`** (CEL); forbidden otherwise. |
| `schedule` | `{cron, timezone?}` | cond. | agentd RFC 0008 §3.1.4 (schedule mode; internal cron is UTC-only) | the CronJob cadence (§5.2). **Required iff `mode == schedule`** (CEL); forbidden otherwise. `cron` is a 5-field expression; `timezone` defaults to `UTC` (the contract's internal cron is UTC-only — a non-UTC `timezone` is honoured by the k8s CronJob clock, which is the one clock §5.2 keeps). Mirrors the `subscribe`↔`reactive` treatment. |
| `limits` | object (sparse) | no | manifest `limits` (RFC 0015 §5.2) | sparse override of class/contract limits (`maxSteps`, `maxDepth`, `maxTokens`, `treeTokenBudget`, `maxTotalSubagents`, `deadlineSeconds`). |
| `drain` | `{timeoutSeconds}` | no | RFC 0011 §4.2 (drain) | graceful-drain budget. CEL: `drain.timeoutSeconds < podGraceSeconds`. |
| `podGraceSeconds` | int | no | RFC 0011 §3.3 | rendered to `terminationGracePeriodSeconds` **and** injected as the contract's grace env (§9). |
| `surfaces` | object | no | manifest `surfaces{}` (RFC 0014 §6.2) | which surfaces the operator should **enable** (`management`, `metrics`, `events`, `a2a`). Intent, not advertisement — `.status` mirrors what the agent actually advertised. |
| `substrate` | `{tier, runtimeClassName?}` | no | agentctl RFC 0002 | substrate **selection**: `stock-unix` (primary/dev) \| `kata-hybrid` (hardened; default for multi-tenant) \| `sidecar-emptydir` (portable). Usually defaulted via `AgentClass`. |
| `security` | `{allowTrifecta, enableExec, attachPolicy}` | no | RFC 0012 §3.2/§3.6 | `allowTrifecta` (default `false`) gates the lethal-trifecta override; `enableExec` (default `false`); `attachPolicy: deny\|readOnly\|steer` (agentctl PEP policy, agentctl RFC 0009). |

**Why these and not the binary's flags.** The spec names *concepts the contract
freezes* — `mode` (RFC 0008), the `surfaces{}` keys (RFC 0014 §6.2), the
trifecta tag vocabulary (RFC 0012 §3.1), the limits box (RFC 0015 §5.2) — each of
which a second conformant agent must also honour. The operator's renderer
(agentctl RFC 0006) is the only place that knows how a *particular* image spells a
concept (a flag, a config key, an env var); the CRD stays one level above that.

### 3.2 Substrate-assigned addresses are NOT in the spec

`spec.surfaces.management`/`metrics`/`a2a` are **booleans (enable/disable)**, never
literal addresses. The served address (`unix:PATH` on the stock tier, `vsock:PORT`
on the hardened tier) is **assigned per-pod by the node-agent / substrate**
(agentctl RFC 0002): CID/port allocation is the node-agent's job, not a user's
literal in the CR (agentd RFC 0015 §10 open item). The manifest's
`surfaces.management` value (`false | "vsock:PORT" | "unix:PATH"`) is reported by
the *agent* and surfaces in `.status` (§6), never copied into `.spec`.

### 3.3 `surfaces.a2a` is gated on a contract ask

`spec.surfaces.a2a` is accepted in the schema but is **inert until the contract
adds `surfaces.a2a`** to the frozen manifest — it is referenced by the reference
impl's A2A binding (agentd RFC 0020) but **not listed** in the frozen manifest
schema (agentd RFC 0015 §5.2). This is the contract ask **P2** (a new manifest
key + a commitment to specific A2A wire-method strings). Until P2 lands and
agentctl RFC 0013 ships the A2A gateway, the webhook sets condition
`A2AUnsupported` and the operator does not wire an A2A surface. **Contract ask: P2.**

### 3.4 Example — a reactive `Agent` (Deployment)

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata:
  name: triage
  namespace: agents
spec:
  classRef: { name: standard }          # AgentClass (RFC 0004); fields below override its defaults
  mode: reactive                         # immutable; renders a Deployment (§5)
  instruction:                           # optional for reactive — here a template
    configMapRef: { name: triage-instruction }
  config:
    configMapRef: { name: triage-config }   # the RFC 0017 declarative config file (structural only)
  intelligenceRef: { name: anthropic-pool }  # ordered endpoint list (RFC 0004 / RFC 0018)
  model: claude-opus-4
  mcp:
    serverSetRefs: [core-readers]        # reusable MCPServerSet (RFC 0004)
    servers:                             # inline ADDITIONS (refs + inline compose)
      - name: mailer
        tags: [egress]
  subscribe:                             # REQUIRED for reactive (CEL)
    - "fs:file:///watch/inbox/*.json"
  limits: { maxSteps: 200 }              # sparse override
  drain: { timeoutSeconds: 25 }
  podGraceSeconds: 30                     # CEL: drain.timeoutSeconds < podGraceSeconds
  surfaces:
    management: true
    metrics: true
    events: true
    a2a: false                           # gated on contract ask P2 + agentctl RFC 0013
  substrate: { tier: kata-hybrid }        # hardened tier (RFC 0002); default for multi-tenant
  security:
    allowTrifecta: false
    enableExec: false
    attachPolicy: readOnly
```

---

## 4. The `AgentFleet` resource — `.spec`

`AgentFleet` is the elastic, autoscaled form: N reactive workers sharing **one
work source**, where cross-instance ownership is solved by a claim/lease *or* a
static shard (agentd RFC 0019), and the replica count is driven by an autoscaling
signal. It embeds an `Agent` template (forced to `mode: reactive`) plus the
fleet-only blocks.

### 4.1 Field reference

| Field | Type | Req | Contract anchor | Notes |
|---|---|---|---|---|
| `template` | `AgentSpec` (mode pinned `reactive`) | **yes** | §3 | the per-replica `Agent` spec; CEL pins `template.mode == "reactive"`. |
| `scaling.mode` | enum `claim\|shard` | **yes** | RFC 0019 §2 | the ownership regime (§4.2). Exactly one (CEL). |
| `scaling.min` | int (≥0) | cond. | RFC 0019 §5 | **claim mode only**; floor; `0` enables scale-from-zero. Unset in shard mode. |
| `scaling.max` | int (>min) | cond. | RFC 0019 §5 | **claim mode only**; the KEDA replica ceiling (CEL `max > min`). Unset in shard mode. |
| `scaling.shards` | int (≥1) | cond. | RFC 0019 §4.1 | **shard mode only**; the partition count `N` — the FNV-1a/64 modulus **and** the StatefulSet `.spec.replicas` (§4.3). `N` is a partition count, **not** a KEDA replica range; overloading `min`/`max` to mean `N` would be a category error. |
| `scaling.target` | `{signal, threshold, activationThreshold?}` | cond. | RFC 0019 §5.1 (signal set) | the autoscaling target → a KEDA trigger (§4.3); required in claim mode, optional/inert in shard mode (used only if claim is layered on a shard fleet). |
| `work.source` | `{mcp, uri}` | **yes** | RFC 0008 (subscription) | the shared subscribed MCP resource every replica watches. |
| `work.claim` | `{server, style, ttlSeconds?, key?}` | cond. | RFC 0019 §3 | required iff `scaling.mode: claim`; the coordination MCP server + lease params. |
| `work.shardKey` | string (expr) | no | RFC 0019 §4.1 | the stable key the shard predicate hashes; defaults to the resource URI. |
| `drain` | `{timeoutSeconds}` | no | RFC 0019 §6 (claim-release) | per-replica drain; on scale-down the pod's SIGTERM path releases claims. |
| `podGraceSeconds` | int | no | RFC 0019 §5.2 | **MUST exceed** `drain.timeoutSeconds` (CEL) or a scale-down SIGKILL leaks a held claim until its TTL. |

### 4.2 Two ownership regimes (claim vs shard)

A single topology cannot serve both KEDA jiggling `.spec.replicas` continuously
*and* `--shard K/N` requiring `N` to be fleet-consistent immutable config (agent
RFC 0019 Decision 4). So `scaling.mode` selects the regime:

- **`claim` (recommended default, the only elastic regime).** Every pod is shard
  `0/1`; cross-instance ownership is the work-claim lease alone (agentd RFC 0019
  §3). KEDA owns `.spec.replicas` freely; new pods join the claim race; scale-down
  pods drain → release. Renders to a **Deployment** (fungible) with
  `controller.kubernetes.io/pod-deletion-cost` driven from per-pod load so the
  autoscaler removes the least-loaded victim (a StatefulSet can only delete the
  highest ordinal).

- **`shard` (fixed partition, NOT KEDA-elastic).** `N` (`scaling.shards`) is a
  deliberate operator-chosen partition count; an `N`-change is an operator-driven
  `shard-resize` rolling restart (drain → release → restart with new `K/N`), and
  KEDA is paused/handed-off for that fleet. Renders to a **StatefulSet** (stable
  ordinal = `K`, `.spec.replicas == N`). Claim MAY be layered on top for the
  rebalance seam. Shard mode does **not** use `scaling.min`/`scaling.max` — those
  are the claim-mode KEDA range.

### 4.3 KEDA owns `.spec.replicas` — the rendered workload OMITS it

For **claim** mode the rendered Deployment **MUST NOT set `.spec.replicas`**: the
operator server-side-applies the workload without that field, and the KEDA
`ScaledObject` (and its generated HPA) owns it. If the operator wrote `replicas`,
it and the HPA would fight on every reconcile (a status hot-loop + scale churn).
For **shard** mode the replica count *is* `N` (`scaling.shards`) and is
operator-owned; KEDA is paused for the resize window — by pinning the **rendered
`ScaledObject`'s `minReplicaCount == maxReplicaCount == N`** (a KEDA-level
artifact, distinct from the CRD's claim-only `scaling.min`/`scaling.max`), or by
removing the `ScaledObject` — so the same no-fight rule holds: there is exactly
one writer of the replica field at any time. The autoscaling signal and KEDA trigger details are owned by agentctl
RFC 0011; this RFC only fixes the **field-ownership contract** (one writer).

Scale-from-zero (`scaling.min: 0`) reads an **off-pod backlog** from the
coordination server, not a per-pod metric — at replica=0 no pod emits
`reactive_backlog`. That off-pod signal is a contract ask (**P9**) and lives in
agentctl RFC 0011; the CRD only records the target. The autoscaling signal **name
set** itself is unreconciled between the reference impl's frozen metrics (agent
RFC 0016) and its scaling RFC (agentd RFC 0019 §5) — a contract FIX (**P10**); the
CRD validates `scaling.target.signal` against an allowed enum the webhook keys to
the negotiated `metrics_schema`, not a hard-coded literal. The token in
`scaling.target.signal` is therefore **intentionally un-prefixed and
contract-neutral** (e.g. `reactive_backlog`, *not* the reference impl's
`agent_reactive_backlog`): the webhook maps the neutral token onto the negotiated
`metrics_schema`'s actual (possibly `agent_`-prefixed) metric name, so the CRD
never bakes in a vendor prefix. This is deliberate P0 neutralization, not a
transcription slip (the brainstorm §11.2 flags hand-transcribed metric names as
exactly the hazard codegen exists to prevent).

### 4.4 Example — see §10.3.

---

## 5. Mode → workload rendering

The operator (agentctl RFC 0006) renders by `mode`. This is **why `mode` is
immutable**: you cannot mutate a Job into a Deployment into a StatefulSet — the
workload *Kind* changes (agentctl brainstorm §2.2 corrected the rationale; it is
the Kind change, not the contract's restart-only partition).

| `mode` / kind | Rendered workload | Strategy / policy | Trap avoided |
|---|---|---|---|
| `once` | **Job** (`restartPolicy: Never`) | mechanically compiled `podFailurePolicy` from the exit-code contract (RFC 0011 §5 / RFC 0016) | retry-on-runaway: `FailJob` for non-retriable codes |
| `schedule` | **CronJob** of `mode=once` Jobs | `concurrencyPolicy: Forbid`; **one clock** | **double-schedule** (below) |
| `loop` | **Deployment** (`replicas: 1`) or Job-with-deadline | `strategy: Recreate` | bounded by deadline/budget (RFC 0008) |
| `reactive` (singleton) | **Deployment** `strategy: Recreate` (or single-replica StatefulSet) | **at-most-one** | **double-processing** (below) |
| `AgentFleet` claim | **Deployment** (fungible) + KEDA `ScaledObject` | `.spec.replicas` **omitted** (§4.3) | replica-field fight; victim selection via pod-deletion-cost |
| `AgentFleet` shard | **StatefulSet** (ordinal = `K`) + KEDA paused for resize | `.spec.replicas` operator-owned | two-owner on resize → drain-and-reassign (RFC 0019 §4.3) |

### 5.1 The double-processing trap (claim/lease)

A `reactive` singleton rendered as a default-`RollingUpdate` Deployment with
`maxSurge: 1` **briefly runs two reactive pods on the same source** during a
rollout. Each is, by the contract's intra-instance rule, an exactly-one-owner
(agentd RFC 0008 §2.2) — so the same `updated{uri}` is processed **twice** for the
overlap window (agentd RFC 0019 §1). Rendering avoids this two ways:

1. **Singleton:** force `strategy: Recreate` (or a single-replica StatefulSet),
   so the old pod terminates before the new pod starts — at-most-one on the
   source.
2. **Fleet:** any `AgentFleet` (claim or shard) has cross-instance ownership by
   construction (the lease or the shard predicate), so a transient two-pod overlap
   is resolved by `work.claim` granting to exactly one (agentd RFC 0019 §3.2 — the
   claim grant/LOST lifecycle; §4.4 is the shard config surface, not the grant).

A `reactive` singleton with non-idempotent side effects therefore renders
`Recreate` by default; the webhook warns if a singleton's resolved rollout
strategy is `RollingUpdate` — a singleton has no claim/shard cross-instance
ownership (that is an `AgentFleet` concept, §4.2), so a rolling overlap
double-processes the source (agentctl RFC 0007 B7).

### 5.2 The double-schedule trap (CronJob concurrencyPolicy)

`schedule` mode has **two candidate clocks**: the Kubernetes CronJob, and the
agent's *own* internal cron (agentd RFC 0008 §2.7). Running both fires the work
twice per tick. Rendering picks **exactly one clock**: the k8s CronJob drives a
`mode=once` Job per fire, and the rendered config **disables the agent's internal
cron** (agentd RFC 0008 makes them mutually exclusive). The CronJob is rendered
with **`concurrencyPolicy: Forbid`** so a slow run cannot overlap its own next
fire (a second, in-cluster double-schedule). `startingDeadlineSeconds` is set so a
missed fire is bounded, not infinitely backfilled.

### 5.3 Probes — exec for networkless pods

**The default probe on *every* tier is an `exec` probe** reading the contract's
health state (agentd RFC 0010 §3.7: the `--health-file` read by an exec probe is
the *default* daemon health surface; the reference impl ships **no HTTP server** by
default, agentd RFC 0015 §1). On any networkless substrate (the hardened/portable
tiers, agentctl RFC 0002), `exec` is the **only** option anyway — kubelet
`httpGet`/`tcpSocket`/`grpc` probes cannot reach a pod with no network; only `exec`
traverses CRI. Because a scratch image has no shell to `cat` a health file, the
exec probe must invoke a contract **health verb** the agent binary answers itself —
a contract ask (**P1**, an exec-readable health check); until P1 lands, the
networkless tiers fall back to liveness-via-supervisor-heartbeat only. An `httpGet`
`/readyz` probe is rendered **only as an opt-in**, when the agent advertises an HTTP
health surface (the reference impl's `--health-http ADDR`, agentd RFC 0010 §3.7
item 4) — never as the default, since a conformant agent need not serve HTTP at
all. **Contract ask: P1.**

---

## 6. The status subresource

`.status` is a **curated projection** — the stable, structural facts agentctl
needs for `kubectl agents get`, GitOps health, and reconcile — distilled from the
live capabilities manifest (agentd RFC 0015 §5.2), `agent://inventory`/`status`
(agentd RFC 0015 §5.3/§5.4), and workload status. It is **not** a raw manifest
dump: a raw dump would (a) couple the CRD to one implementation's exact document,
(b) churn etcd on every token/inventory tick, and (c) trigger watch storms
(agentctl brainstorm §2.2). The single-writer is the operator; it patches
`.status` only on a `DeepEqual` change (agentctl RFC 0006).

### 6.1 Shape

```jsonc
// Agent.status — curated, structural-only
{
  "observedGeneration": 7,                 // the spec generation this status reflects
  "phase": "Running",                       // Pending|Progressing|Running|Draining|Degraded|Failed (derived from conditions)

  "contract": {                             // negotiated from the manifest (RFC 0015 §5.2)
    "version": "1.0",                       // contract_version (NOT the CRD apiVersion — §8)
    "agentVersion": "2.2.0",               // implementation build id (agent_version for the ref impl)
    "compatible": true                      // agentctl understands this major
  },
  "buildFeatures": ["serve-mcp","vsock","metrics","events","hot-reload"], // OPAQUE, agent-defined; informational only — see note below

  "surfaces": {                             // what the agent ACTUALLY advertised (vs spec intent)
    "management": "vsock:5005",             // false | "vsock:PORT" | "unix:PATH"  (substrate-assigned)
    "metrics": ":9090",
    "events": true,
    "a2a": false                            // false until contract ask P2
  },

  "identity": {                             // downward-API identity, echoed by the agent (RFC 0015 §5.4)
    "instance": "triage-7d9f-abc", "uid": "f3c1-…", "node": "node-3", "namespace": "agents"
  },

  "health": { "ready": true, "draining": false, "paused": false, "lameDuck": false },

  "workload": { "kind": "Deployment", "name": "triage", "ready": 1, "desired": 1 },

  "conditions": [
    { "type": "Validated",          "status": "True", "reason": "AdmissionPassed" },
    { "type": "ContractCompatible", "status": "True", "reason": "MajorUnderstood" },
    { "type": "Rendered",           "status": "True", "reason": "WorkloadApplied" },
    { "type": "Ready",              "status": "True", "reason": "SubscriptionsReconciled" }
  ]
}
```

**`buildFeatures` is OPAQUE and MUST NOT be branched on (P0).** It mirrors the
manifest's `build_features`, which agentd RFC 0015 §5.2 defines as "compiled-in
cargo features" — a Rust/cargo-specific vocabulary a second conformant (non-Rust)
agent does not have. agentctl therefore treats the *values* as **agent-defined,
informational strings** (surfaced for humans/audit only). **Capability discovery
keys exclusively off `surfaces{}`** — the contract's single discovery point (agent
RFC 0014 §6.2) — **never off `buildFeatures`**. The example values
(`serve-mcp`/`vsock`/…) are the reference impl's spelling and MUST NOT train the
operator to branch on agent cargo strings; an operator that needs to know whether
management/metrics/events/a2a are live reads `.status.surfaces`, not
`.status.buildFeatures`.

**Explicitly NOT in `.status`** (served via metrics + `kubectl agents
describe`/`top` instead): `tokensIn/Out`, `activeSubagents`, fleet `backlog`,
`configGeneration`, any per-run counter. These churn and would write-amplify etcd.
Reference-count fields (`inUseBy`) are replaced by finalizer logic that enumerates
actual referrers (agentctl RFC 0006), not an integer in status.

### 6.2 Conditions taxonomy

Standard `metav1.Condition` (type, status, reason, message, lastTransitionTime,
observedGeneration). The taxonomy, in reconcile order:

| Condition | True when | Notable reasons |
|---|---|---|
| `Validated` | admission (CEL + webhook, §7) accepted the spec | `AdmissionPassed`, `ConfigSchemaValid`; `ConfigSchemaDeferred` (rung C cache-miss — config validated at runtime by the init-container, agentctl RFC 0007 rung D) |
| `ContractCompatible` | the negotiated `contract_version` major is understood | `MajorUnderstood`; False → `MajorUnknown` (degrade to liveness/exit-code mgmt) |
| `Rendered` | the workload + config objects were server-side-applied | `WorkloadApplied`; False → `RenderError` |
| `Ready` | the rollout is complete, management is reachable, subscriptions reconciled | `SubscriptionsReconciled`; False → `RolloutProgressing`, `ManagementUnreachable`, `AttestationFailed` (pod→socket attestation failed, agentctl RFC 0002 §7) |
| `Draining` | a drain is in progress (manifest `draining:true` or scale-down) | `DrainRequested`, `ScaleDown` |
| `Degraded` | a partial/unhealthy state the operator wants surfaced without `phase: Failed` | `IntelligenceUnhealthy`, `A2AUnsupported`, `ConfigValidatedAtRuntimeFailed` |
| `TrifectaUnionObserved` | (advisory) the resolved MCP tag union spans the full lethal trifecta — admission **warned, not blocked** (agentctl RFC 0007 §3); the agent's per-spawn Rule-of-Two is the live control | `TrifectaUnion` |

Each advisory is its **own `metav1.Condition`** with its own single `reason` —
the trifecta advisory is the standalone `TrifectaUnionObserved` condition and the
config-schema-deferred state is a `reason` on `Validated`; they are **never**
crammed into one condition's `reason`. So an `Agent` that passed admission,
deferred its config schema, and shows a trifecta union is representable as three
independent conditions (`Validated=True/ConfigSchemaDeferred`,
`TrifectaUnionObserved=True/TrifectaUnion`, and — if A2A is requested pre-P2 —
`Degraded=True/A2AUnsupported`), with no single-`reason` collision.

`phase` is a **derived** convenience string for humans, computed from the
conditions; conditions are the source of truth (GitOps tools read both).

`AgentFleet.status` adds `replicas` (`desired`/`ready`/`updated`) and the
`scaling` projection (`mode`, `currentMin`/`currentMax`, `lastScaleTime`) but
keeps the **same no-churn rule** — backlog and per-pod load stay in metrics.

### 6.3 additionalPrinterColumns (`kubectl agents get [-o wide]`)

```yaml
additionalPrinterColumns:
  - { name: Mode,      type: string, jsonPath: .spec.mode }
  - { name: Contract,  type: string, jsonPath: .status.contract.version }
  - { name: Ready,     type: string, jsonPath: .status.conditions[?(@.type=="Ready")].status }
  - { name: Phase,     type: string, jsonPath: .status.phase }
  - { name: Age,       type: date,   jsonPath: .metadata.creationTimestamp }
  # -o wide:
  - { name: Model,     type: string, jsonPath: .spec.model,                   priority: 1 }
  - { name: Build,     type: string, jsonPath: .status.contract.agentVersion, priority: 1 }
  - { name: Substrate, type: string, jsonPath: .spec.substrate.tier,          priority: 1 }
  - { name: Node,      type: string, jsonPath: .status.identity.node,         priority: 1 }
```

The `Model` column reads **`.spec.model`** (the only place a model id is recorded —
`.status` carries no model field); the implementation build id is a separate `Build`
column off `.status.contract.agentVersion`. (Earlier drafts mistakenly pointed
`Model` at `agentVersion`, which printed the build under a "Model" heading.) For
`AgentFleet`, the `Ready` column becomes `READY` as `ready/desired` and a `SCALE`
column shows `min..max`; that fleet-specific printer set is **illustrative here and
defined with the AgentFleet status projection (§6.2)**, not repeated as a second
YAML block.

---

## 7. Validation — what CEL can do, and why the webhook is still mandatory

Validation is a **ladder** (agentctl RFC 0007 owns the full ladder; this RFC owns
the CRD-resident rung). CEL `x-kubernetes-validations` on the CRD handle every
**single-object** invariant, in the apiserver, with no extra hop:

```yaml
# x-kubernetes-validations on Agent — a NON-EXHAUSTIVE sample of the single-object
# invariants. The complete set ships with the CRD; the full validation ladder
# (CEL + webhook) is owned by agentctl RFC 0007.
# --- mode <-> work-source coherence ---
- rule: "self.spec.mode != 'reactive' || (has(self.spec.subscribe) && self.spec.subscribe.size() > 0)"
  message: "reactive mode requires at least one subscribe source"
- rule: "self.spec.mode == 'reactive' || !has(self.spec.subscribe)"
  message: "subscribe is only valid for reactive mode"
# --- schedule field <-> schedule mode (mirrors subscribe<->reactive) ---
- rule: "self.spec.mode != 'schedule' || has(self.spec.schedule)"
  message: "schedule mode requires spec.schedule.cron"
- rule: "self.spec.mode == 'schedule' || !has(self.spec.schedule)"
  message: "spec.schedule is only valid for schedule mode"
# --- drain budget ---
- rule: "!has(self.spec.drain) || !has(self.spec.podGraceSeconds) || self.spec.drain.timeoutSeconds < self.spec.podGraceSeconds"
  message: "drain.timeoutSeconds must be less than podGraceSeconds"
# --- instruction: required for non-reactive; XOR when present (presence-guarded so
#     a legal reactive Agent that omits instruction is NOT rejected) ---
- rule: "self.spec.mode == 'reactive' || has(self.spec.instruction)"
  message: "instruction is required for once/loop/schedule modes"
- rule: "!has(self.spec.instruction) || (has(self.spec.instruction.inline) != has(self.spec.instruction.configMapRef))"
  message: "instruction must be exactly one of inline or configMapRef"
# --- image vs class: exactly one provides the agent image (the class owns it when referenced) ---
- rule: "has(self.spec.classRef) != has(self.spec.image)"
  message: "exactly one of classRef or image must be set (the AgentClass owns the image when referenced; a classless Agent names its own image)"
# --- intelligence binding: inline ordered list XOR ref (endpoint order IS the failover policy; never merged, §3.1 / RFC 0004 §4.5) ---
- rule: "!has(self.spec.intelligence) || !has(self.spec.intelligenceRef)"
  message: "intelligence (inline ordered list) and intelligenceRef are mutually exclusive — order is the failover policy and is replaced, never merged"
# --- immutability (transition rule) ---
- rule: "self.spec.mode == oldSelf.spec.mode"
  message: "mode is immutable (the rendered workload Kind depends on it)"
# --- AgentFleet (scaling.mode is already an enum claim|shard, so no XOR rule needed) ---
# claim mode = a KEDA replica range (max > min); shard mode = a fixed partition count N (scaling.shards).
# N is a partition count, NOT a replica ceiling — overloading min/max to mean N is a category error.
- rule: "self.spec.scaling.mode != 'claim' || (has(self.spec.scaling.max) && self.spec.scaling.max > self.spec.scaling.min)"
  message: "claim mode requires scaling.max with max > min (the KEDA replica range)"
- rule: "self.spec.scaling.mode != 'shard' || (has(self.spec.scaling.shards) && !has(self.spec.scaling.max))"
  message: "shard mode requires scaling.shards (the partition count N) and must NOT set the claim-only scaling.min/max range"
- rule: "!has(self.spec.scaling.min) || self.spec.scaling.min != 0 || self.spec.scaling.mode == 'claim'"
  message: "scale-from-zero (scaling.min: 0) is only valid in claim mode"
- rule: "self.spec.template.mode == 'reactive'"
  message: "AgentFleet template must be reactive"
- rule: "has(self.spec.template.classRef) != has(self.spec.template.image)"
  message: "AgentFleet template: exactly one of classRef or image must be set (same rule as Agent)"
```

**Three classes of check are NOT expressible in CEL and force a validating
admission webhook** (so "CEL is enough, no webhook" is false):

1. **Trifecta-tag union across objects.** Two individually-"safe" `MCPServerSet`s
   (RFC 0004) can compose the full lethal trifecta (`untrusted_input` +
   `sensitive` + `egress`, agentd RFC 0012 §3.1) on one `Agent` once
   `serverSetRefs` + inline `servers` are unioned. CEL cannot fetch and union
   referenced objects. The webhook computes the union and checks it against
   `security.allowTrifecta`. **Per the brainstorm §2.2 correction this check is
   advisory/observational, not blocking** — the contract already enforces
   Rule-of-Two per-spawn over each child's narrowed grant (agentd RFC 0012 §3.2),
   and the canonical safe pattern is a reader/actor split that a naive blocking
   union would refuse. The *real* control is gating the `allowTrifecta` override
   behind elevated RBAC + audit (agentctl RFC 0007 / RFC 0015).

2. **MCP-server name collision across `serverSetRefs` + inline.** A duplicate
   server name between a referenced set and an inline addition is ambiguous; CEL
   cannot dereference the sets to detect it. The webhook resolves and rejects.

3. **Config-schema validation.** The rendered config file must validate against
   the **contract's config JSON Schema** (agentd RFC 0017 §4.2 `--config-schema`),
   which CEL cannot express. Under P0 the webhook validates against the **published
   schema** keyed to the image's negotiated contract major — agentctl **never
   links a data-plane crate** and **never execs a tenant image synchronously in
   the webhook** (image-pull on the apply path; running tenant binaries in the
   control plane). Ground-truth validation is an **init-container running the exact
   target image** (`--validate-config`, agentd RFC 0017 §4.1) whose failure
   surfaces as a runtime `ConfigValidatedAtRuntimeFailed` condition. Both
   `--config-schema` and `--validate-config` are unbuilt in the reference impl
   today (**contract ask P6**).

The webhook must also be wired fail-closed *without* stranding the operator's own
finalizer writes (the operator ServiceAccount is exempted via a `matchCondition`);
that wiring is agentctl RFC 0007.

---

## 8. Versioning & conversion

**One served version at a time + a conversion webhook + `StorageVersionMigration`.
Never an additive `apiVersion` ladder under `conversion: None`.** This is a
Kubernetes correctness point, not a preference:

- With `conversion: None`, the apiserver serves the **stored bytes** for every
  served version, and **structural-schema pruning silently drops fields not in the
  requested version's schema** on a round-trip. A `v1alpha1`→`v1beta1` "additive"
  ladder under `None` therefore silently corrupts objects when a client GETs at
  one version and PUTs at another.
- Therefore: ship **`v1alpha1` single-served**, absorb churn **additively within
  `v1alpha1`** while it is alpha, and bump to a new version **only** with a
  conversion webhook + a `StorageVersionMigration` to rewrite stored objects.

```
v1alpha1 (single served, single stored)
   │   additive-within-alpha churn (new optional fields only)
   ▼
v1beta1  ── conversion webhook (round-trip lossless) ── StorageVersionMigration
   │
   ▼
v1       ── same machinery ──
```

**The CRD version ladder is DECOUPLED from the contract version.** The agent's
`contract_version` (agentd RFC 0014 §6.3) is negotiated at runtime, lives in
`.status.contract.version` and in the `AgentClass` `contractVersionRange`, and
moves on the **contract's** additive-minor / breaking-major rule — entirely
independently of the CRD `apiVersion`. A new `contract_version` minor is **not** a
CRD version bump; a CRD field addition is **not** a contract change. Conflating
them would force a CRD migration every time the data plane ships a feature. The
full conversion/graduation policy (alpha→beta→GA, the SVM choreography, the
thin-conversion-webhook tooling the Rust/kube-rs stack must own) is agentctl RFC
0005; this RFC only fixes the **posture** (single-served + webhook + SVM, decoupled
from the contract).

---

## 9. Downward-API env injection & the shard defect

The operator injects the contract's **downward-API env convention** (agentd RFC
0014 §6.4 / RFC 0015 §6) into every rendered Pod. These are read by the agent
**env-only** — a conformant agent never calls the kube API:

```yaml
# rendered into every agent Pod (the contract's env convention; AGENT_* is the
# reference impl's current spelling of the contract token — see open question)
env:
  - { name: AGENT_POD_NAME,      valueFrom: { fieldRef: { fieldPath: metadata.name } } }
  - { name: AGENT_POD_UID,       valueFrom: { fieldRef: { fieldPath: metadata.uid } } }
  - { name: AGENT_POD_NAMESPACE, valueFrom: { fieldRef: { fieldPath: metadata.namespace } } }
  - { name: AGENT_NODE_NAME,     valueFrom: { fieldRef: { fieldPath: spec.nodeName } } }
  - { name: AGENT_POD_GRACE_SECONDS, value: "30" }   # == terminationGracePeriodSeconds (drain<grace)
  # per-endpoint intelligence credentials ride AGENT_INTELLIGENCE_TOKEN[_n] / _FILE (RFC 0014 §6.4)
```

These are **descriptive, never load-bearing** (agentd RFC 0015 §6): the agent uses
them for correlation/labelling and surfaces them in `.status.identity`; it makes no
placement decision from them.

The `AGENT_*` prefix is the **deepest residual coupling in this RFC** — it is the
reference impl's spelling of the contract token (agentd RFC 0014 §6.4), so injecting
it is P0-clean only because the *contract itself* currently names the env family
`AGENT_*`. **Normative commitment:** before GA this MUST be resolved one of two
ways (Open Question 2) — either the contract **freezes `AGENT_*` as the stable,
vendor-neutral contract token** (awkwardly named but stable), or the neutral
Agent-Control-Contract extraction (P0, agentctl RFC 0001) **defines a vendor-neutral
prefix** (e.g. `AGENT_*`) with a compatibility window. The operator's injection set
is keyed to whichever the contract commits; agentctl MUST NOT ship GA with an
unresolved prefix.

### 9.1 The `AGENT_SHARD` defect (a contract ask, not an agentctl bug)

The contract's env convention lists `AGENT_SHARD="K/N"` (agentd RFC 0014 §6.4),
and the scaling RFC derives `K` from the StatefulSet ordinal and `N` from
`.spec.replicas` (agentd RFC 0019 §4.2). **This is unimplementable from a single
StatefulSet pod template**: a Pod template's `env` is **identical across all
ordinals** — every replica would receive the *same* `AGENT_SHARD` string — and
the downward API exposes `metadata.name` (which contains the ordinal) but **cannot
express a computed composite** like `"3/8"`. So an operator literally cannot inject
a correct per-replica `K/N` via the template env. This is a **defect in the frozen
contract**, surfaced here because it directly shapes how an `AgentFleet` in shard
mode renders.

**The fix is a contract primitive, not a leak into agentctl.** The ask
(**P3**, a FIX) is `--shard auto/N`: the agent reads `N` from config and derives
`K` itself from the ordinal in `AGENT_POD_NAME` (`<name>-<ordinal>`). The operator
then injects only `N` (uniform across the template, legal) and the per-pod `K` is
the agent's own derivation. **Interim workaround** (until P3): an initContainer
shim parses the ordinal from the downward-API pod name and writes
`AGENT_SHARD="K/N"` to a shared `emptyDir` env file the agent sources — which
**requires a non-scratch image** (a shell), defeating the scratch/minimal posture,
so it is a stopgap only. The CRD records `scaling.mode: shard` + `scaling.shards: N`
(§4.1); the operator
chooses the injection mechanism by the negotiated contract (P3 present →
`--shard auto/N`; absent → the initContainer shim, gated on a non-scratch image).
**Contract ask: P3.**

---

## 10. Worked examples

### 10.1 A `once` Agent (renders to a Job)

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata:
  name: nightly-report
  namespace: agents
spec:
  mode: once                              # → Job (restartPolicy: Never), immutable
  image: registry.example.com/acme/agent@sha256:abcd…   # classless Agent names its own image (image XOR classRef, §7)
  instruction:
    inline: "Summarise yesterday's incidents from the incident MCP server and write a report."
  config:
    configMapRef: { name: report-config }
  intelligenceRef: { name: anthropic-pool }
  model: claude-opus-4
  mcp:
    servers:
      - { name: incidents, tags: [sensitive] }
      - { name: writer,    tags: [egress] }
  limits: { maxSteps: 120, deadlineSeconds: 900 }
  drain: { timeoutSeconds: 20 }
  podGraceSeconds: 30
  surfaces: { management: true, metrics: true, events: true }
  substrate: { tier: stock-unix }         # dev / single-tenant primary tier (RFC 0002)
  security: { allowTrifecta: false, enableExec: false, attachPolicy: deny }
# Rendered: a Job with podFailurePolicy compiled from the exit-code contract
# (2,5 → FailJob; 1,4,6 → Count; 137/143 → explicit handling). The node-agent
# captures the run-outcome report before the pod GCs (contract ask P5).
status:
  observedGeneration: 1
  phase: Running
  contract: { version: "1.0", agentVersion: "2.2.0", compatible: true }
  conditions:
    - { type: Validated,          status: "True",  reason: AdmissionPassed }
    - { type: ContractCompatible, status: "True",  reason: MajorUnderstood }
    - { type: Rendered,           status: "True",  reason: WorkloadApplied }
    - { type: Ready,              status: "False", reason: JobRunning }
```

### 10.2 A `reactive` Agent — see §3.4.

### 10.3 An `AgentFleet` (claim mode → Deployment + KEDA)

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: AgentFleet
metadata:
  name: inbox-workers
  namespace: agents
spec:
  template:                               # the per-replica Agent spec (mode pinned reactive)
    mode: reactive
    image: registry.example.com/acme/agent@sha256:abcd…   # classless template: own image (image XOR classRef, §7)
    instruction: { configMapRef: { name: inbox-instruction } }
    config:      { configMapRef: { name: inbox-config } }
    intelligenceRef: { name: anthropic-pool }
    model: claude-opus-4
    mcp:
      serverSetRefs: [inbox-readers]
      servers:
        - { name: ticketer, tags: [egress] }
    limits: { maxSteps: 80 }
    surfaces: { management: true, metrics: true, events: true }
    substrate: { tier: kata-hybrid }       # hardened tier — default for multi-tenant (RFC 0002)
    security: { allowTrifecta: false, attachPolicy: readOnly }
  scaling:
    mode: claim                           # elastic regime; KEDA owns .spec.replicas
    min: 0                                 # scale-from-zero (reads off-pod backlog — contract ask P9)
    max: 50
    target:
      signal: reactive_backlog            # validated against the negotiated metrics_schema (P10)
      threshold: 5
      activationThreshold: 1
  work:
    source: { mcp: inbox, uri: "file:///inbox/*.json" }
    claim:
      server: coord                       # a declared MCP server advertising work.* (RFC 0019 §3.3)
      style: tool                         # tool | resource
      ttlSeconds: 30                       # requested lease TTL; server is the authority
      key: item                           # stable, item-derived dedupe key (RFC 0019 §3.5)
  drain: { timeoutSeconds: 45 }
  podGraceSeconds: 60                      # CEL: MUST exceed drain.timeoutSeconds
# Rendered Deployment OMITS .spec.replicas (KEDA's HPA owns it). A KEDA
# ScaledObject targets the coordination-server backlog. Scale-down pods drain →
# release claims (RFC 0019 §6); the reference coordination MCP server is agentctl
# RFC 0011 (contract ask P12 freezes work.* ownership).
status:
  observedGeneration: 3
  phase: Running
  contract: { version: "1.0", agentVersion: "2.2.0", compatible: true }
  replicas: { desired: 4, ready: 4, updated: 4 }
  scaling: { mode: claim, currentMin: 0, currentMax: 50, lastScaleTime: "2026-06-27T10:15:00Z" }
  conditions:
    - { type: Validated,          status: "True", reason: AdmissionPassed }
    - { type: ContractCompatible, status: "True", reason: MajorUnderstood }
    - { type: Rendered,           status: "True", reason: WorkloadApplied }
    - { type: Ready,              status: "True", reason: SubscriptionsReconciled }
```

### 10.4 A `schedule` Agent (renders to a CronJob of `once` Jobs)

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata: { name: hourly-sweep, namespace: agents }
spec:
  mode: schedule
  image: registry.example.com/acme/agent@sha256:abcd…   # classless Agent names its own image (image XOR classRef, §7)
  instruction: { configMapRef: { name: sweep-instruction } }
  schedule: { cron: "0 * * * *", timezone: "UTC" }   # the ONE clock; agent internal cron disabled
  intelligenceRef: { name: anthropic-pool }
  mcp: { servers: [ { name: store, tags: [sensitive] } ] }
  limits: { maxSteps: 60, deadlineSeconds: 1800 }
  podGraceSeconds: 60
  substrate: { tier: stock-unix }
# Rendered: CronJob (concurrencyPolicy: Forbid, startingDeadlineSeconds set) whose
# jobTemplate is a mode=once Job. The rendered config disables the agent's internal
# cron so there is exactly one clock (double-schedule trap, §5.2).
```

---

## 11. Non-goals

- **The reconcile loop, the capability cache/probe, the edge-trigger/status-hot-loop
  discipline** — agentctl RFC 0006. This RFC fixes the *shape*; the controller
  behaviour is there.
- **The admission webhook implementation** (cert rotation, fail-closed wiring,
  operator-SA exemption, the init-container ground-truth rung) — agentctl RFC 0007.
- **The substrate descriptor and CID/uds/socket allocation** — agentctl RFC 0002.
- **The KEDA external scaler, the reference coordination MCP server, the
  shard-resize controller, standby/warm-pool** — agentctl RFC 0011.
- **`AgentClass`, `IntelligenceService`/`ModelPool`, `MCPServerSet`** — agentctl
  RFC 0004 (named and ref'd here, defined there).
- **Conversion-webhook machinery and alpha→GA graduation policy** — agentctl RFC 0005.
- **The A2A gateway, the Agent Card projection, the durable task store** — agentctl
  RFC 0013/0014 (and gated on contract ask P2).
- **Any data-plane internals.** The CRD describes contract intent; it MUST NOT
  encode one binary's flags, file layout, or version-specific behaviour (P0).
- **A `Task`/`Run` CRD.** Task/status-event churn is an etcd anti-pattern; run
  outcomes live in a store + a curated `.status.lastRun` (agentctl RFC 0010), never
  per-run CRs.

---

## 12. Rollout & compatibility

- Ship `Agent` + `AgentFleet` at **`v1alpha1`, single served + stored version**;
  absorb churn additively within alpha; defer the first real version bump to the
  conversion-webhook + SVM path (agentctl RFC 0005).
- The CRDs are **usable without RFC 0004**: every `*Ref` has an inline equivalent,
  so a v1 user can apply a fully-inline `Agent` against the shipped reference agent
  on the stock-unix substrate on day one.
- **Graceful degradation is built into the shape:** a `contract_version` major
  agentctl does not understand sets `ContractCompatible: False` and the agent is
  managed by liveness + exit codes + logs only (agentd RFC 0014 §7); a surface the
  agent does not advertise is simply absent from `.status.surfaces` and not driven.
- The CRD `apiVersion` and the agent `contract_version` move on **independent**
  clocks (§8); neither bump forces the other.

---

## 13. Open questions

1. **The apiGroup string.** `agents.x-k8s.io` (proposed — vendor-neutral noun,
   SIG-upstreamable intent, aligns with P0) vs `agentctl.dev` (safe, vendor-owned)
   vs the brainstorm's `agentctl.io`. Picking `x-k8s.io` presumes pursuing an
   upstream home; if that is not the plan, `agentctl.dev` is more honest. The
   *shape* in this RFC is group-string-independent.
2. **The downward-API env prefix under a neutral contract.** The contract token is
   currently spelled `AGENT_*` (the reference impl). If the contract is extracted
   into a neutral "Agent Control Contract" spec (P0 open question, agentctl RFC
   0001), should the env family be frozen as `AGENT_*` (a stable contract token,
   awkwardly named) or migrated to a vendor-neutral prefix (e.g. `AGENT_*`) with a
   compatibility window? This decides what the operator injects.
3. **Embedded `template` vs `AgentFleet` owning real `Agent` objects.** This RFC
   embeds the per-replica spec as `template` (lean; loses per-replica
   `Agent.status` — synthesized via the node-agent). The alternative (the fleet
   creates real `Agent` children) gives per-replica status at the cost of object
   sprawl. Confirm embedded.
4. **Deep-merge vs replace for sparse `limits`/`surfaces` overrides** against
   `AgentClass` defaults (RFC 0004). This RFC assumes field-wise deep-merge for
   maps and replace for lists; confirm with RFC 0004.
5. **`scaling.target.signal` enum source.** Validated against the negotiated
   `metrics_schema` rather than a hard-coded literal — but the contract's own
   metric-name set is unreconciled (P10). Until P10, the webhook keys the allowed
   set to the AgentClass's contract major.
6. **`contract_version` negotiation home when two `AgentClass`es pin different
   ranges** in one namespace — an RFC 0004 concern surfaced by this RFC's
   `.status.contract` projection.
7. **Should `surfaces.a2a` be omitted from the schema entirely until P2 lands**,
   rather than accepted-but-inert? Accepting-but-inert lets users write
   forward-compatible specs; omitting prevents a false sense of support. Leaning
   accepted-but-inert with an `A2AUnsupported` condition.

---

## 14. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & contract-as-schema foundation: the
  contract-not-agent (P0) framing, the codegen/conformance anti-drift strategy
  this CRD's `.status` projection consumes.
- **agentctl RFC 0002** — substrate & transport abstraction: the substrate tiers
  (`stock-unix`/`kata-hybrid`/`sidecar-emptydir`) `spec.substrate` selects and the
  per-pod address assignment that keeps addresses out of `.spec`.
- **agentctl RFC 0004** — `AgentClass`, `IntelligenceService`/`ModelPool`,
  `MCPServerSet`: the deferred CRDs the `classRef`/`intelligenceRef`/`serverSetRefs`
  fields resolve against.
- **agentctl RFC 0005** — CRD versioning & conversion: the single-served-version +
  conversion-webhook + SVM machinery and the alpha→GA graduation this RFC's §8
  posture commits to.
- **agentctl RFC 0006** — operator reconcile & manifest-driven capability model:
  the renderer, the capability cache, the single-writer `.status` discipline.
- **agentctl RFC 0007** — admission validation ladder: the webhook that performs
  the trifecta union, name-collision, and config-schema checks CEL cannot (§7).
- **agentctl RFC 0011** — scaling plane: the KEDA external scaler, the reference
  coordination MCP server, the shard-resize controller behind `AgentFleet`.
- **agentctl RFC 0013/0014** — A2A gateway & mesh identity: the A2A surface
  `spec.surfaces.a2a` gates on.
- **agentctl RFC 0016** — CLI & kubectl-plugin grammar: the consumer of the
  additionalPrinterColumns and `.status` projection.

**Contract spec (the reference implementation, agentd RFCs)**

- **agentd RFC 0014** — control-plane contract umbrella (the reference impl's
  contract spec): the capabilities-manifest spine (§5), the `surfaces{}` discovery
  point (§6.2), the downward-API env convention (§6.4), contract versioning (§6.3).
- **agentd RFC 0015** — management & control surface: the manifest schema (§5.2),
  `agent://inventory`/`status` (§5.3/§5.4), the operator tools, env identity (§6) —
  the source of the `.status` curated projection.
- **agentd RFC 0016** — telemetry & lifecycle contract: the frozen metrics schema
  + exit-code table the `podFailurePolicy` and `scaling.target.signal` key off.
- **agentd RFC 0017** — declarative config & hot reload: the config-file schema
  `spec.config` renders to, `--validate-config`/`--config-schema` (contract ask P6),
  the reloadable-vs-restart partition.
- **agentd RFC 0018** — intelligence transport resilience: the multi-endpoint
  binding `intelligence`/`intelligenceRef` projects.
- **agentd RFC 0019** — horizontal scaling: the claim/lease (§3), shard predicate
  (§4), autoscaling signals (§5), drain-release (§6) behind `AgentFleet`; the
  `AGENT_SHARD` defect (§4.2, contract ask P3).
- **agentd RFC 0008** — execution modes & reactive routing: the `mode` vocabulary
  and the internal-cron-vs-CronJob mutual exclusion (§5.2).
- **agentd RFC 0011** — cloud-native contract: the exit-code table, drain
  choreography, and `RUN_ID` the rendering and status lean on.
- **agentd RFC 0012** — security posture: the trifecta tag vocabulary (§3.1) and
  the `allow-trifecta` / `enable-exec` gates `spec.security` projects.
- **agentd RFC 0007** — agentic loop & terminal status: the terminal-status
  vocabulary `.status` and the once-mode `podFailurePolicy` consume.
- **agentd RFC 0020** — A2A interop over vsock: the A2A binding and the
  `surfaces.a2a` manifest-key ask (contract ask P2).

**Contract asks raised or cited by this RFC** (agentctl brainstorm §14): **P1**
(exec health verb — probe rendering on networkless pods), **P2** (`surfaces.a2a`),
**P3** (`--shard auto/N` — the `AGENT_SHARD` defect), **P5** (run-outcome capture
for `once`), **P6** (`--config-schema`/`--validate-config`), **P9** (off-pod
scale-from-zero signal), **P10** (autoscaling metric-name reconciliation), **P12**
(`work.*` ownership freeze).
