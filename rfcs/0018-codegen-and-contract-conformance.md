# agentctl RFC 0018: Codegen & contract conformance — the three sources of truth, the generated client, and the behavioral suite that proves any agent

**Status:** Proposed (agentctl codegen & conformance track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the detailed specification of the anti-drift machinery agentctl RFC 0001 §4 fixes in principle; it closes the P0 loop by making "couple to the contract, generate your own client, prove any agent behaviorally" a concrete, CI-enforced pipeline

> **This RFC is the detailed spec of agentctl RFC 0001 §4.** RFC 0001 decided the
> *principle* — agentctl couples to a published, language-neutral **contract**,
> generates its **own** client from it (`agent-contract-client`), and proves
> **any** conformant agent against it with a **black-box behavioral conformance
> suite** that never links an agent library. This RFC specifies that machinery
> end-to-end: the three sources of truth and how each is produced, the codegen
> pipeline and the pinning identity, the conformance suite's exact assertions and
> the new-vendor self-certification path, and the E2E + chaos test strategy. Where
> RFC 0001 says "this exists and lives here," this RFC says "here is how it is
> built, pinned, negotiated, and gated."

> **P0 (locked 2026-06-27).** The data plane is *any* agent that conforms to the
> contract; the reference implementation (whose contract is currently authored in
> agentd RFCs 0014–0020) is the **first** conformant agent, **not a dependency**.
> Every mechanism here is therefore vendor- and language-neutral by construction:
> the codegen input is *schemas*, never an agent crate; the conformance suite
> drives a *binary*, never a linked library; and the pinning identity is a
> *contract version*, never an agent SHA. Naming the reference impl's branded
> surfaces (`--capabilities`, `agent://`, the `agent_` metric prefix, `AGENT_*`
> env, the `a2a.*` strings) is **citing where the contract is presently written
> down**, flagged for neutralization (§5.4), never importing it.

> **agentctl does not author the contract; it consumes it.** The schemas' *content*
> is owned by the contract (the reference impl's agentd RFCs 0014–0020, eventually
> a neutral spec — §5). This RFC owns the *consumption pipeline*: how those schemas
> become a typed client, how an agent is proven to honour them, and how drift is
> caught before it reaches production. The single open contract precondition (CC —
> the schemas must actually be *published*; today the reference manifest is
> `json!`→`Value` and `--config-schema` is unbuilt) is named honestly throughout
> as contract work (agent asks **P6 / P3b**), not agentctl-coupling work.

---

## 1. Problem / Context

agentctl RFC 0001 §4 made the load-bearing call that replaces the void
"shared-wire-crate" argument: under P0, agentctl MUST NOT import a crate from a
specific agent binary (it would weld agentctl to one vendor and make a
second-vendor conformant agent unmanageable), and the shared crate would be
*illusory* anyway (the reference manifest is built `serde_json::json!` → `Value`
by deliberate secret-safety design — the `Secret` newtype has no `Serialize`, so
it cannot reach the builder; agentd RFC 0012 §3.7). RFC 0001 §4 therefore fixed
the anti-drift mechanism as **conformance to a published, language-neutral
contract + a behavioral conformance suite**, drew the diagram, and named the
crates — but explicitly deferred the *mechanics* here (RFC 0001 §7 Non-goals,
RFC 0001 §10 → "agentctl RFC 0018 owns the §4 pipeline and the CC schema corpus").

This RFC is that spec. The problem it solves, stated as the three obligations P0
imposes:

1. **agentctl must couple to a *published* contract, not to an agent.** The
   contract must exist as machine-readable artifacts agentctl can read as *data*
   — a manifest schema, a config schema, a metrics/exit-code registry, a report
   schema, an A2A method set. That corpus is the **first source of truth**.
2. **agentctl must generate its *own* typed client** from that corpus — the
   single home of the wire shape inside the workspace
   (`crates/contract-client` = `agent-contract-client`), depending on schemas and
   nothing else. That generated crate is the **second source of truth** for every
   one of the five components.
3. **agentctl must prove *any* agent behaviorally** — a black-box conformance
   suite that drives a real agent binary over the substrate transport and asserts
   the contract holds, mirroring the reference impl's own `agent-conformance`
   crate, which is black-box *by charter* and "links nothing." That suite is the
   **third source of truth** — the executable definition of "a conformant agent,"
   and the operational meaning of P0.

The honest difficulty — the reason this is a real RFC and not a paragraph — is
that **the contract is not yet published in the form this pipeline needs**, and
several of its surfaces are genuinely hostile to naive codegen:

- **The reference manifest is `json!` → `Value`, not a `derive(Serialize)`
  struct** (agentd RFC 0012 §3.7; verified in `crates/agent/src/capabilities.rs`).
  There is no schema document to consume, and the no-struct property is a
  *security invariant*, not laziness. So the manifest schema must be **hand-authored**
  until the contract ships an emitter, with conformance as the enforcement.
- **`--config-schema` / `--validate-config` are unbuilt** in the reference impl
  (agentd RFC 0017 §4.1/§4.2 specifies them; they do not yet exist). Config-struct
  codegen and the admission ladder (agentctl RFC 0007) are both blocked on them
  (contract ask **P6**).
- **The `surfaces{}` block is JSON sum types** (`bool | string`, `bool | object`
  — `"management": false | "vsock:PORT" | "unix:PATH"`; agentd RFC 0014 §5,
  §6.2) that defeat typed codegen in *any* language and need **hand-written
  deserializers** scoped to those fields (RFC 0001 §2.4).
- **The contract is additive-by-minor by design** (agentd RFC 0014 §6.3):
  consumers MUST tolerate unknown additive fields/tools/metrics and refuse only on
  unknown **major**. A generated type with `deny_unknown_fields` would *break*
  negotiation; a lenient one dissolves the static "cannot drift" guarantee — so
  conformance must catch *regressive* drift and an additive-drift report must
  surface *additive* drift (§6.4).
- **The autoscaling metric names are unreconciled** (agentd RFC 0016 frozen set vs
  RFC 0019 scaling signals — e.g. `agent_reactive_backlog` is cited by the
  scaling RFC but absent from the frozen schema; contract ask **P10**). Hand-
  transcribing them is "exactly the failure codegen is meant to prevent"
  (brainstorm §11.2) — so the metrics registry is **derived by scraping a real
  binary**, never hand-typed.
- **The A2A wire strings are uncommitted** (the reference impl cites A2A 0.2.x
  spellings `SendMessage`/`GetTask`/…; A2A v1.0 renames them
  `message/send`/`tasks/get`/…; and `surfaces.a2a` is absent from the frozen
  manifest; contract ask **P2**). The A2A method-name constants cannot be
  generated faithfully until P2 lands.
- **No versioned golden corpus of `--capabilities` outputs is published** (contract
  ask **P3b**) — yet the conformance suite and codegen regression both need a
  per-feature-set golden corpus keyed by `(contract major.minor + digest)`.

This RFC specifies the pipeline *as it must be built today* (hand-authored schemas
where the contract has not yet published them, scrape-derived registries where
hand-transcription is the hazard, behavioral conformance as the safety net for
both) **and** the migration path to the published-and-extracted end state (§5).
It is consistent with RFC 0001 (the contract-as-schema principle, the generated
client, the conformance suite, the `surfaces{}` sum types needing hand-written
deserializers), with the contract asks **CC / P6** (published schemas +
`--config-schema`/`--validate-config`) and **P3b** (versioned `--capabilities`
golden corpus), with the reference impl's version-negotiation spine (agent
RFC 0014 §6.3), and with `agent-conformance` as the blessed black-box pattern.

---

## 2. Decision — the three sources of truth

agentctl's anti-drift machinery is exactly three artifacts plus the runtime
discipline that binds them. This is RFC 0001 §4 made concrete; the lettering
`(a)/(b)/(c)/(d)` is preserved so the two RFCs index identically.

```
   (a) THE CONTRACT  —  language-neutral, machine-readable schemas (this RFC §3)
   ┌──────────────────────────────────────────────────────────────────────────┐
   │  manifest.schema.json        config.schema.json        report.schema.json  │
   │  (RFC 0014 §5 / 0015 §5.2)    (RFC 0017 §4.2, P6)       (RFC 0016 §6, P6)   │
   │  metrics.registry.json        a2a.methods.json          exit-codes table    │
   │  (RFC 0016 §4, scrape/P10)    (RFC 0020, PENDING/P2)    (RFC 0016 §5)       │
   │            pinned by (contract major.minor + schema digest)  (§6.1)         │
   └───────────────┬──────────────────────────────────────────┬─────────────────┘
   codegen (xtask)  │ consumed as DATA — never a linked binary  │ validated against
                    ▼                                            ▼
   ┌─────────────────────────────────┐        ┌────────────────────────────────────┐
   │ (b) crates/contract-client       │        │ (c) crates/conformance (black-box)  │
   │ `agent-contract-client`          │        │ drives ANY conformant agent binary  │
   │ GENERATED Rust types +           │        │ over the substrate transport;       │
   │ hand-written surfaces{} deserders│        │ asserts the contract BEHAVIORALLY;  │
   │ links NO agent  (§4)             │        │ mirrors agent-conformance  (§7)    │
   └───────────────┬─────────────────┘        └────────────────┬───────────────────┘
                   │ the SOLE wire-type home; used by all 5      │ first subject =
                   ▼ components                                  ▼ the reference agent
            operator · node-agent · gateway · cli · scaler       (one of many)
                   └──── (d) negotiate on contract_version + degrade off surfaces{} (§6) ────►
```

| Field | Value |
|---|---|
| **Decision** | Anti-drift is three artifacts — **(a) published neutral schemas**, **(b) a generated client crate `agent-contract-client`**, **(c) a black-box behavioral conformance suite** — bound by **(d) runtime version-negotiation + graceful degradation**, all pinned by **(contract major.minor + schema digest)**. |
| **Status** | Accepted (the detailed spec of RFC 0001 §4; brainstorm §11, §0.6 P0/CC). |
| **Source of truth precedence** | The **schemas (a)** are authoritative for *shape*; the **conformance suite (c)** is authoritative for *behaviour*. The generated client **(b)** is a derived artifact of (a) and is never hand-edited (except the scoped sum-type deserializers, §4.2). On conflict, a *behavioral* failure (c) outranks a *shape* match (a) — a binary that parses but misbehaves is non-conformant. |
| **Rejected alternatives** | A shared wire crate (forbidden by P0 + illusory; RFC 0001 §2.4); hand-maintained client types (drift-prone, the exact failure this prevents); schema-validation-only with no behavioral suite (catches shape but not the `drain ≡ SIGTERM ≡ exit 0` class of contract — agentd RFC 0015 §4.1); a single monolithic "contract crate" the agent also imports (re-creates the forbidden coupling). |
| **Consequences** | agentctl owns the codegen pipeline (`xtask`), the conformance suite, the pinning discipline, the CI gating, and — jointly with RFC 0001 §9 — the contract-extraction + brand-neutralization plan (§5). It depends on the CC precondition (P6/P3b) being real contract work. |

The three artifacts and their per-surface production strategy are §3 (a), §4 (b),
§7 (c); the binding discipline (d) — pinning + negotiation + the additive-drift
report — is §6; the extraction realization is §5; the E2E + chaos strategy that
exercises the whole thing on a real cluster is §8.

---

## 3. Source of truth (a) — the neutral, versioned contract schemas

The contract is consumed as **data**. The corpus lives (interim) in `contract/`
in the workspace (RFC 0001 §5), vendored and pinned by `(contract major.minor +
schema digest)`, until neutral extraction (§5). Each member has a *production
strategy* chosen by how the reference impl exposes the surface today — and each
strategy is honest about whether the artifact is **published**, **hand-authored**,
**scrape-derived**, or **pending freeze**.

> Do not conflate "the three sources of truth" (§2: schemas / client / suite)
> with "the per-surface codegen lanes" below. The former is the anti-drift
> architecture; the latter is *how source-of-truth (a) is produced surface by
> surface*. The brainstorm's "codegen as three tiers" (§11.1) is this second
> decomposition; it is refined here into four lanes.

### 3.1 The schema corpus and where each surface is presently specified

| Artifact (`contract/…`) | Surface | Presently specified in (reference impl) | Production strategy | State |
|---|---|---|---|---|
| `manifest.schema.json` | capabilities manifest + `surfaces{}` + identity + limits | agentd RFC 0014 §5, RFC 0015 §5.2 | **hand-authored** (manifest is `json!`→`Value`, no emitter) | interim; conformance-enforced |
| `config.schema.json` | the config file the operator renders to a ConfigMap | agentd RFC 0017 §4.2 (`--config-schema`) | **emitter** (`--config-schema` → `typify`) | **PENDING (P6)** — emitter unbuilt |
| `report.schema.json` | machine-readable run-outcome report | agentd RFC 0016 §6 | hand-authored, then emitter | interim (P6) |
| `metrics.registry.json` | frozen metric names/labels/HELP | agentd RFC 0016 §4 | **scrape-derived** from a live `/metrics` | derived; names unreconciled (**P10**) |
| `exit-codes.table.json` | the versioned exit-code table | agentd RFC 0016 §5, RFC 0011 §5 | checked-in table → `podFailurePolicy` constants | stable |
| `a2a.methods.json` | A2A method set + `Task`/`Message`/`Part` shapes | agentd RFC 0020 | hand-authored *once strings freeze* | **PENDING (P2)** — wire strings uncommitted |
| `mgmt.profile.json` | operator tool/resource names | agentd RFC 0015 §4/§5 | checked-in registry of names | stable |

The corpus is JSON Schema (Draft 2020-12, the dialect the reference impl's
`--config-schema` promises; agentd RFC 0017 §4.2) plus two small flat registries
(metrics, methods). It is neutral by construction — no Rust, no agent binary, no
language assumption.

### 3.2 The four codegen lanes (how (a) feeds (b))

**Lane 1 — config (emitter → `typify`).** When the reference impl ships
`agent --config-schema` (P6), agentctl vendors its output, pins it by digest, and
runs `typify` to generate the config structs into `agent-contract-client`.
**Caveat — `typify` vs Draft 2020-12.** `typify` is built around draft-07/2019-09
(the schemars 0.8 Schema model) and has *incomplete* support for 2020-12-specific
keywords (`prefixItems`, `unevaluatedProperties`, `$dynamicRef`/`$dynamicAnchor`); a
2020-12 `--config-schema` output may not round-trip through it — silently dropping
constraints or failing codegen. So this lane MUST do one of: constrain the published
config schema to the typify-supported keyword subset, pin a `typify` version with
documented 2020-12 coverage, or name an alternate generator — **and** add a **Lane-1
round-trip CI gate** (schema → generated types → re-validate a config corpus),
mirroring the golden round-trip the manifest path already has (§6.2/§7.4). Until P6
this lane is **blocked**; the admission ladder's in-webhook JSON-Schema check
(agentctl RFC 0007 §3.3) and the operator's render-target validation degrade to
the hand-authored interim schema, and the authoritative check defers to the
`agent --validate-config` init-container running the *exact target image*
(agentctl RFC 0007). Both subcommands are P6.

**Lane 2 — manifest / inventory / status / report / capacity (hand-authored JSON
Schema, conformance-enforced).** Because the manifest is deliberately `json!` →
`Value` (no `derive`, no `schemars`, no emitter — agentd RFC 0012 §3.7), agentctl
**hand-authors** these schemas from the contract RFCs and *enforces them
behaviorally* via the conformance suite (§7), which scrapes the real binary and
fails on any divergence. This lane is the reason (c) exists: it is the only honest
way to keep a hand-authored schema true to a `Value`-built surface. (`agent://capacity`
and `agent://metrics` are referenced by the scaling RFC but undefined in the
reference RFCs 0005/0015 — contract ask **P4**; the schema for them is pending
that freeze.)

**Lane 3 — metrics + exit codes (scrape-derived registry → constants).** The
metric-name registry is **derived by scraping a live `/metrics` (or
`agent://metrics`)**, *never hand-transcribed* — hand-transcription
(`agent_reactive_backlog` does not exist in the frozen schema) is precisely the
class of error codegen exists to prevent (brainstorm §11.2; the metric-name
reconciliation is contract ask **P10**). The scrape output, pinned by
`(digest + feature-set)`, is checked in as `metrics.registry.json` and compiled
into name constants; the exit-code table (agentd RFC 0016 §5) is checked in
directly and compiled into the `podFailurePolicy` mapping constants the operator
uses (agentctl RFC 0010 / RFC 0006). Conformance asserts the registry names are
*present* on a live scrape (§7.2).

**Lane 4 — A2A (PENDING, gated on P2).** The A2A method-name constants and the
`Task`/`Message`/`Part` types cannot be generated faithfully until the contract
freezes the wire strings (P2) and adds `surfaces.a2a` to the manifest. Until then
`a2a.methods.json` is a **placeholder marked pending**; `agent-contract-client`
emits the `Task`/`Message`/`Part` *shapes* (stable across 0.2.x/v1.0) but **not**
the method-name constants, and the gateway (agentctl RFC 0013) carries the
version-translation responsibility in the interim. This is the one corpus member
agentctl cannot generate against today.

### 3.3 The `surfaces{}` sum types — hand-written deserializers, always

`surfaces{}` is the *single discovery point* (agentd RFC 0014 §6.2) and is JSON
sum types — `bool | string` (`management`, `metrics`, `hot_reload`, …) and
`bool | object` (the richer entries). `typify` cannot generate a faithful Rust
type for these; **every `surfaces{}` field gets a hand-written `Deserialize`**
(RFC 0001 §2.4, brainstorm §11.2) into a small `enum` such as:

```rust
// crates/contract-client/src/surfaces.rs  (HAND-WRITTEN; the one place codegen
// is not authoritative — scoped to the sum-type fields only)
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceAddr {
    Off,                 // false
    On,                  // true        (build-gated on, address substrate-assigned)
    Unix(String),        // "unix:/run/agent/<pod>.sock"
    Vsock(u32),          // "vsock:PORT"
    Tcp(String),         // ":9090"
    Other(String),       // any unknown additive string form — RETAINED, never an error
}
// deserialize bool | string into SurfaceAddr; tolerate an unknown string form by
// retaining it as Other(String) rather than erroring — additive tolerance (§6.4).
// The Other variant is what makes the additive-by-minor guarantee structurally true:
// a future address scheme deserializes into Other instead of failing the read.
```

These deserializers are the **only** hand-maintained code in
`agent-contract-client`; everything else is regenerated from schema. They are
covered by their own unit tests and by the golden `--capabilities` corpus (§6.2).

### 3.4 Supply-chain: the codegen/scrape input must be pinned and signed

The binary fetched to **emit** schemas (Lane 1) or to **scrape** the registry
(Lane 3) is a codegen input and therefore a supply-chain surface. It MUST be
**hash-pinned and signature-verified** before use (brainstorm §11.2), recorded
alongside the pin (§6.1). A scrape from an unpinned or unverified binary MUST NOT
update `metrics.registry.json`. This applies equally to the reference agent and to
any vendor binary used to (re)derive a registry.

---

## 4. Source of truth (b) — the generated client crate `agent-contract-client`

`crates/contract-client` (crate name `agent-contract-client`) is the **single home
of the wire shape inside agentctl** (RFC 0001 §4.2, §5). Every one of the five
components depends on it and on nothing else for contract types. It is **generated**
from the §3 corpus by an `xtask` target and **links no agent binary** — the
forbidden coupling is *structurally impossible* because no agent crate exists in
the dependency graph (RFC 0001 §5 layout invariant).

### 4.1 What it exposes

- typed `Manifest`, `Identity`, `Limits`, `IntelligenceSummary`, `McpServerRef`
  (generated from `manifest.schema.json`);
- `Surfaces` with the **hand-written** sum-type deserializers (§3.3);
- the config types (generated from `config.schema.json` via `typify` — *once P6
  ships the emitter*; §3.2 Lane 1);
- the `RunReport` type (from `report.schema.json`);
- the **metrics-name registry as `const`s** and the **exit-code table** (from
  Lane 3) — the names the scrape-proxy and dashboards key off (agentctl RFC 0010)
  and the codes compiled into `podFailurePolicy` (agentctl RFC 0006);
- the operator tool/resource name constants (`DRAIN`, `LAME_DUCK`, `CANCEL`,
  `RES_CAPABILITIES`, `RES_INVENTORY`, `RES_STATUS`, `RES_EVENTS`; from
  `mgmt.profile.json`);
- the A2A `Task`/`Message`/`Part` *shapes* — and the A2A method-name constants
  **only once P2 freezes the wire strings** (§3.2 Lane 4).

### 4.2 Generation rules (the invariants that keep it drift-proof)

1. **Regenerated, never hand-edited** — except the scoped `surfaces{}`
   deserializers (§3.3), which live in a clearly-marked hand-written module and are
   the documented exception. A CI drift check (§7.4) regenerates and fails on any
   diff to the checked-in artifact, so a manual edit cannot silently persist.
2. **Lenient deserialization, never `deny_unknown_fields`** — the contract is
   additive-by-minor (agentd RFC 0014 §6.3); unknown fields/tools/metrics are
   *tolerated* and surfaced through the additive-drift report (§6.4), not rejected.
   A `deny_unknown_fields` derive would break negotiation and is **forbidden**
   (RFC 0001 §4.2/§4.4).
3. **No `Secret`, no credentials, ever** — the schema corpus carries none (the
   reference impl's `Secret` has no `Serialize`, agentd RFC 0012 §3.7), so the
   generated client structurally cannot hold one. The secret-safety invariant of
   the data plane is preserved on the consumer side by construction.
4. **Single dependency direction** — `agent-contract-client` depends only on
   `serde`/`serde_json` (and the schema artifacts at build time). It depends on no
   other `crates/*` and on no agent. All five components depend on *it*.

### 4.3 Checked-in vs build-time-generated

Leaning **checked-in artifact + CI drift check** (RFC 0001 §9 Open Q #2): the
generated `agent-contract-client` source is committed (aids review, makes the wire
shape diffable in PRs, and reproducible offline), and a CI job regenerates it from
the pinned corpus and fails on any diff (§7.4). Build-time-only generation avoids
stale artifacts but hides the wire shape from review and couples every build to the
generator toolchain. The `xtask codegen` target produces it either way; the choice
is whether the output is committed (Open Q §9).

---

## 5. The P0 contract-extraction realization

This is the realization P0 forces and RFC 0001 §9 Open Q #1 names: **the contract
is currently authored *inside* the reference agent's repo (agentd RFCs 0014–0020),
but it is implementation-neutral, and under P0 neither side should own the other.**
This RFC owns the *plan* (jointly with RFC 0001 §9); the steady-state home is a
human decision (Open Q §9).

### 5.1 Where the schemas live: vendored interim → neutral extracted

**Interim (today, and for v1):** agentctl **vendors** the corpus under `contract/`
(RFC 0001 §5), pinned by `(contract major.minor + schema digest)`. Where the
contract has not yet published an artifact (manifest, report — `json!`→`Value`;
config — emitter unbuilt; A2A — strings uncommitted), agentctl **hand-authors**
the interim schema from the contract RFCs and relies on the conformance suite (§7)
to keep it true. This is explicitly the **bootstrapping** state (RFC 0001 §8): the
hand-authored schemas are a liability the conformance suite contains, not an asset.

**End state (recommended):** the contract is **extracted into a neutral "Agent
Control Contract" spec** — its own home, its own version line, its own published,
versioned JSON-Schema corpus and golden-fixture corpus — so neither agentctl nor
any agent owns the other. agentctl then consumes the *published* artifacts
(deleting its hand-authored interim copies) and any vendor's agent maps its
internal structs onto the *same* published schemas. This is the clean P0
resolution (RFC 0001 §4.5, brainstorm §0.6 P0).

### 5.2 The migration path from today's reference RFCs 0014–0020

```
   PHASE 0 (now)          PHASE 1 (CC lands)         PHASE 2 (extraction)
   hand-authored          contract publishes         neutral contract spec
   interim schemas   ──▶  schemas + emitters    ──▶  owns the corpus + golden
   in contract/,          (P6, P3b); agentctl        fixtures; agentctl + every
   conformance-           replaces hand-authored      vendor consume it; branded
   enforced               copies with published       surfaces neutralized (§5.4)
```

1. **Phase 0 — interim, conformance-contained.** Hand-authored manifest/report
   schemas; scrape-derived metrics registry; A2A method names omitted (P2). The
   conformance suite is the truth-keeper. (This is v1.)
2. **Phase 1 — CC lands.** The reference impl implements `--config-schema` /
   `--validate-config` (P6), publishes the versioned `--capabilities` golden corpus
   (P3b), reconciles the metric names (P10), and freezes the A2A wire strings (P2).
   agentctl swaps each hand-authored schema for the published one as it lands,
   pinning by digest. The conformance suite is unchanged — it now validates against
   *published* schemas rather than hand-authored ones, and **catches the case where
   the published schema and the running binary disagree.**
3. **Phase 2 — extraction.** The corpus moves to the neutral spec home; agentctl
   re-points `contract/` (or a fetch step) at it; branded surfaces are neutralized
   (§5.4). At this point "any vendor" becomes literally true.

### 5.3 The CC / P6 precondition, stated as a hard gate

The Rust decision (RFC 0001 §2.2) and this pipeline both rest on **CC — the
contract must be published as language-neutral, machine-readable schemas.** Today:
the reference **manifest is `json!`→`Value`** (no struct, no schema — by design,
agentd RFC 0012 §3.7), and **`--config-schema` is unbuilt** (agentd RFC 0017 §4.2
specifies it; it does not exist). So CC is **real contract work** — and crucially
it is **contract work, not agent-coupling work**: implementing the emitter and
publishing the corpus is exactly what makes the contract portable to *other*
agents, which is what P0 demands (RFC 0001 §4.5). agentctl MUST NOT work around the
gap by linking the agent (forbidden) or by exec'ing tenant binaries in the webhook
(agentctl RFC 0007 §3.3 rejects this) — it works around it with hand-authored
interim schemas + behavioral conformance, and tracks P6/P3b as the path to
deletion of that interim code.

### 5.4 Brand-neutralization — the surfaces this RFC owns the plan for

The contract is presently *branded* with the reference impl's identifiers, which
are **contract-normative-but-branded**: the `--capabilities` CLI entrypoint, the
`agent://` URI scheme (`agent://capabilities`/`metrics`/`inventory`/`status`/
`events`), the `agent_`-prefixed metric names (agentd RFC 0016 §4), the `AGENT_*`
downward-API env family (agentd RFC 0014 §6.4), and the `a2a.*` strings. Depending
on these is depending on the *contract* (they are normative — RFC 0014 §3,
RFC 0015 §5.2, RFC 0016 §4), so it is P0-clean; but **true, brand-free vendor
portability requires renaming them to a vendor-neutral spelling** (RFC 0001 §4.3
"Honest scope of any vendor").

This RFC owns the **neutralization plan** (the contract-extraction open question is
"owned by RFC 0018 / RFC 0001 §9"):

- The neutral contract spec (§5.1 end state) defines a vendor-neutral spelling for
  each branded surface (e.g. a neutral URI scheme and metric prefix) **plus an
  alias table** mapping the branded reference spelling to the neutral one.
- `agent-contract-client` carries the alias table as data; the conformance suite
  (§7) accepts either spelling during the transition (an agent that emits the
  branded *or* the neutral spelling passes), so the rename is non-breaking for the
  reference impl.
- Until extraction, the suite asserts the **branded** spelling verbatim, and the
  portability claim is honestly scoped to "implementations of the agent-branded
  contract," not arbitrary brandings (RFC 0001 §4.3).

The brand-neutralization is **not** a v1 deliverable; it is the Phase-2 work this
RFC commits to *owning the plan for* so it does not become orphaned between repos.

---

## 6. Source of truth (d) — pinning, version negotiation & the additive-drift report

The three artifacts are bound at build time by a **pin** and at runtime by
**negotiation + graceful degradation**. This section specifies both, and why
`deny_unknown_fields` is forbidden.

### 6.1 The pinning identity: `(contract major.minor + schema digest)`, never an agent SHA

Codegen, the schema corpus, the golden fixtures, and the metrics registry are all
pinned by **`(contract major.minor + schema digest)`** — *not* by an agent SHA
(RFC 0001 §8, which makes the P0 "not a SHA" call; brainstorm §11.3 records the
pinning-unit as an open question — **superseding** the SHA-*inclusive* formula in
brainstorm §11.1, "(agent SHA + cargo feature-set + contract major.minor)", which P0
overrides). The reason is decisive: the reference impl's
manifest, `surfaces{}`, and metric set are **`cfg!`-conditional / build-feature-
gated** — two binaries at the *same* SHA but different cargo feature-sets emit
*different* manifests and *different* metric registries. A SHA is therefore not a
stable contract identity; the contract *version* plus the *schema digest* is.

The full pin recorded with each generated artifact:

```jsonc
// contract/PIN.json  — recorded alongside the corpus and the generated client
{
  "contract_version": "1.0",                 // major.minor (agentd RFC 0014 §6.3)
  "schema_digest": "sha256:9f2c…",           // digest of the contract/ corpus
  "feature_set": ["serve-mcp","metrics","events","hot-reload","vsock","a2a"],
  "source_binary": {                          // the codegen/scrape input (§3.4)
    "ref_impl_version": "agent 2.2.0",       // descriptive, NOT a pin authority
    "binary_digest": "sha256:1a4e…",          // hash-pinned + signature-verified
    "signature": "cosign:…"
  },
  "golden_corpus_ref": "fixtures/1.0/"        // the P3b golden --capabilities set
}
```

`feature_set` is part of the pin because the registry and manifest are
build-conditional (the cache key for the metrics scrape and the manifest schema is
`(digest + feature_set)`, mirroring the operator's CapabilityProbe cache key,
agentctl RFC 0006 §3.1).

### 6.2 The P3b golden-fixture corpus

`test/fixtures/<contract_version>/` holds a **per-feature-set golden corpus of
`--capabilities` outputs** (contract ask **P3b**), versioned by
`(contract major.minor + digest)`. Each fixture is a real `--capabilities` JSON for
a named feature-set (`minimal`, `metrics-only`, `full`, `vsock+a2a`, …). The corpus
drives three things:

1. **Codegen regression** — `agent-contract-client` MUST deserialize every fixture
   without loss (round-trip), including the `surfaces{}` sum-type deserializers
   (§3.3). A new fixture that the client cannot parse is a generation bug or a
   missing additive-tolerance.
2. **Negotiation tests** — fixtures at minor `1.0` *and* a synthesized `1.1` (with
   an added field/tool/metric) prove additive tolerance; a synthesized `2.0` proves
   the major-refusal path (§6.3).
3. **Conformance baselining** — the suite (§7) compares a live agent's
   `--capabilities` against the matching golden fixture for its advertised
   feature-set.

Until P3b publishes a *contract-blessed* corpus, agentctl **scrapes its own**
fixtures from the pinned reference binary (§3.4) and marks them
`source: scraped` rather than `source: published`; Phase-1 (§5.2) replaces them.

### 6.3 Runtime version negotiation (agentd RFC 0014 §6.3)

Every component negotiates through `agent-contract-client`, identically:

- **Refuse on unknown major.** agentctl reads `contract_version`; if the **major**
  is unknown, it refuses to drive the instance (it still manages liveness + exit
  codes + logs — the floor every agent supports, agentd RFC 0014 §8). This mirrors
  the reference impl's own refusal of an instance whose major it does not
  understand (`CONTRACT_VERSION = "1.0"`; the negotiation spine is real and shipped,
  brainstorm §0.6).
- **Branch on minor + sub-schema versions.** Within a known major, agentctl reads
  the **minor** and the independently-versioned sub-schemas surfaced in
  `surfaces{}` (`metrics_schema`, `report_schema`, `config_schema`; agent
  RFC 0014 §6.3) to decide which features to drive.
- **Degrade off `surfaces{}`.** `surfaces{}` is the *single* discovery point
  (agentd RFC 0014 §6.2). A surface absent ⇒ unbuilt/off ⇒ agentctl drives only
  what is declared. A `"cluster":false` agent degrades to an unscaled singleton
  (agentctl RFC 0011 §9 graceful degradation); an agent advertising only `management` degrades to
  control-without-metrics; an agent advertising nothing degrades to the floor.

### 6.4 Why `deny_unknown_fields` is forbidden — and the additive-drift report

The contract is **additive-by-minor by design** (agentd RFC 0014 §6.3): a new
optional manifest key, optional tool/resource, or new metric is a *minor* bump that
older consumers MUST tolerate. Therefore:

- **`deny_unknown_fields` is forbidden** in `agent-contract-client` (§4.2 rule 2).
  It would turn a contract-legal additive minor into a hard parse failure — the
  opposite of negotiation. A *lenient* client, however, gives up the static
  "cannot drift" guarantee a strict type would have provided.
- **The conformance suite catches *regressive* drift** (a surface that disappeared
  or changed semantics — §7), but it cannot catch *additive* drift (a new
  capability the suite does not yet drive).
- **The additive-drift report closes the gap.** `agent-contract-client` records, on
  every manifest read, any field/tool/metric **seen but not driven** by the pinned
  contract version, and the operator/node-agent surface it as an
  `AdditiveDrift` observation (a log line + a low-severity event, never a hard
  error). It is the signal that "the agent advertises a `1.1` capability; agentctl
  pins `1.0` and is leaving it on the table — time to bump the pin." Regressive drift
  (suite, hard-fail) + additive drift (report, advisory) together are the **full**
  anti-drift picture (RFC 0001 §4.4, brainstorm §11.2).

---

## 7. Source of truth (c) — the behavioral, black-box conformance suite

`crates/conformance` is agentctl's **executable definition of "a conformant
agent."** It mirrors the reference impl's own `agent-conformance` crate — which is
black-box *by charter* (`crates/agent-conformance/src/lib.rs`, verbatim):
*"Nothing here links the agent library: conformance is judged against the MCP /
JSON-RPC spec and the documented exit-code table, not against agent's own types."*
agentctl's suite does the same against **any** agent binary, over the substrate
transports — the unix-socket dev loop being the contract-clean local path (agentctl
RFC 0002). It **links no agent**; it drives a *binary* (downloaded/pinned, §3.4)
and asserts behaviour. Where the reference impl's `agent-conformance` proves the
*data-plane* contract (MCP server/client, supervisor, agent loop, security,
work-claim — the `Category` families it already ships), agentctl's
`crates/conformance` proves the *control-plane* contract the five components
consume.

### 7.1 The harness and the family taxonomy

```rust
// crates/conformance — drives ANY conformant agent binary; links NO agent.
pub struct Harness { /* spawns the binary, opens the discovered socket (RFC 0002) */ }

pub enum Family {
    Manifest,    // --capabilities + agent://capabilities vs the published schema
    Management,  // the operator tool/resource profile BEHAVES (drain/lame-duck/cancel)
    Metrics,     // the frozen metric names are PRESENT on a live scrape
    Config,      // --validate-config / --config-schema  (gated on P6)
    ExitCodes,   // the exit-code table holds under induced failures
    A2A,         // the A2A method set + Task/Message/Part  (gated on P2)
    Negotiation, // major-refusal, minor-tolerance, surfaces{} degradation
}
```

Each family is a flat list of named checks returning pass / fail-with-diagnostic,
exactly as `agent-conformance` structures its `Check`/`Category`/`Outcome` — so a
single suite backs both `cargo test` (CI gating) and a `conformance` runner binary
that renders a PASS/FAIL report a vendor runs against their own agent (§7.3).

### 7.2 What it asserts (the contract, behaviorally)

- **Manifest (shape + identity invariant).** `--capabilities` (stdout JSON, exit 0)
  and the live `agent://capabilities` resource both validate against
  `manifest.schema.json`, **and are semantically equal — parsed-then-compared as JSON
  values, never byte-compared.** The contract guarantee is *same-source* (both surfaces
  read one const "so the manifest and the live surface cannot drift," agent
  `capabilities.rs`), which is **structural equality of the parsed documents**, not byte
  identity: a conformant agent that compacts one surface and pretty-prints the other (or
  orders keys differently) is **fully conformant** and MUST NOT be failed. Asserting
  byte-equality would reject conformant agents — a P0-portability defect, so the suite
  parses both and compares JSON values. `surfaces{}` deserializes through the
  hand-written sum-type path (§3.3) for every advertised form.
- **Management profile (behaviour, not presence).** `drain` ≡ SIGTERM ≡ a **clean
  exit 0** (agentd RFC 0015 §4.1 — `drain` is *not* a drain-without-delete; it is
  the supervised graceful exit); `lame-duck` flips readiness to NotReady **without
  exiting** (agentd RFC 0015 §4.2); `cancel` by run handle terminates the targeted
  run; the `agent://` resource set (`capabilities`/`inventory`/`status`/`events`)
  is subscribable. (Asserting *presence* of a `drain` tool is worthless; asserting
  it *exits 0 cleanly* is the contract.) **The required-vs-optional partition is read
  from `mgmt.profile.json`** (the contract-declared management profile, §3.1), **not
  hardcoded** to the reference impl's present inventory: a tool the profile marks
  *required* MUST behave; a tool it marks *optional* is checked
  present-and-correct-when-advertised, optional-when-absent. Today the reference profile
  marks `{drain,lame-duck,cancel}` required and `{pause,resume}` optional-pending-**P-pause**
  (`OPERATOR_TOOLS = [drain, lame-duck, cancel]`), but the suite reads that partition
  *from the artifact* — so a contract minor that makes a tool optional does **not**
  require a suite code change, and the bar tracks the contract rather than one binary.
- **Metrics (registry presence on a live scrape, after warm-up).** The authority for
  *which names must exist* is the contract's **frozen metrics schema** (agentd RFC 0016
  §4, the P10-reconciled set), **not** the reference binary's emitted set. The scrape of
  a live `/metrics` (or `agent://metrics`, P4) is used to (a) assert every
  contract-frozen name is present on the agent and (b) catch the *hand-transcription*
  error by **diffing the scrape against the contract-declared frozen set** (a name in a
  doc that no binary emits, e.g. `agent_reactive_backlog`, §3.2 Lane 3). **Scrape
  precondition:** counters/histograms are commonly absent from `/metrics` until first
  observation, so presence is asserted only **after a defined warm-up** (drive one run /
  one work item) — or the registry distinguishes always-present gauges from
  observe-on-first-use series; a freshly-spawned agent is never failed for not yet having
  emitted an observe-on-use metric. The clean-drain exit-code distinction (a graceful
  SIGTERM returns `0`, not `143`; agentd RFC 0016 §5.3) is asserted here as it is the
  metric/exit-code seam.
- **Config (gated on P6).** `agent --validate-config` exits `2` on bad config with
  diagnostics and `0` on good (agentd RFC 0017 §4.1); `agent --config-schema`
  emits a valid Draft 2020-12 document that round-trips through the Lane-1 generator
  (§3.2). Skipped-with-reason until P6.
- **Exit codes (under induced failures).** Drive a usage error → `2` (`FailJob`), a
  semantic refusal → `5` (`FailJob`), a budget exhaustion → `7` (policy, **remap-
  sensitive** — `--budget-exit-code` can move it; brainstorm §3.2, P-cost), a timeout
  → `124`, and assert the table **sourced from `exit-codes.table.json`** (the default
  values; agentd RFC 0016 §5), never inline literals — a conformant agent configured
  with a remapped budget code is driven with that config and not failed for the remap.
  These are the codes the operator compiles into `podFailurePolicy` (agentctl RFC
  0006/0010).
- **A2A (gated on P2).** Once the wire strings freeze: the method set, the
  `Task`/`Message`/`Part` shapes, and the manifest-as-Agent-Card projection (agent
  RFC 0020). Skipped-with-reason until P2.
- **Negotiation (the (d) discipline).** Feed the major-refusal, minor-tolerance,
  and `surfaces{}`-degradation fixtures (§6.2) and assert agentctl's client behaves
  per §6.3 — refuses unknown major, tolerates unknown additive minor (and emits the
  additive-drift report, §6.4), degrades off an absent surface.

### 7.3 How a new agent vendor self-certifies

This is the operational meaning of P0. A second-vendor agent — **any language, any
vendor** — self-certifies by:

1. Building their agent to serve the management profile over a discovered socket
   (the unix-socket dev loop is the contract-clean target; agentctl RFC 0002).
2. Running agentctl's `conformance` runner binary against it:
   `agentctl-conformance --agent ./their-agent --transport unix:/tmp/agent.sock`.
3. Reading the PASS/FAIL report. **A full PASS means agentctl manages the agent
   unchanged** — no agentctl code change, no recompile, no vendor-specific branch.

The **reference agent is simply the first subject** of this suite, not a privileged
one (RFC 0001 §4.3). The honest caveat (RFC 0001 §4.3): until brand-neutralization
(§5.4), "conformant" means honouring the agent-*branded* surfaces verbatim
(`--capabilities`, `agent://`, `agent_` metric prefix); the suite accepts the
neutral spelling too once the alias table lands. The vendor-portability claim is
scoped to "implementations of the (branded today, neutral later) contract."

### 7.4 CI gating

| Gate | What runs | Block / warn |
|---|---|---|
| **Codegen drift** | `xtask codegen` regenerates `agent-contract-client`; diff vs checked-in (§4.3) | **block** on diff |
| **Conformance (regressive)** | `crates/conformance` drives the **pinned reference binary** (§3.4) over the unix dev loop | **block** on any fail |
| **Golden-corpus round-trip** | `agent-contract-client` deserializes every `test/fixtures/<ver>/` fixture losslessly (§6.2) | **block** on loss |
| **Config round-trip (Lane 1)** | generate config types from `config.schema.json`, re-validate a config corpus through them (the typify/2020-12 guard, §3.2 Lane 1) | **block** on loss — *active once P6 lands; skipped-with-reason until then* |
| **Additive-drift** | the additive-drift report (§6.4) over the live reference manifest | **warn** (advisory; a new capability seen-but-not-driven is not a failure) |
| **Schema validity** | every `contract/*.schema.json` is valid Draft 2020-12 | **block** on invalid |
| **Supply-chain** | the codegen/scrape input binary is hash-pinned + signature-verified (§3.4) | **block** on unverified |

Regressive drift blocks merge; additive drift warns (it is the signal to bump the
pin, §6.4); the codegen drift check makes a hand-edit of the generated client
(outside the §3.3 sum-type module) impossible to land silently.

---

## 8. E2E + chaos test strategy

Conformance (§7) proves the *contract* against one binary on one socket. E2E proves
the **five components manage a real fleet on a real cluster**, and chaos proves the
failure-domain invariants the architecture *claims* (the node-agent bounce-safety
scoping, the scaler-as-SPOF, the A2A store durability, contract-skew degradation).

### 8.1 kind-based E2E on the stock-unix substrate (no microVM for local/dev)

The default E2E lane is **kind** (Kubernetes-in-Docker) on the **stock unix-hostPath
substrate** — RFC 0002's PRIMARY/dev tier, which runs on stock runc/containerd and
needs **no microVM**. This is the whole point of RFC 0002's tiering for testing: the
unix-hostPath path and the hardened Kata-vsock path converge on "open a discovered
socket," so the *control-plane logic* (operator render, node-agent bridge, status
projection, scaler, gateway) is fully exercisable on kind without Kata. The local
dev loop is identical: `cargo test` runs `crates/conformance` against a
downloaded/pinned reference binary over a unix socket — no cluster, no microVM, no
network (RFC 0001 §8 bootstrapping order, brainstorm §16 Phase 0).

### 8.2 The substrate matrix

| Lane | Substrate (RFC 0002) | Where it runs | What it exercises | Cadence |
|---|---|---|---|---|
| **unix-fast** | unix-hostPath PRIMARY | kind, any runner | full control-plane logic; the contract-clean loop | **every PR** |
| **kata-vsock** | Kata-hybrid-vsock HARDENED | kind + Kata (self-hosted node) | the **guest↔host crossing** — CID/uds discovery, the host-side listener, the bridge/transport code | **every PR** (the risky path) |
| **sidecar** | emptyDir-sidecar PORTABLE | kind | the restricted-cluster fallback tier (Autopilot/Fargate shape) | nightly |

The **kata-vsock lane on every PR** is non-negotiable per brainstorm §11.2: the
unix fast-loop *never exercises the guest↔host crossing*, and "CID allocation +
host-side listener is the project's actual hard part." A green unix lane with a red
kata lane is the exact silent-breakage the matrix exists to prevent. **Feasibility
caveat:** Kata *inside* kind nodes (containerd-in-Docker) needs **nested
virtualization** and `/dev/kvm` in a privileged node container, and Kata normally
wants to be the *node's* runtime on a real/bare node — so the every-PR cadence may
require a **KVM-capable self-hosted node running Kata as the node runtime**, not
kind-node-in-Docker. This is a feasibility constraint, not merely a runner-budget one,
and it ties directly to the per-PR-vs-nightly decision (Open Q #7). (The kata lane
needs a Kata-capable node — a self-hosted runner; cost/placement is Open Q #7.)

### 8.3 Chaos scenarios (each asserts a claimed invariant)

| Scenario | The invariant under test | Expected behaviour |
|---|---|---|
| **node-agent (Tier A) bounce** | "Tier A crash = control gap only, **zero data-plane impact**" (agentctl RFC 0008 §3.3 — the bounce-safe invariant, scoped to Tier A) | agent keeps running (liveness = supervisor heartbeat, independent of the mgmt connection; agentd RFC 0015 §8); reconnect is a clean re-read; `drain` still reachable via SIGTERM (pod delete). **Assert no run is interrupted.** |
| **scaler (`crates/scaler`) loss** | the from-zero signal is a control-plane SPOF (agentctl RFC 0011 §11 #9) | a `min:0` claim fleet stranded at zero wakes via the `activationFallbackReplicas` floor or KEDA's last-replicas hold; **assert no permanent stuck-at-zero.** |
| **coordination-server loss** | the serializing point's blast radius (agentctl RFC 0011 §11 #1) | in-flight claims survive failover via transactional `claim_key` dedupe; **assert no double-processing across the seam.** |
| **A2A task-store loss** | shared durable store survives node/pod loss (agentctl RFC 0013 / brainstorm D4) | terminal tasks answered from the store; live ops re-routed to the owner; **assert no lost distillate** (the relay is a must-not-miss consumer; contract ask **P5**). |
| **contract-skew** | negotiation + graceful degradation (§6.3) | an agent advertising a **minor-newer** manifest is driven at the pinned minor + emits the additive-drift report; an agent advertising a **feature-subset** is driven down to its declared surfaces; an **unknown-major** agent is refused-but-floor-managed. **Assert no hard crash, correct degradation.** |
| **two-owner lease seam** | claim correctness under a partition (agentctl RFC 0011) | at most one owner per item; **assert no concurrent double-claim.** |
| **operator failover mid-reconcile** | level-triggered idempotent reconcile (agentctl RFC 0006) | the new leader re-derives desired state from spec+observed; **assert no orphaned/duplicated children, no status hot-loop.** |
| **reschedule mid-task** | statelessness + re-drive policy (agentctl RFC 0013 / agentd RFC 0019 §6) | default = FAIL + final webhook on owner loss; re-drive only behind the per-fleet idempotency opt-in. **Assert the default does not silently re-execute a non-idempotent composition.** |

### 8.4 The local/dev story (no microVM needed)

The dev inner loop requires **only** the stock-unix tier: a pinned reference binary
+ a unix socket + `cargo test`. Conformance (§7), codegen drift (§7.4), and the
golden-corpus round-trip all run with no cluster. The kind unix-fast lane reproduces
the cluster behaviour locally on Docker; the kata-vsock lane is the **only** thing
that needs special infrastructure (a Kata node) and is therefore CI-gated rather
than required for every local change. This keeps the contributor barrier at "Docker
+ Rust," which is the brainstorm §16 Phase-0 cut line.

---

## Non-goals

- **Authoring the contract's content.** The schemas' *shape and semantics* are
  owned by the contract (the reference impl's agentd RFCs 0014–0020, eventually the
  neutral spec, §5). This RFC owns the *consumption pipeline*, not the contract. A
  disagreement on the wire is a contract ask (P-series), never an agentctl override.
- **Building the reference impl's schema emitters.** `--config-schema` /
  `--validate-config` (P6), the published golden corpus (P3b), the metric-name
  reconciliation (P10), and the A2A wire-string freeze (P2) are **contract work in
  the contract's repo**. agentctl *consumes* them and contains their absence with
  hand-authored interim schemas + behavioral conformance (§5.3); it does not
  implement them, and it does not link or exec the agent to fake them (agentctl
  RFC 0007 §3.3).
- **Codegen of the CRD types.** `crates/crds` (the `Agent`/`AgentFleet` types via
  `#[derive(CustomResource, JsonSchema)]` + `schemars` + CEL) is **not**
  `agent-contract-client`. CRD type generation and CRD-YAML emission are agentctl
  RFC 0003 (+ the `xtask` CRD target, RFC 0001 §5); CRD versioning/conversion is
  agentctl RFC 0005. This RFC's codegen is strictly the **contract client**, not
  the CRD surface.
- **A schema-registry *service*.** The corpus is a vendored, pinned set of files
  (and later a published spec), not a running registry component to operate.
- **Release/upgrade engineering and the CI infrastructure itself.** agentctl's own
  multi-component upgrade/skew matrix, DR, GitOps fit, and air-gap are agentctl
  RFC 0017 (brainstorm §12). This RFC specifies *what CI gates* (§7.4) and *what
  E2E/chaos lanes exist* (§8), not the pipeline platform that runs them.
- **Choosing the data plane's language.** Settled in RFC 0001 §7 — the contract is
  language-neutral; a vendor agent in any language is in scope to manage by
  construction (§7.3).

## Open questions

1. **The contract-extraction home and timeline** (jointly with RFC 0001 §9 Open
   Q #1). Recommended: a neutral "Agent Control Contract" spec with its own
   versioned JSON-Schema + golden-fixture corpus (§5.1). Until then agentctl
   vendors under `contract/`. *Who* owns the neutral spec repo, and on what
   timeline does Phase 1 (CC) → Phase 2 (extraction) land relative to v1?
2. **Brand-neutralization sequencing** (§5.4). The alias-table + dual-spelling
   acceptance makes the rename non-breaking — but when does the neutral spelling
   become the *primary* one, and does v1 commit to *emitting* the neutral spelling
   anywhere, or only *accepting* it?
3. **Checked-in vs build-time-generated `agent-contract-client`** (RFC 0001 §9
   Open Q #2). Leaning checked-in + CI drift check (§4.3); confirm.
4. **Build-vs-download the reference binary in CI** (brainstorm §11.3). Downloading
   a signed, hash-pinned release is faster and is the supply-chain-clean default
   (§3.4); building from a pinned SHA is hermetic but slow and re-introduces a
   feature-set ambiguity (the pin is contract-version, not SHA — §6.1). Which is
   canonical?
5. **How additive drift graduates from warn to block** (§6.4/§7.4). Additive drift
   is advisory today (a new capability seen-but-not-driven). Should there be a
   policy that an additive capability *unconsumed for N releases* becomes a
   block-level "bump the pin" gate, or does it stay perpetually advisory?
6. **A2A codegen interim** (§3.2 Lane 4, P2). Until the wire strings freeze, does
   `agent-contract-client` ship a hand-stubbed A2A method set behind a `pending`
   feature flag (so the gateway compiles against *something*), or omit method
   constants entirely and force the gateway's interim translation layer (agentctl
   RFC 0013) to own all spellings?
7. **The kata-vsock CI lane placement and cost** (§8.2). The every-PR kata lane
   needs a Kata-capable node (self-hosted runner). Is that acceptable for v1, or is
   the kata lane demoted to nightly + pre-release (accepting that the "actual hard
   part" — the guest↔host crossing — is then not gated per-PR)?
8. **Conformance over non-unix transports for vendor self-cert** (§7.3). The dev
   loop is the unix socket. Must a vendor *also* pass the suite over vsock (the
   hardened tier) to be "fully conformant," or is unix-pass sufficient for
   certification with vsock proven only in agentctl's own kata lane?

## References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo: **§4 is the principle this RFC details**
  (the contract-as-schema anti-drift, the generated client, the conformance suite,
  the `surfaces{}` sum types needing hand-written deserializers §2.4); §5 the
  workspace layout (`crates/contract-client`, `crates/conformance`, `contract/`,
  `xtask`, `test/`); §8 the pinning unit + bootstrapping order; §9 Open Q #1 (the
  extraction home this RFC co-owns).
- **agentctl RFC 0002** — substrate & transport: the "open a discovered socket"
  convergence the conformance harness and E2E matrix ride; the unix-hostPath dev
  loop; the kata-vsock hardened tier the §8.2 matrix exercises.
- **agentctl RFC 0003** — Agent & AgentFleet CRDs: the CRD type codegen
  (`schemars`/CEL) that is **distinct** from `agent-contract-client` (Non-goals);
  the curated status projection that consumes the generated `Manifest`.
- **agentctl RFC 0005** — CRD versioning & conversion: the CRD-side versioning,
  **distinct** from the contract-version negotiation of §6.3.
- **agentctl RFC 0006** — operator reconcile & capability model: the CapabilityProbe
  cache keyed by `(digest + feature-set)` mirroring §6.1; the `podFailurePolicy`
  compilation from the exit-code table (§4.1 Lane 3).
- **agentctl RFC 0007** — admission validation ladder: the in-webhook JSON-Schema
  check against `config.schema.json` and the `--validate-config` init-container
  ground truth — both gated on P6 (§3.2 Lane 1, §5.3).
- **agentctl RFC 0008** — node-agent architecture: the Tier-A bounce-safety
  invariant the §8.3 chaos lane asserts; the additive-drift report surface.
- **agentctl RFC 0010** — observability & telemetry bridge: the metrics-name
  registry (§3.2 Lane 3) the scrape-proxy keys off; the exit-code→`podFailurePolicy`
  map.
- **agentctl RFC 0011** — scaling plane: the scaler-as-SPOF and coordination/lease
  chaos scenarios (§8.3); the unreconciled metric names (P10) the registry contains.
- **agentctl RFC 0012** — intelligence plane: the per-binary intelligence **dialect
  set** the manifest should advertise (contract ask **P-dialects**) so the proxy's
  pass-through-vs-translate boundary is contract-driven, not keyed to the reference
  impl's two-adapter inventory.
- **agentctl RFC 0013** — A2A gateway & task store: the A2A method-set consumer
  (pending P2, §3.2 Lane 4); the task-store-loss durability chaos lane (§8.3, P5).
- **agentctl RFC 0016** — CLI & kubectl-plugin: the runtime negotiation +
  graceful-degradation UX (§6.3) the CLI renders.

**agentd RFCs (the reference impl's contract spec)**

- **agentd RFC 0014** (the contract umbrella) — manifest spine §5, `surfaces{}`
  discovery §6.2, **versioning/negotiation §6.3** (the additive-minor/breaking-major
  rule this RFC enforces), downward-API env §6.4, graceful degradation §8.
- **agentd RFC 0015** (the reference impl's contract spec) — management & control
  surface; manifest schema §5.2, operator tools §4 (`drain`/`lame-duck`/`cancel`
  the suite asserts behaviorally), the frozen `work.*` §5.6.
- **agentd RFC 0016** (the reference impl's contract spec) — the frozen metrics
  schema §4 (the scrape-derived registry), the exit-code table §5, the clean-drain
  `0`-not-`143` distinction §5.3, run-outcome reports §6.
- **agentd RFC 0017** (the reference impl's contract spec) — declarative config;
  `--validate-config` §4.1 and `--config-schema` §4.2 (the CC precondition surface,
  unbuilt — contract ask **P6**).
- **agentd RFC 0020** (the reference impl's contract spec) — A2A over the substrate;
  the A2A method set (wire strings not yet frozen — contract ask **P2**, §3.2
  Lane 4).
- **agentd RFC 0012 §3.7** (the reference impl) — the `Secret`-has-no-`Serialize`
  invariant that makes the manifest deliberately `json!`→`Value`, the root reason
  the manifest schema is hand-authored (§3.2 Lane 2, §4.2 rule 3).
- **`crates/agent-conformance`** (the reference impl) — the black-box,
  never-link-the-library conformance pattern `crates/conformance` mirrors for *any*
  agent (§7); its `Category` taxonomy (MCP server/client, supervisor, agent loop,
  security, work-claim) is the data-plane analogue of §7.1's control-plane families.

**Contract asks (brainstorm §14)**

- **CC / P6** — published, versioned JSON-Schema corpus + `--config-schema` /
  `--validate-config` (the schema corpus precondition, §3.2 Lane 1, §5.3).
- **P3b** — confirm every `surfaces{}` key addition bumps contract MINOR; publish a
  versioned golden-fixture corpus of `--capabilities` outputs per feature-set
  (§6.2).
- **P2** — `surfaces.a2a` manifest key + committed A2A wire strings (§3.2 Lane 4).
- **P10** — reconcile the autoscaling metric names into one frozen set (§3.2
  Lane 3).
- **P4** — define `agent://metrics` (byte-identical Prom 0.0.4) and
  `agent://capacity` (frozen schema) (§3.2 Lane 2).
- **P5** — read-before-exit / terminal-distillate re-read (the A2A store-loss chaos
  lane, §8.3).
- **P-pause** — implement `pause`/`resume` (the optional-when-absent management
  checks, §7.2).
- **P-dialects** — advertise the in-binary intelligence dialect set in the manifest
  (agentctl RFC 0012 cross-cut).

**Brainstorm**

- agentctl architecture brainstorm — §11 (stack/repo/**codegen**: the three codegen
  tiers refined into §3's four lanes, the scrape-don't-transcribe rule, the
  hand-written sum-type unmarshalers, the kind+Kata-every-PR lane, the
  supply-chain-the-input rule), §0.6 (P0 + CC/AT preconditions), §14 (the contract
  asks above), §16 (the Phase-0 cut line the local/dev story matches).
