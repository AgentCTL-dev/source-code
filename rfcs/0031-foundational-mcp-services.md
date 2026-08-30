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

**Implementation (upstream-steered, 2026-08-30):** pure **SQL bindings on the
backend-sql plugin**, exactly the `agent-shared-memory` template's shape —
Postgres (dedicated database; per-agent rows keyed `(org, agent_instance,
key)`), server-side CAS (`UPDATE … WHERE seq = :expected` → `rows_affected` =
the optimistic version-column pattern). The mcpg team's guidance: graduate to
a `store`-class plugin **only** for conflict semantics SQL cannot express
(server-side merge, multi-key atomicity beyond a transaction) — seq-CAS needs
neither; and the p99 budget is loose against their measurements (the gateway
proxies ~24–27k QPS; the historical hot-path ceiling was the audit sink's
fsync, since group-committed). **Tenant fencing is server-side and native to
the bindings**: the prefix/instance derives from `${identity.subject_id}`,
resolved host-side from the VERIFIED caller identity — not spoofable by
arguments — so an agent cannot name another agent's keys.

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

Our mcpg **backend plugin** with a Kubernetes provider — the shape mcpg's own
sandbox draft anticipates (**plugin-protocol RFC 0022**,
`docs/plugin-protocol/rfcs/0022-backend-remote-sandbox.md`,
`dev.mcpg.backend.sandbox`; cite the plugin-protocol series — mcpg's
control-plane series has an unrelated RFC 0022). Upstream status
(2026-08-30): Draft, unimplemented, header pre-dates their ABI freeze — so
the **baseline our chart states is a self-host cell running our static
build** (per §7's governance corollary), with config-against-a-blessed-0022
adopted as an upgrade if upstream ships it in time (prioritization flagged to
the owner). The draft's provider abstraction (register_profile-owned spec,
agnostic backend kind, sync + streaming) is stable enough to shape our
interface against now:

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
  from its conformance ideas re-run against `store.kind: mcp`). Upstream note
  (2026-08-30, from the mcpg team): mcpg has shipped release governance —
  **platform-blessed gateway versions** — constraining which gateway builds
  our surfaces may assume; the pin must come from that blessed set, not an
  arbitrary tag. Corollary (upstream-confirmed): a static `declare_plugin!`
  build compiles INTO the gateway binary, so the blessed gateway version IS
  the plugin version (no skew possible, capabilities enumerated in the
  manifest) — while a consumer static-linking their OWN gateway build is a
  **self-host cell posture entirely outside the blessed set**. Our
  foundational surfaces therefore prefer config-against-blessed-builds; any
  custom static plugin (e.g. the sandbox backend) makes that deployment a
  self-host cell by definition, and the chart must say so. Also: the `agent-shared-memory` template's CAS is
  **optimistic concurrency by version column** (publish fails on a stale
  version and the caller re-reads), not a row lock — which happens to be
  exactly the seq-CAS shape agentd's checkpointer profile wants (§2): map
  `seq` onto the version column and a CAS miss onto the conflict result.

## 8. Open questions

1. State backend beyond PG (FoundationDB/CRDB) for very large fleets —
   deferred; the profile is backend-agnostic by design.
2. Whether `work.*` should adopt native queue backends (NATS JetStream via
   mcpg cluster role) behind the same tools — v2.1 candidate.
3. Sandbox warm-pool sizing/eviction policy — start static per org, learn.
