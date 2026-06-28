# agentctl RFC 0007: Admission validation ladder

**Status:** Proposed (agentctl foundational track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the admission tier that rejects a bad `Agent`/`AgentFleet`/`AgentClass` *before* it schedules; it owns the full validation ladder that agentctl RFC 0003 §7 defers to.

> **A ladder, cheapest→most authoritative.** Validation is **four rungs**, each
> stricter, slower, and more authoritative than the last: (A) **CEL** single-object
> invariants in the apiserver; (B) the **webhook**'s cross-object + policy checks;
> (C) **config-schema** validation against the contract's published schema, cached;
> (D) the **init-container ground truth** that runs the *exact target image*'s
> `--validate-config`. Reject as early as a check is *sound*; defer to a later rung
> only what an earlier rung *cannot* decide. The only **authoritative** check is the
> exact image (rung D) — everything before it is a fast pre-filter.

> **P0 — the webhook validates against the *contract*, never a *binary*.** The
> webhook MUST NOT link a data-plane crate and MUST NOT exec a tenant-supplied agent
> image synchronously on the apply path (an image pull on the critical path; running
> a tenant binary in the control plane). It validates the rendered config against the
> **published config JSON Schema** (agent ask **P6**; cached per agentctl RFC 0006),
> and delegates ground truth to an **init-container running the tenant's own image**.
> The reference implementation (agent) is named only as the current home of a
> contract surface; a second conformant agent is admitted by the same ladder.

> **Trifecta is advisory; the *override* is gated, not the safe pattern.** A
> lethal-trifecta tag *union* across an `Agent`'s MCP servers is **not** a rejection —
> the agent already enforces Rule-of-Two **per-spawn** (agentd RFC 0012 §3.2), and the
> canonical safe shape (a reader/actor split) composes a union the admission layer must
> not refuse. The real control is gating the `allowTrifecta` **override** behind an
> explicit, audited annotation + elevated RBAC (§3).

---

## 1. Problem / Context

agentctl turns a `kubectl apply`'d `Agent`/`AgentFleet` (agentctl RFC 0003) into a
rendered workload (agentctl RFC 0006). The cheapest place to stop a wrong CR is
**before it is persisted and scheduled**: a config that would `CrashLoopBackOff`,
a substrate tier that violates the tenancy rule, a dangling reference, or a
name-collision should be rejected at `apply` time with a message the author reads
in their terminal — not discovered minutes later as a pod that never goes Ready.

The CRD's structural schema + CEL (`x-kubernetes-validations`, agentctl RFC 0003 §7)
catch the **single-object** errors in the apiserver, free, with no extra hop. But
three classes of check are structurally **inexpressible in CEL**, and agentctl RFC
0003 §7 enumerates them as exactly why a validating webhook is **mandatory**, not
optional:

1. **Cross-object trifecta-tag union.** Two individually-"safe" `MCPServerSet`s
   (agentctl RFC 0004) can compose the full lethal trifecta (`untrusted_input` +
   `sensitive` + `egress`, agentd RFC 0012 §3.1) on one `Agent` once
   `mcp.serverSetRefs` + inline `mcp.servers` are unioned. **CEL cannot fetch and
   union referenced objects.**

2. **MCP-server name collision across `serverSetRefs` + inline.** A duplicate server
   name between a referenced set and an inline addition is ambiguous. **CEL cannot
   dereference the sets to detect it.**

3. **Config-schema validation.** The rendered config file must validate against the
   contract's config JSON Schema (agentd RFC 0017 §4.2). **CEL cannot express a JSON
   Schema check**, and the schema is per-image, runtime-negotiated by contract major.

So "CEL is enough, no webhook" is false advertising (agentctl RFC 0003 §7). But the
honest converse is equally true: **the webhook is not enough either.** It cannot
exec the tenant's binary (P0 + apply-path latency), so its config check is only as
good as a *published, possibly version-skewed, possibly cache-missed* schema; and it
cannot see the **runtime env layer** the agent will actually resolve against —
downward-API identity, mounted secrets, and the `AGENT_SHARD` ordinal (agentd RFC
0014 §6.4) are not present at admission, so env/identity/shard/secret coherence is
structurally undecidable at apply time (brainstorm §3.3). The authoritative check is
the **exact image, with the exact env, in the pod**.

That asymmetry — each tier catches what the cheaper one cannot, and only the last is
authoritative — is why validation is a **ladder**, not a single gate. This RFC owns
the whole ladder: the rung boundaries, the webhook's cross-object/policy checks, the
trifecta-override gating, and the webhook's operational wiring (failure policy, cert,
HA, scoping, latency). It does **not** own: the CEL rules themselves (the CRD-resident
rung — agentctl RFC 0003 §7), the renderer or the capability cache the webhook reads
(agentctl RFC 0006), the substrate tiers and the tenancy×substrate *rule* it enforces
(agentctl RFC 0002 §5), the CRDs it dereferences (agentctl RFC 0004), or the webhook
HTTP server + cert plumbing it *runs on* (agentctl RFC 0001 §3).

---

## 2. Decision — the validation ladder

Validation is four rungs. A check lives on the **cheapest rung that can decide it
soundly**; anything an earlier rung cannot decide is deferred — explicitly, with a
named condition — to a later one. No rung silently re-does another's job.

```
                       cheapest / earliest                          most authoritative / latest
   ─────────────────────────────────────────────────────────────────────────────────────────►
   ┌── RUNG A ──────────┐  ┌── RUNG B ──────────────┐  ┌── RUNG C ─────────────┐  ┌── RUNG D ───────────────┐
   │ CEL / OpenAPI       │  │ validating webhook      │  │ config-schema check    │  │ init-container          │
   │ (in apiserver)      │  │ (operator, axum/hyper)  │  │ (in webhook, cached)   │  │ (in the pod, exact img) │
   ├─────────────────────┤  ├─────────────────────────┤  ├────────────────────────┤  ├─────────────────────────┤
   │ single-object        │  │ cross-object + policy:   │  │ rendered config vs      │  │ EXACT image runs        │
   │ invariants:          │  │ • trifecta UNION (advis.)│  │ PUBLISHED config schema │  │ `--validate-config`     │
   │ mode↔subscribe,      │  │ • server-name collision  │  │ keyed to contract MAJOR │  │ against rendered config │
   │ drain<grace,         │  │ • tenancy×substrate (§5) │  │ (agent ask P6),        │  │ + the REAL env layer    │
   │ claim XOR shard,     │  │ • references resolve     │  │ from RFC 0006 cache.    │  │ (downward API, secrets, │
   │ mode immutable,      │  │ • allowTrifecta override │  │ NEVER exec/link a binary│  │ AGENT_SHARD).          │
   │ instruction XOR ref  │  │   gating (§3)            │  │ (P0). file+flag layer.  │  │ exit 2 → CrashLoop.     │
   ├─────────────────────┤  ├─────────────────────────┤  ├────────────────────────┤  ├─────────────────────────┤
   │ <1 ms, no hop        │  │ p99 < 50 ms, 1 cache read│  │ same hop as B            │  │ pod-start, ~ms in-image │
   │ apiserver-native     │  │ fail-closed (§4)         │  │ cache miss ⇒ defer to D  │  │ AUTHORITATIVE           │
   └─────────────────────┘  └─────────────────────────┘  └────────────────────────┘  └─────────────────────────┘
        owned: RFC 0003 §7        ◄────────────  owned by THIS RFC  ────────────►          renderer: RFC 0006
   admission-time (pre-schedule, synchronous on `kubectl apply`)  ──────────────►  │  runtime (post-schedule)
```

Rungs A–C run **at admission** (synchronous, pre-schedule, on the `apply` path);
rung D runs **at runtime** (the first pod-start). The boundary between C and D is the
load-bearing seam: it is where agentctl stops trusting a *published schema* and starts
trusting the *exact binary*.

| Rung | Where | Owns | Catches | Cannot decide (defers to) |
|---|---|---|---|---|
| **A — CEL** | apiserver (in-process) | agentctl RFC 0003 §7 | single-object invariants (mode↔subscribe, drain<grace, claim XOR shard, mode immutability, instruction XOR ref, image XOR classRef, intelligence XOR intelligenceRef) | anything cross-object or contract-aware → **B** |
| **B — webhook** | operator (this RFC) | cross-object + policy | trifecta union (advisory, §3), server-name collision, tenancy×substrate (RFC 0002 §5), reference resolution, override gating | the config's *content* validity → **C**; env coherence → **D** |
| **C — config-schema** | webhook (this RFC) | structural config validity | rendered config vs the **published** config JSON Schema (P6), keyed to the image's contract major (RFC 0006 cache) | semantic checks needing the binary, and a cache-missed/new image → **D** |
| **D — init-container** | the pod (renderer: RFC 0006) | ground truth | the **exact image**'s `--validate-config` against the rendered config + real env; catches version skew, uncached builds, env/secret/shard coherence | — (authoritative) |

### 2.1 Rung A — CEL single-object invariants (the CRD-resident rung)

CEL `x-kubernetes-validations` on the CRD decide everything expressible per-object,
in the apiserver, with no network hop, **before any webhook is called** (the apiserver
runs structural + CEL validation, then dispatches admission webhooks). agentctl RFC 0003
§7 owns the rule set (mode↔work-source coherence, `drain<grace`, `claim XOR shard`,
mode immutability, instruction XOR ref, **image XOR classRef**, **intelligence XOR
intelligenceRef**, `scaling.max > scaling.min`, scale-from-zero only in claim mode,
`template.mode == reactive`). This RFC does not restate them; it
only fixes their **place in the ladder**: rung A is the floor, it is free, and the
webhook MUST NOT duplicate a check CEL already makes (duplication drifts).

### 2.2 Rung B — the webhook's cross-object + policy checks

The validating webhook is a first-class component from day one (agentctl RFC 0003 §7),
served by the operator (the axum/hyper TLS server of agentctl RFC 0001 §3). It decodes
the `AdmissionReview` by hand against the generated CRD types and runs the checks CEL
cannot. Each is **reject** unless marked advisory:

| # | Check | Verdict | Why CEL can't |
|---|---|---|---|
| B1 | **Trifecta-tag union** across inline `mcp.servers` + all `mcp.serverSetRefs` vs `security.allowTrifecta` | **advisory** (§3) — warns; never a bare reject | CEL cannot fetch + union referenced `MCPServerSet`s |
| B2 | **MCP-server name collision** across `serverSetRefs` + inline | **reject** (`NameCollision`) | CEL cannot dereference the sets |
| B3 | **tenancy × substrate** binding rule (agentctl RFC 0002 §5): reject a `stock-unix` effective tier for an untrusted-tenant namespace | **reject** (`SubstrateForbidden`) | CEL cannot read the namespace's tenancy posture nor resolve the `AgentClass` default |
| B4 | **References resolve**: `classRef`→`AgentClass`, `intelligenceRef`→`IntelligenceService`/`ModelPool`, `serverSetRefs`→`MCPServerSet` (agentctl RFC 0004) | **reject** (`ReferenceUnresolved`) | CEL cannot do cross-object existence |
| B5 | **`allowTrifecta` override gating** (annotation + RBAC + audit) | **reject** without the gate (§3) | CEL cannot read `userInfo`, issue a `SubjectAccessReview`, or require an annotation correlated to a field transition |
| B6 | **`A2AUnsupported`** when `surfaces.a2a: true` but the contract has not committed `surfaces.a2a` (agent ask **P2**, agentctl RFC 0003 §3.3) | **warn** + condition | contract-version-aware |
| B7 | **a `reactive` singleton whose resolved rollout strategy is `RollingUpdate`** (no claim/shard cross-instance ownership — the double-processing trap, agentctl RFC 0003 §5.1) | **warn** | needs the renderer's resolved strategy |

**B3, the tenancy×substrate rule, needs both the *effective tenancy* and the
*effective tier***, which the webhook resolves exactly as the operator's renderer will
(agentctl RFC 0006). The **effective tier** is `spec.substrate.tier` if set, else the
`AgentClass` default, else the cluster default. The **effective tenancy** is **`max(the
namespace's `agents.x-k8s.io/tenancy` label, the resolved `AgentClass.spec.substrate.tenancy`)`**
with `hostile` dominating `single` — the two-source `max()` is **decided in agentctl
RFC 0004 §3.3** (the source of truth for this formula), not invented here. Omitting the
`AgentClass` term would **under-enforce**: a class marked `substrate.tenancy: hostile`
in a `single`-labelled namespace must still be forced off `stock-unix`. The resolution
logic is a single internal agentctl crate shared by the webhook and the renderer
(agentctl RFC 0001 §5 `crates/crds`/`common`) — this is **not** the forbidden coupling
(P0 forbids linking a *data-plane* crate; sharing logic between two agentctl components
is the point of the one-workspace decision). An empty tier under effective-`hostile`
resolves to the **safe** `kata-hybrid` default (no reject); the reject (`SubstrateForbidden`)
fires only when the effective tier resolves to `stock-unix` under effective-`hostile`.
An **absent** `agents.x-k8s.io/tenancy` label fail-safes to `hostile` in the `max()` — the
binding rule (agentctl RFC 0015 §4.5): there is **no** weakening cluster-wide default that
relaxes an unlabelled namespace; a single-tenant cluster opts a namespace *down* by
**explicitly** labelling it `agents.x-k8s.io/tenancy: single` at provisioning. The class
posture is `AgentClass.spec.substrate.tenancy`.

**B4 reads the operator's informer cache, not live GETs.** The webhook reads the
operator's **per-replica** `reflector`/`Store` caches — populated on **every** replica
independent of leader election (agentctl RFC 0006 §3.4), so they back the webhook on
non-leader replicas too (§4.3) — so a referenced object lookup is in-memory (latency,
§4.5). A cache miss on a referenced ops-owned object
falls back to **one** bounded live `GET`; if that also misses, the webhook rejects with
`ReferenceUnresolved`. The GitOps co-apply-ordering hazard (a tenant `Agent` applied in
the same bundle as its `MCPServerSet`, before the set is cached) is real and is an open
question (§7) — the mitigation is "apply ops objects first," with a documented
two-pass `--dry-run` retry for bundles that violate it.

### 2.3 Rung C — config-schema validation (cached; never exec, never link)

The rendered config file (the `spec.config` projection, agentd RFC 0017 §3) must
validate against the contract's **config JSON Schema** (agentd RFC 0017 §4.2,
`--config-schema`). The webhook does this **against the published schema, in-process**,
and **never** by execing the tenant image (the red-team's load-bearing correction,
brainstorm §3.3 / §10.2):

- **Never link a data-plane crate (P0).** The operator validates against a JSON Schema
  document, using a schema validator over the rendered config `Value`. There is no agent
  binary in the dependency graph (agentctl RFC 0001 §4.2).
- **Never exec a tenant image synchronously (red-team).** Execing `agent --validate-config`
  inside the webhook would pull the tenant's image on the `apply` critical path and run a
  tenant-controlled binary in the control plane. Forbidden.
- **The schema is cached, keyed to the image's contract major.** The webhook fetches the
  config schema from the **capability cache** (agentctl RFC 0006) keyed by
  `(image digest + feature-set)`, populated by the `CapabilityProbe` one-shot Job that
  ran `--config-schema` once per unseen digest. The webhook selects the schema whose
  `x-agent-contract-version` major matches the negotiated contract for the image
  (agentd RFC 0014 §6.3); a config validating against a major agentctl does not
  understand is rejected (`ContractMajorUnknown`).
- **It validates the file+flag layers only.** The schema check decides the *structural*
  validity of the rendered config; it cannot run the binary's full `Config::validate()`
  semantic pipeline (agentd RFC 0011 §3.3) and cannot see the env layer. Those are rung D.

**Cache miss ⇒ defer to D, do not probe synchronously.** If the image digest is unseen
(no cached schema — a brand-new build, or a second-vendor agent whose schema has never
been probed), the webhook MUST NOT trigger a `CapabilityProbe` Job synchronously on the
`apply` path (image pull + Job latency). It **admits**, skips rung C, and records that
config validation is deferred to rung D (the operator sets `Validated=True` with reason
`ConfigSchemaDeferred` rather than `ConfigSchemaValid`, agentctl RFC 0003 §6.2). The
asynchronous probe still runs in the background so the *next* apply of that digest gets
rung C.

**Rung C gates on P6.** Both `--config-schema` (the emitter rung C consumes) and
`--validate-config` (rung D) are **unbuilt in the reference implementation today**
(agent ask **P6**). Until P6 lands and the schema corpus is published (agentctl RFC
0001 §4.5), rung C is inert: the ladder runs A + B, admits, and config validity degrades
to the contract's **validate-at-startup → exit 2** (agentd RFC 0011 §3.3) caught at pod
start — i.e. a bad config still fast-fails, just at runtime (rung D's fallback), not
pre-schedule. This is graceful degradation off `surfaces.config_schema`/`config_validate`
(agentd RFC 0014 §6.2), the same posture the whole control plane takes against an
unadvertised surface.

### 2.4 Rung D — init-container ground truth (the exact image)

The authoritative config check runs the **tenant's own image** against the **rendered
config + the real env**, as an init-container the operator injects (agentctl RFC 0006):

```yaml
# Rendered by the operator into every agent Pod (agentctl RFC 0006). The init-container
# is the SAME image as the agent container, so it is byte-identical to what will run.
initContainers:
  - name: validate-config
    image: <the agent image, by digest>          # the EXACT target image — version-skew-proof
    args: ["<config-validate invocation>"]        # RENDERER-RESOLVED per image (RFC 0006). Reference impl
                                                  #   spelling: --validate-config (agentd RFC 0017 §4.1; exit 0
                                                  #   valid / exit 2 invalid) — a BRANDED contract surface,
                                                  #   flagged for neutralization; a 2nd-vendor agent may spell
                                                  #   it differently. NOT a frozen flag in normative YAML.
    envFrom: [ ... ]                              # the SAME downward-API + secret env the agent gets
    env:                                          # downward-API identity (agentd RFC 0014 §6.4)
      - { name: AGENT_POD_UID, valueFrom: { fieldRef: { fieldPath: metadata.uid } } }
      # … AGENT_POD_NAME / _NAMESPACE / _NODE / _GRACE_SECONDS / AGENT_SHARD as rendered
    volumeMounts: [ { name: config, mountPath: /etc/agent } ]   # the rendered ConfigMap(s)
# exit 2 → the init-container fails → the Pod never starts the agent container → fast-fail.
```

**The config-validate invocation is renderer-resolved, not a frozen flag (P0).** The
`--validate-config` / `--config-schema` (§2.3) spellings are the **reference impl's
branding** of two contract surfaces (agentd RFC 0017, contract ask **P6**); a second
conformant agent may spell config validation as a different flag or subcommand. The
**renderer (agentctl RFC 0006)** owns the per-image spelling — it reads the surface from
the image's manifest and emits the right token, exactly as RFC 0002 §6.1 and RFC 0001
§4.3 flag comparable agent-branded CLI surfaces for contract-neutralization. This RFC
fixes only that rung D runs the *exact tenant image* against the rendered config + real
env; *how* that image is asked to validate is deferred to the renderer, pending neutral
contract extraction.

Why rung D is necessary even with rungs A–C:

- **Version skew.** Running `agent --validate-config` from a webhook-bundled binary
  validates against the *webhook's* version, not the tenant's. The init-container runs
  the tenant's exact image, so what it accepts is exactly what the daemon accepts (agent
  RFC 0017 §4.1: "no drift between the gate and the runtime").
- **Uncached / second-vendor builds.** It handles the cache-miss case (§2.3) and any
  conformant agent whose schema the operator has never probed — the binary is its own
  authority.
- **The env layer.** It is the **only** rung that sees downward-API identity, mounted
  secrets, and the `AGENT_SHARD` ordinal (agentd RFC 0014 §6.4 — the per-replica
  `K/N`, itself a contract defect tracked as ask **P3**, agentctl RFC 0003 §9.1). So
  env/identity/shard/secret coherence — undecidable at admission — is decided here.

**Failure surfaces as a condition, not a silent crash.** A rung-D `exit 2` fails the
init-container; the operator (RFC 0006) reads the failed init-container status and writes
`Degraded` with reason `ConfigValidatedAtRuntimeFailed` (agentctl RFC 0003 §6.2),
surfacing the agent's structured `config.invalid` diagnostics (agentd RFC 0017 §4.1) into
`.status` and events for `kubectl agents describe`. The init-container's `restartPolicy`
backoff is the natural fast-fail (a config that cannot validate never burns a Ready agent
container).

**A note on `--config-only` (P6).** P6 also asks for a `--validate-config --config-only`
mode that **tolerates absent env**. That mode is *not* for rung D (which has the full env)
— it is what would let an **optional** asynchronous operator pre-flight run the binary's
full semantic `Config::validate()` (richer than rung C's schema-only check) **without** the
pod's env, in a `CapabilityProbe`-style Job, closing rung C's semantic gap before pod-start
without the version-skew of a webhook-bundled binary. It is an optional refinement, not a
fifth synchronous rung; the four-rung ladder stands without it.

### 2.5 What each rung deliberately does NOT do (the seams)

- **The webhook never mutates.** This is a *validating* webhook (allow/deny + warnings);
  defaulting (tier from `AgentClass`/tenancy, env injection, ConfigMap partitioning) is
  the renderer's job (agentctl RFC 0006) and the CRD's `default`s. Keeping admission
  mutation-free keeps `sideEffects: None` honest (§4.6).
- **The webhook never writes `.status` and makes no out-of-band write.** Its only
  outputs are fields of the `AdmissionResponse` (`allowed` + `status.message` +
  `warnings[]` + `auditAnnotations`); the `auditAnnotations` are written to the audit
  log by the **apiserver**, not by the webhook, so they are not a side effect and are
  dry-run-aware (§3.3, §4.6). Advisories that must persist (the trifecta union,
  `A2AUnsupported`, the `RollingUpdate` warning) are surfaced as apply-time `warnings`
  AND mirrored into conditions by the operator (the single `.status` writer, agentctl
  RFC 0006) — the webhook does not race the operator for `.status`.
- **The webhook never gates deletion.** Operations are **CREATE + UPDATE only**; `DELETE`
  is never intercepted (a fail-closed webhook on `DELETE`/finalizer writes strands objects
  in `Terminating`, §4.2).

---

## 3. Trifecta-override gating (the red-team correction)

This is the section the brainstorm flags hardest (§2.2 point 3, §10.1) and the one a
naive admission design gets wrong. The correct design is: **detect the union and advise;
gate the *override*, never the safe pattern.**

### 3.1 Why a blocking union is wrong

The instinct is "if an `Agent`'s MCP servers collectively span all three trifecta legs
(`untrusted_input` + `sensitive` + `egress`, agentd RFC 0012 §3.1), reject it unless
`allowTrifecta: true`." That is wrong, for two contract-level reasons:

1. **The agent already enforces Rule-of-Two — per spawn, not per Agent.** agent checks
   the trifecta budget at the `subagent.spawn` chokepoint over **each child's narrowed
   grant** (agentd RFC 0012 §3.2), refusing any *single* subagent that would hold all
   three legs. The enforcement is real, unforgeable, and at the right granularity (one
   isolation unit = one process).

2. **The canonical *safe* shape composes an Agent-level union.** The blessed pattern is a
   **reader/actor split**: a no-sensitive/no-egress reader subagent that quarantines
   untrusted content and returns a distilled summary, and a no-untrusted-input actor that
   consumes the summary and holds the sensitive/egress tools (agentd RFC 0012 §3.3). That
   Agent *declares* servers spanning all three legs — its tag union **is** a full trifecta
   — yet **no single spawn ever holds all three**. A blocking Agent-level union would
   **refuse this exact safe configuration**, and would train operators to flip
   `allowTrifecta` routinely "to make admission shut up" — defeating the gate it pretends
   to be.

So the union, by itself, is **not** an error. It is a fact worth surfacing, nothing more.

### 3.2 Advisory union detection

The webhook computes the union across inline `mcp.servers` + all `mcp.serverSetRefs`
(agentctl RFC 0004), using the **operator-declared** tags (never server-declared metadata
— agentd RFC 0012 §3.4):

```
union(tags) = ⋃ { server.tags : server ∈ inline ∪ resolved(serverSetRefs) }
isTrifecta  = union ⊇ { untrusted_input, sensitive, egress }
```

When `isTrifecta` and `security.allowTrifecta == false` (the default): **admit**, attach
an `AdmissionResponse.warning` naming the offending servers and the three legs, and let
the operator mirror it as the **standalone `TrifectaUnionObserved` advisory condition**
(agentctl RFC 0003 §6.2 — its own `metav1.Condition` with reason `TrifectaUnion`, **not**
a `reason` overloaded onto `Validated`, so it never collides with `Validated`'s
`AdmissionPassed`/`ConfigSchemaDeferred` reason). The agent's per-spawn check is the live
control; the safe reader/actor split is admitted **unchanged**. This is observational,
not blocking.

### 3.3 Gating the override (annotation + RBAC + audit)

The dangerous thing is not the union — it is **turning the per-spawn guard off**.
`security.allowTrifecta: true` renders the contract's `--allow-trifecta`, which flips the
spawn chokepoint from `Refuse` to `Warn` (agentd RFC 0012 §3.2) so a *single* subagent may
hold all three legs. And that flag is **process-global** today (agentd RFC 0012 §6 open
item — a per-route/per-spawn override is deferred), so its blast radius is the whole
daemon. That is precisely the decision that warrants friction. The webhook gates it:

> **Binding rule.** An `Agent`/`AgentFleet` whose effective `security.allowTrifecta` is
> `true` (on CREATE, or any UPDATE transitioning it `false → true`) is **rejected** unless
> **both** hold:
> 1. the **explicit override annotation** `agents.x-k8s.io/allow-trifecta-override` is
>    present and non-empty (a justification / change-ticket string), **and**
> 2. the requesting identity (`AdmissionReview.request.userInfo`) passes a
>    `SubjectAccessReview` for the dedicated verb `override-trifecta` on the resource
>    (`agents`/`agentfleets`) in the target namespace — the **elevated RBAC** grant.
>
> On admit, the webhook records the override as **`AdmissionResponse.auditAnnotations`**
> (keys in the closed `mgmt.invoked`-class vocabulary, agent ask **P-audit** / agentctl
> RFC 0015 audit vocabulary) carrying `{user, namespace, name, annotation, legs}`. The
> **apiserver** writes those annotations into its **own audit log** — this is the
> emitter, not an out-of-band write by the webhook, so it is **dry-run-aware** (no audit
> record on `dryRun==true`) and keeps `sideEffects: None` honest (§4.6). The webhook does
> **not** create an Event, write `.status`, or call any external system. Without the
> annotation **or** the RBAC, the reject message names the **reader/actor split** as the
> non-override alternative.

Two reasons this shape is right and minimal:

- **Per-field RBAC does not exist on a CRD.** Kubernetes RBAC is verb-on-resource, not
  verb-on-field; you cannot natively say "alice may set `allowTrifecta` but bob may not."
  The webhook synthesizes it: it reads `userInfo` from the `AdmissionReview` and issues a
  `SubjectAccessReview` for a synthetic `override-trifecta` verb, so the elevated grant is
  ordinary RBAC an operator can audit with `kubectl auth can-i`.
- **The annotation makes the override *legible and audited*.** It forces an author to state
  *why* (a ticket/justification), it makes the override greppable in Git (GitOps review),
  and it pairs with the audit event so a flip is never silent. The annotation alone is not
  sufficient (any author can write an annotation); the RBAC is the authority and the
  annotation is the intent + audit trail. Both, together.

This composes with the agent's own audit: agent logs `scope.trifecta_grant` at `warn` on
each overridden spawn (agentd RFC 0012 §3.2), so the override is auditable at *both* the
admission boundary (who turned it on) and the runtime boundary (which spawns used it).

### 3.4 Worked union computation

```jsonc
// An Agent referencing two "safe" MCPServerSets + one inline server.
// Each set is individually NOT a trifecta; the Agent-level UNION is.
//
// MCPServerSet "inbox-readers":  { web:[untrusted_input], parser:[untrusted_input] }   // legs: {untrusted_input}
// MCPServerSet "crm-tools":      { crm-read:[sensitive] }                              // legs: {sensitive}
// inline mcp.servers:            { mailer:[egress] }                                   // legs: {egress}
//
// union = {untrusted_input, sensitive, egress}  →  isTrifecta = TRUE
//
//  allowTrifecta:false  →  ADMIT + warning (the reader/actor split is the safe shape; per-spawn guard live)
//  allowTrifecta:true   →  REJECT unless override annotation + override-trifecta RBAC (§3.3), then ADMIT + audit
```

The webhook's verdict here is `admit + warning` (because `allowTrifecta` defaults `false`):
the operator is free to wire `web`/`parser` into a reader subagent and `crm-read`/`mailer`
into an actor; the agent's spawn chokepoint guarantees no single child gets all three.

---

## 4. Webhook operational reality

A fail-closed admission webhook is a piece of **critical-path control-plane
infrastructure**: if it is wrong, nobody can apply an `Agent`; if it is *too* broad, it can
deadlock its own bootstrap or wedge core cluster operations. The wiring below is as
load-bearing as the checks above.

### 4.1 Scope — resources, operations, selectors

```yaml
# ValidatingWebhookConfiguration (rendered by deploy/, agentctl RFC 0001 §5 deploy/)
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: agentctl-validating
  annotations:
    cert-manager.io/inject-ca-from: agentctl-system/agentctl-webhook-cert   # caBundle injection (§4.4)
webhooks:
  - name: validate.agents.x-k8s.io
    admissionReviewVersions: ["v1"]
    sideEffects: None                       # no mutation, no external write → dry-run safe (§4.6)
    timeoutSeconds: 5                        # tight budget (§4.5); apiserver hard-caps at 30
    failurePolicy: Fail                      # fail-CLOSED for the tenant surface (§4.2)
    matchPolicy: Equivalent
    rules:
      - apiGroups:   ["agents.x-k8s.io"]
        apiVersions: ["v1alpha1"]
        operations:  ["CREATE", "UPDATE"]    # NEVER DELETE (§4.2 / §2.5)
        resources:   ["agents", "agentfleets", "agentclasses"]
    namespaceSelector:                       # exclude control-plane namespaces (§4.2)
      matchExpressions:
        - { key: kubernetes.io/metadata.name, operator: NotIn, values: ["kube-system","agentctl-system"] }
        - { key: control-plane,               operator: DoesNotExist }
    objectSelector: {}                       # all CRs in selected namespaces (the CRDs are ours)
    matchConditions:                         # CEL: exempt the operator's own SA (§4.2)
      - name: exclude-operator-sa
        expression: "request.userInfo.username != 'system:serviceaccount:agentctl-system:agentctl-operator'"
    clientConfig:
      service: { namespace: agentctl-system, name: agentctl-webhook, path: /validate, port: 443 }
```

`AgentClass` is cluster-scoped, so its admission is governed by the `rules` entry (the
`namespaceSelector` does not constrain a cluster-scoped resource) and it has **no
namespace** at admission time. The tenancy×substrate check therefore **splits** (agentctl
RFC 0004 §3.3): on the **`AgentClass`** the check is **self-contained** — reject
`spec.substrate.tenancy == hostile && spec.substrate.tier == stock-unix` using the
class's *own* `substrate.tenancy` field (a single-object invariant, CEL-able, no
namespace needed). The **namespace-dependent** part — the effective-tenancy `max()` with
the *consuming* namespace's label — is re-evaluated at **`Agent`** admission (B3 above),
the only point a namespace is in scope. The webhook does **not** try to enumerate "the
namespaces a class is referenced by."

### 4.2 failurePolicy, the bootstrap deadlock, and the control-plane exemption

**`failurePolicy: Fail` (fail-closed) for the tenant surface.** Under hostile tenancy a
webhook that fails *open* would let a tenant slip a `stock-unix` agent (B3) or an
ungated `allowTrifecta` override (B5) through while the webhook is down — a security
regression. So the tenant-facing webhook fails **closed**. But fail-closed has two sharp
edges this RFC must defuse:

- **The operator's own writes must not be gated.** A `Fail` webhook on `UPDATE agents`
  intercepts the operator's **finalizer add/remove** (a metadata UPDATE), so a webhook
  outage would strand every `Agent` in `Terminating` and break reconcile (agentctl RFC
  0003 §7, brainstorm §3.2). **The operator ServiceAccount is exempted via `matchConditions`**
  (§4.1) so its finalizer/`.status` writes never traverse the webhook. Finalizer removal
  MUST NOT depend on the webhook — full stop.

- **The bootstrap deadlock.** If the webhook intercepted the resources needed to bring
  *itself* up — its own Deployment, Service, the cert Secret, core objects in `kube-system`
  — then a cold cluster (or a webhook that is down) could never schedule the very pods that
  serve the webhook: a self-inflicted deadlock. Defused three ways: (1) the `rules` scope is
  **only** the three agentctl CRDs — the webhook never sees core/`apps` resources, so it can
  never block its own Deployment/Service/Secret; (2) the `namespaceSelector` **excludes
  `kube-system` and `agentctl-system`** (the operator's own namespace), so control-plane
  and operator-namespace objects bypass it entirely; (3) the operator SA exemption (above).
  The combination means "the webhook can only ever reject a tenant's `Agent`/`AgentFleet`/
  `AgentClass`" — it has no reach over anything required to start or recover itself.

A `single`-tenancy / dev cluster MAY relax to `failurePolicy: Ignore` to trade the security
guarantee for availability (a webhook outage degrades to rungs A + D — CEL still fires,
and a bad config still fast-fails at the rung-D init-container). Under **hostile tenancy
`Ignore` is forbidden** — it would void B3/B5.

### 4.3 HA — replicas, PDB, priority

The webhook is served **in the operator process**, but the operator is **leader-elected**
(one active reconciler) while the **webhook must be served by every replica** (admission is
not leader-gated — any replica behind the Service must answer). So:

- **≥ 2 replicas** of the operator Deployment serving the webhook endpoint (≥ 3 recommended
  to survive a node failure *during* a rollout), behind the webhook `Service`, with a
  **`PodDisruptionBudget`** (`minAvailable: 1`, ideally `maxUnavailable: 1` with ≥3) so a
  node drain / voluntary disruption never takes the last webhook serving pod.
- **`topologySpreadConstraints`** across nodes/zones so a single node loss cannot remove all
  webhook endpoints.
- **`priorityClassName: system-cluster-critical`** so the webhook pods are scheduled and
  protected ahead of tenant workloads (a fail-closed webhook that gets evicted under pressure
  wedges all applies).
- Leader election (agentctl RFC 0001 §3, `coordination.k8s.io` Lease) gates only the
  *reconcile actuation* (SSA writes, the `.status` patch); the webhook HTTP handler is
  stateless and runs on **all** replicas. Normatively, **each replica runs its own
  `watcher`/`reflector` `Store`s, populated independently of the `Lease`** (agentctl RFC
  0006 §3.4), so a non-leader replica answers admission against a **warm** cache — never
  an empty/stale Store. If the informers were leader-gated, a non-leader webhook would
  emit false `ReferenceUnresolved` (B4) and miss trifecta unions (B1); the
  caches-on-every-replica rule is what makes the fail-closed webhook correct under HA.

### 4.4 The serving cert — cert-manager default, in-repo fallback

The webhook needs a serving cert whose CA the apiserver trusts via the
`ValidatingWebhookConfiguration`'s `caBundle`. agentctl uses the **same** mechanism on
Rust that a Go shop uses (agentctl RFC 0001 §3 is explicit that cert provisioning/rotation
is cert-manager on either stack — it is *not* a controller-runtime freebie):

- **Default: cert-manager.** A `Certificate` issues the serving cert into a Secret the
  operator mounts; **cert-manager's ca-injector** patches the `caBundle` into the webhook
  config (the `cert-manager.io/inject-ca-from` annotation, §4.1). Rotation is cert-manager's
  job; the operator hot-reloads the cert from disk.
- **The cert-manager-absent fallback (the real Rust-only delta, agentctl RFC 0001 §3).** On
  a cluster without cert-manager, agentctl ships a **small in-repo cert controller** that
  mints a self-signed CA, issues + rotates the webhook serving cert, **patches the webhook
  `caBundle` itself**, and hot-reloads the cert on disk (the `CertWatcher` equivalent). This
  is the one genuinely new piece of cert engineering agentctl owns (controller-runtime would
  not have given it either — only cert-manager does), and it is bounded and well-trodden.
- The serving stack (axum/hyper TLS) is agentctl RFC 0001 §3; this RFC only fixes that the
  cert path has a cert-manager default **and** a no-cert-manager fallback, because a webhook
  with no trusted cert is a webhook the apiserver refuses to call (which, fail-closed, wedges
  applies).

### 4.5 Timeout / latency budget

Admission is **synchronous on every `apply`** of an agentctl CR, so its latency is the
tenant's `kubectl apply` latency. The budget is tight and the design protects it:

- `timeoutSeconds: 5` (the apiserver hard-caps at 30; 5 leaves headroom for a retry within
  the apiserver's own budget). Target **p99 < 50 ms** for the handler.
- **No synchronous image pull, no binary exec, no remote model/API calls** (the whole reason
  rungs C/D are split off the webhook). The only I/O is a **cache read** (shared informer
  `Store`, §2.2) plus, at most, **one** bounded live `GET` on a reference cache-miss (§2.2)
  and **one** `SubjectAccessReview` when (and only when) `allowTrifecta: true` (§3.3). A
  cache-missed config schema **defers to rung D** rather than blocking (§2.3) — config
  validation never adds an image pull to the apply path.
- CEL (rung A) has already run in the apiserver before the webhook is invoked, so the webhook
  never re-evaluates single-object invariants.

### 4.6 `sideEffects: None` + dry-run

The webhook performs **no out-of-band mutation** (it does not write `.status`, create
objects, or call external systems) — its `SubjectAccessReview` and informer reads are
**reads**, not side effects. So `sideEffects: None` is honest, which lets the apiserver call
the webhook during **`--dry-run=server`** and during other dry-run evaluations. The handler
MUST honour `AdmissionReview.request.dryRun == true` identically to a live request (it
already does — it mutates nothing), so `kubectl apply --dry-run=server` gives an author the
*exact* admission verdict (including the trifecta override gate and the tenancy×substrate
reject) without persisting anything. This is the recommended pre-merge GitOps check.

---

## 5. Worked AdmissionReview examples

Both use `admission.k8s.io/v1`. Requests are abridged to the load-bearing fields.

### 5.1 Rejected — `stock-unix` under hostile tenancy (B3)

```jsonc
// → REQUEST (apiserver → webhook): an Agent pinning stock-unix in a hostile-tenant namespace
{
  "apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
  "request": {
    "uid": "9b2c-…",
    "resource": { "group": "agents.x-k8s.io", "version": "v1alpha1", "resource": "agents" },
    "namespace": "tenant-acme",                       // label: agents.x-k8s.io/tenancy=hostile
    "operation": "CREATE",
    "userInfo": { "username": "system:serviceaccount:tenant-acme:deployer" },
    "dryRun": false,
    "object": {
      "kind": "Agent", "metadata": { "name": "triage", "namespace": "tenant-acme" },
      "spec": { "mode": "reactive", "substrate": { "tier": "stock-unix" }, "subscribe": ["fs:file:///in/*"] }
    }
  }
}
```
```jsonc
// ← RESPONSE: B3 reject — effective tier resolves to stock-unix for an untrusted tenant
{
  "apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
  "response": {
    "uid": "9b2c-…",
    "allowed": false,
    "status": {
      "code": 422,
      "reason": "Invalid",
      "message": "SubstrateForbidden: substrate.tier 'stock-unix' is forbidden for an untrusted-tenant namespace (agents.x-k8s.io/tenancy=hostile). The stock-unix tier shares the host kernel; hostile tenancy requires the microVM kernel boundary. Use substrate.tier 'kata-hybrid' (the default for this namespace) or 'sidecar-emptydir' only where Kata is unavailable, with an explicit isolation acknowledgement. See agentctl RFC 0002 §5."
    }
  }
}
```

### 5.2 Trifecta override — required-and-granted admit (B5), and the reject variant

```jsonc
// → REQUEST: an Agent that turns OFF the per-spawn guard (allowTrifecta:true), WITH the
//    override annotation; userInfo is a principal that holds the override-trifecta grant.
{
  "apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
  "request": {
    "uid": "f10a-…",
    "resource": { "group": "agents.x-k8s.io", "version": "v1alpha1", "resource": "agents" },
    "namespace": "tenant-acme", "operation": "CREATE",
    "userInfo": { "username": "alice", "groups": ["agent-admins"] },
    "object": {
      "kind": "Agent",
      "metadata": {
        "name": "single-pass-trifecta", "namespace": "tenant-acme",
        "annotations": { "agents.x-k8s.io/allow-trifecta-override": "JIRA-4412: legacy single-agent migration; reader/actor split planned Q3" }
      },
      "spec": {
        "mode": "reactive", "substrate": { "tier": "kata-hybrid" },
        "security": { "allowTrifecta": true },
        "mcp": { "servers": [ { "name": "web", "tags": ["untrusted_input"] },
                              { "name": "crm", "tags": ["sensitive"] },
                              { "name": "mailer", "tags": ["egress"] } ] }
      }
    }
  }
}
```
```jsonc
// ← RESPONSE: gate satisfied — annotation present AND SubjectAccessReview(override-trifecta)
//    for alice succeeded. ADMIT + warning; the webhook sets AdmissionResponse.auditAnnotations
//    (mgmt.invoked-class keys) which the APISERVER writes to its audit log — dry-run-aware,
//    no out-of-band write, sideEffects:None honest (§3.3 / §4.6).
{
  "apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
  "response": {
    "uid": "f10a-…",
    "allowed": true,
    "warnings": [
      "trifecta override ACTIVE: this Agent disables the per-spawn Rule-of-Two guard (allowTrifecta=true), permitting a SINGLE subagent to hold untrusted_input + sensitive + egress. Override authorized for user 'alice' via annotation 'JIRA-4412…' and the override-trifecta grant; audited. The reader/actor split (agentd RFC 0012 §3.3) remains the safe alternative."
    ]
  }
}
```
```jsonc
// ← RESPONSE (variant): SAME spec but the annotation is ABSENT (or alice lacks the grant) → REJECT
{
  "apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
  "response": {
    "uid": "f10a-…",
    "allowed": false,
    "status": {
      "code": 403, "reason": "Forbidden",
      "message": "TrifectaOverrideUngated: security.allowTrifecta=true disables the per-spawn lethal-trifecta guard and requires BOTH the annotation 'agents.x-k8s.io/allow-trifecta-override' (a justification) AND the 'override-trifecta' RBAC grant for the requesting user. Missing: override-trifecta authorization for 'alice'. Prefer the reader/actor split (a no-sensitive/no-egress reader returning a distilled summary to a no-untrusted-input actor) which needs NO override — see agentd RFC 0012 §3.2/§3.3."
    }
  }
}
```

Note the contrast with a **non-override** trifecta union (§3.4): an `Agent` with the same
three servers but `allowTrifecta:false` is **admitted with a warning**, never rejected —
because the safe reader/actor split must pass.

---

## 6. Non-goals

- **The CEL rule set (rung A).** Defined on the CRD by agentctl RFC 0003 §7; this RFC fixes
  only its place in the ladder and the no-duplication rule.
- **The renderer, the capability cache / `CapabilityProbe`, the init-container injection,
  the single-`.status`-writer discipline.** All agentctl RFC 0006. This RFC consumes the
  cached config schema and specifies *what* the init-container runs, not *how* it is rendered.
- **The substrate tiers and the tenancy×substrate *rule* itself.** Defined by agentctl RFC
  0002 §5; this RFC enforces it at admission (B3), it does not define it.
- **The `AgentClass`/`MCPServerSet`/`IntelligenceService` schemas.** agentctl RFC 0004; this
  RFC dereferences them (B1/B2/B4).
- **The webhook HTTP/TLS server and leader election.** agentctl RFC 0001 §3 (axum/hyper + the
  cert-manager-absent fallback this RFC's §4.4 wires).
- **A mutating webhook / defaulting.** Out of scope by design (§2.5) — defaulting is the
  renderer + CRD `default`s; this RFC is validation-only so `sideEffects: None` holds.
- **The management-access RBAC for *runtime* verbs** (`drain`/`cancel`/`attach`). That is
  agentctl RFC 0009 (the aggregated-APIServer / `pods/proxy` path); the only RBAC this RFC
  touches is the admission-time `override-trifecta` `SubjectAccessReview` (§3.3).
- **Executing or linking any data-plane binary in the webhook (P0).** Forbidden by §2.3; the
  exact-image authority lives in the init-container (rung D), never the control plane.
- **Conversion / version-skew of the CRD itself.** agentctl RFC 0005 (single served version +
  conversion webhook + SVM) — distinct from the *contract*-version skew rung C/D handle.

---

## 7. Open questions

1. **GitOps co-apply ordering for B4.** A tenant bundle that applies an `Agent` and its
   `MCPServerSet` together can present the `Agent` before the set is in cache, yielding a
   false `ReferenceUnresolved`. Recommended: document "apply ops objects first," and support
   a server-side-dry-run two-pass retry; consider a short bounded re-check window before the
   hard reject. Which mitigation is default?
2. **Webhook contract-major skew policy.** When the cached config schema's major and the
   negotiated instance major disagree (e.g. an `AgentClass` image pinned to a major agentctl's
   cached schema predates), does the webhook reject (`ContractMajorUnknown`) or admit-and-defer
   to rung D? Leaning **admit-and-defer** for an *unknown-newer* major (rung D is authoritative)
   and **reject** for a *removed-older* major. Confirm against agentd RFC 0014 §6.3.
3. **`override-trifecta` verb modelling.** Is the `SubjectAccessReview` issued against a real
   subresource (`agents/allow-trifecta`, which needs the aggregated APIServer of agentctl RFC
   0009 to be *callable*, though RBAC string-matching works without it) or a synthetic
   non-resource verb? The brainstorm notes CRD subresources cannot host connect verbs (D5);
   confirm the synthetic-verb SAR is acceptable as the gate.
4. **Where the namespace-tenancy *label* authoritatively lives** (not the formula — the
   formula is settled). The effective tenancy is `max(namespace label,
   AgentClass.spec.substrate.tenancy)` per agentctl RFC 0004 §3.3 (the source of truth;
   the `AgentClass` term and the namespace label are **both** inputs, not competitors).
   The open part is only *who owns the namespace label*: the platform (namespace labels,
   as assumed here) vs a cluster-scoped policy object if tenants own their own namespaces.
   Reconcile the label authority with agentctl RFC 0004 OQ4 / RFC 0015.
5. **`failurePolicy: Ignore` for single-tenant.** §4.2 permits `Ignore` on dev/single-tenant
   clusters. Should agentctl ship two `ValidatingWebhookConfiguration` profiles (hostile=Fail,
   single=Ignore) selected at install, or always `Fail` with the control-plane exemptions as
   the only relaxation?
6. **Optional async `--config-only` pre-flight (P6).** Is the richer-than-schema, env-less
   `--validate-config --config-only` probe (§2.4) worth shipping as a `CapabilityProbe`-adjacent
   Job to close rung C's semantic gap pre-schedule, or is rung D's runtime authority sufficient?
7. **`apiGroup` string.** Inherits agentctl RFC 0003 §13 open question (`agents.x-k8s.io` vs
   `agentctl.dev`); the webhook `rules`/`matchConditions`/annotations key off whichever wins.

---

## 8. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo decision record: §3 the webhook serving stack
  (axum/hyper TLS, decode `AdmissionReview` by hand) and the **cert-manager-default +
  cert-manager-absent fallback** cert path this RFC's §4.4 wires; §4 the contract-as-schema
  (P0) anti-drift the no-exec/no-link config check (§2.3) depends on.
- **agentctl RFC 0002** — substrate & transport abstraction: §5 the **tenancy×substrate
  binding rule** the webhook enforces (B3); the tier vocabulary
  (`stock-unix`/`kata-hybrid`/`sidecar-emptydir`).
- **agentctl RFC 0003** — Agent & AgentFleet CRDs: §7 the CEL-vs-webhook split (rung A; the
  three CEL-impossible checks this RFC owns); §6.2 the conditions taxonomy
  (`Validated`/`ConfigSchemaValid`/`ConfigSchemaDeferred`, `Degraded`/
  `ConfigValidatedAtRuntimeFailed`, `TrifectaUnionObserved`, `A2AUnsupported`); §3.3 the
  `surfaces.a2a` (P2) gating; §5.1 the `RollingUpdate`-without-claim warning; §9.1 the
  `AGENT_SHARD` (P3) env the init-container resolves.
- **agentctl RFC 0004** — `AgentClass`/`IntelligenceService`/`MCPServerSet`: the referenced
  objects B1/B2/B4 dereference; the source of the operator-declared MCP tags.
- **agentctl RFC 0005** — CRD versioning & conversion: the CRD-`apiVersion` skew (distinct
  from the contract-version skew this RFC's rung C/D handle).
- **agentctl RFC 0006** — operator reconcile & manifest-driven capability model: the
  **renderer** (init-container injection, ConfigMap partitioning), the **`CapabilityProbe`
  capability/config-schema cache** rung C reads, and the single-`.status`-writer that mirrors
  the webhook's warnings into conditions.
- **agentctl RFC 0009** — management access path & RBAC: the runtime-verb authorization
  (distinct from the admission-time `override-trifecta` SAR, §3.3) and the aggregated-APIServer
  question behind open question 3.
- **agentctl RFC 0015** — security & multi-tenancy: the management-action audit vocabulary the
  override-admit event (§3.3) joins.

**Contract spec (the reference implementation, agentd RFCs)**

- **agentd RFC 0014 (the reference impl's contract spec)** — contract-version negotiation
  (§6.3, the major the config schema is keyed to), the `surfaces{}` single discovery point
  (§6.2, graceful degradation off `config_schema`/`config_validate`), the downward-API env
  convention (§6.4, the env layer rung D resolves; the `AGENT_SHARD` defect).
- **agentd RFC 0017 (the reference impl's contract spec)** — `--validate-config` (§4.1, rung D;
  exit 0/2 with structured diagnostics) and `--config-schema` (§4.2, the published JSON Schema
  rung C validates against) — **both unbuilt today (contract ask P6)**; the config-file shape
  the operator renders.
- **agentd RFC 0012 (the reference impl's contract spec)** — the trifecta tag vocabulary
  (§3.1), the **per-spawn Rule-of-Two chokepoint** that makes a blocking Agent-level union
  wrong (§3.2), the reader/actor distillate firewall (§3.3), the process-global
  `--allow-trifecta` blast radius and the deferred per-route override (§6) — the basis of §3.
- **agentd RFC 0011 (the reference impl's contract spec)** — validate-at-startup → **exit 2**
  (§3.3) that is rung C's degradation backstop until P6, and the exit-code contract.
- **agentd RFC 0015 (the reference impl's contract spec)** — reachability == operator authority
  (§7), the management audit vocabulary the override-admit event references.

**Contract asks raised or cited by this RFC** (brainstorm §14): **P6** (`--config-schema` +
`--validate-config`/`--config-only` — gates rungs C and D), **P2** (`surfaces.a2a` — the B6
`A2AUnsupported` warning), **P3** (`--shard auto/N` — the `AGENT_SHARD` env rung D resolves),
**P-audit** (the closed-vocabulary `mgmt.invoked` event the override-admit emits, §3.3).

*Where this RFC and a contract spec disagree on the wire, the contract wins and this RFC is
corrected; where this RFC needs a primitive the contract does not yet expose (a published
config schema, an exec-safe validate mode), it is a contract ask — never a leak of cluster
logic into the agent, and never a data-plane binary linked or exec'd in the control plane.*
