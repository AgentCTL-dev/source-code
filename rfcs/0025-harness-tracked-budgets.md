# agentctl RFC 0025: Harness-tracked budgets — budget as a property of the agent

**Status:** Partially implemented (identity/delegation track; standalone value beyond it;
unblocks 0024 §7.2; extends 0012's cost governance and 0022's per-fleet budget with a
third enforcement point)
**Author:** Andrii Tsok
**Date:** 2026-07-09

> **Implemented (agentctl):** `Agent.spec.limits.lifetimeTokens` →
> `--budget-tokens-lifetime` (operator render); the contract's `budget`
> env-convention group + the `agent_budget_tokens_remaining` gauge +
> `limit=tokens_lifetime` (metrics_schema 1.1). The reference agent (agentd
> `7394dea`) implements the enforcement. **Corrections folded in from what
> landed** (see §3.1/§3.4): exhaustion is `EXIT_BUDGET(7)` only on a bounded
> `once` run; a `reactive`/`loop`/`schedule` daemon **drains to exit 0** (or the
> agent's `--budget-exit-code`), it does not hard-exit 7. And the lifetime value
> is **not** surfaced in the `--capabilities` manifest or `report.usage` — only
> the `agent://config/effective` resource and the metric expose it.
> **Deferred:** the §3.3 admission requiredness keys on `ModelPool.auth.mode:
> aauth` (0024 §7.2), which is itself gated on modelgateway-inbound AAuth — so
> it lands with that, not now.

**Part of:** the agentctl control plane — moves budget from *a property of the gateway
path* to *a property of the agent*: the control plane **declares** the budget on the CR,
the conformant harness **enforces** it against its own metered usage, and the contract's
existing reporting surfaces **prove** it. Paths with no metering gateway (RFC 0024's
direct dials) stay bounded.

> **Contract-first (P0).** This RFC is mostly *recognition*: the Agent Control Contract
> already contains a budget machine (declaration flags, a frozen budget exit code, usage
> reporting with a token-honesty rule, token metrics). What is added is one new
> declaration (per-instance lifetime budget), one env-convention group, and the
> admission/telemetry coupling that makes the machine load-bearing for delegation.

---

## 1. Summary — what already exists (inventory, verified)

| Layer | Mechanism | Status |
|---|---|---|
| **Declare** | `Agent.spec.limits.{maxTokens,maxSteps,maxDepth}` → rendered `--max-tokens/--max-steps/--max-depth` (fleet templates inherit — `template` *is* `AgentSpec`) | **shipped** (operator `render.rs` §limits) |
| **Enforce** | the harness self-meters per run; exhaustion → status `exhausted_tokens`/`exhausted_steps` → **`EXIT_BUDGET(7)`**, intent `policy`, operator-remappable via `--budget-exit-code`; Jobs compile the intent into `podFailurePolicy` | **frozen contract** (exit-codes table 1.0) |
| **Prove** | `report.usage.{tokens_in,tokens_out,steps}` per run under the **token-honesty rule** ("absence is 0, NEVER an estimate"); metrics `agent_tokens_total{type=in\|out}` (the cost subset), limit-hit domain includes `tokens`/`tree_tokens` | **frozen contract** (report + metrics registry 1.0) |
| **Aggregate (adversarial)** | ModelPool budget + per-fleet budget, atomic reserve-then-reconcile at the modelgateway, 429 on exceed, leaked-reservation TTL sweep | **shipped** (0012/0022) |

So "the contract supports budget" is *already true* for a bounded **run**. What is missing
is narrower, and precise:

1. a **per-instance lifetime** budget — a `reactive`/`loop` agent has no cumulative cap
   across reactions (each run is boxed; the instance is not);
2. the **coupling** that makes harness budgets *required* exactly where no adversarial
   meter exists (RFC 0024 §7.2 direct-dial pools);
3. the **usage-truth story without the gateway ledger** — what aggregate observability
   remains, honestly stated.

---

## 2. The three metering trust models (the design's spine)

| Model | Enforcer | Trust class | Granularity | Works where |
|---|---|---|---|---|
| **Gateway-metered** | modelgateway ledger | **adversarial** — the agent cannot lie or bypass (in-path, atomic) | aggregate: pool, fleet, namespace | only through the gateway |
| **Harness-metered** (this RFC) | the conformant agent itself | **cooperative** — same trust class as the rest of the contract (exit codes, drain semantics, report honesty). A hostile image ignores it. | per run; per instance (new) | **anywhere**, including direct dials |
| **Provider-metered** | the remote endpoint | contractual/billing | per identity (needs RFC 0023/0024: per-agent identity upstream) | AAuth-native providers (future) |

Positioning under the locked *hostile multi-tenancy* stance: the gateway ledger remains
the **only adversarial backstop** and stays mandatory wherever it is in-path. Harness
budgets do not weaken that — they *extend bounded-ness* to paths the ledger cannot see,
against the same conformance trust the platform already leans on everywhere else. The two
compose: through the gateway, whichever bound trips first wins (a 429 from the pool ledger
or a self-stop from the harness box). Admission never treats a harness budget as a
substitute for a pool budget on gateway-fronted pools.

---

## 3. Additions

### 3.1 Per-instance lifetime budget (contract ask — agentd Ask 4)

New declaration bounding the **instance across all runs/reactions**:

```
flag: --budget-tokens-lifetime <N>      env: AGENT_BUDGET_TOKENS
```

Semantics (to be fixed with the reference implementation; the contract needs them
*deterministic and observable*, not any particular choice):

- metered against the same counters that feed `agent_tokens_total`;
- on exhaustion: a bounded-run mode finishes as today (`EXIT_BUDGET(7)`); a `reactive`
  instance **stops accepting new reactions**, emits a terminal-ish event + metric, and
  either drains cleanly (preferred — parallels the clean-SIGTERM-drain = exit 0 rule) or
  exits 7 per an operator knob;
- threshold-crossing is observable *before* exhaustion (event + gauge, §3.4) so scaling
  and alerting can react.

CRD shape (additive to the existing group — no new top-level concept):

```yaml
spec:
  limits:
    maxTokens: 200000          # per-run box (existing, unchanged)
    lifetimeTokens: 2000000    # NEW: per-instance cumulative cap → --budget-tokens-lifetime
```

Rendered exactly like its siblings. Fleet note: `lifetimeTokens` is **per member** (the
template renders identical pods); the *fleet-aggregate* bound remains `AgentFleet.spec.budget`
at the gateway — the two answer different questions and are documented side by side.

### 3.2 Env-convention group (additive, contract stays 1.0)

`env-convention.json` gains a `budget` group documenting `AGENT_BUDGET_TOKENS`
(per-instance lifetime; restart-only; never a secret) and cross-referencing the per-run
flags (`--max-tokens` et al.) as the run-scoped box. All-optional rule preserved; absence
= unbounded (today's behavior).

### 3.3 Admission coupling (where cooperative budget becomes *required*)

- `ModelPool.auth.mode: aauth` (RFC 0024 §7.2 — no gateway ledger on the path): every
  Agent bound to such a pool **must declare** a budget appropriate to its mode —
  `limits.maxTokens` for bounded-run modes, `limits.lifetimeTokens` for
  `reactive`/`loop`. Webhook rung, same cross-object class as the ModelPool-existence
  check. Message names the missing field.
- Gateway-fronted pools: no new requirement (the adversarial ledger is already there);
  harness boxes remain recommended (fail-fast locality: the agent stops itself instead of
  discovering the pool 429).

### 3.4 Usage truth without the ledger (honest scope)

What aggregate observability remains on direct-dial paths:

- **Per-run:** `report.usage` (unchanged) — the durable roll-up, honesty rule intact
  (provider-reported usage only; a provider that hides usage yields 0s, never estimates —
  the *harness* budget then meters what it can see, which is the same number the report
  carries; the bound degrades visibly rather than silently).
- **Live:** `agent_tokens_total` scraped per pod → telemetry rollups by
  namespace/fleet/agent. Proposed additive registry entry: gauge
  `agent_budget_tokens_remaining` (absent when unbounded) — the alerting/scaling hook for
  §3.1's threshold events.
- **Not available, stated plainly:** `ModelPool.status.usedTokens` remains
  **ledger-scope** — a pool consumed via direct dial shows no gateway-side usage, and the
  contract metrics carry no pool label (the agent does not know pool topology; teaching it
  would leak control-plane structure into the data plane for a reporting nicety). The
  per-namespace metric rollup is the aggregate view on those paths.

---

## 4. Interplay & precedence (documented semantics)

1. Through the gateway: `min(pool ledger, fleet ledger, harness boxes)` — first bound
   trips. Operators sizing harness boxes near the pool share avoid surprise 429s; this is
   guidance, not mechanism.
2. Direct dial: harness boxes are the only bound (hence §3.3's requiredness) + provider
   metering where it exists.
3. Remap discipline: `EXIT_BUDGET(7)`'s `policy` intent and `--budget-exit-code` remain
   the single knob deciding whether budget exhaustion retries — this RFC adds no second
   disposition mechanism.

---

## 5. Alternatives considered

- **A metering sidecar per pod** (TLS-terminating proxy): adversarial-ish, but per-pod
  cost, cert MITM machinery, and a second data-plane component to version — rejected;
  it rebuilds the gateway worse, per pod.
- **eBPF byte accounting**: sees ciphertext sizes, not tokens — rejected.
- **Provider-side only**: not portable across providers, absent today — kept as the
  eventual third leg (§2), not the mechanism.
- **Teaching the gateway ledger about direct dials via agent self-reports**: turns an
  adversarial ledger into a cooperative one wearing the adversarial ledger's clothes —
  rejected for the dishonesty; the trust boundary stays visible instead.

---

## 6. Phasing & verification

1. **Contract PR** (additive): env-convention `budget` group; metrics registry gauge;
   conformance assertions — lifetime budget honored across ≥2 reactions, threshold event
   emitted, report/metric agreement on the metered totals, honesty rule preserved when the
   provider omits usage.
2. **agentd Ask 4** lands the enforcement (reference implementation).
3. **CRD + render + admission** (`limits.lifetimeTokens`, §3.3 rungs) — independent of
   AAuth; useful the day it merges for any long-lived reactive agent.
4. **Telemetry rollups + alert recipe** on `agent_budget_tokens_remaining`.
5. RFC 0024 §7.2 flips on only after (1)–(3) are in place.
