# agentctl RFC 0006: Operator reconcile & the manifest-driven capability model

**Status:** Proposed (agentctl foundational track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the controller that turns the declarative API (agentctl RFC 0003) into running, observable workloads and projects live state back, driven entirely by the contract's capabilities manifest

> **Manifest-driven, never agent-assumed (P0).** The operator renders and manages
> **any agent that conforms to the Agent Control Contract**. It learns what an
> image can do from the contract's **capabilities manifest** (`--capabilities` /
> `agent://capabilities`), keys every rendering decision off the manifest's
> `surfaces{}` discovery block (agentd RFC 0014 §6.2), and **drives only what is
> advertised** — never a private flag, file layout, or assumed feature of one
> binary. A surface absent ⇒ not driven. A contract major it does not understand ⇒
> managed by liveness + exit codes + logs only (agentd RFC 0014 §8). Where this RFC
> cites a concrete value it names the **reference implementation** (agentd RFCs
> 0014–0020) as *where the contract is presently written down*, not as a dependency.

> **The operator is the single writer of `.status`, and it is level-triggered and
> idempotent.** Desired state is derived from `.spec` + the observed cluster + the
> manifest — **never** from the triggering event. `.status` is patched **only on a
> `DeepEqual` change** (no hot-loops, §8). The operator **cannot speak the
> unix/vsock management profile to a pod** — only the node-agent is socket-adjacent
> (agentctl RFC 0008) — so capability knowledge enters by **two separate paths**
> (§4): a static probe of the image (render time) and a live snapshot from the
> node-agent (status time).

---

## 1. Problem / Context

agentctl RFC 0003 fixes the **shape** of `Agent`/`AgentFleet` — the `.spec` a user
applies, the `.status` they read, the mode→workload table, the CEL invariants. This
RFC owns the **controller behaviour** that makes that shape live: the
level-triggered, idempotent reconcile loop that compiles an admission-validated CR
(agentctl RFC 0007) into a workload + config + substrate wiring, and projects a
curated live status back onto the CR. It is the engine; RFC 0003 is the schema the
engine reads and writes.

Three forces shape every decision here, and they are in tension:

1. **The operator is not socket-adjacent.** It runs as an ordinary Deployment with
   a kube client; it has no path to the unix/vsock management socket of a pod (that
   is the node-agent's exclusive role — agentctl RFC 0002 §3, RFC 0008). Yet it must
   (a) know what an *image* can do **before** any pod exists, to render correctly,
   and (b) reflect what a *running instance* actually is, for status. Those are two
   physically different questions reached over two different paths, and conflating
   them is a category error this RFC refuses to make (§4).

2. **It must never assume agent capabilities.** Under P0 the data plane is any
   conformant agent — possibly a second vendor's, possibly a build with half the
   surfaces compiled out. The operator cannot render a management socket for an
   agent that does not advertise the `management` surface, cannot render a
   config-validate init-container for an agent that does not advertise
   `config_validate`, cannot render an HTTP probe for a networkless pod (agentctl RFC
   0002 §8). Every such decision must be **driven off the manifest's `surfaces{}`**,
   the contract's
   single discovery point (agentd RFC 0014 §6.2), and degrade gracefully when a
   surface is absent (agentd RFC 0014 §8).

3. **Reconcile correctness is unforgiving.** A controller that requeues on its own
   status writes hot-loops the apiserver; one that wakes on the wrong edge never
   reconciles live state; one that writes `.spec.replicas` on a KEDA-autoscaled
   fleet fights the HPA forever; one whose finalizer depends on a fail-closed
   webhook strands every object in `Terminating`. These are not hypotheticals — the
   brainstorm red-team (§3.2) caught each as a load-bearing bug. This RFC writes the
   discipline that avoids them (§8) as normative, not advisory.

The resolution, stated once: **a level-triggered `kube::runtime::Controller` per
CRD on one shared manager (agentctl RFC 0001), a two-path capability model (static
probe → render; live node-agent snapshot → status), a digest-keyed capability cache
fed by a one-shot `CapabilityProbe`, manifest-driven rendering that drives only
advertised surfaces, finalizer choreography that drains cleanly without depending on
the webhook, and a single `DeepEqual`-guarded writer of `.status`.**

This RFC owns: the controller/manager/watch topology, the two-path capability
model, the `CapabilityProbe` + cache, the surfaces→rendering decision map, the
finalizer/drain-on-delete choreography, the reconcile-correctness discipline, and
the end-to-end reconcile flow. It does **not** own: the CRD schema and status
taxonomy (agentctl RFC 0003 — this RFC *drives* the rendering RFC 0003 specifies);
the admission webhook (agentctl RFC 0007 — this RFC consumes admission-validated
CRs and shares the capability cache with it); the substrate descriptor, discovery,
and attestation (agentctl RFC 0002 / RFC 0008); the KEDA external scaler and
coordination server (agentctl RFC 0011); CRD versioning/conversion (agentctl RFC
0005).

---

## 2. Decision — the reconcile contract (eight points)

1. **Two controllers, one manager, label-scoped shared caches.** A single
   leader-elected operator binary runs an `AgentReconciler` and an
   `AgentFleetReconciler` on one `kube::runtime` manager, sharing `watcher`-backed
   `reflector`/`Store` caches (agentctl RFC 0001 §3). Caches over rendered children
   are **restricted to the `agents.x-k8s.io/managed=true` label** so server-side-apply
   drift detection sees only agentctl-owned objects (§3).

2. **Strictly level-triggered and idempotent.** Each reconcile derives desired
   state from `.spec` + the observed cluster + the cached manifest — **never** from
   the triggering event's contents. A reconcile is safe to run any number of times
   and at any time; the trigger only decides *when*, never *what* (§3, §8).

3. **The capability model is two-path and the paths never cross.** **STATIC**
   capability (the `--capabilities` manifest of an *image*, pinned by image digest +
   build-feature set, obtained by the `CapabilityProbe`, cached) is used at **render
   time**. **LIVE** capability (the `agent://capabilities` / `inventory` / `status`
   resources of a *running instance*, read by the node-agent and published as a
   snapshot) is used at **status time**. The operator stays the single `.status`
   writer; the node-agent never writes `Agent.status` (§4).

4. **Rendering is manifest-driven: drive only advertised surfaces.** Every
   workload/config/probe/substrate decision keys off the manifest's **`surfaces{}`**
   discovery block — the contract's single discovery point (agentd RFC 0014 §6.2) —
   and **never** off `build_features`, whose values are opaque/agent-defined and MUST
   NOT be a branch condition (agentctl RFC 0003 §6.1). A surface the image cannot
   serve is **not wired**; an unknown contract major degrades to liveness + exit
   codes + logs (agentd RFC 0014 §8). The capability→render decision map is §6.

5. **The `CapabilityProbe` is a one-shot, side-effect-free image probe, cached by
   `(digest + feature-set)`.** The operator obtains a manifest for an unseen image
   by running the contract's `--capabilities` entrypoint (a short-lived Job; an
   init-container variant for the same-pod case), which is contractually side-effect
   free and exits `0` (agentd RFC 0015 §5.2). The cache stores **only digest-stable
   facts**; per-CR surface *values* are derived from the rendered config, never the
   probe (§5).

6. **`.status` has exactly one writer, guarded by `DeepEqual`.** The operator
   patches `.status` (via the status subresource) only when the projected status
   differs from the stored one, and stamps `observedGeneration`. Pod-driven live
   edges wake reconcile **without** the operator owning pods (it does not, and
   cannot, `Owns(Pod)` — §8.2); they are routed by a managed-pod label mapper or a
   node-agent-owned watchable object.

7. **KEDA owns `.spec.replicas`; the renderer omits it.** For any autoscaled
   workload the operator server-side-applies a workload that **omits**
   `.spec.replicas`, leaving the KEDA-generated HPA the sole writer (agentctl RFC
   0003 §4.3). The operator never includes that field in its apply patch for an
   elastic fleet (§8.4).

8. **Finalizers choreograph a clean drain on delete, and never depend on the
   webhook.** A CR finalizer runs lame-duck → drain → clean exit `0` on CR deletion
   (agentd RFC 0011/0015); per-pod scale-down drain is the **pod's** SIGTERM path,
   not the CR finalizer (§7). The operator's own finalizer writes are exempted from
   the fail-closed admission webhook (agentctl RFC 0007) so a webhook outage never
   strands an object in `Terminating` (§7.3).

These eight are final for the reconcile surface. Each defers to its owning sibling
RFC where it touches another plane (noted inline).

---

## 3. Controllers, the manager, watches & the Store

### 3.1 The controller set and the shared manager

One operator binary, leader-elected (a `coordination.k8s.io` `Lease` via
`kube-leader-election` / `kubert`, agentctl RFC 0001 §3), runs **two**
`kube::runtime::Controller`s on **one** manager so they share informer caches and a
single kube client:

```
operator (Deployment, ≥2 replicas, 1 active via Lease leader election)
  ├── shared kube client + shared watcher/reflector Stores (label-scoped)
  ├── AgentReconciler        : For(Agent)
  │     .owns(Job) .owns(CronJob) .owns(Deployment) .owns(StatefulSet)
  │     .owns(ConfigMap)                       // the two rendered config CMs (§6.4)
  │     .watches(Pod,   managed-pod → Agent)   // live-status edge (NOT .owns — §8.2)
  │     .watches(AgentClass, class → Agents)   // resolve-on-class-change (RFC 0004)
  ├── AgentFleetReconciler   : For(AgentFleet)
  │     .owns(Deployment) .owns(StatefulSet) .owns(ScaledObject) .owns(ConfigMap)
  │     .watches(Pod, managed-pod → AgentFleet)
  └── CapabilityProbe driver : owns the probe Jobs + the digest-keyed cache (§5)
```

Both controllers use `kube`'s server-side-apply (`Patch::Apply` with
`fieldManager: "agentctl"`, agentctl RFC 0001 §3) for every rendered child, so the
operator is declarative and conflict-aware: a field another manager owns (the HPA's
`.spec.replicas`, §8.4) is never clobbered because the operator simply never lists
it in its apply.

### 3.2 ownerReferences + garbage collection

Every **per-CR** rendered child (Job/CronJob/Deployment/StatefulSet, both ConfigMaps,
the `ScaledObject`) carries an **`ownerReference` to the CR** with `controller: true`
and `blockOwnerDeletion: true`. (The `CapabilityProbe` Jobs are **NOT** per-CR
children — the capability cache is shared across all CRs by image digest, so a probe
Job is owned by the **singleton cache object** (or the operator `Deployment`), never
by one CR; §3.1, §5.1. Owning it from a CR would GC a digest's probe when that CR is
deleted even though other CRs share the digest.) Deletion of the CR therefore
**garbage-collects the whole per-CR tree** via Kubernetes' built-in GC — the operator does
**not** hand-delete children on CR deletion (the finalizer's job is the *graceful
drain*, §7, not cascade). This is the standard owner-GC contract and it is the
reason the reconcile loop can be "get CR; if absent, return" (the children are
already being reaped).

Two GC subtleties this RFC fixes:

- **Pods are NOT owned by the CR.** A Pod's controller `ownerReference` points to its
  `ReplicaSet`/`Job`/`StatefulSet`, **not** to the `Agent` (Kubernetes sets exactly
  one controller owner). The operator therefore reaches pods by **label**, never by
  owner-ref traversal (§8.2). It does not add a second owner-ref to pods.
- **Reference counting is finalizer-enumerated, not an integer.** Cross-object
  references (an `MCPServerSet`/`IntelligenceService`/`AgentClass` "in use by" an
  `Agent`) are resolved by **enumerating actual referrers** under a finalizer on the
  referenced object (agentctl RFC 0004), never by an `inUseBy: int` in status that
  would drift (agentctl RFC 0003 §6.1).

### 3.3 The watch / informer / Store model

The operator is a `watcher` + `reflector`/`Store` system (agentctl RFC 0001 §3,
near-parity with controller-runtime's shared informers):

- **`For(&Agent)` / `For(&AgentFleet)`** is the primary watch — the spec source of
  truth. It carries the `GenerationChangedPredicate` so trivial metadata churn does
  **not** wake reconcile (scoped to the primary source ONLY — §8.3).
- **`.owns(child)`** wakes reconcile when a rendered child changes (a Job completes,
  a Deployment's `readyReplicas` moves) so workload status flows into `.status`.
- **`.watches(Pod, mapper)`** is the live-status edge: managed pods are labelled and
  mapped back to their owning CR by a closure (the kube-rs analogue of
  `EnqueueRequestsFromMapFunc`), because the operator cannot `Owns(Pod)` (§8.2).
- **`.watches(AgentClass, mapper)`** re-resolves dependent `Agent`s when an ops-owned
  `AgentClass` default changes (agentctl RFC 0004).

The **label-scoped cache** is load-bearing: the `watcher` for rendered children is
created with a `label_selector` of `agents.x-k8s.io/managed=true`, so the reflector
`Store` holds only agentctl-managed objects. (The brainstorm names this label
`agent.agentctl.io/managed`; this RFC uses the **proposed** `agents.x-k8s.io` group
prefix from agentctl RFC 0003 §2 point 3, but the API-group string is still **open**
— RFC 0003 §13 Open Question 1 weighs `agents.x-k8s.io` vs `agentctl.dev` vs the
brainstorm's `agentctl.io`. The managed-label prefix **tracks whichever group string
RFC 0003 finally adopts** (with `agentctl.dev` as the fallback); it is not the
reference-era `agent.` spelling.) If the cache were
unfiltered, SSA drift detection and owned-object enqueue would silently break on a
busy cluster (too much churn) — the label is what makes `.owns()`/`.watches()`
correct at scale.

### 3.4 Leader election & requeue runtime

Leader election ensures **exactly one** active reconciler across replicas (the
others stand by on the `Lease`), so there is never a two-writer race on `.status` or
on SSA. **Leader election gates only reconcile *actuation* (the SSA writes and the
`.status` patch), not the informers:** each replica runs its **own** `watcher`/
`reflector` `Store`s, populated independently of the `Lease`, so a non-leader replica
has a warm cache. This is load-bearing for the admission webhook (agentctl RFC 0007),
which is served by **every** replica and reads these same caches for cross-object
checks — a webhook on a non-leader replica must not see an empty Store. The
`Controller` runtime supplies the requeue/backoff machinery: a
successful reconcile returns an `Action` (`Action::requeue(jittered 5–10m)` as a
long level-triggered backstop, or `Action::await_change()` when nothing more is
pending); a failed reconcile is retried with the runtime's **exponential backoff**
via `error_policy` (§8.5). The edge triggers (§3.3) do the real work; the backstop
exists only to self-heal a missed edge.

---

## 4. The two-path capability model — STATIC (render) vs LIVE (status)

The single most important structural decision in this RFC: **capability knowledge
enters by two distinct paths, used for two distinct purposes, and they are kept
separate.** This is forced by the operator not being socket-adjacent (§1): it can
*run an image* (a Job) but it cannot *talk to a running pod's socket* (only the
node-agent can).

```
                       ┌─────────────────────────────────────────────────────────┐
   STATIC path         │  IMAGE (by digest) — "what CAN this build do?"            │
   (render time)       │  obtained by: CapabilityProbe Job → `--capabilities`      │
                       │  (side-effect free, exits 0; agentd RFC 0015 §5.2)        │
                       │  cached by (digest + feature-set); DIGEST-STABLE facts:   │
                       │   contract_version, build_features, config_schema,        │
                       │   the build-gated surface KEY SET, operator_tools         │
                       └───────────────────────────┬─────────────────────────────┘
                                                    │ used by renderWorkload()/renderConfig()
                                                    ▼ + by the admission webhook (RFC 0007)
                          ┌──────────────────────────────────────────────┐
                          │  RENDERED: Job/Deploy/STS + 2× ConfigMap +    │
                          │  downward-API env + substrate wiring (RFC 0002)│
                          └──────────────────────────────────────────────┘
                                                    │ agent starts, ADVERTISES live surfaces
                                                    ▼
                       ┌─────────────────────────────────────────────────────────┐
   LIVE path           │  RUNNING INSTANCE — "what IS this instance right now?"    │
   (status time)       │  obtained by: node-agent reads agent://capabilities /    │
                       │  inventory / status over the attested socket (RFC 0002/8) │
                       │  → publishes an observed Snapshot (node-agent owns it)     │
                       └───────────────────────────┬─────────────────────────────┘
                                                    │ operator reads Snapshot, projects status
                                                    ▼ (single writer; DeepEqual-guarded)
                                              Agent.status (RFC 0003 §6)
```

### 4.1 STATIC capability — the image, at render time

Before a pod exists, the renderer must answer image-level questions: *Which surface
keys can this build ever advertise (does the key-set include `management`)? What
`contract_version` major? What `config_schema` version validates the rendered config?
Does it expose an exec-health verb (agentctl RFC 0002 §8, contract ask P1)?* These are
properties of the **image**, stable per digest, read from the manifest's `surfaces{}`
key-set (never inferred from a `build_features` value), and the operator gets them
from the
`CapabilityProbe` (§5): a one-shot `--capabilities` run that is contractually
side-effect free, runs before any MCP connect / LLM call / socket bind, and emits
the manifest with live fields at their pre-connect/unknown values (agentd RFC 0015
§5.2). The operator deserializes the probe output through the generated
`agent-contract-client` (agentctl RFC 0001 §4.2), validates it against the published
manifest schema, and renders against it.

**What STATIC capability decides (render time):** whether to wire a management socket
+ node-agent discovery at all; which probe kind to render (exec-health verb vs HTTP);
whether to render the `--validate-config` init-container (P6); whether to render the
two-ConfigMap hot-reload partition vs a single immutable ConfigMap (RFC 0017
`hot_reload`); the `podFailurePolicy` from the exit-code table version the build
honours. (A fleet's ownership **regime** — claim via a coordination MCP server, or
shard config — is read from `AgentFleet.spec` (agentctl RFC 0003 §4.2), not from a
manifest surface; it is not a STATIC capability fact.) The full map is §6.

### 4.2 LIVE capability — the running instance, at status time

Once a pod runs, the **node-agent** (the only socket-adjacent component, agentctl RFC
0002 §3 / RFC 0008) holds the attested management connection and reads
`agent://capabilities` (re-read on hot-reload/model-swap via the `updated`
notification), `agent://inventory` (the live subagent tree), and `agent://status`
(identity + lifecycle flags) — agentd RFC 0015 §5.2–5.4. It publishes an **observed
snapshot** correlated to the pod by `identity.uid` (agentd RFC 0015 §6). The operator
**reads** that snapshot and projects the curated `.status` (RFC 0003 §6): the
*advertised* `surfaces{}` (vs the *intended* `spec.surfaces`), the negotiated
`contract.version`, the `health` flags, the `Ready` condition reasons
(`ManagementUnreachable`/`AttestationFailed`, agentctl RFC 0002 §3, §7).

**The node-agent never writes `Agent.status`.** The operator is the single writer
(§2.6). The transport by which the snapshot reaches the operator — a node-agent-owned
watchable object the operator watches (preferred, gives a clean reconcile edge) vs
the operator querying the node-agent's management API (agentctl RFC 0009 access path)
— is deliberately left to agentctl RFC 0008 and recorded as an open question (§12).
Either way, the operator **tolerates an unreachable node-agent**: a missing snapshot
sets `Ready=False`/`ManagementUnreachable`, never an error that blocks rendering
(§8.1).

### 4.3 Why two paths and not one

A single path is tempting and wrong in both directions:

- **"Just probe the running pod for everything"** is impossible: the operator cannot
  reach the pod's socket. Only the node-agent can, and routing every render decision
  through the node-agent would make rendering depend on node-agent liveness and on a
  pod *already existing* (a chicken-and-egg: you cannot render the pod by asking the
  pod).
- **"Just use the static manifest for everything"** is wrong for status: the static
  manifest reports live fields (`intelligence.healthy`, subagent counts,
  `draining`/`paused`) at their unknown/zero values (agentd RFC 0015 §5.2), so it
  cannot describe a *running* instance. Status needs the LIVE read.

The two paths answer two genuinely different questions — *what can this image do* vs
*what is this instance doing* — and the architecture makes that explicit rather than
papering over it.

---

## 5. The `CapabilityProbe` & the digest-keyed capability cache

### 5.1 How the operator obtains a manifest for an image it has never run

The operator must not assume; it must **probe**. For an image digest it has not seen,
it runs the contract's static capabilities entrypoint and reads the result:

- **Primary: a short-lived `CapabilityProbe` Job.** The operator renders a one-shot
  Job (`restartPolicy: Never`, tight `activeDeadlineSeconds`, the exact target image,
  no intelligence binding, no MCP servers, no network) whose command is the
  contract's static-capabilities invocation — reference impl: `agent --capabilities`
  (agentd RFC 0015 §5.2). The contract guarantees this is **side-effect free, binds
  no socket, makes no LLM call, and exits `0`** (agentd RFC 0011 §3.3 discipline,
  agentd RFC 0015 §5.2). The operator captures the JSON (from the pod's logs or a
  shared `emptyDir` file), validates it against the published manifest schema
  (agentctl RFC 0001 §4.1), and caches it. The probe Job carries an `ownerReference`
  to the **singleton cache object** (or, absent one, the operator `Deployment`) — a
  concrete same-namespace object, **never** a CR (§3.2) — and is GC'd after capture.
- **Variant: an init-container probe** for the same-pod case (when the operator wants
  the manifest *and* the workload in one shot — e.g. to confirm the digest's
  capability set matches the cache before the main container starts). The probe runs
  as an init-container writing the manifest to an `emptyDir` the node-agent or a
  lightweight reader picks up. This shares the rendering with the §7 lame-duck/drain
  init pattern and is the natural fit where a probe Job is undesirable.

The operator **never execs a tenant image synchronously inside the admission
webhook** (image-pull on the apply critical path; running tenant binaries in the
control plane — agentctl RFC 0003 §7, RFC 0007). The probe is an asynchronous,
sandboxed Job, and admission validates against the **cached schema** while the probe
populates the cache out of band.

### 5.2 The cache: keyed by `(image digest + build-feature set)`, digest-stable facts only

```jsonc
// CapabilityCache entry — one per (digest + feature-set). The KEY is digest-stable;
// so is the VALUE. Per-CR, config-driven values are DELIBERATELY ABSENT here.
{
  "key": {
    "digest":      "sha256:9f2c…",          // resolved image digest, NOT a tag (§5.3)
    "featureSet":  ["serve-mcp","vsock","metrics","events","hot-reload"]
  },                                         // build_features — the cargo/build cfg set
  "contractVersion": "1.0",                  // contract_version major.minor (negotiate on major)
  "configSchema":    { "version": "1.0", "ref": "config.schema.json#…" }, // RFC 0017 (P6)
  "surfaceKeySet":   ["management","operator_tools","metrics","metrics_schema",
                      "events","hot_reload","config_validate","exit_codes"],
                                             // the surface KEYS this BUILD can ever advertise
  "operatorTools":   ["drain","lame-duck","cancel"], // surfaces.operator_tools (agentd RFC 0015 §5.2)
  "exitCodesTag":    "RFC-0011-§5",          // the exit-code table version → podFailurePolicy (§6)
  "execHealthVerb":  false,                  // P1 advertisement (agentctl RFC 0002 §8); false until P1
  "probedAt":        "2026-06-27T09:40:00Z",
  "lastSeen":        "2026-06-27T10:15:00Z"
}
```

The red-team correction this RFC bakes in (brainstorm §3.1): **cache only
digest-stable facts.** The cache answers "what can this *build* do" — `build_features`,
`contract_version`, `config_schema`, the **set of surface keys** the build can
advertise, the operator-tool set, the exit-code table tag. It deliberately does **not**
cache the per-CR surface *values* — the management address (`unix:PATH` vs `vsock:PORT`),
the configured `mode`/`model`, the resolved `limits` — because those are **config-driven**,
not image-driven, and the operator derives them from the **rendered config**, not the
probe. A cache that mixed config-driven values would be wrong for the next `Agent`
sharing the digest.

### 5.3 Keying, invalidation, GC

- **Key on the resolved digest, never a tag.** The operator resolves `spec.image`
  (or the `AgentClass` image, agentctl RFC 0004) to an immutable digest before
  probing/caching; a moving tag (`:latest`) would silently serve a stale manifest.
  This matches agentctl RFC 0001 §8: pin by `(contract major.minor + schema digest)`,
  never by a mutable reference. The **feature-set is part of the key** because the
  reference impl's manifest/surfaces are build-conditional (`cfg!`), so the same
  semantic version with a different cargo feature set is a *different* capability set
  (agentctl RFC 0001 §8).
- **Invalidation is by digest identity.** A new digest is a cache miss → a new probe.
  The cache is therefore **append-mostly**; an entry is never mutated in place (a
  digest's capabilities do not change).
- **GC by TTL + liveness.** An entry not referenced by any live CR for a TTL window
  is evicted (`lastSeen` ages out). The cache survives operator restarts by being
  persisted (a `ConfigMap`/`Secret`-backed store or a small CRD; the persistence
  shape is an open question, §12) so a leader handover does not re-probe every digest.
- **Shared with admission.** The same cache backs the admission webhook's
  config-schema validation (agentctl RFC 0007): the webhook reads `configSchema` for
  the image's contract major; the renderer reads `surfaceKeySet`/`exitCodesTag`.
  One probe, two consumers.

---

## 6. Manifest-driven rendering — drive only what `surfaces{}` advertises

This is the operative meaning of "manifest-driven, never agent-assumed." The renderer
(agentctl RFC 0003 §5 specifies the *table*; this RFC *implements* it) keys every
decision off the cached STATIC capability (§5) and degrades gracefully where a
surface is absent (agentd RFC 0014 §8). It **drives only what is advertised**.

### 6.1 The capability → rendering decision map

| Capability fact (STATIC, from the manifest) | If present | If absent (graceful degradation) |
|---|---|---|
| `contract_version` **major** understood | render full management wiring + status projection | render workload **only**; `ContractCompatible=False`; manage by **liveness + exit codes + logs** (agentd RFC 0014 §8) |
| `surfaces.management` key present in the manifest key-set | wire the management socket + substrate descriptor (RFC 0002 §6); node-agent discovers + attests | no management socket; `surfaces.management:false` in status; lifecycle via SIGTERM only (§7) |
| `surfaces.hot_reload` (RFC 0017) | render **two ConfigMaps** — stable-named reloadable + content-hashed restart-only (§6.4) | render **one** content-hashed immutable ConfigMap; reload = pod recreate |
| `surfaces.config_validate` / `config_schema` (RFC 0017, P6) | render the `--validate-config` **init-container** (ground-truth, RFC 0007); admission validates against cached schema | skip the init-container; rely on the webhook JSON-schema rung only; `ConfigValidatedAtRuntime` unset |
| exec-health verb advertised (P1, agentctl RFC 0002 §8) | render **`exec`** liveness/readiness probes invoking the verb (the only option on networkless tiers) | networkless tier **not shippable** (RFC 0002 §8); networked tier falls back to `httpGet /readyz` only if an HTTP health surface is advertised |
| `surfaces.metrics` (addr / `false`) | render metrics scrape wiring (agentctl RFC 0010); TCP or in-socket per RFC 0002 §3 (P4) | no scrape target; metrics columns/`top` empty |
| `surfaces.operator_tools` ⊇ `drain`/`lame-duck`/`cancel` | finalizer choreography uses the management tools (§7) | finalizer falls back to **SIGTERM-only** drain (pod delete) |
| fleet ownership: `work.claim` references a coordination MCP server (claim mode, agentctl RFC 0003 §4.2 / agentd RFC 0019 §3) **or** shard config + the `--shard auto/N` ask (shard mode, P3) | render fleet ownership wiring + KEDA `ScaledObject` (agentctl RFC 0011) | reject the fleet at admission (RFC 0007) — a fleet needs an ownership regime (claim coordination server or shard config) |
| `surfaces.a2a` (P2 — **not in the frozen manifest**) | wire the A2A surface (agentctl RFC 0013) | **inert**; set `A2AUnsupported`; do not wire A2A (RFC 0003 §3.3) |

The rule is uniform: **the renderer asks the manifest, not the binary's name.** A
second-vendor agent that advertises `hot_reload:false` gets a single immutable
ConfigMap with no special-casing; one that omits the exec-health verb on a networkless
tier is held back, not crash-looped. Capability *absence is never an error* (agent
RFC 0015 §8) — it is a rendering branch.

### 6.2 Surface VALUES are config-derived, not probed

A subtlety the two-path model demands: the **presence** of a surface key is a STATIC
(image) fact, but its **value** is a config fact. That the image **can** serve
management is the presence of the `surfaces.management` **key** in the manifest
key-set (STATIC — read from `surfaces{}`, never inferred from a `build_features`
value); its *value* (`unix:/run/agent/mgmt.sock` vs `vsock:5005`) is set by the
rendered config + the substrate tier (agentctl RFC 0002 §6, RFC 0003 §3.2). The renderer therefore composes: *"the image CAN serve
management (STATIC) → the substrate tier says bind `unix:…` (RFC 0002) → render that
bind instruction into config + downward-API env."* The agent then **advertises** the
actual value at runtime, which the node-agent reads and the operator projects into
`.status.surfaces` (LIVE). Three layers — *can / configured-to / actually-does* — and
the renderer only ever asserts the first two.

### 6.3 The downward-API env and substrate wiring

The renderer injects the contract's downward-API env convention (agentd RFC 0014 §6.4,
the exact set in agentctl RFC 0003 §9) — `*_POD_NAME`/`_UID`/`_NAMESPACE`/`_NODE_NAME`,
`*_POD_GRACE_SECONDS` (= `terminationGracePeriodSeconds`, preserving `drain < grace`),
and the per-endpoint intelligence credential env (agentd RFC 0014 §6.4). These are
**descriptive, never load-bearing** (agentd RFC 0015 §6). The substrate wiring (the
per-pod hostPath socket subdir via `subPathExpr`, or the Kata `runtimeClassName`, or
the sidecar) is rendered per the tier the `AgentClass`/`spec.substrate` selects
(agentctl RFC 0002 §6, RFC 0003 §3.2). The renderer carries the **`AGENT_SHARD`
defect** (agentctl RFC 0003 §9.1, contract ask P3): it injects only `N` (uniform,
template-legal) and relies on the agent deriving `K` from the ordinal via `--shard
auto/N` when P3 is present, falling back to the init-container shim only on a
non-scratch image.

### 6.4 The two-ConfigMap partition (drives hot reload correctly)

A single content-hashed immutable ConfigMap is **never re-projected** into a running
pod, so inotify/SIGHUP hot-reload cannot fire from it (brainstorm §3.2). When the
image advertises `hot_reload` (agentd RFC 0017), the renderer partitions the rendered
config into **two** ConfigMaps:

```
agent "triage"
  ├── triage-config-reloadable   (STABLE NAME)        → mutated IN PLACE on a reloadable change
  │     reloadable partition: MCP servers, subscriptions, model/intel params
  │     (the RFC 0017 reloadable allowlist) → inotify/SIGHUP fires; no pod restart
  └── triage-config-<hash>       (CONTENT-HASHED)      → immutable; a change is a NEW object
        restart-only partition → template references the new name → rolling restart
```

This is the mechanically correct way to make "edit config, agent reloads without
restart" work and "edit a restart-only field, agent rolls" work — and it is **gated
on the `hot_reload` surface** (an agent without it gets one immutable ConfigMap and
reload-by-restart). The reloadable-vs-restart partition itself is the contract's
(agentd RFC 0017); the renderer reads it and lays out the two objects.

---

## 7. Finalizers & drain-on-delete choreography

### 7.1 Two distinct drain paths (do not conflate them)

| Trigger | Path | Mechanism |
|---|---|---|
| **CR deletion** (`kubectl delete agent …`) | **CR finalizer** | the operator runs lame-duck → drain → clean exit `0` via the management tools, then removes the finalizer (§7.2) |
| **Fleet scale-down** (KEDA deletes a replica pod) | **the POD's SIGTERM path** | `terminationGracePeriodSeconds` + SIGTERM → the agent's own drain choreography → claim-release (agentd RFC 0019 §6); the CR finalizer does **not** fire (no CR deletion) |

A CR finalizer fires only on CR deletion, so it is the wrong tool for per-pod
scale-down; that drain rides the pod's grace + the agent's SIGTERM handling (agent
RFC 0011 §4.2). The operator's job for scale-down is to render the right
`terminationGracePeriodSeconds` (> `drain.timeoutSeconds`, the CEL invariant in RFC
0003) and let the kubelet + agent do the rest. Conflating the two paths is the
brainstorm §3.2 trap.

### 7.2 The CR-delete finalizer choreography

On `deletionTimestamp` set, the `AgentReconciler` runs the graceful sequence (built on
the contract's lifecycle tools, agentd RFC 0015 §4) **before** removing the finalizer
that releases the object to GC:

```
handleFinalizer(agent):
  1. lame-duck the instance(s)  → flip readiness NotReady WITHOUT exiting
     (agentd RFC 0015 §4.2); new reactive triggers stop routing, in-flight bleeds off.
     [via the node-agent management API; tolerate unreachable → fall through to (3)]
  2. wait for in-flight to drain (watch agent://inventory active count / agent_active_subagents),
     bounded by the CR's drain budget.
  3. STOP THE RECREATING CONTROLLER FIRST, THEN drain. The owning workload kind decides order:
       • RECREATING kinds (Deployment / StatefulSet / CronJob): scale the workload to 0
         (or delete the controller object / suspend the CronJob) BEFORE/AS the drain, so
         the controller cannot start a replacement; the resulting pod SIGTERM IS the drain
         trigger → clean exit 0 (agentd RFC 0015 §4.1, agentd RFC 0011 §4.2/§5). A bare
         management "drain → exit 0" on a still-live controller would be UNDONE — the
         controller recreates the lame-ducked pod, un-drained, possibly processing work.
       • NON-RECREATING kinds (once/Job): the management "drain → exit 0" IS the primary
         path — there is no controller to recreate the pod.   drain ≡ SIGTERM ≡ exit 0.
  4. remove the agentctl finalizer → built-in GC reaps the owned child tree (§3.2).
```

If `surfaces.operator_tools` lacks `drain`/`lame-duck` (graceful degradation, §6.1),
or the node-agent is unreachable, the finalizer **falls back to deleting/scaling the
workload and relying on SIGTERM** (pod delete → the agent's own drain), then removes
the finalizer. The finalizer MUST be **bounded** (a drain deadline ≤ the grace budget)
so a wedged drain cannot strand the object forever; on deadline it proceeds to step 4
and records a `DrainTimedOut` event. The **per-kind discipline is the PRIMARY path,
not just a fallback** (brainstorm §4.2): for any recreating controller, "delete/scale
the workload before draining" is required precisely because SIGTERM≡drain≡exit-0 on a
pod whose controller is still live would be reverted by an un-drained replacement.

`drain` is **idempotent and monotonic** (agentd RFC 0015 §8): a second `drain`, or a
SIGTERM arriving after the tool call, re-reports the snapshot — so the finalizer is
safe to re-run on requeue, exactly as a level-triggered loop requires.

### 7.3 The finalizer MUST NOT depend on the admission webhook

Adding/removing a finalizer is an **UPDATE on `agents`**, which a `failurePolicy:
Fail` validating webhook (agentctl RFC 0007) would intercept — so a webhook outage
would strand **every** `Agent` in `Terminating` (brainstorm §3.2). Normative: the
operator's ServiceAccount is **exempted** from the webhook via a `matchCondition`
(agentctl RFC 0007 owns the wiring), and **finalizer removal MUST NEVER be gated on a
webhook call**. The operator patches metadata-only for finalizer add/remove (no spec
change), so even the exemption aside, the finalizer write carries nothing the webhook
needs to validate. This is the rule that keeps deletion always-live.

---

## 8. Reconcile correctness discipline (the red-team bugs, made normative)

Each subsection is a load-bearing bug the brainstorm red-team (§3.2) caught; each is
stated as a normative rule.

### 8.1 The status-hot-loop trap

A controller that writes `.status` and then wakes itself on that write spins forever.
Rules:

- **Patch `.status` only on a `DeepEqual` change.** The operator computes the
  projected status, compares it to the stored `.status`, and issues the status-subresource
  patch **only if they differ**. No-op reconciles write nothing.
- **Stamp and compare `observedGeneration`.** `.status.observedGeneration` records the
  `.metadata.generation` the status reflects; spec-driven work is keyed on
  `generation != observedGeneration`, decoupling "spec changed" from "status changed."
- **Status writes are on the status subresource**, which does not bump
  `.metadata.generation` — so a status write cannot masquerade as a spec change. The
  status write therefore does not, by itself, re-trigger spec-driven rendering.
- **Tolerate-unreachable is not a status flap.** A transient missing node-agent
  snapshot (§4.2) sets `Ready=False`/`ManagementUnreachable` **once**; it must not
  oscillate the condition on every reconcile (use `lastTransitionTime` + reason
  stability).

### 8.2 The live-status edge: do NOT `Owns(Pod)`

A Pod's controller `ownerReference` points to its `ReplicaSet`/`Job`/`StatefulSet`,
**not** to the `Agent` CR (§3.2). So `.owns(Pod)` would never fire for an Agent, and
owner-ref traversal from a pod to its Agent does not exist. Rules:

- The operator **labels managed pods** (via the pod template it renders) and wakes
  reconcile through a **`.watches(Pod, mapper)`** closure that maps a labelled pod back
  to its owning CR (the kube-rs analogue of `EnqueueRequestsFromMapFunc`).
- The **preferred** long-term shape is a node-agent-owned **watchable object** (an
  `AgentInstance` / EndpointSlice the node-agent maintains, §4.2) that the operator
  `.watches()` — giving a clean, typed live-status edge instead of pod-label fan-out.
  The choice is agentctl RFC 0008's; this RFC mandates only that it is **not**
  owner-ref traversal and **not** `.owns(Pod)`.

### 8.3 Scope `GenerationChangedPredicate` to the primary source ONLY

`GenerationChangedPredicate` suppresses events that do not bump `.metadata.generation`.
Applied controller-wide it is a footgun (brainstorm §3.2):

- It would **suppress pod/annotation edges** (annotations and status do not bump
  generation) — killing the §8.2 live-status trigger.
- It would **delay finalizer/deletion handling** (`deletionTimestamp` does not bump
  generation) — stranding deletes.

Normative: apply `GenerationChangedPredicate` to the **`For(&Agent)` primary watch
only**; never to `.owns()`/`.watches()` sources; guard status writes with `DeepEqual`
(§8.1) rather than with a global generation predicate.

### 8.4 Do not fight KEDA over `.spec.replicas`

For an elastic fleet (claim mode, agentctl RFC 0003 §4.3), the KEDA-generated HPA owns
`.spec.replicas`. Rules:

- The operator's SSA patch for the rendered Deployment **OMITS `.spec.replicas`
  entirely** — it never appears in the apply, so the operator's `fieldManager` never
  claims it and the HPA's writes are never reverted. (SSA makes this clean: omitted
  fields are not owned.)
- For **shard** mode the replica count *is* `N` and is operator-owned; KEDA is paused
  (`min==max` pin or the `ScaledObject` removed for the resize window) so there is
  **exactly one writer** of the replica field at any time (agentctl RFC 0003 §4.3).
- The operator renders the `ScaledObject` (agentctl RFC 0011 owns the scaler/trigger
  detail) but never writes the HPA or the replica field directly.

### 8.5 Requeue, backoff, rate-limit; double-processing/double-schedule at render

- **Requeue**: a successful reconcile returns a long, **jittered** level-triggered
  backstop (`Action::requeue(5–10m)`); edges (§3.3) do the real work. A reconcile with
  pending external state (a probe in flight, a rollout progressing) returns a shorter
  requeue.
- **Backoff**: reconcile errors are retried with the runtime's **exponential backoff**
  via `error_policy`; a persistently failing object backs off rather than tight-looping.
- **Rate-limit**: the shared workqueue rate-limits per object so one churning CR cannot
  starve the others.
- **Double-processing / double-schedule are avoided at RENDER time**, not by the
  reconcile cadence: the renderer enforces the RFC 0003 §5.1/§5.2 mode→workload rules —
  a `reactive` singleton renders `strategy: Recreate` (at-most-one on the source); a
  `schedule` Agent renders a `CronJob` with `concurrencyPolicy: Forbid` **and** disables
  the agent's internal cron in the rendered config (one clock). The operator implements
  these; RFC 0003 specifies them.

### 8.6 `podFailurePolicy` is compiled from the exit-code contract — and unmatched codes retry

For `once`/`schedule` (Job-backed) workloads the renderer compiles `podFailurePolicy`
from the exit-code table the build honours (agentd RFC 0011 §5, cache `exitCodesTag`):

| Exit code(s) | Action | Why |
|---|---|---|
| `2` (`EXIT_USAGE`), `5` (`EXIT_SEMANTIC`) | **`FailJob`** | non-retriable: config error / deterministic refusal |
| `1`, `4` (`EXIT_INTELLIGENCE`), `6` (`EXIT_MCP`) | `Count` (toward `backoffLimit`) | transient/upstream — a retry may help |
| `3` (`EXIT_PARTIAL`), `7` (`EXIT_BUDGET`) | `Count` (+ `--budget-exit-code` remap if configured) | policy; usually raise budget |
| `124` (`EXIT_TIMEOUT`) | `Count` | wall-clock deadline |
| `137` (SIGKILL/OOM), `143` (ungraceful SIGTERM) | **explicit `FailJob`/alert + `onPodConditions: DisruptionTarget`** | there is no "alert-only" action; an **unmatched** code COUNTS toward `backoffLimit` and **is retried** — pair 137 with the disruption condition because OOM/eviction exit-code matching is unreliable |

The load-bearing correction (brainstorm §3.2): **an unmatched exit code is not
inert — it counts toward `backoffLimit` and is retried.** So 137/143 need *explicit*
rules or a runaway pod silently retries. The operator emits these rules mechanically
from the exit-code table version in the cache, never hand-transcribed.

---

## 9. The strawman reconcile flow

```
AgentReconciler.reconcile(agent):                          [level-triggered; idempotent]
 ─────────────────────────────────────────────────────────────────────────────────────
  1. Store.get(agent)  ──not found──▶  return Action::await_change()   // GC handled it (§3.2)
  2. agent.deletionTimestamp set?  ──yes──▶  handleFinalizer(agent)    // §7.2 drain choreography
                                              └─ lame-duck → drain(exit 0) → remove finalizer → return
  3. ensure finalizer (metadata-only patch; operator SA is webhook-exempt — §7.3)
 ─────────────────────────── STATIC capability (render time, §4.1/§5) ──────────────────
  4. digest := resolveImageDigest(spec.image | AgentClass.image)       // never a tag (§5.3)
  5. capFacts := CapabilityCache.getOrProbe(digest, featureSet)        // digest-stable facts only
        └─ miss ▶ run CapabilityProbe Job (`--capabilities`, side-effect free) → cache → requeue-soon
  6. contractMajor understood?  ──no──▶ render workload-only;
                                         status.ContractCompatible=False; manage liveness+exitcodes+logs
                                         (agentd RFC 0014 §8); goto 11
 ──────────────────────────────── resolve refs (RFC 0004) ──────────────────────────────
  7. class  := resolve(AgentClass)        // substrate tier, contractVersionRange, default limits
     intel  := resolve(IntelligenceService/ModelPool)  // ordered endpoint list (RFC 0018)
     mcp    := resolve(MCPServerSet refs) ∪ inline      // refs+inline ADD; webhook deduped (RFC 0007)
 ──────────────────────────── render (manifest-driven, §6) ─────────────────────────────
  8. cfgReloadable, cfgRestartOnly := renderConfig(spec, class, intel, mcp, capFacts)  // 2 CMs iff hot_reload
     env      := renderDownwardAPIEnv(spec, class)                     // RFC 0003 §9 (P3 shard)
     workload := renderWorkload(spec.mode, …, capFacts)                // RFC 0003 §5 table
                  ├─ surfaces-gated: mgmt socket? validate-config init? exec vs http probe? a2a inert?
                  ├─ podFailurePolicy from exitCodesTag (§8.6) for Job-backed modes
                  └─ KEDA-managed fleet: OMIT .spec.replicas (§8.4)
     substrate:= wireSubstrate(class.tier)                             // RFC 0002 §6 (hostPath/kata/sidecar)
  9. serverSideApply(workload, cfgReloadable, cfgRestartOnly, [scaledObject],
                     fieldManager="agentctl", ownerRef=agent, label managed=true)   // §3.1/§3.2
 ──────────────────────────── LIVE capability (status time, §4.2) ──────────────────────
 10. snapshot := nodeAgent.observedSnapshot(agent)   // agent://capabilities/inventory/status; tolerate nil
 11. newStatus := projectStatus(capFacts, snapshot, workloadStatus)   // RFC 0003 §6 curated projection
     if !DeepEqual(agent.status, newStatus):  patchStatus(newStatus); set observedGeneration   // §8.1
 12. return Action::requeue(jitter 5–10m)            // long backstop; edges (§3.3) do the work
```

Every rendered child in step 9 carries the `agents.x-k8s.io/managed=true` label (so the
label-scoped Store sees it, §3.3) and an `ownerReference` to the CR (so GC reaps it,
§3.2). The flow is **safe to run at any time, any number of times**: a no-op reconcile
re-derives the same desired state, the SSA is a no-op, and the `DeepEqual` guard writes
nothing. That is the whole point of level-triggering.

---

## 10. Non-goals

- **The CRD schema, status taxonomy, mode→workload table, CEL invariants,
  additionalPrinterColumns.** agentctl RFC 0003 — this RFC *drives* the rendering RFC
  0003 specifies; it does not redefine the shape.
- **The admission webhook** (cert rotation, fail-closed wiring, the operator-SA
  `matchCondition` exemption, the trifecta-union / name-collision / config-schema
  rungs, the init-container ground-truth rung). agentctl RFC 0007. This RFC consumes
  admission-validated CRs and **shares the capability cache** with the webhook.
- **The substrate descriptor, socket discovery, pod→socket attestation, the node-agent
  itself, and the live-snapshot transport.** agentctl RFC 0002 / RFC 0008. This RFC
  consumes the endpoint descriptor and the observed snapshot; it does not produce them.
- **The KEDA external scaler, the autoscaling trigger detail, the reference
  coordination MCP server, the shard-resize controller, standby/warm-pool.** agentctl
  RFC 0011. This RFC only renders the `ScaledObject` and fixes the
  replica-field-ownership rule (§8.4).
- **CRD versioning, the conversion webhook, `StorageVersionMigration`.** agentctl RFC
  0005. This RFC assumes a single served version (RFC 0003 §8).
- **The codegen pipeline and the conformance suite.** agentctl RFC 0001 §4 / RFC 0018.
  This RFC consumes the generated `agent-contract-client` to deserialize manifests.
- **Observability/telemetry projection, run-outcome capture, the metrics scrape-proxy.**
  agentctl RFC 0010 (run-outcome capture for `once` is contract ask P5).
- **Any data-plane internals.** The operator drives the contract; it MUST NOT branch on
  one binary's flags, file layout, or `build_features` *values* (those are opaque,
  agentctl RFC 0003 §6.1) — only on `surfaces{}` and the negotiated `contract_version`.

---

## 11. Rollout & compatibility

- **Phase 0 / pre-MVP** (agentctl RFC 0001 roadmap): the `AgentReconciler` rendering
  `once`/`reactive` on the **stock-unix** substrate, the digest-keyed cache + the
  `CapabilityProbe`, the single-writer `DeepEqual` status discipline, and the
  finalizer choreography. No unbuilt contract primitive is required on a stock
  cluster for the management-by-unix-socket path.
- **Graceful degradation is the default, not an edge case.** An image advertising a
  partial `surfaces{}` is rendered to exactly what it can serve (§6.1); an unknown
  contract major is managed by liveness + exit codes + logs (agentd RFC 0014 §8). The
  operator never refuses to render because a surface is missing — it renders *less*.
- **Capability-affecting contract asks gate specific render branches**, not the loop:
  P6 (`--config-schema`/`--validate-config`) gates the validate-config init-container
  and the webhook's schema rung; P1 (exec-health verb) gates networkless-tier probes
  (agentctl RFC 0002 §8); P3 (`--shard auto/N`) gates the shard env injection (RFC
  0003 §9.1); P2 (`surfaces.a2a`) gates the A2A surface (inert until it lands); P5
  gates robust `once`-mode run-outcome capture. Until each lands, the corresponding
  branch degrades per §6.1.
- **The reconcile contract is forward-compatible with a second-vendor agent.** Because
  rendering keys off the manifest and status off the live snapshot, a conformant
  agent from another vendor — possibly another language — is rendered and managed
  unchanged the moment it passes the conformance suite (agentctl RFC 0001 §4.3). That
  is the operator-level expression of P0.

---

## 12. Open questions

1. **Live-status snapshot transport (the §4.2 / §8.2 shape).** A node-agent-owned
   watchable object (`AgentInstance` / EndpointSlice) the operator `.watches()` —
   clean typed edge, more objects — vs the operator querying the node-agent management
   API on a label-driven pod edge — fewer objects, a runtime dependency on the
   node-agent for the edge. Owned by agentctl RFC 0008; leaning the watchable object.
2. **Capability-cache persistence + GC.** A `ConfigMap`/`Secret`-backed store vs a
   small cluster-scoped CRD vs an in-memory cache re-warmed on leader handover; the TTL
   and the eviction policy (§5.3). Persistence avoids a re-probe storm on restart but
   adds an object to manage.
3. **`CapabilityProbe` execution shape on hostile-tenant clusters.** The probe runs the
   tenant's image (side-effect free, no network) — but on a `tenancy: hostile` cluster
   should it run under the Kata `RuntimeClass` (agentctl RFC 0002 §5) like the workload,
   or is a tightly-sandboxed stock Job acceptable for a no-network, no-LLM,
   exits-immediately probe? Leaning: probe under the same tier the workload will use.
4. **Init-container probe vs probe Job as the default.** The Job is asynchronous and
   off the apply path (§5.1); the init-container couples the probe to the workload's
   own startup. Pick one default; the other is the situational variant.
5. **Drain-budget vs grace coupling in the finalizer (§7.2).** The finalizer's bounded
   drain deadline must be ≤ the pod grace; confirm the precedence when the CR's
   `drain.timeoutSeconds`, the `AgentClass` default, and the contract's
   `drain_timeout_ms` (agentd RFC 0015 §5.2) disagree.
6. **Resolve-on-class-change fan-out (§3.3).** An `AgentClass` default change wakes every
   dependent `Agent`; on a large fleet this is a reconcile storm. Rate-limit/debounce
   policy for the `.watches(AgentClass)` mapper (owned with agentctl RFC 0004).
7. **Deep-merge vs replace for sparse `limits`/`surfaces` overrides** against
   `AgentClass` defaults during `renderConfig` (step 8) — mirrors agentctl RFC 0003
   open question 4 and must be settled identically.

---

## 13. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — Stack & repo decision record: the `kube-rs`
  `Controller`/`watcher`/`reflector`/`Store` runtime, SSA `Patch::Apply` with a
  `fieldManager`, leader election (`kube-leader-election`/`kubert`), the generated
  `agent-contract-client` the operator deserializes manifests through, the
  Contract-as-Schema (P0) anti-drift the two-path model rests on.
- **agentctl RFC 0002** — Substrate & transport abstraction: the endpoint descriptor
  this reconcile consumes via the node-agent, the substrate tiers the renderer wires,
  the `Ready` condition reasons (`ManagementUnreachable`/`AttestationFailed`), the
  exec-health probe ask (P1) the §6.1 probe branch gates on.
- **agentctl RFC 0003** — Agent & AgentFleet CRD schema + status contract: the `.spec`
  the renderer reads, the curated `.status` it writes, the mode→workload table and the
  double-processing/double-schedule rules this RFC *drives*, the two-ConfigMap
  partition, the downward-API env set, the `AGENT_SHARD` defect (P3), the KEDA
  replica-ownership rule.
- **agentctl RFC 0004** — AgentClass / IntelligenceService / MCPServerSet: the
  ops-owned objects step 7 resolves (substrate tier, contract-version range, default
  limits/intel binding, MCP-server bundles).
- **agentctl RFC 0005** — CRD versioning & conversion: the single-served-version
  posture this RFC assumes.
- **agentctl RFC 0007** — Admission validation ladder: the webhook that validates the
  CRs this RFC consumes, shares the capability cache, and whose fail-closed wiring the
  §7.3 finalizer exemption depends on.
- **agentctl RFC 0008** — node-agent architecture: discovery, attestation, the live
  management connection, and the observed-snapshot transport the LIVE path consumes.
- **agentctl RFC 0009** — management access path & RBAC: the operator→node-agent
  access path (§4.2) by which the operator queries the live snapshot when it is not
  carried on a watchable object.
- **agentctl RFC 0010** — Observability & telemetry bridge: the metrics scrape and
  run-outcome capture the rendered surfaces feed (run-outcome capture is ask P5).
- **agentctl RFC 0011** — Scaling plane: the KEDA external scaler, the `ScaledObject`
  trigger detail, the coordination server behind the fleet rendering.
- **agentctl RFC 0013** — A2A gateway & task store: the A2A surface §6.1 holds inert
  until contract ask P2.

**Contract spec (the reference implementation, agentd RFCs)**

- **agentd RFC 0014** — control-plane contract umbrella: the capabilities-manifest
  spine (§5), `surfaces{}` as the single discovery point (§6.2), contract versioning
  /negotiation (§6.3), the downward-API env convention (§6.4), graceful degradation
  (§7/§8) — the spine the whole manifest-driven model keys off.
- **agentd RFC 0015** — management & control surface: the manifest schema and its
  static-vs-live emission (§5.2), `agent://inventory`/`status` (§5.3/§5.4), the
  operator tools `drain`/`lame-duck`/`cancel` (§4) the finalizer uses, reconnect = a
  clean re-read (§8) that makes the LIVE path idempotent.
- **agentd RFC 0016** — telemetry & lifecycle contract: the frozen metrics schema +
  exit-code table the `podFailurePolicy` compiles from (§8.6) and the scrape surface
  status projects.
- **agentd RFC 0017** — declarative config & hot reload: the config-file schema the
  two-ConfigMap partition (§6.4) lays out, `--validate-config`/`--config-schema`
  (contract ask P6), the reloadable-vs-restart allowlist.
- **agentd RFC 0011** — cloud-native contract: the exit-code table (§5) and the drain
  state machine / clean exit `0` (§4.2) the §7 choreography invokes; the
  validate-at-startup discipline the `CapabilityProbe` relies on for side-effect-free
  `--capabilities`.
- **agentd RFC 0019** — horizontal scaling: claim/shard ownership, the claim-release
  drain (§6) the pod-SIGTERM path runs, the `AGENT_SHARD` defect (§4.2, contract ask
  P3).
- **agentd RFC 0008** — execution modes & reactive routing: the `mode` vocabulary and
  the internal-cron-vs-CronJob mutual exclusion the §8.5 render enforces.
- **agentd RFC 0012** — security posture: the trifecta tags the renderer carries and
  the `Secret`-has-no-`Serialize` invariant that keeps the probed manifest secret-free.

**Contract asks raised or cited by this RFC** (agentctl brainstorm §14): **P1**
(exec-health verb — networkless probe rendering, §6.1), **P2** (`surfaces.a2a` — inert
A2A branch), **P3** (`--shard auto/N` — the `AGENT_SHARD` defect, §6.3), **P5**
(run-outcome capture for `once`), **P6** (`--config-schema`/`--validate-config` — the
cache's `configSchema` + the validate-config init-container), **P10** (autoscaling
metric-name reconciliation, cited via the fleet render).
