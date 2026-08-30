# RFC 0032 — the registry and config projection

- **Status:** Proposed
- **Date:** 2026-08-30
- **Decisions:** ADR-0007, ADR-0010 · **Supersedes-in-part:** RFC 0004 (AgentClass
  returns, re-founded), amends 0007 (admission), 0020 (instruction sourcing)

## 1. Motivation

Req. 12, 20, 25, 26, 32: capabilities, workflows, skills, settings, instructions
and HITL channels are defined centrally, scoped per tenant, overridable
downward, and **provisioned into agents** exactly the way agentd wants to
receive them — a layered directory. The registry is the source of truth; the
projection is the compiler.

## 2. Scopes and resolution

Four scopes, resolved most-specific-last for *content*, with **monotonic
narrowing for security-bearing fields**:

```
system  (infra namespace; chart-seeded)
  → organization  (AgentClass in org namespace)
      → group     (AgentClass with groupSelector)
          → user  (connections, supervisor override, personal templates)
```

- Content (workflows, skills, templates, prompts): lower scopes may **add or
  shadow by name**.
- Security floors (trifecta tags, egress mode, budget ceilings, tool ceilings,
  approval requirements): lower scopes may **narrow only**; widening is an
  admission error naming the violated floor. This mirrors agentd's own
  catalog-tags-as-floors semantics, so the model holds end-to-end.

## 3. Registry kinds

| Kind | CR / storage | Notes |
|---|---|---|
| `MCPService` | CRD | endpoint or in-cluster deployment ref; `kind: mcp\|peer\|http`; tags (floors), allow/exclude (ceilings), `auth: service\|obo\|passthrough` + audience, rate/breaker defaults, `direct: bool`. **Valid at every scope, user included** — a user-scoped entry lands in that user's catalog slice (their supervisor + agents they own, where the class permits). **External SaaS defaults to `auth: obo` federated through the org mcpg** — that federation is what buys the credential UX: connect once, every agent acting for you carries a fresh scoped token (RFC 0030) |
| `AgentClass` | CRD | the scoped defaults bundle: settings fragments (limits, budgets, approval, priority, store class, agentd version/digest), service grants (MCPService refs), workflow/skill/context bundle refs, subagent templates, supervisor profile (instruction + grants), HITL channels |
| `AgentTemplate` | CRD | instantiable agent spec + `params` schema (control MCP + CLI) |
| `WorkflowSet` / `SkillSet` | CRD → ConfigMap/OCI ref | named bundles of `*.yaml` workflows / `*.md`+`SKILL.md` skills; content-hashed |
| `ModelPool` | CRD (existing) | provider endpoints, failover order, model tiers, token secrets |
| HITL channels | in `AgentClass.spec.hitl` | Slack/email/webhook notifier configs per org/group/user |

CRUD: CRs directly (GitOps), `agentctl registry …`, and read-only `control.
registry.*` tools. Everything content-addressable (hashes in status) so
projections are diffable and rollbacks exact.

## 4. Projection (the compiler)

For each `Agent`, the operator resolves: system defaults → org class → group
class → `Agent.spec` and emits the rendered directory:

```
/etc/agentctl/
  services.yaml        # catalog layer: services entries + streams + shared vars
  agentd.yml           # instance layer: agent, intelligence(+tiers), store, a2a
                       # (listener, principals refs, peers), webhooks, lifecycle,
                       # limits, budgets, security (egress: closed, policies),
                       # identity (aauth enroll, autonomous_as), observability
  workflows/  skills/  subagents/  context/     # folder conventions
```

Renderer rules (encoded as tests, per the agentd v1.3.1 analysis):

1. Invocation is `-c services.yaml -c agentd.yml`; folders sit beside the last
   config; folder adoption is only-when-absent, so the renderer either writes
   the folder or writes the explicit key — never both.
2. Lists replace across layers ⇒ cross-layer variability rides `vars:` +
   `{{config.*}}` (partitions, per-replica names); secrets only as
   `{{secret:}}`/`{{secret-file:}}` refs.
3. `lifecycle.run_until` explicit; `agent.name` from downward API; drain <
   pod grace; probes and `podFailurePolicy` from the intent table.
4. Workflow documents must carry `name:` (dir entries are not stem-named);
   projected folders suppress the sugar `main` loop deliberately.
5. `Agent.spec.instruction` accepts prose, a ConfigMap ref, or a full
   directive-format **instruction document** (agentd's RFC 0034) — directives
   allowed from *registry/spec* sources only; anything user-conversational is
   folded as data (RFC 0027 §4).
6. The renderer never sets a custom `skills.reference_prefix`: the interface
   composer hardcodes `@skill:` (the daemon does not send the configured
   prefix to clients — upstream note, PLAN U8), and the explicit form is what
   keeps bare `@<handle>` agent mentions collision-free (RFC 0027 §7.1).

## 5. Admission ladder (amending RFC 0007)

1. Schema/structural validation of CRs (vendored JSON schemas: fast, advisory).
2. **Binary validation**: `agentd --validate-config` (matching pinned version)
   against the rendered directory in a sandboxed, offline job; exit 2 output is
   surfaced verbatim on the CR condition; the printed effective surfaces
   (`config.effective_server` lines) are recorded into status for diffing.
3. Policy floors: trifecta legs, egress closure, budget/tool ceilings vs scope
   chain; tag-laundering checks (an `MCPService` edit that would widen a live
   agent's legs is refused, not silently applied).
4. Impact classing: reloadable vs restart-only diff (agentd's RESTART_ONLY set,
   vendored) → operator chooses reload path or rolling restart; either way live
   runs finish under pinned definitions.

## 6. Drift and delivery

- Projection outputs are hashed; `Agent.status.renderedHash` + per-bundle hashes
  make `kubectl agent describe` show exactly which registry versions an agent
  runs.
- Bundle updates roll fleet-wide with rate limits (maxUnavailable-style knobs on
  AgentClass) — a bad skill/workflow push cannot restart the world at once.
- The registry seeds at install (system scope): foundational MCPService entries
  (state/artifacts/control/work/sandbox/hitl), default supervisor class, base
  AgentClass, starter templates.

## 7. Open questions

1. OCI-artifact bundles vs ConfigMaps for large workflow/skill sets — v2.0 ships
   ConfigMaps (size caps enforced), OCI in P7.
2. A `RegistryExport/Import` flow for cross-org sharing — deferred.
3. Per-user personal `WorkflowSet`s (beyond supervisor override) — deferred until
   a concrete need; the scope machinery already permits it.
