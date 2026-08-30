# ADR-0006 — one supervisor per user, implemented as an agentd instance

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0027, ARCHITECTURE §3.7, §5

## Context

Requirements 4, 7, 14, 20: an agentic control experience — each user talks to a
personal supervisor over A2A which can spin up, modify, and steer agents in the
user's space; instructions provisioned centrally with user overrides. agentd
provides everything a supervisor needs: durable conversations, workflows, MCP
tools, gates; and its instruction is a directive document (agentd's RFC 0034
format) we can layer.

## Decision

1. **The supervisor is a stock agentd instance** — no bespoke supervisor binary.
   Its special-ness is entirely configuration: a `Supervisor` CR rendered by the
   operator into an `Agent` with `class: supervisor`.
2. **Scope: one per user** (not per org): personal context, personal authority,
   personal budget. Org-level automation belongs to ordinary (service) agents.
3. **Authority = the user's, never more.** Supervisor tool calls to `control.*`
   are OBO-exchanged (its agent identity + acting_for its user → the user's
   management-API token); the apiserver authorizes as the user. A supervisor of a
   viewer can view; of an admin, administer. No supervisor-specific privilege
   exists anywhere.
4. **Instruction layering:** system default (registry) → org `AgentClass` override
   → user override — the user layer is prose-only (persona, preferences, standing
   guidance); tool grants and machinery come only from the class. Directives in
   user-supplied text are refused by rendering order (operator composes; user text
   is folded as data).
5. Supervisors are the default chat target and the **subagent factory** for agents
   granted narrowed `control.*` (create-in-own-space), making heavy children
   first-class `Agent`s (req. 6).

## Consequences

- The control plane becomes conversational with zero new runtime code; every
  supervisor improvement is a registry/config change.
- Supervisor cost is a per-user standing pod — mitigations: tiny defaults
  (idle agentd ≈ 5.5 MiB), scale-to-zero for dormant users (gateway parks the
  route, wakes on message), org policy to disable supervisors for some groups.
- Blast radius: a prompt-injected supervisor can do at most what its user can, and
  every action is attributed + audited + policy-gated (approval policies can
  require gates for destructive verbs).

## Alternatives rejected

- One shared org supervisor: cross-user context bleed, unattributable actions,
  addressed-gate ambiguity; agentd is deliberately "not multi-tenancy."
- A hand-written supervisor service: re-implements conversation/durability/HITL
  that agentd already has; violates "agentd is the runtime."
