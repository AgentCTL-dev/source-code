# agentctl RFC 0020: Instruction source & live delivery — the `instructionSource` union, the resolver, and hot-reloadable instructions

**Status:** Proposed (agentctl tools/identity track)
**Author:** Andrii Tsok
**Date:** 2026-07-01
**Part of:** the agentctl control plane — how an `Agent`/`AgentFleet` sources its **instruction** from more than an inline string: a ConfigMap/Secret key, a mounted file, a fetched URL (typed, authenticated, polled), or a **live MCP resource** that keeps the instruction current by subscription — delivered secret-free and hot-reloaded without a restart

> **Contract-first, not agent-first (P0).** A conformant agent is handed **an
> instruction** at provisioning time (`--instruction` / `--instruction-file` /
> `INSTRUCTION` env — a **startup input**, `agentd .../config.rs`; instruction is **not**
> in the contract's reloadable subset today, `config.schema.json` `x-reloadable`). This
> RFC owns **where that instruction comes from** and **how it is kept current** — the
> sourcing today, and *hot* (no-restart) refresh once the instruction is made reloadable
> (**P-instr-file**, §4) — for **any** agent that conforms to the contract's config
> surface. It MUST NOT encode one data-plane binary's config internals. Where a concrete
> shape is needed it cites the **reference implementation** (`agentd` v1.0.0 —
> `AgentSpec.instruction: Option<String>` today, `crates/agent-api/src/lib.rs`; the
> `--instruction-file` flag) as *where the contract is presently written down*, never as
> a dependency. The agent-branded surfaces this plane resolves into (the instruction
> input, the reload trigger, the `agent://` scheme) are contract-normative-but-branded —
> flagged for neutralization under the P0 contract-extraction open question (agentctl RFC
> 0001 §9).

> **The split with RFC 0019 is load-bearing.** This RFC owns the **instruction
> delivery** — the `instructionSource` union, the **resolver** that materializes each
> source into the instruction the agent reads, the poll/subscribe refresh, the reload
> semantics, and the injection-surface security. RFC 0019 owns the
> **identity & authentication of the MCP server** that the `mcpResource` source
> subscribes to. When an instruction is sourced from an MCP resource, this RFC says
> *how the live text becomes the agent's instruction and when it reloads*; RFC 0019
> says *how the subscription to that server is authenticated, secret-free*. One
> MCP-identity model, two consumers (RFC 0019 §11).

> **Optional, never load-bearing for v1 usability.** The inline `instruction: <string>`
> that ships today (`AgentSpec.instruction`) is `instructionSource: { inline: … }`
> under this RFC — the **default**, unchanged, requiring none of the resolver
> machinery. Every other source is an **opt-in convenience/decoupling seam**; an
> `Agent` runs on day one with a plain inline instruction and nothing here engaged
> (brainstorm §16 — not on the Phase-0 critical path).

---

## 1. Problem / Context

An agent's instruction is its system prompt — *what it is and what it should do*.
Today it is an **inline string** and nothing else: `AgentSpec.instruction:
Option<String>` (`crates/agent-api/src/lib.rs`), "Inline instruction. Required for
non-reactive modes." That is correct for a hello-world agent and wrong for how
instructions are actually authored and operated at fleet scale:

1. **Instructions are authored elsewhere and reused.** A team maintains a prompt in a
   ConfigMap, a Git-backed file, a prompt-management service behind a URL, or a
   living document. Copy-pasting it into every `Agent.spec.instruction` is the same
   anti-pattern RFC 0004 removed for ops config: every edit is a fleet-wide `kubectl
   apply`, and drift is silent.

2. **Instructions change *while the agent runs*.** A reactive daemon or a long-lived
   fleet should pick up a corrected instruction with as little disruption as possible —
   yet today an instruction is a **startup input** (`--instruction` / `--instruction-file`
   / `INSTRUCTION`), **not** in the contract's reloadable subset (`config.schema.json`
   `x-reloadable` = `[intelligence, model_swap, model, max_tokens, limits, mcp_servers,
   subscribe, a2a_peers, log_level, intelligence_headers]` — no `instruction`), so
   changing it means editing the CRD and rolling the pod. And there is no way to
   *source* a new value at all short of that edit. The most valuable case — an
   instruction that a **living document keeps current** — has no expression, and no
   no-restart path (this RFC adds the sourcing; **P-instr-file**, §4, adds the
   no-restart reload).

3. **A sourced instruction may need a credential, and the pod must not hold it.** A
   URL-sourced or MCP-sourced instruction may sit behind Basic auth, a bearer header,
   or an OAuth-protected MCP server. In the hostile-tenant posture (brainstorm §0.6)
   that fetch credential must **never** land in the agent pod — the same zero-secret
   rule intelligence (RFC 0012) and MCP tools (RFC 0019) already obey.

4. **A sourced instruction is untrusted input — a first-class injection surface.** An
   instruction fetched from a URL or an MCP resource is **externally-controlled data
   that becomes the agent's system prompt**. That is a prompt-injection vector by
   construction, and in the lethal-trifecta model an externally-sourced instruction is
   an **`untrusted_input`** leg (agentd RFC 0012 §3.1). This must be named, allow-listed,
   and provenance-bounded — not bolted on.

The user's ask names all of it: *"add instructions also by url, defining the
mime/type, using basic or an auth header, with a pull interval, or a file path, or by
defining an MCP server and the resource uri so it can update automatically by itself
via MCP resource subscription."* The resolution is a small, closed **union of
instruction sources** plus a **resolver** that materializes any source into the one
thing the contract already consumes — a reloadable instruction — holding any fetch
credential off the pod and treating the sourced text as untrusted.

---

## 2. Decision — the instruction-source plane (eight principles)

1. **`instruction` becomes a closed union `instructionSource` (§3).** The six variants:
   `inline` (a string — today's behaviour, the default), `configMapKeyRef`,
   `secretKeyRef`, `file` (a mounted path), `url` (typed + authenticated + polled), and
   `mcpResource` (subscription-driven live). Exactly one is set (CEL). The bare
   `instruction: <string>` shorthand desugars to `instructionSource: { inline: … }`, so
   **every existing manifest is valid unchanged**.

2. **Delivery is always through the agent's instruction input — the agent never
   fetches (§4).** Whatever the source, the **resolver** (§5) materializes it into the
   instruction the agent is handed. For **static** sources that is the agent's
   **startup instruction** (`--instruction-file` / `--instruction` / `INSTRUCTION` env —
   all exist today; no contract change). For **live** sources, applying a new value
   **without a restart** requires the instruction to be **reloadable at runtime** —
   which the contract does **not** provide today (instruction is a startup input, not in
   the reloadable subset) — so hot live reload is contract ask **P-instr-file** (§4), and
   until it lands a live update is applied by a **managed roll** (§4). Either way the
   agent's job is unchanged: read an instruction. No agent learns about URLs, ConfigMaps,
   or MCP subscriptions — the same "control-plane resolves, data-plane consumes a thin
   primitive" discipline as intelligence (RFC 0012) and MCP (RFC 0019).

3. **A resolver controller materializes and refreshes each source (§5).** For static
   sources (`inline`/`configMapKeyRef`/`secretKeyRef`/`file`) resolution is a
   one-shot render at reconcile + a watch on the referenced object. For **live**
   sources (`url` with `pollInterval`, `mcpResource` with `subscribe`) a resolver
   **keeps the instruction fresh** — polling on a cadence or holding an MCP
   subscription — and writes each new value into the reloadable instruction atomically.

4. **Secretless: a fetch credential lives in the resolver, never the pod (§6, §8.1).**
   A `url` source's Basic/bearer credential (`authSecretRef`) and an `mcpResource`
   server's OAuth credential (RFC 0019's broker) are held by the **resolver / broker**,
   off the agent pod. The agent only ever sees the **resolved instruction text**, never
   the credential that fetched it — the zero-secret rule extended from egress
   (models/tools) to *ingress* (instructions).

5. **`url` sources are typed, authenticated, polled, and bounded (§6).** `url` carries
   a declared `mimeType` (validated on fetch), an optional `auth` (`basic` |
   `header`), an optional `pollInterval` (absent ⇒ fetch-once), a size cap, and
   caching/backoff. A fetch failure **holds the last-good instruction** and degrades
   observably — never blanks the agent's prompt.

6. **`mcpResource` is subscription-driven live instruction, composed with RFC 0019
   (§7).** An `mcpResource` names an `MCPServerSet` server (RFC 0019) + a resource
   `uri` + `subscribe: true`. The resolver holds an MCP **resource subscription** (the
   same MCP capability the contract's reactive mode uses) through RFC 0019's broker —
   so the server is authenticated secret-free — and on each `resources/updated`
   notification writes the new resource contents as the instruction and signals reload.
   *"Updates by itself"* = the living document drives the agent with no redeploy.

7. **A sourced instruction is `untrusted_input`; admission allow-lists it (§8).** Any
   `url`/`mcpResource` (and any external `file`) instruction is treated as an
   **`untrusted_input`** trifecta leg (agentd RFC 0012 §3.1) and contributes to the
   `Agent`-level trifecta union (RFC 0004 §5.3). Admission (agentctl RFC 0007)
   enforces a **host/scheme allow-list** for `url`, requires TLS for remote fetches,
   caps size, and rejects cross-namespace `Secret`/`ConfigMap` refs without a grant.
   Provenance (source URI + fetched digest) is recorded in `Agent.status`.

8. **Reload is atomic, change-gated, and turn-boundary-safe (§9).** The resolver writes
   each new instruction via an atomic swap and applies it at the agent's next turn/run
   boundary — **hot (no restart) where the instruction is reloadable (P-instr-file),
   else via a managed roll** (§4/§9); `once` in-flight runs always finish on the old
   instruction. A malformed/empty resolved value is **rejected** and the last-good
   retained. The applied instruction's digest + timestamp are surfaced in `Agent.status`
   for auditability.

### 2.1 What this RFC owns vs reuses (the boundary)

| Concern | Home | Note |
|---|---|---|
| The `instructionSource` **union schema** (6 variants, CEL, desugaring) | **agentctl RFC 0020 (here)** | extends `AgentSpec.instruction` (agent-api) |
| The **resolver** (materialize + poll/subscribe + atomic write + status) | **agentctl RFC 0020 (here)** | a controller/sidecar; implemented per RFC 0006 |
| The agent's **instruction input** — startup `--instruction-file`/`--instruction`/`INSTRUCTION` (today) + the **reloadable** `--instruction-file` hot live reload needs (**P-instr-file**) | **the contract** (agentd RFC 0017; `config.rs`; `config.schema.json` `x-reloadable`) | the resolver writes the value; the agent reads it (hot) or is rolled (interim) |
| The **MCP server identity/auth** an `mcpResource` subscribes to (the broker, the OAuth, secret-free) | **agentctl RFC 0019** | this RFC consumes it; §7 |
| The **MCP resource-subscription** protocol capability the resolver uses to watch | **the contract** (MCP resources/subscribe; agentd reactive `subscribe`) | the resolver is the subscriber, not the agent |
| **Admission** (url host/scheme allow-list, TLS-required, size cap, cross-ns ref rejection, trifecta union) | **agentctl RFC 0007** | this RFC specifies the rules; 0007 executes |
| **Renderer** compiling a source into the resolver config + the reloadable instruction wiring | **agentctl RFC 0006** | this RFC specifies *what* it renders |
| The **five-class secret lifecycle** the `url` `authSecretRef` lives under | **agentctl RFC 0015** | the fetch credential is a managed secret class |
| The **trifecta tag vocabulary** + per-spawn budget an untrusted instruction feeds | **the contract** (agentd RFC 0012 §3.1/§3.2) | agentctl surfaces the `untrusted_input` leg |

---

## 3. The `instructionSource` union — schema

`instructionSource` replaces the bare `instruction: <string>` on `Agent`/`AgentFleet`
with a closed, one-of union. The scalar form is retained as sugar.

### 3.1 Full schema

```yaml
# On Agent / AgentFleet .spec — exactly one variant (CEL); the scalar `instruction: "..."`
# is accepted as sugar for `instructionSource: { inline: "..." }`.
spec:
  instructionSource:

    # (1) inline — today's behaviour, the default; no resolver needed
    inline: "Triage inbound issues and open PRs."

    # (2) configMapKeyRef — reuse a namespace-local ConfigMap key
    configMapKeyRef: { name: triage-prompt, key: instruction.md }

    # (3) secretKeyRef — a confidential instruction (rare; the VALUE is confidential,
    #     distinct from a URL's FETCH credential in §6)
    secretKeyRef: { name: triage-prompt-secret, key: instruction.md }

    # (4) file — a path the platform mounts (Git-sync sidecar, CSI, projected volume)
    file: { path: /etc/agent/instruction.md, watch: true }   # watch ⇒ reload on file change

    # (5) url — typed, authenticated, polled (the fetch credential is off-pod, §6/§8.1)
    url:
      href: https://prompts.example.com/triage/v3
      mimeType: text/markdown            # declared + validated on fetch (text/plain|markdown|…)
      auth:                              # optional; the credential is held by the RESOLVER, never the pod
        mode: header                     # basic | header
        header:
          name: Authorization
          valueSecretRef: { name: prompts-token, key: bearer }   # namespace-local Secret → resolver only
        # mode: basic → basic: { userSecretRef: {name,key}, passwordSecretRef: {name,key} } (also resolver-only)
      pollInterval: 5m                   # absent ⇒ fetch once at provisioning; set ⇒ live refresh (§6)
      maxBytes: 262144                   # size cap (default from policy); over-cap ⇒ reject, keep last-good
      onError: keepLastGood              # keepLastGood (default) | failReady

    # (6) mcpResource — subscription-driven live instruction (composes with RFC 0019, §7)
    mcpResource:
      serverRef: { name: docs, serverSetRef: engineering-tools }   # an MCPServerSet server (RFC 0019 §3)
      uri: doc://living/triage-runbook   # the MCP resource URI
      subscribe: true                    # true ⇒ resolver holds a resources/subscribe; false ⇒ read-once
      mimeType: text/markdown

# status (operator/resolver-written, DeepEqual-guarded)
status:
  instruction:
    source: url                          # which variant is active
    ref: https://prompts.example.com/triage/v3
    appliedDigest: "sha256:1a2b…"        # digest of the currently-applied instruction (provenance)
    appliedAt: "2026-07-01T10:05:00Z"
    live: true                           # a poll/subscription is active
    untrustedInput: true                 # this source contributes an untrusted_input trifecta leg (§8)
  conditions:
    - { type: InstructionResolved, status: "True", reason: Applied }
    # False reasons: SourceUnreachable (keepLastGood in effect) | MimeMismatch | OverSizeCap |
    #                DisallowedHost | McpServerUnready (RFC 0019)
```

### 3.2 Field reference (the non-obvious ones)

| Field | Type | Notes |
|---|---|---|
| `inline` | string | the default; identical to today's `instruction` |
| `configMapKeyRef` / `secretKeyRef` | `{name,key}` | namespace-local; `secretKeyRef` is for a *confidential instruction value*, **not** a fetch credential |
| `file.path` / `file.watch` | path / bool | a platform-mounted file (Git-sync, CSI, projected). `watch:true` ⇒ resolver reloads on inotify change |
| `url.href` | https URL | remote fetch; **TLS required** for non-cluster hosts (admission, §8) |
| `url.mimeType` | string | declared content type; a fetch whose `Content-Type` mismatches ⇒ `MimeMismatch`, keep last-good |
| `url.auth.mode` | enum `basic\|header` | `basic` (`userSecretRef`/`passwordSecretRef`) or `header` (arbitrary header + `valueSecretRef`) — credential mounted into the **resolver** |
| `url.pollInterval` | duration | absent ⇒ fetch-once; set ⇒ live refresh cadence (min bound by policy, §6) |
| `url.maxBytes` / `url.onError` | int / enum | size cap; `keepLastGood` (default) vs `failReady` on fetch error |
| `mcpResource.serverRef` | `{name, serverSetRef}` | a server in an `MCPServerSet` (RFC 0019) — supplies the identity/auth |
| `mcpResource.uri` / `subscribe` | MCP URI / bool | the resource; `subscribe:true` ⇒ live via `resources/updated` |

### 3.3 Validation

- **CEL (single-object):** exactly one `instructionSource.<variant>` is set; `url.href`
  is `https://` (or an in-cluster `http://` allowed only by policy); a non-reactive mode
  still requires *some* instruction source (the existing `mode != reactive ⇒ has
  instruction` rule, generalized to the union). (Whether a source is *live* follows from
  spec — `url.pollInterval` present, or `mcpResource.subscribe: true` — and is reported
  in `status.instruction.live`; it is **not** a CEL invariant, since CEL cannot read
  status.)
- **Webhook (cross-object, agentctl RFC 0007):** the `url.href` host/scheme is on the
  **allow-list**; cross-namespace `configMapKeyRef`/`secretKeyRef`/`auth…SecretRef`
  are rejected without a grant; the `mcpResource.serverRef` resolves to a real server
  in a referenced `MCPServerSet` (RFC 0019); `maxBytes` ≤ the policy ceiling.

---

## 4. Delivery — through the agent's instruction input; the agent never fetches

The invariant that keeps every source cheap and the data plane thin: **the agent
consumes exactly one thing — an instruction — no matter the source.** The resolver
materializes any source into that instruction; *how* the agent receives an *updated*
instruction splits by whether the source is static or live.

```
   instructionSource (any variant)
        │
        ▼  RESOLVER (§5) materializes → validates (mime/size/non-empty) → atomic write
   the agent's instruction  ── STATIC: the startup instruction (--instruction-file /
        │                          --instruction / INSTRUCTION env — all exist today)
        │                     ── LIVE:  a reloadable --instruction-file (P-instr-file),
        │                          else a managed roll (interim)
        ▼
   agent reads it  —  learns nothing about URLs / ConfigMaps / MCP subscriptions
```

**Static sources — no contract change.** `inline`, `configMapKeyRef`, `secretKeyRef`,
and a fetch-once `file`/`url` resolve to a value the operator renders into the agent's
**startup instruction** — the reference agent already accepts `--instruction`,
`--instruction-file <path>`, and the `INSTRUCTION` env (`agentd .../config.rs`). A
change to a static source (a ConfigMap edit, a new fetch-once URL) is applied by the
operator **re-rendering and rolling** the workload — standard controller behaviour, no
runtime reload needed. `inline`/`configMapKeyRef`/`secretKeyRef`/`file` also need no
*live* component (a reconcile-time render + a watch of the referenced object/file).

**Live sources — hot reload needs a contract change (P-instr-file); managed roll is the
interim.** The instruction is **not** in the contract's reloadable subset today — it is
a **startup input**, and `config.schema.json` `x-reloadable` is `[intelligence,
model_swap, model, max_tokens, limits, mcp_servers, subscribe, a2a_peers, log_level,
intelligence_headers]`, with **no `instruction`**. So a `url`-poll / `mcpResource`-
subscribe / watched-`file` update that must apply **without a restart** requires the
contract to make the instruction reloadable:

> **Contract ask P-instr-file.** Make the reference agent's **existing**
> `--instruction-file` **tailed / reloadable** — i.e. add instruction (or the
> instruction-file) to the reloadable subset (agentd RFC 0017 §5.1), so a resolver
> rewrite of that file is picked up at the next turn/run boundary with **no restart**.
> The flag already exists (read-once); the ask is only to make it live. This is the
> **prerequisite for the *hot* live-reload cases** — not an optional cleanup.

Until P-instr-file lands, a live source still works and still *"updates by itself"* —
but the update is applied by a **managed roll**: the resolver detects the changed
content (by digest), writes the new startup instruction, and the operator rolls the
pod(s) (a `once`/`schedule` run picks it up on its next fire; a `reactive`/`loop` daemon
is restarted at a drain boundary). Automatic, but not hot. P-instr-file upgrades exactly
this to no-restart. Static sources are unaffected either way.

---

## 5. The resolver controller — materialize + refresh

The resolver is the component that turns a declarative source into a live instruction.
Its placement follows the same off-pod / not-in-node-agent discipline as the other
egress/ingress components, but it is lighter because it terminates **inbound content**,
not a blocking egress:

- **Placement.** For static + `file` sources, resolution is a **reconcile-time render
  + watch** in the operator (agentctl RFC 0006) — no separate process. For **live**
  sources (`url` polling, `mcpResource` subscription) the refresh loop runs as a
  **per-pod sidecar** (default, simplest isolation — one tenant's fetch credential and
  one subscription per pod) or a **node-local resolver** (shared, for scale), **never**
  the node-agent (same privilege-concentration argument as RFC 0012 §3.1 / RFC 0019
  §4.1 — it would put tenant fetch credentials + externally-controlled instruction
  content in the god-mode host process). The sidecar is the v1 default; a shared
  resolver is §Open.
- **What it does.** (1) resolves the source to bytes; (2) **validates** — mime-type
  match, size ≤ cap, non-empty, UTF-8; (3) computes a digest and, if changed from
  last-applied, **atomically writes** the instruction and applies it — **hot** (signal
  reload) where P-instr-file makes it reloadable, else by requesting a **managed roll**
  (§4); (4) updates `Agent.status.instruction` (source, ref, appliedDigest, appliedAt,
  live).
- **Credential handling.** The `url.auth…SecretRef` (Basic/header) and the
  `mcpResource` server's OAuth credential (via RFC 0019's broker) are mounted into /
  held by the **resolver**, never the agent pod (§8.1). Rotation is a `Secret`/file
  replacement the resolver re-reads live (the RFC 0012 §5.3 file-rotation discipline).
- **Failure posture.** Per `onError`: `keepLastGood` (default) holds the last valid
  instruction and sets `InstructionResolved: False / SourceUnreachable` (observable,
  non-fatal); `failReady` additionally flips the agent **not-ready** (back-pressure)
  until the source recovers — never a crash-loop, never a blank prompt.

---

## 6. `url` sources — typed, authenticated, polled, bounded

The richest static-ish source, and the one the user called out in most detail.

- **Typed.** `mimeType` is declared and **validated** against the response
  `Content-Type` (a mismatch is `MimeMismatch` → keep last-good). Supported v1:
  `text/plain`, `text/markdown` (and a policy-extensible list). A non-text type is
  rejected (an instruction is text).
- **Authenticated, off-pod.** `auth.mode: basic` (`userSecretRef` + `passwordSecretRef`)
  or `auth.mode: header` (an arbitrary header name + `valueSecretRef`, e.g.
  `Authorization: Bearer …` or `X-API-Key: …`). The credential is a **namespace-local
  `Secret` mounted into the resolver** and attached to the outbound fetch — it **never**
  enters the agent pod, appears in the CRD, the manifest, `Agent.status`, or a log
  (§8.1). This is the zero-secret rule applied to instruction ingress.
- **Polled.** `pollInterval` absent ⇒ **fetch-once** at provisioning (a static remote
  instruction). Set ⇒ the resolver refetches on that cadence, using **ETag /
  Last-Modified conditional requests** where offered (a `304` costs nothing and does
  not reload), with jittered backoff on error. A `pollInterval` below the policy floor
  is clamped (admission, §8) to bound fetch load on the source.
- **Bounded.** `maxBytes` caps the response (default from policy); over-cap ⇒ reject +
  keep last-good. The fetch has a timeout and a redirect cap, and (like all egress in
  the hostile posture) is subject to the SSRF/allow-list controls (RFC 0015) —
  enforced at the resolver's egress, or, if the resolver dials through the egress
  broker, there (§8.2).

---

## 7. `mcpResource` — subscription-driven live instruction (composition with RFC 0019)

The most powerful source and the reason RFC 0019 lands first: an instruction that a
**living document keeps current**, authenticated **secret-free** through the MCP broker.

- **Declaration.** `mcpResource.serverRef` names a server in an `MCPServerSet` (RFC
  0019 §3) — so the server's transport, endpoint, and **auth mode** (Tier 1 headless
  or Tier 2 on-behalf-of-user) are exactly the ones RFC 0019 already registered. `uri`
  is the MCP resource; `subscribe: true` requests live updates.
- **The resolver is the subscriber (not the agent).** The resolver opens the MCP
  session **through RFC 0019's broker** (so the OAuth/EMA credential stays off the pod,
  RFC 0019 §4/§6/§7), issues `resources/read` for the initial value, and
  `resources/subscribe` for updates. On each `resources/updated` → `resources/read`,
  it validates + atomically writes the new instruction and applies it (§4). *"Updates
  automatically by itself"* is realized: the document changes, the server notifies, the
  resolver rewrites the instruction, and the agent picks it up at its next boundary —
  **hot (no restart) with P-instr-file, else via a managed roll** (§4) — with no
  `kubectl` in the loop.
- **Why the resolver, not the agent's reactive `subscribe`.** The contract's reactive
  `subscribe` (`AgentSpec.subscribe`) drives **work triggers**, not the **instruction**;
  and routing an instruction update through the agent's own subscription would require
  the agent to hold the server's credential. Making the **resolver** the subscriber
  keeps the agent unchanged and the credential off-pod. The MCP resource-subscription
  *protocol capability* is the same one the contract already uses for reactive mode;
  only the subscriber differs.
- **`subscribe: false`** degrades `mcpResource` to a periodic read (poll semantics like
  `url`) for servers that expose resources but not subscriptions.
- **Identity/auth is entirely RFC 0019's.** If the server is `authReady: false` (its
  AS/IdP unsupported, RFC 0019 §7.4/§12) the source is `McpServerUnready` and the
  resolver keeps last-good. This RFC never re-implements MCP auth — it consumes the
  authenticated session RFC 0019's broker provides.

---

## 8. Security — secretless ingress, allow-listed, injection-aware

### 8.1 Secretless: the fetch credential never enters the pod

The same load-bearing property as the egress planes, applied to **ingress**: a `url`
source's Basic/header credential and an `mcpResource` server's OAuth credential are
held by the **resolver / broker**, and the agent pod sees only the **resolved
instruction text**. A compromised agent pod yields **no** fetch credential — it never
had one. The credential value appears **nowhere** in the CRD, the manifest,
`Agent.status`, resolver logs, or traces (the never-logged discipline, agentd RFC 0006
§6). On the hostile-tenant / networkless tier the resolver is off-pod (node-local) or
the fetch is brokered, so the pod holds neither the credential nor the network path.

### 8.2 The instruction is `untrusted_input` — name it, allow-list it, bound it

An instruction sourced from a `url` or `mcpResource` (or an externally-written `file`)
is **externally-controlled data that becomes the agent's system prompt** — a
prompt-injection surface, and an **`untrusted_input`** trifecta leg by construction
(agentd RFC 0012 §3.1). This RFC makes that explicit and defended:

- **It contributes to the `Agent`-level trifecta union** (RFC 0004 §5.3): an agent with
  a URL-sourced instruction **and** an `egress`-tagged tool **and** a `sensitive`-tagged
  tool composes the full trifecta — surfaced advisorily at admission (RFC 0007) exactly
  as the MCP tag union is, with the contract's per-spawn Rule-of-Two as the real
  control. `status.instruction.untrustedInput: true` records the leg.
- **Admission allow-lists the source** (agentctl RFC 0007): a host/scheme **allow-list**
  for `url.href` (a fetch to an un-listed host is rejected — no arbitrary-URL prompt
  ingestion), **TLS required** for remote hosts, a **size cap** (`maxBytes` ≤ policy
  ceiling), a **poll-interval floor**, and cross-namespace ref rejection. The
  `mcpResource` path inherits RFC 0019's server allow-listing.
- **Provenance is recorded** (`status.instruction.{ref, appliedDigest, appliedAt}`) so
  every applied instruction is attributable to a source + a content digest — an audit
  trail for "what was this agent told to do, from where, when."
- **SSRF controls** (RFC 0015) apply to the resolver's fetch egress, since a
  `url`/`mcpResource` fetch is outbound network from a control-plane-adjacent
  component.

### 8.3 Confidential instructions

`secretKeyRef` (variant 3) is for an instruction whose **value is confidential** (it
lives in a `Secret`, not a ConfigMap). The resolver reads it and writes it into the
reloadable instruction like any other — but the operator SHOULD note that the
instruction value is, by the contract, delivered to the agent as its prompt (it is
*meant* to reach the agent), which is distinct from a *fetch credential* (§8.1) that
must never reach it. The two are not confused: `secretKeyRef` protects the instruction
**at rest / in transit to the resolver**; §8.1 protects a **fetch credential** that the
agent must never see at all.

---

## 9. Reload semantics — atomic, turn-boundary, versioned (hot with P-instr-file)

**Hot (no-restart) reload is contingent on P-instr-file** (§4); where the instruction
is not yet reloadable, the same semantics below hold except that "application" is a
**managed roll** at a drain/run boundary rather than an in-place reload. Either way:

- **Atomic swap.** The resolver writes the new instruction to a temp file and renames
  it into place (atomic on the same filesystem), so the agent never reads a
  half-written prompt. The reload signal (or roll) follows the write.
- **Turn/run-boundary application.** The new instruction takes effect at the agent's
  next turn (reactive/loop) or next run (`once`/schedule) — hot via the reloadable
  `--instruction-file` (P-instr-file, agentd RFC 0017 reload semantics) or via a managed
  roll; an in-flight run/turn always completes on the instruction it started with. No
  mid-reasoning prompt swap.
- **Change-gated.** The resolver reloads **only when the content digest changes** — a
  poll returning identical bytes (or a `304`) does **not** reload, so a 30-second poll
  on a stable document costs zero reloads.
- **Validated before applied.** A resolved value that is empty, over-cap, mime-mismatched,
  or non-UTF-8 is **rejected**; the last-good instruction is retained and the failure
  surfaced (`InstructionResolved: False`). The agent's prompt is never blanked or
  corrupted by a bad fetch.
- **Versioned in status.** `appliedDigest` + `appliedAt` give a rollback/audit anchor;
  a fleet can be asserted to have converged on a new instruction digest.

---

## 10. Worked example — a live, brokered, hot-reloaded instruction

```yaml
# ── OPS: the tool/doc servers, incl. the living-runbook MCP server (RFC 0019) ──────
apiVersion: agents.x-k8s.io/v1alpha1
kind: MCPServerSet
metadata: { name: engineering-tools, namespace: agents }
spec:
  servers:
    - name: docs                            # the MCP server hosting the living runbook resource
      transport: streamableHttp
      endpoint: https://mcp.corp.example.com/docs
      tags: { "*": [sensitive] }
      auth: { mode: ema, ema: { enterpriseIdp: https://idp.corp.example.com,
                                resource: https://mcp.corp.example.com/docs, scopes: [read] } }
    - name: github
      transport: streamableHttp
      endpoint: https://mcp.github.example.com/mcp
      tags: { "*": [untrusted_input], "create_*": [egress] }
      auth: { mode: oauthClientCredentials, oauthClientCredentials: {
                resource: https://mcp.github.example.com/mcp, scopes: [issues.write],
                credential: { kind: privateKeyJwt, clientId: agentctl-eng-bot,
                              keyRef: { name: github-mcp-client-key, key: private-key.pem } } } }
---
# ── DEV: instruction is the LIVING runbook, kept current by subscription ───────────
apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata: { name: eng-assistant, namespace: agents }
spec:
  classRef: { name: hardened }              # kata-hybrid, hostile tenancy
  mode: reactive
  instructionSource:
    mcpResource:
      serverRef: { name: docs, serverSetRef: engineering-tools }   # EMA-brokered (RFC 0019 §7)
      uri: doc://living/triage-runbook
      subscribe: true                       # live: the resolver holds resources/subscribe
      mimeType: text/markdown
  mcp:
    serverSetRefs: [engineering-tools]      # github tools (Tier 1) + docs (also the instruction source)
  access:
    oidc: { issuer: https://idp.corp.example.com, audiences: [eng-assistant] }
# Resolution (operator + resolver render, agentctl RFC 0006):
#  • resolver (per-pod sidecar) opens the `docs` MCP session THROUGH the RFC 0019 broker
#    (EMA/ID-JAG credential off-pod), resources/read → writes the runbook as the reloadable
#    instruction, resources/subscribe → on each update rewrites + signals reload
#  • agent hot-reloads the instruction at its next turn boundary — never restarts, never sees
#    the docs-server credential
#  • the sourced instruction is untrusted_input → contributes to the trifecta union with the
#    github `egress` tool → admission ADVISORY warning; contract enforces Rule-of-Two per spawn
#  • status.instruction = { source: mcpResource, ref: doc://living/triage-runbook,
#                           appliedDigest: sha256:…, live: true, untrustedInput: true }
```

Compare a simpler `url` instruction — a versioned prompt behind a token, polled:

```yaml
spec:
  instructionSource:
    url:
      href: https://prompts.example.com/triage/v3
      mimeType: text/markdown
      auth: { mode: header, header: { name: Authorization,
                                      valueSecretRef: { name: prompts-token, key: bearer } } }
      pollInterval: 5m                      # live refresh; the bearer stays in the RESOLVER, not the pod
```

The payoff: the developer declares *where the instruction lives and how fresh to keep
it*; the resolver holds any credential off the pod, treats the sourced text as
untrusted, and hot-reloads the agent without a restart — and for `mcpResource`, the
instruction stays current **by itself**.

---

## 11. Versioning, rollout & compatibility

- **Fully backward-compatible.** The scalar `instruction: <string>` is retained as
  sugar for `instructionSource: { inline: … }`; every existing manifest and the shipped
  `AgentSpec.instruction` field validate unchanged. `instructionSource` is an additive
  sibling; the CRD conversion (agentctl RFC 0005) maps the old scalar into the union.
- **Same group/version** (`agents.x-k8s.io/v1alpha1`); graduates with the API set under
  the single-served-version + SVM posture (agentctl RFC 0003 §8 / RFC 0005).
- **Contract change: none for static sources, P-instr-file for *hot* live reload.**
  Static sources (`inline`/`configMapKeyRef`/`secretKeyRef`/fetch-once) render into the
  agent's existing startup instruction (`--instruction-file`/`--instruction`/
  `INSTRUCTION`) with **no** contract change. Live sources (`url`-poll /
  `mcpResource`-subscribe / watched `file`) work today via managed roll and become
  **hot** (no-restart) once P-instr-file makes the existing `--instruction-file`
  reloadable (agentd RFC 0017 §5.1, §4).
- **Additive / removable.** Adopting a non-inline source is a refactor (extract the
  prompt to a ConfigMap/URL/MCP resource, repoint `instructionSource`); reverting to
  inline is a one-field edit. None of it gates an MVP milestone (brainstorm §16).
- **Graceful degradation.** An unreachable source (`keepLastGood`) or an unready MCP
  server (`McpServerUnready`, RFC 0019) holds the last-good instruction and surfaces
  the condition — the agent keeps running on what it last had.

---

## Non-goals

- **The resolver's reconcile/render implementation, the watch/poll loop, the atomic
  write.** agentctl RFC 0006. This RFC fixes the *union shape*, the *delivery
  invariant*, the *refresh model*, and the *security posture*; the controller behaviour
  is there.
- **The admission webhook that executes these rules** (the url host/scheme allow-list,
  TLS-required, size/poll-floor caps, cross-ns ref rejection, the trifecta union). agentctl
  RFC 0007.
- **The MCP server identity/authentication** an `mcpResource` subscribes to — the
  broker, the OAuth/EMA flow, the secret-free credential. agentctl RFC 0019. This RFC
  consumes the authenticated session.
- **Making the instruction reloadable at runtime.** The contract (agentd RFC 0017 —
  **P-instr-file**, §4). This RFC writes the value; the contract owns whether the agent
  re-reads it hot or is rolled.
- **The per-tool trifecta tag vocabulary + the per-spawn Rule-of-Two budget** an
  untrusted instruction feeds. The contract (agentd RFC 0012). agentctl surfaces the
  `untrusted_input` leg (§8.2).
- **A prompt-management / versioning service.** `url`/`mcpResource` *point at* one; this
  RFC does not implement prompt storage, diffing, or approval workflows (a source URI +
  applied digest is the seam).
- **Templating / variable interpolation of the instruction.** v1 delivers the resolved
  text verbatim; parameterized prompts (values injected into a template) are §Open.
- **Any data-plane config internals.** This union describes contract-level instruction
  sourcing; it MUST NOT encode one binary's config file layout (P0).

---

## Open questions

1. **Shared resolver vs per-pod sidecar for live sources.** §5 defaults to a per-pod
   sidecar (simplest isolation of fetch credential + subscription). A **node-local
   shared resolver** scales better (one poll for N pods sharing a source) but must carry
   the RFC 0012 §5.4-style `peer→Agent→source` authz to avoid one tenant reading
   another's credentialed source. Decide whether v1 ships the shared resolver or defers
   it (as RFC 0019 defers the shared broker).
2. **Fleet-shared live instruction.** An `AgentFleet` whose members share one
   `mcpResource` instruction: one subscription fanned out to N pods (efficient, needs
   the shared resolver of Q1) vs N per-pod subscriptions (simple, N× the server load).
   Reconcile with the fleet rendering (agentctl RFC 0011).
3. **Templating / parameterization.** Should `instructionSource` support variable
   interpolation (per-agent values into a shared template)? Additive, but it introduces
   an injection surface of its own and a templating grammar. Defer past v1 unless a
   concrete need lands.
4. **Hot live reload vs managed roll for v1.** §4 makes hot (no-restart) live reload
   depend on P-instr-file (making the existing `--instruction-file` reloadable) and
   ships **managed roll** as the interim. Decide whether v1 blocks on P-instr-file for
   the live cases or ships them via managed roll first (static + roll-based live need no
   contract change); sequence P-instr-file with the contract track (agentd RFC 0017).
5. **Signed instruction provenance.** For the highest-assurance case, should a sourced
   instruction be **signed** (the source publishes a signature the resolver verifies)
   so a compromised source cannot silently repoint an agent's prompt? Additive to §8.2's
   allow-list + digest; gate on demand.
6. **Poll-interval floor + fetch-cost governance.** §6 clamps `pollInterval` to a
   policy floor; whether the floor is global or per-source, and whether resolver fetch
   egress counts toward a tenant budget (as MCP/model egress does), is an ops-policy
   question.

---

## References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & Contract-as-Schema (P0): the contract-not-agent
  framing this plane's field neutralization follows.
- **agentctl RFC 0003** — `Agent`/`AgentFleet` CRDs: the `AgentSpec.instruction` this
  RFC generalizes into `instructionSource`; the reactive `subscribe` distinction (§7).
- **agentctl RFC 0004 §5** — `MCPServerSet`: the reusable tagged bundle whose servers an
  `mcpResource` references; the advisory trifecta union (§5.3) an untrusted instruction
  contributes to.
- **agentctl RFC 0005** — CRD versioning & conversion: the scalar→union conversion (§11).
- **agentctl RFC 0006** — operator reconcile: the resolver render/watch/poll
  implementation; the atomic write + status projection.
- **agentctl RFC 0007** — admission ladder: the url allow-list / TLS / size / poll-floor
  / cross-ns rules and the trifecta union (§8.2).
- **agentctl RFC 0010** — observability: the `InstructionResolved` condition + the
  provenance status; resolver fetch metrics.
- **agentctl RFC 0012** — intelligence plane: the off-pod / not-in-node-agent placement
  discipline (§5) and the file-rotation secret discipline (§5) this RFC reuses.
- **agentctl RFC 0015** — security & multi-tenancy: the five-class secret lifecycle the
  `url` fetch credential lives under; the SSRF/egress allow-list the resolver fetch obeys.
- **agentctl RFC 0019** — MCP server registration, identity & authentication: the
  identity/auth of the `mcpResource` server (§7); the broker the resolver subscribes
  through; the one-MCP-identity-two-consumers composition (RFC 0019 §11).

**Contract spec (the reference implementation, agentd RFCs / schemas)**

- **agentd RFC 0017** — declarative config & hot reload: the reloadable subset (§5.1)
  that instruction must join for hot live reload (**P-instr-file**, §4); the
  turn-boundary reload semantics (§9).
- **agentd RFC 0012** — security posture: the per-tool glob trifecta tags (§3.1) an
  externally-sourced `untrusted_input` instruction contributes a leg to (§8.2); the
  per-spawn Rule-of-Two budget; the never-logged secret discipline (§6).
- **`contract/schemas/config.schema.json` / `agentd .../config.rs`** — instruction is a
  **startup input** (`--instruction`/`--instruction-file`/`INSTRUCTION`), **not** in the
  `x-reloadable` subset (§4 — the gap P-instr-file addresses); the "config file NEVER
  carries a credential" rule §8.1 extends to instruction ingress.
- **`crates/agent-api/src/lib.rs`** — the shipped `AgentSpec.instruction:
  Option<String>` this RFC generalizes and the reactive `subscribe` field (§7); the
  `SecretKeyRef`/`LocalRef` shapes the union reuses (a `ConfigMapKeyRef` shape is
  **added** by this RFC — it is not shipped today).

**Contract asks raised by this RFC** (agentctl brainstorm §14): **P-instr-file** — make
the reference agent's **existing** `--instruction-file` **tailed/reloadable** (add
instruction to the reloadable subset, agentd RFC 0017 §5.1), the **prerequisite for hot
(no-restart) live instruction reload**; static sources and managed-roll live updates
need no contract change (§4). The agent-branded surfaces this plane resolves into (the
instruction input, the reload trigger, `agent://`) are flagged for the **P0
contract-extraction** open question (agentctl RFC 0001 §9).

*Where this RFC and a contract spec disagree on the wire, the contract wins and this
RFC is corrected; where this RFC needs a primitive the contract does not expose
(P-instr-file), it is a contract ask — never a leak of cluster logic into the agent.*
