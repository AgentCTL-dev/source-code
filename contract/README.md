# Agent Control Contract (ACC) — v1

The **Agent Control Contract (ACC)** is the neutral, language-neutral, machine-readable
contract that **agentctl** (the Kubernetes control plane) consumes and that **any conformant
agent** implements. It is published as a set of JSON Schemas (draft 2020-12) plus golden
fixtures and frozen data catalogues.

This directory is the **working home** for the contract while it is extracted out of the
reference implementation's RFCs (agentd RFCs 0014–0020). It is structured to later lift into
its own neutral repository — see *Open question P0* at the bottom.

---

## P0 — agentctl depends on the CONTRACT, never on a specific agent

The foundational principle: **agentctl consumes only this contract; it never depends on a
specific agent implementation.** The reference `agentd` binary is the *reference*
implementation — the first agent to satisfy the ACC — but it is not privileged. Any binary
that emits a conformant capabilities manifest, honors the exit-code table, serves the declared
surfaces, and speaks the declared wire protocols is a conformant agent that agentctl can drive.

The contract is **fully neutral**: it defines only the neutral tokens (`agent_*` env and
metric prefix, `AGENT_*` env vars, the `agent://` URI scheme, the `agent/*` `_meta`
namespace). Because these tokens are **vendor-neutral**, any agent can implement the
contract; the reference implementation is **agentd v1.0.0**, which speaks this neutral
contract (and keeps `agentd://` as a legacy alias of `agent://`) — see *Neutral-wire map*.

---

## File map

```
contract/
  VERSION                                  # the contract version: 1.0
  README.md                                # this file — the ACC overview + P0 + neutral-wire map
  SPEC.md                                  # the spec-level companion: cross-cutting laws, frozen catalogues, sum types, gotchas
  schemas/                                 # CANONICAL finalized schema set (draft 2020-12)
    manifest.schema.json                   # capabilities manifest — the discovery spine
    config.schema.json                     # agent config-file schema (reloadable + restart-only view)
    report.schema.json                     # run-outcome report
    events.schema.json                     # agent://events live event-stream envelope + closed event vocabulary
    metrics.registry.json                  # FROZEN metrics_schema 1.0 registry (DATA catalogue)
    a2a.methods.json                       # A2A method registry + types (bridge table)
    exit-codes.table.json                  # FROZEN exit-code table (DATA catalogue)
    management-profile.json                # operator tools + agent:// resources + PeerOrigin gating
    env-convention.json                    # downward-API env-var convention
  fixtures/
    capabilities/
      default.json                         # GOLDEN — real --capabilities capture (release build, surfaces off)
      full-features.json                   # GOLDEN — real --capabilities capture (debug build, surfaces on)
      reference-full.json                  # synthetic full-feature manifest (schema-author fixture)
      minimal-degraded.json                # synthetic all-surfaces-off manifest (graceful-degradation fixture)
```

### Schema vs. data catalogue

`manifest.schema.json`, `config.schema.json`, and `report.schema.json` are **JSON-document
validators** — they validate an instance (a manifest, a config file, a report). The other
three (`metrics.registry.json`, `a2a.methods.json`, `exit-codes.table.json`) and the two
control artifacts (`management-profile.json`, `env-convention.json`) are **draft-2020-12
envelopes around frozen DATA catalogues** (registries/tables). They carry `$schema`/`$id`/
`title`/`description` and pass the metaschema, but their payload is reference data for codegen,
not an instance validator. (Prometheus `/metrics` output is text, not JSON — do not try to
validate it against `metrics.registry.json`.)

### Canonical set

`schemas/` is the single canonical set (9 files). The earlier parallel `v1/` extraction has
been **removed**, and its one unique artifact (`events.schema.json`) promoted into `schemas/`.
Every `$id` is unified under `https://agentctl.dev/contract/v1/<file>`, so there is no
duplicate-`$id` clash. (The `v1` in the `$id` path is the *contract* major version, not a
directory.) All `$ref`s are file-internal (`#/$defs/*`) and resolve.

---

## Version negotiation

`contract_version` is **major.minor** (reference: `"1.0"`).

- **Additive growth ⇒ MINOR bump.** New manifest fields, new `surfaces{}` keys, new operator
  tools, new metrics, new config keys are additive. A consumer **MUST tolerate** them: every
  open object uses `additionalProperties:true`. **Never apply `deny_unknown_fields` /
  `additionalProperties:false` to an open object.** (The config-file *input* surface is the
  deliberate exception — see *Provisional/open items*.)
- **Breaking change ⇒ MAJOR bump.** Removing/renaming/narrowing a field, changing a sum-type
  shape, or breaking a frozen table is major. A consumer **refuses only an unknown MAJOR**;
  within a known major it accepts any unknown additive content.
- **`surfaces{}` is the single discovery point** (RFC 0014 §6.2). Each control-plane surface
  is reported honestly as served-or-not for *this* build/config. **A key absent ⇒ surface
  unbuilt ⇒ degrade gracefully** — absence is never an error, and agentctl drives only what
  is declared. Do not branch on a value that is not advertised in `surfaces{}`.

### Sum-type surface keys (hand-written deserializers required)

Several keys are **sum types** that codegen cannot derive (agentctl RFC 0018 §3.3). They each
need a hand-written deserializer:

| key | shape | meaning |
|---|---|---|
| `surfaces.management` | `false \| string` | management transport address, else `false` |
| `surfaces.metrics` | `false \| string` | `/metrics` scrape address, else `false` |
| `surfaces.a2a` | `false \| object{version,streaming,methods[]}` | A2A surface, else `false` |
| `surfaces.claim` | `bool \| object{styles[]}` | claim styles — **omitted-when-absent, never `false`** |
| `surfaces.shard` | `string \| null` | `"K/N"` shard identity, else `null` |
| `intelligence.healthy` | `bool \| "unknown"` | reachability, or `"unknown"` pre-connect |

---

## Neutral-wire map (P0)

The contract defines only the **neutral** canonical spellings, so any agent can implement it.
The reference implementation is **agentd v1.0.0**, which speaks this neutral contract (and
keeps `agentd://` as a legacy alias of `agent://`).

| concern | neutral (canonical) |
|---|---|
| downward-API env prefix | `AGENT_*` |
| URI scheme | `agent://` |
| metric name prefix | `agent_` |
| manifest version key | `agent_version` |
| `_meta` namespace | `agent/*` |
| capabilities entrypoint | `--capabilities` (neutral; binary name impl-specific) |

Rules enforced by the schemas:

- The manifest root requires **`agent_version`**.
- `report.schema.json` `distillate_ref` matches **`^agent://`**.
- Every metric in `metrics.registry.json` carries only the neutral `name`. Every env var in
  `env-convention.json` carries the `AGENT_*` name. Same for `agent://` resources in
  `management-profile.json`.

Codegen targets the single neutral scheme.

---

## Provisional / open items

These are the unresolved contract asks (design record §14). Each was given the best-grounded
choice and recorded:

- **P10 — metric-name reconciliation (RESOLVED, provisional).** The source-emitted RFC 0016
  names are canonical. `agent_pending_events` is the primary autoscaling signal;
  `agent_reactive_backlog` (RFC 0019) is recorded as a scaling alias. `agent_tokens_per_sec`
  and `agent_intelligence_latency_ms` are marked provisional/not-emitted. The three histograms
  (`agent_run_duration_ms`, `agent_intel_call_duration_ms`, `agent_tool_call_duration_ms`) are
  provisional with **buckets undefined** (the reference emits no histograms). P10 does not
  affect manifest shape — `metrics_schema` is an opaque version string.
- **P2 — A2A wire strings (OPEN).** Both spellings are recorded on every method:
  `reference_method` (PascalCase `a2a.*` over JSON-RPC NDJSON — what `dispatch_a2a` routes
  today) and `spec_method` (A2A slash-form: `message/send`, `tasks/get`, `tasks/cancel`,
  `tasks/list`, `message/stream`, `tasks/resubscribe`). **The normative spelling is deferred**
  to a later contract decision; a gateway translates until then. Sub-question: the reference
  A2A surface shares the management listener and omits a dedicated `address` (the manifest
  `surfaces.a2a.address` is optional).
- **P6 — `--config-schema` (RESOLVED-in-source, provisional).** The design record flagged it
  unbuilt; the config-file extraction found `config_schema()` IS implemented. `surfaces.config_schema`
  is kept boolean; a `true` value is contract-declared and now source-backed.
- **P4 — `agent://metrics` text body + `agent://capacity` schema (OUT OF SCOPE / downstream).**
  Undefined upstream. The manifest `surfaces.metrics` carries only the scrape address; the
  byte-identical Prom-text resource and the capacity schema are deferred to a future resources
  extraction. Recorded as a downstream dependency.

Additional grounded choices: operator-tools are the **5-tool source set**
`[drain, lame-duck, pause, resume, cancel]` in frozen order (not the design record's 3;
`attach` = `subagent.send`, not a tool; no `force` tool). Exit-code version is the bare
source string `"1.0"` (the `RFC-0011-§5` literal is a documentation alias). The config-file
*input* surface is intentionally **closed** (`additionalProperties:false`, mirroring serde
`deny_unknown_fields` → a typo'd key is exit 2) — this is the deliberate exception to the
open-object rule, since config input is validated, not discovered.

---

## How agentctl consumes the contract

agentctl **codegens** a typed `agent-contract-client` from these schemas (agentctl RFC 0018).
The three sources of truth are the published JSON Schemas here, the reference source, and the
behavioral conformance suite. Codegen notes:

1. Point the resolver at **`schemas/`** — the single canonical set, every `$id` unified.
2. The **sum-type fields** (`management`, `metrics`, `a2a`, `claim`, `shard`,
   `intelligence.healthy`) need **hand-written deserializers** — codegen cannot derive `oneOf`
   bool|string|object discriminations (RFC 0018 §3.3).
3. **Tolerate unknown additive content** (open objects, unknown surface keys, unknown operator
   tools/metrics). Refuse only an unknown `contract_version` MAJOR.
4. Target the **neutral spellings** — the contract is fully neutral (see neutral-wire map).
5. Treat `build_features` as opaque diagnostic metadata — branch on `surfaces{}`, never on a
   feature token.

## How a new agent self-certifies

A new agent is conformant by **behavior**, not by sharing code with the reference:

1. Emit a manifest from `--capabilities` that validates against `schemas/manifest.schema.json`,
   declaring `contract_version` and an honest `surfaces{}` block.
2. Honor the frozen **exit-code table** (`schemas/exit-codes.table.json`) and emit a
   run-outcome report that validates against `schemas/report.schema.json`.
3. For each surface declared `true`/served in `surfaces{}`, serve it per the matching schema:
   management tools (`management-profile.json`), metrics (`metrics.registry.json`,
   `metrics_schema` 1.0), A2A (`a2a.methods.json`), config (`config.schema.json`), the
   `agent://events` stream (`events.schema.json`).
4. Honor the downward-API env convention (`env-convention.json`) and the `agent://` resource
   naming.
5. Pass the behavioral conformance suite. The golden fixtures in `fixtures/capabilities/` are
   the validation ground-truth: `default.json` and `full-features.json` are **real captures**
   from the reference `agentd` binary (carrying `agent_version` with value `1.0.0`) and together
   exercise both branches of every sum-type surface key.

---

## Open question P0 — neutral home

This contract currently lives inside the agentctl repo working tree. Per P0 it should be
extracted into its **own neutral repository** so that neither agentctl nor any agent owns it.
The `$id` namespace (`https://agentctl.dev/contract/v1/...`) anticipates a stable published
home; the directory layout (`schemas/`, `fixtures/`, `VERSION`) is ready to lift wholesale.
