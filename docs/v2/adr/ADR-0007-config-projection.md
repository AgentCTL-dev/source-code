# ADR-0007 — agent config is a projected directory of layered files, never flags

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0032, 0033; ARCHITECTURE §6

## Context

agentd v1.3.x made "a project is a directory" its config model: a
`config_version: "1"` document, multi-file RFC 7396 layering (`-c services.yaml -c
agent.yml`), conventional folders (`workflows/`, `skills/`, `subagents/`,
`context/`), `vars:` + `{{config.*}}` for overlay indirection, `{{secret:}}`
references, published JSON schemas, and `--validate-config` as the authority. The
old flag-rendering agentctl operator (`--mode`, `--shard`, …) is broken against
it — those flags exit 2.

## Decision

1. The operator renders every agent as a **read-only projected directory**:
   `services.yaml` (org catalog layer) + `agentd.yml` (instance layer) +
   conventional folders, invoked as `agentd -c …/services.yaml -c …/agentd.yml`.
   Flags/env are reserved for process-intent only (`--metrics-addr`, downward-API
   env).
2. **Layer semantics are load-bearing**: lists replace ⇒ per-replica/partition
   differences ride `vars:` + `{{config.*}}` tokens baked into the base layers;
   secrets are always references; `lifecycle.run_until` is always explicit.
3. **Validation is the binary's**: admission runs `agentd --validate-config`
   (sandboxed, offline) against the rendered directory and records the effective
   surfaces it prints; the vendored JSON schemas are advisory (editor UX), not the
   gate.
4. **Config changes roll safely**: restart-only diffs (store, listeners, security,
   name) → rolling restart; reloadable diffs → SIGHUP path once the upstream
   ConfigMap-watch behavior is verified (until then, restart for everything —
   durable store + drain + definition pins make restarts loss-free).

## Consequences

- agentctl's provisioning surface (registry → folders) matches agentd's mental
  model exactly; a rendered agent is inspectable and diffable as files.
- The renderer must respect agentd's sharp edges (folder adoption only-when-absent,
  suppression of the sugar `main` workflow, `dir:` entries need `name:` in
  documents) — encoded as renderer tests against `--validate-config`.
- Hot-reload latency for config pushes is deferred to the upstream ask; the
  interim restart path is our safety-first default anyway.

## Alternatives rejected

- Flag/env rendering (v1 style): removed upstream; also unreviewable.
- One merged mega-YAML per agent: loses the catalog/instance separation that
  makes org-level governance a single shared layer, and defeats folder-based
  registry composition.
- An instruction-document-only CRD (everything via `:::` directives): attractive
  minimalism, kept as an *input option* on `Agent.spec.instruction`, but the
  projected directory remains the canonical rendering target.
