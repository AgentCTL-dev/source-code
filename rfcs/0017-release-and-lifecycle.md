# agentctl RFC 0017: Release & lifecycle engineering

**Status:** Proposed (agentctl lifecycle track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the release, upgrade, skew, disaster-recovery, GitOps, and air-gap engineering that the five-component control plane and the fleets of independently-versioned conformant agents it manages both require, *designed rather than improvised*

> **Two version clocks, never conflated (the spine of this RFC).** agentctl is a
> control plane that ships **its own** software (the five components + the CRDs)
> *and* manages a **data plane it does not ship** (any conformant agent build, on
> its own release cadence). Three independent clocks therefore tick under this
> RFC: the **agentctl release** (its component images + CRDs), the **CRD
> `apiVersion`** (agentctl RFC 0005, Axis A), and the agent **`contract_version`**
> (agent RFC 0014 §6.3, Axis B — the reference impl's contract spec). Every
> lifecycle operation below is precise about *which clock* is moving, because the
> single most expensive lifecycle mistake is letting one clock drag the others.

> **P0 (locked 2026-06-27).** The agent-image rolling upgrade (§3) rolls **any**
> conformant build, keying every decision off the **contract** — the re-probed
> capabilities manifest, the negotiated `contract_version`, the advertised
> `surfaces{}` — and **never** off a hardcoded reference-implementation version. A
> second-vendor agent that passes the conformance suite (agentctl RFC 0018) and
> advertises a contract major in range is rolled by the *identical* machinery. The
> reference implementation (agent) is the first conformant build, not a
> dependency; agent-branded contract surfaces (`--capabilities`, `agent://`, the
> `agent_` metric prefix, `AGENT_*` env) are cited here as the reference spelling
> only, flagged for neutralization under the standing contract-extraction question
> (P0; agentctl RFC 0018 / RFC 0001 §9).

---

## 1. Problem / Context

agentctl is a **multi-component control plane** — operator (with the admission +
conversion webhook server), node-agent (Tier A control+telemetry / Tier B A2A
data path: a node-pinned relay + a replicated stateless gateway), the
`crates/scaler` KEDA external scaler, the CLI/`kubectl agent[s]` plugin, and the
optional Go aggregated APIServer (the one sanctioned RFC 0001 §6 hybrid seam) —
that manages **fleets of independently-versioned conformant agents** across one or
many clusters. Four lifecycle concerns fall out of that shape, and the brainstorm
(§12) named them as whole areas *no other dimension owns*:

1. **Rolling a new agent build** through live fleets is the single most-repeated
   dangerous operation, and it is undesigned: it must drain in-flight work
   without dropping it, re-negotiate the contract on the new build's manifest,
   canary before it commits, and roll back cleanly.
2. **agentctl's own components** upgrade on their own cadence and *will* run
   skewed against one another and against the CRD storage version during any
   roll; without a stated skew window and rollout order, an upgrade is a coin
   flip — worst when it carries a CRD version bump (the agentctl RFC 0005
   conversion/SVM choreography).
3. **Stateful components must be recoverable.** Most of agentctl is
   reconstructable from the declarative CRs, but three stores are not — the
   durable A2A task store (RFC 0013), the coordination dedupe ledger (RFC 0011),
   and the signing/PKI key material (RFC 0014 / RFC 0015) — and a DR posture that
   does not distinguish the reconstructable from the irreplaceable wastes effort
   on the former and loses the latter.
4. **The deployment surface is GitOps-shaped and sometimes air-gapped.** CRDs are
   declarative, so Argo/Flux are the natural delivery path — but several of the
   patterns the other RFCs settled (the KEDA-owns-`replicas` single-writer rule,
   operator-rendered children, status churn) *fight* a naive GitOps setup unless
   the boundary is drawn deliberately. And the P0 contract-as-vendored-schema
   design (RFC 0001 §5) already makes most of the control plane air-gap-clean —
   but the irreducible model-egress leg and a handful of fetch points
   (cosign/Rekor, Krew index, JWKS) need an explicit offline story.

This RFC owns the **policy and choreography** for all four. It does **not**
redefine the machinery the siblings already own — it *sequences* it: the drain
contract (agent RFC 0011 §4 / agent RFC 0015 §4), the digest-keyed
`CapabilityProbe` capability model (RFC 0006 §4/§5), the conversion-webhook + SVM
mechanics (RFC 0005 §5), the durable-store + relay model (RFC 0013), the
coordination server (RFC 0011), the card-signer key lifecycle (RFC 0014 §4.3),
the internal PKI/egress trust model (RFC 0015), the conditions taxonomy (RFC 0003
§6.2), and the codegen/conformance pipeline (RFC 0018).

---

## 2. Decision — the lifecycle posture

agentctl commits to **seven** lifecycle rules; the rest of the RFC is their
elaboration.

| # | Rule | Section |
|---|---|---|
| **L1** | **Rolling a new agent build is a contract operation.** A build roll = an image-digest change → a `CapabilityProbe` re-probe → a `contract_version` re-negotiation off the *new* manifest → a re-render off the *new* `surfaces{}` → a partitioned (canary) roll with the lame-duck → drain → exit-0 choreography per pod → promote or roll back. Keyed off the contract, never a reference-impl version (P0). | §3 |
| **L2** | **The operator + CRD lead; the rest trail within one agentctl minor.** node-agent / gateway / scaler / APIServer MUST tolerate an operator one minor *ahead* and MUST NOT run *ahead* of the operator. The internal control-plane wires version additively-by-minor, like the contract. | §4 |
| **L3** | **A CRD version bump is the RFC 0005 §5 inner loop, sequenced inside the operator roll.** The conversion-handler-bearing operator is healthy *before* a second CRD version is served; the SVM runs *after*; the operator is **never** rolled back past a completed SVM. | §4.3, §5 |
| **L4** | **DR effort concentrates on the three irreplaceable stores.** The CRs (ideally in Git) reconstruct everything else; the durable task store, the coordination dedupe ledger, and the signing/PKI key material are the only state that *must* be backed up, each with its own RPO/RTO and restore drill. | §6 |
| **L5** | **GitOps owns the agentctl install + the CRs; the operator owns the rendered children.** GitOps MUST NOT manage rendered Deployments/ConfigMaps/ScaledObjects (operator-owned via SSA field manager `agentctl`) and MUST NOT manage `.spec.replicas` on an autoscaled fleet (KEDA's). | §7 |
| **L6** | **Air-gap is the default-clean path, not a port.** The contract is vendored (RFC 0001 §5); images pin by digest; the only runtime egress is the model channel, which in air-gap is in-cluster-only through the RFC 0012 egress proxy. The remaining fetch points (cosign, Krew, JWKS) get offline modes. | §8 |
| **L7** | **A release is the atomic unit, and the channel name tracks CRD stability.** An agentctl release pins `{component images + CRD version + contract_version range + vendored schemas}` together; the `alpha`/`beta`/`stable` channels map onto the RFC 0005 `v1alpha1`/`v1beta1`/`v1` graduation ladder. | §5.3 |

---

## 3. The agent-image rolling upgrade

This is L1 — the operation operators run constantly and the one most able to drop
work or strand a fleet. The unit of the roll is an **image digest**: changing
`AgentClass.image` (the common case — one edit rolls every Agent pinned to that
class) or an Agent's inline `spec.image` (RFC 0003 §3.1) from `sha256:OLD` to
`sha256:NEW`. Because the agent holds **no durable state** (distillate-only;
agent RFC 0009 / RFC 0020 §6), the roll is a pod-replacement problem with a
contract-renegotiation problem layered on top — *not* a data-migration problem.

```
AgentClass.image  sha256:OLD ──► sha256:NEW   (one edit; reconcile, RFC 0006)
   │
   1. resolve NEW spec.image → immutable digest          (RFC 0006 §5.3: never a tag)
   2. CapabilityProbe MISS on NEW digest → one-shot Job runs `--capabilities`,
      exits 0; cache by (digest + feature-set)            (RFC 0006 §5)
   3. STATIC re-negotiation (the GATE): read NEW manifest —
        contract_version (major.minor), surfaces{}, sub-schema tags (metrics/report/config/exit_codes).
        build_features is read but is opaque/informational — NEVER a gate (RFC 0004 §3.2, RFC 0003).
        │
        ├─ NEW major ∈ AgentClass.contractVersionRange ?   (RFC 0004 §3.1, Axis B)
        │     NO  → HOLD the roll. ContractCompatible=False/MajorUnknown (RFC 0003 §6.2);
        │           keep OLD pods serving; alert. The roll NEVER proceeds blind.
        ├─ NEW surfaces{} ⊇ AgentClass.requiredSurfaces ?   (RFC 0004 §3.3 — the co-equal P0-clean gate)
        │     NO  → HOLD the roll. A dropped REQUIRED surface is a condition failure,
        │           NOT a silent degrade ("Absent surface ⇒ admission/condition failure, not silent");
        │           keep OLD pods serving; alert.
        │     YES → continue
        ▼
   4. re-render the workload off NEW surfaces{} — drive ONLY what NEW advertises.
      A dropped OPTIONAL surface degrades gracefully; a dropped REQUIRED surface already HELD at step 3
      ("degrade, never assume" applies to OPTIONAL surfaces only; RFC 0006 §6, 0014 §8)
   5. PARTITIONED roll (canary, §3.3). Per replaced pod:
        lame-duck (NotReady; stop taking new work)         agent RFC 0015 §4.2
          └► drain (SIGTERM) → wind down at turn boundary → bleed in-flight
             → release claims → flush → exit 0             agent RFC 0011 §4.2 / agentctl RFC 0011 §3.3
          └► NEW pod up → node-agent re-`initialize` + re-attest + re-read manifest
             (LIVE re-negotiation)                          RFC 0008 §3.3 / RFC 0002 §7
   6. gate on health → PROMOTE (finish) | ROLLBACK (revert digest, §3.4)
```

### 3.1 Drain choreography — lame-duck → drain → exit 0

Replacing a pod cleanly is a **two-step** sequence, and conflating the steps is
the classic error. The contract gives agentctl two distinct primitives (agent
RFC 0015 §4):

- **`lame-duck`** is *stay-resident, flip readiness to NotReady* (agent RFC 0015
  §4.2). It does **not** exit. It removes the pod from Service endpoints and from
  the work it would newly accept, while letting in-flight work finish.
- **`drain` ≡ SIGTERM ≡ clean exit `0`** (agent RFC 0015 §4.1 / agent RFC 0011
  §4.2). The management `drain` tool *is* SIGTERM; it is **not** a
  drain-without-delete. A clean drain returns **`0`, not `143`** — the load-bearing
  exit-code fact (agent RFC 0011 §5) that makes a rolled reactive/loop
  Deployment look like success in dashboards instead of a `143` failure.

So the operator-driven graceful replacement is: **lame-duck (optional, for
zero-drop) → SIGTERM the pod → the agent's bounded drain runs inside
`terminationGracePeriodSeconds` → exit 0 → the new-digest pod starts.** Two
hard couplings carry over verbatim:

- **`drain.timeoutSeconds` < `terminationGracePeriodSeconds`** (the RFC 0003 CEL
  invariant; agent RFC 0011 §3.3). If the kubelet's SIGKILL lands before the
  drain budget elapses, the clean exit is lost (becomes `137`/`143`) and
  in-flight work is dropped — the internal budget is always the smaller number.
- **claim-mode fleets drain → bleed → release** (agentctl RFC 0011 §3.3; the
  agent-side claim-release contract is agent RFC 0019 §6): a rolled claim-mode pod
  MUST release its held claims so the item is re-offered rather than orphaned until a
  lease TTL elapses. This is the pod's SIGTERM path, **not** a CR finalizer (RFC 0006
  §8) — the roll deletes individual replica pods, not the CR.

**Two contract gaps the roll inherits** (carried, not solved here): a `once`/Job
pod drained mid-run during a roll loses its run report unless Tier A captured it
before GC — the read-before-exit / linger guarantee (P5; RFC 0008 / RFC 0010); and
readiness gating during the roll on a networkless substrate needs the exec health
verb (P1; RFC 0002), since kubelet HTTP probes cannot reach a networkless pod.

> **`pause`/`resume` are not contract-complete (P-pause).** The reference impl's
> `OPERATOR_TOOLS` is `[drain, lame-duck, cancel]`; `pause`/`resume` are not
> implemented (brainstorm §0.6). A "freeze the fleet, roll, unfreeze" upgrade mode
> is therefore **not** available in v1; the roll uses lame-duck + drain, which
> *are* shipped. If a turn-boundary suspend ever lands (P-pause), it becomes an
> additional, optional roll mode — it is not assumed here.

### 3.2 Contract re-negotiation on the new manifest (the heart of L1/P0)

A new build is, by definition, a build agentctl has not characterized. It may add
surfaces (additive minor), drop or rename them (breaking major), or change its
config schema. The roll therefore re-runs the **two-path capability model**
(RFC 0006 §4) against the new digest, in order:

1. **STATIC, pre-pod (the gate).** The `CapabilityProbe` re-probes the new digest
   (a cache **miss** by construction — the cache is digest-keyed; the *old* digest
   stays cached, which is what makes rollback a cache hit, §3.4). agentctl reads
   the new manifest and negotiates per agent RFC 0014 §6.3: it **refuses an
   instance whose major it does not know** and reads minor + the independently
   versioned sub-schemas (`metrics_schema`, `report_schema`, `config_schema`,
   `exit_codes`) to branch. The gate is **two-part, both keyed off the contract**:
   - if the new major is outside `AgentClass.contractVersionRange`, the roll **holds**
     — the new pods are never admitted, the old pods keep serving, and
     `ContractCompatible=False/MajorUnknown` surfaces the reason (RFC 0003 §6.2);
   - if the new digest's `surfaces{}` **omits any `AgentClass.requiredSurfaces` key**
     (e.g. `management` or `metrics`), the roll **holds** identically — the co-equal
     P0-clean capability gate (RFC 0004 §3.3): "Absent surface ⇒ admission/condition
     failure, **not silent**." A new build that drops a *required* surface is a held
     roll, surfaced as a condition failure (RFC 0003 §6.2), **not** a silent degrade.

   This is the single most important safety property of the roll: **a
   contract-incompatible build — whether by an out-of-range major OR by a dropped
   *required* surface — cannot silently replace a working fleet.** ("Degrade
   gracefully" is reserved for *optional* surfaces only; see step 4.)
2. **LIVE, post-pod (the confirmation).** When a new-digest pod starts, the
   node-agent re-`initialize`s the management connection, re-attests the
   pod→socket identity (RFC 0002 §7), and re-reads `agent://capabilities` —
   confirming the running instance matches the probed image and re-establishing
   the live snapshot the operator projects into `.status` (RFC 0008 §3.3). A
   reconnect is a clean re-read; there is no per-connection durable state to lose.

The re-render in step 4 of the diagram keys **entirely off the new
`surfaces{}`** — agentctl renders the management bridge, metrics scrape, exec
probe, A2A, and config-validate init-container *only for the surfaces the new
build advertises* (RFC 0006 §6). A build that added `surfaces.a2a` (once P2
lands) gains the gateway wiring on the roll; a build that dropped an *optional*
surface degrades gracefully rather than rendering a dead reference (a dropped
*required* surface already held the roll at the §3.2 path-1 gate). **Nowhere in this
path is there a hardcoded agent version or a `build_features` allowlist** — the
verb/feature set renders from the manifest, which is exactly what lets a
second-vendor conformant build roll through the identical code (P0).

### 3.3 Canary / partition rollout

A blind, all-at-once digest swap is forbidden for anything but a single dev Agent.
The roll is **partitioned**, and the partition mechanism is workload-kind-specific
(RFC 0003 §2.3 mode→workload table):

| Workload | Canary mechanism | Notes |
|---|---|---|
| **Deployment** (loop, reactive-claim, claim-mode fleet) | `RollingUpdate` with `maxSurge`/`maxUnavailable`; for finer control the operator stages the digest behind a small replica subset first | **reactive *singleton* MUST be `Recreate`/at-most-one** — default `RollingUpdate` `maxSurge` briefly runs two reactive pods on the same source → double-processing (RFC 0003 §2.3) |
| **StatefulSet** (shard fleet) | the built-in `.spec.updateStrategy.rollingUpdate.partition` — only ordinals `≥ partition` take the new digest; lower it stepwise to widen the canary | the natural ordinal canary; pairs with the shard-resize controller (RFC 0011) — a digest roll is **not** an `N`-change and must not be confused with one |
| **Job / CronJob** (`once`, `schedule`) | next-fire cutover — new fires use the new digest; in-flight `once` Jobs run to completion on the old digest | a `once` roll is a config change to the next Job template, not a live pod replacement |

The **promotion gate** between canary and full roll is read from the signals the
control plane already produces — agentctl invents no new telemetry for it:

- `ContractCompatible=True` and `Ready=True` on the canary pods (RFC 0003 §6.2);
- the frozen liveness/exit signals — canary pods exiting **`0` cleanly** (a clean
  drain is `0`, **not** `143`; §3.1, agent RFC 0011 §5). **`143` is a NEGATIVE
  signal**, alongside `2` (config/usage — non-retriable, a sign the new build rejects
  the rendered config), `4` (intelligence), and `6` (MCP): a canary that consistently
  exits `143` is failing to drain within budget and dropping in-flight work (§3.1), so
  it must **never** be read as a clean promotion signal. (These numerics are the
  frozen-contract exit-code table; the gate sources them from the negotiated
  `exit_codes` sub-schema, §3.2, not a hardcoded list.);
- intel-endpoint health and per-fleet rollups (RFC 0010 / RFC 0012).

Promotion can be operator-driven (an annotation/field advances the partition) or
automated against those signals. v1 ships the **manual-gate** form (advance the
partition; the operator surfaces canary health in `.status` and metrics); a
metric-gated auto-promotion controller is an open question (§Open questions),
deliberately *not* a v1 commitment, to avoid reinventing a progressive-delivery
engine (Argo Rollouts / Flagger compose here — §7).

### 3.4 Rollback

Rollback is **re-rolling to the previous digest** — and it is cheap *because* of
two prior decisions:

- **The old digest is still in the `CapabilityProbe` cache** (digest-keyed;
  RFC 0006 §5). Reverting `AgentClass.image` to `sha256:OLD` is a cache **hit** —
  no re-probe, no re-negotiation, immediate re-render off the known-good
  `surfaces{}`.
- **The agent is stateless.** There is no agent-side data to migrate backward; a
  rollback is symmetric with a roll-forward, using the identical drain
  choreography (§3.1).

What rollback must still respect is **in-flight work on the pods it tears down**,
and the contract already fixes the policy:

- **A2A tasks** owned by a rolled/rolled-back pod follow the RFC 0013 §6.5 owner-loss
  rule: default **FAIL + final webhook**, or idempotent re-drive gated on an
  explicit per-fleet opt-in (the re-drive epoch rejects stale SSE cursors). A
  rollback does not magically resurrect a live task whose owner pod is gone.
- **Claims** are released on drain and re-offered (§3.1); a rollback that drains
  cleanly loses no claimed item.

The dangerous rollback is **not** the agent build — it is rolling the *agentctl
operator* back across a CRD storage-version bump, which §4.3 forbids. The agent
build rolls back freely; the control plane's own version has a one-way gate.

---

## 4. The agentctl component upgrade & skew matrix

This is L2/L3 — upgrading the control plane's *own* five components, which run
skewed against one another during every roll. The governing facts:

- The control plane talks to the data plane over the **contract** (versioned on
  Axis B, negotiated, degraded gracefully). The control plane talks to *itself*
  over **internal wires** agentctl owns — the operator↔node-agent mTLS management
  API (RFC 0008/0009), the scaler↔coordination gRPC (RFC 0011), the gateway↔relay
  and gateway↔store paths (RFC 0013). Those internal wires version on the
  **agentctl-release** clock, **additively-by-minor like the contract**: a
  consumer tolerates unknown additive fields and refuses only an unknown major.
- The CRD storage version (Axis A) is the one piece of *persisted* control-plane
  state, and it is a **one-way high-water mark** (RFC 0005 §2.4).

### 4.1 The skew rule (who may lag whom)

```
        ┌──────────────────────────────────────── leads ──────────────────────────────┐
   CRD schema (Axis A)  ≡  operator (+ admission & conversion webhook)
        │                         │  internal wire = agentctl-release clock, additive-minor
        │                         ▼
        │            node-agent (Tier A, then Tier B relay) │ gateway │ scaler │ aggregated APIServer
        │                  may LAG the operator by ≤ 1 agentctl MINOR; MUST NOT LEAD it
        ▼
   conformant agent (Axis B / contract_version) — versions on its OWN clock,
        bounded only by AgentClass.contractVersionRange; NOT an agentctl component
```

| Component | Versions on | May lag operator by | May lead operator? | Violation symptom |
|---|---|---|---|---|
| **CRD schema** | agentctl release (Axis A) | n/a — moves *with* the operator | n/a | a CRD field the operator doesn't understand, or vice-versa |
| **operator** (+ webhooks) | agentctl release | — (the reference) | — | — |
| **node-agent Tier A** | agentctl release | **≤ 1 minor** | **No** | a Tier A ahead of the operator may use an internal-wire field the operator doesn't send → `ManagementUnreachable` |
| **node-agent Tier B** (relay) | agentctl release | **≤ 1 minor** | **No** | relay/store schema skew → A2A status writes rejected |
| **A2A gateway** | agentctl release | **≤ 1 minor** | **No** | gateway expecting a newer store schema than migrated |
| **scaler** (`crates/scaler`) | agentctl release | **≤ 1 minor** | **No** | newer `ExternalScaler` metric-spec the operator's ScaledObject doesn't request |
| **aggregated APIServer** (Go, optional) | agentctl release | **≤ 1 minor** | **No** | newer verb the CLI calls but the operator/node-agent can't service |
| **CLI / Krew plugin** | agentctl release (client) | **unbounded** (best-effort) | yes (degrades) | a CLI ahead of the cluster degrades per contract negotiation (RFC 0016) — client-side, low blast radius |
| **conformant agent** | **contract_version (Axis B)** | n/a — negotiated | n/a | major outside `contractVersionRange` → `ContractCompatible=False` (§3.2) |

The rule in one line: **the operator (and the CRD it carries) leads; every other
agentctl component trails it by at most one minor and never runs ahead; the agent
versions independently and is governed by negotiation, not by this matrix.**

### 4.2 Rollout order (no CRD bump — the common minor upgrade)

```
1. operator Deployment        rolling update; leader hands off via Lease (§5.1);
                              ≥2 webhook-serving replicas stay up behind a PDB (RFC 0005 §4.2)
2. node-agent DaemonSet       Tier A first (bounce-safe, rolls freely; RFC 0008 §3.3),
                              then Tier B (relay: drain-aware cadence; gateway: own PDB/surge, RFC 0008 §6)
3. gateway / scaler / APIServer   each on its own cadence within the §4.1 window
4. CLI / Krew                 client-side, independent (operators update when they choose)
```

The operator goes first because it is the only component that can be *ahead* of
the others; bringing it up first keeps the whole cluster inside the "trailers lag
the operator" invariant for the entire roll. Tier A is sequenced before Tier B
because a Tier A bounce is a control-gap-only event (RFC 0008 §3.3) while a Tier B
roll touches the A2A data path (drain-aware relay + the gateway's own PDB).

### 4.3 The CRD-bump interaction (L3 — RFC 0005 SVM ordering)

When an agentctl release carries a **CRD version bump** (a rename/restructure or a
graduation, RFC 0005 §2.4 — *not* the additive-in-place common case, which needs
nothing here), the RFC 0005 §5.5 choreography is the **inner loop nested inside
step 1** of §4.2, and the ordering is load-bearing:

```
1a. ROLL the new operator FIRST, carrying the /convert conversion handler (both arms)
    live — but the CRD is still single-version, conversion: None. Handler dormant, healthy.
        └─ TIE: the conversion-bearing operator MUST be healthy BEFORE a 2nd version is served.
           A 2nd served version while the webhook is down fails EVERY read/write of the CRD at
           the non-storage version — including the operator's own reconcile reads (RFC 0005 §4.2:
           conversion has NO failurePolicy; it is fail-closed by construction).
1b. APPLY the CRD update ATOMICALLY — add the new served+storage version, flip the old to
    storage:false (still served), set conversion.strategy: Webhook + caBundle (RFC 0005 §5.5 step 2).
1c. RUN the StorageVersionMigration (or the operator-driven migrator Job on clusters without
    storagemigration.k8s.io, RFC 0005 §2.4). Wait for completion; confirm .status.storedVersions
    collapses to [new]. THE SVM RUNS AFTER 1a/1b, NOT BEFORE.
1d. DEPRECATE → UNSERVE → REMOVE the old version across the RFC 0005 §2.5 deprecation window.
2.  THEN proceed with §4.2 steps 2–4 (node-agent, gateway, scaler, CLI).
```

The **one-way gate** this creates is the most important rollback constraint in the
whole RFC: **once an SVM has completed (stored objects are at the new storage
version), the operator MUST NOT be rolled back to a release that predates the new
version's conversion arm.** The old operator would lack the conversion function
for objects now stored at the new version, stranding every CRD read/write. The CRD
storage version is a high-water mark; component rollbacks (§3.4 applies to agents,
not the control plane) are bounded above by it. A release that bumps the CRD MUST
document this as a non-reversible step and ship the reverse conversion arm if a
downgrade path is required at all (RFC 0005 §5.3 round-trip losslessness makes the
*conversion* reversible, but the *operator binary* that contains it is the gate).

---

## 5. The operator's own HA & upgrade; release channels

### 5.1 Operator HA — single-leader reconcile, all-replica webhook serving

The operator runs as a **Deployment with ≥2 replicas**, and HA has a subtlety the
rest of this RFC depends on:

- **Reconcile is single-leader.** Exactly one replica holds the
  `coordination.k8s.io` `Lease` (via `kube-leader-election` / `kubert`, RFC 0001
  §3 / RFC 0006 §3.1) and runs the two controllers. This preserves the
  single-writer-on-`.status` discipline (RFC 0006 §8) — a second active reconciler
  would race the `DeepEqual`-guarded status writer.
- **Webhook serving is all-replica.** The admission webhook (RFC 0007) **and** the
  conversion webhook (RFC 0005 §4.2) MUST be served by **every** replica, not just
  the leader, behind a `PodDisruptionBudget`. Conversion is on the apiserver
  read/write hot path and has **no `failurePolicy` lever** — a conversion outage
  fails every non-storage-version CRD operation cluster-wide. So the webhook
  endpoints must outnumber and out-survive the leader.

This split — single-leader actuation, multi-replica policy serving — is what lets
the operator upgrade (§5.2) without a control-plane brownout.

### 5.2 Operator upgrade choreography

A `RollingUpdate` of the operator Deployment, with two guarantees:

- **Leader handoff is clean.** The departing leader releases (or lets expire) its
  Lease; an incoming replica acquires it. Reconcile pauses for at most the Lease
  acquisition latency; because reconcile is level-triggered and idempotent
  (RFC 0006 §3/§8), a brief pause loses nothing — the next reconcile derives
  desired state from `.spec` + observed cluster, never from a missed event.
- **The webhook never goes to zero endpoints.** The PDB + `maxUnavailable: 0` on
  the webhook-serving Deployment (or a surge strategy) keeps ≥1 healthy webhook
  endpoint throughout, so admission and conversion stay answerable mid-roll. This
  is mandatory, not advisory, given §5.1.

When the operator upgrade *also* bumps the CRD, §4.3's inner loop applies and the
new operator (with the conversion arm) is the one that goes up first.

### 5.3 Release channels tied to CRD graduation (L7)

A **release** is the atomic lifecycle unit: a coherent, co-versioned set of
`{operator + node-agent + gateway + scaler + (APIServer) + CLI images, the CRD
manifests, the `contract_version` range the operator manages, the vendored
contract schemas under `contract/`}` (RFC 0001 §5). Skew *within* a release is
zero; skew *across* an in-progress roll is bounded by §4.1.

agentctl ships **three channels**, and they map onto the RFC 0005 §2.5 CRD
graduation ladder — the channel name *is* the user-facing promise about CRD
stability. **These are *temporal maturity stages a cluster tracks*, not three
permanently co-served `apiVersion`s.** A cluster on the `alpha` channel serves
`v1alpha1`; as the project matures and the cluster crosses a channel boundary, its
CRD `apiVersion` **advances forward** (the one-time §4.3 graduation), and the old
version follows RFC 0005's deprecate → unserve → **remove** lifecycle (§5.5 steps
5–7) — it does **not** stay permanently served. The column below names the
`apiVersion` a cluster *on that channel* serves at steady state, not a standing
parallel release train; RFC 0005's single-served-version invariant is preserved.

| Channel | CRD `apiVersion` | Promise | Upgrade cadence |
|---|---|---|---|
| **alpha** | `v1alpha1` | API may change between minors; no migration guarantee (RFC 0005 §2.5 — but *still* never corrupts etcd; a real breaking change uses webhook+SVM once data exists) | fast; intended for dev / single-tenant / conformance clusters (the stock-unix tier, brainstorm D1) |
| **beta** | `v1beta1` | served by default; supported; the RFC 0005 deprecation window (3 releases / 9 months) before a bump | steady |
| **stable** | `v1` (GA) | long-term; 3 releases / 12 months deprecation floor | conservative; the production / hostile-multi-tenant kata-hybrid tier target |

Promoting a cluster *across* a channel boundary (alpha→beta, beta→stable) is, by
construction, a **CRD graduation event** — it executes the §4.3 / RFC 0005 §5.5
conversion+SVM choreography. Promoting *within* a channel is a §4.2 minor upgrade.
This binds the whole RFC together: the channel a cluster tracks selects its CRD
stability promise, its upgrade cadence, and (via the brainstorm D1 tiering) its
expected substrate posture.

---

## 6. Disaster recovery & backup (L4)

The DR posture starts from a classification, because most of agentctl is
**reconstructable** and only three stores are not. Effort spent backing up the
reconstructable parts is wasted; effort *not* spent on the irreplaceable three is
data loss.

### 6.1 The stateful inventory — reconstructable vs irreplaceable

| State | Where | Reconstructable? | From what |
|---|---|---|---|
| `Agent`/`AgentFleet`/`AgentClass`/`IntelligenceService`/`MCPServerSet` CRs | etcd | **Yes** — declarative source-of-truth | **Git** (GitOps, §7) is the real backup; Velero for non-GitOps clusters |
| Rendered children (Deployments, CronJobs, ConfigMaps, ScaledObjects, probe Jobs) | etcd | **Yes** | the operator re-renders from the CRs (RFC 0006); never backed up |
| `CapabilityProbe` cache | operator memory / ConfigMap | **Yes** | re-probe each digest on miss (RFC 0006 §5) |
| Leader-election `Lease` | etcd | **Yes** | re-acquired on restart (§5.1) |
| Webhook serving cert + `caBundle` | Secret / cert-manager | **Yes** | cert-manager re-mints (RFC 0007) |
| **Durable A2A task store** | Postgres (RFC 0013 §6) | **No** | task history, status, the webhook registry (encrypted creds), `tasks/list` index, rate-limit/quota state |
| **Coordination dedupe ledger** | the coordination server backend (RFC 0011) | **Partly** — leases self-heal; the `claim_key` dedupe ledger does **not** | — |
| **Signing / PKI key material** | KMS/HSM + Secrets (RFC 0014 / RFC 0015) | **No** | the card-signer private key, the internal mTLS CA, the JWKS trust anchor |

### 6.2 Per-store RPO/RTO and backup mechanism

| Store | Backup mechanism | RPO target | RTO target | Loss consequence (and the contract that bounds it) |
|---|---|---|---|---|
| **CRs** | Git (primary); Velero etcd/CR snapshots (fallback) | **0** with Git; else snapshot cadence | minutes (re-apply → operator re-renders) | none if in Git — the operator rebuilds the whole runtime from `.spec` |
| **Durable task store** | Postgres PITR (WAL archiving) + scheduled base backups; HA/replication delegated to the chosen Postgres deployment (RFC 0013 §6.2), **posture owned here** | **≤ 5 min** (WAL) | **≤ 30 min** | in-flight tasks → owner-loss rule (FAIL + final webhook, or gated idempotent re-drive; RFC 0013 §6.5); terminal history + webhook registry + quota counters lost back to the last backup |
| **Coordination ledger** | backend-dependent (Redis AOF / Postgres) — back up the **dedupe ledger**; leases are disposable | **≤ 1 min** for the dedupe ledger; n/a for leases | minutes | leases self-heal via TTL re-offer (agentctl RFC 0011 §3.3); a lost `claim_key` dedupe ledger risks **double-execution of non-idempotent compositions** (agentctl RFC 0011 §3.2 / brainstorm D4) — the reason this ledger is in the irreplaceable set |
| **Card-signer key + JWKS** | KMS/HSM escrow (RFC 0014 §4.3 custody) | **0** (static secret) | minutes | without escrow, a loss is a **revocation event**: rotate `kid`, edit the JWKS, re-sign every fleet card (RFC 0014 §4.3) |
| **Internal mTLS CA / webhook CA** | back up the CA key, or let cert-manager regenerate + roll | 0 if backed up | minutes–hours (rolling cert refresh) | a CA loss forces re-issue + a rolling restart of components to pick up the new trust anchor (RFC 0009 / RFC 0015) |
| **Provider/webhook secrets** | External Secrets / Vault (RFC 0015) | per the external store | minutes | re-sync from the external secret store; never the etcd-only copy |

The one-paragraph posture: **back up the three irreplaceable stores (task store,
coordination dedupe ledger, signing/PKI key material) on the cadences above;
treat everything else as reconstructable from the CRs, and keep the CRs in Git so
their RPO is zero.**

### 6.3 Restore ordering and drills

Restore is **dependency-ordered** — a component restored before what it depends on
just fails and retries:

```
1. PKI / key material         KMS/HSM + cert-manager CA          → components can mTLS; the signer can sign
2. agentctl install           CRDs + operator + webhooks (Helm/Kustomize from the mirror, §8)
3. the CR set                 re-apply from Git / Velero
      └► operator re-renders children, re-probes digests (cache rebuild), re-emits + re-signs cards (RFC 0014 §5.4)
4. durable task store         Postgres PITR restore             → A2A history / registry / quota back
5. coordination server        backend restore                   → dedupe ledger back; leases self-heal
6. node-agent / gateway / scaler reconnect → planes resume
```

**Restore drills are a requirement, not a nicety.** The chaos/e2e lane (RFC 0018)
MUST exercise, on a cadence (target: quarterly): a full restore from Git + a
task-store PITR restore; a coordination-ledger restore confirming `claim_key`
dedupe survives; a signing-key restore-from-escrow confirming card verification
still roots in the pinned JWKS; and the re-drive-epoch behaviour (RFC 0013 §6.5)
after a task-store rollback — confirming stale SSE cursors are rejected, not
mis-resumed. A DR plan that has never been rehearsed is a hypothesis.

---

## 7. GitOps fit (Argo / Flux) (L5)

CRDs are declarative, so Argo CD / Flux are the intended delivery path. The
brainstorm (§12) warned that several settled patterns *fight* a naive GitOps
setup; the boundary below makes them compose.

### 7.1 The ownership boundary — what GitOps manages, what the operator manages

```
   Git repo (desired state)            cluster (actual state)
   ┌────────────────────────┐  apply   ┌──────────────────────────────────────────────┐
   │ agentctl install:      │ ───────► │ CRDs, operator, node-agent, gateway, scaler,  │  GitOps field-manages these
   │   Helm/Kustomize        │         │ RBAC, cert-manager wiring, KEDA, Postgres ref  │  (install + the CRs)
   │ the CRs:                │ ───────► │ Agent / AgentFleet / AgentClass / Intel / MCP  │
   └────────────────────────┘         └───────────────────┬──────────────────────────┘
                                                            │ the OPERATOR field-manages (SSA, fieldManager="agentctl")
                                                            ▼
                                       Deployments / StatefulSets / CronJobs / ConfigMaps / ScaledObjects / probe Jobs
                                       ── GitOps MUST NOT manage these (operator-owned via ownerRef + SSA) ──
```

**GitOps owns the install and the CRs; the operator owns the rendered children.**
A GitOps tool configured to adopt the rendered Deployments/ConfigMaps/ScaledObjects
would enter a permanent three-way SSA fight with the operator's `agentctl` field
manager. So GitOps `Application`/`Kustomization` scope MUST be the agentctl CRs +
the install manifests, **never** the operator's output. This is the operator-vs-GitOps
boundary in one rule.

### 7.2 Server-side apply, field ownership, and the `replicas` trap

- **Use server-side apply with field ownership.** GitOps applies the CRs with its
  own field manager; the operator writes `.status` with `agentctl`; they own
  disjoint subtrees, so SSA does not conflict (the spec/status split, §7.3).
- **`.spec.replicas` is KEDA's, and GitOps must not touch it.** For an autoscaled
  fleet, the rendered workload **omits** `.spec.replicas` and the KEDA-generated
  HPA is the sole writer (the single-writer rule, RFC 0006 §point 7 / RFC 0011).
  Since GitOps manages the *CR* and not the rendered workload (§7.1), the classic
  Argo "replicas drift" fight does not arise here at the workload level — but if an
  operator *does* expose a replica-ish field on the CR for a non-autoscaled mode,
  GitOps tooling MUST `ignoreDifferences` it. The safe default: never put a
  KEDA-owned quantity in Git.

### 7.3 The spec/status split and drift detection

- **Status is operator-owned and curated.** `.status` carries only stable,
  structural facts + the conditions taxonomy (RFC 0003 §6) — churny per-run
  telemetry (token counts, backlog, inventory) is deliberately **excluded** and
  served via metrics / `kubectl agent describe` instead. This is what keeps GitOps
  drift detection sane: there is no high-frequency status churn for a differ to
  mistake for drift. Argo ignores `status` on custom resources by default; Flux
  likewise reconciles `.spec`.
- **Defaulting must not manufacture perpetual drift.** A *mutating* admission
  webhook (RFC 0007) that injects defaults makes the cluster object differ from
  the Git manifest forever, which a GitOps differ reports as drift on every sync.
  The mitigation: prefer **CRD schema defaults** (which GitOps tolerates — the
  apiserver applies them and SSA-aware diffing accounts for them) over webhook
  mutation for anything GitOps will diff, and where webhook mutation is
  unavoidable, document the `ignoreDifferences`/SSA-diff configuration. (RFC 0007
  owns which defaults live where; this RFC states the GitOps constraint on them.)

### 7.4 Custom health checks — reading the conditions taxonomy

A GitOps tool needs to know when an `Agent`/`AgentFleet` is *healthy* to gate sync
waves and report status. agentctl **ships the health checks** rather than leaving
operators to write them, reading the RFC 0003 §6.2 conditions:

| Condition state | GitOps health |
|---|---|
| `Ready=True` | **Healthy** |
| `Rendered=False` / `Ready=False` reason `RolloutProgressing` | **Progressing** |
| `ContractCompatible=False` (reason `MajorUnknown`) | **Degraded** — the build is contract-incompatible (the §3.2 held roll) |
| `Ready=False` reason `ManagementUnreachable` / `AttestationFailed` | **Degraded** |
| `phase: Failed` | **Degraded/Failed** |

Concretely: an **Argo CD Lua health check** and a **Flux health-check expression**
(both reading `.status.conditions`) ship in `deploy/` alongside the Krew/Helm
artifacts (RFC 0001 §5). Because the canary promotion gate (§3.3) reads the same
conditions, a progressive-delivery controller (Argo Rollouts / Flagger) **composes**
with the agent-image roll rather than duplicating it — agentctl provides the
health signal; the delivery tool drives the partition. agentctl does **not** ship
its own progressive-delivery engine (Non-goals).

### 7.5 Pruning

GitOps prune (deleting a CR removed from Git) MUST honour the operator's finalizer
(RFC 0006 §8): deletion blocks on the per-kind drain choreography, so a pruned
`Agent` drains rather than vanishing mid-task. Two guards: **prune-protect the
CRDs themselves** (resource exclusion / `Prune=false` on the CRD objects) so a
mis-scoped sync cannot cascade-delete every `Agent` in the cluster; and ensure the
GitOps tool's deletion respects finalizers (the default) rather than
force-deleting. A foreground/orphan misconfiguration here is the GitOps equivalent
of `kubectl delete --force` on the whole fleet.

---

## 8. Air-gapped / offline operation (L6)

Air-gap is the **default-clean** path because the P0 contract-as-vendored-schema
design already removed the largest would-be runtime dependency. What remains is a
short, enumerable list of fetch points, each given an offline mode.

### 8.1 Image & artifact mirroring

- **Every image pins by digest.** `AgentClass.image` / `spec.image` resolve to an
  immutable digest before use (RFC 0006 §5.3), so a mirrored private registry is a
  drop-in — there is no implicit tag-chasing pull from a public registry at
  runtime.
- **The full image set mirrors:** the five agentctl component images, the
  conformant-agent image(s), the reference coordination server (RFC 0011), the
  durable-store backend (Postgres, RFC 0013), KEDA, and cert-manager. All Helm
  charts and OCI artifacts push to the mirrored OCI registry.
- **Supply-chain verification works offline — key-based, keyless disabled.** cosign
  image verification (RFC 0015 supply chain) runs in **key-based** mode against
  mirrored signatures, using a long-lived cosign keypair. Sigstore **keyless** is
  **not** air-gap-viable: it additionally needs Fulcio CA roots, a TUF trust root, and
  a signing-time OIDC issuer — none reachable in an air-gapped cluster — so a mirrored
  Rekor alone does **not** make keyless verification work offline. The codegen input
  binary is hash-pinned (RFC 0018 / brainstorm §11.2), so no signing-time network call
  is on the runtime path.

### 8.2 Vendored contract schemas; no external calls at runtime

- **The contract is already vendored.** agentctl vendors the contract schemas
  under `contract/`, pinned by `(contract major.minor + digest)` (RFC 0001 §5 /
  RFC 0018). Codegen of `agent-contract-client` is a build-time, offline step; the
  conformance suite drives a *local* agent binary over the unix-socket dev loop
  (RFC 0002). There is **no schema fetch at runtime** — the P0/CC design is
  air-gap-clean by construction, not by retrofit.
- **The only irreducible runtime egress is the model channel** (brainstorm D1
  honesty correction: "no network = nothing to exfiltrate" is false). In an
  air-gapped cluster, intelligence MUST be **in-cluster**: the
  `IntelligenceService`/`ModelPool` (RFC 0012) points at in-cluster inference
  backends, model discovery is disabled, and the RFC 0012 **egress proxy is the
  chokepoint** that enforces in-cluster-only and blocks any external upstream.
- **A2A webhook delivery is allow-listed to in-cluster targets.** The RFC 0013
  webhook delivery already applies SSRF controls (metadata-endpoint blocking,
  allow-listing); in air-gap the allow-list is set to in-cluster/known peers only,
  so client-supplied webhook URLs cannot become an exfiltration channel.
- **Card verification needs no fetch.** The JWKS trust anchor is pinned
  out-of-band and `jku` is advisory-only (RFC 0014 §4.2) — a verifier never
  fetches a key over the network to validate a card, which is exactly the property
  air-gap needs.

### 8.3 Offline Krew / Helm / OCI

- **Krew normally fetches from the centralized index**, which an air-gapped
  cluster cannot reach. The offline story: ship the `kubectl-agent` /
  `kubectl-agents` binaries (distinct on-`PATH` names; RFC 0001 §5 / RFC 0016) in
  the install bundle for direct placement on `PATH`, and optionally host a
  **custom Krew index** in the mirror. The plugin itself makes no external call —
  it speaks only to the cluster apiserver via kubeconfig.
- **Helm/OCI** charts are served from the mirrored OCI registry (§8.1); the GitOps
  tool (§7) syncs from an internal Git mirror. The whole install + CR + chart
  supply chain is internal.

---

## Non-goals

- **Redefining the machinery the siblings own.** This RFC *sequences* the drain
  contract (agent RFC 0011 / RFC 0015 §4), the capability model (RFC 0006), the
  conversion/SVM mechanics (RFC 0005 §5), the durable store (RFC 0013), the
  coordination server (RFC 0011), the card-signer key lifecycle (RFC 0014 §4.3),
  the internal PKI/egress trust model (RFC 0015), and the codegen/conformance
  pipeline (RFC 0018). It owns none of their internals.
- **Shipping a progressive-delivery engine.** The canary roll (§3.3) exposes the
  conditions + metrics that Argo Rollouts / Flagger consume; agentctl provides the
  health signal and the partition primitive, not a new rollout controller. A
  metric-gated auto-promotion controller is an open question, not a v1 commitment.
- **Operating the user's Postgres / KMS / Vault.** §6 sets the RPO/RTO *posture*
  and restore *ordering* for the durable store, signing keys, and external
  secrets; the operational backup mechanics are delegated to the chosen
  deployment (RFC 0013 §6.2 / RFC 0015), which this RFC requires but does not run.
- **Control-plane self-observability + SLOs.** The operator/node-agent/gateway/
  coordination/webhook metrics and node-agent-SPOF alerting are agentctl RFC 0010
  (§12 self-observability); this RFC consumes those signals for promotion gates
  and restore drills but does not define them.
- **The contract-extraction (P0) and the contract primitive asks.** The standing
  neutral-contract extraction is RFC 0018 / RFC 0001 §9; the per-milestone agent
  asks (P1/P5/P-pause, etc.) are the brainstorm §14 cross-repo critical path, cited
  here where a lifecycle step inherits one.
- **Data residency / retention / compliance** (task-store region pinning,
  model-routing region affinity, erasure). Named in brainstorm §12; deferred to a
  future RFC / RFC 0015.

## Open questions

1. **Automated canary promotion (§3.3).** v1 ships the manual partition-advance
   gate. Should agentctl ship a metric-gated auto-promotion controller, or treat
   that as compose-with-Argo-Rollouts/Flagger territory permanently? (Leaning:
   compose — provide the health signal, not the engine.)
2. **Operator-driven migrator vs in-tree SVM as the default for CRD bumps.**
   Inherited from RFC 0005 Open Q 2: should the operator *always* drive its own
   list-and-touch migrator (one tested path, works everywhere) and use
   `storagemigration.k8s.io` only as an optimization? This RFC's §4.3 ordering is
   identical either way; the default choice is open.
3. **Cross-cluster / fleet-of-clusters lifecycle.** This RFC is single-cluster.
   Rolling a build or an agentctl release across *many* clusters (and the DR story
   for a regional loss) is a layer above — does it belong here, in a federation
   RFC, or in the GitOps tool's app-of-apps? (Leaning: GitOps-driven, with this
   RFC as the per-cluster contract.)
4. **Coordination-ledger RPO under high claim churn.** §6.2 sets ≤ 1 min for the
   dedupe ledger. For a high-throughput claim fleet, is synchronous ledger
   durability (and its latency cost) required, or is the FAIL-by-default +
   idempotent-opt-in posture (agentctl RFC 0011 §3.2 dedupe / RFC 0013 §6.5 owner-loss
   rule) sufficient to tolerate a small RPO?
5. **Signing-key escrow vs regenerate-and-re-sign as the DR default.** §6.2
   recommends KMS/HSM escrow (RPO 0). Is escrow mandatory, or is "regenerate `kid`
   + re-sign all cards" (RFC 0014 §4.3) an acceptable DR path that trades a
   verification gap for not holding escrow? (Leaning: escrow for the federation-grade
   posture; regenerate acceptable for in-cluster-only meshes.)
6. **Channel ↔ substrate coupling (§5.3).** Should the `stable` channel *require*
   the kata-hybrid hardened tier for hostile-multi-tenant clusters (brainstorm D1),
   or is the substrate an orthogonal cluster choice the channel only *recommends*?

## References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo decision record: §3 (the kube-rs gaps —
  leader election, webhook serving, conversion handler — this RFC's HA/upgrade
  builds on); §5 (the monorepo layout, `contract/` vendored schemas, `deploy/`
  Krew/Helm/health-check home, `test/` e2e+chaos lane); §6 (the one sanctioned Go
  aggregated-APIServer hybrid seam in the skew matrix); §9 (the P0 contract-extraction
  open question).
- **agentctl RFC 0003** — `Agent`/`AgentFleet` CRD schema & status contract: §2.3
  (mode→workload, the reactive-singleton double-processing trap that shapes §3.3);
  §3.1 (`spec.image` / classless Agent — the roll unit); §6 / §6.2 (the curated
  `.status` projection + conditions taxonomy the canary gate and GitOps health
  checks read); the `drain.timeoutSeconds < terminationGracePeriodSeconds` CEL
  invariant (§3.1).
- **agentctl RFC 0004** — `AgentClass`/`IntelligenceService`/`MCPServerSet`:
  `AgentClass.image` (the common roll unit) and `AgentClass.contractVersionRange`
  (the Axis-B pin the §3.2 re-negotiation gates on); the `IntelligenceService`/`ModelPool`
  the air-gap in-cluster-inference story (§8.2) points at.
- **agentctl RFC 0005** — CRD versioning & conversion policy: §2.4/§2.5 (the
  storage-version high-water mark, the graduation ladder the §5.3 channels map
  onto), §4.2 (conversion is hot-path + has no `failurePolicy` — the §5.1 HA
  driver), §5.5 (the exact conversion+SVM choreography the §4.3 inner loop nests).
- **agentctl RFC 0006** — operator reconcile & capability model: §3.1 (leader
  election), §4/§5 (the two-path STATIC/LIVE capability model + the digest-keyed
  `CapabilityProbe` cache the §3.2 re-negotiation and rollback-as-cache-hit rely
  on), §8 (the `DeepEqual` single-writer + per-kind finalizer drain the prune
  story §7.5 honours).
- **agentctl RFC 0007** — admission validation ladder & webhook server: the shared
  `axum`/`hyper` TLS server, cert provisioning/rotation, and the
  defaulting-source rule the GitOps drift story (§7.3) constrains.
- **agentctl RFC 0008** — node-agent architecture: §3.3 (the Tier A bounce-safe
  invariant + reconnect-is-a-clean-re-read that makes the §4.2 node-agent roll
  order safe), §6 (per-tier failure/blast-radius/upgrade — Tier A free cadence vs
  Tier B relay/gateway PDB), the LIVE re-negotiation (§3.2).
- **agentctl RFC 0009** — management access path & RBAC: the operator↔node-agent
  mTLS internal wire (a §4.1 skew axis) and the aggregated APIServer (the §4.1
  optional Go component).
- **agentctl RFC 0010** — observability & telemetry bridge: the run-outcome
  capture before once/Job GC (the P5 window §3.1 inherits), the exit-code
  observability the §3.3 gate reads, and the control-plane self-observability the
  restore drills (§6.3) lean on.
- **agentctl RFC 0011** — scaling plane: the claim/shard regimes (the §3.3 roll
  mechanics), the drain→bleed→release on SIGTERM (§3.1), the reference
  coordination MCP server + `claim_key` dedupe ledger (the §6 irreplaceable
  store), the shard-resize controller (distinct from a digest roll, §3.3).
- **agentctl RFC 0012** — intelligence plane: the egress proxy that is the air-gap
  in-cluster-only chokepoint (§8.2).
- **agentctl RFC 0013** — A2A gateway & task store: §6 (the durable Postgres store
  — the §6 irreplaceable store, its HA/DR delegation), §6.5 (the owner-loss
  rule + re-drive epoch the rollback §3.4 and restore drill §6.3 honour), §4.4
  (encrypted SSRF-guarded webhooks — the §8.2 air-gap allow-list).
- **agentctl RFC 0014** — agent mesh identity: §4.1/§4.3 (central card signing, the
  signing-key custody/rotation/revocation lifecycle the §6 DR posture backs up;
  the out-of-band-pinned JWKS the §8.2 air-gap relies on), §5.4 (low-churn
  re-sign-on-restore).
- **agentctl RFC 0015** — security & multi-tenancy: the internal mTLS
  CA / PKI lifecycle (the §6 restore-first dependency), the egress allow-list
  (§8.2), External Secrets/Vault (§6.2), the supply-chain cosign posture (§8.1).
- **agentctl RFC 0016** — CLI & kubectl-plugin grammar: the three CLI
  faces / Krew packaging the §8.3 offline-Krew story serves.
- **agentctl RFC 0018** — codegen & contract conformance: the vendored
  schema pinning + conformance suite (the §8.2 air-gap-clean contract path; the
  §6.3 restore-drill chaos lane); the P0 neutral-contract extraction home.

**Contract spec (the reference implementation, agent RFCs)**

- **agent RFC 0011** — cloud-native contract: §3.3 (`drain_timeout` <
  `terminationGracePeriodSeconds`), §4 (the drain choreography — disarm → wind
  down at turn boundary → ladder → flush → exit), §5 (the exit-code table; clean
  drain = `0` not `143`; `2` non-retriable; the canary gate's signals) — the
  reference impl's spec.
- **agent RFC 0014** — control-plane contract umbrella: §6.2 (`surfaces{}` as the
  single discovery point the re-render keys off), §6.3 (`contract_version`
  negotiation — refuse-unknown-major, read minor + sub-schemas — the §3.2 STATIC
  re-negotiation), §8 (graceful degradation) — the reference impl's spec.
- **agent RFC 0015** — management & control surface: §4.1 (`drain` ≡ SIGTERM ≡
  exit 0), §4.2 (`lame-duck` = stay-resident NotReady), §4.3 (`pause`/`resume` are
  *specified* here), §8 (reconnect = clean re-read) — the reference impl's spec. That
  the reference impl's `OPERATOR_TOOLS = [drain, lame-duck, cancel]` ships **without**
  `pause`/`resume` (the P-pause gap) is an *implementation* fact verified against
  agent source in **brainstorm §0.6**, not something agent RFC 0015 §4 states.
- **agent RFC 0009 / RFC 0020 §6** — distillate-only statelessness (why the agent
  roll is a pod-replacement, not a data-migration, problem) — the reference impl's
  spec.

**Platform**

- Kubernetes **`StorageVersionMigration`** (`storagemigration.k8s.io`) +
  conversion webhooks + `.status.storedVersions` (the §4.3 CRD-bump ordering).
- Kubernetes **Deployment/StatefulSet rollout** (`RollingUpdate` maxSurge/maxUnavailable;
  StatefulSet `partition`) — the §3.3 canary mechanisms.
- Kubernetes **`PodDisruptionBudget`** + leader-election `Lease`
  (`coordination.k8s.io`) — the §5.1 operator HA.
- **Argo CD / Flux** — server-side apply, field ownership, `ignoreDifferences`,
  custom health checks, prune protection (§7); **Argo Rollouts / Flagger** as the
  composable progressive-delivery engines (§3.3/§7.4).
- **Velero** — etcd/CR snapshot backup for non-GitOps clusters (§6.2).
- **cosign / Rekor**, **Krew custom index**, **OCI Helm** — the §8 offline supply
  chain.

**Brainstorm**

- agentctl architecture brainstorm — §12 (the completeness gaps this RFC owns:
  agent build/version upgrade choreography, GitOps fit, DR/backup, agentctl
  multi-component upgrade + skew, air-gap); §0.6 (P0 + the locked decisions; the
  `OPERATOR_TOOLS` / P-pause note); §14 (the contract asks P1/P5/P-pause this RFC
  inherits); §15 (the agentctl-0017 track entry); §16 (the phased roadmap — Phase 7
  hardening & lifecycle).
