# CRD & API design notes

A running design review of the agentctl Custom Resources (`agentctl.dev/v1alpha1`:
`Agent`, `AgentFleet`, `ModelPool`, `MCPServerSet`). It records what has been
applied and the recommendations still open, so the API can be tightened while it
is still `v1alpha1` and pre-adoption (breaking changes are cheap now, expensive
later).

The guiding principles:

- **One obvious place for each thing** — no near-duplicate fields.
- **Name a binding after the kind it references** — `modelPool` ↔ `ModelPool`,
  `mcpServers` ↔ `MCPServerSet`.
- **Bare names for same-namespace references** (strings); objects only when a
  reference needs more than a name.
- **Ship only wired fields** — no phantom references or documentation-only flags
  in a served CRD.
- **Group fields evaluated together** — the lethal trifecta, the work fabric,
  per-mode config.
- **Durations are Go-duration strings**; be **safe-by-default**; make requiredness
  a **CEL invariant**, not just prose.
- **Consistent observability** (`conditions` + a `Ready` column) across all kinds.

---

## Applied (this pass)

A consistency pass that removes dead surface and simplifies the tool binding.
All are **breaking** CRD changes, made deliberately pre-adoption.

| Change | What & why |
| --- | --- |
| **Remove `spec.intelligenceRef`** | Vestigial: a `LocalRef` to a non-existent `IntelligenceService`, consumed nowhere. The real, admission-validated intelligence binding is `spec.modelPool`. |
| **Remove `spec.classRef` + the `LocalRef` type** | Vestigial: there is no `AgentClass` CRD and no resolver — `resolve_image` never consulted it; the only reader was a cosmetic `Class:` line in `kubectl agent describe`. `LocalRef` had no remaining users after this + the `mcpServers` flatten. |
| **Remove `spec.limits.treeTokenBudget`** | Declared but **never emitted** to the agent (`render.rs`: "no agentd flag yet → not emitted"). A silent no-op field is a versioning liability. (Unrelated to the agent's *reported* `tree_token_budget` in the capabilities manifest, which stays — that is the contract, not the CRD.) |
| **`spec.mcpServerSetRefs` → `spec.mcpServers`** | Was `[]LocalRef{name}`; now `[]string`. Drops the redundant `Refs` suffix and the `{ name: … }` wrapper — a bare list of `MCPServerSet` names, parallel to `modelPool`. |

---

## Recommended — naming & structure

Breaking, and best done together (pre-adoption). These were the strong consensus
of an independent naming-scheme panel.

- **Group the model binding.** `spec.model` (a decorative model-id string) and
  `spec.modelPool` (the real binding) are two look-alike top-level keys one field
  apart. Fold them into `spec.model: { pool, id }` — `pool` names the `ModelPool`
  kind, `id` is the model chosen within it. (You earlier floated
  `modelPool → intelligence`; `modelPool` was kept this pass per your call. The
  `model: {pool, id}` grouping is the cleaner long-term fix — it *ends* the
  `model`/`modelPool` confusion rather than renaming around it.)
- **Group the lethal trifecta.** `spec.exec` / `spec.egress` / `spec.secrets` are
  three scattered top-level flags that the admission gate evaluates **as a union**.
  Move them into `spec.capabilities: { exec, egress, secrets }` so the privileged
  grants read as one reviewable, gated block (mirrors k8s `securityContext`).
- **Group the fleet work fabric.** `AgentFleet.spec.workSource` +
  `spec.workPolicy{maxAttempts,claimTtlMs}` → `spec.work: { source, maxAttempts,
  claimTtl }` — one work section instead of two same-prefixed top-level fields.
- **Durations as Go-duration strings.** `workPolicy.claimTtlMs` (`u64` ms) →
  `claimTtl: "30s"`, matching `loop.interval` / `loop.deadline`.
- **Per-mode scaling sub-blocks.** `scaling: { mode, claim: {min, max, target},
  shard: {count} }` so mode-only fields can't be set for the wrong mode (today only
  a CEL rule could catch `shards` set on a claim fleet). Rename `scaling.min/max` →
  `minReplicas/maxReplicas` and `scaling.target.signal` → `metric`.
- **Cosmetic Rust-type tidies** (wire keys unchanged): `DesiredSurfaces` →
  `Surfaces`, `LoopParams` → `Loop`.
- **(Optional, low priority)** Fold the per-mode inputs (`subscribe`, `loop`,
  `schedule`, `workflow`) under a discriminated `trigger`/`run` object keyed by
  `mode`. The current flat form is readable, so this is a judgment call, not a fix.

**Illustrative target `Agent.spec`, grouped by concern:**

```yaml
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0   # optional — operator default fills it
  instruction: "…"
  model: { pool: gpt, id: gpt-4o-mini }    # was modelPool + model
  mcpServers: [tools]                       # was mcpServerSetRefs: [{name: tools}]
  subscribe: ["queue://jobs"]
  surfaces: { a2a: true, metrics: true }
  limits: { maxTokens: 20000 }
  capabilities: { exec: false, egress: true, secrets: [db-creds] }  # the trifecta, grouped
  access: { oidc: { … } }
```

---

## Recommended — validation gaps & latent bugs

These are correctness issues the review surfaced; worth addressing regardless of
any renaming.

- **`substrate` default is inverted and advertises unrenderable tiers.** The
  `Substrate` enum offers `stock-unix` / `kata-hybrid` / `sidecar-emptydir`, but the
  renderer implements **only `stock-unix`** (`require_stock_unix` rejects the
  others) — while the field doc says hostile tenancy *forces* `kata-hybrid`. So the
  hardened "default" would fail to render. Restrict the enum to what is implemented
  (mark `kata`/`sidecar` as roadmap), or implement them, and fix the default story.
  **Potential bug.**
- **`instruction` / `subscribe` requiredness is documented but unenforced.** The
  docs promise "instruction required for non-reactive modes" and "subscribe required
  for reactive", but the only CEL rules cover `schedule`/`workflow`. Add CEL
  invariants: a non-reactive, non-workflow mode requires `instruction`; a reactive
  mode requires `subscribe` **or** `workflow` (a reactive-daemon workflow is valid).
- **The trifecta fields gate admission but wire nothing downstream.** `exec` /
  `egress` / `secrets` are validated by the admission webhook, but the operator
  never mounts the named `Secret`s, drives the egress `NetworkPolicy` from
  `spec.egress`, or passes `exec` to the agent. Either wire them end-to-end or mark
  each docstring "declared intent; enforced at admission only."

---

## Recommended — consistency & ergonomics

Mostly additive, non-breaking.

- **`access.public` is unenforced** — a `public: true` that gates nothing (a
  safe-by-default footgun). Wire it (gate A2A ingress, paired with `surfaces.a2a`)
  or remove it; real exposure is already governed by `surfaces.a2a` + `access.oidc`.
- **`ModelPool` and `MCPServerSet` status lack `conditions`.** `Agent`/`AgentFleet`
  carry a `conditions` array + `Ready`; the other two do not. Add `conditions`
  (reuse the `Condition` type) + a `Ready` printer column so all four kinds share
  one health idiom.
- **The Agent `Model` printer column surfaces the decorative `spec.model`, not the
  real `spec.modelPool` binding.** Point it at the binding, or add a second column.
- **Add a shared `agentctl` category to all four CRDs** so `kubectl get agentctl`
  lists Agents/AgentFleets/ModelPools/MCPServerSets together (cert-manager does this
  with its `cert-manager` category). Additive.
- **Consider CRD-level `default:` values** (via `schemars`) so the apiserver applies
  static safe defaults deterministically, rather than defaults living only in the
  operator/renderer.
- **`workSource` vs `template.subscribe` overlap.** Every fleet example writes the
  same queue URI twice. Default the worker `subscribe` from `workSource`, or
  document that they are normally equal.
- **`replicas` vs `scaling.min/max`.** `.spec.replicas` must stay top-level (it is
  the scale-subresource path), but document that KEDA owns it in steady state for
  claim fleets so the precedence is clear.
