# ADR-0001 — agentd is the agent runtime, integrated at the wire only

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0026, ARCHITECTURE §3.6

> **Update (2026-08-30):** the owner of agentd (and mcpg) has granted this
> project a special license to use them — the AGPL-contamination rationale
> below no longer binds us. **The wire-only decision stands on its engineering
> merits** (anti-drift, contract discipline, independent release cadence), but
> linking is now an open option where it demonstrably pays (e.g. reusing
> agentd's store-mapping types in the state service). Any such exception is a
> deliberate, per-crate decision recorded in the PLAN — never a default.

## Context

agentd v1.3.1 (post-August-2026 rewrite) is a complete durable agent runtime: one
durable reactive process, 10 trigger kinds, dialect-3 workflows (72 node kinds),
HITL gates, subagent trees, service catalog + closed egress, pressure-aware
admission, and a designed-for-Kubernetes operational contract (exit-code intents,
probes, drain timings, downward-API identity). It is licensed **AGPL-3.0-only**
(relicensed 2026-08-23; commercial licensing offered). Linking `agentd-core` into
any agentctl component would make that component an AGPL derivative; the existing
contract principle P0 ("depend on the contract, never on the binary's code")
already forbids it for drift reasons.

agentd also *expects* an external control plane: it deleted clustering ("a fleet
partitions upstream"), documents "replica 0 arms the schedule" as deployment
config, publishes `pod_failure_intent()` explicitly "for agentctl," and stamps the
config schema with `x-agentd-contract-version` naming agentctl admission as a
consumer.

## Decision

1. agentd is agentctl's **first-class agent runtime** — supervisors included are
   agentd instances. We run the **unmodified upstream signed image**, pinned by
   digest.
2. Integration is **wire-only**: the process contract (flags/env/exit codes/config
   documents), A2A, MCP, Prometheus/OTLP, and `agentd --validate-config` as the
   config authority. **No agentctl crate ever links agentd code.** The vendored
   contract under `contract/` is regenerated against v1.3.1 surfaces.
3. Gaps in the wire contract become **upstream asks** (tracked in PLAN §Upstream),
   never forks. If we ever must ship a patched agentd image, it is a separate,
   clearly-AGPL artifact and an explicit exception to this ADR.

## Consequences

- License-clean control plane (Apache/BUSL as today), regardless of AGPL §13.
- We inherit agentd's stated seams (exit intents, schemas, A2A admin verbs) and
  their gaps (no `--capabilities` surfaces block today, no A2A TLS hot-rotation,
  `run_until: auto` traps) — the operator works around gaps until upstream fixes.
- Version skew management: `AgentClass` pins the agentd version; the contract
  fixtures capture per-version capability differences.

## Alternatives rejected

- **Embedding `agentd-core`** (best latency/control): AGPL contamination + drift
  coupling; rejected.
- **Own runtime**: re-building agentd's 3 years of runtime semantics is not our
  mission; the control plane is.
