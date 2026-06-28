# agentctl

**The Kubernetes control plane for fleets of conformant agents.** agentctl
provisions, supplies intelligence to, scales, observes, and manages agents on
Kubernetes — via a CLI, a `kubectl agent[s]` plugin, an operator, an on-node
bridge (node-agent), an aggregated APIServer, and an A2A gateway.

> **Principle P0 — depend on the *contract*, never on a specific agent.** The
> data plane is *any* agent that conforms to the **Agent Control Contract**
> (`contract/`): the capabilities manifest, the management MCP profile, the
> frozen metrics + exit-code contract, the config schema, A2A over the substrate,
> and the downward-API env convention. `agentd` is the reference implementation,
> not a dependency. agentctl is **Rust-only**.

## Architecture

```
 user / kubectl agent ─▶ kube-apiserver ─▶ aggregator ─(front-proxy cert+identity)─▶
   agentctl-apiserver (verify · SubjectAccessReview) ─▶ resolve Agent→pod→node-agent ─▶
     node-agent (DaemonSet) ─(unix socket: management profile)─▶  agent

 Agent / AgentFleet (CRD) ─▶ agentctl-operator ─▶ Job / Deployment / StatefulSet
   · status + finalizer GC · KEDA-safe replicas
 node-agent ─(scrape over socket)─▶ /metrics ─▶ Prometheus   (networkless agents stay observable)
```

The load-bearing design choices (see `docs/design/agentctl-architecture-brainstorm.md`):
a **tiered substrate** (stock-unix hostPath socket → DaemonSet as the portable
default; Kata-hybrid vsock as the hardened multi-tenant tier), a **two-tier
node-agent**, and **contract-as-schema** anti-drift (codegen + behavioral
conformance, not a shared crate).

## Workspace (10 crates)

| crate | role |
|---|---|
| `agent-contract-client` | typed client for the contract (manifest sum-types, version negotiation) |
| `agent-api` | `Agent` / `AgentFleet` / `ModelPool` CRDs (kube-rs) |
| `agentctl-operator` | render core + reconcile controllers (Agent & AgentFleet) |
| `agentctl-crdgen` | emits `deploy/crds/*.yaml` |
| `agentctl-node-agent` | on-node bridge: socket discovery, management client, HTTP API, metrics scrape-proxy |
| `agentctl-apiserver` | aggregated APIServer: front-proxy auth + SAR + verb forwarding |
| `agentctl-gateway` | A2A gateway: public A2A HTTP/JSON-RPC + Agent Card projection, bridging to the agent over the node-agent |
| `agentctl-modelgateway` | intelligence proxy: `ModelPool`-driven credential injection, token metering, budget enforcement |
| `agentctl-cli` | `agentctl get` / `describe` |
| `mock-agent` | a conformant-agent stand-in (management profile) for dev/e2e/conformance |

## Quickstart (kind)

```console
kind create cluster --name agentctl
# build + load images, install CRDs + operator + node-agent + apiserver
cargo run -p agentctl-crdgen && kubectl apply -f deploy/crds/
# … see deploy/README.md for the full image-build + apply walkthrough …
kubectl apply -f deploy/examples/agent-once.yaml
kubectl get agents
```

Full walkthrough: **[deploy/README.md](deploy/README.md)**.

## Status

The control plane is **implemented and verified end-to-end on kind** — operator
(CRD→workload), scaling (fleets, KEDA-safe), the node-agent keystone, the
aggregated-APIServer management path (`drain`/`lame-duck` round-trip with RBAC),
and the observability scrape-proxy all run. See **[docs/STATUS.md](docs/STATUS.md)**
for the per-plane status and roadmap.

## Design & specs

- **[docs/design/agentctl-architecture-brainstorm.md](docs/design/agentctl-architecture-brainstorm.md)** — the binding pre-RFC record (decisions, P0, contract asks).
- **[rfcs/](rfcs/)** — the agentctl RFC track (0001–0018).
- **[contract/](contract/)** — the Agent Control Contract v1 (JSON Schemas + golden fixtures).

## License

Apache-2.0.
