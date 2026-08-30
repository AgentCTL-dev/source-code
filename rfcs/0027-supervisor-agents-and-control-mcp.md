# RFC 0027 — supervisor agents and the control MCP

- **Status:** Proposed
- **Date:** 2026-08-30
- **Depends on:** RFC 0028 (identity/OBO), 0031 (system mcpg), 0032 (registry),
  0033 (CRDs) · **Decisions:** ADR-0006, ADR-0003

## 1. Motivation

The v2 control experience is conversational: each user has a personal
**supervisor** they steer their agent estate through, over A2A (req. 4, 7, 14,
20). Agents themselves need a governed way to create *cluster-level* subagents
instead of only in-pod processes (req. 6). Both reduce to the same design: a
stock agentd instance whose hands are a **control MCP** whose authority is
OBO-derived from the acting human.

## 2. The Supervisor resource

```yaml
apiVersion: agentctl.dev/v1alpha2
kind: Supervisor
metadata: { name: sup-andrii, namespace: org-acme }
spec:
  user: "andrii@tsok.org"          # IdP subject (canonicalized)
  paused: false
  instructionOverride: |            # prose-only user layer (see §4)
    Prefer terse answers. Never create agents with egress without asking me.
  budgetOverride: { windows: [{per: day, tokens: 500000}] }   # ≤ class ceiling
status:
  agentRef: sup-andrii              # the rendered Agent
  phase: Ready | Provisioning | Paused | Degraded
  lastConversation: "2026-08-30T09:12:00Z"
```

- **Ensured automatically**: on a user's first authenticated request, the
  apiserver creates the CR (org policy `supervisors: auto | manual | disabled`).
- The operator renders it into an `Agent` with `class: supervisor` drawn from the
  org's `AgentClass.spec.supervisor` profile.
- Deleting the CR removes the agent; the conversation store follows the org's
  retention policy.

## 3. The supervisor's rendering

From the class profile (system default → org override; RFC 0032):

- **Runtime**: durable daemon (`run_until: drained`), `store.class: managed`,
  small limits, `priority: normal`, budget windows (user-overridable downward).
- **Grant** (services catalog): `control.*` (full user scope), `state.*`,
  `artifacts.*`, the org tenant gateway, **no direct internet**, model tiers per
  class. Trifecta: the supervisor holds `sensitive` (control surface) and speaks
  to users (`untrusted_input`); it gets **no egress leg** — outbound effects
  happen in the agents it creates, keeping the supervisor outside the lethal
  trifecta by construction.
- **A2A**: listener with principals for its user (full), org admins (operator
  verbs), and the gateway; addressed-gate support on.
- **Approval posture**: destructive control verbs (`agents.delete`,
  `fleets.resize`, budget raises) are wrapped in workflows with `human` gates
  addressed `to: {id: "user:<subject>"}` — the supervisor *asks its own human*
  before irreversible acts; org policy can widen/narrow the list.
- **Workflows**: class-provided maintenance workflows (daily estate digest,
  budget watch via `event: budget.exhausted`, gate-escalation via HITL bridge).

## 4. Instruction layering

Rendered as one instruction document in agentd's directive format (agentd
RFC 0034), composed by the operator:

```
[system default persona + operating rules]     (registry: system scope)
[org overlay]                                   (AgentClass.supervisorInstruction)
<user-override>                                 (Supervisor.spec.instructionOverride)
[machinery: :::config / :::tools per class]     (never from user text)
```

The user layer is folded **as data** after directive extraction — a `:::` fence
inside it is inert prose (agentd's extraction runs only on operator-authored
surfaces; we compose so user text never is one). Grants therefore cannot be
widened from the override (req. 20's "override" is persona/policy, not power).

## 5. The control MCP (`control.*` on the system mcpg)

Declarative mcpg bindings over the apiserver, plus a thin backend plugin where
transaction shaping is needed. Tool families (input schemas normative in the
mcpg config; all calls audited):

| Tool | Effect |
|---|---|
| `control.agents.create` | `{name?, handle?, template?, class?, spec?, triggers?, dryRun?}` → Agent CR (server names it if omitted). `triggers` is the typed sugar over all ten agentd start kinds (RFC 0033 §2.2) — so "make it run every weekday at 7" or "wake it on this webhook / when that spreadsheet changes" are schema-checked arguments, not prose the model must translate into workflow YAML; the supervisor's class instruction tells it to elicit the trigger shape when the user hasn't stated one. Template path validates params against `AgentTemplate.spec.params`. |
| `control.agents.get/list/status` | reads incl. live status (runs, gates, budget, pressure) |
| `control.agents.update` | spec patch (admission ladder applies; restart/reload decided by operator) |
| `control.agents.delete` | policy-gated (final backup per org policy) |
| `control.agents.pause/resume/drain/cancel` | lifecycle verbs (operator → A2A admin) |
| `control.agents.backup/restore/migrate` | RFC 0033 verbs |
| `control.fleets.*` | fleet CRUD + resize (guarded re-partition) |
| `control.templates.list/instantiate` | registry templates |
| `control.registry.*` | scoped registry reads (writes: admin roles only) |
| `control.subagents.create` | **narrowed variant** (see §6) |
| `control.events.tail` / `control.logs.read` | bounded observability reads |

**Authority chain** (the load-bearing part): the caller (supervisor or agent)
reaches the system mcpg with its **agent identity** (AAuth-signed; verified by
mcpg via identity's JWKS) and `_meta agent/acting_for = user:<subject>` (agentd
propagates it from the run's principal). The binding's credential is
`cred://agentctl-identity/mgmt` → identity performs the exchange **(agent token,
acting_for) → user-scoped management-API token**, refused unless a live grant
links that agent to that user (supervisors: implicit for their owner; other
agents: the principal that started the run). The apiserver then authorizes as
the user. **There is no supervisor privilege anywhere** — remove the user's role
and their supervisor is inert.

Autonomous runs (schedules) act as the mapped `identity.autonomous_as` service
principal with a deliberately smaller role (read + pause only, by default).

## 6. Cluster subagents for ordinary agents (req. 6)

Any agent whose class grants `control.subagents.create` may spawn **first-class
child Agents** instead of (or alongside) in-pod subagents:

- Constraints enforced by the tool (not trust): child lands in the same org
  namespace, labeled `agentctl.dev/parent: <agent>`; template allow-list from the
  class; child budgets/TTL deducted-from/bounded-by the parent's class ceilings;
  depth ≤ class limit; children are enumerable (`control.agents.list
  {parent: me}`) and die with `ttl:` or `parent-delete` policy.
- Rationale vs agentd's in-pod subagents: visibility (CRs, metrics, budgets),
  independent scheduling/limits, survivability beyond the parent pod — for heavy
  or long-lived children. In-pod subagents stay the right tool for small bounded
  helpers; guidance table ships in the docs.

## 7. Conversation path

`agentctl chat` (no arg) → gateway resolves the caller's supervisor route →
A2A `SendMessage` with the per-(user, supervisor) principal bearer. Gates the
supervisor raises render in the CLI (`agentctl inbox`), the agentd TUI/UI (which
remain fully usable), and HITL channels (RFC 0031).

### 7.1 Handles and @mention orchestration

Every agent has an **org-unique `@handle`** (`Agent.spec.handle`, RFC 0033;
DNS-1123 label, defaults to the CR name). A user message to the supervisor may
mention one or more — *"ask @triage and @deploy-watcher, then summarize"* — and
the supervisor's **class-provided mention workflow** handles it:

1. **Parse** `@<handle>` tokens from the user turn (prose-level, in the
   supervisor's instruction/workflow — the daemon does not interpret them).
2. **Resolve** via `control.agents.resolve {handles: […]}` — which, being
   OBO-authorized as the user, returns *only agents the user may access*
   (accessPolicies apply automatically); unknown or forbidden handles are
   reported back by name, never guessed.
3. **Fan out**: `batch` over the resolved peers (`{size, parallel, rate}` —
   note `parallel` past `limits.workflow.fan_out` is *refused at load*, not
   clamped) → `a2a.delegate` with templated `peer: "{{ member }}"` — a typed
   command where the target's `a2a` start declares one (`command: ask`,
   schema-checked at the listener), a prose objective with an
   `output_contract` otherwise — `idempotency: true` (pins the A2A messageId
   so a retried ask dedupes at the peer), a per-peer `breaker` for flaky
   peers, and per-peer `timeout` with `on_timeout` as an **expected branch**:
   "@x did not answer within 60s" is news to report, not an error to hide
   (`on_error: continue`, `collect: true`).
4. **Gather and loop**: synthesize the replies with its own context; if the
   synthesis surfaces follow-ups, re-delegate (bounded) before answering. For
   *decoupled* replies (minutes-long, out-of-band), the workflow parks on
   `wait {on: event, match: …}` over a declared reply stream instead — the
   `match` sees `event` + `inputs` together, which is what correlates "the
   reply about *my* ask." Delegate when a person is waiting; stream-wait when
   answers trickle in. (Both shapes verified with agentd upstream; the
   worked precedent is `examples/startup/chief-of-staff.yaml`.)

Bounds (class-configurable): mention cap per message (default 5), per-peer
timeout (default 60s), one re-delegation round by default, the supervisor's
budget windows and `limits.run.steps` (a fan-out spends steps), and
`concurrency: {scope: key}` with `key: "{{ principal }}"` so each user gets a
per-user gather *lock* rather than a queue. **Cross-instance recursion is
bounded by us, not agentd**: verified upstream — `limits.max_message_depth` is
not propagated in the outbound A2A envelope, so depth resets at every instance
boundary. The typed `ask` command therefore carries `args.hops`, incremented
per delegation and refused past a ceiling (default 3) by the receiving
workflow's schema/guard — supervisor → @a → (mentions back) → supervisor dies
there. Fleet handles resolve to the fleet's A2A route (dispatcher), so
"@triage" can name a fleet as naturally as a solo agent.

**Upstream note (PLAN U8 — resolved 2026-08-30):** the interface composer's
`@` autocomplete used to insert bare `@<name>` for skills, which never matched
the daemon's `@skill:` prefix (a pre-existing bug: accepted completions never
preloaded) and collided with agent handles. Fixed upstream on main
(`bc203942`, rides the next agentd tag): the composer inserts the full
`@skill:<name>`, and the docs page that had described the bare form as
working was corrected. Our convention is now **safe by construction**: agentd
claims `@skill:` and nothing else in prose — bare `@<handle>` is ours, and
the daemon deliberately parses no conversational text, so there is no second
namespace for it to grow into. One caveat carried as a renderer rule
(RFC 0032 §4.6): the composer's prefix is hardcoded (the daemon does not send
`skills.reference_prefix` to clients), so agentctl never projects a custom
prefix.

## 8. Failure modes and containment

- **Prompt injection at the supervisor**: bounded by user authority + approval
  gates on destructive verbs + no-egress grant + full audit (`acting_for` on
  every hop). The estate-changing surface is enumerable (the control tools).
- **Runaway creation loops**: class rate quotas on `control.agents.create`
  (mcpg quota bindings, per-identity), org ResourceQuota backstop, budget
  ceilings; `agents.create` is idempotent by `(name)`/idempotency key.
- **Supervisor loss**: stateless-below-the-store; recreate re-attaches to the
  managed store (same `agent.name`), conversation intact.

## 9. Alternatives considered

- Org-level single supervisor; bespoke supervisor service — rejected in
  ADR-0006.
- Exposing the raw management REST API as tools 1:1 — rejected: tool schemas are
  a *curated, model-shaped* surface (small, verb-oriented, schema'd), and the
  narrowing (§6) needs tool-level semantics.

## 10. Open questions

1. Scale-to-zero supervisors (gateway-parked wake) — target v2.1 (PLAN P7).
2. Should `control.registry.*` writes be available to org-admin supervisors, or
   CLI/GitOps-only at first? (Lean: read-only in v2.0.)
3. Whether child-agent budget deduction meters against the parent's windows live
   (requires fabric metering) or only bounds at create-time (v2.0: create-time).
