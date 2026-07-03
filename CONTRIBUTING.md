# Contributing to agentctl

Thanks for contributing! A few things keep this project healthy and its
licensing model viable.

## Licensing & the CLA (required)

agentctl is **dual-licensed by component** (see [`LICENSE`](LICENSE)): the
contract + SDK + tooling are **Apache-2.0** (open source); the runnable control
plane is **BUSL-1.1** (source-available, converts to Apache-2.0 on the Change
Date). To keep that model legal, the project must hold sufficient rights in every
contribution, so **all contributors must sign the [CLA](CLA.md)**.

You don't sign anything up front: when you open your first pull request, the **CLA
bot** comments with a one-line phrase to reply with. Once you've replied, the bot
records your signature and the check goes green; future PRs are automatic.

Please also **sign off your commits** (DCO) as a provenance record:

```sh
git commit -s -m "your message"   # adds a Signed-off-by: line
```

## Source headers

Every source file carries an SPDX header on line 1. New files **must** include it,
matching the crate's license (see the map in [`LICENSE`](LICENSE)):

```rust
// SPDX-License-Identifier: BUSL-1.1     // control-plane crates
// SPDX-License-Identifier: Apache-2.0   // contract / SDK / CLI / tooling
```

## P0 — depend on the contract, never on a specific agent

agentctl drives **any** agent conforming to the Agent Control Contract
(`contract/`, see `contract/SPEC.md`). Do not add a dependency on a specific agent
implementation (e.g. agentd) or branch on an agent's internals — code against the
contract. New wire shapes belong in the contract + the generated client, not
inline in a component.

## Dev workflow

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings   # must be clean
cargo fmt --all                                          # must be clean
cargo run -p agentctl-crdgen                             # regenerate deploy/crds (no drift)
```

CI gates on all of the above (fmt, clippy `-D`, tests, CRD drift) — run them
locally first. End-to-end work is verified on a `kind` cluster; see
[`deploy/README.md`](deploy/README.md).

## Where things live

### Crates (`crates/`)

The workspace is Rust-only. Every control-plane component is a crate; the
contract client and CRD types are shared libraries.

| Crate | Role |
| --- | --- |
| `agent-api` | CRD types (`Agent`, `AgentFleet`, `ModelPool`, `MCPServerSet`) as kube-rs `CustomResource`s. |
| `agent-contract-client` | Typed client for the Agent Control Contract (capabilities manifest, surfaces discovery, version negotiation). |
| `agentctl-operator` | Reconciles `Agent`/`AgentFleet` into workloads (the pure rendering core plus the kube runtime controller). |
| `agentctl-apiserver` | Aggregated APIServer serving the management verbs (drain, lame-duck, cancel, pause, resume). |
| `agentctl-admission` | Validating + mutating webhooks (image allow-list, lethal-trifecta gate, secure defaults). |
| `agentctl-gateway` | A2A gateway — the public agent-to-agent surface + Agent Card projection. |
| `agentctl-modelgateway` | Intelligence broker — injects the ModelPool credential, meters tokens, enforces budgets. |
| `agentctl-mcpgateway` | Tools broker — scopes calls to the bound MCPServerSet and injects the server credential off-pod. |
| `agentctl-coordination` | Reference work-distribution MCP server (`work.*`) — the exactly-one-owner claim backbone and backlog signal. |
| `agentctl-scaler` | KEDA external gRPC scaler that reads the coordination backlog so claim fleets scale from zero. |
| `agentctl-cli` | The `agentctl` CLI / `kubectl-agent` plugin (`get`/`describe`, management verbs). |
| `agentctl-crdgen` | Emits the CRDs as apply-able YAML under `deploy/crds/`. |
| `agentctl-telemetry` | Shared tracing init (fmt layer + optional OTLP export when `OTEL_EXPORTER_OTLP_ENDPOINT` is set). |
| `mock-agent` | A minimal conformant-agent stand-in used by dev/e2e/conformance fixtures. |
| `agentctl-e2e` | End-to-end + benchmark harness (excluded from the workspace; needs a cluster). |

### Other top-level directories

- `contract/` — the Agent Control Contract: `README.md`, `SPEC.md`, JSON schemas, and fixtures.
- `charts/agentctl/` — the production Helm chart.
- `deploy/` — raw per-component manifests, generated CRDs, and the local kind walkthrough.
- `bundle/` — the alpha OLM / OperatorHub bundle.
- `docs/` — [`architecture.md`](docs/architecture.md), [`operations.md`](docs/operations.md), [`security.md`](docs/security.md), [`benchmarks.md`](docs/benchmarks.md).

By submitting a contribution you agree to the [CLA](CLA.md).
