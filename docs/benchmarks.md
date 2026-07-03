# agentctl — scale & resource benchmarks

End-to-end measurements of the agentctl control plane driving the reference agent
`agentd` as the data plane, produced by the benchmark harness (`e2e/` +
`crates/agentctl-e2e`). Every number here was **measured**, not modeled.

> **Host-bound caveat — read first.** These runs are on a **single-node kind
> cluster** (one Docker "node" sharing the host's CPU/RAM). Absolute capacity (max
> agents, ops/sec) is therefore bound by *this host*, **not** by agentctl's design.
> The durable, portable results are the **per-agent overhead**, the **control-plane
> scaling trends**, and the **methodology** — re-run the identical suite on a real
> multi-node cluster for true capacity numbers:
> `make -C e2e e2e bench report KUBECONFIG=<real-cluster> SKIP_BRINGUP=1`.

## Host profile

| | |
|---|---|
| CPU | AMD EPYC 7502P (16 vCPU visible to the node) |
| Memory | 64,314 MiB |
| Kubernetes | v1.31.0 (kind, **1 node**) |
| Kernel | Linux 6.1.0 x86_64 |
| Data-plane agent | `agentd` (static binary, ~1.3 MB; 100m CPU / 128Mi requests) |
| Coordination store | in-memory (Postgres comparison below) |

## Per-agent overhead (the headline)

Marginal cost of one additional idle `agentd` agent, at steady state — an agent is
an ordinary pod that serves its mTLS `/mcp` surface and dials the gateways keyless:

| Component | CPU (millicores) | Memory (MiB) |
|---|---|---|
| **`agentd` pod** (idle, reactive) | **~1.3** | **< 1** |
| control-plane marginal (per agent) | ~0 | ~0 |

`agentd` is about the lightest possible conformant agent, so this is effectively the
**floor**: an idle agent costs ~1.3 millicores and a sub-MiB working set, and the
control plane's marginal cost per agent is in the noise. Density is bound by the
node, not the agent.

## Control-plane footprint

Spot measurements (`kubectl top`) of the full control plane plus a reactive `agentd`
agent bound to an MCP tool server, all `Ready` (Rust components, distroless nonroot):

| Component | CPU (m) | Mem (MiB) | | Component | CPU (m) | Mem (MiB) |
|---|---|---|---|---|---|---|
| operator | 1 | 3 | | modelgateway | 1 | 2 |
| apiserver | 1 | 4 | | mcpgateway | 1 | 1 |
| gateway | 1 | 2 | | admission | 1 | 4 |
| coordination | 3 | 7 | | scaler | 1 | 2 |
| postgres | 8 | 54 | | | | |

The whole control plane idles at **~18 millicores and ~79 MiB** across nine pods —
Postgres is the single largest line; the eight Rust components together are ~10m /
~25 MiB. Because the control plane runs no per-node component, an N-agent, M-node
fleet pays no per-node tax, and the agent pod itself is unchanged in weight
regardless of fleet size. These are point-in-time readings of one idle agent, not a
density sweep — the sweeps below quantify how the numbers move with scale.

## Density (agents per node)

Requested vs. scheduled on this single node (`agentd` at the default 100m CPU request):

| Requested | Running | Pending |
|---|---|---|
| 1 | 1 | 0 |
| 10 | 10 | 0 |
| 50 | 50 | 0 |
| 100 | **82** | 18 |

Ceiling on this host: **~82 `agentd` pods** — and the binding constraint is the
**kubelet's pods-per-node cap**, *not* CPU, memory, or anything in agentctl. The node
reports `capacity.pods: 110` (the Kubernetes default), and with ~28 control-plane and
system pods already resident (the agentctl components, KEDA, cert-manager,
metrics-server, …), ~82 agent slots remain (82 + 28 ≈ 110). On CPU this node had ~8×
headroom (82 × 100m ≈ 8.2 of 16 cores) and on memory ~6× (82 × 128 MiB ≈ 10.5 of
64 GiB). The cap is purely configurational — raise `--max-pods` (kubelet), lower the
agent's CPU/memory requests toward the measured ~1.3m / sub-MiB for idle fleets, or
add nodes, and density rises directly.

## Control-plane scaling trends

Operator reconcile latency and control-plane footprint as the fleet grows 1 → 100:

| Agents (N) | reconcile p50 | reconcile p95 | CP CPU (millicores) | CP mem (MiB) |
|---|---|---|---|---|
| 1 | 8.3 ms | 23.8 ms | 12 | 65 |
| 10 | 8.3 ms | 23.8 ms | 12 | 65 |
| 50 | 8.3 ms | 23.8 ms | 13 | 66 |
| 100 | 8.3 ms | 23.8 ms | 15 | 65 |

**Flat.** Reconcile latency and control-plane CPU/memory are essentially constant
from 1 to 100 agents — the operator and control plane do not degrade as the fleet
grows (on this host).

## Coordination throughput (the work-distribution serializing point)

The coordination server is the single atomic-claim serializing point. A concurrent
load generator drove `work.submit` / `work.claim` / `work.ack` at rising client
concurrency, in-memory store:

| Clients | Ops/sec | p50 | p99 | Ops | Errors |
|---|---|---|---|---|---|
| 1 | 320 | 2.8 ms | 7.4 ms | 2,557 | 0 |
| 4 | 1,274 | 3.2 ms | 4.4 ms | 10,194 | 0 |
| 16 | 3,354 | 4.7 ms | 7.8 ms | 26,846 | 0 |
| 64 | 4,585 | 12.4 ms | 36.2 ms | 36,716 | 0 |
| 256 | **5,137** | 47.3 ms | 94.9 ms | 41,292 | **0** |

**~5,100 work ops/sec at 256 concurrent clients with p99 < 100 ms and zero errors**
over 41k+ operations — the atomic single-grant invariant holds under contention at
load. A dedicated correctness run drove 72 concurrent claims over 12 items and
observed exactly 12 grants and **0 double-grants**, including across two
Postgres-backed replicas.

The same sweep against the **durable Postgres store** (the bundled single Postgres on
an untuned `emptyDir`):

| Clients | Ops/sec | p50 | p99 | Ops | Errors |
|---|---|---|---|---|---|
| 1 | 192 | 5.1 ms | 6.8 ms | 1,535 | 0 |
| 4 | **538** | 5.8 ms | 34.6 ms | 4,308 | 0 |
| 16 | 514 | 9.7 ms | 84.4 ms | 4,145 | 0 |
| 64 | 532 | 108 ms | 187 ms | 4,301 | 0 |
| 256 | 526 | 510 ms | 597 ms | 4,318 | 0 |

**In-memory vs. Postgres — the durability/HA trade.** Postgres tops out at **~530
ops/sec — roughly 10× lower than the in-memory store (~5,100)** — and saturates much
earlier (its knee is ~4 clients; beyond that you only add latency, p50 reaching
510 ms at 256 clients). Still **zero errors** at every level. This is the expected
cost of durability: each grant is a row-locked SQL `UPSERT` (a disk write plus fsync)
instead of an in-process mutex. In return you get a **durable, restart-safe claim
ledger that runs across multiple replicas** — the atomic grant-one invariant is
preserved by the conditional row lock (verified at 0 double-grants across two
replicas). Choose in-memory for raw single-replica throughput, Postgres when you need
durability and HA; scale Postgres throughput horizontally with replicas and a tuned,
provisioned database (the bundled `emptyDir` Postgres here is the floor, not a
production configuration).

## Functional coverage (real `agentd`)

The harness exercises every plane against the real agent. Scenarios cover
provisioning; the management path (drain / lame-duck / cancel through the aggregated
apiserver, plus an RBAC-denied 403); intelligence (an inference call through the
modelgateway to a mock provider, with token metering and a budget-exceeded 429);
claim-mode work distribution (atomic grant, dedupe, lease expiry, KEDA
scale-from-zero); shard-mode partitioning; A2A (Agent Card JWS verification and
`message/send` / `message/stream`); conformance (exit codes and metric-registry
membership); and the security gates (OIDC, trusted-proxy, attested identity,
coordination attestation, mTLS, the bearer-token gate, and NetworkPolicy enforcement
on a policy-capable CNI).

## Provisioning & scale-from-zero latency

Time to bring agents up:

| Phase | Measured |
|---|---|
| Provisioning 0→1 (apply Agent → pod Running) | **~2.2 s** |
| Provisioning 0→5 (apply Fleet → all 5 Running) | **~2.2 s** |

Provisioning is dominated by pod start — the `agentd` image is ~1.3 MB and cached, so
five agents come up as fast as one.

**Scale-from-zero** (KEDA: backlog → first worker) is functionally verified — a claim
fleet scales 0→N on backlog and back to 0, observed repeatedly, including under the
OIDC, attested-identity, and scaler-mTLS gates. The end-to-end latency decomposes as
the **scaler poll interval** (operator-configurable, typically 10–30 s) **plus KEDA
activation plus pod start (~2.2 s, measured)**; the poll interval dominates. For a
precise fresh timing, run with a real claimant draining the queue in a dedicated,
empty namespace (a shared cluster with residual unclaimed backlog never settles
cleanly to zero to be timed).

## Reproduce

```sh
# local kind (this report)
make -C e2e images up install e2e
make -C e2e bench report          # writes e2e/results/<ts>/*.csv + this file

# real multi-node cluster (true capacity numbers)
make -C e2e e2e bench report KUBECONFIG=<kubeconfig> SKIP_BRINGUP=1
```

Raw per-run CSVs (`density`, `overhead`, `cp_trends`, `throughput`, `host.json`) live
under `e2e/results/` (git-ignored).
