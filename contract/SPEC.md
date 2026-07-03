# Agent Control Contract (ACC) — Specification

The normative, field-by-field companion to the JSON Schemas in [`schemas/`](schemas/). The schemas are
authoritative for **shape**; this document specifies the **rules, catalogues, and behaviors** a
conformant agent — and any consumer — must honor. For the overview, the neutral-token map, and how to
conform, see [`README.md`](README.md).

**One-line model.** agentctl drives *any* binary that emits a conformant capabilities **manifest**,
honors the frozen **exit-code table**, serves the surfaces it **declares**, and speaks the declared
**wire protocols**. `agentd` is the reference implementation, not a dependency.

The contract version is **2.0**. A conformant agent serves its control surface over mTLS HTTPS
(`POST /mcp`) and dials the control-plane gateways with no embedded credential; identity is a verified
mTLS client certificate (Management) or an attested source (the gateways).

---

## 1. The artifacts

Nine files in two categories. Every `$id` is `https://agentctl.dev/contract/v1/<file>` (the `v2` is the
contract major version, not a directory); every `$ref` is file-internal (`#/$defs/...`).

| File | Category | Validates / carries |
|---|---|---|
| `manifest.schema.json` | document validator | the capabilities manifest (the discovery spine) |
| `config.schema.json` | document validator | the declarative agent config file |
| `report.schema.json` | document validator | the run-outcome report |
| `events.schema.json` | document validator | the `agent://events` read body |
| `metrics.registry.json` | data catalogue | the frozen Prometheus registry (`/metrics` is *text*, not JSON) |
| `a2a.methods.json` | data catalogue | the A2A method registry + `Task`/`Message`/`Part` wire types |
| `exit-codes.table.json` | data catalogue | the frozen code → intent table |
| `management-profile.json` | data catalogue | operator methods/resources + PeerOrigin gating |
| `env-convention.json` | data catalogue | the downward-API env vars |

A **data catalogue** carries the standard schema header and passes the metaschema, but its payload is
frozen reference data for codegen — it is not an instance validator.

---

## 2. The cross-cutting laws

These govern every artifact.

### L1 — Open-object additive tolerance (one exception)

Every object is `additionalProperties: true`, and every enum-like array (`build_features`,
`operator_tools`, `a2a.methods`, `claim.styles`, `mcp_server.tags`) is **open strings, not a closed
enum**. A future additive field, method, metric, or value **deserializes instead of erroring**.
Applying `additionalProperties: false` / deny-unknown to a discovery surface is forbidden.

**The single exception** is [`config.schema.json`](schemas/config.schema.json): it is **closed**
(`additionalProperties: false`), so an operator typo (`max_token` for `max_tokens`) is caught as
**exit 2**. Config is input validation, not discovery.

### L2 — `surfaces{}` is the single discovery point

The manifest's `surfaces{}` block is the *only* place a consumer learns what is served. **A key absent
means that surface is unbuilt, so the consumer degrades gracefully** — absence is never an error.
Consequently, **never branch on `build_features`**; it is opaque diagnostic metadata, and behavior keys
off `surfaces{}` alone.

### L3 — Version negotiation: refuse only an unknown MAJOR

All version keys are `major.minor` (pattern `^\d+\.\d+$`). Additive growth bumps MINOR (consumers must
tolerate it); a breaking change bumps MAJOR (consumers refuse only an unknown major). The sub-schemas
(`metrics_schema`, `report_schema`, `events_schema`, `exit_codes`) version independently within a known
contract major. See [Version keys](#10-version-keys).

### L4 — Neutral wire

The contract defines only the neutral spellings (`AGENT_*`, `agent://`, `agent_`, `agent_version`,
`agent/*`), so any agent can implement it. The manifest root **requires `agent_version`**. Codegen
targets the single neutral scheme.

### L5 — Secret-freedom is structural

The manifest **never** carries credentials — `intelligence` is structural only (transport scheme +
endpoint count + reachability), never a URL or token. The config file carries only **references**
(`{{secret:NAME}}` / `{{secret-file:PATH}}`), never resolved values. Credentials travel **only** the
`AGENT_*_TOKEN[_FILE]` env path — never the identity downward-API path, never the config file.

### L6 — Three distinct encodings of "not set"

- **`false`** — `surfaces.management`, `surfaces.metrics`, and `surfaces.a2a` use a literal `const false`
  on the off-branch (a boolean `true` does **not** validate them).
- **omitted** — `surfaces.claim` is omitted when absent, never `false`; so are
  `intelligence_summary.max_context_hint`, `surfaces.a2a.address`, and report `instance` / `trace_id`.
- **`null`** — `surfaces.shard` (`"K/N" | null`), `intelligence.transport`, `model`, and
  `agent://status.exit_disposition` (`null | integer`).

Do not conflate the three.

### L7 — Identity is authority (PeerOrigin)

`peer_origin` is the closed set `{Stdio, Management}`. A caller that presents a client certificate the
mTLS acceptor **verified against the pinned client CA** is `Management`; the agent's own in-process
driving harness is `Stdio`. There is no lesser wire origin — a request on the HTTPS listener with no
verified certificate is unauthenticated and **refused**, never downgraded. A non-Management caller that
invokes an operator method or resource receives **`-32601` (METHOD_NOT_FOUND)**; only
`agent://capabilities` is visible to a `Stdio` caller. The A2A gateway relays to the agent under the
control-plane client certificate, so gateway-forwarded A2A work arrives as `Management`.

### L8 — Source is authoritative

Where a prose description and the reference source disagree on a detail, the contract takes the source,
and the behavioral conformance suite keeps it honest. Examples: the exit-code version is `"1.0"`; the
rendered metric name wins over a prose name; `agent_tokens_total` carries only `{type}`.

---

## 3. The capabilities manifest

Emitted from the one-shot `--capabilities` entrypoint and from the live `agent://capabilities`
resource — the two must be **semantically equal as parsed JSON** (a compacted vs pretty-printed body is
conformant). Validator: [`manifest.schema.json`](schemas/manifest.schema.json).

**Required root keys:** `contract_version`, `agent_version`, `build_features`, `identity`, `mode`,
`intelligence`, `surfaces`.

| Field | Type | Notes |
|---|---|---|
| `contract_version` | string `^\d+\.\d+$` | reference `"2.0"`; negotiate on major. |
| `agent_version` | string | build/version string; free-form (reference emits `1.0.0`). |
| `build_features` | string[] | opaque diagnostic tokens; **never branch on a value**. |
| `identity` | object | downward-API identity; descriptive only (see below). |
| `mode` | string (open) | reference set `once \| loop \| reactive \| schedule`; tolerate unknown values. |
| `model` | string \| null | operator-declared model id; metadata, never a secret. |
| `intelligence` | object | structural binding (see below). |
| `intelligence_summary` | object (optional) | advisory hints (`toolmode`; optional `max_context_hint`). |
| `mcp_servers` | object[] (optional) | `{name, tags}` — structural only, never an endpoint or headers. |
| `a2a_peers` | object[] (optional) | `{name, transport}` — `https \| unknown`, never an endpoint. |
| `exec_enabled` | boolean (optional) | the reference reports `false`; no local exec surface is served. |
| `allow_trifecta` | boolean (optional) | whether the lethal-trifecta combination is permitted. |
| `limits` | object (optional) | integer bounding box (see below). |
| `surfaces` | object | **the single discovery point** (required). |

**`identity`** requires `run_id` (always present — a ULID is synthesized if unset). The Kubernetes
fields `instance` / `uid` / `node` / `namespace` are `string | null` (an empty env value coerces to
`null`). Identity is descriptive; it is never load-bearing for placement.

**`intelligence`** requires `transport` (`https | null`; `null` when no endpoint is configured or the
scheme is unrecognized), `endpoints` (integer ≥ 0), and `healthy` (the sum type `bool | "unknown"` —
`"unknown"` on the pre-connect probe). It carries no URL and no token.

**`limits`** is an open, all-integer object: `max_depth`, `max_children`, `max_total_subagents`,
`max_steps`, `max_tokens`, `tree_token_budget`, `deadline_ms`, `drain_timeout_ms`.

### 3.1 The `surfaces{}` block

`surfaces` is `additionalProperties: true`. The non-boolean keys are sum types requiring hand-written
deserializers:

| Key | Type | Meaning |
|---|---|---|
| `management` | `false \| string` | management address — an mTLS HTTPS URL (e.g. `"https://0.0.0.0:8443"`), else `false`. |
| `metrics` | `false \| string` | scrape address (e.g. `"0.0.0.0:9090"`), else `false`. |
| `a2a` | `false \| {version, streaming, methods[]}` | the A2A surface; advertises the **compiled** capability, independent of any bound listener. |
| `claim` | `bool \| {styles[]}` | claim styles (reference `["tool", "resource"]`); **omitted when absent, never `false`**. |
| `shard` | `string \| null` | `"K/N"` shard identity, else `null` (unsharded or no cluster). |

Plain keys: `operator_tools[]`, `metrics_schema`, `report_schema`, `events_schema`, `exit_codes`,
`events` (bool), `intelligence` (bool), `hot_reload`, `config_validate`, `config_schema`, `cluster`,
`standby`.

When `surfaces.a2a` is an object it requires `version`, `streaming`, and `methods`; the optional
`address` key is present only if a build serves A2A on a dedicated listener (the reference shares the
management mTLS listener and omits it).

### 3.2 Example (real capture — once mode, listeners off)

Abridged from [`fixtures/capabilities/default.json`](fixtures/capabilities/default.json):

```json
{
  "contract_version": "1.0",
  "agent_version": "1.0.0",
  "build_features": ["tls", "serve-mcp", "serve-https", "a2a", "metrics"],
  "mode": "once",
  "identity": { "run_id": "19f25a529241ed9b0", "instance": null, "namespace": null, "node": null, "uid": null },
  "intelligence": { "transport": null, "endpoints": 0, "healthy": "unknown" },
  "limits": { "max_steps": 50, "max_depth": 4, "max_tokens": 200000, "deadline_ms": 600000, "drain_timeout_ms": 25000 },
  "surfaces": {
    "management": false,
    "metrics": false,
    "a2a": { "version": "1.0", "streaming": true,
             "methods": ["SendMessage", "GetTask", "CancelTask", "ListTasks", "SendStreamingMessage", "SubscribeToTask"] },
    "claim": { "styles": ["tool", "resource"] },
    "operator_tools": ["a2a.Drain", "a2a.LameDuck", "a2a.Pause", "a2a.Resume", "a2a.Cancel"],
    "metrics_schema": "1.0", "report_schema": "1.0", "exit_codes": "1.0",
    "shard": null, "cluster": true, "standby": false,
    "events": false, "hot_reload": true, "config_validate": true, "config_schema": true, "intelligence": true
  }
}
```

Note that `management`/`metrics` are `false` (no listener bound) while `a2a` is an object: the A2A key
advertises the compiled capability independent of a bound listener. In a fully-served build (see
[`full-features.json`](fixtures/capabilities/full-features.json)) `management` becomes
`"https://0.0.0.0:8443"`, `metrics` becomes `"0.0.0.0:9090"`, `events` becomes `true`, and `shard`
becomes `"0/3"`.

---

## 4. The MCP management profile

Catalogue: [`management-profile.json`](schemas/management-profile.json). The operator admin methods are
served on the mTLS `POST /mcp` listener and mirrored in `surfaces.operator_tools`.

### 4.1 Operator methods (frozen order)

`a2a.Drain, a2a.LameDuck, a2a.Pause, a2a.Resume, a2a.Cancel`. The `a2a.` prefix marks them operator
extensions, distinct from the bare A2A-protocol methods in [A2A over HTTPS](#5-a2a-over-https). They advertise as
`[]` when the build cannot serve the management surface. `attach` is **not** a method (it maps to the
`subagent.send` work tool); there is **no `force`** method (forcing a drain past budget is a second
SIGTERM).

| Method | Input | Behavior |
|---|---|---|
| `a2a.Drain` | `{ deadline_ms? }` (silently clamped to ≤ `drain_timeout_ms`) | Identical to receiving SIGTERM: the supervised graceful exit. Idempotent and monotonic. **A clean drain exits 0, not 143.** |
| `a2a.LameDuck` | `{ ready?: bool = false }` | Flips readiness toward NotReady without exiting; reversible; performs no drain. |
| `a2a.Pause` | `{}` | Suspends the tree at a turn boundary; reflected in `agent_paused`. |
| `a2a.Resume` | `{}` | Inverse of pause; resumes turn execution. |
| `a2a.Cancel` | `{ handle, reason? }` | `handle` `"0"` or omitted means the root subtree (the whole run); runs the kill ladder. |

These behaviors are asserted by the conformance suite, not merely by presence.

### 4.2 Operator resources

`agent://` resources served on the management surface:

| Resource | Subscribable | Visible to `Stdio` | Notes |
|---|---|---|---|
| `agent://capabilities` | yes | **yes** | the capabilities manifest / base Agent Card; the only resource a `Stdio` caller may read. |
| `agent://inventory` | yes | no | |
| `agent://status` | yes | no | identity-bearing; `exit_disposition` is `null \| integer(code)`. |
| `agent://events` | no | no | the bounded ring stream (see [The event stream](#8-the-event-stream)). |
| `agent://run/{run_id}` | — | no | embeds the run-outcome report under `report`. |
| `agent://intelligence` | — | no | per-endpoint intelligence health. |

Subscribable resources notify-then-read: a payload-free `notifications/resources/updated{uri}` fires,
and the subscriber then reads the resource.

### 4.3 PeerOrigin gating

`peer_origin` is the closed set `{Stdio, Management}` (see [L7](#l7--identity-is-authority-peerorigin)).

| Origin | Source | Work tools | Operator methods | Operator resources |
|---|---|---|---|---|
| `Stdio` | the in-process work loop | yes | no | `agent://capabilities` only |
| `Management` | an mTLS peer whose client cert chains to the pinned client CA | yes | yes | all |

An mTLS peer with no verified client certificate is unauthenticated and refused — it is not an origin.

**agentctl consumption.** The apiserver serves the management verbs (`drain`, `lame-duck`, `pause`,
`resume`, `cancel`) by invoking these methods on the target pod over mTLS as the Management origin,
authorizing each caller by SubjectAccessReview; fleet verbs fan out to all replicas.

---

## 5. A2A over HTTPS

Catalogue: [`a2a.methods.json`](schemas/a2a.methods.json). The agent serves the A2A (Agent2Agent)
JSON-RPC method set over the **same mTLS HTTPS listener** as management (`POST /mcp`; SSE for
streaming), gated to a `Management`-origin caller. The normative wire name is the **bare PascalCase**
form (`SendMessage`, `GetTask`, …); the legacy `a2a.`-prefixed spelling is accepted for back-compat.

### 5.1 The method set

The agent serves the **live task core** and is otherwise **stateless**: durable history, version
negotiation, OAuth/OIDC, webhooks, and push-notification config live in the gateway. A gateway-owned
method invoked on the agent returns `-32601`.

| Method | `served_by` | Streaming | Notes |
|---|---|---|---|
| `SendMessage` | live | no | `configuration.returnImmediately` defaults `true` → an async WORKING task to poll; `false` blocks to a terminal task. |
| `GetTask` | live | no | unknown id → `-32001`; a completed run carries the distillate artifact. |
| `CancelTask` | live | no | a live run → CANCELED; an already-terminal task returns its real terminal state; unknown id → `-32001`. |
| `ListTasks` | live | no | the live instance-local registry only; pagination accepted, `nextPageToken` omitted. |
| `SendStreamingMessage` | live | **yes** | status-level SSE stream (see below). |
| `SubscribeToTask` | live | **yes** | resubscribe to an existing run by id. |
| `SetTaskPushNotificationConfig` | gateway | — | agent → `-32601`. |
| `GetTaskPushNotificationConfig` | gateway | — | agent → `-32601`. |
| `ListTaskPushNotificationConfig` | gateway | — | agent → `-32601`. |
| `DeleteTaskPushNotificationConfig` | gateway | — | agent → `-32601`. |
| `GetAuthenticatedExtendedCard` | gateway | — | agent → `-32601`; the base card is the capabilities manifest. |

### 5.2 Error codes (closed set)

| Code | Name | Raised when |
|---|---|---|
| `-32001` | `TASK_NOT_FOUND` | `GetTask` / `CancelTask` / `SubscribeToTask` for an id not in the live registry. |
| `-32004` | `UNSUPPORTED_OPERATION` | e.g. `CancelTask` on an already-terminal task. |
| `-32601` | `METHOD_NOT_FOUND` | an unsupported method, a `Stdio`-origin caller, or a gateway-owned method. |
| `-32602` | `INVALID_PARAMS` | e.g. `SendMessage` with no non-empty text part. |
| `-32603` | `INTERNAL_ERROR` | spawn/dispatch failure. |

### 5.3 Task states and mapping

The four **terminal** states are `completed`, `failed`, `canceled`, `rejected`; `submitted`/`working`
are in-flight. The agent's terminal status maps to an A2A task state through a closed mapping:

| Terminal status | A2A task state |
|---|---|
| `completed` | `completed` |
| `refused` | `rejected` |
| `cancelled` | `canceled` |
| `exhausted_steps`, `exhausted_tokens`, `deadline`, `stalled`, `loop_detected`, `crashed` | `failed` |
| `running` (synthetic) | `working` |

A `Task` requires `id` and `status`. A **COMPLETED** task carries **exactly one** artifact — the
distillate, with `artifactId` `"<taskId>.distillate"` and one text `Part`. `artifacts` is absent on
working/failed/canceled/rejected. `history` is gateway-held (the agent is stateless).

### 5.4 Streaming

Streaming is **status-level framed, not unary**. For one streaming request id the agent emits several
same-id JSON-RPC reply frames over an SSE `text/event-stream`, each carrying a `StreamResponse`
(`statusUpdate` or `artifactUpdate`). A `SendStreamingMessage` run writes a WORKING `statusUpdate`,
then (on completion) an `artifactUpdate` with the distillate, then a terminal-state `statusUpdate`, and
closes the stream. **A consumer keys termination off the terminal task state plus stream close** — the
reference emits no non-spec `final` flag.

### 5.5 Wire types

The core shapes (in `a2a.methods.json` `$defs`): `Message` (requires `parts` ≥ 1; the reference reads
only text parts), `Part` (the reference produces only the `{text}` variant; others are tolerated),
`Artifact`, `TaskStatus`, `Task`, `TaskStatusUpdateEvent`, `TaskArtifactUpdateEvent`, and
`StreamResponse`. The agent advertises `protocol_version` `"1.0"` in `surfaces.a2a.version`.

**agentctl consumption.** The A2A gateway projects and signs each agent's (and each fleet's) Agent Card
from the manifest, serves `message/send` and `message/stream` to external callers, persists tasks, and
relays to the agent over mTLS as the Management origin. A fleet is addressable as one endpoint: the
coordinator is the front door, else the gateway load-balances across worker replicas with task
affinity.

---

## 6. The metrics registry

Catalogue: [`metrics.registry.json`](schemas/metrics.registry.json). `metrics_schema` is **1.0**.
Metrics are Prometheus `0.0.4` **text** exposition (hand-rendered — do not validate it as JSON), on the
opt-in `/metrics` surface gated by `surfaces.metrics`. `/metrics` carries no version label; the version
is discovered out-of-band via `surfaces.metrics_schema`.

The registry holds **51 series**: **36 stable**, **8 legacy** (retained additively; prefer the stable
replacement), and **7 provisional** (declared but **not emitted** by the reference). By type: 29
counters, 19 gauges, and 3 histograms — **all three histograms are provisional and unbuilt**, with
bucket boundaries undefined; a consumer treats them as absent unless a future surface advertises them.

### 6.1 Cardinality rule

**Bounded labels only.** Never carry `run_id` / `agent_id` / `agent_path` / call id / session id / URI
as a label. Each closed label domain ends in an `other` overflow slot (except `token_type` = `in|out`).
The `server` label is bounded structurally by a 16-slot intern table; overflow folds into
`server="other"`.

| Domain | Values |
|---|---|
| `status` | `completed, refused, exhausted_steps, exhausted_tokens, deadline, stalled, loop_detected, cancelled, crashed, other` |
| `token_type` | `in, out` |
| `refusal_reason` | `trifecta, rate, budget, depth, mcp, other` |
| `limit` | `steps, tokens, deadline, depth, tree_tokens, restart_storm, spawn_rate, other` |
| `restart_reason` | `crashed, stuck, rate, other` |
| `stuck_signal` | `term, kill, other` |
| `intel_error_reason` | `unreachable, auth, timeout, 5xx, other` |
| `drain_phase` | `started, completed, forced, other` |
| `reload_result` | `applied, rejected, other` |

### 6.2 Signal subsets

The registry marks each series with `autoscaling_signal` and `cost_signal` flags.

**Autoscaling signals:** `agent_pending_events` (the primary backlog gauge; the alias
`agent_reactive_backlog` names the same signal), `agent_reaction_lag_ms`, `agent_saturation`,
`agent_active_subagents`, `agent_inflight_reactions`, `agent_claims_lost_total`,
`agent_shard_skipped_total`.

**Cost signals (named subset):** `agent_tokens_total`, `agent_intel_calls_total`,
`agent_tokens_input_total`, `agent_tokens_output_total`. (`agent_tokens_per_sec` also carries the cost
flag but is provisional and not emitted.)

### 6.3 Notable rules

- **`agent_saturation` is the only float** — a gauge in `[0,1]` (stored as basis points ÷ 10000). A
  codegen that assumes a uniform integer value type breaks on it.
- **`agent_tokens_total` carries only `{type}`** (`in|out`), not a `{model}` label.
- **`agent_memory_max_bytes` / `agent_memory_current_bytes`** are emitted only when the kernel exposes
  the cgroup v2 field, and are **omitted** (not zero) otherwise.
- Drain and lame-duck state is observable via `agent_ready == 0` plus `agent_drains_total{phase}`;
  there is no standalone `agent_draining` or `agent_lame_duck` gauge.

**agentctl consumption.** Every component and agent exposes `/metrics`, scraped directly by Prometheus.
The scaler and autoscalers target the backlog signals so reactive/claim workloads scale from zero;
dashboards, alerts, and scalers are codegenned from the registry.

---

## 7. The exit-code table

Catalogue: [`exit-codes.table.json`](schemas/exit-codes.table.json). `exit_codes_version` is **1.0**
(the plain `major.minor` string; the value at `surfaces.exit_codes`). agentctl compiles each code's
`intent` into the Job `podFailurePolicy` / `onExitCodes`.

The **intent vocabulary** is the closed set `complete, terminal, retriable, policy, infra`. An
unrecognized code defaults to `retriable` — never a silent FailJob.

| Code | Name | Intent | Notes |
|---|---|---|---|
| 0 | `EXIT_OK` | complete | success / loop clean bound / **clean drain (0, not 143)**. |
| 1 | `EXIT_FAILURE` | retriable | generic/unspecified failure (also maps cancelled and crashed). |
| 2 | `EXIT_USAGE` | terminal | config/usage error before any side effect; **never reachable from a report**. |
| 3 | `EXIT_PARTIAL` | policy | a usable partial was emitted; operator-remappable. |
| 4 | `EXIT_INTELLIGENCE` | retriable | intelligence endpoint unreachable / auth error after retries. |
| 5 | `EXIT_SEMANTIC` | terminal | the run refused (concluded the task cannot be done). |
| 6 | `EXIT_MCP` | retriable | a required MCP server failed connect/handshake or died. |
| 7 | `EXIT_BUDGET` | policy | hit max-steps / max-tokens / deadline / tree budget; operator-remappable. |
| 124 | `EXIT_TIMEOUT` | policy | hard wall-clock deadline via the supervisor kill ladder. |
| 137 | `SIGKILL_EXIT` | infra | `128+9`, kernel-set (OOM / hard kill); the binary never returns it. |
| 143 | `SIGTERM_EXIT` | infra | `128+15`, kernel-set; the **ungraceful** exit only. |

Only codes **3** and **7** are operator-remappable, via `--budget-exit-code`. `os_set` is `true` only
for 137 and 143. A bounded run's terminal mapping folds a wall-clock deadline into `EXIT_BUDGET (7)`,
while `EXIT_TIMEOUT (124)` is produced by the supervisor's hard-kill ladder; both codes are reachable.

The `agent://status.exit_disposition` field is `null` until terminal, then the integer code.

---

## 8. The run-outcome report

Validator: [`report.schema.json`](schemas/report.schema.json). `report_schema` is **1.0**. A
`once`/`loop`/`schedule`-bounded run writes this object at the terminal transition; **reactive daemons
emit no report** (they have no single terminal outcome). Two optional delivery surfaces: a report file
written atomically (env `AGENT_REPORT_FILE` / flag `--report-file PATH`) and the served
`agent://run/{run_id}` resource (which embeds this object under `report`). A failed report write never
changes the exit code — the exit code is the floor contract.

**12 required keys:** `report_schema`, `run_id`, `mode`, `status`, `exit_code`, `has_usable_partial`,
`usage`, `duration_ms`, `started_at`, `ended_at`, `distillate_ref`, `refusals`.

| Field | Type | Notes |
|---|---|---|
| `mode` | `once \| loop \| schedule` | closed; **never `reactive`**. |
| `status` | closed 9-set | `completed, refused, exhausted_steps, exhausted_tokens, deadline, stalled, loop_detected, cancelled, crashed`. |
| `exit_code` | integer | the coarse projection of `status` (see [The exit-code table](#7-the-exit-code-table)). |
| `has_usable_partial` | boolean | a result-body property (not a status); drives the 3-vs-7 exit split. |
| `usage` | object | requires `tokens_in`, `tokens_out`, `steps`, `subagents` — **tokens, never currency** (absence is 0, never an estimate). |
| `duration_ms`, `started_at`, `ended_at` | integer ms / UTC `date-time` | timestamps are UTC with millisecond precision; duration is clamped to 0 on a non-monotonic clock. |
| `distillate_ref` | string `^agent://` | **points** to the result body (e.g. `agent://subagent/0/result`); it does not embed it. |
| `refusals` | object | requires `trifecta`, `rate`, `budget`, `depth`, `mcp` (this run's counts). |
| `instance` | string (optional) | **omitted when absent, never null**. |
| `trace_id` | string `^[0-9a-f]{32}$` (optional) | W3C trace id; **omitted when absent, never null**. |

**agentctl consumption.** The report is the durable, structured backend for `kubectl agents results`: a
Job's pod is gone seconds after exit, so the outcome is captured here, not inferred from a vanished pod.

---

## 9. The event stream

Validator: [`events.schema.json`](schemas/events.schema.json). `events_schema` is **1.0**; it versions
the **envelope** only, not the line schema. The read body of `agent://events` is a subscribable,
fixed-size in-memory ring (default 1024, env `AGENT_EVENTS_RING`) that is **lossy by design**: an
overrun drops the oldest line and bumps `dropped`, never blocking a slow subscriber.

**Envelope** (required `events_schema`, `oldest_seq`, `newest_seq`, `dropped`, `events`). Each entry in
`events` is one telemetry line plus a monotonic ring `seq` — the only field added over the raw stderr
line and the subscriber's cursor key.

**Read request:** `resources/read("agent://events?after=<seq>&level=<level>&event=<dotted-prefix,...>")`.
`after` defaults to 0 (the whole held window); `level` filters by severity; `event` is a comma-list of
dotted **prefixes** matched against the line `event`. Unknown query keys are ignored; a malformed
`after` falls back to 0. The reference caps one read window at 512 entries, so a subscriber pages by
advancing `after`.

**Line fields** (required `seq`, `ts`, `level`, `event`, `run_id`, `agent_id`, `agent_path`, `comp`,
`pid`): `level` is the closed set `trace, debug, info, warn, error`; `comp` is the closed set
`supervisor, agent, mcp, intel`. **`event` is an open string** — a frozen 27-name vocabulary exists
in `$defs.event_name_v1` for reference but is deliberately not enforced, so an unrecognized event name
stays additive-tolerant. Event-specific fields (`tool`, `server`, `tokens_in`, `depth`, …) ride
`additionalProperties`; a consumer ignores fields it does not recognize.

---

## 10. The config file

Validator: [`config.schema.json`](schemas/config.schema.json). This is **the only closed object** — a
typo'd key is **exit 2**. There are no required root keys (an empty `{}` is valid). Precedence:
**default < file < env < flag.** List keys **replace** at the file layer; repeatable flags **add**.

| Key | Type | Notes |
|---|---|---|
| `config_version` | string | optional metadata. |
| `model` | string | operator-declared model id. |
| `max_tokens` | integer ≥ 1 | per-run token cap. |
| `log_level` | enum | `trace, debug, info, warn, error`. |
| `model_swap` | enum | `finish-on-old, restart-turn`. |
| `limits` | object | `deadline_secs` (≥ 0), `max_depth` (≥ 0), `max_steps` (≥ 1). |
| `intelligence` | string | the intelligence endpoint. **Restart-only** — applied at startup, not on hot-reload. |
| `intelligence_headers` | object<string,string> | header templates; a credential-shaped value uses a `{{secret:NAME}}` template. |
| `mcp_servers` | object[] | each `{name, endpoint, headers?, tags?}` (see below). |
| `a2a_peers` | object[] | each `{name, endpoint, headers?, client_cert?, client_key?}`. |
| `subscribe` | string[] | resource URIs to subscribe to. |

`mcp_servers[]` entries require `name` (`^[a-zA-Z0-9_-]+$`) and `endpoint` (a remote HTTPS endpoint —
there is no transport key), with optional `headers` and `tags`. **`tags` is an object** whose values are
arrays drawn from the closed set `untrusted_input, sensitive, egress`. (Note the manifest advertises MCP
tags as a flat string array; the config file carries this object form.)

`a2a_peers[]` entries require `name` and `endpoint`, with optional `headers` (secret-free auth
templates) and mutual-TLS `client_cert` / `client_key` PEM file paths.

**Secret references.** Header values are strings that may be `{{secret:NAME}}` or
`{{secret-file:PATH}}` templates; a credential never appears literally, and validation fails (exit 2) if
a credential-shaped header carries a raw value.

**Run-shape inputs are not config-file keys.** `mode`, `interval`, `cron`, and the workflow file are
startup-only CLI/env inputs (`--mode` / `AGENT_MODE`, `--interval`, `--cron`, `--workflow`), so the
config file cannot change the run shape. The hot-reloadable subset is every config key **except**
`config_version` and `intelligence`; agentctl applies it on SIGHUP when `surfaces.hot_reload` is
advertised.

**agentctl consumption.** The operator renders this config from the CRD spec, mounts it, and (when
`surfaces.config_validate` is advertised) validates it via `--validate-config` before rollout.

---

## 11. The downward-API env convention

Catalogue: [`env-convention.json`](schemas/env-convention.json). All values arrive via env
(`valueFrom.fieldRef`); the agent **never calls the Kubernetes API**. **Every var is optional**, an
empty value coerces to **unset**, and identity is descriptive (no placement decision derives from it).
Credentials use **only** the `*_TOKEN[_FILE]` path — never the identity path, never the config file.

| Group | Vars | Notes |
|---|---|---|
| Identity | `AGENT_RUN_ID`, `AGENT_POD_NAME`, `AGENT_POD_UID`, `AGENT_POD_NAMESPACE`, `AGENT_NODE_NAME` | from the downward API; `AGENT_RUN_ID` is the one field never absent (a ULID is minted if unset, stable across a retried Job). |
| Sharding | `AGENT_SHARD` (`"K/N"`, `0 ≤ K < N`, `N ≥ 1`), `AGENT_SHARD_TIMER` (`shard0 \| keyed`), `AGENT_STANDBY` | a malformed `AGENT_SHARD` (`N==0` / `K>=N`) fails as `EXIT_USAGE (2)`; maps to `surfaces.shard`/`surfaces.standby`. |
| Credentials | `AGENT_INTELLIGENCE_TOKEN[_FILE]` (endpoint 1), `AGENT_INTELLIGENCE_TOKEN_{N}[_FILE]` (1-indexed) | per-endpoint; the `_FILE` variant is a mounted-secret path (rotation-friendly); never logged. |
| Lifecycle | `AGENT_SERVE_MCP` (mTLS HTTPS management URL), `AGENT_INTELLIGENCE` (keyless HTTPS URL to the gateway), `AGENT_MODE`, `AGENT_MODEL`, `AGENT_DRAIN_TIMEOUT` | restart-only; `AGENT_SERVE_MCP` drives `surfaces.management`. |

The serving-TLS material (leaf cert, key, the client CA the agent verifies peers against, and the
outbound trust anchor) is supplied as **filesystem paths to mounted certificates** via flags
(`--serve-cert` / `--serve-key` / `--serve-client-ca` / `--tls-ca`), never inline in env.

`AGENT_POD_GRACE_SECONDS` is reserved for the drain-versus-grace hint and is not currently read by the
reference agent.

---

## 12. The sum types

Codegen cannot derive these; each needs a hand-written deserializer that **retains** an unknown
additive form rather than erroring:

| Field | Shape |
|---|---|
| `intelligence.healthy` | `bool \| "unknown"` |
| `surfaces.management` | `false \| string` |
| `surfaces.metrics` | `false \| string` |
| `surfaces.a2a` | `false \| object` |
| `surfaces.claim` | `bool \| object` (omitted, never `false`) |
| `surfaces.shard` | `string \| null` |
| config `header value` | literal string \| `{{secret:…}}` template |
| `agent://status.exit_disposition` | `null \| integer` |

---

## 13. Version keys

All are `major.minor`; refuse only an unknown major, tolerate an additive minor.

| Key | Where | Reference |
|---|---|---|
| `contract_version` | manifest root | `2.0` |
| `metrics_schema` | `surfaces.metrics_schema` | `1.0` |
| `report_schema` | report root / `surfaces.report_schema` | `1.0` |
| `events_schema` | events root / `surfaces.events_schema` | `1.0` |
| `exit_codes` | `surfaces.exit_codes` / `exit_codes_version` | `1.0` |
| `protocol_version` | `surfaces.a2a.version` | `1.0` |
| `config_version` | config root (optional) | — |

---

## 14. Conformance

Shape is necessary but not sufficient. The behavioral conformance suite is the executable definition of
a conformant agent: it drives a real binary and asserts the behaviors above — `drain ≡ exit 0`, metric
presence after warm-up, negotiation and graceful degradation, and the exit-code table under induced
failures — reading the required-versus-optional partition from these artifacts, not from any one
implementation. The golden fixtures in [`fixtures/capabilities/`](fixtures/capabilities/) are the
validation ground-truth.

### Sharpest gotchas

1. **`143 ≠ graceful termination`.** A clean drain returns **0**; 143 is only the forced (past-budget)
   exit.
2. **`build_features` is a trap.** Never branch on it; only `surfaces{}` is normative.
3. **Three "off" encodings.** `surfaces.claim` is *omitted*; `management`/`metrics`/`a2a` use literal
   `false`; `shard` uses `null`.
4. **Config is the only closed object.** A config typo is exit 2 by design; everywhere else, unknown
   keys are tolerated.
5. **`reactive` mode emits no report** — and the report `mode` enum excludes it.
6. **A2A streaming is framed, not unary.** Read same-id SSE frames until a terminal task state plus
   stream close; there is no `final` flag.
7. **The data catalogues are not validators** despite the schema header — they are frozen reference
   data for codegen.
8. **The A2A split is in the contract.** Six of eleven methods are agent-served; five are the gateway's
   (durable history, push config).
9. **`agent_saturation` is a float; every other series is an integer.**
