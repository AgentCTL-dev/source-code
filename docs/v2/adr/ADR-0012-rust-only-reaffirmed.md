# ADR-0012 — Rust only, reaffirmed for every v2 component

**Status:** Accepted · **Date:** 2026-08-30 · **Reaffirms:** RFC 0001; the standing repo constraint

## Context

The v2 transformation adds components (identity, gateway evolution, mcpg plugins,
sandbox runner, CLI growth) that in many ecosystems default to Go (kube-rs vs
client-go territory) or TypeScript. The repo has a standing hard constraint:
agentctl is Rust-only — including the aggregated-apiserver decision (axum +
rustls, no Go). Both upstreams are Rust: agentd entirely; mcpg's gateway/plugin
ABI is Rust (cdylib / static entities).

## Decision

1. **Every agentctl v2 component is Rust**: operator (kube-rs), apiserver +
   gateway + identity (axum + rustls + ring), CLI/kubectl plugin, mcpg plugins
   (`declare_plugin!` cdylibs or static entities), sandbox runner, conformance
   tooling. No Go, no exceptions; no new TypeScript beyond the existing web
   assets.
2. mcpg plugins are built in this workspace against pinned mcpg tags (pre-1.0 ABI
   ⇒ lockstep pinning in CI; ADR-0003).
3. The existing toolchain discipline carries over: CI-matching `cargo +stable
   clippy/fmt/test`, `cargo deny check` (BUSL exception table maintained for new
   crates), workspace lints.

## Consequences

- One language across control plane and both upstreams: shared review standards,
  shared security posture (rustls/ring everywhere), plugin reuse.
- Some ecosystem conveniences (Go-based operators' libraries, controller-runtime
  patterns) are re-derived on kube-rs — accepted cost, already paid in v1.

## Alternatives rejected

- Go for the operator or identity: violates the standing constraint; splits the
  build/review world for no capability we lack in Rust.
