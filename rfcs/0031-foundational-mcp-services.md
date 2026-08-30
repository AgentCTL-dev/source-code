# RFC 0031 — foundational MCP services on mcpg

- **Status:** Proposed
- **Date:** 2026-08-30
- **Decisions:** ADR-0003, ADR-0008 · **Depends on:** RFC 0026 §3 (store
  profile), 0028 (identity), 0030 (tenant tier)

## 1. Motivation

Durable agents need platform services (req. 10): state that makes
`store.class: managed` real, artifacts, the work fabric, sandboxed execution
(req. 11), HITL channels (req. 32), and the control surface (RFC 0027). All are
MCP (req. 18), all on mcpg, split across the **system gateway** (cluster infra;
state/artifacts/control/work) and the **tenant gateway** (per org;
sandbox/HITL) per RFC 0030 §2.

## 2. `state.*` — the checkpointer service (store.class: managed)

**Contract (normative, from agentd's MCP store adapter):** tools `state.put`,
`state.get`, `state.list`, `state.delete` — agentd's default mapping needs zero
config. Semantics agentd relies on:

- Keys are `<prefix>/<instance>/<kind>/<id>`; **`put` is a seq CAS** (expected
  seq in, conflict out — agentd treats a conflict on an owned key as fatal
  split-brain, which is the fence we must preserve, not paper over).
- Every call carries `_meta agent/idempotency_key = "<key>#<seq>"` and
  `agent/instance` — replays of the same put must be idempotent.
- `list` by prefix with cursors; `get` returns value + seq; timeouts bounded by
  the agent's store timeout (this service's p99 budget: **≤50 ms in-cluster** —
  agentd's reactor does store I/O synchronously; slow state = slow agent).

**Implementation:** Postgres (dedicated database; per-agent rows keyed
`(org, agent_instance, key)`), via mcpg SQL-backend bindings with server-side
CAS (`UPDATE … WHERE seq = :expected` → `rows_affected`), and — where binding
expressiveness runs out — a small Apache-2.0 backend plugin over the same pool.
**Tenant fencing is server-side**: the prefix/instance is derived from the
verified caller identity (mcpg's `param_exprs: context.principal_id` pattern),
never from arguments — an agent cannot name another agent's keys.

Also here: **snapshot/restore admin tools** (`state.admin.snapshot/restore/
export`) used by lifecycle verbs (RFC 0033) — operator/apiserver-only grants.

## 3. `artifacts.*` — blobs

mcpg content store (content-addressed, TTL, tenant-isolated, signed URLs) with
an S3-compatible provider; a thin binding/plugin façade exposes
`artifacts.put/get/list/delete/link` as tools and `mcpg-resource://` URIs via
`resources/read`. Used by: agents (large outputs), sandbox I/O, backups,
HITL attachments. Size caps + org quotas from the registry.

## 4. `work.*` — the fabric (fleet strategy C)

`agentctl-coordination`'s queue/DLQ machinery re-surfaced as tools:
`work.push`, `work.lease {queue, ttl_ms, max}`, `work.ack`, `work.nack
{requeue|dlq}`, `work.peek/stats`. TTL leases; at-least-once; item-id dedup;
per-queue org scoping; DLQ with replay. Agents run agentd's own
`lease → work → ack` workflow shapes against it (RFC 0034).

## 5. `sandbox.*` — execution as a service (req. 11)

Our Apache-2.0 mcpg **backend plugin** (the shape mcpg's own draft sandbox RFC
anticipates) with a Kubernetes provider:

- `sandbox.run {language|image, code|argv, files_in: [artifact refs], timeout,
  resources}` → runs in a **disposable Job** (or warm-pool pod) in a dedicated
  sandbox namespace: no ServiceAccount token, no network (default), gVisor/Kata
  via RuntimeClass (values-selectable), CPU/mem/wall/output caps, stdout/stderr
  + `files_out` captured to artifacts.
- `sandbox.session.*` (create/exec/destroy) for warm REPL-style sessions with
  TTL — same isolation, faster loop for coding agents.
- Governance: registry-gated per org/class; tags declared honestly
  (`sensitive`; `egress` only when network is enabled); quotas per agent + per
  user; every run audited with input hashes.

## 6. `hitl.*` and the approval fabric (req. 32)

Two halves, one inbox:

1. **mcpg-side**: approval gates on governed tools (`PendingApproval`) + notifier
   plugins (Slack/email/Teams/PagerDuty) with HMAC-signed callbacks — "may the
   agent do X" moments at the capability plane.
2. **agentd-side bridge**: a small system service (Rust) that consumes each
   agent's gate signals (`human.asked/answered/timeout` events via the gateway's
   `SubscribeToEvents`, plus open `input-required` tasks) and routes them to the
   org's registered channels; answers flow back as ordinary A2A gate answers
   under the answering user's principal — so **addressed gates keep their
   meaning** (only the named human can satisfy them; Slack identity maps to IdP
   identity via identity service).
   `hitl.ask` is also exposed as a tool for workflows that want an out-of-band
   question without an agentd gate.

Channel registry (which Slack workspace/webhook/email per org/group/user) lives
in the registry (RFC 0032) and renders into both halves.

## 7. Cross-cutting

- **AuthN**: all callers are AAuth-verified agents (or mTLS'd platform
  components); per-tool grants derive from class catalogs.
- **Multi-tenancy**: org isolation at data layer (RLS or keyed schemas for
  state/work; bucket prefixes for artifacts) *and* at policy layer (CEL).
- **Ops**: each service = mcpg config + our plugins in one deployment per tier;
  Prometheus + OTLP on; capacity model documented (state QPS ∝ fleet checkpoint
  rate — the dominant load; benchmarked in PLAN P3).
- **Versioning**: plugins pinned to the mcpg gateway tag (pre-1.0 ABI); the
  store profile is contract-tested against real agentd (SIGKILL/resume matrix
  from its conformance ideas re-run against `store.kind: mcp`).

## 8. Open questions

1. State backend beyond PG (FoundationDB/CRDB) for very large fleets —
   deferred; the profile is backend-agnostic by design.
2. Whether `work.*` should adopt native queue backends (NATS JetStream via
   mcpg cluster role) behind the same tools — v2.1 candidate.
3. Sandbox warm-pool sizing/eviction policy — start static per org, learn.
