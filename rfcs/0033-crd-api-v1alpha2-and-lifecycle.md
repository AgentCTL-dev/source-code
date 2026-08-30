# RFC 0033 — CRD API v1alpha2 and the agent lifecycle

- **Status:** Proposed
- **Date:** 2026-08-30
- **Supersedes:** RFC 0003 (CRDs); amends 0006 (reconcile) and 0017 (lifecycle)
- **Depends on:** RFC 0032 (projection), 0028/0029 (identity/gateway), 0031 (state)

## 1. The kinds (`agentctl.dev/v1alpha2`)

| Kind | Scope | Purpose |
|---|---|---|
| `Agent` | ns | one agentd instance (daemon or job) |
| `AgentFleet` | ns | N partitioned agents from one template (RFC 0034) |
| `AgentTemplate` | ns/cluster | instantiable spec + params |
| `AgentClass` | ns/cluster | scoped defaults bundle (RFC 0032) |
| `MCPService` | ns/cluster | capability registry entry |
| `ModelPool` | ns/cluster | intelligence binding (kept; gains `models:` tiers) |
| `Organization` | cluster | tenancy root (namespaces, quotas, IdP mapping, tenant gateway) |
| `Supervisor` | ns | per-user supervisor (RFC 0027) |
| `Cluster` | cluster | spoke registration (RFC 0035; Draft) |

Conversion from v1alpha1 (`Agent`, `AgentFleet`, `ModelPool`) per RFC 0005's
webhook: removed flag-era fields (`mode`, shard hints) convert to start-node/
partition equivalents with warnings recorded in status.

## 2. Agent (the heart of the API)

```yaml
apiVersion: agentctl.dev/v1alpha2
kind: Agent
metadata: { name: zendesk-triage, namespace: org-acme }
spec:
  class: default                     # AgentClass (scope chain root)
  handle: zendesk-triage             # org-unique @handle (DNS-1123; defaults to name)
  displayName: "Zendesk triage"      # human label for cards/UI
  runtime: { version: "1.3.1" }      # agentd, digest-pinned via class
  shape: daemon | job | cron         # → Deployment/StatefulSet | Job | CronJob
  schedule: "0 * * * *"              # cron shape only (external CronJob preferred per agentd docs)
  instruction:                       # prose | directive document (agentd RFC 0034) | ref
    text: |
      You triage Zendesk tickets…
  triggers:                          # sugar over agentd's 10 start kinds (§2.2)
    - schedule: { cron: "0 7 * * 1-5", tz: UTC }
    - webhook:  { path: /zendesk, auth: hmac, rate: "30/1m" }
    - subscribe: { service: drive, uri: "drive://tickets/inbox.xlsx", debounce: 500ms }
  workflows: [{setRef: triage-flows}, {inline: …}]
  skills:    [{setRef: support-skills}]
  subagents: { templates: [...], allowFreeform: false }
  services:                          # grants (narrowing over class)
    - name: zendesk        # MCPService ref; ceilings narrowed here
      allow: [ticket_read, ticket_reply]
    - name: sandbox
  intelligence: { pool: default, tier: small, budget: {...} }
  store: { class: managed | local | ephemeral, retention: {...} }
  identity:
    actingForRequired: true          # refuse autonomous runs needing user grants
    autonomousAs: "svc:triage"
  expose:
    a2a: true
    webhooks: [{ path: /zendesk, auth: hmac }]
  peers: [{agent: escalation-agent}] # east-west wiring
  lifecycle: { runUntil: drained, drainTimeout: 25s, paused: false }
  limits: {...}  priority: normal
  approval: { policy: ask, hitl: [slack:#support-approvals] }
status:
  phase: Provisioning|Ready|Paused|Draining|Degraded|Failed
  a2a: { endpoint: …, card: … }
  renderedHash: …    bundles: {...}
  runtime: { runsActive, gatesOpen, budgetRemaining, pressure }   # from A2A status
  conditions: [...]
```

Handle uniqueness per org is enforced at admission; the handle is the routing
segment at the gateway and the `@mention` token in supervisor chat (RFC 0027
§7.1; RFC 0029 §2). `metadata.labels` (e.g. `team: engineering`) are the
targets access policies select on — labeling an agent *is* placing it in the
company's access model.

## 2.1 Organization (the tenancy root)

```yaml
apiVersion: agentctl.dev/v1alpha2
kind: Organization
metadata: { name: acme }
spec:
  displayName: "Acme Corp"
  namespaces: { mode: managed }          # org-acme (+ optional extra spaces)
  quotas: { agents: 200, tokensPerDay: …, sandboxCpuSeconds: … }
  identity:
    providers: [{ issuer: https://acme.okta.com, clientRef: … }]
    claimMappings: { user: sub, groups: groups }
  accessPolicies:                        # IdP claims/scopes → roles over labels
    - match: { groups: ["okta:eng-*"] }
      role: operator
      selector: { matchLabels: { team: engineering } }
    - match: { claims: { dept: marketing } }
      role: viewer
      selector: { matchLabels: { team: marketing } }
    - match: { groups: ["okta:platform-admins"] }
      role: admin
      selector: {}                       # everything in the org
  supervisors: auto                      # auto | manual | disabled
  registryScope: { classRef: acme-defaults }
  gatewayHosts: { a2a: a2a.acme.example, hooks: hooks.acme.example }  # optional vanity
status: { namespaces: [...], members: …, tenantGatewayRef: …, phase: … }
```

The resolved policy document (per authenticated principal) is computed once by
apiserver/identity and consumed by **every** enforcement point — gateway,
apiserver verbs, mcpg tool visibility, and a mirrored K8s RBAC projection for
direct `kubectl` users — so team-scoped access ("engineering sees engineering,
admins see all") is stated once and enforced everywhere. Roles: `viewer`
(read/status), `operator` (converse, lifecycle verbs), `admin` (CRUD, registry
writes, org settings); scopes on tokens may further narrow a session below the
policy ceiling.

## 2.2 Triggers — every agentd start kind, as a one-liner

Users must be able to spin up *any* shape agentd supports — scheduled, webhook/
event-driven, MCP-resource-listening, stream-consuming, command-serving —
without authoring dialect-3 workflow YAML. `spec.triggers[]` is a typed union
over **all ten** agentd start kinds; the renderer compiles each entry into a
generated start-node workflow wrapping the instruction (the same expansion
agentd's own `--instruction` sugar performs: `start → agent → finish`),
alongside any explicit `workflows:` the spec carries.

| Trigger | agentd start | Prerequisites the renderer wires | Typical workload |
|---|---|---|---|
| `once: {}` | `once` | — | Job |
| `manual: {}` | `manual` | fired via `workflow.run` (CLI/chat) | Job or daemon |
| `loop: {interval, until?}` | `loop` | — | Deployment |
| `schedule: {cron\|every\|at, tz?, runtime?}` | `schedule` **or** external CronJob | see shape rule below | CronJob / Deployment |
| `webhook: {path, auth, methods?, rate?, idempotency?, respond?}` | `webhook` | `webhooks.listen` + HMAC secret + optional `expose.webhooks` route | Deployment |
| `subscribe: {service, uri, debounce?, window?, filter?}` | `subscribe` | the `MCPService` grant (admission refuses a subscribe against an ungrated service) | Deployment |
| `stream: {stream, subject?, from?, rate?}` | `stream` | the `streams:` declaration | Deployment |
| `signal: {name, filter?}` | `signal` | — | Deployment |
| `event: {name, filter?}` | `event` | closed runtime-event vocabulary validated | Deployment |
| `a2aCommand: {command, schema?, roles?}` | `a2a` | `a2a.listen` (already on) — registers a typed command on the listener | Deployment |

**Shape is inferred but always rendered explicit.** Any long-lived trigger ⇒
`shape: daemon` with `run_until: drained` (never agentd's `auto` — its
webhook/stream misclassification is a known trap); only `once`/`manual` ⇒
`shape: job`. A *sole* `schedule` trigger defaults to `schedule.runtime:
external` — a CronJob, agentd's own documented preference — and flips to
`runtime: internal` (a `schedule` start inside a daemon) when combined with
other triggers or when the spec asks for store continuity between firings.
Explicit `spec.shape` always wins over inference.

Sugar and explicit workflows **compose**: triggers generate `main-<kind>`
workflows; admission warns on overlaps (e.g. a sugar webhook path colliding
with a declared `webhook` start). Fleet templates and `AgentTemplate.params`
parameterize triggers like any other field, so "a scheduled report agent" is a
template a user instantiates with a cron expression — from the CLI
(`agentctl create agent --from-template report --set schedule.cron='0 7 * * 1'`)
or by telling their supervisor, whose `control.agents.create` takes the same
typed `triggers` (RFC 0027 §5).

## 3. Reconcile (amending RFC 0006)

Render (RFC 0032) → admission → workload + volumes + Secrets + Certificates +
NetworkPolicies + principal projection → gateway route registration → status
loop (A2A `status` command + metrics → `.status.runtime`; exit intents →
`podFailurePolicy` on job shapes). Diffs classify reload-vs-restart via the
vendored RESTART_ONLY set; restarts are rolling and drain-first (A2A `drain`,
then delete pod; 28 s worst case < grace).

## 4. Lifecycle verbs (req. 24) — CLI/control-MCP → apiserver → operator

| Verb | Semantics |
|---|---|
| `pause` / `resume` | A2A admin verbs; durable, reversible; `spec.lifecycle.paused` for declarative parity |
| `drain` | graceful quiesce (intake off, runs finish) |
| `stop` / `start` | drain → scale 0, store retained → scale 1 resumes (managed/local) |
| `backup` | state snapshot (state.admin.snapshot per prefix) + artifacts manifest + rendered-config hash → S3; on-demand + `spec.backup.schedule` |
| `restore` | new generation from snapshot (same `agent.name`, `--fresh` semantics respected); cross-namespace/cluster capable |
| `migrate` | managed: drain → reschedule (state external, free); local: drain → PVC snapshot/move; cross-cluster: backup→restore (RFC 0035) |
| `upgrade` | runtime version bump per class rollout policy; canary via fleet partitions; pinned definitions drain in-flight runs |
| `reset` | explicit `--fresh` (generation bump; store kept, ignored) |
| `delete` | drain → final backup per policy → remove; store retention window before purge |

Every verb is audited with actor + `acting_for`; destructive verbs honor the
approval policy (gate to the acting human when configured).

## 5. Durability invariants

- `managed` store ⇒ the pod is cattle; identity fence = `agent.name` (seq CAS
  conflict = split-brain alarm, surfaced as a condition, never auto-resolved).
- One writer per store identity enforced by the renderer (unique names per
  replica; file stores never shared — agentd flocks anyway).
- Backups are consistent-enough by construction (agentd checkpoints before
  effects; snapshot = point-in-time of the durable log; restore replays exactly
  like a crash restart — the semantics agentd already proves under SIGKILL).

## 6. Open questions

1. `shape: cron` — external CronJob (agentd-preferred) vs internal `schedule`
   start for sub-minute/jittered needs: ship both, document the default
   (external).
2. PodDisruptionBudget defaults for daemons (lean: on, maxUnavailable 0 for
   `managed`-store singletons, drain-aware eviction handler in P3).
3. Whether `Supervisor` folds into `Agent` with a class at GA — kept separate
   for RBAC clarity until proven redundant.
