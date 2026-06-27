# agentctl RFC 0004: AgentClass, IntelligenceService (ModelPool) & MCPServerSet — the ops/dev decoupling CRDs

**Status:** Proposed (agentctl foundational track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the ops-owned reference objects an `Agent`/`AgentFleet` points at, so a developer's `Agent` never encodes cluster operations

> **Contract-first, not agent-first (P0).** These three CRDs describe **ops
> intent** — a substrate tier, a tenancy posture, a contract-version pin, an
> ordered set of model endpoints, a reusable tagged MCP-server bundle — for **any
> agent that conforms to the Agent Control Contract**. They MUST NOT encode the
> internals of one data-plane binary. Where a concrete field shape is needed, this
> RFC cites the **reference implementation's** contract spec (agentd RFCs
> 0014–0020) as *where the contract is presently written down*, never as a
> dependency. The agentd-branded contract surfaces these objects resolve into
> (`AGENTD_INTELLIGENCE`, the `AGENTD_INTELLIGENCE_TOKEN[_n]` env family, the
> failover/breaker env family `AGENTD_INTEL_BREAKER_*` / `AGENTD_INTEL_ALLDOWN_BACKOFF`,
> the `agentd_intel_*` metric series, the `--model-swap` knob) are
> contract-normative-but-branded — cited as the reference spelling and flagged for
> neutralization (they fall under the general `AGENTD_*` / `agentd_` neutralization
> pattern; the P0 contract-extraction open question, agentctl RFC 0001 §9).

> **The ops/dev seam is the whole point.** An `Agent` author writes *what the agent
> is* — an instruction, a mode, a set of tools, a model preference. A platform
> operator owns *how it runs in this cluster* — which substrate isolates it, which
> tenancy rule binds it, which model backends serve it and where their secrets
> live, which contract version the image speaks. RFC 0003 reserved three reference
> fields (`classRef`, `intelligenceRef`, `mcp.serverSetRefs`) precisely so these
> two roles can be edited independently. This RFC defines the objects those fields
> resolve against.

> **Optional, never load-bearing for v1 usability.** Every reference these CRDs
> satisfy has an **inline equivalent** on `Agent`/`AgentFleet` (agentctl RFC 0003
> §2.2). A fully-inline `Agent` on the stock-unix substrate runs on day one
> *without* any object defined here. These CRDs add a **reuse and decoupling seam**;
> they remove no capability and gate no MVP milestone.

---

## 1. Problem / Context

An `Agent` (agentctl RFC 0003) is the thing a developer `kubectl apply`s. Left to
itself it is tempted to grow two unrelated bodies of knowledge into one object:

1. **Developer intent** — the instruction, the execution mode, the tools the agent
   reasons with, the model it prefers, the work it subscribes to. This is what the
   author of the agent actually knows and wants to change.

2. **Cluster operations** — which substrate tier isolates the workload (agentctl
   RFC 0002), whether the namespace is hostile multi-tenant and therefore *forced*
   onto a microVM, which `RuntimeClass` realizes that, what node placement and
   resource floors apply, which model **backends** exist and **where their provider
   credentials live**, and which agent **image** speaks which **contract version**.
   This is what a platform operator knows and what a developer should never have to
   encode — or be able to get wrong.

If both live on `Agent`, three failures follow. (a) **Every developer re-derives
ops policy**, badly: copy-pasted `runtimeClassName`, hand-typed model endpoint
URLs, provider API keys leaking toward the pod spec. (b) **A backend move becomes a
fleet-wide edit** — repointing one model service means editing every `Agent` that
named its address. (c) **Security policy is unauditable** — there is no single
object on which "this class of agents runs on the kernel-isolated tier with these
secrets at the proxy" can be reviewed, RBAC-scoped, and enforced.

The resolution is the standard Kubernetes ops/dev seam — the shape `StorageClass`
took for volumes and `IngressClass` for ingress — applied to three concerns:

| Concern | Dev writes (on `Agent`, RFC 0003) | Ops writes (this RFC) |
|---|---|---|
| **Substrate / tenancy / contract pin** | nothing (or a sparse `substrate` override) | **`AgentClass`** (cluster-scoped) |
| **Model endpoints / provider secrets / failover** | `intelligenceRef` (or an inline endpoint list) | **`IntelligenceService`** (namespaced) |
| **Reusable tagged tool bundles** | `mcp.serverSetRefs` (+ inline `servers`) | **`MCPServerSet`** (namespaced) |

This RFC owns the **shape** of those three objects, the **enforcement hooks** they
expose to admission (agentctl RFC 0007) and the renderer (agentctl RFC 0006), and
the **override/merge semantics** between a sparse `Agent` field and a class default.
It does **not** own: the reconcile/render loop or the capability probe (agentctl
RFC 0006), the admission webhook implementation that *executes* the rules named
here (agentctl RFC 0007), the substrate descriptor and the §5 binding rule's *text*
(agentctl RFC 0002), the intelligence **egress proxy** data path and cost
governance (agentctl RFC 0012), the scaling controller (agentctl RFC 0011), or the
conversion/versioning machinery (agentctl RFC 0005). It references each.

---

## 2. Decision — seven points

1. **Three CRDs, group `agents.x-k8s.io`, version `v1alpha1`.** `AgentClass`
   (**cluster-scoped**, StorageClass-shaped), `IntelligenceService`
   (**namespaced**, colloquially the *ModelPool*), `MCPServerSet` (**namespaced**).
   The group and version match agentctl RFC 0003 exactly so the whole API set
   versions and graduates together (the group string itself is the shared open
   question of RFC 0003 §13.1 — the *shape* here is group-string-independent).

2. **`AgentClass` is the single home of substrate, tenancy, and the contract pin.**
   It **owns `spec.substrate.tier`** — the field the RFC 0002 §5 tenancy×substrate
   binding rule *reads* and the admission gate (agentctl RFC 0007) *enforces*. A
   hostile-tenant class is forced to `kata-hybrid` and an `AgentClass` selecting
   `stock-unix` under hostile tenancy is **rejected at admission** (§3.3). The
   contract-version pin (which image + which `contract_version` range a class binds,
   negotiated per agentd RFC 0014 §6.3) lives here and **nowhere else** (§3.4).

3. **`IntelligenceService` is the model-endpoint plane as data.** It is an **ordered
   list of endpoints (pools)** that resolves to the contract's ordered
   `--intelligence` failover list (agentd RFC 0018 §2). Provider credentials are
   resolved **at the egress proxy** (agentctl RFC 0012), **never injected into the
   agent pod** (agentd RFC 0006 §6 / RFC 0012 §3.7); the agent dials the proxy over
   the substrate socket/vsock, keyless. Each endpoint carries a **stable name** for
   metric labels (contract ask **P7**) and may advertise **served models** for
   model-aware placement (§4).

4. **`MCPServerSet` is a reusable, tagged tool bundle.** It names MCP servers and
   their operator-declared trifecta/capability tags (agentd RFC 0012 §3.1). An
   `Agent` composes `serverSetRefs` + inline `servers` by **union (ADD)**; the
   **tag union is computed at the `Agent`** and feeds admission (agentctl RFC 0007)
   and the contract's granted-MCP-subset trust budget (agentd RFC 0012 §3.2). The
   admission trifecta check is **advisory**, not blocking (§5.3, brainstorm §2.2).

5. **Override/merge is deterministic: deep-merge maps, replace lists, ADD MCP
   servers.** A sparse field on `Agent` overrides the class default **per-key** for
   maps/scalars (`limits`, `surfaces`, `resources`, `nodeSelector`), **replaces
   wholesale** for lists (`tolerations`), and **adds (unions)** for MCP servers
   (the one documented exception, mirroring the contract's `--mcp` deviation,
   agentd RFC 0017 §3.3 / agentctl RFC 0003 §3.1). This answers RFC 0003 OQ4 (§6.1).

6. **All three are optional; they add a seam, not a requirement.** Each resolves a
   reference RFC 0003 already reserved, and each has an inline `Agent` equivalent. A
   v1 cluster may ship none of them. Adopting them is a refactor toward reuse and
   least-privilege, never a precondition for managing an agent.

7. **Capability/version gating keys off `surfaces{}` and `contract_version`, never
   off `build_features` (P0).** The `AgentClass` contract pin is a `contract_version`
   **range** (agentd RFC 0014 §6.3) plus a **required-surfaces** set (the
   `surfaces{}` keys the class needs the agent to advertise, agentd RFC 0014 §6.2).
   `build_features` is opaque/informational and MUST NOT be a gate (agentctl RFC 0003
   §6.1). The CRD `apiVersion` and the agent `contract_version` move on **independent
   clocks** (agentctl RFC 0005).

---

## 3. `AgentClass` — the ops profile

`AgentClass` is **cluster-scoped** and StorageClass-shaped: a platform operator
defines a few named classes (`standard`, `hardened`, `batch`) and developers select
one by `classRef`. It is the single place substrate, tenancy, placement, resource
floors, and the contract pin are decided — so those decisions are reviewed and
RBAC-scoped once, not re-typed per `Agent`.

### 3.1 Full schema

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: AgentClass                         # CLUSTER-SCOPED (no namespace)
metadata:
  name: hardened
spec:
  # --- the agent image + contract pin (§3.4) ---
  image: registry.example.com/acme/agent@sha256:…   # the conformant agent image this class binds
  imagePullPolicy: IfNotPresent
  imagePullSecrets: [{ name: acme-pull }]
  contractVersionRange: ">=1.0 <2.0"     # the contract MAJOR.MINOR range this class accepts
                                         #   (agentd RFC 0014 §6.3: refuse unknown major, tolerate additive minor)
  requiredSurfaces: [management, metrics, events]   # surfaces{} keys the class REQUIRES the agent to advertise
                                                    #   (P0-clean capability gate — agentd RFC 0014 §6.2; NOT build_features)

  # --- substrate + tenancy: the field RFC 0002 §5 reads, RFC 0007 enforces (§3.3) ---
  substrate:
    tier: kata-hybrid                    # stock-unix | kata-hybrid | sidecar-emptydir  (agentctl RFC 0002 §4)
    tenancy: hostile                     # single | hostile — the posture this class is FOR (RFC 0002 §5)
    runtimeClassName: kata-clh           # REQUIRED for kata-hybrid; the cluster's Kata RuntimeClass

  # --- default model binding (resolved before the Agent's, overridable) ---
  defaultIntelligenceRef: { name: anthropic-pool }  # an IntelligenceService in the Agent's namespace (§4)

  # --- ops defaults the Agent sparsely overrides (§6.1 merge rules) ---
  defaults:
    resources:                           # k8s pod resources (NOT the contract limits box below)
      requests: { cpu: "500m", memory: 512Mi }
      limits:   { cpu: "2",    memory: 2Gi }
    limits:                              # the CONTRACT agentic limits box (agentd RFC 0015 §5.2) — deep-merged
      maxSteps: 200
      maxDepth: 4
      maxTokens: 2000000
      treeTokenBudget: 4000000
      maxTotalSubagents: 64
      deadlineSeconds: 1800
    drain: { timeoutSeconds: 25 }
    podGraceSeconds: 30                  # CEL on the rendered pod: drain.timeoutSeconds < podGraceSeconds
    security: { allowTrifecta: false, enableExec: false, attachPolicy: readOnly }

  # --- node placement (rendered into the pod template) ---
  placement:
    nodeSelector: { agentctl.dev/kata: "true" }       # map — deep-merged with Agent override
    tolerations:                                       # list — REPLACED wholesale by an Agent override
      - { key: "agents-only", operator: "Exists", effect: "NoSchedule" }
    affinity: {}                                       # optional; replaced wholesale if the Agent sets it
    topologySpreadConstraints: []

  # --- reclaim / lifecycle ---
  reclaimPolicy: Retain                  # Retain | Delete — what happens to rendered children on class detach (informational v1)
status:                                  # operator-written (single writer, agentctl RFC 0006); DeepEqual-guarded
  imageDigest: "sha256:abcd…"            # spec.image resolved to a digest (RFC 0006 §5.3)
  inUse: true                            # any referrers? (finalizer ENUMERATES referrers — never an int, §8)
  conditions:
    - { type: Ready,              status: "True",  reason: ClassResolved }
    - { type: ContractCompatible, status: "True",  reason: MajorUnderstood }   # the class IMAGE's probed contract major is within contractVersionRange
                                                                               #   False → MajorUnknown (the per-Agent negotiation still records its own result in Agent.status, §3.4)
```

> **AgentClass status is class-level, not instance-level.** The operator probes the
> class **image** once (digest-keyed `CapabilityProbe`, agentctl RFC 0006 §5) and
> records whether that image's contract major is understood and within
> `contractVersionRange` (`ContractCompatible` / `MajorUnknown`). The **per-`Agent`
> negotiated** result still lands in `Agent.status.contract` (§3.4); the class
> condition is the shared image verdict, not a substitute for it. The
> single writer is the operator; `inUse` is computed from the referrer-enumerating
> finalizer (§8), never a stored reference count.

### 3.2 Field reference

| Field | Type | Owner-only? | Contract anchor | Notes |
|---|---|---|---|---|
| `image` / `imagePullPolicy` / `imagePullSecrets` | image spec | ops | — | the conformant agent image for this class. An `Agent` that sets `classRef` MUST NOT also set `spec.image` (CEL, agentctl RFC 0003 §7); a classless `Agent` names `spec.image` inline instead (RFC 0003 §3.1). The class is the image's home **when referenced**, not the only way to specify one. |
| `contractVersionRange` | semver range | ops | agentd RFC 0014 §6.3 | the negotiated `contract_version` major.minor window (§3.4). |
| `requiredSurfaces` | `[]string` | ops | agentd RFC 0014 §6.2 | `surfaces{}` keys the agent MUST advertise; the P0-clean capability gate. Absent surface ⇒ admission/condition failure, not silent. |
| `substrate.tier` | enum `stock-unix\|kata-hybrid\|sidecar-emptydir` | ops | agentctl RFC 0002 §4 | **the field the §5 binding rule reads** (§3.3). |
| `substrate.tenancy` | enum `single\|hostile` | ops | agentctl RFC 0002 §5 | the posture this class is for; the **effective** tenancy is `max(namespace label, this)` (§3.3). |
| `substrate.runtimeClassName` | string | ops | agentctl RFC 0002 §5 | required for `kata-hybrid`; the cluster's Kata `RuntimeClass`. |
| `defaultIntelligenceRef` | `{name}` | ops | agentd RFC 0018 | the `IntelligenceService` used when the `Agent` sets no `intelligenceRef`/inline (§4.5). |
| `defaults.resources` | k8s ResourceRequirements | ops | — | pod CPU/memory requests+limits. Distinct from `defaults.limits`. |
| `defaults.limits` | contract limits box | ops | agentd RFC 0015 §5.2 | the **agentic** limits (`maxSteps`/`maxDepth`/`maxTokens`/`treeTokenBudget`/`maxTotalSubagents`/`deadlineSeconds`); deep-merged with `Agent.spec.limits`. |
| `defaults.drain` / `defaults.podGraceSeconds` | `{timeoutSeconds}` / int | ops | agentd RFC 0011 §3.3/§4.2 | drain budget + grace; the `drain<grace` CEL holds on the merged result. |
| `defaults.security` | `{allowTrifecta,enableExec,attachPolicy}` | ops | agentd RFC 0012 §3.2/§3.6 | security floor; `allowTrifecta`/`enableExec` are **floors an `Agent` may not loosen without elevated RBAC** (§5.3, agentctl RFC 0007). |
| `placement.*` | nodeSelector/tolerations/affinity/spread | ops | — | rendered into the pod template; merge per §6.1. |
| `reclaimPolicy` | enum `Retain\|Delete` | ops | — | informational in v1 (children are owned by the `Agent`, GC'd with it). |

### 3.3 Tenancy × substrate — the field this RFC owns and RFC 0007 enforces

agentctl RFC 0002 §5 states the **tier-binding consequence** (hostile tenancy ⇒
the microVM kernel boundary; `stock-unix` forbidden for untrusted tenants). **This
RFC introduces the per-class `spec.substrate.tenancy` posture field and the
effective-tenancy resolution** that consequence is evaluated against — the `max()`
formula, the per-class `substrate.tenancy` field, and the `agents.x-k8s.io/tenancy`
namespace-label key are **new here**, not restated from 0002 §5 (which is framed as
a cluster-level tenancy marking; where that label authoritatively lives is still
open — OQ4). The resolution model this RFC adds:

> **Effective-tenancy resolution (introduced by this RFC).** The **effective
> tenancy** of a class as applied to a namespace is `max(namespace label
> `agents.x-k8s.io/tenancy`, `spec.substrate.tenancy`)`, with `hostile` dominating
> `single`. If the effective tenancy is **`hostile`**, `spec.substrate.tier` **MUST
> be `kata-hybrid`** (or, only where Kata is unavailable, the explicitly-audited
> `sidecar-emptydir` per RFC 0002 §4.3/§5); `stock-unix` is **forbidden** (the RFC
> 0002 §5 consequence). If the effective tenancy is `single`, the default tier is
> `stock-unix` and `kata-hybrid` is opt-in hardening. An **absent** namespace label
> resolves to `hostile` in the `max()` (the fail-safe default — there is no weakening
> cluster-wide default; ownership and the explicit-downgrade-only rule are binding in
> agentctl RFC 0015 §4.5).

**Two enforcement points, two evaluators (the cluster-scoped-class subtlety).**
`AgentClass` is cluster-scoped and has no namespace, so the namespace-dependent
part of the rule **cannot** be decided at class admission. The check splits, and
this is also the CEL-vs-webhook partition for the substrate rule:

1. **Class-time, self-contained (CEL-expressible on the class).** On `AgentClass`
   CREATE/UPDATE, reject the self-contradiction `spec.substrate.tenancy == hostile
   && spec.substrate.tier == stock-unix`, and require `runtimeClassName` when
   `tier == kata-hybrid`. These need only the object itself — single-object
   invariants the CRD carries as CEL.
2. **Agent-time, namespace-dependent (webhook, agentctl RFC 0007).** On `Agent`
   CREATE/UPDATE, the webhook recomputes the **effective** tenancy with the
   *consuming namespace's* `agents.x-k8s.io/tenancy` label and the resolved
   class/inline tier, then re-checks the rule. This is the **only** point the
   namespace label is in scope, so it is the only place the effective-tenancy
   `max()` can be evaluated — it is a cross-object/namespace check (CEL cannot read
   the namespace label or dereference the class), owned by RFC 0007 B3.

So a stock-unix class never slips through: a `hostile`-posture class is rejected at
class-create (rule 1); a `single`-posture class consumed from a `hostile`-labelled
namespace is rejected at the `Agent` (rule 2). RFC 0007 owns the webhook that
performs the second check; this RFC owns the field and the resolution it evaluates.

```
                effective tenancy = max(ns label, spec.substrate.tenancy)
   ┌─────────────────────────────────────────────────────────────────────────┐
   │  hostile  ──▶  tier MUST ∈ {kata-hybrid, sidecar-emptydir(audited)}      │
   │               tier == stock-unix  ──▶  REJECT (admission, RFC 0007)      │
   │               kata-hybrid ⇒ runtimeClassName REQUIRED (else REJECT)      │
   │  single   ──▶  default tier = stock-unix; kata-hybrid opt-in              │
   └─────────────────────────────────────────────────────────────────────────┘
```

Why the field lives on `AgentClass` and not `Agent`: tenancy and isolation are an
**operator** decision a tenant must not be able to weaken. Putting `tier` on a
cluster-scoped, ops-RBAC'd object means a developer **cannot** select `stock-unix`
out of a hostile namespace — the only `tier` they can reach is the one the class
exposes (and a sparse `Agent.spec.substrate.tier` override is itself re-checked
against the same rule by admission, never trusted to loosen it). The operator's
renderer (agentctl RFC 0006) compiles `substrate.runtimeClassName` and the per-tier
probe/discovery wiring (agentctl RFC 0002 §6/§8) from this block; the agent binary
and its config are unchanged across tiers (agentctl RFC 0002 §12).

### 3.4 The contract-version pin (and the two-class-different-ranges case)

When an `Agent` references a class, the `AgentClass` is the **only** home of the
agent **image** and the `contract_version` **range** it binds — an `Agent` with a
`classRef` MUST NOT also set `spec.image` (CEL `has(classRef) != has(image)`,
agentctl RFC 0003 §7). A **classless** `Agent` instead names its own
`spec.image` inline (agentctl RFC 0003 §3.1) and carries **no** contract-range
pin: the operator negotiates against whatever major the image advertises and
manages by understood-major / graceful degradation (agentd RFC 0014 §6.3/§8). The
class exists to make the image + pin an ops-reviewed, RBAC-scoped decision, not to
be the *only* way to name an image. Resolution and negotiation for the
class-referenced case (owned by the operator, agentctl RFC 0006; the negotiation
rule is agentd RFC 0014 §6.3):

1. The operator resolves `spec.image` to a digest and runs the one-shot
   `CapabilityProbe` (cached by `(digest + feature-set)`, agentctl RFC 0006 §3.1) to
   read the agent's `--capabilities` manifest.
2. It checks the manifest's `contract_version` **major** is within
   `contractVersionRange` and understood; an unknown major ⇒ `ContractCompatible:
   False / MajorUnknown` and degrade to liveness+exit-code management (agentctl RFC
   0003 §6.2). An additive **minor** above the range's ceiling is **tolerated**
   (additive-by-minor, agentd RFC 0014 §6.3), not rejected.
3. It checks every `requiredSurfaces` key is present in `surfaces{}` (P0-clean,
   §2 point 7); a missing required surface ⇒ a `Degraded`/admission failure, not silent.
4. It projects the negotiated version into `Agent.status.contract.version` (agentct
   RFC 0003 §6.1).

This is the **answer to agentctl RFC 0003 OQ6** ("`contract_version` negotiation
home when two `AgentClass`es pin different ranges"): the negotiation home is the
`AgentClass` **per class**. Two classes pinning `>=1.0 <2.0` and `>=2.0 <3.0`
coexist with no conflict — each `Agent` negotiates against **the one class it
references**, and `Agent.status.contract` records the result for *that* class. There
is no cluster-global contract version; there is one per class, negotiated per
instance.

The image+pin belonging to ops is also a P0 hygiene win: the agentd-branded
contract surfaces (`--capabilities`, the `agentd://` scheme, the `agentd_` metric
prefix) are resolved through **this one object's image**, so swapping in a
second-vendor conformant agent — or the neutralized contract surfaces (P0) — is a
single `AgentClass.image` edit, not a fleet-wide change.

---

## 4. `IntelligenceService` (ModelPool) — the model-endpoint plane

`IntelligenceService` is **namespaced** and is the model plane expressed as data: an
**ordered list of endpoints (pools)** that the operator resolves into the contract's
ordered `--intelligence` failover list. A "pool" is a set of **interchangeable
backends** serving the same model(s); the **egress proxy** (agentctl RFC 0012)
load-balances **within** a pool, while the agent's own resilience machinery (agentd
RFC 0018) fails over **across** pools. The two-level division is deliberate and maps
exactly onto the data/control split:

```
   Agent pod (NO provider secret)                 IntelligenceService (this CRD)
   ┌───────────────────────────┐                  ┌──────────────────────────────┐
   │ agentd RFC 0018:           │  --intelligence  │ endpoints (ORDERED = failover)│
   │  ACROSS pools = failover,  │◀───ordered list──│  [0] primary  (pool)          │
   │  sticky-primary, breaker   │  over substrate  │  [1] fallback (pool)          │
   │  (NEVER load-balances)     │   socket/vsock   │  …                            │
   └─────────────┬─────────────┘   (keyless dial) └──────────────┬───────────────┘
                 │ dials proxy endpoint, no key                    │ each pool:
                 ▼                                                 ▼  backends[] + credentialRef
   ┌───────────────────────────────────────────────────────────────────────────┐
   │ egress proxy (agentctl RFC 0012 — NOT in the node-agent; per-pod sidecar or │
   │ node-local Deployment): holds provider cred, terminates TLS, translates     │
   │ dialect, LBs WITHIN a pool, per-backend circuit-break, re-exports backend   │
   │ health on its own series                                                    │
   └───────────────────────────────────────────────────────────────────────────┘
```

### 4.1 Full schema

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: IntelligenceService                # namespaced; colloquially the ModelPool
metadata:
  name: anthropic-pool
  namespace: agents
spec:
  # ordered: index 0 is primary, the rest are agentd RFC 0018 failover fallbacks (across-pool)
  endpoints:
    - name: opus-primary                 # STABLE per-endpoint NAME → metric label (contract ask P7);
                                         #   survives list reorder (NOT a list index, NOT a moving URI)
      models: [claude-opus-4]            # served models (model-aware placement, §4.4) — P7
      dialect: anthropic                 # openai | anthropic (agentd RFC 0006 §4); proxy may translate to openai
      backends:                          # interchangeable replicas the PROXY load-balances across (in-pool)
        - { service: anthropic-gw.models.svc, port: 443, weight: 1 }
        - { service: anthropic-gw-2.models.svc, port: 443, weight: 1 }
      credentialRef:                     # provider cred — resolved AT THE PROXY, NEVER in the agent pod (§4.3)
        secretRef: { name: anthropic-key, key: api-key }
      tls: { mode: terminate-at-proxy }  # proxy terminates TLS; agent dials keyless over the substrate
      circuitBreaker:                    # PER-BACKEND breaker, executed by the proxy (in-pool)
        consecutiveFailures: 3
        cooldownSeconds: 5
    - name: opus-fallback
      models: [claude-opus-4]
      dialect: anthropic
      backends: [{ service: bedrock-gw.models.svc, port: 443 }]
      credentialRef: { secretRef: { name: bedrock-key, key: api-key } }

  # ACROSS-pool failover/breaker → the agent's own agentd RFC 0018 knobs (the ordered list above).
  # The CRD field names (breakerThreshold, …) are NEUTRAL; the AGENTD_INTEL_* env they map to is the
  # reference impl's BRANDED spelling — flagged for contract-neutralization (intro blockquote, RFC 0001 §9).
  failover:
    stickyPrimary: true                  # agentd RFC 0018 §3.3 (always true; documented, not tunable in v1)
    breakerThreshold: 3                  # → AGENTD_INTEL_BREAKER_THRESHOLD (agentd RFC 0018 §4.2) [branded; flagged]
    breakerCooldownSeconds: 5            # → AGENTD_INTEL_BREAKER_COOLDOWN [branded; flagged]
    breakerCooldownMaxSeconds: 60        # → AGENTD_INTEL_BREAKER_COOLDOWN_MAX [branded; flagged]
    allDownBackoff: "1s..30s"            # → AGENTD_INTEL_ALLDOWN_BACKOFF (agentd RFC 0018 §6) [branded; flagged]

  # hot-swap INTENT — the CRD records the policy; the operator+proxy EXECUTE it (§4.5)
  swapPolicy: finish-on-old              # finish-on-old | restart-turn → agentd --model-swap (RFC 0018 §5.3)

  # where the egress proxy runs (the field is OURS; the proxy data path is agentctl RFC 0012)
  proxy:
    topology: sidecar                    # sidecar | node-local | none  (§4.2)
    image: registry.example.com/agentctl/intel-proxy@sha256:…   # owned/operated by agentctl RFC 0012
  fallbackDirect: false                  # if true, append a non-proxy endpoint so the proxy is never a hard SPOF (§4.2)

  # model-aware placement hint (§4.4)
  placement:
    modelAware: true                     # operator routes an Agent's model to a pool whose endpoint serves it
status:
  endpoints:                             # mirrored from the proxy's re-exported per-backend health (§4.3)
    - { name: opus-primary, healthy: true,  backendsReady: 2, activeModel: claude-opus-4 }
    - { name: opus-fallback, healthy: true, backendsReady: 1 }
  conditions:
    - { type: Ready, status: "True", reason: AllPoolsHealthy }
```

### 4.2 The proxy is OUT of the node-agent; keep a non-proxy fallback

Two corrections from the brainstorm (§6.2, D3) are normative in the CRD's shape:

- **The proxy MUST NOT be co-located in the node-agent.** Both pool endpoints
  terminating on one per-node process makes agentd's inter-pool failover (agentd
  RFC 0018) shared-fate, and because the agentic loop **blocks** on the model call,
  a per-node proxy crash stalls inference on **every** local pod. `proxy.topology`
  is therefore `sidecar` (per-pod, `unix:/run/intel.sock`, agentd RFC 0006) or
  `node-local` (a separate Deployment) — **never** the Tier-A node-agent (agentct
  RFC 0008). The proxy's *implementation* (deployment, LB, dialect translation,
  cost metering) is agentctl RFC 0012; this CRD owns only the **declaration** of
  topology and endpoints.
- **`fallbackDirect: true` keeps a non-proxy endpoint** appended to the ordered
  list so the proxy is never a **hard inference SPOF** (brainstorm §6.2/D3). That
  fallback endpoint is the one case where a credential may reach the agent pod (the
  direct-dial path, §4.3) and is therefore permitted only on `single`-tenancy
  classes — admission (agentctl RFC 0007) rejects `fallbackDirect: true` on a
  hostile-tenant class.

### 4.3 Zero-secret-in-pod — where the provider credential actually lives

The load-bearing security property: **the provider credential is resolved at the
proxy and never enters the agent pod.** This rides three contract facts —
secrets are env/file only behind the `resolve()` front door (agentd RFC 0006 §6),
the `Secret` newtype is structurally unserializable so it cannot reach the manifest
(agentd RFC 0012 §3.7), and on the keyless tier the agent dials a substrate-local
socket with no key (agentctl RFC 0002 §10 correction 1). The resolution table:

| `proxy.topology` | Provider credential lives in | Agent pod sees | Agent's `--intelligence` points at | Allowed under hostile tenancy? |
|---|---|---|---|---|
| `sidecar` | the **sidecar proxy** (mounts `credentialRef`) | **no provider secret** | `unix:/run/intel.sock` (substrate-local) | **yes** (the zero-secret path) |
| `node-local` | the **node-local proxy Deployment** | **no provider secret** | the proxy endpoint over the substrate | **yes** |
| `none` (direct dial) | the **agent pod** (env `AGENTD_INTELLIGENCE_TOKEN[_n]`/`_FILE`) | the provider secret | the provider endpoint directly | **no** — rejected by admission (RFC 0007) |

So in the default (proxy-fronted) topology the operator renders `AGENTD_INTELLIGENCE`
= the proxy endpoints over the substrate and injects **no**
`AGENTD_INTELLIGENCE_TOKEN*` into the agent pod; the `credentialRef` is mounted into
the proxy (agentctl RFC 0012). Only `proxy.topology: none` injects the credential as
the contract's `AGENTD_INTELLIGENCE_TOKEN[_n]`/`_FILE` env (agentd RFC 0014 §6.4) —
the explicitly non-zero-secret, single-tenant-only path. The `credentialRef` is a
reference to a `Secret`; the **value never appears in this CRD, in the manifest, or
in `Agent.status`** (agentd RFC 0012 §3.7).

### 4.4 Model-aware placement + stable endpoint names (P7)

Two capabilities depend on the contract ask **P7** (per-endpoint model arrays in the
manifest + stable operator-assigned endpoint **names**):

- **Stable names for metric labels.** Each `endpoints[].name` is the **bounded,
  stable label** the operator passes via the reference impl's `--intelligence-names`
  (agentd RFC 0018 §4.3/§11, contract ask P7) so the `agentd_intel_endpoint_*`
  series survive a list reorder. Without P7 the contract labels endpoints by **list
  index** (`"0"`,`"1"`), which shifts when a fallback is inserted ahead of the
  primary — exactly the dashboard-breaking churn the names solve. The CRD always
  records the name; the operator uses it when the negotiated contract supports P7
  and degrades to index labels otherwise.
- **Model-aware placement.** `endpoints[].models` (P7) lets the operator route an
  `Agent` whose `model` (or whose pool's model) is `claude-opus-4` onto a pod whose
  bound endpoint **serves** opus (agentd RFC 0018 §5.4 surfaces discovered models
  into `intelligence.models`). With `placement.modelAware: true` the renderer
  (agentctl RFC 0006) adds the corresponding node/endpoint affinity; without P7 it
  assumes only the configured `model` and skips model-aware affinity.

### 4.5 How an `Agent` references it (and how it merges)

`Agent.spec.intelligenceRef: {name}` (agentctl RFC 0003 §3.1) names an
`IntelligenceService` **in the Agent's namespace**. Resolution order (most specific
wins): `Agent.spec.intelligence` (inline ordered list) **>** `Agent.spec.intelligenceRef`
**>** `AgentClass.spec.defaultIntelligenceRef`. The reference and an inline list are
**mutually exclusive** on one `Agent` (CEL, agentctl RFC 0003 §7); they do **not**
merge — intelligence is replace-not-merge because endpoint order is the failover
policy and a partial merge would silently reorder it. `Agent.spec.model` (if set)
selects which of the pool's models to request and overrides the pool's default; the
pool may supply the model when the `Agent` omits it (agentctl RFC 0003 §3.1).

A **backend move is a one-object edit**: the operator edits the
`IntelligenceService` (repoint a `backends[]` address, swap a `credentialRef`),
re-renders the proxy + the agent's `AGENTD_INTELLIGENCE` list, and signals a hot
reload (agentd RFC 0017); the agent executes the **quiesce → switch → resume**
primitive per `swapPolicy` (agentd RFC 0018 §5) with no restart and no dropped
in-flight work. The CRD records the **intent** (`swapPolicy`, the new endpoints);
the operator+proxy **execute** it. *Deciding* to swap (cost/latency policy) is
agentctl RFC 0012, not this CRD.

---

## 5. `MCPServerSet` — a reusable tagged tool bundle

`MCPServerSet` is **namespaced** and bundles MCP servers with their
operator-declared **trifecta/capability tags** so a set of tools is defined,
tagged, and reviewed **once** and referenced by many `Agent`s. It is the dev-facing
counterpart to the contract's per-server tag config (agentd RFC 0012 §3.1) and the
`--mcp`/`--mcp-config` surface (agentd RFC 0017 §3.3).

> **Tag shape (canonical, shared with `Agent.spec.mcp.servers[]`).** `tags` is a
> **per-tool glob map** — *tool-name-glob → legs* (`{ "*": [untrusted_input],
> "read_*": [sensitive] }`, first-match / longest-glob-wins) — the faithful
> rendering of the contract's per-tool glob tagging (agentd RFC 0012 §3.1). This is
> the same field shape on both `MCPServerSet.spec.servers[]` (here) and
> `Agent.spec.mcp.servers[]` (agentctl RFC 0003 §3.1); a bare list `tags: [legs]`
> is accepted **shorthand** for `{ "*": [legs] }`. The map form here is the
> normative spelling; the flat-list examples in RFC 0003 are that shorthand, not a
> second schema.

### 5.1 Full schema

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: MCPServerSet
metadata:
  name: core-readers
  namespace: agents
spec:
  servers:
    - name: fs                           # the MCP server name (unique within the resolved Agent set, §5.2)
      transport: stdio                   # stdio (default — confinement win, agentd RFC 0012 §3.4) | unix
      command: ["mcp-fs", "--root", "/watch"]   # operator config ONLY — never model/server-derived (agentd RFC 0012 §3.4)
      tags:                              # per-tool glob; first-match, longest-glob-wins (agentd RFC 0012 §3.1)
        "*":      [untrusted_input]      # default leg for every tool of this server
        "read_*": [untrusted_input]      # a read subset
    - name: incidents
      transport: stdio
      command: ["mcp-incidents"]
      tags: { "*": [sensitive] }
  # informational: the union of legs this set CAN contribute (computed, surfaced for review)
status:
  tagUnion: [untrusted_input, sensitive] # NOT the trifecta alone — the Agent-level union is the gate (§5.3)
  conditions:
    - { type: Resolved, status: "True", reason: ServersValid }
```

### 5.2 Composition — refs + inline ADD (union), names must not collide

An `Agent` composes tools from `mcp.serverSetRefs` (zero or more `MCPServerSet`s,
in the same namespace) **plus** `mcp.servers` (inline additions). They **ADD
(union)**, mirroring the contract's `--mcp` deviation where refs and inline compose
rather than replace (agentd RFC 0017 §3.3, agentctl RFC 0003 §3.1):

```
resolved servers = ⋃(serverSetRefs[*].spec.servers) ⊎ Agent.spec.mcp.servers
                   └────────────── union; server name is the key ──────────────┘
```

**Server names MUST be unique across the union.** A duplicate `name` between two
referenced sets, or between a set and an inline addition, is **ambiguous and
rejected by the webhook** (agentctl RFC 0007) — CEL cannot dereference the sets to
detect it, so this is one of the three checks that make the validating webhook
mandatory (agentctl RFC 0003 §7). The reference impl's MCP servers are expected to be
**baked into the verified agent image** (brainstorm §10.2): the `command` is
operator config that must resolve **inside** the image, and supply-chain
verification (cosign/Kyverno `verifyImages`) covers the image, not a separately
fetched server binary (the servers are the real ASI01 surface, brainstorm §10.2).

### 5.3 Tag union → admission (advisory) + the per-spawn trust budget

Tags compose by **union across the whole resolved set**, computed **at the `Agent`**
(not per `MCPServerSet`): the lethal-trifecta legs (`untrusted_input` + `sensitive`
+ `egress`, agentd RFC 0012 §3.1) are OR-ed across `serverSetRefs` + inline. Two
individually-"safe" sets (one `sensitive`-only, one `untrusted_input`+`egress`) can
compose the **full trifecta** on one `Agent` — which is why the union must be an
`Agent`-level computation and why no single `MCPServerSet` can be judged safe in
isolation (`status.tagUnion` is informational, not a verdict).

Two consumers of the union, and the **deliberate split** between them (brainstorm
§2.2/§10.1):

1. **Admission is advisory, not blocking.** The webhook (agentctl RFC 0007) computes
   the union and checks it against `security.allowTrifecta`, but emits a
   **warning/observational** condition rather than refusing — because the contract
   already enforces Rule-of-Two **per subagent at the spawn chokepoint** over each
   child's *narrowed* grant (agentd RFC 0012 §3.2), and the canonical **safe**
   pattern is a reader/actor split across servers that a naive blocking
   `Agent`-level union would wrongly refuse (training operators to flip
   `allowTrifecta` routinely — the anti-pattern). The **real** control is gating the
   `allowTrifecta` **override** behind elevated RBAC + audit (agentctl RFC 0007 /
   agentctl RFC 0009), and `AgentClass.defaults.security.allowTrifecta` is a **floor**
   an `Agent` may not loosen without that RBAC.
2. **The contract enforces the budget at runtime.** The resolved tags render into
   the agent's per-server config (agentd RFC 0012 §3.1); the supervisor computes the
   tag union over each granted child scope and **refuses/warns at every spawn**
   (agentd RFC 0012 §3.2). agentctl **surfaces operator-declared tags**; the agent
   **enforces** the granted-MCP-subset trust budget. agentctl never re-implements
   the per-spawn check.

`enableExec` (agentd RFC 0012 §3.6, tagged `egress`+`sensitive`) is likewise a
class **floor**: a set or inline server may request `exec`, but the agent only
registers it under `--enable-exec`, which `AgentClass.defaults.security.enableExec`
gates.

---

## 6. Ownership — one field, one home (no duplication)

The seam only works if every concern has exactly one home and the override
direction is unambiguous. This is the canonical map; where a concern appears in two
columns the relationship is **override** (Agent wins, sparsely), never duplication.

| Concern | `Agent`/`AgentFleet` (RFC 0003) | `AgentClass` (§3) | `IntelligenceService` (§4) | `MCPServerSet` (§5) |
|---|---|---|---|---|
| instruction / mode / subscribe / schedule | **owns** | — | — | — |
| agent **image** | inline `image` (classless) **or** forbidden when `classRef` set | **owns** (`image`) when referenced | — | — |
| **contract_version** pin / `requiredSurfaces` | none inline (classless ⇒ no pin; manage understood-major) + mirrors negotiated result in `.status` | **owns** (`contractVersionRange` / `requiredSurfaces`) | — | — |
| **substrate tier / tenancy / runtimeClassName** | sparse `substrate` **override** (re-checked) | **owns** (`substrate.*`) | — | — |
| node **placement** (selector/tolerations/affinity) | sparse **override** | **owns** (`placement.*`) | — | — |
| pod **resources** (cpu/mem) | sparse **override** | **owns** (`defaults.resources`) | — | — |
| **contract limits** box (maxSteps…) | sparse **override** (`limits`) | **owns** default (`defaults.limits`) | — | — |
| **drain / podGrace** | sparse **override** | **owns** default (`defaults.drain`/`podGraceSeconds`) | — | — |
| **security** (`allowTrifecta`/`enableExec`/`attachPolicy`) | may **tighten** (`security`) | **owns floor** (`defaults.security`) | — | — |
| model **endpoints / order / failover / breaker** | inline list **or** `intelligenceRef` | `defaultIntelligenceRef` (fallback) | **owns** (`endpoints`/`failover`) | — |
| provider **credential** location | — (never holds it in proxy mode) | — | **owns** (`endpoints[].credentialRef` → proxy) | — |
| **model id** preference | `model` (**override / select**) | — | supplies default (`endpoints[].models`) | — |
| **swap policy** / proxy topology | — | — | **owns** (`swapPolicy`/`proxy`) | — |
| **MCP servers + tags** | inline `servers` (**ADD**) | — | — | **owns** reusable bundle |
| **trifecta tag union** | **computed here** (inline ⊎ refs) | floor via `security.allowTrifecta` | — | contributes legs (`status.tagUnion`, informational) |
| surfaces to expose | `surfaces` (**override**) | (could default; v1: Agent-only) | — | — |

### 6.1 Merge semantics (answers RFC 0003 OQ4)

The effective spec the renderer compiles is `AgentClass.defaults ⊕ Agent.spec`, with
`⊕` defined field-class-wise so the result is deterministic and reviewable:

| Field class | Merge rule | Examples |
|---|---|---|
| scalars | **Agent overrides** if present | `podGraceSeconds`, `model`, `swapPolicy` |
| maps / structs | **field-wise deep-merge**, Agent wins per key | `limits`, `surfaces`, `resources`, `nodeSelector` |
| lists (general) | **Agent replaces wholesale** | `tolerations`, `affinity`, `topologySpreadConstraints` |
| **MCP servers** | **ADD (union)** — the one documented exception | `mcp.serverSetRefs` ⊎ `mcp.servers` (§5.2) |
| **intelligence** | **replace, never merge** (order is policy) | inline list **or** `intelligenceRef` **or** class default (§4.5) |
| security floors | **Agent may tighten, not loosen** | `allowTrifecta`/`enableExec` (§5.3) |

The substrate `tier` is a special case of "Agent may tighten, not loosen": a sparse
`Agent.spec.substrate.tier` override is accepted only if it satisfies the §3.3
binding rule for the effective tenancy (a tenant cannot downgrade `kata-hybrid` to
`stock-unix`); admission (agentctl RFC 0007) re-checks the merged result, never
trusts the override.

---

## 7. Worked example — one class, one pool, one set, one Agent

```yaml
# ── OPS (cluster admin) ───────────────────────────────────────────────────────
apiVersion: agents.x-k8s.io/v1alpha1
kind: AgentClass
metadata: { name: hardened }
spec:
  image: registry.example.com/acme/agent@sha256:abcd…
  contractVersionRange: ">=1.0 <2.0"
  requiredSurfaces: [management, metrics, events]
  substrate: { tier: kata-hybrid, tenancy: hostile, runtimeClassName: kata-clh }   # §3.3: stock-unix would be REJECTED
  defaultIntelligenceRef: { name: anthropic-pool }
  defaults:
    resources: { requests: { cpu: "500m", memory: 512Mi }, limits: { cpu: "2", memory: 2Gi } }
    limits: { maxSteps: 200, maxDepth: 4, treeTokenBudget: 4000000 }
    drain: { timeoutSeconds: 25 }
    podGraceSeconds: 30
    security: { allowTrifecta: false, enableExec: false, attachPolicy: readOnly }   # floors
  placement: { nodeSelector: { agentctl.dev/kata: "true" } }
---
# ── OPS (platform team, namespaced) ───────────────────────────────────────────
apiVersion: agents.x-k8s.io/v1alpha1
kind: IntelligenceService
metadata: { name: anthropic-pool, namespace: agents }
spec:
  endpoints:
    - name: opus-primary
      models: [claude-opus-4]
      dialect: anthropic
      backends: [{ service: anthropic-gw.models.svc, port: 443 }]
      credentialRef: { secretRef: { name: anthropic-key, key: api-key } }   # mounted into the PROXY, not the pod
  failover: { breakerThreshold: 3, breakerCooldownSeconds: 5 }
  swapPolicy: finish-on-old
  proxy: { topology: sidecar, image: registry.example.com/agentctl/intel-proxy@sha256:ef01… }
  placement: { modelAware: true }
---
apiVersion: agents.x-k8s.io/v1alpha1
kind: MCPServerSet
metadata: { name: triage-readers, namespace: agents }
spec:
  servers:
    - { name: inbox, transport: stdio, command: ["mcp-fs","--root","/watch/inbox"], tags: { "*": [untrusted_input] } }
    - { name: incidents, transport: stdio, command: ["mcp-incidents"], tags: { "*": [sensitive] } }
---
# ── DEV (agent author) — references all three; no ops knowledge encoded ────────
apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata: { name: triage, namespace: agents }
spec:
  classRef: { name: hardened }            # → image, substrate, tenancy, contract pin, floors, placement
  mode: reactive
  instruction: { configMapRef: { name: triage-instruction } }
  intelligenceRef: { name: anthropic-pool }   # → ordered endpoints, proxy, keyless dial (could omit → class default)
  model: claude-opus-4                    # selects the pool's model; model-aware placement applies (§4.4)
  mcp:
    serverSetRefs: [triage-readers]       # inbox(untrusted_input) + incidents(sensitive)
    servers:
      - { name: mailer, tags: { "send_*": [egress] } }   # inline ADD — composes egress onto the union
  subscribe: ["fs:file:///watch/inbox/*.json"]
  limits: { maxSteps: 120 }               # deep-merged over class default (maxDepth/treeTokenBudget inherited)
  security: { attachPolicy: deny }        # tightens the class floor (allowed); cannot loosen allowTrifecta
# Resolution: tier=kata-hybrid (hostile, enforced); image+contract from class; AGENTD_INTELLIGENCE → sidecar
# proxy (no key in pod); tag union = {untrusted_input, sensitive, egress} → FULL TRIFECTA → admission emits an
# ADVISORY warning (allowTrifecta=false floor unchanged); the agent's per-spawn Rule-of-Two (agentd RFC 0012
# §3.2) is the real control — the safe pattern is a reader(inbox)/actor(mailer) split, which it permits.
```

The split is the payoff: the developer's `Agent` names *what triage is*; the
operator's three objects decide *how it runs, what it costs, and where its secrets
live* — and the trifecta on this `Agent` is surfaced (advisory) at admission and
**enforced per-spawn by the contract**, not refused wholesale.

---

## 8. Versioning, rollout & compatibility

- **Same group/version as RFC 0003** (`agents.x-k8s.io/v1alpha1`), so the whole API
  set graduates together under the single-served-version + conversion-webhook + SVM
  posture (agentctl RFC 0003 §8, agentctl RFC 0005). `AgentClass` is cluster-scoped;
  the other two are namespaced.
- **The CRD `apiVersion` is decoupled from the agent `contract_version`** (agentct
  RFC 0003 §8): a new `contract_version` minor is not a CRD bump, and a CRD field
  addition is not a contract change. The `AgentClass` contract pin (§3.4) moves on
  the **contract's** clock.
- **Usable additively / removable cleanly.** Because every reference has an inline
  equivalent (agentctl RFC 0003 §2.2), adopting these CRDs is a refactor: extract a
  class, a pool, a set; repoint `Agent`s; delete the inline blocks. Removing them
  reverses cleanly. None gates an MVP milestone (brainstorm §16: these are not on
  the Phase-0 critical path).
- **Finalizers, not refcounts.** "Is this class/pool/set in use" is answered by a
  finalizer that **enumerates actual referrers** (agentctl RFC 0006), not an integer
  on `.status` (agentctl RFC 0003 §6.1) — deleting an in-use `AgentClass`/
  `IntelligenceService`/`MCPServerSet` is blocked while referrers exist.

---

## Non-goals

- **The reconcile/render loop, the capability probe, the merge implementation.**
  agentctl RFC 0006. This RFC fixes the *shape* and the *merge rules*; the
  controller behaviour is there.
- **The admission webhook that executes these rules** (the §3.3 binding-rule
  rejection, the §5.2 name-collision check, the §5.3 advisory trifecta union, the
  §4.2 `fallbackDirect`/hostile rejection, the security-floor enforcement). agentctl
  RFC 0007.
- **The intelligence egress proxy itself** — the data path, in-pool load balancing,
  dialect translation, per-backend health re-export, **cost/token governance and
  budget enforcement** (and the `EXIT_BUDGET`/back-pressure contract ask **P-cost**).
  agentctl RFC 0012. This RFC owns only the `IntelligenceService` **declaration**.
- **The substrate descriptor, CID/uds/socket discovery, and the §5 binding rule's
  text.** agentctl RFC 0002. This RFC owns the `AgentClass.substrate` field that
  rule reads.
- **The conversion-webhook machinery and alpha→GA graduation.** agentctl RFC 0005.
- **Provider/model procurement, pricing tables, or a model catalog.** A
  `credentialRef` points at a `Secret`; what the model costs and how budgets are
  governed is agentctl RFC 0012 (and the `EXIT_BUDGET` primitive, P-cost).
- **A `Task`/`Run` CRD, an `A2AGateway` CRD, a `ClaimService`/`AgentMesh` CRD.**
  Out of scope here (agentctl RFC 0003 §11 / brainstorm §2.1); A2A is agentctl RFC
  0013/0014, coordination is agentctl RFC 0011.
- **Any data-plane internals.** These CRDs describe contract-level ops intent; they
  MUST NOT encode one binary's flags, file layout, or version-specific behaviour
  (P0).

---

## Open questions

1. **The apiGroup string (shared with RFC 0003).** `agents.x-k8s.io` (proposed —
   vendor-neutral, SIG-upstreamable, P0-aligned) vs `agentctl.dev` vs the
   brainstorm's `agentctl.io`. This RFC inherits RFC 0003 §13.1's choice verbatim;
   the *shape* here is group-string-independent. Resolve once, for all CRDs.
2. **Should `IntelligenceService`/`ModelPool` COMPOSE with KServe `InferenceService`
   rather than reinvent endpoint management?** (brainstorm §12/§17 Q10.) A pool's
   `backends[]` could be a KServe `InferenceService` ref, letting agentctl reuse
   KServe's model-serving, autoscaling, and canarying instead of modelling backends
   directly. The tension: KServe owns the *serving* topology, but the **zero-secret-
   in-pod / egress-proxy / dialect-translation** posture (§4.2/§4.3) and the agentd
   RFC 0018 **across-pool failover** are agentctl's and have no KServe equivalent.
   Leaning: keep the `IntelligenceService` shape, allow a `backends[].inferenceServiceRef`
   as an alternative to a raw `service` so the two compose rather than compete.
   Confirm before GA.
3. **`AgentClass` cluster-scoped vs namespaced.** Cluster-scoped (chosen, StorageClass
   parity) centralizes ops policy but means a namespaced tenant cannot define their
   own class; a namespaced `AgentClass` (or a per-namespace allow-list of classes)
   may be needed for self-service multi-tenancy. Revisit with the tenancy model
   (agentctl RFC 0015).
4. **Where the cluster/namespace tenancy label authoritatively lives.** §3.3 reads
   `max(namespace label, class.substrate.tenancy)`; the **namespace label** as the
   authority assumes the platform owns namespace labels. If tenants own their
   namespaces, the authority must move to a cluster-policy object. Reconcile with
   agentctl RFC 0007/0015.
5. **`requiredSurfaces` vs a richer capability predicate.** §2 point 7 gates on the
   presence of `surfaces{}` keys (P0-clean). A class may eventually want to require a
   **minimum sub-schema version** (e.g. `metrics_schema >= 1.1`) — additive, but it
   needs the contract to version sub-schemas independently (agentd RFC 0014 §6.3,
   which it does). Decide whether `requiredSurfaces` grows value predicates or stays
   key-presence-only.
6. **Per-endpoint credential keying under P7.** §4.3 maps `credentialRef` onto the
   contract's index-keyed `AGENTD_INTELLIGENCE_TOKEN_<n>` env on the direct-dial
   path; agentd RFC 0018 §11 flags that name-keyed (not index-keyed) credentials may
   be preferable once stable endpoint names (P7) land. Align the direct-dial keying
   with whatever P7 freezes so there is one mental model.
7. **Deep-merge vs replace for `affinity`/`topologySpread`** specifically. §6.1
   replaces these lists wholesale; some operators will want additive placement.
   Confirm wholesale-replace (simpler, predictable) over a structural append.

---

## References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & Contract-as-Schema (P0): the contract-not-agentd
  framing these CRDs' field neutralization follows; the `agent-contract-client` the
  renderer reads to resolve a pool/class into contract surfaces.
- **agentctl RFC 0002** — substrate & transport abstraction: the `stock-unix`/
  `kata-hybrid`/`sidecar-emptydir` tiers `AgentClass.substrate.tier` selects, and
  the **§5 tenancy×substrate binding rule** this RFC owns the field for (§3.3).
- **agentctl RFC 0003** — `Agent`/`AgentFleet` CRDs: the reserved `classRef`/
  `intelligenceRef`/`mcp.serverSetRefs` fields these objects resolve, the merge
  open question (OQ4) §6.1 answers, the contract-negotiation-home (OQ6) §3.4 answers.
- **agentctl RFC 0005** — CRD versioning & conversion: the single-served-version +
  SVM posture §8 commits to.
- **agentctl RFC 0006** — operator reconcile: the renderer, the `CapabilityProbe`,
  the merge implementation, the referrer-enumerating finalizers.
- **agentctl RFC 0007** — admission validation ladder: enforces the §3.3 binding
  rule, the §4.2 hostile/`fallbackDirect` rejection, the §5.2 name-collision check,
  the §5.3 advisory trifecta union, and the security floors.
- **agentctl RFC 0011** — scaling plane: the `AgentFleet.template` these classes/
  refs apply to; the coordination server (not a CRD here).
- **agentctl RFC 0012** — intelligence plane: the **egress proxy** data path,
  in-pool LB, dialect translation, cost/budget governance (P-cost) — the *execution*
  of the `IntelligenceService` declaration.
- **agentctl RFC 0015** — security & multi-tenancy: the hostile-tenancy mandate and
  RBAC behind the `allowTrifecta` override and the tenancy authority (OQ3/OQ4).

**Contract spec (the reference implementation, agentd RFCs)**

- **agentd RFC 0014** — control-plane contract umbrella: the manifest spine (§5),
  `surfaces{}` as the single discovery point (§6.2), contract versioning/negotiation
  (§6.3), the downward-API env convention incl. `AGENTD_INTELLIGENCE_TOKEN[_n]`/`_FILE`
  (§6.4) — the negotiation §3.4 keys off and the env §4.3 injects.
- **agentd RFC 0015** — management & control surface: the manifest's `limits` box
  (§5.2) `AgentClass.defaults.limits` defaults, `surfaces{}` keys `requiredSurfaces`
  gates on.
- **agentd RFC 0006** — intelligence transport & wire: the `parse_intelligence_uri`
  the endpoint list parses to, the dialects (openai/anthropic), the `resolve()`
  secrets front door (§6) and `{{secret:NAME}}` posture the zero-secret rule (§4.3)
  rides.
- **agentd RFC 0018** — intelligence transport resilience: the ordered `--intelligence`
  failover list, sticky-primary, the breaker knobs (§4.2), all-down backoff (§6),
  hot-swap quiesce-switch-resume + `--model-swap` (§5.3), model discovery + the
  per-endpoint model arrays and stable endpoint **names** (§4.3/§11 — contract ask P7).
- **agentd RFC 0012** — security posture: the trifecta tag vocabulary + per-tool
  glob tagging (§3.1), the spawn-chokepoint Rule-of-Two trust-budget check (§3.2),
  the `exec` gate (§3.6), and the `Secret`-has-no-`Serialize` invariant (§3.7) the
  zero-secret-in-pod rule (§4.3) depends on.
- **agentd RFC 0017** — declarative config & hot reload: the `--mcp`/`--mcp-config`
  refs+inline ADD deviation (§3.3) `MCPServerSet` composition mirrors, and the reload
  trigger the backend-move one-object edit (§4.5) signals.
- **agentd RFC 0016** — telemetry & lifecycle contract: the frozen `agentd_intel_*`
  metric series the stable endpoint **names** (P7) label.

**Contract asks raised or cited by this RFC** (agentctl brainstorm §14): **P7**
(per-endpoint model arrays in the manifest + stable operator-assigned endpoint names
for metric labels — §4.4), **P-cost** (a clean budget-exhausted signal for cost
governance — deferred to agentctl RFC 0012, §Non-goals). The agentd-branded
contract surfaces these CRDs resolve into (`AGENTD_INTELLIGENCE`,
`AGENTD_INTELLIGENCE_TOKEN[_n]`, the `AGENTD_INTEL_BREAKER_*` /
`AGENTD_INTEL_ALLDOWN_BACKOFF` failover env family, `agentd_intel_*`, `agentd://`,
`--model-swap`) are flagged for the **P0 contract-extraction** open question
(agentctl RFC 0001 §9).

*Where this RFC and a contract spec disagree on the wire, the contract wins and this
RFC is corrected; where this RFC needs a primitive the contract does not expose (P7,
P-cost), it is a contract ask — never a leak of cluster logic into the agent.*
