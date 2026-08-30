# ADR-0008 — agents never execute code; computation is a sandboxed MCP service

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0031 §sandbox, ARCHITECTURE §3.5, §11

## Context

Requirement 11: agents in agentctl sandboxes must not run commands or code;
such capabilities are separate services exposed over MCP. This aligns exactly
with agentd's posture — `exec` is compiled out of release binaries ("opt-in twice
over"), images are `FROM scratch` with no shell — and with mcpg's default-deny
stance (`allow_stdio: false`, command backend flagged as unisolated).

## Decision

1. **No `exec` anywhere in the fleet**: agentd release images only; admission
   refuses any configuration that presumes local execution; no shell in any image
   we ship.
2. Computation is the **sandbox service**: `sandbox.*` MCP tools on the tenant
   gateway, implemented as our Apache-licensed mcpg backend plugin that executes
   submissions in disposable Kubernetes Jobs or a warm pool — per-run resource
   caps (CPU/mem/wall/output), no network by default (opt-in egress lists),
   optional gVisor/Kata via RuntimeClass, artifacts in/out via the content store.
3. Sandbox access is a **registry-governed capability** like any other: tag floors
   (`sensitive`? `egress` if network enabled), per-org enablement, quotas, audit,
   optional approval gates on first use.
4. Other "dangerous" base capabilities follow the same pattern (headless browser,
   package installation, git write access): a governed service, never a local
   power.

## Consequences

- The lethal-trifecta calculus stays honest: an agent's blast radius is its
  catalog, and the catalog is reviewable.
- Code-running agents (coding assistants, data analysis) work through tools with
  latency costs; the warm pool bounds them. Streaming output rides mcpg's
  streaming backend API.
- We own a real security-critical service; it gets its own threat model and the
  strictest defaults (deny network, tight quotas, short TTLs).

## Alternatives rejected

- Per-agent sidecar executors: N privileged-ish sidecars, no central governance,
  breaks the scratch/no-shell pod story.
- Building on agentd's `exec` feature (source build): abandons the signed upstream
  image and re-opens local execution on the reasoning host — the exact coupling
  the trifecta design exists to prevent.
