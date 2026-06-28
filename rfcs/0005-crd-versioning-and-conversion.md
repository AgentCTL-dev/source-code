# agentctl RFC 0005: CRD versioning & conversion policy

**Status:** Proposed (agentctl foundational track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the safe-evolution policy for the `Agent`/`AgentFleet` CRD group: the conversion/migration machinery and graduation ladder that agentctl RFC 0003 §8 fixed the *posture* of and **deferred the mechanics to here**

> **This RFC governs agentctl's OWN API surface.** The CRD `apiVersion`
> (`v1alpha1 → v1beta1 → v1`) is agentctl's declarative contract with its *users*
> — the shape of the object they `kubectl apply`. It is **independent** of the
> agent contract (the runtime agentctl↔agent wire that P0 makes load-bearing).
> The agent `contract_version` (agent RFC 0014 §6.3, the reference impl's contract
> spec) is negotiated at runtime and lives in `.status` and the `AgentClass`; it
> moves on its **own** clock. Conflating the two — bumping the CRD every time the
> data plane ships a feature — is the anti-pattern this RFC exists partly to
> forbid (§3). Nothing here couples agentctl to a specific agent.

> **The correctness point in one sentence.** `conversion: None`
> (the apiextensions default) does **not** convert between CRD versions — it serves
> the *stored bytes* relabelled, and structural-schema **pruning silently drops
> fields absent from the version through which an object is read or written**, so a
> naive "additive" multi-version ladder loses data on a round-trip through etcd
> (agentctl brainstorm §2.2). The whole decision below is built to make that class
> of silent corruption **impossible by construction**.

---

## 1. Problem / Context

The `Agent` and `AgentFleet` CRDs (agentctl RFC 0003) are agentctl's load-bearing
public API. They **will** evolve: fields will be added (the common case), and —
rarely — renamed, restructured, or removed as the API graduates from alpha toward
GA. Kubernetes gives CRDs a first-class multi-version mechanism for exactly this.
Its **default configuration is a silent data-loss footgun**, and the obvious-looking
"just keep the old version served and add a new one" ladder walks straight into it.
agentctl RFC 0003 §8 committed to the *safe posture* (one served version + a
conversion webhook + `StorageVersionMigration`, decoupled from `contract_version`)
and explicitly **deferred the policy and mechanics to this RFC**. agentctl RFC 0001
§3 recorded the standing cost that shapes this RFC: **`kube-rs` ships no conversion
scaffolding** — every conversion handler is hand-written (there is no
`kubebuilder`/`controller-gen` to scaffold it). This RFC owns the policy, the
choreography, and the discipline that keeps that hand-written surface small.

### 1.1 The Kubernetes multi-version mechanism (the parts that matter)

A `CustomResourceDefinition` declares a list of `versions[]`. Each entry has:

- `served: true|false` — whether the apiserver accepts requests at that
  `apiVersion`;
- `storage: true|false` — **exactly one** version is the *storage version*; the
  apiserver persists every object to etcd encoded at that version;
- a per-version `schema` (an OpenAPI v3 structural schema; **mandatory** for
  `apiextensions.k8s.io/v1`).

And the CRD has one `spec.conversion` block with a `strategy`, whose **default is
`None`**. Two facts about the platform combine into the trap:

1. **Pruning is always on.** Because the schema MUST be structural,
   the apiserver **prunes** any field not specified in the relevant version's
   schema. There is no global opt-out; only an explicit
   `x-kubernetes-preserve-unknown-fields` subtree escapes pruning, and you would
   never blanket-enable that on a typed API.
2. **`conversion: None` does not convert.** With `strategy: None`, the apiserver
   serves a request at version *V* by taking the **stored object's bytes**,
   **only rewriting the `apiVersion`/`kind` strings**, and pruning against *V*'s
   schema. No field-level transformation happens. (`strategy: Webhook` is the only
   alternative — it calls an external HTTPS endpoint to transform objects between
   versions.)

### 1.2 The trap, stated precisely

Take the seductive "additive ladder": ship `v1alpha1`, later add `v1beta1` that
adds an optional field `foo`, leave **both `served: true`** under the default
`conversion: None`, and make `v1beta1` the storage version. The reasoning "it is
only additive, so it is safe" is **false across versions**. Walk one round-trip:

```
client/controller still pinned to v1alpha1 (older client, GitOps manifest, kubectl)
        │  GET .../v1alpha1/agents/triage
        ▼
apiserver: read stored bytes (encoded v1beta1, INCLUDES foo)
           relabel apiVersion → v1alpha1
           PRUNE against v1alpha1 schema  ──►  foo is DROPPED from the response
        │  client now holds an object with NO foo (it never knew foo existed)
        ▼
client: PUT .../v1alpha1/agents/triage   (re-apply, reconcile write-back, GitOps sync)
        ▼
apiserver: validate+prune against v1alpha1 schema (no foo)
           store at storage version v1beta1  ──►  foo is now GONE from etcd
        ▼
RESULT: foo silently deleted. No error. No event. No warning.
```

The corruption needs no malice and no bug in agentctl: a single client or
controller that reads/writes at the *older* served version is enough, and in a
real cluster there are always such clients (an out-of-date GitOps render, a
`kubectl` from last quarter, a sibling controller pinned to `v1alpha1`). The loss
is **silent** because pruning is the intended, normal behaviour for unknown
fields — it cannot distinguish "field the user never set" from "field this version
cannot represent." `conversion: None` is only safe when **every served version has
an identical schema** (in which case multiple versions buy nothing).

### 1.3 Why the naive ladder is the wrong instinct

The instinct is imported from libraries and wire protocols, where *additive* means
*backward-compatible*. It is correct **within a single CRD version's schema** —
adding an optional field to `v1alpha1`'s schema in a later agentctl release is
safe, because there is only one schema doing the pruning. It is **wrong across two
served versions under `conversion: None`**, because the *older* version's schema
does the pruning on its own request path and that schema, by construction, cannot
hold the new field. The brainstorm (§2.2) flagged this exact "do NOT promise a
`v1alpha1 → v1beta1 → v1` additive ladder under `conversion: None`" as a
Kubernetes **correctness error**, not a style preference. This RFC turns that
finding into policy.

### 1.4 What this RFC owns vs. defers

This RFC owns: the served/storage-version rule, the conversion-webhook + SVM
choreography, the alpha→beta→GA graduation ladder and deprecation windows, the
decoupling from `contract_version`, and the conversion-minimization discipline. It
**defers**: the webhook *server*, TLS cert provisioning/rotation, fail-closed
wiring, and HA sizing to **agentctl RFC 0007** (the conversion handler shares that
server — §4.2); the CRD *shape* and `.status` projection to **agentctl RFC 0003**;
the reconcile/render loop and single-writer `.status` discipline to **agentctl RFC
0006**; the contract codegen/conformance pipeline (the Axis-B tooling, §3) to
**agentctl RFC 0018**.

---

## 2. Decision — one served version + a conversion webhook + StorageVersionMigration

### 2.1 The rule, stated plainly

1. **Exactly one served *and* storage version at steady state.** At rest, the CRD
   has a single `versions[]` entry with `served: true, storage: true`. There is
   never more than one served version *except transiently*, during a migration
   window, and **only** under `conversion: Webhook` (§2.4).
2. **Absorb churn additively, in place, while the version is pre-GA.** New optional
   fields are added to the *current* version's schema in an agentctl release — **no
   new `apiVersion`, no conversion, no migration**. This is where the overwhelming
   majority of evolution happens (§4).
3. **A version *bump* is a three-part lockstep operation:** a hand-written
   **conversion webhook** that round-trips losslessly, a transition window where
   the old and new versions are both served *through the webhook*, and a
   **`StorageVersionMigration`** that rewrites every stored object to the new
   storage version before the old version is retired (§2.4, §5).
4. **The CRD `apiVersion` ladder is decoupled from the agent `contract_version`**
   (§3). They are orthogonal clocks; neither bump forces the other.

`conversion: None` is **permitted only** while the CRD has a single version (the
steady state), where there is nothing to convert. The moment a second version is
served, `strategy: Webhook` is mandatory.

### 2.2 Why one served version (and not "a few, carefully")

Because §1.2 has no safe middle ground under `None`, and because every additional
served version under `Webhook` is a per-version arm in a **hand-written** converter
(RFC 0001 §3) plus a hot-path apiserver round-trip on every cross-version read.
"One served version" is the configuration that makes the data-loss class
*impossible* (there is only one schema, so pruning can drop nothing the API
promises) and keeps the converter at its minimum size. The transition window in
§2.4 is the bounded, deliberate exception — not a standing posture.

### 2.3 Additive-within-version while pre-GA (the cheap path)

While a version is `alpha` or `beta`, agentctl evolves it by **editing that
version's schema in place** across agentctl releases, under one discipline
(elaborated in §4): changes are **strictly additive** — new **optional** fields
with safe defaults, new enum values, relaxed (never tightened) validation. A
field is **never** renamed, retyped, or repurposed in place, because that is a
silent breaking change *even with one version* (old stored values now fail
validation or mean something new). Renames/retypes/removals are deferred and
batched into a **version bump** (§2.4, §5). This is the lever that makes
conversions rare: an additive change costs a schema replacement and zero
migration.

### 2.4 A version bump: webhook + window + SVM (and the `storedVersions` gate)

When a non-additive change is unavoidable (a rename, a restructure, or a
graduation milestone), the bump is choreographed. The mechanics rest on three
apiserver facts:

- The apiserver **always writes** new/updated objects at the **current storage
  version**; objects already in etcd stay encoded at whatever version was storage
  when they were last written, until something rewrites them.
- `.status.storedVersions` on the CRD lists every version that **may still have
  objects encoded at it** in etcd. The apiserver **refuses to remove** a version
  from `spec.versions` while it is listed in `.status.storedVersions`.
- A **`StorageVersionMigration`** (the in-tree `storagemigration.k8s.io`
  storage-version-migrator) lists every object and issues a no-op rewrite, forcing
  re-encode at the current storage version (invoking the conversion webhook for any
  object still stored at an older version). When it completes, agentctl prunes
  `.status.storedVersions` down to just the new version.

So a bump is: **deploy the webhook → atomically add the new served+storage version
under `conversion: Webhook` → run the SVM to drain the old storage encoding →
deprecate, then unserve, then remove the old version.** §5 gives the exact ordered
choreography and the ordering traps. The SVM is **mandatory on every storage-version
change**, even one whose conversion is the identity, because without it the old
encoding lingers in etcd and blocks ever dropping the old version from
`storedVersions`.

> **SVM availability.** The in-tree `StorageVersionMigration` API
> (`storagemigration.k8s.io/v1alpha1`) is recent and may be disabled on a target
> cluster. agentctl therefore ships its migration step in two interchangeable
> forms: (a) emit a `StorageVersionMigration` where the API is present; (b) fall
> back to an **operator-driven migrator Job** that lists all `Agent`/`AgentFleet`
> objects and issues an idempotent no-op server-side-apply to force re-encode (the
> same technique the out-of-tree `kube-storage-version-migrator` uses). Both are
> driven from `xtask`/the operator (agentctl RFC 0006); the policy is identical.

### 2.5 The graduation ladder & deprecation windows

agentctl follows the **Kubernetes API deprecation policy**, mapped onto agentctl's
own release cadence (the CRDs are shipped by agentctl, so "release" means an
agentctl release, not a cluster release):

| Stage | `apiVersion` | Stability promise to users | Min. support after deprecation announced | In-place schema change rule |
|---|---|---|---|---|
| **Alpha** | `v1alpha1` | none — may change or be removed between agentctl minors | 0 releases (may unserve immediately) | additive in place freely; a breaking change *still* uses webhook+SVM **once real data exists** (alpha's "no promise" is API instability to users, **not** a licence to corrupt etcd) |
| **Beta** | `v1beta1` | served by default; supported | **3 agentctl releases or 9 months**, whichever is longer | additive in place only; any breaking change ⇒ new version + webhook + SVM |
| **GA** | `v1` | stable, long-term | **3 agentctl releases or 12 months**, whichever is longer | additive in place only; any breaking change ⇒ new **major** (`v2`) + webhook + SVM |

The ladder agentctl ships on:

```
v1alpha1  (single served + stored; conversion: None)
   │   additive-in-place churn across agentctl releases — NO bump, NO webhook
   │   (most evolution lives here; alpha buys the room to get the shape right)
   ▼
v1beta1   ── conversion: Webhook (lossless round-trip) ──► StorageVersionMigration
   │   beta deprecation window keeps v1alpha1 SERVED-through-the-webhook
   │   until clients/GitOps catch up; then unserve, then remove from versions[]
   ▼
v1 (GA)   ── same machinery ── 12-month / 3-release support floor once deprecated
   │
   ▼
v2        ── only for a breaking GA change; same machinery again
```

RFC 0003 ships `Agent`/`AgentFleet` at **`v1alpha1`, single served + stored**
(RFC 0003 §2 point 3 / §12). The first real bump is the first exercise of this RFC's
machinery — and, by §2.3, should be *late*: stay alpha until the shape has earned
graduation, so the first conversion handler is written once, against a settled
design.

### 2.6 The hand-written conversion webhook (the RFC 0001 §3 cost, named)

`kube-rs` provides **no** conversion-webhook scaffolding — there is no
`controller-gen`/`kubebuilder` equivalent that generates conversion functions or
the serving glue (agentctl RFC 0001 §3, the "Conversion webhooks" row: *"Hand-write
the conversion HTTP handler — minimized by policy"*). Concretely, agentctl owns:

- an HTTPS handler that decodes an `apiextensions.k8s.io/v1` **`ConversionReview`**
  request, converts each object in `request.objects[]` to
  `request.desiredAPIVersion`, and returns a `ConversionReview` response with the
  converted objects and a `result: Status` (it shares the operator's `axum`/`hyper`
  TLS server with the admission webhook — §4.2, RFC 0007);
- the per-version-pair **conversion functions themselves**, hand-written (§5),
  operating on `serde_json::Value` rather than typed structs (§5.2 explains why
  that is a correctness requirement, not a shortcut);
- a **round-trip property test** per bump (`from → to → from` is the identity on
  every fixture; §5) that is the executable proof a conversion is lossless.

This cost is **bounded and rare by construction** (§2.3, §4): the converter only
grows an arm at a version bump, and the policy is designed to make bumps
infrequent. It is the deliberate price of the Rust decision (RFC 0001 §2.5,
§2.3) — recorded, not hidden.

### 2.7 Rollout & compatibility (the first bump is greenfield)

The starting state is inherited from agentctl RFC 0003 §2 point 3 / §12:
`Agent`/`AgentFleet` ship at **`v1alpha1`, single served + stored,
`conversion: None`** — the steady state of §2.1, with the converter dormant.
Because **no stored data predates v1alpha1**, the first version bump is a
**greenfield** exercise of §5's choreography (there is nothing to convert *from*
except v1alpha1 objects the SVM rewrites once). The members of the group that ship
later (`AgentClass`/`IntelligenceService`/`MCPServerSet`, agentctl RFC 0004)
**inherit this policy verbatim** (§6); the cluster-scoped `AgentClass` shares the
identical SVM choreography, with only a wider blast radius per bad conversion
(Open Question 6).

---

## 3. Decoupling the CRD version from `contract_version`

This is the conceptual core, and the reason RFC 0003 §2 (point 6) and §8 insisted
the two be kept apart. There are **two independent versioning axes**, and treating
them as one would be expensive and wrong.

| | **Axis A — CRD `apiVersion`** | **Axis B — agent `contract_version`** |
|---|---|---|
| **What it versions** | the *shape of the declarative object* a user applies (`Agent`/`AgentFleet` `.spec`/`.status`) | the *runtime agentctl↔agent wire*: capabilities manifest, `surfaces{}`, metrics schema, exit-code table, config schema, A2A methods |
| **Spelling** | `agents.x-k8s.io/v1alpha1 → v1beta1 → v1` | `1.0`, `1.1`, `2.0` (major.minor) |
| **Owned by** | **agentctl** (this RFC) | **the contract** (currently agent RFC 0014 §6.3, the reference impl's spec) |
| **Negotiated where** | resolved by the apiserver at apply/read time | negotiated at runtime per instance |
| **Lives in** | the stored object's `apiVersion` | `.status.contract.version` (RFC 0003 §6.1) + `AgentClass.contractVersionRange` (RFC 0004) |
| **Moves when** | agentctl makes a breaking schema change or graduates a stage | the contract adds (minor) or breaks (major) a wire surface |
| **Bump machinery** | conversion webhook + SVM (this RFC) | client regen + conformance (agentctl RFC 0018); runtime negotiation + graceful degradation (RFC 0014 §6.3/§8) |

> **The apiGroup string is provisional.** This RFC writes `agents.x-k8s.io`
> throughout (the axis table, the §5 YAML, the §5.2 handler match-arms) using RFC
> 0003's **proposed** value, but the group string is RFC 0003 §13 Open Question 1
> (`agents.x-k8s.io` vs `agentctl.dev` vs `agentctl.io`) and is **not final**. The
> conversion/SVM mechanics here are **group-string-independent** — a group change
> swaps the string in every conversion arm and SVM `resource.group` but changes no
> policy. The `AgentClass.contractVersionRange` field name (§3, the Axis-B pin) is
> confirmed by agentctl RFC 0004 §3.1.

The two clocks are **orthogonal**. A new `contract_version` **minor** (say the
contract adds a metric, or a `surfaces.a2a` key — RFC 0014 §6.3's additive rule)
is **not** a CRD version bump: it changes what agentctl's *generated client* reads
and what `.status` *reports*, neither of which is the CRD's persisted schema. A
CRD field addition is **not** a contract change: the agent never sees the CRD.

**Why conflating them is the anti-pattern.** If the CRD `apiVersion` tracked
`contract_version`, then every data-plane feature release — a new metric, a new
optional manifest key — would force a CRD version bump, which forces a conversion
webhook arm **and** a `StorageVersionMigration` that rewrites every stored object
in etcd. That is an etcd-churn / operational-toil engine driven by the *data
plane's* release cadence, for changes that do not touch the persisted object at
all. Decoupling means the contract can ship features at runtime (negotiated,
degraded-gracefully) while the CRD sits still at one served version.

### 3.1 The legitimate interaction (a field may *surface* a capability)

The axes are decoupled, not disconnected. A CRD field often **projects** a contract
concept: `spec.surfaces.a2a` (RFC 0003 §3.3) surfaces the contract's A2A surface;
`AgentClass.contractVersionRange` (RFC 0004) is the *pin* that tells the operator
which contract **majors** it will manage; `scaling.target.signal` (RFC 0003 §4.3)
names a contract metric. When the contract grows a capability agentctl wants to
expose, agentctl **may add an optional CRD field** to surface it — but:

1. that is an **additive-in-place change** (§2.3) to the current version's schema,
   **not** an `apiVersion` bump — so it triggers no conversion and no migration;
2. the **field's *values*** still version on Axis B: `contractVersionRange` holds a
   contract spelling, and the runtime answer to "does *this instance* actually
   support a2a?" stays in `.status.surfaces`, keyed off the agent's live
   `surfaces{}` (RFC 0014 §6.2), **never** baked into the CRD `apiVersion`. RFC
   0003 already encodes this: `spec.surfaces.a2a` is *accepted but inert* until the
   contract ask **P2** lands — the CRD shape did not have to wait for the contract,
   and the contract landing will not bump the CRD.

The one-line rule: **a CRD field may *expose* a contract capability, but how and
when the CRD itself versions is agentctl's own concern, on agentctl's own clock.**

### 3.2 Interaction matrix (what triggers what)

| Event | Axis A (CRD `apiVersion`) | Axis B (`contract_version`) | Conversion webhook + SVM? |
|---|---|---|---|
| Contract adds a metric / manifest key (minor) | unchanged | minor bump | **No** — client regen (RFC 0018) + `.status` only |
| Contract renames/removes a wire surface (major) | unchanged* | major bump | **No** for the CRD; operator refuses majors it doesn't know (`ContractCompatible: False`, RFC 0003 §6.2) |
| agentctl adds an optional CRD field (to surface a capability or otherwise) | unchanged (additive in place, §2.3) | unchanged | **No** |
| agentctl renames/retypes/removes a CRD field | **bump** | unchanged | **Yes** (§5) |
| agentctl graduates alpha→beta or beta→GA | **bump** | unchanged | **Yes** |

\* A contract major *may* prompt agentctl to add or deprecate optional CRD fields
(additive, no bump) to expose/retire a surface, but the contract major never *forces*
a CRD `apiVersion` change.

---

## 4. Conversion-minimization by construction

The cheapest conversion webhook is the one that almost never has to convert. The
policy is engineered so that the hand-written surface (§2.6) stays minimal.

### 4.1 The disciplines

1. **Stay alpha; absorb additively.** The dominant evolution mode — new optional
   fields — is in-place schema editing on a single version (§2.3). It costs a
   schema replacement and **zero** conversion/migration. Graduate only when the
   shape is settled, so the first conversion is written once.
2. **Additive-only within a version, enforced in CI.** New fields are optional with
   safe defaults; enum values may be added; validation may be loosened, never
   tightened; a field is **never** renamed/retyped/repurposed in place. A schema
   diff check in `xtask` (the CRD-YAML emitter, RFC 0001 §5) fails the build on a
   non-additive in-place change, forcing it onto the version-bump path instead of
   letting it land as silent corruption.
3. **Projection-shaped spec resists contract churn.** Because the `.spec` projects
   contract *concepts*, not a binary's flags (RFC 0003 §4 / §3.1), the contract's
   own additive evolution (Axis B) rarely needs any CRD shape change at all — the
   projection is stable across contract minors. This is a second-order win of the
   RFC 0003 design that directly shrinks Axis-A churn.
4. **`contract_version` is not in the CRD shape.** Runtime negotiation lives in
   `.status` and `AgentClass` (§3), so the data plane's release cadence never
   reaches the persisted schema — the single biggest source of would-be churn is
   structurally absent.
5. **Batch breaking changes into one graduation event.** When a bump is
   unavoidable, land *all* pending renames/restructures in that one version bump
   rather than dribbling them across several — one conversion arm, one SVM, one
   round-trip test, instead of N.
6. **Prefer lossless, mechanical conversions.** Design renames/restructures so the
   conversion is a pure, reversible field move (§5.2); a lossy conversion needs the
   round-trip-annotation escape hatch (§5.3) and is a smell that the change should
   be reconsidered.

### 4.2 Webhook HA & cert provisioning defer to RFC 0007 (with a sharper HA bar)

The conversion handler is **not** a separate server: it is a second route
(`/convert`) on the **same** `axum`/`hyper` TLS server in the operator that hosts
the admission webhook (`/validate`, `/mutate`), sharing one serving certificate,
one `caBundle`, and one HA model. All of that — cert-manager provisioning with the
self-signed in-repo fallback for cert-manager-absent clusters (RFC 0001 §3), the
`caBundle` injection into both the `ValidatingWebhookConfiguration` and the CRD's
`spec.conversion.webhookClientConfig`, multi-replica/leader behaviour, and
fail-closed wiring — is **owned by agentctl RFC 0007**. This RFC only states the
two requirements that are *sharper* for conversion than for admission, so RFC 0007
sizes for them:

- **Availability is on the read/write hot path, and conversion failure is
  *unconditionally* fatal.** A down or unhealthy conversion webhook makes the
  apiserver **fail every read and write** of `Agent`/`AgentFleet` at any version not
  equal to the storage version — including the operator's own reconcile reads and
  `kubectl agents get`. This is strictly more severe than a down admission webhook
  (which only blocks *writes*). Critically, **conversion webhooks have no
  `failurePolicy` and no `timeoutSeconds` knob** — unlike `Validating`/`Mutating`
  webhook configs, the CRD's `spec.conversion.webhook` exposes only `clientConfig` +
  `conversionReviewVersions` (the timeout is a fixed apiserver constant). A
  conversion failure therefore **always** fails the affected request; it is
  fail-closed *by construction*, with no fail-open lever to reach for. So the only
  real mitigations are (a) the steady-state single-version / `conversion: None`
  posture (§2.1), which keeps the webhook **off the hot path entirely** except during
  a migration window, and (b) **HA** — ≥2 operator replicas serving the webhook
  (even though reconcile is single-leader), behind a `PodDisruptionBudget`. An
  implementer (sizing this in agentctl RFC 0007) must **not** go looking for a
  fail-open/`failurePolicy` setting for conversion; it does not exist.
- **The handler MUST be stateless, deterministic, and total.** The apiserver may
  invoke it on any path (get/list/watch/write) for any source→desired version pair
  in `versions[]`, in batches of mixed source versions; it must convert all
  declared pairs, be a pure function of its input, and never depend on cluster
  state.

---

## 5. Worked example — a `v1alpha1 → v1beta1` field rename

Concrete bump: rename `spec.subscribe` (the reactive work-source list, RFC 0003
§3.1) to `spec.sources`, keeping the type identical (`[]string`). The rename is
deliberately type-preserving so the conversion is **provably lossless in both
directions** — the canonical clean case. (§5.3 covers the harder retype case.)

### 5.1 The CRD across the window

```yaml
# During the migration window only — TWO served versions, conversion: Webhook.
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: { name: agents.agents.x-k8s.io }
spec:
  group: agents.x-k8s.io
  names: { kind: Agent, plural: agents, singular: agent }
  scope: Namespaced
  conversion:
    strategy: Webhook                       # MANDATORY the instant >1 version is served
    webhook:
      conversionReviewVersions: ["v1"]
      clientConfig:
        service: { name: agentctl-operator-webhook, namespace: agentctl-system, path: /convert }
        # caBundle injected by cert-manager ca-injector / the RFC 0007 fallback controller
  versions:
    - name: v1alpha1
      served: true                          # still served THROUGH the webhook for the deprecation window
      storage: false                        # NO LONGER the storage version
      schema: { openAPIV3Schema: { /* …spec.subscribe… */ } }
    - name: v1beta1
      served: true
      storage: true                         # new storage version
      schema: { openAPIV3Schema: { /* …spec.sources… */ } }
# .status.storedVersions transitions ["v1alpha1"] → ["v1alpha1","v1beta1"] → ["v1beta1"] (post-SVM)
```

### 5.2 The conversion handler (hand-written; `kube-rs` has no scaffolding)

```rust
// crates/operator: the /convert route on the shared webhook TLS server (RFC 0007).
// CRITICAL: convert on serde_json::Value, NOT on a typed Agent struct. Reserializing
// through a strict typed struct would itself prune any field the struct doesn't model
// (status, metadata.managedFields, unknown-but-preserved subtrees, future fields) —
// reintroducing the EXACT silent data loss §1.2 forbids. The handler touches ONLY the
// renamed path and passes everything else through byte-for-byte.
async fn convert(Json(review): Json<ConversionReview>) -> Json<ConversionReview> {
    let req = review.request.expect("conversion request");
    let desired = req.desired_api_version;            // e.g. "agents.x-k8s.io/v1beta1"
    let converted: Vec<Value> = req.objects.into_iter().map(|mut obj| {
        let from = obj["apiVersion"].as_str().unwrap_or_default().to_string();
        match (from.as_str(), desired.as_str()) {
            ("agents.x-k8s.io/v1alpha1", "agents.x-k8s.io/v1beta1") =>
                rename_spec_field(&mut obj, "subscribe", "sources"),
            ("agents.x-k8s.io/v1beta1", "agents.x-k8s.io/v1alpha1") =>
                rename_spec_field(&mut obj, "sources", "subscribe"),
            _ => { /* same version, or an unknown pair: pass through untouched */ }
        }
        obj["apiVersion"] = json!(desired);           // only the version label changes otherwise
        obj
    }).collect();
    Json(ConversionReview::success(req.uid, converted))   // result: Status{status:"Success"}
}

// Pure, reversible field move. No other field is read or written.
fn rename_spec_field(obj: &mut Value, from: &str, to: &str) {
    if let Some(spec) = obj.get_mut("spec").and_then(Value::as_object_mut) {
        if let Some(v) = spec.remove(from) { spec.insert(to.into(), v); }
    }
}
```

```rust
// The executable proof the bump is lossless: from → to → from is the identity
// on every golden fixture (the test/fixtures corpus, RFC 0001 §5). Run in CI per bump.
#[test]
fn rename_round_trips() {
    for obj in golden_v1alpha1_agents() {
        let up   = convert_one(obj.clone(), V1ALPHA1, V1BETA1);
        let down = convert_one(up,          V1BETA1, V1ALPHA1);
        assert_eq!(down, obj, "v1alpha1 → v1beta1 → v1alpha1 must be the identity");
    }
}
```

### 5.3 Round-trip losslessness is a hard requirement (and the retype caveat)

The conversion **MUST** round-trip: `from → to → from` returns the original. For a
type-preserving rename this is trivial (§5.2). The dangerous case is a **lossy**
restructure — e.g. `subscribe: []string` → `sources: []{uri, priority}`, where
`v1beta1` adds a `priority` the older version cannot represent. Converting
`v1beta1 → v1alpha1` then drops `priority`, and a client round-tripping through
`v1alpha1` loses it — the §1.2 trap returns *through the webhook*. The Kubernetes
remedy is the **conversion-annotation escape hatch**: stash the
otherwise-unrepresentable data in a reserved annotation
(`agents.x-k8s.io/v1beta1-priority`) on the down-convert, and restore it on the
up-convert, so the field survives a round-trip through the lossy version. This is
load-bearing but ugly; §4.1 rule 6 says to **avoid designing lossy conversions in
the first place**, preferring type-preserving moves so the annotation hatch is
never needed.

### 5.4 The `StorageVersionMigration` step

After `v1beta1` becomes the storage version (§5.1), existing objects are still
encoded `v1alpha1` in etcd. The SVM forces every one to be rewritten at `v1beta1`
(invoking the webhook for each):

```yaml
apiVersion: storagemigration.k8s.io/v1alpha1
kind: StorageVersionMigration
metadata: { name: agents-to-v1beta1 }
spec:
  resource: { group: agents.x-k8s.io, resource: agents, version: v1beta1 }
---
# Repeat for agentfleets. On clusters without storagemigration.k8s.io, the operator
# runs the fallback migrator Job instead (§2.4): list all objects, issue an idempotent
# no-op server-side-apply, forcing re-encode at the storage version.
```

When it reports complete, the operator patches the CRD's `.status.storedVersions`
down to `["v1beta1"]`, which is the gate that later permits removing `v1alpha1`
from `spec.versions`.

### 5.5 The rollout order (and the ordering traps)

The choreography is **strictly ordered**; each step's trap is named.

```
1. DEPLOY the webhook code first.   Operator rolls out with the /convert handler
                                    (both arms) live, but the CRD is STILL single-version
                                    v1alpha1 / conversion: None. Webhook is dormant.
   └─ trap: if you add the 2nd served version before the webhook is healthy, ALL CRD
            reads/writes at the non-storage version break instantly (§4.2).

2. APPLY the CRD update ATOMICALLY. In ONE apply: add v1beta1 (served+storage),
                                    flip v1alpha1 to storage:false (stays served),
                                    set conversion.strategy: Webhook + caBundle.
   └─ trap: adding a 2nd served version and switching to Webhook MUST be the same
            apply. A second served version under the lingering default conversion:None
            is the §1.2 data-loss window — never let that state exist, even briefly.

3. (steady, transient)             New writes store at v1beta1; reads at v1alpha1 go
                                    through the webhook; old objects still encoded v1alpha1.

4. RUN the SVM (§5.4).             Drains the v1alpha1 encoding from etcd. Wait for
                                    completion; confirm .status.storedVersions == [v1beta1].
   └─ trap: do NOT proceed to remove v1alpha1 before this; the apiserver refuses to
            drop a version still in storedVersions, and you'd orphan stored objects.

5. DEPRECATE v1alpha1.             Announce. Keep v1alpha1 served (through the webhook)
                                    for the deprecation window (§2.5: beta = 3 releases /
                                    9 months) so stale GitOps/kubectl/controllers keep working.

6. UNSERVE v1alpha1.               After the window: v1alpha1 served:false (still listed,
                                    not in storedVersions). Reads/writes at v1alpha1 now 404.

7. REMOVE v1alpha1.                Drop the v1alpha1 entry from spec.versions entirely
                                    (legal now: not served, not in storedVersions). The
                                    converter's v1alpha1 arms may be retired or left inert.
```

The invariants that make this safe: **(i)** the webhook is healthy *before* a
second version is served (step 1 → 2); **(ii)** there is **never** a moment with
two differing served schemas under `conversion: None` (step 2 is atomic); **(iii)**
the old version is drained by SVM *before* it is removed (step 4 → 7); **(iv)** the
deprecation window (step 5) gives clients time to migrate while the webhook keeps
the old version correct.

---

## 6. Non-goals

- **The CRD *shape* (`.spec`/`.status`/CEL/printer columns).** Owned by agentctl
  RFC 0003. This RFC versions and migrates that shape; it does not define it.
- **The webhook *server*, TLS cert provisioning/rotation, `caBundle` injection,
  fail-closed wiring, operator-SA exemption.** Owned by agentctl RFC 0007; the
  conversion handler is a route on that shared server (§4.2).
- **The admission/validation ladder (CEL + validating webhook).** Owned by agentctl
  RFC 0003 §7 / agentctl RFC 0007. Conversion and admission are distinct webhook
  kinds on the same server.
- **The reconcile/render loop, single-writer `.status`, the capability cache.**
  agentctl RFC 0006. This RFC's `.status.storedVersions` patches are issued by that
  operator.
- **The agent `contract_version` negotiation, graceful degradation, and the
  codegen/conformance pipeline.** The contract's own versioning is agent RFC 0014
  §6.3 (the reference impl's spec); the Axis-B client regen + conformance is
  agentctl RFC 0018. This RFC only decouples Axis A from Axis B (§3).
- **`AgentClass`/`IntelligenceService`/`MCPServerSet`.** agentctl RFC 0004. They
  are members of the same CRD group and **inherit this versioning policy verbatim**
  when they ship, but their shapes are defined there.
- **Conversion of non-agentctl objects** (ConfigMaps, the rendered workloads).
  Those are stock Kubernetes types; agentctl does not version them.

---

## 7. Open questions

1. **Beta deprecation-window length in agentctl-release terms.** §2.5 adopts the
   upstream "3 releases or 9 months (beta) / 12 months (GA), whichever is longer."
   agentctl's release cadence is unset; confirm the calendar floor dominates (i.e.
   pin the months, treat releases as the *minimum*) so a fast release train cannot
   shorten a user's migration runway below the calendar window.
2. **In-tree SVM vs. the operator-driven migrator as the default.** §2.4 ships
   both. Should the operator *always* drive its own list-and-touch migrator (one
   code path, works on every cluster) and use `storagemigration.k8s.io` only as an
   optimization where present — or prefer the in-tree API and fall back? Leaning
   operator-driven-default for portability + a single tested path.
3. **Whether to ever publish two GA majors simultaneously (`v1` + `v2` both
   served).** The window machinery supports it, but a permanent two-served-version
   posture doubles the converter and keeps the webhook on the hot path. Leaning:
   `v2` only ever overlaps `v1` during a bounded migration window, never as a
   standing offering.
4. **Checked-in vs. generated conversion fixtures.** The round-trip test (§5.2)
   needs a golden corpus. Reuse the `test/fixtures` `--capabilities`/CR corpus (RFC
   0001 §5), or maintain a dedicated per-bump conversion fixture set? Leaning a
   dedicated, version-pinned conversion corpus so a bump's losslessness proof is
   self-contained.
5. **Conversion-handler typing strategy at scale.** §5.2 converts on
   `serde_json::Value` for pass-through safety. If the converter grows many arms,
   is a thin typed "envelope" (typed `apiVersion`/`spec` head, `Value` tail) worth
   it, or does any typing reintroduce prune risk? Leaning stay fully `Value`-based.
6. **Does `AgentClass` (cluster-scoped, RFC 0004) need a distinct migration cadence**
   from the namespaced `Agent`/`AgentFleet`? Cluster-scoped objects are fewer but
   more load-bearing; the SVM choreography is identical, but the blast radius of a
   bad conversion differs. Confirm one policy covers the whole group.

---

## 8. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo decision record: §3 (the `kube-rs` gaps
  table — the "Conversion webhooks: None / hand-write, minimized by policy" and
  "Webhook TLS cert" rows this RFC's hand-written converter and §4.2 cert deferral
  build on); §2.5 (the cost ledger this conversion surface is part of); §5 (the
  `xtask` CRD-YAML emitter + `test/fixtures` the schema-diff check and round-trip
  corpus live in).
- **agentctl RFC 0003** — `Agent`/`AgentFleet` CRD schema & status contract: §8
  (the versioning *posture* this RFC fixes the mechanics of), §6 (`.status`,
  including `.status.contract.version` — the Axis-B home), §3.3 (`surfaces.a2a`
  accepted-but-inert — the §3.1 interaction example), §12 (ships `v1alpha1`
  single-served). The CRD *shape* this RFC migrates.
- **agentctl RFC 0004** — `AgentClass`/`IntelligenceService`/`MCPServerSet`: the
  group members that inherit this policy; `AgentClass.contractVersionRange` (the
  Axis-B pin, §3).
- **agentctl RFC 0006** — operator reconcile & capability model: the single-writer
  that issues the `.status.storedVersions` patches and drives the migrator
  (§2.4/§5.4).
- **agentctl RFC 0007** — admission validation ladder & webhook server: owns the
  shared `axum`/`hyper` TLS server, cert provisioning/rotation, `caBundle`
  injection, HA, and fail-closed wiring the conversion `/convert` route reuses
  (§4.2).
- **agentctl RFC 0018** — codegen & contract conformance: the Axis-B tooling
  (client regen + conformance) that handles `contract_version` movement, decoupled
  from this RFC's Axis-A machinery (§3).

**Contract spec (the reference implementation, agent RFCs)**

- **agent RFC 0014** — control-plane contract umbrella (the reference impl's
  contract spec): §6.3 (`contract_version` major.minor, additive-minor /
  breaking-major, runtime negotiation — the Axis-B clock §3 decouples from), §6.2
  (`surfaces{}` as the single discovery point — why runtime capability lives in
  `.status`, not the CRD `apiVersion`), §8 (graceful degradation on unknown
  surfaces).
- **agent RFC 0015** — management & control surface: §5.2 (the capabilities
  manifest the curated `.status` projection — and thus Axis B — reads from).

**Platform**

- Kubernetes **CustomResourceDefinition versioning** — `versions[]`,
  `served`/`storage`, `spec.conversion.strategy` (`None` default vs `Webhook`),
  structural-schema pruning, and `.status.storedVersions` (the mechanics of §1.1,
  §1.2, §2.4).
- Kubernetes **conversion webhooks** — the `apiextensions.k8s.io/v1`
  `ConversionReview` request/response contract the §5.2 handler implements.
- Kubernetes **`StorageVersionMigration`** (`storagemigration.k8s.io`) and the
  out-of-tree `kube-storage-version-migrator` — the storage-rewrite step (§2.4,
  §5.4).
- The **Kubernetes API deprecation policy** — the alpha/beta/GA support windows
  §2.5 maps onto agentctl releases.

**Brainstorm**

- agentctl architecture brainstorm — §2.2 (the `conversion: None` pruning finding
  and the "single served version + conversion webhook + SVM, decoupled from
  `contract_version`" correction this RFC implements), §15 (the agentctl-0005 track
  entry: "single-served-version + conversion webhook + SVM; alpha→beta→GA
  graduation; decoupled from agent contract_version").
