# Agent Control Contract (ACC) — v1 spec

The normative, human-readable companion to the JSON Schemas in `schemas/`. The
schemas are authoritative for *shape*; this document explains the **rules,
nuances, and frozen catalogues** a conformant agent and any consumer must honour.
For the principle (P0), the de-branding map, and the codegen-consumption notes,
see [`README.md`](README.md); for the anti-drift pipeline see agentctl RFC 0018.

> **One-line model.** agentctl drives *any* binary that emits a conformant
> capabilities **manifest**, honours the frozen **exit-code table**, serves the
> surfaces it **declares**, and speaks the declared **wire protocols** — never a
> specific agent. `agentd` is the *reference* implementation, not a dependency.

---

## 1. The artifacts

Nine files. Two categories — do not confuse them:

| File | Category | Validates |
|---|---|---|
| `manifest.schema.json` | **document validator** | the capabilities manifest (the discovery spine) |
| `config.schema.json` | **document validator** | the declarative agent config file |
| `report.schema.json` | **document validator** | the run-outcome report |
| `events.schema.json` | **document validator** | the `agent://events` read body |
| `metrics.registry.json` | data catalogue | — (frozen Prometheus registry; `/metrics` is *text*, not JSON) |
| `a2a.methods.json` | data catalogue (+ wire-type `$defs`) | — (A2A method registry + `Task`/`Message`/`Part` shapes) |
| `exit-codes.table.json` | data catalogue | — (frozen code → intent table) |
| `management-profile.json` | data catalogue | — (operator tools/resources, PeerOrigin) |
| `env-convention.json` | data catalogue | — (downward-API env vars) |

Every `$id` is `https://agentctl.dev/contract/v1/<file>` — the `v1` is the
**contract major version**, not a directory. All `$ref`s are file-internal
(`#/$defs/*`).

---

## 2. The eight cross-cutting laws

These govern every artifact. They are the contract's load-bearing rules.

### L1 — Open-object additive tolerance (one exception)
Every object is `additionalProperties: true`, and every enum-like array
(`build_features`, `operator_tools`, `a2a.methods`, `claim.styles`,
`mcp_server.tags`) is **open strings, not a closed enum**. A future additive
field/tool/metric/value **deserializes instead of erroring**. Applying
`additionalProperties:false` / `deny_unknown_fields` to a discovery surface is
**forbidden**.
**The single exception** is `config.schema.json` — it is **closed**, mirroring
serde `deny_unknown_fields`, so an operator typo (`max_token` vs `max_tokens`) is
caught as **exit 2**. Config is *input* validation, not discovery.

### L2 — `surfaces{}` is the single discovery point
The manifest's `surfaces{}` block is the *only* place a consumer learns what is
served. **A key absent ⇒ that surface is unbuilt ⇒ degrade gracefully** (absence
is never an error). Consequence: **never branch on `build_features`** — it is
opaque diagnostic metadata; behaviour keys off `surfaces{}` alone.

### L3 — Version negotiation: refuse unknown MAJOR only
All version keys are `major.minor` (`^\d+\.\d+$`). **Additive growth ⇒ MINOR bump
(consumers MUST tolerate); breaking change ⇒ MAJOR bump (consumers refuse only an
unknown major).** Sub-schemas (`metrics_schema`, `report_schema`, `config_schema`)
version independently within a known contract major. See [§6](#6-version-keys).

### L4 — De-branding (P0): neutral canonical, branded alias, both accepted to GA
The reference emits branded spellings; the contract canonicalizes neutral ones and
keeps branded as **accepted aliases**, dropped only at a GA cutover:

| concern | neutral (canonical) | branded alias |
|---|---|---|
| env prefix | `AGENT_*` | `AGENTD_*` |
| URI scheme | `agent://` | `agentd://` |
| metric prefix | `agent_` | `agentd_` |
| manifest version key | `agent_version` | `agentd_version` |
| `_meta` namespace | `agent/*` | `agentd/*` |

The manifest root `anyOf`-requires **`agent_version` OR `agentd_version`**.
Consumers MUST accept both spellings; codegen MUST NOT hardcode one scheme.

### L5 — Secret-freedom is structural
The manifest **never** carries credentials — `intelligence` is structural only
(`transport` scheme + `endpoints` count + `healthy`), never a URL or token. The
config file carries only **references** (`{{secret:NAME}}` / `{{secret-file:PATH}}`),
never resolved values. Credentials travel **only** the `*_TOKEN[_FILE]` env path —
never the identity downward-API path, never the config file. (Root cause: the
reference `Secret` type has no `Serialize`.)

### L6 — Three distinct encodings of "not set"
- **`false`** — `surfaces.management` / `surfaces.metrics` / `surfaces.a2a` use a
  literal `const false` on the off-branch (`true` would *not* validate them).
- **omitted** — `surfaces.claim` is **omitted-when-absent, never false**; so are
  `intelligence_summary.max_context_hint`, `surfaces.a2a.address`, report
  `instance` / `trace_id`, A2A `Task.artifacts` (until COMPLETED).
- **`null`** — `surfaces.shard` (`"K/N" | null`), `intelligence.transport`,
  `model`, status `exit_disposition` (`null | int`).

### L7 — Transport is authority (PeerOrigin)
`peer_origin` is closed `{Stdio, Management}` and **reachability *is* authorization**
in v1 (no in-band auth): `stdio → Stdio`, `unix:PATH → Management`,
`vsock:[CID:]PORT → Management`. A non-Management caller invoking an operator
tool/resource gets **`-32601` METHOD_NOT_FOUND**. On the stdio (work) transport
only `agent://capabilities` is visible.

### L8 — "Source wins" on RFC-vs-implementation divergence
Where an RFC sketch and the reference source disagree, the contract takes the
*source*, and the behavioral conformance suite keeps it honest. Examples:
`exit_codes` version is `"1.0"` (not the sketch's `"RFC-0011-§5"`); `Limits` has 3
file keys (not the RFC's 5); `agent_tokens_total` carries only `{type}` (not
`{model,type}`); the rendered metric name wins over a prose name.

---

## 3. The capabilities manifest (the spine)

Emitted from `--capabilities` (one-shot) and the live `agent://capabilities`
resource — the two MUST be **semantically equal as parsed JSON** (not byte-equal:
a compacted vs pretty-printed surface is conformant).

**Required root:** `contract_version`, `build_features`, `identity`, `mode`,
`intelligence`, `surfaces` — plus (`agent_version` OR `agentd_version`).

- `contract_version` — `major.minor`; reference `"1.0"`; negotiate on major (L3).
- `mode` — **open** string; reference set `once | loop | reactive | schedule`;
  tolerate unknown.
- `identity` — required `run_id` (always present; ULID synthesized if unset). K8s
  fields `instance` / `uid` / `node` / `namespace` are `string|null` (empty env →
  null). Descriptive, never load-bearing for placement.
- `intelligence` — required `transport` (`unix|vsock|https|null`), `endpoints`
  (integer ≥ 0), `healthy` (**`bool | "unknown"`** sum type). No URL/token.
- `limits` — open object, all-integer values (`max_depth`, `max_children`,
  `max_total_subagents`, `max_steps`, `max_tokens`, `tree_token_budget`,
  `deadline_ms`, `drain_timeout_ms`).
- `mcp_servers[]` — `{name, tags}` structural only (never the argv); `a2a_peers[]`
  — `{name, transport}` (never the endpoint).

### The `surfaces{}` sum types
`surfaces` is `additionalProperties:true` (mandatory). The non-boolean keys are
sum types requiring **hand-written deserializers** (the only hand-maintained code
in the generated client):

| key | type | meaning |
|---|---|---|
| `management` | `false \| string` | mgmt transport addr (`"unix:…"`, `"vsock:5005"`) |
| `metrics` | `false \| string` | scrape addr (`":9090"`) |
| `a2a` | `false \| {version, streaming, methods[]}` | A2A surface (provisional, P2) |
| `claim` | `bool \| {styles[]}` | **omitted-when-absent, never false** |
| `shard` | `string \| null` | `"K/N"`, else null (unsharded *or* no cluster) |
| `intelligence.healthy` | `bool \| "unknown"` | `"unknown"` on the pre-connect probe |

Plain keys: `operator_tools[]`, `metrics_schema`, `report_schema`, `exit_codes`,
`events`, `intelligence`, `hot_reload`, `config_validate`, `config_schema`,
`cluster`, `standby`.

---

## 4. The frozen catalogues

### 4.1 Operator tools & resources (`management-profile.json`)
**Frozen order:** `drain, lame-duck, pause, resume, cancel`. `attach` is **not** a
tool (it maps to `subagent.send`); there is **no `force`** tool (force = a second
SIGTERM). Tools advertise as `[]` without a management transport.

Behaviours (asserted by conformance, not mere presence):
- `drain` ≡ SIGTERM ≡ the supervised graceful exit → **clean exit 0** (not 143);
  idempotent + monotonic; `deadline_ms` is silently clamped to ≤ `drain_timeout_ms`.
- `lame-duck` flips readiness to NotReady **without exiting** (reversible).
- `pause` / `resume` suspend/resume at a turn boundary (reflected in `agent_paused`).
- `cancel handle` — `"0"`/omitted = the whole run (root subtree); runs the kill ladder.

Resources (neutral / alias): `agent://capabilities`, `agent://inventory`,
`agent://status`, `agent://events`, `agent://run/{run_id}`. Subscribable resources
notify-then-read (`notifications/resources/updated{uri}`, no payload). PeerOrigin
is closed `{Stdio, Management}` (L7).

### 4.2 Exit-code table (`exit-codes.table.json`)
Intent vocabulary (closed): `complete, terminal, retriable, policy, infra`. Each
code's `intent` compiles into the Job `podFailurePolicy`.

| code | name | intent | notes |
|---|---|---|---|
| 0 | `EXIT_OK` | complete | **clean drain is 0, not 143** |
| 1 | `EXIT_FAILURE` | retriable | |
| 2 | `EXIT_USAGE` | terminal | config invalid; never reachable from a report |
| 3 | `EXIT_PARTIAL` | policy | remappable via `--budget-exit-code` |
| 4 | `EXIT_INTELLIGENCE` | retriable | |
| 5 | `EXIT_SEMANTIC` | terminal | refused |
| 6 | `EXIT_MCP` | retriable | |
| 7 | `EXIT_BUDGET` | policy | remappable via `--budget-exit-code` |
| 124 | `EXIT_TIMEOUT` | policy | defined but **unreachable** (folded into 7) |
| 137 | `SIGKILL_EXIT` | infra | kernel-set (`128+9`); binary never returns it |
| 143 | `SIGTERM_EXIT` | infra | kernel-set (`128+15`); **only the *ungraceful* exit** |

`os_set:true` only for 137/143. Only codes **3 and 7** are operator-remappable.
An unrecognised code defaults to `retriable`.

### 4.3 Run-outcome report (`report.schema.json`)
Open object, 12 required keys. `mode` is `once | loop | schedule` — **never
`reactive`** (reactive daemons emit no report). `status` is the closed 9-set:
`completed, refused, exhausted_steps, exhausted_tokens, deadline, stalled,
loop_detected, cancelled, crashed`. `usage` carries **tokens, never currency**
(cost = tokens × a price table the consumer owns). `distillate_ref` *points*
(`^(agent|agentd)://…`), it does not embed the body. `has_usable_partial` (a
result-body property, not a status) drives the 3-vs-7 exit split.

### 4.4 Metrics registry (`metrics.registry.json`)
Prefix `agent_` / `agentd_`. **46 records — 29 stable, 8 legacy, 9 provisional.**
Prometheus `0.0.4` text (hand-rendered; no version label on `/metrics` — version
is `surfaces.metrics_schema`).
**Cardinality rule: bounded labels only** — never `run_id` / `agent_id` /
`agent_path` / URI as a label. Each closed label domain ends in an `other`
overflow slot (except `token_type` = `in|out`):
`status`, `refusal_reason` (`trifecta,rate,budget,depth,mcp,other`), `limit`,
`restart_reason`, `stuck_signal`, `intel_error_reason`, `drain_phase`,
`reload_result`. The `server` label is bounded by a 16-slot intern table → `other`.

Gotchas: **all histograms are provisional and not emitted**; **`agent_saturation`
is the only float** (stored basis points ÷ 10000 — breaks uniform-`u64` codegen);
`agent_pending_events` is canonical, `agent_reactive_backlog` is its scaling alias
(P10); `agent_memory_*` are omitted (not 0) when the cgroup field is absent.

### 4.5 A2A method set (`a2a.methods.json`)
**11 methods, recorded in both spellings** — reference PascalCase (`a2a.SendMessage`)
↔ A2A-spec slash-form (`message/send`); the normative binding is **open (P2)**, so
a gateway translates. `served_by`:

- **6 live (agent-served):** `SendMessage, GetTask, CancelTask, ListTasks,
  SendStreamingMessage, SubscribeToTask`.
- **5 gateway-owned:** the `…PushNotificationConfig` quartet + `GetAuthenticatedExtendedCard`
  (the agent returns `-32601` for these — it is **stateless**; durable history &
  push config live in the gateway).

Closed error set: `TASK_NOT_FOUND -32001`, `METHOD_NOT_FOUND -32601`,
`INVALID_PARAMS -32602`, `INTERNAL_ERROR -32603`. Transport: JSON-RPC 2.0 over
NDJSON, Management-gated, substrate `vsock|unix`.

Terminal task states: `completed, failed, canceled, rejected`. Status→A2A mapping
is closed: `completed→completed`, `refused→rejected`, `cancelled→canceled`,
`{exhausted_*, deadline, stalled, loop_detected, crashed}→failed`,
`running→working`. A COMPLETED task carries **exactly one** artifact
`<taskId>.distillate`. **Streaming is status-level framed, not unary:** for one
request id the agent emits several same-id `{result: StreamResponse}` frames —
read them as a stream until `statusUpdate.final == true`.

### 4.6 Event stream (`events.schema.json`)
Read body of `agent://events`: a lossy, fixed-size ring (default 1024, drops oldest
→ bumps `dropped`). `level` is closed (`trace, debug, info, warn, error`); `comp`
is closed (`supervisor, agent, mcp, intel`); but **`event` is an open string** —
a 27-name vocabulary exists in `$defs.event_name_v1` but is deliberately **not
`$ref`'d**, so unknown event names stay additive-tolerant. `seq` is the only field
distinguishing a ring entry from the raw stderr JSON line. `events_schema` versions
the *envelope* only — the line schema is owned/versioned separately.

### 4.7 Config file (`config.schema.json`)
The **only closed** object (L1). No required root keys (an empty `{}` is valid).
Reloadable keys: `model, max_tokens, limits, mcp_servers, subscribe, a2a_peers,
log_level, intelligence_headers`. Restart-only keys **declared for warning**:
`mode, interval, cron` (changing one on a live reload is rejected; a typo'd
*reloadable* key is still exit 2). Frozen enums: `log_level`
(`trace,debug,info,warn,error`), `mode` (`once,loop,reactive,schedule`),
`McpServer.transport` (`stdio,unix`), MCP `tags` (`untrusted_input, sensitive,
egress`; untagged ⇒ `untrusted_input`). `mode=schedule` requires `interval` OR
`cron`; `cron` is valid only with `mode=schedule`. List keys **replace** at the
file layer but repeatable flags **add**. Precedence: default < file < env < flag.
`HeaderValue` is a sum type (literal string | `{{secret:…}}` template); a
credential-shaped header name MUST use a template or validation fails exit 2.

### 4.8 Env convention (`env-convention.json`)
Downward-API identity (`AGENT_RUN_ID/POD_NAME/POD_UID/POD_NAMESPACE/NODE_NAME`
from `fieldRef`). **All vars optional; empty coerces to unset** (`Some("")` never
produced); identity is descriptive; the agent never calls the K8s API.
`run_id` is the one field never absent (ULID minted when unset, stable across a
retried Job). Sharding: `AGENT_SHARD="K/N"` (parse rejects `N==0`/`K>=N` →
EXIT_USAGE 2), `AGENT_SHARD_TIMER=shard0|keyed`. Credentials are 1-indexed by
endpoint position (`AGENT_INTELLIGENCE_TOKEN` = endpoint 1; `…_TOKEN_{N}`
1-indexed; each with a `_FILE` rotation variant), kept strictly separate from the
identity and config paths (L5).

---

## 5. Sum types (the hand-written-deserializer set) {#5-sum-types}

Codegen cannot derive these `oneOf` discriminations; each needs a hand-written
deserializer that **retains** an unknown additive form rather than erroring:

| field | shape |
|---|---|
| `intelligence.healthy` | `bool \| "unknown"` |
| `surfaces.management` | `false \| string` |
| `surfaces.metrics` | `false \| string` |
| `surfaces.a2a` | `false \| object` |
| `surfaces.claim` | `bool \| object` (omitted, never false) |
| `surfaces.shard` | `string \| null` |
| config `HeaderValue` | literal `string \| {{secret:…}}` template |
| status `exit_disposition` | `null \| integer` |

---

## 6. Version keys {#6-version-keys}

All `major.minor`; refuse unknown major, tolerate additive minor (L3).

| key | where | reference |
|---|---|---|
| `contract_version` | manifest root | `1.0` |
| `metrics_schema` | `surfaces.metrics_schema` | `1.0` |
| `report_schema` | report root / `surfaces.report_schema` | `1.0` |
| `events_schema` | events root | `1.0` |
| `config_version` | config root (optional) | — |
| `exit_codes_version` | exit-codes table / `surfaces.exit_codes` | `1.0` (plain string, no pattern) |
| `protocol_version` | a2a.methods root | `1.0` (free string) |

---

## 7. Sharpest gotchas

1. **`143 ≠ graceful termination`** — a clean drain returns **0**; 143 is only the
   forced (past-budget) exit.
2. **`build_features` is a trap** — never branch on it; only `surfaces{}` is normative.
3. **Three "off" encodings** — `surfaces.claim` is *omitted*, `management/metrics/a2a`
   use literal `false`, `shard` uses `null`. Don't conflate them.
4. **Config is the only closed object** — a config typo is exit 2 *by design*;
   everywhere else, unknown keys are tolerated.
5. **`reactive` mode emits no report** — and the report `mode` enum excludes it.
6. **A2A streaming is framed, not unary** — same-id frames until `final`; the open
   method-name binding (P2) is the gateway's to translate.
7. **The data catalogues are not validators** despite the `$schema` header — they
   are frozen reference data for codegen.
8. **6 of 11 A2A methods are agent-served; 5 are the gateway's** — the "stateless
   agent + stateful gateway" split (durable task store, push config) is *in the
   contract*, not an implementation choice.
9. **`agent_saturation` is a float; everything else is an integer** — uniform-`u64`
   metric codegen breaks on it.

---

## 8. Conformance

Shape is necessary but not sufficient — a binary that parses but misbehaves is
**non-conformant**. The behavioral conformance suite (agentctl RFC 0018 §7) is the
executable definition of "a conformant agent": it drives a real binary over the
substrate and asserts the *behaviours* above (`drain ≡ exit 0`, metric presence
after warm-up, negotiation/degradation, the exit-code table under induced
failures), reading the required-vs-optional partition **from these artifacts**, not
from any one implementation. The golden fixtures in `fixtures/capabilities/` are
the validation ground-truth.
