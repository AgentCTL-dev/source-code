# agentctl — scale & resource benchmarks

End-to-end measurements of the agentctl control plane driving the **real reference
agent `agentd` v1.0.0** as the data plane, produced by the Phase-4 harness
(`e2e/` + `crates/agentctl-e2e`). Every number here was **measured**, not modeled.

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
| Data-plane agent | `agentd` v1.0.0 (static musl, ~1.3 MB; 100m CPU / 128Mi requests) |
| Store | coordination `store=memory` (Postgres comparison: see "Not yet measured") |

## Per-agent overhead (the headline)

Marginal cost of one additional `agentd` agent, measured at steady state:

| Component | CPU (millicores) | Memory (MiB) |
|---|---|---|
| **agentd pod** (idle/reactive) | **~1.3** | **< 1** |
| node-agent (per *node*, constant — DaemonSet) | ~0 | ~3 |
| control-plane marginal (per agent) | ~0 | ~0 |

`agentd` is about the lightest possible conformant agent, so this is effectively
the **floor**: an idle agent costs ~1.3 millicores and sub-MiB working set, and the
control plane's marginal cost per agent is in the noise. Density is bound by the
node, not the agent.

## Density (agents per node)

Requested vs. scheduled on this single node (agentd at the default 100m CPU request):

| Requested | Running | Pending |
|---|---|---|
| 1 | 1 | 0 |
| 10 | 10 | 0 |
| 50 | 50 | 0 |
| 100 | **82** | 18 |

Ceiling on this host: **~82 agentd pods** — and the binding constraint is the
**kubelet's pods-per-node cap**, *not* CPU, memory, or anything in agentctl.
Confirmed: the node reports `capacity.pods: 110` (the Kubernetes default), and with
~28 control-plane + system pods already resident (operator, node-agent, gateway,
ModelGateway, coordination, scaler, Postgres, admission, apiserver, KEDA,
cert-manager, metrics-server, …), only ~82 agent slots remain (82 + 28 ≈ 110). On
CPU this node had ~8× headroom (82 × 100m ≈ 8.2 of 16 cores) and on memory ~6×
(82 × 128 MiB ≈ 10.5 of 64 GiB). The cap is purely configurational — raise
`--max-pods` (kubelet), lower the agent's CPU/memory requests toward the measured
~1.3m / sub-MiB for idle fleets, or add nodes, and density rises directly.

## Control-plane scaling trends

Operator reconcile latency and control-plane footprint as the fleet grows 1 → 100:

| Agents (N) | reconcile p50 | reconcile p95 | CP CPU (millicores) | CP mem (MiB) |
|---|---|---|---|---|
| 1 | 8.3 ms | 23.8 ms | 12 | 65 |
| 10 | 8.3 ms | 23.8 ms | 12 | 65 |
| 50 | 8.3 ms | 23.8 ms | 13 | 66 |
| 100 | 8.3 ms | 23.8 ms | 15 | 65 |

**Flat.** Reconcile latency and control-plane CPU/memory are essentially constant
from 1 to 100 agents — the operator + control plane do not degrade as the fleet
grows (on this host).

## Coordination throughput (the work-distribution serializing point)

The coordination server is the single atomic-claim serializing point. A concurrent
load generator drove `work.submit`/`work.claim`/`work.ack` at rising client
concurrency (memory store):

| Clients | Ops/sec | p50 | p99 | Ops | Errors |
|---|---|---|---|---|---|
| 1 | 320 | 2.8 ms | 7.4 ms | 2,557 | 0 |
| 4 | 1,274 | 3.2 ms | 4.4 ms | 10,194 | 0 |
| 16 | 3,354 | 4.7 ms | 7.8 ms | 26,846 | 0 |
| 64 | 4,585 | 12.4 ms | 36.2 ms | 36,716 | 0 |
| 256 | **5,137** | 47.3 ms | 94.9 ms | 41,292 | **0** |

**~5,100 work ops/sec at 256 concurrent clients with p99 < 100 ms and zero errors**
over 41k+ operations — the atomic single-grant invariant holds under contention at
load. (Earlier dedicated correctness runs: 72 concurrent claims over 12 items →
exactly 12 grants, **0 double-grants**, including across 2 Postgres-backed replicas.)

The same sweep against the **durable Postgres store** (`coordination.store=postgres`,
the bundled single Postgres, untuned `emptyDir`):

| Clients | Ops/sec | p50 | p99 | Ops | Errors |
|---|---|---|---|---|---|
| 1 | 192 | 5.1 ms | 6.8 ms | 1,535 | 0 |
| 4 | **538** | 5.8 ms | 34.6 ms | 4,308 | 0 |
| 16 | 514 | 9.7 ms | 84.4 ms | 4,145 | 0 |
| 64 | 532 | 108 ms | 187 ms | 4,301 | 0 |
| 256 | 526 | 510 ms | 597 ms | 4,318 | 0 |

**Memory vs. Postgres — the durability/HA trade.** Postgres tops out at **~530
ops/sec — roughly 10× lower than the in-memory store (~5,100)** — and saturates
much earlier (its knee is ~4 clients; beyond that you only add latency, p50 reaching
510 ms at 256 clients). Still **zero errors** across every level. This is the
expected cost of durability: each grant is a row-locked SQL `UPSERT` (a disk write +
fsync) instead of an in-process `Mutex`. In return you get a **durable, restart-safe
claim ledger that runs across multiple HA replicas** (the atomic grant-one invariant
is preserved by the conditional row lock — verified at 0 double-grants across 2
replicas). Choose memory for raw single-replica throughput, Postgres when you need
durability/HA; scale Postgres throughput horizontally with replicas + a tuned,
provisioned database (the bundled `emptyDir` Postgres here is the floor, not a
production config).

## Functional end-to-end (real agentd)

The harness drives every plane with the real agent — and that first contact with
`agentd` (vs. the `mock-agent` stand-in) **surfaced and fixed three real
control-plane interop bugs**, which is precisely the point of Phase 4:

- **ModelGateway** served only `/v1/infer`; an OpenAI-compatible agent dials
  `/v1/chat/completions` — added as an alias to the same identity/budget/credential
  path, so the routed-infer loop reaches the gateway.
- **Operator** ran the agent as PID 1 (scratch image) — agentd's worker orphan-guard
  (`getppid()==1 ⇒ bail`) misfired and aborted every run → `shareProcessNamespace: true`.
- **Operator** forced the agent off-root, but agentd (`USER 65532`) must bind its
  management socket into the kubelet's `root:root` hostPath dir → pinned
  `runAsUser: 0` (capabilities still fully dropped, no-privilege-escalation,
  read-only root filesystem; nonroot remains a documented follow-up gated on
  node-agent-chowned per-pod dirs, RFC 0002 §6.1).

Scenarios cover provisioning, the management path (drain/lame-duck/cancel via the
aggregated APIServer + a SAR-denied 403), intelligence (routed-infer → ModelGateway
→ mock provider, token metering + budget 429), claim-mode work distribution
(atomic grant, dedupe, lease expiry, KEDA scale-from-zero), shard-mode, A2A (card
JWS + message/send/stream), conformance (exit codes, metric-registry membership),
and the security gates (OIDC, trusted-proxy, attested identity, coordination
attest, mTLS, apiToken; NetworkPolicy on the Calico lane).

## Provisioning & scale-from-zero latency

Time to bring agents up:

| Phase | Measured |
|---|---|
| Provisioning 0→1 (apply Agent → pod Running) | **~2.2 s** |
| Provisioning 0→5 (apply Fleet → all 5 Running) | **~2.2 s** |

Provisioning is dominated by pod start — the agentd image is ~1.3 MB and cached, so
five agents come up as fast as one.

**Scale-from-zero (KEDA: backlog → first worker)** is **functionally verified** — a
claim fleet scales 0→N on backlog and back to 0, observed repeatedly (the
elastic-from-zero loop, and again under the OIDC / attested-identity / scaler-mTLS
gates). A precise *fresh timing* on this shared, stateful cluster is confounded:
residual unclaimed backlog from mock-agent fleets — which submit work but don't
claim it — keeps a worker correctly running, so the fleet never settles cleanly to 0
to be timed (the bench's own latency sweep hit the same wall). The end-to-end
latency decomposes as **scaler poll interval** (operator-configurable, typically
10–30 s) **+ KEDA activation + pod start (~2.2 s, measured)** — the poll interval
dominates. For a precise figure, re-run with a real claimer (`agentd` + the
`work-mcp-bridge`) draining the queue, in a dedicated empty namespace.

## Not yet measured (re-run on a real / drained cluster)

- **Scale-from-zero precise timing** — functionally verified (above); a clean timed
  number needs a drained queue / real claimer (mock agents don't drain backlog).
- **Density beyond the pods-per-node cap** — this node's 110-pod cap bound the
  ceiling, not resources; raise `--max-pods` and/or add nodes for true capacity.

## Reproduce

```sh
# local kind (this report)
make -C e2e images up install e2e
make -C e2e bench report          # writes e2e/results/<ts>/*.csv + this file

# real multi-node cluster (true capacity numbers)
make -C e2e e2e bench report KUBECONFIG=<kubeconfig> SKIP_BRINGUP=1
```
Raw per-run CSVs (`density`, `overhead`, `cp_trends`, `throughput`, `host.json`)
live under `e2e/results/` (git-ignored).
