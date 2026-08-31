# agentctl reference

The exhaustive element-by-element reference: every component, every custom
resource (field by field), the management API, the config/env conventions, and
the operational nuances. For the *why* and the flows, read
[docs/v2/ARCHITECTURE.md](v2/ARCHITECTURE.md); this is the *what*.

- [1. Components](#1-components)
- [2. Custom resources](#2-custom-resources)
- [3. The management API](#3-the-management-api)
- [4. Config & env conventions](#4-config--env-conventions)
- [5. Nuances](#5-nuances)

---

## 1. Components

Every control-plane component is one distroless, non-root, read-only-rootfs
container image, hand-rolled in Rust on the `kube-rs`/`axum`/`rustls` stack (the
platform is Rust-only and OpenSSL-free). The two data-plane services (`state`,
`tenant mcpg`) are built on the Apache-licensed **mcpg** governance gateway image.

### Control plane

| Component | Crate | Serves | What it does |
|---|---|---|---|
| **operator** | `agentctl-operator` | — (controller) | The reconcile engine. Watches the CRD family and renders each into a workload: it projects the agent's config directory (`-c services.json -c agentd.json`), resolves defaults/floors through the `AgentClass` chain, issues per-workload serving certs (cert-manager) and distributes the CA, reconciles per-namespace NetworkPolicies, wires the KEDA `ScaledObject`, creates managed org namespaces (owner-ref GC + `ResourceQuota`), provisions per-org tenant gateways and supervisors, and choreographs guarded shard resizes. Leader-elected for HA. |
| **apiserver** | `agentctl-apiserver` | `:6443` (aggregated API, mTLS) | The `management.agentctl.dev/v1alpha1` aggregated API. Connect verbs on `Agent`/`AgentFleet`, the state-plane lifecycle verbs, and the `metering/export` + `audit/query` read paths. Front-proxy-authenticated (rustls requires the requestheader CA), then `SubjectAccessReview`-authorized per call. |
| **admission** | `agentctl-admission` | `:8443` (webhooks) | Validating + mutating + conversion (`/convert`) webhooks. Storage version is `v1alpha2`; a v2-spec annotation stash preserves v2-only fields across v1-mediated writes. |
| **identity** | `agentctl-identity` | `:8087` (HTTPS) | OIDC federation (issuer-pinned discovery + JWKS), RFC 8628 device flow, an AES-GCM sealed credential custody (Postgres or memory), the RFC 8693 `/v1/exchange` token endpoint, the connections consent flow, and the AAuth agent-identity provider (`/enroll`, `/agent-token` over RFC 9421 signatures; `/.well-known/aauth-agent.json`). The fail-closed admin gate refuses without a token. |
| **gateway** | `agentctl-gateway` | `:8080` (A2A/HTTP) | The tenant data-plane front door. Org routes `/orgs/{org}/{agents,fleets,supervisor}/…`, Agent Cards + `message/send`/`message/stream` (SSE), the external `/hooks/{ns}/{name}/…` ingress, HITL channel fan-out, supervisor auto-ensure + idle-park, and inbound OIDC introspection that stamps the caller's per-(user,agent) principal bearer upstream. Reaches agents by mTLS as the **Management** origin. |
| **control** | `agentctl-control` | `:8443` (HTTPS MCP) | The `control.*` MCP server — `control.agents.{list,get,status,resolve,create}` and `control.subagents.create`, each scoped server-side to the verified caller's namespace (no namespace argument on the wire). Auth is RFC 9421 jwt-scheme verification against the identity provider's JWKS. |
| **coordination** | `agentctl-coordination` | `:8080` (HTTP MCP) | The `work.*` claim hub: submit / claim / renew / ack / release / stats / result / deadletter, with exactly-one-owner leasing, a result-correlation channel, dead-lettering after `maxAttempts`, and an in-memory or durable-Postgres store. The backlog is the scale-from-zero signal. |
| **scaler** | `agentctl-scaler` | `:9090` (KEDA gRPC) | A KEDA external scaler reading the coordination backlog and per-fleet `inbox_pending` metric, with downscale damping so a claim fleet scales elastically (and from zero). |
| **sandbox** | `agentctl-sandbox` | `:8080` (HTTP MCP) | The `sandbox.run` backend: agent-authored code executes in a single-use, network-denied, capability-stripped pod inside a dedicated cell namespace, with a warm pool for latency and an optional `runtimeClassName` (Kata/gVisor). |
| **artifacts** | `agentctl-artifacts` | `:8080` (HTTP MCP) | The `artifacts.put/get/list` backend over S3 (bundled MinIO). Objects are keyed under the caller's org prefix; a per-org byte quota is checked before each write. The S3 client is hand-rolled on reqwest + SigV4. |
| **state** | mcpg image + `state-config` | `:8787` (HTTPS MCP) | The `state.*` seq-CAS checkpointer for `store.class: managed`. A governed SQL binding on mcpg over Postgres, TLS-serving (agentd refuses plaintext MCP off-loopback), with the P3-2 server-side tenant fence (`param_exprs: identity.subject_id`). |
| **audit** | `agentctl-audit` | (library + sink + shipper) | The `audit/v1` record vocabulary, a Postgres sink, the `audit/query` API, and an `audit-shipper` sidecar that tails per-org gateway records into the one trail (org forced from its token). |
| **postgres** | (bundled) | `:5432` | The default durable store for coordination, identity custody, metering, audit, and managed state. Production points these at your own database. |

### Per-org data plane (operator-provisioned)

| Component | Built on | What it does |
|---|---|---|
| **tenant mcpg** | mcpg image | The org's governance **capability plane**: a proxy-only gateway that federates the org's `MCPService` registry to its agents, governed — tool-name filtering (`exact`/`*`/`prefix*`), per-user credential injection via the exchange plugin (`auth.mode: obo`), and the verified-caller trust tier (JWKS from identity). `control` is never federated. |
| **supervisor** | rendered `Agent` | One per user: their personal agent, owner-ref'd to a `Supervisor` CR, with an owner-only principal and the class's supervisor instruction layer. |
| **agent pods** | `agentd` (contract-conformant) | The workloads themselves — a Job, CronJob, Deployment, or StatefulSet per the CR's `shape`/fleet mode. |

### Libraries & tools (not deployed as services)

`agent-api` (CRD types + the pure policy engines: registry resolver, org access,
scope chain), `agent-config` (the two-layer config projection + restart-hash),
`agent-contract-client` (reads a conformant agent's contract manifest),
`agentctl-metering` (the `metering/v1` billing vocabulary + PG sink + export),
`agentctl-telemetry` (OTLP init), `agentctl-crdgen` (writes `deploy/crds/`),
`mock-agent` (conformant test stand-ins), and the **`agentctl`** CLI.

---

## 2. Custom resources

Group **`agentctl.dev/v1alpha2`** (storage version). Every kind shares the
`conditions[]` + `Ready`-column status idiom.

### 2.1 Organization (`org`, cluster-scoped)

A tenant. Deleting it deletes its managed namespaces.

| Field | Meaning |
|---|---|
| `displayName` | Human-facing name for listings/dashboards. |
| `namespaces` | `managed` (default — the operator creates + owns `org-<name>`, GC on delete) or `unmanaged` (the platform team owns namespaces; the org only references them). `extra[]` adds more managed namespaces. |
| `quotas` | Tenant ceilings. `agents` is enforced as a `ResourceQuota` on the managed namespaces; the metering ceilings (tokens/day, sandbox CPU) are recorded for the billing plane and enforced there. |
| `identity` | IdP binding: which issuers authenticate this org's members and how their claims map to user + groups. |
| `accessPolicies[]` | Claims/groups → roles over label selectors. Evaluated top-down; **every** matching rule grants (a principal holds the union of its grants). |
| `supervisors` | Member supervisor provisioning: `auto` (default) / `manual` / `disabled`. |
| `registryScope` | The org's defaults `AgentClass` (resolved in the scope chain). |
| `gatewayHosts` | Optional vanity hosts for the tenant gateway endpoints. |

### 2.2 Agent (`agent`, namespaced)

One agent: an instruction + typed triggers + scoped bindings.

| Field | Meaning |
|---|---|
| `class` | The `AgentClass` this agent resolves defaults/floors through. Absent ⇒ the org's `default` class. |
| `handle` | Org-unique @handle (DNS-1123; defaults to the CR name) — the address for `@mention` and peer wiring. |
| `displayName` | Human-facing display name. |
| `runtime` | `{version?, image?}` — the agent image. `version` resolves to a digest via class policy; an explicit `image` wins (the v1 `spec.image` path). |
| `shape` | `daemon` / `job` / `cron` — the rendered workload. Inferred when omitted: any long-lived trigger ⇒ daemon; `once`/`manual` only ⇒ job; a sole `schedule` ⇒ cron. |
| `schedule` | Cron expression for `shape: cron` (the external-CronJob path). |
| `instruction` | `{text?, configMapRef?}` — the instruction document (prose or an agentd directive document). |
| `triggers[]` | Typed wake sources over agentd's ten start kinds; each compiles to a generated `main-<kind>` workflow. Exactly one kind per entry (see [2.9](#29-trigger-kinds)). Max 32. |
| `workflows[]` | Explicit workflow documents (inline / configMapRef / setRef), composed with the trigger sugar. |
| `skills[]` | Skill-bundle references (`SkillSet` names). |
| `services[]` | **Grants** of `MCPService` entries with per-agent narrowing (`allow` shrinks the entry's ceiling, never widens). |
| `mcpServers[]` | Inline direct-dial MCP servers (the v1 spelling; deprecated — prefer an `MCPService` + grant). |
| `intelligence` | `{pool, tier?/model?, budget?}` — the model binding. |
| `store` | `{class, size?}` — durable-state class: `managed` (the state service) / `local` (a PVC StatefulSet) / `ephemeral` (emptyDir; the default). |
| `identity` | Workload identity + delegation posture (opt into an AAuth identity). |
| `access` | Per-agent access policy (named principals, per-agent OIDC). |
| `expose` | `{a2a?, webhooks?}` — what the gateway exposes for this agent. |
| `peers[]` | East-west peer wiring (same-org agent handles the owner may reach). |
| `lifecycle` | `{paused?, idleParkSeconds?, runUntil?}` — declarative pause/park. |
| `limits` | `{maxTokens?, lifetimeTokens?, maxDepth?, maxSteps?}` — narrowing over the class. |
| `capabilities` | `{exec?, egress?, secrets?}` — the dangerous-capability legs the **lethal-trifecta** gate governs at admission. |
| `priority` | Scheduling priority band. |
| `approval` | `{policy}` — HITL policy: `never` / `ask` / `deny` (risky actions pause for a human). |

**Rendered workload:** `job` → Job; `cron` → CronJob; `daemon` → Deployment (or,
with `store.class: local`, a StatefulSet with a PVC).

### 2.3 AgentFleet (`afleet`, namespaced)

A replicated worker set.

| Field | Meaning |
|---|---|
| `template` | The per-replica worker `AgentSpec`. |
| `scaling` | `{mode, minReplicas?, maxReplicas?, shards?, target?}` — `claim` (elastic, KEDA) or `shard` (fixed partitions). |
| `partitioning` | v2 strategy: `static` (fixed member set — per-member `vars[]` overlays, ordinal-0 `singletons[]`), `dispatcher` (owner + workers behind a fleet route), or `workqueue` (members pull `work.*` leases). |
| `work` | Work-fabric policy: `{maxAttempts, claimTtl}` (dead-letter threshold + lease TTL). |
| `coordinator` | Optional main agent — its own `template`, `replicas`, and `distribution` (`queue`/`a2a`); renders an extra Deployment and becomes the fleet's A2A front door + work producer. |
| `budget` | `{kind, maxUnits, windowSeconds}` — per-fleet budget read from the metering export; breach **pauses intake** (pool → 0 for the rest of the window, `BudgetExceeded`), loss-free for leased items via redelivery. |
| `replicas` | Fixed replica count (shard/static). |

**Rendered workload:** `claim` → Deployment (KEDA owns replicas); `shard`/`static`
→ StatefulSet of `N` partitions; a `coordinator` adds a Deployment.

### 2.4 AgentClass (namespaced)

The scoped **defaults + security floors** an agent resolves through. The scope
chain is system → org → group; lower scopes **narrow only**.

| Field | Meaning |
|---|---|
| `parent` | The parent class in the chain. Absent ⇒ a chain root. |
| `groupSelector[]` | This class applies to principals matching the selector (a class with a selector *is* the group scope). |
| `defaults` | Content defaults shadowed by name downward: `{runtime, limits, budget, approval, priority, store, intelligence}`. |
| `services[]` | Service grants agents of this class may draw on. |
| `workflowSets[]` / `skillSets[]` | Bundle refs the class contributes. |
| `floors` | **Security floors** — lower scopes may only narrow (widening is an admission error naming the floor): `capabilities` (max trifecta legs), `egress` (`closed` forbids un-granted egress), `budget` (ceiling), `tools[]` (max tool-name patterns), `approval` (may not weaken below). |
| `supervisor` | The supervisor profile agents-of-users in this class get: `{instruction, services[], budget}` (users override the instruction with **prose only**). |
| `hitl[]` | HITL notifier channels. |

### 2.5 AgentTemplate (namespaced)

An instantiable agent spec with typed holes.

| Field | Meaning |
|---|---|
| `template` | The `AgentSpec` to instantiate; `{{params.X}}` holes fold in at create. |
| `params{}` | The **only** holes an instantiation may fill: `{type (string/number/boolean/cron/duration), default?, description?, required}`, schema-validated. |

### 2.6 MCPService (namespaced)

A capability-registry entry — valid at every scope, user included.

| Field | Meaning |
|---|---|
| `kind` | `mcp` / `peer` / `http` (agentd 1.3.1's catalog accepts only `mcp` today; the renderer projects the others by other means). |
| `endpoint` | The service endpoint (`https://…`). |
| `deploymentRef` | An in-cluster Deployment/Service this entry fronts (the operator resolves the endpoint). |
| `tags[]` | Capability tags — **unconditional floors**: `egress`, `secrets`, `exec`, `untrusted-content`, … |
| `allow[]` / `exclude[]` | Tool-name ceiling (consumers narrow, never widen). Grammar: `exact` / `*` / `prefix*`. |
| `auth` | How agents authenticate: `mode` (`service` = shared credential / `obo` = per-user token via the RFC 8693 exchange / `passthrough`), `audience?` (RFC 8707 resource), `secretRef?`. |
| `rate` | Default arrival rate (`<burst>/<per>`). |
| `direct` | Dial straight from the agent pod (the AAuth posture) instead of through the tenant mcpg. |

### 2.7 ModelPool (`mp`, namespaced)

A direct-dial model endpoint registry — not on the data path (no broker, no meter).

| Field | Meaning |
|---|---|
| `provider` | Provider id (`mock` / `anthropic` / `openai` / …; free string). |
| `endpoint` | Provider base URL the agent dials directly — rendered into the pod as `INTELLIGENCE`. |
| `credentialSecretRef` | Optional `{name, key}` provider key. Present ⇒ the operator mounts it as the agent's `INTELLIGENCE_TOKEN` (the agent holds the key). Absent ⇒ the agent authenticates by its AAuth identity (secret-free). |
| `models[]` / `defaultModel` | Allowed model ids and the default. |

### 2.8 Supervisor (namespaced)

A user's personal agent — the on-behalf-of anchor.

| Field | Meaning |
|---|---|
| `user` | The canonicalized IdP subject this supervisor serves. |
| `paused` | Declarative pause (idle-park sets this). |
| `instructionOverride` | The user's **prose-only** instruction layer (the renderer strips machinery). |
| `budgetOverride` | Budget narrowing below the class ceiling. |
| status `ownerGroups[]` | The owner's identity-resolved groups, **stamped by the gateway** at each introspection — the supervisor never asserts its own groups; stale grants age out at the owner's next conversation. |

### 2.9 Trigger kinds

Exactly one per `triggers[]` entry (CEL-enforced). Each compiles to a
`main-<kind>` start-node workflow.

| Kind | Fields | Wakes on |
|---|---|---|
| `once` | — | Run once to completion (job shape). |
| `manual` | — | Only when invoked (no automatic wake). |
| `loop` | `{interval, until?}` | A cadence (`30s`, `5m`). |
| `schedule` | `{cron?/every?, tz?, runtime?}` | A cron/interval. `runtime: external` → CronJob (the default for a sole schedule); `internal` → an agentd schedule start in a daemon. |
| `webhook` | `{path, auth?, methods[], rate?, idempotency?}` | An external HTTP delivery through the gateway hooks ingress. `auth` is `hmac` (default) or `bearer` — agentd refuses unauthenticated non-loopback routes. |
| `subscribe` | `{service, uri, debounce?, filter?}` | An MCP resource on a **granted** service changing. |
| `stream` | `{stream, subject?, from?, rate?}` | Messages on a declared stream. |
| `signal` | `{name, filter?}` | A process/logical signal. |
| `event` | `{name, filter?}` | An agentd runtime event (named `name`, not `on` — a bare `on:` YAML key parses as boolean `true`). |
| `a2aCommand` | (typed command) | A typed A2A command arriving (needs a principal grant). |

---

## 3. The management API

Aggregated API group **`management.agentctl.dev/v1alpha1`**, RBAC-gated by
`SubjectAccessReview`. Connect verbs POST to
`/apis/management.agentctl.dev/v1alpha1/namespaces/{ns}/agents/{name}/{verb}`
(and `agentfleets/…`, which fan out to every replica).

| Verb | CLI | Effect |
|---|---|---|
| `drain` / `lame-duck` / `cancel` | `agentctl drain` … | Runtime verbs forwarded to the agent pod over mTLS (finish in-flight, stop taking work, cancel a run). |
| `pause` / `resume` | `agentctl pause` … | Runtime pause/resume on the pod. |
| `stop` / `start` | `agentctl stop` … | **Infra** park/wake — patch `lifecycle.paused` (operator → replicas 0/back). Durable state persists. |
| `backup` / `restore` / `reset` | `agentctl backup -o f` … | State-plane: snapshot / restore / purge a managed agent's checkpoint via the state service admin tools. |
| `migrate` | `agentctl migrate` | Reschedule a **managed** agent's pod, proving the checkpoint is preserved (refuses ephemeral/local). |

Read paths: `GET …/metering/export?from=&to=` (billing) and
`GET …/audit/query?...` (the trail). Both SAR-gated.

---

## 4. Config & env conventions

- **The downward API.** The operator renders each agent's config as a directory
  and passes `-c services.json -c agentd.json` (reference-never-restate: the
  second layer references, never re-states, the first). Provider + MCP endpoints
  land as env: `INTELLIGENCE` (the ModelPool endpoint), `INTELLIGENCE_TOKEN` (the
  mounted provider key, when the pool has one), and per-service headers/tokens for
  governed calls.
- **Restart vs. reload.** The operator rolls a pod only when a *restart-required*
  path changes, computed as a hash over agentd's authoritative `RESTART_ONLY`
  set ∪ `{a2a.principals, webhooks}` (both of which agentd neither refuses nor
  reloads — a silent strand, so the platform treats them as restart-required).
  Restart-required: `config_version`, `agent.name`, `store.*`, `lifecycle.*`,
  listeners/TLS/bearer/principals, `observability.*`, the whole `security`
  subtree, `webhooks`. Everything else (instruction, persona, `intelligence.*`,
  `mcp.servers[]`, `workflows[]`, skills/tools) hot-reloads through the ConfigMap.
- **Secrets are landed before config.** Admission catches a dangling
  `{{secret:…}}` ref; the operator lands referenced Secrets *before* the config
  so agentd's startup secret-resolution never exits non-zero.
- **DNS.** In-cluster calls use trailing-dot FQDNs and `ndots:1` where a pod dials
  public registries, to dodge a cluster wildcard search domain capturing them.

Chart values (planes on/off, images, credentials, quotas, netpol, TLS) are
documented in [`charts/agentctl/README.md`](../charts/agentctl/README.md) and
schema-validated by `values.schema.json`.

---

## 5. Nuances

The details that bite — each with the reason it exists.

- **A grant narrows, never widens.** An agent's `services[]`, `limits`,
  `capabilities`, and `budget` can only *shrink* what its `AgentClass` floor
  allows. Widening is an admission error that names the floor. This is how a
  tenant customizes within a plan they can't escape.
- **Attribution is stamped, not asserted.** The gateway stamps org membership and
  `ownerGroups` from the introspected token; a supervisor cannot claim its own
  groups, and the audit shipper's org comes from its token, not its payload. OBO
  acting-user is a signed claim the workload cannot mint.
- **Managed state & artifacts are fenced by host-supplied identity.** Every
  `state.*` row and `artifacts.*` object is keyed under the caller's asserted
  subject prefix, bound host-side (`param_exprs: identity.subject_id` for state;
  the org prefix from `x-mcpg-subject-id` for artifacts). No caller argument can
  reach another tenant's data; a call with no identity fails closed.
- **`migrate` requires a managed store.** Ephemeral (emptyDir) and local (PVC)
  state are node-local; only `managed` state lives in central Postgres, so only a
  managed agent can be rescheduled with zero run loss. `migrate` refuses the
  others with a 409.
- **A templated instruction can retire a workflow.** An embedded `:::workflow`
  directive in a *templated* instruction is a workflow to agentd, so an innocuous
  prose edit that removes it *retires* that workflow under its unload policy. Keep
  real workflow definitions in `spec.workflows[]`.
- **Config hot-reload never fires on kubelet `..data` swaps** in some kernels
  (an inotify basename-filter interaction) — the platform therefore keeps a
  restart-on-restart-required-change posture rather than relying on the agent's
  live reload for those paths.
- **NetworkPolicies need a policy-enforcing CNI.** The chart renders every
  policy — the sandbox deny-all, the per-tenant boundary, the identity/state
  perimeters — but **kindnet ignores them**. Without Calico/Cilium the sandbox's
  network isolation and cross-tenant isolation are inert. This is the single most
  important production prerequisite.
- **Internal MCP services disable the anonymous rate limiter.** The `state`
  service is reachable only by netpol-admitted pods and fences every row by
  subject, so it sets `anonymous_rate_limit_per_min: 0` — mcpg's per-IP limiter
  would otherwise throttle a real header-asserted agent (default 600/min + 100
  burst = a stall at 10 writes/s). Keep the limiter armed on any gateway facing
  untrusted callers.
- **The state gateway serves TLS.** agentd refuses to dial a plaintext MCP
  endpoint off-loopback, so any gateway a real agent dials directly (state, a
  tenant gateway) must serve HTTPS with a cert the agent trusts via the chart CA.
- **A failed Helm upgrade drifts silently.** A FAILED upgrade still applies
  manifests the next 3-way merge can't see (mystery 401s / env drift); heal with
  `helm upgrade --force`. One un-Ready deployment wedges every `--wait`.
- **Disk pressure looks like a database bug.** A full node disk (cargo targets +
  image rebuilds) crash-loops the bundled Postgres and reads like a connection
  bug; recover with `cargo clean` + an image prune.

The complete, always-current set of hard-won operational facts lives alongside the
implementation register in [docs/v2/PLAN.md](v2/PLAN.md).
