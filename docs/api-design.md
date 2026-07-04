# CRD & API design notes

A running design review of the agentctl Custom Resources (`agentctl.dev/v1alpha1`:
`Agent`, `AgentFleet`, `ModelPool`, `MCPServerSet`). It records what has been
applied and the recommendations still open, so the API can be tightened while it
is still `v1alpha1` and pre-adoption (breaking changes are cheap now, expensive
later).

The guiding principles:

- **One obvious place for each thing** — no near-duplicate fields.
- **Name a binding after the kind it references** — `model.pool` ↔ `ModelPool`,
  `mcpServers` ↔ `MCPServerSet`.
- **Bare names for same-namespace references** (strings); objects only when a
  reference needs more than a name.
- **Ship only wired fields** — no phantom references or documentation-only flags
  in a served CRD; if a field is declared-but-enforced-elsewhere, say so in its
  doc.
- **Group fields evaluated together** — the lethal trifecta, the work fabric,
  the model binding.
- **Durations are Go-duration strings**; be **safe-by-default**; make requiredness
  a **CEL invariant**, not just prose.
- **Consistent observability** (`conditions` + a `Ready` column) across all kinds.

---

## Applied

Two passes of breaking, pre-adoption changes that remove dead surface, group
fields by concern, and make documented invariants machine-checked.

### Pass 1 — dead-surface removal + tool-binding flatten

| Change | What & why |
| --- | --- |
| **Remove `spec.intelligenceRef`** | Vestigial: a `LocalRef` to a non-existent `IntelligenceService`, consumed nowhere. The real binding is `spec.model.pool`. |
| **Remove `spec.classRef` + the `LocalRef` type** | Vestigial: there is no `AgentClass` CRD and no resolver. `LocalRef` had no remaining users after the `mcpServers` flatten. |
| **Remove `spec.limits.treeTokenBudget`** | Declared but **never emitted** to the agent. A silent no-op field is a versioning liability. (Unrelated to the agent's *reported* `tree_token_budget` in the capabilities manifest, which stays — that is the contract, not the CRD.) |
| **`spec.mcpServerSetRefs` → `spec.mcpServers`** | Was `[]LocalRef{name}`; now `[]string` — a bare list of `MCPServerSet` names. |

### Pass 2 — grouping, latent-bug fixes, observability

| Change | What & why |
| --- | --- |
| **Group the model binding → `spec.model: { pool, id }`** | `spec.model` (a decorative id string) and `spec.modelPool` (the real binding) were two look-alike top-level keys. `pool` names the `ModelPool` kind; `id` is the model within it. Ends the `model`/`modelPool` confusion instead of renaming around it. |
| **Group the lethal trifecta → `spec.capabilities: { exec, egress, secrets }`** | The three grants the admission gate evaluates **as a union** now read as one reviewable block (mirrors k8s `securityContext`). Each doc says **"declared intent, enforced at admission only"** — the operator wires none of them downstream, so the honesty gap is closed. |
| **Group the fleet work fabric → `spec.work: { source, maxAttempts, claimTtl }`** | Replaces `workSource` + `workPolicy{maxAttempts, claimTtlMs}`. `claimTtlMs` (`u64` ms) → `claimTtl` (Go-duration string, e.g. `"30s"`), matching `loop.interval`. `maxAttempts`/`claimTtl` were previously **unwired**; the operator now delivers them to the coordinator (`AGENT_FLEET_MAX_ATTEMPTS` / `AGENT_FLEET_CLAIM_TTL`, alongside `AGENT_FLEET_WORKSOURCE`), so a conformant coordinator can stamp them onto each `work.submit`. |
| **Scaling renames** | `scaling.min`/`scaling.max` → `minReplicas`/`maxReplicas`; `scaling.target.signal` → `scaling.target.metric`. |
| **`substrate` honesty** | The enum keeps all three tiers (`stock-unix` is the locked direction alongside Kata), but the field/variant docs now say plainly that **only `stock-unix` is rendered today** — `kata-hybrid`/`sidecar-emptydir` are declared roadmap tiers, rejected at render. The misleading "hostile tenancy forces `kata-hybrid`" default story is gone. |
| **CEL invariants for requiredness** | Two rules added to `Agent`/`template`: `instruction` is required for `once`/`loop`/`schedule`; a `reactive` agent must carry a wake source — `subscribe`, a `workflow`, or `surfaces.a2a` (the last covers an A2A-driven coordinator that has neither `subscribe` nor `workflow`). |
| **Remove `access.public`** | An unenforced `public: true` that gated nothing (a safe-by-default footgun). Real exposure is governed by `surfaces.a2a` + `access.oidc`. |
| **`ModelPool` / `MCPServerSet` status `conditions` + `Ready` column** | All four kinds now share one health idiom (a `conditions` array + a `Ready` printer column). |
| **Agent printer columns point at the binding** | The default-wide columns are now `Pool` (`.spec.model.pool`, the real binding) and `Model` (`.spec.model.id`), instead of the single decorative `.spec.model`. |
| **Shared `agentctl` category on all four kinds** | `kubectl get agentctl` lists Agents/AgentFleets/ModelPools/MCPServerSets together (as cert-manager does with its `cert-manager` category). |

**Illustrative target `Agent.spec`, grouped by concern:**

```yaml
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0   # optional — operator default fills it
  instruction: "…"
  model: { pool: gpt, id: gpt-4o-mini }    # the binding + the model id
  mcpServers: [tools]                       # MCPServerSet names
  subscribe: ["queue://jobs"]               # a wake source (CEL-required for reactive)
  surfaces: { a2a: true, metrics: true }
  limits: { maxTokens: 20000 }
  capabilities: { exec: false, egress: true, secrets: [db-creds] }  # trifecta, admission-gated
  access: { oidc: { … } }
```

---

## Still open

Deliberately deferred — either a bigger restructure with modest marginal value,
or a judgment call the current flat form already serves well.

- **Per-mode scaling sub-blocks.** `scaling: { mode, claim: {minReplicas,
  maxReplicas, target}, shard: {count} }` would make it structurally impossible to
  set a claim-only field on a shard fleet (today only a CEL rule could). The flat
  form + the `shards`-requiredness CEL rule works; deferred as a larger change.
- **Trifecta end-to-end wiring.** `capabilities.exec`/`egress`/`secrets` gate
  admission but drive nothing in the operator (no `Secret` mounts, no egress
  `NetworkPolicy` from the field, no `exec` flag). Now documented as
  "admission only". Wiring them end-to-end (mount the named `Secret`s, derive the
  egress policy, pass `exec`) remains the real fix and is the open item.
- **Cosmetic Rust-type renames** (wire keys unchanged): `DesiredSurfaces` →
  `Surfaces`, `LoopParams` → `Loop`. Pure internal tidy; deferred.
- **Discriminated `trigger`/`run` union** keyed by `mode` (folding `subscribe` /
  `loop` / `schedule` / `workflow`). The flat form is readable, so this is a
  judgment call, not a fix.
- **CRD-level `default:` values** (via `schemars`) so the apiserver applies static
  safe defaults deterministically, rather than defaults living only in the
  operator/renderer.
- **`work.source` vs `template.subscribe` overlap.** Every claim-fleet example
  still writes the same queue URI twice. Default the worker `subscribe` from
  `work.source`, or keep documenting that they are normally equal.
- **`replicas` vs `scaling.minReplicas`/`maxReplicas`.** `.spec.replicas` must
  stay top-level (it is the scale-subresource path); documented that KEDA owns it
  in steady state for claim fleets so the precedence is clear.
