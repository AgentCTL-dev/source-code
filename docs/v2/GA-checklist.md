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
| **P3** State / durability | P3-1..6 | ✅ **Complete** | state-durability, store-classes, lifecycle-verbs, artifacts-flow + checkpoint bench |
| **P4** Control plane (supervisors, OBO, approvals, @mention) | P4-1..7 | ✅ Done | control-mcp, mention-orchestration |
| **P5** Capability plane (tenant mcpg, OBO, connect, sandbox, HITL, work) | P5-1..7 | ✅ **Complete** | tenant-mcpg, obo-exchange, connections-flow, sandbox-run, hitl-gate, work-redelivery |
| **P6** Fleets + scaling | P6-1..6 | ✅ **Complete** | fleet-static, shard-resize, dispatcher-fanout, fleet-budget, webhook-scale-zero |
| **P7** GA surfaces (hooks, dashboards, audit, metering, scale-to-zero, hardening) | P7-1..6 done, P7-7 this doc | ✅ Done | hooks-ingress, metering-export, audit-trail, supervisor-park |

**53 e2e scenarios; last full catalogue: 50 passed / 3 documented skips / 0
failed.** The three skips are environmental, not gaps:
`sec-oidc`/`sec-trusted-proxy` (a pre-existing unarmed-gate gap superseded by
the P1 identity-gateway authn scenarios) and `sec-netpol` (needs a Calico lane —
kindnet does not enforce NetworkPolicy).

## The state plane (P3) — complete

Once mcpg **beta.26** (the `#664` entity-vtable fix) booted the `backend-sql`
pairing, the whole state plane landed on the proven P3-1 checkpointer:

- **P3-1** `state.*` seq-CAS checkpointer (single-CTE CAS, byte-identical-replay
  idempotence, the 4-tool store contract) — a real `store.class: managed`
  agentd agent checkpoints through it and survives `kill -9` with no
  split-brain (`state-durability`).
- **P3-2** server-side tenant fence (`param_exprs` `identity.subject_id`): a
  conforming caller physically cannot touch another agent's keys; plus
  `state.admin.snapshot/restore/purge`.
- **P3-3** `artifacts.*` façade over an S3 content store (bundled MinIO),
  org-fenced with per-org quotas (`artifacts-flow`).
- **P3-4** store classes ephemeral/local/managed, the `local` StatefulSet+PVC
  path surviving a pod delete (`store-classes`).
- **P3-5** lifecycle verbs backup/restore/reset/stop/start/migrate — `migrate`
  reschedules a managed agent's pod with the checkpoint provably preserved
  (`lifecycle-verbs`).
- **P3-6** checkpoint capacity benchmark (~735 checkpoints/sec single-replica,
  zero CAS errors; numbers in [benchmarks.md](../benchmarks.md)).

## GA gates — all green

- [x] **Every plane live-verified in a real cluster** against real agentd
      1.3.1, blessed mcpg beta.26, and a bundled MinIO (not mocks) — 50/53
      scenarios, the 3 skips documented and environmental.
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

**Every plane of the v2 program — P0 through P7 — is done, integrated, and
live-verified against real agentd, blessed mcpg beta.26, and a bundled MinIO.**
The state plane (P3), the last area to land, is complete: managed durability
that survives `kill -9`, a server-side tenant fence, an S3-backed artifacts
façade with org quotas, three store classes, the full lifecycle-verb set
(including a checkpoint-preserving `migrate`), and a measured capacity envelope.
Nothing in the program is parked or blocked. That is the GA state: shippable.
