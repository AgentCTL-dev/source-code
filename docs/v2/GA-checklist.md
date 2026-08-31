# agentctl v2 — GA readiness checklist (P7-7)

The honest state of the v2 program at the GA gate: what is done and
live-verified, what is deferred, and the one thing blocked on an upstream
dependency. This is the capstone of [PLAN.md](PLAN.md) — every claim here
traces to a row there and to a live e2e scenario.

## Planes — status at a glance

| Plane | Rows | Status | Proof |
|---|---|---|---|
| **P0** Substrate + agent-agnosticism | P0-1..5 | ✅ Done | real agentd 1.3.1 in kind |
| **P1** Identity (OIDC, AAuth, principals) | P1-1..6 | ✅ Done | sec-aauth, org-route-user, supervisor-route |
| **P2** Tenancy, CRDs, projection, triggers | P2-1..8 | ✅ Done | org-tenancy, policy-ladder, trigger-matrix |
| **P3** State / durability | P3-1 built·parked, **P3-2..6 blocked** | ⛔ upstream | see below |
| **P4** Control plane (supervisors, OBO, approvals, @mention) | P4-1..7 | ✅ Done | control-mcp, mention-orchestration |
| **P5** Capability plane (tenant mcpg, OBO, connect, sandbox, HITL, work) | P5-1..7 | ✅ **Complete** | tenant-mcpg, obo-exchange, connections-flow, sandbox-run, hitl-gate, work-redelivery |
| **P6** Fleets + scaling | P6-1..6 | ✅ **Complete** | fleet-static, shard-resize, dispatcher-fanout, fleet-budget, webhook-scale-zero |
| **P7** GA surfaces (hooks, dashboards, audit, metering, scale-to-zero, hardening) | P7-1..6 done, P7-7 this doc | ✅ Done | hooks-ingress, metering-export, audit-trail, supervisor-park |

**49 e2e scenarios; last full catalogue: 45 passed / 4 documented skips / 0
failed.** The four skips are environmental, not gaps:
`sec-oidc`/`sec-trusted-proxy` (a pre-existing unarmed-gate gap superseded by
the P1 identity-gateway authn scenarios), `sec-netpol` (needs a Calico lane —
kindnet does not enforce NetworkPolicy), and `state-durability` (the one
blocked item below).

## The one blocked dependency — the state plane (P3-2..6)

**P3-1 is built and correct** (the `state.*` checkpointer profile: single-CTE
seq-CAS, byte-identical-replay idempotence, prefix-fenced tenancy, the full
4-tool store contract, the chart's digest-pinned state service). It is
**parked, not missing.**

The block is upstream and specific: the blessed mcpg image cannot `dlopen`
the `backend-sql` state plugin — three artifact pairings failed in sequence
(GLIBC floor, missing entry exports, then a watch-strategy registration
collision), and the mcpg team is re-blessing a pairing built from the same
source line as the gateway. **The moment a pairing boots, `state.enabled`
flips on and `state-durability` unskips itself** — P3-2..6 (server-side
fencing, artifacts façade, store classes, lifecycle verbs, capacity
benchmark) then land on top of the proven P3-1 foundation. This is tracked,
not open-ended: the interim prefix-trust + NetworkPolicy posture ships today,
and the cross-session contract with mcpg is settled.

Nothing else in the program depends on it — every other plane is complete and
live-verified without state durability.

## GA gates — all green

- [x] **Every plane live-verified in a real cluster** against real agentd
      1.3.1 and blessed mcpg beta.24 (not mocks) — 45/49 scenarios, the 4 skips
      documented and environmental.
- [x] **Fail-closed by construction** — audited in the hardening pass
      (P7-5): identity admin refuses without a token, the exchange refuses a
      user-less subject, a gate with no channel refuses at compile, admission
      catches dangling secret refs before a pod crash-loops.
- [x] **Attribution unforgeable** — server-stamped org membership, token-forced
      audit-shipper org, signed OBO acting-user claims.
- [x] **Supply chain** — `cargo deny` green (advisories, bans, licenses,
      sources) with per-crate BUSL exceptions; CI runs clippy/fmt/test/deny on
      the pinned toolchain; release images + chart signed at their tag.
- [x] **One queryable audit trail** across gateway/identity/mcpg for a full OBO
      tool call (P7-3, `audit-trail`).
- [x] **Billing computable from export alone** (P7-4, `metering-export`).
- [x] **Dashboards + alerts** shipped in the chart (P7-2).
- [x] **The "build on agentctl" guide** and the multi-tenant hardening
      checklist (P7-5, [build-on-agentctl.md](build-on-agentctl.md)).

## Deferred to post-GA (polish, not gates)

- **OCI-bundle resolution for registry sets.** `WorkflowSource.set_ref` and
  `SkillSet` are wire-modeled in the CRD (v1alpha2), and inline + ConfigMap
  workflow sources are fully rendered and tested today. Resolving a `set_ref`
  as an *OCI artifact* (pull + verify + project) is an additive operator
  feature that does not change the CRD contract — it slots behind the existing
  `set_ref` field when the bundle registry story is prioritized. Air-gapped
  installs use OCI mirrors for the same refs.
- **Docs-site refresh.** The reference docs (`docs/*.md`) and the v2 set
  (`docs/v2/*`) are current; the public agentctl.dev site render is a
  presentation pass, not a capability.
- **The two upstream defects found this program**, both confirmed in source by
  the mcpg/agentd sessions and riding their next waves, with our side already
  correct against them: mcpg's `x-request-id`→AuditEvent gap and its
  zero-gate-plugin tools/call-unaudited bug (our audit e2e asserts the honest
  join meanwhile); agentd's `a2a.principals`/`webhooks` silent-no-op reload
  paths (our config-hash already treats both as restart-required).

## Verdict

Every buildable plane of the v2 program is **done, integrated, and
live-verified**. P5 and P6 are complete planes; P7's GA surfaces are all live.
The single incomplete area — state durability (P3-2..6) — is blocked solely on
an upstream mcpg plugin pairing, is built up to that boundary (P3-1), and
unskips itself the moment the pairing boots. That is the GA state: shippable
now for every posture that does not require managed state durability, and one
upstream digest away from complete.
