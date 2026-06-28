# agentctl RFC 0016: CLI & kubectl-plugin grammar

**Status:** Proposed (agentctl interface track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — **the human client.** It is the ergonomic, kubectl-native surface over the management/observe/A2A verbs: the `kubectl agent[s]` plugin faces and the standalone `agentctl`. It is a *client* of agentctl RFC 0009's access path, not a new access path; it renders its verb set from the contract manifest, not a hardcoded list.

> **The CLI is a client, never a path.** Every byte a human sends flows through the
> access path agentctl RFC 0009 owns. **Cold reads** span **four** back-ends with
> *distinct* auth (§4.1): the kube-apiserver (CRD `.status`, reusing kubeconfig and
> strictly pod-gone-safe), the persisted run-report store, Prometheus, and the A2A
> gateway (its own `securitySchemes` — **not** kubeconfig). **Live/mutating verbs** hit
> the **aggregated APIServer's connect subresources** on `management.agents.x-k8s.io`
> (per-verb RBAC + end-user identity + per-human audit, agentctl RFC 0009 §4.4/§5).
> This RFC adds **no** new auth, **no** new transport, and **no** new reachability —
> it adds grammar, output, and the attach UX.

> **Verb *visibility* renders from the manifest, never from a hardcoded list or
> `build_features` (P0) — with one honest bound.** Which subcommands a target exposes
> is computed from that agent's advertised `surfaces{}` / `surfaces.operator_tools`
> (agentd RFC 0015 §5.2): a conformant agent that does not advertise a surface simply
> does not get the verb, and capability *absence is graceful degradation, not an error*
> (agentd RFC 0014 §8). **The bound, stated plainly:** `operator_tools` is a bare
> **name list with no per-tool input schema** (agentd RFC 0015 §5.2) and `clap`
> subcommands are compile-time — so the CLI **subtracts** (hides a verb the agent
> omits) and surfaces the *contract-defined* vocabulary with typed grammar, but it
> cannot synthesize bespoke flags for a *novel* second-vendor verb outside
> {drain,lame-duck,pause,resume,cancel}. A second-vendor agent advertising a different
> *subset* of that vocabulary gets a different subcommand set with no code change; a
> genuinely *additive* vendor tool is surfaced (not dropped) via the generic
> `kubectl agent <x> tool <name> [--arg …]` passthrough (§3.2), with per-tool typed
> grammar awaiting a contract ask for per-tool input schemas in the manifest.

> **The data plane is any conformant agent.** This RFC names **agent** only as the
> reference implementation in examples and when citing where a contract surface is
> currently specified. The `agent://` URIs, the `agent_` metric prefix, and the
> `--capabilities` entrypoint are the **reference spelling** of contract-normative
> surfaces, flagged for neutralization (the P0 contract-extraction open question,
> agentctl RFC 0001 §9 / RFC 0018).

---

## 1. Problem / Context

agentctl exposes a large, capability-negotiated verb surface — lifecycle
(`drain`/`lame-duck`/`pause`/`resume`/`cancel`), live reads (`tree`/`logs`/`top`),
cold reads (`get`/`describe`/`results`/`card`), and the novel interactive steering
verb (`attach`). Operators and humans need that surface **the way Kubernetes
already trained them to expect it**: `kubectl`-native, RBAC-governed, with `-o
wide/json/yaml`, label selectors, `-w` watch, namespaces, and contexts — not a
bespoke client with its own auth and its own mental model.

Three properties make this more than "wrap an API in a CLI":

1. **The surface is contract-negotiated, per agent.** The verbs are not invented
   here and are not uniform across agents. They are the agent's *operator profile*
   (agentd RFC 0015 §4), discovered from `surfaces.operator_tools` in the
   capabilities manifest (agentd RFC 0015 §5.2), plus the read surfaces
   (`surfaces.management`/`metrics`/`events`, the `agent://inventory`/`status`
   resources). A binary built without a surface does not advertise it, and the CLI
   MUST then **not** offer the verb — graceful degradation is the default posture,
   not an exception (agentd RFC 0014 §8). The CLI therefore plans its subcommand and
   column set **from the manifest projected into `Agent.status`** (agentctl RFC 0006
   §4.2 / RFC 0003 §6), never from the agent's name and never from `build_features`.

2. **The surface is split across three back-ends with three trust models.** Cold
   reads come from the kube-apiserver (CRD `.status`, the persisted run-report
   store) reusing kubeconfig auth; live/mutating verbs come from the **aggregated
   APIServer** (agentctl RFC 0009 §4.4) which carries per-verb RBAC and end-user
   identity to the node-agent PEP; and `card`/`tasks` come from the **A2A gateway**
   (agentctl RFC 0013), a third path with its own auth and durability. The CLI must
   route each verb to the right back-end and **degrade each independently** (a
   down aggregated APIServer must not break cold reads — agentctl RFC 0009 §4.5).

3. **`attach` is a genuinely new interactive UX** with no Kubernetes precedent —
   live steering of a warm agent session, multi-viewer, leased, audited. It is
   `subagent.send` (agentd RFC 0015 §4.5) under the hood, but the terminal UX, the
   single-writer lease, and the multi-viewer echo are agentctl's to design (§5).

This RFC owns: the three faces and their packaging (§2), the command grammar and
its mapping to backing surfaces + access paths (§3), the cold-vs-live routing
(§4), the `attach` UX (§5), and the output/negotiation/exit-code contract (§6). It
does **not** own the access path's authz (agentctl RFC 0009), the node-agent
management API (agentctl RFC 0008), the telemetry it reads (agentctl RFC 0010), the
CRD/status schema it prints (agentctl RFC 0003/0006), or the A2A gateway it queries
(agentctl RFC 0013). It is the **human client** those RFCs repeatedly defer to.

---

## 2. Decision — three faces, one codebase

**One `clap` binary's worth of logic, shipped under three installed names, so
`kubectl agent[s] …` Just Works and `agentctl …` works standalone.** This is the
direct realization of agentctl RFC 0001 §5 (`crates/cli`, three `bin` targets
sharing one lib) and resolves brainstorm §8.1.

### 2.1 The three faces

| Installed name | Invoked as | Scope | Why it exists |
|---|---|---|---|
| `kubectl-agent` (singular) | `kubectl agent <name> …` | **one instance** | per-instance verbs: `describe`, `tree`, `logs`, `top`, `drain`, `lame-duck`, `pause`, `resume`, `cancel`, `attach`, `card`, `results` |
| `kubectl-agents` (plural) | `kubectl agents …` | **fleet / list** | list/aggregate verbs: `get`, `top`, `results`, `tasks` |
| `agentctl` (standalone) | `agentctl agent[s] …` | both | the no-`kubectl` entrypoint (CI, scripts, machines without the plugin on `PATH`); mirrors the same grammar with explicit nouns so muscle memory transfers |

**Both `kubectl-agent` and `kubectl-agents` ship as distinct on-`PATH` names — a
red-team correction, not an option.** `kubectl` resolves a plugin by matching the
*longest* on-`PATH` `kubectl-<token…>` binary to the command tokens, **per top-level
token**: `kubectl agents get` dispatches to `kubectl-agents`, `kubectl agent foo
drain` to `kubectl-agent`. One binary under one name **cannot** serve both the
singular and the plural verb — so two installed names are mandatory (agentctl RFC
0001 §5). They are the **same code**: thin `bin` shims over `crates/cli`'s shared
library, differing only in which noun (`agent` singular vs `agents` plural) they
pre-bind. Whether they are three physically distinct binaries or one multi-call
binary dispatched by `argv[0]` is a packaging detail (agentctl RFC 0001 §9 OQ #3) —
the *names on `PATH`* are the contract Krew resolves against, and those are fixed.

```
                       PATH lookup by kubectl (longest-prefix, per token)
  kubectl agents get ─────────────────►  kubectl-agents ┐
  kubectl agent triage drain ──────────►  kubectl-agent  ├─► crates/cli (one shared lib; clap)
  agentctl agents get / agent t drain ─►  agentctl       ┘        │
                                                                  ├─ kube (kubeconfig, context, auth) — RFC 0001 §3
                                                                  ├─ COLD  → kube-apiserver: CRD .status / store / gateway (§4)
                                                                  └─ LIVE  → aggregated APIServer connect subresources (RFC 0009 §4.4)
```

### 2.2 clap, kube, Krew, completion

- **`clap` is the parser** (agentctl RFC 0001 §2.1). Subcommands, flags, help, and
  shell-completion generation all come from one `clap` command tree; the three
  faces are three roots over the same subcommand definitions.
- **`kube` owns kubeconfig** (agentctl RFC 0001 §3): context selection
  (`--context`), namespace (`-n`/`--namespace`, `--all-namespaces/-A`),
  `--kubeconfig`, and the standard auth resolution (OIDC/exec-credential/SA token).
  The CLI presents **no** auth model of its own — it is exactly the kubeconfig the
  human already uses for `kubectl`, which is *why* per-human RBAC and audit work end
  to end (agentctl RFC 0009 §5.2). There is **no** `client-go`; `kube` is the client
  (agentctl RFC 0001 §3).
- **Krew is the distribution, language-agnostic.** Krew requires only a
  `kubectl-<name>` binary on `PATH`; it is indifferent to the implementing language
  (this is precisely why the all-Rust decision costs nothing here — agentctl RFC
  0001 §3/§5). agentctl ships **two Krew manifests** (`kubectl-agent`,
  `kubectl-agents`) generated by `xtask` into `deploy/krew/` (agentctl RFC 0001 §5).
  `agentctl` standalone ships via the ordinary release artifacts (and Homebrew/OCI),
  not Krew.
- **Shell completion** is `clap`-generated (bash/zsh/fish/powershell) via `agentctl
  completion <shell>`. For the plugin faces, completion follows the kubectl plugin
  completion convention (a `kubectl_complete-agent` / `kubectl_complete-agents`
  helper on `PATH`) so `kubectl agent <TAB>` completes instance names, verbs, and —
  critically — **only the verbs the target advertises** (§6.3). Dynamic completion
  (instance names, handles for `cancel`, sessions for `attach`) does cold list reads
  through `kube`; it never reaches a live socket.

---

## 3. The command surface

The grammar is small and mnemonic. Every verb maps to **exactly one backing
contract surface** and **exactly one access path** — the two columns that make the
cold/live split (§4) legible. Nothing in this table is invented by agentctl: each
verb is a thin, kubectl-shaped front over a surface the contract already defines.

```
# FLEET / LIST  (kubectl-agents)
kubectl agents get [-o wide|json|yaml] [-l SELECTOR] [-A|-n NS] [-w]
kubectl agents top    [-l SELECTOR] [-w]          # fleet metric rollup
kubectl agents results [-l SELECTOR] [--since D]  # terminal run outcomes (persisted store)
kubectl agents tasks  [-l SELECTOR] [--state S]   # A2A tasks (gateway)

# ONE INSTANCE  (kubectl-agent)
kubectl agent <name> describe                     # cold status + curated projection
kubectl agent <name> tree [-w]                    # live subagent tree
kubectl agent <name> logs [-f] [--since D] [--tail N]
kubectl agent <name> top [-w]
kubectl agent <name> results [--run ID]
kubectl agent <name> drain      [--deadline D]
kubectl agent <name> lame-duck  [--ready]         # --ready restores readiness
kubectl agent <name> pause | resume               # if advertised (P-pause)
kubectl agent <name> cancel <handle> [--reason R]
kubectl agent <name> attach [--session S | --handle H] [--read-only] [--steal] [--send TEXT]
kubectl agent <name> card                         # the agent's A2A card (gateway)
```

### 3.1 Verb → backing surface → access path

| Command | Backing contract surface (reference spelling) | Access path (agentctl RFC 0009) | Kind |
|---|---|---|---|
| `agents get [-o wide]` | `Agent.status` curated projection + CRD `additionalPrinterColumns` (agentctl RFC 0003 §6.3, RFC 0006 §4.2) | kube-apiserver **CRD list** (`agents.x-k8s.io`) | **COLD** |
| `agent <x> describe` | `Agent.status` (full curated projection; optional one-shot live enrich) | kube-apiserver CRD get (+ optional `agents/inventory` read) | **COLD** (+opt live) |
| `agent <x> tree [-w]` | `agent://inventory` — live subagent tree (agentd RFC 0015 §5.3) | `agents/inventory` **`get`** connect subresource (RFC 0009 §5.1) | **LIVE** (`-w`) |
| `agent <x> logs [-f]` | **bulk** stderr→Loki (agentctl RFC 0010 §6.1); **live tail** `agent://events` ring (agentd RFC 0016 §7) | bulk: Loki / `kubectl logs`; live: `agents/log` **`get`** connect | **LIVE** (`-f`) |
| `agent <x> top [-w]`, `agents top` | metrics `agent_*` / `agent://metrics` (ask **P4**); fleet rollups + recording rules (agentctl RFC 0010 §5/§9) | static: Prometheus/recording-rule read (cold); live: `agents/metrics` **`get`** connect | COLD static / **LIVE** (`-w`) |
| `agents results`, `agent <x> results` | persisted run-report store, `report_schema` (agentctl RFC 0010 §7; ask **P5**) | kube-apiserver / store read | **COLD** (store) |
| `agent <x> drain` | `drain` tool (agentd RFC 0015 §4.1 — ≡ SIGTERM ≡ clean exit 0) | `agents/drain` **`create`** connect (RFC 0009 §5.1) | **LIVE / mutating** |
| `agent <x> lame-duck` | `lame-duck` tool (agentd RFC 0015 §4.2 — reversible readiness flip) | `agents/lame-duck` **`create`** connect | **LIVE / mutating** |
| `agent <x> pause`/`resume` | `pause`/`resume` (agentd RFC 0015 §4.3 — **ask P-pause**) | `agents/pause` **`create`** connect | **LIVE / mutating** |
| `agent <x> cancel <handle>` | `cancel` tool (agentd RFC 0015 §4.4 — wraps `subagent.cancel`) | `agents/cancel` **`create`** connect | **LIVE / mutating** |
| `agent <x> attach` | **`subagent.send`** (agentd RFC 0015 §4.5 — `ctrl/inject` into a warm session) | `agents/attach` **`create`/stream** connect — **gated, §5/§RFC 0009 §6** | **LIVE / steer** |
| `agents tasks`, `agent <x> card`, `agent <x> tasks` | A2A gateway: `tasks/list`, projected Agent Card (agentctl RFC 0013 §3.2, RFC 0014) | **A2A gateway** (third path; own auth + durability) | gateway |

### 3.2 The verb set is rendered, not hardcoded (the P0 mechanism)

The CLI does not ship a fixed subcommand list per target. For a given `<name>` it
reads the target's `Agent.status` (one cold CRD get) and **computes the available
verbs from the advertised surfaces**:

```
plan_verbs(status) =
  always:  describe, get, results                 # cold paths — always offered (degrade per back-end §6.3)
  if status.contract.compatible == false:         # major not understood (agentd RFC 0014 §8)
      → offer ONLY cold reads + `logs` (stderr) ; everything else hidden as "contract-incompatible"
  if surfaces.management present:                  # the live management transport exists
      tree, logs -f                               # streaming reads off inventory/events
      for v in surfaces.operator_tools:           # ← the AUTHORITATIVE list (agentd RFC 0015 §5.2)
          if v ∈ contract-vocab {drain,lame-duck,pause,resume,cancel}:
              expose typed   kubectl agent <v>    #   contract-fixed flag grammar (pause/resume IFF advertised)
          else:                                    #   novel/additive vendor tool — no per-tool schema today
              expose generic kubectl agent <x> tool <v> [--arg …]   # passthrough: visible, no bespoke grammar
      if "subagent.send" reachable on mgmt:        # attach == subagent.send
          expose attach (subject to RBAC §5 + P-attach-gate §5.5)
  if surfaces.metrics != false:  expose top
  if surfaces.events:            expose logs -f (ring live tail)
  if surfaces.a2a (P2):          expose tasks, card   # gateway paths — else hidden / inert (agentctl RFC 0003 §3.3)
```

Two consequences the house style insists on. **First, `pause`/`resume` are a
manifest call, not a CLI call.** The reference implementation's
`OPERATOR_TOOLS` is `["drain","lame-duck","cancel"]` today — `pause`/`resume` are
contract-specified (agentd RFC 0015 §4.3) but **not yet implemented** (ask
**P-pause**). Because the verb set renders from `surfaces.operator_tools`, a `pause`
verb on a binary that does not advertise it is simply **not exposed** — no special
case, the same capability-absence-not-error posture as everywhere (agentctl RFC
0009 §1 P-pause caveat). **Second, a second-vendor agent's *contract-vocabulary*
subset Just Works, and its *additive* tools are surfaced rather than dropped.** A
conformant agent advertising a different *subset* of {drain,lame-duck,pause,resume,
cancel} gets a different *typed* subcommand set with no change to this CLI. A
conformant agent advertising a genuinely *novel* operator tool (e.g. `quarantine`,
`rotate-creds`) is surfaced as `kubectl agent <x> tool quarantine [--arg …]` — visible
and invocable, but **without** bespoke typed flags until the contract adds per-tool
input schemas to the manifest (the honest bound flagged in the header). The failure
this design avoids is the worst one: *silently dropping* a vendor's advertised
capability because no `clap` subcommand was compiled for it.

---

## 4. The cold-vs-live (and gateway) path split

The single most important routing rule: **cold reads never touch a live socket;
live/mutating verbs never touch etcd.** This is agentctl RFC 0009 §2.3, realized in
the client. It is what makes the read path work when the pod is gone, keeps the
RBAC economy clean (a read-only auditor reaches **no** live socket — agentctl RFC
0009 §2.3/§5.4), and lets each back-end fail independently.

```
                         kubectl agent[s] <verb>
                                  │
        ┌─────────────────────────┼─────────────────────────────────────┐
        ▼                         ▼                                       ▼
   COLD (read)              LIVE / mutating                          GATEWAY (A2A)
   get·describe·            tree -w·logs -f·top -w·                  card·tasks
   results·static top       drain·lame-duck·pause·resume·            (RFC 0013/0014)
        │                   cancel·attach                                  │
        ▼                         ▼                                       ▼
   kube-apiserver        kube-apiserver aggregation front            A2A gateway
   ┌───────────────┐     ┌───────────────────────────────┐          ┌─────────────┐
   │ CRD .status   │     │ management.agents.x-k8s.io      │          │ HTTP PEP    │
   │ (agents.x-k8s)│     │   /v1alpha1  (AGGREGATED, §4.4) │          │ TLS/OAuth/  │
   │ persisted     │     │ → SAR per-verb (alice?)         │          │ mTLS, store │
   │ run-report    │     │ → identity forwarded            │          │ (own auth/  │
   │ store (0010)  │     │ → mTLS → node-agent → agent      │          │  durability)│
   └───────────────┘     └───────────────────────────────┘          └─────────────┘
     kubeconfig auth        per-verb RBAC + per-human audit             card schemes
     works pod-gone         (RFC 0009 §5)                               (RFC 0013 §4.1)
```

### 4.1 Cold reads — four back-ends, distinct auth (only the kube-apiserver reuses kubeconfig)

"Cold" means "no live socket, no node-agent hop" — it does **not** mean "kubeconfig."
`get`/`describe`/`results`/static `top` (and the gateway `card`/`tasks`, §4.4) are reads
against already-persisted state, but they span **four** back-ends with *different* auth and
*different* pod-gone guarantees:

| Cold verb | Back-end | Auth | Endpoint discovery | Pod-gone-safe? |
|---|---|---|---|---|
| `get` / `describe` | kube-apiserver CRD (`agents.x-k8s.io`) | **kubeconfig** | the kubeconfig server | **yes** — etcd-backed, strictly |
| `results` | persisted run-report store (agentctl RFC 0010 §7) | the store's own credential | the operator-published store endpoint (RFC 0010 §7) | yes (durably once **P5** lands) |
| static `top` | Prometheus / recording rules (agentctl RFC 0010 §5/§9) | the Prometheus endpoint's auth | a configured/annotated Prometheus endpoint (OQ #7) | yes — metrics outlive the pod |
| `card` / `tasks` | A2A gateway (§4.4) | card `securitySchemes` — **not** kubeconfig | the gateway endpoint (RFC 0013 §4.1) | gateway-durable |

- **`get` / `describe`** are a CRD `list`/`get` on `agents.x-k8s.io` — the curated
  `.status` projection (agentctl RFC 0003 §6 / RFC 0006 §4.2). `kubectl agents get`
  is, at its core, `kubectl get agents` with the plugin adding convenience: the
  **columns come from the CRD's server-side `additionalPrinterColumns`** (agentctl
  RFC 0003 §6.3), so `-o wide` is the apiserver's job, not the client's. Governed by
  ordinary `get/list/watch agents` RBAC. **These are the only cold reads that strictly
  reuse the human's kubeconfig and are strictly pod-gone-safe by construction.**
- **`results`** reads the **persisted run-report store** (agentctl RFC 0010 §7), not
  a vanished `--report-file` and not the live `agent://run/{run_id}` (which the pod
  takes with it). "Works when the pod is gone" is scoped to the **store** — a
  red-team correction baked in (brainstorm §8.2): the store is what survives, the
  pod is not. Robust once-mode capture depends on ask **P5**. The CLI reaches the
  store at the endpoint the operator publishes (RFC 0010 §7), with the store's own
  credential — **not** the kubeconfig identity.
- **static `top`** (no `-w`) reads the metrics already scraped into Prometheus and
  the recording-rule rollups agentctl RFC 0010 §5/§9 owns — a cold query, not a live
  socket read — against a configured/annotated Prometheus endpoint (the discovery
  mechanism is OQ #7).

So `get`/`describe` reuse kubeconfig and require **no** aggregated subresource and **no**
node-agent hop (agentctl RFC 0009 §2.3); `results` and static `top` are cold (no live
socket) but reach their **own** back-ends with their **own** credentials; `card`/`tasks`
authenticate to the gateway per its `securitySchemes` (§4.4). Conflating "cold" with
"kubeconfig" is the precise error this table exists to prevent.

### 4.2 Live / mutating verbs — the aggregated APIServer connect subresources

`tree -w`, `logs -f`, live `top -w`, and every mutation
(`drain`/`lame-duck`/`pause`/`resume`/`cancel`/`attach`) go through the **aggregated
APIServer** on `management.agents.x-k8s.io/v1alpha1` (agentctl RFC 0009 §4.4). The
CLI POSTs (mutations, `create` verb — kubelet `pods/exec` idiom) or GETs (streaming
reads, `get` verb — `pods/log` idiom) the corresponding **connect subresource**
(`agents/drain`, `agents/log`, …). The aggregation front issues the per-verb
`SubjectAccessReview` as the authenticated user, audits it per human, resolves
name→node, and dials the node-agent over mTLS (agentctl RFC 0009 §5.2/§5.3). The
CLI's only job is to construct the right subresource request and stream the result;
it carries **no** authz logic itself.

This is also why the two-`apiGroup` ergonomic cost (agentctl RFC 0009 §4.4.1) is a
CLI concern: a human checking their own access runs

```
kubectl auth can-i create agents/drain.management.agents.x-k8s.io -n tenant-acme   # → yes/no
kubectl auth can-i create agents/attach.management.agents.x-k8s.io -n tenant-acme  # → the §5.5 gate
```

and the CLI's `--help` and "permission denied" messages name the **group-qualified**
subresource so the user can map a `403` straight to the RBAC rule that denied it.

### 4.3 The `pods/proxy` stopgap (single-tenant / admin only)

On a cluster that installed the coarse `pods/proxy` stopgap instead of the
aggregated APIServer (agentctl RFC 0009 §3.5 — **forbidden under hostile
tenancy**), the live verbs route through raw `pods/proxy` with **no** per-verb RBAC
and **no** per-human audit. The CLI MUST detect this (the control-plane condition /
install flag agentctl RFC 0009 §3.5 advertises) and **say so**: `kubectl agent`
help and the pre-flight of any mutation print "management path = pods/proxy stopgap:
no per-verb RBAC, no per-human audit; admin/single-tenant only," so an operator
never mistakes it for the hardened path. This is the only path on which `attach`'s
per-verb gate (§5.5) is unexpressible — another reason it is non-default.

### 4.4 Gateway path — `card` / `tasks`

`card` and `tasks` are a **third** path with their own auth and durability — the
A2A gateway (agentctl RFC 0013 §3/§4, the projected card agentctl RFC 0014). They
do **not** share cold/live semantics with the rest: `tasks` reads the gateway's
durable store (`tasks/list`), `card` fetches the projected, centrally-signed Agent
Card. The CLI authenticates to the gateway per the card's `securitySchemes`
(agentctl RFC 0013 §4.1), which is **not** the kubeconfig identity. The CLI labels
these clearly so a user does not assume a `tasks` listing is governed by the same
RBAC as `drain`.

---

## 5. `kubectl agent <x> attach` — the interactive steering UX

`attach` is the one verb with no Kubernetes precedent and the one this RFC designs
in full. It is **live puppeting**: injecting free-text steering into a warm agent
session and watching the result stream back. The *authorization* of attach is
agentctl RFC 0009 §6's; this RFC owns the **UX, the streaming model, the session
selection, the multi-viewer echo, and the lease** — and is explicit about which of
those are backed today versus gated behind contract asks.

### 5.1 Design framing — it is `subagent.send`, leased and viewed

`attach` adds **no new agent tool**. It is `subagent.send` (agentd RFC 0015 §4.5),
which delivers an `InjectEvent` into a warm session via `ctrl/inject` (agentd RFC
0005 §4.3). agentctl wraps it with three things the agent deliberately does not
have (the agent has no session-ownership, no auth, no multi-viewer model — agent
RFC 0012 §3.8):

1. a **read-many / write-one lease** so two operators do not fight over one session;
2. a **multi-viewer fan-out** so observers see every steer, not just the writer's
   own terminal; and
3. a **per-verb RBAC + contract gate** (§5.5) so reachability is not steering.

### 5.2 The streaming model — connect/stream over the aggregated API

`attach` opens the `agents/attach` connect subresource (agentctl RFC 0009 §5.1, a
`create`/stream verb). The wire is a **bidirectional stream** over the aggregation
front's connect-upgrade machinery (the same machinery that carries `kubectl
exec`-style verbs for aggregated APIs — agentctl RFC 0009 §3.4): the **uplink**
carries steer events (`--send TEXT`, or interactive lines in a TTY), each becoming
one `subagent.send{session, event}`; the **downlink** is the merged tail of
`agent://events` (agentd RFC 0016 §7) and `agent://session/{session_id}` `updated`
notifications (agentd RFC 0005 §3.4), re-framed to the client as SSE/line frames.
The exact upgrade shape (SPDY vs WebSocket vs SSE) is the aggregated APIServer's to
fix (agentctl RFC 0009 OQ / §3.4); the CLI consumes whatever that connect handler
negotiates.

> **v1 scope (backed today) vs gated (contract asks).** v1 `attach` is **one-shot
> `--send` + a read-only event tail** — both fully backed by today's contract
> (`subagent.send` + `agent://events`). **Full interactive multi-viewer steering**
> — a TTY where every keystroke steers and every viewer sees a clean steer-echo —
> needs new primitives (P-inject, P-session below) and is **gated behind a contract
> minor bump**. The CLI offers the interactive TTY mode only when the manifest
> advertises the gated capability; otherwise it offers `--send` + tail and says so.
> **Further v1 bound:** even one-shot `--send` is *single-warm-session-only* when an
> agent has more than one warm session, because v1 has no surface that enumerates a
> `session_id` (the count-only gap, §5.3); multi-session attach awaits **P-session**.

### 5.3 Session / handle selection (ask P-session)

A correctness subtlety the brainstorm (§8.2) flags and this RFC must not get wrong:
`subagent.send` targets a warm **session**, addressed by `session_id` off
`agent://session/{session_id}` (agentd RFC 0005 §3.4) — **not** a subagent
**handle** off `agent://subagent/{handle}` (which addresses tree nodes for
`cancel`/`tree`). The two key spaces are different and must not be conflated.
Today `agent://status` exposes only a **`warm_sessions` count** (agentd RFC 0015
§5.4), not an enumerable list — so the CLI **cannot enumerate steerable sessions**.

- **v1 behavior:** if `warm_sessions == 1`, `attach` targets that session
  implicitly; if `> 1`, the CLI **requires** an explicit `--session S` and **fails
  with a clear error** rather than guessing (no silent wrong-session steer). **The
  honest limitation this exposes:** with `warm_sessions > 1` there is *no* v1 contract
  surface from which a user obtains a valid `session_id` — `agent://status` exposes
  only the *count*, and inventory/tree key by subagent *handle* (a different key
  space, §below). So interactive attach is **effectively single-warm-session-only
  until P-session**. The CLI MAY accept a `session_id` that surfaced incidentally in
  the `agent://events` tail (or a future `describe` field) if the user supplies it,
  but it never manufactures or guesses one.
- **Contract ask P-session:** warm-session/handle **enumeration** with a
  `steerable` flag, so the CLI (and tab-completion) can list sessions and the user
  can pick. `--handle H` remains reserved for the day handle-vs-session keying is
  resolved in the contract (brainstorm §8.2); until then `attach` is
  session-addressed only.

### 5.4 Multi-viewer echo (ask P-inject)

The "every viewer sees every steer" guarantee is **bounded by two contract facts**
the CLI must design around, not paper over:

1. `agent://events` is a **lossy in-memory ring** with a `dropped` counter (agent
   RFC 0016 §7) — a viewer that falls behind misses frames, and the read returns
   `dropped > 0` so the CLI can surface "(N events dropped)" rather than silently
   lie.
2. There is **no inject event in the closed event vocabulary today** — a steer
   injected by one writer does not necessarily appear as a distinct event other
   viewers can render as "alice steered: …". That is ask **P-inject**: a frozen
   `InjectEvent` shape **plus** an `inject` member of the closed event vocabulary so
   the steer echoes to all viewers through the ring.

Because the ring is lossy and the inject event is not yet contract, agentctl
**does not rely on the ring for the audit of a steer.** Each `attach` write emits a
**durable audit record** independent of the events ring — `{session, writer,
text-hash, ts}` — through the management-action audit sink (agentctl RFC 0009 §5.3.4
/ RFC 0010 §10.2, asks P-meta/P-audit). The ring is the *live convenience* echo; the
audit record is the *truth* of who steered what.

### 5.5 The no-puppeting gate — who may attach vs observe

`attach` is the verb hostile tenancy makes load-bearing: a neighbour may need to
`drain`/`cancel`/observe but **must not steer** (brainstorm §0.6 tenancy (c)). The
CLI inherits the two-layer gate agentctl RFC 0009 §6 owns and surfaces it cleanly:

- **`--read-only`** joins as a **viewer**: downlink tail only, **no** uplink. It is
  backed by the observe surfaces (`agents/log`/`agents/inventory`, `get` verbs) and
  needs **no** `agents/attach` grant. This is the "watch the steering without being
  able to steer" mode.
- **Write attach** (uplink enabled) requires the `agents/attach` `create` grant
  (agentctl RFC 0009 §5.4 — held by the `agent-steerer` role, deliberately absent
  from `agent-operator`). On `403` the CLI prints the group-qualified verb that was
  denied (§4.2).
- **Structural no-puppeting (ask P-attach-gate):** where the agent is configured to
  omit `subagent.send` from the management transport (agentctl RFC 0009 §6.1 layer
  2), `attach` is simply **not advertised** and the CLI does not offer write attach
  at all — capability absence, not a `403`. The CLI must render the same way for
  "RBAC-denied" and "not-advertised" only at the *help* layer (both hide the verb);
  at the *invocation* layer it distinguishes them (a `403` vs a "this agent does not
  expose steering") so the user knows whether to ask for a grant or that the tier
  forbids it.

### 5.6 The lease (read-many / write-one)

A single-writer **advisory lease** per `(pod, session)` lives in **agentctl**, not
the agent (the agent has no session/auth model — agentd RFC 0012 §3.8):

- **read-many:** any number of `--read-only` viewers attach concurrently.
- **write-one:** at most one writer holds the lease; a second `attach` without
  `--steal` is refused with "session held by <writer> since <ts>; use --steal".
- **`--steal`** transfers the lease (audited: a steal emits its own audit record);
  the displaced writer is notified and demoted to viewer.
- **revocation:** dropping the connection releases the lease after a short TTL; the
  lease is the kill-switch surface a compromised steer grant is revoked through
  (agentctl RFC 0015 OQ).

Where the lease *physically lives* — in-memory in the aggregated APIServer (simple,
lost on restart) vs a `coordination.k8s.io` `Lease`/CR (durable, watchable, GitOps-
visible) — is an open question (§8, brainstorm §8.3). The **semantics** above are
fixed regardless.

```
  alice (writer, holds lease)  ──steer──►┐
  bob   (--read-only viewer)             ├─► agents/attach (aggregated, RFC 0009 §6)
  carol (--read-only viewer)             │      │ SAR: alice may create agents/attach? ✓
                                         │      │ lease(pod,session): alice=writer
  downlink (events ring + session updated)◄─────┘ uplink: subagent.send{session, InjectEvent}
        │  (lossy ring → "(N dropped)"; inject-echo gated on P-inject)
        └─ every steer ALSO → durable audit record (P-meta/P-audit) — the truth, independent of the ring
```

---

## 6. Output formats, contract negotiation, exit codes

### 6.1 Output formats

`-o` follows kubectl idiom exactly, so existing tooling and pipelines transfer:

| `-o` | Cold reads | Live/aggregated reads |
|---|---|---|
| *(default)* | the CRD's **priority-0** `additionalPrinterColumns` (agentctl RFC 0003 §6.3) | a compact human table the CLI renders from the streamed/structured result |
| `wide` | priority-0 **plus** priority>0 columns — `-o wide` is *additive* per kubectl semantics (RFC 0003 §6.3 puts Model, Build, Substrate, Node at priority>0); the apiserver's job, not the client's | extra columns where the surface provides them |
| `json` / `yaml` | the apiserver's own serialization (the CRD object) | the structured result (`CallToolResult.structuredContent`, `agent://inventory` body) serialized verbatim |
| `name` | `agent/<name>` | n/a |
| `jsonpath` / `go-template` | kubectl's own templating over the cold object | the CLI applies the same templating over the structured live result |

Two consistency rules. **First, cold `get`/`get -o wide` columns are the CRD's
server-side printer columns** (agentctl RFC 0003 §6.3), not client-invented — `-o
wide` is answered by the apiserver. **Second, churny live counts are not cold
columns.** Per agentctl RFC 0003 §2.2, in-flight/active-subagent counts and token
counters are deliberately kept **out** of `.status` (etcd write-amplification), so
they are **not** `get -o wide` columns. The brainstorm's "in-flight" wide column is
served by **`describe`** (which may do one live `agents/inventory` read) or by
**`top`**, not by the cold list — a correction this RFC adopts to stay consistent
with the status contract. `kubectl agents get -o wide` shows the *stable* facts
(mode, model, contract, ready, substrate, node, phase, age); live occupancy is a
live read.

### 6.2 Contract negotiation + graceful degradation

The CLI negotiates against the **manifest projected into status**, not the agent's
identity (P0). The discipline, in priority order:

1. **Contract major.** If `status.contract.compatible == false` (the agent's
   `contract_version` major is one the control plane does not understand — agent
   RFC 0014 §6.3 / agentctl RFC 0006 §6.1), the CLI offers **only** cold reads +
   `logs` (stderr) and labels everything else "contract-incompatible (major N)." It
   never tries a live verb it cannot frame.
2. **Surface presence → verb presence.** A verb whose backing surface is absent is
   **hidden from help and refused on invocation** with a precise message — not a
   crash and not a generic error: `kubectl agent x top` against
   `surfaces.metrics:false` prints "this agent does not advertise a metrics surface
   (surfaces.metrics:false)" and exits non-zero (§6.3). Columns degrade the same
   way: a `top` table for a metrics-less agent is not rendered empty, it is not
   offered.
3. **Additive tolerance.** An unknown *additive* field/tool/metric (a higher minor,
   or a second-vendor extension) is **tolerated**, never an error (agentd RFC 0014
   §6.3); the CLI renders what it understands and can `--show-unknown` to dump the
   rest verbatim. This mirrors the lenient-but-typed posture agentctl RFC 0001 §4.4
   and the additive-drift report agentctl RFC 0010 §5.6 take.
4. **Back-end availability.** Each back-end degrades independently (agentctl RFC
   0009 §4.5): a down aggregated APIServer disables live verbs but **cold reads
   still work**; the CLI prints "management path (aggregated APIServer) unavailable;
   cold reads still work; the operator's autonomous path is unaffected" rather than
   an opaque error. A down gateway disables `card`/`tasks` only.

The golden-fixture corpus of manifests per feature-set (ask **P3b**, agentctl RFC
0001 §4) is what the CLI's negotiation is tested against — so "hide the verb the
agent didn't advertise" is a conformance assertion, not a hope.

### 6.3 Exit codes (the CLI's own, distinct from the agent's)

The **CLI process exit code** is the agentctl/`kubectl-agent` binary's own status —
**not** the agent's exit-code contract (agentd RFC 0011 §5 / RFC 0016 §5), which is
data surfaced *by* `results`. Keeping them distinct avoids the trap of a human
reading a `4` from `kubectl agent … results` and thinking the CLI failed.

| CLI exit | Meaning |
|---|---|
| `0` | success |
| `1` | generic runtime error (unexpected) |
| `2` | usage error (bad flags/args — `clap`) |
| `3` | target not found (no such `Agent`/instance) |
| `4` | **forbidden** — RBAC denied (`403` from the aggregated layer / `auth can-i` = no) |
| `5` | **management path unavailable** (APIService down, node-agent unreachable, stream broken) |
| `6` | **not supported by this agent** — verb's backing surface not advertised, or contract major incompatible |
| `7` | gateway path error (`card`/`tasks` — gateway unreachable / auth failed) |

`kubectl agent x results` exits `0` even when the *run it reports* failed — the
run's failure is in the report's `status`/`exit_code` fields (agentd RFC 0016 §6.2),
faithfully surfaced; the CLI's `0` means "I successfully fetched the outcome." A
caller that wants to gate a pipeline on the *run's* outcome inspects the reported
fields (or uses `--exit-on-failure` to remap the run's terminal status onto the CLI
exit), never the bare CLI exit code.

---

## 7. Non-goals

- **The access path's authorization.** agentctl RFC 0009 owns the aggregated
  APIServer, the per-verb `SubjectAccessReview`, identity forwarding, the
  node-agent's per-target authz, and `attach`'s authz. This RFC is that path's
  *client*; it adds no auth.
- **The node-agent management API + discovery, and the connect-upgrade transport.**
  agentctl RFC 0008 (the API surface, connection manager) and agentctl RFC 0009
  §3.4/§5.1 (the connect subresources the CLI calls). The CLI consumes them.
- **The telemetry it reads.** agentctl RFC 0010 owns metrics scrape, the Loki
  pipeline, run-report persistence, and the metric/exit-code reading. `top`/`logs`/
  `results` are thin fronts over it.
- **The CRD/status schema and printer columns.** agentctl RFC 0003 (schema, CEL,
  `additionalPrinterColumns`) / RFC 0006 (the status projection). The CLI prints
  what those define; `-o wide` is the apiserver's columns.
- **The A2A gateway, card projection, and their auth.** agentctl RFC 0013 / RFC
  0014. `card`/`tasks` are queries against them.
- **Defining or adding auth to agent tools.** agentd RFC 0015 owns the tools;
  agentd RFC 0012 §3.8 keeps the agent auth-free. The CLI never asks the agent to
  authenticate.
- **The contract client + codegen.** agentctl RFC 0001 §4 / RFC 0018 — the CLI
  imports the generated `agent-contract-client`; it hand-rolls no wire types.
- **Web UI / dashboards.** Out of scope; the CLI is the v1 human interface and the
  Grafana dashboards (agentctl RFC 0010 §5.1) are the visual layer.

---

## 8. Open questions

1. **Krew two-manifest acceptance + completion integration.** Confirm the
   `kubectl-agent` + `kubectl-agents` two-manifest install (agentctl RFC 0001 §5)
   and the `kubectl_complete-*` helper convention for per-target verb completion
   (§2.2). Does the Krew index accept two manifests for one project cleanly?
2. **One multi-call binary (argv[0]) vs three bins.** Inherits agentctl RFC 0001 §9
   OQ #3. The *names on PATH* are fixed; the physical packaging (one multicall
   binary symlinked thrice vs three binaries) is undecided.
3. **attach lease location (§5.6).** In-memory in the aggregated APIServer (simple,
   lost on restart, no GitOps visibility) vs a `coordination.k8s.io`/CR `Lease`
   (durable, watchable, revocable out-of-band) — brainstorm §8.3. The revocation/
   kill-switch requirement (agentctl RFC 0015 OQ) leans toward a CR.
4. **attach target keying (§5.3).** Session-addressed only in v1 (P-session for
   enumeration). When does `--handle` become meaningful, i.e. when is
   handle-vs-session keying resolved in the contract (brainstorm §8.2)?
5. **Live `in-flight`/occupancy in `get -o wide` (§6.1).** Adopted here: omit from
   the cold printer set (consistent with agentctl RFC 0003 §2.2); served by
   `describe`/`top`. Confirm vs an alternative where the operator projects a coarse
   bucketed occupancy into `.status` for at-a-glance fleet triage (accepting some
   `.status` churn) — a status-contract decision for RFC 0003/0006.
6. **Standalone `agentctl` grammar.** Explicit nouns (`agentctl agents get`,
   `agentctl agent x drain`) to mirror kubectl exactly, vs a flatter
   `agentctl get` / `agentctl drain x`. The explicit-noun form transfers muscle
   memory; the flat form is terser for scripts.
7. **static `top` source (§4.1).** Read recording-rule rollups from Prometheus
   (requires a Prometheus endpoint the CLI can reach) vs a coarse occupancy the
   operator projects, vs requiring `-w` (live, aggregated) for any occupancy at all.
   Couples to agentctl RFC 0010 §5/§9.
8. **`--send` framing for non-interactive steer.** The exact `InjectEvent` body the
   CLI constructs from `--send TEXT` is pinned by ask **P-inject**; until P-inject
   freezes the shape, v1 sends a minimal free-text event and the multi-viewer echo
   is best-effort (§5.4).
9. **Per-tool input schemas in the manifest (a candidate contract ask).** Today
   `surfaces.operator_tools` is a bare name list with **no per-tool input schema**
   (agentd RFC 0015 §5.2), so a *novel* second-vendor operator tool is reachable only
   through the generic `kubectl agent <x> tool <name> [--arg …]` passthrough (§3.2) —
   typed `clap` grammar for it is impossible without a code change. Should the contract
   add an optional per-tool input-schema member to the manifest (a candidate for
   brainstorm §14), the CLI could build typed subcommands for additive vendor verbs
   dynamically, closing the last gap in the "manage other vendors unchanged" promise.

---

## 9. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo decision record: §2.1 `clap`; §3 `kube` for
  kubeconfig, Krew language-agnostic, the kubectl-plugin gap; §5 `crates/cli` three
  `bin` faces + the **distinct on-PATH names** rule + `xtask` Krew-manifest
  generation; §4 the generated `agent-contract-client` + lenient-but-typed
  negotiation (§6.2); §9 OQ #3 (one binary vs three).
- **agentctl RFC 0009** — management access path & RBAC (**the path this CLI is the
  client of**): §2.3 the cold/live split; §3.5 the `pods/proxy` stopgap surface
  (§4.3); §4.4/§4.4.1 the aggregated `management.agents.x-k8s.io` GroupVersion + the
  two-`apiGroup` ergonomic cost (§4.2); §4.5 the APIService availability/degradation
  message (§6.2); §5.1 the connect subresources each verb calls; §5.4 the RBAC roles
  (`agent-operator`/`agent-steerer`/`agent-viewer`); §6 the attach/inject gate +
  P-attach-gate this CLI surfaces (§5.5); §6.3 attach UX/lease deferred **to this
  RFC**.
- **agentctl RFC 0008** — node-agent architecture: §7 the management API surface +
  the per-target-namespace authz chokepoint behind every live verb the CLI calls;
  the connect-upgrade transport the aggregated APIServer fronts (§4.2/§5.2).
- **agentctl RFC 0003** — Agent & AgentFleet CRDs: §6.3 `additionalPrinterColumns`
  (the cold `get`/`get -o wide` columns, §6.1); §2.2 churny counts kept out of
  `.status` (why in-flight is not a cold column); §3.3 inert A2A when `surfaces.a2a`
  absent (why `card`/`tasks` are gated, §3.2).
- **agentctl RFC 0006** — operator reconcile & capability model: §4.2 the curated
  `.status` projection the cold reads serve; §6 manifest-driven rendering / graceful
  degradation off `surfaces{}` — the same negotiation discipline the CLI applies
  client-side (§3.2/§6.2).
- **agentctl RFC 0010** — observability & telemetry bridge: §5/§9 the metric series +
  recording-rule rollups behind `top`; §6.1 stderr→Loki (bulk `logs`) + the live
  `agent://events` tail; §7 the persisted run-report store behind `results` (+ ask
  P5); §10.2 the management-action audit sink behind the attach audit record (§5.4).
- **agentctl RFC 0013 / RFC 0014** — A2A gateway & task store / agent mesh identity:
  the gateway path behind `card`/`tasks` (§4.4), its own `securitySchemes` auth, and
  the centrally-signed projected card.
- **agentctl RFC 0007** — admission validation ladder: §3.3 the admission-only
  `override-trifecta` synthetic verb (a distinct, non-callable gate) — not a CLI
  verb; cited to avoid conflation with §4.2's callable runtime verbs.
- **agentctl RFC 0015** — security & multi-tenancy: the multi-tenant trust model and
  the **P-attach-gate** home behind the no-puppeting gate (§5.5); the revocation/
  kill-switch requirement bearing on the lease location (§5.6 / §8 OQ #3).
- **agentctl RFC 0018** — codegen & contract conformance: the generated
  `agent-contract-client` the CLI imports for wire types (§7 Non-goals), the manifest
  negotiation / additive-tolerance discipline (§6.2), and the P3b golden manifest
  corpus the verb-rendering negotiation is conformance-tested against.

**Contract spec (the reference implementation, agentd RFCs)**

- **agentd RFC 0015 (the reference impl's contract spec)** — management & control
  surface: §4 the operator tools (`drain`/`lame-duck`/`pause`/`resume`/`cancel`);
  §4.5 **`attach` == `subagent.send`**; §5.2 the manifest + **`surfaces.operator_tools`**
  the verb set renders from (§3.2); §5.3 `agent://inventory` behind `tree`; §5.4
  `agent://status` (and the `warm_sessions` count gap → P-session, §5.3); P-pause.
- **agentd RFC 0016 (the reference impl's contract spec)** — telemetry & lifecycle:
  §7 the `agent://events` lossy ring + cursor/`dropped` behind `logs -f` and the
  attach downlink (§5.4); §6 the run-report `report_schema` behind `results`; §5 the
  exit-code contract `results` surfaces (distinct from the CLI's own exit codes,
  §6.3).
- **agentd RFC 0005 (the reference impl's contract spec)** — self-MCP & control
  protocol: `agent://session/{session_id}` (session-addressed steering) vs
  `agent://subagent/{handle}` (handle-addressed tree) — the §5.3 keying
  distinction; `ctrl/inject` + the `InjectEvent` shape (ask P-inject, §5.4).
- **agentd RFC 0014 (the reference impl's contract spec)** — contract umbrella:
  §6.3 contract_version negotiation (additive-by-minor, refuse-unknown-major); §8
  graceful degradation off `surfaces{}` — the basis of §6.2.
- **agentd RFC 0012 (the reference impl's contract spec)** — security posture: §3.8
  the agent has no session/auth model (why the lease and multi-viewer fan-out live
  in agentctl, §5.1/§5.6).

**Contract asks raised or cited by this RFC** (brainstorm §14): **P-inject** (frozen
`InjectEvent` shape + an `inject` event in the closed vocabulary for multi-viewer
steer-echo — §5.4); **P-session** (warm-session enumeration + `steerable` flag —
§5.3); **P-attach-gate** (per-tool management gate so steering can be omitted
without dropping drain/cancel/observe — §5.5; home agentctl RFC 0009/0015);
**P-pause** (the unbuilt `pause`/`resume` tools — §3.2); **P4** (`agent://metrics`
for live `top -w` — §3.1); **P5** (run-report durability so `results` works
pod-gone — §4.1); **P-meta/P-audit** (caller `_meta` + the management-action audit
event behind the attach audit record — §5.4); **P3b** (golden manifest corpus the
negotiation is conformance-tested against — §6.2).

*Where this RFC and a contract spec disagree on the wire, the contract wins and
this RFC is corrected; where this RFC needs a primitive the contract does not yet
expose (an inject event, session enumeration, a per-tool management gate), it is a
contract ask — never a leak of cluster logic into the agent, and never auth pushed
into a data-plane binary. The CLI is a client of the contract and of agentctl RFC
0009's access path; it invents grammar and UX, never a new way to reach an agent.*
